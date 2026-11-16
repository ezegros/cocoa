//! Snapshot reads with metadata/byte preflight and batched payload decoding.
//! ID batches disable secondary indexes to avoid whole-agent sequence scans;
//! SQLite can still seek the integer primary key and sort at most 64 matches.

use std::{collections::BTreeMap, path::Path};

use cocoa_core::{
    context::InstructionState,
    store::{
        AgentId, InvocationId, Record, RecordId, RecordKind, RecordSequence, StoreError,
        StoreLimits, StoreSnapshot, StoredRecord, encoded_size,
    },
};
use sqlx::{Connection, FromRow, QueryBuilder, Sqlite, SqliteConnection};

use super::{
    DIRECTORY_BYTES, FORMAT_VERSION, PersistenceError, RECORD_ENVELOPE_BYTES, RECORD_VERSION,
    SESSION_RESPONSE_BYTES, SessionCursor, SessionId, SessionPage, SessionPublicId, SessionSummary,
    SqliteStore, WAIT, archives, session_directory,
};

pub(super) async fn sessions(
    connection: &mut SqliteConnection,
    directory: Option<&str>,
    limit: u32,
    before: Option<SessionCursor>,
) -> Result<Vec<SessionRow>, PersistenceError> {
    let mut query = session_query();
    if let Some(directory) = directory {
        query
            .push(" WHERE working_directory = ")
            .push_bind(directory);
    }
    if let Some(cursor) = before {
        query
            .push(if directory.is_some() {
                " AND "
            } else {
                " WHERE "
            })
            .push("(updated_at_ms, id) < (")
            .push_bind(cursor.updated_at_ms())
            .push(", ")
            .push_bind(i64::from(cursor.id()))
            .push(")");
    }
    query
        .push(" ORDER BY updated_at_ms DESC, id DESC LIMIT ")
        .push_bind(i64::from(limit) + 1);
    Ok(query.build_query_as().fetch_all(connection).await?)
}

pub(super) async fn resolve_session(
    connection: &mut SqliteConnection,
    reference: &str,
) -> Result<Vec<SessionRow>, PersistenceError> {
    let mut query = session_query();
    if reference.len() == 37 {
        query.push(" WHERE public_id = ").push_bind(reference);
    } else {
        query
            .push(" WHERE public_id GLOB ")
            .push_bind(format!("{reference}*"));
    }
    query.push(" ORDER BY sessions.public_id LIMIT 2");
    Ok(query.build_query_as().fetch_all(connection).await?)
}

pub(super) fn session_page(
    rows: Vec<SessionRow>,
    limit: u32,
) -> Result<SessionPage, PersistenceError> {
    let count = rows.len();
    let mut page = SessionPage {
        sessions: Vec::with_capacity(count.min(limit as usize)),
        next_cursor: None,
    };
    // Cursor storage must fit even when the byte budget ends the page early.
    let mut bytes = encoded_size(&page)? - b"null".len();
    for row in rows {
        if page.sessions.len() == limit as usize {
            break;
        }
        let session = SessionSummary::try_from(row)?;
        let cursor = SessionCursor::new(session.updated_at_ms, session.id)?;
        let added = encoded_size(&session)? + usize::from(!page.sessions.is_empty());
        if bytes + added + encoded_size(&cursor)? > SESSION_RESPONSE_BYTES {
            break;
        }
        bytes += added;
        page.sessions.push(session);
        page.next_cursor = Some(cursor);
    }
    if page.sessions.len() == count {
        page.next_cursor = None;
    } else if page.sessions.is_empty() {
        return Err(PersistenceError::InvalidData(
            "session metadata response budget",
        ));
    }
    Ok(page)
}

pub(super) fn resolved_session(rows: Vec<SessionRow>) -> Result<SessionSummary, PersistenceError> {
    let mut rows = rows.into_iter();
    let first = SessionSummary::try_from(rows.next().ok_or(PersistenceError::SessionNotFound)?)?;
    if let Some(second) = rows.next() {
        let second = SessionSummary::try_from(second)?;
        return Err(PersistenceError::AmbiguousSessionId {
            first: first.public_id,
            second: second.public_id,
        });
    }
    Ok(first)
}

