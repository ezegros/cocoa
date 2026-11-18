//! Source-backed checkpoints for an unchanged canonical history and live executor.
//!
//! [`CompactionPolicy`] enables automatic foreground generation at settled engine
//! boundaries. [`SummaryAttempt`] supplies bounded portable source material to a
//! read-only provider operation. The host validates proposals and publishes full
//! checkpoints before projecting them. Publication does not reset or replay cells.
//! Encoded-request byte budgets do not establish a provider's token-window fit.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

mod automatic;
pub use automatic::{CompactionPolicy, SUMMARY_INSTRUCTIONS, SummaryAttempt};

use crate::{
    capability::{Effects, OperationOutcome},
    model::ModelAction,
    store::{
        ActiveSnapshot, RecordId, RecordKind, StoreError, StoreSnapshot, StoredRecord, encoded_size,
    },
};

/// Explicit checkpoint bounds, independent of history and provider request limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointLimits {
    pub bytes: u64,
    pub required_entries: u64,
    /// Maximum length of each other collection, including item source lists.
    pub items: u64,
}

/// Host-issued identity within one agent's checkpoint lineage, never a source ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RequiredEntryId(u64);

/// An exact original excerpt. Repeated excerpts expand to the whole message.
/// Supplying the whole message explicitly also preserves ambiguous scope.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quotation {
    pub source: RecordId,
    pub text: String,
}

/// User direction and user adoption retain their distinct original authors.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RequiredOrigin {
    User {
        quotations: Vec<Quotation>,
    },
    Adopted {
        wording: Vec<Quotation>,
        acceptance: Vec<Quotation>,
    },
}

/// Work is pending unless the user explicitly paused it. Plan deferral is separate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RequestState {
    Pending,
    Paused {
        instruction: RequiredOrigin,
        resume_when: String,
    },
}

/// Proposed mandatory material. `request: None` denotes an ongoing requirement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequiredAddition {
    pub origin: RequiredOrigin,
    pub scope: String,
    #[serde(deserialize_with = "required_option")]
    pub request: Option<RequestState>,
}

/// A carried entry. The host assigns IDs and never recycles retired IDs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequiredEntry {
    pub id: RequiredEntryId,
    pub content: RequiredAddition,
}

/// An original-evidence-backed claim, not mechanically established truth.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceNote {
    pub text: String,
    pub sources: Vec<RecordId>,
}

/// Representation maintenance cannot change request control state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequiredCorrection {
    pub origin: RequiredOrigin,
    pub scope: String,
    pub justification: EvidenceNote,
}

/// A separate explanation is required for a correction and a work transition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RequiredTransition {
    Pause {
        instruction: RequiredOrigin,
        resume_when: String,
    },
    Resume {
        instruction: RequiredOrigin,
    },
    Complete {
        outcome: EvidenceNote,
        verification: EvidenceNote,
    },
    Withdraw {
        instruction: RequiredOrigin,
    },
}

/// An atomic set permits at most one change per existing entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequiredChange {
    pub entry: RequiredEntryId,
    #[serde(deserialize_with = "required_option")]
    pub correction: Option<RequiredCorrection>,
    #[serde(deserialize_with = "required_option")]
    pub transition: Option<RequiredTransition>,
}

/// A recap's status does not confer user authority or execution permission.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecapStatus {
    Proposed,
    Accepted,
    Active,
    Deferred,
    Completed,
    Verified,
    Failed,
    Unknown,
}

/// Plan keys are local to a checkpoint, not persistent work identities.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStage {
    pub key: String,
    pub text: String,
    pub status: RecapStatus,
    pub prerequisites: Vec<String>,
    pub sources: Vec<RecordId>,
    #[serde(deserialize_with = "required_option")]
    pub revisit_when: Option<String>,
}

/// Preserve the goal and deferred branches independently of the next action.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recap {
    pub goal: String,
    pub stages: Vec<PlanStage>,
    #[serde(deserialize_with = "required_option")]
    pub current_stage: Option<String>,
    pub decisions: Vec<PlanStage>,
    pub progress: Vec<EvidenceNote>,
    pub failures: Vec<EvidenceNote>,
    pub next_actions: Vec<String>,
}

