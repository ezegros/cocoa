//! Checkpoint provenance, atomic transitions and summary protocol contracts.

use std::{error::Error, time::Duration};

use cocoa_core::{
    capability::{Effects, OperationOutcome},
    compaction::{
        Checkpoint, CheckpointError, CheckpointLimits, CheckpointProposal, CompactionPolicy,
        EvidenceNote, PlanStage, Quotation, Recap, RecapStatus, RequestState, RequiredAddition,
        RequiredChange, RequiredCorrection, RequiredOrigin, RequiredTransition, SummaryAttempt,
    },
    context::{BoundedContext, ContextAssembler, InstructionFeedback, InstructionState},
    execution::{Cell, CellResponse},
    model::{Image, ModelAction, ModelResponse, UserInput},
    store::{
        ActiveSnapshot, InMemoryStore, InvocationId, OperationId, Record, RecordId, RecordKind,
        Store, StoreError, StoreLimits, StoreSnapshot,
    },
};
use serde_json::{Value, json};

#[test]
fn shared_context_accepts_unknown_fields() -> Result<(), Box<dyn Error>> {
    let known = json!({
        "authority": "agent_authored",
        "records": [{
            "key": "review", "type": "observation", "value": {"finding": "original"},
            "labels": [], "owner": "7"
        }],
        "omitted": 2
    });
    let expected: cocoa_core::context::SharedContext = serde_json::from_value(known.clone())?;
    let mut extended = known;
    extended["unknown"] = json!({"future": true});
    extended["records"][0]["unknown"] = json!([1, 2]);
    let actual: cocoa_core::context::SharedContext = serde_json::from_value(extended)?;
    assert_eq!(actual, expected);
    Ok(())
}

#[tokio::test]
async fn shared_deliveries_are_summary_evidence_and_return_after_compaction()
-> Result<(), Box<dyn Error>> {
    let store = store()?;
    append(
        &store,
        1,
        RecordKind::InputAccepted {
            input: "Review".into(),
        },
    )
    .await?;
    let shared: cocoa_core::context::SharedContext = serde_json::from_value(json!({
        "authority":"agent_authored",
        "records":[{"key":"review","type":"observation","value":"original finding","labels":[],"owner":"7"}],
        "omitted":2,
    }))?;
    let mut answer = final_answer("Reviewed");
    let RecordKind::ModelCompleted { shared_context, .. } = &mut answer else {
        return Err("expected model response".into());
    };
    *shared_context = Some(shared.clone());
    let through = append(&store, 1, answer).await?;
    append(
        &store,
        2,
        RecordKind::InputAccepted {
            input: "Continue".into(),
        },
    )
    .await?;
    let snapshot = store.load().await?;
    let attempt = SummaryAttempt::prepare(&snapshot, through, policy())?;
    let summary: Value = serde_json::from_str(&attempt.request().input)?;
    assert_eq!(
        summary["sources"][1]["shared_context"],
        serde_json::to_value(&shared)?
    );
    let checkpoint =
        Checkpoint::prepare(&snapshot, through, empty_proposal(), checkpoint_limits())?;
    let mut context = BoundedContext
        .prepare(snapshot.records.clone(), Vec::new(), 65536)
        .await?
        .context;
    context.shared = Some(shared.clone());
    assert!(context.shared_update().is_none());
    let retained_bytes = context.encoded_size()?;
    context.shared.as_mut().ok_or("missing snapshot")?.records[0]
        .content
        .value = json!("new finding");
    assert_eq!(
        context.shared_update().ok_or("missing update")?.records[0]
            .content
            .value,
        "new finding"
    );
    assert!(context.encoded_size()? > retained_bytes);
    let mut compacted = BoundedContext
        .prepare_compacted(snapshot.records, Vec::new(), 65536, &checkpoint)
        .await?
        .context;
    compacted.shared = Some(shared.clone());
    assert_eq!(compacted.shared_update(), Some(&shared));
    Ok(())
}

