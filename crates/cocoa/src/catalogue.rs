//! Bounded, credential-isolated model discovery for known catalogue routes.

use std::{collections::BTreeMap, time::Duration};

use cocoa_llm::{ModelLimits, ProviderKind, ThinkingLevel};
use futures_util::{StreamExt, stream};
use secrecy::{ExposeSecret, SecretString};
use serde_json::Value;

/// Provider profiles in discovery order, also bounding concurrent account discovery.
pub const PROVIDERS: [ProviderKind; ProviderKind::ALL.len()] = [
    ProviderKind::ChatGpt,
    ProviderKind::OpenAi,
    ProviderKind::Anthropic,
    ProviderKind::DeepSeek,
    ProviderKind::OpenRouter,
    ProviderKind::Ollama,
    ProviderKind::HuggingFace,
    ProviderKind::QwenTokenPlan,
    ProviderKind::XAi,
    ProviderKind::Groq,
    ProviderKind::Nvidia,
    ProviderKind::Together,
    ProviderKind::Fireworks,
    ProviderKind::MoonshotAi,
    ProviderKind::KimiCode,
    ProviderKind::Zai,
    ProviderKind::Meta,
    ProviderKind::VercelAiGateway,
    ProviderKind::Cerebras,
    ProviderKind::OpenCodeZen,
    ProviderKind::OpenCodeGo,
    ProviderKind::Baseten,
    ProviderKind::MoonshotAiCn,
    ProviderKind::ZaiCodingCn,
    ProviderKind::QwenTokenPlanCn,
    ProviderKind::QwenTokenPlanIndividual,
    ProviderKind::QwenTokenPlanIndividualCn,
    ProviderKind::Xiaomi,
    ProviderKind::AntLing,
    ProviderKind::XiaomiTokenPlanCn,
    ProviderKind::XiaomiTokenPlanAms,
    ProviderKind::XiaomiTokenPlanSgp,
];

/// One advertised or configured model and its available provider metadata.
#[derive(Clone)]
pub struct Model {
    pub provider: ProviderKind,
    pub id: String,
    pub display_name: Option<String>,
    pub efforts: Option<Efforts>,
    pub none_allowed: bool,
    pub limits: ModelLimits,
}

/// Explicit provider effort choices take precedence over supplementary metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Efforts {
    Provider(Vec<ThinkingLevel>),
    Unrestricted,
    ModelsDev(Vec<ThinkingLevel>),
}

/// Discovered models, configured provider profiles, and unavailable catalogues.
pub struct Catalogue {
    pub models: Vec<Model>,
    /// Configured profiles with any required credentials resolved,
    /// even when model discovery fails.
    pub connected: Vec<ProviderKind>,
    pub unavailable: Vec<ProviderKind>,
}

/// Sanitized discovery failures, without credential or response-body details.
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error("Provider credential rejected. Reconnect or choose another provider.")]
    Rejected,
    #[error("Model discovery unavailable. Retry or enter a known tool-capable model ID.")]
    Unavailable,
}

impl Efforts {
    /// Whether this catalogue permits the requested thinking level.
    pub fn supports(&self, level: ThinkingLevel) -> bool {
        match self {
            Self::Provider(levels) | Self::ModelsDev(levels) => levels.contains(&level),
            Self::Unrestricted => true,
        }
    }
}

/// Curated preference within a provider, or absence for an unranked model.
/// Uses the same model identifiers as the onboarding suggestions, not usage telemetry.
#[must_use]
pub fn suggested_model_rank(provider: ProviderKind, id: &str) -> Option<usize> {
    suggested_model_groups(provider)
        .into_iter()
        .flatten()
        .position(|candidate| *candidate == id)
}

