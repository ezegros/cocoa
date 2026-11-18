//! Native Messages contracts through the public provider and local HTTP fixtures.

use std::{
    error::Error,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use cocoa_core::{
    capability::{CapabilityDescription, CapabilityError, CapabilityHost, OperationContext},
    context::BoundedContext,
    engine::{
        Engine, EngineAdapters, EngineSettings, EventPublisher, Limits, Observation, RunRequest,
    },
    execution::{Cell, CellExecutor, CellResponse, ExecutionError, ShutdownReport, WorkLimits},
    model::{
        Image, ModelAction, ModelError, ModelProvider, ModelRequest, ModelResponse, UserInput,
    },
    store::{InMemoryStore, InvocationId, Record, RecordId, RecordKind, StoreLimits},
};
use cocoa_llm::{
    ModelLimits, Provider, ProviderKind, ProviderOptions,
    openai::{ExecutionMode, ThinkingLevel},
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tokio_util::sync::CancellationToken;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

struct Capture(Arc<Mutex<Option<ModelRequest>>>);

#[async_trait]
impl ModelProvider for Capture {
    async fn respond(
        &self,
        request: ModelRequest,
        _: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        *self.0.lock().map_err(|_| ModelError::Protocol {
            message: "capture lock poisoned".into(),
        })? = Some(request);
        Ok(ModelResponse {
            action: ModelAction::Final {
                answer: "captured".into(),
            },
            continuation: None,
            usage: None,
        })
    }
}

struct Unused;

#[async_trait]
impl CellExecutor for Unused {
    async fn execute(
        &mut self,
        _: Cell,
        _: &OperationContext,
        _: CancellationToken,
    ) -> Result<CellResponse, ExecutionError> {
        panic!("request factory must not execute")
    }
    async fn shutdown(&mut self, _: CancellationToken) -> Result<ShutdownReport, ExecutionError> {
        Ok(ShutdownReport { unresolved_work: 0 })
    }
}

#[async_trait]
impl CapabilityHost for Unused {
    fn descriptions(&self) -> Vec<CapabilityDescription> {
        Vec::new()
    }
    async fn call(
        &self,
        _: &str,
        _: Value,
        _: WorkLimits,
        _: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        panic!("request factory must not call capabilities")
    }
}

async fn request() -> Result<ModelRequest, Box<dyn Error>> {
    request_with_events(None).await
}

async fn request_with_events(
    events: Option<EventPublisher>,
) -> Result<ModelRequest, Box<dyn Error>> {
    let captured = Arc::new(Mutex::new(None));
    let mut engine = Engine::new(
        EngineSettings::new(Limits {
            input_bytes: 4096,
            output_bytes: 8192,
            record_bytes: 1024 * 1024,
            context_bytes: 4 * 1024 * 1024,
            pending_operations: 2,
            stall_timeout: Duration::from_secs(3),
            settlement_timeout: Duration::from_millis(100),
            shutdown_timeout: Duration::from_millis(100),
        })?,
        EngineAdapters {
            model: Arc::new(Capture(captured.clone())),
            executor: Box::new(Unused),
            capabilities: Arc::new(Unused),
            context: Arc::new(BoundedContext),
            store: Arc::new(InMemoryStore::new(StoreLimits {
                record_bytes: 1024 * 1024,
            })?),
        },
        events,
    );
    engine
        .run(
            RunRequest {
                input: "hello".into(),
            },
            CancellationToken::new(),
        )
        .await?;
    engine.shutdown().await?;
    let mut request = captured
        .lock()
        .map_err(|_| "capture lock poisoned")?
        .take()
        .ok_or("missing request")?;
    request.limits.deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    Ok(request)
}

fn options(base: &str) -> ProviderOptions {
    ProviderOptions {
        kind: ProviderKind::Anthropic,
        endpoint: format!("{base}/v1/messages"),
        model: "claude-opus-4-6".into(),
        api_key: Some("fixture-api-key".into()),
        execution: ExecutionMode::Enabled,
        thinking_level: None,
        session_id: "anthropic-fixture".into(),
    }
}

fn start() -> Value {
    json!({"type":"message_start","message":{"type":"message","id":"msg_native","role":"assistant","model":"claude-opus-4-6","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":17,"cache_creation_input_tokens":5,"cache_read_input_tokens":7,"output_tokens":1}}})
}

fn terminal(reason: &str) -> Vec<Value> {
    vec![
        json!({"type":"message_delta","delta":{"stop_reason":reason,"stop_sequence":null},"usage":{"output_tokens":11}}),
        json!({"type":"message_stop"}),
    ]
}

fn events(blocks: Vec<Value>, reason: &str) -> Vec<Value> {
    let mut events = vec![start()];
    for (index, block) in blocks.into_iter().enumerate() {
        let mut initial = block.clone();
        let delta = match block["type"].as_str() {
            Some("text") => {
                initial["text"] = json!("");
                Some(json!({"type":"text_delta","text":block["text"]}))
            }
            Some("tool_use") => {
                initial["input"] = json!({});
                Some(json!({"type":"input_json_delta","partial_json":block["input"].to_string()}))
            }
            Some("thinking") => {
                initial["thinking"] = json!("");
                initial["signature"] = json!("");
                Some(json!({"type":"thinking_delta","thinking":block["thinking"]}))
            }
            _ => None,
        };
        events.push(json!({"type":"content_block_start","index":index,"content_block":initial}));
        if let Some(delta) = delta {
            events.push(json!({"type":"content_block_delta","index":index,"delta":delta}));
        }
        if block["type"] == "thinking" {
            events.push(json!({"type":"content_block_delta","index":index,"delta":{"type":"signature_delta","signature":block["signature"]}}));
        }
        events.push(json!({"type":"content_block_stop","index":index}));
    }
    events.extend(terminal(reason));
    events
}

fn encode(events: &[Value]) -> String {
    let mut output = String::new();
    for event in events {
        output.push_str(&format!(
            "event: {}\ndata: {event}\n\n",
            event["type"].as_str().unwrap_or("invalid")
        ));
    }
    output
}

fn text_block(answer: &str) -> Value {
    json!({"type":"text","text":answer})
}
fn call(id: &str, code: &str) -> Value {
    json!({"type":"tool_use","id":id,"name":"exec","input":{"code":code}})
}
fn template(events: &[Value]) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(encode(events), "text/event-stream")
}

#[tokio::test]
async fn context_budgets_reserve_generation_or_ten_percent_without_stacking()
-> Result<(), Box<dyn Error>> {
    let request = request().await?;
    for (context_tokens, output_tokens, input_limit, compaction_trigger) in [
        (None, None, 96_000, 96_000),
        (Some(500_000), None, 468_000, 450_000),
        (Some(500_003), Some(1700), 498_303, 450_002),
        (Some(200_000), Some(96_000), 104_000, 104_000),
        (Some(64_000), Some(4096), 59_904, 57_600),
        (Some(8192), None, 4096, 4096),
        (Some(16_384), None, 8192, 8192),
        (Some(32_000), None, 16_000, 16_000),
        (Some(32_001), None, 16_000, 16_000),
        (Some(32_768), None, 16_384, 16_384),
        (Some(40_960), None, 20_480, 20_480),
        (Some(64_000), None, 32_000, 32_000),
        (Some(65_536), None, 33_536, 33_536),
        (Some(131_072), Some(131_071), 65_536, 65_536),
        (Some(131_072), Some(131_072), 65_536, 65_536),
    ] {
        let mut provider = Provider::new(options("http://127.0.0.1:1"))?;
        provider.set_model_limits(ModelLimits {
            context_tokens,
            output_tokens,
        })?;
        let budget = provider
            .request_token_budget(&request)?
            .ok_or("missing token budget")?;
        assert_eq!(budget.input_limit, input_limit);
        assert_eq!(budget.compaction_trigger, compaction_trigger);
    }
    Ok(())
}

