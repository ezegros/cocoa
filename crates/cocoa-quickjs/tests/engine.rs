//! JavaScript execution and host-boundary contracts through the engine API.

use std::{error::Error, sync::Arc, time::Duration};

use async_trait::async_trait;
use cocoa_core::{
    capability::{CapabilityDescription, CapabilityError, CapabilityHost},
    compaction::{CheckpointLimits, CompactionPolicy, RecapStatus, RequestState, RequiredOrigin},
    context::BoundedContext,
    engine::{Engine, EngineAdapters, EngineSettings, EventPublisher, Limits, RunRequest},
    execution::{
        Cell, CellExecutor, CellResponse as CellOutcome, ContextEffects, FeedbackError,
        OperationEffects, WorkLimits,
    },
    model::{ModelAction, ModelError, ModelProvider, ModelRequest, ModelResponse, SummaryRequest},
    store::{AgentId, InMemoryStore, RecordKind, Store, StoreLimits},
};
use cocoa_quickjs::{QuickJsError, QuickJsExecutor, QuickJsOptions};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

struct Cells(Vec<String>);

#[async_trait]
impl ModelProvider for Cells {
    async fn respond(
        &self,
        request: ModelRequest,
        _: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        let count = request
            .context
            .records
            .iter()
            .rev()
            .take_while(|record| !matches!(record.kind, RecordKind::InputAccepted { .. }))
            .filter(|record| matches!(record.kind, RecordKind::CellCompleted { .. }))
            .count();
        Ok(ModelResponse {
            action: match self.0.get(count) {
                Some(code) => ModelAction::Execute {
                    cells: vec![Cell { code: code.clone() }],
                },
                None => ModelAction::Final {
                    answer: "settled".into(),
                },
            },
            continuation: None,
            usage: None,
        })
    }
}

struct Host;

struct ResumingCells {
    before: Vec<String>,
    after: String,
}

#[async_trait]
impl ModelProvider for ResumingCells {
    async fn respond(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        let resume = request.context.records.iter().rev().find_map(|record| {
            if let RecordKind::InputAccepted { input } = &record.kind {
                Some(input.text() == "resume")
            } else {
                None
            }
        }) == Some(true);
        Cells(if resume {
            vec![self.after.clone()]
        } else {
            self.before.clone()
        })
        .respond(request, cancellation)
        .await
    }
}

#[async_trait]
impl CapabilityHost for Host {
    fn descriptions(&self) -> Vec<CapabilityDescription> {
        Vec::new()
    }
    async fn call(
        &self,
        name: &str,
        arguments: Value,
        _: WorkLimits,
        cancellation: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        if name == "echo" {
            return Ok(arguments);
        }
        tokio::select! {
            () = cancellation.cancelled() => return Err(CapabilityError::Cancelled),
            () = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
        if name == "multiply" {
            let left = arguments["left"]
                .as_i64()
                .ok_or_else(|| CapabilityError::Rejected {
                    message: "left must be an integer".into(),
                })?;
            let right = arguments["right"]
                .as_i64()
                .ok_or_else(|| CapabilityError::Rejected {
                    message: "right must be an integer".into(),
                })?;
            Ok(json!(left * right))
        } else {
            Err(CapabilityError::Rejected {
                message: "no grants".into(),
            })
        }
    }
}

struct CodingHost {
    grants: Vec<&'static str>,
    calls: tokio::sync::Mutex<Vec<(String, Value)>>,
}

struct ContextErrorHost {
    error: CapabilityError,
}

#[async_trait]
impl CapabilityHost for ContextErrorHost {
    fn descriptions(&self) -> Vec<CapabilityDescription> {
        CodingHost::new(&["context.get", "context.set", "context.list"]).descriptions()
    }

    async fn call(
        &self,
        _: &str,
        arguments: Value,
        _: WorkLimits,
        _: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        if arguments["key"] == "fail" || arguments["fail"] == true {
            Err(self.error.clone())
        } else {
            Ok(arguments)
        }
    }
}

struct SpawnHost {
    calls: tokio::sync::Mutex<Vec<(String, Value)>>,
    wait_granted: std::sync::atomic::AtomicBool,
}

impl SpawnHost {
    fn new() -> Self {
        Self {
            calls: tokio::sync::Mutex::new(Vec::new()),
            wait_granted: std::sync::atomic::AtomicBool::new(true),
        }
    }
}

#[async_trait]
impl CapabilityHost for SpawnHost {
    fn descriptions(&self) -> Vec<CapabilityDescription> {
        CodingHost::new(&["cocoa.spawn", "cocoa.wait", "cocoa.cancel", "cocoa.read"]).descriptions()
    }

