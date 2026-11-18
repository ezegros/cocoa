//! Browser PKCE/OIDC sessions with service-bound registrations and protected persistence.
//!
//! Each service supplies its own issuer, audience, permissions and registration policy.
//! Discovery endpoints must remain on the configured issuer's origin. Storage is
//! plaintext, owner-only, and atomic, not encrypted or isolated from the same OS user.
//! A stable file lock serializes rotation, selection, login commits and logout across
//! processes. Started refreshes finish independently of any inference waiter's lifetime.

mod browser;
pub mod mcp;
mod storage;

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::OnceLock,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{Engine, prelude::BASE64_URL_SAFE_NO_PAD};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
use reqwest::{Client, Url};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use storage::LockedStore;
use tokio::sync::Semaphore;
use tokio_util::task::TaskTracker;

pub use browser::{BrowserLogin, open_link};

static REFRESHES: OnceLock<TaskTracker> = OnceLock::new();
static RESOLUTION_PERMITS: Semaphore = Semaphore::const_new(64);

/// Service policy supplied by trusted application code, never by remote discovery.
/// Issuer endpoints use HTTPS, except literal loopback HTTP for local services/tests.
#[derive(Clone)]
pub struct Service {
    pub issuer: String,
    pub resource: String,
    pub scopes: String,
    pub inference_scope: String,
    /// Registration entrypoint, not an issued client identity.
    pub registration_client: String,
    /// Additional parameters used only for new client registration, not reconnect.
    pub registration_parameters: BTreeMap<String, String>,
    /// Service-specific parameter receiving the installation's stable opaque ID.
    pub host_id_parameter: Option<String>,
}

/// Reusable service client. Clones share the authoritative store, not token snapshots.
#[derive(Clone)]
pub struct OAuthClient {
    service: Service,
    path: PathBuf,
    http: Client,
}

/// Safe account-picker data. Tokens and authorization URLs never enter this type.
#[derive(Clone, Debug)]
pub struct Account {
    pub registration: String,
    pub label: String,
    pub active: bool,
    pub connected: bool,
    pub inference_enabled: bool,
}

/// Sanitized errors never include token responses, callback parameters or URLs.
#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    #[error("OAuth configuration is invalid")]
    Configuration,
    #[error("OAuth storage unavailable; require owner-only directory and regular 0600 files")]
    Storage,
    #[error("OAuth storage is busy; retry later")]
    Busy,
    #[error("OAuth provider is unavailable; saved credentials were preserved")]
    Network,
    #[error("OAuth provider returned an invalid response")]
    Response,
    #[error("OAuth identity signature, issuer, audience, expiry or nonce did not verify")]
    Identity,
    #[error("Sign-in returned a different account; reconnect the original or add an account")]
    AccountMismatch,
    #[error("Sign-in was cancelled or timed out")]
    Cancelled,
    #[error("Sign-in was denied")]
    Denied,
    #[error("Cannot open the system browser")]
    Browser,
    #[error("Cannot listen for the local OAuth callback")]
    Listener,
    #[error("OAuth registration changed; retry sign-in or start a new session")]
    Changed,
    #[error("Sign in or reconnect the selected account")]
    Reauthorize,
    #[error("Identity-only sign-in; the required inference permission was not granted")]
    Scope,
    #[error(
        "Local credentials removed; remote revocation unconfirmed. Disconnect the app in provider settings"
    )]
    Revocation,
}

/// Finish credential resolutions started in this process, including those whose
/// inference waiters were cancelled. Call after stopping all request producers and
/// before dropping their Tokio runtimes. Each resolution has a two-minute bound.
pub async fn finish_refreshes() {
    if let Some(refreshes) = REFRESHES.get() {
        refreshes.close();
        refreshes.wait().await;
    }
}

