//! Single foreground invocation control and validated, no-replay restoration.

use std::{collections::BTreeMap, sync::Arc, time::Duration};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::{
    capability::{
        ActivityOutput, CapabilityHost, CapabilityInput, OperationContext, OperationState,
    },
    compaction::{
        Checkpoint, CheckpointError, CheckpointLimits, CheckpointProposal, CompactionPolicy,
        SummaryAttempt,
    },
    context::{ContextAssembler, ContextError, InstructionState, PreparedContext},
    execution::{CellExecutor, CellResponse, ExecutionError, RuntimeState},
    model::{
        AccountingSummary, ModelAction, ModelError, ModelProvider, ModelRequest, ModelResponse,
        RequestAccounting, Usage, UserInput,
    },
    store::{
        ActiveSnapshot, FailureCause, InvocationId, InvocationOutcome, Record, RecordId,
        RecordKind, Store, StoreError, StoreSnapshot, append_bounded, encoded_size,
    },
};

/// Validate reconciled envelopes without restoring context or executing work.
/// Storage recovery can use this before publishing a repaired attachment.
///
/// # Errors
/// Return the same history, counter and instruction-state errors as
/// [`Engine::resume`], without its adapter IO or installation errors.
pub fn validate_resume_snapshot(snapshot: &StoreSnapshot) -> Result<(), ResumeError> {
    validate_history(snapshot, HistoryRequirement::Settled)?;
    Ok(())
}

/// Validate a recoverable history prefix before changing durable evidence.
/// Only the last invocation may be incomplete. Invalid references, facts after
/// a terminal and an unfinished invocation preceding another invocation reject.
/// This never infers an outcome, changes a counter or executes retained work.
///
/// # Errors
/// Reject malformed envelopes, references, counters and instruction state.
pub fn validate_recovery_snapshot(snapshot: &StoreSnapshot) -> Result<(), ResumeError> {
    validate_history(snapshot, HistoryRequirement::Recoverable)?;
    Ok(())
}

/// Explicit resource and deadline budgets. Model rounds, cells and capability
/// calls have no count limit.
/// No client defaults are selected here.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// UTF-8 text plus image data URLs, also subject to the accepted-record limit.
    pub input_bytes: usize,
    /// Encoded bytes per cell result or host result, not model responses.
    pub output_bytes: usize,
    /// Encoded bytes per canonical record, including its identity and variant.
    pub record_bytes: usize,
    /// Combined encoded history, capability and executor descriptions per call.
    pub context_bytes: usize,
    /// In-flight capability calls. Excess work is rejected, never queued.
    pub pending_operations: usize,
    /// Maximum inactivity between progress events and wall time of one cell.
    /// Model bytes and completed steps reset inactivity; this does not cap a turn.
    pub stall_timeout: Duration,
    /// Shared deadline for worker cleanup and operation reconciliation after error.
    pub settlement_timeout: Duration,
    /// Shared deadline for consuming shutdown's cleanup and reconciliation.
    pub shutdown_timeout: Duration,
}

/// Validated once, before assembling an engine.
pub struct EngineSettings {
    limits: Limits,
    compaction: Option<CompactionPolicy>,
}

impl EngineSettings {
    /// Validate every resource bound and deadline.
    ///
    /// # Errors
    /// Configured budgets and durations must be positive and representable by
    /// the runtime.
    pub fn new(limits: Limits) -> Result<Self, SettingsError> {
        if limits.input_bytes == 0
            || limits.output_bytes == 0
            || limits.record_bytes == 0
            || limits.context_bytes == 0
            || limits.pending_operations == 0
            || limits.pending_operations > tokio::sync::Semaphore::MAX_PERMITS
            || limits.stall_timeout.is_zero()
            || limits.settlement_timeout.is_zero()
            || limits.shutdown_timeout.is_zero()
        {
            return Err(SettingsError);
        }
        let now = std::time::Instant::now();
        if [
            limits.stall_timeout,
            limits.settlement_timeout,
            limits.shutdown_timeout,
        ]
        .into_iter()
        .any(|duration| now.checked_add(duration).is_none())
        {
            return Err(SettingsError);
        }
        Ok(Self {
            limits,
            compaction: None,
        })
    }

    /// Enable automatic foreground checkpoints under explicit byte budgets.
    /// The provider must support request accounting and read-only summarization;
    /// the store must support checkpoint publication. The current projection must
    /// fit `context_bytes` before provider accounting can establish eligibility;
    /// projection overflow fails without speculative summary generation.
    /// Token-window fit is not inferred from a byte budget or output-token cap.
    ///
    /// # Errors
    /// Reject unordered/zero budgets, invalid deadlines and oversized checkpoints.
    pub fn with_compaction(mut self, policy: CompactionPolicy) -> Result<Self, SettingsError> {
        let checkpoint_bytes =
            usize::try_from(policy.checkpoint.bytes).map_err(|_| SettingsError)?;
        if !policy.validate() || checkpoint_bytes > self.limits.record_bytes {
            return Err(SettingsError);
        }
        self.compaction = Some(policy);
        Ok(self)
    }

    /// Automatic checkpoint policy, if explicitly configured by the application.
    #[must_use]
    pub fn compaction(&self) -> Option<CompactionPolicy> {
        self.compaction
    }

    /// Validated limits supplied by the embedding application.
    #[must_use]
    pub fn limits(&self) -> Limits {
        self.limits
    }
}

/// Construction input rejected before any IO or execution.
#[derive(Debug, thiserror::Error)]
#[error("engine budgets and deadlines must be positive and representable")]
pub struct SettingsError;

/// Ready injected interfaces; only the private executor requires unique ownership.
pub struct EngineAdapters {
    pub model: Arc<dyn ModelProvider>,
    pub executor: Box<dyn CellExecutor>,
    pub capabilities: Arc<dyn CapabilityHost>,
    pub context: Arc<dyn ContextAssembler>,
    pub store: Arc<dyn Store>,
}

/// One user assignment, not a provider request. The engine admits at most four
/// images; each image also has its own validation bound and shares the input budget.
pub struct RunRequest {
    pub input: UserInput,
}

/// A failed model request awaiting an explicit user decision. Sending on `retry`
/// retries only that request. Cancel the invocation token to stop the turn safely.
/// Raw provider diagnostics may contain secrets and must not be displayed.
pub struct ProviderFailure {
    pub error: ModelError,
    pub retry: tokio::sync::oneshot::Sender<()>,
}

/// Complete authoritative answer. Event closure is not a substitute for this.
#[derive(Debug, PartialEq)]
pub struct RunOutput {
    pub invocation: InvocationId,
    pub answer: String,
    /// All requests made by this agent during the invocation, including compaction.
    /// Absent when any request's final usage is unknown. Children have separate stores.
    pub usage: Option<Usage>,
    pub accounting: AccountingSummary,
}

/// An invocation-tagged live notification, not operation evidence.
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    pub invocation: InvocationId,
    pub kind: EventKind,
}

/// Text is UTF-8 bounded independently of canonical record/feedback limits.
#[derive(Clone, Debug, PartialEq)]
pub enum EventKind {
    ModelStarted {
        round: u32,
    },
    /// Queued user input entered canonical history before the next model request.
    SteeringApplied,
    /// A rejected request will be retried at this monotonic deadline.
    ProviderRetryScheduled {
        status: u16,
        retry_at: std::time::Instant,
    },
    /// The retry delay ended and the provider is requesting a response again.
    ProviderRetryStarted,
    ModelFinished {
        executing: bool,
    },
    ReasoningStarted {
        item: u32,
    },
    ReasoningText {
        item: u32,
        text: String,
        truncated: bool,
    },
    ReasoningFinished {
        item: u32,
    },
    CellStarted {
        cell: usize,
        text: String,
        truncated: bool,
    },
    CellFinished {
        cell: usize,
        status: ActivityStatus,
        text: String,
        truncated: bool,
    },
    AssistantText {
        text: String,
        truncated: bool,
    },
    /// Display preview only; replace provisional text with the run result.
    AnswerCompleted {
        text: String,
        truncated: bool,
    },
    Printed {
        text: String,
        truncated: bool,
    },
    ExecutionState {
        running: bool,
    },
    CapabilityStarted {
        operation: crate::store::OperationId,
        name: String,
        truncated: bool,
    },
    CapabilityInput {
        operation: crate::store::OperationId,
        title: String,
        text: String,
        summary: Option<crate::capability::ActivitySummary>,
        truncated: bool,
    },
    CapabilityOutput {
        operation: crate::store::OperationId,
        output: ActivityOutput,
    },
    CapabilityFinished {
        operation: crate::store::OperationId,
        status: ActivityStatus,
    },
}

