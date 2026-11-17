//! File-backed Store contracts, including rollback, ownership and bounded restore.

#[path = "support/process.rs"]
mod process;

use process::CommandExt;
use std::{
    error::Error,
    path::{Path, PathBuf},
    task::Poll,
};

use cocoa::persistence::{
    AgentMembership, PersistenceError, RetainedAgent, SessionCursor, SessionDatabase, SessionId,
    SessionPublicId, SqliteStore,
};
use cocoa_core::{
    capability::OperationOutcome,
    context::{
        InstructionContent, InstructionDelivery, InstructionFeedback, InstructionState,
        ScopedInstruction, SelectedSkill,
    },
    execution::{Cell, CellResponse, RuntimeState},
    model::{Image, ModelAction, ModelResponse, ProviderData, Usage, UserInput},
    store::{
        AgentId, InMemoryStore, InstructionStateError, InvocationId, OperationId, Record, RecordId,
        RecordKind, Store, StoreError, StoreLimits,
    },
};
use serde_json::json;
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};

#[tokio::test]
async fn run_export_preserves_durable_coverage_failures_and_sanitizes_private_payloads()
-> Result<(), Box<dyn Error>> {
    use cocoa::persistence::{AgentAssignment, ExportLimits};
    use cocoa_core::accounting::{
        RequestAccounting, RequestOutcome, RequestPurpose, RequestSettings,
    };
    use cocoa_core::model::{InputCacheUsage, ModelError};
    let fixture = Fixture::empty().await?;
    let session = fixture
        .database
        .create_session(fixture._directory.path())
        .await?;
    let root = session.root_store_with_settings(limits(), effective_settings())?;
    root.append(&input("sensitive user prompt retained", 1), None)
        .await?;
    let accounting = RequestAccounting::new(root.request_recorder(InvocationId(1)));
    let settings = RequestSettings {
        provider: "fixture".into(),
        model: "actual-root".into(),
        endpoint_default: None,
        thinking_level: Some("high".into()),
        output_tokens: Some(137),
        reasoning_summary: false,
    };
    let mut request = accounting
        .start(RequestPurpose::Response, Some(&settings))
        .await?;
    request
        .report(
            Usage {
                input_tokens: 11,
                output_tokens: 3,
                input_cache: Some(InputCacheUsage {
                    read_tokens: Some(2),
                    write_tokens: Some(1),
                }),
            },
            true,
        )
        .await?;
    request.finish(&Ok::<(), ModelError>(())).await?;
    let private = ProviderData {
        provider: "fixture".into(),
        data: json!({"encrypted_content":"private-continuation-never-export","reasoning_content":"private-reasoning-never-export"}),
    };
    let model = root
        .append(
            &Record {
                invocation: InvocationId(1),
                kind: RecordKind::ModelCompleted {
                    response: ModelResponse {
                        action: ModelAction::Final {
                            answer: "public answer".into(),
                        },
                        continuation: Some(private.clone()),
                        usage: None,
                    },
                },
            },
            Some(&InstructionState {
                invocation: InvocationId(1),
                scoped_sources: Vec::new(),
                selected_skills: Vec::new(),
                scoped_feedback: Vec::new(),
                skill_feedback: Vec::new(),
            }),
        )
        .await?;
    let assignment = AgentAssignment {
        prompt: "child assignment retained".into(),
        alias: "main".into(),
    };
    let start = root
        .append(
            &operation(
                1,
                1,
                "cocoa.spawn",
                json!({"name":"child","prompt":assignment.prompt,"alias":"main"}),
            ),
            None,
        )
        .await?;
    let child = root.admit_agent(start, "child", &assignment, 65536).await?;
    let child_store = session.agent_store(child.agent, limits()).await?;
    child_store.append(&input("child input", 1), None).await?;
    let child_accounting = RequestAccounting::new(child_store.request_recorder(InvocationId(1)));
    let mut request = child_accounting
        .start(
            RequestPurpose::Response,
            Some(&RequestSettings {
                model: "actual-child".into(),
                ..settings.clone()
            }),
        )
        .await?;
    request
        .report(
            Usage {
                input_tokens: 1013,
                output_tokens: 19,
                input_cache: Some(InputCacheUsage {
                    read_tokens: Some(3),
                    write_tokens: Some(2),
                }),
            },
            true,
        )
        .await?;
    request.finish(&Ok::<(), ModelError>(())).await?;
    let mut request = accounting
        .start(RequestPurpose::Compaction, Some(&settings))
        .await?;
    request
        .report(
            Usage {
                input_tokens: 100,
                output_tokens: 1,
                input_cache: None,
            },
            false,
        )
        .await?;
    request
        .report(
            Usage {
                input_tokens: 107,
                output_tokens: 7,
                input_cache: Some(InputCacheUsage {
                    read_tokens: Some(5),
                    write_tokens: Some(3),
                }),
            },
            true,
        )
        .await?;
    request.finish(&Ok::<(), ModelError>(())).await?;
    let mut request = accounting
        .start(RequestPurpose::Response, Some(&settings))
        .await?;
    request
        .finish(&Err::<(), _>(ModelError::Transport {
            message: "response read failed".into(),
        }))
        .await?;
    let mut request = accounting
        .start(RequestPurpose::Response, Some(&settings))
        .await?;
    request
        .report(
            Usage {
                input_tokens: 23,
                output_tokens: 5,
                input_cache: Some(InputCacheUsage {
                    read_tokens: Some(7),
                    write_tokens: None,
                }),
            },
            false,
        )
        .await?;
    drop(request);
    let export = session.export_run(ExportLimits::default()).await?;
    assert_eq!(export.public_id, *session.public_id());
    assert_eq!(export.root_agent, session.root_agent());
    assert_eq!(export.agents.len(), 2);
    assert_eq!(export.requests[1].agent, child.agent);
    assert_eq!(
        export.requests[1]
            .settings
            .as_ref()
            .map(|settings| settings.model.as_str()),
        Some("actual-child")
    );
    assert_eq!(
        export.requests[3].update.outcome,
        Some(RequestOutcome::Transport)
    );
    assert_eq!(
        export.requests[3].update.error.as_deref(),
        Some("model transport failed: response read failed")
    );
    assert_eq!(export.requests[4].update.outcome, None);
    assert_eq!(export.accounting.requests_started, 5);
    assert_eq!(export.accounting.requests_finished, 4);
    assert_eq!(export.accounting.requests_with_usage, 4);
    assert_eq!(export.accounting.requests_with_final_usage, 3);
    assert!(export.accounting.usage.is_none());
    let reported = export
        .accounting
        .reported_usage
        .as_ref()
        .ok_or("missing reported usage")?;
    assert_eq!(reported.input_tokens, 1154);
    assert_eq!(reported.output_tokens, 34);
    assert_eq!(
        reported.input_cache,
        Some(InputCacheUsage {
            read_tokens: Some(17),
            write_tokens: None
        })
    );
    let encoded = serde_json::to_string(&export)?;
    for absent in [
        "private-continuation-never-export",
        "private-reasoning-never-export",
        "account-id",
    ] {
        assert!(!encoded.contains(absent));
    }
    assert!(encoded.contains("sensitive user prompt retained"));
    assert!(export.sensitivity.contains("sensitive"));
    assert_eq!(
        serde_json::to_string(&session.export_run(ExportLimits::default()).await?)?,
        encoded
    );
    assert!(matches!(
        session.export_run(ExportLimits::new(1, 1024 * 1024)?).await,
        Err(PersistenceError::ExportLimit)
    ));
    assert!(matches!(
        session
            .export_run(ExportLimits::new(100, encoded.len() as u64 - 1)?)
            .await,
        Err(PersistenceError::ExportLimit)
    ));
    assert_eq!(
        serde_json::to_string(
            &session
                .export_run(ExportLimits::new(100, encoded.len() as u64)?)
                .await?
        )?,
        encoded
    );
    assert!(
        matches!(root.read_record(model, 65536).await?.map(|record| record.record.kind), Some(RecordKind::ModelCompleted { response }) if response.continuation == Some(private))
    );
    Ok(())
}

#[tokio::test]
async fn conversation_forks_remap_originals_and_survive_reopen_without_future_state()
-> Result<(), Box<dyn Error>> {
    use cocoa_core::store::InvocationOutcome;
    let mut fixture = Fixture::empty().await?;
    let source = fixture
        .database
        .create_session(fixture._directory.path())
        .await?;
    let source_id = source.id();
    let store = source.root_store_with_settings(limits(), effective_settings())?;
    let first = store
        .append(
            &Record {
                invocation: InvocationId(1),
                kind: RecordKind::InputAccepted {
                    input: UserInput::WithImages {
                        text: "original".into(),
                        images: vec![image()?],
                    },
                },
            },
            None,
        )
        .await?;
    let initial_state = InstructionState {
        invocation: InvocationId(1),
        scoped_sources: Vec::new(),
        selected_skills: Vec::new(),
        scoped_feedback: Vec::new(),
        skill_feedback: Vec::new(),
    };
    let model_id = store
        .append(
            &Record {
                invocation: InvocationId(1),
                kind: RecordKind::ModelCompleted {
                    response: ModelResponse {
                        action: ModelAction::Execute {
                            cells: vec![Cell {
                                code: "print(23)".into(),
                            }],
                        },
                        continuation: None,
                        usage: None,
                    },
                },
            },
            Some(&initial_state),
        )
        .await?;
    store
        .append(
            &Record {
                invocation: InvocationId(1),
                kind: RecordKind::CellStarted {
                    model: model_id,
                    cell: 0,
                },
            },
            None,
        )
        .await?;
    store
        .append(
            &Record {
                invocation: InvocationId(1),
                kind: RecordKind::CellCompleted {
                    model: model_id,
                    cell: 0,
                    code: "print(23)".into(),
                    outcome: CellResponse::Interrupted {
                        printed: vec![json!(23)],
                    },
                },
            },
            None,
        )
        .await?;
    store
        .append(
            &Record {
                invocation: InvocationId(1),
                kind: RecordKind::InvocationFinished {
                    outcome: InvocationOutcome::Interrupted,
                },
            },
            None,
        )
        .await?;
    let cut = store.append(&input("future prompt", 2), None).await?;
    let later = InstructionState {
        invocation: InvocationId(2),
        scoped_sources: Vec::new(),
        selected_skills: Vec::new(),
        scoped_feedback: Vec::new(),
        skill_feedback: Vec::new(),
    };
    store
        .append(
            &Record {
                invocation: InvocationId(2),
                ..model()
            },
            Some(&later),
        )
        .await?;
    let before = source.load(limits()).await?;
    let points = source.rewind_points(limits()).await?;
    assert_eq!(points.len(), 2);
    assert_eq!(points[0].record, first);
    assert_eq!(points[0].input.images().len(), 1);
    assert_eq!(points[1].record, cut);
    let fork = source.fork_before(cut, limits()).await?;
    let fork_reference = fork.public_id().to_string();
    let fork_root = fork.root_agent();
    assert_ne!(fork_root, source.root_agent());
    let snapshot = fork.load(limits()).await?;
    assert!(snapshot.context.is_empty());
    assert_eq!(snapshot.agents.len(), 1);
    assert_eq!(snapshot.settings, before.settings);
    let history = snapshot
        .histories
        .get(&fork_root)
        .ok_or("missing fork history")?;
    assert_eq!(history.records.len(), 5);
    assert!(history.instruction_state.is_none());
    assert!(history.checkpoints.is_empty());
    let new_model = history.records[1].id;
    assert_ne!(new_model, model_id);
    assert_ne!(history.records[0].id, first);
    assert!(
        matches!(history.records[2].record.kind, RecordKind::CellStarted { model, .. } if model == new_model)
    );
    assert!(
        matches!(history.records[3].record.kind, RecordKind::CellCompleted { model, .. } if model == new_model)
    );
    assert_eq!(source.load(limits()).await?, before);
    assert!(source.fork_before(model_id, limits()).await.is_err());
    let future: (i64, String) = sqlx::query_as(
        "SELECT id, payload_json FROM records WHERE session_id = ? ORDER BY sequence DESC LIMIT 1",
    )
    .bind(i64::from(source_id))
    .fetch_one(&mut fixture.admin)
    .await?;
    sqlx::query("UPDATE records SET payload_json = '{}' WHERE id = ?")
        .bind(future.0)
        .execute(&mut fixture.admin)
        .await?;
    let isolated = source.fork_before(cut, limits()).await?;
    assert_eq!(
        isolated.root_store(limits())?.load().await?.records.len(),
        5
    );
    drop(isolated);
    sqlx::query("UPDATE records SET payload_json = ? WHERE id = ?")
        .bind(future.1)
        .bind(future.0)
        .execute(&mut fixture.admin)
        .await?;
    assert_eq!(source.load(limits()).await?, before);
    let provenance: (i64, i64) = sqlx::query_as(
        "SELECT source_session_id, before_record_id FROM conversation_forks WHERE session_id = ?",
    )
    .bind(i64::from(fork.id()))
    .fetch_one(&mut fixture.admin)
    .await?;
    assert_eq!(provenance, (i64::from(source_id), i64::from(cut)));
    drop(snapshot);
    drop(fork);
    drop(store);
    drop(source);
    drop(fixture.database);
    let database = SessionDatabase::open(&fixture.path).await?;
    let reopened = database
        .attach_session(&fork_reference, limits())
        .await?
        .recover(limits())
        .await?;
    let store = reopened.root_store(limits())?;
    assert_eq!(store.load().await?.records.len(), 5);
    store
        .append(&input("independent next prompt", 2), None)
        .await?;
    assert_eq!(store.load().await?.records.len(), 6);
    assert_eq!(store.read_record(cut, 64 * 1024).await?, None);
    assert_eq!(store.read_record(first, 64 * 1024).await?, None);
    sqlx::query("DELETE FROM instruction_state WHERE agent_id IN (SELECT id FROM agents WHERE session_id = ?)")
        .bind(i64::from(source_id)).execute(&mut fixture.admin).await?;
    sqlx::query("DELETE FROM records WHERE session_id = ?")
        .bind(i64::from(source_id))
        .execute(&mut fixture.admin)
        .await?;
    sqlx::query("DELETE FROM agents WHERE session_id = ?")
        .bind(i64::from(source_id))
        .execute(&mut fixture.admin)
        .await?;
    sqlx::query("DELETE FROM sessions WHERE id = ?")
        .bind(i64::from(source_id))
        .execute(&mut fixture.admin)
        .await?;
    assert_eq!(store.load().await?.records.len(), 6);
    assert_eq!(reopened.rewind_points(limits()).await?.len(), 2);
    Ok(())
}

