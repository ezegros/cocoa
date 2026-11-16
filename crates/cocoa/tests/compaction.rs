//! Real SQLite, provider HTTP, instruction assembly, tools and QuickJS continuation.

use std::{
    collections::BTreeMap,
    error::Error,
    fs,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use cocoa::persistence::{
    EffectiveAliasSettings, EffectiveSessionSettings, HistoryReadLimits, SessionDatabase,
    SessionExecutionLimits, SqliteStore,
};
use cocoa_core::{
    compaction::{Checkpoint, CheckpointLimits, CheckpointProposal, CompactionPolicy, Recap},
    engine::{Engine, EngineAdapters, EngineSettings, Limits, RunRequest},
    execution::CellResponse,
    model::{Image, ModelAction, ModelResponse},
    store::{
        InvocationId, InvocationOutcome, Record, RecordId, RecordKind, Store, StoreError,
        StoreLimits,
    },
};
use cocoa_llm::{Provider, ProviderKind, ProviderOptions, openai::ExecutionMode};
use cocoa_quickjs::{FeedbackLimits, QuickJsExecutor, QuickJsOptions};
use cocoa_tools::{
    CodingCapabilities, CodingLimits, LocalConfiguration, LocalEnvironment, ProjectEnvironment,
};
use serde_json::{Value, json};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use tokio_util::sync::CancellationToken;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate, matchers::method};

const USER: &str = "Never publish. Pause the benchmark until I ask. Build automatic compaction now; preserve the future Jev classifier stage.";

#[tokio::test]
async fn automatic_http_checkpoints_preserve_live_runtime_tools_images_and_full_history()
-> Result<(), Box<dyn Error>> {
    for automatic in [false, true] {
        run_http_continuation(automatic).await?;
    }
    Ok(())
}

