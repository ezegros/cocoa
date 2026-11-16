//! Historical text through public session bindings, SQLite and real QuickJS calls.

use std::{
    collections::{BTreeMap, VecDeque},
    error::Error,
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use cocoa::{
    agents::{AgentFactory, AgentLimits, AgentManager, AgentParticipant},
    persistence::{
        EffectiveAliasSettings, EffectiveSessionSettings, HistoryReadLimits, SessionAttachment,
        SessionDatabase, SessionExecutionLimits, SqliteStore,
    },
};
use cocoa_core::{
    capability::{
        CapabilityDescription, CapabilityError, CapabilityHost, CapabilityObserver, Effects,
        OperationOutcome,
    },
    context::{BoundedContext, ContextAssembler, ContextError, InstructionState, PreparedContext},
    engine::{Engine, EngineAdapters, EngineSettings, Limits, RunRequest},
    execution::{Cell, CellResponse, WorkLimits},
    model::{
        Image, ModelAction, ModelError, ModelProvider, ModelRequest, ModelResponse, ProviderData,
        UserInput,
    },
    store::{
        InvocationId, OperationId, Record, RecordId, RecordKind, Store, StoreLimits, StoredRecord,
    },
};
use cocoa_quickjs::{FeedbackLimits, QuickJsExecutor, QuickJsOptions};
use serde_json::{Value, json};
use sqlx::{Connection, SqliteConnection};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn exact_text_preserves_full_width_ids_and_images_stay_out_of_text()
-> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new().await?;
    let original = Record {
        invocation: InvocationId(9_007_199_254_740_993),
        kind: RecordKind::InputAccepted {
            input: UserInput::WithImages {
                text: "π original failure, preserve %_ literal text".into(),
                images: vec![Image::from_png(b"\x89PNG\r\n\x1a\nprivate pixels")?],
            },
        },
    };
    let initial = fixture.store.append(&original, None).await?;
    let mut admin = fixture.admin().await?;
    let record = 9_007_199_254_740_995_i64;
    sqlx::query("UPDATE records SET id = ? WHERE id = ?")
        .bind(record)
        .bind(i64::from(initial))
        .execute(&mut admin)
        .await?;
    let operation = 9_007_199_254_740_997_u64;
    let execution = fixture
        .store
        .append(
            &Record {
                invocation: original.invocation,
                kind: RecordKind::OperationFinished {
                    operation: OperationId(operation),
                    outcome: OperationOutcome::Completed {
                        value: json!({"exit_code":17,"stderr":"older asymmetric failure"}),
                    },
                },
            },
            None,
        )
        .await?;
    let model = fixture
        .store
        .append(
            &Record {
                invocation: original.invocation,
                kind: RecordKind::ModelCompleted {
                    response: ModelResponse {
                        action: ModelAction::Final {
                            answer: "visible assistant evidence".into(),
                        },
                        continuation: Some(ProviderData {
                            provider: "fixture".into(),
                            data: json!({"hidden":"opaque-private-marker"}),
                        }),
                        usage: None,
                    },
                },
            },
            None,
        )
        .await?;
    let response = call(
        &fixture.host,
        "history.read",
        json!({"record":record.to_string()}),
    )
    .await?;
    assert_eq!(response["record"], record.to_string());
    assert_eq!(response["sequence"], "1");
    assert_eq!(response["invocation"], "9007199254740993");
    assert_eq!(
        response["text"],
        "π original failure, preserve %_ literal text"
    );
    assert_eq!(response["image_count"], 1);
    assert!(
        response["image_notice"]
            .as_str()
            .ok_or("missing image notice")?
            .contains("image ordinals 1..=1")
    );
    assert!(!response.to_string().contains("base64"));
    assert!(!response.to_string().contains("private pixels"));
    assert_eq!(
        fixture
            .store
            .read_record(RecordId::try_from(record)?, 4096)
            .await?
            .ok_or("missing original")?
            .record,
        original
    );
    let executed = call(
        &fixture.host,
        "history.read",
        json!({"record":i64::from(execution).to_string()}),
    )
    .await?;
    let text = executed["text"].as_str().ok_or("missing execution text")?;
    assert!(text.contains("operation=\"9007199254740997\""));
    assert!(text.contains("older asymmetric failure"));
    assert!(text.contains("\"exit_code\":17"));
    assert_eq!(
        call(
            &fixture.host,
            "history.read",
            json!({"record":i64::from(model).to_string()})
        )
        .await?["text"],
        "visible assistant evidence"
    );
    let first = call(&fixture.host, "history.list", json!({"limit":1})).await?;
    assert_eq!(first["items"][0]["record"], record.to_string());
    assert_eq!(first["through"], "3");
    assert_eq!(first["complete"], false);
    fixture
        .store
        .append(&accepted("new append beyond the pinned boundary"), None)
        .await?;
    let rest = call(
        &fixture.host,
        "history.list",
        json!({"cursor":first["next_cursor"]}),
    )
    .await?;
    assert_eq!(rest["through"], "3");
    assert_eq!(rest["items"].as_array().map(Vec::len), Some(2));
    assert_eq!(rest["complete"], true);
    assert_eq!(rest["next_cursor"], Value::Null);
    let literal = call(&fixture.host, "history.search", json!({"query":"%_"})).await?;
    assert_eq!(literal["items"].as_array().map(Vec::len), Some(1));
    assert_eq!(literal["items"][0]["record"], record.to_string());
    let hidden = call(
        &fixture.host,
        "history.search",
        json!({"query":"opaque-private-marker"}),
    )
    .await?;
    assert_eq!(hidden["items"], json!([]));
    assert_eq!(hidden["complete"], true);
    fixture.manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn byte_work_limits_resume_before_unconsumed_evidence() -> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new().await?;
    fixture
        .store
        .append(&accepted(&"x".repeat(9 * 1024 * 1024)), None)
        .await?;
    let target = fixture
        .store
        .append(
            &accepted(&format!("TARGET{}", "y".repeat(8 * 1024 * 1024))),
            None,
        )
        .await?;
    let page = call(&fixture.host, "history.search", json!({"query":"TARGET"})).await?;
    assert_eq!(page["items"], json!([]));
    assert_eq!(page["scanned"], 1);
    assert_eq!(page["stop"], "read_bytes");
    assert_eq!(page["complete"], false);
    assert!(
        page["read_bytes"]
            .as_u64()
            .is_some_and(|bytes| bytes <= 16 * 1024 * 1024)
    );
    let next = call(
        &fixture.host,
        "history.search",
        json!({"query":"TARGET","cursor":page["next_cursor"]}),
    )
    .await?;
    assert_eq!(next["items"].as_array().map(Vec::len), Some(1));
    assert_eq!(next["items"][0]["record"], i64::from(target).to_string());
    assert_eq!(next["complete"], true);
    fixture.manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn search_reports_partial_coverage_and_pins_the_remaining_range() -> Result<(), Box<dyn Error>>
{
    let fixture = Fixture::new().await?;
    for index in 0..64 {
        fixture
            .store
            .append(
                &accepted(&format!("nonmatching earlier record {index}")),
                None,
            )
            .await?;
    }
    let target = fixture
        .store
        .append(&accepted("last TARGET evidence"), None)
        .await?;
    let first = call(&fixture.host, "history.search", json!({"query":"TARGET"})).await?;
    assert_eq!(first["items"], json!([]));
    assert_eq!(first["scanned"], 64);
    assert_eq!(first["through"], "65");
    assert_eq!(first["stop"], "scan_limit");
    assert_eq!(first["complete"], false);
    assert!(first["next_cursor"].is_string());
    fixture
        .store
        .append(&accepted("new TARGET append outside pin"), None)
        .await?;
    let last = call(
        &fixture.host,
        "history.search",
        json!({"query":"TARGET","cursor":first["next_cursor"]}),
    )
    .await?;
    assert_eq!(last["items"].as_array().map(Vec::len), Some(1));
    assert_eq!(last["items"][0]["record"], i64::from(target).to_string());
    assert_eq!(last["items"][0]["match_offset"], 5);
    assert_eq!(last["scanned"], 1);
    assert_eq!(last["through"], "65");
    assert_eq!(last["complete"], true);
    assert_eq!(last["next_cursor"], Value::Null);
    let absent = call(
        &fixture.host,
        "history.search",
        json!({"query":"target","cursor":first["next_cursor"]}),
    )
    .await?;
    assert_eq!(absent["items"], json!([]));
    assert_eq!(absent["complete"], true);
    assert!(
        first["read_bytes"]
            .as_u64()
            .is_some_and(|bytes| bytes <= 16 * 1024 * 1024)
    );
    fixture.manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn text_pages_validate_utf8_arguments_and_independent_output_limits()
-> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new().await?;
    let original = "π🦀quoted \"text\"\n".repeat(1300);
    let record = fixture.store.append(&accepted(&original), None).await?;
    let record = i64::from(record).to_string();
    let mut restored = String::new();
    let mut offset = 0;
    for _ in 0..32 {
        let page = call(
            &fixture.host,
            "history.read",
            json!({"record":record,"offset":offset,"limit":4096}),
        )
        .await?;
        let text = page["text"].as_str().ok_or("missing text page")?;
        assert!(text.len() <= 4096);
        assert!(serde_json::to_vec(&page)?.len() <= 65536);
        assert_eq!(page["text_bytes"], original.len());
        restored.push_str(text);
        if page["complete"] == true {
            break;
        }
        offset = page["next_offset"]
            .as_u64()
            .ok_or("missing text continuation")?;
    }
    assert_eq!(restored, original);
    for arguments in [
        json!({"record":record,"offset":1}),
        json!({"record":record,"offset":u32::MAX}),
        json!({"record":record,"limit":1}),
        json!({"record":record,"limit":16385}),
        json!({"record":record,"limit":null}),
        json!({"record":record,"agent":"foreign"}),
        json!({"record":1}),
        json!({"record":"01"}),
        json!({"record":"+1"}),
        json!({"record":"0"}),
    ] {
        rejection(
            call(&fixture.host, "history.read", arguments).await,
            "invalid_arguments",
        );
    }
    for arguments in [
        json!(null),
        json!([]),
        json!({"cursor":null}),
        json!({"limit":17}),
        json!({"limit":0}),
        json!({"agent":"foreign"}),
    ] {
        rejection(
            call(&fixture.host, "history.list", arguments).await,
            "invalid_arguments",
        );
    }
    for arguments in [
        json!({"query":""}),
        json!({"query":null}),
        json!({"query":"λ".repeat(257)}),
        json!({"query":"x","regex":true}),
    ] {
        rejection(
            call(&fixture.host, "history.search", arguments).await,
            "invalid_arguments",
        );
    }
    rejection(
        call(&fixture.host, "history.list", json!({"cursor":"bogus"})).await,
        "invalid_cursor",
    );
    rejection(
        call(&fixture.host, "history.delete", json!({})).await,
        "unknown_method",
    );
    rejection(
        fixture
            .host
            .call(
                "history.read",
                json!({"record":record}),
                work_limits(128),
                CancellationToken::new(),
            )
            .await,
        "result_limit",
    );
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    rejection(
        fixture
            .host
            .call("history.list", json!({}), work_limits(65536), cancellation)
            .await,
        "cancelled",
    );
    let cancellation = CancellationToken::new();
    let mut reading = fixture.host.call(
        "history.list",
        json!({}),
        work_limits(65536),
        cancellation.clone(),
    );
    assert!(matches!(
        futures_util::poll!(&mut reading),
        std::task::Poll::Pending
    ));
    cancellation.cancel();
    rejection(reading.await, "cancelled");
    rejection(
        fixture
            .host
            .call(
                "history.list",
                json!({}),
                WorkLimits {
                    output_bytes: 65536,
                    deadline: tokio::time::Instant::now(),
                },
                CancellationToken::new(),
            )
            .await,
        "deadline",
    );
    assert_eq!(
        call(
            &fixture.host,
            "history.read",
            json!({"record":"9223372036854775807"})
        )
        .await?,
        Value::Null
    );
    let image_text = fixture
        .store
        .append(
            &accepted("before data:image/png;base64,AAABBB=== after"),
            None,
        )
        .await?;
    assert_eq!(
        call(
            &fixture.host,
            "history.read",
            json!({"record":i64::from(image_text).to_string()})
        )
        .await?["text"],
        "before [image] after"
    );
    fixture.manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn oversized_and_corrupt_evidence_fail_without_loading_unrelated_history()
-> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new().await?;
    let first = fixture
        .store
        .append(&accepted("small original"), None)
        .await?;
    let large = fixture
        .store
        .append(&accepted(&"x".repeat(16 * 1024 * 1024)), None)
        .await?;
    let last = fixture
        .store
        .append(&accepted("small exact evidence"), None)
        .await?;
    let page = call(&fixture.host, "history.list", json!({"limit":1})).await?;
    assert_eq!(page["items"].as_array().map(Vec::len), Some(1));
    assert_eq!(page["stop"], "limit");
    assert_eq!(page["complete"], false);
    rejection(
        call(
            &fixture.host,
            "history.read",
            json!({"record":i64::from(large).to_string()}),
        )
        .await,
        "record_too_large",
    );
    rejection(
        call(
            &fixture.host,
            "history.list",
            json!({"cursor":page["next_cursor"]}),
        )
        .await,
        "record_too_large",
    );
    rejection(
        call(
            &fixture.host,
            "history.search",
            json!({"query":"missing","cursor":page["next_cursor"]}),
        )
        .await,
        "record_too_large",
    );
    assert_eq!(
        call(
            &fixture.host,
            "history.read",
            json!({"record":i64::from(last).to_string()})
        )
        .await?["text"],
        "small exact evidence"
    );
    let mut admin = fixture.admin().await?;
    sqlx::query("UPDATE records SET payload_json = '{\"unknown_variant\":{}}' WHERE id = ?")
        .bind(i64::from(large))
        .execute(&mut admin)
        .await?;
    assert_eq!(
        call(
            &fixture.host,
            "history.read",
            json!({"record":i64::from(first).to_string()})
        )
        .await?["text"],
        "small original"
    );
    assert_eq!(
        call(
            &fixture.host,
            "history.read",
            json!({"record":i64::from(last).to_string()})
        )
        .await?["text"],
        "small exact evidence"
    );
    rejection(
        call(
            &fixture.host,
            "history.read",
            json!({"record":i64::from(large).to_string()}),
        )
        .await,
        "storage",
    );
    admin.close().await?;
    fixture.manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn uncertain_writes_remain_fenced_and_memory_history_is_explicitly_unavailable()
-> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new().await?;
    let record = fixture
        .store
        .append(&accepted("already accepted"), None)
        .await?;
    let mut admin = fixture.admin().await?;
    let transaction = admin.begin_with("BEGIN IMMEDIATE").await?;
    let uncertain = accepted("uncertain write");
    let mut append = fixture.store.append(&uncertain, None);
    assert!(matches!(
        futures_util::poll!(&mut append),
        std::task::Poll::Pending
    ));
    drop(append);
    transaction.rollback().await?;
    for (name, arguments) in [
        (
            "history.read",
            json!({"record":i64::from(record).to_string()}),
        ),
        ("history.list", json!({})),
        ("history.search", json!({"query":"accepted"})),
    ] {
        rejection(
            call(&fixture.host, name, arguments).await,
            "outcome_unknown",
        );
    }
    assert!(
        fixture
            .store
            .append(&accepted("must remain blocked"), None)
            .await
            .is_err()
    );
    fixture.manager.shutdown().await?;
    let manager = AgentManager::new(
        agent_limits(),
        engine_limits(),
        Arc::new(Factory::default()),
    )?;
    let host = manager.bind("fixture".into(), Arc::new(Inner))?;
    rejection(call(&host, "history.list", json!({})).await, "unavailable");
    assert_eq!(
        call(&host, "fixture.echo", json!({"unchanged":true})).await?,
        json!({"unchanged":true})
    );
    assert!(
        matches!(call(&host, "fixture.fail", json!({})).await, Err(CapabilityError::Tool { effects:Effects::Partial, code, .. }) if code == "inner_error")
    );
    assert!(
        host.descriptions()
            .iter()
            .any(|description| description.name == "history.read")
    );
    manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn root_and_child_quickjs_calls_use_their_own_history_and_forward_started_dispatch()
-> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new().await?;
    let root_code = r#"
        const page = await capabilities.call('history.list', {limit:1});
        const evidence = await capabilities.call('history.read', {record:page.items[0].record});
        if (evidence.text !== 'root private evidence') throw Error('wrong root history');
        if (typeof evidence.record !== 'string') throw Error('numeric record identity');
        const echoed = await capabilities.call('fixture.echo', {started:true});
        if (!echoed.started_dispatch) throw Error('call_started was not forwarded');
        const child = await cocoa.spawn({name:'child',prompt:'child private evidence'});
        const result = await cocoa.wait({child}, {timeout_ms:5000});
        if (result.child.status !== 'completed') throw Error('child failed');
        print({record:evidence.record,child:result.child.status});
    "#;
    let mut engine = make_engine(
        fixture.attachment.root_store_with_settings(
            StoreLimits {
                record_bytes: 128 * 1024,
            },
            effective_settings(),
        )?,
        fixture.host.clone(),
        Arc::new(Instructions),
        root_code,
    )
    .await?;
    let report = engine
        .run(
            RunRequest {
                input: "root private evidence".into(),
            },
            CancellationToken::new(),
        )
        .await?;
    assert_eq!(report.answer, "done");
    let history = fixture
        .store
        .read_history(None, HistoryReadLimits::new(64, 1024 * 1024)?)
        .await?;
    assert!(history.records.iter().any(|stored| matches!(&stored.record.kind, RecordKind::CellCompleted { outcome:CellResponse::Completed { printed }, .. } if printed == &vec![json!({"record":"1","child":"completed"})])));
    let child_host = fixture
        .factory
        .child
        .lock()
        .await
        .clone()
        .ok_or("child host was not bound")?;
    assert_eq!(
        call(&child_host, "history.read", json!({"record":"1"})).await?,
        Value::Null
    );
    let root_page = call(&fixture.host, "history.list", json!({"limit":1})).await?;
    rejection(
        call(
            &child_host,
            "history.list",
            json!({"cursor":root_page["next_cursor"]}),
        )
        .await,
        "invalid_cursor",
    );
    let child_page = call(&child_host, "history.list", json!({"limit":1})).await?;
    let child_record = &child_page["items"][0]["record"];
    assert_eq!(
        call(&child_host, "history.read", json!({"record":child_record})).await?["text"],
        "child private evidence"
    );
    assert_eq!(
        call(
            &fixture.host,
            "history.read",
            json!({"record":child_record})
        )
        .await?,
        Value::Null
    );
    let child_page = call(&child_host, "history.list", json!({"limit":16})).await?;
    let completed = child_page["items"]
        .as_array()
        .ok_or("missing child metadata")?
        .iter()
        .find(|item| item["kind"] == "cell_completed")
        .ok_or("child cell did not finish")?;
    let child_cell = call(
        &child_host,
        "history.read",
        json!({"record":completed["record"]}),
    )
    .await?;
    assert!(
        child_cell["text"]
            .as_str()
            .ok_or("missing child outcome")?
            .contains("\"printed\":[\"child own-agent retrieval succeeded\"]")
    );
    engine.shutdown().await?;
    fixture.manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn rewound_history_is_private_and_resumes_without_old_javascript_bindings()
-> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new().await?;
    let limits = StoreLimits {
        record_bytes: 128 * 1024,
    };
    let mut source = make_engine(
        fixture
            .attachment
            .root_store_with_settings(limits, effective_settings())?,
        fixture.host.clone(),
        Arc::new(Instructions),
        "var source_binding = 'old runtime'; print(source_binding);",
    )
    .await?;
    source
        .run(
            RunRequest {
                input: "retained prompt".into(),
            },
            CancellationToken::new(),
        )
        .await?;
    let abandoned = source
        .run(
            RunRequest {
                input: "abandoned unique continuation".into(),
            },
            CancellationToken::new(),
        )
        .await;
    assert!(abandoned.is_err(), "the scripted provider is exhausted");
    source.shutdown().await?;
    let before = fixture.store.load().await?;
    let points = fixture.attachment.rewind_points(limits).await?;
    let selected = points.last().ok_or("missing rewind point")?;
    assert_eq!(
        selected.input,
        UserInput::from("abandoned unique continuation")
    );
    let fork = fixture
        .attachment
        .fork_before(selected.record, limits)
        .await?;
    let manager = attach(fork.clone(), Arc::new(Factory::default())).await?;
    let host = manager.bind("fixture".into(), Arc::new(Inner))?;
    assert_eq!(
        call(
            &host,
            "history.read",
            json!({"record":i64::from(selected.record).to_string()}),
        )
        .await?,
        Value::Null
    );
    let search = call(
        &host,
        "history.search",
        json!({"query":"abandoned unique continuation"}),
    )
    .await?;
    assert_eq!(search["items"], json!([]));
    assert_eq!(search["complete"], true);
    let mut resumed = Engine::resume(
        EngineSettings::new(engine_limits())?,
        make_adapters(
            fork.root_store(limits)?,
            host,
            Arc::new(Instructions),
            "if (typeof source_binding !== 'undefined') throw Error('old binding survived'); print('fresh runtime');",
        )
        .await?,
        None,
    )
    .await?;
    let report = resumed
        .run(
            RunRequest {
                input: "replacement prompt".into(),
            },
            CancellationToken::new(),
        )
        .await?;
    assert_eq!(report.answer, "done");
    assert!(fork.root_store(limits)?.load().await?.records.iter().any(|stored| {
        matches!(&stored.record.kind, RecordKind::CellCompleted { outcome: CellResponse::Completed { printed }, .. } if printed == &vec![json!("fresh runtime")])
    }));
    assert_eq!(fixture.store.load().await?, before);
    resumed.shutdown().await?;
    manager.shutdown().await?;
    fixture.manager.shutdown().await?;
    Ok(())
}

struct Fixture {
    attachment: SessionAttachment,
    store: SqliteStore,
    manager: AgentManager,
    factory: Arc<Factory>,
    host: Arc<dyn CapabilityHost>,
    directory: tempfile::TempDir,
}

impl Fixture {
    async fn new() -> Result<Self, Box<dyn Error>> {
        let mut builder = tempfile::Builder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(std::fs::Permissions::from_mode(0o700));
        }
        let directory = builder.tempdir()?;
        let database = SessionDatabase::open(&directory.path().join("history.sqlite3")).await?;
        let attachment = database.create_session(directory.path()).await?;
        let store = attachment.root_store(StoreLimits {
            record_bytes: 256 * 1024 * 1024,
        })?;
        let factory = Arc::new(Factory::default());
        let manager = attach(attachment.clone(), factory.clone()).await?;
        let host = manager.bind("fixture".into(), Arc::new(Inner))?;
        Ok(Self {
            directory,
            attachment,
            store,
            manager,
            factory,
            host,
        })
    }

    async fn admin(&self) -> Result<SqliteConnection, sqlx::Error> {
        SqliteConnection::connect_with(
            &sqlx::sqlite::SqliteConnectOptions::new()
                .filename(self.directory.path().join("history.sqlite3")),
        )
        .await
    }
}