#[tokio::test]
async fn summary_json_is_complete_and_bound_to_one_attempt() -> Result<(), Box<dyn Error>> {
    let store = store()?;
    let user = append(
        &store,
        1,
        RecordKind::InputAccepted {
            input: "Never publish.".into(),
        },
    )
    .await?;
    let through = append(&store, 1, final_answer("Working on the plan.")).await?;
    let snapshot = store.load().await?;
    for target_bytes in [8_192, 32_768] {
        let attempt = SummaryAttempt::prepare(
            &snapshot,
            through,
            CompactionPolicy {
                target_bytes,
                ..policy()
            },
        )?;
        let input: Value = serde_json::from_str(&attempt.request().input)?;
        assert_eq!(input["target_request_bytes"], target_bytes);
    }
    for invalid in [
        "duplicate_root",
        "duplicate_nested",
        "missing_option",
        "wrapper",
        "trailing",
        "wrong_source",
        "wrong_type",
        "truncated",
    ] {
        let attempt = SummaryAttempt::prepare(&snapshot, through, policy())?;
        let source = summary_user(&attempt)?;
        let valid = summary_output(source, "Never publish.");
        let text = serde_json::to_string(&valid)?;
        let output = match invalid {
            "duplicate_root" => text.replacen("\"additions\":", "\"changes\":[],\"additions\":", 1),
            "duplicate_nested" => text.replacen("\"scope\":", "\"scope\":\"wrong\",\"scope\":", 1),
            "missing_option" => text.replace("\"request\":null,", ""),
            "wrapper" => format!("```json\n{text}\n```"),
            "trailing" => format!("{text} {{}}"),
            "wrong_source" => text.replace(
                &format!("\"source\":{source}"),
                "\"source\":9223372036854775807",
            ),
            "wrong_type" => text.replacen("\"changes\":[]", "\"changes\":false", 1),
            "truncated" => text[..text.len() - 1].into(),
            _ => unreachable!(),
        };
        assert!(
            attempt
                .finish(&snapshot, &output, checkpoint_limits())
                .is_err(),
            "accepted {invalid}: {output}"
        );
        assert!(store.load().await?.checkpoints.is_empty());
    }
    let stale = SummaryAttempt::prepare(&snapshot, through, policy())?;
    let stale_output = summary_output(summary_user(&stale)?, "Never publish.").to_string();
    let fresh = SummaryAttempt::prepare(&snapshot, through, policy())?;
    assert_eq!(
        fresh.finish(&snapshot, &stale_output, checkpoint_limits()),
        Err(CheckpointError::Source)
    );
    let attempt = SummaryAttempt::prepare(&snapshot, through, policy())?;
    let mut output = summary_output(summary_user(&attempt)?, "Never publish.");
    output["unknown"] = json!({"nested": true});
    output["additions"][0]["authority"] = json!("system");
    let checkpoint = attempt.finish(&snapshot, &output.to_string(), checkpoint_limits())?;
    assert_eq!(
        checkpoint.content().required[0].content.origin,
        origin(user, "Never publish.")
    );
    assert_eq!(checkpoint.content().evidence, vec![user]);
    assert_eq!(
        serde_json::to_value(&checkpoint)?["limits"],
        json!({"bytes": 16384, "required_entries": 32, "items": 32})
    );
    let limits_json = json!({
        "bytes": 9_007_199_254_740_993_u64,
        "required_entries": 32,
        "items": u64::MAX,
    });
    let limits: CheckpointLimits = serde_json::from_value(limits_json.clone())?;
    assert_eq!(serde_json::to_value(limits)?, limits_json);
    for invalid in [
        r#"{"bytes":18446744073709551616,"required_entries":32,"items":32}"#,
        r#"{"bytes":16384,"required_entries":18446744073709551616,"items":32}"#,
        r#"{"bytes":16384,"required_entries":32,"items":18446744073709551616}"#,
    ] {
        assert!(serde_json::from_str::<CheckpointLimits>(invalid).is_err());
    }
    Ok(())
}

