//! Typed session snapshots and attachment preflight. No loader executes work.

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use cocoa_core::{
    capability::Effects,
    context::{ContextContent, InstructionState},
    store::{
        AgentId, Record, RecordId, RecordSequence, StoreError, StoreLimits, StoreSnapshot,
        StoredRecord, encoded_size,
    },
};
use serde::{Deserialize, Serialize, Serializer, ser::SerializeStruct};
use sqlx::{Connection, FromRow, QueryBuilder, Sqlite, SqliteConnection};

use super::{
    ASSIGNMENT_JSON_BYTES_MAX, AttachmentState, FORMAT_VERSION, PersistenceError, RECORD_VERSION,
    SessionAttachment, SessionDatabase, SessionId, SessionSummary, SqliteStore, WAIT, archives,
    reads, validate_limits,
};

/// Nonsecret provider identity and policy saved with accepted root input.
/// Later input may rebind settings until the root retains model output.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectiveSessionSettings {
    pub alias: String,
    pub provider: String,
    pub protocol: String,
    pub model: String,
    pub endpoint: Option<String>,
    pub account: Option<String>,
    pub thinking_level: Option<String>,
    pub reasoning_summary: bool,
    pub execution_version: u32,
    pub aliases: BTreeMap<String, EffectiveAliasSettings>,
    pub limits: SessionExecutionLimits,
}

/// Saved eligible child alias definition. Credentials are resolved separately.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectiveAliasSettings {
    pub provider: String,
    pub model: String,
    pub endpoint: Option<String>,
    pub account: Option<String>,
    pub thinking_level: Option<String>,
    pub reasoning_summary: bool,
    pub output_tokens: Option<u32>,
    /// Pinned model window. Missing legacy metadata may resolve on resume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u32>,
}

/// Effective execution/admission policy retained without process-local handles.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionExecutionLimits {
    pub input_bytes: u64,
    pub output_bytes: u64,
    pub record_bytes: u64,
    pub context_bytes: u64,
    pub agents_active: u32,
    pub agents_retained: u32,
    pub pending_operations: u32,
    pub stall_timeout_ms: u64,
    pub settlement_timeout_ms: u64,
    pub shutdown_timeout_ms: u64,
    pub message_steering: bool,
}

/// An empty session need not have selected a provider or configured credentials.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum StoredSessionSettings {
    Unconfigured,
    Bound {
        effective: Box<EffectiveSessionSettings>,
    },
}

/// Exact accepted assignment, separate from a nonunique display name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentAssignment {
    pub prompt: String,
    pub alias: String,
}

/// Retained result, never an instruction to restart a child.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AgentOutcome {
    Completed {
        value: serde_json::Value,
    },
    Failed {
        code: String,
        message: String,
        effects: Effects,
    },
}

/// Durable membership and exact terminal result. Names do not identify agents.
/// JSON retains separate `parent` and `assignment` fields, both null for roots
/// and both present for children. Deserialization rejects an incomplete pair.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(try_from = "RetainedAgentFields")]
pub struct RetainedAgent {
    pub id: AgentId,
    pub name: String,
    pub membership: AgentMembership,
    pub outcome: Option<AgentOutcome>,
}

/// A root has no parent or assignment; a child retains both together.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentMembership {
    Root,
    Child {
        parent: AgentId,
        assignment: AgentAssignment,
    },
}

impl Serialize for RetainedAgent {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let (parent, assignment) = match &self.membership {
            AgentMembership::Root => (None, None),
            AgentMembership::Child { parent, assignment } => (Some(parent), Some(assignment)),
        };
        let mut fields = serializer.serialize_struct("RetainedAgent", 5)?;
        fields.serialize_field("id", &self.id)?;
        fields.serialize_field("parent", &parent)?;
        fields.serialize_field("name", &self.name)?;
        fields.serialize_field("assignment", &assignment)?;
        fields.serialize_field("outcome", &self.outcome)?;
        fields.end()
    }
}

