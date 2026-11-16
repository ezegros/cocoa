//! A headless, single-agent engine with explicit ownership and bounded evidence.
//!
//! Adapters are ready before construction. UI observations are optional and lossy;
//! invocation results and store records are authoritative. `run_with_recovery`
//! permits model-request retries; accepted capability effects are never replayed.
//!
//! # Lifecycle
//!
//! Validate [`engine::EngineSettings`], construct ready [`engine::EngineAdapters`],
//! then call [`engine::Engine::new`] synchronously without IO. [`engine::Engine::run`]
//! borrows the engine mutably and requires a Tokio runtime with time enabled.
//! Await cancellation settlement, then consume the engine with
//! [`engine::Engine::shutdown`]. Do not infer termination from a dropped future.
//!
//! Pre-admission input rejection and already-cancelled requests leave the engine
//! usable. Requested cancellation also permits reuse after successful settlement.
//! Other admitted failures stop the engine to avoid reusing interrupted private
//! runtime state. Recoverable cell failures become model feedback; capability
//! budget and storage failures remain fatal even when an executor catches them.
//!
//! # Evidence and limits
//!
//! [`store::Store`] retains accepted input, complete model responses, cell feedback
//! and capability starts/outcomes. An unmatched start or explicit unknown outcome
//! is not evidence of rollback or termination. Store errors propagate even when
//! an executor catches a capability error. Cleanup attempts worker settlement and
//! record reconciliation independently within one deadline, reporting both failures.
//!
//! Byte limits count UTF-8 input or JSON-encoded canonical data, not exact heap
//! consumption. Counting does not create serialized copies. Adapters must bound
//! their own transport buffers, allocations and CPU work while producing results;
//! async deadlines cannot preempt blocking adapter code. Oversized capability
//! output retains a bounded completion/failure summary, never truncated success.
//!
//! Notifications have separate count/text bounds and explicit lag. They may all
//! be lost without affecting results or evidence. Provider continuations remain
//! opaque JSON in records/context, without provider SDK types or role/text flattening.
//!
//! `Engine::new` requires an initially empty store namespace reserved for its own
//! lifetime. `Engine::resume` validates retained history and starts a fresh runtime.
//! External readers may inspect retained evidence after shutdown. Explicit
//! [`compaction::CompactionPolicy`] enables automatic, foreground checkpoint
//! generation at settled model-round boundaries. Providers must support request
//! accounting and read-only summarization; stores must support publication before
//! activation. The in-memory store implements that contract. This crate supplies
//! no durable checkpoint backend, effect replay, background compaction, or concrete
//! provider, JavaScript, terminal or coding-environment adapter.

pub mod accounting;
pub mod capability;
pub mod compaction;
pub mod context;
pub mod engine;
pub mod execution;
pub mod model;
pub mod store;