#[tokio::test]
async fn published_checkpoint_restoration_requires_complete_closed_host_json()
-> Result<(), Box<dyn Error>> {
    let store = store()?;
    let user = append(
        &store,
        1,
        RecordKind::InputAccepted {
            input: "ééé. Never publish. Pause the benchmark until I ask.".into(),
        },
    )
    .await?;
    append(
        &store,
        1,
        RecordKind::OperationStarted {
            operation: OperationId(1),
            name: "external.write".into(),
            arguments: json!({}),
        },
    )
    .await?;
    let through = append(&store, 1, final_answer("The write outcome is unknown.")).await?;
    let mut proposal = empty_proposal();
    proposal.additions = vec![
        addition(user, "éé", None),
        addition(
            user,
            "Pause the benchmark until I ask.",
            Some(RequestState::Paused {
                instruction: origin(user, "Pause the benchmark until I ask."),
                resume_when: "The user asks for the benchmark".into(),
            }),
        ),
    ];
    proposal.recap.stages.push(PlanStage {
        key: "write".into(),
        text: "Wait for the write outcome".into(),
        status: RecapStatus::Unknown,
        prerequisites: vec![],
        sources: vec![through],
        revisit_when: None,
    });
    proposal
        .recap
        .progress
        .push(note(through, "Write attempted"));
    proposal.evidence.push(user);
    let checkpoint =
        Checkpoint::prepare(&store.load().await?, through, proposal, checkpoint_limits())?;
    store.publish_checkpoint(&checkpoint).await?;
    let published = store.load().await?;
    let mut snapshot = published.clone();
    snapshot.checkpoints.clear();
    let encoded = serde_json::to_string(&published.checkpoints[0].checkpoint)?;
    let restored = Checkpoint::restore(&snapshot, &encoded)?;
    assert_eq!(restored, checkpoint);
    assert_eq!(restored.after(), through);
    assert_eq!(serde_json::to_string(&restored)?, encoded);

    let value = serde_json::to_value(&checkpoint)?;
    let text = value.to_string();
    assert_eq!(
        Checkpoint::restore(&snapshot, &format!("\n {text} \t"))?,
        checkpoint
    );
    for path in [
        "",
        "/limits",
        "/content",
        "/content/required/0",
        "/content/required/0/content",
        "/content/required/0/content/origin",
        "/content/required/0/content/origin/User",
        "/content/required/0/content/origin/User/quotations/0",
        "/content/required/1/content/request",
        "/content/required/1/content/request/Paused",
        "/content/required/1/content/request/Paused/instruction",
        "/content/required/1/content/request/Paused/instruction/User",
        "/content/recap",
        "/content/recap/stages/0",
        "/content/recap/progress/0",
        "/content/unresolved_effects/0",
    ] {
        let object = value
            .pointer(path)
            .and_then(Value::as_object)
            .ok_or("missing encoded object")?;
        let mut unknown = value.clone();
        unknown
            .pointer_mut(path)
            .and_then(Value::as_object_mut)
            .ok_or("missing encoded object")?
            .insert("unknown".into(), Value::Null);
        assert_eq!(
            Checkpoint::restore(&snapshot, &unknown.to_string()),
            Err(CheckpointError::Encoding),
            "accepted unknown field at {path}"
        );
        for (field, field_value) in object {
            let mut missing = value.clone();
            missing
                .pointer_mut(path)
                .and_then(Value::as_object_mut)
                .ok_or("missing encoded object")?
                .remove(field);
            assert_eq!(
                Checkpoint::restore(&snapshot, &missing.to_string()),
                Err(CheckpointError::Encoding),
                "accepted missing {path}/{field}"
            );
            let object_text = serde_json::to_string(object)?;
            let duplicate = object_text.replacen(
                '{',
                &format!("{{{}:{field_value},", serde_json::to_string(field)?),
                1,
            );
            let duplicate = text.replacen(&object_text, &duplicate, 1);
            assert_eq!(
                Checkpoint::restore(&snapshot, &duplicate),
                Err(CheckpointError::Encoding),
                "accepted duplicate {path}/{field}"
            );
        }
    }
    for malformed in [
        "null".into(),
        "[]".into(),
        json!([
            value["content"],
            value["after"],
            value["changes"],
            value["entry_last"],
            value["limits"],
        ])
        .to_string(),
        format!("```json\n{encoded}\n```"),
        format!("{encoded} {{}}"),
        encoded[..encoded.len() - 1].into(),
        encoded.replacen("\"changes\":[]", "\"changes\":false", 1),
        encoded.replacen("\"entry_last\":2", "\"entry_last\":18446744073709551616", 1),
    ] {
        assert_eq!(
            Checkpoint::restore(&snapshot, &malformed),
            Err(CheckpointError::Encoding),
            "accepted malformed publication: {malformed}"
        );
    }
    for (path, replacement, error) in [
        ("/limits/bytes", json!(0), CheckpointError::Limits),
        ("/limits/bytes", json!(1), CheckpointError::Limits),
        (
            "/limits/required_entries",
            json!(0),
            CheckpointError::Limits,
        ),
        (
            "/limits/required_entries",
            json!(1),
            CheckpointError::Limits,
        ),
        ("/limits/items", json!(0), CheckpointError::Limits),
        ("/limits/items", json!(-1), CheckpointError::Encoding),
        (
            "/limits",
            json!([16_384, 32, 32]),
            CheckpointError::Encoding,
        ),
        (
            "/content/required/0",
            json!([
                value["content"]["required"][0]["id"],
                value["content"]["required"][0]["content"],
            ]),
            CheckpointError::Encoding,
        ),
        (
            "/content/unresolved_effects/0/effects",
            json!({"Unknown": null}),
            CheckpointError::Encoding,
        ),
        (
            "/content/required/0/content/origin/User/quotations/0/text",
            json!("Invented instruction"),
            CheckpointError::Quotation,
        ),
        (
            "/content/required/0/content/origin/User/quotations/0/text",
            json!("éé"),
            CheckpointError::Lineage,
        ),
        (
            "/content/required/0/content/origin/User/quotations/0/source",
            json!(through),
            CheckpointError::Source,
        ),
        ("/content/required/0/id", json!(9), CheckpointError::Lineage),
        ("/entry_last", json!(0), CheckpointError::Lineage),
        (
            "/content/unresolved_effects",
            json!([]),
            CheckpointError::Lineage,
        ),
        (
            "/content/predecessor",
            json!(through),
            CheckpointError::Lineage,
        ),
        ("/after", json!(user), CheckpointError::Lineage),
        ("/after", json!(i64::MAX), CheckpointError::Lineage),
        (
            "/content/through",
            json!(i64::MAX),
            CheckpointError::Boundary,
        ),
        (
            "/content/evidence",
            json!([user, user]),
            CheckpointError::Source,
        ),
        ("/content/recap/goal", json!(" "), CheckpointError::Empty),
    ] {
        let mut invalid = value.clone();
        *invalid.pointer_mut(path).ok_or("missing encoded field")? = replacement;
        assert_eq!(
            Checkpoint::restore(&snapshot, &invalid.to_string()),
            Err(error),
            "accepted invalid {path}"
        );
    }
    assert!(snapshot.checkpoints.is_empty());
    assert_eq!(snapshot.records, published.records);
    assert_eq!(store.load().await?, published);
    Ok(())
}

