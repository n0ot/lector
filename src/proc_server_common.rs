use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::{self, Read, Write};
use std::sync::LazyLock;

pub const SPEECH_PROTOCOL_VERSION: &str = "1.0";
pub const MAX_RPC_FRAME_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PeerInfo {
    pub name: String,
    pub version: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InitializeParams {
    pub protocol_version: String,
    pub client: PeerInfo,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SpeechCapabilities {
    pub speak: bool,
    pub stop: bool,
    pub set_rate: bool,
    pub rpc_discover: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InitializeResult {
    pub protocol_version: String,
    pub server: PeerInfo,
    pub capabilities: SpeechCapabilities,
}

pub fn handle_protocol_request(
    request: &Request,
    server_name: &str,
    server_version: &str,
) -> Option<std::result::Result<Value, RpcError>> {
    match request.method.as_str() {
        "initialize" => Some(initialize(request, server_name, server_version)),
        "rpc.discover" => Some(discover(request, server_name, server_version)),
        _ => None,
    }
}

fn initialize(
    request: &Request,
    server_name: &str,
    server_version: &str,
) -> std::result::Result<Value, RpcError> {
    let params: InitializeParams = serde_json::from_value(
        request
            .params
            .clone()
            .ok_or_else(|| RpcError::invalid_params("missing params"))?,
    )
    .map_err(|error| RpcError::invalid_params(error.to_string()))?;
    if params.protocol_version != SPEECH_PROTOCOL_VERSION {
        return Err(RpcError::unsupported_protocol_version(format!(
            "unsupported speech protocol version {:?}",
            params.protocol_version
        )));
    }
    if params.client.name.is_empty() || params.client.version.is_empty() {
        return Err(RpcError::invalid_params(
            "client name and version must not be empty",
        ));
    }
    serde_json::to_value(InitializeResult {
        protocol_version: SPEECH_PROTOCOL_VERSION.to_owned(),
        server: PeerInfo {
            name: server_name.to_owned(),
            version: server_version.to_owned(),
        },
        capabilities: SpeechCapabilities {
            speak: true,
            stop: true,
            set_rate: true,
            rpc_discover: true,
        },
    })
    .map_err(|error| RpcError::internal_error(error.to_string()))
}

fn discover(
    request: &Request,
    server_name: &str,
    server_version: &str,
) -> std::result::Result<Value, RpcError> {
    if !matches!(request.params, None | Some(Value::Null)) {
        return Err(RpcError::invalid_params(
            "rpc.discover does not accept params",
        ));
    }
    Ok(openrpc_document(server_name, server_version))
}

#[must_use]
pub fn openrpc_document(server_name: &str, server_version: &str) -> Value {
    let _ = (server_name, server_version);
    static DOCUMENT: LazyLock<Value> = LazyLock::new(|| {
        serde_json::from_str(include_str!("../openrpc.json"))
            .expect("the checked-in OpenRPC document must be valid JSON")
    });
    DOCUMENT.clone()
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("{operation}")]
    Io {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("{operation}")]
    Json {
        operation: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error("RPC response frame exceeds {MAX_RPC_FRAME_BYTES} bytes")]
    ResponseFrameTooLarge,
}

fn io_error(operation: &'static str) -> impl FnOnce(io::Error) -> Error {
    move |source| Error::Io { operation, source }
}

fn json_error(operation: &'static str) -> impl FnOnce(serde_json::Error) -> Error {
    move |source| Error::Json { operation, source }
}

#[derive(Debug)]
pub struct Request {
    pub id: Option<u64>,
    pub method: String,
    pub params: Option<Value>,
}

#[derive(Debug, thiserror::Error)]
#[error("RPC error {code}: {message}")]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl RpcError {
    fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn parse_error(message: impl Into<String>) -> Self {
        Self::new(-32700, message)
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(-32600, message)
    }

    pub fn method_not_found(method: impl Into<String>) -> Self {
        Self::new(-32601, format!("method not found: {}", method.into()))
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(-32602, message)
    }

    pub fn unsupported_protocol_version(message: impl Into<String>) -> Self {
        Self::new(-32001, message)
    }

    pub fn internal_error(message: impl Into<String>) -> Self {
        Self::new(-32603, message)
    }
}

pub fn run_server<F>(mut handler: F) -> Result<()>
where
    F: FnMut(Request) -> std::result::Result<Value, RpcError>,
{
    run_server_with_tick(&mut handler, || {})
}

pub fn run_server_with_tick<F, T>(mut handler: F, mut tick: T) -> Result<()>
where
    F: FnMut(Request) -> std::result::Result<Value, RpcError>,
    T: FnMut(),
{
    #[cfg(target_os = "macos")]
    {
        run_server_macos(&mut handler, &mut tick)
    }
    #[cfg(not(target_os = "macos"))]
    {
        run_server_blocking(&mut handler, &mut tick)
    }
}

#[cfg(not(target_os = "macos"))]
fn run_server_blocking<F, T>(handler: &mut F, tick: &mut T) -> Result<()>
where
    F: FnMut(Request) -> std::result::Result<Value, RpcError>,
    T: FnMut(),
{
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    let mut stdin = stdin.lock();
    let mut frames = FrameBuffer::new();
    let mut chunk = [0u8; 4096];
    loop {
        match stdin.read(&mut chunk) {
            Ok(0) => return Ok(()),
            Ok(read) => frames.push(&chunk[..read], |frame| {
                handle_frame(frame, handler, &mut stdout)?;
                tick();
                Ok(())
            })?,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(io_error("read stdin")(error)),
        }
    }
}

#[cfg(target_os = "macos")]
fn run_server_macos<F, T>(handler: &mut F, tick: &mut T) -> Result<()>
where
    F: FnMut(Request) -> std::result::Result<Value, RpcError>,
    T: FnMut(),
{
    use crate::platform;
    use mio::{Events, Interest, Poll, Token};
    use nix::fcntl::{FcntlArg, OFlag, fcntl};
    use std::os::fd::AsRawFd;

    let mut poll = Poll::new().map_err(io_error("create poll"))?;
    let mut events = Events::with_capacity(8);
    let mut stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    let stdin_fd = stdin.as_raw_fd();
    let flags = fcntl(stdin_fd, FcntlArg::F_GETFL).map_err(|error| Error::Io {
        operation: "read stdin flags",
        source: error.into(),
    })?;
    fcntl(
        stdin_fd,
        FcntlArg::F_SETFL(OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK),
    )
    .map_err(|error| Error::Io {
        operation: "make stdin nonblocking",
        source: error.into(),
    })?;
    poll.registry()
        .register(
            &mut mio::unix::SourceFd(&stdin_fd),
            Token(0),
            Interest::READABLE,
        )
        .map_err(io_error("register stdin poll source"))?;
    let mut frames = FrameBuffer::new();
    loop {
        poll.poll(&mut events, platform::adjust_poll_timeout(None))
            .map_err(io_error("poll stdin"))?;
        for event in &events {
            if event.token() != Token(0) {
                continue;
            }
            loop {
                let mut chunk = [0u8; 4096];
                match stdin.read(&mut chunk) {
                    Ok(0) => return Ok(()),
                    Ok(read) => frames.push(&chunk[..read], |frame| {
                        handle_frame(frame, handler, &mut stdout)?;
                        // A synchronous client may send its next call as soon
                        // as this response flushes, keeping the pipe readable.
                        // AVFoundation still needs a run-loop turn between
                        // speech calls rather than only after the pipe drains.
                        platform::tick_runloop();
                        tick();
                        Ok(())
                    })?,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) => return Err(io_error("read stdin")(error)),
                }
            }
        }
        platform::tick_runloop();
        tick();
    }
}

