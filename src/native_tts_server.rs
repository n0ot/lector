//! Built-in implementation of the versioned speech-host protocol.
//!
//! Native identifiers, callback threads, and platform indexing never cross
//! this boundary. The host accepts one Lector utterance at a time and emits
//! correlated lifecycle events; Lector owns all queueing.

use crate::{
    proc_server_common::{Request, RpcError, ServerNotification, run_server_with_tick},
    speech::protocol::{
        AcceptedResult, ControlCapabilities, DeliveryGuarantee, EventCapability,
        LifecycleCapabilities, MAX_UTTERANCE_TEXT_BYTES, PauseResult, PauseResumeSupport,
        ProgressCapabilities, ProgressMode, SettingCapabilities, SettingSupport,
        SpeechCapabilities, SpeechEventNotification, SpeechEventPayload, StopSupport,
        TerminalCapability, TextPosition, UtteranceId, UtteranceParams,
    },
};
use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    collections::{BTreeMap, VecDeque},
    fs::{File, OpenOptions},
    io::Write,
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tts::{Tts, UtteranceId as NativeUtteranceId};

type SpeechEventLog = Arc<Mutex<File>>;

#[derive(Debug)]
enum NativeEvent {
    Started(NativeUtteranceId),
    Completed(NativeUtteranceId),
    Cancelled(NativeUtteranceId),
    #[cfg(target_vendor = "apple")]
    Range {
        id: NativeUtteranceId,
        start: usize,
    },
}

struct ActiveUtterance {
    logical_id: UtteranceId,
    native_id: Option<NativeUtteranceId>,
    text: String,
    /// UTF-8 offset represented by byte zero of the current native utterance.
    native_base: usize,
    current_word: usize,
    next_sequence: u64,
    started: bool,
    paused: bool,
}

struct State {
    tts: Tts,
    rate: f32,
    min_rate: f32,
    max_rate: f32,
    initialized: bool,
    capabilities: SpeechCapabilities,
    rpc_log: Option<File>,
    active: Option<ActiveUtterance>,
    native_events: mpsc::Receiver<NativeEvent>,
    notifications: VecDeque<ServerNotification>,
}

pub fn run(expected_parent_pid: Option<u32>) -> Result<()> {
    if let Some(expected_parent_pid) = expected_parent_pid {
        start_parent_watchdog(expected_parent_pid)?;
    }
    let state = RefCell::new(State::new()?);
    run_server_with_tick(
        |request| handle_request(request, &mut state.borrow_mut()),
        || {
            let mut state = state.borrow_mut();
            state.advance();
            state.notifications.drain(..).collect()
        },
    )?;
    Ok(())
}

impl State {
    fn new() -> Result<Self> {
        let tts = Tts::default().map_err(|error| anyhow::anyhow!(error))?;
        let test_event_log = configure_test_observation(&tts)?;
        let min_rate = tts.min_rate().map_err(|error| anyhow::anyhow!(error))?;
        let max_rate = tts.max_rate().map_err(|error| anyhow::anyhow!(error))?;
        let rate = tts.normal_rate().map_err(|error| anyhow::anyhow!(error))?;
        tts.set_rate(rate).map_err(|error| anyhow::anyhow!(error))?;
        let features = tts.supported_features();
        let capabilities = native_capabilities(features);
        let (native_event_tx, native_events) = mpsc::channel();
        install_native_callbacks(
            &tts,
            &native_event_tx,
            features.utterance_callbacks,
            test_event_log,
        )
        .map_err(|error| anyhow::anyhow!(error))?;
        let rpc_log = std::env::var_os("LECTOR_SPEECH_RPC_LOG")
            .map(|path| OpenOptions::new().create(true).append(true).open(path))
            .transpose()?;
        Ok(Self {
            tts,
            rate,
            min_rate,
            max_rate,
            initialized: false,
            capabilities,
            rpc_log,
            active: None,
            native_events,
            notifications: VecDeque::new(),
        })
    }

    fn speak(&mut self, id: UtteranceId, text: String) -> Result<(), RpcError> {
        self.advance();
        if self.active.is_some() {
            return Err(RpcError::invalid_request(
                "speech.speak received while another utterance is active",
            ));
        }
        if !id.is_valid() || text.is_empty() || text.len() > MAX_UTTERANCE_TEXT_BYTES {
            return Err(RpcError::invalid_params(
                "utteranceId must be valid and text must contain at most 65536 UTF-8 bytes",
            ));
        }
        let native_id = self
            .tts
            .speak(&text, false)
            .map_err(|error| RpcError::internal_error(error.to_string()))?;
        self.active = Some(ActiveUtterance {
            logical_id: id,
            native_id,
            text,
            native_base: 0,
            current_word: 0,
            next_sequence: 0,
            started: false,
            paused: false,
        });
        Ok(())
    }

