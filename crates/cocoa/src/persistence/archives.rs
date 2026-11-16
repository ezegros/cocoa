//! Lossless per-record archives. Stable envelopes and receipts never move.

use std::sync::{Arc, Mutex, Weak};

use cocoa_core::store::{
    AgentId, InvocationId, Record, RecordId, RecordKind, StoreError, encoded_size,
};
use miniz_oxide::{
    deflate::compress_to_vec_zlib,
    inflate::{
        TINFLStatus,
        core::{DecompressorOxide, decompress, inflate_flags},
    },
};
use sha2::{Digest, Sha256};
use sqlx::{Connection, FromRow, QueryBuilder, Sqlite, SqliteConnection};
use tokio::sync::OwnedSemaphorePermit;

use super::{PersistenceError, RECORD_ENVELOPE_BYTES, RECORD_VERSION, SqliteStore, WAIT, reads};

/// Prepare at most 64 inline originals covered by an own-agent stable record.
/// Source bytes total at most 8 MiB. Larger originals remain inline.
/// A bounded attachment-local cursor advances past unprofitable attempts.
/// The caller holds the attachment operation lock, but no writer transaction.
///
/// # Errors
/// Reject stale authority, invalid boundaries, malformed originals, admission
/// exhaustion and database or worker failures.
pub(super) async fn prepare(
    store: &SqliteStore,
    through: RecordId,
) -> Result<Vec<ArchivedPayload>, PersistenceError> {
    let attachment = Arc::downgrade(&store.attachment);
    let after = {
        let cursors = CURSORS
            .lock()
            .map_err(|_| PersistenceError::InvalidData("archive cursor poisoned"))?;
        cursors
            .iter()
            .find(|cursor| cursor.attachment.ptr_eq(&attachment) && cursor.agent == store.agent)
            .map_or(0, |cursor| cursor.sequence)
    };
    let mut reader = tokio::time::timeout(WAIT, store.attachment.database.reader.lock())
        .await
        .map_err(|_| PersistenceError::Busy)?;
    reader.settle().await?;
    reader.attachment = Some(Arc::clone(&store.attachment.lease));
    let mut transaction = reader.connection()?.begin().await?;
    let result = async {
        reads::authority(&mut transaction, store).await?;
        let sequence: i64 = sqlx::query_scalar(
            "SELECT sequence FROM records WHERE id = ? AND session_id = ? AND agent_id = ?",
        )
        .bind(i64::from(through))
        .bind(i64::from(store.attachment.session_id))
        .bind(i64::from(store.agent))
        .fetch_optional(&mut *transaction)
        .await?
        .ok_or(PersistenceError::InvalidData("archive boundary"))?;
        let mut metadata = candidates(&mut transaction, store, sequence, after).await?;
        if metadata.is_empty() && after != 0 {
            metadata = candidates(&mut transaction, store, sequence, 0).await?;
        }
        let mut selected = Vec::with_capacity(metadata.len());
        let mut bytes = 0_usize;
        for source in metadata {
            let size = usize::try_from(source.bytes).map_err(|_| StoreError::Capacity)?;
            if size > ARCHIVE_BYTES_MAX - bytes {
                break;
            }
            if source.payload_version != RECORD_VERSION {
                return Err(PersistenceError::UnsupportedPayload(source.payload_version));
            }
            if source.id <= 0
                || source.sequence <= 0
                || source.invocation <= 0
                || source.runtime_epoch <= 0
                || source.runtime_epoch > store.attachment.epoch
                || source.created_at_ms < 0
                || size + source.invocation.to_string().len() + RECORD_ENVELOPE_BYTES
                    > store.limits.record_bytes
            {
                return Err(PersistenceError::InvalidData("archive source envelope"));
            }
            bytes += size;
            selected.push(source);
        }
        let payload = Arc::new(store.attachment.database.payload(bytes * 4)?);
        if selected.is_empty() {
            return Ok((Vec::new(), selected, payload));
        }
        let mut query = QueryBuilder::<Sqlite>::new(
            "SELECT id, payload_json FROM records NOT INDEXED WHERE session_id = ",
        );
        query
            .push_bind(i64::from(store.attachment.session_id))
            .push(" AND agent_id = ")
            .push_bind(i64::from(store.agent))
            .push(" AND id IN (");
        {
            let mut ids = query.separated(",");
            for source in &selected {
                ids.push_bind(source.id);
            }
        }
        query.push(") ORDER BY sequence");
        let rows: Vec<(i64, String)> = query.build_query_as().fetch_all(&mut *transaction).await?;
        Ok::<_, PersistenceError>((rows, selected, payload))
    }
    .await;
    let (rows, metadata, payload) = match result {
        Ok(prepared) => {
            transaction.commit().await?;
            reader.attachment = None;
            prepared
        }
        Err(error) => {
            transaction.rollback().await?;
            reader.attachment = None;
            return Err(error);
        }
    };
    drop(reader);
    let sequence = metadata.last().map(|source| source.sequence);
    let prepared = tokio::task::spawn_blocking(move || compress(rows, metadata, payload)).await??;
    if let Some(sequence) = sequence {
        let mut cursors = CURSORS
            .lock()
            .map_err(|_| PersistenceError::InvalidData("archive cursor poisoned"))?;
        cursors.retain(|cursor| cursor.attachment.strong_count() != 0);
        if let Some(cursor) = cursors
            .iter_mut()
            .find(|cursor| cursor.attachment.ptr_eq(&attachment) && cursor.agent == store.agent)
        {
            cursor.sequence = sequence;
        } else {
            if cursors.len() == CURSORS_MAX {
                cursors.remove(0);
            }
            cursors.push(ArchiveCursor {
                attachment,
                agent: store.agent,
                sequence,
            });
        }
    }
    Ok(prepared)
}

