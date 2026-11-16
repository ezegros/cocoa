//! Engine lifecycle and canonical evidence contracts with injected adapters.

use std::{error::Error, sync::Arc, time::Duration};

use async_trait::async_trait;
use cocoa_core::{
    capability::{
        ActivityOutput, ActivityOutputKind, CapabilityDescription, CapabilityError, CapabilityHost,
        OperationContext, OperationOutcome,
    },
    compaction::{Checkpoint, CheckpointLimits, CheckpointProposal, CompactionPolicy, Recap},
    context::BoundedContext,
    engine::{Engine, EngineAdapters, EngineSettings, Limits, RunRequest},
    execution::{
        Cell, CellExecutor, CellResponse as CellOutcome, ExecutionError, FeedbackError,
        OperationEffects, ShutdownReport, WorkLimits,
    },
    model::{
        ModelAction, ModelError, ModelProvider, ModelRequest, ModelResponse, ProviderData,
        SummaryRequest,
    },
    store::{
        ActiveSnapshot, InMemoryStore, InvocationId, InvocationOutcome, Record, RecordId,
        RecordKind, Store, StoreError, StoreLimits, StoreSnapshot,
    },
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

#[test]
fn host_payload_fit_accounts_for_encoded_keys_bytes_nodes_and_depth() {
    use cocoa_core::execution::host_value_fits;
    use serde_json::Value;

    assert!(host_value_fits(&json!({"é":"\n"}), 11));
    assert!(!host_value_fits(&json!({"é":"\n"}), 10));
    assert!(host_value_fits(
        &json!("x".repeat(256 * 1024 - 2)),
        512 * 1024
    ));
    assert!(!host_value_fits(
        &json!("x".repeat(256 * 1024 - 1)),
        512 * 1024
    ));
    assert!(!host_value_fits(&json!({"a".repeat(100):null}), 64));
    assert!(host_value_fits(&json!(vec![0; 4095]), 256 * 1024));
    assert!(!host_value_fits(&json!(vec![0; 4096]), 256 * 1024));
    let mut nested = Value::Null;
    for _ in 0..32 {
        nested = json!([nested]);
    }
    assert!(host_value_fits(&nested, 256 * 1024));
    assert!(!host_value_fits(&json!([nested]), 256 * 1024));
}

struct ArithmeticModel;

#[async_trait]
impl ModelProvider for ArithmeticModel {
    async fn respond(
        &self,
        request: ModelRequest,
        _: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        let context = request.context;
        if let Some(result) = context
            .records
            .iter()
            .find_map(|record| match &record.kind {
                RecordKind::CellCompleted { outcome, .. } => Some(outcome),
                _ => None,
            })
        {
            assert_eq!(
                result,
                &CellOutcome::Completed {
                    printed: vec![json!(23)]
                }
            );
            assert!(context.records.iter().any(|record| matches!(&record.kind,
            RecordKind::ModelCompleted { response } if response.continuation == Some(ProviderData {
                provider: "fixture".into(), data: json!({"opaque": [7, "keep"]})
            }))));
            Ok(ModelResponse {
                action: ModelAction::Final {
                    answer: "23, not 32".into(),
                },
                continuation: None,
                usage: None,
            })
        } else {
            Ok(ModelResponse {
                action: ModelAction::Execute {
                    cells: vec![Cell {
                        code: "multiply(7, 3) + 2".into(),
                    }],
                },
                continuation: Some(ProviderData {
                    provider: "fixture".into(),
                    data: json!({"opaque": [7, "keep"]}),
                }),
                usage: None,
            })
        }
    }
}

struct ArithmeticExecutor;

#[async_trait]
impl CellExecutor for ArithmeticExecutor {
    async fn execute(
        &mut self,
        cell: Cell,
        operations: &OperationContext,
        _: CancellationToken,
    ) -> Result<CellOutcome, ExecutionError> {
        assert_eq!(cell.code, "multiply(7, 3) + 2");
        let value = operations
            .call("multiply", json!({"left":7,"right":3}))
            .await?;
        assert_eq!(value, json!(21));
        Ok(CellOutcome::Completed {
            printed: vec![json!(23)],
        })
    }
    async fn shutdown(&mut self, _: CancellationToken) -> Result<ShutdownReport, ExecutionError> {
        Ok(ShutdownReport { unresolved_work: 0 })
    }
}

struct ArithmeticHost;

#[async_trait]
impl CapabilityHost for ArithmeticHost {
    fn descriptions(&self) -> Vec<CapabilityDescription> {
        vec![CapabilityDescription {
            name: "multiply".into(),
            description: "Multiply two integers".into(),
            parameters: json!({"type":"object"}),
        }]
    }
    async fn call(
        &self,
        name: &str,
        arguments: serde_json::Value,
        _: WorkLimits,
        _: CancellationToken,
    ) -> Result<serde_json::Value, CapabilityError> {
        assert_eq!(name, "multiply");
        assert_eq!(arguments, json!({"left":7,"right":3}));
        Ok(json!(21))
    }

    fn describe_output(
        &self,
        _: &str,
        _: &serde_json::Value,
        outcome: &OperationOutcome,
    ) -> Option<ActivityOutput> {
        let OperationOutcome::Completed { value } = outcome else {
            return None;
        };
        Some(ActivityOutput {
            text: format!("λ result: {value}"),
            kind: ActivityOutputKind::Command,
            summary: None,
            truncated: false,
        })
    }
}

struct RecoveringModel {
    attempts: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl ModelProvider for RecoveringModel {
    async fn respond(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        use std::sync::atomic::Ordering;
        match self.attempts.fetch_add(1, Ordering::SeqCst) {
            1 => Err(ModelError::Transport {
                message: "rejected".into(),
            }),
            2 => {
                request
                    .observations
                    .assistant_text("discard this partial answer");
                Err(ModelError::Protocol {
                    message: "incomplete stream".into(),
                })
            }
            3 => Err(ModelError::Stalled),
            4 => {
                cancellation.cancelled().await;
                Err(ModelError::Cancelled)
            }
            5 => Ok(ModelResponse {
                action: ModelAction::Execute { cells: Vec::new() },
                continuation: None,
                usage: None,
            }),
            _ => ArithmeticModel.respond(request, cancellation).await,
        }
    }
}

#[tokio::test]
async fn manual_provider_retries_preserve_completed_work_without_duplicating_responses()
-> Result<(), Box<dyn Error>> {
    use cocoa_core::engine::ProviderFailure;
    use std::sync::atomic::Ordering;

    let store = store()?;
    let mut limits = settings()?.limits();
    limits.stall_timeout = Duration::from_millis(100);
    let model = Arc::new(RecoveringModel {
        attempts: std::sync::atomic::AtomicUsize::new(0),
    });
    let mut engine = Engine::new(
        EngineSettings::new(limits)?,
        EngineAdapters {
            model: model.clone(),
            executor: Box::new(ArithmeticExecutor),
            capabilities: Arc::new(ArithmeticHost),
            context: Arc::new(BoundedContext),
            store: store.clone(),
        },
        None,
    );
    let (_steering, mut messages) = tokio::sync::mpsc::channel(1);
    let (recovery, mut failures) = tokio::sync::mpsc::channel::<ProviderFailure>(1);
    let decisions = async {
        for expected in 1..=5 {
            let failure = tokio::time::timeout(Duration::from_secs(2), failures.recv())
                .await?
                .ok_or("missing provider failure")?;
            assert!(matches!(
                (&failure.error, expected),
                (ModelError::Transport { .. }, 1)
                    | (ModelError::Protocol { .. }, 2)
                    | (ModelError::Stalled, 3 | 4)
                    | (ModelError::Protocol { .. }, 5)
            ));
            tokio::time::sleep(Duration::from_millis(250)).await;
            assert_eq!(
                model.attempts.load(Ordering::SeqCst),
                expected + 1,
                "waiting for input must not retry or time out"
            );
            failure
                .retry
                .send(())
                .map_err(|()| "retry request ended while awaiting user input")?;
        }
        Ok::<_, Box<dyn Error>>(())
    };
    let (result, decisions) = tokio::join!(
        engine.run_with_recovery(
            RunRequest {
                input: "calculate".into()
            },
            CancellationToken::new(),
            &mut messages,
            &recovery
        ),
        decisions,
    );
    decisions?;
    assert_eq!(result?.answer, "23, not 32");
    let records = store.load().await?.into_records();
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record.kind, RecordKind::InputAccepted { .. }))
            .count(),
        1
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record.kind, RecordKind::CellCompleted { .. }))
            .count(),
        1
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record.kind, RecordKind::ModelCompleted { .. }))
            .count(),
        2
    );
    assert!(!serde_json::to_string(&records)?.contains("discard this partial answer"));
    engine.shutdown().await?;
    Ok(())
}

struct DescribedExecutor(String);

#[async_trait]
impl CellExecutor for DescribedExecutor {
    fn description(&self, _: WorkLimits) -> Result<String, ExecutionError> {
        Ok(self.0.clone())
    }

    async fn execute(
        &mut self,
        _: Cell,
        _: &OperationContext,
        _: CancellationToken,
    ) -> Result<CellOutcome, ExecutionError> {
        panic!("description-only control must not execute")
    }

    async fn shutdown(&mut self, _: CancellationToken) -> Result<ShutdownReport, ExecutionError> {
        Ok(ShutdownReport { unresolved_work: 0 })
    }
}

#[tokio::test]
async fn executor_facts_count_encoded_bytes_and_reserve_context_before_model_work()
-> Result<(), Box<dyn Error>> {
    for (characters, context_bytes, accepted) in [
        (2047, 65536, true),
        (2048, 65536, false),
        (2047, 4096, false),
    ] {
        let store = store()?;
        let mut limits = settings()?.limits();
        limits.context_bytes = context_bytes;
        let mut engine = Engine::new(
            EngineSettings::new(limits)?,
            EngineAdapters {
                model: Arc::new(FixedModel(ModelResponse {
                    action: ModelAction::Final {
                        answer: "described".into(),
                    },
                    continuation: None,
                    usage: None,
                })),
                executor: Box::new(DescribedExecutor("\n".repeat(characters))),
                capabilities: Arc::new(ArithmeticHost),
                context: Arc::new(BoundedContext),
                store: store.clone(),
            },
            None,
        );
        let result = engine
            .run(
                RunRequest {
                    input: "facts".into(),
                },
                CancellationToken::new(),
            )
            .await;
        if accepted {
            assert_eq!(result?.answer, "described");
        } else {
            assert!(
                matches!(
                    result,
                    Err(cocoa_core::engine::RunError::Context(
                        cocoa_core::context::ContextError::Limit
                    ))
                ),
                "{result:?}"
            );
            assert!(
                !store
                    .load()
                    .await?
                    .into_records()
                    .iter()
                    .any(|record| matches!(record.kind, RecordKind::ModelCompleted { .. }))
            );
        }
        engine.shutdown().await?;
    }
    Ok(())
}

fn settings() -> Result<EngineSettings, Box<dyn Error>> {
    Ok(EngineSettings::new(Limits {
        input_bytes: 1024,
        output_bytes: 4096,
        record_bytes: 8192,
        context_bytes: 65536,
        pending_operations: 2,
        stall_timeout: Duration::from_secs(2),
        settlement_timeout: Duration::from_millis(100),
        shutdown_timeout: Duration::from_millis(100),
    })?)
}