#[tokio::test]
async fn historical_restoration_preserves_lineage_identities_boundaries_and_u64_limits()
-> Result<(), Box<dyn Error>> {
    let store = store()?;
    let user = append(
        &store,
        1,
        RecordKind::InputAccepted {
            input: "Never publish. Build the parser.".into(),
        },
    )
    .await?;
    let first_after = append(&store, 1, final_answer("Parser started.")).await?;
    let mut proposal = empty_proposal();
    proposal.additions = vec![
        addition(user, "Never publish.", None),
        addition(user, "Build the parser.", Some(RequestState::Pending)),
    ];
    let first = Checkpoint::prepare(&store.load().await?, user, proposal, checkpoint_limits())?;
    store.publish_checkpoint(&first).await?;
    let kept = first.content().required[0].clone();
    let retired = first.content().required[1].id;

    let outcome = append(&store, 1, final_answer("Parser implemented and verified.")).await?;
    let tests = append(
        &store,
        2,
        RecordKind::InputAccepted {
            input: "Add tests.".into(),
        },
    )
    .await?;
    let second_after = append(&store, 2, final_answer("Tests are next.")).await?;
    let mut proposal = empty_proposal();
    proposal.changes.push(RequiredChange {
        entry: retired,
        correction: None,
        transition: Some(RequiredTransition::Complete {
            outcome: note(outcome, "Parser implemented"),
            verification: note(outcome, "Parser verified"),
        }),
    });
    proposal
        .additions
        .push(addition(tests, "Add tests.", Some(RequestState::Pending)));
    let second = Checkpoint::prepare(&store.load().await?, tests, proposal, checkpoint_limits())?;
    assert_eq!(second.content().required[0], kept);
    let tests_entry = second.content().required[1].clone();
    assert!(tests_entry.id > retired);
    store.publish_checkpoint(&second).await?;

    let fixtures = append(
        &store,
        3,
        RecordKind::InputAccepted {
            input: "Keep fixtures small.".into(),
        },
    )
    .await?;
    let third_after = append(&store, 3, final_answer("Using small fixtures.")).await?;
    let mut proposal = empty_proposal();
    proposal
        .additions
        .push(addition(fixtures, "Keep fixtures small.", None));
    let limits = CheckpointLimits {
        bytes: 9_007_199_254_740_993,
        required_entries: u64::MAX,
        items: u64::MAX,
    };
    let third = Checkpoint::prepare(&store.load().await?, fixtures, proposal, limits)?;
    assert_eq!(third.content().required[..2], [kept, tests_entry]);
    assert!(third.content().required[2].id > third.content().required[1].id);
    store.publish_checkpoint(&third).await?;
    let tail = append(
        &store,
        3,
        final_answer("Newer original after all publications."),
    )
    .await?;
    let published = store.load().await?;
    let mut snapshot = published.clone();
    snapshot.checkpoints.clear();
    for (index, (stored, after)) in published
        .checkpoints
        .iter()
        .zip([first_after, second_after, third_after])
        .enumerate()
    {
        let encoded = serde_json::to_string(&stored.checkpoint)?;
        let restored = Checkpoint::restore(&snapshot, &encoded)?;
        assert_eq!(restored, stored.checkpoint);
        assert_eq!(restored.after(), after);
        assert_ne!(restored.after(), tail);
        assert_eq!(serde_json::to_string(&restored)?, encoded);
        assert!(store.publish_checkpoint(&restored).await.is_err());
        if index == 1 {
            let value = serde_json::to_value(&stored.checkpoint)?;
            for (path, replacement, error) in [
                (
                    "/content/required/0/content/scope",
                    json!("Changed without a correction"),
                    CheckpointError::Lineage,
                ),
                (
                    "/content/required/1/id",
                    json!(retired),
                    CheckpointError::Lineage,
                ),
                ("/changes", json!([]), CheckpointError::Lineage),
                (
                    "/changes/0/transition/Complete/verification/sources",
                    json!([]),
                    CheckpointError::Empty,
                ),
                (
                    "/content/predecessor",
                    json!(user),
                    CheckpointError::Lineage,
                ),
                ("/after", json!(first_after), CheckpointError::Lineage),
                ("/content/through", json!(tail), CheckpointError::Lineage),
                ("/content/evidence", json!([tail]), CheckpointError::Source),
            ] {
                let mut invalid = value.clone();
                *invalid.pointer_mut(path).ok_or("missing encoded field")? = replacement;
                assert_eq!(
                    Checkpoint::restore(&snapshot, &invalid.to_string()),
                    Err(error),
                    "accepted invalid historical {path}"
                );
            }
            let mut missing_predecessor = snapshot.clone();
            missing_predecessor.checkpoints.clear();
            assert_eq!(
                Checkpoint::restore(&missing_predecessor, &encoded),
                Err(CheckpointError::Lineage)
            );
            let mut invalid_predecessor = snapshot.clone();
            invalid_predecessor.checkpoints[0].id = user;
            assert_eq!(
                Checkpoint::restore(&invalid_predecessor, &encoded),
                Err(CheckpointError::Lineage)
            );
        }
        if index == 2 {
            assert_eq!(
                serde_json::to_value(&restored)?["limits"],
                serde_json::to_value(limits)?
            );
        }
        let mut publication = stored.clone();
        publication.checkpoint = restored;
        snapshot.checkpoints.push(publication);
        assert_eq!(
            Checkpoint::restore(&snapshot, &encoded),
            Err(CheckpointError::Lineage)
        );
    }
    snapshot.validate_checkpoints()?;
    assert_eq!(snapshot, published);
    assert_eq!(store.load().await?, published);
    Ok(())
}

