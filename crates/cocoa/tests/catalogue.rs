//! Public model catalogue parsing and credential-isolated limit resolution.

use std::{collections::BTreeMap, error::Error};

use cocoa::{
    catalogue::{self, LimitDiscovery},
    config::{CredentialSource, ResolvedAlias},
};
use cocoa_llm::{ModelLimits, ProviderKind};
use serde_json::{Value, json};

#[test]
fn curated_model_ranking_uses_provider_specific_suggestions() {
    assert_eq!(
        catalogue::suggested_model_rank(ProviderKind::OpenAi, "gpt-6-luna"),
        Some(0)
    );
    assert_eq!(
        catalogue::suggested_model_rank(ProviderKind::OpenAi, "gpt-5-mini"),
        Some(1)
    );
    assert_eq!(
        catalogue::suggested_model_rank(ProviderKind::OpenAi, "gpt-6.1-sol"),
        Some(3),
        "a repeated identifier keeps its first rank"
    );
    assert_eq!(
        catalogue::suggested_model_rank(ProviderKind::OpenAi, "gpt-6-astra"),
        Some(6)
    );
    assert_eq!(
        catalogue::suggested_model_rank(ProviderKind::OpenCodeGo, "kimi-k2.7-code"),
        Some(2)
    );
    assert_eq!(
        catalogue::suggested_model_rank(ProviderKind::Baseten, "zai-org/GLM-5.3"),
        Some(0)
    );
    assert_eq!(
        catalogue::suggested_model_rank(ProviderKind::Ollama, "qwen3-coder:latest"),
        Some(1)
    );
    assert_eq!(
        catalogue::suggested_model_rank(ProviderKind::OpenAi, "unranked-model"),
        None
    );
    assert_eq!(
        catalogue::suggested_model_rank(ProviderKind::OpenRouter, "openai/gpt-6-luna"),
        Some(0)
    );
    assert_eq!(
        catalogue::suggested_model_rank(ProviderKind::OpenAi, "openai/gpt-6-luna"),
        None
    );
    assert_eq!(
        catalogue::suggested_model_rank(ProviderKind::Anthropic, "unranked-model"),
        None
    );
}

#[test]
fn catalogue_discovery_parses_provider_limits_and_scopes_headers() -> Result<(), Box<dyn Error>> {
    let client = reqwest::Client::builder().no_proxy().build()?;
    for (kind, body, context_tokens, output_tokens) in [
        (
            ProviderKind::Anthropic,
            json!({"data":[{"id":"fixture-model","max_input_tokens":200_000,"max_tokens":64_000,
                "context_length":7,"top_provider":{"max_completion_tokens":8}}]}),
            200_000,
            Some(64_000),
        ),
        (
            ProviderKind::OpenRouter,
            json!({"data":[{"id":"fixture-model","context_length":400_000,
                "top_provider":{"max_completion_tokens":32_000},"max_input_tokens":7,"max_tokens":8}]}),
            400_000,
            Some(32_000),
        ),
        (
            ProviderKind::ChatGpt,
            json!({"models":[
                {"slug":"fixture-model","display_name":"Fixture","visibility":"list","context_window":272_000},
                {"slug":"hidden","display_name":"Hidden","visibility":"hidden","context_window":1}]}),
            272_000,
            None,
        ),
    ] {
        let models = catalogue::parse_models(kind, &body)?;
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "fixture-model");
        assert_eq!(models[0].limits.context_tokens, Some(context_tokens));
        assert_eq!(models[0].limits.output_tokens, output_tokens);

        let url = catalogue::discovery_url(kind).ok_or("missing catalogue route")?;
        let key = (kind != ProviderKind::OpenRouter).then(|| "catalogue-private-key".into());
        let request = catalogue::catalogue_request(&client, url, key).build()?;
        assert_eq!(request.url().as_str(), url);
        let headers = request.headers();
        match kind {
            ProviderKind::Anthropic => {
                assert_eq!(headers["authorization"], "Bearer catalogue-private-key");
                assert_eq!(headers["anthropic-version"], "2026-10-21");
                assert!(!headers.contains_key("x-api-key"));
                assert_eq!(request.url().query(), Some("limit=1000"));
            }
            ProviderKind::OpenRouter => {
                assert!(!headers.contains_key("authorization"));
                assert!(!headers.contains_key("x-api-key"));
                assert!(!headers.contains_key("anthropic-version"));
                assert_eq!(
                    request.url().query(),
                    Some(
                        "output_modalities=text&endpoints=%2Fapi%2Fv1%2Fresponses&supported_parameters=tools"
                    )
                );
            }
            ProviderKind::ChatGpt => {
                assert_eq!(headers["authorization"], "Bearer catalogue-private-key");
                assert!(!headers.contains_key("x-api-key"));
                assert!(!headers.contains_key("anthropic-version"));
                assert_eq!(models[0].display_name.as_deref(), Some("Fixture"));
            }
            _ => return Err("unexpected provider".into()),
        }
    }
    Ok(())
}

