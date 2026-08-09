use serde_json::{Value, json};
use std::io::{self, Read, Write};

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

    pub fn internal_error(message: impl Into<String>) -> Self {
        Self::new(-32603, message)
    }
}

pub fn run_server<F>(mut handler: F) -> Result<()>
where
    F: FnMut(Request) -> std::result::Result<Value, RpcError>,
{
    #[cfg(target_os = "macos")]
    {
        run_server_macos(&mut handler)
    }
    #[cfg(not(target_os = "macos"))]
    {
        run_server_blocking(&mut handler)
    }
}

#[cfg(not(target_os = "macos"))]
fn run_server_blocking<F>(handler: &mut F) -> Result<()>
where
    F: FnMut(Request) -> std::result::Result<Value, RpcError>,
{
    use std::io::BufRead;

    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    let mut line = String::new();
    let mut stdin = stdin.lock();
    loop {
        line.clear();
        let read = stdin.read_line(&mut line).map_err(io_error("read stdin"))?;
        if read == 0 {
            return Ok(());
        }
        handle_line(&line, handler, &mut stdout)?;
    }
}

#[cfg(target_os = "macos")]
fn run_server_macos<F>(handler: &mut F) -> Result<()>
where
    F: FnMut(Request) -> std::result::Result<Value, RpcError>,
{
    use crate::platform;
    use mio::{Events, Interest, Poll, Token};
    use std::os::fd::AsRawFd;
    use std::time::Duration;

    let mut poll = Poll::new().map_err(io_error("create poll"))?;
    let mut events = Events::with_capacity(8);
    let mut stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    poll.registry()
        .register(
            &mut mio::unix::SourceFd(&stdin.as_raw_fd()),
            Token(0),
            Interest::READABLE,
        )
        .map_err(io_error("register stdin poll source"))?;
    let mut buffer: Vec<u8> = Vec::new();
    loop {
        poll.poll(&mut events, Some(Duration::from_millis(10)))
            .map_err(io_error("poll stdin"))?;
        for event in events.iter() {
            if event.token() == Token(0) {
                let mut chunk = [0u8; 4096];
                let read = stdin.read(&mut chunk).map_err(io_error("read stdin"))?;
                if read == 0 {
                    return Ok(());
                }
                buffer.extend_from_slice(&chunk[..read]);
                while let Some(pos) = buffer.iter().position(|b| *b == b'\n') {
                    let consumed = pos + 1;
                    {
                        let line = String::from_utf8_lossy(&buffer[..consumed]);
                        handle_line(
                            line.trim_end_matches(&['\r', '\n'][..]),
                            handler,
                            &mut stdout,
                        )?;
                    }
                    buffer.drain(..consumed);
                }
            }
        }
        platform::tick_runloop();
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
    serde_json::to_writer(&mut *stdout, response).map_err(json_error("write RPC response"))?;
    stdout
        .write_all(b"\n")
        .map_err(io_error("write RPC response newline"))?;
    stdout.flush().map_err(io_error("flush RPC response"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Request, RpcError, handle_line, parse_request};
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
}
