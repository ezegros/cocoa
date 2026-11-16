//! Owned process groups, repeatable snapshots and bounded retained output.

use std::{
    collections::BTreeMap,
    io,
    os::unix::process::ExitStatusExt,
    process::{ExitStatus, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use cocoa_core::capability::{ActivityOutput, CapabilityError, CapabilityObserver, Effects};
use cocoa_core::engine::ActivityStatus;
use process_wrap::tokio::{ChildWrapper, CommandWrap, ProcessGroup};
use rustix::process::{Pid, WaitId, WaitIdOptions, test_kill_process_group, waitid};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::OwnedSemaphorePermit,
};
use tokio_util::sync::CancellationToken;

use crate::{
    AgentScope, CodingContext, RunArguments,
    local::{Inner, io_error},
    tool,
};

pub(crate) struct CommandEntry {
    scope: AgentScope,
    cancellation: CancellationToken,
    state: Mutex<CommandState>,
    stdout_path: String,
    stderr_path: String,
    _directory: TempDir,
}

struct CommandState {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_exposed: bool,
    stderr_exposed: bool,
    terminal: Option<Result<ExitStatus, CapabilityError>>,
    observer: Option<CapabilityObserver>,
}

pub(crate) struct AcceptedCommand {
    pub reference: String,
    pub entry: Arc<CommandEntry>,
    child: Box<dyn ChildWrapper>,
    process_group: Pid,
    stdout_file: tokio::fs::File,
    stderr_file: tokio::fs::File,
    arguments: RunArguments,
    inner: Arc<Inner>,
    reservation: Reservation,
    _permit: OwnedSemaphorePermit,
    accepted: tokio::time::Instant,
}

/// All fallible capacity and receipt checks precede spawning. There is no await
/// between spawning and transferring ownership to the environment's worker set.
pub(crate) async fn accept(
    inner: Arc<Inner>,
    arguments: RunArguments,
    context: &CodingContext,
) -> Result<AcceptedCommand, CapabilityError> {
    static NEXT_COMMAND: AtomicU64 = AtomicU64::new(1);
    let permit = inner.processes.clone().try_acquire_owned().map_err(|_| {
        tool(
            "capacity",
            "Process capacity exhausted; no command queued",
            Effects::None,
        )
    })?;
    let reservation = Reservation::new(inner.clone())?;
    let directory = tempfile::Builder::new()
        .prefix("command-")
        .tempdir_in(&inner.output_path)
        .map_err(|error| io_error(error, Effects::None))?;
    let stdout_path = directory.path().join("stdout");
    let stderr_path = directory.path().join("stderr");
    let stdout_file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&stdout_path)
        .await
        .map_err(|error| io_error(error, Effects::None))?;
    let stderr_file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&stderr_path)
        .await
        .map_err(|error| io_error(error, Effects::None))?;
    let path_text = |path: &std::path::Path| {
        path.to_str()
            .map(str::to_owned)
            .ok_or_else(|| tool("storage", "Output path is not UTF-8", Effects::None))
    };
    let stdout_path = path_text(&stdout_path)?;
    let stderr_path = path_text(&stderr_path)?;
    let reference = format!(
        "process.{}.{}",
        context.scope.0,
        NEXT_COMMAND.fetch_add(1, Ordering::Relaxed)
    );
    if !context.fits(&json!({"reference":reference,"kind":"process"})) {
        return Err(CapabilityError::OutputLimit);
    }
    let working_directory = arguments.working_directory.as_deref().map_or_else(
        || inner.configuration.working_directory.clone(),
        |path| inner.configuration.working_directory.join(path),
    );
    let mut environment = inner.configuration.environment.clone();
    if let Some(overrides) = &arguments.environment {
        for (name, value) in overrides {
            match value {
                Some(value) => {
                    environment.insert(name.clone(), value.clone());
                }
                None => {
                    environment.remove(name);
                }
            }
        }
    }
    let mut command = CommandWrap::with_new(&inner.configuration.shell, |command| {
        command
            .args([
                "--noprofile",
                "--norc",
                "-o",
                "pipefail",
                "-c",
                &arguments.command,
            ])
            .current_dir(working_directory)
            .env_clear()
            .envs(environment)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
    });
    command.wrap(ProcessGroup::leader());
    context.check()?;
    let child = command.spawn().map_err(|error| {
        tool(
            "process_start",
            &format!("Could not start configured Bash: {error}"),
            Effects::None,
        )
    })?;
    let process_group = child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .and_then(Pid::from_raw)
        .ok_or_else(|| {
            tool(
                "backend",
                "Spawned process has no valid ID",
                Effects::Unknown,
            )
        })?;
    let entry = Arc::new(CommandEntry {
        scope: context.scope,
        cancellation: CancellationToken::new(),
        state: Mutex::new(CommandState {
            stdout: Vec::new(),
            stderr: Vec::new(),
            stdout_exposed: false,
            stderr_exposed: false,
            terminal: None,
            observer: None,
        }),
        stdout_path,
        stderr_path,
        _directory: directory,
    });
    Ok(AcceptedCommand {
        reference,
        entry,
        child,
        process_group,
        stdout_file,
        stderr_file,
        arguments,
        inner,
        reservation,
        _permit: permit,
        accepted: tokio::time::Instant::now(),
    })
}

