//! Bounded startup configuration. The main config never stores credentials.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs::File,
    io::Read,
    path::PathBuf,
};

use clap::{Parser, Subcommand};
use cocoa_core::{
    compaction::{CheckpointLimits, CompactionPolicy},
    engine::{EngineSettings, Limits},
    store::StoreLimits,
};
use cocoa_llm::{DECODER_BYTES, ModelLimits, ProviderKind, ThinkingLevel};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Resolved startup data. Credentials deliberately lack serialization and Debug.
pub struct Configuration {
    pub alias: String,
    pub provider: ProviderKind,
    pub model: String,
    pub thinking_level: Option<ThinkingLevel>,
    pub reasoning_summary: bool,
    pub endpoint: String,
    pub api_key: Option<SecretString>,
    /// Credential variables for all configured model providers and MCP headers.
    pub credential_env_names: Vec<String>,
    pub engine: EngineSettings,
    pub store: StoreLimits,
    pub aliases: BTreeMap<String, ResolvedAlias>,
    /// Selected regular choice followed by fallback-only choices, for each alias.
    pub candidates: BTreeMap<String, Vec<ResolvedAlias>>,
    pub agents_active: usize,
    pub agents_retained: usize,
    pub message_steering: bool,
    pub web_search: crate::mcp::SearchProvider,
    pub mcp_servers: BTreeMap<String, crate::mcp::Server>,
}

/// Resolved provider settings and a captured credential source for one alias.
/// Configuration loading validates these fields; struct literals do not.
/// Only the selected alias has its key resolved at startup. Neither this type
/// nor its credential source implements Debug or serialization.
#[derive(Clone)]
pub struct ResolvedAlias {
    pub kind: ProviderKind,
    pub endpoint: String,
    pub model: String,
    pub model_limits: ModelLimits,
    pub api_key: Option<SecretString>,
    pub thinking_level: Option<ThinkingLevel>,
    pub reasoning_summary: bool,
    pub credential_source: CredentialSource,
}

/// Explicit environment selection, saved auth with provider-environment fallback, or OAuth.
#[derive(Clone, PartialEq, Eq)]
pub enum CredentialSource {
    None,
    Environment { reference: String },
    Saved,
    Subscription { registration: Option<String> },
}

impl Configuration {
    /// Capture nonsecret settings that must remain compatible across restarts.
    /// Unavailable child subscriptions retain no account and cannot start a child.
    /// Compaction is a current host request policy, not part of this persisted
    /// execution profile. Resuming adopts the current configuration's policy.
    ///
    /// # Errors
    /// Reject an unavailable root alias/account or limits outside stored integer bounds.
    pub fn effective_settings(
        &self,
    ) -> Result<crate::persistence::EffectiveSessionSettings, ConfigError> {
        use crate::persistence::{EffectiveSessionSettings, SessionExecutionLimits};
        let profile = ResolvedAlias::effective_settings;
        let root = profile(self.aliases.get(&self.alias).ok_or(ConfigError::Alias)?)?;
        if self.provider == ProviderKind::ChatGpt && root.account.is_none() {
            return Err(ConfigError::SessionCompatibility);
        }
        let limits = self.engine.limits();
        Ok(EffectiveSessionSettings {
            alias: self.alias.clone(),
            provider: root.provider,
            protocol: if self.provider == ProviderKind::Anthropic {
                "anthropic-messages-v1"
            } else if self.provider.is_chat_completions() {
                "chat-completions-v1"
            } else {
                "openai-responses-v1"
            }
            .into(),
            model: root.model,
            endpoint: root.endpoint,
            account: root.account,
            thinking_level: root.thinking_level,
            reasoning_summary: root.reasoning_summary,
            execution_version: 1,
            aliases: self
                .aliases
                .iter()
                .map(|(name, alias)| Ok((name.clone(), profile(alias)?)))
                .collect::<Result<_, ConfigError>>()?,
            limits: SessionExecutionLimits {
                input_bytes: limits
                    .input_bytes
                    .try_into()
                    .map_err(|_| ConfigError::Bounds)?,
                output_bytes: limits
                    .output_bytes
                    .try_into()
                    .map_err(|_| ConfigError::Bounds)?,
                record_bytes: limits
                    .record_bytes
                    .try_into()
                    .map_err(|_| ConfigError::Bounds)?,
                context_bytes: limits
                    .context_bytes
                    .try_into()
                    .map_err(|_| ConfigError::Bounds)?,
                agents_active: self
                    .agents_active
                    .try_into()
                    .map_err(|_| ConfigError::Bounds)?,
                agents_retained: self
                    .agents_retained
                    .try_into()
                    .map_err(|_| ConfigError::Bounds)?,
                pending_operations: limits
                    .pending_operations
                    .try_into()
                    .map_err(|_| ConfigError::Bounds)?,
                stall_timeout_ms: u64::try_from(limits.stall_timeout.as_millis())
                    .map_err(|_| ConfigError::Bounds)?,
                settlement_timeout_ms: u64::try_from(limits.settlement_timeout.as_millis())
                    .map_err(|_| ConfigError::Bounds)?,
                shutdown_timeout_ms: u64::try_from(limits.shutdown_timeout.as_millis())
                    .map_err(|_| ConfigError::Bounds)?,
                message_steering: self.message_steering,
            },
        })
    }

    fn select_candidate(&mut self) -> Result<(), ConfigError> {
        for (name, choices) in &self.candidates {
            self.aliases.insert(
                name.clone(),
                choices.first().ok_or(ConfigError::AliasChoices)?.clone(),
            );
        }
        let alias = self.aliases.get(&self.alias).ok_or(ConfigError::Alias)?;
        self.provider = alias.kind;
        self.model = alias.model.clone();
        self.thinking_level = alias.thinking_level;
        self.reasoning_summary = alias.reasoning_summary;
        self.endpoint = alias.endpoint.clone();
        self.api_key = alias.api_key.clone();
        Ok(())
    }
}

/// Resolve current credentials and reject changes to a saved execution profile.
///
/// # Errors
/// Reject missing credentials, unavailable pinned accounts or changed execution settings.
pub fn resume_configuration(
    saved: &crate::persistence::EffectiveSessionSettings,
) -> Result<Configuration, ConfigError> {
    let mut configuration = if saved.alias.contains('/') && !aliases()?.contains_key(&saved.alias) {
        let thinking = saved
            .thinking_level
            .as_deref()
            .map(parse_thinking)
            .transpose()?
            .and_then(|choice| choice.0);
        load_selection(None, Some(saved.alias.clone()), thinking, None)?
    } else {
        let entries = aliases()?;
        let choice = entries.get(&saved.alias).and_then(|entries| {
            entries.iter().position(|entry| {
                !entry.fallback_only
                    && entry.provider == saved.provider
                    && entry.model == saved.model
                    && entry
                        .thinking_level
                        .and_then(|level| serde_json::to_value(level).ok())
                        .as_ref()
                        .and_then(serde_json::Value::as_str)
                        == saved.thinking_level.as_deref()
            })
        });
        load_selection(Some(saved.alias.clone()), None, None, choice)?
    };
    let root_choices = configuration
        .candidates
        .get_mut(&saved.alias)
        .ok_or(ConfigError::SessionCompatibility)?;
    let root = root_choices
        .iter()
        .position(|alias| {
            alias.kind.to_string() == saved.provider
                && alias.model == saved.model
                && alias.endpoint.as_str() == saved.endpoint.as_deref().unwrap_or("")
                && alias.effective_settings().is_ok_and(|profile| {
                    profile.thinking_level == saved.thinking_level
                        && profile.account == saved.account
                        && profile.reasoning_summary == saved.reasoning_summary
                })
        })
        .ok_or(ConfigError::SessionCompatibility)?;
    let mut root = root_choices[root].clone();
    root.api_key = root.resolve_api_key()?;
    root_choices.clear();
    root_choices.push(root);
    configuration.select_candidate()?;
    for (name, alias) in &mut configuration.aliases {
        if let Some(saved) = saved.aliases.get(name) {
            alias.model_limits.output_tokens = saved.output_tokens;
            alias.model_limits.context_tokens =
                saved.context_tokens.or(alias.model_limits.context_tokens);
        }
    }
    for (name, choices) in &mut configuration.candidates {
        if let Some(selected) = configuration.aliases.get(name)
            && let Some(first) = choices.first_mut()
        {
            first.model_limits = selected.model_limits;
        }
    }
    if !saved.can_resolve_limits_to(&configuration.effective_settings()?) {
        return Err(ConfigError::SessionCompatibility);
    }
    Ok(configuration)
}

