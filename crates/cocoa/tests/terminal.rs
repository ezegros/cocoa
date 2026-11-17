//! Real-terminal command, cancellation and recovery contracts against a local HTTP provider.

#![cfg(unix)]

#[path = "support/process.rs"]
mod process;
#[path = "support/terminal.rs"]
mod terminal;

use base64::{Engine, engine::general_purpose::STANDARD};
use cocoa::persistence::SessionDatabase;
use rustix::process::{Pid, test_kill_process};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use terminal::Terminal;

#[test]
fn first_message_and_draft_remain_visible_while_session_initialization_is_blocked() -> io::Result<()>
{
    assert_responsive_startup(StartupCompletion::Respond)
}

#[test]
fn cancellation_during_session_initialization_preserves_the_draft_without_sending() -> io::Result<()>
{
    assert_responsive_startup(StartupCompletion::Cancel)
}

#[test]
fn exit_during_session_initialization_waits_for_cleanup_without_sending() -> io::Result<()> {
    assert_responsive_startup(StartupCompletion::Exit)
}

#[derive(Clone, Copy)]
enum StartupCompletion {
    Respond,
    Cancel,
    Exit,
}

fn assert_responsive_startup(completion: StartupCompletion) -> io::Result<()> {
    let root = Homes::new()?;
    let provider = Provider::start(ProviderKind::Commands)?;
    let mcp = Provider::start(ProviderKind::StartupMcp)?;
    let home = root.command_home("blocked-startup", &provider.endpoint, true, None)?;
    let path = home.join(".config/cocoa/config.json");
    let mut configuration: Value = serde_json::from_slice(&fs::read(&path)?)?;
    configuration["mcp_servers"] = json!({"startup": {
        "id": "0123456789abcdef0123456789abcdef",
        "url": mcp.endpoint,
        "enabled": true,
        "headers_env": {"authorization": "FIXTURE_KEY"}
    }});
    fs::write(&path, serde_json::to_vec(&configuration)?)?;
    let mut terminal = Terminal::spawn(&home, &[], 90, 28, None)?;
    terminal.wait_text(" medium ")?;
    terminal.command("startup-message-sentinel")?;
    terminal.until(|_| mcp.events.cycle_started.load(Ordering::SeqCst))?;
    terminal.wait_text("startup-message-sentinel")?;
    assert!(provider.requests()?.is_empty());

    terminal.resize(70, 24)?;
    terminal.command("/model another-model")?;
    terminal.wait_text("Draft unchanged")?;
    assert!(terminal.contains("/model another-model"));
    terminal.key(b"\x15")?;
    let image_path = home.join("startup.png");
    image::RgbImage::from_pixel(1, 1, image::Rgb([180, 30, 80]))
        .save(&image_path)
        .map_err(io::Error::other)?;
    terminal.text(&format!("{} ", image_path.display()))?;
    terminal.wait_text("[Image 1]")?;
    terminal.text("startup-draft-sentinel")?;
    terminal.wait_text("startup-draft-sentinel")?;
    assert!(terminal.contains("startup-message-sentinel"));
    assert!(!mcp.events.cycle_release.load(Ordering::SeqCst));
    assert!(provider.requests()?.is_empty());

    match completion {
        StartupCompletion::Respond => {}
        StartupCompletion::Cancel => {
            terminal.key(b"\x1b")?;
            terminal.key(b"\x1b")?;
        }
        StartupCompletion::Exit => {
            terminal.key(b"\x15")?;
            terminal.command(":q")?;
        }
    }
    if !matches!(completion, StartupCompletion::Respond) {
        terminal.wait_text("Stopping")?;
        assert!(!terminal.exited());
        assert!(provider.requests()?.is_empty());
    }
    mcp.events.cycle_release.store(true, Ordering::SeqCst);
    if matches!(completion, StartupCompletion::Exit) {
        terminal.finish(0)?;
        assert!(provider.requests()?.is_empty());
    } else {
        if matches!(completion, StartupCompletion::Cancel) {
            assert_cancelled_idle(&mut terminal)?;
            assert!(provider.requests()?.is_empty());
        } else {
            terminal.wait_text("answer-1")?;
            assert_eq!(provider.requests()?.len(), 1);
        }
        assert!(terminal.contains("startup-draft-sentinel"));
        terminal.key(b"\x15")?;
        if matches!(completion, StartupCompletion::Cancel) {
            terminal.command("after-startup-cancellation")?;
            terminal.wait_text("answer-1")?;
            assert_eq!(provider.requests()?.len(), 1);
        }
        close_command_session(&mut terminal)?;
    }
    mcp.finish()?;
    provider.finish()
}

#[test]
fn configured_themes_and_cli_overrides_apply_before_any_model_request() -> io::Result<()> {
    let root = Homes::new()?;
    let server = Provider::start(ProviderKind::Commands)?;
    for (name, saved, arguments, background) in [
        ("cocoa-default", None, vec![], Some("48;2;16;12;10")),
        (
            "cocoa-milk",
            Some("milk-chocolate"),
            vec![],
            Some("48;2;245;237;223"),
        ),
        (
            "cocoa-grove",
            Some("milk-chocolate"),
            vec!["--theme", "cacao-grove"],
            Some("48;2;11;16;13"),
        ),
        (
            "cocoa-native",
            Some("milk-chocolate"),
            vec!["--theme", "terminal-native"],
            None,
        ),
    ] {
        let home = root.command_home(name, &server.endpoint, true, None)?;
        let path = home.join(".config/cocoa/config.json");
        let mut configuration: Value = serde_json::from_slice(&fs::read(&path)?)?;
        if let Some(saved) = saved {
            configuration["theme"] = json!(saved);
        }
        let original = serde_json::to_vec(&configuration)?;
        fs::write(&path, &original)?;
        let mut session = Terminal::spawn(&home, &arguments, 80, 24, None)?;
        session.wait_text("cocoa")?;
        capture_session_view(&mut session, name)?;
        let transcript = String::from_utf8_lossy(&session.transcript);
        if let Some(background) = background {
            assert!(transcript.contains(background), "{name}: {transcript:?}");
        } else {
            assert!(
                !transcript.contains("48;2;"),
                "native must not force an RGB background"
            );
        }
        session.command("/aliases")?;
        session.wait_text("Aliases")?;
        capture_session_view(&mut session, &format!("{name}-menu"))?;
        session.key(b"\x1b")?;
        session.wait_text("cocoa")?;
        assert_eq!(
            fs::read(&path)?,
            original,
            "a CLI override must not save settings"
        );
        close_command_session(&mut session)?;
    }
    assert!(server.requests()?.is_empty());
    server.finish()
}

#[test]
fn saved_and_cli_reduced_motion_keep_a_static_bar_without_writing_settings() -> io::Result<()> {
    let root = Homes::new()?;
    for (name, saved, arguments, static_bar) in [
        ("motion-default", None, vec![], false),
        ("motion-saved", Some(true), vec![], true),
        (
            "motion-override",
            Some(false),
            vec!["--reduced-motion"],
            true,
        ),
    ] {
        let server = Provider::start(ProviderKind::Commands)?;
        let home = root.command_home(
            name,
            &server.endpoint,
            true,
            Some(json!({
                "medium": [{"provider": "openai", "model": "cycle-high", "priority":0}]
            })),
        )?;
        let path = home.join(".config/cocoa/config.json");
        let mut configuration: Value = serde_json::from_slice(&fs::read(&path)?)?;
        if let Some(saved) = saved {
            configuration["reduced_motion"] = json!(saved);
        }
        let original = serde_json::to_vec(&configuration)?;
        fs::write(&path, &original)?;
        let mut session = Terminal::spawn(&home, &arguments, 64, 20, None)?;
        session.wait_text("cocoa")?;
        session.command("check motion preference")?;
        session.wait_text("Toasting")?;
        if static_bar {
            session.wait_text("[#] Toasting")?;
            session.wait_text("╭  [###] Toasting ")?;
            let deadline = Instant::now() + Duration::from_millis(6100);
            while Instant::now() < deadline {
                session.pump()?;
                assert!(session.screen().contains("[#] Toasting"));
                assert!(session.screen().contains("╭  [###] Toasting "));
            }
        } else {
            session.wait_text("(o) Toasting")?;
            session.wait_text("╭  (o|o)  Toasting ")?;
            session.wait_text("╭  (|||)  Toasting ")?;
            session.wait_text("(o) Blending")?;
            session.wait_text("╭  (o|o)  Blending ")?;
        }
        server.events.cycle_release.store(true, Ordering::SeqCst);
        session.wait_text("answer-1")?;
        let completed = session.screen();
        assert!(!completed.contains("Toasting"), "{completed}");
        assert!(!completed.contains("Blending"), "{completed}");
        assert!(!completed.contains("[#]"), "{completed}");
        assert!(!completed.contains("[###]"), "{completed}");
        assert_eq!(fs::read(&path)?, original);
        close_command_session(&mut session)?;
        server.finish()?;
    }
    Ok(())
}

#[test]
fn interrupting_worktree_startup_stops_git_before_exiting() -> io::Result<()> {
    let root = Homes::new()?;
    let server = Provider::start(ProviderKind::Commands)?;
    let home = root.command_home("worktree-interrupt", &server.endpoint, true, None)?;
    let executable = home.join("git");
    fs::write(
        &executable,
        "#!/bin/sh\necho $$ > \"$HOME/git.pid\"\nexec /bin/sleep 60\n",
    )?;
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))?;
    let mut session = Terminal::spawn_with_environment(
        &home,
        &["-w", "interrupted"],
        100,
        26,
        None,
        &[("PATH", home.as_os_str())],
    )?;
    session.until(|_| home.join("git.pid").exists())?;
    let process = read_pid(&home.join("git.pid"))?;
    session.interrupt()?;
    session.finish(1)?;
    assert!(session.contains("worktree creation interrupted"));
    let deadline = Instant::now() + Duration::from_secs(3);
    while process_exists(process)? && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !process_exists(process)?,
        "Git must stop after startup is interrupted"
    );
    assert!(!home.join(".config/cocoa/worktrees").exists());
    server.finish()
}

