//! Each write owns its transaction; helpers only borrow that connection.

use std::{
    collections::BTreeSet,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use cocoa_core::{
    context::InstructionState,
    model::UserInput,
    store::{AgentId, Record, RecordId, RecordKind, StoreError},
};
use sqlx::{Connection, FromRow, SqliteConnection};
use unicode_segmentation::UnicodeSegmentation;

use super::{
    Database, EffectiveSessionSettings, FORMAT_VERSION, InstructionPolicy, PersistenceError,
    RECORD_VERSION, SessionId, SessionLease, SessionPublicId, SqliteStore, StoredSessionSettings,
    WAIT, reads, sessions,
};

pub(super) async fn create(
    database: &Database,
    directory: &str,
    public_id: &SessionPublicId,
    lease: &Arc<SessionLease>,
    fork: Option<&super::forks::ForkPrefix>,
) -> Result<Option<CreatedIdentity>, PersistenceError> {
    let mut writer = tokio::time::timeout(WAIT, database.writer.lock())
        .await
        .map_err(|_| PersistenceError::Busy)?;
    writer.settle().await?;
    writer.attachment = Some(Arc::clone(lease));
    let result = async {
        let mut transaction = writer.connection()?.begin_with("BEGIN IMMEDIATE").await?;
        let created = async {
            let created = create_transaction(&mut transaction, directory, public_id).await?;
            if let (Some(created), Some(fork)) = (&created, fork) {
                super::forks::insert(&mut transaction, created.session, created.root_agent, fork).await?;
            }
            Ok(created)
        }.await;
        match created {
            Ok(Some(created)) => match transaction.commit().await {
                Ok(()) => Ok(Some(created)),
                Err(error) => {
                    tracing::error!(%error, "SQLite session creation commit needs reconciliation");
                    Err(PersistenceError::OutcomeUnknown)
                }
            },
            result => match transaction.rollback().await {
                Ok(()) => result,
                Err(error) => {
                    tracing::error!(%error, "SQLite session creation rollback needs reconciliation");
                    Err(PersistenceError::OutcomeUnknown)
                }
            },
        }
    }
    .await;
    if let Err(error) = writer.settle().await {
        tracing::error!(%error, "SQLite session creation worker did not settle");
        return Err(PersistenceError::OutcomeUnknown);
    }
    result
}

pub(super) fn encode(
    record: Record,
    instruction_state: Option<InstructionState>,
) -> Result<EncodedAppend, PersistenceError> {
    let payload_json = serde_json::to_string(&record.kind).map_err(|_| StoreError::Encoding)?;
    // Serialization accepts deeper JSON than deserialization. Check restoration
    // before a successful append could make the retained history unreadable.
    let restored: RecordKind =
        serde_json::from_str(&payload_json).map_err(|_| StoreError::Encoding)?;
    if restored != record.kind {
        return Err(StoreError::Encoding.into());
    }
    drop(restored);
    let state_json = instruction_state
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|_| StoreError::Encoding)?;
    let targets: BTreeSet<_> = instruction_state
        .iter()
        .flat_map(|state| state.scoped_feedback.iter().chain(&state.skill_feedback))
        .map(|feedback| i64::from(feedback.record))
        .collect();
    let targets = targets.into_iter().collect();
    let title = match &record.kind {
        RecordKind::InputAccepted { input } => Some(initial_title(input)),
        _ => None,
    };
    Ok(EncodedAppend {
        record,
        payload_json,
        instruction_state,
        state_json,
        targets,
        title,
    })
}

pub(super) async fn append(
    store: &SqliteStore,
    encoded: EncodedAppend,
) -> Result<RecordId, PersistenceError> {
    let mut writer = tokio::time::timeout(WAIT, store.attachment.database.writer.lock())
        .await
        .map_err(|_| PersistenceError::Busy)?;
    writer.settle().await?;
    writer.attachment = Some(Arc::clone(&store.attachment.lease));
    let result = async {
        let mut transaction = writer.connection()?.begin_with("BEGIN IMMEDIATE").await?;
        match append_transaction(&mut transaction, store, &encoded).await {
            Ok(id) => match transaction.commit().await {
                Ok(()) => Ok(id),
                Err(error) => {
                    tracing::error!(%error, "SQLite append commit outcome needs reconciliation");
                    Err(PersistenceError::OutcomeUnknown)
                }
            },
            Err(error) => match transaction.rollback().await {
                Ok(()) => Err(error),
                Err(rollback) => {
                    tracing::error!(%rollback, "SQLite append rollback needs reconciliation");
                    Err(PersistenceError::OutcomeUnknown)
                }
            },
        }
    }
    .await;
    if let Err(error) = writer.settle().await {
        tracing::error!(%error, "SQLite append worker did not settle");
        return Err(PersistenceError::OutcomeUnknown);
    }
    result
}

