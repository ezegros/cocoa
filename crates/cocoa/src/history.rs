//! Read-only, own-agent historical text with separate decoding and output bounds.

use std::{fmt::Write, sync::Arc, time::Duration};

use async_trait::async_trait;
use cocoa_core::{
    capability::{
        ActivityOutput, CapabilityBinding, CapabilityDescription, CapabilityError, CapabilityHost,
        CapabilityInput, CapabilityObserver, Effects, OperationOutcome,
    },
    engine::ActivityStatus,
    execution::{WorkLimits, host_value_fits},
    model::ModelAction,
    store::{RecordId, RecordKind, StoreLimits, StoredRecord, encoded_size},
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{sync::Semaphore, time::Instant};
use tokio_util::sync::CancellationToken;

use crate::{
    agents::AgentParticipant,
    persistence::{HistoryCursor, HistoryReadLimits, PersistenceError, SqliteStore},
};

const STORE_RECORD_BYTES_MAX: usize = 256 * 1024 * 1024;
const READ_BYTES_MAX: usize = 16 * 1024 * 1024;
const OUTPUT_BYTES_MAX: usize = 64 * 1024;
const TEXT_BYTES_MAX: u32 = 16 * 1024;
const ITEMS_MAX: u32 = 16;
const SCANNED_MAX: u32 = 64;
const METHODS: [&str; 3] = ["history.read", "history.list", "history.search"];
static CALLS: Semaphore = Semaphore::const_new(8);

pub(crate) fn bind(
    participant: AgentParticipant,
    inner: Arc<dyn CapabilityHost>,
) -> Arc<dyn CapabilityHost> {
    Arc::new(HistoryCapabilities { participant, inner })
}

struct HistoryCapabilities {
    participant: AgentParticipant,
    inner: Arc<dyn CapabilityHost>,
}

#[async_trait]
impl CapabilityHost for HistoryCapabilities {
    fn descriptions(&self) -> Vec<CapabilityDescription> {
        let mut descriptions = self.inner.descriptions();
        descriptions.retain(|description| !description.name.starts_with("history."));
        let common = "Shared policy for ALL history methods: read-only own-agent evidence, including originals before compaction. Use capabilities.call(name, arguments); no history JavaScript namespace is installed. IDs are decimal strings, never Numbers. Images and opaque provider continuations are excluded; text retrieval cannot reattach images. Each call decodes at most 16 MiB, returns at most 64 KiB and the current call budget, and has a two-second deadline. Already admitted reads may settle after cancellation. Arguments are at most 2048 encoded bytes; at most 8 calls run concurrently. Errors have no effects; memory-only history is unavailable. Unknown and null argument fields reject.";
        descriptions.extend([
            CapabilityDescription {
                name: METHODS[0].into(),
                description: format!("Retrieve text from one immutable record with capabilities.call('history.read', {{record:'123',offset:0,limit:8192}}). Use history.list to discover IDs or history.search for bounded literal matching. Returns null for an absent or foreign record, otherwise source metadata, text, offset, text_bytes, next_offset and complete. offset is a UTF-8 byte boundary in the rendered text; default 0. limit is 1..=16384 UTF-8 bytes, default 8192. Follow next_offset for remaining text; reduce limit on result_limit. A record above the decode allowance reports record_too_large, not absence. Structured execution values are compact JSON inside text. {common}"),
                parameters: json!({"type":"object","properties":{"record":{"type":"string","pattern":"^[1-9][0-9]*$"},"offset":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":TEXT_BYTES_MAX}},"required":["record"],"additionalProperties":false}),
            },
            CapabilityDescription {
                name: METHODS[1].into(),
                description: "Browse original-record metadata in ascending per-agent sequence with capabilities.call('history.list', {limit:8}), then pass next_cursor as cursor. limit is 1..=16 items, default 8. The first page pins through; appends after through do not enter continuation pages. Returns items, through, next_cursor, scanned, read_bytes, complete and stop. Metadata contains record, sequence, invocation, kind and image_count; use history.read for text. complete means the pinned range after the supplied cursor was consumed. stop names complete, limit, read_bytes or result_limit. Non-null next_cursor resumes unconsumed coverage. Empty items with complete:false is not evidence of absent history. Oversized records fail explicitly rather than being skipped.".into(),
                parameters: json!({"type":"object","properties":{"cursor":{"type":"string"},"limit":{"type":"integer","minimum":1,"maximum":ITEMS_MAX}},"additionalProperties":false}),
            },
            CapabilityDescription {
                name: METHODS[2].into(),
                description: "Find a case-sensitive literal substring of history.read's rendered text with capabilities.call('history.search', {query:'failed',limit:8}). query is 1..=512 UTF-8 bytes, without trimming, case folding, regex or SQL wildcard interpretation. Structured values use compact JSON, including its escaping. Returns the history.list paging fields and matching metadata with a UTF-8 snippet of at most 512 bytes and match_offset for history.read. Scans at most 64 records and 16 MiB per call; limit is 1..=16 matching records, default 8. stop may also be scan_limit. Follow next_cursor with the same query to search the remaining pinned range. No matches with complete:false means partial coverage, not absent evidence.".into(),
                parameters: json!({"type":"object","properties":{"query":{"type":"string","minLength":1,"maxLength":512},"cursor":{"type":"string"},"limit":{"type":"integer","minimum":1,"maximum":ITEMS_MAX}},"required":["query"],"additionalProperties":false}),
            },
        ]);
        descriptions
    }

    fn bindings(&self) -> Vec<CapabilityBinding> {
        self.inner.bindings()
    }

    async fn call(
        &self,
        name: &str,
        arguments: Value,
        limits: WorkLimits,
        cancellation: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        if !name.starts_with("history.") {
            return self.inner.call(name, arguments, limits, cancellation).await;
        }
        let limits = WorkLimits {
            deadline: limits.deadline.min(Instant::now() + Duration::from_secs(2)),
            output_bytes: limits.output_bytes.min(OUTPUT_BYTES_MAX),
        };
        check(limits, &cancellation)?;
        let request = Request::parse(name, arguments)?;
        let _permit = CALLS
            .try_acquire()
            .map_err(|_| rejected("busy", "At most 8 historical text calls may run at once"))?;
        let store = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(cancelled()),
            () = tokio::time::sleep_until(limits.deadline) => return Err(deadline()),
            store = self.participant.durable_store(StoreLimits { record_bytes: STORE_RECORD_BYTES_MAX }) => {
                store.map_err(storage_error)?.ok_or_else(|| rejected("unavailable", "Historical text retrieval requires this agent's durable SQLite store; memory-only history is unavailable"))?
            }
        };
        let result = match request {
            Request::Read {
                record,
                offset,
                limit,
            } => read(&store, record, offset, limit, limits, &cancellation).await,
            Request::Browse {
                cursor,
                limit,
                query,
            } => {
                browse(
                    &store,
                    cursor,
                    limit,
                    query.as_deref(),
                    limits,
                    &cancellation,
                )
                .await
            }
        }?;
        check(limits, &cancellation)?;
        fits(result, limits.output_bytes)
    }

    async fn call_observed(
        &self,
        name: &str,
        arguments: Value,
        limits: WorkLimits,
        cancellation: CancellationToken,
        observer: CapabilityObserver,
    ) -> Result<Value, CapabilityError> {
        if name.starts_with("history.") {
            observer.input(&self.describe_input(name, &arguments));
            return self.call(name, arguments, limits, cancellation).await;
        }
        self.inner
            .call_observed(name, arguments, limits, cancellation, observer)
            .await
    }

    async fn call_started(
        &self,
        name: &str,
        arguments: Value,
        limits: WorkLimits,
        cancellation: CancellationToken,
        observer: CapabilityObserver,
        operation_record: RecordId,
    ) -> Result<Value, CapabilityError> {
        if name.starts_with("history.") {
            observer.input(&self.describe_input(name, &arguments));
            return self.call(name, arguments, limits, cancellation).await;
        }
        self.inner
            .call_started(
                name,
                arguments,
                limits,
                cancellation,
                observer,
                operation_record,
            )
            .await
    }

    fn describe_input(&self, name: &str, arguments: &Value) -> CapabilityInput {
        if !name.starts_with("history.") {
            return self.inner.describe_input(name, arguments);
        }
        CapabilityInput {
            title: name.into(),
            text: arguments
                .get("record")
                .and_then(Value::as_str)
                .unwrap_or("Own-agent historical text")
                .into(),
            summary: None,
        }
    }

    fn describe_output(
        &self,
        name: &str,
        arguments: &Value,
        outcome: &OperationOutcome,
    ) -> Option<ActivityOutput> {
        if name.starts_with("history.") {
            None
        } else {
            self.inner.describe_output(name, arguments, outcome)
        }
    }

    fn activity_status(&self, outcome: &OperationOutcome) -> ActivityStatus {
        self.inner.activity_status(outcome)
    }

    async fn observe_outcome(
        &self,
        name: &str,
        arguments: &Value,
        outcome: &OperationOutcome,
        observer: CapabilityObserver,
    ) {
        if name.starts_with("history.") {
            observer.status(ActivityStatus::from(outcome));
        } else {
            self.inner
                .observe_outcome(name, arguments, outcome, observer)
                .await;
        }
    }
}

enum Request {
    Read {
        record: RecordId,
        offset: u32,
        limit: u32,
    },
    Browse {
        cursor: Option<HistoryCursor>,
        limit: u32,
        query: Option<String>,
    },
}

impl Request {
    fn parse(name: &str, arguments: Value) -> Result<Self, CapabilityError> {
        if !METHODS.contains(&name) {
            return Err(rejected(
                "unknown_method",
                "Installed historical text methods are history.read, history.list and history.search",
            ));
        }
        if !host_value_fits(&arguments, 2048)
            || arguments
                .as_object()
                .is_none_or(|fields| fields.values().any(Value::is_null))
        {
            return Err(rejected(
                "invalid_arguments",
                "Expected a closed argument object without null fields, at most 2048 encoded bytes",
            ));
        }
        match name {
            "history.read" => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Arguments {
                    record: String,
                    offset: Option<u32>,
                    limit: Option<u32>,
                }
                let arguments: Arguments = serde_json::from_value(arguments).map_err(|_| {
                    rejected(
                        "invalid_arguments",
                        "Expected {record:decimal_string,offset?:u32,limit?:1..=16384}",
                    )
                })?;
                let record = arguments.record.parse::<i64>().ok()
                    .filter(|record| record.to_string() == arguments.record)
                    .and_then(|record| RecordId::try_from(record).ok())
                    .ok_or_else(|| rejected("invalid_arguments", "record must be a canonical positive decimal string fitting signed 64-bit integers"))?;
                let limit = arguments.limit.unwrap_or(8192);
                if !(1..=TEXT_BYTES_MAX).contains(&limit) {
                    return Err(rejected(
                        "invalid_arguments",
                        "history.read limit must be 1..=16384 UTF-8 bytes",
                    ));
                }
                Ok(Self::Read {
                    record,
                    offset: arguments.offset.unwrap_or(0),
                    limit,
                })
            }
            "history.list" => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Arguments {
                    cursor: Option<String>,
                    limit: Option<u32>,
                }
                let arguments: Arguments = serde_json::from_value(arguments).map_err(|_| {
                    rejected(
                        "invalid_arguments",
                        "Expected {cursor?:string,limit?:1..=16}",
                    )
                })?;
                Self::page(arguments.cursor, arguments.limit, None)
            }
            _ => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Arguments {
                    query: String,
                    cursor: Option<String>,
                    limit: Option<u32>,
                }
                let arguments: Arguments = serde_json::from_value(arguments).map_err(|_| {
                    rejected(
                        "invalid_arguments",
                        "Expected {query:string,cursor?:string,limit?:1..=16}",
                    )
                })?;
                if arguments.query.is_empty() || arguments.query.len() > 512 {
                    return Err(rejected(
                        "invalid_arguments",
                        "history.search query must be 1..=512 UTF-8 bytes",
                    ));
                }
                Self::page(arguments.cursor, arguments.limit, Some(arguments.query))
            }
        }
    }

    fn page(
        cursor: Option<String>,
        limit: Option<u32>,
        query: Option<String>,
    ) -> Result<Self, CapabilityError> {
        let cursor = cursor
            .map(|cursor| cursor.parse())
            .transpose()
            .map_err(storage_error)?;
        let limit = limit.unwrap_or(8);
        if !(1..=ITEMS_MAX).contains(&limit) {
            return Err(rejected(
                "invalid_arguments",
                "Historical page limit must be 1..=16 items",
            ));
        }
        Ok(Self::Browse {
            cursor,
            limit,
            query,
        })
    }
}

