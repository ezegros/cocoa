//! Release installation through real shells, archives, hashes and local download fixtures.

#![cfg(unix)]

use std::{
    error::Error,
    fs,
    io::{Read, Write},
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

use sha2::{Digest, Sha256};
use tempfile::TempDir;

const INSTALLER: &[u8] = include_bytes!("../../../install.sh");
const LINUX_TARGET: &str = "x86_64-unknown-linux-musl";

#[test]
fn preview_channel_orders_versions_and_preserves_policy() -> Result<(), Box<dyn Error>> {
    for shell in ["/bin/sh", "/bin/bash"] {
        let fixture = Fixture::new("Linux", "x86_64")?;
        fixture.release("v0.2.0-preview.10", LINUX_TARGET)?;
        let releases = serde_json::json!([
            {"tag_name": "v0.2.0-preview.2", "draft": false},
            {"tag_name": "v0.1.9", "draft": false},
            {"tag_name": "v0.2.0-preview.10", "draft": false},
            {"tag_name": "v9.0.0", "draft": true},
            {"tag_name": "v9.0.0-beta.1", "draft": false},
            {"tag_name": "v9.0.0-preview.01", "draft": false},
            {"tag_name": "v9.0.0-preview.1+build", "draft": false}
        ]);
        fs::write(fixture.assets.join("page-1.json"), releases.to_string())?;
        let output = run_installer(fixture.command(shell).args(["--channel", "preview"]))?;
        assert!(output.status.success(), "{output:?}");
        let directory = fixture.home.join(".local/bin");
        let receipt_path = directory.join(".cocoa-install.json");
        let receipt: serde_json::Value = serde_json::from_slice(&fs::read(&receipt_path)?)?;
        assert_eq!(receipt["schema"], 1);
        assert_eq!(receipt["channel"], "preview");
        assert_eq!(receipt["version"], "0.2.0-preview.10");
        assert_eq!(receipt["pinned_version"], serde_json::Value::Null);

        fixture.release("v0.2.0", LINUX_TARGET)?;
        fs::write(
            fixture.assets.join("page-1.json"),
            r#"[{"tag_name":"v0.2.0-preview.10","draft":false},{"tag_name":"v0.2.0","draft":false}]"#,
        )?;
        let output = run_installer(&mut fixture.command(shell))?;
        assert!(output.status.success(), "{output:?}");
        let receipt: serde_json::Value = serde_json::from_slice(&fs::read(&receipt_path)?)?;
        assert_eq!(receipt["channel"], "preview");
        assert_eq!(receipt["version"], "0.2.0");
        let output = run_installer(fixture.command(shell).args(["--version", "0.2.0"]))?;
        assert!(output.status.success(), "{output:?}");
        let receipt: serde_json::Value = serde_json::from_slice(&fs::read(&receipt_path)?)?;
        assert_eq!(receipt["channel"], "preview");
        assert_eq!(receipt["pinned_version"], "0.2.0");
    }
    Ok(())
}

#[test]
fn preview_discovery_paginates_and_rejects_truncation() -> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new("Linux", "x86_64")?;
    fixture.release("v0.2.0-preview.1", LINUX_TARGET)?;
    let full_page = vec![serde_json::json!({"tag_name":"v0.1.0", "draft":false}); 100];
    fs::write(
        fixture.assets.join("page-1.json"),
        serde_json::to_vec(&full_page)?,
    )?;
    fs::write(
        fixture.assets.join("page-2.json"),
        r#"[{"tag_name":"v0.2.0-preview.1","draft":false}]"#,
    )?;
    let output = run_installer(fixture.command("/bin/sh").args(["--channel", "preview"]))?;
    assert!(output.status.success(), "{output:?}");
    let directory = fixture.home.join(".local/bin");
    let previous = fs::read(directory.join(".cocoa-install.json"))?;
    for page in 2..=10 {
        fs::write(
            fixture.assets.join(format!("page-{page}.json")),
            serde_json::to_vec(&full_page)?,
        )?;
    }
    let output = run_installer(fixture.command("/bin/sh").args(["--channel", "preview"]))?;
    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr)?.contains("exceeded 1000 releases"));
    assert_eq!(fs::read(directory.join(".cocoa-install.json"))?, previous);
    for response in [
        "not JSON",
        "{}",
        "[]",
        r#"[{"tag_name":"v0.3.0","draft":"false"}]"#,
    ] {
        fs::write(fixture.assets.join("page-1.json"), response)?;
        let output = run_installer(fixture.command("/bin/sh").args(["--channel", "preview"]))?;
        assert!(!output.status.success(), "{response}: {output:?}");
        assert_eq!(fs::read(directory.join(".cocoa-install.json"))?, previous);
    }
    Ok(())
}