/// Switch prepared originals inside the caller's publication transaction.
/// Every original must still match its complete envelope, source size and digest.
/// The caller rolls back the transaction on any error.
///
/// # Errors
/// Reject stale authority, changed originals, invalid preparation and SQL errors.
pub(super) async fn apply(
    connection: &mut SqliteConnection,
    store: &SqliteStore,
    prepared: &[ArchivedPayload],
) -> Result<(), PersistenceError> {
    reads::authority(connection, store).await?;
    if prepared.len() > RECORDS_MAX {
        return Err(PersistenceError::InvalidData("archive preparation count"));
    }
    let mut bytes = 0_usize;
    for archive in prepared {
        let source = &archive.source;
        let size = usize::try_from(source.bytes).map_err(|_| StoreError::Capacity)?;
        bytes = bytes.checked_add(size).ok_or(StoreError::Capacity)?;
        if bytes > ARCHIVE_BYTES_MAX {
            return Err(PersistenceError::InvalidData("archive preparation bytes"));
        }
        let text: Option<String> = sqlx::query_scalar(
            "SELECT payload_json FROM records WHERE id = ? AND session_id = ? AND agent_id = ? \
             AND sequence = ? AND invocation = ? AND runtime_epoch = ? AND created_at_ms = ? \
             AND payload_version = ? AND octet_length(payload_json) = ?",
        )
        .bind(source.id)
        .bind(i64::from(store.attachment.session_id))
        .bind(i64::from(store.agent))
        .bind(source.sequence)
        .bind(source.invocation)
        .bind(source.runtime_epoch)
        .bind(source.created_at_ms)
        .bind(source.payload_version)
        .bind(source.bytes)
        .fetch_optional(&mut *connection)
        .await?;
        let text = text.ok_or(PersistenceError::InvalidData("archive source changed"))?;
        if text.len() != size || Sha256::digest(text.as_bytes()).as_slice() != archive.sha256 {
            return Err(PersistenceError::InvalidData(
                "archive source digest changed",
            ));
        }
        let changed = sqlx::query(
            "UPDATE records SET payload_json = NULL, archive_zlib = ?, archive_decoded_bytes = ?, \
             archive_sha256 = ?, archive_kind = ?, archive_input_images = input_images WHERE id = ? AND session_id = ? AND agent_id = ? \
             AND payload_json = ?",
        )
        .bind(&archive.zlib)
        .bind(source.bytes)
        .bind(archive.sha256.as_slice())
        .bind(archive.kind)
        .bind(source.id)
        .bind(i64::from(store.attachment.session_id))
        .bind(i64::from(store.agent))
        .bind(text)
        .execute(&mut *connection)
        .await?;
        if changed.rows_affected() != 1 {
            return Err(PersistenceError::InvalidData("archive source changed"));
        }
    }
    Ok(())
}

