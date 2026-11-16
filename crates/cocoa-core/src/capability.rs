//! Generic capability dispatch with canonical start/outcome evidence.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::engine::{ActivityStatus, EventKind, ObservationSink};
use crate::store::{
    AgentId, InvocationId, OperationId, Record, RecordId, RecordKind, Store, StoreError,
};

/// An adapter-owned method schema, included in model context.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CapabilityDescription {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// A generated executor function. Path segments are data, never source code.
/// The host must revalidate the captured capability identity on every call.
#[derive(Clone, Debug)]
pub struct CapabilityBinding {
    pub path: Vec<String>,
    pub name: String,
}

/// Ready capability host. Validate grants and arguments before effects. Never
/// destroy a shared coding environment on cancellation or engine shutdown.
#[async_trait]
pub trait CapabilityHost: Send + Sync {
    /// Return bounded method descriptions. Engine context limits still apply.
    fn descriptions(&self) -> Vec<CapabilityDescription>;

    /// Additional generated functions, refreshed between cells without resetting
    /// executor state. These do not add their schemas to every model request.
    fn bindings(&self) -> Vec<CapabilityBinding> {
        Vec::new()
    }

    /// Dispatch a granted method, preserving uncertain effects in errors.
    ///
    /// # Errors
    /// Reject invalid grants/arguments or report backend errors with effect status.
    async fn call(
        &self,
        name: &str,
        arguments: Value,
        limits: crate::execution::WorkLimits,
        cancellation: CancellationToken,
    ) -> Result<Value, CapabilityError>;

    /// Dispatch with optional operation-scoped display metadata. Notifications
    /// must not authorize work or replace the returned outcome.
    ///
    /// # Errors
    /// Same failure contract as `call`.
    async fn call_observed(
        &self,
        name: &str,
        arguments: Value,
        limits: crate::execution::WorkLimits,
        cancellation: CancellationToken,
        observer: CapabilityObserver,
    ) -> Result<Value, CapabilityError> {
        observer.input(&self.describe_input(name, &arguments));
        self.call(name, arguments, limits, cancellation).await
    }

    /// Dispatch after the operation start commits. `operation_record` is host
    /// authority for transaction-sized local mutations, never a tool argument.
    /// Decorators must forward it unchanged. The default supports stateless hosts.
    ///
    /// # Errors
    /// Same failure contract as [`Self::call`], including unknown commit outcomes.
    async fn call_started(
        &self,
        name: &str,
        arguments: Value,
        limits: crate::execution::WorkLimits,
        cancellation: CancellationToken,
        observer: CapabilityObserver,
        _operation_record: RecordId,
    ) -> Result<Value, CapabilityError> {
        self.call_observed(name, arguments, limits, cancellation, observer)
            .await
    }

    /// Describe requested work without executing it or including its output.
    /// Used to recover display metadata after observation loss. Missing target
    /// metadata must not reject work; renderers bound and sanitize this text.
    fn describe_input(&self, name: &str, arguments: &Value) -> CapabilityInput {
        CapabilityInput {
            title: name.into(),
            text: arguments.to_string(),
            summary: None,
        }
    }

    /// Recover display-only output from retained evidence without executing work.
    /// This may omit unavailable content; it must not reread mutable live files.
    fn describe_output(
        &self,
        _name: &str,
        _arguments: &Value,
        _outcome: &OperationOutcome,
    ) -> Option<ActivityOutput> {
        None
    }

    /// Classify retained evidence for display. Coding adapters may distinguish
    /// nonzero command exits or deferred effects from successful host returns.
    fn activity_status(&self, outcome: &OperationOutcome) -> ActivityStatus {
        ActivityStatus::from(outcome)
    }

