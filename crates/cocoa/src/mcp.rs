//! Bounded HTTP MCP connections, resource-bound OAuth and generated REPL functions.
//! Configuration is an explicit user grant. No subprocesses, prompts, resources,
//! sampling, elicitation or background server-initiated execution are supported.

pub mod presets;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use cocoa_core::{
    capability::{
        ActivityOutput, CapabilityBinding, CapabilityDescription, CapabilityError, CapabilityHost,
        CapabilityInput, CapabilityObserver, Effects, OperationOutcome,
    },
    execution::WorkLimits,
    store::RecordId,
};
use cocoa_oauth::mcp::{McpLogin, McpOAuthClient};
use futures_util::{StreamExt, stream};
use reqwest::{
    Client, Url,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::web_search::{EXA_ENDPOINT, WebSearch};

const MODERN: &str = "2026-07-28";
const RESPONSE_BYTES: usize = 1024 * 1024;

/// Persisted server grant. IDs survive enable/disable, but never removal/re-add.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Server {
    id: String,
    url: String,
    enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_id: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    headers_env: BTreeMap<String, String>,
}

/// Sanitized local/protocol errors. Remote response bodies never become diagnostics.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("invalid MCP settings; use at most 16 named HTTPS or loopback HTTP servers")]
    Configuration,
    #[error("MCP server is missing or disabled")]
    Unavailable,
    #[error("unknown MCP preset; run cocoa mcp presets or provide an explicit URL")]
    Preset,
    #[error("MCP server requires authentication")]
    Auth { metadata: Option<String> },
    #[error(
        "MCP header credentials are missing or invalid; check headers_env environment variables"
    )]
    Credentials,
    #[error("MCP OAuth is disabled when headers_env is configured")]
    HeaderAuth,
    #[error("MCP server returned HTTP {0}")]
    Http(u16),
    #[error("MCP server returned JSON-RPC error {0}")]
    Rpc(i64),
    #[error("invalid or unsupported MCP response")]
    Protocol,
    #[error("MCP response or tool catalogue exceeds limits")]
    Limit,
    #[error("MCP network failed or timed out; an attempted tool call may have executed")]
    Network,
    #[error("cannot listen for MCP sign-in cancellation")]
    Signal,
    #[error(transparent)]
    OAuth(#[from] cocoa_oauth::OAuthError),
}

/// Safe menu snapshot. Connected means protocol and complete catalogue succeeded.
#[derive(Clone, Debug)]
pub struct ServerView {
    pub name: String,
    pub status: String,
    pub tools: Vec<Tool>,
    pub enabled: bool,
}

/// Original server names and schemas, without identifier normalization.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// Search selection shared by root and child agents, tagged by `provider`.
/// Selecting MCP hides built-in search without changing any server grants.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, clap::Subcommand)]
#[serde(tag = "provider", rename_all = "snake_case")]
pub enum SearchProvider {
    /// Use built-in keyless Exa search.
    #[default]
    Exa,
    /// Disable built-in search without changing MCP grants.
    Disabled,
    /// Use a configured MCP server's native search tools without an Exa fallback.
    Mcp { server: String },
}

impl<'de> Deserialize<'de> for SearchProvider {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Serde rejects unknown fields on empty struct variants, but not unit variants.
        #[derive(Deserialize)]
        #[serde(tag = "provider", rename_all = "snake_case")]
        enum Fields {
            Exa {},
            Disabled {},
            Mcp { server: String },
        }
        Ok(match Fields::deserialize(deserializer)? {
            Fields::Exa {} => Self::Exa,
            Fields::Disabled {} => Self::Disabled,
            Fields::Mcp { server } => Self::Mcp { server },
        })
    }
}

/// Session-owned connection/catalogue registry shared by root and child agents.
#[derive(Clone)]
pub struct McpManager {
    shared: Arc<Shared>,
}

struct Shared {
    entries: RwLock<BTreeMap<String, Arc<Entry>>>,
    credentials: PathBuf,
    http: Client,
    requests: AtomicU64,
    grants: AtomicU64,
    pending: Semaphore,
    web_search: WebSearch,
    search_provider: RwLock<SearchProvider>,
}

struct Entry {
    identity: u64,
    server: Server,
    state: RwLock<Connection>,
    connecting: tokio::sync::Mutex<()>,
}

#[derive(Clone)]
enum Connection {
    Disconnected,
    Connected {
        tools: Vec<Tool>,
        transport: Transport,
    },
    Failed(String),
    NeedsAuth(Option<String>),
}

#[derive(Clone)]
struct Transport {
    version: String,
    session: Option<String>,
}

struct McpCapabilities {
    manager: McpManager,
    inner: Arc<dyn CapabilityHost>,
}

/// Validate the complete configuration, including unique stable grant identities.
///
/// # Errors
/// Reject invalid names/servers, duplicate IDs or more than sixteen servers.
pub fn validate_servers(servers: &BTreeMap<String, Server>) -> Result<(), McpError> {
    if servers.len() > 16 {
        return Err(McpError::Configuration);
    }
    let mut identities = BTreeSet::new();
    for (name, server) in servers {
        if !valid_server_name(name) || !identities.insert(&server.id) {
            return Err(McpError::Configuration);
        }
        server.validate()?;
    }
    Ok(())
}

