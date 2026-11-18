//! Public CLI and persistent terminal command contracts.

#[path = "support/process.rs"]
mod process;

use process::CommandExt;
use serde_json::{Value, json};
use std::{
    error::Error,
    path::Path,
    process::{Command, Stdio},
};

use cocoa::persistence::SessionDatabase;
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};

#[test]
fn update_is_noninteractive_and_independent_of_provider_configuration() -> Result<(), Box<dyn Error>>
{
    let home = tempfile::tempdir()?;
    std::fs::create_dir_all(home.path().join(".config/cocoa"))?;
    std::fs::write(
        home.path().join(".config/cocoa/config.json"),
        "invalid JSON",
    )?;
    for arguments in [
        vec!["update"],
        vec!["update", "--check"],
        vec!["update", "--version", "v0.2.3-preview.1"],
        vec!["update", "--unpin"],
        vec!["update", "--channel", "stable"],
        vec!["update", "--channel", "preview"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_cocoa"))
            .env_clear()
            .env("HOME", home.path())
            .env("COCOA_AUTO_UPDATE", "0")
            .args(arguments)
            .stdin(Stdio::null())
            .output_bounded()?;
        assert!(!output.status.success());
        let diagnostic = String::from_utf8(output.stderr)?;
        assert!(diagnostic.contains("not installer-managed"), "{diagnostic}");
        assert!(output.stdout.is_empty());
    }
    for arguments in [
        vec!["update", "--check", "--unpin"],
        vec!["update", "--check", "--version", "0.2.3"],
        vec!["update", "--version", "0.2.3", "--unpin"],
        vec!["update", "--channel", "preview", "--version", "0.2.3"],
        vec!["update", "--channel", "stable", "--unpin"],
        vec!["update", "--channel", "preview", "--check"],
        vec!["update", "--channel", "nightly"],
        vec!["update", "--version", "latest"],
        vec!["update", "--version", "../0.2.3"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_cocoa"))
            .env_clear()
            .args(arguments)
            .stdin(Stdio::null())
            .output_bounded()?;
        assert!(!output.status.success());
        assert!(String::from_utf8(output.stderr)?.contains("invalid arguments"));
    }
    Ok(())
}

#[test]
fn theme_flags_accept_only_the_four_builtin_palettes() -> Result<(), Box<dyn Error>> {
    for theme in [
        "dark-chocolate",
        "cacao-grove",
        "milk-chocolate",
        "terminal-native",
        "unknown",
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_cocoa"))
            .env_clear()
            .args(["--theme", theme])
            .stdin(Stdio::null())
            .output_bounded()?;
        assert!(!output.status.success());
        let diagnostic = String::from_utf8(output.stderr)?;
        assert!(
            diagnostic.contains(if theme == "unknown" {
                "invalid arguments"
            } else {
                "interactive sessions require a terminal"
            }),
            "{diagnostic}"
        );
    }
    Ok(())
}

#[test]
fn public_model_flags_parse_and_conflict_with_aliases() -> Result<(), Box<dyn Error>> {
    for flag in ["--model", "-m"] {
        let output = Command::new(env!("CARGO_BIN_EXE_cocoa"))
            .env_clear()
            .args([flag, "openai/private-sentinel", "hello"])
            .stdin(Stdio::null())
            .output_bounded()?;
        assert!(!output.status.success());
        let diagnostic = String::from_utf8(output.stderr)?;
        assert!(
            diagnostic.contains("interactive sessions require a terminal"),
            "{diagnostic}"
        );
        assert!(!diagnostic.contains("private-sentinel"));
        let output = Command::new(env!("CARGO_BIN_EXE_cocoa"))
            .env_clear()
            .args([flag, "openai/private-sentinel", "--alias", "medium"])
            .stdin(Stdio::null())
            .output_bounded()?;
        assert!(!output.status.success());
        let diagnostic = String::from_utf8(output.stderr)?;
        assert!(diagnostic.contains("invalid arguments"), "{diagnostic}");
        assert!(!diagnostic.contains("private-sentinel"));
    }
    Ok(())
}

#[test]
fn cli_exposes_only_the_requested_root_flags() -> Result<(), Box<dyn Error>> {
    let home = tempfile::tempdir()?;
    let output = Command::new(env!("CARGO_BIN_EXE_cocoa"))
        .env_clear()
        .env("HOME", home.path())
        .arg("--help")
        .stdin(Stdio::null())
        .output_bounded()?;
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let help = String::from_utf8(output.stdout)?;
    for expected in [
        "login",
        "logout",
        "aliases",
        "models",
        "--alias",
        "--model",
        "-m",
        "--help",
        "--version",
        "sessions",
        "--data-dir",
        "[PROMPT]",
    ] {
        assert!(help.contains(expected), "{help}");
    }
    assert!(
        !help
            .lines()
            .any(|line| { matches!(line.split_whitespace().next(), Some("alias" | "presets")) })
    );
    for removed in [
        "--preset",
        "--prompt",
        "--config",
        "--auth-file",
        "--timeout-ms",
        "--input-bytes",
        "--output-bytes",
        "--agents-active",
    ] {
        assert!(!help.contains(removed), "{help}");
    }
    let output = Command::new(env!("CARGO_BIN_EXE_cocoa"))
        .env_clear()
        .env("HOME", home.path())
        .arg("--version")
        .stdin(Stdio::null())
        .output_bounded()?;
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    assert_eq!(
        String::from_utf8(output.stdout)?,
        concat!("cocoa ", env!("CARGO_PKG_VERSION"), "\n")
    );
    for flag in [
        "--preset",
        "--prompt",
        "--config",
        "--auth-file",
        "--timeout-ms",
        "--input-bytes",
        "--output-bytes",
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_cocoa"))
            .env_clear()
            .env("HOME", home.path())
            .args([flag, "private-sentinel"])
            .stdin(Stdio::null())
            .output_bounded()?;
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("private-sentinel"));
    }
    for (count, expected) in [
        (63, "invalid arguments"),
        (64, "startup arguments exceed limits"),
    ] {
        let output = process::CapturedProcess::spawn(
            Command::new(env!("CARGO_BIN_EXE_cocoa"))
                .env_clear()
                .env("HOME", home.path())
                .args(vec!["private-sentinel"; count]),
        )?
        .finish(std::time::Duration::from_secs(5))?;
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let diagnostic = String::from_utf8(output.stderr)?;
        assert!(diagnostic.contains(expected), "{diagnostic}");
        assert!(!diagnostic.contains("private-sentinel"));
    }
    Ok(())
}

#[test]
fn alias_updates_preserve_settings_and_clear_thinking_explicitly() -> Result<(), Box<dyn Error>> {
    let home = tempfile::tempdir()?;
    let unrelated_directory = home.path().join(".config/unrelated-app");
    std::fs::create_dir_all(&unrelated_directory)?;
    let unrelated_path = unrelated_directory.join("config.json");
    let unrelated = serde_json::to_vec(&json!({
        "providers":{"ollama":{}},
        "aliases":{"unrelated":[{"provider":"ollama","model":"unrelated-model"}]},
        "default_alias":"unrelated","input_bytes":73
    }))?;
    std::fs::write(&unrelated_path, &unrelated)?;
    let directory = home.path().join(".config/cocoa");
    std::fs::create_dir_all(&directory)?;
    let path = directory.join("config.json");
    std::fs::write(
        &path,
        serde_json::to_vec(&json!({
            "providers":{"openai":{"credential_env":"EXPLICIT_KEY","endpoint":"http://127.0.0.1:1/v1/responses"}},
            "aliases":{"high":[
                {"provider":"openai","model":"fallback","effort":"low","priority":99,"fallback_only":true},
                {"provider":"openai","model":"secondary","effort":"medium","priority":99},
                {"provider":"openai","model":"before","thinking_level":"high","reasoning_summary":true,"priority":31},
                {"provider":"openai","model":"zero-fallback","priority":0,"fallback_only":true}
            ]},
            "default_alias":"high","input_bytes":37,"agents_active":2,"message_steering":false,"web_search":{"provider":"disabled"},
            "compaction":{"request_bytes":268435456,"summary_input_bytes":268435456}
        }))?,
    )?;
    let run = |extra: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_cocoa"))
            .env_clear()
            .env("HOME", home.path())
            .args(["alias", "high", "--model=after", "--provider=openai"])
            .args(extra)
            .stdin(Stdio::null())
            .output_bounded()
    };
    let output = run(&[])?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    assert_eq!(value["default_alias"], "high");
    assert_eq!(value["aliases"]["high"][2]["model"], "after");
    assert_eq!(
        value["aliases"].as_object().map(serde_json::Map::len),
        Some(1)
    );
    assert_eq!(value["aliases"]["high"].as_array().map(Vec::len), Some(4));
    assert_eq!(
        value["aliases"]["high"][0],
        json!({"provider":"openai","model":"fallback","effort":"low","priority":99,"fallback_only":true})
    );
    assert_eq!(
        value["aliases"]["high"][1],
        json!({"provider":"openai","model":"secondary","effort":"medium","priority":99})
    );
    assert_eq!(
        value["aliases"]["high"][3],
        json!({"provider":"openai","model":"zero-fallback","priority":0,"fallback_only":true})
    );
    assert_eq!(value["aliases"]["high"][2]["effort"], "high");
    assert!(value["aliases"]["high"][2].get("thinking_level").is_none());
    assert_eq!(value["aliases"]["high"][2]["priority"], 31);
    assert_eq!(
        value["providers"]["openai"]["credential_env"],
        "EXPLICIT_KEY"
    );
    assert_eq!(
        value["providers"]["openai"]["endpoint"],
        "http://127.0.0.1:1/v1/responses"
    );
    assert_eq!(value["aliases"]["high"][2]["reasoning_summary"], true);
    assert_eq!(value["input_bytes"], 37);
    assert_eq!(value["agents_active"], 2);
    assert_eq!(value["message_steering"], false);
    assert_eq!(value["web_search"], json!({"provider":"disabled"}));
    assert_eq!(
        value["compaction"],
        json!({
            "request_bytes":268435456,"trigger_bytes":50331648,"target_bytes":33554432,
            "summary_input_bytes":268435456
        })
    );
    let preserved_choices = value["aliases"]["high"].clone();
    assert!(run(&["--thinking=default"])?.status.success());
    let value: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    assert!(value["aliases"]["high"][2].get("effort").is_none());
    assert!(value["aliases"]["high"][2].get("thinking_level").is_none());
    assert!(run(&["--thinking=none"])?.status.success());
    let value: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    assert_eq!(value["aliases"]["high"][2]["effort"], "none");
    assert_eq!(value["aliases"]["high"][2]["priority"], 31);
    assert_eq!(value["aliases"]["high"][0], preserved_choices[0]);
    assert_eq!(value["aliases"]["high"][1], preserved_choices[1]);
    assert_eq!(value["aliases"]["high"][3], preserved_choices[3]);
    let before = std::fs::read(&path)?;
    let output = Command::new(env!("CARGO_BIN_EXE_cocoa"))
        .env_clear()
        .env("HOME", home.path())
        .args([
            "alias",
            "new",
            "--provider=ollama",
            "--model=local",
            "--thinking=high",
        ])
        .stdin(Stdio::null())
        .output_bounded()?;
    assert!(!output.status.success());
    assert_eq!(std::fs::read(&path)?, before);
    for (field, replacement, expected) in [
        ("priority", json!(100), "aliases require"),
        ("priority", json!(256), "invalid JSON configuration"),
        ("priority", json!(-100), "invalid JSON configuration"),
        ("priority", json!(-1), "invalid JSON configuration"),
        ("priority", json!("first"), "invalid JSON configuration"),
        ("fallback_only", json!(0), "invalid JSON configuration"),
        ("fallback_only", json!("true"), "invalid JSON configuration"),
        ("fallback_only", json!(null), "invalid JSON configuration"),
        ("fallback_only", json!([]), "invalid JSON configuration"),
        ("fallback_only", json!({}), "invalid JSON configuration"),
    ] {
        let mut invalid = value.clone();
        invalid["aliases"]["high"][0][field] = replacement;
        let bytes = serde_json::to_vec(&invalid)?;
        std::fs::write(&path, &bytes)?;
        let output = run(&[])?;
        assert!(!output.status.success(), "{invalid}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected),
            "{invalid}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(std::fs::read(&path)?, bytes);
    }
    let mut invalid = value.clone();
    invalid["aliases"]["fallback-list"] = json!([
        {"provider":"openai","model":"fallback","priority":0,"fallback_only":true},
        {"provider":"openai","model":"last-fallback","priority":99,"fallback_only":true}
    ]);
    let bytes = serde_json::to_vec(&invalid)?;
    std::fs::write(&path, &bytes)?;
    let output = run(&[])?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("aliases require"));
    assert_eq!(std::fs::read(&path)?, bytes);
    let valid = serde_json::to_string(&value)?;
    for duplicate in [
        valid.replace(
            "\"request_bytes\":268435456",
            "\"request_bytes\":268435456,\"request_bytes\":1048576",
        ),
        valid.replace("\"compaction\":", "\"compaction\":{},\"compaction\":"),
    ] {
        std::fs::write(&path, &duplicate)?;
        let output = run(&[])?;
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("invalid JSON configuration"));
        assert_eq!(std::fs::read_to_string(&path)?, duplicate);
    }
    for removed in [
        "presets",
        "default_preset",
        "tiers",
        "default_tier",
        "order",
    ] {
        let mut compatible = value.clone();
        if removed == "order" {
            compatible["aliases"]["high"][2][removed] = json!(31);
        } else {
            compatible[removed] = json!(null);
        }
        let bytes = serde_json::to_vec(&compatible)?;
        std::fs::write(&path, &bytes)?;
        let output = run(&[])?;
        assert!(output.status.success(), "{removed}");
        let saved: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
        assert_eq!(saved, value);
    }
    assert_eq!(std::fs::read(&unrelated_path)?, unrelated);
    assert_eq!(
        std::fs::read_dir(&unrelated_directory)?
            .collect::<Result<Vec<_>, _>>()?
            .len(),
        1
    );
    Ok(())
}

