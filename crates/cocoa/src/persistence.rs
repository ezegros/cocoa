//! SQLite session creation, discovery and canonical per-agent storage.
//! Recovery reconciles retained mutations without restarting external execution.

use std::{
    collections::BTreeMap,
    fmt,
    fs::File,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Arc, Weak},
    time::Duration,
};

use async_trait::async_trait;
use cocoa_core::{
    compaction::Checkpoint,
    context::InstructionState,
    store::{
        ActiveSnapshot, AgentId, IdentityError, Record, RecordId, Store, StoreError, StoreLimits,
        StoreSnapshot, encoded_size,
    },
};
use serde::{Serialize, Serializer};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use tokio::sync::{Mutex, OwnedMutexGuard, OwnedSemaphorePermit, Semaphore};

#[path = "persistence/accounting.rs"]
mod accounting;
#[path = "persistence/active.rs"]
mod active;
#[path = "persistence/archives.rs"]
mod archives;
#[path = "persistence/compaction.rs"]
mod compaction;
#[path = "persistence/export.rs"]
mod export;
#[path = "persistence/forks.rs"]
mod forks;
#[path = "persistence/history.rs"]
mod history;
#[path = "persistence/locks.rs"]
mod locks;
#[path = "persistence/migrations.rs"]
mod migrations;
#[path = "persistence/operations.rs"]
mod operations;
#[path = "persistence/reads.rs"]
mod reads;
#[path = "persistence/recovery.rs"]
mod recovery;
#[path = "persistence/search.rs"]
mod search;
#[path = "persistence/sessions.rs"]
mod sessions;
#[path = "persistence/storage.rs"]
mod storage;
#[path = "persistence/writes.rs"]
mod writes;

pub use export::{ExportHistory, ExportLimits, ExportRequest, RunExport};
pub use forks::RewindPoint;
pub use history::{HistoryCursor, HistoryPage, HistoryReadLimits};
pub use operations::AgentAdmission;
pub use search::{SearchMatch, SearchSource, SessionSearch};
pub use sessions::{
    AgentAssignment, AgentMembership, AgentOutcome, ContextRecordId, CoordinationSnapshot,
    EffectiveAliasSettings, EffectiveSessionSettings, RetainedAgent, SessionExecutionLimits,
    SessionSnapshot, StoredContextRecord, StoredSessionSettings,
};
pub use storage::StorageUsage;

/// Shared database with one private writer and one private query-only reader.
/// Clones share connections, session ownership and bounded request admission.
#[derive(Clone)]
pub struct SessionDatabase {
    inner: Arc<Database>,
}

impl SessionDatabase {
    /// Open a database in an existing private directory, initializing empty files.
    /// Existing databases must match the embedded migration history and schema.
    /// Upgrades preserve a verified backup before changing nonempty databases.
    /// It never creates session or agent rows or automatically restores backups.
    /// Dropping this future does not cancel admitted initialization or migration.
    ///
    /// # Errors
    /// Reject unsafe paths, unrelated or incompatible databases, blocked schema
    /// initialization, SQL failures or exhausted startup admission.
    pub async fn open(path: &Path) -> Result<Self, PersistenceError> {
        let permit = OPEN_REQUESTS
            .try_acquire()
            .map_err(|_| PersistenceError::Busy)?;
        let path = path.to_owned();
        tokio::spawn(async move {
            let _permit = permit;
            let path = tokio::task::spawn_blocking(move || {
                locks::database_path(&path, locks::FileCreation::Allowed)
            })
            .await??;
            let (writer, schema) = migrations::open(&path).await?;
            Self::connect_reader(path, writer, schema).await
        })
        .await?
    }

    /// Open only initialized storage, without creating a database or directory.
    /// Return `None` for a missing database or parent directory. Existing empty,
    /// unrelated, incompatible or damaged databases are errors, not empty lists.
    /// This never migrates persisted storage, but may create a shared schema-lock
    /// sidecar for an existing database. The handle is not restricted to reads.
    ///
    /// # Errors
    /// Reject unsafe/inaccessible paths, invalid databases, blocked schema locks,
    /// SQL failures or exhausted startup admission, as with [`Self::open`].
    pub async fn open_existing(path: &Path) -> Result<Option<Self>, PersistenceError> {
        let permit = OPEN_REQUESTS
            .try_acquire()
            .map_err(|_| PersistenceError::Busy)?;
        let path = path.to_owned();
        tokio::spawn(async move {
            let _permit = permit;
            let path = match tokio::task::spawn_blocking(move || {
                locks::database_path(&path, locks::FileCreation::Forbidden)
            })
            .await?
            {
                Ok(path) => path,
                Err(PersistenceError::Io(error))
                    if error.kind() == std::io::ErrorKind::NotFound =>
                {
                    return Ok(None);
                }
                Err(error) => return Err(error),
            };
            let (writer, schema) = migrations::open_existing(&path).await?;
            Self::connect_reader(path, writer, schema).await.map(Some)
        })
        .await?
    }