#[tokio::test]
async fn publication_rejection_abandonment_and_selective_reads_preserve_originals()
-> Result<(), Box<dyn Error>> {
    let directory = private_directory()?;
    let path = directory.path().join("publication.sqlite3");
    let database = SessionDatabase::open(&path).await?;
    let session = database.create_session(directory.path()).await?;
    let store = session.root_store(StoreLimits {
        record_bytes: 512 * 1024,
    })?;
    append_record(
        &store,
        1,
        RecordKind::InputAccepted {
            input: "Original requirements remain.".into(),
        },
    )
    .await?;
    let older = append_record(&store, 1, final_answer(&"older evidence ".repeat(4000))).await?;
    let through = append_record(&store, 1, final_answer("First settled boundary")).await?;
    append_record(
        &store,
        1,
        RecordKind::InvocationFinished {
            outcome: InvocationOutcome::Completed,
        },
    )
    .await?;
    let stale = prepare_checkpoint(&store, through).await?;
    append_record(
        &store,
        2,
        RecordKind::InputAccepted {
            input: "New steering".into(),
        },
    )
    .await?;
    assert_eq!(
        store.publish_checkpoint(&stale).await,
        Err(StoreError::InvalidCheckpoint)
    );
    assert!(store.load().await?.checkpoints.is_empty());
    let candidate = prepare_checkpoint(&store, through).await?;
    let mut admin =
        SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path)).await?;
    sqlx::raw_sql("CREATE TRIGGER reject_checkpoint BEFORE INSERT ON checkpoints BEGIN SELECT RAISE(ABORT,'fixture publication rejection'); END;").execute(&mut admin).await?;
    assert!(store.publish_checkpoint(&candidate).await.is_err());
    assert!(store.load().await?.checkpoints.is_empty());
    sqlx::query("DROP TRIGGER reject_checkpoint")
        .execute(&mut admin)
        .await?;
    sqlx::raw_sql("CREATE TRIGGER reject_archive BEFORE UPDATE OF payload_json ON records WHEN NEW.payload_json IS NULL BEGIN SELECT RAISE(ABORT,'fixture archive rejection'); END;").execute(&mut admin).await?;
    let first = store.publish_checkpoint(&candidate).await?;
    assert_eq!(store.load().await?.checkpoints.len(), 1);
    assert_eq!(database.storage_usage().await?.archive_bytes, 0);
    assert!(store.read_record(older, 512 * 1024).await?.is_some());
    sqlx::query("DROP TRIGGER reject_archive")
        .execute(&mut admin)
        .await?;
    let usage = store.maintain_storage().await?;
    assert!(usage.archive_bytes > 0 && usage.archive_bytes < usage.archive_decoded_bytes);
    let original = store
        .read_record(older, 512 * 1024)
        .await?
        .ok_or("missing original")?;
    let active = store.load_active().await?;
    assert_eq!(active.checkpoint().map(|stored| stored.id), Some(first));
    assert!(!active.records().iter().any(|stored| stored.id == older));
    let digest: Vec<u8> = sqlx::query_scalar("SELECT archive_sha256 FROM records WHERE id = ?")
        .bind(i64::from(older))
        .fetch_one(&mut admin)
        .await?;
    sqlx::query("UPDATE records SET archive_sha256 = zeroblob(32) WHERE id = ?")
        .bind(i64::from(older))
        .execute(&mut admin)
        .await?;
    assert!(store.load_active().await.is_ok());
    assert!(store.read_record(older, 512 * 1024).await.is_err());
    assert!(store.load().await.is_err());
    sqlx::query("UPDATE records SET archive_sha256 = ? WHERE id = ?")
        .bind(digest)
        .bind(i64::from(older))
        .execute(&mut admin)
        .await?;
    assert_eq!(
        store.read_record(older, 512 * 1024).await?.as_ref(),
        Some(&original)
    );
    let through = append_record(&store, 2, final_answer("Second settled boundary")).await?;
    append_record(
        &store,
        2,
        RecordKind::InvocationFinished {
            outcome: InvocationOutcome::Completed,
        },
    )
    .await?;
    let candidate = prepare_checkpoint(&store, through).await?;
    let blocked = admin.begin_with("BEGIN IMMEDIATE").await?;
    let mut publication = store.publish_checkpoint(&candidate);
    assert!(matches!(
        futures_util::poll!(&mut publication),
        std::task::Poll::Pending
    ));
    drop(publication);
    blocked.rollback().await?;
    assert!(matches!(
        append_record(
            &store,
            3,
            RecordKind::InputAccepted {
                input: "Not before reconciliation".into()
            }
        )
        .await,
        Err(StoreError::Backend { .. })
    ));
    let reconciled = store.load().await?;
    assert_eq!(reconciled.checkpoints.len(), 2);
    assert_eq!(reconciled.checkpoints[1].checkpoint, candidate);
    assert_eq!(
        store.publish_checkpoint(&candidate).await,
        Err(StoreError::InvalidCheckpoint)
    );
    let later = append_record(
        &store,
        3,
        RecordKind::InputAccepted {
            input: "After reconciliation".into(),
        },
    )
    .await?;
    assert!(
        reconciled
            .checkpoints
            .iter()
            .all(|checkpoint| checkpoint.id != later)
    );
    let encoded = serde_json::to_string(&candidate)?;
    let mut missing_predecessor = serde_json::to_value(&candidate)?;
    missing_predecessor["content"]["predecessor"] = json!(i64::MAX);
    for (version, corrupt) in [
        (2, encoded.clone()),
        (1, "{}".into()),
        (1, missing_predecessor.to_string()),
    ] {
        sqlx::query(
            "UPDATE checkpoints SET format_version = ?, payload_json = ? WHERE sequence = 2",
        )
        .bind(version)
        .bind(corrupt)
        .execute(&mut admin)
        .await?;
        assert!(store.load_active().await.is_err());
        assert!(store.load().await.is_err());
        assert!(
            session
                .load(StoreLimits {
                    record_bytes: 512 * 1024
                })
                .await
                .is_err()
        );
        assert_eq!(
            store.read_record(older, 512 * 1024).await?.as_ref(),
            Some(&original)
        );
    }
    sqlx::query("UPDATE checkpoints SET format_version = 1, payload_json = ? WHERE sequence = 2")
        .bind(encoded)
        .execute(&mut admin)
        .await?;
    assert_eq!(store.load().await?.checkpoints, reconciled.checkpoints);
    admin.close().await?;
    Ok(())
}