/// Bounded payload columns for queries that already preflight inline byte sizes.
pub(super) const PAYLOAD_COLUMNS: &str = "id, payload_json, CASE WHEN length(archive_zlib) BETWEEN 1 AND 8388608 THEN archive_zlib END AS archive_zlib, \
     archive_decoded_bytes, CASE WHEN length(archive_sha256) = 32 THEN archive_sha256 END AS archive_sha256, \
     CASE WHEN octet_length(archive_kind) <= 32 THEN archive_kind END AS archive_kind, archive_input_images";

pub(super) struct ArchivedPayload {
    source: SourceMetadata,
    zlib: Vec<u8>,
    sha256: [u8; 32],
    kind: &'static str,
    _payload: Arc<OwnedSemaphorePermit>,
}

#[derive(FromRow)]
pub(super) struct PayloadRow {
    pub(super) id: i64,
    payload_json: Option<String>,
    archive_zlib: Option<Vec<u8>>,
    archive_decoded_bytes: Option<i64>,
    archive_sha256: Option<Vec<u8>>,
    archive_kind: Option<String>,
    archive_input_images: Option<i64>,
}

impl PayloadRow {
    /// Decode the real original with the preflighted source size. Archives require
    /// one complete bounded zlib stream, exact length, SHA-256 and matching kind.
    ///
    /// # Errors
    /// Reject mixed/missing representations, changed sizes and corrupt evidence.
    pub(super) fn decode(self, bytes: i64) -> Result<RecordKind, PersistenceError> {
        match (
            self.payload_json,
            self.archive_zlib,
            self.archive_decoded_bytes,
            self.archive_sha256,
            self.archive_kind,
            self.archive_input_images,
        ) {
            (Some(text), None, None, None, None, None) if text.len() as i64 == bytes => {
                serde_json::from_str(&text)
                    .map_err(|_| PersistenceError::InvalidData("record JSON"))
            }
            (None, Some(zlib), Some(decoded), Some(sha256), Some(kind), Some(images))
                if decoded == bytes =>
            {
                let text = decode_archive(&zlib, decoded, &sha256)?;
                let record: RecordKind = serde_json::from_slice(&text)
                    .map_err(|_| PersistenceError::InvalidData("archived record JSON"))?;
                if kind_name(&record) != kind
                    || i64::try_from(input_images(&record)).ok() != Some(images)
                {
                    return Err(PersistenceError::InvalidData("archive kind/image count"));
                }
                Ok(record)
            }
            _ => Err(PersistenceError::InvalidData(
                "record payload representation/size",
            )),
        }
    }
}

const ARCHIVE_BYTES_MAX: usize = 8 * 1024 * 1024;
const RECORDS_MAX: usize = 64;
// Reserve the digest, decoded length, image count and worst-case column serial types.
const ARCHIVE_METADATA_BYTES: usize = 32 + 8 + 8 + 6 * 9;
const CURSORS_MAX: usize = 1024;
static CURSORS: Mutex<Vec<ArchiveCursor>> = Mutex::new(Vec::new());

struct ArchiveCursor {
    attachment: Weak<super::AttachedSession>,
    agent: AgentId,
    sequence: i64,
}

#[derive(FromRow)]
struct SourceMetadata {
    id: i64,
    sequence: i64,
    invocation: i64,
    runtime_epoch: i64,
    created_at_ms: i64,
    payload_version: i64,
    bytes: i64,
}

async fn candidates(
    connection: &mut SqliteConnection,
    store: &SqliteStore,
    through: i64,
    after: i64,
) -> Result<Vec<SourceMetadata>, PersistenceError> {
    // Literal size bounds let SQLite prove the partial index is applicable.
    Ok(sqlx::query_as(
        "SELECT id, sequence, invocation, runtime_epoch, created_at_ms, payload_version, \
         octet_length(payload_json) AS bytes FROM records INDEXED BY records_archive_candidates \
         WHERE session_id = ? AND agent_id = ? AND sequence > ? AND sequence <= ? \
         AND payload_json IS NOT NULL AND octet_length(payload_json) BETWEEN 103 AND 8388608 ORDER BY sequence LIMIT 64",
    )
    .bind(i64::from(store.attachment.session_id))
    .bind(i64::from(store.agent))
    .bind(after)
    .bind(through)
    .fetch_all(connection)
    .await?)
}