impl Catalogue {
    /// Suggest known coding models that the selected catalogue actually advertised.
    /// Missing model groups reuse another known group; unknown models require a choice.
    pub fn suggested_aliases(&self, provider: ProviderKind) -> Vec<crate::config::AliasSuggestion> {
        let groups = suggested_model_groups(provider);
        let choices = groups.map(|ids| {
            ids.iter().find_map(|id| {
                self.models
                    .iter()
                    .find(|model| model.provider == provider && model.id == *id)
            })
        });
        let Some(fallback) = choices.iter().flatten().next().copied() else {
            return Vec::new();
        };
        let levels = [
            ThinkingLevel::Low,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
            ThinkingLevel::XHigh,
            ThinkingLevel::Max,
        ];
        ["x-low", "low", "medium", "high", "x-high", "ultra"]
            .into_iter()
            .enumerate()
            .map(|(index, name)| {
                let model = choices[index / 2].unwrap_or(fallback);
                let requested = levels[index];
                let supported = |level| {
                    provider != ProviderKind::Ollama
                        && (provider != ProviderKind::Baseten
                            || matches!(
                                level,
                                ThinkingLevel::Minimal
                                    | ThinkingLevel::Low
                                    | ThinkingLevel::Medium
                                    | ThinkingLevel::High
                            ))
                        && model
                            .efforts
                            .as_ref()
                            .is_some_and(|efforts| efforts.supports(level))
                };
                let thinking_level = if supported(requested) {
                    Some(requested)
                } else if index >= 4 && supported(ThinkingLevel::High) {
                    Some(ThinkingLevel::High)
                } else {
                    None
                };
                crate::config::AliasSuggestion {
                    name: name.into(),
                    model: model.id.clone(),
                    thinking_level,
                }
            })
            .collect()
    }

    /// Fill token limits and missing efforts for exact provider/model matches only.
    /// Provider allowlists, including empty and unrestricted lists, take precedence.
    pub fn enrich(&mut self, metadata: &Value) {
        for model in &mut self.models {
            if matches!(model.provider, ProviderKind::Ollama | ProviderKind::ChatGpt) {
                continue;
            }
            let Some(entry) = metadata
                .get(model.provider.to_string())
                .and_then(|provider| provider.get("models"))
                .and_then(|models| models.get(&model.id))
                .filter(|entry| entry.get("id").and_then(Value::as_str) == Some(model.id.as_str()))
            else {
                continue;
            };
            model.limits.output_tokens = model
                .limits
                .output_tokens
                .or_else(|| token_limit(&entry["limit"]["output"]));
            model.limits.context_tokens = model
                .limits
                .context_tokens
                .or_else(|| token_limit(&entry["limit"]["context"]));
            if model.efforts.is_some() {
                continue;
            }
            if entry.get("reasoning") == Some(&Value::Bool(false)) {
                model.efforts = Some(Efforts::ModelsDev(Vec::new()));
                continue;
            }
            if entry.get("reasoning") != Some(&Value::Bool(true)) {
                continue;
            }
            if let Some(values) = entry
                .get("reasoning_options")
                .and_then(Value::as_array)
                .and_then(|options| {
                    options
                        .iter()
                        .find(|option| option.get("type").and_then(Value::as_str) == Some("effort"))
                })
                .and_then(|option| option.get("values"))
                .and_then(Value::as_array)
            {
                model.efforts = Some(Efforts::ModelsDev(
                    values
                        .iter()
                        .filter_map(|value| serde_json::from_value(value.clone()).ok())
                        .collect(),
                ));
            }
        }
    }
}

