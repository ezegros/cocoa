//! Bounded request projection, separate from canonical history and delivery state.

use std::path::PathBuf;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    capability::CapabilityDescription,
    compaction::{Checkpoint, CheckpointContent, CheckpointError},
    store::{AgentId, InvocationId, Record, RecordId, StoreError, StoredRecord, encoded_size},
};

/// Host-owned instruction snapshots for one agent's current invocation.
/// Stores validate paths, bounds, delivery transitions and retained feedback
/// targets on acceptance. Discovery and filesystem authority remain tool policy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstructionState {
    pub invocation: InvocationId,
    pub scoped_sources: Vec<ScopedInstruction>,
    pub selected_skills: Vec<SelectedSkill>,
    pub scoped_feedback: Vec<InstructionFeedback>,
    pub skill_feedback: Vec<InstructionFeedback>,
}

/// Captured scoped instructions, including a deletion awaiting model delivery.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopedInstruction {
    pub source_path: PathBuf,
    pub content: InstructionContent,
    pub delivery: InstructionDelivery,
}

/// Removal is evidence about an observed source, not an absent snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InstructionContent {
    Text(String),
    Removed,
}

/// A selected skill body, independent of its current catalog entry or file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectedSkill {
    pub source_path: PathBuf,
    pub text: String,
    pub delivery: InstructionDelivery,
}

/// Delivery requires acceptance of a complete model response to the request
/// carrying these instructions. A partial stream does not establish delivery.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InstructionDelivery {
    Pending,
    Delivered,
}

/// Historical note text attached to a retained completed cell by stable identity.
/// Unlike [`FeedbackNote`], this reference does not depend on context projection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstructionFeedback {
    pub record: RecordId,
    pub text: String,
}

/// Current session knowledge, distinct from chronological conversation evidence.
/// Every session participant may read it; only its owner may replace its content.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ContextRecord {
    pub key: String,
    #[serde(flatten)]
    pub content: ContextContent,
    pub owner: AgentId,
}

/// Agent-authored content. Replacement replaces every field; omitted labels
/// become empty. `value: null` is present content, not an absent record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextContent {
    #[serde(rename = "type")]
    pub kind: ContextType,
    pub value: Value,
    #[serde(default)]
    pub labels: Vec<String>,
}

/// Classification of shared working knowledge, without special write authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextType {
    Artifact,
    Decision,
    Constraint,
    Observation,
    Hypothesis,
}

/// An agent-authored shared-data snapshot, not trusted instructions.
/// Omitted records remain readable through the session's context capabilities.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "authority", rename = "agent_authored")]
pub struct SharedContext {
    pub records: Vec<ContextRecord>,
    pub omitted: u32,
}

/// Required history and callable descriptions, including opaque provider data.
#[derive(Clone, Debug)]
pub struct Context {
    pub records: Vec<Record>,
    pub capabilities: Vec<CapabilityDescription>,
    /// Source-backed portable checkpoint, separate from live instructions and replay.
    pub checkpoint: Option<CheckpointContent>,
    /// Application-owned system instructions, separate from chronological history.
    /// The assembler must include their encoded size in its byte budget.
    pub instructions: String,
    /// Application-owned notes appended to execution feedback during projection.
    /// The assembler must budget these and retain them while their records remain.
    pub feedback_notes: Vec<FeedbackNote>,
    /// Current agent-authored session data. Only a changed snapshot is delivered;
    /// accepted snapshots remain in history before the response that consumed them.
    pub shared: Option<SharedContext>,
}

impl Context {
    /// Return the current snapshot only when it differs from the last retained
    /// delivery. After compaction removes that delivery, send the snapshot again.
    #[must_use]
    pub fn shared_update(&self) -> Option<&SharedContext> {
        let current = self.shared.as_ref()?;
        (self.shared_delivered() != Some(current)).then_some(current)
    }

    /// Return the most recent shared snapshot still retained in history.
    #[must_use]
    pub fn shared_delivered(&self) -> Option<&SharedContext> {
        self.records
            .iter()
            .rev()
            .find_map(|record| match &record.kind {
                crate::store::RecordKind::ModelCompleted { shared_context, .. } => {
                    shared_context.as_ref()
                }
                _ => None,
            })
    }

    /// Sum the encoded component sizes used by context assemblers. Empty auxiliary
    /// instructions and notes, and an absent shared snapshot, consume no allowance.
    /// Provider request framing has its own transport byte limit.
    ///
    /// # Errors
    /// Return an encoding error if a component cannot be serialized.
    pub fn encoded_size(&self) -> Result<usize, ContextError> {
        let mut bytes =
            encoded_size(&self.records)?.saturating_add(encoded_size(&self.capabilities)?);
        if !self.instructions.is_empty() {
            bytes = bytes.saturating_add(encoded_size(&self.instructions)?);
        }
        if !self.feedback_notes.is_empty() {
            bytes = bytes.saturating_add(encoded_size(&self.feedback_notes)?);
        }
        if let Some(shared) = self.shared_update() {
            bytes = bytes.saturating_add(encoded_size(shared)?);
        }
        if let Some(checkpoint) = &self.checkpoint {
            bytes = bytes.saturating_add(encoded_size(checkpoint)?);
        }
        Ok(bytes)
    }
}