#[tokio::test]
async fn required_entries_carry_with_identity_and_changes_need_separate_support()
-> Result<(), Box<dyn Error>> {
    let store = store()?;
    let user = append(
        &store,
        1,
        RecordKind::InputAccepted {
            input: "Never publish. Build the parser. Pause the benchmark until I ask.".into(),
        },
    )
    .await?;
    let through = append(&store, 1, final_answer("Parser implemented, not verified.")).await?;
    let mut proposal = empty_proposal();
    proposal.additions = vec![
        addition(user, "Never publish.", None),
        addition(user, "Build the parser.", Some(RequestState::Pending)),
        addition(
            user,
            "Pause the benchmark until I ask.",
            Some(RequestState::Paused {
                instruction: origin(user, "Pause the benchmark until I ask."),
                resume_when: "A new benchmark request from the user".into(),
            }),
        ),
    ];
    let first = Checkpoint::prepare(&store.load().await?, through, proposal, checkpoint_limits())?;
    store.publish_checkpoint(&first).await?;
    let required = first.content().required.clone();
    let next = append(&store, 1, final_answer("A newer observation.")).await?;
    let carried = Checkpoint::prepare(
        &store.load().await?,
        next,
        empty_proposal(),
        checkpoint_limits(),
    )?;
    assert_eq!(carried.content().required, required);
    let snapshot = store.load().await?;
    let mut invalid = empty_proposal();
    invalid.changes.push(RequiredChange {
        entry: required[1].id,
        correction: None,
        transition: Some(RequiredTransition::Complete {
            outcome: note(through, "Implemented"),
            verification: EvidenceNote {
                text: "Tests passed".into(),
                sources: vec![],
            },
        }),
    });
    assert_eq!(
        Checkpoint::prepare(&snapshot, next, invalid, checkpoint_limits()),
        Err(CheckpointError::Empty)
    );
    let mut correction = empty_proposal();
    correction.changes.push(RequiredChange {
        entry: required[2].id,
        correction: Some(RequiredCorrection {
            origin: origin(user, "Pause the benchmark until I ask."),
            scope: "Only the benchmark".into(),
            justification: note(user, "Restore the original scope"),
        }),
        transition: None,
    });
    let corrected = Checkpoint::prepare(&snapshot, next, correction.clone(), checkpoint_limits())?;
    assert_eq!(corrected.content().required[2].id, required[2].id);
    assert_eq!(
        corrected.content().required[2].content.request,
        required[2].content.request
    );
    correction.changes.push(correction.changes[0].clone());
    assert_eq!(
        Checkpoint::prepare(&snapshot, next, correction, checkpoint_limits()),
        Err(CheckpointError::ConflictingChange)
    );
    assert_eq!(store.load().await?.checkpoints.len(), 1);
    Ok(())
}

#[tokio::test]
async fn quotes_keep_authorship_and_overlapping_unicode_expands_to_original()
-> Result<(), Box<dyn Error>> {
    let store = store()?;
    let user = append(
        &store,
        1,
        RecordKind::InputAccepted {
            input: "ééé. Never publish without approval.".into(),
        },
    )
    .await?;
    let assistant = append(
        &store,
        1,
        final_answer("Use a red-black tree, then benchmark."),
    )
    .await?;
    let accepted = append(
        &store,
        1,
        RecordKind::InputAccepted {
            input: "Yes to the tree, not the benchmark.".into(),
        },
    )
    .await?;
    let mut proposal = empty_proposal();
    proposal.additions.push(addition(user, "éé", None));
    proposal.additions.push(RequiredAddition {
        origin: RequiredOrigin::Adopted {
            wording: vec![Quotation {
                source: assistant,
                text: "Use a red-black tree".into(),
            }],
            acceptance: vec![Quotation {
                source: accepted,
                text: "Yes to the tree, not the benchmark.".into(),
            }],
        },
        scope: "Tree implementation only".into(),
        request: Some(RequestState::Pending),
    });
    let checkpoint = Checkpoint::prepare(
        &store.load().await?,
        accepted,
        proposal.clone(),
        checkpoint_limits(),
    )?;
    assert_eq!(
        checkpoint.content().required[0].content.origin,
        origin(user, "ééé. Never publish without approval.")
    );
    if let RequiredOrigin::Adopted { acceptance, .. } = &mut proposal.additions[1].origin {
        acceptance[0].source = assistant;
    }
    assert_eq!(
        Checkpoint::prepare(
            &store.load().await?,
            accepted,
            proposal,
            checkpoint_limits()
        ),
        Err(CheckpointError::Source)
    );
    Ok(())
}

