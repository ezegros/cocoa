//! Shared bounded HTTP/SSE transport for all provider protocols.
//!
//! Connections never use environment/system proxies, redirects or transparent
//! retries. HTTPS is required except HTTP to literal loopback IPs, including local
//! Ollama. Protocol adapters own request encoding, replay and stream completion.
//! No remote error bodies, transport diagnostics or credentials enter errors.
//! HTTP 429 and transient 500/502/503/504 rejections allow three retries.
//! Bounded `Retry-After` seconds or HTTP dates override exponential backoff
//! with jitter. Hints beyond five minutes prevent retries rather than being
//! shortened. Retry waits report progress and remain cancellable; explicit
//! quota failures, transport failures and partial streams are never replayed.
//!
//! Each provider admits at most eight concurrent requests without queuing.
//! Requests are limited to 64 MiB encoded JSON. One 64 MiB incoming-body budget
//! bounds framing, JSON events and streamed arguments before decoding. Completed
//! responses and provider continuation have no separate encoded-byte quota.
//! Generation uses supplied model output metadata or a 32,000-token fallback.
//! Only nonblank `exec` code is accepted, with at most 32 calls per response.
//! Byte-limit errors name the stage and effective bound. Observed byte counts
//! are lower bounds through rejection, not sizes of the entire remote response.
//! These are encoded byte limits, not exact process heap limits. HTTP/TLS buffers
//! belong to reqwest; parsing and JSON mapping require additional bounded copies.
//! Connection setup is limited to 30 seconds. The caller's initial work window
//! bounds response-header waiting and gaps between nonempty body chunks, not
//! the duration of a progressing response. Bounded synchronous JSON/framing
//! work cannot be preempted by async cancellation or timers.
//! Checkpoint generation instead uses an absolute deadline across retries,
//! headers and body progress, a separate token cap and a UTF-8 text-byte cap.
//! It shares transport admission and decoders, but has no execution or live output.
//! Context-aware OpenAI summaries retain the ordinary prefix with tool choice none;
//! Anthropic reuses tools/system only. Oversized forks fall back to incremental
//! input. These routes retain the same decoder-level tool-call rejection.
//! ChatGPT subscription requests omit unsupported output-token caps.
//! Cancellation drops the HTTP future, not proof of remote generation stopping.
//! Stalls return `ModelError::Stalled`; the engine classifies them as deadlines.

use std::{
    io::Write,
    sync::Arc,
    time::{Duration, SystemTime},
};

use async_trait::async_trait;
use cocoa_core::{
    engine::ObservationSink,
    execution::Cell,
    model::{
        ModelAction, ModelError, ModelProvider, ModelRequest, ModelResponse, RequestAccounting,
        RequestPurpose, RequestSettings, RequestTokenBudget, SummaryRequest,
    },
};
use eventsource_stream::{EventStreamError, Eventsource};
use futures_util::StreamExt;
use reqwest::{
    Client, Url,
    header::{AUTHORIZATION, HeaderValue, RETRY_AFTER},
};
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::{
    CredentialResolver, DECODER_BYTES, ModelLimits, ProviderError, ProviderKind, ProviderOptions,
    anthropic, chat_completions, responses,
};

pub(crate) const REQUEST_BYTES: usize = DECODER_BYTES;
pub(crate) const CALLS: usize = 32;
const REQUEST_RETRIES: u32 = 3;
const RETRY_DELAY_MAX: Duration = Duration::from_secs(300);

/// Controls whether the model may submit JavaScript cells through `exec`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutionMode {
    /// No tools are advertised; execution requests are protocol errors.
    Disabled,
    /// Advertise only `exec({code})`. Capabilities remain JavaScript bindings.
    Enabled,
}

/// Thinking level, serialized using the provider's wire spelling.
///
/// Supported levels depend on the selected model. These values follow the
/// [reasoning guide](https://developers.openai.com/api/docs/guides/reasoning).
/// Responses profiles defer model compatibility to the server except Baseten
/// and Kimi Code. Chat Completions and Anthropic validate their profile-specific
/// levels locally. `Max` is sent as `max`, not `xhigh`. Ollama rejects explicit
/// levels. Omitting a level preserves the model default.
/// Invalid parameter rejections are never retried.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    None,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

/// A reusable bounded client shared by all provider protocols.
/// Construction performs no network requests.
pub struct Transport {
    client: Client,
    credentials: Option<Arc<dyn CredentialResolver>>,
    kind: ProviderKind,
    endpoint: Url,
    model: String,
    session_id: String,
    execution: ExecutionMode,
    thinking_level: Option<ThinkingLevel>,
    reasoning_summary: bool,
    requests: Semaphore,
    pub(crate) model_limits: ModelLimits,
}