    /// Publish display metadata after the outcome is recorded. Hosts with
    /// independently owned work may retain the observer until that work settles.
    /// This must not execute, cancel or wait for the underlying work.
    async fn observe_outcome(
        &self,
        name: &str,
        arguments: &Value,
        outcome: &OperationOutcome,
        observer: CapabilityObserver,
    ) {
        if let Some(output) = self.describe_output(name, arguments, outcome) {
            observer.output(output);
        }
        observer.status(self.activity_status(outcome));
    }
}

/// Display-only description of requested work. Never include returned contents,
/// process output or errors here; the canonical outcome remains model feedback.
pub struct CapabilityInput {
    pub title: String,
    pub text: String,
    pub summary: Option<ActivitySummary>,
}

/// Inline ranges or change counts. Inputs describe requested work; outputs may
/// replace counts with retained effects. Ranges are inclusive and one-based;
/// change counts exclude equal lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivitySummary {
    LineRange { start: u64, end: u64 },
    LineChanges { added: u64, removed: u64 },
}

/// Display-only output. Publishers and renderers independently bound this text;
/// canonical capability results remain the source of execution feedback.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActivityOutput {
    pub text: String,
    pub kind: ActivityOutputKind,
    pub summary: Option<ActivitySummary>,
    pub truncated: bool,
}

/// How retained output is presented, without interpreting it as Markdown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivityOutputKind {
    Command,
    Diff,
}

/// Display metadata scoped to one admitted capability. Clones keep only the
/// bounded notification channel alive, not the operation or its cancellation.
#[derive(Clone, Debug)]
pub struct CapabilityObserver {
    operation: OperationId,
    observations: ObservationSink,
}

impl CapabilityObserver {
    /// Set the title and input details from validated host arguments and resolved
    /// targets. The publisher bounds UTF-8; renderers sanitize and width-limit it.
    pub fn input(&self, input: &CapabilityInput) {
        self.observations.capability_input(self.operation, input);
    }

    /// Replace the bounded output preview from retained evidence.
    pub fn output(&self, output: ActivityOutput) {
        self.observations.capability_output(self.operation, output);
    }

    /// Report whether the displayed work is still running or has settled.
    /// Independently owned work may report after its accepting call returns.
    pub fn status(&self, status: ActivityStatus) {
        self.observations.emit(EventKind::CapabilityFinished {
            operation: self.operation,
            status,
        });
    }
}

impl From<&OperationOutcome> for ActivityStatus {
    fn from(outcome: &OperationOutcome) -> Self {
        match outcome {
            OperationOutcome::Completed { .. } | OperationOutcome::CompletedOmitted { .. } => {
                ActivityStatus::Completed
            }
            OperationOutcome::Failed { error } => {
                if matches!(error, CapabilityError::Cancelled) {
                    ActivityStatus::Interrupted
                } else {
                    ActivityStatus::Failed { exit_code: None }
                }
            }
            OperationOutcome::FailedOmitted { .. } => ActivityStatus::Failed { exit_code: None },
            OperationOutcome::Unknown => ActivityStatus::Unknown,
        }
    }
}

/// Effect status describes evidence, not a rollback promise.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Effects {
    /// This operation failed before producing effects; earlier operations remain.
    None,
    /// The operation failed after effects became possible; no rollback is promised.
    Partial,
    /// The caller cannot establish whether effects occurred or work stopped.
    Unknown,
}

/// Canonical operation evidence, including uncertain effects and omitted output.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum OperationOutcome {
    Completed {
        value: Value,
    },
    /// The host completed, but its encoded output exceeded the feedback budget.
    CompletedOmitted {
        output_bytes: u64,
    },
    Failed {
        error: CapabilityError,
    },
    /// The failure details exceeded the budget, but effect evidence survives.
    FailedOmitted {
        effects: Effects,
        output_bytes: u64,
    },
    /// No completed outcome was retained; this does not establish termination.
    Unknown,
}