#[tokio::test]
async fn archive_catch_up_reclaims_space_without_discarding_evidence() -> Result<(), Box<dyn Error>>
{
    let directory = private_directory()?;
    let path = directory.path().join("storage.sqlite3");
    let database = SessionDatabase::open(&path).await?;
    let session = database.create_session(directory.path()).await?;
    let store = session.root_store(StoreLimits {
        record_bytes: 8 * 1024 * 1024,
    })?;
    append_record(
        &store,
        1,
        RecordKind::InputAccepted {
            input: "Retain both original outputs".into(),
        },
    )
    .await?;
    let first = append_record(&store, 1, final_answer(&"A".repeat(5 * 1024 * 1024))).await?;
    let second = append_record(&store, 1, final_answer(&"B".repeat(5 * 1024 * 1024))).await?;
    let through = append_record(&store, 1, final_answer("Ready")).await?;
    append_record(
        &store,
        1,
        RecordKind::InvocationFinished {
            outcome: InvocationOutcome::Completed,
        },
    )
    .await?;
    let before = database.storage_usage().await?;
    let candidate = prepare_checkpoint(&store, through).await?;
    store.publish_checkpoint(&candidate).await?;
    let partial = database.storage_usage().await?;
    assert!(partial.inline_bytes > 5 * 1024 * 1024);
    let after = store.maintain_storage().await?;
    assert!(after.archive_decoded_bytes >= 10 * 1024 * 1024);
    assert!(after.inline_bytes < 1024);
    assert!(after.archive_bytes < 32 * 1024);
    assert_eq!(after.database_bytes, fs::metadata(&path)?.len());
    assert_eq!(
        after.wal_bytes,
        fs::metadata(path.with_extension("sqlite3-wal"))?.len()
    );
    assert!(after.database_bytes < before.database_bytes);
    assert!(after.wal_bytes <= 8 * 1024 * 1024);
    for (source, character) in [(first, 'A'), (second, 'B')] {
        let record = store
            .read_record(source, 8 * 1024 * 1024)
            .await?
            .ok_or("missing archived evidence")?;
        assert!(
            matches!(record.record.kind, RecordKind::ModelCompleted { response: ModelResponse { action: ModelAction::Final { answer }, .. } } if answer.len() == 5 * 1024 * 1024 && answer.chars().all(|value| value == character))
        );
    }
    println!("storage before={before:?}; after bounded catch-up={after:?}");
    Ok(())
}