#[tokio::test]
async fn conversation_forks_decode_archived_inputs_and_create_empty_prefixes_atomically()
-> Result<(), Box<dyn Error>> {
    use sha2::{Digest, Sha256};
    let mut fixture = Fixture::empty().await?;
    let source = fixture
        .database
        .create_session(fixture._directory.path())
        .await?;
    let store = source.root_store(limits())?;
    let first = store.append(&input("archived prompt", 1), None).await?;
    let payload = serde_json::to_string(&input("archived prompt", 1).kind)?;
    let compressed = miniz_oxide::deflate::compress_to_vec_zlib(payload.as_bytes(), 6);
    sqlx::query("UPDATE records SET payload_json = NULL, archive_zlib = ?, archive_decoded_bytes = ?, archive_sha256 = ?, archive_kind = 'InputAccepted', archive_input_images = 0 WHERE id = ?")
        .bind(compressed.clone()).bind(payload.len() as i64).bind(Sha256::digest(payload.as_bytes()).as_slice())
        .bind(i64::from(first)).execute(&mut fixture.admin).await?;
    let points = source.rewind_points(limits()).await?;
    assert_eq!(points.len(), 1);
    assert_eq!(points[0].input, UserInput::from("archived prompt"));
    let fork = source.fork_before(first, limits()).await?;
    assert!(fork.root_store(limits())?.load().await?.records.is_empty());
    let source_archive: Vec<u8> =
        sqlx::query_scalar("SELECT archive_zlib FROM records WHERE id = ?")
            .bind(i64::from(first))
            .fetch_one(&mut fixture.admin)
            .await?;
    assert_eq!(source_archive, compressed);
    sqlx::raw_sql("CREATE TRIGGER reject_fork BEFORE INSERT ON conversation_forks BEGIN SELECT RAISE(ABORT, 'reject fork'); END;")
        .execute(&mut fixture.admin).await?;
    assert!(source.fork_before(first, limits()).await.is_err());
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions")
        .fetch_one(&mut fixture.admin)
        .await?;
    assert_eq!(count, 2);
    let foreign = fork
        .root_store(limits())?
        .append(&input("foreign", 1), None)
        .await?;
    assert!(source.fork_before(foreign, limits()).await.is_err());
    assert_eq!(source.rewind_points(limits()).await?, points);
    sqlx::query("DROP TRIGGER reject_fork")
        .execute(&mut fixture.admin)
        .await?;
    store.append(&model(), None).await?;
    store
        .append(
            &Record {
                invocation: InvocationId(1),
                kind: RecordKind::InvocationFinished {
                    outcome: cocoa_core::store::InvocationOutcome::Completed,
                },
            },
            None,
        )
        .await?;
    let second = store
        .append(&input("selected later input", 2), None)
        .await?;
    let retained = source.fork_before(second, limits()).await?;
    let history = retained.root_store(limits())?.load().await?;
    assert_eq!(history.records.len(), 3);
    assert_eq!(history.records[0].record, input("archived prompt", 1));
    assert_ne!(history.records[0].id, first);
    let malformed = Record {
        invocation: InvocationId(3),
        kind: RecordKind::CellStarted {
            model: first,
            cell: 0,
        },
    };
    let malformed_id = store.append(&malformed, None).await?;
    let invalid_cut = store
        .append(&input("invalid canonical prefix", 4), None)
        .await?;
    assert!(source.fork_before(invalid_cut, limits()).await.is_err());
    assert!(source.fork_before(malformed_id, limits()).await.is_err());
    sqlx::query("UPDATE sessions SET runtime_epoch = runtime_epoch + 1 WHERE id = ?")
        .bind(i64::from(source.id()))
        .execute(&mut fixture.admin)
        .await?;
    assert!(matches!(
        source.fork_before(first, limits()).await,
        Err(PersistenceError::AgentUnavailable)
    ));
    Ok(())
}

#[test]
fn session_identities_require_positive_integers_and_complete_canonical_references()
-> Result<(), Box<dyn Error>> {
    for value in [i64::MIN, -1, 0] {
        assert!(SessionId::try_from(value).is_err());
    }
    for value in [1, 23, i64::MAX] {
        assert_eq!(i64::from(SessionId::try_from(value)?), value);
    }
    let reference = "sess_84af31e209c74863b0d521a76ef409bd";
    let public_id: SessionPublicId = reference.parse()?;
    assert_eq!(public_id.as_str(), reference);
    assert_eq!(public_id.to_string(), reference);
    for value in [
        "sess_84af31e2",
        "sess_84af31e209c74863b0d521a76ef409b",
        "sess_84af31e209c74863b0d521a76ef409bd0",
        "sess_84AF31e209c74863b0d521a76ef409bd",
        "SESS_84af31e209c74863b0d521a76ef409bd",
        "sess_84af31e209c74863b0d521a76ef409bg",
        "sess_../84af31e209c74863b0d521a76ef40",
        "sess_84af31e209c74863b0d521a76ef409b\0",
        "sess_84af31e209c74863b0d521a76ef409bé",
    ] {
        assert!(
            value.parse::<SessionPublicId>().is_err(),
            "accepted {value}"
        );
    }
    Ok(())
}

#[test]
fn session_cursors_preserve_full_width_positions() -> Result<(), Box<dyn Error>> {
    for (timestamp, id) in [(0, 1), (37, 9_007_199_254_740_993), (i64::MAX, i64::MAX)] {
        let cursor = SessionCursor::new(timestamp, SessionId::try_from(id)?)?;
        assert_eq!(cursor.updated_at_ms(), timestamp);
        assert_eq!(i64::from(cursor.id()), id);
        assert_eq!(cursor.to_string().parse::<SessionCursor>()?, cursor);
        assert_eq!(
            serde_json::to_value(cursor)?,
            json!(format!("v1:{timestamp}:{id}"))
        );
    }
    assert!(matches!(
        SessionCursor::new(-1, SessionId::try_from(1)?),
        Err(PersistenceError::InvalidSessionCursor)
    ));
    for value in [
        "",
        "v2:1:1",
        "1:1",
        "v1:-1:1",
        "v1:1:0",
        "v1:1:-1",
        "v1::1",
        "v1:1:",
        "v1:+1:1",
        "v1:1:2:3",
        "v1:9223372036854775808:1",
        "v1:1:9223372036854775808",
        "v1:1:2\n",
    ] {
        assert!(
            matches!(
                value.parse::<SessionCursor>(),
                Err(PersistenceError::InvalidSessionCursor)
            ),
            "accepted {value:?}"
        );
    }
    Ok(())
}

#[test]
fn retained_membership_preserves_json_shape_and_rejects_incomplete_pairs()
-> Result<(), Box<dyn Error>> {
    use cocoa::persistence::{AgentAssignment, AgentOutcome};

    let root = RetainedAgent {
        id: AgentId::try_from(9_007_199_254_740_993)?,
        name: "root".into(),
        membership: AgentMembership::Root,
        outcome: None,
    };
    let root_json = json!({
        "id":"9007199254740993", "parent":null, "name":"root",
        "assignment":null, "outcome":null
    });
    assert_eq!(serde_json::to_value(&root)?, root_json);
    assert_eq!(serde_json::from_value::<RetainedAgent>(root_json)?, root);
    assert_eq!(
        serde_json::from_value::<RetainedAgent>(json!({
            "id":"9007199254740993", "name":"root"
        }))?,
        root
    );
    let child = RetainedAgent {
        id: AgentId::try_from(9_007_199_254_740_994)?,
        name: "researcher".into(),
        membership: AgentMembership::Child {
            parent: root.id,
            assignment: AgentAssignment {
                prompt: "exact task".into(),
                alias: "main".into(),
            },
        },
        outcome: Some(AgentOutcome::Completed { value: json!(17) }),
    };
    let child_json = json!({
        "id":"9007199254740994", "parent":"9007199254740993", "name":"researcher",
        "assignment":{"prompt":"exact task","alias":"main"},
        "outcome":{"Completed":{"value":17}}
    });
    assert_eq!(serde_json::to_value(&child)?, child_json);
    assert_eq!(
        serde_json::from_value::<RetainedAgent>(child_json.clone())?,
        child
    );
    for field in ["parent", "assignment"] {
        let mut incomplete = child_json.clone();
        incomplete[field] = serde_json::Value::Null;
        assert!(serde_json::from_value::<RetainedAgent>(incomplete.clone()).is_err());
        incomplete
            .as_object_mut()
            .ok_or("child JSON is not an object")?
            .remove(field);
        assert!(serde_json::from_value::<RetainedAgent>(incomplete).is_err());
    }
    Ok(())
}

#[tokio::test]
async fn listing_pages_by_activity_and_directory_with_exact_integer_ids()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::empty().await?;
    let empty = fixture.database.list_sessions(None, 20, None).await?;
    assert!(empty.sessions.is_empty());
    assert_eq!(empty.next_cursor, None);
    for (id, directory, activity) in [
        (7_i64, "/moved/alpha", 400),
        (91, "/moved/beta", 400),
        (9_007_199_254_740_992, "/moved/alpha", 300),
        (9_007_199_254_740_993, "/moved/alpha", 300),
        (13, "/moved/alpha", 100),
        (27, "/moved/alpha", 200),
    ] {
        sqlx::query("INSERT INTO sessions (id, public_id, working_directory, title, created_at_ms, updated_at_ms, last_opened_at_ms, runtime_epoch, settings_version, settings_json) VALUES (?, ?, ?, 'Shared title', 9, ?, 12, 1, 1, '{\"state\":\"unconfigured\"}')")
            .bind(id).bind(format!("sess_{id:032x}")).bind(directory).bind(activity)
            .execute(&mut fixture.admin).await?;
    }
    let mut cursor = None;
    for expected in [
        [91, 7],
        [9_007_199_254_740_993, 9_007_199_254_740_992],
        [27, 13],
    ] {
        let page = fixture.database.list_sessions(None, 2, cursor).await?;
        assert_eq!(
            page.sessions
                .iter()
                .map(|session| i64::from(session.id))
                .collect::<Vec<_>>(),
            expected
        );
        for session in &page.sessions {
            assert_eq!(session.title.as_deref(), Some("Shared title"));
            assert_eq!(session.created_at_ms, 9);
            assert_eq!(session.last_opened_at_ms, 12);
            assert_eq!(
                serde_json::to_value(session)?["id"],
                json!(i64::from(session.id).to_string())
            );
        }
        if let Some(next) = page.next_cursor {
            let last = page.sessions.last().ok_or("empty page with continuation")?;
            assert_eq!(next.id(), last.id);
            assert_eq!(next.updated_at_ms(), last.updated_at_ms);
            assert_ne!(Some(next), cursor);
        }
        cursor = page.next_cursor;
    }
    assert_eq!(cursor, None);
    let mut cursor = None;
    for expected in [
        &[7, 9_007_199_254_740_993][..],
        &[9_007_199_254_740_992, 27],
        &[13],
    ] {
        let page = fixture
            .database
            .list_sessions(Some(Path::new("/moved/alpha")), 2, cursor)
            .await?;
        assert_eq!(
            page.sessions
                .iter()
                .map(|session| i64::from(session.id))
                .collect::<Vec<_>>(),
            expected
        );
        assert!(
            page.sessions
                .iter()
                .all(|session| session.working_directory == Path::new("/moved/alpha"))
        );
        cursor = page.next_cursor;
    }
    assert_eq!(cursor, None);
    assert!(
        fixture
            .database
            .list_sessions(Some(Path::new("/other")), 20, None)
            .await?
            .sessions
            .is_empty()
    );
    for limit in [0, 51, u32::MAX] {
        assert!(matches!(
            fixture.database.list_sessions(None, limit, None).await,
            Err(PersistenceError::InvalidSessionLimit)
        ));
    }
    for directory in ["", "relative", "/invalid\0directory"] {
        assert!(matches!(
            fixture
                .database
                .list_sessions(Some(Path::new(directory)), 20, None)
                .await,
            Err(PersistenceError::InvalidData(_))
        ));
    }
    let too_long = format!("/{}", "x".repeat(16_384));
    assert!(matches!(
        fixture
            .database
            .list_sessions(Some(Path::new(&too_long)), 20, None)
            .await,
        Err(PersistenceError::InvalidData(_))
    ));
    let limit = fixture.database.list_sessions(None, 50, None).await?;
    assert_eq!(limit.sessions.len(), 6);
    assert_eq!(limit.next_cursor, None);
    Ok(())
}

