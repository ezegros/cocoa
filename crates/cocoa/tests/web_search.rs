//! Search contracts exercised through the public capability and QuickJS engine APIs.

use std::{error::Error, sync::Arc, time::Duration};

use async_trait::async_trait;
use cocoa::mcp::McpError;
use cocoa::web_search::WebSearch;
use cocoa_core::{
    capability::{CapabilityError, CapabilityHost},
    context::BoundedContext,
    engine::{Engine, EngineAdapters, EngineSettings, Limits, RunRequest},
    execution::{Cell, CellResponse, WorkLimits},
    model::{ModelAction, ModelError, ModelProvider, ModelRequest, ModelResponse},
    store::{InMemoryStore, RecordKind, Store, StoreLimits},
};
use cocoa_quickjs::{FeedbackLimits, QuickJsExecutor, QuickJsOptions};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use wiremock::{Mock, MockServer, Request, ResponseTemplate, matchers::method};

#[test]
fn search_endpoints_allow_https_and_literal_loopback_http_only() -> Result<(), Box<dyn Error>> {
    for endpoint in [
        "https://example.com/mcp",
        "http://127.0.0.1:8080/mcp",
        "http://127.23.45.67/mcp",
        "http://[::1]:8080/mcp",
        "http://[0:0:0:0:0:0:0:1]/mcp",
    ] {
        WebSearch::new(endpoint)?;
    }
    for endpoint in [
        "http://localhost/mcp",
        "http://example.com/mcp",
        "http://127.0.0.1.example.com/mcp",
        "http://192.168.1.1/mcp",
        "http://0.0.0.0/mcp",
        "http://[::]/mcp",
        "http://[::2]/mcp",
        "http://[2001:db8::1]/mcp",
        "http://[::ffff:127.0.0.1]/mcp",
        "ftp://127.0.0.1/mcp",
        "https://user:password@example.com/mcp",
        "http://user:password@[::1]/mcp",
        "https://example.com/mcp?token=private",
        "http://[::1]/mcp?token=private",
        "https://example.com/mcp#fragment",
        "http://[::1]/mcp#fragment",
    ] {
        assert!(matches!(
            WebSearch::new(endpoint),
            Err(McpError::Configuration)
        ));
    }
    Ok(())
}