async fn read(
    store: &SqliteStore,
    record: RecordId,
    offset: u32,
    limit: u32,
    limits: WorkLimits,
    cancellation: &CancellationToken,
) -> Result<Value, CapabilityError> {
    let stored = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(cancelled()),
        () = tokio::time::sleep_until(limits.deadline) => return Err(deadline()),
        result = store.read_record(record, READ_BYTES_MAX as u64) => result.map_err(storage_error)?,
    };
    let Some(stored) = stored else {
        return Ok(Value::Null);
    };
    check(limits, cancellation)?;
    let text = render(&stored)?;
    check(limits, cancellation)?;
    let offset = offset as usize;
    if !text.is_char_boundary(offset) {
        return Err(rejected(
            "invalid_arguments",
            "offset must be a UTF-8 boundary within text_bytes",
        ));
    }
    let end = text_end(&text, offset.saturating_add(limit as usize));
    if end == offset && end < text.len() {
        return Err(rejected(
            "invalid_arguments",
            "limit is too small for the next UTF-8 character",
        ));
    }
    let mut result = metadata(&stored);
    result["offset"] = json!(offset);
    result["text"] = json!(&text[offset..end]);
    result["text_bytes"] = json!(text.len());
    result["next_offset"] = if end < text.len() {
        json!(end)
    } else {
        Value::Null
    };
    result["complete"] = json!(end == text.len());
    Ok(result)
}

