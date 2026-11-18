//! Release fixtures exercise the public updater without touching a real installation.

use std::{
    error::Error,
    fs,
    io::Read,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use cocoa::update::{Channel, Error as UpdateError, Outcome, Request, Updater};
use flate2::{Compression, write::GzEncoder};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path, query_param},
};

const TARGET: &str = "aarch64-apple-darwin";
const OLD: &[u8] = b"old Cocoa executable";
const NEW: &[u8] = b"new Cocoa executable, different size";

#[tokio::test]
async fn stable_updates_are_atomic_and_automatic_checks_share_a_cooldown()
-> Result<(), Box<dyn Error>> {
    let server = MockServer::start().await;
    release(&server, "1.10.0", archive(NEW, false)?, None).await;
    latest(&server, "1.10.0").await;
    let installation = installation("1.9.0", None)?;
    let updater = Updater::new(installation.path().join("cocoa"), server.uri().parse()?)?;
    let mut old = fs::File::open(installation.path().join("cocoa"))?;
    assert_eq!(
        updater.run(Request::Automatic, |_| {}).await?,
        Outcome::Installed {
            version: "1.10.0".parse()?,
            pinned: false,
            channel: Channel::Stable
        }
    );
    assert_eq!(fs::read(installation.path().join("cocoa"))?, NEW);
    let mut retained = Vec::new();
    old.read_to_end(&mut retained)?;
    assert_eq!(
        retained, OLD,
        "replacement truncated the running executable"
    );
    let mut saved = receipt(&installation)?;
    assert_eq!(saved["version"], "1.10.0");
    assert_eq!(saved["sha256"], digest(NEW));
    assert_eq!(saved["pinned_version"], Value::Null);
    assert!(
        !installation
            .path()
            .join(".cocoa-update-pending.json")
            .exists()
    );
    assert!(!installation.path().join(".cocoa-update.lock").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(installation.path().join("cocoa"))?
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
    }
    server.reset().await;
    assert_eq!(
        updater.run(Request::Automatic, |_| {}).await?,
        Outcome::Skipped
    );
    saved["checked_at"] = json!(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() - 3590);
    fs::write(
        installation.path().join(".cocoa-install.json"),
        serde_json::to_vec(&saved)?,
    )?;
    assert_eq!(
        updater.run(Request::Automatic, |_| {}).await?,
        Outcome::Skipped
    );
    latest(&server, "1.10.0").await;
    saved["checked_at"] = json!(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() - 3610);
    fs::write(
        installation.path().join(".cocoa-install.json"),
        serde_json::to_vec(&saved)?,
    )?;
    assert_eq!(
        updater.run(Request::Automatic, |_| {}).await?,
        Outcome::Current {
            version: "1.10.0".parse()?,
            channel: Channel::Stable
        }
    );
    assert_eq!(
        updater.run(Request::Check, |_| {}).await?,
        Outcome::Checked {
            installed: "1.10.0".parse()?,
            latest: "1.10.0".parse()?,
            pinned: None,
            channel: Channel::Stable
        }
    );
    Ok(())
}

