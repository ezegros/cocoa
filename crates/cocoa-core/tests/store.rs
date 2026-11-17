//! Atomic record-limit rejection and canonical feedback retention through the store API.

use std::{collections::BTreeMap, error::Error};

use cocoa_core::{
    context::{
        InstructionContent, InstructionDelivery, InstructionFeedback, InstructionState,
        ScopedInstruction, SelectedSkill,
    },
    execution::CellResponse,
    model::{ModelAction, ModelResponse},
    store::{
        AgentId, IdentityError, InMemoryStore, InstructionStateError, InvocationId, OperationId,
        Record, RecordId, RecordKind, RecordSequence, Store, StoreError, StoreLimits,
    },
};

fn input(text: &str) -> Record {
    Record {
        invocation: InvocationId(1),
        kind: RecordKind::InputAccepted { input: text.into() },
    }
}

#[tokio::test]
async fn oversized_record_is_rejected_without_partial_append() -> Result<(), Box<dyn Error>> {
    let store = InMemoryStore::new(StoreLimits { record_bytes: 128 })?;
    assert_eq!(
        store.append(&input(&"é".repeat(65)), None).await,
        Err(StoreError::RecordLimit)
    );
    assert!(store.load().await?.records.is_empty());
    store.append(&input("still usable"), None).await?;
    let snapshot = store.load().await?;
    assert_eq!(i64::from(snapshot.records[0].sequence), 1);
    let active = store.load_active().await?;
    assert_eq!(active.records(), snapshot.records);
    assert_eq!(active.suffix(), snapshot.records);
    assert!(active.checkpoint().is_none());
    assert!(active.instruction_state().is_none());
    assert_eq!(snapshot.into_records(), vec![input("still usable")]);
    Ok(())
}

#[tokio::test]
async fn unlimited_record_count_accepts_more_than_the_previous_cap() -> Result<(), Box<dyn Error>> {
    let store = InMemoryStore::new(StoreLimits { record_bytes: 1024 })?;
    for index in 0..=1024_u64 {
        store
            .append(
                &Record {
                    invocation: InvocationId(index + 1),
                    kind: RecordKind::InputAccepted {
                        input: format!("input {index}").into(),
                    },
                },
                None,
            )
            .await?;
    }
    assert_eq!(store.load().await?.records.len(), 1025);
    Ok(())
}

#[tokio::test]
async fn canonical_cell_feedback_retains_prints_before_success_or_failure()
-> Result<(), Box<dyn Error>> {
    for outcome in [
        serde_json::json!({"status":"completed","printed":[{"value":21},"two more"]}),
        serde_json::json!({"status":"failed","error":{"kind":"javascript","message":"later failure","truncated":false},"printed":["effect completed 🦀"]}),
        serde_json::json!({"status":"failed","error":{"kind":"output_limit","limit_bytes":4294967295_u32}}),
    ] {
        let store = InMemoryStore::new(StoreLimits { record_bytes: 4096 })?;
        store
            .append(
                &Record {
                    invocation: InvocationId(1),
                    kind: RecordKind::CellCompleted {
                        model: cocoa_core::store::RecordId::try_from(1)?,
                        cell: 0,
                        code: "fixture".into(),
                        outcome: serde_json::from_value(outcome.clone())?,
                    },
                },
                None,
            )
            .await?;
        let records = store.load().await?.into_records();
        let RecordKind::CellCompleted {
            outcome: retained, ..
        } = &records[0].kind
        else {
            return Err("cell feedback was not retained".into());
        };
        assert_eq!(serde_json::to_value(retained)?, outcome);
    }
    assert!(serde_json::from_value::<cocoa_core::execution::CellResponse>(
        serde_json::json!({"status":"failed","error":{"kind":"output_limit","limit_bytes":4294967296_u64}})
    ).is_err());
    for effects in ["none", "partial", "unknown"] {
        for kind in ["context", "capability"] {
            let outcome = serde_json::json!({
                "status": "failed",
                "error": {
                    "kind": kind,
                    "method": "context.set",
                    "code": "context_owned",
                    "message": "another agent owns the entry",
                    "owner": null,
                    "operation_effects": effects,
                    "truncated": false
                }
            });
            let decoded = serde_json::from_value::<CellResponse>(outcome.clone());
            if kind == "context" && effects != "none" {
                assert!(
                    decoded.is_err(),
                    "accepted contradictory context effects: {outcome}"
                );
            } else {
                assert_eq!(
                    serde_json::to_value(decoded?)?["error"]["operation_effects"],
                    effects
                );
            }
        }
    }
    Ok(())
}

