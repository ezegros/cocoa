//! Engine-owned cell execution. Adapters retain private state, never UI state.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::capability::{CapabilityError, OperationContext};

/// Complete JavaScript source, bounded by the engine before execution.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Cell {
    pub code: String,
}

/// Per-operation bounds. Adapters must enforce these while allocating output
/// and doing work; the engine additionally checks returned boundary data.
#[derive(Clone, Copy, Debug)]
pub struct WorkLimits {
    /// Maximum encoded cell/host feedback bytes, including metadata.
    /// Model providers use their own generation and decoder limits instead.
    pub output_bytes: usize,
    /// Absolute deadline for this cell or host operation, not the whole turn.
    /// For model requests this marks the initial stream-inactivity window;
    /// receiving bytes resets that window through `ModelRequest::observations`.
    pub deadline: tokio::time::Instant,
}

/// Whether a host result fits Rust-to-JavaScript conversion: at most
/// `min(output_bytes, 256 KiB)` encoded JSON bytes, 4096 value nodes and depth 32.
/// Object keys count toward encoded bytes, not value nodes. Hosts can shorten
/// previews or reject before effects using the same check as the executor.
///
/// This does not promise the value fits selected printed feedback, which has
/// separate encoded-byte and error/framing reservations. Completions are silent.
#[must_use]
pub fn host_value_fits(value: &Value, output_bytes: usize) -> bool {
    let mut stack = vec![(value, 0)];
    let mut nodes = 0;
    while let Some((value, depth)) = stack.pop() {
        nodes += 1;
        if nodes > 4096 || depth > 32 {
            return false;
        }
        match value {
            Value::Array(values) => {
                if values.len() > 4096 - nodes {
                    return false;
                }
                stack.extend(values.iter().map(|value| (value, depth + 1)));
            }
            Value::Object(values) => {
                if values.len() > 4096 - nodes {
                    return false;
                }
                stack.extend(values.values().map(|value| (value, depth + 1)));
            }
            _ => {}
        }
    }
    crate::store::encoded_size(value).is_ok_and(|bytes| bytes <= output_bytes.min(256 * 1024))
}

/// Selected, print-only cell feedback. Earlier prints and effects survive failure.
/// The final expression is evaluated and awaited, but never included here.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum CellResponse {
    Completed {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        printed: Vec<Value>,
    },
    /// Execution was interrupted. Earlier prints and effects are not rolled back.
    Interrupted {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        printed: Vec<Value>,
    },
    /// The model requested this cell, but cancellation prevented execution.
    NotStarted,
    Failed {
        error: FeedbackError,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        printed: Vec<Value>,
    },
}

/// State of retained JavaScript bindings after a settled turn cancellation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuntimeState {
    Preserved,
    /// The interrupted runtime was retired; no effects were replayed or reverted.
    Reset,
}

/// Bounded recoverable feedback. Capability effects describe only the failed
/// operation, never earlier operations in the cell. Core errors remain authoritative.
#[derive(Clone, Debug, PartialEq, thiserror::Error, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FeedbackError {
    #[error("JavaScript error: {message}")]
    Javascript { message: String, truncated: bool },
    #[error("{method}: {code}: {message}")]
    Capability {
        method: String,
        code: String,
        message: String,
        operation_effects: OperationEffects,
        truncated: bool,
    },
    /// Context failure with intact owner metadata. Effects are always `None`.
    #[error("{method}: {code}: {message}")]
    Context {
        method: String,
        code: String,
        message: String,
        owner: Option<crate::store::AgentId>,
        operation_effects: ContextEffects,
        truncated: bool,
    },
    #[error("invalid printed output: {message}")]
    InvalidOutput { message: String, truncated: bool },
    /// Effective encoded printed-array ceiling for print overflow/count failure,
    /// or encoded error-object ceiling when fixed error metadata cannot fit.
    #[error("feedback limit reached (encoded byte ceiling {limit_bytes})")]
    OutputLimit { limit_bytes: u32 },
}

/// Evidence for one failed capability, without a cell-wide rollback implication.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OperationEffects {
    None,
    Partial,
    Unknown,
}

/// A rejected context operation cannot have applied or uncertain effects.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContextEffects {
    None,
}

/// Unresolved work is not evidence of confirmed termination.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShutdownReport {
    /// Zero confirms settlement; any positive count makes engine shutdown fail.
    pub unresolved_work: usize,
}

/// One executor belongs to exactly one engine. Dropping execute must be safe,
/// but is not proof that external effects stopped. Shutdown must settle workers.
/// All invocation-scoped capability futures must be settled before `execute`
/// returns, including recoverable cell failure. Do not detach or abandon a call
/// and return success with an unmatched operation start. On cancellation/fatal
/// failure, owned workers remain the executor's responsibility until shutdown.
#[async_trait]
pub trait CellExecutor: Send {
    /// Describe the installed runtime under these effective engine limits.
    /// Return at most 4096 encoded JSON bytes, including string escaping.
    /// The engine reserves this in context; providers include it in instructions.
    /// This is request metadata, not a capability or persisted record.
    /// If cells cannot run under the limits, describe that restriction without
    /// rejecting final-answer-only work.
    ///
    /// # Errors
    /// Report failures preparing the runtime description.
    fn description(&self, _limits: WorkLimits) -> Result<String, ExecutionError> {
        Ok(String::new())
    }

    /// Evaluate a cell, routing every capability effect through `operations`.
    ///
    /// Enforce `operations.limits()` while running and allocating results. Include
    /// required printed output in canonical feedback, not only UI notifications.
    ///
    /// # Errors
    /// Infrastructure errors stop the invocation. Language/tool errors should
    /// return `CellResponse::Failed` when the model can recover.
    async fn execute(
        &mut self,
        cell: Cell,
        operations: &OperationContext,
        cancellation: CancellationToken,
    ) -> Result<CellResponse, ExecutionError>;

    /// Settle cancelled turn work and prepare safe execution of another turn.
    /// Called only after the invocation future has returned. Preserve bindings
    /// where safe; report a reset explicitly. Never replay external operations.
    ///
    /// # Errors
    /// Refuse continuation unless all owned work has settled and the runtime is safe.
    async fn cancel_turn(&mut self) -> Result<RuntimeState, ExecutionError> {
        Err(ExecutionError::Unavailable {
            message: "executor does not support turn continuation".into(),
        })
    }

    /// Stop owned workers without destroying a shared coding environment.
    ///
    /// Must be idempotent, including after an earlier cancelled shutdown future.
    ///
    /// # Errors
    /// Report cleanup failures; a timeout is enforced by the engine separately.
    async fn shutdown(
        &mut self,
        cancellation: CancellationToken,
    ) -> Result<ShutdownReport, ExecutionError>;
}

/// Executor infrastructure failures, distinct from model-visible cell failures.
#[derive(Debug, thiserror::Error)]
pub enum ExecutionError {
    #[error("executor failed: {message}")]
    Unavailable { message: String },
    #[error(transparent)]
    Capability(#[from] CapabilityError),
}
