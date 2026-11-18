//! Private production adapters for depth-one, REPL-spawned child agents.

use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

use async_trait::async_trait;
use cocoa::agents::{AgentFactory, AgentParticipant, AgentSelection};
use cocoa_core::{
    capability::{CapabilityError, Effects},
    compaction::CompactionPolicy,
    engine::{Engine, EngineAdapters, EngineSettings, Limits},
    store::{AgentId, InMemoryStore, Store, StoreLimits},
};
use cocoa_quickjs::{QuickJsError, QuickJsExecutor, QuickJsOptions};
use cocoa_tools::{
    CodingCapabilities, CodingLimits, LocalConfiguration, LocalEnvironment, ProjectEnvironment,
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::config::ResolvedAlias;

/// Captured alias choices and the live caller profile used for child selection.
pub struct AgentModels {
    pub aliases: BTreeMap<String, Vec<ResolvedAlias>>,
    pub caller: Arc<crate::session::startup_model::StartupModel>,
}

/// Each child owns its provider, JavaScript heap, history and coding environment.
/// Child capabilities exclude agent management. History and coding environments
/// outlive finished engines, preserving evidence and output until session closure.
/// The manager must shut down each engine before calling `close`.
pub struct LocalAgentFactory {
    aliases: BTreeMap<String, Vec<ResolvedAlias>>,
    caller: Arc<crate::session::startup_model::StartupModel>,
    store: StoreLimits,
    compaction: CompactionPolicy,
    working_directory: PathBuf,
    environment: BTreeMap<String, String>,
    session_id: String,
    mcp: cocoa::mcp::McpManager,
    children: Mutex<BTreeMap<AgentId, OwnedEnvironment>>,
}

impl LocalAgentFactory {
    /// Capture startup settings without rereading config or resolving child keys.
    /// Configured credential variables are excluded from every child's shell.
    /// The manager supplies admission limits; this registry also caps ownership
    /// at 32 children, including environments awaiting a cleanup retry.
    #[must_use]
    pub fn new(
        models: AgentModels,
        store: StoreLimits,
        compaction: CompactionPolicy,
        working_directory: PathBuf,
        mut environment: BTreeMap<String, String>,
        session_id: String,
        mcp: cocoa::mcp::McpManager,
    ) -> Self {
        for alias in models.aliases.values().flatten() {
            if let Some(reference) = alias.credential_env() {
                environment.remove(reference);
            }
        }
        Self {
            aliases: models.aliases,
            caller: models.caller,
            store,
            compaction,
            working_directory,
            environment,
            session_id,
            mcp,
            children: Mutex::new(BTreeMap::new()),
        }
    }

    /// Capture production settings using the committed session identity. Child
    /// histories are bound through their admitted participant, never their name.
    #[must_use]
    pub fn new_durable(
        models: AgentModels,
        store: StoreLimits,
        compaction: CompactionPolicy,
        working_directory: PathBuf,
        environment: BTreeMap<String, String>,
        attachment: cocoa::persistence::SessionAttachment,
        mcp: cocoa::mcp::McpManager,
    ) -> Self {
        Self::new(
            models,
            store,
            compaction,
            working_directory,
            environment,
            attachment.public_id().as_str().into(),
            mcp,
        )
    }

    fn selected_candidates(
        &self,
        selection: &AgentSelection,
    ) -> Result<Vec<ResolvedAlias>, CapabilityError> {
        crate::config::agent_candidates(self.caller.current_settings(), &self.aliases, selection)
            .map_err(|_| setup_failed("Invalid child model, thinking level or alias selection"))
    }
}

#[async_trait]
impl AgentFactory for LocalAgentFactory {
    fn supports_alias(&self, alias: &str) -> bool {
        self.aliases.contains_key(alias)
    }

    fn validate_selection(
        &self,
        _default_alias: &str,
        selection: &AgentSelection,
    ) -> Result<(), CapabilityError> {
        self.selected_candidates(selection).map(|_| ())
    }

    async fn create(
        &self,
        participant: AgentParticipant,
        selection: &AgentSelection,
        limits: Limits,
        cancellation: CancellationToken,
    ) -> Result<Engine, CapabilityError> {
        let id = participant.id();
        let mut children = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(CapabilityError::Cancelled),
            children = self.children.lock() => children,
        };
        if cancellation.is_cancelled() {
            return Err(CapabilityError::Cancelled);
        }
        if children.len() >= 32 || children.contains_key(&id) {
            return Err(setup_failed(
                "Child agent ownership limit or identity conflict",
            ));
        }
        let alias = self.selected_candidates(selection)?;
        if self.session_id.is_empty()
            || self.session_id.len() > 108
            || !self
                .session_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
        {
            return Err(setup_failed("Invalid child agent session identity"));
        }
        let session_id = format!("{}.{}", self.session_id, i64::from(id));
        debug_assert!(
            session_id.len() <= 128 && session_id != self.session_id,
            "child provider identities must be bounded and distinct from the root"
        );
        let settings = EngineSettings::new(limits)
            .and_then(|settings| settings.with_compaction(self.compaction))
            .map_err(|_| setup_failed("Invalid child engine limits"))?;
        let durable_store = participant
            .durable_store(self.store)
            .await
            .map_err(|_| setup_failed("Cannot bind child history"))?;
        let store: Arc<dyn Store> = match &durable_store {
            Some(store) => Arc::new(store.clone()),
            None => Arc::new(
                InMemoryStore::new(self.store)
                    .map_err(|_| setup_failed("Cannot initialize child history"))?,
            ),
        };
        let working_directory = self.working_directory.clone();
        let request_bytes = self.compaction.request_bytes;
        let (provider, working_directory, home) = tokio::task::spawn_blocking(move || {
            let working_directory = std::fs::canonicalize(working_directory)
                .map_err(|_| setup_failed("Cannot prepare child working directory"))?;
            let home = std::env::home_dir();
            // ProjectEnvironment consumes its local backend even on validation failure.
            // Check its path constraints before allocating that backend's output directory.
            if !working_directory.is_dir()
                || working_directory.to_str().is_none()
                || working_directory.components().count() > 256
                || working_directory.as_os_str().len() > 4096
                || home.as_ref().is_some_and(|path| {
                    !path.is_absolute() || path.to_str().is_none_or(|text| text.len() > 4096)
                })
            {
                return Err(setup_failed("Invalid child project paths"));
            }
            let provider = crate::session::startup_model::StartupModel::new(
                alias,
                &session_id,
                request_bytes,
                None,
            )
            .map_err(|_| {
                setup_failed("Cannot initialize child provider credentials or settings")
            })?;
            Ok((provider, working_directory, home))
        })
        .await
        .map_err(|_| unsettled())??;
        if cancellation.is_cancelled() {
            return Err(CapabilityError::Cancelled);
        }
        // Finish constructors after cancellation rather than dropping their owned workers.
        let local = LocalEnvironment::new(
            LocalConfiguration {
                working_directory,
                shell: "/bin/bash".into(),
                environment: self.environment.clone(),
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
        .await
        .map_err(|_| setup_failed("Cannot initialize child coding tools"))?;
        let environment = ProjectEnvironment::new(local, home).map_err(|_| unsettled())?;
        let environment = Arc::new(match durable_store {
            Some(store) => environment.with_instruction_acceptance(Arc::new(store)),
            None => environment,
        });
        children.insert(
            id,
            OwnedEnvironment {
                environment: environment.clone(),
                _store: store.clone(),
                shutdown_timeout: limits.shutdown_timeout,
                cleanup: EnvironmentCleanup::AfterEngineShutdown,
            },
        );
        if cancellation.is_cancelled() {
            close_environment(&environment, limits.shutdown_timeout).await?;
            children.remove(&id);
            return Err(CapabilityError::Cancelled);
        }
        let executor = match QuickJsExecutor::new(QuickJsOptions {
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
        {
            Ok(executor) => executor,
            Err(QuickJsError::Initialization { .. }) => {
                children.get_mut(&id).ok_or_else(unsettled)?.cleanup =
                    EnvironmentCleanup::UnconfirmedWorker;
                return Err(unsettled());
            }
            Err(
                QuickJsError::InvalidOptions
                | QuickJsError::WorkerStart { .. }
                | QuickJsError::InitializationSettled { .. },
            ) => {
                close_environment(&environment, limits.shutdown_timeout).await?;
                children.remove(&id);
                return Err(setup_failed("Cannot initialize child JavaScript runtime"));
            }
        };
        // Transfer completed setup even after cancellation so the supervisor owns engine cleanup.
        Ok(Engine::new(
            settings,
            EngineAdapters {
                model: Arc::new(provider),
                executor: Box::new(executor),
                capabilities: participant.bind(
                    self.mcp
                        .bind(Arc::new(CodingCapabilities::new(environment.clone()))),
                ),
                context: participant.bind_context(environment.clone()),
                store,
            },
            participant.events(),
        ))
    }

    async fn close(&self, _name: &str) -> Result<(), CapabilityError> {
        Err(setup_failed(
            "Child resource cleanup requires its admitted identity",
        ))
    }

    async fn close_agent(&self, agent: AgentId, _name: &str) -> Result<(), CapabilityError> {
        let mut children = self.children.lock().await;
        if let Some(child) = children.get(&agent) {
            if matches!(child.cleanup, EnvironmentCleanup::UnconfirmedWorker) {
                return Err(unsettled());
            }
            close_environment(&child.environment, child.shutdown_timeout).await?;
            children.remove(&agent);
        }
        Ok(())
    }
}

struct OwnedEnvironment {
    environment: Arc<ProjectEnvironment>,
    _store: Arc<dyn Store>,
    shutdown_timeout: Duration,
    cleanup: EnvironmentCleanup,
}

enum EnvironmentCleanup {
    AfterEngineShutdown,
    UnconfirmedWorker,
}

async fn close_environment(
    environment: &ProjectEnvironment,
    timeout: Duration,
) -> Result<(), CapabilityError> {
    match tokio::time::timeout(timeout, environment.close()).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) | Err(_) => Err(unsettled()),
    }
}

fn setup_failed(message: &str) -> CapabilityError {
    CapabilityError::Tool {
        code: "setup_failed".into(),
        message: message.into(),
        effects: Effects::None,
    }
}

fn unsettled() -> CapabilityError {
    CapabilityError::Unsettled {
        message: "Child agent setup or owned cleanup could not be confirmed".into(),
    }
}
