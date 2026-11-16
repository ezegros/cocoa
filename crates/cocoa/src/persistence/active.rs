//! Indexed running-engine reads. Unknown publications require full restoration.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use cocoa_core::{
    compaction::StoredCheckpoint,
    store::{ActiveSnapshot, RecordId, StoreError},
};
use sqlx::{Connection, FromRow, QueryBuilder, Sqlite, SqliteConnection};

use super::{PersistenceError, SqliteStore, WAIT, compaction, reads};

#[derive(Clone)]
pub(super) struct CachedHead {
    pub(super) stored: StoredCheckpoint,
    pub(super) sequence: i64,
    pub(super) encoded: String,
}

pub(super) async fn load(store: &SqliteStore) -> Result<ActiveSnapshot, PersistenceError> {
    let cached = store.checkpoint_cache.lock().await.clone();
    let mut reader = tokio::time::timeout(WAIT, store.attachment.database.reader.lock())
        .await
        .map_err(|_| PersistenceError::Busy)?;
    reader.settle().await?;
    reader.attachment = Some(Arc::clone(&store.attachment.lease));
    let mut transaction = reader.connection()?.begin().await?;
    let result = async {
        reads::authority(&mut transaction, store).await?;
        let head: Option<Head> = sqlx::query_as(
            "SELECT id, sequence, after_record_id, through_record_id, format_version, octet_length(payload_json) AS bytes \
             FROM checkpoints WHERE session_id = ? AND agent_id = ? ORDER BY sequence DESC LIMIT 1",
        ).bind(i64::from(store.attachment.session_id)).bind(i64::from(store.agent))
            .fetch_optional(&mut *transaction).await?;
        let checkpoint = match (head, cached) {
            (None, None) => None,
            (None, Some(_)) => return Err(PersistenceError::InvalidData("checkpoint head disappeared")),
            (Some(head), cached) => {
                if head.format_version != 1 || !(1..=compaction::CHECKPOINT_BYTES_MAX as i64).contains(&head.bytes) {
                    return Err(PersistenceError::InvalidData("checkpoint head version/size"));
                }
                let Some(cached) = cached.filter(|cached| {
                    i64::from(cached.stored.id) == head.id && cached.sequence == head.sequence
                        && i64::from(cached.stored.checkpoint.after()) == head.after_record_id
                        && i64::from(cached.stored.checkpoint.content().through) == head.through_record_id
                }) else {
                    return Ok(None);
                };
                let _payload = store.attachment.database.payload(head.bytes as usize)?;
                let encoded: String = sqlx::query_scalar("SELECT payload_json FROM checkpoints WHERE id = ? AND session_id = ? AND agent_id = ?")
                    .bind(head.id).bind(i64::from(store.attachment.session_id)).bind(i64::from(store.agent))
                    .fetch_one(&mut *transaction).await?;
                if encoded != cached.encoded {
                    return Ok(None);
                }
                Some(cached.stored)
            }
        };
        select(&mut transaction, store, checkpoint).await.map(Some)
    }.await;
    let active = match result {
        Ok(active) => {
            transaction.commit().await?;
            reader.attachment = None;
            active
        }
        Err(error) => {
            transaction.rollback().await?;
            reader.attachment = None;
            return Err(error);
        }
    };
    drop(reader);
    match active {
        Some(active) => Ok(active),
        None => Ok(reads::load(store).await?.into_active()?),
    }
}

#[derive(FromRow)]
struct Head {
    id: i64,
    sequence: i64,
    after_record_id: i64,
    through_record_id: i64,
    format_version: i64,
    bytes: i64,
}