/// Exact callable expression using dot syntax only for conservative JS identifiers.
#[must_use]
pub fn callable(server: &str, tool: &str) -> String {
    let mut path = "mcp".to_owned();
    for segment in [server, tool] {
        if !segment.is_empty()
            && segment.bytes().enumerate().all(|(index, byte)| {
                byte.is_ascii_alphabetic()
                    || b"_$".contains(&byte)
                    || index > 0 && byte.is_ascii_digit()
            })
        {
            path.push('.');
            path.push_str(segment);
        } else {
            path.push('[');
            path.push_str(&json!(segment).to_string());
            path.push(']');
        }
    }
    path
}

impl Server {
    /// Create a new grant with an opaque identity. URLs cannot contain credentials,
    /// fragments or query strings; HTTP is restricted to literal loopback addresses.
    ///
    /// # Errors
    /// Reject unsafe URLs/client IDs or unavailable system randomness.
    pub fn new(url: &str, client_id: Option<String>) -> Result<Self, McpError> {
        let mut random = [0; 16];
        getrandom::fill(&mut random).map_err(|_| McpError::Configuration)?;
        let server = Self {
            id: random.iter().map(|byte| format!("{byte:02x}")).collect(),
            url: url.to_owned(),
            enabled: true,
            client_id,
            headers_env: BTreeMap::new(),
        };
        server.validate()?;
        Ok(server)
    }

    /// Use explicit environment-backed authentication headers instead of OAuth.
    /// Values are complete header values, including `Bearer ` when needed.
    /// No secret values are retained in configuration or read by this method.
    ///
    /// # Errors
    /// Reject more than eight headers, duplicate case-insensitive names, headers
    /// other than Authorization or X-* headers, invalid environment names, or
    /// simultaneous OAuth client registration.
    pub fn with_headers_env(mut self, headers: BTreeMap<String, String>) -> Result<Self, McpError> {
        self.headers_env = headers;
        self.validate()?;
        Ok(self)
    }

    /// Explicit credential references to exclude from agent shell environments.
    pub fn credential_env_names(&self) -> impl Iterator<Item = &str> {
        self.headers_env.values().map(String::as_str)
    }

