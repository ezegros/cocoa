//! Remembered startup selection stays separate from user configuration.

use cocoa::config::{self, ConfigError, Selection, SelectorPreferences};
use cocoa_llm::ThinkingLevel;
use serde_json::json;
use std::{error::Error, fs, process::Command};

#[test]
fn agent_model_selection_inherits_active_profile() -> Result<(), Box<dyn Error>> {
    use cocoa::agents::AgentSelection;
    use cocoa::config::{CredentialSource, ResolvedAlias};
    use cocoa_llm::{ModelLimits, ProviderKind};
    use std::collections::BTreeMap;

    let caller = ResolvedAlias {
        kind: ProviderKind::OpenAi,
        endpoint: "https://api.openai.com/v1/responses".into(),
        model: "caller-active".into(),
        model_limits: ModelLimits {
            output_tokens: Some(4096),
            context_tokens: Some(32768),
        },
        api_key: Some("test-key".into()),
        thinking_level: Some(ThinkingLevel::High),
        reasoning_summary: false,
        credential_source: CredentialSource::Saved,
    };
    let mut configured = caller.clone();
    configured.model = "configured-alias".into();
    configured.thinking_level = Some(ThinkingLevel::Low);
    let aliases = BTreeMap::from([("worker".into(), vec![configured, caller.clone()])]);

    let inherited = config::agent_candidates(caller.clone(), &aliases, &AgentSelection::default())?;
    assert_eq!(inherited.len(), 1);
    assert_eq!(inherited[0].model, "caller-active");
    assert_eq!(inherited[0].thinking_level, Some(ThinkingLevel::High));
    assert_eq!(inherited[0].model_limits.context_tokens, Some(32768));
    assert_eq!(inherited[0].endpoint, caller.endpoint);

    let overridden = config::agent_candidates(
        caller.clone(),
        &aliases,
        &AgentSelection {
            model: Some("openai/explicit".into()),
            ..AgentSelection::default()
        },
    )?;
    assert_eq!(overridden.len(), 1);
    assert_eq!(overridden[0].model, "explicit");
    assert_eq!(overridden[0].thinking_level, Some(ThinkingLevel::High));
    assert_eq!(overridden[0].model_limits.context_tokens, None);

    let thinking = config::agent_candidates(
        caller.clone(),
        &aliases,
        &AgentSelection {
            thinking_level: Some(ThinkingLevel::Low),
            ..AgentSelection::default()
        },
    )?;
    assert_eq!(thinking[0].model, "caller-active");
    assert_eq!(thinking[0].thinking_level, Some(ThinkingLevel::Low));

    let aliased = config::agent_candidates(
        caller.clone(),
        &aliases,
        &AgentSelection {
            alias: Some("worker".into()),
            ..AgentSelection::default()
        },
    )?;
    assert_eq!(aliased.len(), 2);
    assert_eq!(aliased[0].model, "configured-alias");
    assert_eq!(aliased[0].thinking_level, Some(ThinkingLevel::Low));

    let explicit = config::agent_candidates(
        caller.clone(),
        &aliases,
        &AgentSelection {
            alias: Some("worker".into()),
            model: Some("explicit".into()),
            thinking_level: Some(ThinkingLevel::High),
        },
    )?;
    assert_eq!(explicit.len(), 1);
    assert_eq!(explicit[0].model, "explicit");
    assert_eq!(explicit[0].thinking_level, Some(ThinkingLevel::High));

    let mut other_provider = caller.clone();
    other_provider.kind = ProviderKind::OpenRouter;
    other_provider.endpoint = "https://openrouter.ai/api/v1/responses".into();
    other_provider.api_key = Some("other-key".into());
    other_provider.credential_source = CredentialSource::Environment {
        reference: "OPENROUTER_API_KEY".into(),
    };
    let other_aliases = BTreeMap::from([("other".into(), vec![other_provider.clone()])]);
    let cross_provider = config::agent_candidates(
        caller.clone(),
        &other_aliases,
        &AgentSelection {
            model: Some("openrouter/meta-llama/Llama".into()),
            ..AgentSelection::default()
        },
    )?;
    assert_eq!(cross_provider.len(), 1);
    assert_eq!(cross_provider[0].kind, ProviderKind::OpenRouter);
    assert_eq!(cross_provider[0].model, "meta-llama/Llama");
    assert_eq!(cross_provider[0].endpoint, other_provider.endpoint);
    assert_eq!(
        cross_provider[0]
            .api_key
            .as_ref()
            .map(secrecy::ExposeSecret::expose_secret),
        Some("other-key")
    );
    assert!(matches!(
        &cross_provider[0].credential_source,
        CredentialSource::Environment { reference } if reference == "OPENROUTER_API_KEY"
    ));

    for model in ["", "openai/", "unknown/model", "bad\nmodel"] {
        assert!(
            config::agent_candidates(
                caller.clone(),
                &aliases,
                &AgentSelection {
                    model: Some(model.into()),
                    ..AgentSelection::default()
                },
            )
            .is_err()
        );
    }
    assert!(
        config::agent_candidates(
            caller,
            &aliases,
            &AgentSelection {
                alias: Some("missing".into()),
                ..AgentSelection::default()
            },
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn selector_preferences() -> Result<(), Box<dyn Error>> {
    let home = tempfile::tempdir()?;
    let output = Command::new(std::env::current_exe()?)
        .args(["--exact", "selector_preferences_child", "--nocapture"])
        .env("HOME", home.path())
        .env("COCOA_SELECTOR_TEST", "1")
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
fn selector_preferences_child() -> Result<(), Box<dyn Error>> {
    if std::env::var_os("COCOA_SELECTOR_TEST").is_none() {
        return Ok(());
    }
    let home = std::env::home_dir().ok_or("missing test home")?;
    let directory = home.join(".config/cocoa");
    let path = directory.join("selector.json");
    assert_eq!(
        config::selector_preferences()?,
        SelectorPreferences::default()
    );
    assert!(!path.exists());
    fs::create_dir_all(&directory)?;
    let configuration_path = directory.join("config.json");
    fs::write(&configuration_path, b"configuration sentinel")?;
    let preferences = SelectorPreferences {
        model_pins: vec!["openai/z-last".into(), "openai/a-first".into()],
        model_recent: vec!["openai/recent".into()],
        alias_pins: vec!["work".into(), "fast".into()],
        alias_recent: vec!["fast".into(), "work".into()],
    };
    config::save_selector_preferences(&preferences)?;
    assert_eq!(config::selector_preferences()?, preferences);
    for valid in [
        SelectorPreferences {
            model_pins: (0..128).map(|index| format!("openai/{index}")).collect(),
            alias_pins: (0..128).map(|index| format!("alias-{index}")).collect(),
            ..Default::default()
        },
        SelectorPreferences {
            model_pins: vec!["x".repeat(512)],
            alias_pins: vec!["x".repeat(64)],
            model_recent: (0..3).map(|index| format!("openai/{index}")).collect(),
            alias_recent: (0..3).map(|index| format!("alias-{index}")).collect(),
        },
    ] {
        config::save_selector_preferences(&valid)?;
        assert_eq!(config::selector_preferences()?, valid);
    }
    config::save_selector_preferences(&preferences)?;
    let original = fs::read(&path)?;
    for invalid in [
        SelectorPreferences {
            model_pins: vec!["same".into(); 2],
            ..Default::default()
        },
        SelectorPreferences {
            model_pins: vec!["x".repeat(513)],
            ..Default::default()
        },
        SelectorPreferences {
            alias_pins: vec!["x".repeat(65)],
            ..Default::default()
        },
        SelectorPreferences {
            alias_pins: vec![String::new()],
            ..Default::default()
        },
        SelectorPreferences {
            alias_pins: vec!["   ".into()],
            ..Default::default()
        },
        SelectorPreferences {
            alias_pins: (0..129).map(|index| format!("alias-{index}")).collect(),
            ..Default::default()
        },
        SelectorPreferences {
            alias_pins: vec!["bad\nname".into()],
            ..Default::default()
        },
        SelectorPreferences {
            model_pins: (0..129).map(|index| format!("openai/{index}")).collect(),
            ..Default::default()
        },
        SelectorPreferences {
            model_recent: (0..4).map(|index| format!("openai/{index}")).collect(),
            ..Default::default()
        },
        SelectorPreferences {
            alias_recent: (0..4).map(|index| format!("alias-{index}")).collect(),
            ..Default::default()
        },
    ] {
        assert!(
            config::save_selector_preferences(&invalid).is_err(),
            "{invalid:?}"
        );
        assert_eq!(fs::read(&path)?, original);
        fs::write(&path, serde_json::to_vec(&invalid)?)?;
        assert!(config::selector_preferences().is_err(), "{invalid:?}");
        fs::write(&path, &original)?;
    }
    fs::write(&path, br#"{"alias_pins":["work"]}"#)?;
    assert_eq!(
        config::selector_preferences()?,
        SelectorPreferences {
            alias_pins: vec!["work".into()],
            ..Default::default()
        }
    );
    fs::write(&path, b"not JSON")?;
    assert!(config::selector_preferences().is_err());
    fs::write(&path, vec![b' '; 128 * 1024])?;
    assert!(config::selector_preferences().is_err());
    assert_eq!(fs::read(&configuration_path)?, b"configuration sentinel");
    Ok(())
}

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
