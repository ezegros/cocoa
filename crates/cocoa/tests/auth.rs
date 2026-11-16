//! Credential CLI contracts, including hidden terminal input and concurrent writers.

#[path = "support/process.rs"]
mod process;

use process::CommandExt;
use std::{
    error::Error,
    io::{Read, Write},
    net::TcpListener,
    path::Path,
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

use serde_json::{Value, json};

#[cfg(unix)]
#[path = "support/terminal.rs"]
mod terminal;

#[cfg(unix)]
#[test]
fn login_and_logout_use_cocoa_roots_without_a_temporary_directory() -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::PermissionsExt;

    let home = tempfile::tempdir()?;
    let unrelated_directory = home.path().join(".config/unrelated-app");
    let unrelated_oauth_directory = unrelated_directory.join("oauth");
    std::fs::create_dir_all(&unrelated_oauth_directory)?;
    let unrelated_auth = unrelated_directory.join("auth.json");
    std::fs::write(&unrelated_auth, b"unrelated-private-key")?;
    let unrelated_oauth = unrelated_oauth_directory.join("chatgpt.json");
    std::fs::write(&unrelated_oauth, b"unrelated-private-oauth")?;

    let directory = home.path().join(".config/cocoa");
    assert!(
        auth(home.path(), &["logout", "openai"], b"")?
            .status
            .success()
    );
    assert!(!directory.exists());
    assert!(
        auth(home.path(), &["login", "openai"], b"cocoa-private-key")?
            .status
            .success()
    );
    let saved: Value = serde_json::from_slice(&std::fs::read(directory.join("auth.json"))?)?;
    assert_eq!(
        saved,
        json!({"openai":{"type":"api-key","api_key":"cocoa-private-key"}})
    );

    let output = Command::new(env!("CARGO_BIN_EXE_cocoa"))
        .env_clear()
        .env("HOME", home.path())
        .env("TMPDIR", home.path().join("missing"))
        .args(["logout", "openai"])
        .stdin(Stdio::null())
        .output_bounded()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let saved: Value = serde_json::from_slice(&std::fs::read(directory.join("auth.json"))?)?;
    assert_eq!(saved, json!({}));

    let oauth_directory = directory.join("oauth");
    std::fs::create_dir(&oauth_directory)?;
    std::fs::set_permissions(&oauth_directory, std::fs::Permissions::from_mode(0o700))?;
    let oauth_path = oauth_directory.join("chatgpt.json");
    std::fs::write(
        &oauth_path,
        serde_json::to_vec(&json!({
            "issuer":"https://auth.openai.com",
            "resource":"https://api.openai.com/v1",
            "host_id":"urn:uuid:00000000-0000-4000-8000-000000000000",
            "active":"cocoa-account",
            "registrations":{"cocoa-account":{"generation":"fixture-generation","state":{"status":"pending"}}}
        }))?,
    )?;
    std::fs::set_permissions(&oauth_path, std::fs::Permissions::from_mode(0o600))?;
    let output = auth(home.path(), &["logout", "chatgpt"], b"")?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let saved: Value = serde_json::from_slice(&std::fs::read(&oauth_path)?)?;
    assert!(saved["active"].is_null());
    assert_eq!(
        saved["registrations"]["cocoa-account"]["state"]["status"],
        "pending"
    );
    assert_eq!(std::fs::read(&unrelated_auth)?, b"unrelated-private-key");
    assert_eq!(std::fs::read(&unrelated_oauth)?, b"unrelated-private-oauth");
    assert!(!directory.join("config.json").exists());
    Ok(())
}

