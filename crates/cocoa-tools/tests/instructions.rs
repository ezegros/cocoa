//! Instruction discovery, scope gates and context budgeting through public adapters.

use std::{
    collections::BTreeMap, error::Error, fs, os::unix::fs::symlink, path::Path, sync::Arc,
    time::Duration,
};

use cocoa_core::{
    capability::{CapabilityError, CapabilityHost, Effects},
    context::{Context, ContextAssembler, ContextError},
    execution::{CellResponse, WorkLimits},
    store::{InvocationId, Record, RecordId, RecordKind, RecordSequence, StoredRecord},
};
use cocoa_tools::{
    CodingCapabilities, CodingLimits, LocalConfiguration, LocalEnvironment, ProjectEnvironment,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn startup_order_home_boundary_and_turn_refresh() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let home = directory.path().join("home");
    let root = home.join("projects/repo");
    fs::create_dir_all(root.join("src"))?;
    fs::create_dir_all(home.join(".config/unrelated-app"))?;
    let unrelated_rules = home.join(".config/unrelated-app/AGENTS.md");
    let unrelated = b"UNRELATED RULES MUST STAY UNUSED\n";
    fs::write(&unrelated_rules, unrelated)?;
    fs::create_dir_all(home.join(".config/cocoa"))?;
    fs::write(home.join(".config/cocoa/AGENTS.md"), "GLOBAL RULE\n")?;
    fs::write(directory.path().join("AGENTS.md"), "ABOVE HOME\n")?;
    fs::write(home.join("AGENTS.md"), "HOME RULE\n")?;
    fs::write(home.join("projects/AGENTS.md"), "ANCESTOR RULE\n")?;
    fs::write(root.join("AGENTS.md"), "ROOT RULE\n")?;
    fs::write(root.join("AGENTS.override.md"), "OVERRIDE RULE\n")?;
    fs::write(root.join("CLAUDE.md"), "CLAUDE RULE\n")?;
    fs::write(root.join("src/AGENTS.md"), "NESTED RULE\n")?;
    fs::create_dir(root.join(".git"))?;
    let (environment, host) = project(&root, Some(&home)).await?;
    let context = assemble(&environment, vec![input(1)]).await?;
    let instructions = &context.instructions;
    let global = instructions
        .find("GLOBAL RULE")
        .ok_or("missing global rules")?;
    let ancestor = instructions
        .find("ANCESTOR RULE")
        .ok_or("missing ancestor rules")?;
    let root_position = instructions.find("ROOT RULE").ok_or("missing root rules")?;
    assert!(global < ancestor && ancestor < root_position);
    assert!(instructions.contains("<project-rules from="));
    for absent in [
        "ABOVE HOME",
        "HOME RULE",
        "OVERRIDE RULE",
        "CLAUDE RULE",
        "NESTED RULE",
        "UNRELATED RULES",
    ] {
        assert!(!instructions.contains(absent), "unexpected {absent}");
    }
    assert!(context.feedback_notes.is_empty());
    fs::write(root.join("AGENTS.md"), "ROOT UPDATED\n")?;
    assert_eq!(
        assemble(&environment, vec![input(1)]).await?.instructions,
        *instructions
    );
    let refreshed = assemble(&environment, vec![input(1), input(2)]).await?;
    assert!(refreshed.instructions.contains("ROOT UPDATED"));
    assert!(!refreshed.instructions.contains("ROOT RULE"));
    assert!(matches!(
        environment
            .prepare(stored(vec![input(2)])?, host.descriptions(), 1)
            .await,
        Err(ContextError::Limit)
    ));
    environment.close().await?;

    let (environment, host) = project(&home, Some(&home)).await?;
    let context = assemble(&environment, vec![input(1)]).await?;
    assert!(context.instructions.contains("HOME RULE"));
    assert!(!context.instructions.contains("ABOVE HOME"));
    call(&host, "cocoa.read", json!({"path":".config/cocoa/new.txt"})).await?;
    assert!(
        assemble(&environment, vec![input(1), feedback(1)])
            .await?
            .feedback_notes
            .is_empty()
    );
    environment.close().await?;

    let (environment, _) = project(&home.join(".config/cocoa"), Some(&home)).await?;
    let context = assemble(&environment, vec![input(1)]).await?;
    assert_eq!(context.instructions.matches("GLOBAL RULE").count(), 1);
    environment.close().await?;

    let (environment, _) = project(&root, None).await?;
    let context = assemble(&environment, vec![input(1)]).await?;
    assert!(context.instructions.contains("ROOT UPDATED"));
    assert!(!context.instructions.contains("ANCESTOR RULE"));
    assert!(!context.instructions.contains("GLOBAL RULE"));
    environment.close().await?;

    fs::remove_file(home.join(".config/cocoa/AGENTS.md"))?;
    let (environment, _) = project(&root, Some(&home)).await?;
    let context = assemble(&environment, vec![input(1)]).await?;
    assert!(context.instructions.contains("ROOT UPDATED"));
    assert!(!context.instructions.contains("GLOBAL RULE"));
    assert!(!context.instructions.contains("UNRELATED RULES"));
    environment.close().await?;
    assert_eq!(fs::read(&unrelated_rules)?, unrelated);
    Ok(())
}

