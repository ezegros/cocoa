//! Reconcile retained evidence without retrying cells, providers or children.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use cocoa_core::{
    capability::{Effects, OperationOutcome},
    execution::CellResponse,
    model::ModelAction,
    store::{
        AgentId, InvocationId, InvocationOutcome, Record, RecordId, RecordKind, StoreError,
        StoreLimits, StoreSnapshot, encoded_size,
    },
};
use sqlx::{Connection, SqliteConnection};

use super::{
    AgentMembership, AgentOutcome, AppendCompletion, AttachedSession, AttachmentState,
    FORMAT_VERSION, PersistenceError, RECORD_VERSION, SessionAttachment, SessionSnapshot, WAIT,
    sessions, writes,
};

impl SessionAttachment {
    /// Append missing known local results, paired cell feedback and interrupted
    /// terminals atomically, then advance the runtime epoch. Unmatched external
    /// operation starts remain unknown. No child or external operation restarts.
    /// Calling this on its returned attachment is a no-op.
    /// Admitted recovery outlives a dropped future. If its result is lost, discard
    /// the old attachment and its stores, then reattach to inspect the saved epoch.
    ///
    /// # Errors
    /// Reject malformed evidence, exhausted reconciliation capacity, stale epochs
    /// and storage failures. Unknown commits require inspecting durable state.
    pub async fn recover(&self, limits: StoreLimits) -> Result<Self, PersistenceError> {
        if self.inner.recovered {
            return Ok(self.clone());
        }
        super::validate_limits(limits)?;
        let request = self.inner.database.request()?;
        let payload = self.inner.database.payload(0)?;
        let mut guard = tokio::time::timeout(WAIT, Arc::clone(&self.inner.operation).lock_owned())
            .await
            .map_err(|_| PersistenceError::Busy)?;
        if matches!(*guard, AttachmentState::Uncertain { .. }) {
            return Err(PersistenceError::OutcomeUnknown);
        }
        *guard = AttachmentState::Uncertain {
            agent: self.root_agent(),
        };
        let attachment = self.clone();
        let mut completion = tokio::spawn(async move {
            let result = recover(&attachment, limits).await;
            AppendCompletion {
                known: !matches!(result, Err(PersistenceError::OutcomeUnknown)),
                result,
                operation: guard,
                _attachment: Arc::clone(&attachment.inner),
                _request: request,
                _payload: payload,
            }
        })
        .await?;
        if completion.known {
            *completion.operation = AttachmentState::Ready;
        }
        let epoch = completion.result?;
        let inner = Arc::new(AttachedSession {
            database: Arc::clone(&self.inner.database),
            session_id: self.id(),
            public_id: self.public_id().clone(),
            root_agent: self.root_agent(),
            epoch,
            recovered: true,
            lease: Arc::clone(&self.inner.lease),
            operation: Arc::clone(&self.inner.operation),
        });
        self.inner
            .database
            .attachments
            .lock()
            .await
            .insert(self.id(), Arc::downgrade(&inner));
        Ok(Self { inner })
    }
}