#[tokio::test]
async fn resolution_normalizes_case_and_rejects_global_prefix_ambiguity()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new().await?;
    let first = "sess_deadbeef00000000000000000000000a";
    let second = "sess_deadbeef00000000000000000000000b";
    for (id, public_id, directory) in [(1, first, "/alpha"), (2, second, "/beta")] {
        sqlx::query("UPDATE sessions SET public_id = ?, working_directory = ? WHERE id = ?")
            .bind(public_id)
            .bind(directory)
            .bind(id)
            .execute(&mut fixture.admin)
            .await?;
    }
    for (id, public_id) in [
        (3, "sess_cafebabe00000000000000000000000c"),
        (4, "sess_deadbeef00000000000000000000000c"),
    ] {
        sqlx::query("INSERT INTO sessions (id, public_id, working_directory, created_at_ms, updated_at_ms, last_opened_at_ms, runtime_epoch, settings_version, settings_json) VALUES (?, ?, '/gamma', 0, 100, 0, 1, 1, '{}')")
            .bind(id).bind(public_id).execute(&mut fixture.admin).await?;
    }
    let page = fixture
        .database
        .list_sessions(Some(Path::new("/alpha")), 20, None)
        .await?;
    assert_eq!(page.sessions.len(), 1);
    assert_eq!(i64::from(page.sessions[0].id), 1);
    for reference in [first.to_owned(), first.to_ascii_uppercase()] {
        let session = fixture.database.resolve_session(&reference).await?;
        assert_eq!(i64::from(session.id), 1);
        assert_eq!(session.public_id.as_str(), first);
        assert_eq!(session.title, None);
    }
    assert_eq!(
        i64::from(fixture.database.resolve_session(second).await?.id),
        2
    );
    for reference in [
        "sess_cafebabe",
        "SESS_CAFEBABE",
        "sess_cafebabe00000000000000000000000",
    ] {
        assert_eq!(
            i64::from(fixture.database.resolve_session(reference).await?.id),
            3
        );
    }
    for reference in [
        "sess_deadbeef",
        "SESS_DEADBEEF",
        "sess_deadbeef00000000000000000000000",
    ] {
        match fixture.database.resolve_session(reference).await {
            Err(PersistenceError::AmbiguousSessionId {
                first: candidate_first,
                second: candidate_second,
            }) => {
                assert_eq!(candidate_first.as_str(), first);
                assert_eq!(candidate_second.as_str(), second);
            }
            result => return Err(format!("expected bounded ambiguity, got {result:?}").into()),
        }
    }
    for reference in ["sess_11111111", "sess_deadbeef00000000000000000000000d"] {
        assert!(matches!(
            fixture.database.resolve_session(reference).await,
            Err(PersistenceError::SessionNotFound)
        ));
    }
    for reference in [
        "",
        "deadbeef",
        "sess_deadbee",
        "sess_012345678901234567890123456789012",
        "sess_deadbee*",
        "sess_deadbee?",
        "sess_deadbe[e]",
        "sess_deadbeéf",
        "sess_deadbee\0",
        "sess_' OR 1=1 --",
    ] {
        assert!(
            matches!(
                fixture.database.resolve_session(reference).await,
                Err(PersistenceError::InvalidSessionReference)
            ),
            "accepted {reference:?}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn listing_respects_encoded_byte_budget_without_skipping_rows() -> Result<(), Box<dyn Error>>
{
    let mut fixture = Fixture::empty().await?;
    let directory = format!("/{}", "\u{0001}".repeat(16_383));
    let title = format!("{}界", "\u{0002}".repeat(509));
    for id in 1..=5_i64 {
        sqlx::query("INSERT INTO sessions (id, public_id, working_directory, title, created_at_ms, updated_at_ms, last_opened_at_ms, runtime_epoch, settings_version, settings_json) VALUES (?, ?, ?, ?, 0, 7, 0, 1, 1, '{}')")
            .bind(id).bind(format!("sess_{id:032x}")).bind(&directory).bind(&title).execute(&mut fixture.admin).await?;
    }
    let mut cursor = None;
    for expected in [&[5, 4][..], &[3, 2], &[1]] {
        let page = fixture.database.list_sessions(None, 50, cursor).await?;
        assert!(serde_json::to_vec(&page)?.len() <= 256 * 1024);
        assert_eq!(
            page.sessions
                .iter()
                .map(|session| i64::from(session.id))
                .collect::<Vec<_>>(),
            expected
        );
        for session in &page.sessions {
            assert_eq!(session.working_directory, Path::new(&directory));
            assert_eq!(session.title.as_deref(), Some(title.as_str()));
        }
        cursor = page.next_cursor;
    }
    assert_eq!(cursor, None);
    Ok(())
}

#[tokio::test]
async fn discovery_reads_active_sessions_without_writes_or_execution_payloads()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new().await?;
    let store = fixture.store(1).await?;
    store.append(&input("Durable title", 1), None).await?;
    sqlx::query("UPDATE sessions SET settings_version = 999 WHERE id = 1")
        .execute(&mut fixture.admin)
        .await?;
    sqlx::query("UPDATE records SET payload_version = 999")
        .execute(&mut fixture.admin)
        .await?;
    let other = SessionDatabase::open(&fixture.path).await?;
    let before: Vec<CreationMetadata> = sqlx::query_as("SELECT public_id, working_directory, title, created_at_ms, updated_at_ms, last_opened_at_ms, runtime_epoch, settings_version, settings_json FROM sessions ORDER BY id").fetch_all(&mut fixture.admin).await?;
    let mut transaction = fixture.admin.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("UPDATE sessions SET title = 'Uncommitted title' WHERE id = 1")
        .execute(&mut *transaction)
        .await?;
    let page = other.list_sessions(None, 20, None).await?;
    assert_eq!(page.sessions.len(), 2);
    assert_eq!(page.sessions[0].title.as_deref(), Some("Durable title"));
    let session = other
        .resolve_session("SESS_00000000000000000000000000000001")
        .await?;
    assert_eq!(i64::from(session.id), 1);
    assert_eq!(session.title.as_deref(), Some("Durable title"));
    transaction.rollback().await?;
    let after: Vec<CreationMetadata> = sqlx::query_as("SELECT public_id, working_directory, title, created_at_ms, updated_at_ms, last_opened_at_ms, runtime_epoch, settings_version, settings_json FROM sessions ORDER BY id").fetch_all(&mut fixture.admin).await?;
    assert_eq!(after, before);
    assert!(matches!(
        other.agent_store(AgentId::try_from(1)?, limits()).await,
        Err(PersistenceError::SessionInUse)
    ));
    Ok(())
}

#[tokio::test]
async fn discovery_rejects_invalid_disk_metadata_instead_of_truncating_it()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new().await?;
    sqlx::query("PRAGMA ignore_check_constraints=ON")
        .execute(&mut fixture.admin)
        .await?;
    for title in [String::new(), "界".repeat(171), "x".repeat(1024 * 1024)] {
        sqlx::query("UPDATE sessions SET title = ? WHERE id = 1")
            .bind(&title)
            .execute(&mut fixture.admin)
            .await?;
        assert!(matches!(
            fixture.database.list_sessions(None, 20, None).await,
            Err(PersistenceError::InvalidData(_))
        ));
        assert!(matches!(
            fixture
                .database
                .resolve_session("sess_00000000000000000000000000000001")
                .await,
            Err(PersistenceError::InvalidData(_))
        ));
    }
    sqlx::query("UPDATE sessions SET title = NULL WHERE id = 1")
        .execute(&mut fixture.admin)
        .await?;
    for directory in [
        "relative".to_owned(),
        "/nul\0directory".to_owned(),
        format!("/{}", "x".repeat(16_384)),
    ] {
        sqlx::query("UPDATE sessions SET working_directory = ? WHERE id = 1")
            .bind(directory)
            .execute(&mut fixture.admin)
            .await?;
        assert!(matches!(
            fixture.database.list_sessions(None, 20, None).await,
            Err(PersistenceError::InvalidData(_))
        ));
    }
    sqlx::query(
        "UPDATE sessions SET working_directory = '/alpha', updated_at_ms = -1 WHERE id = 1",
    )
    .execute(&mut fixture.admin)
    .await?;
    assert!(matches!(
        fixture.database.list_sessions(None, 20, None).await,
        Err(PersistenceError::InvalidData(_))
    ));
    sqlx::query("UPDATE sessions SET updated_at_ms = 0, public_id = 'invalid' WHERE id = 1")
        .execute(&mut fixture.admin)
        .await?;
    assert!(matches!(
        fixture.database.list_sessions(None, 20, None).await,
        Err(PersistenceError::InvalidData(_))
    ));
    Ok(())
}

#[tokio::test]
async fn creation_commits_an_empty_session_and_root_without_configuration()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::empty().await?;
    let nested = fixture._directory.path().join("nested");
    std::fs::create_dir(&nested)?;
    let session = fixture.database.create_session(&nested.join("..")).await?;
    let root = session.root_agent();
    let public_id = session.public_id().clone();
    assert_eq!(public_id.as_str().parse::<SessionPublicId>()?, public_id);
    let metadata: CreationMetadata = sqlx::query_as(
        "SELECT public_id, working_directory, title, created_at_ms, updated_at_ms, \
         last_opened_at_ms, runtime_epoch, settings_version, settings_json FROM sessions WHERE id = ?",
    )
    .bind(i64::from(session.id()))
    .fetch_one(&mut fixture.admin)
    .await?;
    assert_eq!(metadata.public_id, public_id.as_str());
    assert_eq!(
        metadata.working_directory,
        fixture
            ._directory
            .path()
            .canonicalize()?
            .to_str()
            .ok_or("directory encoding")?
    );
    assert_eq!(metadata.title, None);
    assert!(metadata.created_at_ms > 0);
    assert_eq!(metadata.updated_at_ms, metadata.created_at_ms);
    assert_eq!(metadata.last_opened_at_ms, metadata.created_at_ms);
    assert_eq!(metadata.runtime_epoch, 1);
    assert_eq!(metadata.settings_version, 1);
    let page = fixture.database.list_sessions(None, 20, None).await?;
    assert_eq!(page.sessions.len(), 1);
    assert_eq!(page.sessions[0].id, session.id());
    assert_eq!(page.sessions[0].title, None);
    assert_eq!(page.next_cursor, None);
    assert_eq!(
        fixture.database.resolve_session(public_id.as_str()).await?,
        page.sessions[0]
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&metadata.settings_json)?,
        json!({"state": "unconfigured"})
    );
    let roots: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM agents WHERE session_id = ? AND parent_agent_id IS NULL \
         AND assignment_json IS NULL AND outcome_json IS NULL AND lifecycle = 'participating'",
    )
    .bind(i64::from(session.id()))
    .fetch_all(&mut fixture.admin)
    .await?;
    assert_eq!(roots, vec![i64::from(root)]);
    let store = session.root_store(limits())?;
    let empty = store.load().await?;
    assert!(empty.records.is_empty());
    assert_eq!(empty.instruction_state, None);
    assert!(matches!(
        session.root_store(StoreLimits { record_bytes: 0 }),
        Err(PersistenceError::Store(StoreError::InvalidLimits))
    ));
    let instruction_store = session.root_store_with_instruction_state(limits())?;
    assert_eq!(
        instruction_store.append(&model(), None).await,
        Err(StoreError::InstructionStateRequired)
    );
    drop(instruction_store);
    let record = input("  Save\nthis session  ", 1);
    let record_id = store.append(&record, None).await?;
    let title: Option<String> = sqlx::query_scalar("SELECT title FROM sessions WHERE id = ?")
        .bind(i64::from(session.id()))
        .fetch_one(&mut fixture.admin)
        .await?;
    assert_eq!(title.as_deref(), Some("Save this session"));
    let path = fixture.path.clone();
    drop(session);
    drop(store);
    fixture.admin.close().await?;
    drop(fixture.database);
    let reopened = SessionDatabase::open(&path).await?;
    let store = reopened.agent_store(root, limits()).await?;
    let retained = store.load().await?;
    assert_eq!(retained.records[0].id, record_id);
    assert_eq!(retained.into_records(), vec![record]);
    Ok(())
}

#[tokio::test]
async fn creation_owns_its_lock_and_keeps_same_named_roots_isolated() -> Result<(), Box<dyn Error>>
{
    let mut fixture = Fixture::new().await?;
    let other = SessionDatabase::open(&fixture.path).await?;
    let (first, second) = tokio::join!(
        fixture.database.create_session(fixture._directory.path()),
        other.create_session(fixture._directory.path())
    );
    let first = first?;
    let second = second?;
    assert_ne!(first.id(), second.id());
    assert_ne!(first.public_id(), second.public_id());
    assert_ne!(first.root_agent(), second.root_agent());
    assert_ne!(i64::from(first.id()), i64::from(first.root_agent()));
    assert!(matches!(
        other.agent_store(first.root_agent(), limits()).await,
        Err(PersistenceError::SessionInUse)
    ));
    assert!(matches!(
        fixture
            .database
            .agent_store(second.root_agent(), limits())
            .await,
        Err(PersistenceError::SessionInUse)
    ));
    let root_name: String = sqlx::query_scalar("SELECT name FROM agents WHERE id = ?")
        .bind(i64::from(first.root_agent()))
        .fetch_one(&mut fixture.admin)
        .await?;
    assert_eq!(root_name, "root");
    let root = first.root_agent();
    let store = first.root_store(limits())?;
    let clone = first.clone();
    drop(first);
    drop(clone);
    assert!(matches!(
        other.agent_store(root, limits()).await,
        Err(PersistenceError::SessionInUse)
    ));
    store.append(&input("first root", 1), None).await?;
    let second_store = second.root_store(limits())?;
    assert!(second_store.load().await?.records.is_empty());
    second_store.append(&input("second root", 7), None).await?;
    assert_eq!(
        second_store.load().await?.into_records(),
        vec![input("second root", 7)]
    );
    drop(store);
    let reacquired = other.agent_store(root, limits()).await?;
    assert_eq!(
        reacquired.load().await?.into_records(),
        vec![input("first root", 1)]
    );
    Ok(())
}

#[tokio::test]
async fn invalid_directories_and_failed_root_insertion_leave_no_partial_session()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::empty().await?;
    for path in [
        PathBuf::new(),
        fixture._directory.path().join("missing"),
        fixture.path.clone(),
        PathBuf::from("x".repeat(16_385)),
    ] {
        assert!(fixture.database.create_session(&path).await.is_err());
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        let path = fixture
            ._directory
            .path()
            .join(std::ffi::OsString::from_vec(vec![0xff]));
        assert!(matches!(
            fixture.database.create_session(&path).await,
            Err(PersistenceError::InvalidData("session working directory"))
        ));
    }
    sqlx::raw_sql("CREATE TRIGGER reject_root BEFORE INSERT ON agents BEGIN SELECT RAISE(ABORT, 'fixture root rejection'); END;")
        .execute(&mut fixture.admin).await?;
    assert!(matches!(
        fixture
            .database
            .create_session(fixture._directory.path())
            .await,
        Err(PersistenceError::Database(_))
    ));
    let counts: (i64, i64) =
        sqlx::query_as("SELECT (SELECT count(*) FROM sessions), (SELECT count(*) FROM agents)")
            .fetch_one(&mut fixture.admin)
            .await?;
    assert_eq!(counts, (0, 0));
    sqlx::query("DROP TRIGGER reject_root")
        .execute(&mut fixture.admin)
        .await?;
    sqlx::raw_sql(
        "CREATE TRIGGER ignore_root BEFORE INSERT ON agents BEGIN SELECT RAISE(IGNORE); END;",
    )
    .execute(&mut fixture.admin)
    .await?;
    assert!(matches!(
        fixture
            .database
            .create_session(fixture._directory.path())
            .await,
        Err(PersistenceError::InvalidData(
            "root insertion did not create one agent"
        ))
    ));
    let counts: (i64, i64) =
        sqlx::query_as("SELECT (SELECT count(*) FROM sessions), (SELECT count(*) FROM agents)")
            .fetch_one(&mut fixture.admin)
            .await?;
    assert_eq!(counts, (0, 0));
    sqlx::query("DROP TRIGGER ignore_root")
        .execute(&mut fixture.admin)
        .await?;
    let session = fixture
        .database
        .create_session(fixture._directory.path())
        .await?;
    assert!(
        session
            .root_store(limits())?
            .load()
            .await?
            .records
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn creation_rejects_identity_exhaustion_without_random_integer_fallback()
-> Result<(), Box<dyn Error>> {
    for (statement, related) in [
        (
            "UPDATE sessions SET id = ? WHERE id = 2",
            Some("UPDATE agents SET session_id = ? WHERE session_id = 2"),
        ),
        ("UPDATE agents SET id = ? WHERE id = 4", None),
    ] {
        let mut fixture = Fixture::new().await?;
        let mut transaction = fixture.admin.begin().await?;
        sqlx::query("PRAGMA defer_foreign_keys = ON")
            .execute(&mut *transaction)
            .await?;
        sqlx::query(statement)
            .bind(i64::MAX - 1)
            .execute(&mut *transaction)
            .await?;
        if let Some(related) = related {
            sqlx::query(related)
                .bind(i64::MAX - 1)
                .execute(&mut *transaction)
                .await?;
        }
        transaction.commit().await?;
        let last = fixture
            .database
            .create_session(fixture._directory.path())
            .await?;
        if related.is_some() {
            assert_eq!(i64::from(last.id()), i64::MAX);
        } else {
            assert_eq!(i64::from(last.root_agent()), i64::MAX);
        }
        assert!(last.root_store(limits())?.load().await?.records.is_empty());
        assert!(matches!(
            fixture
                .database
                .create_session(fixture._directory.path())
                .await,
            Err(PersistenceError::Store(StoreError::IdentityExhausted))
        ));
        let counts: (i64, i64) =
            sqlx::query_as("SELECT (SELECT count(*) FROM sessions), (SELECT count(*) FROM agents)")
                .fetch_one(&mut fixture.admin)
                .await?;
        assert_eq!(counts, (3, 5));
    }
    Ok(())
}

#[tokio::test]
async fn public_id_collisions_are_bounded_and_rolled_back() -> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::empty().await?;
    sqlx::raw_sql(
        "CREATE TRIGGER collide_public_id BEFORE INSERT ON sessions WHEN NEW.title IS NULL BEGIN \
         INSERT INTO sessions (id, public_id, working_directory, title, created_at_ms, updated_at_ms, \
         last_opened_at_ms, runtime_epoch, settings_version, settings_json) \
         VALUES (NEW.id + 1, NEW.public_id, NEW.working_directory, 'collision', 0, 0, 0, 1, 1, '{}'); END;",
    ).execute(&mut fixture.admin).await?;
    assert!(matches!(
        fixture
            .database
            .create_session(fixture._directory.path())
            .await,
        Err(PersistenceError::PublicIdExhausted)
    ));
    let counts: (i64, i64) =
        sqlx::query_as("SELECT (SELECT count(*) FROM sessions), (SELECT count(*) FROM agents)")
            .fetch_one(&mut fixture.admin)
            .await?;
    assert_eq!(counts, (0, 0));
    sqlx::query("DROP TRIGGER collide_public_id")
        .execute(&mut fixture.admin)
        .await?;
    fixture
        .database
        .create_session(fixture._directory.path())
        .await?;
    Ok(())
}