    fn stop(&mut self, id: &UtteranceId) -> Result<(), RpcError> {
        self.advance();
        let Some(active) = self.active.take() else {
            return Ok(());
        };
        if active.logical_id != *id {
            self.active = Some(active);
            return Err(RpcError::invalid_params(
                "utteranceId does not identify the active utterance",
            ));
        }
        if !active.paused
            && let Err(error) = self.stop_native(active.native_id)
        {
            self.active = Some(active);
            return Err(error);
        }
        self.emit_for(&active, "ended", None, Some("cancelled"), None);
        Ok(())
    }

    fn pause(&mut self, id: &UtteranceId) -> Result<PauseResult, RpcError> {
        self.advance();
        if !self.capabilities.supports_resumable_pause() {
            return Err(RpcError::method_not_found("speech.pause"));
        }
        let Some(mut active) = self.active.take() else {
            return Ok(PauseResult {
                paused: false,
                position: None,
            });
        };
        if active.logical_id != *id {
            self.active = Some(active);
            return Err(RpcError::invalid_params(
                "utteranceId does not identify the active utterance",
            ));
        }
        if active.paused {
            let result = PauseResult {
                paused: true,
                position: Some(TextPosition::Utf8ByteOffset {
                    offset: active.current_word,
                }),
            };
            self.active = Some(active);
            return Ok(result);
        }
        let position = active.current_word;
        active.paused = true;
        if let Err(error) = self.stop_native(active.native_id) {
            active.paused = false;
            self.active = Some(active);
            return Err(error);
        }
        self.active = Some(active);
        self.emit_active(
            "paused",
            Some(TextPosition::Utf8ByteOffset { offset: position }),
            None,
            None,
        );
        Ok(PauseResult {
            paused: true,
            position: Some(TextPosition::Utf8ByteOffset { offset: position }),
        })
    }

    /// Stop the native utterance and make the backend ready for another one.
    /// AVFoundation has historically returned from `stop` without always
    /// issuing its cancellation callback. In that state it may silently drop
    /// the next utterance, so lack of terminal evidence causes a conservative
    /// synthesizer replacement.
    fn stop_native(&mut self, stopping: Option<NativeUtteranceId>) -> Result<(), RpcError> {
        self.tts
            .stop()
            .map_err(|error| RpcError::internal_error(error.to_string()))?;
        let Some(stopping) = stopping else {
            return Ok(());
        };
        let deadline = Instant::now() + Duration::from_millis(30);
        let mut confirmed = false;
        while Instant::now() < deadline {
            crate::platform::settle_speech_runloop();
            while let Ok(event) = self.native_events.try_recv() {
                confirmed |= matches!(
                    event,
                    NativeEvent::Completed(id) | NativeEvent::Cancelled(id) if id == stopping
                );
            }
            if confirmed {
                break;
            }
        }
        if !confirmed {
            self.replace_synthesizer()?;
        }
        Ok(())
    }

    fn replace_synthesizer(&mut self) -> Result<(), RpcError> {
        let tts = Tts::default().map_err(|error| RpcError::internal_error(error.to_string()))?;
        let test_event_log = configure_test_observation(&tts)
            .map_err(|error| RpcError::internal_error(error.to_string()))?;
        tts.set_rate(self.rate)
            .map_err(|error| RpcError::internal_error(error.to_string()))?;
        let features = tts.supported_features();
        let (native_event_tx, native_events) = mpsc::channel();
        install_native_callbacks(
            &tts,
            &native_event_tx,
            features.utterance_callbacks,
            test_event_log,
        )
        .map_err(|error| RpcError::internal_error(error.to_string()))?;
        self.tts = tts;
        self.native_events = native_events;
        Ok(())
    }

    fn resume(&mut self, id: &UtteranceId) -> Result<(), RpcError> {
        self.advance();
        let Some(active) = self.active.as_mut() else {
            return Err(RpcError::invalid_request("there is no paused utterance"));
        };
        if active.logical_id != *id || !active.paused {
            return Err(RpcError::invalid_params(
                "utteranceId does not identify a paused utterance",
            ));
        }
        let start = active.current_word;
        let suffix = resumable_suffix(&active.text, start).ok_or_else(|| {
            RpcError::internal_error("native word position is not a UTF-8 boundary")
        })?;
        let native_id = self
            .tts
            .speak(suffix, false)
            .map_err(|error| RpcError::internal_error(error.to_string()))?;
        active.native_id = native_id;
        active.native_base = start;
        active.paused = false;
        self.emit_active(
            "resumed",
            Some(TextPosition::Utf8ByteOffset { offset: start }),
            None,
            None,
        );
        Ok(())
    }

