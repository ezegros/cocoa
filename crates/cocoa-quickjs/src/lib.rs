//! Private, bounded JavaScript cell execution for one engine.
//!
//! Each executor owns one QuickJS thread. A cell evaluates ordinary script
//! completion with top-level `await`, then awaits a returned native Promise.
//! Completion values are never echoed or converted to JSON. One context retains
//! top-level variables, functions and closures across cells and successful turns.
//! Native `var`/`let`/`const` and block/function scope rules apply without rewriting.
//! Prefer `var` for reusable working bindings because it permits redeclaration.
//! Other agents and subagents need independent executors; they inherit no objects
//! or bindings. Handoffs require explicit context. Retained values share the heap
//! bound; they are neither JSON snapshots nor durable storage. Recoverable errors
//! preserve prior assignments and effects.
//!
//! Granted `cocoa.read/write/edit/run` methods return Promises through a
//! nonreplaceable namespace. Generic embeddings can also use
//! `capabilities.call(name, arguments)`. Both dispatch only through the borrowed
//! current core operation context, including saved functions. Trusted helpers
//! capture intrinsics once; per-cell brokers and budgets are never captured.
//! Direct methods accept one argument object and return a result or reject.
//! Compose calls with ordinary `await` and bounded loops in one cell. Recoverable
//! host failures retain bounded method/code/message/effects even when caught;
//! fatal failures stop the invocation. Each call is separately authorized,
//! recorded and budgeted, not part of a transaction.
//!
//! Granted `context.get(key)`, `context.set(key, content)` and
//! `context.list(filters)` methods form a frozen, nonreplaceable namespace.
//! They require exactly one, two and one arguments, respectively. List filters
//! must be a JSON object. Context rejections expose `owner` as a string or null
//! and always report `effects: 'none'`. Owner metadata is never shortened;
//! feedback uses an explicit truncated fallback if the metadata cannot fit.
//!
//! Granted `cocoa.spawn({name, prompt, alias?, model?, thinking_level?})` resolves to a frozen, nonthenable
//! opaque handle with read-only `name` and `alias` metadata. Handles survive cells
//! but contain no cell Promise and cannot be reconstructed from copied metadata.
//! `cocoa.wait(handle, {timeout_ms?})` resolves to one tagged outcome; passing an
//! object of own data-property aliases to handles returns keyed outcomes instead.
//! Empty groups are valid; groups reject arrays, symbols and accessors. Optional
//! `timeout_ms` is a `u32`, including zero. `cocoa.cancel(handle)` resolves to the
//! host's `{name, state}` snapshot. These methods reject invalid handles before
//! host dispatch, and generic `capabilities.call` rejects management methods.
//! Printing a handle, including a nested handle, fails strict conversion. Copy
//! its metadata into ordinary JSON data to print it. All usual bounds still apply.
//!
//! The direct namespace reflects the initial grant catalog; the host must still
//! validate current grants on every call. Arguments and returned values
//! must be finite JSON data. The boundary rejects functions, symbols, accessors,
//! cycles, unsupported objects and nested undefined rather than silently losing
//! data. There is no module loader, native filesystem, process, network or timer
//! binding. The embedding capability host supplies any granted external work.
//!
//! `print(value)` snapshots finite JSON data without invoking getters or `toJSON`.
//! Multiple arguments form one array entry; zero arguments emit nothing. Accepted
//! calls populate ordered `CellResponse::printed` on both success and failure;
//! provisional observations can be dropped without affecting canonical feedback.
//! Print overflow appends a final `{truncated:true,json_prefix?:string}` notice,
//! preserving earlier prints and continuing execution without further print output.
//! The optional prefix is incomplete JSON text, not the original value. Retained
//! JavaScript values are unchanged. A snapshot overflow omits the prefix; invalid
//! values still fail. Printed budgets below 20 bytes cannot fit the notice and
//! retain the output-limit failure behavior. Unhandled promise rejections fail. All
//! admitted host calls and their microtasks settle before recoverable feedback,
//! including calls that the script did not await.
//!
//! Fixed limits are 256 KiB source, 16 pending host calls, at most 256 nonempty
//! exact prints plus one overflow notice, 32 arguments per print, and conversion
//! depth 32 with 4096 nodes.
//! There is no fixed total host-call count per cell. The engine's optional
//! operation budget, cell deadline and canonical store limits still apply.
//! Callers explicitly select feedback byte/error/count bounds.
//! Command, host-request and provisional-print queues hold 1, 16 and 32 items.
//! Arguments must fit half the core output budget, capped at 128 KiB, as encoded
//! UTF-8 JSON including escaping, keys and punctuation. Conversion rejects
//! oversize arguments before host dispatch with the byte ceiling and a bounded
//! path where available.
//! Print conversion uses bounded snapshots and actual encoded JSON sizes.
//! Printed JSON has its own ceiling, including array brackets and separators.
//! Where the ceiling permits, 19 bytes are reserved for an overflow notice and
//! its separator, so prior prints never need to be replaced.
//! Feedback separately reserves the error bound and 39 other framing bytes.
//! The effective error reserve is capped at a quarter of the whole-cell budget,
//! but never below 64 bytes. The whole-cell budget must be at least 128 bytes.
//! A failed operation's original method and effect evidence survive retained errors.
//!
//! Cancellation and deadlines interrupt busy JavaScript. Dropping execution
//! signals cancellation; explicit, retryable shutdown joins the owned worker.
//! Drop alone requests termination but does not confirm it. Interruption retires
//! the private runtime, discarding all bindings, closures and pending JavaScript
//! work. Successful cells and ordinary errors preserve the live context.
//! External effects are never rolled back or replayed. The shared backend is
//! never shut down here.