/// Presentation of retained work, never a promise of rollback or success
/// inferred from a later answer. Running work may outlive its accepting call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivityStatus {
    Running,
    Completed,
    /// Failure, with a numeric process exit code when retained evidence has one.
    /// Use `Signalled` or `TimedOut` when that failure cause is known.
    Failed {
        exit_code: Option<i32>,
    },
    Signalled {
        signal: i32,
    },
    TimedOut,
    Interrupted,
    Deferred,
    Unknown,
}

/// Publisher with no awaited callbacks. Clone only when another owner is intended
/// to keep the engine-lifetime observation channel open.
#[derive(Clone, Debug)]
pub struct EventPublisher {
    sender: broadcast::Sender<Event>,
    payload_bytes: usize,
}

impl EventPublisher {
    /// Create a bounded Tokio broadcast channel. Tokio rounds capacity up to a
    /// power of two; at most that many events, each with bounded text, are held.
    ///
    /// # Errors
    /// Reject zero limits and capacities above Tokio's representable range.
    pub fn channel(
        capacity: usize,
        payload_bytes: usize,
    ) -> Result<(Self, EventReceiver), EventSettingsError> {
        if capacity == 0 || capacity > usize::MAX / 2 || payload_bytes == 0 {
            return Err(EventSettingsError);
        }
        let (sender, receiver) = broadcast::channel(capacity);
        Ok((
            Self {
                sender,
                payload_bytes,
            },
            EventReceiver { receiver },
        ))
    }

    /// Subscribe to future observations only; no canonical replay is implied.
    #[must_use]
    pub fn subscribe(&self) -> EventReceiver {
        EventReceiver {
            receiver: self.sender.subscribe(),
        }
    }

    fn publish(&self, event: Event) {
        match self.sender.send(event) {
            Ok(_) => {}
            Err(_) => tracing::trace!("no live observation receiver"),
        }
    }
}

/// Invalid observer bounds, independent of engine settings.
#[derive(Debug, thiserror::Error)]
#[error("observation capacity and payload limit must be positive and representable")]
pub struct EventSettingsError;

/// Narrow receiver that makes missed notifications and closure explicit.
pub struct EventReceiver {
    receiver: broadcast::Receiver<Event>,
}

impl EventReceiver {
    /// Wait for one event, explicit lag, or channel closure. Closure is not run success.
    pub async fn recv(&mut self) -> Observation {
        match self.receiver.recv().await {
            Ok(event) => Observation::Event(event),
            Err(broadcast::error::RecvError::Lagged(missed)) => Observation::Lagged { missed },
            Err(broadcast::error::RecvError::Closed) => Observation::Closed,
        }
    }
}

/// Loss affects only provisional live display.
#[derive(Debug, PartialEq)]
pub enum Observation {
    Event(Event),
    Lagged { missed: u64 },
    Closed,
}

/// Scoped live output for an invocation; safe to omit entirely.
#[derive(Clone, Debug)]
pub struct ObservationSink {
    pub(crate) invocation: InvocationId,
    pub(crate) publisher: Option<EventPublisher>,
    progress: Option<tokio::sync::watch::Sender<tokio::time::Instant>>,
}

impl ObservationSink {
    /// Record progress, including nonempty model-stream bytes that contain no
    /// display text. This remains active when live UI delivery is disabled.
    pub fn progress(&self) {
        if let Some(progress) = &self.progress {
            progress.send_replace(tokio::time::Instant::now());
        }
    }

    /// Display a server-directed retry without recording a completed response.
    /// Providers must keep reporting progress during bounded retry waits so the
    /// engine does not treat the intentional delay as a stalled stream.
    pub fn provider_retry_scheduled(&self, status: u16, retry_at: std::time::Instant) {
        self.emit(EventKind::ProviderRetryScheduled { status, retry_at });
    }

    /// Clear the retry countdown when another transport attempt starts.
    pub fn provider_retry_started(&self) {
        self.emit(EventKind::ProviderRetryStarted);
    }

    /// Emit provisional assistant text without blocking or changing the result.
    pub fn assistant_text(&self, text: &str) {
        self.text(text, |text, truncated| EventKind::AssistantText {
            text,
            truncated,
        });
    }

    /// Start a provider-owned reasoning item within the current model round.
    pub fn reasoning_started(&self, item: u32) {
        self.emit(EventKind::ReasoningStarted { item });
    }

    /// Display only provider-returned reasoning text or summaries, never opaque
    /// continuation data. The item identity is scoped to the current model round.
    pub fn reasoning_text(&self, item: u32, text: &str) {
        self.text(text, |text, truncated| EventKind::ReasoningText {
            item,
            text,
            truncated,
        });
    }

    /// End an item independently of summary-part or request completion.
    pub fn reasoning_finished(&self, item: u32) {
        self.emit(EventKind::ReasoningFinished { item });
    }

    /// Emit printed text. Adapters must also return required execution feedback
    /// in their canonical result, not recover it from this notification.
    pub fn printed(&self, text: &str) {
        self.text(text, |text, truncated| EventKind::Printed {
            text,
            truncated,
        });
    }

    pub(crate) fn emit(&self, kind: EventKind) {
        self.progress();
        if let Some(publisher) = &self.publisher {
            publisher.publish(Event {
                invocation: self.invocation,
                kind,
            });
        }
    }

    pub(crate) fn text(&self, text: &str, kind: impl FnOnce(String, bool) -> EventKind) {
        if !text.is_empty() {
            self.progress();
        }
        if let Some(publisher) = &self.publisher {
            let (text, truncated) = preview_text(text, publisher.payload_bytes);
            self.emit(kind(text, truncated));
        }
    }

    pub(crate) fn capability_input(
        &self,
        operation: crate::store::OperationId,
        input: &CapabilityInput,
    ) {
        if let Some(publisher) = &self.publisher {
            self.emit(capability_input(operation, input, publisher.payload_bytes));
        }
    }

    pub(crate) fn capability_output(
        &self,
        operation: crate::store::OperationId,
        output: ActivityOutput,
    ) {
        if let Some(publisher) = &self.publisher {
            self.emit(capability_output(
                operation,
                output,
                publisher.payload_bytes,
            ));
        }
    }
}

/// One agent's conversation and private executor.
pub struct Engine {
    settings: EngineSettings,
    adapters: EngineAdapters,
    next: u64,
    events: Option<EventPublisher>,
    state: EngineState,
    cells_started: usize,
    requires_empty_store: bool,
    runtime_restarted: bool,
    compaction_attempt: Option<(RecordId, Option<RecordId>)>,
    accounting: RequestAccounting,
}

enum EngineState {
    Ready,
    Unsettled {
        invocation: InvocationId,
        cancellation: CancellationToken,
    },
    Stopped,
}

impl Engine {
    /// Assemble ready adapters without IO or worker startup.
    /// The store namespace must be empty and reserved for this engine's lifetime;
    /// a fresh engine does not resume a nonempty store. External readers may share
    /// the store for inspection. Only the executor requires exclusive ownership.
    #[must_use]
    pub fn new(
        settings: EngineSettings,
        adapters: EngineAdapters,
        events: Option<EventPublisher>,
    ) -> Self {
        Self {
            settings,
            adapters,
            next: 0,
            events,
            state: EngineState::Ready,
            cells_started: 0,
            requires_empty_store: true,
            runtime_restarted: false,
            compaction_attempt: None,
            accounting: RequestAccounting::default(),
        }
    }