#[tokio::test]
async fn projection_keeps_captured_images_current_inputs_and_unknown_effects()
-> Result<(), Box<dyn Error>> {
    let store = store()?;
    let image = Image::from_png(b"\x89PNG\r\n\x1a\ncaptured")?;
    let image_input = UserInput::WithImages {
        text: "Captured evidence".into(),
        images: vec![image.clone()],
    };
    append(
        &store,
        1,
        RecordKind::InputAccepted {
            input: image_input.clone(),
        },
    )
    .await?;
    let start = append(
        &store,
        1,
        RecordKind::OperationStarted {
            operation: OperationId(1),
            name: "external.write".into(),
            arguments: json!({"value":17}),
        },
    )
    .await?;
    let through = append(&store, 1, final_answer("The write outcome is unknown.")).await?;
    let first = Checkpoint::prepare(
        &store.load().await?,
        through,
        empty_proposal(),
        checkpoint_limits(),
    )?;
    assert_eq!(first.content().unresolved_effects[0].start, start);
    assert_eq!(
        first.content().unresolved_effects[0].effects,
        Effects::Unknown
    );
    store.publish_checkpoint(&first).await?;
    append(
        &store,
        1,
        RecordKind::OperationFinished {
            operation: OperationId(1),
            outcome: OperationOutcome::Completed { value: json!(17) },
        },
    )
    .await?;
    append(
        &store,
        2,
        RecordKind::InputAccepted {
            input: "Continue the parser".into(),
        },
    )
    .await?;
    append(
        &store,
        2,
        RecordKind::InputAccepted {
            input: "Steering: do not run the benchmark".into(),
        },
    )
    .await?;
    let snapshot = store.load().await?;
    let prepared = BoundedContext
        .prepare_compacted(snapshot.records.clone(), vec![], 64 * 1024, &first)
        .await?;
    assert!(
        prepared
            .context
            .checkpoint
            .as_ref()
            .is_some_and(|checkpoint| checkpoint.unresolved_effects.is_empty())
    );
    for text in [
        "Captured evidence",
        "Continue the parser",
        "Steering: do not run the benchmark",
    ] {
        assert_eq!(prepared.context.records.iter().filter(|record| matches!(&record.kind, RecordKind::InputAccepted { input } if input.text() == text)).count(), 1);
    }
    assert!(prepared.context.records.iter().any(|record| matches!(&record.kind, RecordKind::InputAccepted { input } if input.images() == [image.clone()])));
    assert_eq!(store.load().await?.records, snapshot.records);
    assert_eq!(
        store.load().await?.instruction_state,
        snapshot.instruction_state
    );
    Ok(())
}

#[tokio::test]
async fn summary_gathering_and_plan_validation_fail_without_partial_publication()
-> Result<(), Box<dyn Error>> {
    let store = store()?;
    append(
        &store,
        1,
        RecordKind::InputAccepted {
            input: "Preserve the full plan".into(),
        },
    )
    .await?;
    let through = append(&store, 1, final_answer(&"large evidence ".repeat(100))).await?;
    let snapshot = store.load().await?;
    let mut bounded = policy();
    bounded.summary_input_bytes = 100;
    assert!(matches!(
        SummaryAttempt::prepare(&snapshot, through, bounded),
        Err(CheckpointError::Limits)
    ));
    bounded = policy();
    bounded.sources = 1;
    assert!(matches!(
        SummaryAttempt::prepare(&snapshot, through, bounded),
        Err(CheckpointError::Limits)
    ));
    let mut cyclic = empty_proposal();
    cyclic.recap.stages = vec![stage("first", "second"), stage("second", "first")];
    assert_eq!(
        Checkpoint::prepare(&snapshot, through, cyclic, checkpoint_limits()),
        Err(CheckpointError::Plan)
    );
    assert!(store.load().await?.checkpoints.is_empty());
    Ok(())
}

