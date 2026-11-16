//! Coding capability contracts against temporary files and owned Unix processes.

use std::{
    collections::BTreeMap,
    error::Error,
    fs,
    os::unix::{
        fs::{MetadataExt, PermissionsExt, symlink},
        process::CommandExt,
    },
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use cocoa_core::{
    capability::{
        ActivityOutputKind, ActivitySummary, CapabilityError, CapabilityHost, Effects,
        OperationOutcome,
    },
    engine::ActivityStatus,
    execution::{WorkLimits, host_value_fits},
};
use cocoa_tools::{
    CodingCapabilities, CodingContext, CodingEnvironment, CodingLimits, CodingOperation,
    LocalConfiguration, LocalEnvironment, ReadSelector,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

type TestResult = Result<(), Box<dyn Error>>;

struct Fixture {
    directory: TempDir,
    environment: Arc<LocalEnvironment>,
    host: CodingCapabilities,
}

impl Fixture {
    async fn new(limits: CodingLimits) -> Result<Self, Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let environment = Arc::new(LocalEnvironment::new(configuration(&directory), limits).await?);
        let host = CodingCapabilities::new(environment.clone());
        Ok(Self {
            directory,
            environment,
            host,
        })
    }

    async fn call(&self, method: &str, arguments: Value) -> Result<Value, CapabilityError> {
        if method == "cocoa.run" {
            return run_to_completion(&self.host, arguments, work(64 * 1024)).await;
        }
        self.host
            .call(method, arguments, work(64 * 1024), CancellationToken::new())
            .await
    }

    async fn read(&self, path: &str) -> Result<Value, CapabilityError> {
        self.call("cocoa.read", json!({"path":path})).await
    }
}

async fn run_to_completion(
    host: &CodingCapabilities,
    arguments: Value,
    limits: WorkLimits,
) -> Result<Value, CapabilityError> {
    let receipt = host
        .call("cocoa.run", arguments, limits, CancellationToken::new())
        .await?;
    let results = host
        .call(
            "cocoa.wait",
            json!({"handles":{"command":receipt["reference"]}}),
            limits,
            CancellationToken::new(),
        )
        .await?;
    assert_ne!(
        results["command"]["status"]["kind"], "running",
        "test helper requires a terminal command"
    );
    Ok(results["command"].clone())
}

fn limits() -> CodingLimits {
    CodingLimits {
        file_bytes: 1024 * 1024,
        observed_bytes: 16 * 1024 * 1024,
        observations: 256,
        processes: 4,
        captured_output_bytes: 1024 * 1024,
        retained_output_bytes: 8 * 1024 * 1024,
        preview_lines: 200,
        default_timeout: Duration::from_secs(2),
    }
}

fn configuration(directory: &TempDir) -> LocalConfiguration {
    LocalConfiguration {
        working_directory: directory.path().into(),
        shell: "/bin/bash".into(),
        environment: BTreeMap::from([
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("SELECTED".into(), "baseline".into()),
        ]),
        output_directory: directory.path().into(),
    }
}

fn work(output_bytes: usize) -> WorkLimits {
    WorkLimits {
        output_bytes,
        deadline: tokio::time::Instant::now() + Duration::from_secs(5),
    }
}

fn failure(result: Result<Value, CapabilityError>, expected: &str, effects: Effects) {
    assert!(
        matches!(result, Err(CapabilityError::Tool { ref code, effects: actual, .. }) if code == expected && actual == effects),
        "expected {expected}/{effects:?}, got {result:?}"
    );
}

#[tokio::test]
async fn activity_describes_targets_ranges_and_counts_without_exposing_file_content() -> TestResult
{
    let fixture = Fixture::new(limits()).await?;
    fs::write(
        fixture.directory.path().join("note.txt"),
        "private original\n",
    )?;
    let read = fixture.read("note.txt").await?;
    let input = fixture
        .host
        .describe_input("cocoa.read", &json!({"path":"note.txt"}));
    assert_eq!(input.title, "Read note.txt");
    assert_eq!(input.text, "Requested lines: 1-200");
    assert_eq!(
        input.summary,
        Some(ActivitySummary::LineRange { start: 1, end: 200 })
    );
    let input = fixture.host.describe_input(
        "cocoa.read",
        &json!({"file":read["file"],"start_line":7,"line_count":3}),
    );
    assert_eq!(input.title, "Read note.txt");
    assert_eq!(input.text, "Requested lines: 7-9");
    assert_eq!(
        input.summary,
        Some(ActivitySummary::LineRange { start: 7, end: 9 })
    );
    let input = fixture.host.describe_input(
        "cocoa.write",
        &json!({"file":read["file"],"content":"α\nnew"}),
    );
    assert_eq!(input.title, "Write note.txt");
    assert_eq!(input.text, "Bytes supplied: 6");
    assert_eq!(
        input.summary,
        Some(ActivitySummary::LineChanges {
            added: 2,
            removed: 0
        })
    );
    for (content, added) in [("", 0), ("\n", 1), ("α\r\nβ\r\n", 2)] {
        let input = fixture.host.describe_input(
            "cocoa.write",
            &json!({"file":read["file"],"content":content}),
        );
        assert_eq!(
            input.summary,
            Some(ActivitySummary::LineChanges { added, removed: 0 })
        );
    }
    let input = fixture.host.describe_input(
        "cocoa.edit",
        &json!({"file":read["file"],"edits":[
            {"old_text":"private","new_text":"hidden replacement"},
            {"old_text":"original","new_text":"another replacement"}
        ]}),
    );
    assert_eq!(input.title, "Edit note.txt");
    assert_eq!(input.text, "Replacements requested: 2");
    assert_eq!(
        input.summary,
        Some(ActivitySummary::LineChanges {
            added: 2,
            removed: 2
        })
    );
    for (old_text, new_text, added, removed) in [
        ("keep\nold\nlast\n", "keep\nnew\nextra\nlast\n", 2, 1),
        ("keep\r\nold\r\n", "keep\r\n", 0, 1),
        ("same", "same", 0, 0),
        ("one\n", "", 0, 1),
        ("one", "one\n", 1, 1),
    ] {
        let input = fixture.host.describe_input(
            "cocoa.edit",
            &json!({"file":read["file"],"edits":[{ "old_text":old_text,"new_text":new_text }]}),
        );
        assert_eq!(
            input.summary,
            Some(ActivitySummary::LineChanges { added, removed })
        );
        assert_eq!(input.text, "Replacements requested: 1");
    }
    let input = fixture.host.describe_input(
        "cocoa.write",
        &json!({"file":read["file"],"content":"secret","unexpected":true}),
    );
    assert_eq!(input.text, "Input details unavailable.");
    assert_eq!(input.summary, None);
    let command = "\nprintf '%s\\n' 'a long command argument that exceeds the old eighty-byte header cutoff'\nexit 7";
    let input = fixture.host.describe_input(
        "cocoa.run",
        &json!({"command":command,"working_directory":"src","timeout_ms":1234,
            "environment":{"TOKEN":"hidden value","OLD":null},"stdin":"αβ"}),
    );
    assert_eq!(
        input.title,
        "Running printf '%s\\n' 'a long command argument that exceeds the old eighty-byte header cutoff'"
    );
    assert!(input.text.contains(command));
    assert!(input.text.contains("Working directory: src"));
    assert!(input.text.contains("Timeout: 1234 ms"));
    assert!(input.text.contains("Stdin bytes supplied: 4"));
    assert!(input.text.contains("set TOKEN"));
    assert!(input.text.contains("unset OLD"));
    assert!(!input.text.contains("hidden value"));
    assert!(!input.text.contains("αβ"));
    assert_eq!(input.summary, None);
    assert_eq!(
        fs::read_to_string(fixture.directory.path().join("note.txt"))?,
        "private original\n"
    );
    fixture.environment.close().await?;
    Ok(())
}

#[tokio::test]
async fn activity_diffs_use_committed_snapshots_and_count_actual_changed_lines() -> TestResult {
    let fixture = Fixture::new(limits()).await?;
    let path = fixture.directory.path().join("note.txt");
    fs::write(&path, "head\r\nold value\r\nlast\r\n")?;
    let read = fixture.read("note.txt").await?;
    assert!(
        fixture
            .host
            .describe_output(
                "cocoa.read",
                &json!({"path":"note.txt"}),
                &OperationOutcome::Completed {
                    value: read.clone()
                }
            )
            .is_none()
    );
    let arguments = json!({"file":read["file"],"content":"head\r\nnew value\r\nextra\r\nlast\r\n"});
    let written = fixture.call("cocoa.write", arguments.clone()).await?;
    let outcome = OperationOutcome::Completed {
        value: written.clone(),
    };
    let output = fixture
        .host
        .describe_output("cocoa.write", &arguments, &outcome)
        .ok_or("missing write diff")?;
    assert_eq!(output.kind, ActivityOutputKind::Diff);
    assert_eq!(
        output.summary,
        Some(ActivitySummary::LineChanges {
            added: 2,
            removed: 1
        })
    );
    assert!(output.text.starts_with("@@ -1,3 +1,4 @@\n"));
    assert!(
        output
            .text
            .contains("-old value\r\n+new value\r\n+extra\r\n")
    );
    assert!(output.text.contains(" head\r\n"));
    assert!(!output.truncated);
    let edits = json!({"file":written["file"],"edits":[{"old_text":"new","new_text":"changed"},{"old_text":"value","new_text":"limit"}]});
    let edited = fixture.call("cocoa.edit", edits.clone()).await?;
    let edit_outcome = OperationOutcome::Completed { value: edited };
    let edit_output = fixture
        .host
        .describe_output("cocoa.edit", &edits, &edit_outcome)
        .ok_or("missing edit diff")?;
    assert_eq!(
        edit_output.summary,
        Some(ActivitySummary::LineChanges {
            added: 1,
            removed: 1
        })
    );
    assert!(
        edit_output
            .text
            .contains("-new value\r\n+changed limit\r\n")
    );
    assert_eq!(
        fs::read_to_string(&path)?,
        "head\r\nchanged limit\r\nextra\r\nlast\r\n"
    );
    fs::write(&path, "external writer\n")?;
    assert_eq!(
        fixture
            .host
            .describe_output("cocoa.write", &arguments, &outcome),
        Some(output)
    );
    assert_eq!(
        fixture
            .host
            .describe_output("cocoa.edit", &edits, &edit_outcome),
        Some(edit_output)
    );
    let error = match fixture.call("cocoa.write", arguments.clone()).await {
        Err(error) => error,
        Ok(_) => return Err("stale write unexpectedly committed".into()),
    };
    assert!(
        fixture
            .host
            .describe_output(
                "cocoa.write",
                &arguments,
                &OperationOutcome::Failed { error }
            )
            .is_none()
    );
    for (name, content, truncated) in [
        ("new.txt", "α".to_owned(), false),
        ("large.txt", "α".repeat(5000), true),
    ] {
        let missing = fixture.read(name).await?;
        let arguments = json!({"file":missing["file"],"content":content});
        let value = fixture.call("cocoa.write", arguments.clone()).await?;
        let output = fixture
            .host
            .describe_output(
                "cocoa.write",
                &arguments,
                &OperationOutcome::Completed { value },
            )
            .ok_or("missing creation diff")?;
        assert_eq!(
            output.summary,
            Some(ActivitySummary::LineChanges {
                added: 1,
                removed: 0
            })
        );
        assert!(output.text.starts_with("@@ -0,0 +1 @@\n+α"));
        assert_eq!(output.truncated, truncated);
        assert!(output.text.len() <= 4096);
        if !truncated {
            assert!(output.text.contains("\\ No newline at end of file"));
        }
    }
    fixture.environment.close().await?;
    Ok(())
}

#[tokio::test]
async fn command_activities_disclose_only_captured_output_including_stderr() -> TestResult {
    let fixture = Fixture::new(CodingLimits {
        preview_lines: 1,
        ..limits()
    })
    .await?;
    for (command, expected, truncated, status) in [
        ("true", None, false, ActivityStatus::Completed),
        (
            "exit 7",
            None,
            false,
            ActivityStatus::Failed { exit_code: Some(7) },
        ),
        (
            "kill -TERM $$",
            None,
            false,
            ActivityStatus::Signalled {
                signal: rustix::process::Signal::TERM.as_raw(),
            },
        ),
        (
            "printf 'one'; printf 'error\\n' >&2; exit 13",
            Some("one\nerror\n"),
            false,
            ActivityStatus::Failed {
                exit_code: Some(13),
            },
        ),
        (
            "printf '**literal**\\n```diff\\n+text\\n'",
            Some("**literal**\n"),
            true,
            ActivityStatus::Completed,
        ),
        ("printf '\\n'", Some("\n"), false, ActivityStatus::Completed),
    ] {
        let arguments = json!({"command":command});
        let value = fixture
            .host
            .call(
                "cocoa.run",
                arguments.clone(),
                work(64 * 1024),
                CancellationToken::new(),
            )
            .await?;
        fixture
            .call(
                "cocoa.wait",
                json!({"handles":{"command":value["reference"]}}),
            )
            .await?;
        let outcome = OperationOutcome::Completed { value };
        assert_eq!(fixture.host.activity_status(&outcome), status, "{command}");
        let output = fixture
            .host
            .describe_output("cocoa.run", &arguments, &outcome);
        if let Some(expected) = expected {
            let output = output.ok_or("missing captured output")?;
            assert_eq!(output.text, expected, "{command}");
            assert_eq!(output.kind, ActivityOutputKind::Command);
            assert_eq!(output.truncated, truncated, "{command}");
        } else {
            assert!(output.is_none(), "{command}: {output:?}");
        }
    }
    let arguments =
        json!({"command":"printf 'issued output'; printf 'issued error\\n' >&2; exit 7"});
    let receipt = fixture
        .host
        .call(
            "cocoa.run",
            arguments.clone(),
            work(64 * 1024),
            CancellationToken::new(),
        )
        .await?;
    let results = fixture
        .call(
            "cocoa.wait",
            json!({"handles":{"command":receipt["reference"]}}),
        )
        .await?;
    assert_eq!(
        results["command"]["status"],
        json!({"kind":"exited","code":7})
    );
    let output = fixture
        .host
        .describe_output(
            "cocoa.run",
            &arguments,
            &OperationOutcome::Completed {
                value: receipt.clone(),
            },
        )
        .ok_or("issued command lost its captured output")?;
    assert_eq!(output.text, "issued output\nissued error\n");
    assert_eq!(output.kind, ActivityOutputKind::Command);
    assert!(!output.truncated);
    let other_host = CodingCapabilities::new(fixture.environment.clone());
    assert_eq!(
        other_host.activity_status(&OperationOutcome::Completed {
            value: receipt.clone()
        }),
        ActivityStatus::Unknown,
        "foreign command status must not be exposed or reported as success"
    );
    assert!(
        other_host
            .describe_output(
                "cocoa.run",
                &arguments,
                &OperationOutcome::Completed { value: receipt }
            )
            .is_none(),
        "command output must remain scoped to its issuing agent"
    );
    let arguments = json!({"command":"printf 'before timeout\\n'; sleep 5","timeout_ms":200});
    let receipt = fixture
        .host
        .call(
            "cocoa.run",
            arguments.clone(),
            work(64 * 1024),
            CancellationToken::new(),
        )
        .await?;
    let error = match fixture
        .call(
            "cocoa.wait",
            json!({"handles":{"command":receipt["reference"]}}),
        )
        .await
    {
        Err(error) => error,
        Ok(_) => return Err("timeout command unexpectedly completed".into()),
    };
    assert_eq!(
        fixture
            .host
            .activity_status(&OperationOutcome::Completed { value: receipt }),
        ActivityStatus::TimedOut,
        "a successful receipt must retain the later timeout failure"
    );
    let output = fixture
        .host
        .describe_output("cocoa.run", &arguments, &OperationOutcome::Failed { error })
        .ok_or("missing timeout output")?;
    assert_eq!(output.text, "before timeout\n");
    assert!(output.truncated);
    fixture.environment.close().await?;
    Ok(())
}

#[tokio::test]
async fn fresh_and_captured_pages_preserve_bytes_and_line_boundaries() -> TestResult {
    let fixture = Fixture::new(limits()).await?;
    for (name, text, lines) in [
        ("empty", "", 0),
        ("crlf", "left\r\nβeta\r\nfinalé", 3),
        ("newline", "one\n", 1),
        ("blank", "\n\n", 2),
    ] {
        fs::write(fixture.directory.path().join(name), text)?;
        let read = fixture.read(name).await?;
        assert_eq!(read["kind"], "text");
        assert_eq!(read["bytes"], text.len());
        assert_eq!(read["line_count"], lines);
        assert!(read.get("text").is_none());
        assert_eq!(read["page"]["text"], text);
        fs::write(fixture.directory.path().join(name), "outside writer")?;
        let chunk = fixture
            .call(
                "cocoa.read",
                json!({"file":read["file"],"start_line":1,"line_count":200}),
            )
            .await?;
        assert_eq!(chunk["page"]["text"], text);
        assert_eq!(chunk["page"]["line_count"], lines);
        assert_eq!(chunk["next"], Value::Null);
        assert_eq!(fixture.read(name).await?["page"]["text"], "outside writer");
        let past_end = fixture
            .call(
                "cocoa.read",
                json!({"file":read["file"],"start_line":999,"line_count":200}),
            )
            .await?;
        assert_eq!(past_end["page"]["text"], "");
        assert_eq!(past_end["page"]["line_count"], 0);
        assert_eq!(past_end["next"], Value::Null);
    }
    fixture.environment.close().await?;
    Ok(())
}

#[tokio::test]
async fn pages_handle_199_200_201_lines_and_asymmetric_continuation() -> TestResult {
    let fixture = Fixture::new(limits()).await?;
    for count in [199, 200, 201] {
        let text: String = (1..=count)
            .map(|number| format!("line-{number}\n"))
            .collect();
        fs::write(fixture.directory.path().join("lines"), &text)?;
        let read = fixture.read("lines").await?;
        let first = &read;
        assert_eq!(first["line_count"], count);
        assert_eq!(first["page"]["line_count"], count.min(200));
        assert_eq!(
            first["page"]["text"],
            text.split_inclusive('\n').take(200).collect::<String>()
        );
        assert_eq!(
            first["next"],
            if count == 201 {
                json!({"file":read["file"],"start_line":201,"line_count":200})
            } else {
                Value::Null
            }
        );
        let last = fixture
            .call(
                "cocoa.read",
                json!({"file":read["file"],"start_line":199,"line_count":2}),
            )
            .await?;
        assert_eq!(
            last["page"]["text"],
            if count == 199 {
                "line-199\n"
            } else {
                "line-199\nline-200\n"
            }
        );
        assert_eq!(
            last["next"],
            if count == 201 {
                json!({"file":read["file"],"start_line":201,"line_count":2})
            } else {
                Value::Null
            }
        );
        if count == 201 {
            let remainder = fixture.call("cocoa.read", first["next"].clone()).await?;
            assert_eq!(remainder["page"]["text"], "line-201\n");
            assert_eq!(remainder["next"], Value::Null);
            fs::write(fixture.directory.path().join("lines"), "outside\n")?;
            let stable = fixture.call("cocoa.read", last["next"].clone()).await?;
            assert_eq!(stable["page"]["text"], "line-201\n");
        }
    }
    fixture.environment.close().await?;
    Ok(())
}

#[tokio::test]
async fn byte_limited_pages_do_not_skip_or_split_unicode_lines() -> TestResult {
    let fixture = Fixture::new(limits()).await?;
    let text = format!("small\r\n{}\nend", "é\\\"".repeat(150));
    fs::write(fixture.directory.path().join("bytes"), &text)?;
    let read = fixture.read("bytes").await?;
    let first = fixture
        .host
        .call(
            "cocoa.read",
            json!({"file":read["file"],"start_line":1,"line_count":200}),
            work(512),
            CancellationToken::new(),
        )
        .await?;
    assert_eq!(first["page"]["text"], "small\r\n");
    assert_eq!(first["page"]["line_count"], 1);
    assert_eq!(
        first["next"],
        json!({"file":read["file"],"start_line":2,"line_count":200})
    );
    assert!(host_value_fits(&first, 512));
    failure(
        fixture
            .host
            .call(
                "cocoa.read",
                first["next"].clone(),
                work(512),
                CancellationToken::new(),
            )
            .await,
        "line_too_large",
        Effects::None,
    );
    let rest = fixture.call("cocoa.read", first["next"].clone()).await?;
    assert_eq!(rest["page"]["text"], &text[7..]);
    assert_eq!(rest["next"], Value::Null);
    fs::write(
        fixture.directory.path().join("huge"),
        "x".repeat(270 * 1024),
    )?;
    failure(
        fixture
            .host
            .call(
                "cocoa.read",
                json!({"path":"huge"}),
                work(1024 * 1024),
                CancellationToken::new(),
            )
            .await,
        "line_too_large",
        Effects::None,
    );
    fixture.environment.close().await?;
    Ok(())
}

#[tokio::test]
async fn absence_is_distinct_from_parent_encoding_permissions_and_unsupported_files() -> TestResult
{
    let fixture = Fixture::new(limits()).await?;
    let missing = fixture.read("missing").await?;
    assert_eq!(missing["kind"], "missing");
    assert_eq!(
        fixture
            .call(
                "cocoa.read",
                json!({"file":missing["file"],"start_line":1,"line_count":200})
            )
            .await?,
        missing
    );
    assert_eq!(fixture.read("absent-parent/leaf").await?["kind"], "missing");
    assert!(!fixture.directory.path().join("absent-parent").exists());
    fs::write(fixture.directory.path().join("blocker"), "file")?;
    failure(
        fixture.read("blocker/leaf").await,
        "not_directory",
        Effects::None,
    );
    fs::write(fixture.directory.path().join("invalid"), [0xff, b'\n'])?;
    failure(
        fixture.read("invalid").await,
        "invalid_encoding",
        Effects::None,
    );
    failure(fixture.read(".").await, "unsupported_file", Effects::None);
    symlink("missing", fixture.directory.path().join("link"))?;
    failure(
        fixture.read("link").await,
        "unsupported_file",
        Effects::None,
    );
    fs::write(fixture.directory.path().join("private"), "not absent")?;
    fs::set_permissions(
        fixture.directory.path().join("private"),
        fs::Permissions::from_mode(0o0),
    )?;
    // Privileged CI users can read mode-000 files, so only assert denial when the OS denies it.
    if fs::read(fixture.directory.path().join("private")).is_err() {
        failure(
            fixture.read("private").await,
            "permission_denied",
            Effects::None,
        );
    }
    fs::set_permissions(
        fixture.directory.path().join("private"),
        fs::Permissions::from_mode(0o600),
    )?;
    fixture.environment.close().await?;
    Ok(())
}

#[tokio::test]
async fn observations_are_agent_and_environment_scoped_and_never_evicted() -> TestResult {
    let fixture = Fixture::new(CodingLimits {
        observations: 1,
        ..limits()
    })
    .await?;
    fs::write(fixture.directory.path().join("source"), "original")?;
    let read = fixture.read("source").await?;
    let other_agent = CodingCapabilities::new(fixture.environment.clone());
    failure(
        other_agent
            .call(
                "cocoa.read",
                json!({"file":read["file"],"start_line":1,"line_count":200}),
                work(4096),
                CancellationToken::new(),
            )
            .await,
        "invalid_reference",
        Effects::None,
    );
    let other = Fixture::new(limits()).await?;
    failure(
        other
            .call(
                "cocoa.read",
                json!({"file":read["file"],"start_line":1,"line_count":200}),
            )
            .await,
        "invalid_reference",
        Effects::None,
    );
    failure(fixture.read("new").await, "capacity", Effects::None);
    failure(
        fixture
            .call(
                "cocoa.write",
                json!({"file":read["file"],"content":"not committed"}),
            )
            .await,
        "capacity",
        Effects::None,
    );
    assert_eq!(
        fs::read_to_string(fixture.directory.path().join("source"))?,
        "original"
    );
    assert_eq!(
        fixture
            .call(
                "cocoa.read",
                json!({"file":read["file"],"start_line":1,"line_count":200})
            )
            .await?["page"]["text"],
        "original"
    );
    fixture.environment.close().await?;
    other.environment.close().await?;
    Ok(())
}

#[tokio::test]
async fn normalized_multi_edits_target_original_bytes_without_collateral_changes() -> TestResult {
    let fixture = Fixture::new(limits()).await?;
    let path = fixture.directory.path().join("normalized");
    let original = "\u{feff}keep　\r\nα ‘cafe\u{301}’  \r\nsecond\nlast\runtouched ﬁ\t";
    fs::write(&path, original)?;
    let read = fixture.read("normalized").await?;
    let result = fixture
        .call(
            "cocoa.edit",
            json!({"file":read["file"],"edits":[
                {"old_text":"second","new_text":"a\nb\rc\r\nd ‘ﬁ’"},
                {"old_text":"'café'","new_text":"second"}
            ]}),
        )
        .await?;
    assert_ne!(result["file"], read["file"]);
    assert_eq!(
        fs::read(&path)?,
        "\u{feff}keep　\r\nα second  \r\na\r\nb\r\nc\r\nd ‘ﬁ’\nlast\runtouched ﬁ\t".as_bytes()
    );
    assert_eq!(
        fixture
            .call(
                "cocoa.read",
                json!({"file":read["file"],"start_line":1,"line_count":200})
            )
            .await?["page"]["text"],
        original
    );
    fixture.environment.close().await?;
    Ok(())
}

#[tokio::test]
async fn normalized_edits_cover_composition_reordering_hangul_and_compatibility_units() -> TestResult
{
    let fixture = Fixture::new(limits()).await?;
    let path = fixture.directory.path().join("unicode");
    for (source, anchor) in [
        ("cafe\u{301}", "café"),
        ("café", "cafe\u{301}"),
        ("a\u{315}\u{300}", "à\u{315}"),
        ("à\u{315}", "a\u{300}\u{315}"),
        ("X\u{0344}", "Ẍ\u{0301}"),
        ("\u{1100}\u{1161}\u{11a8}", "각"),
        ("각", "\u{1100}\u{1161}\u{11a8}"),
        ("가\u{11a8}", "각"),
        ("\u{09c7}\u{09be}", "\u{09cb}"),
        ("ﬃ", "ffi"),
        ("ffi", "ﬃ"),
        ("Ⅳ", "IV"),
        ("①", "1"),
        ("ＡＢＣ", "ABC"),
        ("ﷺ", "صلى الله عليه وسلم"),
        ("‘x’ ‚y‛ “z” „w‟", "'x' 'y' \"z\" \"w\""),
        ("a‐b‑c‒d–e—f―g−h", "a-b-c-d-e-f-g-h"),
        (
            "a\u{a0}b\u{2002}c\u{200a}d\u{202f}e\u{205f}f\u{3000}g",
            "a b c d e f g",
        ),
        (
            "  one\u{2003}\t\r\n  two\u{3000}\rthree",
            "  one\n  two\nthree",
        ),
    ] {
        let original = format!("\u{feff}Ω|{source}|untouched ﬁ　\r\n");
        fs::write(&path, &original)?;
        let read = fixture.read("unicode").await?;
        fixture
            .call(
                "cocoa.edit",
                json!({"file":read["file"],"edits":[{"old_text":anchor,"new_text":"changed"}]}),
            )
            .await?;
        assert_eq!(
            fs::read(&path)?,
            "\u{feff}Ω|changed|untouched ﬁ　\r\n".as_bytes(),
            "source {source:?}, anchor {anchor:?}"
        );
    }
    fixture.environment.close().await?;
    Ok(())
}

#[tokio::test]
async fn normalized_edits_reject_duplicates_overlap_empty_and_partial_units_without_effects()
-> TestResult {
    let fixture = Fixture::new(limits()).await?;
    let path = fixture.directory.path().join("reject");
    for (source, anchors, code) in [
        ("'x'|‘x’", vec!["'x'"], "match_ambiguous"),
        ("‘x’|‚x‛", vec!["'x'"], "match_ambiguous"),
        ("ａａａ", vec!["aa"], "match_ambiguous"),
        ("a\u{301}a\u{301}a\u{301}", vec!["áá"], "match_ambiguous"),
        ("ﬁ|fi", vec!["fi"], "match_ambiguous"),
        ("café", vec!["café", "cafe\u{301}"], "invalid_arguments"),
        ("‘word’", vec!["'word'", "‘word’"], "invalid_arguments"),
        ("abcdef", vec!["bcd", "cde"], "match_overlap"),
        ("ﬁXY", vec!["fiX", "XY"], "match_overlap"),
        ("a b", vec!["a ", " b"], "match_overlap"),
        ("ﬃ", vec!["ff"], "match_boundary"),
        ("ﬁ", vec!["i"], "match_boundary"),
        ("a\u{315}\u{300}", vec!["à"], "match_boundary"),
        ("x\u{301}", vec!["x"], "match_boundary"),
        ("a \u{301}b", vec!["a "], "match_boundary"),
        ("start\tend", vec!["\t"], "invalid_arguments"),
        ("start\u{2003}end", vec!["\u{2003}"], "invalid_arguments"),
        ("  first\n  second", vec!["first\nsecond"], "match_missing"),
        ("line\nnext", vec!["line\\nnext"], "match_missing"),
        ("first second", vec!["first", "missing"], "match_missing"),
    ] {
        fs::write(&path, source)?;
        let read = fixture.read("reject").await?;
        let edits: Vec<_> = anchors
            .into_iter()
            .map(|old_text| json!({"old_text":old_text,"new_text":"replacement"}))
            .collect();
        failure(
            fixture
                .call("cocoa.edit", json!({"file":read["file"],"edits":edits}))
                .await,
            code,
            Effects::None,
        );
        assert_eq!(fs::read(&path)?, source.as_bytes(), "source {source:?}");
    }
    fixture.environment.close().await?;
    Ok(())
}

#[tokio::test]
async fn edit_newline_equivalence_preserves_mixed_endings_and_complete_crlf_units() -> TestResult {
    let fixture = Fixture::new(limits()).await?;
    let path = fixture.directory.path().join("endings");
    for (source, anchor, replacement, expected) in [
        ("left\r\nright", "\nright", "\ntail", "left\r\ntail"),
        ("left\r\nright", "left\r", "head\n", "head\r\nright"),
        (
            "first\nsecond\r\nlast",
            "second",
            "α\rβ\r\nγ\nδ",
            "first\nα\nβ\nγ\nδ\r\nlast",
        ),
        (
            "first\rsecond\nlast",
            "second",
            "α\rβ\r\nγ\nδ",
            "first\rα\rβ\rγ\rδ\nlast",
        ),
        ("none", "none", "α\rβ\r\nγ\nδ", "α\nβ\nγ\nδ"),
        ("X  \nY", "X ", "Z", "Z \nY"),
        (
            "‘X’ \t\r\nY\runtouched",
            "'X'\n",
            "Z\n",
            "Z\r\nY\runtouched",
        ),
        ("‘X’\u{2003}", "'X'", "ﬁ　‘new’\\n", "ﬁ　‘new’\\n\u{2003}"),
    ] {
        fs::write(&path, source)?;
        let read = fixture.read("endings").await?;
        fixture
            .call(
                "cocoa.edit",
                json!({"file":read["file"],"edits":[{"old_text":anchor,"new_text":replacement}]}),
            )
            .await?;
        assert_eq!(fs::read(&path)?, expected.as_bytes(), "source {source:?}");
    }
    fixture.environment.close().await?;
    Ok(())
}

#[tokio::test]
async fn edit_schema_is_object_only_and_64_edits_consume_one_fresh_observation() -> TestResult {
    let fixture = Fixture::new(CodingLimits {
        observations: 2,
        ..limits()
    })
    .await?;
    let path = fixture.directory.path().join("many");
    let source: String = (0..64).map(|index| format!("key-{index:02}!\n")).collect();
    fs::write(&path, &source)?;
    let read = fixture.read("many").await?;
    for arguments in [
        json!({"file":read["file"],"old_text":"key-00!","new_text":"legacy"}),
        json!({"file":read["file"],"edits":[]}),
        json!({"file":read["file"],"edits":vec![json!({"old_text":"key-00!","new_text":"x"});65]}),
        json!({"file":read["file"],"edits":null}),
        json!({"file":read["file"],"edits":{ "old_text":"key-00!","new_text":"x"}}),
        json!({"file":read["file"],"edits":[{"old_text":"key-00!","new_text":null}]}),
        json!({"file":read["file"],"edits":[{"old_text":"key-00!","new_text":"x","extra":1}]}),
        json!({"file":read["file"],"edits":[["key-00!","x"]]}),
        json!({"file":read["file"],"edits":[{"old_text":"key-00!","new_text":"x"}],"old_text":"legacy"}),
    ] {
        failure(
            fixture.call("cocoa.edit", arguments).await,
            "invalid_arguments",
            Effects::None,
        );
        assert_eq!(fs::read(&path)?, source.as_bytes());
    }
    for (method, item) in [
        ("cocoa.read", json!({"path":"many"})),
        ("cocoa.write", json!({"file":read["file"],"content":"no"})),
        (
            "cocoa.edit",
            json!({"file":read["file"],"edits":[{"old_text":"key-00!","new_text":"no"}]}),
        ),
        ("cocoa.run", json!({"command":"touch unexpected"})),
    ] {
        failure(
            fixture.call(method, json!([item])).await,
            "invalid_arguments",
            Effects::None,
        );
    }
    assert!(!fixture.directory.path().join("unexpected").exists());
    let edits: Vec<_> = (0..64).rev().map(|index| json!({"old_text":format!("key-{index:02}!"),"new_text":format!("replacement-{index}")})).collect();
    let edited = fixture
        .call("cocoa.edit", json!({"file":read["file"],"edits":edits}))
        .await?;
    let expected: String = (0..64)
        .map(|index| format!("replacement-{index}\n"))
        .collect();
    assert_eq!(fs::read(&path)?, expected.as_bytes());
    assert_eq!(edited["bytes"], expected.len());
    assert_eq!(
        fixture
            .call(
                "cocoa.read",
                json!({"file":edited["file"],"start_line":1,"line_count":200})
            )
            .await?["page"]["text"],
        expected
    );
    assert_eq!(
        fixture
            .call(
                "cocoa.read",
                json!({"file":read["file"],"start_line":1,"line_count":200})
            )
            .await?["page"]["text"],
        source
    );
    failure(fixture.read("many").await, "capacity", Effects::None);
    fixture.environment.close().await?;
    Ok(())
}

#[tokio::test]
async fn normalized_edits_keep_scoping_and_stale_checks_without_clobbering() -> TestResult {
    let fixture = Fixture::new(limits()).await?;
    let path = fixture.directory.path().join("stale");
    fs::write(&path, "‘first’ second")?;
    let read = fixture.read("stale").await?;
    let arguments = json!({"file":read["file"],"edits":[{"old_text":"'first'","new_text":"changed"},{"old_text":"second","new_text":"last"}]});
    let other = CodingCapabilities::new(fixture.environment.clone());
    failure(
        other
            .call(
                "cocoa.edit",
                arguments.clone(),
                work(4096),
                CancellationToken::new(),
            )
            .await,
        "invalid_reference",
        Effects::None,
    );
    fs::write(&path, "external writer")?;
    failure(
        fixture.call("cocoa.edit", arguments.clone()).await,
        "stale_read",
        Effects::None,
    );
    assert_eq!(fs::read_to_string(&path)?, "external writer");
    fs::remove_file(&path)?;
    failure(
        fixture.call("cocoa.edit", arguments).await,
        "stale_read",
        Effects::None,
    );
    assert!(!path.exists());
    let missing = fixture.read("never-created/leaf").await?;
    failure(
        fixture
            .call(
                "cocoa.edit",
                json!({"file":missing["file"],"edits":[{"old_text":"x","new_text":"y"}]}),
            )
            .await,
        "invalid_reference",
        Effects::None,
    );
    assert!(!fixture.directory.path().join("never-created").exists());
    fixture.environment.close().await?;
    Ok(())
}

#[tokio::test]
async fn normalized_edits_reserve_file_observation_and_response_capacity_before_commit()
-> TestResult {
    let fixture = Fixture::new(CodingLimits {
        file_bytes: 20,
        observed_bytes: 24,
        captured_output_bytes: 20,
        ..limits()
    })
    .await?;
    let path = fixture.directory.path().join("bounded");
    fs::write(&path, "abcdefghijklmnop")?;
    let read = fixture.read("bounded").await?;
    for replacement in ["123456789", "012345678901234567890"] {
        failure(fixture.call("cocoa.edit", json!({"file":read["file"],"edits":[{"old_text":"abcdefghijklmnop","new_text":replacement}]})).await, "capacity", Effects::None);
        assert_eq!(fs::read_to_string(&path)?, "abcdefghijklmnop");
    }
    let arguments =
        json!({"file":read["file"],"edits":[{"old_text":"abcdefghijklmnop","new_text":"new"}]});
    let small = fixture
        .host
        .call(
            "cocoa.edit",
            arguments.clone(),
            work(130),
            CancellationToken::new(),
        )
        .await;
    assert!(
        matches!(small, Err(CapabilityError::OutputLimit)),
        "{small:?}"
    );
    assert_eq!(fs::read_to_string(&path)?, "abcdefghijklmnop");
    fixture.call("cocoa.edit", arguments).await?;
    assert_eq!(fs::read_to_string(&path)?, "new");
    fixture.environment.close().await?;

    let fixture = Fixture::new(CodingLimits {
        file_bytes: 12,
        observed_bytes: 24,
        captured_output_bytes: 12,
        ..limits()
    })
    .await?;
    let path = fixture.directory.path().join("final-size");
    fs::write(&path, "abc XYZ12345")?;
    let read = fixture.read("final-size").await?;
    fixture
        .call(
            "cocoa.edit",
            json!({"file":read["file"],"edits":[
                {"old_text":"abc","new_text":"replacement"},
                {"old_text":"XYZ12345","new_text":""}
            ]}),
        )
        .await?;
    assert_eq!(fs::read_to_string(&path)?, "replacement ");
    fixture.environment.close().await?;
    Ok(())
}

#[tokio::test]
async fn whole_file_write_is_verbatim_even_after_a_crlf_observation() -> TestResult {
    let fixture = Fixture::new(limits()).await?;
    let path = fixture.directory.path().join("verbatim");
    fs::write(&path, "original\r\n")?;
    let read = fixture.read("verbatim").await?;
    let content = "\u{feff}‘cafe\u{301}’　\r\nﬃ\rfullwidth Ａ\nlast\t\u{2003}";
    let written = fixture
        .call(
            "cocoa.write",
            json!({"file":read["file"],"content":content}),
        )
        .await?;
    assert_eq!(fs::read(&path)?, content.as_bytes());
    assert_eq!(written["bytes"], content.len());
    let missing = fixture.read("new-verbatim").await?;
    fixture
        .call(
            "cocoa.write",
            json!({"file":missing["file"],"content":content}),
        )
        .await?;
    assert_eq!(
        fs::read(fixture.directory.path().join("new-verbatim"))?,
        content.as_bytes()
    );
    fixture.environment.close().await?;
    Ok(())
}

#[tokio::test]
async fn normalization_input_mapping_and_source_unit_limits_reject_without_effects() -> TestResult {
    let fixture = Fixture::new(CodingLimits {
        file_bytes: 9 * 1024 * 1024,
        observed_bytes: 24 * 1024 * 1024,
        ..limits()
    })
    .await?;
    let path = fixture.directory.path().join("normalization-limit");
    for source in [
        "\n".repeat(8 * 1024 * 1024 + 1),
        "\n".repeat(1024 * 1024 + 1),
        format!("a{}", "\u{301}".repeat(2048)),
        "ﷺ".repeat(255_000),
    ] {
        let source = format!("page\n{source}");
        fs::write(&path, &source)?;
        let read = fixture
            .call(
                "cocoa.read",
                json!({"path":"normalization-limit","line_count":1}),
            )
            .await?;
        failure(fixture.call("cocoa.edit", json!({"file":read["file"],"edits":[{"old_text":"not present","new_text":"new"}]})).await, "capacity", Effects::None);
        assert_eq!(fs::read(&path)?, source.as_bytes());
    }
    fixture.environment.close().await?;
    Ok(())
}

#[tokio::test]
async fn normalization_accepts_the_source_unit_byte_and_mapping_count_boundaries() -> TestResult {
    let fixture = Fixture::new(CodingLimits {
        file_bytes: 2 * 1024 * 1024,
        ..limits()
    })
    .await?;
    let path = fixture.directory.path().join("inclusive");
    let source = format!("é{}", "\u{301}".repeat(2047));
    assert_eq!(source.len(), 4096);
    fs::write(&path, &source)?;
    let read = fixture.read("inclusive").await?;
    fixture
        .call(
            "cocoa.edit",
            json!({"file":read["file"],"edits":[{"old_text":source,"new_text":"X"}]}),
        )
        .await?;
    assert_eq!(fs::read_to_string(&path)?, "X");
    let source = format!("{}X", "\n".repeat(1024 * 1024 - 1));
    fs::write(&path, &source)?;
    let read = fixture
        .call("cocoa.read", json!({"path":"inclusive","line_count":1}))
        .await?;
    fixture
        .call(
            "cocoa.edit",
            json!({"file":read["file"],"edits":[{"old_text":"X","new_text":"Y"}]}),
        )
        .await?;
    assert_eq!(
        fs::read_to_string(&path)?,
        format!("{}Y", "\n".repeat(1024 * 1024 - 1))
    );
    fixture.environment.close().await?;
    Ok(())
}

#[tokio::test]
async fn cancelled_and_expired_normalized_edits_never_commit() -> TestResult {
    let fixture = Fixture::new(limits()).await?;
    let path = fixture.directory.path().join("cancelled-edit");
    let original = format!("first\n{}\ntarget", "x".repeat(512 * 1024));
    fs::write(&path, &original)?;
    let read = fixture
        .call(
            "cocoa.read",
            json!({"path":"cancelled-edit","line_count":1}),
        )
        .await?;
    let arguments =
        json!({"file":read["file"],"edits":[{"old_text":"target","new_text":"changed"}]});
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(matches!(
        fixture
            .host
            .call("cocoa.edit", arguments.clone(), work(4096), cancelled)
            .await,
        Err(CapabilityError::Cancelled)
    ));
    assert_eq!(fs::read(&path)?, original.as_bytes());
    let expired = WorkLimits {
        output_bytes: 4096,
        deadline: tokio::time::Instant::now(),
    };
    assert!(matches!(
        fixture
            .host
            .call("cocoa.edit", arguments, expired, CancellationToken::new())
            .await,
        Err(CapabilityError::Deadline)
    ));
    assert_eq!(fs::read(&path)?, original.as_bytes());
    fixture.environment.close().await?;
    Ok(())
}

#[tokio::test]
async fn write_and_edit_return_fresh_snapshots_and_preserve_surroundings_and_permissions()
-> TestResult {
    let fixture = Fixture::new(limits()).await?;
    let path = fixture.directory.path().join("source");
    fs::write(&path, "before\r\nλtarget🦀\r\nafter")?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o751))?;
    let original = fixture.read("source").await?;
    let edited = fixture
        .call(
            "cocoa.edit",
            json!({"file":original["file"],"edits":[{"old_text":"target","new_text":"changed"}]}),
        )
        .await?;
    assert_ne!(original["file"], edited["file"]);
    assert_eq!(fs::read(&path)?, "before\r\nλchanged🦀\r\nafter".as_bytes());
    assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o751);
    assert_eq!(
        fixture
            .call(
                "cocoa.read",
                json!({"file":original["file"],"start_line":1,"line_count":200})
            )
            .await?["page"]["text"],
        "before\r\nλtarget🦀\r\nafter"
    );
    let replaced = fixture
        .call(
            "cocoa.write",
            json!({"file":edited["file"],"content":"next"}),
        )
        .await?;
    assert_eq!(replaced["bytes"], 4);
    assert_eq!(fs::read_to_string(&path)?, "next");
    failure(
        fixture
            .call(
                "cocoa.write",
                json!({"file":original["file"],"content":"stale"}),
            )
            .await,
        "stale_read",
        Effects::None,
    );
    fixture.environment.close().await?;
    Ok(())
}