#[tokio::test]
async fn worktree_session_records_its_directory_and_keeps_relative_storage() -> io::Result<()> {
    let root = Homes::new()?;
    let server = Provider::start(ProviderKind::Commands)?;
    let home = root.command_home("worktree-startup", &server.endpoint, true, None)?;
    for arguments in [
        vec!["init"],
        vec![
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--allow-empty",
            "-m",
            "initial",
        ],
    ] {
        let output = std::process::Command::new("git")
            .args(arguments)
            .current_dir(&home)
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let mut session = Terminal::spawn(
        &home,
        &["-w", "fix-login", "--data-dir", "session-data"],
        110,
        36,
        None,
    )?;
    session.wait_text(" medium ")?;
    session.command("start a conversation")?;
    session.wait_text("answer-1")?;
    close_command_session(&mut session)?;
    let database = SessionDatabase::open(&home.join("session-data/sessions.sqlite3"))
        .await
        .map_err(io::Error::other)?;
    let page = database
        .list_sessions(None, 20, None)
        .await
        .map_err(io::Error::other)?;
    assert_eq!(page.sessions.len(), 1);
    let saved = database
        .resolve_session(page.sessions[0].public_id.as_str())
        .await
        .map_err(io::Error::other)?;
    assert!(
        saved
            .working_directory
            .starts_with(home.join(".config/cocoa/worktrees").canonicalize()?)
    );
    assert_eq!(
        saved.working_directory.file_name(),
        Some(std::ffi::OsStr::new("fix-login"))
    );
    assert!(!saved.working_directory.join("session-data").exists());
    assert!(saved.working_directory.exists());
    server.finish()
}

#[test]
fn exit_prints_resume_command_with_quoted_data_directory() -> io::Result<()> {
    let root = Homes::new()?;
    let server = Provider::start(ProviderKind::Commands)?;
    let home = root.command_home("exit-resume", &server.endpoint, true, None)?;
    let directory = "session data's";
    let mut session = Terminal::spawn(&home, &["--data-dir", directory], 110, 36, None)?;
    session.wait_text(" medium ")?;
    session.command("start a conversation")?;
    session.wait_text("answer-1")?;
    let page = session_page(&home, Some(Path::new(directory)))?;
    let identity = page["sessions"][0]["public_id"]
        .as_str()
        .ok_or_else(|| io::Error::other("missing session ID"))?;
    close_command_session(&mut session)?;
    assert!(has_bytes(
        &session.transcript,
        format!(
            "Resume the session with: \x1b[1mcocoa resume {identity} --data-dir 'session data'\"'\"'s'\x1b[22m"
        )
        .as_bytes()
    ));
    assert_eq!(server.requests()?.len(), 1);
    server.finish()
}

#[test]
fn rewind_forks_before_selected_prompt_without_replaying_effects() -> io::Result<()> {
    let root = Homes::new()?;
    let server = Provider::start(ProviderKind::Persistence)?;
    let home = root.command_home("rewind", &server.endpoint, true, None)?;
    let mut terminal = Terminal::spawn(&home, &[], 110, 36, None)?;
    terminal
        .wait_text(" medium ")
        .map_err(|error| io::Error::other(format!("startup: {error}")))?;
    terminal.command("root-sentinel retained prompt")?;
    terminal
        .wait_text("answer-2")
        .map_err(|error| io::Error::other(format!("first turn: {error}")))?;
    terminal.command("selected prompt to edit")?;
    terminal
        .wait_text("answer-4")
        .map_err(|error| io::Error::other(format!("second turn: {error}")))?;
    let source = session_page(&home, None)?["sessions"][0]["public_id"].clone();
    terminal.command("/rewind")?;
    terminal
        .wait_text("Rewind before prompt")
        .map_err(|error| io::Error::other(format!("picker: {error}")))?;
    terminal.key(b"\x1b")?;
    terminal.until(|terminal| !terminal.contains("Rewind before prompt"))?;
    assert_eq!(
        session_page(&home, None)?["sessions"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
    terminal.command("/rewind")?;
    terminal.wait_text("Rewind before prompt")?;
    terminal.key(b"\x1b[B")?;
    terminal.key(b"\r")?;
    terminal
        .wait_text("The original session is preserved.")
        .map_err(|error| io::Error::other(format!("fork: {error}")))?;
    assert!(terminal.contains("selected prompt to edit"));
    assert!(!terminal.contains("answer-4"));
    assert!(terminal.contains("answer-2"));
    assert_eq!(
        server.requests()?.len(),
        4,
        "rewind must not request a response"
    );
    assert_eq!(fs::read(home.join("durable-effect"))?, b"x");
    let page = session_page(&home, None)?;
    let sessions = page["sessions"]
        .as_array()
        .ok_or_else(|| io::Error::other("missing sessions"))?;
    assert_eq!(sessions.len(), 2);
    assert!(
        sessions
            .iter()
            .any(|session| session["public_id"] == source)
    );
    terminal.key(b"\r")?;
    terminal.wait_text("answer-5")?;
    let requests = server.requests()?;
    assert_eq!(requests.len(), 5);
    let request = requests[4].body.to_string();
    assert!(request.contains("selected prompt to edit"));
    assert!(
        !request.contains("answer-4"),
        "fork must omit the discarded answer"
    );
    close_command_session(&mut terminal)?;
    server.finish()
}

#[test]
fn durable_conversations_resume_without_replay_and_switch_after_preflight() -> io::Result<()> {
    let root = Homes::new()?;
    let server = Provider::start(ProviderKind::Persistence)?;
    let home = root.command_home("durable-resume", &server.endpoint, true, None)?;
    let mut first = Terminal::spawn(&home, &[], 110, 36, None)?;
    first.wait_text(" medium ")?;
    first.command("root-sentinel first conversation")?;
    first.wait_text("answer-2")?;
    assert_eq!(fs::read(home.join("durable-effect"))?, b"x");
    let page = session_page(&home, None)?;
    let identity = page["sessions"][0]["public_id"]
        .as_str()
        .ok_or_else(|| io::Error::other("missing session ID"))?
        .to_owned();
    assert_eq!(
        page["sessions"][0]["title"],
        "root-sentinel first conversation"
    );
    close_command_session(&mut first)?;
    assert!(has_bytes(
        &first.transcript,
        format!("Resume the session with: \x1b[1mcocoa resume {identity}\x1b[22m").as_bytes()
    ));
    assert_eq!(server.requests()?.len(), 2);

    let mut resumed = Terminal::spawn(&home, &["resume", &identity], 110, 36, None)?;
    resumed.wait_text("root-sentinel first conversation")?;
    assert!(!resumed.contains("Session sess_"));
    resumed.wait_text("root-sentinel first conversation")?;
    resumed.wait_text("Work Done")?;
    assert!(!resumed.contains("Session resumed"));
    capture_session_view(&mut resumed, "resumed-transcript")?;
    resumed.key(b"\t")?;
    resumed.key(b"\x1b[C")?;
    resumed.wait_text("cocoa.run")?;
    capture_session_view(&mut resumed, "resumed-activities")?;
    resumed.key(b"\x1b")?;
    assert_eq!(
        server.requests()?.len(),
        2,
        "opening resume must not call the provider"
    );
    assert_eq!(
        fs::read(home.join("durable-effect"))?,
        b"x",
        "opening resume must not replay effects"
    );
    assert_eq!(
        session_page(&home, None)?["sessions"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
    resumed.command("continue after restart")?;
    resumed.wait_text("answer-4")?;
    let requests = server.requests()?;
    let after_restart = requests[3].body.to_string();
    assert!(
        after_restart.contains("undefined"),
        "the resumed executor must not retain old bindings: {after_restart}"
    );
    assert!(after_restart.contains("root-sentinel first conversation"));
    assert!(after_restart.contains("continue after restart"));
    assert_eq!(fs::read(home.join("durable-effect"))?, b"x");

    let before_resume = session_page(&home, None)?;
    resumed.command(&format!("/resume {}", identity.to_ascii_uppercase()))?;
    resumed.command("/sessions")?;
    resumed.wait_text("Up to 50 results")?;
    resumed.key(b"\x1b")?;
    resumed.until(|terminal| !terminal.contains("Up to 50 results"))?;
    assert!(resumed.contains("root-sentinel first conversation"));
    assert!(resumed.contains("continue after restart"));
    assert!(resumed.contains("answer-4"));
    assert_eq!(session_page(&home, None)?, before_resume);
    assert_eq!(server.requests()?.len(), 4);
    resumed.command("/new")?;
    resumed.until(|terminal| !terminal.contains("answer-4"))?;
    let page = session_page(&home, None)?;
    assert_eq!(page["sessions"].as_array().map(Vec::len), Some(1));
    resumed.command("other conversation")?;
    resumed.wait_text("answer-5")?;
    let configuration_path = home.join(".config/cocoa/config.json");
    let original = fs::read(&configuration_path)?;
    let mut changed: Value = serde_json::from_slice(&original)?;
    changed["aliases"]["medium"][0]["model"] = json!("incompatible-model");
    fs::write(&configuration_path, serde_json::to_vec(&changed)?)?;
    resumed.command(&format!("/resume {identity}"))?;
    resumed.wait_text("saved session settings differ")?;
    capture_session_view(&mut resumed, "switch-preflight-rejected")?;
    assert!(
        resumed.contains("answer-5"),
        "preflight failure must keep the current transcript"
    );
    resumed.command("current survives failed preflight")?;
    resumed.wait_text("answer-6")?;
    let current = server.requests()?[5].body.to_string();
    assert!(current.contains("other conversation"));
    assert!(!current.contains("root-sentinel"));
    fs::write(&configuration_path, &original)?;

    let mut locked = Terminal::spawn(&home, &["resume", &identity], 110, 36, None)?;
    locked.wait_text("root-sentinel first conversation")?;
    assert!(!locked.contains("Session sess_"));
    resumed.command(&format!("/resume {identity}"))?;
    resumed.wait_text("another attachment owns this session")?;
    assert!(resumed.contains("answer-6"));
    close_command_session(&mut locked)?;
    resumed.command(&format!("/resume {}", &identity[..13]))?;
    resumed.wait_text("root-sentinel first conversation")?;
    assert!(!resumed.contains("Session sess_"));
    assert!(!resumed.contains("Session resumed"));
    assert!(resumed.contains("root-sentinel first conversation"));
    assert!(!resumed.contains("other conversation"));
    assert_eq!(server.requests()?.len(), 6);
    resumed.command("/sessions root-sentinel first")?;
    resumed.wait_text("Up to 50 results")?;
    capture_session_view(&mut resumed, "session-picker-search")?;
    assert!(resumed.contains("root-sentinel first conversation"));
    assert!(!resumed.contains("other conversation"));
    resumed.resize(70, 24)?;
    resumed.key(b"")?;
    resumed.wait_text("root-sentinel first conversation")?;
    assert!(!resumed.contains(&identity));
    assert!(!resumed.contains("Updated at"));
    assert!(!resumed.contains(&home.to_string_lossy()));
    capture_session_view(&mut resumed, "session-picker-narrow")?;
    resumed.key(b"\x1b")?;
    resumed.wait_text("root-sentinel first conversation")?;
    assert!(!resumed.contains("Session sess_"));
    assert_eq!(
        session_page(&home, None)?["sessions"]
            .as_array()
            .map(Vec::len),
        Some(2)
    );
    close_command_session(&mut resumed)?;
    assert!(has_bytes(
        &resumed.transcript,
        format!("Resume the session with: \x1b[1mcocoa resume {identity}\x1b[22m").as_bytes()
    ));
    assert_eq!(server.requests()?.len(), 6);
    assert_eq!(fs::read(home.join("durable-effect"))?, b"x");
    server.finish()
}

#[test]
fn switching_waits_for_delegated_work_without_cancelling_it() -> io::Result<()> {
    let root = Homes::new()?;
    let server = Provider::start(ProviderKind::DelegatedSwitch)?;
    let home = root.command_home("delegated-switch", &server.endpoint, true, None)?;
    let mut session = Terminal::spawn(&home, &["delegate work"], 110, 36, None)?;
    session.wait_text("Root finished while child works")?;
    session.until(|_| server.events.cycle_started.load(Ordering::SeqCst))?;
    session.command("/new")?;
    session.wait_text("Cannot switch while delegated work")?;
    assert!(session.contains("Root finished while child works"));
    assert_eq!(
        session_page(&home, None)?["sessions"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
    assert_eq!(server.requests()?.len(), 3);
    server.events.cycle_release.store(true, Ordering::SeqCst);
    session.wait_text("Work Done")?;
    session.command("/new")?;
    session.wait_text("Got an idea?")?;
    assert!(session.contains("cocoa"));
    assert!(session.screen().contains(['█', '▀', '▄']));
    assert!(!session.contains("Root finished while child works"));
    assert!(!session.contains("Session sess_"));
    assert_eq!(
        session_page(&home, None)?["sessions"]
            .as_array()
            .map(Vec::len),
        Some(1),
        "an empty composer must not persist a new session"
    );
    assert_eq!(server.requests()?.len(), 3);
    close_command_session(&mut session)?;
    server.finish()
}

#[tokio::test]
async fn resume_picker_last_and_relocation_never_create_a_conversation() -> io::Result<()> {
    let root = Homes::new()?;
    let home = root.0.join("resume-selection");
    fs::create_dir(&home)?;
    let config = home.join(".config/cocoa");
    fs::create_dir_all(&config)?;
    fs::write(
        config.join("config.json"),
        b"malformed-config-private-sentinel",
    )?;
    let data = home.join("data");
    let data_text = data
        .to_str()
        .ok_or_else(|| io::Error::other("fixture directory"))?;
    let mut picker = Terminal::spawn(&home, &["--data-dir", data_text, "resume"], 90, 28, None)?;
    picker.wait_text("No sessions found.")?;
    capture_session_view(&mut picker, "session-picker-empty")?;
    assert!(!data.exists(), "opening discovery must not create storage");
    picker.key(b"\x03")?;
    picker.finish(0)?;
    let mut missing = Terminal::spawn(
        &home,
        &["--data-dir", data_text, "resume", "--last"],
        90,
        28,
        None,
    )?;
    missing.finish(1)?;
    assert!(!data.exists());
    assert!(!has_bytes(&missing.transcript, b"\x1b[?1049h"));

    fs::create_dir(&data)?;
    fs::set_permissions(&data, fs::Permissions::from_mode(0o700))?;
    let database = SessionDatabase::open(&data.join("sessions.sqlite3"))
        .await
        .map_err(io::Error::other)?;
    let local = database
        .create_session(&home)
        .await
        .map_err(io::Error::other)?;
    let local_id = local.public_id().to_string();
    drop(local);
    let old_directory = home.join("old-project");
    fs::create_dir(&old_directory)?;
    let other = database
        .create_session(&old_directory)
        .await
        .map_err(io::Error::other)?;
    let other_id = other.public_id().to_string();
    drop(other);
    let pool = sqlx::SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::new().filename(data.join("sessions.sqlite3")),
    )
    .await
    .map_err(io::Error::other)?;
    sqlx::query("UPDATE sessions SET title = CASE WHEN public_id = ? THEN 'Local session' ELSE 'Other directory session' END")
        .bind(&local_id)
        .execute(&pool)
        .await
        .map_err(io::Error::other)?;
    pool.close().await;
    let mut picker = Terminal::spawn(
        &home,
        &["--data-dir", data_text, "resume", "--all"],
        110,
        36,
        None,
    )?;
    picker.wait_text("Up to 50 results")?;
    assert!(picker.contains("Local session"));
    assert!(!picker.contains("Other directory session"));
    assert!(!picker.contains(&local_id));
    assert!(!picker.contains(&other_id));
    picker.key(b"	")?;
    picker.wait_text("Up to 50 results")?;
    assert!(!picker.contains("Other directory session"));
    picker.key(b"Other")?;
    picker.wait_text("No sessions found.")?;
    picker.key(b"")?;
    picker.finish(0)?;
    let mut scoped = Terminal::spawn(
        &home,
        &["--data-dir", data_text, "resume", "--last"],
        110,
        36,
        None,
    )?;
    scoped.wait_text("Ready")?;
    assert!(!scoped.contains("Session sess_"));
    close_command_session(&mut scoped)?;
    let moved = home.join("moved-project");
    fs::rename(&old_directory, &moved)?;
    let mut unavailable = Terminal::spawn(
        &home,
        &["--data-dir", data_text, "resume", "--last", "--all"],
        110,
        36,
        None,
    )?;
    unavailable.finish(1)?;
    assert!(has_bytes(&unavailable.transcript, b"--cwd"));
    assert!(!has_bytes(&unavailable.transcript, b"\x1b[?1049h"));
    let mut relocated = Terminal::spawn(
        &home,
        &[
            "--data-dir",
            data_text,
            "resume",
            &other_id,
            "--cwd",
            moved
                .to_str()
                .ok_or_else(|| io::Error::other("fixture directory"))?,
        ],
        110,
        36,
        None,
    )?;
    relocated.wait_text("Session relocated to")?;
    assert!(!relocated.contains("Session sess_"));
    close_command_session(&mut relocated)?;
    assert_eq!(
        database
            .resolve_session(&other_id)
            .await
            .map_err(io::Error::other)?
            .working_directory,
        moved.canonicalize()?
    );
    assert_eq!(
        database
            .list_sessions(None, 20, None)
            .await
            .map_err(io::Error::other)?
            .sessions
            .len(),
        2
    );
    assert_eq!(
        fs::read(config.join("config.json"))?,
        b"malformed-config-private-sentinel"
    );
    Ok(())
}

#[test]
fn onboarding_does_not_create_sessions_or_storage() -> io::Result<()> {
    let root = Homes::new()?;
    let home = root.0.join("empty-sessions");
    fs::create_dir(&home)?;
    let mut first = Terminal::spawn(&home, &[], 90, 28, None)?;
    first.wait_text("Choose a provider")?;
    let page = session_page(&home, None)?;
    assert_eq!(page["sessions"], json!([]));
    assert!(!home.join(".config").exists());
    assert!(!home.join(".local").exists());

    let mut second = Terminal::spawn(&home, &[], 90, 28, None)?;
    second.wait_text("Choose a provider")?;
    assert_eq!(session_page(&home, None)?, page);
    first.key(b"\x03")?;
    first.finish(0)?;
    second.key(b"\x03")?;
    second.finish(0)?;
    assert_eq!(session_page(&home, None)?, page);
    assert!(!home.join(".config").exists());
    assert!(!home.join(".local").exists());
    Ok(())
}

#[test]
fn management_only_creates_a_session_when_a_conversation_starts() -> io::Result<()> {
    let root = Homes::new()?;
    let server = Provider::start(ProviderKind::Commands)?;
    let home = root.command_home("management-sessions", &server.endpoint, true, None)?;
    let before = fs::read(home.join(".config/cocoa/config.json"))?;
    for _ in 0..2 {
        let mut terminal = Terminal::spawn(&home, &[], 90, 28, None)?;
        terminal.wait_text(" medium ")?;
        assert!(!home.join(".local").exists());
        terminal.command("/new")?;
        terminal.command("/new")?;
        terminal.key(b"unsent draft")?;
        terminal.wait_text("unsent draft")?;
        terminal.key(b"\x03")?;
        terminal.until(|terminal| !terminal.contains("unsent draft"))?;
        close_command_session(&mut terminal)?;
        assert!(!home.join(".local").exists());
        assert!(!has_bytes(
            &terminal.transcript,
            b"Resume the session with:"
        ));
    }
    let data = home.join("chosen/nested/data");
    let selected = data
        .to_str()
        .ok_or_else(|| io::Error::other("invalid fixture path"))?;
    for (command, title) in [("aliases", "+ Add alias"), ("mcp", "MCP servers")] {
        let mut terminal =
            Terminal::spawn(&home, &[command, "--data-dir", selected], 90, 28, None)?;
        terminal.wait_text(title)?;
        assert!(!data.exists());
        assert!(!home.join(".local").exists());
        assert!(server.requests()?.is_empty());
        terminal.key(b"\x1b")?;
        terminal.until(|terminal| !terminal.contains(title))?;
        close_command_session(&mut terminal)?;
        assert!(!data.exists());
    }
    let mut terminal = Terminal::spawn(&home, &["aliases", "--data-dir", selected], 90, 28, None)?;
    terminal.wait_text("+ Add alias")?;
    terminal.key(b"\x1b")?;
    terminal.until(|terminal| !terminal.contains("+ Add alias"))?;
    assert!(!data.exists());
    terminal.command("start the conversation")?;
    terminal.wait_text("answer-1")?;
    let page = session_page(&home, Some(&data))?;
    assert_eq!(page["sessions"].as_array().map(Vec::len), Some(1));
    terminal.command("continue the conversation")?;
    terminal.wait_text("answer-2")?;
    let continued = session_page(&home, Some(&data))?;
    assert_eq!(continued["sessions"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        continued["sessions"][0]["public_id"],
        page["sessions"][0]["public_id"]
    );
    close_command_session(&mut terminal)?;
    let closed = session_page(&home, Some(&data))?;
    assert_eq!(
        closed["sessions"][0]["public_id"],
        page["sessions"][0]["public_id"]
    );
    let mut returning = Terminal::spawn(&home, &["--data-dir", selected], 90, 28, None)?;
    returning.wait_text(" medium ")?;
    let reopened = session_page(&home, Some(&data))?;
    assert_eq!(reopened["sessions"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        reopened["sessions"][0]["public_id"],
        page["sessions"][0]["public_id"]
    );
    close_command_session(&mut returning)?;
    assert!(!home.join(".local").exists());
    assert_eq!(fs::metadata(&data)?.permissions().mode() & 0o777, 0o700);
    assert_eq!(fs::read(home.join(".config/cocoa/config.json"))?, before);
    assert_eq!(server.requests()?.len(), 2);
    server.finish()
}

#[test]
fn unsafe_session_storage_fails_on_first_message_and_restores_terminal_modes() -> io::Result<()> {
    let root = Homes::new()?;
    for kind in ["file", "public-directory"] {
        let home = root.0.join(kind);
        fs::create_dir(&home)?;
        let data = home.join("data");
        if kind == "file" {
            fs::write(&data, b"private-sentinel")?;
        } else {
            fs::create_dir(&data)?;
            fs::set_permissions(&data, fs::Permissions::from_mode(0o755))?;
        }
        let selected = data
            .to_str()
            .ok_or_else(|| io::Error::other("invalid fixture path"))?;
        let mut terminal = Terminal::spawn(
            &home,
            &["--data-dir", selected, "--model", "openai/test-model"],
            90,
            28,
            None,
        )?;
        terminal.wait_text("test-model")?;
        terminal.command("first message")?;
        terminal.finish(1)?;
        let diagnostic = String::from_utf8_lossy(&terminal.transcript);
        assert!(
            diagnostic.contains("terminal session failed"),
            "{diagnostic}"
        );
        assert!(!diagnostic.contains("private-sentinel"));
        assert!(has_bytes(&terminal.transcript, b"\x1b[?1049h"));
        assert!(has_bytes(&terminal.transcript, b"\x1b[?1049l"));
        assert!(!home.join(".config").exists());
        assert!(!home.join(".local").exists());
        if kind == "file" {
            assert_eq!(fs::read(&data)?, b"private-sentinel");
        } else {
            assert!(diagnostic.contains("database directory must be owner-only"));
            assert_eq!(fs::metadata(&data)?.permissions().mode() & 0o777, 0o755);
            assert_eq!(fs::read_dir(&data)?.count(), 0);
        }
    }
    Ok(())
}

#[test]
fn deadline_retains_the_conversation_and_resumes_the_same_session() -> io::Result<()> {
    let root = Homes::new()?;
    let server = Provider::start(ProviderKind::Steering)?;
    let mut terminal = root.cancellation_session("deadline-retained", &server.endpoint, 2500)?;
    let initial = session_page(&terminal.home, None)?;
    assert_eq!(initial["sessions"].as_array().map(Vec::len), Some(0));
    terminal.command("seed")?;
    terminal.until(|terminal| terminal.home.join("steering-started").exists())?;
    terminal.text("after the deadline")?;
    terminal.key(b"\x1b[D")?;
    terminal.key(b"\x1b[D")?;
    terminal.wait_text("Conversation retained")?;
    assert!(!terminal.screen().contains("Session reset"));
    assert!(terminal.screen().contains("after the deadline"));
    capture_session_view(&mut terminal, "deadline-preserved-draft")?;
    let retained = session_page(&terminal.home, None)?;
    assert_eq!(retained["sessions"].as_array().map(Vec::len), Some(1));
    terminal.text("!")?;
    terminal.key(b"\r")?;
    until_result(&mut terminal, |_| Ok(server.requests()?.len() >= 2))?;
    let requests = server.requests()?;
    let input = requests[1].body["input"]
        .as_array()
        .ok_or_else(|| io::Error::other("request without input"))?;
    assert!(
        input.len() > 1,
        "resumed request lost the earlier turn: {input:?}"
    );
    assert!(
        input
            .iter()
            .any(|item| item["role"] == "user" && item["content"] == "after the deadli!ne"),
        "draft or cursor was lost: {input:?}"
    );
    terminal.wait_text("answer-3")?;
    let continued = session_page(&terminal.home, None)?;
    assert_eq!(continued["sessions"].as_array().map(Vec::len), Some(1));
    close_command_session(&mut terminal)?;
    server.finish()
}

#[test]
fn signed_out_child_alias_does_not_block_another_provider() -> io::Result<()> {
    let root = Homes::new()?;
    let server = Provider::start(ProviderKind::Commands)?;
    let home = root.command_home("signed-out-child", &server.endpoint, true, None)?;
    let path = home.join(".config/cocoa/config.json");
    let mut configuration: Value = serde_json::from_slice(&fs::read(&path)?)?;
    configuration["providers"]["chatgpt"] = json!({});
    configuration["aliases"]["subscription"] =
        json!([{"provider":"chatgpt","model":"gpt-5.2","priority":0}]);
    fs::write(path, serde_json::to_vec(&configuration)?)?;
    let mut terminal = Terminal::spawn(&home, &[], 80, 24, None)?;
    terminal.wait_text("medium")?;
    terminal.command("question")?;
    terminal.wait_text("answer-1")?;
    assert_eq!(server.requests()?.len(), 1);
    close_command_session(&mut terminal)?;
    server.finish()
}

#[test]
fn copy_command_writes_the_last_answer_to_the_terminal_clipboard() -> io::Result<()> {
    let root = Homes::new()?;
    let server = Provider::start(ProviderKind::Commands)?;
    let home = root.command_home("copy-command", &server.endpoint, true, None)?;
    let mut session = Terminal::spawn(&home, &[], 90, 24, None)?;
    session.wait_text("medium")?;
    session.command("/copy")?;
    session.wait_text("No agent response to copy yet.")?;
    assert!(server.requests()?.is_empty());
    for index in 1..=2 {
        session.command("question")?;
        let answer = format!("answer-{index}");
        session.wait_text(&answer)?;
        session.command("/copy")?;
        let mut clipboard = String::new();
        ratatui::crossterm::Command::write_ansi(
            &ratatui::crossterm::clipboard::CopyToClipboard::to_clipboard_from(&answer),
            &mut clipboard,
        )
        .map_err(io::Error::other)?;
        session.until(|terminal| {
            terminal
                .transcript
                .windows(clipboard.len())
                .any(|window| window == clipboard.as_bytes())
        })?;
        session.wait_text("Copied!")?;
        assert_eq!(server.requests()?.len(), index);
    }
    session.command(":q")?;
    session.finish(0)?;
    server.finish()
}

#[test]
fn fast_stream_remains_complete_before_provider_settlement() -> io::Result<()> {
    use process::CommandExt;

    let root = Homes::new()?;
    let server = Provider::start(ProviderKind::Streaming)?;
    let home = root.command_home("fast-stream", &server.endpoint, true, None)?;
    fs::write(home.join("tracked.txt"), "old\n")?;
    for arguments in [
        vec!["init", "--initial-branch=feat/composer"],
        vec!["add", "tracked.txt"],
        vec![
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "fixture",
        ],
    ] {
        let output = std::process::Command::new("git")
            .args(arguments)
            .current_dir(&home)
            .output_bounded()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    fs::write(home.join("tracked.txt"), "new\nextra\n")?;
    let mut session = Terminal::spawn(&home, &["stream quickly"], 90, 30, None)?;
    session.wait_text("preview-ready")?;
    let screen = session.screen();
    for index in 0..8 {
        let line = format!("Stream line {index}: abcdefghijklmnopqrstuvwxyz012345");
        assert!(screen.contains(&line), "missing {line:?}:\n{screen}");
    }
    assert!(!session.states()?.contains(&"done".to_owned()));
    session.wait_text("tps")?;
    session.wait_text("feat/composer +2 -1")?;
    capture_session_view(&mut session, "composer-stats-streaming")?;
    server.events.cycle_release.store(true, Ordering::SeqCst);
    session.wait_text("settled-answer")?;
    session.wait_text("C88%")?;
    let ready = session.screen();
    let title = ready
        .lines()
        .find(|line| line.starts_with('╭'))
        .ok_or_else(|| io::Error::other("missing composer"))?;
    assert!(
        title.contains("Ready · ") && title.contains("s "),
        "{ready}"
    );
    assert!(title.contains("fixture-model high · C88%"), "{ready}");
    assert!(!title.contains("tps") && !title.contains('$'), "{ready}");
    capture_session_view(&mut session, "composer-stats-ready")?;
    session.resize(52, 16)?;
    session.wait_text("C88%")?;
    capture_session_view(&mut session, "composer-stats-narrow")?;
    session.command(":q")?;
    session.finish(0)?;
    server.finish()
}

#[test]
fn terminal_text_paste_remains_editable_until_submission() -> io::Result<()> {
    let root = Homes::new()?;
    let server = Provider::start(ProviderKind::Commands)?;
    let home = root.command_home("text-paste", &server.endpoint, true, None)?;
    let mut session = Terminal::spawn(&home, &[], 90, 24, None)?;
    session.wait_text("medium")?;
    session.text("ordinary pasted text")?;
    session.wait_text("ordinary pasted text")?;
    assert!(server.requests()?.is_empty(), "paste submitted a prompt");
    session.command(" after paste")?;
    session.wait_text("answer-1")?;
    assert_eq!(
        server.requests()?[0].body["input"][0]["content"],
        "ordinary pasted text after paste"
    );
    session.command(":q")?;
    session.finish(0)?;
    server.finish()
}

#[test]
fn terminal_bracketed_paste_keeps_line_breaks_until_one_submission() -> io::Result<()> {
    let root = Homes::new()?;
    let server = Provider::start(ProviderKind::Commands)?;
    let home = root.command_home("multiline-paste", &server.endpoint, true, None)?;
    let mut session = Terminal::spawn(&home, &[], 90, 24, None)?;
    session.wait_text("medium")?;
    session.text("\x1b[200~first line\nsecond line\x1b[201~")?;
    session.wait_text("second line")?;
    assert!(
        server.requests()?.is_empty(),
        "pasting a line break submitted a prompt"
    );
    session.key(b"\r")?;
    session.wait_text("answer-1")?;
    assert_eq!(
        server.requests()?[0].body["input"][0]["content"],
        "first line\nsecond line"
    );
    session.command(":q")?;
    session.finish(0)?;
    server.finish()
}

#[test]
fn steering_delivers_during_execution_and_can_be_disabled_in_configuration() -> io::Result<()> {
    let root = Homes::new()?;
    for setting in [None, Some(true), Some(false)] {
        let server = Provider::start(ProviderKind::Commands)?;
        let home = root.command_home(
            &format!("steering-{setting:?}"),
            &server.endpoint,
            true,
            Some(json!({"medium":[{"provider":"openai","model":"cycle-high","priority":0}]})),
        )?;
        if let Some(enabled) = setting {
            let path = home.join(".config/cocoa/config.json");
            let mut config = read_json(&path)?;
            config["message_steering"] = json!(enabled);
            fs::write(path, serde_json::to_vec(&config).map_err(io::Error::other)?)?;
        }
        let mut session = Terminal::spawn(&home, &["initial assignment"], 90, 24, None)?;
        session.until(|_| server.events.cycle_started.load(Ordering::SeqCst))?;
        session.command("/new")?;
        session.wait_text("Cannot switch sessions while a turn is active.")?;
        assert!(
            session.contains("/new"),
            "rejection must preserve the draft"
        );
        session.key(b"\x15")?;
        session.command("redirect λ")?;
        if setting != Some(false) {
            session.command("also verify\nwith tests")?;
        }
        assert_eq!(
            server.requests()?.len(),
            1,
            "steering interrupted the active request"
        );
        server.events.cycle_release.store(true, Ordering::SeqCst);
        if setting == Some(false) {
            session.wait_text("answer-1")?;
            assert_eq!(
                server.requests()?.len(),
                1,
                "disabled steering queued a message"
            );
            session.key(b"\r")?;
        }
        session.wait_text("answer-2")?;
        let requests = server.requests()?;
        assert_eq!(requests.len(), 2);
        let history = &requests[1].body["input"];
        assert_eq!(history[0]["content"], "initial assignment");
        assert_eq!(history[1]["role"], "assistant");
        assert_eq!(history[2], json!({"role":"user","content":"redirect λ"}));
        if setting != Some(false) {
            assert_eq!(
                history[3],
                json!({"role":"user","content":"also verify\nwith tests"})
            );
            assert_eq!(session.states()?, ["idle", "working", "done"]);
        }
        session.command(":q")?;
        session.finish(0)?;
        server.finish()?;
    }

    let server = Provider::start(ProviderKind::Steering)?;
    let home = root.command_home("steering-tools", &server.endpoint, true, None)?;
    let mut session = Terminal::spawn(&home, &["initial assignment"], 90, 24, None)?;
    session.until(|_| home.join("steering-started").exists())?;
    session.command("redirect during tools")?;
    assert_eq!(server.requests()?.len(), 1);
    fs::write(home.join("release-steering"), b"")?;
    session.wait_text("answer-3")?;
    let requests = server.requests()?;
    let history = &requests[1].body["input"];
    assert_eq!(history[3]["type"], "function_call_output");
    assert_eq!(history[4]["type"], "function_call_output");
    assert!(
        history[4]["output"]
            .as_str()
            .is_some_and(|text| text.contains("\"printed\":[42]"))
    );
    assert_eq!(
        history[5],
        json!({"role":"user","content":"redirect during tools"})
    );
    assert!(
        requests[2].body["input"][7]["output"]
            .as_str()
            .is_some_and(|text| text.contains("\"printed\":[42]"))
    );
    session.command(":q")?;
    session.finish(0)?;
    server.finish()
}

#[test]
fn steering_queue_overflow_preserves_the_draft_and_cancellation_reports_pending_messages()
-> io::Result<()> {
    let root = Homes::new()?;
    let server = Provider::start(ProviderKind::Steering)?;
    let home = root.command_home("steering-overflow", &server.endpoint, true, None)?;
    let mut session = Terminal::spawn(&home, &["initial assignment"], 90, 24, None)?;
    session.until(|_| home.join("steering-started").exists())?;
    for index in 0..16 {
        session.command(&format!("steering {index}"))?;
    }
    session.command("keep overflow draft")?;
    session.wait_text("Steering queue is full")?;
    assert!(session.screen().contains("│  keep overflow draft"));
    assert_eq!(server.requests()?.len(), 1);
    session.key(b"\x1b")?;
    session.key(b"\x1b")?;
    session.wait_text("not delivered before this turn ended")?;
    assert!(session.screen().contains("16 queued steering messages"));
    assert!(session.screen().contains("│  keep overflow draft"));
    assert_eq!(
        server.requests()?.len(),
        1,
        "cancelled steering was replayed"
    );
    fs::write(home.join("release-steering"), b"")?;
    session.key(b"\x15")?;
    session.command(":q")?;
    session.finish(0)?;
    server.finish()
}

#[test]
fn slash_commands_run_while_a_turn_is_active() -> io::Result<()> {
    let root = Homes::new()?;
    let server = Provider::start(ProviderKind::Steering)?;
    let home = root.command_home("slash-while-running", &server.endpoint, true, None)?;
    let mut session = Terminal::spawn(&home, &["initial assignment"], 90, 28, None)?;
    session.until(|_| home.join("steering-started").exists())?;
    assert_eq!(server.requests()?.len(), 1);
    session.text("/upd")?;
    session.wait_text("Update Cocoa or pin a release")?;
    capture_session_view(&mut session, "update-completion")?;
    session.key(b"\x15")?;
    session.command("/update --check")?;
    session.wait_text("not installer-managed")?;
    assert_eq!(server.requests()?.len(), 1, "updater contacted the model");
    capture_session_view(&mut session, "update-during-turn")?;
    session.command("/model")?;
    session.wait_text("Alias and model are fixed for this session.")?;
    assert!(
        !session.contains("/model"),
        "executed command left its draft"
    );
    session.command("/models")?;
    session.wait_text("Models · Aliases")?;
    session.key(b"\x1b[C\r")?;
    session.wait_text("Alias and model are fixed for this session.")?;
    assert!(!session.screen().contains("Models · Aliases"));
    assert_eq!(server.requests()?.len(), 1);
    session.command("/bogus")?;
    session.wait_text("Unknown command.")?;
    assert!(
        !session.contains("/bogus"),
        "executed command left its draft"
    );
    session.command("/new")?;
    session.wait_text("Cannot switch sessions while a turn is active.")?;
    assert!(session.contains("│/new"), "blocked command lost its draft");
    session.key(b"\x15")?;
    fs::write(home.join("release-steering"), b"")?;
    session.wait_text("answer-3")?;
    let requests = server.requests()?;
    assert_eq!(requests.len(), 3, "commands issued model requests");
    let entries = requests[2].body["input"]
        .as_array()
        .ok_or_else(|| io::Error::other("missing input history"))?;
    assert!(
        entries.iter().all(|entry| {
            entry["role"] != json!("user")
                || entry["content"]
                    .as_str()
                    .is_none_or(|text| !text.starts_with('/'))
        }),
        "commands reached the model"
    );
    session.command(":q")?;
    session.finish(0)?;
    server.finish()
}

#[test]
fn pasted_image_paths_attach_files_and_keep_failed_paths_editable() -> io::Result<()> {
    let root = Homes::new()?;
    let server = Provider::start(ProviderKind::Commands)?;
    let home = root.command_home("images", &server.endpoint, true, None)?;
    let png_path = home.join("Screen λ (1).png");
    let pixels = image::RgbaImage::from_fn(13, 7, |column, row| {
        image::Rgba([column as u8 * 11, row as u8 * 23, 90, 200])
    });
    pixels.save(&png_path).map_err(io::Error::other)?;
    let jpeg_path = home.join("other image.jpg");
    image::RgbImage::from_pixel(9, 5, image::Rgb([180, 30, 80]))
        .save(&jpeg_path)
        .map_err(io::Error::other)?;
    let mut session = Terminal::spawn(&home, &[], 90, 24, None)?;
    session.wait_text("medium")?;
    let escaped = png_path
        .to_string_lossy()
        .replace(' ', "\\ ")
        .replace('(', "\\(")
        .replace(')', "\\)");
    session.text(&format!("What's in {escaped} "))?;
    session.wait_text("What's in [Image 1]")?;
    assert!(server.requests()?.is_empty(), "paste submitted a prompt");
    session.text(&format!("and \"{}\" ", jpeg_path.display()))?;
    session.wait_text("and [Image 2]")?;
    session.command("compare")?;
    session.wait_text("answer-1")?;
    let requests = server.requests()?;
    let user = &requests[0].body["input"][0];
    assert_eq!(user["role"], "user");
    let parts = user["content"]
        .as_array()
        .ok_or_else(|| io::Error::other("missing image content parts"))?;
    assert_eq!(parts.len(), 3);
    assert_eq!(
        parts[0]["text"],
        "What's in [Image 1] and [Image 2] compare"
    );
    for (index, dimensions) in [(1, (13, 7)), (2, (9, 5))] {
        assert_eq!(parts[index]["type"], "input_image");
        let data = parts[index]["image_url"]
            .as_str()
            .and_then(|data| data.strip_prefix("data:image/png;base64,"))
            .ok_or_else(|| io::Error::other("missing PNG attachment"))?;
        let bytes = STANDARD.decode(data).map_err(io::Error::other)?;
        let decoded = image::load_from_memory(&bytes).map_err(io::Error::other)?;
        assert_eq!((decoded.width(), decoded.height()), dimensions);
        if index == 1 {
            assert_eq!(decoded.to_rgba8(), pixels);
        }
    }

    let url = url::Url::from_file_path(&png_path)
        .map_err(|()| io::Error::other("invalid fixture file URL"))?;
    session.send(format!("{url}\r").as_bytes())?;
    session.wait_text("answer-2")?;
    let requests = server.requests()?;
    assert_eq!(requests[1].body["input"][0], *user);
    assert_eq!(
        requests[1].body["input"][2]["content"][0]["text"],
        "[Image 3]"
    );
    assert_eq!(requests[1].body["input"][2]["content"][1], parts[1]);

    fs::write(home.join("broken.png"), b"not image data")?;
    session.text("./broken.png")?;
    session.wait_text("Cannot decode")?;
    assert!(
        session.contains("./broken.png"),
        "failure discarded the path"
    );
    assert_eq!(server.requests()?.len(), 2);
    session.key(b"\x15")?;
    session.text("./missing.png")?;
    session.wait_text("Cannot read the image file")?;
    assert!(session.contains("./missing.png"));
    session.key(b"\x15")?;
    fs::File::create(home.join("large.png"))?.set_len(8 * 1024 * 1024 + 1)?;
    session.text("./large.png")?;
    session.wait_text("regular file no larger than 8 MiB")?;
    assert!(session.contains("./large.png"));
    session.key(b"\x15")?;
    session.command("text after failed attachments")?;
    session.wait_text("answer-3")?;
    let requests = server.requests()?;
    assert_eq!(
        requests[2].body["input"][4]["content"],
        "text after failed attachments"
    );
    session.command(":q")?;
    session.finish(0)?;
    server.finish()
}

#[test]
fn commands_stay_local_and_preserve_history_model_edits_and_onboarding() -> io::Result<()> {
    let root = Homes::new()?;
    let server = Provider::start(ProviderKind::Commands)?;
    let home = root.command_home("persistent", &server.endpoint, true, None)?;
    let mut session = Terminal::spawn(&home, &["first positional prompt"], 90, 28, None)?;
    session.wait_text("answer-1")?;
    assert_eq!(session.states()?, ["idle", "working", "done"]);
    assert!(!session.exited(), "positional prompt exited after answer");
    let mark = session.transcript.len();
    session.text("/login openr")?;
    session.key(b"\r")?;
    assert!(!has_bytes(&session.transcript[mark..], b"API key"));
    assert_eq!(reports(&session)?.len(), 3, "draft editing changed status");
    session.key(b"\r")?;
    session.until(|session| has_bytes(&session.transcript[mark..], b"API key"))?;
    assert_report(&session, "blocked", Some("auth"))?;
    session.command("dialog-private-sentinel")?;
    let auth_path = home.join(".config/cocoa/auth.json");
    session.until(|_| auth_path.exists())?;
    assert_eq!(
        read_json(&auth_path)?["openrouter"]["api_key"],
        "dialog-private-sentinel"
    );
    session.wait_text("Saved credentials")?;
    assert_report(&session, "idle", None)?;
    session.command("/logout openrouter")?;
    assert_report(&session, "blocked", Some("permission"))?;
    session.key(b"\r")?;
    until_result(&mut session, |_| {
        Ok(read_json(&auth_path)?.get("openrouter").is_none())
    })?;
    session.command("/aliases")?;
    assert_report(&session, "blocked", Some("question"))?;
    session.key(b"\r\r\r")?;
    session.until(|session| saved_model(session, "fixture-model"))?;
    session.key(b"\x1b[B")?;
    assert!(
        saved_model(&session, "fixture-model"),
        "moving the cursor hid the saved model"
    );
    session.key(b"\x1b[A")?;
    let screen = session.screen();
    let (row, line) = screen
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains("fixture-model") && line.contains("✓ saved"))
        .ok_or_else(|| io::Error::other("missing saved model row"))?;
    let column = line
        .find("fixture-model")
        .ok_or_else(|| io::Error::other("missing saved model column"))?;
    let column = line[..column].chars().count() + 1;
    session.key(format!("\x1b[<35;{column};{}M", row + 1).as_bytes())?;
    session.wait_text("Effort")?;
    session.key(b"\x1b[D\x1b[D")?;
    session.key(b"\r\r")?;
    until_result(&mut session, |session| {
        Ok(configuration(session)?["aliases"]["medium"][0]["effort"] == "low")
    })?;
    session.wait_text("/ Filter")?;
    session.key(b"\x1b")?;
    session.key(b"\x1b[F")?;
    session.key(b"\r")?;
    session.command("medium")?;
    session.wait_text("already exists")?;
    session.key(b"\x1b")?;
    session.key(b"\x1b[B")?;
    session.key(b"\r")?;
    session.command("cancelled-alias")?;
    session.key(b"\x1b")?;
    session.key(b"\x1b")?;
    assert!(
        configuration(&session)?["aliases"]
            .get("cancelled-alias")
            .is_none()
    );
    session.key(b"\x1b[B")?;
    session.key(b"\r")?;
    session.command("z-custom-alias")?;
    session.text("not-in-catalogue")?;
    session.key(b"\r")?;
    session.command(&"x".repeat(257))?;
    session.wait_text("Input exceeds 256 bytes")?;
    assert!(
        configuration(&session)?["aliases"]
            .get("z-custom-alias")
            .is_none()
    );
    session.key(b"\x15")?;
    session.command("local-model")?;
    session.key(b"\r\r")?;
    until_result(&mut session, |session| {
        Ok(configuration(session)?["aliases"]
            .get("z-custom-alias")
            .is_some())
    })?;
    assert_eq!(
        configuration(&session)?["aliases"]["z-custom-alias"],
        json!([{"provider":"openai","model":"local-model","priority":0}])
    );
    session.wait_text("/ Filter")?;
    session.key(b"\r\r")?;
    session.wait_text("z-custom-alias › Model")?;
    assert!(
        saved_model(&session, "local-model"),
        "saving lost the edited alias or saved model"
    );
    session.resize(32, 12)?;
    session.wait_text("openai")?;
    capture_session_view(&mut session, "effort-compact")?;
    session.key(b"\x1b")?;
    session.key(b"\x1b")?;
    session.key(b"\x1b")?;
    let before = configuration(&session)?;
    session.command("/model openai/slash-model none")?;
    session.wait_text("Alias and model are fixed for this session")?;
    assert_eq!(
        configuration(&session)?,
        before,
        "locked model command changed configuration"
    );
    session.command("second prompt")?;
    session.wait_text("answer-2")?;
    assert!(!session.exited());
    close_command_session(&mut session)?;
    assert_report(&session, "done", None)?;
    assert!(!has_bytes(&session.transcript, b"dialog-private-sentinel"));
    let requests = server.requests()?;
    assert_eq!(requests.len(), 2, "local commands reached the model");
    for request in &requests {
        assert_eq!(request.body["model"], "fixture-model");
        assert_eq!(request.authorization, "Bearer fixture-private-key");
        let body = request.body.to_string();
        for forbidden in ["/login", "/model", "dialog-private-sentinel"] {
            assert!(!body.contains(forbidden));
        }
    }
    assert!(
        requests[1]
            .body
            .to_string()
            .contains("first positional prompt")
    );
    assert!(requests[1].body.to_string().contains("second prompt"));

    let home = root.command_home("onboarding", &server.endpoint, false, None)?;
    let auth_path = home.join(".config/cocoa/auth.json");
    fs::write(
        &auth_path,
        serde_json::to_vec(
            &json!({"openai":{"type":"api-key","api_key":"initial-onboarding-key"}}),
        )?,
    )?;
    fs::set_permissions(&auth_path, fs::Permissions::from_mode(0o600))?;
    let mut onboarding = Terminal::spawn(&home, &["aliases"], 90, 28, None)?;
    onboarding.until(|session| has_bytes(&session.transcript, b"+ Add alias"))?;
    assert_report(&onboarding, "blocked", Some("question"))?;
    onboarding.key(b"\r")?;
    onboarding.command("fresh")?;
    onboarding.wait_text("+ Enter model ID")?;
    let screen = onboarding.screen();
    let provider_index = screen
        .lines()
        .filter(|line| line.contains("+ Enter model ID"))
        .position(|line| line.contains("openai"))
        .ok_or_else(|| io::Error::other(format!("missing openai manual entry: {screen}")))?;
    for _ in 0..provider_index {
        onboarding.key(b"\x1b[B")?;
    }
    onboarding.key(b"\r")?;
    onboarding.wait_text("Model ID · openai")?;
    onboarding.command("onboarding-model")?;
    onboarding.key(b"\r\r")?;
    until_result(&mut onboarding, |session| {
        Ok(configuration(session)?["default_alias"] == "fresh")
    })?;
    onboarding.key(b"\x1b")?;
    onboarding.key(b"\x1b")?;
    onboarding.command("/login openai")?;
    onboarding.command("onboarding-private-sentinel")?;
    until_result(&mut onboarding, |_| {
        Ok(read_json(&auth_path)?["openai"]["api_key"] == "onboarding-private-sentinel")
    })?;
    onboarding.until(|session| {
        session
            .screen()
            .lines()
            .any(|line| line.contains(" fresh "))
    })?;
    onboarding.command("onboarding prompt")?;
    onboarding.wait_text("answer-3")?;
    close_command_session(&mut onboarding)?;
    assert_report(&onboarding, "done", None)?;
    assert!(!has_bytes(
        &onboarding.transcript,
        b"onboarding-private-sentinel"
    ));
    let requests = server.requests()?;
    assert_eq!(requests.len(), 3, "onboarding dialogs reached the model");
    assert_eq!(requests[2].body["model"], "onboarding-model");
    assert_eq!(
        requests[2].authorization,
        "Bearer onboarding-private-sentinel"
    );
    server.finish()
}

#[test]
fn model_commands_pick_catalogue_and_manual_models_without_saving() -> io::Result<()> {
    let root = Homes::new()?;
    let server = Provider::start(ProviderKind::Commands)?;
    let home = root.command_home("model-picker", &server.endpoint, true, None)?;
    let path = home.join(".config/cocoa/config.json");
    let before = fs::read(&path)?;
    let mut session = Terminal::spawn(&home, &[], 100, 28, None)?;
    session.wait_text(" medium ")?;
    session.command("/model openai/direct-model")?;
    session.wait_text("direct-model")?;
    session.command("/model")?;
    session.wait_text("Select model")?;
    session.key(b"\x1b")?;
    session.until(|session| !session.screen().contains("Select model"))?;
    assert!(session.contains("direct-model"));
    session.command("/model")?;
    session.wait_text("Select model")?;
    session.text("fixture-model")?;
    session.wait_text("←→ effort")?;
    session.key(b"\x1b[C\x1b[C\x1b[C")?;
    session.key(b"\r")?;
    session.until(|session| !session.screen().contains("Select model"))?;
    session.wait_text("fixture-model low")?;
    assert!(!session.screen().contains("Thinking"));
    session.command("/model")?;
    session.wait_text("Select model")?;
    session.text("manual-only-sentinel")?;
    session.key(b"\r")?;
    session.wait_text("Model ID · openai")?;
    session.command("org/manual-model")?;
    session.wait_text("org/manual-model › Thinking")?;
    for _ in 0..5 {
        session.key(b"\x1b[B")?;
    }
    session.key(b"\r")?;
    session.wait_text("org/manual-model")?;
    assert!(
        server.requests()?.is_empty(),
        "model commands reached the provider"
    );
    assert_eq!(fs::read(&path)?, before);
    session.command("first model prompt")?;
    session.wait_text("answer-1")?;
    for command in ["/model", "/model openai/other-model", "/model medium"] {
        session.command(command)?;
        session.wait_text("Alias and model are fixed for this session")?;
        assert!(!session.screen().contains("Select model"));
        assert!(session.contains("org/manual-model"));
    }
    session.command("second model prompt")?;
    session.wait_text("answer-2")?;
    let requests = server.requests()?;
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert_eq!(request.body["model"], "org/manual-model");
        assert_eq!(request.body["reasoning"]["effort"], "high");
        let inputs = request.body["input"]
            .as_array()
            .ok_or_else(|| io::Error::other("missing request input"))?;
        for input in inputs.iter().filter(|input| input["role"] == "user") {
            assert!(!input.to_string().contains("/model"));
        }
    }
    assert!(requests[1].body.to_string().contains("first model prompt"));
    assert_eq!(fs::read(&path)?, before);
    close_command_session(&mut session)?;
    server.finish()
}

#[test]
fn model_picker_manual_entries_only_offer_connected_providers_and_select_their_provider()
-> io::Result<()> {
    let root = Homes::new()?;
    let server = Provider::start(ProviderKind::Commands)?;
    let home = root.command_home("connected-manual-models", &server.endpoint, true, None)?;
    let path = home.join(".config/cocoa/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path)?)?;
    config["providers"]
        .as_object_mut()
        .ok_or_else(|| io::Error::other("missing providers"))?
        .remove("ollama");
    config["providers"]["baseten"]["credential_env"] = json!("COCOA_TEST_KEY");
    fs::write(&path, serde_json::to_vec(&config)?)?;
    let before = fs::read(&path)?;
    let mut session = Terminal::spawn(&home, &[], 100, 32, None)?;
    session.wait_text(" medium ")?;
    session.command("/model")?;
    session.wait_text("Select model")?;
    session.text("manual-only-sentinel")?;
    session.wait_text("+ Enter model ID")?;
    let screen = session.screen();
    let rows: Vec<_> = screen
        .lines()
        .filter(|line| line.contains("+ Enter model ID"))
        .collect();
    assert_eq!(rows.len(), 2, "{screen}");
    assert!(rows.iter().any(|line| line.contains("openai")), "{screen}");
    let index = rows
        .iter()
        .position(|line| line.contains("baseten"))
        .ok_or_else(|| io::Error::other(format!("missing baseten manual entry: {screen}")))?;
    for _ in 0..index {
        session.key(b"\x1b[B")?;
    }
    session.key(b"\r")?;
    session.wait_text("Model ID · baseten")?;
    session.command("connected-manual-model")?;
    session.wait_text("connected-manual-model › Thinking")?;
    session.key(b"\r")?;
    session.until(|session| !session.screen().contains("Select model"))?;
    session.command("selected provider prompt")?;
    session.wait_text("answer-1")?;
    let requests = server.requests()?;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].body["model"], "connected-manual-model");
    assert_eq!(requests[0].authorization, "Bearer fixture-key");
    assert_eq!(fs::read(&path)?, before);
    close_command_session(&mut session)?;
    server.finish()
}

#[test]
fn model_picker_offers_declared_local_provider_without_credentials() -> io::Result<()> {
    let root = Homes::new()?;
    let server = Provider::start(ProviderKind::Commands)?;
    let home = root.command_home("local-manual-model", &server.endpoint, false, None)?;
    let path = home.join(".config/cocoa/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path)?)?;
    config["aliases"] =
        json!({"medium":[{"provider":"ollama","model":"local-model","priority":0}]});
    config["default_alias"] = json!("medium");
    fs::write(&path, serde_json::to_vec(&config)?)?;
    let mut session = Terminal::spawn(&home, &[], 100, 32, None)?;
    session.wait_text(" medium ")?;
    session.command("/model")?;
    session.wait_text("Select model")?;
    session.text("manual-only-sentinel")?;
    session.wait_text("+ Enter model ID")?;
    let screen = session.screen();
    let rows: Vec<_> = screen
        .lines()
        .filter(|line| line.contains("+ Enter model ID"))
        .collect();
    assert_eq!(rows.len(), 1, "{screen}");
    assert!(rows[0].contains("ollama"), "{screen}");
    session.key(b"\r")?;
    session.wait_text("Model ID · ollama")?;
    assert!(server.requests()?.is_empty());
    assert!(!home.join(".config/cocoa/auth.json").exists());
    session.key(b"\x1b")?;
    session.key(b"\x1b")?;
    close_command_session(&mut session)?;
    server.finish()
}

#[test]
fn alias_picker_selects_saved_models_without_changing_effort_or_configuration() -> io::Result<()> {
    let root = Homes::new()?;
    let server = Provider::start(ProviderKind::Commands)?;
    let home = root.command_home("inline-model", &server.endpoint, true, None)?;
    let path = home.join(".config/cocoa/config.json");
    let mut settings: Value = serde_json::from_slice(&fs::read(&path)?)?;
    settings["aliases"] = json!({
        "first":[
            {"provider":"openai","model":"fallback-hidden","effort":"max","priority":0,"fallback_only":true},
            {"provider":"openai","model":"alternate","effort":"high","priority":9},
            {"provider":"openai","model":"first-model","effort":"low","priority":2},
            {"provider":"openai","model":"equal-priority","effort":"medium","priority":9}
        ],
        "second":[{"provider":"openai","model":"second-model","effort":"high","priority":0}],
        "local":[{"provider":"ollama","model":"local-model","priority":0}]
    });
    settings["default_alias"] = json!("first");
    let before = serde_json::to_vec(&settings)?;
    fs::write(&path, &before)?;

    for (index, (scenario, width, right, left, model, effort)) in [
        ("default", 90, 0, 0, "first-model", "low"),
        ("selected", 90, 1, 0, "alternate", "high"),
        ("narrow", 44, 12, 0, "equal-priority", "medium"),
        ("compact", 32, 1, 0, "alternate", "high"),
        ("left-boundary", 90, 12, 12, "first-model", "low"),
        ("cancel", 90, 1, 0, "first-model", "low"),
    ]
    .into_iter()
    .enumerate()
    {
        let mut session = Terminal::spawn(&home, &[], width, 28, None)?;
        session.wait_text(" first ")?;
        session.command("/models")?;
        session.wait_text("←→ model")?;
        assert!(!session.screen().contains("Effort  "));
        assert!(!session.screen().contains("fallback-hidden"));
        for line in session.screen().lines().filter(|line| {
            line.contains(" first ") || line.contains(" local ") || line.contains(" second ")
        }) {
            assert!(
                !line.contains("openai/"),
                "model detail beside alias: {line}"
            );
            assert!(
                !line.contains("ollama/"),
                "model detail beside alias: {line}"
            );
        }
        for _ in 0..right {
            session.key(b"\x1b[C")?;
        }
        for _ in 0..left {
            session.key(b"\x1b[D")?;
        }
        session.key(b"\x1b[B\x1b[C\x1b[A")?;
        capture_session_view(&mut session, &format!("alias-model-{scenario}"))?;
        if scenario == "cancel" {
            session.key(b"\x1b")?;
        } else {
            session.key(b"\r")?;
        }
        session.until(|session| !session.screen().contains("←→ model"))?;
        assert_eq!(
            server.requests()?.len(),
            index,
            "selection started inference"
        );
        assert_eq!(fs::read(&path)?, before);
        session.command("use the chosen saved model")?;
        session.wait_text(&format!("answer-{}", index + 1))?;
        let requests = server.requests()?;
        assert_eq!(requests[index].body["model"], model);
        assert_eq!(requests[index].body["reasoning"]["effort"], effort);
        close_command_session(&mut session)?;
        assert_eq!(fs::read(&path)?, before);
    }

    let mut session = Terminal::spawn(&home, &[], 90, 28, None)?;
    session.wait_text(" first ")?;
    session.command("/models")?;
    session.wait_text("←→ model")?;
    session.key(b"\x1b[B\x1b[C\x1b[C")?;
    capture_session_view(&mut session, "alias-model-single")?;
    let scale = session
        .screen()
        .lines()
        .find(|line| line.contains("Model 1/1"))
        .unwrap_or_default()
        .to_owned();
    assert!(scale.contains("ollama/local-model · default"), "{scale}");
    session.key(b"\r")?;
    session.wait_text(" local ")?;
    close_command_session(&mut session)?;
    assert_eq!(server.requests()?.len(), 6);
    assert_eq!(fs::read(&path)?, before);
    server.finish()
}

#[test]
fn stale_selection_does_not_hide_global_configuration_errors() -> io::Result<()> {
    let root = Homes::new()?;
    let server = Provider::start(ProviderKind::Commands)?;
    let home = root.command_home(
        "stale-selection-error",
        &server.endpoint,
        true,
        Some(json!({})),
    )?;
    let mut first = Terminal::spawn(&home, &["--model", "openai/remembered"], 100, 28, None)?;
    first.wait_text("remembered")?;
    close_command_session(&mut first)?;
    fs::write(
        home.join(".config/cocoa/config.json"),
        serde_json::to_vec(&json!({"providers":{},"input_bytes":0}))?,
    )?;
    let mut second = Terminal::spawn(&home, &[], 100, 28, None)?;
    second.wait_text("invalid bounds")?;
    assert!(!second.screen().contains("Choose a provider"));
    assert!(!second.screen().contains("Select model"));
    close_command_session(&mut second)?;
    assert!(server.requests()?.is_empty());
    server.finish()?;
    Ok(())
}

#[test]
fn alias_free_startup_picks_and_remembers_a_model() -> io::Result<()> {
    let root = Homes::new()?;
    let server = Provider::start(ProviderKind::Commands)?;
    let home = root.command_home("remembered-model", &server.endpoint, true, Some(json!({})))?;
    let path = home.join(".config/cocoa/config.json");
    let before = fs::read(&path)?;
    let mut first = Terminal::spawn(&home, &["initial model prompt"], 100, 28, None)?;
    first.wait_text("Select model")?;
    first.wait_text("+ Enter model ID")?;
    assert!(server.requests()?.is_empty());
    first.key(b"\r")?;
    first.wait_text("Model ID · openai")?;
    first.command("remembered-model")?;
    first.wait_text("remembered-model › Thinking")?;
    first.key(b"\r")?;
    first.until(|session| !session.screen().contains("Select model"))?;
    first.wait_text("remembered-model")?;
    first.wait_text("answer-1")?;
    assert_eq!(server.requests()?[0].body["model"], "remembered-model");
    close_command_session(&mut first)?;
    assert_eq!(fs::read(&path)?, before);

    let mut second = Terminal::spawn(&home, &[], 100, 28, None)?;
    second.wait_text("remembered-model")?;
    assert!(!second.screen().contains("Choose a provider"));
    assert!(!second.screen().contains("Select model"));
    second.command("remembered model prompt")?;
    second.wait_text("answer-2")?;
    assert_eq!(server.requests()?[1].body["model"], "remembered-model");
    close_command_session(&mut second)?;
    assert_eq!(fs::read(&path)?, before);

    fs::write(home.join(".config/cocoa/selection.json"), b"invalid JSON")?;
    let mut third = Terminal::spawn(&home, &[], 100, 28, None)?;
    third.wait_text("Select model")?;
    third.key(b"\x1b")?;
    close_command_session(&mut third)?;
    assert_eq!(server.requests()?.len(), 2);
    server.finish()?;
    Ok(())
}

#[test]
fn explicit_models_skip_onboarding_and_alias_changes_clear_the_override() -> io::Result<()> {
    let root = Homes::new()?;
    for (name, aliases, change, expected) in [
        ("no-aliases", json!({}), "", "org/picked-model"),
        (
            "choose-alias",
            json!({"medium":[{"provider":"openai","model":"alias-model","priority":0}]}),
            "/model medium",
            "alias-model",
        ),
        (
            "cycle-alias",
            json!({"medium":[{"provider":"openai","model":"alias-model","priority":0}]}),
            "cycle",
            "alias-model",
        ),
    ] {
        let server = Provider::start(ProviderKind::Commands)?;
        let home = root.command_home(name, &server.endpoint, true, Some(aliases))?;
        let path = home.join(".config/cocoa/config.json");
        let before = fs::read(&path)?;
        let mut session = Terminal::spawn(
            &home,
            &["--model", "openai/org/initial-model"],
            100,
            28,
            None,
        )?;
        session.wait_text("org/initial-model")?;
        assert!(!session.screen().contains("Choose a provider"));
        if name == "no-aliases" {
            session.command("/models")?;
            session.wait_text("Select model")?;
            assert!(!session.screen().contains("Models · Aliases"));
            session.key(b"\x1b")?;
            session.until(|session| !session.screen().contains("Select model"))?;
            assert!(session.screen().contains("org/initial-model"));
            session.key(b"\x10")?;
            session.wait_text("Select model")?;
            assert!(!session.screen().contains("Models · Aliases"));
            session.wait_text("+ Enter model ID")?;
            capture_session_view(&mut session, "no-aliases-model-browser")?;
            session.key(b"\r")?;
            session.wait_text("Model ID · openai")?;
            session.command("org/picked-model")?;
            session.wait_text("org/picked-model › Thinking")?;
            session.key(b"\r")?;
            session.until(|session| !session.screen().contains("Select model"))?;
            session.wait_text("org/picked-model")?;
        }
        if change == "cycle" {
            session.key(b"\x14")?;
        } else if !change.is_empty() {
            session.command(change)?;
        }
        if !change.is_empty() {
            session.wait_text(" medium ")?;
        }
        assert!(server.requests()?.is_empty());
        session.command("explicit model prompt")?;
        session.wait_text("answer-1")?;
        let requests = server.requests()?;
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].body["model"], expected);
        assert_eq!(fs::read(&path)?, before);
        close_command_session(&mut session)?;
        server.finish()?;
    }

    let home = root.0.join("unconfigured-model");
    write_home(&home, &json!({}), true)?;
    let mut session = Terminal::spawn(
        &home,
        &["--model", "ollama/vendor/model:latest"],
        100,
        28,
        None,
    )?;
    session.wait_text("vendor/model:latest")?;
    assert!(!session.screen().contains("Choose a provider"));
    close_command_session(&mut session)?;
    assert_eq!(fs::read(home.join(".config/cocoa/config.json"))?, b"{}");
    assert!(!home.join(".config/cocoa/auth.json").exists());
    Ok(())
}

#[test]
fn explicit_thinking_levels_reach_explicit_model_requests() -> io::Result<()> {
    let root = Homes::new()?;
    let server = Provider::start(ProviderKind::Commands)?;
    let home = root.command_home("explicit-thinking", &server.endpoint, true, None)?;
    let path = home.join(".config/cocoa/config.json");
    let before = fs::read(&path)?;
    let mut session = Terminal::spawn(
        &home,
        &[
            "--model",
            "openai/org/initial-model",
            "--thinking",
            "medium",
        ],
        100,
        28,
        None,
    )?;
    session.wait_text("org/initial-model")?;
    session.command("explicit thinking prompt")?;
    session.wait_text("answer-1")?;
    let requests = server.requests()?;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].body["model"], "org/initial-model");
    assert_eq!(requests[0].body["reasoning"]["effort"], "medium");
    close_command_session(&mut session)?;
    assert_eq!(fs::read(&path)?, before);

    let mut resumed = Terminal::spawn(&home, &["resume", "--last"], 100, 28, None)?;
    resumed.wait_text("org/initial-model")?;
    assert_eq!(server.requests()?.len(), 1);
    resumed.command("continue with explicit thinking")?;
    resumed.wait_text("answer-2")?;
    let requests = server.requests()?;
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].body["model"], "org/initial-model");
    assert_eq!(requests[1].body["reasoning"]["effort"], "medium");
    close_command_session(&mut resumed)?;
    assert_eq!(fs::read(&path)?, before);
    server.finish()
}

#[test]
fn model_picker_selects_aliases_without_the_removed_alias_command() -> io::Result<()> {
    let root = Homes::new()?;
    let server = Provider::start(ProviderKind::Commands)?;
    let aliases = [
        "z-review",
        "high",
        "low",
        "a-review",
        "medium",
        "high-review",
    ]
    .into_iter()
    .map(|name| {
        (
            name.to_owned(),
            json!([{"provider":"openai","model":format!("select-{name}"),"priority":0}]),
        )
    })
    .collect::<serde_json::Map<String, Value>>();
    let home = root.command_home(
        "selecting",
        &server.endpoint,
        true,
        Some(Value::Object(aliases)),
    )?;
    let mut session = Terminal::spawn(&home, &[], 96, 28, None)?;
    session.wait_text(" medium ")?;
    let before = fs::read(home.join(".config/cocoa/config.json"))?;
    session.text("/")?;
    session.wait_text("/models")?;
    capture_session_view(&mut session, "model-commands")?;
    assert!(session.screen().contains("/aliases"));
    assert!(
        !session
            .screen()
            .split_whitespace()
            .any(|word| word == "/alias")
    );
    session.key(b"\x15")?;
    session.command("/alias low")?;
    session.wait_text("Unknown command.")?;
    assert!(session.screen().contains(" medium "));
    session.command("/models")?;
    session.wait_text("Models · Aliases")?;
    let screen = session.screen();
    let mut previous = 0;
    for name in [
        "low",
        "medium",
        "high",
        "a-review",
        "high-review",
        "z-review",
    ] {
        let position = screen
            .lines()
            .position(|line| line.split_whitespace().any(|word| word == name))
            .ok_or_else(|| io::Error::other(format!("missing alias {name}: {screen}")))?;
        assert!(position > previous, "wrong alias order: {screen}");
        previous = position;
    }
    assert!(!screen.contains("+ Add alias"));
    session.key(b"\x1b")?;
    session.until(|session| !session.screen().contains("Models · Aliases"))?;
    assert!(
        session.screen().contains(" medium "),
        "cancel changed selection"
    );
    session.command("/models")?;
    session.wait_text("Models · Aliases")?;
    session.key(b"\x1b[B\x1b[B\r")?;
    session.wait_text(" high ")?;
    for command in ["/model missing", "/model High"] {
        session.command(command)?;
        session.wait_text("unknown alias")?;
        assert!(
            session.screen().contains(" high "),
            "unknown name changed selection"
        );
    }
    session.command("/model low")?;
    session.wait_text(" low ")?;
    session.command("/model h")?;
    assert!(
        session.screen().contains(" low "),
        "completion executed immediately"
    );
    session.key(b"\r")?;
    session.wait_text(" high ")?;
    session.command("/models")?;
    session.wait_text("Models · Aliases")?;
    session.text("z-review")?;
    session.key(b"\r")?;
    session.wait_text(" z-review ")?;
    session.key(b"\x14")?;
    session.wait_text(" z-review ")?;
    session.command("/model high")?;
    session.wait_text(" high ")?;
    assert_eq!(fs::read(home.join(".config/cocoa/config.json"))?, before);
    assert!(
        server.requests()?.is_empty(),
        "alias selection reached the provider"
    );
    session.command("first selection prompt")?;
    session.wait_text("answer-1")?;
    session.command("/alias low")?;
    session.wait_text("Unknown command.")?;
    for command in ["/model", "/model low"] {
        session.command(command)?;
        session.wait_text("Alias and model are fixed for this session")?;
        assert!(!session.screen().contains("Models · Aliases"));
        assert!(session.screen().contains(" high "));
    }
    session.command("second selection prompt")?;
    session.wait_text("answer-2")?;
    let requests = server.requests()?;
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert_eq!(request.body["model"], "select-high");
        assert!(!request.body.to_string().contains("/alias"));
    }
    assert!(
        requests[1]
            .body
            .to_string()
            .contains("first selection prompt")
    );
    assert_eq!(fs::read(home.join(".config/cocoa/config.json"))?, before);
    close_command_session(&mut session)?;
    server.finish()
}

