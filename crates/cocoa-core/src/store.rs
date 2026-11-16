//! Canonical bounded records, independent of the lossy observation channel.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path},
    sync::atomic::{AtomicI64, Ordering},
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::{
    capability::OperationOutcome,
    compaction::{Checkpoint, StoredCheckpoint},
    context::{InstructionContent, InstructionDelivery, InstructionState},
    execution::{CellResponse, RuntimeState},
    model::{ModelResponse, UserInput},
};

/// Database-local agent identity, independent of a nonunique display name.
/// JSON uses decimal strings to preserve identities above JavaScript's 2^53 bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AgentId(i64);

impl TryFrom<i64> for AgentId {
    type Error = IdentityError;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        if value <= 0 {
            return Err(IdentityError::NonPositive);
        }
        Ok(Self(value))
    }
}

impl From<AgentId> for i64 {
    fn from(value: AgentId) -> Self {
        value.0
    }
}

impl TryFrom<String> for AgentId {
    type Error = IdentityError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let number: i64 = value.parse().map_err(|_| IdentityError::NonPositive)?;
        if number.to_string() != value {
            return Err(IdentityError::NonPositive);
        }
        Self::try_from(number)
    }
}

impl From<AgentId> for String {
    fn from(value: AgentId) -> Self {
        value.0.to_string()
    }
}

/// Immutable record identity within a backend, not its history position.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "i64", into = "i64")]
pub struct RecordId(i64);

impl TryFrom<i64> for RecordId {
    type Error = IdentityError;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        if value <= 0 {
            return Err(IdentityError::NonPositive);
        }
        Ok(Self(value))
    }
}

impl From<RecordId> for i64 {
    fn from(value: RecordId) -> Self {
        value.0
    }
}

/// One-based execution-record acceptance order within an agent's retained history.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "i64", into = "i64")]
pub struct RecordSequence(i64);

impl TryFrom<i64> for RecordSequence {
    type Error = IdentityError;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        if value <= 0 {
            return Err(IdentityError::NonPositive);
        }
        Ok(Self(value))
    }
}

impl From<RecordSequence> for i64 {
    fn from(value: RecordSequence) -> Self {
        value.0
    }
}

/// Checked identities cannot represent zero or negative SQL integers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum IdentityError {
    #[error("identity must be a positive signed 64-bit integer")]
    NonPositive,
}

/// Identity within one engine's conversation. A new engine starts at one and
/// requires an empty, exclusive store namespace. Reusing a nonempty store with a
/// fresh engine is not resume and would collide with retained identities.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvocationId(pub u64);

/// An operation identity is unique within its invocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct OperationId(pub u64);

/// An immutable canonical fact. An unmatched start means an unknown outcome.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub invocation: InvocationId,
    pub kind: RecordKind,
}

impl Record {
    /// Check that invocation and operation identities fit positive SQL integers.
    /// This does not check encoded size, store capacity or instruction state.
    ///
    /// # Errors
    /// Return [`StoreError::InvalidRecord`] for a zero identity or one above
    /// the signed 64-bit integer range.
    pub fn validate(&self) -> Result<(), StoreError> {
        if self.invocation.0 == 0 || i64::try_from(self.invocation.0).is_err() {
            return Err(StoreError::InvalidRecord);
        }
        if let RecordKind::OperationStarted { operation, .. }
        | RecordKind::OperationFinished { operation, .. } = &self.kind
            && (operation.0 == 0 || i64::try_from(operation.0).is_err())
        {
            return Err(StoreError::InvalidRecord);
        }
        Ok(())
    }
}

/// Rich context includes provider continuations and structured operation results.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum RecordKind {
    InputAccepted {
        input: UserInput,
    },
    ModelCompleted {
        response: ModelResponse,
    },
    /// Committed before execution. `cell` is a zero-based ordinal in `model`.
    CellStarted {
        model: RecordId,
        cell: u32,
    },
    CellCompleted {
        model: RecordId,
        cell: u32,
        code: String,
        outcome: CellResponse,
    },
    InvocationFinished {
        outcome: InvocationOutcome,
    },
    TurnCancelled {
        runtime: RuntimeState,
    },
    OperationStarted {
        operation: OperationId,
        name: String,
        arguments: serde_json::Value,
    },
    OperationFinished {
        operation: OperationId,
        outcome: OperationOutcome,
    },
}