/// Unvalidated proposal. Coverage, lineage and durable IDs belong to the host.
/// Decode model output through [`SummaryAttempt`] to resolve temporary sources.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointProposal {
    pub additions: Vec<RequiredAddition>,
    pub changes: Vec<RequiredChange>,
    pub recap: Recap,
    pub evidence: Vec<RecordId>,
}

/// Canonical effect evidence that must not become permission to repeat work.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnresolvedEffect {
    pub start: RecordId,
    #[serde(deserialize_with = "required_option")]
    pub outcome: Option<RecordId>,
    pub effects: Effects,
}

/// The portable, always-visible checkpoint. Original sources remain in the store.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointContent {
    pub through: RecordId,
    #[serde(deserialize_with = "required_option")]
    pub predecessor: Option<RecordId>,
    pub required: Vec<RequiredEntry>,
    pub recap: Recap,
    pub evidence: Vec<RecordId>,
    pub unresolved_effects: Vec<UnresolvedEffect>,
}

/// Validated complete publication, including its source-backed transition audit.
/// Construction resolves against one agent's coherent store snapshot. Validation
/// establishes structural fidelity, not semantic completeness of extraction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Checkpoint {
    content: CheckpointContent,
    after: RecordId,
    changes: Vec<RequiredChange>,
    entry_last: u64,
    limits: CheckpointLimits,
}

/// Agent-scoped control evidence, not a model response or synthetic invocation.
/// `checkpoint.after` anchors publication to the original-history acceptance
/// boundary. Vector order orders publications made at the same boundary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StoredCheckpoint {
    pub id: RecordId,
    pub checkpoint: Checkpoint,
}

impl Checkpoint {
    /// Construct a complete checkpoint from originals and the previous publication.
    /// Required entries omitted by the proposal carry forward unchanged.
    ///
    /// # Errors
    /// Reject invalid boundaries, sources, quotations, transitions, plan graphs,
    /// bounds or lineage. The snapshot and previous projection remain unchanged.
    pub fn prepare(
        snapshot: &StoreSnapshot,
        through: RecordId,
        proposal: CheckpointProposal,
        limits: CheckpointLimits,
    ) -> Result<Self, CheckpointError> {
        snapshot.validate_checkpoints()?;
        Self::prepare_validated(snapshot, through, proposal, limits)
    }

    /// Advance a validated active view using its complete new suffix and selected
    /// originals. Carried entries retain their IDs and omitted entries carry forward.
    /// Unlike [`Self::restore`], this does not restore or validate historical lineage.
    /// The active view already carries the store's latest validated publication.
    ///
    /// # Errors
    /// Reject nonadvancing or incomplete boundaries and the same proposal, quotation,
    /// transition, plan and limit errors as [`Self::prepare`]. Older sources absent
    /// from the active view cannot be cited. Publication must still compare the
    /// predecessor and original tail atomically against the store.
    pub fn prepare_active(
        snapshot: &ActiveSnapshot,
        through: RecordId,
        proposal: CheckpointProposal,
        limits: CheckpointLimits,
    ) -> Result<Self, CheckpointError> {
        boundary(snapshot.suffix(), through)?;
        let records = snapshot.records();
        let cut = records
            .iter()
            .position(|record| record.id == through)
            .ok_or(CheckpointError::Boundary)?;
        let effects = match snapshot.checkpoint() {
            Some(previous) => previous.checkpoint.effects_after(&records[..=cut])?,
            None => unresolved(&records[..=cut]),
        };
        Self::prepare_sources(
            records,
            snapshot.checkpoint(),
            through,
            proposal,
            limits,
            effects,
        )
    }