fn store() -> Result<Arc<InMemoryStore>, Box<dyn Error>> {
    Ok(Arc::new(InMemoryStore::new(StoreLimits {
        record_bytes: 8192,
    })?))
}

fn engine(store: Arc<dyn Store>) -> Result<Engine, Box<dyn Error>> {
    Ok(Engine::new(
        settings()?,
        EngineAdapters {
            model: Arc::new(ArithmeticModel),
            executor: Box::new(ArithmeticExecutor),
            capabilities: Arc::new(ArithmeticHost),
            context: Arc::new(BoundedContext),
            store,
        },
        None,
    ))
}

#[tokio::test]
async fn model_cell_model_cycle_preserves_answer_and_operation_evidence()
-> Result<(), Box<dyn Error>> {
    let store = store()?;
    let mut engine = engine(store.clone())?;
    let output = engine
        .run(
            RunRequest {
                input: "seven times three plus two".into(),
            },
            CancellationToken::new(),
        )
        .await?;
    assert_eq!(output.answer, "23, not 32");
    let records = store.load().await?.into_records();
    assert!(
        matches!(&records[0].kind, RecordKind::InputAccepted { input } if input.text() == "seven times three plus two")
    );
    assert!(records.iter().any(|record| matches!(&record.kind, RecordKind::OperationStarted { name, arguments, .. } if name == "multiply" && arguments == &json!({"left":7,"right":3}))));
    assert!(records.iter().any(|record| matches!(&record.kind, RecordKind::OperationFinished { outcome: cocoa_core::capability::OperationOutcome::Completed { value }, .. } if value == &json!(21))));
    engine.shutdown().await?;
    Ok(())
}

struct SteeringModel {
    sender: tokio::sync::mpsc::Sender<cocoa_core::model::UserInput>,
    execute: bool,
}

#[async_trait]
impl ModelProvider for SteeringModel {
    async fn respond(
        &self,
        request: ModelRequest,
        _: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        let inputs: Vec<_> = request
            .context
            .records
            .iter()
            .filter_map(|record| match &record.kind {
                RecordKind::InputAccepted { input } => Some(input.text()),
                _ => None,
            })
            .collect();
        let action = if inputs == ["initial"] {
            assert!(self.sender.try_send("instead use λ".into()).is_ok());
            assert!(self.sender.try_send("then verify".into()).is_ok());
            if self.execute {
                ModelAction::Execute {
                    cells: vec![
                        Cell {
                            code: "multiply(7, 3) + 2".into(),
                        },
                        Cell {
                            code: "multiply(7, 3) + 2".into(),
                        },
                    ],
                }
            } else {
                ModelAction::Final {
                    answer: "superseded".into(),
                }
            }
        } else {
            assert_eq!(inputs, ["initial", "instead use λ", "then verify"]);
            ModelAction::Final {
                answer: "steered".into(),
            }
        };
        Ok(ModelResponse {
            action,
            continuation: None,
            usage: None,
        })
    }
}