#[tokio::test]
async fn scoped_reads_gate_writes_and_commands_until_feedback_delivery()
-> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join("repo");
    fs::create_dir_all(root.join("src/0deep"))?;
    fs::create_dir_all(root.join("commands"))?;
    fs::write(root.join("src/AGENTS.md"), "BROAD SCOPE\n")?;
    fs::write(root.join("src/0deep/AGENTS.md"), "NARROW SCOPE\n")?;
    fs::write(root.join("commands/AGENTS.md"), "COMMAND SCOPE\n")?;
    let (environment, host) = project(&root, Some(directory.path())).await?;
    assemble(&environment, vec![input(1)]).await?;
    let observation = call(
        &host,
        "cocoa.read",
        json!({"path":"src/0deep/missing/new.txt"}),
    )
    .await?;
    assert_eq!(observation["kind"], "missing");
    rejected(
        call(
            &host,
            "cocoa.write",
            json!({"file":observation["file"],"content":"changed"}),
        )
        .await,
        "instructions_pending",
    );
    assert!(!root.join("src/0deep/missing").exists());
    rejected(
        call(
            &host,
            "cocoa.run",
            json!({"working_directory":"commands","command":"printf bad > sentinel"}),
        )
        .await,
        "instructions_pending",
    );
    assert!(!root.join("commands/sentinel").exists());
    let unaffected = call(
        &host,
        "cocoa.run",
        json!({"command":"printf root > root-sentinel"}),
    )
    .await?;
    assert_eq!(unaffected["status"]["code"], 0);
    assert_eq!(fs::read_to_string(root.join("root-sentinel"))?, "root");

    let records = vec![input(1), feedback(1)];
    assert!(matches!(
        environment
            .prepare(stored(records.clone())?, Vec::new(), 1)
            .await,
        Err(ContextError::Limit)
    ));
    rejected(
        call(
            &host,
            "cocoa.write",
            json!({"file":observation["file"],"content":"still blocked"}),
        )
        .await,
        "instructions_pending",
    );
    let context = assemble(&environment, records.clone()).await?;
    assert!(!context.instructions.contains("BROAD SCOPE"));
    assert_eq!(context.feedback_notes.len(), 1);
    assert_eq!(context.feedback_notes[0].record, 1);
    let note = &context.feedback_notes[0].text;
    assert!(note.contains("COMMAND SCOPE"));
    assert!(
        note.find("BROAD SCOPE").ok_or("missing broad rules")?
            < note.find("NARROW SCOPE").ok_or("missing narrow rules")?
    );
    let written = call(
        &host,
        "cocoa.write",
        json!({"file":observation["file"],"content":"changed"}),
    )
    .await?;
    assert_eq!(
        fs::read_to_string(root.join("src/0deep/missing/new.txt"))?,
        "changed"
    );
    call(
        &host,
        "cocoa.edit",
        json!({"file":written["file"],"edits":[{"old_text":"changed","new_text":"edited"}]}),
    )
    .await?;
    assert_eq!(
        fs::read_to_string(root.join("src/0deep/missing/new.txt"))?,
        "edited"
    );
    call(
        &host,
        "cocoa.run",
        json!({"working_directory":"commands","command":"printf good > sentinel"}),
    )
    .await?;
    assert_eq!(fs::read_to_string(root.join("commands/sentinel"))?, "good");
    assert_eq!(
        assemble(&environment, records).await?.feedback_notes.len(),
        1
    );
    environment.close().await?;
    Ok(())
}

#[tokio::test]
async fn scoped_changes_and_deletions_are_delivered_on_later_turns() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    fs::create_dir(directory.path().join("src"))?;
    let rules = directory.path().join("src/AGENTS.md");
    fs::write(&rules, "ORIGINAL SCOPE\n")?;
    let (environment, host) = project(directory.path(), None).await?;
    let mut records = vec![input(1)];
    assemble(&environment, records.clone()).await?;
    let observation = call(&host, "cocoa.read", json!({"path":"src/new.txt"})).await?;
    records.push(feedback(1));
    let context = assemble(&environment, records.clone()).await?;
    assert!(context.feedback_notes[0].text.contains("ORIGINAL SCOPE"));

    fs::write(&rules, [0xff])?;
    call(&host, "cocoa.read", json!({"path":"src/new.txt"})).await?;
    fs::write(&rules, "ORIGINAL SCOPE\n")?;
    records.push(input(2));
    assemble(&environment, records.clone()).await?;
    call(
        &host,
        "cocoa.read",
        json!({"file":observation["file"],"start_line":1,"line_count":1}),
    )
    .await?;
    records.push(feedback(2));
    assert_eq!(
        assemble(&environment, records.clone())
            .await?
            .feedback_notes
            .len(),
        1
    );

    records.push(input(3));
    assemble(&environment, records.clone()).await?;
    fs::write(&rules, "UPDATED SCOPE\n")?;
    rejected(
        call(
            &host,
            "cocoa.write",
            json!({"file":observation["file"],"content":"after review"}),
        )
        .await,
        "instructions_pending",
    );
    assert!(!directory.path().join("src/new.txt").exists());
    records.push(feedback(3));
    let context = assemble(&environment, records.clone()).await?;
    assert_eq!(context.feedback_notes.len(), 2);
    assert!(context.feedback_notes[1].text.contains("UPDATED SCOPE"));
    assert!(!context.instructions.contains("UPDATED SCOPE"));
    call(
        &host,
        "cocoa.write",
        json!({"file":observation["file"],"content":"after review"}),
    )
    .await?;

    records.push(input(4));
    assemble(&environment, records.clone()).await?;
    fs::remove_file(&rules)?;
    rejected(
        call(
            &host,
            "cocoa.run",
            json!({"working_directory":"src","command":"printf reviewed"}),
        )
        .await,
        "instructions_pending",
    );
    records.push(feedback(4));
    let context = assemble(&environment, records).await?;
    assert_eq!(context.feedback_notes.len(), 3);
    assert!(context.feedback_notes[2].text.contains("no longer apply"));
    environment.close().await?;
    Ok(())
}

#[tokio::test]
async fn external_targets_do_not_contribute_rules_and_rule_symlinks_stay_in_scope()
-> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let home = directory.path().join("home");
    let root = directory.path().join("repo");
    let external = directory.path().join("external");
    fs::create_dir_all(home.join(".config/cocoa"))?;
    fs::create_dir_all(root.join("src"))?;
    fs::create_dir(&external)?;
    fs::write(directory.path().join("AGENTS.md"), "OUTSIDE ANCESTOR\n")?;
    fs::write(home.join(".config/cocoa/AGENTS.md"), "PERSONAL RULE\n")?;
    fs::write(root.join("AGENTS.md"), "LOCAL ROOT\n")?;
    fs::write(external.join("AGENTS.md"), "EXTERNAL RULE\n")?;
    fs::write(external.join("file.txt"), "external data")?;
    symlink(&external, root.join("outside"))?;
    fs::write(root.join("src/CLAUDE.md"), "EXPLICIT LINK RULE\n")?;
    symlink("CLAUDE.md", root.join("src/AGENTS.md"))?;
    let (environment, host) = project(&root, Some(&home)).await?;
    let initial = assemble(&environment, vec![input(1)]).await?;
    assert!(initial.instructions.contains("PERSONAL RULE"));
    assert!(initial.instructions.contains("LOCAL ROOT"));
    assert!(!initial.instructions.contains("OUTSIDE ANCESTOR"));
    call(&host, "cocoa.read", json!({"path":"outside/file.txt"})).await?;
    let context = assemble(&environment, vec![input(1), feedback(1)]).await?;
    assert!(context.feedback_notes.is_empty());
    call(&host, "cocoa.read", json!({"path":"src/new.txt"})).await?;
    let context = assemble(&environment, vec![input(1), feedback(1), feedback(1)]).await?;
    assert!(
        context.feedback_notes[0]
            .text
            .contains("EXPLICIT LINK RULE")
    );
    fs::remove_file(root.join("src/AGENTS.md"))?;
    symlink(external.join("AGENTS.md"), root.join("src/AGENTS.md"))?;
    assert!(matches!(
        assemble(
            &environment,
            vec![input(1), feedback(1), feedback(1), input(2)],
        )
        .await,
        Err(ContextError::Unavailable { .. })
    ));
    rejected(
        call(
            &host,
            "cocoa.run",
            json!({"working_directory":"src","command":"printf bad > sentinel"}),
        )
        .await,
        "instructions_unavailable",
    );
    assert!(!root.join("src/sentinel").exists());
    fs::remove_file(external.join("AGENTS.md"))?;
    rejected(
        call(
            &host,
            "cocoa.run",
            json!({"working_directory":"src","command":"printf bad > sentinel"}),
        )
        .await,
        "instructions_unavailable",
    );
    assert!(!root.join("src/sentinel").exists());
    environment.close().await?;
    Ok(())
}