/// Private row identity preserved by context replacement.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "i64", into = "i64")]
pub struct ContextRecordId(i64);

impl TryFrom<i64> for ContextRecordId {
    type Error = cocoa_core::store::IdentityError;
    fn try_from(value: i64) -> Result<Self, Self::Error> {
        if value <= 0 {
            return Err(Self::Error::NonPositive);
        }
        Ok(Self(value))
    }
}

impl From<ContextRecordId> for i64 {
    fn from(value: ContextRecordId) -> Self {
        value.0
    }
}

/// Current owner-bearing content. The internal row identity is not public JSON.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct StoredContextRecord {
    #[serde(skip)]
    pub id: ContextRecordId,
    pub key: String,
    pub owner: AgentId,
    #[serde(flatten)]
    pub content: ContextContent,
}

/// Coherent bounded restoration evidence, keeping participant histories separate.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionSnapshot {
    pub metadata: SessionSummary,
    pub settings: StoredSessionSettings,
    pub agents: Vec<RetainedAgent>,
    pub histories: BTreeMap<AgentId, StoreSnapshot>,
    pub context: Vec<StoredContextRecord>,
    pub receipts: BTreeMap<RecordId, cocoa_core::capability::OperationOutcome>,
}

/// Coordination state does not require decoding provider transcripts.
#[derive(Clone, Debug, PartialEq)]
pub struct CoordinationSnapshot {
    pub agents: Vec<RetainedAgent>,
    pub context: Vec<StoredContextRecord>,
}

impl SessionDatabase {
    /// Acquire the target lock and validate its retained data without recovery.
    /// An already attached session returns the same attachment, without reset.
    ///
    /// # Errors
    /// Reject ambiguous/missing IDs, active owners, bounds and malformed data.
    pub async fn attach_session(
        &self,
        reference: &str,
        limits: StoreLimits,
    ) -> Result<SessionAttachment, PersistenceError> {
        let summary = self.resolve_session(reference).await?;
        let agent = {
            let _request = self.inner.request()?;
            let mut reader = tokio::time::timeout(WAIT, self.inner.reader.lock())
                .await
                .map_err(|_| PersistenceError::Busy)?;
            reader.settle().await?;
            sqlx::query_scalar::<_, i64>(
                "SELECT id FROM agents WHERE session_id = ? AND parent_agent_id IS NULL",
            )
            .bind(i64::from(summary.id))
            .fetch_optional(reader.connection()?)
            .await?
            .ok_or(PersistenceError::InvalidData("missing root"))?
        };
        let store = self
            .agent_store(
                AgentId::try_from(agent)
                    .map_err(|_| PersistenceError::InvalidData("root identity"))?,
                limits,
            )
            .await?;
        let attachment = SessionAttachment {
            inner: store.attachment,
        };
        attachment.load(limits).await?;
        Ok(attachment)
    }
}

impl SessionAttachment {
    /// Shared database handle, retaining its schema ownership and connection limits.
    #[must_use]
    pub fn database(&self) -> SessionDatabase {
        SessionDatabase {
            inner: Arc::clone(&self.inner.database),
        }
    }

    /// Whether this attachment represents a fresh runtime after recovery.
    #[must_use]
    pub fn is_resumed(&self) -> bool {
        self.inner.recovered
    }

    /// Whether the root history retained no model output yet.
    ///
    /// Nothing a provider produced is retained in such a session, so it can
    /// still adopt another alias or model: the next accepted input rebinds its
    /// effective settings. A failure before the first response leaves exactly
    /// this state, which lets the terminal offer another model instead of
    /// stranding the session on a rejected provider.
    ///
    /// # Errors
    /// Reject unreadable evidence, exhausted admission or storage failures.
    pub async fn model_switch_allowed(&self) -> Result<bool, PersistenceError> {
        let _request = self.inner.database.request()?;
        let mut reader = tokio::time::timeout(WAIT, self.inner.database.reader.lock())
            .await
            .map_err(|_| PersistenceError::Busy)?;
        reader.settle().await?;
        let retained =
            reads::holds_model_output(reader.connection()?, self.id(), self.root_agent()).await?;
        Ok(!retained)
    }

