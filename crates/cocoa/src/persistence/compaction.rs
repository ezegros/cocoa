//! Agent-control publication shares the original journal's durability boundary.

use std::{collections::BTreeMap, sync::Arc};

use cocoa_core::{
    compaction::{Checkpoint, StoredCheckpoint},
    store::{AgentId, RecordId, StoreError, StoreSnapshot},
};
use sqlx::{Connection, FromRow, SqliteConnection};

use super::{
    Database, PersistenceError, SessionAttachment, SessionId, SqliteStore, WAIT, active, archives,
    reads, storage, writes,
};

pub(super) const CHECKPOINT_BYTES_MAX: usize = 8 * 1024 * 1024;

pub(super) async fn publish(
    store: &SqliteStore,
    checkpoint: Checkpoint,
) -> Result<RecordId, PersistenceError> {
    let snapshot = reads::load(store).await?;
    if snapshot.records.last().map(|record| record.id) != Some(checkpoint.after()) {
        return Err(StoreError::InvalidCheckpoint.into());
    }
    let previous = snapshot.checkpoints.last().map(|stored| stored.id);
    let sequence = i64::try_from(snapshot.checkpoints.len())
        .ok()
        .and_then(|length| length.checked_add(1))
        .ok_or(StoreError::IdentityExhausted)?;
    let (checkpoint, encoded) = tokio::task::spawn_blocking(move || {
        let encoded = serde_json::to_string(&checkpoint).map_err(|_| StoreError::Encoding)?;
        let restored =
            Checkpoint::restore(&snapshot, &encoded).map_err(|_| StoreError::InvalidCheckpoint)?;
        Ok::<_, PersistenceError>((restored, encoded))
    })
    .await??;
    let prepared = match archives::prepare(store, checkpoint.content().through).await {
        Ok(prepared) => prepared,
        Err(error) => {
            tracing::warn!(%error, "checkpoint publication proceeds with original payloads inline");
            Vec::new()
        }
    };
    let mut writer = tokio::time::timeout(WAIT, store.attachment.database.writer.lock())
        .await
        .map_err(|_| PersistenceError::Busy)?;
    writer.settle().await?;
    writer.attachment = Some(Arc::clone(&store.attachment.lease));
    let result = async {
        let mut transaction = writer.connection()?.begin_with("BEGIN IMMEDIATE").await?;
        let result = async {
            reads::writable(&mut transaction, store).await?;
            let tail: Option<i64> = sqlx::query_scalar(
                "SELECT id FROM records WHERE session_id = ? AND agent_id = ? ORDER BY sequence DESC LIMIT 1",
            )
            .bind(i64::from(store.attachment.session_id))
            .bind(i64::from(store.agent))
            .fetch_optional(&mut *transaction)
            .await?;
            let head: Option<(i64, i64)> = sqlx::query_as(
                "SELECT id, sequence FROM checkpoints WHERE session_id = ? AND agent_id = ? ORDER BY sequence DESC LIMIT 1",
            )
            .bind(i64::from(store.attachment.session_id))
            .bind(i64::from(store.agent))
            .fetch_optional(&mut *transaction)
            .await?;
            if tail != Some(i64::from(checkpoint.after()))
                || head.map(|head| head.0) != previous.map(i64::from)
                || head.map_or(0, |head| head.1) != sequence - 1
            {
                return Err(StoreError::InvalidCheckpoint.into());
            }
            let maximum: i64 = sqlx::query_scalar(
                "SELECT max(coalesce((SELECT max(id) FROM records), 0), coalesce((SELECT max(id) FROM checkpoints), 0))",
            )
            .fetch_one(&mut *transaction)
            .await?;
            let id = maximum.checked_add(1).ok_or(StoreError::IdentityExhausted)?;
            let now = writes::timestamp()?;
            sqlx::query(
                "INSERT INTO checkpoints (id, session_id, agent_id, sequence, after_record_id, through_record_id, format_version, created_at_ms, payload_json) \
                 VALUES (?, ?, ?, ?, ?, ?, 1, ?, ?)",
            )
            .bind(id)
            .bind(i64::from(store.attachment.session_id))
            .bind(i64::from(store.agent))
            .bind(sequence)
            .bind(i64::from(checkpoint.after()))
            .bind(i64::from(checkpoint.content().through))
            .bind(now)
            .bind(&encoded)
            .execute(&mut *transaction)
            .await?;
            let mut archive_transaction = transaction.begin().await?;
            match archives::apply(&mut archive_transaction, store, &prepared).await {
                Ok(()) => archive_transaction.commit().await?,
                Err(error) => {
                    archive_transaction.rollback().await?;
                    tracing::warn!(%error, "checkpoint retained; archive move rolled back to inline originals");
                }
            }
            sqlx::query("UPDATE sessions SET updated_at_ms = max(updated_at_ms, ?) WHERE id = ? AND runtime_epoch = ?")
                .bind(now)
                .bind(i64::from(store.attachment.session_id))
                .bind(store.attachment.epoch)
                .execute(&mut *transaction)
                .await?;
            RecordId::try_from(id)
                .map_err(|_| PersistenceError::InvalidData("checkpoint identity"))
        }
        .await;
        match result {
            Ok(id) => match transaction.commit().await {
                Ok(()) => Ok(id),
                Err(error) => {
                    tracing::error!(%error, "checkpoint commit requires reconciliation");
                    Err(PersistenceError::OutcomeUnknown)
                }
            },
            Err(error) => match transaction.rollback().await {
                Ok(()) => Err(error),
                Err(rollback) => {
                    tracing::error!(%rollback, "checkpoint rollback requires reconciliation");
                    Err(PersistenceError::OutcomeUnknown)
                }
            },
        }
    }
    .await;
    if result.is_ok()
        && let Err(error) =
            storage::reclaim(writer.connection()?, &store.attachment.database.path).await
    {
        tracing::warn!(%error, "checkpoint committed; storage reclamation deferred");
    }
    if let Err(error) = writer.settle().await {
        tracing::error!(%error, "checkpoint writer did not settle");
        return Err(PersistenceError::OutcomeUnknown);
    }
    if let Ok(id) = &result {
        *store.checkpoint_cache.lock().await = Some(active::CachedHead {
            stored: StoredCheckpoint {
                id: *id,
                checkpoint,
            },
            sequence,
            encoded,
        });
    }
    result
}