#[tokio::test]
async fn oversized_invalid_and_nonregular_rules_fail_before_effects() -> Result<(), Box<dyn Error>>
{
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("AGENTS.md");
    let (environment, host) = project(directory.path(), None).await?;
    fs::write(&path, "x".repeat(64 * 1024))?;
    let context = assemble(&environment, vec![input(1)]).await?;
    assert!(context.instructions.contains(&"x".repeat(64 * 1024)));
    fs::write(&path, "x".repeat(64 * 1024 + 1))?;
    assert!(matches!(
        assemble(&environment, vec![input(2)]).await,
        Err(ContextError::Unavailable { .. })
    ));
    fs::write(&path, [0xff])?;
    assert!(matches!(
        assemble(&environment, vec![input(3)]).await,
        Err(ContextError::Unavailable { .. })
    ));
    fs::remove_file(&path)?;
    fs::create_dir(&path)?;
    assert!(matches!(
        assemble(&environment, vec![input(4)]).await,
        Err(ContextError::Unavailable { .. })
    ));
    fs::remove_dir(&path)?;
    assemble(&environment, vec![input(5)]).await?;
    fs::create_dir(directory.path().join("bad"))?;
    fs::write(directory.path().join("bad/AGENTS.md"), [0xff])?;
    rejected(
        call(
            &host,
            "cocoa.run",
            json!({"working_directory":"bad","command":"printf bad > sentinel"}),
        )
        .await,
        "instructions_unavailable",
    );
    assert!(!directory.path().join("bad/sentinel").exists());
    environment.close().await?;
    Ok(())
}

#[tokio::test]
async fn process_control_bypasses_instruction_target_discovery() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let (environment, host) = project(directory.path(), None).await?;
    rejected(
        call(
            &host,
            "cocoa.wait",
            json!({"handles":{"build":"process.0.0"},"timeout_ms":0}),
        )
        .await,
        "invalid_reference",
    );
    rejected(
        call(&host, "cocoa.cancel", json!({"reference":"process.0.0"})).await,
        "invalid_reference",
    );
    assemble(&environment, vec![input(1)]).await?;
    let receipt = host
        .call(
            "cocoa.run",
            json!({"command":"printf ready; sleep 30"}),
            WorkLimits {
                output_bytes: 65536,
                deadline: tokio::time::Instant::now() + Duration::from_secs(5),
            },
            CancellationToken::new(),
        )
        .await?;
    fs::write(directory.path().join("AGENTS.md"), [0xff])?;
    let pending = call(
        &host,
        "cocoa.wait",
        json!({"handles":{"build":receipt["reference"]},"timeout_ms":50}),
    )
    .await?;
    assert_eq!(pending["build"]["status"]["kind"], "running");
    assert_eq!(pending["build"]["stdout"]["text"], "ready");
    assert_eq!(
        call(
            &host,
            "cocoa.cancel",
            json!({"reference":receipt["reference"]})
        )
        .await?["state"],
        "cancelling"
    );
    let completed = call(
        &host,
        "cocoa.wait",
        json!({"handles":{"build":receipt["reference"]}}),
    )
    .await?;
    assert_eq!(completed["build"]["status"]["kind"], "signalled");
    environment.close().await?;
    Ok(())
}

