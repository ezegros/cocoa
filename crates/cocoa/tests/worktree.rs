//! Worktree creation against temporary Git repositories.

#[path = "support/process.rs"]
mod process;

use std::{error::Error, fs, path::Path, process::Command};

use cocoa::worktree::{Error as WorktreeError, create};
use process::CommandExt;
use sha2::{Digest, Sha256};

#[tokio::test]
async fn creates_exact_branch_from_head_without_dirty_files() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    let home = tempfile::tempdir()?;
    fs::write(repository.path().join("tracked"), "dirty\n")?;
    fs::write(repository.path().join("untracked"), "local\n")?;
    fs::create_dir(repository.path().join("nested"))?;
    let destination = create(&repository.path().join("nested"), home.path(), "feature").await?;
    assert_eq!(
        fs::read_to_string(destination.join("tracked"))?,
        "committed\n"
    );
    assert!(!destination.join("untracked").exists());
    assert_eq!(
        git(&destination, &["branch", "--show-current"])?,
        "feature\n"
    );
    assert_eq!(
        git(&destination, &["rev-parse", "HEAD"])?,
        git(repository.path(), &["rev-parse", "HEAD"])?
    );
    assert_eq!(git(&destination, &["status", "--porcelain"])?, "");
    let common = fs::canonicalize(repository.path().join(".git"))?;
    let digest = Sha256::digest(common.as_os_str().as_encoded_bytes());
    let digest: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    let basename = repository
        .path()
        .file_name()
        .ok_or("missing basename")?
        .to_string_lossy();
    assert_eq!(
        destination,
        fs::canonicalize(home.path())?
            .join(".config/cocoa/worktrees")
            .join(format!("{basename}-{digest}"))
            .join("feature")
    );
    assert_eq!(
        fs::read_to_string(repository.path().join("tracked"))?,
        "dirty\n"
    );
    Ok(())
}

#[tokio::test]
async fn linked_and_detached_trees_share_repository_identity() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    let home = tempfile::tempdir()?;
    let first = create(repository.path(), home.path(), "first").await?;
    git(&first, &["checkout", "--detach"])?;
    fs::create_dir(first.join("nested"))?;
    let second = create(&first.join("nested"), home.path(), "second").await?;
    assert_eq!(first.parent(), second.parent());
    assert_eq!(git(&second, &["branch", "--show-current"])?, "second\n");
    Ok(())
}

#[tokio::test]
async fn same_basename_repositories_do_not_collide() -> Result<(), Box<dyn Error>> {
    let first_root = tempfile::tempdir()?;
    let second_root = tempfile::tempdir()?;
    let home = tempfile::tempdir()?;
    let first = first_root.path().join("project");
    let second = second_root.path().join("project");
    initialize(&first)?;
    initialize(&second)?;
    let first = create(&first, home.path(), "feature").await?;
    let second = create(&second, home.path(), "feature").await?;
    assert_ne!(first.parent(), second.parent());
    Ok(())
}

#[tokio::test]
async fn accepts_unicode_and_maximum_length_names() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    let home = tempfile::tempdir()?;
    let long = "a".repeat(128);
    for name in ["修正", long.as_str()] {
        let destination = create(repository.path(), home.path(), name).await?;
        assert_eq!(destination.file_name(), Some(std::ffi::OsStr::new(name)));
        assert_eq!(
            git(&destination, &["branch", "--show-current"])?,
            format!("{name}\n")
        );
    }
    Ok(())
}

#[tokio::test]
async fn reports_git_mutation_failure_without_removing_existing_state() -> Result<(), Box<dyn Error>>
{
    let repository = repository()?;
    let home = tempfile::tempdir()?;
    let lock = repository.path().join(".git/refs/heads/feature.lock");
    fs::write(&lock, "keep")?;
    assert!(matches!(
        create(repository.path(), home.path(), "feature").await,
        Err(WorktreeError::Git {
            operation: "worktree add",
            ..
        })
    ));
    assert_eq!(fs::read_to_string(lock)?, "keep");
    assert!(git(repository.path(), &["branch", "--list", "feature"])?.is_empty());
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn does_not_run_checkout_hooks() -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::PermissionsExt;

    let repository = repository()?;
    let home = tempfile::tempdir()?;
    let hook = repository.path().join(".git/hooks/post-checkout");
    fs::write(&hook, "#!/bin/sh\nprintf ran > hook-ran\n")?;
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755))?;
    let destination = create(repository.path(), home.path(), "feature").await?;
    assert!(!destination.join("hook-ran").exists());
    assert!(!repository.path().join("hook-ran").exists());
    Ok(())
}