impl ResolvedAlias {
    /// Nonsecret execution profile for durable model pinning.
    ///
    /// # Errors
    /// Reject an effort that cannot be serialized.
    pub fn effective_settings(
        &self,
    ) -> Result<crate::persistence::EffectiveAliasSettings, ConfigError> {
        Ok(crate::persistence::EffectiveAliasSettings {
            provider: self.kind.to_string(),
            endpoint: Some(self.endpoint.clone()),
            model: self.model.clone(),
            account: match &self.credential_source {
                CredentialSource::Subscription { registration } => registration.clone(),
                _ => None,
            },
            thinking_level: self
                .thinking_level
                .map(|level| {
                    serde_json::to_value(level)
                        .map_err(|_| ConfigError::Thinking)?
                        .as_str()
                        .map(str::to_owned)
                        .ok_or(ConfigError::Thinking)
                })
                .transpose()?,
            reasoning_summary: self.reasoning_summary,
            output_tokens: self.model_limits.output_tokens,
            context_tokens: self.model_limits.context_tokens,
        })
    }

    /// Explicit credential variable to exclude from root and child shell environments.
    #[must_use]
    pub fn credential_env(&self) -> Option<&str> {
        match &self.credential_source {
            CredentialSource::Environment { reference } => Some(reference),
            CredentialSource::None
            | CredentialSource::Saved
            | CredentialSource::Subscription { .. } => None,
        }
    }

    /// Reuse a startup key or resolve the captured source without rereading config.
    /// Ollama never reads environment credentials or the saved auth file.
    ///
    /// # Errors
    /// Reject unavailable or invalid keys and unsafe or malformed saved auth.
    pub fn resolve_api_key(&self) -> Result<Option<SecretString>, ConfigError> {
        if let Some(key) = &self.api_key {
            return Ok(Some(key.clone()));
        }
        match &self.credential_source {
            CredentialSource::None | CredentialSource::Subscription { .. } => Ok(None),
            CredentialSource::Environment { reference } => {
                Ok(Some(environment_api_key(reference)?))
            }
            CredentialSource::Saved => Ok(Some(saved_or_environment_api_key(self.kind)?)),
        }
    }

    /// Construct a resolver for the captured subscription registration.
    /// Token resolution checks the registration and inference permission against storage.
    ///
    /// # Errors
    /// Reject a missing registration or failed OAuth client initialization.
    pub fn subscription_credentials(
        &self,
    ) -> Result<Option<std::sync::Arc<dyn cocoa_llm::CredentialResolver>>, ConfigError> {
        match &self.credential_source {
            CredentialSource::Subscription { registration } => {
                let registration = registration
                    .clone()
                    .ok_or(cocoa_oauth::OAuthError::Reauthorize)?;
                Ok(Some(std::sync::Arc::new(crate::chatgpt::Credentials::new(
                    crate::chatgpt::client(None)?,
                    registration,
                ))))
            }
            _ => Ok(None),
        }
    }
}

#[derive(Parser)]
#[command(
    version,
    about = "Cocoa: a code-driven coding agent with private JavaScript execution and local file and Bash access"
)]
pub struct Arguments {
    #[command(subcommand)]
    pub command: Option<Command>,
    /// Start a session and submit this prompt without exiting after the answer.
    pub prompt: Option<String>,
    /// Alias name to start on, overriding the configured default_alias.
    #[arg(short, long, global = true)]
    pub alias: Option<String>,
    /// Select a named model or use provider/model directly.
    #[arg(
        short,
        long,
        global = true,
        conflicts_with = "alias",
        value_name = "MODEL"
    )]
    pub model: Option<String>,
    /// Thinking level for a direct model selection; default omits it.
    #[arg(long, global = true, value_parser = parse_thinking, requires = "model", value_name = "LEVEL")]
    pub thinking: Option<ThinkingChoice>,
    /// Session data directory; does not change configuration or credentials.
    #[arg(long, global = true, value_name = "DIRECTORY")]
    pub data_dir: Option<PathBuf>,
    /// Override the configured TUI theme without changing saved settings.
    #[arg(long, global = true, value_enum)]
    pub theme: Option<crate::theme::Theme>,
    /// Show a static chocolate bar instead of the animated TUI loader.
    #[arg(long, global = true)]
    pub reduced_motion: bool,
}

#[derive(Subcommand)]
pub enum Command {
    /// Update Cocoa, check availability or pin an exact release.
    Update(crate::update::Arguments),
    /// Run one instruction without a terminal and print a JSON outcome.
    Exec {
        /// Save a sanitized whole-session JSON export after shutdown; path must not exist.
        #[arg(long, value_name = "PATH")]
        export: Option<PathBuf>,
        /// Instruction words; when omitted, read UTF-8 text from stdin until EOF.
        #[arg(value_name = "INSTRUCTION", trailing_var_arg = true)]
        instruction: Vec<String>,
    },
    /// Restore a saved session, or browse sessions without creating one.
    Resume(ResumeCommand),
    /// Inspect saved sessions without starting an agent.
    Sessions {
        #[command(subcommand)]
        command: SessionCommand,
    },
    /// Manage HTTP MCP servers and browser authentication.
    Mcp {
        #[command(subcommand)]
        command: Option<McpCommand>,
    },
    /// Select built-in Exa, an MCP search server, or disabled built-in search.
    Search {
        #[command(subcommand)]
        provider: crate::mcp::SearchProvider,
    },
    /// Save an API key from hidden terminal input or piped stdin.
    Login { provider: ProviderKind },
    /// Remove a saved key, without revoking it or changing environment credentials.
    Logout { provider: ProviderKind },
    /// Select any saved model or discover provider models.
    Models,
    /// Open the interactive alias editor.
    Aliases,
    /// Save an alias's provider, model and optional thinking level.
    #[command(hide = true)]
    Alias {
        name: String,
        #[arg(long)]
        model: String,
        #[arg(long)]
        provider: ProviderKind,
        /// Omit to keep an existing level; use default to clear it.
        #[arg(long, value_parser = parse_thinking)]
        thinking: Option<ThinkingChoice>,
    },
}

#[derive(Subcommand)]
pub enum SessionCommand {
    /// Export retained execution evidence without running or recovering an agent.
    Export {
        /// Full session ID or unique prefix of at least eight hexadecimal digits.
        #[arg(value_name = "SESSION")]
        reference: String,
        /// Print compact JSON instead of pretty-printed JSON.
        #[arg(long)]
        json: bool,
    },
    /// List sessions in the current directory, newest activity first.
    List {
        /// Include sessions from every directory.
        #[arg(long)]
        all: bool,
        /// Maximum number of sessions to return.
        #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u32).range(1..=50))]
        limit: u32,
        /// Continue after a cursor returned by an earlier list.
        #[arg(long, value_name = "CURSOR")]
        before: Option<crate::persistence::SessionCursor>,
        /// Print metadata and the continuation cursor as compact JSON.
        #[arg(long)]
        json: bool,
    },
    /// Search literal title and root conversation text without starting an agent.
    Search {
        query: String,
        #[arg(long)]
        all: bool,
        #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u32).range(1..=50))]
        limit: u32,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Clone, Default, Parser)]