#[test]
fn alias_editor_saves_priorities_and_preserves_list_order() -> io::Result<()> {
    let root = Homes::new()?;
    let server = Provider::start(ProviderKind::Commands)?;
    let home = root.command_home(
        "priorities",
        &server.endpoint,
        true,
        Some(json!({"medium":[
            {"provider":"openai","model":"first","effort":"low","priority":10},
            {"provider":"openai","model":"second","effort":"high","priority":20}
        ]})),
    )?;
    let mut session = Terminal::spawn(&home, &[], 96, 28, None)?;
    session.wait_text("first low")?;
    session.command("/aliases")?;
    session.wait_text("+ Add alias")?;
    let before = configuration(&session)?;
    session.key(b"\x1b[1;3A\x1b[1;3B")?;
    assert_eq!(configuration(&session)?, before, "alias list was reordered");
    session.key(b"\x1b[H\r")?;
    session.wait_text("medium › Model choices")?;
    session.wait_text("priority 10 · low")?;
    session.wait_text("priority 20 · high")?;
    capture_session_view(&mut session, "alias-choices")?;
    session.key(b"\x1b[B\r")?;
    session.wait_text("Edit priority")?;
    session.key(b"\x1b[B\r")?;
    session.wait_text("medium › Priority")?;
    capture_session_view(&mut session, "alias-priority")?;
    session.key(b"\x15")?;
    session.command("100")?;
    session.wait_text("Enter a priority from 0 to 99")?;
    capture_session_view(&mut session, "alias-priority-invalid")?;
    assert_eq!(
        configuration(&session)?,
        before,
        "invalid priority was saved"
    );
    session.key(b"\x15")?;
    session.command("-1")?;
    session.wait_text("Enter a priority from 0 to 99")?;
    assert_eq!(
        configuration(&session)?,
        before,
        "invalid fallback was saved"
    );
    let lock = fs::File::create(home.join(".config/cocoa/config.json.lock"))?;
    lock.try_lock()?;
    session.key(b"\x15")?;
    session.command("0")?;
    session.wait_text("configuration is busy")?;
    assert_eq!(
        configuration(&session)?,
        before,
        "failed save changed choices"
    );
    lock.unlock()?;
    session.key(b"\r")?;
    session.wait_text("✓ Alias saved")?;
    let saved = configuration(&session)?;
    assert_eq!(saved["aliases"]["medium"][0]["model"], "first");
    assert_eq!(saved["aliases"]["medium"][0]["priority"], 10);
    assert_eq!(saved["aliases"]["medium"][1]["model"], "second");
    assert_eq!(saved["aliases"]["medium"][1]["priority"], 0);
    assert_eq!(saved["aliases"]["medium"][1]["effort"], "high");
    session.key(b"\x1b")?;
    session.key(b"\x1b")?;
    session.wait_text("second high")?;
    session.key(b"\x14")?;
    session.wait_text("first low")?;
    assert!(server.requests()?.is_empty(), "editing started inference");
    close_command_session(&mut session)?;
    server.finish()
}

