//! Observation-based coding capabilities with an injectable file/process environment.
//!
//! The generic adapter never accesses the host filesystem or starts processes.
//! A local working directory is not a sandbox. Observations detect changes but
//! do not provide atomic compare-and-swap against external writers.

#[cfg(not(unix))]
compile_error!("cocoa-tools local persistence and process ownership require Unix");

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use cocoa_core::{
    capability::{
        ActivityOutput, ActivityOutputKind, ActivitySummary, CapabilityDescription,
        CapabilityError, CapabilityHost, CapabilityInput, CapabilityObserver, Effects,
        OperationOutcome,
    },
    engine::ActivityStatus,
    execution::{WorkLimits, host_value_fits},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use similar::{DiffTag, TextDiff};
use tokio_util::sync::CancellationToken;

mod instructions;
mod local;
mod matching;
#[cfg(unix)]
mod process;
mod skills;

pub use instructions::{InstructionAcceptance, ProjectEnvironment};
pub use local::{CodingLimits, ConfigurationError, LocalConfiguration, LocalEnvironment};
pub use skills::{SkillCommand, skill_commands};

const ACTIVITY_BYTES: usize = 4096;

/// An adapter-owned agent identity. Backends must scope references to this value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AgentScope(u64);

/// Parsed method arguments. The adapter validates them before dispatch;
/// native callers must also respect the selected environment's bounds.
#[derive(Clone, Debug, Serialize)]
pub enum CodingOperation {
    /// Return complete captured lines. Line numbers start at one and each page
    /// requests 1..=200 lines; output limits may shorten it.
    Read {
        selector: ReadSelector,
        start_line: u32,
        line_count: u32,
    },
    /// Replace an observed file verbatim, or create from a missing observation.
    /// `file` is a scoped observation reference, never a path.
    Write { file: String, content: String },
    /// Apply 1..=64 disjoint replacements against one original observation.
    /// `file` is a scoped observation reference, never a path.
    Edit { file: String, edits: Vec<TextEdit> },
    /// Accept an owned command and return its scoped process receipt.
    Run(RunArguments),
    /// Wait for up to 32 issued process references without consuming results.
    /// Zero polls; absence waits at most 30 seconds, bounded by this call's deadline.
    Wait {
        handles: std::collections::BTreeMap<String, String>,
        timeout_ms: Option<u32>,
    },
    /// Request supervised cancellation. A subsequent wait confirms settlement.
    Cancel { reference: String },
    /// Queue a catalogued skill's complete instructions for model feedback.
    /// Only environments advertising skill loading accept this operation.
    LoadSkill { id: String },
}

/// One replacement within an edit operation. All anchors target the original
/// observation, must be nonempty after normalization, unique, and disjoint.
/// Replacement newlines follow the observation's first newline style, or LF
/// when it has none; other replacement characters are preserved verbatim.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TextEdit {
    pub old_text: String,
    pub new_text: String,
}

/// Fresh path observation or an existing scoped observation, never both.
#[derive(Clone, Debug, Serialize)]
pub enum ReadSelector {
    /// Capture the current file or its absence without creating directories.
    Path { path: String },
    /// Page through previously captured bytes without rereading the live file.
    File { file: String },
}

/// Shell arguments. Missing options differ from invalid explicit JSON nulls.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RunArguments {
    /// Shell source, not an executable name or an argument vector. NUL is rejected.
    pub command: String,
    /// Absent uses the environment's working directory; relative paths resolve
    /// against it. Neither choice confines the command's filesystem access.
    #[serde(default, deserialize_with = "present")]
    pub working_directory: Option<String>,
    /// Optional positive absolute kill bound in milliseconds. Without it only
    /// the environment's output-idle window limits command lifetime. Accepted
    /// commands are independent of their caller's deadline and cancellation.
    #[serde(default, deserialize_with = "present")]
    pub timeout_ms: Option<u32>,
    /// Overrides to the selected baseline, not the ambient process environment.
    /// A `None` value removes that variable; startup-hook names are rejected.
    #[serde(default, deserialize_with = "present")]
    pub environment: Option<std::collections::BTreeMap<String, Option<String>>>,
    /// UTF-8 bytes sent before closing stdin. Absent closes it without input.
    #[serde(default, deserialize_with = "present")]
    pub stdin: Option<String>,
}

/// Per-call identity, output bounds, deadline, and cooperative cancellation.
#[derive(Clone, Debug)]
pub struct CodingContext {
    pub scope: AgentScope,
    pub limits: WorkLimits,
    pub cancellation: CancellationToken,
    /// Host-issued committed operation start, absent for process-local callers.
    pub operation_record: Option<cocoa_core::store::RecordId>,
}