pub struct ResumeCommand {
    /// Full session ID or unique prefix of at least eight hexadecimal digits.
    #[arg(value_name = "SESSION", conflicts_with = "last")]
    pub reference: Option<String>,
    /// Resume the most recently active session, without falling back if locked.
    #[arg(long)]
    pub last: bool,
    /// Include sessions from every directory in the picker or --last selection.
    #[arg(long)]
    pub all: bool,
    /// Explicitly relocate the selected session to an existing directory.
    #[arg(long, value_name = "DIRECTORY")]
    pub cwd: Option<PathBuf>,
}

/// A parsed local session command, without configuration or credential resolution.
pub enum SessionAction {
    New,
    Resume(ResumeCommand),
    Browse(String),
    Rewind,
    RewindBefore(cocoa_core::store::RecordId),
}

/// Parse local session commands without resolving configuration or credentials.
///
/// # Errors
/// Reject malformed arguments to recognized session commands.
pub fn session_action(command: &str) -> Result<Option<SessionAction>, ConfigError> {
    let (name, arguments) = command
        .trim()
        .split_once(char::is_whitespace)
        .unwrap_or((command.trim(), ""));
    match name {
        "/new" if arguments.trim().is_empty() => Ok(Some(SessionAction::New)),
        "/new" => Err(ConfigError::Arguments),
        "/rewind" if arguments.trim().is_empty() => Ok(Some(SessionAction::Rewind)),
        "/rewind" => Err(ConfigError::Arguments),
        "/rewind-before" => {
            let record = arguments
                .trim()
                .parse::<i64>()
                .ok()
                .and_then(|record| cocoa_core::store::RecordId::try_from(record).ok())
                .ok_or(ConfigError::Arguments)?;
            Ok(Some(SessionAction::RewindBefore(record)))
        }
        "/sessions" => Ok(Some(SessionAction::Browse(arguments.trim().to_owned()))),
        "/resume" => {
            let resume = ResumeCommand::try_parse_from(
                std::iter::once("resume").chain(arguments.split_whitespace()),
            )
            .map_err(|_| ConfigError::Arguments)?;
            Ok(Some(SessionAction::Resume(resume)))
        }
        _ => Ok(None),
    }
}

#[derive(Subcommand)]
pub enum McpCommand {
    /// Browse name/URL defaults and setup requirements without connecting.
    Presets {
        #[arg(long)]
        json: bool,
    },
    /// Save and connect a server, opening OAuth if required.
    Add {
        name: String,
        /// Explicit endpoint. Omit to use the preset matching NAME.
        url: Option<String>,
        /// Use a preset under a different server name.
        #[arg(long, conflicts_with = "url")]
        preset: Option<String>,
        #[arg(long)]
        client_id: Option<String>,
        /// Read a complete authentication header from an environment variable.
        #[arg(long, value_name = "HEADER=ENV")]
        header_env: Vec<String>,
        /// Save/connect without opening the browser.
        #[arg(long)]
        no_auth: bool,
        /// Save without connecting or opening the browser.
        #[arg(long)]
        no_connect: bool,
    },
    /// Authorize a configured server in the browser.
    Auth { name: String },
    /// Connect enabled servers and show their current status.
    List,
    /// Print the server's exact tool names and JSON schemas.
    Tools { name: String },
    /// Remove a server grant. In-flight effects cannot be undone.
    Remove { name: String },
    /// Enable an existing server grant.
    Enable { name: String },
    /// Stop admitting new calls without deleting credentials.
    Disable { name: String },
    /// Forget local OAuth credentials, without claiming remote revocation.
    Logout { name: String },
}

#[derive(Clone)]
pub struct ThinkingChoice(pub Option<ThinkingLevel>);

/// A configured alias or a direct `provider/model` selection.
#[derive(Clone, Default)]
pub struct Selection {
    pub alias: Option<String>,
    pub model: Option<String>,
    pub thinking: Option<ThinkingLevel>,
    /// Original list index of a regular choice, or the highest-priority choice.
    pub choice: Option<usize>,
}

/// Launch-time presentation, independent of execution and durable session compatibility.
#[derive(Default)]
pub struct Presentation {
    pub theme: crate::theme::Theme,
    pub reduced_motion: bool,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    #[serde(
        default,
        deserialize_with = "unique_table",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    mcp_servers: BTreeMap<String, crate::mcp::Server>,
    #[serde(default, deserialize_with = "unique_table")]
    providers: BTreeMap<String, Provider>,
    #[serde(
        default,
        deserialize_with = "unique_table",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    aliases: BTreeMap<String, Vec<Alias>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    default_alias: Option<String>,
    /// TUI palette. Omission selects Dark chocolate, including during onboarding.
    #[serde(skip_serializing_if = "Option::is_none")]
    theme: Option<crate::theme::Theme>,
    /// Replace the busy animation with a static bar. Omission enables motion.
    #[serde(skip_serializing_if = "Option::is_none")]
    reduced_motion: Option<bool>,
    /// Install stable updates in the background at interactive startup. Default true.
    #[serde(skip_serializing_if = "Option::is_none")]
    auto_update: Option<bool>,
    /// Submit messages during execution for the next model round. Default true.
    #[serde(skip_serializing_if = "Option::is_none")]
    message_steering: Option<bool>,
    /// Search provider. Defaults to keyless Exa; MCP grants remain independent.
    #[serde(skip_serializing_if = "Option::is_none")]
    web_search: Option<crate::mcp::SearchProvider>,
    /// Prompt byte budget, 1..=8388608. Default 8388608.
    #[serde(skip_serializing_if = "Option::is_none")]
    input_bytes: Option<u64>,
    /// Cell and tool feedback byte budget, 1..=1048576. Default 65536.
    #[serde(skip_serializing_if = "Option::is_none")]
    output_bytes: Option<u64>,
    /// Foreground request compaction for both root and child agents.
    #[serde(skip_serializing_if = "Option::is_none")]
    compaction: Option<CompactionSettings>,
    /// Model inactivity and per-cell limit, 1..=3600000 ms. Default 600000.
    /// Does not limit the combined duration of a progressing invocation.
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout_ms: Option<u64>,
    /// Error settlement deadline, 1..=60000 milliseconds. Default 2000.
    #[serde(skip_serializing_if = "Option::is_none")]
    settlement_timeout_ms: Option<u64>,
    /// Consuming shutdown deadline, 1..=60000 milliseconds. Default 2000.
    #[serde(skip_serializing_if = "Option::is_none")]
    shutdown_timeout_ms: Option<u64>,
    /// Maximum active child agents, 1..=16. Default 4.
    #[serde(skip_serializing_if = "Option::is_none")]
    agents_active: Option<u64>,
    /// Maximum retained child agents, agents_active..=32. Default 8.
    #[serde(skip_serializing_if = "Option::is_none")]
    agents_retained: Option<u64>,
}

/// Independent host byte safeguards in addition to model token-window budgeting.
/// Require 0 < target < trigger < request and positive summary input, each at
/// most the engine's context limit, currently 256 MiB. These settings never
/// reduce accepted input/record limits or discard canonical evidence.
/// Summary generation has a 60-second deadline and 4096 sources. Providers that
/// support output-token limits also receive an 8192-token cap. Checkpoints allow
/// 256 KiB, 256 required entries and 256 items per other collection. These
/// independent bounds do not imply token-window fit. Summaries use the selected
/// provider/model and preserve its thinking settings; there is no fallback.
#[derive(Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct CompactionSettings {
    /// Complete encoded continuation request, including provider framing. Default 64 MiB.
    request_bytes: u64,
    /// Independent byte-pressure trigger. Default 48 MiB.
    trigger_bytes: u64,
    /// Soft byte target, not a guaranteed fit. Default 32 MiB.
    target_bytes: u64,
    /// Portable summary evidence/instructions and hosted summary wire bound. Default 64 MiB.
    summary_input_bytes: u64,
}