    /// Bind settings for the next accepted root input. Binding alone writes nothing.
    /// Once model output is retained, appends may only fill missing token limits;
    /// all other settings and known limits remain pinned.
    ///
    /// # Errors
    /// Reject invalid settings and store limits before constructing adapters.
    pub fn root_store_with_settings(
        &self,
        limits: StoreLimits,
        settings: EffectiveSessionSettings,
    ) -> Result<SqliteStore, PersistenceError> {
        settings.validate()?;
        let mut store = self.root_store_with_instruction_state(limits)?;
        store.settings = Some(Arc::new(std::sync::Mutex::new(settings)));
        Ok(store)
    }

    /// Bind a member whose model appends require prepared instruction state.
    ///
    /// # Errors
    /// Return membership, limit and storage failures as with `agent_store`.
    pub async fn agent_store_with_instruction_state(
        &self,
        agent: AgentId,
        limits: StoreLimits,
    ) -> Result<SqliteStore, PersistenceError> {
        let mut store = self.agent_store(agent, limits).await?;
        store.instructions = super::InstructionPolicy::Required;
        if let StoredSessionSettings::Bound { effective } = self.settings().await? {
            store.settings = Some(Arc::new(std::sync::Mutex::new(*effective)));
        }
        Ok(store)
    }

    /// Bind only a participating member of this attachment's session.
    /// Completed children remain readable, but their stores reject further writes.
    ///
    /// # Errors
    /// Reject foreign/absent agents, invalid limits, stale epochs or storage failures.
    pub async fn agent_store(
        &self,
        agent: AgentId,
        limits: StoreLimits,
    ) -> Result<SqliteStore, PersistenceError> {
        let store = self.database().agent_store(agent, limits).await?;
        if !Arc::ptr_eq(&store.attachment, &self.inner) {
            return Err(PersistenceError::AgentUnavailable);
        }
        Ok(store)
    }

    /// Read metadata, typed settings and every participant collection in one view.
    /// Observing a successful result clears the uncertain-write fence. Reconcile
    /// the snapshot before deciding whether to retry an abandoned mutation.
    ///
    /// # Errors
    /// Reject stale epochs, unsupported encodings, invalid references or bounds,
    /// exhausted admission/lock waits and storage failures.
    pub async fn load(&self, limits: StoreLimits) -> Result<SessionSnapshot, PersistenceError> {
        validate_limits(limits)?;
        let request = self.inner.database.request()?;
        let attachment = self.clone();
        let (snapshot, mut operation) = tokio::spawn(async move {
            let _request = request;
            let operation =
                tokio::time::timeout(WAIT, Arc::clone(&attachment.inner.operation).lock_owned())
                    .await
                    .map_err(|_| PersistenceError::Busy)?;
            if matches!(*operation, AttachmentState::Uncertain { .. }) {
                tokio::time::timeout(WAIT, attachment.inner.database.writer.lock())
                    .await
                    .map_err(|_| PersistenceError::Busy)?
                    .settle()
                    .await?;
            }
            let mut reader = tokio::time::timeout(WAIT, attachment.inner.database.reader.lock())
                .await
                .map_err(|_| PersistenceError::Busy)?;
            reader.settle().await?;
            let mut transaction = reader.connection()?.begin().await?;
            let snapshot = snapshot(&mut transaction, &attachment, limits).await?;
            transaction.commit().await?;
            Ok::<_, PersistenceError>((snapshot, operation))
        })
        .await??;
        *operation = AttachmentState::Ready;
        Ok(snapshot)
    }