async fn select(
    connection: &mut SqliteConnection,
    store: &SqliteStore,
    checkpoint: Option<StoredCheckpoint>,
) -> Result<ActiveSnapshot, PersistenceError> {
    let tail: Option<(i64, i64, i64)> = sqlx::query_as(
        "SELECT id, sequence, invocation FROM records WHERE session_id = ? AND agent_id = ? ORDER BY sequence DESC LIMIT 1",
    ).bind(i64::from(store.attachment.session_id)).bind(i64::from(store.agent)).fetch_optional(&mut *connection).await?;
    let cut: i64 = match &checkpoint {
        Some(stored) => sqlx::query_scalar(
            "SELECT sequence FROM records WHERE id = ? AND session_id = ? AND agent_id = ?",
        )
        .bind(i64::from(stored.checkpoint.content().through))
        .bind(i64::from(store.attachment.session_id))
        .bind(i64::from(store.agent))
        .fetch_optional(&mut *connection)
        .await?
        .ok_or(StoreError::InvalidCheckpoint)?,
        None => 0,
    };
    let state = reads::state(connection, store).await?;
    let mut required = BTreeSet::<RecordId>::new();
    if let Some(stored) = &checkpoint {
        required.extend(stored.checkpoint.source_ids()?);
        required.insert(stored.checkpoint.after());
    }
    if let Some(state) = &state {
        required.extend(
            state
                .scoped_feedback
                .iter()
                .chain(&state.skill_feedback)
                .map(|note| note.record),
        );
    }
    let mut metadata = BTreeMap::new();
    let mut bytes = 0_usize;
    let mut cursor = cut;
    let mut suffix_invocation = None;
    loop {
        let mut query = metadata_query(store, MetadataAccess::Sequence);
        query
            .push(" AND sequence > ")
            .push_bind(cursor)
            .push(" ORDER BY sequence LIMIT 64");
        let rows: Vec<reads::RecordMetadata> =
            query.build_query_as().fetch_all(&mut *connection).await?;
        if rows.is_empty() {
            break;
        }
        if suffix_invocation.is_none() {
            suffix_invocation = rows.first().map(|row| row.invocation);
        }
        for row in rows {
            cursor = row.sequence;
            include(&mut metadata, &mut bytes, row, store)?;
        }
    }
    for sources in required.into_iter().collect::<Vec<_>>().chunks(64) {
        let mut query = metadata_query(store, MetadataAccess::Identity);
        query.push(" AND id IN (");
        {
            let mut ids = query.separated(",");
            for source in sources {
                ids.push_bind(i64::from(*source));
            }
        }
        query.push(") ORDER BY sequence");
        let rows = query
            .build_query_as::<reads::RecordMetadata>()
            .fetch_all(&mut *connection)
            .await?;
        if rows.len() != sources.len() {
            return Err(StoreError::InvalidCheckpoint.into());
        }
        for row in rows {
            include(&mut metadata, &mut bytes, row, store)?;
        }
    }
    if checkpoint.is_some() {
        for images in [false, true] {
            let mut cursor = 0_i64;
            loop {
                let mut query = metadata_query(store, MetadataAccess::Sequence);
                query
                    .push(" AND sequence > ")
                    .push_bind(cursor)
                    .push(" AND sequence <= ")
                    .push_bind(cut);
                if images {
                    query.push(" AND input_images > 0");
                } else {
                    query
                        .push(" AND is_input = 1 AND invocation IN (")
                        .push_bind(tail.map_or(0, |tail| tail.2))
                        .push(",")
                        .push_bind(suffix_invocation.unwrap_or(0))
                        .push(")");
                }
                query.push(" ORDER BY sequence LIMIT 64");
                let rows = query
                    .build_query_as::<reads::RecordMetadata>()
                    .fetch_all(&mut *connection)
                    .await?;
                if rows.is_empty() {
                    break;
                }
                for row in rows {
                    cursor = row.sequence;
                    include(&mut metadata, &mut bytes, row, store)?;
                }
            }
        }
    }
    let _payload = store.attachment.database.payload(bytes)?;
    let records = reads::payload_pages(connection, store, metadata.into_values().collect()).await?;
    if records.last().map(|record| i64::from(record.id)) != tail.map(|tail| tail.0)
        || records.last().map(|record| i64::from(record.sequence)) != tail.map(|tail| tail.1)
    {
        return Err(PersistenceError::InvalidData("active history tail"));
    }
    Ok(ActiveSnapshot::new(records, state, checkpoint)?)
}

enum MetadataAccess {
    Sequence,
    Identity,
}

fn metadata_query(store: &SqliteStore, access: MetadataAccess) -> QueryBuilder<Sqlite> {
    let mut query = QueryBuilder::new(
        "SELECT id, sequence, invocation, runtime_epoch, payload_version, coalesce(octet_length(payload_json), archive_decoded_bytes) AS payload_bytes FROM records",
    );
    if matches!(access, MetadataAccess::Identity) {
        query.push(" NOT INDEXED");
    }
    query
        .push(" WHERE session_id = ")
        .push_bind(i64::from(store.attachment.session_id))
        .push(" AND agent_id = ")
        .push_bind(i64::from(store.agent));
    query
}

fn include(
    selected: &mut BTreeMap<i64, reads::RecordMetadata>,
    bytes: &mut usize,
    record: reads::RecordMetadata,
    store: &SqliteStore,
) -> Result<(), PersistenceError> {
    if selected.contains_key(&record.sequence) {
        return Ok(());
    }
    *bytes = bytes
        .checked_add(reads::metadata_bytes(&record, store)?)
        .ok_or(StoreError::Capacity)?;
    if *bytes > super::PENDING_BYTES {
        return Err(StoreError::Capacity.into());
    }
    selected.insert(record.sequence, record);
    Ok(())
}