#[test]
fn channel_validation_and_safe_receipt_fallback() -> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new("Linux", "x86_64")?;
    fixture.release("v0.1.0", LINUX_TARGET)?;
    for arguments in [
        vec!["--channel", "nightly"],
        vec!["--channel"],
        vec!["--version", "0.1.0", "--channel", "stable"],
    ] {
        let output = run_installer(fixture.command("/bin/sh").args(arguments))?;
        assert!(!output.status.success());
        assert!(!fixture.home.join(".local/bin/cocoa").exists());
    }
    let output = run_installer(&mut fixture.command("/bin/sh"))?;
    assert!(output.status.success(), "{output:?}");
    let directory = fixture.home.join(".local/bin");
    let receipt_path = directory.join(".cocoa-install.json");
    let mut receipt: serde_json::Value = serde_json::from_slice(&fs::read(&receipt_path)?)?;
    receipt
        .as_object_mut()
        .ok_or("receipt must be an object")?
        .remove("channel");
    fs::write(&receipt_path, serde_json::to_vec(&receipt)?)?;
    let output = run_installer(&mut fixture.command("/bin/sh"))?;
    assert!(output.status.success(), "{output:?}");
    assert_receipt(&directory, LINUX_TARGET, "0.1.0", None)?;

    fs::remove_file(fixture.tools.join("jq"))?;
    let output = run_installer(&mut fixture.command("/bin/sh"))?;
    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr)?.contains("requires jq"));
    let output = run_installer(fixture.command("/bin/sh").args(["--channel", "stable"]))?;
    assert!(output.status.success(), "{output:?}");
    let output = run_installer(fixture.command("/bin/sh").args(["--channel", "preview"]))?;
    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr)?.contains("requires jq"));
    Ok(())
}

#[test]
fn installs_each_platform_and_replaces_only_the_executable() -> Result<(), Box<dyn Error>> {
    for (system, architecture, target, shell) in [
        ("Darwin", "arm64", "aarch64-apple-darwin", "/bin/bash"),
        ("Darwin", "x86_64", "x86_64-apple-darwin", "/bin/sh"),
        ("Linux", "aarch64", "aarch64-unknown-linux-musl", "/bin/sh"),
        ("Linux", "x86_64", LINUX_TARGET, "/bin/bash"),
    ] {
        let fixture = Fixture::new(system, architecture)?;
        fixture.release("v0.1.0", target)?;
        let directory = fixture.home.join(".local/bin");
        fs::create_dir_all(&directory)?;
        let executable = directory.join("cocoa");
        write_executable(&executable, "previous executable")?;
        let mut previous = fs::File::open(&executable)?;

        let output = run_installer(fixture.command(shell).args(["--version", "0.1.0"]))?;
        assert!(output.status.success(), "{target}: {output:?}");
        let installed = Command::new(&executable).output()?;
        assert!(installed.status.success());
        assert_eq!(
            String::from_utf8(installed.stdout)?,
            format!("v0.1.0 {target}\n")
        );
        assert_eq!(
            fs::metadata(&executable)?.permissions().mode() & 0o777,
            0o755
        );
        let mut retained = String::new();
        previous.read_to_string(&mut retained)?;
        assert_eq!(
            retained, "previous executable",
            "replace by rename, not truncation"
        );
        assert_receipt(&directory, target, "0.1.0", Some("0.1.0"))?;
        assert_directory_entries(&directory, &[".cocoa-install.json", "cocoa"])?;
        fixture.assert_user_data_unchanged()?;

        let output = run_installer(fixture.command(shell).args(["--version", "v0.1.0"]).env(
            "PATH",
            format!("{}:{}", directory.display(), fixture.tools.display()),
        ))?;
        assert!(output.status.success(), "{output:?}");
        assert!(!String::from_utf8(output.stdout)?.contains("export PATH="));
        assert_receipt(&directory, target, "0.1.0", Some("0.1.0"))?;
        assert_directory_entries(&directory, &[".cocoa-install.json", "cocoa"])?;
    }
    Ok(())
}

