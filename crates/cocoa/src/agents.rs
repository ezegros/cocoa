//! Session-owned agent identities and bounded, session-scoped UI observations.

use std::{
    collections::BTreeMap,
    sync::{
        Arc, Weak,
        atomic::{AtomicU64, Ordering},
    },
};

use async_trait::async_trait;
use cocoa_core::{
    capability::{
        ActivityOutput, CapabilityDescription, CapabilityError, CapabilityHost, CapabilityInput,
        CapabilityObserver, Effects, OperationOutcome,
    },
    context::{ContextAssembler, ContextError},
    engine::{
        ActivityStatus, Engine, EngineSettings, EventPublisher, EventReceiver, Limits, Observation,
        ProviderFailure, RunError, RunRequest,
    },
    execution::{ExecutionError, WorkLimits, host_value_fits},
    store::{AgentId, InvocationId, Record, RecordId, RecordKind, StoreError, StoreLimits},
};
use cocoa_llm::ThinkingLevel;
use futures_util::FutureExt;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::{
    sync::{Mutex, Semaphore, broadcast, mpsc, oneshot, watch},
    task::JoinHandle,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

use crate::context::{ContextRecords, ContextSnapshot};
use crate::persistence::{
    AgentAssignment, AgentMembership, AgentOutcome, PersistenceError, SessionAttachment,
    SqliteStore,
};

static SESSION_NEXT: AtomicU64 = AtomicU64::new(1);
const OBSERVATIONS_RETAINED: usize = 256;
const CHILD_OBSERVATIONS_RETAINED: usize = 64;

/// A child's presentation update. Identity is the pair of session and agent,
/// not an invocation or operation identifier from the child's private engine.
#[derive(Debug)]
pub struct AgentUpdate {
    pub session: u64,
    pub agent: AgentId,
    pub name: String,
    pub kind: AgentUpdateKind,
}

/// Lifecycle comes from manager admission and settlement, never spawn receipts
/// or provisional engine events. Unknown means supervision could not confirm cleanup.
#[derive(Debug)]
pub enum AgentUpdateKind {
    Started,
    ProviderPaused { reason: String },
    ProviderResumed,
    Observation(Observation),
    Finished(ActivityStatus),
}

/// One live child's pending provider request. Retrying preserves its runtime and
/// completed effects. Cancelling stops only this child, not its parent or siblings.
pub struct AgentProviderFailure {
    pub agent: AgentId,
    pub name: String,
    pub failure: ProviderFailure,
    pub cancellation: CancellationToken,
}

/// Bounded live observations with retained lifecycle reconciliation. Slow readers
/// may lose observations, but recover each admission and latest terminal status.
/// Holding a receiver does not keep the session or its workers alive.
pub struct AgentUpdates {
    shared: Weak<Shared>,
    receiver: broadcast::Receiver<Arc<AgentUpdate>>,
    lifecycle: BTreeMap<AgentId, ActivityStatus>,
    paused: BTreeMap<AgentId, String>,
    recovery: Option<mpsc::Receiver<AgentProviderFailure>>,
}

impl AgentUpdates {
    /// Route child provider failures to this terminal observer. Without an
    /// interactive recovery owner, failures remain terminal for headless callers.
    /// Enable before admitting children; one observer owns decisions per manager.
    ///
    /// # Errors
    /// Reject a closed manager or another observer already owning recovery.
    pub async fn enable_recovery(&mut self) -> Result<(), CapabilityError> {
        if self.recovery.is_some() {
            return Ok(());
        }
        let shared = self.shared.upgrade().ok_or_else(unsettled)?;
        let mut registry = shared.registry.lock().await;
        registry.check()?;
        if registry.recovery.is_some() {
            return Err(tool(
                "recovery_owned",
                "another observer owns provider recovery",
            ));
        }
        let (sender, receiver) = mpsc::channel(shared.limits.active);
        registry.recovery = Some(sender);
        self.recovery = Some(receiver);
        Ok(())
    }

    /// Take the next pending child interruption without waiting. Decisions must
    /// remain owned until retry, child cancellation or session shutdown.
    pub fn try_failure(&mut self) -> Option<AgentProviderFailure> {
        self.recovery.as_mut()?.try_recv().ok()
    }

    /// Poll without waiting for work or a busy manager. Each drain reads at most
    /// 257 channel entries, including lag, and reconciles at most 32 agents. Lag is
    /// reported per child; it never implies a lifecycle transition. New readers
    /// also recover admissions and terminal states that predate subscription.
    pub fn drain(&mut self) -> Vec<Arc<AgentUpdate>> {
        let Some(shared) = self.shared.upgrade() else {
            return Vec::new();
        };
        let Ok(registry) = shared.registry.try_lock() else {
            return Vec::new();
        };
        let mut updates = Vec::new();
        for entry in registry.entries.values() {
            if let std::collections::btree_map::Entry::Vacant(lifecycle) =
                self.lifecycle.entry(entry.id)
            {
                updates.push(shared.update(entry.id, &entry.name, AgentUpdateKind::Started));
                lifecycle.insert(ActivityStatus::Running);
            }
        }
        let mut missed = 0_u64;
        for _ in 0..=OBSERVATIONS_RETAINED {
            match self.receiver.try_recv() {
                Ok(update) => updates.push(update),
                Err(broadcast::error::TryRecvError::Lagged(count)) => {
                    missed = missed.saturating_add(count);
                }
                Err(
                    broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed,
                ) => break,
            }
        }
        for entry in registry.entries.values() {
            if let Stage::ProviderPaused { reason } = &entry.stage {
                if self.paused.get(&entry.id) != Some(reason)
                    || updates.iter().any(|update| update.agent == entry.id)
                {
                    updates.push(shared.update(
                        entry.id,
                        &entry.name,
                        AgentUpdateKind::ProviderPaused {
                            reason: reason.clone(),
                        },
                    ));
                    self.paused.insert(entry.id, reason.clone());
                }
            } else if self.paused.remove(&entry.id).is_some() {
                updates.push(shared.update(
                    entry.id,
                    &entry.name,
                    AgentUpdateKind::ProviderResumed,
                ));
            }
            if missed > 0 {
                updates.push(shared.update(
                    entry.id,
                    &entry.name,
                    AgentUpdateKind::Observation(Observation::Lagged { missed }),
                ));
            }
            if entry.activity != ActivityStatus::Running
                && self.lifecycle.get(&entry.id) != Some(&entry.activity)
            {
                updates.push(shared.update(
                    entry.id,
                    &entry.name,
                    AgentUpdateKind::Finished(entry.activity),
                ));
                self.lifecycle.insert(entry.id, entry.activity);
            }
        }
        updates
    }
}

/// Host-issued session membership, supplied only after child admission. Cloning
/// it preserves identity; a name in JavaScript cannot construct this authority.
/// It does not keep the coordinator alive or grant agent-management methods.
#[derive(Clone)]
pub struct AgentParticipant {
    id: AgentId,
    name: String,
    shared: Weak<Shared>,
    events: Option<EventPublisher>,
}

impl AgentParticipant {
    /// Immutable session membership, independent of the display name.
    #[must_use]
    pub fn id(&self) -> AgentId {
        self.id
    }

    /// Bind this participant's durable history, or return `None` for memory sessions.
    ///
    /// # Errors
    /// Reject invalid limits, unavailable storage or invalid session membership.
    pub async fn durable_store(
        &self,
        limits: StoreLimits,
    ) -> Result<Option<SqliteStore>, PersistenceError> {
        let shared = self
            .shared
            .upgrade()
            .ok_or(PersistenceError::InvalidData("closed agent session"))?;
        match &shared.attachment {
            Some(attachment) => attachment
                .agent_store_with_instruction_state(self.id, limits)
                .await
                .map(Some),
            None => Ok(None),
        }
    }

    /// Nonunique display name within this participant's session.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Publisher for this admitted child's engine. Pass it to Engine::new to
    /// forward bounded observations through the manager's owned relay.
    /// Root participants have no child publisher.
    #[must_use]
    pub fn events(&self) -> Option<EventPublisher> {
        self.events.clone()
    }

    /// Add own-agent historical text reads and session-wide shared context tools.
    /// All unrelated dispatch, generated bindings and observations are forwarded.
    #[must_use]
    pub fn bind(&self, inner: Arc<dyn CapabilityHost>) -> Arc<dyn CapabilityHost> {
        crate::history::bind(self.clone(), crate::context::bind(self.clone(), inner))
    }

    /// Compose private project/history assembly with fresh session-wide shared data.
    /// This uses the same admitted identity as capability reads and retains no
    /// coordinator ownership. Do not wrap an already session-bound assembler.
    #[must_use]
    pub fn bind_context(&self, inner: Arc<dyn ContextAssembler>) -> Arc<dyn ContextAssembler> {
        crate::context::bind_assembler(self.clone(), inner)
    }

    pub(crate) async fn context_snapshot(&self) -> Result<Option<ContextSnapshot>, ContextError> {
        let shared = self
            .shared
            .upgrade()
            .ok_or_else(|| ContextError::Unavailable {
                message: "Context session is closed".into(),
            })?;
        let mut registry = shared.registry.lock().await;
        registry
            .check()
            .map_err(|error| ContextError::Unavailable {
                message: error.to_string(),
            })?;
        debug_assert!(
            self.id == shared.root || registry.entries.contains_key(&self.id),
            "context readers must be admitted session participants"
        );
        if let Some(attachment) = &shared.attachment {
            let snapshot = attachment.coordination_snapshot().await.map_err(|error| {
                ContextError::Unavailable {
                    message: error.to_string(),
                }
            })?;
            registry.context =
                ContextRecords::restore(snapshot.context.into_iter().map(|record| {
                    cocoa_core::context::ContextRecord {
                        key: record.key,
                        owner: record.owner,
                        content: record.content,
                    }
                }));
        }
        registry.context.snapshot()
    }

    pub(crate) async fn call_context(
        &self,
        name: &str,
        arguments: Value,
        limits: WorkLimits,
        cancellation: CancellationToken,
        operation_record: Option<RecordId>,
    ) -> Result<Value, CapabilityError> {
        check_caller(limits, &cancellation)?;
        let unavailable = || CapabilityError::Context {
            code: "session_closed".into(),
            message: "Context session is closed".into(),
            owner: None,
        };
        let shared = self.shared.upgrade().ok_or_else(unavailable)?;
        let _permit = shared
            .context_calls
            .try_acquire()
            .map_err(|_| CapabilityError::Context {
                code: "capacity".into(),
                message: "Pending context call limit reached".into(),
                owner: None,
            })?;
        let mut registry = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(CapabilityError::Cancelled),
            () = tokio::time::sleep_until(limits.deadline) => return Err(CapabilityError::Deadline),
            registry = shared.registry.lock() => registry,
        };
        check_caller(limits, &cancellation)?;
        if registry.closed {
            return Err(unavailable());
        }
        registry.check()?;
        debug_assert!(
            self.id == shared.root || registry.entries.contains_key(&self.id),
            "context callers must be admitted session participants"
        );
        if let Some(attachment) = &shared.attachment {
            let snapshot = attachment
                .coordination_snapshot()
                .await
                .map_err(persistence_error)?;
            registry.context =
                ContextRecords::restore(snapshot.context.into_iter().map(|record| {
                    cocoa_core::context::ContextRecord {
                        key: record.key,
                        owner: record.owner,
                        content: record.content,
                    }
                }));
        }
        if name == "context.set"
            && let Some(attachment) = &shared.attachment
        {
            let operation_record = operation_record.ok_or_else(|| {
                tool(
                    "operation_required",
                    "durable mutation requires a committed operation start",
                )
            })?;
            #[derive(Deserialize)]
            struct Arguments {
                key: String,
                content: cocoa_core::context::ContextContent,
            }
            let owner = arguments
                .get("key")
                .and_then(Value::as_str)
                .and_then(|key| registry.context.owner(key));
            if !host_value_fits(&arguments, limits.output_bytes) {
                return Err(crate::context::rejected(
                    "invalid_arguments",
                    "Context arguments exceed the call budget",
                    owner,
                ));
            }
            if arguments
                .get("content")
                .and_then(|content| content.get("value"))
                .is_none()
            {
                return Err(crate::context::rejected(
                    "invalid_arguments",
                    "Context content requires a value, including null",
                    owner,
                ));
            }
            let arguments: Arguments = serde_json::from_value(arguments).map_err(|_| {
                crate::context::rejected(
                    "invalid_arguments",
                    "Expected context.set(key, {type, value, labels?}) without owner metadata",
                    owner,
                )
            })?;
            crate::context::validate_key(&arguments.key, owner)?;
            crate::context::validate_labels(&arguments.content.labels, owner)?;
            if owner.is_some_and(|owner| owner != self.id) {
                return Err(crate::context::rejected(
                    "not_owner",
                    "Only the current owner may replace this record",
                    owner,
                ));
            }
            let value = serde_json::to_value(cocoa_core::context::ContextRecord {
                key: arguments.key.clone(),
                owner: self.id,
                content: arguments.content.clone(),
            })
            .map_err(|_| StoreError::Encoding)?;
            crate::context::fits(value, limits.output_bytes, owner)?;
            let store = attachment
                .agent_store(self.id, coordination_store_limits())
                .await
                .map_err(persistence_error)?;
            let outcome = store
                .set_context(
                    operation_record,
                    &arguments.key,
                    &arguments.content,
                    limits.output_bytes,
                )
                .await
                .map_err(|error| match error {
                    PersistenceError::ContextNotOwner => CapabilityError::Context {
                        code: "not_owner".into(),
                        message: "Only the current owner may replace this record".into(),
                        owner,
                    },
                    PersistenceError::Store(StoreError::RecordLimit) => CapabilityError::Context {
                        code: "record_limit".into(),
                        message: "Complete context record exceeds the byte limit".into(),
                        owner,
                    },
                    PersistenceError::Store(StoreError::Capacity) => CapabilityError::Context {
                        code: "capacity".into(),
                        message: "Session context capacity reached".into(),
                        owner,
                    },
                    error => persistence_error(error),
                })?;
            return outcome_value(outcome);
        }
        registry
            .context
            .call(self.id, name, arguments, limits, &cancellation)
    }
}