#[tokio::test]
async fn search_preserves_source_text_through_json_sse_and_the_repl() -> Result<(), Box<dyn Error>>
{
    let remote = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(|request: &Request| {
            let Ok(body) = request.body_json::<Value>() else {
                return ResponseTemplate::new(400);
            };
            let arguments = &body["params"]["arguments"];
            if body["method"] != "tools/call"
                || body["params"]["name"] != "web_search_exa"
                || arguments["objective"] != arguments["query"]
                || request.headers.contains_key("authorization")
                || request.headers.contains_key("x-api-key")
                || request.headers["accept"] != "application/json, text/event-stream"
            {
                return ResponseTemplate::new(400);
            }
            let text = match (arguments["query"].as_str(), arguments["numResults"].as_u64()) {
                (Some("tokio abort λ"), Some(2)) => "Title: Blocking\nURL: https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html\nText: A started blocking task cannot be aborted.",
                (Some("reqwest redirects"), Some(5)) => "URL: https://docs.rs/reqwest/latest/reqwest/redirect/struct.Policy.html\nText: Maximum of 10 redirects.",
                _ => return ResponseTemplate::new(400),
            };
            let reply = json!({"jsonrpc":"2.0","id":body["id"],"result":{"content":[{"type":"text","text":text},{"type":"text","text":"second block"}],"isError":false}});
            if arguments["numResults"] == 2 {
                ResponseTemplate::new(200).set_body_raw(
                    format!("event: message\r\ndata: {reply}\r\n\r\n"),
                    "text/event-stream",
                )
            } else {
                ResponseTemplate::new(200).set_body_json(reply)
            }
        })
        .mount(&remote)
        .await;
    let search = Arc::new(WebSearch::new(&remote.uri())?);
    let store = Arc::new(InMemoryStore::new(StoreLimits {
        record_bytes: 65536,
    })?);
    let mut engine = Engine::new(
        EngineSettings::new(Limits {
            input_bytes: 4096,
            output_bytes: 16384,
            record_bytes: 65536,
            context_bytes: 512000,
            pending_operations: 4,
            stall_timeout: Duration::from_secs(3),
            settlement_timeout: Duration::from_secs(1),
            shutdown_timeout: Duration::from_secs(1),
        })?,
        EngineAdapters {
            model: Arc::new(SearchScript),
            executor: Box::new(
                QuickJsExecutor::new(QuickJsOptions {
                    memory_bytes: 16 * 1024 * 1024,
                    stack_bytes: 256 * 1024,
                    feedback: FeedbackLimits {
                        output_bytes: 16384,
                        printed_bytes: 8192,
                        error_bytes: 2048,
                        print_calls: 32,
                    },
                })
                .await?,
            ),
            capabilities: search,
            context: Arc::new(BoundedContext),
            store: store.clone(),
        },
        None,
    );
    engine
        .run(
            RunRequest {
                input: "search".into(),
            },
            CancellationToken::new(),
        )
        .await?;
    let mut printed = Vec::new();
    for record in store.load().await?.into_records() {
        if let RecordKind::CellCompleted { outcome, .. } = record.kind {
            let CellResponse::Completed { printed: values } = outcome else {
                return Err(format!("search cell failed: {outcome:?}").into());
            };
            printed.extend(values);
        }
    }
    assert_eq!(
        printed,
        vec![
            json!({"provider":"exa","text":"Title: Blocking\nURL: https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html\nText: A started blocking task cannot be aborted.\nsecond block"}),
            json!({"provider":"exa","text":"URL: https://docs.rs/reqwest/latest/reqwest/redirect/struct.Policy.html\nText: Maximum of 10 redirects.\nsecond block"}),
        ]
    );
    engine.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn invalid_cancelled_and_expired_searches_make_no_request() -> Result<(), Box<dyn Error>> {
    let remote = MockServer::start().await;
    let search = WebSearch::new(&remote.uri())?;
    for arguments in [
        json!({"query":" \n"}),
        json!({"query":"x","limit":0}),
        json!({"query":"x","limit":11}),
        json!({"query":"x","limit":1.5}),
        json!({"query":"x","limit":null}),
        json!({"query":"λ".repeat(2049)}),
        json!({"query":"x","api_key":"not-accepted"}),
    ] {
        assert!(matches!(
            search
                .call(
                    "cocoa.web_search",
                    arguments,
                    limits(),
                    CancellationToken::new()
                )
                .await,
            Err(CapabilityError::Rejected { .. })
        ));
    }
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert_eq!(
        search
            .call(
                "cocoa.web_search",
                json!({"query":"x"}),
                limits(),
                cancellation
            )
            .await,
        Err(CapabilityError::Cancelled)
    );
    let expired = WorkLimits {
        deadline: tokio::time::Instant::now(),
        ..limits()
    };
    assert_eq!(
        search
            .call(
                "cocoa.web_search",
                json!({"query":"x"}),
                expired,
                CancellationToken::new()
            )
            .await,
        Err(CapabilityError::Deadline)
    );
    assert!(
        remote
            .received_requests()
            .await
            .ok_or("missing requests")?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn failures_are_sanitized_bounded_and_not_automatically_retried() -> Result<(), Box<dyn Error>>
{
    let secret = "private-remote-diagnostic";
    let bodies = [
        ResponseTemplate::new(429).set_body_string(secret),
        ResponseTemplate::new(200).set_body_json(json!({"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":secret}],"isError":true}})),
        ResponseTemplate::new(200).set_body_json(json!({"jsonrpc":"2.0","id":1,"error":{"code":-32603,"message":secret}})),
        ResponseTemplate::new(200).set_body_json(json!({"jsonrpc":"2.0","id":999,"result":{"content":[]}})),
        ResponseTemplate::new(200).set_body_json(json!({"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"image","data":secret}]}})),
        ResponseTemplate::new(200).set_body_json(json!({"jsonrpc":"2.0","id":1,"result":{"content":[],"isError":"false"}})),
        ResponseTemplate::new(200).set_body_raw("{".to_owned()+&" ".repeat(1024*1024), "application/json"),
        ResponseTemplate::new(200).set_body_json(json!({"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"x".repeat(17000)}]}})),
    ];
    for response in bodies {
        let remote = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(response)
            .mount(&remote)
            .await;
        let search = WebSearch::new(&remote.uri())?;
        let error = search
            .call(
                "cocoa.web_search",
                json!({"query":"public question"}),
                limits(),
                CancellationToken::new(),
            )
            .await
            .err()
            .ok_or("search unexpectedly succeeded")?;
        assert!(
            matches!(&error, CapabilityError::Tool { code, .. } if code == "web_search_failed")
        );
        assert!(!error.to_string().contains(secret));
        assert_eq!(
            remote
                .received_requests()
                .await
                .ok_or("missing requests")?
                .len(),
            1
        );
    }
    let redirect = MockServer::start().await;
    let target = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(307).insert_header("location", target.uri()))
        .mount(&redirect)
        .await;
    let search = WebSearch::new(&redirect.uri())?;
    assert!(
        search
            .call(
                "cocoa.web_search",
                json!({"query":"x"}),
                limits(),
                CancellationToken::new()
            )
            .await
            .is_err()
    );
    assert!(
        target
            .received_requests()
            .await
            .ok_or("missing requests")?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn cancelling_search_releases_capacity_for_later_calls() -> Result<(), Box<dyn Error>> {
    let remote = MockServer::start().await;
    let (arrived, mut requests) = tokio::sync::mpsc::channel(4);
    Mock::given(method("POST"))
        .respond_with(move |request: &Request| {
            let Ok(body) = request.body_json::<Value>() else {
                return ResponseTemplate::new(400);
            };
            let response = ResponseTemplate::new(200)
                .set_body_json(json!({"jsonrpc":"2.0","id":body["id"],"result":{"content":[]}}));
            if body["params"]["arguments"]["query"] == "cancel" {
                assert!(arrived.try_send(()).is_ok(), "request gate overflowed");
                response.set_delay(Duration::from_secs(30))
            } else {
                response
            }
        })
        .expect(5)
        .mount(&remote)
        .await;
    let search = WebSearch::new(&remote.uri())?;
    tokio::time::timeout(Duration::from_secs(5), async {
        for _ in 0..4 {
            let cancellation = CancellationToken::new();
            let cancel = async {
                assert_eq!(requests.recv().await, Some(()));
                cancellation.cancel();
            };
            let (result, ()) = tokio::join!(
                search.call(
                    "cocoa.web_search",
                    json!({"query":"cancel"}),
                    limits(),
                    cancellation.clone()
                ),
                cancel
            );
            assert_eq!(result, Err(CapabilityError::Cancelled));
        }
        assert_eq!(
            search
                .call(
                    "cocoa.web_search",
                    json!({"query":"next","limit":1}),
                    limits(),
                    CancellationToken::new()
                )
                .await?,
            json!({"provider":"exa","text":""})
        );
        Ok::<(), Box<dyn Error>>(())
    })
    .await??;
    Ok(())
}

struct SearchScript;

#[async_trait]
impl ModelProvider for SearchScript {
    async fn respond(
        &self,
        request: ModelRequest,
        _: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        let completed = request
            .context
            .records
            .iter()
            .any(|record| matches!(record.kind, RecordKind::CellCompleted { .. }));
        Ok(ModelResponse {
            action: if completed {
                ModelAction::Final {
                    answer: "searched".into(),
                }
            } else {
                ModelAction::Execute { cells:vec![Cell { code:"var first = await cocoa.web_search({query:'tokio abort λ',limit:2}); print(first); var second = await cocoa.web_search({query:'reqwest redirects'}); print(second);".into() }] }
            },
            continuation: None,
            usage: None,
        })
    }
}

fn limits() -> WorkLimits {
    WorkLimits {
        output_bytes: 16384,
        deadline: tokio::time::Instant::now() + Duration::from_secs(3),
    }
}
