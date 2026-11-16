//! Bounded API-key input and plaintext, owner-only credential persistence.
//! Permission bits do not provide encryption or isolation from the same user.

use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{IsTerminal, Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};

use clap::Subcommand;
use cocoa_llm::ProviderKind;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};

const KEY_BYTES: usize = 8192;
const STORE_BYTES: usize = 65_536;
const COMMAND_CACHE_ENTRIES: usize = 64;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

static COMMAND_CACHE: Mutex<BTreeMap<String, Result<SecretString, ()>>> =
    Mutex::new(BTreeMap::new());

/// Provider credentials. ChatGPT uses interactive browser OAuth instead of a key.
/// No secret is accepted in process arguments.
#[derive(Subcommand)]
pub enum AuthCommand {
    /// Save a key from hidden terminal input, or explicitly from bounded stdin.
    Set {
        provider: ProviderKind,
        /// Read a key from nonterminal stdin, at most 8192 bytes plus a final LF/CRLF.
        #[arg(long)]
        stdin: bool,
    },
    /// Remove only the saved entry. Does not revoke the key or change environment/config.
    Remove { provider: ProviderKind },
}

/// Sanitized failures. Never retain input values, paths, JSON or underlying errors.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error(transparent)]
    OAuth(#[from] cocoa_oauth::OAuthError),
    #[error("ChatGPT uses browser sign-in, not an API key; choose openai for API-key billing")]
    Subscription,
    #[error("cannot determine auth file path; configure a home directory")]
    Path,
    #[cfg(not(unix))]
    #[error("secure auth files are unsupported on this platform; use credential_env")]
    Unsupported,
    #[error(
        "auth files must be owner-only regular files with one link; repair permissions explicitly"
    )]
    Permissions,
    #[error("auth directory must be owner-only and not a symlink; repair permissions explicitly")]
    Directory,
    #[error("cannot read auth file")]
    Read,
    #[error("auth file exceeds 65536 bytes")]
    Limit,
    #[error("invalid auth JSON, duplicate or unknown provider/key record")]
    Json,
    #[error(
        "API key must contain 1..=8192 graphic ASCII bytes without whitespace, or be a nonempty !command of at most 8192 bytes without NUL"
    )]
    Key,
    #[error("API key command failed or exceeded its limits")]
    Command,
    #[error("API key command cache is unavailable or full")]
    CommandCache,
    #[error("no saved API key for selected provider; use login or configure credential_env")]
    Missing,
    #[error("local Ollama does not accept credentials")]
    OllamaCredential,
    #[error("cannot acquire auth file lock")]
    Lock,
    #[error("auth file is busy; retry later")]
    Busy,
    #[error("cannot save auth file; original file was not replaced")]
    Save,
    #[error("auth file replaced, but directory sync failed; durability is unknown")]
    Sync,
    #[error("auth temporary-file cleanup failed; a private temporary file may remain")]
    Cleanup,
    #[error("hidden key input requires terminal stdin and stderr; pipe a key for automation")]
    Terminal,
    #[error("--stdin requires nonterminal input; omit it for hidden terminal input")]
    StdinTerminal,
    #[error("cannot read API key input")]
    Input,
    #[error("API key input cancelled or timed out")]
    Cancelled,
    #[error("cannot restore terminal settings")]
    Restore,
    #[error("cannot write auth command result")]
    Output,
}

/// Execute without loading the main configuration. API-key operations use local
/// storage only; ChatGPT sign-in opens a browser and makes OAuth requests, and
/// sign-out removes local tokens before attempting remote revocation.
///
/// # Errors
/// Reject invalid input, unsafe storage, lock contention, failed persistence or
/// OAuth failures, including cancellation and unconfirmed remote revocation.
pub fn run(command: AuthCommand) -> Result<(), AuthError> {
    if matches!(
        command,
        AuthCommand::Set {
            provider: ProviderKind::ChatGpt,
            ..
        } | AuthCommand::Remove {
            provider: ProviderKind::ChatGpt
        }
    ) {
        return run_chatgpt(command);
    }
    secure_platform()?;
    let path = auth_path()?;
    let (provider, key) = match command {
        AuthCommand::Set {
            provider: ProviderKind::Ollama,
            ..
        } => return Err(AuthError::OllamaCredential),
        AuthCommand::Set { provider, stdin } => (provider, Some(read_key(stdin)?)),
        AuthCommand::Remove { provider } => (provider, None),
    };
    let setting = key.is_some();
    update(&path, provider, key)?;
    writeln!(
        std::io::stderr(),
        "{} saved API key for {provider}; environment and config unchanged",
        if setting { "Set" } else { "Removed" }
    )
    .map_err(|_| AuthError::Output)
}

