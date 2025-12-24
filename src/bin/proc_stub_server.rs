use anyhow::Result;
use lector::proc_server_common::{Request, RpcError, run_server};
use serde_json::{Value, json};

struct State {
    rate: f32,
}

fn main() -> Result<()> {
    // Minimal proc server used by tests to validate JSON-RPC wiring without real TTS.
    let mut state = State { rate: 1.0 };
    run_server(|req| handle_request(req, &mut state))
}

fn handle_request(request: Request, state: &mut State) -> Result<Value, RpcError> {
    match request.method.as_str() {
        "speak" => Ok(Value::Null),
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