#[tokio::test]
async fn skill_catalog_preserves_source_identity_and_refreshes_per_invocation()
-> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let home = directory.path().join("home");
    let root = home.join("projects/repo");
    fs::create_dir_all(root.join(".git"))?;
    let paths = [
        home.join(".config/cocoa/skills/review/SKILL.md"),
        home.join(".agents/skills/review/SKILL.md"),
        home.join("projects/.agents/skills/review/SKILL.md"),
        root.join(".agents/skills/group/review/SKILL.md"),
        root.join(".cocoa/skills/review/SKILL.md"),
    ];
    for (index, path) in paths.iter().enumerate() {
        write_skill(path, "review", &format!("Source {index}"), "BODY ONLY\n")?;
    }
    let unrelated_paths = [
        home.join(".config/unrelated-app/skills/review/SKILL.md"),
        root.join(".unrelated-app/skills/review/SKILL.md"),
        home.join(".config/unrelated-app/skills/unrelated-only/SKILL.md"),
        root.join(".unrelated-app/skills/unrelated-only/SKILL.md"),
    ];
    let mut unrelated_bytes = Vec::with_capacity(unrelated_paths.len());
    for (index, path) in unrelated_paths.iter().enumerate() {
        write_skill(
            path,
            if index < 2 {
                "review"
            } else {
                "unrelated-only"
            },
            &format!("UNRELATED SOURCE {index}"),
            "UNRELATED BODY\n",
        )?;
        unrelated_bytes.push(fs::read(path)?);
    }
    let project_skill = paths[3].parent().ok_or("missing skill directory")?;
    symlink(project_skill, root.join(".agents/skills/alias"))?;
    fs::create_dir(project_skill.join("nested"))?;
    fs::write(
        project_skill.join("nested/SKILL.md"),
        "not a separate skill",
    )?;
    write_skill(
        &home.join(".cocoa/skills/excluded/SKILL.md"),
        "excluded-home",
        "Excluded home project root",
        "EXCLUDED\n",
    )?;
    write_skill(
        &directory.path().join(".agents/skills/excluded/SKILL.md"),
        "excluded-ancestor",
        "Excluded above home",
        "EXCLUDED\n",
    )?;
    let (environment, host) = project(&root, Some(&home)).await?;
    assert!(
        host.descriptions()
            .iter()
            .any(|method| method.name == "cocoa.load_skill")
    );
    let initial = assemble(&environment, vec![input(1)]).await?;
    let entries = initial
        .instructions
        .split_once("<skill-catalog>\n")
        .and_then(|(_, text)| text.split_once("\n</skill-catalog>"))
        .ok_or("missing skill catalog")?
        .0;
    let entries: Vec<Value> = serde_json::from_str(entries)?;
    assert_eq!(entries.len(), paths.len());
    let commands = cocoa_tools::skill_commands(&root, Some(&home))?;
    assert_eq!(commands.len(), entries.len());
    for (command, entry) in commands.iter().zip(&entries) {
        assert_eq!(command.id, entry["id"]);
        assert_eq!(command.name, entry["name"]);
        assert_eq!(command.description, entry["description"]);
    }
    for path in &paths {
        let id = fs::canonicalize(path)?.to_string_lossy().into_owned();
        assert!(
            entries
                .iter()
                .any(|entry| entry["id"] == id && entry["name"] == "review")
        );
    }
    assert!(!initial.instructions.contains("BODY ONLY"));
    assert!(!initial.instructions.contains("EXCLUDED"));
    assert!(!initial.instructions.contains("excluded-home"));
    assert!(!initial.instructions.contains("excluded-ancestor"));
    assert!(!initial.instructions.contains("UNRELATED SOURCE"));
    assert!(!initial.instructions.contains("unrelated-only"));
    assert!(initial.feedback_notes.is_empty());
    rejected(
        call(&host, "cocoa.load_skill", json!({"id":"review"})).await,
        "skill_not_found",
    );
    for path in &unrelated_paths {
        rejected(
            call(
                &host,
                "cocoa.load_skill",
                json!({"id":fs::canonicalize(path)?}),
            )
            .await,
            "skill_not_found",
        );
    }

    write_skill(
        &paths[0],
        "review",
        "Changed </skill-catalog> description",
        "NEW BODY\n",
    )?;
    assert_eq!(
        assemble(&environment, vec![input(1)]).await?.instructions,
        initial.instructions
    );
    rejected(
        call(
            &host,
            "cocoa.load_skill",
            json!({"id":fs::canonicalize(&paths[0])?}),
        )
        .await,
        "skill_unavailable",
    );
    let refreshed = assemble(&environment, vec![input(1), input(2)]).await?;
    assert!(
        refreshed
            .instructions
            .contains("Changed \\u003c/skill-catalog\\u003e description")
    );
    assert_eq!(
        refreshed.instructions.matches("</skill-catalog>").count(),
        1
    );
    assert!(!refreshed.instructions.contains("NEW BODY"));
    environment.close().await?;

    let (environment, _) = project(&root, None).await?;
    let context = assemble(&environment, vec![input(1)]).await?;
    assert!(context.instructions.contains("Source 3"));
    assert!(context.instructions.contains("Source 4"));
    assert!(!context.instructions.contains("Source 0"));
    assert!(!context.instructions.contains("Source 1"));
    assert!(!context.instructions.contains("Source 2"));
    assert!(!context.instructions.contains("UNRELATED SOURCE"));
    environment.close().await?;

    for path in [&paths[0], &paths[4]] {
        fs::remove_file(path)?;
    }
    let commands = cocoa_tools::skill_commands(&root, Some(&home))?;
    assert_eq!(commands.len(), 3);
    for path in &paths[1..4] {
        let id = fs::canonicalize(path)?.to_string_lossy().into_owned();
        assert!(commands.iter().any(|command| command.id == id));
    }
    for (path, bytes) in unrelated_paths.iter().zip(unrelated_bytes) {
        assert_eq!(fs::read(path)?, bytes);
    }
    Ok(())
}