async fn browse(
    store: &SqliteStore,
    mut cursor: Option<HistoryCursor>,
    limit: u32,
    query: Option<&str>,
    limits: WorkLimits,
    cancellation: &CancellationToken,
) -> Result<Value, CapabilityError> {
    let mut items = Vec::with_capacity(limit as usize);
    let mut through = None;
    let mut scanned = 0_u32;
    let mut read_bytes = 0_usize;
    let mut stop = "complete";
    let mut complete = false;
    for _ in 0..SCANNED_MAX {
        check(limits, cancellation)?;
        let remaining = READ_BYTES_MAX.saturating_sub(read_bytes);
        if remaining == 0 {
            stop = "read_bytes";
            break;
        }
        let page_limits = HistoryReadLimits::new(1, remaining as u64).map_err(storage_error)?;
        let page = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(cancelled()),
            () = tokio::time::sleep_until(limits.deadline) => return Err(deadline()),
            result = store.read_history(cursor.clone(), page_limits) => result,
        };
        let page = match page {
            Ok(page) => page,
            Err(PersistenceError::HistoryRecordTooLarge { bytes_required, .. })
                if scanned > 0 && bytes_required <= READ_BYTES_MAX as u64 =>
            {
                stop = "read_bytes";
                break;
            }
            Err(error) => return Err(storage_error(error)),
        };
        let bytes = encoded_size(&page)
            .map_err(|_| rejected("storage", "Historical page size could not be encoded"))?;
        read_bytes += bytes;
        through = page.through.map(|sequence| i64::from(sequence).to_string());
        let Some(stored) = page.records.first() else {
            complete = true;
            cursor = None;
            break;
        };
        check(limits, cancellation)?;
        let item = if let Some(query) = query {
            let text = render(stored)?;
            check(limits, cancellation)?;
            text.find(query).map(|offset| {
                let mut item = metadata(stored);
                let mut start = offset.saturating_sub(128);
                while !text.is_char_boundary(start) {
                    start += 1;
                }
                let end = text_end(&text, start.saturating_add(512));
                item["snippet"] = json!(&text[start..end]);
                item["match_offset"] = json!(offset);
                item
            })
        } else {
            Some(metadata(stored))
        };
        if let Some(item) = item {
            items.push(item);
            let candidate = page_value(
                &items,
                &through,
                &page.next_cursor,
                scanned + 1,
                read_bytes,
                page.next_cursor.is_none(),
                "result_limit",
            );
            if !host_value_fits(&candidate, limits.output_bytes) {
                items.pop();
                if scanned == 0 {
                    return Err(rejected(
                        "result_limit",
                        "One historical metadata item cannot fit the output budget",
                    ));
                }
                stop = "result_limit";
                break;
            }
        }
        scanned += 1;
        cursor = page.next_cursor;
        if cursor.is_none() {
            complete = true;
            break;
        }
        if items.len() >= limit as usize {
            stop = "limit";
            break;
        }
        stop = "scan_limit";
    }
    Ok(page_value(
        &items, &through, &cursor, scanned, read_bytes, complete, stop,
    ))
}