pub(crate) fn check_scope(entry: &CommandEntry, scope: AgentScope) -> Result<(), CapabilityError> {
    if entry.scope != scope {
        return Err(tool(
            "invalid_reference",
            "Process reference does not belong to this agent/environment",
            Effects::None,
        ));
    }
    Ok(())
}

pub(crate) fn cancel(entry: &CommandEntry) -> Result<Value, CapabilityError> {
    let state = lock(entry)?;
    let status = match &state.terminal {
        Some(Ok(_)) => "completed",
        Some(Err(_)) => "failed",
        None => {
            entry.cancellation.cancel();
            "cancelling"
        }
    };
    Ok(json!({"state":status}))
}

pub(crate) async fn wait(
    entries: BTreeMap<String, Arc<CommandEntry>>,
    timeout_ms: Option<u32>,
    context: &CodingContext,
    limits: crate::CodingLimits,
) -> Result<Value, CapabilityError> {
    if entries.len() > 32 {
        return Err(tool(
            "invalid_arguments",
            "handles must contain at most 32 references",
            Effects::None,
        ));
    }
    if entries.is_empty() {
        return Ok(json!({}));
    }
    let deadline = context.limits.deadline.min(
        tokio::time::Instant::now()
            + Duration::from_millis(u64::from(timeout_ms.unwrap_or(30_000))),
    );
    let mut interval = tokio::time::interval(Duration::from_millis(5));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        if context.cancellation.is_cancelled() {
            return Err(CapabilityError::Cancelled);
        }
        let mut complete = true;
        for entry in entries.values() {
            complete &= lock(entry)?.terminal.is_some();
        }
        if complete || tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::select! {
            () = context.cancellation.cancelled() => return Err(CapabilityError::Cancelled),
            () = tokio::time::sleep_until(deadline) => break,
            _ = interval.tick() => {}
        }
    }
    let framing = serde_json::to_vec(
        &entries
            .keys()
            .map(|alias| (alias, Value::Null))
            .collect::<BTreeMap<_, _>>(),
    )
    .map_err(|_| tool("backend", "Cannot encode wait aliases", Effects::None))?
    .len();
    let mut output_context = context.clone();
    output_context.limits.output_bytes =
        context.limits.output_bytes.saturating_sub(framing) / entries.len();
    let mut result = serde_json::Map::new();
    let mut snapshots: Vec<(Arc<CommandEntry>, Value)> = Vec::with_capacity(entries.len());
    for (alias, entry) in entries {
        let value = match snapshots
            .iter()
            .find(|(previous, _)| Arc::ptr_eq(previous, &entry))
        {
            Some((_, value)) => value.clone(),
            None => {
                let value = snapshot(&entry, limits.preview_lines, &output_context)?;
                snapshots.push((entry, value.clone()));
                value
            }
        };
        result.insert(alias, value);
    }
    let result = Value::Object(result);
    if !context.fits(&result) {
        return Err(CapabilityError::OutputLimit);
    }
    Ok(result)
}