pub(super) async fn receipts(
    connection: &mut SqliteConnection,
    attachment: &SessionAttachment,
) -> Result<BTreeMap<RecordId, OperationOutcome>, PersistenceError> {
    let mut receipts = BTreeMap::new();
    let mut cursor = 0_i64;
    let mut bytes = 0_usize;
    loop {
        let rows:Vec<(i64,i64,Option<String>)>=sqlx::query_as("SELECT m.operation_record_id,m.outcome_version,CASE WHEN octet_length(m.outcome_json)<=1048576 THEN m.outcome_json END FROM records r JOIN mutation_receipts m ON m.operation_record_id=r.id WHERE r.session_id=? AND r.id>? ORDER BY r.id LIMIT 4")
            .bind(i64::from(attachment.id())).bind(cursor).fetch_all(&mut *connection).await?;
        if rows.is_empty() {
            break;
        }
        for (start, version, text) in rows {
            cursor = start;
            if version != FORMAT_VERSION {
                return Err(PersistenceError::UnsupportedPayload(version));
            }
            let text = text.ok_or(StoreError::RecordLimit)?;
            bytes = bytes.checked_add(text.len()).ok_or(StoreError::Capacity)?;
            if bytes > 64 * 1024 * 1024 || receipts.len() >= 33 * 1024 {
                return Err(StoreError::Capacity.into());
            }
            let start = RecordId::try_from(start)
                .map_err(|_| PersistenceError::InvalidData("receipt identity"))?;
            let outcome = serde_json::from_str(&text)
                .map_err(|_| PersistenceError::InvalidData("receipt JSON"))?;
            receipts.insert(start, outcome);
        }
    }
    Ok(receipts)
}

async fn recover(
    attachment: &SessionAttachment,
    limits: StoreLimits,
) -> Result<i64, PersistenceError> {
    let (snapshot, record_top) = {
        let mut reader = tokio::time::timeout(WAIT, attachment.inner.database.reader.lock())
            .await
            .map_err(|_| PersistenceError::Busy)?;
        reader.settle().await?;
        let mut transaction = reader.connection()?.begin().await?;
        let snapshot = sessions::snapshot(&mut transaction, attachment, limits).await?;
        let record_top: i64 = sqlx::query_scalar(
            "SELECT max(coalesce((SELECT max(id) FROM records), 0), coalesce((SELECT max(id) FROM checkpoints), 0))",
        )
        .fetch_one(&mut *transaction)
        .await?;
        transaction.commit().await?;
        (snapshot, record_top)
    };
    let plan =
        tokio::task::spawn_blocking(move || plan_recovery(snapshot, limits, record_top)).await??;
    let epoch = attachment
        .inner
        .epoch
        .checked_add(1)
        .ok_or(StoreError::IdentityExhausted)?;
    let mut writer = tokio::time::timeout(WAIT, attachment.inner.database.writer.lock())
        .await
        .map_err(|_| PersistenceError::Busy)?;
    writer.settle().await?;
    writer.attachment = Some(Arc::clone(&attachment.inner.lease));
    let result=async {
        let mut transaction=writer.connection()?.begin_with("BEGIN IMMEDIATE").await?;
        let result=async {
            sessions::check_epoch(&mut transaction,attachment).await?;
            let cutoffs:Vec<HistoryCutoff>=sqlx::query_as("SELECT agent_id AS agent,count(*) AS count,max(id) AS record,max(sequence) AS sequence FROM records WHERE session_id=? GROUP BY agent_id ORDER BY agent_id")
                .bind(i64::from(attachment.id())).fetch_all(&mut *transaction).await?;
            if cutoffs!=plan.cutoffs {return Err(PersistenceError::InvalidData("history changed during recovery preflight"));}
            let mut record_top:i64=sqlx::query_scalar("SELECT max(coalesce((SELECT max(id) FROM records),0),coalesce((SELECT max(id) FROM checkpoints),0))").fetch_one(&mut *transaction).await?;
            record_top.checked_add(i64::try_from(plan.records.len()).map_err(|_|StoreError::IdentityExhausted)?).ok_or(StoreError::IdentityExhausted)?;
            let mut cursor=0;
            let now=writes::timestamp()?;
            while cursor<plan.records.len() {
                let mut end=cursor;
                let mut bytes=0;
                while end<plan.records.len() && end-cursor<64 {
                    let size=plan.records[end].payload.len();
                    if end>cursor && bytes+size>4*1024*1024 {break;}
                    bytes+=size;end+=1;
                }
                let mut insert=sqlx::QueryBuilder::<sqlx::Sqlite>::new("INSERT INTO records (id,session_id,agent_id,sequence,invocation,runtime_epoch,created_at_ms,payload_version,payload_json) ");
                insert.push_values(&plan.records[cursor..end],|mut row,record| {
                    record_top+=1;
                    row.push_bind(record_top).push_bind(i64::from(attachment.id())).push_bind(record.agent).push_bind(record.sequence).push_bind(record.invocation)
                        .push_bind(attachment.inner.epoch).push_bind(now).push_bind(RECORD_VERSION).push_bind(&record.payload);
                });
                if insert.build().execute(&mut *transaction).await?.rows_affected()!=u64::try_from(end-cursor).map_err(|_|StoreError::Capacity)? {
                    return Err(PersistenceError::InvalidData("recovery repair insertion"));
                }
                cursor=end;
            }
            for agent in &plan.abandoned {
                if sqlx::query("UPDATE agents SET outcome_json=? WHERE session_id=? AND id=? AND outcome_json IS NULL")
                    .bind(&plan.abandoned_outcome).bind(i64::from(attachment.id())).bind(i64::from(*agent)).execute(&mut *transaction).await?.rows_affected()!=1 {
                    return Err(PersistenceError::InvalidData("recovery participant changed"));
                }
            }
            if sqlx::query("UPDATE sessions SET runtime_epoch=?,last_opened_at_ms=max(last_opened_at_ms,?) WHERE id=? AND runtime_epoch=?")
                .bind(epoch).bind(now).bind(i64::from(attachment.id())).bind(attachment.inner.epoch).execute(&mut *transaction).await?.rows_affected()!=1 {
                return Err(PersistenceError::AgentUnavailable);
            }
            Ok::<_,PersistenceError>(epoch)
        }.await;
        match result {
            Ok(epoch)=>transaction.commit().await.map(|()|epoch).map_err(|error|{tracing::error!(%error,"SQLite recovery commit outcome unknown");PersistenceError::OutcomeUnknown}),
            Err(error)=>match transaction.rollback().await {Ok(())=>Err(error),Err(error)=>{tracing::error!(%error,"SQLite recovery rollback outcome unknown");Err(PersistenceError::OutcomeUnknown)}}
        }
    }.await;
    if let Err(error) = writer.settle().await {
        tracing::error!(%error,"SQLite recovery did not settle");
        return Err(PersistenceError::OutcomeUnknown);
    }
    result
}

