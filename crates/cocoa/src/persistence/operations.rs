//! Short local mutation transactions with exact operation-start receipts.

use std::{collections::BTreeSet, path::Path, sync::Arc};

use cocoa_core::{
    capability::OperationOutcome,
    context::{ContextContent, InstructionState},
    execution::host_value_fits,
    store::{AgentId, Record, RecordId, RecordKind, StoreError, StoreLimits, encoded_size},
};
use serde_json::json;
use sqlx::{Connection, SqliteConnection};

use super::{
    ASSIGNMENT_JSON_BYTES_MAX, AgentAssignment, AgentMembership, AgentOutcome, AppendCompletion,
    AttachmentState, ContextRecordId, FORMAT_VERSION, PersistenceError, SessionAttachment,
    SqliteStore, StoredContextRecord, WAIT, reads, sessions, writes,
};

/// Reprocessing a retained admission must not construct another child runtime.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentAdmission {
    pub agent: AgentId,
    pub outcome: OperationOutcome,
    pub created: bool,
}

impl SqliteStore {
    /// Commit a child's identity, assignment and exact receipt before setup.
    /// `created == false` forbids restarting work for a repeated operation.
    /// Assignment JSON must fit 8 MiB + 1 KiB, including escaped text.
    ///
    /// # Errors
    /// Reject caller/start mismatches, conflicting retries, bounds or storage errors.
    pub async fn admit_agent(
        &self,
        operation_record: RecordId,
        name: &str,
        assignment: &AgentAssignment,
        output_bytes: usize,
    ) -> Result<AgentAdmission, PersistenceError> {
        if name.is_empty()
            || name.len() > 64
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
            || assignment.prompt.is_empty()
            || assignment.prompt.len() > 8 * 1024 * 1024
            || assignment.alias.is_empty()
            || assignment.alias.len() > 128
        {
            return Err(PersistenceError::InvalidData("child assignment"));
        }
        let assignment_bytes = encoded_size(assignment)?;
        if assignment_bytes > ASSIGNMENT_JSON_BYTES_MAX {
            return Err(StoreError::RecordLimit.into());
        }
        let bytes = assignment_bytes + name.len();
        match self
            .mutate(
                Mutation::Admit {
                    start: operation_record,
                    name: name.into(),
                    assignment: assignment.clone(),
                    output_bytes,
                },
                bytes,
            )
            .await?
        {
            MutationResult::Admission(admission) => Ok(admission),
            _ => Err(PersistenceError::InvalidData("admission result")),
        }
    }

    /// Replace current context only as its permanent owner. Receipt and content
    /// commit together, and the complete public result must fit before acceptance.
    ///
    /// # Errors
    /// Reject foreign/mismatched starts, ownership, bounds or storage failures.
    pub async fn set_context(
        &self,
        operation_record: RecordId,
        key: &str,
        content: &ContextContent,
        output_bytes: usize,
    ) -> Result<OperationOutcome, PersistenceError> {
        sessions::validate_context(key, content)?;
        let bytes = encoded_size(content)? + key.len();
        if bytes > 16384 {
            return Err(StoreError::RecordLimit.into());
        }
        match self
            .mutate(
                Mutation::Context {
                    start: operation_record,
                    key: key.into(),
                    content: content.clone(),
                    output_bytes,
                },
                bytes,
            )
            .await?
        {
            MutationResult::Outcome(outcome) => Ok(outcome),
            _ => Err(PersistenceError::InvalidData("context result")),
        }
    }

    /// Commit pending instruction gates and their exact local operation result.
    /// A repeated start must carry the same outcome and accepted candidate.
    ///
    /// # Errors
    /// Reject malformed transitions, invalid starts, conflicts or capacity errors.
    pub async fn accept_instruction_change(
        &self,
        operation_record: RecordId,
        state: &InstructionState,
        outcome: &OperationOutcome,
    ) -> Result<(), PersistenceError> {
        state.validate_bounds(usize::MAX)?;
        let bytes = encoded_size(state)?
            .checked_add(encoded_size(outcome)?)
            .ok_or(StoreError::Capacity)?;
        self.mutate(
            Mutation::Instructions {
                start: operation_record,
                state: state.clone(),
                outcome: outcome.clone(),
            },
            bytes,
        )
        .await?;
        Ok(())
    }