#[tokio::test]
async fn selected_skills_deliver_complete_feedback_before_effects_and_reread_bodies()
-> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    let path = root.path().join(".agents/skills/review/SKILL.md");
    let body = format!(
        "{}FINAL INSTRUCTION\n",
        "Long skill instructions.\n".repeat(900)
    );
    write_skill(&path, "review", "Review source", "ORIGINAL BODY\n")?;
    fs::write(root.path().join("source.txt"), "original")?;
    let id = fs::canonicalize(&path)?;
    let (environment, host) = project(root.path(), None).await?;
    let initial = assemble(&environment, vec![input(1)]).await?;
    assert!(!initial.instructions.contains("ORIGINAL BODY"));
    let missing = call(&host, "cocoa.read", json!({"path":"new.txt"})).await?;
    let source = call(&host, "cocoa.read", json!({"path":"source.txt"})).await?;
    write_skill(&path, "review", "Review source", &body)?;
    assert_eq!(
        call(&host, "cocoa.load_skill", json!({"id":id})).await?,
        json!({"id":id,"state":"pending"})
    );
    assert_eq!(
        call(&host, "cocoa.load_skill", json!({"id":id})).await?["state"],
        "pending"
    );
    assert_eq!(
        call(&host, "cocoa.read", json!({"path":"source.txt"})).await?["page"]["text"],
        "original"
    );
    for (method, arguments) in [
        (
            "cocoa.write",
            json!({"file":missing["file"],"content":"created"}),
        ),
        (
            "cocoa.edit",
            json!({"file":source["file"],"edits":[{"old_text":"original","new_text":"edited"}]}),
        ),
        ("cocoa.run", json!({"command":"printf ran > command.txt"})),
    ] {
        rejected(call(&host, method, arguments).await, "instructions_pending");
    }
    assert!(!root.path().join("new.txt").exists());
    assert!(!root.path().join("command.txt").exists());
    assert_eq!(
        fs::read_to_string(root.path().join("source.txt"))?,
        "original"
    );
    let records = vec![input(1), feedback(1)];
    assert!(matches!(
        environment
            .prepare(stored(records.clone())?, Vec::new(), 1)
            .await,
        Err(ContextError::Limit)
    ));
    rejected(
        call(&host, "cocoa.run", json!({"command":"true"})).await,
        "instructions_pending",
    );
    let context = assemble(&environment, records.clone()).await?;
    assert_eq!(context.instructions, initial.instructions);
    assert_eq!(context.feedback_notes.len(), 1);
    assert_eq!(context.feedback_notes[0].record, 1);
    assert!(context.feedback_notes[0].text.contains(&body));
    assert!(
        context.feedback_notes[0]
            .text
            .contains(&serde_json::to_string(&id)?)
    );
    assert!(!context.feedback_notes[0].text.contains("ORIGINAL BODY"));
    assert_eq!(
        context.feedback_notes[0]
            .text
            .matches("<skill-instructions>")
            .count(),
        1
    );
    assert_eq!(
        call(&host, "cocoa.load_skill", json!({"id":id})).await?["state"],
        "available"
    );
    assert_eq!(
        serde_json::to_value(
            assemble(&environment, records.clone())
                .await?
                .feedback_notes
        )?,
        serde_json::to_value(&context.feedback_notes)?
    );
    call(
        &host,
        "cocoa.write",
        json!({"file":missing["file"],"content":"created"}),
    )
    .await?;
    call(
        &host,
        "cocoa.edit",
        json!({"file":source["file"],"edits":[{"old_text":"original","new_text":"edited"}]}),
    )
    .await?;
    call(
        &host,
        "cocoa.run",
        json!({"command":"printf ran > command.txt"}),
    )
    .await?;
    assert_eq!(fs::read_to_string(root.path().join("new.txt"))?, "created");
    assert_eq!(
        fs::read_to_string(root.path().join("source.txt"))?,
        "edited"
    );
    assert_eq!(fs::read_to_string(root.path().join("command.txt"))?, "ran");

    write_skill(&path, "review", "Review source", "UPDATED BODY\n")?;
    assert_eq!(
        call(&host, "cocoa.load_skill", json!({"id":id})).await?["state"],
        "pending"
    );
    let mut history = records;
    history.push(feedback(1));
    let updated = assemble(&environment, history.clone()).await?;
    assert_eq!(updated.feedback_notes.len(), 2);
    assert_eq!(
        updated.feedback_notes[0].record,
        context.feedback_notes[0].record
    );
    assert_eq!(
        updated.feedback_notes[0].text,
        context.feedback_notes[0].text
    );
    assert_eq!(updated.feedback_notes[1].record, 2);
    assert!(updated.feedback_notes[1].text.contains("UPDATED BODY"));
    assert!(!updated.feedback_notes[1].text.contains("FINAL INSTRUCTION"));
    history.push(input(2));
    assemble(&environment, history).await?;
    assert_eq!(
        call(&host, "cocoa.load_skill", json!({"id":id})).await?["state"],
        "available"
    );
    environment.close().await?;
    Ok(())
}

#[tokio::test]
async fn skill_sources_validate_yaml_sizes_and_symlink_boundaries() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join("repo");
    let path = root.join(".agents/skills/review/SKILL.md");
    write_skill(&path, "review", "Review", "valid\n")?;
    let (environment, _) = project(&root, None).await?;
    for content in [
        "not YAML frontmatter".to_owned(),
        "---\nname: review\n---\ninstructions".to_owned(),
        "---\nname: bad--name\ndescription: test\n---\ninstructions".to_owned(),
        "---\nname: review\ndescription: !include /etc/passwd\n---\ninstructions".to_owned(),
        "---\nname: review\ndescription: test\n---\n  \n".to_owned(),
        format!(
            "---\nname: review\ndescription: test\nextra: {}\n---\ninstructions",
            "x".repeat(16 * 1024)
        ),
        format!(
            "---\nname: review\ndescription: test\n---\n{}",
            "x".repeat(64 * 1024)
        ),
    ] {
        fs::write(&path, content)?;
        assert!(matches!(
            assemble(&environment, vec![input(1)]).await,
            Err(ContextError::Unavailable { .. })
        ));
    }
    fs::write(
        &path,
        "\u{feff}---\r\nname: review\r\ndescription: >-\r\n  Review YAML\r\n  metadata\r\nallowed-tools: [shell]\r\n---\r\nCOMPLETE BODY\r\n",
    )?;
    let context = assemble(&environment, vec![input(1)]).await?;
    assert!(context.instructions.contains("Review YAML metadata"));
    assert!(!context.instructions.contains("allowed-tools"));
    assert!(!context.instructions.contains("COMPLETE BODY"));
    environment.close().await?;

    let skill_directory = path.parent().ok_or("missing skill directory")?;
    let body_path = skill_directory.join("content/body.md");
    fs::create_dir_all(body_path.parent().ok_or("missing content directory")?)?;
    fs::rename(&path, &body_path)?;
    symlink(&body_path, &path)?;
    let (environment, host) = project(&root, None).await?;
    assemble(&environment, vec![input(1)]).await?;
    call(
        &host,
        "cocoa.load_skill",
        json!({"id":fs::canonicalize(&body_path)?}),
    )
    .await?;
    let context = assemble(&environment, vec![input(1), feedback(1)]).await?;
    let metadata: Value = serde_json::from_str(
        context.feedback_notes[0]
            .text
            .lines()
            .nth(1)
            .ok_or("missing loaded skill metadata")?,
    )?;
    assert_eq!(
        metadata["directory"],
        serde_json::to_value(fs::canonicalize(skill_directory)?)?
    );
    assert!(context.feedback_notes[0].text.contains("COMPLETE BODY\r\n"));
    environment.close().await?;

    fs::remove_file(&path)?;
    fs::write(
        directory.path().join("outside.md"),
        "---\nname: outside\ndescription: outside\n---\noutside",
    )?;
    symlink(directory.path().join("outside.md"), &path)?;
    let (environment, _) = project(&root, None).await?;
    assert!(matches!(
        assemble(&environment, vec![input(1)]).await,
        Err(ContextError::Unavailable { .. })
    ));
    environment.close().await?;
    fs::remove_file(&path)?;
    fs::remove_dir_all(skill_directory)?;
    symlink(directory.path(), path.parent().ok_or("missing parent")?)?;
    let (environment, _) = project(&root, None).await?;
    assert!(matches!(
        assemble(&environment, vec![input(1)]).await,
        Err(ContextError::Unavailable { .. })
    ));
    environment.close().await?;
    Ok(())
}

