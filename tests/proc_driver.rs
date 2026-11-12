use lector::{
    proc_server_common::MAX_RPC_FRAME_BYTES,
    speech::{
        manager::Host,
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
    assert!((driver.get_rate() - 50.0).abs() < f32::EPSILON);
    driver.speak("hello", true).expect("speak");
    driver.set_rate(65.0).expect("set_rate");
    assert!((driver.get_rate() - 65.0).abs() < f32::EPSILON);
    driver.stop().expect("stop");
}

#[test]
fn proc_driver_negotiates_the_current_protocol() {
    let server_path = PathBuf::from(env!("CARGO_BIN_EXE_proc_stub_server"));
    let mut driver = ProcDriver::new(&server_path).expect("spawn proc stub server");
    assert!(!driver.is_legacy_protocol());
    assert_eq!(Host::backend_info(&driver).unwrap().id, "stub");
    assert!(Host::capabilities(&driver).voices.list);
    assert!(Host::capabilities(&driver).voices.current);
    assert!(Host::capabilities(&driver).voices.select);
    assert!(Host::capabilities(&driver).settings.pitch.can_write());
    assert!(Host::capabilities(&driver).settings.volume.can_write());

    assert_eq!(Host::get_rate(&mut driver).unwrap(), 50.0);
    assert_eq!(Host::set_rate(&mut driver, 65.0).unwrap(), 65.0);
    assert_eq!(Host::get_rate(&mut driver).unwrap(), 65.0);
    assert_eq!(Host::get_pitch(&mut driver).unwrap(), 1.0);
    assert_eq!(Host::set_pitch(&mut driver, 0.75).unwrap(), 0.75);
    assert_eq!(Host::get_pitch(&mut driver).unwrap(), 0.75);
    assert_eq!(Host::get_volume(&mut driver).unwrap(), 1.0);
    assert_eq!(Host::set_volume(&mut driver, 0.5).unwrap(), 0.5);
    assert_eq!(Host::get_volume(&mut driver).unwrap(), 0.5);

    let voices = Host::list_voices(&mut driver).expect("list voices");
    assert_eq!(
        voices
            .iter()
            .map(|voice| voice.id.as_str())
            .collect::<Vec<_>>(),
        ["stub-a", "stub-b"]
    );
    assert_eq!(
        Host::current_voice(&mut driver)
            .expect("get current voice")
            .voice
            .unwrap()
            .id,
        "stub-a"
    );
    assert_eq!(
        Host::set_voice(&mut driver, "stub-b")
            .expect("select voice")
            .voice
            .unwrap()
            .id,
        "stub-b"
    );
    assert_eq!(
        Host::current_voice(&mut driver)
            .expect("get selected voice")
            .voice
            .unwrap()
            .id,
        "stub-b"
    );
}

#[test]
fn proc_driver_accepts_a_method_not_found_initialize_as_legacy() {
    let server_path = PathBuf::from(env!("CARGO_BIN_EXE_proc_stub_server"));
    let mut driver = ProcDriver::new_with_args(&server_path, ["--legacy"])
        .expect("spawn legacy proc stub server");
    assert!(driver.is_legacy_protocol());
    driver.speak("legacy speech", false).expect("legacy speak");
    driver.set_rate(70.0).expect("legacy set_rate");
    assert!((driver.get_rate() - 70.0).abs() < f32::EPSILON);
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
    for method in [
        "speech.getRate",
        "speech.getPitch",
        "speech.setPitch",
        "speech.getVolume",
        "speech.setVolume",
        "speech.listVoices",
        "speech.getVoice",
        "speech.setVoice",
    ] {
        assert!(
            response["result"]["methods"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["name"] == method)
        );
    }
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
fn proc_driver_interleaves_extensible_notifications_with_a_response() {
    let mut driver = adversarial_driver("event-before-response");
    driver.speak("hello", false).unwrap();

    let events = Host::take_events(&mut driver).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].utterance_id.as_str(), "direct-1");
    assert_eq!(events[0].event.kind, "started");
    assert_eq!(
        events[0].event.extensions.get("futureMember"),
        Some(&serde_json::json!(7))
    );
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
fn proc_driver_rejects_invalid_normalized_rate_without_changing_it() {
    let server_path = PathBuf::from(env!("CARGO_BIN_EXE_proc_stub_server"));
    let mut driver = ProcDriver::new(&server_path).expect("spawn proc stub server");

    for rate in [-0.01, 100.01, f32::NAN] {
        let error = driver.set_rate(rate).unwrap_err();
        assert!(error.to_string().contains("between 0 and 100"));
    }
    assert!((driver.get_rate() - 50.0).abs() < f32::EPSILON);
}

#[test]
fn proc_driver_rejects_an_out_of_range_rate_from_the_host() {
    let mut driver = adversarial_driver("out-of-range-rate-result");

    let error = Host::get_rate(&mut driver).unwrap_err();
    let error = error.downcast_ref::<ProcError>().expect("proc error");
    assert!(matches!(error, ProcError::InvalidResponse(_)));
    assert!(error.is_transport_failure());
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
mod native_speech {
    use super::*;

    #[test]
    #[ignore = "requires native audio; run scripts/test-native-speech"]
    fn lector_tts_host_advances_past_its_first_utterance() {
        let directory = tempfile::tempdir().expect("speech test directory");
        let event_log = directory.path().join("events.jsonl");
        let mut host = NativeTestHost::spawn(
            Command::new(env!("CARGO_BIN_EXE_lector"))
                .arg("tts")
                .env("LECTOR_SPEECH_TEST_MUTE", "1")
                .env("LECTOR_SPEECH_EVENT_LOG", &event_log),
        );

        {
            let sample_text = "A sample sentence for the speech lifecycle test.";
            let before_initialize = host.rpc(
                1,
                "speech.speak",
                serde_json::json!({"utteranceId": "before", "text": "sample greeting"}),
            );
            assert_eq!(before_initialize["error"]["code"], -32600);
            assert!(
                before_initialize["error"]["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("not initialized"))
            );

            let initialized = host.rpc(
                2,
                "initialize",
                serde_json::json!({
                    "protocol": {"major": 2, "minimumMinor": 0, "maximumMinor": 0},
                    "client": {"name": "lector-test", "version": "1"},
                    "clientCapabilities": {
                        "speechEvents": true,
                        "progressModes": ["marker", "utf8ByteOffset"]
                    }
                }),
            );
            assert_eq!(
                initialized["result"]["protocol"],
                serde_json::json!({"major": 2, "minor": 0})
            );

            let first = host.rpc(
                3,
                "speech.speak",
                serde_json::json!({"utteranceId": "first", "text": "sample greeting"}),
            );
            assert!(first.get("error").is_none(), "native TTS error: {first}");
            wait_for_speech_events(&event_log, 1);
            host.wait_for_completion("first");
            let second = host.rpc(
                4,
                "speech.speak",
                serde_json::json!({"utteranceId": "second", "text": sample_text}),
            );
            assert!(second.get("error").is_none(), "native TTS error: {second}");
            thread::sleep(Duration::from_millis(100));
            let stopped = host.rpc(
                5,
                "speech.stop",
                serde_json::json!({"utteranceId": "second"}),
            );
            assert!(
                stopped.get("error").is_none(),
                "native TTS error: {stopped}"
            );
            let third = host.rpc(
                6,
                "speech.speak",
                serde_json::json!({"utteranceId": "third", "text": sample_text}),
            );
            assert!(third.get("error").is_none(), "native TTS error: {third}");
            wait_for_speech_events(&event_log, 2);
            host.wait_for_completion("third");
        }

        host.finish();
    }
}

#[cfg(target_os = "macos")]
const NATIVE_TEST_DEADLINE: Duration = Duration::from_secs(15);

/// Frame reception is asynchronous so an unresponsive project-built host cannot
/// block the test in read_line. Unrelated notifications remain available to the
/// next expectation, including events received while waiting for an RPC reply.
#[cfg(target_os = "macos")]
struct NativeTestFrames {
    receiver: mpsc::Receiver<Result<Value, String>>,
    pending: std::collections::VecDeque<Value>,
}

#[cfg(target_os = "macos")]
impl NativeTestFrames {
    fn wait_for(
        &mut self,
        timeout: Duration,
        description: &str,
        matches: impl Fn(&Value) -> bool,
    ) -> Result<Value, String> {
        if let Some(index) = self.pending.iter().position(&matches) {
            return Ok(self.pending.remove(index).expect("matching pending frame"));
        }
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(format!("timed out waiting for {description}"));
            }
            let frame = self
                .receiver
                .recv_timeout(remaining)
                .map_err(|error| format!("waiting for {description}: {error}"))??;
            if matches(&frame) {
                return Ok(frame);
            }
            if self.pending.len() == 128 {
                return Err(format!(
                    "too many unrelated frames while waiting for {description}"
                ));
            }
            self.pending.push_back(frame);
        }
    }

    fn completion(&mut self, id: &str, timeout: Duration) -> Result<(), String> {
        let event = self.wait_for(
            timeout,
            &format!("natural completion of utterance {id}"),
            |frame| {
                frame["method"] == "speech.event"
                    && frame["params"]["utteranceId"] == id
                    && frame["params"]["event"]["type"] == "ended"
            },
        )?;
        if event["params"]["event"]["reason"] != "completed" {
            return Err(format!(
                "utterance {id} did not complete naturally: {}",
                event["params"]["event"]
            ));
        }
        Ok(())
    }
}

/// Own the host, its pipes and its reader together. Assertion unwinding closes
/// stdin and terminates/reaps the child instead of leaving a native synthesizer
/// behind to interfere with the next test.
#[cfg(target_os = "macos")]
struct NativeTestHost {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    frames: Option<NativeTestFrames>,
    reader: Option<thread::JoinHandle<()>>,
    reader_done: mpsc::Receiver<()>,
}

#[cfg(target_os = "macos")]
impl NativeTestHost {
    fn spawn(command: &mut Command) -> Self {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn native test host");
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("native test stdout");
        let (sender, receiver) = mpsc::sync_channel(16);
        let (done, reader_done) = mpsc::channel();
        let reader = thread::spawn(move || {
            use std::io::Read;
            let mut stdout = BufReader::new(stdout);
            loop {
                let mut bytes = Vec::new();
                let frame = match stdout
                    .by_ref()
                    .take((MAX_RPC_FRAME_BYTES + 1) as u64)
                    .read_until(b'\n', &mut bytes)
                {
                    Ok(0) => Err("native test host closed stdout".to_owned()),
                    Ok(_) if bytes.len() > MAX_RPC_FRAME_BYTES => {
                        Err("native test frame exceeds size bound".to_owned())
                    }
                    Ok(_) => serde_json::from_slice(&bytes)
                        .map_err(|error| format!("invalid native test frame: {error}")),
                    Err(error) => Err(format!("read native test frame: {error}")),
                };
                let failed = frame.is_err();
                if sender.send(frame).is_err() || failed {
                    break;
                }
            }
            let _ = done.send(());
        });
        Self {
            child,
            stdin,
            frames: Some(NativeTestFrames {
                receiver,
                pending: Default::default(),
            }),
            reader: Some(reader),
            reader_done,
        }
    }

    fn rpc(&mut self, id: u64, method: &str, params: Value) -> Value {
        let stdin = self.stdin.as_mut().expect("native test stdin");
        writeln!(
            stdin,
            "{}",
            serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
        )
        .expect("write native TTS request");
        stdin.flush().expect("flush native TTS request");
        self.frames
            .as_mut()
            .unwrap()
            .wait_for(
                NATIVE_TEST_DEADLINE,
                &format!("RPC {method} response {id}"),
                |frame| frame["id"] == id,
            )
            .unwrap_or_else(|error| panic!("{error}"))
    }

    fn wait_for_completion(&mut self, id: &str) {
        self.frames
            .as_mut()
            .unwrap()
            .completion(id, NATIVE_TEST_DEADLINE)
            .unwrap_or_else(|error| panic!("{error}"));
    }

    fn finish(&mut self) {
        self.stdin.take();
        let status = self
            .wait_for_exit(NATIVE_TEST_DEADLINE)
            .expect("native test host did not exit after EOF");
        assert!(status.success(), "native test host exit: {status}");
    }

    fn wait_for_exit(&mut self, timeout: Duration) -> Option<std::process::ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return Some(status),
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                _ => return None,
            }
        }
    }
}

