//! Bounded, sanitized exports from one coherent durable read snapshot.

use cocoa_core::{
    accounting::{
        AccountingSummary, RequestOutcome, RequestPurpose, RequestSettings, RequestUpdate,
    },
    compaction::StoredCheckpoint,
    store::{InvocationId, RecordKind, StoredRecord},
};
use serde::{Deserialize, Serialize};
use sqlx::{Connection, FromRow};

use super::*;

/// Whole-export bounds. Exceeding either returns an error, never partial evidence.
/// The byte bound applies both to decoded source payloads and compact exported JSON.
#[derive(Clone, Copy, Debug)]
pub struct ExportLimits {
    records: u32,
    bytes: usize,
}

impl ExportLimits {
    /// Bound all retained rows, including requests, records and checkpoints.
    ///
    /// # Errors
    /// Reject zero limits, more than 65,536 rows or more than 128 MiB.
    pub fn new(records: u32, bytes: u64) -> Result<Self, PersistenceError> {
        if !(1..=65_536).contains(&records) || !(1..=134_217_728).contains(&bytes) {
            return Err(PersistenceError::InvalidExportLimits);
        }
        Ok(Self {
            records,
            bytes: bytes as usize,
        })
    }
}

impl Default for ExportLimits {
    fn default() -> Self {
        Self {
            records: 10_000,
            bytes: 64 * 1024 * 1024,
        }
    }
}

/// Portable execution evidence and accepted instruction snapshots. Provider
/// continuations are excluded, without modifying retained originals.
#[derive(Clone, Debug, Serialize)]
pub struct ExportHistory {
    pub records: Vec<StoredRecord>,
    pub checkpoints: Vec<StoredCheckpoint>,
    pub instruction_state: Option<InstructionState>,
}

/// Latest durable evidence for one actual transport attempt. A null outcome means
/// unknown, not completed, failed, cancelled or proof that generation stopped.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExportRequest {
    pub id: i64,
    pub agent: AgentId,
    pub invocation: InvocationId,
    pub runtime_epoch: i64,
    pub started_at_ms: i64,
    pub updated_at_ms: i64,
    pub purpose: RequestPurpose,
    pub settings: Option<RequestSettings>,
    #[serde(flatten)]
    pub update: RequestUpdate,
}

/// A versioned whole-session export, not a replay token or workspace snapshot.
/// Credentials, provider-private continuation and private reasoning are excluded.
/// User prompts, final answers, tool outputs, paths and workspace content may still
/// be sensitive. Review before sharing. No monetary prices or spend policy apply.
#[derive(Clone, Debug, Serialize)]
pub struct RunExport {
    pub schema_version: u32,
    pub cocoa_version: &'static str,
    pub public_id: SessionPublicId,
    pub root_agent: AgentId,
    pub metadata: SessionSummary,
    pub settings: StoredSessionSettings,
    pub agents: Vec<RetainedAgent>,
    pub histories: BTreeMap<AgentId, ExportHistory>,
    pub context: Vec<StoredContextRecord>,
    pub receipts: BTreeMap<RecordId, cocoa_core::capability::OperationOutcome>,
    pub requests: Vec<ExportRequest>,
    /// False for upgraded sessions or copied conversation prefixes. Older failed
    /// requests and compaction usage cannot be reconstructed from completed output.
    pub ledger_complete: bool,
    pub accounting: AccountingSummary,
    pub sensitivity: &'static str,
}