#[test]
fn stored_identities_enforce_positive_signed_integers() -> Result<(), Box<dyn Error>> {
    for value in [i64::MIN, -1, 0] {
        assert_eq!(AgentId::try_from(value), Err(IdentityError::NonPositive));
        assert_eq!(RecordId::try_from(value), Err(IdentityError::NonPositive));
        assert_eq!(
            RecordSequence::try_from(value),
            Err(IdentityError::NonPositive)
        );
        assert!(serde_json::from_str::<AgentId>(&value.to_string()).is_err());
        assert!(serde_json::from_str::<RecordId>(&value.to_string()).is_err());
        assert!(serde_json::from_str::<RecordSequence>(&value.to_string()).is_err());
    }
    for value in [1, 9_007_199_254_740_993, i64::MAX] {
        assert_eq!(i64::from(AgentId::try_from(value)?), value);
        assert_eq!(i64::from(RecordId::try_from(value)?), value);
        assert_eq!(i64::from(RecordSequence::try_from(value)?), value);
        let encoded = value.to_string();
        let agent_encoded = serde_json::to_string(&AgentId::try_from(value)?)?;
        assert_eq!(agent_encoded, format!("\"{value}\""));
        assert_eq!(
            i64::from(serde_json::from_str::<AgentId>(&agent_encoded)?),
            value
        );
        assert!(serde_json::from_str::<AgentId>(&encoded).is_err());
        assert_eq!(
            i64::from(serde_json::from_str::<RecordId>(&encoded)?),
            value
        );
        assert_eq!(
            i64::from(serde_json::from_str::<RecordSequence>(&encoded)?),
            value
        );
    }
    let overflow = "9223372036854775808";
    assert!(serde_json::from_str::<AgentId>(overflow).is_err());
    assert!(serde_json::from_str::<RecordId>(overflow).is_err());
    assert!(serde_json::from_str::<RecordSequence>(overflow).is_err());
    for value in ["0", "-1", "+1", "01", " 1", "9223372036854775808"] {
        assert!(serde_json::from_value::<AgentId>(serde_json::json!(value)).is_err());
    }
    Ok(())
}

#[tokio::test]
async fn invalid_record_counters_never_consume_history_positions() -> Result<(), Box<dyn Error>> {
    let store = InMemoryStore::new(limits())?;
    for value in [0, i64::MAX as u64 + 1, u64::MAX] {
        let mut record = input("invalid invocation");
        record.invocation = InvocationId(value);
        assert_eq!(record.validate(), Err(StoreError::InvalidRecord));
        assert_eq!(
            store.append(&record, None).await,
            Err(StoreError::InvalidRecord)
        );
        for kind in [
            RecordKind::OperationStarted {
                operation: OperationId(value),
                name: "read".into(),
                arguments: serde_json::json!({}),
            },
            RecordKind::OperationFinished {
                operation: OperationId(value),
                outcome: cocoa_core::capability::OperationOutcome::Unknown,
            },
        ] {
            assert_eq!(
                store
                    .append(
                        &Record {
                            invocation: InvocationId(1),
                            kind
                        },
                        None
                    )
                    .await,
                Err(StoreError::InvalidRecord)
            );
        }
        assert!(store.load().await?.records.is_empty());
    }
    store.append(&input("valid"), None).await?;
    assert_eq!(i64::from(store.load().await?.records[0].sequence), 1);
    let mut maximum = cell();
    maximum.invocation = InvocationId(i64::MAX as u64);
    maximum.validate()?;
    store.append(&maximum, None).await?;
    assert_eq!(i64::from(store.load().await?.records[1].sequence), 2);
    Ok(())
}

