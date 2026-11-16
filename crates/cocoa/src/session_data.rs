//! Shared session-data location and terminal-owned session creation.

use std::path::{Path, PathBuf};

use cocoa::persistence::{PersistenceError, SessionAttachment, SessionDatabase};

pub(crate) fn database_path(data_directory: Option<&Path>) -> Result<PathBuf, SessionDataError> {
    let directory = match data_directory {
        Some(directory) => directory.to_owned(),
        None => default_directory()?,
    };
    Ok(directory.join("sessions.sqlite3"))
}

pub(crate) async fn create(
    data_directory: Option<&Path>,
    working_directory: &Path,
) -> Result<SessionAttachment, SessionDataError> {
    let path = database_path(data_directory)?;
    let parent = path
        .parent()
        .ok_or(PersistenceError::InvalidData("session data directory"))?
        .to_owned();
    tokio::task::spawn_blocking(move || {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(parent)
    })
    .await
    .map_err(PersistenceError::Task)?
    .map_err(PersistenceError::Io)?;
    let database = SessionDatabase::open(&path).await?;
    Ok(database.create_session(working_directory).await?)
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum SessionDataError {
    #[error("cannot determine session data directory; set --data-dir or XDG_DATA_HOME")]
    DataDirectory,
    #[error(transparent)]
    Persistence(#[from] PersistenceError),
}

fn default_directory() -> Result<PathBuf, SessionDataError> {
    if let Some(directory) = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    {
        return Ok(directory.join("cocoa"));
    }
    std::env::home_dir()
        .map(|home| home.join(".local/share/cocoa"))
        .ok_or(SessionDataError::DataDirectory)
}
