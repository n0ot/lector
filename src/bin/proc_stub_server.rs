use anyhow::Result;
use lector::proc_server_common::{Request, RpcError, run_server};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs::{File, OpenOptions},
    io::{self, BufRead, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

struct State {
    rate: f32,
    speech_log: Option<File>,
    rpc_log: Option<File>,
    stall_speech: bool,
    legacy_protocol: bool,
    generation: u64,
    identity: String,
    crash_speak: bool,
}

#[derive(Default)]
struct Options {
    adversary: Option<String>,
    legacy_protocol: bool,
    speech_log: Option<PathBuf>,
    rpc_log: Option<PathBuf>,
    lifecycle_state: Option<PathBuf>,
    fail_start_generations: BTreeSet<u64>,
    crash_speak_generations: BTreeSet<u64>,
    identity: String,
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut options = Options::default();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--legacy" => options.legacy_protocol = true,
            "--adversary" => {
                options.adversary = Some(required_arg(&mut args, "--adversary")?);
            }
            "--speech-log" => {
                options.speech_log = Some(required_arg(&mut args, "--speech-log")?.into());
            }
            "--rpc-log" => {
                options.rpc_log = Some(required_arg(&mut args, "--rpc-log")?.into());
            }
            "--lifecycle-state" => {
                options.lifecycle_state =
                    Some(required_arg(&mut args, "--lifecycle-state")?.into());
            }
            "--fail-start-generations" => {
                options.fail_start_generations =
                    parse_generations(&required_arg(&mut args, "--fail-start-generations")?)?;
            }
            "--crash-speak-generations" => {
                options.crash_speak_generations =
                    parse_generations(&required_arg(&mut args, "--crash-speak-generations")?)?;
            }
            "--identity" => options.identity = required_arg(&mut args, "--identity")?,
            _ => return Err(anyhow::anyhow!("unknown argument {arg:?}")),
        }
    }
    if let Some(mode) = options.adversary {
        return run_adversary(&mode);
    }

    let generation = options
        .lifecycle_state
        .as_deref()
        .map(next_generation)
        .transpose()?
        .unwrap_or(1);
    if options.fail_start_generations.contains(&generation) {
        return Ok(());
    }

    // Minimal proc server used by tests to validate JSON-RPC wiring without real TTS.
    let speech_log_path = options
        .speech_log
        .or_else(|| std::env::var_os("LECTOR_PROC_STUB_LOG").map(PathBuf::from));
    let speech_log = speech_log_path
        .map(|path| OpenOptions::new().create(true).append(true).open(path))
        .transpose()?;
    let rpc_log = options
        .rpc_log
        .or_else(|| std::env::var_os("LECTOR_SPEECH_RPC_LOG").map(PathBuf::from))
        .or_else(|| std::env::var_os("LECTOR_PROC_STUB_RPC_LOG").map(PathBuf::from))
        .map(|path| OpenOptions::new().create(true).append(true).open(path))
        .transpose()?;
    let mut state = State {
        rate: 1.0,
        speech_log,
        rpc_log,
        stall_speech: std::env::var_os("LECTOR_PROC_STUB_STALL_SPEECH").is_some(),
        legacy_protocol: options.legacy_protocol,
        generation,
        identity: options.identity,
        crash_speak: options.crash_speak_generations.contains(&generation),
    };
    run_server(|req| handle_request(req, &mut state))?;
    Ok(())
}

fn required_arg(args: &mut impl Iterator<Item = String>, option: &str) -> Result<String> {
    args.next()
        .ok_or_else(|| anyhow::anyhow!("{option} requires a value"))
}

fn parse_generations(value: &str) -> Result<BTreeSet<u64>> {
    value
        .split(',')
        .map(|generation| {
            generation
                .parse()
                .map_err(|error| anyhow::anyhow!("invalid generation {generation:?}: {error}"))
        })
        .collect()
}

