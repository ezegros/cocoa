//! Chat Completions mapping for local Ollama and hosted profiles.
//! Require a successful finish and [DONE], including on Ollama where the
//! [Responses converter](https://github.com/ollama/ollama/blob/c1db5f9ef203b2e946161bf4c6bd4f2ec7f123b3/middleware/openai.go#L85-L174) loses length stops.
//! Hosted [reasoning](https://api-docs.deepseek.com/guides/thinking_mode) survives tool and followup replay.

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

pub(crate) fn validate_options(
    kind: ProviderKind,
    model: &str,
    thinking: Option<ThinkingLevel>,
) -> Result<(), ProviderError> {
    if let Some(level) = thinking {
        let supported = match kind {
            ProviderKind::DeepSeek if matches!(model, "deepseek-flash" | "deepseek-pro") => {
                matches!(
                    level,
                    ThinkingLevel::None
                        | ThinkingLevel::Low
                        | ThinkingLevel::High
                        | ThinkingLevel::Max
                )
            }
            ProviderKind::MoonshotAi | ProviderKind::MoonshotAiCn
                if matches!(model, "kimi-k3.1-pro" | "kimi-k3.1-pro-highspeed") =>
            {
                matches!(
                    level,
                    ThinkingLevel::Low | ThinkingLevel::High | ThinkingLevel::Max
                )
            }
            _ => false,
        };
        if !supported {
            return Err(ProviderError::UnsupportedThinkingLevel);
        }
    }
    Ok(())
}