#[test]
fn catalogue_discovery_rejects_invalid_limits_and_unrelated_provider_fields()
-> Result<(), Box<dyn Error>> {
    for invalid in [
        Value::Null,
        json!(0),
        json!(-1),
        json!(1.5),
        json!("32000"),
        json!(u64::from(u32::MAX) + 1),
    ] {
        for kind in [
            ProviderKind::Anthropic,
            ProviderKind::OpenRouter,
            ProviderKind::ChatGpt,
        ] {
            let body = if kind == ProviderKind::ChatGpt {
                json!({"models":[{"slug":"fixture-model","display_name":"Fixture","visibility":"list","context_window":invalid}]})
            } else {
                json!({"data":[{"id":"fixture-model","max_input_tokens":invalid,"max_tokens":invalid,
                    "context_length":invalid,"top_provider":{"max_completion_tokens":invalid}}]})
            };
            let models = catalogue::parse_models(kind, &body)?;
            assert_eq!(models.len(), 1);
            assert_eq!(
                models[0].limits,
                ModelLimits::default(),
                "{kind}: {invalid}"
            );
        }
    }
    let body = json!({"data":[{"id":"fixture-model","max_input_tokens":200_000,"max_tokens":64_000,
        "context_length":400_000,"top_provider":{"max_completion_tokens":32_000}}]});
    for kind in ProviderKind::ALL {
        if matches!(
            kind,
            ProviderKind::Anthropic | ProviderKind::OpenRouter | ProviderKind::ChatGpt
        ) {
            continue;
        }
        let models = catalogue::parse_models(kind, &body)?;
        assert_eq!(models[0].limits, ModelLimits::default(), "{kind}");
    }
    Ok(())
}