/// Why an invocation stopped before finishing on its own.
///
/// Deadline causes record which bound fired, so resumed history tells a silent model
/// stream apart from a runaway cell. `Unspecified` marks failures that reached the
/// caller without crossing a stall bound, such as budget or store errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FailureCause {
    /// The invocation reported no progress within the stall window.
    Stalled { stall_timeout_ms: u64 },
    /// The model stream reported no progress within the stall window.
    ModelStream { stall_timeout_ms: u64 },
    /// One JavaScript cell exceeded the stall window.
    Cell { stall_timeout_ms: u64 },
    /// One capability call exceeded its deadline, which the capability host owns.
    Capability,
    /// The failure came from something other than a stall bound.
    Unspecified,
}

impl std::fmt::Display for FailureCause {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stalled { stall_timeout_ms } => {
                write!(formatter, "no progress within {stall_timeout_ms}ms")
            }
            Self::ModelStream { stall_timeout_ms } => {
                write!(formatter, "model stream stalled for {stall_timeout_ms}ms")
            }
            Self::Cell { stall_timeout_ms } => {
                write!(formatter, "JavaScript cell exceeded {stall_timeout_ms}ms")
            }
            Self::Capability => formatter.write_str("capability deadline exhausted"),
            Self::Unspecified => formatter.write_str("invocation failed"),
        }
    }
}

/// Terminal evidence for one invocation, independent of its last model response.
/// Interrupted is appended by host recovery, never an instruction to replay work.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InvocationOutcome {
    Completed,
    /// The invocation stopped without finishing; the cause is recorded for resume.
    Failed {
        cause: FailureCause,
    },
    Cancelled,
    Interrupted,
}

/// A canonical fact with stable identity and explicit per-agent ordering.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredRecord {
    pub id: RecordId,
    pub sequence: RecordSequence,
    pub record: Record,
}

/// Ordered history and instruction state observed at the same acceptance boundary.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct StoreSnapshot {
    pub records: Vec<StoredRecord>,
    pub instruction_state: Option<InstructionState>,
    /// Ordered agent-control publications observed at the same acceptance boundary.
    pub checkpoints: Vec<StoredCheckpoint>,
}

/// Selective, coherent input for a running engine, not a full recovery snapshot.
/// Records keep their original identities and sequences. Before the checkpoint's
/// coverage boundary they may be sparse; after it they are complete and contiguous.
/// This type must never be supplied to full-history resume or checkpoint restoration.
#[derive(Clone, Debug, PartialEq)]
pub struct ActiveSnapshot {
    records: Vec<StoredRecord>,
    instruction_state: Option<InstructionState>,
    checkpoint: Option<StoredCheckpoint>,
    suffix_start: usize,
}