    /// Inspect a known local result through a host-issued same-agent start ID.
    /// This does not repeat work or clear an uncertain attachment's fence.
    ///
    /// # Errors
    /// Reject foreign/non-start records, unsupported receipts, exhausted
    /// admission/lock waits or storage failures.
    pub async fn operation_receipt(
        &self,
        operation_record: RecordId,
    ) -> Result<Option<OperationOutcome>, PersistenceError> {
        let _request = self.attachment.database.request()?;
        let _operation = tokio::time::timeout(WAIT, self.attachment.operation.lock())
            .await
            .map_err(|_| PersistenceError::Busy)?;
        let mut reader = tokio::time::timeout(WAIT, self.attachment.database.reader.lock())
            .await
            .map_err(|_| PersistenceError::Busy)?;
        reader.settle().await?;
        let mut transaction = reader.connection()?.begin().await?;
        operation_start(&mut transaction, self, operation_record).await?;
        let outcome = receipt(&mut transaction, operation_record).await?;
        transaction.commit().await?;
        Ok(outcome)
    }

    async fn mutate(
        &self,
        mutation: Mutation,
        bytes: usize,
    ) -> Result<MutationResult, PersistenceError> {
        let request = self.attachment.database.request()?;
        let payload = self.attachment.database.payload(bytes)?;
        let mut guard =
            tokio::time::timeout(WAIT, Arc::clone(&self.attachment.operation).lock_owned())
                .await
                .map_err(|_| PersistenceError::Busy)?;
        if matches!(*guard, AttachmentState::Uncertain { .. }) {
            return Err(PersistenceError::OutcomeUnknown);
        }
        *guard = AttachmentState::Uncertain { agent: self.agent };
        let store = self.clone();
        let mut completion = tokio::spawn(async move {
            let result = write(&store, mutation).await;
            AppendCompletion {
                known: !matches!(result, Err(PersistenceError::OutcomeUnknown)),
                result,
                operation: guard,
                _attachment: Arc::clone(&store.attachment),
                _request: request,
                _payload: payload,
            }
        })
        .await?;
        if completion.known {
            *completion.operation = AttachmentState::Ready;
        }
        completion.result
    }
}

impl SessionAttachment {
    /// Retain the exact child result before publishing it to waiters. Completion
    /// never retires the identity or transfers its context ownership.
    ///
    /// # Errors
    /// Reject roots/foreign agents, conflicting outcomes or storage failures.
    pub async fn complete_agent(
        &self,
        agent: AgentId,
        outcome: &AgentOutcome,
    ) -> Result<(), PersistenceError> {
        let bytes = encoded_size(outcome)?;
        if bytes > 1024 * 1024 {
            return Err(StoreError::RecordLimit.into());
        }
        self.mutation_store()?
            .mutate(
                Mutation::Complete {
                    agent,
                    outcome: outcome.clone(),
                },
                bytes,
            )
            .await?;
        Ok(())
    }

    /// Explicitly relocate a saved project after canonicalizing its new directory.
    /// The caller must invalidate environment authority before executing new work.
    ///
    /// # Errors
    /// Reject nonexistent/non-directory paths, stale attachment or storage errors.
    pub async fn relocate(&self, directory: &Path) -> Result<(), PersistenceError> {
        let directory = directory.to_owned();
        let directory = tokio::task::spawn_blocking(move || {
            let directory = directory.canonicalize()?;
            if !directory.is_dir() {
                return Err(PersistenceError::InvalidData("relocation directory"));
            }
            super::session_directory(&directory)?;
            Ok::<_, PersistenceError>(directory)
        })
        .await??;
        let directory = directory
            .to_str()
            .ok_or(PersistenceError::InvalidData("relocation UTF-8"))?
            .to_owned();
        let bytes = directory.len();
        self.mutation_store()?
            .mutate(Mutation::Relocate { directory }, bytes)
            .await?;
        Ok(())
    }

