//! Stateless Responses encoding, continuation validation and streaming assembly.
//! Provider-specific replay and reasoning rules stay with this protocol.
//! Requests use `store: false`, except Groq, which omits that unsupported field.
//! OpenAI/OpenRouter/xAI/Zen/Meta request encrypted reasoning; other profiles omit
//! that include flag. Outputs contain at most 128 items and 32 calls. Unknown
//! fields on supported output items survive replay; unsupported item types fail.
//!
//! Protocol references: [Responses](https://developers.openai.com/api/reference/resources/responses),
//! [stream events](https://developers.openai.com/api/reference/resources/responses/streaming-events),
//! and [function calling](https://developers.openai.com/api/docs/guides/function-calling).

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
        CALLS, ExecutionMode, REQUEST_BYTES, ThinkingLevel, byte_limit, check_text_bytes, encode,
        encode_request, exec_cell, execution_function, identifier, protocol, subscription_failure,
        text,
    },
};

const OUTPUT_ITEMS: usize = 128;

pub(crate) struct Responses<'a> {
    pub kind: ProviderKind,
    pub model: &'a str,
    pub session_id: &'a str,
    pub execution: ExecutionMode,
    pub thinking_level: Option<ThinkingLevel>,
    pub reasoning_summary: bool,
    pub registration: Option<&'a str>,
}

pub(crate) fn validate_options(
    kind: ProviderKind,
    model: &str,
    thinking_level: Option<ThinkingLevel>,
) -> Result<(), ProviderError> {
    let model_supported = match kind {
        ProviderKind::OpenCodeZen => matches!(
            model,
            "gpt-6-astra"
                | "gpt-6-sol"
                | "gpt-6.1-sol"
                | "gpt-6-luna"
                | "gpt-5.6-sol"
                | "gpt-5.6-terra"
                | "gpt-5.6-luna"
                | "gpt-5.5"
                | "gpt-5.5-pro"
                | "gpt-5.4"
                | "gpt-5.4-pro"
                | "gpt-5.4-mini"
                | "gpt-5.4-nano"
                | "gpt-5.3-codex"
                | "gpt-5.3-codex-spark"
                | "gpt-5.2"
                | "gpt-5.2-codex"
                | "gpt-5.1"
                | "gpt-5.1-codex"
                | "gpt-5.1-codex-max"
                | "gpt-5.1-codex-mini"
                | "gpt-5"
                | "gpt-5-codex"
                | "gpt-5-nano"
        ),
        ProviderKind::Together => matches!(model, "zai-org/GLM-5.2" | "zai-org/GLM-5.3"),
        ProviderKind::KimiCode => matches!(model, "k3.1-pro" | "k3.1-pro-highspeed"),
        _ => true,
    };
    if !model_supported {
        return Err(ProviderError::InvalidOptions);
    }
    if let Some(level) = thinking_level {
        let supported = match kind {
            ProviderKind::OpenAi
            | ProviderKind::ChatGpt
            | ProviderKind::OpenRouter
            | ProviderKind::OpenCodeGo
            | ProviderKind::XAi
            | ProviderKind::OpenCodeZen
            | ProviderKind::VercelAiGateway
            | ProviderKind::Together
            | ProviderKind::Fireworks
            | ProviderKind::Meta
            | ProviderKind::Groq => true,
            ProviderKind::KimiCode => matches!(
                level,
                ThinkingLevel::Low | ThinkingLevel::High | ThinkingLevel::Max
            ),
            ProviderKind::Baseten => matches!(
                level,
                ThinkingLevel::Minimal
                    | ThinkingLevel::Low
                    | ThinkingLevel::Medium
                    | ThinkingLevel::High
            ),
            _ => false,
        };
        if !supported {
            return Err(ProviderError::UnsupportedThinkingLevel);
        }
    }
    Ok(())
}

