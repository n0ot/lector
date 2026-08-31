use anyhow::Result;
use lector::{
    proc_server_common::{Request, RpcError, ServerNotification, run_server_with_tick},
    speech::protocol::{
        AcceptedResult, BackendInfo, ControlCapabilities, CurrentVoiceResult, SettingCapabilities,
        SettingSupport, SpeechCapabilities, SpeechEventNotification, SpeechEventPayload,
        StopSupport, UtteranceId, VoiceCapabilities, VoiceInfo, VoiceListResult,
    },
};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs::{File, OpenOptions},
    io::{self, BufRead, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

struct State {
    rate: f32,
    pitch: f32,
    volume: f32,
    voice_id: String,
    speech_log: Option<File>,
    rpc_log: Option<File>,
    stall_speech: bool,
    legacy_protocol: bool,
    generation: u64,
    identity: String,
    crash_speak: bool,
    initialized: bool,
    notifications: VecDeque<ServerNotification>,
}

#[derive(Default)]
struct Options {
    adversary: Option<String>,
    pid_file: Option<PathBuf>,
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
            "--pid-file" => {
                options.pid_file = Some(required_arg(&mut args, "--pid-file")?.into());
            }
            "--startup-argv-probe" => {
                let received = [
                    required_arg(&mut args, "--startup-argv-probe")?,
                    required_arg(&mut args, "--startup-argv-probe")?,
                    required_arg(&mut args, "--startup-argv-probe")?,
                ];
                let expected = [
                    "argument with spaces",
                    "'literal punctuation'",
                    "$(opaque text)",
                ];
                if received.each_ref().map(String::as_str) != expected {
                    anyhow::bail!(
                        "LECTOR-SPEECH-ARGV-ERROR: expected {expected:?}, received {received:?}"
                    );
                }
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
        return run_adversary(&mode, options.pid_file.as_deref());
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
    let state = State {
        rate: 1.0,
        pitch: 1.0,
        volume: 1.0,
        voice_id: "stub-a".to_owned(),
        speech_log,
        rpc_log,
        stall_speech: std::env::var_os("LECTOR_PROC_STUB_STALL_SPEECH").is_some(),
        legacy_protocol: options.legacy_protocol,
        generation,
        identity: options.identity,
        crash_speak: options.crash_speak_generations.contains(&generation),
        initialized: false,
        notifications: VecDeque::new(),
    };
    let state = RefCell::new(state);
    run_server_with_tick(
        |req| handle_request(req, &mut state.borrow_mut()),
        || state.borrow_mut().notifications.drain(..).collect(),
    )?;
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
    if !state.legacy_protocol && request.method == "initialize" && state.initialized {
        return Err(RpcError::invalid_request(
            "speech server is already initialized",
        ));
    }
    let backend = stub_backend_info();
    if !state.legacy_protocol
        && let Some(result) = lector::proc_server_common::handle_protocol_request(
            &request,
            "lector-proc-stub",
            env!("CARGO_PKG_VERSION"),
            Some(&backend),
            &stub_capabilities(),
        )
    {
        if request.method == "initialize" && result.is_ok() {
            state.initialized = true;
        }
        return result;
    }
    if !state.legacy_protocol && request.method.starts_with("speech.") && !state.initialized {
        return Err(RpcError::invalid_request(
            "speech server is not initialized",
        ));
    }
    match request.method.as_str() {
        "speak" | "speech.speak" => {
            let text = request
                .params
                .as_ref()
                .and_then(|params| params.get("text"))
                .and_then(Value::as_str)
                .ok_or_else(|| RpcError::invalid_params("missing text"))?;
            let utterance_id = request
                .params
                .as_ref()
                .and_then(|params| params.get("utteranceId"))
                .and_then(Value::as_str)
                .map(UtteranceId::new);
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
            if let Some(utterance_id) = utterance_id {
                queue_stub_lifecycle(state, utterance_id);
                serde_json::to_value(AcceptedResult { accepted: true })
                    .map_err(|error| RpcError::internal_error(error.to_string()))
            } else {
                Ok(Value::Null)
            }
        }
        "stop" => Ok(Value::Null),
        "speech.stop" | "speech.resume" => serde_json::to_value(AcceptedResult { accepted: true })
            .map_err(|error| RpcError::internal_error(error.to_string())),
        "speech.pause" => Ok(json!({"paused": false})),
        "speech.getRate" => Ok(json!({ "rate": state.rate })),
        "set_rate" | "speech.setRate" => {
            let params = request
                .params
                .ok_or_else(|| RpcError::invalid_params("missing params"))?;
            let rate = params
                .get("rate")
                .and_then(Value::as_f64)
                .ok_or_else(|| RpcError::invalid_params("missing rate"))?;
            state.rate = rate as f32;
            if state.legacy_protocol || request.method == "set_rate" {
                Ok(Value::Null)
            } else {
                Ok(json!({ "rate": state.rate }))
            }
        }
        "speech.getPitch" => Ok(json!({ "pitch": state.pitch })),
        "speech.setPitch" => {
            let pitch = request
                .params
                .as_ref()
                .and_then(|params| params.get("pitch"))
                .and_then(Value::as_f64)
                .ok_or_else(|| RpcError::invalid_params("missing pitch"))?;
            state.pitch = pitch as f32;
            Ok(json!({ "pitch": state.pitch }))
        }
        "speech.getVolume" => Ok(json!({ "volume": state.volume })),
        "speech.setVolume" => {
            let volume = request
                .params
                .as_ref()
                .and_then(|params| params.get("volume"))
                .and_then(Value::as_f64)
                .ok_or_else(|| RpcError::invalid_params("missing volume"))?;
            state.volume = volume as f32;
            Ok(json!({ "volume": state.volume }))
        }
        "speech.listVoices" => serde_json::to_value(VoiceListResult {
            voices: stub_voices(),
        })
        .map_err(|error| RpcError::internal_error(error.to_string())),
        "speech.getVoice" => serde_json::to_value(CurrentVoiceResult {
            voice: stub_voices()
                .into_iter()
                .find(|voice| voice.id == state.voice_id),
        })
        .map_err(|error| RpcError::internal_error(error.to_string())),
        "speech.setVoice" => {
            let voice_id = request
                .params
                .as_ref()
                .and_then(|params| params.get("voiceId"))
                .and_then(Value::as_str)
                .ok_or_else(|| RpcError::invalid_params("missing voiceId"))?;
            let voice = stub_voices()
                .into_iter()
                .find(|voice| voice.id == voice_id)
                .ok_or_else(|| RpcError::invalid_params("voice not found"))?;
            state.voice_id.clone_from(&voice.id);
            serde_json::to_value(CurrentVoiceResult { voice: Some(voice) })
                .map_err(|error| RpcError::internal_error(error.to_string()))
        }
        _ => Err(RpcError::method_not_found(request.method)),
    }
}

fn stub_backend_info() -> BackendInfo {
    BackendInfo {
        id: "stub".to_owned(),
        name: "Deterministic stub".to_owned(),
        extensions: BTreeMap::new(),
    }
}

fn stub_voices() -> Vec<VoiceInfo> {
    [
        ("stub-a", "Stub voice A", "en-US"),
        ("stub-b", "Stub voice B", "en-GB"),
    ]
    .into_iter()
    .map(|(id, name, language)| VoiceInfo {
        id: id.to_owned(),
        name: name.to_owned(),
        language: language.to_owned(),
        gender: None,
        extensions: BTreeMap::new(),
    })
    .collect()
}

fn stub_capabilities() -> SpeechCapabilities {
    use lector::speech::protocol::{
        DeliveryGuarantee, EventCapability, LifecycleCapabilities, TerminalCapability,
    };
    SpeechCapabilities {
        lifecycle: LifecycleCapabilities {
            started: EventCapability {
                delivery: DeliveryGuarantee::Reliable,
                ..Default::default()
            },
            terminal: TerminalCapability {
                delivery: DeliveryGuarantee::Reliable,
                distinguishes: vec!["completed".to_owned()],
                ..Default::default()
            },
            ..Default::default()
        },
        controls: ControlCapabilities {
            stop: StopSupport::Confirmed,
            ..Default::default()
        },
        settings: SettingCapabilities {
            rate: SettingSupport::ReadWrite,
            pitch: SettingSupport::ReadWrite,
            volume: SettingSupport::ReadWrite,
            ..Default::default()
        },
        voices: VoiceCapabilities {
            list: true,
            current: true,
            select: true,
            ..Default::default()
        },
        ..Default::default()
    }
}

fn queue_stub_lifecycle(state: &mut State, utterance_id: UtteranceId) {
    for (sequence, kind, reason) in [
        (0, "started", None),
        (1, "ended", Some("completed".to_owned())),
    ] {
        let event = SpeechEventNotification {
            utterance_id: utterance_id.clone(),
            sequence,
            event: SpeechEventPayload {
                kind: kind.to_owned(),
                position: None,
                reason,
                message: None,
                extensions: BTreeMap::new(),
            },
        };
        if let Ok(params) = serde_json::to_value(event) {
            state
                .notifications
                .push_back(ServerNotification::new("speech.event", params));
        }
    }
}

fn stub_log_error(error: io::Error) -> RpcError {
    RpcError::internal_error(format!("write proc stub speech log: {error}"))
}

fn run_adversary(mode: &str, pid_file: Option<&Path>) -> Result<()> {
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
        if let Some(path) = pid_file {
            std::fs::write(path, format!("{}\n", std::process::id()))?;
        }
        stall_forever();
    }
    writeln!(
        stdout,
        "{}",
        json!({
            "jsonrpc": "2.0",
            "id": initialize_id,
            "result": {
                "protocol": {"major": 2, "minor": 0},
                "server": {"name": "lector-proc-adversary", "version": env!("CARGO_PKG_VERSION")},
                "capabilities": {
                    "lifecycle": {
                        "started": {"delivery": "reliable"},
                        "terminal": {"delivery": "reliable", "distinguishes": ["completed"]}
                    },
                    "controls": {"stop": "confirmed"}
                },
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
        "event-before-response" => {
            writeln!(
                stdout,
                "{}",
                json!({
                    "jsonrpc": "2.0",
                    "method": "future.notification",
                    "params": {"ignored": true}
                })
            )?;
            writeln!(
                stdout,
                "{}",
                json!({
                    "jsonrpc": "2.0",
                    "method": "speech.event",
                    "params": {
                        "utteranceId": "direct-1",
                        "sequence": 0,
                        "event": {"type": "started", "futureMember": 7}
                    }
                })
            )?;
            writeln!(
                stdout,
                "{}",
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {"accepted": true, "futureMember": 7}
                })
            )?;
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