async fn run_http_continuation(automatic: bool) -> Result<(), Box<dyn Error>> {
    let directory = private_directory()?;
    let root = fs::canonicalize(directory.path())?;
    fs::write(
        root.join("AGENTS.md"),
        "Keep effects in this temporary project.\n",
    )?;
    let server = MockServer::start().await;
    let fixture = Arc::new(CompactionFixture::default());
    Mock::given(method("POST"))
        .respond_with(CompactionResponder(fixture.clone()))
        .mount(&server)
        .await;
    let provider = Provider::new(ProviderOptions {
        kind: ProviderKind::OpenAi,
        endpoint: format!("{}/v1/responses", server.uri()),
        model: "compaction-integration-fixture".into(),
        api_key: Some("fixture-only".into()),
        execution: ExecutionMode::Enabled,
        thinking_level: None,
        session_id: "compaction-integration".into(),
    })?;
    let environment = local_environment(root.clone()).await?;
    let path = root.join("session.sqlite3");
    let database = SessionDatabase::open(&path).await?;
    let session = database.create_session(&root).await?;
    let store_limits = StoreLimits {
        record_bytes: 512 * 1024,
    };
    let store = Arc::new(session.root_store_with_settings(store_limits, effective_settings())?);
    let policy = CompactionPolicy {
        request_bytes: 120 * 1024,
        trigger_bytes: 40 * 1024,
        target_bytes: 30 * 1024,
        summary_input_bytes: 512 * 1024,
        summary_output_tokens: 4096,
        sources: 256,
        checkpoint: CheckpointLimits {
            bytes: 16 * 1024,
            required_entries: 32,
            items: 32,
        },
        timeout: Duration::from_secs(5),
    };
    let settings = engine_settings(automatic, policy)?;
    let mut engine = Engine::new(
        settings,
        EngineAdapters {
            model: Arc::new(provider),
            executor: Box::new(QuickJsExecutor::new(quickjs_options()).await?),
            capabilities: Arc::new(CodingCapabilities::new(environment.clone())),
            context: environment.clone(),
            store: store.clone(),
        },
        None,
    );
    let image: Image = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a/aAAAAAASUVORK5CYII=".to_owned().try_into()?;
    let result = engine
        .run(
            RunRequest {
                input: cocoa_core::model::UserInput::WithImages {
                    text: USER.into(),
                    images: vec![image.clone()],
                },
            },
            CancellationToken::new(),
        )
        .await;
    let cleanup = engine.shutdown().await;
    let environment_cleanup = environment.close().await;
    let result = result?;
    assert_eq!(result.answer, "continued after two checkpoints");
    let usage = result.usage.as_ref().ok_or("missing aggregate usage")?;
    assert_eq!(usage.input_tokens, if automatic { 246 } else { 44 });
    assert_eq!(usage.output_tokens, if automatic { 26 } else { 12 });
    assert_eq!(
        result.accounting.requests_started,
        if automatic { 6 } else { 4 }
    );
    let export = session
        .export_run(cocoa::persistence::ExportLimits::default())
        .await?;
    assert_eq!(export.accounting, result.accounting);
    assert!(export.ledger_complete);
    cleanup?;
    environment_cleanup?;
    assert_eq!(fs::read_to_string(root.join("effects.txt"))?, "once\n");
    let snapshot = store.load().await?;
    snapshot.validate_checkpoints()?;
    let checkpoints_expected = if automatic { 2 } else { 0 };
    assert_eq!(snapshot.checkpoints.len(), checkpoints_expected);
    for checkpoint in &snapshot.checkpoints {
        let content = checkpoint.checkpoint.content();
        assert_eq!(content.required.len(), 2);
        assert!(
            content
                .recap
                .stages
                .iter()
                .any(|stage| stage.key == "jev" && stage.text.contains("Jev"))
        );
    }
    if automatic {
        assert_eq!(
            snapshot.checkpoints[0].checkpoint.content().required,
            snapshot.checkpoints[1].checkpoint.content().required
        );
    }
    let results: Vec<_> = snapshot
        .records
        .iter()
        .filter_map(|stored| match &stored.record.kind {
            RecordKind::CellCompleted {
                outcome: CellResponse::Completed { printed },
                ..
            } => Some(printed),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 3);
    assert_eq!(results[1][0], 96);
    assert_eq!(results[2][0], 27);
    assert_eq!(
        results[0].last().and_then(Value::as_str).map(str::len),
        Some(48000)
    );
    assert_eq!(results[1][1].as_str().map(str::len), Some(48000));
    assert_eq!(snapshot.records.iter().filter(|stored| matches!(&stored.record.kind, RecordKind::OperationStarted { name, .. } if name == "cocoa.run")).count(), 1);
    assert_eq!(snapshot.records.iter().filter(|stored| matches!(&stored.record.kind, RecordKind::InputAccepted { input } if input.images() == [image.clone()])).count(), 1);
    assert_eq!(
        fixture.summaries.load(Ordering::SeqCst),
        checkpoints_expected
    );
    {
        let requests = fixture
            .requests
            .lock()
            .map_err(|_| "fixture lock poisoned")?;
        assert_eq!(requests.len(), 4);
        for (index, request) in requests.iter().enumerate() {
            if automatic {
                assert!(request.to_string().len() <= policy.request_bytes);
            }
            let encoded = request.to_string();
            assert_eq!(
                encoded.matches(image.data_url()).count(),
                1,
                "image changed or duplicated in request {index}"
            );
            assert_eq!(
                encoded.matches(USER).count(),
                1,
                "current user input duplicated in request {index}"
            );
            if !automatic {
                assert!(!encoded.contains("checkpoint_projection"));
                if index > 0 {
                    assert!(encoded.contains(&"A".repeat(48000)));
                }
                if index > 1 {
                    assert!(encoded.contains(&"B".repeat(48000)));
                }
            }
            if automatic && index > 0 {
                assert!(encoded.contains("checkpoint_projection"));
                assert!(!encoded.contains(&"A".repeat(48000)));
            }
            if automatic && index > 1 {
                assert!(!encoded.contains(&"B".repeat(48000)));
            }
        }
        println!(
            "automatic={automatic}; encoded request bytes={:?}; checkpoints={}; shell effects=1; closure results=96,27; captured images per request=1",
            requests
                .iter()
                .map(|request| request.to_string().len())
                .collect::<Vec<_>>(),
            snapshot.checkpoints.len()
        );
    }
    let mut inspection =
        SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path)).await?;
    let storage: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT coalesce(sum(octet_length(payload_json)),0), coalesce(sum(octet_length(archive_zlib)),0), coalesce(sum(archive_decoded_bytes),0), count(archive_zlib) FROM records",
    ).fetch_one(&mut inspection).await?;
    if automatic {
        assert!(storage.3 > 0);
        assert!(storage.1 < storage.2);
        assert!(storage.2 >= 96_000);
    } else {
        assert_eq!(storage.3, 0);
        assert_eq!(storage.1, 0);
    }
    let database_bytes: i64 = sqlx::query_scalar(
        "SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()",
    )
    .fetch_one(&mut inspection)
    .await?;
    println!(
        "automatic={automatic}; inline payload bytes={}; archive bytes={}; archive decoded bytes={}; archive records={}; database allocated bytes={database_bytes}; wal bytes={}",
        storage.0,
        storage.1,
        storage.2,
        storage.3,
        fs::metadata(path.with_extension("sqlite3-wal")).map_or(0, |metadata| metadata.len())
    );
    inspection.close().await?;
    let first_page = store
        .read_history(None, HistoryReadLimits::new(2, 512 * 1024)?)
        .await?;
    let cursor = first_page
        .next_cursor
        .ok_or("missing pinned history cursor")?;
    let reference = session.public_id().to_string();
    drop(environment);
    drop(store);
    drop(session);
    drop(database);
    let database = SessionDatabase::open_existing(&path)
        .await?
        .ok_or("missing durable database")?;
    let session = database.attach_session(&reference, store_limits).await?;
    let store = Arc::new(session.root_store_with_settings(store_limits, effective_settings())?);
    let restored = store.load().await?;
    assert_eq!(restored, snapshot);
    let session_snapshot = session.load(store_limits).await?;
    assert_eq!(
        session_snapshot.histories.get(&session.root_agent()),
        Some(&snapshot)
    );
    let continuation = store
        .read_history(Some(cursor), HistoryReadLimits::new(64, 512 * 1024)?)
        .await?;
    assert_eq!(continuation.records, snapshot.records[2..]);
    for original in &snapshot.records {
        assert_eq!(
            store.read_record(original.id, 512 * 1024).await?.as_ref(),
            Some(original)
        );
    }
    let environment = local_environment(root.clone()).await?;
    let provider = Provider::new(ProviderOptions {
        kind: ProviderKind::OpenAi,
        endpoint: format!("{}/v1/responses", server.uri()),
        model: "compaction-integration-fixture".into(),
        api_key: Some("fixture-only".into()),
        execution: ExecutionMode::Enabled,
        thinking_level: None,
        session_id: "compaction-integration".into(),
    })?;
    let mut resumed = Engine::resume(
        engine_settings(automatic, policy)?,
        EngineAdapters {
            model: Arc::new(provider),
            executor: Box::new(QuickJsExecutor::new(quickjs_options()).await?),
            capabilities: Arc::new(CodingCapabilities::new(environment.clone())),
            context: environment.clone(),
            store: store.clone(),
        },
        None,
    )
    .await?;
    let result = resumed
        .run(
            RunRequest {
                input: "Continue without replaying earlier effects.".into(),
            },
            CancellationToken::new(),
        )
        .await;
    let shutdown = resumed.shutdown().await;
    let close = environment.close().await;
    assert_eq!(result?.answer, "resumed without replay");
    shutdown?;
    close?;
    assert_eq!(fs::read_to_string(root.join("effects.txt"))?, "once\n");
    let resumed_history = store.load().await?;
    assert_eq!(resumed_history.checkpoints, snapshot.checkpoints);
    assert!(resumed_history.records.iter().any(|stored| matches!(&stored.record.kind, RecordKind::CellCompleted { outcome: CellResponse::Completed { printed }, .. } if printed == &[json!("undefined")])));
    assert_eq!(
        fixture.summaries.load(Ordering::SeqCst),
        checkpoints_expected
    );
    let requests = fixture
        .requests
        .lock()
        .map_err(|_| "fixture lock poisoned")?;
    assert_eq!(requests.len(), 6);
    assert!(requests[4].to_string().contains("runtime-restarted"));
    println!(
        "automatic={automatic}; database reopen preserved all originals and checkpoints; resumed runtime bindings reset; shell effects still=1"
    );
    Ok(())
}