enum Frame<'a> {
    Line(&'a [u8]),
    TooLarge,
}

struct FrameBuffer {
    bytes: Vec<u8>,
    discarding: bool,
}

impl FrameBuffer {
    fn new() -> Self {
        Self {
            bytes: Vec::with_capacity(4096),
            discarding: false,
        }
    }

    fn push<F>(&mut self, mut input: &[u8], mut handle: F) -> Result<()>
    where
        F: FnMut(Frame<'_>) -> Result<()>,
    {
        while !input.is_empty() {
            let newline = input.iter().position(|byte| *byte == b'\n');
            let consumed = newline.map_or(input.len(), |position| position + 1);
            let segment = &input[..consumed];
            input = &input[consumed..];

            if self.discarding {
                if newline.is_some() {
                    self.discarding = false;
                    handle(Frame::TooLarge)?;
                }
                continue;
            }

            if self.bytes.len().saturating_add(segment.len()) > MAX_RPC_FRAME_BYTES {
                self.bytes.clear();
                if newline.is_some() {
                    handle(Frame::TooLarge)?;
                } else {
                    self.discarding = true;
                }
                continue;
            }

            self.bytes.extend_from_slice(segment);
            if newline.is_some() {
                handle(Frame::Line(&self.bytes))?;
                self.bytes.clear();
            }
        }
        Ok(())
    }
}

fn handle_frame<F>(frame: Frame<'_>, handler: &mut F, stdout: &mut dyn Write) -> Result<()>
where
    F: FnMut(Request) -> std::result::Result<Value, RpcError>,
{
    match frame {
        Frame::Line(bytes) => match std::str::from_utf8(bytes) {
            Ok(line) => handle_line(line, handler, stdout),
            Err(error) => write_error(stdout, None, &RpcError::parse_error(error.to_string())),
        },
        Frame::TooLarge => write_error(
            stdout,
            None,
            &RpcError::invalid_request(format!(
                "request frame exceeds {MAX_RPC_FRAME_BYTES} bytes"
            )),
        ),
    }
}

fn handle_line<F>(line: &str, handler: &mut F, stdout: &mut dyn Write) -> Result<()>
where
    F: FnMut(Request) -> std::result::Result<Value, RpcError>,
{
    if line.trim().is_empty() {
        return Ok(());
    }
    let request = match parse_request(line) {
        Ok(request) => request,
        Err(err) => {
            write_error(stdout, None, &err)?;
            return Ok(());
        }
    };
    let id = request.id;
    let result = handler(request);
    if let Some(id) = id {
        match result {
            Ok(value) => write_result(stdout, id, value)?,
            Err(err) => write_error(stdout, Some(id), &err)?,
        }
    }
    Ok(())
}

fn parse_request(line: &str) -> std::result::Result<Request, RpcError> {
    let value: Value =
        serde_json::from_str(line).map_err(|e| RpcError::parse_error(e.to_string()))?;
    let obj = value
        .as_object()
        .ok_or_else(|| RpcError::invalid_request("request must be an object"))?;
    let jsonrpc = obj
        .get("jsonrpc")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_request("missing jsonrpc"))?;
    if jsonrpc != "2.0" {
        return Err(RpcError::invalid_request("jsonrpc must be 2.0"));
    }
    let method = obj
        .get("method")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_request("missing method"))?;
    let id = match obj.get("id") {
        Some(Value::Number(n)) => Some(
            n.as_u64()
                .ok_or_else(|| RpcError::invalid_request("id must be an unsigned integer"))?,
        ),
        Some(Value::Null) | None => None,
        Some(_) => return Err(RpcError::invalid_request("id must be a number or null")),
    };
    let params = obj.get("params").cloned();
    Ok(Request {
        id,
        method: method.to_string(),
        params,
    })
}