#[test]
fn commands_cycle_regular_choices_without_moving_drafts_or_changing_locked_sessions()
-> io::Result<()> {
    let root = Homes::new()?;
    let server = Provider::start(ProviderKind::Commands)?;
    let home = root.command_home(
        "cycling",
        &server.endpoint,
        true,
        Some(json!({
            "low":[
                {"provider":"openai","model":"fallback-first","priority":0,"fallback_only":true},
                {"provider":"openai","model":"cycle-high","priority":2},
                {"provider":"openai","model":"cycle-low","priority":0},
                {"provider":"openai","model":"cycle-medium","priority":1},
                {"provider":"openai","model":"fallback-last","priority":99,"fallback_only":true},
                {"provider":"openai","model":"cycle-x-high","priority":2},
                {"provider":"openai","model":"cycle-ultra","priority":3},
                {"provider":"openai","model":"cycle-a-review","priority":4},
                {"provider":"openai","model":"cycle-z-review","priority":5},
                {"provider":"openai","model":"cycle-x-low","priority":99}
            ],
            "other":[{"provider":"openai","model":"not-in-this-cycle","priority":0}]
        })),
    )?;
    let path = home.join(".config/cocoa/config.json");
    let mut saved: Value = serde_json::from_slice(&fs::read(&path)?)?;
    saved["default_alias"] = json!("low");
    fs::write(&path, serde_json::to_vec(&saved)?)?;
    let mut cycling = Terminal::spawn(&home, &["--alias", "low"], 90, 28, None)?;
    cycling.wait_text("cycle-low")?;
    let before = configuration(&cycling)?;
    cycling.key(b"\x14")?;
    cycling.wait_text("cycle-medium")?;
    cycling.command("/model low")?;
    cycling.wait_text("cycle-low")?;
    cycling.command("/model openai/direct-model")?;
    cycling.wait_text("direct-model")?;
    cycling.key(b"\x14")?;
    cycling.wait_text("cycle-low")?;
    cycling.command("/model low")?;
    cycling.key(b"\x14")?;
    cycling.wait_text("cycle-medium")?;
    cycling.command("/model low")?;
    cycling.text("cycle draft")?;
    cycling.key(b"\x1b[D")?;
    for name in [
        "medium", "high", "x-high", "ultra", "a-review", "z-review", "x-low", "low", "medium",
        "high",
    ] {
        cycling.key(b"\x14")?;
        cycling.until(|session| {
            session
                .screen()
                .lines()
                .any(|line| line.contains(&format!("cycle-{name}")))
        })?;
    }
    for name in [
        "medium", "low", "x-low", "z-review", "a-review", "ultra", "x-high", "high",
    ] {
        cycling.key(b"\x1b[84;6u")?;
        cycling.until(|session| {
            session
                .screen()
                .lines()
                .any(|line| line.contains(&format!("cycle-{name}")))
        })?;
    }
    assert_eq!(
        configuration(&cycling)?,
        before,
        "cycling changed configuration"
    );
    assert!(server.requests()?.is_empty(), "cycling started model work");
    cycling.key(b"!\r")?;
    cycling.until(|_| server.events.cycle_started.load(Ordering::SeqCst))?;
    cycling.text("next draft")?;
    cycling.key(b"\x14")?;
    assert!(
        cycling
            .screen()
            .lines()
            .any(|line| line.contains("cycle-high")),
        "busy shortcut changed models"
    );
    server.events.cycle_release.store(true, Ordering::SeqCst);
    cycling.wait_text("answer-1")?;
    cycling.key(b"\x14")?;
    cycling.wait_text("Alias and model are fixed for this session")?;
    assert!(
        cycling
            .screen()
            .lines()
            .any(|line| line.contains("cycle-high")),
        "completed session changed models"
    );
    cycling.key(b"\r")?;
    cycling.wait_text("answer-2")?;
    let requests = server.requests()?;
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert_eq!(request.body["model"], "cycle-high");
        assert_eq!(request.authorization, "Bearer fixture-private-key");
    }
    assert!(
        requests[0].body.to_string().contains("cycle draf!t"),
        "cycling moved the draft cursor"
    );
    assert!(
        requests[1].body.to_string().contains("cycle draf!t"),
        "cycling reset session history"
    );
    assert!(
        requests[1].body.to_string().contains("next draft"),
        "busy or locked shortcut changed the draft"
    );
    close_command_session(&mut cycling)?;
    for (name, configured) in [("single-alias", true), ("no-aliases", false)] {
        let home = root.command_home(name, &server.endpoint, configured, None)?;
        let mut idle = Terminal::spawn(&home, &[], 90, 28, None)?;
        idle.wait_text(if configured { "medium" } else { "Select model" })?;
        if configured {
            idle.command("/models")?;
            idle.wait_text("Models · Aliases")?;
            idle.key(b"\r")?;
            idle.key(b"\x14")?;
            idle.wait_text(" medium ")?;
            close_command_session(&mut idle)?;
        } else {
            idle.wait_text("Select model")?;
            idle.key(b"\x14")?;
            idle.wait_text("Select model")?;
            idle.key(b"\x1b")?;
            close_command_session(&mut idle)?;
        }
        assert_eq!(
            server.requests()?.len(),
            2,
            "idle cycling reached the model"
        );
    }
    server.finish()
}