    /// Read coordination rows in one snapshot, without provider payloads.
    ///
    /// # Errors
    /// Reject stale epochs, malformed collections, exhausted admission/lock waits
    /// or storage failures.
    pub async fn coordination_snapshot(&self) -> Result<CoordinationSnapshot, PersistenceError> {
        let request = self.inner.database.request()?;
        let _request = request;
        let _operation = tokio::time::timeout(WAIT, self.inner.operation.lock())
            .await
            .map_err(|_| PersistenceError::Busy)?;
        let mut reader = tokio::time::timeout(WAIT, self.inner.database.reader.lock())
            .await
            .map_err(|_| PersistenceError::Busy)?;
        reader.settle().await?;
        let mut transaction = reader.connection()?.begin().await?;
        check_epoch(&mut transaction, self).await?;
        let agents = agents(&mut transaction, self.id()).await?;
        let context = context(&mut transaction, self.id()).await?;
        transaction.commit().await?;
        Ok(CoordinationSnapshot { agents, context })
    }

    /// Saved project directory; reading does not relocate or mark activity.
    ///
    /// # Errors
    /// Propagate metadata/storage validation failures.
    pub async fn working_directory(&self) -> Result<PathBuf, PersistenceError> {
        Ok(self
            .database()
            .resolve_session(self.public_id().as_str())
            .await?
            .working_directory)
    }

    /// Saved typed configuration, including the unconfigured empty-session state.
    ///
    /// # Errors
    /// Reject malformed/unsupported settings, exhausted admission/lock waits
    /// or storage failures.
    pub async fn settings(&self) -> Result<StoredSessionSettings, PersistenceError> {
        let _request = self.inner.database.request()?;
        let mut reader = tokio::time::timeout(WAIT, self.inner.database.reader.lock())
            .await
            .map_err(|_| PersistenceError::Busy)?;
        reader.settle().await?;
        settings(reader.connection()?, self.id()).await
    }
}

impl EffectiveSessionSettings {
    /// Whether `resolved` only fills previously unknown model token limits.
    /// Model identity, accounts, execution settings and known limits cannot change.
    #[must_use]
    pub fn can_resolve_limits_to(&self, resolved: &Self) -> bool {
        let mut previous = self.clone();
        for (name, alias) in &mut previous.aliases {
            if let Some(candidate) = resolved.aliases.get(name) {
                alias.output_tokens = alias.output_tokens.or(candidate.output_tokens);
                alias.context_tokens = alias.context_tokens.or(candidate.context_tokens);
            }
        }
        previous == *resolved
    }

    pub(super) fn validate(&self) -> Result<(), PersistenceError> {
        let limits = &self.limits;
        if self.alias.is_empty()
            || self.provider.is_empty()
            || self.protocol.is_empty()
            || self.model.is_empty()
            || self.execution_version == 0
            || self.aliases.len() > 64
            || encoded_size(self)? > 64 * 1024
            || limits.input_bytes == 0
            || limits.output_bytes == 0
            || limits.context_bytes == 0
            || limits.pending_operations == 0
            || limits.pending_operations > 1024
            || limits.stall_timeout_ms == 0
            || limits.settlement_timeout_ms == 0
            || limits.shutdown_timeout_ms == 0
            || !(1..=16).contains(&limits.agents_active)
            || !(limits.agents_active..=32).contains(&limits.agents_retained)
            || self.aliases.iter().any(|(name, alias)| {
                name.is_empty()
                    || alias.provider.is_empty()
                    || alias.model.is_empty()
                    || alias.output_tokens == Some(0)
                    || alias.context_tokens == Some(0)
            })
        {
            return Err(PersistenceError::InvalidData("effective session settings"));
        }
        let record_bytes =
            usize::try_from(limits.record_bytes).map_err(|_| StoreError::InvalidLimits)?;
        validate_limits(StoreLimits { record_bytes })?;
        Ok(())
    }
}