#[test]
fn first_alias_can_be_saved_without_credentials_and_invalid_edits_are_atomic()
-> Result<(), Box<dyn Error>> {
    let home = tempfile::tempdir()?;
    let unrelated_directory = home.path().join(".config/unrelated-app");
    std::fs::create_dir_all(&unrelated_directory)?;
    let unrelated_path = unrelated_directory.join("config.json");
    let unrelated = serde_json::to_vec(&json!({
        "providers":{"ollama":{}},
        "aliases":{"unrelated":[{"provider":"ollama","model":"unrelated-model"}]},
        "default_alias":"unrelated","input_bytes":73
    }))?;
    std::fs::write(&unrelated_path, &unrelated)?;
    let run = |name: &str| {
        Command::new(env!("CARGO_BIN_EXE_cocoa"))
            .env_clear()
            .env("HOME", home.path())
            .args([
                "alias",
                name,
                "--model=chosen-model",
                "--provider=openai",
                "--thinking=low",
            ])
            .stdin(Stdio::null())
            .output_bounded()
    };
    assert!(run("custom")?.status.success());
    let path = home.path().join(".config/cocoa/config.json");
    let value: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    assert_eq!(value["default_alias"], "custom");
    assert_eq!(
        value["aliases"].as_object().map(serde_json::Map::len),
        Some(1)
    );
    assert_ne!(value["input_bytes"], 73);
    assert_eq!(
        value["aliases"]["custom"],
        json!([{"provider":"openai","model":"chosen-model","effort":"low","priority":0}])
    );
    assert!(!home.path().join(".config/cocoa/auth.json").exists());
    let before = std::fs::read(&path)?;
    assert!(!run("bad name")?.status.success());
    assert_eq!(std::fs::read(&path)?, before);
    std::fs::write(&path, b"invalid-private-sentinel")?;
    let output = run("another")?;
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("private-sentinel"));
    assert_eq!(std::fs::read(&path)?, b"invalid-private-sentinel");
    assert_eq!(std::fs::read(&unrelated_path)?, unrelated);
    assert_eq!(
        std::fs::read_dir(&unrelated_directory)?
            .collect::<Result<Vec<_>, _>>()?
            .len(),
        1
    );
    Ok(())
}

