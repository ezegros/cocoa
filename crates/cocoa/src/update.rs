//! Verified, installation-owned updates shared by the CLI and interactive sessions.

use std::{
    fmt,
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use flate2::read::GzDecoder;
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use url::Url;

const RECEIPT: &str = ".cocoa-install.json";
const PENDING: &str = ".cocoa-update-pending.json";
const LOCK: &str = ".cocoa-update.lock";
const ARCHIVE_BYTES: u64 = 256 * 1024 * 1024;
const MANIFEST_BYTES: u64 = 64 * 1024;
const RELEASE_PAGES_MAX: usize = 10;
const RELEASE_PAGE_ENTRIES_MAX: usize = 100;
const RELEASE_PAGE_BYTES_MAX: u64 = 1024 * 1024;
const RECEIPT_BYTES: u64 = 4096;
const CHECK_INTERVAL: u64 = 3600;

/// Release stream tracked by an installer-managed executable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    #[default]
    Stable,
    /// Stable releases and numbered preview releases, ordered by semantic version.
    Preview,
}

impl fmt::Display for Channel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Stable => "stable",
            Self::Preview => "preview",
        })
    }
}

/// Options shared by `cocoa update` and `/update`. Actions are mutually exclusive.
#[derive(clap::Args)]
#[group(multiple = false)]
pub struct Arguments {
    /// Report the selected channel's latest release without changing installation policy.
    #[arg(long)]
    check: bool,
    /// Install and persistently pin an exact version, including older releases.
    #[arg(long, value_name = "VERSION", value_parser = parse_version)]
    version: Option<Version>,
    /// Clear the version pin and resume the selected channel without downgrading.
    #[arg(long)]
    unpin: bool,
    /// Persist a release channel and check for updates, respecting version pins.
    #[arg(long, value_enum)]
    channel: Option<Channel>,
}

/// Update policy for this operation. Explicit versions are persistent pins.
#[derive(Clone, Debug)]
pub enum Request {
    Latest,
    Check,
    Version(Version),
    Unpin,
    Channel(Channel),
    /// Respect pins and the installation's shared one-hour check interval.
    Automatic,
}

impl From<Arguments> for Request {
    fn from(arguments: Arguments) -> Self {
        if let Some(version) = arguments.version {
            Self::Version(version)
        } else if let Some(channel) = arguments.channel {
            Self::Channel(channel)
        } else if arguments.check {
            Self::Check
        } else if arguments.unpin {
            Self::Unpin
        } else {
            Self::Latest
        }
    }
}

/// Result based on the installed executable, which may differ from this process.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Skipped,
    Pinned {
        version: Version,
        channel: Channel,
    },
    Current {
        version: Version,
        channel: Channel,
    },
    Checked {
        installed: Version,
        latest: Version,
        pinned: Option<Version>,
        channel: Channel,
    },
    Installed {
        version: Version,
        pinned: bool,
        channel: Channel,
    },
}

impl fmt::Display for Outcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Skipped => Ok(()),
            Self::Pinned { version, channel } => write!(
                formatter,
                "Cocoa is pinned to {version}. Use --unpin to resume {channel} updates."
            ),
            Self::Current { version, channel } => write!(
                formatter,
                "Installed Cocoa {version}; no newer eligible release. Tracking {channel} updates."
            ),
            Self::Checked {
                installed,
                latest,
                pinned,
                channel,
            } => {
                write!(
                    formatter,
                    "Installed Cocoa {installed}; latest {channel} {latest}."
                )?;
                if let Some(version) = pinned {
                    write!(formatter, " Pinned to {version}.")?;
                }
                Ok(())
            }
            Self::Installed {
                version,
                pinned,
                channel,
            } => write!(
                formatter,
                "Installed Cocoa {version}{}. Tracking {channel} updates. Start Cocoa again to use it; this session is unchanged.",
                if *pinned { " and pinned it" } else { "" }
            ),
        }
    }
}