#[tokio::test]
async fn edits_reject_zero_multiple_and_overlapping_matches_without_effects() -> TestResult {
    let fixture = Fixture::new(limits()).await?;
    for (content, anchor, code) in [
        ("abc", "missing", "match_missing"),
        ("x-y-x", "x", "match_ambiguous"),
        ("aaa", "aa", "match_ambiguous"),
        ("ééé", "éé", "match_ambiguous"),
    ] {
        fs::write(fixture.directory.path().join("source"), content)?;
        let read = fixture.read("source").await?;
        failure(
            fixture
                .call(
                    "cocoa.edit",
                    json!({"file":read["file"],"edits":[{"old_text":anchor,"new_text":"new"}]}),
                )
                .await,
            code,
            Effects::None,
        );
        assert_eq!(
            fs::read_to_string(fixture.directory.path().join("source"))?,
            content
        );
    }
    let read = fixture.read("source").await?;
    failure(
        fixture
            .call(
                "cocoa.edit",
                json!({"file":read["file"],"edits":[{"old_text":"","new_text":"new"}]}),
            )
            .await,
        "invalid_arguments",
        Effects::None,
    );
    fixture.environment.close().await?;
    Ok(())
}

#[tokio::test]
async fn stale_absence_and_changed_inode_or_bytes_never_clobber() -> TestResult {
    let fixture = Fixture::new(limits()).await?;
    let missing = fixture.read("new").await?;
    fs::write(fixture.directory.path().join("new"), "external")?;
    failure(
        fixture
            .call(
                "cocoa.write",
                json!({"file":missing["file"],"content":"clobber"}),
            )
            .await,
        "stale_read",
        Effects::None,
    );
    assert_eq!(
        fs::read_to_string(fixture.directory.path().join("new"))?,
        "external"
    );
    let original = fixture.read("new").await?;
    let path = fixture.directory.path().join("new");
    let metadata = fs::metadata(&path)?;
    let replacement = fixture.directory.path().join("replacement");
    fs::write(&replacement, "external")?;
    fs::File::open(&replacement)?
        .set_times(fs::FileTimes::new().set_modified(metadata.modified()?))?;
    fs::rename(replacement, &path)?;
    assert_ne!(fs::metadata(&path)?.ino(), metadata.ino());
    assert_eq!(fs::metadata(&path)?.len(), metadata.len());
    assert_eq!(fs::metadata(&path)?.modified()?, metadata.modified()?);
    failure(
        fixture
            .call(
                "cocoa.write",
                json!({"file":original["file"],"content":"clobber"}),
            )
            .await,
        "stale_read",
        Effects::None,
    );
    assert_eq!(fs::read_to_string(&path)?, "external");
    let original = fixture.read("new").await?;
    let metadata = fs::metadata(&path)?;
    fs::write(&path, "modified")?;
    fs::File::open(&path)?.set_times(fs::FileTimes::new().set_modified(metadata.modified()?))?;
    assert_eq!(fs::metadata(&path)?.ino(), metadata.ino());
    assert_eq!(fs::metadata(&path)?.len(), metadata.len());
    assert_eq!(fs::metadata(&path)?.modified()?, metadata.modified()?);
    failure(
        fixture
            .call(
                "cocoa.write",
                json!({"file":original["file"],"content":"clobber"}),
            )
            .await,
        "stale_read",
        Effects::None,
    );
    assert_eq!(fs::read_to_string(&path)?, "modified");
    let missing = fixture.read("created").await?;
    let created = fixture
        .call(
            "cocoa.write",
            json!({"file":missing["file"],"content":"created"}),
        )
        .await?;
    assert_eq!(created["kind"], "text");
    assert_eq!(
        fs::read_to_string(fixture.directory.path().join("created"))?,
        "created"
    );
    fixture.environment.close().await?;
    Ok(())
}