impl Default for CompactionSettings {
    fn default() -> Self {
        Self {
            request_bytes: 64 * 1024 * 1024,
            trigger_bytes: 48 * 1024 * 1024,
            target_bytes: 32 * 1024 * 1024,
            summary_input_bytes: 64 * 1024 * 1024,
        }
    }
}

impl CompactionSettings {
    fn policy(self, limits: Limits) -> Result<CompactionPolicy, ConfigError> {
        let policy = CompactionPolicy {
            request_bytes: usize::try_from(self.request_bytes).map_err(|_| ConfigError::Bounds)?,
            trigger_bytes: usize::try_from(self.trigger_bytes).map_err(|_| ConfigError::Bounds)?,
            target_bytes: usize::try_from(self.target_bytes).map_err(|_| ConfigError::Bounds)?,
            summary_input_bytes: usize::try_from(self.summary_input_bytes)
                .map_err(|_| ConfigError::Bounds)?,
            summary_output_tokens: 8192,
            sources: 4096,
            checkpoint: CheckpointLimits {
                bytes: 256 * 1024,
                required_entries: 256,
                items: 256,
            },
            timeout: Duration::from_secs(60),
        };
        if policy.request_bytes > limits.context_bytes
            || policy.summary_input_bytes > limits.context_bytes
        {
            return Err(ConfigError::Bounds);
        }
        Ok(policy)
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Provider {
    #[serde(default, deserialize_with = "credential_reference")]
    #[serde(skip_serializing_if = "Option::is_none")]
    credential_env: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    endpoint: Option<String>,
}

/// One model choice, ordered by priority within regular or fallback-only choices.
#[derive(Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Alias {
    pub provider: String,
    pub model: String,
    #[serde(
        rename = "effort",
        alias = "thinking_level",
        skip_serializing_if = "Option::is_none"
    )]
    pub thinking_level: Option<ThinkingLevel>,
    #[serde(default)]
    pub priority: u8,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fallback_only: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    reasoning_summary: bool,
    /// Explicit context window for custom endpoints or missing model metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    context_tokens: Option<u32>,
}

impl Alias {
    /// Build a choice; configuration persistence validates its model and priority.
    pub fn new(
        provider: String,
        model: String,
        thinking_level: Option<ThinkingLevel>,
        priority: u8,
    ) -> Self {
        Self {
            provider,
            model,
            thinking_level,
            priority,
            fallback_only: false,
            reasoning_summary: false,
            context_tokens: None,
        }
    }
}

/// One regular model choice, retaining its original list index for session selection.
#[derive(Clone)]
pub struct ModelSelection {
    pub key: String,
    pub choice: usize,
    pub provider: String,
    pub model: String,
    pub thinking_level: Option<ThinkingLevel>,
}

/// One suggested first-run alias. Persistence validates the complete configuration.
pub struct AliasSuggestion {
    pub name: String,
    pub model: String,
    pub thinking_level: Option<ThinkingLevel>,
}

/// Sanitized startup failures, without user-supplied values or credential names.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error(transparent)]
    Mcp(#[from] crate::mcp::McpError),
    #[error(transparent)]
    OAuth(#[from] cocoa_oauth::OAuthError),
    #[error(transparent)]
    Auth(#[from] crate::auth::AuthError),
    #[error("startup arguments exceed limits")]
    ArgumentsLimit,
    #[error("invalid arguments; use --help for usage")]
    Arguments,
    #[error("cannot read configuration file")]
    File,
    #[error("configuration resolution worker failed")]
    Worker,
    #[error("configuration exceeds 65536 bytes")]
    FileLimit,
    #[error("invalid JSON configuration or unknown configuration key")]
    Json,
    #[error("unsupported provider; choose a provider listed in /login or docs/providers.md")]
    Provider,
    #[error("alias references an unknown or unconfigured provider")]
    ProviderReference,
    #[error("selection is required; use --model, --alias or configure default_alias")]
    AliasRequired,
    #[error("unknown alias or invalid alias name")]
    Alias,
    #[error("default_alias must name a configured alias")]
    DefaultAlias,
    #[error(
        "aliases require 1..=32 choices, at least one regular choice and priorities from 0 to 99"
    )]
    AliasChoices,
    #[error(
        "saved session settings differ from the current configuration; restore the original alias, model, account and limits before resuming"
    )]
    SessionCompatibility,
    #[error(
        "configuration tables allow at most 32 aliases and 256 model choices; names require 1..=64 ASCII letters, digits, underscores or hyphens"
    )]
    Tables,
    #[error("model is required, nonblank and at most 256 bytes")]
    Model,
    #[error("direct model selection requires provider/model and cannot be combined with an alias")]
    ModelSelection,
    #[error("thinking overrides require a direct provider/model selection, not a named model")]
    ThinkingSelection,
    #[error("reasoning_summary requires an OpenAI alias with a supporting model")]
    ReasoningSummary,
    #[error("a valid credential environment reference is required")]
    CredentialReference,
    #[error(
        "referenced credential is absent or empty, non-Unicode, not graphic ASCII, or exceeds 8192 bytes"
    )]
    Credential,
    #[error("invalid bounds")]
    Bounds,
    #[error("endpoint must be nonblank and at most 2048 bytes")]
    Endpoint,
    #[error("provider setup failed; check endpoint, model and thinking level")]
    AliasOptions(#[source] cocoa_llm::ProviderError),
    #[error("use default, none, minimal, low, medium, high, xhigh or max")]
    Thinking,
    #[error("cannot save configuration; original file was not replaced")]
    Save,
    #[error("configuration is busy; retry later")]
    Busy,
    #[error("configuration was saved, but directory sync failed")]
    Sync,
}

/// Parse bounded command-line input without loading configuration or credentials.
///
/// # Errors
/// Reject invalid or oversized arguments. Help and version return `None`.
pub fn arguments() -> Result<Option<Arguments>, ConfigError> {
    let mut arguments = Vec::<OsString>::new();
    let mut bytes = 0usize;
    for argument in std::env::args_os() {
        bytes = bytes.saturating_add(argument.len());
        if bytes > 8 * 1024 * 1024 + 65_536 || arguments.len() >= 64 {
            return Err(ConfigError::ArgumentsLimit);
        }
        arguments.push(argument);
    }
    let arguments = match Arguments::try_parse_from(arguments) {
        Ok(arguments) => arguments,
        Err(error)
            if matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) =>
        {
            print!("{error}");
            return Ok(None);
        }
        Err(_) => return Err(ConfigError::Arguments),
    };
    Ok(Some(arguments))
}

/// Resolve an alias from the default file, without changing its credential source.
///
/// # Errors
/// Reject invalid configuration, unknown aliases or unavailable selected credentials.
pub fn load(alias: Option<String>) -> Result<Configuration, ConfigError> {
    load_selection(alias, None, None, None)
}