pub(super) async fn restore(
    connection: &mut SqliteConnection,
    store: &SqliteStore,
    snapshot: &mut StoreSnapshot,
) -> Result<(), PersistenceError> {
    restore_agent(
        connection,
        &store.attachment.database,
        store.attachment.session_id,
        store.agent,
        snapshot,
    )
    .await?;
    let cached = snapshot
        .checkpoints
        .last()
        .map(|stored| {
            Ok::<_, PersistenceError>(active::CachedHead {
                stored: stored.clone(),
                sequence: i64::try_from(snapshot.checkpoints.len())
                    .map_err(|_| StoreError::Capacity)?,
                encoded: serde_json::to_string(&stored.checkpoint)
                    .map_err(|_| StoreError::Encoding)?,
            })
        })
        .transpose()?;
    *store.checkpoint_cache.lock().await = cached;
    Ok(())
}

pub(super) async fn restore_session(
    connection: &mut SqliteConnection,
    attachment: &SessionAttachment,
    histories: &mut BTreeMap<AgentId, StoreSnapshot>,
) -> Result<(), PersistenceError> {
    for (agent, snapshot) in histories {
        restore_agent(
            connection,
            &attachment.inner.database,
            attachment.id(),
            *agent,
            snapshot,
        )
        .await?;
    }
    Ok(())
}

#[derive(FromRow)]
struct CheckpointMetadata {
    id: i64,
    sequence: i64,
    after_record_id: i64,
    through_record_id: i64,
    format_version: i64,
    bytes: i64,
}

async fn restore_agent(
    connection: &mut SqliteConnection,
    database: &Database,
    session: SessionId,
    agent: AgentId,
    snapshot: &mut StoreSnapshot,
) -> Result<(), PersistenceError> {
    let mut sequence = 0_i64;
    loop {
        let rows: Vec<CheckpointMetadata> = sqlx::query_as(
            "SELECT id, sequence, after_record_id, through_record_id, format_version, octet_length(payload_json) AS bytes \
             FROM checkpoints WHERE session_id = ? AND agent_id = ? AND sequence > ? ORDER BY sequence LIMIT 64",
        )
        .bind(i64::from(session))
        .bind(i64::from(agent))
        .bind(sequence)
        .fetch_all(&mut *connection)
        .await?;
        if rows.is_empty() {
            break;
        }
        for row in rows {
            if row.format_version != 1 {
                return Err(PersistenceError::UnsupportedPayload(row.format_version));
            }
            if row.sequence
                != sequence
                    .checked_add(1)
                    .ok_or(StoreError::IdentityExhausted)?
                || !(1..=CHECKPOINT_BYTES_MAX as i64).contains(&row.bytes)
            {
                return Err(PersistenceError::InvalidData("checkpoint sequence/size"));
            }
            let _payload = database.payload(row.bytes as usize)?;
            let encoded: Option<String> = sqlx::query_scalar(
                "SELECT CASE WHEN octet_length(payload_json) = ? THEN payload_json END FROM checkpoints \
                 WHERE id = ? AND session_id = ? AND agent_id = ?",
            )
            .bind(row.bytes)
            .bind(row.id)
            .bind(i64::from(session))
            .bind(i64::from(agent))
            .fetch_one(&mut *connection)
            .await?;
            let encoded = encoded.ok_or(PersistenceError::InvalidData("checkpoint changed"))?;
            let history = std::mem::replace(
                snapshot,
                StoreSnapshot {
                    records: Vec::new(),
                    instruction_state: None,
                    checkpoints: Vec::new(),
                },
            );
            sequence = row.sequence;
            *snapshot = tokio::task::spawn_blocking(move || {
                let mut history = history;
                let checkpoint = Checkpoint::restore(&history, &encoded)
                    .map_err(|_| PersistenceError::InvalidData("checkpoint contents or lineage"))?;
                if i64::from(checkpoint.after()) != row.after_record_id
                    || i64::from(checkpoint.content().through) != row.through_record_id
                {
                    return Err(PersistenceError::InvalidData("checkpoint envelope"));
                }
                let id = RecordId::try_from(row.id)
                    .map_err(|_| PersistenceError::InvalidData("checkpoint identity"))?;
                if history.records.iter().any(|record| record.id == id)
                    || history.checkpoints.iter().any(|stored| stored.id == id)
                {
                    return Err(PersistenceError::InvalidData(
                        "checkpoint identity collision",
                    ));
                }
                history
                    .checkpoints
                    .push(StoredCheckpoint { id, checkpoint });
                Ok::<_, PersistenceError>(history)
            })
            .await??;
        }
    }
    Ok(())
}
