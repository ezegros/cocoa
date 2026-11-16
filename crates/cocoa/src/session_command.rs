//! Retained session discovery and export without configuration or execution.

use std::{
    fmt,
    io::{self, Write},
    path::PathBuf,
};

use cocoa::persistence::{ExportLimits, PersistenceError, SessionDatabase, SessionPage};

use crate::{
    config::SessionCommand,
    session_data::{self, SessionDataError},
};

pub(crate) async fn run(
    command: SessionCommand,
    data_directory: Option<PathBuf>,
) -> Result<(), SessionCommandError> {
    let database_path = session_data::database_path(data_directory.as_deref())?;
    if let SessionCommand::Export { reference, json } = command {
        let database = SessionDatabase::open_existing(&database_path)
            .await?
            .ok_or(PersistenceError::SessionNotFound)?;
        let attachment = database
            .attach_session(
                &reference,
                cocoa_core::store::StoreLimits {
                    record_bytes: 2 * cocoa_llm::DECODER_BYTES + 4096,
                },
            )
            .await?;
        let export = attachment.export_run(ExportLimits::default()).await?;
        let mut output = io::stdout().lock();
        if json {
            serde_json::to_writer(&mut output, &export)?;
        } else {
            serde_json::to_writer_pretty(&mut output, &export)?;
        }
        writeln!(output)?;
        return Ok(());
    }
    let (all, limit, json) = match &command {
        SessionCommand::List {
            all, limit, json, ..
        }
        | SessionCommand::Search {
            all, limit, json, ..
        } => (*all, *limit, *json),
        SessionCommand::Export { .. } => unreachable!("export returned above"),
    };
    let directory = if all {
        None
    } else {
        Some(std::env::current_dir()?.canonicalize()?)
    };
    if let SessionCommand::Search { query, .. } = command {
        let matches = match SessionDatabase::open_existing(&database_path).await? {
            Some(database) => {
                database
                    .search_sessions(directory.as_deref(), &query, limit)
                    .await?
            }
            None => cocoa::persistence::SessionSearch {
                sessions: Vec::new(),
                limit,
            },
        };
        let mut output = io::stdout().lock();
        if json {
            serde_json::to_writer(&mut output, &matches)?;
            writeln!(output)?;
        } else if matches.sessions.is_empty() {
            writeln!(output, "No sessions found.")?;
        } else {
            writeln!(output, "SESSION\tTITLE\tDIRECTORY\tUPDATED_MS\tMATCH")?;
            for matched in &matches.sessions {
                let session = &matched.session;
                writeln!(
                    output,
                    "{}\t{}\t{}\t{}\t{}",
                    session.public_id,
                    TerminalText(session.title.as_deref().unwrap_or("Untitled session")),
                    TerminalText(&session.working_directory.to_string_lossy()),
                    session.updated_at_ms,
                    TerminalText(&matched.snippet)
                )?;
            }
            writeln!(
                io::stderr().lock(),
                "Search returns at most {limit} sessions; refine the query for more specific results."
            )?;
        }
        return Ok(());
    }
    let SessionCommand::List { before, .. } = command else {
        unreachable!("search returned above");
    };
    let page = match SessionDatabase::open_existing(&database_path).await? {
        Some(database) => {
            database
                .list_sessions(directory.as_deref(), limit, before)
                .await?
        }
        None => SessionPage {
            sessions: Vec::new(),
            next_cursor: None,
        },
    };
    let mut output = io::stdout().lock();
    if json {
        serde_json::to_writer(&mut output, &page)?;
        writeln!(output)?;
    } else if page.sessions.is_empty() {
        writeln!(output, "No sessions found.")?;
    } else {
        writeln!(output, "SESSION\tTITLE\tDIRECTORY\tUPDATED_MS")?;
        for session in &page.sessions {
            let directory = session
                .working_directory
                .to_str()
                .ok_or(PersistenceError::InvalidData("session working directory"))?;
            writeln!(
                output,
                "{}\t{}\t{}\t{}",
                session.public_id,
                TerminalText(session.title.as_deref().unwrap_or("Untitled session")),
                TerminalText(directory),
                session.updated_at_ms,
            )?;
        }
        if let Some(cursor) = page.next_cursor {
            writeln!(io::stderr().lock(), "Next cursor: {cursor}")?;
        }
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum SessionCommandError {
    #[error(transparent)]
    Data(#[from] SessionDataError),
    #[error("session command filesystem or output operation failed: {0}")]
    Io(#[from] io::Error),
    #[error("cannot encode session metadata: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Persistence(#[from] PersistenceError),
}

struct TerminalText<'a>(&'a str);

impl fmt::Display for TerminalText<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for character in self.0.chars() {
            if character.is_control()
                || matches!(character, '\\' | '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
            {
                write!(formatter, "{}", character.escape_default())?;
            } else {
                write!(formatter, "{character}")?;
            }
        }
        Ok(())
    }
}