#[test]
fn resolves_latest_to_one_release_and_reports_unpublished_releases() -> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new("Linux", "x86_64")?;
    fixture.release("v0.1.0", LINUX_TARGET)?;
    fixture.release("v0.2.0", LINUX_TARGET)?;
    fixture.release("v0.2.0-preview.3", LINUX_TARGET)?;
    fs::write(
        fixture.assets.join("latest"),
        "https://github.com/ezegros/cocoa/releases/tag/v0.2.0",
    )?;
    let output = run_installer(&mut fixture.command("/bin/sh"))?;
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        Command::new(fixture.home.join(".local/bin/cocoa"))
            .output()?
            .stdout,
        format!("v0.2.0 {LINUX_TARGET}\n").as_bytes()
    );
    let directory = fixture.home.join(".local/bin");
    assert_receipt(&directory, LINUX_TARGET, "0.2.0", None)?;
    assert_directory_entries(&directory, &[".cocoa-install.json", "cocoa"])?;

    for (version, expected) in [
        ("0.2.0-preview.3", "v0.2.0-preview.3"),
        ("v0.1.0", "v0.1.0"),
    ] {
        let output = run_installer(fixture.command("/bin/bash").args(["--version", version]))?;
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            Command::new(fixture.home.join(".local/bin/cocoa"))
                .output()?
                .stdout,
            format!("{expected} {LINUX_TARGET}\n").as_bytes()
        );
        let normalized = expected.trim_start_matches('v');
        assert_receipt(&directory, LINUX_TARGET, normalized, Some(normalized))?;
        assert_directory_entries(&directory, &[".cocoa-install.json", "cocoa"])?;
    }
    let output = run_installer(&mut fixture.command("/bin/sh"))?;
    assert!(output.status.success(), "{output:?}");
    assert_receipt(&directory, LINUX_TARGET, "0.2.0", None)?;
    let previous = fs::read(fixture.home.join(".local/bin/cocoa"))?;
    let previous_receipt = fs::read(directory.join(".cocoa-install.json"))?;
    for latest in [None, Some("https://github.com/ezegros/cocoa/releases")] {
        match latest {
            Some(url) => fs::write(fixture.assets.join("latest"), url)?,
            None => fs::remove_file(fixture.assets.join("latest"))?,
        }
        let output = run_installer(&mut fixture.command("/bin/sh"))?;
        assert!(!output.status.success(), "{output:?}");
        assert!(String::from_utf8(output.stderr)?.contains("publish a release"));
        assert_eq!(fs::read(fixture.home.join(".local/bin/cocoa"))?, previous);
        assert_eq!(
            fs::read(directory.join(".cocoa-install.json"))?,
            previous_receipt
        );
        assert_directory_entries(&directory, &[".cocoa-install.json", "cocoa"])?;
    }
    Ok(())
}

#[test]
fn failed_downloads_and_checksums_preserve_the_installed_version() -> Result<(), Box<dyn Error>> {
    for (failure, message) in [
        ("missing manifest", "Could not download checksums"),
        ("missing entry", "Missing, invalid or duplicate"),
        ("duplicate entry", "Missing, invalid or duplicate"),
        ("invalid checksum", "Missing, invalid or duplicate"),
        ("missing archive", "Could not download cocoa-"),
        ("corrupt archive", "SHA-256 checksum mismatch"),
        ("invalid archive", "Archive is missing"),
    ] {
        let fixture = Fixture::new("Linux", "x86_64")?;
        let archive = fixture.release("v0.1.0", LINUX_TARGET)?;
        let manifest = fixture.assets.join("v0.1.0/sha256.sum");
        let checksum = fs::read_to_string(&manifest)?;
        match failure {
            "missing manifest" => fs::remove_file(&manifest)?,
            "missing entry" => {
                fs::write(&manifest, checksum.replace(LINUX_TARGET, "other-target"))?
            }
            "duplicate entry" => fs::write(&manifest, checksum.repeat(2))?,
            "invalid checksum" => fs::write(&manifest, checksum.replacen(' ', "", 1))?,
            "missing archive" => fs::remove_file(&archive)?,
            "corrupt archive" => fs::write(&archive, "corrupted download")?,
            "invalid archive" => {
                fs::write(&archive, "not a tar archive")?;
                write_checksum(&archive)?;
            }
            _ => unreachable!("all fixture failure modes are handled"),
        }
        let directory = fixture.home.join(".local/bin");
        fs::create_dir_all(&directory)?;
        let executable = directory.join("cocoa");
        write_executable(&executable, "previous executable")?;
        let receipt = directory.join(".cocoa-install.json");
        let previous_receipt = serde_json::json!({
            "schema": 1,
            "repository": "ezegros/cocoa",
            "target": LINUX_TARGET,
            "channel": "stable"
        })
        .to_string();
        fs::write(&receipt, &previous_receipt)?;
        let output = run_installer(fixture.command("/bin/sh").args(["--version", "v0.1.0"]))?;
        assert!(!output.status.success(), "{failure}: {output:?}");
        assert!(
            String::from_utf8(output.stderr)?.contains(message),
            "{failure}"
        );
        assert_eq!(fs::read_to_string(&executable)?, "previous executable");
        assert_eq!(
            fs::metadata(&executable)?.permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(fs::read_to_string(&receipt)?, previous_receipt);
        assert_directory_entries(&directory, &[".cocoa-install.json", "cocoa"])?;
        fixture.assert_user_data_unchanged()?;
    }
    Ok(())
}

#[test]
fn refuses_archive_links_and_destination_links_or_directories() -> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new("Linux", "x86_64")?;
    fixture.release("v0.1.0", LINUX_TARGET)?;
    let outside = fixture.root.path().join("outside");
    fs::write(&outside, "do not overwrite")?;
    let directory = fixture.home.join(".local/bin");
    fs::create_dir_all(&directory)?;
    let executable = directory.join("cocoa");
    symlink(&outside, &executable)?;
    let output = run_installer(fixture.command("/bin/sh").args(["--version", "v0.1.0"]))?;
    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8(output.stderr)?.contains("Refusing to replace"));
    assert!(fs::symlink_metadata(&executable)?.file_type().is_symlink());
    assert_eq!(fs::read_to_string(&outside)?, "do not overwrite");

    fs::remove_file(&executable)?;
    fs::create_dir(&executable)?;
    let output = run_installer(fixture.command("/bin/sh").args(["--version", "v0.1.0"]))?;
    assert!(!output.status.success(), "{output:?}");
    assert!(executable.is_dir());
    fs::remove_dir(&executable)?;

    let member = fixture
        .root
        .path()
        .join(format!("staging/cocoa-{LINUX_TARGET}/cocoa"));
    fs::remove_file(&member)?;
    symlink(&outside, &member)?;
    fixture.pack("v0.1.0", LINUX_TARGET)?;
    let output = run_installer(fixture.command("/bin/sh").args(["--version", "v0.1.0"]))?;
    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8(output.stderr)?.contains("exactly one regular"));
    assert!(!executable.exists());
    assert_eq!(fs::read_dir(&directory)?.count(), 0);
    assert_eq!(fs::read_to_string(&outside)?, "do not overwrite");
    Ok(())
}