    /// List metadata by descending activity, breaking timestamp ties by session ID.
    /// `None` selects all directories; a filter must be an absolute UTF-8 path
    /// matching the stored directory exactly. It need not still exist.
    /// Limits are 1..=50; use 20 for the default presentation. Compact JSON pages
    /// contain at most 256 KiB and return a cursor only when more rows remain.
    /// Activity can move sessions ahead of a cursor between separate calls.
    /// This reads neither histories nor settings, acquires no session lock and
    /// does not change session activity or construct a provider/runtime.
    ///
    /// # Errors
    /// Reject invalid filters/limits, malformed stored metadata, exhausted
    /// request/byte admission or database failures. No partial page is returned.
    pub async fn list_sessions(
        &self,
        directory: Option<&Path>,
        limit: u32,
        before: Option<SessionCursor>,
    ) -> Result<SessionPage, PersistenceError> {
        if !(1..=SESSION_LIMIT_MAX).contains(&limit) {
            return Err(PersistenceError::InvalidSessionLimit);
        }
        let directory = directory.map(session_directory).transpose()?;
        let request = self.inner.request()?;
        let payload = self.inner.payload(SESSION_METADATA_BUFFER_BYTES)?;
        let directory = directory.map(str::to_owned);
        let database = Arc::clone(&self.inner);
        tokio::spawn(async move {
            let _request = request;
            let _payload = payload;
            let rows = {
                let mut reader = tokio::time::timeout(WAIT, database.reader.lock())
                    .await
                    .map_err(|_| PersistenceError::Busy)?;
                reader.settle().await?;
                reads::sessions(reader.connection()?, directory.as_deref(), limit, before).await?
            };
            tokio::task::spawn_blocking(move || reads::session_page(rows, limit)).await?
        })
        .await?
    }