use std::thread::{self, JoinHandle};

use async_trait::async_trait;
use cocoa_core::{
    capability::{CapabilityError, OperationContext},
    execution::{
        Cell, CellExecutor, CellResponse, ExecutionError, FeedbackError, RuntimeState,
        ShutdownReport, WorkLimits,
    },
};
use futures_util::{StreamExt, stream::FuturesUnordered};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

mod bindings;
mod runtime;

/// QuickJS heap and JavaScript stack bounds, in bytes.
///
/// The heap must be at least 1 MiB. The JavaScript stack must be 64 KiB through
/// 1 MiB, leaving headroom on the worker's fixed 2 MiB native stack.
#[derive(Clone, Copy)]
pub struct QuickJsOptions {
    /// QuickJS-managed heap limit, not a whole-process resident-memory limit.
    pub memory_bytes: usize,
    /// QuickJS's stack limit, separate from the worker's native stack allocation.
    pub stack_bytes: usize,
    /// Explicit selected feedback bounds, separate from host/model response bounds.
    pub feedback: FeedbackLimits,
}

/// Encoded JSON bounds for one cell, including status, prints and error framing.
/// `output_bytes` must be 128..=256 KiB, `error_bytes` 96..=output_bytes - 32,
/// `printed_bytes` 2..=256 KiB, and `print_calls` 1..=256.
/// Error space and framing are reserved separately from the printed-array ceiling.
/// A smaller whole-cell budget lowers the effective printed ceiling.
#[derive(Clone, Copy, Debug)]
pub struct FeedbackLimits {
    /// Whole encoded response ceiling, also capped by the engine's output bound.
    pub output_bytes: usize,
    /// Encoded `printed` array ceiling, including brackets and separators.
    pub printed_bytes: usize,
    /// Entire encoded error-object ceiling, reserved before accepting prints.
    pub error_bytes: usize,
    /// Maximum exact nonempty prints, followed by at most one overflow notice.
    /// `print()` does not consume this budget.
    pub print_calls: usize,
}

/// Failure to construct a private JavaScript executor.
#[derive(Debug, thiserror::Error)]
pub enum QuickJsError {
    #[error("invalid QuickJS heap, stack or feedback bounds")]
    InvalidOptions,
    /// No worker was started; there is no native execution to settle.
    #[error("QuickJS worker could not start: {message}")]
    WorkerStart { message: String },
    /// Runtime initialization failed and the worker was joined.
    #[error("QuickJS initialization failed: {message}")]
    InitializationSettled { message: String },
    /// A worker was started and its cleanup has not been confirmed.
    #[error("QuickJS initialization failed: {message}")]
    Initialization { message: String },
}

/// One engine's private JavaScript state and execution worker.
pub struct QuickJsExecutor {
    commands: Option<mpsc::Sender<Command>>,
    worker: Option<JoinHandle<()>>,
    stop: CancellationToken,
    feedback: FeedbackLimits,
    interrupted: bool,
}

