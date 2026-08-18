use anyhow::Result;
use lector::proc_server_common::{Request, RpcError, run_server};
use serde_json::{Value, json};
use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    time::{SystemTime, UNIX_EPOCH},
};

struct State {
    rate: f32,
    speech_log: Option<File>,
    rpc_log: Option<File>,
    stall_speech: bool,
}

fn main() -> Result<()> {
    // Minimal proc server used by tests to validate JSON-RPC wiring without real TTS.
    let speech_log = std::env::var_os("LECTOR_PROC_STUB_LOG")
        .map(|path| OpenOptions::new().create(true).append(true).open(path))
        .transpose()?;
    let rpc_log = std::env::var_os("LECTOR_SPEECH_RPC_LOG")
        .or_else(|| std::env::var_os("LECTOR_PROC_STUB_RPC_LOG"))
        .map(|path| OpenOptions::new().create(true).append(true).open(path))
        .transpose()?;
    let mut state = State {
        rate: 1.0,
        speech_log,
        rpc_log,
        stall_speech: std::env::var_os("LECTOR_PROC_STUB_STALL_SPEECH").is_some(),
    };
    run_server(|req| handle_request(req, &mut state))?;
    Ok(())
}

fn handle_request(request: Request, state: &mut State) -> Result<Value, RpcError> {
    if let Some(log) = &mut state.rpc_log {
        let time_unix_us = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as u64;
        serde_json::to_writer(
            &mut *log,
            &json!({
                "time_unix_us": time_unix_us,
                "id": request.id,
                "method": &request.method,
                "params": &request.params,
            }),
        )
        .map_err(|error| RpcError::internal_error(format!("write proc stub RPC log: {error}")))?;
        writeln!(log).map_err(stub_log_error)?;
        log.flush().map_err(stub_log_error)?;
    }
    match request.method.as_str() {
        "speak" => {
            let text = request
                .params
                .as_ref()
                .and_then(|params| params.get("text"))
                .and_then(Value::as_str)
                .ok_or_else(|| RpcError::invalid_params("missing text"))?;
            if state.stall_speech {
                loop {
                    std::thread::park_timeout(std::time::Duration::from_secs(60));
                }
            }
            if let Some(log) = &mut state.speech_log {
                serde_json::to_writer(&mut *log, text).map_err(|error| {
                    RpcError::internal_error(format!("write proc stub speech log: {error}"))
                })?;
                writeln!(log).map_err(stub_log_error)?;
                log.flush().map_err(stub_log_error)?;
            }
            Ok(Value::Null)
        }
        "stop" => Ok(Value::Null),
        "set_rate" => {
            let params = request
                .params
                .ok_or_else(|| RpcError::invalid_params("missing params"))?;
            let rate = params
                .get("rate")
                .and_then(Value::as_f64)
                .ok_or_else(|| RpcError::invalid_params("missing rate"))?;
            state.rate = rate as f32;
            Ok(json!({ "rate": state.rate }))
        }
        _ => Err(RpcError::method_not_found(request.method)),
    }
}

fn stub_log_error(error: io::Error) -> RpcError {
    RpcError::internal_error(format!("write proc stub speech log: {error}"))
}