#[test]
fn directory_overrides_and_printed_path_support_shell_metacharacters() -> Result<(), Box<dyn Error>>
{
    let fixture = Fixture::new("Darwin", "arm64")?;
    fixture.release("v0.1.0", "aarch64-apple-darwin")?;
    let directory = fixture.home.join("tools' $literal \\ bin");
    let ignored = fixture.home.join("ignored");
    let output = run_installer(
        fixture
            .command("/bin/bash")
            .env("COCOA_INSTALL_DIR", &directory)
            .args(["--version", "v0.1.0"]),
    )?;
    assert!(output.status.success(), "{output:?}");
    assert!(directory.join("cocoa").is_file());
    assert!(!fixture.home.join(".local/bin").exists());
    let stdout = String::from_utf8(output.stdout)?;
    let export = stdout
        .lines()
        .find(|line| line.trim_start().starts_with("export PATH="))
        .ok_or("missing PATH instruction")?;
    for shell in ["/bin/bash", "/bin/zsh"] {
        if !Path::new(shell).is_file() {
            continue;
        }
        let output = Command::new(shell)
            .env_clear()
            .env("PATH", "/original")
            .env("HOME", &fixture.home)
            .env("ZDOTDIR", &fixture.home)
            .args(["-f", "-c", &format!("{export}\nprintf '%s' \"$PATH\"")])
            .output()?;
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            String::from_utf8(output.stdout)?,
            format!("{}:/original", directory.display())
        );
    }
    let output = run_installer(
        fixture
            .command("/bin/sh")
            .env("COCOA_INSTALL_DIR", &ignored)
            .args([
                "--version",
                "v0.1.0",
                "--install-dir",
                directory.to_str().ok_or("non-UTF-8 fixture")?,
            ]),
    )?;
    assert!(output.status.success(), "{output:?}");
    assert!(
        !ignored.exists(),
        "CLI directory must override the environment"
    );
    fixture.assert_user_data_unchanged()?;
    Ok(())
}

#[test]
fn refuses_missing_hash_tools_unsupported_platforms_and_invalid_options()
-> Result<(), Box<dyn Error>> {
    let fixture = Fixture::new("Linux", "x86_64")?;
    fs::remove_file(fixture.tools.join(fixture.checksum_tool))?;
    let output = run_installer(fixture.command("/bin/sh").args(["--version", "v0.1.0"]))?;
    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8(output.stderr)?.contains("requires sha256sum or shasum"));
    assert!(!fixture.home.join(".local/bin").exists());

    let fixture = Fixture::new("Linux", "riscv64")?;
    let output = run_installer(fixture.command("/bin/sh").args(["--version", "v0.1.0"]))?;
    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8(output.stderr)?.contains("Unsupported platform: Linux riscv64"));
    for (arguments, message) in [
        (vec!["--version"], "Missing value"),
        (vec!["--unknown"], "Unknown option"),
        (vec!["--install-dir", "relative"], "absolute path"),
        (vec!["--help"], "Usage:"),
    ] {
        let output = run_installer(
            Command::new("/bin/sh")
                .env_clear()
                .args(["-s", "--"])
                .args(&arguments),
        )?;
        assert_eq!(
            output.status.success(),
            arguments == ["--help"],
            "{output:?}"
        );
        let combined = format!(
            "{}{}",
            String::from_utf8(output.stdout)?,
            String::from_utf8(output.stderr)?
        );
        assert!(combined.contains(message), "{combined}");
    }
    let fixture = Fixture::new("Linux", "x86_64")?;
    for version in [
        "../v0.1.0",
        "v0.1.0\nother",
        "v01.1.0",
        "v0.01.0",
        "v0.1.00",
        "v0.1.0-",
        "v0.1.0-preview..3",
        "v0.1.0-preview.03",
        "v0.1.0-preview.",
        "v0.1.0+",
        "v0.1.0+build..3",
        "v0.1.0+build.",
        "v0.1.0+build+other",
        "v18446744073709551616.1.0",
        "v0.18446744073709551616.0",
        "v0.1.18446744073709551616",
    ] {
        let output = run_installer(fixture.command("/bin/sh").args(["--version", version]))?;
        assert!(!output.status.success(), "{output:?}");
        assert!(String::from_utf8(output.stderr)?.contains("Invalid version"));
        assert!(!fixture.home.join(".local/bin").exists());
    }
    Ok(())
}