#[tokio::test]
async fn missing_targets_create_parents_only_after_reserving_capacity_and_report_partial_effects()
-> TestResult {
    let fixture = Fixture::new(limits()).await?;
    let missing = fixture.read("new/nested/leaf").await?;
    assert_eq!(missing["kind"], "missing");
    assert!(!fixture.directory.path().join("new").exists());
    let small = fixture
        .host
        .call(
            "cocoa.write",
            json!({"file":missing["file"],"content":"new"}),
            work(130),
            CancellationToken::new(),
        )
        .await;
    assert!(
        matches!(small, Err(CapabilityError::OutputLimit)),
        "{small:?}"
    );
    assert!(!fixture.directory.path().join("new").exists());
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(matches!(
        fixture
            .host
            .call(
                "cocoa.write",
                json!({"file":missing["file"],"content":"new"}),
                work(65536),
                cancelled
            )
            .await,
        Err(CapabilityError::Cancelled)
    ));
    assert!(!fixture.directory.path().join("new").exists());
    let created = fixture
        .call(
            "cocoa.write",
            json!({"file":missing["file"],"content":"λ\r\nlast"}),
        )
        .await?;
    assert!(created.get("page").is_none());
    assert_eq!(
        fs::read(fixture.directory.path().join("new/nested/leaf"))?,
        "λ\r\nlast".as_bytes()
    );
    assert_eq!(
        fixture
            .call(
                "cocoa.read",
                json!({"file":missing["file"],"start_line":1,"line_count":200})
            )
            .await?["kind"],
        "missing"
    );
    fs::remove_file(fixture.directory.path().join("new/nested/leaf"))?;
    failure(
        fixture
            .call(
                "cocoa.write",
                json!({"file":created["file"],"content":"must not recreate"}),
            )
            .await,
        "stale_read",
        Effects::None,
    );
    assert!(!fixture.directory.path().join("new/nested/leaf").exists());

    let path = format!("partial/{}/leaf", "x".repeat(256));
    let missing = fixture.read(&path).await?;
    failure(
        fixture
            .call(
                "cocoa.write",
                json!({"file":missing["file"],"content":"no leaf"}),
            )
            .await,
        "storage",
        Effects::Partial,
    );
    assert!(fixture.directory.path().join("partial").is_dir());
    assert_eq!(
        fs::read_dir(fixture.directory.path().join("partial"))?.count(),
        0
    );
    fixture.environment.close().await?;

    let fixture = Fixture::new(CodingLimits {
        observations: 1,
        ..limits()
    })
    .await?;
    let missing = fixture.read("quota/leaf").await?;
    failure(
        fixture
            .call(
                "cocoa.write",
                json!({"file":missing["file"],"content":"no effects"}),
            )
            .await,
        "capacity",
        Effects::None,
    );
    assert!(!fixture.directory.path().join("quota").exists());
    fixture.environment.close().await?;
    Ok(())
}

