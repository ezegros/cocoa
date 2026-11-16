//! Unix snapshots, prepared replacements, quotas and owned worker lifecycle.

use std::{
    collections::{BTreeMap, HashMap},
    fs::{self, File, Metadata},
    io::{self, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex as BlockingMutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use cocoa_core::capability::{
    ActivityOutput, ActivityOutputKind, ActivitySummary, CapabilityError, CapabilityObserver,
    Effects,
};
use cocoa_core::engine::ActivityStatus;
use serde_json::{Value, json};
use similar::{DiffTag, TextDiff};
use tempfile::TempDir;
use tokio::{
    sync::{Mutex, Semaphore, oneshot},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;

use crate::{
    AgentScope, CodingContext, CodingEnvironment, CodingOperation, ReadSelector, tool,
    valid_variable, validate,
};

/// Explicit per-environment bounds. Construction validates every field.
/// `captured_output_bytes` is shared by stdout and stderr of one command;
/// it cannot exceed `file_bytes`, so retained text remains readable.
/// `retained_output_bytes` includes one capture-ceiling reservation per active
/// command, released down to its captured bytes after completion. Commands and
/// their output remain retained until close; `observations` also caps commands.
/// Byte limits are in `1..=u32::MAX`, observations in `1..=65536`, processes
/// in `1..=1024`, preview lines in `1..=200`, and timeouts in `1..=u32::MAX` ms.
/// Observed bytes must cover one maximum file, retained bytes one maximum capture.
#[derive(Clone, Copy, Debug)]
pub struct CodingLimits {
    pub file_bytes: usize,
    pub observed_bytes: usize,
    pub observations: usize,
    pub processes: usize,
    pub captured_output_bytes: usize,
    pub retained_output_bytes: usize,
    pub preview_lines: usize,
    /// Maximum interval without stdout or stderr bytes, not absolute duration.
    pub default_timeout: Duration,
}

/// Local paths and a deliberately selected environment, never ambient globals.
/// The shell must support Bash's `--noprofile --norc -o pipefail -c` options.
#[derive(Clone, Debug)]
pub struct LocalConfiguration {
    pub working_directory: PathBuf,
    pub shell: PathBuf,
    pub environment: BTreeMap<String, String>,
    pub output_directory: PathBuf,
}

/// Invalid construction arguments or inaccessible local directories.
#[derive(Debug, thiserror::Error)]
pub enum ConfigurationError {
    #[error("coding limits must be positive, bounded and internally consistent")]
    Limits,
    #[error("paths must be absolute UTF-8 and baseline variables must exclude startup hooks")]
    Configuration,
    #[error("cannot prepare local coding environment: {0}")]
    Io(#[from] io::Error),
}

/// One owned Unix backend. References survive calls and engine shutdown until
/// `close`. File operations run in tracked workers so dropping a caller requests
/// cancellation without abandoning a mutation. Accepted commands have their own
/// lifetime and settle only on completion, stall, explicit timeout/cancel or close.
pub struct LocalEnvironment {
    inner: Arc<Inner>,
    runtime: Mutex<Runtime>,
}

impl LocalEnvironment {
    /// Validate bounds and create a private output directory below the supplied
    /// existing directory. Neither directory is a sandbox or path confinement.
    ///
    /// # Errors
    /// Reject invalid limits, startup hooks, non-UTF-8/relative paths, or storage
    /// setup failures. Unsupported platforms fail at compilation.
    pub async fn new(
        configuration: LocalConfiguration,
        limits: CodingLimits,
    ) -> Result<Self, ConfigurationError> {
        validate_limits(limits)?;
        if [
            &configuration.working_directory,
            &configuration.output_directory,
            &configuration.shell,
        ]
        .iter()
        .any(|path| !path.is_absolute() || path.to_str().is_none())
            || !configuration
                .environment
                .iter()
                .all(|(name, value)| valid_variable(name) && !value.contains('\0'))
        {
            return Err(ConfigurationError::Configuration);
        }
        let (configuration, output) =
            tokio::task::spawn_blocking(move || -> Result<_, ConfigurationError> {
                let mut configuration = configuration;
                configuration.working_directory =
                    fs::canonicalize(&configuration.working_directory)?;
                configuration.output_directory = fs::canonicalize(&configuration.output_directory)?;
                if !configuration.working_directory.is_dir()
                    || !configuration.output_directory.is_dir()
                    || configuration.working_directory.to_str().is_none()
                    || configuration.output_directory.to_str().is_none()
                {
                    return Err(ConfigurationError::Configuration);
                }
                let output = tempfile::Builder::new()
                    .prefix("cocoa-output-")
                    .tempdir_in(&configuration.output_directory)?;
                Ok((configuration, output))
            })
            .await
            .map_err(|_| ConfigurationError::Configuration)??;
        let output_path = output.path().to_path_buf();
        Ok(Self {
            inner: Arc::new(Inner {
                configuration,
                limits,
                output_path,
                files: BlockingMutex::new(FileState::default()),
                retained: BlockingMutex::new(0),
                output: Mutex::new(Some(output)),
                processes: Arc::new(Semaphore::new(limits.processes)),
                operations: Arc::new(Semaphore::new(limits.observations + limits.processes)),
                shutdown: CancellationToken::new(),
            }),
            runtime: Mutex::new(Runtime {
                closed: false,
                workers: JoinSet::new(),
                commands: HashMap::new(),
                failure: None,
            }),
        })
    }

    /// Cancel and settle owned work, then remove snapshots and retained output.
    /// Idempotent and resumable if the close future itself is dropped.
    ///
    /// # Errors
    /// Report worker infrastructure or cleanup failures, including those already
    /// observed by callers. Ordinary operation errors and cancellation are not
    /// close failures. Retained worker failures prevent removal and remain errors
    /// on repeated close calls; filesystem removal failures can be retried.
    pub async fn close(&self) -> Result<(), CapabilityError> {
        self.inner.shutdown.cancel();
        let mut runtime = self.runtime.lock().await;
        runtime.closed = true;
        while let Some(result) = runtime.workers.join_next().await {
            runtime.record_worker(result);
        }
        if let Some(error) = &runtime.failure {
            return Err(error.clone());
        }
        runtime.commands.clear();
        self.inner
            .files
            .lock()
            .map_err(|_| tool("backend", "Snapshot state unavailable", Effects::None))?
            .snapshots
            .clear();
        let mut output = self.inner.output.lock().await;
        if let Some(directory) = output.as_ref() {
            match tokio::fs::remove_dir_all(directory.path()).await {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(io_error(error, Effects::None)),
            }
        }
        output.take();
        Ok(())
    }

    pub(crate) fn working_directory(&self) -> &Path {
        &self.inner.configuration.working_directory
    }

    pub(crate) fn operation_path(
        &self,
        operation: &CodingOperation,
        scope: AgentScope,
    ) -> Result<PathBuf, CapabilityError> {
        match operation {
            CodingOperation::Read {
                selector: ReadSelector::Path { path },
                ..
            } => Ok(self.working_directory().join(path)),
            CodingOperation::Run(arguments) => Ok(self
                .working_directory()
                .join(arguments.working_directory.as_deref().unwrap_or("."))),
            CodingOperation::Read {
                selector: ReadSelector::File { file },
                ..
            }
            | CodingOperation::Write { file, .. }
            | CodingOperation::Edit { file, .. } => {
                let state = self.inner.files.try_lock().map_err(|_| {
                    tool(
                        "capacity",
                        "File operation already active; no work queued",
                        Effects::None,
                    )
                })?;
                Ok(reference(&state, file, scope)?.path.clone())
            }
            CodingOperation::Wait { .. }
            | CodingOperation::Cancel { .. }
            | CodingOperation::LoadSkill { .. } => Err(tool(
                "invalid_arguments",
                "Operation has no coding filesystem target",
                Effects::None,
            )),
        }
    }
}

impl Drop for LocalEnvironment {
    fn drop(&mut self) {
        self.inner.shutdown.cancel();
        // Workers retain the environment until they settle; aborting them would
        // abandon their child processes and blocking persistence operations.
        self.runtime.get_mut().workers.detach_all();
    }
}

#[async_trait]
impl CodingEnvironment for LocalEnvironment {
    fn activity_title(&self, operation: &CodingOperation, scope: AgentScope) -> String {
        match self.operation_path(operation, scope) {
            Ok(path) => {
                let path = path.strip_prefix(self.working_directory()).unwrap_or(&path);
                operation.activity_title(path.to_str())
            }
            Err(_) => operation.activity_title(None),
        }
    }

    fn activity_output(
        &self,
        operation: &CodingOperation,
        scope: AgentScope,
        value: &Value,
    ) -> Option<ActivityOutput> {
        let file = match operation {
            CodingOperation::Write { file, .. } | CodingOperation::Edit { file, .. } => file,
            CodingOperation::Run(_) if value["kind"] == "process" => {
                let runtime = self.runtime.try_lock().ok()?;
                let entry = runtime.command(value["reference"].as_str()?, scope).ok()?;
                return entry.activity_output(self.inner.limits.preview_lines);
            }
            CodingOperation::Run(_) | CodingOperation::Wait { .. } => {
                return crate::command_output(value);
            }
            CodingOperation::Read { .. }
            | CodingOperation::Cancel { .. }
            | CodingOperation::LoadSkill { .. } => return None,
        };
        let state = self.inner.files.try_lock().ok()?;
        let before = reference(&state, file, scope).ok()?;
        let after = reference(&state, value["file"].as_str()?, scope).ok()?;
        let before = before
            .text
            .as_ref()
            .map_or("", |text| text.content.as_str());
        let after = &after.text.as_ref()?.content;
        Some(file_diff(before, after))
    }

    fn command_status(&self, reference: &str, scope: AgentScope) -> ActivityStatus {
        let Ok(runtime) = self.runtime.try_lock() else {
            return ActivityStatus::Unknown;
        };
        match runtime.command(reference, scope) {
            Ok(entry) => entry.activity_status(),
            Err(_) => ActivityStatus::Unknown,
        }
    }

    async fn observe_command(
        &self,
        reference: &str,
        scope: AgentScope,
        observer: CapabilityObserver,
    ) {
        let entry = self.runtime.lock().await.command(reference, scope);
        match entry {
            Ok(entry) => entry.observe(observer, self.inner.limits.preview_lines),
            Err(_) => observer.status(ActivityStatus::Unknown),
        }
    }

    fn description(&self) -> String {
        let limits = self.inner.limits;
        format!(
            "Local backend OS {}, architecture {}; Bash {:?}, --noprofile --norc -o pipefail -c, no PTY or ambient environment; errexit and nounset are not enabled. Subshells inherit pipefail; separately launched Bash scripts do not automatically inherit it in this cleared environment. Later commands can mask pipeline status; set +o pipefail explicitly overrides the default. SIGPIPE failures are not suppressed. Do not assume GNU-only utility options from the OS. Relative file inputs resolve against cwd {:?}; returned paths are absolute and separate from opaque references. This working directory is not a sandbox. Bounds: {} bytes/file, {} observed bytes, {} observations and separately the same maximum retained commands, {} concurrent processes without queueing, {} captured bytes/command shared across streams, {} retained captured bytes total, {} preview lines/stream, output-idle timeout {}ms reset by stdout/stderr, no default absolute command duration. Accepted commands survive caller deadlines/cancellation until explicit cancellation or environment close. Edit normalization additionally caps input/output at 8 MiB, source units at 1048576, each source unit at 4096 original bytes, and edits at 64; scans check cancellation/deadline every 1024 steps and between bounded units. JSON/host conversion limits may shorten previews/pages. File paths permit at most 256 components; write creates required parents cooperatively, with no rollback. Startup-hook variables BASH_ENV, ENV, SHELLOPTS, BASHOPTS, CDPATH, GLOBIGNORE, BASH_COMPAT, PROMPT_COMMAND, PS4 and exported function names are rejected. External-writer races are not atomic CAS; detached descendants may escape group cleanup.",
            std::env::consts::OS,
            std::env::consts::ARCH,
            self.inner.configuration.shell,
            self.inner.configuration.working_directory,
            limits.file_bytes,
            limits.observed_bytes,
            limits.observations,
            limits.processes,
            limits.captured_output_bytes,
            limits.retained_output_bytes,
            limits.preview_lines,
            limits.default_timeout.as_millis(),
        )
    }

    async fn execute(
        &self,
        operation: CodingOperation,
        mut context: CodingContext,
    ) -> Result<Value, CapabilityError> {
        validate(&operation)?;
        context.check()?;
        let arguments = serde_json::to_value(&operation).map_err(|_| {
            tool(
                "invalid_arguments",
                "Cannot encode coding arguments",
                Effects::None,
            )
        })?;
        if !context.fits(&arguments) {
            return Err(tool(
                "invalid_arguments",
                "Arguments exceed conversion bounds",
                Effects::None,
            ));
        }
        let permit = self
            .inner
            .operations
            .clone()
            .try_acquire_owned()
            .map_err(|_| {
                tool(
                    "capacity",
                    "Active operation capacity exhausted; no work queued",
                    Effects::None,
                )
            })?;
        let mut runtime = self.runtime.lock().await;
        if runtime.closed || self.inner.shutdown.is_cancelled() {
            return Err(tool(
                "backend",
                "Coding environment is closed",
                Effects::None,
            ));
        }
        while let Some(result) = runtime.workers.try_join_next() {
            runtime.record_worker(result);
        }
        if !matches!(
            operation,
            CodingOperation::Wait { .. } | CodingOperation::Cancel { .. }
        ) && let Some(error) = &runtime.failure
        {
            return Err(error.clone());
        }
        match operation {
            CodingOperation::Run(arguments) => {
                if runtime.commands.len() >= self.inner.limits.observations {
                    return Err(tool(
                        "capacity",
                        "Retained command capacity exhausted",
                        Effects::None,
                    ));
                }
                runtime.commands.try_reserve(1).map_err(|_| {
                    tool("capacity", "Cannot reserve command storage", Effects::None)
                })?;
                let command =
                    crate::process::accept(self.inner.clone(), arguments, &context).await?;
                let receipt = json!({"reference":command.reference,"kind":"process"});
                runtime
                    .commands
                    .insert(command.reference.clone(), command.entry.clone());
                runtime.workers.spawn(async move { command.settle().await });
                return Ok(receipt);
            }
            CodingOperation::Wait {
                handles,
                timeout_ms,
            } => {
                let mut entries = BTreeMap::new();
                for (alias, reference) in handles {
                    entries.insert(alias, runtime.command(&reference, context.scope)?);
                }
                drop(runtime);
                return crate::process::wait(entries, timeout_ms, &context, self.inner.limits)
                    .await;
            }
            CodingOperation::Cancel { reference } => {
                let entry = runtime.command(&reference, context.scope)?;
                context.check()?;
                return crate::process::cancel(&entry);
            }
            _ => {}
        }
        let token = context.cancellation.child_token();
        let guard = token.clone().drop_guard();
        context.cancellation = token.clone();
        let inner = self.inner.clone();
        let (sender, receiver) = oneshot::channel();
        runtime.workers.spawn(async move {
            let _permit = permit;
            let work = execute_owned(inner.clone(), operation, context);
            tokio::pin!(work);
            let result = tokio::select! {
                result = &mut work => result,
                () = inner.shutdown.cancelled() => { token.cancel(); work.await }
            };
            let failure = match &result {
                Err(error @ CapabilityError::Tool { code, .. }) if code == "backend" => {
                    Err(error.clone())
                }
                _ => Ok(()),
            };
            if sender.send(result).is_err() {
                tracing::trace!("Coding caller dropped; owned worker settled");
            }
            failure
        });
        drop(runtime);
        let result = receiver
            .await
            .map_err(|_| tool("backend", "Owned coding worker failed", Effects::Unknown))?;
        guard.disarm();
        result
    }
}

pub(crate) struct Inner {
    pub configuration: LocalConfiguration,
    pub limits: CodingLimits,
    pub output_path: PathBuf,
    files: BlockingMutex<FileState>,
    pub retained: BlockingMutex<usize>,
    output: Mutex<Option<TempDir>>,
    pub processes: Arc<Semaphore>,
    operations: Arc<Semaphore>,
    pub shutdown: CancellationToken,
}

struct Runtime {
    closed: bool,
    workers: JoinSet<Result<(), CapabilityError>>,
    commands: HashMap<String, Arc<crate::process::CommandEntry>>,
    failure: Option<CapabilityError>,
}

impl Runtime {
    fn command(
        &self,
        reference: &str,
        scope: AgentScope,
    ) -> Result<Arc<crate::process::CommandEntry>, CapabilityError> {
        let entry = self.commands.get(reference).ok_or_else(|| {
            tool(
                "invalid_reference",
                "Process reference does not belong to this agent/environment",
                Effects::None,
            )
        })?;
        crate::process::check_scope(entry, scope)?;
        Ok(entry.clone())
    }

    fn record_worker(
        &mut self,
        result: Result<Result<(), CapabilityError>, tokio::task::JoinError>,
    ) {
        let result = result
            .unwrap_or_else(|_| Err(tool("backend", "Owned worker failed", Effects::Unknown)));
        if let Err(error) = result {
            self.failure.get_or_insert(error);
        }
    }
}

#[derive(Default)]
struct FileState {
    snapshots: HashMap<String, Snapshot>,
    bytes: usize,
    next: u64,
}

struct Snapshot {
    scope: AgentScope,
    path: PathBuf,
    text: Option<TextObservation>,
}

#[derive(PartialEq, Eq)]
struct TextObservation {
    content: String,
    stamp: Stamp,
}

#[derive(PartialEq, Eq)]
struct Stamp {
    device: u64,
    inode: u64,
    mode: u32,
    length: u64,
    modified: i64,
    modified_nanos: i64,
}

async fn execute_owned(
    inner: Arc<Inner>,
    operation: CodingOperation,
    context: CodingContext,
) -> Result<Value, CapabilityError> {
    context.check()?;
    tokio::task::spawn_blocking(move || {
        context.check()?;
        let mut state = inner.files.try_lock().map_err(|_| {
            tool(
                "capacity",
                "File operation already active; no work queued",
                Effects::None,
            )
        })?;
        match operation {
            CodingOperation::Read {
                selector: ReadSelector::Path { path },
                start_line,
                line_count,
            } => {
                reserve(&inner, &mut state, 0)?;
                let path = inner.configuration.working_directory.join(path);
                let maximum = inner
                    .limits
                    .file_bytes
                    .min(inner.limits.observed_bytes - state.bytes);
                let text = observe(&path, maximum)?;
                insert(
                    &inner,
                    &mut state,
                    Snapshot {
                        scope: context.scope,
                        path,
                        text,
                    },
                    start_line,
                    line_count,
                    &context,
                )
            }
            CodingOperation::Read {
                selector: ReadSelector::File { file },
                start_line,
                line_count,
            } => {
                let snapshot = reference(&state, &file, context.scope)?;
                read_page(snapshot, &file, start_line, line_count, &context)
            }
            CodingOperation::Write { file, content } => {
                replace(&inner, &mut state, &file, content, &context)
            }
            CodingOperation::Edit { file, edits } => {
                let snapshot = reference(&state, &file, context.scope)?;
                let text = snapshot.text.as_ref().ok_or_else(|| {
                    tool(
                        "invalid_reference",
                        "Cannot edit a missing observation",
                        Effects::None,
                    )
                })?;
                let updated = crate::matching::apply(
                    &text.content,
                    &edits,
                    inner.limits.file_bytes,
                    &context,
                )?;
                replace(&inner, &mut state, &file, updated, &context)
            }
            CodingOperation::Run(_)
            | CodingOperation::Wait { .. }
            | CodingOperation::Cancel { .. }
            | CodingOperation::LoadSkill { .. } => {
                Err(tool("backend", "Invalid file dispatch", Effects::None))
            }
        }
    })
    .await
    .map_err(|_| tool("backend", "File worker failed", Effects::Unknown))?
}

fn file_diff(before: &str, after: &str) -> ActivityOutput {
    let diff = TextDiff::configure()
        .deadline(Instant::now() + Duration::from_millis(10))
        .diff_lines(before, after);
    let mut added = 0;
    let mut removed = 0;
    for operation in diff.ops() {
        if operation.tag() != DiffTag::Equal {
            added += operation.new_range().len() as u64;
            removed += operation.old_range().len() as u64;
        }
    }
    let mut text = String::new();
    let mut truncated = false;
    'hunks: for hunk in diff.unified_diff().iter_hunks() {
        truncated |= crate::append_activity(&mut text, &format!("{}\n", hunk.header()));
        for change in hunk.iter_changes() {
            truncated |= crate::append_activity(&mut text, &change.tag().to_string());
            truncated |= crate::append_activity(&mut text, change.value());
            if change.missing_newline() {
                truncated |= crate::append_activity(&mut text, "\n\\ No newline at end of file\n");
            }
            if truncated {
                break 'hunks;
            }
        }
    }
    if text.is_empty() {
        text.push_str("No file changes.");
    }
    ActivityOutput {
        text,
        kind: ActivityOutputKind::Diff,
        summary: Some(ActivitySummary::LineChanges { added, removed }),
        truncated,
    }
}

fn validate_limits(limits: CodingLimits) -> Result<(), ConfigurationError> {
    let byte_limits = [
        limits.file_bytes,
        limits.observed_bytes,
        limits.captured_output_bytes,
        limits.retained_output_bytes,
    ];
    if byte_limits
        .into_iter()
        .any(|limit| limit == 0 || limit > u32::MAX as usize)
        || limits.observed_bytes < limits.file_bytes
        || limits.captured_output_bytes > limits.file_bytes
        || limits.retained_output_bytes < limits.captured_output_bytes
        || !(1..=65536).contains(&limits.observations)
        || !(1..=1024).contains(&limits.processes)
        || !(1..=200).contains(&limits.preview_lines)
        || limits.default_timeout < Duration::from_millis(1)
        || limits.default_timeout > Duration::from_millis(u64::from(u32::MAX))
    {
        return Err(ConfigurationError::Limits);
    }
    Ok(())
}

fn observe(path: &Path, maximum: usize) -> Result<Option<TextObservation>, CapabilityError> {
    if path.components().count() > 256 {
        return Err(tool(
            "invalid_arguments",
            "path must contain at most 256 components",
            Effects::None,
        ));
    }
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(path_error(error, path, Effects::None)),
    };
    if !metadata.is_file() {
        return Err(tool(
            "unsupported_file",
            "Only regular non-symlink UTF-8 files are supported",
            Effects::None,
        ));
    }
    if metadata.len() > maximum as u64 {
        return Err(tool("capacity", "File exceeds byte limit", Effects::None));
    }
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(path)
        .map_err(|error| io_error(error, Effects::None))?;
    let opened = file
        .metadata()
        .map_err(|error| io_error(error, Effects::None))?;
    if !opened.is_file() || Stamp::from(&metadata) != Stamp::from(&opened) {
        return Err(tool(
            "stale_read",
            &format!("{} changed while opening; read again", path_label(path)),
            Effects::None,
        ));
    }
    let mut bytes = Vec::with_capacity((opened.len() as usize).min(maximum));
    file.take(maximum as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| io_error(error, Effects::None))?;
    if bytes.len() > maximum {
        return Err(tool("capacity", "File exceeds byte limit", Effects::None));
    }
    let content = String::from_utf8(bytes)
        .map_err(|_| tool("invalid_encoding", "File is not valid UTF-8", Effects::None))?;
    Ok(Some(TextObservation {
        content,
        stamp: Stamp::from(&opened),
    }))
}

impl From<&Metadata> for Stamp {
    fn from(metadata: &Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            mode: metadata.mode(),
            length: metadata.len(),
            modified: metadata.mtime(),
            modified_nanos: metadata.mtime_nsec(),
        }
    }
}

fn reference<'a>(
    state: &'a FileState,
    file: &str,
    scope: AgentScope,
) -> Result<&'a Snapshot, CapabilityError> {
    state
        .snapshots
        .get(file)
        .filter(|snapshot| snapshot.scope == scope)
        .ok_or_else(|| {
            tool(
                "invalid_reference",
                "Reference does not belong to this agent/environment",
                Effects::None,
            )
        })
}

