//! Worktree startup argument and command compatibility contracts.

use clap::Parser;
use cocoa::config::Arguments;
use std::{error::Error, process::Command};

#[test]
fn worktree_requires_a_name_and_preserves_the_prompt() -> Result<(), Box<dyn Error>> {
    for flag in ["--worktree", "-w"] {
        assert!(Arguments::try_parse_from(["cocoa", flag]).is_err());
        let arguments = Arguments::try_parse_from(["cocoa", flag, "fix-login", "Fix login"])?;
        assert_eq!(arguments.worktree.as_deref(), Some("fix-login"));
        assert_eq!(arguments.prompt.as_deref(), Some("Fix login"));
    }
    assert!(Arguments::try_parse_from(["cocoa"])?.worktree.is_none());
    Ok(())
}

#[test]
fn subcommands_reject_worktrees_without_side_effects() -> Result<(), Box<dyn Error>> {
    let home = tempfile::tempdir()?;
    for arguments in [
        vec!["-w", "fix-login", "resume", "--last"],
        vec!["exec", "-w", "fix-login", "hello"],
        vec!["-w", "fix-login", "models"],
        vec!["-w", "fix-login", "sessions", "list"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_cocoa"))
            .args(arguments)
            .env("HOME", home.path())
            .current_dir(home.path())
            .output()?;
        assert!(!output.status.success());
        let message = String::from_utf8_lossy(&output.stderr);
        assert!(
            message.contains("--worktree is supported only"),
            "{message}"
        );
        assert!(!home.path().join(".config/cocoa/worktrees").exists());
    }
    Ok(())
}