pub(super) async fn settings(
    connection: &mut SqliteConnection,
    session: SessionId,
) -> Result<StoredSessionSettings, PersistenceError> {
    let (version, text): (i64, Option<String>) = sqlx::query_as("SELECT settings_version, CASE WHEN octet_length(settings_json) <= 65536 THEN settings_json END FROM sessions WHERE id = ?")
        .bind(i64::from(session)).fetch_one(connection).await?;
    if version != FORMAT_VERSION {
        return Err(PersistenceError::UnsupportedPayload(version));
    }
    let settings: StoredSessionSettings = serde_json::from_str(&text.ok_or(StoreError::Capacity)?)
        .map_err(|_| PersistenceError::InvalidData("session settings JSON"))?;
    if let StoredSessionSettings::Bound { effective } = &settings {
        effective.validate()?;
    }
    Ok(settings)
}

pub(super) async fn check_epoch(
    connection: &mut SqliteConnection,
    attachment: &SessionAttachment,
) -> Result<(), PersistenceError> {
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM sessions WHERE id = ? AND runtime_epoch = ?")
            .bind(i64::from(attachment.id()))
            .bind(attachment.inner.epoch)
            .fetch_one(connection)
            .await?;
    if count != 1 {
        return Err(PersistenceError::AgentUnavailable);
    }
    Ok(())
}

pub(super) async fn snapshot(
    connection: &mut SqliteConnection,
    attachment: &SessionAttachment,
    limits: StoreLimits,
) -> Result<SessionSnapshot, PersistenceError> {
    check_epoch(connection, attachment).await?;
    let metadata = reads::resolved_session(
        reads::resolve_session(connection, attachment.public_id().as_str()).await?,
    )?;
    let settings = settings(connection, attachment.id()).await?;
    let agents = agents(connection, attachment.id()).await?;
    if agents
        .iter()
        .filter(|agent| matches!(agent.membership, AgentMembership::Root))
        .map(|agent| agent.id)
        .collect::<Vec<_>>()
        != vec![attachment.root_agent()]
    {
        return Err(PersistenceError::InvalidData("session root membership"));
    }
    let context = context(connection, attachment.id()).await?;
    if context
        .iter()
        .any(|record| !agents.iter().any(|agent| agent.id == record.owner))
    {
        return Err(PersistenceError::InvalidData("context owner membership"));
    }
    let mut histories = histories_batched(connection, attachment, &agents, limits).await?;
    super::compaction::restore_session(connection, attachment, &mut histories).await?;
    let receipts = super::recovery::receipts(connection, attachment).await?;
    let starts: std::collections::BTreeSet<_> = histories
        .values()
        .flat_map(|history| &history.records)
        .filter(|record| {
            matches!(
                record.record.kind,
                cocoa_core::store::RecordKind::OperationStarted { .. }
            )
        })
        .map(|record| record.id)
        .collect();
    if receipts.keys().any(|id| !starts.contains(id)) {
        return Err(PersistenceError::InvalidData(
            "receipt is not an operation start",
        ));
    }
    if matches!(settings, StoredSessionSettings::Unconfigured)
        && histories
            .values()
            .any(|history| !history.records.is_empty())
    {
        return Err(PersistenceError::InvalidData(
            "unconfigured session has history",
        ));
    }
    Ok(SessionSnapshot {
        metadata,
        settings,
        agents,
        histories,
        context,
        receipts,
    })
}

