//! Stable sidecar locks; lock files are never unlinked or replaced.

use std::{
    fs::{File, OpenOptions, TryLockError},
    path::{Path, PathBuf},
};

use super::{PersistenceError, WAIT};

#[derive(Clone, Copy)]
pub(super) enum FileCreation {
    Allowed,
    Forbidden,
}

pub(super) fn database_path(
    path: &Path,
    creation: FileCreation,
) -> Result<PathBuf, PersistenceError> {
    let name = path
        .file_name()
        .ok_or(PersistenceError::InvalidData("database filename"))?;
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent = parent.canonicalize()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = parent.metadata()?;
        if metadata.mode() & 0o077 != 0 || metadata.uid() != rustix::process::geteuid().as_raw() {
            return Err(PersistenceError::InvalidData(
                "database directory must be owner-only",
            ));
        }
    }
    let path = parent.join(name);
    drop(private_file(&path, creation)?);
    Ok(path)
}

pub(super) async fn schema(path: &Path, mode: Mode) -> Result<File, PersistenceError> {
    let path = sidecar(path, ".schema.lock");
    acquire(path, mode).await.map_err(|error| match error {
        PersistenceError::Busy => PersistenceError::MigrationBlocked,
        error => error,
    })
}

pub(super) async fn session(path: &Path, public_id: &str) -> Result<File, PersistenceError> {
    let directory = sidecar(path, ".locks");
    let public_id = public_id.to_owned();
    let path = tokio::task::spawn_blocking(move || {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .mode(0o700)
                .recursive(true)
                .create(&directory)?;
        }
        #[cfg(not(unix))]
        std::fs::create_dir_all(&directory)?;
        Ok::<_, PersistenceError>(directory.join(format!("{public_id}.lock")))
    })
    .await??;
    let file =
        tokio::task::spawn_blocking(move || private_file(&path, FileCreation::Allowed)).await??;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(PersistenceError::SessionInUse),
        Err(TryLockError::Error(error)) => Err(error.into()),
    }
}

#[derive(Clone, Copy)]
pub(super) enum Mode {
    Shared,
    Exclusive,
}

async fn acquire(path: PathBuf, mode: Mode) -> Result<File, PersistenceError> {
    let file =
        tokio::task::spawn_blocking(move || private_file(&path, FileCreation::Allowed)).await??;
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let result = match mode {
            Mode::Shared => file.try_lock_shared(),
            Mode::Exclusive => file.try_lock(),
        };
        match result {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            Err(TryLockError::WouldBlock) => return Err(PersistenceError::Busy),
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
    }
}

pub(super) fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    name.into()
}

pub(super) fn private_directory(path: &Path) -> Result<File, PersistenceError> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(
            (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::DIRECTORY).bits() as i32,
        );
    }
    let directory = options.open(path)?;
    let metadata = directory.metadata()?;
    if !metadata.is_dir() || std::fs::symlink_metadata(path)?.is_symlink() {
        return Err(PersistenceError::InvalidData(
            "backup path is not a directory",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.mode() & 0o077 != 0 || metadata.uid() != rustix::process::geteuid().as_raw() {
            return Err(PersistenceError::InvalidData(
                "backup directory must be owner-only",
            ));
        }
    }
    Ok(directory)
}

pub(super) fn private_file(path: &Path, creation: FileCreation) -> Result<File, PersistenceError> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(matches!(creation, FileCreation::Allowed))
        .truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(PersistenceError::InvalidData(
            "database/lock path is not a regular file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.mode() & 0o077 != 0 || metadata.uid() != rustix::process::geteuid().as_raw() {
            return Err(PersistenceError::InvalidData(
                "database/lock file must be owner-only",
            ));
        }
    }
    Ok(file)
}