impl QuickJsExecutor {
    /// Construct the runtime on its private execution thread.
    ///
    /// # Errors
    /// Reject invalid bounds or runtime initialization failure. Initialization and
    /// failed-start cleanup each have a five-second deadline. Await this constructor
    /// to completion to receive worker-settlement evidence.
    pub async fn new(options: QuickJsOptions) -> Result<Self, QuickJsError> {
        if options.memory_bytes < 1024 * 1024
            || !(64 * 1024..=1024 * 1024).contains(&options.stack_bytes)
            || !(128..=256 * 1024).contains(&options.feedback.output_bytes)
            || !(2..=256 * 1024).contains(&options.feedback.printed_bytes)
            || !(96..=options.feedback.output_bytes.saturating_sub(32))
                .contains(&options.feedback.error_bytes)
            || !(1..=256).contains(&options.feedback.print_calls)
        {
            return Err(QuickJsError::InvalidOptions);
        }
        let (commands, receiver) = mpsc::channel::<Command>(1);
        let (ready, initialized) = oneshot::channel();
        let stop = CancellationToken::new();
        let worker_stop = stop.clone();
        let feedback = options.feedback;
        let worker = thread::Builder::new()
            .name("cocoa-quickjs".into())
            .stack_size(2 * 1024 * 1024)
            .spawn(move || runtime::worker(options, receiver, ready, worker_stop))
            .map_err(|error| QuickJsError::WorkerStart {
                message: error.to_string(),
            })?;
        let mut executor = Self {
            commands: Some(commands),
            worker: Some(worker),
            stop,
            feedback,
            interrupted: false,
        };
        let initialized =
            match tokio::time::timeout(std::time::Duration::from_secs(5), initialized).await {
                Ok(Ok(result)) => result,
                Ok(Err(error)) => Err(QuickJsError::Initialization {
                    message: error.to_string(),
                }),
                Err(_) => Err(QuickJsError::Initialization {
                    message: "worker startup timed out".into(),
                }),
            };
        let Err(error) = initialized else {
            return Ok(executor);
        };
        match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            executor.shutdown(CancellationToken::new()),
        )
        .await
        {
            Ok(Ok(_)) => Err(QuickJsError::InitializationSettled {
                message: error.to_string(),
            }),
            Ok(Err(cleanup)) => Err(QuickJsError::Initialization {
                message: format!("{error}; cleanup failed: {cleanup}"),
            }),
            Err(_) => Err(QuickJsError::Initialization {
                message: format!("{error}; cleanup timed out"),
            }),
        }
    }
}

impl Drop for QuickJsExecutor {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

#[async_trait]
impl CellExecutor for QuickJsExecutor {
    fn description(&self, limits: WorkLimits) -> Result<String, ExecutionError> {
        let feedback = match self.feedback.effective(limits.output_bytes) {
            Ok(feedback) => feedback,
            Err(error) => {
                return Ok(format!(
                    "Cell execution unavailable under these engine limits: {error}. Do not submit cells."
                ));
            }
        };
        Ok(format!(
            "Effective cell feedback limits: {} encoded UTF-8 JSON bytes across all prints; {} exact nonempty prints; {} encoded bytes for errors; {} encoded bytes for the whole response. Overflow appends a final {{truncated:true,json_prefix?:string}} notice and omits later prints in that cell; execution continues and retained values stay unchanged. json_prefix is partial JSON text, not a complete value. The notice reserves 19 bytes. A print budget below 20 bytes fails on overflow instead. Invalid values and host failures still fail the cell.",
            feedback.printed_bytes,
            feedback.print_calls,
            feedback.error_bytes,
            feedback.output_bytes,
        ))
    }

