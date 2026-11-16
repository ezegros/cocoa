//! Startup-only model fallback, with concrete provider pinning after the first response.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use async_trait::async_trait;
use cocoa::persistence::SqliteStore;
use cocoa_core::model::{
    ModelError, ModelProvider, ModelRequest, ModelResponse, RequestAccounting, SummaryRequest,
};
use cocoa_llm::{ExecutionMode, Provider, ProviderKind, ProviderOptions};
use tokio_util::sync::CancellationToken;

use crate::config::{ConfigError, ResolvedAlias};

pub(crate) struct StartupModel {
    choices: Vec<Choice>,
    selected: AtomicUsize,
    pinned: AtomicBool,
    request_bytes: usize,
    store: Option<SqliteStore>,
    admission: tokio::sync::Mutex<()>,
}

struct Choice {
    settings: ResolvedAlias,
    provider: Provider,
}

impl StartupModel {
    /// Prepare the selected model and eligible fallbacks without making inference calls.
    /// Missing credentials skip only the affected choice, never its credential source.
    ///
    /// # Errors
    /// Reject an empty list or a list whose providers cannot initialize.
    pub fn new(
        candidates: Vec<ResolvedAlias>,
        session_id: &str,
        request_bytes: usize,
        store: Option<SqliteStore>,
    ) -> Result<Self, ConfigError> {
        let mut choices = Vec::with_capacity(candidates.len());
        let mut last_error = ConfigError::AliasChoices;
        for settings in candidates {
            match prepare(&settings, session_id) {
                Ok(provider) => choices.push(Choice { settings, provider }),
                Err(error) => last_error = error,
            }
        }
        if choices.is_empty() {
            return Err(last_error);
        }
        Ok(Self {
            choices,
            selected: AtomicUsize::new(0),
            pinned: AtomicBool::new(false),
            request_bytes,
            store,
            admission: tokio::sync::Mutex::new(()),
        })
    }

    fn current(&self) -> &Choice {
        &self.choices[self.selected.load(Ordering::Acquire)]
    }

    fn pin(&self) -> Result<(), ModelError> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        let settings = &self.current().settings;
        let profile = settings
            .effective_settings()
            .map_err(|_| ModelError::Protocol {
                message: "cannot pin startup model settings".into(),
            })?;
        let protocol = if settings.kind == ProviderKind::Anthropic {
            "anthropic-messages-v1"
        } else if settings.kind.is_chat_completions() {
            "chat-completions-v1"
        } else {
            "openai-responses-v1"
        };
        store
            .select_initial_model(profile, protocol)
            .map_err(|_| ModelError::Protocol {
                message: "cannot pin startup model settings".into(),
            })
    }
}

#[async_trait]
impl ModelProvider for StartupModel {
    async fn summarize_accounted(
        &self,
        request: SummaryRequest,
        context: &ModelRequest,
        input_bytes: usize,
        cancellation: CancellationToken,
        accounting: RequestAccounting,
    ) -> Result<String, ModelError> {
        self.current()
            .provider
            .summarize_accounted(request, context, input_bytes, cancellation, accounting)
            .await
    }

    fn request_bytes(&self, request: &ModelRequest) -> Result<Option<usize>, ModelError> {
        self.current().provider.request_bytes(request)
    }

    async fn summarize(
        &self,
        request: SummaryRequest,
        cancellation: CancellationToken,
    ) -> Result<String, ModelError> {
        self.current()
            .provider
            .summarize(request, cancellation)
            .await
    }

    async fn summarize_with_context(
        &self,
        request: SummaryRequest,
        context: &ModelRequest,
        input_bytes: usize,
        cancellation: CancellationToken,
    ) -> Result<String, ModelError> {
        self.current()
            .provider
            .summarize_with_context(request, context, input_bytes, cancellation)
            .await
    }

    async fn respond(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        self.respond_accounted(request, cancellation, RequestAccounting::default())
            .await
    }

    async fn respond_accounted(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
        accounting: RequestAccounting,
    ) -> Result<ModelResponse, ModelError> {
        let _admission = self
            .admission
            .try_lock()
            .map_err(|_| ModelError::Transport {
                message: "concurrent model request".into(),
            })?;
        let startup = !self.pinned.swap(true, Ordering::AcqRel);
        if !startup {
            let result = self
                .current()
                .provider
                .respond_accounted(request, cancellation, accounting)
                .await;
            if result.is_ok() {
                self.pin()?;
            }
            return result;
        }
        for _ in 0..self.choices.len() {
            if cancellation.is_cancelled() {
                return Err(ModelError::Cancelled);
            }
            let choice = self.current();
            if choice
                .provider
                .request_bytes(&request)?
                .is_some_and(|bytes| bytes > self.request_bytes)
            {
                return Err(ModelError::Protocol {
                    message: "startup model exceeds the encoded request budget".into(),
                });
            }
            let result = choice
                .provider
                .respond_accounted(request.clone(), cancellation.clone(), accounting.clone())
                .await;
            let selected = self.selected.load(Ordering::Acquire);
            if matches!(result, Err(ModelError::Rejected { .. }))
                && startup
                && selected + 1 < self.choices.len()
                && !cancellation.is_cancelled()
            {
                self.selected.store(selected + 1, Ordering::Release);
                continue;
            }
            if result.is_ok() {
                self.pin()?;
            }
            return result;
        }
        unreachable!("a bounded attempt either selects its next choice or returns")
    }
}

fn prepare(settings: &ResolvedAlias, session_id: &str) -> Result<Provider, ConfigError> {
    let credentials = settings.subscription_credentials()?;
    let options = ProviderOptions {
        kind: settings.kind,
        endpoint: settings.endpoint.clone(),
        model: settings.model.clone(),
        api_key: settings.resolve_api_key()?,
        execution: ExecutionMode::Enabled,
        thinking_level: settings.thinking_level,
        session_id: session_id.into(),
    };
    let mut provider = match credentials {
        Some(credentials) => Provider::chatgpt(options, credentials),
        None => Provider::new(options),
    }
    .map_err(ConfigError::AliasOptions)?;
    provider
        .set_model_limits(settings.model_limits)
        .map_err(ConfigError::AliasOptions)?;
    if settings.reasoning_summary {
        provider
            .enable_reasoning_summary()
            .map_err(ConfigError::AliasOptions)?;
    }
    Ok(provider)
}
