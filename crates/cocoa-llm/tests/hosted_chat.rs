//! Hosted Chat Completions contracts exercised through the public client and HTTP.

use async_trait::async_trait;
use cocoa_core::{
    capability::{CapabilityDescription, CapabilityError, CapabilityHost, OperationContext},
    context::BoundedContext,
    engine::{
        Engine, EngineAdapters, EngineSettings, EventKind, EventPublisher, Limits, Observation,
        RunError, RunRequest,
    },
    execution::{Cell, CellExecutor, CellResponse, ExecutionError, ShutdownReport, WorkLimits},
    model::{Image, ModelError, ModelProvider, ModelRequest, ModelResponse, UserInput},
    store::{InMemoryStore, InvocationId, RecordKind, StoreLimits},
};
use cocoa_llm::{
    Provider, ProviderKind, ProviderOptions,
    openai::{ExecutionMode, ThinkingLevel},
};
use serde_json::{Value, json};
use std::{
    error::Error,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use tokio_util::sync::CancellationToken;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const PROFILES: [ProviderKind; 17] = [
    ProviderKind::DeepSeek,
    ProviderKind::Cerebras,
    ProviderKind::Nvidia,
    ProviderKind::HuggingFace,
    ProviderKind::AntLing,
    ProviderKind::MoonshotAi,
    ProviderKind::MoonshotAiCn,
    ProviderKind::Zai,
    ProviderKind::ZaiCodingCn,
    ProviderKind::QwenTokenPlan,
    ProviderKind::QwenTokenPlanCn,
    ProviderKind::QwenTokenPlanIndividual,
    ProviderKind::QwenTokenPlanIndividualCn,
    ProviderKind::Xiaomi,
    ProviderKind::XiaomiTokenPlanCn,
    ProviderKind::XiaomiTokenPlanAms,
    ProviderKind::XiaomiTokenPlanSgp,
];

#[tokio::test]
async fn chat_cache_usage_preserves_unknowns_in_both_usage_event_forms()
-> Result<(), Box<dyn Error>> {
    for kind in [ProviderKind::DeepSeek, ProviderKind::Ollama] {
        for inline in [false, true] {
            if inline && kind == ProviderKind::Ollama {
                continue;
            }
            for (details, read_tokens) in [
                (None, None),
                (Some(Value::Null), None),
                (Some(json!({})), None),
                (Some(json!({"cached_tokens":null})), None),
                (Some(json!({"audio_tokens":3})), None),
                (Some(json!({"cached_tokens":11})), Some(11)),
                (Some(json!({"cached_tokens":0})), Some(0)),
                (Some(json!({"cached_tokens":17})), Some(17)),
            ] {
                let mut usage = json!({"prompt_tokens":17,"completion_tokens":9});
                if let Some(details) = details {
                    usage["prompt_tokens_details"] = details;
                }
                let mut completion: Value = serde_json::from_str(
                    chunk(json!({"role":"assistant","content":"done"}), Value::Null)
                        .trim_start_matches("data: ")
                        .trim(),
                )?;
                let body = if inline {
                    completion["choices"][0]["finish_reason"] = json!("stop");
                    completion["usage"] = usage;
                    format!("data: {completion}\n\ndata: [DONE]\n\n")
                } else {
                    format!("data: {completion}\n\n")
                        + &chunk(json!({}), json!("stop"))
                        + &format!("data: {{\"choices\":[],\"usage\":{usage}}}\n\ndata: [DONE]\n\n")
                };
                let server = MockServer::start().await;
                Mock::given(method("POST"))
                    .respond_with(
                        ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"),
                    )
                    .expect(1)
                    .mount(&server)
                    .await;
                let mut provider_options = options(kind, &server.uri());
                if kind == ProviderKind::Ollama {
                    provider_options.api_key = None;
                }
                let mut engine = engine(
                    Arc::new(Provider::new(provider_options)?),
                    Arc::new(AtomicUsize::new(0)),
                    None,
                )?;
                let output = engine.run(input("usage"), CancellationToken::new()).await?;
                let usage = output.usage.ok_or("missing usage")?;
                assert_eq!((usage.input_tokens, usage.output_tokens), (17, 9));
                assert_eq!(
                    usage.input_cache,
                    read_tokens.map(|read_tokens| cocoa_core::model::InputCacheUsage {
                        read_tokens: Some(read_tokens),
                        write_tokens: None,
                    })
                );
                engine.shutdown().await?;
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn chat_cache_usage_rejects_invalid_counts() -> Result<(), Box<dyn Error>> {
    let valid = json!({"prompt_tokens":17,"completion_tokens":9});
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
        for field in ["prompt_tokens", "completion_tokens"] {
            let mut usage = valid.clone();
            usage[field] = count.clone();
            invalid.push(usage);
        }
        let mut usage = valid.clone();
        usage["prompt_tokens_details"] = json!({"cached_tokens":count});
        invalid.push(usage);
    }
    for details in [
        json!([]),
        json!("invalid"),
        json!(true),
        json!(3),
        json!({"cached_tokens":18}),
    ] {
        let mut usage = valid.clone();
        usage["prompt_tokens_details"] = details;
        invalid.push(usage);
    }
    for inline in [false, true] {
        for usage in &invalid {
            let mut completion: Value = serde_json::from_str(
                chunk(json!({"role":"assistant","content":"done"}), json!("stop"))
                    .trim_start_matches("data: ")
                    .trim(),
            )?;
            let body = if inline {
                completion["usage"] = usage.clone();
                format!("data: {completion}\n\ndata: [DONE]\n\n")
            } else {
                format!(
                    "data: {completion}\n\ndata: {{\"choices\":[],\"usage\":{usage}}}\n\ndata: [DONE]\n\n"
                )
            };
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
                .expect(1)
                .mount(&server)
                .await;
            let mut engine = engine(
                Arc::new(Provider::new(options(
                    ProviderKind::DeepSeek,
                    &server.uri(),
                ))?),
                Arc::new(AtomicUsize::new(0)),
                None,
            )?;
            let result = engine.run(input("usage"), CancellationToken::new()).await;
            assert!(
                matches!(result, Err(RunError::Model(ModelError::Protocol { .. }))),
                "{usage}: {result:?}"
            );
            engine.shutdown().await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn every_profile_replays_reasoning_and_ordered_tool_batches() -> Result<(), Box<dyn Error>> {
    for kind in PROFILES {
        let server = MockServer::start().await;
        let first = chunk(
            json!({"role":"assistant","reasoning_content":"think "}),
            Value::Null,
        ) + &chunk(
            json!({"reasoning_content":"exactly","tool_calls":[call(1,"second","ex","{\"code\":")]}),
            Value::Null,
        ) + &chunk(
            json!({"tool_calls":[call(0,"first","exec","{\"code\":\"print(1)\"}"), {"index":1,"function":{"name":"ec","arguments":"\"print(2)\"}"}}]}),
            Value::Null,
        ) + &chunk(json!({"content":null}), json!("tool_calls"))
            + "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":5}}\n\ndata: [DONE]\n\n";
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(|request: &wiremock::Request| {
                request.body_json::<Value>().is_ok_and(|body| {
                    !body["messages"].as_array().is_some_and(|messages| {
                        messages.iter().any(|message| message["role"] == "tool")
                    })
                })
            })
            .respond_with(ResponseTemplate::new(200).set_body_raw(first, "text/event-stream"))
            .mount(&server)
            .await;
        let mut final_chunk: Value = serde_json::from_str(
            &chunk(
                json!({"reasoning_content":"answer reasoning","content":"done"}),
                json!("stop"),
            )
            .replace("data: ", ""),
        )?;
        final_chunk["usage"] = json!({"prompt_tokens":19,"completion_tokens":3,"prompt_tokens_details":{"cached_tokens":7}});
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(|request: &wiremock::Request| {
                request.body_json::<Value>().is_ok_and(|body| {
                    body["messages"].as_array().is_some_and(|messages| {
                        messages.iter().any(|message| message["role"] == "tool")
                    })
                })
            })
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                format!("data: {final_chunk}\n\ndata: [DONE]\n\n"),
                "text/event-stream",
            ))
            .mount(&server)
            .await;
        let calls = Arc::new(AtomicUsize::new(0));
        let mut engine = engine(
            Arc::new(Provider::new(options(kind, &server.uri()))?),
            calls.clone(),
            None,
        )?;
        let output = engine.run(input("batch"), CancellationToken::new()).await?;
        assert_eq!(output.answer, "done", "{kind}");
        let usage = output.usage.ok_or("missing usage")?;
        assert_eq!((usage.input_tokens, usage.output_tokens), (30, 8));
        assert_eq!(usage.input_cache, None);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            engine
                .run(input("follow up"), CancellationToken::new())
                .await?
                .answer,
            "done"
        );
        engine.shutdown().await?;
        let requests = server.received_requests().await.ok_or("missing requests")?;
        assert_eq!(requests.len(), 3, "{kind}");
        for request in &requests {
            assert_eq!(
                request.headers["authorization"],
                "Bearer test-secret-never-log"
            );
            let body: Value = request.body_json()?;
            assert_eq!(body["stream_options"]["include_usage"], true);
            assert_eq!(body["tools"][0]["function"]["name"], "exec");
            assert_eq!(
                body["tools"][0]["function"]["parameters"]["additionalProperties"],
                false
            );
            for absent in [
                "reasoning_effort",
                "thinking",
                "store",
                "include",
                "parallel_tool_calls",
            ] {
                assert!(body.get(absent).is_none(), "{kind}: {absent}");
            }
        }
        let replay: Value = requests[1].body_json()?;
        let messages = replay["messages"].as_array().ok_or("missing messages")?;
        let assistant = messages
            .iter()
            .find(|message| message["role"] == "assistant")
            .ok_or("missing assistant")?;
        assert_eq!(assistant["reasoning_content"], "think exactly");
        assert_eq!(assistant["tool_calls"][0]["id"], "first");
        assert_eq!(
            assistant["tool_calls"][1]["function"]["arguments"],
            "{\"code\":\"print(2)\"}"
        );
        let feedback: Vec<_> = messages
            .iter()
            .filter(|message| message["role"] == "tool")
            .collect();
        assert_eq!(feedback.len(), 2);
        for (ordinal, message) in feedback.iter().enumerate() {
            assert_eq!(
                message["tool_call_id"],
                if ordinal == 0 { "first" } else { "second" }
            );
            assert!(
                message["content"]
                    .as_str()
                    .is_some_and(|text| text.contains("Selected exec calls for this input: 2."))
            );
        }
        let followup: Value = requests[2].body_json()?;
        assert!(
            followup["messages"]
                .as_array()
                .ok_or("missing history")?
                .iter()
                .any(|message| message["reasoning_content"] == "answer reasoning")
        );
    }
    Ok(())
}

#[tokio::test]
async fn structured_reasoning_is_bounded_replayed_and_not_displayed() -> Result<(), Box<dyn Error>>
{
    let server = MockServer::start().await;
    let first = chunk(
        json!({"reasoning_details":[{"index":0,"id":"opaque","type":"reasoning.encrypted","format":"vendor","data":"enc","future":{"keep":true}}]}),
        Value::Null,
    ) + &chunk(
        json!({"reasoning_details":[{"index":0,"data":"rypted"}],"tool_calls":[call(0,"tool","exec","{\"code\":\"print(1)\"}")]}),
        json!("tool_calls"),
    ) + "data: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(|request: &wiremock::Request| {
            request.body_json::<Value>().is_ok_and(|body| {
                !body["messages"].as_array().is_some_and(|messages| {
                    messages.iter().any(|message| message["role"] == "tool")
                })
            })
        })
        .respond_with(ResponseTemplate::new(200).set_body_raw(first, "text/event-stream"))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(|request: &wiremock::Request| {
            request.body_json::<Value>().is_ok_and(|body| {
                body["messages"].as_array().is_some_and(|messages| {
                    messages.iter().any(|message| message["role"] == "tool")
                })
            })
        })
        .respond_with(ResponseTemplate::new(200).set_body_raw(answer("done"), "text/event-stream"))
        .mount(&server)
        .await;
    let (events, mut receiver) = EventPublisher::channel(64, 65536)?;
    let mut engine = engine(
        Arc::new(Provider::new(options(
            ProviderKind::HuggingFace,
            &server.uri(),
        ))?),
        Arc::new(AtomicUsize::new(0)),
        Some(events),
    )?;
    assert_eq!(
        engine
            .run(input("opaque"), CancellationToken::new())
            .await?
            .answer,
        "done"
    );
    engine.shutdown().await?;
    let mut previews = String::new();
    while let Observation::Event(event) = receiver.recv().await {
        if let EventKind::ReasoningText { text, .. } = event.kind {
            previews.push_str(&text);
        }
    }
    assert!(previews.is_empty());
    let requests = server.received_requests().await.ok_or("requests missing")?;
    let body: Value = requests[1].body_json()?;
    let assistant = body["messages"]
        .as_array()
        .ok_or("messages missing")?
        .iter()
        .find(|message| message["role"] == "assistant")
        .ok_or("assistant missing")?;
    assert_eq!(
        assistant["reasoning_details"],
        json!([{"index":0,"id":"opaque","type":"reasoning.encrypted","format":"vendor","data":"encrypted","future":{"keep":true}}])
    );
    Ok(())
}

