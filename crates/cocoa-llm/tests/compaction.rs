//! Read-only checkpoint transport contracts against local HTTP fixtures.

use std::{error::Error, sync::Arc, time::Duration};

use async_trait::async_trait;
use cocoa_core::{
    compaction::{CheckpointProposal, Recap},
    model::{ModelError, ModelProvider, SummaryRequest},
};
use cocoa_llm::{
    CredentialResolver, ModelLimits, Provider, ProviderKind, ProviderOptions,
    openai::{ExecutionMode, OpenAiOptions, OpenAiProvider, ThinkingLevel},
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use tokio_util::sync::CancellationToken;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

const PROFILES: [ProviderKind; 4] = [
    ProviderKind::OpenAi,
    ProviderKind::Anthropic,
    ProviderKind::DeepSeek,
    ProviderKind::Ollama,
];

#[tokio::test]
async fn summaries_are_exact_read_only_and_use_selected_model_credentials_and_thinking()
-> Result<(), Box<dyn Error>> {
    let proposal = " \n{\"kept\":\"é\",\"duplicate\":1,\"duplicate\":2}\t";
    for kind in PROFILES {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(template(success(kind, proposal)))
            .expect(1)
            .mount(&server)
            .await;
        let options = options(kind, &server.uri());
        let model = options.model.clone();
        let mut provider = Provider::new(options)?;
        provider.set_model_limits(ModelLimits {
            output_tokens: Some(45678),
        })?;
        let request = request(proposal.len());
        let input = request.input.clone();
        assert_eq!(
            provider
                .summarize(request, CancellationToken::new())
                .await?,
            proposal
        );
        let received = server.received_requests().await.ok_or("missing requests")?;
        assert_eq!(received.len(), 1);
        let sent = &received[0];
        if kind == ProviderKind::Anthropic {
            assert_eq!(sent.headers["x-api-key"], "summary-secret-never-log");
        } else if kind == ProviderKind::Ollama {
            assert!(!sent.headers.contains_key("authorization"));
        } else {
            assert_eq!(
                sent.headers["authorization"],
                "Bearer summary-secret-never-log"
            );
        }
        let body: Value = sent.body_json()?;
        assert_eq!(body["model"], model);
        assert_eq!(body["stream"], true);
        for absent in ["tools", "tool_choice", "parallel_tool_calls"] {
            assert!(body.get(absent).is_none(), "{kind}: {absent}");
        }
        let prompt = match kind {
            ProviderKind::OpenAi => {
                assert_eq!(body["max_output_tokens"], 73);
                assert_eq!(body["reasoning"]["effort"], "high");
                assert_eq!(body["input"], json!([{"role":"user","content":input}]));
                assert_eq!(body["store"], false);
                assert_eq!(body["prompt_cache_key"], "summary-fixture");
                &body["instructions"]
            }
            ProviderKind::Anthropic => {
                assert_eq!(body["max_tokens"], 73);
                assert_eq!(body["cache_control"], json!({"type":"ephemeral"}));
                assert_eq!(body["thinking"]["type"], "adaptive");
                assert_eq!(body["output_config"]["effort"], "high");
                assert_eq!(body["messages"], json!([{"role":"user","content":input}]));
                assert_eq!(
                    body["system"][0]["cache_control"],
                    json!({"type":"ephemeral"})
                );
                &body["system"][0]["text"]
            }
            _ => {
                assert_eq!(body["max_tokens"], 73);
                assert_eq!(body["messages"].as_array().map(Vec::len), Some(2));
                assert_eq!(body["messages"][1], json!({"role":"user","content":input}));
                if kind == ProviderKind::DeepSeek {
                    assert_eq!(body["reasoning_effort"], "high");
                } else {
                    assert!(body.get("reasoning_effort").is_none());
                }
                &body["messages"][0]["content"]
            }
        }
        .as_str()
        .ok_or("missing checkpoint instructions")?;
        assert!(!prompt.contains("exec("));
        assert!(!prompt.contains("private REPL"));
        let example: Value = serde_json::from_str(
            prompt
                .lines()
                .find(|line| line.starts_with("{\"additions\":"))
                .ok_or("missing minimal example")?,
        )?;
        assert_eq!(
            example,
            serde_json::to_value(CheckpointProposal {
                additions: Vec::new(),
                changes: Vec::new(),
                recap: Recap {
                    goal: "Preserve the user's goal".into(),
                    stages: Vec::new(),
                    current_stage: None,
                    decisions: Vec::new(),
                    progress: Vec::new(),
                    failures: Vec::new(),
                    next_actions: Vec::new(),
                },
                evidence: Vec::new(),
            })?
        );
    }
    Ok(())
}

#[tokio::test]
async fn standalone_openai_provider_implements_summary_transport() -> Result<(), Box<dyn Error>> {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(template(success(ProviderKind::OpenAi, "{}")))
        .expect(1)
        .mount(&server)
        .await;
    let provider = OpenAiProvider::new(OpenAiOptions {
        endpoint: format!("{}/v1/responses", server.uri()),
        model: "selected-model".into(),
        api_key: "summary-secret-never-log".into(),
        execution: ExecutionMode::Enabled,
        thinking_level: Some(ThinkingLevel::High),
    })?;
    assert_eq!(
        provider
            .summarize(request(2), CancellationToken::new())
            .await?,
        "{}"
    );
    Ok(())
}

#[tokio::test]
async fn summary_bytes_are_utf8_bounded_independently_of_tokens_and_reasoning()
-> Result<(), Box<dyn Error>> {
    for kind in PROFILES {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(template(success(kind, "é")))
            .expect(2)
            .mount(&server)
            .await;
        let provider = Provider::new(options(kind, &server.uri()))?;
        assert_eq!(
            provider
                .summarize(request(2), CancellationToken::new())
                .await?,
            "é"
        );
        let result = provider
            .summarize(request(1), CancellationToken::new())
            .await;
        assert!(
            matches!(result, Err(ModelError::Protocol { ref message }) if message.contains("output_text")),
            "{kind}: {result:?}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn summaries_reject_calls_length_stops_and_missing_success() -> Result<(), Box<dyn Error>> {
    for kind in PROFILES {
        for failure in ["calls", "length", "nonterminal"] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(template(failure_stream(kind, failure)))
                .expect(1)
                .mount(&server)
                .await;
            let provider = Provider::new(options(kind, &server.uri()))?;
            let result = provider
                .summarize(request(4096), CancellationToken::new())
                .await;
            assert!(
                matches!(result, Err(ModelError::Protocol { .. })),
                "{kind}: {failure}: {result:?}"
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn oversized_streamed_text_is_rejected_before_a_terminal_event() -> Result<(), Box<dyn Error>>
{
    for kind in PROFILES {
        let stream = if kind == ProviderKind::Anthropic {
            messages_prefix(json!({"type":"text","text":"éx"}))
        } else if is_chat(kind) {
            chat(json!({"content":"éx"}), Value::Null)
        } else {
            event(json!({"type":"response.output_text.delta","delta":"éx"}))
        };
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(template(stream))
            .expect(1)
            .mount(&server)
            .await;
        let provider = Provider::new(options(kind, &server.uri()))?;
        let result = provider
            .summarize(request(2), CancellationToken::new())
            .await;
        assert!(
            matches!(result, Err(ModelError::Protocol { ref message }) if message.contains("output_text")),
            "{kind}: {result:?}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn summary_retries_keep_the_same_read_only_request_and_sanitized_errors()
-> Result<(), Box<dyn Error>> {
    for kind in PROFILES {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(503).insert_header("retry-after", "0"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(template(success(kind, "{}")))
            .mount(&server)
            .await;
        let provider = Provider::new(options(kind, &server.uri()))?;
        assert_eq!(
            provider
                .summarize(request(2), CancellationToken::new())
                .await?,
            "{}"
        );
        let received = server.received_requests().await.ok_or("missing requests")?;
        assert_eq!(received.len(), 2);
        assert_eq!(received[0].body, received[1].body);
        let body: Value = received[1].body_json()?;
        assert!(body.get("tools").is_none());
        server.reset().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_body_string("summary-secret-never-log remote secret"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let error = provider
            .summarize(request(4096), CancellationToken::new())
            .await
            .err()
            .ok_or("expected HTTP failure")?;
        assert!(matches!(error, ModelError::Rejected { .. }));
        assert!(!format!("{error:?} {error}").contains("secret"));
    }
    Ok(())
}

#[tokio::test]
async fn summary_deadlines_and_cancellation_cover_headers_and_retry_waits()
-> Result<(), Box<dyn Error>> {
    for kind in PROFILES {
        for retry in [false, true] {
            for cancel in [false, true] {
                let server = MockServer::start().await;
                let response = if retry {
                    ResponseTemplate::new(503).insert_header("retry-after", "2")
                } else {
                    template(success(kind, "{}")).set_delay(Duration::from_secs(2))
                };
                Mock::given(method("POST"))
                    .respond_with(response)
                    .expect(1)
                    .mount(&server)
                    .await;
                let provider = Provider::new(options(kind, &server.uri()))?;
                let mut request = request(4096);
                request.deadline = tokio::time::Instant::now()
                    + if cancel {
                        Duration::from_secs(2)
                    } else {
                        Duration::from_millis(100)
                    };
                let cancellation = CancellationToken::new();
                let stop = async {
                    if cancel {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        cancellation.cancel();
                    }
                };
                let started = tokio::time::Instant::now();
                let (result, ()) =
                    tokio::join!(provider.summarize(request, cancellation.clone()), stop);
                if cancel {
                    assert!(
                        matches!(result, Err(ModelError::Cancelled)),
                        "{kind}: {retry}: {result:?}"
                    );
                } else {
                    assert!(
                        matches!(result, Err(ModelError::Stalled)),
                        "{kind}: {retry}: {result:?}"
                    );
                }
                assert!(started.elapsed() < Duration::from_secs(1));
                assert_eq!(
                    server
                        .received_requests()
                        .await
                        .ok_or("missing requests")?
                        .len(),
                    1
                );
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn summary_absolute_deadline_and_cancellation_stop_a_progressing_body()
-> Result<(), Box<dyn Error>> {
    for kind in PROFILES {
        for cancel in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").await?;
            let base = format!("http://{}", listener.local_addr()?);
            let (streaming, started) = tokio::sync::oneshot::channel();
            let fixture = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await?;
                let mut received = Vec::new();
                let mut buffer = [0u8; 8192];
                while !received.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let count = socket.read(&mut buffer).await?;
                    if count == 0 || received.len() + count > 128 * 1024 {
                        return Err(std::io::Error::other("missing bounded HTTP headers"));
                    }
                    received.extend_from_slice(&buffer[..count]);
                }
                socket.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n: progress\n\n").await?;
                if streaming.send(()).is_err() {
                    return Err(std::io::Error::other("missing fixture observer"));
                }
                for _ in 0..40 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    if socket.write_all(b": progress\n\n").await.is_err() {
                        break;
                    }
                }
                Ok::<_, std::io::Error>(())
            });
            let provider = Provider::new(options(kind, &base))?;
            let mut request = request(4096);
            request.deadline = tokio::time::Instant::now()
                + if cancel {
                    Duration::from_secs(2)
                } else {
                    Duration::from_millis(150)
                };
            let cancellation = CancellationToken::new();
            let stop = async {
                tokio::time::timeout(Duration::from_secs(1), started).await??;
                if cancel {
                    tokio::time::sleep(Duration::from_millis(40)).await;
                    cancellation.cancel();
                }
                Ok::<_, Box<dyn Error>>(())
            };
            let (result, stop_result) =
                tokio::join!(provider.summarize(request, cancellation.clone()), stop);
            stop_result?;
            if cancel {
                assert!(
                    matches!(result, Err(ModelError::Cancelled)),
                    "{kind}: {result:?}"
                );
            } else {
                assert!(
                    matches!(result, Err(ModelError::Stalled)),
                    "{kind}: {result:?}"
                );
            }
            tokio::time::timeout(Duration::from_secs(2), fixture).await???;
        }
    }
    Ok(())
}

#[tokio::test]
async fn invalid_budgets_expiry_and_precancellation_make_no_http_call() -> Result<(), Box<dyn Error>>
{
    for kind in PROFILES {
        let server = MockServer::start().await;
        let provider = Provider::new(options(kind, &server.uri()))?;
        for (tokens, bytes) in [(0, 1), (1, 0)] {
            let mut request = request(bytes);
            request.output_tokens = tokens;
            assert!(matches!(
                provider.summarize(request, CancellationToken::new()).await,
                Err(ModelError::Protocol { .. })
            ));
        }
        let mut expired = request(4096);
        expired.deadline = tokio::time::Instant::now();
        assert!(matches!(
            provider.summarize(expired, CancellationToken::new()).await,
            Err(ModelError::Stalled)
        ));
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(matches!(
            provider.summarize(request(4096), cancelled).await,
            Err(ModelError::Cancelled)
        ));
        assert!(
            server
                .received_requests()
                .await
                .ok_or("missing requests")?
                .is_empty()
        );
    }
    Ok(())
}

#[tokio::test]
async fn subscription_summary_is_unsupported_without_resolving_credentials()
-> Result<(), Box<dyn Error>> {
    struct UnusedCredentials;
    #[async_trait]
    impl CredentialResolver for UnusedCredentials {
        fn identity(&self) -> &str {
            "summary-fixture"
        }
        async fn resolve(&self) -> Result<secrecy::SecretString, ModelError> {
            panic!("unsupported summary must not resolve credentials")
        }
    }
    let mut options = options(ProviderKind::OpenAi, "unused");
    options.kind = ProviderKind::ChatGpt;
    options.endpoint = ProviderKind::ChatGpt.endpoint_default().into();
    options.api_key = None;
    let provider = Provider::chatgpt(options, Arc::new(UnusedCredentials))?;
    let result = provider
        .summarize(request(4096), CancellationToken::new())
        .await;
    assert!(
        matches!(result, Err(ModelError::Protocol { ref message }) if message.contains("unsupported"))
    );
    Ok(())
}

fn options(kind: ProviderKind, base: &str) -> ProviderOptions {
    ProviderOptions {
        kind,
        endpoint: format!("{base}/v1/responses"),
        model: match kind {
            ProviderKind::Anthropic => "claude-opus-4-6",
            ProviderKind::DeepSeek => "deepseek-pro",
            _ => "selected-model",
        }
        .into(),
        api_key: (kind != ProviderKind::Ollama).then(|| "summary-secret-never-log".into()),
        execution: ExecutionMode::Enabled,
        thinking_level: (kind != ProviderKind::Ollama).then_some(ThinkingLevel::High),
        session_id: "summary-fixture".into(),
    }
}

fn request(output_bytes: usize) -> SummaryRequest {
    SummaryRequest {
        input: "{\"previous\":null,\"sources\":[{\"id\":1,\"text\":\"Keep the goal\"}]}".into(),
        output_tokens: 73,
        output_bytes,
        deadline: tokio::time::Instant::now() + Duration::from_secs(5),
    }
}

fn success(kind: ProviderKind, answer: &str) -> String {
    let offset = answer
        .char_indices()
        .nth(1)
        .map_or(answer.len(), |(offset, _)| offset);
    let (first, second) = answer.split_at(offset);
    if kind == ProviderKind::Anthropic {
        messages_prefix(json!({"type":"thinking","thinking":"r".repeat(128),"signature":"signed"}))
            + &event(json!({"type":"content_block_stop","index":0}))
            + &event(
                json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":first}}),
            )
            + &event(
                json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":second}}),
            )
            + &event(json!({"type":"content_block_stop","index":1}))
            + &messages_terminal("end_turn")
    } else if is_chat(kind) {
        chat(
            json!({"reasoning":"r".repeat(128),"content":first}),
            Value::Null,
        ) + &chat(json!({"content":second}), Value::Null)
            + &chat(json!({}), json!("stop"))
            + "data: [DONE]\n\n"
    } else {
        responses(
            json!([
                {"id":"reasoning","type":"reasoning","summary":[],"encrypted_content":"r".repeat(128)},
                {"id":"answer","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":answer}]}
            ]),
            "completed",
        )
    }
}

fn failure_stream(kind: ProviderKind, failure: &str) -> String {
    if kind == ProviderKind::Anthropic {
        let block = if failure == "calls" {
            json!({"type":"tool_use","id":"call","name":"exec","input":{"code":"print(1)"}})
        } else {
            json!({"type":"text","text":"{}"})
        };
        let prefix =
            messages_prefix(block) + &event(json!({"type":"content_block_stop","index":0}));
        if failure == "nonterminal" {
            return prefix;
        }
        prefix
            + &messages_terminal(if failure == "calls" {
                "tool_use"
            } else {
                "max_tokens"
            })
    } else if is_chat(kind) {
        let delta = if failure == "calls" {
            json!({"content":"{}","tool_calls":[{"index":0,"id":"call","type":"function","function":{"name":"exec","arguments":"{\"code\":\"print(1)\"}"}}]})
        } else {
            json!({"content":"{}"})
        };
        let prefix = chat(delta, Value::Null);
        if failure == "nonterminal" {
            return prefix + "data: [DONE]\n\n";
        }
        prefix
            + &chat(
                json!({}),
                json!(if failure == "calls" {
                    "tool_calls"
                } else {
                    "length"
                }),
            )
            + "data: [DONE]\n\n"
    } else if failure == "calls" {
        responses(
            json!([{"id":"call","type":"function_call","call_id":"call","name":"exec","arguments":"{\"code\":\"print(1)\"}","status":"completed"}]),
            "completed",
        )
    } else if failure == "length" {
        responses(
            json!([{"id":"answer","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"{}"}]}]),
            "incomplete",
        )
    } else {
        event(json!({"type":"response.output_text.delta","delta":"{}"}))
    }
}

fn messages_prefix(block: Value) -> String {
    event(
        json!({"type":"message_start","message":{"type":"message","id":"summary","role":"assistant","content":[],"stop_reason":null,"usage":{"input_tokens":1,"output_tokens":0}}}),
    ) + &event(json!({"type":"content_block_start","index":0,"content_block":block}))
}

fn messages_terminal(reason: &str) -> String {
    event(
        json!({"type":"message_delta","delta":{"stop_reason":reason,"stop_sequence":null},"usage":{"output_tokens":2}}),
    ) + &event(json!({"type":"message_stop"}))
}

fn responses(output: Value, status: &str) -> String {
    event(
        json!({"type":"response.completed","response":{"id":"summary","status":status,"error":null,"incomplete_details":null,"output":output}}),
    )
}

fn chat(delta: Value, reason: Value) -> String {
    format!(
        "data: {}\n\n",
        json!({"id":"summary","choices":[{"index":0,"delta":delta,"finish_reason":reason}]})
    )
}

fn event(value: Value) -> String {
    format!(
        "event: {}\ndata: {value}\n\n",
        value["type"].as_str().unwrap_or("message")
    )
}

fn template(body: String) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

fn is_chat(kind: ProviderKind) -> bool {
    matches!(kind, ProviderKind::DeepSeek | ProviderKind::Ollama)
}
