//! Application configuration and root/child composition through the real CLI.

#![cfg(unix)]

#[path = "support/process.rs"]
mod process;
#[path = "support/terminal.rs"]
mod terminal;

use process::{CapturedProcess, CommandExt};
use serde_json::{Value, json};
use std::{
    error::Error,
    ffi::OsStr,
    fs,
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use terminal::Terminal;

#[tokio::test]
async fn exec_uses_persistent_repl_tools_compaction_children_and_durable_history()
-> Result<(), Box<dyn Error>> {
    let home = tempfile::tempdir()?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let mut settings = configuration(&format!("http://{}/v1/responses", listener.local_addr()?));
    settings["aliases"]["child"] =
        json!([{"provider":"openai","model":"child-model","effort":"low"}]);
    settings["compaction"] = json!({
        "request_bytes":262144,"trigger_bytes":131072,"target_bytes":65536,
        "summary_input_bytes":1048576
    });
    write_configuration(home.path(), &settings)?;
    let server = thread::spawn(move || -> io::Result<()> {
        for round in 0..5 {
            let (mut stream, body, _) = request(&listener)?;
            let event = match round {
                0 => {
                    assert_eq!(body["model"], "direct-root");
                    assert_eq!(body["reasoning"]["effort"], "high");
                    assert!(body.to_string().contains("Exercise headless execution"));
                    execution(
                        round,
                        &format!("var seed = 17; /*{}*/", "x".repeat(140 * 1024)),
                    )
                }
                1 => {
                    assert_eq!(body["tool_choice"], "none");
                    completion(&checkpoint_proposal())
                }
                2 => {
                    assert!(body.to_string().contains("checkpoint_projection"));
                    execution(
                        round,
                        "seed += 6; var destination = await cocoa.read({path:'result.txt'}); await cocoa.write({file:destination.file,content:String(seed)}); print(await cocoa.wait(await cocoa.spawn({name:'worker',alias:'child',prompt:'Check child'})));",
                    )
                }
                3 => {
                    assert_eq!(body["model"], "child-model");
                    completion("child verified")
                }
                4 => {
                    assert!(body.to_string().contains("child verified"));
                    completion("root answered")
                }
                _ => unreachable!("bounded fixture"),
            };
            let mut event: Value = serde_json::from_str(event.trim_start_matches("data: ").trim())?;
            let (input, output, read, write) = [
                (101, 3, 7, 11),
                (37, 5, 2, 3),
                (53, 11, 13, 5),
                (211, 17, 19, 23),
                (71, 19, 17, 7),
            ][round];
            event["response"]["usage"] = json!({"input_tokens":input,"output_tokens":output,
                "input_tokens_details":{"cached_tokens":read,"cache_write_tokens":write}});
            respond(
                &mut stream,
                "text/event-stream",
                &format!("data: {event}\n\n"),
            )?;
        }
        no_requests(&listener)
    });
    let output = cli_command(
        home.path(),
        &[
            "exec",
            "--model",
            "openai/direct-root",
            "--thinking",
            "high",
            "--export",
            "run.json",
            "Exercise",
            "headless",
            "execution",
        ],
    )
    .output_bounded()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(report["outcome"], "completed");
    assert_eq!(report["cleanup"], "confirmed");
    assert_eq!(report["final_answer"], "root answered");
    assert!(
        report["session_id"]
            .as_str()
            .is_some_and(|id| id.starts_with("sess_"))
    );
    assert!(report["root_agent_id"].is_string());
    assert_eq!(report["invocation_id"], 1);
    assert_eq!(report["accounting"]["requests_started"], 5);
    let totals = json!({"input_tokens":473,"output_tokens":55,
        "input_cache":{"read_tokens":58,"write_tokens":49}});
    assert_eq!(report["accounting"]["usage"], totals);
    assert_eq!(report["accounting"]["reported_usage"], totals);
    let export: Value = serde_json::from_slice(&fs::read(home.path().join("run.json"))?)?;
    assert_eq!(export["accounting"], report["accounting"]);
    assert_eq!(export["ledger_complete"], true);
    let requests = export["requests"].as_array().ok_or("missing requests")?;
    assert_eq!(requests.len(), 5);
    for (index, request) in requests.iter().enumerate() {
        assert_eq!(request["settings"]["provider"], "openai");
        assert_eq!(request["settings"]["endpoint_default"], false);
        assert_eq!(
            request["settings"]["model"],
            if index == 3 {
                "child-model"
            } else {
                "direct-root"
            }
        );
        assert_eq!(
            request["settings"]["thinking_level"],
            if index == 3 { "low" } else { "high" }
        );
        assert_eq!(
            request["settings"]["output_tokens"],
            if index == 1 { 8192 } else { 32000 }
        );
        assert_eq!(
            request["purpose"],
            if index == 1 { "compaction" } else { "response" }
        );
        assert_eq!(request["agent"] == export["root_agent"], index != 3);
        assert_eq!(request["outcome"], "completed");
    }
    assert_eq!(fs::read_to_string(home.path().join("result.txt"))?, "23");
    server.join().map_err(|_| "exec fixture panicked")??;
    let snapshot = saved_snapshot(home.path()).await?;
    assert_eq!(snapshot.histories.len(), 2);
    assert!(
        snapshot
            .histories
            .values()
            .any(|history| history.checkpoints.len() == 1)
    );
    Ok(())
}

#[test]
fn exec_reads_stdin_and_rejects_invalid_input_without_starting_a_session()
-> Result<(), Box<dyn Error>> {
    for piped in [false, true] {
        let home = tempfile::tempdir()?;
        let listener = TcpListener::bind("127.0.0.1:0")?;
        write_configuration(
            home.path(),
            &configuration(&format!("http://{}/v1/responses", listener.local_addr()?)),
        )?;
        let server = thread::spawn(move || -> io::Result<()> {
            let (mut stream, body, _) = request(&listener)?;
            assert!(body.to_string().contains("stdin instruction\\nsecond line"));
            respond(
                &mut stream,
                "text/event-stream",
                &completion("stdin accepted"),
            )
        });
        let mut command = cli_command(home.path(), &["exec"]);
        let output = if piped {
            let mut child = command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;
            child
                .stdin
                .take()
                .ok_or("missing stdin")?
                .write_all(b"stdin instruction\nsecond line")?;
            CapturedProcess::try_from(child)?.finish(process::EXIT_TIMEOUT)?
        } else {
            fs::write(
                home.path().join("instruction"),
                "stdin instruction\nsecond line",
            )?;
            command
                .stdin(fs::File::open(home.path().join("instruction"))?)
                .output_bounded()?
        };
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout)?["final_answer"],
            "stdin accepted"
        );
        server.join().map_err(|_| "stdin fixture panicked")??;
    }
    for input in [b"   ".as_slice(), b"\xff", b"12345"] {
        let home = tempfile::tempdir()?;
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let mut settings =
            configuration(&format!("http://{}/v1/responses", listener.local_addr()?));
        settings["input_bytes"] = json!(4);
        write_configuration(home.path(), &settings)?;
        fs::write(home.path().join("instruction"), input)?;
        let output = cli_command(home.path(), &["exec"])
            .stdin(fs::File::open(home.path().join("instruction"))?)
            .output_bounded()?;
        assert_eq!(output.status.code(), Some(1));
        let report: Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(report["failure"], "input");
        assert_eq!(report["cleanup"], "not_required");
        assert!(report["session_id"].is_null());
        assert!(!home.path().join("data").exists());
        no_requests(&listener)?;
    }
    Ok(())
}