#[cfg(target_os = "macos")]
impl Drop for NativeTestHost {
    fn drop(&mut self) {
        self.stdin.take();
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            if self.wait_for_exit(Duration::from_secs(2)).is_none() {
                eprintln!(
                    "native test host {} did not exit after kill",
                    self.child.id()
                );
            }
        }
        // Disconnect a reader blocked on the bounded channel before joining.
        self.frames.take();
        if self
            .reader_done
            .recv_timeout(Duration::from_secs(2))
            .is_ok()
            && let Some(reader) = self.reader.take()
        {
            let _ = reader.join();
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn native_test_frame_waits_preserve_events_and_require_natural_completion() {
    let (sender, receiver) = mpsc::channel();
    let complete = serde_json::json!({"method":"speech.event","params":{"utteranceId":"sample","event":{"type":"ended","reason":"completed"}}});
    sender.send(Ok(complete)).unwrap();
    sender
        .send(Ok(serde_json::json!({"id":1,"result":{}})))
        .unwrap();
    let mut frames = NativeTestFrames {
        receiver,
        pending: Default::default(),
    };
    assert!(
        frames
            .wait_for(Duration::from_millis(100), "reply", |frame| frame["id"]
                == 1)
            .is_ok()
    );
    assert!(
        frames
            .completion("sample", Duration::from_millis(100))
            .is_ok()
    );
    sender.send(Ok(serde_json::json!({"method":"speech.event","params":{"utteranceId":"sample","event":{"type":"ended","reason":"cancelled"}}}))).unwrap();
    assert!(
        frames
            .completion("sample", Duration::from_millis(100))
            .unwrap_err()
            .contains("did not complete naturally")
    );
    assert!(
        frames
            .completion("missing", Duration::from_millis(1))
            .is_err()
    );
    drop(sender);
    assert!(
        frames
            .wait_for(Duration::from_millis(100), "closed stream", |_| true)
            .is_err()
    );
}

#[cfg(target_os = "macos")]
#[test]
fn native_test_host_cleanup_reaps_an_unresponsive_project_fixture() {
    let mut host = NativeTestHost::spawn(
        Command::new(env!("CARGO_BIN_EXE_proc_stub_server"))
            .args(["--adversary", "stall-on-initialize"]),
    );
    let pid = host.child.id();
    // The fixture deliberately does not reply. Exercise the same bounded wait
    // without native speech or any external host application.
    writeln!(
        host.stdin.as_mut().unwrap(),
        "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}}"
    )
    .unwrap();
    assert!(
        host.frames
            .as_mut()
            .unwrap()
            .wait_for(Duration::from_millis(100), "stalled fixture", |_| true)
            .is_err()
    );
    drop(host);
    let result = nix::sys::wait::waitpid(
        nix::unistd::Pid::from_raw(pid as i32),
        Some(nix::sys::wait::WaitPidFlag::WNOHANG),
    );
    assert_eq!(
        result,
        Err(nix::errno::Errno::ECHILD),
        "fixture must already be reaped"
    );
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
