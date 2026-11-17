//! Git status reads against disposable repositories and bounded process fixtures.

#[path = "support/process.rs"]
mod process;

use std::{
    error::Error,
    fs,
    path::Path,
    process::{Command, Output},
    time::{Duration, Instant},
};

use cocoa::workspace::{Error as WorkspaceError, GitStatus, read_git_status};
use process::CommandExt;

#[tokio::test]
async fn clean_and_untracked_files_have_zero_counts() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    assert_eq!(
        status(repository.path()).await?,
        GitStatus {
            branch: "main".to_owned(),
            changes: Some((0, 0)),
        }
    );
    fs::write(repository.path().join("untracked"), "not counted\n")?;
    assert_eq!(status(repository.path()).await?.changes, Some((0, 0)));
    Ok(())
}

#[tokio::test]
async fn staged_and_unstaged_changes_are_compared_once_against_head() -> Result<(), Box<dyn Error>>
{
    let repository = repository()?;
    let file = repository.path().join("tracked");
    fs::write(&file, "base\nstaged\n")?;
    git(repository.path(), &["add", "tracked"])?;
    fs::write(&file, "base\nstaged\nunstaged\n")?;
    assert_eq!(status(repository.path()).await?.changes, Some((2, 0)));

    fs::write(&file, "base\n")?;
    assert_eq!(status(repository.path()).await?.changes, Some((0, 0)));

    fs::write(&file, "replacement\n")?;
    git(repository.path(), &["add", "tracked"])?;
    assert_eq!(status(repository.path()).await?.changes, Some((1, 1)));

    fs::remove_file(&file)?;
    assert_eq!(status(repository.path()).await?.changes, Some((0, 1)));
    Ok(())
}

#[tokio::test]
async fn new_tracked_files_and_odd_paths_use_current_working_tree_content()
-> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    let name = "new\tfile\nname";
    fs::write(repository.path().join(name), "staged\n")?;
    git(repository.path(), &["add", "--", name])?;
    fs::write(repository.path().join(name), "working\ntree\n")?;
    assert_eq!(status(repository.path()).await?.changes, Some((2, 0)));
    fs::remove_file(repository.path().join(name))?;
    assert_eq!(status(repository.path()).await?.changes, Some((0, 0)));
    Ok(())
}

#[tokio::test]
async fn subdirectories_and_linked_worktrees_read_their_repository() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    let subdirectory = repository.path().join("nested");
    fs::create_dir(&subdirectory)?;
    fs::write(repository.path().join("tracked"), "base\noutside nested\n")?;
    git(repository.path(), &["config", "diff.relative", "true"])?;
    assert_eq!(status(&subdirectory).await?.changes, Some((1, 0)));

    let destination = tempfile::tempdir()?;
    let linked = destination.path().join("linked");
    let linked_text = linked.to_str().ok_or("non-UTF-8 temporary path")?;
    git(
        repository.path(),
        &["worktree", "add", "-b", "linked", linked_text],
    )?;
    fs::write(linked.join("tracked"), "base\nlinked\n")?;
    let linked_status = status(&linked).await?;
    assert_eq!(linked_status.branch, "linked");
    assert_eq!(linked_status.changes, Some((1, 0)));
    Ok(())
}

#[tokio::test]
async fn unborn_repositories_count_tracked_working_tree_content() -> Result<(), Box<dyn Error>> {
    let repository = tempfile::tempdir()?;
    git(repository.path(), &["init", "-b", "new-branch"])?;
    let initial = status(repository.path()).await?;
    assert_eq!(initial.branch, "new-branch");
    assert_eq!(initial.changes, Some((0, 0)));

    fs::write(repository.path().join("tracked"), "staged\n")?;
    git(repository.path(), &["add", "tracked"])?;
    fs::write(repository.path().join("tracked"), "current\ncontent\n")?;
    assert_eq!(status(repository.path()).await?.changes, Some((2, 0)));
    Ok(())
}

#[tokio::test]
async fn detached_head_has_a_commit_label() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    let commit = git(repository.path(), &["rev-parse", "HEAD"])?;
    let commit = std::str::from_utf8(&commit.stdout)?.trim();
    git(repository.path(), &["checkout", "--detach"])?;
    assert_eq!(
        status(repository.path()).await?.branch,
        format!("detached@{}", &commit[..12])
    );
    Ok(())
}

#[tokio::test]
async fn binary_only_changes_are_unknown_while_mixed_changes_count_text()
-> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    fs::write(repository.path().join("binary"), b"\0old")?;
    git(repository.path(), &["add", "binary"])?;
    git(repository.path(), &["commit", "-m", "binary"])?;
    fs::write(repository.path().join("binary"), b"\0new")?;
    assert_eq!(status(repository.path()).await?.changes, None);
    fs::write(repository.path().join("tracked"), "base\ntext\n")?;
    assert_eq!(status(repository.path()).await?.changes, Some((1, 0)));
    Ok(())
}