#[tokio::test]
async fn selected_skill_budget_rejects_whole_load_without_gating_unselected_work()
-> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    let first = root.path().join(".agents/skills/first/SKILL.md");
    let second = root.path().join(".agents/skills/second/SKILL.md");
    let body = "\\".repeat(40 * 1024);
    write_skill(&first, "first", "First", &body)?;
    write_skill(&second, "second", "Second", &body)?;
    let (environment, host) = project(root.path(), None).await?;
    assemble(&environment, vec![input(1)]).await?;
    assert_eq!(
        call(
            &host,
            "cocoa.load_skill",
            json!({"id":fs::canonicalize(&first)?})
        )
        .await?["state"],
        "pending"
    );
    let context = assemble(&environment, vec![input(1), feedback(1)]).await?;
    assert!(context.feedback_notes[0].text.contains(&body));
    rejected(
        call(
            &host,
            "cocoa.load_skill",
            json!({"id":fs::canonicalize(&second)?}),
        )
        .await,
        "skill_unavailable",
    );
    assert_eq!(
        call(&host, "cocoa.run", json!({"command":"printf allowed"})).await?["stdout"]["text"],
        "allowed"
    );
    let unchanged = assemble(&environment, vec![input(1), feedback(1), feedback(1)]).await?;
    assert_eq!(
        serde_json::to_value(unchanged.feedback_notes)?,
        serde_json::to_value(context.feedback_notes)?
    );
    environment.close().await?;
    Ok(())
}

fn write_skill(
    path: &Path,
    name: &str,
    description: &str,
    body: &str,
) -> Result<(), Box<dyn Error>> {
    fs::create_dir_all(path.parent().ok_or("missing skill parent")?)?;
    fs::write(
        path,
        format!(
            "---\nname: {name}\ndescription: {}\n---\n{body}",
            serde_json::to_string(description)?
        ),
    )?;
    Ok(())
}

async fn project(
    root: &Path,
    home: Option<&Path>,
) -> Result<(Arc<ProjectEnvironment>, CodingCapabilities), Box<dyn Error>> {
    project_with_acceptance(root, home, None).await
}

async fn project_with_acceptance(
    root: &Path,
    home: Option<&Path>,
    acceptance: Option<Arc<dyn cocoa_tools::InstructionAcceptance>>,
) -> Result<(Arc<ProjectEnvironment>, CodingCapabilities), Box<dyn Error>> {
    let local = LocalEnvironment::new(
        LocalConfiguration {
            working_directory: root.to_path_buf(),
            shell: "/bin/bash".into(),
            environment: BTreeMap::from([("PATH".into(), "/usr/bin:/bin".into())]),
            output_directory: root.to_path_buf(),
        },
        CodingLimits {
            file_bytes: 1024 * 1024,
            observed_bytes: 16 * 1024 * 1024,
            observations: 64,
            processes: 2,
            captured_output_bytes: 65536,
            retained_output_bytes: 1024 * 1024,
            preview_lines: 200,
            default_timeout: Duration::from_secs(2),
        },
    )
    .await?;
    let environment = ProjectEnvironment::new(local, home.map(Path::to_path_buf))?;
    let environment = Arc::new(match acceptance {
        Some(acceptance) => environment.with_instruction_acceptance(acceptance),
        None => environment,
    });
    let host = CodingCapabilities::new(environment.clone());
    Ok((environment, host))
}

async fn assemble(
    environment: &ProjectEnvironment,
    records: Vec<Record>,
) -> Result<Context, ContextError> {
    let records = stored(records).map_err(|_| ContextError::Unavailable {
        message: "Invalid test identity".into(),
    })?;
    let prepared = environment
        .prepare(records, Vec::new(), 1024 * 1024)
        .await?;
    if let Some(state) = &prepared.instruction_state {
        environment.restore(state).await?;
    }
    Ok(prepared.context)
}

fn stored(records: Vec<Record>) -> Result<Vec<StoredRecord>, cocoa_core::store::IdentityError> {
    records
        .into_iter()
        .enumerate()
        .map(|(index, record)| {
            let number = index as i64 + 1;
            Ok(StoredRecord {
                id: RecordId::try_from(number)?,
                sequence: RecordSequence::try_from(number)?,
                record,
            })
        })
        .collect()
}

async fn call(
    host: &CodingCapabilities,
    method: &str,
    arguments: Value,
) -> Result<Value, CapabilityError> {
    let value = host
        .call(
            method,
            arguments,
            WorkLimits {
                output_bytes: 65536,
                deadline: tokio::time::Instant::now() + Duration::from_secs(5),
            },
            CancellationToken::new(),
        )
        .await?;
    if method == "cocoa.run" {
        let results = host
            .call(
                "cocoa.wait",
                json!({"handles":{"command":value["reference"]}}),
                WorkLimits {
                    output_bytes: 65536,
                    deadline: tokio::time::Instant::now() + Duration::from_secs(5),
                },
                CancellationToken::new(),
            )
            .await?;
        assert_ne!(results["command"]["status"]["kind"], "running");
        return Ok(results["command"].clone());
    }
    Ok(value)
}

fn rejected(result: Result<Value, CapabilityError>, expected: &str) {
    assert!(
        matches!(&result, Err(CapabilityError::Tool { code, effects: Effects::None, .. }) if code == expected),
        "{result:?}"
    );
}

fn input(invocation: u64) -> Record {
    Record {
        invocation: InvocationId(invocation),
        kind: RecordKind::InputAccepted {
            input: "Work in the project".into(),
        },
    }
}

fn feedback(invocation: u64) -> Record {
    Record {
        invocation: InvocationId(invocation),
        kind: RecordKind::CellCompleted {
            model: match RecordId::try_from(1) {
                Ok(id) => id,
                Err(_) => unreachable!(),
            },
            cell: 0,
            code: String::new(),
            outcome: CellResponse::Completed {
                printed: Vec::new(),
            },
        },
    }
}

