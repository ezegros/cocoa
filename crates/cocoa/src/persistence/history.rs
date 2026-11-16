//! Bounded original-evidence reads, independent of complete snapshot restoration.

use std::{fmt, str::FromStr, sync::Arc};

use cocoa_core::store::{
    AgentId, RecordId, RecordSequence, StoreError, StoredRecord, encoded_size,
};
use serde::{Serialize, Serializer};
use sqlx::{Connection, SqliteConnection};

use super::{
    AttachmentState, PENDING_BYTES, PersistenceError, SessionPublicId, SqliteStore, WAIT, reads,
};

/// Per-read bounds on returned records and compact JSON bytes, including paging
/// metadata. These limits do not cap the total retained history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HistoryReadLimits {
    records: u32,
    bytes: usize,
}

impl HistoryReadLimits {
    /// Admit at most 64 records and at most the database's 256 MiB payload budget.
    /// A record that cannot fit is reported explicitly, never skipped.
    ///
    /// # Errors
    /// Reject zero limits, more than 64 records or an oversized byte allowance.
    pub fn new(records: u32, bytes: u64) -> Result<Self, PersistenceError> {
        let bytes = usize::try_from(bytes)
            .ok()
            .filter(|bytes| *bytes > 0 && *bytes <= PENDING_BYTES)
            .ok_or(PersistenceError::InvalidHistoryLimits)?;
        if !(1..=64).contains(&records) {
            return Err(PersistenceError::InvalidHistoryLimits);
        }
        Ok(Self { records, bytes })
    }
}

/// Original records in sequence order at a pinned upper boundary. Appends after
/// that boundary do not enter later pages. No live read transaction spans pages.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct HistoryPage {
    pub records: Vec<StoredRecord>,
    pub through: Option<RecordSequence>,
    pub next_cursor: Option<HistoryCursor>,
}

/// An own-agent position and upper sequence, not permission to read another agent.
/// The string encoding preserves complete integer identities across JSON clients.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryCursor {
    session: SessionPublicId,
    agent: AgentId,
    after: RecordSequence,
    through: RecordSequence,
}

impl FromStr for HistoryCursor {
    type Err = PersistenceError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() > 120 {
            return Err(PersistenceError::InvalidHistoryCursor);
        }
        let mut parts = value.split(':');
        if parts.next() != Some("history-v1") {
            return Err(PersistenceError::InvalidHistoryCursor);
        }
        let session = parts
            .next()
            .and_then(|session| session.parse().ok())
            .ok_or(PersistenceError::InvalidHistoryCursor)?;
        let mut integer = || {
            parts
                .next()
                .filter(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
                .and_then(|part| part.parse::<i64>().ok())
                .ok_or(PersistenceError::InvalidHistoryCursor)
        };
        let agent =
            AgentId::try_from(integer()?).map_err(|_| PersistenceError::InvalidHistoryCursor)?;
        let after = RecordSequence::try_from(integer()?)
            .map_err(|_| PersistenceError::InvalidHistoryCursor)?;
        let through = RecordSequence::try_from(integer()?)
            .map_err(|_| PersistenceError::InvalidHistoryCursor)?;
        if parts.next().is_some() || after >= through {
            return Err(PersistenceError::InvalidHistoryCursor);
        }
        let cursor = Self {
            session,
            agent,
            after,
            through,
        };
        if cursor.to_string() != value {
            return Err(PersistenceError::InvalidHistoryCursor);
        }
        Ok(cursor)
    }
}

impl fmt::Display for HistoryCursor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "history-v1:{}:{}:{}:{}",
            self.session,
            i64::from(self.agent),
            i64::from(self.after),
            i64::from(self.through),
        )
    }
}

impl Serialize for HistoryCursor {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl SqliteStore {
    /// Retrieve one unchanged same-agent record by immutable identity. Foreign
    /// identities are indistinguishable from absent records. The byte allowance
    /// bounds compact JSON for the returned record, or `null` when absent.
    /// Reads do not clear an uncertain write outcome or execute historical work.
    ///
    /// # Errors
    /// Reject invalid bounds, oversized or corrupt requested evidence, stale
    /// authority, uncertain writes, exhausted admission and database failures.
    pub async fn read_record(
        &self,
        record: RecordId,
        bytes: u64,
    ) -> Result<Option<StoredRecord>, PersistenceError> {
        let limits = HistoryReadLimits::new(1, bytes)?;
        let mut page = self
            .history_read(Selection::Exact(record), limits, None)
            .await?;
        Ok(page.records.pop())
    }