fn metadata(snapshot: &Snapshot, file: &str) -> Value {
    match &snapshot.text {
        Some(text) => {
            json!({"kind":"text","file":file,"path":snapshot.path,"bytes":text.content.len(),"line_count":text.content.split_inclusive('\n').count()})
        }
        None => json!({"kind":"missing","file":file,"path":snapshot.path}),
    }
}

fn reserve(inner: &Inner, state: &mut FileState, bytes: usize) -> Result<(), CapabilityError> {
    check_file_bytes(inner, bytes)?;
    if state.snapshots.len() >= inner.limits.observations
        || bytes > inner.limits.observed_bytes.saturating_sub(state.bytes)
    {
        return Err(tool(
            "capacity",
            "Observation capacity exhausted; existing references remain valid",
            Effects::None,
        ));
    }
    state.snapshots.try_reserve(1).map_err(|_| {
        tool(
            "capacity",
            "Cannot reserve observation storage",
            Effects::None,
        )
    })?;
    Ok(())
}

fn check_file_bytes(inner: &Inner, bytes: usize) -> Result<(), CapabilityError> {
    if bytes > inner.limits.file_bytes {
        return Err(tool("capacity", "File exceeds byte limit", Effects::None));
    }
    Ok(())
}

fn next_reference(inner: &Inner, state: &FileState) -> String {
    format!(
        "file_{}_{}",
        inner
            .output_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("local"),
        state.next
    )
}