fn next_generation(path: &Path) -> Result<u64> {
    let previous = match std::fs::read_to_string(path) {
        Ok(value) => value.trim().parse::<u64>()?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
        Err(error) => return Err(error.into()),
    };
    let generation = previous
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("lifecycle generation overflow"))?;
    let mut state = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)?;
    writeln!(state, "{generation}")?;
    state.flush()?;
    Ok(generation)
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
                "generation": state.generation,
                "identity": &state.identity,
            }),
        )
        .map_err(|error| RpcError::internal_error(format!("write proc stub RPC log: {error}")))?;
        writeln!(log).map_err(stub_log_error)?;
        log.flush().map_err(stub_log_error)?;
    }
    if !state.legacy_protocol
        && let Some(result) = lector::proc_server_common::handle_protocol_request(
            &request,
            "lector-proc-stub",
            env!("CARGO_PKG_VERSION"),
        )
    {
        return result;
    }
    match request.method.as_str() {
        "speak" => {
            let text = request
                .params
                .as_ref()
                .and_then(|params| params.get("text"))
                .and_then(Value::as_str)
                .ok_or_else(|| RpcError::invalid_params("missing text"))?;
            if state.crash_speak {
                // Model the uncertain-delivery case: the server received and
                // durably logged the request, then died before acknowledging
                // it. A correct supervisor must not replay this utterance.
                std::process::exit(86);
            }
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
            if state.legacy_protocol {
                Ok(Value::Null)
            } else {
                Ok(json!({ "rate": state.rate }))
            }
        }
        _ => Err(RpcError::method_not_found(request.method)),
    }
}

fn stub_log_error(error: io::Error) -> RpcError {
    RpcError::internal_error(format!("write proc stub speech log: {error}"))
}

fn run_adversary(mode: &str) -> Result<()> {
    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();
    let mut stdout = io::stdout().lock();
    let Some(initialize) = lines.next() else {
        return Ok(());
    };
    let initialize: Value = serde_json::from_str(&initialize?)?;
    let initialize_id = initialize["id"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("initialize request has no numeric id"))?;
    if mode == "eof-on-initialize" {
        return Ok(());
    }
    if mode == "stall-on-initialize" {
        stall_forever();
    }
    writeln!(
        stdout,
        "{}",
        json!({
            "jsonrpc": "2.0",
            "id": initialize_id,
            "result": {
                "protocol_version": "1.0",
                "server": {"name": "lector-proc-adversary", "version": env!("CARGO_PKG_VERSION")},
                "capabilities": {"speak": true, "stop": true, "set_rate": true, "rpc_discover": true},
            },
        })
    )?;
    stdout.flush()?;

    let Some(request) = lines.next() else {
        return Ok(());
    };
    let request: Value = serde_json::from_str(&request?)?;
    let id = request["id"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("request has no numeric id"))?;
    match mode {
        "eof-after-initialize" => Ok(()),
        "stall-after-initialize" => stall_forever(),
        "malformed-after-initialize" => {
            writeln!(stdout, "{{definitely-not-json")?;
            stdout.flush()?;
            Ok(())
        }
        "wrong-id-after-initialize" => {
            writeln!(
                stdout,
                "{}",
                json!({"jsonrpc": "2.0", "id": id + 1, "result": null})
            )?;
            stdout.flush()?;
            Ok(())
        }
        "oversized-after-initialize" => {
            let prefix = format!(r#"{{"jsonrpc":"2.0","id":{id},"result":""#);
            stdout.write_all(prefix.as_bytes())?;
            let chunk = [b'x'; 8192];
            let mut remaining = lector::proc_server_common::MAX_RPC_FRAME_BYTES;
            while remaining != 0 {
                let count = remaining.min(chunk.len());
                stdout.write_all(&chunk[..count])?;
                remaining -= count;
            }
            stdout.write_all(b"\"}\n")?;
            stdout.flush()?;
            Ok(())
        }
        _ => Err(anyhow::anyhow!("unknown adversary mode {mode:?}")),
    }
}

fn stall_forever() -> ! {
    loop {
        std::thread::park_timeout(std::time::Duration::from_secs(60));
    }
}
