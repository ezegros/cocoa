//! Conversation-only forks from bounded canonical original evidence.

use cocoa_core::{
    model::UserInput,
    store::{RecordId, RecordKind, StoreLimits, StoredRecord},
};
use sqlx::SqliteConnection;

use super::*;

/// A root accepted input, including captured images, identified by its original ID.
#[derive(Clone, Debug, PartialEq)]
pub struct RewindPoint {
    pub record: RecordId,
    pub input: UserInput,
}

pub(super) struct ForkPrefix {
    source: SessionAttachment,
    _payload: OwnedSemaphorePermit,
    _operation: OwnedMutexGuard<AttachmentState>,
    before: RecordId,
    records: Vec<StoredRecord>,
    settings: StoredSessionSettings,
}

impl SessionAttachment {
    /// List root accepted inputs in canonical acceptance order, including archives.
    /// At most 65,536 records and 64 MiB of decoded evidence are inspected.
    ///
    /// # Errors
    /// Reject invalid limits, corrupt/oversized evidence or unavailable authority.
    pub async fn rewind_points(
        &self,
        limits: StoreLimits,
    ) -> Result<Vec<RewindPoint>, PersistenceError> {
        let _payload = self.inner.database.payload(PREFIX_BYTES_MAX * 3)?;
        Ok(self
            .fork_records(None, limits)
            .await?
            .into_iter()
            .filter_map(|stored| match stored.record.kind {
                RecordKind::InputAccepted { input } => Some(RewindPoint {
                    record: stored.id,
                    input,
                }),
                _ => None,
            })
            .collect())
    }

    /// Atomically create an independent session before a root accepted input.
    /// Retain the working directory and current configuration, not workspace state.
    /// Copy only canonical root originals with fresh identities, including shared
    /// snapshots already delivered. Live shared records, children, checkpoints
    /// and instruction delivery are intentionally absent. The session context
    /// assembler delivers an empty snapshot to distinguish the reset live state
    /// from retained historical snapshots.
    /// Dropping this future after creation is admitted does not cancel the commit.
    ///
    /// # Errors
    /// Reject foreign/non-input cuts, oversized or malformed prefixes, unavailable
    /// authority, or normal session creation errors. No source rows are modified.
    pub async fn fork_before(
        &self,
        record: RecordId,
        limits: StoreLimits,
    ) -> Result<SessionAttachment, PersistenceError> {
        let _payload = self.inner.database.payload(PREFIX_BYTES_MAX * 3)?;
        let records = self.fork_records(Some(record), limits).await?;
        let directory = self.working_directory().await?;
        let operation = tokio::time::timeout(WAIT, Arc::clone(&self.inner.operation).lock_owned())
            .await
            .map_err(|_| PersistenceError::Busy)?;
        if matches!(*operation, AttachmentState::Uncertain { .. }) {
            return Err(PersistenceError::OutcomeUnknown);
        }
        let prefix = ForkPrefix {
            source: self.clone(),
            _payload,
            _operation: operation,
            before: record,
            records,
            settings: self.settings().await?,
        };
        self.database()
            .create_with_fork(&directory, Some(prefix))
            .await
    }

    async fn fork_records(
        &self,
        before: Option<RecordId>,
        limits: StoreLimits,
    ) -> Result<Vec<StoredRecord>, PersistenceError> {
        let store = self.root_store(limits)?;
        let page_bytes = limits
            .record_bytes
            .saturating_add(4096)
            .clamp(4 * 1024 * 1024, PREFIX_BYTES_MAX);
        let through = if let Some(before) = before {
            let cut = store.read_record(before, page_bytes as u64).await?.ok_or(
                PersistenceError::InvalidData("fork cut absent from root history"),
            )?;
            if !matches!(cut.record.kind, RecordKind::InputAccepted { .. }) {
                return Err(PersistenceError::InvalidData("fork cut is not an input"));
            }
            Some(cut.sequence)
        } else {
            None
        };
        let mut cursor = None;
        let mut records = Vec::new();
        let mut bytes = 0_usize;
        for _ in 0..=PREFIX_RECORDS_MAX {
            let page = store
                .read_history_through(
                    cursor,
                    HistoryReadLimits::new(64, page_bytes as u64)?,
                    through,
                )
                .await?;
            for stored in page.records {
                if Some(stored.id) == before {
                    if !matches!(stored.record.kind, RecordKind::InputAccepted { .. }) {
                        return Err(PersistenceError::InvalidData("fork cut is not an input"));
                    }
                    return validate_prefix(records);
                }
                bytes = bytes
                    .checked_add(encoded_size(&stored.record)?)
                    .ok_or(PersistenceError::InvalidData("fork prefix bytes"))?;
                if records.len() >= PREFIX_RECORDS_MAX || bytes > PREFIX_BYTES_MAX {
                    return Err(PersistenceError::InvalidData("fork prefix limit"));
                }
                records.push(stored);
            }
            cursor = page.next_cursor;
            if cursor.is_none() {
                if before.is_some() {
                    return Err(PersistenceError::InvalidData(
                        "fork cut absent from root history",
                    ));
                }
                return validate_prefix(records);
            }
        }
        Err(PersistenceError::InvalidData("fork prefix limit"))
    }
}

