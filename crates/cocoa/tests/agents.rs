//! Agent ownership and retained results through public hosts and real QuickJS.

use std::{
    collections::{BTreeMap, VecDeque},
    error::Error,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use cocoa::agents::{AgentFactory, AgentLimits, AgentManager, AgentParticipant, AgentUpdateKind};
use cocoa::persistence::{AgentAssignment, SessionDatabase, SqliteStore};
use cocoa_core::{
    capability::{
        CapabilityDescription, CapabilityError, CapabilityHost, Effects, OperationContext,
    },
    context::{BoundedContext, ContextAssembler, ContextError, InstructionState, PreparedContext},
    engine::{
        ActivityStatus, Engine, EngineAdapters, EngineSettings, Limits, Observation, RunError,
        RunRequest,
    },
    execution::{
        Cell, CellExecutor, CellResponse, ExecutionError, FeedbackError, ShutdownReport, WorkLimits,
    },
    model::{ModelAction, ModelError, ModelProvider, ModelRequest, ModelResponse, ProviderData},
    store::{InMemoryStore, RecordKind, Store, StoreLimits, StoredRecord},
};
use cocoa_quickjs::{FeedbackLimits, QuickJsExecutor, QuickJsOptions};
use cocoa_tools::{CodingLimits, LocalConfiguration, LocalEnvironment, ProjectEnvironment};
use serde_json::{Value, json};
use tokio::{
    sync::{Mutex, Semaphore},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

#[path = "support/report_capture.rs"]
mod report_capture;

#[path = "support/process.rs"]
mod process;

use process::CommandExt;

#[tokio::test]
async fn equal_assignments_with_duplicate_names_retain_separate_results()
-> Result<(), Box<dyn Error>> {
    let model = Arc::new(Script::new(
        vec![Step::Answer("first answer"), Step::Answer("second answer")],
        false,
    ));
    let factory = Arc::new(Factory::new([("researcher", model)]));
    let manager = manager(factory.clone(), 1, 2)?;
    let host = manager.bind("medium".into(), Arc::new(EmptyHost))?;
    let first = call(
        &host,
        "cocoa.spawn",
        json!({"name":"researcher","prompt":"same work"}),
    )
    .await?;
    assert_eq!(first["agent_id"], "2");
    assert_eq!(
        wait(&host, json!({"first":first["reference"]}), None, 4096).await?["first"]["answer"],
        "first answer"
    );
    let second = call(
        &host,
        "cocoa.spawn",
        json!({"name":"researcher","prompt":"same work"}),
    )
    .await?;
    assert_eq!(second["agent_id"], "3");
    assert_ne!(first["reference"], second["reference"]);
    let outcomes = wait(
        &host,
        json!({"first":first["reference"],"second":second["reference"]}),
        None,
        4096,
    )
    .await?;
    assert_eq!(outcomes["first"]["answer"], "first answer");
    assert_eq!(outcomes["second"]["answer"], "second answer");
    let retained = manager.retained_results().await?;
    assert_eq!(retained[0]["agent_id"], "2");
    assert_eq!(retained[0]["outcome"], outcomes["first"]);
    assert_eq!(retained[1]["agent_id"], "3");
    assert_eq!(retained[1]["outcome"], outcomes["second"]);
    assert_eq!(factory.created.load(Ordering::SeqCst), 2);
    manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn durable_children_restore_exact_results_and_ownership_without_restarting_runtimes()
-> Result<(), Box<dyn Error>> {
    use cocoa::persistence::{
        EffectiveAliasSettings, EffectiveSessionSettings, SessionExecutionLimits,
    };
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()?;
    let path = directory.path().join("sessions.sqlite3");
    let database = SessionDatabase::open(&path).await?;
    let attachment = database.create_session(directory.path()).await?;
    let public_id = attachment.public_id().as_str().to_owned();
    let store_limits = StoreLimits {
        record_bytes: limits().record_bytes,
    };
    let child = Arc::new(Script::new(
        vec![
            Step::Batch(vec![Cell {
                code: "var saved = await context.set('finding',{type:'observation',value:17}); print(saved.owner);".into(),
            }]),
            Step::Answer("first answer"),
            Step::InterruptedCell(
                "var saved = await context.get('finding'); var denied; try { await context.set('finding',{type:'observation',value:91}); } catch(error) { denied = error.code; } print({owner:saved.owner,denied});",
            ),
            Step::Answer("second answer"),
        ],
        false,
    ));
    let factory = Arc::new(Factory::new([("researcher", child)]));
    let manager = AgentManager::attach(
        AgentLimits {
            active: 1,
            retained: 2,
        },
        limits(),
        factory.clone(),
        attachment.clone(),
    )
    .await?;
    let effective = EffectiveSessionSettings {
        alias: "medium".into(),
        provider: "ollama".into(),
        protocol: "chat-completions-v1".into(),
        model: "fixture".into(),
        endpoint: None,
        account: None,
        thinking_level: None,
        reasoning_summary: false,
        execution_version: 1,
        aliases: BTreeMap::from([(
            "medium".into(),
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
            input_bytes: 4096,
            output_bytes: 4096,
            record_bytes: u64::try_from(limits().record_bytes)?,
            context_bytes: 1024 * 1024,
            agents_active: 1,
            agents_retained: 2,
            pending_operations: 8,
            stall_timeout_ms: 3000,
            settlement_timeout_ms: 500,
            shutdown_timeout_ms: 500,
            message_steering: false,
        },
    };
    let store = attachment.root_store_with_settings(store_limits, effective)?;
    let root = Arc::new(Script::new(
        vec![
            Step::Cell(
                "var first = await cocoa.spawn({name:'researcher',prompt:'same work'}); print((await cocoa.wait(first)).answer); var second = await cocoa.spawn({name:'researcher',prompt:'same work'}); print((await cocoa.wait(second)).answer);",
                vec![json!("first answer"), json!("second answer")],
            ),
            Step::InterruptedCell(
                "try { await context.set('',{type:'decision',value:1}); } catch(error) { print(error.code); } try { await context.set('bad_labels',{type:'decision',value:1,labels:['']}); } catch(error) { print(error.code); }",
            ),
            Step::Answer("collected"),
        ],
        false,
    ));
    let (mut engine, environment) =
        persisted_engine(root, &manager, store.clone(), directory.path(), false).await?;
    assert_eq!(
        engine
            .run(
                RunRequest {
                    input: "delegate".into()
                },
                CancellationToken::new()
            )
            .await?
            .answer,
        "collected"
    );
    assert_eq!(factory.created.load(Ordering::SeqCst), 2);
    let results = manager.retained_results().await?;
    let snapshot = attachment.load(store_limits).await?;
    assert_eq!(snapshot.agents.len(), 3);
    let first = snapshot.agents[1].id;
    let second = snapshot.agents[2].id;
    assert_ne!(first, second);
    assert_eq!(snapshot.agents[1].name, "researcher");
    assert_eq!(snapshot.agents[2].name, "researcher");
    assert_eq!(snapshot.context.len(), 1);
    assert_eq!(snapshot.context[0].owner, first);
    assert_eq!(snapshot.context[0].content.value, json!(17));
    let child_cells = |id| {
        snapshot
            .histories
            .get(&id)
            .into_iter()
            .flat_map(|history| &history.records)
            .filter_map(|stored| match &stored.record.kind {
                RecordKind::CellCompleted { outcome, .. } => Some(outcome.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        child_cells(first),
        vec![CellResponse::Completed {
            printed: vec![json!(i64::from(first).to_string())]
        }]
    );
    assert!(matches!(
        child_cells(second).as_slice(),
        [CellResponse::Failed {
            error: FeedbackError::Context { code, owner: Some(owner), .. },
            printed,
        }] if code == "not_owner" && *owner == first
            && printed == &vec![json!({"owner":i64::from(first).to_string(),"denied":"not_owner"})]
    ));
    assert!(
        matches!(
            child_cells(attachment.root_agent()).as_slice(),
            [CellResponse::Completed {..},CellResponse::Failed {printed,..}]
                if printed == &vec![json!("invalid_arguments"),json!("invalid_arguments")]
        ),
        "{:?}",
        child_cells(attachment.root_agent())
    );
    let history = store.load().await?;
    let starts:Vec<_> = history.records.iter().filter(|stored| matches!(&stored.record.kind,RecordKind::OperationStarted {name,..} if name == "cocoa.spawn")).collect();
    assert_eq!(starts.len(), 2);
    let repeated = store
        .admit_agent(
            starts[0].id,
            "researcher",
            &AgentAssignment {
                prompt: "same work".into(),
                alias: "medium".into(),
            },
            4096,
        )
        .await?;
    assert!(!repeated.created);
    assert_eq!(repeated.agent, first);
    assert_eq!(attachment.coordination_snapshot().await?.agents.len(), 3);
    engine.shutdown().await?;
    environment.close().await?;
    drop(environment);
    manager.shutdown().await?;
    drop(manager);
    drop(store);
    drop(attachment);
    drop(database);

    let database = SessionDatabase::open(&path).await?;
    let attachment = database
        .attach_session(&public_id, store_limits)
        .await?
        .recover(store_limits)
        .await?;
    let factory = Arc::new(Factory::new([]));
    let manager = AgentManager::attach(
        AgentLimits {
            active: 1,
            retained: 2,
        },
        limits(),
        factory.clone(),
        attachment.clone(),
    )
    .await?;
    assert_eq!(manager.retained_results().await?, results);
    assert!(!manager.has_active_agents().await?);
    assert_eq!(factory.created.load(Ordering::SeqCst), 0);
    let root = Arc::new(Script::new(
        vec![
            Step::InterruptedCell(
                "print(typeof first); print((await context.get('finding')).owner); try { await context.set('finding',{type:'observation',value:99}); } catch(error) { print({code:error.code,owner:error.owner}); }",
            ),
            Step::Answer("resumed"),
        ],
        false,
    ));
    let (mut engine, environment) = persisted_engine(
        root,
        &manager,
        attachment.root_store_with_instruction_state(store_limits)?,
        directory.path(),
        true,
    )
    .await?;
    assert_eq!(
        engine
            .run(
                RunRequest {
                    input: "continue".into()
                },
                CancellationToken::new()
            )
            .await?
            .answer,
        "resumed"
    );
    let restored = attachment.load(store_limits).await?;
    assert_eq!(restored.context, snapshot.context);
    let root_cell = restored
        .histories
        .get(&attachment.root_agent())
        .and_then(|history| {
            history
                .records
                .iter()
                .rev()
                .find_map(|stored| match &stored.record.kind {
                    RecordKind::CellCompleted { outcome, .. } => Some(outcome),
                    _ => None,
                })
        });
    assert!(matches!(
        root_cell,
        Some(CellResponse::Failed {
            error: FeedbackError::Context {code,owner:Some(owner),..},
            printed,
        }) if code == "not_owner" && *owner == first
            && printed == &vec![json!("undefined"),json!(i64::from(first).to_string()),json!({"code":"not_owner","owner":i64::from(first).to_string()})]
    ));
    assert_eq!(
        restored.histories.get(&first),
        snapshot.histories.get(&first)
    );
    assert_eq!(
        restored.histories.get(&second),
        snapshot.histories.get(&second)
    );
    assert_eq!(factory.created.load(Ordering::SeqCst), 0);
    engine.shutdown().await?;
    environment.close().await?;
    manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn lagged_application_observations_reconcile_every_admission_and_terminal_state()
-> Result<(), Box<dyn Error>> {
    let mut factory = Factory::new(
        [
            "child_0", "child_1", "child_2", "child_3", "child_4", "child_5",
        ]
        .map(|name| {
            (
                name,
                Arc::new(Script::new(
                    vec![Step::StreamedAnswer {
                        text: "x",
                        chunks: 96,
                    }],
                    false,
                )),
            )
        }),
    );
    factory.forward_observations = true;
    let manager = manager(Arc::new(factory), 1, 6)?;
    let mut receiver = manager.subscribe();
    let host = manager.bind("medium".into(), Arc::new(EmptyHost))?;
    for index in 0..6 {
        let name = format!("child_{index}");
        let reference = spawn(&host, &name, "answer", "medium").await?;
        assert_eq!(
            wait(&host, json!({"child":reference}), None, 4096).await?["child"]["status"],
            "completed"
        );
        assert_eq!(
            wait(&host, json!({"child":reference}), Some(0), 4096).await?["child"]["status"],
            "completed"
        );
    }
    manager.shutdown().await?;
    let updates = receiver.drain();
    for index in 0..6 {
        let name = format!("child_{index}");
        assert_eq!(
            updates
                .iter()
                .filter(
                    |update| update.name == name && matches!(update.kind, AgentUpdateKind::Started)
                )
                .count(),
            1
        );
        assert_eq!(
            updates
                .iter()
                .filter(|update| update.name == name
                    && matches!(
                        update.kind,
                        AgentUpdateKind::Finished(ActivityStatus::Completed)
                    ))
                .count(),
            1
        );
        assert!(updates.iter().any(|update| update.name == name
            && matches!(update.kind,
            AgentUpdateKind::Observation(Observation::Lagged { missed }) if missed > 64)));
    }
    assert!(receiver.drain().is_empty());
    let recovered = manager.subscribe().drain();
    assert_eq!(recovered.len(), 12);
    assert!(
        recovered
            .iter()
            .all(|update| !matches!(update.kind, AgentUpdateKind::Observation(_)))
    );
    Ok(())
}

#[tokio::test]
async fn agent_lifecycle_identity_changes_with_the_session() -> Result<(), Box<dyn Error>> {
    let mut sessions = Vec::new();
    for _ in 0..2 {
        let manager = manager(Arc::new(Factory::new([])), 1, 1)?;
        let mut updates = manager.subscribe();
        let host = manager.bind("medium".into(), Arc::new(EmptyHost))?;
        let reference = spawn(&host, "missing", "work", "medium").await?;
        assert_eq!(
            wait(&host, json!({"child":reference}), None, 4096).await?["child"]["status"],
            "failed"
        );
        manager.shutdown().await?;
        let observed = updates.drain();
        assert_eq!(observed.len(), 2);
        assert!(matches!(observed[0].kind, AgentUpdateKind::Started));
        assert!(matches!(
            observed[1].kind,
            AgentUpdateKind::Finished(ActivityStatus::Failed { exit_code: None })
        ));
        assert_eq!(observed[0].session, observed[1].session);
        assert_eq!(observed[0].name, "missing");
        sessions.push(observed[0].session);
    }
    assert_ne!(sessions[0], sessions[1]);
    Ok(())
}

#[tokio::test]
async fn shared_context_is_readable_by_children_and_siblings_but_only_writable_by_its_owner()
-> Result<(), Box<dyn Error>> {
    let researcher = Arc::new(Script::new(vec![
        Step::Batch(vec![Cell { code: "var observed = await context.get('build.mode'); observed.value.jobs = 99; var denied; try { await context.set('build.mode',{type:'decision',value:{jobs:99}}); } catch(error) { denied = {code:error.code,owner:error.owner,effects:error.effects,method:error.method}; } await context.set('research.finding',{type:'observation',value:denied,labels:['build','review']});".into() }]),
        Step::Cell("print([(await context.get('build.mode')).value.jobs,typeof parentSecret,typeof context.transfer,typeof context.delete]);", vec![json!([3,"undefined","undefined","undefined"])]),
        Step::Answer("research complete"),
    ], false));
    let checker = Arc::new(Script::new(
        vec![
            Step::Cell(
                "var record = await context.get('research.finding'); print({mode:(await context.get('build.mode')).value.mode,owner:record.owner,denied:record.value}); var page = await context.list({labels:['review','build']}); print(page.items.map(item => item.key));",
                vec![
                    json!({"mode":"release","owner":"2","denied":{"code":"not_owner","owner":"1","effects":"none","method":"context.set"}}),
                    json!(["research.finding"]),
                ],
            ),
            Step::Answer("checked"),
        ],
        false,
    ));
    let factory = Arc::new(Factory::new([
        ("researcher", researcher),
        ("checker", checker),
    ]));
    let manager = manager(factory, 2, 2)?;
    let host = manager.bind("medium".into(), Arc::new(EmptyHost))?;
    let root = Arc::new(Script::new(vec![
        Step::Cell("var parentSecret = 17; var savedGet = context.get; await context.set('build.mode',{type:'decision',value:{mode:'debug',jobs:3},labels:['build']}); var researcher = await cocoa.spawn({name:'researcher',prompt:'inspect shared context'}); print((await cocoa.wait(researcher)).status); print((await savedGet('research.finding')).owner);", vec![json!("completed"),json!("2")]),
        Step::Cell("await context.set('build.mode',{type:'decision',value:{mode:'release',jobs:7}}); var checker = await cocoa.spawn({name:'checker',prompt:'read the other child findings'}); print((await cocoa.wait(checker)).status); print((await savedGet('build.mode')).labels);", vec![json!("completed"),json!([])]),
        Step::Batch(vec![Cell { code: "try { await context.set('research.finding',{type:'observation',value:'root override'}); } catch(error) { await context.set('root.denial',{type:'observation',value:{code:error.code,owner:error.owner,effects:error.effects}}); }".into() }]),
        Step::Cell("print((await context.get('root.denial')).value); print((await savedGet('research.finding')).value.owner);", vec![json!({"code":"not_owner","owner":"2","effects":"none"}),json!("1")]),
        Step::Answer("shared context verified"),
    ], false));
    let mut engine = engine(root, host, limits()).await?;
    let result = engine
        .run(
            RunRequest {
                input: "share context without transferring ownership".into(),
            },
            CancellationToken::new(),
        )
        .await?;
    assert_eq!(result.answer, "shared context verified");
    engine.shutdown().await?;
    manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn report_capture_validates_unprinted_work_and_retains_output_after_cleanup()
-> Result<(), Box<dyn Error>> {
    let directory = tempfile::Builder::new().prefix("capture-").tempdir()?;
    let path = directory.path().canonicalize()?;
    let run_id = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("run name")?;
    let fixture = path.join("fixture");
    std::fs::create_dir(&fixture)?;
    std::fs::write(
        fixture.join("Cargo.toml"),
        "[package]\nname='capture-fixture'\nversion='0.0.0'\nedition='2024'\n[workspace]\n",
    )?;
    std::fs::create_dir(fixture.join("src"))?;
    std::fs::write(
        fixture.join("src/lib.rs"),
        "#[test] fn useful_control() { assert_eq!(7 * 3, 21); }\n#[test] #[ignore = \"control\"] fn not_executed() {}\n",
    )?;
    std::fs::write(
        fixture.join("rust-toolchain.toml"),
        include_str!("../../../rust-toolchain.toml"),
    )?;
    std::fs::write(path.join("source.txt"), "original source\n")?;
    let junit = fixture.join("results.xml");
    std::fs::write(
        fixture.join("nextest.toml"),
        format!(
            "[profile.evidence]\nretries=0\n[profile.evidence.junit]\npath={}\nreport-name={}\nreport-skipped='all'\n",
            json!(junit),
            json!(format!("{run_id}/checks"))
        ),
    )?;
    let command = "printf 'first-line\\nsecond-line\\ncomplete-tail\\n'; cargo nextest run --manifest-path Cargo.toml --config-file nextest.toml --profile evidence --offline --no-tests=fail";
    let arguments = cocoa_tools::RunArguments {
        command: command.into(),
        working_directory: Some(fixture.to_str().ok_or("fixture path")?.into()),
        timeout_ms: Some(30000),
        environment: None,
        stdin: None,
    };
    let run_code = format!(
        "var processHandle = await cocoa.run({}); var processResult = await cocoa.wait(processHandle); if (processResult.status.code !== 0) throw Error(JSON.stringify(processResult));",
        json!({"command":command,"working_directory":fixture,"timeout_ms":30000})
    );
    let version = std::process::Command::new("cargo")
        .args(["nextest", "--version"])
        .stdin(std::process::Stdio::null())
        .output_bounded()?;
    assert!(version.status.success());
    let version = String::from_utf8(version.stdout)?;
    let version = version
        .lines()
        .find_map(|line| line.strip_prefix("release: "))
        .ok_or("nextest version")?;
    let contract = json!({"schema_version":1,"run_id":run_id,"participants":{
        "root":{"cells_max":1,"answer_chars_max_exclusive":100,"expected_status":"completed"},
        "reviewer":{"cells_max":2,"answer_chars_max_exclusive":100,"expected_status":"completed"},
        "tester":{"cells_max":1,"answer_chars_max_exclusive":100,"expected_status":"completed"}},
        "aliases":{"audit":"reviewer","tests":"tester"},
        "sources":{"source.txt":"5424073a96f9f8a575231c720bdb3efa7f6d44f8e4ec63f480d1102c41750db8"},
        "test_runs":{"checks":{"participant":"tester","command":command,"report_name":format!("{run_id}/checks"),"expected_exit_code":0}},
        "notes":{"tests.txt":"tester"},"require_citations":false,"require_commentary":false});
    let draft = json!({"schema_version":1,"test_runs":["checks"],"citations":[],"commentary":"","assertions":[
        {"test_run":"checks","binary":"capture-fixture","metric":"passed","value":1},
        {"test_run":"checks","binary":"capture-fixture","metric":"skipped","value":1},
        {"test_run":"checks","binary":"capture-fixture","metric":"executed","value":1}
    ]});
    let mut tester = Script::new(
        vec![
            Step::OwnedCell(run_code.clone(), vec![]),
            Step::Answer("tests done\r\n✓"),
        ],
        false,
    );
    tester.continuation = Some(ProviderData {
        provider: "fixture".into(),
        data: json!({"private":"provider-continuation-sentinel"}),
    });
    let tester = Arc::new(tester);
    let reviewer = Arc::new(Script::new(
        vec![
            Step::Cell("await cocoa.read({path:'source.txt'});", vec![]),
            Step::Cell("await cocoa.read({path:'source.txt'});", vec![]),
            Step::Answer("review done"),
        ],
        false,
    ));
    let capture = Arc::new(report_capture::Capture::new(
        directory,
        BTreeMap::from([
            ("tester".into(), tester as Arc<dyn ModelProvider>),
            ("reviewer".into(), reviewer as Arc<dyn ModelProvider>),
        ]),
        BTreeMap::from([(
            "checks".into(),
            report_capture::CommandSpec {
                participant: "tester".into(),
                arguments,
                junit,
                runner: json!({"name":"cargo-nextest","version":version,"report_skipped":"all","retries":0,"partitioned":false}),
            },
        )]),
        &["source.txt"],
    )?);
    let mut capture_limits = limits();
    capture_limits.stall_timeout = Duration::from_secs(60);
    let manager = AgentManager::new(
        AgentLimits {
            active: 2,
            retained: 2,
        },
        capture_limits,
        capture.clone(),
    )?;
    let host = manager.bind("medium".into(), capture.coding("root").await?)?;
    let root = Arc::new(Script::new(
        vec![
            Step::Cell(
                "var reviewer = await cocoa.spawn({name:'reviewer',prompt:'review'}); var tester = await cocoa.spawn({name:'tester',prompt:'test'}); var results = await cocoa.wait({audit:reviewer,tests:tester}); var again = await cocoa.wait({audit:reviewer,tests:tester}); var output = await cocoa.read({path:'outcomes.json'}); await cocoa.write({file:output.file,content:JSON.stringify(results)}); var note = await cocoa.read({path:'tests.txt'}); await cocoa.write({file:note.file,content:results.tests.answer});",
                vec![],
            ),
            Step::Answer("root exact answer"),
        ],
        false,
    ));
    let mut engine = capture.engine("root", root, host, capture_limits).await?;
    assert_eq!(
        capture
            .run_root(
                &mut engine,
                RunRequest {
                    input: "capture two children".into()
                },
                CancellationToken::new()
            )
            .await?
            .answer,
        "root exact answer"
    );
    engine.shutdown().await?;
    manager.shutdown().await?;
    capture.close("root").await?;
    for entry in std::fs::read_dir(&path)? {
        assert!(
            !entry?
                .file_name()
                .to_string_lossy()
                .starts_with("cocoa-output-")
        );
    }
    std::fs::write(path.join("source.txt"), "changed after work\n")?;
    let evidence = capture
        .packet(run_id, "outcomes.json", &["tests.txt"])
        .await?;
    assert_eq!(
        evidence["participants"]["tester"]["calls"][0]["code"],
        run_code
    );
    assert_eq!(
        evidence["sources"]["source.txt"]["text"],
        "original source\n"
    );
    assert_eq!(
        evidence["test_runs"]["checks"]["stdout"],
        "first-line\nsecond-line\ncomplete-tail\n"
    );
    assert_eq!(evidence["notes"]["tests.txt"]["text"], "tests done\r\n✓");
    assert_eq!(
        evidence["participants"]["tester"]["outcome"],
        evidence["wait_results"]["tests"]
    );
    assert_eq!(evidence["wait_results"], evidence["repeat_results"]);
    assert_eq!(evidence["wait_results"], evidence["saved_outcomes"]);
    assert!(!serde_json::to_string(&evidence)?.contains("provider-continuation-sentinel"));
    let calls = &evidence["participants"]["reviewer"]["calls"];
    assert_eq!(calls.as_array().ok_or("calls")?.len(), 2);
    assert_ne!(calls[0]["call_id"], calls[1]["call_id"]);
    assert_eq!(calls[0]["code"], calls[1]["code"]);
    let result = validate_capture(&path, &contract, &evidence, &draft)?;
    assert_eq!(result["exit_code"], 0, "{result:#}");
    assert_eq!(
        capture
            .packet(run_id, "outcomes.json", &["tests.txt"])
            .await?,
        evidence
    );
    let saved = std::fs::read_to_string(path.join("outcomes.json"))?;
    let mut altered: Value = serde_json::from_str(&saved)?;
    altered["tests"]["answer"] = json!("fabricated");
    std::fs::write(path.join("outcomes.json"), serde_json::to_vec(&altered)?)?;
    let changed = capture
        .packet(run_id, "outcomes.json", &["tests.txt"])
        .await?;
    assert_eq!(changed["participants"], evidence["participants"]);
    assert_eq!(
        validate_capture(&path, &contract, &changed, &draft)?["exit_code"],
        1
    );
    let duplicate = format!(
        "{{\"tests\":{{\"name\":\"tester\",\"status\":\"completed\",\"answer\":\"fabricated\"}},{}}}",
        &saved[1..saved.len() - 1]
    );
    std::fs::write(path.join("outcomes.json"), duplicate)?;
    let ambiguous = capture
        .packet(run_id, "outcomes.json", &["tests.txt"])
        .await?;
    assert!(ambiguous.get("saved_outcomes").is_none());
    assert_eq!(
        validate_capture(&path, &contract, &ambiguous, &draft)?["exit_code"],
        2
    );
    std::fs::write(
        path.join("outcomes.json"),
        "{\"tests\":{\"name\":\"tester\",\"status\":\"cancelled\",\"answer\":null}}",
    )?;
    assert!(
        capture
            .packet(run_id, "outcomes.json", &[])
            .await?
            .get("saved_outcomes")
            .is_none()
    );
    std::fs::write(path.join("outcomes.json"), &saved)?;
    std::os::unix::fs::symlink("tests.txt", path.join("symlink-note"))?;
    let missing = capture
        .packet(run_id, "missing.json", &["missing.txt", "symlink-note"])
        .await?;
    assert_eq!(missing["notes"], json!({}));
    assert!(missing.get("saved_outcomes").is_none());
    assert!(capture.packet(run_id, "../escape", &[]).await.is_err());
    Ok(())
}

#[tokio::test]
async fn report_capture_does_not_promote_missing_ambiguous_or_failed_commands()
-> Result<(), Box<dyn Error>> {
    for scenario in [
        "printed",
        "repeated",
        "wrong_options",
        "missing_artifact",
        "stale_artifact",
        "nonzero",
        "signalled",
    ] {
        let directory = tempfile::tempdir()?;
        let path = directory.path().canonicalize()?;
        let command = match scenario {
            "missing_artifact" | "stale_artifact" => "printf 'real-output\\n'",
            "nonzero" => "printf '<not-a-runner/>' > results.xml; printf 'real-output\\n'; exit 7",
            "signalled" => "printf '<not-a-runner/>' > results.xml; kill -TERM $$",
            _ => "printf '<not-a-runner/>' > results.xml; printf 'real-output\\n'",
        };
        let run = format!(
            "await cocoa.wait(await cocoa.run({}));",
            json!({"command":command})
        );
        let code = match scenario {
            "printed" => "print({status:{kind:'exited',code:0},junit:'forged-success'});".into(),
            "wrong_options" => format!(
                "await cocoa.wait(await cocoa.run({}));",
                json!({"command":command,"timeout_ms":2000})
            ),
            _ => run,
        };
        let capture = report_capture::Capture::new(
            directory,
            BTreeMap::new(),
            BTreeMap::from([(
                "checks".into(),
                report_capture::CommandSpec {
                    participant: "root".into(),
                    arguments: cocoa_tools::RunArguments {
                        command: command.into(),
                        working_directory: None,
                        timeout_ms: None,
                        environment: None,
                        stdin: None,
                    },
                    junit: path.join("results.xml"),
                    runner: json!({"name":"cargo-nextest","version":"0.9.143","report_skipped":"all","retries":0,"partitioned":false}),
                },
            )]),
            &[],
        )?;
        if scenario == "stale_artifact" {
            std::fs::write(path.join("results.xml"), "stale")?;
        }
        let printed = if scenario == "printed" {
            vec![json!({"status":{"kind":"exited","code":0},"junit":"forged-success"})]
        } else {
            vec![]
        };
        let mut steps = vec![Step::OwnedCell(code.clone(), printed)];
        if scenario == "repeated" {
            steps.push(Step::OwnedCell(code, vec![]));
        }
        steps.push(Step::Answer("unchanged"));
        let root = Arc::new(Script::new(steps, false));
        let mut engine = capture
            .engine("root", root, capture.coding("root").await?, limits())
            .await?;
        assert_eq!(
            capture
                .run_root(
                    &mut engine,
                    RunRequest {
                        input: "probe".into()
                    },
                    CancellationToken::new()
                )
                .await?
                .answer,
            "unchanged",
            "{scenario}"
        );
        engine.shutdown().await?;
        capture.close("root").await?;
        let evidence = capture.packet("probe", "absent.json", &[]).await?;
        let run = &evidence["test_runs"]["checks"];
        assert_eq!(
            evidence["participants"]["root"]["outcome"]["answer"],
            "unchanged"
        );
        assert_eq!(
            run["complete"],
            matches!(scenario, "nonzero" | "signalled"),
            "{scenario}: {run}"
        );
        match scenario {
            "nonzero" => assert_eq!(run["status"], json!({"kind":"exited","code":7})),
            "signalled" => assert_eq!(run["status"], json!({"kind":"signalled","signal":15})),
            "missing_artifact" | "stale_artifact" => {
                assert_eq!(run["status"], json!({"kind":"exited","code":0}));
                assert!(run.get("junit").is_none());
            }
            _ => assert!(run.get("status").is_none()),
        }
    }
    Ok(())
}

#[tokio::test]
async fn report_capture_preserves_cancellation_and_leaves_unrecorded_failures_unknown()
-> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().canonicalize()?;
    let command = "printf started > started; while :; do :; done";
    let child = Arc::new(Script::new(
        vec![Step::Batch(vec![
            Cell {
                code: format!(
                    "await cocoa.wait(await cocoa.run({}));",
                    json!({"command":command})
                ),
            },
            Cell {
                code: "throw Error('must not start');".into(),
            },
        ])],
        false,
    ));
    let broken = Arc::new(Script::new(vec![Step::Fail], false));
    let capture = Arc::new(report_capture::Capture::new(
        directory,
        BTreeMap::from([
            ("worker".into(), child as Arc<dyn ModelProvider>),
            ("broken".into(), broken as Arc<dyn ModelProvider>),
        ]),
        BTreeMap::from([(
            "checks".into(),
            report_capture::CommandSpec {
                participant: "worker".into(),
                arguments: cocoa_tools::RunArguments {
                    command: command.into(),
                    working_directory: None,
                    timeout_ms: None,
                    environment: None,
                    stdin: None,
                },
                junit: path.join("results.xml"),
                runner: json!({"name":"cargo-nextest","version":"0.9.143","report_skipped":"all","retries":0,"partitioned":false}),
            },
        )]),
        &[],
    )?);
    let manager = AgentManager::new(
        AgentLimits {
            active: 2,
            retained: 2,
        },
        limits(),
        capture.clone(),
    )?;
    let host = manager.bind("medium".into(), Arc::new(EmptyHost))?;
    let worker = spawn(&host, "worker", "work", "medium").await?;
    let broken = spawn(&host, "broken", "fail", "medium").await?;
    tokio::time::timeout(Duration::from_secs(2), async {
        let mut interval = tokio::time::interval(Duration::from_millis(5));
        loop {
            interval.tick().await;
            if path.join("started").is_file() {
                break;
            }
        }
    })
    .await?;
    call(&host, "cocoa.cancel", json!({"reference":worker})).await?;
    let results = wait(&host, json!({"work":worker,"failure":broken}), None, 4096).await?;
    assert_eq!(
        results["work"],
        json!({"name":"worker","status":"cancelled"})
    );
    assert_eq!(results["failure"]["status"], "failed");
    manager.shutdown().await?;
    let evidence = capture.packet("cancellation", "missing.json", &[]).await?;
    assert_eq!(
        evidence["participants"]["worker"]["outcome"],
        results["work"]
    );
    assert_eq!(
        evidence["participants"]["worker"]["calls"][0]["state"],
        "interrupted"
    );
    assert_eq!(
        evidence["participants"]["worker"]["calls"][1]["state"],
        "not_started"
    );
    assert_eq!(evidence["participants"]["worker"]["complete"], true);
    assert_eq!(evidence["participants"]["broken"]["complete"], false);
    assert!(evidence["participants"]["broken"].get("outcome").is_none());
    assert_eq!(evidence["test_runs"]["checks"]["complete"], false);
    assert!(evidence["test_runs"]["checks"].get("status").is_none());
    assert!(!serde_json::to_string(&evidence)?.contains("private provider diagnostic"));
    Ok(())
}

fn validate_capture(
    path: &std::path::Path,
    contract: &Value,
    evidence: &Value,
    draft: &Value,
) -> Result<Value, Box<dyn Error>> {
    for (name, value) in [
        ("contract", contract),
        ("evidence", evidence),
        ("draft", draft),
    ] {
        std::fs::write(
            path.join(format!("{name}.json")),
            serde_json::to_vec(value)?,
        )?;
    }
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_validate-agent-reports"))
        .arg("--contract")
        .arg(path.join("contract.json"))
        .arg("--evidence")
        .arg(path.join("evidence.json"))
        .arg("--draft")
        .arg(path.join("draft.json"))
        .stdin(std::process::Stdio::null())
        .output_bounded()?;
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(serde_json::from_slice(&output.stdout)?)
}

#[tokio::test]
async fn private_handles_survive_cells_and_root_completion() -> Result<(), Box<dyn Error>> {
    let child = Arc::new(Script::new(
        vec![
            Step::Cell(
                "print({secret: typeof parentSecret, spawn: typeof globalThis.cocoa?.spawn})",
                vec![json!({"secret":"undefined","spawn":"undefined"})],
            ),
            Step::Answer("child exact answer"),
        ],
        true,
    ));
    let factory = Arc::new(Factory::new([("reviewer", child.clone())]));
    let manager = manager(factory.clone(), 1, 2)?;
    let mut updates = manager.subscribe();
    let host = manager.bind("medium".into(), Arc::new(EmptyHost))?;
    let root = Arc::new(Script::new(
        vec![
            Step::Cell(
                "var parentSecret = 91; var reviewer = await cocoa.spawn({name:'reviewer',prompt:'inspect independently'}); var savedWait = cocoa.wait; print({name:reviewer.name,alias:reviewer.alias,frozen:Object.isFrozen(reviewer),same:await reviewer === reviewer});",
                vec![json!({"name":"reviewer","alias":"medium","frozen":true,"same":true})],
            ),
            Step::Cell(
                "var aliases = Object.create(null); aliases.review = reviewer; aliases.__proto__ = reviewer; var initial = await savedWait(aliases,{timeout_ms:0}); print({pending:initial.review.status,aliases:JSON.stringify(initial.review)===JSON.stringify(initial.__proto__)});",
                vec![json!({"pending":"pending","aliases":true})],
            ),
            Step::Answer("parent finished first"),
            Step::Cell(
                "print(await savedWait(reviewer)); print(await cocoa.wait(reviewer));",
                vec![
                    json!({"name":"reviewer","status":"completed","answer":"child exact answer"}),
                    json!({"name":"reviewer","status":"completed","answer":"child exact answer"}),
                ],
            ),
            Step::Answer("collected twice"),
        ],
        false,
    ));
    let mut engine = engine(root, host, limits()).await?;
    assert_eq!(
        engine
            .run(
                RunRequest {
                    input: "first".into()
                },
                CancellationToken::new()
            )
            .await?
            .answer,
        "parent finished first"
    );
    let started = updates.drain();
    assert_eq!(started.len(), 1);
    assert!(matches!(started[0].kind, AgentUpdateKind::Started));
    child.entered.acquire().await?.forget();
    assert_eq!(factory.created.load(Ordering::SeqCst), 1);
    child.release.add_permits(1);
    assert_eq!(
        engine
            .run(
                RunRequest {
                    input: "collect".into()
                },
                CancellationToken::new()
            )
            .await?
            .answer,
        "collected twice"
    );
    assert_eq!(factory.created.load(Ordering::SeqCst), 1);
    engine.shutdown().await?;
    manager.shutdown().await?;
    let settled = updates.drain();
    assert_eq!(settled.len(), 1);
    assert!(matches!(
        settled[0].kind,
        AgentUpdateKind::Finished(ActivityStatus::Completed)
    ));
    assert_eq!(*factory.closed.lock().await, vec!["reviewer"]);
    Ok(())
}

#[tokio::test]
async fn finished_children_release_execution_before_retaining_repeatable_results()
-> Result<(), Box<dyn Error>> {
    let factory = Arc::new(Factory::new([
        (
            "cleanup_wait",
            Arc::new(Script::new(vec![Step::Answer("saved answer")], false)),
        ),
        (
            "next",
            Arc::new(Script::new(vec![Step::Answer("next answer")], false)),
        ),
    ]));
    let manager = manager(factory.clone(), 1, 2)?;
    let mut updates = manager.subscribe();
    let host = manager.bind("medium".into(), Arc::new(EmptyHost))?;
    let reference = spawn(&host, "cleanup_wait", "work", "medium").await?;
    let started = updates.drain();
    assert_eq!(started.len(), 1);
    assert!(matches!(started[0].kind, AgentUpdateKind::Started));
    tokio::time::timeout(Duration::from_secs(1), factory.cleanup_entered.acquire())
        .await??
        .forget();
    assert_eq!(
        wait(&host, json!({"w":reference}), Some(0), 4096).await?["w"]["status"],
        "pending"
    );
    assert!(matches!(
        spawn(&host, "next", "work", "medium").await,
        Err(CapabilityError::Tool { code, .. }) if code == "capacity"
    ));
    assert!(
        updates.drain().is_empty(),
        "answer completion is not engine settlement"
    );
    factory.cleanup_release.add_permits(1);
    for _ in 0..2 {
        assert_eq!(
            wait(&host, json!({"w":reference}), None, 4096).await?["w"],
            json!({"name":"cleanup_wait","status":"completed","answer":"saved answer"})
        );
    }
    let next = spawn(&host, "next", "work", "medium").await?;
    assert_eq!(
        wait(&host, json!({"w":next}), None, 4096).await?["w"],
        json!({"name":"next","status":"completed","answer":"next answer"})
    );
    assert!(matches!(
        spawn(&host, "extra", "work", "medium").await,
        Err(CapabilityError::Tool { code, .. }) if code == "capacity"
    ));
    assert!(factory.closed.lock().await.is_empty());
    manager.shutdown().await?;
    let settled = updates.drain();
    assert_eq!(
        settled
            .iter()
            .filter(|update| update.name == "cleanup_wait")
            .count(),
        1
    );
    assert!(settled.iter().any(|update| update.name == "cleanup_wait"
        && matches!(
            update.kind,
            AgentUpdateKind::Finished(ActivityStatus::Completed)
        )));
    assert_eq!(factory.closed.lock().await.len(), 2);
    Ok(())
}

#[tokio::test]
async fn interrupting_root_wait_keeps_child_without_reconstructing_runtime_handles()
-> Result<(), Box<dyn Error>> {
    let child = Arc::new(Script::new(vec![Step::Answer("retained answer")], true));
    let factory = Arc::new(Factory::new([("worker", child.clone())]));
    let manager = manager(factory.clone(), 1, 1)?;
    let host = manager.bind("medium".into(), Arc::new(EmptyHost))?;
    let root = Arc::new(Script::new(
        vec![
            Step::Cell(
                "var worker = await cocoa.spawn({name:'worker',prompt:'work'}); print(worker.name);",
                vec![json!("worker")],
            ),
            Step::Answer("started"),
            Step::InterruptedCell("await cocoa.wait(worker)"),
            Step::Cell("print(typeof worker);", vec![json!("undefined")]),
            Step::Answer("recovered"),
        ],
        false,
    ));
    let mut engine = engine(root, host.clone(), limits()).await?;
    assert_eq!(
        engine
            .run(
                RunRequest {
                    input: "start".into()
                },
                CancellationToken::new()
            )
            .await?
            .answer,
        "started"
    );
    child.entered.acquire().await?.forget();
    let token = CancellationToken::new();
    {
        let invocation = engine.run(
            RunRequest {
                input: "wait".into(),
            },
            token.clone(),
        );
        tokio::pin!(invocation);
        tokio::select! {
            result = &mut invocation => return Err(format!("root finished before interruption: {result:?}").into()),
            () = tokio::task::yield_now() => {},
        }
        token.cancel();
        assert!(matches!(
            invocation.await,
            Err(RunError::TurnCancelled { .. })
        ));
    }
    child.release.add_permits(1);
    assert_eq!(
        engine
            .run(
                RunRequest {
                    input: "collect".into()
                },
                CancellationToken::new()
            )
            .await?
            .answer,
        "recovered"
    );
    let retained = tokio::time::timeout(Duration::from_secs(1), async {
        let mut tick = tokio::time::interval(Duration::from_millis(10));
        loop {
            tick.tick().await;
            let retained = manager.retained_results().await?;
            if retained[0]["outcome"]["status"] != "pending" {
                return Ok::<_, CapabilityError>(retained);
            }
        }
    })
    .await??;
    assert_eq!(
        retained[0]["outcome"],
        json!({"name":"worker","status":"completed","answer":"retained answer"})
    );
    assert_eq!(factory.created.load(Ordering::SeqCst), 1);
    engine.shutdown().await?;
    manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn process_handles_survive_turns_and_share_named_waits_with_agents()
-> Result<(), Box<dyn Error>> {
    use cocoa_tools::{CodingCapabilities, CodingLimits, LocalConfiguration, LocalEnvironment};

    let directory = tempfile::tempdir()?;
    let local = Arc::new(
        LocalEnvironment::new(
            LocalConfiguration {
                working_directory: directory.path().into(),
                output_directory: directory.path().into(),
                shell: "/bin/bash".into(),
                environment: BTreeMap::from([("PATH".into(), "/usr/bin:/bin".into())]),
            },
            CodingLimits {
                file_bytes: 4096,
                observed_bytes: 16384,
                observations: 16,
                processes: 2,
                captured_output_bytes: 4096,
                retained_output_bytes: 16384,
                preview_lines: 200,
                default_timeout: Duration::from_secs(2),
            },
        )
        .await?,
    );
    let factory = Arc::new(Factory::new([(
        "reviewer",
        Arc::new(Script::new(vec![Step::Answer("review done")], false)),
    )]));
    let manager = manager(factory, 1, 1)?;
    let host = manager.bind(
        "medium".into(),
        Arc::new(CodingCapabilities::new(local.clone())),
    )?;
    let root = Arc::new(Script::new(
        vec![
            Step::Cell(
                "var build = await cocoa.run({command:'printf first; while [ ! -f release ]; do sleep 0.01; done; printf last; printf x >> count'}); var reviewer = await cocoa.spawn({name:'reviewer',prompt:'review'}); var forged = false; try { await cocoa.wait({kind:'process'}); } catch { forged = true; } print([build.kind,Object.isFrozen(build),Object.getPrototypeOf(build) === null,await build === build,forged]);",
                vec![json!(["process", true, true, true, true])],
            ),
            Step::Answer("started"),
            Step::Cell(
                "var results = await cocoa.wait({build,alias:build,reviewer}); print([results.build.status,results.build.stdout.text,results.reviewer.answer,JSON.stringify(results.build) === JSON.stringify(results.alias)]); var repeated = await cocoa.wait(build,{timeout_ms:0}); print(JSON.stringify(repeated) === JSON.stringify(results.build));",
                vec![
                    json!([{"kind":"exited","code":0},"firstlast","review done",true]),
                    json!(true),
                ],
            ),
            Step::Answer("collected"),
        ],
        false,
    ));
    let mut engine = engine(root, host, limits()).await?;
    assert_eq!(
        engine
            .run(
                RunRequest {
                    input: "start".into()
                },
                CancellationToken::new()
            )
            .await?
            .answer,
        "started"
    );
    assert!(!directory.path().join("count").exists());
    {
        let collecting = engine.run(
            RunRequest {
                input: "collect".into(),
            },
            CancellationToken::new(),
        );
        tokio::pin!(collecting);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut collecting)
                .await
                .is_err(),
            "ordinary mixed waits must still await a running process"
        );
        std::fs::write(directory.path().join("release"), "")?;
        assert_eq!(collecting.await?.answer, "collected");
    }
    assert_eq!(
        std::fs::read_to_string(directory.path().join("count"))?,
        "x"
    );
    engine.shutdown().await?;
    manager.shutdown().await?;
    local.close().await?;
    Ok(())
}

#[tokio::test]
async fn paused_children_retain_their_runtime_and_recover_independently()
-> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let local = Arc::new(
        LocalEnvironment::new(
            LocalConfiguration {
                working_directory: directory.path().into(),
                output_directory: directory.path().into(),
                shell: "/bin/bash".into(),
                environment: BTreeMap::from([("PATH".into(), "/usr/bin:/bin".into())]),
            },
            CodingLimits {
                file_bytes: 4096,
                observed_bytes: 16384,
                observations: 16,
                processes: 2,
                captured_output_bytes: 4096,
                retained_output_bytes: 16384,
                preview_lines: 200,
                default_timeout: Duration::from_secs(5),
            },
        )
        .await?,
    );
    let recovering = Arc::new(Script::new(
        vec![
            Step::Cell(
                "var retained = 41; await context.set('effect',{type:'observation',value:retained}); print(retained);",
                vec![json!(41)],
            ),
            Step::Fail,
            Step::Cell("print(retained + 1);", vec![json!(42)]),
            Step::Answer("recovered"),
        ],
        false,
    ));
    let cancelled = Arc::new(Script::new(
        vec![Step::Fail, Step::Answer("must not run")],
        false,
    ));
    let factory = Arc::new(Factory::new([
        ("recovering", recovering.clone()),
        ("cancelled", cancelled.clone()),
    ]));
    let manager = manager(factory.clone(), 2, 2)?;
    let mut updates = manager.subscribe();
    updates.enable_recovery().await?;
    let host = manager.bind(
        "medium".into(),
        Arc::new(cocoa_tools::CodingCapabilities::new(local.clone())),
    )?;
    let process = call(
        &host,
        "cocoa.run",
        json!({"command":"while [ ! -f release ]; do sleep 0.01; done; printf survived"}),
    )
    .await?;
    let first = spawn(&host, "recovering", "first", "medium").await?;
    let second = spawn(&host, "cancelled", "second", "medium").await?;
    let mut failures = BTreeMap::new();
    tokio::time::timeout(Duration::from_secs(3), async {
        while failures.len() < 2 {
            if let Some(failure) = updates.try_failure() {
                failures.insert(failure.name.clone(), failure);
            }
            tokio::task::yield_now().await;
        }
    })
    .await?;
    let paused = tokio::time::timeout(
        Duration::from_millis(200),
        wait(&host, json!({"first":first,"second":second}), None, 4096),
    )
    .await??;
    assert_eq!(
        paused["first"],
        json!({"name":"recovering","status":"pending","state":"provider_paused"})
    );
    assert_eq!(paused["second"]["state"], "provider_paused");
    assert!(manager.has_active_agents().await?);
    assert!(factory.closed.lock().await.is_empty());
    assert!(updates.drain().iter().any(|update| matches!(&update.kind, AgentUpdateKind::ProviderPaused { reason } if !reason.contains("private"))));
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        updates.drain().is_empty(),
        "unchanged pauses must not invalidate rendered interaction targets"
    );
    let mixed = tokio::time::timeout(
        Duration::from_millis(500),
        wait(
            &host,
            json!({"worker":first,"build":process["reference"]}),
            Some(4000),
            4096,
        ),
    )
    .await??;
    assert_eq!(mixed["worker"]["state"], "provider_paused");
    assert_eq!(mixed["build"]["status"]["kind"], "running");
    std::fs::write(directory.path().join("release"), "")?;
    let completed = wait(
        &host,
        json!({"build":process["reference"]}),
        Some(1000),
        4096,
    )
    .await?;
    assert_eq!(
        completed["build"]["status"],
        json!({"kind":"exited","code":0})
    );
    assert_eq!(completed["build"]["stdout"]["text"], "survived");
    assert_eq!(recovering.requests.load(Ordering::SeqCst), 2);
    assert_eq!(cancelled.requests.load(Ordering::SeqCst), 1);
    let failure = failures.remove("recovering").ok_or("missing retry")?;
    failure
        .failure
        .retry
        .send(())
        .map_err(|()| "child stopped before retry")?;
    let cancelled_failure = failures.remove("cancelled").ok_or("missing cancellation")?;
    cancelled_failure.cancellation.cancel();
    let result = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let result = wait(
                &host,
                json!({"first":first,"second":second}),
                Some(10),
                4096,
            )
            .await?;
            if result["first"]["status"] != "pending" && result["second"]["status"] != "pending" {
                break Ok::<_, CapabilityError>(result);
            }
            tokio::task::yield_now().await;
        }
    })
    .await??;
    assert_eq!(result["first"]["answer"], "recovered");
    assert_eq!(result["second"]["status"], "cancelled");
    assert_eq!(cancelled.requests.load(Ordering::SeqCst), 1);
    assert_eq!(factory.created.load(Ordering::SeqCst), 2);
    assert!(!manager.has_active_agents().await?);
    manager.shutdown().await?;
    local.close().await?;
    Ok(())
}

#[tokio::test]
async fn named_waits_share_snapshots_and_preserve_partial_and_failed_results()
-> Result<(), Box<dyn Error>> {
    let slow = Arc::new(Script::new(vec![Step::Answer("slow answer")], true));
    let fast = Arc::new(Script::new(vec![Step::Answer("fast answer")], false));
    let broken = Arc::new(Script::new(vec![Step::Fail], false));
    let factory = Arc::new(Factory::new([
        ("slow", slow.clone()),
        ("fast", fast),
        ("broken", broken),
    ]));
    let manager = manager(factory, 3, 3)?;
    let host = manager.bind("medium".into(), Arc::new(EmptyHost))?;
    let slow_handle = spawn(&host, "slow", "first", "high").await?;
    let fast_handle = spawn(&host, "fast", "second", "medium").await?;
    let broken_handle = spawn(&host, "broken", "third", "medium").await?;
    let ready = wait(
        &host,
        json!({"second":fast_handle,"error":broken_handle}),
        None,
        4096,
    )
    .await?;
    assert_eq!(
        ready["second"],
        json!({"name":"fast","status":"completed","answer":"fast answer"})
    );
    assert_eq!(
        ready["error"],
        json!({"name":"broken","status":"failed","error":{"code":"provider_failed","message":"agent provider failed"}})
    );
    let group = json!({"first":slow_handle,"second":fast_handle,"same":slow_handle,"__proto__":broken_handle});
    let partial = wait(&host, group.clone(), Some(10), 4096).await?;
    assert_eq!(partial["first"]["status"], "pending");
    assert_eq!(partial["first"], partial["same"]);
    assert_eq!(partial["second"], ready["second"]);
    assert_eq!(partial["__proto__"], ready["error"]);
    slow.release.add_permits(1);
    let complete = wait(&host, group, None, 4096).await?;
    assert_eq!(
        complete["first"],
        json!({"name":"slow","status":"completed","answer":"slow answer"})
    );
    assert_eq!(complete["same"], complete["first"]);
    assert_eq!(wait(&host, json!({}), None, 4096).await?, json!({}));
    manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn cancellation_targets_work_not_waiters_or_parent_invocations() -> Result<(), Box<dyn Error>>
{
    let child = Arc::new(Script::new(vec![Step::Answer("eventually")], true));
    let factory = Arc::new(Factory::new([("worker", child.clone())]));
    let manager = manager(factory.clone(), 1, 1)?;
    assert!(!manager.has_active_agents().await?);
    let mut updates = manager.subscribe();
    let host = manager.bind("medium".into(), Arc::new(EmptyHost))?;
    let reference = spawn(&host, "worker", "work", "medium").await?;
    assert!(manager.has_active_agents().await?);
    child.entered.acquire().await?.forget();
    assert!(manager.has_active_agents().await?);
    let token = CancellationToken::new();
    let waiting = host.call(
        "cocoa.wait",
        json!({"handles":{"w":reference}}),
        work_limits(4096),
        token.clone(),
    );
    tokio::pin!(waiting);
    tokio::select! {
        result = &mut waiting => return Err(format!("wait completed early: {result:?}").into()),
        () = tokio::task::yield_now() => {},
    }
    token.cancel();
    assert_eq!(waiting.await, Err(CapabilityError::Cancelled));
    assert_eq!(
        wait(&host, json!({"w":reference}), Some(0), 4096).await?["w"]["status"],
        "pending"
    );
    let first = call(&host, "cocoa.cancel", json!({"reference":reference})).await?;
    assert_eq!(first, json!({"name":"worker","state":"cancelling"}));
    let started = updates.drain();
    assert_eq!(started.len(), 1);
    assert!(matches!(started[0].kind, AgentUpdateKind::Started));
    let result = wait(&host, json!({"w":reference}), None, 4096).await?;
    assert_eq!(result["w"], json!({"name":"worker","status":"cancelled"}));
    assert!(!manager.has_active_agents().await?);
    assert_eq!(
        call(&host, "cocoa.cancel", json!({"reference":reference})).await?,
        json!({"name":"worker","state":"cancelled"})
    );
    assert!(
        matches!(spawn(&host, "worker", "work", "medium").await, Err(CapabilityError::Tool {code,..}) if code == "capacity")
    );
    manager.shutdown().await?;
    assert!(matches!(manager.has_active_agents().await,
        Err(CapabilityError::Tool {code,..}) if code == "session_closed"));
    let settled = updates.drain();
    assert_eq!(settled.len(), 1);
    assert!(matches!(
        settled[0].kind,
        AgentUpdateKind::Finished(ActivityStatus::Interrupted)
    ));
    assert_eq!(factory.created.load(Ordering::SeqCst), 1);
    assert!(
        matches!(call(&host,"cocoa.wait",json!({"handles":{"w":reference}})).await,Err(CapabilityError::Tool {code,..}) if code=="session_closed")
    );
    Ok(())
}

#[tokio::test]
async fn admission_is_atomic_and_handles_are_authorized() -> Result<(), Box<dyn Error>> {
    let blocked = Arc::new(Script::new(vec![Step::Answer("done")], true));
    let factory = Arc::new(Factory::new([
        ("worker", blocked.clone()),
        ("other", blocked.clone()),
    ]));
    let manager = manager(factory.clone(), 1, 2)?;
    let host = manager.bind("medium".into(), Arc::new(EmptyHost))?;
    let foreign = manager.bind("high".into(), Arc::new(EmptyHost))?;
    for arguments in [
        json!({"name":"","prompt":"work"}),
        json!({"name":"a b","prompt":"work"}),
        json!({"name":"x".repeat(65),"prompt":"work"}),
        json!({"name":"worker","prompt":" "}),
        json!({"name":"worker","prompt":"work","extra":1}),
    ] {
        assert!(
            matches!(call(&host,"cocoa.spawn",arguments).await,Err(CapabilityError::Tool {code,..}) if code=="invalid_arguments")
        );
    }
    assert!(
        matches!(spawn(&host,"worker","work","missing").await,Err(CapabilityError::Tool {code,..}) if code=="alias_unavailable")
    );
    let arguments = json!({"name":"worker","prompt":"work","alias":"high"});
    let receipt = call(&host, "cocoa.spawn", arguments).await?;
    let reference = receipt["reference"].clone();
    assert!(
        matches!(spawn(&host,"worker","work!","high").await,Err(CapabilityError::Tool {code,..}) if code=="capacity")
    );
    assert!(
        matches!(spawn(&foreign,"worker","work","high").await,Err(CapabilityError::Tool {code,..}) if code=="capacity")
    );
    assert!(
        matches!(wait(&foreign,json!({"stolen":reference}),Some(0),4096).await,Err(CapabilityError::Tool {code,..}) if code=="invalid_handle")
    );
    assert!(
        matches!(call(&foreign,"cocoa.cancel",json!({"reference":reference})).await,Err(CapabilityError::Tool {code,..}) if code=="invalid_handle")
    );
    assert!(
        matches!(spawn(&host,"other","other","medium").await,Err(CapabilityError::Tool {code,..}) if code=="capacity")
    );
    blocked.entered.acquire().await?.forget();
    assert_eq!(factory.created.load(Ordering::SeqCst), 1);
    let token = CancellationToken::new();
    token.cancel();
    assert_eq!(
        host.call(
            "cocoa.spawn",
            json!({"name":"other","prompt":"other"}),
            work_limits(4096),
            token
        )
        .await,
        Err(CapabilityError::Cancelled)
    );
    manager.shutdown().await?;
    assert_eq!(factory.created.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn oversized_group_does_not_consume_individual_answers() -> Result<(), Box<dyn Error>> {
    let answer = "\"\\\n".repeat(200);
    let first = Arc::new(Script::new(vec![Step::OwnedAnswer(answer.clone())], false));
    let second = Arc::new(Script::new(vec![Step::Answer("small")], false));
    let factory = Arc::new(Factory::new([("large", first), ("small", second)]));
    let manager = manager(factory, 2, 2)?;
    let host = manager.bind("medium".into(), Arc::new(EmptyHost))?;
    let large = spawn(&host, "large", "work", "medium").await?;
    let small = spawn(&host, "small", "work", "medium").await?;
    let group = json!({"a":large,"b":large,"small":small});
    assert!(
        matches!(wait(&host,group,None,2000).await,Err(CapabilityError::Tool {code,..}) if code=="result_limit")
    );
    assert_eq!(
        wait(&host, json!({"only":large}), None, 2000).await?["only"]["answer"],
        answer
    );
    assert_eq!(
        wait(&host, json!({"only":small}), None, 2000).await?["only"]["answer"],
        "small"
    );
    manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn cancellation_keeps_capacity_until_settlement_and_shutdown_can_resume()
-> Result<(), Box<dyn Error>> {
    let factory = Arc::new(Factory::new([]));
    let manager = manager(factory.clone(), 1, 2)?;
    let host = manager.bind("medium".into(), Arc::new(EmptyHost))?;
    let reference = spawn(&host, "setup_settlement", "work", "medium").await?;
    factory.setup_entered.acquire().await?.forget();
    assert_eq!(
        call(&host, "cocoa.cancel", json!({"reference":reference})).await?,
        json!({"name":"setup_settlement","state":"cancelling"})
    );
    assert!(manager.has_active_agents().await?);
    assert!(matches!(spawn(&host,"other","work","medium").await,
        Err(CapabilityError::Tool {code,..}) if code=="capacity"));
    let mut shutdown = Box::pin(manager.shutdown());
    assert!(
        tokio::time::timeout(Duration::from_millis(10), &mut shutdown)
            .await
            .is_err()
    );
    drop(shutdown);
    assert!(factory.closed.lock().await.is_empty());
    factory.setup_release.add_permits(1);
    manager.shutdown().await?;
    assert_eq!(*factory.closed.lock().await, vec!["setup_settlement"]);
    assert_eq!(factory.created.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn setup_cancellation_faults_admission_without_abandoning_cleanup()
-> Result<(), Box<dyn Error>> {
    for requested in [true, false] {
        let factory = Arc::new(Factory::new([]));
        let mut child_limits = limits();
        child_limits.stall_timeout = if requested {
            Duration::from_secs(3)
        } else {
            Duration::from_millis(20)
        };
        child_limits.settlement_timeout = Duration::from_millis(20);
        child_limits.shutdown_timeout = Duration::from_millis(20);
        let manager = AgentManager::new(
            AgentLimits {
                active: 2,
                retained: 3,
            },
            child_limits,
            factory.clone(),
        )?;
        let host = manager.bind("medium".into(), Arc::new(EmptyHost))?;
        let reference = spawn(&host, "setup_settlement", "work", "medium").await?;
        factory.setup_entered.acquire().await?.forget();
        if requested {
            call(&host, "cocoa.cancel", json!({"reference":reference})).await?;
        }
        assert!(matches!(
            tokio::time::timeout(
                Duration::from_secs(1),
                wait(&host, json!({"work":reference}), None, 4096)
            )
            .await?,
            Err(CapabilityError::Unsettled { .. })
        ));
        assert!(matches!(
            spawn(&host, "later", "work", "medium").await,
            Err(CapabilityError::Unsettled { .. })
        ));
        assert!(matches!(
            manager.has_active_agents().await,
            Err(CapabilityError::Unsettled { .. })
        ));
        assert!(matches!(
            manager.shutdown().await,
            Err(CapabilityError::Unsettled { .. })
        ));
        assert!(factory.closed.lock().await.is_empty());
        factory.setup_release.add_permits(1);
        assert!(matches!(
            manager.shutdown().await,
            Err(CapabilityError::Unsettled { .. })
        ));
        assert_eq!(*factory.closed.lock().await, vec!["setup_settlement"]);
    }
    Ok(())
}

#[tokio::test]
async fn overdue_first_poll_reports_deadline_without_faulting_other_work()
-> Result<(), Box<dyn Error>> {
    let factory = Arc::new(Factory::new([
        (
            "expired",
            Arc::new(Script::new(vec![Step::Answer("late")], false)),
        ),
        (
            "ready",
            Arc::new(Script::new(vec![Step::Answer("ready answer")], false)),
        ),
    ]));
    let mut child_limits = limits();
    child_limits.stall_timeout = Duration::from_secs(1);
    let manager = AgentManager::new(
        AgentLimits {
            active: 1,
            retained: 2,
        },
        child_limits,
        factory,
    )?;
    let host = manager.bind("medium".into(), Arc::new(EmptyHost))?;
    let expired = spawn(&host, "expired", "work", "medium").await?;
    std::thread::sleep(Duration::from_millis(1100));
    assert_eq!(
        wait(&host, json!({"late":expired}), None, 4096).await?["late"],
        json!({"name":"expired","status":"failed","error":{"code":"deadline","message":"agent deadline exhausted"}})
    );
    let ready = spawn(&host, "ready", "work", "medium").await?;
    assert_eq!(
        wait(&host, json!({"next":ready}), None, 4096).await?["next"],
        json!({"name":"ready","status":"completed","answer":"ready answer"})
    );
    manager.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn admission_deadline_includes_setup_and_unsafe_cleanup_faults_session()
-> Result<(), Box<dyn Error>> {
    let factory = Arc::new(Factory::new([]));
    let mut child_limits = limits();
    child_limits.stall_timeout = Duration::from_millis(20);
    let manager = AgentManager::new(
        AgentLimits {
            active: 1,
            retained: 2,
        },
        child_limits,
        factory.clone(),
    )?;
    let host = manager.bind("medium".into(), Arc::new(EmptyHost))?;
    let delayed = spawn(&host, "setup_wait", "work", "medium").await?;
    let result = wait(&host, json!({"setup":delayed}), None, 4096).await?;
    assert_eq!(result["setup"]["error"]["code"], "deadline");
    let uncertain = spawn(&host, "unsafe_setup", "work", "medium").await?;
    assert!(matches!(
        wait(&host, json!({"unsafe":uncertain}), None, 4096).await,
        Err(CapabilityError::Unsettled { .. })
    ));
    assert!(matches!(
        spawn(&host, "later", "work", "medium").await,
        Err(CapabilityError::Unsettled { .. })
    ));
    assert!(matches!(
        call(&host, "anything", json!({})).await,
        Err(CapabilityError::Unsettled { .. })
    ));
    assert!(matches!(
        manager.shutdown().await,
        Err(CapabilityError::Unsettled { .. })
    ));
    assert_eq!(factory.created.load(Ordering::SeqCst), 2);
    Ok(())
}

#[tokio::test]
async fn repeated_shutdown_never_closes_an_environment_after_an_unconfirmed_worker_join()
-> Result<(), Box<dyn Error>> {
    for name in ["unsafe_shutdown", "panicking_shutdown", "cancelled_setup"] {
        let child = Arc::new(Script::new(vec![Step::Answer("done")], false));
        let factory = Arc::new(Factory::new([(name, child)]));
        let manager = manager(factory.clone(), 1, 1)?;
        let mut updates = manager.subscribe();
        let host = manager.bind("medium".into(), Arc::new(EmptyHost))?;
        let reference = spawn(&host, name, "work", "medium").await?;
        if name == "cancelled_setup" {
            factory.setup_entered.acquire().await?.forget();
            call(&host, "cocoa.cancel", json!({"reference":reference})).await?;
            factory.setup_release.add_permits(1);
        }
        assert!(matches!(
            wait(&host, json!({"w":reference}), None, 4096).await,
            Err(CapabilityError::Unsettled { .. })
        ));
        assert!(matches!(
            spawn(&host, "later", "work", "medium").await,
            Err(CapabilityError::Unsettled { .. })
        ));
        for _ in 0..2 {
            assert!(matches!(
                manager.shutdown().await,
                Err(CapabilityError::Unsettled { .. })
            ));
            assert!(
                factory.closed.lock().await.is_empty(),
                "uncertain worker cleanup must not permit environment teardown"
            );
        }
        let observed = updates.drain();
        assert_eq!(observed.len(), 2);
        assert!(matches!(observed[0].kind, AgentUpdateKind::Started));
        assert!(matches!(
            observed[1].kind,
            AgentUpdateKind::Finished(ActivityStatus::Unknown)
        ));
    }
    Ok(())
}

fn manager(
    factory: Arc<Factory>,
    active: usize,
    retained: usize,
) -> Result<AgentManager, Box<dyn Error>> {
    Ok(AgentManager::new(
        AgentLimits { active, retained },
        limits(),
        factory,
    )?)
}

async fn spawn(
    host: &Arc<dyn CapabilityHost>,
    name: &str,
    prompt: &str,
    alias: &str,
) -> Result<Value, CapabilityError> {
    Ok(call(
        host,
        "cocoa.spawn",
        json!({"name":name,"prompt":prompt,"alias":alias}),
    )
    .await?["reference"]
        .clone())
}

async fn wait(
    host: &Arc<dyn CapabilityHost>,
    handles: Value,
    timeout: Option<u32>,
    bytes: usize,
) -> Result<Value, CapabilityError> {
    let mut arguments = json!({"handles":handles});
    if let Some(timeout) = timeout {
        arguments["timeout_ms"] = json!(timeout);
    }
    host.call(
        "cocoa.wait",
        arguments,
        work_limits(bytes),
        CancellationToken::new(),
    )
    .await
}

async fn call(
    host: &Arc<dyn CapabilityHost>,
    name: &str,
    arguments: Value,
) -> Result<Value, CapabilityError> {
    host.call(name, arguments, work_limits(4096), CancellationToken::new())
        .await
}

fn work_limits(output_bytes: usize) -> WorkLimits {
    WorkLimits {
        output_bytes,
        deadline: Instant::now() + Duration::from_secs(5),
    }
}

fn limits() -> Limits {
    Limits {
        input_bytes: 4096,
        output_bytes: 4096,
        record_bytes: 65536,
        context_bytes: 1024 * 1024,
        pending_operations: 8,
        stall_timeout: Duration::from_secs(3),
        settlement_timeout: Duration::from_millis(500),
        shutdown_timeout: Duration::from_millis(500),
    }
}

async fn persisted_engine(
    model: Arc<dyn ModelProvider>,
    manager: &AgentManager,
    store: SqliteStore,
    directory: &std::path::Path,
    resume: bool,
) -> Result<(Engine, Arc<ProjectEnvironment>), Box<dyn Error>> {
    let local = LocalEnvironment::new(
        LocalConfiguration {
            working_directory: directory.into(),
            shell: "/bin/bash".into(),
            environment: BTreeMap::new(),
            output_directory: directory.into(),
        },
        CodingLimits {
            file_bytes: 65536,
            observed_bytes: 1024 * 1024,
            observations: 64,
            processes: 2,
            captured_output_bytes: 65536,
            retained_output_bytes: 1024 * 1024,
            preview_lines: 200,
            default_timeout: Duration::from_secs(3),
        },
    )
    .await?;
    let environment = Arc::new(
        ProjectEnvironment::new(local, None)?.with_instruction_acceptance(Arc::new(store.clone())),
    );
    let executor = quickjs().await?;
    let settings = EngineSettings::new(limits())?;
    let adapters = EngineAdapters {
        model,
        executor: Box::new(executor),
        capabilities: manager.bind("medium".into(), Arc::new(EmptyHost))?,
        context: manager.bind_context(environment.clone()),
        store: Arc::new(store),
    };
    let engine = if resume {
        Engine::resume(settings, adapters, None).await?
    } else {
        Engine::new(settings, adapters, None)
    };
    Ok((engine, environment))
}

async fn engine(
    model: Arc<dyn ModelProvider>,
    capabilities: Arc<dyn CapabilityHost>,
    limits: Limits,
) -> Result<Engine, Box<dyn Error>> {
    Ok(Engine::new(
        EngineSettings::new(limits)?,
        EngineAdapters {
            model,
            executor: Box::new(quickjs().await?),
            capabilities,
            context: Arc::new(BoundedContext),
            store: Arc::new(InMemoryStore::new(StoreLimits {
                record_bytes: limits.record_bytes,
            })?),
        },
        None,
    ))
}

async fn quickjs() -> Result<QuickJsExecutor, cocoa_quickjs::QuickJsError> {
    QuickJsExecutor::new(QuickJsOptions {
        memory_bytes: 4 * 1024 * 1024,
        stack_bytes: 256 * 1024,
        feedback: FeedbackLimits {
            output_bytes: 4096,
            printed_bytes: 3072,
            error_bytes: 512,
            print_calls: 16,
        },
    })
    .await
}

struct Factory {
    models: BTreeMap<String, Arc<Script>>,
    forward_observations: bool,
    created: AtomicUsize,
    closed: Mutex<Vec<String>>,
    setup_entered: Semaphore,
    setup_release: Semaphore,
    cleanup_entered: Arc<Semaphore>,
    cleanup_release: Arc<Semaphore>,
}

impl Factory {
    fn new<const N: usize>(models: [(&str, Arc<Script>); N]) -> Self {
        Self {
            models: models
                .into_iter()
                .map(|(name, model)| (name.into(), model))
                .collect(),
            forward_observations: false,
            created: AtomicUsize::new(0),
            closed: Mutex::new(Vec::new()),
            setup_entered: Semaphore::new(0),
            setup_release: Semaphore::new(0),
            cleanup_entered: Arc::new(Semaphore::new(0)),
            cleanup_release: Arc::new(Semaphore::new(0)),
        }
    }
}

#[async_trait]
impl AgentFactory for Factory {
    fn supports_alias(&self, alias: &str) -> bool {
        matches!(alias, "medium" | "high")
    }

    async fn create(
        &self,
        participant: AgentParticipant,
        _alias: &str,
        limits: Limits,
        cancellation: CancellationToken,
    ) -> Result<Engine, CapabilityError> {
        let name = participant.name();
        self.created.fetch_add(1, Ordering::SeqCst);
        if name == "setup_wait" {
            cancellation.cancelled().await;
            return Err(CapabilityError::Cancelled);
        }
        if matches!(name, "setup_settlement" | "cancelled_setup") {
            self.setup_entered.add_permits(1);
            cancellation.cancelled().await;
            self.setup_release
                .acquire()
                .await
                .map_err(|_| CapabilityError::Unsettled {
                    message: "fixture settlement gate closed".into(),
                })?
                .forget();
            if name == "setup_settlement" {
                return Err(CapabilityError::Cancelled);
            }
        }
        if name == "unsafe_setup" {
            return Err(CapabilityError::Unsettled {
                message: "fixture uncertainty".into(),
            });
        }
        let model = self
            .models
            .get(name)
            .ok_or_else(|| CapabilityError::Rejected {
                message: "missing fixture".into(),
            })?
            .clone();
        let executor = match name {
            "unsafe_shutdown" | "cancelled_setup" => Some(FixtureExecutor::Unsettled),
            "panicking_shutdown" => Some(FixtureExecutor::Panicking),
            "expired" | "ready" => Some(FixtureExecutor::Idle),
            "cleanup_wait" => Some(FixtureExecutor::Gated {
                entered: self.cleanup_entered.clone(),
                release: self.cleanup_release.clone(),
            }),
            _ => None,
        };
        let engine = async {
            let settings = EngineSettings::new(limits)?;
            let store_limits = StoreLimits {
                record_bytes: limits.record_bytes,
            };
            let (store, context): (Arc<dyn Store>, Arc<dyn ContextAssembler>) =
                match participant.durable_store(store_limits).await? {
                    Some(store) => (Arc::new(store), Arc::new(EmptyInstructions)),
                    None => (
                        Arc::new(InMemoryStore::new(store_limits)?),
                        Arc::new(BoundedContext),
                    ),
                };
            let executor: Box<dyn CellExecutor> = if let Some(executor) = executor {
                Box::new(executor)
            } else {
                Box::new(quickjs().await?)
            };
            Ok::<_, Box<dyn Error>>(Engine::new(
                settings,
                EngineAdapters {
                    model,
                    executor,
                    capabilities: participant.bind(Arc::new(EmptyHost)),
                    context: participant.bind_context(context),
                    store,
                },
                if self.forward_observations {
                    participant.events()
                } else {
                    None
                },
            ))
        };
        engine.await.map_err(|error| CapabilityError::Backend {
            message: error.to_string(),
            effects: Effects::None,
        })
    }

    async fn close(&self, name: &str) -> Result<(), CapabilityError> {
        self.closed.lock().await.push(name.into());
        Ok(())
    }
}

struct EmptyInstructions;

#[async_trait]
impl ContextAssembler for EmptyInstructions {
    async fn prepare(
        &self,
        records: Vec<StoredRecord>,
        capabilities: Vec<CapabilityDescription>,
        bytes: usize,
    ) -> Result<PreparedContext, ContextError> {
        let invocation = records
            .last()
            .ok_or_else(|| ContextError::Unavailable {
                message: "fixture requires an invocation".into(),
            })?
            .record
            .invocation;
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

enum FixtureExecutor {
    Idle,
    Unsettled,
    Panicking,
    Gated {
        entered: Arc<Semaphore>,
        release: Arc<Semaphore>,
    },
}

#[async_trait]
impl CellExecutor for FixtureExecutor {
    async fn execute(
        &mut self,
        _cell: Cell,
        _operations: &OperationContext,
        _cancellation: CancellationToken,
    ) -> Result<CellResponse, ExecutionError> {
        Err(ExecutionError::Unavailable {
            message: "fixture does not execute cells".into(),
        })
    }

    async fn shutdown(
        &mut self,
        _cancellation: CancellationToken,
    ) -> Result<ShutdownReport, ExecutionError> {
        match self {
            Self::Idle => Ok(ShutdownReport { unresolved_work: 0 }),
            Self::Unsettled => Err(ExecutionError::Unavailable {
                message: "injected unconfirmed worker join".into(),
            }),
            Self::Panicking => panic!("injected worker shutdown panic"),
            Self::Gated { entered, release } => {
                entered.add_permits(1);
                release
                    .acquire()
                    .await
                    .map_err(|_| ExecutionError::Unavailable {
                        message: "fixture cleanup gate closed".into(),
                    })?
                    .forget();
                Ok(ShutdownReport { unresolved_work: 0 })
            }
        }
    }
}

enum Step {
    Cell(&'static str, Vec<Value>),
    OwnedCell(String, Vec<Value>),
    Batch(Vec<Cell>),
    InterruptedCell(&'static str),
    Answer(&'static str),
    OwnedAnswer(String),
    StreamedAnswer { text: &'static str, chunks: u32 },
    Fail,
}

struct Script {
    steps: Mutex<VecDeque<Step>>,
    expected: Mutex<Option<Vec<Value>>>,
    release: Semaphore,
    entered: Semaphore,
    requests: AtomicUsize,
    continuation: Option<ProviderData>,
}

impl Script {
    fn new(steps: Vec<Step>, blocked: bool) -> Self {
        Self {
            steps: Mutex::new(steps.into()),
            expected: Mutex::new(None),
            release: Semaphore::new(usize::from(!blocked)),
            entered: Semaphore::new(0),
            requests: AtomicUsize::new(0),
            continuation: None,
        }
    }
}

#[async_trait]
impl ModelProvider for Script {
    async fn respond(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        if self.requests.fetch_add(1, Ordering::SeqCst) == 0 {
            self.entered.add_permits(1);
            tokio::select! {
                () = cancellation.cancelled() => return Err(ModelError::Cancelled),
                permit = self.release.acquire() => match permit {
                    Ok(permit) => permit.forget(),
                    Err(_) => return Err(ModelError::Protocol {message:"closed fixture gate".into()}),
                },
            }
        }
        if let Some(expected) = self.expected.lock().await.take() {
            let outcome =
                request
                    .context
                    .records
                    .iter()
                    .rev()
                    .find_map(|record| match &record.kind {
                        RecordKind::CellCompleted { outcome, .. } => Some(outcome),
                        _ => None,
                    });
            assert_eq!(
                outcome,
                Some(&CellResponse::Completed { printed: expected })
            );
        }
        let step = self
            .steps
            .lock()
            .await
            .pop_front()
            .ok_or_else(|| ModelError::Protocol {
                message: "script exhausted".into(),
            })?;
        let action = match step {
            Step::Cell(code, expected) => {
                *self.expected.lock().await = Some(expected);
                ModelAction::Execute {
                    cells: vec![Cell { code: code.into() }],
                }
            }
            Step::OwnedCell(code, expected) => {
                *self.expected.lock().await = Some(expected);
                ModelAction::Execute {
                    cells: vec![Cell { code }],
                }
            }
            Step::Batch(cells) => ModelAction::Execute { cells },
            Step::InterruptedCell(code) => ModelAction::Execute {
                cells: vec![Cell { code: code.into() }],
            },
            Step::Answer(answer) => ModelAction::Final {
                answer: answer.into(),
            },
            Step::OwnedAnswer(answer) => ModelAction::Final { answer },
            Step::StreamedAnswer { text, chunks } => {
                let mut answer = String::new();
                for _ in 0..chunks {
                    request.observations.assistant_text(text);
                    answer.push_str(text);
                }
                ModelAction::Final { answer }
            }
            Step::Fail => {
                return Err(ModelError::Transport {
                    message: "private provider diagnostic".into(),
                });
            }
        };
        Ok(ModelResponse {
            action,
            continuation: self.continuation.clone(),
            usage: None,
        })
    }
}

struct EmptyHost;

#[async_trait]
impl CapabilityHost for EmptyHost {
    fn descriptions(&self) -> Vec<CapabilityDescription> {
        vec![]
    }
    async fn call(
        &self,
        _name: &str,
        _arguments: Value,
        _limits: WorkLimits,
        _cancellation: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        Err(CapabilityError::Rejected {
            message: "no coding grants in this fixture".into(),
        })
    }
}