#[tokio::test]
async fn small_windows_fit_wire_output_to_remaining_context() -> Result<(), Box<dyn Error>> {
    for kind in [
        ProviderKind::OpenAi,
        ProviderKind::Anthropic,
        ProviderKind::Nvidia,
        ProviderKind::Ollama,
    ] {
        for (context_tokens, output_tokens, input_limit) in [
            (8192, None, 4096),
            (16_384, None, 8192),
            (32_000, None, 16_000),
            (32_768, None, 16_384),
            (131_072, Some(131_072), 65_536),
        ] {
            let server = MockServer::start().await;
            let stream = match kind {
                ProviderKind::Anthropic => encode(&events(vec![text_block("done")], "end_turn")),
                ProviderKind::OpenAi => format!(
                    "data: {}\n\n",
                    json!({"type":"response.completed","response":{"id":"answer","status":"completed","output":[{"id":"message","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"done"}]}],"usage":{"input_tokens":29,"output_tokens":11}}})
                ),
                _ => format!(
                    "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                    json!({"id":"answer","choices":[{"index":0,"delta":{"role":"assistant","content":"done"},"finish_reason":null}]}),
                    json!({"id":"answer","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
                    json!({"id":"answer","choices":[],"usage":{"prompt_tokens":29,"completion_tokens":11}})
                ),
            };
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(200).set_body_raw(stream, "text/event-stream"))
                .expect(3)
                .mount(&server)
                .await;
            let mut options = options(&server.uri());
            options.kind = kind;
            options.endpoint = format!("{}/v1/responses", server.uri());
            options.api_key = (kind != ProviderKind::Ollama).then(|| "fixture-api-key".into());
            let mut provider = Provider::new(options)?;
            provider.set_model_limits(ModelLimits {
                context_tokens: Some(context_tokens),
                output_tokens,
            })?;
            let mut request = request().await?;
            let response = provider
                .respond(request.clone(), CancellationToken::new())
                .await?;
            assert_eq!(
                response
                    .usage
                    .as_ref()
                    .map(|usage| (usage.input_tokens, usage.output_tokens)),
                Some((29, 11))
            );
            request.context.records.push(Record {
                invocation: request.context.records[0].invocation,
                kind: RecordKind::ModelCompleted {
                    response,
                    shared_context: None,
                },
            });
            let budget = provider
                .request_token_budget(&request)?
                .ok_or("missing budget")?;
            assert!(budget.input_tokens >= 29 + 11);
            let first_cap = (u64::from(context_tokens) - budget.input_tokens)
                .min(u64::from(output_tokens.unwrap_or(32_000)));
            provider
                .respond(request.clone(), CancellationToken::new())
                .await?;
            let field = if kind == ProviderKind::OpenAi {
                "max_output_tokens"
            } else {
                "max_tokens"
            };
            let sent = server.received_requests().await.ok_or("missing requests")?;
            assert_eq!(sent[1].body_json::<Value>()?[field], first_cap, "{kind}");
            // Shift reported usage to reach each admission boundary without
            // depending on the estimator's text ratio or message framing.
            let retained_tokens = budget.input_tokens - 29;
            for input_tokens in [input_limit, input_limit + 1] {
                let RecordKind::ModelCompleted { response, .. } = &mut request
                    .context
                    .records
                    .last_mut()
                    .ok_or("missing retained response")?
                    .kind
                else {
                    return Err("missing retained response".into());
                };
                response.usage.as_mut().ok_or("missing usage")?.input_tokens =
                    u64::from(input_tokens)
                        .checked_sub(retained_tokens)
                        .ok_or("fixture exceeds input limit")?;
                let result = provider
                    .respond(request.clone(), CancellationToken::new())
                    .await;
                if input_tokens == input_limit {
                    assert_eq!(
                        result?.action,
                        ModelAction::Final {
                            answer: "done".into()
                        }
                    );
                    let sent = server.received_requests().await.ok_or("missing requests")?;
                    assert_eq!(
                        sent[2].body_json::<Value>()?[field],
                        context_tokens - input_limit,
                        "{kind}"
                    );
                } else {
                    assert!(
                        matches!(result, Err(ModelError::ContextLimit { input_tokens: observed, input_limit: limit })
                        if observed == u64::from(input_tokens) && limit == u64::from(input_limit)),
                        "{kind}"
                    );
                }
            }
            assert_eq!(
                server
                    .received_requests()
                    .await
                    .ok_or("missing requests")?
                    .len(),
                3
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn messages_headers_system_images_and_cached_usage() -> Result<(), Box<dyn Error>> {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(template(&events(vec![text_block("bonjour é")], "end_turn")))
        .expect(1)
        .mount(&server)
        .await;
    let provider = Provider::new(options(&server.uri()))?;
    let mut request = request().await?;
    request.context.instructions = "system policy".into();
    let image = Image::from_png(b"\x89PNG\r\n\x1a\n")?;
    request.context.records[0].kind = RecordKind::InputAccepted {
        input: UserInput::WithImages {
            text: "describe".into(),
            images: vec![image],
        },
    };
    let response = provider.respond(request, CancellationToken::new()).await?;
    assert_eq!(
        response.action,
        ModelAction::Final {
            answer: "bonjour é".into()
        }
    );
    let usage = response.usage.ok_or("missing usage")?;
    assert_eq!((usage.input_tokens, usage.output_tokens), (29, 11));
    assert_eq!(
        usage.input_cache,
        Some(cocoa_core::model::InputCacheUsage {
            read_tokens: Some(7),
            write_tokens: Some(5),
        })
    );
    let received = server.received_requests().await.ok_or("missing requests")?;
    let sent = &received[0];
    assert_eq!(sent.url.path(), "/v1/messages");
    assert_eq!(
        sent.headers.get("x-api-key").ok_or("missing key")?,
        "fixture-api-key"
    );
    assert_eq!(
        sent.headers
            .get("anthropic-version")
            .ok_or("missing version")?,
        "2023-06-01"
    );
    assert!(!sent.headers.contains_key("authorization"));
    let body: Value = serde_json::from_slice(&sent.body)?;
    assert!(
        body["system"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("system policy") && text.contains("exec({code})"))
    );
    assert_eq!(
        body["messages"][0],
        json!({"role":"user","content":[{"type":"text","text":"describe"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgo="}}]})
    );
    assert_eq!(
        body["tools"][0]["input_schema"],
        json!({"type":"object","properties":{"code":{"type":"string"}},"required":["code"],"additionalProperties":false})
    );
    assert_eq!(body["tools"][0]["strict"], true);
    assert_eq!(body["max_tokens"], 32000);
    assert_eq!(body["cache_control"], json!({"type":"ephemeral"}));
    assert!(body.get("thinking").is_none());
    assert!(body.get("store").is_none());
    Ok(())
}

#[tokio::test]
async fn grouped_results_replay_signed_and_redacted_thinking_unchanged()
-> Result<(), Box<dyn Error>> {
    let server = MockServer::start().await;
    let blocks = vec![
        json!({"type":"thinking","thinking":"private-thought","signature":"opaque-signature","future":{"unchanged":true}}),
        json!({"type":"redacted_thinking","data":"opaque-redacted"}),
        call("tool_a", "print(1)"),
        call("tool_b", "print(2)"),
    ];
    let first = template(&events(blocks.clone(), "tool_use"));
    let last = template(&events(vec![text_block("done")], "end_turn"));
    let count = Arc::new(AtomicUsize::new(0));
    let replies = count.clone();
    Mock::given(method("POST"))
        .respond_with(move |_: &wiremock::Request| {
            if replies.fetch_add(1, Ordering::SeqCst) == 0 {
                first.clone()
            } else {
                last.clone()
            }
        })
        .expect(2)
        .mount(&server)
        .await;
    let provider = Provider::new(options(&server.uri()))?;
    let mut request = request().await?;
    let first = provider
        .respond(request.clone(), CancellationToken::new())
        .await?;
    assert_eq!(
        first.action,
        ModelAction::Execute {
            cells: vec![
                Cell {
                    code: "print(1)".into()
                },
                Cell {
                    code: "print(2)".into()
                }
            ]
        }
    );
    assert_eq!(
        first
            .continuation
            .as_ref()
            .ok_or("missing continuation")?
            .data["message"]["content"],
        json!(blocks)
    );
    let invocation = request.context.records[0].invocation;
    request.context.records.push(Record {
        invocation,
        kind: RecordKind::ModelCompleted {
            response: first,
            shared_context: None,
        },
    });
    for (cell, code) in ["print(1)", "print(2)"].into_iter().enumerate() {
        request.context.records.push(Record {
            invocation,
            kind: RecordKind::CellStarted {
                model: RecordId::try_from(2)?,
                cell: cell as u32,
            },
        });
        request.context.records.push(Record {
            invocation,
            kind: RecordKind::CellCompleted {
                model: RecordId::try_from(2)?,
                cell: cell as u32,
                code: code.into(),
                outcome: CellResponse::Completed {
                    printed: vec![json!(cell + 1)],
                },
            },
        });
    }
    let response = provider.respond(request, CancellationToken::new()).await?;
    assert_eq!(
        response.action,
        ModelAction::Final {
            answer: "done".into()
        }
    );
    assert_eq!(count.load(Ordering::SeqCst), 2);
    let requests = server.received_requests().await.ok_or("missing requests")?;
    let initial: Value = serde_json::from_slice(&requests[0].body)?;
    let body: Value = serde_json::from_slice(&requests[1].body)?;
    assert_eq!(initial["cache_control"], json!({"type":"ephemeral"}));
    assert_eq!(body["cache_control"], initial["cache_control"]);
    assert_eq!(body["system"], initial["system"]);
    assert_eq!(body["tools"], initial["tools"]);
    assert_eq!(body["messages"][0], initial["messages"][0]);
    assert_eq!(
        body["messages"][1],
        json!({"role":"assistant","content":blocks})
    );
    let results = body["messages"][2]["content"]
        .as_array()
        .ok_or("missing grouped results")?;
    assert_eq!(results.len(), 2);
    for (index, id) in ["tool_a", "tool_b"].into_iter().enumerate() {
        assert_eq!(results[index]["type"], "tool_result");
        assert_eq!(results[index]["tool_use_id"], id);
        let content = results[index]["content"]
            .as_str()
            .ok_or("missing feedback")?;
        let feedback = serde_json::Deserializer::from_str(content)
            .into_iter::<Value>()
            .next()
            .ok_or("missing JSON feedback")??;
        assert!(content.ends_with("Selected exec calls for this input: 2."));
        assert_eq!(feedback["status"], "completed");
        assert_eq!(feedback["printed"], json!([index + 1]));
    }
    Ok(())
}

#[tokio::test]
async fn partial_failed_refused_and_inconsistent_stops_never_return_calls()
-> Result<(), Box<dyn Error>> {
    let mut fixtures = Vec::new();
    for reason in [
        "max_tokens",
        "refusal",
        "stop_sequence",
        "pause_turn",
        "end_turn",
        "unknown",
    ] {
        fixtures.push(events(vec![call("tool_a", "print(1)")], reason));
    }
    let valid = events(vec![call("tool_a", "print(1)")], "tool_use");
    for count in 0..valid.len() {
        fixtures.push(valid[..count].to_vec());
    }
    fixtures.push(events(vec![text_block("answer")], "tool_use"));
    let mut error = valid[..4].to_vec();
    error.push(json!({"type":"error","error":{"type":"overloaded_error","message":"secret-provider-error"}}));
    fixtures.push(error);
    let mut unfinished = valid.clone();
    unfinished.remove(3);
    fixtures.push(unfinished);
    let mut late = valid.clone();
    late.insert(4, valid[2].clone());
    fixtures.push(late);
    let mut mismatch = valid.clone();
    mismatch[2]["delta"]["type"] = json!("text_delta");
    fixtures.push(mismatch);
    let mut duplicate = events(
        vec![call("same", "print(1)"), call("same", "print(2)")],
        "tool_use",
    );
    fixtures.push(std::mem::take(&mut duplicate));
    for input in [
        json!({"code":""}),
        json!({"code":1}),
        json!({"source":"print(1)"}),
    ] {
        let mut block = call("tool_a", "print(1)");
        block["input"] = input;
        fixtures.push(events(vec![block], "tool_use"));
    }
    for fixture in fixtures {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(template(&fixture))
            .expect(1)
            .mount(&server)
            .await;
        let provider = Provider::new(options(&server.uri()))?;
        let failure = provider
            .respond(request().await?, CancellationToken::new())
            .await
            .err()
            .ok_or("invalid response accepted")?;
        let diagnostic = format!("{failure:?}: {failure}");
        assert!(!diagnostic.contains("secret-provider-error"));
        assert!(!diagnostic.contains("fixture-api-key"));
        assert_eq!(
            server
                .received_requests()
                .await
                .ok_or("missing requests")?
                .len(),
            1
        );
    }
    Ok(())
}

#[tokio::test]
async fn explicit_effort_is_faithful_and_defaults_are_omitted() -> Result<(), Box<dyn Error>> {
    for (model, level) in [
        ("claude-opus-4-6", ThinkingLevel::XHigh),
        ("claude-opus-4-6", ThinkingLevel::Minimal),
        ("claude-3-7-sonnet-latest", ThinkingLevel::High),
        ("unknown", ThinkingLevel::Max),
    ] {
        let mut options = options("http://127.0.0.1:1");
        options.model = model.into();
        options.thinking_level = Some(level);
        assert!(Provider::new(options).is_err());
    }
    for level in [
        ThinkingLevel::None,
        ThinkingLevel::Low,
        ThinkingLevel::Medium,
        ThinkingLevel::High,
        ThinkingLevel::XHigh,
        ThinkingLevel::Max,
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(template(&events(vec![text_block("done")], "end_turn")))
            .mount(&server)
            .await;
        let mut options = options(&server.uri());
        options.model = "claude-opus-4-7".into();
        options.thinking_level = Some(level);
        Provider::new(options)?
            .respond(request().await?, CancellationToken::new())
            .await?;
        let requests = server.received_requests().await.ok_or("missing requests")?;
        let body: Value = serde_json::from_slice(&requests[0].body)?;
        if level == ThinkingLevel::None {
            assert_eq!(body["thinking"], json!({"type":"disabled"}));
            assert!(body.get("output_config").is_none());
        } else {
            assert_eq!(body["thinking"], json!({"type":"adaptive"}));
            assert_eq!(body["output_config"]["effort"], json!(level));
        }
        assert!(body["thinking"].get("budget_tokens").is_none());
    }
    Ok(())
}

#[tokio::test]
async fn retries_only_explicit_rejections_and_cancellation_stays_terminal()
-> Result<(), Box<dyn Error>> {
    let server = MockServer::start().await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let replies = attempts.clone();
    Mock::given(method("POST"))
        .respond_with(move |_: &wiremock::Request| {
            if replies.fetch_add(1, Ordering::SeqCst) == 0 {
                ResponseTemplate::new(429).insert_header("retry-after", "0")
            } else {
                template(&events(vec![text_block("retried")], "end_turn"))
            }
        })
        .expect(2)
        .mount(&server)
        .await;
    let provider = Provider::new(options(&server.uri()))?;
    let accounting = cocoa_core::model::RequestAccounting::default();
    let response = provider
        .respond_accounted(
            request().await?,
            CancellationToken::new(),
            accounting.clone(),
        )
        .await?;
    assert_eq!(
        response.action,
        ModelAction::Final {
            answer: "retried".into()
        }
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    let summary = accounting.summary()?;
    assert_eq!(summary.requests_started, 2);
    assert_eq!(summary.requests_finished, 2);
    assert_eq!(summary.requests_with_final_usage, 1);
    assert!(summary.usage.is_none());
    assert_eq!(
        summary
            .reported_usage
            .as_ref()
            .map(|usage| (usage.input_tokens, usage.output_tokens)),
        Some((29, 11))
    );
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(matches!(
        provider.respond(request().await?, cancelled).await,
        Err(ModelError::Cancelled)
    ));
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    Ok(())
}

async fn receive_request(socket: &mut TcpStream) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut received = Vec::new();
    for _ in 0..128 {
        let mut buffer = [0; 8192];
        let count = socket.read(&mut buffer).await?;
        if count == 0 {
            return Err("request disconnected".into());
        }
        received.extend_from_slice(&buffer[..count]);
        if let Some(end) = received.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&received[..end])?;
            let length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|value| value.trim().parse::<usize>())
                })
                .transpose()?
                .ok_or("missing content-length")?;
            if received.len() >= end + 4 + length {
                return Ok(());
            }
        }
    }
    Err("fixture request exceeded limit".into())
}

#[tokio::test]
async fn fragmented_utf8_and_concurrent_blocks_complete_in_index_order()
-> Result<(), Box<dyn Error>> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let mut fixture = vec![
        start(),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"β"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"α"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_stop","index":1}),
    ];
    fixture.extend(terminal("end_turn"));
    let bytes = encode(&fixture);
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await?;
        receive_request(&mut socket).await?;
        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",bytes.len()).as_bytes()).await?;
        for chunk in bytes.as_bytes().chunks(1) {
            socket.write_all(chunk).await?;
            tokio::task::yield_now().await;
        }
        Ok::<(), Box<dyn Error + Send + Sync>>(())
    });
    let response = Provider::new(options(&format!("http://{address}")))?
        .respond(request().await?, CancellationToken::new())
        .await?;
    task.await?.map_err(|error| -> Box<dyn Error> { error })?;
    assert_eq!(
        response.action,
        ModelAction::Final {
            answer: "αβ".into()
        }
    );
    Ok(())
}

#[tokio::test]
async fn replay_rejects_foreign_provider_missing_feedback_and_wrong_invocation()
-> Result<(), Box<dyn Error>> {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(template(&events(
            vec![call("tool_a", "print(1)")],
            "tool_use",
        )))
        .expect(1)
        .mount(&server)
        .await;
    let provider = Provider::new(options(&server.uri()))?;
    let original = request().await?;
    let response = provider
        .respond(original.clone(), CancellationToken::new())
        .await?;
    for variant in 0..4 {
        let mut request = original.clone();
        let invocation = request.context.records[0].invocation;
        let mut response = response.clone();
        if variant == 0 {
            response
                .continuation
                .as_mut()
                .ok_or("missing continuation")?
                .provider = "openai.responses".into();
        }
        request.context.records.push(Record {
            invocation,
            kind: RecordKind::ModelCompleted {
                response,
                shared_context: None,
            },
        });
        if variant >= 2 {
            request.context.records.push(Record {
                invocation: if variant == 2 {
                    InvocationId(invocation.0 + 1)
                } else {
                    invocation
                },
                kind: RecordKind::CellCompleted {
                    model: RecordId::try_from(2)?,
                    cell: 0,
                    code: if variant == 3 {
                        "different".into()
                    } else {
                        "print(1)".into()
                    },
                    outcome: CellResponse::Completed {
                        printed: Vec::new(),
                    },
                },
            });
        }
        assert!(
            provider
                .respond(request, CancellationToken::new())
                .await
                .is_err()
        );
    }
    assert_eq!(
        server
            .received_requests()
            .await
            .ok_or("missing requests")?
            .len(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn anthropic_cache_usage_preserves_asymmetric_counts_and_unknowns()
-> Result<(), Box<dyn Error>> {
    for (cache, input_tokens, read_tokens, write_tokens) in [
        (json!({}), 17, None, None),
        (
            json!({"cache_read_input_tokens":null,"cache_creation_input_tokens":null}),
            17,
            None,
            None,
        ),
        (
            json!({"cache_read_input_tokens":7,"cache_creation_input_tokens":5}),
            29,
            Some(7),
            Some(5),
        ),
        (json!({"cache_read_input_tokens":7}), 24, Some(7), None),
        (json!({"cache_creation_input_tokens":5}), 22, None, Some(5)),
        (
            json!({"cache_read_input_tokens":7,"cache_creation_input_tokens":null}),
            24,
            Some(7),
            None,
        ),
        (
            json!({"cache_read_input_tokens":null,"cache_creation_input_tokens":5}),
            22,
            None,
            Some(5),
        ),
        (
            json!({"cache_read_input_tokens":0,"cache_creation_input_tokens":0}),
            17,
            Some(0),
            Some(0),
        ),
    ] {
        let mut fixture = events(vec![text_block("done")], "end_turn");
        fixture[0]["message"]["usage"] = json!({"input_tokens":17,"output_tokens":1});
        for (field, count) in cache.as_object().ok_or("missing cache object")? {
            fixture[0]["message"]["usage"][field] = count.clone();
        }
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(template(&fixture))
            .expect(1)
            .mount(&server)
            .await;
        let response = Provider::new(options(&server.uri()))?
            .respond(request().await?, CancellationToken::new())
            .await?;
        let usage = response.usage.ok_or("missing usage")?;
        assert_eq!(
            (usage.input_tokens, usage.output_tokens),
            (input_tokens, 11)
        );
        assert_eq!(
            usage.input_cache,
            if read_tokens.is_some() || write_tokens.is_some() {
                Some(cocoa_core::model::InputCacheUsage {
                    read_tokens,
                    write_tokens,
                })
            } else {
                None
            }
        );
    }
    Ok(())
}

#[tokio::test]
async fn anthropic_cache_usage_rejects_malformed_counts_and_overflow() -> Result<(), Box<dyn Error>>
{
    let valid = json!({"input_tokens":17,"output_tokens":11,"cache_read_input_tokens":7,"cache_creation_input_tokens":5});
    let mut invalid = Vec::new();
    for count in [
        json!(-1),
        json!(1.5),
        json!("1"),
        json!(true),
        json!([]),
        json!({}),
        json!(1e30),
    ] {
        for field in [
            "input_tokens",
            "output_tokens",
            "cache_read_input_tokens",
            "cache_creation_input_tokens",
        ] {
            let mut usage = valid.clone();
            usage[field] = count.clone();
            invalid.push(usage);
        }
    }
    for field in ["input_tokens", "output_tokens"] {
        let mut usage = valid.clone();
        usage[field] = Value::Null;
        invalid.push(usage);
    }
    for field in [
        "input_tokens",
        "cache_read_input_tokens",
        "cache_creation_input_tokens",
    ] {
        let mut usage = valid.clone();
        usage[field] = json!(u64::MAX);
        invalid.push(usage);
    }
    for at_start in [false, true] {
        for usage in &invalid {
            let mut fixture = events(vec![text_block("invalid usage")], "end_turn");
            let index = if at_start { 0 } else { fixture.len() - 2 };
            if at_start {
                fixture[index]["message"]["usage"] = usage.clone();
            } else {
                fixture[index]["usage"] = usage.clone();
            }
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(template(&fixture))
                .expect(1)
                .mount(&server)
                .await;
            let result = Provider::new(options(&server.uri()))?
                .respond(request().await?, CancellationToken::new())
                .await;
            assert!(
                matches!(result, Err(ModelError::Protocol { .. })),
                "{usage}: {result:?}"
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn opaque_thinking_is_never_published_and_usage_deltas_are_cumulative()
-> Result<(), Box<dyn Error>> {
    let server = MockServer::start().await;
    let blocks = vec![
        json!({"type":"thinking","thinking":"private-secret-thought","signature":"private-signature"}),
        json!({"type":"redacted_thinking","data":"private-redaction"}),
        text_block("safe-answer"),
    ];
    let mut fixture = events(blocks, "end_turn");
    let terminal = fixture.len() - 2;
    fixture[terminal]["usage"] = json!({"input_tokens":20,"cache_creation_input_tokens":3,"cache_read_input_tokens":9,"output_tokens":11});
    fixture.insert(
        terminal + 1,
        json!({"type":"message_delta","delta":{},"usage":{"output_tokens":15}}),
    );
    Mock::given(method("POST"))
        .respond_with(template(&fixture))
        .mount(&server)
        .await;
    let (publisher, mut observations) = EventPublisher::channel(64, 4096)?;
    let request = request_with_events(Some(publisher)).await?;
    let accounting = cocoa_core::model::RequestAccounting::default();
    let response = Provider::new(options(&server.uri()))?
        .respond_accounted(request, CancellationToken::new(), accounting.clone())
        .await?;
    let summary = accounting.summary()?;
    assert_eq!(summary.requests_started, 1);
    assert_eq!(summary.requests_finished, 1);
    assert_eq!(summary.usage, response.usage);
    let usage = response.usage.ok_or("missing usage")?;
    assert_eq!((usage.input_tokens, usage.output_tokens), (32, 15));
    assert_eq!(
        usage.input_cache,
        Some(cocoa_core::model::InputCacheUsage {
            read_tokens: Some(9),
            write_tokens: Some(3),
        })
    );
    let mut displayed = String::new();
    for _ in 0..64 {
        match tokio::time::timeout(Duration::from_secs(1), observations.recv()).await? {
            Observation::Closed => break,
            observation => displayed.push_str(&format!("{observation:?}")),
        }
    }
    assert!(displayed.contains("safe-answer"));
    for secret in [
        "private-secret-thought",
        "private-signature",
        "private-redaction",
    ] {
        assert!(!displayed.contains(secret));
    }
    Ok(())
}

#[tokio::test]
async fn structural_bounds_and_bad_signature_or_usage_fail_closed() -> Result<(), Box<dyn Error>> {
    let mut fixtures = Vec::new();
    let mut block = call(&"x".repeat(257), "print(1)");
    fixtures.push(events(vec![block.clone()], "tool_use"));
    block["id"] = json!("ok");
    block["name"] = json!("x".repeat(257));
    fixtures.push(events(vec![block], "tool_use"));
    fixtures.push(events(
        (0..33)
            .map(|index| call(&format!("tool_{index}"), "print(1)"))
            .collect(),
        "tool_use",
    ));
    let mut fixture = events(vec![text_block("answer")], "end_turn");
    fixture[1]["index"] = json!(1024);
    fixtures.push(fixture);
    let mut fixture = events(vec![text_block("answer")], "end_turn");
    fixture[1]["index"] = json!(1);
    fixture[2]["index"] = json!(1);
    fixture[3]["index"] = json!(1);
    fixtures.push(fixture);
    fixtures.push(events(
        vec![
            json!({"type":"thinking","thinking":"private","signature":""}),
            text_block("answer"),
        ],
        "end_turn",
    ));
    fixtures.push(events(
        vec![
            json!({"type":"redacted_thinking","data":""}),
            text_block("answer"),
        ],
        "end_turn",
    ));
    let mut fixture = events(vec![text_block("answer")], "end_turn");
    fixture[0]["message"]["usage"]["input_tokens"] = json!(u64::MAX);
    fixtures.push(fixture);
    let mut fixture = events(vec![text_block("answer")], "end_turn");
    fixture[4]["usage"]["output_tokens"] = json!(0);
    fixtures.push(fixture);
    let mut fixture = events(vec![call("tool_a", "print(1)")], "tool_use");
    fixture[2]["delta"]["partial_json"] = json!("{bad-json");
    fixtures.push(fixture);
    let mut fixture = events(vec![call("tool_a", "print(1)")], "tool_use");
    fixture[2]["delta"]["partial_json"] = json!("{\"code\":\"print(1)\"}{}");
    fixtures.push(fixture);
    for fixture in fixtures {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(template(&fixture))
            .expect(1)
            .mount(&server)
            .await;
        assert!(
            Provider::new(options(&server.uri()))?
                .respond(request().await?, CancellationToken::new())
                .await
                .is_err()
        );
    }
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(template(&events(
            vec![call("tool_a", "print(1)")],
            "tool_use",
        )))
        .expect(1)
        .mount(&server)
        .await;
    let mut disabled = options(&server.uri());
    disabled.execution = ExecutionMode::Disabled;
    assert!(
        Provider::new(disabled)?
            .respond(request().await?, CancellationToken::new())
            .await
            .is_err()
    );
    let requests = server.received_requests().await.ok_or("missing request")?;
    let body: Value = serde_json::from_slice(&requests[0].body)?;
    assert!(body.get("tools").is_none());
    Ok(())
}

#[tokio::test]
async fn documented_models_keep_max_distinct_from_xhigh() -> Result<(), Box<dyn Error>> {
    for model in ["claude-opus-4-6", "claude-sonnet-4-6"] {
        let mut rejected = options("http://127.0.0.1:1");
        rejected.model = model.into();
        rejected.thinking_level = Some(ThinkingLevel::XHigh);
        assert!(Provider::new(rejected).is_err());
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(template(&events(vec![text_block("done")], "end_turn")))
            .expect(1)
            .mount(&server)
            .await;
        let mut accepted = options(&server.uri());
        accepted.model = model.into();
        accepted.thinking_level = Some(ThinkingLevel::Max);
        Provider::new(accepted)?
            .respond(request().await?, CancellationToken::new())
            .await?;
        let requests = server.received_requests().await.ok_or("missing requests")?;
        let body: Value = serde_json::from_slice(&requests[0].body)?;
        assert_eq!(body["output_config"]["effort"], "max");
    }
    Ok(())
}

#[tokio::test]
async fn cancellation_during_partial_message_does_not_return_calls() -> Result<(), Box<dyn Error>> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let (ready, received) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await?;
        receive_request(&mut socket).await?;
        let partial = encode(&events(vec![call("tool_a", "print(1)")], "tool_use")[..4]);
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 100000\r\n\r\n").await?;
        socket.write_all(partial.as_bytes()).await?;
        ready.send(()).map_err(|_| "receiver dropped")?;
        let mut byte = [0];
        if let Err(error) = socket.read(&mut byte).await
            && error.kind() != std::io::ErrorKind::ConnectionReset
        {
            return Err(error.into());
        }
        Ok::<(), Box<dyn Error + Send + Sync>>(())
    });
    let cancellation = CancellationToken::new();
    let stop = cancellation.clone();
    let accounting = cocoa_core::model::RequestAccounting::default();
    let observed = accounting.clone();
    let cancel = tokio::spawn(async move {
        received.await?;
        for _ in 0..1000 {
            if observed.summary()?.reported_usage.is_some() {
                stop.cancel();
                return Ok::<(), Box<dyn Error + Send + Sync>>(());
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        stop.cancel();
        Err("provider did not account the partial message".into())
    });
    let result = Provider::new(options(&format!("http://{address}")))?
        .respond_accounted(request().await?, cancellation, accounting.clone())
        .await;
    assert!(matches!(result, Err(ModelError::Cancelled)));
    let summary = accounting.summary()?;
    assert_eq!(summary.requests_started, 1);
    assert_eq!(summary.requests_with_final_usage, 0);
    assert!(summary.usage.is_none());
    let reported = summary.reported_usage.ok_or("missing partial usage")?;
    assert_eq!((reported.input_tokens, reported.output_tokens), (29, 1));
    cancel.await?.map_err(|error| -> Box<dyn Error> { error })?;
    tokio::time::timeout(Duration::from_secs(3), task)
        .await??
        .map_err(|error| -> Box<dyn Error> { error })?;
    Ok(())
}

#[tokio::test]
async fn accounting_captures_failed_terminal_usage_missing_usage_and_partial_streams()
-> Result<(), Box<dyn Error>> {
    use cocoa_core::model::RequestAccounting;
    let completed = |usage: Value| json!({"type":"response.completed","response":{"id":"response_1","status":"completed","output":[{"id":"message_1","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"safe answer","annotations":[]}]}],"usage":usage}});
    let failed = json!({"type":"response.failed","response":{"id":"response_1","status":"failed","error":{"message":"private-remote-diagnostic"},"usage":{"input_tokens":41,"output_tokens":13}}});
    let chat = json!({"id":"chat_1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":"truncated"},"finish_reason":"length"}],"usage":{"prompt_tokens":53,"completion_tokens":17}});
    let mut chat_stop = chat.clone();
    chat_stop["usage"] = Value::Null;
    let chat_usage = json!({"id":"chat_1","object":"chat.completion.chunk","choices":[],"usage":{"prompt_tokens":67,"completion_tokens":23}});
    for (kind, fixture, successful, counts, complete) in [
        (
            ProviderKind::OpenAi,
            vec![completed(json!({"input_tokens":31,"output_tokens":7}))],
            true,
            Some((31, 7)),
            true,
        ),
        (
            ProviderKind::OpenAi,
            vec![completed(Value::Null)],
            true,
            None,
            false,
        ),
        (
            ProviderKind::OpenAi,
            vec![failed],
            false,
            Some((41, 13)),
            true,
        ),
        (
            ProviderKind::DeepSeek,
            vec![chat],
            false,
            Some((53, 17)),
            true,
        ),
        (
            ProviderKind::DeepSeek,
            vec![chat_stop, chat_usage],
            false,
            Some((67, 23)),
            true,
        ),
        (
            ProviderKind::OpenAi,
            vec![
                json!({"type":"response.refusal.delta","delta":"refused"}),
                completed(json!({"input_tokens":71,"output_tokens":29})),
            ],
            false,
            Some((71, 29)),
            true,
        ),
        (
            ProviderKind::Anthropic,
            vec![start()],
            false,
            Some((29, 1)),
            false,
        ),
        (
            ProviderKind::Anthropic,
            vec![
                start(),
                json!({"type":"error","error":{"message":"private-remote-diagnostic"}}),
            ],
            false,
            Some((29, 1)),
            false,
        ),
    ] {
        let server = MockServer::start().await;
        let template = if kind.is_chat_completions() {
            let stream: String = fixture
                .iter()
                .map(|value| format!("data: {value}\n\n"))
                .collect();
            ResponseTemplate::new(200).set_body_raw(stream, "text/event-stream")
        } else {
            template(&fixture)
        };
        Mock::given(method("POST"))
            .respond_with(template)
            .expect(1)
            .mount(&server)
            .await;
        let mut settings = options(&server.uri());
        settings.kind = kind;
        settings.model = "accounting-model".into();
        let accounting = RequestAccounting::default();
        let result = Provider::new(settings)?
            .respond_accounted(
                request().await?,
                CancellationToken::new(),
                accounting.clone(),
            )
            .await;
        assert_eq!(result.is_ok(), successful, "{kind}: {result:?}");
        if let Err(error) = &result {
            assert!(!error.to_string().contains("private-remote-diagnostic"));
        }
        let summary = accounting.summary()?;
        assert_eq!(summary.requests_started, 1);
        assert_eq!(summary.requests_finished, 1);
        assert_eq!(summary.requests_with_usage, u64::from(counts.is_some()));
        assert_eq!(summary.requests_with_final_usage, u64::from(complete));
        assert_eq!(
            summary
                .reported_usage
                .as_ref()
                .map(|usage| (usage.input_tokens, usage.output_tokens)),
            counts
        );
        assert_eq!(summary.usage.is_some(), complete);
    }
    Ok(())
}

#[tokio::test]
async fn request_and_wire_decoder_share_existing_byte_limits() -> Result<(), Box<dyn Error>> {
    let server = MockServer::start().await;
    let provider = Provider::new(options(&server.uri()))?;
    let mut oversized = request().await?;
    oversized.context.instructions = "x".repeat(64 * 1024 * 1024);
    assert!(
        provider
            .respond(oversized, CancellationToken::new())
            .await
            .is_err()
    );
    assert!(
        server
            .received_requests()
            .await
            .ok_or("missing requests")?
            .is_empty()
    );
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await?;
        receive_request(&mut socket).await?;
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 67117056\r\nConnection: close\r\n\r\n").await?;
        let comment = format!(":{}\n\n", "x".repeat(8189));
        for _ in 0..8193 {
            if let Err(error) = socket.write_all(comment.as_bytes()).await {
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                ) {
                    return Ok(());
                }
                return Err(error.into());
            }
        }
        Ok::<(), Box<dyn Error + Send + Sync>>(())
    });
    let mut request = request().await?;
    request.limits.deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let failure = Provider::new(options(&format!("http://{address}")))?
        .respond(request, CancellationToken::new())
        .await
        .err()
        .ok_or("oversized body accepted")?;
    assert!(matches!(failure, ModelError::Protocol { .. }));
    assert!(format!("{failure}").contains("byte limit"));
    tokio::time::timeout(Duration::from_secs(3), task)
        .await??
        .map_err(|error| -> Box<dyn Error> { error })?;
    Ok(())
}

#[tokio::test]
async fn turn_cancellation_preserves_provenance_and_runtime_notice() -> Result<(), Box<dyn Error>> {
    use cocoa_core::{execution::RuntimeState, store::InvocationOutcome};
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(template(&events(vec![text_block("continued")], "end_turn")))
        .expect(2)
        .mount(&server)
        .await;
    let provider = Provider::new(options(&server.uri()))?;
    for (runtime, notice) in [
        (RuntimeState::Reset, "retained bindings are gone"),
        (RuntimeState::Preserved, "bindings were preserved"),
    ] {
        let mut request = request().await?;
        let invocation = request.context.records[0].invocation;
        let mut foreign = request.clone();
        foreign.context.records.push(Record {
            invocation: InvocationId(invocation.0 + 1),
            kind: RecordKind::TurnCancelled { runtime },
        });
        assert!(
            provider
                .respond(foreign, CancellationToken::new())
                .await
                .is_err()
        );
        let mut foreign = request.clone();
        foreign.context.records.push(Record {
            invocation: InvocationId(invocation.0 + 1),
            kind: RecordKind::ModelCompleted {
                shared_context: None,
                response: ModelResponse {
                    action: ModelAction::Final {
                        answer: "foreign".into(),
                    },
                    continuation: None,
                    usage: None,
                },
            },
        });
        assert!(
            provider
                .respond(foreign, CancellationToken::new())
                .await
                .is_err()
        );
        request.context.records.push(Record {
            invocation,
            kind: RecordKind::TurnCancelled { runtime },
        });
        request.context.records.push(Record {
            invocation,
            kind: RecordKind::InvocationFinished {
                outcome: InvocationOutcome::Cancelled,
            },
        });
        request.context.records.push(Record {
            invocation: InvocationId(invocation.0 + 1),
            kind: RecordKind::InputAccepted {
                input: "continue".into(),
            },
        });
        provider.respond(request, CancellationToken::new()).await?;
        let requests = server.received_requests().await.ok_or("missing requests")?;
        let body: Value = serde_json::from_slice(&requests.last().ok_or("missing request")?.body)?;
        assert!(
            body["messages"][1]["content"]
                .as_str()
                .is_some_and(|text| text.contains(notice))
        );
        assert_eq!(body["messages"][2]["content"][0]["text"], "continue");
    }
    Ok(())
}

#[tokio::test]
async fn cache_sharing_summaries_reject_tool_calls_despite_retaining_definitions()
-> Result<(), Box<dyn Error>> {
    for kind in [ProviderKind::OpenAi, ProviderKind::Anthropic] {
        let server = MockServer::start().await;
        let stream = if kind == ProviderKind::Anthropic {
            encode(&events(
                vec![call("forbidden", "print('must not execute')")],
                "tool_use",
            ))
        } else {
            format!(
                "event: response.completed\ndata: {}\n\n",
                json!({"type":"response.completed","response":{"id":"answer","status":"completed","output":[
                    {"id":"call","type":"function_call","call_id":"forbidden","name":"exec","arguments":"{\"code\":\"print('must not execute')\"}","status":"completed"}
                ]}})
            )
        };
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(stream, "text/event-stream"))
            .expect(1)
            .mount(&server)
            .await;
        let mut options = options(&server.uri());
        options.kind = kind;
        let provider = Provider::new(options)?;
        let context = request().await?;
        let summary = cocoa_core::model::SummaryRequest {
            input: "{}".into(),
            output_tokens: 73,
            output_bytes: 4096,
            deadline: tokio::time::Instant::now() + Duration::from_secs(5),
        };
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(matches!(
            provider
                .summarize_with_context(summary.clone(), &context, 1024 * 1024, cancelled)
                .await,
            Err(ModelError::Cancelled)
        ));
        assert!(matches!(
            provider
                .summarize_with_context(summary, &context, 1024 * 1024, CancellationToken::new())
                .await,
            Err(ModelError::Protocol { .. })
        ));
        let received = server.received_requests().await.ok_or("missing requests")?;
        let body: Value = received[0].body_json()?;
        assert!(body.get("tools").is_some());
    }
    Ok(())
}

#[tokio::test]
async fn request_accounting_matches_encoded_media_checkpoint_shared_data_tools_and_opaque_replay()
-> Result<(), Box<dyn Error>> {
    use cocoa_core::{
        compaction::{CheckpointContent, Recap},
        context::{ContextContent, ContextRecord, ContextType, SharedContext},
        store::AgentId,
    };
    use cocoa_llm::openai::{OpenAiOptions, OpenAiProvider};

    for kind in [
        ProviderKind::OpenAi,
        ProviderKind::Anthropic,
        ProviderKind::Nvidia,
        ProviderKind::Ollama,
    ] {
        let server = MockServer::start().await;
        let stream = if kind == ProviderKind::Anthropic {
            encode(&events(
                vec![
                    json!({"type":"thinking","thinking":"opaque reasoning","signature":"signed"}),
                    text_block("done"),
                ],
                "end_turn",
            ))
        } else if matches!(kind, ProviderKind::Nvidia | ProviderKind::Ollama) {
            format!(
                "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                json!({"id":"answer","choices":[{"index":0,"delta":{"role":"assistant","content":"done","reasoning":"opaque reasoning"},"finish_reason":null}]}),
                json!({"id":"answer","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})
            )
        } else {
            format!(
                "event: response.completed\ndata: {}\n\n",
                json!({"type":"response.completed","response":{"id":"answer","status":"completed","output":[
                    {"id":"reasoning","type":"reasoning","summary":[],"encrypted_content":"opaque reasoning","unknown":{"preserved":"é"}},
                    {"id":"message","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"done"}]}
                ]}})
            )
        };
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(stream, "text/event-stream"))
            .expect(5)
            .mount(&server)
            .await;
        let mut options = options(&server.uri());
        options.kind = kind;
        options.model = "accounted-model".into();
        options.endpoint = format!("{}/v1/responses", server.uri());
        options.api_key = (kind != ProviderKind::Ollama).then(|| "fixture-api-key".into());
        if kind == ProviderKind::OpenAi {
            options.session_id = "legacy-openai".into();
        }
        let provider = Provider::new(options)?;
        let (publisher, mut live) = EventPublisher::channel(64, 4096)?;
        let mut request = request_with_events(Some(publisher)).await?;
        request.context.instructions = "policy with escaped \0\n\"\\ and é".into();
        request.context.capabilities.push(CapabilityDescription {
            name: "accounted".into(),
            description: "count this description".into(),
            parameters: json!({"type":"object"}),
        });
        request.context.records[0].kind = RecordKind::InputAccepted {
            input: UserInput::WithImages {
                text: "describe \"é\"".into(),
                images: vec![Image::from_png(b"\x89PNG\r\n\x1a\n")?],
            },
        };
        request.context.checkpoint = Some(CheckpointContent {
            through: RecordId::try_from(1)?,
            predecessor: None,
            required: Vec::new(),
            recap: Recap {
                goal: "checkpoint goal".into(),
                stages: Vec::new(),
                current_stage: None,
                decisions: Vec::new(),
                progress: Vec::new(),
                failures: Vec::new(),
                next_actions: Vec::new(),
            },
            evidence: vec![RecordId::try_from(1)?],
            unresolved_effects: Vec::new(),
        });
        request.context.shared = Some(SharedContext {
            records: vec![ContextRecord {
                key: "shared".into(),
                owner: AgentId::try_from(1)?,
                content: ContextContent {
                    kind: ContextType::Observation,
                    value: json!({"quoted":"\0\"é"}),
                    labels: vec!["current".into()],
                },
            }],
            omitted: 3,
        });
        let first = provider
            .request_bytes(&request)?
            .ok_or("missing accounting")?;
        if kind == ProviderKind::OpenAi {
            let standalone = OpenAiProvider::new(OpenAiOptions {
                endpoint: format!("{}/v1/responses", server.uri()),
                model: "accounted-model".into(),
                api_key: "fixture-api-key".into(),
                execution: ExecutionMode::Enabled,
                thinking_level: None,
            })?;
            assert_eq!(standalone.request_bytes(&request)?, Some(first));
        }
        assert!(
            server
                .received_requests()
                .await
                .ok_or("missing requests")?
                .is_empty()
        );
        let response = provider
            .respond(request.clone(), CancellationToken::new())
            .await?;
        let shared_context = request.context.shared_update().cloned();
        request.context.records.push(Record {
            invocation: request.context.records[0].invocation,
            kind: RecordKind::ModelCompleted {
                response,
                shared_context,
            },
        });
        let second = provider
            .request_bytes(&request)?
            .ok_or("missing replay accounting")?;
        assert!(second > first, "{kind}");
        let mut invalid = request.clone();
        let RecordKind::ModelCompleted { response, .. } = &mut invalid
            .context
            .records
            .last_mut()
            .ok_or("missing response")?
            .kind
        else {
            return Err("missing response".into());
        };
        response
            .continuation
            .as_mut()
            .ok_or("missing continuation")?
            .provider = "foreign".into();
        assert!(matches!(
            provider.request_bytes(&invalid),
            Err(ModelError::Protocol { .. })
        ));
        let mut invalid = request.clone();
        let RecordKind::ModelCompleted { response, .. } = &mut invalid
            .context
            .records
            .last_mut()
            .ok_or("missing response")?
            .kind
        else {
            return Err("missing response".into());
        };
        response.action = ModelAction::Final {
            answer: "mismatched action".into(),
        };
        assert!(matches!(
            provider.request_bytes(&invalid),
            Err(ModelError::Protocol { .. })
        ));
        for _ in 0..64 {
            match tokio::time::timeout(Duration::from_millis(1), live.recv()).await {
                Err(_) => break,
                Ok(Observation::Closed) => return Err("live observations closed early".into()),
                Ok(_) => {}
            }
        }
        let summary_request = cocoa_core::model::SummaryRequest {
            input: "{}".into(),
            output_tokens: 73,
            output_bytes: 4,
            deadline: tokio::time::Instant::now() + Duration::from_secs(5),
        };
        assert_eq!(
            provider
                .summarize_with_context(
                    summary_request.clone(),
                    &request,
                    1024 * 1024,
                    CancellationToken::new(),
                )
                .await?,
            "done"
        );
        assert_eq!(
            provider
                .summarize(summary_request.clone(), CancellationToken::new())
                .await?,
            "done"
        );
        let received = server.received_requests().await.ok_or("missing requests")?;
        let incremental_bytes = received[2].body.len();
        assert_eq!(
            provider
                .summarize_with_context(
                    summary_request.clone(),
                    &request,
                    incremental_bytes,
                    CancellationToken::new(),
                )
                .await?,
            "done"
        );
        for input_bytes in [0, incremental_bytes - 1] {
            assert!(matches!(
                provider
                    .summarize_with_context(
                        summary_request.clone(),
                        &request,
                        input_bytes,
                        CancellationToken::new(),
                    )
                    .await,
                Err(ModelError::Protocol { .. })
            ));
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(10), live.recv())
                .await
                .is_err()
        );
        provider.respond(request, CancellationToken::new()).await?;
        let received = server.received_requests().await.ok_or("missing requests")?;
        assert_eq!(received.len(), 5);
        assert_eq!(received[2].body, received[3].body, "{kind}");
        let summary: Value = received[1].body_json()?;
        let ordinary: Value = received[4].body_json()?;
        let initial: Value = received[0].body_json()?;
        let field = if kind == ProviderKind::OpenAi {
            "input"
        } else {
            "messages"
        };
        let prefix = initial[field]
            .as_array()
            .ok_or("missing initial messages")?;
        let replay = ordinary[field]
            .as_array()
            .ok_or("missing replay messages")?;
        assert_eq!(
            &replay[..prefix.len()],
            prefix,
            "{kind}: shared delivery moved"
        );
        assert_eq!(
            replay.len() - prefix.len(),
            if kind == ProviderKind::OpenAi { 2 } else { 1 }
        );
        if kind == ProviderKind::OpenAi {
            for field in [
                "instructions",
                "tools",
                "parallel_tool_calls",
                "reasoning",
                "prompt_cache_key",
            ] {
                assert_eq!(summary[field], ordinary[field], "{field}");
            }
            let prefix = ordinary["input"].as_array().ok_or("missing input")?;
            let fork = summary["input"].as_array().ok_or("missing summary input")?;
            assert_eq!(&fork[..prefix.len()], prefix);
            assert_eq!(fork.len(), prefix.len() + 2);
            assert_eq!(fork[prefix.len()]["role"], "developer");
            assert_eq!(
                fork[prefix.len()]["content"],
                cocoa_core::compaction::SUMMARY_INSTRUCTIONS
            );
            assert_eq!(
                fork[prefix.len() + 1],
                json!({"role":"user","content":"{}"})
            );
            assert_eq!(summary["tool_choice"], "none");
            assert_eq!(summary["max_output_tokens"], 73);
        } else if kind == ProviderKind::Anthropic {
            assert_eq!(summary["system"][0], ordinary["system"][0]);
            assert_eq!(
                summary["system"][0]["cache_control"],
                json!({"type":"ephemeral"})
            );
            assert_eq!(
                summary["system"][1]["text"],
                cocoa_core::compaction::SUMMARY_INSTRUCTIONS
            );
            assert_eq!(summary["tools"], ordinary["tools"]);
            assert_eq!(summary["thinking"], ordinary["thinking"]);
            assert_eq!(summary["tool_choice"], json!({"type":"none"}));
            assert_eq!(summary["messages"], json!([{"role":"user","content":"{}"}]));
            assert_eq!(summary["max_tokens"], 73);
        } else {
            assert_eq!(received[1].body, received[2].body);
            assert!(summary.get("tools").is_none());
        }
        for (sent, measured) in [&received[0], &received[4]]
            .into_iter()
            .zip([first, second])
        {
            assert_eq!(sent.body.len(), measured, "{kind}");
            let body: Value = sent.body_json()?;
            assert!(body.get("tools").is_some());
            let encoded = String::from_utf8(sent.body.clone())?;
            assert!(encoded.contains("checkpoint_projection"));
            assert!(encoded.contains("agent_authored"));
            assert!(encoded.contains("iVBOR"));
        }
        assert!(String::from_utf8(received[4].body.clone())?.contains("opaque reasoning"));
    }
    Ok(())
}