    fn mutation_store(&self) -> Result<SqliteStore, PersistenceError> {
        self.root_store(StoreLimits {
            record_bytes: 16 * 1024 * 1024,
        })
    }
}

enum Mutation {
    Admit {
        start: RecordId,
        name: String,
        assignment: AgentAssignment,
        output_bytes: usize,
    },
    Context {
        start: RecordId,
        key: String,
        content: ContextContent,
        output_bytes: usize,
    },
    Instructions {
        start: RecordId,
        state: InstructionState,
        outcome: OperationOutcome,
    },
    Complete {
        agent: AgentId,
        outcome: AgentOutcome,
    },
    Relocate {
        directory: String,
    },
}

enum MutationResult {
    Admission(AgentAdmission),
    Outcome(OperationOutcome),
    Unit,
}

async fn write(
    store: &SqliteStore,
    mutation: Mutation,
) -> Result<MutationResult, PersistenceError> {
    let mut writer = tokio::time::timeout(WAIT, store.attachment.database.writer.lock())
        .await
        .map_err(|_| PersistenceError::Busy)?;
    writer.settle().await?;
    writer.attachment = Some(Arc::clone(&store.attachment.lease));
    let result = async {
        let mut transaction = writer.connection()?.begin_with("BEGIN IMMEDIATE").await?;
        let result = apply(&mut transaction, store, mutation).await;
        match result {
            Ok(result) => transaction
                .commit()
                .await
                .map(|()| result)
                .map_err(|error| {
                    tracing::error!(%error,"SQLite local commit outcome unknown");
                    PersistenceError::OutcomeUnknown
                }),
            Err(error) => match transaction.rollback().await {
                Ok(()) => Err(error),
                Err(rollback) => {
                    tracing::error!(%rollback,"SQLite local rollback outcome unknown");
                    Err(PersistenceError::OutcomeUnknown)
                }
            },
        }
    }
    .await;
    if let Err(error) = writer.settle().await {
        tracing::error!(%error,"SQLite mutation did not settle");
        return Err(PersistenceError::OutcomeUnknown);
    }
    result
}