    /// Restore reconciled history with a fresh, exclusively owned executor.
    /// The caller must supply a new runtime and coding environment with no old
    /// bindings, observations, handles or process authority. This constructor
    /// restores instruction policy only; it never calls a model or executes work.
    /// Application recovery must first resolve incomplete canonical evidence.
    ///
    /// # Errors
    /// Reject invalid envelopes, counters, references, unfinished invocations or
    /// incompatible instruction state. Propagate load and installation failures.
    /// Failed construction shuts down the executor under the configured deadline;
    /// an unconfirmed shutdown retains both errors. Await construction to completion.
    pub async fn resume(
        settings: EngineSettings,
        adapters: EngineAdapters,
        events: Option<EventPublisher>,
    ) -> Result<Self, ResumeError> {
        let mut engine = Self::new(settings, adapters, events);
        let restored = async {
            let snapshot = engine.adapters.store.load().await?;
            let next = validate_history(&snapshot, HistoryRequirement::Settled)?;
            if let Some(state) = &snapshot.instruction_state {
                engine.adapters.context.restore(state).await?;
            }
            Ok::<_, ResumeError>(next)
        }
        .await;
        match restored {
            Ok(next) => {
                engine.next = next;
                engine.requires_empty_store = false;
                engine.runtime_restarted = true;
                Ok(engine)
            }
            Err(cause) => match engine.shutdown().await {
                Ok(()) => Err(cause),
                Err(cleanup) => Err(ResumeError::Settlement {
                    cause: Box::new(cause),
                    cleanup,
                }),
            },
        }
    }

    /// Publish a host-prepared checkpoint between invocations without touching the
    /// private executor or calling a summarizer. Sources must be available in the
    /// active store view. Coverage and complete call/result groups use original IDs.
    ///
    /// # Errors
    /// Reject invalid or nonreducing proposals and unsupported stores/assemblers.
    /// Rejection preserves the previous projection. An unknown publication outcome
    /// or a dropped publication future stops this engine pending reconciliation.
    pub async fn compact(
        &mut self,
        through: RecordId,
        proposal: CheckpointProposal,
        limits: CheckpointLimits,
    ) -> Result<RecordId, CompactionError> {
        if !matches!(self.state, EngineState::Ready) || self.requires_empty_store {
            return Err(CompactionError::NotReady);
        }
        let snapshot = self.adapters.store.load_active().await?;
        let checkpoint = Checkpoint::prepare_active(&snapshot, through, proposal, limits)?;
        let descriptions = self.adapters.capabilities.descriptions();
        let previous = if let Some(previous) = snapshot.checkpoint() {
            self.adapters
                .context
                .prepare_compacted(
                    snapshot.records().to_vec(),
                    descriptions.clone(),
                    usize::MAX,
                    &previous.checkpoint,
                )
                .await?
        } else {
            self.adapters
                .context
                .prepare(
                    snapshot.records().to_vec(),
                    descriptions.clone(),
                    usize::MAX,
                )
                .await?
        };
        let executor = self
            .adapters
            .executor
            .description(crate::execution::WorkLimits {
                output_bytes: self.settings.limits.output_bytes,
                deadline: tokio::time::Instant::now()
                    .checked_add(self.settings.limits.stall_timeout)
                    .ok_or(ContextError::Limit)?,
            })?;
        let notice = if self.runtime_restarted {
            RUNTIME_RESTART_NOTICE
        } else {
            RUNTIME_COMPACTION_NOTICE
        };
        let notice_bytes = encoded_size(&notice)?;
        let executor_bytes = encoded_size(&executor)?;
        if executor_bytes > 4096 {
            return Err(ContextError::Limit.into());
        }
        let budget = self
            .settings
            .limits
            .context_bytes
            .checked_sub(encoded_size(&executor)?)
            .and_then(|bytes| bytes.checked_sub(notice_bytes))
            .ok_or(ContextError::Limit)?;
        let prepared = self
            .adapters
            .context
            .prepare_compacted(
                snapshot.records().to_vec(),
                descriptions,
                budget,
                &checkpoint,
            )
            .await?;
        if prepared.context.encoded_size()? >= previous.context.encoded_size()? {
            return Err(CheckpointError::NoProgress.into());
        }
        self.state = EngineState::Stopped;
        match self.adapters.store.publish_checkpoint(&checkpoint).await {
            Ok(id) => {
                self.state = EngineState::Ready;
                Ok(id)
            }
            Err(error) => {
                if !matches!(error, StoreError::Backend { .. }) {
                    self.state = EngineState::Ready;
                }
                Err(error.into())
            }
        }
    }

    /// Borrow the agent exclusively for one invocation.
    ///
    /// # Errors
    /// Oversized input and already-cancelled requests are rejected before admission
    /// and leave the engine usable. Requested cancellation leaves it usable only
    /// after bounded worker settlement, safe runtime preparation and history repair.
    /// Other admitted failures stop it and require consuming shutdown. Dropping this
    /// future also prevents reuse; it does not settle work.
    pub async fn run(
        &mut self,
        request: RunRequest,
        cancellation: CancellationToken,
    ) -> Result<RunOutput, RunError> {
        let (_, mut steering) = tokio::sync::mpsc::channel(1);
        self.run_with_steering(request, cancellation, &mut steering)
            .await
    }

    /// Run one invocation with additional user messages from a bounded channel.
    /// Messages enter history in FIFO order before the next model request, after
    /// all cells from the preceding response settle. Pending input extends a final
    /// response into another round. It does not cancel work or reset turn budgets.
    /// Unconsumed messages remain in the receiver on cancellation or failure;
    /// the caller must report or recover them, never silently replay them.
    ///
    /// # Errors
    /// Return the same failures as `run`. Each steering message must satisfy the
    /// input, image and record limits; invalid messages fail the invocation.
    pub async fn run_with_steering(
        &mut self,
        request: RunRequest,
        cancellation: CancellationToken,
        steering: &mut tokio::sync::mpsc::Receiver<UserInput>,
    ) -> Result<RunOutput, RunError> {
        self.run_invocation(request, cancellation, steering, None)
            .await
    }

    /// Run with manual recovery for every provider failure except cancellation.
    /// Each failure waits on the bounded channel for explicit retry or invocation
    /// cancellation, without consuming another model round or repeating cells.
    /// Waiting for a decision does not count toward the stream stall timeout.
    ///
    /// # Errors
    /// Return the same failures as `run_with_steering`. Closing the failure channel
    /// propagates the provider failure; dropping its retry sender ends the request.
    pub async fn run_with_recovery(
        &mut self,
        request: RunRequest,
        cancellation: CancellationToken,
        steering: &mut tokio::sync::mpsc::Receiver<UserInput>,
        recovery: &tokio::sync::mpsc::Sender<ProviderFailure>,
    ) -> Result<RunOutput, RunError> {
        self.run_invocation(request, cancellation, steering, Some(recovery))
            .await
    }

    /// Consume the agent and await owned cleanup, never shared environment teardown.
    ///
    /// # Errors
    /// Report unresolved work, deadline expiry and adapter failures.
    pub async fn shutdown(mut self) -> Result<(), ShutdownError> {
        self.settle(self.settings.limits.shutdown_timeout, None)
            .await
    }