/// Resolve an alias or a direct `provider/model` selection without saving settings.
/// Direct selections use the provider's configured endpoint and credential source.
/// An explicit `credential_env` excludes saved auth; Ollama never reads auth files.
///
/// # Errors
/// Reject conflicting selections, invalid configuration or unavailable credentials.
pub fn load_selection(
    mut alias: Option<String>,
    mut model: Option<String>,
    thinking: Option<ThinkingLevel>,
    choice: Option<usize>,
) -> Result<Configuration, ConfigError> {
    let file = read_settings()?;
    let saved_model = model.as_ref().is_some_and(|name| !name.contains('/'));
    if saved_model {
        if alias.is_some() {
            return Err(ConfigError::ModelSelection);
        }
        if thinking.is_some() {
            return Err(ConfigError::ThinkingSelection);
        }
        alias = model.take();
    }
    let mut configuration = build(
        Selection {
            alias,
            model,
            thinking,
            choice,
        },
        file,
    )?;
    for alias in configuration.candidates.values_mut().flatten() {
        if let CredentialSource::Subscription { registration } = &mut alias.credential_source {
            *registration = crate::chatgpt::client(None)?.active_registration().ok();
        }
    }
    let candidates = configuration
        .candidates
        .get_mut(&configuration.alias)
        .ok_or(ConfigError::Alias)?;
    let mut last_error = ConfigError::Alias;
    let mut selected = None;
    for (index, candidate) in candidates.iter_mut().enumerate() {
        match candidate.resolve_api_key().and_then(|key| {
            candidate.subscription_credentials()?;
            Ok(key)
        }) {
            Ok(key) => {
                candidate.api_key = key;
                selected = Some(index);
                break;
            }
            Err(error) => last_error = error,
        }
    }
    let index = selected.ok_or(last_error)?;
    candidates.drain(..index);
    configuration.select_candidate()?;
    Ok(configuration)
}

/// Read presentation settings without resolving a provider or requiring credentials.
///
/// # Errors
/// Reject unreadable, oversized or malformed configuration.
pub fn presentation() -> Result<Presentation, ConfigError> {
    let settings = read_settings()?;
    Ok(Presentation {
        theme: settings.theme.unwrap_or_default(),
        reduced_motion: settings.reduced_motion.unwrap_or_default(),
    })
}

/// Read the automatic-update preference without resolving inference credentials.
/// `COCOA_AUTO_UPDATE=0` disables it for this process, including with invalid config.
///
/// # Errors
/// Rejects unreadable, oversized or malformed configuration.
pub fn auto_update() -> Result<bool, ConfigError> {
    if std::env::var_os("COCOA_AUTO_UPDATE").is_some_and(|value| value == "0") {
        return Ok(false);
    }
    Ok(read_settings()?.auto_update.unwrap_or(true))
}

/// Read alias mappings without requiring credentials or a selected alias.
///
/// # Errors
/// Reject unreadable, oversized or malformed configuration.
pub fn aliases() -> Result<BTreeMap<String, Vec<Alias>>, ConfigError> {
    let file = read_settings()?;
    let aliases = file.aliases.clone();
    if let Some(name) = aliases.keys().next() {
        build(
            Selection {
                alias: Some(name.clone()),
                ..Selection::default()
            },
            file,
        )?;
    }
    Ok(aliases)
}

/// Read validated MCP grants independently of inference-provider setup.
///
/// # Errors
/// Reject malformed configuration, unsafe endpoints or duplicate grant IDs.
pub fn mcp_servers() -> Result<BTreeMap<String, crate::mcp::Server>, ConfigError> {
    let servers = read_settings()?.mcp_servers;
    crate::mcp::validate_servers(&servers)?;
    Ok(servers)
}

/// Construct a session registry with credentials separate from subscription login.
///
/// # Errors
/// Reject unavailable home directory, configuration or HTTP initialization.
pub fn mcp_manager() -> Result<crate::mcp::McpManager, ConfigError> {
    let home = std::env::home_dir().ok_or(ConfigError::Save)?;
    let manager = crate::mcp::McpManager::new(home.join(".config/cocoa/oauth/mcp"))?;
    let settings = read_settings()?;
    manager.configure(settings.mcp_servers)?;
    manager.set_search_provider(settings.web_search.unwrap_or_default())?;
    Ok(manager)
}

/// Save search selection without connecting or requiring inference credentials.
/// Disabled or removed selected servers never restore Exa implicitly.
///
/// # Errors
/// Reject selection of a missing/disabled server or failed configuration persistence.
pub fn save_search_provider(provider: crate::mcp::SearchProvider) -> Result<(), ConfigError> {
    save_settings(|file| {
        crate::mcp::validate_servers(&file.mcp_servers)?;
        if let crate::mcp::SearchProvider::Mcp { server } = &provider
            && !file
                .mcp_servers
                .get(server)
                .is_some_and(crate::mcp::Server::enabled)
        {
            return Err(crate::mcp::McpError::Unavailable.into());
        }
        file.web_search = Some(provider);
        Ok(file.aliases.keys().next().cloned().unwrap_or_default())
    })
}

/// An MCP grant edit. Add requires a new name; replace and remove require an existing name.
pub enum McpEdit {
    Add(crate::mcp::Server),
    Replace(crate::mcp::Server),
    Remove,
}

/// Update only MCP grants under the shared configuration lock.
///
/// # Errors
/// Reject duplicate/missing names, malformed settings or failed persistence.
pub fn save_mcp(name: String, edit: McpEdit) -> Result<(), ConfigError> {
    save_settings(|file| {
        let add = matches!(edit, McpEdit::Add(_));
        if add && file.mcp_servers.contains_key(&name)
            || !add && !file.mcp_servers.contains_key(&name)
        {
            return Err(crate::mcp::McpError::Configuration.into());
        }
        match edit {
            McpEdit::Add(server) | McpEdit::Replace(server) => {
                file.mcp_servers.insert(name, server);
            }
            McpEdit::Remove => {
                file.mcp_servers.remove(&name);
            }
        }
        crate::mcp::validate_servers(&file.mcp_servers)?;
        Ok(file.aliases.keys().next().cloned().unwrap_or_default())
    })
}

/// Configured names, with standard aliases first and custom names alphabetically.
pub fn alias_names(aliases: &BTreeMap<String, Vec<Alias>>) -> Vec<String> {
    let mut names: Vec<_> = aliases.keys().cloned().collect();
    let standard = ["x-low", "low", "medium", "high", "x-high", "ultra"];
    names.sort_by_key(|name| {
        standard
            .iter()
            .position(|standard| name == standard)
            .unwrap_or(standard.len())
    });
    names
}

/// Regular choices for an alias, ascending priority with list order breaking ties.
/// `None` uses the configured default alias. Resolves no credentials.
///
/// # Errors
/// Reject unreadable or invalid configuration, or an unknown alias.
pub fn cycle_entries(alias: Option<&str>) -> Result<Vec<ModelSelection>, ConfigError> {
    let file = read_settings()?;
    let Some(name) = alias.or(file.default_alias.as_deref()) else {
        return Ok(Vec::new());
    };
    let entries = file.aliases.get(name).ok_or(ConfigError::Alias)?;
    let choices = regular_choices(entries);
    Ok(choices
        .into_iter()
        .map(|choice| {
            let entry = &entries[choice];
            ModelSelection {
                key: name.into(),
                choice,
                provider: entry.provider.clone(),
                model: entry.model.clone(),
                thinking_level: entry.thinking_level,
            }
        })
        .collect())
}

/// Original list indices of regular choices, ascending priority with stable ties.
pub fn regular_choices(entries: &[Alias]) -> Vec<usize> {
    let mut choices: Vec<_> = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| !entry.fallback_only)
        .map(|(index, _)| index)
        .collect();
    choices.sort_by_key(|index| entries[*index].priority);
    choices
}

/// Save an alias's complete model list atomically. An empty list removes the alias.
/// The first alias becomes the default; removing the default selects the next alias.
///
/// # Errors
/// Reject invalid names, choices, resource limits or failed persistence.
pub fn save_alias_choices(
    name: String,
    entries: Vec<Alias>,
    make_default: bool,
) -> Result<(), ConfigError> {
    save_settings(|file| {
        if !valid_name(&name) {
            return Err(ConfigError::Alias);
        }
        for entry in &entries {
            let kind = entry
                .provider
                .parse::<ProviderKind>()
                .map_err(|_| ConfigError::Provider)?;
            file.providers.entry(kind.to_string()).or_insert(Provider {
                credential_env: None,
                endpoint: None,
            });
        }
        if entries.is_empty() {
            file.aliases.remove(&name);
        } else {
            file.aliases.insert(name.clone(), entries);
        }
        if make_default
            || file.default_alias.is_none()
            || file.default_alias.as_ref() == Some(&name) && !file.aliases.contains_key(&name)
        {
            file.default_alias = if file.aliases.contains_key(&name) {
                Some(name.clone())
            } else {
                file.aliases.keys().next().cloned()
            };
        }
        Ok(file.default_alias.clone().unwrap_or_default())
    })
}