async fn apply(
    connection: &mut SqliteConnection,
    store: &SqliteStore,
    mutation: Mutation,
) -> Result<MutationResult, PersistenceError> {
    reads::writable(connection, store).await?;
    let result = match mutation {
        Mutation::Admit {
            start,
            name,
            assignment,
            output_bytes,
        } => {
            let record = operation_start(connection, store, start).await?;
            let RecordKind::OperationStarted {
                name: method,
                arguments,
                ..
            } = &record.kind
            else {
                return Err(PersistenceError::OperationMismatch);
            };
            #[derive(serde::Deserialize)]
            struct Arguments {
                name: String,
                prompt: String,
                alias: Option<String>,
            }
            let arguments: Arguments = serde_json::from_value(arguments.clone())
                .map_err(|_| PersistenceError::OperationMismatch)?;
            if method != "cocoa.spawn"
                || arguments.name != name
                || arguments.prompt != assignment.prompt
                || arguments
                    .alias
                    .as_ref()
                    .is_some_and(|alias| alias != &assignment.alias)
            {
                return Err(PersistenceError::OperationMismatch);
            }
            if let Some(outcome) = receipt(connection, start).await? {
                check_output(&outcome, output_bytes)?;
                let OperationOutcome::Completed { value } = &outcome else {
                    return Err(PersistenceError::InvalidData("admission receipt"));
                };
                let agent: AgentId = serde_json::from_value(
                    value
                        .get("agent_id")
                        .cloned()
                        .ok_or(PersistenceError::InvalidData("admission identity"))?,
                )
                .map_err(|_| PersistenceError::InvalidData("admission identity"))?;
                let retained = sessions::agents(connection, store.attachment.session_id).await?;
                if !retained.iter().any(|entry| {
                    entry.id == agent
                        && entry.name == name
                        && matches!(
                            &entry.membership,
                            AgentMembership::Child { parent, assignment: retained }
                                if *parent == store.agent && retained == &assignment
                        )
                }) {
                    return Err(PersistenceError::OperationMismatch);
                }
                return Ok(MutationResult::Admission(AgentAdmission {
                    agent,
                    outcome,
                    created: false,
                }));
            }
            let settings = sessions::settings(connection, store.attachment.session_id).await?;
            let super::StoredSessionSettings::Bound { effective } = settings else {
                return Err(PersistenceError::InvalidData(
                    "child admission before root configuration",
                ));
            };
            if !effective.aliases.contains_key(&assignment.alias) {
                return Err(PersistenceError::SettingsMismatch);
            }
            let agents = sessions::agents(connection, store.attachment.session_id).await?;
            let retained_max = usize::try_from(effective.limits.agents_retained)
                .map_err(|_| StoreError::InvalidLimits)?;
            let active_max = usize::try_from(effective.limits.agents_active)
                .map_err(|_| StoreError::InvalidLimits)?;
            if agents.len() > retained_max
                || agents
                    .iter()
                    .filter(|agent| {
                        matches!(agent.membership, AgentMembership::Child { .. })
                            && agent.outcome.is_none()
                    })
                    .count()
                    >= active_max
            {
                return Err(StoreError::Capacity.into());
            }
            let maximum: i64 = sqlx::query_scalar("SELECT coalesce(max(id),0) FROM agents")
                .fetch_one(&mut *connection)
                .await?;
            let id = maximum
                .checked_add(1)
                .ok_or(StoreError::IdentityExhausted)?;
            let agent = AgentId::try_from(id)
                .map_err(|_| PersistenceError::InvalidData("allocated child identity"))?;
            let outcome = OperationOutcome::Completed {
                value: json!({"agent_id":agent,"name":name,"alias":assignment.alias}),
            };
            check_output(&outcome, output_bytes)?;
            sqlx::query("INSERT INTO agents (id,session_id,name,parent_agent_id,format_version,assignment_json,lifecycle) VALUES (?,?,?,?,?,?,'participating')")
                .bind(id).bind(i64::from(store.attachment.session_id)).bind(&name).bind(i64::from(store.agent)).bind(FORMAT_VERSION).bind(encode(&assignment)?).execute(&mut *connection).await?;
            put_receipt(connection, start, &outcome).await?;
            MutationResult::Admission(AgentAdmission {
                agent,
                outcome,
                created: true,
            })
        }
        Mutation::Context {
            start,
            key,
            content,
            output_bytes,
        } => {
            let record = operation_start(connection, store, start).await?;
            let RecordKind::OperationStarted {
                name, arguments, ..
            } = &record.kind
            else {
                return Err(PersistenceError::OperationMismatch);
            };
            #[derive(serde::Deserialize)]
            struct Arguments {
                key: String,
                content: ContextContent,
            }
            let arguments: Arguments = serde_json::from_value(arguments.clone())
                .map_err(|_| PersistenceError::OperationMismatch)?;
            if name != "context.set" || arguments.key != key || arguments.content != content {
                return Err(PersistenceError::OperationMismatch);
            }
            if let Some(outcome) = receipt(connection, start).await? {
                check_output(&outcome, output_bytes)?;
                return Ok(MutationResult::Outcome(outcome));
            }
            let mut records = sessions::context(connection, store.attachment.session_id).await?;
            let previous = records.iter().position(|record| record.key == key);
            if previous.is_some_and(|index| records[index].owner != store.agent) {
                return Err(PersistenceError::ContextNotOwner);
            }
            let id = match previous {
                Some(index) => records[index].id,
                None => {
                    let maximum: i64 =
                        sqlx::query_scalar("SELECT coalesce(max(id),0) FROM context_records")
                            .fetch_one(&mut *connection)
                            .await?;
                    ContextRecordId::try_from(
                        maximum
                            .checked_add(1)
                            .ok_or(StoreError::IdentityExhausted)?,
                    )
                    .map_err(|_| PersistenceError::InvalidData("allocated context identity"))?
                }
            };
            let accepted = StoredContextRecord {
                id,
                key: key.clone(),
                owner: store.agent,
                content,
            };
            if encoded_size(&accepted)? > 16384 {
                return Err(StoreError::RecordLimit.into());
            }
            let outcome = OperationOutcome::Completed {
                value: serde_json::to_value(&accepted).map_err(|_| StoreError::Encoding)?,
            };
            check_output(&outcome, output_bytes)?;
            match previous {
                Some(index) => records[index] = accepted.clone(),
                None => records.push(accepted.clone()),
            };
            if records.len() > 256
                || records.iter().try_fold(0_usize, |bytes, record| {
                    Ok::<_, StoreError>(bytes + encoded_size(record)?)
                })? > 1024 * 1024
            {
                return Err(StoreError::Capacity.into());
            }
            sqlx::query("INSERT INTO context_records (id,session_id,key,owner_agent_id,content_version,content_json) VALUES (?,?,?,?,?,?) ON CONFLICT(session_id,key) DO UPDATE SET content_version=excluded.content_version,content_json=excluded.content_json WHERE context_records.owner_agent_id=excluded.owner_agent_id")
                .bind(i64::from(id)).bind(i64::from(store.attachment.session_id)).bind(key).bind(i64::from(store.agent)).bind(FORMAT_VERSION).bind(encode(&accepted.content)?).execute(&mut *connection).await?;
            put_receipt(connection, start, &outcome).await?;
            MutationResult::Outcome(outcome)
        }
        Mutation::Instructions {
            start,
            state,
            outcome,
        } => {
            let record = operation_start(connection, store, start).await?;
            let previous = reads::state(connection, store).await?;
            if let Some(retained) = receipt(connection, start).await? {
                if retained != outcome || previous.as_ref() != Some(&state) {
                    return Err(PersistenceError::OperationMismatch);
                }
                return Ok(MutationResult::Unit);
            }
            let targets: BTreeSet<_> = state
                .scoped_feedback
                .iter()
                .chain(&state.skill_feedback)
                .map(|feedback| i64::from(feedback.record))
                .collect();
            let targets = reads::feedback_targets(
                connection,
                store,
                &targets.into_iter().collect::<Vec<_>>(),
            )
            .await?;
            state.validate_transition(&record, previous.as_ref(), &targets)?;
            reads::append_stats(connection, store).await?;
            sqlx::query("INSERT INTO instruction_state (agent_id,format_version,state_json) VALUES (?,?,?) ON CONFLICT(agent_id) DO UPDATE SET format_version=excluded.format_version,state_json=excluded.state_json")
                .bind(i64::from(store.agent)).bind(FORMAT_VERSION).bind(encode(&state)?).execute(&mut *connection).await?;
            put_receipt(connection, start, &outcome).await?;
            MutationResult::Unit
        }
        Mutation::Complete { agent, outcome } => {
            let agents = sessions::agents(connection, store.attachment.session_id).await?;
            let agent = agents
                .iter()
                .find(|entry| {
                    entry.id == agent && matches!(entry.membership, AgentMembership::Child { .. })
                })
                .ok_or(PersistenceError::AgentUnavailable)?;
            if let Some(retained) = &agent.outcome {
                if retained != &outcome {
                    return Err(PersistenceError::OperationMismatch);
                }
                return Ok(MutationResult::Unit);
            }
            sqlx::query("UPDATE agents SET outcome_json=? WHERE id=? AND session_id=? AND parent_agent_id IS NOT NULL AND outcome_json IS NULL")
                .bind(encode(&outcome)?).bind(i64::from(agent.id)).bind(i64::from(store.attachment.session_id)).execute(&mut *connection).await?;
            MutationResult::Unit
        }
        Mutation::Relocate { directory } => {
            sqlx::query("UPDATE sessions SET working_directory=? WHERE id=? AND runtime_epoch=?")
                .bind(directory)
                .bind(i64::from(store.attachment.session_id))
                .bind(store.attachment.epoch)
                .execute(&mut *connection)
                .await?;
            MutationResult::Unit
        }
    };
    sqlx::query(
        "UPDATE sessions SET updated_at_ms=max(updated_at_ms,?) WHERE id=? AND runtime_epoch=?",
    )
    .bind(writes::timestamp()?)
    .bind(i64::from(store.attachment.session_id))
    .bind(store.attachment.epoch)
    .execute(connection)
    .await?;
    Ok(result)
}