pub(super) async fn binding(
    connection: &mut SqliteConnection,
    agent: AgentId,
) -> Result<Binding, PersistenceError> {
    let binding = sqlx::query_as::<_, Binding>(
        "SELECT a.session_id, s.public_id, s.runtime_epoch AS epoch, a.format_version, \
         (SELECT id FROM agents root WHERE root.session_id = a.session_id AND root.parent_agent_id IS NULL) AS root_agent \
         FROM agents a JOIN sessions s ON s.id = a.session_id WHERE a.id = ? AND a.lifecycle = 'participating'",
    ).bind(i64::from(agent)).fetch_optional(connection).await?.ok_or(PersistenceError::AgentUnavailable)?;
    if binding.session_id <= 0
        || binding.epoch <= 0
        || binding.root_agent.is_none_or(|id| id <= 0)
        || binding.public_id.parse::<SessionPublicId>().is_err()
    {
        return Err(PersistenceError::InvalidData(
            "session binding/root identity",
        ));
    }
    if binding.format_version != FORMAT_VERSION {
        return Err(PersistenceError::UnsupportedPayload(binding.format_version));
    }
    Ok(binding)
}

pub(super) async fn authority(
    connection: &mut SqliteConnection,
    store: &SqliteStore,
) -> Result<(), PersistenceError> {
    let admitted: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM agents a JOIN sessions s ON s.id = a.session_id \
         WHERE a.id = ? AND a.session_id = ? AND a.lifecycle = 'participating' \
         AND a.format_version = ? AND s.runtime_epoch = ?",
    )
    .bind(i64::from(store.agent))
    .bind(i64::from(store.attachment.session_id))
    .bind(FORMAT_VERSION)
    .bind(store.attachment.epoch)
    .fetch_one(connection)
    .await?;
    if admitted != 1 {
        return Err(PersistenceError::AgentUnavailable);
    }
    Ok(())
}

pub(super) async fn stats(
    connection: &mut SqliteConnection,
    store: &SqliteStore,
) -> Result<Stats, PersistenceError> {
    let stats = sqlx::query_as::<_, Stats>(
        "SELECT count(*) AS count, \
         coalesce(max(record_bytes), 0) AS record_max, coalesce(max(sequence), 0) AS sequence \
         FROM (SELECT sequence, coalesce(octet_length(payload_json), archive_decoded_bytes) + length(CAST(invocation AS TEXT)) + ? AS record_bytes \
         FROM records WHERE session_id = ? AND agent_id = ?)",
    ).bind(RECORD_ENVELOPE_BYTES as i64).bind(i64::from(store.attachment.session_id)).bind(i64::from(store.agent))
        .fetch_one(connection).await?;
    stats.validate(store)
}

pub(super) async fn append_stats(
    connection: &mut SqliteConnection,
    store: &SqliteStore,
) -> Result<Stats, PersistenceError> {
    // Exclusive attachment ownership keeps an admitted prefix immutable. Recheck
    // its last record and only the newer suffix; full loads still audit every row.
    let mut prefix = store.append_prefix.lock().await;
    let after = prefix
        .as_ref()
        .map_or(0, |prefix| prefix.sequence.saturating_sub(1).max(0));
    let mut stats = sqlx::query_as::<_, Stats>(
        "SELECT count(*) AS count, coalesce(max(record_bytes),0) AS record_max, coalesce(max(sequence),0) AS sequence \
         FROM (SELECT sequence, coalesce(octet_length(payload_json), archive_decoded_bytes) + length(CAST(invocation AS TEXT)) + ? AS record_bytes \
         FROM records WHERE session_id = ? AND agent_id = ? AND sequence > ?)",
    ).bind(RECORD_ENVELOPE_BYTES as i64).bind(i64::from(store.attachment.session_id)).bind(i64::from(store.agent)).bind(after)
        .fetch_one(connection).await?;
    stats.count = stats.count.checked_add(after).ok_or(StoreError::Capacity)?;
    if let Some(prefix) = prefix.as_ref() {
        stats.record_max = stats.record_max.max(prefix.record_max);
    }
    let stats = stats.validate(store)?;
    *prefix = Some(stats);
    Ok(stats)
}