#[tokio::test]
async fn quotas_and_result_capacity_fail_before_mutation_and_leave_no_partial_file() -> TestResult {
    let fixture = Fixture::new(CodingLimits {
        file_bytes: 20,
        observed_bytes: 24,
        captured_output_bytes: 20,
        ..limits()
    })
    .await?;
    fs::write(fixture.directory.path().join("source"), "abcdefghijklmnop")?;
    let original = fixture.read("source").await?;
    failure(
        fixture
            .call(
                "cocoa.write",
                json!({"file":original["file"],"content":"123456789"}),
            )
            .await,
        "capacity",
        Effects::None,
    );
    assert_eq!(
        fs::read_to_string(fixture.directory.path().join("source"))?,
        "abcdefghijklmnop"
    );
    failure(
        fixture
            .call(
                "cocoa.write",
                json!({"file":original["file"],"content":"x".repeat(21)}),
            )
            .await,
        "capacity",
        Effects::None,
    );
    let small = fixture
        .host
        .call(
            "cocoa.write",
            json!({"file":original["file"],"content":"new"}),
            work(130),
            CancellationToken::new(),
        )
        .await;
    assert!(matches!(small, Err(CapabilityError::OutputLimit)));
    assert_eq!(
        fs::read_to_string(fixture.directory.path().join("source"))?,
        "abcdefghijklmnop"
    );
    assert!(
        !fs::read_dir(fixture.directory.path())?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .any(|entry| entry.file_name().to_string_lossy().starts_with(".tmp"))
    );
    fixture.environment.close().await?;
    Ok(())
}