/// Persist a literal key or !command without executing it, or remove the saved entry.
///
/// # Errors
/// Reject invalid keys, local Ollama credentials or unsafe or unavailable storage.
pub fn save(provider: ProviderKind, key: Option<SecretString>) -> Result<(), AuthError> {
    secure_platform()?;
    if provider == ProviderKind::ChatGpt {
        return Err(AuthError::Subscription);
    }
    if let Some(key) = &key {
        if provider == ProviderKind::Ollama {
            return Err(AuthError::OllamaCredential);
        }
        if !valid_credential(key.expose_secret()) {
            return Err(AuthError::Key);
        }
    }
    update(
        &auth_path()?,
        provider,
        key.map(|key| key.expose_secret().to_owned()),
    )
}

/// Resolve only the saved key, never inferred environment variables.
/// A leading ! runs the remainder through /bin/sh. Results, including failures,
/// are cached by command text for this process's lifetime without eviction.
/// Commands inherit the environment, have no stdin, and discard stderr.
/// Execution is limited to five seconds and 8192 stdout bytes, followed by at
/// most 250 milliseconds of cleanup. Trimmed stdout must satisfy [valid_key].
/// At most 64 distinct commands are executed, one at a time.
///
/// # Errors
/// Missing keys, unsafe files, malformed stores, failed commands and a full cache
/// are errors. Only [AuthError::Missing] denotes an absent credential.
pub fn saved(provider: ProviderKind) -> Result<SecretString, AuthError> {
    if provider == ProviderKind::ChatGpt {
        return Err(AuthError::Subscription);
    }
    let path = auth_path()?;
    let mut records = read_store(&path)?;
    let record = records
        .remove(&provider.to_string())
        .ok_or(AuthError::Missing)?;
    match record {
        Record::ApiKey { api_key } => resolve_credential(&api_key),
    }
}

/// Strict resolved/environment key contract, also accepted by the transport.
#[must_use]
pub fn valid_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= KEY_BYTES && key.bytes().all(|byte| byte.is_ascii_graphic())
}

/// Accept a literal key or a nonempty !command, at most 8192 bytes including !.
/// Commands may contain whitespace and Unicode, but never NUL bytes.
#[must_use]
pub fn valid_credential(credential: &str) -> bool {
    match credential.strip_prefix('!') {
        Some(command) => {
            credential.len() <= KEY_BYTES && !command.trim().is_empty() && !command.contains('\0')
        }
        None => valid_key(credential),
    }
}

/// Validate a stored credential and resolve its literal key or cached !command.
/// Command execution and cache limits are the same as for [saved]. This does
/// not persist credentials or inspect environment variables for fallback keys.
///
/// # Errors
/// Invalid credentials, failed commands and an unavailable or full cache fail
/// without including the credential, command or output in the error.
pub fn resolve_credential(credential: &str) -> Result<SecretString, AuthError> {
    if !valid_credential(credential) {
        return Err(AuthError::Key);
    }
    match credential.strip_prefix('!') {
        Some(command) => command_key(command),
        None => Ok(credential.into()),
    }
}

fn command_key(command: &str) -> Result<SecretString, AuthError> {
    let mut cache = COMMAND_CACHE.lock().map_err(|_| AuthError::CommandCache)?;
    if let Some(result) = cache.get(command) {
        return result.clone().map_err(|()| AuthError::Command);
    }
    if cache.len() >= COMMAND_CACHE_ENTRIES {
        return Err(AuthError::CommandCache);
    }
    let result = execute_command(command);
    cache.insert(command.to_owned(), result.clone());
    result.map_err(|()| AuthError::Command)
}