pub(super) async fn writable(
    connection: &mut SqliteConnection,
    store: &SqliteStore,
) -> Result<(), PersistenceError> {
    authority(connection, store).await?;
    let active: bool =
        sqlx::query_scalar("SELECT outcome_json IS NULL FROM agents WHERE id=? AND session_id=?")
            .bind(i64::from(store.agent))
            .bind(i64::from(store.attachment.session_id))
            .fetch_one(connection)
            .await?;
    if !active {
        return Err(PersistenceError::AgentUnavailable);
    }
    Ok(())
}

pub(super) async fn state(
    connection: &mut SqliteConnection,
    store: &SqliteStore,
) -> Result<Option<InstructionState>, PersistenceError> {
    let row = sqlx::query_as::<_, StateRow>(
        "SELECT format_version, octet_length(state_json) AS bytes, \
         CASE WHEN format_version = ? AND octet_length(state_json) <= ? THEN state_json END AS state_json \
         FROM instruction_state WHERE agent_id = ?",
    ).bind(FORMAT_VERSION).bind(16 * 1024 * 1024_i64).bind(i64::from(store.agent)).fetch_optional(connection).await?;
    let Some(row) = row else {
        return Ok(None);
    };
    if row.format_version != FORMAT_VERSION {
        return Err(PersistenceError::UnsupportedPayload(row.format_version));
    }
    let text = row.state_json.ok_or(StoreError::Capacity)?;
    tokio::task::spawn_blocking(move || {
        let state: InstructionState = serde_json::from_str(&text)
            .map_err(|_| PersistenceError::InvalidData("instruction state JSON"))?;
        state.validate_bounds(usize::MAX)?;
        if state.invocation.0 == 0
            || i64::try_from(state.invocation.0).is_err()
            || encoded_size(&state)? as i64 != row.bytes
        {
            return Err(PersistenceError::InvalidData(
                "instruction state identity/encoding",
            ));
        }
        Ok(Some(state))
    })
    .await?
}

pub(super) async fn feedback_targets(
    connection: &mut SqliteConnection,
    store: &SqliteStore,
    targets: &[i64],
) -> Result<BTreeMap<RecordId, InvocationId>, PersistenceError> {
    let mut completed = BTreeMap::new();
    for targets in targets.chunks(PAGE_RECORDS) {
        let mut query = QueryBuilder::<Sqlite>::new(
            "SELECT id, sequence, invocation, runtime_epoch, payload_version, coalesce(octet_length(payload_json), archive_decoded_bytes) AS payload_bytes \
             FROM records NOT INDEXED WHERE session_id = ",
        );
        query
            .push_bind(i64::from(store.attachment.session_id))
            .push(" AND agent_id = ")
            .push_bind(i64::from(store.agent))
            .push(" AND id IN (");
        {
            let mut ids = query.separated(", ");
            for id in targets {
                ids.push_bind(*id);
            }
        }
        query.push(") ORDER BY sequence");
        let metadata = query
            .build_query_as::<RecordMetadata>()
            .fetch_all(&mut *connection)
            .await?;
        let bytes = metadata.iter().try_fold(0_usize, |bytes, record| {
            bytes
                .checked_add(metadata_bytes(record, store)?)
                .ok_or(PersistenceError::Store(StoreError::Capacity))
        })?;
        let _payload = store.attachment.database.payload(bytes)?;
        let records = payload_pages(connection, store, metadata).await?;
        completed.extend(completed_cells(&records));
    }
    Ok(completed)
}