#[test]
fn refuses_existing_update_locks_without_removing_them() -> Result<(), Box<dyn Error>> {
    for shell in ["/bin/sh", "/bin/bash"] {
        for kind in ["directory", "symlink", "dangling symlink", "file"] {
            let fixture = Fixture::new("Linux", "x86_64")?;
            fixture.release("v0.1.0", LINUX_TARGET)?;
            let directory = fixture.home.join(".local/bin");
            fs::create_dir_all(&directory)?;
            let lock = directory.join(".cocoa-update.lock");
            let outside = fixture.root.path().join("outside");
            match kind {
                "directory" => {
                    fs::create_dir(&lock)?;
                    fs::write(lock.join("owner"), "another updater")?;
                }
                "symlink" => {
                    fs::create_dir(&outside)?;
                    fs::write(outside.join("owner"), "another updater")?;
                    symlink(&outside, &lock)?;
                }
                "dangling symlink" => symlink(&outside, &lock)?,
                "file" => fs::write(&lock, "another updater")?,
                _ => unreachable!("all lock fixture kinds are handled"),
            }
            let output = run_installer(&mut fixture.command(shell))?;
            assert!(!output.status.success(), "{shell}, {kind}: {output:?}");
            let message = String::from_utf8(output.stderr)?;
            assert!(message.contains("Another Cocoa installer or updater may be running"));
            assert!(message.contains("confirm no updater is running"));
            assert!(message.contains("manually"));
            assert_directory_entries(&directory, &[".cocoa-update.lock"])?;
            match kind {
                "directory" | "symlink" => {
                    assert_eq!(fs::read_to_string(lock.join("owner"))?, "another updater");
                }
                "dangling symlink" => {
                    assert!(fs::symlink_metadata(&lock)?.file_type().is_symlink());
                    assert!(!outside.exists());
                }
                "file" => assert_eq!(fs::read_to_string(&lock)?, "another updater"),
                _ => unreachable!("all lock fixture kinds are handled"),
            }
        }
    }
    Ok(())
}

#[test]
fn refuses_unsafe_receipt_and_pending_sidecars() -> Result<(), Box<dyn Error>> {
    for name in [".cocoa-install.json", ".cocoa-update-pending.json"] {
        for kind in ["symlink", "dangling symlink", "directory", "fifo"] {
            let fixture = Fixture::new("Linux", "x86_64")?;
            fixture.release("v0.1.0", LINUX_TARGET)?;
            let directory = fixture.home.join(".local/bin");
            fs::create_dir_all(&directory)?;
            write_executable(&directory.join("cocoa"), "previous executable")?;
            let sidecar = directory.join(name);
            let outside = fixture.root.path().join("outside");
            match kind {
                "symlink" => {
                    fs::write(&outside, "do not overwrite")?;
                    symlink(&outside, &sidecar)?;
                }
                "dangling symlink" => symlink(&outside, &sidecar)?,
                "directory" => fs::create_dir(&sidecar)?,
                "fifo" => {
                    let output = Command::new("mkfifo").arg(&sidecar).output()?;
                    assert!(output.status.success(), "{output:?}");
                }
                _ => unreachable!("all sidecar fixture kinds are handled"),
            }
            let before = fs::symlink_metadata(&sidecar)?.file_type();
            let output = run_installer(fixture.command("/bin/sh").args(["--version", "0.1.0"]))?;
            assert!(!output.status.success(), "{name}, {kind}: {output:?}");
            assert!(String::from_utf8(output.stderr)?.contains("Refusing to replace"));
            assert_eq!(fs::symlink_metadata(&sidecar)?.file_type(), before);
            assert_eq!(
                fs::read_to_string(directory.join("cocoa"))?,
                "previous executable"
            );
            assert_directory_entries(&directory, &[name, "cocoa"])?;
            if kind == "symlink" {
                assert_eq!(fs::read_to_string(&outside)?, "do not overwrite");
            } else {
                assert!(!outside.exists());
            }
        }
    }
    Ok(())
}