#[tokio::test]
async fn checkpoint_projection_reattaches_scoped_rules_without_acknowledging_delivery()
-> Result<(), Box<dyn Error>> {
    use cocoa_core::{
        compaction::{Checkpoint, CheckpointLimits, CheckpointProposal, Recap},
        execution::Cell,
        model::{ModelAction, ModelResponse},
        store::StoreSnapshot,
    };
    let root = tempfile::tempdir()?;
    fs::create_dir(root.path().join("src"))?;
    fs::write(root.path().join("src/AGENTS.md"), "SCOPED COMPACTION RULE")?;
    let (environment, host) = project(root.path(), None).await?;
    assemble(&environment, vec![input(1)]).await?;
    call(&host, "cocoa.read", json!({"path":"src/new.txt"})).await?;
    let model = RecordId::try_from(2)?;
    let invocation = InvocationId(1);
    let history = stored(vec![
        input(1),
        Record {
            invocation,
            kind: RecordKind::ModelCompleted {
                response: ModelResponse {
                    action: ModelAction::Execute {
                        cells: vec![Cell {
                            code: "read scoped source".into(),
                        }],
                    },
                    continuation: None,
                    usage: None,
                },
            },
        },
        Record {
            invocation,
            kind: RecordKind::CellStarted { model, cell: 0 },
        },
        Record {
            invocation,
            kind: RecordKind::CellCompleted {
                model,
                cell: 0,
                code: "read scoped source".into(),
                outcome: CellResponse::Completed {
                    printed: Vec::new(),
                },
            },
        },
    ])?;
    let prepared = environment
        .prepare(history.clone(), Vec::new(), 1024 * 1024)
        .await?;
    assert_eq!(prepared.context.feedback_notes.len(), 1);
    assert_eq!(prepared.context.feedback_notes[0].record, 3);
    assert!(
        prepared.context.feedback_notes[0]
            .text
            .contains("SCOPED COMPACTION RULE")
    );
    let checkpoint = Checkpoint::prepare(
        &StoreSnapshot {
            records: history.clone(),
            instruction_state: None,
            checkpoints: Vec::new(),
        },
        history[3].id,
        CheckpointProposal {
            additions: Vec::new(),
            changes: Vec::new(),
            evidence: Vec::new(),
            recap: Recap {
                goal: "Keep scoped instructions available".into(),
                stages: Vec::new(),
                current_stage: None,
                decisions: Vec::new(),
                progress: Vec::new(),
                failures: Vec::new(),
                next_actions: Vec::new(),
            },
        },
        CheckpointLimits {
            bytes: 16 * 1024,
            required_entries: 16,
            items: 16,
        },
    )?;
    let compacted = environment
        .prepare_compacted(history.clone(), Vec::new(), 1024 * 1024, &checkpoint)
        .await?;
    assert_eq!(
        compacted
            .context
            .instructions
            .matches("SCOPED COMPACTION RULE")
            .count(),
        1
    );
    assert!(compacted.context.feedback_notes.is_empty());
    assert!(
        !compacted
            .context
            .records
            .iter()
            .any(|record| matches!(record.kind, RecordKind::CellCompleted { .. }))
    );
    rejected(
        call(
            &host,
            "cocoa.run",
            json!({"working_directory":"src", "command":"touch unexpected"}),
        )
        .await,
        "instructions_pending",
    );
    assert!(!root.path().join("src/unexpected").exists());
    let candidate = compacted
        .instruction_state
        .ok_or("missing compacted instruction candidate")?;
    assert_eq!(candidate.scoped_feedback[0].record, history[3].id);
    environment.restore(&candidate).await?;
    assert_eq!(
        call(
            &host,
            "cocoa.run",
            json!({"working_directory":"src", "command":"printf accepted"})
        )
        .await?["stdout"]["text"],
        "accepted"
    );
    environment.close().await?;
    Ok(())
}

#[tokio::test]
async fn prepared_delivery_keeps_gates_pending_until_restore_and_uses_stable_feedback_ids()
-> Result<(), Box<dyn Error>> {
    use cocoa_core::context::{InstructionDelivery, InstructionState};
    let root = tempfile::tempdir()?;
    fs::create_dir(root.path().join("src"))?;
    fs::write(root.path().join("src/AGENTS.md"), "SCOPED ORIGINAL")?;
    let skill = root.path().join(".agents/skills/check/SKILL.md");
    write_skill(&skill, "check", "Check", "SKILL ORIGINAL")?;
    let (environment, host) = project(root.path(), None).await?;
    assemble(&environment, vec![input(1)]).await?;
    call(&host, "cocoa.read", json!({"path":"src/new.txt"})).await?;
    call(
        &host,
        "cocoa.load_skill",
        json!({"id":fs::canonicalize(&skill)?}),
    )
    .await?;
    let mut records = stored(vec![input(1), feedback(1)])?;
    records[1].id = RecordId::try_from(9_007_199_254_740_993)?;
    let first = environment
        .prepare(records.clone(), Vec::new(), 1024 * 1024)
        .await?;
    let second = environment
        .prepare(records.clone(), Vec::new(), 1024 * 1024)
        .await?;
    assert_eq!(first.instruction_state, second.instruction_state);
    assert_eq!(first.context.feedback_notes[0].record, 1);
    rejected(
        call(&host, "cocoa.run", json!({"command":"touch unexpected"})).await,
        "instructions_pending",
    );
    assert!(!root.path().join("unexpected").exists());
    let mut pending = first.instruction_state.ok_or("missing candidate")?;
    pending.scoped_sources[0].delivery = InstructionDelivery::Pending;
    pending.selected_skills[0].delivery = InstructionDelivery::Pending;
    pending.scoped_feedback.clear();
    pending.skill_feedback.clear();
    let pending: InstructionState = serde_json::from_slice(&serde_json::to_vec(&pending)?)?;
    environment.close().await?;
    let (restored, host) = project(root.path(), None).await?;
    restored.restore(&pending).await?;
    let prepared = restored
        .prepare(records.clone(), Vec::new(), 1024 * 1024)
        .await?;
    let committed = prepared
        .instruction_state
        .ok_or("missing restored candidate")?;
    assert_eq!(committed.scoped_feedback[0].record, records[1].id);
    assert_eq!(committed.skill_feedback[0].record, records[1].id);
    rejected(
        call(&host, "cocoa.run", json!({"command":"touch unexpected"})).await,
        "instructions_pending",
    );
    restored.restore(&committed).await?;
    assert_eq!(
        call(&host, "cocoa.run", json!({"command":"printf accepted"})).await?["stdout"]["text"],
        "accepted"
    );
    fs::write(root.path().join("src/AGENTS.md"), "SCOPED UPDATED")?;
    records.push(StoredRecord {
        id: RecordId::try_from(9_007_199_254_740_994)?,
        sequence: RecordSequence::try_from(3)?,
        record: input(2),
    });
    let refreshed = restored.prepare(records, Vec::new(), 1024 * 1024).await?;
    assert!(refreshed.context.instructions.contains("SCOPED UPDATED"));
    assert!(
        refreshed.context.feedback_notes[0]
            .text
            .contains("SCOPED ORIGINAL")
    );
    let candidate = refreshed
        .instruction_state
        .ok_or("missing refreshed state")?;
    assert_eq!(candidate.scoped_feedback, committed.scoped_feedback);
    restored.close().await?;
    Ok(())
}