    /// Restore the host JSON serialization and revalidate the complete publication.
    /// The snapshot must contain exactly the preceding checkpoints in order, not
    /// this publication. Its originals may extend beyond the recorded publication
    /// boundary. Callers must bound the encoded byte length before decoding.
    ///
    /// # Errors
    /// Reject malformed or incomplete JSON, unknown or duplicate fields, invalid
    /// bounds, sources, quotations, transitions, plan graphs, boundaries or lineage.
    /// The snapshot remains unchanged.
    pub fn restore(snapshot: &StoreSnapshot, encoded: &str) -> Result<Self, CheckpointError> {
        let publication: CheckpointEncoding =
            serde_json::from_str(encoded).map_err(|_| CheckpointError::Encoding)?;
        let checkpoint = Self {
            content: publication.content,
            after: publication.after,
            changes: publication.changes,
            entry_last: publication.entry_last,
            limits: publication.limits,
        };
        // Serde also accepts positional struct arrays and object-form unit variants.
        let representation: serde_json::Value =
            serde_json::from_str(encoded).map_err(|_| CheckpointError::Encoding)?;
        if serde_json::to_value(&checkpoint).map_err(|_| StoreError::Encoding)? != representation {
            return Err(CheckpointError::Encoding);
        }
        snapshot.validate_checkpoints()?;
        checkpoint.validate_publication(&snapshot.records, snapshot.checkpoints.last())?;
        Ok(checkpoint)
    }

    /// Validated model-visible contents, without the publication audit.
    #[must_use]
    pub fn content(&self) -> &CheckpointContent {
        &self.content
    }

    /// Changes remain canonical audit evidence, outside the always-visible list.
    #[must_use]
    pub fn changes(&self) -> &[RequiredChange] {
        &self.changes
    }

    /// Original-history tail recorded when this publication was prepared.
    /// Historical restoration may retain newer originals. Storage must compare
    /// this boundary with its current tail before accepting a new publication.
    #[must_use]
    pub fn after(&self) -> RecordId {
        self.after
    }

    /// Sorted, unique original IDs needed to carry this checkpoint into an active
    /// read. Includes coverage, required wording and pause instructions, recap and
    /// evidence sources, plus unresolved operation starts and recorded outcomes.
    /// Audit-only retired-entry sources and predecessors are not active inputs.
    /// The suffix automatically includes [`Self::after`] when it follows coverage.
    /// Image/input and instruction-feedback selection is separate and mandatory.
    ///
    /// # Errors
    /// Report an invalid source traversal as an invalid checkpoint.
    pub fn source_ids(&self) -> Result<Vec<RecordId>, StoreError> {
        let mut selected = BTreeSet::from([self.content.through]);
        automatic::visit_previous(
            &mut self.content.required.clone(),
            &mut Some(self.content.recap.clone()),
            &mut self.content.evidence.clone(),
            &mut |source| {
                selected.insert(*source);
                Ok(())
            },
        )
        .map_err(|_| StoreError::InvalidCheckpoint)?;
        for effect in &self.content.unresolved_effects {
            selected.insert(effect.start);
            selected.extend(effect.outcome);
        }
        Ok(selected.into_iter().collect())
    }

    /// Select an unchanged suffix plus original current inputs and captured images.
    /// A suffix beginning within an invocation also keeps that invocation's inputs.
    /// Accept complete history or [`ActiveSnapshot::records`]. A validated checkpoint
    /// already establishes its cut; sparse source records before it need not contain
    /// complete historical cell groups. The caller must supply the complete suffix
    /// and required original inputs/images, as in [`ActiveSnapshot::new`].
    ///
    /// # Errors
    /// Reject a missing coverage boundary.
    pub fn retained_indexes(
        &self,
        records: &[StoredRecord],
    ) -> Result<Vec<usize>, CheckpointError> {
        let cut = records
            .iter()
            .position(|record| record.id == self.content.through)
            .ok_or(CheckpointError::Boundary)?;
        let current = records.last().map(|stored| stored.record.invocation);
        let suffix = records.get(cut + 1).map(|stored| stored.record.invocation);
        Ok(records.iter().enumerate().filter_map(|(index, stored)| {
            let retained_input = matches!(&stored.record.kind, RecordKind::InputAccepted { input }
                if !input.images().is_empty()
                    || Some(stored.record.invocation) == current
                    || Some(stored.record.invocation) == suffix);
            (index > cut || retained_input).then_some(index)
        }).collect())
    }