impl Transport {
    /// Opt in to OpenAI's model-dependent `reasoning.summary: auto` output.
    /// Raw private reasoning and encrypted replay are never display text.
    ///
    /// # Errors
    /// Reject other provider profiles. Model support is validated by the server,
    /// without retrying invalid parameter rejections.
    pub fn enable_reasoning_summary(&mut self) -> Result<(), ProviderError> {
        if self.kind != ProviderKind::OpenAi {
            return Err(ProviderError::UnsupportedReasoningSummary);
        }
        self.reasoning_summary = true;
        Ok(())
    }
}

#[async_trait]
impl ModelProvider for Transport {
    fn request_bytes(&self, request: &ModelRequest) -> Result<Option<usize>, ModelError> {
        // A fitted cap never has more digits than the unfitted ceiling, so this
        // bounds the sent body without estimating tokens.
        self.body(request, self.model_limits)
            .map(|body| Some(body.len()))
    }

    fn request_token_budget(
        &self,
        request: &ModelRequest,
    ) -> Result<Option<RequestTokenBudget>, ModelError> {
        crate::tokens::budget(request, self.execution, self.model_limits)
            .map(|(budget, _)| Some(budget))
    }

    async fn summarize(
        &self,
        request: SummaryRequest,
        cancellation: CancellationToken,
    ) -> Result<String, ModelError> {
        self.generate_summary(request, None, cancellation, RequestAccounting::default())
            .await
    }

    async fn summarize_with_context(
        &self,
        request: SummaryRequest,
        context: &ModelRequest,
        input_bytes: usize,
        cancellation: CancellationToken,
    ) -> Result<String, ModelError> {
        self.generate_summary(
            request,
            Some((context, input_bytes)),
            cancellation,
            RequestAccounting::default(),
        )
        .await
    }

    async fn summarize_accounted(
        &self,
        request: SummaryRequest,
        context: &ModelRequest,
        input_bytes: usize,
        cancellation: CancellationToken,
        accounting: RequestAccounting,
    ) -> Result<String, ModelError> {
        self.generate_summary(
            request,
            Some((context, input_bytes)),
            cancellation,
            accounting,
        )
        .await
    }

    async fn respond(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        self.respond_accounted(request, cancellation, RequestAccounting::default())
            .await
    }

    async fn respond_accounted(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
        accounting: RequestAccounting,
    ) -> Result<ModelResponse, ModelError> {
        let deadline = request.limits.deadline;
        check_work(deadline, &cancellation)?;
        let timeout = deadline.saturating_duration_since(tokio::time::Instant::now());
        let (progress, activity) = tokio::sync::watch::channel(tokio::time::Instant::now());
        let stalled = async {
            loop {
                let deadline = *activity.borrow() + timeout;
                tokio::time::sleep_until(deadline).await;
                if tokio::time::Instant::now() >= *activity.borrow() + timeout {
                    break;
                }
            }
        };
        tokio::select! {
            biased;
            () = cancellation.cancelled() => Err(ModelError::Cancelled),
            () = stalled => Err(ModelError::Stalled),
            result = async {
                let (budget, estimated) = crate::tokens::budget(&request, self.execution, self.model_limits)?;
                let limits = self.model_limits.fit_output(budget.input_tokens)?;
                let body = self.body(&request, limits)?;
                let checkpoint = crate::tokens::checkpoint_through(&request);
                let outgoing = EncodedRequest {
                    body,
                    execution: self.execution,
                    observations: Some(request.observations),
                    output_bytes: DECODER_BYTES,
                    accounting,
                    purpose: RequestPurpose::Response,
                };
                check_work(deadline, &cancellation)?;
                let mut response = self.request(outgoing, &cancellation, progress, timeout).await?;
                crate::tokens::anchor(&mut response, checkpoint, estimated);
                Ok(response)
            } => result,
        }
    }
}

struct EncodedRequest {
    body: Vec<u8>,
    execution: ExecutionMode,
    observations: Option<ObservationSink>,
    output_bytes: usize,
    accounting: RequestAccounting,
    purpose: RequestPurpose,
}