#[derive(Default)]
struct CompactionFixture {
    summaries: AtomicUsize,
    requests: Mutex<Vec<Value>>,
}

struct CompactionResponder(Arc<CompactionFixture>);

impl Respond for CompactionResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let fixture = &self.0;
        let body: Value = match request.body_json() {
            Ok(body) => body,
            Err(error) => panic!("invalid request: {error}"),
        };
        assert_eq!(body["model"], "compaction-integration-fixture");
        let output = if body["tool_choice"] == "none" {
            assert!(body.get("tools").is_some());
            assert_eq!(body["parallel_tool_calls"], false);
            assert_eq!(body["max_output_tokens"], 4096);
            let input: Value = match body["input"]
                .as_array()
                .and_then(|input| input.last())
                .and_then(|input| input["content"].as_str())
                .map(serde_json::from_str)
            {
                Some(Ok(input)) => input,
                _ => panic!("invalid summary input"),
            };
            assert!(!input.to_string().contains("data:image/png;"));
            let requests = match fixture.requests.lock() {
                Ok(requests) => requests,
                Err(error) => panic!("fixture lock poisoned: {error}"),
            };
            let Some(previous) = requests.last() else {
                panic!("summary must follow ordinary work");
            };
            assert_eq!(body["instructions"], previous["instructions"]);
            assert_eq!(body["tools"], previous["tools"]);
            let prefix = previous["input"].as_array().map(Vec::as_slice);
            let fork = body["input"].as_array().map(Vec::as_slice);
            assert!(
                matches!((prefix, fork), (Some(prefix), Some(fork)) if fork.starts_with(prefix))
            );
            drop(requests);
            let number = fixture.summaries.fetch_add(1, Ordering::SeqCst);
            let source = input["sources"]
                .as_array()
                .and_then(|sources| sources.iter().find(|source| source["kind"] == "user"))
                .map(|source| source["source"].clone())
                .unwrap_or(Value::Null);
            let origin =
                |text: &str| json!({"User":{"quotations":[{"source":source,"text":text}]}});
            let additions = if number == 0 {
                json!([
                    {"origin":origin("Never publish."),"scope":"This task","request":null},
                    {"origin":origin("Pause the benchmark until I ask."),"scope":"Benchmark","request":{"Paused":{"instruction":origin("Pause the benchmark until I ask."),"resume_when":"The user asks"}}}
                ])
            } else {
                json!([])
            };
            let proposal = json!({"additions":additions,"changes":[],"recap":{"goal":"Automatic compaction preserving evidence and runtime","stages":[{"key":"automatic","text":"Complete automatic compaction","status":"Active","prerequisites":[],"sources":[],"revisit_when":null},{"key":"jev","text":"Later add Jev classification and ranking","status":"Deferred","prerequisites":["automatic"],"sources":[],"revisit_when":"After the deterministic baseline is measured"}],"current_stage":"automatic","decisions":[],"progress":[],"failures":[],"next_actions":["Continue using calculate without repeating the effect"]},"evidence":[]});
            message(&proposal.to_string())
        } else {
            let mut requests = match fixture.requests.lock() {
                Ok(requests) => requests,
                Err(error) => panic!("fixture lock poisoned: {error}"),
            };
            let index = requests.len();
            requests.push(body.clone());
            match index {
                0 => call(
                    index,
                    "var offset = 5; var calculate = (left, right) => left * right + offset; var effect = await cocoa.run({command:\"printf 'once\\n' >> effects.txt\"}); print(await cocoa.wait(effect, {timeout_ms:1000})); print('A'.repeat(48000));",
                ),
                1 => call(index, "print(calculate(13, 7)); print('B'.repeat(48000));"),
                2 => call(index, "print(calculate(2, 11));"),
                3 => message("continued after two checkpoints"),
                4 => call(index, "print(typeof calculate);"),
                5 => message("resumed without replay"),
                _ => panic!("unexpected request {index}"),
            }
        };
        let usage = if body["tool_choice"] == "none" {
            json!({"input_tokens":101,"output_tokens":7})
        } else {
            json!({"input_tokens":11,"output_tokens":3})
        };
        let event = json!({"type":"response.completed","response":{"id":"compaction-fixture","status":"completed","error":null,"incomplete_details":null,"output":[output],"usage":usage}});
        ResponseTemplate::new(200).set_body_raw(
            format!("event: response.completed\ndata: {event}\n\n"),
            "text/event-stream",
        )
    }
}