#[tokio::test]
async fn active_reads_keep_sparse_sources_images_feedback_and_complete_suffix()
-> Result<(), Box<dyn Error>> {
    let store = store()?;
    let image = Image::from_png(b"\x89PNG\r\n\x1a\ncaptured")?;
    let image_record = append(
        &store,
        1,
        RecordKind::InputAccepted {
            input: UserInput::WithImages {
                text: "Captured evidence".into(),
                images: vec![image.clone()],
            },
        },
    )
    .await?;
    let user = append(
        &store,
        2,
        RecordKind::InputAccepted {
            input: "Never publish.".into(),
        },
    )
    .await?;
    let model = append(
        &store,
        2,
        RecordKind::ModelCompleted {
            shared_context: None,
            response: ModelResponse {
                action: ModelAction::Execute {
                    cells: vec![Cell { code: "17".into() }, Cell { code: "18".into() }],
                },
                continuation: None,
                usage: None,
            },
        },
    )
    .await?;
    let completed = append(
        &store,
        2,
        RecordKind::CellCompleted {
            model,
            cell: 0,
            code: "17".into(),
            outcome: CellResponse::Completed {
                printed: vec![json!(17)],
            },
        },
    )
    .await?;
    let feedback_target = append(
        &store,
        2,
        RecordKind::CellCompleted {
            model,
            cell: 1,
            code: "18".into(),
            outcome: CellResponse::Completed {
                printed: vec![json!(18)],
            },
        },
    )
    .await?;
    let mut starts = Vec::new();
    for operation in 1..=3 {
        starts.push(
            append(
                &store,
                2,
                RecordKind::OperationStarted {
                    operation: OperationId(operation),
                    name: "external.write".into(),
                    arguments: json!({"value":17}),
                },
            )
            .await?,
        );
        if operation != 2 {
            append(
                &store,
                2,
                RecordKind::OperationFinished {
                    operation: OperationId(operation),
                    outcome: if operation == 1 {
                        OperationOutcome::Completed { value: json!(17) }
                    } else {
                        OperationOutcome::FailedOmitted {
                            effects: Effects::Partial,
                            output_bytes: 100_000,
                        }
                    },
                },
            )
            .await?;
        }
    }
    let through = append(&store, 2, final_answer(&"retired detail ".repeat(200))).await?;
    let mut proposal = empty_proposal();
    proposal
        .additions
        .push(addition(user, "Never publish.", None));
    proposal.evidence = vec![completed, starts[0]];
    let first = Checkpoint::prepare(&store.load().await?, through, proposal, checkpoint_limits())?;
    store.publish_checkpoint(&first).await?;
    append(
        &store,
        2,
        RecordKind::OperationFinished {
            operation: OperationId(2),
            outcome: OperationOutcome::Completed { value: json!(17) },
        },
    )
    .await?;
    let current_input = append(
        &store,
        3,
        RecordKind::InputAccepted {
            input: "Continue without publication".into(),
        },
    )
    .await?;
    let instruction_state = InstructionState {
        invocation: InvocationId(3),
        scoped_sources: vec![],
        selected_skills: vec![],
        scoped_feedback: vec![InstructionFeedback {
            record: feedback_target,
            text: "Original instruction feedback".into(),
        }],
        skill_feedback: vec![],
    };
    let next = store
        .append(
            &Record {
                invocation: InvocationId(3),
                kind: final_answer("Continuing the parser"),
            },
            Some(&instruction_state),
        )
        .await?;
    let full = store.load().await?;
    let active = store.load_active().await?;
    assert!(active.records().len() < full.records.len());
    assert!(!active.records().iter().any(|record| record.id == model));
    assert!(active.records().iter().any(|record| record.id == completed));
    assert!(
        active
            .records()
            .iter()
            .any(|record| record.id == feedback_target)
    );
    assert!(
        active
            .records()
            .iter()
            .any(|record| record.id == image_record)
    );
    assert!(
        active
            .records()
            .iter()
            .any(|record| record.id == current_input)
    );
    assert_eq!(active.instruction_state(), Some(&instruction_state));
    let cut = full
        .records
        .iter()
        .position(|record| record.id == through)
        .ok_or("missing cut")?;
    assert_eq!(active.suffix(), &full.records[cut + 1..]);
    let effects = active.unresolved_effects()?;
    assert_eq!(effects.len(), 1);
    assert_eq!(effects[0].start, starts[2]);
    assert_eq!(effects[0].effects, Effects::Partial);
    assert!(effects[0].outcome.is_some());

    let full_projection = BoundedContext
        .prepare_compacted(full.records.clone(), vec![], 64 * 1024, &first)
        .await?;
    let active_projection = BoundedContext
        .prepare_compacted(active.records().to_vec(), vec![], 64 * 1024, &first)
        .await?;
    assert_eq!(
        active_projection.context.records,
        full_projection.context.records
    );
    assert_eq!(
        active_projection.context.checkpoint,
        full_projection.context.checkpoint
    );
    assert!(active_projection.context.records.iter().any(|record| matches!(
        &record.kind, RecordKind::InputAccepted { input } if input.images() == [image.clone()]
    )));

    let mut proposal = empty_proposal();
    proposal
        .recap
        .progress
        .push(note(completed, "Original result retained"));
    let second = Checkpoint::prepare_active(&active, next, proposal.clone(), checkpoint_limits())?;
    assert_eq!(
        second,
        Checkpoint::prepare(&full, next, proposal, checkpoint_limits())?
    );
    let attempt = SummaryAttempt::prepare_active(&active, next, policy())?;
    let output = serde_json::to_string(&empty_proposal())?;
    let from_summary = attempt.finish_active(&active, &output, checkpoint_limits())?;
    assert_eq!(
        from_summary,
        Checkpoint::prepare(&full, next, empty_proposal(), checkpoint_limits())?
    );
    store.publish_checkpoint(&second).await?;
    store.load().await?.validate_checkpoints()?;
    assert_eq!(
        store
            .load_active()
            .await?
            .checkpoint()
            .map(|stored| &stored.checkpoint),
        Some(&second)
    );

    let checkpoint = active.checkpoint().cloned();
    for missing in [completed, feedback_target, starts[1]] {
        let records = active
            .records()
            .iter()
            .filter(|record| record.id != missing)
            .cloned()
            .collect();
        let result =
            ActiveSnapshot::new(records, Some(instruction_state.clone()), checkpoint.clone());
        assert!(
            result.is_err(),
            "accepted missing required source {missing:?}"
        );
    }
    let records = active
        .records()
        .iter()
        .filter(|record| record.id != current_input)
        .cloned()
        .collect();
    assert_eq!(
        ActiveSnapshot::new(records, Some(instruction_state.clone()), checkpoint.clone()),
        Err(StoreError::InvalidRecord)
    );
    let sparse_full = StoreSnapshot {
        records: active.records().to_vec(),
        instruction_state: Some(instruction_state),
        checkpoints: checkpoint.into_iter().collect(),
    };
    assert!(sparse_full.validate_checkpoints().is_err());
    Ok(())
}