#[tokio::test]
async fn rejects_unsafe_and_invalid_branch_names_without_creating_directories()
-> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    let home = tempfile::tempdir()?;
    for name in [
        "",
        ".",
        "..",
        "../escape",
        "a/b",
        "a\\b",
        "-option",
        "bad name",
        "a..b",
        "a@{b",
        "bad.lock",
        "bad.",
        "a:b",
        "line\nfeed",
        "@{-1}",
        "HEAD",
        "nul\0byte",
        "tab\tname",
    ] {
        assert!(
            matches!(
                create(repository.path(), home.path(), name).await,
                Err(WorktreeError::InvalidName)
            ),
            "accepted {name:?}"
        );
    }
    let long = "a".repeat(129);
    assert!(matches!(
        create(repository.path(), home.path(), &long).await,
        Err(WorktreeError::InvalidName)
    ));
    assert!(!home.path().join(".config").exists());
    Ok(())
}

#[tokio::test]
async fn refuses_existing_branch_and_destination_including_dangling_symlink()
-> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    let home = tempfile::tempdir()?;
    git(repository.path(), &["branch", "existing"])?;
    assert!(matches!(
        create(repository.path(), home.path(), "existing").await,
        Err(WorktreeError::BranchExists)
    ));
    let first = create(repository.path(), home.path(), "first").await?;
    let parent = first.parent().ok_or("missing destination parent")?;
    let blocked = parent.join("blocked");
    fs::create_dir(&blocked)?;
    assert!(
        matches!(create(repository.path(), home.path(), "blocked").await, Err(WorktreeError::DestinationExists(path)) if path == blocked)
    );
    assert!(git(repository.path(), &["branch", "--list", "blocked"])?.is_empty());
    let file = parent.join("file");
    fs::write(&file, "keep")?;
    assert!(
        matches!(create(repository.path(), home.path(), "file").await, Err(WorktreeError::DestinationExists(path)) if path == file)
    );
    assert_eq!(fs::read_to_string(file)?, "keep");
    #[cfg(unix)]
    {
        let link = parent.join("link");
        std::os::unix::fs::symlink(parent.join("missing"), &link)?;
        assert!(
            matches!(create(repository.path(), home.path(), "link").await, Err(WorktreeError::DestinationExists(path)) if path == link)
        );
    }
    Ok(())
}

#[tokio::test]
async fn rejects_non_repository_bare_and_unborn_repositories() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let home = tempfile::tempdir()?;
    assert!(matches!(
        create(directory.path(), home.path(), "feature").await,
        Err(WorktreeError::NotWorkingTree)
    ));
    git(directory.path(), &["init", "--bare"])?;
    assert!(matches!(
        create(directory.path(), home.path(), "feature").await,
        Err(WorktreeError::NotWorkingTree)
    ));
    let unborn = tempfile::tempdir()?;
    git(unborn.path(), &["init"])?;
    assert!(matches!(
        create(unborn.path(), home.path(), "feature").await,
        Err(WorktreeError::UnbornHead)
    ));
    Ok(())
}

#[tokio::test]
async fn reports_missing_directory_and_unusable_home() -> Result<(), Box<dyn Error>> {
    let repository = repository()?;
    let home = tempfile::tempdir()?;
    assert!(matches!(
        create(&repository.path().join("missing"), home.path(), "feature").await,
        Err(WorktreeError::Io(_))
    ));
    let file = home.path().join("file");
    fs::write(&file, "keep")?;
    assert!(matches!(
        create(repository.path(), &file, "feature").await,
        Err(WorktreeError::Io(_))
    ));
    Ok(())
}

fn repository() -> Result<tempfile::TempDir, Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    initialize(directory.path())?;
    Ok(directory)
}

fn initialize(directory: &Path) -> Result<(), Box<dyn Error>> {
    fs::create_dir_all(directory)?;
    git(directory, &["init", "-b", "main"])?;
    fs::write(directory.join("tracked"), "committed\n")?;
    git(directory, &["add", "tracked"])?;
    git(
        directory,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "initial",
        ],
    )?;
    Ok(())
}

fn git(directory: &Path, arguments: &[&str]) -> Result<String, Box<dyn Error>> {
    let output = Command::new("git")
        .args(["-c", "core.hooksPath=/dev/null"])
        .args(arguments)
        .current_dir(directory)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .output_bounded()?;
    if !output.status.success() {
        return Err(format!(
            "git {arguments:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(String::from_utf8(output.stdout)?)
}