    fn prepare_validated(
        snapshot: &StoreSnapshot,
        through: RecordId,
        proposal: CheckpointProposal,
        limits: CheckpointLimits,
    ) -> Result<Self, CheckpointError> {
        let cut = boundary(&snapshot.records, through)?;
        let previous = snapshot.checkpoints.last();
        if let Some(previous) = previous {
            let previous_cut = boundary(&snapshot.records, previous.checkpoint.content.through)?;
            if cut <= previous_cut {
                return Err(CheckpointError::NoProgress);
            }
        }
        Self::prepare_sources(
            &snapshot.records,
            previous,
            through,
            proposal,
            limits,
            unresolved(&snapshot.records[..=cut]),
        )
    }

    fn prepare_sources(
        records: &[StoredRecord],
        previous: Option<&StoredCheckpoint>,
        through: RecordId,
        mut proposal: CheckpointProposal,
        limits: CheckpointLimits,
        unresolved_effects: Vec<UnresolvedEffect>,
    ) -> Result<Self, CheckpointError> {
        if limits.bytes == 0 || limits.required_entries == 0 || limits.items == 0 {
            return Err(CheckpointError::Limits);
        }
        let cut = records
            .iter()
            .position(|record| record.id == through)
            .ok_or(CheckpointError::Boundary)?;
        let originals = Originals {
            records: records[..=cut]
                .iter()
                .map(|record| (record.id, record))
                .collect(),
            limits,
        };
        originals.collection(&proposal.additions, limits.required_entries)?;
        originals.collection(&proposal.changes, limits.required_entries)?;
        originals.sources(&proposal.evidence, SourceRequirement::Optional)?;
        originals.recap(&proposal.recap)?;
        let mut required: BTreeMap<_, _> = previous
            .into_iter()
            .flat_map(|stored| &stored.checkpoint.content.required)
            .map(|entry| (entry.id, entry.clone()))
            .collect();
        let mut changed = BTreeSet::new();
        for change in &mut proposal.changes {
            if !changed.insert(change.entry) {
                return Err(CheckpointError::ConflictingChange);
            }
            let entry = required
                .get_mut(&change.entry)
                .ok_or(CheckpointError::Entry)?;
            if change.correction.is_none() && change.transition.is_none() {
                return Err(CheckpointError::Entry);
            }
            if let Some(correction) = &mut change.correction {
                originals.origin(&mut correction.origin)?;
                nonempty(&correction.scope)?;
                originals.note(&correction.justification)?;
                entry.content.origin = correction.origin.clone();
                entry.content.scope = correction.scope.clone();
            }
            if let Some(transition) = &mut change.transition {
                match transition {
                    RequiredTransition::Pause {
                        instruction,
                        resume_when,
                    } => {
                        if entry.content.request != Some(RequestState::Pending) {
                            return Err(CheckpointError::Transition);
                        }
                        originals.origin(instruction)?;
                        nonempty(resume_when)?;
                        entry.content.request = Some(RequestState::Paused {
                            instruction: instruction.clone(),
                            resume_when: resume_when.clone(),
                        });
                    }
                    RequiredTransition::Resume { instruction } => {
                        if !matches!(entry.content.request, Some(RequestState::Paused { .. })) {
                            return Err(CheckpointError::Transition);
                        }
                        originals.origin(instruction)?;
                        entry.content.request = Some(RequestState::Pending);
                    }
                    RequiredTransition::Complete {
                        outcome,
                        verification,
                    } => {
                        if entry.content.request != Some(RequestState::Pending) {
                            return Err(CheckpointError::Transition);
                        }
                        originals.note(outcome)?;
                        originals.note(verification)?;
                        required.remove(&change.entry);
                    }
                    RequiredTransition::Withdraw { instruction } => {
                        originals.origin(instruction)?;
                        required.remove(&change.entry);
                    }
                }
            }
        }
        let mut entry_last = previous.map_or(0, |stored| stored.checkpoint.entry_last);
        for mut addition in proposal.additions {
            originals.addition(&mut addition)?;
            entry_last = entry_last.checked_add(1).ok_or(CheckpointError::Limits)?;
            let id = RequiredEntryId(entry_last);
            required.insert(
                id,
                RequiredEntry {
                    id,
                    content: addition,
                },
            );
        }
        if required.len()
            > usize::try_from(limits.required_entries).map_err(|_| CheckpointError::Limits)?
        {
            return Err(CheckpointError::Limits);
        }
        let after = records.last().ok_or(CheckpointError::Boundary)?.id;
        let checkpoint = Self {
            content: CheckpointContent {
                through,
                predecessor: previous.map(|stored| stored.id),
                required: required.into_values().collect(),
                recap: proposal.recap,
                evidence: proposal.evidence,
                unresolved_effects,
            },
            after,
            changes: proposal.changes,
            entry_last,
            limits,
        };
        if checkpoint.content.unresolved_effects.len()
            > usize::try_from(limits.items).map_err(|_| CheckpointError::Limits)?
            || encoded_size(&checkpoint)?
                > usize::try_from(limits.bytes).map_err(|_| CheckpointError::Limits)?
        {
            return Err(CheckpointError::Limits);
        }
        Ok(checkpoint)
    }