#[tokio::test]
async fn attachment_capacity_rejects_creation_before_committing_another_session()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::empty().await?;
    let mut sessions = Vec::with_capacity(32);
    for _ in 0..32 {
        sessions.push(
            fixture
                .database
                .create_session(fixture._directory.path())
                .await?,
        );
    }
    assert!(matches!(
        fixture
            .database
            .create_session(fixture._directory.path())
            .await,
        Err(PersistenceError::Busy)
    ));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions")
        .fetch_one(&mut fixture.admin)
        .await?;
    assert_eq!(count, 32);
    drop(sessions.pop());
    let next = fixture
        .database
        .create_session(fixture._directory.path())
        .await?;
    assert_eq!(i64::from(next.id()), 33);
    assert!(next.root_store(limits())?.load().await?.records.is_empty());
    Ok(())
}

#[tokio::test]
async fn abandoned_creation_settles_once_and_releases_its_session_ownership()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::empty().await?;
    let blocker = fixture.admin.begin_with("BEGIN IMMEDIATE").await?;
    let mut creation = Box::pin(fixture.database.create_session(fixture._directory.path()));
    assert!(matches!(futures_util::poll!(&mut creation), Poll::Pending));
    drop(creation);
    blocker.rollback().await?;
    let other = SessionDatabase::open(&fixture.path).await?;
    let mut recovered = None;
    for _ in 0..500 {
        let root: Option<i64> =
            sqlx::query_scalar("SELECT id FROM agents WHERE parent_agent_id IS NULL")
                .fetch_optional(&mut fixture.admin)
                .await?;
        if let Some(root) = root {
            match other.agent_store(AgentId::try_from(root)?, limits()).await {
                Ok(store) => {
                    recovered = Some(store);
                    break;
                }
                Err(PersistenceError::SessionInUse) => {}
                Err(error) => return Err(error.into()),
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let store = recovered.ok_or("abandoned session ownership did not settle")?;
    assert!(store.load().await?.records.is_empty());
    let counts: (i64, i64) =
        sqlx::query_as("SELECT (SELECT count(*) FROM sessions), (SELECT count(*) FROM agents)")
            .fetch_one(&mut fixture.admin)
            .await?;
    assert_eq!(counts, (1, 1));
    store
        .append(&input("after abandoned creation", 1), None)
        .await?;
    assert_eq!(
        store.load().await?.into_records(),
        vec![input("after abandoned creation", 1)]
    );
    Ok(())
}

#[tokio::test]
async fn exact_history_and_typed_state_survive_reopening() -> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new().await?;
    let store = fixture
        .database
        .agent_store_with_instruction_state(AgentId::try_from(1)?, limits())
        .await?;
    assert_eq!(
        store.append(&model(), None).await,
        Err(StoreError::InstructionStateRequired)
    );
    assert!(store.load().await?.records.is_empty());
    let pending = instructions();
    let first = Record {
        invocation: InvocationId(1),
        kind: RecordKind::InputAccepted {
            input: UserInput::WithImages {
                text: "Keep 🦀, quotes \" and\nnewlines".into(),
                images: vec![image()?],
            },
        },
    };
    let first_id = store.append(&first, Some(&pending)).await?;
    let cell = cell(1);
    let cell_id = store.append(&cell, None).await?;
    let mut delivered = pending;
    delivered.scoped_sources[0].delivery = InstructionDelivery::Delivered;
    delivered.selected_skills[0].delivery = InstructionDelivery::Delivered;
    delivered.scoped_feedback.push(InstructionFeedback {
        record: cell_id,
        text: "Captured rules before later edits".into(),
    });
    delivered.skill_feedback.push(InstructionFeedback {
        record: cell_id,
        text: "Captured skill".into(),
    });
    let completed = Record {
        invocation: InvocationId(1),
        kind: RecordKind::ModelCompleted {
            response: ModelResponse {
                action: ModelAction::Execute {
                    cells: vec![Cell {
                        code: "print(23)".into(),
                    }],
                },
                continuation: Some(ProviderData {
                    provider: "fixture".into(),
                    data: json!({
                        "opaque": [7, {"token": "kept", "score": 1.23456789}],
                        "numbers": [
                            51.248178375505404,
                            7.101215824554616e260,
                            f64::MIN_POSITIVE,
                            f64::from_bits(1),
                            -0.0,
                            i64::MIN,
                            u64::MAX,
                        ],
                    }),
                }),
                usage: Some(Usage {
                    input_tokens: 71,
                    output_tokens: 13,
                    input_cache: Some(cocoa_core::model::InputCacheUsage {
                        read_tokens: Some(23),
                        write_tokens: Some(11),
                    }),
                }),
            },
        },
    };
    let model_id = store.append(&completed, Some(&delivered)).await?;
    assert_ne!(first_id, cell_id);
    assert_ne!(cell_id, model_id);
    let started = Record {
        invocation: InvocationId(1),
        kind: RecordKind::OperationStarted {
            operation: OperationId(9),
            name: "fixture.write".into(),
            arguments: json!({"path": "file", "value": [7, 23]}),
        },
    };
    let finished = Record {
        invocation: InvocationId(1),
        kind: RecordKind::OperationFinished {
            operation: OperationId(9),
            outcome: OperationOutcome::Completed {
                value: json!({"written": 2}),
            },
        },
    };
    let cancelled = Record {
        invocation: InvocationId(1),
        kind: RecordKind::TurnCancelled {
            runtime: RuntimeState::Reset,
        },
    };
    let unmatched = Record {
        invocation: InvocationId(2),
        kind: RecordKind::OperationStarted {
            operation: OperationId(1),
            name: "fixture.unknown".into(),
            arguments: json!({}),
        },
    };
    for record in [&started, &finished, &cancelled, &unmatched] {
        store.append(record, None).await?;
    }
    let before = store.load().await?;
    assert_eq!(before.instruction_state.as_ref(), Some(&delivered));
    assert_eq!(
        before.clone().into_records(),
        vec![
            first, cell, completed, started, finished, cancelled, unmatched
        ]
    );
    for (index, record) in before.records.iter().enumerate() {
        assert_eq!(i64::from(record.sequence), index as i64 + 1);
    }
    drop(store);
    drop(fixture.database);
    let reopened = SessionDatabase::open(&fixture.path).await?;
    let store = reopened
        .agent_store(AgentId::try_from(1)?, limits())
        .await?;
    assert_eq!(store.load().await?, before);
    assert_eq!(
        store.append(&model(), None).await,
        Err(StoreError::InstructionStateRequired)
    );
    let last = store.append(&input("new input", 3), None).await?;
    let after = store.load().await?;
    assert_eq!(after.records[7].id, last);
    assert_eq!(i64::from(after.records[7].sequence), 8);
    assert_eq!(&after.records[..7], before.records.as_slice());
    assert_eq!(after.instruction_state, before.instruction_state);
    Ok(())
}

#[tokio::test]
async fn append_rejects_payloads_that_cannot_be_restored() -> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new().await?;
    let store = fixture.store(1).await?;
    let mut arguments = json!("nested value");
    for _ in 0..160 {
        arguments = serde_json::Value::Array(vec![arguments]);
    }
    let record = Record {
        invocation: InvocationId(1),
        kind: RecordKind::OperationStarted {
            operation: OperationId(1),
            name: "fixture.nested".into(),
            arguments,
        },
    };
    assert_eq!(
        store.append(&record, Some(&instructions())).await,
        Err(StoreError::Encoding)
    );
    let snapshot = store.load().await?;
    assert!(snapshot.records.is_empty());
    assert!(snapshot.instruction_state.is_none());
    let accepted = input("still writable", 1);
    store.append(&accepted, Some(&instructions())).await?;
    let snapshot = store.load().await?;
    assert_eq!(i64::from(snapshot.records[0].sequence), 1);
    assert_eq!(snapshot.instruction_state, Some(instructions()));
    assert_eq!(snapshot.into_records(), vec![accepted]);
    Ok(())
}

#[tokio::test]
async fn record_limit_matches_memory_at_exact_encoded_boundaries() -> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new().await?;
    let record = input("escaped \"\n and é🦀", 19);
    let mut state = instructions();
    state.invocation = InvocationId(19);
    let record_bytes = serde_json::to_vec(&record)?.len();
    for (index, (single, expected)) in [
        (record_bytes, None),
        (record_bytes - 1, Some(StoreError::RecordLimit)),
    ]
    .into_iter()
    .enumerate()
    {
        let agent = AgentId::try_from(index as i64 + 1)?;
        let limits = StoreLimits {
            record_bytes: single,
        };
        let persistent = fixture.database.agent_store(agent, limits).await?;
        let memory = InMemoryStore::new(limits)?;
        let actual = persistent.append(&record, Some(&state)).await;
        let control = memory.append(&record, Some(&state)).await;
        match expected {
            Some(expected) => {
                assert_eq!(actual, Err(expected.clone()));
                assert_eq!(control, Err(expected));
            }
            None => {
                actual?;
                control?;
                persistent.append(&record, None).await?;
                memory.append(&record, None).await?;
            }
        }
        let actual = persistent.load().await?;
        let control = memory.load().await?;
        assert_eq!(actual.instruction_state, control.instruction_state);
        assert_eq!(actual.into_records(), control.into_records());
    }
    let maximum = fixture
        .database
        .agent_store(AgentId::try_from(1)?, limits())
        .await?;
    assert_eq!(
        maximum.load().await?.into_records(),
        vec![record.clone(), record]
    );
    assert!(matches!(
        fixture
            .database
            .agent_store(
                AgentId::try_from(1)?,
                StoreLimits {
                    record_bytes: 256 * 1024 * 1024 + 1,
                },
            )
            .await,
        Err(PersistenceError::Store(StoreError::InvalidLimits))
    ));
    Ok(())
}

#[tokio::test]
async fn database_failures_roll_back_record_state_title_activity_and_sequence()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new().await?;
    let store = fixture.store(1).await?;
    sqlx::raw_sql("CREATE TRIGGER reject_session_update BEFORE UPDATE ON sessions BEGIN SELECT RAISE(ABORT, 'fixture'); END;")
        .execute(&mut fixture.admin).await?;
    assert!(matches!(
        store
            .append(&input("must not become a title", 1), Some(&instructions()))
            .await,
        Err(StoreError::Backend { .. })
    ));
    assert!(store.load().await?.records.is_empty());
    assert!(store.load().await?.instruction_state.is_none());
    let metadata: (Option<String>, i64) =
        sqlx::query_as("SELECT title, updated_at_ms FROM sessions WHERE id = 1")
            .fetch_one(&mut fixture.admin)
            .await?;
    assert_eq!(metadata, (None, 0));
    sqlx::query("DROP TRIGGER reject_session_update")
        .execute(&mut fixture.admin)
        .await?;
    store
        .append(&input("accepted title", 1), Some(&instructions()))
        .await?;
    let before = store.load().await?;
    assert_eq!(i64::from(before.records[0].sequence), 1);
    sqlx::raw_sql("CREATE TRIGGER reject_session_update BEFORE UPDATE ON sessions BEGIN SELECT RAISE(ABORT, 'fixture'); END;")
        .execute(&mut fixture.admin).await?;
    let mut delivered = instructions();
    delivered.scoped_sources[0].delivery = InstructionDelivery::Delivered;
    delivered.selected_skills[0].delivery = InstructionDelivery::Delivered;
    assert!(store.append(&model(), Some(&delivered)).await.is_err());
    assert_eq!(store.load().await?, before);
    sqlx::query("DROP TRIGGER reject_session_update")
        .execute(&mut fixture.admin)
        .await?;
    store.append(&model(), Some(&delivered)).await?;
    assert_eq!(i64::from(store.load().await?.records[1].sequence), 2);
    Ok(())
}

#[tokio::test]
async fn agent_ids_isolate_nonunique_names_and_feedback() -> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new().await?;
    let first = fixture.store(2).await?;
    let second = fixture.store(3).await?;
    let other_session = fixture.store(4).await?;
    let own_cell = first.append(&cell(1), Some(&instructions())).await?;
    let foreign_cell = second.append(&cell(1), None).await?;
    let other_cell = other_session.append(&cell(1), None).await?;
    let not_cell = first
        .append(&input("not a feedback target", 1), None)
        .await?;
    let future_cell = first.append(&cell(2), None).await?;
    assert_ne!(own_cell, foreign_cell);
    assert_ne!(own_cell, other_cell);
    let before = first.load().await?;
    for record in [foreign_cell, other_cell, not_cell, future_cell] {
        let mut candidate = instructions();
        candidate.scoped_feedback.push(InstructionFeedback {
            record,
            text: "captured".into(),
        });
        assert_eq!(
            first.append(&model(), Some(&candidate)).await,
            Err(StoreError::InstructionState(
                InstructionStateError::FeedbackTarget { record }
            ))
        );
        assert_eq!(first.load().await?, before);
    }
    let mut invalid_delivery = instructions();
    invalid_delivery.scoped_sources[0].delivery = InstructionDelivery::Delivered;
    assert_eq!(
        first
            .append(&input("not model delivery", 1), Some(&invalid_delivery))
            .await,
        Err(StoreError::InstructionState(
            InstructionStateError::Delivery
        ))
    );
    let mut lost_pending = instructions();
    lost_pending.selected_skills.clear();
    assert_eq!(
        first.append(&model(), Some(&lost_pending)).await,
        Err(StoreError::InstructionState(
            InstructionStateError::Delivery
        ))
    );
    let mut accepted = instructions();
    accepted.scoped_feedback.push(InstructionFeedback {
        record: own_cell,
        text: "retained text".into(),
    });
    first.append(&model(), Some(&accepted)).await?;
    let retained = first.load().await?;
    accepted.scoped_feedback[0].text = "changed history".into();
    assert_eq!(
        first.append(&model(), Some(&accepted)).await,
        Err(StoreError::InstructionState(
            InstructionStateError::Feedback
        ))
    );
    assert_eq!(first.load().await?, retained);
    assert_eq!(second.load().await?.into_records(), vec![cell(1)]);
    assert_eq!(other_session.load().await?.into_records(), vec![cell(1)]);
    Ok(())
}