#[test]
fn aliases_manage_model_choices_and_select_a_default() -> io::Result<()> {
    let root = Homes::new()?;
    let server = Provider::start(ProviderKind::Commands)?;
    let home = root.command_home("alias-manager", &server.endpoint, true, None)?;
    let path = home.join(".config/cocoa/config.json");
    let mut session = Terminal::spawn(&home, &[], 90, 28, None)?;
    session.wait_text(" medium ")?;
    session.command("/models")?;
    session.wait_text("Models · Aliases")?;
    capture_session_view(&mut session, "alias-picker")?;
    session.key(b"\x1b")?;
    session.command("/aliases")?;
    session.wait_text("+ Add alias")?;
    capture_session_view(&mut session, "alias-manager")?;
    let before = fs::read(&path)?;
    session.key(b"\x1b[F\r")?;
    session.wait_text("Create an alias")?;
    session.command("cancelled")?;
    session.key(b"\x1b")?;
    session.key(b"\x1b")?;
    session.wait_text("+ Add alias")?;
    assert_eq!(fs::read(&path)?, before, "cancel saved an empty alias");
    session.key(b"\x1b[F\r")?;
    session.command("night")?;
    session.wait_text("night › Model")?;
    session.command("manual-entry-filter")?;
    session.wait_text("Model ID")?;
    session.command("night-model")?;
    session.wait_text("night › Thinking")?;
    session.key(b"\x1b[B\x1b[B\x1b[B\x1b[B\x1b[B\r")?;
    session.wait_text("night › Priority")?;
    session.key(b"\x15")?;
    session.command("99")?;
    session.wait_text("✓ Alias saved")?;
    assert_eq!(
        configuration(&session)?["aliases"]["night"][0]["model"],
        "night-model"
    );
    assert_eq!(
        configuration(&session)?["aliases"]["night"][0]["effort"],
        "high"
    );
    assert_eq!(
        configuration(&session)?["aliases"]["night"][0]["priority"],
        99
    );
    session.key(b"\x1b[F\x1b[A\r")?;
    session.wait_text("No saved model. Choose one below.")?;
    session.command("fixture-model")?;
    session.wait_text("night › Priority")?;
    session.key(b"\x15")?;
    session.command("99")?;
    session.wait_text("✓ Alias saved")?;
    assert_eq!(
        configuration(&session)?["aliases"]["night"][1]["priority"],
        99
    );
    session.key(b"\r")?;
    session.wait_text("night › Edit model")?;
    capture_session_view(&mut session, "alias-fallback-toggle-off")?;
    session.key(b"\x1b[B\x1b[B\r")?;
    session.wait_text("✓ Alias saved")?;
    session.wait_text("fallback 99")?;
    capture_session_view(&mut session, "alias-fallback-editor")?;
    assert_eq!(
        configuration(&session)?["aliases"]["night"][1]["fallback_only"],
        true
    );
    session.key(b"\r")?;
    session.wait_text("night › Edit model")?;
    capture_session_view(&mut session, "alias-fallback-toggle-on")?;
    session.key(b"\x1b[F\r")?;
    session.wait_text("✓ Alias saved")?;
    assert_eq!(
        configuration(&session)?["aliases"]["night"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
    let before = configuration(&session)?;
    session.key(b"\r")?;
    session.wait_text("night › Edit model")?;
    session.key(b"\x1b[B\x1b[B\r")?;
    session.wait_text("at least one regular choice")?;
    assert_eq!(
        configuration(&session)?,
        before,
        "last regular choice became fallback-only"
    );
    capture_session_view(&mut session, "alias-fallback-toggle-rejected")?;
    session.key(b"\x1b")?;
    session.wait_text("night › Model choices")?;
    session.key(b"\x1b[F\r")?;
    until_result(&mut session, |session| {
        Ok(configuration(session)?["default_alias"] == "night")
    })?;
    session.key(b"\x1b")?;
    session.wait_text("+ Add alias")?;
    session.key(b"\x1b")?;
    session.command("/model night")?;
    session.wait_text("night-model high")?;
    session.command("run the saved alias")?;
    session.wait_text("answer-1")?;
    let requests = server.requests()?;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].body["model"], "night-model");
    assert_eq!(requests[0].body["reasoning"], json!({"effort":"high"}));
    session.command("/model medium")?;
    session.wait_text("fixed for this session")?;
    close_command_session(&mut session)?;
    let mut returning = Terminal::spawn(&home, &[], 60, 24, None)?;
    returning.wait_text("night-model high")?;
    returning.command("/models")?;
    returning.wait_text("Models · Aliases")?;
    returning.wait_text("medium")?;
    capture_session_view(&mut returning, "alias-picker-narrow")?;
    returning.key(b"\x1b")?;
    returning.command("/aliases")?;
    returning.command("night")?;
    returning.wait_text("night › Model choices")?;
    returning.key(b"\r")?;
    returning.wait_text("night › Edit model")?;
    returning.key(b"\x1b[F\r")?;
    until_result(&mut returning, |session| {
        Ok(configuration(session)?["aliases"].get("night").is_none())
    })?;
    assert_eq!(configuration(&returning)?["default_alias"], "medium");
    returning.wait_text("+ Add alias")?;
    returning.key(b"\x1b")?;
    close_command_session(&mut returning)?;
    assert_eq!(server.requests()?.len(), 1);
    server.finish()
}

