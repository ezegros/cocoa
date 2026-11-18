//! Bounded loopback authorization. No callback or authorization URL is logged.

use std::time::Duration;

use secrecy::{ExposeSecret, SecretString};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::{
    Account, Discovery, OAuthClient, OAuthError, Registration, RegistrationState, identifier, now,
    random, storage::LockedStore,
};
use base64::{Engine, prelude::BASE64_URL_SAFE_NO_PAD};

/// One browser attempt. Dropping it closes its listener and discards its PKCE secrets.
/// The URL is a secret because provider hints may contain identity information.
pub struct BrowserLogin {
    client: OAuthClient,
    listener: TcpListener,
    authorization: SecretString,
    redirect: String,
    state: String,
    nonce: String,
    verifier: String,
    registration: Option<String>,
    previous: Option<Registration>,
    active: Option<String>,
}

impl BrowserLogin {
    /// Authorization URL for an application-owned browser opener. Never render or log it.
    #[must_use]
    pub fn authorization_url(&self) -> &SecretString {
        &self.authorization
    }

    /// Open the system browser without a shell or inherited standard streams.
    ///
    /// # Errors
    /// Report unavailable or failed browser launch, bounded to five seconds.
    pub async fn open_browser(&self) -> Result<(), OAuthError> {
        open_url(self.authorization.expose_secret()).await
    }