    pub(crate) fn projected_content(
        &self,
        records: &[StoredRecord],
    ) -> Result<CheckpointContent, CheckpointError> {
        let mut content = self.content.clone();
        content.unresolved_effects = self.effects_after(records)?;
        if content.unresolved_effects.len()
            > usize::try_from(self.limits.items).map_err(|_| CheckpointError::Limits)?
        {
            return Err(CheckpointError::Limits);
        }
        Ok(content)
    }

    pub(crate) fn validate_active_sources(
        &self,
        records: &[StoredRecord],
    ) -> Result<(), CheckpointError> {
        let originals = Originals {
            records: records.iter().map(|record| (record.id, record)).collect(),
            limits: self.limits,
        };
        for entry in &self.content.required {
            let mut content = entry.content.clone();
            originals.addition(&mut content)?;
            if content != entry.content {
                return Err(CheckpointError::Quotation);
            }
        }
        originals.recap(&self.content.recap)?;
        originals.sources(&self.content.evidence, SourceRequirement::Optional)?;
        if self.effects_after(records)? != self.content.unresolved_effects {
            return Err(CheckpointError::Source);
        }
        Ok(())
    }

    fn effects_after(
        &self,
        records: &[StoredRecord],
    ) -> Result<Vec<UnresolvedEffect>, CheckpointError> {
        let cut = records
            .iter()
            .position(|record| record.id == self.content.through)
            .ok_or(CheckpointError::Boundary)?;
        let mut selected = BTreeSet::new();
        for effect in &self.content.unresolved_effects {
            selected.insert(effect.start);
            selected.extend(effect.outcome);
        }
        let effects = unresolved_records(
            records[..=cut]
                .iter()
                .filter(|record| selected.contains(&record.id))
                .chain(&records[cut + 1..]),
        );
        Ok(effects)
    }

