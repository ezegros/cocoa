//! OpenAI Responses construction API retained for existing callers.
//! Provider-neutral construction uses [`crate::Provider`] and [`crate::ProviderOptions`].

use reqwest::{Client, Url};
use secrecy::SecretString;

use crate::{ProviderError, ProviderKind, ProviderOptions};

pub use crate::transport::{ExecutionMode, ThinkingLevel, Transport as OpenAiProvider};

/// Explicit construction inputs. No credential or configuration discovery occurs.
pub struct OpenAiOptions {
    /// Complete URL with exactly the `/v1/responses` path, at most 2048 bytes.
    /// HTTPS is required except HTTP to a literal loopback IP. URL credentials,
    /// query strings and fragments are rejected.
    pub endpoint: String,
    /// Nonblank model identifier, at most 256 bytes, without control characters.
    pub model: String,
    /// Bearer token of 1..=8192 graphic ASCII bytes, never included in diagnostics.
    pub api_key: SecretString,
    pub execution: ExecutionMode,
    /// Omit `reasoning` when absent, preserving the provider's model default.
    /// `Some(ThinkingLevel::None)` instead requests the explicit `none` level.
    pub thinking_level: Option<ThinkingLevel>,
}

impl OpenAiProvider {
    /// Validate OpenAI options and build the reusable client synchronously.
    ///
    /// # Errors
    /// Reject invalid options or client initialization failure without exposing
    /// credentials, endpoint contents or underlying transport diagnostics.
    pub fn new(options: OpenAiOptions) -> Result<Self, OpenAiError> {
        if options.endpoint.len() > 2048
            || !Url::parse(&options.endpoint)
                .is_ok_and(|endpoint| endpoint.path() == "/v1/responses")
        {
            return Err(OpenAiError::InvalidOptions);
        }
        Self::new_profile(
            ProviderOptions {
                kind: ProviderKind::OpenAi,
                endpoint: options.endpoint,
                model: options.model,
                api_key: Some(options.api_key),
                execution: options.execution,
                thinking_level: options.thinking_level,
                session_id: "legacy-openai".into(),
            },
            None,
            Client::builder(),
        )
        .map_err(|error| match error {
            ProviderError::Client => OpenAiError::Client,
            _ => OpenAiError::InvalidOptions,
        })
    }
}

/// Sanitized setup errors. Runtime failures use the core's `ModelError`.
#[derive(Debug, thiserror::Error)]
pub enum OpenAiError {
    #[error("invalid OpenAI options")]
    InvalidOptions,
    #[error("OpenAI client initialization failed")]
    Client,
}