struct RecoveryPlan {
    cutoffs: Vec<HistoryCutoff>,
    records: Vec<RepairRecord>,
    abandoned: Vec<AgentId>,
    abandoned_outcome: String,
}

#[derive(Debug, PartialEq, Eq, sqlx::FromRow)]
struct HistoryCutoff {
    agent: i64,
    count: i64,
    record: i64,
    sequence: i64,
}

struct RepairRecord {
    agent: i64,
    sequence: i64,
    invocation: i64,
    payload: String,
}

fn plan_recovery(
    mut snapshot: SessionSnapshot,
    limits: StoreLimits,
    mut record_top: i64,
) -> Result<RecoveryPlan, PersistenceError> {
    let repairs = repairs(&snapshot, &snapshot.receipts)?;
    let mut cutoffs = Vec::new();
    for (agent, history) in &snapshot.histories {
        if let Some(last) = history.records.last() {
            let maximum = history
                .records
                .iter()
                .map(|stored| i64::from(stored.id))
                .max()
                .ok_or(PersistenceError::InvalidData("history cutoff"))?;
            cutoffs.push(HistoryCutoff {
                agent: i64::from(*agent),
                count: i64::try_from(history.records.len()).map_err(|_| StoreError::Capacity)?,
                record: maximum,
                sequence: i64::from(last.sequence),
            });
        }
    }
    let mut encoded = Vec::new();
    for (agent, records) in repairs {
        let history = snapshot
            .histories
            .get_mut(&agent)
            .ok_or(PersistenceError::InvalidData("repair participant"))?;
        for record in records {
            if encoded_size(&record)? > limits.record_bytes {
                return Err(StoreError::RecordLimit.into());
            }
            record_top = record_top
                .checked_add(1)
                .ok_or(StoreError::IdentityExhausted)?;
            let id = RecordId::try_from(record_top)
                .map_err(|_| PersistenceError::InvalidData("repair record ID"))?;
            let sequence = cocoa_core::store::RecordSequence::try_from(
                i64::try_from(history.records.len()).map_err(|_| StoreError::IdentityExhausted)?
                    + 1,
            )
            .map_err(|_| StoreError::IdentityExhausted)?;
            let payload = serde_json::to_string(&record.kind).map_err(|_| StoreError::Encoding)?;
            encoded.push(RepairRecord {
                agent: i64::from(agent),
                sequence: i64::from(sequence),
                invocation: i64::try_from(record.invocation.0)
                    .map_err(|_| StoreError::InvalidRecord)?,
                payload,
            });
            history.records.push(cocoa_core::store::StoredRecord {
                id,
                sequence,
                record,
            });
        }
        cocoa_core::engine::validate_resume_snapshot(history)?;
    }
    let abandoned: Vec<_> = snapshot
        .agents
        .iter()
        .filter(|agent| {
            matches!(agent.membership, AgentMembership::Child { .. }) && agent.outcome.is_none()
        })
        .map(|agent| agent.id)
        .collect();
    let abandoned_outcome = serde_json::to_string(&AgentOutcome::Failed {
        code: "host_restarted".into(),
        message:
            "Host restarted before retaining a terminal child result; external effects may remain"
                .into(),
        effects: Effects::Unknown,
    })
    .map_err(|_| StoreError::Encoding)?;
    Ok(RecoveryPlan {
        cutoffs,
        records: encoded,
        abandoned,
        abandoned_outcome,
    })
}

