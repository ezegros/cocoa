//! Native Anthropic Messages encoding and strict streaming assembly.
//! Signed and redacted thinking is opaque replay data, never display text.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use cocoa_core::{
    engine::ObservationSink,
    model::{ModelAction, ModelError, ModelRequest, ModelResponse, ProviderData, Usage},
    store::RecordKind,
};
use eventsource_stream::Event;
use serde_json::{Value, json};

use crate::{
    DECODER_BYTES, ModelLimits, ProviderError, ProviderKind, instructions,
    transport::{
        CALLS, ExecutionMode, ThinkingLevel, byte_limit, check_text_bytes, encode_request,
        exec_cell, execution_function, identifier, protocol, text,
    },
};

const BLOCKS: usize = 1024;

pub(crate) fn validate_options(
    model: &str,
    thinking: Option<ThinkingLevel>,
) -> Result<(), ProviderError> {
    let Some(level) = thinking else { return Ok(()) };
    let adaptive = matches!(
        model,
        "claude-opus-4-6" | "claude-sonnet-4-6" | "claude-opus-4-7" | "claude-opus-4-8"
    );
    let supported = adaptive
        && match level {
            ThinkingLevel::Minimal => false,
            ThinkingLevel::XHigh => matches!(model, "claude-opus-4-7" | "claude-opus-4-8"),
            _ => true,
        };
    if supported {
        Ok(())
    } else {
        Err(ProviderError::UnsupportedThinkingLevel)
    }
}