#[tokio::test]
async fn row_pages_byte_pages_and_batched_feedback_restore_the_complete_snapshot()
-> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new().await?;
    let store = fixture
        .database
        .agent_store(
            AgentId::try_from(1)?,
            StoreLimits {
                record_bytes: 8 * 1024 * 1024,
            },
        )
        .await?;
    let mut expected = Vec::new();
    let mut state = instructions();
    for index in 0..130 {
        let record = Record {
            invocation: InvocationId(1),
            kind: RecordKind::CellCompleted {
                model: RecordId::try_from(1)?,
                cell: index,
                code: format!("print({index})"),
                outcome: CellResponse::Completed {
                    printed: vec![json!(index * 7)],
                },
            },
        };
        let id = store.append(&record, None).await?;
        state.scoped_feedback.push(InstructionFeedback {
            record: id,
            text: format!("rules for {index}"),
        });
        expected.push(record);
    }
    for (character, bytes) in [
        ('x', 2300 * 1024),
        ('y', 2300 * 1024),
        ('z', 5 * 1024 * 1024),
    ] {
        let record = input(&character.to_string().repeat(bytes), 1);
        store.append(&record, None).await?;
        expected.push(record);
    }
    store.append(&model(), Some(&state)).await?;
    expected.push(model());
    let actual = store.load().await?;
    assert_eq!(actual.instruction_state, Some(state));
    assert_eq!(actual.records.len(), 134);
    assert_eq!(i64::from(actual.records[64].sequence), 65);
    assert_eq!(i64::from(actual.records[133].sequence), 134);
    assert_eq!(actual.into_records(), expected);
    Ok(())
}

#[tokio::test]
async fn invalid_and_oversized_disk_payloads_are_rejected_without_rewriting()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new().await?;
    let store = fixture.store(1).await?;
    store
        .append(&input("retained", 1), Some(&instructions()))
        .await?;
    let before = store.load().await?;
    for statement in [
        "UPDATE records SET payload_version = 1",
        "UPDATE records SET payload_version = 3",
        "UPDATE records SET runtime_epoch = 2",
        "UPDATE records SET sequence = 7",
        "UPDATE records SET payload_json = '{\"UnknownKind\":{}}'",
        "UPDATE instruction_state SET format_version = 2",
        "UPDATE instruction_state SET state_json = '{}'",
    ] {
        let mut transaction = fixture.admin.begin().await?;
        sqlx::query(statement).execute(&mut *transaction).await?;
        transaction.commit().await?;
        assert!(
            store.load().await.is_err(),
            "accepted corrupt data from {statement}"
        );
        sqlx::query("UPDATE records SET payload_version = 2, runtime_epoch = 1, sequence = 1, payload_json = ?")
            .bind(serde_json::to_string(&before.records[0].record.kind)?).execute(&mut fixture.admin).await?;
        sqlx::query("UPDATE instruction_state SET format_version = 1, state_json = ?")
            .bind(serde_json::to_string(&instructions())?)
            .execute(&mut fixture.admin)
            .await?;
        assert_eq!(store.load().await?, before);
    }
    let smaller = fixture
        .database
        .agent_store(AgentId::try_from(1)?, StoreLimits { record_bytes: 50 })
        .await?;
    assert_eq!(smaller.load().await, Err(StoreError::RecordLimit));
    let oversized = serde_json::to_string(&RecordKind::InputAccepted {
        input: "x".repeat(70 * 1024).into(),
    })?;
    sqlx::query("UPDATE records SET payload_json = ?")
        .bind(&oversized)
        .execute(&mut fixture.admin)
        .await?;
    assert_eq!(store.load().await, Err(StoreError::RecordLimit));
    let retained: String = sqlx::query_scalar("SELECT payload_json FROM records WHERE id = ?")
        .bind(i64::from(before.records[0].id))
        .fetch_one(&mut fixture.admin)
        .await?;
    assert_eq!(retained, oversized);
    Ok(())
}

#[tokio::test]
async fn disk_feedback_must_resolve_to_same_agent_completed_cells() -> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new().await?;
    let first = fixture.store(2).await?;
    let second = fixture.store(3).await?;
    first
        .append(&input("own state", 1), Some(&instructions()))
        .await?;
    let foreign = second.append(&cell(1), None).await?;
    let mut state = instructions();
    state.scoped_feedback.push(InstructionFeedback {
        record: foreign,
        text: "foreign".into(),
    });
    let payload = serde_json::to_string(&state)?;
    sqlx::query("UPDATE instruction_state SET state_json = ? WHERE agent_id = 2")
        .bind(&payload)
        .execute(&mut fixture.admin)
        .await?;
    assert_eq!(
        first.load().await,
        Err(StoreError::InstructionState(
            InstructionStateError::FeedbackTarget { record: foreign }
        ))
    );
    let retained: String =
        sqlx::query_scalar("SELECT state_json FROM instruction_state WHERE agent_id = 2")
            .fetch_one(&mut fixture.admin)
            .await?;
    assert_eq!(retained, payload);
    Ok(())
}

#[tokio::test]
async fn stale_epochs_retirement_and_invalid_ids_never_append() -> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new().await?;
    let store = fixture.store(2).await?;
    for invocation in [0, i64::MAX as u64 + 1, u64::MAX] {
        assert_eq!(
            store.append(&input("invalid", invocation), None).await,
            Err(StoreError::InvalidRecord)
        );
    }
    for operation in [0, u64::MAX] {
        let record = Record {
            invocation: InvocationId(1),
            kind: RecordKind::OperationStarted {
                operation: OperationId(operation),
                name: "fixture".into(),
                arguments: json!({}),
            },
        };
        assert_eq!(
            store.append(&record, None).await,
            Err(StoreError::InvalidRecord)
        );
    }
    let maximum = input("valid maximum invocation", i64::MAX as u64);
    store.append(&maximum, None).await?;
    assert_eq!(store.load().await?.into_records(), vec![maximum]);
    sqlx::query("UPDATE sessions SET runtime_epoch = 2 WHERE id = 1")
        .execute(&mut fixture.admin)
        .await?;
    assert!(store.append(&input("stale", 1), None).await.is_err());
    assert!(store.load().await.is_err());
    drop(store);
    let current = fixture.store(2).await?;
    assert_eq!(current.load().await?.records.len(), 1);
    current.append(&input("new epoch", 2), None).await?;
    let epochs: Vec<i64> = sqlx::query_scalar(
        "SELECT runtime_epoch FROM records WHERE agent_id = 2 ORDER BY sequence",
    )
    .fetch_all(&mut fixture.admin)
    .await?;
    assert_eq!(epochs, vec![1, 2]);
    sqlx::query("UPDATE agents SET lifecycle = 'retired' WHERE id = 2")
        .execute(&mut fixture.admin)
        .await?;
    assert!(current.append(&input("retired", 3), None).await.is_err());
    assert!(current.load().await.is_err());
    assert!(matches!(
        fixture.store(2).await,
        Err(PersistenceError::AgentUnavailable)
    ));
    assert!(matches!(
        fixture.store(99).await,
        Err(PersistenceError::AgentUnavailable)
    ));
    Ok(())
}

#[tokio::test]
async fn ownership_excludes_other_attachments_but_not_other_sessions() -> Result<(), Box<dyn Error>>
{
    let mut fixture = Fixture::new().await?;
    let first = fixture.store(1).await?;
    let other = SessionDatabase::open(&fixture.path).await?;
    assert!(matches!(
        other.agent_store(AgentId::try_from(1)?, limits()).await,
        Err(PersistenceError::SessionInUse)
    ));
    let second = other.agent_store(AgentId::try_from(4)?, limits()).await?;
    let first_record = input("session one", 1);
    let second_record = input("session two", 7);
    let (first_id, second_id) = tokio::join!(
        first.append(&first_record, None),
        second.append(&second_record, None)
    );
    assert_ne!(first_id?, second_id?);
    assert_eq!(first.load().await?.into_records(), vec![first_record]);
    assert_eq!(second.load().await?.into_records(), vec![second_record]);
    drop(first);
    let released = other.agent_store(AgentId::try_from(1)?, limits()).await?;
    let before = released.load().await?;
    assert_eq!(before.records.len(), 1);
    let blocker = fixture.admin.begin_with("BEGIN IMMEDIATE").await?;
    assert!(matches!(
        released.append(&input("writer unavailable", 2), None).await,
        Err(StoreError::Backend { .. })
    ));
    blocker.rollback().await?;
    drop(released);
    let reacquired = fixture.store(1).await?;
    assert_eq!(reacquired.load().await?, before);
    Ok(())
}

#[tokio::test]
async fn abandoned_append_settles_once_and_requires_observed_reconciliation()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new().await?;
    let store = fixture.store(1).await?;
    let transaction = fixture.admin.begin_with("BEGIN IMMEDIATE").await?;
    let record = input("abandoned acknowledgement", 1);
    let pending = instructions();
    let mut append = store.append(&record, Some(&pending));
    assert!(matches!(futures_util::poll!(&mut append), Poll::Pending));
    drop(append);
    transaction.rollback().await?;
    assert!(fixture.store(2).await?.load().await?.records.is_empty());
    assert!(
        matches!(store.append(&input("must not slip past uncertainty", 2), None).await, Err(StoreError::Backend { message }) if message.contains("unknown append outcome"))
    );
    let mut abandoned_load = store.load();
    assert!(matches!(
        futures_util::poll!(&mut abandoned_load),
        Poll::Pending
    ));
    drop(abandoned_load);
    assert!(
        matches!(store.append(&input("still unreconciled", 2), None).await, Err(StoreError::Backend { message }) if message.contains("unknown append outcome"))
    );
    let snapshot = store.load().await?;
    assert_eq!(snapshot.into_records(), vec![record]);
    store
        .append(&input("after reconciliation", 2), None)
        .await?;
    let snapshot = store.load().await?;
    assert_eq!(snapshot.records.len(), 2);
    assert_eq!(i64::from(snapshot.records[1].sequence), 2);
    Ok(())
}

#[tokio::test]
async fn initial_titles_use_the_first_root_input_and_bounded_graphemes()
-> Result<(), Box<dyn Error>> {
    for (text, expected) in [
        ("  fix\nparser\t\u{1}🔎  ".into(), "fix parser 🔎".into()),
        ("e\u{301}".repeat(80), "e\u{301}".repeat(80)),
        ("e\u{301}".repeat(81), format!("{}…", "e\u{301}".repeat(79))),
        ("👩‍💻".repeat(80), format!("{}…", "👩‍💻".repeat(46))),
        (format!("a{}", "\u{301}".repeat(3000)), "…".into()),
        (" \n\u{1}".into(), "Untitled session".into()),
    ] {
        let mut fixture = Fixture::new().await?;
        fixture
            .store(2)
            .await?
            .append(&input("child must not name the session", 1), None)
            .await?;
        let untitled: Option<String> =
            sqlx::query_scalar("SELECT title FROM sessions WHERE id = 1")
                .fetch_one(&mut fixture.admin)
                .await?;
        assert!(untitled.is_none());
        let root = fixture.store(1).await?;
        root.append(&input(&text, 1), None).await?;
        sqlx::query("UPDATE sessions SET updated_at_ms = ? WHERE id = 1")
            .bind(i64::MAX)
            .execute(&mut fixture.admin)
            .await?;
        root.append(&input("must not rename", 2), None).await?;
        let metadata: (String, i64, i64) = sqlx::query_as(
            "SELECT title, updated_at_ms, last_opened_at_ms FROM sessions WHERE id = 1",
        )
        .fetch_one(&mut fixture.admin)
        .await?;
        assert_eq!(metadata, (expected, i64::MAX, 0));
    }
    let mut fixture = Fixture::new().await?;
    let record = Record {
        invocation: InvocationId(1),
        kind: RecordKind::InputAccepted {
            input: UserInput::WithImages {
                text: String::new(),
                images: vec![image()?],
            },
        },
    };
    fixture.store(1).await?.append(&record, None).await?;
    let title: String = sqlx::query_scalar("SELECT title FROM sessions WHERE id = 1")
        .fetch_one(&mut fixture.admin)
        .await?;
    assert_eq!(title, "Image prompt");
    Ok(())
}

#[tokio::test]
async fn existing_open_never_initializes_missing_or_empty_storage() -> Result<(), Box<dyn Error>> {
    let directory = private_directory()?;
    for path in [
        directory.path().join("sessions.sqlite"),
        directory.path().join("missing/sessions.sqlite"),
    ] {
        assert!(SessionDatabase::open_existing(&path).await?.is_none());
        assert!(!path.exists());
    }
    assert_eq!(std::fs::read_dir(directory.path())?.count(), 0);

    let empty = tempfile::NamedTempFile::new_in(directory.path())?;
    assert!(matches!(
        SessionDatabase::open_existing(empty.path()).await,
        Err(PersistenceError::InvalidData("uninitialized database"))
    ));
    assert!(std::fs::read(empty.path())?.is_empty());

    let fixture = Fixture::new().await?;
    let existing = SessionDatabase::open_existing(&fixture.path)
        .await?
        .ok_or("initialized database was treated as absent")?;
    assert_eq!(
        existing.list_sessions(None, 20, None).await?.sessions.len(),
        2
    );
    Ok(())
}