    pub(crate) fn validate_publication(
        &self,
        records: &[StoredRecord],
        previous: Option<&StoredCheckpoint>,
    ) -> Result<(), CheckpointError> {
        let after = records
            .iter()
            .position(|stored| stored.id == self.after)
            .ok_or(CheckpointError::Lineage)?;
        let cut = boundary(records, self.content.through)?;
        if cut > after
            || self.content.predecessor != previous.map(|stored| stored.id)
            || previous.is_some_and(|stored| {
                !records[..=cut]
                    .iter()
                    .any(|record| record.id == stored.checkpoint.content.through)
                    || self.content.through == stored.checkpoint.content.through
                    || !records[..=after]
                        .iter()
                        .any(|record| record.id == stored.checkpoint.after)
            })
        {
            return Err(CheckpointError::Lineage);
        }
        let entry_previous = previous.map_or(0, |stored| stored.checkpoint.entry_last);
        let proposal = CheckpointProposal {
            additions: self
                .content
                .required
                .iter()
                .filter(|entry| entry.id.0 > entry_previous)
                .map(|entry| entry.content.clone())
                .collect(),
            changes: self.changes.clone(),
            recap: self.content.recap.clone(),
            evidence: self.content.evidence.clone(),
        };
        let snapshot = StoreSnapshot {
            records: records[..=after].to_vec(),
            instruction_state: None,
            checkpoints: previous.into_iter().cloned().collect(),
        };
        let rebuilt =
            Self::prepare_validated(&snapshot, self.content.through, proposal, self.limits)?;
        if &rebuilt != self {
            return Err(CheckpointError::Lineage);
        }
        Ok(())
    }
}

impl ActiveSnapshot {
    /// Current unresolved external effects, carrying checkpoint evidence through
    /// the complete suffix. Unrelated sparse evidence cannot create false starts.
    /// An outcome marked unknown or partial never grants permission to repeat work.
    ///
    /// # Errors
    /// Reject a missing coverage boundary. Projection/publication limits do not
    /// truncate this evidence or prevent settlement of a larger new suffix.
    pub fn unresolved_effects(&self) -> Result<Vec<UnresolvedEffect>, CheckpointError> {
        match self.checkpoint() {
            Some(stored) => stored.checkpoint.effects_after(self.records()),
            None => Ok(unresolved(self.records())),
        }
    }
}

impl StoreSnapshot {
    /// Validate control identity, ordering and lineage against canonical originals.
    /// No invalid publication is skipped in favor of an earlier checkpoint.
    ///
    /// # Errors
    /// Reject missing originals, duplicated identities, invalid cuts or lineage.
    pub fn validate_checkpoints(&self) -> Result<(), CheckpointError> {
        let mut identities: BTreeSet<_> = self.records.iter().map(|record| record.id).collect();
        if identities.len() != self.records.len() {
            return Err(CheckpointError::Lineage);
        }
        let mut previous = None;
        for stored in &self.checkpoints {
            if !identities.insert(stored.id) {
                return Err(CheckpointError::Lineage);
            }
            stored
                .checkpoint
                .validate_publication(&self.records, previous)?;
            previous = Some(stored);
        }
        Ok(())
    }
}

/// Checkpoint failure never authorizes loss of mandatory material or effect replay.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, Serialize)]
pub enum CheckpointError {
    #[error("summary proposal is not complete closed-schema JSON")]
    Proposal,
    #[error("checkpoint publication is not complete closed-schema JSON")]
    Encoding,
    #[error("checkpoint bounds exceeded or invalid")]
    Limits,
    #[error("checkpoint boundary is absent or splits a call/result group")]
    Boundary,
    #[error("checkpoint lineage does not match retained history")]
    Lineage,
    #[error("checkpoint source is absent, outside coverage or has the wrong author")]
    Source,
    #[error("checkpoint quote does not match its original")]
    Quotation,
    #[error("checkpoint text or required source list is empty")]
    Empty,
    #[error("checkpoint refers to an unknown or empty required-entry change")]
    Entry,
    #[error("checkpoint changes an entry more than once")]
    ConflictingChange,
    #[error("checkpoint request transition is invalid")]
    Transition,
    #[error("checkpoint plan has invalid keys, references or cyclic prerequisites")]
    Plan,
    #[error("checkpoint does not advance coverage or reduce model context")]
    NoProgress,
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[derive(Deserialize)]
struct CheckpointEncoding {
    content: CheckpointContent,
    after: RecordId,
    changes: Vec<RequiredChange>,
    entry_last: u64,
    limits: CheckpointLimits,
}

struct Originals<'a> {
    records: BTreeMap<RecordId, &'a StoredRecord>,
    limits: CheckpointLimits,
}

enum SourceRequirement {
    Optional,
    Required,
}

#[derive(Clone, Copy)]
enum QuotationAuthor {
    User,
    Assistant,
}