impl CodingContext {
    /// Check cancellation and deadline before starting or committing work.
    ///
    /// # Errors
    /// Returns invocation-stopping cancellation or deadline errors.
    pub fn check(&self) -> Result<(), CapabilityError> {
        if self.cancellation.is_cancelled() {
            return Err(CapabilityError::Cancelled);
        }
        if tokio::time::Instant::now() >= self.limits.deadline {
            return Err(CapabilityError::Deadline);
        }
        Ok(())
    }

    /// Test the actual encoded response and shared host-to-JavaScript limits.
    #[must_use]
    pub fn fits(&self, value: &Value) -> bool {
        host_value_fits(value, self.limits.output_bytes)
    }
}

/// An open environment contract. Every path, observation and output location
/// belongs to this implementation, including for nonlocal implementations.
///
/// Implementations reserve result/reference/storage capacity before effects,
/// preserve effect evidence on failures, and settle file work on cancellation.
/// Accepted commands outlive their callers and settle on completion, explicit
/// command cancellation, timeout or environment close. Returned references,
/// terminal results and output remain available until explicitly closed.
#[async_trait]
pub trait CodingEnvironment: Send + Sync {
    /// Bounded human-readable interpreter, resource limits and backend caveats.
    fn description(&self) -> String;

    /// Whether this environment supplies a skill catalog and host-owned delivery.
    /// Plain file/process backends do not advertise skill loading.
    fn supports_skill_loading(&self) -> bool {
        false
    }

    /// Best-effort display title for validated arguments. Missing target metadata
    /// must not reject or change execution. Never use opaque references as paths.
    fn activity_title(&self, operation: &CodingOperation, _scope: AgentScope) -> String {
        operation.activity_title(None)
    }

    /// Describe retained output without rereading live files. File backends may
    /// provide committed diffs; the default exposes only captured command output,
    /// grouped as stdout followed by stderr rather than temporal interleaving.
    fn activity_output(
        &self,
        operation: &CodingOperation,
        _scope: AgentScope,
        value: &Value,
    ) -> Option<ActivityOutput> {
        if matches!(
            operation,
            CodingOperation::Run(_) | CodingOperation::Wait { .. }
        ) {
            command_output(value)
        } else {
            None
        }
    }

    /// Recover a command's display status from its retained, scoped receipt.
    /// Return `Running` while pending and `Unknown` when evidence is unavailable.
    /// This must not wait for, cancel or rerun the command.
    fn command_status(&self, _reference: &str, _scope: AgentScope) -> ActivityStatus {
        ActivityStatus::Unknown
    }

    /// Observe retained command work independently of the accepting call.
    /// Backends may retain one observer per command until settlement; report
    /// captured output before the terminal status. Never wait for or rerun work.
    async fn observe_command(
        &self,
        reference: &str,
        scope: AgentScope,
        observer: CapabilityObserver,
    ) {
        observer.status(self.command_status(reference, scope));
    }

    /// Execute validated arguments in the selected environment.
    ///
    /// # Errors
    /// Use `CapabilityError::Tool` for recoverable failures, preserving effects.
    /// Caller cancellation and budgets remain fatal to that call. A wait that
    /// reaches its deadline returns pending snapshots instead of killing work.
    async fn execute(
        &self,
        operation: CodingOperation,
        context: CodingContext,
    ) -> Result<Value, CapabilityError>;
}

/// Provider-neutral catalog, validation and dispatch for coding methods.
/// Construct one adapter per agent, even when the environment is shared.
pub struct CodingCapabilities {
    environment: Arc<dyn CodingEnvironment>,
    scope: AgentScope,
}

impl CodingCapabilities {
    /// Create an isolated reference scope without taking ownership of shutdown.
    #[must_use]
    pub fn new(environment: Arc<dyn CodingEnvironment>) -> Self {
        static NEXT_SCOPE: AtomicU64 = AtomicU64::new(1);
        Self {
            environment,
            scope: AgentScope(NEXT_SCOPE.fetch_add(1, Ordering::Relaxed)),
        }
    }
}

#[async_trait]
impl CapabilityHost for CodingCapabilities {
    fn descriptions(&self) -> Vec<CapabilityDescription> {
        let environment = self.environment.description();
        Method::ALL
            .into_iter()
            .filter(|method| {
                !matches!(method, Method::LoadSkill) || self.environment.supports_skill_loading()
            })
            .map(|method| {
                method.description(if matches!(method, Method::Read) {
                    &environment
                } else {
                    ""
                })
            })
            .collect()
    }