pub(crate) fn body(
    request: &ModelRequest,
    model: &str,
    execution: ExecutionMode,
    model_limits: ModelLimits,
    thinking_level: Option<ThinkingLevel>,
) -> Result<Vec<u8>, ModelError> {
    let mut messages = Vec::new();
    let mut pending = VecDeque::new();
    let mut results = Vec::new();
    let mut current_invocation = None;
    if let Some(checkpoint) = crate::checkpoint_context(request)? {
        messages.push(json!({"role":"user","content":checkpoint}));
    }
    for (index, record) in request.context.records.iter().enumerate() {
        match &record.kind {
            RecordKind::InputAccepted { input } if pending.is_empty() => {
                current_invocation = Some(record.invocation);
                let mut content = Vec::with_capacity(input.images().len() + 1);
                if !input.text().is_empty() {
                    content.push(json!({"type":"text","text":input.text()}));
                }
                for image in input.images() {
                    let data = image
                        .data_url()
                        .strip_prefix("data:image/png;base64,")
                        .ok_or_else(|| protocol("unsupported image"))?;
                    content.push(json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":data}}));
                }
                messages.push(json!({"role":"user","content":content}));
            }
            RecordKind::ModelCompleted {
                response,
                shared_context,
            } if pending.is_empty() => {
                if current_invocation != Some(record.invocation) {
                    return Err(protocol("model invocation mismatch"));
                }
                if let Some(shared) = crate::shared_context(shared_context.as_ref())? {
                    messages.push(json!({"role":"user","content":shared}));
                }
                let continuation = response
                    .continuation
                    .as_ref()
                    .filter(|data| data.provider == ProviderKind::Anthropic.continuation_provider())
                    .ok_or_else(|| protocol("incompatible continuation"))?;
                let message = &continuation.data["message"];
                if action(message, execution, DECODER_BYTES)? != response.action {
                    return Err(protocol("continuation action mismatch"));
                }
                if let ModelAction::Execute { cells } = &response.action {
                    let content = message["content"]
                        .as_array()
                        .ok_or_else(|| protocol("missing content"))?;
                    let calls = content.iter().filter(|block| block["type"] == "tool_use");
                    for (ordinal, (call, cell)) in calls.zip(cells).enumerate() {
                        pending.push_back((
                            record.invocation,
                            ordinal,
                            identifier(call, "id")?,
                            cell.code.as_str(),
                        ));
                    }
                }
                messages.push(json!({"role":"assistant","content":message["content"]}));
            }
            RecordKind::CellStarted { cell, .. } => {
                let (invocation, ordinal, _, _) = pending
                    .front()
                    .ok_or_else(|| protocol("unmatched cell start"))?;
                if *invocation != record.invocation || *cell as usize != *ordinal {
                    return Err(protocol("cell start mismatch"));
                }
            }
            RecordKind::CellCompleted {
                cell,
                code,
                outcome,
                ..
            } => {
                let (invocation, ordinal, call_id, expected) = pending
                    .pop_front()
                    .ok_or_else(|| protocol("unmatched cell feedback"))?;
                if invocation != record.invocation || *cell as usize != ordinal || code != expected
                {
                    return Err(protocol("cell feedback mismatch"));
                }
                let content = crate::execution_feedback(request, index, outcome)?;
                results.push(json!({"type":"tool_result","tool_use_id":call_id,"content":content}));
                if pending.is_empty() {
                    messages.push(json!({"role":"user","content":std::mem::take(&mut results)}));
                }
            }
            RecordKind::TurnCancelled { runtime } if pending.is_empty() => {
                if current_invocation != Some(record.invocation) {
                    return Err(protocol("cancellation invocation mismatch"));
                }
                messages
                    .push(json!({"role":"user","content":crate::cancellation_notice(*runtime)}));
            }
            RecordKind::InvocationFinished { .. } if pending.is_empty() => {
                if current_invocation != Some(record.invocation) {
                    return Err(protocol("finished invocation mismatch"));
                }
                current_invocation = None;
            }
            RecordKind::OperationStarted { .. } | RecordKind::OperationFinished { .. } => {}
            _ => return Err(protocol("unsupported history")),
        }
    }
    if !pending.is_empty() {
        return Err(protocol("missing cell feedback"));
    }
    if let Some(shared) = crate::shared_context(request.context.shared_update())? {
        messages.push(json!({"role":"user","content":shared}));
    }
    encode_messages(
        messages,
        &instructions(request, execution)?,
        model,
        execution,
        model_limits,
        thinking_level,
    )
}

pub(crate) fn encode_messages(
    messages: Vec<Value>,
    system: &str,
    model: &str,
    execution: ExecutionMode,
    model_limits: ModelLimits,
    thinking_level: Option<ThinkingLevel>,
) -> Result<Vec<u8>, ModelError> {
    let mut body = json!({"model":model,"messages":messages,"stream":true,"cache_control":{"type":"ephemeral"}});
    if !system.is_empty() {
        body["system"] =
            json!([{"type":"text","text":system,"cache_control":{"type":"ephemeral"}}]);
    }
    if execution == ExecutionMode::Enabled {
        let function = execution_function();
        body["tools"] = json!([{"name":function["name"],"description":function["description"],"input_schema":function["parameters"],"strict":true}]);
    }
    if let Some(level) = thinking_level {
        if level == ThinkingLevel::None {
            body["thinking"] = json!({"type":"disabled"});
        } else {
            body["thinking"] = json!({"type":"adaptive"});
            body["output_config"] = json!({"effort":level});
        }
    }
    encode_request(body, model_limits, "max_tokens")
}

#[derive(Default)]
pub(crate) struct MessageStream {
    message: Option<Value>,
    blocks: BTreeMap<usize, Block>,
    ids: BTreeSet<String>,
    delta: bool,
    stopped: bool,
    usage: Option<Usage>,
    text_bytes: usize,
}

struct Block {
    value: Value,
    arguments: String,
    closed: bool,
    signature: bool,
}

impl MessageStream {
    pub(crate) fn accept_event(
        &mut self,
        event: &Event,
        execution: ExecutionMode,
        output_bytes: usize,
        observations: Option<&ObservationSink>,
    ) -> Result<Option<ModelResponse>, ModelError> {
        let value: Value =
            serde_json::from_str(&event.data).map_err(|_| protocol("invalid event JSON"))?;
        let fragment = match value["type"].as_str() {
            Some("content_block_start") if value["content_block"]["type"] == "text" => {
                value["content_block"]["text"].as_str()
            }
            Some("content_block_delta") if value["delta"]["type"] == "text_delta" => {
                value["delta"]["text"].as_str()
            }
            _ => None,
        };
        if let Some(fragment) = fragment {
            self.text_bytes = self.text_bytes.saturating_add(fragment.len());
            check_text_bytes(self.text_bytes, output_bytes)?;
        }
        self.accept(&event.event, &value, DECODER_BYTES, observations)?;
        if value["type"] == "message_stop" {
            return std::mem::take(self)
                .completed(execution, DECODER_BYTES)
                .map(Some);
        }
        Ok(None)
    }

    fn accept(
        &mut self,
        event_name: &str,
        value: &Value,
        limit: usize,
        observations: Option<&ObservationSink>,
    ) -> Result<(), ModelError> {
        if self.stopped || value["type"] != event_name {
            return Err(protocol("unexpected Messages event"));
        }
        match event_name {
            "ping" => {}
            "error" => return Err(protocol("Messages provider error")),
            "message_start" => {
                let message = &value["message"];
                if self.message.is_some()
                    || message["type"] != "message"
                    || message["role"] != "assistant"
                    || message["content"]
                        .as_array()
                        .is_none_or(|content| !content.is_empty())
                    || !message["stop_reason"].is_null()
                {
                    return Err(protocol("invalid message start"));
                }
                identifier(message, "id")?;
                self.usage = Some(usage(&message["usage"])?);
                self.message = Some(message.clone());
            }
            "content_block_start" => {
                if self.message.is_none() || self.delta {
                    return Err(protocol("unexpected block start"));
                }
                let index = block_index(value)?;
                if self.blocks.contains_key(&index) {
                    return Err(protocol("duplicate block index"));
                }
                let block = &value["content_block"];
                match text(block, "type")? {
                    "text" => {
                        checked(text(block, "text")?, limit)?;
                        if let Some(observations) = observations {
                            observations.assistant_text(text(block, "text")?);
                        }
                    }
                    "thinking" => {
                        checked(text(block, "thinking")?, limit)?;
                        checked(text(block, "signature")?, limit)?;
                        if let Some(observations) = observations {
                            observations.reasoning_started(index as u32);
                        }
                    }
                    "redacted_thinking" => {
                        checked(text(block, "data")?, limit)?;
                    }
                    "tool_use" => {
                        let id = identifier(block, "id")?;
                        if self.ids.len() >= CALLS || !self.ids.insert(id.to_owned()) {
                            return Err(protocol("duplicate or excess tool calls"));
                        }
                        if identifier(block, "name")? != "exec" || !block["input"].is_object() {
                            return Err(protocol("invalid tool block"));
                        }
                    }
                    _ => return Err(protocol("unsupported content block")),
                }
                self.blocks.insert(
                    index,
                    Block {
                        value: block.clone(),
                        arguments: String::new(),
                        closed: false,
                        signature: false,
                    },
                );
            }
            "content_block_delta" => {
                if self.delta {
                    return Err(protocol("late block delta"));
                }
                let index = block_index(value)?;
                let block = self
                    .blocks
                    .get_mut(&index)
                    .filter(|block| !block.closed)
                    .ok_or_else(|| protocol("delta without open block"))?;
                let delta = &value["delta"];
                match (text(&block.value, "type")?, text(delta, "type")?) {
                    ("text", "text_delta") => {
                        append_field(&mut block.value, "text", text(delta, "text")?, limit)?;
                        if let Some(observations) = observations {
                            observations.assistant_text(text(delta, "text")?);
                        }
                    }
                    ("thinking", "thinking_delta") if !block.signature => {
                        append_field(
                            &mut block.value,
                            "thinking",
                            text(delta, "thinking")?,
                            limit,
                        )?;
                    }
                    ("thinking", "signature_delta") => {
                        append_field(
                            &mut block.value,
                            "signature",
                            text(delta, "signature")?,
                            limit,
                        )?;
                        block.signature = true;
                    }
                    ("tool_use", "input_json_delta") if block.value["input"] == json!({}) => {
                        append(&mut block.arguments, text(delta, "partial_json")?, limit)?;
                    }
                    _ => return Err(protocol("invalid block delta")),
                }
            }
            "content_block_stop" => {
                let index = block_index(value)?;
                let block = self
                    .blocks
                    .get_mut(&index)
                    .filter(|block| !block.closed)
                    .ok_or_else(|| protocol("stop without open block"))?;
                match text(&block.value, "type")? {
                    "tool_use" if !block.arguments.is_empty() => {
                        let cell = exec_cell(&block.arguments, limit)?;
                        block.value["input"] = json!({"code":cell.code});
                    }
                    "thinking" => {
                        if text(&block.value, "signature")?.is_empty() {
                            return Err(protocol("unsigned thinking"));
                        }
                        if let Some(observations) = observations {
                            observations.reasoning_finished(index as u32);
                        }
                    }
                    _ => {}
                }
                block.closed = true;
            }
            "message_delta" => {
                if self.message.is_none() || self.blocks.values().any(|block| !block.closed) {
                    return Err(protocol("message delta before blocks finish"));
                }
                let delta = &value["delta"];
                if let Some(reason) = delta.get("stop_reason").filter(|reason| !reason.is_null()) {
                    if self
                        .message
                        .as_ref()
                        .is_some_and(|message| !message["stop_reason"].is_null())
                    {
                        return Err(protocol("duplicate stop reason"));
                    }
                    let message = self
                        .message
                        .as_mut()
                        .ok_or_else(|| protocol("missing message"))?;
                    message["stop_reason"] = reason.clone();
                    message["stop_sequence"] = delta["stop_sequence"].clone();
                }
                let output = value["usage"]["output_tokens"]
                    .as_u64()
                    .ok_or_else(|| protocol("invalid usage"))?;
                if self
                    .usage
                    .as_ref()
                    .is_some_and(|usage| output < usage.output_tokens)
                {
                    return Err(protocol("decreasing output usage"));
                }
                let message = self
                    .message
                    .as_mut()
                    .ok_or_else(|| protocol("missing message"))?;
                for field in [
                    "input_tokens",
                    "output_tokens",
                    "cache_creation_input_tokens",
                    "cache_read_input_tokens",
                ] {
                    if let Some(count) = value["usage"].get(field) {
                        message["usage"][field] = count.clone();
                    }
                }
                self.usage = Some(usage(&message["usage"])?);
                self.delta = true;
            }
            "message_stop" => {
                if !self.delta || self.blocks.values().any(|block| !block.closed) {
                    return Err(protocol("unfinished message"));
                }
                self.stopped = true;
            }
            _ => return Err(protocol("unsupported Messages event")),
        }
        Ok(())
    }

    pub(crate) fn completed(
        self,
        execution: ExecutionMode,
        limit: usize,
    ) -> Result<ModelResponse, ModelError> {
        if !self.stopped {
            return Err(protocol("missing message_stop"));
        }
        let mut message = self.message.ok_or_else(|| protocol("missing message"))?;
        let mut content = Vec::with_capacity(self.blocks.len());
        for (expected, (index, block)) in self.blocks.into_iter().enumerate() {
            if expected != index || !block.closed {
                return Err(protocol("incomplete content"));
            }
            content.push(block.value);
        }
        message["content"] = json!(content);
        let action = action(&message, execution, limit)?;
        Ok(ModelResponse {
            action,
            continuation: Some(ProviderData {
                provider: ProviderKind::Anthropic.continuation_provider().into(),
                data: json!({"message":message}),
            }),
            usage: self.usage,
        })
    }
}

fn action(
    message: &Value,
    execution: ExecutionMode,
    limit: usize,
) -> Result<ModelAction, ModelError> {
    if message["type"] != "message" || message["role"] != "assistant" {
        return Err(protocol("invalid assistant message"));
    }
    identifier(message, "id")?;
    let content = message["content"]
        .as_array()
        .ok_or_else(|| protocol("missing content"))?;
    if content.len() > BLOCKS {
        return Err(protocol("too many content blocks"));
    }
    let mut answer = String::new();
    let mut cells = Vec::new();
    let mut ids = BTreeSet::new();
    for block in content {
        match text(block, "type")? {
            "text" => append(&mut answer, text(block, "text")?, limit)?,
            "thinking" => {
                checked(text(block, "thinking")?, limit)?;
                let signature = text(block, "signature")?;
                if signature.is_empty() {
                    return Err(protocol("unsigned thinking"));
                }
                checked(signature, limit)?;
            }
            "redacted_thinking" => {
                let data = text(block, "data")?;
                if data.is_empty() {
                    return Err(protocol("empty redacted thinking"));
                }
                checked(data, limit)?;
            }
            "tool_use" => {
                if execution != ExecutionMode::Enabled
                    || cells.len() >= CALLS
                    || identifier(block, "name")? != "exec"
                    || !ids.insert(identifier(block, "id")?)
                {
                    return Err(protocol("invalid tool call"));
                }
                let arguments = serde_json::to_string(&block["input"])
                    .map_err(|_| protocol("invalid tool input"))?;
                cells.push(exec_cell(&arguments, limit)?);
            }
            _ => return Err(protocol("unsupported content block")),
        }
    }
    match (text(message, "stop_reason")?, cells.is_empty()) {
        ("end_turn", true) if !answer.is_empty() => Ok(ModelAction::Final { answer }),
        ("tool_use", false) => Ok(ModelAction::Execute { cells }),
        _ => Err(protocol("Messages response did not finish successfully")),
    }
}

fn block_index(value: &Value) -> Result<usize, ModelError> {
    let index = value["index"]
        .as_u64()
        .ok_or_else(|| protocol("invalid block index"))?;
    if index >= BLOCKS as u64 {
        return Err(protocol("block index exceeds limit"));
    }
    Ok(index as usize)
}

pub(crate) fn usage(value: &Value) -> Result<Usage, ModelError> {
    let input = value["input_tokens"]
        .as_u64()
        .ok_or_else(|| protocol("invalid input usage"))?;
    let cache_count = |field: &str| {
        value
            .get(field)
            .filter(|count| !count.is_null())
            .map(|count| {
                count
                    .as_u64()
                    .ok_or_else(|| protocol("invalid cache usage"))
            })
            .transpose()
    };
    let read_tokens = cache_count("cache_read_input_tokens")?;
    let write_tokens = cache_count("cache_creation_input_tokens")?;
    let total = input
        .checked_add(read_tokens.unwrap_or(0))
        .and_then(|total| total.checked_add(write_tokens.unwrap_or(0)))
        .ok_or_else(|| protocol("usage overflow"))?;
    Ok(Usage {
        input_tokens: total,
        output_tokens: value["output_tokens"]
            .as_u64()
            .ok_or_else(|| protocol("invalid output usage"))?,
        input_cache: if read_tokens.is_some() || write_tokens.is_some() {
            Some(cocoa_core::model::InputCacheUsage {
                read_tokens,
                write_tokens,
            })
        } else {
            None
        },
    })
}

fn checked(value: &str, limit: usize) -> Result<(), ModelError> {
    if value.len() > limit {
        return Err(byte_limit("response", "messages_field", value.len(), limit));
    }
    Ok(())
}

fn append(output: &mut String, delta: &str, limit: usize) -> Result<(), ModelError> {
    let bytes = output.len().saturating_add(delta.len());
    if bytes > limit {
        return Err(byte_limit("response", "messages_field", bytes, limit));
    }
    output.push_str(delta);
    Ok(())
}

fn append_field(value: &mut Value, key: &str, delta: &str, limit: usize) -> Result<(), ModelError> {
    match value.get_mut(key) {
        Some(Value::String(output)) => append(output, delta, limit),
        _ => Err(protocol("invalid content field")),
    }
}
