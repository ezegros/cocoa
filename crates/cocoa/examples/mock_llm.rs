//! Loopback-only HTTP transport for the JavaScript mock provider, outside agent runtimes.

use std::{
    collections::BTreeMap,
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    process::ExitCode,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use clap::Parser;
use rquickjs::{CaughtError, Context, Function, Runtime};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

const REQUEST_BYTES_MAX: usize = 4 * 1024 * 1024;
const HEADERS_BYTES_MAX: usize = 64 * 1024;
const RESPONSE_BYTES_MAX: usize = 256 * 1024;
const IO_INTERVAL: Duration = Duration::from_millis(100);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Parser)]
#[command(about = "Run bounded, credential-free provider fixtures for local Cocoa demos.")]
struct Arguments {
    #[arg(long, default_value_t = 8787)]
    port: u16,
    #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u64).range(0..=1000))]
    delay_ms: u64,
}

#[derive(Debug, thiserror::Error)]
enum MockError {
    #[error("mock transport failed: {0}")]
    Io(#[from] io::Error),
    #[error("mock worker failed: {0}")]
    Worker(#[from] tokio::task::JoinError),
    #[error("mock JavaScript failed: {message}")]
    Javascript { message: String },
    #[error("mock produced an invalid response: {0}")]
    Response(#[from] serde_json::Error),
    #[error("mock response exceeds its bounds")]
    ResponseLimit,
    #[error("request rejected with HTTP {status}")]
    Rejected { status: u16 },
}

struct Request {
    path: String,
    headers: BTreeMap<String, String>,
    body: String,
}

#[derive(Deserialize)]
struct Response {
    status: u16,
    events: Vec<Event>,
}

#[derive(Deserialize)]
struct Event {
    data: String,
    delayed: bool,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let arguments = Arguments::parse();
    match run(arguments).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("mock-llm: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(arguments: Arguments) -> Result<(), MockError> {
    let stop = CancellationToken::new();
    let worker_stop = stop.clone();
    let mut worker = tokio::task::spawn_blocking(move || serve(arguments, &worker_stop));
    tokio::select! {
        result = &mut worker => result??,
        signal = tokio::signal::ctrl_c() => {
            stop.cancel();
            let result = worker.await?;
            signal?;
            result?;
        }
    }
    Ok(())
}

fn serve(arguments: Arguments, stop: &CancellationToken) -> Result<(), MockError> {
    let runtime = Runtime::new().map_err(javascript_error)?;
    runtime.set_memory_limit(64 * 1024 * 1024);
    runtime.set_max_stack_size(256 * 1024);
    let context = Context::full(&runtime).map_err(javascript_error)?;
    context.with(|context| {
        let handler: Function = context
            .eval(include_str!("mock_llm.js"))
            .map_err(|error| caught_error(&context, error))?;
        context
            .globals()
            .set("handle_request", handler)
            .map_err(|error| caught_error(&context, error))
    })?;
    let listener = TcpListener::bind(("127.0.0.1", arguments.port))?;
    listener.set_nonblocking(true)?;
    println!(
        "Mock Responses endpoint: http://{}/v1/responses",
        listener.local_addr()?
    );
    io::stdout().flush()?;
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    let mut sequence = 0_u64;
    while !stop.is_cancelled() {
        let mut stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        };
        stream.set_nonblocking(false)?;
        stream.set_read_timeout(Some(IO_INTERVAL))?;
        stream.set_write_timeout(Some(IO_INTERVAL))?;
        sequence = sequence.checked_add(1).ok_or(MockError::ResponseLimit)?;
        let identity = format!("{:032x}", seed.wrapping_add(u128::from(sequence)));
        let response = read_request(&mut stream, stop)
            .and_then(|request| response(&runtime, &context, &request, &identity));
        let result = match response {
            Ok(response) => write_response(&mut stream, response, arguments.delay_ms, stop),
            Err(MockError::Rejected { status }) => write_response(
                &mut stream,
                Response {
                    status,
                    events: Vec::new(),
                },
                0,
                stop,
            ),
            Err(error) => Err(error),
        };
        match result {
            Ok(()) => {}
            Err(MockError::Io(error)) if disconnected(&error) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn read_request(stream: &mut TcpStream, stop: &CancellationToken) -> Result<Request, MockError> {
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    let mut bytes = Vec::with_capacity(8192);
    let mut buffer = [0; 8192];
    let end = loop {
        let count = read(stream, &mut buffer, deadline, stop)?;
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(end) = bytes.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            if end > HEADERS_BYTES_MAX {
                return Err(MockError::Rejected { status: 400 });
            }
            break end;
        }
        if bytes.len() > HEADERS_BYTES_MAX {
            return Err(MockError::Rejected { status: 400 });
        }
    };
    let header_text =
        std::str::from_utf8(&bytes[..end]).map_err(|_| MockError::Rejected { status: 400 })?;
    let mut lines = header_text.split("\r\n");
    let mut request_line = lines.next().unwrap_or_default().split_whitespace();
    let method = request_line.next().unwrap_or_default();
    let path = request_line.next().unwrap_or_default().to_owned();
    if !matches!(request_line.next(), Some("HTTP/1.0" | "HTTP/1.1"))
        || request_line.next().is_some()
    {
        return Err(MockError::Rejected { status: 400 });
    }
    if method != "POST" {
        return Err(MockError::Rejected { status: 501 });
    }
    let mut headers = BTreeMap::new();
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .ok_or(MockError::Rejected { status: 400 })?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            || headers
                .insert(name.to_ascii_lowercase(), value.trim().to_owned())
                .is_some()
        {
            return Err(MockError::Rejected { status: 400 });
        }
    }
    if headers.contains_key("transfer-encoding") {
        return Err(MockError::Rejected { status: 400 });
    }
    let length = headers.get("content-length").map_or("0", String::as_str);
    let digits = length
        .strip_prefix('+')
        .or_else(|| length.strip_prefix('-'))
        .unwrap_or(length);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(MockError::Rejected { status: 400 });
    }
    let size: usize = digits
        .parse()
        .ok()
        .filter(|_| !length.starts_with('-'))
        .filter(|size| (1..=REQUEST_BYTES_MAX).contains(size))
        .ok_or(MockError::Rejected { status: 413 })?;
    if headers
        .get("expect")
        .is_some_and(|value| value.eq_ignore_ascii_case("100-continue"))
    {
        write(stream, b"HTTP/1.1 100 Continue\r\n\r\n", stop)?;
    }
    let total = end + 4 + size;
    while bytes.len() < total {
        let count = read(stream, &mut buffer, deadline, stop).map_err(|error| {
            if error.kind() == io::ErrorKind::UnexpectedEof {
                MockError::Rejected { status: 400 }
            } else {
                MockError::Io(error)
            }
        })?;
        bytes.extend_from_slice(&buffer[..count]);
    }
    let body = std::str::from_utf8(&bytes[end + 4..total])
        .map_err(|_| MockError::Rejected { status: 400 })?
        .to_owned();
    Ok(Request {
        path,
        headers,
        body,
    })
}

fn response(
    runtime: &Runtime,
    context: &Context,
    request: &Request,
    identity: &str,
) -> Result<Response, MockError> {
    let headers = serde_json::to_string(&request.headers)?;
    let session = request
        .headers
        .get("x-opencode-session")
        .map_or("", String::as_str);
    let digest: String = Sha256::digest(session.as_bytes())
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let deadline = Instant::now() + Duration::from_secs(5);
    runtime.set_interrupt_handler(Some(Box::new(move || Instant::now() >= deadline)));
    let result = context.with(|context| {
        let handler: Function = context
            .globals()
            .get("handle_request")
            .map_err(|error| caught_error(&context, error))?;
        handler
            .call::<_, String>((
                request.path.as_str(),
                headers.as_str(),
                request.body.as_str(),
                identity,
                digest.as_str(),
            ))
            .map_err(|error| caught_error(&context, error))
    });
    runtime.set_interrupt_handler(None);
    let encoded = result?;
    if encoded.len() > RESPONSE_BYTES_MAX {
        return Err(MockError::ResponseLimit);
    }
    let response: Response = serde_json::from_str(&encoded)?;
    if !matches!(response.status, 200 | 400 | 401 | 404)
        || response.events.len() > 256
        || response
            .events
            .iter()
            .any(|event| event.data.contains(['\r', '\n']))
    {
        return Err(MockError::ResponseLimit);
    }
    Ok(response)
}

fn write_response(
    stream: &mut TcpStream,
    response: Response,
    delay_ms: u64,
    stop: &CancellationToken,
) -> Result<(), MockError> {
    let headers = if response.status == 200 {
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\nConnection: close\r\n\r\n".to_owned()
    } else {
        format!(
            "HTTP/1.1 {} Rejected\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            response.status
        )
    };
    write(stream, headers.as_bytes(), stop)?;
    for event in response.events {
        write(stream, format!("data: {}\n\n", event.data).as_bytes(), stop)?;
        if event.delayed {
            let deadline = Instant::now() + Duration::from_millis(delay_ms);
            while !stop.is_cancelled() && Instant::now() < deadline {
                thread::sleep(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(IO_INTERVAL),
                );
            }
        }
    }
    Ok(())
}

fn read(
    stream: &mut TcpStream,
    bytes: &mut [u8],
    deadline: Instant,
    stop: &CancellationToken,
) -> io::Result<usize> {
    loop {
        check_deadline(deadline, stop)?;
        match stream.read(bytes) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(count) => return Ok(count),
            Err(error) if retryable(&error) => {}
            Err(error) => return Err(error),
        }
    }
}

fn write(stream: &mut TcpStream, bytes: &[u8], stop: &CancellationToken) -> io::Result<()> {
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    let mut remaining = bytes;
    while !remaining.is_empty() {
        check_deadline(deadline, stop)?;
        match stream.write(remaining) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(count) => remaining = &remaining[count..],
            Err(error) if retryable(&error) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn check_deadline(deadline: Instant, stop: &CancellationToken) -> io::Result<()> {
    if stop.is_cancelled() {
        Err(io::ErrorKind::Interrupted.into())
    } else if Instant::now() >= deadline {
        Err(io::ErrorKind::TimedOut.into())
    } else {
        Ok(())
    }
}

fn retryable(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

fn disconnected(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::TimedOut
            | io::ErrorKind::Interrupted
    )
}

fn javascript_error(error: rquickjs::Error) -> MockError {
    MockError::Javascript {
        message: error.to_string(),
    }
}

fn caught_error(context: &rquickjs::Ctx<'_>, error: rquickjs::Error) -> MockError {
    MockError::Javascript {
        message: CaughtError::from_error(context, error).to_string(),
    }
}
