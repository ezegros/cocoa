//! Resource-bound OAuth for HTTP MCP. Shares browser, PKCE and protected storage
//! with subscription login, without imposing OIDC identity or inference scopes.

use std::{path::PathBuf, time::Duration};

use base64::{Engine, prelude::BASE64_URL_SAFE_NO_PAD};
use reqwest::{Client, Url};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::{
    OAuthError, REFRESHES, RESOLUTION_PERMITS, bounded_json, browser, now, random, storage,
    storage::LockedStore, trusted_url,
};

/// A public OAuth client for one exact MCP endpoint and credential file.
#[derive(Clone)]
pub struct McpOAuthClient {
    endpoint: Url,
    path: PathBuf,
    client_id: Option<String>,
    http: Client,
}

/// One bounded, one-use browser authorization. Dropping it closes the callback.
pub struct McpLogin {
    client: McpOAuthClient,
    listener: TcpListener,
    redirect: String,
    state: String,
    verifier: String,
    authorization: SecretString,
    registration: Registration,
}

#[derive(Serialize, Deserialize)]
struct Stored {
    endpoint: String,
    registration: Option<Registration>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Registration {
    generation: String,
    resource: String,
    issuer: String,
    token_endpoint: String,
    client_id: String,
    scopes: String,
    tokens: Option<Tokens>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Tokens {
    access_token: String,
    refresh_token: Option<String>,
    expires_at: Option<u64>,
}

#[derive(Deserialize)]
struct ProtectedResource {
    resource: String,
    authorization_servers: Vec<String>,
    #[serde(default)]
    scopes_supported: Vec<String>,
}

#[derive(Deserialize)]
struct Metadata {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: Option<String>,
    #[serde(default)]
    code_challenge_methods_supported: Vec<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    token_type: String,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
}

impl McpOAuthClient {
    /// Configure a public client. With no pre-registered client ID, discovery must
    /// advertise dynamic registration. Only HTTPS or literal loopback HTTP is allowed.
    ///
    /// # Errors
    /// Reject unsafe endpoints, client IDs or HTTP client initialization failure.
    pub fn new(
        endpoint: &str,
        path: PathBuf,
        client_id: Option<String>,
    ) -> Result<Self, OAuthError> {
        let endpoint = safe_url(endpoint)?;
        if client_id.as_ref().is_some_and(|value| !text(value, 2048)) {
            return Err(OAuthError::Configuration);
        }
        let http = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(Duration::from_secs(15))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .map_err(|_| OAuthError::Configuration)?;
        Ok(Self {
            endpoint,
            path,
            client_id,
            http,
        })
    }

    /// Discover resource and issuer metadata, then bind a loopback PKCE callback.
    /// Metadata hints come only from this resource's authentication challenge.
    /// No browser is opened until the caller explicitly requests it.
    ///
    /// # Errors
    /// Reject mismatched resource/issuer, missing S256, registration or storage failure.
    pub async fn begin(&self, metadata_hint: Option<&str>) -> Result<McpLogin, OAuthError> {
        let resource: ProtectedResource = if let Some(hint) = metadata_hint {
            self.get(safe_url(hint)?).await?
        } else {
            let mut url = self.endpoint.clone();
            url.set_query(None);
            url.set_path(&format!(
                "/.well-known/oauth-protected-resource{}",
                self.endpoint.path().trim_end_matches('/')
            ));
            match self.get(url.clone()).await {
                Ok(resource) => resource,
                Err(_) => {
                    url.set_path("/.well-known/oauth-protected-resource");
                    self.get(url).await?
                }
            }
        };
        let audience = safe_url(&resource.resource)?;
        let audience_path = audience.path().trim_end_matches('/');
        if audience.origin() != self.endpoint.origin()
            || !(self.endpoint.path() == audience_path
                || self
                    .endpoint
                    .path()
                    .starts_with(&format!("{audience_path}/")))
            || audience.query().is_some()
            || resource.authorization_servers.is_empty()
            || resource.authorization_servers.len() > 8
            || resource.scopes_supported.len() > 64
            || resource
                .scopes_supported
                .iter()
                .any(|scope| !text(scope, 256) || scope.contains(' '))
        {
            return Err(OAuthError::Configuration);
        }
        let issuer = safe_url(&resource.authorization_servers[0])?;
        if issuer.query().is_some() {
            return Err(OAuthError::Configuration);
        }
        let mut discovery = issuer.clone();
        discovery.set_path(&format!(
            "/.well-known/oauth-authorization-server{}",
            issuer.path().trim_end_matches('/')
        ));
        let metadata: Metadata = match self.get(discovery).await {
            Ok(metadata) => metadata,
            Err(_) => {
                let mut discovery = issuer.clone();
                discovery.set_path(&format!(
                    "{}/.well-known/openid-configuration",
                    issuer.path().trim_end_matches('/')
                ));
                self.get(discovery).await?
            }
        };
        if metadata.issuer.trim_end_matches('/') != issuer.as_str().trim_end_matches('/')
            || !metadata
                .code_challenge_methods_supported
                .iter()
                .any(|method| method == "S256")
        {
            return Err(OAuthError::Configuration);
        }
        let mut authorization = safe_url(&metadata.authorization_endpoint)?;
        let token_endpoint = safe_url(&metadata.token_endpoint)?.to_string();
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|_| OAuthError::Listener)?;
        let port = listener
            .local_addr()
            .map_err(|_| OAuthError::Listener)?
            .port();
        let redirect = format!("http://127.0.0.1:{port}/auth/callback");
        let lock = LockedStore::acquire(&self.path).await?;
        let mut store = self.read_locked(&lock)?;
        let saved = store.registration.as_ref().filter(|saved| {
            saved.issuer == metadata.issuer
                && saved.resource == resource.resource
                && saved.token_endpoint == token_endpoint
                && self
                    .client_id
                    .as_ref()
                    .is_none_or(|id| id == &saved.client_id)
        });
        let client_id = match self
            .client_id
            .as_ref()
            .or_else(|| saved.map(|saved| &saved.client_id))
        {
            Some(client_id) => client_id.clone(),
            None => {
                let registration = safe_url(
                    metadata
                        .registration_endpoint
                        .as_deref()
                        .ok_or(OAuthError::Configuration)?,
                )?;
                let response = self
                    .http
                    .post(registration)
                    .json(&serde_json::json!({
                        "client_name":"Cocoa", "redirect_uris":[redirect],
                        "grant_types":["authorization_code", "refresh_token"],
                        "response_types":["code"], "token_endpoint_auth_method":"none"
                    }))
                    .send()
                    .await
                    .map_err(|_| OAuthError::Network)?;
                if !response.status().is_success() {
                    return Err(OAuthError::Response);
                }
                let value: serde_json::Value = bounded_json(response).await?;
                value["client_id"]
                    .as_str()
                    .filter(|id| text(id, 2048))
                    .ok_or(OAuthError::Response)?
                    .to_owned()
            }
        };
        let registration = Registration {
            generation: random()?,
            resource: resource.resource,
            issuer: metadata.issuer,
            token_endpoint,
            client_id,
            scopes: resource.scopes_supported.join(" "),
            tokens: saved.and_then(|saved| saved.tokens.clone()),
        };
        store.registration = Some(registration.clone());
        lock.save(&store)?;
        let state = random()?;
        let verifier = random()?;
        authorization.query_pairs_mut().extend_pairs([
            ("response_type", "code"),
            ("client_id", &registration.client_id),
            ("redirect_uri", &redirect),
            ("resource", &registration.resource),
            ("scope", &registration.scopes),
            ("state", &state),
            ("code_challenge_method", "S256"),
            (
                "code_challenge",
                &BASE64_URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
            ),
        ]);
        Ok(McpLogin {
            client: self.clone(),
            listener,
            redirect,
            state,
            verifier,
            authorization: authorization.to_string().into(),
            registration,
        })
    }

    /// Resolve a saved grant, settling refresh rotation even if the waiter is dropped.
    /// `None` means no local grant; tokens are never sent during discovery.
    /// At most 64 resolutions are retained across both OAuth clients in this process.
    ///
    /// # Errors
    /// Reject unsafe storage, changed resource/client, expired grants or failed refresh.
    /// Return [`OAuthError::Busy`] immediately when the process-wide limit is reached.
    pub async fn access_token(&self) -> Result<Option<SecretString>, OAuthError> {
        let permit = RESOLUTION_PERMITS
            .try_acquire()
            .map_err(|_| OAuthError::Busy)?;
        let client = self.clone();
        REFRESHES
            .get_or_init(TaskTracker::new)
            .spawn(async move {
                let _permit = permit;
                tokio::time::timeout(Duration::from_secs(120), client.resolve())
                    .await
                    .map_err(|_| OAuthError::Network)?
            })
            .await
            .map_err(|_| OAuthError::Network)?
    }

    /// Forget local MCP tokens and registration. Does not claim remote revocation.
    ///
    /// # Errors
    /// Reject unsafe or unavailable storage.
    pub async fn logout(&self) -> Result<(), OAuthError> {
        let lock = LockedStore::acquire(&self.path).await?;
        let mut store = self.read_locked(&lock)?;
        store.registration = None;
        lock.save(&store)
    }

    async fn resolve(&self) -> Result<Option<SecretString>, OAuthError> {
        if storage::read::<Stored>(&self.path)?.is_none() {
            return Ok(None);
        }
        let lock = LockedStore::acquire(&self.path).await?;
        let mut store = self.read_locked(&lock)?;
        let Some(registration) = &mut store.registration else {
            return Ok(None);
        };
        let Some(tokens) = &registration.tokens else {
            return Ok(None);
        };
        if tokens
            .expires_at
            .is_none_or(|expiry| now().is_ok_and(|now| now.saturating_add(30) < expiry))
        {
            return Ok(Some(tokens.access_token.clone().into()));
        }
        let refresh = tokens
            .refresh_token
            .as_deref()
            .ok_or(OAuthError::Reauthorize)?;
        let result = self
            .token(
                registration,
                &[("grant_type", "refresh_token"), ("refresh_token", refresh)],
            )
            .await;
        match result {
            Ok(response) => {
                registration.tokens = Some(response.tokens(tokens.refresh_token.clone())?);
                lock.save(&store)?;
                Ok(store
                    .registration
                    .and_then(|registration| registration.tokens)
                    .map(|tokens| tokens.access_token.into()))
            }
            Err(OAuthError::Reauthorize) => {
                registration.tokens = None;
                lock.save(&store)?;
                Err(OAuthError::Reauthorize)
            }
            Err(error) => Err(error),
        }
    }

    fn read_locked(&self, lock: &LockedStore) -> Result<Stored, OAuthError> {
        let store = lock.read::<Stored>()?.unwrap_or_else(|| Stored {
            endpoint: self.endpoint.to_string(),
            registration: None,
        });
        if store.endpoint != self.endpoint.as_str()
            || store.registration.as_ref().is_some_and(|registration| {
                self.client_id
                    .as_ref()
                    .is_some_and(|id| id != &registration.client_id)
                    || safe_url(&registration.token_endpoint).is_err()
            })
        {
            return Err(OAuthError::Changed);
        }
        Ok(store)
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, url: Url) -> Result<T, OAuthError> {
        let response = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|_| OAuthError::Network)?;
        if !response.status().is_success() {
            return Err(OAuthError::Response);
        }
        bounded_json(response).await
    }