impl ActiveSnapshot {
    /// Accept an adapter's indexed selection from one read transaction.
    /// `checkpoint` must be the latest publication in this namespace, previously
    /// validated through full [`Checkpoint::restore`] or host preparation and
    /// accepted publication. A durable adapter can cache this immutable typed
    /// value during full load/resume and publication. An unknown or changed head
    /// requires full validation, never unchecked decoding or an earlier fallback.
    ///
    /// `records` must be ordered by sequence without duplicates. Include the
    /// checkpoint's [`Checkpoint::source_ids`], every subsequent original through
    /// the observed tail, all input records in the tail invocation and in the
    /// first suffix invocation, and every input containing captured images from
    /// the entire history. Also include instruction-feedback target originals.
    /// Without a checkpoint, include the complete history starting at sequence one.
    /// Instruction state and the publication head must share the records' read
    /// boundary. Do not evict originals or infer missing image recovery support.
    ///
    /// The constructor checks ordering, suffix gaps, named sources and feedback.
    /// Only the adapter can establish tail completeness, latest-head selection,
    /// absence of omitted images/inputs and namespace membership.
    ///
    /// # Errors
    /// Reject malformed records, missing checkpoint sources, suffix gaps or invalid
    /// instruction state. This does not revalidate a checkpoint against sparse data.
    pub fn new(
        records: Vec<StoredRecord>,
        instruction_state: Option<InstructionState>,
        checkpoint: Option<StoredCheckpoint>,
    ) -> Result<Self, StoreError> {
        let mut identities = BTreeSet::new();
        let mut sequence_previous = 0_i64;
        for stored in &records {
            stored.record.validate()?;
            let sequence = i64::from(stored.sequence);
            if sequence <= sequence_previous || !identities.insert(stored.id) {
                return Err(StoreError::InvalidRecord);
            }
            sequence_previous = sequence;
        }
        let suffix_start = if let Some(stored) = &checkpoint {
            if identities.contains(&stored.id) {
                return Err(StoreError::InvalidCheckpoint);
            }
            let cut = records
                .iter()
                .position(|record| record.id == stored.checkpoint.content().through)
                .ok_or(StoreError::InvalidCheckpoint)?;
            let sources: BTreeSet<_> = records[..=cut].iter().map(|record| record.id).collect();
            if stored
                .checkpoint
                .source_ids()?
                .iter()
                .any(|source| !sources.contains(source))
                || !identities.contains(&stored.checkpoint.after())
            {
                return Err(StoreError::InvalidCheckpoint);
            }
            cut + 1
        } else {
            0
        };
        let mut sequence = if suffix_start == 0 {
            0
        } else {
            i64::from(records[suffix_start - 1].sequence)
        };
        for record in &records[suffix_start..] {
            sequence = sequence.checked_add(1).ok_or(StoreError::InvalidRecord)?;
            if i64::from(record.sequence) != sequence {
                return Err(StoreError::InvalidRecord);
            }
        }
        if let Some(state) = &instruction_state {
            state.validate_bounds(usize::MAX)?;
            if state.invocation.0 == 0
                || records
                    .last()
                    .is_none_or(|record| state.invocation.0 > record.record.invocation.0)
            {
                return Err(StoreError::InvalidRecord);
            }
            let completed_cells = records
                .iter()
                .filter(|record| matches!(record.record.kind, RecordKind::CellCompleted { .. }))
                .map(|record| (record.id, record.record.invocation))
                .collect();
            state.validate_feedback_targets(&completed_cells)?;
        }
        let active = Self {
            records,
            instruction_state,
            checkpoint,
            suffix_start,
        };
        if let Some(stored) = &active.checkpoint {
            stored
                .checkpoint
                .validate_active_sources(&active.records[..active.suffix_start])
                .map_err(|_| StoreError::InvalidCheckpoint)?;
        }
        Ok(active)
    }

    /// Selected originals, including the complete suffix, in acceptance order.
    #[must_use]
    pub fn records(&self) -> &[StoredRecord] {
        &self.records
    }

    /// Every original strictly after checkpoint coverage, or all history if absent.
    /// Checkpoint cuts never split a pending model/cell group.
    #[must_use]
    pub fn suffix(&self) -> &[StoredRecord] {
        &self.records[self.suffix_start..]
    }

    /// Latest validated publication at this read boundary.
    #[must_use]
    pub fn checkpoint(&self) -> Option<&StoredCheckpoint> {
        self.checkpoint.as_ref()
    }

    /// Accepted instruction delivery and feedback at this read boundary.
    #[must_use]
    pub fn instruction_state(&self) -> Option<&InstructionState> {
        self.instruction_state.as_ref()
    }
}

impl StoreSnapshot {
    /// Consume history for a projection that does not use stable references.
    /// This deliberately discards record identities and instruction state.
    #[must_use]
    pub fn into_records(self) -> Vec<Record> {
        self.records
            .into_iter()
            .map(|stored| stored.record)
            .collect()
    }

