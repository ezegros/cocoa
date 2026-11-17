//! Bounded own-agent original evidence reads against file-backed SQLite.

use std::error::Error;

use cocoa::persistence::{
    EffectiveSessionSettings, HistoryReadLimits, PersistenceError, SessionDatabase,
    SessionExecutionLimits, SqliteStore,
};
use cocoa_core::{
    capability::OperationOutcome,
    compaction::{Checkpoint, CheckpointLimits, CheckpointProposal, Recap},
    context::{ContextContent, ContextType, InstructionFeedback, InstructionState},
    execution::{Cell, CellResponse},
    model::{Image, ModelAction, ModelResponse, ProviderData, UserInput},
    store::{
        InvocationId, OperationId, Record, RecordId, RecordKind, Store, StoreLimits, encoded_size,
    },
};
use miniz_oxide::deflate::compress_to_vec_zlib;
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{Connection, SqliteConnection};

#[tokio::test]
async fn exact_evidence_and_pinned_pages_preserve_originals_across_appends()
-> Result<(), Box<dyn Error>> {
    let directory = private_directory()?;
    let path = directory.path().join("history.sqlite3");
    let database = SessionDatabase::open(&path).await?;
    let session = database.create_session(directory.path()).await?;
    let store = session.root_store(store_limits())?;
    let image = Image::from_png(b"\x89PNG\r\n\x1a\noriginal captured image")?;
    let original = Record {
        invocation: InvocationId(1),
        kind: RecordKind::InputAccepted {
            input: UserInput::WithImages {
                text: "Do not publish. Inspect the failure.".into(),
                images: vec![image],
            },
        },
    };
    let input = store.append(&original, None).await?;
    let started = store.append(&operation_started(), None).await?;
    let failed = store
        .append(
            &Record {
                invocation: InvocationId(1),
                kind: RecordKind::OperationFinished {
                    operation: OperationId(1),
                    outcome: OperationOutcome::Completed {
                        value: json!({"exit_code": 17, "stderr": "older asymmetric failure"}),
                    },
                },
            },
            None,
        )
        .await?;
    let limits = HistoryReadLimits::new(2, 16_384)?;
    let first = store.read_history(None, limits).await?;
    assert_eq!(
        first
            .records
            .iter()
            .map(|record| record.id)
            .collect::<Vec<_>>(),
        vec![input, started]
    );
    assert_eq!(i64::from(first.through.ok_or("missing upper boundary")?), 3);
    assert!(encoded_size(&first)? <= 16_384);
    let cursor = first.next_cursor.ok_or("missing continuation")?;
    let token = cursor.to_string();
    assert_eq!(token.parse::<cocoa::persistence::HistoryCursor>()?, cursor);
    let later = store
        .append(&accepted("later successful run"), None)
        .await?;
    let second = store.read_history(Some(cursor), limits).await?;
    assert_eq!(
        second
            .records
            .iter()
            .map(|record| record.id)
            .collect::<Vec<_>>(),
        vec![failed]
    );
    assert_eq!(second.through, first.through);
    assert!(second.next_cursor.is_none());
    let restored = store
        .read_record(input, 16_384)
        .await?
        .ok_or("input not found")?;
    assert_eq!(restored.record, original);
    assert_eq!(i64::from(restored.sequence), 1);
    let failure = store
        .read_record(failed, 16_384)
        .await?
        .ok_or("failure not found")?;
    assert!(
        matches!(&failure.record.kind, RecordKind::OperationFinished { outcome: OperationOutcome::Completed { value }, .. } if value["exit_code"] == 17)
    );
    let latest = store
        .read_history(None, HistoryReadLimits::new(8, 16_384)?)
        .await?;
    assert_eq!(latest.records.last().ok_or("empty latest page")?.id, later);
    assert_eq!(store.load().await?.records.len(), 4);
    let agent = session.root_agent();
    drop(store);
    drop(session);
    drop(database);
    let reopened = SessionDatabase::open_existing(&path)
        .await?
        .ok_or("database disappeared")?;
    let store = reopened.agent_store(agent, store_limits()).await?;
    assert_eq!(
        store
            .read_record(input, 16_384)
            .await?
            .ok_or("captured evidence disappeared on reopen")?
            .record,
        original
    );
    assert_eq!(
        store.read_history(Some(token.parse()?), limits).await?,
        second
    );
    Ok(())
}