async fn attach(
    attachment: SessionAttachment,
    factory: Arc<Factory>,
) -> Result<AgentManager, CapabilityError> {
    AgentManager::attach(agent_limits(), engine_limits(), factory, attachment).await
}

async fn call(
    host: &Arc<dyn CapabilityHost>,
    name: &str,
    arguments: Value,
) -> Result<Value, CapabilityError> {
    host.call(
        name,
        arguments,
        work_limits(65536),
        CancellationToken::new(),
    )
    .await
}

fn rejection(result: Result<Value, CapabilityError>, expected: &str) {
    assert!(
        matches!(&result, Err(CapabilityError::Tool { code, effects:Effects::None, .. }) if code == expected),
        "{result:?}"
    );
}

fn accepted(text: &str) -> Record {
    Record {
        invocation: InvocationId(1),
        kind: RecordKind::InputAccepted { input: text.into() },
    }
}

fn work_limits(output_bytes: usize) -> WorkLimits {
    WorkLimits {
        output_bytes,
        deadline: tokio::time::Instant::now() + Duration::from_secs(5),
    }
}

fn agent_limits() -> AgentLimits {
    AgentLimits {
        active: 1,
        retained: 1,
    }
}

fn engine_limits() -> Limits {
    Limits {
        input_bytes: 8192,
        output_bytes: 65536,
        record_bytes: 128 * 1024,
        context_bytes: 1024 * 1024,
        pending_operations: 8,
        stall_timeout: Duration::from_secs(10),
        settlement_timeout: Duration::from_secs(2),
        shutdown_timeout: Duration::from_secs(2),
    }
}

