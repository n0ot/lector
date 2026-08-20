use lector::{
    proc_server_common::MAX_RPC_FRAME_BYTES,
    speech::{
        Driver,
        proc_driver::{Error as ProcError, ProcDriver, RpcTimeouts},
    },
};
use serde_json::Value;
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[test]
fn proc_driver_smoke() {
    let server_path = PathBuf::from(env!("CARGO_BIN_EXE_proc_stub_server"));
    let mut driver = ProcDriver::new(&server_path).expect("spawn proc stub server");
    assert!((driver.get_rate() - 1.0).abs() < f32::EPSILON);
    driver.speak("hello", true).expect("speak");
    driver.set_rate(1.25).expect("set_rate");
    assert!((driver.get_rate() - 1.25).abs() < f32::EPSILON);
    driver.stop().expect("stop");
}

#[test]
fn proc_driver_negotiates_the_current_protocol() {
    let server_path = PathBuf::from(env!("CARGO_BIN_EXE_proc_stub_server"));
    let driver = ProcDriver::new(&server_path).expect("spawn proc stub server");
    assert!(!driver.is_legacy_protocol());
}

#[test]
fn proc_driver_accepts_a_method_not_found_initialize_as_legacy() {
    let server_path = PathBuf::from(env!("CARGO_BIN_EXE_proc_stub_server"));
    let mut driver = ProcDriver::new_with_args(&server_path, ["--legacy"])
        .expect("spawn legacy proc stub server");
    assert!(driver.is_legacy_protocol());
    driver.speak("legacy speech", false).expect("legacy speak");
    driver.set_rate(1.5).expect("legacy set_rate");
    assert!((driver.get_rate() - 1.5).abs() < f32::EPSILON);
}

#[test]
fn rpc_discover_is_available_before_initialize() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_proc_stub_server"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn proc stub server");
    writeln!(
        child.stdin.as_mut().unwrap(),
        r#"{{"jsonrpc":"2.0","id":1,"method":"rpc.discover"}}"#
    )
    .unwrap();
    let mut response = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut response)
        .unwrap();
    let response: Value = serde_json::from_str(&response).unwrap();
    assert_eq!(response["result"]["openrpc"], "1.4.0");
    assert!(response["result"]["methods"].is_array());
    drop(child.stdin.take());
    assert!(child.wait().unwrap().success());
}