/// Whether an agent retained anything that depends on the provider which
/// produced it. Accepted input and cancelled turns retain no model output, so
/// such a history still permits another model.
pub(super) async fn holds_model_output(
    connection: &mut SqliteConnection,
    session: SessionId,
    agent: AgentId,
) -> Result<bool, PersistenceError> {
    let retained: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM records WHERE session_id = ? AND agent_id = ? \
         AND (archive_kind NOT IN ('InputAccepted', 'TurnCancelled', 'InvocationFinished') \
         OR (payload_json IS NOT NULL AND json_type(payload_json,'$.InputAccepted') IS NULL \
         AND json_type(payload_json,'$.TurnCancelled') IS NULL \
         AND json_type(payload_json,'$.InvocationFinished') IS NULL)) LIMIT 1",
    )
    .bind(i64::from(session))
    .bind(i64::from(agent))
    .fetch_optional(&mut *connection)
    .await?;
    Ok(retained.is_some())
}

pub(super) async fn load(store: &SqliteStore) -> Result<StoreSnapshot, PersistenceError> {
    *store.append_prefix.lock().await = None;
    let mut reader = tokio::time::timeout(WAIT, store.attachment.database.reader.lock())
        .await
        .map_err(|_| PersistenceError::Busy)?;
    reader.settle().await?;
    reader.attachment = Some(std::sync::Arc::clone(&store.attachment.lease));
    let mut transaction = reader.connection()?.begin().await?;
    let result = async {
        authority(&mut transaction, store).await?;
        let stats = stats(&mut transaction, store).await?;
        let instruction_state = state(&mut transaction, store).await?;
        let records = record_pages(&mut transaction, store).await?;
        if records.len() as i64 != stats.count {
            return Err(PersistenceError::InvalidData("snapshot history count"));
        }
        if let Some(state) = &instruction_state {
            if !records
                .iter()
                .any(|record| record.record.invocation == state.invocation)
            {
                return Err(PersistenceError::InvalidData(
                    "instruction invocation absent from history",
                ));
            }
            state.validate_feedback_targets(&completed_cells(&records))?;
        }
        let mut snapshot = StoreSnapshot {
            records,
            instruction_state,
            checkpoints: Vec::new(),
        };
        super::compaction::restore(&mut transaction, store, &mut snapshot).await?;
        Ok((snapshot, stats))
    }
    .await;
    match result {
        Ok((snapshot, stats)) => {
            transaction.commit().await?;
            reader.attachment = None;
            *store.append_prefix.lock().await = Some(stats);
            Ok(snapshot)
        }
        Err(error) => {
            transaction.rollback().await?;
            reader.attachment = None;
            Err(error)
        }
    }
}

const PAGE_RECORDS: usize = 64;

#[derive(FromRow)]
pub(super) struct SessionRow {
    id: i64,
    public_id: Option<String>,
    working_directory: Option<String>,
    title: Option<String>,
    title_bytes: Option<i64>,
    created_at_ms: i64,
    updated_at_ms: i64,
    last_opened_at_ms: i64,
}

impl TryFrom<SessionRow> for SessionSummary {
    type Error = PersistenceError;

    fn try_from(row: SessionRow) -> Result<Self, Self::Error> {
        let id =
            SessionId::try_from(row.id).map_err(|_| PersistenceError::InvalidData("session ID"))?;
        let public_id = row
            .public_id
            .ok_or(PersistenceError::InvalidData("session public ID size"))?
            .parse()
            .map_err(|_| PersistenceError::InvalidData("session public ID"))?;
        let directory = row.working_directory.ok_or(PersistenceError::InvalidData(
            "session working directory size",
        ))?;
        session_directory(Path::new(&directory))
            .map_err(|_| PersistenceError::InvalidData("session working directory"))?;
        if row.title_bytes.is_some() != row.title.is_some()
            || row.created_at_ms < 0
            || row.updated_at_ms < 0
            || row.last_opened_at_ms < 0
        {
            return Err(PersistenceError::InvalidData("session metadata"));
        }
        Ok(Self {
            id,
            public_id,
            working_directory: directory.into(),
            title: row.title,
            created_at_ms: row.created_at_ms,
            updated_at_ms: row.updated_at_ms,
            last_opened_at_ms: row.last_opened_at_ms,
        })
    }
}