fn effective_settings() -> EffectiveSessionSettings {
    EffectiveSessionSettings {
        alias: "fixture".into(),
        provider: "ollama".into(),
        protocol: "chat-completions-v1".into(),
        model: "fixture".into(),
        endpoint: None,
        account: None,
        thinking_level: None,
        reasoning_summary: false,
        execution_version: 1,
        aliases: BTreeMap::from([(
            "fixture".into(),
            EffectiveAliasSettings {
                provider: "ollama".into(),
                model: "fixture".into(),
                endpoint: None,
                account: None,
                thinking_level: None,
                reasoning_summary: false,
                output_tokens: None,
                context_tokens: None,
            },
        )]),
        limits: SessionExecutionLimits {
            input_bytes: 8192,
            output_bytes: 65536,
            record_bytes: 128 * 1024,
            context_bytes: 1024 * 1024,
            agents_active: 1,
            agents_retained: 1,
            pending_operations: 8,
            stall_timeout_ms: 10000,
            settlement_timeout_ms: 2000,
            shutdown_timeout_ms: 2000,
            message_steering: false,
        },
    }
}

async fn make_engine(
    store: SqliteStore,
    capabilities: Arc<dyn CapabilityHost>,
    context: Arc<dyn ContextAssembler>,
    code: &str,
) -> Result<Engine, Box<dyn Error>> {
    Ok(Engine::new(
        EngineSettings::new(engine_limits())?,
        make_adapters(store, capabilities, context, code).await?,
        None,
    ))
}

