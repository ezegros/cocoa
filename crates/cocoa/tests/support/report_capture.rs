//! Test-owned evidence capture. Nothing here is installed in the executable.

use std::{
    collections::{BTreeMap, VecDeque},
    error::Error,
    fs::{self, OpenOptions},
    io::{self, Read},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use cocoa::agents::{AgentFactory, AgentParticipant};
use cocoa_core::{
    capability::{CapabilityError, CapabilityHost, Effects, OperationOutcome},
    context::BoundedContext,
    engine::{Engine, EngineAdapters, EngineSettings, Limits, RunError, RunOutput, RunRequest},
    execution::CellResponse,
    model::{ModelAction, ModelProvider},
    store::{InMemoryStore, Record, RecordKind, Store, StoreLimits},
};
use cocoa_quickjs::{FeedbackLimits, QuickJsExecutor, QuickJsOptions};
use cocoa_tools::{
    AgentScope, CodingCapabilities, CodingContext, CodingEnvironment, CodingLimits,
    CodingOperation, LocalConfiguration, LocalEnvironment, RunArguments,
};
use serde::{
    Deserialize, Deserializer, Serialize,
    de::{MapAccess, Visitor},
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

const FILE_BYTES_MAX: usize = 512 * 1024;
const CAPTURES_MAX: usize = 32;

pub struct CommandSpec {
    pub participant: String,
    pub arguments: RunArguments,
    pub junit: PathBuf,
    pub runner: Value,
}

pub struct Capture {
    _directory: TempDir,
    directory: PathBuf,
    models: BTreeMap<String, Arc<dyn ModelProvider>>,
    commands: BTreeMap<String, CommandSpec>,
    participants: Mutex<BTreeMap<String, Participant>>,
    sources: BTreeMap<String, String>,
    root_outcome: Mutex<Option<Value>>,
}

impl Capture {
    pub fn new(
        directory: TempDir,
        models: BTreeMap<String, Arc<dyn ModelProvider>>,
        commands: BTreeMap<String, CommandSpec>,
        sources: &[&str],
    ) -> Result<Self, Box<dyn Error>> {
        if models.len() > 2 || commands.len() > CAPTURES_MAX || sources.len() > 32 {
            return Err("capture manifest exceeds limits".into());
        }
        let base = directory.path().canonicalize()?;
        let mut identities = std::collections::BTreeSet::new();
        let mut artifacts = std::collections::BTreeSet::new();
        for spec in commands.values() {
            if !identities.insert((&spec.participant, &spec.arguments.command))
                || !artifacts.insert(&spec.junit)
                || !spec.junit.is_absolute()
                || !spec.junit.starts_with(&base)
            {
                return Err(
                    "command capture must have a unique name and fresh local artifact".into(),
                );
            }
        }
        let mut snapshots = BTreeMap::new();
        let mut bytes = 0;
        for path in sources {
            let text = snapshot(&relative(&base, path)?)?;
            bytes += text.len();
            if bytes > 4 * 1024 * 1024 {
                return Err("source capture exceeds byte limit".into());
            }
            snapshots.insert((*path).into(), text);
        }
        Ok(Self {
            _directory: directory,
            directory: base,
            models,
            commands,
            participants: Mutex::new(BTreeMap::new()),
            sources: snapshots,
            root_outcome: Mutex::new(None),
        })
    }

    pub async fn coding(&self, name: &str) -> Result<Arc<dyn CapabilityHost>, Box<dyn Error>> {
        let local = Arc::new(
            LocalEnvironment::new(
                LocalConfiguration {
                    working_directory: self.directory.clone(),
                    output_directory: self.directory.clone(),
                    shell: "/bin/bash".into(),
                    environment: BTreeMap::from([
                        ("PATH".into(), std::env::var("PATH")?),
                        ("HOME".into(), std::env::var("HOME")?),
                    ]),
                },
                CodingLimits {
                    file_bytes: FILE_BYTES_MAX,
                    observed_bytes: 2 * FILE_BYTES_MAX,
                    observations: 64,
                    processes: 2,
                    captured_output_bytes: FILE_BYTES_MAX,
                    retained_output_bytes: 2 * FILE_BYTES_MAX,
                    preview_lines: 1,
                    default_timeout: Duration::from_secs(30),
                },
            )
            .await?,
        );
        let environment = Arc::new(CapturedEnvironment {
            local,
            commands: self
                .commands
                .values()
                .filter(|spec| spec.participant == name)
                .map(|spec| (spec.arguments.command.clone(), spec.junit.clone()))
                .collect(),
            state: Mutex::new(CaptureState::default()),
        });
        let mut participants = self.participants.lock().await;
        if participants.contains_key(name) || participants.len() >= 3 {
            return Err("participant already registered or capacity exhausted".into());
        }
        participants.insert(
            name.into(),
            Participant {
                store: Arc::new(InMemoryStore::new(StoreLimits {
                    record_bytes: 65536,
                })?),
                environment: environment.clone(),
            },
        );
        Ok(Arc::new(CodingCapabilities::new(environment)))
    }

    pub async fn engine(
        &self,
        name: &str,
        model: Arc<dyn ModelProvider>,
        host: Arc<dyn CapabilityHost>,
        limits: Limits,
    ) -> Result<Engine, Box<dyn Error>> {
        let store = self
            .participants
            .lock()
            .await
            .get(name)
            .ok_or("unregistered participant")?
            .store
            .clone();
        Ok(Engine::new(
            EngineSettings::new(limits)?,
            EngineAdapters {
                model,
                executor: Box::new(
                    QuickJsExecutor::new(QuickJsOptions {
                        memory_bytes: 4 * 1024 * 1024,
                        stack_bytes: 256 * 1024,
                        feedback: FeedbackLimits {
                            output_bytes: 4096,
                            printed_bytes: 3072,
                            error_bytes: 512,
                            print_calls: 16,
                        },
                    })
                    .await?,
                ),
                capabilities: host,
                context: Arc::new(BoundedContext),
                store,
            },
            None,
        ))
    }

    pub async fn run_root(
        &self,
        engine: &mut Engine,
        request: RunRequest,
        cancellation: CancellationToken,
    ) -> Result<RunOutput, RunError> {
        *self.root_outcome.lock().await = None;
        let result = engine.run(request, cancellation).await;
        *self.root_outcome.lock().await = match &result {
            Ok(output) => Some(json!({"name":"root","status":"completed","answer":output.answer})),
            Err(RunError::Cancelled | RunError::TurnCancelled { .. }) => {
                Some(json!({"name":"root","status":"cancelled"}))
            }
            Err(_) => None,
        };
        result
    }

    pub async fn packet(
        &self,
        run_id: &str,
        saved: &str,
        notes: &[&str],
    ) -> Result<Value, Box<dyn Error>> {
        if notes.len() > 32 {
            return Err("too many note snapshots".into());
        }
        let mut packet = json!({"schema_version":1,"run_id":run_id,"participants":{},"sources":{},"test_runs":{},"notes":{}});
        for (path, text) in &self.sources {
            packet["sources"][path] = json!({"text":text});
        }
        for path in notes {
            if let Ok(text) = snapshot(&relative(&self.directory, path)?) {
                packet["notes"][*path] = json!({"text":text});
            }
        }
        if let Ok(text) = snapshot(&relative(&self.directory, saved)?)
            && let Ok(value) = serde_json::from_str::<SavedOutcomes>(&text)
        {
            packet["saved_outcomes"] = json!(value.0);
        }
        for (name, participant) in self.participants.lock().await.iter() {
            let records = participant.store.load().await?.into_records();
            packet["participants"][name] = participant_evidence(name, &records);
            if name == "root" {
                if let Some(outcome) = self.root_outcome.lock().await.as_ref() {
                    packet["participants"][name]["outcome"] = outcome.clone();
                } else if let Some(participant) = packet["participants"][name].as_object_mut() {
                    participant.remove("outcome");
                    participant.insert("complete".into(), json!(false));
                }
                let waits: Vec<_> = records
                    .iter()
                    .filter(|record| {
                        matches!(&record.kind,
                            RecordKind::OperationStarted { name, .. } if name == "cocoa.wait"
                        )
                    })
                    .collect();
                if let [first, second] = waits.as_slice() {
                    for (key, start) in [("wait_results", *first), ("repeat_results", *second)] {
                        if let Some(OperationOutcome::Completed { value }) =
                            operation_result(&records, start)
                        {
                            packet[key] = value.clone();
                        }
                    }
                }
            }
            let captures = participant.environment.state.lock().await;
            for (run, spec) in self
                .commands
                .iter()
                .filter(|(_, spec)| spec.participant == *name)
            {
                packet["test_runs"][run] = command_evidence(spec, &records, &captures);
            }
        }
        if serde_json::to_vec(&packet)?.len() > 4 * 1024 * 1024 {
            return Err("evidence packet exceeds byte limit".into());
        }
        Ok(packet)
    }
}

#[async_trait]
impl AgentFactory for Capture {
    fn supports_alias(&self, alias: &str) -> bool {
        alias == "medium"
    }

    async fn create(
        &self,
        participant: AgentParticipant,
        _alias: &str,
        limits: Limits,
        _cancellation: CancellationToken,
    ) -> Result<Engine, CapabilityError> {
        let name = participant.name();
        let model = self.models.get(name).ok_or_else(capture_error)?.clone();
        let host = participant.bind(self.coding(name).await.map_err(|_| capture_error())?);
        self.engine(name, model, host, limits)
            .await
            .map_err(|_| capture_error())
    }

    async fn close(&self, name: &str) -> Result<(), CapabilityError> {
        if let Some(participant) = self.participants.lock().await.get(name) {
            participant.environment.local.close().await?;
        }
        Ok(())
    }
}

struct Participant {
    store: Arc<InMemoryStore>,
    environment: Arc<CapturedEnvironment>,
}

struct CapturedEnvironment {
    local: Arc<LocalEnvironment>,
    commands: BTreeMap<String, PathBuf>,
    state: Mutex<CaptureState>,
}

#[derive(Default)]
struct CaptureState {
    sequence: u64,
    overflow: bool,
    commands: Vec<CommandCapture>,
    bytes: usize,
}

struct CommandCapture {
    sequence: u64,
    arguments: Value,
    receipt: Result<Value, CapabilityError>,
    fresh: bool,
    result: Result<Value, CapabilityError>,
    stdout: Option<String>,
    stderr: Option<String>,
    junit: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SavedOutcome {
    name: String,
    status: String,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    answer: Option<String>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    error: Option<SavedError>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    state: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SavedError {
    code: String,
    message: String,
}

struct SavedOutcomes(BTreeMap<String, SavedOutcome>);

impl<'de> Deserialize<'de> for SavedOutcomes {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct OutcomesVisitor;
        impl<'de> Visitor<'de> for OutcomesVisitor {
            type Value = SavedOutcomes;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("at most 32 distinct outcome aliases")
            }

            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut outcomes = BTreeMap::new();
                while let Some((alias, outcome)) = map.next_entry::<String, SavedOutcome>()? {
                    if alias.chars().count() > 128
                        || outcomes.len() >= 32
                        || outcomes.insert(alias, outcome).is_some()
                    {
                        return Err(serde::de::Error::custom(
                            "ambiguous or oversized outcome map",
                        ));
                    }
                }
                Ok(SavedOutcomes(outcomes))
            }
        }
        deserializer.deserialize_map(OutcomesVisitor)
    }
}

#[async_trait]
impl CodingEnvironment for CapturedEnvironment {
    fn description(&self) -> String {
        self.local.description()
    }

    fn activity_title(&self, operation: &CodingOperation, scope: AgentScope) -> String {
        self.local.activity_title(operation, scope)
    }

    fn activity_output(
        &self,
        operation: &CodingOperation,
        scope: AgentScope,
        value: &Value,
    ) -> Option<cocoa_core::capability::ActivityOutput> {
        self.local.activity_output(operation, scope, value)
    }

    async fn execute(
        &self,
        operation: CodingOperation,
        context: CodingContext,
    ) -> Result<Value, CapabilityError> {
        let designated = match &operation {
            CodingOperation::Run(arguments) => self
                .commands
                .get(&arguments.command)
                .map(|path| (json!(arguments), path.clone())),
            _ => None,
        };
        let sequence = if designated.is_some() {
            let mut state = self.state.lock().await;
            if state.sequence < CAPTURES_MAX as u64 {
                state.sequence += 1;
                Some(state.sequence)
            } else {
                state.overflow = true;
                None
            }
        } else {
            None
        };
        let fresh = designated.as_ref().is_some_and(|(_, path)| {
            matches!(fs::symlink_metadata(path), Err(error) if error.kind() == io::ErrorKind::NotFound)
        });
        let handles = match &operation {
            CodingOperation::Wait { handles, .. } => Some(handles.clone()),
            _ => None,
        };
        let result = self.local.execute(operation, context).await;
        if let (Some((arguments, path)), Some(sequence)) = (designated, sequence) {
            let captured = CommandCapture {
                sequence,
                arguments,
                receipt: result.clone(),
                fresh,
                stdout: result
                    .as_ref()
                    .ok()
                    .and_then(|value| stream(&value["stdout"])),
                stderr: result
                    .as_ref()
                    .ok()
                    .and_then(|value| stream(&value["stderr"])),
                junit: if fresh { snapshot(&path).ok() } else { None },
                result: result.clone(),
            };
            let mut state = self.state.lock().await;
            let bytes = captured.stdout.as_ref().map_or(0, String::len)
                + captured.stderr.as_ref().map_or(0, String::len)
                + captured.junit.as_ref().map_or(0, String::len)
                + captured.arguments.to_string().len()
                + json!(captured.result).to_string().len();
            if state.commands.len() < CAPTURES_MAX
                && bytes <= (4 * 1024 * 1024_usize).saturating_sub(state.bytes)
            {
                state.bytes += bytes;
                state.commands.push(captured);
            } else {
                state.overflow = true;
            }
        }
        if let (Some(handles), Ok(outcomes)) = (handles, &result) {
            let mut state = self.state.lock().await;
            let mut bytes = 0;
            for capture in &mut state.commands {
                let Some(reference) = capture
                    .receipt
                    .as_ref()
                    .ok()
                    .and_then(|receipt| receipt["reference"].as_str())
                else {
                    continue;
                };
                let Some((alias, _)) = handles
                    .iter()
                    .find(|(_, selected)| selected.as_str() == reference)
                else {
                    continue;
                };
                let value = &outcomes[alias];
                if !matches!(
                    value["status"]["kind"].as_str(),
                    Some("exited" | "signalled")
                ) || capture.stdout.is_some()
                {
                    continue;
                }
                capture.stdout = stream(&value["stdout"]);
                capture.stderr = stream(&value["stderr"]);
                capture.junit = if capture.fresh {
                    capture.arguments["command"]
                        .as_str()
                        .and_then(|command| self.commands.get(command))
                        .and_then(|path| snapshot(path).ok())
                } else {
                    None
                };
                capture.result = Ok(value.clone());
                bytes += capture.stdout.as_ref().map_or(0, String::len)
                    + capture.stderr.as_ref().map_or(0, String::len)
                    + capture.junit.as_ref().map_or(0, String::len)
                    + value.to_string().len();
            }
            state.bytes += bytes;
            if state.bytes > 4 * 1024 * 1024 {
                state.overflow = true;
            }
        }
        result
    }
}

fn participant_evidence(name: &str, records: &[Record]) -> Value {
    let mut calls = Vec::new();
    let mut complete = true;
    let mut pending = VecDeque::new();
    let mut ordinals = BTreeMap::new();
    let mut outcome = None;
    for record in records {
        match &record.kind {
            RecordKind::InputAccepted { .. } => outcome = None,
            RecordKind::ModelCompleted { response } => match &response.action {
                ModelAction::Execute { cells } => {
                    let ordinal = ordinals.entry(record.invocation.0).or_insert(0_u64);
                    for cell in cells {
                        *ordinal += 1;
                        let index = calls.len();
                        calls.push(json!({
                            "invocation":record.invocation.0,"call_id":format!("cell-{ordinal}"),
                            "code":cell.code,"state":"selected"
                        }));
                        pending.push_back((record.invocation, index));
                    }
                }
                ModelAction::Final { answer } => {
                    outcome = Some(json!({"name":name,"status":"completed","answer":answer}));
                }
            },
            RecordKind::CellCompleted { code, outcome, .. } => match pending.pop_front() {
                Some((invocation, index))
                    if invocation == record.invocation && calls[index]["code"] == *code =>
                {
                    calls[index]["state"] = json!(match outcome {
                        CellResponse::Completed { .. } => "completed",
                        CellResponse::Failed { .. } => "failed",
                        CellResponse::Interrupted { .. } => "interrupted",
                        CellResponse::NotStarted => "not_started",
                    });
                }
                _ => complete = false,
            },
            RecordKind::TurnCancelled { .. } => {
                outcome = Some(json!({"name":name,"status":"cancelled"}));
            }
            _ => {}
        }
    }
    let mut evidence =
        json!({"complete":complete && pending.is_empty() && outcome.is_some(),"calls":calls});
    if let Some(outcome) = outcome {
        evidence["outcome"] = outcome;
    }
    evidence
}

fn command_evidence(spec: &CommandSpec, records: &[Record], captures: &CaptureState) -> Value {
    let mut evidence = json!({"participant":spec.participant,"command":spec.arguments.command,
        "complete":false,"runner":spec.runner});
    let starts: Vec<_> = records
        .iter()
        .filter(|record| {
            matches!(&record.kind,
                RecordKind::OperationStarted {name, arguments, ..}
                if name == "cocoa.run" && arguments["command"] == spec.arguments.command
            )
        })
        .collect();
    let snapshots: Vec<_> = captures
        .commands
        .iter()
        .filter(|capture| capture.arguments["command"] == spec.arguments.command)
        .collect();
    let ([start], [capture]) = (starts.as_slice(), snapshots.as_slice()) else {
        return evidence;
    };
    let RecordKind::OperationStarted { arguments, .. } = &start.kind else {
        return evidence;
    };
    let Ok(arguments) = serde_json::from_value::<RunArguments>(arguments.clone()) else {
        return evidence;
    };
    if json!(arguments) != capture.arguments || json!(spec.arguments) != capture.arguments {
        return evidence;
    }
    let (Some(OperationOutcome::Completed { value: receipt }), Ok(captured_receipt), Ok(captured)) = (
        operation_result(records, start),
        &capture.receipt,
        &capture.result,
    ) else {
        return evidence;
    };
    if receipt != captured_receipt {
        return evidence;
    }
    let Some(reference) = receipt["reference"].as_str() else {
        return evidence;
    };
    let result = records.iter().find_map(|record| {
        let RecordKind::OperationStarted {
            name, arguments, ..
        } = &record.kind
        else {
            return None;
        };
        if name != "cocoa.wait" {
            return None;
        }
        let alias = arguments["handles"]
            .as_object()?
            .iter()
            .find(|(_, selected)| selected.as_str() == Some(reference))?
            .0;
        let OperationOutcome::Completed { value } = operation_result(records, record)? else {
            return None;
        };
        let selected = value.get(alias)?;
        (selected == captured).then_some(selected)
    });
    let Some(value) = result else {
        return evidence;
    };
    evidence["status"] = value["status"].clone();
    for (key, text) in [
        ("stdout", &capture.stdout),
        ("stderr", &capture.stderr),
        ("junit", &capture.junit),
    ] {
        if let Some(text) = text {
            evidence[key] = json!(text);
        }
    }
    evidence["complete"] = json!(
        !captures.overflow
            && captures.sequence == captures.commands.len() as u64
            && capture.sequence > 0
            && capture.stdout.is_some()
            && capture.stderr.is_some()
            && capture.junit.is_some()
    );
    evidence
}

fn operation_result<'a>(records: &'a [Record], start: &Record) -> Option<&'a OperationOutcome> {
    let RecordKind::OperationStarted { operation, .. } = start.kind else {
        return None;
    };
    let mut found = None;
    for record in records {
        if record.invocation == start.invocation
            && let RecordKind::OperationFinished {
                operation: identity,
                outcome,
            } = &record.kind
            && *identity == operation
        {
            if found.is_some() {
                return None;
            }
            found = Some(outcome);
        }
    }
    found
}