#[tokio::test]
async fn steering_follows_complete_batches_and_extends_answers_without_resetting_budgets()
-> Result<(), Box<dyn Error>> {
    for execute in [true, false] {
        let store = store()?;
        let (sender, mut messages) = tokio::sync::mpsc::channel(2);
        let mut engine = Engine::new(
            settings()?,
            EngineAdapters {
                model: Arc::new(SteeringModel { sender, execute }),
                executor: Box::new(ArithmeticExecutor),
                capabilities: Arc::new(ArithmeticHost),
                context: Arc::new(BoundedContext),
                store: store.clone(),
            },
            None,
        );
        let result = engine
            .run_with_steering(
                RunRequest {
                    input: "initial".into(),
                },
                CancellationToken::new(),
                &mut messages,
            )
            .await;
        let output = result?;
        assert_eq!(output.answer, "steered");
        let records = store.load().await?.into_records();
        assert!(
            records
                .iter()
                .all(|record| record.invocation == output.invocation)
        );
        let steering = records.iter().position(|record| matches!(
            &record.kind, RecordKind::InputAccepted { input } if input.text() == "instead use λ"
        )).ok_or("missing steering")?;
        assert_eq!(
            records[..steering]
                .iter()
                .filter(|record| matches!(record.kind, RecordKind::CellCompleted { .. }))
                .count(),
            if execute { 2 } else { 0 }
        );
        assert!(messages.is_empty());
        engine.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn rejected_input_leaves_engine_and_store_usable() -> Result<(), Box<dyn Error>> {
    use cocoa_core::model::{Image, UserInput};

    let store = store()?;
    let mut engine = engine(store.clone())?;
    assert!(
        engine
            .run(
                RunRequest {
                    input: "é".repeat(513).into()
                },
                CancellationToken::new()
            )
            .await
            .is_err()
    );
    assert!(store.load().await?.into_records().is_empty());
    let image: Image = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a/aAAAAAASUVORK5CYII=".to_owned().try_into()?;
    let input = UserInput::WithImages {
        text: "x".repeat(1024 - image.data_url().len()),
        images: vec![image.clone()],
    };
    assert_eq!(input.bytes(), 1024);
    assert_eq!(
        serde_json::from_str::<UserInput>(&serde_json::to_string(&input)?)?,
        input
    );
    for input in [
        UserInput::WithImages {
            text: "x".repeat(1025 - image.data_url().len()),
            images: vec![image.clone()],
        },
        UserInput::WithImages {
            text: String::new(),
            images: vec![image; 5],
        },
    ] {
        assert!(matches!(
            engine
                .run(RunRequest { input }, CancellationToken::new())
                .await,
            Err(cocoa_core::engine::RunError::InputLimit)
        ));
        assert!(store.load().await?.into_records().is_empty());
    }
    let output = engine
        .run(RunRequest { input }, CancellationToken::new())
        .await?;
    assert_eq!(output.answer, "23, not 32");
    assert_eq!(output.invocation.0, 1);
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert!(matches!(
        engine
            .run(
                RunRequest {
                    input: "already cancelled".into()
                },
                cancellation
            )
            .await,
        Err(cocoa_core::engine::RunError::Cancelled)
    ));
    assert!(!store.load().await?.into_records().is_empty());
    let output = engine
        .run(
            RunRequest {
                input: "é".repeat(512).into(),
            },
            CancellationToken::new(),
        )
        .await?;
    assert_eq!(output.answer, "23, not 32");
    assert_eq!(output.invocation.0, 2);
    engine.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn observation_loss_never_changes_answers_or_records() -> Result<(), Box<dyn Error>> {
    use cocoa_core::engine::{EventKind, EventPublisher, Observation};
    let baseline_store = store()?;
    let mut baseline = engine(baseline_store.clone())?;
    let expected = baseline
        .run(
            RunRequest {
                input: "same request".into(),
            },
            CancellationToken::new(),
        )
        .await?;
    let expected_records = normalized_records(baseline_store.load().await?)?;
    baseline.shutdown().await?;
    for mode in ["consumed", "saturated", "disconnected"] {
        let store = store()?;
        let (publisher, mut receiver) =
            EventPublisher::channel(if mode == "consumed" { 32 } else { 1 }, 5)?;
        let mut engine = Engine::new(
            settings()?,
            EngineAdapters {
                model: Arc::new(ArithmeticModel),
                executor: Box::new(ArithmeticExecutor),
                capabilities: Arc::new(ArithmeticHost),
                context: Arc::new(BoundedContext),
                store: store.clone(),
            },
            Some(publisher),
        );
        let output = match mode {
            "consumed" => {
                let (output, ()) = tokio::time::timeout(Duration::from_secs(5), async {
                    tokio::join!(
                        engine.run(
                            RunRequest {
                                input: "same request".into()
                            },
                            CancellationToken::new()
                        ),
                        async {
                            loop {
                                match receiver.recv().await {
                                    Observation::Event(event) => {
                                        if let EventKind::AnswerCompleted { text, truncated } =
                                            event.kind
                                        {
                                            assert_eq!(event.invocation.0, 1);
                                            assert_eq!(text, "23, n");
                                            assert!(truncated);
                                            break;
                                        }
                                    }
                                    other => panic!("unexpected observation: {other:?}"),
                                }
                            }
                        }
                    )
                })
                .await?;
                output?
            }
            "saturated" => {
                let output = engine
                    .run(
                        RunRequest {
                            input: "same request".into(),
                        },
                        CancellationToken::new(),
                    )
                    .await?;
                assert!(
                    matches!(receiver.recv().await, Observation::Lagged { missed } if missed >= 3)
                );
                assert!(matches!(receiver.recv().await, Observation::Event(_)));
                output
            }
            _ => {
                drop(receiver);
                engine
                    .run(
                        RunRequest {
                            input: "same request".into(),
                        },
                        CancellationToken::new(),
                    )
                    .await?
            }
        };
        assert_eq!(output, expected, "{mode}");
        let snapshot = engine.activity_snapshot().await?;
        assert_eq!(snapshot.len(), 4, "{mode}");
        assert!(
            matches!(&snapshot[0].kind, EventKind::CapabilityStarted { operation, name, truncated: true } if operation.0 == 1 && name == "multi")
        );
        assert!(
            matches!(&snapshot[1].kind, EventKind::CapabilityInput { operation, title, text, summary: None, truncated: true } if operation.0 == 1 && title == "multi" && text == "{\"lef")
        );
        assert!(
            matches!(&snapshot[2].kind, EventKind::CapabilityOutput { operation, output } if operation.0 == 1 && output.text == "λ re" && output.truncated)
        );
        assert!(
            matches!(&snapshot[3].kind, EventKind::CapabilityFinished { operation, status: cocoa_core::engine::ActivityStatus::Completed } if operation.0 == 1)
        );
        engine.shutdown().await?;
        assert_eq!(
            normalized_records(store.load().await?)?,
            expected_records,
            "{mode}"
        );
    }
    Ok(())
}

struct PendingHost {
    started: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl CapabilityHost for PendingHost {
    fn descriptions(&self) -> Vec<CapabilityDescription> {
        ArithmeticHost.descriptions()
    }
    async fn call(
        &self,
        _: &str,
        _: serde_json::Value,
        _: WorkLimits,
        _: CancellationToken,
    ) -> Result<serde_json::Value, CapabilityError> {
        self.started.notify_one();
        std::future::pending().await
    }
}

#[tokio::test]
async fn cancellation_settles_pending_evidence_without_waiting_for_observers()
-> Result<(), Box<dyn Error>> {
    use cocoa_core::{
        capability::OperationOutcome,
        engine::{EventPublisher, RunError},
    };
    let store = store()?;
    let started = Arc::new(tokio::sync::Notify::new());
    let cancellation = CancellationToken::new();
    let (events, _unread) = EventPublisher::channel(1, 3)?;
    let mut engine = Engine::new(
        settings()?,
        EngineAdapters {
            model: Arc::new(ArithmeticModel),
            executor: Box::new(ArithmeticExecutor),
            capabilities: Arc::new(PendingHost {
                started: started.clone(),
            }),
            context: Arc::new(BoundedContext),
            store: store.clone(),
        },
        Some(events),
    );
    let (result, ()) = tokio::time::timeout(Duration::from_millis(300), async {
        tokio::join!(
            engine.run(
                RunRequest {
                    input: "cancel during external work".into()
                },
                cancellation.clone()
            ),
            async {
                started.notified().await;
                cancellation.cancel();
            }
        )
    })
    .await?;
    assert!(
        matches!(result, Err(RunError::Settlement { cause, .. }) if matches!(*cause, RunError::Cancelled))
    );
    assert!(matches!(
        engine
            .run(
                RunRequest {
                    input: "must not replay".into()
                },
                CancellationToken::new()
            )
            .await,
        Err(RunError::Stopped)
    ));
    engine.shutdown().await?;
    let records = store.load().await?.into_records();
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record.kind, RecordKind::OperationStarted { .. }))
            .count(),
        1
    );
    assert!(records.iter().any(|record| matches!(
        record.kind,
        RecordKind::OperationFinished {
            outcome: OperationOutcome::Unknown,
            ..
        }
    )));
    assert!(!records.iter().any(|record| matches!(
        record.kind,
        RecordKind::CellCompleted {
            outcome: CellOutcome::Completed { .. } | CellOutcome::Failed { .. },
            ..
        }
    )));
    Ok(())
}

struct FixedModel(ModelResponse);

#[async_trait]
impl ModelProvider for FixedModel {
    async fn respond(
        &self,
        _: ModelRequest,
        _: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        Ok(self.0.clone())
    }
}

#[tokio::test]
async fn admitted_budget_failures_stop_new_work_without_erasing_prior_effects()
-> Result<(), Box<dyn Error>> {
    use cocoa_core::engine::RunError;
    for resource in ["output", "record", "context"] {
        let store = store()?;
        let mut limits = settings()?.limits();
        let response = ModelResponse {
            action: ModelAction::Execute {
                cells: vec![Cell {
                    code: "multiply(7, 3) + 2".into(),
                }],
            },
            continuation: None,
            usage: None,
        };
        match resource {
            "output" => limits.output_bytes = 17,
            "record" => limits.record_bytes = 95,
            _ => limits.context_bytes = 19,
        }
        let mut engine = Engine::new(
            EngineSettings::new(limits)?,
            EngineAdapters {
                model: Arc::new(FixedModel(response)),
                executor: Box::new(ArithmeticExecutor),
                capabilities: Arc::new(ArithmeticHost),
                context: Arc::new(BoundedContext),
                store: store.clone(),
            },
            None,
        );
        let result = engine
            .run(
                RunRequest {
                    input: "bounded".into(),
                },
                CancellationToken::new(),
            )
            .await;
        let expected = match resource {
            "output" => matches!(result, Err(RunError::OutputLimit)),
            "record" => matches!(
                result,
                Err(RunError::Store(cocoa_core::store::StoreError::RecordLimit))
            ),
            _ => matches!(
                result,
                Err(RunError::Context(cocoa_core::context::ContextError::Limit))
            ),
        };
        assert!(expected, "{resource}: {result:?}");
        assert!(
            matches!(
                engine
                    .run(
                        RunRequest {
                            input: "no retry".into()
                        },
                        CancellationToken::new()
                    )
                    .await,
                Err(RunError::Stopped)
            ),
            "{resource}"
        );
        engine.shutdown().await?;
        let records = store.load().await?.into_records();
        let effects = records.iter().filter(|record| matches!(&record.kind, RecordKind::OperationFinished { outcome: cocoa_core::capability::OperationOutcome::Completed { value }, .. } if value == &json!(21))).count();
        assert_eq!(
            effects,
            if resource == "output" { 1 } else { 0 },
            "{resource}"
        );
    }
    Ok(())
}

struct FailureExecutor {
    fatal: bool,
}

#[async_trait]
impl CellExecutor for FailureExecutor {
    async fn execute(
        &mut self,
        _: Cell,
        operations: &OperationContext,
        _: CancellationToken,
    ) -> Result<CellOutcome, ExecutionError> {
        operations
            .call("multiply", json!({"left":7,"right":3}))
            .await?;
        if self.fatal {
            return Err(ExecutionError::Unavailable {
                message: "worker failed after effect".into(),
            });
        }
        Ok(CellOutcome::Failed {
            error: FeedbackError::Javascript {
                message: "reference error after effect".into(),
                truncated: false,
            },
            printed: Vec::new(),
        })
    }
    async fn shutdown(&mut self, _: CancellationToken) -> Result<ShutdownReport, ExecutionError> {
        Ok(ShutdownReport { unresolved_work: 0 })
    }
}

struct RecoveryModel;

#[async_trait]
impl ModelProvider for RecoveryModel {
    async fn respond(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        if let Some(error) = request
            .context
            .records
            .iter()
            .find_map(|record| match &record.kind {
                RecordKind::CellCompleted {
                    outcome: CellOutcome::Failed { error, .. },
                    ..
                } => Some(error),
                _ => None,
            })
        {
            let answer = match error {
                FeedbackError::Javascript { .. } => "21 was retained despite the later error",
                FeedbackError::Capability { .. } => "grant denied; no effect",
                _ => {
                    return Err(ModelError::Protocol {
                        message: "unexpected output error".into(),
                    });
                }
            };
            Ok(ModelResponse {
                action: ModelAction::Final {
                    answer: answer.into(),
                },
                continuation: None,
                usage: None,
            })
        } else {
            ArithmeticModel.respond(request, cancellation).await
        }
    }
}

#[tokio::test]
async fn later_cell_failure_retains_completed_effects_and_recoverable_feedback()
-> Result<(), Box<dyn Error>> {
    for fatal in [false, true] {
        let store = store()?;
        let mut engine = Engine::new(
            settings()?,
            EngineAdapters {
                model: Arc::new(RecoveryModel),
                executor: Box::new(FailureExecutor { fatal }),
                capabilities: Arc::new(ArithmeticHost),
                context: Arc::new(BoundedContext),
                store: store.clone(),
            },
            None,
        );
        let result = engine
            .run(
                RunRequest {
                    input: "keep completed work".into(),
                },
                CancellationToken::new(),
            )
            .await;
        if fatal {
            assert!(matches!(
                result,
                Err(cocoa_core::engine::RunError::Execution(_))
            ));
        } else {
            assert_eq!(result?.answer, "21 was retained despite the later error");
        }
        engine.shutdown().await?;
        assert!(store.load().await?.into_records().iter().any(|record| matches!(&record.kind,
            RecordKind::OperationFinished { outcome: cocoa_core::capability::OperationOutcome::Completed { value }, .. } if value == &json!(21))));
    }
    Ok(())
}

struct FeedbackExecutor;

fn feedback_error(error: CapabilityError) -> FeedbackError {
    FeedbackError::Capability {
        method: "multiply".into(),
        code: "invalid_arguments".into(),
        message: match error {
            CapabilityError::Rejected { message } => message,
            error => error.to_string(),
        },
        operation_effects: OperationEffects::None,
        truncated: false,
    }
}

#[async_trait]
impl CellExecutor for FeedbackExecutor {
    async fn execute(
        &mut self,
        _: Cell,
        operations: &OperationContext,
        _: CancellationToken,
    ) -> Result<CellOutcome, ExecutionError> {
        match operations
            .call("multiply", json!({"left":7,"right":3}))
            .await
        {
            Ok(value) => Ok(CellOutcome::Completed {
                printed: vec![value],
            }),
            Err(error) => Ok(CellOutcome::Failed {
                error: feedback_error(error),
                printed: Vec::new(),
            }),
        }
    }
    async fn shutdown(&mut self, _: CancellationToken) -> Result<ShutdownReport, ExecutionError> {
        Ok(ShutdownReport { unresolved_work: 0 })
    }
}

struct RejectingStore {
    inner: Arc<InMemoryStore>,
    reject_start: bool,
}

#[async_trait]
impl Store for RejectingStore {
    async fn append(
        &self,
        record: &cocoa_core::store::Record,
        instruction_state: Option<&cocoa_core::context::InstructionState>,
    ) -> Result<cocoa_core::store::RecordId, cocoa_core::store::StoreError> {
        if (self.reject_start && matches!(record.kind, RecordKind::OperationStarted { .. }))
            || (!self.reject_start && matches!(record.kind, RecordKind::OperationFinished { .. }))
        {
            return Err(cocoa_core::store::StoreError::Backend {
                message: "injected storage failure".into(),
            });
        }
        self.inner.append(record, instruction_state).await
    }
    async fn load(
        &self,
    ) -> Result<cocoa_core::store::StoreSnapshot, cocoa_core::store::StoreError> {
        self.inner.load().await
    }
}

struct EvidenceHost {
    store: Arc<InMemoryStore>,
    effect: Arc<std::sync::atomic::AtomicI64>,
}

#[async_trait]
impl CapabilityHost for EvidenceHost {
    fn descriptions(&self) -> Vec<CapabilityDescription> {
        ArithmeticHost.descriptions()
    }
    async fn call(
        &self,
        name: &str,
        arguments: serde_json::Value,
        limits: WorkLimits,
        cancellation: CancellationToken,
    ) -> Result<serde_json::Value, CapabilityError> {
        assert!(
            self.store
                .load()
                .await?
                .into_records()
                .iter()
                .any(|record| matches!(record.kind, RecordKind::OperationStarted { .. }))
        );
        self.effect.store(21, std::sync::atomic::Ordering::SeqCst);
        ArithmeticHost
            .call(name, arguments, limits, cancellation)
            .await
    }
}

#[tokio::test]
async fn storage_failure_cannot_be_downgraded_to_model_feedback() -> Result<(), Box<dyn Error>> {
    use cocoa_core::{engine::RunError, store::StoreError};
    for reject_start in [true, false] {
        let store = store()?;
        let effect = Arc::new(std::sync::atomic::AtomicI64::new(0));
        let mut engine = Engine::new(
            settings()?,
            EngineAdapters {
                model: Arc::new(RecoveryModel),
                executor: Box::new(FeedbackExecutor),
                capabilities: Arc::new(EvidenceHost {
                    store: store.clone(),
                    effect: effect.clone(),
                }),
                context: Arc::new(BoundedContext),
                store: Arc::new(RejectingStore {
                    inner: store.clone(),
                    reject_start,
                }),
            },
            None,
        );
        let error = engine
            .run(
                RunRequest {
                    input: "never hide storage failure".into(),
                },
                CancellationToken::new(),
            )
            .await;
        assert!(
            matches!(&error, Err(RunError::Store(StoreError::Backend { .. })))
                || matches!(&error, Err(RunError::Settlement { cause, .. }) if matches!(cause.as_ref(), RunError::Store(StoreError::Backend { .. }))),
            "{error:?}"
        );
        assert_eq!(
            effect.load(std::sync::atomic::Ordering::SeqCst),
            if reject_start { 0 } else { 21 }
        );
        let cleanup = engine.shutdown().await;
        assert_eq!(cleanup.is_ok(), reject_start);
        let records = store.load().await?.into_records();
        assert!(!records.iter().any(|record| matches!(
            record.kind,
            RecordKind::CellCompleted {
                outcome: CellOutcome::Completed { .. } | CellOutcome::Failed { .. },
                ..
            }
        )));
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(record.kind, RecordKind::OperationStarted { .. }))
                .count(),
            usize::from(!reject_start)
        );
    }
    Ok(())
}

struct PendingModel;

#[async_trait]
impl ModelProvider for PendingModel {
    async fn respond(
        &self,
        request: ModelRequest,
        _: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        request.observations.assistant_text("ééé unfinished");
        std::future::pending().await
    }
}

