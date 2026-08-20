#[cfg(not(target_os = "macos"))]
use crate::proc_server_common::run_server;
#[cfg(target_os = "macos")]
use crate::proc_server_common::run_server_with_tick;
use crate::proc_server_common::{Request, RpcError};
use anyhow::{Context, Result};
use serde_json::{Value, json};
#[cfg(target_os = "macos")]
use std::{
    cell::RefCell,
    collections::VecDeque,
    sync::mpsc::{self, Receiver, Sender},
    time::{Duration, Instant},
};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use tts::Tts;
#[cfg(target_os = "macos")]
use tts::UtteranceId;

#[cfg(target_os = "macos")]
const MAX_PENDING_UTTERANCES: usize = 32;

type SpeechEventLog = Arc<Mutex<File>>;

struct State {
    tts: Tts,
    rate: f32,
    min_rate: f32,
    max_rate: f32,
    initialized: bool,
    rpc_log: Option<File>,
    #[cfg(target_os = "macos")]
    active: Option<UtteranceId>,
    #[cfg(target_os = "macos")]
    pending: VecDeque<String>,
    #[cfg(target_os = "macos")]
    completed: Receiver<UtteranceId>,
    #[cfg(target_os = "macos")]
    completed_tx: Sender<UtteranceId>,
}

pub fn run(expected_parent_pid: Option<u32>) -> Result<()> {
    if let Some(expected_parent_pid) = expected_parent_pid {
        start_parent_watchdog(expected_parent_pid)?;
    }
    let tts = Tts::default().map_err(|error| anyhow::anyhow!(error))?;
    let _test_event_log = configure_test_observation(&tts)?;
    let min_rate = tts.min_rate().map_err(|error| anyhow::anyhow!(error))?;
    let max_rate = tts.max_rate().map_err(|error| anyhow::anyhow!(error))?;
    let rate = tts.normal_rate().map_err(|error| anyhow::anyhow!(error))?;
    tts.set_rate(rate).map_err(|error| anyhow::anyhow!(error))?;
    let rpc_log = std::env::var_os("LECTOR_SPEECH_RPC_LOG")
        .map(|path| OpenOptions::new().create(true).append(true).open(path))
        .transpose()?;
    #[cfg(target_os = "macos")]
    let (completed_tx, completed) = {
        let (completed_tx, completed_rx) = mpsc::channel();
        install_lifecycle_callbacks(&tts, &completed_tx).map_err(|error| anyhow::anyhow!(error))?;
        (completed_tx, completed_rx)
    };
    let state = State {
        tts,
        rate,
        min_rate,
        max_rate,
        initialized: false,
        rpc_log,
        #[cfg(target_os = "macos")]
        active: None,
        #[cfg(target_os = "macos")]
        pending: VecDeque::new(),
        #[cfg(target_os = "macos")]
        completed,
        #[cfg(target_os = "macos")]
        completed_tx,
    };
    #[cfg(target_os = "macos")]
    {
        let state = RefCell::new(state);
        run_server_with_tick(
            |request| handle_request(request, &mut state.borrow_mut()),
            || state.borrow_mut().advance(),
        )?;
    }
    #[cfg(not(target_os = "macos"))]
    {
        let mut state = state;
        run_server(|request| handle_request(request, &mut state))?;
    }
    Ok(())
}