fn page_value(
    items: &[Value],
    through: &Option<String>,
    cursor: &Option<HistoryCursor>,
    scanned: u32,
    read_bytes: usize,
    complete: bool,
    stop: &str,
) -> Value {
    json!({"items":items,"through":through,"next_cursor":cursor,"scanned":scanned,"read_bytes":read_bytes,"complete":complete,"stop":stop})
}

fn metadata(stored: &StoredRecord) -> Value {
    let kind = match &stored.record.kind {
        RecordKind::InputAccepted { .. } => "input_accepted",
        RecordKind::ModelCompleted { .. } => "model_completed",
        RecordKind::CellStarted { .. } => "cell_started",
        RecordKind::CellCompleted { .. } => "cell_completed",
        RecordKind::InvocationFinished { .. } => "invocation_finished",
        RecordKind::TurnCancelled { .. } => "turn_cancelled",
        RecordKind::OperationStarted { .. } => "operation_started",
        RecordKind::OperationFinished { .. } => "operation_finished",
    };
    let image_count = match &stored.record.kind {
        RecordKind::InputAccepted { input } => input.images().len(),
        _ => 0,
    };
    let mut value = json!({
        "record":i64::from(stored.id).to_string(),
        "sequence":i64::from(stored.sequence).to_string(),
        "invocation":stored.record.invocation.0.to_string(),
        "kind":kind,"image_count":image_count,
    });
    if image_count > 0 {
        value["image_notice"] = json!(format!(
            "Source record {}; image ordinals 1..={image_count} in original attachment order. Image payloads excluded from text; original inputs remain inline. No historical-image reattachment is installed.",
            i64::from(stored.id)
        ));
    }
    value
}