impl CommandEntry {
    pub(crate) fn activity_output(&self, lines: usize) -> Option<ActivityOutput> {
        let state = lock(self).ok()?;
        let preview = |bytes: &[u8]| {
            let text = String::from_utf8_lossy(bytes);
            let end = text
                .split_inclusive('\n')
                .take(lines)
                .map(str::len)
                .sum::<usize>();
            let end = text.floor_char_boundary(end.min(crate::ACTIVITY_BYTES));
            json!({"text": &text[..end], "truncated": end < text.len()})
        };
        crate::command_output(&json!({
            "stdout": preview(&state.stdout),
            "stderr": preview(&state.stderr),
        }))
    }

    pub(crate) fn activity_status(&self) -> ActivityStatus {
        let Ok(state) = lock(self) else {
            return ActivityStatus::Unknown;
        };
        match &state.terminal {
            Some(Ok(status)) if status.success() => ActivityStatus::Completed,
            Some(Ok(status)) => match status.signal() {
                Some(signal) => ActivityStatus::Signalled { signal },
                None => ActivityStatus::Failed {
                    exit_code: status.code(),
                },
            },
            Some(Err(error)) if error.effects() == Effects::Unknown => ActivityStatus::Unknown,
            Some(Err(CapabilityError::Tool { code, .. })) if code == "timeout" => {
                ActivityStatus::TimedOut
            }
            Some(Err(_)) => ActivityStatus::Failed { exit_code: None },
            None => ActivityStatus::Running,
        }
    }

    pub(crate) fn observe(&self, observer: CapabilityObserver, lines: usize) {
        let Ok(mut state) = lock(self) else {
            observer.status(ActivityStatus::Unknown);
            return;
        };
        if state.terminal.is_none() {
            observer.status(ActivityStatus::Running);
            state.observer = Some(observer);
        } else {
            drop(state);
            self.publish(&observer, lines);
        }
    }

    fn publish(&self, observer: &CapabilityObserver, lines: usize) {
        if let Some(output) = self.activity_output(lines) {
            observer.output(output);
        }
        observer.status(self.activity_status());
    }
}