#[tokio::test]
async fn model_stall_bounds_a_nonresponsive_adapter() -> Result<(), Box<dyn Error>> {
    use cocoa_core::engine::{EventKind, EventPublisher, Observation};
    let store = store()?;
    let (publisher, mut receiver) = EventPublisher::channel(8, 5)?;
    let mut limits = settings()?.limits();
    limits.stall_timeout = Duration::from_millis(15);
    let mut engine = Engine::new(
        EngineSettings::new(limits)?,
        EngineAdapters {
            model: Arc::new(PendingModel),
            executor: Box::new(ArithmeticExecutor),
            capabilities: Arc::new(ArithmeticHost),
            context: Arc::new(BoundedContext),
            store: store.clone(),
        },
        Some(publisher),
    );
    let result = tokio::time::timeout(
        Duration::from_millis(300),
        engine.run(
            RunRequest {
                input: "deadline".into(),
            },
            CancellationToken::new(),
        ),
    )
    .await?;
    assert!(matches!(
        result,
        Err(cocoa_core::engine::RunError::Deadline {
            cause: cocoa_core::store::FailureCause::Stalled { .. }
        })
    ));
    engine.shutdown().await?;
    let records = store.load().await?.into_records();
    assert_eq!(records.len(), 2);
    assert!(matches!(
        records[1].kind,
        RecordKind::InvocationFinished {
            outcome: cocoa_core::store::InvocationOutcome::Failed {
                cause: cocoa_core::store::FailureCause::Stalled { .. }
            }
        }
    ));
    let mut saw_provisional = false;
    loop {
        match receiver.recv().await {
            Observation::Event(event) => match event.kind {
                EventKind::AssistantText { text, truncated } => {
                    assert_eq!(text, "éé");
                    assert!(truncated);
                    saw_provisional = true;
                }
                EventKind::AnswerCompleted { .. } => {
                    panic!("partial output is not a completed answer")
                }
                _ => {}
            },
            Observation::Closed => break,
            Observation::Lagged { .. } => panic!("small fixture must fit the channel"),
        }
    }
    assert!(saw_provisional);
    Ok(())
}

#[tokio::test]
async fn stalled_context_is_bounded_without_model_activity() -> Result<(), Box<dyn Error>> {
    use cocoa_core::context::{ContextAssembler, ContextError, InstructionState, PreparedContext};
    use cocoa_core::store::StoredRecord;

    struct PendingContext;

    #[async_trait]
    impl ContextAssembler for PendingContext {
        async fn prepare(
            &self,
            _: Vec<StoredRecord>,
            _: Vec<CapabilityDescription>,
            _: usize,
        ) -> Result<PreparedContext, ContextError> {
            std::future::pending().await
        }
        async fn restore(&self, _: &InstructionState) -> Result<(), ContextError> {
            Ok(())
        }
    }

    let mut limits = settings()?.limits();
    limits.stall_timeout = Duration::from_millis(15);
    let mut engine = Engine::new(
        EngineSettings::new(limits)?,
        EngineAdapters {
            model: Arc::new(ArithmeticModel),
            executor: Box::new(ArithmeticExecutor),
            capabilities: Arc::new(ArithmeticHost),
            context: Arc::new(PendingContext),
            store: store()?,
        },
        None,
    );
    assert!(matches!(
        tokio::time::timeout(
            Duration::from_millis(300),
            engine.run(
                RunRequest {
                    input: "assemble context".into()
                },
                CancellationToken::new()
            )
        )
        .await?,
        Err(cocoa_core::engine::RunError::Deadline {
            cause: cocoa_core::store::FailureCause::Stalled { .. }
        })
    ));
    engine.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn progressing_requests_and_successive_rounds_do_not_share_a_turn_deadline()
-> Result<(), Box<dyn Error>> {
    struct ProgressingModel {
        chunks: u32,
        interval: Duration,
    }

    #[async_trait]
    impl ModelProvider for ProgressingModel {
        async fn respond(
            &self,
            request: ModelRequest,
            cancellation: CancellationToken,
        ) -> Result<ModelResponse, ModelError> {
            for _ in 0..self.chunks {
                tokio::time::sleep(self.interval).await;
                request.observations.progress();
            }
            ArithmeticModel.respond(request, cancellation).await
        }
    }

    for (chunks, interval) in [(1, 100), (5, 40)] {
        let mut limits = settings()?.limits();
        limits.stall_timeout = Duration::from_millis(150);
        let mut engine = Engine::new(
            EngineSettings::new(limits)?,
            EngineAdapters {
                model: Arc::new(ProgressingModel {
                    chunks,
                    interval: Duration::from_millis(interval),
                }),
                executor: Box::new(ArithmeticExecutor),
                capabilities: Arc::new(ArithmeticHost),
                context: Arc::new(BoundedContext),
                store: store()?,
            },
            None,
        );
        let started = tokio::time::Instant::now();
        assert_eq!(
            engine
                .run(
                    RunRequest {
                        input: "keep working".into()
                    },
                    CancellationToken::new()
                )
                .await?
                .answer,
            "23, not 32"
        );
        assert!(started.elapsed() > limits.stall_timeout);
        engine.shutdown().await?;
    }
    Ok(())
}

struct RepeatedExecutor;

#[async_trait]
impl CellExecutor for RepeatedExecutor {
    async fn execute(
        &mut self,
        _: Cell,
        operations: &OperationContext,
        _: CancellationToken,
    ) -> Result<CellOutcome, ExecutionError> {
        for _ in 0..3 {
            if let Err(error) = operations
                .call("multiply", json!({"left":7,"right":3}))
                .await
            {
                return Ok(CellOutcome::Failed {
                    error: feedback_error(error),
                    printed: Vec::new(),
                });
            }
        }
        Ok(CellOutcome::Completed {
            printed: vec![json!(63)],
        })
    }
    async fn shutdown(&mut self, _: CancellationToken) -> Result<ShutdownReport, ExecutionError> {
        Ok(ShutdownReport { unresolved_work: 0 })
    }
}

struct ConcurrentExecutor {
    started: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl CellExecutor for ConcurrentExecutor {
    async fn execute(
        &mut self,
        _: Cell,
        operations: &OperationContext,
        _: CancellationToken,
    ) -> Result<CellOutcome, ExecutionError> {
        let started = self.started.clone();
        let first = operations.call("multiply", json!({"left":7,"right":3}));
        let second = async {
            started.notified().await;
            operations
                .call("multiply", json!({"left":7,"right":3}))
                .await
        };
        let error = tokio::select! {
            result = first => match result {
                Ok(_) => return Err(ExecutionError::Unavailable {
                    message: "pending fixture call completed".into(),
                }),
                Err(error) => error,
            },
            result = second => match result {
                Ok(_) => return Err(ExecutionError::Unavailable {
                    message: "second call bypassed the pending limit".into(),
                }),
                Err(error) => error,
            },
        };
        Ok(CellOutcome::Failed {
            error: feedback_error(error),
            printed: Vec::new(),
        })
    }
    async fn shutdown(&mut self, _: CancellationToken) -> Result<ShutdownReport, ExecutionError> {
        Ok(ShutdownReport { unresolved_work: 0 })
    }
}

#[tokio::test]
async fn pending_operation_limit_rejects_before_starting_another_effect()
-> Result<(), Box<dyn Error>> {
    let store = store()?;
    let started = Arc::new(tokio::sync::Notify::new());
    let mut limits = settings()?.limits();
    limits.pending_operations = 1;
    let mut engine = Engine::new(
        EngineSettings::new(limits)?,
        EngineAdapters {
            model: Arc::new(RecoveryModel),
            executor: Box::new(ConcurrentExecutor {
                started: started.clone(),
            }),
            capabilities: Arc::new(PendingHost { started }),
            context: Arc::new(BoundedContext),
            store: store.clone(),
        },
        None,
    );
    let result = tokio::time::timeout(
        Duration::from_millis(300),
        engine.run(
            RunRequest {
                input: "no pending queue".into(),
            },
            CancellationToken::new(),
        ),
    )
    .await?;
    assert!(matches!(
        result,
        Err(cocoa_core::engine::RunError::Execution(
            ExecutionError::Capability(CapabilityError::PendingLimit)
        ))
    ));
    engine.shutdown().await?;
    let records = store.load().await?.into_records();
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record.kind, RecordKind::OperationStarted { .. }))
            .count(),
        1
    );
    assert!(records.iter().any(|record| matches!(
        record.kind,
        RecordKind::OperationFinished {
            outcome: cocoa_core::capability::OperationOutcome::Unknown,
            ..
        }
    )));
    Ok(())
}

struct OversizedHost;

#[async_trait]
impl CapabilityHost for OversizedHost {
    fn descriptions(&self) -> Vec<CapabilityDescription> {
        ArithmeticHost.descriptions()
    }
    async fn call(
        &self,
        _: &str,
        _: serde_json::Value,
        _: WorkLimits,
        _: CancellationToken,
    ) -> Result<serde_json::Value, CapabilityError> {
        Ok(json!("é".repeat(3000)))
    }
}

#[tokio::test]
async fn oversized_tool_output_keeps_bounded_completed_effect_evidence()
-> Result<(), Box<dyn Error>> {
    let store = store()?;
    let mut engine = Engine::new(
        settings()?,
        EngineAdapters {
            model: Arc::new(RecoveryModel),
            executor: Box::new(FeedbackExecutor),
            capabilities: Arc::new(OversizedHost),
            context: Arc::new(BoundedContext),
            store: store.clone(),
        },
        None,
    );
    assert!(matches!(
        engine
            .run(
                RunRequest {
                    input: "bound tool output".into()
                },
                CancellationToken::new()
            )
            .await,
        Err(cocoa_core::engine::RunError::Execution(
            ExecutionError::Capability(CapabilityError::OutputLimit)
        ))
    ));
    engine.shutdown().await?;
    assert!(
        store
            .load()
            .await?
            .into_records()
            .iter()
            .any(|record| matches!(
                record.kind,
                RecordKind::OperationFinished {
                    outcome: cocoa_core::capability::OperationOutcome::CompletedOmitted {
                        output_bytes: 6002
                    },
                    ..
                }
            ))
    );
    Ok(())
}

struct CleanupExecutor {
    failure: &'static str,
}

#[async_trait]
impl CellExecutor for CleanupExecutor {
    async fn execute(
        &mut self,
        cell: Cell,
        operations: &OperationContext,
        cancellation: CancellationToken,
    ) -> Result<CellOutcome, ExecutionError> {
        ArithmeticExecutor
            .execute(cell, operations, cancellation)
            .await
    }
    async fn shutdown(&mut self, _: CancellationToken) -> Result<ShutdownReport, ExecutionError> {
        match self.failure {
            "deadline" => std::future::pending().await,
            "unresolved" => Ok(ShutdownReport { unresolved_work: 3 }),
            "retry" => {
                self.failure = "settled";
                Ok(ShutdownReport { unresolved_work: 3 })
            }
            "settled" => Ok(ShutdownReport { unresolved_work: 0 }),
            _ => Err(ExecutionError::Unavailable {
                message: "cleanup unavailable".into(),
            }),
        }
    }
}