    async fn token(
        &self,
        registration: &Registration,
        fields: &[(&str, &str)],
    ) -> Result<TokenResponse, OAuthError> {
        let mut form = fields.to_vec();
        form.extend([
            ("client_id", registration.client_id.as_str()),
            ("resource", registration.resource.as_str()),
        ]);
        let response = self
            .http
            .post(&registration.token_endpoint)
            .form(&form)
            .send()
            .await
            .map_err(|_| OAuthError::Network)?;
        if !response.status().is_success() {
            let status = response.status();
            let value: serde_json::Value = bounded_json(response).await?;
            return Err(
                if status.as_u16() == 400 && value["error"] == "invalid_grant" {
                    OAuthError::Reauthorize
                } else {
                    OAuthError::Response
                },
            );
        }
        bounded_json(response).await
    }
}

impl McpLogin {
    /// Secret URL for an application-owned browser opener. Do not log it.
    #[must_use]
    pub fn authorization_url(&self) -> &SecretString {
        &self.authorization
    }

    /// Open the same bounded system-browser launcher used for subscription login.
    ///
    /// # Errors
    /// Report failed or timed-out launch.
    pub async fn open_browser(&self) -> Result<(), OAuthError> {
        browser::open_url(self.authorization.expose_secret()).await
    }