fn render(stored: &StoredRecord) -> Result<String, CapabilityError> {
    let encoding = || rejected("storage", "Historical text could not be encoded");
    let text = match &stored.record.kind {
        RecordKind::InputAccepted { input } => input.text().to_owned(),
        RecordKind::ModelCompleted { response } => match &response.action {
            ModelAction::Final { answer } => answer.clone(),
            ModelAction::Execute { cells } => {
                let mut text = String::new();
                for (ordinal, cell) in cells.iter().enumerate() {
                    writeln!(text, "cell {ordinal}:\n{}", cell.code).map_err(|_| encoding())?;
                }
                text
            }
        },
        RecordKind::CellStarted { model, cell } => {
            format!("model_record=\"{}\" cell={cell}", i64::from(*model))
        }
        RecordKind::CellCompleted {
            model,
            cell,
            code,
            outcome,
        } => format!(
            "model_record=\"{}\" cell={cell}\n{code}\noutcome={}",
            i64::from(*model),
            serde_json::to_string(outcome).map_err(|_| encoding())?
        ),
        RecordKind::OperationStarted {
            operation,
            name,
            arguments,
        } => format!(
            "operation=\"{}\"\n{name}\narguments={}",
            operation.0,
            serde_json::to_string(arguments).map_err(|_| encoding())?
        ),
        RecordKind::OperationFinished { operation, outcome } => format!(
            "operation=\"{}\"\noutcome={}",
            operation.0,
            serde_json::to_string(outcome).map_err(|_| encoding())?
        ),
        RecordKind::InvocationFinished { outcome } => {
            serde_json::to_string(outcome).map_err(|_| encoding())?
        }
        RecordKind::TurnCancelled { runtime } => {
            serde_json::to_string(runtime).map_err(|_| encoding())?
        }
    };
    Ok(redact_images(text))
}