fn store() -> Result<InMemoryStore, Box<dyn Error>> {
    Ok(InMemoryStore::new(StoreLimits {
        record_bytes: 64 * 1024,
    })?)
}

async fn append(
    store: &InMemoryStore,
    invocation: u64,
    kind: RecordKind,
) -> Result<RecordId, Box<dyn Error>> {
    Ok(store
        .append(
            &Record {
                invocation: InvocationId(invocation),
                kind,
            },
            None,
        )
        .await?)
}

fn final_answer(answer: &str) -> RecordKind {
    RecordKind::ModelCompleted {
        shared_context: None,
        response: ModelResponse {
            action: ModelAction::Final {
                answer: answer.into(),
            },
            continuation: None,
            usage: None,
        },
    }
}

fn checkpoint_limits() -> CheckpointLimits {
    CheckpointLimits {
        bytes: 16 * 1024,
        required_entries: 32,
        items: 32,
    }
}

fn policy() -> CompactionPolicy {
    CompactionPolicy {
        request_bytes: 64 * 1024,
        trigger_bytes: 48 * 1024,
        target_bytes: 32 * 1024,
        summary_input_bytes: 64 * 1024,
        summary_output_tokens: 4096,
        sources: 64,
        checkpoint: checkpoint_limits(),
        timeout: Duration::from_secs(1),
    }
}

fn empty_proposal() -> CheckpointProposal {
    CheckpointProposal {
        additions: vec![],
        changes: vec![],
        recap: Recap {
            goal: "Source-backed automatic compaction".into(),
            stages: vec![],
            current_stage: None,
            decisions: vec![],
            progress: vec![],
            failures: vec![],
            next_actions: vec!["Continue the current task".into()],
        },
        evidence: vec![],
    }
}

fn origin(source: RecordId, text: &str) -> RequiredOrigin {
    RequiredOrigin::User {
        quotations: vec![Quotation {
            source,
            text: text.into(),
        }],
    }
}

fn addition(source: RecordId, text: &str, request: Option<RequestState>) -> RequiredAddition {
    RequiredAddition {
        origin: origin(source, text),
        scope: "This task".into(),
        request,
    }
}

fn note(source: RecordId, text: &str) -> EvidenceNote {
    EvidenceNote {
        text: text.into(),
        sources: vec![source],
    }
}

fn stage(key: &str, dependency: &str) -> PlanStage {
    PlanStage {
        key: key.into(),
        text: "Future classifier work".into(),
        status: RecapStatus::Deferred,
        prerequisites: vec![dependency.into()],
        sources: vec![],
        revisit_when: None,
    }
}

fn summary_user(attempt: &SummaryAttempt) -> Result<i64, Box<dyn Error>> {
    let input: Value = serde_json::from_str(&attempt.request().input)?;
    input["sources"]
        .as_array()
        .and_then(|sources| sources.iter().find(|source| source["kind"] == "user"))
        .and_then(|source| source["source"].as_i64())
        .ok_or_else(|| "missing summary source".into())
}

fn summary_output(source: i64, text: &str) -> Value {
    json!({"additions":[{"origin":{"User":{"quotations":[{"source":source,"text":text}]}},"scope":"This task","request":null}],"changes":[],"recap":{"goal":"Continue source-backed compaction","stages":[],"current_stage":null,"decisions":[],"progress":[],"failures":[],"next_actions":[]},"evidence":[source]})
}