    /// Browse original records in source order without loading the full history.
    /// The first page pins the latest sequence. A cursor is bound to this session
    /// and agent; current authority is checked on every page. If bytes end a page
    /// early, the cursor follows its last returned record, not the last inspected
    /// metadata row. An oversized first record fails rather than hiding evidence.
    /// Reads neither change activity nor acknowledge an uncertain mutation.
    ///
    /// # Errors
    /// Reject foreign/invalid cursors, insufficient response budgets, corrupt or
    /// oversized requested evidence, stale authority, uncertain writes and backend
    /// failures. A page is not a full-history integrity audit.
    pub async fn read_history(
        &self,
        cursor: Option<HistoryCursor>,
        limits: HistoryReadLimits,
    ) -> Result<HistoryPage, PersistenceError> {
        self.history_read(Selection::Ordered(cursor), limits, None)
            .await
    }

    pub(super) async fn read_history_through(
        &self,
        cursor: Option<HistoryCursor>,
        limits: HistoryReadLimits,
        through: Option<RecordSequence>,
    ) -> Result<HistoryPage, PersistenceError> {
        self.history_read(Selection::Ordered(cursor), limits, through)
            .await
    }

    async fn history_read(
        &self,
        selection: Selection,
        limits: HistoryReadLimits,
        through: Option<RecordSequence>,
    ) -> Result<HistoryPage, PersistenceError> {
        if let Selection::Ordered(Some(cursor)) = &selection
            && (cursor.session != self.attachment.public_id || cursor.agent != self.agent)
        {
            return Err(PersistenceError::InvalidHistoryCursor);
        }
        let request = self.attachment.database.request()?;
        let store = self.clone();
        tokio::spawn(async move {
            let _request = request;
            let operation =
                tokio::time::timeout(WAIT, Arc::clone(&store.attachment.operation).lock_owned())
                    .await
                    .map_err(|_| PersistenceError::Busy)?;
            if matches!(*operation, AttachmentState::Uncertain { .. }) {
                return Err(PersistenceError::OutcomeUnknown);
            }
            let mut reader = tokio::time::timeout(WAIT, store.attachment.database.reader.lock())
                .await
                .map_err(|_| PersistenceError::Busy)?;
            reader.settle().await?;
            reader.attachment = Some(Arc::clone(&store.attachment.lease));
            let mut transaction = reader.connection()?.begin().await?;
            let result = async {
                reads::authority(&mut transaction, &store).await?;
                query(&mut transaction, &store, selection, limits, through).await
            }
            .await;
            match result {
                Ok(page) => {
                    transaction.commit().await?;
                    reader.attachment = None;
                    Ok(page)
                }
                Err(error) => {
                    transaction.rollback().await?;
                    reader.attachment = None;
                    Err(error)
                }
            }
        })
        .await?
    }
}

enum Selection {
    Exact(RecordId),
    Ordered(Option<HistoryCursor>),
}

async fn query(
    connection: &mut SqliteConnection,
    store: &SqliteStore,
    selection: Selection,
    limits: HistoryReadLimits,
    upper: Option<RecordSequence>,
) -> Result<HistoryPage, PersistenceError> {
    let mut page = HistoryPage {
        records: Vec::new(),
        through: None,
        next_cursor: None,
    };
    let (metadata, after) = match &selection {
        Selection::Exact(record) => {
            let rows = sqlx::query_as::<_, reads::RecordMetadata>(
                "SELECT id, sequence, invocation, runtime_epoch, payload_version, coalesce(octet_length(payload_json), archive_decoded_bytes) AS payload_bytes \
                 FROM records WHERE id = ? AND session_id = ? AND agent_id = ?",
            ).bind(i64::from(*record)).bind(i64::from(store.attachment.session_id)).bind(i64::from(store.agent))
                .fetch_all(&mut *connection).await?;
            (rows, 0)
        }
        Selection::Ordered(cursor) => {
            let latest: Option<i64> = sqlx::query_scalar(
                "SELECT sequence FROM records WHERE session_id = ? AND agent_id = ? ORDER BY sequence DESC LIMIT 1",
            ).bind(i64::from(store.attachment.session_id)).bind(i64::from(store.agent))
                .fetch_optional(&mut *connection).await?;
            let latest = latest
                .map(RecordSequence::try_from)
                .transpose()
                .map_err(|_| PersistenceError::InvalidData("history upper sequence"))?;
            let through = match cursor {
                Some(cursor) if latest.is_some_and(|latest| cursor.through <= latest) => {
                    Some(cursor.through)
                }
                Some(_) => return Err(PersistenceError::InvalidHistoryCursor),
                None => latest,
            };
            let through = through.map(|through| upper.map_or(through, |upper| upper.min(through)));
            page.through = through;
            let after = cursor.as_ref().map_or(0, |cursor| i64::from(cursor.after));
            let rows = sqlx::query_as::<_, reads::RecordMetadata>(
                "SELECT id, sequence, invocation, runtime_epoch, payload_version, coalesce(octet_length(payload_json), archive_decoded_bytes) AS payload_bytes \
                 FROM records WHERE session_id = ? AND agent_id = ? AND sequence > ? AND sequence <= ? \
                 ORDER BY sequence LIMIT ?",
            ).bind(i64::from(store.attachment.session_id)).bind(i64::from(store.agent)).bind(after)
                .bind(through.map_or(0, i64::from)).bind(i64::from(limits.records)).fetch_all(&mut *connection).await?;
            (rows, after)
        }
    };
    if metadata.is_empty()
        && page
            .through
            .is_some_and(|through| i64::from(through) > after)
    {
        return Err(PersistenceError::InvalidData("history sequence gap"));
    }
    let mut selected = Vec::with_capacity(metadata.len());
    let mut records_bytes = 0_usize;
    let mut payload_bytes = 0_usize;
    let mut previous = after;
    for metadata in metadata {
        if matches!(selection, Selection::Ordered(_)) && metadata.sequence != previous + 1 {
            return Err(PersistenceError::InvalidData("history sequence gap"));
        }
        let payload = reads::metadata_bytes(&metadata, store)?;
        let record_bytes = stored_bytes(&metadata, payload)?;
        let cursor = page
            .through
            .filter(|through| i64::from(*through) > metadata.sequence)
            .map(|through| {
                Ok::<_, PersistenceError>(HistoryCursor {
                    session: store.attachment.public_id.clone(),
                    agent: store.agent,
                    after: RecordSequence::try_from(metadata.sequence)
                        .map_err(|_| PersistenceError::InvalidData("history sequence"))?,
                    through,
                })
            })
            .transpose()?;
        let header_bytes = match selection {
            Selection::Exact(_) => 0,
            Selection::Ordered(_) => encoded_size(&HistoryPage {
                records: Vec::new(),
                through: page.through,
                next_cursor: cursor.clone(),
            })?,
        };
        let bytes_required = header_bytes
            .checked_add(records_bytes)
            .and_then(|bytes| bytes.checked_add(record_bytes))
            .and_then(|bytes| bytes.checked_add(usize::from(!selected.is_empty())))
            .ok_or(StoreError::Capacity)?;
        if bytes_required > limits.bytes {
            if selected.is_empty() {
                return Err(PersistenceError::HistoryRecordTooLarge {
                    record: RecordId::try_from(metadata.id)
                        .map_err(|_| PersistenceError::InvalidData("history identity"))?,
                    bytes_required: u64::try_from(bytes_required)
                        .map_err(|_| StoreError::Capacity)?,
                });
            }
            break;
        }
        records_bytes = records_bytes
            .checked_add(record_bytes)
            .and_then(|bytes| bytes.checked_add(usize::from(!selected.is_empty())))
            .ok_or(StoreError::Capacity)?;
        payload_bytes = payload_bytes
            .checked_add(payload)
            .ok_or(StoreError::Capacity)?;
        previous = metadata.sequence;
        page.next_cursor = cursor;
        selected.push(metadata);
    }
    let _payload = store.attachment.database.payload(payload_bytes)?;
    page.records = reads::payload_pages(connection, store, selected).await?;
    let actual_bytes = match selection {
        Selection::Exact(_) => encoded_size(&page.records.first())?,
        Selection::Ordered(_) => encoded_size(&page)?,
    };
    if actual_bytes > limits.bytes {
        return Err(StoreError::Capacity.into());
    }
    Ok(page)
}

fn stored_bytes(
    metadata: &reads::RecordMetadata,
    record_bytes: usize,
) -> Result<usize, StoreError> {
    record_bytes
        .checked_add(br#"{"id":,"sequence":,"record":}"#.len())
        .and_then(|bytes| bytes.checked_add(metadata.id.to_string().len()))
        .and_then(|bytes| bytes.checked_add(metadata.sequence.to_string().len()))
        .ok_or(StoreError::Capacity)
}
