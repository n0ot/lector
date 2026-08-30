use lector::speech::{
    Driver, SpeechServerSpec,
    supervisor::{Supervisor, SupervisorEvent},
};
use serde_json::Value;
#[cfg(target_os = "macos")]
use std::time::{Duration, Instant};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(name: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "lector-speech-supervisor-{name}-{}-{unique}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create speech supervisor test directory");
        Self(path)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct StubSpec {
    state: PathBuf,
    log: PathBuf,
    spec: SpeechServerSpec,
}

fn stub_spec(
    directory: &TestDirectory,
    prefix: &str,
    identity: &str,
    fail_start_generations: Option<&str>,
    crash_speak_generations: Option<&str>,
) -> StubSpec {
    let state = directory.path(&format!("{prefix}-state"));
    let log = directory.path(&format!("{prefix}-rpc.jsonl"));
    let mut args = vec![
        "--lifecycle-state".to_owned(),
        state.display().to_string(),
        "--rpc-log".to_owned(),
        log.display().to_string(),
        "--identity".to_owned(),
        identity.to_owned(),
    ];
    if let Some(generations) = fail_start_generations {
        args.extend([
            "--fail-start-generations".to_owned(),
            generations.to_owned(),
        ]);
    }
    if let Some(generations) = crash_speak_generations {
        args.extend([
            "--crash-speak-generations".to_owned(),
            generations.to_owned(),
        ]);
    }
    StubSpec {
        state,
        log,
        spec: SpeechServerSpec::Process {
            program: env!("CARGO_BIN_EXE_proc_stub_server").to_owned(),
            args,
        },
    }
}

fn generation(path: &Path) -> u64 {
    fs::read_to_string(path)
        .expect("read lifecycle state")
        .trim()
        .parse()
        .expect("parse lifecycle generation")
}

fn records(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .expect("read RPC log")
        .lines()
        .map(|line| serde_json::from_str(line).expect("parse RPC log record"))
        .collect()
}

fn speech_records(path: &Path) -> Vec<Value> {
    records(path)
        .into_iter()
        .filter(|record| matches!(record["method"].as_str(), Some("speak" | "speech.speak")))
        .collect()
}

#[test]
fn real_process_startup_retries_once_after_the_first_generation_fails() {
    let directory = TestDirectory::new("startup-retry");
    let stub = stub_spec(&directory, "server", "retry", Some("1"), None);
    let mut supervisor = Supervisor::new(stub.spec);

    supervisor.set_rate(1.75).unwrap();
    supervisor.start().expect("second generation starts");

    assert_eq!(generation(&stub.state), 2);
    let records = records(&stub.log);
    assert!(records.iter().any(|record| {
        record["generation"] == 2
            && record["method"] == "initialize"
            && record["identity"] == "retry"
    }));
    assert!(records.iter().any(|record| {
        record["generation"] == 2
            && matches!(
                record["method"].as_str(),
                Some("set_rate" | "speech.setRate")
            )
            && record["params"]["rate"].as_f64() == Some(1.75)
    }));
}

#[test]
fn real_process_startup_fails_after_exactly_two_attempts() {
    let directory = TestDirectory::new("startup-fails-twice");
    let stub = stub_spec(&directory, "server", "never-ready", Some("1,2"), None);
    let mut supervisor = Supervisor::new(stub.spec);

    let error = supervisor.start().expect_err("both starts must fail");
    assert!(format!("{error:#}").contains("startup failed twice"));
    assert_eq!(generation(&stub.state), 2);

    supervisor
        .start()
        .expect_err("a terminal startup error must remain terminal");
    assert_eq!(generation(&stub.state), 2, "must not attempt a third start");
}

#[test]
fn real_process_crash_restarts_without_replaying_uncertain_speech() {
    let directory = TestDirectory::new("runtime-restart");
    let stub = stub_spec(&directory, "server", "restart", None, Some("1"));
    let mut supervisor = Supervisor::new(stub.spec);
    let handle = supervisor.handle();
    supervisor.set_rate(1.625).unwrap();
    supervisor.start().unwrap();

    supervisor
        .speak("possibly delivered", false)
        .expect_err("first server dies before acknowledging speech");
    assert_eq!(generation(&stub.state), 2, "the server is restarted once");
    supervisor.speak("definitely later", true).unwrap();

    let speech = speech_records(&stub.log);
    assert_eq!(speech.len(), 2);
    assert_eq!(speech[0]["generation"], 1);
    assert_eq!(speech[0]["params"]["text"], "possibly delivered");
    assert_eq!(speech[1]["generation"], 2);
    assert_eq!(speech[1]["params"]["text"], "definitely later");
    assert!(speech.iter().all(|record| {
        record["generation"] != 2 || record["params"]["text"] != "possibly delivered"
    }));
    assert!(records(&stub.log).iter().any(|record| {
        record["generation"] == 2
            && matches!(
                record["method"].as_str(),
                Some("set_rate" | "speech.setRate")
            )
            && record["params"]["rate"].as_f64() == Some(1.625)
    }));
    assert!(handle.take_events().is_empty());
}