async fn operation_start(
    connection: &mut SqliteConnection,
    store: &SqliteStore,
    start: RecordId,
) -> Result<Record, PersistenceError> {
    let metadata = sqlx::query_as::<_, reads::RecordMetadata>(
        "SELECT id, sequence, invocation, runtime_epoch, payload_version, \
         coalesce(octet_length(payload_json), archive_decoded_bytes) AS payload_bytes \
         FROM records WHERE id = ? AND session_id = ? AND agent_id = ?",
    )
    .bind(i64::from(start))
    .bind(i64::from(store.attachment.session_id))
    .bind(i64::from(store.agent))
    .fetch_optional(&mut *connection)
    .await?
    .ok_or(PersistenceError::OperationMismatch)?;
    let _payload = store
        .attachment
        .database
        .payload(reads::metadata_bytes(&metadata, store)?)?;
    let record = reads::payload_pages(connection, store, vec![metadata])
        .await?
        .pop()
        .ok_or(PersistenceError::OperationMismatch)?
        .record;
    if !matches!(record.kind, RecordKind::OperationStarted { .. }) {
        return Err(PersistenceError::OperationMismatch);
    }
    Ok(record)
}

async fn receipt(
    connection: &mut SqliteConnection,
    start: RecordId,
) -> Result<Option<OperationOutcome>, PersistenceError> {
    let row:Option<(i64,Option<String>)>=sqlx::query_as("SELECT outcome_version,CASE WHEN octet_length(outcome_json)<=1048576 THEN outcome_json END FROM mutation_receipts WHERE operation_record_id=?")
        .bind(i64::from(start)).fetch_optional(connection).await?;
    let Some((version, text)) = row else {
        return Ok(None);
    };
    if version != FORMAT_VERSION {
        return Err(PersistenceError::UnsupportedPayload(version));
    }
    Ok(Some(
        serde_json::from_str(&text.ok_or(StoreError::RecordLimit)?)
            .map_err(|_| PersistenceError::InvalidData("receipt JSON"))?,
    ))
}

async fn put_receipt(
    connection: &mut SqliteConnection,
    start: RecordId,
    outcome: &OperationOutcome,
) -> Result<(), PersistenceError> {
    if encoded_size(outcome)? > 1024 * 1024 {
        return Err(StoreError::RecordLimit.into());
    }
    sqlx::query("INSERT INTO mutation_receipts (operation_record_id,outcome_version,outcome_json) VALUES (?,?,?)")
        .bind(i64::from(start)).bind(FORMAT_VERSION).bind(encode(outcome)?).execute(connection).await?;
    Ok(())
}

fn check_output(outcome: &OperationOutcome, output_bytes: usize) -> Result<(), PersistenceError> {
    if let OperationOutcome::Completed { value } = outcome {
        if !host_value_fits(value, output_bytes) {
            return Err(StoreError::RecordLimit.into());
        }
    } else if encoded_size(outcome)? > output_bytes {
        return Err(StoreError::RecordLimit.into());
    }
    Ok(())
}

fn encode(value: &impl serde::Serialize) -> Result<String, PersistenceError> {
    serde_json::to_string(value).map_err(|_| StoreError::Encoding.into())
}