#[cfg(unix)]
fn execute_command(command: &str) -> Result<SecretString, ()> {
    use rustix::process::Pid;
    use std::{
        os::unix::process::CommandExt,
        process::{Command, Stdio},
    };

    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|_| ())?;
    let group = Pid::from_child(&child);
    let result = command_output(&mut child, group);
    cleanup_command(&mut child, group)?;
    result
}

#[cfg(not(unix))]
fn execute_command(_: &str) -> Result<SecretString, ()> {
    Err(())
}

#[cfg(unix)]
fn command_output(
    child: &mut std::process::Child,
    group: rustix::process::Pid,
) -> Result<SecretString, ()> {
    use rustix::{
        fs::{OFlags, fcntl_getfl, fcntl_setfl},
        process::{WaitId, WaitIdOptions, waitid},
    };

    let mut stdout = child.stdout.take().ok_or(())?;
    let flags = fcntl_getfl(&stdout).map_err(|_| ())?;
    fcntl_setfl(&stdout, flags | OFlags::NONBLOCK).map_err(|_| ())?;
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    let mut bytes = Vec::with_capacity(KEY_BYTES);
    let mut buffer = [0; 1024];
    let mut finished = false;
    while Instant::now() < deadline {
        match stdout.read(&mut buffer) {
            Ok(0) => finished = true,
            Ok(length) => {
                if bytes.len() + length > KEY_BYTES {
                    return Err(());
                }
                bytes.extend_from_slice(&buffer[..length]);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) => {}
            Err(_) => return Err(()),
        }
        // Keep the leader unreaped until cleanup signals its group, preventing PID reuse.
        if let Some(status) = waitid(
            WaitId::Pid(group),
            WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG,
        )
        .map_err(|_| ())?
        {
            if status.exit_status() != Some(0) {
                return Err(());
            }
            if finished {
                let key = std::str::from_utf8(&bytes).map_err(|_| ())?.trim();
                return if valid_key(key) {
                    Ok(key.into())
                } else {
                    Err(())
                };
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Err(())
}

#[cfg(unix)]
fn cleanup_command(child: &mut std::process::Child, group: rustix::process::Pid) -> Result<(), ()> {
    use rustix::{
        io::Errno,
        process::{Signal, kill_process_group, test_kill_process_group},
    };

    let group_result = match kill_process_group(group, Signal::KILL) {
        Ok(()) => Ok(()),
        Err(Errno::SRCH) => Ok(()),
        Err(Errno::PERM) if cfg!(target_os = "macos") => Ok(()),
        Err(_) => Err(()),
    };
    let root_result = if group_result.is_err() {
        child.kill().map_err(|_| ())
    } else {
        Ok(())
    };
    let deadline = Instant::now() + Duration::from_millis(250);
    let mut reaped = false;
    while Instant::now() < deadline {
        if !reaped {
            reaped = child.try_wait().map_err(|_| ())?.is_some();
        }
        if reaped {
            match test_kill_process_group(group) {
                Err(Errno::SRCH) => {
                    root_result?;
                    return group_result;
                }
                Ok(()) => {}
                // Darwin reports EPERM for zombie-only groups; require their disappearance.
                Err(Errno::PERM) if cfg!(target_os = "macos") => {}
                Err(_) => return Err(()),
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    Err(())
}

fn run_chatgpt(command: AuthCommand) -> Result<(), AuthError> {
    let client = crate::chatgpt::client(None)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| AuthError::Input)?;
    runtime.block_on(async move {
        match command {
            AuthCommand::Set { .. } => {
                use std::io::BufRead;
                if !(std::io::stdin().is_terminal() && std::io::stderr().is_terminal()) { return Err(AuthError::Terminal) }
                eprintln!("Sign in with ChatGPT. Inference uses your ChatGPT plan and available credits, not API billing.\nContinue? [y/N]");
                let mut confirmation = String::new();
                std::io::stdin().lock().take(8).read_line(&mut confirmation).map_err(|_| AuthError::Input)?;
                if !confirmation.trim().eq_ignore_ascii_case("y") { return Err(AuthError::Cancelled) }
                let registration = client.accounts()?.into_iter().find(|account| account.active).map(|account| account.registration);
                let login = client.begin(registration).await?;
                let cancellation = tokio_util::sync::CancellationToken::new();
                let result = async {
                    login.open_browser().await?;
                    eprintln!("Waiting for browser sign-in. Ctrl+C cancels.");
                    login.finish(cancellation.clone()).await
                };
                let account = tokio::select! {
                    signal = tokio::signal::ctrl_c() => {
                        signal.map_err(|_| AuthError::Input)?;
                        cancellation.cancel();
                        return Err(AuthError::Cancelled);
                    }
                    account = result => account?,
                };
                eprintln!("{}", if account.inference_enabled { "Signed in." } else { "Identity-only sign-in; inference permission was not granted." });
            }
            AuthCommand::Remove { .. } => {
                let registration = client.accounts()?.into_iter().find(|account| account.active).ok_or(cocoa_oauth::OAuthError::Reauthorize)?.registration;
                client.logout(&registration).await?;
                eprintln!("Signed out locally. ChatGPT registration retained.");
            }
        }
        Ok(())
    })
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
enum Record {
    ApiKey { api_key: String },
}

#[derive(Deserialize)]
struct Records(#[serde(deserialize_with = "crate::config::unique_table")] BTreeMap<String, Record>);

fn auth_path() -> Result<PathBuf, AuthError> {
    std::env::home_dir()
        .map(|home| home.join(".config/cocoa/auth.json"))
        .ok_or(AuthError::Path)
}

fn read_store(path: &Path) -> Result<BTreeMap<String, Record>, AuthError> {
    secure_platform()?;
    let file = match secure_open(path, OpenMode::Read) {
        Ok(Some(file)) => file,
        Ok(None) => return Ok(BTreeMap::new()),
        Err(error) => return Err(error),
    };
    #[cfg(unix)]
    check_directory(
        path.parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )?;
    let mut bytes = Vec::new();
    file.take((STORE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| AuthError::Read)?;
    if bytes.len() > STORE_BYTES {
        return Err(AuthError::Limit);
    }
    let Records(records) = serde_json::from_slice(&bytes).map_err(|_| AuthError::Json)?;
    for (name, record) in &records {
        name.parse::<ProviderKind>().map_err(|_| AuthError::Json)?;
        let Record::ApiKey { api_key } = record;
        if !valid_credential(api_key) {
            return Err(AuthError::Key);
        }
    }
    Ok(records)
}

#[cfg(unix)]
fn update(path: &Path, provider: ProviderKind, key: Option<String>) -> Result<(), AuthError> {
    use std::{fs::TryLockError, os::unix::fs::DirBuilderExt};

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    if key.is_none() {
        match fs::symlink_metadata(path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(_) => return Err(AuthError::Read),
        }
    }
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)
        .map_err(|_| AuthError::Directory)?;
    check_directory(parent)?;
    let mut lock_name = path.as_os_str().to_os_string();
    lock_name.push(".lock");
    let lock = secure_open(Path::new(&lock_name), OpenMode::Create)?.ok_or(AuthError::Lock)?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20))
            }
            Err(TryLockError::WouldBlock) => return Err(AuthError::Busy),
            Err(TryLockError::Error(_)) => return Err(AuthError::Lock),
        }
    }
    // Removing the sidecar would let writers lock different inodes for the same store.
    let mut records = read_store(path)?;
    match key {
        Some(api_key) => {
            records.insert(provider.to_string(), Record::ApiKey { api_key });
        }
        None => {
            records.remove(&provider.to_string());
        }
    }
    let bytes = serde_json::to_vec(&records).map_err(|_| AuthError::Save)?;
    if bytes.len() > STORE_BYTES {
        return Err(AuthError::Limit);
    }
    let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(|_| AuthError::Save)?;
    if temporary
        .write_all(&bytes)
        .and_then(|()| temporary.as_file().sync_all())
        .is_err()
    {
        temporary.close().map_err(|_| AuthError::Cleanup)?;
        return Err(AuthError::Save);
    }
    match temporary.persist(path) {
        Ok(file) => drop(file),
        Err(error) => {
            error.file.close().map_err(|_| AuthError::Cleanup)?;
            return Err(AuthError::Save);
        }
    }
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| AuthError::Sync)
}

#[cfg(not(unix))]
fn update(_: &Path, _: ProviderKind, _: Option<String>) -> Result<(), AuthError> {
    Err(AuthError::Unsupported)
}

#[cfg(unix)]
fn secure_platform() -> Result<(), AuthError> {
    Ok(())
}

#[cfg(not(unix))]
fn secure_platform() -> Result<(), AuthError> {
    Err(AuthError::Unsupported)
}

enum OpenMode {
    Read,
    #[cfg(unix)]
    Create,
}

#[cfg(unix)]
fn secure_open(path: &Path, mode: OpenMode) -> Result<Option<File>, AuthError> {
    use std::{
        fs::OpenOptions,
        os::unix::fs::{MetadataExt, OpenOptionsExt},
    };
    let create = matches!(mode, OpenMode::Create);
    match fs::symlink_metadata(path) {
        Ok(metadata) => check_file(&metadata)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if !create {
                return Ok(None);
            }
        }
        Err(_) => return Err(AuthError::Read),
    }
    let file = OpenOptions::new()
        .read(true)
        .write(create)
        .create(create)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map_err(|_| AuthError::Read)?;
    let opened = file.metadata().map_err(|_| AuthError::Read)?;
    check_file(&opened)?;
    let named = fs::symlink_metadata(path).map_err(|_| AuthError::Read)?;
    if named.file_type().is_symlink() || named.ino() != opened.ino() || named.dev() != opened.dev()
    {
        return Err(AuthError::Permissions);
    }
    Ok(Some(file))
}

