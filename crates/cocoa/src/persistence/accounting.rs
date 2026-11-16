//! Per-transport-attempt evidence, stored independently of provider replay.

use async_trait::async_trait;
use cocoa_core::{
    accounting::{RequestPurpose, RequestRecorder, RequestSettings, RequestUpdate},
    store::{InvocationId, StoreError},
};
use sqlx::Connection;

use super::*;

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