impl OAuthClient {
    /// Bind a store to an explicit trusted service. Does not access credentials or network.
    ///
    /// # Errors
    /// Reject unsafe issuer URLs, invalid policy or HTTP client initialization failure.
    pub fn new(service: Service, path: PathBuf) -> Result<Self, OAuthError> {
        let issuer = Url::parse(&service.issuer).map_err(|_| OAuthError::Configuration)?;
        if !trusted_url(&issuer)
            || issuer.path() != "/"
            || issuer.query().is_some()
            || issuer.fragment().is_some()
            || service.issuer.ends_with('/')
            || service.resource.is_empty()
            || service.scopes.len() > 4096
            || !service
                .scopes
                .split_whitespace()
                .any(|scope| scope == service.inference_scope)
            || !identifier(&service.registration_client)
            || service.registration_parameters.len() > 16
            || service
                .registration_parameters
                .iter()
                .any(|(key, value)| !extension_parameter(key) || value.len() > 256)
            || service.host_id_parameter.as_ref().is_some_and(|key| {
                !extension_parameter(key) || service.registration_parameters.contains_key(key)
            })
        {
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
            service,
            path,
            http,
        })
    }

    /// Read safe saved registrations without resolving or refreshing any token.
    ///
    /// # Errors
    /// Reject unsafe, malformed or differently service-bound storage.
    pub fn accounts(&self) -> Result<Vec<Account>, OAuthError> {
        let store = self.read()?;
        Ok(store
            .registrations
            .iter()
            .map(|(id, registration)| {
                let (label, connected, inference_enabled) = match &registration.state {
                    RegistrationState::Pending => ("Unfinished registration".into(), false, false),
                    RegistrationState::IdentityOnly { identity } => (
                        identity
                            .email
                            .clone()
                            .or_else(|| identity.name.clone())
                            .unwrap_or_else(|| identity.sub.clone()),
                        true,
                        false,
                    ),
                    RegistrationState::Verified { identity, tokens } => (
                        identity
                            .email
                            .clone()
                            .or_else(|| identity.name.clone())
                            .unwrap_or_else(|| identity.sub.clone()),
                        tokens.is_some(),
                        tokens
                            .as_ref()
                            .is_some_and(|tokens| self.permitted(&tokens.scopes)),
                    ),
                };
                Account {
                    registration: id.clone(),
                    label,
                    active: store.active.as_ref() == Some(id),
                    connected,
                    inference_enabled,
                }
            })
            .collect())
    }

    /// Select a verified registration. Existing runtime sessions remain pinned and fail closed.
    ///
    /// # Errors
    /// Reject unknown, unfinished or disconnected accounts and storage failures.
    pub async fn select(&self, registration: &str) -> Result<(), OAuthError> {
        let lock = LockedStore::acquire(&self.path).await?;
        let mut store = self.read_locked(&lock)?;
        if !store
            .registrations
            .get(registration)
            .is_some_and(|registration| {
                matches!(
                    registration.state,
                    RegistrationState::Verified {
                        tokens: Some(_),
                        ..
                    }
                )
            })
        {
            return Err(OAuthError::Reauthorize);
        }
        store.active = Some(registration.into());
        lock.save(&store)
    }

    /// Active registration for pinning a runtime session, without exposing its tokens.
    ///
    /// # Errors
    /// Reject missing, disconnected or identity-only registrations.
    pub fn active_registration(&self) -> Result<String, OAuthError> {
        self.accounts()?
            .into_iter()
            .find(|account| account.active && account.connected)
            .ok_or(OAuthError::Reauthorize)
            .and_then(|account| {
                if account.inference_enabled {
                    Ok(account.registration)
                } else {
                    Err(OAuthError::Scope)
                }
            })
    }

    /// Bind a fresh loopback listener and build a one-use PKCE/OIDC login attempt.
    /// `None` adds a separate registration; `Some` reconnects only that registration.
    ///
    /// # Errors
    /// Reject unavailable discovery/storage, unknown registrations or listener failure.
    pub async fn begin(&self, registration: Option<String>) -> Result<BrowserLogin, OAuthError> {
        let discovery = self.discovery().await?;
        let lock = LockedStore::acquire(&self.path).await?;
        let mut store = self.read_locked(&lock)?;
        if store.host_id.is_empty() {
            store.host_id = host_id()?;
            lock.save(&store)?;
        }
        let previous = registration
            .as_ref()
            .map(|id| {
                store
                    .registrations
                    .get(id)
                    .cloned()
                    .ok_or(OAuthError::Changed)
            })
            .transpose()?;
        BrowserLogin::bind(
            self.clone(),
            discovery,
            registration,
            previous,
            store.host_id,
        )
        .await
    }

    /// Resolve a pinned registration with serialized rotating refresh. Dropping this
    /// waiter does not cancel an already-started rotation or its atomic persistence.
    /// At most 64 resolutions are retained across both OAuth clients in this process.
    ///
    /// # Errors
    /// Reject account changes, absent grants, terminal refresh errors or unavailable storage/network.
    /// Return [`OAuthError::Busy`] immediately when the process-wide limit is reached.
    pub async fn access_token(&self, registration: &str) -> Result<SecretString, OAuthError> {
        let permit = RESOLUTION_PERMITS
            .try_acquire()
            .map_err(|_| OAuthError::Busy)?;
        let client = self.clone();
        let registration = registration.to_owned();
        REFRESHES
            .get_or_init(TaskTracker::new)
            .spawn(async move {
                let _permit = permit;
                tokio::time::timeout(Duration::from_secs(120), client.resolve(&registration))
                    .await
                    .map_err(|_| OAuthError::Network)?
            })
            .await
            .map_err(|_| OAuthError::Network)?
    }

    /// Remove local tokens before remote revocation, retaining verified registration
    /// identity and the host ID. A failed revocation never restores local tokens.
    ///
    /// # Errors
    /// Report storage failures or unconfirmed remote revocation honestly.
    pub async fn logout(&self, registration: &str) -> Result<(), OAuthError> {
        let lock = LockedStore::acquire(&self.path).await?;
        let mut store = self.read_locked(&lock)?;
        let registration_record = store
            .registrations
            .get_mut(registration)
            .ok_or(OAuthError::Changed)?;
        registration_record.generation = random()?;
        let tokens = match &mut registration_record.state {
            RegistrationState::Verified { tokens, .. } => tokens.take(),
            RegistrationState::IdentityOnly { identity } => {
                registration_record.state = RegistrationState::Verified {
                    identity: identity.clone(),
                    tokens: None,
                };
                None
            }
            RegistrationState::Pending => None,
        };
        if store.active.as_deref() == Some(registration) {
            store.active = None;
        }
        lock.save(&store)?;
        drop(lock);
        let Some(tokens) = tokens else { return Ok(()) };
        let refresh = tokens
            .pending
            .as_ref()
            .map(|pending| &pending.refresh_token)
            .unwrap_or(&tokens.refresh_token);
        let discovery = self.discovery().await.map_err(|_| OAuthError::Revocation)?;
        let url = discovery
            .revocation_endpoint
            .ok_or(OAuthError::Revocation)?;
        let response = self
            .http
            .post(url)
            .form(&[
                ("token", refresh.as_str()),
                ("client_id", registration),
                ("token_type_hint", "refresh_token"),
            ])
            .send()
            .await
            .map_err(|_| OAuthError::Revocation)?;
        if response.status().as_u16() == 200 {
            Ok(())
        } else {
            Err(OAuthError::Revocation)
        }
    }

    async fn resolve(&self, registration: &str) -> Result<SecretString, OAuthError> {
        let lock = LockedStore::acquire(&self.path).await?;
        let mut store = self.read_locked(&lock)?;
        if store.active.as_deref() != Some(registration) {
            return Err(OAuthError::Changed);
        }
        // A checkpoint can outlive its access token during a JWKS outage. Allow
        // one renewal after verifying it, using the replacement grant under this lock.
        for _ in 0..2 {
            let record = store
                .registrations
                .get_mut(registration)
                .ok_or(OAuthError::Changed)?;
            if matches!(record.state, RegistrationState::IdentityOnly { .. }) {
                return Err(OAuthError::Scope);
            }
            let RegistrationState::Verified {
                tokens: Some(tokens),
                ..
            } = &mut record.state
            else {
                return Err(OAuthError::Reauthorize);
            };
            if !self.permitted(&tokens.scopes) {
                return Err(OAuthError::Scope);
            }
            let current_time = now()?;
            if tokens.pending.is_none()
                && (current_time.saturating_add(180) < tokens.expires_at
                    || tokens.earliest_refresh_at.is_some_and(|earliest| {
                        current_time < earliest && current_time < tokens.expires_at
                    }))
            {
                return Ok(tokens.access_token.clone().into());
            }
            if tokens.pending.is_none() {
                let response = self
                    .token(&[
                        ("grant_type", "refresh_token"),
                        ("client_id", registration),
                        ("refresh_token", &tokens.refresh_token),
                        ("resource", &self.service.resource),
                    ])
                    .await;
                let response = match response {
                    Ok(response) => response,
                    Err(OAuthError::Reauthorize) => {
                        if let RegistrationState::Verified { tokens, .. } = &mut record.state {
                            *tokens = None;
                        }
                        lock.save(&store)?;
                        return Err(OAuthError::Reauthorize);
                    }
                    Err(error) => return Err(error),
                };
                let pending = response.tokens(Some(tokens), now()?)?;
                // Checkpoint rotating credentials before JWKS retrieval can fail. Pending
                // credentials are never authorized until their refreshed identity verifies.
                tokens.pending = Some(Box::new(pending));
                lock.save(&store)?;
            }
            let record = store
                .registrations
                .get_mut(registration)
                .ok_or(OAuthError::Changed)?;
            let RegistrationState::Verified {
                identity,
                tokens: Some(tokens),
            } = &mut record.state
            else {
                return Err(OAuthError::Reauthorize);
            };
            let pending = tokens.pending.as_ref().ok_or(OAuthError::Response)?;
            if let Some(id_token) = &pending.id_token {
                let refreshed = self
                    .verify(id_token, registration, None, pending.received_at)
                    .await?;
                if refreshed.sub != identity.sub {
                    return Err(OAuthError::AccountMismatch);
                }
                **identity = refreshed;
            }
            let pending = tokens.pending.take().ok_or(OAuthError::Response)?;
            let retained_id_token = pending.id_token.clone().or_else(|| tokens.id_token.clone());
            *tokens = pending;
            tokens.id_token = retained_id_token;
            let permitted = self.permitted(&tokens.scopes);
            let access = (now()? < tokens.expires_at)
                .then(|| SecretString::from(tokens.access_token.clone()));
            lock.save(&store)?;
            if !permitted {
                return Err(OAuthError::Scope);
            }
            if let Some(access) = access {
                return Ok(access);
            }
        }
        Err(OAuthError::Network)
    }

    fn permitted(&self, scopes: &[String]) -> bool {
        scopes.contains(&self.service.inference_scope)
    }

    fn read(&self) -> Result<Store, OAuthError> {
        self.validate_store(storage::read(&self.path)?)
    }

    fn read_locked(&self, lock: &LockedStore) -> Result<Store, OAuthError> {
        self.validate_store(lock.read()?)
    }

    fn validate_store(&self, store: Option<Store>) -> Result<Store, OAuthError> {
        let store = store.unwrap_or_else(|| Store {
            issuer: self.service.issuer.clone(),
            resource: self.service.resource.clone(),
            host_id: String::new(),
            active: None,
            registrations: BTreeMap::new(),
        });
        if store.issuer != self.service.issuer
            || store.resource != self.service.resource
            || store.registrations.len() > 32
        {
            return Err(OAuthError::Storage);
        }
        Ok(store)
    }

    async fn discovery(&self) -> Result<Discovery, OAuthError> {
        let response = self
            .http
            .get(format!(
                "{}/.well-known/openid-configuration",
                self.service.issuer
            ))
            .send()
            .await
            .map_err(|_| OAuthError::Network)?;
        if !response.status().is_success() {
            return Err(OAuthError::Network);
        }
        let discovery: Discovery = bounded_json(response).await?;
        let issuer = Url::parse(&self.service.issuer).map_err(|_| OAuthError::Configuration)?;
        if discovery.issuer != self.service.issuer {
            return Err(OAuthError::Identity);
        }
        for endpoint in [
            &discovery.authorization_endpoint,
            &discovery.token_endpoint,
            &discovery.jwks_uri,
        ]
        .into_iter()
        .chain(discovery.revocation_endpoint.iter())
        {
            let url = Url::parse(endpoint).map_err(|_| OAuthError::Response)?;
            if !trusted_url(&url)
                || url.origin() != issuer.origin()
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return Err(OAuthError::Response);
            }
        }
        Ok(discovery)
    }

    async fn token(&self, form: &[(&str, &str)]) -> Result<TokenResponse, OAuthError> {
        let discovery = self.discovery().await?;
        let response = self
            .http
            .post(discovery.token_endpoint)
            .form(form)
            .send()
            .await
            .map_err(|_| OAuthError::Network)?;
        let success = response.status().is_success();
        let value: serde_json::Value = bounded_json(response).await?;
        if !success {
            return Err(
                if matches!(
                    value["error"].as_str(),
                    Some("invalid_grant" | "invalid_client")
                ) {
                    OAuthError::Reauthorize
                } else {
                    OAuthError::Network
                },
            );
        }
        serde_json::from_value(value).map_err(|_| OAuthError::Response)
    }

    async fn verify(
        &self,
        token: &str,
        client: &str,
        nonce: Option<&str>,
        received_at: u64,
    ) -> Result<Identity, OAuthError> {
        let discovery = self.discovery().await?;
        let response = self
            .http
            .get(discovery.jwks_uri)
            .send()
            .await
            .map_err(|_| OAuthError::Network)?;
        if !response.status().is_success() {
            return Err(OAuthError::Network);
        }
        let keys: JwkSet = bounded_json(response).await?;
        let header = decode_header(token).map_err(|_| OAuthError::Identity)?;
        if header.alg != Algorithm::RS256 || keys.keys.len() > 32 {
            return Err(OAuthError::Identity);
        }
        let key = header
            .kid
            .as_ref()
            .and_then(|kid| keys.find(kid))
            .ok_or(OAuthError::Identity)?;
        let key = DecodingKey::from_jwk(key).map_err(|_| OAuthError::Identity)?;
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&[&self.service.issuer]);
        validation.set_audience(&[client]);
        validation.set_required_spec_claims(&["iss", "sub", "aud", "exp", "iat"]);
        // Refresh checkpoints may outlive the ID token while JWKS is unavailable.
        // Verify expiry at reception, never at an attacker-supplied token timestamp.
        validation.validate_exp = false;
        let claims = decode::<Identity>(token, &key, &validation)
            .map_err(|_| OAuthError::Identity)?
            .claims;
        if !display_text(&claims.sub)
            || claims.exp <= received_at
            || claims.iat > received_at.saturating_add(5)
            || nonce.is_some_and(|nonce| claims.nonce.as_deref() != Some(nonce))
            || claims.azp.as_ref().is_some_and(|azp| azp != client)
            || (claims
                .aud
                .as_array()
                .is_some_and(|audiences| audiences.len() > 1)
                && claims.azp.as_deref() != Some(client))
            || claims
                .email
                .as_ref()
                .is_some_and(|text| !display_text(text))
            || claims.name.as_ref().is_some_and(|text| !display_text(text))
        {
            return Err(OAuthError::Identity);
        }
        Ok(claims)
    }
}