pub(super) fn timestamp() -> Result<i64, PersistenceError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .ok_or(PersistenceError::InvalidData(
            "wall clock outside persisted timestamp range",
        ))
}

pub(super) struct EncodedAppend {
    record: Record,
    payload_json: String,
    instruction_state: Option<InstructionState>,
    state_json: Option<String>,
    targets: Vec<i64>,
    title: Option<String>,
}

pub(super) struct CreatedIdentity {
    pub(super) session: SessionId,
    pub(super) root_agent: AgentId,
}

#[derive(FromRow)]
struct IdentityTops {
    session: i64,
    agent: i64,
}

async fn create_transaction(
    connection: &mut SqliteConnection,
    directory: &str,
    public_id: &SessionPublicId,
) -> Result<Option<CreatedIdentity>, PersistenceError> {
    let tops = sqlx::query_as::<_, IdentityTops>(
        "SELECT coalesce((SELECT max(id) FROM sessions), 0) AS session, \
         coalesce((SELECT max(id) FROM agents), 0) AS agent",
    )
    .fetch_one(&mut *connection)
    .await?;
    // SQLite otherwise switches to random row IDs after reaching i64::MAX.
    let session = tops
        .session
        .checked_add(1)
        .ok_or(StoreError::IdentityExhausted)?;
    let root_agent = tops
        .agent
        .checked_add(1)
        .ok_or(StoreError::IdentityExhausted)?;
    let session = SessionId::try_from(session)
        .map_err(|_| PersistenceError::InvalidData("allocated session identity"))?;
    let root_agent = AgentId::try_from(root_agent)
        .map_err(|_| PersistenceError::InvalidData("allocated root identity"))?;
    let now = timestamp()?;
    let inserted = sqlx::query(
        "INSERT INTO sessions (id, public_id, working_directory, created_at_ms, updated_at_ms, \
         last_opened_at_ms, runtime_epoch, settings_version, settings_json, accounting_version) \
         VALUES (?, ?, ?, ?, ?, ?, 1, 1, '{\"state\":\"unconfigured\"}', 1) \
         ON CONFLICT(public_id) DO NOTHING",
    )
    .bind(i64::from(session))
    .bind(public_id.as_str())
    .bind(directory)
    .bind(now)
    .bind(now)
    .bind(now)
    .execute(&mut *connection)
    .await?;
    if inserted.rows_affected() == 0 {
        return Ok(None);
    }
    let inserted = sqlx::query(
        "INSERT INTO agents (id, session_id, name, parent_agent_id, format_version, lifecycle) \
         VALUES (?, ?, 'root', NULL, ?, 'participating')",
    )
    .bind(i64::from(root_agent))
    .bind(i64::from(session))
    .bind(FORMAT_VERSION)
    .execute(connection)
    .await?;
    if inserted.rows_affected() != 1 {
        return Err(PersistenceError::InvalidData(
            "root insertion did not create one agent",
        ));
    }
    Ok(Some(CreatedIdentity {
        session,
        root_agent,
    }))
}

async fn bind_settings(
    connection: &mut SqliteConnection,
    store: &SqliteStore,
    effective: &EffectiveSessionSettings,
) -> Result<(), PersistenceError> {
    let settings = StoredSessionSettings::Bound {
        effective: Box::new(effective.clone()),
    };
    sqlx::query("UPDATE sessions SET settings_version = ?, settings_json = ? WHERE id = ? AND runtime_epoch = ?")
        .bind(FORMAT_VERSION)
        .bind(serde_json::to_string(&settings).map_err(|_| StoreError::Encoding)?)
        .bind(i64::from(store.attachment.session_id))
        .bind(store.attachment.epoch)
        .execute(&mut *connection)
        .await?;
    Ok(())
}