    /// Resolve a full public ID or `sess_` plus 8..=31 hex prefix digits.
    /// ASCII case is normalized. Resolution considers every directory and
    /// returns metadata, not execution ownership or permission to resume.
    /// It reads at most two rows without loading histories or settings and
    /// never selects the newest session when a prefix is ambiguous.
    ///
    /// # Errors
    /// Return invalid-reference, not-found or bounded ambiguity errors, malformed
    /// stored metadata, exhausted admission or database failures.
    pub async fn resolve_session(
        &self,
        reference: &str,
    ) -> Result<SessionSummary, PersistenceError> {
        if !(13..=37).contains(&reference.len()) {
            return Err(PersistenceError::InvalidSessionReference);
        }
        let reference = reference.to_ascii_lowercase();
        let suffix = reference
            .strip_prefix("sess_")
            .ok_or(PersistenceError::InvalidSessionReference)?;
        if !suffix
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(PersistenceError::InvalidSessionReference);
        }
        let request = self.inner.request()?;
        let payload = self.inner.payload(SESSION_METADATA_BUFFER_BYTES)?;
        let database = Arc::clone(&self.inner);
        tokio::spawn(async move {
            let _request = request;
            let _payload = payload;
            let rows = {
                let mut reader = tokio::time::timeout(WAIT, database.reader.lock())
                    .await
                    .map_err(|_| PersistenceError::Busy)?;
                reader.settle().await?;
                reads::resolve_session(reader.connection()?, &reference).await?
            };
            tokio::task::spawn_blocking(move || reads::resolved_session(rows)).await?
        })
        .await?
    }

    /// Commit an untitled, unconfigured session and exactly one root agent.
    /// The directory must exist, resolve to a directory and fit 16,384 UTF-8 bytes.
    /// No provider, credentials or executor are needed. The returned attachment
    /// retains exclusive session ownership, shared with stores created from it.
    /// Dropping this future does not cancel admitted creation; it may leave an
    /// empty durable session, whose ownership releases after the write settles.
    ///
    /// # Errors
    /// Reject invalid directories, exhausted admission/attachment capacity,
    /// randomness failure, repeated public-ID collisions or database failures.
    /// [`PersistenceError::CreationOutcomeUnknown`] identifies a session whose
    /// commit needs inspection rather than an automatic creation retry.
    pub async fn create_session(
        &self,
        working_directory: &Path,
    ) -> Result<SessionAttachment, PersistenceError> {
        self.create_with_fork(working_directory, None).await
    }

    async fn create_with_fork(
        &self,
        working_directory: &Path,
        fork: Option<forks::ForkPrefix>,
    ) -> Result<SessionAttachment, PersistenceError> {
        if working_directory.as_os_str().is_empty()
            || working_directory
                .to_str()
                .is_none_or(|path| path.len() > DIRECTORY_BYTES)
        {
            return Err(PersistenceError::InvalidData("session working directory"));
        }
        let request = self.inner.request()?;
        let database = Arc::clone(&self.inner);
        let working_directory = working_directory.to_owned();
        tokio::spawn(async move {
            let _request = request;
            let directory = tokio::task::spawn_blocking(move || {
                let directory = working_directory.canonicalize()?;
                if !directory.is_dir()
                    || directory
                        .to_str()
                        .is_none_or(|path| path.len() > DIRECTORY_BYTES)
                {
                    return Err(PersistenceError::InvalidData("session working directory"));
                }
                Ok(directory)
            })
            .await??;
            let directory = directory
                .to_str()
                .ok_or(PersistenceError::InvalidData("session working directory"))?;
            let mut attachments = tokio::time::timeout(WAIT, database.attachments.lock())
                .await
                .map_err(|_| PersistenceError::Busy)?;
            attachments.retain(|_, attachment| attachment.strong_count() > 0);
            if attachments.len() >= ATTACHMENTS_MAX {
                return Err(PersistenceError::Busy);
            }
            for _ in 0..=PUBLIC_ID_RETRIES {
                let public_id = SessionPublicId::generate()?;
                let lease = match locks::session(&database.path, public_id.as_str()).await {
                    Ok(file) => Arc::new(SessionLease { _file: file }),
                    Err(PersistenceError::SessionInUse) => continue,
                    Err(error) => return Err(error),
                };
                let created =
                    writes::create(&database, directory, &public_id, &lease, fork.as_ref())
                        .await
                        .map_err(|error| match error {
                            PersistenceError::OutcomeUnknown => {
                                PersistenceError::CreationOutcomeUnknown {
                                    public_id: public_id.clone(),
                                }
                            }
                            error => error,
                        })?;
                let Some(created) = created else {
                    continue;
                };
                let attachment = Arc::new(AttachedSession {
                    database: Arc::clone(&database),
                    session_id: created.session,
                    public_id,
                    root_agent: created.root_agent,
                    epoch: 1,
                    recovered: false,
                    lease,
                    operation: Arc::new(Mutex::new(AttachmentState::Ready)),
                });
                attachments.insert(created.session, Arc::downgrade(&attachment));
                return Ok(SessionAttachment { inner: attachment });
            }
            Err(PersistenceError::PublicIdExhausted)
        })
        .await?
    }

    /// Bind an existing participating agent; names never select a namespace.
    /// All handles for a session share its exclusive execution lock and epoch.
    /// A fresh engine must still receive an empty history; binding is not resume.
    ///
    /// # Errors
    /// Reject invalid limits, absent/retired agents, invalid root membership,
    /// unsupported agent formats, another session owner or storage failures.
    pub async fn agent_store(
        &self,
        agent: AgentId,
        limits: StoreLimits,
    ) -> Result<SqliteStore, PersistenceError> {
        self.bind(agent, limits, InstructionPolicy::Optional).await
    }

    /// Bind an agent whose model responses require instruction state immediately.
    /// Ordinary appends may preserve state with `None`, as with the memory store.
    ///
    /// # Errors
    /// Return the same binding failures as [`Self::agent_store`].
    pub async fn agent_store_with_instruction_state(
        &self,
        agent: AgentId,
        limits: StoreLimits,
    ) -> Result<SqliteStore, PersistenceError> {
        self.bind(agent, limits, InstructionPolicy::Required).await
    }

    async fn connect_reader(
        path: PathBuf,
        writer: GuardedConnection,
        schema: Arc<File>,
    ) -> Result<Self, PersistenceError> {
        let mut reader = GuardedConnection::connect(&path, schema, true).await?;
        migrations::check_options(reader.connection()?, true).await?;
        Ok(Self {
            inner: Arc::new(Database {
                path,
                writer: Mutex::new(writer),
                reader: Mutex::new(reader),
                attachments: Mutex::new(BTreeMap::new()),
                requests: Arc::new(Semaphore::new(32)),
                pending_bytes: Arc::new(Semaphore::new(PENDING_BYTES)),
            }),
        })
    }

    async fn bind(
        &self,
        agent: AgentId,
        limits: StoreLimits,
        instructions: InstructionPolicy,
    ) -> Result<SqliteStore, PersistenceError> {
        validate_limits(limits)?;
        let request = self.inner.request()?;
        let database = Arc::clone(&self.inner);
        tokio::spawn(async move {
            let _request = request;
            let binding = {
                let mut reader = tokio::time::timeout(WAIT, database.reader.lock())
                    .await
                    .map_err(|_| PersistenceError::Busy)?;
                reads::binding(reader.connection()?, agent).await?
            };
            let session_id = SessionId::try_from(binding.session_id)
                .map_err(|_| PersistenceError::InvalidData("session identity"))?;
            let mut attachments = tokio::time::timeout(WAIT, database.attachments.lock())
                .await
                .map_err(|_| PersistenceError::Busy)?;
            attachments.retain(|_, attachment| attachment.strong_count() > 0);
            let attachment = match attachments.get(&session_id).and_then(Weak::upgrade) {
                Some(attachment) => {
                    if attachment.epoch != binding.epoch {
                        return Err(PersistenceError::AgentUnavailable);
                    }
                    attachment
                }
                None => {
                    if attachments.len() >= ATTACHMENTS_MAX {
                        return Err(PersistenceError::Busy);
                    }
                    let lease = Arc::new(SessionLease {
                        _file: locks::session(&database.path, &binding.public_id).await?,
                    });
                    let attachment = Arc::new(AttachedSession {
                        database: Arc::clone(&database),
                        session_id,
                        public_id: binding.public_id.parse().map_err(|_| {
                            PersistenceError::InvalidData("session public identity")
                        })?,
                        root_agent: AgentId::try_from(
                            binding
                                .root_agent
                                .ok_or(PersistenceError::InvalidData("session root identity"))?,
                        )
                        .map_err(|_| PersistenceError::InvalidData("session root identity"))?,
                        epoch: binding.epoch,
                        recovered: false,
                        lease,
                        operation: Arc::new(Mutex::new(AttachmentState::Ready)),
                    });
                    attachments.insert(session_id, Arc::downgrade(&attachment));
                    attachment
                }
            };
            Ok(SqliteStore {
                attachment,
                agent,
                limits,
                instructions,
                settings: None,
                checkpoint_cache: Arc::new(Mutex::new(None)),
                append_prefix: Arc::new(Mutex::new(None)),
                storage_next: Arc::new(Mutex::new(tokio::time::Instant::now())),
            })
        })
        .await?
    }
}