#[tokio::test]
async fn cleanup_is_bounded_and_reconciles_records_even_when_workers_fail()
-> Result<(), Box<dyn Error>> {
    use cocoa_core::engine::{RecordSettlement, RunError, WorkerSettlement};
    for failure in ["deadline", "unresolved", "failed", "retry"] {
        let store = store()?;
        let started = Arc::new(tokio::sync::Notify::new());
        let cancellation = CancellationToken::new();
        let mut limits = settings()?.limits();
        limits.settlement_timeout = Duration::from_millis(15);
        limits.shutdown_timeout = Duration::from_millis(15);
        let mut engine = Engine::new(
            EngineSettings::new(limits)?,
            EngineAdapters {
                model: Arc::new(ArithmeticModel),
                executor: Box::new(CleanupExecutor { failure }),
                capabilities: Arc::new(PendingHost {
                    started: started.clone(),
                }),
                context: Arc::new(BoundedContext),
                store: store.clone(),
            },
            None,
        );
        let (result, ()) = tokio::time::timeout(Duration::from_millis(300), async {
            tokio::join!(
                engine.run(
                    RunRequest {
                        input: "cleanup".into()
                    },
                    cancellation.clone()
                ),
                async {
                    started.notified().await;
                    cancellation.cancel();
                }
            )
        })
        .await?;
        let Err(RunError::Settlement { cause, cleanup }) = result else {
            panic!("missing cleanup error");
        };
        assert!(matches!(cause.as_ref(), RunError::Cancelled));
        assert!(matches!(cleanup.records, RecordSettlement::Settled));
        match failure {
            "deadline" => assert!(matches!(cleanup.workers, WorkerSettlement::Deadline)),
            "unresolved" | "retry" => assert!(matches!(
                cleanup.workers,
                WorkerSettlement::Unresolved { work: 3 }
            )),
            _ => assert!(matches!(cleanup.workers, WorkerSettlement::Failed { .. })),
        }
        assert!(
            store
                .load()
                .await?
                .into_records()
                .iter()
                .any(|record| matches!(
                    record.kind,
                    RecordKind::OperationFinished {
                        outcome: cocoa_core::capability::OperationOutcome::Unknown,
                        ..
                    }
                ))
        );
        let shutdown = tokio::time::timeout(Duration::from_millis(300), engine.shutdown()).await?;
        if failure == "retry" {
            shutdown?;
            let snapshot = store.load().await?;
            cocoa_core::engine::validate_resume_snapshot(&snapshot)?;
            assert_eq!(
                snapshot
                    .records
                    .iter()
                    .filter(|stored| matches!(
                        stored.record.kind,
                        RecordKind::InvocationFinished { .. }
                    ))
                    .count(),
                1
            );
            assert_eq!(
                snapshot
                    .records
                    .iter()
                    .filter(|stored| matches!(
                        stored.record.kind,
                        RecordKind::CellCompleted {
                            outcome: CellOutcome::Interrupted { .. },
                            ..
                        }
                    ))
                    .count(),
                1
            );
        } else {
            assert!(shutdown.is_err());
        }
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
    }
    Ok(())
}

struct RepairFailureStore {
    inner: Arc<InMemoryStore>,
    kind: &'static str,
    failed: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl Store for RepairFailureStore {
    async fn append(
        &self,
        record: &cocoa_core::store::Record,
        instruction_state: Option<&cocoa_core::context::InstructionState>,
    ) -> Result<cocoa_core::store::RecordId, cocoa_core::store::StoreError> {
        let target = match self.kind {
            "cell" => matches!(record.kind, RecordKind::CellCompleted { .. }),
            _ => matches!(record.kind, RecordKind::InvocationFinished { .. }),
        };
        if target && !self.failed.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return Err(cocoa_core::store::StoreError::Backend {
                message: "repair append rejected".into(),
            });
        }
        self.inner.append(record, instruction_state).await
    }

    async fn load(
        &self,
    ) -> Result<cocoa_core::store::StoreSnapshot, cocoa_core::store::StoreError> {
        self.inner.load().await
    }
}

#[tokio::test]
async fn shutdown_retries_unfinished_cell_and_terminal_repairs_without_repeating_effects()
-> Result<(), Box<dyn Error>> {
    for kind in ["cell", "terminal"] {
        let inner = store()?;
        let store = Arc::new(RepairFailureStore {
            inner: inner.clone(),
            kind,
            failed: std::sync::atomic::AtomicBool::new(false),
        });
        let mut engine = Engine::new(
            settings()?,
            EngineAdapters {
                model: Arc::new(ArithmeticModel),
                executor: Box::new(FailureExecutor { fatal: true }),
                capabilities: Arc::new(ArithmeticHost),
                context: Arc::new(BoundedContext),
                store,
            },
            None,
        );
        let result = engine
            .run(
                RunRequest {
                    input: "retain repair identity".into(),
                },
                CancellationToken::new(),
            )
            .await;
        assert!(
            matches!(result, Err(cocoa_core::engine::RunError::Settlement { .. })),
            "{result:?}"
        );
        engine.shutdown().await?;
        let snapshot = inner.load().await?;
        cocoa_core::engine::validate_resume_snapshot(&snapshot)?;
        assert_eq!(
            snapshot
                .records
                .iter()
                .filter(|stored| matches!(
                    stored.record.kind,
                    RecordKind::InvocationFinished { .. }
                ))
                .count(),
            1
        );
        assert_eq!(
            snapshot
                .records
                .iter()
                .filter(|stored| matches!(
                    stored.record.kind,
                    RecordKind::CellCompleted {
                        outcome: CellOutcome::Interrupted { .. },
                        ..
                    }
                ))
                .count(),
            1
        );
        assert_eq!(snapshot.records.iter().filter(|stored| matches!(&stored.record.kind, RecordKind::OperationFinished { outcome: OperationOutcome::Completed { value }, .. } if value == &json!(21))).count(), 1);
    }
    Ok(())
}

#[test]
fn validated_settings_reject_unrepresentable_runtime_limits() -> Result<(), Box<dyn Error>> {
    let mut limits = settings()?.limits();
    limits.pending_operations = tokio::sync::Semaphore::MAX_PERMITS + 1;
    assert!(EngineSettings::new(limits).is_err());
    limits = settings()?.limits();
    limits.stall_timeout = Duration::MAX;
    assert!(EngineSettings::new(limits).is_err());
    limits = settings()?.limits();
    limits.shutdown_timeout = Duration::ZERO;
    assert!(EngineSettings::new(limits).is_err());
    Ok(())
}

#[tokio::test]
async fn input_must_fit_its_record_before_admission() -> Result<(), Box<dyn Error>> {
    let store = store()?;
    let mut limits = settings()?.limits();
    limits.record_bytes = 200;
    let mut engine = Engine::new(
        EngineSettings::new(limits)?,
        EngineAdapters {
            model: Arc::new(FixedModel(ModelResponse {
                action: ModelAction::Final {
                    answer: "ok".into(),
                },
                continuation: None,
                usage: None,
            })),
            executor: Box::new(ArithmeticExecutor),
            capabilities: Arc::new(ArithmeticHost),
            context: Arc::new(BoundedContext),
            store: store.clone(),
        },
        None,
    );
    assert!(matches!(
        engine
            .run(
                RunRequest {
                    input: "x".repeat(300).into()
                },
                CancellationToken::new()
            )
            .await,
        Err(cocoa_core::engine::RunError::InputLimit)
    ));
    assert!(store.load().await?.into_records().is_empty());
    let output = engine
        .run(
            RunRequest {
                input: "valid".into(),
            },
            CancellationToken::new(),
        )
        .await?;
    assert_eq!(output.answer, "ok");
    assert_eq!(output.invocation.0, 1);
    engine.shutdown().await?;
    Ok(())
}

struct CancellingHost {
    cancellation: CancellationToken,
}

#[async_trait]
impl CapabilityHost for CancellingHost {
    fn descriptions(&self) -> Vec<CapabilityDescription> {
        ArithmeticHost.descriptions()
    }
    async fn call(
        &self,
        _: &str,
        _: serde_json::Value,
        _: WorkLimits,
        _: CancellationToken,
    ) -> Result<serde_json::Value, CapabilityError> {
        self.cancellation.cancel();
        Ok(json!(21))
    }
}

#[tokio::test]
async fn cancellation_stops_new_effects_even_when_adapters_never_yield()
-> Result<(), Box<dyn Error>> {
    let store = store()?;
    let cancellation = CancellationToken::new();
    let mut engine = Engine::new(
        settings()?,
        EngineAdapters {
            model: Arc::new(FixedModel(ModelResponse {
                action: ModelAction::Execute {
                    cells: vec![Cell {
                        code: "effect".into(),
                    }],
                },
                continuation: None,
                usage: None,
            })),
            executor: Box::new(RepeatedExecutor),
            capabilities: Arc::new(CancellingHost {
                cancellation: cancellation.clone(),
            }),
            context: Arc::new(BoundedContext),
            store: store.clone(),
        },
        None,
    );
    assert!(matches!(
        engine
            .run(
                RunRequest {
                    input: "stop after first effect".into()
                },
                cancellation
            )
            .await,
        Err(cocoa_core::engine::RunError::Execution(
            ExecutionError::Unavailable { .. }
        ))
    ));
    engine.shutdown().await?;
    let records = store.load().await?.into_records();
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record.kind, RecordKind::OperationStarted { .. }))
            .count(),
        1
    );
    assert!(records.iter().any(|record| matches!(&record.kind, RecordKind::OperationFinished { outcome: cocoa_core::capability::OperationOutcome::Completed { value }, .. } if value == &json!(21))));
    Ok(())
}

struct CapturingHost {
    started: tokio::sync::Notify,
    cancellation: tokio::sync::Mutex<Option<CancellationToken>>,
}

#[async_trait]
impl CapabilityHost for CapturingHost {
    fn descriptions(&self) -> Vec<CapabilityDescription> {
        ArithmeticHost.descriptions()
    }
    async fn call(
        &self,
        _: &str,
        _: serde_json::Value,
        _: WorkLimits,
        cancellation: CancellationToken,
    ) -> Result<serde_json::Value, CapabilityError> {
        *self.cancellation.lock().await = Some(cancellation);
        self.started.notify_one();
        std::future::pending().await
    }
}