#[tokio::test]
async fn opening_checks_native_sqlite_migration_history_and_actual_schema()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new().await?;
    let version: (String, String) = sqlx::query_as("SELECT sqlite_version(), sqlite_source_id()")
        .fetch_one(&mut fixture.admin)
        .await?;
    assert_eq!(version.0, "3.51.3");
    assert_eq!(
        version.1,
        "2026-03-13 10:38:09 737ae4a34738ffa0c3ff7f9bb18df914dd1cad163f28fd6b6e114a344fe6d618"
    );
    let applied: Vec<(i64, bool)> =
        sqlx::query_as("SELECT version, success FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&mut fixture.admin)
            .await?;
    assert_eq!(applied, vec![(1, true), (2, true), (3, true), (4, true)]);
    let violation = sqlx::query("INSERT INTO records (session_id, agent_id, sequence, invocation, runtime_epoch, created_at_ms, payload_version, payload_json) VALUES (2, 1, 1, 1, 1, 0, 1, '{}')").execute(&mut fixture.admin).await;
    assert!(violation.is_err());
    for statement in [
        "UPDATE _sqlx_migrations SET checksum = x'00'",
        "UPDATE _sqlx_migrations SET success = 0",
        "UPDATE _sqlx_migrations SET version = 99 WHERE version = 2",
        "ALTER TABLE records ADD COLUMN untracked TEXT",
    ] {
        let mut changed = Fixture::new().await?;
        sqlx::query(statement).execute(&mut changed.admin).await?;
        assert!(
            matches!(
                SessionDatabase::open(&changed.path).await,
                Err(PersistenceError::InvalidData(_))
            ),
            "accepted {statement}"
        );
        assert!(
            matches!(
                SessionDatabase::open_existing(&changed.path).await,
                Err(PersistenceError::InvalidData(_))
            ),
            "existing open accepted {statement}"
        );
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions")
            .fetch_one(&mut changed.admin)
            .await?;
        assert_eq!(count, 2);
    }
    let directory = private_directory()?;
    let path = directory.path().join("unrelated.sqlite");
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    drop(options.open(&path)?);
    let mut unrelated =
        SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path)).await?;
    sqlx::query("CREATE TABLE sqlitex_user_data (value TEXT)")
        .execute(&mut unrelated)
        .await?;
    assert!(matches!(
        SessionDatabase::open(&path).await,
        Err(PersistenceError::InvalidData(
            "not a Cocoa session database; choose a new --data-dir"
        ))
    ));
    let identity: i64 = sqlx::query_scalar("PRAGMA application_id")
        .fetch_one(&mut unrelated)
        .await?;
    assert_eq!(identity, 0);
    unrelated.close().await?;
    Ok(())
}

#[tokio::test]
async fn real_version_one_upgrade_preserves_originals_receipts_settings_and_verified_backup()
-> Result<(), Box<dyn Error>> {
    use cocoa::persistence::StoredSessionSettings;
    use cocoa_core::store::InvocationOutcome;

    let directory = private_directory()?;
    let path = directory.path().join("upgrade.sqlite3");
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    drop(options.open(&path)?);
    let mut admin = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&path)
            .foreign_keys(true),
    )
    .await?;
    sqlx::migrate!("./migrations").run_to(1, &mut admin).await?;
    let reference = "sess_84af31e209c74863b0d521a76ef409bd";
    let settings = StoredSessionSettings::Bound {
        effective: Box::new(effective_settings()),
    };
    let settings_json = serde_json::to_string(&settings)?;
    sqlx::query("INSERT INTO sessions (id, public_id, working_directory, title, created_at_ms, updated_at_ms, last_opened_at_ms, runtime_epoch, settings_version, settings_json) VALUES (7, ?, ?, 'Original title', 11, 12, 13, 1, 1, ?)")
        .bind(reference).bind(directory.path().to_str().ok_or("non-UTF8 fixture path")?).bind(&settings_json)
        .execute(&mut admin).await?;
    sqlx::query("INSERT INTO agents (id, session_id, name, parent_agent_id, format_version, lifecycle) VALUES (9, 7, 'root', NULL, 1, 'participating')")
        .execute(&mut admin).await?;
    let outcome = OperationOutcome::Completed {
        value: json!({"record_id":"23"}),
    };
    let originals = [
        Record {
            invocation: InvocationId(1),
            kind: RecordKind::InputAccepted {
                input: UserInput::WithImages {
                    text: "Original évidence".into(),
                    images: vec![image()?],
                },
            },
        },
        operation(
            1,
            1,
            "cocoa.context.set",
            json!({"key":"result","value":"original"}),
        ),
        Record {
            invocation: InvocationId(1),
            kind: RecordKind::OperationFinished {
                operation: OperationId(1),
                outcome: outcome.clone(),
            },
        },
        Record {
            invocation: InvocationId(1),
            kind: RecordKind::InvocationFinished {
                outcome: InvocationOutcome::Completed,
            },
        },
    ];
    let mut payloads = Vec::new();
    for (index, original) in originals.iter().enumerate() {
        let encoded = serde_json::to_string(&original.kind)?;
        sqlx::query("INSERT INTO records (id, session_id, agent_id, sequence, invocation, runtime_epoch, created_at_ms, payload_version, payload_json) VALUES (?, 7, 9, ?, 1, 1, 17, 2, ?)")
            .bind(11 + index as i64).bind(index as i64 + 1).bind(&encoded).execute(&mut admin).await?;
        payloads.push(encoded);
    }
    sqlx::query("INSERT INTO mutation_receipts VALUES (12, 1, ?)")
        .bind(serde_json::to_string(&outcome)?)
        .execute(&mut admin)
        .await?;
    admin.close().await?;
    assert!(matches!(
        SessionDatabase::open_existing(&path).await,
        Err(PersistenceError::MigrationRequired)
    ));
    let database = SessionDatabase::open(&path).await?;
    let session = database.attach_session(reference, limits()).await?;
    let store = session.root_store(limits())?;
    assert_eq!(store.load().await?.into_records(), originals);
    assert_eq!(session.settings().await?, settings);
    let export = session
        .export_run(cocoa::persistence::ExportLimits::default())
        .await?;
    assert!(!export.ledger_complete);
    assert!(export.accounting.usage.is_none());
    assert!(export.accounting.reported_usage.is_none());
    assert_eq!(
        store.operation_receipt(RecordId::try_from(12)?).await?,
        Some(outcome)
    );
    let mut admin =
        SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path)).await?;
    let migrated: Vec<(i64, i64, i64, String)> = sqlx::query_as(
        "SELECT id, sequence, created_at_ms, payload_json FROM records ORDER BY sequence",
    )
    .fetch_all(&mut admin)
    .await?;
    for (index, (id, sequence, created, encoded)) in migrated.into_iter().enumerate() {
        assert_eq!(
            (id, sequence, created),
            (11 + index as i64, 1 + index as i64, 17)
        );
        assert_eq!(encoded, payloads[index]);
    }
    let versions: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&mut admin)
            .await?;
    assert_eq!(versions, [1, 2, 3, 4]);
    assert!(
        sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(&mut admin)
            .await?
            .is_empty()
    );
    admin.close().await?;
    let backups = std::fs::read_dir(directory.path().join("upgrade.sqlite3.backups"))?
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(backups.len(), 1);
    assert!(
        backups[0]
            .file_name()
            .to_string_lossy()
            .starts_with("verified-")
    );
    let mut backup = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(backups[0].path().join("database.sqlite3"))
            .read_only(true),
    )
    .await?;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT version FROM _sqlx_migrations")
            .fetch_one(&mut backup)
            .await?,
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT payload_json FROM records ORDER BY sequence")
            .fetch_all(&mut backup)
            .await?,
        payloads
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT settings_json FROM sessions")
            .fetch_one(&mut backup)
            .await?,
        settings_json
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT operation_record_id FROM mutation_receipts")
            .fetch_one(&mut backup)
            .await?,
        12
    );
    assert!(
        sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(&mut backup)
            .await?
            .is_empty()
    );
    backup.close().await?;
    Ok(())
}

#[tokio::test]
async fn cocoa_storage_rejects_foreign_application_ids() -> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new().await?;
    let identity: i64 = sqlx::query_scalar("PRAGMA application_id")
        .fetch_one(&mut fixture.admin)
        .await?;
    assert_eq!(identity, 0x434f434f);
    fixture
        .store(1)
        .await?
        .append(&input("foreign sentinel", 1), None)
        .await?;
    sqlx::query("PRAGMA application_id = 0x54455354")
        .execute(&mut fixture.admin)
        .await?;
    for result in [
        SessionDatabase::open(&fixture.path).await,
        SessionDatabase::open_existing(&fixture.path)
            .await
            .and_then(|database| database.ok_or(PersistenceError::SessionNotFound)),
    ] {
        assert!(matches!(result, Err(PersistenceError::InvalidData(_))));
    }
    let identity: i64 = sqlx::query_scalar("PRAGMA application_id")
        .fetch_one(&mut fixture.admin)
        .await?;
    assert_eq!(identity, 0x54455354);

    let data_directory = fixture._directory.path().join("foreign-data");
    std::fs::create_dir(&data_directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&data_directory, std::fs::Permissions::from_mode(0o700))?;
    }
    let foreign_path = data_directory.join("sessions.sqlite3");
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(&mut fixture.admin)
        .await?;
    std::fs::copy(&fixture.path, &foreign_path)?;
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_cocoa"))
        .env_clear()
        .env("HOME", fixture._directory.path())
        .arg("--data-dir")
        .arg(&data_directory)
        .args(["sessions", "list"])
        .stdin(std::process::Stdio::null())
        .output_bounded()?;
    assert!(!output.status.success());
    let diagnostic = String::from_utf8(output.stderr)?;
    assert!(
        diagnostic.contains("not a Cocoa session database"),
        "{diagnostic}"
    );
    assert!(diagnostic.contains("--data-dir"), "{diagnostic}");
    Ok(())
}