    async fn call(
        &self,
        name: &str,
        arguments: Value,
        limits: WorkLimits,
        cancellation: CancellationToken,
    ) -> Result<Value, CapabilityError> {
        self.call_inner(name, arguments, limits, cancellation, None, None)
            .await
    }

    async fn call_observed(
        &self,
        name: &str,
        arguments: Value,
        limits: WorkLimits,
        cancellation: CancellationToken,
        observer: CapabilityObserver,
    ) -> Result<Value, CapabilityError> {
        self.call_inner(name, arguments, limits, cancellation, Some(observer), None)
            .await
    }

    async fn call_started(
        &self,
        name: &str,
        arguments: Value,
        limits: WorkLimits,
        cancellation: CancellationToken,
        observer: CapabilityObserver,
        operation_record: cocoa_core::store::RecordId,
    ) -> Result<Value, CapabilityError> {
        self.call_inner(
            name,
            arguments,
            limits,
            cancellation,
            Some(observer),
            Some(operation_record),
        )
        .await
    }

    fn describe_input(&self, name: &str, arguments: &Value) -> CapabilityInput {
        match Method::from_name(name).and_then(|method| method.parse(arguments.clone())) {
            Ok(operation) => self.activity_input(&operation),
            Err(_) => CapabilityInput {
                title: name.into(),
                text: "Input details unavailable.".into(),
                summary: None,
            },
        }
    }

    fn describe_output(
        &self,
        name: &str,
        arguments: &Value,
        outcome: &OperationOutcome,
    ) -> Option<ActivityOutput> {
        let operation = Method::from_name(name)
            .ok()?
            .parse(arguments.clone())
            .ok()?;
        match outcome {
            OperationOutcome::Completed { value } => self
                .environment
                .activity_output(&operation, self.scope, value),
            OperationOutcome::Failed {
                error: CapabilityError::Tool { code, message, .. },
            } if code == "timeout"
                && matches!(
                    operation,
                    CodingOperation::Run(_) | CodingOperation::Wait { .. }
                ) =>
            {
                let (_, details) = message.rsplit_once('\n')?;
                let value: Value = serde_json::from_str(details).ok()?;
                let mut output = command_output(&value)?;
                output.truncated = true;
                Some(output)
            }
            _ => None,
        }
    }

    fn activity_status(&self, outcome: &OperationOutcome) -> ActivityStatus {
        match outcome {
            OperationOutcome::Completed { value } if value["kind"] == "process" => {
                match value["reference"].as_str() {
                    Some(reference) => self.environment.command_status(reference, self.scope),
                    None => ActivityStatus::Unknown,
                }
            }
            OperationOutcome::Completed { value } => {
                let statuses: Vec<&Value> = if value["status"]["kind"].is_string() {
                    vec![&value["status"]]
                } else if let Some(snapshots) = value.as_object() {
                    snapshots
                        .values()
                        .take(32)
                        .filter_map(|snapshot| snapshot.get("status"))
                        .collect()
                } else {
                    Vec::new()
                };
                if statuses.is_empty() {
                    return ActivityStatus::from(outcome);
                }
                let mut activity = ActivityStatus::Completed;
                for status in statuses {
                    match (status["kind"].as_str(), status["code"].as_i64()) {
                        (Some("exited"), Some(0)) => {}
                        (Some("exited"), Some(code)) => {
                            return ActivityStatus::Failed {
                                exit_code: i32::try_from(code).ok(),
                            };
                        }
                        (Some("signalled"), _) => {
                            return match status["signal"]
                                .as_i64()
                                .and_then(|signal| i32::try_from(signal).ok())
                            {
                                Some(signal) => ActivityStatus::Signalled { signal },
                                None => ActivityStatus::Unknown,
                            };
                        }
                        (Some("timed_out"), _) => return ActivityStatus::TimedOut,
                        (Some("running"), _) if activity != ActivityStatus::Unknown => {
                            activity = ActivityStatus::Running;
                        }
                        (Some("running"), _) => {}
                        _ => activity = ActivityStatus::Unknown,
                    }
                }
                activity
            }
            OperationOutcome::Failed {
                error: CapabilityError::Tool { code, .. },
            } if code == "instructions_pending" => ActivityStatus::Deferred,
            OperationOutcome::Failed {
                error: CapabilityError::Tool { code, .. },
            } if code == "timeout" => ActivityStatus::TimedOut,
            _ => ActivityStatus::from(outcome),
        }
    }