/// Fill missing limits for default hosted endpoints without sharing account metadata.
/// ChatGPT needs only a context window; a pinned context never triggers discovery.
/// Discovery failures leave limits absent for the host's fallback policy.
pub async fn resolve_limits(aliases: &mut BTreeMap<String, Vec<crate::config::ResolvedAlias>>) {
    let mut groups = LimitDiscovery::prepare(aliases);
    let mut jobs = stream::iter(&mut groups)
        .map(|group| async move {
                match discover_provider_checked(group.provider.clone()).await {
                    Ok(models) => {
                        for existing in &mut group.catalogue.models {
                            if let Some(model) = models.iter().find(|model| model.id == existing.id) {
                                *existing = model.clone();
                            }
                        }
                    }
                    Err(error) => {
                        tracing::debug!(kind = %group.provider.kind, %error, "provider token metadata unavailable");
                    }
                }
        })
        .buffer_unordered(PROVIDERS.len());
    while jobs.next().await.is_some() {}
    drop(jobs);
    if groups
        .iter()
        .flat_map(|group| &group.catalogue.models)
        .any(|model| {
            !matches!(model.provider, ProviderKind::ChatGpt | ProviderKind::Ollama)
                && (model.limits.context_tokens.is_none() || model.limits.output_tokens.is_none())
        })
    {
        match fetch("https://models.dev/api.json", None, 16 * 1024 * 1024).await {
            Some(metadata) => {
                for group in &mut groups {
                    group.catalogue.enrich(&metadata);
                }
            }
            None => tracing::warn!("model token metadata unavailable; using host fallback"),
        }
    }
    for group in groups {
        group.apply(aliases);
    }
}

/// Missing model limits for one provider endpoint and captured credential source.
/// The caller fills the catalogue before applying it to matching aliases.
pub struct LimitDiscovery {
    pub provider: crate::config::ResolvedAlias,
    pub catalogue: Catalogue,
}

impl LimitDiscovery {
    /// Group aliases needing hosted discovery without resolving credentials or doing I/O.
    /// Pinned ChatGPT windows, complete limits and custom/local endpoints need no job.
    /// Alias overrides never seed another alias's catalogue defaults.
    pub fn prepare(aliases: &BTreeMap<String, Vec<crate::config::ResolvedAlias>>) -> Vec<Self> {
        let mut groups: Vec<Self> = Vec::new();
        for alias in aliases.values().flatten().filter(|alias| {
            alias.endpoint == alias.kind.endpoint_default()
                && alias.kind != ProviderKind::Ollama
                && (alias.model_limits.context_tokens.is_none()
                    || (alias.kind != ProviderKind::ChatGpt
                        && alias.model_limits.output_tokens.is_none()))
        }) {
            let index = match groups.iter().position(|group| group.matches(alias)) {
                Some(index) => index,
                None => {
                    groups.push(Self {
                        provider: alias.clone(),
                        catalogue: Catalogue {
                            connected: vec![alias.kind],
                            models: Vec::new(),
                            unavailable: Vec::new(),
                        },
                    });
                    groups.len() - 1
                }
            };
            let catalogue = &mut groups[index].catalogue;
            if !catalogue.models.iter().any(|model| model.id == alias.model) {
                catalogue.models.push(Model {
                    provider: alias.kind,
                    id: alias.model.clone(),
                    display_name: None,
                    efforts: None,
                    none_allowed: true,
                    limits: ModelLimits::default(),
                });
            }
        }
        groups
    }

    /// Fill missing limits for exact model/account matches, preserving every override.
    pub fn apply(&self, aliases: &mut BTreeMap<String, Vec<crate::config::ResolvedAlias>>) {
        for alias in aliases
            .values_mut()
            .flatten()
            .filter(|alias| self.matches(alias))
        {
            if let Some(model) = self
                .catalogue
                .models
                .iter()
                .find(|model| model.id == alias.model)
            {
                alias.model_limits.output_tokens = alias
                    .model_limits
                    .output_tokens
                    .or(model.limits.output_tokens);
                alias.model_limits.context_tokens = alias
                    .model_limits
                    .context_tokens
                    .or(model.limits.context_tokens);
            }
        }
    }

