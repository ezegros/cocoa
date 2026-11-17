//! Remembered startup selection stays separate from user configuration.

use cocoa::config::{self, ConfigError, Selection};
use cocoa_llm::ThinkingLevel;
use serde_json::json;
use std::{error::Error, fs, process::Command};

#[test]
fn remembered_selection() -> Result<(), Box<dyn Error>> {
    let home = tempfile::tempdir()?;
    let output = Command::new(std::env::current_exe()?)
        .args(["--exact", "remembered_selection_child", "--nocapture"])
        .env("HOME", home.path())
        .env("COCOA_SELECTION_TEST", "1")
        .env("OPENAI_API_KEY", "selection-test-key")
        .output()?;
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[test]
fn remembered_selection_child() -> Result<(), Box<dyn Error>> {
    if std::env::var_os("COCOA_SELECTION_TEST").is_none() {
        return Ok(());
    }
    let home = std::env::home_dir().ok_or("missing test home")?;
    let directory = home.join(".config/cocoa");
    fs::create_dir_all(&directory)?;
    let settings_path = directory.join("config.json");
    let state_path = directory.join("selection.json");
    let mut settings = json!({
        "providers": {"openai": {}},
        "aliases": {"work": [
            {"provider":"openai", "model":"first", "priority":0},
            {"provider":"openai", "model":"second", "priority":1, "effort":"high"}
        ]}
    });
    fs::write(&settings_path, serde_json::to_vec(&settings)?)?;
    let settings_original = fs::read(&settings_path)?;
    assert!(config::has_configured_providers()?);
    assert!(matches!(
        config::load(None),
        Err(ConfigError::AliasRequired)
    ));
    assert!(!state_path.exists());

    let direct = Selection {
        model: Some("openai/direct".into()),
        thinking: Some(ThinkingLevel::High),
        ..Selection::default()
    };
    config::load_selection(None, direct.model.clone(), direct.thinking, None)?;
    assert!(!state_path.exists(), "loading must not remember selections");
    remember_selection(&direct)?;
    let restored = config::load(None)?;
    assert_eq!(restored.model, "direct");
    assert_eq!(restored.thinking_level, Some(ThinkingLevel::High));
    assert_eq!(fs::read(&settings_path)?, settings_original);
    settings["input_bytes"] = json!(0);
    fs::write(&settings_path, serde_json::to_vec(&settings)?)?;
    assert!(matches!(config::load(None), Err(ConfigError::Bounds)));
    fs::remove_file(&state_path)?;
    fs::write(&settings_path, b"invalid JSON")?;
    config::remember_selection(&restored)?;
    let saved: serde_json::Value = serde_json::from_slice(&fs::read(&state_path)?)?;
    assert_eq!(saved["selection"]["model"], "openai/direct");
    assert_eq!(saved["selection"]["thinking"], "high");
    fs::write(&settings_path, &settings_original)?;
    settings
        .as_object_mut()
        .ok_or("settings object")?
        .remove("input_bytes");
    let state_original = fs::read(&state_path)?;
    assert_eq!(config::load(Some("work".into()))?.model, "first");
    assert_eq!(fs::read(&state_path)?, state_original);

    settings["default_alias"] = json!("work");
    fs::write(&settings_path, serde_json::to_vec(&settings)?)?;
    assert_eq!(config::load(None)?.model, "first");
    assert_eq!(
        config::load_selection(None, Some("openai/explicit".into()), None, None)?.model,
        "explicit"
    );
    assert_eq!(fs::read(&state_path)?, state_original);
    remember_selection(&Selection::default())?;
    settings
        .as_object_mut()
        .ok_or("settings object")?
        .remove("default_alias");
    fs::write(&settings_path, serde_json::to_vec(&settings)?)?;
    assert_eq!(config::load(None)?.model, "first");

    remember_selection(&Selection {
        alias: Some("work".into()),
        choice: Some(1),
        ..Selection::default()
    })?;
    let restored = config::load(None)?;
    assert_eq!(restored.alias, "work");
    assert_eq!(restored.model, "second");
    assert_eq!(restored.thinking_level, Some(ThinkingLevel::High));
    let aliases_original = settings["aliases"].clone();
    settings["aliases"]["work"] = json!([]);
    fs::write(&settings_path, serde_json::to_vec(&settings)?)?;
    assert!(matches!(config::load(None), Err(ConfigError::AliasChoices)));
    settings["aliases"]["work"] = json!([
        {"provider":"openai", "model":"first", "priority":0}
    ]);
    fs::write(&settings_path, serde_json::to_vec(&settings)?)?;
    assert!(matches!(
        config::load(None),
        Err(ConfigError::AliasRequired)
    ));
    settings["aliases"] = aliases_original;
    settings["aliases"]["work"][1]["model"] = json!("replacement");
    fs::write(&settings_path, serde_json::to_vec(&settings)?)?;
    assert!(matches!(
        config::load(None),
        Err(ConfigError::AliasRequired)
    ));
    assert_eq!(config::load(Some("work".into()))?.model, "first");
    settings["aliases"] = json!({});
    fs::write(&settings_path, serde_json::to_vec(&settings)?)?;
    assert!(matches!(
        config::load(None),
        Err(ConfigError::AliasRequired)
    ));

    remember_selection(&direct)?;
    settings["providers"] = json!({});
    fs::write(&settings_path, serde_json::to_vec(&settings)?)?;
    assert!(matches!(
        config::load(None),
        Err(ConfigError::AliasRequired)
    ));
    assert_eq!(
        config::load_selection(None, direct.model.clone(), direct.thinking, None)?.model,
        "direct"
    );
    settings["providers"] = json!({"openai":{}});
    fs::write(&settings_path, serde_json::to_vec(&settings)?)?;
    settings["providers"]["openai"]["credential_env"] = json!("COCOA_MISSING_SELECTION_KEY");
    fs::write(&settings_path, serde_json::to_vec(&settings)?)?;
    assert!(matches!(
        config::load(None),
        Err(ConfigError::AliasRequired)
    ));
    let state_before_failure = fs::read(&state_path)?;
    assert!(remember_selection(&direct).is_err());
    assert_eq!(fs::read(&state_path)?, state_before_failure);
    assert!(config::load_selection(None, direct.model.clone(), direct.thinking, None).is_err());

    let mut conflicting: serde_json::Value = serde_json::from_slice(&state_before_failure)?;
    conflicting["selection"]["alias"] = json!("work");
    let mut invalid_model: serde_json::Value = serde_json::from_slice(&state_before_failure)?;
    invalid_model["selection"]["model"] = json!("unknown/model");
    for invalid in [
        b"invalid JSON".to_vec(),
        vec![b' '; 65_537],
        serde_json::to_vec(&conflicting)?,
        serde_json::to_vec(&invalid_model)?,
    ] {
        fs::write(&state_path, &invalid)?;
        assert!(matches!(
            config::load(None),
            Err(ConfigError::AliasRequired)
        ));
        assert_eq!(fs::read(&state_path)?, invalid);
    }
    Ok(())
}

#[test]
fn global_validation_precedes_remembered_recovery() -> Result<(), Box<dyn Error>> {
    let home = tempfile::tempdir()?;
    let output = Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "global_validation_precedes_remembered_recovery_child",
            "--nocapture",
        ])
        .env("HOME", home.path())
        .env("COCOA_SELECTION_VALIDATION_TEST", "1")
        .env("OPENAI_API_KEY", "selection-test-key")
        .output()?;
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[test]
fn global_validation_precedes_remembered_recovery_child() -> Result<(), Box<dyn Error>> {
    if std::env::var_os("COCOA_SELECTION_VALIDATION_TEST").is_none() {
        return Ok(());
    }
    let home = std::env::home_dir().ok_or("missing test home")?;
    let directory = home.join(".config/cocoa");
    fs::create_dir_all(&directory)?;
    let settings_path = directory.join("config.json");
    let settings = json!({
        "providers": {"openai": {}},
        "aliases": {"work": [{"provider": "openai", "model": "first"}]}
    });
    fs::write(&settings_path, serde_json::to_vec(&settings)?)?;
    let direct = config::load_selection(None, Some("openai/direct".into()), None, None)?;
    config::remember_selection(&direct)?;

    let mut removed_provider = settings.clone();
    removed_provider["providers"] = json!({});
    removed_provider["aliases"] = json!({});
    removed_provider["input_bytes"] = json!(0);
    fs::write(&settings_path, serde_json::to_vec(&removed_provider)?)?;
    assert!(matches!(config::load(None), Err(ConfigError::Bounds)));

    removed_provider = json!({
        "providers": {},
        "mcp_servers": {"": {
            "id": "0123456789abcdef0123456789abcdef",
            "url": "https://example.com/mcp",
            "enabled": true
        }}
    });
    fs::write(&settings_path, serde_json::to_vec(&removed_provider)?)?;
    assert!(matches!(
        config::load(None),
        Err(ConfigError::Mcp(cocoa::mcp::McpError::Configuration))
    ));

    fs::write(&settings_path, br#"{"providers":{}}"#)?;
    assert!(matches!(
        config::load(None),
        Err(ConfigError::AliasRequired)
    ));

    let mut invalid_auth = settings.clone();
    invalid_auth["providers"]["ollama"] = json!({"credential_env": "OLLAMA_API_KEY"});
    fs::write(&settings_path, serde_json::to_vec(&invalid_auth)?)?;
    assert!(matches!(
        config::load(None),
        Err(ConfigError::Auth(cocoa::auth::AuthError::OllamaCredential))
    ));

    fs::write(&settings_path, serde_json::to_vec(&settings)?)?;
    let alias = config::load(Some("work".into()))?;
    config::remember_selection(&alias)?;
    let mut removed_alias = settings;
    removed_alias["aliases"] = json!({});
    removed_alias["input_bytes"] = json!(0);
    fs::write(&settings_path, serde_json::to_vec(&removed_alias)?)?;
    assert!(matches!(config::load(None), Err(ConfigError::Bounds)));

    fs::write(&settings_path, br#"{"providers":{"openai":{}}}"#)?;
    assert!(matches!(
        config::load(None),
        Err(ConfigError::AliasRequired)
    ));
    Ok(())
}

fn remember_selection(selection: &Selection) -> Result<(), ConfigError> {
    let configuration = config::load_selection(
        selection.alias.clone(),
        selection.model.clone(),
        selection.thinking,
        selection.choice,
    )?;
    config::remember_selection(&configuration)
}