fn insert(
    inner: &Inner,
    state: &mut FileState,
    snapshot: Snapshot,
    start_line: u32,
    line_count: u32,
    context: &CodingContext,
) -> Result<Value, CapabilityError> {
    let bytes = snapshot.text.as_ref().map_or(0, |text| text.content.len());
    reserve(inner, state, bytes)?;
    let file = next_reference(inner, state);
    let value = read_page(&snapshot, &file, start_line, line_count, context)?;
    if !context.fits(&value) {
        return Err(CapabilityError::OutputLimit);
    }
    context.check()?;
    state.bytes += bytes;
    state.next += 1;
    state.snapshots.insert(file, snapshot);
    Ok(value)
}

fn read_page(
    snapshot: &Snapshot,
    file: &str,
    start_line: u32,
    line_count: u32,
    context: &CodingContext,
) -> Result<Value, CapabilityError> {
    let mut value = metadata(snapshot, file);
    let Some(text) = snapshot.text.as_ref() else {
        return Ok(value);
    };
    let mut lines = text
        .content
        .split_inclusive('\n')
        .skip(start_line as usize - 1)
        .peekable();
    let mut text = String::new();
    let mut count = 0;
    let mut next_line = None;
    for _ in 0..line_count {
        context.check()?;
        let Some(line) = lines.next() else {
            break;
        };
        let candidate = format!("{text}{line}");
        let next = lines.peek().map(|_| start_line + count + 1);
        value["page"] = json!({"start_line":start_line,"line_count":count + 1,"text":candidate});
        value["next"] = next.map_or(
            Value::Null,
            |start_line| json!({"file":file,"start_line":start_line,"line_count":line_count}),
        );
        if !context.fits(&value) {
            if count == 0 {
                return Err(tool(
                    "line_too_large",
                    "First requested line exceeds byte/conversion limits",
                    Effects::None,
                ));
            }
            next_line = Some(start_line + count);
            break;
        }
        text = candidate;
        count += 1;
        next_line = next;
    }
    value["page"] = json!({"start_line":start_line,"line_count":count,"text":text});
    value["next"] = next_line.map_or(
        Value::Null,
        |start_line| json!({"file":file,"start_line":start_line,"line_count":line_count}),
    );
    if !context.fits(&value) {
        return Err(CapabilityError::OutputLimit);
    }
    Ok(value)
}

