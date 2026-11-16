//! Bounded child-process output and cleanup for command integration tests.

#![allow(dead_code)]

use std::{
    io::{self, Read},
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};

/// Maximum wait for a captured command to exit and close both output pipes.
pub const EXIT_TIMEOUT: Duration = Duration::from_secs(60);

const OUTPUT_BYTES_MAX: usize = 4 * 1024 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(10);

pub trait CommandExt {
    /// Capture both output streams within `EXIT_TIMEOUT`, preserving configured
    /// stdin. Unconfigured stdin is inherited; piped stdin is closed after spawn.
    ///
    /// # Errors
    /// Report spawn, pipe, capture-limit, or exit/EOF timeout failures.
    fn output_bounded(&mut self) -> io::Result<Output>;
}

impl CommandExt for Command {
    fn output_bounded(&mut self) -> io::Result<Output> {
        CapturedProcess::try_from(self.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?)?
            .finish(EXIT_TIMEOUT)
    }
}

pub struct CapturedProcess {
    pub child: Child,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl CapturedProcess {
    /// Spawn with closed stdin and nonblocking, bounded stdout/stderr capture.
    ///
    /// # Errors
    /// Report process creation or pipe configuration failures.
    pub fn spawn(command: &mut Command) -> io::Result<Self> {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?
            .try_into()
    }

    /// Drain available output and return stdout captured so far.
    ///
    /// # Errors
    /// Reject I/O failures or either stream exceeding 4 MiB.
    pub fn pump(&mut self) -> io::Result<&[u8]> {
        if let Some(pipe) = self.child.stdout.as_mut()
            && read_available(pipe, &mut self.stdout)?
        {
            self.child.stdout = None;
        }
        if let Some(pipe) = self.child.stderr.as_mut()
            && read_available(pipe, &mut self.stderr)?
        {
            self.child.stderr = None;
        }
        Ok(&self.stdout)
    }

    /// Wait for exit and EOF while draining both streams. Drop stops a live child.
    ///
    /// # Errors
    /// Report output failures or failure to exit and close its pipes in time.
    pub fn finish(&mut self, timeout: Duration) -> io::Result<Output> {
        self.child.stdin = None;
        let deadline = Instant::now() + timeout;
        loop {
            self.pump()?;
            if let Some(status) = self.child.try_wait()?
                && self.child.stdout.is_none()
                && self.child.stderr.is_none()
            {
                return Ok(Output {
                    status,
                    stdout: std::mem::take(&mut self.stdout),
                    stderr: std::mem::take(&mut self.stderr),
                });
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("process {} exceeded {timeout:?}", self.child.id()),
                ));
            }
            thread::sleep(POLL_INTERVAL);
        }
    }
}

impl TryFrom<Child> for CapturedProcess {
    type Error = io::Error;

    fn try_from(child: Child) -> io::Result<Self> {
        let process = Self {
            child,
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        if let Some(stdout) = &process.child.stdout {
            fcntl_setfl(stdout, fcntl_getfl(stdout)? | OFlags::NONBLOCK)?;
        }
        if let Some(stderr) = &process.child.stderr {
            fcntl_setfl(stderr, fcntl_getfl(stderr)? | OFlags::NONBLOCK)?;
        }
        Ok(process)
    }
}

impl Drop for CapturedProcess {
    fn drop(&mut self) {
        match self.child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => {}
            Err(error) => eprintln!("cannot inspect test child: {error}"),
        }
        if let Err(error) = self.child.kill() {
            eprintln!("cannot stop test child: {error}");
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => thread::sleep(POLL_INTERVAL),
                Err(error) => {
                    eprintln!("cannot reap test child: {error}");
                    return;
                }
            }
        }
        eprintln!("test child {} did not stop", self.child.id());
    }
}

fn read_available(reader: &mut impl Read, output: &mut Vec<u8>) -> io::Result<bool> {
    let mut buffer = [0; 8192];
    for _ in 0..64 {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(true),
            Ok(length) => {
                if output.len() + length > OUTPUT_BYTES_MAX {
                    return Err(io::Error::other("test child exceeded capture limit"));
                }
                output.extend_from_slice(&buffer[..length]);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}