    async fn execute(
        &mut self,
        cell: Cell,
        operations: &OperationContext,
        cancellation: CancellationToken,
    ) -> Result<CellResponse, ExecutionError> {
        if self.interrupted {
            return Err(unavailable("interrupted runtime requires settlement"));
        }
        let feedback = self.feedback.effective(operations.limits().output_bytes)?;
        if cell.code.len() > 256 * 1024 {
            return Ok(CellResponse::Failed {
                error: bindings::Bindings::bounded(
                    FeedbackError::Javascript {
                        message: "cell source limit exceeded".into(),
                        truncated: false,
                    },
                    feedback,
                ),
                printed: Vec::new(),
            });
        }
        let cancellation = cancellation.child_token();
        let _guard = cancellation.clone().drop_guard();
        let (reply, mut result) = oneshot::channel();
        let (prints, mut observations) = mpsc::channel(32);
        let (host, mut requests) = mpsc::channel::<HostRequest>(16);
        let mut calls = FuturesUnordered::new();
        self.commands
            .as_ref()
            .ok_or_else(|| unavailable("executor stopped"))?
            .try_send(Command::Execute(CellCommand {
                cell,
                methods: operations
                    .descriptions()
                    .into_iter()
                    .filter_map(|description| {
                        matches!(
                            description.name.as_str(),
                            "cocoa.read"
                                | "cocoa.write"
                                | "cocoa.edit"
                                | "cocoa.run"
                                | "cocoa.wait"
                                | "cocoa.cancel"
                                | "cocoa.check"
                                | "cocoa.spawn"
                                | "cocoa.mcp_tools"
                                | "cocoa.web_search"
                                | "cocoa.load_skill"
                                | "context.get"
                                | "context.set"
                                | "context.list"
                        )
                        .then_some(description.name)
                    })
                    .collect(),
                bindings: operations.bindings(),
                limits: operations.limits(),
                feedback,
                cancellation,
                prints,
                host,
                reply,
            }))
            .map_err(|error| unavailable(error.to_string()))?;
        self.interrupted = true;
        loop {
            tokio::select! {
                result = &mut result => {
                    while calls.next().await.is_some() {}
                    while let Ok(text) = observations.try_recv() { operations.printed(&text); }
                    let result = result.map_err(|error| unavailable(error.to_string()))?;
                    if matches!(result, Ok(CellResponse::Completed { .. } | CellResponse::Failed { .. })) {
                        self.interrupted = false;
                    }
                    return result;
                },
                Some(text) = observations.recv() => operations.printed(&text),
                Some(request) = requests.recv(), if calls.len() < 16 => calls.push(async move {
                    let result = operations.call(&request.name, request.arguments).await;
                    if request.reply.send(result).is_err() { tracing::debug!("capability receiver stopped"); }
                }),
                Some(()) = calls.next(), if !calls.is_empty() => {},
            }
        }
    }

    async fn cancel_turn(&mut self) -> Result<RuntimeState, ExecutionError> {
        let (reply, result) = oneshot::channel();
        self.commands
            .as_ref()
            .ok_or_else(|| unavailable("executor stopped"))?
            .try_send(Command::Cancel { reply })
            .map_err(|error| unavailable(error.to_string()))?;
        result
            .await
            .map_err(|error| unavailable(error.to_string()))??;
        self.interrupted = false;
        Ok(RuntimeState::Reset)
    }

    async fn shutdown(
        &mut self,
        cancellation: CancellationToken,
    ) -> Result<ShutdownReport, ExecutionError> {
        self.stop.cancel();
        self.commands.take();
        while self
            .worker
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
        {
            tokio::select! {
                () = cancellation.cancelled() => return Err(unavailable("shutdown cancelled")),
                () = tokio::time::sleep(std::time::Duration::from_millis(1)) => {}
            }
        }
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| unavailable("QuickJS worker panicked"))?;
        }
        Ok(ShutdownReport { unresolved_work: 0 })
    }
}

impl FeedbackLimits {
    fn effective(self, output_bytes: usize) -> Result<Self, ExecutionError> {
        let output_bytes = output_bytes.min(self.output_bytes);
        if output_bytes < 128 {
            return Err(CapabilityError::OutputLimit.into());
        }
        let error_bytes = self.error_bytes.min((output_bytes / 4).max(64));
        Ok(Self {
            output_bytes,
            printed_bytes: self.printed_bytes.min(output_bytes - error_bytes - 39),
            error_bytes,
            print_calls: self.print_calls,
        })
    }
}

enum Command {
    Execute(CellCommand),
    Cancel {
        reply: oneshot::Sender<Result<(), ExecutionError>>,
    },
}

struct CellCommand {
    cell: Cell,
    methods: Vec<String>,
    bindings: Vec<cocoa_core::capability::CapabilityBinding>,
    limits: WorkLimits,
    feedback: FeedbackLimits,
    cancellation: CancellationToken,
    prints: mpsc::Sender<String>,
    host: mpsc::Sender<HostRequest>,
    reply: oneshot::Sender<Result<CellResponse, ExecutionError>>,
}

struct HostRequest {
    name: String,
    arguments: serde_json::Value,
    reply: oneshot::Sender<Result<serde_json::Value, CapabilityError>>,
}

fn unavailable(message: impl Into<String>) -> ExecutionError {
    ExecutionError::Unavailable {
        message: message.into(),
    }
}

fn encoded_fits(value: &impl serde::Serialize, bytes: usize) -> bool {
    serde_json::to_writer(ByteLimit { remaining: bytes }, value).is_ok()
}

struct ByteLimit {
    remaining: usize,
}

impl std::io::Write for ByteLimit {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.remaining = self
            .remaining
            .checked_sub(bytes.len())
            .ok_or_else(|| std::io::Error::other("encoded byte limit exceeded"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