impl Responses<'_> {
    pub(crate) fn body(
        &self,
        request: &ModelRequest,
        model_limits: ModelLimits,
    ) -> Result<Vec<u8>, ModelError> {
        let mut input = Vec::new();
        let mut pending = VecDeque::new();
        if let Some(checkpoint) = crate::checkpoint_context(request)? {
            input.push(json!({"role":"user","content":checkpoint}));
        }
        for (index, record) in request.context.records.iter().enumerate() {
            match &record.kind {
                RecordKind::InputAccepted { input: content } if pending.is_empty() => {
                    if content.images().is_empty() {
                        input.push(json!({"role":"user","content":content.text()}));
                    } else {
                        let mut parts = Vec::with_capacity(content.images().len() + 1);
                        if !content.text().is_empty() {
                            parts.push(json!({"type":"input_text","text":content.text()}));
                        }
                        for image in content.images() {
                            parts.push(json!({"type":"input_image","image_url":image.data_url()}));
                        }
                        input.push(json!({"role":"user","content":parts}));
                    }
                }
                RecordKind::ModelCompleted { response } if pending.is_empty() => {
                    let continuation = response
                        .continuation
                        .as_ref()
                        .filter(|data| data.provider == self.kind.continuation_provider())
                        .ok_or_else(|| protocol("incompatible continuation"))?;
                    if let Some(registration) = self.registration
                        && continuation.data["registration"].as_str() != Some(registration)
                    {
                        return Err(protocol("continuation account mismatch"));
                    }
                    let output = continuation.data["output"]
                        .as_array()
                        .ok_or_else(|| protocol("invalid continuation"))?;
                    let action = self.action(output, self.execution, DECODER_BYTES)?;
                    if action != response.action {
                        return Err(protocol("continuation action mismatch"));
                    }
                    if let ModelAction::Execute { cells } = &response.action {
                        let calls = output.iter().filter(|item| item["type"] == "function_call");
                        for (ordinal, (call, cell)) in calls.zip(cells).enumerate() {
                            pending.push_back((
                                record.invocation,
                                ordinal,
                                text(call, "call_id")?,
                                cell.code.as_str(),
                            ));
                        }
                    }
                    input.extend(output.iter().cloned());
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
                    if invocation != record.invocation
                        || *cell as usize != ordinal
                        || code != expected
                    {
                        return Err(protocol("cell feedback mismatch"));
                    }
                    let output = crate::execution_feedback(request, index, outcome)?;
                    input.push(
                        json!({"type":"function_call_output","call_id":call_id,"output":output}),
                    );
                }
                RecordKind::TurnCancelled { runtime } if pending.is_empty() => {
                    input.push(
                        json!({"role":"user","content":crate::cancellation_notice(*runtime)}),
                    );
                }
                RecordKind::InvocationFinished { .. } if pending.is_empty() => {}
                RecordKind::OperationStarted { .. } | RecordKind::OperationFinished { .. } => {}
                _ => return Err(protocol("unsupported history")),
            }
        }
        if !pending.is_empty() {
            return Err(protocol("missing cell feedback"));
        }
        if let Some(shared) = crate::shared_context(request)? {
            input.push(json!({"role":"user","content":shared}));
        }
        self.encode_input(
            input,
            &instructions(request, self.execution)?,
            self.execution,
            model_limits,
        )
    }

    pub(crate) fn encode_input(
        &self,
        input: Vec<Value>,
        instructions: &str,
        execution: ExecutionMode,
        limits: ModelLimits,
    ) -> Result<Vec<u8>, ModelError> {
        let mut body = json!({"model":self.model,"input":input,"stream":true});
        if self.kind != ProviderKind::Groq {
            body["store"] = json!(false);
        }
        if matches!(
            self.kind,
            ProviderKind::OpenAi
                | ProviderKind::ChatGpt
                | ProviderKind::OpenRouter
                | ProviderKind::XAi
                | ProviderKind::OpenCodeZen
                | ProviderKind::Meta
        ) {
            body["include"] = json!(["reasoning.encrypted_content"]);
        }
        if matches!(
            self.kind,
            ProviderKind::OpenAi
                | ProviderKind::ChatGpt
                | ProviderKind::XAi
                | ProviderKind::Fireworks
        ) {
            body["prompt_cache_key"] = json!(self.session_id);
        }
        if self.kind == ProviderKind::VercelAiGateway {
            body["caching"] = json!("auto");
        }
        if self.kind == ProviderKind::OpenRouter {
            body["provider"] = json!({"require_parameters":true});
            body["session_id"] = json!(self.session_id);
            if self.model.trim_start_matches('~').starts_with("anthropic/") {
                body["cache_control"] = json!({"type":"ephemeral"});
            }
        }
        if let Some(level) = self.thinking_level {
            body["reasoning"] = json!({"effort":level});
        }
        if self.reasoning_summary {
            body["reasoning"]["summary"] = json!("auto");
        }
        if !instructions.is_empty() {
            body["instructions"] = json!(instructions);
        }
        if execution == ExecutionMode::Enabled {
            let mut function = execution_function();
            function["type"] = json!("function");
            match self.kind {
                ProviderKind::Together => function["strict"] = json!(false),
                ProviderKind::Fireworks | ProviderKind::KimiCode | ProviderKind::Groq => {}
                _ => function["strict"] = json!(true),
            }
            body["tools"] = if self.kind == ProviderKind::ChatGpt {
                json!([{"type":"namespace","name":"cocoa","description":"Cocoa local JavaScript execution","tools":[function]}])
            } else {
                json!([function])
            };
            body["parallel_tool_calls"] = json!(false);
        }
        if self.kind == ProviderKind::ChatGpt {
            encode(&body, REQUEST_BYTES, "request_body")
        } else {
            encode_request(body, limits, "max_output_tokens")
        }
    }

    fn completed(
        &self,
        response: &Value,
        execution: ExecutionMode,
        limit: usize,
    ) -> Result<ModelResponse, ModelError> {
        if response["status"] != "completed"
            || !response["error"].is_null()
            || !response["incomplete_details"].is_null()
        {
            return Err(protocol("invalid completed status"));
        }
        let output = response["output"]
            .as_array()
            .ok_or_else(|| protocol("missing output"))?;
        let action = self.action(output, execution, limit)?;
        let usage = usage(&response["usage"])?;
        let result = ModelResponse {
            action,
            continuation: Some(ProviderData {
                provider: self.kind.continuation_provider().into(),
                data: if let Some(registration) = self.registration {
                    json!({"output":output,"registration":registration})
                } else {
                    json!({"output":output})
                },
            }),
            usage,
        };
        Ok(result)
    }

    fn action(
        &self,
        output: &[Value],
        execution: ExecutionMode,
        limit: usize,
    ) -> Result<ModelAction, ModelError> {
        if output.len() > OUTPUT_ITEMS {
            return Err(protocol("output item limit"));
        }
        let mut answer = String::new();
        let mut cells = Vec::new();
        let mut item_ids = BTreeSet::new();
        let mut call_ids = BTreeSet::new();
        for item in output {
            if (item["type"] != "function_call" || item.get("id").is_some())
                && !item_ids.insert(identifier(item, "id")?)
            {
                return Err(protocol("duplicate output item"));
            }
            if item["type"] == "reasoning" {
                if item
                    .get("status")
                    .is_some_and(|status| status != "completed")
                {
                    return Err(protocol("invalid reasoning status"));
                }
                continue;
            }
            if item["type"] == "function_call" {
                if execution == ExecutionMode::Disabled {
                    return Err(protocol("execution disabled"));
                }
                if item["name"] != "exec"
                    || item
                        .get("status")
                        .is_some_and(|status| status != "completed")
                    || item.get("namespace").map_or(
                        self.kind == ProviderKind::ChatGpt,
                        |namespace| {
                            namespace
                                != if self.kind == ProviderKind::ChatGpt {
                                    "cocoa"
                                } else {
                                    ""
                                }
                        },
                    )
                    || item.get("async").is_some_and(|value| value != false)
                    || item
                        .get("caller")
                        .is_some_and(|caller| !caller.is_null() && caller["type"] != "direct")
                    || cells.len() >= CALLS
                {
                    return Err(protocol("invalid execution request"));
                }
                if !call_ids.insert(identifier(item, "call_id")?) {
                    return Err(protocol("duplicate call identifier"));
                }
                cells.push(exec_cell(text(item, "arguments")?, limit)?);
                continue;
            }
            if item["type"] != "message"
                || match item.get("status") {
                    Some(status) => status != "completed",
                    None => !matches!(self.kind, ProviderKind::Baseten | ProviderKind::OpenRouter),
                }
                || item["role"] != "assistant"
            {
                return Err(protocol("unsupported output"));
            }
            for part in item["content"]
                .as_array()
                .ok_or_else(|| protocol("invalid message content"))?
            {
                if part["type"] != "output_text" {
                    return Err(protocol("unsupported message content or refusal"));
                }
                let fragment = text(part, "text")?;
                check_text_bytes(answer.len().saturating_add(fragment.len()), limit)?;
                answer.push_str(fragment);
            }
        }
        if !cells.is_empty() {
            return Ok(ModelAction::Execute { cells });
        }
        if answer.is_empty() {
            return Err(protocol("empty answer"));
        }
        Ok(ModelAction::Final { answer })
    }
}

