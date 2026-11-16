//! Bounded archive catch-up and space accounting, independent of model generation.

use std::{path::PathBuf, sync::Arc, time::Duration};

use serde::Serialize;
use sqlx::{Connection, SqliteConnection};
use tokio::time::Instant;

use super::{PersistenceError, SessionDatabase, SqliteStore, WAIT, archives, reads};

/// Logical payload sizes and database/WAL file lengths, in bytes.
/// Archives retain all originals; free database pages are reusable, not yet freed
/// filesystem space. File lengths do not measure filesystem block allocation and
/// are observed after the SQLite read.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StorageUsage {
    pub inline_bytes: u64,
    pub archive_bytes: u64,
    pub archive_decoded_bytes: u64,
    pub checkpoint_bytes: u64,
    pub database_bytes: u64,
    pub reusable_bytes: u64,
    pub wal_bytes: u64,
}

impl SessionDatabase {
    /// Account for this database without decoding originals or acquiring execution
    /// ownership. This explicit diagnostic scans payload metadata, not hot requests.
    /// It does not acknowledge uncertain mutations or delete evidence/backups.
    ///
    /// # Errors
    /// Reject exhausted admission, invalid accounting, SQL or filesystem failures.
    pub async fn storage_usage(&self) -> Result<StorageUsage, PersistenceError> {
        let request = self.inner.request()?;
        let database = Arc::clone(&self.inner);
        tokio::spawn(async move {
            let _request = request;
            let mut reader = tokio::time::timeout(WAIT, database.reader.lock()).await
                .map_err(|_| PersistenceError::Busy)?;
            reader.settle().await?;
            let connection = reader.connection()?;
            let totals: (i64, i64, i64) = sqlx::query_as(
                "SELECT coalesce(sum(octet_length(payload_json)),0), coalesce(sum(length(archive_zlib)),0), coalesce(sum(archive_decoded_bytes),0) FROM records",
            ).fetch_one(&mut *connection).await?;
            let checkpoints: i64 = sqlx::query_scalar("SELECT coalesce(sum(octet_length(payload_json)),0) FROM checkpoints")
                .fetch_one(&mut *connection).await?;
            let reusable: i64 = sqlx::query_scalar(
                "SELECT freelist_count * page_size FROM pragma_page_size(), pragma_freelist_count()",
            ).fetch_one(connection).await?;
            let path = database.path.clone();
            drop(reader);
            let (database_bytes, wal_bytes) = tokio::task::spawn_blocking(move || {
                Ok::<_, PersistenceError>((std::fs::metadata(&path)?.len(), wal_bytes(&path)?))
            }).await??;
            let count = |value| u64::try_from(value).map_err(|_| PersistenceError::InvalidData("negative storage accounting"));
            Ok(StorageUsage {
                inline_bytes: count(totals.0)?,
                archive_bytes: count(totals.1)?,
                archive_decoded_bytes: count(totals.2)?,
                checkpoint_bytes: count(checkpoints)?,
                database_bytes,
                reusable_bytes: count(reusable)?,
                wal_bytes,
            })
        }).await?
    }
}

impl SqliteStore {
    /// Run one bounded archive catch-up batch without generating another summary.
    /// This can move at most 64 covered originals and 8 MiB of source payloads.
    /// Routine appends also attempt catch-up at most once per 60 seconds. Reclaim
    /// at least 8 MiB of free pages when they cover half a database of at most
    /// 64 MiB, sufficient temporary space exists, and a five-second bound permits.
    /// No evidence or backups expire. Returned accounting covers the database.
    ///
    /// # Errors
    /// Reject uncertain writes, unavailable authority, admission and storage errors.
    /// An abandoned/unknown write requires a successful load before further writes.
    pub async fn maintain_storage(&self) -> Result<StorageUsage, PersistenceError> {
        let request = self.attachment.database.request()?;
        let payload = self.attachment.database.payload(0)?;
        let mut operation =
            tokio::time::timeout(WAIT, Arc::clone(&self.attachment.operation).lock_owned())
                .await
                .map_err(|_| PersistenceError::Busy)?;
        if matches!(*operation, super::AttachmentState::Uncertain { .. }) {
            return Err(PersistenceError::OutcomeUnknown);
        }
        *operation = super::AttachmentState::Uncertain { agent: self.agent };
        let store = self.clone();
        let mut completion = tokio::spawn(async move {
            let result = async {
                if store.checkpoint_cache.lock().await.is_none() {
                    reads::load(&store).await?;
                }
                maintain(&store, Maintenance::Requested).await
            }
            .await;
            let known = !matches!(result, Err(PersistenceError::OutcomeUnknown));
            super::AppendCompletion {
                result,
                known,
                operation,
                _attachment: Arc::clone(&store.attachment),
                _request: request,
                _payload: payload,
            }
        })
        .await?;
        if completion.known {
            *completion.operation = super::AttachmentState::Ready;
        }
        completion.result?;
        SessionDatabase {
            inner: Arc::clone(&self.attachment.database),
        }
        .storage_usage()
        .await
    }
}

pub(super) enum Maintenance {
    Due,
    Requested,
}