impl Transport {
    pub(crate) fn new_profile(
        options: ProviderOptions,
        credentials: Option<Arc<dyn CredentialResolver>>,
        transport: reqwest::ClientBuilder,
    ) -> Result<Self, ProviderError> {
        if options.kind == ProviderKind::ChatGpt {
            if options.endpoint != ProviderKind::ChatGpt.endpoint_default()
                || options.api_key.is_some()
                || credentials.as_ref().is_none_or(|source| {
                    source.identity().is_empty() || source.identity().len() > 256
                })
            {
                return Err(ProviderError::InvalidOptions);
            }
        } else if credentials.is_some() {
            return Err(ProviderError::InvalidOptions);
        }
        if options.endpoint.len() > 2048 {
            return Err(ProviderError::InvalidOptions);
        }
        let endpoint = Url::parse(&options.endpoint).map_err(|_| ProviderError::InvalidOptions)?;
        let loopback = endpoint.host_str().is_some_and(|host| {
            host.trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback())
        });
        if !(endpoint.scheme() == "https" || endpoint.scheme() == "http" && loopback)
            || endpoint.host_str().is_none()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
            || options.model.trim().is_empty()
            || options.model.len() > 256
            || options.model.chars().any(char::is_control)
            || options.session_id.is_empty()
            || options.session_id.len() > 128
            || options
                .session_id
                .bytes()
                .any(|byte| !byte.is_ascii_alphanumeric() && !b"-_.".contains(&byte))
            || (!matches!(options.kind, ProviderKind::Ollama | ProviderKind::ChatGpt)
                && options.api_key.is_none())
            || (options.kind == ProviderKind::Ollama && options.api_key.is_some())
        {
            return Err(ProviderError::InvalidOptions);
        }
        if options.kind.is_chat_completions() {
            chat_completions::validate_options(
                options.kind,
                &options.model,
                options.thinking_level,
            )?;
        } else if options.kind == ProviderKind::Anthropic {
            anthropic::validate_options(&options.model, options.thinking_level)?;
        } else {
            responses::validate_options(options.kind, &options.model, options.thinking_level)?;
        }
        let mut headers = reqwest::header::HeaderMap::new();
        if let Some(key) = options.api_key {
            let key = key.expose_secret();
            if key.is_empty()
                || key.len() > 8192
                || key.bytes().any(|byte| !byte.is_ascii_graphic())
            {
                return Err(ProviderError::InvalidOptions);
            }
            if options.kind == ProviderKind::Anthropic {
                let mut authorization =
                    HeaderValue::from_str(key).map_err(|_| ProviderError::InvalidOptions)?;
                authorization.set_sensitive(true);
                headers.insert("x-api-key", authorization);
                headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
            } else {
                let mut authorization = HeaderValue::from_str(&format!("Bearer {key}"))
                    .map_err(|_| ProviderError::InvalidOptions)?;
                authorization.set_sensitive(true);
                headers.insert(AUTHORIZATION, authorization);
            }
        }
        if options.kind == ProviderKind::OpenCodeGo {
            let session = HeaderValue::from_str(&options.session_id)
                .map_err(|_| ProviderError::InvalidOptions)?;
            headers.insert("x-opencode-session", session);
        }
        if options.kind == ProviderKind::ChatGpt {
            let session = HeaderValue::from_str(&options.session_id)
                .map_err(|_| ProviderError::InvalidOptions)?;
            headers.insert("session-id", session);
        }
        if matches!(
            options.kind,
            ProviderKind::Baseten | ProviderKind::VercelAiGateway
        ) {
            let session = HeaderValue::from_str(&options.session_id)
                .map_err(|_| ProviderError::InvalidOptions)?;
            headers.insert("x-session-affinity", session);
        }
        if matches!(
            options.kind,
            ProviderKind::OpenCodeGo | ProviderKind::KimiCode
        ) {
            headers.insert(
                reqwest::header::USER_AGENT,
                HeaderValue::from_static(concat!("cocoa/", env!("CARGO_PKG_VERSION"))),
            );
        }
        let client = transport
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .connect_timeout(Duration::from_secs(30))
            .pool_max_idle_per_host(2)
            .build()
            .map_err(|_| ProviderError::Client)?;
        Ok(Self {
            client,
            credentials,
            kind: options.kind,
            endpoint,
            model: options.model,
            session_id: options.session_id,
            execution: options.execution,
            thinking_level: options.thinking_level,
            reasoning_summary: false,
            requests: Semaphore::new(8),
            model_limits: ModelLimits::default(),
        })
    }

    async fn request(
        &self,
        request: EncodedRequest,
        cancellation: &CancellationToken,
        progress: tokio::sync::watch::Sender<tokio::time::Instant>,
        timeout: Duration,
    ) -> Result<ModelResponse, ModelError> {
        let _permit = self
            .requests
            .try_acquire()
            .map_err(|_| transport("concurrent request limit"))?;
        let body: Value = serde_json::from_slice(&request.body)
            .map_err(|_| protocol("invalid encoded request"))?;
        let settings = RequestSettings {
            provider: self.kind.to_string(),
            model: self.model.clone(),
            endpoint_default: Some(self.endpoint.as_str() == self.kind.endpoint_default()),
            thinking_level: self
                .thinking_level
                .map(|level| format!("{level:?}").to_lowercase()),
            output_tokens: body["max_output_tokens"]
                .as_u64()
                .or_else(|| body["max_tokens"].as_u64())
                .or_else(|| body["max_completion_tokens"].as_u64())
                .and_then(|tokens| u32::try_from(tokens).ok()),
            reasoning_summary: self.reasoning_summary,
        };
        drop(body);
        let mut retries = 0;
        let (response, mut attempt) = loop {
            let mut outgoing = self
                .client
                .post(self.endpoint.clone())
                .header("content-type", "application/json")
                .header("accept", "text/event-stream")
                .body(request.body.clone());
            if let Some(source) = &self.credentials {
                let token = source.resolve().await?;
                let token = token.expose_secret();
                if token.is_empty()
                    || token.len() > 16384
                    || !token.bytes().all(|byte| byte.is_ascii_graphic())
                {
                    return Err(transport("invalid OAuth credential"));
                }
                let mut authorization = HeaderValue::from_str(&format!("Bearer {token}"))
                    .map_err(|_| transport("invalid OAuth credential"))?;
                authorization.set_sensitive(true);
                outgoing = outgoing.header(AUTHORIZATION, authorization);
            }
            let mut attempt = request
                .accounting
                .start(request.purpose, Some(&settings))
                .await?;
            let result = outgoing
                .send()
                .await
                .map_err(|_| transport("request failed"));
            let response = match result {
                Ok(response) => response,
                Err(error) => {
                    let result: Result<ModelResponse, _> = Err(error);
                    attempt.finish(&result).await?;
                    return result;
                }
            };
            if response.status().is_success() {
                break (response, attempt);
            }
            let status = response.status().as_u16();
            let delay = retry_delay(&response, retries);
            let message = rejection_message(&response);
            let rejected: Result<ModelResponse, _> = Err(ModelError::Rejected { message });
            attempt.finish(&rejected).await?;
            let Some(delay) = delay.filter(|_| retries < REQUEST_RETRIES) else {
                return rejected;
            };
            let failure = rejection_body(response).await;
            let retryable = failure.as_ref().is_some_and(|failure| {
                ![
                    "/response/error/code",
                    "/error/code",
                    "/error/type",
                    "/code",
                ]
                .iter()
                .any(|path| {
                    matches!(
                        failure.pointer(path).and_then(Value::as_str),
                        Some(
                            "insufficient_quota"
                                | "quota_exceeded"
                                | "usage_limit_reached"
                                | "billing_hard_limit_reached"
                                | "subscription_sharing_usage_limit_exceeded"
                                | "subscription_sharing_user_not_eligible"
                                | "invalid_grant"
                                | "insufficient_scope"
                                | "invalid_token"
                                | "invalid_api_key"
                                | "invalid_request_error"
                        )
                    )
                })
            });
            if !retryable {
                return rejected;
            }
            retries += 1;
            let retry_at = tokio::time::Instant::now() + delay;
            if let Some(observations) = &request.observations {
                observations.provider_retry_scheduled(status, retry_at.into_std());
            }
            let heartbeat_period =
                (timeout.min(Duration::from_secs(1)) / 2).max(Duration::from_millis(1));
            let mut heartbeat = tokio::time::interval(heartbeat_period);
            heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    () = tokio::time::sleep_until(retry_at) => break,
                    _ = heartbeat.tick() => {
                        progress.send_replace(tokio::time::Instant::now());
                        if let Some(observations) = &request.observations {
                            observations.progress();
                        }
                    }
                }
            }
            progress.send_replace(tokio::time::Instant::now());
            if let Some(observations) = &request.observations {
                observations.provider_retry_started();
            }
        };
        let result = async {
            if response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok())
                .map_or(self.kind != ProviderKind::ChatGpt, |value| {
                    !value
                        .split(';')
                        .next()
                        .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/event-stream"))
                })
            {
                return Err(protocol("expected event stream"));
            }
            let mut bounds = WireBounds::default();
            let observations = request.observations.clone();
            let stream = response
                .bytes_stream()
                .map(move |chunk| {
                    let chunk = chunk.map_err(|_| transport("response read failed"))?;
                    if !chunk.is_empty() {
                        progress.send_replace(tokio::time::Instant::now());
                        if let Some(observations) = &observations {
                            observations.progress();
                        }
                    }
                    bounds.accept(&chunk)
                })
                .eventsource();
            tokio::pin!(stream);
            let mut output = responses::OutputStream::default();
            let mut chat = chat_completions::ChatStream::new(self.kind);
            let mut messages = anthropic::MessageStream::default();
            let mut message_usage = json!({});
            let mut trailing_usage_error = None;
            while let Some(event) = stream.next().await {
                if cancellation.is_cancelled() {
                    return Err(ModelError::Cancelled);
                }
                let event = event.map_err(|error| match error {
                    EventStreamError::Transport(error) => error,
                    _ => protocol("invalid event framing or UTF-8"),
                })?;
                let mut terminal = event.data == "[DONE]";
                let mut await_trailing_usage = false;
                if event.data != "[DONE]" {
                    let value: Value = serde_json::from_str(&event.data)
                        .map_err(|_| protocol("invalid event JSON"))?;
                    let response_terminal = matches!(
                        value["type"].as_str(),
                        Some("response.completed" | "response.failed" | "response.incomplete")
                    ) || (self.kind == ProviderKind::OpenRouter
                        && value["type"] == "response.done");
                    terminal = terminal
                        || response_terminal
                        || matches!(value["type"].as_str(), Some("error" | "message_stop"));
                    await_trailing_usage = (self.kind.is_chat_completions()
                        && matches!(
                            value["choices"][0]["finish_reason"].as_str(),
                            Some("length" | "content_filter")
                        ))
                        || matches!(
                            value["type"].as_str(),
                            Some("response.refusal.delta" | "response.refusal.done")
                        );
                    let reported = if self.kind == ProviderKind::Anthropic {
                        let counts = if value["type"] == "message_start" {
                            &value["message"]["usage"]
                        } else {
                            &value["usage"]
                        };
                        if let Some(counts) = counts.as_object() {
                            for (field, count) in counts {
                                message_usage[field] = count.clone();
                            }
                            Some((
                                anthropic::usage(&message_usage)?,
                                value["type"] == "message_delta"
                                    && !value["delta"]["stop_reason"].is_null(),
                            ))
                        } else {
                            None
                        }
                    } else if self.kind.is_chat_completions() {
                        if value["usage"].is_null() {
                            None
                        } else {
                            Some((chat_completions::usage(&value["usage"])?, true))
                        }
                    } else {
                        responses::usage(&value["response"]["usage"])?
                            .map(|usage| (usage, response_terminal))
                    };
                    if let Some((usage, complete)) = reported {
                        attempt.report(usage, complete).await?;
                        terminal = terminal || complete;
                    }
                }
                if trailing_usage_error.is_some() {
                    if terminal && let Some(error) = trailing_usage_error.take() {
                        return Err(error);
                    }
                    continue;
                }
                let completed = if self.kind == ProviderKind::Anthropic {
                    messages.accept_event(
                        &event,
                        request.execution,
                        request.output_bytes,
                        request.observations.as_ref(),
                    )
                } else if self.kind.is_chat_completions() {
                    chat.accept_event(
                        &event,
                        request.execution,
                        request.output_bytes,
                        request.observations.as_ref(),
                    )
                } else {
                    output.accept_event(
                        &event,
                        &self.responses(),
                        request.execution,
                        request.output_bytes,
                        request.observations.as_ref(),
                    )
                };
                let completed = match completed {
                    Err(error) if await_trailing_usage && !terminal => {
                        // Refusals and length stops can precede a final usage-only
                        // event. Drain only accounting; never accept later actions.
                        trailing_usage_error = Some(error);
                        continue;
                    }
                    result => result?,
                };
                if let Some(response) = completed {
                    if let Some(usage) = &response.usage {
                        attempt.report(usage.clone(), true).await?;
                    }
                    return Ok(response);
                }
            }
            Err(trailing_usage_error.unwrap_or_else(|| protocol("missing completed response")))
        }
        .await;
        attempt.finish(&result).await?;
        result
    }

    async fn generate_summary(
        &self,
        request: SummaryRequest,
        context: Option<(&ModelRequest, usize)>,
        cancellation: CancellationToken,
        accounting: RequestAccounting,
    ) -> Result<String, ModelError> {
        check_work(request.deadline, &cancellation)?;
        if request.output_tokens == 0 || request.output_bytes == 0 {
            return Err(protocol("checkpoint output budget must be positive"));
        }
        let generate = async {
            let outgoing = EncodedRequest {
                body: match context {
                    Some((context, input_bytes)) => {
                        self.context_summary_body(&request, context, input_bytes)?
                    }
                    None => self.summary_body(&request)?,
                },
                execution: ExecutionMode::Disabled,
                observations: None,
                output_bytes: request.output_bytes.min(DECODER_BYTES),
                accounting,
                purpose: RequestPurpose::Compaction,
            };
            check_work(request.deadline, &cancellation)?;
            let timeout = request
                .deadline
                .saturating_duration_since(tokio::time::Instant::now());
            let (progress, _activity) = tokio::sync::watch::channel(tokio::time::Instant::now());
            let response = self
                .request(outgoing, &cancellation, progress, timeout)
                .await?;
            check_work(request.deadline, &cancellation)?;
            match response.action {
                ModelAction::Final { answer } => {
                    check_text_bytes(answer.len(), request.output_bytes)?;
                    Ok(answer)
                }
                ModelAction::Execute { .. } => {
                    Err(protocol("checkpoint generation returned calls"))
                }
            }
        };
        tokio::select! {
            biased;
            () = cancellation.cancelled() => Err(ModelError::Cancelled),
            () = tokio::time::sleep_until(request.deadline) => Err(ModelError::Stalled),
            result = generate => result,
        }
    }

    fn body(&self, request: &ModelRequest, limits: ModelLimits) -> Result<Vec<u8>, ModelError> {
        crate::validate_context(request)?;
        if self.kind.is_chat_completions() {
            return chat_completions::body(
                request,
                &self.model,
                self.kind,
                &self.session_id,
                self.execution,
                self.thinking_level,
                limits,
            );
        }
        if self.kind == ProviderKind::Anthropic {
            return anthropic::body(
                request,
                &self.model,
                self.execution,
                limits,
                self.thinking_level,
            );
        }
        self.responses().body(request, limits)
    }

    fn context_summary_body(
        &self,
        request: &SummaryRequest,
        context: &ModelRequest,
        input_bytes: usize,
    ) -> Result<Vec<u8>, ModelError> {
        let input_bytes = input_bytes.min(REQUEST_BYTES);
        if input_bytes == 0 {
            return Err(protocol("checkpoint input budget must be positive"));
        }
        let mut candidate = match self.kind {
            ProviderKind::OpenAi => {
                let mut body: Value =
                    serde_json::from_slice(&self.body(context, self.model_limits)?)
                        .map_err(|_| protocol("invalid encoded request"))?;
                let input = body["input"]
                    .as_array_mut()
                    .ok_or_else(|| protocol("missing encoded input"))?;
                input.push(json!({"role":"developer","content":cocoa_core::compaction::SUMMARY_INSTRUCTIONS}));
                input.push(json!({"role":"user","content":request.input}));
                body["max_output_tokens"] = json!(request.output_tokens);
                Some(body)
            }
            ProviderKind::Anthropic => {
                crate::validate_context(context)?;
                // Changing tool choice invalidates Anthropic's message cache.
                // Reuse the tools/system boundary without paying to resend history.
                let encoded = anthropic::encode_messages(
                    vec![json!({"role":"user","content":request.input})],
                    &crate::instructions(context, self.execution)?,
                    &self.model,
                    self.execution,
                    ModelLimits {
                        output_tokens: Some(request.output_tokens),
                        context_tokens: None,
                    },
                    self.thinking_level,
                )?;
                let mut body: Value = serde_json::from_slice(&encoded)
                    .map_err(|_| protocol("invalid encoded request"))?;
                if body.get("system").is_none() {
                    body["system"] = json!([]);
                }
                body["system"]
                    .as_array_mut()
                    .ok_or_else(|| protocol("invalid encoded system"))?
                    .push(
                        json!({"type":"text","text":cocoa_core::compaction::SUMMARY_INSTRUCTIONS}),
                    );
                Some(body)
            }
            _ => None,
        };
        if let Some(body) = &mut candidate {
            if body.get("tools").is_some() {
                body["tool_choice"] = if self.kind == ProviderKind::Anthropic {
                    json!({"type":"none"})
                } else {
                    json!("none")
                };
            }
            let tokens = if self.kind == ProviderKind::OpenAi {
                crate::tokens::budget(context, self.execution, self.model_limits)?
                    .0
                    .input_tokens
                    .saturating_add(summary_tokens(&request.input))
            } else {
                crate::tokens::text(&body.to_string())
            };
            if let Ok(limits) = (ModelLimits {
                output_tokens: Some(request.output_tokens.min(self.model_limits.output_limit())),
                ..self.model_limits
            })
            .fit_output(tokens)
            {
                let parameter = if self.kind == ProviderKind::Anthropic {
                    "max_tokens"
                } else {
                    "max_output_tokens"
                };
                body[parameter] = json!(limits.output_limit());
                if encoded_within(body, input_bytes, "checkpoint_input").is_ok() {
                    return encode(body, input_bytes, "checkpoint_input");
                }
            }
        }
        let body = self.summary_body(request)?;
        if body.len() > input_bytes {
            return Err(byte_limit(
                "encoded",
                "checkpoint_input",
                body.len(),
                input_bytes,
            ));
        }
        Ok(body)
    }

    fn summary_body(&self, request: &SummaryRequest) -> Result<Vec<u8>, ModelError> {
        let output_tokens = if self.kind == ProviderKind::ChatGpt {
            request.output_tokens.max(self.model_limits.output_limit())
        } else {
            request.output_tokens.min(self.model_limits.output_limit())
        };
        let limits = ModelLimits {
            output_tokens: Some(output_tokens),
            ..self.model_limits
        }
        .fit_output(summary_tokens(&request.input))?;
        let mut input = vec![json!({"role":"user","content":request.input})];
        if self.kind == ProviderKind::Anthropic {
            return anthropic::encode_messages(
                input,
                cocoa_core::compaction::SUMMARY_INSTRUCTIONS,
                &self.model,
                ExecutionMode::Disabled,
                limits,
                self.thinking_level,
            );
        }
        if self.kind.is_chat_completions() {
            input.insert(
                0,
                json!({"role":"system","content":cocoa_core::compaction::SUMMARY_INSTRUCTIONS}),
            );
            return chat_completions::encode_messages(
                input,
                &self.model,
                self.kind,
                &self.session_id,
                ExecutionMode::Disabled,
                self.thinking_level,
                limits,
            );
        }
        self.responses().encode_input(
            input,
            cocoa_core::compaction::SUMMARY_INSTRUCTIONS,
            ExecutionMode::Disabled,
            limits,
        )
    }

    fn responses(&self) -> responses::Responses<'_> {
        responses::Responses {
            kind: self.kind,
            model: &self.model,
            session_id: &self.session_id,
            execution: self.execution,
            thinking_level: self.thinking_level,
            reasoning_summary: self.reasoning_summary,
            registration: self.credentials.as_ref().map(|source| source.identity()),
        }
    }
}