    async fn observe_outcome(
        &self,
        name: &str,
        arguments: &Value,
        outcome: &OperationOutcome,
        observer: CapabilityObserver,
    ) {
        if let Some(output) = self.describe_output(name, arguments, outcome) {
            observer.output(output);
        }
        if name == "cocoa.run"
            && let OperationOutcome::Completed { value } = outcome
            && value["kind"] == "process"
            && let Some(reference) = value["reference"].as_str()
        {
            self.environment
                .observe_command(reference, self.scope, observer)
                .await;
        } else {
            observer.status(self.activity_status(outcome));
        }
    }
}

impl CodingCapabilities {
    fn activity_input(&self, operation: &CodingOperation) -> CapabilityInput {
        CapabilityInput {
            title: self.environment.activity_title(operation, self.scope),
            text: operation.activity_input(),
            summary: operation.activity_summary(),
        }
    }

    async fn call_inner(
        &self,
        name: &str,
        arguments: Value,
        limits: WorkLimits,
        cancellation: CancellationToken,
        observer: Option<CapabilityObserver>,
        operation_record: Option<cocoa_core::store::RecordId>,
    ) -> Result<Value, CapabilityError> {
        let context = CodingContext {
            scope: self.scope,
            limits,
            cancellation,
            operation_record,
        };
        context.check()?;
        if !host_value_fits(&arguments, limits.output_bytes) {
            return Err(tool(
                "invalid_arguments",
                "Arguments exceed conversion bounds",
                Effects::None,
            ));
        }
        let method = Method::from_name(name)?;
        if matches!(method, Method::LoadSkill) && !self.environment.supports_skill_loading() {
            return Err(invalid(
                "Skill loading is not installed in this environment",
            ));
        }
        let operation = method.parse(arguments)?;
        if let Some(observer) = observer {
            observer.input(&self.activity_input(&operation));
        }
        let value = self.environment.execute(operation, context.clone()).await?;
        if !context.fits(&value) {
            return Err(CapabilityError::OutputLimit);
        }
        Ok(value)
    }
}

impl CodingOperation {
    fn activity_summary(&self) -> Option<ActivitySummary> {
        match self {
            Self::Read {
                start_line,
                line_count,
                ..
            } => Some(ActivitySummary::LineRange {
                start: u64::from(*start_line),
                end: u64::from(*start_line) + u64::from(*line_count) - 1,
            }),
            Self::Write { content, .. } => Some(ActivitySummary::LineChanges {
                added: content.split_inclusive('\n').count() as u64,
                removed: 0,
            }),
            Self::Edit { edits, .. } => {
                let deadline = Instant::now() + Duration::from_millis(10);
                let mut added = 0;
                let mut removed = 0;
                for edit in edits {
                    let diff = TextDiff::configure()
                        .deadline(deadline)
                        .diff_lines(&edit.old_text, &edit.new_text);
                    for operation in diff.ops() {
                        if operation.tag() != DiffTag::Equal {
                            added += operation.new_range().len() as u64;
                            removed += operation.old_range().len() as u64;
                        }
                    }
                }
                Some(ActivitySummary::LineChanges { added, removed })
            }
            Self::Run(_) | Self::Wait { .. } | Self::Cancel { .. } | Self::LoadSkill { .. } => None,
        }
    }

    fn activity_input(&self) -> String {
        let mut text = match self {
            Self::Read {
                start_line,
                line_count,
                ..
            } => format!(
                "Requested lines: {start_line}-{}",
                u64::from(*start_line) + u64::from(*line_count) - 1
            ),
            Self::Write { content, .. } => format!("Bytes supplied: {}", content.len()),
            Self::Edit { edits, .. } => format!("Replacements requested: {}", edits.len()),
            Self::Wait { handles, .. } => format!("Commands requested: {}", handles.len()),
            Self::Cancel { .. } => "Request command cancellation".into(),
            Self::LoadSkill { id } => format!("Skill source: {id}"),
            Self::Run(arguments) => {
                let mut text = format!("Command:\n{}", arguments.command);
                if let Some(environment) = &arguments.environment {
                    text.push_str("\n\nEnvironment variable overrides:");
                    for (name, value) in environment {
                        text.push('\n');
                        text.push_str(if value.is_some() { "set " } else { "unset " });
                        text.push_str(name);
                    }
                }
                if let Some(stdin) = &arguments.stdin {
                    text.push_str(&format!("\n\nStdin bytes supplied: {}", stdin.len()));
                }
                text
            }
        };
        let (directory, timeout_ms) = match self {
            Self::Run(arguments) => (&arguments.working_directory, arguments.timeout_ms),
            _ => return text,
        };
        if let Some(directory) = directory {
            text.push_str(&format!("\n\nWorking directory: {directory}"));
        }
        if let Some(timeout_ms) = timeout_ms {
            text.push_str(&format!("\nTimeout: {timeout_ms} ms"));
        }
        text
    }