/// Optional overrides for a child's inherited model configuration.
#[derive(Clone, Debug, Default)]
pub struct AgentSelection {
    pub alias: Option<String>,
    pub model: Option<String>,
    pub thinking_level: Option<ThinkingLevel>,
}

/// Application-owned construction and cleanup of a child's private adapters.
/// Creation must settle partially constructed resources before returning an error.
/// The manager consumes each engine after its invocation, before publishing the
/// outcome. Retain any canonical evidence and environment resources that must
/// outlive execution separately, until `close` at session shutdown.
/// Cancellation must stop setup without abandoning native workers. `close` runs
/// only after engine shutdown and must never destroy externally owned resources.
#[async_trait]
pub trait AgentFactory: Send + Sync {
    /// Whether this session authorizes the configured alias.
    fn supports_alias(&self, alias: &str) -> bool;

    /// Validate overrides before admitting work, without provider requests.
    ///
    /// # Errors
    /// Reject unavailable aliases or unsupported model overrides.
    fn validate_selection(
        &self,
        default_alias: &str,
        selection: &AgentSelection,
    ) -> Result<(), CapabilityError> {
        if !self.supports_alias(selection.alias.as_deref().unwrap_or(default_alias)) {
            return Err(tool("alias_unavailable", "alias is not configured"));
        }
        if selection.model.is_some() || selection.thinking_level.is_some() {
            return Err(tool(
                "invalid_arguments",
                "model overrides are not supported",
            ));
        }
        Ok(())
    }

    /// Construct a private engine without starting its initial invocation.
    /// Bind its capabilities through `participant.bind` and its context assembler
    /// through `participant.bind_context` before its first request.
    /// The participant carries admitted session identity, not a caller-supplied name.
    /// A completed engine may be returned after cancellation; its cleanup belongs
    /// to the supervisor. Setup cancellation has the selected settlement plus
    /// shutdown window before the supervisor faults admission, while still owning
    /// and awaiting the constructor.
    ///
    /// # Errors
    /// Return sanitized setup errors, cancellation, or `Unsettled` when cleanup
    /// cannot be confirmed. Do not run provider work before returning the engine.
    async fn create(
        &self,
        participant: AgentParticipant,
        selection: &AgentSelection,
        limits: Limits,
        cancellation: CancellationToken,
    ) -> Result<Engine, CapabilityError>;