#[cfg(unix)]
#[test]
fn command_credentials_are_saved_without_execution_and_cached_across_model_selections()
-> Result<(), Box<dyn Error>> {
    let home = tempfile::tempdir()?;
    let listener = credential_provider(home.path())?;
    let marker = home.path().join("executions");
    let source = home.path().join("credential.sh");
    std::fs::write(
        &source,
        "printf ' command-private-'; sleep 0.05; printf 'key \\n'; printf stderr-private-sentinel >&2",
    )?;
    let mut command = format!(
        "!printf x >> '{}'; sleep 30 >/dev/null 2>&1 & . '{}' # clé ",
        marker.display(),
        source.display()
    );
    command.push_str(&"x".repeat(8192 - command.len()));
    for provider in ["openai", "openrouter"] {
        let output = auth(home.path(), &["login", provider], command.as_bytes())?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.is_empty());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("credential.sh"));
        assert!(!marker.exists(), "saving must not execute the command");
    }
    let saved: Value =
        serde_json::from_slice(&std::fs::read(home.path().join(".config/cocoa/auth.json"))?)?;
    assert_eq!(saved["openai"]["api_key"], command);
    assert_eq!(saved["openrouter"]["api_key"], command);

    let missing = home.path().join("missing");
    let mut session = terminal::Terminal::spawn_with_environment(
        home.path(),
        &[],
        100,
        28,
        None,
        &[("TMPDIR", missing.as_os_str())],
    )?;
    session.wait_text("idle-model")?;
    session.command("/model openai/first-model")?;
    session.until(|session| session.contains("first-model") && !session.contains("/model "))?;
    assert_eq!(std::fs::read(&marker)?, b"x");
    std::fs::write(&source, "printf changed-private-key")?;
    session.command("/model openrouter/cached-model")?;
    session.until(|session| session.contains("cached-model") && !session.contains("/model "))?;
    assert_eq!(std::fs::read(&marker)?, b"x");
    // Starting agent work needs a temporary directory for local tool output, unlike key resolution.
    std::fs::create_dir(&missing)?;
    for answer in ["first-accepted", "second-accepted"] {
        session.command("Hello provider")?;
        let request = respond(&listener, answer)?;
        assert_eq!(request.body["model"], "cached-model");
        assert!(
            request
                .headers
                .lines()
                .any(|line| line.eq_ignore_ascii_case("authorization: Bearer command-private-key"))
        );
        for line in request
            .headers
            .lines()
            .filter(|line| !line.to_ascii_lowercase().starts_with("authorization:"))
        {
            assert!(!line.contains("private"));
        }
        for forbidden in [
            "command-private-key",
            "changed-private-key",
            "stderr-private-sentinel",
            "credential.sh",
        ] {
            assert!(!request.body.to_string().contains(forbidden));
        }
        session.wait_text(answer)?;
        session.until(|session| {
            session
                .states()
                .is_ok_and(|states| states.last().is_some_and(|state| state == "done"))
        })?;
        assert_eq!(std::fs::read(&marker)?, b"x");
    }
    session.command("exit")?;
    session.finish(0)?;
    let output = Command::new(env!("CARGO_BIN_EXE_cocoa"))
        .env_clear()
        .env("HOME", home.path())
        .current_dir(home.path())
        .args(["sessions", "list", "--json"])
        .stdin(Stdio::null())
        .output_bounded()?;
    assert!(output.status.success());
    serde_json::from_slice::<Value>(&output.stdout)?;
    for forbidden in [
        "command-private-key",
        "changed-private-key",
        "stderr-private-sentinel",
        "credential.sh",
    ] {
        assert!(!String::from_utf8_lossy(&session.transcript).contains(forbidden));
        assert!(!String::from_utf8_lossy(&output.stdout).contains(forbidden));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(forbidden));
    }
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn rejected_command_credentials_are_cached_without_network_disclosure_and_clean_up_children()
-> Result<(), Box<dyn Error>> {
    use rustix::process::{Pid, test_kill_process};

    let home = tempfile::tempdir()?;
    let listener = credential_provider(home.path())?;
    for command in [
        "!".to_owned(),
        "! \t".to_owned(),
        "!printf bad\0key".to_owned(),
        format!("!{}", "x".repeat(8192)),
    ] {
        let output = auth(home.path(), &["login", "openai"], command.as_bytes())?;
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("API key must contain"));
        assert!(!home.path().join(".config/cocoa/auth.json").exists());
    }
    let mut session = terminal::Terminal::spawn(home.path(), &[], 100, 28, None)?;
    session.wait_text("idle-model")?;
    for (name, script) in [
        ("empty", "exit 0"),
        ("whitespace", "printf ' \\n\\t'"),
        ("embedded-space", "printf 'private sentinel'"),
        ("invalid-utf8", "printf '\\377'"),
        ("nul", "printf 'nul-private\\000sentinel'"),
        (
            "nonzero",
            "printf private-sentinel; printf stderr-private-sentinel >&2; exit 7",
        ),
        ("oversized", "head -c 8193 /dev/zero | tr '\\000' x"),
        (
            "oversized-whitespace",
            "head -c 8193 /dev/zero | tr '\\000' ' '",
        ),
        ("timeout", "printf partial-private-key; wait"),
    ] {
        let marker = home.path().join(format!("executions-{name}"));
        let identity = home.path().join(format!("child-{name}"));
        let source = home.path().join(format!("{name}.sh"));
        std::fs::write(&source, script)?;
        let command = format!(
            "!printf x >> '{}'; sleep 30 >/dev/null 2>&1 & printf '%s' $! > '{}'; . '{}'",
            marker.display(),
            identity.display(),
            source.display(),
        );
        assert!(
            auth(home.path(), &["login", "openai"], command.as_bytes())?
                .status
                .success()
        );
        assert!(!marker.exists());
        let started = Instant::now();
        session.command("/model openai/rejected-model")?;
        session.wait_text("API key command failed or exceeded its limits")?;
        if name == "timeout" {
            assert!(started.elapsed() >= Duration::from_secs(5));
        }
        assert!(started.elapsed() < Duration::from_secs(8), "{name}");
        assert_eq!(std::fs::read(&marker)?, b"x", "{name}");
        let identity = std::fs::read_to_string(&identity)?.parse::<i32>()?;
        let child = Pid::from_raw(identity).ok_or("invalid child PID")?;
        let deadline = Instant::now() + Duration::from_secs(2);
        while test_kill_process(child).is_ok() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            test_kill_process(child),
            Err(rustix::io::Errno::SRCH),
            "{name}"
        );
        std::fs::write(&source, "printf now-valid-private-key")?;
        session.command("/model ollama/idle-model")?;
        session.until(|session| {
            !session.contains("API key command failed") && !session.contains("/model ")
        })?;
        session.command("/model openai/rejected-model")?;
        session.wait_text("API key command failed or exceeded its limits")?;
        assert_eq!(
            std::fs::read(&marker)?,
            b"x",
            "failure was not cached: {name}"
        );
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
            "{name}: unexpected network request"
        );
        session.command("/model ollama/idle-model")?;
        session.until(|session| {
            !session.contains("API key command failed") && !session.contains("/model ")
        })?;
    }
    assert!(
        auth(
            home.path(),
            &["login", "openai"],
            b"!head -c 8192 /dev/zero | tr '\\000' x"
        )?
        .status
        .success()
    );
    session.command("/model openai/boundary-model")?;
    session.until(|session| session.contains("boundary-model") && !session.contains("/model "))?;
    session.command("Check boundary credential")?;
    let request = respond(&listener, "boundary-accepted")?;
    assert!(
        request
            .headers
            .lines()
            .any(|line| line == format!("authorization: Bearer {}", "x".repeat(8192)))
    );
    assert!(!request.body.to_string().contains(&"x".repeat(64)));
    session.wait_text("boundary-accepted")?;
    session.command("exit")?;
    session.finish(0)?;
    for forbidden in [
        "private-sentinel",
        "private sentinel",
        "nul-private",
        "partial-private-key",
        "now-valid-private-key",
        "executions-",
        "child-",
    ] {
        assert!(!String::from_utf8_lossy(&session.transcript).contains(forbidden));
        assert!(!request.body.to_string().contains(forbidden));
    }
    assert!(!String::from_utf8_lossy(&session.transcript).contains(&"x".repeat(64)));
    Ok(())
}

