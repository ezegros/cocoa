//! Sign in with ChatGPT policy. API-key OpenAI and other services never share this store.

use std::path::PathBuf;

use async_trait::async_trait;
use cocoa_core::model::ModelError;
use cocoa_llm::CredentialResolver;
use cocoa_oauth::{OAuthClient, OAuthError, Service};
use secrecy::SecretString;

pub const MANAGE_USAGE: &str = "https://chatgpt.com/settings/usage";

/// Construct the pinned SIWC service with separate owner-only registration storage.
/// An explicit path is for application-owned storage, never a server override.
///
/// # Errors
/// Reject missing home directories or invalid service initialization.
pub fn client(path: Option<PathBuf>) -> Result<OAuthClient, OAuthError> {
    let path = path
        .or_else(|| std::env::home_dir().map(|home| home.join(".config/cocoa/oauth/chatgpt.json")))
        .ok_or(OAuthError::Storage)?;
    OAuthClient::new(
        Service {
            issuer: "https://auth.openai.com".into(),
            resource: "https://api.openai.com/v1".into(),
            scopes: "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct"
                .into(),
            inference_scope: "chatgpt.tokens.use.direct".into(),
            registration_client: "dynamic_agent_client".into(),
            registration_parameters: [("agent_name_hint".into(), "Cocoa".into())].into(),
            host_id_parameter: Some("ext_agent_host_id".into()),
        },
        path,
    )
}

/// A runtime session's fixed registration. Tokens resolve from authoritative
/// storage on every request, including requests from separately owned children.
pub struct Credentials {
    client: OAuthClient,
    registration: String,
}

impl Credentials {
    /// Capture a registration identifier without validation or storage access.
    /// Token resolution checks it against the saved registration on each request.
    #[must_use]
    pub fn new(client: OAuthClient, registration: String) -> Self {
        Self {
            client,
            registration,
        }
    }
}

#[async_trait]
impl CredentialResolver for Credentials {
    fn identity(&self) -> &str {
        &self.registration
    }

    async fn resolve(&self) -> Result<SecretString, ModelError> {
        self.client
            .access_token(&self.registration)
            .await
            .map_err(|error| ModelError::Transport {
                message: error.to_string(),
            })
    }
}