impl Originals<'_> {
    fn collection<T>(&self, items: &[T], limit: u64) -> Result<(), CheckpointError> {
        if items.len() > usize::try_from(limit).map_err(|_| CheckpointError::Limits)? {
            return Err(CheckpointError::Limits);
        }
        Ok(())
    }

    fn sources(
        &self,
        sources: &[RecordId],
        requirement: SourceRequirement,
    ) -> Result<(), CheckpointError> {
        self.collection(sources, self.limits.items)?;
        if matches!(requirement, SourceRequirement::Required) && sources.is_empty() {
            return Err(CheckpointError::Empty);
        }
        let mut seen = BTreeSet::new();
        for source in sources {
            if !self.records.contains_key(source) || !seen.insert(source) {
                return Err(CheckpointError::Source);
            }
        }
        Ok(())
    }

    fn quotations(
        &self,
        quotations: &mut [Quotation],
        author: QuotationAuthor,
    ) -> Result<(), CheckpointError> {
        self.collection(quotations, self.limits.items)?;
        if quotations.is_empty() {
            return Err(CheckpointError::Empty);
        }
        for quotation in quotations {
            nonempty(&quotation.text)?;
            let record = self
                .records
                .get(&quotation.source)
                .ok_or(CheckpointError::Source)?;
            let text = match (&record.record.kind, author) {
                (RecordKind::InputAccepted { input }, QuotationAuthor::User) => input.text(),
                (RecordKind::ModelCompleted { response, .. }, QuotationAuthor::Assistant) => {
                    let ModelAction::Final { answer } = &response.action else {
                        return Err(CheckpointError::Source);
                    };
                    answer
                }
                _ => return Err(CheckpointError::Source),
            };
            let mut matches = text.match_indices(&quotation.text);
            let (offset, _) = matches.next().ok_or(CheckpointError::Quotation)?;
            // Advance one character, not one whole match, to detect overlapping quotes.
            let next = offset + text[offset..].chars().next().map_or(0, char::len_utf8);
            if text[next..].contains(&quotation.text) {
                quotation.text = text.to_owned();
            }
        }
        Ok(())
    }

    fn origin(&self, origin: &mut RequiredOrigin) -> Result<(), CheckpointError> {
        match origin {
            RequiredOrigin::User { quotations } => {
                self.quotations(quotations, QuotationAuthor::User)
            }
            RequiredOrigin::Adopted {
                wording,
                acceptance,
            } => {
                self.quotations(wording, QuotationAuthor::Assistant)?;
                self.quotations(acceptance, QuotationAuthor::User)
            }
        }
    }

    fn addition(&self, addition: &mut RequiredAddition) -> Result<(), CheckpointError> {
        self.origin(&mut addition.origin)?;
        nonempty(&addition.scope)?;
        if let Some(RequestState::Paused {
            instruction,
            resume_when,
        }) = &mut addition.request
        {
            self.origin(instruction)?;
            nonempty(resume_when)?;
        }
        Ok(())
    }

    fn note(&self, note: &EvidenceNote) -> Result<(), CheckpointError> {
        nonempty(&note.text)?;
        self.sources(&note.sources, SourceRequirement::Required)
    }

    fn recap(&self, recap: &Recap) -> Result<(), CheckpointError> {
        nonempty(&recap.goal)?;
        self.collection(&recap.stages, self.limits.items)?;
        self.collection(&recap.decisions, self.limits.items)?;
        self.collection(&recap.progress, self.limits.items)?;
        self.collection(&recap.failures, self.limits.items)?;
        self.collection(&recap.next_actions, self.limits.items)?;
        let mut keys = BTreeSet::new();
        for stage in &recap.stages {
            if !keys.insert(stage.key.as_str()) {
                return Err(CheckpointError::Plan);
            }
        }
        if recap
            .current_stage
            .as_ref()
            .is_some_and(|key| !keys.contains(key.as_str()))
        {
            return Err(CheckpointError::Plan);
        }
        let mut plan_keys = keys.clone();
        for decision in &recap.decisions {
            if !plan_keys.insert(decision.key.as_str()) {
                return Err(CheckpointError::Plan);
            }
        }
        for stage in recap.stages.iter().chain(&recap.decisions) {
            nonempty(&stage.key)?;
            nonempty(&stage.text)?;
            self.collection(&stage.prerequisites, self.limits.items)?;
            let mut references = BTreeSet::new();
            for key in &stage.prerequisites {
                if !keys.contains(key.as_str()) || !references.insert(key) {
                    return Err(CheckpointError::Plan);
                }
            }
            let requirement = match stage.status {
                RecapStatus::Accepted
                | RecapStatus::Completed
                | RecapStatus::Verified
                | RecapStatus::Failed => SourceRequirement::Required,
                _ => SourceRequirement::Optional,
            };
            self.sources(&stage.sources, requirement)?;
            if let Some(condition) = &stage.revisit_when {
                nonempty(condition)?;
            }
        }
        let mut visited = BTreeSet::new();
        for _ in 0..recap.stages.len() {
            for stage in &recap.stages {
                if stage
                    .prerequisites
                    .iter()
                    .all(|key| visited.contains(key.as_str()))
                {
                    visited.insert(stage.key.as_str());
                }
            }
        }
        if visited.len() != recap.stages.len() {
            return Err(CheckpointError::Plan);
        }
        for note in recap.progress.iter().chain(&recap.failures) {
            self.note(note)?;
        }
        for action in &recap.next_actions {
            nonempty(action)?;
        }
        Ok(())
    }
}