#[tokio::test]
async fn sibling_cancellation_cannot_hide_unsettled_owned_work() -> Result<(), Box<dyn Error>> {
    use cocoa_core::{engine::RunError, execution::RuntimeState};

    struct UnsettledHost {
        started: tokio::sync::Notify,
        cancellation: CancellationToken,
    }

    #[async_trait]
    impl CapabilityHost for UnsettledHost {
        fn descriptions(&self) -> Vec<CapabilityDescription> {
            Vec::new()
        }

        async fn call(
            &self,
            name: &str,
            _: serde_json::Value,
            _: WorkLimits,
            cancellation: CancellationToken,
        ) -> Result<serde_json::Value, CapabilityError> {
            if name == "unsettled" {
                self.started.notified().await;
                self.cancellation.cancel();
                return Err(CapabilityError::Unsettled {
                    message: "process group termination unconfirmed".into(),
                });
            }
            self.started.notify_one();
            cancellation.cancelled().await;
            Err(CapabilityError::Cancelled)
        }
    }

    struct ConcurrentExecutor;

    #[async_trait]
    impl CellExecutor for ConcurrentExecutor {
        async fn execute(
            &mut self,
            _: Cell,
            operations: &OperationContext,
            _: CancellationToken,
        ) -> Result<CellOutcome, ExecutionError> {
            let (first, second) = tokio::join!(
                biased;
                operations.call("unsettled", json!({})),
                operations.call("cancelled", json!({}))
            );
            assert!(matches!(first, Err(CapabilityError::Unsettled { .. })));
            assert!(matches!(second, Err(CapabilityError::Cancelled)));
            Ok(CellOutcome::Interrupted {
                printed: Vec::new(),
            })
        }

        async fn cancel_turn(&mut self) -> Result<RuntimeState, ExecutionError> {
            Ok(RuntimeState::Preserved)
        }

        async fn shutdown(
            &mut self,
            _: CancellationToken,
        ) -> Result<ShutdownReport, ExecutionError> {
            Ok(ShutdownReport { unresolved_work: 0 })
        }
    }

    let store = store()?;
    let cancellation = CancellationToken::new();
    let mut engine = Engine::new(
        settings()?,
        EngineAdapters {
            model: Arc::new(FixedModel(ModelResponse {
                action: ModelAction::Execute {
                    cells: vec![Cell {
                        code: "concurrent work".into(),
                    }],
                },
                continuation: None,
                usage: None,
            })),
            executor: Box::new(ConcurrentExecutor),
            capabilities: Arc::new(UnsettledHost {
                started: tokio::sync::Notify::new(),
                cancellation: cancellation.clone(),
            }),
            context: Arc::new(BoundedContext),
            store: store.clone(),
        },
        None,
    );
    let result = engine
        .run(
            RunRequest {
                input: "cancel both".into(),
            },
            cancellation,
        )
        .await;
    assert!(
        matches!(
            result,
            Err(RunError::Execution(ExecutionError::Capability(
                CapabilityError::Unsettled { .. }
            )))
        ),
        "{result:?}"
    );
    assert!(matches!(
        engine
            .run(
                RunRequest {
                    input: "must not continue".into()
                },
                CancellationToken::new()
            )
            .await,
        Err(RunError::Stopped)
    ));
    assert!(
        !store
            .load()
            .await?
            .into_records()
            .iter()
            .any(|record| matches!(
                record.kind,
                RecordKind::TurnCancelled { .. }
                    | RecordKind::CellCompleted {
                        outcome: CellOutcome::Completed { .. } | CellOutcome::Failed { .. },
                        ..
                    }
            ))
    );
    engine.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn shutdown_signals_backend_cancellation_after_run_future_is_dropped()
-> Result<(), Box<dyn Error>> {
    let store = store()?;
    let host = Arc::new(CapturingHost {
        started: tokio::sync::Notify::new(),
        cancellation: tokio::sync::Mutex::new(None),
    });
    let mut engine = Engine::new(
        settings()?,
        EngineAdapters {
            model: Arc::new(ArithmeticModel),
            executor: Box::new(ArithmeticExecutor),
            capabilities: host.clone(),
            context: Arc::new(BoundedContext),
            store: store.clone(),
        },
        None,
    );
    tokio::select! {
        result = engine.run(RunRequest { input: "drop is not cancellation".into() }, CancellationToken::new()) => panic!("unexpected completion: {result:?}"),
        () = host.started.notified() => {},
    }
    assert!(matches!(
        engine
            .run(
                RunRequest {
                    input: "no overlap".into()
                },
                CancellationToken::new()
            )
            .await,
        Err(cocoa_core::engine::RunError::Stopped)
    ));
    engine.shutdown().await?;
    let token = host.cancellation.lock().await;
    assert!(token.as_ref().is_some_and(CancellationToken::is_cancelled));
    assert!(
        store
            .load()
            .await?
            .into_records()
            .iter()
            .any(|record| matches!(
                record.kind,
                RecordKind::OperationFinished {
                    outcome: cocoa_core::capability::OperationOutcome::Unknown,
                    ..
                }
            ))
    );
    Ok(())
}

struct ErrorHost {
    error: CapabilityError,
}

#[async_trait]
impl CapabilityHost for ErrorHost {
    fn descriptions(&self) -> Vec<CapabilityDescription> {
        ArithmeticHost.descriptions()
    }
    async fn call(
        &self,
        _: &str,
        _: serde_json::Value,
        _: WorkLimits,
        _: CancellationToken,
    ) -> Result<serde_json::Value, CapabilityError> {
        Err(self.error.clone())
    }
}

#[tokio::test]
async fn recoverable_tool_error_is_canonical_model_feedback() -> Result<(), Box<dyn Error>> {
    let store = store()?;
    let mut engine = Engine::new(
        settings()?,
        EngineAdapters {
            model: Arc::new(RecoveryModel),
            executor: Box::new(FeedbackExecutor),
            capabilities: Arc::new(ErrorHost {
                error: CapabilityError::Rejected {
                    message: "grant denied".into(),
                },
            }),
            context: Arc::new(BoundedContext),
            store: store.clone(),
        },
        None,
    );
    let output = engine
        .run(
            RunRequest {
                input: "recover".into(),
            },
            CancellationToken::new(),
        )
        .await?;
    assert_eq!(output.answer, "grant denied; no effect");
    let records = store.load().await?.into_records();
    assert!(records.iter().any(|record| matches!(&record.kind, RecordKind::OperationFinished {
        outcome: cocoa_core::capability::OperationOutcome::Failed { error: CapabilityError::Rejected { message } }, ..
    } if message == "grant denied")));
    assert!(
        records
            .iter()
            .any(|record| matches!(&record.kind, RecordKind::CellCompleted {
        outcome: CellOutcome::Failed { error: FeedbackError::Capability { message, .. }, .. }, ..
    } if message == "grant denied"))
    );
    engine.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn host_fatal_errors_cannot_be_downgraded_to_cell_feedback() -> Result<(), Box<dyn Error>> {
    use cocoa_core::{
        capability::{Effects, OperationOutcome},
        engine::RunError,
        store::StoreError,
    };
    for error in [
        CapabilityError::OutputLimit,
        CapabilityError::Budget,
        CapabilityError::PendingLimit,
        CapabilityError::Store(StoreError::Backend {
            message: "host evidence unavailable".into(),
        }),
        CapabilityError::Cancelled,
        CapabilityError::Deadline,
        CapabilityError::Backend {
            message: "partial backend failure".into(),
            effects: Effects::Partial,
        },
        CapabilityError::Tool {
            code: "stale_read".into(),
            message: "observation changed".into(),
            effects: Effects::None,
        },
    ] {
        let store = store()?;
        let mut engine = Engine::new(
            settings()?,
            EngineAdapters {
                model: Arc::new(RecoveryModel),
                executor: Box::new(FeedbackExecutor),
                capabilities: Arc::new(ErrorHost {
                    error: error.clone(),
                }),
                context: Arc::new(BoundedContext),
                store: store.clone(),
            },
            None,
        );
        let result = engine
            .run(
                RunRequest {
                    input: "preserve the host failure category".into(),
                },
                CancellationToken::new(),
            )
            .await;
        let recoverable = matches!(
            error,
            CapabilityError::Backend { .. } | CapabilityError::Tool { .. }
        );
        let expected = match &error {
            CapabilityError::Store(expected) => {
                matches!(&result, Err(RunError::Store(actual)) if actual == expected)
            }
            CapabilityError::Cancelled => matches!(result, Err(RunError::Cancelled)),
            CapabilityError::Deadline => matches!(
                result,
                Err(RunError::Deadline {
                    cause: cocoa_core::store::FailureCause::Capability
                })
            ),
            CapabilityError::Backend { .. } | CapabilityError::Tool { .. } => result.is_ok(),
            expected => {
                matches!(&result, Err(RunError::Execution(ExecutionError::Capability(actual))) if actual == expected)
            }
        };
        assert!(
            expected,
            "host returned {error:?}, engine returned {result:?}"
        );
        if !recoverable {
            assert!(matches!(
                engine
                    .run(
                        RunRequest {
                            input: "no recovery after fatal failure".into()
                        },
                        CancellationToken::new()
                    )
                    .await,
                Err(RunError::Stopped)
            ));
        }
        engine.shutdown().await?;
        let records = store.load().await?.into_records();
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(record.kind, RecordKind::OperationStarted { .. }))
                .count(),
            1
        );
        let outcomes: Vec<_> = records
            .iter()
            .filter_map(|record| match &record.kind {
                RecordKind::OperationFinished { outcome, .. } => Some(outcome),
                _ => None,
            })
            .collect();
        assert_eq!(
            outcomes,
            vec![&OperationOutcome::Failed {
                error: error.clone()
            }]
        );
        assert_eq!(
            records.iter().any(|record| matches!(
                record.kind,
                RecordKind::CellCompleted {
                    outcome: CellOutcome::Completed { .. } | CellOutcome::Failed { .. },
                    ..
                }
            )),
            recoverable
        );
        assert_eq!(
            records.iter().any(|record| matches!(
                record.kind,
                RecordKind::ModelCompleted {
                    response: ModelResponse {
                        action: ModelAction::Final { .. },
                        ..
                    }
                }
            )),
            recoverable
        );
    }
    Ok(())
}

fn normalized_records(
    snapshot: cocoa_core::store::StoreSnapshot,
) -> Result<Vec<cocoa_core::store::Record>, Box<dyn Error>> {
    let positions: std::collections::BTreeMap<_, _> = snapshot
        .records
        .iter()
        .enumerate()
        .map(|(index, stored)| (stored.id, index as i64 + 1))
        .collect();
    snapshot
        .records
        .into_iter()
        .map(|mut stored| {
            if let RecordKind::CellStarted { model, .. } | RecordKind::CellCompleted { model, .. } =
                &mut stored.record.kind
            {
                *model = cocoa_core::store::RecordId::try_from(
                    *positions.get(model).ok_or("missing authorizing model")?,
                )?;
            }
            Ok(stored.record)
        })
        .collect()
}

#[tokio::test]
async fn resume_preserves_history_without_replaying_work_and_advances_invocations()
-> Result<(), Box<dyn Error>> {
    let history = store()?;
    let mut original = engine(history.clone())?;
    original
        .run(
            RunRequest {
                input: "first".into(),
            },
            CancellationToken::new(),
        )
        .await?;
    original.shutdown().await?;
    let retained = history.load().await?;
    let mut resumed = Engine::resume(
        settings()?,
        EngineAdapters {
            model: Arc::new(ResumeModel),
            executor: Box::new(ArithmeticExecutor),
            capabilities: Arc::new(ArithmeticHost),
            context: Arc::new(BoundedContext),
            store: history.clone(),
        },
        None,
    )
    .await?;
    assert_eq!(history.load().await?, retained);
    let result = resumed
        .run(
            RunRequest {
                input: "second".into(),
            },
            CancellationToken::new(),
        )
        .await?;
    assert_eq!(result.invocation.0, 2);
    assert_eq!(result.answer, "history survived");
    let after = history.load().await?;
    assert_eq!(
        &after.records[..retained.records.len()],
        retained.records.as_slice()
    );
    assert_eq!(
        after
            .records
            .iter()
            .filter(|stored| matches!(stored.record.kind, RecordKind::OperationStarted { .. }))
            .count(),
        1
    );
    resumed.shutdown().await?;
    Ok(())
}

struct ResumeModel;

#[async_trait]
impl ModelProvider for ResumeModel {
    async fn respond(
        &self,
        request: ModelRequest,
        _: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        assert!(request.context.instructions.contains("<runtime-restarted>"));
        assert!(
            request
                .context
                .instructions
                .contains("Inspect current shared context through its tools")
        );
        assert!(
            !request
                .context
                .instructions
                .contains("shared context survive")
        );
        assert!(
            request
                .context
                .instructions
                .contains("Read files again before mutation")
        );
        assert!(request.context.records.iter().any(|record| matches!(&record.kind, RecordKind::ModelCompleted { response } if response.continuation.is_some())));
        Ok(ModelResponse {
            action: ModelAction::Final {
                answer: "history survived".into(),
            },
            continuation: None,
            usage: None,
        })
    }
}

