//! One-shot execution through the production session, without terminal recovery.

use std::{
    collections::BTreeMap,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};

use cocoa::persistence::{AgentMembership, AgentOutcome, ExportLimits, SessionAttachment};
use cocoa_core::{
    engine::{RunError, RunRequest},
    model::AccountingSummary,
    store::AgentId,
};
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::{config, session::Session, session_data};

pub(crate) fn run(
    instruction: Vec<String>,
    selection: config::Selection,
    data_directory: Option<PathBuf>,
    export_path: Option<PathBuf>,
) -> io::Result<ExitCode> {
    let mut report = Report::default();
    match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime.block_on(lifecycle(
            &mut report,
            instruction,
            selection,
            data_directory,
            export_path,
        )),
        Err(error) => report.fail(&ExecError::Runtime(error)),
    }
    let mut output = io::stdout().lock();
    serde_json::to_writer(&mut output, &report)?;
    writeln!(output)?;
    output.flush()?;
    Ok(ExitCode::from(match report.signal {
        Some(Signal::Interrupt) => 130,
        Some(Signal::Terminate) => 143,
        None if report.outcome == Outcome::Completed && report.cleanup == Cleanup::Confirmed => 0,
        None => 1,
    }))
}

async fn lifecycle(
    report: &mut Report,
    instruction: Vec<String>,
    selection: config::Selection,
    data_directory: Option<PathBuf>,
    export_path: Option<PathBuf>,
) {
    let mut signals = match Signals::new() {
        Ok(signals) => signals,
        Err(error) => {
            report.fail(&ExecError::Signals(error));
            return;
        }
    };
    let cancellation = CancellationToken::new();
    let mut signal = None;
    let mut signal_error = None;
    let result = {
        let execution = async {
            let result = execute(
                report,
                instruction,
                selection,
                data_directory,
                export_path,
                cancellation.clone(),
            )
            .await;
            cocoa_oauth::finish_refreshes().await;
            result
        };
        tokio::pin!(execution);
        loop {
            tokio::select! {
                biased;
                received = signals.wait(), if !cancellation.is_cancelled() => {
                    match received {
                        Ok(received) => signal = Some(received),
                        Err(error) => signal_error = Some(error),
                    }
                    cancellation.cancel();
                }
                result = &mut execution => break result,
            }
        }
    };
    if let Err(error) = result {
        report.fail(&error);
    }
    report.signal = signal;
    if signal.is_some() {
        report.outcome = Outcome::Cancelled;
        eprintln!("cocoa exec: cancelled by signal");
    }
    if let Some(error) = signal_error {
        report.fail(&ExecError::Signals(error));
    }
}

async fn execute(
    report: &mut Report,
    instruction: Vec<String>,
    selection: config::Selection,
    data_directory: Option<PathBuf>,
    export_path: Option<PathBuf>,
    cancellation: CancellationToken,
) -> Result<(), ExecError> {
    let configuration = tokio::task::spawn_blocking(move || {
        config::load_selection(
            selection.alias,
            selection.model,
            selection.thinking,
            selection.choice,
        )
    })
    .await
    .map_err(|_| config::ConfigError::Worker)??;
    let input = instruction_text(
        instruction,
        configuration.engine.limits().input_bytes,
        &cancellation,
    )
    .await?;
    if cancellation.is_cancelled() {
        return Ok(());
    }
    let working_directory = std::env::current_dir().map_err(ExecError::Directory)?;
    let attachment = session_data::create(data_directory.as_deref(), &working_directory).await?;
    report.session_id = Some(attachment.public_id().to_string());
    report.root_agent_id = Some(i64::from(attachment.root_agent()).to_string());
    // Construction owns cleanup once adapters exist, so a signal must not drop it.
    let mut session = match Session::new(configuration, attachment.clone(), None).await {
        Ok(session) => session,
        Err(error) => {
            if let Err(export_error) = collect_export(report, &attachment, export_path).await {
                eprintln!("cocoa exec: {export_error}");
            }
            return Err(ExecError::Startup(error));
        }
    };
    report.cleanup = Cleanup::Unconfirmed;
    let result = session
        .engine
        .run(
            RunRequest {
                input: input.into(),
            },
            cancellation,
        )
        .await;
    let result = match result {
        Ok(output) => {
            report.invocation_id = Some(output.invocation.0);
            report.final_answer = Some(output.answer);
            report.outcome = Outcome::Completed;
            Ok(())
        }
        Err(error) => {
            eprintln!("cocoa exec: {}", crate::session::run_description(&error));
            Err(ExecError::Execution(error))
        }
    };
    // Root completion is not a join: close cancels any children still running.
    let cleanup = session.close().await;
    if cleanup.is_ok() {
        report.cleanup = Cleanup::Confirmed;
    }
    let export = collect_export(report, &attachment, export_path).await;
    if let Err(error) = &export {
        eprintln!("cocoa exec: {error}");
    }
    cleanup.map_err(ExecError::Cleanup)?;
    result?;
    export
}