#[derive(Deserialize, Serialize)]
struct Store {
    issuer: String,
    resource: String,
    host_id: String,
    active: Option<String>,
    registrations: BTreeMap<String, Registration>,
}

#[derive(Clone, Deserialize, Serialize)]
struct Registration {
    generation: String,
    state: RegistrationState,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum RegistrationState {
    Pending,
    IdentityOnly {
        identity: Box<Identity>,
    },
    Verified {
        identity: Box<Identity>,
        tokens: Option<Box<Tokens>>,
    },
}

#[derive(Clone, Deserialize, Serialize)]
struct Identity {
    sub: String,
    iss: String,
    aud: serde_json::Value,
    exp: u64,
    iat: u64,
    nonce: Option<String>,
    azp: Option<String>,
    email: Option<String>,
    name: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
struct Tokens {
    access_token: String,
    refresh_token: String,
    id_token: Option<String>,
    scopes: Vec<String>,
    expires_at: u64,
    earliest_refresh_at: Option<u64>,
    received_at: u64,
    pending: Option<Box<Tokens>>,
}

#[derive(Deserialize)]
struct Discovery {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
    revocation_endpoint: Option<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    id_token: Option<String>,
    token_type: Option<String>,
    scope: Option<String>,
    expires_in: Option<u64>,
    earliest_refresh_at: Option<serde_json::Value>,
}

impl TokenResponse {
    fn tokens(self, previous: Option<&Tokens>, received_at: u64) -> Result<Tokens, OAuthError> {
        let scope = self
            .scope
            .or_else(|| previous.map(|tokens| tokens.scopes.join(" ")))
            .ok_or(OAuthError::Response)?;
        let scopes: Vec<String> = scope.split_whitespace().map(str::to_owned).collect();
        let access_token = self.access_token.ok_or(OAuthError::Response)?;
        let refresh_token = self.refresh_token.ok_or(OAuthError::Response)?;
        let expires_in = self
            .expires_in
            .filter(|expiry| (1..=86400).contains(expiry))
            .ok_or(OAuthError::Response)?;
        if !secret_text(&access_token)
            || !secret_text(&refresh_token)
            || self
                .id_token
                .as_ref()
                .is_some_and(|token| !secret_text(token))
            || !self
                .token_type
                .is_some_and(|kind| kind.eq_ignore_ascii_case("bearer"))
            || scope.len() > 4096
        {
            return Err(OAuthError::Response);
        }
        let earliest_refresh_at = self
            .earliest_refresh_at
            .map(|value| {
                value
                    .as_u64()
                    .or_else(|| {
                        value.as_str().and_then(|text| {
                            text.parse().ok().or_else(|| {
                                time::OffsetDateTime::parse(
                                    text,
                                    &time::format_description::well_known::Rfc3339,
                                )
                                .ok()
                                .and_then(|time| u64::try_from(time.unix_timestamp()).ok())
                            })
                        })
                    })
                    .ok_or(OAuthError::Response)
            })
            .transpose()?;
        Ok(Tokens {
            access_token,
            refresh_token,
            id_token: self.id_token,
            scopes,
            expires_at: received_at + expires_in,
            earliest_refresh_at,
            received_at,
            pending: None,
        })
    }
}

async fn bounded_json<T: serde::de::DeserializeOwned>(
    mut response: reqwest::Response,
) -> Result<T, OAuthError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| OAuthError::Network)? {
        if bytes.len().saturating_add(chunk.len()) > 256 * 1024 {
            return Err(OAuthError::Response);
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| OAuthError::Response)
}

fn trusted_url(url: &Url) -> bool {
    (url.scheme() == "https"
        || url.scheme() == "http"
            && match url.host() {
                Some(url::Host::Ipv4(address)) => address.is_loopback(),
                Some(url::Host::Ipv6(address)) => address.is_loopback(),
                _ => false,
            })
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
}

fn now() -> Result<u64, OAuthError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| OAuthError::Response)
}

fn random() -> Result<String, OAuthError> {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes).map_err(|_| OAuthError::Configuration)?;
    Ok(BASE64_URL_SAFE_NO_PAD.encode(bytes))
}

fn host_id() -> Result<String, OAuthError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| OAuthError::Configuration)?;
    bytes[6] = (bytes[6] & 15) | 64;
    bytes[8] = (bytes[8] & 63) | 128;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!(
        "urn:uuid:{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

fn identifier(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 200
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}

fn extension_parameter(key: &str) -> bool {
    identifier(key)
        && !matches!(
            key,
            "client_id"
                | "redirect_uri"
                | "resource"
                | "scope"
                | "state"
                | "nonce"
                | "response_type"
                | "code_challenge"
                | "code_challenge_method"
        )
}

fn secret_text(text: &str) -> bool {
    !text.is_empty() && text.len() <= 16384 && text.bytes().all(|byte| byte.is_ascii_graphic())
}
fn display_text(text: &str) -> bool {
    !text.is_empty() && text.len() <= 256 && !text.chars().any(char::is_control)
}