pub(crate) fn encode_request(
    mut body: Value,
    limits: ModelLimits,
    output_parameter: &str,
) -> Result<Vec<u8>, ModelError> {
    body[output_parameter] = json!(limits.output_limit());
    encode(&body, REQUEST_BYTES, "request_body")
}

pub(crate) fn check_text_bytes(bytes: usize, limit: usize) -> Result<(), ModelError> {
    if bytes > limit {
        return Err(byte_limit("response", "output_text", bytes, limit));
    }
    Ok(())
}

fn summary_tokens(input: &str) -> u64 {
    crate::tokens::text(input)
        .saturating_add(crate::tokens::text(
            cocoa_core::compaction::SUMMARY_INSTRUCTIONS,
        ))
        .saturating_add(128)
}

fn check_work(
    deadline: tokio::time::Instant,
    cancellation: &CancellationToken,
) -> Result<(), ModelError> {
    if cancellation.is_cancelled() {
        return Err(ModelError::Cancelled);
    }
    if tokio::time::Instant::now() >= deadline {
        return Err(ModelError::Stalled);
    }
    Ok(())
}

pub(crate) fn execution_function() -> Value {
    json!({"name":"exec","description":"Evaluate a complete JavaScript cell in the agent's private REPL. Retain full results in variables; print only selected fields or short excerpts. Keep ordinary printed feedback under 2000 characters TOTAL per cell, across all prints, and within the runtime's encoded byte limit. For data analysis, print computed aggregates rather than raw rows.","parameters":{"type":"object","properties":{"code":{"type":"string"}},"required":["code"],"additionalProperties":false}})
}