#[test]
fn alias_choices_keep_distinct_efforts_when_cycling_and_resuming() -> io::Result<()> {
    let root = Homes::new()?;
    let server = Provider::start(ProviderKind::Commands)?;
    let home = root.command_home("alias-efforts", &server.endpoint, true, None)?;
    let path = home.join(".config/cocoa/config.json");
    let mut settings: Value = serde_json::from_slice(&fs::read(&path)?)?;
    settings["aliases"] = json!({"medium":[
        {"provider":"openai","model":"same-model","effort":"low","priority":0},
        {"provider":"openai","model":"same-model","effort":"high","priority":0}
    ]});
    fs::write(&path, serde_json::to_vec(&settings)?)?;
    let mut session = Terminal::spawn(&home, &[], 90, 28, None)?;
    session.wait_text("same-model low")?;
    session.key(b"\x14")?;
    session.wait_text("same-model high")?;
    session.command("use high effort")?;
    session.wait_text("answer-1")?;
    close_command_session(&mut session)?;
    let mut resumed = Terminal::spawn(&home, &["resume", "--last"], 90, 28, None)?;
    resumed.wait_text("same-model high")?;
    resumed.command("continue at high effort")?;
    resumed.wait_text("answer-2")?;
    close_command_session(&mut resumed)?;
    let mut low = Terminal::spawn(&home, &[], 90, 28, None)?;
    low.wait_text("same-model low")?;
    low.command("use low effort")?;
    low.wait_text("answer-3")?;
    close_command_session(&mut low)?;
    let requests = server.requests()?;
    assert_eq!(requests.len(), 3);
    for (index, effort) in ["high", "high", "low"].into_iter().enumerate() {
        assert_eq!(requests[index].body["model"], "same-model");
        assert_eq!(requests[index].body["reasoning"], json!({"effort":effort}));
    }
    server.finish()
}

#[test]
fn startup_fallback_updates_the_pinned_model_and_effort_during_work_and_after_resume()
-> io::Result<()> {
    let root = Homes::new()?;
    for model in ["fallback-tools", "fallback-final"] {
        let server = Provider::start(ProviderKind::StartupFallback)?;
        let home = root.command_home(
            model,
            &server.endpoint,
            true,
            Some(json!({"medium":[
                {"provider":"openai","model":"unused-primary","effort":"medium","priority":0},
                {"provider":"openai","model":model,"effort":"high","priority":0,"fallback_only":true},
                {"provider":"openai","model":"failed-primary","effort":"low","priority":1}
            ]})),
        )?;
        let mut session = Terminal::spawn(&home, &[], 110, 30, None)?;
        session.wait_text("unused-primary medium")?;
        session.command("/models")?;
        session.wait_text("←→ model")?;
        session.key(b"\x1b[C\r")?;
        session.wait_text("medium · failed-primary low")?;
        capture_session_view(&mut session, &format!("alias-{model}-before"))?;
        session.command("check the pinned footer")?;
        if model == "fallback-tools" {
            session.until(|session| session.home.join("fallback-tool-started").exists())?;
            session.wait_text("medium · fallback-tools high")?;
            capture_session_view(&mut session, "alias-fallback-tools-working")?;
            assert!(!session.screen().contains("failed-primary"));
            assert!(!session.screen().contains("answer-6"));
            assert_eq!(server.requests()?.len(), 5, "turn already settled");
            fs::write(home.join("release-fallback"), "")?;
        }
        let answer = if model == "fallback-tools" { 6 } else { 5 };
        session.wait_text(&format!("answer-{answer}"))?;
        session.wait_text(&format!("medium · {model} high"))?;
        capture_session_view(&mut session, &format!("alias-{model}-after"))?;
        assert!(!session.screen().contains("failed-primary"));
        session.command("continue with the pinned choice")?;
        session.wait_text(&format!("answer-{}", answer + 1))?;
        session.wait_text(&format!("medium · {model} high"))?;
        close_command_session(&mut session)?;
        let mut resumed = Terminal::spawn(&home, &["resume", "--last"], 110, 30, None)?;
        resumed.wait_text(&format!("medium · {model} high"))?;
        capture_session_view(&mut resumed, &format!("alias-{model}-resumed"))?;
        resumed.command("continue after resume")?;
        resumed.wait_text(&format!("answer-{}", answer + 2))?;
        resumed.wait_text(&format!("medium · {model} high"))?;
        close_command_session(&mut resumed)?;
        let requests = server.requests()?;
        assert_eq!(requests.len(), answer + 2);
        for request in &requests[..4] {
            assert_eq!(request.body["model"], "failed-primary");
            assert_eq!(request.body["reasoning"], json!({"effort":"low"}));
        }
        for request in &requests[4..] {
            assert_eq!(request.body["model"], model);
            assert_eq!(request.body["reasoning"], json!({"effort":"high"}));
        }
        server.finish()?;
    }
    Ok(())
}

#[test]
fn first_run_selects_a_concrete_model_without_creating_aliases() -> io::Result<()> {
    let root = Homes::new()?;
    let home = root.0.join("fresh");
    fs::create_dir(&home)?;
    let mut fresh = Terminal::spawn(&home, &[], 90, 28, None)?;
    fresh.wait_text("Choose a provider")?;
    fresh.key(b"\x03")?;
    fresh.finish(0)?;
    assert!(!home.join(".config/cocoa/config.json").exists());

    let server = Provider::start(ProviderKind::Commands)?;
    for (index, width) in [(1, 90), (2, 60)] {
        let home = root.command_home(&format!("setup-{index}"), &server.endpoint, false, None)?;
        let mut session = Terminal::spawn(
            &home,
            &["--alias", "medium", "first setup assignment"],
            width,
            28,
            None,
        )?;
        let before = configuration(&session)?;
        session.wait_text("Choose a provider")?;
        session.key(b"\x1b[B")?;
        session.key(b"\r")?;
        session.wait_text("API key")?;
        session.command("onboarding-private-sentinel")?;
        session.wait_text("openai › Choose a model")?;
        session.wait_text("Enter model ID")?;
        assert_eq!(
            server.requests()?.len(),
            index - 1,
            "connecting a provider submitted the initial prompt"
        );
        assert!(home.join(".config/cocoa/auth.json").exists());
        assert!(!has_bytes(&session.transcript, b"Ready to work"));
        assert!(!has_bytes(&session.transcript, b"+ Add alias"));
        assert!(!has_bytes(&session.transcript, b"Create an alias"));
        session.key(b"\r")?;
        session.wait_text("openai › Model ID")?;
        session.command("onboarding-model")?;
        session.wait_text(&format!("answer-{index}"))?;
        let after = configuration(&session)?;
        assert_eq!(after["aliases"], before["aliases"]);
        assert_eq!(after["default_alias"], before["default_alias"]);
        let requests = server.requests()?;
        assert_eq!(requests.len(), index);
        let request = &requests[index - 1];
        assert_eq!(request.body["model"], "onboarding-model");
        assert_eq!(
            request.body["input"][0]["content"],
            "first setup assignment"
        );
        assert_eq!(request.authorization, "Bearer onboarding-private-sentinel");
        assert!(!has_bytes(
            &session.transcript,
            b"onboarding-private-sentinel"
        ));
        close_command_session(&mut session)?;
        let mut returning = Terminal::spawn(&home, &[], width, 28, None)?;
        returning.wait_text("onboarding-model")?;
        assert!(!returning.contains("Choose a provider"));
        assert!(!returning.contains("Select model"));
        close_command_session(&mut returning)?;
        let returned = configuration(&returning)?;
        assert_eq!(returned["aliases"], before["aliases"]);
        assert_eq!(returned["default_alias"], before["default_alias"]);
    }
    server.finish()
}

#[test]
fn cancellation_preserves_http_state_and_effects_but_resets_interrupted_javascript()
-> io::Result<()> {
    let root = Homes::new()?;
    let server = Provider::start(ProviderKind::Cancellation)?;
    let mut session = root.cancellation_session("continuation", &server.endpoint, 600_000)?;
    assert_eq!(session.states()?, ["idle"]);
    session.send(b"seed\r")?;
    session.wait_text("Seed ready")?;
    assert_report(&session, "done", None)?;
    assert_eq!(
        fs::read_to_string(session.home.join("existing.txt"))?,
        "retained effect"
    );
    for mode in ["headers", "body"] {
        server.reset_stall();
        session.send(format!("stall-{mode}\r").as_bytes())?;
        session.until(|_| server.events.stalled.load(Ordering::SeqCst))?;
        assert_report(&session, "working", None)?;
        let count = session.states()?.len();
        session.send(b"\x1b")?;
        session.wait_text("Esc again to stop")?;
        assert_eq!(
            session.states()?.len(),
            count,
            "gesture or animation repeated a status"
        );
        session.send(b"\x1b")?;
        session.until(|_| server.events.disconnected.load(Ordering::SeqCst))?;
        wait_state(&mut session, "idle")?;
        assert_cancelled_idle(&mut session)?;
        session.send(format!("after-http-{mode}\r").as_bytes())?;
        session.wait_text(&format!("after-http-{mode} continuation verified"))?;
    }
    session.send(b"busy\r")?;
    session.until(|session| session.home.join("busy-started").exists())?;
    session.send(b"resume-draft")?;
    session.send(b"\x1b")?;
    session.wait_text("Esc again to stop")?;
    session.send(b"\x1b")?;
    wait_state(&mut session, "idle")?;
    assert_cancelled_idle(&mut session)?;
    session.send(b"\r")?;
    session.wait_text("JavaScript reset verified")?;
    assert!(!session.home.join("must-not-run").exists());
    assert_eq!(
        fs::read_to_string(session.home.join("existing.txt"))?,
        "retained effect"
    );
    session.send(b"shell\r")?;
    session.until(|session| session.home.join("shell-started").exists())?;
    session.send(b"\x03")?;
    wait_state(&mut session, "idle")?;
    for name in ["shell.pid", "descendant.pid"] {
        assert!(
            process_exists(read_pid(&session.home.join(name))?)?,
            "turn cancellation terminated the retained shell"
        );
    }
    assert_cancelled_idle(&mut session)?;
    session.send(b"after-shell\r")?;
    session.wait_text("Shell continuation verified")?;
    for name in ["shell.pid", "descendant.pid"] {
        let identity = read_pid(&session.home.join(name))?;
        until_result(&mut session, |_| Ok(!process_exists(identity)?))?;
    }
    assert!(!session.home.join("late-effect").exists());
    session.send(b"\x03")?;
    session.wait_text("Ctrl+C to close")?;
    session.send(b"\x03")?;
    finish_cancellation(&mut session)?;
    assert!(!has_bytes(&session.transcript, b"Turn cancelled"));
    assert!(!session.states()?.iter().any(|state| state == "error"));
    server.finish()
}

#[test]
fn cancellation_distinguishes_draft_clear_exit_and_external_signals() -> io::Result<()> {
    let root = Homes::new()?;
    let server = Provider::start(ProviderKind::Cancellation)?;
    for mode in ["draft-exit", "signals"] {
        let mut session = root.cancellation_session(mode, &server.endpoint, 600_000)?;
        server.reset_stall();
        session.send(b"stall-headers\r")?;
        session.until(|_| server.events.stalled.load(Ordering::SeqCst))?;
        if mode == "draft-exit" {
            session.send(b"discard me\x03")?;
            session.wait_text("Ctrl+C to close")?;
            assert!(
                !server.events.disconnected.load(Ordering::SeqCst),
                "draft clear cancelled the provider request"
            );
            session.send(b"\x03")?;
            finish_cancellation(&mut session)?;
            assert_report(&session, "idle", None)?;
            session.until(|_| server.events.disconnected.load(Ordering::SeqCst))?;
        } else {
            session.interrupt()?;
            session.until(|_| server.events.disconnected.load(Ordering::SeqCst))?;
            wait_state(&mut session, "idle")?;
            assert_cancelled_idle(&mut session)?;
            session.interrupt()?;
            finish_cancellation(&mut session)?;
            assert_report(&session, "idle", None)?;
        }
        assert!(!has_bytes(&session.transcript, b"Turn cancelled"));
        assert!(!session.states()?.iter().any(|state| state == "error"));
    }
    server.finish()
}

#[test]
fn child_http_recovery_preserves_edits_and_continues_after_a_rejected_tool_call() -> io::Result<()>
{
    let root = Homes::new()?;
    let server = Provider::start(ProviderKind::ChildRecovery)?;
    let home = root.command_home("child-recovery", &server.endpoint, true, None)?;
    fs::write(home.join("existing.txt"), "repeat\nrepeat\n")?;
    let mut session = Terminal::spawn(&home, &["recover delegated work"], 110, 32, None)?;
    session.wait_text("Retrying provider request")?;
    capture_session_view(&mut session, "child-provider-retrying")?;
    session.wait_text("Parent finished independently")?;
    session.wait_text("Provider paused")?;
    session.wait_text("HTTP status 503")?;
    wait_state(&mut session, "blocked")?;
    capture_session_view(&mut session, "child-provider-paused")?;
    let child_row = session
        .screen()
        .lines()
        .position(|line| line.contains("Licenses Cli · Provider paused"))
        .ok_or_else(|| io::Error::other("missing paused child header"))?
        + 1;
    session.send(format!("\x1b[<0;8;{child_row}M\x1b[<0;8;{child_row}m").as_bytes())?;
    session.wait_text("Edited existing.txt · failed")?;
    session.send(format!("\x1b[<0;8;{child_row}M\x1b[<0;8;{child_row}m").as_bytes())?;
    session.until(|session| !session.screen().contains("Edited existing.txt · failed"))?;
    assert!(!session.screen().contains("Agent stopped"));
    assert_eq!(fs::read_to_string(home.join("recovery-effects"))?, "x");
    assert_eq!(
        fs::read_to_string(home.join("existing.txt"))?,
        "repeat\nrepeat\n"
    );
    let failed_requests: Vec<_> = server
        .requests()?
        .into_iter()
        .filter(|request| {
            request.body["input"].as_array().is_some_and(|input| {
                input.iter().any(|item| {
                    item["call_id"] == "child-effect" && item["type"] == "function_call_output"
                })
            })
        })
        .collect();
    assert_eq!(
        failed_requests.len(),
        4,
        "initial request plus three automatic retries"
    );
    for request in &failed_requests[1..] {
        assert_eq!(request.body, failed_requests[0].body);
    }
    server.events.cycle_release.store(true, Ordering::SeqCst);
    session.key(b"r")?;
    session.until(|_| {
        fs::read_to_string(home.join("existing.txt")).is_ok_and(|text| text == "fixed\nrepeat\n")
    })?;
    session.wait_text("Work Done")?;
    session.key(b"\t")?;
    session.key(b"\x1b[C")?;
    session.key(b"\x1b[B")?;
    session.key(b"\x1b[C")?;
    session.key(b"\t")?;
    session.key(b"\t")?;
    session.key(b"\x1b[C")?;
    session.wait_text("Child recovered and corrected the edit")?;
    session.until(|session| !session.screen().contains("Provider paused"))?;
    capture_session_view(&mut session, "child-provider-recovered")?;
    assert_eq!(fs::read_to_string(home.join("recovery-effects"))?, "x");
    assert_eq!(
        fs::read_to_string(home.join("existing.txt"))?,
        "fixed\nrepeat\n"
    );
    assert!(!has_bytes(
        &session.transcript,
        b"provider-private-sentinel"
    ));
    session.key(b"\x1b")?;
    close_command_session(&mut session)?;
    server.finish()
}

#[test]
fn parent_completion_preserves_the_selected_child_cancellation() -> io::Result<()> {
    let root = Homes::new()?;
    let server = Provider::start(ProviderKind::ChildRecovery)?;
    let home = root.command_home("child-cancellation", &server.endpoint, true, None)?;
    fs::write(home.join("existing.txt"), "repeat\nrepeat\n")?;
    let mut session = Terminal::spawn(&home, &["cancel delegated work"], 110, 32, None)?;
    session.wait_text("Provider paused")?;
    session.until(|_| server.events.cycle_started.load(Ordering::SeqCst))?;
    session.key(b"\x1b[B")?;
    session.until(|session| {
        session
            .screen()
            .lines()
            .any(|line| line.trim_start().starts_with("▎ Cancel"))
    })?;
    capture_session_view(&mut session, "child-cancel-selected")?;
    let requests = server.requests()?.len();
    server.events.cycle_release.store(true, Ordering::SeqCst);
    session.wait_text("Parent finished independently")?;
    assert!(
        session
            .screen()
            .lines()
            .any(|line| line.trim_start().starts_with("▎ Cancel")),
        "{}",
        session.screen()
    );
    capture_session_view(&mut session, "child-cancel-after-parent")?;
    session.key(b"\r")?;
    session.wait_text("Work Done")?;
    assert_eq!(
        server.requests()?.len(),
        requests,
        "Cancel must not resend the child's provider request"
    );
    assert_eq!(
        fs::read_to_string(home.join("existing.txt"))?,
        "repeat\nrepeat\n"
    );
    assert_eq!(fs::read_to_string(home.join("recovery-effects"))?, "x");
    close_command_session(&mut session)?;
    server.finish()
}