#[tokio::test]
async fn exact_versions_pin_rollbacks_and_unpin_resumes_stable_without_downgrading()
-> Result<(), Box<dyn Error>> {
    let server = MockServer::start().await;
    release(&server, "1.8.0-preview.2", archive(NEW, false)?, None).await;
    let installation = installation("1.9.0", None)?;
    let updater = Updater::new(installation.path().join("cocoa"), server.uri().parse()?)?;
    assert_eq!(
        updater
            .run(Request::Version("1.8.0-preview.2".parse()?), |_| {})
            .await?,
        Outcome::Installed {
            version: "1.8.0-preview.2".parse()?,
            pinned: true,
            channel: Channel::Stable
        }
    );
    assert_eq!(receipt(&installation)?["pinned_version"], "1.8.0-preview.2");
    server.reset().await;
    assert_eq!(
        updater.run(Request::Latest, |_| {}).await?,
        Outcome::Pinned {
            version: "1.8.0-preview.2".parse()?,
            channel: Channel::Stable
        }
    );
    assert_eq!(
        updater.run(Request::Automatic, |_| {}).await?,
        Outcome::Skipped
    );
    latest(&server, "1.7.0").await;
    assert_eq!(
        updater.run(Request::Check, |_| {}).await?,
        Outcome::Checked {
            installed: "1.8.0-preview.2".parse()?,
            latest: "1.7.0".parse()?,
            pinned: Some("1.8.0-preview.2".parse()?),
            channel: Channel::Stable
        }
    );
    assert_eq!(receipt(&installation)?["pinned_version"], "1.8.0-preview.2");
    assert_eq!(
        updater.run(Request::Unpin, |_| {}).await?,
        Outcome::Current {
            version: "1.8.0-preview.2".parse()?,
            channel: Channel::Stable
        }
    );
    assert_eq!(receipt(&installation)?["pinned_version"], Value::Null);
    assert_eq!(fs::read(installation.path().join("cocoa"))?, NEW);
    server.reset().await;
    latest(&server, "1.11.0").await;
    release(&server, "1.11.0", archive(OLD, false)?, None).await;
    assert_eq!(
        updater.run(Request::Latest, |_| {}).await?,
        Outcome::Installed {
            version: "1.11.0".parse()?,
            pinned: false,
            channel: Channel::Stable
        }
    );
    assert_eq!(fs::read(installation.path().join("cocoa"))?, OLD);
    Ok(())
}