#[test]
fn publication_failures_and_signals_retain_pending_receipts_for_recovery()
-> Result<(), Box<dyn Error>> {
    for shell in ["/bin/sh", "/bin/bash"] {
        for failure in ["pending", "binary", "receipt", "HUP", "INT", "TERM"] {
            let fixture = Fixture::new("Linux", "x86_64")?;
            fixture.release("v0.1.0", LINUX_TARGET)?;
            let output = run_installer(fixture.command(shell).args(["--version", "0.1.0"]))?;
            assert!(output.status.success(), "{output:?}");
            let directory = fixture.home.join(".local/bin");
            let previous = fs::read(directory.join("cocoa"))?;
            let previous_receipt = fs::read(directory.join(".cocoa-install.json"))?;
            fixture.release("v0.2.0", LINUX_TARGET)?;
            fs::rename(fixture.tools.join("mv"), fixture.tools.join("mv-real"))?;
            write_executable(
                &fixture.tools.join("mv"),
                r#"#!/bin/sh
set -eu
[ -d "$HOME/.local/bin/.cocoa-update.lock" ] || exit 91
case "$3" in
    */.cocoa-update-pending.json) step=pending ;;
    */cocoa) step=binary ;;
    */.cocoa-install.json) step=receipt ;;
    *) exit 92 ;;
esac
if [ "$step" = "$COCOA_TEST_FAILURE" ]; then exit 77; fi
"${0%/*}/mv-real" "$@"
if [ "$step" = binary ]; then
    case "$COCOA_TEST_FAILURE" in
        HUP|INT|TERM) kill -"$COCOA_TEST_FAILURE" "$PPID" ;;
    esac
fi
"#,
            )?;
            let output = run_installer(
                fixture
                    .command(shell)
                    .args(["--version", "0.2.0"])
                    .env("COCOA_TEST_FAILURE", failure),
            )?;
            let status = match failure {
                "HUP" => 129,
                "INT" => 130,
                "TERM" => 143,
                _ => 77,
            };
            assert_eq!(
                output.status.code(),
                Some(status),
                "{shell}, {failure}: {output:?}"
            );
            assert_eq!(
                fs::read(directory.join(".cocoa-install.json"))?,
                previous_receipt
            );
            if failure == "pending" {
                assert_eq!(fs::read(directory.join("cocoa"))?, previous);
                assert_directory_entries(&directory, &[".cocoa-install.json", "cocoa"])?;
            } else {
                assert_directory_entries(
                    &directory,
                    &[".cocoa-install.json", ".cocoa-update-pending.json", "cocoa"],
                )?;
                let pending: serde_json::Value = serde_json::from_slice(&fs::read(
                    directory.join(".cocoa-update-pending.json"),
                )?)?;
                assert_eq!(pending["version"], "0.2.0");
                assert_eq!(pending["pinned_version"], "0.2.0");
                let new_executable = fs::read(
                    fixture
                        .root
                        .path()
                        .join(format!("staging/cocoa-{LINUX_TARGET}/cocoa")),
                )?;
                let checksum: String = Sha256::digest(&new_executable)
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect();
                assert_eq!(pending["sha256"], checksum);
                if failure == "binary" {
                    assert_eq!(fs::read(directory.join("cocoa"))?, previous);
                } else {
                    assert_eq!(fs::read(directory.join("cocoa"))?, new_executable);
                }
            }
            fs::remove_file(fixture.tools.join("mv"))?;
            fs::rename(fixture.tools.join("mv-real"), fixture.tools.join("mv"))?;
            let output = run_installer(fixture.command(shell).args(["--version", "0.2.0"]))?;
            assert!(output.status.success(), "{output:?}");
            assert_receipt(&directory, LINUX_TARGET, "0.2.0", Some("0.2.0"))?;
            assert_directory_entries(&directory, &[".cocoa-install.json", "cocoa"])?;
        }
    }
    Ok(())
}

#[test]
fn cleans_up_when_signaled_while_creating_lock_or_staging() -> Result<(), Box<dyn Error>> {
    for shell in ["/bin/sh", "/bin/bash"] {
        for tool in ["mkdir", "mktemp"] {
            let fixture = Fixture::new("Linux", "x86_64")?;
            let original = fixture.tools.join(tool);
            fs::rename(&original, fixture.tools.join(format!("{tool}-real")))?;
            write_executable(
                &original,
                r#"#!/bin/sh
set -eu
"$0-real" "$@"
case "$0:$1" in
    */mkdir:*/.cocoa-update.lock|*/mktemp:-d) kill -TERM "$COCOA_TEST_INSTALLER_PROCESS" ;;
esac
"#,
            )?;
            let mut script =
                b"COCOA_TEST_INSTALLER_PROCESS=$$; export COCOA_TEST_INSTALLER_PROCESS\n".to_vec();
            script.extend_from_slice(INSTALLER);
            let output = run_script(&mut fixture.command(shell), &script)?;
            assert_eq!(
                output.status.code(),
                Some(143),
                "{shell}, {tool}: {output:?}"
            );
            assert_directory_entries(&fixture.home.join(".local/bin"), &[])?;
        }
    }
    Ok(())
}

