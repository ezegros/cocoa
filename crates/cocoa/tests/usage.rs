use std::{collections::BTreeMap, error::Error};

use cocoa::{
    persistence::{
        AgentAssignment, EffectiveAliasSettings, EffectiveSessionSettings, SessionDatabase,
        SessionExecutionLimits, StoredSessionSettings, UsageLedger, UsageRequest,
    },
    usage::{PricingState, cache_hit_percent, snapshot},
};
use cocoa_core::{
    accounting::{RequestOutcome, RequestPurpose, RequestSettings, RequestUpdate},
    context::InstructionState,
    model::{InputCacheUsage, Usage},
    store::{InvocationId, OperationId, Record, RecordKind, Store, StoreLimits},
};
use serde_json::json;
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};

#[test]
fn prices_exact_models_and_cache_categories_without_double_counting() {
    let pricing = prices();
    let mut ledger = ledger();
    assert_eq!(pricing.evaluate(&ledger).cache_hit_percent, Some(25.0));
    assert_eq!(pricing.evaluate(&ledger).approximate_usd, Some(3.7));
    ledger.requests.push(ledger.requests[0].clone());
    assert_eq!(pricing.evaluate(&ledger).approximate_usd, Some(7.4));
    ledger.requests[1].settings = Some(RequestSettings {
        model: "fixture-suffix".into(),
        ..request_settings()
    });
    assert_eq!(pricing.evaluate(&ledger).approximate_usd, None);
    let wrong_id = PricingState::from_metadata(&json!({
        "openai": {"models": {"fixture": {"id":"other", "cost":{"input":2,"output":8}}}}
    }));
    ledger.requests.truncate(1);
    assert_eq!(wrong_id.evaluate(&ledger).approximate_usd, None);
    for (provider, key) in [
        ("together", "togetherai"),
        ("fireworks", "fireworks-ai"),
        ("opencode-zen", "opencode"),
        ("vercel-ai-gateway", "vercel"),
    ] {
        let pricing = PricingState::from_metadata(&json!({key: {"models": {"fixture": {
            "id":"fixture", "cost":{"input":2,"output":8,"cache_read":0.4,"cache_write":3}
        }}}}));
        if let StoredSessionSettings::Bound { effective } = &mut ledger.settings {
            effective.provider = provider.into();
        }
        ledger.requests[0].settings = Some(RequestSettings {
            provider: provider.into(),
            ..request_settings()
        });
        assert_eq!(
            pricing.evaluate(&ledger).approximate_usd,
            Some(3.7),
            "{provider}"
        );
    }
}

#[test]
fn incomplete_coverage_and_missing_usage_never_publish_partial_totals() {
    let pricing = prices();
    let mut ledger = ledger();
    ledger.complete = false;
    assert_eq!(pricing.evaluate(&ledger).approximate_usd, None);
    assert_eq!(pricing.evaluate(&ledger).cache_hit_percent, Some(25.0));
    ledger.complete = true;
    ledger.requests.push(UsageRequest {
        settings: Some(request_settings()),
        update: RequestUpdate::default(),
    });
    assert_eq!(pricing.evaluate(&ledger).approximate_usd, None);
    ledger.requests[1].update = ledger.requests[0].update.clone();
    assert_eq!(pricing.evaluate(&ledger).approximate_usd, Some(7.4));
    ledger.requests[0].update.usage_complete = false;
    assert_eq!(pricing.evaluate(&ledger).approximate_usd, None);
    ledger.requests[0].update.usage_complete = true;
    ledger.requests[0].settings = None;
    assert_eq!(pricing.evaluate(&ledger).approximate_usd, None);
}