    /// Recover retained capability notifications after invocation settlement,
    /// including commands accepted by earlier invocations that can still run.
    /// This does not execute or republish operations.
    /// The publisher's payload limit bounds each text, but retained history
    /// determines the event count without a total snapshot bound.
    ///
    /// # Errors
    /// Propagate store inspection failures. Call only after `run` returns.
    pub async fn activity_snapshot(&self) -> Result<Vec<Event>, StoreError> {
        let Some(publisher) = &self.events else {
            return Ok(Vec::new());
        };
        let mut events = Vec::new();
        let records = self.adapters.store.load().await?.into_records();
        let mut inputs = BTreeMap::new();
        for record in &records {
            let kind = match &record.kind {
                RecordKind::OperationStarted {
                    operation,
                    name,
                    arguments,
                } => {
                    inputs.insert((record.invocation.0, *operation), (name, arguments));
                    let input = self.adapters.capabilities.describe_input(name, arguments);
                    let (name, truncated) = preview_text(name, publisher.payload_bytes);
                    events.push(Event {
                        invocation: record.invocation,
                        kind: EventKind::CapabilityStarted {
                            operation: *operation,
                            name,
                            truncated,
                        },
                    });
                    capability_input(*operation, &input, publisher.payload_bytes)
                }
                RecordKind::OperationFinished { operation, outcome } => {
                    if let Some((name, arguments)) =
                        inputs.remove(&(record.invocation.0, *operation))
                        && let Some(output) = self
                            .adapters
                            .capabilities
                            .describe_output(name, arguments, outcome)
                    {
                        events.push(Event {
                            invocation: record.invocation,
                            kind: capability_output(*operation, output, publisher.payload_bytes),
                        });
                    }
                    EventKind::CapabilityFinished {
                        operation: *operation,
                        status: self.adapters.capabilities.activity_status(outcome),
                    }
                }
                _ => continue,
            };
            events.push(Event {
                invocation: record.invocation,
                kind,
            });
        }
        Ok(events)
    }

    async fn run_invocation(
        &mut self,
        request: RunRequest,
        cancellation: CancellationToken,
        steering: &mut tokio::sync::mpsc::Receiver<UserInput>,
        recovery: Option<&tokio::sync::mpsc::Sender<ProviderFailure>>,
    ) -> Result<RunOutput, RunError> {
        if !matches!(self.state, EngineState::Ready) {
            return Err(RunError::Stopped);
        }
        if request.input.bytes() > self.settings.limits.input_bytes
            || request.input.images().len() > 4
        {
            return Err(RunError::InputLimit);
        }
        if cancellation.is_cancelled() {
            return Err(RunError::Cancelled);
        }
        if self.requires_empty_store {
            let snapshot = self.adapters.store.load().await?;
            if !snapshot.records.is_empty()
                || snapshot.instruction_state.is_some()
                || !snapshot.checkpoints.is_empty()
            {
                return Err(RunError::Store(StoreError::InvalidRecord));
            }
            self.requires_empty_store = false;
        }
        let invocation = InvocationId(
            self.next
                .checked_add(1)
                .filter(|&next| i64::try_from(next).is_ok())
                .ok_or(RunError::Budget)?,
        );
        let accepted = Record {
            invocation,
            kind: RecordKind::InputAccepted {
                input: request.input,
            },
        };
        if encoded_size(&accepted)? > self.settings.limits.record_bytes {
            return Err(RunError::InputLimit);
        }
        self.next = invocation.0;
        let work = cancellation.child_token();
        self.state = EngineState::Unsettled {
            invocation,
            cancellation: work.clone(),
        };
        self.cells_started = 0;
        self.accounting = RequestAccounting::new(self.adapters.store.request_recorder(invocation));
        let mut settlement_deadline = None;
        let mut settled = true;
        let settlement_timeout = self.settings.limits.settlement_timeout;
        let stall_timeout = self.settings.limits.stall_timeout;
        let (progress, activity) = tokio::sync::watch::channel(tokio::time::Instant::now());
        let observations = ObservationSink {
            invocation,
            publisher: self.events.clone(),
            progress: Some(progress),
        };
        let result = {
            let invocation_work = self.run_inner(
                invocation,
                accepted,
                work.clone(),
                observations,
                steering,
                recovery,
            );
            tokio::pin!(invocation_work);
            let stalled = async {
                loop {
                    let deadline = *activity.borrow() + stall_timeout;
                    tokio::time::sleep_until(deadline).await;
                    if tokio::time::Instant::now() >= *activity.borrow() + stall_timeout {
                        break;
                    }
                }
            };
            tokio::select! {
                biased;
                () = cancellation.cancelled() => {
                    work.cancel();
                    let deadline = tokio::time::Instant::now() + settlement_timeout;
                    settlement_deadline = Some(deadline);
                    match tokio::time::timeout_at(deadline, &mut invocation_work).await {
                        Ok(Ok(output)) => Ok(output),
                        Ok(Err(error)) => Err(error),
                        Err(_) => { settled = false; Err(RunError::Cancelled) }
                    }
                },
                result = &mut invocation_work => result,
                () = stalled => Err(RunError::Deadline {
                    cause: FailureCause::Stalled {
                        stall_timeout_ms: stall_timeout_ms(stall_timeout),
                    },
                }),
            }
        };
        ObservationSink {
            invocation,
            publisher: self.events.clone(),
            progress: None,
        }
        .emit(EventKind::ExecutionState { running: false });
        match result {
            Ok(output) => {
                self.state = EngineState::Ready;
                Ok(output)
            }
            Err(cause) => {
                work.cancel();
                if settled && cancellation.is_cancelled() && is_cancellation(&cause) {
                    let deadline = settlement_deadline
                        .unwrap_or_else(|| tokio::time::Instant::now() + settlement_timeout);
                    let continued = tokio::time::timeout_at(deadline, async {
                        let runtime = if self.cells_started == 0 {
                            RuntimeState::Preserved
                        } else {
                            self.adapters.executor.cancel_turn().await?
                        };
                        reconcile(
                            self.adapters.store.as_ref(),
                            Some(invocation),
                            self.settings.limits.record_bytes,
                        )
                        .await?;
                        self.complete_cancelled_cells(invocation).await?;
                        append_bounded(
                            self.adapters.store.as_ref(),
                            &Record {
                                invocation,
                                kind: RecordKind::TurnCancelled { runtime },
                            },
                            self.settings.limits.record_bytes,
                        )
                        .await?;
                        self.finish_invocation(invocation, InvocationOutcome::Cancelled)
                            .await?;
                        Ok::<_, RunError>(runtime)
                    })
                    .await;
                    match continued {
                        Ok(Ok(runtime)) => {
                            self.state = EngineState::Ready;
                            return Err(RunError::TurnCancelled { runtime });
                        }
                        Ok(Err(error)) => {
                            return match self
                                .settle(settlement_timeout, deadline_cause(&error))
                                .await
                            {
                                Ok(()) => Err(error),
                                Err(cleanup) => Err(RunError::Settlement {
                                    cause: Box::new(error),
                                    cleanup,
                                }),
                            };
                        }
                        Err(_) => settled = false,
                    }
                }
                match self
                    .settle(
                        self.settings.limits.settlement_timeout,
                        deadline_cause(&cause),
                    )
                    .await
                {
                    Ok(()) if settled => Err(cause),
                    Ok(()) => Err(RunError::Settlement {
                        cause: Box::new(cause),
                        cleanup: ShutdownError {
                            workers: WorkerSettlement::Deadline,
                            records: RecordSettlement::Settled,
                        },
                    }),
                    Err(cleanup) => Err(RunError::Settlement {
                        cause: Box::new(cause),
                        cleanup,
                    }),
                }
            }
        }
    }

    async fn prepare_model_request(
        &mut self,
        executor: String,
        operations: &OperationContext,
        cancellation: &CancellationToken,
    ) -> Result<(ModelRequest, Option<InstructionState>), RunError> {
        let snapshot = self.adapters.store.load_active().await?;
        let previous = self
            .prepare_projection(&snapshot, None, &executor, operations)
            .await?;
        let Some(policy) = self.settings.compaction else {
            return Ok(previous);
        };
        let Some(previous_bytes) = self.adapters.model.request_bytes(&previous.0)? else {
            return Ok(previous);
        };
        if previous_bytes < policy.trigger_bytes {
            return Ok(previous);
        }
        let result = self
            .automatic_checkpoint(
                &snapshot,
                &previous.0,
                operations,
                cancellation,
                policy,
                previous_bytes,
            )
            .await;
        match result {
            Ok(prepared) => Ok(prepared),
            Err(error) => {
                if matches!(error, CompactionError::Store(_)) {
                    return Err(error.into());
                }
                if cancellation.is_cancelled() {
                    return Err(RunError::Cancelled);
                }
                if previous_bytes <= policy.request_bytes {
                    tracing::warn!(
                        "automatic checkpoint rejected; continuing unchanged request within budget"
                    );
                    return Ok(previous);
                }
                Err(error.into())
            }
        }
    }