impl SessionAttachment {
    /// Export durable evidence without recovery, provider calls or execution.
    /// Read after host cleanup to include settled child outcomes. Reads taken
    /// during execution preserve unmatched starts and absent outcomes as unknown.
    /// The export is a coherent transaction, but not a captured workspace image.
    ///
    /// # Errors
    /// Reject insufficient export limits, stale ownership, uncertain writes,
    /// malformed evidence and backend failures. No truncated export is returned.
    pub async fn export_run(&self, limits: ExportLimits) -> Result<RunExport, PersistenceError> {
        let request = self.inner.database.request()?;
        let payload = self.inner.database.payload(limits.bytes)?;
        let attachment = self.clone();
        tokio::spawn(async move {
            let _request = request;
            let _payload = payload;
            let operation = tokio::time::timeout(WAIT, Arc::clone(&attachment.inner.operation).lock_owned()).await.map_err(|_| PersistenceError::Busy)?;
            if matches!(*operation, AttachmentState::Uncertain { .. }) { return Err(PersistenceError::OutcomeUnknown); }
            let mut reader = tokio::time::timeout(WAIT, attachment.inner.database.reader.lock()).await.map_err(|_| PersistenceError::Busy)?;
            reader.settle().await?;
            reader.attachment = Some(Arc::clone(&attachment.inner.lease));
            let mut transaction = reader.connection()?.begin().await?;
            let id = i64::from(attachment.id());
            let sizes: Vec<i64> = sqlx::query_scalar(
                "SELECT bytes FROM (SELECT coalesce(octet_length(payload_json), archive_decoded_bytes) AS bytes FROM records WHERE session_id = ? \
                 UNION ALL SELECT octet_length(payload_json) FROM checkpoints WHERE session_id = ? \
                 UNION ALL SELECT octet_length(settings_json) + octet_length(update_json) + 128 FROM model_requests WHERE session_id = ? \
                 UNION ALL SELECT coalesce(octet_length(assignment_json), 0) + coalesce(octet_length(outcome_json), 0) + 256 FROM agents WHERE session_id = ? \
                 UNION ALL SELECT octet_length(content_json) FROM context_records WHERE session_id = ? \
                 UNION ALL SELECT octet_length(state_json) FROM instruction_state WHERE agent_id IN (SELECT id FROM agents WHERE session_id = ?) \
                 UNION ALL SELECT octet_length(outcome_json) FROM mutation_receipts WHERE operation_record_id IN (SELECT id FROM records WHERE session_id = ?) \
                 UNION ALL SELECT octet_length(settings_json) + 512 FROM sessions WHERE id = ?) LIMIT ?"
            ).bind(id).bind(id).bind(id).bind(id).bind(id).bind(id).bind(id).bind(id).bind(i64::from(limits.records) + 1).fetch_all(&mut *transaction).await?;
            let mut bytes = 0_u64;
            for size in &sizes {
                bytes = bytes.checked_add(u64::try_from(*size).map_err(|_| PersistenceError::InvalidData("export payload size"))?).ok_or(PersistenceError::ExportLimit)?;
            }
            if sizes.len() > limits.records as usize || bytes > limits.bytes as u64 { return Err(PersistenceError::ExportLimit); }
            let snapshot = sessions::snapshot(&mut transaction, &attachment, StoreLimits { record_bytes: limits.bytes }).await?;
            let ledger_version: i64 = sqlx::query_scalar("SELECT accounting_version FROM sessions WHERE id = ?").bind(id).fetch_one(&mut *transaction).await?;
            let fork: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM conversation_forks WHERE session_id = ?)").bind(id).fetch_one(&mut *transaction).await?;
            let rows: Vec<RequestRow> = sqlx::query_as("SELECT id, agent_id, invocation, runtime_epoch, started_at_ms, updated_at_ms, purpose_json, settings_json, update_json FROM model_requests WHERE session_id = ? ORDER BY id LIMIT ?").bind(id).bind(i64::from(limits.records) + 1).fetch_all(&mut *transaction).await?;
            let mut requests = Vec::with_capacity(rows.len());
            for row in rows { requests.push(row.decode()?); }
            transaction.commit().await?;
            reader.attachment = None;
            let mut settings = snapshot.settings;
            if let StoredSessionSettings::Bound { effective } = &mut settings {
                effective.endpoint = None;
                effective.account = None;
                for profile in effective.aliases.values_mut() { profile.endpoint = None; profile.account = None; }
            }
            let mut legacy_responses = 0_u64;
            let mut histories = BTreeMap::new();
            for (agent, history) in snapshot.histories {
                let completed = history.records.iter().filter(|record| matches!(record.record.kind, RecordKind::ModelCompleted { .. })).count();
                let recorded = requests.iter().filter(|request| request.agent == agent && request.purpose == RequestPurpose::Response && request.update.outcome == Some(RequestOutcome::Completed)).count();
                legacy_responses += completed.saturating_sub(recorded) as u64;
                let mut records = history.records;
                for record in &mut records {
                    if let RecordKind::ModelCompleted { response, .. } = &mut record.record.kind { response.continuation = None; }
                }
                histories.insert(agent, ExportHistory { records, checkpoints: history.checkpoints, instruction_state: history.instruction_state });
            }
            let ledger_complete = ledger_version == 1 && !fork && legacy_responses == 0;
            let mut accounting = AccountingSummary::from_updates(requests.iter().map(|request| &request.update), legacy_responses)?;
            if !ledger_complete { accounting.usage = None; }
            let export = RunExport {
                schema_version: 1,
                cocoa_version: env!("CARGO_PKG_VERSION"),
                public_id: attachment.public_id().clone(),
                root_agent: attachment.root_agent(),
                metadata: snapshot.metadata,
                settings,
                agents: snapshot.agents,
                histories,
                context: snapshot.context,
                receipts: snapshot.receipts,
                requests,
                ledger_complete,
                accounting,
                sensitivity: "Prompts, answers, tool outputs, paths and workspace content may be sensitive. Review before sharing. Credentials and provider-private continuations/reasoning are excluded, not secrets embedded in user or tool content.",
            };
            if encoded_size(&export)? > limits.bytes { return Err(PersistenceError::ExportLimit); }
            Ok(export)
        }).await?
    }
}

#[derive(FromRow)]
struct RequestRow {
    id: i64,
    agent_id: i64,
    invocation: i64,
    runtime_epoch: i64,
    started_at_ms: i64,
    updated_at_ms: i64,
    purpose_json: String,
    settings_json: String,
    update_json: String,
}

impl RequestRow {
    fn decode(self) -> Result<ExportRequest, PersistenceError> {
        if self.id <= 0
            || self.invocation <= 0
            || self.runtime_epoch <= 0
            || self.started_at_ms < 0
            || self.updated_at_ms < self.started_at_ms
        {
            return Err(PersistenceError::InvalidData("request accounting metadata"));
        }
        Ok(ExportRequest {
            id: self.id,
            agent: AgentId::try_from(self.agent_id)
                .map_err(|_| PersistenceError::InvalidData("request accounting agent"))?,
            invocation: InvocationId(self.invocation as u64),
            runtime_epoch: self.runtime_epoch,
            started_at_ms: self.started_at_ms,
            updated_at_ms: self.updated_at_ms,
            purpose: serde_json::from_str(&self.purpose_json)
                .map_err(|_| PersistenceError::InvalidData("request purpose"))?,
            settings: serde_json::from_str(&self.settings_json)
                .map_err(|_| PersistenceError::InvalidData("request settings"))?,
            update: serde_json::from_str(&self.update_json)
                .map_err(|_| PersistenceError::InvalidData("request counters"))?,
        })
    }
}
