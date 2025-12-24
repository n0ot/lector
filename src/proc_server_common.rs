use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::io::{self, Read, Write};

#[derive(Debug)]
pub struct Request {
    pub id: Option<u64>,
    pub method: String,
    pub params: Option<Value>,
}

#[derive(Debug)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl RpcError {
    pub fn parse_error(message: impl Into<String>) -> Self {
        RpcError {
            code: -32700,
            message: message.into(),
            data: None,
        }
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        RpcError {
            code: -32600,
            message: message.into(),
            data: None,
        }
    }

    pub fn method_not_found(method: impl Into<String>) -> Self {
        RpcError {
            code: -32601,
            message: format!("method not found: {}", method.into()),
            data: None,
        }
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        RpcError {
            code: -32602,
            message: message.into(),
            data: None,
        }
    }

    pub fn internal_error(message: impl Into<String>) -> Self {
        RpcError {
            code: -32603,
            message: message.into(),
            data: None,
        }
    }
}

pub fn run_server<F>(mut handler: F) -> Result<()>
where
    F: FnMut(Request) -> Result<Value, RpcError>,
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
    F: FnMut(Request) -> Result<Value, RpcError>,
{
    use std::io::BufRead;

    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    let mut line = String::new();
    let mut stdin = stdin.lock();
    loop {
        line.clear();
        let read = stdin.read_line(&mut line).context("read stdin")?;
        if read == 0 {
            return Ok(());
        }
        handle_line(&line, handler, &mut stdout)?;
    }
}

#[cfg(target_os = "macos")]
fn run_server_macos<F>(handler: &mut F) -> Result<()>
where
    F: FnMut(Request) -> Result<Value, RpcError>,
{
    use crate::platform;
    use mio::{Events, Interest, Poll, Token};
    use std::os::fd::AsRawFd;
    use std::time::Duration;

    let mut poll = Poll::new().context("create poll")?;
    let mut events = Events::with_capacity(8);
    let mut stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    poll.registry().register(
        &mut mio::unix::SourceFd(&stdin.as_raw_fd()),
        Token(0),
        Interest::READABLE,
    )?;
    let mut buffer: Vec<u8> = Vec::new();
    loop {
        poll.poll(&mut events, Some(Duration::from_millis(10)))?;
        for event in events.iter() {
            if event.token() == Token(0) {
                let mut chunk = [0u8; 4096];
                let read = stdin.read(&mut chunk).context("read stdin")?;
                if read == 0 {
                    return Ok(());
                }
                buffer.extend_from_slice(&chunk[..read]);
                while let Some(pos) = buffer.iter().position(|b| *b == b'\n') {
                    let line = buffer.drain(..=pos).collect::<Vec<u8>>();
                    let line = String::from_utf8_lossy(&line);
                    handle_line(line.trim_end_matches(&['\r', '\n'][..]), handler, &mut stdout)?;
                }
            }
        }
        platform::tick_runloop()?;
    }
}

fn handle_line<F>(line: &str, handler: &mut F, stdout: &mut dyn Write) -> Result<()>
where
    F: FnMut(Request) -> Result<Value, RpcError>,
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

fn parse_request(line: &str) -> Result<Request, RpcError> {
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
        Some(Value::Number(n)) => n.as_u64(),
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
    serde_json::to_writer(&mut *stdout, &response).context("write rpc response")?;
    stdout.write_all(b"\n").context("write response newline")?;
    stdout.flush().context("flush response")?;
    Ok(())
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
    serde_json::to_writer(&mut *stdout, &response).context("write rpc error")?;
    stdout.write_all(b"\n").context("write error newline")?;
    stdout.flush().context("flush error")?;
    Ok(())
}