pub(crate) fn body(
    request: &ModelRequest,
    model: &str,
    kind: ProviderKind,
    session_id: &str,
    execution: ExecutionMode,
    thinking: Option<ThinkingLevel>,
    model_limits: ModelLimits,
) -> Result<Vec<u8>, ModelError> {
    let mut messages = Vec::new();
    let instructions = instructions(request, execution)?;
    if !instructions.is_empty() {
        messages.push(json!({"role":"system","content":instructions}));
    }
    let mut pending = VecDeque::new();
    if let Some(checkpoint) = crate::checkpoint_context(request)? {
        messages.push(json!({"role":"user","content":checkpoint}));
    }
    for (index, record) in request.context.records.iter().enumerate() {
        match &record.kind {
            RecordKind::InputAccepted { input } if pending.is_empty() => {
                if !input.images().is_empty()
                    && matches!(
                        kind,
                        ProviderKind::DeepSeek | ProviderKind::Cerebras | ProviderKind::AntLing
                    )
                {
                    return Err(protocol("images unsupported by selected provider"));
                }
                if input.images().is_empty() {
                    messages.push(json!({"role":"user","content":input.text()}));
                } else {
                    let mut parts = Vec::with_capacity(input.images().len() + 1);
                    if !input.text().is_empty() {
                        parts.push(json!({"type":"text","text":input.text()}));
                    }
                    for image in input.images() {
                        parts
                            .push(json!({"type":"image_url","image_url":{"url":image.data_url()}}));
                    }
                    messages.push(json!({"role":"user","content":parts}));
                }
            }
            RecordKind::ModelCompleted {
                response,
                shared_context,
            } if pending.is_empty() => {
                if let Some(shared) = crate::shared_context(shared_context.as_ref())? {
                    messages.push(json!({"role":"user","content":shared}));
                }
                let continuation = response
                    .continuation
                    .as_ref()
                    .filter(|data| data.provider == kind.continuation_provider())
                    .ok_or_else(|| protocol("incompatible continuation"))?;
                let message = &continuation.data["message"];
                validate_message(message, kind, DECODER_BYTES)?;
                if action(message, execution, DECODER_BYTES)? != response.action {
                    return Err(protocol("continuation action mismatch"));
                }
                if let ModelAction::Execute { cells } = &response.action {
                    let calls = message["tool_calls"]
                        .as_array()
                        .ok_or_else(|| protocol("missing calls"))?;
                    for (ordinal, (call, cell)) in calls.iter().zip(cells).enumerate() {
                        pending.push_back((
                            record.invocation,
                            ordinal,
                            identifier(call, "id")?,
                            cell.code.as_str(),
                        ));
                    }
                }
                messages.push(message.clone());
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
                messages.push(json!({"role":"tool","tool_call_id":call_id,"content":content}));
            }
            RecordKind::TurnCancelled { runtime } if pending.is_empty() => {
                messages
                    .push(json!({"role":"user","content":crate::cancellation_notice(*runtime)}));
            }
            RecordKind::InvocationFinished { .. } if pending.is_empty() => {}
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
        model,
        kind,
        session_id,
        execution,
        thinking,
        model_limits,
    )
}

pub(crate) fn encode_messages(
    messages: Vec<Value>,
    model: &str,
    kind: ProviderKind,
    session_id: &str,
    execution: ExecutionMode,
    thinking: Option<ThinkingLevel>,
    model_limits: ModelLimits,
) -> Result<Vec<u8>, ModelError> {
    let mut body = json!({"model":model,"messages":messages,"stream":true,"stream_options":{"include_usage":true}});
    if matches!(
        kind,
        ProviderKind::Cerebras | ProviderKind::MoonshotAi | ProviderKind::MoonshotAiCn
    ) {
        body["prompt_cache_key"] = json!(session_id);
    }
    if let Some(level) = thinking {
        body["reasoning_effort"] = json!(level);
    }
    if execution == ExecutionMode::Enabled {
        body["tools"] = json!([{"type":"function","function":execution_function()}]);
    }
    encode_request(body, model_limits, "max_tokens")
}

pub(crate) struct ChatStream {
    kind: ProviderKind,
    id: Option<String>,
    content: String,
    reasoning: String,
    reasoning_content: Option<String>,
    reasoning_details: BTreeMap<usize, Value>,
    reasoning_details_bytes: usize,
    calls: BTreeMap<usize, Value>,
    finished: bool,
    usage: Option<Usage>,
    reasoning_active: bool,
    text_bytes: usize,
}

impl ChatStream {
    pub(crate) fn new(kind: ProviderKind) -> Self {
        Self {
            kind,
            id: None,
            content: String::new(),
            reasoning: String::new(),
            reasoning_content: None,
            reasoning_details: BTreeMap::new(),
            reasoning_details_bytes: 0,
            calls: BTreeMap::new(),
            finished: false,
            usage: None,
            reasoning_active: false,
            text_bytes: 0,
        }
    }

    pub(crate) fn accept_event(
        &mut self,
        event: &Event,
        execution: ExecutionMode,
        output_bytes: usize,
        observations: Option<&ObservationSink>,
    ) -> Result<Option<ModelResponse>, ModelError> {
        if event.event != "message" {
            return Err(protocol("invalid chat event type"));
        }
        if event.data == "[DONE]" {
            if self.reasoning_active
                && let Some(observations) = observations
            {
                observations.reasoning_finished(0);
            }
            return std::mem::replace(self, Self::new(self.kind))
                .completed(execution, DECODER_BYTES)
                .map(Some);
        }
        let value: Value =
            serde_json::from_str(&event.data).map_err(|_| protocol("invalid event JSON"))?;
        let delta = &value["choices"][0]["delta"];
        if let Some(fragment) = delta["content"].as_str() {
            self.text_bytes = self.text_bytes.saturating_add(fragment.len());
            check_text_bytes(self.text_bytes, output_bytes)?;
        }
        self.accept(&value, DECODER_BYTES)?;
        if let Some(observations) = observations {
            if let Some(reasoning) = delta["reasoning"]
                .as_str()
                .or_else(|| delta["reasoning_content"].as_str())
                .filter(|text| !text.is_empty())
            {
                if !self.reasoning_active {
                    observations.reasoning_started(0);
                }
                self.reasoning_active = true;
                observations.reasoning_text(0, reasoning);
            }
            if self.reasoning_active
                && (delta["content"]
                    .as_str()
                    .is_some_and(|text| !text.is_empty())
                    || delta["tool_calls"].is_array()
                    || value["choices"][0]["finish_reason"].is_string())
            {
                observations.reasoning_finished(0);
                self.reasoning_active = false;
            }
            if let Some(delta) = delta["content"].as_str() {
                observations.assistant_text(delta);
            }
        }
        Ok(None)
    }

    fn accept(&mut self, value: &Value, limit: usize) -> Result<(), ModelError> {
        if !value["error"].is_null() {
            return Err(protocol("provider did not complete successfully"));
        }
        if value.get("id").is_some() {
            let id = identifier(value, "id")?;
            if self.id.as_deref().is_some_and(|previous| previous != id) {
                return Err(protocol("response identifier mismatch"));
            }
            self.id = Some(id.into());
        }
        let choices = value["choices"]
            .as_array()
            .ok_or_else(|| protocol("missing choices"))?;
        if choices.is_empty() {
            if !self.finished || self.usage.is_some() {
                return Err(protocol("unexpected usage event"));
            }
            self.usage = Some(usage(&value["usage"])?);
            return Ok(());
        }
        if self.finished
            || choices.len() != 1
            || choices[0]["index"] != 0
            || (self.kind == ProviderKind::Ollama && !value["usage"].is_null())
            || self.id.is_none()
        {
            return Err(protocol("unexpected chat choice"));
        }
        let choice = &choices[0];
        let delta = choice["delta"]
            .as_object()
            .ok_or_else(|| protocol("invalid chat delta"))?;
        let finish = choice
            .get("finish_reason")
            .filter(|reason| !reason.is_null());
        if self.kind == ProviderKind::Ollama && finish.is_some() && !delta.is_empty() {
            return Err(protocol("invalid finish event"));
        }
        if delta.keys().any(|key| {
            !matches!(
                key.as_str(),
                "role"
                    | "content"
                    | "reasoning"
                    | "tool_calls"
                    | "reasoning_content"
                    | "reasoning_details"
                    | "refusal"
            )
        }) || delta.get("role").is_some_and(|role| role != "assistant")
            || !delta.get("refusal").is_none_or(Value::is_null)
            || (self.kind == ProviderKind::Ollama
                && delta.keys().any(|key| {
                    matches!(
                        key.as_str(),
                        "reasoning_content" | "reasoning_details" | "refusal"
                    )
                }))
        {
            return Err(protocol("unsupported chat delta"));
        }
        for (field, target, stage) in [
            ("content", &mut self.content, "chat_content"),
            ("reasoning", &mut self.reasoning, "chat_reasoning"),
        ] {
            if let Some(value) = delta.get(field).filter(|value| !value.is_null()) {
                append(
                    target,
                    value
                        .as_str()
                        .ok_or_else(|| protocol("invalid chat text"))?,
                    limit,
                    stage,
                )?;
            }
        }
        if let Some(reasoning) = delta
            .get("reasoning_content")
            .filter(|value| !value.is_null())
        {
            append(
                self.reasoning_content.get_or_insert_with(String::new),
                reasoning
                    .as_str()
                    .ok_or_else(|| protocol("invalid chat reasoning"))?,
                limit,
                "chat_reasoning_content",
            )?;
        }
        if let Some(details) = delta
            .get("reasoning_details")
            .filter(|value| !value.is_null())
        {
            let details = details
                .as_array()
                .ok_or_else(|| protocol("invalid reasoning details"))?;
            if details.len() > CALLS {
                return Err(protocol("reasoning group limit"));
            }
            for detail in details {
                let bytes = serde_json::to_vec(detail)
                    .map_err(|_| protocol("invalid reasoning details"))?
                    .len();
                self.reasoning_details_bytes = self.reasoning_details_bytes.saturating_add(bytes);
                if self.reasoning_details_bytes > limit {
                    return Err(byte_limit(
                        "chat",
                        "chat_reasoning_details",
                        self.reasoning_details_bytes,
                        limit,
                    ));
                }
                merge_detail(&mut self.reasoning_details, detail, limit)?;
            }
        }
        if let Some(calls) = delta.get("tool_calls").filter(|value| !value.is_null()) {
            let calls = calls.as_array().ok_or_else(|| protocol("invalid calls"))?;
            if calls.len() > CALLS {
                return Err(protocol("call group limit"));
            }
            for call in calls {
                let fields = call.as_object().ok_or_else(|| protocol("invalid call"))?;
                if fields
                    .keys()
                    .any(|key| !matches!(key.as_str(), "index" | "id" | "type" | "function"))
                {
                    return Err(protocol("unsupported call field"));
                }
                let index = call["index"]
                    .as_u64()
                    .and_then(|value| usize::try_from(value).ok())
                    .filter(|&index| index < CALLS)
                    .ok_or_else(|| protocol("call index limit"))?;
                let current = self.calls.entry(index).or_insert_with(
                    || json!({"type":"function","function":{"name":"","arguments":""}}),
                );
                if call.get("id").is_some() {
                    let id = identifier(call, "id")?;
                    if current.get("id").is_some_and(|previous| previous != id) {
                        return Err(protocol("call identity mismatch"));
                    }
                    current["id"] = json!(id);
                }
                if call.get("type").is_some_and(|kind| kind != "function") {
                    return Err(protocol("unsupported call type"));
                }
                let function = call["function"]
                    .as_object()
                    .ok_or_else(|| protocol("invalid function"))?;
                if function
                    .keys()
                    .any(|key| !matches!(key.as_str(), "name" | "arguments"))
                {
                    return Err(protocol("unsupported function field"));
                }
                for (field, bound, stage) in [
                    ("name", 256, "chat_function_name"),
                    ("arguments", limit, "chat_arguments"),
                ] {
                    if let Some(value) = function.get(field) {
                        let Value::String(accumulated) = &mut current["function"][field] else {
                            return Err(protocol("invalid function"));
                        };
                        append(
                            accumulated,
                            value.as_str().ok_or_else(|| protocol("invalid function"))?,
                            bound,
                            stage,
                        )?;
                    }
                }
            }
        }
        if let Some(reason) = finish {
            match reason.as_str() {
                Some("stop") if self.calls.is_empty() => {}
                Some("tool_calls") if !self.calls.is_empty() => {}
                _ => return Err(protocol("unsuccessful chat finish")),
            }
            self.finished = true;
        }
        if self.kind != ProviderKind::Ollama && !value["usage"].is_null() {
            if !self.finished || self.usage.is_some() {
                return Err(protocol("unexpected usage event"));
            }
            self.usage = Some(usage(&value["usage"])?);
        }
        Ok(())
    }

    pub(crate) fn completed(
        self,
        execution: ExecutionMode,
        limit: usize,
    ) -> Result<ModelResponse, ModelError> {
        if !self.finished {
            return Err(protocol("missing successful finish"));
        }
        let mut message = json!({"role":"assistant","content":self.content});
        if !self.reasoning.is_empty() {
            message["reasoning"] = json!(self.reasoning);
        }
        if let Some(reasoning) = self.reasoning_content {
            message["reasoning_content"] = json!(reasoning);
        }
        if !self.reasoning_details.is_empty() {
            message["reasoning_details"] =
                json!(self.reasoning_details.into_values().collect::<Vec<_>>());
        }
        validate_message(&message, self.kind, limit)?;
        if !self.calls.is_empty() {
            if self.calls.keys().copied().ne(0..self.calls.len()) {
                return Err(protocol("noncontiguous call indices"));
            }
            message["tool_calls"] = json!(self.calls.into_values().collect::<Vec<_>>());
        }
        let result = ModelResponse {
            action: action(&message, execution, limit)?,
            continuation: Some(ProviderData {
                provider: self.kind.continuation_provider().into(),
                data: json!({"message":message}),
            }),
            usage: self.usage,
        };
        Ok(result)
    }
}

fn action(
    message: &Value,
    execution: ExecutionMode,
    limit: usize,
) -> Result<ModelAction, ModelError> {
    if message["role"] != "assistant" || !message["refusal"].is_null() {
        return Err(protocol("unsupported assistant message"));
    }
    let content = text(message, "content")?;
    if content.len() > limit {
        return Err(byte_limit("chat", "chat_content", content.len(), limit));
    }
    if let Some(calls) = message.get("tool_calls") {
        let calls = calls.as_array().ok_or_else(|| protocol("invalid calls"))?;
        if execution == ExecutionMode::Disabled || calls.is_empty() || calls.len() > CALLS {
            return Err(protocol("invalid execution request"));
        }
        let mut cells = Vec::with_capacity(calls.len());
        let mut ids = BTreeSet::new();
        for call in calls {
            if call.as_object().is_none_or(|fields| {
                fields
                    .keys()
                    .any(|key| !matches!(key.as_str(), "id" | "type" | "function"))
            }) || call["function"].as_object().is_none_or(|fields| {
                fields
                    .keys()
                    .any(|key| !matches!(key.as_str(), "name" | "arguments"))
            }) {
                return Err(protocol("unsupported call field"));
            }
            if !ids.insert(identifier(call, "id")?)
                || call["type"] != "function"
                || call["function"]["name"] != "exec"
            {
                return Err(protocol("invalid execution request"));
            }
            cells.push(exec_cell(text(&call["function"], "arguments")?, limit)?);
        }
        return Ok(ModelAction::Execute { cells });
    }
    if content.is_empty() {
        return Err(protocol("empty answer"));
    }
    Ok(ModelAction::Final {
        answer: content.into(),
    })
}

fn append(
    target: &mut String,
    value: &str,
    limit: usize,
    stage: &'static str,
) -> Result<(), ModelError> {
    let bytes = target.len().saturating_add(value.len());
    if bytes > limit {
        return Err(byte_limit("chat", stage, bytes, limit));
    }
    target.push_str(value);
    Ok(())
}

pub(crate) fn usage(value: &Value) -> Result<Usage, ModelError> {
    let input_tokens = value["prompt_tokens"]
        .as_u64()
        .ok_or_else(|| protocol("invalid usage"))?;
    let details = &value["prompt_tokens_details"];
    if !details.is_null() && !details.is_object() {
        return Err(protocol("invalid input token details"));
    }
    let read_tokens = details
        .get("cached_tokens")
        .filter(|count| !count.is_null())
        .map(|count| {
            count
                .as_u64()
                .ok_or_else(|| protocol("invalid cache usage"))
        })
        .transpose()?;
    if read_tokens.is_some_and(|count| count > input_tokens) {
        return Err(protocol("cache usage exceeds input usage"));
    }
    Ok(Usage {
        input_tokens,
        output_tokens: value["completion_tokens"]
            .as_u64()
            .ok_or_else(|| protocol("invalid usage"))?,
        input_cache: read_tokens.map(|read_tokens| cocoa_core::model::InputCacheUsage {
            read_tokens: Some(read_tokens),
            write_tokens: None,
        }),
    })
}

fn validate_message(message: &Value, kind: ProviderKind, limit: usize) -> Result<(), ModelError> {
    let fields = message
        .as_object()
        .ok_or_else(|| protocol("invalid assistant message"))?;
    if fields.keys().any(|key| {
        !matches!(
            key.as_str(),
            "role"
                | "content"
                | "reasoning"
                | "reasoning_content"
                | "reasoning_details"
                | "tool_calls"
        )
    }) || (kind == ProviderKind::Ollama
        && (fields.contains_key("reasoning_content") || fields.contains_key("reasoning_details")))
    {
        return Err(protocol("unsupported assistant message"));
    }
    for field in ["reasoning", "reasoning_content"] {
        if let Some(value) = fields.get(field) {
            let value = value
                .as_str()
                .ok_or_else(|| protocol("invalid reasoning"))?;
            if value.len() > limit {
                return Err(byte_limit("chat", "chat_reasoning", value.len(), limit));
            }
        }
    }
    if let Some(details) = fields.get("reasoning_details") {
        let bytes = serde_json::to_vec(details)
            .map_err(|_| protocol("invalid reasoning details"))?
            .len();
        if bytes > limit {
            return Err(byte_limit("chat", "chat_reasoning_details", bytes, limit));
        }
        let mut checked = BTreeMap::new();
        for detail in details
            .as_array()
            .ok_or_else(|| protocol("invalid reasoning details"))?
        {
            merge_detail(&mut checked, detail, limit)?;
        }
    }
    Ok(())
}

fn merge_detail(
    details: &mut BTreeMap<usize, Value>,
    detail: &Value,
    limit: usize,
) -> Result<(), ModelError> {
    let fields = detail
        .as_object()
        .ok_or_else(|| protocol("invalid reasoning detail"))?;
    let index = detail["index"]
        .as_u64()
        .and_then(|index| usize::try_from(index).ok())
        .filter(|&index| index < CALLS)
        .ok_or_else(|| protocol("reasoning index limit"))?;
    let current = details.entry(index).or_insert_with(|| json!({}));
    for (key, value) in fields {
        if matches!(key.as_str(), "text" | "summary" | "data" | "signature") {
            let fragment = value
                .as_str()
                .ok_or_else(|| protocol("invalid reasoning detail text"))?;
            if current.get(key).is_none() {
                current[key] = json!("");
            }
            let Value::String(target) = &mut current[key] else {
                return Err(protocol("invalid reasoning detail text"));
            };
            append(target, fragment, limit, "chat_reasoning_details")?;
        } else if current.get(key).is_some_and(|previous| previous != value) {
            return Err(protocol("reasoning identity mismatch"));
        } else {
            current[key] = value.clone();
        }
    }
    Ok(())
}