/// Committed session ownership. Clones and derived stores retain the same lock;
/// dropping the last owner releases it only after admitted operations settle.
/// Mutations follow [`SqliteStore`]'s cancellation and reconciliation contract.
#[derive(Clone)]
pub struct SessionAttachment {
    inner: Arc<AttachedSession>,
}

impl SessionAttachment {
    /// Database-local identity, distinct from its public reference and root agent.
    #[must_use]
    pub fn id(&self) -> SessionId {
        self.inner.session_id
    }

    /// Full permanent CLI/UI reference, never a shortened or ambiguous prefix.
    #[must_use]
    pub fn public_id(&self) -> &SessionPublicId {
        &self.inner.public_id
    }

    /// Root membership is structural, independent of the agent's display name.
    #[must_use]
    pub fn root_agent(&self) -> AgentId {
        self.inner.root_agent
    }

    /// Bind the root history without another query or session-lock acquisition.
    /// This does not construct an engine or resume an existing invocation.
    ///
    /// # Errors
    /// Reject a zero per-record limit or one above the 256 MiB payload budget.
    pub fn root_store(&self, limits: StoreLimits) -> Result<SqliteStore, PersistenceError> {
        self.store(limits, InstructionPolicy::Optional)
    }

    /// Bind the root with mandatory candidate state on model-response appends.
    ///
    /// # Errors
    /// Return the same limit failures as [`Self::root_store`].
    pub fn root_store_with_instruction_state(
        &self,
        limits: StoreLimits,
    ) -> Result<SqliteStore, PersistenceError> {
        self.store(limits, InstructionPolicy::Required)
    }

    fn store(
        &self,
        limits: StoreLimits,
        instructions: InstructionPolicy,
    ) -> Result<SqliteStore, PersistenceError> {
        validate_limits(limits)?;
        Ok(SqliteStore {
            attachment: Arc::clone(&self.inner),
            agent: self.inner.root_agent,
            limits,
            instructions,
            settings: None,
            checkpoint_cache: Arc::new(Mutex::new(None)),
            append_prefix: Arc::new(Mutex::new(None)),
            storage_next: Arc::new(Mutex::new(tokio::time::Instant::now())),
        })
    }
}

/// Metadata only. A missing title stays absent; presentation may show
/// `Untitled session`. Listing a session does not validate execution compatibility.
/// JSON encodes the internal ID as decimal text to preserve all 64 bits.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SessionSummary {
    pub id: SessionId,
    pub public_id: SessionPublicId,
    pub working_directory: PathBuf,
    pub title: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub last_opened_at_ms: i64,
}

/// Bounded metadata and a continuation after the last returned session.
/// Separate pages do not retain a live SQLite read snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SessionPage {
    pub sessions: Vec<SessionSummary>,
    pub next_cursor: Option<SessionCursor>,
}

/// A descending activity/ID keyset position, encoded as `v1:<milliseconds>:<id>`.
/// This is a pagination position, not an execution token or public session ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionCursor {
    updated_at_ms: i64,
    id: SessionId,
}

impl SessionCursor {
    /// Construct a position with a nonnegative activity timestamp.
    ///
    /// # Errors
    /// Reject timestamps before the Unix epoch.
    pub fn new(updated_at_ms: i64, id: SessionId) -> Result<Self, PersistenceError> {
        if updated_at_ms < 0 {
            return Err(PersistenceError::InvalidSessionCursor);
        }
        Ok(Self { updated_at_ms, id })
    }

    #[must_use]
    pub fn updated_at_ms(&self) -> i64 {
        self.updated_at_ms
    }

    #[must_use]
    pub fn id(&self) -> SessionId {
        self.id
    }
}

impl FromStr for SessionCursor {
    type Err = PersistenceError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() > 42 {
            return Err(PersistenceError::InvalidSessionCursor);
        }
        let (timestamp, id) = value
            .strip_prefix("v1:")
            .and_then(|position| position.split_once(':'))
            .ok_or(PersistenceError::InvalidSessionCursor)?;
        if !timestamp.bytes().all(|byte| byte.is_ascii_digit())
            || !id.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(PersistenceError::InvalidSessionCursor);
        }
        let timestamp = timestamp
            .parse()
            .map_err(|_| PersistenceError::InvalidSessionCursor)?;
        let id = id
            .parse::<i64>()
            .ok()
            .and_then(|id| SessionId::try_from(id).ok())
            .ok_or(PersistenceError::InvalidSessionCursor)?;
        Self::new(timestamp, id)
    }
}

impl fmt::Display for SessionCursor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "v1:{}:{}",
            self.updated_at_ms,
            i64::from(self.id)
        )
    }
}