    /// Release application-owned resources after the corresponding engine joins.
    /// Must also accept a name whose creation failed and be safe to call again.
    ///
    /// # Errors
    /// Report cleanup uncertainty rather than silently discarding it.
    async fn close(&self, name: &str) -> Result<(), CapabilityError>;

    /// Release resources bound to this admitted identity. Factories which retain
    /// resources must override this method when display names can repeat.
    ///
    /// # Errors
    /// Report cleanup uncertainty, as with [`Self::close`].
    async fn close_agent(&self, _agent: AgentId, name: &str) -> Result<(), CapabilityError> {
        self.close(name).await
    }
}

/// Independent bounds on running assignments and retained agents/results.
#[derive(Clone, Copy, Debug)]
pub struct AgentLimits {
    pub active: usize,
    pub retained: usize,
}

/// Invalid host-selected limits, rejected before starting workers.
#[derive(Debug, thiserror::Error)]
#[error("agent limits require 1..=16 active, active..=32 retained, and valid engine limits")]
pub struct AgentSettingsError;

/// A process-local session. Accepted agents outlive caller cells and invocations.
/// The embedding application must call `shutdown` before releasing its resources.
#[derive(Clone)]
pub struct AgentManager {
    shared: Arc<Shared>,
}

impl AgentManager {
    /// Create an in-memory supervisor without starting agents. Display names may repeat.
    ///
    /// # Errors
    /// Reject invalid bounds or exhausted process-local session identities.
    pub fn new(
        limits: AgentLimits,
        child: Limits,
        factory: Arc<dyn AgentFactory>,
    ) -> Result<Self, AgentSettingsError> {
        if !(1..=16).contains(&limits.active)
            || limits.retained < limits.active
            || limits.retained > 32
            || EngineSettings::new(child).is_err()
            || child
                .settlement_timeout
                .checked_add(child.shutdown_timeout)
                .and_then(|duration| Instant::now().checked_add(duration))
                .is_none()
        {
            return Err(AgentSettingsError);
        }
        let session = SESSION_NEXT
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .map_err(|_| AgentSettingsError)?;
        let (changed, _) = watch::channel(());
        let (updates, _) = broadcast::channel(OBSERVATIONS_RETAINED);
        Ok(Self {
            shared: Arc::new(Shared {
                limits,
                child,
                factory,
                session,
                root: AgentId::try_from(1).map_err(|_| AgentSettingsError)?,
                attachment: None,
                changed,
                updates,
                calls: Semaphore::new(child.pending_operations),
                context_calls: Semaphore::new(64),
                registry: Mutex::new(Registry {
                    entries: BTreeMap::new(),
                    admissions: BTreeMap::new(),
                    next_agent: 2,
                    context: ContextRecords::default(),
                    recovery: None,
                    closed: false,
                    faulted: false,
                }),
                shutdown: Mutex::new(()),
            }),
        })
    }

    /// Restore durable identities, shared context and retained child results.
    /// This never creates child engines or restores old opaque handles. Recovery
    /// must reconcile interrupted children before attachment.
    ///
    /// # Errors
    /// Reject invalid settings, unavailable storage, or unreconciled children.
    pub async fn attach(
        limits: AgentLimits,
        child: Limits,
        factory: Arc<dyn AgentFactory>,
        attachment: SessionAttachment,
    ) -> Result<Self, CapabilityError> {
        let mut manager = Self::new(limits, child, factory)
            .map_err(|_| tool("invalid_settings", "invalid agent settings"))?;
        let snapshot = attachment
            .coordination_snapshot()
            .await
            .map_err(persistence_error)?;
        let shared = Arc::get_mut(&mut manager.shared).ok_or_else(unsettled)?;
        shared.root = attachment.root_agent();
        shared.attachment = Some(attachment);
        let registry = shared.registry.get_mut();
        registry.context = ContextRecords::restore(snapshot.context.into_iter().map(|record| {
            cocoa_core::context::ContextRecord {
                key: record.key,
                owner: record.owner,
                content: record.content,
            }
        }));
        for agent in snapshot.agents {
            if agent.id == shared.root {
                continue;
            }
            if registry.entries.len() >= limits.retained {
                return Err(tool("capacity", "retained agents exceed configured limits"));
            }
            let AgentMembership::Child { assignment, .. } = agent.membership else {
                return Err(tool("invalid_data", "child assignment is missing"));
            };
            let outcome = match agent
                .outcome
                .ok_or_else(|| tool("recovery_required", "child outcome requires recovery"))?
            {
                AgentOutcome::Completed { value } => value,
                AgentOutcome::Failed {
                    code,
                    message,
                    effects,
                } => {
                    json!({"name":agent.name,"status":"failed","error":{"code":code,"message":message,"effects":effects}})
                }
            };
            registry.entries.insert(
                agent.id,
                Entry {
                    id: agent.id,
                    name: agent.name,
                    prompt: assignment.prompt,
                    alias: assignment.alias,
                    selection: AgentSelection::default(),
                    reference: String::new(),
                    creator: Arc::new(()),
                    cancellation: CancellationToken::new(),
                    activity: outcome_activity(&outcome),
                    stage: Stage::Finished { outcome },
                    task: None,
                },
            );
        }
        Ok(manager)
    }

    /// Read detached retained results by permanent agent ID, including restored
    /// children which have no runtime or usable handle in this process.
    ///
    /// # Errors
    /// Reject closed or faulted coordination, as with live handle reads.
    pub async fn retained_results(&self) -> Result<Value, CapabilityError> {
        let registry = self.shared.registry.lock().await;
        registry.check()?;
        let mut results = Vec::with_capacity(registry.entries.len());
        for entry in registry.entries.values() {
            results.push(json!({"agent_id":i64::from(entry.id).to_string(),"name":entry.name,"alias":entry.alias,"outcome":entry.snapshot()?}));
        }
        Ok(Value::Array(results))
    }

    /// Check authoritative child state before switching away from this session.
    /// Starting, running and cancelling children remain active until settlement.
    ///
    /// # Errors
    /// Reject closed or faulted coordination and unconfirmed settlement or cleanup.
    pub async fn has_active_agents(&self) -> Result<bool, CapabilityError> {
        let registry = self.shared.registry.lock().await;
        registry.check()?;
        if registry
            .entries
            .values()
            .any(|entry| matches!(entry.stage, Stage::Unsettled { .. } | Stage::Closing))
        {
            return Err(unsettled());
        }
        Ok(registry.entries.values().any(|entry| entry.stage.active()))
    }

    /// Subscribe to live child observations and retained lifecycle state.
    #[must_use]
    pub fn subscribe(&self) -> AgentUpdates {
        AgentUpdates {
            shared: Arc::downgrade(&self.shared),
            receiver: self.shared.updates.subscribe(),
            lifecycle: BTreeMap::new(),
            paused: BTreeMap::new(),
            recovery: None,
        }
    }

    /// Compose management with an existing caller's capabilities. Each binding
    /// grants a distinct creator authority; names alone never recover its work.
    /// Root and children share context reads; only the root gets management.
    ///
    /// # Errors
    /// Reject an unavailable default alias before exposing any capability.
    pub fn bind(
        &self,
        alias: String,
        inner: Arc<dyn CapabilityHost>,
    ) -> Result<Arc<dyn CapabilityHost>, CapabilityError> {
        if !self.shared.factory.supports_alias(&alias) {
            return Err(tool("alias_unavailable", "alias is not configured"));
        }
        Ok(Arc::new(ManagedCapabilities {
            manager: self.clone(),
            inner: AgentParticipant {
                id: self.shared.root,
                name: "root".into(),
                shared: Arc::downgrade(&self.shared),
                events: None,
            }
            .bind(inner),
            creator: Arc::new(()),
            alias,
        }))
    }