#[tokio::test]
async fn failed_verification_preserves_the_executable_and_pin() -> Result<(), Box<dyn Error>> {
    for failure in [
        "checksum",
        "duplicate-checksum",
        "symlink",
        "missing-binary",
        "duplicate-binary",
        "large-manifest",
    ] {
        let server = MockServer::start().await;
        let installation = installation("1.9.0", Some("1.9.0"))?;
        let updater = Updater::new(installation.path().join("cocoa"), server.uri().parse()?)?;
        let mut archive = archive(NEW, failure == "duplicate-binary")?;
        if failure == "symlink" || failure == "missing-binary" {
            let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::fast()));
            let mut header = tar::Header::new_gnu();
            header.set_mode(0o755);
            header.set_size(0);
            if failure == "symlink" {
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_link_name("/tmp/not-cocoa")?;
            }
            header.set_cksum();
            builder.append_data(
                &mut header,
                if failure == "symlink" {
                    format!("cocoa-{TARGET}/cocoa")
                } else {
                    "unrelated".to_owned()
                },
                &[][..],
            )?;
            archive = builder.into_inner()?.finish()?;
        }
        let line = format!("{}  cocoa-{TARGET}.tar.gz\n", digest(&archive));
        let manifest = match failure {
            "checksum" => Some(format!("{}  cocoa-{TARGET}.tar.gz\n", "0".repeat(64))),
            "duplicate-checksum" => Some(line.repeat(2)),
            "large-manifest" => Some("x".repeat(65_537)),
            _ => None,
        };
        release(&server, "1.8.0", archive, manifest).await;
        assert!(
            updater
                .run(Request::Version("1.8.0".parse()?), |_| {})
                .await
                .is_err(),
            "accepted {failure}"
        );
        assert_eq!(
            fs::read(installation.path().join("cocoa"))?,
            OLD,
            "{failure}"
        );
        let receipt = receipt(&installation)?;
        assert_eq!(receipt["version"], "1.9.0", "{failure}");
        assert_eq!(receipt["pinned_version"], "1.9.0", "{failure}");
        assert_eq!(
            fs::read_dir(installation.path())?.count(),
            2,
            "staging leaked after {failure}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn ownership_locks_and_interrupted_publications_are_respected() -> Result<(), Box<dyn Error>>
{
    let server = MockServer::start().await;
    let installation = installation("1.9.0", Some("1.9.0"))?;
    let updater = Updater::new(installation.path().join("cocoa"), server.uri().parse()?)?;
    fs::create_dir(installation.path().join(".cocoa-update.lock"))?;
    assert!(matches!(
        updater.run(Request::Latest, |_| {}).await,
        Err(UpdateError::Busy)
    ));
    assert_eq!(
        updater.run(Request::Automatic, |_| {}).await?,
        Outcome::Skipped
    );
    fs::remove_dir(installation.path().join(".cocoa-update.lock"))?;
    let mut pending = receipt(&installation)?;
    pending["version"] = json!("1.10.0");
    pending["pinned_version"] = json!("1.10.0");
    pending["sha256"] = json!(digest(NEW));
    fs::write(
        installation.path().join(".cocoa-update-pending.json"),
        serde_json::to_vec(&pending)?,
    )?;
    assert_eq!(
        updater.run(Request::Latest, |_| {}).await?,
        Outcome::Pinned {
            version: "1.9.0".parse()?,
            channel: Channel::Stable
        }
    );
    assert!(
        !installation
            .path()
            .join(".cocoa-update-pending.json")
            .exists()
    );
    fs::write(
        installation.path().join(".cocoa-update-pending.json"),
        serde_json::to_vec(&pending)?,
    )?;
    fs::write(installation.path().join("cocoa"), NEW)?;
    assert_eq!(
        updater.run(Request::Latest, |_| {}).await?,
        Outcome::Pinned {
            version: "1.10.0".parse()?,
            channel: Channel::Stable
        }
    );
    assert_eq!(receipt(&installation)?["version"], "1.10.0");
    fs::write(installation.path().join("cocoa"), b"modified installation")?;
    assert!(matches!(
        updater.run(Request::Latest, |_| {}).await,
        Err(UpdateError::Ownership)
    ));
    assert!(!installation.path().join(".cocoa-update.lock").exists());
    fs::remove_file(installation.path().join(".cocoa-install.json"))?;
    assert!(matches!(
        updater.run(Request::Latest, |_| {}).await,
        Err(UpdateError::Unmanaged)
    ));
    assert_eq!(
        updater.run(Request::Automatic, |_| {}).await?,
        Outcome::Skipped
    );
    Ok(())
}

#[tokio::test]
async fn automatic_checks_are_nonblocking_and_cancelled_downloads_release_the_lock()
-> Result<(), Box<dyn Error>> {
    let server = MockServer::start().await;
    let installation = installation("1.9.0", None)?;
    let updater = Updater::new(installation.path().join("cocoa"), server.uri().parse()?)?;
    Mock::given(method("GET"))
        .and(path("/ezegros/cocoa/releases/latest"))
        .respond_with(ResponseTemplate::new(503).set_delay(Duration::from_secs(30)))
        .mount(&server)
        .await;
    let (sender, mut receiver) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(async move {
        updater
            .run(Request::Automatic, |_| {
                sender.send_replace(true);
            })
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), receiver.changed()).await??;
    let competing = Updater::new(installation.path().join("cocoa"), server.uri().parse()?)?;
    assert!(matches!(
        competing.run(Request::Latest, |_| {}).await,
        Err(UpdateError::Busy)
    ));
    task.abort();
    assert!(task.await.is_err());
    assert!(!installation.path().join(".cocoa-update.lock").exists());
    assert_eq!(fs::read(installation.path().join("cocoa"))?, OLD);
    assert_eq!(
        competing.run(Request::Automatic, |_| {}).await?,
        Outcome::Skipped
    );
    Ok(())
}

#[tokio::test]
async fn preview_discovery_and_changed_installations_are_never_published()
-> Result<(), Box<dyn Error>> {
    let server = MockServer::start().await;
    let installation = installation("1.9.0", None)?;
    let updater = Updater::new(installation.path().join("cocoa"), server.uri().parse()?)?;
    latest(&server, "1.10.0-preview.1").await;
    assert!(matches!(
        updater.run(Request::Latest, |_| {}).await,
        Err(UpdateError::Metadata(_))
    ));
    assert_eq!(fs::read(installation.path().join("cocoa"))?, OLD);
    server.reset().await;
    latest(&server, "1.10.0").await;
    release(&server, "1.10.0", archive(NEW, false)?, None).await;
    assert!(matches!(
        updater
            .run(Request::Latest, |stage| {
                if stage == "Downloading and verifying Cocoa..." {
                    assert!(
                        fs::write(installation.path().join("cocoa"), b"external replacement")
                            .is_ok()
                    );
                }
            })
            .await,
        Err(UpdateError::Ownership)
    ));
    assert_eq!(
        fs::read(installation.path().join("cocoa"))?,
        b"external replacement"
    );
    assert_eq!(receipt(&installation)?["version"], "1.9.0");
    assert_eq!(fs::read_dir(installation.path())?.count(), 2);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn unsafe_receipt_pending_and_executable_links_are_not_followed() -> Result<(), Box<dyn Error>>
{
    use std::os::unix::fs::symlink;
    let server = MockServer::start().await;
    for name in ["cocoa", ".cocoa-install.json", ".cocoa-update-pending.json"] {
        let installation = installation("1.9.0", None)?;
        let protected = installation.path().join("protected");
        fs::write(&protected, OLD)?;
        let path = installation.path().join(name);
        if path.exists() {
            fs::remove_file(&path)?;
        }
        symlink(&protected, &path)?;
        let updater = Updater::new(installation.path().join("cocoa"), server.uri().parse()?)?;
        assert!(
            matches!(
                updater.run(Request::Latest, |_| {}).await,
                Err(UpdateError::Metadata(_))
            ),
            "followed {name}"
        );
        assert_eq!(fs::read(&protected)?, OLD);
        assert!(fs::symlink_metadata(path)?.file_type().is_symlink());
        assert!(!installation.path().join(".cocoa-update.lock").exists());
    }
    Ok(())
}

#[tokio::test]
async fn preview_discovery_orders_numeric_versions_and_ignores_unrelated_releases()
-> Result<(), Box<dyn Error>> {
    let server = MockServer::start().await;
    preview_page(
        &server,
        1,
        json!([
            {"tag_name": "v1.10.0-preview.2", "draft": false, "prerelease": true},
            {"tag_name": "v1.10.0-preview.10", "draft": false, "prerelease": true},
            {"tag_name": "v1.9.9", "draft": false, "prerelease": false},
            {"tag_name": "v99.0.0-preview.1", "draft": true, "prerelease": true},
            {"tag_name": "v99.0.0-beta.1", "draft": false, "prerelease": true},
            {"tag_name": "v99.0.0-preview.01", "draft": false, "prerelease": true},
            {"tag_name": "v99.0.0-preview.1.extra", "draft": false, "prerelease": true},
            {"tag_name": "v99.0.0-preview", "draft": false, "prerelease": true},
            {"tag_name": "v99.0.0-preview.1+build", "draft": false, "prerelease": true}
        ]),
    )
    .await;
    release(&server, "1.10.0-preview.10", archive(NEW, false)?, None).await;
    let installation = installation("1.9.0", None)?;
    let updater = Updater::new(installation.path().join("cocoa"), server.uri().parse()?)?;
    assert_eq!(
        updater
            .run(Request::Channel(Channel::Preview), |_| {})
            .await?,
        Outcome::Installed {
            version: "1.10.0-preview.10".parse()?,
            pinned: false,
            channel: Channel::Preview,
        }
    );
    assert_eq!(receipt(&installation)?["channel"], "preview");
    assert_eq!(fs::read(installation.path().join("cocoa"))?, NEW);
    assert_eq!(
        updater.run(Request::Check, |_| {}).await?,
        Outcome::Checked {
            installed: "1.10.0-preview.10".parse()?,
            latest: "1.10.0-preview.10".parse()?,
            pinned: None,
            channel: Channel::Preview,
        }
    );
    Ok(())
}

#[tokio::test]
async fn preview_channel_graduates_to_stable_and_latest_keeps_the_channel()
-> Result<(), Box<dyn Error>> {
    let server = MockServer::start().await;
    preview_page(
        &server,
        1,
        json!([
            {"tag_name": "v1.10.0-preview.10", "draft": false, "prerelease": true},
            {"tag_name": "v1.10.0", "draft": false, "prerelease": false}
        ]),
    )
    .await;
    release(&server, "1.10.0", archive(NEW, false)?, None).await;
    let installation = installation("1.10.0-preview.10", None)?;
    save_channel(&installation, "preview")?;
    let updater = Updater::new(installation.path().join("cocoa"), server.uri().parse()?)?;
    assert_eq!(
        updater.run(Request::Latest, |_| {}).await?,
        Outcome::Installed {
            version: "1.10.0".parse()?,
            pinned: false,
            channel: Channel::Preview,
        }
    );
    assert_eq!(receipt(&installation)?["channel"], "preview");
    assert_eq!(receipt(&installation)?["pinned_version"], Value::Null);
    Ok(())
}

#[tokio::test]
async fn channel_switches_bypass_cooldown_without_downgrading() -> Result<(), Box<dyn Error>> {
    let server = MockServer::start().await;
    let installation = installation("2.0.0-preview.1", None)?;
    let mut saved = receipt(&installation)?;
    saved["checked_at"] = json!(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs());
    fs::write(
        installation.path().join(".cocoa-install.json"),
        serde_json::to_vec(&saved)?,
    )?;
    preview_page(
        &server,
        1,
        json!([
            {"tag_name": "v1.10.0-preview.10", "draft": false, "prerelease": true}
        ]),
    )
    .await;
    let updater = Updater::new(installation.path().join("cocoa"), server.uri().parse()?)?;
    assert_eq!(
        updater.run(Request::Automatic, |_| {}).await?,
        Outcome::Skipped
    );
    assert_eq!(
        updater
            .run(Request::Channel(Channel::Preview), |_| {})
            .await?,
        Outcome::Current {
            version: "2.0.0-preview.1".parse()?,
            channel: Channel::Preview
        }
    );
    assert_eq!(receipt(&installation)?["channel"], "preview");
    latest(&server, "1.10.0").await;
    assert_eq!(
        updater
            .run(Request::Channel(Channel::Stable), |_| {})
            .await?,
        Outcome::Current {
            version: "2.0.0-preview.1".parse()?,
            channel: Channel::Stable
        }
    );
    assert_eq!(receipt(&installation)?["channel"], "stable");
    assert_eq!(fs::read(installation.path().join("cocoa"))?, OLD);
    Ok(())
}

#[tokio::test]
async fn switching_a_pin_is_offline_and_unpin_uses_the_selected_channel()
-> Result<(), Box<dyn Error>> {
    let server = MockServer::start().await;
    let installation = installation("1.9.0", Some("1.9.0"))?;
    let updater = Updater::new(installation.path().join("cocoa"), server.uri().parse()?)?;
    assert_eq!(
        updater
            .run(Request::Channel(Channel::Preview), |_| {})
            .await?,
        Outcome::Pinned {
            version: "1.9.0".parse()?,
            channel: Channel::Preview
        }
    );
    assert_eq!(receipt(&installation)?["channel"], "preview");
    assert_eq!(receipt(&installation)?["pinned_version"], "1.9.0");
    assert_eq!(
        updater.run(Request::Automatic, |_| {}).await?,
        Outcome::Skipped
    );
    assert_eq!(
        updater.run(Request::Latest, |_| {}).await?,
        Outcome::Pinned {
            version: "1.9.0".parse()?,
            channel: Channel::Preview
        }
    );
    let requests = server
        .received_requests()
        .await
        .ok_or("request recording disabled")?;
    assert!(requests.is_empty(), "switching a pin contacted the network");
    preview_page(
        &server,
        1,
        json!([
            {"tag_name": "v1.10.0-preview.2", "draft": false, "prerelease": true}
        ]),
    )
    .await;
    release(&server, "1.10.0-preview.2", archive(NEW, false)?, None).await;
    assert_eq!(
        updater.run(Request::Check, |_| {}).await?,
        Outcome::Checked {
            installed: "1.9.0".parse()?,
            latest: "1.10.0-preview.2".parse()?,
            pinned: Some("1.9.0".parse()?),
            channel: Channel::Preview,
        }
    );
    assert_eq!(receipt(&installation)?["pinned_version"], "1.9.0");
    assert_eq!(
        updater.run(Request::Unpin, |_| {}).await?,
        Outcome::Installed {
            version: "1.10.0-preview.2".parse()?,
            pinned: false,
            channel: Channel::Preview,
        }
    );
    assert_eq!(receipt(&installation)?["pinned_version"], Value::Null);
    release(&server, "1.8.0", archive(OLD, false)?, None).await;
    assert_eq!(
        updater
            .run(Request::Version("1.8.0".parse()?), |_| {})
            .await?,
        Outcome::Installed {
            version: "1.8.0".parse()?,
            pinned: true,
            channel: Channel::Preview,
        }
    );
    assert_eq!(receipt(&installation)?["channel"], "preview");
    assert_eq!(receipt(&installation)?["pinned_version"], "1.8.0");
    Ok(())
}

#[tokio::test]
async fn failed_channel_discovery_or_download_does_not_commit_the_channel()
-> Result<(), Box<dyn Error>> {
    for failure in [
        "http",
        "json",
        "object",
        "missing field",
        "empty",
        "oversized",
        "download",
        "checksum",
    ] {
        let server = MockServer::start().await;
        let installation = installation("1.9.0", None)?;
        save_channel(&installation, "stable")?;
        let updater = Updater::new(installation.path().join("cocoa"), server.uri().parse()?)?;
        let response = match failure {
            "http" => ResponseTemplate::new(503),
            "json" => ResponseTemplate::new(200).set_body_string("["),
            "object" => ResponseTemplate::new(200).set_body_json(json!({"message": "not a list"})),
            "missing field" => {
                ResponseTemplate::new(200).set_body_json(json!([{"tag_name": "v1.10.0-preview.1"}]))
            }
            "empty" => ResponseTemplate::new(200).set_body_json(json!([])),
            "oversized" => {
                ResponseTemplate::new(200).set_body_string(format!("[{}]", " ".repeat(1_048_576)))
            }
            _ => ResponseTemplate::new(200).set_body_json(json!([
                {"tag_name": "v1.10.0-preview.1", "draft": false, "prerelease": true}
            ])),
        };
        Mock::given(method("GET"))
            .and(path("/repos/ezegros/cocoa/releases"))
            .and(query_param("per_page", "100"))
            .and(query_param("page", "1"))
            .respond_with(response)
            .mount(&server)
            .await;
        if failure == "checksum" {
            release(
                &server,
                "1.10.0-preview.1",
                archive(NEW, false)?,
                Some(format!("{}  cocoa-{TARGET}.tar.gz\n", "0".repeat(64))),
            )
            .await;
        }
        let result = updater
            .run(Request::Channel(Channel::Preview), |_| {})
            .await;
        assert!(result.is_err(), "accepted {failure}");
        if matches!(failure, "json" | "object" | "missing field") {
            assert!(
                matches!(
                    result,
                    Err(UpdateError::Metadata("invalid GitHub releases response"))
                ),
                "misclassified {failure}: {result:?}"
            );
        }
        assert_eq!(receipt(&installation)?["channel"], "stable", "{failure}");
        assert_eq!(receipt(&installation)?["version"], "1.9.0", "{failure}");
        assert_eq!(
            receipt(&installation)?["pinned_version"],
            Value::Null,
            "{failure}"
        );
        assert_eq!(
            fs::read(installation.path().join("cocoa"))?,
            OLD,
            "{failure}"
        );
        assert_eq!(fs::read_dir(installation.path())?.count(), 2, "{failure}");
    }
    Ok(())
}

#[tokio::test]
async fn preview_discovery_paginates_until_a_short_page() -> Result<(), Box<dyn Error>> {
    let server = MockServer::start().await;
    preview_page(
        &server,
        1,
        json!(vec![
            json!({
                "tag_name": "v1.9.1", "draft": false, "prerelease": false
            });
            100
        ]),
    )
    .await;
    preview_page(
        &server,
        2,
        json!([
            {"tag_name": "v1.10.0-preview.10", "draft": false, "prerelease": true}
        ]),
    )
    .await;
    release(&server, "1.10.0-preview.10", archive(NEW, false)?, None).await;
    let installation = installation("1.9.0", None)?;
    let updater = Updater::new(installation.path().join("cocoa"), server.uri().parse()?)?;
    assert_eq!(
        updater
            .run(Request::Channel(Channel::Preview), |_| {})
            .await?,
        Outcome::Installed {
            version: "1.10.0-preview.10".parse()?,
            pinned: false,
            channel: Channel::Preview,
        }
    );
    let requests = server
        .received_requests()
        .await
        .ok_or("request recording disabled")?;
    let discovery: Vec<_> = requests
        .iter()
        .filter(|request| request.url.path() == "/repos/ezegros/cocoa/releases")
        .collect();
    assert_eq!(discovery.len(), 2);
    assert!(
        discovery[0]
            .url
            .query_pairs()
            .any(|(key, value)| key == "page" && value == "1")
    );
    assert!(
        discovery[1]
            .url
            .query_pairs()
            .any(|(key, value)| key == "page" && value == "2")
    );
    Ok(())
}

#[tokio::test]
async fn preview_discovery_rejects_ten_full_pages_instead_of_selecting_partial_results()
-> Result<(), Box<dyn Error>> {
    let server = MockServer::start().await;
    for page in 1..=10 {
        preview_page(
            &server,
            page,
            json!(vec![
                json!({
                    "tag_name": "v1.10.0-preview.1", "draft": false, "prerelease": true
                });
                100
            ]),
        )
        .await;
    }
    let installation = installation("1.9.0", None)?;
    let updater = Updater::new(installation.path().join("cocoa"), server.uri().parse()?)?;
    assert!(matches!(
        updater
            .run(Request::Channel(Channel::Preview), |_| {})
            .await,
        Err(UpdateError::Metadata(_))
    ));
    let requests = server
        .received_requests()
        .await
        .ok_or("request recording disabled")?;
    assert_eq!(requests.len(), 10);
    assert!(
        requests
            .iter()
            .all(|request| request.url.path() == "/repos/ezegros/cocoa/releases")
    );
    assert_eq!(fs::read(installation.path().join("cocoa"))?, OLD);
    assert_eq!(receipt(&installation)?["channel"], "stable");
    Ok(())
}

fn save_channel(directory: &tempfile::TempDir, channel: &str) -> Result<(), Box<dyn Error>> {
    let mut saved = receipt(directory)?;
    saved["channel"] = json!(channel);
    fs::write(
        directory.path().join(".cocoa-install.json"),
        serde_json::to_vec(&saved)?,
    )?;
    Ok(())
}

async fn preview_page(server: &MockServer, page: u32, releases: Value) {
    Mock::given(method("GET"))
        .and(path("/repos/ezegros/cocoa/releases"))
        .and(query_param("per_page", "100"))
        .and(query_param("page", page.to_string()))
        .respond_with(ResponseTemplate::new(200).set_body_json(releases))
        .expect(1..)
        .mount(server)
        .await;
}

fn installation(version: &str, pinned: Option<&str>) -> Result<tempfile::TempDir, Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("cocoa"), OLD)?;
    fs::write(
        directory.path().join(".cocoa-install.json"),
        serde_json::to_vec(&json!({
            "schema": 1, "repository": "ezegros/cocoa", "target": TARGET,
            "version": version, "sha256": digest(OLD), "pinned_version": pinned,
            "checked_at": 0,
        }))?,
    )?;
    Ok(directory)
}