#[tokio::test]
async fn partial_refused_malformed_and_unbounded_calls_never_execute_or_retry()
-> Result<(), Box<dyn Error>> {
    let tool = json!({"tool_calls":[call(0,"one","exec","{\"code\":\"print(1)\"}")]});
    let mut invalid=vec![
        chunk(tool.clone(),Value::Null),
        chunk(tool.clone(),json!("tool_calls")),
        "data: [DONE]\n\n".into(),
        chunk(json!({"content":"partial"}),json!("length"))+"data: [DONE]\n\n",
        chunk(json!({"content":"hidden"}),json!("content_filter"))+"data: [DONE]\n\n",
        chunk(json!({"refusal":"test-secret-never-log"}),json!("stop"))+"data: [DONE]\n\n",
        "data: {\"error\":{\"message\":\"test-secret-never-log\"}}\n\n".into(),
        chunk(json!({"audio":{"data":"lost"}}),Value::Null),
        chunk(json!({"reasoning_details":[{"index":32,"data":"huge index"}]}),Value::Null),
        chunk(json!({"reasoning_details":[{"index":0,"data":44}]}),Value::Null),
        chunk(json!({"reasoning_content":42}),Value::Null),
        chunk(json!({"tool_calls":[call(32,"far","exec","{}")] }),json!("tool_calls"))+"data: [DONE]\n\n",
        chunk(json!({"tool_calls":[call(1,"gap","exec","{\"code\":\"print(1)\"}")] }),json!("tool_calls"))+"data: [DONE]\n\n",
        chunk(json!({"tool_calls":[call(0,"duplicate","exec","{\"code\":\"print(1)\"}"),call(1,"duplicate","exec","{\"code\":\"print(2)\"}")] }),json!("tool_calls"))+"data: [DONE]\n\n",
        chunk(json!({"tool_calls":[call(0,&"a".repeat(257),"exec","{}")] }),json!("tool_calls"))+"data: [DONE]\n\n",
        chunk(json!({"tool_calls":[call(0,"one",&"x".repeat(257),"{}")] }),json!("tool_calls"))+"data: [DONE]\n\n",
        answer("done").replace("data: [DONE]\n\n",&chunk(json!({"content":"after finish"}),Value::Null)),
        chunk(json!({"tool_calls":[call(0,"one","other","{}")] }),json!("tool_calls"))+"data: [DONE]\n\n",
        "data: {\"id\":\"id\",\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\n".into(),
        "data: invalid-json\n\n".into(),
    ];
    for arguments in [
        "{}",
        "{\"code\":42}",
        "{\"code\":\"print(1)\",\"extra\":true}",
        "{\"code\":\"x\",\"code\":\"y\"}",
        "{\"code\":",
        "[]",
    ] {
        invalid.push(
            chunk(
                json!({"tool_calls":[call(0,"one","exec",arguments)]}),
                json!("tool_calls"),
            ) + "data: [DONE]\n\n",
        );
    }
    for (ordinal, body) in invalid.into_iter().enumerate() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
            .mount(&server)
            .await;
        let calls = Arc::new(AtomicUsize::new(0));
        let mut engine = engine(
            Arc::new(Provider::new(options(
                ProviderKind::DeepSeek,
                &server.uri(),
            ))?),
            calls.clone(),
            None,
        )?;
        let error = engine
            .run(input("reject"), CancellationToken::new())
            .await
            .err()
            .ok_or_else(|| format!("accepted case {ordinal}"))?;
        assert!(
            matches!(error, RunError::Model(ModelError::Protocol { .. })),
            "case {ordinal}: {error:?}"
        );
        assert!(!format!("{error} {error:?}").contains("test-secret-never-log"));
        assert_eq!(calls.load(Ordering::SeqCst), 0, "case {ordinal}");
        assert_eq!(
            server
                .received_requests()
                .await
                .ok_or("requests missing")?
                .len(),
            1,
            "case {ordinal}"
        );
        engine.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn foreign_continuations_and_stale_feedback_fail_before_another_post()
-> Result<(), Box<dyn Error>> {
    for tamper in [
        "provider",
        "action",
        "invocation",
        "ordinal",
        "code",
        "unknown content",
    ] {
        let server = MockServer::start().await;
        let body = chunk(
            json!({"tool_calls":[call(0,"one","exec","{\"code\":\"print(1)\"}")]}),
            json!("tool_calls"),
        ) + "data: [DONE]\n\n";
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
            .mount(&server)
            .await;
        let model = Control {
            inner: Provider::new(options(ProviderKind::DeepSeek, &server.uri()))?,
            tamper: Some(tamper),
            timeout: Duration::from_secs(2),
            cancellation: CancellationToken::new(),
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let mut engine = engine(Arc::new(model), calls.clone(), None)?;
        let error = engine
            .run(input("tamper"), CancellationToken::new())
            .await
            .err()
            .ok_or("invalid replay accepted")?;
        assert!(
            matches!(error, RunError::Model(ModelError::Protocol { .. })),
            "{tamper}: {error:?}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            server
                .received_requests()
                .await
                .ok_or("requests missing")?
                .len(),
            1,
            "{tamper}"
        );
        engine.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn image_parts_replay_or_fail_explicitly() -> Result<(), Box<dyn Error>> {
    for kind in [
        ProviderKind::HuggingFace,
        ProviderKind::MoonshotAi,
        ProviderKind::Xiaomi,
        ProviderKind::QwenTokenPlan,
        ProviderKind::DeepSeek,
        ProviderKind::Cerebras,
        ProviderKind::AntLing,
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(answer("seen"), "text/event-stream"),
            )
            .mount(&server)
            .await;
        let mut engine = engine(
            Arc::new(Provider::new(options(kind, &server.uri()))?),
            Arc::new(AtomicUsize::new(0)),
            None,
        )?;
        let image = Image::from_png(b"\x89PNG\r\n\x1a\nimage")?;
        let result = engine
            .run(
                RunRequest {
                    input: UserInput::WithImages {
                        text: "inspect".into(),
                        images: vec![image.clone()],
                    },
                },
                CancellationToken::new(),
            )
            .await;
        if matches!(
            kind,
            ProviderKind::DeepSeek | ProviderKind::Cerebras | ProviderKind::AntLing
        ) {
            assert!(matches!(
                result,
                Err(RunError::Model(ModelError::Protocol { .. }))
            ));
            assert!(
                server
                    .received_requests()
                    .await
                    .ok_or("requests missing")?
                    .is_empty()
            );
        } else {
            assert_eq!(result?.answer, "seen");
            assert_eq!(
                engine
                    .run(input("again"), CancellationToken::new())
                    .await?
                    .answer,
                "seen"
            );
            for request in server.received_requests().await.ok_or("requests missing")? {
                let body: Value = request.body_json()?;
                let user = body["messages"]
                    .as_array()
                    .ok_or("messages missing")?
                    .iter()
                    .find(|message| message["role"] == "user")
                    .ok_or("user missing")?;
                assert_eq!(
                    user["content"],
                    json!([{"type":"text","text":"inspect"},{"type":"image_url","image_url":{"url":image.data_url()}}])
                );
            }
        }
        engine.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn explicit_thinking_has_no_invented_or_downgraded_mapping() -> Result<(), Box<dyn Error>> {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(answer("level accepted"), "text/event-stream"),
        )
        .mount(&server)
        .await;
    for kind in PROFILES {
        for level in [
            ThinkingLevel::None,
            ThinkingLevel::Minimal,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
            ThinkingLevel::XHigh,
            ThinkingLevel::Max,
        ] {
            let requests_before = server
                .received_requests()
                .await
                .ok_or("requests missing")?
                .len();
            let mut settings = options(kind, &server.uri());
            settings.thinking_level = Some(level);
            let supported = kind == ProviderKind::DeepSeek
                && matches!(
                    level,
                    ThinkingLevel::None
                        | ThinkingLevel::Low
                        | ThinkingLevel::High
                        | ThinkingLevel::Max
                );
            if supported {
                let mut engine = engine(
                    Arc::new(Provider::new(settings)?),
                    Arc::new(AtomicUsize::new(0)),
                    None,
                )?;
                assert_eq!(
                    engine
                        .run(input("effort"), CancellationToken::new())
                        .await?
                        .answer,
                    "level accepted"
                );
                let requests = server.received_requests().await.ok_or("requests missing")?;
                assert_eq!(requests.len(), requests_before + 1);
                let body: Value = requests.last().ok_or("request missing")?.body_json()?;
                assert_eq!(body["reasoning_effort"], json!(level));
                engine.shutdown().await?;
            } else {
                assert!(
                    matches!(
                        Provider::new(settings),
                        Err(cocoa_llm::ProviderError::UnsupportedThinkingLevel)
                    ),
                    "{kind} {level:?}"
                );
                assert_eq!(
                    server
                        .received_requests()
                        .await
                        .ok_or("requests missing")?
                        .len(),
                    requests_before
                );
            }
        }
    }
    let mut settings = options(ProviderKind::DeepSeek, "http://127.0.0.1:1");
    settings.model = "deepseek-reasoner".into();
    settings.thinking_level = Some(ThinkingLevel::High);
    assert!(Provider::new(settings).is_err());
    Ok(())
}

#[tokio::test]
async fn fragmented_sse_reports_plaintext_without_exposing_opaque_reasoning()
-> Result<(), Box<dyn Error>> {
    for newline in ["\n", "\r\n"] {
        let body = (": keepalive\n\n".to_string()
            + &chunk(
                json!({"role":"assistant","reasoning_content":"reason é"}),
                Value::Null,
            )
            + &chunk(json!({"content":"é🦀"}), json!("stop"))
            + "data: [DONE]\n\n")
            .replace('\n', newline)
            .into_bytes();
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await?;
            read_request(&mut socket).await?;
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).as_bytes()).await?;
            for fragment in body.chunks(3) {
                socket.write_all(fragment).await?;
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            Ok::<_, Box<dyn Error + Send + Sync>>(())
        });
        let (events, mut receiver) = EventPublisher::channel(64, 65536)?;
        let mut engine = engine(
            Arc::new(Provider::new(options(ProviderKind::MoonshotAi, &base))?),
            Arc::new(AtomicUsize::new(0)),
            Some(events),
        )?;
        assert_eq!(
            engine
                .run(input("unicode"), CancellationToken::new())
                .await?
                .answer,
            "é🦀"
        );
        engine.shutdown().await?;
        let mut assistant = String::new();
        let mut reasoning = String::new();
        while let Observation::Event(event) = receiver.recv().await {
            match event.kind {
                EventKind::AssistantText { text, .. } => assistant.push_str(&text),
                EventKind::ReasoningText { text, .. } => reasoning.push_str(&text),
                _ => {}
            }
        }
        assert_eq!(assistant, "é🦀");
        assert_eq!(reasoning, "reason é");
        task.await?.map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[tokio::test]
async fn cancellation_and_deadline_interrupt_headers_and_partial_bodies()
-> Result<(), Box<dyn Error>> {
    for body_wait in [false, true] {
        for cancel in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").await?;
            let base = format!("http://{}", listener.local_addr()?);
            let cancellation = CancellationToken::new();
            let server_cancellation = cancellation.clone();
            let task = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await?;
                read_request(&mut socket).await?;
                if body_wait {
                    socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: ").await?;
                }
                tokio::time::sleep(Duration::from_millis(80)).await;
                if cancel {
                    server_cancellation.cancel();
                }
                tokio::time::sleep(Duration::from_millis(120)).await;
                assert!(
                    tokio::time::timeout(Duration::from_millis(50), listener.accept())
                        .await
                        .is_err(),
                    "POST replayed"
                );
                Ok::<_, Box<dyn Error + Send + Sync>>(())
            });
            let model = Control {
                inner: Provider::new(options(ProviderKind::DeepSeek, &base))?,
                tamper: None,
                timeout: if cancel {
                    Duration::from_secs(2)
                } else {
                    Duration::from_millis(50)
                },
                cancellation,
            };
            let mut engine = engine(Arc::new(model), Arc::new(AtomicUsize::new(0)), None)?;
            let result = engine
                .run(input("interrupt"), CancellationToken::new())
                .await;
            if cancel {
                assert!(
                    matches!(result, Err(RunError::Model(ModelError::Cancelled))),
                    "{result:?}"
                );
            } else {
                assert!(
                    matches!(
                        result,
                        Err(RunError::Deadline {
                            cause: cocoa_core::store::FailureCause::ModelStream { .. }
                        })
                    ),
                    "{result:?}"
                );
            }
            engine.shutdown().await?;
            task.await?.map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn failed_posts_and_provider_rejections_are_redacted_and_never_replayed()
-> Result<(), Box<dyn Error>> {
    for status in [400, 401, 403, 500] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(status)
                    .set_body_string("test-secret-never-log https://sensitive.invalid"),
            )
            .mount(&server)
            .await;
        let mut engine = engine(
            Arc::new(Provider::new(options(
                ProviderKind::DeepSeek,
                &server.uri(),
            ))?),
            Arc::new(AtomicUsize::new(0)),
            None,
        )?;
        let error = engine
            .run(input("rejected"), CancellationToken::new())
            .await
            .err()
            .ok_or("rejection accepted")?;
        assert!(!format!("{error} {error:?}").contains("test-secret-never-log"));
        assert!(!format!("{error} {error:?}").contains("sensitive.invalid"));
        assert_eq!(
            server
                .received_requests()
                .await
                .ok_or("requests missing")?
                .len(),
            1
        );
        engine.shutdown().await?;
    }
    for partial in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await?;
            read_request(&mut socket).await?;
            if partial {
                socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 99999\r\n\r\ndata: {\"id\":").await?;
            }
            drop(socket);
            assert!(
                tokio::time::timeout(Duration::from_millis(200), listener.accept())
                    .await
                    .is_err(),
                "failed POST replayed"
            );
            Ok::<_, Box<dyn Error + Send + Sync>>(())
        });
        let mut engine = engine(
            Arc::new(Provider::new(options(ProviderKind::DeepSeek, &base))?),
            Arc::new(AtomicUsize::new(0)),
            None,
        )?;
        assert!(
            engine
                .run(input("disconnect"), CancellationToken::new())
                .await
                .is_err()
        );
        engine.shutdown().await?;
        task.await?.map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn options(kind: ProviderKind, base: &str) -> ProviderOptions {
    ProviderOptions {
        kind,
        endpoint: format!("{base}/v1/chat/completions"),
        model: "deepseek-flash".into(),
        api_key: Some("test-secret-never-log".into()),
        execution: ExecutionMode::Enabled,
        thinking_level: None,
        session_id: "hosted-chat-fixture".into(),
    }
}
fn input(text: &str) -> RunRequest {
    RunRequest { input: text.into() }
}
fn chunk(delta: Value, finish: Value) -> String {
    format!(
        "data: {}\n\n",
        json!({"id":"chatcmpl-fixture","object":"chat.completion.chunk","choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
    )
}
fn answer(text: &str) -> String {
    chunk(json!({"content":text}), json!("stop")) + "data: [DONE]\n\n"
}
fn call(index: u32, id: &str, name: &str, arguments: &str) -> Value {
    json!({"index":index,"id":id,"type":"function","function":{"name":name,"arguments":arguments}})
}
fn engine(
    model: Arc<dyn ModelProvider>,
    calls: Arc<AtomicUsize>,
    events: Option<EventPublisher>,
) -> Result<Engine, Box<dyn Error>> {
    Ok(Engine::new(
        EngineSettings::new(Limits {
            input_bytes: 4096,
            output_bytes: 65536,
            record_bytes: 1024 * 1024,
            context_bytes: 4 * 1024 * 1024,
            pending_operations: 2,
            stall_timeout: Duration::from_secs(3),
            settlement_timeout: Duration::from_millis(100),
            shutdown_timeout: Duration::from_millis(100),
        })?,
        EngineAdapters {
            model,
            executor: Box::new(Executor(calls)),
            capabilities: Arc::new(NoCapabilities),
            context: Arc::new(BoundedContext),
            store: Arc::new(InMemoryStore::new(StoreLimits {
                record_bytes: 1024 * 1024,
            })?),
        },
        events,
    ))
}
struct Executor(Arc<AtomicUsize>);
#[async_trait]
impl CellExecutor for Executor {
    async fn execute(
        &mut self,
        cell: Cell,
        _: &OperationContext,
        _: CancellationToken,
    ) -> Result<CellResponse, ExecutionError> {
        let ordinal = self.0.fetch_add(1, Ordering::SeqCst);
        assert_eq!(cell.code, format!("print({})", ordinal + 1));
        Ok(CellResponse::Completed {
            printed: vec![json!(ordinal + 1)],
        })
    }
    async fn shutdown(&mut self, _: CancellationToken) -> Result<ShutdownReport, ExecutionError> {
        Ok(ShutdownReport { unresolved_work: 0 })
    }
}
struct NoCapabilities;
#[async_trait]
impl CapabilityHost for NoCapabilities {
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
        panic!("fixture has no capabilities")
    }
}
struct Control {
    inner: Provider,
    tamper: Option<&'static str>,
    timeout: Duration,
    cancellation: CancellationToken,
}
#[async_trait]
impl ModelProvider for Control {
    async fn respond(
        &self,
        mut request: ModelRequest,
        _: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        request.limits.deadline = tokio::time::Instant::now() + self.timeout;
        for record in &mut request.context.records {
            match &mut record.kind {
                RecordKind::ModelCompleted { response } => {
                    if let Some(continuation) = &mut response.continuation {
                        match self.tamper {
                            Some("provider") => {
                                continuation.provider = "ollama.chat_completions".into()
                            }
                            Some("action") => {
                                continuation.data["message"]["tool_calls"][0]["function"]["arguments"] =
                                    json!("{\"code\":\"different\"}")
                            }
                            Some("unknown content") => {
                                continuation.data["message"]["audio"] =
                                    json!({"data":"unsupported"})
                            }
                            _ => {}
                        }
                    }
                }
                RecordKind::CellCompleted { cell, code, .. } => match self.tamper {
                    Some("invocation") => record.invocation = InvocationId(99),
                    Some("ordinal") => *cell = 1,
                    Some("code") => *code = "stale".into(),
                    _ => {}
                },
                _ => {}
            }
        }
        self.inner.respond(request, self.cancellation.clone()).await
    }
}
async fn read_request(
    socket: &mut tokio::net::TcpStream,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    for _ in 0..256 {
        let count = socket.read(&mut buffer).await?;
        if count == 0 {
            return Err("request closed".into());
        }
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&bytes[..end])?;
            let length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|value| value.trim().parse::<usize>())
                })
                .ok_or("missing content length")??;
            if bytes.len() >= end + 4 + length {
                return Ok(());
            }
        }
    }
    Err("request fixture limit".into())
}
