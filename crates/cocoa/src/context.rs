//! Bounded shared records and durable local instruction acceptance.

use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use cocoa_core::{
    capability::{
        ActivityOutput, CapabilityBinding, CapabilityDescription, CapabilityError, CapabilityHost,
        CapabilityInput, CapabilityObserver, OperationOutcome,
    },
    compaction::Checkpoint,
    context::{
        ContextAssembler, ContextContent, ContextError, ContextRecord, ContextType,
        InstructionState, PreparedContext, SharedContext,
    },
    engine::ActivityStatus,
    execution::{WorkLimits, host_value_fits},
    store::{AgentId, RecordId, StoreError, StoredRecord, encoded_size},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::{agents::AgentParticipant, persistence::SqliteStore};

const RECORDS_MAX: usize = 256;
const RECORD_BYTES_MAX: usize = 16 * 1024;
const SESSION_BYTES_MAX: usize = 1024 * 1024;
const PAGE_BYTES_MAX: usize = 8 * 1024;
const SHARED_BYTES_MAX: usize = 64 * 1024;

pub(crate) struct ContextSnapshot {
    shared: SharedContext,
    required: usize,
}

pub(crate) fn bind(
    participant: AgentParticipant,
    inner: Arc<dyn CapabilityHost>,
) -> Arc<dyn CapabilityHost> {
    Arc::new(ContextCapabilities { participant, inner })
}

pub(crate) fn bind_assembler(
    participant: AgentParticipant,
    inner: Arc<dyn ContextAssembler>,
) -> Arc<dyn ContextAssembler> {
    Arc::new(SessionContext { participant, inner })
}

impl ContextSnapshot {
    fn finish(
        mut self,
        bytes: usize,
        delivered: Option<&SharedContext>,
    ) -> Result<SharedContext, ContextError> {
        while delivered != Some(&self.shared) && encoded_size(&self.shared)? > bytes {
            if self.shared.records.len() == self.required {
                return Err(ContextError::Limit);
            }
            self.shared.records.truncate(self.shared.records.len() - 1);
            self.shared.omitted += 1;
        }
        Ok(self.shared)
    }
}

#[derive(Default)]
pub(crate) struct ContextRecords {
    records: BTreeMap<String, ContextRecord>,
}

impl ContextRecords {
    pub(crate) fn restore(records: impl IntoIterator<Item = ContextRecord>) -> Self {
        Self {
            records: records
                .into_iter()
                .map(|record| (record.key.clone(), record))
                .collect(),
        }
    }

    pub(crate) fn owner(&self, key: &str) -> Option<AgentId> {
        self.records.get(key).map(|record| record.owner)
    }

    pub(crate) fn snapshot(&self) -> Result<Option<ContextSnapshot>, ContextError> {
        if self.records.is_empty() {
            return Ok(None);
        }
        debug_assert!(
            self.records.len() <= RECORDS_MAX,
            "snapshot omission counts must fit the session record bound"
        );
        let mut shared = SharedContext {
            records: Vec::new(),
            omitted: self.records.len() as u32,
        };
        for record in self
            .records
            .values()
            .filter(|record| record.content.kind == ContextType::Constraint)
        {
            shared.records.push(record.clone());
            shared.omitted -= 1;
            if encoded_size(&shared)? > SHARED_BYTES_MAX {
                return Err(ContextError::Limit);
            }
        }
        let required = shared.records.len();
        for record in self
            .records
            .values()
            .filter(|record| record.content.kind != ContextType::Constraint)
        {
            shared.records.push(record.clone());
            shared.omitted -= 1;
            if encoded_size(&shared)? > SHARED_BYTES_MAX {
                shared.records.truncate(shared.records.len() - 1);
                shared.omitted += 1;
                break;
            }
        }
        Ok(Some(ContextSnapshot { shared, required }))
    }

    pub(crate) fn call(
        &mut self,
        caller: AgentId,
        name: &str,
        arguments: Value,
        limits: WorkLimits,
        cancellation: &CancellationToken,
    ) -> Result<Value, CapabilityError> {
        let owner = arguments
            .get("key")
            .and_then(Value::as_str)
            .and_then(|key| self.records.get(key))
            .map(|record| record.owner);
        if !host_value_fits(&arguments, limits.output_bytes) {
            return Err(rejected(
                "invalid_arguments",
                "Context arguments exceed the call budget",
                owner,
            ));
        }
        match name {
            "context.get" => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Arguments {
                    key: String,
                }
                let arguments: Arguments = serde_json::from_value(arguments).map_err(|_| {
                    rejected("invalid_arguments", "Expected context.get(key)", owner)
                })?;
                validate_key(&arguments.key, owner)?;
                let value = match self.records.get(&arguments.key) {
                    Some(record) => {
                        serde_json::to_value(record).map_err(|_| StoreError::Encoding)?
                    }
                    None => Value::Null,
                };
                fits(value, limits.output_bytes, owner)
            }
            "context.set" => self.set(caller, arguments, limits, cancellation, owner),
            "context.list" => self.list(arguments, limits),
            _ => Err(rejected(
                "unknown_method",
                "Context method is not installed",
                owner,
            )),
        }
    }

    fn set(
        &mut self,
        caller: AgentId,
        arguments: Value,
        limits: WorkLimits,
        cancellation: &CancellationToken,
        owner: Option<AgentId>,
    ) -> Result<Value, CapabilityError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Arguments {
            key: String,
            content: ContextContent,
        }
        if arguments
            .get("content")
            .and_then(|content| content.get("value"))
            .is_none()
        {
            return Err(rejected(
                "invalid_arguments",
                "Context content requires a value, including null",
                owner,
            ));
        }
        let arguments: Arguments = serde_json::from_value(arguments).map_err(|_| {
            rejected(
                "invalid_arguments",
                "Expected context.set(key, {type, value, labels?}) without owner metadata",
                owner,
            )
        })?;
        validate_key(&arguments.key, owner)?;
        validate_labels(&arguments.content.labels, owner)?;
        if owner.is_some_and(|owner| owner != caller) {
            return Err(rejected(
                "not_owner",
                "Only the current owner may replace this record",
                owner,
            ));
        }
        let record = ContextRecord {
            key: arguments.key,
            content: arguments.content,
            owner: caller,
        };
        let value = serde_json::to_value(&record).map_err(|_| StoreError::Encoding)?;
        let bytes = encoded_size(&value)?;
        if bytes > RECORD_BYTES_MAX {
            return Err(rejected(
                "record_limit",
                "Complete context record exceeds 16384 encoded bytes",
                owner,
            ));
        }
        let value = fits(value, limits.output_bytes, owner)?;
        let mut session_bytes = bytes;
        for existing in self
            .records
            .values()
            .filter(|existing| existing.key != record.key)
        {
            session_bytes += encoded_size(existing)?;
        }
        if session_bytes > SESSION_BYTES_MAX
            || (owner.is_none() && self.records.len() >= RECORDS_MAX)
        {
            return Err(rejected(
                "capacity",
                "Session context capacity reached; no records were evicted",
                owner,
            ));
        }
        if cancellation.is_cancelled() {
            return Err(CapabilityError::Cancelled);
        }
        if tokio::time::Instant::now() >= limits.deadline {
            return Err(CapabilityError::Deadline);
        }
        self.records.insert(record.key.clone(), record);
        Ok(value)
    }

    fn list(&self, arguments: Value, limits: WorkLimits) -> Result<Value, CapabilityError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Filters {
            #[serde(rename = "type")]
            kind: Option<ContextType>,
            #[serde(default)]
            labels: Vec<String>,
            after: Option<String>,
            limit: Option<u32>,
        }
        if arguments
            .as_object()
            .is_some_and(|fields| fields.values().any(Value::is_null))
        {
            return Err(rejected(
                "invalid_arguments",
                "Context filters must not be null",
                None,
            ));
        }
        let filters: Filters = serde_json::from_value(arguments).map_err(|_| {
            rejected(
                "invalid_arguments",
                "Expected context.list({type?, labels?, after?, limit?})",
                None,
            )
        })?;
        validate_labels(&filters.labels, None)?;
        if let Some(after) = &filters.after {
            validate_key(after, None)?;
        }
        let count = filters.limit.unwrap_or(32) as usize;
        if !(1..=64).contains(&count) {
            return Err(rejected(
                "invalid_arguments",
                "Context page limit must be 1..=64",
                None,
            ));
        }
        let mut items = Vec::with_capacity(count);
        let mut next = None;
        let mut matching = self
            .records
            .values()
            .filter(|record| {
                filters
                    .after
                    .as_ref()
                    .is_none_or(|after| record.key > *after)
                    && filters.kind.is_none_or(|kind| record.content.kind == kind)
                    && filters
                        .labels
                        .iter()
                        .all(|label| record.content.labels.contains(label))
            })
            .peekable();
        while items.len() < count {
            let Some(record) = matching.next() else {
                break;
            };
            #[derive(Serialize)]
            struct Metadata<'a> {
                key: &'a str,
                #[serde(rename = "type")]
                kind: ContextType,
                labels: &'a [String],
                owner: AgentId,
            }
            items.push(Metadata {
                key: &record.key,
                kind: record.content.kind,
                labels: &record.content.labels,
                owner: record.owner,
            });
            let continuation = matching.peek().map(|_| record.key.as_str());
            let candidate = json!({"items":items,"next":continuation});
            if !host_value_fits(&candidate, limits.output_bytes.min(PAGE_BYTES_MAX)) {
                items.truncate(items.len() - 1);
                if items.is_empty() {
                    return Err(rejected(
                        "result_limit",
                        "One discovery item cannot fit the call budget",
                        None,
                    ));
                }
                next = items.last().map(|item| item.key);
                break;
            }
            next = continuation;
        }
        fits(
            json!({"items":items,"next":next}),
            limits.output_bytes.min(PAGE_BYTES_MAX),
            None,
        )
    }
}