#[test]
fn proc_driver_initialize_has_an_absolute_deadline() {
    let server_path = PathBuf::from(env!("CARGO_BIN_EXE_proc_stub_server"));
    let started = Instant::now();
    let error = ProcDriver::new_with_args_and_timeouts(
        &server_path,
        ["--adversary", "stall-on-initialize"],
        short_timeouts(),
    )
    .err()
    .expect("stalled initialize must fail");
    assert!(matches!(error, ProcError::Timeout { .. }));
    assert!(error.is_transport_failure());
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn initialization_publishes_a_termination_handle_before_waiting() {
    let server_path = PathBuf::from(env!("CARGO_BIN_EXE_proc_stub_server"));
    let (handle_tx, handle_rx) = mpsc::sync_channel(1);
    let started = Instant::now();
    let constructor = thread::spawn(move || {
        ProcDriver::new_with_args_and_registration(
            &server_path,
            ["--adversary", "stall-on-initialize"],
            RpcTimeouts {
                initialize: Duration::from_secs(5),
                call: Duration::from_millis(150),
            },
            move |handle| assert!(handle_tx.send(handle).is_ok(), "publish termination handle"),
        )
    });

    let handle = handle_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("termination handle must precede initialize completion");
    handle
        .terminate_and_reap()
        .expect("terminate initializing server");
    let error = constructor
        .join()
        .expect("constructor thread")
        .err()
        .expect("terminated initialize must fail");
    assert!(error.is_transport_failure());
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn proc_driver_rpc_has_an_absolute_deadline() {
    let mut driver = adversarial_driver("stall-after-initialize");
    let started = Instant::now();
    let error = driver.speak("hang", false).unwrap_err();
    assert!(matches!(
        error.downcast_ref::<ProcError>(),
        Some(ProcError::Timeout { .. })
    ));
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn proc_driver_reports_eof_as_a_transport_failure() {
    let mut driver = adversarial_driver("eof-after-initialize");
    let error = driver.speak("eof", false).unwrap_err();
    let error = error.downcast_ref::<ProcError>().expect("proc error");
    assert!(matches!(error, ProcError::Closed));
    assert!(error.is_transport_failure());
}

#[test]
fn proc_driver_rejects_malformed_responses() {
    let mut driver = adversarial_driver("malformed-after-initialize");
    let error = driver.speak("malformed", false).unwrap_err();
    let error = error.downcast_ref::<ProcError>().expect("proc error");
    assert!(matches!(error, ProcError::Parse(_)));
    assert!(error.is_transport_failure());
}

#[test]
fn proc_driver_rejects_responses_for_another_request() {
    let mut driver = adversarial_driver("wrong-id-after-initialize");
    let error = driver.speak("wrong id", false).unwrap_err();
    let error = error.downcast_ref::<ProcError>().expect("proc error");
    assert!(matches!(error, ProcError::InvalidResponse(_)));
    assert!(error.is_transport_failure());
}

#[test]
fn proc_driver_bounds_response_frames() {
    // Generating and transferring a full MiB in an unoptimized test build is
    // intentionally allowed more time than the tiny hang tests. This test is
    // about the frame bound, not the ordinary call deadline.
    let mut driver = ProcDriver::new_with_args_and_timeouts(
        &PathBuf::from(env!("CARGO_BIN_EXE_proc_stub_server")),
        ["--adversary", "oversized-after-initialize"],
        RpcTimeouts {
            initialize: Duration::from_secs(2),
            call: Duration::from_secs(2),
        },
    )
    .expect("spawn oversized-response adversary");
    let error = driver.speak("oversized", false).unwrap_err();
    let error = error.downcast_ref::<ProcError>().expect("proc error");
    assert!(matches!(
        error,
        ProcError::ResponseFrameTooLarge { limit } if *limit == MAX_RPC_FRAME_BYTES
    ));
    assert!(error.is_transport_failure());
}

fn short_timeouts() -> RpcTimeouts {
    RpcTimeouts {
        initialize: Duration::from_millis(150),
        call: Duration::from_millis(150),
    }
}

fn adversarial_driver(mode: &str) -> ProcDriver {
    ProcDriver::new_with_args_and_timeouts(
        &PathBuf::from(env!("CARGO_BIN_EXE_proc_stub_server")),
        ["--adversary", mode],
        RpcTimeouts {
            initialize: Duration::from_secs(2),
            call: Duration::from_millis(150),
        },
    )
    .expect("spawn adversarial proc stub server")
}

#[test]
fn proc_driver_handles_requests_larger_than_the_server_read_chunk() {
    let server_path = PathBuf::from(env!("CARGO_BIN_EXE_proc_stub_server"));
    let mut driver = ProcDriver::new(&server_path).expect("spawn proc stub server");

    driver
        .speak(&"x".repeat(8 * 1024), false)
        .expect("large speech request must not strand bytes behind a poll edge");
    driver.stop().expect("server remains responsive");
}

#[test]
fn proc_driver_reports_spawn_failures_with_the_requested_path() {
    let missing = PathBuf::from("/definitely/missing/lector-proc-driver");
    let Err(error) = ProcDriver::new(&missing) else {
        panic!("missing executable unexpectedly spawned");
    };

    let message = error.to_string();
    assert!(message.contains("spawn proc driver"));
    assert!(message.contains(missing.to_str().unwrap()));
}

#[test]
fn proc_driver_preserves_rate_when_server_rejects_invalid_json_number() {
    let server_path = PathBuf::from(env!("CARGO_BIN_EXE_proc_stub_server"));
    let mut driver = ProcDriver::new(&server_path).expect("spawn proc stub server");

    let error = driver.set_rate(f32::NAN).unwrap_err();

    assert!(error.to_string().contains("RPC error -32602"));
    assert!((driver.get_rate() - 1.0).abs() < f32::EPSILON);
}

#[test]
fn proc_server_can_record_the_rpc_requests_used_for_real_speech() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let log_path = std::env::temp_dir().join(format!(
        "lector-speech-rpc-{}-{unique}.jsonl",
        std::process::id()
    ));
    let mut child = Command::new(env!("CARGO_BIN_EXE_proc_stub_server"))
        .env("LECTOR_SPEECH_RPC_LOG", &log_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn proc stub server");
    writeln!(
        child.stdin.as_mut().unwrap(),
        r#"{{"jsonrpc":"2.0","id":7,"method":"speak","params":{{"text":"tmux","interrupt":true}}}}"#
    )
    .unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().expect("wait for proc stub server");
    assert!(output.status.success());

    let record: Value = serde_json::from_str(&fs::read_to_string(&log_path).unwrap()).unwrap();
    assert_eq!(record["id"], 7);
    assert_eq!(record["method"], "speak");
    assert_eq!(record["params"]["text"], "tmux");
    assert_eq!(record["params"]["interrupt"], true);
    assert!(record["time_unix_us"].as_u64().is_some());

    fs::remove_file(log_path).unwrap();
}

#[cfg(target_os = "macos")]
#[test]
fn native_tts_server_advances_past_its_first_utterance() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let event_log = std::env::temp_dir().join(format!(
        "lector-speech-events-{}-{unique}.jsonl",
        std::process::id()
    ));
    let mut child = Command::new(env!("CARGO_BIN_EXE_lector"))
        .arg("--native-speech-server")
        .env("LECTOR_SPEECH_TEST_MUTE", "1")
        .env("LECTOR_SPEECH_EVENT_LOG", &event_log)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn native TTS server");
    let mut stdin = child.stdin.take().expect("native TTS stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("native TTS stdout"));

    {
        let mut rpc = |id: u64, method: &str, params: Value| -> Value {
            writeln!(
                stdin,
                "{}",
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": method,
                    "params": params,
                })
            )
            .expect("write native TTS request");
            stdin.flush().expect("flush native TTS request");
            let mut response = String::new();
            stdout
                .read_line(&mut response)
                .expect("read native TTS response");
            let response: Value =
                serde_json::from_str(&response).expect("parse native TTS response");
            assert_eq!(response["id"], id);
            response
        };

        let before_initialize = rpc(
            1,
            "speak",
            serde_json::json!({"text": "welcome to Lector", "interrupt": false}),
        );
        assert_eq!(before_initialize["error"]["code"], -32600);
        assert!(
            before_initialize["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("not initialized"))
        );

        let initialized = rpc(
            2,
            "initialize",
            serde_json::json!({
                "protocol_version": "1.0",
                "client": {"name": "lector-test", "version": "1"},
            }),
        );
        assert_eq!(initialized["result"]["protocol_version"], "1.0");

        let first = rpc(
            3,
            "speak",
            serde_json::json!({"text": "welcome to Lector", "interrupt": false}),
        );
        assert!(first.get("error").is_none(), "native TTS error: {first}");
        wait_for_speech_events(&event_log, 1);
        let second = rpc(
            4,
            "speak",
            serde_json::json!({"text": "LECTOR dash- BELL dash- READY bar ENV colon: xterm dash- 256color colon: unset", "interrupt": false}),
        );
        assert!(second.get("error").is_none(), "native TTS error: {second}");
        thread::sleep(Duration::from_millis(100));
        let stopped = rpc(5, "stop", Value::Null);
        assert!(
            stopped.get("error").is_none(),
            "native TTS error: {stopped}"
        );
        let third = rpc(
            6,
            "speak",
            serde_json::json!({"text": "LECTOR dash- BELL dash- READY bar ENV colon: xterm dash- 256color colon: unset", "interrupt": false}),
        );
        assert!(third.get("error").is_none(), "native TTS error: {third}");
        wait_for_speech_events(&event_log, 2);
    }

    drop(stdin);
    let status = child.wait().expect("wait for native TTS server");
    assert!(status.success());
    fs::remove_file(event_log).unwrap();
}

#[cfg(target_os = "macos")]
fn wait_for_speech_events(path: &std::path::Path, expected: usize) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let count = fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter(|line| {
                serde_json::from_str::<Value>(line).is_ok_and(|record| record["event"] == "begin")
            })
            .count();
        if count >= expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "native TTS emitted {count} of {expected} expected utterance starts"
        );
        thread::sleep(Duration::from_millis(10));
    }
}