    /// Add fresh session-wide shared data to the root's private context assembler.
    /// Required constraints and omission metadata count within its byte budget.
    /// Do not wrap an already session-bound assembler.
    #[must_use]
    pub fn bind_context(&self, inner: Arc<dyn ContextAssembler>) -> Arc<dyn ContextAssembler> {
        AgentParticipant {
            id: self.shared.root,
            name: "root".into(),
            shared: Arc::downgrade(&self.shared),
            events: None,
        }
        .bind_context(inner)
    }

    /// Revoke admission and handles, cancel active children and join all owned
    /// engines and adapter resources under one bounded cleanup window. A timed-out
    /// join remains supervised and a subsequent shutdown can retry joining it.
    ///
    /// # Errors
    /// Return `Unsettled` on worker panic, unconfirmed settlement or cleanup failure.
    pub async fn shutdown(&self) -> Result<(), CapabilityError> {
        let _shutdown = self.shared.shutdown.lock().await;
        let deadline = Instant::now()
            + self.shared.child.settlement_timeout
            + self.shared.child.shutdown_timeout;
        {
            let mut registry = self.shared.registry.lock().await;
            registry.closed = true;
            for entry in registry.entries.values_mut() {
                entry.cancel(StopReason::Session);
            }
            self.shared.changed.send_replace(());
        }
        let join_failed = self.join_tasks(deadline).await.is_err();
        {
            let mut registry = self.shared.registry.lock().await;
            for (id, entry) in &mut registry.entries {
                if entry.task.as_ref().is_some_and(|task| task.pending()) {
                    continue;
                }
                let engine = match std::mem::replace(&mut entry.stage, Stage::Closing) {
                    Stage::Unsettled { engine } => Some(engine),
                    Stage::Finished { .. } | Stage::SetupFailed { .. } | Stage::Closed => None,
                    stage => {
                        entry.stage = stage;
                        continue;
                    }
                };
                let shared = self.shared.clone();
                let factory = self.shared.factory.clone();
                let id = *id;
                let name = entry.name.clone();
                entry.task = Some(Arc::new(SupervisedTask::new(tokio::spawn(async move {
                    if let Some(engine) = engine {
                        (*engine).shutdown().await.map_err(|_| unsettled())?;
                    }
                    {
                        let mut registry = shared.registry.lock().await;
                        let entry = registry.entries.get_mut(&id).ok_or_else(unsettled)?;
                        entry.stage = Stage::Closed;
                    }
                    factory
                        .close_agent(id, &name)
                        .await
                        .map_err(|_| unsettled())
                }))));
            }
        }
        self.join_tasks(deadline).await?;
        let registry = self.shared.registry.lock().await;
        if join_failed || registry.faulted {
            Err(unsettled())
        } else {
            Ok(())
        }
    }

    async fn join_tasks(&self, deadline: Instant) -> Result<(), CapabilityError> {
        let tasks: Vec<_> = {
            let registry = self.shared.registry.lock().await;
            registry
                .entries
                .values()
                .filter_map(|entry| entry.task.clone())
                .collect()
        };
        let mut failed = false;
        for task in tasks {
            match tokio::time::timeout_at(deadline, task.join()).await {
                Ok(task_failed) => failed |= task_failed,
                Err(_) => failed = true,
            }
        }
        if failed {
            let mut registry = self.shared.registry.lock().await;
            registry.faulted = true;
            for entry in registry.entries.values_mut() {
                entry.finish(ActivityStatus::Unknown);
            }
            Err(unsettled())
        } else {
            Ok(())
        }
    }
}

struct Shared {
    limits: AgentLimits,
    child: Limits,
    factory: Arc<dyn AgentFactory>,
    session: u64,
    root: AgentId,
    attachment: Option<SessionAttachment>,
    calls: Semaphore,
    context_calls: Semaphore,
    registry: Mutex<Registry>,
    changed: watch::Sender<()>,
    updates: broadcast::Sender<Arc<AgentUpdate>>,
    shutdown: Mutex<()>,
}

impl Shared {
    fn update(&self, agent: AgentId, name: &str, kind: AgentUpdateKind) -> Arc<AgentUpdate> {
        Arc::new(AgentUpdate {
            session: self.session,
            agent,
            name: name.into(),
            kind,
        })
    }
}

struct Registry {
    entries: BTreeMap<AgentId, Entry>,
    admissions: BTreeMap<RecordId, (AgentId, Value)>,
    next_agent: i64,
    context: ContextRecords,
    recovery: Option<mpsc::Sender<AgentProviderFailure>>,
    closed: bool,
    faulted: bool,
}

impl Registry {
    fn fault(&mut self) {
        self.faulted = true;
        for entry in self.entries.values_mut() {
            entry.cancel(StopReason::Session);
        }
    }

    fn check(&self) -> Result<(), CapabilityError> {
        if self.faulted {
            return Err(unsettled());
        }
        if self.closed {
            return Err(tool("session_closed", "session is closed"));
        }
        Ok(())
    }

    fn entry(&self, reference: &str, creator: &Arc<()>) -> Result<&Entry, CapabilityError> {
        self.entries
            .values()
            .find(|entry| entry.reference == reference && Arc::ptr_eq(&entry.creator, creator))
            .ok_or_else(|| tool("invalid_handle", "handle does not belong to this caller"))
    }
}

struct Entry {
    id: AgentId,
    name: String,
    prompt: String,
    alias: String,
    selection: AgentSelection,
    reference: String,
    creator: Arc<()>,
    cancellation: CancellationToken,
    stage: Stage,
    activity: ActivityStatus,
    task: Option<Arc<SupervisedTask>>,
}

struct SupervisedTask {
    state: Mutex<TaskState>,
}

impl SupervisedTask {
    fn new(task: JoinHandle<Result<(), CapabilityError>>) -> Self {
        Self {
            state: Mutex::new(TaskState::Running(task)),
        }
    }

    fn pending(&self) -> bool {
        match self.state.try_lock() {
            Ok(state) => matches!(*state, TaskState::Running(_)),
            Err(_) => true,
        }
    }

    async fn join(&self) -> bool {
        let mut state = self.state.lock().await;
        match &mut *state {
            TaskState::Joined { failed } => *failed,
            TaskState::Running(task) => {
                let failed = !matches!(task.await, Ok(Ok(())));
                *state = TaskState::Joined { failed };
                failed
            }
        }
    }
}

enum TaskState {
    Running(JoinHandle<Result<(), CapabilityError>>),
    Joined { failed: bool },
}

impl Entry {
    fn finish(&mut self, status: ActivityStatus) {
        debug_assert_ne!(
            status,
            ActivityStatus::Running,
            "settlement must be terminal"
        );
        if self.activity == ActivityStatus::Running {
            self.activity = status;
        }
    }

    fn receipt(&self) -> Value {
        json!({"reference":self.reference,"name":self.name,"alias":self.alias,"agent_id":i64::from(self.id).to_string()})
    }

    fn snapshot(&self) -> Result<Value, CapabilityError> {
        match &self.stage {
            Stage::Finished { outcome } | Stage::SetupFailed { outcome } => Ok(outcome.clone()),
            Stage::Unsettled { .. } => Err(unsettled()),
            Stage::Closing | Stage::Closed => Err(tool("session_closed", "session is closed")),
            stage => Ok(json!({"name":self.name,"status":"pending","state":stage.name()})),
        }
    }

    fn cancel(&mut self, reason: StopReason) {
        if matches!(
            self.stage,
            Stage::Starting | Stage::Running | Stage::ProviderPaused { .. }
        ) {
            self.stage = Stage::Cancelling { reason };
            self.cancellation.cancel();
        }
    }
}

enum Stage {
    Starting,
    Running,
    ProviderPaused { reason: String },
    Cancelling { reason: StopReason },
    Finished { outcome: Value },
    SetupFailed { outcome: Value },
    Unsettled { engine: Box<Engine> },
    Closing,
    Closed,
}

impl Stage {
    fn active(&self) -> bool {
        matches!(
            self,
            Self::Starting | Self::Running | Self::ProviderPaused { .. } | Self::Cancelling { .. }
        )
    }

    fn name(&self) -> &str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::ProviderPaused { .. } => "provider_paused",
            Self::Cancelling { .. } => "cancelling",
            Self::Finished { outcome } | Self::SetupFailed { outcome } => {
                outcome["status"].as_str().unwrap_or("failed")
            }
            Self::Unsettled { .. } => "failed",
            Self::Closing | Self::Closed => "closed",
        }
    }
}