    /// Exact endpoint to which this user grant is bound.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Whether new tool calls are authorized by this grant.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Toggle admission without changing the grant identity.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Validate deserialized configuration before exposing any connection.
    ///
    /// # Errors
    /// Reject unsafe endpoints, malformed grant IDs or client IDs.
    pub fn validate(&self) -> Result<(), McpError> {
        let url = Url::parse(&self.url).map_err(|_| McpError::Configuration)?;
        let mut header_names = BTreeSet::new();
        if self.headers_env.len() > 8 || !self.headers_env.is_empty() && self.client_id.is_some() {
            return Err(McpError::Configuration);
        }
        for (name, reference) in &self.headers_env {
            let name =
                HeaderName::from_bytes(name.as_bytes()).map_err(|_| McpError::Configuration)?;
            if !(name == reqwest::header::AUTHORIZATION || name.as_str().starts_with("x-"))
                || !header_names.insert(name.as_str().to_owned())
                || reference.is_empty()
                || reference.len() > 128
                || !reference.bytes().enumerate().all(|(index, byte)| {
                    byte == b'_' || byte.is_ascii_alphabetic() || index > 0 && byte.is_ascii_digit()
                })
            {
                return Err(McpError::Configuration);
            }
        }
        if self.id.len() != 32
            || !self.id.bytes().all(|byte| byte.is_ascii_hexdigit())
            || self.url.len() > 2048
            || !safe_url(&url)
            || url.query().is_some()
            || self.client_id.as_ref().is_some_and(|id| {
                id.is_empty() || id.len() > 2048 || !id.bytes().all(|byte| byte.is_ascii_graphic())
            })
        {
            return Err(McpError::Configuration);
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for Server {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Fields {
            id: String,
            url: String,
            enabled: bool,
            client_id: Option<String>,
            #[serde(default)]
            headers_env: BTreeMap<String, String>,
        }
        let fields = Fields::deserialize(deserializer)?;
        let server = Self {
            id: fields.id,
            url: fields.url,
            enabled: fields.enabled,
            client_id: fields.client_id,
            headers_env: fields.headers_env,
        };
        server.validate().map_err(serde::de::Error::custom)?;
        Ok(server)
    }
}

impl McpManager {
    /// Create an empty registry without network or credential access.
    ///
    /// # Errors
    /// Report HTTP client initialization failure.
    pub fn new(credentials: PathBuf) -> Result<Self, McpError> {
        let http = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|_| McpError::Configuration)?;
        Ok(Self {
            shared: Arc::new(Shared {
                entries: RwLock::new(BTreeMap::new()),
                credentials,
                http,
                requests: AtomicU64::new(1),
                grants: AtomicU64::new(1),
                pending: Semaphore::new(4),
                web_search: WebSearch::new(EXA_ENDPOINT)?,
                search_provider: RwLock::new(SearchProvider::default()),
            }),
        })
    }

    /// Select search for root and child agents without network requests or grant
    /// changes. A missing, disabled or disconnected selected server never enables
    /// built-in Exa as a fallback. Existing operations can still finish.
    ///
    /// # Errors
    /// Reject malformed MCP server names or an unavailable selection lock.
    pub fn set_search_provider(&self, provider: SearchProvider) -> Result<(), McpError> {
        if matches!(&provider, SearchProvider::Mcp { server } if !valid_server_name(server)) {
            return Err(McpError::Configuration);
        }
        *self
            .shared
            .search_provider
            .write()
            .map_err(|_| McpError::Unavailable)? = provider;
        Ok(())
    }

    /// Atomically replace user grants. Unchanged entries retain their connections.
    /// Old functions retain old IDs and cannot resolve to replacement endpoints.
    ///
    /// # Errors
    /// Reject invalid/duplicate IDs, names, bounds or unavailable registry locks.
    pub fn configure(&self, servers: BTreeMap<String, Server>) -> Result<(), McpError> {
        validate_servers(&servers)?;
        let mut entries = self
            .shared
            .entries
            .write()
            .map_err(|_| McpError::Unavailable)?;
        let mut next = BTreeMap::new();
        for (name, server) in servers {
            let entry = entries
                .get(&name)
                .filter(|entry| entry.server == server)
                .cloned()
                .unwrap_or_else(|| {
                    Arc::new(Entry {
                        identity: self.shared.grants.fetch_add(1, Ordering::Relaxed),
                        server,
                        state: RwLock::new(Connection::Disconnected),
                        connecting: tokio::sync::Mutex::new(()),
                    })
                });
            next.insert(name, entry);
        }
        *entries = next;
        Ok(())
    }

    /// Compose MCP discovery and generated functions with existing capabilities.
    #[must_use]
    pub fn bind(&self, inner: Arc<dyn CapabilityHost>) -> Arc<dyn CapabilityHost> {
        Arc::new(McpCapabilities {
            manager: self.clone(),
            inner,
        })
    }

    /// Safe current status, without credentials or server-provided error messages.
    ///
    /// # Errors
    /// Report unavailable registry locks.
    pub fn servers(&self) -> Result<Vec<ServerView>, McpError> {
        let entries = self
            .shared
            .entries
            .read()
            .map_err(|_| McpError::Unavailable)?;
        entries
            .iter()
            .map(|(name, entry)| {
                let state = entry.state.read().map_err(|_| McpError::Unavailable)?;
                let (status, tools) = if !entry.server.enabled {
                    ("Disabled".into(), Vec::new())
                } else {
                    match &*state {
                        Connection::Disconnected => ("Not connected".into(), Vec::new()),
                        Connection::Connected { tools, .. } => {
                            (format!("Connected · {} tools", tools.len()), tools.clone())
                        }
                        Connection::Failed(message) => (message.clone(), Vec::new()),
                        Connection::NeedsAuth(_) => ("Auth required".into(), Vec::new()),
                    }
                };
                Ok(ServerView {
                    name: name.clone(),
                    status,
                    tools,
                    enabled: entry.server.enabled,
                })
            })
            .collect()
    }

    /// Refresh enabled catalogues, with four concurrent and thirty-second attempts.
    /// Individual failures remain visible in `servers`; no browser opens implicitly.
    ///
    /// # Errors
    /// Report unavailable registry locks, not individual server failures.
    pub async fn connect_all(&self) -> Result<(), McpError> {
        let names: Vec<_> = self
            .shared
            .entries
            .read()
            .map_err(|_| McpError::Unavailable)?
            .iter()
            .filter(|(_, entry)| entry.server.enabled)
            .map(|(name, _)| name.clone())
            .collect();
        let mut connections = stream::iter(names)
            .map(|name| async move {
                if let Err(error) = self.connect(&name).await {
                    tracing::debug!(%error, "MCP connection unavailable");
                }
            })
            .buffer_unordered(4);
        while connections.next().await.is_some() {}
        Ok(())
    }

    /// Connect and atomically replace the complete catalogue, including paginated tools.
    /// Modern stateless requests are probed first; only discovery may fall back to a
    /// legacy initialize/session handshake. Tool calls are never automatically replayed.
    ///
    /// # Errors
    /// Report authentication, network, protocol or resource-limit failures.
    pub async fn connect(&self, name: &str) -> Result<(), McpError> {
        let entry = self.entry(name)?;
        let attempt = async {
            let _connecting = entry.connecting.lock().await;
            let transport = Transport {
                version: MODERN.into(),
                session: None,
            };
            match self.catalogue(&entry, &transport).await {
                Ok(tools) => Ok((tools, transport)),
                Err(
                    McpError::Http(400 | 404 | 405 | 406)
                    | McpError::Rpc(-32600 | -32601 | -32000 | -32002),
                ) => {
                    let transport = self.initialize(&entry).await?;
                    let tools = self.catalogue(&entry, &transport).await?;
                    Ok((tools, transport))
                }
                Err(error) => Err(error),
            }
        };
        let result = tokio::time::timeout(Duration::from_secs(30), attempt)
            .await
            .map_err(|_| McpError::Network)
            .and_then(|result| result);
        *entry.state.write().map_err(|_| McpError::Unavailable)? = match &result {
            Ok((tools, transport)) => Connection::Connected {
                tools: tools.clone(),
                transport: transport.clone(),
            },
            Err(McpError::Auth { metadata }) => Connection::NeedsAuth(metadata.clone()),
            Err(McpError::OAuth(cocoa_oauth::OAuthError::Reauthorize)) => {
                Connection::NeedsAuth(None)
            }
            Err(error) => Connection::Failed(error.to_string()),
        };
        result.map(|_| ())
    }

    /// Begin explicit browser authorization using this endpoint's challenge metadata.
    ///
    /// # Errors
    /// Reject disabled/missing grants, discovery, registration or storage failure.
    pub async fn begin_auth(&self, name: &str) -> Result<McpLogin, McpError> {
        let entry = self.entry(name)?;
        if !entry.server.headers_env.is_empty() {
            return Err(McpError::HeaderAuth);
        }
        let hint = match &*entry.state.read().map_err(|_| McpError::Unavailable)? {
            Connection::NeedsAuth(hint) => hint.clone(),
            _ => None,
        };
        Ok(self.oauth(&entry.server)?.begin(hint.as_deref()).await?)
    }

    /// Delete only this server's local credentials and disconnect its catalogue.
    ///
    /// # Errors
    /// Report unavailable grants or protected storage failure.
    pub async fn logout(&self, name: &str) -> Result<(), McpError> {
        let entry = self
            .shared
            .entries
            .read()
            .map_err(|_| McpError::Unavailable)?
            .get(name)
            .cloned()
            .ok_or(McpError::Unavailable)?;
        if entry.server.headers_env.is_empty() {
            self.oauth(&entry.server)?.logout().await?;
        }
        *entry.state.write().map_err(|_| McpError::Unavailable)? = Connection::Disconnected;
        Ok(())
    }

    fn search_provider(&self) -> Result<SearchProvider, McpError> {
        self.shared
            .search_provider
            .read()
            .map(|provider| provider.clone())
            .map_err(|_| McpError::Unavailable)
    }

    fn entry(&self, name: &str) -> Result<Arc<Entry>, McpError> {
        self.shared
            .entries
            .read()
            .map_err(|_| McpError::Unavailable)?
            .get(name)
            .filter(|entry| entry.server.enabled)
            .cloned()
            .ok_or(McpError::Unavailable)
    }

    fn oauth(&self, server: &Server) -> Result<McpOAuthClient, McpError> {
        Ok(McpOAuthClient::new(
            &server.url,
            self.shared.credentials.join(format!("{}.json", server.id)),
            server.client_id.clone(),
        )?)
    }

    fn bindings(&self) -> Result<Vec<CapabilityBinding>, McpError> {
        let entries = self
            .shared
            .entries
            .read()
            .map_err(|_| McpError::Unavailable)?;
        let mut bindings = Vec::new();
        for (name, entry) in entries.iter().filter(|(_, entry)| entry.server.enabled) {
            if let Connection::Connected { tools, .. } =
                &*entry.state.read().map_err(|_| McpError::Unavailable)?
            {
                bindings.extend(tools.iter().map(|tool| CapabilityBinding {
                    path: vec!["mcp".into(), name.clone(), tool.name.clone()],
                    name: format!("mcp:{}:{}", entry.identity, tool.name),
                }));
            }
        }
        Ok(bindings)
    }

    async fn call(&self, name: &str, arguments: Value) -> Result<Value, McpError> {
        let (identity, tool_name) = name
            .strip_prefix("mcp:")
            .and_then(|name| name.split_once(':'))
            .ok_or(McpError::Unavailable)?;
        let identity: u64 = identity.parse().map_err(|_| McpError::Unavailable)?;
        let entry = self
            .shared
            .entries
            .read()
            .map_err(|_| McpError::Unavailable)?
            .values()
            .find(|entry| entry.identity == identity && entry.server.enabled)
            .cloned()
            .ok_or(McpError::Unavailable)?;
        let (tool, transport) = match &*entry.state.read().map_err(|_| McpError::Unavailable)? {
            Connection::Connected { tools, transport } => (
                tools
                    .iter()
                    .find(|tool| tool.name == tool_name)
                    .cloned()
                    .ok_or(McpError::Unavailable)?,
                transport.clone(),
            ),
            _ => return Err(McpError::Unavailable),
        };
        if !arguments.is_object() {
            return Err(McpError::Protocol);
        }
        let headers = tool_headers(&tool.input_schema, &arguments)?;
        let result = self
            .request(
                &entry,
                &transport,
                "tools/call",
                json!({"name":tool_name,"arguments":arguments}),
                headers,
            )
            .await
            .map(|(result, _)| result);
        match &result {
            Err(McpError::Auth { metadata }) => {
                *entry.state.write().map_err(|_| McpError::Unavailable)? =
                    Connection::NeedsAuth(metadata.clone());
            }
            Err(McpError::OAuth(cocoa_oauth::OAuthError::Reauthorize)) => {
                *entry.state.write().map_err(|_| McpError::Unavailable)? =
                    Connection::NeedsAuth(None);
            }
            _ => {}
        }
        let result = result?;
        if !result["content"].is_array()
            || result
                .get("isError")
                .is_some_and(|value| !value.is_boolean())
        {
            return Err(McpError::Protocol);
        }
        Ok(result)
    }

    async fn catalogue(&self, entry: &Entry, transport: &Transport) -> Result<Vec<Tool>, McpError> {
        let mut tools = Vec::new();
        let mut cursor = None;
        let mut cursors = BTreeSet::new();
        let mut names = BTreeSet::new();
        let mut bytes = 0usize;
        for _ in 0..32 {
            let params = cursor
                .as_ref()
                .map_or_else(|| json!({}), |cursor| json!({"cursor":cursor}));
            let (value, _) = self
                .request(entry, transport, "tools/list", params, HeaderMap::new())
                .await?;
            bytes = bytes.saturating_add(value.to_string().len());
            if bytes > RESPONSE_BYTES {
                return Err(McpError::Limit);
            }
            let page: Vec<Tool> =
                serde_json::from_value(value["tools"].clone()).map_err(|_| McpError::Protocol)?;
            for tool in page {
                if tool.name.is_empty()
                    || tool.name.len() > 128
                    || tool.name.chars().any(char::is_control)
                    || !tool.input_schema.is_object()
                    || !names.insert(tool.name.clone())
                {
                    return Err(McpError::Protocol);
                }
                tool_headers(&tool.input_schema, &json!({}))?;
                tools.push(tool);
                if tools.len() > 256 {
                    return Err(McpError::Limit);
                }
            }
            cursor = match value.get("nextCursor") {
                None | Some(Value::Null) => return Ok(tools),
                Some(Value::String(cursor))
                    if !cursor.is_empty()
                        && cursor.len() <= 4096
                        && cursors.insert(cursor.clone()) =>
                {
                    Some(cursor.clone())
                }
                _ => return Err(McpError::Protocol),
            };
        }
        Err(McpError::Limit)
    }

    async fn initialize(&self, entry: &Entry) -> Result<Transport, McpError> {
        let mut transport = Transport {
            version: "2025-11-25".into(),
            session: None,
        };
        let (result, session) = self.request(entry, &transport, "initialize", json!({"protocolVersion":"2025-11-25", "clientInfo":{"name":"cocoa","version":env!("CARGO_PKG_VERSION")}, "capabilities":{}}), HeaderMap::new()).await?;
        let version = result["protocolVersion"]
            .as_str()
            .filter(|version| matches!(*version, "2025-03-26" | "2025-06-18" | "2025-11-25"))
            .ok_or(McpError::Protocol)?;
        if !result["capabilities"].is_object() {
            return Err(McpError::Protocol);
        }
        transport.version = version.into();
        transport.session = session;
        self.request(
            entry,
            &transport,
            "notifications/initialized",
            json!({}),
            HeaderMap::new(),
        )
        .await?;
        Ok(transport)
    }

    async fn request(
        &self,
        grant: &Entry,
        transport: &Transport,
        method: &str,
        mut params: Value,
        headers: HeaderMap,
    ) -> Result<(Value, Option<String>), McpError> {
        let _permit = self
            .shared
            .pending
            .acquire()
            .await
            .map_err(|_| McpError::Unavailable)?;
        let server = &grant.server;
        let notification = method.starts_with("notifications/");
        let id = self.shared.requests.fetch_add(1, Ordering::Relaxed);
        let mut body = json!({"jsonrpc":"2.0","method":method,"params":params});
        if !notification {
            body["id"] = id.into();
        }
        if transport.version == MODERN {
            params = body["params"].take();
            params["_meta"] = json!({"io.modelcontextprotocol/protocolVersion":MODERN,"io.modelcontextprotocol/clientCapabilities":{},"io.modelcontextprotocol/clientInfo":{"name":"cocoa","version":env!("CARGO_PKG_VERSION")}});
            body["params"] = params;
        }
        let mut request = self
            .shared
            .http
            .post(&server.url)
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", &transport.version)
            .header("Mcp-Method", method)
            .headers(headers)
            .json(&body);
        if let Some(name) = body["params"]["name"].as_str() {
            request = request.header("Mcp-Name", header_value(name));
        }
        if let Some(session) = &transport.session {
            request = request.header("Mcp-Session-Id", session);
        }
        if server.headers_env.is_empty() {
            if let Some(token) = self.oauth(server)?.access_token().await? {
                request = request.bearer_auth(token.expose_secret());
            }
        } else {
            for (name, reference) in &server.headers_env {
                let value = std::env::var(reference).map_err(|_| McpError::Credentials)?;
                if value.is_empty()
                    || value.len() > 8192
                    || value.trim() != value
                    || !value.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
                {
                    return Err(McpError::Credentials);
                }
                let mut value = HeaderValue::from_str(&value).map_err(|_| McpError::Credentials)?;
                value.set_sensitive(true);
                request = request.header(name, value);
            }
        }
        {
            let entries = self
                .shared
                .entries
                .read()
                .map_err(|_| McpError::Unavailable)?;
            let entry = entries
                .values()
                .find(|entry| entry.identity == grant.identity && entry.server.enabled)
                .ok_or(McpError::Unavailable)?;
            if method == "tools/call" {
                let state = entry.state.read().map_err(|_| McpError::Unavailable)?;
                if !matches!(&*state, Connection::Connected { tools, .. } if tools.iter().any(|tool| Some(tool.name.as_str()) == body["params"]["name"].as_str()))
                {
                    return Err(McpError::Unavailable);
                }
            }
        }
        let response = request.send().await.map_err(|_| McpError::Network)?;
        if response.status().as_u16() == 401 {
            if !server.headers_env.is_empty() {
                return Err(McpError::Http(401));
            }
            let metadata = response
                .headers()
                .get("www-authenticate")
                .and_then(|header| header.to_str().ok())
                .and_then(|header| header.split("resource_metadata=\"").nth(1))
                .and_then(|value| value.split('"').next())
                .filter(|value| {
                    value.len() <= 4096 && Url::parse(value).is_ok_and(|url| safe_url(&url))
                })
                .map(str::to_owned);
            return Err(McpError::Auth { metadata });
        }
        if !response.status().is_success() {
            return Err(McpError::Http(response.status().as_u16()));
        }
        let session = response
            .headers()
            .get("mcp-session-id")
            .map(|value| {
                value
                    .to_str()
                    .map(str::to_owned)
                    .map_err(|_| McpError::Protocol)
            })
            .transpose()?;
        if session
            .as_ref()
            .is_some_and(|value| value.is_empty() || value.len() > 1024)
        {
            return Err(McpError::Protocol);
        }
        if notification {
            return if response.status().as_u16() == 202 {
                Ok((Value::Null, session))
            } else {
                Err(McpError::Protocol)
            };
        }
        Ok((read_response(response, id).await?, session))
    }
}

#[async_trait]
impl CapabilityHost for McpCapabilities {
    fn descriptions(&self) -> Vec<CapabilityDescription> {
        let mut descriptions = self.inner.descriptions();
        let mut discovery = "Discover connected HTTP MCP tools with await cocoa.mcp_tools({server?,query?}). Returns {tools:[{server,name,callable,description,inputSchema,...}],truncated}. Call the exact generated function, e.g. await mcp.docs.search({query:'term'}); invalid identifiers use brackets. Names are preserved. Functions refresh between cells; retain ordinary variables. MCP tool results preserve content, structuredContent and isError. isError is a tool result, not a rejected Promise. Transport/auth failures reject and may have uncertain effects; never blindly retry mutations. Configure/authenticate servers through /mcp, not code.".to_owned();
        match self.manager.search_provider() {
            Ok(SearchProvider::Exa) => {
                descriptions.extend(self.manager.shared.web_search.descriptions());
            }
            Ok(SearchProvider::Mcp { server }) => {
                discovery.push_str(&format!(
                    " Selected web search provider is MCP server {}. For web search, discover it with await cocoa.mcp_tools({}) and use its native search tools and schemas. Other MCP servers remain available for other tasks. Built-in Exa is disabled; if the selected server is unavailable, report that rather than switching search providers.",
                    json!(server), json!({"server": server})
                ));
            }
            Ok(SearchProvider::Disabled) => {}
            Err(error) => tracing::error!(%error, "Search provider selection unavailable"),
        }
        descriptions.push(CapabilityDescription { name: "cocoa.mcp_tools".into(), description: discovery, parameters: json!({"type":"object","properties":{"server":{"type":"string"},"query":{"type":"string"}},"additionalProperties":false}) });
        descriptions
    }