pub(crate) fn exec_cell(arguments: &str, limit: usize) -> Result<Cell, ModelError> {
    if arguments.len() > limit {
        return Err(byte_limit(
            "argument",
            "exec_arguments",
            arguments.len(),
            limit,
        ));
    }
    let arguments: ExecArguments =
        serde_json::from_str(arguments).map_err(|_| protocol("invalid exec arguments"))?;
    if arguments.code.trim().is_empty() {
        return Err(protocol("empty cell"));
    }
    Ok(Cell {
        code: arguments.code,
    })
}

pub(crate) fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str, ModelError> {
    value[key]
        .as_str()
        .ok_or_else(|| protocol("missing string field"))
}

pub(crate) fn identifier<'a>(value: &'a Value, key: &str) -> Result<&'a str, ModelError> {
    let value = text(value, key)?;
    if value.is_empty() || value.len() > 256 {
        return Err(protocol("invalid identifier"));
    }
    Ok(value)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecArguments {
    code: String,
}

pub(crate) fn protocol(message: &str) -> ModelError {
    ModelError::Protocol {
        message: message.into(),
    }
}

pub(crate) fn byte_limit(
    kind: &'static str,
    stage: &'static str,
    observed: usize,
    limit: usize,
) -> ModelError {
    protocol(&format!(
        "{kind} byte limit: stage={stage} observed_bytes_at_least={observed} limit_bytes={limit}"
    ))
}

