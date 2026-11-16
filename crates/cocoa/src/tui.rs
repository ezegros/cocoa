//! Fixed-rate terminal input and rendering without a detached input worker.

use std::{
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Mutex, Once, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use crate::{
    auth, catalogue, clipboard, config,
    dialog::{self, Dialog, RewindPicker, SessionPicker},
    session::Session,
    session_data::{self, SessionDataError},
    status::{Kind, Status},
};
use cocoa::{
    agents::AgentUpdates,
    chat::{Action, Chat, ImagePath},
    persistence::{PersistenceError, SessionAttachment, SessionDatabase, StoredSessionSettings},
};
use cocoa_core::engine::{Engine, EventKind, EventReceiver, Observation, RunError, RunRequest};
use cocoa_core::model::ModelError;
use cocoa_llm::ThinkingLevel;
#[cfg(unix)]
use crossterm::event::{
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::{
    clipboard::CopyToClipboard,
    cursor::Show,
    event::{
        DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
        EnableFocusChange, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{DefaultTerminal, Terminal, backend::CrosstermBackend, widgets::Clear};
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::time::{Instant, Interval, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

const ALIAS_LOCKED_NOTICE: &str =
    "Alias and model are fixed for this session. Start a new session to change them.";
const REWIND_NOTICE: &str = "Rewound into a new session before the selected prompt. Files are unchanged. JavaScript bindings and handles are reset. Shared context and child agents are not copied. The original session is preserved.";
const RESUME_NOTICE: &str = "Session resumed. History and shared context survived. Earlier JavaScript bindings, observations, handles and process supervision are unavailable. External effects may remain. Read current state again before mutations.";

#[derive(Debug, thiserror::Error)]
#[error("{reason}{}{}{}", if .shutdown.is_some() { "; shutdown failed" } else { "" }, if .restore.is_some() { "; terminal restoration failed" } else { "" }, if .display.is_some() { "; stopped view failed" } else { "" })]
pub(crate) struct TuiError {
    #[source]
    reason: SessionError,
    shutdown: Option<Box<crate::session::CommandError>>,
    restore: Option<io::Error>,
    display: Option<SessionError>,
}

#[derive(Debug, thiserror::Error)]
enum SessionError {
    #[error("terminal input or rendering failed")]
    Terminal(#[from] io::Error),
    #[error(transparent)]
    Persistence(Box<SessionDataError>),
    #[error("cannot close failed session")]
    Recovery(#[source] Box<crate::session::CommandError>),
    #[error("exit cleanup failed: {}", crate::run_description(.0))]
    ExitSettlement(Box<RunError>),
    #[error("terminal failed during invocation{}", if .settlement.is_some() { "; invocation settlement also failed" } else { "" })]
    ActiveTerminal {
        source: io::Error,
        settlement: Option<Box<RunError>>,
    },
    #[error("invalid composer limits")]
    View(#[from] cocoa::chat::ChatError),
    #[error("session cleanup failed")]
    Cleanup,
    #[error(transparent)]
    Switch(#[from] Box<SwitchError>),
}

pub(crate) async fn run(
    prompt: Option<String>,
    selection: config::Selection,
    open_menu: Option<&str>,
    data_directory: Option<PathBuf>,
    resume: Option<config::ResumeCommand>,
    presentation: config::Presentation,
) -> Result<(), TuiError> {
    let mut modes = TerminalModes {
        armed: false,
        #[cfg(unix)]
        keyboard: false,
    };
    let mut terminal = None;
    let mut owned = None;
    let mut attachment = None;
    let mut commands = Commands::default();
    let mut chat = Chat::new(65_536);
    let mut interrupt = match signal(SignalKind::interrupt()) {
        Ok(interrupt) => interrupt,
        Err(error) => {
            return Err(TuiError {
                reason: SessionError::Terminal(error),
                shutdown: None,
                restore: None,
                display: None,
            });
        }
    };
    let result = async {
        let mut directory = std::env::current_dir()?.canonicalize()?;
        let mut restored = None;
        if let Some(resume) = &resume {
            let database = SessionDatabase::open_existing(&session_data::database_path(data_directory.as_deref())?).await.map_err(SessionDataError::Persistence)?;
            if resume.reference.is_some() || resume.last {
                let database = database.ok_or_else(|| Box::new(SwitchError::Persistence(PersistenceError::SessionNotFound)))?;
                let reference = resume_reference(&database, resume, &directory).await.map_err(Box::new)?;
                let target = prepare_resume(&database, &reference, resume.cwd.as_deref()).await.map_err(Box::new)?;
                directory = target.directory.clone();
                restored = Some(target);
            } else {
                commands.picker = Some(SessionPicker::new(database, directory.clone(), String::new()));
                commands.picker_cwd = resume.cwd.clone();
            }
        }
        let configuration = if resume.is_some() { Err(config::ConfigError::AliasRequired) } else { resolve_selection(selection.clone()).await };
        let selected = configuration.as_ref().map(|configuration| configuration.alias.clone()).unwrap_or_else(|_| selection.model.clone().or_else(|| selection.alias.clone()).unwrap_or_else(|| "unconfigured".into()));
        let model_label = configuration
            .as_ref()
            .ok()
            .map(configuration_label)
            .unwrap_or_else(|| selected.clone());
        let mut thinking_level = configuration
            .as_ref()
            .ok()
            .and_then(|configuration| configuration.thinking_level);
        let setup_error = match configuration {
            Ok(configuration) => {
                chat = Chat::new(configuration.engine.limits().input_bytes);
                if let Ok(chat) = &mut chat {
                    chat.set_message_steering(configuration.message_steering);
                }
                None
            }
            Err(error) => Some(error.to_string()),
        };
        let chat = chat.as_mut().map_err(|_| cocoa::chat::ChatError)?;
        chat.set_theme(presentation.theme);
        chat.set_reduced_motion(presentation.reduced_motion);
        chat.set_working_directory(&directory);
        commands.working_directory = directory.clone();
        match cocoa_tools::skill_commands(&directory, std::env::home_dir().as_deref()) {
            Ok(skills) => chat.set_skills(skills),
            Err(error) => chat.notice(&format!("Cannot discover skill commands: {error}")),
        }
        let label = match std::env::home_dir() {
            Some(home) => match directory.strip_prefix(home) {
                Ok(relative) => format!("~/{}", relative.display()),
                Err(_) => directory.display().to_string(),
            },
            None => directory.display().to_string(),
        };
        show_selection(chat, &label, &model_label, thinking_level);
        let onboarding = match if resume.is_some() { Ok(std::collections::BTreeMap::new()) } else { config::aliases() } {
            Ok(aliases) => {
                chat.set_aliases(&config::alias_names(&aliases));
                aliases.is_empty() && selection.model.is_none() && open_menu.is_none() && resume.is_none()
            }
            Err(error) => {
                chat.input_notice(&error.to_string());
                false
            }
        };
        if onboarding {
            commands.dialog = Some(Dialog::onboarding(selection.alias.clone()));
        } else if resume.is_none() && let Some(error) = setup_error {
            chat.notice(&error);
            chat.notice("Configure an alias with /aliases and save a provider key with /login. No model is running.");
        }
        if let Some(target) = restored {
            thinking_level = target.thinking_level;
            chat.restore_transcript(&target.records);
            show_selection(chat, &label, &target.model_label, thinking_level);
            if let Some(attachment) = &target.attachment { chat.notice(&format!("Session {}", attachment.public_id())); }
            chat.notice(RESUME_NOTICE);
            if target.relocated { chat.notice(&format!("Session relocated to {}", target.directory.display())); }
            if let Some(active) = target.session.as_ref() {
                chat.set_input_bytes(active.input_bytes)?;
                chat.set_message_steering(active.message_steering);
                commands.mcp = Some(active.mcp.clone());
            }
            attachment = target.attachment;
            owned = target.session;
        }
        if let Some(menu) = open_menu { commands.open(menu, chat); }
        modes.enter()?;
        let mut active_terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        #[cfg(unix)]
        modes.enhance_keyboard()?;
        commands.report_status()?;
        let result = session(
            &mut owned,
            Startup {
                selection,
                directory_label: &label,
                prompt,
                data_directory: data_directory.as_deref(),
                working_directory: &directory,
                attachment: &mut attachment,
            },
            &mut commands,
            &mut active_terminal,
            chat,
            &mut interrupt,
        )
        .await;
        terminal = Some(active_terminal);
        result
    }
    .await;
    if result.is_err() {
        commands.status = Status::Error;
    }
    let mut display = None;
    let shutdown = {
        let shutdown = async {
            match owned.take() {
                Some(session) => session.close().await,
                None => Ok(()),
            }
        };
        tokio::pin!(shutdown);
        let mut tick = interval();
        let mut dirty = true;
        let mut animation_clock = Instant::now();
        if let Ok(chat) = &mut chat {
            chat.stopping();
        }
        loop {
            tokio::select! {
                biased;
                result = &mut shutdown => break result.err(),
                _ = interrupt.recv() => {},
                now = tick.tick(), if display.is_none() => {
                    if let (Some(terminal), Ok(chat)) = (&mut terminal, &mut chat) {
                        match keyboard(chat, &mut commands, &mut dirty).and_then(|_| draw(terminal, chat, &mut commands, &mut dirty, &mut animation_clock, now)) {
                            Ok(()) => {},
                            Err(error) => display = Some(SessionError::Terminal(error)),
                        }
                    }
                }
            }
        }
    };
    let reporting = modes.armed;
    let mut restore = modes.restore().err();
    if reporting {
        let status =
            if result.is_err() || shutdown.is_some() || restore.is_some() || display.is_some() {
                Status::Error
            } else {
                match commands.status {
                    Status::Done | Status::Error => commands.status,
                    _ => Status::Idle,
                }
            };
        if let Err(error) = commands.report(status) {
            restore.get_or_insert(error);
        }
    }
    if reporting
        && restore.is_none()
        && let Some(attachment) = &attachment
    {
        let mut command = format!("cocoa resume {}", attachment.public_id());
        if let Some(directory) = &data_directory {
            command.push_str(" --data-dir '");
            command.push_str(&directory.to_string_lossy().replace("'", "'\"'\"'"));
            command.push('\'');
        }
        if let Err(error) = writeln!(
            io::stdout(),
            "Resume the session with: \x1b[1m{command}\x1b[22m"
        ) {
            restore = Some(error);
        }
    }
    if result.is_ok() && shutdown.is_none() && restore.is_none() && display.is_none() {
        return Ok(());
    }
    Err(TuiError {
        reason: result.err().unwrap_or(SessionError::Cleanup),
        shutdown: shutdown.map(Box::new),
        restore,
        display,
    })
}

async fn session(
    owned: &mut Option<Session>,
    startup: Startup<'_>,
    commands: &mut Commands,
    terminal: &mut DefaultTerminal,
    chat: &mut Chat,
    interrupt: &mut Signal,
) -> Result<(), SessionError> {
    let Startup {
        selection,
        directory_label,
        mut prompt,
        data_directory,
        working_directory,
        attachment,
    } = startup;
    let config::Selection {
        mut alias,
        mut model,
        mut thinking,
        mut choice,
    } = selection;
    if let Some(name) = &model
        && !name.contains('/')
    {
        alias = Some(name.clone());
        model = None;
    }
    let mut directory_label = directory_label.to_owned();
    let mut working_directory = working_directory.to_owned();
    let mut tick = interval();
    let mut dirty = true;
    let mut animation_clock = Instant::now();
    let mut events = owned.as_ref().map(|active| active.events.subscribe());
    loop {
        tokio::select! {
            biased;
            _ = interrupt.recv() => return Ok(()),
            now = tick.tick() => {
                if let Some(receiver) = &mut events
                    && observations(chat, receiver, &mut dirty).await
                    && let Some(active) = owned.as_ref()
                {
                    reconcile(chat, &active.engine, &mut dirty).await;
                }
                if let Some(active) = owned.as_mut() {
                    agent_observations(chat, &mut active.agent_updates, &mut dirty);
                }
                let polled = commands.poll(chat).await;
                dirty |= polled.changed;
                if let Some(picker) = &mut commands.picker { dirty |= picker.poll().await; }
                if polled.settings_saved {
                    match config::aliases() {
                        Ok(aliases) => chat.set_aliases(&config::alias_names(&aliases)),
                        Err(error) => chat.input_notice(&error.to_string()),
                    }
                }
                if polled.settings_saved && owned.is_none() {
                    match resolve_selection(config::Selection { alias: alias.clone(), model: model.clone(), thinking, choice }).await {
                        Ok(configuration) => {
                            show_configuration(chat, &directory_label, &configuration);
                        }
                        Err(error) => chat.notice(&error.to_string()),
                    }
                }
                let action = if commands.dialog.as_ref().is_some_and(Dialog::is_onboarding) {
                    keyboard(chat, commands, &mut dirty)?
                } else if let Some(prompt) = prompt.take() {
                    chat.initial_prompt(&prompt)?
                } else {
                    keyboard(chat, commands, &mut dirty)?
                };
                let (action, picked_choice) = match action {
                    Action::SelectAliasChoice { alias, choice } => (Action::Command(format!("/model {alias}")), Some(choice)),
                    action => (action, None),
                };
                match action {
                    Action::Exit => return Ok(()),
                    Action::Command(command) => {
                        if command == "/rewind-close" {
                            commands.rewind = None;
                            dirty = true;
                            continue;
                        }
                        if command == "/sessions-close" {
                            commands.picker = None;
                            commands.picker_cwd = None;
                            dirty = true;
                            continue;
                        }
                        let session_action = match config::session_action(&command) {
                            Ok(action) => action,
                            Err(error) => { chat.input_notice(&error.to_string()); dirty = true; continue; }
                        };
                        if let Some(action) = session_action {
                            let database = match attachment.as_ref() {
                                Some(attached) => Ok(Some(attached.database())),
                                None => match session_data::database_path(data_directory) {
                                    Ok(path) => SessionDatabase::open_existing(&path).await.map_err(SessionDataError::Persistence),
                                    Err(error) => Err(error),
                                },
                            };
                            let database = match database {
                                Ok(database) => database,
                                Err(error) => { chat.input_notice(&error.to_string()); dirty = true; continue; }
                            };
                            let rewinding = matches!(action, config::SessionAction::Rewind | config::SessionAction::RewindBefore(_));
                            if rewinding {
                                let active = match owned.as_ref() {
                                    Some(session) => session.has_active_agents().await.map_err(|error| SessionError::Recovery(Box::new(error)))?,
                                    None => false,
                                };
                                if !matches!(commands.status, Status::Idle | Status::Done) || active || commands.save.is_some() || commands.paste.is_some() || commands.mcp_job.is_some() || commands.oauth.is_some() || commands.connection.is_some() {
                                    chat.input_notice("Cannot rewind while a turn, delegated work or a local operation is active.");
                                    dirty = true;
                                    continue;
                                }
                                if matches!(action, config::SessionAction::Rewind) {
                                    let Some(attached) = attachment.as_ref() else {
                                        chat.input_notice("No accepted prompts to rewind."); dirty = true; continue;
                                    };
                                    match attached.rewind_points(resume_limits()).await {
                                        Ok(points) if !points.is_empty() => commands.rewind = Some(RewindPicker::new(points)),
                                        Ok(_) => chat.input_notice("No accepted prompts to rewind."),
                                        Err(error) => chat.input_notice(&error.to_string()),
                                    }
                                    dirty = true;
                                    continue;
                                }
                            }
                            let query = match &action {
                                config::SessionAction::Browse(query) => Some(query.clone()),
                                config::SessionAction::Resume(resume) if resume.reference.is_none() && !resume.last => Some(String::new()),
                                _ => None,
                            };
                            if let Some(query) = query {
                                commands.picker_cwd = match &action { config::SessionAction::Resume(resume) => resume.cwd.clone(), _ => None };
                                commands.picker = Some(SessionPicker::new(database, working_directory.clone(), query));
                                dirty = true;
                                continue;
                            }
                            let reference = match &action {
                                config::SessionAction::Resume(resume) => {
                                    let Some(database) = &database else { chat.input_notice("No sessions found."); dirty = true; continue; };
                                    match resume_reference(database, resume, &working_directory).await {
                                        Ok(reference) => match database.resolve_session(&reference).await {
                                            Ok(summary) if attachment.as_ref().is_some_and(|current| current.public_id() == &summary.public_id) => {
                                                commands.picker = None;
                                                commands.picker_cwd = None;
                                                dirty = true;
                                                continue;
                                            }
                                            Ok(summary) => Some(summary.public_id.to_string()),
                                            Err(error) => { commands.picker = None; chat.input_notice(&error.to_string()); dirty = true; continue; }
                                        },
                                        Err(error) => { commands.picker = None; chat.input_notice(&error.to_string()); dirty = true; continue; }
                                    }
                                }
                                _ => None,
                            };
                            let active_agents = match owned.as_ref() {
                                Some(active) => match active.has_active_agents().await {
                                    Ok(active_agents) => active_agents,
                                    Err(error) => {
                                        commands.picker = None;
                                        chat.input_notice(&error.to_string()); dirty = true; continue;
                                    }
                                },
                                None => false,
                            };
                            if active_agents || commands.save.is_some() || commands.paste.is_some() {
                                commands.picker = None;
                                chat.input_notice("Cannot switch while delegated work or a local operation is active."); dirty = true; continue;
                            }
                            if chat.has_draft() && !commands.discard_confirmed {
                                commands.picker = None;
                                commands.confirm_switch = Some(command);
                                chat.input_notice("Discard the current draft and attachments to switch sessions? Y confirms; Esc cancels."); dirty = true; continue;
                            }
                            commands.discard_confirmed = false;
                            let mut restored_prompt = None;
                            let target = match (&action, reference) {
                                (config::SessionAction::RewindBefore(record), _) => {
                                    let point = commands.rewind.as_ref().and_then(|picker| picker.points.iter().find(|point| point.record == *record));
                                    let (Some(point), Some(attached), Some(database)) = (point, attachment.as_ref(), database.as_ref()) else {
                                        chat.input_notice("Select a prompt from /rewind first."); dirty = true; continue;
                                    };
                                    restored_prompt = Some(point.input.clone());
                                    match attached.fork_before(*record, resume_limits()).await {
                                        Ok(fork) => {
                                            let reference = fork.public_id().to_string();
                                            drop(fork);
                                            prepare_resume(database, &reference, None).await
                                        }
                                        Err(error) => Err(SwitchError::Persistence(error)),
                                    }
                                }
                                (config::SessionAction::Resume(resume), Some(reference)) => {
                                    let Some(database) = &database else { unreachable!("resolved above"); };
                                    prepare_resume(database, &reference, resume.cwd.as_deref().or(commands.picker_cwd.as_deref())).await
                                }
                                (config::SessionAction::New, _) => {
                                    let configuration = resolve_selection(config::Selection::default()).await;
                                    Ok(PreparedSwitch { attachment: None, session: None, records: Vec::new(), directory: working_directory.clone(), selected: configuration.as_ref().map(|configuration| configuration.alias.clone()).unwrap_or_else(|_| "unconfigured".into()), model_label: configuration.as_ref().map(configuration_label).unwrap_or_else(|_| "unconfigured".into()), thinking_level: configuration.as_ref().ok().and_then(|configuration| configuration.thinking_level), relocated: false })
                                }
                                _ => unreachable!("picker commands returned above"),
                            };
                            let mut target = match target {
                                Ok(target) => target,
                                Err(SwitchError::Setup(error)) if error.cleanup_unconfirmed() => return Err(SessionError::Recovery(Box::new(error))),
                                Err(error) => { commands.picker = None; commands.picker_cwd = None; chat.input_notice(&error.to_string()); dirty = true; continue; }
                            };
                            commands.picker = None;
                            commands.picker_cwd = None;
                            if let Some(previous) = owned.take() {
                                match reset(previous, terminal, chat, commands, interrupt).await {
                                    Ok(Action::Exit) => { if let Some(session) = target.session.take() { session.close().await.map_err(|error| SessionError::Recovery(Box::new(error)))?; } return Ok(()); }
                                    Ok(_) => {},
                                    Err(error) => { if let Some(session) = target.session.take() { session.close().await.map_err(|error| SessionError::Recovery(Box::new(error)))?; } return Err(error); }
                                }
                            }
                            commands.rewind = None;
                            working_directory = target.directory;
                            directory_label = working_directory.display().to_string();
                            let selected = target.selected;
                            if selected.contains('/') { alias = None; model = Some(selected.clone()); } else { alias = (selected != "unconfigured").then(|| selected.clone()); model = None; }
                            choice = None;
                            thinking = None;
                            chat.restore_transcript(&target.records);
                            chat.set_input_bytes(target.session.as_ref().map_or(65_536, |active| active.input_bytes))?;
                            if let Some(input) = &restored_prompt {
                                if let Err(error) = chat.restore_prompt(input) {
                                    chat.input_notice(&format!("Cannot restore selected prompt: {error}"));
                                }
                                chat.notice(REWIND_NOTICE);
                            }
                            chat.set_working_directory(&working_directory);
                            commands.working_directory.clone_from(&working_directory);
                            commands.path_checked = None;
                            show_selection(chat, &directory_label, &target.model_label, target.thinking_level);
                            match cocoa_tools::skill_commands(&working_directory, std::env::home_dir().as_deref()) {
                                Ok(skills) => chat.set_skills(skills),
                                Err(error) => chat.notice(&format!("Cannot discover skill commands: {error}")),
                            }
                            if matches!(action, config::SessionAction::Resume(_)) {
                                if let Some(attachment) = &target.attachment { chat.notice(&format!("Session {}", attachment.public_id())); }
                                chat.notice(RESUME_NOTICE);
                            }
                            if target.relocated { chat.notice(&format!("Session relocated to {}", working_directory.display())); }
                            if let Some(active) = &target.session { commands.mcp = Some(active.mcp.clone()); chat.set_message_steering(active.message_steering); }
                            events = target.session.as_ref().map(|active| active.events.subscribe());
                            *attachment = target.attachment;
                            *owned = target.session;
                            commands.status = Status::Idle;
                            dirty = true;
                            continue;
                        }
                        let mut words = command.split_whitespace();
                        let parts = (words.next(), words.next(), words.next(), words.next());
                        match parts {
                            (Some(verb @ ("/model" | "/models")), name, level, None) => {
                                if owned.is_some() && !switching_allowed(attachment, chat).await {
                                    chat.input_notice(ALIAS_LOCKED_NOTICE);
                                } else if commands.save.is_some() {
                                    chat.input_notice("A local change is still being saved. Try again after it finishes.");
                                } else if let Some(name) = name {
                                    let level = match level.map(config::parse_thinking).transpose() {
                                        Ok(level) => level.and_then(|choice| choice.0),
                                        Err(error) => { chat.input_notice(&error.to_string()); dirty = true; continue; }
                                    };
                                    match resolve_selection(config::Selection { alias: None, model: Some(name.into()), thinking: level, choice: picked_choice }).await {
                                        Ok(configuration) => {
                                            if release_session(owned, attachment, commands, chat).await? {
                                                events = None;
                                            }
                                            alias = (!name.contains('/')).then(|| configuration.alias.clone());
                                            model = name.contains('/').then(|| name.into());
                                            thinking = level;
                                            choice = picked_choice;
                                            show_configuration(chat, &directory_label, &configuration);
                                        }
                                        Err(error) => chat.input_notice(&error.to_string()),
                                    }
                                } else {
                                    commands.dialog = Some(if verb == "/models" { Dialog::model_picker() } else { Dialog::models() });
                                    if let Some(job) = commands.catalogue.take() {
                                        job.abort();
                                    }
                                    commands.catalogue = Some(tokio::spawn(catalogue::discover()));
                                }
                            }
                            _ => commands.open(&command, chat),
                        }
                        dirty = true;
                    }
                    action @ (Action::CycleAlias | Action::CycleAliasPrevious) => {
                        if owned.is_some() && !switching_allowed(attachment, chat).await {
                            chat.input_notice(ALIAS_LOCKED_NOTICE);
                        } else {
                            match (config::aliases(), config::cycle_entries(alias.as_deref())) {
                                (Ok(aliases), Ok(entries)) => {
                                    chat.set_aliases(&config::alias_names(&aliases));
                                    if entries.is_empty() {
                                        chat.input_notice("No regular model choices configured. Add one with /aliases.");
                                    } else {
                                        let current = choice.or_else(|| model.is_none().then(|| entries[0].choice));
                                        let next = entries.iter().position(|entry| Some(entry.choice) == current).map_or(0, |index| {
                                            if action == Action::CycleAliasPrevious {
                                                (index + entries.len() - 1) % entries.len()
                                            } else {
                                                (index + 1) % entries.len()
                                            }
                                        });
                                        if release_session(owned, attachment, commands, chat).await? {
                                            events = None;
                                        }
                                        alias = Some(entries[next].key.clone());
                                        model = None;
                                        thinking = None;
                                        choice = Some(entries[next].choice);
                                        show_selection(chat, &directory_label, &format!("{}/{} · {}", entries[next].provider, entries[next].key, entries[next].model), entries[next].thinking_level);
                                    }
                                }
                                (Err(error), _) | (_, Err(error)) => chat.input_notice(&error.to_string()),
                            }
                        }
                        dirty = true;
                    }
                    Action::Submit(input) => {
                        commands.status = Status::Working("Running agent");
                        commands.report_status()?;
                        if attachment.is_none() {
                            *attachment = Some(session_data::create(data_directory, &working_directory).await?);
                        }
                        if owned.is_none() {
                            let configuration = match resolve_selection(config::Selection { alias: alias.clone(), model: model.clone(), thinking, choice }).await {
                                Ok(configuration) => configuration,
                                Err(error) => { chat.reject(&error.to_string()); commands.status = Status::Error; dirty = true; continue; }
                            };
                            show_configuration(chat, &directory_label, &configuration);
                            let Some(attached) = attachment.as_ref() else { unreachable!("created above"); };
                            match Session::new(configuration, attached.clone(), commands.mcp.clone()).await {
                                Ok(session) => { commands.mcp = Some(session.mcp.clone()); *owned = Some(session); },
                                Err(error) if error.cleanup_unconfirmed() => return Err(SessionError::Recovery(Box::new(error))),
                                Err(error) => { chat.reject(&error.to_string()); commands.status = Status::Error; dirty = true; continue; }
                            }
                        }
                        let Some(active) = owned.as_mut() else { unreachable!("session initialized above"); };
                        if input.bytes() > active.input_bytes { chat.reject("Prompt exceeds the configured input byte budget."); commands.status = Status::Error; dirty = true; continue; }
                        chat.set_input_bytes(active.input_bytes)?;
                        chat.set_message_steering(active.message_steering);
                        let engine = &mut active.engine;
                        let agent_updates = &mut active.agent_updates;
                        animation_clock = now;
                        let receiver = events.get_or_insert_with(|| active.events.subscribe());
                        let cancellation = CancellationToken::new();
                        let (steering, mut messages) = tokio::sync::mpsc::channel(16);
                        let (recovery, mut failures) = tokio::sync::mpsc::channel(1);
                        let result = {
                        let invocation = engine.run_with_recovery(RunRequest { input }, cancellation.clone(), &mut messages, &recovery);
                        tokio::pin!(invocation);
                        let mut exit_requested = false;
                        let mut terminal_error = None;
                        let mut retry = None;
                        let mut observations_open = true;
                        loop {
                            tokio::select! {
                                biased;
                                _ = interrupt.recv() => {
                                    exit_requested |= cancellation.is_cancelled();
                                    cancellation.cancel();
                                    chat.stopping();
                                    commands.status = Status::Working("Stopping");
                                    dirty = true;
                                }
                                result = &mut invocation => {
                                    observations(chat, receiver, &mut dirty).await;
                                    observations(chat, receiver, &mut dirty).await;
                                    agent_observations(chat, agent_updates, &mut dirty);
                                    if let Some(source) = terminal_error {
                                        return Err(SessionError::ActiveTerminal { source, settlement: result.err().map(Box::new) });
                                    }
                                    if exit_requested {
                                        return match result {
                                            Ok(_) | Err(RunError::Cancelled | RunError::TurnCancelled { .. }) => Ok(()),
                                            Err(error) => Err(SessionError::ExitSettlement(Box::new(error))),
                                        };
                                    }
                                    break result;
                                }
                                Some(failure) = failures.recv(), if !cancellation.is_cancelled() => {
                                    observations(chat, receiver, &mut dirty).await;
                                    observations(chat, receiver, &mut dirty).await;
                                    agent_observations(chat, agent_updates, &mut dirty);
                                    show_pinned_selection(chat, &directory_label, attachment).await;
                                    let reason = match failure.error {
                                        ModelError::Transport { message } | ModelError::Rejected { message } => format!("{message}. Retry or cancel this turn."),
                                        ModelError::Protocol { .. } => "Provider returned an invalid response. Try again or cancel this turn.".into(),
                                        ModelError::Stalled => "Provider response timed out. Try again or cancel this turn.".into(),
                                        ModelError::Cancelled => unreachable!("cancelled requests do not ask for retry"),
                                    };
                                    retry = Some(failure.retry);
                                    chat.provider_paused(&reason);
                                    commands.status = Status::Blocked { kind: Kind::Question, message: "Provider paused; retry or cancel this turn" };
                                    dirty = true;
                                }
                                now = tick.tick() => {
                                    if terminal_error.is_some() {
                                        continue;
                                    }
                                    dirty |= commands.poll(chat).await.changed;
                                    match keyboard(chat, commands, &mut dirty) {
                                        Ok(Action::Steer(input)) => {
                                            if input.bytes() > active.input_bytes {
                                                chat.reject_steering("Prompt exceeds the configured input byte budget.");
                                            } else if steering.try_send(input).is_err() {
                                                chat.reject_steering("Steering queue is full. Your draft is unchanged; submit after the next model round.");
                                            }
                                        },
                                        Ok(Action::Retry) => {
                                            if let Some(retry) = retry.take() {
                                                match retry.send(()) {
                                                    Ok(()) => { chat.provider_resumed(); commands.status = Status::Working("Retrying provider request"); },
                                                    Err(()) => { cancellation.cancel(); chat.stopping(); commands.status = Status::Working("Stopping"); },
                                                }
                                            }
                                        },
                                        Ok(Action::Cancel) => { cancellation.cancel(); chat.stopping(); commands.status = Status::Working("Stopping"); },
                                        Ok(Action::Exit) => { exit_requested = true; cancellation.cancel(); commands.status = Status::Working("Stopping"); },
                                        Err(source) => { terminal_error = Some(source); cancellation.cancel(); continue; },
                                        Ok(Action::SelectAliasChoice { .. }) => {
                                            let notice = if switching_allowed(attachment, chat).await {
                                                "Cancel this turn to change the model."
                                            } else {
                                                ALIAS_LOCKED_NOTICE
                                            };
                                            chat.input_notice(notice);
                                            dirty = true;
                                        }
                                        Ok(Action::Command(command)) => {
                                            let mut words = command.split_whitespace();
                                            match (words.next(), words.next(), words.next(), words.next()) {
                                                (Some("/model"), _, _, None) => {
                                                    let notice = if switching_allowed(attachment, chat).await {
                                                        "Cancel this turn to change the model."
                                                    } else {
                                                        ALIAS_LOCKED_NOTICE
                                                    };
                                                    chat.input_notice(notice);
                                                }
                                                _ => commands.open(&command, chat),
                                            }
                                            dirty = true;
                                        }
                                        _ => {},
                                    }
                                    agent_observations(chat, agent_updates, &mut dirty);
                                    if let Err(error) = draw(terminal, chat, commands, &mut dirty, &mut animation_clock, now) {
                                        cancellation.cancel();
                                        terminal_error = Some(error);
                                    }
                                }
                                observation = receiver.recv(), if observations_open => {
                                    observations_open = !matches!(observation, Observation::Closed);
                                    chat.observe(&observation);
                                    if matches!(&observation, Observation::Event(event) if matches!(event.kind, EventKind::ModelFinished { .. }))
                                        || matches!(observation, Observation::Lagged { .. })
                                    {
                                        show_pinned_selection(chat, &directory_label, attachment).await;
                                    }
                                    dirty = true;
                                }
                            }
                        }
                        };
                        reconcile(chat, engine, &mut dirty).await;
                        agent_observations(chat, agent_updates, &mut dirty);
                        show_pinned_selection(chat, &directory_label, attachment).await;
                        if !messages.is_empty() {
                            chat.notice(&format!("{} queued steering messages were not delivered before this turn ended. Use Up to recall the last message.", messages.len()));
                        }
                        // Consume keys received while busy before settlement enables submission.
                        chat.stopping();
                        match keyboard(chat, commands, &mut dirty)? {
                            Action::Exit => return Ok(()),
                            Action::Cancel | Action::Retry | Action::None | Action::Redraw => {},
                            Action::Command(_) | Action::SelectAliasChoice { .. } => chat.input_notice("Turn finishing. Try the command again after it settles."),
                            Action::Submit(_) | Action::Steer(_) | Action::CycleAlias | Action::CycleAliasPrevious => unreachable!("stopping composer cannot submit or change aliases"),
                            Action::Copy(_) | Action::Paste => unreachable!("keyboard handles clipboard access"),
                        }
                        match result {
                            Ok(output) => { chat.complete(&output.answer); commands.status = Status::Done; },
                            Err(RunError::Cancelled | RunError::TurnCancelled { .. }) => { chat.cancelled(); commands.status = Status::Idle; },
                            Err(error @ RunError::Settlement { .. }) => return Err(SessionError::ExitSettlement(Box::new(error))),
                            Err(error) => {
                                chat.stopping();
                                commands.status = Status::Error;
                                let Some(failed) = owned.take() else { unreachable!("failed invocation owns a session"); };
                                if matches!(reset(failed, terminal, chat, commands, interrupt).await?, Action::Exit) {
                                    return Ok(());
                                }
                                observations(chat, receiver, &mut dirty).await;
                                events = None;
                                if matches!(&error, RunError::Deadline { .. }) {
                                    let reference = attachment
                                        .as_ref()
                                        .map(|attachment| attachment.public_id().to_string());
                                    let database = match attachment.as_ref() {
                                        Some(attached) => Some(attached.database()),
                                        None => SessionDatabase::open_existing(&session_data::database_path(data_directory)?).await.map_err(SessionDataError::Persistence)?,
                                    };
                                    match (database, reference) {
                                        (Some(database), Some(reference)) => {
                                            match prepare_resume(&database, &reference, None).await {
                                                Ok(mut target) => {
                                                    chat.refresh_transcript(&target.records);
                                                    chat.set_input_bytes(target.session.as_ref().map_or(65_536, |active| active.input_bytes))?;
                                                    if let Some(active) = &target.session {
                                                        commands.mcp = Some(active.mcp.clone());
                                                        chat.set_message_steering(active.message_steering);
                                                    }
                                                    events = target.session.as_ref().map(|active| active.events.subscribe());
                                                    *attachment = target.attachment.take();
                                                    *owned = target.session.take();
                                                    chat.retained_session(&crate::run_description(&error));
                                                    commands.status = Status::Idle;
                                                }
                                                Err(SwitchError::Setup(error)) if error.cleanup_unconfirmed() => return Err(SessionError::Recovery(Box::new(error))),
                                                Err(failure) => {
                                                    chat.input_notice(&failure.to_string());
                                                    *attachment = None;
                                                    chat.reset_session(&crate::run_description(&error));
                                                }
                                            }
                                        }
                                        _ => {
                                            *attachment = None;
                                            chat.reset_session(&crate::run_description(&error));
                                        }
                                    }
                                } else {
                                    *attachment = None;
                                    chat.reset_session(&crate::run_description(&error));
                                }
                            },
                        }
                        commands.report_status()?;
                        dirty = true;
                        continue;
                    }
                    _ => {}
                }
                draw(terminal, chat, commands, &mut dirty, &mut animation_clock, now)?;
            }
        }
    }
}

async fn reset(
    failed: Session,
    terminal: &mut DefaultTerminal,
    chat: &mut Chat,
    commands: &mut Commands,
    interrupt: &mut Signal,
) -> Result<Action, SessionError> {
    chat.stopping();
    let shutdown = failed.close();
    tokio::pin!(shutdown);
    let mut tick = interval();
    let mut dirty = true;
    let mut animation_clock = Instant::now();
    let mut exit_requested = false;
    let mut terminal_error = None;
    loop {
        tokio::select! {
            biased;
            _ = interrupt.recv() => exit_requested = true,
            result = &mut shutdown => {
                result.map_err(|error| SessionError::Recovery(Box::new(error)))?;
                if let Some(error) = terminal_error {
                    return Err(SessionError::Terminal(error));
                }
                return Ok(if exit_requested { Action::Exit } else { Action::None });
            }
            now = tick.tick() => {
                if terminal_error.is_none() {
                    match keyboard(chat, commands, &mut dirty).and_then(|action| {
                        exit_requested |= matches!(action, Action::Exit);
                        draw(terminal, chat, commands, &mut dirty, &mut animation_clock, now)
                    }) {
                        Ok(()) => {},
                        Err(error) => terminal_error = Some(error),
                    }
                }
            }
        }
    }
}

/// Limits used to reconcile a session whose runtime this process replaces.
fn resume_limits() -> cocoa_core::store::StoreLimits {
    cocoa_core::store::StoreLimits {
        record_bytes: 64 * 1024 * 1024,
    }
}

/// Whether the live session may still adopt another alias or model.
/// A provider failure before any model output leaves nothing pinned to the old
/// configuration; any retained response keeps the notice.
async fn switching_allowed(attachment: &Option<SessionAttachment>, chat: &mut Chat) -> bool {
    match attachment {
        Some(attached) => match attached.model_switch_allowed().await {
            Ok(allowed) => allowed,
            Err(error) => {
                chat.input_notice(&error.to_string());
                false
            }
        },
        None => false,
    }
}

/// Close the live session so the next submission builds the selected model
/// against the same durable identity. Returns whether a session was released.
async fn release_session(
    owned: &mut Option<Session>,
    attachment: &mut Option<SessionAttachment>,
    commands: &mut Commands,
    chat: &mut Chat,
) -> Result<bool, SessionError> {
    let Some(session) = owned.take() else {
        return Ok(false);
    };
    commands.status = Status::Working("Switching model");
    commands.report_status()?;
    session
        .close()
        .await
        .map_err(|error| SessionError::Recovery(Box::new(error)))?;
    let Some(attached) = attachment.as_ref() else {
        unreachable!("a live session owns its attachment");
    };
    match attached.recover(resume_limits()).await {
        Ok(recovered) => *attachment = Some(recovered),
        Err(error) => {
            *attachment = None;
            chat.reset_session(&error.to_string());
        }
    }
    commands.status = Status::Idle;
    commands.report_status()?;
    Ok(true)
}

struct Startup<'a> {
    pub selection: config::Selection,
    pub directory_label: &'a str,
    pub prompt: Option<String>,
    pub data_directory: Option<&'a Path>,
    pub working_directory: &'a Path,
    pub attachment: &'a mut Option<SessionAttachment>,
}

struct PreparedSwitch {
    attachment: Option<SessionAttachment>,
    session: Option<Session>,
    records: Vec<cocoa_core::store::StoredRecord>,
    directory: PathBuf,
    selected: String,
    model_label: String,
    thinking_level: Option<ThinkingLevel>,
    relocated: bool,
}

#[derive(Debug, thiserror::Error)]
enum SwitchError {
    #[error(transparent)]
    Persistence(#[from] PersistenceError),
    #[error(transparent)]
    Config(#[from] config::ConfigError),
    #[error(
        "session directory is unavailable; use cocoa resume SESSION --cwd DIRECTORY to relocate it"
    )]
    Directory(#[source] io::Error),
    #[error(transparent)]
    Setup(#[from] crate::session::CommandError),
}

async fn resume_reference(
    database: &SessionDatabase,
    command: &config::ResumeCommand,
    directory: &Path,
) -> Result<String, SwitchError> {
    if let Some(reference) = &command.reference {
        return Ok(reference.clone());
    }
    let page = database
        .list_sessions((!command.all).then_some(directory), 1, None)
        .await?;
    page.sessions
        .into_iter()
        .next()
        .map(|session| session.public_id.to_string())
        .ok_or(PersistenceError::SessionNotFound.into())
}

async fn resolve_selection(
    selection: config::Selection,
) -> Result<config::Configuration, config::ConfigError> {
    tokio::task::spawn_blocking(move || {
        config::load_selection(
            selection.alias,
            selection.model,
            selection.thinking,
            selection.choice,
        )
    })
    .await
    .map_err(|_| config::ConfigError::Worker)?
}

async fn prepare_resume(
    database: &SessionDatabase,
    reference: &str,
    cwd: Option<&Path>,
) -> Result<PreparedSwitch, SwitchError> {
    let limits = resume_limits();
    let attachment = database.attach_session(reference, limits).await?;
    let snapshot = attachment.load(limits).await?;
    let directory = cwd
        .unwrap_or(&snapshot.metadata.working_directory)
        .canonicalize()
        .map_err(SwitchError::Directory)?;
    if !directory.is_dir() {
        return Err(SwitchError::Directory(io::Error::other("not a directory")));
    }
    let configuration = match &snapshot.settings {
        StoredSessionSettings::Unconfigured => None,
        StoredSessionSettings::Bound { effective } => {
            let effective = effective.clone();
            Some(
                tokio::task::spawn_blocking(move || config::resume_configuration(&effective))
                    .await
                    .map_err(|_| config::ConfigError::Worker)??,
            )
        }
    };
    let relocated = directory != snapshot.metadata.working_directory;
    if relocated {
        attachment.relocate(&directory).await?;
    }
    let attachment = attachment.recover(limits).await?;
    let mut snapshot = attachment.load(limits).await?;
    let selected = configuration
        .as_ref()
        .map(|configuration| configuration.alias.clone())
        .unwrap_or_else(|| "unconfigured".into());
    let thinking_level = configuration
        .as_ref()
        .and_then(|configuration| configuration.thinking_level);
    let model_label = configuration
        .as_ref()
        .map(configuration_label)
        .unwrap_or_else(|| selected.clone());
    let session = match configuration {
        Some(configuration) => Some(Session::new(configuration, attachment.clone(), None).await?),
        None => None,
    };
    let records = snapshot
        .histories
        .remove(&attachment.root_agent())
        .map(|history| history.records)
        .unwrap_or_default();
    Ok(PreparedSwitch {
        attachment: Some(attachment),
        session,
        records,
        directory,
        selected,
        model_label,
        thinking_level,
        relocated,
    })
}

fn show_configuration(chat: &mut Chat, directory: &str, configuration: &config::Configuration) {
    show_selection(
        chat,
        directory,
        &configuration_label(configuration),
        configuration.thinking_level,
    );
}

/// Show concrete model identity and effort, dropping the effort when space is tight.
fn show_selection(
    chat: &mut Chat,
    directory: &str,
    selected: &str,
    thinking_level: Option<ThinkingLevel>,
) {
    let thinking = thinking_level.map(|level| dialog::thinking_label(Some(level)));
    chat.set_context(directory, selected);
    chat.set_thinking(thinking.as_deref());
}

async fn show_pinned_selection(
    chat: &mut Chat,
    directory: &str,
    attachment: &Option<SessionAttachment>,
) {
    let Some(attached) = attachment else {
        return;
    };
    match attached.settings().await {
        Ok(StoredSessionSettings::Bound { effective }) => {
            let label =
                model_selection_label(&effective.provider, &effective.alias, &effective.model);
            chat.set_context(directory, &label);
            chat.set_thinking(effective.thinking_level.as_deref());
        }
        Ok(StoredSessionSettings::Unconfigured) => {}
        Err(error) => chat.input_notice(&error.to_string()),
    }
}

fn configuration_label(configuration: &config::Configuration) -> String {
    model_selection_label(
        &configuration.provider.to_string(),
        &configuration.alias,
        &configuration.model,
    )
}

fn model_selection_label(provider: &str, alias: &str, model: &str) -> String {
    if alias.contains('/') {
        format!("{provider}/{model}")
    } else {
        format!("{provider}/{alias} · {model}")
    }
}

impl From<SessionDataError> for SessionError {
    fn from(error: SessionDataError) -> Self {
        Self::Persistence(Box::new(error))
    }
}

fn interval() -> Interval {
    let period = Duration::from_nanos(16_666_667);
    let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    tick
}

fn keyboard(chat: &mut Chat, commands: &mut Commands, dirty: &mut bool) -> io::Result<Action> {
    if commands.paste.is_some() {
        return Ok(Action::None);
    }
    for _ in 0..64 {
        let event = if let Some(key) = commands.pending_key.take() {
            Event::Key(key)
        } else {
            if !crossterm::event::poll(Duration::from_micros(100))? {
                commands.ignore_enter = false;
                break;
            }
            crossterm::event::read()?
        };
        if commands.confirm_switch.is_some() {
            if let Event::Key(key) = &event {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Char('y' | 'Y') => {
                        commands.discard_confirmed = true;
                        if let Some(command) = commands.confirm_switch.take() {
                            return Ok(Action::Command(command));
                        }
                    }
                    KeyCode::Esc | KeyCode::Char('n' | 'N') => {
                        commands.confirm_switch = None;
                        chat.input_notice("Switch cancelled. Draft unchanged.");
                        *dirty = true;
                    }
                    _ => {}
                }
            }
            continue;
        }
        if let Some(picker) = &mut commands.rewind {
            *dirty = true;
            match picker.input(event) {
                Action::Redraw | Action::None => continue,
                action => return Ok(action),
            }
        }
        if let Some(picker) = &mut commands.picker {
            *dirty = true;
            match picker.input(event) {
                Action::Redraw | Action::None => continue,
                action => return Ok(action),
            }
        }
        if let Some(dialog) = &mut commands.dialog {
            *dirty = true;
            if commands.save.is_none() {
                let action = dialog.input(event);
                let action = commands.handle(action);
                if !matches!(action, Action::None) {
                    return Ok(action);
                }
                if commands.dialog.is_none() {
                    break;
                }
            }
            continue;
        }
        if let Event::Key(key) = event {
            if key.code == KeyCode::Enter
                && key.modifiers == KeyModifiers::NONE
                && commands.ignore_enter
            {
                continue;
            }
            commands.path_deadline = Some(Instant::now() + Duration::from_millis(150));
            if key.code == KeyCode::Enter
                && key.modifiers == KeyModifiers::NONE
                && key.kind == KeyEventKind::Press
                && chat.can_submit()
                && commands.attach_path(chat)
            {
                commands.pending_key = Some(key);
                return Ok(Action::None);
            }
        }
        let pasted = matches!(event, Event::Paste(_));
        match chat.input(event) {
            Action::Redraw => *dirty = true,
            Action::Copy(text) => {
                execute!(io::stdout(), CopyToClipboard::to_clipboard_from(text))?;
                chat.confirm_copy(Instant::now().into_std());
                *dirty = true;
            }
            Action::Paste => {
                commands.paste = Some(Paste {
                    source: PasteSource::Clipboard,
                    busy: !chat.can_submit(),
                    task: tokio::task::spawn_blocking(clipboard::read),
                });
                *dirty = true;
                break;
            }
            action @ (Action::Submit(_)
            | Action::Steer(_)
            | Action::Command(_)
            | Action::SelectAliasChoice { .. }
            | Action::CycleAlias
            | Action::CycleAliasPrevious
            | Action::Cancel
            | Action::Retry
            | Action::Exit) => {
                *dirty = true;
                return Ok(action);
            }
            Action::None => {}
        }
        if pasted {
            // A paste can complete an image path that the next key would otherwise attach.
            commands.path_deadline = Some(Instant::now());
            commands.path_checked = None;
        }
    }
    if commands.dialog.is_none()
        && commands.paste.is_none()
        && commands
            .path_deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
    {
        commands.path_deadline = None;
        commands.attach_path(chat);
    }
    Ok(Action::None)
}

fn agent_observations(chat: &mut Chat, receiver: &mut AgentUpdates, dirty: &mut bool) {
    for update in receiver.drain() {
        chat.observe_agent(&update);
        *dirty = true;
    }
}

async fn observations(chat: &mut Chat, receiver: &mut EventReceiver, dirty: &mut bool) -> bool {
    let mut lagged = false;
    for _ in 0..32 {
        let observation = tokio::select! {
            biased;
            observation = receiver.recv() => observation,
            () = std::future::ready(()) => break,
        };
        lagged |= matches!(observation, Observation::Lagged { .. });
        let closed = matches!(observation, Observation::Closed);
        chat.observe(&observation);
        *dirty = true;
        if closed {
            break;
        }
    }
    lagged
}

async fn reconcile(chat: &mut Chat, engine: &Engine, dirty: &mut bool) {
    match engine.activity_snapshot().await {
        Ok(snapshot) => {
            for event in snapshot {
                chat.observe(&Observation::Event(event));
            }
        }
        Err(error) => {
            tracing::warn!(%error, "activity reconciliation unavailable");
            chat.observe(&Observation::Lagged { missed: 1 });
        }
    }
    *dirty = true;
}

fn draw(
    terminal: &mut DefaultTerminal,
    chat: &mut Chat,
    commands: &mut Commands,
    dirty: &mut bool,
    animation_clock: &mut Instant,
    now: Instant,
) -> io::Result<()> {
    *dirty |= chat.tick(now.duration_since(*animation_clock));
    *animation_clock = now;
    if *dirty {
        terminal.draw(|frame| {
            if commands.dialog.as_ref().is_some_and(Dialog::is_onboarding) {
                frame.render_widget(Clear, frame.area());
            } else {
                chat.render_terminal(frame);
            }
            if let Some(dialog) = &mut commands.dialog {
                dialog.render(frame, chat.theme());
            }
            if let Some(picker) = &mut commands.picker {
                picker.render(frame, chat.theme());
            }
            if let Some(picker) = &commands.rewind {
                picker.render(frame, chat.theme());
            }
        })?;
        *dirty = false;
    }
    commands.report_status()?;
    Ok(())
}

#[derive(Default)]
struct Commands {
    working_directory: PathBuf,
    dialog: Option<Dialog>,
    picker: Option<SessionPicker>,
    rewind: Option<RewindPicker>,
    picker_cwd: Option<PathBuf>,
    confirm_switch: Option<String>,
    discard_confirmed: bool,
    mcp: Option<cocoa::mcp::McpManager>,
    mcp_job: Option<(
        CancellationToken,
        tokio::task::JoinHandle<Result<(), LocalError>>,
    )>,
    catalogue: Option<tokio::task::JoinHandle<catalogue::Catalogue>>,
    connection: Option<(
        cocoa_llm::ProviderKind,
        tokio::task::JoinHandle<Result<catalogue::Catalogue, LocalError>>,
    )>,
    oauth: Option<(
        CancellationToken,
        tokio::task::JoinHandle<Result<cocoa_oauth::Account, LocalError>>,
    )>,
    save: Option<tokio::task::JoinHandle<Result<Saved, LocalError>>>,
    paste: Option<Paste>,
    path_deadline: Option<Instant>,
    path_checked: Option<ImagePath>,
    pending_key: Option<KeyEvent>,
    ignore_enter: bool,
    status: Status,
    reported_status: Option<Status>,
}

struct Paste {
    source: PasteSource,
    busy: bool,
    task: tokio::task::JoinHandle<Result<clipboard::Content, clipboard::ClipboardError>>,
}

enum PasteSource {
    Clipboard,
    File(ImagePath),
}

enum Saved {
    AliasChoices,
    Key,
    Setup,
    Subscription,
    SignedOut,
    Link,
}

/// One poll of background jobs, reporting whether a saved setting may change
/// what a session started later resolves.
struct PollOutcome {
    changed: bool,
    settings_saved: bool,
}

#[derive(Debug, thiserror::Error)]
enum LocalError {
    #[error(transparent)]
    Mcp(#[from] cocoa::mcp::McpError),
    #[error(transparent)]
    OAuth(#[from] cocoa_oauth::OAuthError),
    #[error(transparent)]
    Config(#[from] config::ConfigError),
    #[error(transparent)]
    Auth(#[from] auth::AuthError),
    #[error(transparent)]
    Connect(#[from] catalogue::ConnectError),
    #[error("local setup worker failed")]
    Worker(#[from] tokio::task::JoinError),
}

impl Commands {
    fn report(&mut self, status: Status) -> io::Result<()> {
        if self.reported_status != Some(status) {
            status.report()?;
            self.reported_status = Some(status);
        }
        Ok(())
    }

    fn report_status(&mut self) -> io::Result<()> {
        let status = if self.status == Status::Error {
            Status::Error
        } else if self.save.is_some() {
            Status::Working("Saving settings")
        } else if let Some(dialog) = &self.dialog {
            dialog.program_status()
        } else {
            self.status
        };
        self.report(status)
    }

    fn open(&mut self, command: &str, chat: &mut Chat) {
        if self.save.is_some() {
            chat.notice("A local change is still being saved. Try again after it finishes.");
            return;
        }
        self.status = Status::Idle;
        if let Some((cancellation, _)) = self.mcp_job.take() {
            cancellation.cancel();
        }
        if let Some((cancellation, _)) = self.oauth.take() {
            cancellation.cancel();
        }
        let words: Vec<_> = command.trim_start_matches('/').split_whitespace().collect();
        match words.as_slice() {
            ["mcp"] => {
                let manager = match &self.mcp {
                    Some(manager) => Ok(manager.clone()),
                    None => config::mcp_manager(),
                };
                match manager {
                    Ok(manager) => {
                        match config::mcp_servers()
                            .map_err(LocalError::from)
                            .and_then(|servers| {
                                manager.configure(servers).map_err(LocalError::from)
                            })
                            .and_then(|()| manager.servers().map_err(LocalError::from))
                        {
                            Ok(servers) => {
                                self.dialog = Some(Dialog::mcp(servers));
                                self.mcp = Some(manager.clone());
                                let cancellation = CancellationToken::new();
                                let worker = cancellation.clone();
                                let job = tokio::spawn(async move {
                                    tokio::select! {
                                        biased;
                                        () = worker.cancelled() => Err(cocoa_oauth::OAuthError::Cancelled.into()),
                                        result = manager.connect_all() => Ok(result?),
                                    }
                                });
                                self.mcp_job = Some((cancellation, job));
                            }
                            Err(error) => chat.notice(&error.to_string()),
                        }
                    }
                    Err(error) => chat.notice(&error.to_string()),
                }
            }
            [verb @ ("aliases" | "models")] => {
                self.dialog = Some(match *verb {
                    "models" => Dialog::model_picker(),
                    _ => Dialog::aliases(),
                });
                if let Some(job) = self.catalogue.take() {
                    job.abort();
                }
                self.catalogue = Some(tokio::spawn(catalogue::discover()));
            }
            [verb @ ("login" | "logout")] => {
                self.dialog = Some(Dialog::auth(*verb == "login", None))
            }
            [verb @ ("login" | "logout"), provider] => match provider.parse() {
                Ok(provider) => self.dialog = Some(Dialog::auth(*verb == "login", Some(provider))),
                Err(_) => chat.notice("Unknown provider. Use /login or /logout to choose one."),
            },
            ["usage"] => {
                self.save = Some(tokio::spawn(async move {
                    cocoa_oauth::open_link(cocoa::chatgpt::MANAGE_USAGE).await?;
                    Ok(Saved::Link)
                }));
            }
            _ => chat
                .input_notice("Unknown command. Use /login, /logout, /models, /aliases or /mcp."),
        }
    }

    fn handle(&mut self, action: dialog::Action) -> Action {
        match action {
            dialog::Action::None => {}
            dialog::Action::Exit => return Action::Exit,
            dialog::Action::FinishSetup => self.dialog = None,
            dialog::Action::CancelConnection => {
                if let Some((cancellation, _)) = self.mcp_job.take() {
                    cancellation.cancel();
                }
                if let Some((cancellation, _)) = self.oauth.take() {
                    cancellation.cancel();
                }
                if let Some((_, job)) = self.connection.take() {
                    job.abort();
                }
            }
            action @ (dialog::Action::McpAdd { .. }
            | dialog::Action::McpConnect(_)
            | dialog::Action::McpAuth(_)
            | dialog::Action::McpToggle(_)
            | dialog::Action::McpLogout(_)
            | dialog::Action::McpRemove(_)) => {
                if let Some((cancellation, _)) = self.mcp_job.take() {
                    cancellation.cancel();
                }
                if let Some(manager) = self.mcp.clone() {
                    let cancellation = CancellationToken::new();
                    let worker = cancellation.clone();
                    let job = tokio::spawn(async move {
                        let operation = async {
                            let (name, authenticate) = match action {
                                dialog::Action::McpAdd { name, url } => {
                                    config::save_mcp(
                                        name.clone(),
                                        config::McpEdit::Add(cocoa::mcp::Server::new(&url, None)?),
                                    )?;
                                    manager.configure(config::mcp_servers()?)?;
                                    (name, true)
                                }
                                dialog::Action::McpConnect(name) => (name, false),
                                dialog::Action::McpAuth(name) => {
                                    let login = manager.begin_auth(&name).await?;
                                    login.open_browser().await?;
                                    login.finish(worker.clone()).await?;
                                    (name, false)
                                }
                                dialog::Action::McpToggle(name) => {
                                    let mut servers = config::mcp_servers()?;
                                    let mut server = servers
                                        .remove(&name)
                                        .ok_or(cocoa::mcp::McpError::Unavailable)?;
                                    let enabled = !server.enabled();
                                    server.set_enabled(enabled);
                                    config::save_mcp(
                                        name.clone(),
                                        config::McpEdit::Replace(server),
                                    )?;
                                    manager.configure(config::mcp_servers()?)?;
                                    if !enabled {
                                        return Ok(());
                                    }
                                    (name, false)
                                }
                                dialog::Action::McpLogout(name) => {
                                    manager.logout(&name).await?;
                                    return Ok(());
                                }
                                dialog::Action::McpRemove(name) => {
                                    manager.logout(&name).await?;
                                    config::save_mcp(name, config::McpEdit::Remove)?;
                                    manager.configure(config::mcp_servers()?)?;
                                    return Ok(());
                                }
                                _ => unreachable!("MCP actions are selected above"),
                            };
                            match manager.connect(&name).await {
                                Err(cocoa::mcp::McpError::Auth { .. }) if authenticate => {
                                    let login = manager.begin_auth(&name).await?;
                                    login.open_browser().await?;
                                    login.finish(worker.clone()).await?;
                                    manager.connect(&name).await?;
                                }
                                result => result?,
                            }
                            Ok(())
                        };
                        tokio::select! {
                            biased;
                            () = worker.cancelled() => Err(cocoa_oauth::OAuthError::Cancelled.into()),
                            result = operation => result,
                        }
                    });
                    self.mcp_job = Some((cancellation, job));
                }
            }
            dialog::Action::ChatGptLogin { registration } => {
                if let Some((cancellation, _)) = self.oauth.take() {
                    cancellation.cancel();
                }
                let cancellation = CancellationToken::new();
                let worker_cancellation = cancellation.clone();
                let job = tokio::spawn(async move {
                    let sign_in = async {
                        let client = cocoa::chatgpt::client(None)?;
                        let login = client.begin(registration).await?;
                        login.open_browser().await?;
                        Ok(login.finish(worker_cancellation.clone()).await?)
                    };
                    tokio::select! {
                        biased;
                        () = worker_cancellation.cancelled() => Err(cocoa_oauth::OAuthError::Cancelled.into()),
                        result = sign_in => result,
                    }
                });
                self.oauth = Some((cancellation, job));
            }
            dialog::Action::SelectAccount(registration) => {
                self.save = Some(tokio::spawn(async move {
                    cocoa::chatgpt::client(None)?.select(&registration).await?;
                    Ok(Saved::Subscription)
                }));
            }
            dialog::Action::LogoutAccount(registration) => {
                self.save = Some(tokio::spawn(async move {
                    cocoa::chatgpt::client(None)?.logout(&registration).await?;
                    Ok(Saved::SignedOut)
                }));
            }
            dialog::Action::ConnectProvider { provider, key } => {
                self.connection = Some((
                    provider,
                    tokio::spawn(async move {
                        let resolved = tokio::task::spawn_blocking(move || {
                            config::setup_provider(provider, key)
                        })
                        .await??;
                        Ok(catalogue::connect(resolved).await?)
                    }),
                ));
            }
            dialog::Action::SaveSetup {
                provider,
                key,
                aliases,
            } => {
                self.save = Some(tokio::task::spawn_blocking(move || {
                    if let Some(key) = key {
                        auth::save(provider, Some(key))?;
                    }
                    config::save_setup(provider, aliases)?;
                    Ok(Saved::Setup)
                }));
            }
            dialog::Action::SelectAliasChoice { alias, choice } => {
                self.dialog = None;
                return Action::SelectAliasChoice { alias, choice };
            }
            dialog::Action::SelectModel { model, thinking } => {
                self.dialog = None;
                return Action::Command(match thinking {
                    Some(level) => {
                        format!("/model {model} {}", dialog::thinking_label(Some(level)))
                    }
                    None => format!("/model {model}"),
                });
            }
            dialog::Action::SaveAliasChoices {
                name,
                entries,
                make_default,
            } => {
                self.save = Some(tokio::task::spawn_blocking(move || {
                    config::save_alias_choices(name, entries, make_default)?;
                    Ok(Saved::AliasChoices)
                }));
            }
            dialog::Action::Close => {
                self.dialog = None;
                if let Some((cancellation, _)) = self.mcp_job.take() {
                    cancellation.cancel();
                }
                if let Some((cancellation, _)) = self.oauth.take() {
                    cancellation.cancel();
                }
                if let Some((_, job)) = self.connection.take() {
                    job.abort();
                }
                if let Some(job) = self.catalogue.take() {
                    job.abort();
                }
            }
            dialog::Action::SaveKey { provider, key } => {
                self.save = Some(tokio::task::spawn_blocking(move || {
                    auth::save(provider, key)?;
                    Ok(Saved::Key)
                }));
            }
        }
        Action::None
    }

    fn attach_path(&mut self, chat: &Chat) -> bool {
        let path = chat.image_path();
        if path == self.path_checked {
            return false;
        }
        self.path_checked.clone_from(&path);
        let Some(path) = path else { return false };
        let file = self.working_directory.join(&path.path);
        self.paste = Some(Paste {
            source: PasteSource::File(path),
            busy: !chat.can_submit(),
            task: tokio::task::spawn_blocking(move || clipboard::read_file(&file)),
        });
        true
    }

    async fn poll(&mut self, chat: &mut Chat) -> PollOutcome {
        let mut outcome = PollOutcome {
            changed: false,
            settings_saved: false,
        };
        if self
            .mcp_job
            .as_ref()
            .is_some_and(|(_, job)| job.is_finished())
        {
            if let Some((_, job)) = self.mcp_job.take() {
                let error = match job.await {
                    Ok(Ok(())) => None,
                    Ok(Err(error)) => Some(error.to_string()),
                    Err(_) => Some("MCP connection worker failed".into()),
                };
                if let Some(manager) = &self.mcp {
                    match manager.servers() {
                        Ok(servers) => match &mut self.dialog {
                            Some(dialog) => dialog.mcp_updated(servers, error),
                            None => {
                                if let Some(error) = error {
                                    chat.notice(&error);
                                }
                            }
                        },
                        Err(error) => chat.notice(&error.to_string()),
                    }
                }
            }
            outcome.changed = true;
        }
        if self
            .oauth
            .as_ref()
            .is_some_and(|(_, job)| job.is_finished())
        {
            if let Some((_, job)) = self.oauth.take() {
                match job.await {
                    Ok(Ok(account)) if account.inference_enabled => {
                        outcome.settings_saved = true;
                        chat.notice("Signed in. Start a new session after changing accounts.");
                        let action = self
                            .dialog
                            .as_mut()
                            .map(Dialog::subscription_ready)
                            .unwrap_or(dialog::Action::None);
                        self.handle(action);
                    }
                    result => {
                        let message = match result {
                            Ok(Ok(_)) => "Identity-only sign-in; ChatGPT inference permission was not granted. Reconnect to enable plan use.".into(),
                            Ok(Err(error)) => error.to_string(),
                            Err(_) => "ChatGPT sign-in failed. Retry from the account menu.".into(),
                        };
                        if let Some(dialog) = &mut self.dialog {
                            dialog.subscription_error(message);
                        } else {
                            chat.notice(&message);
                        }
                    }
                }
            }
            outcome.changed = true;
        }
        if self
            .paste
            .as_ref()
            .is_some_and(|paste| paste.task.is_finished())
        {
            if let Some(paste) = self.paste.take() {
                // Enter buffered during a busy paste must not become a later submission.
                self.ignore_enter |= paste.busy && chat.can_submit();
                let result = match paste.task.await {
                    Ok(Ok(clipboard::Content::Image(image))) => match paste.source {
                        PasteSource::Clipboard => chat.paste_image(image),
                        PasteSource::File(path) => chat.paste_image_path(&path, image),
                    }
                    .map_err(|error| error.to_string()),
                    Ok(Ok(clipboard::Content::Text(text))) => {
                        self.path_deadline = Some(Instant::now());
                        chat.paste_text(&text).map_err(|error| error.to_string())
                    }
                    Ok(Err(error)) => Err(error.to_string()),
                    Err(_) => Err("Image or clipboard read failed.".into()),
                };
                if let Err(error) = result {
                    self.ignore_enter = true;
                    self.pending_key = None;
                    chat.notice(&error);
                } else {
                    self.path_checked = None;
                }
            }
            outcome.changed = true;
        }
        if self
            .catalogue
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
        {
            if let Some(job) = self.catalogue.take() {
                match job.await {
                    Ok(catalogue) => {
                        if let Some(dialog) = &mut self.dialog {
                            dialog.catalogue(catalogue);
                        }
                    }
                    Err(_) => {
                        if let Some(dialog) = &mut self.dialog {
                            dialog
                                .error("Model discovery failed. Enter a model ID manually.".into());
                        }
                    }
                }
            }
            outcome.changed = true;
        }
        if self
            .connection
            .as_ref()
            .is_some_and(|(_, job)| job.is_finished())
        {
            if let Some((provider, job)) = self.connection.take() {
                let result = job.await;
                if let Some(dialog) = &mut self.dialog {
                    let action = match result {
                        Ok(Ok(catalogue)) => dialog.setup_catalogue(provider, catalogue),
                        Ok(Err(LocalError::Connect(catalogue::ConnectError::Unavailable))) => {
                            dialog.setup_unavailable(provider);
                            dialog::Action::None
                        }
                        result => {
                            let error = match result {
                                Ok(Err(error)) => error.to_string(),
                                _ => {
                                    "Provider connection failed. Retry or choose another provider."
                                        .into()
                                }
                            };
                            dialog.setup_rejected(provider, error);
                            dialog::Action::None
                        }
                    };
                    self.handle(action);
                }
            }
            outcome.changed = true;
        }
        if self
            .save
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
        {
            if let Some(job) = self.save.take() {
                match job.await {
                    Ok(Ok(Saved::Subscription)) => {
                        outcome.settings_saved = true;
                        chat.notice("ChatGPT account selected. Existing sessions remain pinned; start a new session to use this account.");
                        let action = self
                            .dialog
                            .as_mut()
                            .map(Dialog::subscription_ready)
                            .unwrap_or(dialog::Action::None);
                        self.handle(action);
                    }
                    Ok(Ok(Saved::SignedOut)) => {
                        outcome.settings_saved = true;
                        chat.notice("Signed out locally. ChatGPT registration retained.");
                        if let Some(dialog) = &mut self.dialog {
                            dialog.subscription_error(String::new());
                        }
                    }
                    Ok(Ok(Saved::Link)) => {}
                    Ok(Ok(Saved::AliasChoices)) => {
                        outcome.settings_saved = true;
                        if let Some(dialog) = &mut self.dialog {
                            dialog.alias_choices_saved();
                        }
                    }
                    Ok(Ok(Saved::Setup)) => {
                        outcome.settings_saved = true;
                        if let Some(dialog) = &mut self.dialog {
                            dialog.setup_saved();
                        }
                    }
                    Ok(Ok(Saved::Key)) => {
                        outcome.settings_saved = true;
                        self.dialog = None;
                        chat.notice("Saved credentials updated. Environment, config and the running session are unchanged.");
                    }
                    result => {
                        let oauth_error = matches!(&result, Ok(Err(LocalError::OAuth(_))));
                        let error = match result {
                            Ok(Err(error)) => error.to_string(),
                            _ => "Local command failed.".into(),
                        };
                        match &mut self.dialog {
                            Some(dialog) if oauth_error => dialog.subscription_error(error),
                            Some(dialog) => dialog.error(error),
                            None => chat.notice(&error),
                        }
                    }
                }
            }
            outcome.changed = true;
        }
        outcome
    }
}

impl Drop for Commands {
    fn drop(&mut self) {
        if let Some((cancellation, _)) = &self.mcp_job {
            cancellation.cancel();
        }
        if let Some((cancellation, _)) = &self.oauth {
            cancellation.cancel();
        }
        if let Some(job) = &self.catalogue {
            job.abort();
        }
        if let Some((_, job)) = &self.connection {
            job.abort();
        }
    }
}

struct TerminalModes {
    armed: bool,
    #[cfg(unix)]
    keyboard: bool,
}

impl TerminalModes {
    fn enter(&mut self) -> io::Result<()> {
        install_panic_hook();
        self.armed = true;
        DEFER_PANIC_REPORTS.store(true, Ordering::Release);
        enable_raw_mode()?;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableBracketedPaste,
            EnableMouseCapture,
            EnableFocusChange
        )
    }

    #[cfg(unix)]
    fn enhance_keyboard(&mut self) -> io::Result<()> {
        self.keyboard = true;
        execute!(
            io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )
    }

    fn restore(&mut self) -> io::Result<()> {
        if !self.armed {
            return Ok(());
        }
        let raw = disable_raw_mode();
        #[cfg(unix)]
        let keyboard = if self.keyboard {
            self.keyboard = false;
            execute!(io::stdout(), PopKeyboardEnhancementFlags)
        } else {
            Ok(())
        };
        let screen = execute!(
            io::stdout(),
            DisableBracketedPaste,
            DisableMouseCapture,
            DisableFocusChange,
            LeaveAlternateScreen,
            Show
        );
        self.armed = false;
        DEFER_PANIC_REPORTS.store(false, Ordering::Release);
        let reports = std::mem::take(
            &mut *DEFERRED_PANIC_REPORTS
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        for report in reports {
            eprintln!("{report}");
        }
        #[cfg(unix)]
        let raw = raw.and(keyboard);
        raw.and(screen)
    }
}

impl Drop for TerminalModes {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            eprintln!(
                "cocoa: emergency terminal restoration failed: {}",
                error.kind()
            );
        }
    }
}

static DEFER_PANIC_REPORTS: AtomicBool = AtomicBool::new(false);
static DEFERRED_PANIC_REPORTS: Mutex<Vec<String>> = Mutex::new(Vec::new());
const DEFERRED_PANIC_REPORTS_MAX: usize = 8;

/// Hold panic reports while the session owns the terminal and print them after
/// [`TerminalModes::restore`]. A panic hook cannot tell whether tokio will catch
/// the panic, so restoring the terminal here, as ratatui's hook does, would leave
/// a still-running session drawing in cooked mode on the normal screen. Uncaught
/// panics unwind through `TerminalModes::drop`, which restores and prints.
fn install_panic_hook() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if !DEFER_PANIC_REPORTS.load(Ordering::Acquire) {
                previous(info);
                return;
            }
            let mut reports = DEFERRED_PANIC_REPORTS
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if reports.len() < DEFERRED_PANIC_REPORTS_MAX {
                reports.push(panic_report(info));
            }
        }));
    });
}

fn panic_report(info: &std::panic::PanicHookInfo<'_>) -> String {
    let thread = std::thread::current();
    let name = thread.name().unwrap_or("<unnamed>");
    let message = info.payload_as_str().unwrap_or("Box<dyn Any>");
    let location = info
        .location()
        .map_or_else(String::new, |location| format!(" at {location}"));
    let backtrace = std::backtrace::Backtrace::capture();
    match backtrace.status() {
        std::backtrace::BacktraceStatus::Captured => {
            format!("thread '{name}' panicked{location}:\n{message}\nstack backtrace:\n{backtrace}")
        }
        _ => format!("thread '{name}' panicked{location}:\n{message}"),
    }
}
