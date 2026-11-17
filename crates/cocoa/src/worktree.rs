//! Create clean, named Git worktrees in Cocoa's per-repository directory.

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    time::Duration,
};

use sha2::{Digest, Sha256};
use tokio::{io::AsyncReadExt, process::Command};

const CREATE_TIMEOUT: Duration = Duration::from_secs(30);
const OUTPUT_BYTES_MAX: u64 = 1024 * 1024;

/// Failures validating or creating a worktree. Git stderr is not exposed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("worktree name must be a single safe path component and a valid Git branch name")]
    InvalidName,
    #[error("directory is not a non-bare Git working tree")]
    NotWorkingTree,
    #[error("repository HEAD must refer to a commit")]
    UnbornHead,
    #[error("branch already exists")]
    BranchExists,
    #[error("worktree destination already exists: {0}")]
    DestinationExists(PathBuf),
    #[error("worktree creation exceeded its thirty-second deadline")]
    Timeout,
    #[error("Git output exceeded its size limit")]
    OutputLimit,
    #[error("Git returned invalid output")]
    InvalidOutput,
    #[error("Git {operation} failed with exit code {code:?}")]
    Git {
        /// The failed Git operation.
        operation: &'static str,
        /// Process exit code, absent when terminated by a signal.
        code: Option<i32>,
    },
    #[error("worktree I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

/// Create a worktree and a branch named exactly as supplied, from source HEAD.
///
/// The destination is home/.config/cocoa/worktrees/<repository-id>/<name>.
/// The repository ID combines a readable basename and the full SHA-256 digest
/// of the canonical Git common directory. Linked trees share an ID, while
/// repositories with identical basenames remain separate. Subdirectories and
/// detached HEADs are supported. Dirty and untracked files are never copied.
/// Names must be nonempty single components, at most 128 bytes, with no leading
/// dash, separators, control characters, or invalid Git branch syntax. The
/// reserved branch name `HEAD` is rejected.
///
/// Git uses no shell, captures at most 1 MiB per command, and has a thirty-second
/// total deadline. Cancellation kills the active child and its Unix process
/// group. Failure or cancellation during Git's mutation may leave Git-created
/// state behind; callers should inspect it before retrying.
///
/// # Errors
/// Rejects invalid names, non-repositories, bare repositories, unborn HEADs,
/// existing branches or destinations, failed Git commands, I/O failures, and
/// output or time limits. Existing branches and paths are never overwritten.
pub async fn create(directory: &Path, home: &Path, name: &str) -> Result<PathBuf, Error> {
    tokio::time::timeout(CREATE_TIMEOUT, create_inner(directory, home, name))
        .await
        .map_err(|_| Error::Timeout)?
}