#[tokio::test]
async fn absent_and_bare_repositories_have_no_status() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    assert_eq!(read_git_status(directory.path()).await?, None);
    git(directory.path(), &["init", "--bare"])?;
    assert_eq!(read_git_status(directory.path()).await?, None);
    assert!(matches!(
        read_git_status(&directory.path().join("missing")).await,
        Err(WorkspaceError::Io(_))
    ));
    Ok(())
}

#[tokio::test]
async fn branch_names_are_bounded_and_strip_terminal_direction_controls()
-> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    git(
        repository.path(),
        &["checkout", "-b", "feature/abc\u{202e}def"],
    )?;
    assert_eq!(status(repository.path()).await?.branch, "feature/abc?def");
    git(repository.path(), &["checkout", "-b", &"x".repeat(120)])?;
    assert_eq!(status(repository.path()).await?.branch, "x".repeat(80));
    #[cfg(unix)]
    {
        use std::{ffi::OsStr, os::unix::ffi::OsStrExt};

        let output = Command::new("git")
            .args(["symbolic-ref", "HEAD"])
            .arg(OsStr::from_bytes(b"refs/heads/feature/\xff"))
            .current_dir(repository.path())
            .output_bounded()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        fs::write(repository.path().join("tracked"), "base\nextra\n")?;
        let current = status(repository.path()).await?;
        assert_eq!(current.branch, "feature/?");
        assert_eq!(current.changes, Some((2, 0)));
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn subprocess_limits_and_failures_are_not_reported_as_clean() -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::PermissionsExt;

    for (mode, script) in [
        ("timeout", "exec /bin/sleep 20"),
        (
            "descendant",
            "(/bin/sleep 3; printf escaped > \"$COCOA_WORKSPACE_DIRECTORY/escaped\") & wait",
        ),
        ("output", "while :; do printf '%02000d' 0; done"),
        (
            "failure",
            "case \"$*\" in *--is-inside-work-tree*) printf 'true\\n';; *) exit 7;; esac",
        ),
        (
            "invalid",
            "case \"$*\" in *--is-inside-work-tree*) printf 'true\\n';; *--numstat*) printf 'invalid\\0';; *) printf 'main\\n';; esac",
        ),
    ] {
        let directory = tempfile::tempdir()?;
        let executable = directory.path().join("git");
        fs::write(&executable, format!("#!/bin/sh\n{script}\n"))?;
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))?;
        let output = Command::new(std::env::current_exe()?)
            .args(["--exact", "reader_process_fixture", "--nocapture"])
            .env("PATH", directory.path())
            .env("COCOA_WORKSPACE_FIXTURE", mode)
            .env("COCOA_WORKSPACE_DIRECTORY", directory.path())
            .output_bounded()?;
        assert!(
            output.status.success(),
            "{mode}: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

#[tokio::test]
async fn reader_process_fixture() -> Result<(), Box<dyn Error>> {
    let Ok(mode) = std::env::var("COCOA_WORKSPACE_FIXTURE") else {
        return Ok(());
    };
    let directory = std::env::var("COCOA_WORKSPACE_DIRECTORY")?;
    let started = Instant::now();
    let result = read_git_status(Path::new(&directory)).await;
    match mode.as_str() {
        "timeout" | "descendant" => {
            assert!(matches!(result, Err(WorkspaceError::Timeout)));
            assert!(started.elapsed() < Duration::from_secs(5));
            if mode == "descendant" {
                tokio::time::sleep(Duration::from_millis(1400)).await;
                assert!(!Path::new(&directory).join("escaped").exists());
            }
        }
        "output" => assert!(matches!(result, Err(WorkspaceError::OutputLimit))),
        "failure" => assert!(matches!(result, Err(WorkspaceError::Git { .. }))),
        "invalid" => assert!(matches!(result, Err(WorkspaceError::InvalidOutput))),
        _ => return Err("unknown fixture mode".into()),
    }
    Ok(())
}

async fn status(directory: &Path) -> Result<GitStatus, Box<dyn Error>> {
    read_git_status(directory)
        .await?
        .ok_or_else(|| "expected repository status".into())
}

fn repository() -> Result<tempfile::TempDir, Box<dyn Error>> {
    let repository = tempfile::tempdir()?;
    git(repository.path(), &["init", "-b", "main"])?;
    git(
        repository.path(),
        &["config", "user.email", "test@example.com"],
    )?;
    git(repository.path(), &["config", "user.name", "Test"])?;
    git(repository.path(), &["config", "commit.gpgsign", "false"])?;
    fs::write(repository.path().join("tracked"), "base\n")?;
    git(repository.path(), &["add", "tracked"])?;
    git(repository.path(), &["commit", "-m", "initial"])?;
    Ok(repository)
}

fn git(directory: &Path, arguments: &[&str]) -> Result<Output, Box<dyn Error>> {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(directory)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output_bounded()?;
    if !output.status.success() {
        return Err(format!(
            "git {arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(output)
}