struct SessionContext {
    participant: AgentParticipant,
    inner: Arc<dyn ContextAssembler>,
}

impl SessionContext {
    async fn prepare_inner(
        &self,
        records: Vec<StoredRecord>,
        capabilities: Vec<CapabilityDescription>,
        bytes: usize,
        checkpoint: Option<&Checkpoint>,
    ) -> Result<PreparedContext, ContextError> {
        let snapshot = self.participant.context_snapshot().await?;
        let mut prepared = match checkpoint {
            Some(checkpoint) => {
                self.inner
                    .prepare_compacted(records, capabilities, bytes, checkpoint)
                    .await?
            }
            None => self.inner.prepare(records, capabilities, bytes).await?,
        };
        let context = &mut prepared.context;
        if context.shared.is_some() {
            return Err(ContextError::Unavailable {
                message: "A session context assembler is already installed".into(),
            });
        }
        let delivered = context.shared_delivered();
        let snapshot = match snapshot {
            Some(snapshot) => snapshot,
            None if delivered.is_some() => ContextSnapshot {
                shared: SharedContext {
                    records: Vec::new(),
                    omitted: 0,
                },
                required: 0,
            },
            None => return Ok(prepared),
        };
        let remaining = bytes
            .checked_sub(context.encoded_size()?)
            .ok_or(ContextError::Limit)?;
        context.shared = Some(snapshot.finish(remaining.min(SHARED_BYTES_MAX), delivered)?);
        debug_assert!(
            context.encoded_size()? <= bytes,
            "shared data must fit alongside required history and project instructions"
        );
        Ok(prepared)
    }
}