    fn advance(&mut self) {
        while let Ok(event) = self.native_events.try_recv() {
            self.handle_native_event(event);
        }
    }

    fn handle_native_event(&mut self, event: NativeEvent) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        let event_id = match event {
            NativeEvent::Started(id) | NativeEvent::Completed(id) | NativeEvent::Cancelled(id) => {
                id
            }
            #[cfg(target_vendor = "apple")]
            NativeEvent::Range { id, .. } => id,
        };
        if active.native_id != Some(event_id) {
            return;
        }

        match event {
            NativeEvent::Started(_) => {
                if active.started {
                    return;
                }
                active.started = true;
                self.emit_active("started", None, None, None);
            }
            #[cfg(target_vendor = "apple")]
            NativeEvent::Range { start, .. } => {
                let position = active.native_base.saturating_add(start);
                if position <= active.text.len() && active.text.is_char_boundary(position) {
                    active.current_word = position;
                    self.emit_active(
                        "progress",
                        Some(TextPosition::Utf8ByteOffset { offset: position }),
                        None,
                        None,
                    );
                }
            }
            NativeEvent::Completed(_) => {
                let active = self.active.take().expect("active checked");
                self.emit_for(&active, "ended", None, Some("completed"), None);
            }
            NativeEvent::Cancelled(_) => {
                if active.paused {
                    return;
                }
                let active = self.active.take().expect("active checked");
                self.emit_for(&active, "ended", None, Some("cancelled"), None);
            }
        }
    }

    fn emit_for(
        &mut self,
        active: &ActiveUtterance,
        kind: &str,
        position: Option<TextPosition>,
        reason: Option<&str>,
        message: Option<String>,
    ) {
        let sequence = active.next_sequence;
        self.emit(
            active.logical_id.clone(),
            sequence,
            kind,
            position,
            reason,
            message,
        );
    }

    fn emit_active(
        &mut self,
        kind: &str,
        position: Option<TextPosition>,
        reason: Option<&str>,
        message: Option<String>,
    ) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        let logical_id = active.logical_id.clone();
        let sequence = active.next_sequence;
        active.next_sequence = active.next_sequence.saturating_add(1);
        self.emit(logical_id, sequence, kind, position, reason, message);
    }

    fn emit(
        &mut self,
        logical_id: UtteranceId,
        sequence: u64,
        kind: &str,
        position: Option<TextPosition>,
        reason: Option<&str>,
        message: Option<String>,
    ) {
        let notification = SpeechEventNotification {
            utterance_id: logical_id,
            sequence,
            event: SpeechEventPayload {
                kind: kind.to_owned(),
                position,
                reason: reason.map(str::to_owned),
                message,
                extensions: BTreeMap::new(),
            },
        };
        match serde_json::to_value(notification) {
            Ok(params) => self
                .notifications
                .push_back(ServerNotification::new("speech.event", params)),
            Err(error) => crate::diagnostics::event(
                "native-speech-host",
                "serialize-event-error",
                &error.to_string(),
            ),
        }
    }
}

fn resumable_suffix(text: &str, offset: usize) -> Option<&str> {
    (offset <= text.len() && text.is_char_boundary(offset)).then(|| &text[offset..])
}

fn native_capabilities(features: tts::Features) -> SpeechCapabilities {
    let lifecycle = if features.utterance_callbacks {
        DeliveryGuarantee::Reliable
    } else {
        DeliveryGuarantee::Unsupported
    };
    SpeechCapabilities {
        lifecycle: LifecycleCapabilities {
            started: EventCapability {
                delivery: lifecycle,
                ..Default::default()
            },
            terminal: TerminalCapability {
                delivery: lifecycle,
                distinguishes: if features.utterance_callbacks {
                    vec!["completed".to_owned(), "cancelled".to_owned()]
                } else {
                    Vec::new()
                },
                ..Default::default()
            },
            ..Default::default()
        },
        progress: ProgressCapabilities {
            modes: if cfg!(target_vendor = "apple") && features.utterance_callbacks {
                vec![ProgressMode {
                    kind: "utf8ByteOffset".to_owned(),
                    granularity: vec!["word".to_owned()],
                    extensions: BTreeMap::new(),
                }]
            } else {
                Vec::new()
            },
            ..Default::default()
        },
        controls: ControlCapabilities {
            stop: if features.stop {
                if cfg!(target_vendor = "apple") {
                    StopSupport::Confirmed
                } else {
                    StopSupport::BestEffort
                }
            } else {
                StopSupport::Unsupported
            },
            pause_resume: if cfg!(target_vendor = "apple")
                && features.stop
                && features.utterance_callbacks
            {
                PauseResumeSupport::RestartFromWord
            } else {
                PauseResumeSupport::Unsupported
            },
            ..Default::default()
        },
        settings: SettingCapabilities {
            rate: if features.rate {
                SettingSupport::ReadWrite
            } else {
                SettingSupport::Unsupported
            },
            ..Default::default()
        },
        ..Default::default()
    }
}