#[test]
fn catalogue_limits_prefer_alias_then_provider_then_exact_models_dev() -> Result<(), Box<dyn Error>>
{
    let mut explicit = catalogue_alias(ProviderKind::OpenRouter, CredentialSource::Saved);
    explicit.model_limits = ModelLimits {
        context_tokens: Some(123),
        output_tokens: Some(456),
    };
    let plain = catalogue_alias(ProviderKind::OpenRouter, CredentialSource::Saved);
    let mut partial = plain.clone();
    partial.model_limits.output_tokens = Some(789);
    let mut mismatched = plain.clone();
    mismatched.model = "mismatched".into();
    let mut other_provider = catalogue_alias(ProviderKind::OpenAi, CredentialSource::Saved);
    other_provider.model = "router-only".into();
    let mut aliases = BTreeMap::from([
        ("explicit".into(), vec![explicit]),
        ("plain".into(), vec![plain]),
        ("partial".into(), vec![partial]),
        ("mismatched".into(), vec![mismatched]),
        ("other-provider".into(), vec![other_provider]),
    ]);
    let metadata = json!({"openrouter":{"models":{
        "fixture-model":{"id":"fixture-model","limit":{"context":999_999,"output":8192}},
        "mismatched":{"id":"not-mismatched","limit":{"context":1,"output":2}},
        "router-only":{"id":"router-only","limit":{"context":3,"output":4}}
    }}});
    for mut group in LimitDiscovery::prepare(&aliases) {
        if group.provider.kind == ProviderKind::OpenRouter {
            group
                .catalogue
                .models
                .iter_mut()
                .find(|model| model.id == "fixture-model")
                .ok_or("missing model")?
                .limits
                .context_tokens = Some(200_000);
        }
        group.catalogue.enrich(&metadata);
        group.apply(&mut aliases);
    }
    assert_eq!(
        aliases["explicit"][0].model_limits,
        ModelLimits {
            context_tokens: Some(123),
            output_tokens: Some(456)
        }
    );
    assert_eq!(
        aliases["plain"][0].model_limits,
        ModelLimits {
            context_tokens: Some(200_000),
            output_tokens: Some(8192)
        }
    );
    assert_eq!(
        aliases["partial"][0].model_limits,
        ModelLimits {
            context_tokens: Some(200_000),
            output_tokens: Some(789)
        }
    );
    assert_eq!(
        aliases["mismatched"][0].model_limits,
        ModelLimits::default()
    );
    assert_eq!(
        aliases["other-provider"][0].model_limits,
        ModelLimits::default()
    );
    Ok(())
}

#[test]
fn catalogue_limits_do_not_reuse_alias_overrides_as_fallbacks() {
    let mut context = catalogue_alias(ProviderKind::OpenAi, CredentialSource::Saved);
    context.model_limits.context_tokens = Some(123);
    let mut output = catalogue_alias(ProviderKind::OpenAi, CredentialSource::Saved);
    output.model_limits.output_tokens = Some(456);
    let mut aliases = BTreeMap::from([("choices".into(), vec![context, output])]);
    let groups = LimitDiscovery::prepare(&aliases);
    assert_eq!(groups.len(), 1);
    for group in groups {
        group.apply(&mut aliases);
    }
    assert_eq!(
        aliases["choices"][0].model_limits,
        ModelLimits {
            context_tokens: Some(123),
            output_tokens: None
        }
    );
    assert_eq!(
        aliases["choices"][1].model_limits,
        ModelLimits {
            context_tokens: None,
            output_tokens: Some(456)
        }
    );
}

#[test]
fn catalogue_limits_skip_pinned_chatgpt_but_discover_legacy_missing_context()
-> Result<(), Box<dyn Error>> {
    let mut pinned = catalogue_alias(
        ProviderKind::ChatGpt,
        CredentialSource::Subscription {
            registration: Some("pinned-account".into()),
        },
    );
    pinned.model_limits.context_tokens = Some(123_456);
    let mut complete = catalogue_alias(ProviderKind::OpenAi, CredentialSource::Saved);
    complete.model_limits = ModelLimits {
        context_tokens: Some(123),
        output_tokens: Some(456),
    };
    let mut custom = catalogue_alias(ProviderKind::OpenAi, CredentialSource::Saved);
    custom.endpoint = "http://127.0.0.1:1/custom".into();
    let local = catalogue_alias(ProviderKind::Ollama, CredentialSource::None);
    let mut aliases = BTreeMap::from([
        ("pinned".into(), vec![pinned]),
        ("complete".into(), vec![complete]),
        ("custom".into(), vec![custom]),
        ("local".into(), vec![local]),
    ]);
    assert!(LimitDiscovery::prepare(&aliases).is_empty());
    aliases.get_mut("pinned").ok_or("missing alias")?[0]
        .model_limits
        .context_tokens = None;
    let groups = LimitDiscovery::prepare(&aliases);
    assert_eq!(groups.len(), 1);
    for mut group in groups {
        assert_eq!(group.provider.kind, ProviderKind::ChatGpt);
        group.catalogue.models = catalogue::parse_models(
            ProviderKind::ChatGpt,
            &json!({"models":[{"slug":"fixture-model","display_name":"Fixture","visibility":"list","context_window":272_000}]}),
        )?;
        group.apply(&mut aliases);
    }
    assert_eq!(
        aliases["pinned"][0].model_limits.context_tokens,
        Some(272_000)
    );
    assert_eq!(aliases["pinned"][0].model_limits.output_tokens, None);
    assert!(LimitDiscovery::prepare(&aliases).is_empty());
    Ok(())
}