/// Updater failures never modify configuration, credentials or session databases.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(
        "this executable is not installer-managed; rerun install.sh to enable self-updates, or use the tool that installed Cocoa"
    )]
    Unmanaged,
    #[error(
        "an update is running or was interrupted; if no updater or installer is running, remove .cocoa-update.lock beside the executable and retry"
    )]
    Busy,
    #[error(
        "installation receipt does not match the executable; rerun install.sh to repair ownership"
    )]
    Ownership,
    #[error("invalid update metadata: {0}")]
    Metadata(&'static str),
    #[error("update download failed: {0}")]
    Download(#[from] reqwest::Error),
    #[error("update filesystem operation failed: {0}")]
    Io(#[from] io::Error),
    #[error("cannot read installation receipt: {0}")]
    Receipt(#[from] serde_json::Error),
    #[error("update worker failed: {0}")]
    Worker(#[from] tokio::task::JoinError),
    #[error("system clock precedes the Unix epoch")]
    Clock,
    #[error("invalid release URL: {0}")]
    Url(#[from] url::ParseError),
}

/// An executable and its trusted release host. The installer receipt chooses the repository.
#[derive(Clone)]
pub struct Updater {
    executable: PathBuf,
    host: Url,
}

impl Updater {
    /// Capture the executable path before another process can replace it.
    ///
    /// # Errors
    /// Fails if the executable path cannot be obtained.
    pub fn current() -> Result<Self, Error> {
        Self::new(std::env::current_exe()?, Url::parse("https://github.com/")?)
    }

    /// Use an explicit executable and trusted HTTPS release host.
    /// Loopback HTTP is accepted for local release fixtures. The host must be an origin.
    ///
    /// # Errors
    /// Rejects non-HTTPS remote hosts, credentials and non-origin URLs.
    pub fn new(executable: PathBuf, host: Url) -> Result<Self, Error> {
        let loopback = match host.host() {
            Some(url::Host::Ipv4(address)) => address.is_loopback(),
            Some(url::Host::Ipv6(address)) => address.is_loopback(),
            _ => false,
        };
        if !(host.scheme() == "https" || host.scheme() == "http" && loopback)
            || host.host_str().is_none()
            || !host.username().is_empty()
            || host.password().is_some()
            || host.path() != "/"
            || host.query().is_some()
            || host.fragment().is_some()
        {
            return Err(Error::Metadata("release host must be an HTTPS origin"));
        }
        Ok(Self { executable, host })
    }

    /// Check or replace the owned installation. Progress messages contain no remote text.
    /// Installation is staged, checksum-verified and atomically renamed beside the executable.
    /// Dropping this future cancels downloads; a publication already started completes under lock.
    ///
    /// # Errors
    /// Rejects unmanaged or modified binaries, concurrent updates, unsafe sidecars,
    /// invalid releases, bounded-download failures and filesystem failures.
    pub async fn run(
        &self,
        request: Request,
        progress: impl Fn(&'static str) + Send,
    ) -> Result<Outcome, Error> {
        let executable = self.executable.clone();
        let automatic = matches!(request, Request::Automatic);
        let installation =
            tokio::task::spawn_blocking(move || Installation::open(executable)).await?;
        let mut installation = match installation {
            Err(Error::Unmanaged | Error::Busy) if automatic => return Ok(Outcome::Skipped),
            result => result?,
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| Error::Clock)?
            .as_secs();
        if automatic
            && (installation.receipt.pinned_version.is_some()
                || now >= installation.receipt.checked_at
                    && now - installation.receipt.checked_at < CHECK_INTERVAL)
        {
            return Ok(Outcome::Skipped);
        }
        let channel = match request {
            Request::Channel(channel) => channel,
            _ => installation.receipt.channel,
        };
        if matches!(request, Request::Latest | Request::Channel(_))
            && let Some(version) = &installation.receipt.pinned_version
        {
            let version = version.clone();
            if matches!(request, Request::Channel(_)) {
                installation.receipt.channel = channel;
                installation.save()?;
            }
            return Ok(Outcome::Pinned { version, channel });
        }
        if let Request::Version(version) = &request {
            parse_version(&version.to_string()).map_err(Error::Metadata)?;
            if version == &installation.receipt.version {
                installation.receipt.pinned_version = Some(version.clone());
                installation.save()?;
                return Ok(Outcome::Pinned {
                    version: version.clone(),
                    channel,
                });
            }
        }
        installation.receipt.checked_at = now;
        installation.save()?;
        progress("Checking for Cocoa updates...");
        let client = reqwest::Client::builder()
            .https_only(self.host.scheme() == "https")
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(300))
            .redirect(reqwest::redirect::Policy::limited(5))
            .user_agent(concat!("cocoa/", env!("CARGO_PKG_VERSION")))
            .build()?;
        let releases = self
            .host
            .join(&format!("{}/releases/", installation.receipt.repository))?;
        let version = match &request {
            Request::Version(version) => version.clone(),
            _ => match channel {
                Channel::Stable => latest(&client, &releases).await?,
                Channel::Preview => {
                    latest_preview(&client, &self.host, &installation.receipt.repository).await?
                }
            },
        };
        if matches!(request, Request::Check) {
            return Ok(Outcome::Checked {
                installed: installation.receipt.version.clone(),
                latest: version,
                pinned: installation.receipt.pinned_version.clone(),
                channel,
            });
        }
        if !matches!(request, Request::Version(_))
            && !version
                .cmp_precedence(&installation.receipt.version)
                .is_gt()
        {
            if matches!(request, Request::Unpin) {
                installation.receipt.pinned_version = None;
            }
            installation.receipt.channel = channel;
            installation.save()?;
            return Ok(Outcome::Current {
                version: installation.receipt.version.clone(),
                channel,
            });
        }
        progress("Downloading and verifying Cocoa...");
        let assets = releases.join(&format!("download/v{version}/"))?;
        let archive_name = format!("cocoa-{}.tar.gz", installation.receipt.target);
        let manifest = download_bytes(&client, assets.join("sha256.sum")?, MANIFEST_BYTES).await?;
        let expected = checksum(&manifest, &archive_name)?;
        let staging = tempfile::Builder::new()
            .prefix(".cocoa-update-")
            .tempdir_in(installation.directory())?;
        let archive_path = staging.path().join("archive.tar.gz");
        let mut archive = tokio::fs::File::create(&archive_path).await?;
        let mut response = client
            .get(assets.join(&archive_name)?)
            .send()
            .await?
            .error_for_status()?;
        let mut bytes = 0_u64;
        let mut digest = Sha256::new();
        while let Some(chunk) = response.chunk().await? {
            bytes += chunk.len() as u64;
            if bytes > ARCHIVE_BYTES {
                return Err(Error::Metadata("archive exceeds 256 MiB"));
            }
            digest.update(&chunk);
            archive.write_all(&chunk).await?;
        }
        archive.flush().await?;
        drop(archive);
        if digest_hex(digest.finalize().as_slice()) != expected {
            return Err(Error::Metadata("archive checksum mismatch"));
        }
        progress("Installing Cocoa for the next launch...");
        tokio::task::spawn_blocking(move || {
            let binary = staging.path().join("cocoa");
            extract(&archive_path, &binary, &installation.receipt.target)?;
            let previous_digest = installation.receipt.sha256.clone();
            installation.receipt.sha256 = hash_file(&binary)?;
            installation.receipt.version = version.clone();
            let pinned = matches!(request, Request::Version(_));
            installation.receipt.pinned_version = pinned.then(|| version.clone());
            installation.receipt.channel = channel;
            installation.publish(&binary, &previous_digest)?;
            Ok(Outcome::Installed {
                version,
                pinned,
                channel,
            })
        })
        .await?
    }
}

#[derive(Deserialize, Serialize)]
struct Receipt {
    schema: u32,
    repository: String,
    target: String,
    version: Version,
    sha256: String,
    pinned_version: Option<Version>,
    #[serde(default)]
    channel: Channel,
    checked_at: u64,
}

struct Installation {
    executable: PathBuf,
    receipt: Receipt,
    _lock: DirectoryLock,
}

impl Installation {
    fn open(executable: PathBuf) -> Result<Self, Error> {
        let directory = executable.parent().ok_or(Error::Unmanaged)?;
        let receipt_path = directory.join(RECEIPT);
        let pending_path = directory.join(PENDING);
        let receipt = read_receipt(&receipt_path)?;
        let pending = read_receipt(&pending_path)?;
        if receipt.is_none() && pending.is_none() {
            return Err(Error::Unmanaged);
        }
        let lock_path = directory.join(LOCK);
        match fs::create_dir(&lock_path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Err(Error::Busy),
            Err(error) => return Err(error.into()),
        }
        let lock = DirectoryLock(directory.to_owned());
        // Read again under the lock because another updater may have just completed.
        let mut receipt = read_receipt(&receipt_path)?;
        let pending = read_receipt(&pending_path)?;
        let digest = hash_file(&executable)?;
        if let Some(pending) = pending {
            if pending.sha256 == digest {
                fs::rename(&pending_path, &receipt_path)?;
                receipt = Some(pending);
            } else if receipt
                .as_ref()
                .is_some_and(|receipt| receipt.sha256 == digest)
            {
                fs::remove_file(&pending_path)?;
            } else {
                return Err(Error::Ownership);
            }
            File::open(directory)?.sync_all()?;
        }
        let receipt = receipt.ok_or(Error::Unmanaged)?;
        if receipt.sha256 != digest {
            return Err(Error::Ownership);
        }
        Ok(Self {
            executable,
            receipt,
            _lock: lock,
        })
    }

    fn directory(&self) -> &Path {
        &self._lock.0
    }

    fn save(&self) -> Result<(), Error> {
        self.write_receipt(RECEIPT)
    }

    fn write_receipt(&self, name: &str) -> Result<(), Error> {
        let destination = self.directory().join(name);
        regular_file(&destination)?;
        let mut temporary = tempfile::NamedTempFile::new_in(self.directory())?;
        serde_json::to_writer(&mut temporary, &self.receipt)?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(destination)
            .map_err(|error| error.error)?;
        File::open(self.directory())?.sync_all()?;
        Ok(())
    }

    fn publish(&self, binary: &Path, previous_digest: &str) -> Result<(), Error> {
        if hash_file(&self.executable)? != previous_digest {
            return Err(Error::Ownership);
        }
        regular_file(&self.directory().join(RECEIPT))?;
        self.write_receipt(PENDING)?;
        fs::rename(binary, &self.executable)?;
        File::open(self.directory())?.sync_all()?;
        fs::rename(
            self.directory().join(PENDING),
            self.directory().join(RECEIPT),
        )?;
        File::open(self.directory())?.sync_all()?;
        Ok(())
    }
}

struct DirectoryLock(PathBuf);

impl Drop for DirectoryLock {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir(self.0.join(LOCK)) {
            tracing::warn!(%error, "cannot remove Cocoa update lock");
        }
    }
}

fn read_receipt(path: &Path) -> Result<Option<Receipt>, Error> {
    if !regular_file(path)? {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    File::open(path)?
        .take(RECEIPT_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > RECEIPT_BYTES {
        return Err(Error::Metadata("installation receipt exceeds 4096 bytes"));
    }
    let receipt: Receipt = serde_json::from_slice(&bytes)?;
    let repository_parts: Vec<_> = receipt.repository.split('/').collect();
    if receipt.schema != 1
        || repository_parts.len() != 2
        || repository_parts
            .iter()
            .any(|part| part.is_empty() || part.starts_with('.') || part.contains(".."))
        || !receipt
            .repository
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_./".contains(&byte))
        || ![
            "aarch64-apple-darwin",
            "x86_64-apple-darwin",
            "aarch64-unknown-linux-musl",
            "x86_64-unknown-linux-musl",
        ]
        .contains(&receipt.target.as_str())
        || !valid_digest(&receipt.sha256)
        || parse_version(&receipt.version.to_string()).is_err()
        || receipt
            .pinned_version
            .as_ref()
            .is_some_and(|version| version != &receipt.version)
    {
        return Err(Error::Metadata("invalid installation receipt"));
    }
    Ok(Some(receipt))
}

fn regular_file(path: &Path) -> Result<bool, Error> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(true),
        Ok(_) => Err(Error::Metadata(
            "executable and update sidecars must be regular files, not links",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn parse_version(input: &str) -> Result<Version, &'static str> {
    let input = input.strip_prefix('v').unwrap_or(input);
    if input.len() > 128 {
        return Err("version exceeds 128 bytes");
    }
    Version::parse(input).map_err(|_| "expected an exact version such as 0.2.3 or 0.2.3-preview.1")
}

async fn latest(client: &reqwest::Client, releases: &Url) -> Result<Version, Error> {
    let response = client
        .get(releases.join("latest")?)
        .send()
        .await?
        .error_for_status()?;
    let tag_url = releases.join("tag/")?;
    let tag = response
        .url()
        .as_str()
        .strip_prefix(tag_url.as_str())
        .ok_or(Error::Metadata(
            "latest release did not resolve to a version tag",
        ))?;
    let version = parse_version(tag).map_err(Error::Metadata)?;
    if !version.pre.is_empty() {
        return Err(Error::Metadata("latest stable release is a prerelease"));
    }
    Ok(version)
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
}

async fn latest_preview(
    client: &reqwest::Client,
    host: &Url,
    repository: &str,
) -> Result<Version, Error> {
    let api = if host.host_str() == Some("github.com") {
        Url::parse("https://api.github.com/")?
    } else {
        host.clone()
    };
    let mut latest: Option<Version> = None;
    for page in 1..=RELEASE_PAGES_MAX {
        let url = api.join(&format!(
            "repos/{repository}/releases?per_page={RELEASE_PAGE_ENTRIES_MAX}&page={page}"
        ))?;
        let bytes = download_bytes(client, url, RELEASE_PAGE_BYTES_MAX).await?;
        let releases: Vec<Release> = serde_json::from_slice(&bytes)
            .map_err(|_| Error::Metadata("invalid GitHub releases response"))?;
        if releases.len() > RELEASE_PAGE_ENTRIES_MAX {
            return Err(Error::Metadata("release page exceeds entry limit"));
        }
        for release in &releases {
            if release.draft {
                continue;
            }
            let Ok(version) = parse_version(&release.tag_name) else {
                continue;
            };
            if !version.build.is_empty() {
                continue;
            }
            if !version.pre.is_empty() {
                let Some(number) = version.pre.as_str().strip_prefix("preview.") else {
                    continue;
                };
                if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
                    continue;
                }
            }
            if latest
                .as_ref()
                .is_none_or(|current| version.cmp_precedence(current).is_gt())
            {
                latest = Some(version);
            }
        }
        if releases.len() < RELEASE_PAGE_ENTRIES_MAX {
            return latest.ok_or(Error::Metadata("no eligible preview or stable release"));
        }
    }
    Err(Error::Metadata("release discovery exceeds page limit"))
}

async fn download_bytes(client: &reqwest::Client, url: Url, limit: u64) -> Result<Vec<u8>, Error> {
    let mut response = client.get(url).send().await?.error_for_status()?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len() as u64 + chunk.len() as u64 > limit {
            return Err(Error::Metadata("download exceeds configured byte limit"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn checksum(manifest: &[u8], archive: &str) -> Result<String, Error> {
    let manifest =
        std::str::from_utf8(manifest).map_err(|_| Error::Metadata("invalid checksum manifest"))?;
    let mut found = None;
    for line in manifest.lines() {
        if line.split_whitespace().last() == Some(archive) {
            let (digest, name) = line
                .split_once("  ")
                .ok_or(Error::Metadata("invalid archive checksum entry"))?;
            if name != archive || !valid_digest(digest) || found.is_some() {
                return Err(Error::Metadata(
                    "invalid or duplicate archive checksum entry",
                ));
            }
            found = Some(digest.to_owned());
        }
    }
    found.ok_or(Error::Metadata("missing archive checksum"))
}

fn valid_digest(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn hash_file(path: &Path) -> Result<String, Error> {
    if !regular_file(path)? {
        return Err(Error::Ownership);
    }
    let mut file = File::open(path)?;
    let mut buffer = [0_u8; 64 * 1024];
    let mut digest = Sha256::new();
    let mut total = 0;
    loop {
        let bytes = file.read(&mut buffer)?;
        if bytes == 0 {
            break;
        }
        total += bytes as u64;
        if total > ARCHIVE_BYTES {
            return Err(Error::Metadata("executable exceeds 256 MiB"));
        }
        digest.update(&buffer[..bytes]);
    }
    if total == 0 {
        return Err(Error::Metadata("empty executable"));
    }
    Ok(digest_hex(digest.finalize().as_slice()))
}

fn digest_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn extract(archive: &Path, binary: &Path, target: &str) -> Result<(), Error> {
    let decoder = GzDecoder::new(File::open(archive)?).take(ARCHIVE_BYTES + 1024 * 1024);
    let mut archive = tar::Archive::new(decoder);
    let expected = format!("cocoa-{target}/cocoa");
    let mut found = false;
    for (index, entry) in archive.entries()?.enumerate() {
        if index >= 1024 {
            return Err(Error::Metadata("archive exceeds 1024 entries"));
        }
        let mut entry = entry?;
        if entry.path_bytes().as_ref() == expected.as_bytes() {
            if found
                || !entry.header().entry_type().is_file()
                || entry.size() == 0
                || entry.size() > ARCHIVE_BYTES
            {
                return Err(Error::Metadata(
                    "archive must contain exactly one regular, nonempty Cocoa executable",
                ));
            }
            found = true;
            let mut output = File::create(binary)?;
            io::copy(&mut entry, &mut output)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                output.set_permissions(fs::Permissions::from_mode(0o755))?;
            }
            output.flush()?;
            output.sync_all()?;
        }
    }
    let mut decoder = archive.into_inner();
    io::copy(&mut decoder, &mut io::sink())?;
    if !found || decoder.limit() == 0 {
        return Err(Error::Metadata(
            "missing executable or oversized archive contents",
        ));
    }
    Ok(())
}
