use lector::speech::{Driver, proc_driver::ProcDriver};
use std::path::PathBuf;

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