    /// Validate complete checkpoint history, then select the running-engine view.
    /// Compatibility adapters can use this; indexed adapters should select the
    /// documented [`ActiveSnapshot::new`] inputs directly instead.
    ///
    /// # Errors
    /// Reject invalid checkpoint history or invalid active-view inputs.
    pub fn into_active(self) -> Result<ActiveSnapshot, StoreError> {
        self.validate_checkpoints()
            .map_err(|_| StoreError::InvalidCheckpoint)?;
        let checkpoint = self.checkpoints.into_iter().last();
        let records = if let Some(stored) = &checkpoint {
            let mut selected: BTreeSet<_> = stored.checkpoint.source_ids()?.into_iter().collect();
            for index in stored
                .checkpoint
                .retained_indexes(&self.records)
                .map_err(|_| StoreError::InvalidCheckpoint)?
            {
                selected.insert(self.records[index].id);
            }
            if let Some(state) = &self.instruction_state {
                selected.extend(
                    state
                        .scoped_feedback
                        .iter()
                        .chain(&state.skill_feedback)
                        .map(|note| note.record),
                );
            }
            self.records
                .into_iter()
                .filter(|record| selected.contains(&record.id))
                .collect()
        } else {
            self.records
        };
        ActiveSnapshot::new(records, self.instruction_state, checkpoint)
    }
}

/// Retain bounded history for one agent. Shared ownership permits inspection,
/// not concurrent engine writers. Fresh engines require empty namespaces;
/// retained history uses [`crate::engine::Engine::resume`].
///
/// Append must be atomic on rejection. After cancellation during an append,
/// adapters may have committed it; callers must not automatically retry effects.
#[async_trait]
pub trait Store: Send + Sync {
    /// Optional durable accounting, independent of replay history. Unsupported
    /// backends still receive invocation-local totals from the engine.
    fn request_recorder(
        &self,
        _invocation: InvocationId,
    ) -> Option<std::sync::Arc<dyn crate::accounting::RequestRecorder>> {
        None
    }

    /// Accept a fact and optional replacement state atomically, then return its ID.
    /// `None` preserves state. Instruction-aware model responses must supply state.
    /// Only completed model responses can establish instruction delivery. No history
    /// is evicted, and failed validation or admission changes neither history nor state.
    ///
    /// # Errors
    /// Reject invalid identities/state, missing required state, exhausted capacity
    /// or backend failures. A backend error can represent an unknown write outcome.
    async fn append(
        &self,
        record: &Record,
        instruction_state: Option<&InstructionState>,
    ) -> Result<RecordId, StoreError>;

    /// Publish a validated checkpoint without changing execution or instruction state.
    /// Reject a stale predecessor or original-history boundary atomically. Backends
    /// that cannot retain control evidence must reject rather than pretend success.
    ///
    /// # Errors
    /// Return unsupported, stale/invalid publication, capacity or backend failures.
    /// An unknown durable write outcome requires reconciliation, never blind retry.
    async fn publish_checkpoint(&self, _checkpoint: &Checkpoint) -> Result<RecordId, StoreError> {
        Err(StoreError::CheckpointUnsupported)
    }

    /// Return a coherent, complete snapshot, including unmatched operation starts.
    ///
    /// # Errors
    /// Propagate backend failures and invalid retained state without dropping data.
    async fn load(&self) -> Result<StoreSnapshot, StoreError>;

    /// Read the latest validated checkpoint, complete suffix and required originals
    /// coherently, without promising a complete historical snapshot.
    /// Indexed adapters should read the head, coverage sequence, suffix, source IDs,
    /// retained input/image records and instruction state in one transaction. See
    /// [`ActiveSnapshot::new`] for the exact selection and validation obligations.
    /// The default preserves compatibility by validating and selecting a full load.
    /// Full [`Self::load`] and resume/recovery retain their existing contracts.
    ///
    /// # Errors
    /// Reject invalid retained state or an unvalidated head; propagate read failures.
    async fn load_active(&self) -> Result<ActiveSnapshot, StoreError> {
        self.load().await?.into_active()
    }
}