#[cfg(not(unix))]
fn secure_open(_: &Path, _: OpenMode) -> Result<Option<File>, AuthError> {
    Err(AuthError::Unsupported)
}

#[cfg(unix)]
fn check_file(metadata: &fs::Metadata) -> Result<(), AuthError> {
    use std::os::unix::fs::MetadataExt;
    if !metadata.is_file()
        || metadata.mode() & 0o777 != 0o600
        || metadata.nlink() != 1
        || metadata.uid() != rustix::process::geteuid().as_raw()
    {
        return Err(AuthError::Permissions);
    }
    Ok(())
}

#[cfg(unix)]
fn check_directory(path: &Path) -> Result<(), AuthError> {
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::symlink_metadata(path).map_err(|_| AuthError::Directory)?;
    if !metadata.is_dir()
        || metadata.mode() & 0o077 != 0
        || metadata.uid() != rustix::process::geteuid().as_raw()
    {
        return Err(AuthError::Directory);
    }
    Ok(())
}

fn read_key(stdin: bool) -> Result<String, AuthError> {
    if stdin {
        if std::io::stdin().is_terminal() {
            return Err(AuthError::StdinTerminal);
        }
        let mut bytes = Vec::new();
        std::io::stdin()
            .lock()
            .take((KEY_BYTES + 3) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| AuthError::Input)?;
        if bytes.ends_with(b"\r\n") {
            bytes.truncate(bytes.len() - 2);
        } else if bytes.ends_with(b"\n") {
            bytes.truncate(bytes.len() - 1);
        }
        let key = String::from_utf8(bytes).map_err(|_| AuthError::Key)?;
        if !valid_credential(&key) {
            return Err(AuthError::Key);
        }
        return Ok(key);
    }
    if !(std::io::stdin().is_terminal() && std::io::stderr().is_terminal()) {
        return Err(AuthError::Terminal);
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| AuthError::Input)?
        .block_on(hidden_prompt())
}

