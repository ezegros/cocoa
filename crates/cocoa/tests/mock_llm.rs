//! The demo server drives all supported demo providers through the real CLI.

#![cfg(unix)]

#[path = "support/process.rs"]
mod process;
#[path = "support/terminal.rs"]
mod terminal;

use process::CapturedProcess;
use serde_json::json;
use std::{
    error::Error,
    ffi::OsStr,
    fs,
    net::{SocketAddr, TcpListener},
    path::Path,
    process::Command,
    thread,
    time::{Duration, Instant},
};
use terminal::Terminal;

#[test]
fn all_five_demo_providers_execute_cells_in_persistent_cli_sessions() -> Result<(), Box<dyn Error>>
{
    let binary = Path::new(env!("CARGO_BIN_EXE_cocoa"))
        .with_file_name("examples")
        .join("mock_llm");
    let mut server =
        CapturedProcess::spawn(Command::new(binary).args(["--port", "0", "--delay-ms", "0"]))?;
    let deadline = Instant::now() + Duration::from_secs(8);
    let address: SocketAddr = loop {
        if let Some(status) = server.child.try_wait()? {
            let output = server.finish(Duration::from_secs(1))?;
            return Err(format!(
                "mock server exited {status}: {}",
                String::from_utf8_lossy(&output.stderr)
            )
            .into());
        }
        let stdout = server.pump()?;
        if let Some(end) = stdout.iter().position(|byte| *byte == b'\n') {
            let line = std::str::from_utf8(&stdout[..end])?;
            break line
                .strip_prefix("Mock Responses endpoint: http://")
                .and_then(|endpoint| endpoint.strip_suffix("/v1/responses"))
                .ok_or("invalid server readiness message")?
                .parse()?;
        }
        if Instant::now() >= deadline {
            return Err("mock server did not announce readiness".into());
        }
        thread::sleep(Duration::from_millis(10));
    };
    assert!(address.ip().is_loopback());
    assert_ne!(address.port(), 0);
    for (provider, path) in [
        ("openai", "/v1/responses"),
        ("opencode-go", "/inference/go/openai/v1/responses"),
        ("baseten", "/v1/responses"),
        ("openrouter", "/api/v1/responses"),
        ("ollama", "/v1/chat/completions"),
    ] {
        let home = tempfile::tempdir()?;
        let directory = home.path().join(".config/cocoa");
        fs::create_dir_all(&directory)?;
        let mut options = json!({"endpoint":format!("http://{address}{path}")});
        if provider != "ollama" {
            options["credential_env"] = json!("COCOA_MOCK_KEY");
        }
        fs::write(
            directory.join("config.json"),
            serde_json::to_vec(&json!({
                "providers":{provider:options},
                "aliases":{"medium":[{"provider":provider,"model":"demo-model"}]},
                "default_alias":"medium"
            }))?,
        )?;
        let mut session = Terminal::spawn_with_environment(
            home.path(),
            &["calculate"],
            140,
            40,
            None,
            &[("COCOA_MOCK_KEY", OsStr::new("fixture"))],
        )?;
        for invocation in 1..=2 {
            session.wait_text(&format!("\"invocation\":{invocation}"))?;
            let screen = session.screen();
            let encoded = screen
                .lines()
                .map(str::trim)
                .find(|line| {
                    line.starts_with("{\"status\"")
                        && line.contains(&format!("\"invocation\":{invocation}"))
                })
                .ok_or("missing canonical feedback")?;
            let feedback: serde_json::Value = serde_json::from_str(encoded)?;
            assert_eq!(
                feedback,
                json!({"status":"completed", "printed":[
                    format!("QuickJS invocation {invocation}"), {"result":23,"invocation":invocation}
                ]}),
                "{provider}"
            );
            session.until(|session| {
                String::from_utf8_lossy(&session.transcript)
                    .matches("state=done:")
                    .count()
                    >= invocation
            })?;
            if invocation == 1 {
                session.command("calculate again")?;
            }
        }
        session.command("exit")?;
        session.finish(0)?;
    }
    use rustix::process::{Pid, Signal, kill_process};
    let identity =
        Pid::from_raw(i32::try_from(server.child.id())?).ok_or("invalid mock server PID")?;
    kill_process(identity, Signal::INT)?;
    let output = server.finish(Duration::from_secs(3))?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout)?,
        format!("Mock Responses endpoint: http://{address}/v1/responses\n")
    );
    drop(TcpListener::bind(address)?);
    Ok(())
}