async fn append_transaction(
    connection: &mut SqliteConnection,
    store: &SqliteStore,
    encoded: &EncodedAppend,
) -> Result<RecordId, PersistenceError> {
    reads::writable(connection, store).await?;
    if store.agent == store.attachment.root_agent
        && matches!(
            encoded.record.kind,
            RecordKind::InputAccepted { .. } | RecordKind::ModelCompleted { .. }
        )
        && let Some(settings) = &store.settings
    {
        let effective = settings
            .lock()
            .map_err(|_| PersistenceError::SettingsMismatch)?
            .clone();
        match sessions::settings(connection, store.attachment.session_id).await? {
            StoredSessionSettings::Unconfigured => {
                let previous: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM records WHERE session_id = ? AND agent_id = ?",
                )
                .bind(i64::from(store.attachment.session_id))
                .bind(i64::from(store.agent))
                .fetch_one(&mut *connection)
                .await?;
                if previous != 0 {
                    return Err(PersistenceError::InvalidData("unconfigured root history"));
                }
                bind_settings(connection, store, &effective).await?;
            }
            StoredSessionSettings::Bound {
                effective: previous,
            } if previous.as_ref() == &effective => {}
            StoredSessionSettings::Bound {
                effective: previous,
            } if previous.can_resolve_limits_to(&effective) => {
                bind_settings(connection, store, &effective).await?;
            }
            // A request that failed before any model output pinned nothing, so
            // the next accepted input may adopt the configuration now supplied.
            StoredSessionSettings::Bound { .. }
                if !reads::holds_model_output(
                    connection,
                    store.attachment.session_id,
                    store.agent,
                )
                .await? =>
            {
                bind_settings(connection, store, &effective).await?;
            }
            StoredSessionSettings::Bound { .. } => return Err(PersistenceError::SettingsMismatch),
        }
    }
    let stats = reads::append_stats(connection, store).await?;
    let previous = reads::state(connection, store).await?;
    if matches!(encoded.record.kind, RecordKind::ModelCompleted { .. })
        && (store.instructions == InstructionPolicy::Required || previous.is_some())
        && encoded.instruction_state.is_none()
    {
        return Err(StoreError::InstructionStateRequired.into());
    }
    if let Some(candidate) = &encoded.instruction_state {
        let targets = reads::feedback_targets(connection, store, &encoded.targets).await?;
        candidate.validate_transition(&encoded.record, previous.as_ref(), &targets)?;
    }
    let sequence = stats
        .sequence
        .checked_add(1)
        .ok_or(StoreError::IdentityExhausted)?;
    let now = timestamp()?;
    let maximum: i64 = sqlx::query_scalar(
        "SELECT max(coalesce((SELECT max(id) FROM records), 0), coalesce((SELECT max(id) FROM checkpoints), 0))",
    )
        .fetch_one(&mut *connection)
        .await?;
    let id = maximum
        .checked_add(1)
        .ok_or(StoreError::IdentityExhausted)?;
    sqlx::query(
        "INSERT INTO records (id, session_id, agent_id, sequence, invocation, runtime_epoch, created_at_ms, payload_version, payload_json) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    ).bind(id).bind(i64::from(store.attachment.session_id)).bind(i64::from(store.agent)).bind(sequence).bind(encoded.record.invocation.0 as i64)
        .bind(store.attachment.epoch).bind(now).bind(RECORD_VERSION).bind(&encoded.payload_json).execute(&mut *connection).await?;
    if let Some(state_json) = &encoded.state_json {
        sqlx::query(
            "INSERT INTO instruction_state (agent_id, format_version, state_json) VALUES (?, ?, ?) \
             ON CONFLICT(agent_id) DO UPDATE SET format_version = excluded.format_version, state_json = excluded.state_json",
        ).bind(i64::from(store.agent)).bind(FORMAT_VERSION).bind(state_json).execute(&mut *connection).await?;
    }
    let updated = sqlx::query(
        "UPDATE sessions SET updated_at_ms = max(updated_at_ms, ?), title = coalesce(title, \
         (SELECT ? FROM agents WHERE id = ? AND session_id = sessions.id AND parent_agent_id IS NULL)) \
         WHERE id = ? AND runtime_epoch = ?",
    ).bind(now).bind(&encoded.title).bind(i64::from(store.agent)).bind(i64::from(store.attachment.session_id)).bind(store.attachment.epoch)
        .execute(connection).await?;
    if updated.rows_affected() != 1 {
        return Err(PersistenceError::AgentUnavailable);
    }
    RecordId::try_from(id).map_err(|_| PersistenceError::InvalidData("allocated record identity"))
}

fn initial_title(input: &UserInput) -> String {
    let mut normalized = String::with_capacity(512);
    let mut space = false;
    let mut shortened = false;
    for character in input.text().chars() {
        if character.is_whitespace() {
            space = !normalized.is_empty();
        } else if !character.is_control() {
            if space {
                normalized.push(' ');
            }
            normalized.push(character);
            space = false;
            if normalized.len() > 512 {
                shortened = true;
                break;
            }
        }
    }
    if normalized.is_empty() {
        return if input.images().is_empty() {
            "Untitled session"
        } else {
            "Image prompt"
        }
        .into();
    }
    shortened |= normalized.graphemes(true).take(81).count() > 80;
    if !shortened {
        return normalized;
    }
    let end = normalized
        .grapheme_indices(true)
        .take(79)
        .take_while(|(index, grapheme)| index + grapheme.len() <= 509)
        .map(|(index, grapheme)| index + grapheme.len())
        .last()
        .unwrap_or(0);
    normalized.truncate(end);
    normalized.push('…');
    normalized
}