async fn hidden_prompt() -> Result<String, AuthError> {
    let interrupt = tokio::signal::ctrl_c();
    tokio::pin!(interrupt);
    // Install Ctrl+C handling before terminal changes so interruption cannot bypass restoration.
    tokio::select! {
        biased;
        signal = &mut interrupt => {
            signal.map_err(|_| AuthError::Input)?;
            return Err(AuthError::Cancelled);
        }
        () = std::future::ready(()) => {}
    }
    #[cfg(unix)]
    let settings = rustix::termios::tcgetattr(std::io::stdin()).map_err(|_| AuthError::Input)?;
    crossterm::terminal::enable_raw_mode().map_err(|_| AuthError::Input)?;
    let mut terminal = HiddenTerminal {
        armed: true,
        #[cfg(unix)]
        settings,
    };
    let prompt = write!(std::io::stderr(), "API key (hidden): ")
        .and_then(|()| std::io::stderr().flush())
        .map_err(|_| AuthError::Output);
    let result = match prompt {
        Ok(()) => hidden_key(interrupt.as_mut()).await,
        Err(error) => Err(error),
    };
    terminal.restore()?;
    writeln!(std::io::stderr()).map_err(|_| AuthError::Output)?;
    result
}

struct HiddenTerminal {
    armed: bool,
    #[cfg(unix)]
    settings: rustix::termios::Termios,
}