fn history_repairs(
    history: &StoreSnapshot,
    receipts: &BTreeMap<RecordId, OperationOutcome>,
) -> Result<Vec<Record>, PersistenceError> {
    let mut invocations = BTreeSet::new();
    let mut terminal = BTreeSet::new();
    let mut models = BTreeMap::new();
    let mut started = BTreeSet::new();
    let mut completed = BTreeSet::new();
    let mut operations = BTreeMap::new();
    let mut finished = BTreeMap::new();
    for stored in &history.records {
        let invocation = stored.record.invocation.0;
        invocations.insert(invocation);
        match &stored.record.kind {
            RecordKind::ModelCompleted { response, .. } => {
                if let ModelAction::Execute { cells } = &response.action {
                    models.insert(stored.id, (invocation, cells));
                }
            }
            RecordKind::CellStarted { model, cell } => {
                validate_cell(&models, *model, *cell, invocation, None)?;
                if !started.insert((*model, *cell)) {
                    return Err(PersistenceError::InvalidData("duplicate cell start"));
                }
            }
            RecordKind::CellCompleted {
                model, cell, code, ..
            } => {
                validate_cell(&models, *model, *cell, invocation, Some(code))?;
                if !completed.insert((*model, *cell)) {
                    return Err(PersistenceError::InvalidData("duplicate cell completion"));
                }
            }
            RecordKind::InvocationFinished { .. } => {
                if !terminal.insert(invocation) {
                    return Err(PersistenceError::InvalidData(
                        "duplicate invocation terminal",
                    ));
                }
            }
            RecordKind::OperationStarted {
                operation, name, ..
            } => {
                if operations
                    .insert((invocation, *operation), (stored.id, name == "cocoa.spawn"))
                    .is_some()
                {
                    return Err(PersistenceError::InvalidData("duplicate operation start"));
                }
            }
            RecordKind::OperationFinished { operation, outcome }
                if !operations.contains_key(&(invocation, *operation))
                    || finished.insert((invocation, *operation), outcome).is_some() =>
            {
                return Err(PersistenceError::InvalidData("unpaired operation finish"));
            }
            _ => {}
        }
    }
    let mut repairs = Vec::new();
    for ((invocation, operation), (start, spawn)) in operations {
        if let Some(outcome) = receipts.get(&start) {
            if let Some(finished) = finished.get(&(invocation, operation)) {
                if !receipt_matches(finished, outcome, spawn) {
                    return Err(PersistenceError::InvalidData(
                        "receipt conflicts with operation finish",
                    ));
                }
            } else {
                repairs.push(Record {
                    invocation: InvocationId(invocation),
                    kind: RecordKind::OperationFinished {
                        operation,
                        outcome: outcome.clone(),
                    },
                });
            }
        }
    }
    for (model, (invocation, cells)) in models {
        for (cell, requested) in cells.iter().enumerate() {
            let cell = u32::try_from(cell).map_err(|_| StoreError::Capacity)?;
            if !completed.contains(&(model, cell)) {
                if terminal.contains(&invocation) {
                    return Err(PersistenceError::InvalidData(
                        "terminal invocation missing cell feedback",
                    ));
                }
                let outcome = if started.contains(&(model, cell)) {
                    CellResponse::Interrupted {
                        printed: Vec::new(),
                    }
                } else {
                    CellResponse::NotStarted
                };
                repairs.push(Record {
                    invocation: InvocationId(invocation),
                    kind: RecordKind::CellCompleted {
                        model,
                        cell,
                        code: requested.code.clone(),
                        outcome,
                    },
                });
            }
        }
    }
    for invocation in invocations.difference(&terminal) {
        repairs.push(Record {
            invocation: InvocationId(*invocation),
            kind: RecordKind::InvocationFinished {
                outcome: InvocationOutcome::Interrupted,
            },
        });
    }
    Ok(repairs)
}