#[test]
fn exec_configuration_and_provider_failures_never_open_recovery() -> Result<(), Box<dyn Error>> {
    let home = tempfile::tempdir()?;
    for invalid in ["missing", "malformed", "credentials"] {
        let mut command = cli_command(home.path(), &["exec", "Do work"]);
        match invalid {
            "missing" => {}
            "malformed" => {
                fs::create_dir_all(home.path().join(".config/cocoa"))?;
                fs::write(home.path().join(".config/cocoa/config.json"), "{")?;
            }
            _ => {
                write_configuration(
                    home.path(),
                    &configuration("http://127.0.0.1:1/v1/responses"),
                )?;
                command.env_remove("COCOA_TEST_KEY");
            }
        }
        let output = command.output_bounded()?;
        assert_eq!(output.status.code(), Some(1));
        let report: Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(report["outcome"], "failed");
        assert_eq!(report["failure"], "configuration");
        assert_eq!(report["cleanup"], "not_required");
        assert!(report["session_id"].is_null());
    }
    let listener = TcpListener::bind("127.0.0.1:0")?;
    write_configuration(
        home.path(),
        &configuration(&format!("http://{}/v1/responses", listener.local_addr()?)),
    )?;
    let server = thread::spawn(move || -> io::Result<TcpListener> {
        let (mut stream, _, _) = request(&listener)?;
        let body = "provider diagnostic fixture-key must stay private";
        write!(
            stream,
            "HTTP/1.1 401 Unauthorized\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )?;
        Ok(listener)
    });
    let output = cli_command(home.path(), &["exec", "Do work"]).output_bounded()?;
    assert_eq!(output.status.code(), Some(1));
    let report: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(report["outcome"], "failed");
    assert_eq!(report["failure"], "execution");
    assert_eq!(report["cleanup"], "confirmed");
    assert!(report["final_answer"].is_null());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("fixture-key"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("fixture-key"));
    no_requests(&server.join().map_err(|_| "failure fixture panicked")??)?;
    Ok(())
}

#[test]
fn exec_signals_cancel_running_commands_and_confirm_cleanup() -> Result<(), Box<dyn Error>> {
    use rustix::process::{Pid, Signal, kill_process};

    for (signal, name, code) in [(Signal::INT, "SIGINT", 130), (Signal::TERM, "SIGTERM", 143)] {
        let home = tempfile::tempdir()?;
        let listener = TcpListener::bind("127.0.0.1:0")?;
        write_configuration(
            home.path(),
            &configuration(&format!("http://{}/v1/responses", listener.local_addr()?)),
        )?;
        let server = thread::spawn(move || -> io::Result<()> {
            let (mut stream, _, _) = request(&listener)?;
            respond(
                &mut stream,
                "text/event-stream",
                &execution(
                    0,
                    "var process = await cocoa.run({command:'echo $$ > running.pid; while :; do echo tick >> ticks; sleep 0.02; done'}); print(await cocoa.wait(process));",
                ),
            )
        });
        let mut process = CapturedProcess::spawn(&mut cli_command(
            home.path(),
            &["exec", "Run until cancelled"],
        ))?;
        wait_file(&home.path().join("running.pid"))?;
        kill_process(
            Pid::from_raw(process.child.id() as i32).ok_or("invalid CLI pid")?,
            signal,
        )?;
        let output = process.finish(process::EXIT_TIMEOUT)?;
        assert_eq!(
            output.status.code(),
            Some(code),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(report["outcome"], "cancelled");
        assert_eq!(report["signal"], name);
        assert_eq!(report["cleanup"], "confirmed");
        assert_process_stopped(home.path(), "running.pid", "ticks")?;
        server.join().map_err(|_| "signal fixture panicked")??;
    }
    Ok(())
}

#[test]
fn exec_root_answer_cancels_working_child_before_export() -> Result<(), Box<dyn Error>> {
    let home = tempfile::tempdir()?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let mut settings = configuration(&format!("http://{}/v1/responses", listener.local_addr()?));
    settings["aliases"]["child"] = json!([{"provider":"openai","model":"child-model"}]);
    write_configuration(home.path(), &settings)?;
    let directory = home.path().to_owned();
    let server = thread::spawn(move || -> io::Result<()> {
        let (mut stream, _, _) = request(&listener)?;
        respond(
            &mut stream,
            "text/event-stream",
            &execution(
                0,
                "var child = await cocoa.spawn({name:'worker',alias:'child',prompt:'Keep writing until cancelled'}); print('spawned');",
            ),
        )?;
        let mut root = None;
        for _ in 0..2 {
            let (mut stream, body, _) = request(&listener)?;
            if body["model"] == "child-model" {
                respond(
                    &mut stream,
                    "text/event-stream",
                    &execution(
                        1,
                        "var process = await cocoa.run({command:'echo $$ > child.pid; while :; do echo tick >> child-ticks; sleep 0.02; done'}); print(await cocoa.wait(process));",
                    ),
                )?;
            } else {
                root = Some(stream);
            }
        }
        wait_file(&directory.join("child.pid"))?;
        respond(
            &mut root.ok_or_else(|| io::Error::other("missing root continuation"))?,
            "text/event-stream",
            &completion("root finished while child worked"),
        )
    });
    let output = cli_command(
        home.path(),
        &["exec", "--export", "run.json", "Start a child and answer"],
    )
    .output_bounded()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(report["outcome"], "completed");
    assert_eq!(report["cleanup"], "confirmed");
    assert_eq!(report["final_answer"], "root finished while child worked");
    assert_eq!(report["exported"], true);
    let outcomes = report["child_outcomes"]
        .as_object()
        .ok_or("missing child outcomes")?;
    assert_eq!(outcomes.len(), 1);
    assert_eq!(
        outcomes.values().next().ok_or("missing child")?["Completed"]["value"]["status"],
        "cancelled"
    );
    assert_process_stopped(home.path(), "child.pid", "child-ticks")?;
    let export: Value = serde_json::from_slice(&fs::read(home.path().join("run.json"))?)?;
    assert_eq!(export["public_id"], report["session_id"]);
    let child = export["agents"]
        .as_array()
        .ok_or("missing export agents")?
        .iter()
        .find(|agent| !agent["parent"].is_null())
        .ok_or("missing export child")?;
    assert_eq!(
        child["outcome"]["Completed"]["value"]["status"],
        "cancelled"
    );
    assert_eq!(export["accounting"], report["accounting"]);
    let id = report["session_id"].as_str().ok_or("missing session id")?;
    fs::write(
        home.path().join(".config/cocoa/config.json"),
        "invalid configuration",
    )?;
    let retained = cli_command(home.path(), &["sessions", "export", id, "--json"])
        .env_remove("COCOA_TEST_KEY")
        .output_bounded()?;
    assert!(
        retained.status.success(),
        "{}",
        String::from_utf8_lossy(&retained.stderr)
    );
    let retained: Value = serde_json::from_slice(&retained.stdout)?;
    assert_eq!(retained["agents"], export["agents"]);
    assert_eq!(retained["accounting"], export["accounting"]);
    assert_process_stopped(home.path(), "child.pid", "child-ticks")?;
    server
        .join()
        .map_err(|_| "working child fixture panicked")??;
    Ok(())
}

#[test]
fn exec_signals_interrupt_stdin_and_provider_waits() -> Result<(), Box<dyn Error>> {
    use rustix::process::{Pid, Signal, kill_process};
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    for waiting_for_stdin in [false, true] {
        let home = tempfile::tempdir()?;
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let mut settings =
            configuration(&format!("http://{}/v1/responses", listener.local_addr()?));
        settings["providers"]["openai"]
            .as_object_mut()
            .ok_or("missing provider")?
            .remove("credential_env");
        write_configuration(home.path(), &settings)?;
        let directory = home.path().join(".config/cocoa");
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        let mut auth = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(directory.join("auth.json"))?;
        serde_json::to_writer(
            &mut auth,
            &json!({"openai":{"type":"api-key","api_key":"!touch credentials-ready; printf fixture-key"}}),
        )?;
        let mut command = cli_command(
            home.path(),
            if waiting_for_stdin {
                &["exec"]
            } else {
                &["exec", "Wait for provider"]
            },
        );
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut process = CapturedProcess::try_from(command.spawn()?)?;
        let stream = if waiting_for_stdin {
            wait_file(&home.path().join("credentials-ready"))?;
            None
        } else {
            Some(request(&listener)?.0)
        };
        kill_process(
            Pid::from_raw(process.child.id() as i32).ok_or("invalid CLI pid")?,
            Signal::TERM,
        )?;
        let output = process.finish(process::EXIT_TIMEOUT)?;
        assert_eq!(output.status.code(), Some(143));
        let report: Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(report["outcome"], "cancelled");
        assert_eq!(
            report["cleanup"],
            if waiting_for_stdin {
                "not_required"
            } else {
                "confirmed"
            }
        );
        if waiting_for_stdin {
            assert!(report["session_id"].is_null());
            no_requests(&listener)?;
        }
        drop(stream);
    }
    Ok(())
}

#[tokio::test]
async fn exec_never_reports_success_when_record_cleanup_is_unconfirmed()
-> Result<(), Box<dyn Error>> {
    use sqlx::{Connection, Executor};

    let home = tempfile::tempdir()?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let mut settings = configuration(&format!("http://{}/v1/responses", listener.local_addr()?));
    settings["shutdown_timeout_ms"] = json!(25);
    settings["settlement_timeout_ms"] = json!(25);
    write_configuration(home.path(), &settings)?;
    let mut process = CapturedProcess::spawn(&mut cli_command(
        home.path(),
        &["exec", "Answer while storage is blocked"],
    ))?;
    let (mut stream, _, _) = request(&listener)?;
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(home.path().join("data/sessions.sqlite3"));
    let mut blocker = sqlx::SqliteConnection::connect_with(&options).await?;
    blocker.execute("BEGIN IMMEDIATE").await?;
    respond(
        &mut stream,
        "text/event-stream",
        &completion("not durably finished"),
    )?;
    let output = process.finish(process::EXIT_TIMEOUT)?;
    blocker.execute("ROLLBACK").await?;
    blocker.close().await?;
    assert_eq!(output.status.code(), Some(1));
    let report: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(report["outcome"], "failed");
    assert_eq!(report["cleanup"], "unconfirmed");
    assert_eq!(report["failure"], "cleanup");
    Ok(())
}

#[test]
fn exec_export_write_failure_preserves_answer_and_existing_file() -> Result<(), Box<dyn Error>> {
    let home = tempfile::tempdir()?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    write_configuration(
        home.path(),
        &configuration(&format!("http://{}/v1/responses", listener.local_addr()?)),
    )?;
    fs::write(home.path().join("run.json"), "keep this file")?;
    let server = thread::spawn(move || -> io::Result<()> {
        let (mut stream, _, _) = request(&listener)?;
        respond(
            &mut stream,
            "text/event-stream",
            &completion("answer retained"),
        )
    });
    let output =
        cli_command(home.path(), &["exec", "--export", "run.json", "Answer"]).output_bounded()?;
    assert_eq!(output.status.code(), Some(1));
    let report: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(report["outcome"], "failed");
    assert_eq!(report["failure"], "export");
    assert_eq!(report["cleanup"], "confirmed");
    assert_eq!(report["final_answer"], "answer retained");
    assert_eq!(report["exported"], false);
    assert_eq!(
        fs::read_to_string(home.path().join("run.json"))?,
        "keep this file"
    );
    server
        .join()
        .map_err(|_| "export failure fixture panicked")??;
    Ok(())
}

fn cli_command(home: &Path, arguments: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cocoa"));
    command
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin")
        .env("COCOA_TEST_KEY", "fixture-key")
        .current_dir(home)
        .args(["--data-dir", "data"])
        .args(arguments)
        .stdin(Stdio::null());
    command
}

fn wait_file(path: &Path) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !path.exists() {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "tool process did not start",
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

fn assert_process_stopped(home: &Path, pid: &str, ticks: &str) -> Result<(), Box<dyn Error>> {
    let pid = fs::read_to_string(home.join(pid))?.trim().parse::<i32>()?;
    assert!(matches!(
        rustix::process::test_kill_process(
            rustix::process::Pid::from_raw(pid).ok_or("invalid tool pid")?
        ),
        Err(rustix::io::Errno::SRCH)
    ));
    let before = fs::read(home.join(ticks))?;
    thread::sleep(Duration::from_millis(150));
    assert_eq!(fs::read(home.join(ticks))?, before);
    Ok(())
}

#[tokio::test]
async fn default_compaction_uses_token_pressure_and_keeps_original_records()
-> Result<(), Box<dyn Error>> {
    let home = tempfile::tempdir()?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    write_configuration(
        home.path(),
        &configuration(&format!("http://{}/v1/responses", listener.local_addr()?)),
    )?;
    let server = thread::spawn(move || -> Result<(), Box<dyn Error + Send + Sync>> {
        let mut summarized = false;
        for round in 0..8 {
            let (mut stream, body, _) = request(&listener)?;
            assert_eq!(body["model"], "root-model");
            let event = if body["tool_choice"] == "none" || body.get("tools").is_none() {
                assert_eq!(round, 2, "compaction must wait for token pressure");
                assert!(!summarized);
                summarized = true;
                assert_eq!(body["max_output_tokens"], 8192);
                let input: Value = serde_json::from_str(summary_evidence(&body)?)?;
                assert_eq!(input["target_request_bytes"], 32 * 1024 * 1024);
                completion(&checkpoint_proposal())
            } else if summarized {
                assert!(body.to_string().contains("checkpoint_projection"));
                assert!(body.to_string().len() <= 1048576);
                respond(
                    &mut stream,
                    "text/event-stream",
                    &completion("default compaction verified"),
                )?;
                return Ok(());
            } else {
                assert!(
                    round < 2,
                    "token pressure must compact before a third model call"
                );
                let bytes = if round == 0 { 240 * 1024 } else { 48 * 1024 };
                execution(round, &format!("print('step'); /*{}*/", "x".repeat(bytes)))
            };
            respond(&mut stream, "text/event-stream", &event)?;
        }
        Err("default compaction did not trigger".into())
    });
    let mut session = Terminal::spawn(
        home.path(),
        &["--data-dir", "data", "Exercise default compaction"],
        110,
        30,
        None,
    )?;
    finish_answer(&mut session, "default compaction verified")?;
    server
        .join()
        .map_err(|_| "default compaction fixture panicked")?
        .map_err(|error| -> Box<dyn Error> { error })?;
    let snapshot = saved_snapshot(home.path()).await?;
    let history = snapshot
        .histories
        .values()
        .next()
        .ok_or("missing root history")?;
    assert_eq!(history.checkpoints.len(), 1);
    history.validate_checkpoints()?;
    assert!(serde_json::to_string(&history.records)?.contains(&"x".repeat(240 * 1024)));
    Ok(())
}

#[tokio::test]
async fn compaction_budgets_do_not_reduce_durable_input_admission() -> Result<(), Box<dyn Error>> {
    for context_tokens in [None, Some(4096)] {
        let home = tempfile::tempdir()?;
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let mut settings =
            configuration(&format!("http://{}/v1/responses", listener.local_addr()?));
        if let Some(tokens) = context_tokens {
            settings["aliases"]["medium"][0]["context_tokens"] = json!(tokens);
        } else {
            settings["compaction"] =
                json!({"request_bytes":10240,"trigger_bytes":8192,"target_bytes":4096});
        }
        write_configuration(home.path(), &settings)?;
        let prompt = "x".repeat(20 * 1024);
        let mut session =
            Terminal::spawn(home.path(), &["--data-dir", "data", &prompt], 110, 30, None)?;
        session.wait_text("automatic compaction failed; history retained")?;
        session.command("exit")?;
        session.finish(0)?;
        no_requests(&listener)?;
        let snapshot = saved_snapshot(home.path()).await?;
        let history = snapshot
            .histories
            .values()
            .next()
            .ok_or("missing root history")?;
        assert!(history.records.iter().any(|stored| {
        matches!(&stored.record.kind, cocoa_core::store::RecordKind::InputAccepted { input } if input.text() == prompt)
    }));
        assert!(history.checkpoints.is_empty());
    }
    Ok(())
}

#[tokio::test]
async fn failed_compaction_keeps_the_session_and_can_retry_without_repeating_effects()
-> Result<(), Box<dyn Error>> {
    for recovery_available in [true, false] {
        let home = tempfile::tempdir()?;
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let mut settings =
            configuration(&format!("http://{}/v1/responses", listener.local_addr()?));
        settings["compaction"] = json!({
            "request_bytes":131072,"trigger_bytes":65536,"target_bytes":32768,
            "summary_input_bytes":2097152
        });
        write_configuration(home.path(), &settings)?;
        let configuration_path = home.path().join(".config/cocoa/config.json");
        let server = thread::spawn(move || -> Result<(), Box<dyn Error + Send + Sync>> {
            for round in 0..4 {
                let (mut stream, body, _) = request(&listener)?;
                let event = match round {
                    0 => execution(
                        round,
                        &format!(
                            "var destination = await cocoa.read({{path:'effect.txt'}}); await cocoa.write({{file:destination.file,content:'one effect'}}); /*{}*/",
                            "x".repeat(140 * 1024)
                        ),
                    ),
                    1 | 2 => {
                        assert_eq!(body["tool_choice"], "none");
                        let evidence = summary_evidence(&body)?;
                        assert!(evidence.contains("Keep the original goal"));
                        if round == 2 {
                            assert!(evidence.contains("one effect"));
                        }
                        if round == 1 && !recovery_available {
                            fs::write(&configuration_path, b"{")?;
                        }
                        let proposal = if round == 1 {
                            "{}".to_owned()
                        } else {
                            checkpoint_proposal()
                        };
                        completion(&proposal)
                    }
                    3 => {
                        assert!(body.to_string().contains("checkpoint_projection"));
                        assert!(
                            body.to_string()
                                .contains("Retry with the original conversation")
                        );
                        completion("conversation recovered")
                    }
                    _ => unreachable!("bounded fixture"),
                };
                respond(&mut stream, "text/event-stream", &event)?;
            }
            no_requests(&listener)?;
            Ok(())
        });
        let mut session = Terminal::spawn(
            home.path(),
            &["--data-dir", "data", "Keep the original goal"],
            110,
            30,
            None,
        )?;
        if recovery_available {
            session.wait_text("automatic compaction failed; history retained")?;
            session.wait_text("Conversation retained")?;
            assert!(!session.contains("Session reset"));
        } else {
            session.finish(1)?;
            assert!(!String::from_utf8_lossy(&session.transcript).contains("Session reset"));
            write_configuration(home.path(), &settings)?;
            session = Terminal::spawn(
                home.path(),
                &["--data-dir", "data", "resume", "--last"],
                110,
                30,
                None,
            )?;
            session.wait_text(" medium ")?;
        }
        assert_eq!(
            fs::read_to_string(home.path().join("effect.txt"))?,
            "one effect"
        );
        session.command("Retry with the original conversation")?;
        finish_answer(&mut session, "conversation recovered")?;
        server
            .join()
            .map_err(|_| "compaction retry fixture panicked")?
            .map_err(|error| -> Box<dyn Error> { error })?;
        let snapshot = saved_snapshot(home.path()).await?;
        let history = snapshot
            .histories
            .values()
            .next()
            .ok_or("missing root history")?;
        assert_eq!(history.checkpoints.len(), 1);
        assert_eq!(
        history.records.iter().filter(|stored| matches!(
            &stored.record.kind,
            cocoa_core::store::RecordKind::OperationStarted { name, .. } if name == "cocoa.write"
        )).count(),
        1
    );
        let database =
            cocoa::persistence::SessionDatabase::open(&home.path().join("data/sessions.sqlite3"))
                .await?;
        assert_eq!(
            database.list_sessions(None, 10, None).await?.sessions.len(),
            1
        );
    }
    Ok(())
}

#[tokio::test]
async fn configured_compaction_applies_to_direct_root_and_child_requests()
-> Result<(), Box<dyn Error>> {
    let home = tempfile::tempdir()?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let mut settings = configuration(&format!("http://{}/v1/responses", listener.local_addr()?));
    settings["aliases"]["child"] =
        json!([{"provider":"openai","model":"child-model","effort":"low","priority":0}]);
    settings["compaction"] = json!({
        "request_bytes":262144,"trigger_bytes":131072,"target_bytes":65536,
        "summary_input_bytes":1048576
    });
    write_configuration(home.path(), &settings)?;
    let before = fs::read(home.path().join(".config/cocoa/config.json"))?;
    let server = thread::spawn(move || -> Result<(), Box<dyn Error + Send + Sync>> {
        for round in 0..7 {
            let (mut stream, body, headers) = request(&listener)?;
            let child = (3..=5).contains(&round);
            assert_eq!(
                body["model"],
                if child { "child-model" } else { "direct-root" }
            );
            assert_eq!(
                body["reasoning"]["effort"],
                if child { "low" } else { "high" }
            );
            assert!(
                headers
                    .lines()
                    .any(|line| line.eq_ignore_ascii_case("authorization: Bearer fixture-key"))
            );
            let event = if round == 1 || round == 4 {
                assert_eq!(body["tool_choice"], "none");
                assert_eq!(body["max_output_tokens"], 8192);
                let input: Value = serde_json::from_str(summary_evidence(&body)?)?;
                assert_eq!(input["target_request_bytes"], 65536);
                assert!(body.to_string().contains(&"x".repeat(140 * 1024)));
                completion(&checkpoint_proposal())
            } else {
                assert!(body.get("tools").is_some());
                assert_eq!(body["max_output_tokens"], 32000);
                if round == 2 || round == 5 || round == 6 {
                    let encoded = body.to_string();
                    assert!(encoded.contains("checkpoint_projection"));
                    assert!(!encoded.contains(&"x".repeat(140 * 1024)));
                    assert!(encoded.len() <= 262144);
                }
                match round {
                    0 | 3 => execution(
                        round,
                        &format!("print('before compaction'); /*{}*/", "x".repeat(140 * 1024)),
                    ),
                    2 => execution(
                        round,
                        "var child = await cocoa.spawn({name:'worker',alias:'child',prompt:'Check child compaction'}); print(await cocoa.wait(child));",
                    ),
                    5 => completion("child compaction verified"),
                    6 => {
                        assert!(body.to_string().contains("child compaction verified"));
                        completion("root and child compaction verified")
                    }
                    _ => return Err("unexpected compaction request".into()),
                }
            };
            respond(&mut stream, "text/event-stream", &event)?;
        }
        Ok(())
    });
    let mut session = Terminal::spawn(
        home.path(),
        &[
            "--data-dir",
            "data",
            "--model",
            "openai/direct-root",
            "--thinking",
            "high",
            "Check root compaction",
        ],
        110,
        30,
        None,
    )?;
    finish_answer(&mut session, "root and child compaction verified")?;
    server
        .join()
        .map_err(|_| "compaction fixture panicked")?
        .map_err(|error| -> Box<dyn Error> { error })?;
    assert_eq!(
        fs::read(home.path().join(".config/cocoa/config.json"))?,
        before
    );
    let snapshot = saved_snapshot(home.path()).await?;
    assert_eq!(snapshot.histories.len(), 2);
    for history in snapshot.histories.values() {
        assert_eq!(history.checkpoints.len(), 1);
        history.validate_checkpoints()?;
        assert!(history.records.iter().any(|stored| {
            matches!(&stored.record.kind,
                cocoa_core::store::RecordKind::ModelCompleted { response, .. }
                if matches!(&response.action, cocoa_core::model::ModelAction::Execute { cells }
                    if cells.iter().any(|cell| cell.code.contains(&"x".repeat(140 * 1024)))))
        }));
    }
    Ok(())
}

#[tokio::test]
async fn resumed_alias_adopts_current_request_policy_without_changing_saved_execution_settings()
-> Result<(), Box<dyn Error>> {
    let home = tempfile::tempdir()?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let mut settings = configuration(&format!("http://{}/v1/responses", listener.local_addr()?));
    settings["aliases"]["medium"][0]["effort"] = json!("medium");
    write_configuration(home.path(), &settings)?;
    let server = thread::spawn(move || -> Result<(), Box<dyn Error + Send + Sync>> {
        for round in 0..4 {
            let (mut stream, body, _) = request(&listener)?;
            assert_eq!(body["model"], "root-model");
            assert_eq!(body["reasoning"]["effort"], "medium");
            let event = match round {
                0 => execution(
                    round,
                    &format!("print('seed'); /*{}*/", "x".repeat(140 * 1024)),
                ),
                1 => {
                    assert_eq!(body["max_output_tokens"], 32000);
                    assert!(!body.to_string().contains("checkpoint_projection"));
                    completion("seed saved")
                }
                2 => {
                    assert_eq!(body["tool_choice"], "none");
                    assert_eq!(body["max_output_tokens"], 8192);
                    let input: Value = serde_json::from_str(summary_evidence(&body)?)?;
                    assert_eq!(input["target_request_bytes"], 65536);
                    completion(&checkpoint_proposal())
                }
                3 => {
                    assert!(body.get("tools").is_some());
                    assert_eq!(body["max_output_tokens"], 32000);
                    assert!(body.to_string().contains("checkpoint_projection"));
                    assert!(!body.to_string().contains(&"x".repeat(140 * 1024)));
                    completion("resumed compaction verified")
                }
                _ => return Err("unexpected resumed request".into()),
            };
            respond(&mut stream, "text/event-stream", &event)?;
        }
        Ok(())
    });
    let mut initial = Terminal::spawn(
        home.path(),
        &["--data-dir", "data", "Seed history"],
        110,
        30,
        None,
    )?;
    finish_answer(&mut initial, "seed saved")?;
    let saved = saved_snapshot(home.path()).await?;
    assert!(
        saved
            .histories
            .values()
            .all(|history| history.checkpoints.is_empty())
    );
    settings["compaction"] = json!({
        "request_bytes":262144,"trigger_bytes":131072,"target_bytes":65536,
        "summary_input_bytes":1048576
    });
    settings["aliases"]["medium"][0]["context_tokens"] = json!(500_000);
    write_configuration(home.path(), &settings)?;
    let mut resumed = Terminal::spawn(
        home.path(),
        &["--data-dir", "data", "resume", "--last"],
        110,
        30,
        None,
    )?;
    resumed.wait_text(" medium ")?;
    resumed.command("Continue with current compaction policy")?;
    finish_answer(&mut resumed, "resumed compaction verified")?;
    server
        .join()
        .map_err(|_| "resume compaction fixture panicked")?
        .map_err(|error| -> Box<dyn Error> { error })?;
    let restored = saved_snapshot(home.path()).await?;
    let mut expected = saved.settings.clone();
    let cocoa::persistence::StoredSessionSettings::Bound { effective } = &mut expected else {
        return Err("missing saved settings".into());
    };
    effective
        .aliases
        .get_mut("medium")
        .ok_or("missing alias")?
        .context_tokens = Some(500_000);
    assert_eq!(restored.settings, expected);
    let history = restored
        .histories
        .values()
        .next()
        .ok_or("missing root history")?;
    assert_eq!(history.checkpoints.len(), 1);
    history.validate_checkpoints()?;
    assert!(
        history.records.starts_with(
            &saved
                .histories
                .values()
                .next()
                .ok_or("missing saved history")?
                .records
        )
    );
    Ok(())
}

#[test]
fn search_selection_and_credentials_apply_to_root_and_child_agents() -> Result<(), Box<dyn Error>> {
    for selection in [
        None,
        Some(json!({"provider":"exa"})),
        Some(json!({"provider":"disabled"})),
        Some(json!({"provider":"mcp","server":"docs"})),
    ] {
        let enabled = selection
            .as_ref()
            .is_none_or(|value| value["provider"] == "exa");
        let selected_mcp = selection
            .as_ref()
            .is_some_and(|value| value["provider"] == "mcp");
        let home = tempfile::tempdir()?;
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let endpoint = format!("http://{}", listener.local_addr()?);
        let mut settings = configuration(&format!("{endpoint}/v1/responses"));
        if let Some(selection) = selection {
            settings["web_search"] = selection;
        }
        settings["aliases"]["child"] =
            json!([{"provider":"openai","model":"child-model","priority":0}]);
        settings["providers"]["baseten"] = json!({"credential_env":"RUSTUP_TOOLCHAIN"});
        settings["mcp_servers"] = json!({"docs":{
            "id":"00112233445566778899aabbccddeeff","url":format!("{endpoint}/mcp"),
            "enabled":true,"headers_env":{"x-api-key":"LANG"}
        }});
        write_configuration(home.path(), &settings)?;
        fs::write(
            home.path().join("AGENTS.md"),
            "PROJECT INSTRUCTION SENTINEL",
        )?;
        let server = thread::spawn(move || -> Result<(), Box<dyn Error + Send + Sync>> {
            for round in 0..7 {
                let (mut stream, body, headers) = request(&listener)?;
                if body["jsonrpc"] == "2.0" {
                    assert!(
                        headers
                            .lines()
                            .any(|line| line.eq_ignore_ascii_case("x-api-key: mcp-secret"))
                    );
                    assert!(!headers.contains("model-secret"));
                    let result = if body["method"] == "tools/list" {
                        json!({"tools":[{"name":"search","description":"Search fixture docs","inputSchema":{"type":"object"}}]})
                    } else {
                        assert_eq!(body["method"], "tools/call");
                        let query = body["params"]["arguments"]["query"]
                            .as_str()
                            .ok_or("missing query")?;
                        json!({"content":[{"type":"text","text":format!("source-{query}")}]})
                    };
                    respond(
                        &mut stream,
                        "application/json",
                        &json!({"jsonrpc":"2.0","id":body["id"],"result":result}).to_string(),
                    )?;
                    continue;
                }
                assert!(headers.lines().any(|line| line.eq_ignore_ascii_case("authorization: Bearer model-secret")));
                assert!(!headers.contains("mcp-secret"));
                for secret in ["model-secret", "mcp-secret", "unused-secret"] {
                    assert!(!body.to_string().contains(secret));
                }
                let instructions = body["instructions"]
                    .as_str()
                    .ok_or("missing instructions")?;
                assert!(instructions.contains("PROJECT INSTRUCTION SENTINEL"));
                assert_eq!(instructions.contains("cocoa.web_search"), enabled);
                assert_eq!(
                    instructions.contains("Selected web search provider is MCP server"),
                    selected_mcp
                );
                assert_eq!(body["tools"].as_array().map(Vec::len), Some(1));
                let child = body["model"] == "child-model";
                let feedback = body["input"]
                    .as_array()
                    .ok_or("missing input")?
                    .iter()
                    .find(|item| item["type"] == "function_call_output")
                    .and_then(|item| item["output"].as_str());
                let event = if let Some(feedback) = feedback {
                    assert!(
                        feedback.contains(if child { "source-child" } else { "source-root" }),
                        "{feedback}"
                    );
                    assert!(feedback.contains("unset/unset/unset"), "{feedback}");
                    assert!(
                        feedback.contains(if enabled { "function" } else { "undefined" }),
                        "{feedback}"
                    );
                    if !child {
                        assert!(feedback.contains("child verified"), "{feedback}");
                    }
                    completion(if child {
                        "child verified"
                    } else {
                        "search configuration verified"
                    })
                } else {
                    let name = if child { "child" } else { "root" };
                    let mut code = format!(
                        r#"var sources = await mcp.docs.search({{query:'{name}'}}); var shell = await cocoa.wait(await cocoa.run({{command:'printf "%s/%s/%s" "${{LANG-unset}}" "${{COCOA_TEST_KEY-unset}}" "${{RUSTUP_TOOLCHAIN-unset}}"'}})); print([typeof cocoa.web_search,sources.content[0].text,shell.stdout.text]);"#
                    );
                    if !child {
                        code.push_str("var child = await cocoa.spawn({name:'searcher',alias:'child',prompt:'Check configured search'}); print(await cocoa.wait(child));");
                    }
                    execution(round, &code)
                };
                respond(&mut stream, "text/event-stream", &event)?;
            }
            Ok(())
        });
        let mut session = Terminal::spawn_with_environment(
            home.path(),
            &["Check search configuration"],
            110,
            30,
            None,
            &[
                ("COCOA_TEST_KEY", OsStr::new("model-secret")),
                ("LANG", OsStr::new("mcp-secret")),
                ("RUSTUP_TOOLCHAIN", OsStr::new("unused-secret")),
            ],
        )?;
        finish_answer(&mut session, "search configuration verified")?;
        for secret in ["model-secret", "mcp-secret", "unused-secret"] {
            assert!(!String::from_utf8_lossy(&session.transcript).contains(secret));
        }
        server
            .join()
            .map_err(|_| "search fixture panicked")?
            .map_err(|error| -> Box<dyn Error> { error })?;
    }
    Ok(())
}

#[test]
fn child_agents_load_skills_without_inheriting_parent_loaded_instructions()
-> Result<(), Box<dyn Error>> {
    let home = tempfile::tempdir()?;
    let skill = home.path().join(".agents/skills/review/SKILL.md");
    fs::create_dir_all(skill.parent().ok_or("missing skill directory")?)?;
    fs::write(
        &skill,
        "---\nname: review\ndescription: Review source\n---\nPRIVATE SKILL BODY\n",
    )?;
    let skill = fs::canonicalize(skill)?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let mut settings = configuration(&format!("http://{}/v1/responses", listener.local_addr()?));
    settings["aliases"]["child"] =
        json!([{"provider":"openai","model":"child-model","priority":0}]);
    write_configuration(home.path(), &settings)?;
    let server = thread::spawn(move || -> Result<(), Box<dyn Error + Send + Sync>> {
        for (round, model) in [
            "root-model",
            "root-model",
            "child-model",
            "child-model",
            "root-model",
        ]
        .into_iter()
        .enumerate()
        {
            let (mut stream, body, _) = request(&listener)?;
            assert_eq!(body["model"], model);
            let instructions = body["instructions"]
                .as_str()
                .ok_or("missing instructions")?;
            assert!(instructions.contains("<skill-catalog>"));
            assert!(instructions.contains("Review source"));
            assert!(!instructions.contains("PRIVATE SKILL BODY"));
            if round == 0 || round == 2 {
                assert!(!body.to_string().contains("PRIVATE SKILL BODY"));
            } else {
                let feedback = body["input"]
                    .as_array()
                    .ok_or("missing input")?
                    .iter()
                    .find(|item| item["type"] == "function_call_output")
                    .and_then(|item| item["output"].as_str())
                    .ok_or("missing feedback")?;
                assert!(feedback.starts_with("{\"status\":\"completed\"}"));
                assert!(feedback.contains("<skill-instructions>"));
                assert!(feedback.contains("PRIVATE SKILL BODY"));
            }
            let code = match round {
                0 | 2 => Some(format!("await cocoa.load_skill({{id:{}}});", serde_json::to_string(&skill)?)),
                1 => Some("var reviewer = await cocoa.spawn({name:'reviewer',alias:'child',prompt:'Use the review skill'}); print(await cocoa.wait(reviewer));".into()),
                _ => None,
            };
            let event = match code {
                Some(code) => execution(round, &code),
                None => {
                    if round == 4 {
                        assert!(body.to_string().contains("child loaded independently"));
                    }
                    completion(if round == 3 {
                        "child loaded independently"
                    } else {
                        "root and child verified"
                    })
                }
            };
            respond(&mut stream, "text/event-stream", &event)?;
        }
        Ok(())
    });
    let mut session = Terminal::spawn(home.path(), &["Use a skill and delegate"], 110, 30, None)?;
    finish_answer(&mut session, "root and child verified")?;
    server
        .join()
        .map_err(|_| "skill fixture panicked")?
        .map_err(|error| -> Box<dyn Error> { error })?;
    Ok(())
}

#[test]
fn invalid_configuration_and_alias_choices_fail_without_network_or_writes()
-> Result<(), Box<dyn Error>> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let endpoint = format!("http://{}/v1/responses", listener.local_addr()?);
    for (field, replacement, expected) in [
        (
            "web_search",
            json!({"provider":"mcp"}),
            "invalid JSON configuration",
        ),
        ("providers", json!({"unknown":{}}), "unsupported provider"),
        ("input_bytes", json!(8_388_609), "invalid bounds"),
        ("compaction", json!({"request_bytes":0}), "invalid bounds"),
        ("compaction", json!({"trigger_bytes":0}), "invalid bounds"),
        ("compaction", json!({"target_bytes":0}), "invalid bounds"),
        (
            "compaction",
            json!({"summary_input_bytes":0}),
            "invalid bounds",
        ),
        (
            "compaction",
            json!({"target_bytes":50331648}),
            "invalid bounds",
        ),
        (
            "compaction",
            json!({"trigger_bytes":67108864}),
            "invalid bounds",
        ),
        (
            "compaction",
            json!({"request_bytes":268435457}),
            "invalid bounds",
        ),
        (
            "compaction",
            json!({"summary_input_bytes":268435457}),
            "invalid bounds",
        ),
        (
            "compaction",
            json!({"request_bytes":-1}),
            "invalid JSON configuration",
        ),
        (
            "compaction",
            json!({"target_bytes":null}),
            "invalid JSON configuration",
        ),
        ("timeout_ms", json!(0), "invalid bounds"),
        ("default_alias", json!("missing"), "default_alias"),
        ("aliases", json!({"medium":[]}), "aliases require"),
        (
            "aliases",
            json!({"medium":[{"provider":"openai","model":3}]}),
            "invalid JSON configuration",
        ),
        (
            "aliases",
            json!({"medium":[{"provider":"openai","model":"bad","priority":100}]}),
            "aliases require",
        ),
        (
            "aliases",
            json!({"medium":[{"provider":"openai","model":"bad","priority":-100}]}),
            "invalid JSON configuration",
        ),
        (
            "aliases",
            json!({"medium":[{"provider":"openai","model":"good"},{"provider":"openai","model":"bad","priority":-1,"fallback_only":true}]}),
            "invalid JSON configuration",
        ),
        (
            "aliases",
            json!({"medium":[{"provider":"openai","model":"bad","priority":256}]}),
            "invalid JSON configuration",
        ),
        (
            "aliases",
            json!({"medium":[{"provider":"openai","model":"fallback","priority":0,"fallback_only":true},{"provider":"openai","model":"last-fallback","priority":99,"fallback_only":true}]}),
            "aliases require",
        ),
        (
            "aliases",
            json!({"medium":[{"provider":"openai","model":"good"},{"provider":"openai","model":"bad","fallback_only":"true"}]}),
            "invalid JSON configuration",
        ),
        (
            "aliases",
            json!({"medium":[{"provider":"openai","model":"bad","fallback_only":0}]}),
            "invalid JSON configuration",
        ),
        (
            "aliases",
            json!({"medium":[{"provider":"openai","model":"bad","fallback_only":null}]}),
            "invalid JSON configuration",
        ),
        (
            "aliases",
            json!({"medium":[{"provider":"openai","model":"bad","fallback_only":[]}]}),
            "invalid JSON configuration",
        ),
        (
            "aliases",
            json!({"medium":[{"provider":"openai","model":"bad","fallback_only":{}}]}),
            "invalid JSON configuration",
        ),
        (
            "aliases",
            json!({"medium":[{"provider":"missing","model":"bad","priority":0}]}),
            "unsupported provider",
        ),
        (
            "aliases",
            json!({"medium":[{"provider":"openai","model":"good","priority":0},{"provider":"openai","model":"bad","priority":1,"fallback_only":true,"effort":"invalid"}]}),
            "invalid JSON configuration",
        ),
    ] {
        let home = tempfile::tempdir()?;
        let mut settings = configuration(&endpoint);
        settings[field] = replacement;
        write_configuration(home.path(), &settings)?;
        let path = home.path().join(".config/cocoa/config.json");
        let before = fs::read(&path)?;
        let mut session = Terminal::spawn(home.path(), &[], 110, 30, None)?;
        session
            .until(|session| {
                session.exited()
                    || (session.contains(expected)
                        && session
                            .transcript
                            .windows(8)
                            .any(|bytes| bytes == b"\x1b[?1049h"))
            })
            .map_err(|error| format!("{field}={}: {error}", settings[field]))?;
        if session.exited() {
            session.finish(1)?;
        } else {
            session.command("exit")?;
            session.finish(0)?;
        }
        let output = String::from_utf8_lossy(&session.transcript);
        assert!(output.contains(expected), "{settings}: {output}");
        assert!(!output.contains("private-sentinel"));
        assert_eq!(fs::read(path)?, before);
        assert!(
            !home
                .path()
                .join(".local/share/cocoa/sessions.sqlite3")
                .exists()
        );
        no_requests(&listener)?;
    }
    Ok(())
}

#[test]
fn alias_startup_priority_reaches_the_provider() -> Result<(), Box<dyn Error>> {
    let entries = json!([
        {"provider":"openai","model":"last","priority":99},
        {"provider":"openai","model":"fallback","priority":0,"fallback_only":true},
        {"provider":"openai","model":"first"},
        {"provider":"openai","model":"equal-later","priority":0,"fallback_only":false}
    ]);
    for arguments in [vec![], vec!["--alias", "medium"], vec!["--model", "medium"]] {
        assert_selection(
            json!({"aliases":{"medium":entries},"default_alias":"medium","api_key":"private-sentinel","compaction":{"request_tokens":1000},"default_preset":"missing","presets":{"default":["openai/model"]}}),
            &arguments,
            "first",
            Value::Null,
        )?;
    }
    assert_selection(
        json!({"providers":{"baseten":{"credential_env":"MISSING_TEST_CREDENTIAL"}},"aliases":{"medium":[
            {"provider":"baseten","model":"unavailable","priority":0},
            {"provider":"openai","model":"credential-fallback","priority":1,"fallback_only":true}
        ]},"default_alias":"medium"}),
        &[],
        "credential-fallback",
        Value::Null,
    )?;
    Ok(())
}

#[tokio::test]
async fn startup_fallback_orders_root_and_child_choices_and_pins_resume()
-> Result<(), Box<dyn Error>> {
    let home = tempfile::tempdir()?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let mut settings = configuration(&format!("http://{}/v1/responses", listener.local_addr()?));
    settings["aliases"]["medium"] = json!([
        {"provider":"openai","model":"regular-unused","priority":99},
        {"provider":"openai","model":"fallback-unused","priority":99,"fallback_only":true},
        {"provider":"openai","model":"root-fallback","effort":"high","priority":2,"fallback_only":true},
        {"provider":"openai","model":"root-primary","priority":0},
        {"provider":"openai","model":"root-rejected","priority":1,"fallback_only":true},
        {"provider":"openai","model":"root-equal-rejected","priority":1,"fallback_only":true}
    ]);
    settings["aliases"]["child"] = json!([
        {"provider":"openai","model":"child-fallback","effort":"low","priority":1,"fallback_only":true},
        {"provider":"openai","model":"child-primary","priority":99}
    ]);
    write_configuration(home.path(), &settings)?;
    let before = fs::read(home.path().join(".config/cocoa/config.json"))?;
    let server = thread::spawn(move || -> Result<(), Box<dyn Error + Send + Sync>> {
        for (round, model) in [
            "root-primary",
            "root-rejected",
            "root-equal-rejected",
            "root-fallback",
            "child-primary",
            "child-fallback",
            "root-fallback",
            "root-fallback",
            "root-fallback",
            "root-fallback",
        ]
        .into_iter()
        .enumerate()
        {
            let (mut stream, body, _) = request(&listener)?;
            assert_eq!(body["model"], model, "round {round}");
            match round {
                0 | 1 | 2 | 4 => {
                    write!(
                        stream,
                        "HTTP/1.1 503 Unavailable\r\nRetry-After: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )?;
                    for _ in 0..3 {
                        let (mut stream, retry_body, _) = request(&listener)?;
                        assert_eq!(retry_body, body);
                        write!(
                            stream,
                            "HTTP/1.1 503 Unavailable\r\nRetry-After: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        )?;
                    }
                }
                3 => {
                    assert_eq!(body["reasoning"]["effort"], "high");
                    respond(
                        &mut stream,
                        "text/event-stream",
                        &execution(
                            round,
                            "var child = await cocoa.spawn({name:'worker',alias:'child',prompt:'Check startup fallback'}); print(await cocoa.wait(child)); var inherited = await cocoa.spawn({name:'inherited',prompt:'Check inherited fallback'}); print(await cocoa.wait(inherited));",
                        ),
                    )?;
                }
                5 => {
                    assert_eq!(body["reasoning"]["effort"], "low");
                    respond(
                        &mut stream,
                        "text/event-stream",
                        &completion("child fallback verified"),
                    )?;
                }
                6 => {
                    assert_eq!(body["reasoning"]["effort"], "high");
                    respond(
                        &mut stream,
                        "text/event-stream",
                        &completion("inherited fallback verified"),
                    )?;
                }
                7 => {
                    assert!(body.to_string().contains("child fallback verified"));
                    assert!(body.to_string().contains("inherited fallback verified"));
                    respond(
                        &mut stream,
                        "text/event-stream",
                        &completion("startup fallback verified"),
                    )?;
                }
                8 => respond(
                    &mut stream,
                    "text/event-stream",
                    &completion("same model verified"),
                )?,
                9 => respond(
                    &mut stream,
                    "text/event-stream",
                    &completion("resumed fallback verified"),
                )?,
                _ => unreachable!("fixture rounds are bounded"),
            }
        }
        no_requests(&listener)?;
        Ok(())
    });
    let mut session = Terminal::spawn(
        home.path(),
        &["--data-dir", "data", "Check startup fallback"],
        110,
        30,
        None,
    )?;
    session.wait_text("startup fallback verified")?;
    session.wait_text("medium · root-fallback high")?;
    session.command("Continue")?;
    finish_answer(&mut session, "same model verified")?;
    let snapshot = saved_snapshot(home.path()).await?;
    let cocoa::persistence::StoredSessionSettings::Bound { effective } = snapshot.settings else {
        return Err("missing pinned model".into());
    };
    assert_eq!(effective.model, "root-fallback");
    assert_eq!(effective.thinking_level.as_deref(), Some("high"));
    assert_eq!(effective.aliases["medium"].model, "root-fallback");
    let mut resumed = Terminal::spawn(
        home.path(),
        &["--data-dir", "data", "resume", "--last"],
        110,
        30,
        None,
    )?;
    resumed.wait_text("medium · root-fallback high")?;
    resumed.command("Continue after resume")?;
    finish_answer(&mut resumed, "resumed fallback verified")?;
    server
        .join()
        .map_err(|_| "fallback fixture panicked")?
        .map_err(|error| -> Box<dyn Error> { error })?;
    assert_eq!(
        fs::read(home.path().join(".config/cocoa/config.json"))?,
        before
    );
    Ok(())
}

#[test]
fn partial_output_ambiguous_failures_and_later_rejections_never_trigger_startup_fallback()
-> Result<(), Box<dyn Error>> {
    for failure in ["partial", "ambiguous", "later-rejection"] {
        let home = tempfile::tempdir()?;
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let mut settings =
            configuration(&format!("http://{}/v1/responses", listener.local_addr()?));
        settings["aliases"]["medium"] = json!([
            {"provider":"openai","model":"root-model","priority":0},
            {"provider":"openai","model":"must-not-run","priority":0,"fallback_only":true}
        ]);
        write_configuration(home.path(), &settings)?;
        let server = thread::spawn(move || -> io::Result<TcpListener> {
            let (mut stream, body, _) = request(&listener)?;
            assert_eq!(body["model"], "root-model");
            if failure == "partial" {
                respond(
                    &mut stream,
                    "text/event-stream",
                    "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"content_index\":0,\"item_id\":\"answer\",\"delta\":\"partial answer\"}\n\n",
                )?;
            } else if failure == "ambiguous" {
                drop(stream);
            } else {
                respond(
                    &mut stream,
                    "text/event-stream",
                    &completion("initial model verified"),
                )?;
                for _ in 0..4 {
                    let (mut stream, body, _) = request(&listener)?;
                    assert_eq!(body["model"], "root-model");
                    write!(
                        stream,
                        "HTTP/1.1 503 Unavailable\r\nRetry-After: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )?;
                }
            }
            Ok(listener)
        });
        let mut session = Terminal::spawn(home.path(), &["Check startup boundary"], 110, 30, None)?;
        if failure == "later-rejection" {
            session.wait_text("initial model verified")?;
            session.command("Fail the second request")?;
        }
        session.wait_text("Provider paused")?;
        let listener = server
            .join()
            .map_err(|_| "fallback boundary fixture panicked")??;
        no_requests(&listener)?;
        session.key(b"c")?;
        session.until(|session| {
            session
                .states()
                .is_ok_and(|states| states.last().is_some_and(|state| state == "idle"))
        })?;
        session.command("exit")?;
        session.finish(0)?;
        no_requests(&listener)?;
    }
    Ok(())
}

#[test]
fn configuration_and_multibyte_paste_enforce_exact_byte_limits() -> Result<(), Box<dyn Error>> {
    let home = tempfile::tempdir()?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let mut settings = configuration(&format!("http://{}/v1/responses", listener.local_addr()?));
    settings["input_bytes"] = json!(4);
    write_configuration(home.path(), &settings)?;
    let path = home.path().join(".config/cocoa/config.json");
    let mut bytes = fs::read(&path)?;
    bytes.resize(65_537, b' ');
    fs::write(&path, &bytes)?;
    let mut oversized = Terminal::spawn(home.path(), &[], 100, 24, None)?;
    oversized.finish(1)?;
    assert!(
        String::from_utf8_lossy(&oversized.transcript)
            .contains("configuration exceeds 65536 bytes")
    );
    no_requests(&listener)?;
    bytes.pop();
    fs::write(&path, &bytes)?;
    let server = thread::spawn(move || -> io::Result<()> {
        let (mut stream, body, _) = request(&listener)?;
        assert_eq!(body["input"][0]["content"], "éé");
        respond(
            &mut stream,
            "text/event-stream",
            &completion("exact bytes accepted"),
        )
    });
    let mut session = Terminal::spawn(home.path(), &[], 100, 24, None)?;
    session.wait_text("medium")?;
    session.send("\x1b[200~ééx\x1b[201~".as_bytes())?;
    session.wait_text("Paste exceeds")?;
    session.send("\x1b[200~éé\x1b[201~".as_bytes())?;
    session.key(b"\r")?;
    finish_answer(&mut session, "exact bytes accepted")?;
    server.join().map_err(|_| "byte limit fixture panicked")??;
    assert_eq!(fs::read(path)?, bytes);
    Ok(())
}

#[test]
fn eight_mib_paste_is_retained_and_an_extra_byte_is_rejected_atomically()
-> Result<(), Box<dyn Error>> {
    let home = tempfile::tempdir()?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let mut settings = configuration(&format!("http://{}/v1/responses", listener.local_addr()?));
    settings["input_bytes"] = json!(8_388_608);
    settings["aliases"]["medium"][0]["context_tokens"] = json!(16_000_000);
    settings["compaction"] = json!({
        "request_bytes":16777216,"trigger_bytes":12582912,"target_bytes":8388608
    });
    write_configuration(home.path(), &settings)?;
    let server = thread::spawn(move || -> io::Result<()> {
        for round in 0..2 {
            let (mut stream, body, _) = request(&listener)?;
            let prompt = body["input"][0]["content"]
                .as_str()
                .ok_or_else(|| io::Error::other("missing prompt"))?;
            assert_eq!(prompt.len(), 8_388_608);
            assert!(prompt.bytes().all(|byte| byte == b'x'));
            if round == 1 {
                assert!(body["input"].as_array().is_some_and(|input| {
                    input
                        .iter()
                        .any(|item| item["role"] == "user" && item["content"] == "again")
                }));
            }
            respond(
                &mut stream,
                "text/event-stream",
                &completion(if round == 0 {
                    "large input accepted"
                } else {
                    "large input retained"
                }),
            )?;
        }
        Ok(())
    });
    let mut session = Terminal::spawn(home.path(), &[], 100, 24, None)?;
    session.until(|session| {
        session
            .states()
            .is_ok_and(|states| states.last().is_some_and(|state| state == "idle"))
    })?;
    let prompt = "x".repeat(8_388_608);
    session.send(b"\x1b[200~")?;
    session.send(prompt.as_bytes())?;
    session.send(b"y\x1b[201~")?;
    session.send(b"\x1b[200~")?;
    session.send(prompt.as_bytes())?;
    session.send(b"\x1b[201~")?;
    session.wait_text("xxxxxxxxxx")?;
    session.key(b"\r")?;
    for completed in 1..=2 {
        let deadline = Instant::now() + Duration::from_secs(25);
        while session
            .states()?
            .iter()
            .filter(|state| *state == "done")
            .count()
            < completed
        {
            session.pump()?;
            if session.exited() || Instant::now() >= deadline {
                return Err(format!("large input did not complete: {}", session.screen()).into());
            }
        }
        if completed == 1 {
            session.command("again")?;
        }
    }
    session.command("exit")?;
    session.finish(0)?;
    server
        .join()
        .map_err(|_| "large input fixture panicked")??;
    Ok(())
}

#[test]
fn invalid_project_instructions_fail_before_model_requests() -> Result<(), Box<dyn Error>> {
    let home = tempfile::tempdir()?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let settings = configuration(&format!("{endpoint}/v1/responses"));
    write_configuration(home.path(), &settings)?;
    fs::write(home.path().join(".config/cocoa/AGENTS.md"), [0xff])?;
    let mut session = Terminal::spawn(home.path(), &["hello"], 100, 24, None)?;
    session.until(|session| {
        session.exited()
            || (session.contains("not UTF-8")
                && session
                    .transcript
                    .windows(8)
                    .any(|bytes| bytes == b"\x1b[?1049h"))
    })?;
    let output = String::from_utf8_lossy(&session.transcript);
    assert!(output.contains("AGENTS.md"), "{output}");
    assert!(output.contains("not UTF-8"), "{output}");
    no_requests(&listener)?;
    if session.exited() {
        session.finish(1)?;
    } else {
        session.command("exit")?;
        session.finish(0)?;
    }
    Ok(())
}

fn assert_selection(
    mut settings: Value,
    arguments: &[&str],
    model: &str,
    reasoning: Value,
) -> Result<(), Box<dyn Error>> {
    let home = tempfile::tempdir()?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    settings["providers"]["openai"] = json!({"credential_env":"COCOA_TEST_KEY", "endpoint":format!("http://{}/v1/responses", listener.local_addr()?)});
    write_configuration(home.path(), &settings)?;
    let path = home.path().join(".config/cocoa/config.json");
    let before = fs::read(&path)?;
    let server = thread::spawn(move || -> io::Result<Value> {
        let (mut stream, body, _) = request(&listener)?;
        respond(
            &mut stream,
            "text/event-stream",
            &completion("selection verified"),
        )?;
        Ok(body)
    });
    let mut arguments = arguments.to_vec();
    arguments.push("hello");
    let mut session = Terminal::spawn(home.path(), &arguments, 110, 30, None)?;
    finish_answer(&mut session, "selection verified")?;
    let body = server.join().map_err(|_| "selection fixture panicked")??;
    assert_eq!(body["model"], model);
    assert_eq!(body["reasoning"], reasoning);
    assert_eq!(fs::read(path)?, before);
    Ok(())
}

fn finish_answer(session: &mut Terminal, answer: &str) -> io::Result<()> {
    session.wait_text(answer)?;
    session.command("exit")?;
    session.finish(0)
}

async fn saved_snapshot(
    home: &Path,
) -> Result<cocoa::persistence::SessionSnapshot, Box<dyn Error>> {
    let database =
        cocoa::persistence::SessionDatabase::open(&home.join("data/sessions.sqlite3")).await?;
    let sessions = database.list_sessions(None, 1, None).await?;
    let identity = &sessions
        .sessions
        .first()
        .ok_or("missing saved session")?
        .public_id;
    let limits = cocoa_core::store::StoreLimits {
        record_bytes: 2 * cocoa_llm::DECODER_BYTES + 4096,
    };
    let attachment = database
        .attach_session(&identity.to_string(), limits)
        .await?;
    Ok(attachment.load(limits).await?)
}

fn summary_evidence(body: &Value) -> Result<&str, Box<dyn Error + Send + Sync>> {
    body["input"]
        .as_array()
        .and_then(|input| input.last())
        .and_then(|input| input["content"].as_str())
        .ok_or_else(|| "missing summary input".into())
}

fn checkpoint_proposal() -> String {
    json!({
        "additions":[],"changes":[],"evidence":[],
        "recap":{"goal":"Check CLI compaction","stages":[],"current_stage":null,
            "decisions":[],"progress":[],"failures":[],"next_actions":[]}
    })
    .to_string()
}

fn configuration(endpoint: &str) -> Value {
    json!({"providers":{"openai":{"credential_env":"COCOA_TEST_KEY", "endpoint":endpoint}},
        "aliases":{"medium":[{"provider":"openai", "model":"root-model","priority":0}]}, "default_alias":"medium"})
}

fn write_configuration(home: &Path, settings: &Value) -> io::Result<()> {
    let directory = home.join(".config/cocoa");
    fs::create_dir_all(&directory)?;
    fs::write(directory.join("config.json"), serde_json::to_vec(settings)?)
}

fn no_requests(listener: &TcpListener) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    assert!(matches!(listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock));
    Ok(())
}

fn execution(round: usize, code: &str) -> String {
    format!(
        "data: {}\n\n",
        json!({"type":"response.completed","response":{
            "id":format!("response_{round}"), "status":"completed", "output":[{
                "type":"function_call", "id":format!("item_{round}"), "call_id":format!("call_{round}"),
                "name":"exec", "arguments":json!({"code":code}).to_string()
            }]
        }})
    )
}

fn completion(answer: &str) -> String {
    format!(
        "data: {}\n\n",
        json!({"type":"response.completed","response":{
            "id":"response_answer", "status":"completed", "output":[{
                "type":"message", "id":"message_answer", "role":"assistant", "status":"completed",
                "content":[{"type":"output_text", "text":answer, "annotations":[]}]
            }]
        }})
    )
}

fn respond(stream: &mut TcpStream, content_type: &str, body: &str) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn request(listener: &TcpListener) -> io::Result<(TcpStream, Value, String)> {
    let deadline = Instant::now() + Duration::from_secs(30);
    listener.set_nonblocking(true)?;
    let mut stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(10))
            }
            Err(error) => return Err(error),
        }
    };
    stream.set_nonblocking(false)?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut bytes = Vec::new();
    let mut buffer = [0; 16 * 1024];
    let (header_end, length) = loop {
        if Instant::now() >= deadline || bytes.len() > 32 * 1024 {
            return Err(io::Error::other("request headers exceeded fixture limits"));
        }
        stream.set_read_timeout(Some(deadline.saturating_duration_since(Instant::now())))?;
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&bytes[..end]).map_err(io::Error::other)?;
            let length = headers
                .lines()
                .find_map(|line| {
                    line.split_once(':')
                        .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                        .map(|(_, value)| value.trim().parse::<usize>())
                })
                .ok_or_else(|| io::Error::other("missing content length"))?
                .map_err(io::Error::other)?;
            if length > 20 * 1024 * 1024 {
                return Err(io::Error::other("request body exceeds fixture limit"));
            }
            break (end, length);
        }
    };
    let end = header_end + 4 + length;
    while bytes.len() < end {
        if Instant::now() >= deadline {
            return Err(io::ErrorKind::TimedOut.into());
        }
        stream.set_read_timeout(Some(deadline.saturating_duration_since(Instant::now())))?;
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    let headers = std::str::from_utf8(&bytes[..header_end])
        .map_err(io::Error::other)?
        .to_owned();
    let body = serde_json::from_slice(&bytes[header_end + 4..end])?;
    Ok((stream, body, headers))
}