/// `Rejected`, `Backend`, `Tool` and `Context` may become cell feedback; other
/// variants stop the invocation even when the executor catches them.
#[derive(Clone, Debug, PartialEq, thiserror::Error, Serialize, Deserialize)]
pub enum CapabilityError {
    #[error("capability rejected: {message}")]
    Rejected { message: String },
    #[error("capability backend failed ({effects:?}): {message}")]
    Backend { message: String, effects: Effects },
    /// Recoverable tool failure with a stable machine-readable code.
    #[error("{code}: {message}")]
    Tool {
        code: String,
        message: String,
        effects: Effects,
    },
    /// Recoverable context failure before effects, with optional owner metadata.
    #[error("{code}: {message}")]
    Context {
        code: String,
        message: String,
        owner: Option<AgentId>,
    },
    #[error("capability budget exhausted")]
    Budget,
    #[error("pending capability limit exhausted")]
    PendingLimit,
    #[error("capability result exceeds limit; effects may have completed")]
    OutputLimit,
    #[error("capability cancelled; effects unknown")]
    Cancelled,
    #[error("capability deadline exhausted; effects unknown")]
    Deadline,
    /// Owned work could not be confirmed settled. Continuation is unsafe.
    #[error("capability cleanup failed: {message}")]
    Unsettled { message: String },
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl CapabilityError {
    /// Effect evidence retained even when verbose backend details are omitted.
    #[must_use]
    pub fn effects(&self) -> Effects {
        match self {
            Self::Rejected { .. } | Self::Context { .. } | Self::Budget | Self::PendingLimit => {
                Effects::None
            }
            Self::Backend { effects, .. } | Self::Tool { effects, .. } => *effects,
            Self::OutputLimit
            | Self::Cancelled
            | Self::Deadline
            | Self::Unsettled { .. }
            | Self::Store(_) => Effects::Unknown,
        }
    }
}

/// Invocation-scoped dispatcher borrowed by the executor. Every effect must go
/// through this boundary; implementations must not spawn calls beyond the cell.
pub struct OperationContext {
    pub(crate) invocation: InvocationId,
    pub(crate) host: Arc<dyn CapabilityHost>,
    pub(crate) store: Arc<dyn Store>,
    pub(crate) cancellation: CancellationToken,
    pub(crate) state: Mutex<OperationState>,
    pub(crate) limits: crate::engine::Limits,
    pub(crate) pending: tokio::sync::Semaphore,
    pub(crate) observations: crate::engine::ObservationSink,
}

impl OperationContext {
    /// Granted method catalog used to install matching executor bindings.
    #[must_use]
    pub fn descriptions(&self) -> Vec<CapabilityDescription> {
        self.host.descriptions()
    }

    /// Snapshot of generated function paths authorized by this host.
    #[must_use]
    pub fn bindings(&self) -> Vec<CapabilityBinding> {
        self.host.bindings()
    }

    /// Bounds the executor and host must enforce during execution and allocation.
    #[must_use]
    pub fn limits(&self) -> crate::execution::WorkLimits {
        crate::execution::WorkLimits {
            output_bytes: self.limits.output_bytes,
            deadline: tokio::time::Instant::now() + self.limits.stall_timeout,
        }
    }

    /// Optional printed UI output. Required feedback must also be returned in
    /// `CellResponse`; this channel may lose every notification.
    pub fn printed(&self, text: &str) {
        self.observations.printed(text);
    }