fn receipt_matches(finished: &OperationOutcome, receipt: &OperationOutcome, spawn: bool) -> bool {
    if spawn
        && let (
            OperationOutcome::Completed { value: finished },
            OperationOutcome::Completed { value: receipt },
        ) = (finished, receipt)
    {
        let Some(mut finished) = finished.as_object().cloned() else {
            return false;
        };
        if finished
            .remove("reference")
            .is_some_and(|reference| !reference.is_string())
        {
            return false;
        }
        return serde_json::Value::Object(finished) == *receipt;
    }
    finished == receipt
}

fn repairs(
    snapshot: &SessionSnapshot,
    receipts: &BTreeMap<RecordId, OperationOutcome>,
) -> Result<BTreeMap<AgentId, Vec<Record>>, PersistenceError> {
    let starts: BTreeSet<_> = snapshot
        .histories
        .values()
        .flat_map(|history| &history.records)
        .filter(|stored| matches!(stored.record.kind, RecordKind::OperationStarted { .. }))
        .map(|stored| stored.id)
        .collect();
    if receipts.keys().any(|id| !starts.contains(id)) {
        return Err(PersistenceError::InvalidData(
            "receipt is not an operation start",
        ));
    }
    snapshot
        .histories
        .iter()
        .map(|(agent, history)| {
            cocoa_core::engine::validate_recovery_snapshot(history)?;
            Ok((*agent, history_repairs(history, receipts)?))
        })
        .collect()
}

fn validate_cell(
    models: &BTreeMap<RecordId, (u64, &Vec<cocoa_core::execution::Cell>)>,
    model: RecordId,
    cell: u32,
    invocation: u64,
    code: Option<&str>,
) -> Result<(), PersistenceError> {
    let (author, cells) = models
        .get(&model)
        .ok_or(PersistenceError::InvalidData("cell authorizing model"))?;
    if *author != invocation
        || cells
            .get(cell as usize)
            .is_none_or(|cell| code.is_some_and(|code| code != cell.code))
    {
        return Err(PersistenceError::InvalidData("cell authorizing request"));
    }
    Ok(())
}