    /// Await the one-use callback, verify identity and atomically activate it.
    /// Unrelated callbacks never consume the pending transaction. Cancellation
    /// closes the listener and prevents activation; issued registration IDs survive.
    ///
    /// # Errors
    /// Reject callback/client mismatch, denied sign-in, invalid identity/grant,
    /// changed registrations or unavailable storage/network. Expires after five minutes.
    pub async fn finish(self, cancellation: CancellationToken) -> Result<Account, OAuthError> {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => Err(OAuthError::Cancelled),
            result = tokio::time::timeout(Duration::from_secs(300), self.complete()) => result.map_err(|_| OAuthError::Cancelled)?,
        }
    }

    pub(super) async fn bind(
        client: OAuthClient,
        discovery: Discovery,
        registration: Option<String>,
        previous: Option<Registration>,
        host: String,
    ) -> Result<Self, OAuthError> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|_| OAuthError::Listener)?;
        let address = listener.local_addr().map_err(|_| OAuthError::Listener)?;
        let redirect = format!("http://127.0.0.1:{}/auth/callback", address.port());
        let state = random()?;
        let nonce = random()?;
        let verifier = random()?;
        let mut authorization =
            Url::parse(&discovery.authorization_endpoint).map_err(|_| OAuthError::Configuration)?;
        {
            let mut query = authorization.query_pairs_mut();
            query.extend_pairs([
                ("response_type", "code"),
                (
                    "client_id",
                    registration
                        .as_deref()
                        .unwrap_or(&client.service.registration_client),
                ),
                ("redirect_uri", &redirect),
                ("resource", &client.service.resource),
                ("scope", &client.service.scopes),
                ("state", &state),
                ("nonce", &nonce),
                ("code_challenge_method", "S256"),
                (
                    "code_challenge",
                    &BASE64_URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
                ),
            ]);
            if let Some(parameter) = &client.service.host_id_parameter {
                query.append_pair(parameter, &host);
            }
            if registration.is_none() {
                query.extend_pairs(&client.service.registration_parameters);
            }
        }
        let active = client.read()?.active;
        Ok(Self {
            client,
            listener,
            authorization: authorization.to_string().into(),
            redirect,
            state,
            nonce,
            verifier,
            registration,
            previous,
            active,
        })
    }

    async fn complete(mut self) -> Result<Account, OAuthError> {
        let (code, issued_client) = self.callback().await?;
        let generation = {
            let lock = LockedStore::acquire(&self.client.path).await?;
            let mut store = self.client.read_locked(&lock)?;
            match &self.registration {
                Some(registration) => {
                    let current = store
                        .registrations
                        .get(registration)
                        .ok_or(OAuthError::Changed)?;
                    if self
                        .previous
                        .as_ref()
                        .is_none_or(|previous| previous.generation != current.generation)
                    {
                        return Err(OAuthError::Changed);
                    }
                    current.generation.clone()
                }
                None => {
                    if store.registrations.len() >= 32
                        || store.registrations.contains_key(&issued_client)
                    {
                        return Err(OAuthError::Changed);
                    }
                    let generation = random()?;
                    store.registrations.insert(
                        issued_client.clone(),
                        Registration {
                            generation: generation.clone(),
                            state: RegistrationState::Pending,
                        },
                    );
                    lock.save(&store)?;
                    generation
                }
            }
        };
        let response = self
            .client
            .token(&[
                ("grant_type", "authorization_code"),
                ("client_id", &issued_client),
                ("code", &code),
                ("code_verifier", &self.verifier),
                ("redirect_uri", &self.redirect),
                ("resource", &self.client.service.resource),
            ])
            .await?;
        let received_at = now()?;
        let identity = self
            .client
            .verify(
                response.id_token.as_deref().ok_or(OAuthError::Identity)?,
                &issued_client,
                Some(&self.nonce),
                received_at,
            )
            .await?;
        if let Some(Registration {
            state:
                RegistrationState::Verified {
                    identity: previous, ..
                }
                | RegistrationState::IdentityOnly { identity: previous },
            ..
        }) = &self.previous
            && previous.sub != identity.sub
        {
            return Err(OAuthError::AccountMismatch);
        }
        let tokens = if response.access_token.is_none() && response.refresh_token.is_none() {
            None
        } else {
            Some(Box::new(response.tokens(None, received_at)?))
        };
        let lock = LockedStore::acquire(&self.client.path).await?;
        let mut store = self.client.read_locked(&lock)?;
        if store.active != self.active {
            return Err(OAuthError::Changed);
        }
        let current = store
            .registrations
            .get_mut(&issued_client)
            .ok_or(OAuthError::Changed)?;
        if current.generation != generation {
            return Err(OAuthError::Changed);
        }
        *current = Registration {
            generation: random()?,
            state: match tokens {
                Some(tokens) => RegistrationState::Verified {
                    identity: Box::new(identity),
                    tokens: Some(tokens),
                },
                None => RegistrationState::IdentityOnly {
                    identity: Box::new(identity),
                },
            },
        };
        store.active = Some(issued_client.clone());
        lock.save(&store)?;
        self.client
            .accounts()?
            .into_iter()
            .find(|account| account.registration == issued_client)
            .ok_or(OAuthError::Changed)
    }

    async fn callback(&mut self) -> Result<(String, String), OAuthError> {
        receive_callback(&self.listener, |request| self.parse_callback(request)).await
    }

    fn parse_callback(&self, request: &str) -> Option<Result<(String, String), OAuthError>> {
        let parameters = callback_parameters(request, &self.redirect, &self.state)?;
        if let Some(error) = parameters.get("error") {
            return Some(Err(authorization_error(error)));
        }
        let code = parameters
            .get("code")
            .filter(|code| !code.is_empty() && code.len() <= 4096)?;
        let issued_client = parameters.get("client_id").or(self.registration.as_ref());
        let Some(issued_client) = issued_client
            .filter(|id| identifier(id) && *id != &self.client.service.registration_client)
        else {
            return Some(Err(OAuthError::Response));
        };
        if self
            .registration
            .as_ref()
            .is_some_and(|saved| saved != issued_client)
        {
            return Some(Err(OAuthError::Changed));
        }
        Some(Ok((code.clone(), issued_client.clone())))
    }
}