async fn prepare_checkpoint(
    store: &SqliteStore,
    through: RecordId,
) -> Result<Checkpoint, Box<dyn Error>> {
    Ok(Checkpoint::prepare(
        &store.load().await?,
        through,
        CheckpointProposal {
            additions: Vec::new(),
            changes: Vec::new(),
            recap: Recap {
                goal: "Preserve exact evidence".into(),
                stages: Vec::new(),
                current_stage: None,
                decisions: Vec::new(),
                progress: Vec::new(),
                failures: Vec::new(),
                next_actions: vec!["Continue without replay".into()],
            },
            evidence: Vec::new(),
        },
        CheckpointLimits {
            bytes: 16_384,
            required_entries: 32,
            items: 32,
        },
    )?)
}

async fn append_record(
    store: &SqliteStore,
    invocation: u64,
    kind: RecordKind,
) -> Result<RecordId, StoreError> {
    store
        .append(
            &Record {
                invocation: InvocationId(invocation),
                kind,
            },
            None,
        )
        .await
}

fn final_answer(answer: &str) -> RecordKind {
    RecordKind::ModelCompleted {
        response: ModelResponse {
            action: ModelAction::Final {
                answer: answer.into(),
            },
            continuation: None,
            usage: None,
        },
    }
}

fn message(text: &str) -> Value {
    json!({"id":"message","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":text,"annotations":[]}]})
}