impl Serialize for SessionCursor {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// Positive database-local session primary key, not an ordering or public ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionId(i64);

impl TryFrom<i64> for SessionId {
    type Error = IdentityError;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        if value <= 0 {
            return Err(IdentityError::NonPositive);
        }
        Ok(Self(value))
    }
}

impl From<SessionId> for i64 {
    fn from(value: SessionId) -> Self {
        value.0
    }
}

impl Serialize for SessionId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&self.0)
    }
}

/// Full `sess_` reference with 128 random bits encoded as 32 lowercase hex digits.
/// Parsing requires the canonical complete form; command prefix resolution is
/// separate and cannot construct this type from a partial reference.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionPublicId(String);

impl SessionPublicId {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn generate() -> Result<Self, PersistenceError> {
        let mut random = [0; 16];
        getrandom::fill(&mut random).map_err(PersistenceError::Randomness)?;
        let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        Ok(Self(format!("sess_{suffix}")))
    }
}

impl FromStr for SessionPublicId {
    type Err = SessionPublicIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let suffix = value.strip_prefix("sess_").ok_or(SessionPublicIdError)?;
        if suffix.len() != 32
            || !suffix
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(SessionPublicIdError);
        }
        Ok(Self(value.into()))
    }
}

impl fmt::Display for SessionPublicId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for SessionPublicId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// A full public reference must use the canonical prefix, length and alphabet.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("session public ID must be sess_ followed by 32 lowercase hex digits")]
pub struct SessionPublicIdError;

/// History handle bound to one admitted agent and an exclusive session attachment.
/// Admitted appends and local mutations outlive a dropped future. An abandoned
/// result or unknown commit blocks further writes until a successful [`Store::load`]
/// on the mutating agent or [`SessionAttachment::load`] observes durable state.
/// Reconcile history, membership, context and receipts as applicable before
/// deciding whether to retry work. Loading does not repeat external execution.
#[derive(Clone)]
pub struct SqliteStore {
    attachment: Arc<AttachedSession>,
    agent: AgentId,
    limits: StoreLimits,
    instructions: InstructionPolicy,
    settings: Option<Arc<std::sync::Mutex<EffectiveSessionSettings>>>,
    checkpoint_cache: Arc<Mutex<Option<active::CachedHead>>>,
    append_prefix: Arc<Mutex<Option<reads::Stats>>>,
    storage_next: Arc<Mutex<tokio::time::Instant>>,
}

impl SqliteStore {
    /// Select the root's startup model. The next append binds this profile atomically
    /// with its record, and rejects changes after retained model output.
    /// Cloned stores share the pending profile.
    ///
    /// # Errors
    /// Reject stores without root settings, invalid profiles or poisoned settings.
    pub fn select_initial_model(
        &self,
        profile: EffectiveAliasSettings,
        protocol: &str,
    ) -> Result<(), PersistenceError> {
        let settings = self
            .settings
            .as_ref()
            .ok_or(PersistenceError::SettingsMismatch)?;
        let mut settings = settings
            .lock()
            .map_err(|_| PersistenceError::SettingsMismatch)?;
        let mut selected = settings.clone();
        selected.provider = profile.provider.clone();
        selected.protocol = protocol.into();
        selected.model = profile.model.clone();
        selected.endpoint = profile.endpoint.clone();
        selected.account = profile.account.clone();
        selected.thinking_level = profile.thinking_level.clone();
        selected.reasoning_summary = profile.reasoning_summary;
        selected.aliases.insert(selected.alias.clone(), profile);
        selected.validate()?;
        *settings = selected;
        Ok(())
    }
}

#[async_trait]
impl Store for SqliteStore {
    fn request_recorder(
        &self,
        invocation: cocoa_core::store::InvocationId,
    ) -> Option<Arc<dyn cocoa_core::accounting::RequestRecorder>> {
        Some(Arc::new(accounting::Recorder {
            store: self.clone(),
            invocation,
        }))
    }

    async fn append(
        &self,
        record: &Record,
        instruction_state: Option<&InstructionState>,
    ) -> Result<RecordId, StoreError> {
        let request = self.attachment.database.request().map_err(store_error)?;
        record.validate()?;
        let record_bytes = encoded_size(record)?;
        if record_bytes > self.limits.record_bytes {
            return Err(StoreError::RecordLimit);
        }
        let state_bytes = instruction_state
            .map(|state| {
                state.validate_bounds(usize::MAX)?;
                encoded_size(state)
            })
            .transpose()?;
        let bytes = record_bytes
            .checked_add(state_bytes.unwrap_or(0))
            .ok_or(StoreError::Capacity)?;
        let payload = self
            .attachment
            .database
            .payload(bytes)
            .map_err(store_error)?;
        let mut guard =
            tokio::time::timeout(WAIT, Arc::clone(&self.attachment.operation).lock_owned())
                .await
                .map_err(|_| store_error(PersistenceError::Busy))?;
        if matches!(*guard, AttachmentState::Uncertain { .. }) {
            return Err(store_error(PersistenceError::OutcomeUnknown));
        }
        *guard = AttachmentState::Uncertain { agent: self.agent };
        let record = record.clone();
        let instruction_state = instruction_state.cloned();
        let store = self.clone();
        let mut completion = tokio::spawn(async move {
            let result = async {
                let encoded =
                    tokio::task::spawn_blocking(move || writes::encode(record, instruction_state))
                        .await??;
                let result = writes::append(&store, encoded).await;
                if result.is_ok()
                    && let Err(error) = storage::maintain(&store, storage::Maintenance::Due).await
                {
                    if matches!(error, PersistenceError::OutcomeUnknown) {
                        return Err(error);
                    }
                    tracing::warn!(%error, "archive catch-up deferred; canonical append retained");
                }
                result
            }
            .await;
            let known = !matches!(result, Err(PersistenceError::OutcomeUnknown));
            AppendCompletion {
                result,
                known,
                _attachment: Arc::clone(&store.attachment),
                operation: guard,
                _request: request,
                _payload: payload,
            }
        })
        .await
        .map_err(PersistenceError::from)
        .map_err(store_error)?;
        if completion.known {
            *completion.operation = AttachmentState::Ready;
        }
        completion.result.map_err(store_error)
    }