async fn collect_export(
    report: &mut Report,
    attachment: &SessionAttachment,
    path: Option<PathBuf>,
) -> Result<(), ExecError> {
    let export = attachment
        .export_run(ExportLimits::default())
        .await
        .map_err(ExecError::Export)?;
    report.accounting = Some(export.accounting.clone());
    report.child_outcomes = Some(
        export
            .agents
            .iter()
            .filter(|agent| matches!(agent.membership, AgentMembership::Child { .. }))
            .map(|agent| (agent.id, agent.outcome.clone()))
            .collect(),
    );
    if let Some(path) = path {
        tokio::task::spawn_blocking(move || {
            let directory = path
                .parent()
                .filter(|directory| !directory.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            let mut file = tempfile::NamedTempFile::new_in(directory)?;
            serde_json::to_writer(&mut file, &export)?;
            writeln!(file)?;
            file.as_file().sync_all()?;
            file.persist_noclobber(&path).map_err(|error| error.error)?;
            Ok::<_, io::Error>(())
        })
        .await
        .map_err(|_| ExecError::ExportWorker)?
        .map_err(ExecError::ExportWrite)?;
        report.exported = true;
    }
    Ok(())
}

#[derive(Serialize)]
struct Report {
    schema_version: u32,
    session_id: Option<String>,
    root_agent_id: Option<String>,
    invocation_id: Option<u64>,
    final_answer: Option<String>,
    outcome: Outcome,
    cleanup: Cleanup,
    failure: Option<Failure>,
    signal: Option<Signal>,
    accounting: Option<AccountingSummary>,
    child_outcomes: Option<BTreeMap<AgentId, Option<AgentOutcome>>>,
    exported: bool,
}

impl Default for Report {
    fn default() -> Self {
        Self {
            schema_version: 1,
            session_id: None,
            root_agent_id: None,
            invocation_id: None,
            final_answer: None,
            outcome: Outcome::Failed,
            cleanup: Cleanup::NotRequired,
            failure: None,
            signal: None,
            accounting: None,
            child_outcomes: None,
            exported: false,
        }
    }
}

impl Report {
    fn fail(&mut self, error: &ExecError) {
        self.outcome = Outcome::Failed;
        self.failure = Some(match error {
            ExecError::Configuration(_) => Failure::Configuration,
            ExecError::InputRead(_)
            | ExecError::InputLimit
            | ExecError::InputEmpty
            | ExecError::InputUtf8
            | ExecError::InputTerminal => Failure::Input,
            ExecError::Execution(_) => Failure::Execution,
            ExecError::Cleanup(_) => Failure::Cleanup,
            ExecError::Export(_) | ExecError::ExportWrite(_) | ExecError::ExportWorker => {
                Failure::Export
            }
            ExecError::Signals(_) => Failure::Signals,
            _ => Failure::Startup,
        });
        if matches!(error, ExecError::Cleanup(_))
            || matches!(error, ExecError::Startup(error) if error.cleanup_unconfirmed())
        {
            self.cleanup = Cleanup::Unconfirmed;
        }
        eprintln!("cocoa exec: {error}");
    }
}

#[derive(PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Outcome {
    Completed,
    Failed,
    Cancelled,
}