fn write_result(stdout: &mut dyn Write, id: u64, result: Value) -> Result<()> {
    let response = json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    });
    write_response(stdout, &response)
}

fn write_error(stdout: &mut dyn Write, id: Option<u64>, err: &RpcError) -> Result<()> {
    let response = json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": err.code,
            "message": err.message,
            "data": err.data,
        }
    });
    write_response(stdout, &response)
}

fn write_response(stdout: &mut dyn Write, response: &Value) -> Result<()> {
    let mut frame = serde_json::to_vec(response).map_err(json_error("write RPC response"))?;
    if frame.len().saturating_add(1) > MAX_RPC_FRAME_BYTES {
        return Err(Error::ResponseFrameTooLarge);
    }
    frame.push(b'\n');
    stdout
        .write_all(&frame)
        .map_err(io_error("write RPC response"))?;
    stdout.flush().map_err(io_error("flush RPC response"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        FrameBuffer, MAX_RPC_FRAME_BYTES, Request, RpcError, handle_frame, handle_line,
        parse_request,
    };
    use serde_json::{Value, json};

    #[test]
    fn parses_calls_and_notifications() {
        let call =
            parse_request(r#"{"jsonrpc":"2.0","id":7,"method":"speak","params":{"text":"hi"}}"#)
                .expect("parse call");
        assert_eq!(call.id, Some(7));
        assert_eq!(call.method, "speak");
        assert_eq!(call.params, Some(json!({"text": "hi"})));

        let notification =
            parse_request(r#"{"jsonrpc":"2.0","method":"stop"}"#).expect("parse notification");
        assert_eq!(notification.id, None);
        assert_eq!(notification.method, "stop");
    }

    #[test]
    fn rejects_invalid_json_rpc_envelopes() {
        assert_eq!(parse_request("not json").unwrap_err().code, -32700);
        assert_eq!(parse_request("[]").unwrap_err().code, -32600);
        assert_eq!(
            parse_request(r#"{"jsonrpc":"1.0","method":"stop"}"#)
                .unwrap_err()
                .code,
            -32600
        );
        assert_eq!(
            parse_request(r#"{"jsonrpc":"2.0","method":"stop","id":"bad"}"#)
                .unwrap_err()
                .code,
            -32600
        );
    }

    #[test]
    fn writes_success_error_and_no_notification_response() {
        let mut output = Vec::new();
        let mut handler = |request: Request| -> Result<Value, RpcError> {
            match request.method.as_str() {
                "ok" => Ok(json!(true)),
                method => Err(RpcError::method_not_found(method)),
            }
        };

        handle_line(
            r#"{"jsonrpc":"2.0","id":1,"method":"ok"}"#,
            &mut handler,
            &mut output,
        )
        .expect("handle success");
        handle_line(
            r#"{"jsonrpc":"2.0","id":2,"method":"bad"}"#,
            &mut handler,
            &mut output,
        )
        .expect("handle error");
        handle_line(
            r#"{"jsonrpc":"2.0","method":"ok"}"#,
            &mut handler,
            &mut output,
        )
        .expect("handle notification");

        let responses: Vec<Value> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0], json!({"jsonrpc":"2.0","id":1,"result":true}));
        assert_eq!(responses[1]["id"], 2);
        assert_eq!(responses[1]["error"]["code"], -32601);
    }

    #[test]
    fn rpc_error_constructors_use_standard_codes_and_messages() {
        for (error, code, message) in [
            (RpcError::parse_error("parse"), -32700, "parse"),
            (RpcError::invalid_request("request"), -32600, "request"),
            (
                RpcError::method_not_found("missing"),
                -32601,
                "method not found: missing",
            ),
            (RpcError::invalid_params("params"), -32602, "params"),
            (
                RpcError::unsupported_protocol_version("version"),
                -32001,
                "version",
            ),
            (RpcError::internal_error("internal"), -32603, "internal"),
        ] {
            assert_eq!(error.code, code);
            assert_eq!(error.message, message);
            assert!(error.data.is_none());
            assert_eq!(error.to_string(), format!("RPC error {code}: {message}"));
        }
    }

    #[test]
    fn rejects_missing_fields_and_unsupported_numeric_ids() {
        for input in [
            r#"{"method":"stop"}"#,
            r#"{"jsonrpc":"2.0"}"#,
            r#"{"jsonrpc":"2.0","method":7}"#,
            r#"{"jsonrpc":"2.0","method":"stop","id":-1}"#,
            r#"{"jsonrpc":"2.0","method":"stop","id":1.5}"#,
        ] {
            assert_eq!(parse_request(input).unwrap_err().code, -32600);
        }

        let request = parse_request(r#"{"jsonrpc":"2.0","method":"stop","id":null}"#)
            .expect("null id is a notification");
        assert_eq!(request.id, None);
    }

    #[test]
    fn blank_invalid_and_failed_notifications_have_correct_response_behavior() {
        let calls = std::cell::Cell::new(0);
        let mut handler = |_request: Request| -> Result<Value, RpcError> {
            calls.set(calls.get() + 1);
            Err(RpcError::internal_error("failed"))
        };
        let mut output = Vec::new();

        handle_line(" \r\n", &mut handler, &mut output).unwrap();
        assert_eq!(calls.get(), 0);
        assert!(output.is_empty());

        handle_line("not json", &mut handler, &mut output).unwrap();
        assert_eq!(calls.get(), 0);
        let invalid: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(invalid["id"], Value::Null);
        assert_eq!(invalid["error"]["code"], -32700);

        output.clear();
        handle_line(
            r#"{"jsonrpc":"2.0","method":"notify"}"#,
            &mut handler,
            &mut output,
        )
        .unwrap();
        assert_eq!(calls.get(), 1);
        assert!(output.is_empty());
    }

    #[test]
    fn oversized_request_frames_are_discarded_without_losing_the_next_frame() {
        let calls = std::cell::Cell::new(0);
        let mut handler = |_request: Request| -> Result<Value, RpcError> {
            calls.set(calls.get() + 1);
            Ok(Value::Null)
        };
        let mut output = Vec::new();
        let mut frames = FrameBuffer::new();
        let oversized = vec![b'x'; MAX_RPC_FRAME_BYTES + 1];

        frames
            .push(&oversized, |frame| {
                handle_frame(frame, &mut handler, &mut output)
            })
            .unwrap();
        assert!(frames.bytes.capacity() <= MAX_RPC_FRAME_BYTES);
        assert!(output.is_empty());
        frames
            .push(
                b"\n{\"jsonrpc\":\"2.0\",\"id\":9,\"method\":\"ok\"}\n",
                |frame| handle_frame(frame, &mut handler, &mut output),
            )
            .unwrap();

        let responses: Vec<Value> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(calls.get(), 1);
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0]["error"]["code"], -32600);
        assert_eq!(responses[1], json!({"jsonrpc":"2.0","id":9,"result":null}));
    }
}