    async fn publish_checkpoint(&self, checkpoint: &Checkpoint) -> Result<RecordId, StoreError> {
        let bytes = encoded_size(checkpoint)?;
        if bytes > compaction::CHECKPOINT_BYTES_MAX {
            return Err(StoreError::RecordLimit);
        }
        let request = self.attachment.database.request().map_err(store_error)?;
        let payload = self
            .attachment
            .database
            .payload(bytes)
            .map_err(store_error)?;
        let mut operation =
            tokio::time::timeout(WAIT, Arc::clone(&self.attachment.operation).lock_owned())
                .await
                .map_err(|_| store_error(PersistenceError::Busy))?;
        if matches!(*operation, AttachmentState::Uncertain { .. }) {
            return Err(store_error(PersistenceError::OutcomeUnknown));
        }
        *operation = AttachmentState::Uncertain { agent: self.agent };
        let checkpoint = checkpoint.clone();
        let store = self.clone();
        let mut completion = tokio::spawn(async move {
            let result = compaction::publish(&store, checkpoint).await;
            let known = !matches!(result, Err(PersistenceError::OutcomeUnknown));
            AppendCompletion {
                result,
                known,
                _attachment: Arc::clone(&store.attachment),
                operation,
                _request: request,
                _payload: payload,
            }
        })
        .await
        .map_err(PersistenceError::from)
        .map_err(store_error)?;
        if completion.known {
            *completion.operation = AttachmentState::Ready;
        }
        completion.result.map_err(store_error)
    }

    async fn load(&self) -> Result<StoreSnapshot, StoreError> {
        let request = self.attachment.database.request().map_err(store_error)?;
        let store = self.clone();
        let mut completion = tokio::spawn(async move {
            let guard =
                tokio::time::timeout(WAIT, Arc::clone(&store.attachment.operation).lock_owned())
                    .await
                    .map_err(|_| PersistenceError::Busy)?;
            if matches!(*guard, AttachmentState::Uncertain { .. }) {
                let mut writer =
                    tokio::time::timeout(WAIT, store.attachment.database.writer.lock())
                        .await
                        .map_err(|_| PersistenceError::Busy)?;
                writer.settle().await?;
            }
            let snapshot = reads::load(&store).await?;
            Ok(LoadCompletion {
                snapshot,
                _attachment: Arc::clone(&store.attachment),
                operation: guard,
                _request: request,
            })
        })
        .await
        .map_err(PersistenceError::from)
        .and_then(std::convert::identity)
        .map_err(store_error)?;
        if matches!(*completion.operation, AttachmentState::Uncertain { agent } if agent == self.agent)
        {
            *completion.operation = AttachmentState::Ready;
        }
        Ok(completion.snapshot)
    }

    async fn load_active(&self) -> Result<ActiveSnapshot, StoreError> {
        let request = self.attachment.database.request().map_err(store_error)?;
        let store = self.clone();
        let mut completion = tokio::spawn(async move {
            let operation =
                tokio::time::timeout(WAIT, Arc::clone(&store.attachment.operation).lock_owned())
                    .await
                    .map_err(|_| PersistenceError::Busy)?;
            let snapshot = if matches!(*operation, AttachmentState::Uncertain { .. }) {
                let mut writer =
                    tokio::time::timeout(WAIT, store.attachment.database.writer.lock())
                        .await
                        .map_err(|_| PersistenceError::Busy)?;
                writer.settle().await?;
                drop(writer);
                reads::load(&store).await?.into_active()?
            } else {
                active::load(&store).await?
            };
            Ok::<_, PersistenceError>(LoadCompletion {
                snapshot,
                operation,
                _attachment: Arc::clone(&store.attachment),
                _request: request,
            })
        })
        .await
        .map_err(PersistenceError::from)
        .and_then(std::convert::identity)
        .map_err(store_error)?;
        if matches!(*completion.operation, AttachmentState::Uncertain { agent } if agent == self.agent)
        {
            *completion.operation = AttachmentState::Ready;
        }
        Ok(completion.snapshot)
    }
}