const PREFIX_RECORDS_MAX: usize = 65_536;
const PREFIX_BYTES_MAX: usize = 64 * 1024 * 1024;

fn validate_prefix(records: Vec<StoredRecord>) -> Result<Vec<StoredRecord>, PersistenceError> {
    let snapshot = StoreSnapshot {
        records,
        instruction_state: None,
        checkpoints: Vec::new(),
    };
    cocoa_core::engine::validate_recovery_snapshot(&snapshot)?;
    Ok(snapshot.records)
}

pub(super) async fn insert(
    connection: &mut SqliteConnection,
    session: SessionId,
    root: AgentId,
    prefix: &ForkPrefix,
) -> Result<(), PersistenceError> {
    sessions::check_epoch(connection, &prefix.source).await?;
    let maximum: i64 = sqlx::query_scalar(
        "SELECT max(coalesce((SELECT max(id) FROM records), 0), coalesce((SELECT max(id) FROM checkpoints), 0))",
    ).fetch_one(&mut *connection).await?;
    let mut remap = BTreeMap::new();
    for (index, stored) in prefix.records.iter().enumerate() {
        let id = maximum
            .checked_add(index as i64 + 1)
            .ok_or(StoreError::IdentityExhausted)?;
        remap.insert(
            stored.id,
            RecordId::try_from(id).map_err(|_| StoreError::IdentityExhausted)?,
        );
    }
    let mut records = Vec::with_capacity(prefix.records.len());
    for stored in &prefix.records {
        let mut record = stored.record.clone();
        if let RecordKind::CellStarted { model, .. } | RecordKind::CellCompleted { model, .. } =
            &mut record.kind
        {
            *model = *remap
                .get(model)
                .ok_or(PersistenceError::InvalidData("fork model reference"))?;
        }
        let id = remap
            .get(&stored.id)
            .ok_or(PersistenceError::InvalidData("fork record identity"))?;
        records.push(StoredRecord {
            id: *id,
            sequence: stored.sequence,
            record,
        });
    }
    let records = validate_prefix(records)?;
    for stored in records {
        sqlx::query("INSERT INTO records (id, session_id, agent_id, sequence, invocation, runtime_epoch, created_at_ms, payload_version, payload_json) VALUES (?, ?, ?, ?, ?, 1, 0, ?, ?)")
            .bind(i64::from(stored.id)).bind(i64::from(session)).bind(i64::from(root))
            .bind(i64::from(stored.sequence)).bind(stored.record.invocation.0 as i64).bind(RECORD_VERSION)
            .bind(serde_json::to_string(&stored.record.kind).map_err(|_| StoreError::Encoding)?)
            .execute(&mut *connection).await?;
    }
    sqlx::query("UPDATE sessions SET settings_json = ? WHERE id = ?")
        .bind(serde_json::to_string(&prefix.settings).map_err(|_| StoreError::Encoding)?)
        .bind(i64::from(session))
        .execute(&mut *connection)
        .await?;
    sqlx::query("INSERT INTO conversation_forks (session_id, source_session_id, before_record_id) VALUES (?, ?, ?)")
        .bind(i64::from(session)).bind(i64::from(prefix.source.id())).bind(i64::from(prefix.before))
        .execute(connection).await?;
    Ok(())
}