    async fn automatic_checkpoint(
        &mut self,
        snapshot: &ActiveSnapshot,
        previous: &ModelRequest,
        operations: &OperationContext,
        cancellation: &CancellationToken,
        policy: CompactionPolicy,
        previous_bytes: usize,
    ) -> Result<(ModelRequest, Option<InstructionState>), CompactionError> {
        let checkpoint_bytes =
            usize::try_from(policy.checkpoint.bytes).map_err(|_| CheckpointError::Limits)?;
        let through = compaction_boundary(
            snapshot,
            policy.request_bytes.saturating_sub(checkpoint_bytes),
        )?;
        let key = (through, snapshot.checkpoint().map(|stored| stored.id));
        if self.compaction_attempt == Some(key) {
            return Err(CheckpointError::NoProgress.into());
        }
        self.compaction_attempt = Some(key);
        let attempt = SummaryAttempt::prepare_active(snapshot, through, policy)?;
        let output = {
            let summary = tokio::time::timeout_at(
                attempt.request().deadline,
                self.adapters.model.summarize_accounted(
                    attempt.request().clone(),
                    previous,
                    policy.summary_input_bytes,
                    cancellation.child_token(),
                    self.accounting.clone(),
                ),
            );
            tokio::pin!(summary);
            let mut heartbeat = tokio::time::interval(
                (self
                    .settings
                    .limits
                    .stall_timeout
                    .min(Duration::from_secs(1))
                    / 2)
                .max(Duration::from_nanos(1)),
            );
            heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    () = cancellation.cancelled() => return Err(ModelError::Cancelled.into()),
                    output = &mut summary => break output.map_err(|_| ModelError::Stalled)??,
                    _ = heartbeat.tick() => operations.observations.progress(),
                }
            }
        };
        let checkpoint = attempt.finish_active(snapshot, &output, policy.checkpoint)?;
        let prepared = self
            .prepare_projection(
                snapshot,
                Some(&checkpoint),
                &previous.executor_description,
                operations,
            )
            .await?;
        let bytes = self
            .adapters
            .model
            .request_bytes(&prepared.0)?
            .ok_or_else(|| ModelError::Protocol {
                message: "automatic compaction requires request accounting".into(),
            })?;
        if bytes > policy.request_bytes {
            return Err(ContextError::Limit.into());
        }
        if bytes >= previous_bytes {
            return Err(CheckpointError::NoProgress.into());
        }
        if cancellation.is_cancelled() {
            return Err(ModelError::Cancelled.into());
        }
        self.adapters.store.publish_checkpoint(&checkpoint).await?;
        operations.observations.progress();
        Ok(prepared)
    }

    async fn prepare_projection(
        &mut self,
        snapshot: &ActiveSnapshot,
        candidate: Option<&Checkpoint>,
        executor: &str,
        operations: &OperationContext,
    ) -> Result<(ModelRequest, Option<InstructionState>), ContextError> {
        let checkpoint =
            candidate.or_else(|| snapshot.checkpoint().map(|stored| &stored.checkpoint));
        let notice = if self.runtime_restarted {
            RUNTIME_RESTART_NOTICE
        } else if checkpoint.is_some() {
            RUNTIME_COMPACTION_NOTICE
        } else {
            ""
        };
        let context_bytes = self
            .settings
            .limits
            .context_bytes
            .checked_sub(encoded_size(&executor)?)
            .ok_or(ContextError::Limit)?;
        let budget = context_bytes
            .checked_sub(if notice.is_empty() {
                0
            } else {
                encoded_size(&notice)?
            })
            .ok_or(ContextError::Limit)?;
        let PreparedContext {
            mut context,
            instruction_state,
        } = if let Some(checkpoint) = checkpoint {
            self.adapters
                .context
                .prepare_compacted(
                    snapshot.records().to_vec(),
                    self.adapters.capabilities.descriptions(),
                    budget,
                    checkpoint,
                )
                .await?
        } else {
            self.adapters
                .context
                .prepare(
                    snapshot.records().to_vec(),
                    self.adapters.capabilities.descriptions(),
                    budget,
                )
                .await?
        };
        context.instructions.push_str(notice);
        if context.encoded_size()? > context_bytes {
            return Err(ContextError::Limit);
        }
        Ok((
            ModelRequest {
                context,
                executor_description: executor.into(),
                limits: operations.limits(),
                observations: operations.observations.clone(),
            },
            instruction_state,
        ))
    }

    async fn run_inner(
        &mut self,
        invocation: InvocationId,
        accepted: Record,
        cancellation: CancellationToken,
        observations: ObservationSink,
        steering: &mut tokio::sync::mpsc::Receiver<UserInput>,
        recovery: Option<&tokio::sync::mpsc::Sender<ProviderFailure>>,
    ) -> Result<RunOutput, RunError> {
        observations.emit(EventKind::ExecutionState { running: true });
        let limits = self.settings.limits;
        append_bounded(self.adapters.store.as_ref(), &accepted, limits.record_bytes).await?;
        observations.progress();
        let operations = OperationContext {
            invocation,
            host: self.adapters.capabilities.clone(),
            store: self.adapters.store.clone(),
            cancellation: cancellation.child_token(),
            state: tokio::sync::Mutex::new(OperationState::default()),
            limits,
            pending: tokio::sync::Semaphore::new(limits.pending_operations),
            observations: observations.clone(),
        };
        let mut round = 0_u32;
        loop {
            operations.check().await?;
            for _ in 0..steering.len() {
                let Ok(input) = steering.try_recv() else {
                    break;
                };
                if input.bytes() > limits.input_bytes || input.images().len() > 4 {
                    return Err(RunError::InputLimit);
                }
                let record = Record {
                    invocation,
                    kind: RecordKind::InputAccepted { input },
                };
                if encoded_size(&record)? > limits.record_bytes {
                    return Err(RunError::InputLimit);
                }
                append_bounded(self.adapters.store.as_ref(), &record, limits.record_bytes).await?;
                observations.emit(EventKind::SteeringApplied);
            }
            let executor_description = self.adapters.executor.description(operations.limits())?;
            let description_bytes = encoded_size(&executor_description)?;
            if description_bytes > 4096 {
                return Err(ContextError::Limit.into());
            }
            observations.progress();
            let (request, instruction_state) = self
                .prepare_model_request(executor_description, &operations, &cancellation)
                .await?;
            if !steering.is_empty() {
                continue;
            }
            operations.check().await?;
            observations.emit(EventKind::ModelStarted { round });
            round = round.saturating_add(1);
            let response = self
                .respond(request, cancellation.child_token(), recovery)
                .await
                .map_err(|error| match error {
                    ModelError::Stalled => RunError::Deadline {
                        cause: FailureCause::ModelStream {
                            stall_timeout_ms: stall_timeout_ms(limits.stall_timeout),
                        },
                    },
                    error => RunError::Model(error),
                })?;
            operations.check().await?;
            let record = Record {
                invocation,
                kind: RecordKind::ModelCompleted { response },
            };
            if encoded_size(&record)? > limits.record_bytes {
                return Err(StoreError::RecordLimit.into());
            }
            let model = self
                .adapters
                .store
                .append(&record, instruction_state.as_ref())
                .await?;
            if let Some(state) = &instruction_state {
                self.adapters.context.restore(state).await?;
            }
            let RecordKind::ModelCompleted { response } = record.kind else {
                unreachable!("model acceptance constructs only ModelCompleted records");
            };
            observations.emit(EventKind::ModelFinished {
                executing: matches!(response.action, ModelAction::Execute { .. }),
            });
            match response.action {
                ModelAction::Final { answer } => {
                    if !steering.is_empty() {
                        continue;
                    }
                    self.finish_invocation(invocation, InvocationOutcome::Completed)
                        .await?;
                    observations.text(&answer, |text, truncated| EventKind::AnswerCompleted {
                        text,
                        truncated,
                    });
                    let accounting = self.accounting.summary()?;
                    return Ok(RunOutput {
                        invocation,
                        answer,
                        usage: accounting.usage.clone(),
                        accounting,
                    });
                }
                ModelAction::Execute { cells } => {
                    for (ordinal, cell) in cells.into_iter().enumerate() {
                        operations.check().await?;
                        let ordinal = u32::try_from(ordinal).map_err(|_| RunError::CellLimit)?;
                        append_bounded(
                            self.adapters.store.as_ref(),
                            &Record {
                                invocation,
                                kind: RecordKind::CellStarted {
                                    model,
                                    cell: ordinal,
                                },
                            },
                            limits.record_bytes,
                        )
                        .await?;
                        observations.emit(EventKind::ExecutionState { running: true });
                        let code = cell.code.clone();
                        self.cells_started += 1;
                        let cell_id = self.cells_started;
                        observations.text(&code, |text, truncated| EventKind::CellStarted {
                            cell: cell_id,
                            text,
                            truncated,
                        });
                        let outcome = tokio::time::timeout(
                            limits.stall_timeout,
                            self.adapters.executor.execute(
                                cell,
                                &operations,
                                cancellation.child_token(),
                            ),
                        )
                        .await
                        .map_err(|_| RunError::Deadline {
                            cause: FailureCause::Cell {
                                stall_timeout_ms: stall_timeout_ms(limits.stall_timeout),
                            },
                        })?;
                        observations.progress();
                        let cancelled = match operations.check().await {
                            Ok(()) => false,
                            Err(crate::capability::CapabilityError::Cancelled)
                                if cancellation.is_cancelled() =>
                            {
                                true
                            }
                            Err(error) => return Err(error.into()),
                        };
                        let outcome = outcome.map_err(|error| match error {
                            ExecutionError::Capability(
                                crate::capability::CapabilityError::Deadline,
                            ) => RunError::Deadline {
                                cause: FailureCause::Cell {
                                    stall_timeout_ms: stall_timeout_ms(limits.stall_timeout),
                                },
                            },
                            error => RunError::Execution(error),
                        })?;
                        if encoded_size(&outcome)? > limits.output_bytes {
                            return Err(RunError::OutputLimit);
                        }
                        let (status, detail) = match &outcome {
                            CellResponse::Completed { .. } => {
                                (ActivityStatus::Completed, String::new())
                            }
                            CellResponse::Failed { error, .. } => (
                                ActivityStatus::Failed { exit_code: None },
                                error.to_string(),
                            ),
                            CellResponse::Interrupted { .. } | CellResponse::NotStarted => {
                                (ActivityStatus::Interrupted, String::new())
                            }
                        };
                        append_bounded(
                            self.adapters.store.as_ref(),
                            &Record {
                                invocation,
                                kind: RecordKind::CellCompleted {
                                    model,
                                    cell: ordinal,
                                    code,
                                    outcome,
                                },
                            },
                            limits.record_bytes,
                        )
                        .await?;
                        observations.progress();
                        observations.text(&detail, |text, truncated| EventKind::CellFinished {
                            cell: cell_id,
                            status,
                            text,
                            truncated,
                        });
                        if cancelled {
                            return Err(RunError::Cancelled);
                        }
                        observations.emit(EventKind::ExecutionState { running: false });
                    }
                }
            }
        }
    }

    async fn respond(
        &mut self,
        mut request: ModelRequest,
        cancellation: CancellationToken,
        recovery: Option<&tokio::sync::mpsc::Sender<ProviderFailure>>,
    ) -> Result<ModelResponse, ModelError> {
        let validate = |response: ModelResponse| {
            if matches!(&response.action, ModelAction::Execute { cells } if cells.is_empty()) {
                return Err(ModelError::Protocol {
                    message: "empty execution request".into(),
                });
            }
            Ok(response)
        };
        let Some(recovery) = recovery else {
            return self
                .adapters
                .model
                .respond_accounted(request, cancellation, self.accounting.clone())
                .await
                .and_then(validate);
        };
        let timeout = self.settings.limits.stall_timeout;
        let mut heartbeat = tokio::time::interval(
            (timeout.min(Duration::from_secs(1)) / 2).max(Duration::from_nanos(1)),
        );
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            request.observations.progress();
            request.limits.deadline = tokio::time::Instant::now() + timeout;
            let (progress, activity) = tokio::sync::watch::channel(tokio::time::Instant::now());
            let mut attempt = request.clone();
            attempt.observations.progress = Some(progress);
            let attempt_cancellation = cancellation.child_token();
            let result = {
                let response = self.adapters.model.respond_accounted(
                    attempt,
                    attempt_cancellation.clone(),
                    self.accounting.clone(),
                );
                tokio::pin!(response);
                loop {
                    let deadline = *activity.borrow() + timeout;
                    tokio::select! {
                        biased;
                        () = cancellation.cancelled() => return Err(ModelError::Cancelled),
                        result = &mut response => break result,
                        () = tokio::time::sleep_until(deadline) => {
                            if tokio::time::Instant::now() >= *activity.borrow() + timeout {
                                break Err(ModelError::Stalled);
                            }
                        }
                        _ = heartbeat.tick() => request.observations.progress(),
                    }
                }
            };
            attempt_cancellation.cancel();
            let error = match result.and_then(validate) {
                Ok(response) => return Ok(response),
                Err(ModelError::Cancelled) => return Err(ModelError::Cancelled),
                Err(error) => error,
            };
            let (retry, decision) = tokio::sync::oneshot::channel();
            let decision = async {
                recovery
                    .send(ProviderFailure { error, retry })
                    .await
                    .map_err(|failure| failure.0.error)?;
                decision.await.map_err(|_| ModelError::Cancelled)
            };
            tokio::pin!(decision);
            loop {
                tokio::select! {
                    biased;
                    () = cancellation.cancelled() => return Err(ModelError::Cancelled),
                    result = &mut decision => { result?; break; }
                    _ = heartbeat.tick() => request.observations.progress(),
                }
            }
            request.observations.provider_retry_started();
        }
    }

    async fn complete_cancelled_cells(
        &mut self,
        invocation: InvocationId,
    ) -> Result<(), StoreError> {
        let mut pending = BTreeMap::new();
        let mut started = std::collections::BTreeSet::new();
        let snapshot = self.adapters.store.load_active().await?;
        for stored in snapshot.suffix() {
            if stored.record.invocation != invocation {
                continue;
            }
            match &stored.record.kind {
                RecordKind::ModelCompleted { response } => {
                    if let ModelAction::Execute { cells } = &response.action {
                        for (ordinal, cell) in cells.iter().enumerate() {
                            let ordinal =
                                u32::try_from(ordinal).map_err(|_| StoreError::InvalidRecord)?;
                            pending.insert((stored.id, ordinal), cell);
                        }
                    }
                }
                RecordKind::CellStarted { model, cell } => {
                    started.insert((*model, *cell));
                }
                RecordKind::CellCompleted { model, cell, .. } => {
                    pending.remove(&(*model, *cell));
                }
                _ => {}
            }
        }
        for ((model, ordinal), cell) in pending {
            let outcome = if started.contains(&(model, ordinal)) {
                CellResponse::Interrupted {
                    printed: Vec::new(),
                }
            } else {
                CellResponse::NotStarted
            };
            append_bounded(
                self.adapters.store.as_ref(),
                &Record {
                    invocation,
                    kind: RecordKind::CellCompleted {
                        model,
                        cell: ordinal,
                        code: cell.code.clone(),
                        outcome,
                    },
                },
                self.settings.limits.record_bytes,
            )
            .await?;
        }
        Ok(())
    }

    async fn finish_invocation(
        &mut self,
        invocation: InvocationId,
        outcome: InvocationOutcome,
    ) -> Result<(), StoreError> {
        let snapshot = self.adapters.store.load_active().await?;
        if !snapshot
            .records()
            .iter()
            .any(|stored| stored.record.invocation == invocation)
            || snapshot.records().iter().any(|stored| {
                stored.record.invocation == invocation
                    && matches!(stored.record.kind, RecordKind::InvocationFinished { .. })
            })
        {
            return Ok(());
        }
        append_bounded(
            self.adapters.store.as_ref(),
            &Record {
                invocation,
                kind: RecordKind::InvocationFinished { outcome },
            },
            self.settings.limits.record_bytes,
        )
        .await?;
        Ok(())
    }

    async fn settle(
        &mut self,
        timeout: Duration,
        cause: Option<FailureCause>,
    ) -> Result<(), ShutdownError> {
        let invocation = match &self.state {
            EngineState::Unsettled {
                invocation,
                cancellation,
            } => {
                cancellation.cancel();
                Some(*invocation)
            }
            EngineState::Ready | EngineState::Stopped => None,
        };
        let deadline = tokio::time::Instant::now() + timeout;
        let cancellation = CancellationToken::new();
        let (workers, records) = tokio::join!(
            tokio::time::timeout_at(
                deadline,
                self.adapters.executor.shutdown(cancellation.clone())
            ),
            tokio::time::timeout_at(
                deadline,
                reconcile(
                    self.adapters.store.as_ref(),
                    invocation,
                    self.settings.limits.record_bytes
                )
            ),
        );
        cancellation.cancel();
        let workers = match workers {
            Ok(Ok(report)) if report.unresolved_work == 0 => WorkerSettlement::Settled,
            Ok(Ok(report)) => WorkerSettlement::Unresolved {
                work: report.unresolved_work,
            },
            Ok(Err(error)) => WorkerSettlement::Failed { error },
            Err(_) => WorkerSettlement::Deadline,
        };
        let records = match records {
            Ok(Ok(())) => RecordSettlement::Settled,
            Ok(Err(error)) => RecordSettlement::Failed { error },
            Err(_) => RecordSettlement::Deadline,
        };
        let records = if matches!(workers, WorkerSettlement::Settled)
            && matches!(records, RecordSettlement::Settled)
            && let Some(invocation) = invocation
        {
            match tokio::time::timeout_at(deadline, async {
                self.complete_cancelled_cells(invocation).await?;
                self.finish_invocation(
                    invocation,
                    InvocationOutcome::Failed {
                        cause: cause.unwrap_or(FailureCause::Unspecified),
                    },
                )
                .await
            })
            .await
            {
                Ok(Ok(())) => records,
                Ok(Err(error)) => RecordSettlement::Failed { error },
                Err(_) => RecordSettlement::Deadline,
            }
        } else {
            records
        };
        if matches!(workers, WorkerSettlement::Settled)
            && matches!(records, RecordSettlement::Settled)
        {
            self.state = EngineState::Stopped;
            Ok(())
        } else {
            Err(ShutdownError { workers, records })
        }
    }
}