    fn activity_title(&self, target: Option<&str>) -> String {
        let (verb, target) = match self {
            Self::Read { selector, .. } => (
                "Read",
                target
                    .or(match selector {
                        ReadSelector::Path { path } => Some(path.as_str()),
                        ReadSelector::File { .. } => None,
                    })
                    .unwrap_or("file"),
            ),
            Self::Write { .. } => ("Write", target.unwrap_or("file")),
            Self::Edit { .. } => ("Edit", target.unwrap_or("file")),
            Self::Wait { .. } => ("Wait", "commands"),
            Self::Cancel { .. } => ("Cancel", "command"),
            Self::LoadSkill { id } => ("Load skill", id.as_str()),
            Self::Run(arguments) => (
                "Running",
                arguments
                    .command
                    .lines()
                    .map(str::trim)
                    .find(|line| !line.is_empty())
                    .unwrap_or("command"),
            ),
        };
        format!("{verb} {target}")
    }
}

fn command_output(value: &Value) -> Option<ActivityOutput> {
    let mut text = String::new();
    let mut truncated = false;
    let snapshots: Vec<&Value> = if value["stdout"]["text"].is_string() {
        vec![value]
    } else {
        value
            .as_object()?
            .values()
            .take(32)
            .filter(|snapshot| snapshot["stdout"]["text"].is_string())
            .collect()
    };
    for snapshot in snapshots {
        for stream in ["stdout", "stderr"] {
            let captured = snapshot[stream]["text"].as_str()?;
            truncated |= snapshot[stream]["truncated"].as_bool().unwrap_or(false);
            if !captured.is_empty() {
                if !text.is_empty() && !text.ends_with('\n') {
                    truncated |= append_activity(&mut text, "\n");
                }
                truncated |= append_activity(&mut text, captured);
            }
        }
    }
    if text.is_empty() && !truncated {
        return None;
    }
    Some(ActivityOutput {
        text,
        kind: ActivityOutputKind::Command,
        summary: None,
        truncated,
    })
}

fn append_activity(text: &mut String, fragment: &str) -> bool {
    let end = fragment.floor_char_boundary(fragment.len().min(ACTIVITY_BYTES - text.len()));
    text.push_str(&fragment[..end]);
    end < fragment.len()
}

#[derive(Clone, Copy)]
enum Method {
    Read,
    Write,
    Edit,
    Run,
    Wait,
    Cancel,
    LoadSkill,
}

impl Method {
    const ALL: [Self; 7] = [
        Self::Read,
        Self::Write,
        Self::Edit,
        Self::Run,
        Self::Wait,
        Self::Cancel,
        Self::LoadSkill,
    ];

    fn from_name(name: &str) -> Result<Self, CapabilityError> {
        Self::ALL
            .into_iter()
            .find(|method| method.name() == name)
            .ok_or_else(|| invalid("Unknown coding capability"))
    }