fn replace(
    inner: &Inner,
    state: &mut FileState,
    file: &str,
    content: String,
    context: &CodingContext,
) -> Result<Value, CapabilityError> {
    reserve(inner, state, content.len())?;
    let previous = reference(state, file, context.scope)?;
    let current =
        observe(&previous.path, inner.limits.file_bytes).map_err(|error| match error {
            CapabilityError::Tool { code, .. }
                if matches!(
                    code.as_str(),
                    "not_found" | "unsupported_file" | "invalid_encoding" | "capacity"
                ) =>
            {
                tool(
                    "stale_read",
                    &format!(
                        "{} is no longer the observed text file; read again",
                        path_label(&previous.path)
                    ),
                    Effects::None,
                )
            }
            error => error,
        })?;
    if current != previous.text {
        return Err(tool(
            "stale_read",
            &format!(
                "{} changed since observation; read again and reconsider",
                path_label(&previous.path)
            ),
            Effects::None,
        ));
    }
    let parent = previous
        .path
        .parent()
        .ok_or_else(|| tool("not_found", "File parent does not exist", Effects::None))?;
    let fresh = next_reference(inner, state);
    let preview = json!({"kind":"text","file":fresh,"path":previous.path,"bytes":content.len(),"line_count":content.split_inclusive('\n').count()});
    if !context.fits(&preview) {
        return Err(CapabilityError::OutputLimit);
    }
    let mut created = false;
    let prepared = (|| {
        if previous.text.is_none() {
            let parents: Vec<_> = parent.ancestors().collect();
            for directory in parents.iter().rev() {
                context.check()?;
                match fs::create_dir(directory) {
                    Ok(()) => created = true,
                    Err(error)
                        if error.kind() == io::ErrorKind::AlreadyExists && directory.is_dir() => {}
                    Err(error) => return Err(path_error(error, &previous.path, Effects::None)),
                }
            }
        }
        context.check()?;
        let mut prepared = tempfile::NamedTempFile::new_in(parent)
            .map_err(|error| path_error(error, &previous.path, Effects::None))?;
        prepared
            .write_all(content.as_bytes())
            .map_err(|error| io_error(error, Effects::None))?;
        if let Some(text) = &previous.text {
            prepared
                .as_file()
                .set_permissions(fs::Permissions::from_mode(text.stamp.mode))
                .map_err(|error| io_error(error, Effects::None))?;
        }
        context.check()?;
        Ok::<_, CapabilityError>(prepared)
    })()
    .map_err(|error| parent_effects(error, created))?;
    let stamp = prepared
        .as_file()
        .metadata()
        .map_err(|error| parent_effects(io_error(error, Effects::None), created))?;
    let snapshot = Snapshot {
        scope: context.scope,
        path: previous.path.clone(),
        text: Some(TextObservation {
            content,
            stamp: Stamp::from(&stamp),
        }),
    };
    let value = metadata(&snapshot, &fresh);
    debug_assert_eq!(
        value, preview,
        "prepared observation must match the reserved response"
    );
    context.check()?;
    let committed: Result<File, _> = if previous.text.is_some() {
        prepared.persist(&previous.path)
    } else {
        prepared.persist_noclobber(&previous.path)
    };
    if let Err(error) = committed {
        if error.error.kind() == io::ErrorKind::AlreadyExists {
            return Err(tool(
                "stale_read",
                &format!("{} now exists; read again", path_label(&previous.path)),
                if created {
                    Effects::Partial
                } else {
                    Effects::None
                },
            ));
        }
        return Err(io_error(error.error, Effects::Unknown));
    }
    // All quota and conversion checks precede commit; registration cannot reject it.
    state.bytes += snapshot.text.as_ref().map_or(0, |text| text.content.len());
    state.next += 1;
    state.snapshots.insert(fresh, snapshot);
    Ok(value)
}

