use lector::speech::{Driver, proc_driver::ProcDriver};
use serde_json::Value;
use std::{
    fs,
    io::Write,
    path::PathBuf,
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
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