impl AcceptedCommand {
    pub async fn settle(mut self) -> Result<(), CapabilityError> {
        let result = self.supervise().await;
        let bytes = {
            let state = lock(&self.entry)?;
            state.stdout.len() + state.stderr.len()
        };
        let result = match self.reservation.commit(bytes) {
            Ok(()) => result,
            Err(error) => Err(error),
        };
        let failure = result
            .as_ref()
            .err()
            .filter(|error| {
                matches!(error, CapabilityError::Unsettled { .. })
                    || error.effects() == Effects::Unknown
            })
            .cloned();
        let observer = {
            let mut state = lock(&self.entry)?;
            state.terminal = Some(result);
            state.observer.take()
        };
        if let Some(observer) = observer {
            self.entry
                .publish(&observer, self.inner.limits.preview_lines);
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    async fn supervise(&mut self) -> Result<ExitStatus, CapabilityError> {
        let stdout = self.child.stdout().take();
        let stderr = self.child.stderr().take();
        let stdin = self.child.stdin().take();
        let mut status = None;
        let mut cleanup_attempted = false;
        let result = async {
            let mut stdout = stdout.ok_or_else(|| tool("backend", "Missing stdout pipe", Effects::Partial))?;
            let mut stderr = stderr.ok_or_else(|| tool("backend", "Missing stderr pipe", Effects::Partial))?;
            let mut stdin = stdin.ok_or_else(|| tool("backend", "Missing stdin pipe", Effects::Partial))?;
            let send_input = async {
                if let Err(error) = stdin.write_all(self.arguments.stdin.as_deref().unwrap_or("").as_bytes()).await
                    && error.kind() != io::ErrorKind::BrokenPipe { return Err(io_error(error, Effects::Partial)); }
                drop(stdin);
                Ok(())
            };
            tokio::pin!(send_input);
            let mut input_sent = false;
            let mut stdout_done = false;
            let mut stderr_done = false;
            let mut stdout_buffer = [0_u8; 8192];
            let mut stderr_buffer = [0_u8; 8192];
            let mut last_output = self.accepted;
            let absolute = async {
                match self.arguments.timeout_ms {
                    Some(milliseconds) => tokio::time::sleep_until(self.accepted + Duration::from_millis(u64::from(milliseconds))).await,
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::pin!(absolute);
            let mut interval = tokio::time::interval(Duration::from_millis(5));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                if status.is_some() && stdout_done && stderr_done { return Ok(()); }
                tokio::select! {
                    biased;
                    () = self.inner.shutdown.cancelled() => {
                        if status.is_none() {
                            cleanup_attempted = true;
                            status = Some(cleanup(self.child.as_mut(), self.process_group).await?);
                        }
                        return Ok(());
                    }
                    () = self.entry.cancellation.cancelled() => {
                        if status.is_none() {
                            cleanup_attempted = true;
                            status = Some(cleanup(self.child.as_mut(), self.process_group).await?);
                        }
                        return Ok(());
                    }
                    () = &mut absolute => return Err(tool("timeout", "Command absolute timeout reached", Effects::Partial)),
                    () = tokio::time::sleep_until(last_output + self.inner.limits.default_timeout) => return Err(tool("timeout", "Command output stalled", Effects::Partial)),
                    read = stdout.read(&mut stdout_buffer), if !stdout_done => {
                        let length = read.map_err(|error| io_error(error, Effects::Partial))?;
                        stdout_done = length == 0;
                        if length > 0 {
                            last_output = tokio::time::Instant::now();
                            capture(&self.entry, &mut self.stdout_file, &stdout_buffer[..length], OutputStream::Stdout, self.inner.limits.captured_output_bytes).await?;
                        }
                    }
                    read = stderr.read(&mut stderr_buffer), if !stderr_done => {
                        let length = read.map_err(|error| io_error(error, Effects::Partial))?;
                        stderr_done = length == 0;
                        if length > 0 {
                            last_output = tokio::time::Instant::now();
                            capture(&self.entry, &mut self.stderr_file, &stderr_buffer[..length], OutputStream::Stderr, self.inner.limits.captured_output_bytes).await?;
                        }
                    }
                    result = &mut send_input, if !input_sent => { result?; input_sent = true; }
                    _ = interval.tick(), if status.is_none() => {
                        match waitid(WaitId::Pid(self.process_group), WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG) {
                            Ok(Some(_)) => {
                                cleanup_attempted = true;
                                status = Some(cleanup(self.child.as_mut(), self.process_group).await?);
                            }
                            Ok(None) => {}
                            Err(error) => return Err(io_error(error.into(), Effects::Unknown)),
                        }
                    }
                }
            }
        }.await;
        if let Err(error) = result {
            if !cleanup_attempted {
                cleanup(self.child.as_mut(), self.process_group).await?;
            }
            return Err(error);
        }
        status.ok_or_else(|| {
            tool(
                "backend",
                "Process completed without status evidence",
                Effects::Unknown,
            )
        })
    }
}

fn lock(entry: &CommandEntry) -> Result<std::sync::MutexGuard<'_, CommandState>, CapabilityError> {
    entry
        .state
        .lock()
        .map_err(|_| tool("backend", "Command state unavailable", Effects::Unknown))
}

enum OutputStream {
    Stdout,
    Stderr,
}

async fn capture(
    entry: &CommandEntry,
    file: &mut tokio::fs::File,
    bytes: &[u8],
    stream: OutputStream,
    maximum: usize,
) -> Result<(), CapabilityError> {
    {
        let state = lock(entry)?;
        if bytes.len() > maximum.saturating_sub(state.stdout.len() + state.stderr.len()) {
            return Err(tool(
                "storage",
                "Command exceeded captured-output byte limit; process group terminated",
                Effects::Partial,
            ));
        }
    }
    file.write_all(bytes)
        .await
        .map_err(|error| io_error(error, Effects::Partial))?;
    file.flush()
        .await
        .map_err(|error| io_error(error, Effects::Partial))?;
    let mut state = lock(entry)?;
    match stream {
        OutputStream::Stdout => state.stdout.extend_from_slice(bytes),
        OutputStream::Stderr => state.stderr.extend_from_slice(bytes),
    }
    Ok(())
}

fn snapshot(
    entry: &CommandEntry,
    lines: usize,
    context: &CodingContext,
) -> Result<Value, CapabilityError> {
    let mut state = lock(entry)?;
    let status = match &state.terminal {
        None => json!({"kind":"running"}),
        Some(Ok(status)) => status_value(*status)?,
        Some(Err(CapabilityError::Tool { code, .. })) if code == "timeout" => {
            return Err(timeout_error(&state.stdout, &state.stderr, context));
        }
        Some(Err(error)) => return Err(error.clone()),
    };
    if !context.fits(&preview_frame(
        "",
        &entry.stdout_path,
        "",
        &entry.stderr_path,
    )) {
        return Err(CapabilityError::OutputLimit);
    }
    let stdout = preview(
        &state.stdout,
        lines,
        &entry.stdout_path,
        &entry.stderr_path,
        context,
    );
    let stderr = preview(
        &state.stderr,
        lines,
        &entry.stdout_path,
        &entry.stderr_path,
        context,
    );
    let stdout_truncated = stdout.0.len() < String::from_utf8_lossy(&state.stdout).len();
    let stderr_truncated = stderr.0.len() < String::from_utf8_lossy(&state.stderr).len();
    state.stdout_exposed |= stdout_truncated;
    state.stderr_exposed |= stderr_truncated;
    Ok(json!({
        "status":status,
        "stdout":{"text":stdout.0,"lossy":stdout.1,"truncated":stdout_truncated,"output_path":if state.stdout_exposed { Some(&entry.stdout_path) } else { None }},
        "stderr":{"text":stderr.0,"lossy":stderr.1,"truncated":stderr_truncated,"output_path":if state.stderr_exposed { Some(&entry.stderr_path) } else { None }},
    }))
}

struct Reservation {
    inner: Arc<Inner>,
    bytes: usize,
}

impl Reservation {
    fn new(inner: Arc<Inner>) -> Result<Self, CapabilityError> {
        let bytes = inner.limits.captured_output_bytes;
        {
            let mut retained = inner
                .retained
                .lock()
                .map_err(|_| tool("backend", "Output quota state unavailable", Effects::None))?;
            if bytes > inner.limits.retained_output_bytes.saturating_sub(*retained) {
                return Err(tool(
                    "capacity",
                    "Retained output capacity exhausted; command not started",
                    Effects::None,
                ));
            }
            *retained += bytes;
        }
        Ok(Self { inner, bytes })
    }

    fn commit(&mut self, retained_bytes: usize) -> Result<(), CapabilityError> {
        debug_assert!(
            retained_bytes <= self.bytes,
            "Captured output must fit the pre-spawn reservation"
        );
        let mut retained = self.inner.retained.lock().map_err(|_| {
            tool(
                "backend",
                "Output quota state unavailable",
                Effects::Unknown,
            )
        })?;
        *retained -= self.bytes - retained_bytes;
        self.bytes = 0;
        Ok(())
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        match self.inner.retained.lock() {
            Ok(mut retained) => *retained -= self.bytes,
            Err(error) => tracing::error!(%error, "Could not release owned output reservation"),
        }
    }
}

fn timeout_error(stdout: &[u8], stderr: &[u8], context: &CodingContext) -> CapabilityError {
    let stdout = String::from_utf8_lossy(stdout);
    let stderr = String::from_utf8_lossy(stderr);
    let stream = |text: &str, lossy: bool, length: usize| {
        let start = text.ceil_char_boundary(text.len().saturating_sub(length));
        json!({"text":&text[start..],"lossy":lossy,"truncated":start > 0})
    };
    let frame = |length| {
        let details = json!({
            "stdout":stream(&stdout, matches!(stdout, std::borrow::Cow::Owned(_)), length),
            "stderr":stream(&stderr, matches!(stderr, std::borrow::Cow::Owned(_)), length),
        });
        CapabilityError::Tool {
            code: "timeout".into(),
            message: format!(
                "Command timeout reached; effects may have occurred; process group terminated. Output captured before timeout is incomplete; earlier bytes may be omitted.\n{details}"
            ),
            effects: Effects::Partial,
        }
    };
    // Leave room for the executor's method/effects framing in bounded cell feedback.
    let budget = context.limits.output_bytes.min(1024);
    let fits = |length| cocoa_core::execution::host_value_fits(&json!(frame(length)), budget);
    if !fits(0) {
        return tool(
            "timeout",
            "Command timeout reached; effects may have occurred",
            Effects::Partial,
        );
    }
    let mut low = 0;
    let mut high = budget;
    while low < high {
        let middle = low + (high - low).div_ceil(2);
        if fits(middle) {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    frame(low)
}

async fn cleanup(
    child: &mut dyn ChildWrapper,
    process_group: Pid,
) -> Result<ExitStatus, CapabilityError> {
    terminate_and_reap(child, process_group)
        .await
        .map_err(|error| CapabilityError::Unsettled {
            message: error.to_string(),
        })
}

async fn terminate_and_reap(
    child: &mut dyn ChildWrapper,
    process_group: Pid,
) -> Result<ExitStatus, CapabilityError> {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(250);
    // WNOWAIT pins the leader's ID until this last destructive group signal.
    let group = child.start_kill();
    let root = if group.is_err() {
        child.inner_mut().start_kill()
    } else {
        Ok(())
    };
    // A group wait could reap unrelated direct children that joined this group.
    let waited = tokio::time::timeout_at(deadline, child.inner_mut().wait())
        .await
        .map_err(|_| {
            tool(
                "backend",
                "Process reaping deadline reached",
                Effects::Unknown,
            )
        })?;
    root.map_err(|error| io_error(error, Effects::Unknown))?;
    let status = waited.map_err(|error| io_error(error, Effects::Unknown))?;
    if let Err(error) = &group
        && error.raw_os_error() != Some(rustix::io::Errno::SRCH.raw_os_error())
        && !(cfg!(target_os = "macos")
            && error.raw_os_error() == Some(rustix::io::Errno::PERM.raw_os_error()))
    {
        return Err(tool(
            "backend",
            &format!("Could not terminate process group: {error}"),
            Effects::Unknown,
        ));
    }
    // Darwin skips zombies in killpg and reports EPERM for zombie-only groups.
    // Confirm disappearance with signal 0; never signal a possibly reused ID.
    loop {
        match test_kill_process_group(process_group) {
            Err(rustix::io::Errno::SRCH) => return Ok(status),
            Ok(()) => {}
            Err(rustix::io::Errno::PERM) if cfg!(target_os = "macos") => {}
            Err(error) => return Err(io_error(error.into(), Effects::Unknown)),
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(tool(
                "backend",
                &format!(
                    "Process group cleanup deadline reached; disappearance not confirmed{}",
                    group
                        .as_ref()
                        .err()
                        .map_or_else(String::new, |error| format!(": {error}"))
                ),
                Effects::Unknown,
            ));
        }
        tokio::time::sleep_until(
            deadline.min(tokio::time::Instant::now() + Duration::from_millis(5)),
        )
        .await;
    }
}

fn preview(
    bytes: &[u8],
    lines: usize,
    stdout_path: &str,
    stderr_path: &str,
    context: &CodingContext,
) -> (String, bool) {
    let decoded = String::from_utf8_lossy(bytes);
    let lossy = matches!(decoded, std::borrow::Cow::Owned(_));
    let length: usize = decoded
        .split_inclusive('\n')
        .take(lines)
        .map(str::len)
        .sum();
    let prefix = &decoded[..length];
    let mut low = 0;
    let mut high = prefix.len();
    while low < high {
        let middle = low + (high - low).div_ceil(2);
        let candidate = &prefix[..prefix.floor_char_boundary(middle)];
        if context.fits(&preview_frame(
            candidate,
            stdout_path,
            candidate,
            stderr_path,
        )) {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    (prefix[..prefix.floor_char_boundary(low)].to_owned(), lossy)
}

fn preview_frame(stdout: &str, stdout_path: &str, stderr: &str, stderr_path: &str) -> Value {
    json!({
        "status":{"kind":"signalled","signal":i32::MIN},
        "stdout":{"text":stdout,"truncated":false,"lossy":false,"output_path":stdout_path},
        "stderr":{"text":stderr,"truncated":false,"lossy":false,"output_path":stderr_path},
    })
}

fn status_value(status: ExitStatus) -> Result<Value, CapabilityError> {
    if let Some(code) = status.code() {
        return Ok(json!({"kind":"exited","code":code}));
    }
    if let Some(signal) = status.signal() {
        return Ok(json!({"kind":"signalled","signal":signal}));
    }
    Err(tool(
        "backend",
        "Shell status is neither exited nor signalled",
        Effects::Unknown,
    ))
}