pub(crate) fn io_error(error: io::Error, effects: Effects) -> CapabilityError {
    let code = match error.kind() {
        io::ErrorKind::NotFound => "not_found",
        io::ErrorKind::NotADirectory => "not_directory",
        io::ErrorKind::PermissionDenied => "permission_denied",
        _ => "storage",
    };
    tool(code, &format!("Local I/O failure: {error}"), effects)
}

fn path_error(error: io::Error, target: &Path, effects: Effects) -> CapabilityError {
    if matches!(
        error.kind(),
        io::ErrorKind::NotADirectory | io::ErrorKind::AlreadyExists
    ) {
        for component in target.ancestors().skip(1) {
            if fs::metadata(component).is_ok_and(|metadata| !metadata.is_dir()) {
                return tool(
                    "not_directory",
                    &format!(
                        "{} blocks a required directory for {}",
                        path_label(component),
                        path_label(target)
                    ),
                    effects,
                );
            }
        }
    }
    io_error(error, effects)
}

fn parent_effects(error: CapabilityError, created: bool) -> CapabilityError {
    match error {
        CapabilityError::Tool {
            code,
            message,
            effects: Effects::None,
        } if created => CapabilityError::Tool {
            code,
            message,
            effects: Effects::Partial,
        },
        error => error,
    }
}

fn path_label(path: &Path) -> String {
    let text = path.to_string_lossy();
    if text.len() <= 160 {
        return text.into_owned();
    }
    format!(
        "{}…{}",
        &text[..text.floor_char_boundary(80)],
        &text[text.ceil_char_boundary(text.len() - 80)..]
    )
}