#[test]
fn management_and_nonterminal_launches_do_not_create_session_storage() -> Result<(), Box<dyn Error>>
{
    let home = tempfile::tempdir()?;
    let data = home.path().join("not-a-directory");
    std::fs::write(&data, b"private-sentinel")?;
    for (arguments, success) in [
        (vec!["--help"], true),
        (vec!["--version"], true),
        (
            vec!["alias", "medium", "--provider=openai", "--model=fixture"],
            true,
        ),
        (vec!["search", "disabled"], true),
        (vec!["logout", "openai"], true),
        (vec!["mcp", "list"], true),
        (vec![], false),
        (vec!["hello"], false),
        (vec!["aliases"], false),
        (vec!["mcp"], false),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_cocoa"))
            .env_clear()
            .env("HOME", home.path())
            .current_dir(home.path())
            .arg("--data-dir")
            .arg(&data)
            .args(&arguments)
            .stdin(Stdio::null())
            .output_bounded()?;
        assert_eq!(
            output.status.success(),
            success,
            "{arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        if !success {
            assert!(
                String::from_utf8_lossy(&output.stderr)
                    .contains("interactive sessions require a terminal")
            );
        }
        assert_eq!(std::fs::read(&data)?, b"private-sentinel");
        assert!(!home.path().join(".local").exists());
    }
    Ok(())
}