#[test]
fn subscriptions_and_custom_or_unidentified_routes_hide_price() {
    let pricing = prices();
    for provider in [
        "chatgpt",
        "opencode-go",
        "kimi-code",
        "zai",
        "zai-coding-cn",
        "qwen-token-plan",
        "qwen-token-plan-cn",
        "qwen-token-plan-individual",
        "qwen-token-plan-individual-cn",
        "xiaomi-token-plan-cn",
        "xiaomi-token-plan-ams",
        "xiaomi-token-plan-sgp",
        "ollama",
        "unknown",
    ] {
        let mut ledger = ledger();
        if let StoredSessionSettings::Bound { effective } = &mut ledger.settings {
            effective.provider = provider.into();
        }
        assert_eq!(
            pricing.evaluate(&ledger).approximate_usd,
            None,
            "{provider}"
        );
        assert_eq!(pricing.evaluate(&ledger).cache_hit_percent, Some(25.0));
    }
    let mut ledger = ledger();
    if let StoredSessionSettings::Bound { effective } = &mut ledger.settings {
        effective.endpoint = Some("https://custom.example/v1/responses".into());
    }
    assert_eq!(pricing.evaluate(&ledger).approximate_usd, None);
    if let StoredSessionSettings::Bound { effective } = &mut ledger.settings {
        effective.endpoint = None;
        if let Some(alias) = effective.aliases.get_mut("main") {
            alias.endpoint = Some("https://custom.example/v1/responses".into());
        }
    }
    if let Some(settings) = &mut ledger.requests[0].settings {
        settings.endpoint_default = Some(false);
    }
    assert_eq!(pricing.evaluate(&ledger).approximate_usd, None);
    if let StoredSessionSettings::Bound { effective } = &mut ledger.settings
        && let Some(alias) = effective.aliases.get_mut("main")
    {
        alias.endpoint = None;
    }
    if let Some(settings) = &mut ledger.requests[0].settings {
        settings.endpoint_default = None;
    }
    assert_eq!(pricing.evaluate(&ledger).approximate_usd, None);
}

#[test]
fn unknown_cache_categories_and_invalid_or_tiered_prices_stay_unknown() {
    let pricing = prices();
    for cache in [
        None,
        Some(InputCacheUsage {
            read_tokens: None,
            write_tokens: Some(0),
        }),
        Some(InputCacheUsage {
            read_tokens: Some(0),
            write_tokens: None,
        }),
        Some(InputCacheUsage {
            read_tokens: Some(900_000),
            write_tokens: Some(200_000),
        }),
    ] {
        let mut ledger = ledger();
        if let Some(usage) = &mut ledger.requests[0].update.usage {
            usage.input_cache = cache;
        }
        assert_eq!(pricing.evaluate(&ledger).approximate_usd, None);
    }
    for cost in [
        json!({"input": -1, "output": 8}),
        json!({"input": 2}),
        json!({"input": 2, "output": 8, "cache_read": "bad"}),
        json!({"input": 2, "output": 8, "context_over_200k": {"input": 4}}),
        json!({"input": 2, "output": 8}),
    ] {
        let pricing = PricingState::from_metadata(&json!({
            "openai": {"models": {"fixture": {"id":"fixture", "cost":cost}}}
        }));
        assert_eq!(pricing.evaluate(&ledger()).approximate_usd, None);
    }
    let mut usage = usage();
    usage.input_cache = None;
    assert_eq!(cache_hit_percent(&usage), None);
    usage.input_cache = Some(InputCacheUsage {
        read_tokens: Some(0),
        write_tokens: None,
    });
    assert_eq!(cache_hit_percent(&usage), Some(0.0));
    usage.input_tokens = 0;
    assert_eq!(cache_hit_percent(&usage), None);
    usage.input_cache = Some(InputCacheUsage {
        read_tokens: Some(1),
        write_tokens: None,
    });
    assert_eq!(cache_hit_percent(&usage), None);
}

#[test]
fn models_without_separate_write_pricing_and_equal_cache_rates_need_no_guesses() {
    let pricing = PricingState::from_metadata(&json!({"openai": {"models": {"fixture": {
        "id":"fixture", "cost":{"input":2,"output":8,"cache_read":0.4}
    }}}}));
    let mut ledger = ledger();
    if let Some(usage) = &mut ledger.requests[0].update.usage {
        usage.input_cache = Some(InputCacheUsage {
            read_tokens: Some(250_000),
            write_tokens: None,
        });
    }
    assert_eq!(pricing.evaluate(&ledger).approximate_usd, Some(3.6));
    let equal = PricingState::from_metadata(&json!({"openai": {"models": {"fixture": {
        "id":"fixture", "cost":{"input":2,"output":8,"cache_read":2,"cache_write":2}
    }}}}));
    if let Some(usage) = &mut ledger.requests[0].update.usage {
        usage.input_cache = None;
    }
    assert_eq!(equal.evaluate(&ledger).approximate_usd, Some(4.0));
    let free = PricingState::from_metadata(&json!({"openai": {"models": {"fixture": {
        "id":"fixture", "cost":{"input":0,"output":0,"cache_read":0,"cache_write":0}
    }}}}));
    assert_eq!(free.evaluate(&ledger).approximate_usd, Some(0.0));
}

