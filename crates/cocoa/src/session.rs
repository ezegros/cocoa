//! Owned engine, coding environment and child-agent cleanup for a terminal session.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

#[path = "startup_model.rs"]
pub(crate) mod startup_model;

use crate::{agent_factory, config};
use cocoa::{
    agents::{AgentLimits, AgentManager, AgentUpdates},
    persistence::SessionAttachment,
};
use cocoa_core::{
    capability::CapabilityError,
    engine::{Engine, EngineAdapters, EventPublisher, RunError, ShutdownError},
};
use cocoa_quickjs::{QuickJsExecutor, QuickJsOptions};
use cocoa_tools::{
    CodingCapabilities, CodingLimits, LocalConfiguration, LocalEnvironment, ProjectEnvironment,
};

pub(crate) struct Session {
    pub engine: Engine,
    pub events: EventPublisher,
    pub agent_updates: AgentUpdates,
    pub input_bytes: usize,
    pub message_steering: bool,
    pub mcp: cocoa::mcp::McpManager,
    agents: AgentManager,
    environment: Arc<ProjectEnvironment>,
    shutdown_timeout: Duration,
}

impl Session {
    /// Build a root session with fresh coding tools and a private runtime.
    /// Check project paths before allocating the local backend, and finish
    /// environment setup and capability binding before starting JavaScript.
    /// A returned session must be consumed by `close`.
    ///
    /// # Errors
    /// Report invalid configuration, unavailable project paths, adapter setup or
    /// history restoration failures. Later setup failures attempt both agent and
    /// environment cleanup; cleanup errors take precedence over the setup error.
    pub async fn new(
        mut configuration: config::Configuration,
        attachment: SessionAttachment,
        mcp: Option<cocoa::mcp::McpManager>,
    ) -> Result<Self, CommandError> {
        let resumed = attachment.is_resumed();
        let working_directory = attachment.working_directory().await?;
        let (working_directory, home) = tokio::task::spawn_blocking(move || {
            let working_directory = std::fs::canonicalize(working_directory)
                .map_err(cocoa_tools::ConfigurationError::Io)?;
            let home = std::env::home_dir();
            // ProjectEnvironment consumes the local backend even when path validation fails.
            if !working_directory.is_dir()
                || working_directory.to_str().is_none()
                || working_directory.components().count() > 256
                || working_directory.as_os_str().len() > 4096
                || home.as_ref().is_some_and(|path| {
                    !path.is_absolute() || path.to_str().is_none_or(|text| text.len() > 4096)
                })
            {
                return Err(cocoa_tools::ConfigurationError::Configuration);
            }
            Ok((working_directory, home))
        })
        .await
        .map_err(|_| CommandError::CodingSetup(cocoa_tools::ConfigurationError::Configuration))?
        .map_err(CommandError::CodingSetup)?;
        // A session with no retained model output adopts this configuration on
        // its next input, so its aliases resolve exactly as a new session's do.
        if !resumed || attachment.model_switch_allowed().await? {
            crate::catalogue::resolve_limits(&mut configuration.candidates).await;
            for (name, choices) in &configuration.candidates {
                if let Some(first) = choices.first() {
                    configuration.aliases.insert(name.clone(), first.clone());
                }
            }
        }
        let store = attachment
            .root_store_with_settings(configuration.store, configuration.effective_settings()?)?;
        let mcp = match mcp {
            Some(mcp) => mcp,
            None => config::mcp_manager()?,
        };
        mcp.configure(configuration.mcp_servers)?;
        mcp.set_search_provider(configuration.web_search)?;
        if !resumed {
            mcp.connect_all().await?;
        }
        let session_id = format!(
            "{}.{}",
            attachment.public_id().as_str(),
            i64::from(attachment.root_agent())
        );
        let environment = process_environment(&configuration.credential_env_names)?;
        let alias = configuration.alias.clone();
        let compaction = configuration
            .engine
            .compaction()
            .ok_or(config::ConfigError::Bounds)?;
        let provider = startup_model::StartupModel::new(
            configuration
                .candidates
                .get(&alias)
                .ok_or(config::ConfigError::Alias)?
                .clone(),
            &session_id,
            compaction.request_bytes,
            Some(store.clone()),
        )?;
        let input_bytes = configuration.engine.limits().input_bytes;
        let shutdown_timeout = configuration.engine.limits().shutdown_timeout;
        let (events, _) = EventPublisher::channel(64, 4096).map_err(CommandError::Events)?;
        let agents = AgentManager::attach(
            AgentLimits {
                active: configuration.agents_active,
                retained: configuration.agents_retained,
            },
            configuration.engine.limits(),
            Arc::new(agent_factory::LocalAgentFactory::new_durable(
                configuration.candidates,
                configuration.store,
                compaction,
                working_directory.clone(),
                environment.clone(),
                attachment.clone(),
                mcp.clone(),
            )),
            attachment,
        )
        .await
        .map_err(CommandError::Agents)?;
        let environment = async {
            let environment = LocalEnvironment::new(
                LocalConfiguration {
                    working_directory,
                    shell: "/bin/bash".into(),
                    environment,
                    output_directory: std::env::temp_dir(),
                },
                CodingLimits {
                    file_bytes: 1024 * 1024,
                    observed_bytes: 16 * 1024 * 1024,
                    observations: 256,
                    processes: 4,
                    captured_output_bytes: 1024 * 1024,
                    retained_output_bytes: 64 * 1024 * 1024,
                    preview_lines: 200,
                    default_timeout: Duration::from_secs(600),
                },
            )
            .await?;
            ProjectEnvironment::new(environment, home)
        }
        .await;
        let environment = match environment {
            Ok(environment) => {
                Arc::new(environment.with_instruction_acceptance(Arc::new(store.clone())))
            }
            Err(error) => {
                agents
                    .shutdown()
                    .await
                    .map_err(CommandError::AgentCleanup)?;
                return Err(CommandError::CodingSetup(error));
            }
        };
        let engine = async {
            let capabilities = agents
                .bind(
                    alias,
                    mcp.bind(Arc::new(CodingCapabilities::new(environment.clone()))),
                )
                .map_err(CommandError::Agents)?;
            let executor = QuickJsExecutor::new(QuickJsOptions {
                memory_bytes: 16 * 1024 * 1024,
                stack_bytes: 256 * 1024,
                feedback: cocoa_quickjs::FeedbackLimits {
                    output_bytes: 64 * 1024,
                    printed_bytes: 8 * 1024,
                    error_bytes: 2 * 1024,
                    print_calls: 32,
                },
            })
            .await
            .map_err(CommandError::Executor)?;
            let adapters = EngineAdapters {
                model: Arc::new(provider),
                executor: Box::new(executor),
                capabilities,
                context: agents.bind_context(environment.clone()),
                store: Arc::new(store),
            };
            if resumed {
                Engine::resume(configuration.engine, adapters, Some(events.clone()))
                    .await
                    .map_err(CommandError::Resume)
            } else {
                Ok(Engine::new(
                    configuration.engine,
                    adapters,
                    Some(events.clone()),
                ))
            }
        }
        .await;
        let engine = match engine {
            Ok(engine) => engine,
            Err(error) => {
                let agents_cleanup = agents.shutdown().await;
                let environment_cleanup = environment.close().await;
                agents_cleanup.map_err(CommandError::AgentCleanup)?;
                environment_cleanup.map_err(CommandError::CodingCleanup)?;
                return Err(error);
            }
        };
        Ok(Self {
            engine,
            events,
            agent_updates: agents.subscribe(),
            input_bytes,
            message_steering: configuration.message_steering,
            mcp,
            agents,
            environment,
            shutdown_timeout,
        })
    }