#[derive(Clone, Copy)]
enum StopReason {
    Requested,
    Deadline,
    Session,
}

struct ManagedCapabilities {
    manager: AgentManager,
    inner: Arc<dyn CapabilityHost>,
    creator: Arc<()>,
    alias: String,
}

#[async_trait]
impl CapabilityHost for ManagedCapabilities {
    fn bindings(&self) -> Vec<cocoa_core::capability::CapabilityBinding> {
        self.inner.bindings()
    }

    fn descriptions(&self) -> Vec<CapabilityDescription> {
        let mut descriptions = self.inner.descriptions();
        let process_wait = descriptions
            .iter()
            .find(|description| description.name == "cocoa.wait")
            .map(|description| description.description.clone())
            .unwrap_or_default();
        let process_cancel = descriptions
            .iter()
            .find(|description| description.name == "cocoa.cancel")
            .map(|description| description.description.clone())
            .unwrap_or_default();
        descriptions.retain(|description| {
            !matches!(description.name.as_str(), "cocoa.wait" | "cocoa.cancel")
        });
        descriptions.extend([
            CapabilityDescription {
                name: "cocoa.spawn".into(),
                description: "Call var worker = await cocoa.spawn({name:'worker',prompt:'Complete task instructions'}), with optional model, thinking_level and alias in that same object. Returns an opaque handle after acceptance, before setup or execution finishes. Without overrides the child uses the caller's current model and thinking level. model selects a model ID; thinking_level is one of none, minimal, low, medium, high, xhigh or max. An optional alias selects a configured profile before model and thinking_level overrides. Retain the handle in a var binding. Never print, stringify, clone or coerce the handle itself; print {name:worker.name,alias:worker.alias} for metadata. Agents have private REPLs, share local files, and cannot spawn children. Every new call admits separate work, even with the same name, prompt and alias. Names are nonunique display labels of 1..64 ASCII letters, digits, underscores or hyphens, without spaces. Do not call spawn to recover earlier work; retained results survive restart but opaque handles and child runtimes do not.".into(),
                parameters: json!({"type":"object","properties":{"name":{"type":"string","minLength":1,"maxLength":64,"pattern":"^[A-Za-z0-9_-]+$"},"prompt":{"type":"string","minLength":1},"alias":{"type":"string"},"model":{"type":"string","minLength":1},"thinking_level":{"type":"string","enum":["none","minimal","low","medium","high","xhigh","max"]}},"required":["name","prompt"],"additionalProperties":false}),
            },
            CapabilityDescription {
                name: "cocoa.wait".into(),
                description: format!("Call await cocoa.wait(handle, {{timeout_ms: 5000}}) or await cocoa.wait({{review: handle, tests: anotherHandle}}, {{timeout_ms: 5000}}). Pass agent or process handles directly, never IDs or names. The second argument is optional; default wait is 30000ms, capped by the caller's operation window. timeout_ms must be a u32, never null or undefined; zero polls. Single waits return one outcome; groups accept at most 32 members, share one deadline and preserve supplied alias keys. Completed agent outcomes are {{name,status:'completed',answer}}; unfinished agent outcomes are {{name,status:'pending',state}}, and settled failures/cancellation have status 'failed'/'cancelled'. A provider_paused state returns immediately and awaits the user's Retry/Cancel decision. Do other work or report the interruption instead of polling or spawning a replacement. Results remain retained; pending or interrupted waits do not cancel work. Print or save returned JSON, not handles. Process results: {process_wait}"),
                parameters: json!({"type":"object","description":"An opaque AgentHandle returned by cocoa.spawn, or a named record of such handles. Handles are not JSON values. Optional second JavaScript argument: {timeout_ms?: u32}."}),
            },
            CapabilityDescription {
                name: "cocoa.cancel".into(),
                description: format!("Call await cocoa.cancel(handle) with an opaque agent or process handle, never an ID or name. For agents returns {{name,state}}. Cancelling is not settled; use cocoa.wait(handle) for the outcome. Accepted work survives caller completion or cancellation. Model/command stalls, bounded cell execution, explicit cancellation or session closure can stop it; a progressing turn has no total time limit. Active capacity is released after settlement; permanent agent IDs and results remain retained. Process cancellation: {process_cancel}"),
                parameters: json!({"type":"object","description":"The opaque AgentHandle returned by cocoa.spawn, passed directly. Metadata copies and JSON values cannot replace it."}),
            },
        ]);
        descriptions
    }

    async fn call(
        &self,
        name: &str,
        arguments: Value,
        limits: WorkLimits,
        cancellation: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        if !management(name) {
            if !name.starts_with("context.") {
                self.manager.shared.registry.lock().await.check()?;
            }
            return self.inner.call(name, arguments, limits, cancellation).await;
        }
        check_caller(limits, &cancellation)?;
        let _permit = self
            .manager
            .shared
            .calls
            .try_acquire()
            .map_err(|_| tool("capacity", "pending management call limit reached"))?;
        if !host_value_fits(&arguments, limits.output_bytes) {
            return Err(tool(
                "invalid_arguments",
                "management arguments exceed the call budget",
            ));
        }
        match name {
            "cocoa.spawn" => self.spawn(arguments, limits, cancellation, None).await,
            "cocoa.wait" => self.wait(arguments, limits, cancellation).await,
            "cocoa.cancel"
                if arguments["reference"]
                    .as_str()
                    .is_some_and(|reference| reference.starts_with("process.")) =>
            {
                self.inner.call(name, arguments, limits, cancellation).await
            }
            "cocoa.cancel" => self.cancel(arguments, limits, cancellation).await,
            _ => unreachable!("management names are checked before dispatch"),
        }
    }