/// Save one complete mapping atomically, preserving other configuration fields.
/// The first saved alias becomes the default. No credential or generation request is needed.
///
/// # Errors
/// Reject invalid mappings, unsafe file types, busy locks and failed persistence.
pub fn save_alias(
    name: String,
    provider: ProviderKind,
    model: String,
    thinking: Option<Option<ThinkingLevel>>,
) -> Result<(), ConfigError> {
    save_settings(|file| {
        let entries = file.aliases.entry(name.clone()).or_default();
        let index = entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| !entry.fallback_only)
            .min_by_key(|(_, entry)| entry.priority)
            .map(|(index, _)| index);
        let existing = index.and_then(|index| entries.get(index));
        let level = thinking.unwrap_or_else(|| existing.and_then(|alias| alias.thinking_level));
        let priority = existing.map_or(0, |alias| alias.priority);
        let reasoning_summary = existing.is_some_and(|alias| alias.reasoning_summary)
            && provider == ProviderKind::OpenAi;
        let context_tokens = existing
            .filter(|alias| alias.provider == provider.to_string() && alias.model == model)
            .and_then(|alias| alias.context_tokens);
        file.providers
            .entry(provider.to_string())
            .or_insert(Provider {
                credential_env: None,
                endpoint: None,
            });
        let entry = Alias {
            provider: provider.to_string(),
            model,
            thinking_level: level,
            priority,
            fallback_only: false,
            reasoning_summary,
            context_tokens,
        };
        if let Some(index) = index {
            entries[index] = entry;
        } else {
            entries.push(entry);
        }
        if file.default_alias.is_none() {
            file.default_alias = Some(name.clone());
        }
        Ok(name)
    })
}

/// Register a provider, seeding aliases only if none exist when the lock is acquired.
/// Existing mappings, provider settings and the default alias are preserved.
///
/// # Errors
/// Reject empty initial aliases, malformed settings, unsafe storage and failed saves.
pub fn save_setup(
    provider: ProviderKind,
    aliases: Vec<AliasSuggestion>,
) -> Result<(), ConfigError> {
    save_settings(|file| {
        file.providers
            .entry(provider.to_string())
            .or_insert(Provider {
                credential_env: None,
                endpoint: None,
            });
        if file.aliases.is_empty() {
            let mut names = BTreeSet::new();
            for alias in aliases {
                if !names.insert(alias.name.clone()) {
                    return Err(ConfigError::Tables);
                }
                file.aliases.insert(
                    alias.name,
                    vec![Alias::new(
                        provider.to_string(),
                        alias.model,
                        alias.thinking_level,
                        0,
                    )],
                );
            }
            file.default_alias = file.aliases.contains_key("medium").then(|| "medium".into());
        }
        let selected = file
            .default_alias
            .clone()
            .or_else(|| file.aliases.keys().next().cloned())
            .ok_or(ConfigError::AliasRequired)?;
        if file.default_alias.is_none() {
            file.default_alias = Some(selected.clone());
        }
        Ok(selected)
    })
}

/// Resolve first-run discovery without persisting a candidate key or guessing an endpoint.
/// An explicit environment credential cannot be replaced by saving an unused key.
///
/// # Errors
/// Reject malformed provider settings and unavailable or conflicting credentials.
pub fn setup_provider(
    kind: ProviderKind,
    key: Option<SecretString>,
) -> Result<ResolvedAlias, ConfigError> {
    if kind == ProviderKind::ChatGpt && key.is_some() {
        return Err(crate::auth::AuthError::Subscription.into());
    }
    let mut provider = catalogue_provider(kind)?.unwrap_or(ResolvedAlias {
        kind,
        endpoint: kind.endpoint_default().into(),
        model: "catalogue".into(),
        model_limits: ModelLimits::default(),
        api_key: None,
        thinking_level: None,
        reasoning_summary: false,
        credential_source: if kind == ProviderKind::ChatGpt {
            CredentialSource::Subscription {
                registration: Some(crate::chatgpt::client(None)?.active_registration()?),
            }
        } else if kind == ProviderKind::Ollama {
            CredentialSource::None
        } else {
            CredentialSource::Saved
        },
    });
    if key.is_some() && provider.credential_env().is_some() {
        return Err(ConfigError::CredentialReference);
    }
    if let Some(key) = key {
        provider.api_key = Some(crate::auth::resolve_credential(
            secrecy::ExposeSecret::expose_secret(&key),
        )?);
    }
    provider.api_key = provider.resolve_api_key()?;
    Ok(provider)
}

/// Discover configured providers and those with saved or standard environment credentials.
/// Undeclared local Ollama is excluded. Explicit credential references never fall
/// back to saved auth; custom endpoints remain unchanged.
///
/// # Errors
/// Reject malformed configuration or unsafe or unavailable credential storage.
pub fn catalogue_provider(kind: ProviderKind) -> Result<Option<ResolvedAlias>, ConfigError> {
    let file = read_settings()?;
    let provider = file.providers.get(&kind.to_string());
    if kind == ProviderKind::ChatGpt {
        if provider.is_some_and(|provider| {
            provider.credential_env.is_some()
                || provider
                    .endpoint
                    .as_ref()
                    .is_some_and(|endpoint| endpoint != kind.endpoint_default())
        }) {
            return Err(ConfigError::CredentialReference);
        }
        let registration = match crate::chatgpt::client(None)?.active_registration() {
            Ok(registration) => registration,
            Err(cocoa_oauth::OAuthError::Reauthorize) if provider.is_none() => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        return Ok(Some(ResolvedAlias {
            kind,
            endpoint: kind.endpoint_default().into(),
            model: "catalogue".into(),
            model_limits: ModelLimits::default(),
            api_key: None,
            thinking_level: None,
            reasoning_summary: false,
            credential_source: CredentialSource::Subscription {
                registration: Some(registration),
            },
        }));
    }
    let api_key = if provider.is_none() {
        if kind == ProviderKind::Ollama {
            return Ok(None);
        }
        match saved_or_environment_api_key(kind) {
            Ok(key) => Some(key),
            Err(ConfigError::Auth(crate::auth::AuthError::Missing)) => return Ok(None),
            Err(error) => return Err(error),
        }
    } else {
        None
    };
    let credential_source = match provider.and_then(|provider| provider.credential_env.clone()) {
        Some(reference) => CredentialSource::Environment { reference },
        None if kind == ProviderKind::Ollama => CredentialSource::None,
        None => CredentialSource::Saved,
    };
    Ok(Some(ResolvedAlias {
        kind,
        endpoint: provider
            .and_then(|provider| provider.endpoint.clone())
            .unwrap_or_else(|| kind.endpoint_default().into()),
        model: "catalogue".into(),
        model_limits: ModelLimits::default(),
        api_key,
        thinking_level: None,
        reasoning_summary: false,
        credential_source,
    }))
}

/// Parse a CLI thinking value; `default` clears the explicit request.
///
/// # Errors
/// Reject values outside the engine's representable thinking levels.
pub fn parse_thinking(value: &str) -> Result<ThinkingChoice, ConfigError> {
    if value == "default" {
        return Ok(ThinkingChoice(None));
    }
    serde_json::from_value(serde_json::Value::String(value.into()))
        .map(|level| ThinkingChoice(Some(level)))
        .map_err(|_| ConfigError::Thinking)
}

pub(crate) fn unique_table<'de, T: Deserialize<'de>, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, T>, D::Error> {
    struct TableVisitor<T>(std::marker::PhantomData<T>);

    impl<'de, T: Deserialize<'de>> serde::de::Visitor<'de> for TableVisitor<T> {
        type Value = BTreeMap<String, T>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a table with unique names")
        }

        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut access: A,
        ) -> Result<Self::Value, A::Error> {
            let mut table = BTreeMap::new();
            while let Some(name) = access.next_key::<String>()? {
                if table.contains_key(&name) {
                    return Err(serde::de::Error::custom("duplicate configuration name"));
                }
                table.insert(name, access.next_value()?);
            }
            Ok(table)
        }
    }

    deserializer.deserialize_map(TableVisitor(std::marker::PhantomData))
}

