//! Owner-only, bounded JSON with a stable cross-process lock and atomic replacement.

use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use serde::{Serialize, de::DeserializeOwned};

use crate::OAuthError;

const STORE_BYTES: usize = 1024 * 1024;

pub(crate) struct LockedStore {
    path: PathBuf,
    _lock: File,
}

impl LockedStore {
    pub async fn acquire(path: &Path) -> Result<Self, OAuthError> {
        prepare(path)?;
        let mut name = path.as_os_str().to_os_string();
        name.push(".lock");
        let lock = open(Path::new(&name), true)?.ok_or(OAuthError::Storage)?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
        loop {
            match lock.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock)
                    if tokio::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(_) => return Err(OAuthError::Busy),
            }
        }
        Ok(Self {
            path: path.into(),
            _lock: lock,
        })
    }

    pub fn read<T: DeserializeOwned>(&self) -> Result<Option<T>, OAuthError> {
        read(&self.path)
    }

    pub fn save<T: Serialize>(&self, value: &T) -> Result<(), OAuthError> {
        let bytes = serde_json::to_vec(value).map_err(|_| OAuthError::Storage)?;
        if bytes.len() > STORE_BYTES {
            return Err(OAuthError::Storage);
        }
        let parent = parent(&self.path)?;
        check_directory(parent)?;
        // Validate the destination again before replacing it, including non-OAuth writers.
        drop(open(&self.path, false)?);
        let mut temporary =
            tempfile::NamedTempFile::new_in(parent).map_err(|_| OAuthError::Storage)?;
        temporary
            .write_all(&bytes)
            .and_then(|()| temporary.as_file().sync_all())
            .map_err(|_| OAuthError::Storage)?;
        temporary
            .persist(&self.path)
            .map_err(|_| OAuthError::Storage)?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| OAuthError::Storage)
    }
}

pub(crate) fn read<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, OAuthError> {
    let Some(file) = open(path, false)? else {
        return Ok(None);
    };
    check_directory(parent(path)?)?;
    let mut bytes = Vec::new();
    file.take((STORE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| OAuthError::Storage)?;
    if bytes.len() > STORE_BYTES {
        return Err(OAuthError::Storage);
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| OAuthError::Storage)
}

fn parent(path: &Path) -> Result<&Path, OAuthError> {
    path.parent()
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or(OAuthError::Storage)
}

#[cfg(unix)]
fn prepare(path: &Path) -> Result<(), OAuthError> {
    use std::os::unix::fs::DirBuilderExt;
    let parent = parent(path)?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)
        .map_err(|_| OAuthError::Storage)?;
    check_directory(parent)
}

#[cfg(not(unix))]
fn prepare(_: &Path) -> Result<(), OAuthError> {
    Err(OAuthError::Storage)
}

#[cfg(unix)]
fn open(path: &Path, create: bool) -> Result<Option<File>, OAuthError> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    match fs::symlink_metadata(path) {
        Ok(metadata) => check_file(&metadata)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if !create {
                return Ok(None);
            }
        }
        Err(_) => return Err(OAuthError::Storage),
    }
    let file = fs::OpenOptions::new()
        .read(true)
        .write(create)
        .create(create)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map_err(|_| OAuthError::Storage)?;
    let opened = file.metadata().map_err(|_| OAuthError::Storage)?;
    check_file(&opened)?;
    let named = fs::symlink_metadata(path).map_err(|_| OAuthError::Storage)?;
    if named.file_type().is_symlink() || opened.ino() != named.ino() || opened.dev() != named.dev()
    {
        return Err(OAuthError::Storage);
    }
    Ok(Some(file))
}

#[cfg(not(unix))]
fn open(_: &Path, _: bool) -> Result<Option<File>, OAuthError> {
    Err(OAuthError::Storage)
}

#[cfg(unix)]
fn check_file(metadata: &fs::Metadata) -> Result<(), OAuthError> {
    use std::os::unix::fs::MetadataExt;
    if !metadata.is_file()
        || metadata.mode() & 0o777 != 0o600
        || metadata.nlink() != 1
        || metadata.uid() != rustix::process::geteuid().as_raw()
    {
        return Err(OAuthError::Storage);
    }
    Ok(())
}

#[cfg(unix)]
fn check_directory(path: &Path) -> Result<(), OAuthError> {
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::symlink_metadata(path).map_err(|_| OAuthError::Storage)?;
    if !metadata.is_dir()
        || metadata.mode() & 0o077 != 0
        || metadata.uid() != rustix::process::geteuid().as_raw()
    {
        return Err(OAuthError::Storage);
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_directory(_: &Path) -> Result<(), OAuthError> {
    Err(OAuthError::Storage)
}