    async fn call(
        &self,
        name: &str,
        arguments: Value,
        _: WorkLimits,
        _: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        self.calls
            .lock()
            .await
            .push((name.into(), arguments.clone()));
        match name {
            "cocoa.spawn" => Ok(json!({
                "reference": format!("private:{}", arguments["name"].as_str().ok_or_else(|| CapabilityError::Rejected { message: "missing name".into() })?),
                "name": arguments["name"],
                "alias": arguments.get("alias").cloned().unwrap_or(json!("standard")),
            })),
            "cocoa.wait" => {
                if !self.wait_granted.load(std::sync::atomic::Ordering::SeqCst) {
                    return Err(CapabilityError::Rejected {
                        message: "wait grant revoked".into(),
                    });
                }
                let handles =
                    arguments["handles"]
                        .as_object()
                        .ok_or_else(|| CapabilityError::Rejected {
                            message: "expected keyed handles".into(),
                        })?;
                let mut outcomes = serde_json::Map::new();
                for (alias, reference) in handles {
                    let name = reference
                        .as_str()
                        .and_then(|reference| reference.strip_prefix("private:"))
                        .ok_or_else(|| CapabilityError::Rejected {
                            message: "foreign reference".into(),
                        })?;
                    outcomes.insert(
                        alias.clone(),
                        json!({"name": name, "state": if arguments["timeout_ms"] == 0 { "pending" } else { "completed" }}),
                    );
                }
                Ok(Value::Object(outcomes))
            }
            "cocoa.cancel" => {
                let name = arguments["reference"]
                    .as_str()
                    .and_then(|reference| reference.strip_prefix("private:"))
                    .ok_or_else(|| CapabilityError::Rejected {
                        message: "foreign reference".into(),
                    })?;
                Ok(json!({"name": name, "state": "cancelled"}))
            }
            _ => Ok(arguments),
        }
    }
}

impl CodingHost {
    fn new(grants: &[&'static str]) -> Self {
        Self {
            grants: grants.to_vec(),
            calls: tokio::sync::Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl CapabilityHost for CodingHost {
    fn descriptions(&self) -> Vec<CapabilityDescription> {
        self.grants
            .iter()
            .map(|name| CapabilityDescription {
                name: (*name).into(),
                description: format!("Injected {name} contract"),
                parameters: json!({"type":"object"}),
            })
            .collect()
    }

    async fn call(
        &self,
        name: &str,
        arguments: Value,
        _: WorkLimits,
        cancellation: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        use cocoa_core::capability::Effects;
        if !self.grants.contains(&name) {
            return Err(CapabilityError::Rejected {
                message: "not granted".into(),
            });
        }
        let delay = if arguments["wait"] == true { 5000 } else { 2 };
        tokio::select! {
            () = cancellation.cancelled() => return Err(CapabilityError::Cancelled),
            () = tokio::time::sleep(Duration::from_millis(delay)) => {}
        }
        if let Some(fault) = arguments["fault"].as_str() {
            return Err(match fault {
                "cancelled" => CapabilityError::Cancelled,
                "deadline" => CapabilityError::Deadline,
                "budget" => CapabilityError::Budget,
                _ => CapabilityError::OutputLimit,
            });
        }
        if let Some(effects) = arguments["fail"].as_str() {
            return Err(CapabilityError::Tool {
                code: "stale_read".into(),
                message: "Read again before editing".into(),
                effects: match effects {
                    "partial" => Effects::Partial,
                    "unknown" => Effects::Unknown,
                    _ => Effects::None,
                },
            });
        }
        self.calls
            .lock()
            .await
            .push((name.into(), arguments.clone()));
        match name {
            "cocoa.read" => Ok(
                json!({"kind":"text","file":"observed","path":arguments["path"],"bytes":17,"line_count":1,"page":{"start_line":1,"line_count":1,"text":"old source\n"},"next":null}),
            ),
            "cocoa.write" | "cocoa.edit" => Ok(
                json!({"kind":"text","file":"updated","path":"src/generated.rs","bytes":31,"line_count":2}),
            ),
            "cocoa.run" => Ok(json!({"reference":"process.fixture.1","kind":"process"})),
            "cocoa.wait" => Ok(
                json!({"":{"status":{"kind":"exited","code":7},"stdout":{"text":"failed\n","truncated":false,"lossy":false,"output_path":null},"stderr":{"text":"","truncated":false,"lossy":false,"output_path":null}}}),
            ),
            _ => Ok(arguments),
        }
    }
}

#[tokio::test]
async fn context_namespace_exposes_only_advertised_methods_and_cannot_be_replaced()
-> Result<(), Box<dyn Error>> {
    for grants in [
        vec![],
        vec![
            "context.transfer",
            "context.delete",
            "context.clear",
            "context.get.extra",
        ],
        vec!["context.get"],
        vec!["context.set", "context.list"],
    ] {
        let host = Arc::new(CodingHost::new(&grants));
        let (mut engine, store) = engine_with_host(
            &["print(typeof context === 'undefined' ? null : Object.keys(context).sort()); print(typeof cocoa)"],
            None,
            engine_limits(),
            host.clone(),
        ).await?;
        run(&mut engine).await?;
        let mut expected: Vec<_> = grants
            .iter()
            .filter_map(|name| match *name {
                "context.get" => Some("get"),
                "context.set" => Some("set"),
                "context.list" => Some("list"),
                _ => None,
            })
            .collect();
        expected.sort();
        assert_eq!(
            outcomes(&store).await?,
            vec![CellOutcome::Completed {
                printed: vec![
                    if expected.is_empty() {
                        Value::Null
                    } else {
                        json!(expected)
                    },
                    json!("undefined")
                ],
            }]
        );
        assert!(host.calls.lock().await.is_empty());
        engine.shutdown().await?;
    }

    let host = Arc::new(CodingHost::new(&[
        "context.get",
        "context.set",
        "context.list",
        "context.transfer",
        "context.delete",
        "context.clear",
        "context.custom",
        "context.get.extra",
        "cocoa.read",
    ]));
    let (mut engine, store) = engine_with_host(&[
        "var originalContext = context; var originalGet = context.get; print([Object.keys(context).sort(),Object.isFrozen(context),Object.getPrototypeOf(context) === null,Object.getOwnPropertyDescriptor(globalThis,'context').writable,Object.getOwnPropertyDescriptor(globalThis,'context').configurable]); print([Reflect.set(globalThis,'context',{}),Reflect.deleteProperty(globalThis,'context'),Reflect.defineProperty(globalThis,'context',{value:{}}),Reflect.set(context,'get',()=>0),Reflect.deleteProperty(context,'set'),Reflect.defineProperty(context,'list',{value:()=>0}),Reflect.set(context,'clear',()=>0),context===originalContext,context.get===originalGet]); print([typeof context.transfer,typeof context.delete,typeof context.clear,typeof context.custom,typeof context.get.extra]); print((await cocoa.read({path:'unchanged'})).path)",
    ], None, engine_limits(), host.clone()).await?;
    run(&mut engine).await?;
    assert_eq!(
        outcomes(&store).await?,
        vec![CellOutcome::Completed {
            printed: vec![
                json!([["get", "list", "set"], true, true, false, false]),
                json!([false, false, false, false, false, false, false, true, true]),
                json!([
                    "undefined",
                    "undefined",
                    "undefined",
                    "undefined",
                    "undefined"
                ]),
                json!("unchanged"),
            ],
        }]
    );
    assert_eq!(
        *host.calls.lock().await,
        vec![("cocoa.read".into(), json!({"path":"unchanged"}))]
    );
    engine.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn context_positional_calls_snapshot_arguments_and_use_saved_functions_on_current_turns()
-> Result<(), Box<dyn Error>> {
    let host = Arc::new(CodingHost::new(&[
        "context.get",
        "context.set",
        "context.list",
    ]));
    let (mut engine, store) = engine_with_host(&[
        "var savedGet = savedGet ?? context.get; var savedSet = savedSet ?? context.set; var savedList = savedList ?? context.list; var content = {nested:[1,null,true]}; var pendingSet = savedSet('entry',content); content.nested[0] = 9; print(pendingSet instanceof Promise); var setResult = await pendingSet; print(setResult); setResult.content.nested[0] = 99; var filters = {owner:'alpha',nested:{prefix:'original'}}; var pendingList = savedList(filters); filters.nested.prefix = 'changed'; print(pendingList instanceof Promise); print(await pendingList)",
        "print(await savedGet('entry')); print(await savedList({})); print(await savedSet('plain','literal \\n text'))",
    ], None, engine_limits(), host.clone()).await?;
    run(&mut engine).await?;
    run(&mut engine).await?;
    let expected = vec![
        CellOutcome::Completed {
            printed: vec![
                json!(true),
                json!({"key":"entry","content":{"nested":[1,null,true]}}),
                json!(true),
                json!({"owner":"alpha","nested":{"prefix":"original"}}),
            ],
        },
        CellOutcome::Completed {
            printed: vec![
                json!({"key":"entry"}),
                json!({}),
                json!({"key":"plain","content":"literal \n text"}),
            ],
        },
    ];
    let feedback = outcomes(&store).await?;
    assert_eq!(&feedback[..2], expected.as_slice());
    assert_eq!(&feedback[2..], expected.as_slice());
    let expected_calls = vec![
        (
            "context.set".into(),
            json!({"key":"entry","content":{"nested":[1,null,true]}}),
        ),
        (
            "context.list".into(),
            json!({"owner":"alpha","nested":{"prefix":"original"}}),
        ),
        ("context.get".into(), json!({"key":"entry"})),
        ("context.list".into(), json!({})),
        (
            "context.set".into(),
            json!({"key":"plain","content":"literal \n text"}),
        ),
    ];
    let calls = host.calls.lock().await;
    assert_eq!(&calls[..5], expected_calls.as_slice());
    assert_eq!(&calls[5..], expected_calls.as_slice());
    drop(calls);
    let records = store.load().await?.into_records();
    let completed: Vec<_> = records
        .iter()
        .filter_map(|record| match &record.kind {
            RecordKind::OperationFinished {
                outcome: cocoa_core::capability::OperationOutcome::Completed { value },
                ..
            } => Some(value.clone()),
            _ => None,
        })
        .collect();
    let expected_values: Vec<_> = expected_calls.into_iter().map(|(_, value)| value).collect();
    assert_eq!(&completed[..5], expected_values.as_slice());
    assert_eq!(&completed[5..], expected_values.as_slice());
    engine.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn context_calls_reject_wrong_arity_and_hostile_values_before_dispatch()
-> Result<(), Box<dyn Error>> {
    let host = Arc::new(CodingHost::new(&[
        "context.get",
        "context.set",
        "context.list",
    ]));
    let (mut engine, store) = engine_with_host(&[
        "var getterRan = false; var jsonRan = false; var cyclic = {}; cyclic.self = cyclic; var symbol = Symbol('x'); var invalid = [()=>context.get(),()=>context.get('k','extra'),()=>context.set('k'),()=>context.set('k','v','extra'),()=>context.list(),()=>context.list({},{}),()=>context.list(null),()=>context.list([]),()=>context.list(7),()=>context.get({get key(){getterRan=true;return 'k'}}),()=>context.set('k',{get value(){getterRan=true;return 'v'}}),()=>context.list({get owner(){getterRan=true;return 'alpha'}}),()=>context.set('k',{toJSON(){jsonRan=true;return 'v'}}),()=>context.list({value:NaN}),()=>context.set('k',Infinity),()=>context.set('k',undefined),()=>context.get(symbol),()=>context.list({[symbol]:1}),()=>context.list(cyclic),()=>context.list(new Date()),()=>context.set('k',[1,,3]),()=>context.get('x'.repeat(9000)),()=>context.set('k','x'.repeat(9000)),()=>context.list({prefix:'x'.repeat(9000)})]; var rejected = []; for (var attempt of invalid) { try { var pending = attempt(); await pending; rejected.push(false); } catch(error) { rejected.push(error instanceof TypeError); } } print(rejected); print([getterRan,jsonRan])",
        "var savedGet=context.get, savedSet=context.set, savedList=context.list; Object.create=Object.defineProperty=Object.getOwnPropertyDescriptor=Object.getPrototypeOf=Object.freeze=()=>{throw Error('patched')}; Array.isArray=()=>{throw Error('patched')}; Reflect.apply=Reflect.ownKeys=()=>{throw Error('patched')}; JSON.stringify=()=>{throw Error('patched')}; TypeError=()=>{throw Error('patched')}; print(await savedGet('after')); print(await savedSet('after',{nested:[1,null]})); print(await savedList({owner:'after'})); try { await savedSet('bad', {get value(){getterRan=true;return 1}}) } catch(error) { print(error.message) } print(getterRan)",
    ], None, engine_limits(), host.clone()).await?;
    run(&mut engine).await?;
    let feedback = outcomes(&store).await?;
    assert_eq!(
        feedback[0],
        CellOutcome::Completed {
            printed: vec![json!(vec![true; 24]), json!([false, false])]
        }
    );
    assert_eq!(
        feedback[1],
        CellOutcome::Completed {
            printed: vec![
                json!({"key":"after"}),
                json!({"key":"after","content":{"nested":[1,null]}}),
                json!({"owner":"after"}),
                json!("accessor at $.content.value"),
                json!(false)
            ]
        }
    );
    assert_eq!(
        *host.calls.lock().await,
        vec![
            ("context.get".into(), json!({"key":"after"})),
            (
                "context.set".into(),
                json!({"key":"after","content":{"nested":[1,null]}})
            ),
            ("context.list".into(), json!({"owner":"after"})),
        ]
    );
    assert_eq!(
        store
            .load()
            .await?
            .into_records()
            .iter()
            .filter(|record| matches!(record.kind, RecordKind::OperationStarted { .. }))
            .count(),
        3
    );
    engine.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn context_errors_keep_owner_metadata_in_rejections_caught_summaries_and_canonical_outcomes()
-> Result<(), Box<dyn Error>> {
    use cocoa_core::capability::{Effects, OperationOutcome};
    for owner in [None, Some(AgentId::try_from(9_007_199_254_740_993)?)] {
        let host_error = CapabilityError::Context {
            code: "context_owned".into(),
            message: "Owned entry".into(),
            owner,
        };
        let (mut engine,store) = engine_with_host(&[
            "var savedContextError; try { await context.get('fail') } catch(error) { savedContextError=error; print({code:error.code,method:error.method,effects:error.effects,owner:error.owner,hasOwner:Object.hasOwn(error,'owner'),isError:error instanceof Error}); error.message='forged'; } print(await context.set('ok','content'))",
            "await context.set('fail','content')",
            "for(var method of ['get','set','list']) { try { if(method==='get') await context.get('fail'); else if(method==='set') await context.set('fail','content'); else await context.list({fail:true}); } catch {} } print('kept'); throw Error('later failure')",
            "context.get('fail'); print('unawaited')",
            "throw savedContextError",
            "print(await context.get('after'))",
        ],None,engine_limits(),Arc::new(ContextErrorHost {error:host_error.clone()})).await?;
        run(&mut engine).await?;
        let feedback = outcomes(&store).await?;
        for (index, method) in [
            (0, "context.get"),
            (1, "context.set"),
            (3, "context.get"),
            (4, "context.get"),
        ] {
            let CellOutcome::Failed { error, .. } = &feedback[index] else {
                panic!("hidden context failure: {:?}", feedback[index]);
            };
            assert_eq!(
                serde_json::to_value(error)?,
                json!({"kind":"context","method":method,"code":"context_owned","message":"Owned entry","owner":owner,"operation_effects":"none","truncated":false})
            );
        }
        assert!(
            matches!(&feedback[0],CellOutcome::Failed {printed,..} if printed==&vec![json!({"code":"context_owned","method":"context.get","effects":"none","owner":owner,"hasOwner":true,"isError":true}),json!({"key":"ok","content":"content"})])
        );
        assert!(
            matches!(&feedback[2],CellOutcome::Failed {error:FeedbackError::Context {owner:actual,operation_effects:ContextEffects::None,message,truncated:false,..},printed} if actual==&owner && message.contains("3 recoverable host failures") && message.contains("context.set") && message.contains("context.list") && message.contains("later failure") && printed==&[json!("kept")])
        );
        assert_eq!(
            feedback[5],
            CellOutcome::Completed {
                printed: vec![json!({"key":"after"})]
            }
        );
        let records = store.load().await?.into_records();
        let errors: Vec<_> = records
            .iter()
            .filter_map(|record| match &record.kind {
                RecordKind::OperationFinished {
                    outcome: OperationOutcome::Failed { error },
                    ..
                } => Some(error),
                _ => None,
            })
            .collect();
        assert_eq!(errors.len(), 6);
        for error in errors {
            assert_eq!(error, &host_error);
            assert_eq!(error.effects(), Effects::None);
        }
        engine.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn context_owner_metadata_is_never_shortened_to_fit_small_feedback_budgets()
-> Result<(), Box<dyn Error>> {
    for output_bytes in [128, 512, 1024, 16384] {
        for owner in [
            None,
            Some(AgentId::try_from(1)?),
            Some(AgentId::try_from(i64::MAX)?),
        ] {
            let limits = engine_limits();
            let store = Arc::new(InMemoryStore::new(StoreLimits {
                record_bytes: limits.record_bytes,
            })?);
            let expected_owner = serde_json::to_string(&owner)?;
            let cells=vec![
                format!("try {{ await context.get('fail') }} catch(error) {{ print(error.owner === {expected_owner} && error.code === 'context_owned' && error.method === 'context.get' && error.effects === 'none'); }}"),
                "await context.set('fail','content')".into(),
                "for(var index=0;index<10;index++) { try { await context.list({fail:true}) } catch {} } print(true)".into(),
            ];
            let executor = QuickJsExecutor::new(QuickJsOptions {
                memory_bytes: 8 * 1024 * 1024,
                stack_bytes: 256 * 1024,
                feedback: cocoa_quickjs::FeedbackLimits {
                    output_bytes,
                    error_bytes: (output_bytes / 4).clamp(96, 2048),
                    ..feedback_limits()
                },
            })
            .await?;
            let host_error = CapabilityError::Context {
                code: "context_owned".into(),
                message: "long message 🦀\"\n".repeat(200),
                owner,
            };
            let mut engine = Engine::new(
                EngineSettings::new(limits)?,
                EngineAdapters {
                    model: Arc::new(Cells(cells)),
                    executor: Box::new(executor),
                    capabilities: Arc::new(ContextErrorHost {
                        error: host_error.clone(),
                    }),
                    context: Arc::new(BoundedContext),
                    store: store.clone(),
                },
                None,
            );
            run(&mut engine).await?;
            let error_bytes = (output_bytes / 4).clamp(64, 2048);
            for (index, outcome) in outcomes(&store).await?.iter().enumerate() {
                assert!(
                    serde_json::to_vec(outcome)?.len() <= output_bytes,
                    "{outcome:?}"
                );
                let CellOutcome::Failed { error, printed } = outcome else {
                    panic!("hidden failure: {outcome:?}");
                };
                assert_eq!(
                    printed,
                    &if index == 1 {
                        vec![]
                    } else {
                        vec![json!(true)]
                    }
                );
                assert!(serde_json::to_vec(error)?.len() <= error_bytes, "{error:?}");
                let method = if index == 0 {
                    "context.get"
                } else if index == 1 {
                    "context.set"
                } else {
                    "context.list"
                };
                let metadata = FeedbackError::Context {
                    method: method.into(),
                    code: "context_owned".into(),
                    message: String::new(),
                    owner,
                    operation_effects: ContextEffects::None,
                    truncated: true,
                };
                if serde_json::to_vec(&metadata)?.len() <= error_bytes {
                    assert!(
                        matches!(error,FeedbackError::Context {owner:actual,operation_effects:ContextEffects::None,truncated:true,..} if actual==&owner),
                        "{error:?}"
                    );
                } else {
                    assert!(
                        matches!(
                            error,
                            FeedbackError::Javascript {
                                truncated: true,
                                ..
                            }
                        ),
                        "{error:?}"
                    );
                }
                if index == 2 {
                    assert!(
                        matches!(error,FeedbackError::Context {message,..}|FeedbackError::Javascript {message,..} if message.starts_with("10 ")),
                        "{error:?}"
                    );
                }
            }
            let records = store.load().await?.into_records();
            let retained: Vec<_> = records
                .iter()
                .filter_map(|record| match &record.kind {
                    RecordKind::OperationFinished {
                        outcome: cocoa_core::capability::OperationOutcome::Failed { error },
                        ..
                    } => Some(error),
                    _ => None,
                })
                .collect();
            assert_eq!(retained.len(), 12);
            assert!(retained.into_iter().all(|error| error == &host_error));
            engine.shutdown().await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn direct_methods_are_exact_grants_and_nonreplaceable() -> Result<(), Box<dyn Error>> {
    let (mut generic, store) = engine(&[
        "print([typeof cocoa, typeof process, typeof require, typeof fetch, typeof state, Object.hasOwn(globalThis, 'state')])",
    ]).await?;
    run(&mut generic).await?;
    assert_eq!(
        outcomes(&store).await?,
        vec![CellOutcome::Completed {
            printed: vec![json!([
                "undefined",
                "undefined",
                "undefined",
                "undefined",
                "undefined",
                false
            ])]
        }]
    );
    generic.shutdown().await?;

    let host = Arc::new(CodingHost::new(&[
        "cocoa.read",
        "cocoa.run",
        "cocoa.custom",
        "cocoa.read.extra",
    ]));
    let (mut engine, store) = engine_with_host(&[
        "const original = cocoa; const read = cocoa.read; print([Object.keys(cocoa).sort(), Object.isFrozen(cocoa), Reflect.set(globalThis, 'cocoa', {}), Reflect.deleteProperty(globalThis, 'cocoa'), Reflect.set(cocoa, 'read', () => 0), Reflect.defineProperty(cocoa, 'run', {value: () => 0}), Reflect.set(cocoa, 'write', () => 0), cocoa === original, cocoa.read === read, typeof cocoa.write, typeof cocoa.custom, typeof cocoa.read.extra])",
        "try { await cocoa.write({}) } catch (error) { print(error instanceof TypeError) }; void 0",
        "try { await capabilities.call('cocoa.write', {}) } catch (error) { print(error.code) }",
    ], None, engine_limits(), host.clone()).await?;
    run(&mut engine).await?;
    let feedback = outcomes(&store).await?;
    assert_eq!(
        feedback[0],
        CellOutcome::Completed {
            printed: vec![json!([
                ["read", "run"],
                true,
                false,
                false,
                false,
                false,
                false,
                true,
                true,
                "undefined",
                "undefined",
                "undefined"
            ])]
        }
    );
    assert_eq!(
        feedback[1],
        CellOutcome::Completed {
            printed: vec![json!(true)]
        }
    );
    assert_eq!(
        feedback[2],
        CellOutcome::Failed {
            error: FeedbackError::Capability {
                method: "cocoa.write".into(),
                code: "invalid_arguments".into(),
                message: "not granted".into(),
                operation_effects: OperationEffects::None,
                truncated: false,
            },
            printed: vec![json!("invalid_arguments")]
        }
    );
    assert!(host.calls.lock().await.is_empty());
    engine.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn spawn_handles_keep_metadata_and_exact_wait_wires_across_cells()
-> Result<(), Box<dyn Error>> {
    let host = Arc::new(SpawnHost::new());
    let (mut engine, store) = engine_with_host(&[
        "var savedWait = cocoa.wait; var savedCancel = cocoa.cancel; var savedSpawn = cocoa.spawn; var pending = savedSpawn({name:'alpha',prompt:'inspect'}); print(pending instanceof Promise); var handle = await pending; print([Object.keys(cocoa).sort(), Object.isFrozen(cocoa), Object.isFrozen(handle), Object.getPrototypeOf(handle) === null, Reflect.ownKeys(handle), typeof handle.then, typeof handle.reference, typeof handle.toJSON]); print([Object.getOwnPropertyDescriptor(handle,'name').writable, Object.getOwnPropertyDescriptor(handle,'alias').configurable, Reflect.set(handle,'name','forged'), Reflect.set(handle,'then',()=>0), Reflect.deleteProperty(handle,'alias')]); print({...handle})",
        "print(await savedWait(handle)); print(await savedWait(handle,{timeout_ms:0})); print(await savedWait(handle,{timeout_ms:-0})); var second = await savedSpawn({name:'beta',prompt:'check',alias:'fast'}); print(await savedWait({first:handle,second},{timeout_ms:4294967295}))",
        "var aliases = Object.create(null); aliases.__proto__ = handle; aliases[''] = second; aliases.constructor = handle; aliases.then = second; Object.defineProperty(aliases,'hidden',{value:handle}); var keyed = await savedWait(aliases,{timeout_ms:7}); print([Object.hasOwn(keyed,'__proto__'), keyed.__proto__.name, keyed.hidden.name]); print(keyed)",
        "print(await savedWait({})); print(await savedWait(Object.create(null),{timeout_ms:0})); print(await savedCancel(handle)); var replay = await savedSpawn({name:'alpha',prompt:'inspect'}); print(await savedWait(replay,{})); print(await cocoa.read({path:'ordinary'})); print(await capabilities.call('echo',{nested:[null,true,3]}))",
    ], None, engine_limits(), host.clone()).await?;
    run(&mut engine).await?;
    let feedback = outcomes(&store).await?;
    assert_eq!(
        feedback[0],
        CellOutcome::Completed {
            printed: vec![
                json!(true),
                json!([
                    ["cancel", "read", "spawn", "wait"],
                    true,
                    true,
                    true,
                    ["name", "alias"],
                    "undefined",
                    "undefined",
                    "undefined"
                ]),
                json!([false, false, false, false, false]),
                json!({"name":"alpha","alias":"standard"})
            ]
        }
    );
    assert_eq!(
        feedback[1],
        CellOutcome::Completed {
            printed: vec![
                json!({"name":"alpha","state":"completed"}),
                json!({"name":"alpha","state":"pending"}),
                json!({"name":"alpha","state":"pending"}),
                json!({"first":{"name":"alpha","state":"completed"},"second":{"name":"beta","state":"completed"}}),
            ]
        }
    );
    assert_eq!(
        feedback[2],
        CellOutcome::Completed {
            printed: vec![
                json!([true, "alpha", "alpha"]),
                json!({"__proto__":{"name":"alpha","state":"completed"},"":{"name":"beta","state":"completed"},"constructor":{"name":"alpha","state":"completed"},"then":{"name":"beta","state":"completed"},"hidden":{"name":"alpha","state":"completed"}}),
            ]
        }
    );
    assert_eq!(
        feedback[3],
        CellOutcome::Completed {
            printed: vec![
                json!({}),
                json!({}),
                json!({"name":"alpha","state":"cancelled"}),
                json!({"name":"alpha","state":"completed"}),
                json!({"path":"ordinary"}),
                json!({"nested":[null,true,3]}),
            ]
        }
    );
    assert_eq!(
        host.calls.lock().await.as_slice(),
        &[
            (
                "cocoa.spawn".into(),
                json!({"name":"alpha","prompt":"inspect"})
            ),
            ("cocoa.wait".into(), json!({"handles":{"":"private:alpha"}})),
            (
                "cocoa.wait".into(),
                json!({"handles":{"":"private:alpha"},"timeout_ms":0})
            ),
            (
                "cocoa.wait".into(),
                json!({"handles":{"":"private:alpha"},"timeout_ms":0})
            ),
            (
                "cocoa.spawn".into(),
                json!({"name":"beta","prompt":"check","alias":"fast"})
            ),
            (
                "cocoa.wait".into(),
                json!({"handles":{"first":"private:alpha","second":"private:beta"},"timeout_ms":4294967295_u32})
            ),
            (
                "cocoa.wait".into(),
                json!({"handles":{"__proto__":"private:alpha","":"private:beta","constructor":"private:alpha","then":"private:beta","hidden":"private:alpha"},"timeout_ms":7})
            ),
            ("cocoa.wait".into(), json!({"handles":{}})),
            ("cocoa.wait".into(), json!({"handles":{},"timeout_ms":0})),
            ("cocoa.cancel".into(), json!({"reference":"private:alpha"})),
            (
                "cocoa.spawn".into(),
                json!({"name":"alpha","prompt":"inspect"})
            ),
            ("cocoa.wait".into(), json!({"handles":{"":"private:alpha"}})),
            ("cocoa.read".into(), json!({"path":"ordinary"})),
            ("echo".into(), json!({"nested":[null,true,3]})),
        ]
    );
    engine.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn spawn_handles_reject_forgery_getters_and_generic_management_before_dispatch()
-> Result<(), Box<dyn Error>> {
    let host = Arc::new(SpawnHost::new());
    let (mut engine, store) = engine_with_host(&[
        r#"var handle = await cocoa.spawn({name:'alpha',prompt:'inspect'});
var foreign = await capabilities.call('echo',{reference:'another-runtime:alpha',name:'alpha',alias:'standard'});
var invalid = [JSON.parse(JSON.stringify(handle)), {...handle}, Object.freeze({name:'alpha',alias:'standard'}), foreign, 'private:alpha', null, undefined, [handle], new Proxy(handle,{})];
var rejected = [];
for (var value of invalid) {
    for (var method of [cocoa.wait,cocoa.cancel]) {
        try { await method(value); rejected.push(false); } catch (error) { rejected.push(error instanceof TypeError); }
    }
}
print(rejected);
var getterRan = false;
var groups = [{get worker() { getterRan = true; return handle; }}, {[Symbol('worker')]:handle}, {worker:foreign}, new Date(), new Map(), ()=>handle];
Object.prototype.value = handle;
var groupRejected = [];
for (var group of groups) {
    try { await cocoa.wait(group); groupRejected.push(false); } catch (error) { groupRejected.push(error instanceof TypeError); }
}
delete Object.prototype.value;
print([groupRejected,getterRan]);
var optionsRejected = [];
for (var options of [{timeout_ms:-1},{timeout_ms:4294967296},{timeout_ms:0.5},{timeout_ms:Infinity},{timeout_ms:undefined},null,[],7,{unknown:1},{get timeout_ms(){getterRan=true;return 0;}},{[Symbol('timeout')]:0}]) {
    try { await cocoa.wait(handle,options); optionsRejected.push(false); } catch (error) { optionsRejected.push(error instanceof TypeError); }
}
print([optionsRejected,getterRan]);
var genericRejected = [];
for (var name of ['cocoa.spawn','cocoa.wait','cocoa.cancel',new String('cocoa.spawn'),['cocoa.spawn']]) {
    try { await capabilities.call(name,{reference:'private:alpha'}); genericRejected.push(false); } catch (error) { genericRejected.push(error instanceof TypeError); }
}
print(genericRejected);
var boundaryRejected = [];
for (var operation of [()=>cocoa.spawn({get name(){getterRan=true;return 'bad';},prompt:'inspect'}),()=>cocoa.spawn({name:'bad',prompt:handle}),()=>cocoa.read({nested:handle}),()=>capabilities.call('echo',{nested:[handle]})]) {
    try { await operation(); boundaryRejected.push(false); } catch (error) { boundaryRejected.push(error instanceof TypeError); }
}
print([boundaryRejected,getterRan]);"#,
        "print('before'); try { print(handle); } catch {}",
        "print('before'); try { print({nested:[handle]}); } catch {}",
        "print({name:handle.name,alias:handle.alias}); var getterRan = false; print({get nested(){getterRan=true;return handle;}})",
        "print(getterRan)",
    ], None, engine_limits(), host.clone()).await?;
    run(&mut engine).await?;
    let feedback = outcomes(&store).await?;
    assert_eq!(
        feedback[0],
        CellOutcome::Completed {
            printed: vec![
                json!(vec![true; 18]),
                json!([vec![true; 6], false]),
                json!([vec![true; 11], false]),
                json!(vec![true; 5]),
                json!([vec![true; 4], false]),
            ]
        }
    );
    for (index, path) in [(1, "$"), (2, "$.nested[0]")] {
        assert!(
            matches!(&feedback[index], CellOutcome::Failed { error: FeedbackError::InvalidOutput { message, .. }, printed } if message == &format!("opaque work handle at {path}") && printed == &[json!("before")]),
            "{:?}",
            feedback[index]
        );
    }
    assert!(
        matches!(&feedback[3], CellOutcome::Failed { error: FeedbackError::InvalidOutput { message, .. }, printed } if message == "accessor at $.nested" && printed == &[json!({"name":"alpha","alias":"standard"})])
    );
    assert_eq!(
        feedback[4],
        CellOutcome::Completed {
            printed: vec![json!(false)]
        }
    );
    assert_eq!(host.calls.lock().await.len(), 2);
    engine.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn spawn_receipts_are_branded_before_assimilation_and_capture_trusted_intrinsics()
-> Result<(), Box<dyn Error>> {
    let host = Arc::new(SpawnHost::new());
    let (mut engine, store) = engine_with_host(&[
        "var leaked = false; Object.defineProperty(Object.prototype,'then',{get(){if(Object.hasOwn(this,'reference')) leaked=true; return undefined;},configurable:true}); var handle = await cocoa.spawn({name:'alpha',prompt:'inspect'}); delete Object.prototype.then; print([leaked,handle.name,typeof handle.then]); var savedWait = cocoa.wait; var savedSpawn = cocoa.spawn; var savedCancel = cocoa.cancel",
        "Object.create = Object.freeze = Object.getPrototypeOf = Object.getOwnPropertyDescriptor = Object.prototype.hasOwnProperty = Reflect.ownKeys = Reflect.apply = WeakMap.prototype.has = WeakMap.prototype.get = WeakMap.prototype.set = Number.isInteger = () => {throw Error('patched')}; TypeError = () => {throw Error('patched')}",
        "var replay = await savedSpawn({name:'alpha',prompt:'inspect'}); print(await savedWait(handle,{timeout_ms:0})); print(await savedWait({replay})); print(await savedCancel(replay)); print({...replay})",
    ], None, engine_limits(), host.clone()).await?;
    run(&mut engine).await?;
    assert_eq!(
        outcomes(&store).await?,
        vec![
            CellOutcome::Completed {
                printed: vec![json!([false, "alpha", "undefined"])]
            },
            CellOutcome::Completed { printed: vec![] },
            CellOutcome::Completed {
                printed: vec![
                    json!({"name":"alpha","state":"pending"}),
                    json!({"replay":{"name":"alpha","state":"completed"}}),
                    json!({"name":"alpha","state":"cancelled"}),
                    json!({"name":"alpha","alias":"standard"}),
                ]
            },
        ]
    );
    assert_eq!(host.calls.lock().await.len(), 5);
    engine.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn spawn_saved_waits_obey_current_turn_grants() -> Result<(), Box<dyn Error>> {
    let host = Arc::new(SpawnHost::new());
    let (mut engine, store) = engine_with_host(&[
        "var handle = handle ?? await cocoa.spawn({name:'alpha',prompt:'inspect'}); var savedWait = savedWait ?? cocoa.wait; print(handle.name)",
        "try { print(await savedWait(handle)); } catch (error) { print(error.code); }",
    ], None, engine_limits(), host.clone()).await?;
    run(&mut engine).await?;
    host.wait_granted
        .store(false, std::sync::atomic::Ordering::SeqCst);
    run(&mut engine).await?;
    host.wait_granted
        .store(true, std::sync::atomic::Ordering::SeqCst);
    run(&mut engine).await?;
    let feedback = outcomes(&store).await?;
    for index in [0, 2, 4] {
        assert_eq!(
            feedback[index],
            CellOutcome::Completed {
                printed: vec![json!("alpha")]
            }
        );
    }
    for index in [1, 5] {
        assert_eq!(
            feedback[index],
            CellOutcome::Completed {
                printed: vec![json!({"name":"alpha","state":"completed"})]
            }
        );
    }
    assert_eq!(
        feedback[3],
        CellOutcome::Failed {
            error: FeedbackError::Capability {
                method: "cocoa.wait".into(),
                code: "invalid_arguments".into(),
                message: "wait grant revoked".into(),
                operation_effects: OperationEffects::None,
                truncated: false,
            },
            printed: vec![json!("invalid_arguments")]
        }
    );
    assert_eq!(host.calls.lock().await.len(), 4);
    engine.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn spawn_metadata_and_private_wires_keep_json_and_feedback_bounds()
-> Result<(), Box<dyn Error>> {
    struct ReceiptHost(Value);
    #[async_trait]
    impl CapabilityHost for ReceiptHost {
        fn descriptions(&self) -> Vec<CapabilityDescription> {
            CodingHost::new(&["cocoa.spawn", "cocoa.wait", "cocoa.cancel"]).descriptions()
        }

        async fn call(
            &self,
            _: &str,
            _: Value,
            _: WorkLimits,
            _: CancellationToken,
        ) -> Result<Value, CapabilityError> {
            Ok(self.0.clone())
        }
    }
    let (mut engine, store) = engine_with_host(
        &["var handle = await cocoa.spawn({name:'large',prompt:'inspect'}); print('before'); try { print({...handle}); } catch {}"],
        None,
        engine_limits(),
        Arc::new(ReceiptHost(json!({"reference":"private:large","name":"n".repeat(6000),"alias":"t".repeat(6000)}))),
    ).await?;
    run(&mut engine).await?;
    let feedback = outcomes(&store).await?;
    let CellOutcome::Completed { printed } = &feedback[0] else {
        panic!("{:?}", feedback[0]);
    };
    assert_eq!(printed.len(), 2);
    assert_eq!(printed[0], "before");
    assert_eq!(printed[1]["truncated"], true);
    assert!(serde_json::to_vec(printed)?.len() <= 8192);
    assert!(!serde_json::to_string(printed)?.contains("private:large"));
    engine.shutdown().await?;

    let (mut engine, store) = engine_with_host(
        &["var handle = await cocoa.spawn({name:'large',prompt:'inspect'}); var rejected=[]; for (var method of [cocoa.wait,cocoa.cancel]) { try { await method(handle); rejected.push(false); } catch (error) { rejected.push(error instanceof TypeError); } } print(rejected)"],
        None,
        engine_limits(),
        Arc::new(ReceiptHost(json!({"reference":"r".repeat(8190),"name":"large","alias":"standard"}))),
    ).await?;
    run(&mut engine).await?;
    assert_eq!(
        outcomes(&store).await?,
        vec![CellOutcome::Completed {
            printed: vec![json!([true, true])]
        }]
    );
    assert_eq!(
        store
            .load()
            .await?
            .into_records()
            .iter()
            .filter(|record| matches!(record.kind, RecordKind::OperationStarted { .. }))
            .count(),
        1
    );
    engine.shutdown().await?;

    let host = Arc::new(SpawnHost::new());
    let (mut engine, store) = engine_with_host(&[
        "var handle = await cocoa.spawn({name:'alpha',prompt:'inspect'}); var huge = {}; for(var index=0;index<4097;index++) huge['worker'+index]=handle; var rejected=[]; for(var group of [{['a'.repeat(8192)]:handle},huge]) { try { await cocoa.wait(group); rejected.push(false); } catch(error) {rejected.push(error instanceof TypeError)} } print(rejected)",
    ], None, engine_limits(), host.clone()).await?;
    run(&mut engine).await?;
    assert_eq!(
        outcomes(&store).await?,
        vec![CellOutcome::Completed {
            printed: vec![json!([true, true])]
        }]
    );
    assert_eq!(host.calls.lock().await.len(), 1);
    engine.shutdown().await?;

    let (mut engine, store) = engine_with_host(
        &["try { await cocoa.spawn({name:'large',prompt:'inspect'}); } catch {}"],
        None,
        engine_limits(),
        Arc::new(ReceiptHost(
            json!({"reference":"private:large","name":"n".repeat(16384),"alias":"standard"}),
        )),
    )
    .await?;
    assert!(matches!(
        engine
            .run(
                RunRequest {
                    input: "metadata bound".into()
                },
                CancellationToken::new()
            )
            .await,
        Err(cocoa_core::engine::RunError::Execution(
            cocoa_core::execution::ExecutionError::Capability(CapabilityError::OutputLimit)
        ))
    ));
    assert_eq!(
        outcomes(&store).await?,
        vec![CellOutcome::Interrupted { printed: vec![] }]
    );
    engine.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn direct_async_calls_keep_source_strings_and_retained_functions_on_current_broker()
-> Result<(), Box<dyn Error>> {
    let host = Arc::new(CodingHost::new(&[
        "cocoa.read",
        "cocoa.write",
        "cocoa.edit",
        "cocoa.run",
        "cocoa.wait",
    ]));
    let (mut engine, store) = engine_with_host(&[
        "const prefix = 'src/'; var saved = async path => { print('read', path); return await cocoa.read({path: prefix + path}); }; var observation = await saved('generated.rs'); var source = `pub fn answer() -> &'static str {\n    \"red\\tblue\"\n}\n`; await cocoa.write({file: observation.file, content: source}); await cocoa.edit({file: 'updated', edits: [{old_text: 'red', new_text: 'green'}]}); var build = await cocoa.run({command: 'cargo test'}); const tests = await cocoa.wait(build); print({status: tests.status, inspected: observation.page.text})",
        "observation = await saved('second.rs'); print(observation.path)",
        "var read = cocoa.read; var late; read({path: 'third.rs'}).then(value => { late = value.path; print('settled', value.path); }); 23",
        "print(late)",
        "await cocoa.write({file:'updated', content:'survives'}); throw Error('later failure')",
    ], None, engine_limits(), host.clone()).await?;
    run(&mut engine).await?;
    let feedback = outcomes(&store).await?;
    let CellOutcome::Completed { printed } = &feedback[0] else {
        panic!("unexpected feedback: {:?}", feedback[0]);
    };
    assert_eq!(printed.len(), 2);
    assert_eq!(printed[0], json!(["read", "generated.rs"]));
    assert_eq!(
        printed[1],
        json!({"status":{"kind":"exited","code":7},"inspected":"old source\n"})
    );
    assert_eq!(
        feedback[1],
        CellOutcome::Completed {
            printed: vec![json!(["read", "second.rs"]), json!("src/second.rs")]
        }
    );
    assert_eq!(
        feedback[2],
        CellOutcome::Completed {
            printed: vec![json!(["settled", "third.rs"])]
        }
    );
    assert_eq!(
        feedback[3],
        CellOutcome::Completed {
            printed: vec![json!("third.rs")]
        }
    );
    assert!(
        matches!(&feedback[4], CellOutcome::Failed { error: FeedbackError::Javascript { message, .. }, .. } if message.contains("later failure"))
    );
    let calls = host.calls.lock().await;
    assert_eq!(
        calls[1],
        (
            "cocoa.write".into(),
            json!({"file":"observed","content":"pub fn answer() -> &'static str {\n    \"red\tblue\"\n}\n"})
        )
    );
    assert_eq!(
        calls.last(),
        Some(&(
            "cocoa.write".into(),
            json!({"file":"updated","content":"survives"})
        ))
    );
    assert_eq!(calls.len(), 8);
    drop(calls);
    let records = store.load().await?.into_records();
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record.kind, RecordKind::OperationStarted { .. }))
            .count(),
        8
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record.kind, RecordKind::OperationFinished { .. }))
            .count(),
        8
    );
    engine.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn sequential_calls_preserve_order_errors_snapshots_and_operation_evidence()
-> Result<(), Box<dyn Error>> {
    let host = Arc::new(CodingHost::new(&[
        "cocoa.read",
        "cocoa.write",
        "cocoa.edit",
        "cocoa.run",
    ]));
    let (mut engine, store) = engine_with_host(&[
        "var savedRead = cocoa.read; var input = {path:'first'}; var pendingRead = savedRead(input); input.path = 'changed'; var results = [(await pendingRead).path]; try { await savedRead({fail:'partial'}); } catch (error) { results.push(error.effects); } results.push((await savedRead({path:'last'})).path); print(results)",
        "for (var method of ['write','edit','run']) { var values = []; var failure; for (var input of [{file:'one',command:'first'}, {fail:'unknown'}, {file:'three',command:'last'}]) { try { var value = await cocoa[method](input); values.push(method === 'run' ? value.kind : value.file); } catch (error) { failure = {method:error.method,code:error.code,message:error.message,effects:error.effects}; } } print({method,error:failure,values}); }",
        "print((await savedRead({path:'later'})).path)",
    ], None, engine_limits(), host.clone()).await?;
    run(&mut engine).await?;
    let feedback = outcomes(&store).await?;
    let CellOutcome::Failed { error, printed } = &feedback[0] else {
        panic!("unexpected sequential feedback: {:?}", feedback[0]);
    };
    assert_eq!(
        error,
        &FeedbackError::Capability {
            method: "cocoa.read".into(),
            code: "stale_read".into(),
            message: "Read again before editing".into(),
            operation_effects: OperationEffects::Partial,
            truncated: false,
        }
    );
    assert_eq!(printed, &[json!(["first", "partial", "last"])]);
    let CellOutcome::Failed { error, printed } = &feedback[1] else {
        panic!("unexpected method feedback: {:?}", feedback[1]);
    };
    assert!(
        matches!(error, FeedbackError::Capability { message, operation_effects: OperationEffects::Unknown, truncated:false, .. } if message.contains("3 recoverable host failures") && message.contains("cocoa.edit") && message.contains("cocoa.run"))
    );
    for (index, method) in ["write", "edit", "run"].into_iter().enumerate() {
        assert_eq!(printed[index]["method"], method);
        assert_eq!(
            printed[index]["error"],
            json!({"method":format!("cocoa.{method}"),"code":"stale_read","message":"Read again before editing","effects":"unknown"})
        );
        assert_eq!(
            printed[index]["values"],
            if method == "run" {
                json!(["process", "process"])
            } else {
                json!(["updated", "updated"])
            }
        );
    }
    assert_eq!(
        feedback[2],
        CellOutcome::Completed {
            printed: vec![json!("later")]
        }
    );
    let calls = host.calls.lock().await;
    assert_eq!(calls.len(), 9);
    assert_eq!(calls[0].1["path"], "first");
    assert_eq!(calls[1].1["path"], "last");
    assert_eq!(calls[8].1["path"], "later");
    drop(calls);
    let records = store.load().await?.into_records();
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record.kind, RecordKind::OperationStarted { .. }))
            .count(),
        13
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record.kind, RecordKind::OperationFinished { .. }))
            .count(),
        13
    );
    engine.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn single_calls_reject_bad_structure_and_whole_encoded_size_before_dispatch()
-> Result<(), Box<dyn Error>> {
    let host = Arc::new(CodingHost::new(&["cocoa.read"]));
    let (mut engine, store) = engine_with_host(&[
        "var getterRan = false; var cyclic = {}; cyclic.self = cyclic; for (var args of [{path:'x',bad:undefined}, cyclic, {get path() { getterRan = true; return 'x'; }}, {values:[{path:'x'}, , {path:'y'}]}, {path:'x',text:'a'.repeat(8192)}, {path:'x',numbers:Array(700).fill(1.2345678901234567e+200)}]) { try { await cocoa.read(args); print('incorrectly admitted'); } catch (error) { print(error instanceof TypeError); } } print(getterRan)",
        "var paths=[]; for (var index=0; index<16; index++) paths.push((await cocoa.read({path:'file-'+index})).path); print(paths)",
    ], None, engine_limits(), host.clone()).await?;
    run(&mut engine).await?;
    assert_eq!(
        outcomes(&store).await?[0],
        CellOutcome::Completed {
            printed: vec![
                json!(true),
                json!(true),
                json!(true),
                json!(true),
                json!(true),
                json!(true),
                json!(false)
            ]
        }
    );
    assert_eq!(
        outcomes(&store).await?[1],
        CellOutcome::Completed {
            printed: vec![json!(
                (0..16)
                    .map(|index| format!("file-{index}"))
                    .collect::<Vec<_>>()
            )]
        }
    );
    assert_eq!(host.calls.lock().await.len(), 16);
    assert_eq!(
        store
            .load()
            .await?
            .into_records()
            .iter()
            .filter(|record| matches!(record.kind, RecordKind::OperationStarted { .. }))
            .count(),
        16
    );
    engine.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn single_call_shape_rejection_is_recorded_and_preserves_fatal_errors()
-> Result<(), Box<dyn Error>> {
    struct ValidatingHost(CodingHost);
    #[async_trait]
    impl CapabilityHost for ValidatingHost {
        fn descriptions(&self) -> Vec<CapabilityDescription> {
            self.0.descriptions()
        }
        async fn call(
            &self,
            name: &str,
            arguments: Value,
            limits: WorkLimits,
            cancellation: CancellationToken,
        ) -> Result<Value, CapabilityError> {
            if arguments.get("fatal").is_some() {
                return Err(CapabilityError::Budget);
            }
            if !arguments.get("path").is_some_and(Value::is_string) {
                return Err(CapabilityError::Tool {
                    code: "invalid_arguments".into(),
                    message: "path must be a string".into(),
                    effects: cocoa_core::capability::Effects::None,
                });
            }
            self.0.call(name, arguments, limits, cancellation).await
        }
    }
    let host = Arc::new(ValidatingHost(CodingHost::new(&["cocoa.read"])));
    let limits = engine_limits();
    let (mut engine, store) = engine_with_host(
        &[
            "try { await cocoa.read({path:null}); } catch {} print('caught');",
            "print((await cocoa.read({path:'allowed'})).path);",
        ],
        None,
        limits,
        host.clone(),
    )
    .await?;
    run(&mut engine).await?;
    assert_eq!(
        outcomes(&store).await?,
        vec![
            CellOutcome::Failed {
                error: FeedbackError::Capability {
                    method: "cocoa.read".into(),
                    code: "invalid_arguments".into(),
                    message: "path must be a string".into(),
                    operation_effects: OperationEffects::None,
                    truncated: false,
                },
                printed: vec![json!("caught")]
            },
            CellOutcome::Completed {
                printed: vec![json!("allowed")]
            },
        ]
    );
    assert_eq!(
        host.0.calls.lock().await.as_slice(),
        &[("cocoa.read".into(), json!({"path":"allowed"}))]
    );
    assert_eq!(
        store
            .load()
            .await?
            .into_records()
            .iter()
            .filter(|record| matches!(record.kind, RecordKind::OperationStarted { .. }))
            .count(),
        2
    );
    engine.shutdown().await?;
    let (mut engine, store) = engine_with_host(
        &["try { await cocoa.read({fatal:true}); } catch {} print('cannot hide fatal');"],
        None,
        engine_limits(),
        host.clone(),
    )
    .await?;
    assert!(matches!(
        engine
            .run(
                RunRequest {
                    input: "fatal".into()
                },
                CancellationToken::new()
            )
            .await,
        Err(cocoa_core::engine::RunError::Execution(
            cocoa_core::execution::ExecutionError::Capability(CapabilityError::Budget)
        ))
    ));
    assert_eq!(
        outcomes(&store).await?,
        vec![CellOutcome::Interrupted { printed: vec![] }]
    );
    assert_eq!(host.0.calls.lock().await.len(), 1);
    engine.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn caught_single_failures_remain_visible_with_bounded_feedback() -> Result<(), Box<dyn Error>>
{
    for output_bytes in [128, 512, 1024, 16384] {
        let limits = engine_limits();
        let cells = [
            "try { await cocoa.read({fail:'partial'}); } catch {} print('kept');",
            "for (var i=0;i<16;i++) { try { await cocoa.read({fail:'unknown'}); } catch {} } print('placeholder');",
            "try { await cocoa.read({fail:'none'}); } catch {} print('kept'); try { print('x'.repeat(9000)); } catch {}",
        ];
        let store = Arc::new(InMemoryStore::new(StoreLimits {
            record_bytes: limits.record_bytes,
        })?);
        let executor = QuickJsExecutor::new(QuickJsOptions {
            memory_bytes: 8 * 1024 * 1024,
            stack_bytes: 256 * 1024,
            feedback: cocoa_quickjs::FeedbackLimits {
                output_bytes,
                error_bytes: (output_bytes / 4).clamp(96, 2048),
                ..feedback_limits()
            },
        })
        .await?;
        let mut engine = Engine::new(
            EngineSettings::new(limits)?,
            EngineAdapters {
                model: Arc::new(Cells(cells.iter().map(|code| (*code).into()).collect())),
                executor: Box::new(executor),
                capabilities: Arc::new(CodingHost::new(&["cocoa.read"])),
                context: Arc::new(BoundedContext),
                store: store.clone(),
            },
            None,
        );
        run(&mut engine).await?;
        let feedback = outcomes(&store).await?;
        for (index, outcome) in feedback.iter().enumerate() {
            assert!(serde_json::to_vec(outcome)?.len() <= output_bytes);
            let CellOutcome::Failed { error, printed } = outcome else {
                panic!("hidden failure: {outcome:?}");
            };
            if output_bytes == 128 {
                assert_eq!(printed, &[json!({"truncated":true})]);
            } else if index == 2 {
                assert_eq!(printed.len(), 2);
                assert_eq!(printed[0], "kept");
                assert_eq!(printed[1]["truncated"], true);
            } else {
                assert_eq!(
                    printed,
                    &[json!(if index == 1 { "placeholder" } else { "kept" })]
                );
            }
            if output_bytes == 128 {
                assert!(
                    matches!(error, FeedbackError::Javascript {message,truncated:true} if message.contains("Host failure") || message.contains("recoverable") || message.contains("failures")),
                    "{error:?}"
                );
            } else {
                assert!(
                    matches!(error, FeedbackError::Capability {code,..} if code == "stale_read")
                );
            }
            if index == 1 {
                assert!(
                    matches!(error, FeedbackError::Capability {message,truncated:true,..} | FeedbackError::Javascript {message,truncated:true} if message.split_whitespace().next() == Some("16")),
                    "{error:?}"
                );
            }
            if index == 2 && output_bytes == 16384 {
                assert!(
                    !error
                        .to_string()
                        .contains("feedback limit reached (encoded byte ceiling 8192)")
                );
            }
        }
        engine.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn sequential_calls_stop_on_fatal_failures() -> Result<(), Box<dyn Error>> {
    use cocoa_core::{engine::RunError, execution::ExecutionError};
    for fault in ["budget", "cancelled", "deadline", "output_limit"] {
        let host = Arc::new(CodingHost::new(&["cocoa.run"]));
        let code = format!(
            "try {{ for (var input of [{{command:'first'}},{{fault:'{fault}'}},{{command:'last'}}]) await cocoa.run(input); }} catch {{}} print('not success')"
        );
        let (mut engine, store) =
            engine_with_host(&[&code], None, engine_limits(), host.clone()).await?;
        let result = engine
            .run(
                RunRequest {
                    input: "fatal sequential calls".into(),
                },
                CancellationToken::new(),
            )
            .await;
        match fault {
            "cancelled" => assert!(matches!(result, Err(RunError::Cancelled))),
            "deadline" => assert!(matches!(
                result,
                Err(RunError::Deadline {
                    cause: cocoa_core::store::FailureCause::Capability
                })
            )),
            "budget" => assert!(matches!(
                result,
                Err(RunError::Execution(ExecutionError::Capability(
                    CapabilityError::Budget
                )))
            )),
            _ => assert!(matches!(
                result,
                Err(RunError::Execution(ExecutionError::Capability(
                    CapabilityError::OutputLimit
                )))
            )),
        }
        assert_eq!(
            outcomes(&store).await?,
            vec![CellOutcome::Interrupted { printed: vec![] }]
        );
        assert_eq!(host.calls.lock().await.len(), 1);
        assert_eq!(
            store
                .load()
                .await?
                .into_records()
                .iter()
                .filter(|record| matches!(record.kind, RecordKind::OperationStarted { .. }))
                .count(),
            2
        );
        engine.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn direct_rejections_preserve_structured_recoverable_errors_and_strict_json()
-> Result<(), Box<dyn Error>> {
    let host = Arc::new(CodingHost::new(&["cocoa.edit"]));
    let (mut engine, store) = engine_with_host(&[
        "const results = []; for (const effects of ['none', 'partial', 'unknown']) { try { await cocoa.edit({fail:effects}); } catch (error) { results.push({code:error.code, effects:error.effects, message:error.message, error: error instanceof Error}); } } print(results)",
        "var saved_error; try { await cocoa.edit({fail:'partial'}) } catch (error) { saved_error = error; print(error.code, error.effects); throw error; }",
        "try { const pending = cocoa.edit({content: undefined}); print(pending instanceof Promise); await pending; } catch (error) { print(error instanceof TypeError); }; void 0",
        "cocoa.edit({fail:'unknown'}); 17",
        "throw saved_error",
    ], None, engine_limits(), host.clone()).await?;
    run(&mut engine).await?;
    let feedback = outcomes(&store).await?;
    assert_eq!(
        feedback[0],
        CellOutcome::Failed {
            error: FeedbackError::Capability {
                method:"cocoa.edit".into(), code:"stale_read".into(),
                message:"3 recoverable host failures; earlier effects remain. First: Read again before editing; cocoa.edit: stale_read: Read again before editing (operation effects Partial); cocoa.edit: stale_read: Read again before editing (operation effects Unknown)".into(),
                operation_effects:OperationEffects::None, truncated:false,
            },
            printed: vec![json!([
                {"code":"stale_read","effects":"none","message":"Read again before editing","error":true},
                {"code":"stale_read","effects":"partial","message":"Read again before editing","error":true},
                {"code":"stale_read","effects":"unknown","message":"Read again before editing","error":true}
            ])]
        }
    );
    assert_eq!(
        feedback[1],
        CellOutcome::Failed {
            error: FeedbackError::Capability {
                method: "cocoa.edit".into(),
                code: "stale_read".into(),
                message: "Read again before editing".into(),
                operation_effects: OperationEffects::Partial,
                truncated: false,
            },
            printed: vec![json!(["stale_read", "partial"])]
        }
    );
    assert_eq!(
        feedback[2],
        CellOutcome::Completed {
            printed: vec![json!(true), json!(true)]
        }
    );
    assert!(matches!(
        &feedback[3],
        CellOutcome::Failed {
            error: FeedbackError::Capability {
                operation_effects: OperationEffects::Unknown,
                ..
            },
            ..
        }
    ));
    assert_eq!(
        feedback[4],
        CellOutcome::Failed {
            error: FeedbackError::Capability {
                method: "cocoa.edit".into(),
                code: "stale_read".into(),
                message: "Read again before editing".into(),
                operation_effects: OperationEffects::Partial,
                truncated: false,
            },
            printed: Vec::new()
        }
    );
    assert!(host.calls.lock().await.is_empty());
    assert_eq!(
        store
            .load()
            .await?
            .into_records()
            .iter()
            .filter(|record| matches!(record.kind, RecordKind::OperationStarted { .. }))
            .count(),
        5
    );
    engine.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn direct_calls_cannot_swallow_fatal_host_errors() -> Result<(), Box<dyn Error>> {
    use cocoa_core::{engine::RunError, execution::ExecutionError};
    for fault in ["budget", "cancelled", "deadline", "output_limit"] {
        let host = Arc::new(CodingHost::new(&["cocoa.run"]));
        let code = format!("try {{ await cocoa.run({{fault:'{fault}'}}); }} catch {{}} 23");
        let (mut engine, store) = engine_with_host(&[&code], None, engine_limits(), host).await?;
        let result = engine
            .run(
                RunRequest {
                    input: "fatal".into(),
                },
                CancellationToken::new(),
            )
            .await;
        match fault {
            "cancelled" => assert!(matches!(result, Err(RunError::Cancelled))),
            "deadline" => assert!(matches!(
                result,
                Err(RunError::Deadline {
                    cause: cocoa_core::store::FailureCause::Capability
                })
            )),
            "budget" => assert!(matches!(
                result,
                Err(RunError::Execution(ExecutionError::Capability(
                    CapabilityError::Budget
                )))
            )),
            _ => assert!(matches!(
                result,
                Err(RunError::Execution(ExecutionError::Capability(
                    CapabilityError::OutputLimit
                )))
            )),
        }
        assert_eq!(
            outcomes(&store).await?,
            vec![CellOutcome::Interrupted { printed: vec![] }]
        );
        engine.shutdown().await?;
    }
    let host = Arc::new(CodingHost::new(&["cocoa.run"]));
    let (mut engine, store) = engine_with_host(
        &["try { await cocoa.run({wait:true}) } catch {} 31"],
        None,
        engine_limits(),
        host,
    )
    .await?;
    let cancellation = CancellationToken::new();
    let cancel = cancellation.clone();
    let (result, ()) = tokio::join!(
        engine.run(
            RunRequest {
                input: "cancel".into()
            },
            cancellation
        ),
        async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            cancel.cancel();
        }
    );
    assert!(matches!(
        result,
        Err(RunError::TurnCancelled {
            runtime: cocoa_core::execution::RuntimeState::Reset
        })
    ));
    assert!(matches!(
        outcomes(&store).await?.as_slice(),
        [CellOutcome::Interrupted { .. }]
    ));
    engine.shutdown().await?;
    Ok(())
}

async fn engine(cells: &[&str]) -> Result<(Engine, Arc<InMemoryStore>), Box<dyn Error>> {
    engine_observed(cells, None, engine_limits()).await
}

fn engine_limits() -> Limits {
    Limits {
        input_bytes: 4096,
        output_bytes: 16384,
        record_bytes: 32768,
        context_bytes: 512_000,
        pending_operations: 4,
        stall_timeout: Duration::from_secs(2),
        settlement_timeout: Duration::from_millis(500),
        shutdown_timeout: Duration::from_millis(500),
    }
}

fn feedback_limits() -> cocoa_quickjs::FeedbackLimits {
    cocoa_quickjs::FeedbackLimits {
        output_bytes: 65536,
        printed_bytes: 8192,
        error_bytes: 2048,
        print_calls: 32,
    }
}

async fn engine_observed(
    cells: &[&str],
    events: Option<EventPublisher>,
    limits: Limits,
) -> Result<(Engine, Arc<InMemoryStore>), Box<dyn Error>> {
    engine_with_host(cells, events, limits, Arc::new(Host)).await
}

async fn engine_with_host(
    cells: &[&str],
    events: Option<EventPublisher>,
    limits: Limits,
    capabilities: Arc<dyn CapabilityHost>,
) -> Result<(Engine, Arc<InMemoryStore>), Box<dyn Error>> {
    let store = Arc::new(InMemoryStore::new(StoreLimits {
        record_bytes: limits.record_bytes,
    })?);
    let settings = EngineSettings::new(limits)?;
    Ok((
        Engine::new(
            settings,
            EngineAdapters {
                model: Arc::new(Cells(cells.iter().map(|code| (*code).into()).collect())),
                executor: Box::new(
                    QuickJsExecutor::new(QuickJsOptions {
                        memory_bytes: 8 * 1024 * 1024,
                        stack_bytes: 256 * 1024,
                        feedback: feedback_limits(),
                    })
                    .await?,
                ),
                capabilities,
                context: Arc::new(BoundedContext),
                store: store.clone(),
            },
            events,
        ),
        store,
    ))
}

async fn run(engine: &mut Engine) -> Result<(), Box<dyn Error>> {
    assert_eq!(
        engine
            .run(
                RunRequest {
                    input: "run".into()
                },
                CancellationToken::new()
            )
            .await?
            .answer,
        "settled"
    );
    Ok(())
}

async fn outcomes(store: &InMemoryStore) -> Result<Vec<CellOutcome>, Box<dyn Error>> {
    Ok(store
        .load()
        .await?
        .into_records()
        .into_iter()
        .filter_map(|record| match record.kind {
            RecordKind::CellCompleted { outcome, .. } => Some(outcome),
            _ => None,
        })
        .collect())
}

async fn wait_for_printed_readiness(
    receiver: &mut cocoa_core::engine::EventReceiver,
) -> Result<(), Box<dyn Error>> {
    use cocoa_core::engine::{EventKind, Observation};

    for _ in 0..32 {
        match receiver.recv().await {
            Observation::Event(event) => {
                if matches!(event.kind, EventKind::Printed { text, truncated: false } if text == "ready")
                {
                    return Ok(());
                }
            }
            observation => {
                return Err(format!("readiness observation lost: {observation:?}").into());
            }
        }
    }
    Err("JavaScript did not print readiness within 32 observations".into())
}

#[tokio::test]
async fn construction_validates_bounds_and_shutdown_can_be_retried() -> Result<(), Box<dyn Error>> {
    for (memory_bytes, stack_bytes) in [
        (1024 * 1024 - 1, 64 * 1024),
        (1024 * 1024, 64 * 1024 - 1),
        (1024 * 1024, 1024 * 1024 + 1),
    ] {
        assert!(matches!(
            QuickJsExecutor::new(QuickJsOptions {
                memory_bytes,
                stack_bytes,
                feedback: feedback_limits(),
            })
            .await,
            Err(QuickJsError::InvalidOptions)
        ));
    }
    let mut executor = QuickJsExecutor::new(QuickJsOptions {
        memory_bytes: 1024 * 1024,
        stack_bytes: 64 * 1024,
        feedback: feedback_limits(),
    })
    .await?;
    {
        let shutdown = executor.shutdown(CancellationToken::new());
        tokio::pin!(shutdown);
        if let std::task::Poll::Ready(result) = futures_util::poll!(&mut shutdown) {
            assert_eq!(result?.unresolved_work, 0);
        }
    }
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    match executor.shutdown(cancellation).await {
        Ok(report) => assert_eq!(report.unresolved_work, 0),
        Err(error) => assert!(matches!(
            error,
            cocoa_core::execution::ExecutionError::Unavailable { .. }
        )),
    }
    assert_eq!(
        tokio::time::timeout(
            Duration::from_secs(1),
            executor.shutdown(CancellationToken::new())
        )
        .await??
        .unresolved_work,
        0
    );
    assert_eq!(
        executor
            .shutdown(CancellationToken::new())
            .await?
            .unresolved_work,
        0
    );
    Ok(())
}

#[tokio::test]
async fn ordinary_expression_completion_is_silent() -> Result<(), Box<dyn Error>> {
    let (mut engine, store) = engine(&["7 * 3 + 2"]).await?;
    run(&mut engine).await?;
    assert_eq!(
        outcomes(&store).await?,
        vec![CellOutcome::Completed {
            printed: Vec::new()
        }]
    );
    engine.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn print_only_wire_contract() -> Result<(), Box<dyn Error>> {
    let (mut engine, store) = engine(&[
        "({silent: true})",
        "(() => 17)",
        "const value = {count: 3}; print(value); value.count = 9; print('label', 7); print()",
    ])
    .await?;
    run(&mut engine).await?;
    let feedback = outcomes(&store).await?;
    assert_eq!(
        serde_json::to_value(&feedback[0])?,
        json!({"status":"completed"})
    );
    assert_eq!(
        serde_json::to_value(&feedback[1])?,
        json!({"status":"completed"})
    );
    assert_eq!(
        serde_json::to_value(&feedback[2])?,
        json!({"status":"completed","printed":[{"count":3},["label",7]]})
    );
    engine.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn silent_completions_still_await_work_and_rejections() -> Result<(), Box<dyn Error>> {
    let (mut engine, store) = engine(&[
        "var hooks = 0; ({get x() { hooks++; throw Error('not output') }})",
        "Symbol('silent')",
        "37n",
        "Promise.resolve(() => 7)",
        "var total; capabilities.call('multiply', {left:7,right:3}).then(value => {total = value}); (() => 0)",
        "print({hooks,total})",
        "Promise.reject(Error('final rejection'))",
    ]).await?;
    run(&mut engine).await?;
    let feedback = outcomes(&store).await?;
    for outcome in &feedback[..5] {
        assert_eq!(
            serde_json::to_value(outcome)?,
            json!({"status":"completed"})
        );
    }
    assert_eq!(
        serde_json::to_value(&feedback[5])?,
        json!({"status":"completed","printed":[{"hooks":0,"total":21}]})
    );
    assert!(
        matches!(&feedback[6], CellOutcome::Failed { error: FeedbackError::Javascript { message, .. }, .. } if message.contains("final rejection"))
    );
    engine.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn syntax_diagnostics_locate_the_cell_without_admitting_host_operations()
-> Result<(), Box<dyn Error>> {
    let (mut engine, store) = engine(&[
        "var previous = 19; var NativeSyntaxError = SyntaxError; SyntaxError = Error;",
        "previous = 0; await capabilities.call('multiply',{left:7,right:3});\nvar valid = 1;\nconst broken = ;",
        "print([previous, typeof valid]);",
        "previous = 0; await capabilities.call('multiply',{left:7,right:3});\nvar valid = 1;\nvar corrected = 23; print(corrected);",
        "previous = 31; await capabilities.call('multiply',{left:5,right:11}); throw new NativeSyntaxError('runtime SyntaxError');",
        "previous = 43; await capabilities.call('multiply',{left:3,right:13}); eval('const broken = ;');",
        "print([previous, valid, corrected]);",
        &format!("var tooDeep = {}0{};", "(".repeat(5000), ")".repeat(5000)),
    ])
    .await?;
    run(&mut engine).await?;
    let feedback = outcomes(&store).await?;
    assert!(
        matches!(&feedback[1],CellOutcome::Failed {error:FeedbackError::Javascript {message,truncated:false},printed} if message.starts_with("Cell not executed: JavaScript parsing failed. Declarations, assignments and host operations in this cell did not run. Correct and resubmit the complete step.") && message.contains("cell line 3") && message.contains("const broken = ;") && printed.is_empty()),
        "{feedback:?}"
    );
    assert_eq!(
        feedback[2],
        CellOutcome::Completed {
            printed: vec![json!([19, "undefined"])]
        }
    );
    assert_eq!(
        feedback[3],
        CellOutcome::Completed {
            printed: vec![json!(23)]
        }
    );
    for outcome in &feedback[4..6] {
        assert!(
            matches!(outcome, CellOutcome::Failed {error:FeedbackError::Javascript {message,..},..} if message.contains("SyntaxError") && !message.contains("Cell not executed")),
            "{outcome:?}"
        );
    }
    assert_eq!(
        feedback[6],
        CellOutcome::Completed {
            printed: vec![json!([43, 1, 23])]
        }
    );
    assert!(
        matches!(&feedback[7], CellOutcome::Failed {error: FeedbackError::Javascript {message, ..}, ..} if message.contains("stack") && !message.contains("Cell not executed")),
        "{:?}",
        feedback[7]
    );
    let records = store.load().await?.into_records();
    let first_operation = records
        .iter()
        .position(|record| matches!(record.kind, RecordKind::OperationStarted { .. }))
        .ok_or("missing corrected operation")?;
    assert_eq!(
        records[..first_operation]
            .iter()
            .filter(|record| matches!(record.kind, RecordKind::CellCompleted { .. }))
            .count(),
        3
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record.kind, RecordKind::OperationStarted { .. }))
            .count(),
        3
    );
    engine.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn printed_values_never_invoke_getters_or_json_hooks() -> Result<(), Box<dyn Error>> {
    let (mut engine, store) = engine(&[
        "var hooks = 0; print('accepted'); print({get value() { hooks++; return 7 }})",
        "print({toJSON() {hooks++; return 7}})",
        "print(hooks)",
        "print({changedLineAfter:undefined})",
        "Object.prototype.toJSON = () => {hooks++; return 'wrong'}; print({nested:[17,'é']})",
        "print(hooks)",
    ])
    .await?;
    run(&mut engine).await?;
    let feedback = outcomes(&store).await?;
    assert!(
        matches!(&feedback[0], CellOutcome::Failed { error: FeedbackError::InvalidOutput { message, .. }, printed } if message.contains("accessor at $.value") && printed == &[json!("accepted")])
    );
    assert!(
        matches!(&feedback[1], CellOutcome::Failed { error: FeedbackError::InvalidOutput { .. }, printed } if printed.is_empty())
    );
    assert_eq!(
        serde_json::to_value(&feedback[2])?,
        json!({"status":"completed","printed":[0]})
    );
    assert!(
        matches!(&feedback[3],CellOutcome::Failed {error:FeedbackError::InvalidOutput {message,..},..} if message.contains("undefined at $.changedLineAfter"))
    );
    assert_eq!(
        serde_json::to_value(&feedback[4])?,
        json!({"status":"completed","printed":[{"nested":[17,"é"]}]})
    );
    assert_eq!(
        serde_json::to_value(&feedback[5])?,
        json!({"status":"completed","printed":[0]})
    );
    engine.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn encoded_print_boundaries_reserve_a_whole_bounded_error() -> Result<(), Box<dyn Error>> {
    for (output_bytes, error_bytes, content_bytes) in [
        (128, 64, 2),
        (512, 128, 322),
        (8192, 2048, 6082),
        (16384, 2048, 8169),
    ] {
        let mut text = "é".repeat(content_bytes / 2);
        if content_bytes % 2 != 0 {
            text.push('x');
        }
        assert_eq!(text.len(), content_bytes);
        let prepare = format!("var text={}", serde_json::to_string(&text)?);
        let (mut engine, store) = engine_observed(
            &[
                &prepare,
                "print(text);throw 'z'.repeat(1e4)",
                "print(text+'x')",
                "print(text); print('x'); throw 'z'.repeat(1e4)",
            ],
            None,
            Limits {
                output_bytes,
                ..engine_limits()
            },
        )
        .await?;
        run(&mut engine).await?;
        let feedback = outcomes(&store).await?;
        let CellOutcome::Failed { error, printed } = &feedback[1] else {
            panic!("{:?}", feedback[1]);
        };
        assert_eq!(printed, &[json!(text)]);
        assert!(matches!(
            error,
            FeedbackError::Javascript {
                truncated: true,
                ..
            }
        ));
        assert_eq!(serde_json::to_vec(error)?.len(), error_bytes);
        assert_eq!(serde_json::to_vec(printed)?.len(), content_bytes + 4);
        assert_eq!(
            serde_json::to_vec(&feedback[1])?.len(),
            content_bytes + error_bytes + 43
        );
        assert!(serde_json::to_vec(&feedback[1])?.len() <= output_bytes);
        assert_eq!(serde_json::to_value(error)?["kind"], "javascript");
        let CellOutcome::Completed { printed } = &feedback[2] else {
            panic!("{:?}", feedback[2]);
        };
        assert_eq!(printed.len(), 1);
        assert_eq!(printed[0]["truncated"], true);
        assert!(serde_json::to_vec(printed)?.len() <= content_bytes + 23);
        assert_eq!(
            feedback[3],
            CellOutcome::Failed {
                error: error.clone(),
                printed: vec![json!(text), json!({"truncated":true})]
            }
        );
        assert!(serde_json::to_vec(&feedback[3])?.len() <= output_bytes);
        engine.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn print_overflow_continues_with_retained_values() -> Result<(), Box<dyn Error>> {
    let (mut engine, store) = engine(&[
        "var retained = {text:'é\\n\"'.repeat(5000)}; var after_print = 0; print('kept'); print(retained); after_print++; print('omitted')",
        "print({length:retained.text.length,tail:retained.text.slice(-3),after_print})",
        "var huge = 'x'.repeat(300000); print(huge); after_print++",
        "print({length:huge.length,after_print})",
        "print('x'.repeat(100000)); throw Error('actual failure')",
        "print('x'.repeat(100000)); print({get value() {after_print++; return 7}})",
        "print(after_print)",
    ]).await?;
    run(&mut engine).await?;
    let feedback = outcomes(&store).await?;
    let CellOutcome::Completed { printed } = &feedback[0] else {
        panic!("{:?}", feedback[0]);
    };
    assert_eq!(printed.len(), 2);
    assert_eq!(printed[0], "kept");
    assert_eq!(printed[1]["truncated"], true);
    let prefix = printed[1]["json_prefix"]
        .as_str()
        .ok_or("missing preview")?;
    let original = serde_json::to_string(&json!({"text":"é\n\"".repeat(5000)}))?;
    assert!(!prefix.is_empty() && original.starts_with(prefix) && prefix.len() < original.len());
    assert!(serde_json::to_vec(printed)?.len() <= 8192);
    assert_eq!(
        feedback[1],
        CellOutcome::Completed {
            printed: vec![json!({"length":15000,"tail":"é\n\"","after_print":1})]
        }
    );
    assert_eq!(
        feedback[2],
        CellOutcome::Completed {
            printed: vec![json!({"truncated":true})]
        }
    );
    assert_eq!(
        feedback[3],
        CellOutcome::Completed {
            printed: vec![json!({"length":300000,"after_print":2})]
        }
    );
    assert!(
        matches!(&feedback[4], CellOutcome::Failed {error:FeedbackError::Javascript {message,..},printed} if message.contains("actual failure") && printed[0]["truncated"] == true)
    );
    assert!(
        matches!(&feedback[5], CellOutcome::Failed {error:FeedbackError::InvalidOutput {..},printed} if printed[0]["truncated"] == true)
    );
    assert_eq!(
        feedback[6],
        CellOutcome::Completed {
            printed: vec![json!(2)]
        }
    );
    for outcome in &feedback {
        assert!(serde_json::to_vec(outcome)?.len() <= 16384);
    }
    engine.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn tiny_print_budgets_keep_notices_and_unicode_within_the_ceiling()
-> Result<(), Box<dyn Error>> {
    for printed_bytes in [2, 3, 19, 20, 21, 22, 23, 25, 32, 64, 255] {
        let limits = engine_limits();
        let store = Arc::new(InMemoryStore::new(StoreLimits {
            record_bytes: limits.record_bytes,
        })?);
        let executor = QuickJsExecutor::new(QuickJsOptions {
            memory_bytes: 8 * 1024 * 1024,
            stack_bytes: 256 * 1024,
            feedback: cocoa_quickjs::FeedbackLimits {
                printed_bytes,
                print_calls: 1,
                ..feedback_limits()
            },
        })
        .await?;
        let mut engine = Engine::new(
            EngineSettings::new(limits)?,
            EngineAdapters {
                model: Arc::new(Cells(vec![
                    "var visits = 0; print(0); print('🦀\\u0000\"\\\\'.repeat(1000)); visits++"
                        .into(),
                    format!(
                        "if (visits !== {}) throw Error('wrong execution state')",
                        usize::from(printed_bytes >= 20)
                    ),
                ])),
                executor: Box::new(executor),
                capabilities: Arc::new(Host),
                context: Arc::new(BoundedContext),
                store: store.clone(),
            },
            None,
        );
        run(&mut engine).await?;
        let feedback = outcomes(&store).await?;
        let printed = if printed_bytes < 20 {
            let CellOutcome::Failed {
                error: FeedbackError::OutputLimit { limit_bytes },
                printed,
            } = &feedback[0]
            else {
                panic!("{:?}", feedback[0]);
            };
            assert_eq!(*limit_bytes as usize, printed_bytes);
            assert_eq!(
                printed,
                &if printed_bytes == 2 {
                    vec![]
                } else {
                    vec![json!(0)]
                }
            );
            printed
        } else {
            let CellOutcome::Completed { printed } = &feedback[0] else {
                panic!("{:?}", feedback[0]);
            };
            assert_eq!(printed.len(), if printed_bytes < 22 { 1 } else { 2 });
            if printed_bytes >= 22 {
                assert_eq!(printed[0], 0);
            }
            let notice = printed.last().ok_or("missing truncation notice")?;
            assert_eq!(notice["truncated"], true);
            if printed_bytes >= 64 {
                let prefix = notice["json_prefix"]
                    .as_str()
                    .ok_or("missing Unicode preview")?;
                assert!(!prefix.is_empty());
                assert!(serde_json::to_string(&"🦀\0\"\\".repeat(1000))?.starts_with(prefix));
            }
            printed
        };
        assert!(serde_json::to_vec(printed)?.len() <= printed_bytes);
        assert_eq!(feedback[1], CellOutcome::Completed { printed: vec![] });
        engine.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn print_count_and_byte_overflow_preserve_completed_effects() -> Result<(), Box<dyn Error>> {
    let host = Arc::new(CodingHost::new(&["cocoa.write", "cocoa.read"]));
    let (mut engine, store) = engine_with_host(&[
        "await cocoa.write({file:'observed',content:'survives'}); print('kept'); print('x'.repeat(9000)); await cocoa.write({file:'after',content:'continues'})",
        "for (let i = 0; i < 32; i++) print(i); for (let i = 0; i < 100; i++) print(); print(32); print(33); await cocoa.write({file:'count',content:'continues'})",
        "var retained_error; await cocoa.write({file:'updated',content:'also survives'}); try {await cocoa.read({fail:'none'})} catch(error) {retained_error = error; throw error}",
        "throw retained_error",
    ], None, engine_limits(), host.clone()).await?;
    run(&mut engine).await?;
    let feedback = outcomes(&store).await?;
    let CellOutcome::Completed { printed } = &feedback[0] else {
        panic!("{:?}", feedback[0]);
    };
    assert_eq!(printed.len(), 2);
    assert_eq!(printed[0], "kept");
    assert_eq!(printed[1]["truncated"], true);
    assert!(serde_json::to_vec(printed)?.len() <= 8192);
    let mut expected: Vec<_> = (0..32).map(|number| json!(number)).collect();
    expected.push(json!({"truncated":true,"json_prefix":"32"}));
    assert_eq!(feedback[1], CellOutcome::Completed { printed: expected });
    for outcome in &feedback[2..] {
        assert!(
            matches!(outcome, CellOutcome::Failed {error:FeedbackError::Capability {method, operation_effects:OperationEffects::None, ..}, ..} if method == "cocoa.read")
        );
    }
    assert_eq!(feedback[2], feedback[3]);
    assert_eq!(host.calls.lock().await.len(), 4);
    let records = store.load().await?.into_records();
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(
                &record.kind,
                RecordKind::OperationFinished {
                    outcome: cocoa_core::capability::OperationOutcome::Completed { .. },
                    ..
                }
            ))
            .count(),
        4
    );
    engine.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn native_bindings_survive_cells_and_turns_with_independent_engine_isolation()
-> Result<(), Box<dyn Error>> {
    let (mut engine, store) = engine(&[
        "var turns = (turns ?? 0) + 1; var reusable = 17; print(turns)",
        "let lexical = 11; const fixed = {nested:7}; function add(value) { return lexical + value }; var closure = (() => { let privateValue = 3; return () => ++privateValue })()",
        "var reusable = 29; lexical += 2; fixed.nested += 5; { let blockOnly = 19; var blockVar = 23; } function local() { var functionOnly = 31; return functionOnly }; print([reusable, lexical, fixed.nested, add(5), closure(), blockVar, local(), typeof blockOnly, typeof functionOnly])",
        "let lexical = 99; print('must not run')",
        "const fixed = 99; print('must not run')",
        "var lexical = 99; print('must not run')",
        "fixed = {}; print('must not run')",
        "lexical += await Promise.resolve(7); print('before failure'); throw Error('recoverable')",
        "Promise.resolve().then(() => { lexical += 3 }); (() => 0)",
        "print([turns, reusable, lexical, fixed.nested, add(2), closure()])",
    ]).await?;
    run(&mut engine).await?;
    run(&mut engine).await?;
    let feedback = outcomes(&store).await?;
    for (offset, turn) in [(0, 1), (10, 2)] {
        assert_eq!(
            feedback[offset],
            CellOutcome::Completed {
                printed: vec![json!(turn)]
            }
        );
        if turn == 1 {
            assert_eq!(
                feedback[offset + 1],
                CellOutcome::Completed { printed: vec![] }
            );
        } else {
            assert!(
                matches!(&feedback[offset + 1], CellOutcome::Failed { error: FeedbackError::Javascript { message, .. }, printed } if message.contains("SyntaxError") && printed.is_empty())
            );
        }
        let lexical = if turn == 1 { 13 } else { 25 };
        let nested = if turn == 1 { 12 } else { 17 };
        assert_eq!(
            feedback[offset + 2],
            CellOutcome::Completed {
                printed: vec![json!([
                    29,
                    lexical,
                    nested,
                    lexical + 5,
                    if turn == 1 { 4 } else { 6 },
                    23,
                    31,
                    "undefined",
                    "undefined"
                ])]
            }
        );
        for index in [3, 4, 5] {
            assert!(
                matches!(&feedback[offset + index], CellOutcome::Failed { error: FeedbackError::Javascript { message, .. }, printed } if message.contains("SyntaxError") && printed.is_empty()),
                "{:?}",
                feedback[offset + index]
            );
        }
        assert!(
            matches!(&feedback[offset + 6], CellOutcome::Failed { error: FeedbackError::Javascript { message, .. }, printed } if message.contains("TypeError") && printed.is_empty())
        );
        assert!(
            matches!(&feedback[offset + 7], CellOutcome::Failed { printed, .. } if printed == &["before failure"])
        );
        assert_eq!(
            feedback[offset + 8],
            CellOutcome::Completed { printed: vec![] }
        );
        assert_eq!(
            feedback[offset + 9],
            CellOutcome::Completed {
                printed: vec![json!([
                    turn,
                    29,
                    lexical + 10,
                    nested,
                    lexical + 12,
                    if turn == 1 { 5 } else { 7 }
                ])]
            }
        );
    }
    engine.shutdown().await?;
    let (mut independent, independent_store) = self::engine(&["print([typeof turns,typeof reusable,typeof lexical,typeof fixed,typeof add,typeof closure,typeof state])"]).await?;
    run(&mut independent).await?;
    assert_eq!(
        outcomes(&independent_store).await?,
        vec![CellOutcome::Completed {
            printed: vec![json!([
                "undefined",
                "undefined",
                "undefined",
                "undefined",
                "undefined",
                "undefined",
                "undefined"
            ])]
        }]
    );
    independent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn generated_bindings_bypass_inherited_indexed_setters() -> Result<(), Box<dyn Error>> {
    struct GeneratedHost;

    #[async_trait]
    impl CapabilityHost for GeneratedHost {
        fn descriptions(&self) -> Vec<CapabilityDescription> {
            Vec::new()
        }

        fn bindings(&self) -> Vec<cocoa_core::capability::CapabilityBinding> {
            vec![cocoa_core::capability::CapabilityBinding {
                path: vec!["mcp".into(), "server".into(), "echo".into()],
                name: "echo".into(),
            }]
        }

        async fn call(
            &self,
            name: &str,
            arguments: Value,
            _: WorkLimits,
            _: CancellationToken,
        ) -> Result<Value, CapabilityError> {
            assert_eq!(name, "echo");
            Ok(arguments)
        }
    }

    tokio::time::timeout(Duration::from_secs(5), async {
        for prototype in ["Array.prototype", "Object.prototype"] {
            let mutation = format!("var setter_calls = 0; Object.defineProperty({prototype}, '0', {{set() {{setter_calls++; throw Error('indexed setter ran')}}, configurable:true}})");
            let (mut engine, store) = engine_with_host(
                &[
                    "var saved_echo = mcp.server.echo; print(await saved_echo({value:17}))",
                    &mutation,
                    "print(await mcp.server.echo({value:23})); print(await saved_echo({value:29})); print(setter_calls)",
                ],
                None,
                engine_limits(),
                Arc::new(GeneratedHost),
            ).await?;
            run(&mut engine).await?;
            assert_eq!(outcomes(&store).await?, vec![
                CellOutcome::Completed { printed: vec![json!({"value":17})] },
                CellOutcome::Completed { printed: vec![] },
                CellOutcome::Completed {
                    printed: vec![json!({"value":23}), json!({"value":29}), json!(0)],
                },
            ], "{prototype}");
            engine.shutdown().await?;
        }
        Ok::<_, Box<dyn Error>>(())
    }).await?
}

#[tokio::test]
async fn boundary_paths_bypass_mutated_regexp_exec() -> Result<(), Box<dyn Error>> {
    tokio::time::timeout(Duration::from_secs(5), async {
        for mutation in [
            "RegExp.prototype.exec = () => { executions++; throw Error('exec ran') }",
            "Object.defineProperty(RegExp.prototype, 'exec', {get() { executions++; throw Error('exec getter ran') }, configurable:true})",
        ] {
            let (mut engine, store) = engine(&[
                "var executions = 0; var saved_call = capabilities.call",
                mutation,
                "print(await saved_call('echo', {plain:7,'space key':11})); print(executions)",
                "print({get plain() {throw Error('getter ran')}})",
                "print({'space key':undefined})",
                "print(executions)",
            ]).await?;
            run(&mut engine).await?;
            let feedback = outcomes(&store).await?;
            assert_eq!(feedback.len(), 6);
            for outcome in &feedback[..2] {
                assert_eq!(outcome, &CellOutcome::Completed { printed: vec![] });
            }
            assert_eq!(feedback[2], CellOutcome::Completed {
                printed: vec![json!({"plain":7,"space key":11}), json!(0)],
            }, "{mutation}");
            for (index, diagnostic) in [(3, "accessor at $.plain"), (4, "undefined at $[\"space key\"]")] {
                assert!(matches!(&feedback[index], CellOutcome::Failed {
                    error: FeedbackError::InvalidOutput { message, .. }, printed,
                } if message.contains(diagnostic) && printed.is_empty()), "{mutation}: {feedback:?}");
            }
            assert_eq!(feedback[5], CellOutcome::Completed { printed: vec![json!(0)] });
            engine.shutdown().await?;
        }
        Ok::<_, Box<dyn Error>>(())
    }).await?
}

#[tokio::test]
async fn trusted_helpers_survive_intrinsic_changes_between_cells() -> Result<(), Box<dyn Error>> {
    let host = Arc::new(CodingHost::new(&["cocoa.read"]));
    let (mut engine, store) = engine_with_host(&[
        "var savedPrint = print; var savedRead = cocoa.read; var savedCall = capabilities.call; var retainedError; try { await savedRead({fail:'partial'}) } catch (error) { retainedError = error; throw error }",
        "Object.create = Object.defineProperty = Object.getOwnPropertyDescriptor = Object.getPrototypeOf = Object.freeze = () => { throw Error('patched') }; Array.isArray = Array.prototype.push = () => { throw Error('patched') }; Reflect.apply = () => { throw Error('patched') }; WeakMap.prototype.get = WeakMap.prototype.set = () => { throw Error('patched') }; JSON.stringify = () => 'wrong'; String.prototype.charCodeAt = () => { throw Error('patched') }; String = () => 'wrong'; TypeError = () => { throw Error('patched') }",
        "var observed = await savedRead({path:'after.rs'}); savedPrint({path:observed.path}); savedPrint((await savedCall('cocoa.read',{path:'generic.rs'})).path); var first = await savedRead({path:'first.rs'}); var effects; try { await savedRead({fail:'unknown'}); } catch (error) { effects = error.effects; } var last = await savedRead({path:'last.rs'}); savedPrint([first.path,effects,last.path])",
        "throw retainedError",
        "throw Error('trusted formatting')",
        "savedPrint({get bad() { throw Error('getter ran') }})",
    ], None, engine_limits(), host).await?;
    run(&mut engine).await?;
    let feedback = outcomes(&store).await?;
    assert_eq!(
        feedback[2],
        CellOutcome::Failed {
            error: FeedbackError::Capability {
                method: "cocoa.read".into(),
                code: "stale_read".into(),
                message: "Read again before editing".into(),
                operation_effects: OperationEffects::Unknown,
                truncated: false,
            },
            printed: vec![
                json!({"path":"after.rs"}),
                json!("generic.rs"),
                json!(["first.rs", "unknown", "last.rs"])
            ]
        }
    );
    assert_eq!(feedback[0], feedback[3]);
    assert!(
        matches!(&feedback[4], CellOutcome::Failed { error: FeedbackError::Javascript { message, .. }, .. } if message == "Error: trusted formatting")
    );
    assert!(
        matches!(&feedback[5], CellOutcome::Failed { error: FeedbackError::InvalidOutput { message, .. }, .. } if message.contains("accessor at $.bad"))
    );
    engine.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn saved_host_functions_obey_current_grants() -> Result<(), Box<dyn Error>> {
    struct RevokingHost(std::sync::atomic::AtomicBool);
    #[async_trait]
    impl CapabilityHost for RevokingHost {
        fn descriptions(&self) -> Vec<CapabilityDescription> {
            CodingHost::new(&["cocoa.read"]).descriptions()
        }

        async fn call(
            &self,
            _: &str,
            arguments: Value,
            _: WorkLimits,
            _: CancellationToken,
        ) -> Result<Value, CapabilityError> {
            if self.0.swap(false, std::sync::atomic::Ordering::SeqCst) {
                Ok(arguments)
            } else {
                Err(CapabilityError::Rejected {
                    message: "grant revoked".into(),
                })
            }
        }
    }
    let (mut engine, store) = engine_with_host(&[
        "var savedRead = cocoa.read; print(await savedRead({value:17}))",
        "for (var value of [29,31,43]) { try { await savedRead({value}) } catch (error) { print(error.message, error.code) } }",
    ], None, engine_limits(), Arc::new(RevokingHost(std::sync::atomic::AtomicBool::new(true)))).await?;
    run(&mut engine).await?;
    assert_eq!(
        outcomes(&store).await?,
        vec![
            CellOutcome::Completed {
                printed: vec![json!({"value":17})]
            },
            CellOutcome::Failed {
                error:FeedbackError::Capability {
                    method:"cocoa.read".into(), code:"invalid_arguments".into(),
                    message:"3 recoverable host failures; earlier effects remain. First: grant revoked; cocoa.read: invalid_arguments: grant revoked (operation effects None); cocoa.read: invalid_arguments: grant revoked (operation effects None)".into(),
                    operation_effects:OperationEffects::None, truncated:false,
                },
                printed: vec![
                    json!(["grant revoked", "invalid_arguments"]),
                    json!(["grant revoked", "invalid_arguments"]),
                    json!(["grant revoked", "invalid_arguments"])
                ]
            },
        ]
    );
    engine.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn retained_bindings_share_the_heap_bound_and_shutdown_releases_them()
-> Result<(), Box<dyn Error>> {
    let (mut engine, store) = engine(&[
        "var retained = new Uint8Array(4 * 1024 * 1024); retained[0] = 31; print(retained.length)",
        "print('before allocation'); var tooLarge = new Uint8Array(5 * 1024 * 1024)",
        "print([retained.length,retained[0],typeof tooLarge]); retained = null; var smaller = new Uint8Array(1024); print(smaller.length)",
    ]).await?;
    run(&mut engine).await?;
    let feedback = outcomes(&store).await?;
    assert_eq!(
        feedback[0],
        CellOutcome::Completed {
            printed: vec![json!(4194304)]
        }
    );
    assert!(
        matches!(&feedback[1], CellOutcome::Failed { error: FeedbackError::Javascript { .. }, printed } if printed == &["before allocation"])
    );
    assert_eq!(
        feedback[2],
        CellOutcome::Completed {
            printed: vec![json!([4194304, 31, "undefined"]), json!(1024)]
        }
    );
    tokio::time::timeout(Duration::from_secs(1), engine.shutdown()).await??;
    Ok(())
}

#[tokio::test]
async fn prints_remain_canonical_after_sync_and_async_failures() -> Result<(), Box<dyn Error>> {
    let (mut engine, store) = engine(&[
        "print('kept', 19); 23",
        "print('before throw'); throw new Error('sync failure')",
        "print('before rejection'); await Promise.reject(new Error('async failure'))",
    ])
    .await?;
    run(&mut engine).await?;
    let feedback = outcomes(&store).await?;
    assert_eq!(
        feedback[0],
        CellOutcome::Completed {
            printed: vec![json!(["kept", 19])]
        }
    );
    for (outcome, expected_print, expected_error) in [
        (&feedback[1], "before throw", "sync failure"),
        (&feedback[2], "before rejection", "async failure"),
    ] {
        assert!(
            matches!(outcome, CellOutcome::Failed { error: FeedbackError::Javascript { message, .. }, printed } if message.contains(expected_error) && printed == &[expected_print])
        );
    }
    engine.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn delayed_capabilities_and_unawaited_work_settle_before_cell_feedback()
-> Result<(), Box<dyn Error>> {
    let (mut engine, store) = engine(&[
        "print(await capabilities.call('multiply', {left: 7, right: 3}) + 2)",
        "var delayed; capabilities.call('multiply', {left: 5, right: 11}).then(x => { delayed = x; print('done', x) }); 17",
        "print(delayed)",
        "capabilities.call('denied', {}); print('started'); throw new Error('later failure')",
        "await capabilities.call('denied', {})",
        "try { await capabilities.call('denied', {}) } catch { 41 }",
    ]).await?;
    run(&mut engine).await?;
    let feedback = outcomes(&store).await?;
    assert_eq!(
        feedback[0],
        CellOutcome::Completed {
            printed: vec![json!(23)]
        }
    );
    assert_eq!(
        feedback[1],
        CellOutcome::Completed {
            printed: vec![json!(["done", 55])]
        }
    );
    assert_eq!(
        feedback[2],
        CellOutcome::Completed {
            printed: vec![json!(55)]
        }
    );
    assert!(matches!(&feedback[3], CellOutcome::Failed { printed, .. } if printed == &["started"]));
    assert!(matches!(
        &feedback[4],
        CellOutcome::Failed {
            error: FeedbackError::Capability { .. },
            ..
        }
    ));
    assert_eq!(
        feedback[5],
        CellOutcome::Failed {
            error: FeedbackError::Capability {
                method: "denied".into(),
                code: "invalid_arguments".into(),
                message: "no grants".into(),
                operation_effects: OperationEffects::None,
                truncated: false,
            },
            printed: Vec::new()
        }
    );
    let records = store.load().await?.into_records();
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record.kind, RecordKind::OperationStarted { .. }))
            .count(),
        5
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record.kind, RecordKind::OperationFinished { .. }))
            .count(),
        5
    );
    assert!(records.iter().any(|record| matches!(&record.kind, RecordKind::OperationFinished { outcome: cocoa_core::capability::OperationOutcome::Completed { value }, .. } if value == &json!(21))));
    engine.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn retained_functions_use_the_current_cells_broker_and_print_feedback()
-> Result<(), Box<dyn Error>> {
    let (mut engine, store) = engine(&[
        "var saved = async x => { print('saved', x); return await capabilities.call('multiply', {left: x, right: 7}) }; undefined",
        "print(await saved(13))",
        "print(await saved(5))",
    ]).await?;
    run(&mut engine).await?;
    assert_eq!(
        outcomes(&store).await?,
        vec![
            CellOutcome::Completed {
                printed: Vec::new()
            },
            CellOutcome::Completed {
                printed: vec![json!(["saved", 13]), json!(91)]
            },
            CellOutcome::Completed {
                printed: vec![json!(["saved", 5]), json!(35)]
            },
        ]
    );
    let records = store.load().await?.into_records();
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record.kind, RecordKind::OperationFinished { .. }))
            .count(),
        2
    );
    engine.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn host_results_bypass_setters_and_allow_reentrant_promise_assimilation()
-> Result<(), Box<dyn Error>> {
    let (mut engine, store) = engine(&[
        "Object.defineProperty(Object.prototype, 'payload', {set() { throw Error('setter must not run'); }}); Object.defineProperty(Object.prototype, 'then', {get() { print('assimilated'); return undefined; }, configurable: true}); const value = await capabilities.call('echo', {payload: 19, nested: [7, 3]}); delete Object.prototype.then; print([value.payload, value.nested])",
    ]).await?;
    run(&mut engine).await?;
    assert!(
        matches!(&outcomes(&store).await?[0], CellOutcome::Completed { printed } if printed.last() == Some(&json!([19, [7, 3]])) && printed.iter().any(|text| text == "assimilated"))
    );
    assert!(store.load().await?.into_records().iter().any(|record| matches!(&record.kind, RecordKind::OperationFinished { outcome: cocoa_core::capability::OperationOutcome::Completed { value }, .. } if value == &json!({"payload":19,"nested":[7,3]}))));
    engine.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn argument_limits_count_encoded_json_instead_of_worst_case_escaping()
-> Result<(), Box<dyn Error>> {
    let host = Arc::new(CodingHost::new(&["cocoa.write"]));
    let mut cells =
        vec!["await cocoa.write({content:'a'.repeat(11208)}); print('written')".to_owned()];
    let mut accepted = vec![json!({"content":"a".repeat(11208)})];
    let limit = 32768;
    for unit in ["a", "\0", "\n\"\\", "λ", "🦀", "漢"] {
        for bytes in [limit - 1, limit, limit + 1] {
            let empty = json!({"content":"", "🦀\n":[true, null, 7.25]});
            let overhead = serde_json::to_vec(&empty)?.len();
            let width = serde_json::to_vec(unit)?.len() - 2;
            let count = (bytes - overhead) / width;
            let content = unit.repeat(count) + &"x".repeat((bytes - overhead) % width);
            let arguments = json!({"content":content, "🦀\n":[true, null, 7.25]});
            assert_eq!(serde_json::to_vec(&arguments)?.len(), bytes);
            let arguments = serde_json::to_string(&arguments)?;
            let call = if bytes % 2 == 0 {
                format!("cocoa.write({arguments})")
            } else {
                format!("capabilities.call('cocoa.write', {arguments})")
            };
            cells.push(format!(
                "try {{ await {call}; print('written') }} catch(error) {{ print(error.message) }}"
            ));
            if bytes <= limit {
                accepted.push(serde_json::from_str(&arguments)?);
            }
        }
    }
    let mut limits = engine_limits();
    limits.output_bytes = 65536;
    limits.record_bytes = 131072;
    limits.context_bytes = 2_000_000;
    let cell_refs: Vec<_> = cells.iter().map(String::as_str).collect();
    let (mut engine, store) = engine_with_host(&cell_refs, None, limits, host.clone()).await?;
    run(&mut engine).await?;
    let feedback = outcomes(&store).await?;
    assert_eq!(feedback.len(), cells.len());
    for (index, outcome) in feedback.iter().enumerate() {
        if index == 0 || index % 3 != 0 {
            assert_eq!(
                outcome,
                &CellOutcome::Completed {
                    printed: vec![json!("written")]
                }
            );
        } else {
            assert!(
                matches!(outcome, CellOutcome::Completed { printed } if printed.len() == 1 && printed[0].as_str().is_some_and(|message| message.contains("32768") && message.contains("argument") && message.len() < 1024)),
                "{outcome:?}"
            );
        }
    }
    let calls = host.calls.lock().await;
    assert_eq!(calls.len(), accepted.len());
    for ((method, arguments), expected) in calls.iter().zip(accepted) {
        assert_eq!(method, "cocoa.write");
        assert_eq!(arguments, &expected);
    }
    drop(calls);
    engine.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn boundary_conversion_rejects_lossy_values() -> Result<(), Box<dyn Error>> {
    let (mut engine, store) = engine(&[
        "print({nested: () => 3})",
        "const cyclic = {}; cyclic.self = cyclic; print(cyclic)",
        "let deep = {}; for (let i = 0; i < 40; i++) deep = {next: deep}; print(deep)",
        "print(new Date())",
        "print({get value() { throw Error('getter must not run') }})",
        "print({missing: undefined})",
        "print({finite: Infinity})",
        "print({ok: [17, 'é', null, false]})",
    ])
    .await?;
    run(&mut engine).await?;
    let feedback = outcomes(&store).await?;
    for outcome in &feedback[..7] {
        assert!(
            matches!(
                outcome,
                CellOutcome::Failed {
                    error: FeedbackError::InvalidOutput { .. },
                    ..
                }
            ),
            "unexpected {outcome:?}"
        );
    }
    assert_eq!(
        feedback[7],
        CellOutcome::Completed {
            printed: vec![json!({"ok":[17,"é",null,false]})]
        }
    );
    engine.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn unhandled_rejections_fail_without_discarding_prints_or_caught_recovery()
-> Result<(), Box<dyn Error>> {
    let (mut engine, store) = engine(&[
        "Promise.reject(new Error('unhandled')); print('retained'); 7",
        "capabilities.call('denied', {}); 11",
        "Promise.reject(new Error('handled')).catch(() => print('recovered')); 19",
        "var formatted; Promise.reject({toString() { capabilities.call('multiply', {left: 3, right: 11}).then(x => {formatted = x; print('formatted', x)}); return 'formatter rejection'; }}); 7",
        "print(formatted)",
    ])
    .await?;
    run(&mut engine).await?;
    let feedback = outcomes(&store).await?;
    assert!(
        matches!(&feedback[0], CellOutcome::Failed { error: FeedbackError::Javascript { message, .. }, printed } if message.contains("unhandled") && printed == &["retained"])
    );
    assert!(matches!(
        &feedback[1],
        CellOutcome::Failed {
            error: FeedbackError::Capability { .. },
            ..
        }
    ));
    assert_eq!(
        feedback[2],
        CellOutcome::Completed {
            printed: vec!["recovered".into()]
        }
    );
    assert!(
        matches!(&feedback[3], CellOutcome::Failed { error: FeedbackError::Javascript { message, .. }, printed } if message.contains("formatter rejection") && printed == &[json!(["formatted", 33])])
    );
    assert_eq!(
        feedback[4],
        CellOutcome::Completed {
            printed: vec![json!(33)]
        }
    );
    assert_eq!(
        store
            .load()
            .await?
            .into_records()
            .iter()
            .filter(|record| matches!(record.kind, RecordKind::OperationFinished { .. }))
            .count(),
        2
    );
    engine.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn memory_print_and_error_bounds_preserve_earlier_prints() -> Result<(), Box<dyn Error>> {
    let (mut engine, store) = engine(&[
        "print('before allocation'); 'x'.repeat(20000000)",
        "print('before overflow'); try { print('x'.repeat(100000)) } catch {} 7",
        "print('before error'); throw new Error('é'.repeat(100000))",
        "const reused = {x: 7}; print([reused, reused])",
        "Object.prototype.exit = true; Object.prototype.toJSON = () => 999; print({ok: 23})",
        "print('before recursion'); (function recurse() { return recurse() })()",
    ])
    .await?;
    run(&mut engine).await?;
    let feedback = outcomes(&store).await?;
    for (outcome, expected) in
        feedback[..3]
            .iter()
            .zip(["before allocation", "before overflow", "before error"])
    {
        if expected == "before overflow" {
            assert!(
                matches!(outcome, CellOutcome::Completed {printed} if printed.len() == 2 && printed[0] == expected && printed[1]["truncated"] == true)
            );
        } else {
            assert!(
                matches!(outcome, CellOutcome::Failed { printed, .. } if printed == &[expected]),
                "{outcome:?}"
            );
        }
        assert!(serde_json::to_vec(outcome)?.len() <= 16384);
    }
    assert_eq!(
        feedback[3],
        CellOutcome::Completed {
            printed: vec![json!([{"x":7},{"x":7}])]
        }
    );
    assert_eq!(
        feedback[4],
        CellOutcome::Completed {
            printed: vec![json!({"ok":23})]
        }
    );
    assert!(
        matches!(&feedback[5], CellOutcome::Failed { error: FeedbackError::Javascript { .. }, printed } if printed == &["before recursion"])
    );
    engine.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn ordinary_javascript_and_host_errors_preserve_live_bindings() -> Result<(), Box<dyn Error>>
{
    let host = Arc::new(CodingHost::new(&["cocoa.read"]));
    let (mut engine, store) = engine_with_host(
        &[
            "var state = {value:1}; var alias = state; let lexical = 7; const fixed = state; function retained() { return ++state.value + lexical; } var savedRead = cocoa.read;",
            "state.value++; lexical++; throw new Error('sync failure');",
            "print([state === alias, fixed === state, retained()]); state.value++; await Promise.reject(new Error('async failure'));",
            "print([state === alias, fixed === state, retained()]); state.value++; await savedRead({fail:'none'});",
            "print([state === alias, fixed === state, retained(), lexical]); print((await savedRead({path:'after errors'})).path);",
        ],
        None,
        engine_limits(),
        host,
    )
    .await?;
    run(&mut engine).await?;
    let feedback = outcomes(&store).await?;
    assert_eq!(feedback.len(), 5);
    assert_eq!(feedback[0], CellOutcome::Completed { printed: vec![] });
    for (index, message, printed) in [
        (1, "sync failure", vec![]),
        (2, "async failure", vec![json!([true, true, 11])]),
    ] {
        assert!(
            matches!(&feedback[index], CellOutcome::Failed { error: FeedbackError::Javascript { message: actual, .. }, printed: actual_printed } if actual.contains(message) && actual_printed == &printed)
        );
    }
    assert!(
        matches!(&feedback[3], CellOutcome::Failed { error: FeedbackError::Capability { method, code, operation_effects: OperationEffects::None, .. }, printed } if method == "cocoa.read" && code == "stale_read" && printed == &[json!([true, true, 13])])
    );
    assert_eq!(
        feedback[4],
        CellOutcome::Completed {
            printed: vec![json!([true, true, 15, 8]), json!("after errors")],
        }
    );
    engine.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn cancellation_and_deadline_reset_the_heap_without_running_old_callbacks()
-> Result<(), Box<dyn Error>> {
    for interruption in ["cancel", "deadline"] {
        let store = Arc::new(InMemoryStore::new(StoreLimits {
            record_bytes: engine_limits().record_bytes,
        })?);
        let host = Arc::new(CodingHost::new(&["cocoa.read", "cocoa.write"]));
        let (events, mut receiver) = EventPublisher::channel(64, 1024)?;
        let mut engine = Engine::new(
            EngineSettings::new(engine_limits())?,
            EngineAdapters {
                model: Arc::new(ResumingCells {
                    before: vec![
                        "var earlierVariable = {value:7}; let earlierLexical = 17; const earlierConstant = 19; function earlierFunction() { return earlierLexical; } var earlierClosure = () => earlierVariable; var savedRead = cocoa.read; print((await savedRead({path:'before'})).path); await cocoa.write({file:'observed',content:'kept'});".into(),
                        "var partial = 23; var target = {}; var registry = new FinalizationRegistry(() => { print('old finalizer'); cocoa.write({file:'forbidden',content:'finalizer'}); }); registry.register(target, 1); queueMicrotask(() => { print('old microtask'); cocoa.write({file:'forbidden',content:'microtask'}); }); Promise.resolve().then(() => { print('old reaction'); cocoa.write({file:'forbidden',content:'reaction'}); }); try { print('ready'); while (true) {} } finally { print('old finally'); cocoa.write({file:'forbidden',content:'finally'}); }".into(),
                    ],
                    after: "print([typeof earlierVariable, typeof earlierLexical, typeof earlierConstant, typeof earlierFunction, typeof earlierClosure, typeof savedRead, typeof partial, typeof target, typeof registry]); var freshRead = cocoa.read; print((await freshRead({path:'continued'})).path); await Promise.resolve(); print(6 * 7);".into(),
                }),
                executor: Box::new(
                    QuickJsExecutor::new(QuickJsOptions {
                        memory_bytes: 8 * 1024 * 1024,
                        stack_bytes: 256 * 1024,
                        feedback: feedback_limits(),
                    })
                    .await?,
                ),
                capabilities: host.clone(),
                context: Arc::new(BoundedContext),
                store: store.clone(),
            },
            Some(events),
        );
        let cancellation = CancellationToken::new();
        let interrupt = async {
            let ready = tokio::time::timeout(Duration::from_secs(1), async {
                for _ in 0..64 {
                    let event = receiver.recv().await;
                    if matches!(event, cocoa_core::engine::Observation::Event(cocoa_core::engine::Event { kind: cocoa_core::engine::EventKind::Printed { text, .. }, .. }) if text == "ready") {
                        return true;
                    }
                }
                false
            }).await == Ok(true);
            if interruption == "cancel" || !ready {
                cancellation.cancel();
            }
            ready
        };
        let (result, ready) = tokio::join!(
            engine.run(
                RunRequest {
                    input: "prepare".into()
                },
                cancellation.clone()
            ),
            interrupt
        );
        assert!(
            ready,
            "setup did not reach interruption point: {interruption}"
        );
        let mut expected = vec![CellOutcome::Completed {
            printed: vec![json!("before")],
        }];
        if interruption == "cancel" {
            assert!(
                matches!(
                    result,
                    Err(cocoa_core::engine::RunError::TurnCancelled {
                        runtime: cocoa_core::execution::RuntimeState::Reset
                    })
                ),
                "{result:?}"
            );
            expected.push(CellOutcome::Interrupted {
                printed: vec![json!("ready")],
            });
        } else {
            assert!(
                matches!(
                    result,
                    Err(cocoa_core::engine::RunError::Deadline {
                        cause: cocoa_core::store::FailureCause::Cell { .. }
                    })
                ),
                "{result:?}"
            );
            expected.push(CellOutcome::Interrupted { printed: vec![] });
        }
        assert_eq!(outcomes(&store).await?, expected, "{interruption}");
        let resumed = engine
            .run(
                RunRequest {
                    input: "resume".into(),
                },
                CancellationToken::new(),
            )
            .await;
        if interruption == "cancel" {
            resumed?;
            expected.push(CellOutcome::Completed {
                printed: vec![json!(vec!["undefined"; 9]), json!("continued"), json!(42)],
            });
        } else {
            assert!(matches!(
                resumed,
                Err(cocoa_core::engine::RunError::Stopped)
            ));
        }
        assert_eq!(outcomes(&store).await?, expected, "{interruption}");
        let mut expected_calls = vec![
            ("cocoa.read".into(), json!({"path":"before"})),
            (
                "cocoa.write".into(),
                json!({"file":"observed","content":"kept"}),
            ),
        ];
        if interruption == "cancel" {
            expected_calls.push(("cocoa.read".into(), json!({"path":"continued"})));
        }
        assert_eq!(*host.calls.lock().await, expected_calls, "{interruption}");
        engine.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn busy_javascript_is_interrupted_and_shutdown_remains_idempotent()
-> Result<(), Box<dyn Error>> {
    tokio::time::timeout(Duration::from_secs(5), async {
        for code in [
            "print('ready'); while (true) {}",
            "Promise.resolve().then(() => { print('ready'); return Promise.resolve().then(function spin() { return Promise.resolve().then(spin) }); }); await new Promise(() => {})",
        ] {
            let (events, mut receiver) = EventPublisher::channel(32, 1024)?;
            let (mut engine, store) =
                engine_observed(&[code], Some(events), engine_limits()).await?;
            let cancellation = CancellationToken::new();
            let result = {
                let request = engine.run(
                    RunRequest {
                        input: "interrupt".into(),
                    },
                    cancellation.clone(),
                );
                tokio::pin!(request);
                tokio::select! {
                    result = &mut request => return Err(format!("run ended before readiness: {result:?}").into()),
                    ready = wait_for_printed_readiness(&mut receiver) => ready?,
                }
                cancellation.cancel();
                request.await
            };
            assert!(
                matches!(
                    result,
                    Err(cocoa_core::engine::RunError::TurnCancelled {
                        runtime: cocoa_core::execution::RuntimeState::Reset
                    })
                ),
                "{result:?}"
            );
            assert_eq!(
                outcomes(&store).await?,
                vec![CellOutcome::Interrupted {
                    printed: vec![json!("ready")]
                }]
            );
            tokio::time::timeout(Duration::from_secs(1), engine.shutdown()).await??;
        }
        let (mut engine, store) = engine(&["while (true) {}"]).await?;
        let started = std::time::Instant::now();
        let result = engine
            .run(
                RunRequest {
                    input: "deadline".into(),
                },
                CancellationToken::new(),
            )
            .await;
        assert!(
            matches!(
                result,
                Err(cocoa_core::engine::RunError::Deadline {
                    cause: cocoa_core::store::FailureCause::Cell { .. }
                })
            ),
            "{result:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_eq!(
            outcomes(&store).await?,
            vec![CellOutcome::Interrupted { printed: vec![] }]
        );
        tokio::time::timeout(Duration::from_secs(1), engine.shutdown()).await??;
        Ok::<_, Box<dyn Error>>(())
    }).await?
}

#[tokio::test]
async fn dropping_active_work_still_allows_bounded_shutdown() -> Result<(), Box<dyn Error>> {
    struct HeldHost {
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }

    #[async_trait]
    impl CapabilityHost for HeldHost {
        fn descriptions(&self) -> Vec<CapabilityDescription> {
            Vec::new()
        }

        async fn call(
            &self,
            name: &str,
            arguments: Value,
            _: WorkLimits,
            _: CancellationToken,
        ) -> Result<Value, CapabilityError> {
            assert_eq!(name, "multiply");
            assert_eq!(arguments, json!({"left":7,"right":3}));
            self.entered.notify_one();
            self.release.notified().await;
            Ok(json!(21))
        }
    }

    tokio::time::timeout(Duration::from_secs(5), async {
        for (code, operations) in [
            ("print('ready'); while (true) {}", 0),
            ("await capabilities.call('multiply', {left: 7, right: 3})", 1),
        ] {
            let host = Arc::new(HeldHost {
                entered: tokio::sync::Notify::new(),
                release: tokio::sync::Notify::new(),
            });
            let (events, mut receiver) = EventPublisher::channel(32, 1024)?;
            let (mut engine, store) =
                engine_with_host(&[code], Some(events), engine_limits(), host.clone()).await?;
            {
                let request = engine.run(
                    RunRequest {
                        input: "drop".into(),
                    },
                    CancellationToken::new(),
                );
                tokio::pin!(request);
                let ready = async {
                    if operations == 0 {
                        wait_for_printed_readiness(&mut receiver).await
                    } else {
                        host.entered.notified().await;
                        Ok(())
                    }
                };
                tokio::select! {
                    result = &mut request => return Err(format!("run ended before readiness: {result:?}").into()),
                    ready = ready => ready?,
                }
                assert!(futures_util::poll!(&mut request).is_pending());
            }
            host.release.notify_one();
            tokio::time::timeout(Duration::from_secs(1), engine.shutdown()).await??;
            let records = store.load().await?.into_records();
            let starts = records
                .iter()
                .filter(|record| matches!(record.kind, RecordKind::OperationStarted { .. }))
                .count();
            let finishes = records
                .iter()
                .filter(|record| matches!(record.kind, RecordKind::OperationFinished { .. }))
                .count();
            assert_eq!(starts, operations);
            assert_eq!(starts, finishes);
            for record in &records {
                if let RecordKind::OperationFinished { outcome, .. } = &record.kind {
                    assert_eq!(outcome, &cocoa_core::capability::OperationOutcome::Unknown);
                }
            }
            assert_eq!(
                outcomes(&store).await?,
                vec![CellOutcome::Interrupted { printed: vec![] }]
            );
        }
        Ok::<_, Box<dyn Error>>(())
    }).await?
}

#[tokio::test]
async fn core_pending_limit_cannot_be_hidden_by_javascript_catch() -> Result<(), Box<dyn Error>> {
    let (mut engine, store) = engine(&["try { await Promise.all(Array.from({length: 5}, () => capabilities.call('multiply', {left: 7, right: 3}))) } catch {} 23"]).await?;
    let result = engine
        .run(
            RunRequest {
                input: "pending".into(),
            },
            CancellationToken::new(),
        )
        .await;
    assert!(
        matches!(
            result,
            Err(cocoa_core::engine::RunError::Execution(
                cocoa_core::execution::ExecutionError::Capability(CapabilityError::PendingLimit)
            ))
        ),
        "{result:?}"
    );
    assert_eq!(
        outcomes(&store).await?,
        vec![CellOutcome::Interrupted { printed: vec![] }]
    );
    let records = store.load().await?.into_records();
    assert!(
        records
            .iter()
            .any(|record| matches!(record.kind, RecordKind::OperationStarted { .. }))
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record.kind, RecordKind::OperationStarted { .. }))
            .count(),
        records
            .iter()
            .filter(|record| matches!(record.kind, RecordKind::OperationFinished { .. }))
            .count()
    );
    engine.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn fixed_bridge_and_source_bounds_fail_cells_without_losing_admitted_operations()
-> Result<(), Box<dyn Error>> {
    let source = format!("7 * 3 + 2{}", " ".repeat(256 * 1024 - 9));
    assert_eq!(source.len(), 256 * 1024);
    let oversized = format!("{source} ");
    let limits = Limits {
        output_bytes: 512 * 1024,
        record_bytes: 1024 * 1024,
        context_bytes: 4_000_000,
        pending_operations: 16,
        stall_timeout: Duration::from_secs(5),
        ..engine_limits()
    };
    let (mut engine, store) = engine_observed(&[
        &source,
        &oversized,
        "print(await Promise.all(Array.from({length: 16}, (_, i) => capabilities.call('multiply', {left: i, right: 3}))))",
        "print('before pending limit'); try { const pending = []; for (let i = 0; i < 17; i++) pending.push(capabilities.call('multiply', {left: i, right: 3})); await Promise.all(pending); } catch {} 23",
        "for (let i = 0; i < 128; i++) await capabilities.call('echo', i); 31",
        "var total = 0; for (let i = 0; i < 150; i++) total += await capabilities.call('echo', 7 * i - 3); print(total);",
    ], None, limits).await?;
    run(&mut engine).await?;
    let feedback = outcomes(&store).await?;
    assert_eq!(
        feedback[0],
        CellOutcome::Completed {
            printed: Vec::new()
        }
    );
    assert!(
        matches!(&feedback[1], CellOutcome::Failed { error: FeedbackError::Javascript { message, .. }, .. } if message.contains("source limit"))
    );
    assert_eq!(
        feedback[2],
        CellOutcome::Completed {
            printed: vec![json!([
                0, 3, 6, 9, 12, 15, 18, 21, 24, 27, 30, 33, 36, 39, 42, 45
            ])]
        }
    );
    assert!(
        matches!(&feedback[3], CellOutcome::Failed { error: FeedbackError::Javascript { message, .. }, printed } if message.contains("bridge limit") && printed == &["before pending limit"])
    );
    assert_eq!(
        feedback[4],
        CellOutcome::Completed {
            printed: Vec::new()
        }
    );
    assert_eq!(
        feedback[5],
        CellOutcome::Completed {
            printed: vec![json!(77775)]
        }
    );
    let records = store.load().await?.into_records();
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record.kind, RecordKind::OperationStarted { .. }))
            .count(),
        310
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record.kind, RecordKind::OperationFinished { .. }))
            .count(),
        310
    );
    engine.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn observation_loss_never_changes_canonical_prints() -> Result<(), Box<dyn Error>> {
    use cocoa_core::engine::{EventKind, Observation};
    let cells = [
        "for (let i = 0; i < 100; i++) print(i); await capabilities.call('multiply', {left: 7, right: 3})",
    ];
    let (mut baseline, baseline_store) = engine(&cells).await?;
    run(&mut baseline).await?;
    baseline.shutdown().await?;
    let without_cell_identities = |records: Vec<cocoa_core::store::Record>| {
        records
            .into_iter()
            .filter(|record| {
                !matches!(
                    record.kind,
                    RecordKind::CellStarted { .. } | RecordKind::CellCompleted { .. }
                )
            })
            .collect::<Vec<_>>()
    };
    let expected = without_cell_identities(baseline_store.load().await?.into_records());
    let expected_outcomes = outcomes(&baseline_store).await?;
    for mode in ["consumed", "saturated", "disconnected"] {
        let (publisher, mut receiver) =
            EventPublisher::channel(if mode == "consumed" { 512 } else { 1 }, 32)?;
        let (mut engine, store) = engine_observed(&cells, Some(publisher), engine_limits()).await?;
        if mode == "consumed" {
            let work = async {
                run(&mut engine).await?;
                engine.shutdown().await?;
                Ok::<_, Box<dyn Error>>(())
            };
            let observe = async {
                let mut printed = false;
                loop {
                    match receiver.recv().await {
                        Observation::Closed => break,
                        Observation::Event(event) => {
                            if matches!(event.kind, EventKind::Printed { .. }) {
                                printed = true;
                            }
                        }
                        Observation::Lagged { .. } => panic!("unexpected lag while consuming"),
                    }
                }
                assert!(printed);
            };
            let (result, ()) = tokio::join!(work, observe);
            result?;
        } else {
            if mode == "disconnected" {
                drop(receiver);
            } else {
                run(&mut engine).await?;
                assert!(matches!(receiver.recv().await, Observation::Lagged { .. }));
                engine.shutdown().await?;
                assert_eq!(
                    without_cell_identities(store.load().await?.into_records()),
                    expected
                );
                assert_eq!(outcomes(&store).await?, expected_outcomes);
                continue;
            }
            run(&mut engine).await?;
            engine.shutdown().await?;
        }
        assert_eq!(
            without_cell_identities(store.load().await?.into_records()),
            expected
        );
        assert_eq!(outcomes(&store).await?, expected_outcomes);
    }
    Ok(())
}

#[tokio::test]
async fn automatic_compaction_preserves_live_closures_effects_and_carried_work_twice()
-> Result<(), Box<dyn Error>> {
    let large_first = json!(format!("first:{}", "A".repeat(6000)));
    let large_second = json!(format!("second:{}", "B".repeat(6000)));
    let large_third = json!(format!("third:{}", "C".repeat(6000)));
    let model = Arc::new(CompactingModel::new(
        vec![
            ModelAction::Final {
                answer: COMPACTION_PLAN.into(),
            },
            execute(&[
                "var retained = (() => { let value = 23; return (left, right) => value += left * 7 - right * 3; })(); await cocoa.write({path:'report.txt',content:'one effect'}); print('first:' + 'A'.repeat(6000))",
                "print(retained(5, 2))",
            ]),
            execute(&["print('second:' + 'B'.repeat(6000))"]),
            execute(&["print(retained(2, 5)); print('third:' + 'C'.repeat(6000))"]),
            execute(&["print(retained(4, 1))"]),
            ModelAction::Final {
                answer: "settled".into(),
            },
        ],
        SummaryReply::Preserve,
    ));
    let policy = CompactionPolicy {
        trigger_bytes: 12_000,
        target_bytes: 8_000,
        ..compaction_policy(28_000)
    };
    let host = Arc::new(CodingHost::new(&["cocoa.write"]));
    let (mut engine, store) = compacting_engine(model.clone(), policy, host.clone()).await?;
    assert_eq!(
        engine
            .run(
                RunRequest {
                    input: COMPACTION_DIRECTIONS.into(),
                },
                CancellationToken::new(),
            )
            .await?
            .answer,
        COMPACTION_PLAN
    );
    run(&mut engine).await?;
    assert_eq!(
        outcomes(&store).await?,
        vec![
            CellOutcome::Completed {
                printed: vec![large_first.clone()],
            },
            CellOutcome::Completed {
                printed: vec![json!(52)],
            },
            CellOutcome::Completed {
                printed: vec![large_second.clone()],
            },
            CellOutcome::Completed {
                printed: vec![json!(51), large_third.clone()],
            },
            CellOutcome::Completed {
                printed: vec![json!(76)],
            },
        ]
    );
    assert_eq!(
        *host.calls.lock().await,
        vec![(
            "cocoa.write".into(),
            json!({"path":"report.txt","content":"one effect"}),
        )]
    );
    let snapshot = store.load().await?;
    assert_eq!(
        snapshot.checkpoints.len(),
        2,
        "provider request sizes: {:?}",
        model
            .requests
            .lock()
            .await
            .iter()
            .map(compaction_request_bytes)
            .collect::<Vec<_>>()
    );
    let original = snapshot
        .records
        .iter()
        .find(|stored| matches!(&stored.record.kind, RecordKind::InputAccepted { input } if input.text() == COMPACTION_DIRECTIONS))
        .ok_or("original directions missing")?;
    assert_eq!(
        snapshot
            .records
            .iter()
            .filter(|stored| matches!(&stored.record.kind, RecordKind::OperationStarted { name, .. } if name == "cocoa.write"))
            .count(),
        1
    );
    assert_eq!(
        snapshot
            .records
            .iter()
            .filter(|stored| matches!(&stored.record.kind, RecordKind::OperationFinished { outcome: cocoa_core::capability::OperationOutcome::Completed { value }, .. } if value["file"] == "updated"))
            .count(),
        1
    );
    let requests = model.requests.lock().await;
    let first = requests
        .iter()
        .find(|request| {
            request
                .context
                .checkpoint
                .as_ref()
                .is_some_and(|checkpoint| checkpoint.predecessor.is_none())
        })
        .ok_or("first compacted continuation missing")?;
    let second = requests
        .iter()
        .find(|request| {
            request
                .context
                .checkpoint
                .as_ref()
                .is_some_and(|checkpoint| checkpoint.predecessor.is_some())
        })
        .ok_or("second compacted continuation missing")?;
    assert_eq!(request_prints(first), vec![large_second]);
    assert_eq!(request_prints(second), vec![json!(51), large_third]);
    for request in &*requests {
        assert!(compaction_request_bytes(request)? <= policy.request_bytes);
    }
    for request in [first, second] {
        assert!(!request.context.records.iter().any(|record| matches!(
            &record.kind,
            RecordKind::InputAccepted { input } if input.text() == COMPACTION_DIRECTIONS
        )));
        assert!(!request.context.records.iter().any(|record| matches!(
            &record.kind,
            RecordKind::OperationStarted { .. } | RecordKind::OperationFinished { .. }
        )));
        assert_eq!(
            request
                .context
                .records
                .iter()
                .filter(|record| matches!(
                    &record.kind,
                    RecordKind::ModelCompleted {
                        response: ModelResponse {
                            action: ModelAction::Execute { .. },
                            ..
                        },
                        ..
                    }
                ))
                .count(),
            1
        );
        assert!(!request.context.records.iter().any(|record| matches!(
            &record.kind,
            RecordKind::ModelCompleted { response: ModelResponse { action: ModelAction::Execute { cells }, .. }, .. }
                if cells.iter().any(|cell| cell.code.contains("var retained"))
        )));
        let checkpoint = request
            .context
            .checkpoint
            .as_ref()
            .ok_or("checkpoint missing from continuation")?;
        assert_eq!(checkpoint.required.len(), 2);
        let prohibition = &checkpoint.required[0].content;
        assert_eq!(prohibition.scope, "Never send the report.");
        assert_eq!(prohibition.request, None);
        assert_eq!(
            prohibition.origin,
            RequiredOrigin::User {
                quotations: vec![cocoa_core::compaction::Quotation {
                    source: original.id,
                    text: "Never send the report.".into(),
                }],
            }
        );
        let paused = &checkpoint.required[1].content;
        assert_eq!(paused.scope, "Publish the report.");
        let Some(RequestState::Paused {
            instruction,
            resume_when,
        }) = &paused.request
        else {
            panic!("the user's pause was lost: {paused:?}");
        };
        assert_eq!(resume_when, "user says 'release report'");
        assert_eq!(
            instruction,
            &RequiredOrigin::User {
                quotations: vec![cocoa_core::compaction::Quotation {
                    source: original.id,
                    text: "Pause publishing until I say 'release report'.".into(),
                }],
            }
        );
        assert_eq!(checkpoint.recap.current_stage.as_deref(), Some("maro"));
        let jev = checkpoint
            .recap
            .stages
            .iter()
            .find(|stage| stage.key == "jev")
            .ok_or("future Jev stage missing")?;
        assert_eq!(jev.text, "Inspect Jev after Maro is verified.");
        assert_eq!(jev.status, RecapStatus::Deferred);
        assert_eq!(jev.prerequisites, ["maro"]);
        assert_eq!(jev.revisit_when.as_deref(), Some("Maro is verified"));
        assert_eq!(
            checkpoint.recap.progress[0].text,
            "The report write already completed; do not repeat it."
        );
        assert!(
            request
                .context
                .instructions
                .contains("did not reset or replay")
        );
    }
    drop(requests);
    let summaries = model.summaries.lock().await;
    assert_eq!(summaries.len(), 2);
    assert_eq!(summaries[0]["previous_required"], json!([]));
    assert_eq!(
        summaries[1]["previous_required"].as_array().map(Vec::len),
        Some(2)
    );
    assert_ne!(
        summary_source(&summaries[0], "user", COMPACTION_DIRECTIONS)?,
        summary_source(&summaries[1], "user", COMPACTION_DIRECTIONS)?,
    );
    let sources = summaries[0]["sources"]
        .as_array()
        .ok_or("missing sources")?;
    assert_eq!(
        sources
            .iter()
            .filter(|source| source["kind"] == "cell_result")
            .count(),
        2
    );
    assert!(sources.iter().any(
        |source| source["kind"] == "cell_result" && source["outcome"]["printed"] == json!([52])
    ));
    assert!(sources.iter().any(|source| source["kind"] == "cell_result"
        && source["outcome"]["printed"] == json!([large_first])));
    drop(summaries);
    engine.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn automatic_compaction_rejects_bad_proposals_and_only_falls_back_below_hard_budget()
-> Result<(), Box<dyn Error>> {
    for reply in [
        SummaryReply::ForeignSource,
        SummaryReply::Truncated,
        SummaryReply::Expanded,
    ] {
        for request_bytes in [20_000, 5_000] {
            let model = Arc::new(CompactingModel::new(
                vec![
                    execute(&["print('original:' + 'x'.repeat(6000))"]),
                    execute(&["print('latest settled')"]),
                    ModelAction::Final {
                        answer: "settled".into(),
                    },
                ],
                reply,
            ));
            let (mut engine, store) = compacting_engine(
                model.clone(),
                compaction_policy(request_bytes),
                Arc::new(Host),
            )
            .await?;
            let result = tokio::time::timeout(
                Duration::from_secs(5),
                engine.run(
                    RunRequest {
                        input: "run".into(),
                    },
                    CancellationToken::new(),
                ),
            )
            .await?;
            let requests = model.requests.lock().await;
            assert!(
                requests
                    .iter()
                    .all(|request| request.context.checkpoint.is_none())
            );
            assert_eq!(
                model.summaries.lock().await.len(),
                1,
                "retrying unchanged evidence: {reply:?}"
            );
            assert!(store.load().await?.checkpoints.is_empty());
            let large = json!(format!("original:{}", "x".repeat(6000)));
            if request_bytes == 20_000 {
                assert_eq!(result?.answer, "settled");
                assert_eq!(requests.len(), 3);
                let continuation = requests.last().ok_or("missing fallback request")?;
                assert_eq!(
                    request_prints(continuation),
                    vec![large.clone(), json!("latest settled")]
                );
                assert!(compaction_request_bytes(continuation)? > 4_000);
                assert!(compaction_request_bytes(continuation)? <= request_bytes);
            } else {
                assert!(
                    matches!(result, Err(cocoa_core::engine::RunError::Compaction(_))),
                    "expected an explicit hard-budget failure for {reply:?}: {result:?}"
                );
                assert_eq!(requests.len(), 1);
                assert!(store.load().await?.records.iter().any(|stored| matches!(
                    stored.record.kind,
                    RecordKind::InvocationFinished {
                        outcome: cocoa_core::store::InvocationOutcome::Failed { .. }
                    }
                )));
            }
            assert_eq!(
                outcomes(&store).await?[0],
                CellOutcome::Completed {
                    printed: vec![large]
                }
            );
            drop(requests);
            engine.shutdown().await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn automatic_compaction_can_cover_the_latest_settled_group_to_fit_a_hard_budget()
-> Result<(), Box<dyn Error>> {
    let model = Arc::new(CompactingModel::new(
        vec![
            execute(&[
                "var retained = (() => { let value = 23; return (left, right) => value += left * 7 - right * 3; })(); print('x'.repeat(6000))",
            ]),
            execute(&["print(retained(4, 7))"]),
            ModelAction::Final {
                answer: "settled".into(),
            },
        ],
        SummaryReply::Preserve,
    ));
    let policy = compaction_policy(5_000);
    let (mut engine, store) = compacting_engine(model.clone(), policy, Arc::new(Host)).await?;
    run(&mut engine).await?;
    let requests = model.requests.lock().await;
    let continuation = requests
        .iter()
        .find(|request| request.context.checkpoint.is_some())
        .ok_or("hard-budget compaction did not run")?;
    assert!(request_prints(continuation).is_empty());
    assert!(
        !continuation
            .context
            .records
            .iter()
            .any(|record| matches!(record.kind, RecordKind::ModelCompleted { .. }))
    );
    assert!(compaction_request_bytes(continuation)? <= policy.request_bytes);
    assert_eq!(store.load().await?.checkpoints.len(), 1);
    assert_eq!(
        outcomes(&store).await?,
        vec![
            CellOutcome::Completed {
                printed: vec![json!("x".repeat(6000))]
            },
            CellOutcome::Completed {
                printed: vec![json!(30)]
            },
        ]
    );
    drop(requests);
    engine.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn automatic_compaction_delivers_steering_queued_during_summary_before_responding()
-> Result<(), Box<dyn Error>> {
    let gate = Arc::new(SummaryGate::default());
    let model = Arc::new(CompactingModel {
        gate: Some(gate.clone()),
        ..CompactingModel::new(
            vec![
                execute(&["print('x'.repeat(6000))"]),
                execute(&["print('latest settled')"]),
                ModelAction::Final {
                    answer: "settled".into(),
                },
            ],
            SummaryReply::Preserve,
        )
    });
    let (mut engine, store) =
        compacting_engine(model.clone(), compaction_policy(20_000), Arc::new(Host)).await?;
    let (sender, mut steering) = tokio::sync::mpsc::channel(1);
    let (result, admitted) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(
            engine.run_with_steering(
                RunRequest {
                    input: "run".into()
                },
                CancellationToken::new(),
                &mut steering
            ),
            async {
                gate.started.notified().await;
                let next_response = model.requests.lock().await.len();
                sender
                    .send("Keep publishing paused; inspect Jev later.".into())
                    .await?;
                gate.release.notify_one();
                Ok::<_, Box<dyn Error>>(next_response)
            }
        )
    })
    .await?;
    let next_response = admitted?;
    assert_eq!(result?.answer, "settled");
    let requests = model.requests.lock().await;
    assert_eq!(requests.len(), 3);
    let continuation = requests
        .get(next_response)
        .ok_or("missing steered continuation")?;
    assert!(continuation.context.checkpoint.is_some());
    assert_eq!(continuation.context.records.iter().filter(|record| matches!(
        &record.kind,
        RecordKind::InputAccepted { input } if input.text() == "Keep publishing paused; inspect Jev later."
    )).count(), 1);
    assert_eq!(store.load().await?.records.iter().filter(|stored| matches!(
        &stored.record.kind,
        RecordKind::InputAccepted { input } if input.text() == "Keep publishing paused; inspect Jev later."
    )).count(), 1);
    assert_eq!(model.summaries.lock().await.len(), 1);
    drop(requests);
    engine.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn automatic_compaction_cancellation_during_summary_publishes_nothing_and_runs_no_more_cells()
-> Result<(), Box<dyn Error>> {
    let gate = Arc::new(SummaryGate::default());
    let model = Arc::new(CompactingModel {
        gate: Some(gate.clone()),
        ..CompactingModel::new(
            vec![
                execute(&["print('x'.repeat(6000))"]),
                execute(&["print('latest settled')"]),
                execute(&["await capabilities.call('echo', 'must not run')"]),
            ],
            SummaryReply::Preserve,
        )
    });
    let mut limits = engine_limits();
    limits.stall_timeout = Duration::from_millis(50);
    let store = Arc::new(InMemoryStore::new(StoreLimits {
        record_bytes: limits.record_bytes,
    })?);
    let mut engine = compacting_engine_with_settings(
        model.clone(),
        EngineSettings::new(limits)?.with_compaction(compaction_policy(20_000))?,
        Arc::new(Host),
        store.clone(),
    )
    .await?;
    let cancellation = CancellationToken::new();
    let (result, settled) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(
            engine.run(
                RunRequest {
                    input: "run".into()
                },
                cancellation.clone()
            ),
            async {
                gate.started.notified().await;
                let feedback = outcomes(&store).await?;
                let responses = model.requests.lock().await.len();
                tokio::time::sleep(Duration::from_millis(120)).await;
                cancellation.cancel();
                Ok::<_, Box<dyn Error>>((feedback, responses))
            }
        )
    })
    .await?;
    let (settled_feedback, responses) = settled?;
    assert!(
        matches!(
            result,
            Err(cocoa_core::engine::RunError::TurnCancelled { .. })
        ),
        "{result:?}"
    );
    let snapshot = store.load().await?;
    assert!(snapshot.checkpoints.is_empty());
    assert!(
        !snapshot
            .records
            .iter()
            .any(|stored| matches!(stored.record.kind, RecordKind::OperationStarted { .. }))
    );
    assert_eq!(
        settled_feedback.first(),
        Some(&CellOutcome::Completed {
            printed: vec![json!("x".repeat(6000))]
        })
    );
    assert_eq!(outcomes(&store).await?, settled_feedback);
    assert_eq!(model.requests.lock().await.len(), responses);
    assert_eq!(model.summaries.lock().await.len(), 1);
    engine.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn automatic_compaction_uses_its_deadline_and_preserves_bounded_fallback()
-> Result<(), Box<dyn Error>> {
    for (delay_ms, timeout_ms, request_bytes, publishes) in [
        (120, 400, 20_000, true),
        (400, 180, 20_000, false),
        (400, 180, 5_000, false),
    ] {
        let model = Arc::new(CompactingModel {
            delay: Duration::from_millis(delay_ms),
            ..CompactingModel::new(
                vec![
                    execute(&["print('x'.repeat(6000))"]),
                    ModelAction::Final {
                        answer: "settled".into(),
                    },
                ],
                SummaryReply::Preserve,
            )
        });
        let store = Arc::new(InMemoryStore::new(StoreLimits {
            record_bytes: 32_768,
        })?);
        let mut limits = engine_limits();
        limits.stall_timeout = Duration::from_millis(50);
        let policy = CompactionPolicy {
            timeout: Duration::from_millis(timeout_ms),
            target_bytes: 100,
            ..compaction_policy(request_bytes)
        };
        let mut engine = compacting_engine_with_settings(
            model.clone(),
            EngineSettings::new(limits)?.with_compaction(policy)?,
            Arc::new(Host),
            store.clone(),
        )
        .await?;
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            engine.run(
                RunRequest {
                    input: "run".into(),
                },
                CancellationToken::new(),
            ),
        )
        .await?;
        if request_bytes == 5_000 {
            assert!(
                matches!(
                    result,
                    Err(cocoa_core::engine::RunError::Compaction(
                        cocoa_core::engine::CompactionError::Model(ModelError::Stalled)
                    ))
                ),
                "{result:?}"
            );
            assert_eq!(model.requests.lock().await.len(), 1);
        } else {
            assert_eq!(result?.answer, "settled");
            let requests = model.requests.lock().await;
            let continuation = requests.last().ok_or("missing continuation")?;
            assert_eq!(continuation.context.checkpoint.is_some(), publishes);
            assert!(compaction_request_bytes(continuation)? > policy.target_bytes);
        }
        assert_eq!(
            store.load().await?.checkpoints.len(),
            usize::from(publishes)
        );
        assert_eq!(model.summaries.lock().await.len(), 1);
        engine.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn automatic_compaction_never_generates_without_accounting_even_on_context_overflow()
-> Result<(), Box<dyn Error>> {
    for context_bytes in [4_000, 512_000] {
        let model = Arc::new(CompactingModel {
            accounting: false,
            ..CompactingModel::new(
                vec![
                    execute(&["print('x'.repeat(6000))"]),
                    ModelAction::Final {
                        answer: "settled".into(),
                    },
                ],
                SummaryReply::Preserve,
            )
        });
        let store = Arc::new(InMemoryStore::new(StoreLimits {
            record_bytes: 32_768,
        })?);
        let mut limits = engine_limits();
        limits.context_bytes = context_bytes;
        let mut engine = compacting_engine_with_settings(
            model.clone(),
            EngineSettings::new(limits)?.with_compaction(compaction_policy(20_000))?,
            Arc::new(Host),
            store.clone(),
        )
        .await?;
        let result = engine
            .run(
                RunRequest {
                    input: "run".into(),
                },
                CancellationToken::new(),
            )
            .await;
        if context_bytes == 4_000 {
            assert!(
                matches!(
                    result,
                    Err(cocoa_core::engine::RunError::Context(
                        cocoa_core::context::ContextError::Limit
                    ))
                ),
                "{result:?}"
            );
        } else {
            assert_eq!(result?.answer, "settled");
        }
        assert!(model.summaries.lock().await.is_empty());
        assert!(store.load().await?.checkpoints.is_empty());
        engine.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn automatic_compaction_publication_failure_survives_concurrent_cancellation()
-> Result<(), Box<dyn Error>> {
    use cocoa_core::{
        engine::{CompactionError, RunError},
        store::StoreError,
    };
    for commit in [false, true] {
        let model = Arc::new(CompactingModel::new(
            vec![
                execute(&["print('x'.repeat(6000))"]),
                ModelAction::Final {
                    answer: "must not run".into(),
                },
            ],
            SummaryReply::Preserve,
        ));
        let inner = Arc::new(InMemoryStore::new(StoreLimits {
            record_bytes: 32_768,
        })?);
        let cancellation = CancellationToken::new();
        let store = Arc::new(CheckpointFailureStore {
            inner: inner.clone(),
            commit,
            cancellation: cancellation.clone(),
        });
        let mut engine = compacting_engine_with_settings(
            model.clone(),
            EngineSettings::new(engine_limits())?.with_compaction(compaction_policy(20_000))?,
            Arc::new(Host),
            store,
        )
        .await?;
        let result = engine
            .run(
                RunRequest {
                    input: "run".into(),
                },
                cancellation,
            )
            .await;
        assert!(
            matches!(
                result,
                Err(RunError::Compaction(CompactionError::Store(
                    StoreError::Backend { .. }
                )))
            ),
            "{result:?}"
        );
        assert!(matches!(
            engine
                .run(
                    RunRequest {
                        input: "no replay".into()
                    },
                    CancellationToken::new()
                )
                .await,
            Err(RunError::Stopped)
        ));
        assert_eq!(model.requests.lock().await.len(), 1);
        assert_eq!(inner.load().await?.checkpoints.len(), usize::from(commit));
        engine.shutdown().await?;
    }
    Ok(())
}

struct CheckpointFailureStore {
    inner: Arc<InMemoryStore>,
    commit: bool,
    cancellation: CancellationToken,
}

#[async_trait]
impl Store for CheckpointFailureStore {
    async fn append(
        &self,
        record: &cocoa_core::store::Record,
        instruction_state: Option<&cocoa_core::context::InstructionState>,
    ) -> Result<cocoa_core::store::RecordId, cocoa_core::store::StoreError> {
        self.inner.append(record, instruction_state).await
    }

    async fn publish_checkpoint(
        &self,
        checkpoint: &cocoa_core::compaction::Checkpoint,
    ) -> Result<cocoa_core::store::RecordId, cocoa_core::store::StoreError> {
        if self.commit {
            self.inner.publish_checkpoint(checkpoint).await?;
        }
        self.cancellation.cancel();
        Err(cocoa_core::store::StoreError::Backend {
            message: "unknown checkpoint publication outcome".into(),
        })
    }

    async fn load(
        &self,
    ) -> Result<cocoa_core::store::StoreSnapshot, cocoa_core::store::StoreError> {
        self.inner.load().await
    }
}

const COMPACTION_DIRECTIONS: &str = "Never send the report. Publish the report. Pause publishing until I say 'release report'. Inspect Maro now, then inspect Jev after Maro is verified.";
const COMPACTION_PLAN: &str =
    "Inspect Maro first. Inspect Jev after Maro is verified; publishing remains paused.";

#[derive(Clone, Copy, Debug)]
enum SummaryReply {
    Preserve,
    ForeignSource,
    Truncated,
    Expanded,
}

#[derive(Default)]
struct SummaryGate {
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

struct CompactingModel {
    actions: Vec<ModelAction>,
    requests: tokio::sync::Mutex<Vec<ModelRequest>>,
    summaries: tokio::sync::Mutex<Vec<Value>>,
    reply: SummaryReply,
    gate: Option<Arc<SummaryGate>>,
    delay: Duration,
    accounting: bool,
}

impl CompactingModel {
    fn new(actions: Vec<ModelAction>, reply: SummaryReply) -> Self {
        Self {
            actions,
            requests: tokio::sync::Mutex::new(Vec::new()),
            summaries: tokio::sync::Mutex::new(Vec::new()),
            reply,
            gate: None,
            delay: Duration::ZERO,
            accounting: true,
        }
    }
}

#[async_trait]
impl ModelProvider for CompactingModel {
    fn request_bytes(&self, request: &ModelRequest) -> Result<Option<usize>, ModelError> {
        if !self.accounting {
            return Ok(None);
        }
        compaction_request_bytes(request).map(Some)
    }

    async fn summarize(
        &self,
        request: SummaryRequest,
        cancellation: CancellationToken,
    ) -> Result<String, ModelError> {
        let input: Value =
            serde_json::from_str(&request.input).map_err(|error| ModelError::Protocol {
                message: error.to_string(),
            })?;
        self.summaries.lock().await.push(input.clone());
        tokio::time::sleep(self.delay).await;
        if let Some(gate) = &self.gate {
            gate.started.notify_one();
            tokio::select! {
                () = cancellation.cancelled() => return Err(ModelError::Cancelled),
                () = gate.release.notified() => {}
            }
        }
        if matches!(self.reply, SummaryReply::Truncated) {
            return Ok(r#"{"additions":[],"changes":[],"recap":{"goal":"unfinished""#.into());
        }
        let mut proposal = compaction_proposal(&input)?;
        match self.reply {
            SummaryReply::ForeignSource => proposal["evidence"] = json!([i64::MAX]),
            SummaryReply::Expanded => {
                proposal["recap"]["goal"] = json!("Retain the full observed evidence. ".repeat(300))
            }
            SummaryReply::Preserve | SummaryReply::Truncated => {}
        }
        Ok(proposal.to_string())
    }

    async fn respond(
        &self,
        request: ModelRequest,
        _: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        let mut requests = self.requests.lock().await;
        let action =
            self.actions
                .get(requests.len())
                .cloned()
                .ok_or_else(|| ModelError::Protocol {
                    message: "unexpected extra response request".into(),
                })?;
        requests.push(request);
        Ok(ModelResponse {
            action,
            continuation: None,
            usage: None,
        })
    }
}

fn compaction_policy(request_bytes: usize) -> CompactionPolicy {
    CompactionPolicy {
        request_bytes,
        trigger_bytes: 4_000,
        target_bytes: 2_000,
        summary_input_bytes: 128_000,
        summary_output_tokens: 4096,
        sources: 128,
        checkpoint: CheckpointLimits {
            bytes: 16_384,
            required_entries: 16,
            items: 32,
        },
        timeout: Duration::from_secs(1),
    }
}

async fn compacting_engine(
    model: Arc<CompactingModel>,
    policy: CompactionPolicy,
    capabilities: Arc<dyn CapabilityHost>,
) -> Result<(Engine, Arc<InMemoryStore>), Box<dyn Error>> {
    let limits = engine_limits();
    let store = Arc::new(InMemoryStore::new(StoreLimits {
        record_bytes: limits.record_bytes,
    })?);
    let engine = compacting_engine_with_settings(
        model,
        EngineSettings::new(limits)?.with_compaction(policy)?,
        capabilities,
        store.clone(),
    )
    .await?;
    Ok((engine, store))
}

async fn compacting_engine_with_settings(
    model: Arc<dyn ModelProvider>,
    settings: EngineSettings,
    capabilities: Arc<dyn CapabilityHost>,
    store: Arc<dyn Store>,
) -> Result<Engine, Box<dyn Error>> {
    let executor = QuickJsExecutor::new(QuickJsOptions {
        memory_bytes: 8 * 1024 * 1024,
        stack_bytes: 256 * 1024,
        feedback: feedback_limits(),
    })
    .await?;
    Ok(Engine::new(
        settings,
        EngineAdapters {
            model,
            executor: Box::new(executor),
            capabilities,
            context: Arc::new(BoundedContext),
            store,
        },
        None,
    ))
}

fn execute(cells: &[&str]) -> ModelAction {
    ModelAction::Execute {
        cells: cells
            .iter()
            .map(|code| Cell {
                code: (*code).into(),
            })
            .collect(),
    }
}

fn compaction_request_bytes(request: &ModelRequest) -> Result<usize, ModelError> {
    serde_json::to_vec(&json!({
        "records": request.context.records,
        "capabilities": request.context.capabilities,
        "checkpoint": request.context.checkpoint,
        "instructions": request.context.instructions,
        "feedback_notes": request.context.feedback_notes,
        "shared": request.context.shared,
        "executor_description": request.executor_description,
        "output_tokens": 512,
    }))
    .map(|encoded| encoded.len())
    .map_err(|error| ModelError::Protocol {
        message: error.to_string(),
    })
}

fn request_prints(request: &ModelRequest) -> Vec<Value> {
    request
        .context
        .records
        .iter()
        .filter_map(|record| match &record.kind {
            RecordKind::CellCompleted {
                outcome: CellOutcome::Completed { printed },
                ..
            } => Some(printed.as_slice()),
            _ => None,
        })
        .flatten()
        .cloned()
        .collect()
}

fn compaction_proposal(input: &Value) -> Result<Value, ModelError> {
    let mut proposal = json!({
        "additions": [], "changes": [], "evidence": [],
        "recap": {
            "goal": "Continue without replaying settled work.", "stages": [], "current_stage": null,
            "decisions": [], "progress": [], "failures": [], "next_actions": [],
        },
    });
    if input["previous_recap"].is_object() {
        proposal["recap"] = input["previous_recap"].clone();
        proposal["evidence"] = input["previous_evidence"].clone();
    } else if input["sources"].as_array().is_some_and(|sources| {
        sources
            .iter()
            .any(|source| source["kind"] == "user" && source["text"] == COMPACTION_DIRECTIONS)
    }) {
        let user = summary_source(input, "user", COMPACTION_DIRECTIONS)?;
        let assistant = summary_source(input, "assistant", COMPACTION_PLAN)?;
        let effect = summary_source(input, "operation_result", "")?;
        let prohibition =
            json!({"User":{"quotations":[{"source":user,"text":"Never send the report."}]}});
        let pause = json!({"User":{"quotations":[{"source":user,"text":"Pause publishing until I say 'release report'."}]}});
        proposal["additions"] = json!([
            {"origin":prohibition,"scope":"Never send the report.","request":null},
            {"origin":{"User":{"quotations":[{"source":user,"text":"Publish the report."}]}},"scope":"Publish the report.","request":{"Paused":{"instruction":pause,"resume_when":"user says 'release report'"}}},
        ]);
        proposal["recap"] = json!({
            "goal":"Inspect Maro and then Jev without sending the report.",
            "stages":[
                {"key":"maro","text":"Inspect Maro.","status":"Active","prerequisites":[],"sources":[user],"revisit_when":null},
                {"key":"jev","text":"Inspect Jev after Maro is verified.","status":"Deferred","prerequisites":["maro"],"sources":[assistant],"revisit_when":"Maro is verified"},
            ],
            "current_stage":"maro","decisions":[],
            "progress":[{"text":"The report write already completed; do not repeat it.","sources":[effect]}],
            "failures":[],"next_actions":["Continue Maro; do not publish until the user says 'release report'."],
        });
        proposal["evidence"] = json!([effect]);
    }
    Ok(proposal)
}

fn summary_source(input: &Value, kind: &str, text: &str) -> Result<Value, ModelError> {
    input["sources"]
        .as_array()
        .and_then(|sources| {
            sources.iter().find(|source| {
                source["kind"] == kind && (text.is_empty() || source["text"] == text)
            })
        })
        .map(|source| source["source"].clone())
        .ok_or_else(|| ModelError::Protocol {
            message: format!("missing {kind} summary evidence"),
        })
}