#[test]
fn session_listing_keeps_missing_storage_absent() -> Result<(), Box<dyn Error>> {
    let home = tempfile::tempdir()?;
    let missing = home.path().join("missing/data");
    for arguments in [
        vec!["sessions", "list", "--json"],
        vec!["sessions", "list", "--all", "--json"],
        vec![
            "sessions",
            "list",
            "--data-dir",
            missing.to_str().ok_or("temporary path is not UTF-8")?,
            "--json",
        ],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_cocoa"))
            .env_clear()
            .env("HOME", home.path())
            .current_dir(home.path())
            .args(arguments)
            .stdin(Stdio::null())
            .output_bounded()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stderr.is_empty());
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout)?,
            json!({"sessions":[], "next_cursor":null})
        );
        assert!(
            std::fs::read_dir(home.path())?
                .collect::<Result<Vec<_>, _>>()?
                .is_empty()
        );
    }
    let data = home.path().join("empty");
    create_private_directory(&data)?;
    let output = session_list(home.path(), &data, home.path())
        .stdin(Stdio::null())
        .output_bounded()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    assert_eq!(output.stdout, b"No sessions found.\n");
    assert!(
        std::fs::read_dir(&data)?
            .collect::<Result<Vec<_>, _>>()?
            .is_empty()
    );
    assert_eq!(
        std::fs::read_dir(home.path())?
            .collect::<Result<Vec<_>, _>>()?
            .len(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn session_search_matches_all_words_without_configuration_or_credentials()
-> Result<(), Box<dyn Error>> {
    use cocoa_core::store::{InvocationId, Record, RecordKind, Store, StoreLimits};
    let home = tempfile::tempdir()?;
    let data = home.path().join("data");
    let run = |query: &str| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cocoa"));
        command
            .env_clear()
            .env("HOME", home.path())
            .current_dir(home.path())
            .arg("--data-dir")
            .arg(&data)
            .args(["sessions", "search", query, "--json"]);
        command
    };
    let absent = run("100%_ OR").stdin(Stdio::null()).output_bounded()?;
    assert!(absent.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&absent.stdout)?,
        json!({"sessions":[],"limit":20})
    );
    assert!(!data.exists());
    create_private_directory(&data)?;
    let database = SessionDatabase::open(&data.join("sessions.sqlite3")).await?;
    let attachment = database.create_session(home.path()).await?;
    let store = attachment.root_store(StoreLimits {
        record_bytes: 65_536,
    })?;
    store
        .append(
            &Record {
                invocation: InvocationId(1),
                kind: RecordKind::InputAccepted {
                    input: "Été 100%_ OR literal phrase".into(),
                },
            },
            None,
        )
        .await?;
    let config = home.path().join(".config/cocoa");
    std::fs::create_dir_all(&config)?;
    std::fs::write(
        config.join("config.json"),
        b"malformed-config-private-sentinel",
    )?;
    std::fs::write(config.join("auth.json"), b"malformed-auth-private-sentinel")?;
    let before = database.list_sessions(None, 20, None).await?;
    let matched = run("  or 100%_ été  ")
        .stdin(Stdio::null())
        .output_bounded()?;
    assert!(
        matched.status.success(),
        "{}",
        String::from_utf8_lossy(&matched.stderr)
    );
    assert!(matched.stderr.is_empty());
    let matched: Value = serde_json::from_slice(&matched.stdout)?;
    assert_eq!(matched["sessions"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        matched["sessions"][0]["public_id"],
        attachment.public_id().as_str()
    );
    assert_eq!(matched["sessions"][0]["source"], "title");
    assert_eq!(
        matched["sessions"][0]["snippet"],
        "Été 100%_ OR literal phrase"
    );
    for query in ["été literal", "% OR"] {
        let output = run(query).stdin(Stdio::null()).output_bounded()?;
        assert!(output.status.success());
        assert!(output.stderr.is_empty());
        assert_eq!(serde_json::from_slice::<Value>(&output.stdout)?, matched);
    }
    for query in [
        "100_ OR",
        "été absent",
        "% missing",
        "malformed-config-private-sentinel",
    ] {
        let output = run(query).stdin(Stdio::null()).output_bounded()?;
        assert!(output.status.success());
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout)?["sessions"],
            json!([])
        );
    }
    assert_eq!(database.list_sessions(None, 20, None).await?, before);
    Ok(())
}