impl HiddenTerminal {
    fn restore(&mut self) -> Result<(), AuthError> {
        if !self.armed {
            return Ok(());
        }
        // Restore and discard unread key bytes together. Re-enabling canonical
        // echo first can expose cancelled input on the shell's next read.
        #[cfg(unix)]
        rustix::termios::tcsetattr(
            std::io::stdin(),
            rustix::termios::OptionalActions::Flush,
            &self.settings,
        )
        .map_err(|_| AuthError::Restore)?;
        crossterm::terminal::disable_raw_mode().map_err(|_| AuthError::Restore)?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for HiddenTerminal {
    fn drop(&mut self) {
        if self.restore().is_err() {
            eprintln!("cocoa: cannot restore terminal settings");
        }
    }
}

async fn hidden_key(
    mut interrupt: std::pin::Pin<&mut impl std::future::Future<Output = std::io::Result<()>>>,
) -> Result<String, AuthError> {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut key = String::new();
    let mut invalid = false;
    while Instant::now() < deadline {
        tokio::select! {
            biased;
            signal = &mut interrupt => {
                signal.map_err(|_| AuthError::Input)?;
                return Err(AuthError::Cancelled);
            }
            () = tokio::time::sleep(Duration::from_millis(10)) => {}
        }
        for _ in 0..64 {
            // The level-triggered TTY reader skips zero-duration polls entirely.
            if !event::poll(Duration::from_micros(100)).map_err(|_| AuthError::Input)? {
                break;
            }
            let Event::Key(event) = event::read().map_err(|_| AuthError::Input)? else {
                continue;
            };
            if event.kind == KeyEventKind::Release {
                continue;
            }
            match event.code {
                KeyCode::Esc => return Err(AuthError::Cancelled),
                KeyCode::Char('c' | 'd') if event.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Err(AuthError::Cancelled);
                }
                KeyCode::Enter => {
                    return if !invalid && valid_credential(&key) {
                        Ok(key)
                    } else {
                        Err(AuthError::Key)
                    };
                }
                KeyCode::Backspace => {
                    key.pop();
                }
                KeyCode::Char(character)
                    if event.modifiers.is_empty() || event.modifiers == KeyModifiers::SHIFT =>
                {
                    // Keep echo disabled until submission even after invalid/oversized
                    // input, so a paste's remaining bytes cannot leak after rejection.
                    if invalid
                        || key.len() + character.len_utf8() > KEY_BYTES
                        || character.is_control()
                    {
                        invalid = true;
                    } else {
                        key.push(character);
                    }
                }
                _ => {}
            }
        }
    }
    Err(AuthError::Cancelled)
}