    fn bindings(&self) -> Vec<CapabilityBinding> {
        let mut bindings = self.inner.bindings();
        match self.manager.bindings() {
            Ok(mcp_bindings) => bindings.extend(mcp_bindings),
            Err(error) => {
                tracing::error!(%error, "MCP binding snapshot unavailable");
            }
        }
        bindings
    }

    async fn call(
        &self,
        name: &str,
        arguments: Value,
        limits: WorkLimits,
        cancellation: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        if name == "cocoa.web_search" {
            if !matches!(self.manager.search_provider(), Ok(SearchProvider::Exa)) {
                return Err(CapabilityError::Rejected {
                    message: "Built-in web search is disabled; user-configured MCP tools remain available".into(),
                });
            }
            return self
                .manager
                .shared
                .web_search
                .call(name, arguments, limits, cancellation)
                .await;
        }
        if name != "cocoa.mcp_tools" && !name.starts_with("mcp:") {
            return self.inner.call(name, arguments, limits, cancellation).await;
        }
        if cancellation.is_cancelled() {
            return Err(CapabilityError::Cancelled);
        }
        if name == "cocoa.mcp_tools" {
            #[derive(Deserialize)]
            struct Query {
                server: Option<String>,
                query: Option<String>,
            }
            let query: Query = serde_json::from_value(arguments).map_err(|_| rejected())?;
            if query
                .server
                .as_ref()
                .is_some_and(|server| server.len() > 64)
                || query.query.as_ref().is_some_and(|query| query.len() > 256)
            {
                return Err(rejected());
            }
            let servers = self.manager.servers().map_err(|_| rejected())?;
            let query_text = query.query.unwrap_or_default().to_lowercase();
            let mut tools = Vec::new();
            let mut bytes = 64usize;
            let mut truncated = false;
            for server in servers.into_iter().filter(|server| {
                query
                    .server
                    .as_ref()
                    .is_none_or(|name| name == &server.name)
            }) {
                for tool in server.tools {
                    if !format!("{} {}", tool.name, tool.description)
                        .to_lowercase()
                        .contains(&query_text)
                    {
                        continue;
                    }
                    let mut value = serde_json::to_value(&tool).map_err(|_| rejected())?;
                    value["server"] = server.name.clone().into();
                    value["callable"] = callable(&server.name, &tool.name).into();
                    bytes = bytes.saturating_add(value.to_string().len() + 1);
                    if tools.len() >= 128 || bytes > limits.output_bytes / 2 {
                        truncated = true;
                        break;
                    }
                    tools.push(value);
                }
            }
            return Ok(json!({"tools":tools,"truncated":truncated}));
        }
        tokio::select! {
            biased;
            () = cancellation.cancelled() => Err(CapabilityError::Cancelled),
            result = tokio::time::timeout_at(limits.deadline, self.manager.call(name, arguments)) => match result {
                Err(_) => Err(CapabilityError::Deadline),
                Ok(Ok(result)) => Ok(result),
                Ok(Err(error)) => Err(CapabilityError::Tool { code: "mcp_failed".into(), message: error.to_string(), effects: if matches!(error, McpError::Unavailable | McpError::Configuration | McpError::Credentials | McpError::HeaderAuth | McpError::OAuth(_)) { Effects::None } else { Effects::Unknown } }),
            }
        }
    }