#[test]
fn local_ollama_rejects_login_before_reading_a_key() -> Result<(), Box<dyn Error>> {
    let home = tempfile::tempdir()?;
    let output = auth(home.path(), &["login", "ollama"], b"")?;
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("local Ollama does not accept credentials")
    );
    assert!(
        auth(home.path(), &["logout", "ollama"], b"")?
            .status
            .success()
    );
    assert!(!home.path().join(".config/cocoa").exists());
    Ok(())
}

#[cfg(unix)]
#[test]
fn hidden_input_never_echoes_and_restores_terminal_on_every_exit() -> Result<(), Box<dyn Error>> {
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        thread,
        time::{Duration, Instant},
    };

    let home = tempfile::tempdir()?;
    for (scenario, input, expected) in [
        ("success", b"terminal-private-key\r".to_vec(), 0),
        ("command", b"!printf terminal-private-key\r".to_vec(), 0),
        ("cancel", b"terminal-private-key\x03".to_vec(), 1),
        ("signal", b"terminal-private-key".to_vec(), 1),
        (
            "malformed",
            [
                b"bad ".as_slice(),
                &vec![b'x'; 256],
                b"terminal-private-key\r",
            ]
            .concat(),
            1,
        ),
        ("oversized", [vec![b'x'; 8704], b"\r".to_vec()].concat(), 1),
        ("storage_failure", b"terminal-private-key\r".to_vec(), 1),
        ("write_failure", b"terminal-private-key\r".to_vec(), 1),
    ] {
        let path = home.path().join(scenario);
        let directory = path.join(".config/cocoa");
        fs::create_dir_all(&directory)?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        fs::write(directory.join("config.json"), b"invalid-generation-config")?;
        let original = br#"{"openrouter":{"type":"api-key","api_key":"keep-original"}}"#;
        let auth_path = directory.join("auth.json");
        if matches!(scenario, "storage_failure" | "write_failure") {
            fs::write(&auth_path, original)?;
            let mode = if scenario == "storage_failure" {
                0o644
            } else {
                0o600
            };
            fs::set_permissions(&auth_path, fs::Permissions::from_mode(mode))?;
        }
        let file_size_limit = (scenario == "write_failure").then_some(32);
        let mut terminal =
            terminal::Terminal::spawn(&path, &["login", "openai"], 80, 24, file_size_limit)?;
        terminal.until(|terminal| {
            String::from_utf8_lossy(&terminal.transcript).contains("API key (hidden): ")
        })?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while terminal.echo_enabled()? {
            assert!(
                Instant::now() < deadline,
                "{scenario}: hidden input left echo enabled"
            );
            terminal.pump()?;
        }
        for chunk in input.chunks(256) {
            terminal.send(chunk)?;
            thread::sleep(Duration::from_millis(2));
        }
        if scenario == "signal" {
            terminal.interrupt_group()?;
        }
        terminal.finish(expected)?;
        assert!(
            terminal.transcript.len() <= 65_536,
            "{scenario}: hidden-input output limit"
        );
        let transcript = String::from_utf8_lossy(&terminal.transcript);
        assert!(
            !transcript.contains("terminal-private-key"),
            "{scenario}: key echoed"
        );
        assert!(
            !transcript.contains("xxxx"),
            "{scenario}: oversized key echoed"
        );
        if matches!(scenario, "cancel" | "signal") {
            assert!(transcript.contains("cancelled"), "{scenario}: {transcript}");
        }
        match scenario {
            "success" => {
                let saved: Value = serde_json::from_slice(&fs::read(&auth_path)?)?;
                assert_eq!(
                    saved,
                    json!({"openai":{"type":"api-key","api_key":"terminal-private-key"}})
                );
            }
            "command" => {
                let saved: Value = serde_json::from_slice(&fs::read(&auth_path)?)?;
                assert_eq!(
                    saved,
                    json!({"openai":{"type":"api-key","api_key":"!printf terminal-private-key"}})
                );
            }
            "storage_failure" | "write_failure" => {
                assert_eq!(
                    fs::read(&auth_path)?,
                    original,
                    "{scenario}: original credentials changed"
                );
                if scenario == "write_failure" {
                    assert!(transcript.contains("original file was not replaced"));
                    let mut files = fs::read_dir(&directory)?
                        .map(|entry| entry.map(|entry| entry.file_name()))
                        .collect::<Result<Vec<_>, _>>()?;
                    files.sort();
                    assert_eq!(
                        files,
                        ["auth.json", "auth.json.lock", "config.json"],
                        "temporary credentials remained after a failed write"
                    );
                }
            }
            _ => assert!(
                !auth_path.exists(),
                "{scenario}: credentials unexpectedly saved"
            ),
        }
    }
    Ok(())
}

