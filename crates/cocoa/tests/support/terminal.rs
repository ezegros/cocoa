//! Bounded PTY sessions shared by the executable's integration tests.

#![allow(dead_code)]

use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};

use rustix::{
    event::{PollFd, PollFlags, Timespec, poll},
    fs::{OFlags, fcntl_getfl, fcntl_setfl},
    io::{FdFlags, fcntl_setfd},
    process::{Pid, Signal, kill_process, kill_process_group},
    pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt},
    termios::{LocalModes, Winsize, tcgetattr, tcsetwinsize},
};

pub struct Terminal {
    pub home: PathBuf,
    pub transcript: Vec<u8>,
    master: File,
    slave: File,
    child: Child,
    status: Option<ExitStatus>,
    width: u16,
    height: u16,
}

impl Terminal {
    pub fn spawn(
        home: &Path,
        arguments: &[&str],
        width: u16,
        height: u16,
        file_size_limit: Option<u64>,
    ) -> io::Result<Self> {
        Self::spawn_with_environment(home, arguments, width, height, file_size_limit, &[])
    }

    pub fn spawn_with_environment(
        home: &Path,
        arguments: &[&str],
        width: u16,
        height: u16,
        file_size_limit: Option<u64>,
        environment: &[(&str, &std::ffi::OsStr)],
    ) -> io::Result<Self> {
        assert!(
            width > 0 && height > 0,
            "a terminal must have visible cells"
        );
        let descriptor = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY)?;
        fcntl_setfd(&descriptor, FdFlags::CLOEXEC)?;
        grantpt(&descriptor)?;
        unlockpt(&descriptor)?;
        let name = ptsname(&descriptor, Vec::new())?;
        use std::os::unix::ffi::OsStrExt;
        let slave = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(OFlags::NOCTTY.bits() as i32)
            .open(std::ffi::OsStr::from_bytes(name.to_bytes()))?;
        tcsetwinsize(
            &slave,
            Winsize {
                ws_row: height,
                ws_col: width,
                ws_xpixel: 0,
                ws_ypixel: 0,
            },
        )?;
        let master = File::from(descriptor);
        fcntl_setfl(&master, fcntl_getfl(&master)? | OFlags::NONBLOCK)?;
        let fixture = Path::new(env!("CARGO_BIN_EXE_cocoa"))
            .with_file_name("examples")
            .join("terminal_fixture");
        let mut command = Command::new(fixture);
        if let Some(limit) = file_size_limit {
            command.arg("--file-size-limit").arg(limit.to_string());
        }
        let child = command
            .arg(env!("CARGO_BIN_EXE_cocoa"))
            .args(arguments)
            .env_clear()
            .env("HOME", home)
            .env("TERM", "xterm-256color")
            .env("PATH", "/usr/bin:/bin")
            .env("COCOA_TEST_KEY", "fixture-key")
            .env("FIXTURE_KEY", "fixture-private-key")
            .envs(environment.iter().copied())
            .current_dir(home)
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave.try_clone()?))
            .spawn()?;
        Ok(Self {
            home: home.to_path_buf(),
            transcript: Vec::new(),
            master,
            slave,
            child,
            status: None,
            width,
            height,
        })
    }

    pub fn pump(&mut self) -> io::Result<()> {
        let mut descriptors = [PollFd::new(&self.master, PollFlags::IN)];
        match poll(
            &mut descriptors,
            Some(&Timespec {
                tv_sec: 0,
                tv_nsec: 20_000_000,
            }),
        ) {
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => return Ok(()),
            Err(error) => return Err(error.into()),
        }
        let mut buffer = [0; 8192];
        for _ in 0..16 {
            match self.master.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    if self.transcript.len() + count > 2 * 1024 * 1024 {
                        return Err(io::Error::other("terminal output exceeds two MiB"));
                    }
                    self.transcript.extend_from_slice(&buffer[..count]);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error)
                    if error.raw_os_error() == Some(rustix::io::Errno::IO.raw_os_error()) =>
                {
                    break;
                }
                Err(error) => return Err(error),
            }
        }
        self.status = self.child.try_wait()?;
        Ok(())
    }

    pub fn until(&mut self, mut predicate: impl FnMut(&Self) -> bool) -> io::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !predicate(self) {
            self.pump()?;
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "terminal condition did not settle; screen:\n{}",
                        self.screen()
                    ),
                ));
            }
        }
        Ok(())
    }

    pub fn send(&mut self, bytes: &[u8]) -> io::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut remaining = bytes;
        while !remaining.is_empty() {
            if Instant::now() >= deadline {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "PTY input stalled"));
            }
            match self.master.write(remaining) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(count) => remaining = &remaining[count..],
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    let mut descriptors =
                        [PollFd::new(&self.master, PollFlags::IN | PollFlags::OUT)];
                    match poll(
                        &mut descriptors,
                        Some(&Timespec {
                            tv_sec: 0,
                            tv_nsec: 20_000_000,
                        }),
                    ) {
                        Ok(_) => {}
                        Err(rustix::io::Errno::INTR) => continue,
                        Err(error) => return Err(error.into()),
                    }
                    if descriptors[0].revents().contains(PollFlags::IN) {
                        self.pump()?;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    pub fn key(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.send(bytes)?;
        let deadline = Instant::now() + Duration::from_millis(130);
        while Instant::now() < deadline {
            self.pump()?;
        }
        Ok(())
    }

    pub fn text(&mut self, text: &str) -> io::Result<()> {
        for chunk in text.as_bytes().chunks(64) {
            self.send(chunk)?;
            self.pump()?;
        }
        Ok(())
    }

    pub fn command(&mut self, text: &str) -> io::Result<()> {
        self.text(text)?;
        self.key(b"\r")
    }

    pub fn wait_text(&mut self, text: &str) -> io::Result<()> {
        self.until(|terminal| terminal.contains(text))
    }

    pub fn contains(&self, text: &str) -> bool {
        let expected: String = text
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect();
        let screen: String = self
            .screen()
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect();
        screen.contains(&expected)
    }

    pub fn screen(&self) -> String {
        let width = usize::from(self.width);
        let height = usize::from(self.height);
        let mut rows = vec![vec![' '; width]; height];
        let (mut row, mut column) = (0usize, 0usize);
        let text = String::from_utf8_lossy(&self.transcript);
        let mut characters = text.chars().peekable();
        while let Some(character) = characters.next() {
            match character {
                '\x1b' => match characters.next() {
                    Some(']') => {
                        while let Some(character) = characters.next() {
                            if character == '\x07' {
                                break;
                            }
                            if character == '\x1b' && characters.peek() == Some(&'\\') {
                                characters.next();
                                break;
                            }
                        }
                    }
                    Some('[') => {
                        let mut parameters = String::new();
                        for character in characters.by_ref() {
                            if ('@'..='~').contains(&character) {
                                let numbers: Vec<usize> = parameters
                                    .split(';')
                                    .map(|number| number.parse().unwrap_or(1))
                                    .collect();
                                match character {
                                    'H' | 'f' => {
                                        row = numbers[0].saturating_sub(1).min(height - 1);
                                        column = numbers
                                            .get(1)
                                            .copied()
                                            .unwrap_or(1)
                                            .saturating_sub(1)
                                            .min(width - 1);
                                    }
                                    'G' => column = numbers[0].saturating_sub(1).min(width - 1),
                                    'J' if matches!(parameters.as_str(), "2" | "3") => {
                                        rows.iter_mut().for_each(|line| line.fill(' '));
                                    }
                                    'K' => {
                                        let start = if matches!(parameters.as_str(), "1" | "2") {
                                            0
                                        } else {
                                            column
                                        };
                                        let end =
                                            if parameters == "1" { column + 1 } else { width };
                                        rows[row][start..end].fill(' ');
                                    }
                                    'h' if parameters == "?1049" => {
                                        rows.iter_mut().for_each(|line| line.fill(' '));
                                    }
                                    _ => {}
                                }
                                break;
                            }
                            parameters.push(character);
                        }
                    }
                    Some('(' | ')') => {
                        characters.next();
                    }
                    _ => {}
                },
                '\r' => column = 0,
                '\n' => row = (row + 1).min(height - 1),
                character if !character.is_control() => {
                    rows[row][column] = character;
                    column = (column + 1).min(width - 1);
                }
                _ => {}
            }
        }
        rows.into_iter()
            .map(|line| line.into_iter().collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn resize(&mut self, width: u16, height: u16) -> io::Result<()> {
        assert!(
            width > 0 && height > 0,
            "a terminal must have visible cells"
        );
        tcsetwinsize(
            &self.slave,
            Winsize {
                ws_row: height,
                ws_col: width,
                ws_xpixel: 0,
                ws_ypixel: 0,
            },
        )?;
        self.width = width;
        self.height = height;
        Ok(())
    }

    pub fn reports(&self) -> io::Result<Vec<BTreeMap<String, String>>> {
        let text = String::from_utf8_lossy(&self.transcript);
        let mut remaining = text.as_ref();
        let mut reports = Vec::new();
        while let Some(start) = remaining.find("\x1b]7501;") {
            remaining = &remaining[start + 7..];
            let Some(end) = remaining.find(['\x1b', '\x07']) else {
                break;
            };
            let terminator = if remaining.as_bytes()[end] == 7 {
                1
            } else {
                if remaining.len() == end + 1 {
                    break;
                }
                if remaining.as_bytes()[end + 1] != b'\\' {
                    return Err(io::Error::other("invalid OSC terminator"));
                }
                2
            };
            let payload = &remaining[..end];
            if payload.len() + 9 > 4096 {
                return Err(io::Error::other("status report exceeds byte limit"));
            }
            let mut report = BTreeMap::new();
            for pair in payload.split(':') {
                let (name, value) = pair
                    .split_once('=')
                    .ok_or_else(|| io::Error::other("invalid status report field"))?;
                if name.is_empty()
                    || name.len() > 16
                    || !name.bytes().all(|byte| byte.is_ascii_lowercase())
                    || !value
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"_.,+/=-".contains(&byte))
                    || report.insert(name.to_owned(), value.to_owned()).is_some()
                {
                    return Err(io::Error::other("invalid status report encoding"));
                }
            }
            reports.push(report);
            remaining = &remaining[end + terminator..];
        }
        Ok(reports)
    }

    pub fn states(&self) -> io::Result<Vec<String>> {
        self.reports()?
            .into_iter()
            .map(|mut report| {
                report
                    .remove("state")
                    .ok_or_else(|| io::Error::other("status report has no state"))
            })
            .collect()
    }

    pub fn exited(&self) -> bool {
        self.status.is_some()
    }

    pub fn echo_enabled(&self) -> io::Result<bool> {
        Ok(tcgetattr(&self.slave)?
            .local_modes
            .contains(LocalModes::ECHO))
    }

    pub fn interrupt(&self) -> io::Result<()> {
        let value = std::fs::read_to_string(self.home.join(".terminal-child.pid"))?;
        let identity = value.parse::<i32>().map_err(io::Error::other)?;
        let identity = Pid::from_raw(identity)
            .ok_or_else(|| io::Error::other("invalid terminal child PID"))?;
        kill_process(identity, Signal::INT)?;
        Ok(())
    }

    pub fn interrupt_group(&self) -> io::Result<()> {
        kill_process_group(self.group()?, Signal::INT)?;
        Ok(())
    }

    pub fn finish(&mut self, expected: i32) -> io::Result<()> {
        self.until(Self::exited)?;
        let deadline = Instant::now() + Duration::from_millis(100);
        while Instant::now() < deadline {
            self.pump()?;
        }
        let status = self
            .status
            .ok_or_else(|| io::Error::other("missing PTY status"))?;
        let text = String::from_utf8_lossy(&self.transcript);
        if status.code() != Some(expected) {
            return Err(io::Error::other(format!(
                "unexpected terminal exit {status}: {text}"
            )));
        }
        if !text.contains("PTY: exact terminal restoration verified") {
            return Err(io::Error::other(format!(
                "terminal restoration was not verified: {text}"
            )));
        }
        Ok(())
    }

    fn group(&self) -> io::Result<Pid> {
        let identity = i32::try_from(self.child.id()).map_err(io::Error::other)?;
        Pid::from_raw(identity).ok_or_else(|| io::Error::other("invalid terminal process group"))
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if self.status.is_some() {
            return;
        }
        match self.group() {
            Ok(group) => {
                if let Err(error) = kill_process_group(group, Signal::KILL)
                    && error != rustix::io::Errno::SRCH
                {
                    eprintln!("could not terminate PTY process group: {error}");
                }
            }
            Err(error) => eprintln!("could not identify PTY process group: {error}"),
        }
        if let Err(error) = self.child.kill()
            && error.kind() != io::ErrorKind::InvalidInput
        {
            eprintln!("could not terminate PTY fixture: {error}");
        }
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    // macOS keeps an exiting session leader alive until its
                    // terminal output drains, so keep reading the master.
                    if let Err(error) = self.pump() {
                        eprintln!("could not drain PTY output: {error}");
                        break;
                    }
                }
                Ok(None) => {
                    eprintln!("PTY fixture did not exit after termination");
                    break;
                }
                Err(error) => {
                    eprintln!("could not reap PTY fixture: {error}");
                    break;
                }
            }
        }
    }
}