#[test]
fn installs_valid_semver_without_running_the_downloaded_executable() -> Result<(), Box<dyn Error>> {
    for version in ["v0.0.0", "v1.2.3-preview.0-x+build.01-2", "v1.2.3+001"] {
        let fixture = Fixture::new("Linux", "x86_64")?;
        fixture.release(version, LINUX_TARGET)?;
        let marker = fixture.root.path().join("executed");
        write_executable(
            &fixture
                .root
                .path()
                .join(format!("staging/cocoa-{LINUX_TARGET}/cocoa")),
            &format!(
                "#!/bin/sh\nprintf executed > '{}'\nexit 99\n",
                marker.display()
            ),
        )?;
        fixture.pack(version, LINUX_TARGET)?;
        let output = run_installer(fixture.command("/bin/sh").args(["--version", version]))?;
        assert!(output.status.success(), "{output:?}");
        assert!(
            !marker.exists(),
            "the installer must not execute release binaries"
        );
        let normalized = version.trim_start_matches('v');
        let directory = fixture.home.join(".local/bin");
        assert_receipt(&directory, LINUX_TARGET, normalized, Some(normalized))?;
        assert_directory_entries(&directory, &[".cocoa-install.json", "cocoa"])?;
    }
    Ok(())
}

struct Fixture {
    root: TempDir,
    home: PathBuf,
    tools: PathBuf,
    assets: PathBuf,
    system: &'static str,
    architecture: &'static str,
    checksum_tool: &'static str,
}

impl Fixture {
    fn new(system: &'static str, architecture: &'static str) -> Result<Self, Box<dyn Error>> {
        let root = tempfile::tempdir()?;
        let home = root.path().join("home");
        let tools = root.path().join("tools");
        let assets = root.path().join("assets");
        for directory in [&home, &tools, &assets] {
            fs::create_dir(directory)?;
        }
        let checksum_tool = if cfg!(target_os = "macos") {
            "shasum"
        } else {
            "sha256sum"
        };
        for name in [
            "tar",
            "gzip",
            "mktemp",
            "mkdir",
            "rmdir",
            "chmod",
            "mv",
            "rm",
            "awk",
            "sed",
            "jq",
            "wc",
            "cat",
            "cp",
            checksum_tool,
        ] {
            let path = std::env::split_paths(&std::env::var_os("PATH").ok_or("PATH is unset")?)
                .map(|directory| directory.join(name))
                .find(|path| path.is_file())
                .ok_or_else(|| format!("required fixture command not found: {name}"))?;
            symlink(path, tools.join(name))?;
        }
        write_executable(
            &tools.join("uname"),
            "#!/bin/sh\ncase $1 in -s) printf '%s\\n' \"$COCOA_TEST_SYSTEM\" ;; -m) printf '%s\\n' \"$COCOA_TEST_ARCHITECTURE\" ;; *) exit 2 ;; esac\n",
        )?;
        write_executable(
            &tools.join("curl"),
            r#"#!/bin/sh
set -eu
output=/dev/null
while [ "$#" -gt 0 ]; do
    case "$1" in
        -q|--fail|--silent|--show-error|--location) shift ;;
        --proto|--proto-redir|--tlsv1.2|--max-redirs|--connect-timeout|--max-time|--retry|--retry-max-time|--max-filesize|--write-out)
            case "$1" in --tlsv1.2) shift ;; *) shift 2 ;; esac ;;
        --output) output=$2; shift 2 ;;
        https://github.com/ezegros/cocoa/releases/*|https://api.github.com/repos/ezegros/cocoa/releases\?*) url=$1; shift ;;
        *) printf 'Unexpected download argument: %s\n' "$1" >&2; exit 2 ;;
    esac