#[tokio::test]
async fn durable_snapshot_includes_children_compaction_retries_and_resumed_epochs()
-> Result<(), Box<dyn Error>> {
    let directory = directory()?;
    let path = directory.path().join("sessions.sqlite");
    let database = SessionDatabase::open(&path).await?;
    let session = database.create_session(directory.path()).await?;
    let root = session.root_store_with_settings(limits(), settings())?;
    root.append(&input(), None).await?;
    let recorder = root
        .request_recorder(InvocationId(1))
        .ok_or("missing recorder")?;
    let request = recorder
        .start(RequestPurpose::Response, Some(&request_settings()))
        .await?;
    recorder.update(request, &update()).await?;
    let mut pricing = prices();
    assert_eq!(
        snapshot(&session, &mut pricing).await?.approximate_usd,
        Some(3.7)
    );

    let pending = recorder
        .start(RequestPurpose::Response, Some(&request_settings()))
        .await?;
    let current = snapshot(&session, &mut pricing).await?;
    assert_eq!(current.cache_hit_percent, Some(25.0));
    assert_eq!(current.approximate_usd, None);
    recorder.update(pending, &update()).await?;
    let compaction = recorder
        .start(RequestPurpose::Compaction, Some(&request_settings()))
        .await?;
    let mut compaction_update = update();
    if let Some(usage) = &mut compaction_update.usage {
        usage.input_cache = Some(InputCacheUsage {
            read_tokens: Some(0),
            write_tokens: Some(0),
        });
    }
    recorder.update(compaction, &compaction_update).await?;
    assert_eq!(
        snapshot(&session, &mut pricing).await?.cache_hit_percent,
        Some(25.0)
    );

    let assignment = AgentAssignment {
        prompt: "child".into(),
        alias: "main".into(),
    };
    let operation = root
        .append(
            &Record {
                invocation: InvocationId(1),
                kind: RecordKind::OperationStarted {
                    operation: OperationId(1),
                    name: "cocoa.spawn".into(),
                    arguments: json!({"name":"child", "prompt":"child", "alias":"main"}),
                },
            },
            None,
        )
        .await?;
    let admitted = root
        .admit_agent(operation, "child", &assignment, 65536)
        .await?;
    let child = session.agent_store(admitted.agent, limits()).await?;
    let child_recorder = child
        .request_recorder(InvocationId(1))
        .ok_or("missing child recorder")?;
    let child_request = child_recorder
        .start(RequestPurpose::Response, Some(&request_settings()))
        .await?;
    child_recorder.update(child_request, &update()).await?;
    let ledger = session.usage_ledger().await?;
    assert_eq!(ledger.requests.len(), 4);
    assert!(ledger.complete);
    let expected = 3.7 * 3.0 + 4.0;
    let total = snapshot(&session, &mut pricing)
        .await?
        .approximate_usd
        .ok_or("missing total")?;
    assert!((total - expected).abs() < 1e-10);
    let reference = session.public_id().to_string();
    drop(child_recorder);
    drop(child);
    drop(recorder);
    drop(root);
    drop(session);
    let resumed = database.attach_session(&reference, limits()).await?;
    let resumed_total = snapshot(&resumed, &mut pricing)
        .await?
        .approximate_usd
        .ok_or("missing resumed total")?;
    assert!((resumed_total - expected).abs() < 1e-10);
    Ok(())
}

#[tokio::test]
async fn rebinding_settings_never_reprices_a_custom_endpoint_attempt() -> Result<(), Box<dyn Error>>
{
    let directory = directory()?;
    let database = SessionDatabase::open(&directory.path().join("sessions.sqlite")).await?;
    let session = database.create_session(directory.path()).await?;
    let mut custom = settings();
    custom.endpoint = Some("https://custom.example/v1/responses".into());
    let root = session.root_store_with_settings(limits(), custom)?;
    root.append(&input(), None).await?;
    let recorder = root
        .request_recorder(InvocationId(1))
        .ok_or("missing recorder")?;
    let request = recorder
        .start(
            RequestPurpose::Response,
            Some(&RequestSettings {
                endpoint_default: Some(false),
                ..request_settings()
            }),
        )
        .await?;
    let mut failed = update();
    failed.outcome = Some(RequestOutcome::Protocol);
    recorder.update(request, &failed).await?;
    let mut pricing = prices();
    assert_eq!(
        snapshot(&session, &mut pricing).await?.approximate_usd,
        None
    );

    let rebound = session.root_store_with_settings(limits(), settings())?;
    rebound
        .append(
            &Record {
                invocation: InvocationId(2),
                ..input()
            },
            None,
        )
        .await?;
    let ledger = session.usage_ledger().await?;
    assert_eq!(
        ledger.settings,
        StoredSessionSettings::Bound {
            effective: Box::new(settings())
        }
    );
    assert_eq!(pricing.evaluate(&ledger).approximate_usd, None);
    assert_eq!(pricing.evaluate(&ledger).cache_hit_percent, Some(25.0));
    Ok(())
}