fn redact_images(text: String) -> String {
    let mut rest = text.as_str();
    let mut result = String::new();
    while let Some(start) = rest.find("data:image/") {
        result.push_str(&rest[..start]);
        rest = &rest[start..];
        let end = rest
            .find(|character: char| {
                character.is_whitespace() || matches!(character, '"' | '\'' | '<' | '>' | '\\')
            })
            .unwrap_or(rest.len());
        result.push_str("[image]");
        rest = &rest[end..];
    }
    if result.is_empty() {
        text
    } else {
        result.push_str(rest);
        result
    }
}

fn text_end(text: &str, maximum: usize) -> usize {
    let mut end = maximum.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

fn fits(value: Value, bytes: usize) -> Result<Value, CapabilityError> {
    if host_value_fits(&value, bytes) {
        Ok(value)
    } else {
        Err(rejected(
            "result_limit",
            "Historical text JSON exceeds 65536 bytes or the current call budget; reduce limit",
        ))
    }
}

fn check(limits: WorkLimits, cancellation: &CancellationToken) -> Result<(), CapabilityError> {
    if cancellation.is_cancelled() {
        return Err(cancelled());
    }
    if Instant::now() >= limits.deadline {
        return Err(deadline());
    }
    Ok(())
}

fn cancelled() -> CapabilityError {
    rejected(
        "cancelled",
        "Historical text read cancelled; no writes were performed",
    )
}
fn deadline() -> CapabilityError {
    rejected(
        "deadline",
        "Historical text read exceeded its two-second or caller deadline; no writes were performed",
    )
}

fn storage_error(error: PersistenceError) -> CapabilityError {
    match error {
        PersistenceError::InvalidHistoryCursor => rejected(
            "invalid_cursor",
            "cursor is invalid or belongs to another session or agent",
        ),
        PersistenceError::HistoryRecordTooLarge {
            record,
            bytes_required,
        } => rejected(
            "record_too_large",
            &format!(
                "Record \"{}\" requires {bytes_required} encoded read bytes; the per-call read/decode allowance is 16777216 bytes. Evidence was not skipped or treated as absent",
                i64::from(record)
            ),
        ),
        PersistenceError::OutcomeUnknown => rejected(
            "outcome_unknown",
            "An earlier write has an uncertain outcome. Historical reads do not acknowledge it; host reconciliation is required",
        ),
        PersistenceError::Busy => rejected(
            "busy",
            "Historical storage admission or lock wait exhausted",
        ),
        PersistenceError::AgentUnavailable => rejected(
            "unavailable",
            "This agent's durable history authority is unavailable or stale",
        ),
        _ => rejected(
            "storage",
            "Historical evidence could not be read or decoded; this is not evidence of absent history",
        ),
    }
}

fn rejected(code: &str, message: &str) -> CapabilityError {
    CapabilityError::Tool {
        code: code.into(),
        message: message.into(),
        effects: Effects::None,
    }
}