    /// Exchange the one-use, state-checked callback and atomically save this grant.
    ///
    /// # Errors
    /// Reject denied, invalid, stale or cancelled authorization. Expires in five minutes.
    pub async fn finish(self, cancellation: CancellationToken) -> Result<(), OAuthError> {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => Err(OAuthError::Cancelled),
            result = tokio::time::timeout(Duration::from_secs(300), self.complete()) => result.map_err(|_| OAuthError::Cancelled)?,
        }
    }

    async fn complete(mut self) -> Result<(), OAuthError> {
        let code = browser::receive_callback(&self.listener, |request| {
            let parameters = browser::callback_parameters(request, &self.redirect, &self.state)?;
            if parameters.contains_key("error") {
                return Some(Err(OAuthError::Denied));
            }
            if parameters
                .get("iss")
                .is_some_and(|issuer| issuer != &self.registration.issuer)
            {
                return Some(Err(OAuthError::Changed));
            }
            let code = parameters.get("code").filter(|code| text(code, 4096))?;
            Some(Ok(code.clone()))
        })
        .await?;
        let response = self
            .client
            .token(
                &self.registration,
                &[
                    ("grant_type", "authorization_code"),
                    ("code", &code),
                    ("code_verifier", &self.verifier),
                    ("redirect_uri", &self.redirect),
                ],
            )
            .await?;
        let tokens = response.tokens(None)?;
        let lock = LockedStore::acquire(&self.client.path).await?;
        let mut store = self.client.read_locked(&lock)?;
        if store
            .registration
            .as_ref()
            .is_none_or(|saved| saved.generation != self.registration.generation)
        {
            return Err(OAuthError::Changed);
        }
        self.registration.tokens = Some(tokens);
        store.registration = Some(self.registration);
        lock.save(&store)
    }
}

impl TokenResponse {
    fn tokens(self, previous_refresh: Option<String>) -> Result<Tokens, OAuthError> {
        if !self.token_type.eq_ignore_ascii_case("bearer")
            || !text(&self.access_token, 16384)
            || self
                .refresh_token
                .as_ref()
                .is_some_and(|token| !text(token, 16384))
            || self
                .expires_in
                .is_some_and(|seconds| seconds == 0 || seconds > 366 * 86400)
        {
            return Err(OAuthError::Response);
        }
        Ok(Tokens {
            access_token: self.access_token,
            refresh_token: self.refresh_token.or(previous_refresh),
            expires_at: self
                .expires_in
                .map(|seconds| now().map(|now| now.saturating_add(seconds)))
                .transpose()?,
        })
    }
}

fn safe_url(value: &str) -> Result<Url, OAuthError> {
    let url = Url::parse(value).map_err(|_| OAuthError::Configuration)?;
    if value.len() > 4096 || !trusted_url(&url) || url.fragment().is_some() {
        return Err(OAuthError::Configuration);
    }
    Ok(url)
}

fn text(value: &str, limit: usize) -> bool {
    !value.is_empty() && value.len() <= limit && value.bytes().all(|byte| byte.is_ascii_graphic())
}