#[tokio::test]
async fn bounded_reads_keep_latest_cache_but_never_claim_a_partial_total()
-> Result<(), Box<dyn Error>> {
    let directory = directory()?;
    let path = directory.path().join("sessions.sqlite");
    let database = SessionDatabase::open(&path).await?;
    let session = database.create_session(directory.path()).await?;
    let root = session.root_store_with_settings(limits(), settings())?;
    root.append(&input(), None).await?;
    let recorder = root
        .request_recorder(InvocationId(1))
        .ok_or("missing recorder")?;
    let request = recorder
        .start(RequestPurpose::Response, Some(&request_settings()))
        .await?;
    recorder.update(request, &update()).await?;
    let mut admin =
        SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(path)).await?;
    sqlx::query("WITH RECURSIVE numbers(n) AS (VALUES(1) UNION ALL SELECT n + 1 FROM numbers WHERE n < 4096) INSERT INTO model_requests (session_id, agent_id, invocation, runtime_epoch, started_at_ms, updated_at_ms, purpose_json, settings_json, update_json) SELECT session_id, agent_id, invocation, runtime_epoch, started_at_ms, updated_at_ms, purpose_json, settings_json, update_json FROM model_requests, numbers WHERE id = ?")
        .bind(request).execute(&mut admin).await?;
    let ledger = session.usage_ledger().await?;
    assert!(!ledger.complete);
    assert!(ledger.requests.is_empty());
    assert_eq!(prices().evaluate(&ledger).cache_hit_percent, Some(25.0));
    assert_eq!(prices().evaluate(&ledger).approximate_usd, None);
    sqlx::query("DELETE FROM model_requests WHERE id != ?")
        .bind(request)
        .execute(&mut admin)
        .await?;
    let mut large = update();
    large.error = Some("x".repeat(15_000));
    sqlx::query("UPDATE model_requests SET update_json = ?")
        .bind(serde_json::to_string(&large)?)
        .execute(&mut admin)
        .await?;
    sqlx::query("WITH RECURSIVE numbers(n) AS (VALUES(1) UNION ALL SELECT n + 1 FROM numbers WHERE n < 600) INSERT INTO model_requests (session_id, agent_id, invocation, runtime_epoch, started_at_ms, updated_at_ms, purpose_json, settings_json, update_json) SELECT session_id, agent_id, invocation, runtime_epoch, started_at_ms, updated_at_ms, purpose_json, settings_json, update_json FROM model_requests, numbers WHERE id = ?")
        .bind(request).execute(&mut admin).await?;
    let ledger = session.usage_ledger().await?;
    assert!(!ledger.complete);
    assert!(ledger.requests.is_empty());
    assert_eq!(prices().evaluate(&ledger).cache_hit_percent, Some(25.0));
    sqlx::query("DELETE FROM model_requests WHERE id != ?")
        .bind(request)
        .execute(&mut admin)
        .await?;
    sqlx::query("WITH RECURSIVE numbers(n) AS (VALUES(1) UNION ALL SELECT n + 1 FROM numbers WHERE n < 4096) INSERT INTO records (session_id, agent_id, sequence, invocation, runtime_epoch, created_at_ms, payload_version, payload_json) SELECT session_id, agent_id, sequence + n, invocation, runtime_epoch, created_at_ms, payload_version, payload_json FROM records, numbers WHERE sequence = 1")
        .execute(&mut admin).await?;
    let ledger = session.usage_ledger().await?;
    assert!(!ledger.complete);
    assert_eq!(prices().evaluate(&ledger).cache_hit_percent, Some(25.0));
    sqlx::query("DELETE FROM records WHERE sequence > 1")
        .execute(&mut admin)
        .await?;
    let large_input = RecordKind::InputAccepted {
        input: "x".repeat(8 * 1024 * 1024).into(),
    };
    sqlx::query("UPDATE records SET payload_json = ?")
        .bind(serde_json::to_string(&large_input)?)
        .execute(&mut admin)
        .await?;
    assert!(!session.usage_ledger().await?.complete);
    sqlx::query("UPDATE records SET payload_json = ?")
        .bind(serde_json::to_string(&input().kind)?)
        .execute(&mut admin)
        .await?;
    sqlx::query("UPDATE sessions SET accounting_version = 0")
        .execute(&mut admin)
        .await?;
    assert!(!session.usage_ledger().await?.complete);
    sqlx::query("UPDATE sessions SET accounting_version = 1")
        .execute(&mut admin)
        .await?;
    sqlx::query("INSERT INTO conversation_forks (session_id, source_session_id, before_record_id) VALUES (?, ?, 1)")
        .bind(i64::from(session.id())).bind(i64::from(session.id()) + 1)
        .execute(&mut admin).await?;
    assert!(!session.usage_ledger().await?.complete);
    sqlx::query("DELETE FROM conversation_forks")
        .execute(&mut admin)
        .await?;
    assert!(session.usage_ledger().await?.complete);
    let response = Record {
        invocation: InvocationId(1),
        kind: RecordKind::ModelCompleted {
            shared_context: None,
            response: cocoa_core::model::ModelResponse {
                action: cocoa_core::model::ModelAction::Final {
                    answer: "legacy".into(),
                },
                continuation: None,
                usage: None,
            },
        },
    };
    let instruction_state = InstructionState {
        invocation: InvocationId(1),
        scoped_sources: Vec::new(),
        selected_skills: Vec::new(),
        scoped_feedback: Vec::new(),
        skill_feedback: Vec::new(),
    };
    root.append(&response, Some(&instruction_state)).await?;
    assert!(session.usage_ledger().await?.complete);
    root.append(&response, Some(&instruction_state)).await?;
    assert!(!session.usage_ledger().await?.complete);
    Ok(())
}