    async fn call_observed(
        &self,
        name: &str,
        arguments: Value,
        limits: WorkLimits,
        cancellation: CancellationToken,
        observer: CapabilityObserver,
    ) -> Result<Value, CapabilityError> {
        if management(name) {
            observer.input(&self.describe_input(name, &arguments));
            self.call(name, arguments, limits, cancellation).await
        } else {
            if !name.starts_with("context.") {
                self.manager.shared.registry.lock().await.check()?;
            }
            self.inner
                .call_observed(name, arguments, limits, cancellation, observer)
                .await
        }
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
        if name == "cocoa.spawn" {
            observer.input(&self.describe_input(name, &arguments));
            check_caller(limits, &cancellation)?;
            let _permit = self
                .manager
                .shared
                .calls
                .try_acquire()
                .map_err(|_| tool("capacity", "pending management call limit reached"))?;
            return self
                .spawn(arguments, limits, cancellation, Some(operation_record))
                .await;
        }
        if management(name) {
            return self
                .call_observed(name, arguments, limits, cancellation, observer)
                .await;
        }
        self.manager.shared.registry.lock().await.check()?;
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
        if !management(name) {
            return self.inner.describe_input(name, arguments);
        }
        CapabilityInput {
            title: name.into(),
            text: arguments
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("Named agent work")
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
        if management(name) && name != "cocoa.wait" {
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
        self.inner
            .observe_outcome(name, arguments, outcome, observer)
            .await;
    }
}

impl ManagedCapabilities {
    async fn spawn(
        &self,
        arguments: Value,
        limits: WorkLimits,
        cancellation: CancellationToken,
        operation_record: Option<RecordId>,
    ) -> Result<Value, CapabilityError> {
        if !host_value_fits(&arguments, limits.output_bytes) {
            return Err(tool(
                "invalid_arguments",
                "management arguments exceed the call budget",
            ));
        }
        #[derive(Deserialize)]
        struct Arguments {
            name: String,
            prompt: String,
            alias: Option<String>,
            model: Option<String>,
            thinking_level: Option<ThinkingLevel>,
        }
        for field in ["alias", "model", "thinking_level"] {
            if arguments.get(field).is_some_and(|value| !value.is_string()) {
                return Err(tool(
                    "invalid_arguments",
                    "model selection fields must be strings",
                ));
            }
        }
        let original_arguments = arguments.clone();
        let arguments: Arguments = serde_json::from_value(arguments).map_err(|_| {
            tool(
                "invalid_arguments",
                "expected name, prompt and optional alias, model and thinking_level",
            )
        })?;
        if !valid_name(&arguments.name)
            || arguments.prompt.trim().is_empty()
            || arguments.prompt.len() > self.manager.shared.child.input_bytes
            || arguments
                .model
                .as_ref()
                .is_some_and(|model| model.trim().is_empty())
        {
            return Err(tool("invalid_arguments", "invalid agent name or prompt"));
        }
        let accepted = Record {
            invocation: InvocationId(1),
            kind: RecordKind::InputAccepted {
                input: arguments.prompt.clone().into(),
            },
        };
        if serde_json::to_vec(&accepted)
            .map_err(|_| StoreError::Encoding)?
            .len()
            > self.manager.shared.child.record_bytes
        {
            return Err(tool(
                "invalid_arguments",
                "prompt exceeds the child evidence budget",
            ));
        }
        let mut registry = self.manager.shared.registry.lock().await;
        registry.check()?;
        check_caller(limits, &cancellation)?;
        let selection = AgentSelection {
            alias: arguments.alias,
            model: arguments.model,
            thinking_level: arguments.thinking_level,
        };
        self.manager
            .shared
            .factory
            .validate_selection(&self.alias, &selection)?;
        let alias = selection
            .alias
            .clone()
            .unwrap_or_else(|| self.alias.clone());
        if let Some(start) = operation_record
            && let Some((id, accepted_arguments)) = registry.admissions.get(&start)
            && let Some(entry) = registry.entries.get(id)
        {
            if accepted_arguments != &original_arguments
                || !Arc::ptr_eq(&entry.creator, &self.creator)
            {
                return Err(tool(
                    "invalid_operation",
                    "admission identity belongs to different work",
                ));
            }
            return fits(entry.receipt(), limits.output_bytes);
        }
        if let Some(attachment) = &self.manager.shared.attachment {
            let start = operation_record.ok_or_else(|| {
                tool(
                    "operation_required",
                    "durable admission requires a committed operation start",
                )
            })?;
            let store = attachment
                .root_store(coordination_store_limits())
                .map_err(persistence_error)?;
            if store
                .operation_receipt(start)
                .await
                .map_err(persistence_error)?
                .is_some()
            {
                let admission = store
                    .admit_agent(
                        start,
                        &arguments.name,
                        &AgentAssignment {
                            prompt: arguments.prompt.clone(),
                            alias: alias.clone(),
                        },
                        limits.output_bytes,
                    )
                    .await
                    .map_err(persistence_error)?;
                return outcome_value(admission.outcome);
            }
        }
        if registry.entries.len() >= self.manager.shared.limits.retained
            || registry
                .entries
                .values()
                .filter(|entry| entry.stage.active())
                .count()
                >= self.manager.shared.limits.active
        {
            return Err(tool(
                "capacity",
                "agent capacity reached; no work was queued",
            ));
        }
        let provisional = json!({"reference":format!("{}.{}",u64::MAX,i64::MAX),"name":arguments.name,"alias":alias,"agent_id":i64::MAX.to_string()});
        fits(provisional, limits.output_bytes)?;
        let envelope = serde_json::to_vec(&json!({"":{
            "name":arguments.name,"status":"completed","answer":""
        }}))
        .map_err(|_| tool("result_limit", "cannot reserve outcome envelope"))?
        .len();
        if limits.output_bytes.min(256 * 1024) < envelope.saturating_add(256) {
            return Err(tool("result_limit", "not enough room for a single outcome"));
        }
        let mut child = self.manager.shared.child;
        child.output_bytes = child
            .output_bytes
            .min(limits.output_bytes.min(256 * 1024) - envelope);
        let id = match &self.manager.shared.attachment {
            Some(attachment) => {
                let start = operation_record.ok_or_else(|| {
                    tool(
                        "operation_required",
                        "durable admission requires a committed operation start",
                    )
                })?;
                let store = attachment
                    .root_store(coordination_store_limits())
                    .map_err(persistence_error)?;
                let admission = store
                    .admit_agent(
                        start,
                        &arguments.name,
                        &AgentAssignment {
                            prompt: arguments.prompt.clone(),
                            alias: alias.clone(),
                        },
                        limits.output_bytes,
                    )
                    .await
                    .map_err(persistence_error)?;
                if !admission.created {
                    return outcome_value(admission.outcome);
                }
                admission.agent
            }
            None => {
                let id = AgentId::try_from(registry.next_agent)
                    .map_err(|_| tool("capacity", "agent identity exhausted"))?;
                registry.next_agent = registry
                    .next_agent
                    .checked_add(1)
                    .ok_or_else(|| tool("capacity", "agent identity exhausted"))?;
                id
            }
        };
        let reference = format!("{}.{}", self.manager.shared.session, i64::from(id));
        let entry = Entry {
            id,
            name: arguments.name.clone(),
            prompt: arguments.prompt,
            alias,
            selection,
            reference,
            creator: self.creator.clone(),
            cancellation: CancellationToken::new(),
            stage: Stage::Starting,
            activity: ActivityStatus::Running,
            task: None,
        };
        let receipt = entry.receipt();
        registry.entries.insert(id, entry);
        if let Some(start) = operation_record {
            registry.admissions.insert(start, (id, original_arguments));
        }
        let manager = self.manager.clone();
        let deadline = Instant::now() + child.stall_timeout;
        let worker = tokio::spawn(async move { manager.run(id, child, deadline).await });
        let shared = self.manager.shared.clone();
        let task = tokio::spawn(async move {
            match worker.await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(_)) | Err(_) => {
                    let mut registry = shared.registry.lock().await;
                    registry.fault();
                    if let Some(entry) = registry.entries.get_mut(&id) {
                        entry.finish(ActivityStatus::Unknown);
                    }
                    shared.changed.send_replace(());
                    Err(unsettled())
                }
            }
        });
        if let Some(entry) = registry.entries.get_mut(&id) {
            entry.task = Some(Arc::new(SupervisedTask::new(task)));
        }
        self.manager.shared.changed.send_replace(());
        Ok(receipt)
    }

    async fn wait(
        &self,
        arguments: Value,
        limits: WorkLimits,
        cancellation: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        let Some(handles) = arguments["handles"].as_object() else {
            return Err(tool("invalid_arguments", "expected a handle record"));
        };
        if handles.len() > 32 {
            return Err(tool("invalid_arguments", "too many wait members"));
        }
        let (processes, agents): (Map<_, _>, Map<_, _>) =
            handles.clone().into_iter().partition(|(_, reference)| {
                reference
                    .as_str()
                    .is_some_and(|reference| reference.starts_with("process."))
            });
        if processes.is_empty() {
            return self.wait_agents(arguments, limits, cancellation).await;
        }
        if agents.is_empty() {
            return self
                .inner
                .call("cocoa.wait", arguments, limits, cancellation)
                .await;
        }
        let mut process_arguments = arguments.clone();
        process_arguments["handles"] = Value::Object(processes);
        let mut agent_arguments = arguments;
        agent_arguments["handles"] = Value::Object(agents);
        let process_wait = self.inner.call(
            "cocoa.wait",
            process_arguments.clone(),
            limits,
            cancellation.child_token(),
        );
        let agent_wait = self.wait_agents(agent_arguments, limits, cancellation.child_token());
        tokio::pin!(process_wait, agent_wait);
        let (processes, agents) = tokio::select! {
            result = &mut process_wait => (result?, agent_wait.await?),
            result = &mut agent_wait => {
                let agents = result?;
                let paused = agents.as_object().ok_or_else(unsettled)?
                    .values().any(|agent| agent["state"] == "provider_paused");
                let processes = if paused {
                    process_arguments["timeout_ms"] = json!(0);
                    self.inner.call("cocoa.wait", process_arguments, limits, cancellation.child_token()).await?
                } else {
                    process_wait.await?
                };
                (processes, agents)
            }
        };
        let mut result = agents.as_object().cloned().ok_or_else(unsettled)?;
        result.extend(processes.as_object().ok_or_else(unsettled)?.clone());
        fits(Value::Object(result), limits.output_bytes)
    }