    async fn call_observed(
        &self,
        name: &str,
        arguments: Value,
        limits: WorkLimits,
        cancellation: CancellationToken,
        observer: CapabilityObserver,
    ) -> Result<Value, CapabilityError> {
        if name == "cocoa.web_search" || name == "cocoa.mcp_tools" || name.starts_with("mcp:") {
            observer.input(&self.describe_input(name, &arguments));
            self.call(name, arguments, limits, cancellation).await
        } else {
            self.inner
                .call_observed(name, arguments, limits, cancellation, observer)
                .await
        }
    }

    async fn call_started(
        &self,
        name: &str,
        arguments: Value,
        limits: WorkLimits,
        cancellation: CancellationToken,
        observer: CapabilityObserver,
        operation_record: RecordId,
    ) -> Result<Value, CapabilityError> {
        if name == "cocoa.web_search" || name == "cocoa.mcp_tools" || name.starts_with("mcp:") {
            self.call_observed(name, arguments, limits, cancellation, observer)
                .await
        } else {
            self.inner
                .call_started(
                    name,
                    arguments,
                    limits,
                    cancellation,
                    observer,
                    operation_record,
                )
                .await
        }
    }

    fn describe_input(&self, name: &str, arguments: &Value) -> CapabilityInput {
        if name == "cocoa.web_search" {
            return self
                .manager
                .shared
                .web_search
                .describe_input(name, arguments);
        }
        if name == "cocoa.mcp_tools" || name.starts_with("mcp:") {
            CapabilityInput {
                title: "MCP".into(),
                text: arguments.to_string(),
                summary: None,
            }
        } else {
            self.inner.describe_input(name, arguments)
        }
    }