fn call(index: usize, code: &str) -> Value {
    json!({"id":format!("function_{index}"),"type":"function_call","status":"completed","call_id":format!("call_{index}"),"name":"exec","arguments":json!({"code":code}).to_string()})
}

fn private_directory() -> std::io::Result<tempfile::TempDir> {
    let mut builder = tempfile::Builder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(fs::Permissions::from_mode(0o700));
    }
    builder.tempdir()
}

async fn local_environment(
    root: std::path::PathBuf,
) -> Result<Arc<ProjectEnvironment>, Box<dyn Error>> {
    Ok(Arc::new(ProjectEnvironment::new(
        LocalEnvironment::new(
            LocalConfiguration {
                working_directory: root.clone(),
                shell: "/bin/bash".into(),
                environment: BTreeMap::from([("PATH".into(), "/usr/bin:/bin".into())]),
                output_directory: root.clone(),
            },
            CodingLimits {
                file_bytes: 1024 * 1024,
                observed_bytes: 2 * 1024 * 1024,
                observations: 32,
                processes: 2,
                captured_output_bytes: 65536,
                retained_output_bytes: 1024 * 1024,
                preview_lines: 200,
                default_timeout: Duration::from_secs(3),
            },
        )
        .await?,
        Some(root),
    )?))
}

fn engine_settings(
    automatic: bool,
    policy: CompactionPolicy,
) -> Result<EngineSettings, cocoa_core::engine::SettingsError> {
    let settings = EngineSettings::new(Limits {
        input_bytes: 8192,
        output_bytes: 128 * 1024,
        record_bytes: 512 * 1024,
        context_bytes: 1024 * 1024,
        pending_operations: 4,
        stall_timeout: Duration::from_secs(10),
        settlement_timeout: Duration::from_secs(2),
        shutdown_timeout: Duration::from_secs(2),
    })?;
    if automatic {
        settings.with_compaction(policy)
    } else {
        Ok(settings)
    }
}

fn quickjs_options() -> QuickJsOptions {
    QuickJsOptions {
        memory_bytes: 16 * 1024 * 1024,
        stack_bytes: 256 * 1024,
        feedback: FeedbackLimits {
            output_bytes: 128 * 1024,
            printed_bytes: 64 * 1024,
            error_bytes: 2048,
            print_calls: 16,
        },
    }
}

fn effective_settings() -> EffectiveSessionSettings {
    EffectiveSessionSettings {
        alias: "main".into(),
        provider: "openai".into(),
        protocol: "responses".into(),
        model: "compaction-integration-fixture".into(),
        endpoint: None,
        account: None,
        thinking_level: None,
        reasoning_summary: false,
        execution_version: 1,
        aliases: BTreeMap::from([(
            "main".into(),
            EffectiveAliasSettings {
                provider: "openai".into(),
                model: "compaction-integration-fixture".into(),
                endpoint: None,
                account: None,
                thinking_level: None,
                reasoning_summary: false,
                output_tokens: None,
            },
        )]),
        limits: SessionExecutionLimits {
            input_bytes: 8192,
            output_bytes: 128 * 1024,
            record_bytes: 512 * 1024,
            context_bytes: 1024 * 1024,
            agents_active: 4,
            agents_retained: 8,
            pending_operations: 4,
            stall_timeout_ms: 10000,
            settlement_timeout_ms: 2000,
            shutdown_timeout_ms: 2000,
            message_steering: true,
        },
    }
}