    /// Record a start, invoke the host, then retain its completed outcome.
    ///
    /// # Errors
    /// Return grant, backend, budget, output, cancellation, deadline and storage
    /// failures. Oversized host results retain an omitted-outcome summary; effects
    /// may already have completed. Host-returned fatal errors are also latched so
    /// an executor cannot turn them into feedback.
    /// Only `Rejected`, `Backend`, `Tool` and `Context` remain recoverable host failures.
    pub async fn call(&self, name: &str, arguments: Value) -> Result<Value, CapabilityError> {
        self.check().await?;
        if crate::store::encoded_size(&(name, &arguments))? > self.limits.record_bytes {
            self.state.lock().await.failure = Some(StoreError::RecordLimit.into());
            return Err(StoreError::RecordLimit.into());
        }
        let _permit = match self.pending.try_acquire() {
            Ok(permit) => permit,
            Err(_) => {
                self.state.lock().await.failure = Some(CapabilityError::PendingLimit);
                return Err(CapabilityError::PendingLimit);
            }
        };
        let operation = {
            let mut state = self.state.lock().await;
            let next = state
                .next
                .checked_add(1)
                .filter(|&next| i64::try_from(next).is_ok());
            let Some(next) = next else {
                state.failure = Some(CapabilityError::Budget);
                return Err(CapabilityError::Budget);
            };
            state.next = next;
            OperationId(next)
        };
        let operation_record = self
            .append(RecordKind::OperationStarted {
                operation,
                name: name.into(),
                arguments: arguments.clone(),
            })
            .await?;
        self.check().await?;
        self.observations.text(name, |name, truncated| {
            crate::engine::EventKind::CapabilityStarted {
                operation,
                name,
                truncated,
            }
        });
        let result = self
            .host
            .call_started(
                name,
                arguments.clone(),
                self.limits(),
                self.cancellation.child_token(),
                CapabilityObserver {
                    operation,
                    observations: self.observations.clone(),
                },
                operation_record,
            )
            .await;
        let bytes = match &result {
            Ok(value) => crate::store::encoded_size(value)?,
            Err(error) => crate::store::encoded_size(error)?,
        };
        if bytes > self.limits.output_bytes {
            let output_bytes = u64::try_from(bytes).map_err(|_| StoreError::Encoding)?;
            let outcome = match result {
                Ok(_) => OperationOutcome::CompletedOmitted { output_bytes },
                Err(error) => OperationOutcome::FailedOmitted {
                    effects: error.effects(),
                    output_bytes,
                },
            };
            self.append(RecordKind::OperationFinished { operation, outcome })
                .await?;
            self.state.lock().await.failure = Some(CapabilityError::OutputLimit);
            return Err(CapabilityError::OutputLimit);
        }
        let outcome = match &result {
            Ok(value) => OperationOutcome::Completed {
                value: value.clone(),
            },
            Err(error) => {
                if !matches!(
                    error,
                    CapabilityError::Rejected { .. }
                        | CapabilityError::Backend { .. }
                        | CapabilityError::Tool { .. }
                        | CapabilityError::Context { .. }
                ) {
                    let mut state = self.state.lock().await;
                    if state.failure.is_none()
                        || matches!(state.failure, Some(CapabilityError::Cancelled))
                    {
                        state.failure = Some(error.clone());
                    }
                }
                OperationOutcome::Failed {
                    error: error.clone(),
                }
            }
        };
        self.append(RecordKind::OperationFinished {
            operation,
            outcome: outcome.clone(),
        })
        .await?;
        self.host
            .observe_outcome(
                name,
                &arguments,
                &outcome,
                CapabilityObserver {
                    operation,
                    observations: self.observations.clone(),
                },
            )
            .await;
        result
    }

    async fn append(&self, kind: RecordKind) -> Result<RecordId, StoreError> {
        match crate::store::append_bounded(
            self.store.as_ref(),
            &Record {
                invocation: self.invocation,
                kind,
            },
            self.limits.record_bytes,
        )
        .await
        {
            Ok(id) => Ok(id),
            Err(error) => {
                self.state.lock().await.failure = Some(error.clone().into());
                Err(error)
            }
        }
    }

    pub(crate) async fn check(&self) -> Result<(), CapabilityError> {
        if let Some(error) = &self.state.lock().await.failure {
            return Err(error.clone());
        }
        if self.cancellation.is_cancelled() {
            return Err(CapabilityError::Cancelled);
        }
        Ok(())
    }
}

#[derive(Default)]
pub(crate) struct OperationState {
    next: u64,
    failure: Option<CapabilityError>,
}