    fn describe_output(
        &self,
        name: &str,
        arguments: &Value,
        outcome: &OperationOutcome,
    ) -> Option<ActivityOutput> {
        if name == "cocoa.web_search" || name == "cocoa.mcp_tools" || name.starts_with("mcp:") {
            None
        } else {
            self.inner.describe_output(name, arguments, outcome)
        }
    }

    fn activity_status(&self, outcome: &OperationOutcome) -> cocoa_core::engine::ActivityStatus {
        self.inner.activity_status(outcome)
    }

    async fn observe_outcome(
        &self,
        name: &str,
        arguments: &Value,
        outcome: &OperationOutcome,
        observer: CapabilityObserver,
    ) {
        if name == "cocoa.web_search" || name == "cocoa.mcp_tools" || name.starts_with("mcp:") {
            observer.status(cocoa_core::engine::ActivityStatus::from(outcome));
        } else {
            self.inner
                .observe_outcome(name, arguments, outcome, observer)
                .await;
        }
    }
}

pub(crate) fn safe_url(url: &Url) -> bool {
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
        && url.fragment().is_none()
}

pub(crate) async fn read_response(
    mut response: reqwest::Response,
    id: u64,
) -> Result<Value, McpError> {
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|header| header.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("");
    let streaming = match content_type {
        "application/json" => false,
        "text/event-stream" => true,
        _ => return Err(McpError::Protocol),
    };
    let mut bytes = Vec::new();
    let mut total = 0usize;
    let mut events = 0usize;
    while let Some(chunk) = response.chunk().await.map_err(|_| McpError::Network)? {
        total = total.saturating_add(chunk.len());
        if total > RESPONSE_BYTES {
            return Err(McpError::Limit);
        }
        bytes.extend_from_slice(&chunk);
        if streaming {
            loop {
                let boundary = bytes
                    .windows(2)
                    .position(|window| window == b"\n\n")
                    .map(|offset| (offset, 2))
                    .into_iter()
                    .chain(
                        bytes
                            .windows(4)
                            .position(|window| window == b"\r\n\r\n")
                            .map(|offset| (offset, 4)),
                    )
                    .min();
                let Some((offset, separator)) = boundary else {
                    break;
                };
                events += 1;
                if events > 256 {
                    return Err(McpError::Limit);
                }
                let event =
                    std::str::from_utf8(&bytes[..offset]).map_err(|_| McpError::Protocol)?;
                let data = event
                    .lines()
                    .filter_map(|line| {
                        line.strip_prefix("data:")
                            .map(|line| line.strip_prefix(' ').unwrap_or(line))
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if !data.is_empty() {
                    let value: Value =
                        serde_json::from_str(&data).map_err(|_| McpError::Protocol)?;
                    if value.get("id").is_some() {
                        return rpc_result(value, id);
                    }
                    if value["jsonrpc"] != "2.0" || !value["method"].is_string() {
                        return Err(McpError::Protocol);
                    }
                }
                bytes.drain(..offset + separator);
            }
        }
    }
    if streaming {
        return Err(McpError::Protocol);
    }
    rpc_result(
        serde_json::from_slice(&bytes).map_err(|_| McpError::Protocol)?,
        id,
    )
}

fn rpc_result(mut value: Value, id: u64) -> Result<Value, McpError> {
    if value["jsonrpc"] != "2.0"
        || value["id"].as_u64() != Some(id)
        || value.get("result").is_some() == value.get("error").is_some()
    {
        return Err(McpError::Protocol);
    }
    if let Some(error) = value.get("error") {
        return Err(McpError::Rpc(
            error["code"].as_i64().ok_or(McpError::Protocol)?,
        ));
    }
    if !value["result"].is_object() {
        return Err(McpError::Protocol);
    }
    Ok(value["result"].take())
}

fn tool_headers(schema: &Value, arguments: &Value) -> Result<HeaderMap, McpError> {
    let mut headers = HeaderMap::new();
    let mut names = BTreeSet::new();
    let mut nodes = vec![(schema, Some(arguments), true)];
    let mut visited = 0usize;
    while let Some((schema, argument, reachable)) = nodes.pop() {
        visited += 1;
        if visited > 4096 {
            return Err(McpError::Limit);
        }
        match schema {
            Value::Object(object) => {
                if let Some(annotation) = object.get("x-mcp-header") {
                    let name = annotation
                        .as_str()
                        .filter(|name| !name.is_empty() && name.len() <= 64)
                        .ok_or(McpError::Protocol)?;
                    let name = HeaderName::from_bytes(format!("Mcp-Param-{name}").as_bytes())
                        .map_err(|_| McpError::Protocol)?;
                    if !reachable
                        || !names.insert(name.as_str().to_owned())
                        || !matches!(
                            schema["type"].as_str(),
                            Some("string" | "integer" | "boolean")
                        )
                    {
                        return Err(McpError::Protocol);
                    }
                    if let Some(argument) = argument.filter(|value| !value.is_null()) {
                        let value = match (schema["type"].as_str(), argument) {
                            (Some("string"), Value::String(value)) => value.clone(),
                            (Some("integer"), Value::Number(value))
                                if value.as_i64().is_some_and(|value| {
                                    value.unsigned_abs() <= 9_007_199_254_740_991
                                }) =>
                            {
                                value.to_string()
                            }
                            (Some("boolean"), Value::Bool(value)) => value.to_string(),
                            _ => return Err(McpError::Protocol),
                        };
                        headers.insert(
                            name,
                            HeaderValue::from_str(&header_value(&value))
                                .map_err(|_| McpError::Protocol)?,
                        );
                    }
                }
                for (key, value) in object {
                    if key == "properties" {
                        if let Some(properties) = value.as_object() {
                            for (name, property) in properties {
                                nodes.push((
                                    property,
                                    argument.and_then(|argument| argument.get(name)),
                                    reachable,
                                ));
                            }
                        }
                    } else if value.is_object() || value.is_array() {
                        nodes.push((value, None, false));
                    }
                }
            }
            Value::Array(values) => {
                nodes.extend(values.iter().map(|value| (value, None, false)));
            }
            _ => {}
        }
    }
    Ok(headers)
}

fn header_value(value: &str) -> String {
    if value.trim() != value
        || value.starts_with("=?base64?")
        || !value.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
    {
        format!("=?base64?{}?=", STANDARD.encode(value))
    } else {
        value.into()
    }
}

fn valid_server_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}

fn rejected() -> CapabilityError {
    CapabilityError::Rejected {
        message: "MCP discovery expects {server?, query?} within the configured limits".into(),
    }
}