fn compress(
    rows: Vec<(i64, String)>,
    metadata: Vec<SourceMetadata>,
    payload: Arc<OwnedSemaphorePermit>,
) -> Result<Vec<ArchivedPayload>, PersistenceError> {
    if rows.len() != metadata.len() {
        return Err(PersistenceError::InvalidData("archive source page changed"));
    }
    let mut prepared = Vec::with_capacity(rows.len());
    for ((id, text), source) in rows.into_iter().zip(metadata) {
        if id != source.id || text.len() as i64 != source.bytes {
            return Err(PersistenceError::InvalidData(
                "archive source identity/size",
            ));
        }
        let record = Record {
            invocation: InvocationId(source.invocation as u64),
            kind: serde_json::from_str(&text)
                .map_err(|_| PersistenceError::InvalidData("archive source JSON"))?,
        };
        record.validate()?;
        if encoded_size(&record)?
            != text.len() + source.invocation.to_string().len() + RECORD_ENVELOPE_BYTES
        {
            return Err(PersistenceError::InvalidData("noncanonical archive source"));
        }
        let kind = kind_name(&record.kind);
        let zlib = compress_to_vec_zlib(text.as_bytes(), 6);
        if zlib.len() > ARCHIVE_BYTES_MAX
            || zlib.len() + ARCHIVE_METADATA_BYTES + kind.len() >= text.len()
        {
            continue;
        }
        let sha256: [u8; 32] = Sha256::digest(text.as_bytes()).into();
        if decode_archive(&zlib, source.bytes, &sha256)? != text.as_bytes() {
            return Err(PersistenceError::InvalidData("archive roundtrip"));
        }
        prepared.push(ArchivedPayload {
            source,
            zlib,
            sha256,
            kind,
            _payload: Arc::clone(&payload),
        });
    }
    Ok(prepared)
}

fn decode_archive(zlib: &[u8], decoded: i64, sha256: &[u8]) -> Result<Vec<u8>, PersistenceError> {
    let decoded = usize::try_from(decoded)
        .ok()
        .filter(|bytes| (1..=ARCHIVE_BYTES_MAX).contains(bytes))
        .ok_or(PersistenceError::InvalidData("archive decoded size"))?;
    if !(1..=ARCHIVE_BYTES_MAX).contains(&zlib.len()) || sha256.len() != 32 {
        return Err(PersistenceError::InvalidData("archive encoded size/digest"));
    }
    let mut output = vec![0; decoded];
    let (status, consumed, written) = decompress(
        &mut DecompressorOxide::new(),
        zlib,
        &mut output,
        0,
        inflate_flags::TINFL_FLAG_PARSE_ZLIB_HEADER
            | inflate_flags::TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF,
    );
    if status != TINFLStatus::Done
        || consumed != zlib.len()
        || written != decoded
        || Sha256::digest(&output).as_slice() != sha256
    {
        return Err(PersistenceError::InvalidData("archive integrity"));
    }
    Ok(output)
}

fn kind_name(kind: &RecordKind) -> &'static str {
    match kind {
        RecordKind::InputAccepted { .. } => "InputAccepted",
        RecordKind::ModelCompleted { .. } => "ModelCompleted",
        RecordKind::CellStarted { .. } => "CellStarted",
        RecordKind::CellCompleted { .. } => "CellCompleted",
        RecordKind::OperationStarted { .. } => "OperationStarted",
        RecordKind::OperationFinished { .. } => "OperationFinished",
        RecordKind::InvocationFinished { .. } => "InvocationFinished",
        RecordKind::TurnCancelled { .. } => "TurnCancelled",
    }
}

fn input_images(kind: &RecordKind) -> usize {
    match kind {
        RecordKind::InputAccepted { input } => input.images().len(),
        _ => 0,
    }
}