#[tokio::test]
async fn first_input_binds_exact_settings_atomically_and_empty_sessions_resume()
-> Result<(), Box<dyn Error>> {
    use cocoa::persistence::StoredSessionSettings;
    let mut fixture = Fixture::empty().await?;
    let session = fixture
        .database
        .create_session(fixture._directory.path())
        .await?;
    assert_eq!(
        session.load(limits()).await?.settings,
        StoredSessionSettings::Unconfigured
    );
    let store = session.root_store_with_settings(limits(), effective_settings())?;
    sqlx::raw_sql("CREATE TRIGGER reject_settings BEFORE UPDATE ON sessions WHEN NEW.title IS NOT NULL BEGIN SELECT RAISE(ABORT, 'failed acceptance'); END;")
        .execute(&mut fixture.admin).await?;
    assert!(store.append(&input("rejected", 1), None).await.is_err());
    assert_eq!(
        session.load(limits()).await?.settings,
        StoredSessionSettings::Unconfigured
    );
    assert!(store.load().await?.records.is_empty());
    sqlx::query("DROP TRIGGER reject_settings")
        .execute(&mut fixture.admin)
        .await?;
    store.append(&input("accepted", 1), None).await?;
    assert_eq!(
        session.settings().await?,
        StoredSessionSettings::Bound {
            effective: Box::new(effective_settings())
        }
    );
    let mut changed = effective_settings();
    changed
        .aliases
        .get_mut("main")
        .ok_or("missing alias")?
        .model = "other-child-model".into();
    let rebound = session.root_store_with_settings(limits(), changed.clone())?;
    rebound
        .append(&input("before model output", 2), None)
        .await?;
    assert_eq!(
        session.settings().await?,
        StoredSessionSettings::Bound {
            effective: Box::new(changed)
        }
    );
    let completed = Record {
        invocation: InvocationId(2),
        ..model()
    };
    let state = InstructionState {
        invocation: InvocationId(2),
        scoped_sources: Vec::new(),
        selected_skills: Vec::new(),
        scoped_feedback: Vec::new(),
        skill_feedback: Vec::new(),
    };
    rebound.append(&completed, Some(&state)).await?;
    assert!(
        store
            .append(&input("after model output", 3), None)
            .await
            .is_err()
    );
    assert_eq!(
        store.load().await?.into_records(),
        vec![
            input("accepted", 1),
            input("before model output", 2),
            completed,
        ]
    );
    let StoredSessionSettings::Bound { mut effective } = session.settings().await? else {
        return Err("missing bound settings".into());
    };
    let alias = effective.aliases.get_mut("main").ok_or("missing alias")?;
    alias.context_tokens = Some(500_000);
    alias.output_tokens = Some(64_000);
    let resolved = session.root_store_with_settings(limits(), *effective.clone())?;
    resolved
        .append(&input("resolved model limits", 3), None)
        .await?;
    assert_eq!(
        session.settings().await?,
        StoredSessionSettings::Bound {
            effective: effective.clone()
        }
    );
    assert!(
        rebound
            .append(&input("cannot forget limits", 4), None)
            .await
            .is_err()
    );
    effective
        .aliases
        .get_mut("main")
        .ok_or("missing alias")?
        .context_tokens = Some(800_000);
    let changed = session.root_store_with_settings(limits(), *effective)?;
    assert!(
        changed
            .append(&input("cannot replace known limits", 4), None)
            .await
            .is_err()
    );
    let empty = fixture
        .database
        .create_session(fixture._directory.path())
        .await?;
    let resumed = empty.recover(limits()).await?;
    assert!(resumed.is_resumed());
    assert_eq!(
        resumed.load(limits()).await?.settings,
        StoredSessionSettings::Unconfigured
    );
    assert!(
        resumed
            .root_store(limits())?
            .load()
            .await?
            .records
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn invalid_stored_membership_is_rejected_without_rewriting() -> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::empty().await?;
    let session = fixture
        .database
        .create_session(fixture._directory.path())
        .await?;
    let root = i64::from(session.root_agent());
    let child = root + 1;
    let assignment = r#"{"prompt":"exact task","alias":"main"}"#;
    sqlx::query("INSERT INTO agents (id, session_id, name, parent_agent_id, format_version, assignment_json, lifecycle) VALUES (?, ?, 'child', ?, 1, ?, 'participating')")
        .bind(child).bind(i64::from(session.id())).bind(root).bind(assignment)
        .execute(&mut fixture.admin).await?;
    let before = session.coordination_snapshot().await?;
    assert_eq!(before.agents[0].membership, AgentMembership::Root);
    assert!(matches!(
        &before.agents[1].membership,
        AgentMembership::Child { parent, assignment }
            if *parent == session.root_agent()
                && assignment.prompt == "exact task" && assignment.alias == "main"
    ));
    sqlx::query("PRAGMA ignore_check_constraints = ON")
        .execute(&mut fixture.admin)
        .await?;
    for (agent, invalid, detail) in [
        (root, Some(assignment), "agent assignment membership"),
        (child, None, "agent assignment membership"),
        (
            child,
            Some(r#"{"prompt":"","alias":"main"}"#),
            "agent assignment membership",
        ),
        (
            child,
            Some(r#"{"prompt":"exact task","alias":""}"#),
            "agent assignment membership",
        ),
        (
            child,
            Some(r#"{"prompt":"exact task"}"#),
            "agent assignment",
        ),
    ] {
        sqlx::query("UPDATE agents SET assignment_json = ? WHERE id = ?")
            .bind(invalid)
            .bind(agent)
            .execute(&mut fixture.admin)
            .await?;
        assert!(matches!(
            session.coordination_snapshot().await,
            Err(PersistenceError::InvalidData(message)) if message == detail
        ));
        assert!(matches!(
            session.load(limits()).await,
            Err(PersistenceError::InvalidData(message)) if message == detail
        ));
        let retained: Option<String> =
            sqlx::query_scalar("SELECT assignment_json FROM agents WHERE id = ?")
                .bind(agent)
                .fetch_one(&mut fixture.admin)
                .await?;
        assert_eq!(retained.as_deref(), invalid);
        sqlx::query("UPDATE agents SET assignment_json = ? WHERE id = ?")
            .bind(if agent == root {
                None
            } else {
                Some(assignment)
            })
            .bind(agent)
            .execute(&mut fixture.admin)
            .await?;
        assert_eq!(session.coordination_snapshot().await?, before);
    }
    Ok(())
}

#[tokio::test]
async fn child_assignment_encoded_limit_preserves_readable_membership() -> Result<(), Box<dyn Error>>
{
    use cocoa::persistence::AgentAssignment;

    let fixture = Fixture::empty().await?;
    let session = fixture
        .database
        .create_session(fixture._directory.path())
        .await?;
    let store = session.root_store_with_settings(
        StoreLimits {
            record_bytes: 16 * 1024 * 1024,
        },
        effective_settings(),
    )?;
    store.append(&input("configured", 1), None).await?;
    let mut assignment = AgentAssignment {
        prompt: format!("{}xxx", "\u{1}".repeat(1_398_267)),
        alias: "main".into(),
    };
    assert_eq!(serde_json::to_vec(&assignment)?.len(), 8_389_633);
    let rejected_start = store
        .append(
            &operation(
                1,
                1,
                "cocoa.spawn",
                json!({"name":"researcher","prompt":assignment.prompt,"alias":"main"}),
            ),
            None,
        )
        .await?;
    let rejected = store
        .admit_agent(rejected_start, "researcher", &assignment, 65536)
        .await;
    assert!(
        matches!(
            rejected,
            Err(PersistenceError::Store(StoreError::RecordLimit))
        ),
        "oversized encoded assignment was not rejected: {rejected:?}"
    );
    assert_eq!(store.operation_receipt(rejected_start).await?, None);
    assert_eq!(session.coordination_snapshot().await?.agents.len(), 1);

    assignment.prompt.pop();
    assert_eq!(serde_json::to_vec(&assignment)?.len(), 8_389_632);
    let accepted_start = store
        .append(
            &operation(
                1,
                2,
                "cocoa.spawn",
                json!({"name":"researcher","prompt":assignment.prompt,"alias":"main"}),
            ),
            None,
        )
        .await?;
    let admitted = store
        .admit_agent(accepted_start, "researcher", &assignment, 65536)
        .await?;
    assert!(admitted.created);
    let snapshot = session.coordination_snapshot().await?;
    assert_eq!(snapshot.agents.len(), 2);
    assert_eq!(snapshot.agents[1].id, admitted.agent);
    assert_eq!(snapshot.agents[0].membership, AgentMembership::Root);
    assert!(matches!(
        &snapshot.agents[1].membership,
        AgentMembership::Child { parent, assignment: retained }
            if *parent == session.root_agent() && retained == &assignment
    ));
    assert_eq!(
        store.operation_receipt(accepted_start).await?,
        Some(admitted.outcome)
    );
    Ok(())
}

#[tokio::test]
async fn duplicate_names_exact_large_ids_and_child_owned_context_survive_recovery()
-> Result<(), Box<dyn Error>> {
    use cocoa::persistence::{AgentAssignment, AgentOutcome, PersistenceError};
    use cocoa_core::context::{ContextContent, ContextType};
    let mut fixture = Fixture::empty().await?;
    let session = fixture
        .database
        .create_session(fixture._directory.path())
        .await?;
    let public = session.public_id().to_string();
    let large = 9_007_199_254_741_111_i64;
    sqlx::query("UPDATE agents SET id=? WHERE id=?")
        .bind(large)
        .bind(i64::from(session.root_agent()))
        .execute(&mut fixture.admin)
        .await?;
    drop(session);
    let session = fixture.database.attach_session(&public, limits()).await?;
    assert_eq!(i64::from(session.root_agent()), large);
    let root = session.root_store_with_settings(limits(), effective_settings())?;
    root.append(&input("configured", 1), None).await?;
    let assignment = AgentAssignment {
        prompt: "same task".into(),
        alias: "main".into(),
    };
    let arguments = json!({"name":"researcher","prompt":"same task","alias":"main"});
    let first_start = root
        .append(&operation(1, 1, "cocoa.spawn", arguments.clone()), None)
        .await?;
    assert!(
        root.admit_agent(first_start, "researcher", &assignment, 1)
            .await
            .is_err()
    );
    assert_eq!(session.coordination_snapshot().await?.agents.len(), 1);
    let first = root
        .admit_agent(first_start, "researcher", &assignment, 65536)
        .await?;
    assert!(first.created);
    assert_eq!(i64::from(first.agent), large + 1);
    let repeated = root
        .admit_agent(first_start, "researcher", &assignment, 65536)
        .await?;
    assert!(!repeated.created);
    assert_eq!(first.agent, repeated.agent);
    assert_eq!(first.outcome, repeated.outcome);
    let second_start = root
        .append(&operation(1, 2, "cocoa.spawn", arguments), None)
        .await?;
    let second = root
        .admit_agent(second_start, "researcher", &assignment, 65536)
        .await?;
    assert_ne!(first.agent, second.agent);
    let content = ContextContent {
        kind: ContextType::Observation,
        value: json!({"publication":"kept"}),
        labels: vec!["result".into()],
    };
    let arguments = json!({"key":"child-result","content":content});
    let child = session.agent_store(first.agent, limits()).await?;
    child.append(&input("child input", 1), None).await?;
    let start = child
        .append(&operation(1, 1, "context.set", arguments.clone()), None)
        .await?;
    let accepted = child
        .set_context(start, "child-result", &content, 65536)
        .await?;
    assert_eq!(
        child.operation_receipt(start).await?,
        Some(accepted.clone())
    );
    let other = session.agent_store(second.agent, limits()).await?;
    other.append(&input("other child input", 1), None).await?;
    let other_start = other
        .append(&operation(1, 1, "context.set", arguments.clone()), None)
        .await?;
    assert!(matches!(
        other
            .set_context(other_start, "child-result", &content, 65536)
            .await,
        Err(PersistenceError::ContextNotOwner)
    ));
    let root_start = root
        .append(&operation(1, 3, "context.set", arguments), None)
        .await?;
    assert!(matches!(
        root.set_context(root_start, "child-result", &content, 65536)
            .await,
        Err(PersistenceError::ContextNotOwner)
    ));
    let outcome = AgentOutcome::Completed {
        value: json!({"status":"completed","answer":"exact child result"}),
    };
    session.complete_agent(first.agent, &outcome).await?;
    let before = session.coordination_snapshot().await?;
    let accepted_value = serde_json::to_value(&before.context[0])?;
    assert_eq!(accepted_value["owner"], json!((large + 1).to_string()));
    assert!(accepted_value.get("id").is_none());
    let resumed = session.recover(limits()).await?;
    let after = resumed.load(limits()).await?;
    assert_eq!(after.context, before.context);
    for (before, after) in before.agents.iter().zip(&after.agents) {
        assert_eq!(after.membership, before.membership);
        assert_eq!(
            serde_json::from_value::<RetainedAgent>(serde_json::to_value(after)?)?,
            *after
        );
    }
    assert_eq!(after.agents[1].outcome, Some(outcome));
    assert!(
        matches!(&after.agents[2].outcome,Some(AgentOutcome::Failed {code,effects:cocoa_core::capability::Effects::Unknown,..}) if code=="host_restarted")
    );
    assert_eq!(after.histories[&first.agent].records.len(), 4);
    assert_eq!(after.histories[&second.agent].records.len(), 3);
    let root = resumed.root_store(limits())?;
    assert!(matches!(
        root.set_context(root_start, "child-result", &content, 65536)
            .await,
        Err(PersistenceError::ContextNotOwner)
    ));
    Ok(())
}

#[tokio::test]
async fn lost_local_acknowledgements_recover_exact_receipts_and_unknown_external_effects()
-> Result<(), Box<dyn Error>> {
    use cocoa_core::context::{ContextContent, ContextType};
    use cocoa_core::store::InvocationOutcome;
    let mut fixture = Fixture::empty().await?;
    let session = fixture
        .database
        .create_session(fixture._directory.path())
        .await?;
    let store = session.root_store_with_settings(limits(), effective_settings())?;
    store
        .append(&input("accepted before crash", 1), None)
        .await?;
    let model = Record {
        invocation: InvocationId(1),
        kind: RecordKind::ModelCompleted {
            response: ModelResponse {
                action: ModelAction::Execute {
                    cells: vec![
                        Cell {
                            code: "same source".into(),
                        },
                        Cell {
                            code: "same source".into(),
                        },
                    ],
                },
                continuation: None,
                usage: None,
            },
        },
    };
    let state = InstructionState {
        invocation: InvocationId(1),
        scoped_sources: Vec::new(),
        selected_skills: Vec::new(),
        scoped_feedback: Vec::new(),
        skill_feedback: Vec::new(),
    };
    let model_id = store.append(&model, Some(&state)).await?;
    store
        .append(
            &Record {
                invocation: InvocationId(1),
                kind: RecordKind::CellStarted {
                    model: model_id,
                    cell: 0,
                },
            },
            None,
        )
        .await?;
    let content = ContextContent {
        kind: ContextType::Decision,
        value: json!("accepted exact"),
        labels: Vec::new(),
    };
    let start = store
        .append(
            &operation(
                1,
                1,
                "context.set",
                json!({"key":"decision","content":content}),
            ),
            None,
        )
        .await?;
    let blocker = fixture.admin.begin_with("BEGIN IMMEDIATE").await?;
    let mut acceptance = Box::pin(store.set_context(start, "decision", &content, 65536));
    assert!(matches!(
        futures_util::poll!(&mut acceptance),
        Poll::Pending
    ));
    drop(acceptance);
    blocker.rollback().await?;
    assert!(store.append(&input("fenced", 2), None).await.is_err());
    let loaded = session.load(limits()).await?;
    assert_eq!(loaded.context[0].content, content);
    let expected = store
        .operation_receipt(start)
        .await?
        .ok_or("missing local receipt")?;
    let external = store
        .append(
            &operation(1, 2, "external.unknown", json!({"path":"marker"})),
            None,
        )
        .await?;
    let mut pending = state.clone();
    pending.selected_skills = instructions().selected_skills;
    let instruction = store
        .append(&operation(1, 3, "load_skill", json!({"id":"review"})), None)
        .await?;
    let outcome = OperationOutcome::Completed {
        value: json!({"id":"review","pending":true}),
    };
    store
        .accept_instruction_change(instruction, &pending, &outcome)
        .await?;
    assert_eq!(
        store.operation_receipt(instruction).await?,
        Some(outcome.clone())
    );
    sqlx::raw_sql("CREATE TRIGGER reject_recovery BEFORE INSERT ON records WHEN json_type(NEW.payload_json,'$.InvocationFinished') IS NOT NULL BEGIN SELECT RAISE(ABORT,'interrupt recovery'); END;").execute(&mut fixture.admin).await?;
    assert!(session.recover(limits()).await.is_err());
    let unchanged = store.load().await?;
    assert_eq!(unchanged.records.len(), 6);
    assert_eq!(unchanged.instruction_state, Some(pending.clone()));
    sqlx::query("DROP TRIGGER reject_recovery")
        .execute(&mut fixture.admin)
        .await?;
    let resumed = session.recover(limits()).await?;
    let history = resumed.root_store(limits())?.load().await?;
    assert_eq!(history.instruction_state, Some(pending));
    assert!(history.records.iter().any(|record|matches!(&record.record.kind,RecordKind::OperationFinished {operation:OperationId(1),outcome} if outcome==&expected)));
    assert!(history.records.iter().any(|record|matches!(&record.record.kind,RecordKind::OperationFinished {operation:OperationId(3),outcome:retained} if retained==&outcome)));
    assert!(history.records.iter().any(|record| record.id == external));
    assert!(!history.records.iter().any(|record| matches!(
        &record.record.kind,
        RecordKind::OperationFinished {
            operation: OperationId(2),
            ..
        }
    )));
    assert!(history.records.iter().any(|record|matches!(&record.record.kind,RecordKind::CellCompleted {model,cell:0,outcome:CellResponse::Interrupted {..},..} if *model==model_id)));
    assert!(history.records.iter().any(|record|matches!(&record.record.kind,RecordKind::CellCompleted {model,cell:1,outcome:CellResponse::NotStarted,..} if *model==model_id)));
    assert!(matches!(
        history.records.last().map(|record| &record.record.kind),
        Some(RecordKind::InvocationFinished {
            outcome: InvocationOutcome::Interrupted
        })
    ));
    let repeated = resumed.recover(limits()).await?;
    assert_eq!(repeated.root_store(limits())?.load().await?, history);
    Ok(())
}

#[tokio::test]
async fn empty_search_enforces_its_final_encoded_response_limit() -> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::empty().await?;
    for _ in 0..15 {
        fixture
            .database
            .create_session(fixture._directory.path())
            .await?;
    }
    sqlx::query("UPDATE sessions SET working_directory=?, title=?, created_at_ms=0, updated_at_ms=0, last_opened_at_ms=0")
        .bind(format!("/{}", "d".repeat(16_383)))
        .bind("t".repeat(512))
        .execute(&mut fixture.admin)
        .await?;
    let page = fixture.database.list_sessions(None, 50, None).await?;
    assert_eq!(page.sessions.len(), 15);
    assert!(serde_json::to_vec(&page)?.len() <= 256 * 1024);

    let rejected = fixture.database.search_sessions(None, " \n ", 50).await;
    let returned_bytes = rejected
        .as_ref()
        .ok()
        .map(serde_json::to_vec)
        .transpose()?
        .map(|encoded| encoded.len());
    assert!(
        matches!(rejected, Err(PersistenceError::Store(StoreError::Capacity))),
        "oversized search response returned {returned_bytes:?} bytes"
    );
    let result = fixture.database.search_sessions(None, "", 14).await?;
    assert_eq!(result.sessions.len(), 14);
    assert!(serde_json::to_vec(&result)?.len() <= 256 * 1024);
    assert!(
        result
            .sessions
            .iter()
            .all(|matched| matched.snippet == "t".repeat(512))
    );
    Ok(())
}

#[tokio::test]
async fn literal_search_checks_complete_root_messages_controls_and_order()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::empty().await?;
    let mut sessions = Vec::new();
    for index in 0..55 {
        let session = fixture
            .database
            .create_session(fixture._directory.path())
            .await?;
        let store = session.root_store(limits())?;
        store
            .append(&input(&format!("title {index}"), 1), None)
            .await?;
        store
            .append(&input("İstanbul CAFÉ path_a% OR \"literal\"", 2), None)
            .await?;
        store.append(&input("separate", 3), None).await?;
        store.append(&input("messages", 4), None).await?;
        store
            .append(
                &Record {
                    invocation: InvocationId(4),
                    kind: RecordKind::OperationStarted {
                        operation: OperationId(1),
                        name: "tool.hidden".into(),
                        arguments: json!({"secret":"tool-only"}),
                    },
                },
                None,
            )
            .await?;
        sqlx::query("UPDATE sessions SET updated_at_ms=? WHERE id=?")
            .bind(index)
            .bind(i64::from(session.id()))
            .execute(&mut fixture.admin)
            .await?;
        sessions.push((session.id(), session.root_agent()));
    }
    let expected: Vec<_> = sessions
        .iter()
        .rev()
        .take(50)
        .map(|session| session.0)
        .collect();
    let result = fixture
        .database
        .search_sessions(None, "  PATH_A% or \"literal\"  ", 50)
        .await?;
    assert_eq!(
        result
            .sessions
            .iter()
            .map(|item| item.session.id)
            .collect::<Vec<_>>(),
        expected
    );
    assert_eq!(
        fixture
            .database
            .search_sessions(None, "café", 5)
            .await?
            .sessions
            .len(),
        5
    );
    assert!(
        fixture
            .database
            .search_sessions(None, "cafe", 50)
            .await?
            .sessions
            .is_empty()
    );
    assert!(
        fixture
            .database
            .search_sessions(None, "separate messages", 50)
            .await?
            .sessions
            .is_empty()
    );
    assert!(
        fixture
            .database
            .search_sessions(None, "tool-only", 50)
            .await?
            .sessions
            .is_empty()
    );
    assert!(
        fixture
            .database
            .search_sessions(None, "pathXa", 50)
            .await?
            .sessions
            .is_empty()
    );
    let unicode = fixture
        .database
        .search_sessions(None, "i\u{307}stanbul", 1)
        .await?;
    assert!(unicode.sessions[0].snippet.starts_with("İstanbul"));
    assert_eq!(
        fixture
            .database
            .search_sessions(None, " \n ", 2)
            .await?
            .sessions
            .len(),
        2
    );
    assert!(
        fixture
            .database
            .search_sessions(None, &"x".repeat(513), 2)
            .await
            .is_err()
    );
    let last = sessions.last().ok_or("missing session")?;
    let store = fixture
        .database
        .agent_store(
            last.1,
            StoreLimits {
                record_bytes: 1024 * 1024,
            },
        )
        .await?;
    store
        .append(
            &input(&format!("{}tail needle", "x".repeat(256 * 1024)), 5),
            None,
        )
        .await?;
    let long = fixture
        .database
        .search_sessions(None, "tail needle", 50)
        .await?;
    assert_eq!(long.sessions.len(), 1);
    assert_eq!(long.sessions[0].session.id, last.0);
    assert!(long.sessions[0].snippet.contains("tail needle"));
    assert!(long.sessions[0].snippet.len() <= 2048);
    let large = fixture
        .database
        .agent_store(
            last.1,
            StoreLimits {
                record_bytes: 32 * 1024 * 1024,
            },
        )
        .await?;
    large
        .append(
            &Record {
                invocation: InvocationId(5),
                kind: RecordKind::ModelCompleted {
                    response: ModelResponse {
                        action: ModelAction::Final {
                            answer: "visible large continuation answer".into(),
                        },
                        continuation: Some(ProviderData {
                            provider: "fixture".into(),
                            data: json!(format!(
                                "provider-private-only{}",
                                "x".repeat(17 * 1024 * 1024)
                            )),
                        }),
                        usage: None,
                    },
                },
            },
            None,
        )
        .await?;
    let found = fixture
        .database
        .search_sessions(None, "large continuation answer", 50)
        .await?;
    assert_eq!(found.sessions.len(), 1);
    assert_eq!(found.sessions[0].session.id, last.0);
    assert_eq!(
        found.sessions[0].source,
        cocoa::persistence::SearchSource::Assistant
    );
    assert!(
        fixture
            .database
            .search_sessions(None, "provider-private-only", 50)
            .await?
            .sessions
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn record_and_context_identity_exhaustion_never_allocates_random_ids()
-> Result<(), Box<dyn Error>> {
    use cocoa_core::context::{ContextContent, ContextType};
    let mut fixture = Fixture::new().await?;
    let store = fixture.store(1).await?;
    store.append(&input("first", 1), None).await?;
    sqlx::query("UPDATE records SET id=?")
        .bind(i64::MAX - 1)
        .execute(&mut fixture.admin)
        .await?;
    assert_eq!(
        i64::from(store.append(&input("last", 2), None).await?),
        i64::MAX
    );
    let before = store.load().await?;
    assert_eq!(
        store.append(&input("exhausted", 3), None).await,
        Err(StoreError::IdentityExhausted)
    );
    assert_eq!(store.load().await?, before);

    let mut fixture = Fixture::empty().await?;
    let session = fixture
        .database
        .create_session(fixture._directory.path())
        .await?;
    let store = session.root_store_with_settings(limits(), effective_settings())?;
    store.append(&input("configured", 1), None).await?;
    let content = ContextContent {
        kind: ContextType::Observation,
        value: json!("retained"),
        labels: vec!["line\n\0label".into()],
    };
    for (index, key) in ["\0first\nkey", "second", "exhausted"]
        .into_iter()
        .enumerate()
    {
        let start = store
            .append(
                &operation(
                    1,
                    u64::try_from(index)? + 1,
                    "context.set",
                    json!({"key":key,"content":content}),
                ),
                None,
            )
            .await?;
        let result = store.set_context(start, key, &content, 65536).await;
        if index == 2 {
            assert!(matches!(
                result,
                Err(PersistenceError::Store(StoreError::IdentityExhausted))
            ));
            assert_eq!(store.operation_receipt(start).await?, None);
        } else {
            result?;
            if index == 0 {
                sqlx::query("UPDATE context_records SET id=?")
                    .bind(i64::MAX - 1)
                    .execute(&mut fixture.admin)
                    .await?;
            }
        }
    }
    let records = session.coordination_snapshot().await?.context;
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].key, "\0first\nkey");
    assert_eq!(records[0].content, content);
    assert_eq!(i64::from(records[0].id), i64::MAX - 1);
    assert_eq!(i64::from(records[1].id), i64::MAX);
    let replacement = ContextContent {
        value: json!("replacement"),
        ..content
    };
    let start = store
        .append(
            &operation(
                1,
                4,
                "context.set",
                json!({"key":"second","content":replacement}),
            ),
            None,
        )
        .await?;
    sqlx::raw_sql("CREATE TRIGGER reject_receipt BEFORE INSERT ON mutation_receipts BEGIN SELECT RAISE(ABORT,'receipt rejected'); END;")
        .execute(&mut fixture.admin).await?;
    assert!(
        store
            .set_context(start, "second", &replacement, 65536)
            .await
            .is_err()
    );
    assert_eq!(store.operation_receipt(start).await?, None);
    assert_eq!(session.coordination_snapshot().await?.context, records);
    sqlx::query("DROP TRIGGER reject_receipt")
        .execute(&mut fixture.admin)
        .await?;
    let outcome = store
        .set_context(start, "second", &replacement, 65536)
        .await?;
    assert_eq!(store.operation_receipt(start).await?, Some(outcome));
    let replaced = session.coordination_snapshot().await?.context;
    assert_eq!(replaced[1].id, records[1].id);
    assert_eq!(replaced[1].content, replacement);
    Ok(())
}

#[tokio::test]
async fn malformed_root_or_child_history_rejects_recovery_without_mutation()
-> Result<(), Box<dyn Error>> {
    use cocoa::persistence::AgentAssignment;
    use cocoa_core::store::InvocationOutcome;
    for child in [false, true] {
        let fixture = Fixture::empty().await?;
        let session = fixture
            .database
            .create_session(fixture._directory.path())
            .await?;
        let root = session.root_store_with_settings(limits(), effective_settings())?;
        root.append(&input("root input", 1), None).await?;
        let (target, agent) = if child {
            let start = root
                .append(
                    &operation(1, 1, "cocoa.spawn", json!({"name":"child","prompt":"work"})),
                    None,
                )
                .await?;
            let admission = root
                .admit_agent(
                    start,
                    "child",
                    &AgentAssignment {
                        prompt: "work".into(),
                        alias: "main".into(),
                    },
                    65536,
                )
                .await?;
            let child = session.agent_store(admission.agent, limits()).await?;
            child
                .append(&input("older unfinished invocation", 1), None)
                .await?;
            child
                .append(&input("illegal newer invocation", 2), None)
                .await?;
            (child, admission.agent)
        } else {
            root.append(
                &Record {
                    invocation: InvocationId(1),
                    kind: RecordKind::InvocationFinished {
                        outcome: InvocationOutcome::Interrupted,
                    },
                },
                None,
            )
            .await?;
            root.append(&operation(1, 1, "after.terminal", json!({})), None)
                .await?;
            (root, session.root_agent())
        };
        let before = session.load(limits()).await?;
        assert!(session.recover(limits()).await.is_err());
        assert_eq!(session.load(limits()).await?, before);
        assert_eq!(target.load().await?, before.histories[&agent]);
    }
    Ok(())
}

fn operation(invocation: u64, operation: u64, name: &str, arguments: serde_json::Value) -> Record {
    Record {
        invocation: InvocationId(invocation),
        kind: RecordKind::OperationStarted {
            operation: OperationId(operation),
            name: name.into(),
            arguments,
        },
    }
}

fn effective_settings() -> cocoa::persistence::EffectiveSessionSettings {
    use cocoa::persistence::{
        EffectiveAliasSettings, EffectiveSessionSettings, SessionExecutionLimits,
    };
    EffectiveSessionSettings {
        alias: "main".into(),
        provider: "fixture".into(),
        protocol: "fixture-v1".into(),
        model: "model".into(),
        endpoint: None,
        account: Some("account-id".into()),
        thinking_level: None,
        reasoning_summary: false,
        execution_version: 1,
        aliases: std::collections::BTreeMap::from([(
            "main".into(),
            EffectiveAliasSettings {
                provider: "fixture".into(),
                model: "model".into(),
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
            record_bytes: 64 * 1024,
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

struct Fixture {
    database: SessionDatabase,
    admin: SqliteConnection,
    path: PathBuf,
    _directory: tempfile::TempDir,
}

impl Fixture {
    async fn empty() -> Result<Self, Box<dyn Error>> {
        let directory = private_directory()?;
        let path = directory.path().join("sessions.sqlite");
        let database = SessionDatabase::open(&path).await?;
        let admin = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&path)
                .foreign_keys(true),
        )
        .await?;
        Ok(Self {
            database,
            admin,
            path,
            _directory: directory,
        })
    }

    async fn new() -> Result<Self, Box<dyn Error>> {
        let mut fixture = Self::empty().await?;
        let mut transaction = fixture.admin.begin().await?;
        for id in [1_i64, 2] {
            sqlx::query("INSERT INTO sessions (id, public_id, working_directory, created_at_ms, updated_at_ms, last_opened_at_ms, runtime_epoch, settings_version, settings_json) VALUES (?, ?, ?, 0, 0, 0, 1, 1, '{\"fixture\":true}')")
                .bind(id).bind(format!("sess_{id:032x}")).bind(fixture._directory.path().to_str().ok_or("fixture path is not UTF-8")?).execute(&mut *transaction).await?;
        }
        sqlx::raw_sql("INSERT INTO agents (id, session_id, name, parent_agent_id, format_version, assignment_json, lifecycle) VALUES
            (1, 1, 'root', NULL, 1, NULL, 'participating'),
            (2, 1, 'research', 1, 1, '{\"prompt\":\"one\"}', 'participating'),
            (3, 1, 'research', 1, 1, '{\"prompt\":\"two\"}', 'participating'),
            (4, 2, 'root', NULL, 1, NULL, 'participating');").execute(&mut *transaction).await?;
        transaction.commit().await?;
        Ok(fixture)
    }

    async fn store(&self, id: i64) -> Result<SqliteStore, PersistenceError> {
        self.database
            .agent_store(
                AgentId::try_from(id)
                    .map_err(|_| PersistenceError::InvalidData("fixture agent ID"))?,
                limits(),
            )
            .await
    }
}

#[derive(Debug, PartialEq, Eq, sqlx::FromRow)]
struct CreationMetadata {
    public_id: String,
    working_directory: String,
    title: Option<String>,
    created_at_ms: i64,
    updated_at_ms: i64,
    last_opened_at_ms: i64,
    runtime_epoch: i64,
    settings_version: i64,
    settings_json: String,
}

fn private_directory() -> std::io::Result<tempfile::TempDir> {
    let mut builder = tempfile::Builder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    builder.tempdir()
}

fn limits() -> StoreLimits {
    StoreLimits {
        record_bytes: 64 * 1024,
    }
}

fn input(text: &str, invocation: u64) -> Record {
    Record {
        invocation: InvocationId(invocation),
        kind: RecordKind::InputAccepted { input: text.into() },
    }
}

fn model() -> Record {
    Record {
        invocation: InvocationId(1),
        kind: RecordKind::ModelCompleted {
            response: ModelResponse {
                action: ModelAction::Final {
                    answer: "complete".into(),
                },
                continuation: None,
                usage: None,
            },
        },
    }
}

fn cell(invocation: u64) -> Record {
    Record {
        invocation: InvocationId(invocation),
        kind: RecordKind::CellCompleted {
            model: match RecordId::try_from(1) {
                Ok(id) => id,
                Err(error) => panic!("{error}"),
            },
            cell: 0,
            code: "print(23)".into(),
            outcome: CellResponse::Interrupted {
                printed: vec![json!({"before": "interruption", "values": [7, 23]})],
            },
        },
    }
}

fn image() -> Result<Image, Box<dyn Error>> {
    Ok(Image::from_png(b"\x89PNG\r\n\x1a\nfixture")?)
}

fn instructions() -> InstructionState {
    InstructionState {
        invocation: InvocationId(1),
        scoped_sources: vec![ScopedInstruction {
            source_path: PathBuf::from("/workspace/AGENTS.md"),
            content: InstructionContent::Removed,
            delivery: InstructionDelivery::Pending,
        }],
        selected_skills: vec![SelectedSkill {
            source_path: PathBuf::from("/workspace/skills/review/SKILL.md"),
            text: "Retained selected skill".into(),
            delivery: InstructionDelivery::Pending,
        }],
        scoped_feedback: Vec::new(),
        skill_feedback: Vec::new(),
    }
}