pub(super) async fn agents(
    connection: &mut SqliteConnection,
    session: SessionId,
) -> Result<Vec<RetainedAgent>, PersistenceError> {
    let rows = sqlx::query_as::<_, AgentRow>("SELECT id, parent_agent_id, name, format_version, lifecycle, CASE WHEN octet_length(assignment_json) <= ? THEN assignment_json END AS assignment_json, CASE WHEN octet_length(outcome_json) <= 1049600 THEN outcome_json END AS outcome_json, assignment_json IS NOT NULL AS has_assignment, outcome_json IS NOT NULL AS has_outcome FROM agents WHERE session_id = ? ORDER BY id LIMIT 34")
        .bind(ASSIGNMENT_JSON_BYTES_MAX as i64).bind(i64::from(session)).fetch_all(connection).await?;
    if rows.len() > 33 {
        return Err(StoreError::Capacity.into());
    }
    let mut agents = Vec::with_capacity(rows.len());
    for row in rows {
        if row.format_version != FORMAT_VERSION {
            return Err(PersistenceError::UnsupportedPayload(row.format_version));
        }
        if row.lifecycle != "participating"
            || row.name.is_empty()
            || row.name.len() > 64
            || !row
                .name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
            || row.has_assignment != row.assignment_json.is_some()
            || row.has_outcome != row.outcome_json.is_some()
        {
            return Err(PersistenceError::InvalidData("agent metadata"));
        }
        let id =
            AgentId::try_from(row.id).map_err(|_| PersistenceError::InvalidData("agent ID"))?;
        let parent = row
            .parent_agent_id
            .map(AgentId::try_from)
            .transpose()
            .map_err(|_| PersistenceError::InvalidData("agent parent"))?;
        if parent.is_some_and(|parent| {
            parent >= id
                || !agents
                    .iter()
                    .any(|agent: &RetainedAgent| agent.id == parent)
        }) {
            return Err(PersistenceError::InvalidData("agent parent membership"));
        }
        let assignment: Option<AgentAssignment> = row
            .assignment_json
            .map(|text| serde_json::from_str(&text))
            .transpose()
            .map_err(|_| PersistenceError::InvalidData("agent assignment"))?;
        let outcome = row
            .outcome_json
            .map(|text| serde_json::from_str(&text))
            .transpose()
            .map_err(|_| PersistenceError::InvalidData("agent outcome"))?;
        if assignment
            .as_ref()
            .is_some_and(|assignment| assignment.prompt.is_empty() || assignment.alias.is_empty())
        {
            return Err(PersistenceError::InvalidData("agent assignment membership"));
        }
        let membership = match (parent, assignment) {
            (None, None) => AgentMembership::Root,
            (Some(parent), Some(assignment)) => AgentMembership::Child { parent, assignment },
            _ => return Err(PersistenceError::InvalidData("agent assignment membership")),
        };
        agents.push(RetainedAgent {
            id,
            name: row.name,
            membership,
            outcome,
        });
    }
    Ok(agents)
}

pub(super) async fn context(
    connection: &mut SqliteConnection,
    session: SessionId,
) -> Result<Vec<StoredContextRecord>, PersistenceError> {
    let rows = sqlx::query_as::<_, ContextRow>("SELECT id, key, owner_agent_id, content_version, CASE WHEN octet_length(content_json) <= 16384 THEN content_json END AS content_json FROM context_records WHERE session_id = ? ORDER BY key LIMIT 257")
        .bind(i64::from(session)).fetch_all(connection).await?;
    if rows.len() > 256 {
        return Err(StoreError::Capacity.into());
    }
    let mut records = Vec::with_capacity(rows.len());
    let mut bytes = 0;
    for row in rows {
        if row.content_version != FORMAT_VERSION {
            return Err(PersistenceError::UnsupportedPayload(row.content_version));
        }
        let record = StoredContextRecord {
            id: ContextRecordId::try_from(row.id)
                .map_err(|_| PersistenceError::InvalidData("context ID"))?,
            key: row.key,
            owner: AgentId::try_from(row.owner_agent_id)
                .map_err(|_| PersistenceError::InvalidData("context owner"))?,
            content: serde_json::from_str(&row.content_json.ok_or(StoreError::Capacity)?)
                .map_err(|_| PersistenceError::InvalidData("context content"))?,
        };
        validate_context(&record.key, &record.content)?;
        let record_bytes = encoded_size(&record)?;
        if record_bytes > 16384 {
            return Err(StoreError::RecordLimit.into());
        }
        bytes += record_bytes;
        if bytes > 1024 * 1024 {
            return Err(StoreError::Capacity.into());
        }
        records.push(record);
    }
    Ok(records)
}