#[derive(FromRow)]
pub(super) struct Binding {
    pub(super) session_id: i64,
    pub(super) public_id: String,
    pub(super) epoch: i64,
    pub(super) root_agent: Option<i64>,
    format_version: i64,
}

#[derive(Clone, Copy, FromRow)]
pub(super) struct Stats {
    pub(super) count: i64,
    pub(super) sequence: i64,
    record_max: i64,
}

impl Stats {
    fn validate(self, store: &SqliteStore) -> Result<Self, PersistenceError> {
        if self.count < 0 || self.record_max < 0 {
            return Err(PersistenceError::InvalidData("negative history accounting"));
        }
        if self.record_max > store.limits.record_bytes as i64 {
            return Err(StoreError::RecordLimit.into());
        }
        if self.sequence != self.count {
            return Err(PersistenceError::InvalidData("history sequence gap"));
        }
        Ok(self)
    }
}

#[derive(FromRow)]
struct StateRow {
    format_version: i64,
    bytes: i64,
    state_json: Option<String>,
}

#[derive(FromRow)]
pub(super) struct RecordMetadata {
    pub(super) id: i64,
    pub(super) sequence: i64,
    pub(super) invocation: i64,
    pub(super) runtime_epoch: i64,
    pub(super) payload_version: i64,
    pub(super) payload_bytes: i64,
}

fn session_query() -> QueryBuilder<Sqlite> {
    let mut query = QueryBuilder::new(
        "SELECT id, CASE WHEN octet_length(public_id) = 37 THEN public_id END AS public_id, \
         CASE WHEN octet_length(working_directory) BETWEEN 1 AND ",
    );
    query.push_bind(DIRECTORY_BYTES as i64).push(
        " THEN working_directory END AS working_directory, \
         CASE WHEN octet_length(title) BETWEEN 1 AND 512 THEN title END AS title, \
         octet_length(title) AS title_bytes, created_at_ms, updated_at_ms, last_opened_at_ms \
         FROM sessions",
    );
    query
}

async fn record_pages(
    connection: &mut SqliteConnection,
    store: &SqliteStore,
) -> Result<Vec<StoredRecord>, PersistenceError> {
    let mut records = Vec::new();
    let mut cursor = 0;
    loop {
        let metadata = sqlx::query_as::<_, RecordMetadata>(
            "SELECT id, sequence, invocation, runtime_epoch, payload_version, coalesce(octet_length(payload_json), archive_decoded_bytes) AS payload_bytes \
             FROM records WHERE session_id = ? AND agent_id = ? AND sequence > ? ORDER BY sequence LIMIT ?",
        ).bind(i64::from(store.attachment.session_id)).bind(i64::from(store.agent)).bind(cursor).bind(PAGE_RECORDS as i64)
            .fetch_all(&mut *connection).await?;
        if metadata.is_empty() {
            break;
        }
        cursor = metadata
            .last()
            .ok_or(PersistenceError::InvalidData("empty metadata page"))?
            .sequence;
        records.extend(payload_pages(connection, store, metadata).await?);
    }
    Ok(records)
}