/// Ensure a native speech host cannot survive an abruptly terminated Lector.
///
/// Pipe EOF handles ordinary parent shutdown, but it cannot help while the
/// server thread is stuck in foreign speech code. This independent watchdog
/// deliberately uses `_exit`: once the owning process is gone there is no
/// useful in-process cleanup left to run, and waiting for arbitrary native
/// destructors could preserve the orphan we are trying to prevent.
fn start_parent_watchdog(expected_parent_pid: u32) -> Result<()> {
    std::thread::Builder::new()
        .name("lector-speech-parent-watchdog".to_owned())
        .spawn(move || {
            loop {
                let actual_parent_pid = nix::unistd::getppid().as_raw() as u32;
                if actual_parent_pid != expected_parent_pid {
                    // SAFETY: the parent has died, so this helper must
                    // terminate even if another thread is blocked in foreign
                    // speech code. `_exit` is async-signal-safe and does not
                    // run potentially blocking destructors.
                    unsafe { nix::libc::_exit(1) };
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        })
        .map(|_| ())
        .context("start native speech parent watchdog")
}

fn configure_test_observation(tts: &Tts) -> Result<Option<SpeechEventLog>> {
    if std::env::var_os("LECTOR_SPEECH_TEST_MUTE").is_some() {
        tts.set_volume(0.0)
            .map_err(|error| anyhow::anyhow!(error))?;
    }
    if let Some(path) = std::env::var_os("LECTOR_SPEECH_EVENT_LOG") {
        let log: SpeechEventLog = Arc::new(Mutex::new(
            OpenOptions::new().create(true).append(true).open(path)?,
        ));
        let begin_log = Arc::clone(&log);
        tts.on_utterance_begin(move |_| write_speech_event(&begin_log, "begin"))
            .map_err(|error| anyhow::anyhow!(error))?;
        return Ok(Some(log));
    }
    Ok(None)
}

fn write_speech_event(log: &Mutex<File>, event: &str) {
    let Ok(mut log) = log.lock() else {
        return;
    };
    let time_unix_us = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64;
    if serde_json::to_writer(
        &mut *log,
        &json!({"time_unix_us": time_unix_us, "event": event}),
    )
    .is_ok()
    {
        let _ = writeln!(log).and_then(|()| log.flush());
    }
}

#[cfg(target_os = "macos")]
impl State {
    fn speak(&mut self, text: &str, interrupt: bool) -> Result<(), RpcError> {
        self.advance();
        if interrupt {
            self.stop()?;
            self.active = self
                .tts
                .speak(text, false)
                .map_err(|error| RpcError::internal_error(error.to_string()))?;
        } else if self.active.is_some() {
            if self.pending.len() == MAX_PENDING_UTTERANCES {
                self.pending.pop_front();
            }
            self.pending.push_back(text.to_owned());
        } else {
            self.active = self
                .tts
                .speak(text, false)
                .map_err(|error| RpcError::internal_error(error.to_string()))?;
        }
        Ok(())
    }

    fn stop(&mut self) -> Result<(), RpcError> {
        self.pending.clear();
        let stopping = self.active.take();
        self.tts
            .stop()
            .map_err(|error| RpcError::internal_error(error.to_string()))?;
        if let Some(stopping) = stopping {
            let deadline = Instant::now() + Duration::from_millis(30);
            let mut stopped = false;
            while Instant::now() < deadline {
                crate::platform::settle_speech_runloop();
                while let Ok(completed) = self.completed.try_recv() {
                    stopped |= completed == stopping;
                }
                if stopped {
                    break;
                }
            }
            if !stopped {
                self.replace_synthesizer()?;
            }
        }
        Ok(())
    }

    fn replace_synthesizer(&mut self) -> Result<(), RpcError> {
        let tts = Tts::default().map_err(|error| RpcError::internal_error(error.to_string()))?;
        let _test_event_log = configure_test_observation(&tts)
            .map_err(|error| RpcError::internal_error(error.to_string()))?;
        tts.set_rate(self.rate)
            .map_err(|error| RpcError::internal_error(error.to_string()))?;
        install_lifecycle_callbacks(&tts, &self.completed_tx)
            .map_err(|error| RpcError::internal_error(error.to_string()))?;
        self.tts = tts;
        while self.completed.try_recv().is_ok() {}
        Ok(())
    }

    fn advance(&mut self) {
        while let Ok(completed) = self.completed.try_recv() {
            if self.active == Some(completed) {
                self.active = None;
            }
        }
        if self.active.is_some() {
            return;
        }
        let Some(text) = self.pending.pop_front() else {
            return;
        };
        match self.tts.speak(&text, false) {
            Ok(active) => self.active = active,
            Err(error) => {
                crate::diagnostics::event("native-speech-host", "backend-error", &error.to_string())
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn install_lifecycle_callbacks(
    tts: &Tts,
    completed_tx: &Sender<UtteranceId>,
) -> std::result::Result<(), tts::Error> {
    let ended_tx = completed_tx.clone();
    let stopped_tx = completed_tx.clone();
    tts.on_utterance_end(move |id| {
        let _ = ended_tx.send(id);
    })?;
    tts.on_utterance_stop(move |id| {
        let _ = stopped_tx.send(id);
    })?;
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
    if request.method == "initialize" && state.initialized {
        return Err(RpcError::invalid_request(
            "speech server is already initialized",
        ));
    }
    if let Some(result) = crate::proc_server_common::handle_protocol_request(
        &request,
        "lector-native-tts",
        env!("CARGO_PKG_VERSION"),
    ) {
        if request.method == "initialize" && result.is_ok() {
            state.initialized = true;
        }
        return result;
    }
    if matches!(request.method.as_str(), "speak" | "stop" | "set_rate") && !state.initialized {
        return Err(RpcError::invalid_request(
            "speech server is not initialized",
        ));
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
            #[cfg(target_os = "macos")]
            state.speak(text, interrupt)?;
            #[cfg(not(target_os = "macos"))]
            state
                .tts
                .speak(text, interrupt)
                .map_err(|error| RpcError::internal_error(error.to_string()))?;
            Ok(Value::Null)
        }
        "stop" => {
            #[cfg(target_os = "macos")]
            state.stop()?;
            #[cfg(not(target_os = "macos"))]
            state
                .tts
                .stop()
                .map_err(|error| RpcError::internal_error(error.to_string()))?;
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
                .map_err(|error| RpcError::internal_error(error.to_string()))?;
            state.rate = clamped;
            Ok(json!({ "rate": state.rate }))
        }
        _ => Err(RpcError::method_not_found(request.method)),
    }
}