/// Host context delivered with a completed cell, independently of selected prints.
#[derive(Clone, Debug, Serialize)]
pub struct FeedbackNote {
    /// Index of a `CellCompleted` record in this context's `records` array.
    pub record: usize,
    pub text: String,
}

/// A request projection and the candidate state for its model acceptance.
pub struct PreparedContext {
    pub context: Context,
    /// Candidate installed only after the complete model response commits.
    pub instruction_state: Option<InstructionState>,
}

/// Context preparation must not consume pending instruction delivery gates.
#[async_trait]
pub trait ContextAssembler: Send + Sync {
    /// Prepare context within the explicit encoded byte budget.
    /// The returned [`Context::encoded_size`] must not exceed `bytes`, including
    /// instructions, feedback notes and any shared-data snapshot.
    ///
    /// # Errors
    /// Reject history and descriptions that exceed the budget or cannot be
    /// encoded. Do not silently truncate continuation data to make them fit.
    async fn prepare(
        &self,
        records: Vec<StoredRecord>,
        capabilities: Vec<CapabilityDescription>,
        bytes: usize,
    ) -> Result<PreparedContext, ContextError>;

    /// Prepare a published checkpoint with original suffix, inputs and images.
    /// Instruction-aware assemblers must remap feedback and reattach applicable
    /// rules/skills without consuming delivery gates during preparation.
    ///
    /// # Errors
    /// Reject unsupported projection, invalid boundaries or an exceeded budget.
    async fn prepare_compacted(
        &self,
        _records: Vec<StoredRecord>,
        _capabilities: Vec<CapabilityDescription>,
        _bytes: usize,
        _checkpoint: &Checkpoint,
    ) -> Result<PreparedContext, ContextError> {
        Err(ContextError::Unavailable {
            message: "context assembler does not support checkpoint projection".into(),
        })
    }

    /// Validate and atomically install accepted state without executing work.
    /// Installation failure after model acceptance stops the engine; it does not
    /// roll back the retained response or authorize another provider request.
    ///
    /// # Errors
    /// Reject incompatible or invalid state without changing installed state.
    async fn restore(&self, state: &InstructionState) -> Result<(), ContextError>;
}

/// Keeps all required history and fails instead of silently compacting it.
pub struct BoundedContext;

#[async_trait]
impl ContextAssembler for BoundedContext {
    async fn prepare(
        &self,
        records: Vec<StoredRecord>,
        capabilities: Vec<CapabilityDescription>,
        bytes: usize,
    ) -> Result<PreparedContext, ContextError> {
        let context = Context {
            records: records.into_iter().map(|stored| stored.record).collect(),
            capabilities,
            checkpoint: None,
            instructions: String::new(),
            feedback_notes: Vec::new(),
            shared: None,
        };
        if context.encoded_size()? > bytes {
            return Err(ContextError::Limit);
        }
        Ok(PreparedContext {
            context,
            instruction_state: None,
        })
    }

    async fn prepare_compacted(
        &self,
        records: Vec<StoredRecord>,
        capabilities: Vec<CapabilityDescription>,
        bytes: usize,
        checkpoint: &Checkpoint,
    ) -> Result<PreparedContext, ContextError> {
        let indexes = checkpoint.retained_indexes(&records)?;
        let content = checkpoint.projected_content(&records)?;
        let mut records = records.into_iter().enumerate();
        let mut retained = Vec::with_capacity(indexes.len());
        for index in indexes {
            if let Some((_, record)) = records.find(|(original, _)| *original == index) {
                retained.push(record);
            }
        }
        let remaining = bytes
            .checked_sub(encoded_size(&content)?)
            .ok_or(ContextError::Limit)?;
        let mut prepared = self.prepare(retained, capabilities, remaining).await?;
        prepared.context.checkpoint = Some(content);
        Ok(prepared)
    }

    async fn restore(&self, _state: &InstructionState) -> Result<(), ContextError> {
        Err(ContextError::Unavailable {
            message: "bounded context does not accept instruction state".into(),
        })
    }
}

/// Context construction cannot claim success after losing required history.
#[derive(Debug, thiserror::Error)]
pub enum ContextError {
    #[error("required context exceeds byte limit")]
    Limit,
    #[error("context assembly failed: {message}")]
    Unavailable { message: String },
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Checkpoint(#[from] CheckpointError),
}