fn save_settings(
    update: impl FnOnce(&mut Settings) -> Result<String, ConfigError>,
) -> Result<(), ConfigError> {
    use std::io::Write;
    let path = default_path().ok_or(ConfigError::Save)?;
    let parent = path.parent().ok_or(ConfigError::Save)?;
    std::fs::create_dir_all(parent).map_err(|_| ConfigError::Save)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(parent.join("config.json.lock"))
        .map_err(|_| ConfigError::Save)?;
    lock.try_lock().map_err(|_| ConfigError::Busy)?;
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if !metadata.file_type().is_file() => return Err(ConfigError::Save),
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            return Err(ConfigError::File);
        }
        _ => {}
    }
    let mut file = read_settings()?;
    let name = update(&mut file)?;
    let bytes = serde_json::to_vec_pretty(&file).map_err(|_| ConfigError::Save)?;
    if bytes.len() > 65_536 {
        return Err(ConfigError::FileLimit);
    }
    if !file.aliases.is_empty() {
        build(
            Selection {
                alias: (!name.is_empty()).then_some(name),
                ..Selection::default()
            },
            file,
        )?;
    }
    let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(|_| ConfigError::Save)?;
    temporary
        .write_all(&bytes)
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|_| ConfigError::Save)?;
    temporary.persist(&path).map_err(|_| ConfigError::Save)?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| ConfigError::Sync)
}

fn default_path() -> Option<PathBuf> {
    std::env::home_dir().map(|home| home.join(".config/cocoa/config.json"))
}

fn read_settings() -> Result<Settings, ConfigError> {
    let file = match default_path().map(File::open) {
        Some(Ok(file)) => {
            let mut bytes = Vec::new();
            file.take(65_537)
                .read_to_end(&mut bytes)
                .map_err(|_| ConfigError::File)?;
            if bytes.len() > 65_536 {
                return Err(ConfigError::FileLimit);
            }
            serde_json::from_slice::<Settings>(&bytes).map_err(|_| ConfigError::Json)?
        }
        Some(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => Settings::default(),
        None => Settings::default(),
        Some(Err(_)) => return Err(ConfigError::File),
    };
    Ok(file)
}

fn build(selection: Selection, mut file: Settings) -> Result<Configuration, ConfigError> {
    if selection.model.is_some() && selection.alias.is_some() {
        return Err(ConfigError::ModelSelection);
    }
    if selection.thinking.is_some() && selection.model.is_none() {
        return Err(ConfigError::ThinkingSelection);
    }
    for entry in file.aliases.values().flatten() {
        let kind = entry
            .provider
            .parse::<ProviderKind>()
            .map_err(|_| ConfigError::Provider)?;
        file.providers.entry(kind.to_string()).or_insert(Provider {
            credential_env: None,
            endpoint: None,
        });
    }
    let direct = match selection.model {
        Some(identifier) => {
            let (provider, model) = direct_model(&identifier)?;
            file.providers.entry(provider.clone()).or_insert(Provider {
                credential_env: None,
                endpoint: None,
            });
            let alias = Alias {
                provider,
                model,
                thinking_level: selection.thinking,
                priority: 0,
                fallback_only: false,
                reasoning_summary: false,
                context_tokens: None,
            };
            Some((identifier, vec![alias]))
        }
        None => None,
    };
    crate::mcp::validate_servers(&file.mcp_servers)?;
    if file.aliases.len() > 32
        || file.aliases.values().map(Vec::len).sum::<usize>() > 256
        || !file
            .providers
            .keys()
            .chain(file.aliases.keys())
            .all(|name| valid_name(name))
    {
        return Err(ConfigError::Tables);
    }
    if file
        .providers
        .keys()
        .any(|provider| provider.parse::<ProviderKind>().is_err())
    {
        return Err(ConfigError::Provider);
    }
    for (name, provider) in &file.providers {
        if name == "chatgpt"
            && (provider.credential_env.is_some()
                || provider
                    .endpoint
                    .as_ref()
                    .is_some_and(|endpoint| endpoint != ProviderKind::ChatGpt.endpoint_default()))
        {
            return Err(ConfigError::CredentialReference);
        }
        if name == "ollama" && provider.credential_env.is_some() {
            return Err(crate::auth::AuthError::OllamaCredential.into());
        }
        if provider
            .credential_env
            .as_ref()
            .is_some_and(|reference| !valid_reference(reference))
        {
            return Err(ConfigError::CredentialReference);
        }
        if provider
            .endpoint
            .as_ref()
            .is_some_and(|endpoint| endpoint.trim().is_empty() || endpoint.len() > 2048)
        {
            return Err(ConfigError::Endpoint);
        }
    }
    for entries in file.aliases.values() {
        if entries.is_empty()
            || entries.len() > 32
            || !entries.iter().any(|entry| !entry.fallback_only)
            || entries.iter().any(|entry| entry.priority > 99)
        {
            return Err(ConfigError::AliasChoices);
        }
    }
    for alias in file.aliases.values().flatten() {
        if !file.providers.contains_key(&alias.provider) {
            return Err(ConfigError::ProviderReference);
        }
        if alias.model.trim().is_empty()
            || alias.model.len() > 256
            || alias.model.chars().any(char::is_control)
        {
            return Err(ConfigError::Model);
        }
        if alias.reasoning_summary && alias.provider != "openai" {
            return Err(ConfigError::ReasoningSummary);
        }
    }
    if file
        .default_alias
        .as_ref()
        .is_some_and(|name| !file.aliases.contains_key(name))
    {
        return Err(ConfigError::DefaultAlias);
    }
    let name = match &direct {
        Some((selection, _)) => selection.clone(),
        None => {
            let name = selection
                .alias
                .clone()
                .or_else(|| file.default_alias.clone())
                .ok_or(ConfigError::AliasRequired)?;
            if !file.aliases.contains_key(&name) {
                return Err(ConfigError::Alias);
            }
            name
        }
    };
    let input_bytes = file.input_bytes.unwrap_or(8 * 1024 * 1024);
    let output_bytes = file.output_bytes.unwrap_or(65_536);
    let timeout_ms = file.timeout_ms.unwrap_or(600_000);
    let settlement_timeout_ms = file.settlement_timeout_ms.unwrap_or(2000);
    let shutdown_timeout_ms = file.shutdown_timeout_ms.unwrap_or(2000);
    let agents_active = file.agents_active.unwrap_or(4);
    let agents_retained = file.agents_retained.unwrap_or(8);
    if !(1..=8 * 1024 * 1024).contains(&input_bytes)
        || !(1..=1_048_576).contains(&output_bytes)
        || !(1..=3_600_000).contains(&timeout_ms)
        || !(1..=60_000).contains(&settlement_timeout_ms)
        || !(1..=60_000).contains(&shutdown_timeout_ms)
        || !(1..=16).contains(&agents_active)
        || !(agents_active..=32).contains(&agents_retained)
    {
        return Err(ConfigError::Bounds);
    }
    let input_bytes = usize::try_from(input_bytes).map_err(|_| ConfigError::Bounds)?;
    let output_bytes = usize::try_from(output_bytes).map_err(|_| ConfigError::Bounds)?;
    let agents_active = usize::try_from(agents_active).map_err(|_| ConfigError::Bounds)?;
    let agents_retained = usize::try_from(agents_retained).map_err(|_| ConfigError::Bounds)?;
    let credential_env_names = file
        .providers
        .values()
        .filter_map(|provider| provider.credential_env.clone())
        .chain(
            file.mcp_servers
                .values()
                .flat_map(|server| server.credential_env_names().map(str::to_owned)),
        )
        .collect();
    let mut aliases = BTreeMap::new();
    let mut candidates = BTreeMap::new();
    for (alias_name, entries) in file.aliases.into_iter().chain(direct) {
        let regular = entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| !entry.fallback_only)
            .min_by_key(|(_, entry)| entry.priority)
            .map(|(index, _)| index)
            .ok_or(ConfigError::AliasChoices)?;
        let selected = if alias_name == name {
            selection.choice.unwrap_or(regular)
        } else {
            regular
        };
        if entries
            .get(selected)
            .is_none_or(|entry| entry.fallback_only)
        {
            return Err(ConfigError::AliasChoices);
        }
        let mut indices: Vec<_> = entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.fallback_only)
            .map(|(index, _)| index)
            .collect();
        indices.sort_by_key(|index| entries[*index].priority);
        indices.insert(0, selected);
        let mut resolved = Vec::with_capacity(entries.len());
        for alias in &entries {
            let provider = file
                .providers
                .get(&alias.provider)
                .ok_or(ConfigError::ProviderReference)?;
            let kind = alias
                .provider
                .parse::<ProviderKind>()
                .map_err(|_| ConfigError::Provider)?;
            let credential_source = match &provider.credential_env {
                Some(reference) => CredentialSource::Environment {
                    reference: reference.clone(),
                },
                None if kind == ProviderKind::Ollama => CredentialSource::None,
                None if kind == ProviderKind::ChatGpt => {
                    CredentialSource::Subscription { registration: None }
                }
                None => CredentialSource::Saved,
            };
            let alias = ResolvedAlias {
                kind,
                endpoint: provider
                    .endpoint
                    .clone()
                    .unwrap_or_else(|| kind.endpoint_default().into()),
                model: alias.model.clone(),
                model_limits: ModelLimits {
                    output_tokens: None,
                    context_tokens: alias.context_tokens,
                },
                api_key: None,
                thinking_level: alias.thinking_level,
                reasoning_summary: alias.reasoning_summary,
                credential_source,
            };
            validate_alias(&alias)?;
            resolved.push(alias);
        }
        let choices: Vec<_> = indices
            .into_iter()
            .map(|index| resolved[index].clone())
            .collect();
        let first = choices.first().ok_or(ConfigError::AliasChoices)?.clone();
        aliases.insert(alias_name.clone(), first);
        candidates.insert(alias_name, choices);
    }
    if aliases.len() > 64 {
        return Err(ConfigError::Tables);
    }
    let alias = aliases.get(&name).ok_or(ConfigError::Alias)?;
    // Canonical responses retain both the action and provider replay data.
    let record_bytes = 2 * DECODER_BYTES + 4096;
    let context_bytes = 4 * DECODER_BYTES;
    let limits = Limits {
        input_bytes,
        output_bytes,
        record_bytes,
        context_bytes,
        pending_operations: 16,
        stall_timeout: Duration::from_millis(timeout_ms),
        settlement_timeout: Duration::from_millis(settlement_timeout_ms),
        shutdown_timeout: Duration::from_millis(shutdown_timeout_ms),
    };
    let policy = file.compaction.unwrap_or_default().policy(limits)?;
    let engine = EngineSettings::new(limits)
        .and_then(|settings| settings.with_compaction(policy))
        .map_err(|_| ConfigError::Bounds)?;
    Ok(Configuration {
        alias: name,
        provider: alias.kind,
        model: alias.model.clone(),
        thinking_level: alias.thinking_level,
        reasoning_summary: alias.reasoning_summary,
        endpoint: alias.endpoint.clone(),
        api_key: alias.api_key.clone(),
        credential_env_names,
        engine,
        store: StoreLimits { record_bytes },
        aliases,
        candidates,
        agents_active,
        agents_retained,
        mcp_servers: file.mcp_servers,
        web_search: file.web_search.unwrap_or_default(),
        message_steering: file.message_steering.unwrap_or(true),
    })
}

