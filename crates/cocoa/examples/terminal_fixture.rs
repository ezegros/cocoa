//! Test-owned session leader that checks terminal restoration before Darwin revokes the PTY.

#[cfg(unix)]
use std::{
    io,
    path::PathBuf,
    process::{Command, ExitCode},
    time::{Duration, Instant},
};

#[cfg(unix)]
use clap::Parser;

#[cfg(unix)]
#[derive(Parser)]
struct Arguments {
    #[arg(long)]
    file_size_limit: Option<u64>,
    binary: PathBuf,
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    arguments: Vec<std::ffi::OsString>,
}

#[cfg(unix)]
#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("terminal fixture: {error}");
            ExitCode::from(125)
        }
    }
}

#[cfg(not(unix))]
fn main() {
    eprintln!("terminal fixture requires Unix");
    std::process::exit(125);
}

#[cfg(unix)]
async fn run() -> io::Result<u8> {
    let arguments = Arguments::parse();
    rustix::process::setsid()?;
    rustix::process::ioctl_tiocsctty(io::stdin())?;
    let _interrupts = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    rustix::io::ioctl_fionread(io::stdin())?;
    let before = terminal_settings()?;
    let mut command = if let Some(limit) = arguments.file_size_limit {
        rustix::process::setrlimit(
            rustix::process::Resource::Fsize,
            rustix::process::Rlimit {
                current: Some(limit),
                maximum: Some(limit),
            },
        )?;
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "trap '' XFSZ; exec \"$@\"", "terminal-fixture"]);
        command.arg(&arguments.binary);
        command
    } else {
        Command::new(&arguments.binary)
    };
    let mut child = command.args(&arguments.arguments).spawn()?;
    std::fs::write(".terminal-child.pid", child.id().to_string())?;
    let deadline = Instant::now() + Duration::from_secs(45);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill()?;
            let cleanup_deadline = Instant::now() + Duration::from_secs(2);
            while child.try_wait()?.is_none() {
                if Instant::now() >= cleanup_deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "terminal executable cleanup could not be confirmed",
                    ));
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "terminal executable did not exit",
            ));
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    // Darwin's kernel-managed PENDIN settles on a readiness query, without consuming input.
    rustix::io::ioctl_fionread(io::stdin())?;
    let after = terminal_settings()?;
    if before != after {
        return Err(io::Error::other(format!(
            "terminal settings changed: before {before}; after {after}"
        )));
    }
    println!("PTY: exact terminal restoration verified");
    use std::os::unix::process::ExitStatusExt;
    let code = status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .ok_or_else(|| io::Error::other("missing child exit status"))?;
    u8::try_from(code).map_err(io::Error::other)
}

#[cfg(unix)]
fn terminal_settings() -> io::Result<String> {
    let settings = rustix::termios::tcgetattr(io::stdin())?;
    Ok(format!(
        "{:?}/{:?}/{:?}/{:?}/{}/{}/{:?}",
        settings.input_modes,
        settings.output_modes,
        settings.control_modes,
        settings.local_modes,
        settings.input_speed(),
        settings.output_speed(),
        settings.special_codes,
    ))
}
