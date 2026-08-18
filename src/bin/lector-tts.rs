use anyhow::Result;
use lector::proc_server_common::{Request, RpcError, run_server};
use serde_json::{Value, json};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    time::{SystemTime, UNIX_EPOCH},
};
use tts::Tts;

struct State {
    tts: Tts,
    rate: f32,
    min_rate: f32,
    max_rate: f32,
    rpc_log: Option<File>,
}

fn main() -> Result<()> {
    let tts = Tts::default().map_err(|e| anyhow::anyhow!(e))?;
    let min_rate = tts.min_rate().map_err(|e| anyhow::anyhow!(e))?;
    let max_rate = tts.max_rate().map_err(|e| anyhow::anyhow!(e))?;
    let rate = tts.normal_rate().map_err(|e| anyhow::anyhow!(e))?;
    tts.set_rate(rate).map_err(|e| anyhow::anyhow!(e))?;
    let rpc_log = std::env::var_os("LECTOR_SPEECH_RPC_LOG")
        .map(|path| OpenOptions::new().create(true).append(true).open(path))
        .transpose()?;
    let mut state = State {
        tts,
        rate,
        min_rate,
        max_rate,
        rpc_log,
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
        .map_err(|error| RpcError::internal_error(format!("write speech RPC log: {error}")))?;
        writeln!(log)
            .and_then(|()| log.flush())
            .map_err(|error| RpcError::internal_error(format!("write speech RPC log: {error}")))?;
    }
    match request.method.as_str() {
        "speak" => {
            let params = request
                .params
                .ok_or_else(|| RpcError::invalid_params("missing params"))?;
            let text = params
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| RpcError::invalid_params("missing text"))?;
            let interrupt = params
                .get("interrupt")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            state
                .tts
                .speak(text, interrupt)
                .map_err(|e| RpcError::internal_error(e.to_string()))?;
            Ok(Value::Null)
        }
        "stop" => {
            state
                .tts
                .stop()
                .map_err(|e| RpcError::internal_error(e.to_string()))?;
            Ok(Value::Null)
        }
        "set_rate" => {
            let params = request
                .params
                .ok_or_else(|| RpcError::invalid_params("missing params"))?;
            let rate = params
                .get("rate")
                .and_then(Value::as_f64)
                .ok_or_else(|| RpcError::invalid_params("missing rate"))?;
            let clamped = (rate as f32).clamp(state.min_rate, state.max_rate);
            state
                .tts
                .set_rate(clamped)
                .map_err(|e| RpcError::internal_error(e.to_string()))?;
            state.rate = clamped;
            Ok(json!({ "rate": state.rate }))
        }
        _ => Err(RpcError::method_not_found(request.method)),
    }
}