    fn matches(&self, alias: &crate::config::ResolvedAlias) -> bool {
        self.provider.kind == alias.kind
            && self.provider.endpoint == alias.endpoint
            && self.provider.credential_source == alias.credential_source
            && self
                .provider
                .api_key
                .as_ref()
                .map(ExposeSecret::expose_secret)
                == alias.api_key.as_ref().map(ExposeSecret::expose_secret)
    }
}

/// Discover configured providers without running credential commands on the async thread.
/// Resolve one provider configuration at a time and admit at most `PROVIDERS.len()`
/// discovery jobs. Credential-command and HTTP deadlines remain independent.
pub async fn discover() -> Catalogue {
    let mut jobs = tokio::task::JoinSet::new();
    let mut catalogue = Catalogue {
        connected: Vec::new(),
        models: Vec::new(),
        unavailable: Vec::new(),
    };
    let aliases = match crate::config::aliases() {
        Ok(aliases) => aliases,
        Err(error) => {
            tracing::warn!(%error, "cannot read saved catalogue models");
            Default::default()
        }
    };
    let mut saved_models = Vec::new();
    for kind in PROVIDERS {
        let provider = match tokio::task::spawn_blocking(move || {
            crate::config::catalogue_provider(kind)?
                .map(|mut provider| {
                    provider.api_key = provider.resolve_api_key()?;
                    Ok::<_, crate::config::ConfigError>(provider)
                })
                .transpose()
        })
        .await
        {
            Ok(provider) => provider,
            Err(error) => {
                tracing::warn!(%kind, %error, "model catalogue configuration worker failed");
                catalogue.unavailable.push(kind);
                continue;
            }
        };
        match provider {
            Ok(Some(provider)) => {
                catalogue.connected.push(kind);
                if provider.endpoint == kind.endpoint_default() && kind != ProviderKind::ChatGpt {
                    for alias in aliases
                        .values()
                        .flatten()
                        .filter(|alias| alias.provider == kind.to_string())
                    {
                        saved_models.push(Model {
                            provider: kind,
                            id: alias.model.clone(),
                            display_name: None,
                            efforts: None,
                            none_allowed: true,
                            limits: ModelLimits::default(),
                        });
                    }
                }
                jobs.spawn(async move { (kind, discover_provider(provider).await) });
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(%kind, %error, "cannot resolve model catalogue configuration");
                catalogue.unavailable.push(kind);
            }
        }
    }
    while let Some(result) = jobs.join_next().await {
        match result {
            Ok((_, Some(models))) => catalogue.models.extend(models),
            Ok((provider, None)) => catalogue.unavailable.push(provider),
            Err(error) => tracing::warn!(%error, "model discovery worker failed"),
        }
    }
    for model in saved_models {
        if !catalogue
            .models
            .iter()
            .any(|existing| existing.provider == model.provider && existing.id == model.id)
        {
            catalogue.models.push(model);
        }
    }
    if catalogue.models.iter().any(|model| {
        (model.efforts.is_none()
            || model.limits.context_tokens.is_none()
            || model.limits.output_tokens.is_none())
            && !matches!(model.provider, ProviderKind::Ollama | ProviderKind::ChatGpt)
    }) {
        match fetch("https://models.dev/api.json", None, 16 * 1024 * 1024).await {
            Some(metadata) => catalogue.enrich(&metadata),
            None => tracing::warn!("models.dev effort metadata unavailable"),
        }
    }
    catalogue.models.sort_by(|left, right| {
        if left.provider == ProviderKind::ChatGpt && right.provider == ProviderKind::ChatGpt {
            return std::cmp::Ordering::Equal;
        }
        let group = (left.provider == ProviderKind::ChatGpt)
            .cmp(&(right.provider == ProviderKind::ChatGpt));
        if group != std::cmp::Ordering::Equal {
            return group;
        }
        left.id
            .cmp(&right.id)
            .then_with(|| left.provider.to_string().cmp(&right.provider.to_string()))
    });
    catalogue
}

