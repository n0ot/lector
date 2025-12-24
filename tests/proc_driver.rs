use lector::speech::{Driver, proc_driver::ProcDriver};
use std::path::PathBuf;

#[test]
fn proc_driver_smoke() {
    let server_path = PathBuf::from(env!("CARGO_BIN_EXE_proc_stub_server"));
    let mut driver = ProcDriver::new(&server_path).expect("spawn proc stub server");
    driver.speak("hello", true).expect("speak");
    driver.set_rate(1.25).expect("set_rate");
    assert!((driver.get_rate() - 1.25).abs() < f32::EPSILON);
    driver.stop().expect("stop");
}
