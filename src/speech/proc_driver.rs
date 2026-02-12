use super::Driver;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

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
    next_id: u64,
    rate: f32,
}

impl ProcDriver {
    pub fn new(path: &Path) -> Result<Self> {
        let mut child = Command::new(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .with_context(|| format!("spawn proc driver {}", path.display()))?;
        let stdin = child.stdin.take().context("capture proc driver stdin")?;
        let stdout = child.stdout.take().context("capture proc driver stdout")?;
        Ok(ProcDriver {
            child,
            stdin,
            stdout: BufReader::new(stdout),
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
        let payload = serde_json::to_string(&request).context("serialize rpc request")?;
        self.stdin
            .write_all(payload.as_bytes())
            .context("write rpc request")?;
        self.stdin.write_all(b"\n").context("write rpc newline")?;
        self.stdin.flush().context("flush rpc request")?;

        loop {
            let mut line = String::new();
            let read = self
                .stdout
                .read_line(&mut line)
                .context("read rpc response")?;
            if read == 0 {
                bail!("proc driver closed stdout while waiting for response");
            }
            let response: JsonRpcResponse =
                serde_json::from_str(line.trim()).context("parse rpc response")?;
            if response.id != Some(id) {
                continue;
            }
            if let Some(err) = response.error {
                bail!(
                    "proc driver rpc error {}: {}{}",
                    err.code,
                    err.message,
                    err.data.map(|v| format!(" ({})", v)).unwrap_or_default()
                );
            }
            return Ok(());
        }
    }
}

impl Driver for ProcDriver {
    fn speak(&mut self, text: &str, interrupt: bool) -> Result<()> {
        self.call(
            "speak",
            Some(json!({ "text": text, "interrupt": interrupt })),
        )
    }

    fn stop(&mut self) -> Result<()> {
        self.call("stop", None)
    }

    fn get_rate(&self) -> f32 {
        self.rate
    }

    fn set_rate(&mut self, rate: f32) -> Result<()> {
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