pub(crate) async fn receive_callback<T>(
    listener: &TcpListener,
    parse: impl Fn(&str) -> Option<Result<T, OAuthError>>,
) -> Result<T, OAuthError> {
    for _ in 0..64 {
        let (mut socket, _) = listener.accept().await.map_err(|_| OAuthError::Listener)?;
        let request = tokio::time::timeout(Duration::from_secs(2), read_request(&mut socket)).await;
        let result = match request {
            Ok(Ok(request)) => parse(&request),
            _ => None,
        };
        let (status, message) = match &result {
            Some(Ok(_)) => (
                "200 OK",
                "Return to Cocoa to finish sign-in. You may close this tab.".to_owned(),
            ),
            Some(Err(error)) => (
                "400 Bad Request",
                format!(
                    "Sign-in failed: {error}. Return to Cocoa to retry. You may close this tab."
                ),
            ),
            None => (
                "400 Bad Request",
                "Invalid callback. Return to the original sign-in tab.".to_owned(),
            ),
        };
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nContent-Security-Policy: default-src 'none'; frame-ancestors 'none'\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{message}",
            message.len()
        );
        match tokio::time::timeout(
            Duration::from_secs(2),
            socket.write_all(response.as_bytes()),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(_)) | Err(_) => {
                // Browser response delivery cannot invalidate a state-checked callback.
                if result.is_none() {
                    continue;
                }
            }
        }
        if let Some(result) = result {
            return result;
        }
    }
    Err(OAuthError::Listener)
}

pub(crate) fn authorization_error(error: &str) -> OAuthError {
    match error {
        "invalid_scope" => OAuthError::InvalidScope,
        _ => OAuthError::Denied,
    }
}

pub(crate) fn callback_parameters(
    request: &str,
    redirect: &str,
    state: &str,
) -> Option<std::collections::BTreeMap<String, String>> {
    let mut lines = request.split("\r\n");
    let mut first = lines.next()?.split(' ');
    if first.next()? != "GET" {
        return None;
    }
    let target = first.next()?;
    if !target.starts_with('/')
        || target.starts_with("//")
        || first.next()? != "HTTP/1.1"
        || first.next().is_some()
    {
        return None;
    }
    let base = Url::parse(redirect).ok()?;
    let url = base.join(target).ok()?;
    let hosts: Vec<_> = lines
        .filter_map(|line| line.split_once(':'))
        .filter(|(key, _)| key.eq_ignore_ascii_case("host"))
        .collect();
    if hosts.len() != 1
        || hosts[0].1.trim() != format!("127.0.0.1:{}", base.port()?)
        || url.path() != "/auth/callback"
        || url.origin() != base.origin()
    {
        return None;
    }
    let mut parameters = std::collections::BTreeMap::new();
    for (key, value) in url.query_pairs() {
        if parameters
            .insert(key.into_owned(), value.into_owned())
            .is_some()
        {
            return None;
        }
    }
    if parameters.get("state")? != state {
        return None;
    }
    Some(parameters)
}

/// Open a trusted application link in the system browser.
///
/// # Errors
/// Reject non-HTTPS links or failed/timed-out browser launch.
pub async fn open_link(url: &str) -> Result<(), OAuthError> {
    if !Url::parse(url).is_ok_and(|url| {
        url.scheme() == "https" && url.username().is_empty() && url.password().is_none()
    }) {
        return Err(OAuthError::Configuration);
    }
    open_url(url).await
}

pub(crate) async fn open_url(url: &str) -> Result<(), OAuthError> {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let mut command = tokio::process::Command::new(program);
    command
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let status = tokio::time::timeout(Duration::from_secs(5), command.status())
        .await
        .map_err(|_| OAuthError::Browser)?
        .map_err(|_| OAuthError::Browser)?;
    if status.success() {
        Ok(())
    } else {
        Err(OAuthError::Browser)
    }
}

async fn read_request(socket: &mut TcpStream) -> Result<String, OAuthError> {
    let mut bytes = Vec::new();
    let mut chunk = [0; 1024];
    while bytes.len() < 8192 {
        let count = socket
            .read(&mut chunk)
            .await
            .map_err(|_| OAuthError::Listener)?;
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..count]);
        if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            return String::from_utf8(bytes).map_err(|_| OAuthError::Listener);
        }
    }
    Err(OAuthError::Listener)
}