/// Canonical history has no total byte or record-count cap; only each record is
/// bounded. `record_bytes` counts canonical [`Record`] JSON and must be positive.
/// No eviction or implicit compaction occurs.
#[derive(Clone, Copy, Debug)]
pub struct StoreLimits {
    pub record_bytes: usize,
}

impl StoreLimits {
    /// Check that the per-record limit is positive.
    ///
    /// # Errors
    /// Return [`StoreError::InvalidLimits`] when `record_bytes` is zero.
    pub fn validate(&self) -> Result<(), StoreError> {
        if self.record_bytes == 0 {
            return Err(StoreError::InvalidLimits);
        }
        Ok(())
    }
}

/// A process-local store. Record IDs are unique across memory stores in this
/// process; sequence numbers start at one independently in each store.
pub struct InMemoryStore {
    limits: StoreLimits,
    instruction_state_required: bool,
    state: Mutex<MemoryState>,
}

impl InMemoryStore {
    /// Construct an empty store without IO.
    ///
    /// # Errors
    /// Reject a zero per-record byte limit.
    pub fn new(limits: StoreLimits) -> Result<Self, StoreError> {
        limits.validate()?;
        Ok(Self {
            limits,
            instruction_state_required: false,
            state: Mutex::new(MemoryState {
                records: Vec::new(),
                checkpoints: Vec::new(),
                instruction_state: None,
            }),
        })
    }

    /// Construct a store whose model responses require prepared instruction state
    /// from the first response. Ordinary record appends can still preserve state.
    ///
    /// # Errors
    /// Reject invalid limits as in [`Self::new`].
    pub fn with_instruction_state(limits: StoreLimits) -> Result<Self, StoreError> {
        Ok(Self {
            instruction_state_required: true,
            ..Self::new(limits)?
        })
    }
}

#[async_trait]
impl Store for InMemoryStore {
    async fn append(
        &self,
        record: &Record,
        instruction_state: Option<&InstructionState>,
    ) -> Result<RecordId, StoreError> {
        record.validate()?;
        let bytes = encoded_size(record)?;
        if bytes > self.limits.record_bytes {
            return Err(StoreError::RecordLimit);
        }
        if let Some(candidate) = instruction_state {
            candidate.validate_bounds(usize::MAX)?;
        }
        let mut state = self.state.lock().await;
        let model_completed = matches!(record.kind, RecordKind::ModelCompleted { .. });
        if model_completed
            && (self.instruction_state_required || state.instruction_state.is_some())
            && instruction_state.is_none()
        {
            return Err(StoreError::InstructionStateRequired);
        }
        if let Some(candidate) = instruction_state {
            let completed_cells: BTreeMap<_, _> = state
                .records
                .iter()
                .filter(|stored| matches!(stored.record.kind, RecordKind::CellCompleted { .. }))
                .map(|stored| (stored.id, stored.record.invocation))
                .collect();
            candidate.validate_transition(
                record,
                state.instruction_state.as_ref(),
                &completed_cells,
            )?;
        }
        let sequence = state.records.last().map_or(Ok(1), |stored| {
            stored
                .sequence
                .0
                .checked_add(1)
                .ok_or(StoreError::IdentityExhausted)
        })?;
        let previous = RECORD_ID_LAST
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |last| {
                last.checked_add(1)
            })
            .map_err(|_| StoreError::IdentityExhausted)?;
        let id = RecordId(previous + 1);
        state.records.push(StoredRecord {
            id,
            sequence: RecordSequence(sequence),
            record: record.clone(),
        });
        if let Some(candidate) = instruction_state {
            state.instruction_state = Some(candidate.clone());
        }
        Ok(id)
    }

    async fn publish_checkpoint(&self, checkpoint: &Checkpoint) -> Result<RecordId, StoreError> {
        let bytes = encoded_size(checkpoint)?;
        if bytes > self.limits.record_bytes {
            return Err(StoreError::RecordLimit);
        }
        let mut state = self.state.lock().await;
        if state.records.last().map(|record| record.id) != Some(checkpoint.after()) {
            return Err(StoreError::InvalidCheckpoint);
        }
        checkpoint
            .validate_publication(&state.records, state.checkpoints.last())
            .map_err(|_| StoreError::InvalidCheckpoint)?;
        let previous = RECORD_ID_LAST
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |last| {
                last.checked_add(1)
            })
            .map_err(|_| StoreError::IdentityExhausted)?;
        let id = RecordId(previous + 1);
        state.checkpoints.push(StoredCheckpoint {
            id,
            checkpoint: checkpoint.clone(),
        });
        Ok(id)
    }

    async fn load(&self) -> Result<StoreSnapshot, StoreError> {
        let state = self.state.lock().await;
        Ok(StoreSnapshot {
            records: state.records.clone(),
            checkpoints: state.checkpoints.clone(),
            instruction_state: state.instruction_state.clone(),
        })
    }
}

