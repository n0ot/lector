use super::Driver;
use anyhow::Result as DriverResult;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("spawn proc driver {path}")]
    Spawn {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("capture proc driver stdin")]
    MissingStdin,
    #[error("capture proc driver stdout")]
    MissingStdout,
    #[error("serialize RPC request")]
    Serialize(#[source] serde_json::Error),
    #[error("{operation}")]
    Io {
        operation: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("proc driver closed stdout while waiting for response")]
    Closed,
    #[error("parse RPC response")]
    Parse(#[source] serde_json::Error),
    #[error("proc driver RPC error {code}: {message}{data}")]
    Rpc {
        code: i64,
        message: String,
        data: String,
    },
}

fn io_error(operation: &'static str) -> impl FnOnce(std::io::Error) -> Error {
    move |source| Error::Io { operation, source }
}

#[derive(Serialize)]
struct JsonRpcRequest<'a> {
    jsonrpc: &'static str,
    id: u64,
    method: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct JsonRpcResponse {
    #[allow(dead_code)]
    jsonrpc: Option<String>,
    id: Option<u64>,
    #[allow(dead_code)]
    result: Option<serde_json::Value>,
    error: Option<JsonRpcError>,
}

#[derive(Deserialize)]
struct JsonRpcError {
    code: i64,
    message: String,
    data: Option<serde_json::Value>,
}

pub struct ProcDriver {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    request_buf: Vec<u8>,
    response_buf: String,
    next_id: u64,
    rate: f32,
}

impl ProcDriver {
    pub fn new(path: &Path) -> Result<Self> {
        let mut child = Command::new(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|source| Error::Spawn {
                path: path.display().to_string(),
                source,
            })?;
        let stdin = child.stdin.take().ok_or(Error::MissingStdin)?;
        let stdout = child.stdout.take().ok_or(Error::MissingStdout)?;
        Ok(ProcDriver {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            request_buf: Vec::with_capacity(256),
            response_buf: String::with_capacity(256),
            next_id: 1,
            rate: 1.0,
        })
    }

    fn call(&mut self, method: &str, params: Option<serde_json::Value>) -> Result<()> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let request = JsonRpcRequest {
            jsonrpc: "2.0",
            id,
            method,
            params,
        };
        self.request_buf.clear();
        serde_json::to_writer(&mut self.request_buf, &request).map_err(Error::Serialize)?;
        self.request_buf.push(b'\n');
        self.stdin
            .write_all(&self.request_buf)
            .map_err(io_error("write RPC request"))?;
        self.stdin.flush().map_err(io_error("flush RPC request"))?;

        loop {
            self.response_buf.clear();
            let read = self
                .stdout
                .read_line(&mut self.response_buf)
                .map_err(io_error("read RPC response"))?;
            if read == 0 {
                return Err(Error::Closed);
            }
            let response: JsonRpcResponse =
                serde_json::from_str(self.response_buf.trim()).map_err(Error::Parse)?;
            if response.id != Some(id) {
                continue;
            }
            if let Some(err) = response.error {
                return Err(Error::Rpc {
                    code: err.code,
                    message: err.message,
                    data: err.data.map(|v| format!(" ({v})")).unwrap_or_default(),
                });
            }
            return Ok(());
        }
    }
}

impl Driver for ProcDriver {
    fn speak(&mut self, text: &str, interrupt: bool) -> DriverResult<()> {
        self.call(
            "speak",
            Some(json!({ "text": text, "interrupt": interrupt })),
        )
        .map_err(Into::into)
    }

    fn stop(&mut self) -> DriverResult<()> {
        self.call("stop", None).map_err(Into::into)
    }

    fn get_rate(&self) -> f32 {
        self.rate
    }

    fn set_rate(&mut self, rate: f32) -> DriverResult<()> {
        self.call("set_rate", Some(json!({ "rate": rate })))?;
        self.rate = rate;
        Ok(())
    }
}

impl Drop for ProcDriver {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