    async fn wait_agents(
        &self,
        arguments: Value,
        limits: WorkLimits,
        cancellation: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        #[derive(Deserialize)]
        struct Arguments {
            handles: BTreeMap<String, String>,
            timeout_ms: Option<u32>,
        }
        if arguments.get("timeout_ms").is_some_and(Value::is_null) {
            return Err(tool(
                "invalid_arguments",
                "timeout_ms must be a nonnegative u32",
            ));
        }
        let arguments: Arguments = serde_json::from_value(arguments).map_err(|_| {
            tool(
                "invalid_arguments",
                "expected a handle record and optional timeout_ms",
            )
        })?;
        if arguments.handles.len() > 32 {
            return Err(tool("invalid_arguments", "too many wait members"));
        }
        let deadline = match arguments.timeout_ms {
            Some(milliseconds) => (Instant::now() + Duration::from_millis(u64::from(milliseconds)))
                .min(limits.deadline),
            None => (Instant::now() + Duration::from_secs(30)).min(limits.deadline),
        };
        let mut changed = self.manager.shared.changed.subscribe();
        loop {
            if cancellation.is_cancelled() {
                return Err(CapabilityError::Cancelled);
            }
            let (snapshot, pending, paused) = {
                let registry = self.manager.shared.registry.lock().await;
                registry.check()?;
                let mut snapshots = BTreeMap::new();
                for reference in arguments.handles.values() {
                    if !snapshots.contains_key(reference) {
                        snapshots.insert(
                            reference.clone(),
                            registry.entry(reference, &self.creator)?.snapshot()?,
                        );
                    }
                }
                let mut result = Map::new();
                let mut pending = false;
                let mut paused = false;
                for (alias, reference) in &arguments.handles {
                    let value = snapshots
                        .get(reference)
                        .ok_or_else(|| tool("invalid_handle", "missing handle snapshot"))?;
                    pending |= value["status"] == "pending";
                    paused |= value["state"] == "provider_paused";
                    result.insert(alias.clone(), value.clone());
                }
                (Value::Object(result), pending, paused)
            };
            fits(snapshot.clone(), limits.output_bytes)?;
            if !pending || paused || Instant::now() >= deadline {
                return Ok(snapshot);
            }
            tokio::select! {
                biased;
                () = cancellation.cancelled() => return Err(CapabilityError::Cancelled),
                () = tokio::time::sleep_until(deadline) => {},
                result = changed.changed() => {
                    if result.is_err() { return Err(unsettled()); }
                },
            }
        }
    }

    async fn cancel(
        &self,
        arguments: Value,
        limits: WorkLimits,
        cancellation: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        #[derive(Deserialize)]
        struct Arguments {
            reference: String,
        }
        let arguments: Arguments = serde_json::from_value(arguments)
            .map_err(|_| tool("invalid_arguments", "expected an agent handle"))?;
        let mut registry = self.manager.shared.registry.lock().await;
        registry.check()?;
        check_caller(limits, &cancellation)?;
        let entry = registry.entry(&arguments.reference, &self.creator)?;
        fits(
            json!({"name":entry.name,"state":"cancelling"}),
            limits.output_bytes,
        )?;
        let id = entry.id;
        let entry = registry
            .entries
            .get_mut(&id)
            .ok_or_else(|| tool("invalid_handle", "agent is not retained"))?;
        entry.cancel(StopReason::Requested);
        let result = json!({"name":entry.name,"state":entry.stage.name()});
        self.manager.shared.changed.send_replace(());
        Ok(result)
    }
}

impl AgentManager {
    async fn run(
        &self,
        id: AgentId,
        limits: Limits,
        deadline: Instant,
    ) -> Result<(), CapabilityError> {
        let (name, cancellation) = {
            let mut registry = self.shared.registry.lock().await;
            let entry = registry.entries.get_mut(&id).ok_or_else(unsettled)?;
            if Instant::now() >= deadline {
                entry.cancel(StopReason::Deadline);
                self.shared.changed.send_replace(());
            }
            (entry.name.clone(), entry.cancellation.clone())
        };
        let (events, mut receiver) =
            EventPublisher::channel(CHILD_OBSERVATIONS_RETAINED, 4096).map_err(|_| unsettled())?;
        let work = self.execute(id, limits, deadline, cancellation, events);
        tokio::pin!(work);
        let mut tick = tokio::time::interval(Duration::from_millis(40));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut expired = false;
        loop {
            tokio::select! {
                biased;
                result = &mut work => {
                    self.forward(id, &name, &mut receiver);
                    let mut registry = self.shared.registry.lock().await;
                    let entry = registry.entries.get_mut(&id).ok_or_else(unsettled)?;
                    let status = match &entry.stage {
                        Stage::Finished { outcome } | Stage::SetupFailed { outcome } if result.is_ok() => {
                            outcome_activity(outcome)
                        }
                        _ => ActivityStatus::Unknown,
                    };
                    entry.finish(status);
                    return result;
                },
                () = tokio::time::sleep_until(deadline), if !expired => {
                    expired = true;
                    let mut registry = self.shared.registry.lock().await;
                    if let Some(entry) = registry.entries.get_mut(&id)
                        && matches!(entry.stage, Stage::Starting)
                    {
                        entry.cancel(StopReason::Deadline);
                    }
                    self.shared.changed.send_replace(());
                },
                _ = tick.tick() => self.forward(id, &name, &mut receiver),
            }
        }
    }

    fn forward(&self, id: AgentId, name: &str, receiver: &mut EventReceiver) {
        for _ in 0..=CHILD_OBSERVATIONS_RETAINED {
            let Some(observation) = receiver.recv().now_or_never() else {
                break;
            };
            if matches!(observation, Observation::Closed) {
                break;
            }
            let update = self
                .shared
                .update(id, name, AgentUpdateKind::Observation(observation));
            if self.shared.updates.send(update).is_err() {
                tracing::trace!("no live agent observation receiver");
            }
        }
    }