/// Discover one explicitly selected provider without a generation request.
/// Custom inference endpoints never establish permission to guess a catalogue route.
/// Go's public catalogue does not validate a key; inference is never claimed verified.
/// Credential commands run on blocking workers under their existing timeout.
///
/// # Errors
/// Return `Rejected` for credential resolution or authorization failures and
/// `Unavailable` for unsupported routes, worker failures or unavailable catalogues.
pub async fn connect(provider: crate::config::ResolvedAlias) -> Result<Catalogue, ConnectError> {
    if provider.endpoint != provider.kind.endpoint_default() {
        return Err(ConnectError::Unavailable);
    }
    if provider.kind == ProviderKind::OpenRouter {
        let key = resolve_api_key(provider.clone()).await?;
        let info = fetch_checked("https://openrouter.ai/api/v1/key", key, 65_536).await?;
        if !info.get("data").is_some_and(Value::is_object) {
            return Err(ConnectError::Unavailable);
        }
    }
    let kind = provider.kind;
    let models = discover_provider_checked(provider).await?;
    let mut catalogue = Catalogue {
        connected: vec![kind],
        models,
        unavailable: Vec::new(),
    };
    if !matches!(kind, ProviderKind::Ollama | ProviderKind::ChatGpt)
        && catalogue.models.iter().any(|model| {
            model.efforts.is_none()
                || model.limits.context_tokens.is_none()
                || model.limits.output_tokens.is_none()
        })
        && let Some(metadata) = fetch("https://models.dev/api.json", None, 16 * 1024 * 1024).await
    {
        catalogue.enrich(&metadata);
    }
    Ok(catalogue)
}

/// Known provider catalogue routes. Custom inference endpoints do not use these routes.
pub fn discovery_url(kind: ProviderKind) -> Option<&'static str> {
    Some(match kind {
        ProviderKind::OpenAi | ProviderKind::ChatGpt => "https://api.openai.com/v1/models",
        ProviderKind::OpenCodeGo => "https://opencode.ai/inference/go/v1/models",
        ProviderKind::Baseten => "https://inference.baseten.co/v1/models",
        ProviderKind::Ollama => "http://127.0.0.1:11434/v1/models",
        ProviderKind::Anthropic => "https://api.anthropic.com/v1/models?limit=1000",
        ProviderKind::OpenRouter => {
            "https://openrouter.ai/api/v1/models?output_modalities=text&endpoints=%2Fapi%2Fv1%2Fresponses&supported_parameters=tools"
        }
        _ => return None,
    })
}

/// Parse the selected provider's catalogue schema without guessing other providers' fields.
///
/// # Errors
/// Return `Unavailable` for malformed catalogue envelopes or oversized model lists.
pub fn parse_models(kind: ProviderKind, value: &Value) -> Result<Vec<Model>, ConnectError> {
    if kind == ProviderKind::ChatGpt {
        return parse_chatgpt(value);
    }
    let data = value.get("data").ok_or(ConnectError::Unavailable)?;
    if data.is_null() {
        return Ok(Vec::new());
    }
    let entries = data.as_array().ok_or(ConnectError::Unavailable)?;
    if entries.len() > 8192 {
        return Err(ConnectError::Unavailable);
    }
    let mut models = Vec::with_capacity(entries.len().min(1024));
    for entry in entries {
        if models.len() == 1024 {
            break;
        }
        if entry.pointer("/capabilities/tools/supported") == Some(&Value::Bool(false)) {
            continue;
        }
        let Some(id) = entry.get("id").and_then(Value::as_str) else {
            continue;
        };
        if id.is_empty() || id.len() > 256 || id.chars().any(char::is_control) {
            continue;
        }
        let efforts_path = if kind == ProviderKind::OpenCodeGo {
            "/capabilities/reasoning/effort"
        } else {
            "/reasoning/supported_efforts"
        };
        let efforts =
            if entry.pointer("/capabilities/reasoning/supported") == Some(&Value::Bool(false)) {
                Some(Efforts::Provider(Vec::new()))
            } else {
                match entry.pointer(efforts_path) {
                    Some(Value::Array(values)) => Some(Efforts::Provider(
                        values
                            .iter()
                            .filter_map(|value| {
                                serde_json::from_value::<ThinkingLevel>(value.clone()).ok()
                            })
                            .collect(),
                    )),
                    Some(Value::Null) if kind == ProviderKind::OpenRouter => {
                        Some(Efforts::Unrestricted)
                    }
                    _ => None,
                }
            };
        models.push(Model {
            provider: kind,
            id: id.into(),
            display_name: None,
            efforts,
            none_allowed: entry.pointer("/reasoning/mandatory") != Some(&Value::Bool(true)),
            limits: match kind {
                ProviderKind::Anthropic => ModelLimits {
                    output_tokens: token_limit(&entry["max_tokens"]),
                    context_tokens: token_limit(&entry["max_input_tokens"]),
                },
                ProviderKind::OpenRouter => ModelLimits {
                    output_tokens: token_limit(&entry["top_provider"]["max_completion_tokens"]),
                    context_tokens: token_limit(&entry["context_length"]),
                },
                _ => ModelLimits::default(),
            },
        });
    }
    Ok(models)
}