fn required_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer)
}

fn nonempty(text: &str) -> Result<(), CheckpointError> {
    if text.trim().is_empty() {
        return Err(CheckpointError::Empty);
    }
    Ok(())
}

fn boundary(records: &[StoredRecord], through: RecordId) -> Result<usize, CheckpointError> {
    let cut = records
        .iter()
        .position(|record| record.id == through)
        .ok_or(CheckpointError::Boundary)?;
    let mut pending = BTreeSet::new();
    for record in &records[..=cut] {
        match &record.record.kind {
            RecordKind::ModelCompleted { response, .. } => {
                if !pending.is_empty() {
                    return Err(CheckpointError::Boundary);
                }
                if let ModelAction::Execute { cells } = &response.action {
                    for ordinal in 0..cells.len() {
                        pending.insert((record.id, ordinal));
                    }
                }
            }
            RecordKind::CellCompleted { model, cell, .. }
                if !pending.remove(&(*model, *cell as usize)) =>
            {
                return Err(CheckpointError::Boundary);
            }
            _ => {}
        }
    }
    if !pending.is_empty() {
        return Err(CheckpointError::Boundary);
    }
    Ok(cut)
}

fn unresolved(records: &[StoredRecord]) -> Vec<UnresolvedEffect> {
    unresolved_records(records.iter())
}

fn unresolved_records<'a>(
    records: impl Iterator<Item = &'a StoredRecord>,
) -> Vec<UnresolvedEffect> {
    let mut operations = BTreeMap::new();
    for stored in records {
        let invocation = stored.record.invocation.0;
        match &stored.record.kind {
            RecordKind::OperationStarted { operation, .. } => {
                operations.insert(
                    (invocation, *operation),
                    UnresolvedEffect {
                        start: stored.id,
                        outcome: None,
                        effects: Effects::Unknown,
                    },
                );
            }
            RecordKind::OperationFinished { operation, outcome } => {
                let effects = match outcome {
                    OperationOutcome::Unknown => Effects::Unknown,
                    OperationOutcome::Failed { error } => error.effects(),
                    OperationOutcome::FailedOmitted { effects, .. } => *effects,
                    _ => Effects::None,
                };
                if effects == Effects::None {
                    operations.remove(&(invocation, *operation));
                } else if let Some(entry) = operations.get_mut(&(invocation, *operation)) {
                    entry.outcome = Some(stored.id);
                    entry.effects = effects;
                }
            }
            _ => {}
        }
    }
    operations.into_values().collect()
}