#[tokio::test]
async fn session_listing_filters_and_paginates_without_configuration() -> Result<(), Box<dyn Error>>
{
    let home = tempfile::tempdir()?;
    let data = home.path().join("data");
    create_private_directory(&data)?;
    let project = home.path().join("project");
    let nested = project.join("nested");
    let other = home.path().join("other");
    std::fs::create_dir_all(&nested)?;
    std::fs::create_dir(&other)?;
    let config = home.path().join(".config/cocoa");
    std::fs::create_dir_all(&config)?;
    for file in ["config.json", "auth.json"] {
        std::fs::write(config.join(file), b"invalid-private-sentinel")?;
    }
    let path = data.join("sessions.sqlite3");
    let database = SessionDatabase::open(&path).await?;
    let mut sessions = Vec::new();
    for _ in 0..23 {
        sessions.push(database.create_session(&project).await?);
    }
    sessions.push(database.create_session(&other).await?);
    sessions.push(database.create_session(&nested).await?);
    let mut admin =
        SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path)).await?;
    sqlx::query(
        "UPDATE sessions SET created_at_ms = 7, updated_at_ms = 100, last_opened_at_ms = 9",
    )
    .execute(&mut admin)
    .await?;
    let mut transaction = admin.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("UPDATE sessions SET title = 'uncommitted-private-sentinel'")
        .execute(&mut *transaction)
        .await?;

    let output = session_list(home.path(), &data, &project)
        .arg("--json")
        .stdin(Stdio::null())
        .output_bounded()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let page: Value = serde_json::from_slice(&output.stdout)?;
    let rows = page["sessions"].as_array().ok_or("missing sessions")?;
    assert_eq!(rows.len(), 20);
    for (row, session) in rows.iter().zip(sessions[..23].iter().rev()) {
        assert_eq!(row["public_id"], session.public_id().as_str());
        assert_eq!(row["id"], i64::from(session.id()).to_string());
        assert_eq!(
            row["working_directory"],
            project.canonicalize()?.to_str().ok_or("invalid path")?
        );
        assert_eq!(row["title"], Value::Null);
        assert_eq!(row["created_at_ms"], 7);
        assert_eq!(row["updated_at_ms"], 100);
        assert_eq!(row["last_opened_at_ms"], 9);
    }
    let cursor = format!("v1:100:{}", i64::from(sessions[3].id()));
    assert_eq!(page["next_cursor"], cursor);
    let output = session_list(home.path(), &data, &project)
        .args(["--json", "--before", &cursor])
        .stdin(Stdio::null())
        .output_bounded()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let page: Value = serde_json::from_slice(&output.stdout)?;
    let rows = page["sessions"].as_array().ok_or("missing sessions")?;
    assert_eq!(rows.len(), 3);
    for (row, session) in rows.iter().zip(sessions[..3].iter().rev()) {
        assert_eq!(row["public_id"], session.public_id().as_str());
    }
    assert_eq!(page["next_cursor"], Value::Null);

    let output = session_list(home.path(), &data, &project)
        .args(["--all", "--limit", "50", "--json"])
        .stdin(Stdio::null())
        .output_bounded()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let page: Value = serde_json::from_slice(&output.stdout)?;
    let rows = page["sessions"].as_array().ok_or("missing sessions")?;
    assert_eq!(rows.len(), 25);
    for (row, session) in rows.iter().zip(sessions.iter().rev()) {
        assert_eq!(row["public_id"], session.public_id().as_str());
    }
    assert_eq!(page["next_cursor"], Value::Null);
    let output = session_list(home.path(), &data, &project)
        .args(["--limit", "1"])
        .stdin(Stdio::null())
        .output_bounded()?;
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout)?.lines().count(), 2);
    assert_eq!(
        String::from_utf8(output.stderr)?,
        format!("Next cursor: v1:100:{}\n", i64::from(sessions[22].id()))
    );
    transaction.rollback().await?;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions WHERE created_at_ms = 7 AND updated_at_ms = 100 AND last_opened_at_ms = 9 AND title IS NULL")
        .fetch_one(&mut admin).await?;
    assert_eq!(count, 25);
    for file in ["config.json", "auth.json"] {
        assert_eq!(
            std::fs::read(config.join(file))?,
            b"invalid-private-sentinel"
        );
    }
    admin.close().await?;
    Ok(())
}