#[tokio::test]
async fn preparation_failure_never_truncates_an_existing_writable_file() -> TestResult {
    let fixture = Fixture::new(limits()).await?;
    let parent = fixture.directory.path().join("readonly");
    fs::create_dir(&parent)?;
    fs::write(parent.join("source"), "original writable file")?;
    let original = fixture.read("readonly/source").await?;
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o500))?;
    let probe = tempfile::NamedTempFile::new_in(&parent);
    if probe.is_err() {
        failure(
            fixture
                .call(
                    "cocoa.write",
                    json!({"file":original["file"],"content":"must not truncate"}),
                )
                .await,
            "permission_denied",
            Effects::None,
        );
        assert_eq!(
            fs::read_to_string(parent.join("source"))?,
            "original writable file"
        );
    }
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o700))?;
    drop(probe);
    fixture.environment.close().await?;
    Ok(())
}

#[tokio::test]
async fn run_separates_streams_status_stdin_cwd_and_explicit_environment() -> TestResult {
    let fixture = Fixture::new(limits()).await?;
    fs::create_dir(fixture.directory.path().join("child"))?;
    let result = fixture.call("cocoa.run", json!({"command":"read -r input; printf '%s|%s|%s|%s' \"$input\" \"$SELECTED\" \"${HOME-unset}\" \"${REMOVED-unset}\"; printf 'error' >&2; printf '%s' \"$PWD\" > where; exit 17", "working_directory":"child", "stdin":"from stdin\n", "environment":{"SELECTED":"override","REMOVED":null}})).await?;
    assert_eq!(result["status"], json!({"kind":"exited","code":17}));
    assert_eq!(
        result["stdout"],
        json!({"text":"from stdin|override|unset|unset","truncated":false,"lossy":false,"output_path":null})
    );
    assert_eq!(result["stderr"]["text"], "error");
    assert_eq!(
        fs::read_to_string(fixture.directory.path().join("child/where"))?,
        fs::canonicalize(fixture.directory.path().join("child"))?.to_string_lossy()
    );
    let signalled = fixture
        .call("cocoa.run", json!({"command":"kill -TERM $$"}))
        .await?;
    assert_eq!(signalled["status"], json!({"kind":"signalled","signal":15}));
    fixture.environment.close().await?;
    Ok(())
}

#[tokio::test]
async fn run_defaults_to_pipefail_without_errexit_or_nounset_and_keeps_shell_scope() -> TestResult {
    let fixture = Fixture::new(limits()).await?;
    fs::write(
        fixture.directory.path().join("child.sh"),
        "#!/bin/bash\n(exit 7) | cat\n",
    )?;
    for (command, code, stdout) in [
        ("(exit 7) | cat", 7, ""),
        ("printf success | cat", 0, "success"),
        ("yes payload | head -1", 141, "payload\n"),
        ("set +o pipefail; (exit 7) | cat", 0, ""),
        ("(exit 7) | cat; printf later", 0, "later"),
        ("((exit 7) | cat)", 7, ""),
        ("/bin/bash --noprofile --norc child.sh", 0, ""),
        ("/bin/bash --noprofile --norc -o pipefail child.sh", 7, ""),
        ("[[ $- != *e* && $- != *u* ]] && shopt -qo pipefail", 0, ""),
        (
            "false; printf '%s' \"$MISSING\"; printf survived",
            0,
            "survived",
        ),
    ] {
        let result = fixture
            .call("cocoa.run", json!({"command":command}))
            .await?;
        assert_eq!(
            result["status"],
            json!({"kind":"exited","code":code}),
            "{command}: {result}"
        );
        assert_eq!(result["stdout"]["text"], stdout, "{command}");
    }
    fixture.environment.close().await?;
    Ok(())
}

#[tokio::test]
async fn run_retains_raw_overflow_and_bounds_both_previews() -> TestResult {
    let fixture = Fixture::new(limits()).await?;
    let result = fixture.call("cocoa.run", json!({"command":"for ((i=1;i<=201;i++)); do printf 'out-%s\\r\\n' \"$i\"; done; printf '\\377err' >&2"})).await?;
    assert_eq!(result["status"], json!({"kind":"exited","code":0}));
    let stdout = result["stdout"]["text"].as_str().ok_or("missing stdout")?;
    assert_eq!(stdout.split_inclusive('\n').count(), 200);
    assert!(stdout.ends_with("out-200\r\n"));
    assert_eq!(result["stdout"]["truncated"], true);
    assert_eq!(result["stderr"]["lossy"], true);
    assert_eq!(result["stderr"]["text"], "�err");
    assert_eq!(result["stderr"]["output_path"], Value::Null);
    let path = result["stdout"]["output_path"]
        .as_str()
        .ok_or("missing overflow path")?;
    let raw = fs::read_to_string(path)?;
    assert!(raw.ends_with("out-201\r\n"));
    let retained = fixture.read(path).await?;
    assert_eq!(retained["line_count"], 201);
    assert_eq!(
        fixture.call("cocoa.read", retained["next"].clone()).await?["page"]["text"],
        "out-201\r\n"
    );
    let large = run_to_completion(
        &fixture.host,
        json!({"command":"printf '%4000s' A; printf '%3000s' B >&2"}),
        work(2048),
    )
    .await?;
    assert!(host_value_fits(&large, 2048));
    for stream in ["stdout", "stderr"] {
        assert_eq!(large[stream]["truncated"], true);
        assert!(
            large[stream]["text"]
                .as_str()
                .is_some_and(|text| !text.is_empty())
        );
        let path = large[stream]["output_path"]
            .as_str()
            .ok_or("missing overflow path")?;
        assert_eq!(
            fs::metadata(path)?.len(),
            if stream == "stdout" { 4000 } else { 3000 }
        );
    }
    fixture.environment.close().await?;
    assert!(!PathBuf::from(path).exists());
    Ok(())
}

#[tokio::test]
async fn spawn_storage_and_quota_failures_preserve_effect_evidence() -> TestResult {
    let directory = tempfile::tempdir()?;
    let mut configuration = configuration(&directory);
    configuration.shell = directory.path().join("not-bash");
    let environment = Arc::new(LocalEnvironment::new(configuration, limits()).await?);
    let host = CodingCapabilities::new(environment.clone());
    failure(
        host.call(
            "cocoa.run",
            json!({"command":"printf bad > started"}),
            work(4096),
            CancellationToken::new(),
        )
        .await,
        "process_start",
        Effects::None,
    );
    assert!(!directory.path().join("started").exists());
    environment.close().await?;

    let fixture = Fixture::new(CodingLimits {
        captured_output_bytes: 100,
        retained_output_bytes: 100,
        preview_lines: 1,
        ..limits()
    })
    .await?;
    let first = fixture
        .call("cocoa.run", json!({"command":"printf 'first\\nsecond\\n'"}))
        .await?;
    let path = first["stdout"]["output_path"]
        .as_str()
        .ok_or("expected retained output")?;
    failure(
        fixture
            .call("cocoa.run", json!({"command":"printf clobber > forbidden"}))
            .await,
        "capacity",
        Effects::None,
    );
    assert!(!fixture.directory.path().join("forbidden").exists());
    assert_eq!(fs::read_to_string(path)?, "first\nsecond\n");
    fixture.environment.close().await?;

    let fixture = Fixture::new(CodingLimits {
        captured_output_bytes: 100,
        ..limits()
    })
    .await?;
    failure(
        fixture
            .call(
                "cocoa.run",
                json!({"command":"printf 'effect' > changed; printf '%70s' x; printf '%70s' y >&2"}),
            )
            .await,
        "storage",
        Effects::Partial,
    );
    assert_eq!(
        fs::read_to_string(fixture.directory.path().join("changed"))?,
        "effect"
    );
    // A leaked reservation would reject this second command.
    assert_eq!(
        fixture
            .call("cocoa.run", json!({"command":"printf ok"}))
            .await?["stdout"]["text"],
        "ok"
    );
    fixture.environment.close().await?;
    Ok(())
}