fn transport(message: &str) -> ModelError {
    ModelError::Transport {
        message: message.into(),
    }
}

fn rejection_message(response: &reqwest::Response) -> String {
    let mut message = format!("HTTP status {}", response.status().as_u16());
    let request_id = response
        .headers()
        .get("x-request-id")
        .and_then(|header| header.to_str().ok())
        .filter(|id| {
            id.len() <= 128
                && id.starts_with("req_")
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
        });
    if let Some(id) = request_id {
        message.push_str(&format!("; request {id}"));
    }
    message
}

async fn rejection_body(mut response: reqwest::Response) -> Option<Value> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if bytes.len().saturating_add(chunk.len()) > 65536 {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    Some(serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

pub(crate) fn subscription_failure(value: &Value, status: Option<u16>) -> ModelError {
    let code = value
        .pointer("/response/error/code")
        .or_else(|| value.pointer("/error/code"))
        .or_else(|| value.get("code"))
        .and_then(Value::as_str);
    let message = match code {
        Some("subscription_sharing_usage_limit_exceeded") => {
            "subscription_sharing_usage_limit_exceeded: ChatGPT app usage limit reached. Use /usage to manage usage"
        }
        Some("subscription_sharing_user_not_eligible") => {
            "subscription_sharing_user_not_eligible: selected ChatGPT account is not eligible"
        }
        Some("invalid_grant" | "insufficient_scope" | "invalid_token") => {
            "ChatGPT authorization requires reconnecting the selected account"
        }
        _ => {
            "ChatGPT request did not complete; check account eligibility, permissions and model support. No API-key billing fallback"
        }
    };
    let mut message = message.to_owned();
    if let Some(status) = status {
        message.push_str(&format!("; HTTP {status}"));
    }
    if let Some(parameter) = value.pointer("/error/param").and_then(Value::as_str)
        && matches!(
            parameter,
            "model" | "tools" | "input" | "reasoning" | "max_output_tokens"
        )
    {
        message.push_str(&format!("; parameter {parameter}"));
    }
    transport(&message)
}

#[derive(Default)]
struct WireBounds {
    wire: usize,
    carriage_return: bool,
    prefix: Vec<u8>,
    prefix_checked: bool,
}

impl WireBounds {
    fn accept(&mut self, bytes: &[u8]) -> Result<Vec<u8>, ModelError> {
        self.wire = self.wire.saturating_add(bytes.len());
        if self.wire > DECODER_BYTES {
            return Err(byte_limit("wire", "wire_body", self.wire, DECODER_BYTES));
        }
        let mut normalized = Vec::with_capacity(bytes.len());
        for &byte in bytes {
            if byte == b'\n' && self.carriage_return {
                self.carriage_return = false;
                continue;
            }
            self.carriage_return = byte == b'\r';
            // eventsource-stream 0.2.3 slices BOM at byte one and waits for CR
            // lookahead at EOF. Normalize both before its UTF-8/framing decoder.
            let byte = if byte == b'\r' { b'\n' } else { byte };
            if !self.prefix_checked {
                self.prefix.push(byte);
                if b"\xef\xbb\xbf".starts_with(&self.prefix) {
                    if self.prefix.len() == 3 {
                        self.prefix.clear();
                        self.prefix_checked = true;
                    }
                } else {
                    normalized.extend_from_slice(&self.prefix);
                    self.prefix.clear();
                    self.prefix_checked = true;
                }
            } else {
                normalized.push(byte);
            }
        }
        Ok(normalized)
    }
}

fn retry_delay(response: &reqwest::Response, retries: u32) -> Option<Duration> {
    if !matches!(response.status().as_u16(), 429 | 500 | 502 | 503 | 504) {
        return None;
    }
    if let Some(value) = response.headers().get(RETRY_AFTER)
        && let Ok(value) = value.to_str()
    {
        let value = value.trim();
        let delay = if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
            // An overflowing number is excessive, not a malformed short hint.
            Some(Duration::from_secs(value.parse().ok()?))
        } else {
            httpdate::parse_http_date(value).ok().map(|date| {
                date.duration_since(SystemTime::now())
                    .unwrap_or(Duration::ZERO)
            })
        };
        if let Some(delay) = delay {
            return (delay <= RETRY_DELAY_MAX).then_some(delay);
        }
    }
    let jitter = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos()
        % 251;
    Some(Duration::from_secs(1 << retries.min(2)) + Duration::from_millis(u64::from(jitter)))
}

fn encoded_within(
    value: &impl Serialize,
    limit: usize,
    stage: &'static str,
) -> Result<(), ModelError> {
    let mut counter = CountLimit {
        remaining: limit,
        observed: 0,
    };
    serde_json::to_writer(&mut counter, value).map_err(|_| {
        if counter.observed > limit {
            byte_limit("encoded", stage, counter.observed, limit)
        } else {
            protocol("JSON encoding failed")
        }
    })
}

pub(crate) fn encode(
    value: &impl Serialize,
    limit: usize,
    stage: &'static str,
) -> Result<Vec<u8>, ModelError> {
    encoded_within(value, limit, stage)?;
    serde_json::to_vec(value).map_err(|_| protocol("JSON encoding failed"))
}

struct CountLimit {
    remaining: usize,
    observed: usize,
}

impl Write for CountLimit {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.observed = self.observed.saturating_add(bytes.len());
        self.remaining = self
            .remaining
            .checked_sub(bytes.len())
            .ok_or_else(|| std::io::Error::other("byte limit"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