#[tokio::test]
async fn session_listing_uses_xdg_home_and_explicit_data_paths() -> Result<(), Box<dyn Error>> {
    let home = tempfile::tempdir()?;
    let fallback = home.path().join(".local/share/cocoa");
    let xdg = home.path().join("xdg");
    let explicit = home.path().join("explicit");
    for path in [&fallback, &xdg.join("cocoa"), &explicit] {
        create_private_directory(path)?;
    }
    let mut sessions = Vec::new();
    for path in [&fallback, &xdg.join("cocoa"), &explicit] {
        let database = SessionDatabase::open(&path.join("sessions.sqlite3")).await?;
        sessions.push(database.create_session(home.path()).await?);
    }
    for (xdg_home, data_directory, expected) in [
        (None, None, &sessions[0]),
        (Some(Path::new("relative-is-ignored")), None, &sessions[0]),
        (Some(xdg.as_path()), None, &sessions[1]),
        (Some(xdg.as_path()), Some(explicit.as_path()), &sessions[2]),
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cocoa"));
        command
            .env_clear()
            .env("HOME", home.path())
            .current_dir(home.path());
        if let Some(xdg_home) = xdg_home {
            command.env("XDG_DATA_HOME", xdg_home);
        }
        if let Some(data_directory) = data_directory {
            command.arg("--data-dir").arg(data_directory);
        }
        let output = command
            .args(["sessions", "list", "--json"])
            .stdin(Stdio::null())
            .output_bounded()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stderr.is_empty());
        let page: Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(
            page["sessions"].as_array().ok_or("missing sessions")?.len(),
            1
        );
        assert_eq!(
            page["sessions"][0]["public_id"],
            expected.public_id().as_str()
        );
    }
    assert!(!home.path().join("relative-is-ignored").exists());
    assert!(!home.path().join(".config").exists());
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn session_listing_escapes_terminal_text_and_preserves_json() -> Result<(), Box<dyn Error>> {
    let home = tempfile::tempdir()?;
    let data = home.path().join("data");
    create_private_directory(&data)?;
    let project = home.path().join("résumé\n\t\u{1b}[2J\\literal\u{202e}");
    std::fs::create_dir(&project)?;
    let path = data.join("sessions.sqlite3");
    let database = SessionDatabase::open(&path).await?;
    let titled = database.create_session(&project).await?;
    let untitled = database.create_session(&project).await?;
    let title = "résumé\n\t\u{1b}]52;c;payload\u{7}\u{009b}2J\\literal\u{202e}👩‍💻";
    let mut admin =
        SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path)).await?;
    sqlx::query("UPDATE sessions SET title = ? WHERE id = ?")
        .bind(title)
        .bind(i64::from(titled.id()))
        .execute(&mut admin)
        .await?;
    let output = session_list(home.path(), &data, &project)
        .stdin(Stdio::null())
        .output_bounded()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let text = String::from_utf8(output.stdout)?;
    assert_eq!(text.lines().count(), 3);
    assert!(text.lines().all(|line| line.split('\t').count() == 4));
    assert!(text.contains(titled.public_id().as_str()));
    assert!(text.contains(untitled.public_id().as_str()));
    assert!(text.contains("Untitled session"));
    assert!(text.contains("résumé\\n\\t\\u{1b}]52;c;payload\\u{7}\\u{9b}2J\\\\literal\\u{202e}👩‍💻"));
    assert!(text.contains("résumé\\n\\t\\u{1b}[2J\\\\literal\\u{202e}"));
    assert!(
        !text
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\t'))
    );
    assert!(!text.contains('\u{202e}'));

    let output = session_list(home.path(), &data, &project)
        .arg("--json")
        .stdin(Stdio::null())
        .output_bounded()?;
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let page: Value = serde_json::from_slice(&output.stdout)?;
    let row = page["sessions"]
        .as_array()
        .ok_or("missing sessions")?
        .iter()
        .find(|row| row["public_id"] == titled.public_id().as_str())
        .ok_or("missing titled session")?;
    assert_eq!(row["title"], title);
    assert_eq!(
        row["working_directory"],
        project.canonicalize()?.to_str().ok_or("invalid path")?
    );
    admin.close().await?;
    Ok(())
}