fn install_native_callbacks(
    tts: &Tts,
    events: &mpsc::Sender<NativeEvent>,
    supported: bool,
    event_log: Option<SpeechEventLog>,
) -> std::result::Result<(), tts::Error> {
    if !supported {
        return Ok(());
    }
    let started = events.clone();
    tts.on_utterance_begin(move |id| {
        if let Some(log) = &event_log {
            write_speech_event(log, "begin");
        }
        let _ = started.send(NativeEvent::Started(id));
    })?;
    let completed = events.clone();
    tts.on_utterance_end(move |id| {
        let _ = completed.send(NativeEvent::Completed(id));
    })?;
    let cancelled = events.clone();
    tts.on_utterance_stop(move |id| {
        let _ = cancelled.send(NativeEvent::Cancelled(id));
    })?;
    #[cfg(target_vendor = "apple")]
    {
        let ranges = events.clone();
        tts.on_utterance_range(move |id, start, _length| {
            let _ = ranges.send(NativeEvent::Range { id, start });
        })?;
    }
    Ok(())
}

fn handle_request(request: Request, state: &mut State) -> Result<Value, RpcError> {
    log_request(&request, state)?;
    if request.method == "initialize" && state.initialized {
        return Err(RpcError::invalid_request(
            "speech server is already initialized",
        ));
    }
    if let Some(result) = crate::proc_server_common::handle_protocol_request(
        &request,
        "lector-native-tts",
        env!("CARGO_PKG_VERSION"),
        &state.capabilities,
    ) {
        if request.method == "initialize" && result.is_ok() {
            state.initialized = true;
        }
        return result;
    }
    if request.method.starts_with("speech.") && !state.initialized {
        return Err(RpcError::invalid_request(
            "speech server is not initialized",
        ));
    }
    match request.method.as_str() {
        "speech.speak" => {
            let params: crate::speech::protocol::SpeakParams = params(request.params)?;
            state.speak(params.utterance_id, params.text)?;
            value(AcceptedResult { accepted: true })
        }
        "speech.stop" => {
            let params: UtteranceParams = params(request.params)?;
            state.stop(&params.utterance_id)?;
            value(AcceptedResult { accepted: true })
        }
        "speech.pause" => {
            let params: UtteranceParams = params(request.params)?;
            value(state.pause(&params.utterance_id)?)
        }
        "speech.resume" => {
            let params: UtteranceParams = params(request.params)?;
            state.resume(&params.utterance_id)?;
            value(AcceptedResult { accepted: true })
        }
        "speech.setRate" => {
            let params = request
                .params
                .ok_or_else(|| RpcError::invalid_params("missing params"))?;
            let rate = params
                .get("rate")
                .and_then(Value::as_f64)
                .filter(|rate| rate.is_finite())
                .ok_or_else(|| RpcError::invalid_params("rate must be finite"))?;
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

fn params<T: DeserializeOwned>(value: Option<Value>) -> Result<T, RpcError> {
    serde_json::from_value(value.ok_or_else(|| RpcError::invalid_params("missing params"))?)
        .map_err(|error| RpcError::invalid_params(error.to_string()))
}

fn value<T: serde::Serialize>(value: T) -> Result<Value, RpcError> {
    serde_json::to_value(value).map_err(|error| RpcError::internal_error(error.to_string()))
}

fn log_request(request: &Request, state: &mut State) -> Result<(), RpcError> {
    let Some(log) = &mut state.rpc_log else {
        return Ok(());
    };
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
        .map_err(|error| RpcError::internal_error(format!("write speech RPC log: {error}")))
}

/// Ensure a native speech host cannot survive an abruptly terminated Lector.
fn start_parent_watchdog(expected_parent_pid: u32) -> Result<()> {
    std::thread::Builder::new()
        .name("lector-speech-parent-watchdog".to_owned())
        .spawn(move || {
            loop {
                let actual_parent_pid = nix::unistd::getppid().as_raw() as u32;
                if actual_parent_pid != expected_parent_pid {
                    // SAFETY: the owning process is gone and arbitrary native
                    // destructors must not keep this helper alive.
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

#[cfg(test)]
mod tests {
    use super::resumable_suffix;

    #[test]
    fn resume_suffix_starts_at_the_reported_utf8_word_boundary() {
        assert_eq!(resumable_suffix("héllo world", 7), Some("world"));
        assert_eq!(resumable_suffix("héllo world", 2), None);
        assert_eq!(resumable_suffix("héllo world", 99), None);
    }
}