#[test]
fn login_and_logout_preserve_other_saved_providers() -> Result<(), Box<dyn Error>> {
    let home = tempfile::tempdir()?;
    let output = auth(home.path(), &["login", "openai"], b"first-private-key\n")?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("first-private-key"));
    assert!(
        auth(
            home.path(),
            &["login", "openrouter"],
            b"second-private-key\r\n"
        )?
        .status
        .success()
    );
    let path = home.path().join(".config/cocoa/auth.json");
    let saved: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    assert_eq!(
        saved,
        json!({"openai":{"type":"api-key","api_key":"first-private-key"},"openrouter":{"type":"api-key","api_key":"second-private-key"}})
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path)?.permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(path.parent().ok_or("no parent")?)?
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
    for _ in 0..2 {
        assert!(
            auth(home.path(), &["logout", "openai"], b"")?
                .status
                .success()
        );
    }
    let saved: Value = serde_json::from_slice(&std::fs::read(path)?)?;
    assert_eq!(
        saved,
        json!({"openrouter":{"type":"api-key","api_key":"second-private-key"}})
    );
    assert!(!home.path().join(".config/cocoa/config.json").exists());
    Ok(())
}

#[test]
fn key_input_is_bounded_redacted_and_never_accepted_as_an_argument() -> Result<(), Box<dyn Error>> {
    let home = tempfile::tempdir()?;
    for input in [
        b"".to_vec(),
        b"private sentinel".to_vec(),
        b"private\nsentinel\n".to_vec(),
        vec![b'x'; 8193],
        vec![0xff],
        b"private\x00sentinel".to_vec(),
    ] {
        let output = auth(home.path(), &["login", "openai"], &input)?;
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("API key must contain"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("private"));
        assert!(!home.path().join(".config/cocoa/auth.json").exists());
    }
    for arguments in [
        vec!["login", "openai", "private-sentinel"],
        vec!["login", "private-sentinel"],
        vec!["login", "openai"],
    ] {
        let output = auth(home.path(), &arguments, b"")?;
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("private-sentinel"));
    }
    let mut boundary = vec![b'x'; 8192];
    boundary.extend_from_slice(b"\r\n");
    assert!(
        auth(home.path(), &["login", "openai"], &boundary)?
            .status
            .success()
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn invalid_stores_are_not_overwritten_or_disclosed() -> Result<(), Box<dyn Error>> {
    let home = tempfile::tempdir()?;
    assert!(
        auth(home.path(), &["login", "openai"], b"original-key")?
            .status
            .success()
    );
    let path = home.path().join(".config/cocoa/auth.json");
    for invalid in [
        "private-sentinel".into(),
        r#"{"openai":{"type":"api-key","api_key":"private-sentinel"},"openai":{"type":"api-key","api_key":"second"}}"#.into(),
        r#"{"openai":{"type":"api-key","api_key":"private-sentinel","api_key":"second"}}"#.into(),
        r#"{"openai":{"api_key":"private-sentinel"}}"#.into(),
        r#"{"openai":{"type":"api_key","key":"private-sentinel"}}"#.into(),
        r#"{"openai":{"type":"oauth","api_key":"private-sentinel"}}"#.into(),
        r#"{"openai":{"type":null,"api_key":"private-sentinel"}}"#.into(),
        r#"{"openai":{"type":"api-key","type":"api-key","api_key":"private-sentinel"}}"#.into(),
        r#"{"unknown":{"type":"api-key","api_key":"private-sentinel"}}"#.into(),
        r#"{"openai":{"type":"api-key","api_key":"private sentinel"}}"#.into(),
        " ".repeat(65537),
    ] {
        std::fs::write(&path, &invalid)?;
        for (arguments, key) in [
            (
                vec!["login", "openrouter"],
                b"replacement".as_slice(),
            ),
            (vec!["logout", "openai"], b"".as_slice()),
        ] {
            let output = auth(home.path(), &arguments, key)?;
            assert!(!output.status.success());
            assert!(!String::from_utf8_lossy(&output.stderr).contains("private-sentinel"));
            assert_eq!(std::fs::read_to_string(&path)?, invalid);
        }
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn unsafe_files_and_directories_are_not_repaired_or_replaced() -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let home = tempfile::tempdir()?;
    assert!(
        auth(home.path(), &["login", "openai"], b"original-key")?
            .status
            .success()
    );
    let directory = home.path().join(".config/cocoa");
    let path = directory.join("auth.json");
    let original = std::fs::read(&path)?;
    for mode in [0o644, 0o400, 0o000] {
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))?;
        let output = auth(home.path(), &["login", "openrouter"], b"replacement")?;
        assert!(!output.status.success());
        assert_eq!(std::fs::metadata(&path)?.permissions().mode() & 0o777, mode);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        assert_eq!(std::fs::read(&path)?, original);
    }
    std::fs::rename(&path, directory.join("original"))?;
    symlink(directory.join("original"), &path)?;
    assert!(
        !auth(home.path(), &["logout", "openai"], b"")?
            .status
            .success()
    );
    std::fs::remove_file(&path)?;
    symlink(directory.join("missing"), &path)?;
    assert!(
        !auth(home.path(), &["logout", "openai"], b"")?
            .status
            .success()
    );
    std::fs::remove_file(&path)?;
    std::fs::hard_link(directory.join("original"), &path)?;
    assert!(
        !auth(home.path(), &["logout", "openai"], b"")?
            .status
            .success()
    );
    std::fs::remove_file(&path)?;
    std::fs::rename(directory.join("original"), &path)?;
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o755))?;
    assert!(
        !auth(home.path(), &["logout", "openai"], b"")?
            .status
            .success()
    );
    assert_eq!(
        std::fs::metadata(&directory)?.permissions().mode() & 0o777,
        0o755
    );
    assert_eq!(std::fs::read(&path)?, original);
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
    std::fs::remove_file(&path)?;
    std::fs::create_dir(&path)?;
    let output = auth(home.path(), &["logout", "openai"], b"")?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("regular files"));
    Ok(())
}