#[test]
fn provider_failures_retry_only_the_request_and_cancel_without_resetting_the_session()
-> io::Result<()> {
    let root = Homes::new()?;
    let server = Provider::start(ProviderKind::Cancellation)?;
    let mut session = root.cancellation_session("failure-recovery", &server.endpoint, 1_000)?;
    session.send(b"seed\r")?;
    session.wait_text("Seed ready")?;
    let initial = session_page(&session.home, None)?;
    assert_eq!(initial["sessions"].as_array().map(Vec::len), Some(1));
    for mode in [
        "fatal",
        "bad-provider",
        "stall-headers",
        "stall-body",
        "failure-with-tools",
    ] {
        server.reset_stall();
        let mark = server.requests()?.len();
        session.send(format!("{mode}\r").as_bytes())?;
        if mode.starts_with("stall-") {
            session.until(|_| server.events.stalled.load(Ordering::SeqCst))?;
            session.send(format!("after-http-{mode}").as_bytes())?;
            session.wait_text(&format!("after-http-{mode}"))?;
        }
        wait_state(&mut session, "blocked")?;
        session.wait_text("Provider paused")?;
        session.wait_text("Retry")?;
        session.wait_text("Cancel")?;
        assert_report(&session, "blocked", Some("question"))?;
        assert!(!session.screen().contains("Session reset"));
        assert!(!has_bytes(
            &session.transcript,
            b"provider-private-sentinel"
        ));
        assert_eq!(
            server.requests()?.len(),
            mark + if mode == "failure-with-tools" { 2 } else { 1 },
            "failure retried automatically"
        );
        let failed_count = server.requests()?.len();
        session.key(b"r")?;
        until_result(&mut session, |_| {
            Ok(server.requests()?.len() == failed_count + 1)
        })?;
        wait_state(&mut session, "blocked")?;
        session.wait_text("Provider paused")?;
        let requests = server.requests()?;
        assert_eq!(
            requests[failed_count - 1].body,
            requests[failed_count].body,
            "retry changed the model request"
        );
        session.key(b"c")?;
        wait_state(&mut session, "idle")?;
        assert!(!session.screen().contains("Session reset"));
        if !mode.starts_with("stall-") {
            session.send(format!("after-http-{mode}").as_bytes())?;
        }
        session.send(b"\r")?;
        session.wait_text(&format!("after-http-{mode} continuation verified"))?;
        wait_state(&mut session, "done")?;
        assert_eq!(
            fs::read_to_string(session.home.join("existing.txt"))?,
            "retained effect"
        );
        if mode == "failure-with-tools" {
            assert_eq!(
                fs::read_to_string(session.home.join("retry-effects"))?,
                "x",
                "retry replayed a completed tool"
            );
        }
    }
    let mark = server.requests()?.len();
    session.send(b"retry-success\r")?;
    session.wait_text("Retrying in 1s · 1/3")?;
    capture_session_view(&mut session, "root-provider-retrying")?;
    wait_state(&mut session, "blocked")?;
    assert_eq!(
        server.requests()?.len(),
        mark + 4,
        "manual recovery appeared before automatic retries were exhausted"
    );
    server.events.cycle_release.store(true, Ordering::SeqCst);
    session.key(b"\r")?;
    session.wait_text("Manual retry succeeded")?;
    wait_state(&mut session, "done")?;
    let continued = session_page(&session.home, None)?;
    assert_eq!(continued["sessions"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        continued["sessions"][0]["public_id"],
        initial["sessions"][0]["public_id"]
    );
    session.send(b"exit\r")?;
    finish_cancellation(&mut session)?;
    server.finish()
}

#[test]
fn a_first_request_that_never_reached_the_model_keeps_the_model_switchable() -> io::Result<()> {
    let root = Homes::new()?;
    let server = Provider::start(ProviderKind::Cancellation)?;
    let mut session = root.cancellation_session("model-switch", &server.endpoint, 1_000)?;
    session.send(b"bad-provider\r")?;
    wait_state(&mut session, "blocked")?;
    session.wait_text("Provider paused")?;
    let initial = session_page(&session.home, None)?;
    assert_eq!(initial["sessions"].as_array().map(Vec::len), Some(1));
    session.key(b"c")?;
    wait_state(&mut session, "idle")?;
    assert!(
        !session.screen().contains(" switched "),
        "cancelling changed the model"
    );
    session.command("/model switched")?;
    session.wait_text(" switched ")?;
    assert!(session.contains("bad-provider"), "the transcript was lost");
    session.send(b"switched\r")?;
    session.wait_text("switched answer")?;
    wait_state(&mut session, "done")?;
    let requests = server.requests()?;
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].body["model"], "fixture");
    assert_eq!(
        requests[1].body["model"], "switched-model",
        "the switched model was not used"
    );
    let continued = session_page(&session.home, None)?;
    assert_eq!(continued["sessions"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        continued["sessions"][0]["public_id"], initial["sessions"][0]["public_id"],
        "switching the model replaced the session identity"
    );
    session.key(b"\x14")?;
    session.wait_text("Alias and model are fixed for this session")?;
    assert_eq!(server.requests()?.len(), 2);
    session.send(b"exit\r")?;
    finish_cancellation(&mut session)?;
    server.finish()
}

#[test]
fn command_activities_follow_process_settlement_before_and_after_answers() -> io::Result<()> {
    let root = Homes::new()?;
    let server = Provider::start(ProviderKind::Activities)?;
    let home = root.command_home("activities", &server.endpoint, true, None)?;
    let mut session = Terminal::spawn(&home, &[], 100, 26, None)?;
    session.wait_text("medium · fixture-model high")?;
    session.command("Check background commands")?;
    session.until(|_| server.events.cycle_started.load(Ordering::SeqCst))?;
    session.wait_text("Toasting ▾")?;
    session.wait_text("Running while [ ! -f release-first ]")?;
    assert!(
        session
            .screen()
            .lines()
            .any(|line| line.trim() == "Ran true")
    );
    assert!(!session.screen().contains("Unknown"));
    fs::write(home.join("release-first"), "")?;
    session.wait_text("· exit 7")?;
    session.wait_text("· SIGTERM")?;
    session.wait_text("· timed out")?;
    assert!(!server.events.cycle_release.load(Ordering::SeqCst));
    assert!(!session.screen().contains("answer-2"));
    server.events.cycle_release.store(true, Ordering::SeqCst);
    session.wait_text("answer-2")?;
    wait_state(&mut session, "done")?;
    assert!(
        session.contains("Running while [ ! -f release-late ]"),
        "{}",
        session.screen()
    );
    fs::write(home.join("release-late"), "")?;
    session.wait_text("Work Done ▸")?;
    session.key(b"\t")?;
    session.key(b"\x1b[C")?;
    session.wait_text("· exit 13")?;
    session.key(b"\x1b")?;
    session.key(b"\t")?;
    session.key(b"\x1b[C")?;
    session.until(|session| {
        session
            .screen()
            .lines()
            .any(|line| line.trim() == "late-output")
    })?;
    assert!(session.screen().contains('▾'));
    assert!(!session.screen().contains("Unknown"));
    assert!(!session.screen().contains("cocoa.wait"));
    session.key(b"\x1b")?;
    close_command_session(&mut session)?;
    server.finish()
}

fn session_page(home: &Path, data_directory: Option<&Path>) -> io::Result<Value> {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_cocoa"));
    command.env_clear().env("HOME", home).current_dir(home);
    if let Some(directory) = data_directory {
        command.arg("--data-dir").arg(directory);
    }
    let output = process::CapturedProcess::spawn(command.args(["sessions", "list", "--json"]))?
        .finish(Duration::from_secs(5))?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    serde_json::from_slice(&output.stdout).map_err(io::Error::other)
}

fn capture_session_view(terminal: &mut Terminal, name: &str) -> io::Result<()> {
    if let Some(directory) = std::env::var_os("COCOA_CAPTURE_DIR") {
        let deadline = Instant::now() + Duration::from_millis(150);
        while Instant::now() < deadline {
            terminal.pump()?;
        }
        let directory = PathBuf::from(directory);
        fs::create_dir_all(&directory)?;
        fs::write(directory.join(format!("{name}.ansi")), &terminal.transcript)?;
        fs::write(directory.join(format!("{name}.txt")), terminal.screen())?;
    }
    Ok(())
}

fn close_command_session(session: &mut Terminal) -> io::Result<()> {
    if !session.exited() {
        session.command("exit")?;
    }
    session.finish(0)?;
    for secret in [
        "fixture-key",
        "fixture-private-key",
        "dialog-private-sentinel",
        "onboarding-private-sentinel",
    ] {
        assert!(
            !has_bytes(&session.transcript, secret.as_bytes()),
            "terminal leaked {secret}"
        );
    }
    reports(session)?;
    Ok(())
}

fn finish_cancellation(session: &mut Terminal) -> io::Result<()> {
    session.finish(0)?;
    assert!(
        has_bytes(&session.transcript, b"\x1b[?1049l"),
        "alternate screen not restored"
    );
    assert!(
        has_bytes(&session.transcript, b"\x1b[?25h"),
        "cursor not restored"
    );
    for secret in [
        "fixture-key",
        "fixture-private-key",
        "provider-private-sentinel",
    ] {
        assert!(
            !has_bytes(&session.transcript, secret.as_bytes()),
            "terminal leaked {secret}"
        );
    }
    reports(session)?;
    Ok(())
}

fn wait_idle(session: &mut Terminal) -> io::Result<()> {
    session.until(|session| {
        let screen = session.screen();
        !screen.contains("Stopping...")
            && !screen.lines().any(|line| {
                line.starts_with('╭')
                    && [
                        "Toasting",
                        "Blending",
                        "Refining",
                        "Winnowing",
                        "Conching",
                        "Setting",
                        "Finishing",
                        "Tempering",
                        "Stirring",
                        "Cracking",
                        "Melting",
                        "Milling",
                    ]
                    .iter()
                    .any(|text| line.contains(text))
            })
    })
}

fn assert_cancelled_idle(session: &mut Terminal) -> io::Result<()> {
    wait_idle(session)?;
    assert_report(session, "idle", None)?;
    let screen = session.screen();
    assert!(!screen.contains("Error:"), "{screen}");
    assert!(!screen.contains("Turn cancelled"), "{screen}");
    Ok(())
}

fn wait_state(session: &mut Terminal, state: &str) -> io::Result<()> {
    until_result(session, |session| {
        Ok(session.states()?.last().is_some_and(|value| value == state))
    })
}

fn until_result(
    session: &mut Terminal,
    mut predicate: impl FnMut(&Terminal) -> io::Result<bool>,
) -> io::Result<()> {
    let mut failure = None;
    session.until(|session| match predicate(session) {
        Ok(ready) => ready,
        Err(error) => {
            failure = Some(error);
            true
        }
    })?;
    match failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn assert_report(session: &Terminal, state: &str, kind: Option<&str>) -> io::Result<()> {
    let reports = reports(session)?;
    let report = reports
        .last()
        .ok_or_else(|| io::Error::other("missing terminal status"))?;
    assert_eq!(report.get("state").map(String::as_str), Some(state));
    assert_eq!(report.get("kind").map(String::as_str), kind);
    Ok(())
}

fn reports(session: &Terminal) -> io::Result<Vec<BTreeMap<String, String>>> {
    let parsed = session.reports()?;
    for record in &parsed {
        assert_eq!(record.get("app").map(String::as_str), Some("cocoa"));
        assert!(
            !record.contains_key("id"),
            "session status must address the root record"
        );
        let message = record
            .get("msg")
            .ok_or_else(|| io::Error::other("missing status message"))?;
        assert!(message.len() <= 2732);
        let message = STANDARD.decode(message).map_err(io::Error::other)?;
        assert!(message.len() <= 2048);
        let message = std::str::from_utf8(&message).map_err(io::Error::other)?;
        assert!(
            message
                .chars()
                .all(|character| character >= ' ' && !('\u{7f}'..='\u{9f}').contains(&character))
        );
        assert!(!message.contains("sentinel") && !message.contains("positional prompt"));
        let state = record
            .get("state")
            .ok_or_else(|| io::Error::other("missing status state"))?;
        if state != "blocked" {
            assert!(
                !record.contains_key("kind"),
                "blocked kind leaked into another state"
            );
        }
    }
    assert!(
        parsed.windows(2).all(|pair| pair[0] != pair[1]),
        "status heartbeat"
    );
    Ok(parsed)
}

fn saved_model(session: &Terminal, name: &str) -> bool {
    session
        .screen()
        .lines()
        .any(|line| line.contains(name) && line.contains("✓ saved"))
}

fn configuration(session: &Terminal) -> io::Result<Value> {
    read_json(&session.home.join(".config/cocoa/config.json"))
}

fn read_json(path: &Path) -> io::Result<Value> {
    serde_json::from_slice(&fs::read(path)?).map_err(io::Error::other)
}

fn has_bytes(bytes: &[u8], text: &[u8]) -> bool {
    bytes.windows(text.len()).any(|window| window == text)
}

fn read_pid(path: &Path) -> io::Result<Pid> {
    let raw = fs::read_to_string(path)?
        .trim()
        .parse()
        .map_err(io::Error::other)?;
    Pid::from_raw(raw).ok_or_else(|| io::Error::other("invalid fixture process ID"))
}

fn process_exists(identity: Pid) -> io::Result<bool> {
    match test_kill_process(identity) {
        Ok(()) => Ok(true),
        Err(rustix::io::Errno::SRCH) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

struct Homes(PathBuf);

impl Homes {
    fn new() -> io::Result<Self> {
        let root = tempfile::Builder::new()
            .prefix("cocoa-terminal-")
            .tempdir()?
            .keep();
        Ok(Self(root))
    }

    fn command_home(
        &self,
        name: &str,
        endpoint: &str,
        configured: bool,
        aliases: Option<Value>,
    ) -> io::Result<PathBuf> {
        let home = self.0.join(name);
        let mut providers = serde_json::Map::new();
        for name in ["openai", "opencode-go", "baseten", "ollama", "openrouter"] {
            providers.insert(name.to_owned(), json!({"endpoint":endpoint}));
        }
        let mut config = json!({"providers":providers});
        if configured {
            config["providers"]["openai"]["credential_env"] = json!("FIXTURE_KEY");
            config["aliases"] = aliases.unwrap_or_else(|| json!({"medium":[{"provider":"openai","model":"fixture-model","effort":"high","priority":0}]}));
            if config["aliases"].get("medium").is_some() {
                config["default_alias"] = json!("medium");
            }
        }
        write_home(&home, &config, true)?;
        Ok(home)
    }

    fn cancellation_session(
        &self,
        name: &str,
        endpoint: &str,
        timeout_ms: u64,
    ) -> io::Result<Terminal> {
        let home = self.0.join(name);
        write_home(
            &home,
            &json!({
                "providers":{"openai":{"credential_env":"COCOA_TEST_KEY","endpoint":endpoint}},
                "aliases":{
                    "medium":[{"provider":"openai","model":"fixture","priority":0}],
                    "switched":[{"provider":"openai","model":"switched-model","priority":0}]
                },
                "default_alias":"medium","timeout_ms":timeout_ms,"settlement_timeout_ms":30_000
            }),
            false,
        )?;
        let mut session = Terminal::spawn(&home, &[], 80, 24, None)?;
        session.until(|session| {
            session.home.join(".terminal-child.pid").exists()
                && has_bytes(&session.transcript, b"medium")
        })?;
        Ok(session)
    }
}

impl Drop for Homes {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0) {
            eprintln!(
                "cannot remove terminal test home {}: {error}",
                self.0.display()
            );
        }
    }
}

fn write_home(home: &Path, config: &Value, private_directory: bool) -> io::Result<()> {
    let directory = home.join(".config/cocoa");
    fs::create_dir_all(&directory)?;
    if private_directory {
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    }
    fs::write(
        directory.join("config.json"),
        serde_json::to_vec(config).map_err(io::Error::other)?,
    )
}

#[derive(Clone, Copy)]
enum ProviderKind {
    Commands,
    Persistence,
    DelegatedSwitch,
    ChildRecovery,
    Cancellation,
    Activities,
    Steering,
    Streaming,
    StartupFallback,
    StartupMcp,
}

#[derive(Clone)]
struct Request {
    body: Value,
    authorization: String,
}

#[derive(Default)]
struct Events {
    stalled: AtomicBool,
    disconnected: AtomicBool,
    cycle_started: AtomicBool,
    cycle_release: AtomicBool,
    stopped: AtomicBool,
}

#[derive(Default)]
struct ProviderState {
    requests: Vec<Request>,
    errors: Vec<String>,
}

struct Provider {
    endpoint: String,
    events: Arc<Events>,
    state: Arc<Mutex<ProviderState>>,
    thread: Option<JoinHandle<io::Result<()>>>,
}

impl Provider {
    fn start(kind: ProviderKind) -> io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let endpoint = format!("http://{}/v1/responses", listener.local_addr()?);
        listener.set_nonblocking(true)?;
        let events = Arc::new(Events::default());
        let state = Arc::new(Mutex::new(ProviderState::default()));
        let worker_events = Arc::clone(&events);
        let worker_state = Arc::clone(&state);
        let thread = thread::spawn(move || serve(listener, kind, worker_events, worker_state));
        Ok(Self {
            endpoint,
            events,
            state,
            thread: Some(thread),
        })
    }

    fn requests(&self) -> io::Result<Vec<Request>> {
        let state = self
            .state
            .lock()
            .map_err(|error| io::Error::other(error.to_string()))?;
        if !state.errors.is_empty() {
            return Err(io::Error::other(state.errors.join("\n")));
        }
        Ok(state.requests.clone())
    }

    fn reset_stall(&self) {
        self.events.stalled.store(false, Ordering::SeqCst);
        self.events.disconnected.store(false, Ordering::SeqCst);
    }

    fn finish(mut self) -> io::Result<()> {
        self.join()
    }

    fn join(&mut self) -> io::Result<()> {
        self.events.stopped.store(true, Ordering::SeqCst);
        self.events.cycle_release.store(true, Ordering::SeqCst);
        if let Some(worker) = self.thread.take() {
            worker
                .join()
                .map_err(|panic| io::Error::other(panic_message(panic)))??;
        }
        self.requests()?;
        Ok(())
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        if let Err(error) = self.join() {
            eprintln!("terminal HTTP fixture failed: {error}");
        }
    }
}

fn serve(
    listener: TcpListener,
    kind: ProviderKind,
    events: Arc<Events>,
    state: Arc<Mutex<ProviderState>>,
) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(180);
    let mut workers: Vec<JoinHandle<io::Result<()>>> = Vec::new();
    let mut connections = 0;
    let mut failure = None;
    while !events.stopped.load(Ordering::SeqCst) {
        let mut index = 0;
        while index < workers.len() {
            if workers[index].is_finished() {
                record_worker(workers.swap_remove(index), &state)?;
            } else {
                index += 1;
            }
        }
        match listener.accept() {
            Ok((stream, _)) => {
                connections += 1;
                if connections > 128 || workers.len() >= 8 {
                    failure = Some(io::Error::other("terminal HTTP fixture connection limit"));
                    break;
                }
                let events = Arc::clone(&events);
                let state = Arc::clone(&state);
                workers.push(thread::spawn(move || {
                    handle_request(stream, kind, &events, &state)
                }));
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5))
            }
            Err(error) => {
                failure = Some(error);
                break;
            }
        }
        if Instant::now() >= deadline {
            failure = Some(io::Error::new(
                io::ErrorKind::TimedOut,
                "terminal HTTP fixture deadline",
            ));
            break;
        }
    }
    for worker in workers {
        record_worker(worker, &state)?;
    }
    match failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn record_worker(
    worker: JoinHandle<io::Result<()>>,
    state: &Mutex<ProviderState>,
) -> io::Result<()> {
    let error = match worker.join() {
        Ok(Ok(())) => return Ok(()),
        Ok(Err(error)) => error.to_string(),
        Err(panic) => panic_message(panic),
    };
    state
        .lock()
        .map_err(|error| io::Error::other(error.to_string()))?
        .errors
        .push(error);
    Ok(())
}