#[async_trait]
impl ContextAssembler for SessionContext {
    async fn prepare(
        &self,
        records: Vec<StoredRecord>,
        capabilities: Vec<CapabilityDescription>,
        bytes: usize,
    ) -> Result<PreparedContext, ContextError> {
        self.prepare_inner(records, capabilities, bytes, None).await
    }

    async fn prepare_compacted(
        &self,
        records: Vec<StoredRecord>,
        capabilities: Vec<CapabilityDescription>,
        bytes: usize,
        checkpoint: &Checkpoint,
    ) -> Result<PreparedContext, ContextError> {
        self.prepare_inner(records, capabilities, bytes, Some(checkpoint))
            .await
    }

    async fn restore(&self, state: &InstructionState) -> Result<(), ContextError> {
        self.inner.restore(state).await
    }
}

struct ContextCapabilities {
    participant: AgentParticipant,
    inner: Arc<dyn CapabilityHost>,
}

#[async_trait]
impl CapabilityHost for ContextCapabilities {
    fn descriptions(&self) -> Vec<CapabilityDescription> {
        let mut descriptions = self.inner.descriptions();
        descriptions.retain(|description| !description.name.starts_with("context."));
        let owner = i64::from(self.participant.id()).to_string();
        descriptions.extend([
            CapabilityDescription {
                name: "context.get".into(),
                description: format!("Call await context.get(key) to read a detached current record, or null if absent. All agents in this session can read all shared records. This agent's immutable owner ID is {owner:?}; display names are not unique. Records contain key,type,value,labels,owner; owner is a decimal string, never a JavaScript Number. Reading never transfers ownership, including after a child finishes or its process disappears. Private REPL bindings and conversations are not shared."),
                parameters: json!({"type":"object","properties":{"key":{"type":"string"}},"required":["key"],"additionalProperties":false}),
            },
            CapabilityDescription {
                name: "context.set".into(),
                description: "Call await context.set(key,{type,value,labels?}). Types: artifact,decision,constraint,observation,hypothesis. Creates a record owned by this agent, readable by all session agents; only its current owner may replace it. Replacement replaces all content; omitted labels become empty. value:null keeps a present record. Returns the accepted record. Errors reject with code,method,effects,owner. Keys are exact, case-sensitive, 1..=128 UTF-8 bytes; at most 16 labels, each 1..=64 bytes. Records are at most 16 KiB; session capacity is 256 records/1 MiB. No deletion or transfer is installed.".into(),
                parameters: json!({"type":"object","properties":{"key":{"type":"string"},"content":{"type":"object","properties":{"type":{"enum":["artifact","decision","constraint","observation","hypothesis"]},"value":{},"labels":{"type":"array","items":{"type":"string"}}},"required":["type","value"],"additionalProperties":false}},"required":["key","content"],"additionalProperties":false}),
            },
            CapabilityDescription {
                name: "context.list".into(),
                description: "Call await context.list({type?,labels?,after?,limit?}), using {} for discovery without filters. Returns {items:[{key,type,labels,owner}],next}; fetch values with get. All supplied labels must match exactly. Pages use ascending UTF-8 key order; after is exclusive. Default limit 32, maximum 64, page at most 8 KiB and current call budget. next is the last key if more matches remain, else null. Pages read the current view, not a retained snapshot.".into(),
                parameters: json!({"type":"object","properties":{"type":{"enum":["artifact","decision","constraint","observation","hypothesis"]},"labels":{"type":"array","items":{"type":"string"}},"after":{"type":"string"},"limit":{"type":"integer","minimum":1,"maximum":64}},"additionalProperties":false}),
            },
        ]);
        descriptions
    }