#[derive(PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Cleanup {
    NotRequired,
    Confirmed,
    Unconfirmed,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum Failure {
    Configuration,
    Input,
    Startup,
    Execution,
    Cleanup,
    Export,
    Signals,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum Signal {
    #[serde(rename = "SIGINT")]
    Interrupt,
    #[serde(rename = "SIGTERM")]
    Terminate,
}

struct Signals {
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
}

impl Signals {
    fn new() -> io::Result<Self> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Ok(Self {
                interrupt: signal(SignalKind::interrupt())?,
                terminate: signal(SignalKind::terminate())?,
            })
        }
        #[cfg(not(unix))]
        Ok(Self {})
    }

    async fn wait(&mut self) -> io::Result<Signal> {
        #[cfg(unix)]
        {
            tokio::select! {
                signal = self.interrupt.recv() => signal.map(|()| Signal::Interrupt),
                signal = self.terminate.recv() => signal.map(|()| Signal::Terminate),
            }
            .ok_or_else(|| io::Error::other("signal stream closed"))
        }
        #[cfg(not(unix))]
        {
            tokio::signal::ctrl_c().await?;
            Ok(Signal::Interrupt)
        }
    }
}

async fn instruction_text(
    instruction: Vec<String>,
    limit: usize,
    cancellation: &CancellationToken,
) -> Result<String, ExecError> {
    let text = if instruction.is_empty() {
        if io::stdin().is_terminal() {
            return Err(ExecError::InputTerminal);
        }
        stdin_text(limit, cancellation).await?
    } else {
        instruction.join(" ")
    };
    if text.len() > limit {
        return Err(ExecError::InputLimit);
    }
    if !cancellation.is_cancelled() && text.trim().is_empty() {
        return Err(ExecError::InputEmpty);
    }
    Ok(text)
}

#[cfg(unix)]
async fn stdin_text(limit: usize, cancellation: &CancellationToken) -> Result<String, ExecError> {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};

    let stdin = io::stdin();
    let mut bytes = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        if cancellation.is_cancelled() {
            return Ok(String::new());
        }
        let mut descriptors = [PollFd::new(&stdin, PollFlags::IN)];
        match poll(&mut descriptors, Some(&Timespec::default())) {
            Ok(0) => {}
            Ok(_) => match rustix::io::read(&stdin, &mut buffer) {
                Ok(0) => break,
                Ok(length) => {
                    if bytes.len() + length > limit {
                        return Err(ExecError::InputLimit);
                    }
                    bytes.extend_from_slice(&buffer[..length]);
                    tokio::task::yield_now().await;
                    continue;
                }
                Err(rustix::io::Errno::INTR | rustix::io::Errno::AGAIN) => {}
                Err(error) => return Err(ExecError::InputRead(error.into())),
            },
            Err(rustix::io::Errno::INTR) => {}
            Err(error) => return Err(ExecError::InputRead(error.into())),
        }
        tokio::select! {
            () = cancellation.cancelled() => return Ok(String::new()),
            () = tokio::time::sleep(Duration::from_millis(10)) => {}
        }
    }
    String::from_utf8(bytes).map_err(|_| ExecError::InputUtf8)
}

#[cfg(not(unix))]
async fn stdin_text(_: usize, _: &CancellationToken) -> Result<String, ExecError> {
    Err(ExecError::InputRead(io::Error::other(
        "stdin requires macOS or Linux",
    )))
}

#[derive(Debug, thiserror::Error)]
enum ExecError {
    #[error("cannot initialize runtime")]
    Runtime(#[source] io::Error),
    #[error("cannot listen for cancellation signals")]
    Signals(#[source] io::Error),
    #[error(transparent)]
    Configuration(#[from] config::ConfigError),
    #[error("cannot read instruction from stdin")]
    InputRead(#[source] io::Error),
    #[error("instruction exceeds the configured input byte limit")]
    InputLimit,
    #[error("instruction must not be blank")]
    InputEmpty,
    #[error("stdin instruction must be UTF-8")]
    InputUtf8,
    #[error("provide an instruction argument or pipe stdin to cocoa exec")]
    InputTerminal,
    #[error("cannot determine working directory")]
    Directory(#[source] io::Error),
    #[error(transparent)]
    Data(#[from] session_data::SessionDataError),
    #[error("session initialization failed: {0}")]
    Startup(#[source] crate::session::CommandError),
    #[error("execution failed")]
    Execution(#[source] RunError),
    #[error("cleanup is unconfirmed: {0}")]
    Cleanup(#[source] crate::session::CommandError),
    #[error("cannot collect persisted run export: {0}")]
    Export(#[source] cocoa::persistence::PersistenceError),
    #[error("cannot write run export; destination must be a new file in an existing directory")]
    ExportWrite(#[source] io::Error),
    #[error("run export worker failed")]
    ExportWorker,
}
