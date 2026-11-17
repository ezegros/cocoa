//! Bounded asynchronous Git metadata for the composer border.

use std::{
    path::Path,
    process::{ExitStatus, Stdio},
    time::Duration,
};

use tokio::{io::AsyncReadExt, process::Command};

/// Suggested interval between reads. Callers should keep at most one read in flight.
pub const GIT_REFRESH_INTERVAL: Duration = Duration::from_secs(3);

const READ_TIMEOUT: Duration = Duration::from_secs(2);
const OUTPUT_BYTES_MAX: u64 = 1024 * 1024;
const BRANCH_CHARACTERS_MAX: usize = 80;

/// Repository-wide tracked changes from HEAD to the working tree, not the index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitStatus {
    /// Branch name, or `detached@<short commit>`, sanitized for terminal display.
    pub branch: String,
    /// Added and deleted text lines, excluding untracked files. `None` means
    /// only binary files changed, so Git cannot report line counts. A clean
    /// working tree returns `Some((0, 0))`. Mixed changes count text files only.
    pub changes: Option<(u64, u64)>,
}

/// Failures reading Git metadata. Git's stderr is never exposed to the terminal.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("cannot run Git: {0}")]
    Io(#[from] std::io::Error),
    #[error("Git status exceeded its two-second deadline")]
    Timeout,
    #[error("Git output exceeded its size limit")]
    OutputLimit,
    #[error("Git {operation} failed with exit code {code:?}")]
    Git {
        operation: &'static str,
        code: Option<i32>,
    },
    #[error("Git returned invalid status output")]
    InvalidOutput,
}

/// Read Git metadata without blocking the executor or modifying the index.
///
/// Returns `None` outside working-tree repositories, including bare repositories.
/// Counts staged and unstaged changes once by comparing HEAD directly to the
/// working tree. An unborn branch uses an empty tree as its baseline. Reads from
/// subdirectories still cover the whole repository. Renames count as a deletion
/// and an addition; binary files and submodule contents do not add line counts.
/// Branch labels contain at most 80 characters, with terminal controls replaced.
///
/// Run this in a background task at [`GIT_REFRESH_INTERVAL`], with at most one
/// read in flight. Dropping the future kills its active Git child and, on Unix,
/// its process group. No subprocess waits belong in rendering or the regular UI
/// tick. Errors mean unavailable status, never zero changes.
///
/// # Errors
/// Returns an error for process I/O, malformed or oversized output, failed Git
/// queries after repository discovery, or a two-second total execution timeout.
pub async fn read_git_status(directory: &Path) -> Result<Option<GitStatus>, Error> {
    tokio::time::timeout(READ_TIMEOUT, read_status(directory))
        .await
        .map_err(|_| Error::Timeout)?
}

async fn read_status(directory: &Path) -> Result<Option<GitStatus>, Error> {
    let (status, output) = git(directory, &["rev-parse", "--is-inside-work-tree"]).await?;
    if !status.success() || output != b"true\n" {
        return Ok(None);
    }

    let (status, head) = git(
        directory,
        &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"],
    )
    .await?;
    let baseline = if status.success() {
        reference(&head)?.to_owned()
    } else if status.code() == Some(1) {
        let (status, empty) = git(directory, &["hash-object", "-t", "tree", "--stdin"]).await?;
        success(status, "empty tree")?;
        reference(&empty)?.to_owned()
    } else {
        success(status, "HEAD")?;
        return Err(Error::InvalidOutput);
    };

    let (status, branch) = git(directory, &["symbolic-ref", "--quiet", "--short", "HEAD"]).await?;
    let branch = if status.success() {
        let branch = branch
            .strip_suffix(b"\n")
            .filter(|branch| !branch.is_empty())
            .ok_or(Error::InvalidOutput)?;
        sanitize_branch(&String::from_utf8_lossy(branch))
    } else if status.code() == Some(1) {
        let commit = reference(&head)?;
        if commit.len() < 12 || !commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(Error::InvalidOutput);
        }
        format!("detached@{}", &commit[..12])
    } else {
        success(status, "branch")?;
        return Err(Error::InvalidOutput);
    };

    let (status, output) = git(
        directory,
        &[
            "diff",
            "--numstat",
            "-z",
            "--no-renames",
            "--no-relative",
            "--no-ext-diff",
            "--no-textconv",
            "--ignore-submodules=all",
            &baseline,
            "--",
        ],
    )
    .await?;
    success(status, "diff")?;
    let changes = line_counts(&output)?;
    Ok(Some(GitStatus { branch, changes }))
}

async fn git(directory: &Path, arguments: &[&str]) -> Result<(ExitStatus, Vec<u8>), Error> {
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

fn reference(output: &[u8]) -> Result<&str, Error> {
    let text = std::str::from_utf8(output).map_err(|_| Error::InvalidOutput)?;
    let text = text.strip_suffix('\n').ok_or(Error::InvalidOutput)?;
    if text.is_empty() {
        return Err(Error::InvalidOutput);
    }
    Ok(text)
}

fn sanitize_branch(branch: &str) -> String {
    branch
        .chars()
        .take(BRANCH_CHARACTERS_MAX)
        .map(|character| {
            if character.is_alphanumeric() || character.is_ascii_graphic() {
                character
            } else {
                '?'
            }
        })
        .collect()
}

fn line_counts(output: &[u8]) -> Result<Option<(u64, u64)>, Error> {
    if !output.is_empty() && !output.ends_with(&[0]) {
        return Err(Error::InvalidOutput);
    }
    let mut additions = 0_u64;
    let mut deletions = 0_u64;
    let mut has_text_changes = output.is_empty();
    for record in output.split_inclusive(|byte| *byte == 0) {
        let record = record.strip_suffix(&[0]).ok_or(Error::InvalidOutput)?;
        let mut fields = record.splitn(3, |byte| *byte == b'\t');
        let added = fields.next().ok_or(Error::InvalidOutput)?;
        let deleted = fields.next().ok_or(Error::InvalidOutput)?;
        if fields.next().is_none_or(<[u8]>::is_empty) {
            return Err(Error::InvalidOutput);
        }
        if added == b"-" && deleted == b"-" {
            continue;
        }
        has_text_changes = true;
        let added: u64 = std::str::from_utf8(added)
            .map_err(|_| Error::InvalidOutput)?
            .parse()
            .map_err(|_| Error::InvalidOutput)?;
        let deleted: u64 = std::str::from_utf8(deleted)
            .map_err(|_| Error::InvalidOutput)?
            .parse()
            .map_err(|_| Error::InvalidOutput)?;
        additions = additions.checked_add(added).ok_or(Error::InvalidOutput)?;
        deletions = deletions.checked_add(deleted).ok_or(Error::InvalidOutput)?;
    }
    Ok(has_text_changes.then_some((additions, deletions)))
}