    pub async fn has_active_agents(&self) -> Result<bool, CommandError> {
        self.agents
            .has_active_agents()
            .await
            .map_err(CommandError::Agents)
    }

    /// Attempt engine, child-agent and coding-environment cleanup in that order.
    /// Earlier failures never skip later cleanup; environment cleanup is bounded
    /// by the configured shutdown timeout.
    ///
    /// # Errors
    /// Return the first cleanup error in that order, including an environment
    /// deadline expiry. An error does not confirm that all resources settled.
    pub async fn close(self) -> Result<(), CommandError> {
        let engine = self.engine.shutdown().await;
        let agents = self.agents.shutdown().await;
        let environment =
            tokio::time::timeout(self.shutdown_timeout, self.environment.close()).await;
        engine.map_err(CommandError::Shutdown)?;
        agents.map_err(CommandError::AgentCleanup)?;
        match environment {
            Ok(result) => result.map_err(CommandError::CodingCleanup),
            Err(_) => Err(CommandError::CodingCleanup(CapabilityError::Unsettled {
                message: "Coding environment cleanup did not settle before its deadline".into(),
            })),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum CommandError {
    #[error(transparent)]
    Persistence(#[from] cocoa::persistence::PersistenceError),
    #[error("cannot restore engine: {0}")]
    Resume(#[source] cocoa_core::engine::ResumeError),
    #[error(transparent)]
    Mcp(#[from] cocoa::mcp::McpError),
    #[error(transparent)]
    Config(#[from] config::ConfigError),
    #[error("selected process environment must be UTF-8 and at most 8192 bytes per value")]
    Environment,
    #[error("cannot initialize local coding tools")]
    CodingSetup(#[source] cocoa_tools::ConfigurationError),
    #[error("cannot initialize agent management")]
    Agents(#[source] CapabilityError),
    #[error("agent cleanup failed")]
    AgentCleanup(#[source] CapabilityError),
    #[error("local coding cleanup failed")]
    CodingCleanup(#[source] CapabilityError),
    #[error("cannot initialize live observations")]
    Events(#[source] cocoa_core::engine::EventSettingsError),
    #[error("JavaScript runtime setup failed")]
    Executor(#[source] cocoa_quickjs::QuickJsError),
    #[error("shutdown failed")]
    Shutdown(#[source] ShutdownError),
}

impl CommandError {
    /// Whether another session could overlap work whose termination is unconfirmed.
    pub fn cleanup_unconfirmed(&self) -> bool {
        matches!(
            self,
            Self::AgentCleanup(_)
                | Self::CodingCleanup(_)
                | Self::Shutdown(_)
                | Self::Executor(cocoa_quickjs::QuickJsError::Initialization { .. })
                | Self::Resume(cocoa_core::engine::ResumeError::Settlement { .. })
        )
    }
}

/// Caller-facing reason for one invocation failure, naming the deadline bound that fired.
pub(crate) fn run_description(error: &RunError) -> String {
    match error {
        RunError::Cancelled | RunError::TurnCancelled { .. } => "cancelled".to_owned(),
        RunError::Deadline { cause } => cause.to_string(),
        RunError::Settlement { .. } => "invocation and settlement failed".to_owned(),
        RunError::Model(_) => "model provider failed".to_owned(),
        RunError::Compaction(_) => "automatic compaction failed; history retained".to_owned(),
        RunError::InputLimit | RunError::OutputLimit | RunError::CellLimit | RunError::Budget => {
            "budget exhausted".to_owned()
        }
        RunError::Store(_) => "store failed".to_owned(),
        RunError::Execution(_) => "execution failed".to_owned(),
        RunError::Context(cocoa_core::context::ContextError::Unavailable { message }) => {
            message.clone()
        }
        RunError::Context(_) => "context assembly failed".to_owned(),
        RunError::Stopped => "engine stopped".to_owned(),
    }
}

fn process_environment(
    credential_names: &[String],
) -> Result<BTreeMap<String, String>, CommandError> {
    let mut environment = BTreeMap::new();
    for name in [
        "PATH",
        "HOME",
        "TMPDIR",
        "LANG",
        "LC_ALL",
        "CARGO_HOME",
        "RUSTUP_HOME",
        "RUSTUP_TOOLCHAIN",
    ] {
        if credential_names.iter().any(|credential| credential == name) {
            continue;
        }
        match std::env::var(name) {
            Ok(value) if value.len() <= 8192 => {
                environment.insert(name.into(), value);
            }
            Err(std::env::VarError::NotPresent) => {}
            Ok(_) | Err(std::env::VarError::NotUnicode(_)) => {
                return Err(CommandError::Environment);
            }
        }
    }
    environment
        .entry("PATH".into())
        .or_insert_with(|| "/usr/bin:/bin".into());
    Ok(environment)
}
