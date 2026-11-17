//! Per-transport-attempt evidence, stored independently of provider replay.

use async_trait::async_trait;
use cocoa_core::{
    accounting::{RequestPurpose, RequestRecorder, RequestSettings, RequestUpdate},
    store::{InvocationId, StoreError},
};
use sqlx::Connection;

use super::*;

/// Small durable accounting read. No conversation or provider continuation is copied.
#[derive(Clone, Debug)]
pub struct UsageLedger {
    pub settings: StoredSessionSettings,
    pub requests: Vec<UsageRequest>,
    /// False for legacy/forked sessions or when the bounded request read cannot fit.
    pub complete: bool,
    /// Latest root response with reported usage, retaining the previous response
    /// while a new request has not reported counters. Compaction is excluded.
    pub latest_root_usage: Option<cocoa_core::model::Usage>,
}

/// Settings and latest counters for one transport attempt, including retries.
#[derive(Clone, Debug)]
pub struct UsageRequest {
    pub settings: Option<RequestSettings>,
    pub update: RequestUpdate,
}

impl SessionAttachment {
    /// Read all agents' request accounting in one coherent transaction, across
    /// runtime epochs. Coverage checks inspect at most 4,096 request/history rows
    /// and 8 MiB of active JSON. Archived history uses kind metadata, not decompression.
    /// Exceeding either hides the total rather than returning partial coverage.
    /// Root counters use the 4,096 newest requests independently of those bounds.
    ///
    /// # Errors
    /// Reject stale ownership, uncertain writes, malformed data, exhausted
    /// admission or database failures. This does not recover or mutate a session.
    pub async fn usage_ledger(&self) -> Result<UsageLedger, PersistenceError> {
        let request = self.inner.database.request()?;
        let payload = self.inner.database.payload(USAGE_BYTES_MAX + 128 * 1024)?;
        let attachment = self.clone();
        tokio::spawn(async move {
            let _request = request;
            let _payload = payload;
            let operation = tokio::time::timeout(
                WAIT,
                Arc::clone(&attachment.inner.operation).lock_owned(),
            )
            .await
            .map_err(|_| PersistenceError::Busy)?;
            if matches!(*operation, AttachmentState::Uncertain { .. }) {
                return Err(PersistenceError::OutcomeUnknown);
            }
            let mut reader = tokio::time::timeout(WAIT, attachment.inner.database.reader.lock())
                .await
                .map_err(|_| PersistenceError::Busy)?;
            reader.settle().await?;
            reader.attachment = Some(Arc::clone(&attachment.inner.lease));
            let mut transaction = reader.connection()?.begin().await?;
            sessions::check_epoch(&mut transaction, &attachment).await?;
            let settings = sessions::settings(&mut transaction, attachment.id()).await?;
            let session = i64::from(attachment.id());
            let covered: bool = sqlx::query_scalar(
                "SELECT accounting_version = 1 AND NOT EXISTS \
                 (SELECT 1 FROM conversation_forks WHERE session_id = sessions.id) \
                 FROM sessions WHERE id = ?",
            )
            .bind(session)
            .fetch_one(&mut *transaction)
            .await?;
            let latest: Option<String> = sqlx::query_scalar(
                "SELECT CASE WHEN octet_length(update_json) <= 16384 THEN update_json END \
                 FROM (SELECT id, agent_id, purpose_json, update_json FROM model_requests \
                 WHERE session_id = ? ORDER BY id DESC LIMIT 4096) WHERE agent_id = ? \
                 AND purpose_json = '\"response\"' AND json_type(update_json, '$.usage') = 'object' \
                 ORDER BY id DESC LIMIT 1",
            )
            .bind(session)
            .bind(i64::from(attachment.root_agent()))
            .fetch_optional(&mut *transaction)
            .await?
            .flatten();
            let latest_root_usage = latest
                .map(|text| decode_update(&text).map(|update| update.usage))
                .transpose()?
                .flatten();
            let sizes: Vec<i64> = sqlx::query_scalar(
                "SELECT octet_length(settings_json) + octet_length(update_json) \
                 FROM model_requests WHERE session_id = ? \
                 UNION ALL SELECT coalesce(octet_length(payload_json), 0) \
                 FROM records WHERE session_id = ? LIMIT 4097",
            )
            .bind(session)
            .bind(session)
            .fetch_all(&mut *transaction)
            .await?;
            let bytes = sizes.iter().try_fold(0_usize, |total, &size| {
                usize::try_from(size).ok().and_then(|size| total.checked_add(size))
            });
            let fits = sizes.len() <= USAGE_ROWS_MAX
                && bytes.is_some_and(|bytes| bytes <= USAGE_BYTES_MAX);
            let legacy: bool = if covered && fits {
                sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM records AS record WHERE record.session_id = ? \
                     GROUP BY record.agent_id HAVING count(CASE WHEN archive_kind = 'ModelCompleted' \
                     OR json_type(payload_json, '$.ModelCompleted') = 'object' THEN 1 END) > \
                     (SELECT count(*) FROM model_requests WHERE session_id = record.session_id \
                     AND agent_id = record.agent_id \
                     AND purpose_json = '\"response\"' \
                     AND json_extract(update_json, '$.outcome') = 'completed'))",
                )
                .bind(session)
                .fetch_one(&mut *transaction)
                .await?
            } else {
                false
            };
            let rows: Vec<(String, String)> = if fits {
                sqlx::query_as(
                    "SELECT settings_json, update_json FROM model_requests \
                     WHERE session_id = ? ORDER BY id LIMIT 4096",
                )
                .bind(session)
                .fetch_all(&mut *transaction)
                .await?
            } else {
                Vec::new()
            };
            transaction.commit().await?;
            reader.attachment = None;
            drop(reader);
            drop(operation);
            tokio::task::spawn_blocking(move || {
                let mut requests = Vec::with_capacity(rows.len());
                for (settings, update) in rows {
                    requests.push(UsageRequest {
                        settings: serde_json::from_str(&settings).map_err(|_| {
                            PersistenceError::InvalidData("request accounting settings")
                        })?,
                        update: decode_update(&update)?,
                    });
                }
                Ok(UsageLedger {
                    settings,
                    requests,
                    complete: covered && fits && !legacy,
                    latest_root_usage,
                })
            })
            .await?
        })
        .await?
    }
}