/// Storage errors retain SQLx/IO sources outside the core `Store` interface.
#[derive(Debug, thiserror::Error)]
pub enum PersistenceError {
    #[error("export limits require 1 to 65536 rows and 1 to 134217728 bytes")]
    InvalidExportLimits,
    #[error("session export exceeds the requested row or byte bound")]
    ExportLimit,
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Resume(#[from] cocoa_core::engine::ResumeError),
    #[error("SQLite operation failed: {0}")]
    Database(#[from] sqlx::Error),
    #[error("SQLite migration failed: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),
    #[error("persistence filesystem operation failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("persistence task failed")]
    Task(#[from] tokio::task::JoinError),
    #[error("session ID randomness failed: {0}")]
    Randomness(#[source] getrandom::Error),
    #[error("session public ID collision retries exhausted")]
    PublicIdExhausted,
    #[error("session list limit must be between 1 and 50")]
    InvalidSessionLimit,
    #[error("session cursor must be v1:<nonnegative milliseconds>:<positive session ID>")]
    InvalidSessionCursor,
    #[error("history reads require 1 to 64 records and 1 to 268435456 response bytes")]
    InvalidHistoryLimits,
    #[error("history cursor is invalid or belongs to another session or agent")]
    InvalidHistoryCursor,
    #[error("history record {record:?} requires {bytes_required} response bytes")]
    HistoryRecordTooLarge {
        record: RecordId,
        bytes_required: u64,
    },
    #[error("session reference must be sess_ followed by 8 to 32 hexadecimal digits")]
    InvalidSessionReference,
    #[error("session not found")]
    SessionNotFound,
    #[error("session ID prefix is ambiguous; matches at least {first} and {second}")]
    AmbiguousSessionId {
        first: SessionPublicId,
        second: SessionPublicId,
    },
    #[error("unknown creation outcome for {public_id}; inspect durable state before retrying")]
    CreationOutcomeUnknown { public_id: SessionPublicId },
    #[error("persistence request admission or lock wait exhausted")]
    Busy,
    #[error("schema initialization is blocked by another database user")]
    MigrationBlocked,
    #[error("nonempty database needs migration with a verified backup")]
    MigrationRequired,
    #[error(
        "database upgrade failed during {phase}; retained backup: {backup:?}; no files were restored automatically: {source}"
    )]
    Upgrade {
        phase: &'static str,
        backup: Option<PathBuf>,
        source: Box<PersistenceError>,
    },
    #[error("another attachment owns this session")]
    SessionInUse,
    #[error("agent is unavailable or its attachment epoch changed")]
    AgentUnavailable,
    #[error("invalid persistence data: {0}")]
    InvalidData(&'static str),
    #[error("unsupported persisted payload version: {0}")]
    UnsupportedPayload(i64),
    #[error("unknown append outcome; load and reconcile durable history before another append")]
    OutcomeUnknown,
    #[error("saved session settings differ from the effective settings")]
    SettingsMismatch,
    #[error("operation does not match the bound caller, method or exact arguments")]
    OperationMismatch,
    #[error("only the context record owner may replace it")]
    ContextNotOwner,
    #[error("session search exceeded its cooperative budget")]
    SearchBudget,
}

const WAIT: Duration = Duration::from_secs(5);
const PENDING_BYTES: usize = 256 * 1024 * 1024;
const DIRECTORY_BYTES: usize = 16_384;
const ATTACHMENTS_MAX: usize = 32;
const ASSIGNMENT_JSON_BYTES_MAX: usize = 8 * 1024 * 1024 + 1024;
const PUBLIC_ID_RETRIES: usize = 3;
const SESSION_LIMIT_MAX: u32 = 50;
const SESSION_RESPONSE_BYTES: usize = 256 * 1024;
const SESSION_METADATA_BUFFER_BYTES: usize = 4 * 1024 * 1024;
const FORMAT_VERSION: i64 = 1;
const RECORD_VERSION: i64 = 2;
const RECORD_ENVELOPE_BYTES: usize = br#"{"invocation":,"kind":}"#.len();
static OPEN_REQUESTS: Semaphore = Semaphore::const_new(4);

struct Database {
    path: PathBuf,
    writer: Mutex<GuardedConnection>,
    reader: Mutex<GuardedConnection>,
    attachments: Mutex<BTreeMap<SessionId, Weak<AttachedSession>>>,
    requests: Arc<Semaphore>,
    pending_bytes: Arc<Semaphore>,
}

impl Database {
    fn request(&self) -> Result<OwnedSemaphorePermit, PersistenceError> {
        Arc::clone(&self.requests)
            .try_acquire_owned()
            .map_err(|_| PersistenceError::Busy)
    }

    fn payload(&self, bytes: usize) -> Result<OwnedSemaphorePermit, PersistenceError> {
        let bytes = u32::try_from(bytes).map_err(|_| PersistenceError::Busy)?;
        Arc::clone(&self.pending_bytes)
            .try_acquire_many_owned(bytes)
            .map_err(|_| PersistenceError::Busy)
    }
}

struct AttachedSession {
    database: Arc<Database>,
    session_id: SessionId,
    public_id: SessionPublicId,
    root_agent: AgentId,
    epoch: i64,
    recovered: bool,
    lease: Arc<SessionLease>,
    operation: Arc<Mutex<AttachmentState>>,
}

enum AttachmentState {
    Ready,
    Uncertain { agent: AgentId },
}

struct SessionLease {
    _file: File,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InstructionPolicy {
    Optional,
    Required,
}

struct AppendCompletion<T = RecordId> {
    result: Result<T, PersistenceError>,
    known: bool,
    operation: OwnedMutexGuard<AttachmentState>,
    _attachment: Arc<AttachedSession>,
    _request: OwnedSemaphorePermit,
    _payload: OwnedSemaphorePermit,
}

struct LoadCompletion<T = StoreSnapshot> {
    snapshot: T,
    operation: OwnedMutexGuard<AttachmentState>,
    _attachment: Arc<AttachedSession>,
    _request: OwnedSemaphorePermit,
}

struct GuardedConnection {
    inner: Option<SqliteConnection>,
    schema: Arc<File>,
    attachment: Option<Arc<SessionLease>>,
    search: search::ActiveSearch,
}

impl GuardedConnection {
    async fn connect(
        path: &Path,
        schema: Arc<File>,
        read_only: bool,
    ) -> Result<Self, PersistenceError> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .read_only(read_only)
            .foreign_keys(true)
            .synchronous(sqlx::sqlite::SqliteSynchronous::Full)
            .busy_timeout(WAIT)
            .statement_cache_capacity(32)
            .command_buffer_size(8)
            .row_buffer_size(8)
            .pragma("query_only", if read_only { "ON" } else { "OFF" })
            .pragma("journal_size_limit", "8388608");
        let mut guarded = Self {
            inner: None,
            schema,
            attachment: None,
            search: Arc::new(std::sync::Mutex::new(None)),
        };
        guarded.inner = Some(SqliteConnection::connect_with(&options).await?);
        if read_only {
            let search = Arc::clone(&guarded.search);
            guarded
                .connection()?
                .lock_handle()
                .await?
                .set_progress_handler(1000, move || {
                    search.lock().is_ok_and(|active| {
                        active.as_ref().is_none_or(|control| !control.stopped())
                    })
                });
        }
        Ok(guarded)
    }

    fn connection(&mut self) -> Result<&mut SqliteConnection, PersistenceError> {
        self.inner
            .as_mut()
            .ok_or(PersistenceError::InvalidData("closed connection"))
    }

    async fn settle(&mut self) -> Result<(), PersistenceError> {
        self.connection()?.ping().await?;
        self.attachment = None;
        Ok(())
    }

    async fn close(mut self) -> Result<(), PersistenceError> {
        let receiver = self.shutdown()?;
        receiver
            .await
            .map_err(|_| PersistenceError::InvalidData("connection shutdown failed"))??;
        Ok(())
    }

    fn shutdown(
        &mut self,
    ) -> Result<tokio::sync::oneshot::Receiver<Result<(), sqlx::Error>>, PersistenceError> {
        let connection = self
            .inner
            .take()
            .ok_or(PersistenceError::InvalidData("closed connection"))?;
        let schema = Arc::clone(&self.schema);
        let keep_schema = Arc::clone(&schema);
        let attachment = self.attachment.take();
        let keep_attachment = attachment.clone();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        // Worker shutdown must outlive cancellation and even the originating runtime.
        // A shutdown-only thread awaits SQLx's existing worker; it executes no queries.
        let started = std::thread::Builder::new()
            .name("cocoa-sqlite-close".into())
            .spawn(move || {
                let _attachment = attachment;
                match tokio::runtime::Builder::new_current_thread().build() {
                    Ok(runtime) => {
                        let result = runtime.block_on(connection.close());
                        if let Err(error) = &result {
                            tracing::error!(%error, "SQLite connection shutdown failed");
                        }
                        if sender.send(result).is_err() {
                            tracing::debug!("SQLite shutdown caller no longer waiting");
                        }
                        drop(schema);
                    }
                    Err(error) => {
                        tracing::error!(%error, "could not await SQLite shutdown; retaining schema lock");
                        std::mem::forget(schema);
                        std::mem::forget(_attachment);
                    }
                }
            });
        if let Err(error) = started {
            // Fail closed if the OS cannot start the shutdown waiter.
            std::mem::forget(keep_schema);
            std::mem::forget(keep_attachment);
            return Err(error.into());
        }
        Ok(receiver)
    }
}

impl Drop for GuardedConnection {
    fn drop(&mut self) {
        if self.inner.is_some()
            && let Err(error) = self.shutdown()
        {
            tracing::error!(%error, "could not arrange SQLite worker shutdown");
        }
    }
}

fn session_directory(path: &Path) -> Result<&str, PersistenceError> {
    let directory = path
        .to_str()
        .filter(|directory| directory.len() <= DIRECTORY_BYTES && !directory.contains('\0'))
        .ok_or(PersistenceError::InvalidData("session directory filter"))?;
    if !path.is_absolute() {
        return Err(PersistenceError::InvalidData("session directory filter"));
    }
    Ok(directory)
}

fn validate_limits(limits: StoreLimits) -> Result<(), PersistenceError> {
    limits.validate()?;
    // One record and its instruction state must fit the in-flight payload budget.
    if limits.record_bytes > PENDING_BYTES {
        return Err(StoreError::InvalidLimits.into());
    }
    Ok(())
}

fn store_error(error: PersistenceError) -> StoreError {
    match error {
        PersistenceError::Store(error) => error,
        error => StoreError::Backend {
            message: error.to_string(),
        },
    }
}