done
case "$url" in
    https://api.github.com/repos/ezegros/cocoa/releases\?per_page=100\&page=*)
        page=${url##*page=}
        cp "$COCOA_TEST_ASSETS/page-$page.json" "$output" ;;
    https://github.com/ezegros/cocoa/releases/latest) cat "$COCOA_TEST_ASSETS/latest" ;;
    https://github.com/ezegros/cocoa/releases/download/v*)
        path=${url#https://github.com/ezegros/cocoa/releases/download/}
        cp "$COCOA_TEST_ASSETS/$path" "$output" ;;
    *) exit 22 ;;
esac
"#,
        )?;
        fs::write(
            assets.join("latest"),
            "https://github.com/ezegros/cocoa/releases/tag/v0.1.0",
        )?;
        for relative in [
            ".profile",
            ".bashrc",
            ".zshrc",
            ".config/cocoa/config.json",
            ".config/cocoa/auth.json",
            ".local/share/cocoa/sessions.sqlite3",
        ] {
            let path = home.join(relative);
            fs::create_dir_all(path.parent().ok_or("fixture file has no parent")?)?;
            fs::write(path, relative)?;
        }
        Ok(Self {
            root,
            home,
            tools,
            assets,
            system,
            architecture,
            checksum_tool,
        })
    }

    fn command(&self, shell: &str) -> Command {
        let mut command = Command::new(shell);
        command
            .env_clear()
            .env("PATH", &self.tools)
            .env("HOME", &self.home)
            .env("LC_ALL", "C")
            .env("COCOA_TEST_ASSETS", &self.assets)
            .env("COCOA_TEST_SYSTEM", self.system)
            .env("COCOA_TEST_ARCHITECTURE", self.architecture)
            .args(["-s", "--"]);
        command
    }

    fn release(&self, version: &str, target: &str) -> Result<PathBuf, Box<dyn Error>> {
        let directory = self.root.path().join(format!("staging/cocoa-{target}"));
        fs::create_dir_all(&directory)?;
        write_executable(
            &directory.join("cocoa"),
            &format!("#!/bin/sh\nprintf '%s\\n' '{version} {target}'\n"),
        )?;
        fs::write(directory.join("README.md"), "fixture readme")?;
        fs::write(directory.join("LICENSE"), "fixture license")?;
        self.pack(version, target)
    }

    fn pack(&self, version: &str, target: &str) -> Result<PathBuf, Box<dyn Error>> {
        let directory = self.assets.join(version);
        fs::create_dir_all(&directory)?;
        let archive = directory.join(format!("cocoa-{target}.tar.gz"));
        let output = Command::new("tar")
            .args(["-czf"])
            .arg(&archive)
            .arg("-C")
            .arg(self.root.path().join("staging"))
            .arg(format!("cocoa-{target}"))
            .output()?;
        assert!(output.status.success(), "{output:?}");
        write_checksum(&archive)?;
        Ok(archive)
    }

    fn assert_user_data_unchanged(&self) -> Result<(), Box<dyn Error>> {
        for relative in [
            ".profile",
            ".bashrc",
            ".zshrc",
            ".config/cocoa/config.json",
            ".config/cocoa/auth.json",
            ".local/share/cocoa/sessions.sqlite3",
        ] {
            assert_eq!(fs::read_to_string(self.home.join(relative))?, relative);
        }
        assert!(!self.home.join(".zprofile").exists());
        Ok(())
    }
}

fn write_executable(path: &Path, content: &str) -> Result<(), Box<dyn Error>> {
    fs::write(path, content)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    Ok(())
}

fn write_checksum(archive: &Path) -> Result<(), Box<dyn Error>> {
    let checksum: String = Sha256::digest(fs::read(archive)?)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let name = archive
        .file_name()
        .ok_or("archive has no name")?
        .to_str()
        .ok_or("non-UTF-8 archive name")?;
    fs::write(
        archive.with_file_name("sha256.sum"),
        format!("{checksum}  {name}\n"),
    )?;
    Ok(())
}

fn assert_receipt(
    directory: &Path,
    target: &str,
    version: &str,
    pinned_version: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let bytes = fs::read(directory.join(".cocoa-install.json"))?;
    assert!(bytes.len() <= 4096);
    let receipt: serde_json::Value = serde_json::from_slice(&bytes)?;
    let checksum: String = Sha256::digest(fs::read(directory.join("cocoa"))?)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert_eq!(
        receipt,
        serde_json::json!({
            "schema": 1,
            "repository": "ezegros/cocoa",
            "target": target,
            "version": version,
            "sha256": checksum,
            "pinned_version": pinned_version,
            "channel": "stable",
            "checked_at": 0,
        })
    );
    Ok(())
}

fn assert_directory_entries(directory: &Path, expected: &[&str]) -> Result<(), Box<dyn Error>> {
    let mut actual = fs::read_dir(directory)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<Result<Vec<_>, _>>()?;
    actual.sort();
    let mut expected: Vec<_> = expected.iter().map(std::ffi::OsString::from).collect();
    expected.sort();
    assert_eq!(actual, expected, "unexpected installer files remain");
    Ok(())
}

fn run_installer(command: &mut Command) -> Result<Output, Box<dyn Error>> {
    run_script(command, INSTALLER)
}

fn run_script(command: &mut Command, script: &[u8]) -> Result<Output, Box<dyn Error>> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or("installer stdin unavailable")?
        .write_all(script)?;
    Ok(child.wait_with_output()?)
}