/// Store failures are never converted into UI notifications or cell feedback.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
pub enum StoreError {
    #[error("invalid store limits")]
    InvalidLimits,
    #[error("record exceeds byte limit")]
    RecordLimit,
    #[error("record has an invalid invocation or operation identity")]
    InvalidRecord,
    #[error("store does not support checkpoint control publications")]
    CheckpointUnsupported,
    #[error("checkpoint publication is stale or inconsistent with this store")]
    InvalidCheckpoint,
    #[error("store identity space exhausted")]
    IdentityExhausted,
    #[error("completed model response requires prepared instruction state")]
    InstructionStateRequired,
    #[error(transparent)]
    InstructionState(#[from] InstructionStateError),
    #[error("store capacity exhausted")]
    Capacity,
    #[error("record encoding failed")]
    Encoding,
    #[error("store backend failed: {message}")]
    Backend { message: String },
}

/// Instruction-state acceptance errors contain no captured instruction bodies.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
pub enum InstructionStateError {
    #[error("instruction state exceeds its collection or byte limits")]
    Limit,
    #[error("instruction state does not belong to the appended invocation")]
    Invocation,
    #[error("instruction source path must be absolute, bounded UTF-8 without parent traversal")]
    SourcePath,
    #[error("instruction state contains duplicate source paths")]
    DuplicateSource,
    #[error("instruction feedback does not target a retained completed cell: {record:?}")]
    FeedbackTarget { record: RecordId },
    #[error("instruction transition loses pending content or claims unaccepted delivery")]
    Delivery,
    #[error("instruction transition changes historical feedback")]
    Feedback,
}

/// Count encoded JSON bytes without allocating the encoded representation.
///
/// # Errors
/// Report serialization failure or a size that cannot fit in memory indexing.
pub fn encoded_size(value: &impl Serialize) -> Result<usize, StoreError> {
    let mut counter = ByteCounter(0);
    serde_json::to_writer(&mut counter, value).map_err(|_| StoreError::Encoding)?;
    Ok(counter.0)
}

impl InstructionState {
    /// Check paths, source uniqueness within each collection and snapshot bounds.
    /// At most 256 scoped sources and 64 skills are allowed. Paths must be absolute
    /// UTF-8, at most 4096 bytes and 256 components, without NUL or parent traversal.
    /// Each body is at most 64 KiB; each body collection totals at most 128 KiB.
    /// Each feedback collection is at most `records_max` notes and 128 KiB of JSON.
    /// Feedback targets and delivery transitions require separate validation.
    ///
    /// # Errors
    /// Return [`InstructionStateError::Limit`], [`InstructionStateError::SourcePath`]
    /// or [`InstructionStateError::DuplicateSource`] for invalid snapshots, wrapped
    /// in [`StoreError::InstructionState`]. Propagate feedback encoding failures.
    pub fn validate_bounds(&self, records_max: usize) -> Result<(), StoreError> {
        const SOURCE_BYTES: usize = 64 * 1024;
        const COLLECTION_BYTES: usize = 128 * 1024;

        if self.scoped_sources.len() > 256
            || self.selected_skills.len() > 64
            || self.scoped_feedback.len() > records_max
            || self.skill_feedback.len() > records_max
        {
            return Err(InstructionStateError::Limit.into());
        }
        let mut paths = BTreeSet::new();
        let mut source_bytes = 0_usize;
        for source in &self.scoped_sources {
            validate_source_path(&source.source_path)?;
            if !paths.insert(&source.source_path) {
                return Err(InstructionStateError::DuplicateSource.into());
            }
            if let InstructionContent::Text(text) = &source.content {
                if text.len() > SOURCE_BYTES {
                    return Err(InstructionStateError::Limit.into());
                }
                source_bytes += text.len();
            }
        }
        if source_bytes > COLLECTION_BYTES {
            return Err(InstructionStateError::Limit.into());
        }
        paths.clear();
        let mut skill_bytes = 0_usize;
        for skill in &self.selected_skills {
            validate_source_path(&skill.source_path)?;
            if !paths.insert(&skill.source_path) {
                return Err(InstructionStateError::DuplicateSource.into());
            }
            if skill.text.len() > SOURCE_BYTES {
                return Err(InstructionStateError::Limit.into());
            }
            skill_bytes += skill.text.len();
        }
        if skill_bytes > COLLECTION_BYTES {
            return Err(InstructionStateError::Limit.into());
        }
        for feedback in [&self.scoped_feedback, &self.skill_feedback] {
            if feedback
                .iter()
                .any(|note| note.text.len() > COLLECTION_BYTES)
                || encoded_size(feedback)? > COLLECTION_BYTES
            {
                return Err(InstructionStateError::Limit.into());
            }
        }
        Ok(())
    }