/// Build a catalogue GET using the selected route's authentication scheme.
/// Public catalogues pass no key; Anthropic also requires its API version.
/// The caller must trust `url`; this function does not validate provider endpoints.
pub fn catalogue_request(
    client: &reqwest::Client,
    url: &str,
    key: Option<SecretString>,
) -> reqwest::RequestBuilder {
    let mut request = client.get(url);
    if url.starts_with("https://api.anthropic.com/") {
        request = request.header("anthropic-version", "2026-10-21");
    }
    if let Some(key) = key {
        request = request.bearer_auth(key.expose_secret());
    }
    request
}

async fn discover_provider(provider: crate::config::ResolvedAlias) -> Option<Vec<Model>> {
    discover_provider_checked(provider).await.ok()
}

async fn discover_provider_checked(
    provider: crate::config::ResolvedAlias,
) -> Result<Vec<Model>, ConnectError> {
    let kind = provider.kind;
    // Custom inference paths do not establish permission to send keys to a guessed route.
    if provider.endpoint != kind.endpoint_default() {
        return Err(ConnectError::Unavailable);
    }
    let url = discovery_url(kind).ok_or(ConnectError::Unavailable)?;
    let key = if kind == ProviderKind::ChatGpt {
        let source = provider
            .subscription_credentials()
            .map_err(|_| ConnectError::Rejected)?
            .ok_or(ConnectError::Rejected)?;
        Some(source.resolve().await.map_err(|_| ConnectError::Rejected)?)
    } else if matches!(
        kind,
        ProviderKind::OpenAi | ProviderKind::Baseten | ProviderKind::Anthropic
    ) {
        Some(
            resolve_api_key(provider)
                .await?
                .ok_or(ConnectError::Rejected)?,
        )
    } else {
        None
    };
    let value = fetch_checked(url, key, 4 * 1024 * 1024).await?;
    parse_models(kind, &value)
}