fn capability_input(
    operation: crate::store::OperationId,
    input: &CapabilityInput,
    limit: usize,
) -> EventKind {
    let (title, title_truncated) = preview_text(&input.title, limit.min(256));
    let (text, truncated) = preview_text(&input.text, limit);
    EventKind::CapabilityInput {
        operation,
        title,
        text,
        summary: input.summary,
        truncated: title_truncated || truncated,
    }
}

fn capability_output(
    operation: crate::store::OperationId,
    mut output: ActivityOutput,
    limit: usize,
) -> EventKind {
    let end = output
        .text
        .floor_char_boundary(output.text.len().min(limit));
    output.truncated |= end < output.text.len();
    output.text.truncate(end);
    EventKind::CapabilityOutput { operation, output }
}

fn compaction_boundary(
    snapshot: &ActiveSnapshot,
    recent_bytes: usize,
) -> Result<RecordId, CheckpointError> {
    let records = snapshot.suffix();
    let mut pending = 0_usize;
    let mut cuts = Vec::new();
    for (index, stored) in records.iter().enumerate() {
        match &stored.record.kind {
            RecordKind::ModelCompleted { response } => {
                if pending != 0 {
                    return Err(CheckpointError::Boundary);
                }
                match &response.action {
                    ModelAction::Execute { cells } => pending = cells.len(),
                    ModelAction::Final { .. } => cuts.push(index),
                }
            }
            RecordKind::CellCompleted { .. } => {
                pending = pending.checked_sub(1).ok_or(CheckpointError::Boundary)?;
                if pending == 0 {
                    cuts.push(index);
                }
            }
            RecordKind::InvocationFinished { .. } if pending == 0 => {
                if let Some(last) = cuts.last_mut() {
                    *last = index;
                }
            }
            _ => {}
        }
    }
    if pending != 0 {
        return Err(CheckpointError::Boundary);
    }
    let last = *cuts.last().ok_or(CheckpointError::NoProgress)?;
    let cut = if cuts.len() >= 2 {
        let recent = cuts[cuts.len() - 2];
        if encoded_size(&&records[recent + 1..])? <= recent_bytes {
            recent
        } else {
            last
        }
    } else {
        last
    };
    Ok(records[cut].id)
}