struct InstructionReceipt {
    operation: RecordId,
    state: cocoa_core::context::InstructionState,
    outcome: cocoa_core::capability::OperationOutcome,
}

#[derive(Default)]
struct InstructionReceipts {
    entries: tokio::sync::Mutex<Vec<InstructionReceipt>>,
    reject: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl cocoa_tools::InstructionAcceptance for InstructionReceipts {
    async fn accept(
        &self,
        operation: RecordId,
        state: &cocoa_core::context::InstructionState,
        outcome: &cocoa_core::capability::OperationOutcome,
    ) -> Result<(), cocoa_core::store::StoreError> {
        if self.reject.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(cocoa_core::store::StoreError::Backend {
                message: "acceptance outcome unknown".into(),
            });
        }
        self.entries.lock().await.push(InstructionReceipt {
            operation,
            state: state.clone(),
            outcome: outcome.clone(),
        });
        Ok(())
    }
}

struct InstructionCalls;

#[async_trait::async_trait]
impl cocoa_core::execution::CellExecutor for InstructionCalls {
    async fn execute(
        &mut self,
        _: cocoa_core::execution::Cell,
        operations: &cocoa_core::capability::OperationContext,
        _: CancellationToken,
    ) -> Result<CellResponse, cocoa_core::execution::ExecutionError> {
        let error = operations
            .call(
                "cocoa.run",
                json!({"working_directory":"src", "command":"touch unexpected"}),
            )
            .await;
        if let Err(CapabilityError::Store(error)) = error {
            return Err(CapabilityError::Store(error).into());
        }
        assert!(
            matches!(error, Err(CapabilityError::Tool { code, .. }) if code == "instructions_pending")
        );
        Ok(CellResponse::Completed {
            printed: Vec::new(),
        })
    }
    async fn shutdown(
        &mut self,
        _: CancellationToken,
    ) -> Result<cocoa_core::execution::ShutdownReport, cocoa_core::execution::ExecutionError> {
        Ok(cocoa_core::execution::ShutdownReport { unresolved_work: 0 })
    }
}

struct InstructionModel;

#[async_trait::async_trait]
impl cocoa_core::model::ModelProvider for InstructionModel {
    async fn respond(
        &self,
        request: cocoa_core::model::ModelRequest,
        _: CancellationToken,
    ) -> Result<cocoa_core::model::ModelResponse, cocoa_core::model::ModelError> {
        use cocoa_core::model::{ModelAction, ModelResponse};
        let action = if request
            .context
            .records
            .iter()
            .any(|record| matches!(record.kind, RecordKind::CellCompleted { .. }))
        {
            assert!(
                request.context.feedback_notes[0]
                    .text
                    .contains("DURABLE RULE")
            );
            ModelAction::Final {
                answer: "accepted".into(),
            }
        } else {
            ModelAction::Execute {
                cells: vec![cocoa_core::execution::Cell {
                    code: "discover".into(),
                }],
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
async fn durable_instruction_acceptance_receives_host_identity_and_exact_deferred_outcome()
-> Result<(), Box<dyn Error>> {
    use cocoa_core::{
        engine::{Engine, EngineAdapters, EngineSettings, Limits, RunRequest},
        store::{InMemoryStore, Store, StoreLimits},
    };
    for reject in [false, true] {
        let root = tempfile::tempdir()?;
        fs::create_dir(root.path().join("src"))?;
        fs::write(root.path().join("src/AGENTS.md"), "DURABLE RULE")?;
        let receipts = Arc::new(InstructionReceipts::default());
        receipts
            .reject
            .store(reject, std::sync::atomic::Ordering::SeqCst);
        let (environment, host) =
            project_with_acceptance(root.path(), None, Some(receipts.clone())).await?;
        let history = Arc::new(InMemoryStore::with_instruction_state(StoreLimits {
            record_bytes: 65536,
        })?);
        let settings = EngineSettings::new(Limits {
            input_bytes: 65536,
            output_bytes: 65536,
            record_bytes: 65536,
            context_bytes: 1024 * 1024,
            pending_operations: 2,
            stall_timeout: Duration::from_secs(5),
            settlement_timeout: Duration::from_secs(5),
            shutdown_timeout: Duration::from_secs(5),
        })?;
        let mut engine = Engine::new(
            settings,
            EngineAdapters {
                model: Arc::new(InstructionModel),
                executor: Box::new(InstructionCalls),
                capabilities: Arc::new(host),
                context: environment.clone(),
                store: history.clone(),
            },
            None,
        );
        let result = engine
            .run(
                RunRequest {
                    input: "test".into(),
                },
                CancellationToken::new(),
            )
            .await;
        assert_eq!(result.is_err(), reject);
        assert!(!root.path().join("src/unexpected").exists());
        let snapshot = history.load().await?;
        let entries = receipts.entries.lock().await;
        if reject {
            assert!(entries.is_empty());
            assert!(
                environment
                    .prepare(snapshot.records, Vec::new(), 1024 * 1024)
                    .await
                    .is_err()
            );
        } else {
            assert_eq!(entries.len(), 1);
            let receipt = &entries[0];
            let start = snapshot
                .records
                .iter()
                .find(|stored| stored.id == receipt.operation)
                .ok_or("receipt has no canonical start")?;
            assert!(
                matches!(&start.record.kind, RecordKind::OperationStarted { name, .. } if name == "cocoa.run")
            );
            assert_eq!(
                receipt.state.scoped_sources[0].delivery,
                cocoa_core::context::InstructionDelivery::Pending
            );
            let finish = snapshot
                .records
                .iter()
                .find_map(|stored| match &stored.record.kind {
                    RecordKind::OperationFinished { outcome, .. } => Some(outcome),
                    _ => None,
                })
                .ok_or("missing operation outcome")?;
            assert_eq!(&receipt.outcome, finish);
            assert_eq!(
                snapshot
                    .instruction_state
                    .ok_or("missing accepted state")?
                    .scoped_sources[0]
                    .delivery,
                cocoa_core::context::InstructionDelivery::Delivered
            );
        }
        engine.shutdown().await?;
        environment.close().await?;
    }
    Ok(())
}