#[tokio::test]
async fn snapshots_accept_typed_delivery_and_preserve_it_on_ordinary_appends()
-> Result<(), Box<dyn Error>> {
    let store: Box<dyn Store> = Box::new(InMemoryStore::with_instruction_state(limits())?);
    assert_eq!(
        store.append(&model(), None).await,
        Err(StoreError::InstructionStateRequired)
    );
    assert!(store.load().await?.records.is_empty());
    assert!(store.load().await?.instruction_state.is_none());

    let pending = instructions();
    let input_id = store.append(&input("request"), Some(&pending)).await?;
    let cell_id = store.append(&cell(), None).await?;
    let mut delivered = pending.clone();
    delivered.scoped_sources[0].delivery = InstructionDelivery::Delivered;
    delivered.selected_skills[0].delivery = InstructionDelivery::Delivered;
    delivered.scoped_feedback.push(InstructionFeedback {
        record: cell_id,
        text: "Captured scoped rules before the source changed".into(),
    });
    delivered.skill_feedback.push(InstructionFeedback {
        record: cell_id,
        text: "Captured skill body".into(),
    });
    let model_id = store.append(&model(), Some(&delivered)).await?;
    assert_ne!(input_id, cell_id);
    assert_ne!(cell_id, model_id);
    let before = store.load().await?;
    assert_eq!(before.instruction_state.as_ref(), Some(&delivered));
    assert_eq!(
        before
            .records
            .iter()
            .map(|stored| stored.id)
            .collect::<Vec<_>>(),
        vec![input_id, cell_id, model_id]
    );
    assert_eq!(
        before
            .records
            .iter()
            .map(|stored| i64::from(stored.sequence))
            .collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert_eq!(
        before.clone().into_records(),
        vec![input("request"), cell(), model()]
    );
    let decoded = serde_json::from_slice::<InstructionState>(&serde_json::to_vec(&delivered)?)?;
    assert_eq!(decoded, delivered);
    let completed_cells = BTreeMap::from([(cell_id, InvocationId(1))]);
    decoded.validate_bounds(usize::MAX)?;
    decoded.validate_feedback_targets(&completed_cells)?;
    decoded.validate_transition(&model(), Some(&pending), &completed_cells)?;
    assert_eq!(
        decoded.validate_bounds(0),
        Err(StoreError::InstructionState(InstructionStateError::Limit))
    );
    assert_eq!(
        decoded.validate_feedback_targets(&BTreeMap::new()),
        Err(StoreError::InstructionState(
            InstructionStateError::FeedbackTarget { record: cell_id }
        ))
    );
    assert_eq!(
        decoded.validate_transition(&model(), Some(&pending), &BTreeMap::new()),
        Err(StoreError::InstructionState(
            InstructionStateError::FeedbackTarget { record: cell_id }
        ))
    );
    assert_eq!(
        decoded.validate_transition(&input("not delivery"), None, &completed_cells),
        Err(StoreError::InstructionState(
            InstructionStateError::Delivery
        ))
    );
    let mut later = decoded.clone();
    later.invocation = InvocationId(2);
    assert_eq!(
        decoded.validate_transition(&model(), Some(&later), &completed_cells),
        Err(StoreError::InstructionState(
            InstructionStateError::Invocation
        ))
    );

    store.append(&input("ordinary append"), None).await?;
    let after = store.load().await?;
    assert_eq!(after.instruction_state.as_ref(), Some(&delivered));
    assert_eq!(&after.records[..3], before.records.as_slice());
    assert_eq!(i64::from(after.records[3].sequence), 4);
    assert_eq!(
        store.append(&model(), None).await,
        Err(StoreError::InstructionStateRequired)
    );
    assert_eq!(store.load().await?, after);

    let plain = InMemoryStore::new(limits())?;
    plain.append(&model(), None).await?;
    assert!(plain.load().await?.instruction_state.is_none());
    plain.append(&input("attach state"), Some(&pending)).await?;
    assert_eq!(
        plain.append(&model(), None).await,
        Err(StoreError::InstructionStateRequired)
    );
    Ok(())
}

#[tokio::test]
async fn rejected_transitions_preserve_pending_gates_and_historical_feedback()
-> Result<(), Box<dyn Error>> {
    let store = InMemoryStore::with_instruction_state(limits())?;
    let target = store.append(&cell(), Some(&instructions())).await?;
    let mut retained = instructions();
    retained.scoped_sources[0].delivery = InstructionDelivery::Delivered;
    retained.selected_skills[0].delivery = InstructionDelivery::Delivered;
    retained.scoped_feedback.push(InstructionFeedback {
        record: target,
        text: "Original rules".into(),
    });
    retained.skill_feedback.push(InstructionFeedback {
        record: target,
        text: "Original skill".into(),
    });
    store.append(&model(), Some(&retained)).await?;
    let before = store.load().await?;

    for kind in [
        "deliver source",
        "deliver skill",
        "change source",
        "change skill",
        "drop source",
        "drop skill",
        "feedback text",
        "drop feedback",
        "invocation",
    ] {
        let mut candidate = retained.clone();
        let error = match kind {
            "deliver source" => {
                candidate.scoped_sources[1].delivery = InstructionDelivery::Delivered;
                InstructionStateError::Delivery
            }
            "deliver skill" => {
                candidate.selected_skills[1].delivery = InstructionDelivery::Delivered;
                InstructionStateError::Delivery
            }
            "change source" => {
                candidate.scoped_sources[0].content =
                    InstructionContent::Text("not delivered".into());
                InstructionStateError::Delivery
            }
            "change skill" => {
                candidate.selected_skills[0].text = "not delivered".into();
                InstructionStateError::Delivery
            }
            "drop source" => {
                candidate.scoped_sources.pop();
                InstructionStateError::Delivery
            }
            "drop skill" => {
                candidate.selected_skills.pop();
                InstructionStateError::Delivery
            }
            "feedback text" => {
                candidate.scoped_feedback[0].text = "rewritten".into();
                InstructionStateError::Feedback
            }
            "drop feedback" => {
                candidate.skill_feedback.clear();
                InstructionStateError::Feedback
            }
            _ => {
                candidate.invocation = InvocationId(2);
                InstructionStateError::Invocation
            }
        };
        assert_eq!(
            store.append(&input("local change"), Some(&candidate)).await,
            Err(StoreError::InstructionState(error)),
            "{kind}"
        );
        assert_eq!(store.load().await?, before, "{kind}");
        if matches!(
            kind,
            "drop source" | "drop skill" | "feedback text" | "drop feedback"
        ) {
            assert!(
                matches!(
                    store.append(&model(), Some(&candidate)).await,
                    Err(StoreError::InstructionState(_))
                ),
                "{kind}"
            );
            assert_eq!(store.load().await?, before, "{kind}");
        }
    }
    let mut changed = retained;
    changed.scoped_sources[0].content = InstructionContent::Text("Changed rules".into());
    changed.scoped_sources[0].delivery = InstructionDelivery::Pending;
    changed.selected_skills[0].text = "Changed skill".into();
    changed.selected_skills[0].delivery = InstructionDelivery::Pending;
    store
        .append(&input("pending replacement"), Some(&changed))
        .await?;
    assert_eq!(
        store.load().await?.instruction_state.as_ref(),
        Some(&changed)
    );
    Ok(())
}

#[tokio::test]
async fn feedback_requires_same_store_completed_cells_with_unique_targets()
-> Result<(), Box<dyn Error>> {
    let store = InMemoryStore::with_instruction_state(limits())?;
    let foreign = InMemoryStore::new(limits())?;
    let foreign_id = foreign.append(&cell(), None).await?;
    let input_id = store.append(&input("not a cell"), None).await?;
    let target = store.append(&cell(), None).await?;
    assert_ne!(foreign_id, input_id);
    assert_ne!(foreign_id, target);
    let mut future = cell();
    future.invocation = InvocationId(2);
    let future_id = store.append(&future, None).await?;
    let before = store.load().await?;
    for record in [
        foreign_id,
        input_id,
        future_id,
        RecordId::try_from(i64::MAX)?,
    ] {
        let mut candidate = instructions();
        candidate.scoped_feedback.push(InstructionFeedback {
            record,
            text: "note".into(),
        });
        assert_eq!(
            store.append(&model(), Some(&candidate)).await,
            Err(StoreError::InstructionState(
                InstructionStateError::FeedbackTarget { record }
            ))
        );
        assert_eq!(store.load().await?, before);
    }
    let mut candidate = instructions();
    candidate.skill_feedback = vec![
        InstructionFeedback {
            record: target,
            text: "note".into()
        };
        2
    ];
    assert_eq!(
        store.append(&model(), Some(&candidate)).await,
        Err(StoreError::InstructionState(
            InstructionStateError::Feedback
        ))
    );
    assert_eq!(store.load().await?, before);
    candidate.skill_feedback.pop();
    store.append(&model(), Some(&candidate)).await?;
    assert_eq!(
        store.load().await?.instruction_state.as_ref(),
        Some(&candidate)
    );
    Ok(())
}

#[tokio::test]
async fn instruction_paths_and_collections_are_bounded_before_acceptance()
-> Result<(), Box<dyn Error>> {
    let store = InMemoryStore::new(limits())?;
    let before = store.load().await?;
    let mut paths = vec![
        "relative/AGENTS.md".into(),
        "/workspace/../AGENTS.md".into(),
        "/workspace/\0/AGENTS.md".into(),
        format!("/{}", "a".repeat(4096)).into(),
        format!("/{}AGENTS.md", "a/".repeat(256)).into(),
    ];
    #[cfg(unix)]
    {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt, path::PathBuf};
        paths.push(PathBuf::from(OsString::from_vec(
            b"/workspace/\xff".to_vec(),
        )));
    }
    for path in paths {
        for selected in [false, true] {
            let mut candidate = instructions();
            if selected {
                candidate.selected_skills[0].source_path = path.clone();
            } else {
                candidate.scoped_sources[0].source_path = path.clone();
            }
            assert_eq!(
                store.append(&input("invalid path"), Some(&candidate)).await,
                Err(StoreError::InstructionState(
                    InstructionStateError::SourcePath
                ))
            );
            assert_eq!(store.load().await?, before);
        }
    }
    for kind in [
        "source count",
        "skill count",
        "source bytes",
        "skill bytes",
        "sources total",
        "skills total",
        "source duplicate",
        "skill duplicate",
        "feedback bytes",
    ] {
        let mut candidate = instructions();
        let error = match kind {
            "source count" => {
                candidate.scoped_sources = vec![candidate.scoped_sources[0].clone(); 257];
                InstructionStateError::Limit
            }
            "skill count" => {
                candidate.selected_skills = vec![candidate.selected_skills[0].clone(); 65];
                InstructionStateError::Limit
            }
            "source bytes" => {
                candidate.scoped_sources[0].content =
                    InstructionContent::Text("a".repeat(64 * 1024 + 1));
                InstructionStateError::Limit
            }
            "skill bytes" => {
                candidate.selected_skills[0].text = "a".repeat(64 * 1024 + 1);
                InstructionStateError::Limit
            }
            "sources total" => {
                candidate.scoped_sources = (0..3)
                    .map(|index| ScopedInstruction {
                        source_path: format!("/workspace/{index}/AGENTS.md").into(),
                        content: InstructionContent::Text("a".repeat(64 * 1024)),
                        delivery: InstructionDelivery::Pending,
                    })
                    .collect();
                InstructionStateError::Limit
            }
            "skills total" => {
                candidate.selected_skills = (0..3)
                    .map(|index| SelectedSkill {
                        source_path: format!("/workspace/{index}/SKILL.md").into(),
                        text: "a".repeat(64 * 1024),
                        delivery: InstructionDelivery::Pending,
                    })
                    .collect();
                InstructionStateError::Limit
            }
            "source duplicate" => {
                candidate
                    .scoped_sources
                    .push(candidate.scoped_sources[0].clone());
                InstructionStateError::DuplicateSource
            }
            "skill duplicate" => {
                candidate
                    .selected_skills
                    .push(candidate.selected_skills[0].clone());
                InstructionStateError::DuplicateSource
            }
            _ => {
                candidate.skill_feedback.push(InstructionFeedback {
                    record: RecordId::try_from(1)?,
                    text: "a".repeat(128 * 1024),
                });
                InstructionStateError::Limit
            }
        };
        assert_eq!(
            store
                .append(&input("invalid bounds"), Some(&candidate))
                .await,
            Err(StoreError::InstructionState(error)),
            "{kind}"
        );
        assert_eq!(store.load().await?, before, "{kind}");
    }
    let mut boundary = instructions();
    boundary.scoped_sources = (0..256)
        .map(|index| ScopedInstruction {
            source_path: format!("/workspace/{index}/AGENTS.md").into(),
            content: if index < 2 {
                InstructionContent::Text("a".repeat(64 * 1024))
            } else {
                InstructionContent::Removed
            },
            delivery: InstructionDelivery::Pending,
        })
        .collect();
    boundary.selected_skills = (0..64)
        .map(|index| SelectedSkill {
            source_path: format!("/workspace/{index}/SKILL.md").into(),
            text: if index < 2 {
                "a".repeat(64 * 1024)
            } else {
                String::new()
            },
            delivery: InstructionDelivery::Pending,
        })
        .collect();
    store
        .append(&input("exact collection boundaries"), Some(&boundary))
        .await?;
    assert_eq!(
        store.load().await?.instruction_state.as_ref(),
        Some(&boundary)
    );
    Ok(())
}