pub(super) fn validate_context(
    key: &str,
    content: &ContextContent,
) -> Result<(), PersistenceError> {
    if key.is_empty()
        || key.len() > 128
        || content.labels.len() > 16
        || content
            .labels
            .iter()
            .any(|label| label.is_empty() || label.len() > 64)
    {
        return Err(PersistenceError::InvalidData("context key or labels"));
    }
    Ok(())
}

async fn histories_batched(
    connection: &mut SqliteConnection,
    attachment: &SessionAttachment,
    agents: &[RetainedAgent],
    limits: StoreLimits,
) -> Result<BTreeMap<AgentId, StoreSnapshot>, PersistenceError> {
    let mut histories: BTreeMap<_, _> = agents
        .iter()
        .map(|agent| {
            (
                agent.id,
                StoreSnapshot {
                    records: Vec::new(),
                    instruction_state: None,
                    checkpoints: Vec::new(),
                },
            )
        })
        .collect();
    let metadata = sqlx::query_as::<_, HistoryMetadata>("SELECT id, agent_id, sequence, invocation, runtime_epoch, payload_version, coalesce(octet_length(payload_json), archive_decoded_bytes) AS bytes FROM records WHERE session_id = ? ORDER BY agent_id, sequence")
        .bind(i64::from(attachment.id())).fetch_all(&mut *connection).await?;
    for row in &metadata {
        if row.payload_version != RECORD_VERSION {
            return Err(PersistenceError::UnsupportedPayload(row.payload_version));
        }
        if row.bytes <= 0
            || row.runtime_epoch <= 0
            || row.runtime_epoch > attachment.inner.epoch
            || row.invocation <= 0
            || row.sequence <= 0
        {
            return Err(PersistenceError::InvalidData("record envelope"));
        }
        let size =
            row.bytes as usize + row.invocation.to_string().len() + super::RECORD_ENVELOPE_BYTES;
        if size > limits.record_bytes {
            return Err(StoreError::RecordLimit.into());
        }
    }
    let mut cursor = 0;
    while cursor < metadata.len() {
        let mut end = cursor;
        let mut page_bytes = 0;
        while end < metadata.len() && end - cursor < 64 {
            let size = metadata[end].bytes as usize;
            if end > cursor && page_bytes + size > 4 * 1024 * 1024 {
                break;
            }
            page_bytes += size;
            end += 1;
        }
        let page = &metadata[cursor..end];
        let _payload = attachment.inner.database.payload(page_bytes)?;
        let mut query = QueryBuilder::<Sqlite>::new("SELECT ");
        query
            .push(archives::PAYLOAD_COLUMNS)
            .push(" FROM records NOT INDEXED WHERE session_id = ")
            .push_bind(i64::from(attachment.id()))
            .push(" AND id IN (");
        {
            let mut ids = query.separated(",");
            for row in page {
                ids.push_bind(row.id);
            }
        }
        query.push(") ORDER BY agent_id, sequence");
        let rows: Vec<archives::PayloadRow> =
            query.build_query_as().fetch_all(&mut *connection).await?;
        if rows.len() != page.len() {
            return Err(PersistenceError::InvalidData("history page changed"));
        }
        let page = page.to_vec();
        let decoded = tokio::task::spawn_blocking(move || {
            let mut records = Vec::with_capacity(rows.len());
            for (payload, row) in rows.into_iter().zip(page) {
                if payload.id != row.id {
                    return Err(PersistenceError::InvalidData("history page identity"));
                }
                let record = Record {
                    invocation: cocoa_core::store::InvocationId(row.invocation as u64),
                    kind: payload.decode(row.bytes)?,
                };
                record.validate()?;
                if encoded_size(&record)?
                    != row.bytes as usize
                        + row.invocation.to_string().len()
                        + super::RECORD_ENVELOPE_BYTES
                {
                    return Err(PersistenceError::InvalidData("noncanonical record"));
                }
                records.push((
                    AgentId::try_from(row.agent_id)
                        .map_err(|_| PersistenceError::InvalidData("record agent"))?,
                    StoredRecord {
                        id: RecordId::try_from(row.id)
                            .map_err(|_| PersistenceError::InvalidData("record ID"))?,
                        sequence: RecordSequence::try_from(row.sequence)
                            .map_err(|_| PersistenceError::InvalidData("record sequence"))?,
                        record,
                    },
                ));
            }
            Ok::<_, PersistenceError>(records)
        })
        .await??;
        for (agent, record) in decoded {
            let history = histories
                .get_mut(&agent)
                .ok_or(PersistenceError::InvalidData("history participant"))?;
            if i64::from(record.sequence) != history.records.len() as i64 + 1 {
                return Err(PersistenceError::InvalidData("history sequence gap"));
            }
            history.records.push(record);
        }
        cursor = end;
    }
    let states:Vec<(i64,i64,Option<String>)> = sqlx::query_as("SELECT s.agent_id,s.format_version,CASE WHEN octet_length(s.state_json) <= 16777216 THEN s.state_json END FROM instruction_state s JOIN agents a ON a.id=s.agent_id WHERE a.session_id=? ORDER BY s.agent_id LIMIT 34")
        .bind(i64::from(attachment.id())).fetch_all(connection).await?;
    for (agent, version, text) in states {
        if version != FORMAT_VERSION {
            return Err(PersistenceError::UnsupportedPayload(version));
        }
        let agent =
            AgentId::try_from(agent).map_err(|_| PersistenceError::InvalidData("state agent"))?;
        let text = text.ok_or(StoreError::Capacity)?;
        let state: InstructionState = serde_json::from_str(&text)
            .map_err(|_| PersistenceError::InvalidData("instruction JSON"))?;
        state.validate_bounds(usize::MAX)?;
        let history = histories
            .get_mut(&agent)
            .ok_or(PersistenceError::InvalidData("state participant"))?;
        if !history
            .records
            .iter()
            .any(|record| record.record.invocation == state.invocation)
        {
            return Err(PersistenceError::InvalidData("state invocation"));
        }
        let completed = history
            .records
            .iter()
            .filter(|record| {
                matches!(
                    record.record.kind,
                    cocoa_core::store::RecordKind::CellCompleted { .. }
                )
            })
            .map(|record| (record.id, record.record.invocation))
            .collect();
        state.validate_feedback_targets(&completed)?;
        history.instruction_state = Some(state);
    }
    Ok(histories)
}