fn relative(base: &Path, path: &str) -> io::Result<PathBuf> {
    if path.is_empty()
        || path.contains(['\\', ':', '\0'])
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(io::Error::other("invalid capture path"));
    }
    Ok(base.join(path))
}

fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(deserializer).map(Some)
}

fn stream(value: &Value) -> Option<String> {
    if value["lossy"] != false {
        return None;
    }
    if value["truncated"] == false {
        return value["text"].as_str().map(str::to_owned);
    }
    snapshot(Path::new(value["output_path"].as_str()?)).ok()
}

fn snapshot(path: &Path) -> io::Result<String> {
    let mut ancestor = PathBuf::new();
    for component in path.components() {
        if matches!(component, std::path::Component::ParentDir) {
            return Err(io::Error::other("parent traversal is unsupported"));
        }
        ancestor.push(component);
        if fs::symlink_metadata(&ancestor)?.file_type().is_symlink() {
            return Err(io::Error::other("symlink snapshot is unsupported"));
        }
    }
    let metadata = fs::symlink_metadata(path)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(path)?;
    let opened = file.metadata()?;
    if !opened.is_file()
        || opened.len() > FILE_BYTES_MAX as u64
        || metadata.dev() != opened.dev()
        || metadata.ino() != opened.ino()
    {
        return Err(io::Error::other("unsupported snapshot"));
    }
    let mut bytes = Vec::new();
    file.take(FILE_BYTES_MAX as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > FILE_BYTES_MAX {
        return Err(io::Error::other("snapshot exceeds byte limit"));
    }
    String::from_utf8(bytes).map_err(io::Error::other)
}

fn capture_error() -> CapabilityError {
    CapabilityError::Backend {
        message: "cannot assemble capture fixture".into(),
        effects: Effects::None,
    }
}