fn preview_text(text: &str, limit: usize) -> (String, bool) {
    let end = text.floor_char_boundary(text.len().min(limit));
    (text[..end].into(), end < text.len())
}

/// Resume rejects retained evidence instead of guessing missing outcomes.
#[derive(Debug, thiserror::Error)]
pub enum ResumeError {
    #[error("{cause}; resume settlement failed: {cleanup}")]
    Settlement {
        cause: Box<ResumeError>,
        cleanup: ShutdownError,
    },
    #[error("retained history has invalid ordering, identities or references")]
    InvalidHistory,
    #[error("retained invocation requires application recovery")]
    RecoveryRequired,
    #[error("retained invocation counter is exhausted")]
    IdentityExhausted,
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Context(#[from] ContextError),
}

/// Invocation failure is independent of observation delivery.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("automatic compaction failed and the prior request does not fit: {0}")]
    Compaction(#[from] CompactionError),
    #[error("{cause}; external effects may be unknown")]
    Deadline { cause: FailureCause },
    #[error("execution output exceeds byte limit")]
    OutputLimit,
    #[error("invocation cell budget exhausted")]
    CellLimit,
    #[error("{cause}; settlement failed: {cleanup}")]
    Settlement {
        cause: Box<RunError>,
        cleanup: ShutdownError,
    },
    #[error("invocation cancelled; external effects may be unknown")]
    Cancelled,
    /// Requested cancellation settled successfully. Another turn may be admitted.
    #[error("turn cancelled; runtime {runtime:?}; existing effects retained")]
    TurnCancelled { runtime: RuntimeState },
    #[error("engine stopped; consume it with shutdown")]
    Stopped,
    #[error("input exceeds byte limit")]
    InputLimit,
    #[error("invocation budget exhausted")]
    Budget,
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error(transparent)]
    Execution(#[from] ExecutionError),
    #[error(transparent)]
    Context(#[from] ContextError),
}

impl From<crate::capability::CapabilityError> for RunError {
    fn from(error: crate::capability::CapabilityError) -> Self {
        match error {
            crate::capability::CapabilityError::Store(error) => Self::Store(error),
            crate::capability::CapabilityError::Cancelled => Self::Cancelled,
            crate::capability::CapabilityError::Deadline => Self::Deadline {
                cause: FailureCause::Capability,
            },
            error => Self::Execution(ExecutionError::Capability(error)),
        }
    }
}

/// Both independent cleanup results remain visible even if both fail.
#[derive(Debug, thiserror::Error)]
#[error("worker settlement: {workers:?}; record settlement: {records:?}")]
pub struct ShutdownError {
    pub workers: WorkerSettlement,
    pub records: RecordSettlement,
}

/// Worker termination evidence from the owned executor.
#[derive(Debug)]
pub enum WorkerSettlement {
    Settled,
    Unresolved { work: usize },
    Failed { error: ExecutionError },
    Deadline,
}

/// Canonical bookkeeping is attempted even when worker cleanup fails.
#[derive(Debug)]
pub enum RecordSettlement {
    Settled,
    Failed { error: StoreError },
    Deadline,
}

fn is_cancellation(error: &RunError) -> bool {
    matches!(
        error,
        RunError::Cancelled
            | RunError::Model(ModelError::Cancelled)
            | RunError::Execution(ExecutionError::Capability(
                crate::capability::CapabilityError::Cancelled
            ))
    )
}

async fn reconcile(
    store: &dyn Store,
    invocation: Option<InvocationId>,
    record_bytes: usize,
) -> Result<(), StoreError> {
    let Some(invocation) = invocation else {
        return Ok(());
    };
    let snapshot = store.load_active().await?;
    let effects = snapshot
        .unresolved_effects()
        .map_err(|_| StoreError::InvalidCheckpoint)?;
    for effect in effects {
        if effect.outcome.is_some() {
            continue;
        }
        let stored = snapshot
            .records()
            .iter()
            .find(|record| record.id == effect.start)
            .ok_or(StoreError::InvalidCheckpoint)?;
        if stored.record.invocation != invocation {
            continue;
        }
        let RecordKind::OperationStarted { operation, .. } = &stored.record.kind else {
            return Err(StoreError::InvalidCheckpoint);
        };
        append_bounded(
            store,
            &Record {
                invocation,
                kind: RecordKind::OperationFinished {
                    operation: *operation,
                    outcome: crate::capability::OperationOutcome::Unknown,
                },
            },
            record_bytes,
        )
        .await?;
    }
    Ok(())
}

fn stall_timeout_ms(stall_timeout: Duration) -> u64 {
    u64::try_from(stall_timeout.as_millis()).unwrap_or(u64::MAX)
}

fn deadline_cause(error: &RunError) -> Option<FailureCause> {
    match error {
        RunError::Deadline { cause } => Some(*cause),
        _ => None,
    }
}

enum HistoryRequirement {
    Settled,
    Recoverable,
}

fn validate_history(
    snapshot: &StoreSnapshot,
    requirement: HistoryRequirement,
) -> Result<u64, ResumeError> {
    snapshot
        .validate_checkpoints()
        .map_err(|_| ResumeError::InvalidHistory)?;
    let mut identities = std::collections::BTreeSet::new();
    let mut cells = BTreeMap::<(RecordId, u32), (&str, bool)>::new();
    let mut completed_cells = BTreeMap::new();
    let mut operations = BTreeMap::new();
    let mut invocation = 0;
    let mut terminal = true;
    let mut final_response = false;
    for (index, stored) in snapshot.records.iter().enumerate() {
        stored.record.validate()?;
        let sequence = i64::try_from(index)
            .ok()
            .and_then(|index| index.checked_add(1));
        if sequence != Some(i64::from(stored.sequence)) || !identities.insert(stored.id) {
            return Err(ResumeError::InvalidHistory);
        }
        let record = &stored.record;
        if record.invocation.0 != invocation {
            if !terminal {
                return Err(ResumeError::InvalidHistory);
            }
            if record.invocation.0 <= invocation
                || !matches!(record.kind, RecordKind::InputAccepted { .. })
            {
                return Err(ResumeError::InvalidHistory);
            }
            invocation = record.invocation.0;
            terminal = false;
            final_response = false;
            operations.clear();
        } else if terminal {
            return Err(ResumeError::InvalidHistory);
        }
        match &record.kind {
            RecordKind::InputAccepted { .. } => final_response = false,
            RecordKind::ModelCompleted { response } => {
                if !cells.is_empty() {
                    return Err(ResumeError::InvalidHistory);
                }
                final_response = matches!(response.action, ModelAction::Final { .. });
                if let ModelAction::Execute { cells: requested } = &response.action {
                    if requested.is_empty() {
                        return Err(ResumeError::InvalidHistory);
                    }
                    for (ordinal, cell) in requested.iter().enumerate() {
                        let ordinal =
                            u32::try_from(ordinal).map_err(|_| ResumeError::InvalidHistory)?;
                        cells.insert((stored.id, ordinal), (&cell.code, false));
                    }
                }
            }
            RecordKind::CellStarted { model, cell } => {
                if cells.first_key_value().map(|(key, _)| key) != Some(&(*model, *cell)) {
                    return Err(ResumeError::InvalidHistory);
                }
                let Some((_, started)) = cells.get_mut(&(*model, *cell)) else {
                    return Err(ResumeError::InvalidHistory);
                };
                if *started {
                    return Err(ResumeError::InvalidHistory);
                }
                *started = true;
            }
            RecordKind::CellCompleted {
                model,
                cell,
                code,
                outcome,
            } => {
                if cells.first_key_value().map(|(key, _)| key) != Some(&(*model, *cell)) {
                    return Err(ResumeError::InvalidHistory);
                }
                let Some((expected, started)) = cells.remove(&(*model, *cell)) else {
                    return Err(ResumeError::InvalidHistory);
                };
                if expected != code || started == matches!(outcome, CellResponse::NotStarted) {
                    return Err(ResumeError::InvalidHistory);
                }
                completed_cells.insert(stored.id, record.invocation);
            }
            RecordKind::OperationStarted { operation, .. } => {
                if operations.insert(*operation, false).is_some() {
                    return Err(ResumeError::InvalidHistory);
                }
            }
            RecordKind::OperationFinished { operation, .. } => {
                let Some(finished) = operations.get_mut(operation) else {
                    return Err(ResumeError::InvalidHistory);
                };
                if *finished {
                    return Err(ResumeError::InvalidHistory);
                }
                *finished = true;
            }
            RecordKind::InvocationFinished { outcome } => {
                if !cells.is_empty() {
                    return Err(ResumeError::RecoveryRequired);
                }
                if *outcome == InvocationOutcome::Completed
                    && (!final_response || operations.values().any(|finished| !finished))
                {
                    return Err(ResumeError::InvalidHistory);
                }
                terminal = true;
            }
            RecordKind::TurnCancelled { .. } => {}
        }
    }
    if matches!(requirement, HistoryRequirement::Settled) && !terminal {
        return Err(ResumeError::RecoveryRequired);
    }
    if invocation >= i64::MAX as u64 {
        return Err(ResumeError::IdentityExhausted);
    }
    if let Some(state) = &snapshot.instruction_state {
        if state.invocation.0 == 0 || state.invocation.0 > invocation {
            return Err(ResumeError::InvalidHistory);
        }
        state.validate_bounds(snapshot.records.len())?;
        state.validate_feedback_targets(&completed_cells)?;
    }
    Ok(invocation)
}

const RUNTIME_RESTART_NOTICE: &str = concat!(
    "\n<runtime-restarted>\n",
    "This session resumed in a fresh runtime. Earlier JavaScript bindings, file observations, ",
    "handles and process supervision are unavailable, even if an older cancellation preserved them. ",
    "Conversation history is retained. Inspect current shared context through its tools. ",
    "External effects may remain. ",
    "Read files again before mutation. Do not replay earlier cells, operations or child work.\n",
    "</runtime-restarted>\n",
);

const RUNTIME_COMPACTION_NOTICE: &str = concat!(
    "\nCompaction did not reset or replay the JavaScript runtime. ",
    "Reuse only bindings established since its latest reset. External effects remain; ",
    "unknown outcomes do not authorize repetition.\n",
);

/// Rejected proposals leave existing evidence and the live executor unchanged.
#[derive(Debug, thiserror::Error)]
pub enum CompactionError {
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error("compaction requires an initialized, settled engine")]
    NotReady,
    #[error(transparent)]
    Checkpoint(#[from] CheckpointError),
    #[error(transparent)]
    Context(#[from] ContextError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    History(#[from] ResumeError),
    #[error(transparent)]
    Execution(#[from] ExecutionError),
}