    fn name(self) -> &'static str {
        match self {
            Self::Read => "cocoa.read",
            Self::Write => "cocoa.write",
            Self::Edit => "cocoa.edit",
            Self::Run => "cocoa.run",
            Self::Wait => "cocoa.wait",
            Self::Cancel => "cocoa.cancel",
            Self::LoadSkill => "cocoa.load_skill",
        }
    }

    fn description(self, environment: &str) -> CapabilityDescription {
        let (help, properties, required) = match self {
            Self::Read => (
                "Read UTF-8 text or confirm absence. Select path for fresh bytes OR file/start_line/line_count for captured bytes. Defaults: start_line=1,line_count=200. Returns Promise<ReadResult>: {kind:'missing',file,path} or {kind:'text',file,path,bytes,line_count,page:{start_line,line_count,text},next:{file,start_line,line_count}|null}. Top-level line_count is total lines. Pages preserve complete original lines/endings, at most 200; byte bounds can shorten them. Pass next directly to read; null means EOF. Empty/past-EOF pages are empty; an oversized first line rejects. Missing parents are allowed; read creates nothing. Example: var source = await cocoa.read({path:'src/lib.rs',line_count:15}); if (source.kind === 'text') print({path:source.path,start_line:source.page.start_line,text:source.page.text.slice(0,1200),more:source.page.text.length>1200 || source.next!==null}); Shared backend policy for ALL coding methods:",
                json!({"path":{"type":"string","minLength":1},"file":{"type":"string"},"start_line":{"type":"integer","minimum":1,"maximum":u32::MAX},"line_count":{"type":"integer","minimum":1,"maximum":200}}),
                vec![],
            ),
            Self::Write => (
                "Create/replace using observation.file from a preceding read. Content is verbatim. Returns Promise<TextFile> {kind:'text',file,path,bytes,line_count}: fresh metadata, NO page/text/next. Missing observations allow no-clobber creation and automatic parent creation; existing text must be inspected and still match. Deleted/changed existing files reject as stale_read. Parent creation can survive failure; no rollback or atomic external-writer CAS. Example: destination = await cocoa.write({file:destination.file,content:`hello\n`});",
                json!({"file":{"type":"string"},"content":{"type":"string"}}),
                vec!["file", "content"],
            ),
            Self::Edit => (
                "Apply 1..64 disjoint replacements against one original text observation, then persist once. Returns Promise<TextFile> {kind:'text',file,path,bytes,line_count}: fresh metadata, NO page/text/next. Literal matching preferred; newline/NFKC/trailing-whitespace/quote/dash/space normalization still requires unique, nonempty anchors and safe complete source ranges. Leading indentation matters. Missing/ambiguous/overlapping/unsafe matches and stale reads reject. Unmatched bytes, BOM and permissions remain unchanged. Replacement newlines follow the first original style, or LF; other characters stay verbatim. No source unescaping or edit distance. Example: source = await cocoa.edit({file:source.file,edits:[{old_text:'hello',new_text:'world'}]});",
                json!({"file":{"type":"string"},"edits":{"type":"array","minItems":1,"maxItems":64,"items":{"type":"object","properties":{"old_text":{"type":"string","minLength":1},"new_text":{"type":"string"}},"required":["old_text","new_text"],"additionalProperties":false}}}),
                vec!["file", "edits"],
            ),
            Self::Run => (
                "Accept a noninteractive shell command and return Promise<ProcessHandle>, an opaque frozen handle with kind:'process'. The host wire receipt is {reference:'process.<scope>.<unique>',kind:'process'}, not a caller-constructed JavaScript handle. Each call starts one fresh shell; cwd defaults to the launch directory. Keep the handle across cells/turns and use cocoa.wait(handle,{timeout_ms}) to observe it or cocoa.cancel(handle) to stop it. Accepted work survives caller cancellation, deadlines and dropped waits. Output resets the backend's stall timer; there is no default absolute duration. Optional timeout_ms is an absolute kill bound. Local process groups, including background jobs, settle before terminal results and when the environment closes. Example: var build = await cocoa.run({command:'npm run build'}); var result = await cocoa.wait(build,{timeout_ms:30000}); Retain result and print its status plus short stdout/stderr excerpts, not the full snapshot.",
                json!({"command":{"type":"string"},"working_directory":{"type":"string","minLength":1},"timeout_ms":{"type":"integer","minimum":1,"maximum":u32::MAX},"environment":{"type":"object","additionalProperties":{"type":["string","null"]}},"stdin":{"type":"string"}}),
                vec!["command"],
            ),
            Self::Wait => (
                "Wait for process handles without consuming output or rerunning commands. Returns Promise<CommandSnapshot> for one handle or Promise<CommandSnapshots> for an alias map. JavaScript accepts a handle or alias-to-handle map plus optional {timeout_ms}; the host wire form is {handles:{alias:reference},timeout_ms?}. At most 32 handles, zero polls, default 30000ms, clamped to this call's deadline. The wire response maps aliases to snapshots with status:{kind:'running'}|{kind:'exited',code}|{kind:'signalled',signal}, stdout and stderr. CapturedOutput is {text,truncated,lossy,output_path:string|null}; truncated raw output paths remain readable until environment close. Nonzero exit is a normal result. A stalled or explicitly timed-out command raises recoverable code timeout with bounded output after confirmed process-group cleanup. Repeated waits retain the same command and terminal result. Example: var result = await cocoa.wait(build,{timeout_ms:30000}); print(result.status); Inspect retained stdout/stderr excerpts before reporting check results.",
                json!({"handles":{"type":"object","maxProperties":32,"additionalProperties":{"type":"string"}},"timeout_ms":{"type":"integer","minimum":0,"maximum":u32::MAX}}),
                vec!["handles"],
            ),
            Self::Cancel => (
                "Request supervised cancellation of an issued process handle. JavaScript accepts the opaque handle; the host wire form is {reference}. Returns Promise<CancelResult> {state:'cancelling'|'completed'|'failed'}. Cancellation does not consume the result; wait confirms terminal process-group settlement. Example: await cocoa.cancel(build); var result = await cocoa.wait(build); print(result.status);",
                json!({"reference":{"type":"string"}}),
                vec!["reference"],
            ),
            Self::LoadSkill => (
                "Load the exact id from the injected skill catalog. Returns Promise<{id,state:'pending'|'available'}>, not instruction text. New or changed complete instructions arrive in host-owned execution feedback independently of print. Finish the cell and read them before continuing skill-guided work. Writes and new commands defer while pending; reads/wait/cancel remain available. Identical already-delivered contents are not repeated. Does not execute scripts, inline references or grant capabilities. Example: await cocoa.load_skill({id:'<exact catalog id>'});",
                json!({"id":{"type":"string","minLength":1,"maxLength":4096}}),
                vec!["id"],
            ),
        };
        let mut parameters = json!({"type":"object","properties":properties,"required":required,"additionalProperties":false});
        if matches!(self, Self::Read) {
            parameters["oneOf"] = json!([
                {"required":["path"],"not":{"required":["file"]}},
                {"required":["file","start_line","line_count"],"not":{"required":["path"]}}
            ]);
        }
        CapabilityDescription {
            name: self.name().into(),
            description: format!("{help} {environment}"),
            parameters,
        }
    }

    fn parse(self, value: Value) -> Result<CodingOperation, CapabilityError> {
        #[derive(Deserialize)]
        struct Write {
            file: String,
            content: String,
        }
        #[derive(Deserialize)]
        struct Edit {
            file: String,
            edits: Vec<TextEdit>,
        }
        #[derive(Deserialize)]
        struct Wait {
            handles: std::collections::BTreeMap<String, String>,
            #[serde(default, deserialize_with = "present")]
            timeout_ms: Option<u32>,
        }
        #[derive(Deserialize)]
        struct Cancel {
            reference: String,
        }
        #[derive(Deserialize)]
        struct LoadSkill {
            id: String,
        }
        if !value.is_object() {
            return Err(invalid("arguments must be an object"));
        }
        let operation = match self {
            Self::Read => {
                let fields = value
                    .as_object()
                    .ok_or_else(|| invalid("arguments must be an object"))?;
                for key in fields.keys() {
                    if !matches!(key.as_str(), "path" | "file" | "start_line" | "line_count") {
                        return Err(invalid(&format!("unexpected argument {key:?}")));
                    }
                }
                if fields.contains_key("path") == fields.contains_key("file") {
                    return Err(invalid("supply exactly one selector: path or file"));
                }
                let continuation = fields.contains_key("file");
                let selector = if continuation {
                    ReadSelector::File {
                        file: string_argument(&value, "file")?.into(),
                    }
                } else {
                    ReadSelector::Path {
                        path: string_argument(&value, "path")?.into(),
                    }
                };
                let number = |name: &str, default: u32, maximum: u32| {
                    if !fields.contains_key(name) && !continuation {
                        return Ok(default);
                    }
                    fields
                        .get(name)
                        .and_then(Value::as_u64)
                        .filter(|number| (1..=u64::from(maximum)).contains(number))
                        .map(|number| number as u32)
                        .ok_or_else(|| {
                            invalid(&format!(
                                "{name} must be an integer from 1 through {maximum}"
                            ))
                        })
                };
                CodingOperation::Read {
                    selector,
                    start_line: number("start_line", 1, u32::MAX)?,
                    line_count: number("line_count", 200, 200)?,
                }
            }
            Self::Write => {
                string_argument(&value, "file")?;
                string_argument(&value, "content")?;
                let arguments: Write = decode(value)?;
                CodingOperation::Write {
                    file: arguments.file,
                    content: arguments.content,
                }
            }
            Self::Edit => {
                string_argument(&value, "file")?;
                if !value
                    .get("edits")
                    .and_then(Value::as_array)
                    .is_some_and(|edits| {
                        (1..=64).contains(&edits.len()) && edits.iter().all(Value::is_object)
                    })
                {
                    return Err(invalid(
                        "edits must be an array of 1 through 64 replacement objects",
                    ));
                }
                if let Some(edits) = value.get("edits").and_then(Value::as_array) {
                    for (index, edit) in edits.iter().enumerate() {
                        for name in ["old_text", "new_text"] {
                            string_argument(edit, name).map_err(|_| {
                                invalid(&format!("edits[{index}].{name} must be a string"))
                            })?;
                        }
                    }
                }
                let arguments: Edit = decode(value)?;
                CodingOperation::Edit {
                    file: arguments.file,
                    edits: arguments.edits,
                }
            }
            Self::Run => {
                string_argument(&value, "command")?;
                for name in ["working_directory", "stdin"] {
                    if value.get(name).is_some() {
                        string_argument(&value, name)?;
                    }
                }
                if let Some(timeout) = value.get("timeout_ms")
                    && !timeout
                        .as_u64()
                        .is_some_and(|timeout| (1..=u64::from(u32::MAX)).contains(&timeout))
                {
                    return Err(invalid(
                        "timeout_ms must be an integer from 1 through 4294967295",
                    ));
                }
                if let Some(environment) = value.get("environment") {
                    let variables = environment.as_object().ok_or_else(|| {
                        invalid("environment must be an object of string or null values")
                    })?;
                    for (name, value) in variables {
                        if !value.is_string() && !value.is_null() {
                            return Err(invalid(&format!(
                                "environment[{name:?}] must be a string or null"
                            )));
                        }
                    }
                }
                CodingOperation::Run(decode(value)?)
            }
            Self::Wait => {
                let arguments: Wait = decode(value)?;
                CodingOperation::Wait {
                    handles: arguments.handles,
                    timeout_ms: arguments.timeout_ms,
                }
            }
            Self::Cancel => {
                let arguments: Cancel = decode(value)?;
                CodingOperation::Cancel {
                    reference: arguments.reference,
                }
            }
            Self::LoadSkill => {
                let arguments: LoadSkill = decode(value)?;
                CodingOperation::LoadSkill { id: arguments.id }
            }
        };
        validate(&operation)?;
        Ok(operation)
    }
}