#[test]
fn session_listing_rejects_invalid_arguments_without_opening_storage() -> Result<(), Box<dyn Error>>
{
    let home = tempfile::tempdir()?;
    let database = tempfile::NamedTempFile::new_in(home.path())?;
    std::fs::write(database.path(), b"private-sentinel")?;
    for arguments in [
        vec!["--limit", "0"],
        vec!["--limit", "51"],
        vec!["--limit", "-1"],
        vec!["--limit", "4294967296"],
        vec!["--before", "v1:0:0"],
        vec!["--before", "v2:1:1"],
        vec!["--before", "private-sentinel"],
    ] {
        let output = session_list(home.path(), database.path(), home.path())
            .args(arguments)
            .stdin(Stdio::null())
            .output_bounded()?;
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let diagnostic = String::from_utf8(output.stderr)?;
        assert!(diagnostic.contains("invalid arguments"), "{diagnostic}");
        assert!(!diagnostic.contains("private-sentinel"));
        assert_eq!(std::fs::read(database.path())?, b"private-sentinel");
        assert_eq!(
            std::fs::read_dir(home.path())?
                .collect::<Result<Vec<_>, _>>()?
                .len(),
            1
        );
    }
    let output = session_list(home.path(), database.path(), home.path())
        .arg("--help")
        .stdin(Stdio::null())
        .output_bounded()?;
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let help = String::from_utf8(output.stdout)?;
    for flag in ["--all", "--before", "--json", "--limit", "--data-dir"] {
        assert!(help.contains(flag), "{help}");
    }
    assert_eq!(
        std::fs::read_dir(home.path())?
            .collect::<Result<Vec<_>, _>>()?
            .len(),
        1
    );
    Ok(())
}