#[test]
fn second_real_process_crash_inside_thirty_seconds_is_fatal() {
    let directory = TestDirectory::new("second-runtime-crash");
    let stub = stub_spec(&directory, "server", "crash-twice", None, Some("1,2"));
    let mut supervisor = Supervisor::new(stub.spec);
    let handle = supervisor.handle();
    supervisor.start().unwrap();

    supervisor.speak("first crash", false).unwrap_err();
    supervisor.speak("second crash", false).unwrap_err();

    assert_eq!(
        generation(&stub.state),
        2,
        "fatal path must not spawn again"
    );
    assert!(matches!(
        handle.take_events().as_slice(),
        [SupervisorEvent::Fatal(message)] if message.contains("within 30 seconds")
    ));
}

#[test]
fn failed_real_process_restart_is_fatal() {
    let directory = TestDirectory::new("restart-fails");
    let stub = stub_spec(
        &directory,
        "server",
        "restart-failure",
        Some("2"),
        Some("1"),
    );
    let mut supervisor = Supervisor::new(stub.spec);
    let handle = supervisor.handle();
    supervisor.start().unwrap();

    supervisor
        .speak("crash before failed restart", false)
        .unwrap_err();

    assert_eq!(generation(&stub.state), 2);
    assert!(matches!(
        handle.take_events().as_slice(),
        [SupervisorEvent::Fatal(message)] if message.contains("restarting the speech server failed")
    ));
}

#[test]
fn real_process_reconfiguration_is_transactional_and_preserves_exact_args() {
    let directory = TestDirectory::new("reconfigure");
    let old = stub_spec(&directory, "old", "old server", None, None);
    let mut supervisor = Supervisor::new(old.spec);
    let handle = supervisor.handle();
    supervisor.set_rate(1.375).unwrap();
    supervisor.start().unwrap();

    let rejected = SpeechServerSpec::Process {
        program: directory
            .path("missing speech server")
            .display()
            .to_string(),
        args: vec!["never-run".to_owned()],
    };
    supervisor
        .configure_server(rejected)
        .expect_err("missing replacement is rejected");
    assert!(matches!(
        handle.take_events().as_slice(),
        [SupervisorEvent::ReconfigureFailed(message)] if message.contains("replace speech server")
    ));
    supervisor.speak("old remains active", false).unwrap();
    assert!(speech_records(&old.log).iter().any(|record| {
        record["identity"] == "old server" && record["params"]["text"] == "old remains active"
    }));

    let exact_identity = "two words, '$not-expanded', and \\backslashes";
    let replacement = stub_spec(&directory, "new", exact_identity, None, None);
    let accepted = replacement.spec.clone();
    supervisor.configure_server(accepted.clone()).unwrap();
    assert_eq!(
        handle.take_events(),
        [SupervisorEvent::Reconfigured(accepted)]
    );
    supervisor.speak("new is active", true).unwrap();

    let replacement_records = records(&replacement.log);
    assert!(replacement_records.iter().all(|record| {
        record["identity"].as_str() == Some(exact_identity) && record["generation"] == 1
    }));
    assert!(replacement_records.iter().any(|record| {
        matches!(
            record["method"].as_str(),
            Some("set_rate" | "speech.setRate")
        ) && record["params"]["rate"].as_f64() == Some(1.375)
    }));
    assert!(
        speech_records(&replacement.log)
            .iter()
            .any(|record| record["params"]["text"] == "new is active")
    );
    assert!(
        !speech_records(&old.log)
            .iter()
            .any(|record| record["params"]["text"] == "new is active")
    );
}

#[cfg(target_os = "macos")]
#[test]
fn native_speech_parent_watchdog_rejects_a_mismatched_parent_promptly() {
    let started = Instant::now();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_lector"))
        .args(["tts", "--parent-pid", &u32::MAX.to_string()])
        .env("LECTOR_SPEECH_TEST_MUTE", "1")
        .spawn()
        .expect("spawn native speech helper with mismatched parent");
    let deadline = started + Duration::from_secs(2);
    loop {
        if let Some(status) = child.try_wait().expect("query native speech helper") {
            assert!(!status.success());
            assert!(started.elapsed() < Duration::from_secs(2));
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("native speech parent watchdog did not exit within two seconds");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