fn suggested_model_groups(provider: ProviderKind) -> [&'static [&'static str]; 3] {
    match provider {
        ProviderKind::OpenAi | ProviderKind::ChatGpt => [
            &["gpt-6-luna", "gpt-5-mini", "gpt-5-nano"],
            &["gpt-6.1-sol", "gpt-6-sol", "gpt-5"],
            &["gpt-6-astra", "gpt-6.1-sol", "gpt-5"],
        ],
        ProviderKind::OpenCodeGo => [
            &["glm-5.3-flash", "glm-4.7-flash"],
            &["kimi-k2.7-code", "glm-5.3", "minimax-m3"],
            &["kimi-k3", "glm-5.3", "kimi-k2.7-code"],
        ],
        ProviderKind::Baseten => [
            &["zai-org/GLM-5.3"],
            &["zai-org/GLM-5.3"],
            &["zai-org/GLM-5.3"],
        ],
        ProviderKind::OpenRouter => [
            &["openai/gpt-6-luna", "openai/gpt-5-mini"],
            &["openai/gpt-6.1-sol", "openai/gpt-5"],
            &["openai/gpt-6-astra", "openai/gpt-6.1-sol", "openai/gpt-5"],
        ],
        ProviderKind::Ollama => [
            &[
                "qwen3-coder:30b",
                "qwen3-coder:latest",
                "llama3.1:8b",
                "llama3.1:latest",
            ],
            &[
                "qwen3-coder:30b",
                "qwen3-coder:latest",
                "llama3.1:8b",
                "llama3.1:latest",
            ],
            &[
                "qwen3-coder:30b",
                "qwen3-coder:latest",
                "llama3.1:8b",
                "llama3.1:latest",
            ],
        ],
        _ => [&[]; 3],
    }
}

fn parse_chatgpt(value: &Value) -> Result<Vec<Model>, ConnectError> {
    let entries = value
        .get("models")
        .and_then(Value::as_array)
        .filter(|entries| entries.len() <= 8192)
        .ok_or(ConnectError::Unavailable)?;
    let mut models = Vec::with_capacity(entries.len().min(1024));
    for entry in entries {
        if entry["visibility"] != "list" {
            continue;
        }
        let id = entry["slug"]
            .as_str()
            .filter(|text| {
                !text.trim().is_empty() && text.len() <= 256 && !text.chars().any(char::is_control)
            })
            .ok_or(ConnectError::Unavailable)?;
        let label = entry["display_name"]
            .as_str()
            .filter(|text| {
                !text.trim().is_empty() && text.len() <= 256 && !text.chars().any(char::is_control)
            })
            .ok_or(ConnectError::Unavailable)?;
        if models.len() == 1024 {
            break;
        }
        models.push(Model {
            provider: ProviderKind::ChatGpt,
            id: id.into(),
            display_name: Some(label.into()),
            efforts: None,
            none_allowed: true,
            limits: ModelLimits {
                output_tokens: None,
                context_tokens: token_limit(&entry["context_window"]),
            },
        });
    }
    Ok(models)
}

fn token_limit(value: &Value) -> Option<u32> {
    value
        .as_u64()
        .and_then(|tokens| u32::try_from(tokens).ok())
        .filter(|&tokens| tokens > 0)
}

async fn resolve_api_key(
    provider: crate::config::ResolvedAlias,
) -> Result<Option<SecretString>, ConnectError> {
    tokio::task::spawn_blocking(move || provider.resolve_api_key())
        .await
        .map_err(|_| ConnectError::Unavailable)?
        .map_err(|_| ConnectError::Rejected)
}

pub(crate) async fn fetch(url: &str, key: Option<SecretString>, bytes_max: usize) -> Option<Value> {
    fetch_checked(url, key, bytes_max).await.ok()
}

async fn fetch_checked(
    url: &str,
    key: Option<SecretString>,
    bytes_max: usize,
) -> Result<Value, ConnectError> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(4))
        .build()
        .map_err(|_| ConnectError::Unavailable)?;
    let mut response = catalogue_request(&client, url, key)
        .send()
        .await
        .map_err(|_| ConnectError::Unavailable)?;
    if matches!(response.status().as_u16(), 401 | 403) {
        return Err(ConnectError::Rejected);
    }
    response
        .error_for_status_ref()
        .map_err(|_| ConnectError::Unavailable)?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| ConnectError::Unavailable)?
    {
        if bytes.len().saturating_add(chunk.len()) > bytes_max {
            return Err(ConnectError::Unavailable);
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| ConnectError::Unavailable)
}