#[test]
fn catalogue_limits_isolate_accounts_credential_sources_and_captured_keys()
-> Result<(), Box<dyn Error>> {
    let subscriptions = ["first", "second", "unavailable", "first"].map(|registration| {
        catalogue_alias(
            ProviderKind::ChatGpt,
            CredentialSource::Subscription {
                registration: Some(registration.into()),
            },
        )
    });
    let environment = |reference: &str| {
        catalogue_alias(
            ProviderKind::Anthropic,
            CredentialSource::Environment {
                reference: reference.into(),
            },
        )
    };
    let mut captured_first = catalogue_alias(ProviderKind::Anthropic, CredentialSource::Saved);
    captured_first.api_key = Some("captured-first".into());
    let mut captured_second = catalogue_alias(ProviderKind::Anthropic, CredentialSource::Saved);
    captured_second.api_key = Some("captured-second".into());
    let mut aliases = BTreeMap::from([
        ("subscriptions".into(), subscriptions.into()),
        (
            "api-keys".into(),
            vec![
                environment("FIRST_KEY"),
                environment("SECOND_KEY"),
                catalogue_alias(ProviderKind::Anthropic, CredentialSource::Saved),
                captured_first,
                captured_second,
            ],
        ),
    ]);
    let groups = LimitDiscovery::prepare(&aliases);
    assert_eq!(groups.len(), 8);
    for mut group in groups {
        let context = match &group.provider.credential_source {
            CredentialSource::Subscription {
                registration: Some(registration),
            } => match registration.as_str() {
                "first" => 100_000,
                "second" => 200_000,
                _ => continue,
            },
            CredentialSource::Environment { reference } if reference == "FIRST_KEY" => 300_000,
            CredentialSource::Environment { .. } => 400_000,
            CredentialSource::Saved => match group
                .provider
                .api_key
                .as_ref()
                .map(secrecy::ExposeSecret::expose_secret)
            {
                None => 500_000,
                Some("captured-first") => 600_000,
                Some(_) => 700_000,
            },
            _ => return Err("unexpected credential source".into()),
        };
        let body = if group.provider.kind == ProviderKind::ChatGpt {
            json!({"models":[{"slug":"fixture-model","display_name":"Fixture","visibility":"list","context_window":context}]})
        } else {
            json!({"data":[{"id":"fixture-model","max_input_tokens":context,"max_tokens":8192}]})
        };
        group.catalogue.models = catalogue::parse_models(group.provider.kind, &body)?;
        group.apply(&mut aliases);
    }
    for (index, context) in [Some(100_000), Some(200_000), None, Some(100_000)]
        .into_iter()
        .enumerate()
    {
        assert_eq!(
            aliases["subscriptions"][index].model_limits.context_tokens,
            context
        );
        assert_eq!(
            aliases["subscriptions"][index].model_limits.output_tokens,
            None
        );
    }
    for (index, context) in [300_000, 400_000, 500_000, 600_000, 700_000]
        .into_iter()
        .enumerate()
    {
        assert_eq!(
            aliases["api-keys"][index].model_limits.context_tokens,
            Some(context)
        );
        assert_eq!(
            aliases["api-keys"][index].model_limits.output_tokens,
            Some(8192)
        );
    }
    Ok(())
}

fn catalogue_alias(kind: ProviderKind, credential_source: CredentialSource) -> ResolvedAlias {
    ResolvedAlias {
        kind,
        endpoint: kind.endpoint_default().into(),
        model: "fixture-model".into(),
        model_limits: ModelLimits::default(),
        api_key: None,
        thinking_level: None,
        reasoning_summary: false,
        credential_source,
    }
}