#[test]
fn session_listing_reports_invalid_storage_without_initializing_it() -> Result<(), Box<dyn Error>> {
    let home = tempfile::tempdir()?;
    let data = home.path().join("data");
    create_private_directory(&data)?;
    let path = data.join("sessions.sqlite3");
    tempfile::NamedTempFile::new_in(&data)?.persist(&path)?;
    for contents in [b"".as_slice(), b"private-sentinel".as_slice()] {
        std::fs::write(&path, contents)?;
        let output = session_list(home.path(), &data, home.path())
            .arg("--json")
            .stdin(Stdio::null())
            .output_bounded()?;
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let diagnostic = String::from_utf8(output.stderr)?;
        assert!(
            diagnostic.contains(if contents.is_empty() {
                "uninitialized database"
            } else {
                "SQLite operation failed"
            }),
            "{diagnostic}"
        );
        assert!(!diagnostic.contains("private-sentinel"));
        assert_eq!(std::fs::read(&path)?, contents);
    }
    Ok(())
}

fn session_list(home: &Path, data_directory: &Path, directory: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cocoa"));
    command
        .env_clear()
        .env("HOME", home)
        .current_dir(directory)
        .arg("--data-dir")
        .arg(data_directory)
        .args(["sessions", "list"]);
    command
}

fn create_private_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(path)
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(path)
}