pub(super) async fn maintain(
    store: &SqliteStore,
    trigger: Maintenance,
) -> Result<(), PersistenceError> {
    let mut next = store.storage_next.lock().await;
    if matches!(trigger, Maintenance::Due) && Instant::now() < *next {
        return Ok(());
    }
    *next = Instant::now() + MAINTENANCE_INTERVAL;
    drop(next);
    let Some(cached) = store.checkpoint_cache.lock().await.clone() else {
        return Ok(());
    };
    let prepared = archives::prepare(store, cached.stored.checkpoint.content().through).await?;
    let mut writer = tokio::time::timeout(WAIT, store.attachment.database.writer.lock())
        .await
        .map_err(|_| PersistenceError::Busy)?;
    writer.settle().await?;
    writer.attachment = Some(Arc::clone(&store.attachment.lease));
    let result = async {
        let mut transaction = writer.connection()?.begin_with("BEGIN IMMEDIATE").await?;
        let result = async {
            reads::writable(&mut transaction, store).await?;
            let head: Option<i64> = sqlx::query_scalar("SELECT id FROM checkpoints WHERE session_id = ? AND agent_id = ? ORDER BY sequence DESC LIMIT 1")
                .bind(i64::from(store.attachment.session_id)).bind(i64::from(store.agent))
                .fetch_optional(&mut *transaction).await?;
            if head != Some(i64::from(cached.stored.id)) {
                return Err(PersistenceError::InvalidData("maintenance checkpoint changed"));
            }
            archives::apply(&mut transaction, store, &prepared).await
        }.await;
        match result {
            Ok(()) => match transaction.commit().await {
                Ok(()) => Ok(()),
                Err(error) => {
                    tracing::error!(%error, "archive maintenance commit requires reconciliation");
                    Err(PersistenceError::OutcomeUnknown)
                }
            },
            Err(error) => match transaction.rollback().await {
                Ok(()) => Err(error),
                Err(rollback) => {
                    tracing::error!(%rollback, "archive maintenance rollback requires reconciliation");
                    Err(PersistenceError::OutcomeUnknown)
                }
            }
        }
    }.await;
    if let Err(error) = writer.settle().await {
        tracing::error!(%error, "archive maintenance writer did not settle");
        return Err(PersistenceError::OutcomeUnknown);
    }
    result?;
    reclaim(writer.connection()?, &store.attachment.database.path).await
}

pub(super) async fn reclaim(
    connection: &mut SqliteConnection,
    path: &std::path::Path,
) -> Result<(), PersistenceError> {
    let checkpoint: (i64, i64, i64) = sqlx::query_as("PRAGMA wal_checkpoint(PASSIVE)")
        .fetch_one(&mut *connection)
        .await?;
    tracing::debug!(
        busy = checkpoint.0,
        frames = checkpoint.1,
        checkpointed = checkpoint.2,
        "storage WAL checkpoint"
    );
    let allocation: (i64, i64) = sqlx::query_as(
        "SELECT page_count * page_size, freelist_count * page_size FROM pragma_page_count(), pragma_page_size(), pragma_freelist_count()",
    ).fetch_one(&mut *connection).await?;
    let eligible = allocation.1 >= RECLAIM_BYTES_MIN
        && allocation.1 >= allocation.0 / 2
        && allocation.0 <= REWRITE_BYTES_MAX;
    let mut rewritten = false;
    if eligible && enough_temporary_space(path, allocation.0).await? {
        let deadline = Instant::now() + WAIT;
        connection
            .lock_handle()
            .await?
            .set_progress_handler(1000, move || Instant::now() < deadline);
        let vacuum = sqlx::query("VACUUM").execute(&mut *connection).await;
        connection.lock_handle().await?.remove_progress_handler();
        match vacuum {
            Ok(_) => rewritten = true,
            Err(error) => {
                tracing::warn!(%error, "space reclamation deferred; retained originals and archives")
            }
        }
    }
    let path = path.to_owned();
    let (file_bytes, wal) = tokio::task::spawn_blocking(move || {
        Ok::<_, PersistenceError>((std::fs::metadata(&path)?.len(), wal_bytes(&path)?))
    })
    .await??;
    let rewrite_pending = i64::try_from(file_bytes)
        .ok()
        .is_some_and(|bytes| bytes.saturating_sub(allocation.0) >= RECLAIM_BYTES_MIN);
    if rewritten || rewrite_pending || wal >= WAL_BYTES_MAX {
        let result: (i64, i64, i64) = sqlx::query_as("PRAGMA wal_checkpoint(TRUNCATE)")
            .fetch_one(connection)
            .await?;
        if result.0 != 0 {
            tracing::debug!("WAL truncation deferred by active readers");
        }
    }
    Ok(())
}

const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(60);
const RECLAIM_BYTES_MIN: i64 = 8 * 1024 * 1024;
const REWRITE_BYTES_MAX: i64 = 64 * 1024 * 1024;
const WAL_BYTES_MAX: u64 = 8 * 1024 * 1024;

fn wal_bytes(path: &std::path::Path) -> Result<u64, PersistenceError> {
    let mut filename = path.as_os_str().to_owned();
    filename.push("-wal");
    match std::fs::metadata(PathBuf::from(filename)) {
        Ok(metadata) => Ok(metadata.len()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error.into()),
    }
}

async fn enough_temporary_space(
    path: &std::path::Path,
    database_bytes: i64,
) -> Result<bool, PersistenceError> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        #[cfg(unix)]
        {
            let available = rustix::fs::statvfs(&path).map_err(std::io::Error::from)?;
            let bytes = available.f_bavail.saturating_mul(available.f_frsize);
            Ok(bytes
                >= (database_bytes as u64)
                    .saturating_mul(3)
                    .saturating_add(128 * 1024 * 1024))
        }
        #[cfg(not(unix))]
        {
            let _ = (path, database_bytes);
            Ok(false)
        }
    })
    .await?
}