fn prices() -> PricingState {
    PricingState::from_metadata(&json!({"openai": {"models": {"fixture": {
        "id": "fixture", "cost": {"input":2,"output":8,"cache_read":0.4,"cache_write":3}
    }}}}))
}

fn ledger() -> UsageLedger {
    UsageLedger {
        settings: StoredSessionSettings::Bound {
            effective: Box::new(settings()),
        },
        requests: vec![UsageRequest {
            settings: Some(request_settings()),
            update: update(),
        }],
        complete: true,
        latest_root_usage: Some(usage()),
    }
}

fn usage() -> Usage {
    Usage {
        input_tokens: 1_000_000,
        output_tokens: 250_000,
        input_cache: Some(InputCacheUsage {
            read_tokens: Some(250_000),
            write_tokens: Some(100_000),
        }),
    }
}

fn update() -> RequestUpdate {
    RequestUpdate {
        usage: Some(usage()),
        usage_complete: true,
        outcome: Some(RequestOutcome::Completed),
        error: None,
    }
}

fn request_settings() -> RequestSettings {
    RequestSettings {
        provider: "openai".into(),
        model: "fixture".into(),
        endpoint_default: Some(true),
        thinking_level: None,
        output_tokens: None,
        reasoning_summary: false,
    }
}

fn settings() -> EffectiveSessionSettings {
    EffectiveSessionSettings {
        alias: "main".into(),
        provider: "openai".into(),
        protocol: "openai.responses".into(),
        model: "fixture".into(),
        endpoint: None,
        account: None,
        thinking_level: None,
        reasoning_summary: false,
        execution_version: 1,
        aliases: BTreeMap::from([(
            "main".into(),
            EffectiveAliasSettings {
                provider: "openai".into(),
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
            input_bytes: 65536,
            output_bytes: 65536,
            record_bytes: 65536,
            context_bytes: 1024 * 1024,
            agents_active: 4,
            agents_retained: 8,
            pending_operations: 8,
            stall_timeout_ms: 1000,
            settlement_timeout_ms: 1000,
            shutdown_timeout_ms: 1000,
            message_steering: true,
        },
    }
}

fn limits() -> StoreLimits {
    StoreLimits {
        record_bytes: 65536,
    }
}

fn input() -> Record {
    Record {
        invocation: InvocationId(1),
        kind: RecordKind::InputAccepted {
            input: "hello".into(),
        },
    }
}

fn directory() -> std::io::Result<tempfile::TempDir> {
    let mut builder = tempfile::Builder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    builder.tempdir()
}