    /// Check feedback identities without requiring an append or previous state.
    /// The caller must resolve `completed_cells` from retained
    /// [`RecordKind::CellCompleted`] records in the same store namespace, with
    /// their invocations. Each target may appear once in each feedback collection
    /// and must not belong to a later invocation than this state.
    /// This does not check snapshot bounds or delivery transitions.
    ///
    /// # Errors
    /// Return [`InstructionStateError::Feedback`] for duplicate targets, or
    /// [`InstructionStateError::FeedbackTarget`] for missing or future targets,
    /// wrapped in [`StoreError::InstructionState`].
    pub fn validate_feedback_targets(
        &self,
        completed_cells: &BTreeMap<RecordId, InvocationId>,
    ) -> Result<(), StoreError> {
        for feedback in [&self.scoped_feedback, &self.skill_feedback] {
            let mut seen = BTreeSet::new();
            for note in feedback {
                if !seen.insert(note.record) {
                    return Err(InstructionStateError::Feedback.into());
                }
                if completed_cells
                    .get(&note.record)
                    .is_none_or(|invocation| invocation.0 > self.invocation.0)
                {
                    return Err(InstructionStateError::FeedbackTarget {
                        record: note.record,
                    }
                    .into());
                }
            }
        }
        Ok(())
    }