fn limits() -> StoreLimits {
    StoreLimits { record_bytes: 4096 }
}

fn cell() -> Record {
    Record {
        invocation: InvocationId(1),
        kind: RecordKind::CellCompleted {
            model: match cocoa_core::store::RecordId::try_from(1) {
                Ok(id) => id,
                Err(_) => unreachable!(),
            },
            cell: 0,
            code: "read rules".into(),
            outcome: CellResponse::Completed {
                printed: vec![serde_json::json!(7)],
            },
        },
    }
}

fn model() -> Record {
    Record {
        invocation: InvocationId(1),
        kind: RecordKind::ModelCompleted {
            shared_context: None,
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

fn instructions() -> InstructionState {
    InstructionState {
        invocation: InvocationId(1),
        scoped_sources: vec![
            ScopedInstruction {
                source_path: "/workspace/src/AGENTS.md".into(),
                content: InstructionContent::Text("Use the existing tests".into()),
                delivery: InstructionDelivery::Pending,
            },
            ScopedInstruction {
                source_path: "/workspace/removed/AGENTS.md".into(),
                content: InstructionContent::Removed,
                delivery: InstructionDelivery::Pending,
            },
        ],
        selected_skills: vec![
            SelectedSkill {
                source_path: "/skills/research/SKILL.md".into(),
                text: "Read primary sources".into(),
                delivery: InstructionDelivery::Pending,
            },
            SelectedSkill {
                source_path: "/skills/review/SKILL.md".into(),
                text: "Check failure paths".into(),
                delivery: InstructionDelivery::Pending,
            },
        ],
        scoped_feedback: Vec::new(),
        skill_feedback: Vec::new(),
    }
}
