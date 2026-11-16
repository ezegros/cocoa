//! Persistent terminal agent with matching local CLI and slash commands.

mod agent_factory;
mod clipboard;
mod dialog;
mod exec;
mod mcp_command;
mod session;
mod session_command;
mod session_data;
mod status;
mod tui;

use cocoa::{auth, catalogue, config};
use session::run_description;
use std::{io::IsTerminal, process::ExitCode};

fn main() -> ExitCode {
    match command() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("cocoa: {error}");
            ExitCode::FAILURE
        }
    }
}

fn command() -> Result<ExitCode, CommandError> {
    let Some(arguments) = config::arguments()? else {
        return Ok(ExitCode::SUCCESS);
    };
    let mut resume = None;
    let menu = match arguments.command {
        Some(config::Command::Update(options)) => {
            let updater = cocoa::update::Updater::current()?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(CommandError::Runtime)?;
            return runtime.block_on(async {
                use tokio::signal::unix::{SignalKind, signal};
                let mut interrupt = signal(SignalKind::interrupt()).map_err(CommandError::Runtime)?;
                let mut terminate = signal(SignalKind::terminate()).map_err(CommandError::Runtime)?;
                tokio::select! {
                    result = updater.run(options.into(), |message| eprintln!("{message}")) => {
                        println!("{}", result?);
                        Ok(ExitCode::SUCCESS)
                    }
                    _ = interrupt.recv() => {
                        eprintln!("Cocoa update cancelled. A replacement already being published may finish; check again before retrying.");
                        Ok(ExitCode::from(130))
                    }
                    _ = terminate.recv() => {
                        eprintln!("Cocoa update cancelled. A replacement already being published may finish; check again before retrying.");
                        Ok(ExitCode::from(143))
                    }
                }
            });
        }
        Some(config::Command::Exec {
            instruction,
            export,
        }) => {
            return exec::run(
                instruction,
                config::Selection {
                    alias: arguments.alias,
                    model: arguments.model,
                    thinking: arguments.thinking.and_then(|choice| choice.0),
                    choice: None,
                },
                arguments.data_dir,
                export,
            )
            .map_err(CommandError::ExecOutput);
        }
        Some(config::Command::Resume(command)) => {
            if arguments.alias.is_some() || arguments.model.is_some() {
                return Err(config::ConfigError::SessionCompatibility.into());
            }
            resume = Some(command);
            None
        }
        Some(config::Command::Sessions { command }) => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(CommandError::Runtime)?;
            return runtime
                .block_on(session_command::run(command, arguments.data_dir))
                .map(|()| ExitCode::SUCCESS)
                .map_err(CommandError::Sessions);
        }
        Some(config::Command::Search { provider }) => {
            config::save_search_provider(provider)?;
            eprintln!("Saved search provider; applies to new sessions.");
            return Ok(ExitCode::SUCCESS);
        }
        Some(config::Command::Mcp {
            command: Some(command),
        }) => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(CommandError::Runtime)?;
            return runtime.block_on(async {
                let result = mcp_command::run(command).await;
                cocoa_oauth::finish_refreshes().await;
                result
                    .map(|()| ExitCode::SUCCESS)
                    .map_err(CommandError::Config)
            });
        }
        Some(config::Command::Mcp { command: None }) => Some("/mcp"),
        Some(config::Command::Login { provider }) => {
            auth::run(auth::AuthCommand::Set {
                provider,
                stdin: !std::io::stdin().is_terminal(),
            })?;
            return Ok(ExitCode::SUCCESS);
        }
        Some(config::Command::Logout { provider }) => {
            auth::run(auth::AuthCommand::Remove { provider })?;
            return Ok(ExitCode::SUCCESS);
        }
        Some(config::Command::Alias {
            name,
            provider,
            model,
            thinking,
        }) => {
            config::save_alias(name, provider, model, thinking.map(|choice| choice.0))?;
            eprintln!("Saved alias; applies to new sessions.");
            return Ok(ExitCode::SUCCESS);
        }
        Some(config::Command::Aliases) => Some("/aliases"),
        Some(config::Command::Models) => Some("/models"),
        None => None,
    };
    if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        return Err(CommandError::NotTerminal);
    }
    let mut presentation = match config::presentation() {
        Ok(presentation) => presentation,
        Err(error) if resume.is_some() => {
            tracing::debug!(%error, "using default presentation for configuration-independent resume");
            config::Presentation::default()
        }
        Err(error) => return Err(error.into()),
    };
    presentation.theme = arguments.theme.unwrap_or(presentation.theme);
    presentation.reduced_motion |= arguments.reduced_motion;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(CommandError::Runtime)?;
    runtime.block_on(async {
        let result = tui::run(
            arguments.prompt,
            config::Selection {
                alias: arguments.alias,
                model: arguments.model,
                thinking: arguments.thinking.and_then(|choice| choice.0),
                choice: None,
            },
            menu,
            arguments.data_dir,
            resume,
            presentation,
        )
        .await;
        cocoa_oauth::finish_refreshes().await;
        result
            .map(|()| ExitCode::SUCCESS)
            .map_err(CommandError::Terminal)
    })
}

#[derive(Debug, thiserror::Error)]
enum CommandError {
    #[error(transparent)]
    Update(#[from] cocoa::update::Error),
    #[error(transparent)]
    Config(#[from] config::ConfigError),
    #[error(transparent)]
    Auth(#[from] auth::AuthError),
    #[error(transparent)]
    Sessions(#[from] session_command::SessionCommandError),
    #[error("interactive sessions require a terminal on stdin and stdout")]
    NotTerminal,
    #[error("cannot initialize runtime")]
    Runtime(#[source] std::io::Error),
    #[error("cannot write execution outcome")]
    ExecOutput(#[source] std::io::Error),
    #[error("terminal session failed: {0}")]
    Terminal(#[source] tui::TuiError),
}