#[cfg(unix)]
#[test]
fn locked_store_times_out_then_merges_concurrent_updates() -> Result<(), Box<dyn Error>> {
    use std::{
        fs::File,
        thread,
        time::{Duration, Instant},
    };
    let home = tempfile::tempdir()?;
    assert!(
        auth(home.path(), &["login", "openai"], b"original-key")?
            .status
            .success()
    );
    let path = home.path().join(".config/cocoa/auth.json");
    let original = std::fs::read(&path)?;
    let lock = File::open(home.path().join(".config/cocoa/auth.json.lock"))?;
    lock.try_lock()?;
    let start = Instant::now();
    let output = auth(home.path(), &["login", "baseten"], b"replacement")?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("busy"));
    assert!(start.elapsed() < Duration::from_secs(4));
    assert_eq!(std::fs::read(&path)?, original);
    lock.unlock()?;
    let writers: Vec<_> = ["openrouter", "baseten", "opencode-go", "openrouter"]
        .into_iter()
        .map(|provider| {
            let home = home.path().to_path_buf();
            thread::spawn(move || -> std::io::Result<bool> {
                auth(&home, &["login", provider], b"parallel-key")
                    .map(|output| output.status.success())
            })
        })
        .collect();
    for writer in writers {
        assert!(writer.join().map_err(|_| "writer panicked")??);
    }
    let saved: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    assert_eq!(
        saved,
        json!({
            "openai":{"type":"api-key","api_key":"original-key"},
            "openrouter":{"type":"api-key","api_key":"parallel-key"},
            "baseten":{"type":"api-key","api_key":"parallel-key"},
            "opencode-go":{"type":"api-key","api_key":"parallel-key"}
        })
    );
    assert_eq!(
        std::fs::read_dir(path.parent().ok_or("missing parent")?)?
            .collect::<Result<Vec<_>, _>>()?
            .len(),
        2
    );
    Ok(())
}