fn panic_message(panic: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = panic.downcast_ref::<String>() {
        message.clone()
    } else if let Some(message) = panic.downcast_ref::<&str>() {
        (*message).to_owned()
    } else {
        "HTTP fixture thread panicked".to_owned()
    }
}

fn handle_request(
    mut stream: TcpStream,
    kind: ProviderKind,
    events: &Events,
    state: &Mutex<ProviderState>,
) -> io::Result<()> {
    let request = read_request(&mut stream)?;
    if matches!(kind, ProviderKind::StartupMcp) {
        let result = match request.body["method"].as_str() {
            Some("tools/list") => {
                events.cycle_started.store(true, Ordering::SeqCst);
                let deadline = Instant::now() + Duration::from_secs(45);
                while !events.cycle_release.load(Ordering::SeqCst) {
                    if Instant::now() >= deadline {
                        return Err(io::Error::other("MCP initialization was not released"));
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                json!({"tools":[]})
            }
            _ => return Err(io::Error::other("unexpected startup MCP method")),
        };
        return write_response(
            &mut stream,
            "200 OK",
            "application/json",
            &json!({"jsonrpc":"2.0","id":request.body["id"],"result":result}).to_string(),
        );
    }
    assert_eq!(request.body["stream"], true);
    for secret in [
        "fixture-key",
        "fixture-private-key",
        "onboarding-private-sentinel",
        "dialog-private-sentinel",
    ] {
        assert!(
            !request.body.to_string().contains(secret),
            "credential leaked in HTTP body"
        );
    }
    let number = {
        let mut state = state
            .lock()
            .map_err(|error| io::Error::other(error.to_string()))?;
        state.requests.push(request.clone());
        state.requests.len()
    };
    let output = match kind {
        ProviderKind::StartupMcp => return Err(io::Error::other("MCP request reached provider")),
        ProviderKind::StartupFallback => {
            if request.body["model"] == "failed-primary" {
                return stream.write_all(b"HTTP/1.1 503 Unavailable\r\nRetry-After: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            }
            if number == 5 && request.body["model"] == "fallback-tools" {
                vec![call(
                    "fallback-tools",
                    "await cocoa.wait(await cocoa.run({command:'touch fallback-tool-started; while [ ! -f release-fallback ]; do sleep 0.02; done',timeout_ms:8000}));",
                )]
            } else {
                vec![message(number, &format!("answer-{number}"))]
            }
        }
        ProviderKind::Streaming => {
            stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )?;
            for index in 0..8 {
                let line = format!("Stream line {index}: abcdefghijklmnopqrstuvwxyz012345\n");
                let mut batch = String::new();
                for character in line.chars() {
                    let event = json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"item_id":format!("message_{number}"),"delta":character.to_string()});
                    batch.push_str(&format!("data: {event}\n\n"));
                }
                stream.write_all(batch.as_bytes())?;
                stream.flush()?;
                thread::sleep(Duration::from_millis(20));
            }
            let event = json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"item_id":format!("message_{number}"),"delta":"preview-ready\n"});
            write!(stream, "data: {event}\n\n")?;
            stream.flush()?;
            let deadline = Instant::now() + Duration::from_secs(8);
            while !events.cycle_release.load(Ordering::SeqCst) {
                if Instant::now() >= deadline {
                    return Err(io::Error::other("stream settlement was not released"));
                }
                thread::sleep(Duration::from_millis(5));
            }
            let event = json!({"type":"response.completed","response":{
                "id":format!("response_{number}"),"status":"completed",
                "usage":{"input_tokens":1000,"output_tokens":100,"input_tokens_details":{"cached_tokens":880}},
                "output":[message(number, "settled-answer")]
            }});
            write!(stream, "data: {event}\n\n")?;
            return stream.flush();
        }
        ProviderKind::Steering => match number {
            1 => vec![
                call(
                    "steering-first",
                    "var steering_value = 41; await cocoa.wait(await cocoa.run({command:'touch steering-started; while [ ! -f release-steering ]; do sleep 0.02; done',timeout_ms:8000}));",
                ),
                call(
                    "steering-second",
                    "steering_value += 1; print(steering_value);",
                ),
            ],
            2 => vec![call("steering-retained", "print(steering_value);")],
            _ => vec![message(number, &format!("answer-{number}"))],
        },
        ProviderKind::Activities => {
            if number == 1 {
                vec![call(
                    "activity-commands",
                    r#"
                    var success = await cocoa.run({command:"true"});
                    await cocoa.wait(success);
                    var first = await cocoa.run({command:"while [ ! -f release-first ]; do sleep 0.02; done; printf first-output; exit 7",timeout_ms:8000});
                    var late = await cocoa.run({command:"while [ ! -f release-late ]; do sleep 0.02; done; printf late-output; exit 13",timeout_ms:8000});
                    await cocoa.run({command:"kill -TERM $$"});
                    await cocoa.run({command:"sleep 20",timeout_ms:150});
                "#,
                )]
            } else {
                if number == 2 {
                    events.cycle_started.store(true, Ordering::SeqCst);
                    let deadline = Instant::now() + Duration::from_secs(8);
                    while !events.cycle_release.load(Ordering::SeqCst) {
                        if Instant::now() >= deadline {
                            return Err(io::Error::other("activity response was not released"));
                        }
                        thread::sleep(Duration::from_millis(5));
                    }
                }
                vec![message(number, &format!("answer-{number}"))]
            }
        }
        ProviderKind::ChildRecovery => {
            let input = request.body["input"]
                .as_array()
                .ok_or_else(|| io::Error::other("missing inputs"))?;
            let child = input
                .iter()
                .any(|item| item["role"] == "user" && item["content"] == "repair child edit");
            let result = |identity: &str| {
                input.iter().find(|item| {
                    item["type"] == "function_call_output" && item["call_id"] == identity
                })
            };
            if !child {
                if result("spawn-recovery").is_some() {
                    if input.iter().any(|item| {
                        item["role"] == "user" && item["content"] == "cancel delegated work"
                    }) {
                        events.cycle_started.store(true, Ordering::SeqCst);
                        let deadline = Instant::now() + Duration::from_secs(8);
                        while !events.cycle_release.load(Ordering::SeqCst) {
                            if Instant::now() >= deadline {
                                return Err(io::Error::other("parent response was not released"));
                            }
                            thread::sleep(Duration::from_millis(5));
                        }
                    }
                    vec![message(number, "Parent finished independently")]
                } else {
                    vec![call(
                        "spawn-recovery",
                        "var worker = await cocoa.spawn({name:'licenses-cli',prompt:'repair child edit'}); print(await cocoa.wait(worker,{timeout_ms:30000}));",
                    )]
                }
            } else if let Some(effect) = result("child-effect") {
                let output = effect["output"]
                    .as_str()
                    .ok_or_else(|| io::Error::other("missing child feedback"))?;
                assert!(output.contains("match_ambiguous"), "{output}");
                if !events.cycle_release.load(Ordering::SeqCst) {
                    stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nRetry-After: 1\r\nx-request-id: req_child_recovery\r\nContent-Length: 25\r\nConnection: close\r\n\r\nprovider-private-sentinel")?;
                    return stream.flush();
                }
                if let Some(corrected) = result("child-correct") {
                    assert!(corrected["output"].as_str().is_some_and(
                        |output| output.contains("42") && output.contains("completed")
                    ));
                    vec![message(number, "Child recovered and corrected the edit")]
                } else {
                    vec![call(
                        "child-correct",
                        "print(retained + 1); original = await cocoa.edit({file:original.file,edits:[{old_text:'repeat\\nrepeat',new_text:'fixed\\nrepeat'}]});",
                    )]
                }
            } else {
                vec![call(
                    "child-effect",
                    "var retained = 41; var original = await cocoa.read({path:'existing.txt'}); await cocoa.wait(await cocoa.run({command:'printf x >> recovery-effects'})); await cocoa.edit({file:original.file,edits:[{old_text:'repeat',new_text:'fixed'}]});",
                )]
            }
        }
        ProviderKind::DelegatedSwitch => {
            if number == 1 {
                vec![call(
                    "delegation",
                    "var reviewer = await cocoa.spawn({name:'Reviewer',prompt:'hold delegated child'});",
                )]
            } else if request.body["input"][0]["content"] == "hold delegated child" {
                events.cycle_started.store(true, Ordering::SeqCst);
                let deadline = Instant::now() + Duration::from_secs(8);
                while !events.cycle_release.load(Ordering::SeqCst) {
                    if Instant::now() >= deadline {
                        return Err(io::Error::other("delegated response was not released"));
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                vec![message(number, "Child completed without cancellation")]
            } else {
                vec![message(number, "Root finished while child works")]
            }
        }
        ProviderKind::Persistence => match number {
            1 => vec![call(
                "durable-initialize",
                "var persistence_marker = 41; var effect = await cocoa.run({command:'printf x >> durable-effect'}); await cocoa.wait(effect); print(persistence_marker);",
            )],
            3 => vec![call(
                "durable-fresh-runtime",
                "print(typeof persistence_marker);",
            )],
            _ => vec![message(number, &format!("answer-{number}"))],
        },
        ProviderKind::Commands => {
            if request.body["model"] == "cycle-high"
                && !events.cycle_started.swap(true, Ordering::SeqCst)
            {
                let deadline = Instant::now() + Duration::from_secs(8);
                while !events.cycle_release.load(Ordering::SeqCst) {
                    if Instant::now() >= deadline {
                        return Err(io::Error::other("alias cycling response was not released"));
                    }
                    thread::sleep(Duration::from_millis(5));
                }
            }
            vec![message(number, &format!("answer-{number}"))]
        }
        ProviderKind::Cancellation => {
            assert_eq!(request.authorization, "Bearer fixture-key");
            match cancellation_output(&request.body, number, &mut stream, events)? {
                Some(output) => output,
                None => return Ok(()),
            }
        }
    };
    let event = json!({"type":"response.completed","response":{
        "id":format!("response_{number}"),"status":"completed","output":output
    }});
    let body = format!("event: response.completed\ndata: {event}\n\n");
    write_response(&mut stream, "200 OK", "text/event-stream", &body)
}

fn read_request(stream: &mut TcpStream) -> io::Result<Request> {
    const BODY_MAX: usize = 4 * 1024 * 1024;
    const HEADERS_MAX: usize = 16 * 1024;
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(6)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    let deadline = Instant::now() + Duration::from_secs(6);
    let mut bytes = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "HTTP request deadline",
            ));
        }
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            return Err(io::Error::other("incomplete HTTP request"));
        }
        bytes.extend_from_slice(&buffer[..count]);
        if bytes.len() > BODY_MAX + HEADERS_MAX {
            return Err(io::Error::other("HTTP request exceeds fixture bound"));
        }
        if let Some(end) = bytes.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            assert!(end <= HEADERS_MAX);
            let headers = std::str::from_utf8(&bytes[..end]).map_err(io::Error::other)?;
            assert_eq!(headers.lines().next(), Some("POST /v1/responses HTTP/1.1"));
            let header = |name: &str| {
                headers
                    .lines()
                    .filter_map(|line| line.split_once(':'))
                    .find(|(key, _)| key.eq_ignore_ascii_case(name))
                    .map(|(_, value)| value.trim())
            };
            assert_eq!(header("content-type"), Some("application/json"));
            assert!(header("transfer-encoding").is_none());
            let size: usize = header("content-length")
                .ok_or_else(|| io::Error::other("missing content length"))?
                .parse()
                .map_err(io::Error::other)?;
            assert!(size <= BODY_MAX);
            if bytes.len() >= end + 4 + size {
                assert_eq!(
                    bytes.len(),
                    end + 4 + size,
                    "unexpected bytes after HTTP request"
                );
                return Ok(Request {
                    body: serde_json::from_slice(&bytes[end + 4..]).map_err(io::Error::other)?,
                    authorization: header("authorization")
                        .ok_or_else(|| io::Error::other("missing authorization"))?
                        .to_owned(),
                });
            }
        } else if bytes.len() > HEADERS_MAX {
            return Err(io::Error::other("HTTP headers exceed fixture bound"));
        }
    }
}

fn write_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &str,
) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()
}

fn message(number: usize, text: &str) -> Value {
    json!({"id":format!("message_{number}"),"status":"completed","type":"message","role":"assistant",
        "content":[{"type":"output_text","text":text,"annotations":[]}]})
}

fn call(identity: &str, code: &str) -> Value {
    json!({"type":"function_call","id":format!("fc_{identity}"),"call_id":identity,"name":"exec",
        "arguments":json!({"code":code}).to_string()})
}

fn cancellation_output(
    body: &Value,
    number: usize,
    stream: &mut TcpStream,
    events: &Events,
) -> io::Result<Option<Vec<Value>>> {
    let records = body["input"]
        .as_array()
        .ok_or_else(|| io::Error::other("missing input history"))?;
    let prompt = records
        .iter()
        .rev()
        .find(|item| item["role"] == "user")
        .and_then(|item| item["content"].as_str())
        .ok_or_else(|| io::Error::other("missing user prompt"))?;
    let mut outputs = BTreeMap::new();
    let mut selected = 0;
    for item in records {
        if item["role"] == "user" {
            selected = 0;
        } else if item["type"] == "function_call" {
            selected += 1;
        } else if item["type"] == "function_call_output" {
            let (outcome, progress) = item["output"]
                .as_str()
                .and_then(|value| value.split_once('\n'))
                .ok_or_else(|| io::Error::other("missing execution progress"))?;
            assert_eq!(
                progress,
                format!("Selected exec calls for this input: {selected}.")
            );
            let identity = item["call_id"]
                .as_str()
                .ok_or_else(|| io::Error::other("missing call ID"))?;
            outputs.insert(
                identity.to_owned(),
                serde_json::from_str::<Value>(outcome).map_err(io::Error::other)?,
            );
        }
    }
    let preserved = || {
        assert!(
            records
                .iter()
                .any(|item| item.to_string().contains("bindings were preserved"))
        )
    };
    let output = if prompt.starts_with("stall-") {
        if prompt == "stall-body" {
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n"
            )?;
            stream.flush()?;
        }
        events.stalled.store(true, Ordering::SeqCst);
        let mut byte = [0];
        match stream.read(&mut byte) {
            Ok(0) => {}
            Err(error) if error.kind() == io::ErrorKind::ConnectionReset => {}
            result => {
                return Err(io::Error::other(format!(
                    "cancelled HTTP connection still open: {result:?}"
                )));
            }
        }
        events.disconnected.store(true, Ordering::SeqCst);
        return Ok(None);
    } else if prompt == "seed" {
        if let Some(output) = outputs.get("seed") {
            assert_eq!(*output, json!({"status":"completed","printed":[41]}));
            vec![message(number, "Seed ready")]
        } else {
            vec![call(
                "seed",
                "var retained = 41; var destination = await cocoa.read({path:'existing.txt'}); await cocoa.write({file:destination.file,content:'retained effect'}); print(retained)",
            )]
        }
    } else if prompt.starts_with("after-http-") {
        let reset = prompt == "after-http-failure-with-tools";
        if reset {
            assert!(
                records
                    .iter()
                    .any(|item| item.to_string().contains("runtime was retired and reset"))
            );
        } else {
            preserved();
        }
        let identity = format!("retained-{prompt}");
        if let Some(output) = outputs.get(&identity) {
            let printed = if reset {
                json!(["undefined"])
            } else {
                json!([42])
            };
            assert_eq!(*output, json!({"status":"completed","printed":printed}));
            vec![message(number, &format!("{prompt} continuation verified"))]
        } else {
            let code = if reset {
                "print(typeof retained)"
            } else {
                "print(retained + 1)"
            };
            vec![call(&identity, code)]
        }
    } else if prompt == "busy" {
        vec![
            call(
                "busy",
                "var polluted = 99; print('before busy'); await cocoa.run({command:'touch busy-started'}); while (true) {}",
            ),
            call(
                "unstarted",
                "await cocoa.run({command:'touch must-not-run'})",
            ),
        ]
    } else if prompt == "resume-draft" {
        assert_eq!(
            outputs.get("busy"),
            Some(&json!({"status":"interrupted","printed":["before busy"]}))
        );
        assert_eq!(
            outputs.get("unstarted"),
            Some(&json!({"status":"notstarted"}))
        );
        assert!(
            records
                .iter()
                .any(|item| item.to_string().contains("runtime was retired and reset"))
        );
        if let Some(output) = outputs.get("fresh") {
            assert_eq!(
                *output,
                json!({"status":"completed","printed":[["undefined","undefined"]]})
            );
            vec![message(number, "JavaScript reset verified")]
        } else {
            vec![call("fresh", "print([typeof retained, typeof polluted])")]
        }
    } else if prompt == "shell" {
        vec![call(
            "shell",
            "var shellHandle = await cocoa.run({command:`trap '' TERM; sleep 30 & echo $! > descendant.pid; echo $$ > shell.pid; printf started > shell-started; wait; touch late-effect`}); await cocoa.wait(shellHandle)",
        )]
    } else if prompt == "after-shell" {
        assert_eq!(
            outputs.get("shell").and_then(|value| value.get("status")),
            Some(&json!("interrupted"))
        );
        if let Some(output) = outputs.get("shell-check") {
            assert_eq!(
                *output,
                json!({"status":"completed","printed":[["undefined","exited",23]]})
            );
            vec![message(number, "Shell continuation verified")]
        } else {
            vec![call(
                "shell-check",
                "var cleanup = await cocoa.run({command: 'kill -KILL \"$(cat shell.pid)\" \"$(cat descendant.pid)\"'}); var stoppedShell = await cocoa.wait(cleanup); print([typeof shellHandle, stoppedShell.status.kind, 7 * 3 + 2])",
            )]
        }
    } else if prompt == "fatal" {
        vec![json!({"type":"unsupported"})]
    } else if prompt == "bad-provider" {
        write_response(
            stream,
            "401 Unauthorized",
            "text/plain",
            "provider-private-sentinel",
        )?;
        return Ok(None);
    } else if prompt == "switched" {
        vec![message(number, "switched answer")]
    } else if prompt == "failure-with-tools" {
        if outputs.contains_key("failure-tools") {
            vec![json!({"type":"unsupported"})]
        } else {
            vec![call(
                "failure-tools",
                "var retryEffect = await cocoa.run({command:'printf x >> retry-effects'}); await cocoa.wait(retryEffect); print(retained)",
            )]
        }
    } else if prompt == "retry-success" {
        if events.cycle_release.load(Ordering::SeqCst) {
            assert!(
                records
                    .iter()
                    .any(|item| item["role"] == "user" && item["content"] == "seed")
            );
            vec![message(number, "Manual retry succeeded")]
        } else {
            stream.write_all(
                b"HTTP/1.1 503 Service Unavailable\r\nRetry-After: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )?;
            stream.flush()?;
            return Ok(None);
        }
    } else {
        return Err(io::Error::other(format!("unexpected prompt: {prompt}")));
    };
    Ok(Some(output))
}