struct SnapshotStore(cocoa_core::store::StoreSnapshot);

#[async_trait]
impl Store for SnapshotStore {
    async fn append(
        &self,
        _: &cocoa_core::store::Record,
        _: Option<&cocoa_core::context::InstructionState>,
    ) -> Result<cocoa_core::store::RecordId, cocoa_core::store::StoreError> {
        Err(cocoa_core::store::StoreError::Backend {
            message: "snapshot is read-only".into(),
        })
    }
    async fn load(
        &self,
    ) -> Result<cocoa_core::store::StoreSnapshot, cocoa_core::store::StoreError> {
        Ok(self.0.clone())
    }
}

#[tokio::test]
async fn rejected_resume_settles_the_fresh_executor_and_reports_cleanup_failure()
-> Result<(), Box<dyn Error>> {
    use cocoa_core::{
        engine::{ResumeError, WorkerSettlement},
        store::{InvocationId, Record},
    };
    for failure in ["settled", "unresolved", "failed", "deadline"] {
        let store = store()?;
        store
            .append(
                &Record {
                    invocation: InvocationId(1),
                    kind: RecordKind::InputAccepted {
                        input: "unfinished".into(),
                    },
                },
                None,
            )
            .await?;
        let before = store.load().await?;
        let mut limits = settings()?.limits();
        limits.shutdown_timeout = Duration::from_millis(20);
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            Engine::resume(
                EngineSettings::new(limits)?,
                EngineAdapters {
                    model: Arc::new(ArithmeticModel),
                    executor: Box::new(CleanupExecutor { failure }),
                    capabilities: Arc::new(ArithmeticHost),
                    context: Arc::new(BoundedContext),
                    store: store.clone(),
                },
                None,
            ),
        )
        .await?;
        let error = result.err().ok_or("unfinished history was accepted")?;
        match (failure, error) {
            ("settled", ResumeError::RecoveryRequired) => {}
            (failure, ResumeError::Settlement { cause, cleanup }) => {
                assert!(matches!(*cause, ResumeError::RecoveryRequired));
                assert!(matches!(
                    (failure, cleanup.workers),
                    ("unresolved", WorkerSettlement::Unresolved { work: 3 })
                        | ("failed", WorkerSettlement::Failed { .. })
                        | ("deadline", WorkerSettlement::Deadline)
                ));
            }
            (_, error) => panic!("missing cleanup outcome for {failure}: {error:?}"),
        }
        assert_eq!(
            store.load().await?,
            before,
            "failed resume must not repair history"
        );
    }
    Ok(())
}

#[tokio::test]
async fn resume_rejects_corrupt_or_unreconciled_evidence_before_execution()
-> Result<(), Box<dyn Error>> {
    use cocoa_core::{
        engine::ResumeError,
        store::{InvocationId, InvocationOutcome, Record, RecordId, RecordSequence, StoreSnapshot},
    };
    let history = store()?;
    let mut original = engine(history.clone())?;
    original
        .run(
            RunRequest {
                input: "first".into(),
            },
            CancellationToken::new(),
        )
        .await?;
    original.shutdown().await?;
    let valid = history.load().await?;
    for fault in [
        "sequence",
        "duplicate_id",
        "foreign_model",
        "ordinal",
        "code",
        "duplicate_start",
        "missing_terminal",
        "exhausted",
        "unfinished_older",
        "after_terminal",
    ] {
        let mut snapshot = valid.clone();
        match fault {
            "sequence" => snapshot.records[0].sequence = RecordSequence::try_from(2)?,
            "duplicate_id" => snapshot.records[1].id = snapshot.records[0].id,
            "foreign_model" => {
                let input_id = snapshot.records[0].id;
                for stored in &mut snapshot.records {
                    if let RecordKind::CellStarted { model, .. }
                    | RecordKind::CellCompleted { model, .. } = &mut stored.record.kind
                    {
                        *model = input_id;
                    }
                }
            }
            "ordinal" => {
                for stored in &mut snapshot.records {
                    if let RecordKind::CellCompleted { cell, .. } = &mut stored.record.kind {
                        *cell = 1;
                    }
                }
            }
            "code" => {
                for stored in &mut snapshot.records {
                    if let RecordKind::CellCompleted { code, .. } = &mut stored.record.kind {
                        *code = "different source".into();
                    }
                }
            }
            "duplicate_start" => {
                let start = snapshot
                    .records
                    .iter()
                    .find(|stored| matches!(stored.record.kind, RecordKind::CellStarted { .. }))
                    .ok_or("missing start")?
                    .clone();
                snapshot.records.insert(3, start);
                for (index, stored) in snapshot.records.iter_mut().enumerate() {
                    stored.sequence = RecordSequence::try_from(index as i64 + 1)?;
                    if index == 3 {
                        stored.id = RecordId::try_from(i64::MAX)?;
                    }
                }
            }
            "missing_terminal" => {
                snapshot.records.pop();
            }
            "exhausted" => {
                for stored in &mut snapshot.records {
                    stored.record.invocation = InvocationId(i64::MAX as u64);
                }
            }
            "unfinished_older" => {
                snapshot.records.pop();
                snapshot.records.push(cocoa_core::store::StoredRecord {
                    id: RecordId::try_from(i64::MAX)?,
                    sequence: RecordSequence::try_from(snapshot.records.len() as i64 + 1)?,
                    record: Record {
                        invocation: InvocationId(2),
                        kind: RecordKind::InputAccepted {
                            input: "newer".into(),
                        },
                    },
                });
            }
            "after_terminal" => {
                snapshot.records.push(cocoa_core::store::StoredRecord {
                    id: RecordId::try_from(i64::MAX)?,
                    sequence: RecordSequence::try_from(snapshot.records.len() as i64 + 1)?,
                    record: Record {
                        invocation: InvocationId(1),
                        kind: RecordKind::CellStarted {
                            model: snapshot.records[1].id,
                            cell: 0,
                        },
                    },
                });
            }
            _ => unreachable!(),
        }
        assert_eq!(
            cocoa_core::engine::validate_recovery_snapshot(&snapshot).is_ok(),
            fault == "missing_terminal",
            "{fault}"
        );
        let result = Engine::resume(
            settings()?,
            EngineAdapters {
                model: Arc::new(ResumeModel),
                executor: Box::new(ArithmeticExecutor),
                capabilities: Arc::new(ArithmeticHost),
                context: Arc::new(BoundedContext),
                store: Arc::new(SnapshotStore(snapshot)),
            },
            None,
        )
        .await;
        assert!(
            matches!(
                result,
                Err(ResumeError::InvalidHistory
                    | ResumeError::RecoveryRequired
                    | ResumeError::IdentityExhausted)
            ),
            "{fault}"
        );
    }
    let mut repeated = valid.clone();
    let model_previous = valid.records[1].id;
    let model_current = RecordId::try_from(i64::MAX - 4)?;
    let mut request = valid.records[1].clone();
    request.id = model_current;
    let mut start = valid.records[2].clone();
    start.id = RecordId::try_from(i64::MAX - 3)?;
    start.record.kind = RecordKind::CellStarted {
        model: model_current,
        cell: 0,
    };
    let mut completion = valid.records[5].clone();
    completion.id = RecordId::try_from(i64::MAX - 2)?;
    if let RecordKind::CellCompleted { model, .. } = &mut completion.record.kind {
        *model = model_current;
    }
    repeated.records.splice(6..6, [request, start, completion]);
    for (index, stored) in repeated.records.iter_mut().enumerate() {
        stored.sequence = RecordSequence::try_from(index as i64 + 1)?;
    }
    cocoa_core::engine::validate_resume_snapshot(&repeated)?;
    if let RecordKind::CellCompleted { model, .. } = &mut repeated.records[8].record.kind {
        *model = model_previous;
    }
    assert!(cocoa_core::engine::validate_resume_snapshot(&repeated).is_err());
    let interrupted = StoreSnapshot {
        records: vec![
            cocoa_core::store::StoredRecord {
                id: RecordId::try_from(10)?,
                sequence: RecordSequence::try_from(1)?,
                record: Record {
                    invocation: InvocationId(9),
                    kind: RecordKind::InputAccepted {
                        input: "retained".into(),
                    },
                },
            },
            cocoa_core::store::StoredRecord {
                id: RecordId::try_from(11)?,
                sequence: RecordSequence::try_from(2)?,
                record: Record {
                    invocation: InvocationId(9),
                    kind: RecordKind::InvocationFinished {
                        outcome: InvocationOutcome::Interrupted,
                    },
                },
            },
        ],
        instruction_state: None,
        checkpoints: Vec::new(),
    };
    Engine::resume(
        settings()?,
        EngineAdapters {
            model: Arc::new(ResumeModel),
            executor: Box::new(ArithmeticExecutor),
            capabilities: Arc::new(ArithmeticHost),
            context: Arc::new(BoundedContext),
            store: Arc::new(SnapshotStore(interrupted)),
        },
        None,
    )
    .await?
    .shutdown()
    .await?;
    Ok(())
}

#[tokio::test]
async fn instruction_delivery_commits_before_installation_and_never_executes_after_install_failure()
-> Result<(), Box<dyn Error>> {
    use cocoa_core::context::{ContextAssembler, InstructionDelivery};
    for failure in ["provider", "install", "none"] {
        let history = Arc::new(InMemoryStore::with_instruction_state(StoreLimits {
            record_bytes: 65536,
        })?);
        let context = Arc::new(DeliveryContext {
            delivered: std::sync::atomic::AtomicBool::new(false),
            fail_install: failure == "install",
        });
        let model: Arc<dyn ModelProvider> = if failure == "provider" {
            Arc::new(RejectModel)
        } else {
            Arc::new(ArithmeticModel)
        };
        let mut engine = Engine::new(
            settings()?,
            EngineAdapters {
                model,
                executor: Box::new(ArithmeticExecutor),
                capabilities: Arc::new(ArithmeticHost),
                context: context.clone(),
                store: history.clone(),
            },
            None,
        );
        let result = engine
            .run(
                RunRequest {
                    input: "deliver".into(),
                },
                CancellationToken::new(),
            )
            .await;
        assert_eq!(result.is_ok(), failure == "none");
        let snapshot = history.load().await?;
        assert_eq!(
            snapshot
                .records
                .iter()
                .any(|stored| matches!(stored.record.kind, RecordKind::CellStarted { .. })),
            failure == "none"
        );
        assert_eq!(
            context.delivered.load(std::sync::atomic::Ordering::SeqCst),
            failure == "none"
        );
        if failure == "provider" {
            assert!(snapshot.instruction_state.is_none());
        } else {
            let state = snapshot
                .instruction_state
                .as_ref()
                .ok_or("missing committed state")?;
            assert_eq!(
                state.selected_skills[0].delivery,
                InstructionDelivery::Delivered
            );
        }
        let prepared = context
            .prepare(snapshot.records, Vec::new(), 1024 * 1024)
            .await?;
        assert_eq!(
            prepared.context.instructions,
            if failure == "none" {
                "delivered"
            } else {
                "pending"
            }
        );
        engine.shutdown().await?;
    }
    Ok(())
}

struct DeliveryContext {
    delivered: std::sync::atomic::AtomicBool,
    fail_install: bool,
}