    /// Check replacement state against an append and the previously accepted state.
    /// The invocation must match `record` and must not precede `previous`.
    /// Pending sources and skills must remain present, and historical feedback
    /// must remain an unchanged prefix. Only [`RecordKind::ModelCompleted`] can
    /// establish delivery of new content or append feedback.
    ///
    /// Validate `record` with [`Record::validate`] and snapshot bounds with
    /// [`Self::validate_bounds`] separately. Feedback targets are checked using
    /// [`Self::validate_feedback_targets`]; its caller-resolved map must exclude
    /// the record being appended.
    ///
    /// # Errors
    /// Return [`InstructionStateError::Invocation`] for mismatched or decreasing
    /// invocations, [`InstructionStateError::Delivery`] for invalid delivery or
    /// lost pending entries, or [`InstructionStateError::Feedback`] for changed
    /// history or feedback appended without a completed model response.
    /// Propagate feedback-target errors, all wrapped in [`StoreError::InstructionState`].
    pub fn validate_transition(
        &self,
        record: &Record,
        previous: Option<&InstructionState>,
        completed_cells: &BTreeMap<RecordId, InvocationId>,
    ) -> Result<(), StoreError> {
        if self.invocation != record.invocation
            || previous.is_some_and(|previous| previous.invocation.0 > self.invocation.0)
        {
            return Err(InstructionStateError::Invocation.into());
        }
        self.validate_feedback_targets(completed_cells)?;
        let model_completed = matches!(record.kind, RecordKind::ModelCompleted { .. });
        let sources: BTreeMap<_, _> = self
            .scoped_sources
            .iter()
            .map(|source| (&source.source_path, source))
            .collect();
        let skills: BTreeMap<_, _> = self
            .selected_skills
            .iter()
            .map(|skill| (&skill.source_path, skill))
            .collect();
        let previous_sources: BTreeMap<_, _> = previous
            .into_iter()
            .flat_map(|state| &state.scoped_sources)
            .map(|source| (&source.source_path, source))
            .collect();
        let previous_skills: BTreeMap<_, _> = previous
            .into_iter()
            .flat_map(|state| &state.selected_skills)
            .map(|skill| (&skill.source_path, skill))
            .collect();
        for source in previous_sources.values() {
            if source.delivery == InstructionDelivery::Pending
                && !sources.contains_key(&source.source_path)
            {
                return Err(InstructionStateError::Delivery.into());
            }
        }
        for skill in previous_skills.values() {
            if skill.delivery == InstructionDelivery::Pending
                && !skills.contains_key(&skill.source_path)
            {
                return Err(InstructionStateError::Delivery.into());
            }
        }
        if !model_completed {
            for source in sources.values() {
                if source.delivery == InstructionDelivery::Delivered
                    && previous_sources
                        .get(&source.source_path)
                        .is_none_or(|previous| {
                            previous.delivery != InstructionDelivery::Delivered
                                || previous.content != source.content
                        })
                {
                    return Err(InstructionStateError::Delivery.into());
                }
            }
            for skill in skills.values() {
                if skill.delivery == InstructionDelivery::Delivered
                    && previous_skills
                        .get(&skill.source_path)
                        .is_none_or(|previous| {
                            previous.delivery != InstructionDelivery::Delivered
                                || previous.text != skill.text
                        })
                {
                    return Err(InstructionStateError::Delivery.into());
                }
            }
        }
        for (feedback, previous_feedback) in [
            (
                &self.scoped_feedback,
                previous.map(|state| state.scoped_feedback.as_slice()),
            ),
            (
                &self.skill_feedback,
                previous.map(|state| state.skill_feedback.as_slice()),
            ),
        ] {
            let previous_feedback = previous_feedback.unwrap_or_default();
            if !feedback.starts_with(previous_feedback)
                || (!model_completed && feedback.len() != previous_feedback.len())
            {
                return Err(InstructionStateError::Feedback.into());
            }
        }
        Ok(())
    }
}

static RECORD_ID_LAST: AtomicI64 = AtomicI64::new(0);

struct MemoryState {
    records: Vec<StoredRecord>,
    checkpoints: Vec<StoredCheckpoint>,
    instruction_state: Option<InstructionState>,
}

pub(crate) async fn append_bounded(
    store: &dyn Store,
    record: &Record,
    bytes: usize,
) -> Result<RecordId, StoreError> {
    if encoded_size(record)? > bytes {
        return Err(StoreError::RecordLimit);
    }
    store.append(record, None).await
}

fn validate_source_path(path: &Path) -> Result<(), StoreError> {
    if !path.is_absolute()
        || path
            .to_str()
            .is_none_or(|text| text.len() > 4096 || text.contains('\0'))
        || path.components().take(257).count() > 256
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(InstructionStateError::SourcePath.into());
    }
    Ok(())
}

struct ByteCounter(usize);

impl std::io::Write for ByteCounter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len())
            .ok_or_else(|| std::io::Error::other("encoded size overflow"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