#[tokio::test]
async fn history_reads_enforce_scope_and_bytes_without_loading_unrelated_payloads()
-> Result<(), Box<dyn Error>> {
    let directory = private_directory()?;
    let path = directory.path().join("history.sqlite3");
    let database = SessionDatabase::open(&path).await?;
    let first_session = database.create_session(directory.path()).await?;
    let first = first_session.root_store(store_limits())?;
    let small = first.append(&accepted("small original"), None).await?;
    let large = first
        .append(&accepted(&"large older evidence ".repeat(300)), None)
        .await?;
    let last = first
        .append(&accepted("requested exact evidence"), None)
        .await?;
    let second_session = database.create_session(directory.path()).await?;
    let second = second_session.root_store(store_limits())?;
    let foreign = second.append(&accepted("foreign evidence"), None).await?;
    assert!(first.read_record(foreign, 16_384).await?.is_none());
    assert!(second.read_record(small, 16_384).await?.is_none());
    let page = first
        .read_history(None, HistoryReadLimits::new(8, 1024)?)
        .await?;
    assert_eq!(
        page.records
            .iter()
            .map(|record| record.id)
            .collect::<Vec<_>>(),
        vec![small]
    );
    assert!(encoded_size(&page)? <= 1024);
    let cursor = page.next_cursor.ok_or("missing byte continuation")?;
    assert!(matches!(
        second
            .read_history(Some(cursor.clone()), HistoryReadLimits::new(8, 1024)?)
            .await,
        Err(PersistenceError::InvalidHistoryCursor)
    ));
    assert!(
        matches!(first.read_history(Some(cursor), HistoryReadLimits::new(8, 1024)?).await, Err(PersistenceError::HistoryRecordTooLarge { record, .. }) if record == large)
    );
    assert!(
        matches!(first.read_record(large, 1024).await, Err(PersistenceError::HistoryRecordTooLarge { record, .. }) if record == large)
    );
    let exact = first
        .read_record(last, 1024)
        .await?
        .ok_or("exact record missing")?;
    assert_eq!(exact.record, accepted("requested exact evidence"));
    let mut admin =
        SqliteConnection::connect_with(&sqlx::sqlite::SqliteConnectOptions::new().filename(&path))
            .await?;
    sqlx::query("UPDATE records SET payload_json = '{\"unknown_variant\":{}}' WHERE id = ?")
        .bind(i64::from(large))
        .execute(&mut admin)
        .await?;
    assert_eq!(
        first
            .read_record(last, 1024)
            .await?
            .ok_or("unrelated payload was loaded")?
            .record,
        accepted("requested exact evidence")
    );
    assert!(first.read_record(large, 16_384).await.is_err());
    assert!(
        first
            .read_history(None, HistoryReadLimits::new(8, 16_384)?)
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn empty_history_and_invalid_limits_have_explicit_results() -> Result<(), Box<dyn Error>> {
    let directory = private_directory()?;
    let database = SessionDatabase::open(&directory.path().join("history.sqlite3")).await?;
    let session = database.create_session(directory.path()).await?;
    let store = session.root_store(store_limits())?;
    let page = store
        .read_history(None, HistoryReadLimits::new(1, 1024)?)
        .await?;
    assert!(page.records.is_empty());
    assert!(page.through.is_none());
    assert!(page.next_cursor.is_none());
    let bytes = u64::try_from(encoded_size(&page)?)?;
    assert_eq!(
        store
            .read_history(None, HistoryReadLimits::new(1, bytes)?)
            .await?,
        page
    );
    assert!(
        store
            .read_history(None, HistoryReadLimits::new(1, bytes - 1)?)
            .await
            .is_err()
    );
    assert!(
        store
            .read_record(RecordId::try_from(999)?, 4)
            .await?
            .is_none()
    );
    assert!(
        store
            .read_record(RecordId::try_from(999)?, 3)
            .await
            .is_err()
    );
    for (records, bytes) in [(0, 1024), (65, 1024), (1, 0), (1, u64::MAX)] {
        assert!(matches!(
            HistoryReadLimits::new(records, bytes),
            Err(PersistenceError::InvalidHistoryLimits)
        ));
    }
    for token in [
        "",
        "v1:1:1:1",
        "history-v1:sess_00000000000000000000000000000001:1:2:1",
    ] {
        assert!(token.parse::<cocoa::persistence::HistoryCursor>().is_err());
    }
    assert!(matches!(
        store.read_record(RecordId::try_from(999)?, 0).await,
        Err(PersistenceError::InvalidHistoryLimits)
    ));
    Ok(())
}

#[tokio::test]
async fn response_limits_include_unicode_envelopes_and_full_width_cursor_identities()
-> Result<(), Box<dyn Error>> {
    let directory = private_directory()?;
    let path = directory.path().join("history.sqlite3");
    let database = SessionDatabase::open(&path).await?;
    let mut admin =
        SqliteConnection::connect_with(&sqlx::sqlite::SqliteConnectOptions::new().filename(&path))
            .await?;
    let session = 9_007_199_254_740_993_i64;
    let agent = session + 2;
    let record = session + 4;
    sqlx::query("INSERT INTO sessions (id, public_id, working_directory, created_at_ms, updated_at_ms, last_opened_at_ms, runtime_epoch, settings_version, settings_json) VALUES (?, 'sess_00000000000000000000000000000001', ?, 0, 0, 0, 1, 1, '{\"state\":\"unconfigured\"}')")
        .bind(session).bind(directory.path().to_str().ok_or("directory is not UTF-8")?)
        .execute(&mut admin).await?;
    sqlx::query("INSERT INTO agents (id, session_id, name, format_version, lifecycle) VALUES (?, ?, 'root', 1, 'participating')")
        .bind(agent).bind(session).execute(&mut admin).await?;
    let original = Record {
        invocation: InvocationId(i64::MAX as u64),
        kind: RecordKind::InputAccepted {
            input: "Retain π, 🦀, \"quotes\", a newline\nand a backslash \\".into(),
        },
    };
    sqlx::query("INSERT INTO records (id, session_id, agent_id, sequence, invocation, runtime_epoch, created_at_ms, payload_version, payload_json) VALUES (?, ?, ?, 1, ?, 1, 0, 2, ?)")
        .bind(record).bind(session).bind(agent).bind(i64::MAX)
        .bind(serde_json::to_string(&original.kind)?).execute(&mut admin).await?;
    let store = database
        .agent_store(cocoa_core::store::AgentId::try_from(agent)?, store_limits())
        .await?;
    let id = RecordId::try_from(record)?;
    let exact = store
        .read_record(id, 4096)
        .await?
        .ok_or("missing original")?;
    assert_eq!(exact.record, original);
    let bytes = u64::try_from(serde_json::to_vec(&exact)?.len())?;
    assert_eq!(store.read_record(id, bytes).await?, Some(exact));
    assert!(matches!(
        store.read_record(id, bytes - 1).await,
        Err(PersistenceError::HistoryRecordTooLarge { record, bytes_required })
            if record == id && bytes_required == bytes
    ));
    let later = store.append(&accepted("newer result"), None).await?;
    assert_eq!(i64::from(later), record + 1);
    let page = store
        .read_history(None, HistoryReadLimits::new(1, 4096)?)
        .await?;
    let cursor = page.next_cursor.as_ref().ok_or("missing continuation")?;
    assert_eq!(
        cursor.to_string(),
        format!("history-v1:sess_00000000000000000000000000000001:{agent}:1:2")
    );
    let bytes = u64::try_from(serde_json::to_vec(&page)?.len())?;
    assert_eq!(
        store
            .read_history(None, HistoryReadLimits::new(1, bytes)?)
            .await?,
        page
    );
    assert!(matches!(
        store.read_history(None, HistoryReadLimits::new(1, bytes - 1)?).await,
        Err(PersistenceError::HistoryRecordTooLarge { record, bytes_required })
            if record == id && bytes_required == bytes
    ));
    let continuation = store
        .read_history(
            Some(cursor.to_string().parse()?),
            HistoryReadLimits::new(1, 4096)?,
        )
        .await?;
    assert_eq!(continuation.records[0].id, later);
    assert!(continuation.next_cursor.is_none());
    sqlx::query("INSERT INTO agents (id, session_id, name, parent_agent_id, format_version, assignment_json, lifecycle) VALUES (?, ?, 'child', ?, 1, '{\"prompt\":\"other agent\",\"alias\":\"fixture\"}', 'participating')")
        .bind(agent + 1).bind(session).bind(agent).execute(&mut admin).await?;
    let child = database
        .agent_store(
            cocoa_core::store::AgentId::try_from(agent + 1)?,
            store_limits(),
        )
        .await?;
    let foreign = child
        .append(&accepted("same session, different agent"), None)
        .await?;
    assert!(store.read_record(foreign, 4096).await?.is_none());
    assert!(child.read_record(id, 4096).await?.is_none());
    assert!(matches!(
        child
            .read_history(Some(cursor.clone()), HistoryReadLimits::new(1, 4096)?)
            .await,
        Err(PersistenceError::InvalidHistoryCursor)
    ));
    Ok(())
}

#[tokio::test]
async fn partial_reads_never_acknowledge_unknown_writes_and_recheck_current_authority()
-> Result<(), Box<dyn Error>> {
    let directory = private_directory()?;
    let path = directory.path().join("history.sqlite3");
    let database = SessionDatabase::open(&path).await?;
    let session = database.create_session(directory.path()).await?;
    let store = session.root_store(store_limits())?;
    let first = store.append(&accepted("already accepted"), None).await?;
    let mut admin =
        SqliteConnection::connect_with(&sqlx::sqlite::SqliteConnectOptions::new().filename(&path))
            .await?;
    let transaction = admin.begin_with("BEGIN IMMEDIATE").await?;
    let record = accepted("unknown acknowledgement");
    let mut append = store.append(&record, None);
    assert!(matches!(
        futures_util::poll!(&mut append),
        std::task::Poll::Pending
    ));
    drop(append);
    transaction.rollback().await?;
    assert!(matches!(
        store.read_record(first, 4096).await,
        Err(PersistenceError::OutcomeUnknown)
    ));
    assert!(matches!(
        store
            .read_history(None, HistoryReadLimits::new(1, 4096)?)
            .await,
        Err(PersistenceError::OutcomeUnknown)
    ));
    assert!(
        store
            .append(&accepted("must stay blocked"), None)
            .await
            .is_err()
    );
    let reconciled = store.load().await?;
    assert_eq!(reconciled.records.len(), 2);
    assert_eq!(reconciled.records[1].record, record);
    let page = store
        .read_history(None, HistoryReadLimits::new(1, 4096)?)
        .await?;
    let cursor = page.next_cursor.ok_or("missing continuation")?;
    sqlx::query("UPDATE sessions SET runtime_epoch = runtime_epoch + 1 WHERE id = ?")
        .bind(i64::from(session.id()))
        .execute(&mut admin)
        .await?;
    assert!(matches!(
        store.read_record(first, 4096).await,
        Err(PersistenceError::AgentUnavailable)
    ));
    assert!(matches!(
        store
            .read_history(Some(cursor), HistoryReadLimits::new(1, 4096)?)
            .await,
        Err(PersistenceError::AgentUnavailable)
    ));
    Ok(())
}

#[tokio::test]
async fn archived_messages_execution_images_receipts_and_checkpoints_survive_recovery()
-> Result<(), Box<dyn Error>> {
    let directory = private_directory()?;
    let path = directory.path().join("archives.sqlite3");
    let database = SessionDatabase::open(&path).await?;
    let session = database.create_session(directory.path()).await?;
    let store = session.root_store_with_settings(store_limits(), effective_settings())?;
    let original = Record {
        invocation: InvocationId(1),
        kind: RecordKind::InputAccepted {
            input: UserInput::WithImages {
                text: format!(
                    "{} archived user needle 🦀",
                    "retain this original ".repeat(500)
                ),
                images: vec![Image::from_png(
                    b"\x89PNG\r\n\x1a\noriginal captured image",
                )?],
            },
        },
    };
    let input = store.append(&original, None).await?;
    assert!(session.model_switch_allowed().await?);
    let mut state = InstructionState {
        invocation: InvocationId(1),
        scoped_sources: Vec::new(),
        selected_skills: Vec::new(),
        scoped_feedback: Vec::new(),
        skill_feedback: Vec::new(),
    };
    let model = store
        .append(
            &Record {
                invocation: InvocationId(1),
                kind: RecordKind::ModelCompleted {
                    shared_context: None,
                    response: ModelResponse {
                        action: ModelAction::Execute {
                            cells: vec![Cell {
                                code: "print(original)".into(),
                            }],
                        },
                        continuation: Some(ProviderData {
                            provider: "archive-fixture".into(),
                            data: json!({
                                "opaque": "unchanged continuation ".repeat(500),
                                "numbers": [f64::from_bits(1), -0.0, i64::MIN, u64::MAX],
                            }),
                        }),
                        usage: None,
                    },
                },
            },
            Some(&state),
        )
        .await?;
    store
        .append(
            &Record {
                invocation: InvocationId(1),
                kind: RecordKind::CellStarted { model, cell: 0 },
            },
            None,
        )
        .await?;
    let content = ContextContent {
        kind: ContextType::Decision,
        value: json!("retain accepted exact decision ".repeat(100)),
        labels: Vec::new(),
    };
    let start = store
        .append(
            &Record {
                invocation: InvocationId(1),
                kind: RecordKind::OperationStarted {
                    operation: OperationId(1),
                    name: "context.set".into(),
                    arguments: json!({"key":"decision", "content":content}),
                },
            },
            None,
        )
        .await?;
    let receipt = store
        .set_context(start, "decision", &content, 65536)
        .await?;
    store
        .append(
            &Record {
                invocation: InvocationId(1),
                kind: RecordKind::OperationFinished {
                    operation: OperationId(1),
                    outcome: receipt.clone(),
                },
            },
            None,
        )
        .await?;
    let cell = store
        .append(
            &Record {
                invocation: InvocationId(1),
                kind: RecordKind::CellCompleted {
                    model,
                    cell: 0,
                    code: "print(original)".into(),
                    outcome: CellResponse::Completed {
                        printed: vec![
                            json!({"exit":17, "stderr":"older asymmetric failure ".repeat(1000)}),
                        ],
                    },
                },
            },
            None,
        )
        .await?;
    let originals = store.load().await?.records;
    publish_archive_checkpoint(&store, cell).await?;
    assert_eq!(store.load().await?.records, originals);
    assert_eq!(
        store
            .set_context(start, "decision", &content, 65536)
            .await?,
        receipt
    );
    assert_eq!(store.operation_receipt(start).await?, Some(receipt.clone()));
    assert!(!session.model_switch_allowed().await?);
    assert!(
        matches!(store.read_record(input, 1024).await, Err(PersistenceError::HistoryRecordTooLarge { record, .. }) if record == input)
    );
    state.scoped_feedback.push(InstructionFeedback {
        record: cell,
        text: "original archived execution feedback".into(),
    });
    let final_record = store
        .append(
            &Record {
                invocation: InvocationId(1),
                kind: RecordKind::ModelCompleted {
                    shared_context: None,
                    response: ModelResponse {
                        action: ModelAction::Final {
                            answer: "archived assistant needle ".repeat(500),
                        },
                        continuation: None,
                        usage: None,
                    },
                },
            },
            Some(&state),
        )
        .await?;
    let checkpoint = publish_archive_checkpoint(&store, final_record).await?;
    let before = store.load().await?;
    assert_eq!(before.checkpoints.len(), 2);
    assert_eq!(before.instruction_state, Some(state));
    assert_eq!(
        session.load(store_limits()).await?.histories[&session.root_agent()],
        before
    );
    for record in &before.records {
        assert_eq!(
            store.read_record(record.id, 128 * 1024).await?.as_ref(),
            Some(record)
        );
    }
    let page = store
        .read_history(None, HistoryReadLimits::new(64, 128 * 1024)?)
        .await?;
    assert_eq!(page.records, before.records);
    for query in ["archived user needle", "archived assistant needle"] {
        let found = database.search_sessions(None, query, 5).await?;
        assert_eq!(found.sessions.len(), 1);
        assert_eq!(found.sessions[0].session.id, session.id());
    }
    let mut admin =
        SqliteConnection::connect_with(&sqlx::sqlite::SqliteConnectOptions::new().filename(&path))
            .await?;
    let archived: Vec<(i64, String, i64, i64)> = sqlx::query_as(
        "SELECT id, archive_kind, archive_decoded_bytes, length(archive_zlib) FROM records WHERE archive_zlib IS NOT NULL ORDER BY sequence",
    ).fetch_all(&mut admin).await?;
    assert_eq!(archived.len(), 6);
    assert_eq!(archived[0].0, i64::from(input));
    assert_eq!(archived[0].1, "InputAccepted");
    let source_bytes: i64 = archived.iter().map(|row| row.2).sum();
    let archive_bytes: i64 = archived.iter().map(|row| row.3 + 128).sum();
    assert!(archive_bytes * 4 < source_bytes);
    let images: (i64, i64, bool) = sqlx::query_as(
        "SELECT input_images, archive_input_images, is_input FROM records WHERE id = ?",
    )
    .bind(i64::from(input))
    .fetch_one(&mut admin)
    .await?;
    assert_eq!(images, (1, 1, true));
    let reference = session.public_id().to_string();
    drop(admin);
    drop(store);
    drop(session);
    drop(database);
    let reopened = SessionDatabase::open_existing(&path)
        .await?
        .ok_or("missing database")?;
    let session = reopened.attach_session(&reference, store_limits()).await?;
    let store = session.root_store(store_limits())?;
    assert_eq!(store.load().await?, before);
    assert_eq!(
        session.load(store_limits()).await?.histories[&session.root_agent()],
        before
    );
    assert_eq!(
        store
            .read_record(input, 128 * 1024)
            .await?
            .ok_or("missing original image")?
            .record,
        original
    );
    assert_eq!(store.operation_receipt(start).await?, Some(receipt));
    drop(store);
    let recovered = session.recover(store_limits()).await?;
    let repaired = recovered.root_store(store_limits())?.load().await?;
    assert_eq!(&repaired.records[..before.records.len()], before.records);
    assert_eq!(repaired.checkpoints, before.checkpoints);
    assert_eq!(repaired.records.len(), before.records.len() + 1);
    assert!(
        repaired
            .records
            .last()
            .ok_or("missing recovery terminal")?
            .id
            > checkpoint
    );
    Ok(())
}

#[tokio::test]
async fn archived_evidence_rejects_trailing_truncated_wrong_size_digest_and_kind()
-> Result<(), Box<dyn Error>> {
    let directory = private_directory()?;
    let path = directory.path().join("archive-integrity.sqlite3");
    let database = SessionDatabase::open(&path).await?;
    let session = database.create_session(directory.path()).await?;
    let store = session.root_store(store_limits())?;
    let original = accepted(&"retained original evidence ".repeat(1000));
    let id = store.append(&original, None).await?;
    let text = serde_json::to_vec(&original.kind)?;
    let zlib = compress_to_vec_zlib(&text, 6);
    let digest = Sha256::digest(&text).to_vec();
    let bytes = i64::try_from(text.len())?;
    let mut admin =
        SqliteConnection::connect_with(&sqlx::sqlite::SqliteConnectOptions::new().filename(&path))
            .await?;
    sqlx::query("UPDATE sessions SET title = NULL")
        .execute(&mut admin)
        .await?;
    let mut trailing = zlib.clone();
    trailing.extend_from_slice(b"trailing bytes");
    let mut concatenated = zlib.clone();
    concatenated.extend_from_slice(&zlib);
    let mut truncated = zlib.clone();
    truncated.truncate(truncated.len() - 1);
    let mut wrong_digest = digest.clone();
    wrong_digest[0] ^= 1;
    for (case, encoded, decoded, sha256, kind) in [
        ("trailing", trailing, bytes, digest.clone(), "InputAccepted"),
        (
            "concatenated",
            concatenated,
            bytes,
            digest.clone(),
            "InputAccepted",
        ),
        (
            "truncated",
            truncated,
            bytes,
            digest.clone(),
            "InputAccepted",
        ),
        (
            "smaller",
            zlib.clone(),
            bytes - 1,
            digest.clone(),
            "InputAccepted",
        ),
        (
            "larger",
            zlib.clone(),
            bytes + 1,
            digest.clone(),
            "InputAccepted",
        ),
        (
            "expansion",
            zlib.clone(),
            1,
            digest.clone(),
            "InputAccepted",
        ),
        ("digest", zlib.clone(), bytes, wrong_digest, "InputAccepted"),
        (
            "kind",
            zlib.clone(),
            bytes,
            digest.clone(),
            "ModelCompleted",
        ),
    ] {
        sqlx::query("UPDATE records SET payload_json = NULL, archive_zlib = ?, archive_decoded_bytes = ?, archive_sha256 = ?, archive_kind = ?, archive_input_images = 0 WHERE id = ?")
            .bind(encoded).bind(decoded).bind(sha256).bind(kind).bind(i64::from(id))
            .execute(&mut admin).await?;
        assert!(
            matches!(
                store.read_record(id, 128 * 1024).await,
                Err(PersistenceError::InvalidData(_))
            ),
            "accepted {case}"
        );
        assert!(
            store
                .read_history(None, HistoryReadLimits::new(64, 128 * 1024)?)
                .await
                .is_err(),
            "paged {case}"
        );
        assert!(store.load().await.is_err(), "loaded {case}");
        assert!(
            database.search_sessions(None, "retained", 5).await.is_err(),
            "searched {case}"
        );
    }
    sqlx::query("UPDATE records SET archive_zlib = ?, archive_decoded_bytes = ?, archive_sha256 = ?, archive_kind = 'InputAccepted' WHERE id = ?")
        .bind(zlib).bind(bytes).bind(digest).bind(i64::from(id)).execute(&mut admin).await?;
    assert_eq!(
        store
            .read_record(id, 128 * 1024)
            .await?
            .ok_or("missing valid archive")?
            .record,
        original
    );
    sqlx::query("UPDATE records SET archive_input_images = 1")
        .execute(&mut admin)
        .await?;
    assert!(matches!(
        store.read_record(id, 128 * 1024).await,
        Err(PersistenceError::InvalidData(_))
    ));
    sqlx::query("UPDATE records SET archive_input_images = 0")
        .execute(&mut admin)
        .await?;
    for update in [
        "UPDATE records SET archive_decoded_bytes = 8388609",
        "UPDATE records SET archive_decoded_bytes = 0",
        "UPDATE records SET archive_zlib = zeroblob(8388609)",
        "UPDATE records SET archive_zlib = zeroblob(0)",
        "UPDATE records SET archive_sha256 = zeroblob(31)",
        "UPDATE records SET archive_kind = 'Unknown'",
        "UPDATE records SET archive_input_images = -1",
        "UPDATE records SET payload_json = '{}'",
    ] {
        assert!(
            sqlx::query(update).execute(&mut admin).await.is_err(),
            "accepted {update}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn archive_preparation_caps_record_count_and_source_bytes_without_touching_foreign_records()
-> Result<(), Box<dyn Error>> {
    let directory = private_directory()?;
    let path = directory.path().join("archive-bounds.sqlite3");
    let database = SessionDatabase::open(&path).await?;
    let session = database.create_session(directory.path()).await?;
    let store = session.root_store(store_limits())?;
    let foreign_session = database.create_session(directory.path()).await?;
    let foreign_store = foreign_session.root_store(store_limits())?;
    let foreign = foreign_store
        .append(&accepted(&"foreign original ".repeat(500)), None)
        .await?;
    let mut through = store
        .append(&accepted(&"original evidence ".repeat(100)), None)
        .await?;
    for _ in 1..65 {
        through = store
            .append(&accepted(&"original evidence ".repeat(100)), None)
            .await?;
    }
    publish_archive_checkpoint(&store, through).await?;
    let mut admin =
        SqliteConnection::connect_with(&sqlx::sqlite::SqliteConnectOptions::new().filename(&path))
            .await?;
    let counts: (i64, i64) = sqlx::query_as(
        "SELECT count(archive_zlib), count(payload_json) FROM records WHERE agent_id = ?",
    )
    .bind(i64::from(session.root_agent()))
    .fetch_one(&mut admin)
    .await?;
    assert_eq!(counts, (64, 1));
    let foreign_inline: bool =
        sqlx::query_scalar("SELECT payload_json IS NOT NULL FROM records WHERE id = ?")
            .bind(i64::from(foreign))
            .fetch_one(&mut admin)
            .await?;
    assert!(foreign_inline);
    let large_session = database.create_session(directory.path()).await?;
    let large = large_session.root_store(StoreLimits {
        record_bytes: 10 * 1024 * 1024,
    })?;
    let oversized = accepted(&"x".repeat(8 * 1024 * 1024));
    let oversized_id = large.append(&oversized, None).await?;
    let source = accepted(&"x".repeat(1024 * 1024));
    let mut through = large.append(&source, None).await?;
    for _ in 1..9 {
        through = large.append(&source, None).await?;
    }
    publish_archive_checkpoint(&large, through).await?;
    let accounting: (i64, i64, i64) = sqlx::query_as("SELECT count(archive_zlib), count(payload_json), sum(coalesce(archive_decoded_bytes, 0)) FROM records WHERE agent_id = ?")
        .bind(i64::from(large_session.root_agent())).fetch_one(&mut admin).await?;
    assert_eq!((accounting.0, accounting.1), (7, 3));
    assert!(accounting.2 <= 8 * 1024 * 1024);
    let oversized_inline: bool =
        sqlx::query_scalar("SELECT payload_json IS NOT NULL FROM records WHERE id = ?")
            .bind(i64::from(oversized_id))
            .fetch_one(&mut admin)
            .await?;
    assert!(oversized_inline);
    assert_eq!(
        large
            .read_record(oversized_id, 10 * 1024 * 1024)
            .await?
            .ok_or("missing oversized inline original")?
            .record,
        oversized
    );
    assert_eq!(large.load().await?.records.len(), 10);
    let exact_session = database.create_session(directory.path()).await?;
    let exact = exact_session.root_store(StoreLimits {
        record_bytes: 10 * 1024 * 1024,
    })?;
    let overhead = serde_json::to_vec(&accepted("").kind)?.len();
    let original = accepted(&"x".repeat(8 * 1024 * 1024 - overhead));
    let id = exact.append(&original, None).await?;
    publish_archive_checkpoint(&exact, id).await?;
    let decoded: Option<i64> =
        sqlx::query_scalar("SELECT archive_decoded_bytes FROM records WHERE id = ?")
            .bind(i64::from(id))
            .fetch_one(&mut admin)
            .await?;
    assert_eq!(decoded, Some(8 * 1024 * 1024));
    assert_eq!(
        exact
            .read_record(id, 10 * 1024 * 1024)
            .await?
            .ok_or("missing maximum-sized archive")?
            .record,
        original
    );
    Ok(())
}

#[tokio::test]
async fn archives_only_replace_covered_originals_when_their_savings_pay_for_metadata()
-> Result<(), Box<dyn Error>> {
    let directory = private_directory()?;
    let path = directory.path().join("archive-selection.sqlite3");
    let database = SessionDatabase::open(&path).await?;
    let session = database.create_session(directory.path()).await?;
    let store = session.root_store(store_limits())?;
    let first = store
        .append(&accepted(&"covered original ".repeat(500)), None)
        .await?;
    let distinct: String = (b'!'..=b'~')
        .filter(|byte| !matches!(byte, b'"' | b'\\'))
        .map(char::from)
        .collect();
    let small = store.append(&accepted(&distinct), None).await?;
    let later = store
        .append(&accepted(&"uncovered original ".repeat(500)), None)
        .await?;
    publish_archive_checkpoint(&store, first).await?;
    let mut admin =
        SqliteConnection::connect_with(&sqlx::sqlite::SqliteConnectOptions::new().filename(&path))
            .await?;
    let inline: Vec<(i64, bool)> =
        sqlx::query_as("SELECT id, payload_json IS NOT NULL FROM records ORDER BY sequence")
            .fetch_all(&mut admin)
            .await?;
    assert_eq!(
        inline,
        [
            (i64::from(first), false),
            (i64::from(small), true),
            (i64::from(later), true)
        ]
    );
    assert!(session.model_switch_allowed().await?);
    publish_archive_checkpoint(&store, later).await?;
    let inline: Vec<(i64, bool)> =
        sqlx::query_as("SELECT id, payload_json IS NOT NULL FROM records ORDER BY sequence")
            .fetch_all(&mut admin)
            .await?;
    assert_eq!(
        inline,
        [
            (i64::from(first), false),
            (i64::from(small), true),
            (i64::from(later), false)
        ]
    );
    assert_eq!(
        store
            .read_record(small, 4096)
            .await?
            .ok_or("missing uncompressed original")?
            .record,
        accepted(&distinct)
    );
    Ok(())
}

#[tokio::test]
async fn incompressible_prefixes_do_not_starve_later_archive_candidates()
-> Result<(), Box<dyn Error>> {
    let directory = private_directory()?;
    let path = directory.path().join("archive-attempts.sqlite3");
    let database = SessionDatabase::open(&path).await?;
    let session = database.create_session(directory.path()).await?;
    let store = session.root_store(store_limits())?;
    let distinct: String = (b'!'..=b'~')
        .filter(|byte| !matches!(byte, b'"' | b'\\'))
        .map(char::from)
        .collect();
    for _ in 0..64 {
        store.append(&accepted(&distinct), None).await?;
    }
    let candidate = store
        .append(&accepted(&"later archive candidate ".repeat(500)), None)
        .await?;
    publish_archive_checkpoint(&store, candidate).await?;
    let mut admin =
        SqliteConnection::connect_with(&sqlx::sqlite::SqliteConnectOptions::new().filename(&path))
            .await?;
    let archived: i64 = sqlx::query_scalar("SELECT count(archive_zlib) FROM records")
        .fetch_one(&mut admin)
        .await?;
    assert_eq!(archived, 0);
    let next = store.append(&accepted("next boundary"), None).await?;
    publish_archive_checkpoint(&store, next).await?;
    let archived: Vec<i64> =
        sqlx::query_scalar("SELECT id FROM records WHERE archive_zlib IS NOT NULL")
            .fetch_all(&mut admin)
            .await?;
    assert_eq!(archived, [i64::from(candidate)]);
    Ok(())
}

#[tokio::test]
async fn a_same_size_source_change_rolls_back_the_entire_archive_switch()
-> Result<(), Box<dyn Error>> {
    let directory = private_directory()?;
    let path = directory.path().join("archive-identity.sqlite3");
    let database = SessionDatabase::open(&path).await?;
    let session = database.create_session(directory.path()).await?;
    let store = session.root_store(store_limits())?;
    store
        .append(&accepted(&"first original ".repeat(500)), None)
        .await?;
    let second = store
        .append(&accepted(&"second original ".repeat(500)), None)
        .await?;
    let originals = store.load().await?.records;
    let mut admin =
        SqliteConnection::connect_with(&sqlx::sqlite::SqliteConnectOptions::new().filename(&path))
            .await?;
    sqlx::raw_sql(
        "CREATE TRIGGER change_prepared_source AFTER UPDATE OF archive_zlib ON records WHEN NEW.sequence = 1 \
         BEGIN UPDATE records SET payload_json = replace(payload_json, 'second original', 'edited original') \
         WHERE session_id = NEW.session_id AND agent_id = NEW.agent_id AND sequence = 2; END",
    ).execute(&mut admin).await?;
    publish_archive_checkpoint(&store, second).await?;
    let snapshot = store.load().await?;
    assert_eq!(snapshot.records, originals);
    assert_eq!(snapshot.checkpoints.len(), 1);
    let archived: i64 = sqlx::query_scalar("SELECT count(archive_zlib) FROM records")
        .fetch_one(&mut admin)
        .await?;
    assert_eq!(archived, 0);
    Ok(())
}

async fn publish_archive_checkpoint(
    store: &SqliteStore,
    through: RecordId,
) -> Result<RecordId, Box<dyn Error>> {
    let snapshot = store.load().await?;
    let checkpoint = Checkpoint::prepare(
        &snapshot,
        through,
        CheckpointProposal {
            additions: Vec::new(),
            changes: Vec::new(),
            recap: Recap {
                goal: "Retain exact original evidence".into(),
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
    )?;
    Ok(store.publish_checkpoint(&checkpoint).await?)
}

fn effective_settings() -> EffectiveSessionSettings {
    EffectiveSessionSettings {
        alias: "fixture".into(),
        provider: "fixture".into(),
        protocol: "fixture".into(),
        model: "fixture".into(),
        endpoint: None,
        account: None,
        thinking_level: None,
        reasoning_summary: false,
        execution_version: 1,
        aliases: std::collections::BTreeMap::new(),
        limits: SessionExecutionLimits {
            input_bytes: 128 * 1024,
            output_bytes: 128 * 1024,
            record_bytes: 128 * 1024,
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

fn private_directory() -> std::io::Result<tempfile::TempDir> {
    let mut builder = tempfile::Builder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    builder.tempdir()
}

fn store_limits() -> StoreLimits {
    StoreLimits {
        record_bytes: 128 * 1024,
    }
}

fn accepted(text: &str) -> Record {
    Record {
        invocation: InvocationId(1),
        kind: RecordKind::InputAccepted { input: text.into() },
    }
}

fn operation_started() -> Record {
    Record {
        invocation: InvocationId(1),
        kind: RecordKind::OperationStarted {
            operation: OperationId(1),
            name: "shell.run".into(),
            arguments: json!({"command": "run asymmetric check"}),
        },
    }
}