pub(crate) fn usage(value: &Value) -> Result<Option<Usage>, ModelError> {
    if value.is_null() {
        return Ok(None);
    }
    let input_tokens = value["input_tokens"]
        .as_u64()
        .ok_or_else(|| protocol("invalid usage"))?;
    let details = &value["input_tokens_details"];
    if !details.is_null() && !details.is_object() {
        return Err(protocol("invalid input token details"));
    }
    let cache_count = |field: &str| {
        details
            .get(field)
            .filter(|count| !count.is_null())
            .map(|count| {
                count
                    .as_u64()
                    .ok_or_else(|| protocol("invalid cache usage"))
            })
            .transpose()
    };
    let read_tokens = cache_count("cached_tokens")?;
    let write_tokens = cache_count("cache_write_tokens")?;
    let cache_total = read_tokens
        .unwrap_or(0)
        .checked_add(write_tokens.unwrap_or(0))
        .ok_or_else(|| protocol("usage overflow"))?;
    if cache_total > input_tokens {
        return Err(protocol("cache usage exceeds input usage"));
    }
    Ok(Some(Usage {
        input_tokens,
        output_tokens: value["output_tokens"]
            .as_u64()
            .ok_or_else(|| protocol("invalid usage"))?,
        input_cache: if read_tokens.is_some() || write_tokens.is_some() {
            Some(cocoa_core::model::InputCacheUsage {
                read_tokens,
                write_tokens,
            })
        } else {
            None
        },
    }))
}