#[tokio::test]
async fn exited_background_jobs_do_not_turn_success_into_cleanup_failure() -> TestResult {
    let fixture = Fixture::new(limits()).await?;
    let mut unrelated = std::process::Command::new("/bin/sleep").arg("30").spawn()?;
    let result = async {
        for _ in 0..32 {
            let result = fixture.call("cocoa.run", json!({"command":"/bin/bash --noprofile --norc -c 'trap \"exit 0\" TERM; printf ready > ready; while :; do :; done' & child=$!; while ! test -f ready; do sleep 0.005; done; rm ready; kill $child; printf completed"})).await?;
            assert_eq!(result["status"], json!({"kind":"exited","code":0}));
            assert_eq!(result["stdout"]["text"], "completed");
            assert!(unrelated.try_wait()?.is_none());
        }
        fixture.environment.close().await?;
        Ok::<_, Box<dyn Error>>(())
    }.await;
    unrelated.kill()?;
    unrelated.wait()?;
    result
}

#[tokio::test]
async fn unconfirmed_group_cleanup_is_bounded_and_keeps_unknown_effects() -> TestResult {
    let fixture = Fixture::new(limits()).await?;
    let receipt = fixture
        .host
        .call(
            "cocoa.run",
            json!({"command":"printf '%s' $$ > running.tmp && printf effect > changed && mv running.tmp running && sleep 30"}),
            work(4096),
            CancellationToken::new(),
        )
        .await?;
    wait_for(fixture.directory.path().join("running")).await?;
    let root = fs::read_to_string(fixture.directory.path().join("running"))?;
    // Keep our own child unreaped in the group so disappearance cannot be confirmed.
    let mut held = std::process::Command::new("/bin/sleep")
        .arg("30")
        .process_group(root.parse()?)
        .spawn()?;
    let before = tokio::time::Instant::now();
    assert_eq!(
        fixture
            .call("cocoa.cancel", json!({"reference":receipt["reference"]}))
            .await?["state"],
        "cancelling"
    );
    let result = fixture
        .call(
            "cocoa.wait",
            json!({"handles":{"command":receipt["reference"]}}),
        )
        .await;
    let held_status = held.wait()?;
    assert!(!held_status.success());
    assert!(before.elapsed() < Duration::from_secs(1));
    assert!(
        matches!(result, Err(CapabilityError::Unsettled { ref message }) if message.contains("disappearance not confirmed")),
        "{result:?}"
    );
    assert_eq!(
        fixture
            .host
            .activity_status(&OperationOutcome::Completed { value: receipt }),
        ActivityStatus::Unknown,
        "unconfirmed cleanup must not become a settled command outcome"
    );
    assert!(!process_exists(&root)?);
    assert_eq!(
        fs::read_to_string(fixture.directory.path().join("changed"))?,
        "effect"
    );
    assert!(matches!(
        fixture.environment.close().await,
        Err(CapabilityError::Unsettled { .. })
    ));
    assert!(matches!(
        fixture.environment.close().await,
        Err(CapabilityError::Unsettled { .. })
    ));
    Ok(())
}