fn receipt(directory: &tempfile::TempDir) -> Result<Value, Box<dyn Error>> {
    Ok(serde_json::from_slice(&fs::read(
        directory.path().join(".cocoa-install.json"),
    )?)?)
}

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn archive(binary: &[u8], duplicate: bool) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut archive = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::fast()));
    for _ in 0..if duplicate { 2 } else { 1 } {
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o755);
        header.set_size(binary.len() as u64);
        header.set_cksum();
        archive.append_data(&mut header, format!("cocoa-{TARGET}/cocoa"), binary)?;
    }
    Ok(archive.into_inner()?.finish()?)
}

async fn latest(server: &MockServer, version: &str) {
    Mock::given(method("GET"))
        .and(path("/ezegros/cocoa/releases/latest"))
        .respond_with(ResponseTemplate::new(302).insert_header(
            "location",
            format!("/ezegros/cocoa/releases/tag/v{version}"),
        ))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/ezegros/cocoa/releases/tag/v{version}")))
        .respond_with(ResponseTemplate::new(200))
        .mount(server)
        .await;
}

async fn release(server: &MockServer, version: &str, archive: Vec<u8>, manifest: Option<String>) {
    let manifest =
        manifest.unwrap_or_else(|| format!("{}  cocoa-{TARGET}.tar.gz\n", digest(&archive)));
    Mock::given(method("GET"))
        .and(path(format!(
            "/ezegros/cocoa/releases/download/v{version}/sha256.sum"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_string(manifest))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/ezegros/cocoa/releases/download/v{version}/cocoa-{TARGET}.tar.gz"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(archive))
        .mount(server)
        .await;
}