#[cfg(unix)]
fn credential_provider(home: &Path) -> Result<TcpListener, Box<dyn Error>> {
    use std::os::unix::fs::PermissionsExt;

    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let endpoint = format!("http://{}/v1/responses", listener.local_addr()?);
    let directory = home.join(".config/cocoa");
    std::fs::create_dir_all(&directory)?;
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
    std::fs::write(
        directory.join("config.json"),
        serde_json::to_vec(&json!({
            "providers": {
                "openai":{"endpoint":endpoint},
                "openrouter":{"endpoint":endpoint},
                "ollama":{"endpoint":endpoint}
            },
            "aliases":{"idle-model":[{"provider":"ollama","model":"unused"}]},
            "default_alias":"idle-model",
            "web_search":{"provider":"disabled"}
        }))?,
    )?;
    Ok(listener)
}

#[cfg(unix)]
struct ProviderRequest {
    headers: String,
    body: Value,
}

#[cfg(unix)]
fn respond(listener: &TcpListener, answer: &str) -> Result<ProviderRequest, Box<dyn Error>> {
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error.into()),
        }
    };
    stream.set_nonblocking(true)?;
    let mut bytes = Vec::new();
    let mut buffer = [0; 8192];
    let request = loop {
        if Instant::now() >= deadline {
            return Err("provider request timed out".into());
        }
        match stream.read(&mut buffer) {
            Ok(0) => return Err("provider request closed early".into()),
            Ok(length) => bytes.extend_from_slice(&buffer[..length]),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(error) => return Err(error.into()),
        }
        if bytes.len() > 128 * 1024 {
            return Err("provider request exceeded 128 KiB".into());
        }
        if let Some(end) = bytes.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&bytes[..end])?;
            let length: usize = headers
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .ok_or("provider request has no content length")?
                .1
                .trim()
                .parse()?;
            if length > 128 * 1024 - end - 4 {
                return Err("provider body exceeded capture limit".into());
            }
            if bytes.len() == end + 4 + length {
                break ProviderRequest {
                    headers: headers.to_owned(),
                    body: serde_json::from_slice(&bytes[end + 4..])?,
                };
            }
        }
    };
    let event = json!({"type":"response.completed","response":{
        "id":format!("response-{answer}"),"status":"completed","output":[{
            "id":format!("message-{answer}"),"type":"message","role":"assistant","status":"completed",
            "content":[{"type":"output_text","text":answer,"annotations":[]}]
        }]
    }});
    let body = format!("data: {event}\n\n");
    stream.set_nonblocking(false)?;
    stream.set_write_timeout(Some(Duration::from_secs(1)))?;
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    Ok(request)
}

fn auth(home: &Path, arguments: &[&str], input: &[u8]) -> std::io::Result<Output> {
    let child = Command::new(env!("CARGO_BIN_EXE_cocoa"))
        .env_clear()
        .env("HOME", home)
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut process = process::CapturedProcess::try_from(child)?;
    process
        .child
        .stdin
        .take()
        .ok_or_else(|| std::io::Error::other("no stdin"))?
        .write_all(input)?;
    process.finish(process::EXIT_TIMEOUT)
}