    async fn execute(
        &self,
        id: AgentId,
        limits: Limits,
        deadline: Instant,
        cancellation: CancellationToken,
        events: EventPublisher,
    ) -> Result<(), CapabilityError> {
        let (name, selection) = {
            let registry = self.shared.registry.lock().await;
            let entry = registry.entries.get(&id).ok_or_else(unsettled)?;
            (entry.name.clone(), entry.selection.clone())
        };
        let name = name.as_str();
        let created = self
            .create_engine(id, name, &selection, limits, cancellation.clone(), events)
            .await;
        let mut engine = match created {
            Ok(engine) => engine,
            Err(error) => {
                let mut registry = self.shared.registry.lock().await;
                let uncertain = matches!(error, CapabilityError::Unsettled { .. });
                if uncertain {
                    registry.fault();
                }
                let entry = registry.entries.get_mut(&id).ok_or_else(unsettled)?;
                if uncertain {
                    entry.finish(ActivityStatus::Unknown);
                }
                if Instant::now() >= deadline {
                    entry.cancel(StopReason::Deadline);
                }
                let outcome = match (&entry.stage, &error) {
                    (
                        Stage::Cancelling {
                            reason: StopReason::Deadline,
                        },
                        _,
                    ) => failed(name, "deadline", "agent deadline exhausted"),
                    (_, CapabilityError::Cancelled) => json!({"name":name,"status":"cancelled"}),
                    _ => failed(name, "setup_failed", "agent initialization failed"),
                };
                drop(registry);
                self.commit_outcome(id, &outcome).await?;
                let mut registry = self.shared.registry.lock().await;
                let entry = registry.entries.get_mut(&id).ok_or_else(unsettled)?;
                entry.stage = Stage::SetupFailed { outcome };
                self.shared.changed.send_replace(());
                return Ok(());
            }
        };
        let prompt = {
            let mut registry = self.shared.registry.lock().await;
            let entry = registry.entries.get_mut(&id).ok_or_else(unsettled)?;
            if Instant::now() >= deadline {
                entry.cancel(StopReason::Deadline);
            }
            if matches!(entry.stage, Stage::Starting) {
                entry.stage = Stage::Running;
            }
            self.shared.changed.send_replace(());
            entry.prompt.clone()
        };
        let recovery = self.shared.registry.lock().await.recovery.clone();
        let request = RunRequest {
            input: prompt.into(),
        };
        let result = if let Some(recovery) = recovery {
            let (_steering, mut messages) = mpsc::channel(1);
            let (failures, mut receiver) = mpsc::channel::<ProviderFailure>(1);
            let invocation =
                engine.run_with_recovery(request, cancellation.clone(), &mut messages, &failures);
            tokio::pin!(invocation);
            loop {
                let failure = tokio::select! {
                    biased;
                    result = &mut invocation => break result,
                    Some(failure) = receiver.recv() => failure,
                };
                {
                    let mut registry = self.shared.registry.lock().await;
                    let entry = registry.entries.get_mut(&id).ok_or_else(unsettled)?;
                    if matches!(entry.stage, Stage::Running) {
                        entry.stage = Stage::ProviderPaused {
                            reason: failure.error.recovery_description(),
                        };
                        self.shared.changed.send_replace(());
                    }
                }
                let (retry, decision) = oneshot::channel();
                let notice = AgentProviderFailure {
                    agent: id,
                    name: name.into(),
                    failure: ProviderFailure {
                        error: failure.error,
                        retry,
                    },
                    cancellation: cancellation.clone(),
                };
                tokio::select! {
                    biased;
                    result = &mut invocation => break result,
                    sent = recovery.send(notice) => {
                        if sent.is_err() { cancellation.cancel(); }
                    }
                }
                tokio::select! {
                    biased;
                    result = &mut invocation => break result,
                    decision = decision => {
                        if decision.is_ok() && !cancellation.is_cancelled() {
                            let mut registry = self.shared.registry.lock().await;
                            let entry = registry.entries.get_mut(&id).ok_or_else(unsettled)?;
                            if matches!(entry.stage, Stage::ProviderPaused { .. }) {
                                entry.stage = Stage::Running;
                                self.shared.changed.send_replace(());
                            }
                            if failure.retry.send(()).is_err() {
                                cancellation.cancel();
                            }
                        } else {
                            cancellation.cancel();
                        }
                    }
                }
            }
        } else {
            engine.run(request, cancellation).await
        };
        let mut registry = self.shared.registry.lock().await;
        let uncertain = matches!(
            &result,
            Err(RunError::Settlement { .. })
                | Err(RunError::Execution(ExecutionError::Capability(
                    CapabilityError::Unsettled { .. }
                )))
        );
        if uncertain {
            registry.fault();
        }
        let entry = registry.entries.get_mut(&id).ok_or_else(unsettled)?;
        if uncertain {
            entry.stage = Stage::Unsettled {
                engine: Box::new(engine),
            };
            self.shared.changed.send_replace(());
            return Ok(());
        }
        let outcome = match result {
            Ok(output) => json!({"name":name,"status":"completed","answer":output.answer}),
            Err(error) => match (&entry.stage, error) {
                (
                    Stage::Cancelling {
                        reason: StopReason::Deadline,
                    },
                    _,
                ) => failed(name, "deadline", "agent deadline exhausted"),
                (_, RunError::Cancelled | RunError::TurnCancelled { .. }) => {
                    json!({"name":name,"status":"cancelled"})
                }
                (_, error) => {
                    let (code, message) = run_failure(&error);
                    failed(name, code, message)
                }
            },
        };
        drop(registry);
        engine.shutdown().await.map_err(|_| unsettled())?;
        self.commit_outcome(id, &outcome).await?;
        let mut registry = self.shared.registry.lock().await;
        let entry = registry.entries.get_mut(&id).ok_or_else(unsettled)?;
        entry.stage = Stage::Finished { outcome };
        self.shared.changed.send_replace(());
        Ok(())
    }

    async fn create_engine(
        &self,
        id: AgentId,
        name: &str,
        selection: &AgentSelection,
        limits: Limits,
        cancellation: CancellationToken,
        events: EventPublisher,
    ) -> Result<Engine, CapabilityError> {
        if cancellation.is_cancelled() {
            return Err(CapabilityError::Cancelled);
        }
        let creation = self.shared.factory.create(
            AgentParticipant {
                id,
                name: name.into(),
                shared: Arc::downgrade(&self.shared),
                events: Some(events),
            },
            selection,
            limits,
            cancellation.clone(),
        );
        tokio::pin!(creation);
        tokio::select! {
            biased;
            result = &mut creation => return result,
            () = cancellation.cancelled() => {},
        }
        let deadline = Instant::now() + limits.settlement_timeout + limits.shutdown_timeout;
        match tokio::time::timeout_at(deadline, &mut creation).await {
            Ok(result) => result,
            Err(_) => {
                {
                    let mut registry = self.shared.registry.lock().await;
                    registry.fault();
                    if let Some(entry) = registry.entries.get_mut(&id) {
                        entry.finish(ActivityStatus::Unknown);
                    }
                }
                self.shared.changed.send_replace(());
                // Keep setup owned after faulting; dropping it could abandon native workers.
                creation.await
            }
        }
    }

    async fn commit_outcome(&self, id: AgentId, outcome: &Value) -> Result<(), CapabilityError> {
        if let Some(attachment) = &self.shared.attachment {
            attachment
                .complete_agent(
                    id,
                    &AgentOutcome::Completed {
                        value: outcome.clone(),
                    },
                )
                .await
                .map_err(persistence_error)?;
        }
        Ok(())
    }
}

fn management(name: &str) -> bool {
    matches!(name, "cocoa.spawn" | "cocoa.wait" | "cocoa.cancel")
}

fn coordination_store_limits() -> StoreLimits {
    StoreLimits {
        record_bytes: 16 * 1024 * 1024,
    }
}

fn persistence_error(error: PersistenceError) -> CapabilityError {
    match error {
        PersistenceError::Store(error) => CapabilityError::Store(error),
        error => CapabilityError::Store(StoreError::Backend {
            message: error.to_string(),
        }),
    }
}

fn outcome_value(outcome: OperationOutcome) -> Result<Value, CapabilityError> {
    match outcome {
        OperationOutcome::Completed { value } => Ok(value),
        OperationOutcome::Failed { error } => Err(error),
        _ => Err(unsettled()),
    }
}

fn check_caller(
    limits: WorkLimits,
    cancellation: &CancellationToken,
) -> Result<(), CapabilityError> {
    if cancellation.is_cancelled() {
        return Err(CapabilityError::Cancelled);
    }
    if Instant::now() >= limits.deadline {
        return Err(CapabilityError::Deadline);
    }
    Ok(())
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn fits(value: Value, bytes: usize) -> Result<Value, CapabilityError> {
    if host_value_fits(&value, bytes) {
        Ok(value)
    } else {
        Err(tool(
            "result_limit",
            "result exceeds this call's budget; retained outcomes remain available",
        ))
    }
}

fn tool(code: &str, message: &str) -> CapabilityError {
    CapabilityError::Tool {
        code: code.into(),
        message: message.into(),
        effects: Effects::None,
    }
}

fn unsettled() -> CapabilityError {
    CapabilityError::Unsettled {
        message: "agent supervision or owned cleanup could not be confirmed".into(),
    }
}

fn failed(name: &str, code: &str, message: &str) -> Value {
    json!({"name":name,"status":"failed","error":{"code":code,"message":message}})
}

fn outcome_activity(outcome: &Value) -> ActivityStatus {
    match outcome["status"].as_str() {
        Some("completed") => ActivityStatus::Completed,
        Some("cancelled") => ActivityStatus::Interrupted,
        Some("failed") if outcome["error"]["code"] == "deadline" => ActivityStatus::TimedOut,
        Some("failed") => ActivityStatus::Failed { exit_code: None },
        _ => ActivityStatus::Unknown,
    }
}

fn run_failure(error: &RunError) -> (&'static str, &'static str) {
    match error {
        RunError::Deadline { .. } => ("deadline", "agent deadline exhausted"),
        RunError::Model(_) => ("provider_failed", "agent provider failed"),
        RunError::InputLimit | RunError::OutputLimit | RunError::CellLimit | RunError::Budget => {
            ("budget", "agent budget exhausted")
        }
        RunError::Store(_) => ("store_failed", "agent evidence storage failed"),
        RunError::Context(_) => ("context_failed", "agent context assembly failed"),
        _ => ("execution_failed", "agent execution failed"),
    }
}