fn decode_update(text: &str) -> Result<RequestUpdate, PersistenceError> {
    serde_json::from_str(text)
        .map_err(|_| PersistenceError::InvalidData("request accounting update"))
}

const USAGE_ROWS_MAX: usize = 4096;
const USAGE_BYTES_MAX: usize = 8 * 1024 * 1024;

pub(super) struct Recorder {
    pub store: SqliteStore,
    pub invocation: InvocationId,
}

#[async_trait]
impl RequestRecorder for Recorder {
    async fn start(
        &self,
        purpose: RequestPurpose,
        settings: Option<&RequestSettings>,
    ) -> Result<i64, StoreError> {
        let purpose = serde_json::to_string(&purpose).map_err(|_| StoreError::Encoding)?;
        let settings = serde_json::to_string(&settings).map_err(|_| StoreError::Encoding)?;
        if settings.len() > 4096 {
            return Err(StoreError::RecordLimit);
        }
        let update =
            serde_json::to_string(&RequestUpdate::default()).map_err(|_| StoreError::Encoding)?;
        self.write(None, purpose, settings, update)
            .await
            .map_err(store_error)
    }

    async fn update(&self, request: i64, update: &RequestUpdate) -> Result<(), StoreError> {
        let update = serde_json::to_string(update).map_err(|_| StoreError::Encoding)?;
        if update.len() > 16384 {
            return Err(StoreError::RecordLimit);
        }
        self.write(Some(request), String::new(), String::new(), update)
            .await
            .map_err(store_error)?;
        Ok(())
    }
}

impl Recorder {
    async fn write(
        &self,
        request_id: Option<i64>,
        purpose: String,
        settings: String,
        update: String,
    ) -> Result<i64, PersistenceError> {
        let request = self.store.attachment.database.request()?;
        let store = self.store.clone();
        let invocation = i64::try_from(self.invocation.0).map_err(|_| StoreError::InvalidRecord)?;
        if invocation == 0 {
            return Err(StoreError::InvalidRecord.into());
        }
        let mut operation =
            tokio::time::timeout(WAIT, Arc::clone(&store.attachment.operation).lock_owned())
                .await
                .map_err(|_| PersistenceError::Busy)?;
        if matches!(*operation, AttachmentState::Uncertain { .. }) {
            return Err(PersistenceError::OutcomeUnknown);
        }
        *operation = AttachmentState::Uncertain { agent: store.agent };
        let (result, mut operation) = tokio::spawn(async move {
            let _request = request;
            let result = async {
                let mut writer = tokio::time::timeout(WAIT, store.attachment.database.writer.lock()).await.map_err(|_| PersistenceError::Busy)?;
                writer.settle().await?;
                writer.attachment = Some(Arc::clone(&store.attachment.lease));
                let mut transaction = writer.connection()?.begin_with("BEGIN IMMEDIATE").await?;
                reads::authority(&mut transaction, &store).await?;
                let now = writes::timestamp()?;
                let id = match request_id {
                    None => {
                        let inserted = sqlx::query("INSERT INTO model_requests (session_id, agent_id, invocation, runtime_epoch, started_at_ms, updated_at_ms, purpose_json, settings_json, update_json) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)")
                            .bind(i64::from(store.attachment.session_id)).bind(i64::from(store.agent)).bind(invocation).bind(store.attachment.epoch).bind(now).bind(now).bind(purpose).bind(settings).bind(update).execute(&mut *transaction).await?;
                        inserted.last_insert_rowid()
                    }
                    Some(id) => {
                        let changed = sqlx::query("UPDATE model_requests SET update_json = ?, updated_at_ms = max(started_at_ms, ?) WHERE id = ? AND session_id = ? AND agent_id = ? AND invocation = ? AND runtime_epoch = ?")
                            .bind(update).bind(now).bind(id).bind(i64::from(store.attachment.session_id)).bind(i64::from(store.agent)).bind(invocation).bind(store.attachment.epoch).execute(&mut *transaction).await?;
                        if changed.rows_affected() != 1 { return Err(PersistenceError::InvalidData("request accounting identity")); }
                        id
                    }
                };
                transaction.commit().await.map_err(|error| {
                    tracing::error!(%error, "request accounting commit needs reconciliation");
                    PersistenceError::OutcomeUnknown
                })?;
                writer.attachment = None;
                Ok(id)
            }.await;
            (result, operation)
        }).await?;
        if !matches!(result, Err(PersistenceError::OutcomeUnknown)) {
            *operation = AttachmentState::Ready;
        }
        result
    }
}