fn invalid(message: &str) -> CapabilityError {
    tool("invalid_arguments", message, Effects::None)
}

fn string_argument<'a>(value: &'a Value, name: &str) -> Result<&'a str, CapabilityError> {
    value
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(&format!("{name} must be a string")))
}

fn decode<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, CapabilityError> {
    serde_json::from_value(value).map_err(|error| invalid(&error.to_string()))
}

fn present<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(deserializer).map(Some)
}

fn validate(operation: &CodingOperation) -> Result<(), CapabilityError> {
    match operation {
        CodingOperation::Read {
            selector,
            start_line,
            line_count,
        } => {
            if let ReadSelector::Path { path } = selector
                && !valid_path(path)
            {
                return Err(invalid("path must be nonempty and contain no NUL"));
            }
            if *start_line == 0 {
                return Err(invalid("start_line must be a one-based integer"));
            }
            if !(1..=200).contains(line_count) {
                return Err(invalid("line_count must be an integer from 1 through 200"));
            }
        }
        CodingOperation::Edit { edits, .. } => {
            if !(1..=64).contains(&edits.len()) {
                return Err(invalid("edits must contain 1 through 64 replacements"));
            }
            if edits.iter().any(|edit| edit.old_text.is_empty()) {
                return Err(invalid("each old_text must be a nonempty string"));
            }
        }
        CodingOperation::Run(arguments) => {
            if arguments.command.contains('\0') {
                return Err(invalid("command must contain no NUL"));
            }
            if arguments.timeout_ms == Some(0) {
                return Err(invalid("timeout_ms must be a positive integer"));
            }
            if arguments
                .working_directory
                .as_deref()
                .is_some_and(|path| !valid_path(path))
            {
                return Err(invalid(
                    "working_directory must be nonempty and contain no NUL",
                ));
            }
            if let Some(environment) = &arguments.environment {
                for (name, value) in environment {
                    if !valid_variable(name) {
                        return Err(invalid(&format!(
                            "environment[{name:?}] must be a permitted shell variable name, not a startup hook"
                        )));
                    }
                    if value.as_ref().is_some_and(|value| value.contains('\0')) {
                        return Err(invalid(&format!(
                            "environment[{name:?}] must contain no NUL"
                        )));
                    }
                }
            }
        }
        CodingOperation::Wait { handles, .. } => {
            if handles.len() > 32 {
                return Err(invalid("handles must contain at most 32 references"));
            }
        }
        CodingOperation::LoadSkill { id } => {
            if !valid_path(id) || id.len() > 4096 {
                return Err(invalid(
                    "id must be nonempty, contain no NUL and fit 4096 bytes",
                ));
            }
        }
        CodingOperation::Write { .. } | CodingOperation::Cancel { .. } => {}
    }
    Ok(())
}

fn valid_path(path: &str) -> bool {
    !path.is_empty() && !path.contains('\0')
}

fn valid_variable(name: &str) -> bool {
    let mut characters = name.bytes();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
        && characters.all(|character| character.is_ascii_alphanumeric() || character == b'_')
        && !matches!(
            name,
            "BASH_ENV"
                | "ENV"
                | "SHELLOPTS"
                | "BASHOPTS"
                | "CDPATH"
                | "GLOBIGNORE"
                | "BASH_COMPAT"
                | "PROMPT_COMMAND"
                | "PS4"
        )
}

fn tool(code: &str, message: &str, effects: Effects) -> CapabilityError {
    CapabilityError::Tool {
        code: code.into(),
        message: message[..message.floor_char_boundary(512)].into(),
        effects,
    }
}