/// Split a direct `provider/model` selection, keeping the identifier after the first slash.
fn direct_model(selection: &str) -> Result<(String, String), ConfigError> {
    let (provider, model) = selection
        .split_once('/')
        .ok_or(ConfigError::ModelSelection)?;
    let kind = provider
        .parse::<ProviderKind>()
        .map_err(|_| ConfigError::Provider)?;
    if model.trim().is_empty() || model.len() > 256 || model.chars().any(char::is_control) {
        return Err(ConfigError::Model);
    }
    Ok((kind.to_string(), model.into()))
}

fn validate_alias(alias: &ResolvedAlias) -> Result<(), ConfigError> {
    // Validate transport options without requiring an unselected alias's key.
    let options = cocoa_llm::ProviderOptions {
        kind: alias.kind,
        endpoint: alias.endpoint.clone(),
        model: alias.model.clone(),
        api_key: (!matches!(alias.kind, ProviderKind::Ollama | ProviderKind::ChatGpt))
            .then(|| "validation".into()),
        execution: cocoa_llm::ExecutionMode::Enabled,
        thinking_level: alias.thinking_level,
        session_id: "configuration-validation".into(),
    };
    let mut provider = if alias.kind == ProviderKind::ChatGpt {
        cocoa_llm::Provider::chatgpt(
            options,
            std::sync::Arc::new(crate::chatgpt::Credentials::new(
                crate::chatgpt::client(None)?,
                "configuration-validation".into(),
            )),
        )
    } else {
        cocoa_llm::Provider::new(options)
    }
    .map_err(ConfigError::AliasOptions)?;
    provider
        .set_model_limits(alias.model_limits)
        .map_err(ConfigError::AliasOptions)?;
    if alias.reasoning_summary {
        provider
            .enable_reasoning_summary()
            .map_err(ConfigError::AliasOptions)?;
    }
    Ok(())
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn valid_reference(reference: &str) -> bool {
    !reference.is_empty()
        && reference.len() <= 256
        && reference.bytes().enumerate().all(|(index, byte)| {
            byte == b'_' || byte.is_ascii_alphabetic() || (index > 0 && byte.is_ascii_digit())
        })
}

fn saved_or_environment_api_key(kind: ProviderKind) -> Result<SecretString, ConfigError> {
    match crate::auth::saved(kind) {
        Ok(key) => Ok(key),
        Err(crate::auth::AuthError::Missing) => {
            let reference = kind
                .credential_env()
                .ok_or(crate::auth::AuthError::Missing)?;
            if std::env::var_os(reference).is_none() {
                return Err(crate::auth::AuthError::Missing.into());
            }
            environment_api_key(reference)
        }
        Err(error) => Err(error.into()),
    }
}

fn environment_api_key(reference: &str) -> Result<SecretString, ConfigError> {
    let key = std::env::var_os(reference).ok_or(ConfigError::Credential)?;
    if key.len() > 8192 {
        return Err(ConfigError::Credential);
    }
    let key = key.into_string().map_err(|_| ConfigError::Credential)?;
    if !crate::auth::valid_key(&key) {
        return Err(ConfigError::Credential);
    }
    Ok(key.into())
}

fn credential_reference<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    // An explicit null must not silently select saved auth or a default variable.
    String::deserialize(deserializer).map(Some)
}