async fn make_adapters(
    store: SqliteStore,
    capabilities: Arc<dyn CapabilityHost>,
    context: Arc<dyn ContextAssembler>,
    code: &str,
) -> Result<EngineAdapters, Box<dyn Error>> {
    let executor = QuickJsExecutor::new(QuickJsOptions {
        memory_bytes: 8 * 1024 * 1024,
        stack_bytes: 256 * 1024,
        feedback: FeedbackLimits {
            output_bytes: 65536,
            printed_bytes: 8192,
            error_bytes: 2048,
            print_calls: 16,
        },
    })
    .await?;
    Ok(EngineAdapters {
        store: Arc::new(store),
        capabilities,
        context,
        executor: Box::new(executor),
        model: Arc::new(Scripted(Mutex::new(VecDeque::from([
            ModelAction::Execute {
                cells: vec![Cell { code: code.into() }],
            },
            ModelAction::Final {
                answer: "done".into(),
            },
        ])))),
    })
}

#[derive(Default)]
struct Factory {
    child: Mutex<Option<Arc<dyn CapabilityHost>>>,
}

#[async_trait]
impl AgentFactory for Factory {
    fn supports_alias(&self, alias: &str) -> bool {
        alias == "fixture"
    }

    async fn create(
        &self,
        participant: AgentParticipant,
        _alias: &str,
        _limits: Limits,
        _cancellation: CancellationToken,
    ) -> Result<Engine, CapabilityError> {
        let store = participant
            .durable_store(StoreLimits {
                record_bytes: 128 * 1024,
            })
            .await
            .map_err(|error| CapabilityError::Rejected {
                message: error.to_string(),
            })?
            .ok_or_else(|| CapabilityError::Rejected {
                message: "fixture requires durable storage".into(),
            })?;
        let host = participant.bind(Arc::new(Inner));
        *self.child.lock().await = Some(host.clone());
        let code = r#"
            const page = await capabilities.call('history.list', {limit:1});
            const evidence = await capabilities.call('history.read', {record:page.items[0].record});
            if (evidence.text !== 'child private evidence') throw Error('wrong child history');
            if (await capabilities.call('history.read', {record:'1'}) !== null) throw Error('foreign root history leaked');
            print('child own-agent retrieval succeeded');
        "#;
        make_engine(store, host, Arc::new(Instructions), code)
            .await
            .map_err(|error| CapabilityError::Rejected {
                message: error.to_string(),
            })
    }