#[derive(Default)]
pub(crate) struct OutputStream {
    added: BTreeMap<usize, Value>,
    done: BTreeMap<usize, Value>,
    arguments: BTreeMap<usize, ArgumentStream>,
    summaries: BTreeSet<usize>,
    response_id: Option<String>,
    text_bytes: usize,
}

struct ArgumentStream {
    item_id: Option<String>,
    value: String,
    finalized: bool,
}

impl OutputStream {
    pub(crate) fn accept_event(
        &mut self,
        event: &Event,
        profile: &Responses<'_>,
        execution: ExecutionMode,
        output_bytes: usize,
        observations: Option<&ObservationSink>,
    ) -> Result<Option<ModelResponse>, ModelError> {
        let value: Value =
            serde_json::from_str(&event.data).map_err(|_| protocol("invalid event JSON"))?;
        let kind = text(&value, "type")?;
        if event.event != "message" && event.event != kind {
            return Err(protocol("inconsistent event type"));
        }
        self.accept(&value, DECODER_BYTES, profile.kind)?;
        match kind {
            "response.output_item.added" if value["item"]["type"] == "reasoning" => {
                if let Some(observations) = observations {
                    observations.reasoning_started(
                        value["output_index"]
                            .as_u64()
                            .ok_or_else(|| protocol("invalid reasoning index"))?
                            as u32,
                    );
                }
            }
            "response.reasoning_summary_text.delta" => {
                let index = value["output_index"]
                    .as_u64()
                    .ok_or_else(|| protocol("invalid reasoning index"))?
                    as u32;
                if let Some(observations) = observations {
                    observations.reasoning_text(index, text(&value, "delta")?);
                }
            }
            "response.output_item.done" if value["item"]["type"] == "reasoning" => {
                let index = value["output_index"]
                    .as_u64()
                    .ok_or_else(|| protocol("invalid reasoning index"))?
                    as u32;
                if let Some(observations) = observations {
                    if !self.summaries.contains(&(index as usize)) {
                        publish_summary(observations, index, &value["item"]);
                    }
                    observations.reasoning_finished(index);
                }
            }
            "response.output_text.delta" => {
                let fragment = text(&value, "delta")?;
                self.text_bytes = self.text_bytes.saturating_add(fragment.len());
                check_text_bytes(self.text_bytes, output_bytes)?;
                if let Some(observations) = observations {
                    observations.assistant_text(fragment);
                }
            }
            "response.completed" => {
                self.validate(&value["response"])?;
                let result = profile.completed(&value["response"], execution, output_bytes)?;
                if let Some(observations) = observations
                    && let Some(items) = value["response"]["output"].as_array()
                {
                    for (index, item) in items.iter().enumerate() {
                        if item["type"] == "reasoning" && !self.done.contains_key(&index) {
                            if !self.added.contains_key(&index) {
                                observations.reasoning_started(index as u32);
                            }
                            if !self.summaries.contains(&index) {
                                publish_summary(observations, index as u32, item);
                            }
                            observations.reasoning_finished(index as u32);
                        }
                    }
                }
                return Ok(Some(result));
            }
            "response.failed"
            | "response.incomplete"
            | "response.refusal.delta"
            | "response.refusal.done"
            | "error" => {
                if profile.kind == ProviderKind::ChatGpt {
                    return Err(subscription_failure(&value, None));
                }
                return Err(protocol("provider did not complete successfully"));
            }
            _ => {}
        }
        Ok(None)
    }