#[async_trait]
impl cocoa_core::context::ContextAssembler for DeliveryContext {
    async fn prepare(
        &self,
        records: Vec<cocoa_core::store::StoredRecord>,
        capabilities: Vec<CapabilityDescription>,
        bytes: usize,
    ) -> Result<cocoa_core::context::PreparedContext, cocoa_core::context::ContextError> {
        use cocoa_core::context::{InstructionDelivery, InstructionState, SelectedSkill};
        let invocation = records
            .last()
            .ok_or(cocoa_core::context::ContextError::Limit)?
            .record
            .invocation;
        let mut prepared = BoundedContext.prepare(records, capabilities, bytes).await?;
        prepared.context.instructions = if self.delivered.load(std::sync::atomic::Ordering::SeqCst)
        {
            "delivered"
        } else {
            "pending"
        }
        .into();
        prepared.instruction_state = Some(InstructionState {
            invocation,
            scoped_sources: Vec::new(),
            selected_skills: vec![SelectedSkill {
                source_path: "/selected/SKILL.md".into(),
                text: "instruction".into(),
                delivery: InstructionDelivery::Delivered,
            }],
            scoped_feedback: Vec::new(),
            skill_feedback: Vec::new(),
        });
        Ok(prepared)
    }
    async fn restore(
        &self,
        _: &cocoa_core::context::InstructionState,
    ) -> Result<(), cocoa_core::context::ContextError> {
        if self.fail_install {
            return Err(cocoa_core::context::ContextError::Unavailable {
                message: "installation rejected".into(),
            });
        }
        self.delivered
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

struct RejectModel;

#[async_trait]
impl ModelProvider for RejectModel {
    async fn respond(
        &self,
        _: ModelRequest,
        _: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        Err(ModelError::Protocol {
            message: "incomplete response".into(),
        })
    }
}

struct CancelAfterCompletion {
    inner: Arc<InMemoryStore>,
    cancellation: CancellationToken,
}

#[async_trait]
impl Store for CancelAfterCompletion {
    async fn append(
        &self,
        record: &cocoa_core::store::Record,
        state: Option<&cocoa_core::context::InstructionState>,
    ) -> Result<cocoa_core::store::RecordId, cocoa_core::store::StoreError> {
        let id = self.inner.append(record, state).await?;
        if matches!(
            record.kind,
            RecordKind::InvocationFinished {
                outcome: cocoa_core::store::InvocationOutcome::Completed
            }
        ) {
            self.cancellation.cancel();
            tokio::task::yield_now().await;
        }
        Ok(id)
    }
    async fn load(
        &self,
    ) -> Result<cocoa_core::store::StoreSnapshot, cocoa_core::store::StoreError> {
        self.inner.load().await
    }
}

#[tokio::test]
async fn committed_success_wins_cancellation_without_appending_conflicting_facts()
-> Result<(), Box<dyn Error>> {
    let cancellation = CancellationToken::new();
    let history = store()?;
    let mut engine = Engine::new(
        settings()?,
        EngineAdapters {
            model: Arc::new(FixedModel(ModelResponse {
                action: ModelAction::Final {
                    answer: "committed".into(),
                },
                continuation: None,
                usage: None,
            })),
            executor: Box::new(ArithmeticExecutor),
            capabilities: Arc::new(ArithmeticHost),
            context: Arc::new(BoundedContext),
            store: Arc::new(CancelAfterCompletion {
                inner: history.clone(),
                cancellation: cancellation.clone(),
            }),
        },
        None,
    );
    assert_eq!(
        engine
            .run(
                RunRequest {
                    input: "finish".into()
                },
                cancellation
            )
            .await?
            .answer,
        "committed"
    );
    let snapshot = history.load().await?;
    cocoa_core::engine::validate_resume_snapshot(&snapshot)?;
    assert!(matches!(
        snapshot.records.last().map(|stored| &stored.record.kind),
        Some(RecordKind::InvocationFinished {
            outcome: cocoa_core::store::InvocationOutcome::Completed
        })
    ));
    engine.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn resumed_engine_uses_active_reads_for_requests_compaction_and_failed_cell_settlement()
-> Result<(), Box<dyn Error>> {
    use std::sync::atomic::Ordering;

    for fatal in [false, true] {
        let (store, through) = selective_store().await?;
        let executor: Box<dyn CellExecutor> = if fatal {
            Box::new(FailureExecutor { fatal: true })
        } else {
            Box::new(ArithmeticExecutor)
        };
        let mut engine = Engine::resume(
            settings()?,
            EngineAdapters {
                model: Arc::new(ArithmeticModel),
                executor,
                capabilities: Arc::new(ArithmeticHost),
                context: Arc::new(BoundedContext),
                store: store.clone(),
            },
            None,
        )
        .await?;
        assert_eq!(store.full_reads.load(Ordering::SeqCst), 1);
        let result = engine
            .run(
                RunRequest {
                    input: "Continue".into(),
                },
                CancellationToken::new(),
            )
            .await;
        if fatal {
            assert!(result.is_err());
            let snapshot = store.inner.load().await?;
            assert!(snapshot.records.iter().any(|record| matches!(
                record.record.kind,
                RecordKind::CellCompleted {
                    outcome: CellOutcome::Interrupted { .. },
                    ..
                }
            )));
        } else {
            assert_eq!(result?.answer, "23, not 32");
            engine
                .compact(through, selective_proposal(), selective_checkpoint_limits())
                .await?;
            assert_eq!(store.inner.load().await?.checkpoints.len(), 2);
        }
        engine.shutdown().await?;
        cocoa_core::engine::validate_resume_snapshot(&store.inner.load().await?)?;
        assert_eq!(store.full_reads.load(Ordering::SeqCst), 1);
        assert!(store.active_reads.load(Ordering::SeqCst) >= 3);
    }
    Ok(())
}

#[tokio::test]
async fn automatic_compaction_advances_a_selective_checkpoint_without_full_reload()
-> Result<(), Box<dyn Error>> {
    use std::sync::atomic::Ordering;

    let (store, _) = selective_store().await?;
    let model = Arc::new(SelectiveSummaryModel {
        summaries: std::sync::atomic::AtomicUsize::new(0),
    });
    let mut engine = Engine::resume(
        settings()?.with_compaction(CompactionPolicy {
            request_bytes: 8192,
            trigger_bytes: 4096,
            target_bytes: 2048,
            summary_input_bytes: 32 * 1024,
            summary_output_tokens: 1024,
            sources: 64,
            checkpoint: selective_checkpoint_limits(),
            timeout: Duration::from_secs(1),
        })?,
        EngineAdapters {
            model: model.clone(),
            executor: Box::new(ArithmeticExecutor),
            capabilities: Arc::new(ArithmeticHost),
            context: Arc::new(BoundedContext),
            store: store.clone(),
        },
        None,
    )
    .await?;
    assert_eq!(
        engine
            .run(
                RunRequest {
                    input: "Continue".into()
                },
                CancellationToken::new()
            )
            .await?
            .answer,
        "Active continuation"
    );
    assert_eq!(model.summaries.load(Ordering::SeqCst), 1);
    assert_eq!(store.inner.load().await?.checkpoints.len(), 2);
    assert_eq!(store.full_reads.load(Ordering::SeqCst), 1);
    engine.shutdown().await?;
    cocoa_core::engine::validate_resume_snapshot(&store.inner.load().await?)?;
    Ok(())
}

struct SelectiveStore {
    inner: Arc<InMemoryStore>,
    full_reads: std::sync::atomic::AtomicUsize,
    active_reads: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl Store for SelectiveStore {
    async fn append(
        &self,
        record: &Record,
        state: Option<&cocoa_core::context::InstructionState>,
    ) -> Result<RecordId, StoreError> {
        self.inner.append(record, state).await
    }

    async fn publish_checkpoint(&self, checkpoint: &Checkpoint) -> Result<RecordId, StoreError> {
        self.inner.publish_checkpoint(checkpoint).await
    }

    async fn load(&self) -> Result<StoreSnapshot, StoreError> {
        if self
            .full_reads
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            != 0
        {
            return Err(StoreError::Backend {
                message: "unexpected full-history read".into(),
            });
        }
        self.inner.load().await
    }

    async fn load_active(&self) -> Result<ActiveSnapshot, StoreError> {
        self.active_reads
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.load_active().await
    }
}

struct SelectiveSummaryModel {
    summaries: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl ModelProvider for SelectiveSummaryModel {
    fn request_bytes(&self, request: &ModelRequest) -> Result<Option<usize>, ModelError> {
        request
            .context
            .encoded_size()
            .map(Some)
            .map_err(|error| ModelError::Protocol {
                message: error.to_string(),
            })
    }

    async fn summarize(
        &self,
        request: SummaryRequest,
        _: CancellationToken,
    ) -> Result<String, ModelError> {
        assert!(request.input.contains("retired detail"));
        self.summaries
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        serde_json::to_string(&selective_proposal()).map_err(|error| ModelError::Protocol {
            message: error.to_string(),
        })
    }

    async fn respond(
        &self,
        request: ModelRequest,
        _: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        assert!(request.context.checkpoint.is_some());
        assert!(!request.context.records.iter().any(|record| matches!(
            &record.kind, RecordKind::ModelCompleted { response }
                if matches!(&response.action, ModelAction::Final { answer } if answer.contains("retired detail"))
        )));
        Ok(ModelResponse {
            action: ModelAction::Final {
                answer: "Active continuation".into(),
            },
            continuation: None,
            usage: None,
        })
    }
}

async fn selective_store() -> Result<(Arc<SelectiveStore>, RecordId), Box<dyn Error>> {
    let inner = store()?;
    let mut through = None;
    for invocation in 1..=2 {
        for kind in [
            RecordKind::InputAccepted {
                input: format!("Task {invocation}").into(),
            },
            RecordKind::ModelCompleted {
                response: ModelResponse {
                    action: ModelAction::Final {
                        answer: "retired detail ".repeat(400),
                    },
                    continuation: None,
                    usage: None,
                },
            },
            RecordKind::InvocationFinished {
                outcome: InvocationOutcome::Completed,
            },
        ] {
            through = Some(
                inner
                    .append(
                        &Record {
                            invocation: InvocationId(invocation),
                            kind,
                        },
                        None,
                    )
                    .await?,
            );
        }
        if invocation == 1 {
            let checkpoint = Checkpoint::prepare(
                &inner.load().await?,
                through.ok_or("missing boundary")?,
                selective_proposal(),
                selective_checkpoint_limits(),
            )?;
            inner.publish_checkpoint(&checkpoint).await?;
        }
    }
    Ok((
        Arc::new(SelectiveStore {
            inner,
            full_reads: std::sync::atomic::AtomicUsize::new(0),
            active_reads: std::sync::atomic::AtomicUsize::new(0),
        }),
        through.ok_or("missing boundary")?,
    ))
}

fn selective_checkpoint_limits() -> CheckpointLimits {
    CheckpointLimits {
        bytes: 4096,
        required_entries: 32,
        items: 32,
    }
}

fn selective_proposal() -> CheckpointProposal {
    CheckpointProposal {
        additions: vec![],
        changes: vec![],
        recap: Recap {
            goal: "Continue the task".into(),
            stages: vec![],
            current_stage: None,
            decisions: vec![],
            progress: vec![],
            failures: vec![],
            next_actions: vec![],
        },
        evidence: vec![],
    }
}