    async fn close(&self, _name: &str) -> Result<(), CapabilityError> {
        Ok(())
    }
}

struct Instructions;

#[async_trait]
impl ContextAssembler for Instructions {
    async fn prepare(
        &self,
        records: Vec<StoredRecord>,
        capabilities: Vec<CapabilityDescription>,
        bytes: usize,
    ) -> Result<PreparedContext, ContextError> {
        let invocation = records.last().ok_or(ContextError::Limit)?.record.invocation;
        let mut prepared = BoundedContext.prepare(records, capabilities, bytes).await?;
        prepared.instruction_state = Some(InstructionState {
            invocation,
            scoped_sources: Vec::new(),
            selected_skills: Vec::new(),
            scoped_feedback: Vec::new(),
            skill_feedback: Vec::new(),
        });
        Ok(prepared)
    }

    async fn restore(&self, state: &InstructionState) -> Result<(), ContextError> {
        assert!(state.scoped_sources.is_empty());
        assert!(state.selected_skills.is_empty());
        assert!(state.scoped_feedback.is_empty());
        assert!(state.skill_feedback.is_empty());
        Ok(())
    }
}

struct Scripted(Mutex<VecDeque<ModelAction>>);

#[async_trait]
impl ModelProvider for Scripted {
    async fn respond(
        &self,
        _request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        let action = self
            .0
            .lock()
            .await
            .pop_front()
            .ok_or_else(|| ModelError::Protocol {
                message: "fixture exhausted".into(),
            })?;
        Ok(ModelResponse {
            action,
            continuation: None,
            usage: None,
        })
    }
}

struct Inner;

#[async_trait]
impl CapabilityHost for Inner {
    fn descriptions(&self) -> Vec<CapabilityDescription> {
        Vec::new()
    }

    async fn call(
        &self,
        name: &str,
        arguments: Value,
        _limits: WorkLimits,
        _cancellation: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        match name {
            "fixture.echo" => Ok(arguments),
            _ => Err(CapabilityError::Tool {
                code: "inner_error".into(),
                message: "unchanged inner error".into(),
                effects: Effects::Partial,
            }),
        }
    }

    async fn call_started(
        &self,
        name: &str,
        arguments: Value,
        limits: WorkLimits,
        cancellation: CancellationToken,
        _observer: CapabilityObserver,
        operation_record: RecordId,
    ) -> Result<Value, CapabilityError> {
        assert!(i64::from(operation_record) > 0);
        let mut value = self.call(name, arguments, limits, cancellation).await?;
        value["started_dispatch"] = json!(true);
        Ok(value)
    }
}