async fn create_inner(directory: &Path, home: &Path, name: &str) -> Result<PathBuf, Error> {
    if name.is_empty()
        || name.len() > 128
        || name.starts_with('-')
        || name == "."
        || name == ".."
        || name == "HEAD"
        || name.contains(['/', '\\'])
        || name.chars().any(char::is_control)
    {
        return Err(Error::InvalidName);
    }
    let reference = format!("refs/heads/{name}");
    let (status, _) = git(
        directory,
        &["check-ref-format".as_ref(), reference.as_ref()],
    )
    .await?;
    if status.code() == Some(1) {
        return Err(Error::InvalidName);
    }
    success(status, "name validation")?;
    let (status, output) = git(
        directory,
        &["rev-parse".as_ref(), "--is-inside-work-tree".as_ref()],
    )
    .await?;
    if !status.success() || output != b"true\n" {
        return Err(Error::NotWorkingTree);
    }
    let (status, head) = git(
        directory,
        &[
            "rev-parse".as_ref(),
            "--verify".as_ref(),
            "--quiet".as_ref(),
            "HEAD^{commit}".as_ref(),
        ],
    )
    .await?;
    if !status.success() {
        return Err(Error::UnbornHead);
    }
    let head = std::str::from_utf8(line(&head)?).map_err(|_| Error::InvalidOutput)?;
    if !matches!(head.len(), 40 | 64) || !head.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(Error::InvalidOutput);
    }
    let (status, _) = git(
        directory,
        &[
            "show-ref".as_ref(),
            "--verify".as_ref(),
            "--quiet".as_ref(),
            reference.as_ref(),
        ],
    )
    .await?;
    if status.success() {
        return Err(Error::BranchExists);
    }
    if status.code() != Some(1) {
        success(status, "branch lookup")?;
    }
    let (status, common) = git(
        directory,
        &[
            "rev-parse".as_ref(),
            "--path-format=absolute".as_ref(),
            "--git-common-dir".as_ref(),
        ],
    )
    .await?;
    success(status, "common directory")?;
    let common = tokio::fs::canonicalize(path_from_output(line(&common)?)?).await?;
    let repository = if common.file_name() == Some(OsStr::new(".git")) {
        common.parent().ok_or(Error::InvalidOutput)?
    } else {
        common.as_path()
    };
    let basename = repository
        .file_name()
        .ok_or(Error::InvalidOutput)?
        .to_string_lossy();
    let mut readable: String = basename
        .chars()
        .take(48)
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect();
    if readable.is_empty() {
        readable.push_str("repository");
    }
    let digest = Sha256::digest(common.as_os_str().as_encoded_bytes());
    let digest: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    let repository_id = format!("{readable}-{digest}");
    let parent = home.join(".config/cocoa/worktrees").join(repository_id);
    tokio::fs::create_dir_all(&parent).await?;
    let parent = tokio::fs::canonicalize(parent).await?;
    let destination = parent.join(name);
    match tokio::fs::symlink_metadata(&destination).await {
        Ok(_) => return Err(Error::DestinationExists(destination)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let (status, _) = git(
        directory,
        &[
            "-c".as_ref(),
            "core.hooksPath=/dev/null".as_ref(),
            "worktree".as_ref(),
            "add".as_ref(),
            "-b".as_ref(),
            name.as_ref(),
            "--".as_ref(),
            destination.as_os_str(),
            head.as_ref(),
        ],
    )
    .await?;
    success(status, "worktree add")?;
    Ok(destination)
}

fn line(output: &[u8]) -> Result<&[u8], Error> {
    output
        .strip_suffix(b"\n")
        .filter(|line| !line.is_empty())
        .ok_or(Error::InvalidOutput)
}

fn path_from_output(output: &[u8]) -> Result<PathBuf, Error> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Ok(PathBuf::from(std::ffi::OsString::from_vec(output.to_vec())))
    }
    #[cfg(not(unix))]
    {
        Ok(PathBuf::from(
            std::str::from_utf8(output).map_err(|_| Error::InvalidOutput)?,
        ))
    }
}

async fn git(directory: &Path, arguments: &[&OsStr]) -> Result<(ExitStatus, Vec<u8>), Error> {
    let mut command = Command::new("git");
    command
        .args([
            "--no-pager",
            "--no-optional-locks",
            "-c",
            "core.fsmonitor=false",
        ])
        .args(arguments)
        .current_dir(directory)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn()?;
    #[cfg(unix)]
    let group = GitProcessGroup {
        process: child
            .id()
            .and_then(|id| i32::try_from(id).ok())
            .and_then(rustix::process::Pid::from_raw)
            .ok_or(Error::InvalidOutput)?,
    };
    let stdout = child.stdout.take().ok_or(Error::InvalidOutput)?;
    let mut output = Vec::new();
    stdout
        .take(OUTPUT_BYTES_MAX + 1)
        .read_to_end(&mut output)
        .await?;
    if output.len() as u64 > OUTPUT_BYTES_MAX {
        return Err(Error::OutputLimit);
    }
    #[cfg(unix)]
    {
        use rustix::process::{WaitId, WaitIdOptions, waitid};

        // Keep the leader unreaped until group cleanup, so its PID cannot be reused.
        while waitid(
            WaitId::Pid(group.process),
            WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG,
        )
        .map_err(std::io::Error::from)?
        .is_none()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        drop(group);
    }
    let status = child.wait().await?;
    Ok((status, output))
}

#[cfg(unix)]
struct GitProcessGroup {
    process: rustix::process::Pid,
}

#[cfg(unix)]
impl Drop for GitProcessGroup {
    fn drop(&mut self) {
        use rustix::{
            io::Errno,
            process::{Signal, kill_process_group},
        };

        match kill_process_group(self.process, Signal::KILL) {
            Ok(()) | Err(Errno::SRCH) => {}
            Err(Errno::PERM) if cfg!(target_os = "macos") => {}
            Err(error) => tracing::warn!(%error, "cannot stop Git process group"),
        }
    }
}

fn success(status: ExitStatus, operation: &'static str) -> Result<(), Error> {
    if status.success() {
        Ok(())
    } else {
        Err(Error::Git {
            operation,
            code: status.code(),
        })
    }
}