#[derive(Deserialize)]
struct RetainedAgentFields {
    id: AgentId,
    parent: Option<AgentId>,
    name: String,
    assignment: Option<AgentAssignment>,
    outcome: Option<AgentOutcome>,
}

impl TryFrom<RetainedAgentFields> for RetainedAgent {
    type Error = PersistenceError;

    fn try_from(fields: RetainedAgentFields) -> Result<Self, Self::Error> {
        let membership = match (fields.parent, fields.assignment) {
            (None, None) => AgentMembership::Root,
            (Some(parent), Some(assignment)) => AgentMembership::Child { parent, assignment },
            _ => return Err(PersistenceError::InvalidData("agent assignment membership")),
        };
        Ok(Self {
            id: fields.id,
            name: fields.name,
            membership,
            outcome: fields.outcome,
        })
    }
}

#[derive(FromRow)]
struct AgentRow {
    id: i64,
    parent_agent_id: Option<i64>,
    name: String,
    format_version: i64,
    lifecycle: String,
    assignment_json: Option<String>,
    outcome_json: Option<String>,
    has_assignment: bool,
    has_outcome: bool,
}

#[derive(FromRow)]
struct ContextRow {
    id: i64,
    key: String,
    owner_agent_id: i64,
    content_version: i64,
    content_json: Option<String>,
}

#[derive(Clone, FromRow)]
struct HistoryMetadata {
    id: i64,
    agent_id: i64,
    sequence: i64,
    invocation: i64,
    runtime_epoch: i64,
    payload_version: i64,
    bytes: i64,
}