    fn accept(
        &mut self,
        event: &Value,
        limit: usize,
        provider: ProviderKind,
    ) -> Result<(), ModelError> {
        let kind = text(event, "type")?;
        if matches!(kind, "response.created" | "response.in_progress") {
            let id = identifier(&event["response"], "id")?;
            if self
                .response_id
                .as_ref()
                .is_some_and(|previous| previous != id)
            {
                return Err(protocol("response identifier mismatch"));
            }
            self.response_id = Some(id.into());
        }
        if !matches!(
            kind,
            "response.output_item.added"
                | "response.output_item.done"
                | "response.reasoning_summary_text.delta"
                | "response.function_call_arguments.delta"
                | "response.function_call_arguments.done"
        ) {
            return Ok(());
        }
        let index = event["output_index"]
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .filter(|&value| value < OUTPUT_ITEMS)
            .ok_or_else(|| protocol("invalid output index"))?;
        match kind {
            "response.reasoning_summary_text.delta" => {
                let item = self
                    .added
                    .get(&index)
                    .ok_or_else(|| protocol("summary without reasoning item"))?;
                if item["type"] != "reasoning"
                    || identifier(item, "id")? != identifier(event, "item_id")?
                    || self.done.contains_key(&index)
                    || !event["summary_index"]
                        .as_u64()
                        .is_some_and(|index| index < OUTPUT_ITEMS as u64)
                {
                    return Err(protocol("inconsistent reasoning summary"));
                }
                text(event, "delta")?;
                self.summaries.insert(index);
            }
            "response.output_item.added" => {
                if self.added.contains_key(&index) || self.done.contains_key(&index) {
                    return Err(protocol("duplicate output event"));
                }
                let item = &event["item"];
                let item_id = if provider == ProviderKind::OpenRouter
                    && item["type"] == "function_call"
                    && item.get("id").is_none()
                {
                    None
                } else {
                    Some(identifier(item, "id")?.to_owned())
                };
                text(item, "type")?;
                if item["type"] == "function_call" {
                    let arguments = text(item, "arguments")?;
                    if arguments.len() > limit {
                        return Err(byte_limit(
                            "argument",
                            "output_item_added_arguments",
                            arguments.len(),
                            limit,
                        ));
                    }
                    self.arguments.insert(
                        index,
                        ArgumentStream {
                            item_id,
                            value: arguments.into(),
                            finalized: false,
                        },
                    );
                }
                self.added.insert(index, item.clone());
            }
            "response.output_item.done" => {
                if self.done.contains_key(&index) {
                    return Err(protocol("duplicate output event"));
                }
                let item = &event["item"];
                if item["type"] == "function_call" {
                    let bytes = text(item, "arguments")?.len();
                    if bytes > limit {
                        return Err(byte_limit(
                            "argument",
                            "output_item_done_arguments",
                            bytes,
                            limit,
                        ));
                    }
                }
                self.check_item(index, &event["item"])?;
                if let Some(arguments) = self.arguments.get_mut(&index) {
                    arguments.finalized = true;
                }
                self.done.insert(index, event["item"].clone());
            }
            _ => {
                let arguments = self
                    .arguments
                    .get_mut(&index)
                    .ok_or_else(|| protocol("arguments without call"))?;
                let item_id =
                    if provider == ProviderKind::OpenRouter && event.get("item_id").is_none() {
                        None
                    } else {
                        Some(identifier(event, "item_id")?)
                    };
                if arguments
                    .item_id
                    .as_deref()
                    .zip(item_id)
                    .is_some_and(|(previous, current)| previous != current)
                    || arguments.finalized
                    || self.done.contains_key(&index)
                {
                    return Err(protocol("inconsistent argument event"));
                }
                if let Some(item_id) = item_id {
                    arguments.item_id = Some(item_id.into());
                }
                if kind == "response.function_call_arguments.delta" {
                    let delta = text(event, "delta")?;
                    let bytes = arguments.value.len().saturating_add(delta.len());
                    if bytes > limit {
                        return Err(byte_limit("argument", "argument_delta", bytes, limit));
                    }
                    arguments.value.push_str(delta);
                } else {
                    if arguments.value != text(event, "arguments")? {
                        return Err(protocol("completed arguments mismatch"));
                    }
                    arguments.finalized = true;
                }
            }
        }
        Ok(())
    }