#[tokio::test]
async fn process_limit_has_no_queue_and_cancellation_reaps_the_shell() -> TestResult {
    let fixture = Fixture::new(CodingLimits {
        processes: 1,
        ..limits()
    })
    .await?;
    let first = fixture
        .host
        .call(
            "cocoa.run",
            json!({"command":"printf '%s' $$ > running; sleep 30 & printf '%s' $! > child; wait"}),
            work(4096),
            CancellationToken::new(),
        )
        .await?;
    wait_for(fixture.directory.path().join("child")).await?;
    let before = tokio::time::Instant::now();
    failure(
        fixture
            .call("cocoa.run", json!({"command":"printf queued > forbidden"}))
            .await,
        "capacity",
        Effects::None,
    );
    assert!(before.elapsed() < Duration::from_secs(1));
    assert!(!fixture.directory.path().join("forbidden").exists());
    assert_eq!(
        fixture
            .call("cocoa.cancel", json!({"reference":first["reference"]}))
            .await?["state"],
        "cancelling"
    );
    let terminal = fixture
        .call(
            "cocoa.wait",
            json!({"handles":{"command":first["reference"]}}),
        )
        .await?;
    assert_eq!(terminal["command"]["status"]["kind"], "signalled");
    assert_eq!(
        fixture
            .call("cocoa.cancel", json!({"reference":first["reference"]}))
            .await?["state"],
        "completed"
    );
    let process = fs::read_to_string(fixture.directory.path().join("running"))?;
    assert!(!process_exists(&process)?);
    let descendant = fs::read_to_string(fixture.directory.path().join("child"))?;
    for _ in 0..100 {
        if !process_exists(&descendant)? {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(!process_exists(&descendant)?);
    fixture.environment.close().await?;
    Ok(())
}

#[tokio::test]
async fn command_timeout_preserves_bounded_incomplete_diagnostics_from_both_streams() -> TestResult
{
    let fixture = Fixture::new(CodingLimits {
        retained_output_bytes: 2 * limits().captured_output_bytes,
        processes: 1,
        ..limits()
    })
    .await?;
    for (command, stdout, stderr, truncated, lossy) in [
        (
            "printf '%s' $$ > running; printf effect > changed; printf 'build complete'; printf 'waiting for browser' >&2; sleep 30",
            "build complete",
            "waiting for browser",
            false,
            false,
        ),
        (
            "printf '%s' $$ > running; printf effect > changed; for ((i=0;i<1000;i++)); do printf 'α\\\"\\n'; printf '\\377\\t' >&2; done; printf 'stdout-tail-β'; printf 'stderr-tail-γ' >&2; sleep 30",
            "stdout-tail-β",
            "stderr-tail-γ",
            true,
            true,
        ),
    ] {
        let result = run_to_completion(
            &fixture.host,
            json!({"command":command,"timeout_ms":250}),
            work(2048),
        )
        .await;
        let Err(CapabilityError::Tool {
            code,
            message,
            effects,
        }) = &result
        else {
            return Err(format!("expected timeout, got {result:?}").into());
        };
        assert_eq!(code, "timeout");
        assert_eq!(*effects, Effects::Partial);
        assert!(message.contains("incomplete"), "{message}");
        assert!(message.contains(stdout), "{message}");
        assert!(message.contains(stderr), "{message}");
        assert!(host_value_fits(&json!(result.as_ref().err()), 1024));
        let (_, details) = message.split_once('\n').ok_or("missing diagnostics")?;
        let details: Value = serde_json::from_str(details)?;
        assert_eq!(details["stdout"]["truncated"], truncated);
        assert_eq!(details["stderr"]["truncated"], truncated);
        assert_eq!(details["stdout"]["lossy"], false);
        assert_eq!(details["stderr"]["lossy"], lossy);
        let process = fs::read_to_string(fixture.directory.path().join("running"))?;
        assert!(!process_exists(&process)?);
        assert_eq!(
            fs::read_to_string(fixture.directory.path().join("changed"))?,
            "effect"
        );
    }
    assert_eq!(
        fixture
            .call("cocoa.run", json!({"command":"printf settled"}))
            .await?["stdout"]["text"],
        "settled"
    );
    fixture.environment.close().await?;
    Ok(())
}

#[tokio::test]
async fn command_timeout_kills_but_wait_deadline_returns_a_pending_snapshot() -> TestResult {
    let fixture = Fixture::new(limits()).await?;
    failure(
        fixture
            .call(
                "cocoa.run",
                json!({"command":"printf effect > changed; sleep 30", "timeout_ms":50}),
            )
            .await,
        "timeout",
        Effects::Partial,
    );
    assert_eq!(
        fs::read_to_string(fixture.directory.path().join("changed"))?,
        "effect"
    );
    let receipt = fixture
        .host
        .call(
            "cocoa.run",
            json!({"command":"sleep 30", "timeout_ms":60000}),
            work(4096),
            CancellationToken::new(),
        )
        .await?;
    let deadline = WorkLimits {
        output_bytes: 4096,
        deadline: tokio::time::Instant::now() + Duration::from_millis(50),
    };
    let pending = fixture
        .host
        .call(
            "cocoa.wait",
            json!({"handles":{"command":receipt["reference"]}}),
            deadline,
            CancellationToken::new(),
        )
        .await?;
    assert_eq!(pending["command"]["status"], json!({"kind":"running"}));
    fixture
        .call("cocoa.cancel", json!({"reference":receipt["reference"]}))
        .await?;
    fixture
        .call(
            "cocoa.wait",
            json!({"handles":{"command":receipt["reference"]}}),
        )
        .await?;
    assert_eq!(
        fixture
            .call("cocoa.run", json!({"command":"printf settled"}))
            .await?["stdout"]["text"],
        "settled"
    );
    fixture.environment.close().await?;
    Ok(())
}

#[tokio::test]
async fn dropped_call_and_explicit_close_settle_workers_and_remove_owned_outputs() -> TestResult {
    let fixture = Fixture::new(limits()).await?;
    {
        let call = fixture.call(
            "cocoa.run",
            json!({"command":"printf '%s' $$ > running; sleep 30 & printf '%s' $! > child; wait; printf bad > late"}),
        );
        tokio::pin!(call);
        tokio::select! { result = &mut call => return Err(format!("command finished early: {result:?}").into()), result = wait_for(fixture.directory.path().join("child")) => result? }
    }
    fixture.environment.close().await?;
    fixture.environment.close().await?;
    let process = fs::read_to_string(fixture.directory.path().join("running"))?;
    assert!(!process_exists(&process)?);
    let descendant = fs::read_to_string(fixture.directory.path().join("child"))?;
    assert!(!process_exists(&descendant)?);
    assert!(!fixture.directory.path().join("late").exists());
    failure(fixture.read("running").await, "backend", Effects::None);
    assert_eq!(fs::read_dir(fixture.directory.path())?.count(), 2);
    Ok(())
}

#[tokio::test]
async fn file_worker_backend_failures_remain_visible_after_delivery_to_the_caller() -> TestResult {
    struct MisroutedEnvironment(Arc<LocalEnvironment>);

    #[async_trait]
    impl CodingEnvironment for MisroutedEnvironment {
        fn description(&self) -> String {
            self.0.description()
        }

        async fn execute(
            &self,
            _: CodingOperation,
            context: CodingContext,
        ) -> Result<Value, CapabilityError> {
            self.0
                .execute(
                    CodingOperation::LoadSkill {
                        id: "misrouted".into(),
                    },
                    context,
                )
                .await
        }
    }

    let fixture = Fixture::new(limits()).await?;
    let host = CodingCapabilities::new(Arc::new(MisroutedEnvironment(fixture.environment.clone())));
    let error = host
        .call(
            "cocoa.read",
            json!({"path":"unused"}),
            work(4096),
            CancellationToken::new(),
        )
        .await
        .err()
        .ok_or("invalid file dispatch unexpectedly succeeded")?;
    assert!(matches!(
        &error,
        CapabilityError::Tool { code, effects: Effects::None, .. } if code == "backend"
    ));
    assert_eq!(fixture.environment.close().await, Err(error.clone()));
    assert_eq!(fixture.environment.close().await, Err(error));
    Ok(())
}

#[tokio::test]
async fn process_receipts_and_pending_snapshots_retain_output_until_close() -> TestResult {
    let fixture = Fixture::new(CodingLimits {
        preview_lines: 1,
        ..limits()
    })
    .await?;
    let receipt = fixture.host.call("cocoa.run", json!({"command":"printf once >> executions; printf 'first\\nsecond\\n'; printf 'diagnostic' >&2; while ! test -f finish; do sleep 0.01; done; printf 'last\\n'"}), work(4096), CancellationToken::new()).await?;
    assert_eq!(
        receipt
            .as_object()
            .ok_or("receipt must be an object")?
            .len(),
        2
    );
    assert_eq!(receipt["kind"], "process");
    let reference = receipt["reference"].as_str().ok_or("missing reference")?;
    assert_eq!(reference.split('.').count(), 3);
    assert!(reference.starts_with("process."));
    let arguments = json!({"handles":{"build":reference},"timeout_ms":100});
    let pending = fixture.call("cocoa.wait", arguments.clone()).await?;
    assert_eq!(pending["build"]["status"], json!({"kind":"running"}));
    assert_eq!(pending["build"]["stdout"]["text"], "first\n");
    assert_eq!(pending["build"]["stderr"]["text"], "diagnostic");
    assert_eq!(pending["build"]["stdout"]["truncated"], true);
    let path = pending["build"]["stdout"]["output_path"]
        .as_str()
        .ok_or("missing retained pending output")?;
    assert_eq!(fs::read_to_string(path)?, "first\nsecond\n");
    let second = fixture
        .call(
            "cocoa.wait",
            json!({"handles":{"build":reference},"timeout_ms":0}),
        )
        .await?;
    assert_eq!(second, pending);
    fs::write(fixture.directory.path().join("finish"), "")?;
    let completed = fixture
        .call("cocoa.wait", json!({"handles":{"build":reference}}))
        .await?;
    assert_eq!(
        completed["build"]["status"],
        json!({"kind":"exited","code":0})
    );
    assert_eq!(completed["build"]["stdout"]["output_path"], path);
    assert_eq!(fs::read_to_string(path)?, "first\nsecond\nlast\n");
    assert_eq!(fixture.call("cocoa.wait", arguments).await?, completed);
    assert_eq!(
        fs::read_to_string(fixture.directory.path().join("executions"))?,
        "once"
    );
    fixture.environment.close().await?;
    assert!(!PathBuf::from(path).exists());
    Ok(())
}

#[tokio::test]
async fn pending_output_paths_survive_larger_terminal_previews_and_small_receipt_budgets()
-> TestResult {
    let fixture = Fixture::new(limits()).await?;
    let receipt = fixture
        .host
        .call(
            "cocoa.run",
            json!({"command":"printf '%4000s' x; while ! test -f finish; do sleep 0.01; done"}),
            work(256),
            CancellationToken::new(),
        )
        .await?;
    assert_eq!(receipt["kind"], "process");
    let arguments = json!({"handles":{"stdout":receipt["reference"]},"timeout_ms":100});
    let pending = fixture
        .host
        .call(
            "cocoa.wait",
            arguments.clone(),
            work(2048),
            CancellationToken::new(),
        )
        .await?;
    assert!(host_value_fits(&pending, 2048));
    assert_eq!(pending["stdout"]["status"]["kind"], "running");
    assert_eq!(pending["stdout"]["stdout"]["truncated"], true);
    let path = pending["stdout"]["stdout"]["output_path"]
        .as_str()
        .ok_or("missing raw output")?;
    assert_eq!(fs::read(path)?.len(), 4000);
    let activity = fixture
        .host
        .describe_output(
            "cocoa.wait",
            &arguments,
            &OperationOutcome::Completed {
                value: pending.clone(),
            },
        )
        .ok_or("missing wait activity output")?;
    assert_eq!(activity.text, pending["stdout"]["stdout"]["text"]);
    assert!(activity.truncated);
    assert_eq!(
        fixture.host.activity_status(&OperationOutcome::Completed {
            value: pending.clone()
        }),
        ActivityStatus::Running
    );
    assert_eq!(
        fixture.host.activity_status(&OperationOutcome::Completed {
            value: receipt.clone()
        }),
        ActivityStatus::Running,
        "accepting a command does not establish successful completion"
    );
    fs::write(fixture.directory.path().join("finish"), "")?;
    let terminal = fixture
        .call(
            "cocoa.wait",
            json!({"handles":{"stdout":receipt["reference"]}}),
        )
        .await?;
    assert_eq!(terminal["stdout"]["status"]["kind"], "exited");
    assert_eq!(terminal["stdout"]["stdout"]["truncated"], false);
    assert_eq!(terminal["stdout"]["stdout"]["output_path"], path);
    assert_eq!(
        fs::read_to_string(path)?,
        terminal["stdout"]["stdout"]["text"]
    );
    assert_eq!(
        fixture
            .host
            .activity_status(&OperationOutcome::Completed { value: terminal }),
        ActivityStatus::Completed
    );
    assert_eq!(
        fixture
            .host
            .activity_status(&OperationOutcome::Completed { value: receipt }),
        ActivityStatus::Completed
    );
    let failed = fixture
        .host
        .call(
            "cocoa.run",
            json!({"command":"printf failure >&2; exit 7"}),
            work(4096),
            CancellationToken::new(),
        )
        .await?;
    let arguments = json!({"handles":{"status":failed["reference"]}});
    let failed = fixture.call("cocoa.wait", arguments.clone()).await?;
    assert_eq!(
        fixture.host.activity_status(&OperationOutcome::Completed {
            value: failed.clone()
        }),
        ActivityStatus::Failed { exit_code: Some(7) }
    );
    let output = fixture
        .host
        .describe_output(
            "cocoa.wait",
            &arguments,
            &OperationOutcome::Completed { value: failed },
        )
        .ok_or("missing failed wait output")?;
    assert_eq!(output.text, "failure");
    fixture.environment.close().await?;
    Ok(())
}

#[tokio::test]
async fn process_references_require_the_issuing_scope_and_actual_registry_entry() -> TestResult {
    let fixture = Fixture::new(limits()).await?;
    let receipt = fixture
        .host
        .call(
            "cocoa.run",
            json!({"command":"printf valid"}),
            work(4096),
            CancellationToken::new(),
        )
        .await?;
    let other_scope = CodingCapabilities::new(fixture.environment.clone());
    let other_environment = Fixture::new(limits()).await?;
    let reference = receipt["reference"].as_str().ok_or("missing reference")?;
    for host in [&other_scope, &other_environment.host] {
        for (method, arguments) in [
            (
                "cocoa.wait",
                json!({"handles":{"build":reference},"timeout_ms":0}),
            ),
            ("cocoa.cancel", json!({"reference":reference})),
        ] {
            failure(
                host.call(method, arguments, work(4096), CancellationToken::new())
                    .await,
                "invalid_reference",
                Effects::None,
            );
        }
    }
    for forged in [
        format!("{reference}0"),
        "process.0.0".into(),
        "file_1".into(),
        "1".into(),
    ] {
        failure(
            fixture
                .call(
                    "cocoa.wait",
                    json!({"handles":{"build":forged},"timeout_ms":0}),
                )
                .await,
            "invalid_reference",
            Effects::None,
        );
        failure(
            fixture
                .call("cocoa.cancel", json!({"reference":forged}))
                .await,
            "invalid_reference",
            Effects::None,
        );
    }
    assert_eq!(
        fixture
            .call("cocoa.wait", json!({"handles":{"build":reference}}))
            .await?["build"]["stdout"]["text"],
        "valid"
    );
    fixture.environment.close().await?;
    other_environment.environment.close().await?;
    Ok(())
}

#[tokio::test]
async fn command_lifetime_is_independent_of_run_and_wait_callers() -> TestResult {
    let fixture = Fixture::new(limits()).await?;
    let run_cancellation = CancellationToken::new();
    let receipt = fixture.host.call("cocoa.run", json!({"command":"printf '%s' $$ > running; while ! test -f finish; do sleep 0.01; done; printf survived"}), WorkLimits { output_bytes:4096, deadline:tokio::time::Instant::now() + Duration::from_millis(100) }, run_cancellation.clone()).await?;
    run_cancellation.cancel();
    wait_for(fixture.directory.path().join("running")).await?;
    let wait_cancellation = CancellationToken::new();
    let arguments = json!({"handles":{"build":receipt["reference"]}});
    let waiting = fixture.host.call(
        "cocoa.wait",
        arguments.clone(),
        work(4096),
        wait_cancellation.clone(),
    );
    let cancel_wait = async {
        tokio::time::sleep(Duration::from_millis(30)).await;
        wait_cancellation.cancel();
    };
    let (result, ()) = tokio::join!(waiting, cancel_wait);
    assert!(matches!(result, Err(CapabilityError::Cancelled)));
    {
        let waiting = fixture.call("cocoa.wait", arguments.clone());
        tokio::pin!(waiting);
        tokio::select! { result = &mut waiting => return Err(format!("expected pending wait, got {result:?}").into()), () = tokio::time::sleep(Duration::from_millis(100)) => {} }
    }
    let process = fs::read_to_string(fixture.directory.path().join("running"))?;
    assert!(process_exists(&process)?);
    fs::write(fixture.directory.path().join("finish"), "")?;
    let completed = fixture.call("cocoa.wait", arguments).await?;
    assert_eq!(
        completed["build"]["status"],
        json!({"kind":"exited","code":0})
    );
    assert_eq!(completed["build"]["stdout"]["text"], "survived");
    fixture.environment.close().await?;
    Ok(())
}

#[tokio::test]
async fn stdout_and_stderr_reset_stall_timeout_but_absolute_timeout_still_kills() -> TestResult {
    let fixture = Fixture::new(CodingLimits {
        default_timeout: Duration::from_millis(250),
        ..limits()
    })
    .await?;
    for stream in ["", ">&2"] {
        let started = tokio::time::Instant::now();
        let result = fixture.call("cocoa.run", json!({"command":format!("for ((i=0;i<8;i++)); do printf tick {stream}; sleep 0.07; done")})).await?;
        assert!(started.elapsed() > Duration::from_millis(500));
        assert_eq!(result["status"], json!({"kind":"exited","code":0}));
        assert_eq!(
            result[if stream.is_empty() {
                "stdout"
            } else {
                "stderr"
            }]["text"],
            "tick".repeat(8)
        );
    }
    for timeout in [Value::Null, json!(180)] {
        let mut arguments = json!({"command":"printf '%s' $$ > stalled; printf initial; printf error >&2; sleep 30"});
        if !timeout.is_null() {
            arguments["command"] = json!(
                "printf '%s' $$ > stalled; while :; do printf initial; printf error >&2; sleep 0.04; done"
            );
            arguments["timeout_ms"] = timeout;
        }
        let receipt = fixture
            .host
            .call("cocoa.run", arguments, work(4096), CancellationToken::new())
            .await?;
        let started = tokio::time::Instant::now();
        let result = fixture
            .call(
                "cocoa.wait",
                json!({"handles":{"build":receipt["reference"]}}),
            )
            .await;
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(
            matches!(&result, Err(CapabilityError::Tool { code, message, effects:Effects::Partial }) if code == "timeout" && message.contains("initial") && message.contains("error")),
            "{result:?}"
        );
        let process = fs::read_to_string(fixture.directory.path().join("stalled"))?;
        assert!(!process_exists(&process)?);
        assert_eq!(
            fixture
                .call("cocoa.cancel", json!({"reference":receipt["reference"]}))
                .await?["state"],
            "failed"
        );
        failure(
            fixture
                .call(
                    "cocoa.wait",
                    json!({"handles":{"build":receipt["reference"]},"timeout_ms":0}),
                )
                .await,
            "timeout",
            Effects::Partial,
        );
    }
    fixture.environment.close().await?;
    Ok(())
}

#[tokio::test]
async fn duplicate_process_handles_share_one_snapshot_while_output_changes() -> TestResult {
    let fixture = Fixture::new(limits()).await?;
    let receipt = fixture
        .host
        .call(
            "cocoa.run",
            json!({"command":"while ! test -f finish; do printf 'tick\\n'; sleep 0.002; done"}),
            work(65536),
            CancellationToken::new(),
        )
        .await?;
    let handles = (0..32)
        .map(|index| (format!("alias{index}"), receipt["reference"].clone()))
        .collect::<serde_json::Map<_, _>>();
    for _ in 0..8 {
        let snapshots = fixture
            .call("cocoa.wait", json!({"handles":handles,"timeout_ms":25}))
            .await?;
        assert_eq!(snapshots["alias0"]["status"]["kind"], "running");
        for index in 1..32 {
            assert_eq!(snapshots[format!("alias{index}")], snapshots["alias0"]);
        }
    }
    fs::write(fixture.directory.path().join("finish"), "")?;
    let terminal = fixture
        .call("cocoa.wait", json!({"handles":handles}))
        .await?;
    assert_eq!(terminal["alias0"]["status"]["kind"], "exited");
    assert!(
        terminal["alias0"]["stdout"]["text"]
            .as_str()
            .is_some_and(|text| !text.is_empty())
    );
    for index in 1..32 {
        assert_eq!(terminal[format!("alias{index}")], terminal["alias0"]);
    }
    fixture.environment.close().await?;
    Ok(())
}

#[tokio::test]
async fn command_count_and_wait_group_bounds_preserve_existing_results() -> TestResult {
    let fixture = Fixture::new(CodingLimits {
        observations: 2,
        ..limits()
    })
    .await?;
    let mut receipts = Vec::new();
    for command in ["printf one", "printf two"] {
        let receipt = fixture
            .host
            .call(
                "cocoa.run",
                json!({"command":command}),
                work(65536),
                CancellationToken::new(),
            )
            .await?;
        fixture
            .call(
                "cocoa.wait",
                json!({"handles":{"build":receipt["reference"]}}),
            )
            .await?;
        receipts.push(receipt);
    }
    failure(
        fixture
            .host
            .call(
                "cocoa.run",
                json!({"command":"touch forbidden"}),
                work(65536),
                CancellationToken::new(),
            )
            .await,
        "capacity",
        Effects::None,
    );
    assert!(!fixture.directory.path().join("forbidden").exists());
    let handles = (0..32)
        .map(|index| {
            (
                format!("alias{index}"),
                receipts[index % 2]["reference"].clone(),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    let result = fixture
        .call("cocoa.wait", json!({"handles":handles,"timeout_ms":0}))
        .await?;
    assert_eq!(result.as_object().ok_or("expected result map")?.len(), 32);
    for index in 0..32 {
        assert_eq!(
            result[format!("alias{index}")]["stdout"]["text"],
            if index % 2 == 0 { "one" } else { "two" }
        );
    }
    let mut oversized = handles;
    oversized.insert("extra".into(), receipts[0]["reference"].clone());
    failure(
        fixture
            .call("cocoa.wait", json!({"handles":oversized}))
            .await,
        "invalid_arguments",
        Effects::None,
    );
    assert_eq!(
        fixture.call("cocoa.wait", json!({"handles":{}})).await?,
        json!({})
    );
    let wait = fixture
        .host
        .descriptions()
        .into_iter()
        .find(|description| description.name == "cocoa.wait")
        .ok_or("missing wait schema")?;
    assert_eq!(
        wait.parameters["properties"]["handles"]["minProperties"]
            .as_u64()
            .unwrap_or(0),
        0
    );
    for arguments in [
        json!({"handles":{"build":receipts[0]["reference"]},"timeout_ms":null}),
        json!({"handles":{"build":receipts[0]["reference"]},"timeout_ms":-1}),
        json!({"handles":{"build":{}}}),
    ] {
        failure(
            fixture.call("cocoa.wait", arguments).await,
            "invalid_arguments",
            Effects::None,
        );
    }
    fixture.environment.close().await?;
    Ok(())
}

#[tokio::test]
async fn schemas_reject_unknown_fields_nulls_startup_hooks_and_invalid_counts() -> TestResult {
    let fixture = Fixture::new(limits()).await?;
    for (method, arguments) in [
        ("cocoa.read", json!({"path":"x","extra":true})),
        ("cocoa.read", json!({"path":""})),
        ("cocoa.read", json!({"path":"x","start_line":null})),
        ("cocoa.read", json!({"path":"x","start_line":0})),
        ("cocoa.read", json!({"path":"x","line_count":201})),
        ("cocoa.read", json!({"path":"x","line_count":1.5})),
        ("cocoa.read", json!({"path":"x","file":"x"})),
        ("cocoa.read", json!({"file":"x"})),
        ("cocoa.chunk", json!({"file":"x"})),
        ("cocoa.run", json!(["true"])),
        ("cocoa.run", json!({"command":"true","stdin":null})),
        ("cocoa.run", json!({"command":"true","timeout_ms":0})),
        ("cocoa.run", json!({"command":"true","environment":null})),
        (
            "cocoa.run",
            json!({"command":"true","environment":{"BASH_ENV":"startup"}}),
        ),
        (
            "cocoa.run",
            json!({"command":"true","environment":{"BASH_FUNC_x%%":"() { echo bad; }"}}),
        ),
        (
            "cocoa.run",
            json!({"command":"true","working_directory":null}),
        ),
        ("cocoa.nope", json!({})),
    ] {
        let execution = fixture.call(method, arguments).await;
        failure(execution, "invalid_arguments", Effects::None);
    }
    for (method, arguments, field) in [
        ("cocoa.read", json!("x"), "object"),
        ("cocoa.write", json!({"file":null,"content":"x"}), "file"),
        (
            "cocoa.edit",
            json!({"file":"stale","edits":[{"old_text":"a","new_text":null}]}),
            "edits[0].new_text",
        ),
        (
            "cocoa.run",
            json!({"command":"true","timeout_ms":"1"}),
            "timeout_ms",
        ),
        (
            "cocoa.run",
            json!({"command":"true","environment":{"X":1}}),
            "environment[\"X\"]",
        ),
    ] {
        let error = fixture
            .call(method, arguments)
            .await
            .err()
            .ok_or("accepted malformed shape")?;
        assert!(
            matches!(&error, CapabilityError::Tool { code, message, effects:Effects::None } if code == "invalid_arguments" && message.contains(field)),
            "{error}"
        );
    }
    failure(
        fixture
            .call("cocoa.write", json!({"file":"stale","content":"x"}))
            .await,
        "invalid_reference",
        Effects::None,
    );
    let result = fixture
        .call("cocoa.run", json!({"command":"printf allowed"}))
        .await?;
    assert_eq!(result["stdout"]["text"], "allowed");
    let descriptions = fixture.host.descriptions();
    assert_eq!(
        descriptions
            .iter()
            .map(|description| description.name.as_str())
            .collect::<Vec<_>>(),
        [
            "cocoa.read",
            "cocoa.write",
            "cocoa.edit",
            "cocoa.run",
            "cocoa.wait",
            "cocoa.cancel"
        ]
    );
    for backend_detail in ["/bin/bash", "256 observations"] {
        assert_eq!(
            descriptions
                .iter()
                .filter(|description| description.description.contains(backend_detail))
                .count(),
            1,
            "shared backend policy must appear once in the catalog"
        );
    }
    for description in descriptions {
        assert!(description.description.contains("Promise<"));
        assert!(description.description.contains("Example:"));
        assert_eq!(description.parameters["type"], "object");
        assert_eq!(description.parameters["additionalProperties"], false);
        if description.name == "cocoa.edit" {
            assert_eq!(description.parameters["required"], json!(["file", "edits"]));
            assert_eq!(description.parameters["properties"]["edits"]["minItems"], 1);
            assert_eq!(
                description.parameters["properties"]["edits"]["maxItems"],
                64
            );
            assert!(
                description.parameters["properties"]
                    .get("old_text")
                    .is_none()
            );
        }
    }
    fixture.environment.close().await?;
    Ok(())
}

#[tokio::test]
async fn invalid_library_limits_and_startup_baselines_fail_at_construction() -> TestResult {
    let directory = tempfile::tempdir()?;
    for invalid in [
        CodingLimits {
            processes: 0,
            ..limits()
        },
        CodingLimits {
            preview_lines: 201,
            ..limits()
        },
        CodingLimits {
            observed_bytes: 1,
            ..limits()
        },
        CodingLimits {
            captured_output_bytes: limits().file_bytes + 1,
            ..limits()
        },
        CodingLimits {
            default_timeout: Duration::ZERO,
            ..limits()
        },
    ] {
        assert!(
            LocalEnvironment::new(configuration(&directory), invalid)
                .await
                .is_err()
        );
    }
    let mut unsafe_baseline = configuration(&directory);
    unsafe_baseline
        .environment
        .insert("BASH_ENV".into(), "/actual/home/startup".into());
    assert!(
        LocalEnvironment::new(unsafe_baseline, limits())
            .await
            .is_err()
    );
    Ok(())
}

struct RemoteEnvironment;

#[async_trait]
impl CodingEnvironment for RemoteEnvironment {
    fn description(&self) -> String {
        "Remote Bash, no local path access".into()
    }

    async fn execute(
        &self,
        operation: CodingOperation,
        context: CodingContext,
    ) -> Result<Value, CapabilityError> {
        tokio::time::sleep(Duration::from_millis(5)).await;
        context.check()?;
        match operation {
            CodingOperation::Read {
                selector: ReadSelector::Path { path },
                ..
            } => {
                assert_eq!(path, "/remote-only/path-absent-on-host");
                Ok(
                    json!({"kind":"text","file":"remote-ref","path":path,"bytes":7,"line_count":1,"page":{"start_line":1,"line_count":1,"text":"remote\n"},"next":null}),
                )
            }
            CodingOperation::Read {
                selector: ReadSelector::File { file },
                start_line,
                line_count,
            } => {
                assert_eq!(file, "remote-ref");
                assert_eq!(start_line, 1);
                assert_eq!(line_count, 200);
                Ok(
                    json!({"kind":"text","file":file,"path":"/remote-only/path-absent-on-host","bytes":7,"line_count":1,"page":{"start_line":1,"line_count":1,"text":"remote\n"},"next":null}),
                )
            }
            CodingOperation::Write { file, content } => {
                assert_eq!(file, "remote-ref");
                assert_eq!(content, "updated");
                Ok(
                    json!({"kind":"text","file":"remote-fresh","path":"/remote-only/path-absent-on-host","bytes":7,"line_count":1}),
                )
            }
            CodingOperation::Edit { file, edits } => {
                assert_eq!(file, "remote-fresh");
                assert_eq!(edits.len(), 1);
                assert_eq!(edits[0].old_text, "updated");
                assert_eq!(edits[0].new_text, "changed");
                Ok(
                    json!({"kind":"text","file":"remote-edited","path":"/remote-only/path-absent-on-host","bytes":7,"line_count":1}),
                )
            }
            CodingOperation::Run(arguments) => {
                assert_eq!(arguments.command, "remote-tests");
                assert_eq!(
                    arguments.working_directory.as_deref(),
                    Some("/remote-only/cwd")
                );
                Ok(json!({"reference":"process.remote.1","kind":"process"}))
            }
            CodingOperation::Wait { handles, .. } => {
                assert_eq!(handles["command"], "process.remote.1");
                Ok(
                    json!({"command":{"status":{"kind":"exited","code":3},"stdout":{"text":"remote output","truncated":true,"lossy":false,"output_path":"/remote-only/output"},"stderr":{"text":"","truncated":false,"lossy":false,"output_path":null}}}),
                )
            }
            CodingOperation::Cancel { .. } => Ok(json!({"state":"completed"})),
            CodingOperation::LoadSkill { .. } => Err(CapabilityError::Tool {
                code: "unsupported".into(),
                message: "Remote environment has no skill catalog".into(),
                effects: Effects::None,
            }),
        }
    }
}

#[tokio::test]
async fn injected_environment_owns_every_observation_mutation_command_and_output_path() -> TestResult
{
    assert!(!PathBuf::from("/remote-only/path-absent-on-host").exists());
    let host = CodingCapabilities::new(Arc::new(RemoteEnvironment));
    let read = host
        .call(
            "cocoa.read",
            json!({"path":"/remote-only/path-absent-on-host"}),
            work(4096),
            CancellationToken::new(),
        )
        .await?;
    assert_eq!(
        host.call(
            "cocoa.read",
            json!({"file":read["file"],"start_line":1,"line_count":200}),
            work(4096),
            CancellationToken::new()
        )
        .await?["page"]["text"],
        "remote\n"
    );
    let fresh = host
        .call(
            "cocoa.write",
            json!({"file":read["file"],"content":"updated"}),
            work(4096),
            CancellationToken::new(),
        )
        .await?;
    assert_eq!(
        host.call(
            "cocoa.edit",
            json!({"file":fresh["file"],"edits":[{"old_text":"updated","new_text":"changed"}]}),
            work(4096),
            CancellationToken::new()
        )
        .await?["file"],
        "remote-edited"
    );
    let run = run_to_completion(
        &host,
        json!({"command":"remote-tests","working_directory":"/remote-only/cwd"}),
        work(4096),
    )
    .await?;
    assert_eq!(run["stdout"]["output_path"], "/remote-only/output");
    assert_eq!(run["status"]["code"], 3);
    Ok(())
}

async fn wait_for(path: PathBuf) -> TestResult {
    for _ in 0..200 {
        if path.exists() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    Err("process did not produce readiness evidence".into())
}

fn process_exists(process: &str) -> Result<bool, Box<dyn Error>> {
    let output = std::process::Command::new("/bin/ps")
        .args(["-o", "stat=", "-p", process])
        .output()?;
    Ok(output.status.success() && !String::from_utf8(output.stdout)?.trim().is_empty())
}