pub(super) async fn payload_pages(
    connection: &mut SqliteConnection,
    store: &SqliteStore,
    metadata: Vec<RecordMetadata>,
) -> Result<Vec<StoredRecord>, PersistenceError> {
    const PAGE_BYTES: usize = 4 * 1024 * 1024;

    let mut records = Vec::with_capacity(metadata.len());
    let mut metadata = metadata.into_iter().peekable();
    while metadata.peek().is_some() {
        let mut selected = Vec::with_capacity(PAGE_RECORDS);
        let mut page_bytes = 0_usize;
        while let Some(next) = metadata.peek() {
            let record_bytes = metadata_bytes(next, store)?;
            if !selected.is_empty() && page_bytes.saturating_add(record_bytes) > PAGE_BYTES {
                break;
            }
            page_bytes = page_bytes
                .checked_add(record_bytes)
                .ok_or(StoreError::Capacity)?;
            if let Some(next) = metadata.next() {
                selected.push(next);
            }
        }
        let mut query = QueryBuilder::<Sqlite>::new("SELECT ");
        query
            .push(archives::PAYLOAD_COLUMNS)
            .push(" FROM records NOT INDEXED WHERE session_id = ")
            .push_bind(i64::from(store.attachment.session_id))
            .push(" AND agent_id = ")
            .push_bind(i64::from(store.agent))
            .push(" AND id IN (");
        {
            let mut ids = query.separated(", ");
            for record in &selected {
                ids.push_bind(record.id);
            }
        }
        query.push(") ORDER BY sequence");
        let rows = query
            .build_query_as::<archives::PayloadRow>()
            .fetch_all(&mut *connection)
            .await?;
        let limits = store.limits;
        let decoded =
            tokio::task::spawn_blocking(move || decode_page(rows, selected, limits)).await??;
        records.extend(decoded);
    }
    Ok(records)
}

pub(super) fn metadata_bytes(
    metadata: &RecordMetadata,
    store: &SqliteStore,
) -> Result<usize, PersistenceError> {
    if metadata.payload_version != RECORD_VERSION {
        return Err(PersistenceError::UnsupportedPayload(
            metadata.payload_version,
        ));
    }
    if metadata.id <= 0
        || metadata.sequence <= 0
        || metadata.invocation <= 0
        || metadata.runtime_epoch <= 0
        || metadata.runtime_epoch > store.attachment.epoch
        || metadata.payload_bytes <= 0
    {
        return Err(PersistenceError::InvalidData("record envelope"));
    }
    let bytes = usize::try_from(metadata.payload_bytes)
        .map_err(|_| StoreError::Capacity)?
        .checked_add(metadata.invocation.to_string().len())
        .and_then(|bytes| bytes.checked_add(RECORD_ENVELOPE_BYTES))
        .ok_or(StoreError::Capacity)?;
    if bytes > store.limits.record_bytes {
        return Err(StoreError::RecordLimit.into());
    }
    Ok(bytes)
}

fn decode_page(
    rows: Vec<archives::PayloadRow>,
    metadata: Vec<RecordMetadata>,
    limits: StoreLimits,
) -> Result<Vec<StoredRecord>, PersistenceError> {
    if rows.len() != metadata.len() {
        return Err(PersistenceError::InvalidData("record page changed"));
    }
    let mut records = Vec::with_capacity(rows.len());
    for (row, metadata) in rows.into_iter().zip(metadata) {
        if row.id != metadata.id {
            return Err(PersistenceError::InvalidData("record page identity/size"));
        }
        let kind = row.decode(metadata.payload_bytes)?;
        let record = Record {
            invocation: InvocationId(metadata.invocation as u64),
            kind,
        };
        record.validate()?;
        let bytes = encoded_size(&record)?;
        let expected = metadata.payload_bytes as usize
            + metadata.invocation.to_string().len()
            + RECORD_ENVELOPE_BYTES;
        if bytes != expected || bytes > limits.record_bytes {
            return Err(PersistenceError::InvalidData(
                "noncanonical record encoding",
            ));
        }
        records.push(StoredRecord {
            id: RecordId::try_from(metadata.id)
                .map_err(|_| PersistenceError::InvalidData("record identity"))?,
            sequence: RecordSequence::try_from(metadata.sequence)
                .map_err(|_| PersistenceError::InvalidData("record sequence"))?,
            record,
        });
    }
    Ok(records)
}

fn completed_cells(records: &[StoredRecord]) -> BTreeMap<RecordId, InvocationId> {
    records
        .iter()
        .filter(|stored| matches!(stored.record.kind, RecordKind::CellCompleted { .. }))
        .map(|stored| (stored.id, stored.record.invocation))
        .collect()
}