    fn check_item(&self, index: usize, item: &Value) -> Result<(), ModelError> {
        if let Some(added) = self.added.get(&index) {
            if (added.get("id").is_some() && added["id"] != item["id"])
                || added["type"] != item["type"]
            {
                return Err(protocol("output identity mismatch"));
            }
            if added["type"] == "function_call"
                && (added["name"] != item["name"] || added["call_id"] != item["call_id"])
            {
                return Err(protocol("call identity mismatch"));
            }
        }
        if let Some(arguments) = self.arguments.get(&index) {
            if arguments.value != text(item, "arguments")? {
                return Err(protocol("completed arguments mismatch"));
            }
            if let Some(item_id) = &arguments.item_id
                && item_id != identifier(item, "id")?
            {
                return Err(protocol("output identity mismatch"));
            }
        }
        Ok(())
    }

    fn validate(&self, response: &Value) -> Result<(), ModelError> {
        let id = identifier(response, "id")?;
        if self
            .response_id
            .as_ref()
            .is_some_and(|previous| previous != id)
        {
            return Err(protocol("response identifier mismatch"));
        }
        let output = response["output"]
            .as_array()
            .ok_or_else(|| protocol("missing output"))?;
        for &index in self.added.keys().chain(self.done.keys()) {
            let item = output
                .get(index)
                .ok_or_else(|| protocol("missing streamed output item"))?;
            self.check_item(index, item)?;
            if self.done.get(&index).is_some_and(|done| done != item) {
                return Err(protocol("completed item mismatch"));
            }
        }
        Ok(())
    }
}

fn publish_summary(observations: &ObservationSink, index: u32, item: &Value) {
    if let Some(parts) = item["summary"].as_array() {
        for (part_index, part) in parts.iter().enumerate() {
            if part["type"] == "summary_text"
                && let Some(text) = part["text"].as_str()
            {
                if part_index > 0 {
                    observations.reasoning_text(index, "\n");
                }
                observations.reasoning_text(index, text);
            }
        }
    }
}