    fn bindings(&self) -> Vec<CapabilityBinding> {
        self.inner.bindings()
    }

    async fn call(
        &self,
        name: &str,
        arguments: Value,
        limits: WorkLimits,
        cancellation: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        if name.starts_with("context.") {
            return self
                .participant
                .call_context(name, arguments, limits, cancellation, None)
                .await;
        }
        self.inner.call(name, arguments, limits, cancellation).await
    }

    async fn call_observed(
        &self,
        name: &str,
        arguments: Value,
        limits: WorkLimits,
        cancellation: CancellationToken,
        observer: CapabilityObserver,
    ) -> Result<Value, CapabilityError> {
        if name.starts_with("context.") {
            observer.input(&self.describe_input(name, &arguments));
            return self.call(name, arguments, limits, cancellation).await;
        }
        self.inner
            .call_observed(name, arguments, limits, cancellation, observer)
            .await
    }

    async fn call_started(
        &self,
        name: &str,
        arguments: Value,
        limits: WorkLimits,
        cancellation: CancellationToken,
        observer: CapabilityObserver,
        operation_record: RecordId,
    ) -> Result<Value, CapabilityError> {
        if name.starts_with("context.") {
            observer.input(&self.describe_input(name, &arguments));
            return self
                .participant
                .call_context(
                    name,
                    arguments,
                    limits,
                    cancellation,
                    Some(operation_record),
                )
                .await;
        }
        self.inner
            .call_started(
                name,
                arguments,
                limits,
                cancellation,
                observer,
                operation_record,
            )
            .await
    }

    fn describe_input(&self, name: &str, arguments: &Value) -> CapabilityInput {
        if !name.starts_with("context.") {
            return self.inner.describe_input(name, arguments);
        }
        CapabilityInput {
            title: name.into(),
            text: arguments
                .get("key")
                .and_then(Value::as_str)
                .unwrap_or("Shared session context")
                .into(),
            summary: None,
        }
    }

    fn describe_output(
        &self,
        name: &str,
        arguments: &Value,
        outcome: &OperationOutcome,
    ) -> Option<ActivityOutput> {
        if name.starts_with("context.") {
            None
        } else {
            self.inner.describe_output(name, arguments, outcome)
        }
    }

    fn activity_status(&self, outcome: &OperationOutcome) -> ActivityStatus {
        self.inner.activity_status(outcome)
    }

    async fn observe_outcome(
        &self,
        name: &str,
        arguments: &Value,
        outcome: &OperationOutcome,
        observer: CapabilityObserver,
    ) {
        if name.starts_with("context.") {
            observer.status(ActivityStatus::from(outcome));
        } else {
            self.inner
                .observe_outcome(name, arguments, outcome, observer)
                .await;
        }
    }
}

#[async_trait]
impl cocoa_tools::InstructionAcceptance for SqliteStore {
    async fn accept(
        &self,
        operation_record: RecordId,
        state: &InstructionState,
        outcome: &OperationOutcome,
    ) -> Result<(), StoreError> {
        self.accept_instruction_change(operation_record, state, outcome)
            .await
            .map_err(|error| StoreError::Backend {
                message: error.to_string(),
            })
    }
}

pub(crate) fn validate_key(key: &str, owner: Option<AgentId>) -> Result<(), CapabilityError> {
    if key.is_empty() || key.len() > 128 {
        return Err(rejected(
            "invalid_arguments",
            "Context key must be 1..=128 UTF-8 bytes",
            owner,
        ));
    }
    Ok(())
}

pub(crate) fn validate_labels(
    labels: &[String],
    owner: Option<AgentId>,
) -> Result<(), CapabilityError> {
    if labels.len() > 16
        || labels
            .iter()
            .any(|label| label.is_empty() || label.len() > 64)
    {
        return Err(rejected(
            "invalid_arguments",
            "Expected at most 16 labels of 1..=64 UTF-8 bytes each",
            owner,
        ));
    }
    Ok(())
}

pub(crate) fn fits(
    value: Value,
    bytes: usize,
    owner: Option<AgentId>,
) -> Result<Value, CapabilityError> {
    if !host_value_fits(&value, bytes) {
        return Err(rejected(
            "result_limit",
            "Context result exceeds the call budget; no changes accepted",
            owner,
        ));
    }
    Ok(value)
}

pub(crate) fn rejected(code: &str, message: &str, owner: Option<AgentId>) -> CapabilityError {
    CapabilityError::Context {
        code: code.into(),
        message: message.into(),
        owner,
    }
}
