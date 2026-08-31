//! Built-in implementation of the versioned speech-host protocol.
//!
//! Native identifiers, callback threads, and platform indexing never cross
//! this boundary. The host accepts one Lector utterance at a time and emits
//! correlated lifecycle events; Lector owns all queueing.

use crate::{
    Backends, Error as TtsError, Features, Gender, Tts, UtteranceId as NativeUtteranceId, Voice,
    protocol::{
        AcceptedResult, BackendInfo, ControlCapabilities, CurrentVoiceResult, DeliveryGuarantee,
        EventCapability, LifecycleCapabilities, MAX_UTTERANCE_TEXT_BYTES, PauseResult,
        PauseResumeSupport, PitchResult, ProgressCapabilities, ProgressMode, RateResult,
        SetVoiceParams, SettingCapabilities, SettingSupport, SpeakParams, SpeechCapabilities,
        SpeechEventNotification, SpeechEventPayload, StopSupport, TerminalCapability, TextPosition,
        UtteranceId, UtteranceParams, VoiceCapabilities, VoiceInfo, VoiceListResult, VolumeResult,
    },
    server::{InitializeResult, Request, RpcError, ServerNotification, run_server_with_tick},
};
#[cfg(unix)]
use anyhow::Context;
use anyhow::Result;
use clap::Args;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    collections::{BTreeMap, VecDeque},
    fs::{File, OpenOptions},
    io::{self, Write},
    str::FromStr,
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Args, Clone, Debug)]
pub struct Options {
    /// Backend ID, or `auto` for the first currently available backend.
    #[arg(long, default_value = "auto")]
    pub backend: String,
    /// Select a backend voice by its stable backend-provided ID.
    #[arg(long)]
    pub voice: Option<String>,
    /// Print every backend compiled for this platform and exit.
    #[arg(long, conflicts_with = "list_voices")]
    pub list_backends: bool,
    /// Print voices exposed by the selected backend and exit.
    #[arg(long, conflicts_with = "list_backends")]
    pub list_voices: bool,
    /// Exit if this local parent process is no longer the host's parent.
    #[arg(long, hide = true)]
    pub parent_pid: Option<u32>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            backend: "auto".to_owned(),
            voice: None,
            list_backends: false,
            list_voices: false,
            parent_pid: None,
        }
    }
}

type SpeechEventLog = Arc<Mutex<File>>;

#[derive(Clone, Copy, Debug)]
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
    backend: Backends,
    backend_info: BackendInfo,
    rate: Option<SettingState>,
    pitch: Option<SettingState>,
    volume: Option<SettingState>,
    selected_voice: Option<String>,
    initialized: bool,
    capabilities: SpeechCapabilities,
    rpc_log: Option<File>,
    active: Option<ActiveUtterance>,
    native_events: mpsc::Receiver<NativeEvent>,
    notifications: VecDeque<ServerNotification>,
}

#[derive(Clone, Copy)]
struct SettingState {
    current: f32,
    min: f32,
    max: f32,
}

#[derive(serde::Deserialize)]
struct RateParams {
    rate: f32,
}

#[derive(serde::Deserialize)]
struct PitchParams {
    pitch: f32,
}

#[derive(serde::Deserialize)]
struct VolumeParams {
    volume: f32,
}

/// A transient external endpoint is a backend state, not a host transport
/// failure. Keeping that distinction here prevents every caller from making a
/// different restart or replay decision.
#[derive(Debug)]
enum BackendAttempt<T> {
    Completed(T),
    Unavailable(String),
}

/// Run the host with default backend selection and an optional parent guard.
///
/// # Errors
///
/// Returns an error when the backend cannot be constructed or protocol I/O
/// fails.
pub fn run(expected_parent_pid: Option<u32>) -> Result<()> {
    run_with_options(&Options {
        parent_pid: expected_parent_pid,
        ..Options::default()
    })
}

/// Run a listing command or serve the speech protocol with `options`.
///
/// # Errors
///
/// Returns an error when selection, backend initialization, listing, parent
/// monitoring, or protocol I/O fails.
pub fn run_with_options(options: &Options) -> Result<()> {
    if options.list_backends {
        let stdout = io::stdout();
        let mut stdout = stdout.lock();
        for backend in Tts::compiled_backends() {
            writeln!(
                stdout,
                "{}\t{}\t{}",
                backend.id(),
                backend.name(),
                if backend.is_available() {
                    "available"
                } else {
                    "unavailable"
                }
            )?;
        }
        return Ok(());
    }
    if let Some(expected_parent_pid) = options.parent_pid {
        start_parent_watchdog(expected_parent_pid)?;
    }
    let backend = select_backend(&options.backend)?;
    if options.list_voices {
        let tts = Tts::new(backend).map_err(|error| anyhow::anyhow!(error))?;
        for voice in tts.voices().map_err(|error| anyhow::anyhow!(error))? {
            let voice = voice_info(&voice);
            writeln!(
                io::stdout().lock(),
                "{}\t{}\t{}",
                voice.id,
                voice.name,
                voice.language
            )?;
        }
        return Ok(());
    }
    let state = RefCell::new(State::new(backend, options.voice.as_deref())?);
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

fn select_backend(value: &str) -> Result<Backends> {
    if value == "auto" {
        return Tts::backends()
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("no speech backend is currently available"));
    }
    Backends::from_str(value).map_err(Into::into)
}

impl State {
    fn new(backend: Backends, voice_id: Option<&str>) -> Result<Self> {
        let tts = Tts::new(backend).map_err(|error| anyhow::anyhow!(error))?;
        let features = tts.supported_features();
        let test_event_log = configure_test_observation(&tts)?;
        let rate = if features.rate {
            let min = tts.min_rate().map_err(|error| anyhow::anyhow!(error))?;
            let max = tts.max_rate().map_err(|error| anyhow::anyhow!(error))?;
            let current = tts.get_rate().map_err(|error| anyhow::anyhow!(error))?;
            Some(SettingState { current, min, max })
        } else {
            None
        };
        let pitch = if features.pitch {
            let min = tts.min_pitch().map_err(|error| anyhow::anyhow!(error))?;
            let max = tts.max_pitch().map_err(|error| anyhow::anyhow!(error))?;
            let current = tts.get_pitch().map_err(|error| anyhow::anyhow!(error))?;
            Some(SettingState { current, min, max })
        } else {
            None
        };
        let volume = if features.volume {
            let min = tts.min_volume().map_err(|error| anyhow::anyhow!(error))?;
            let max = tts.max_volume().map_err(|error| anyhow::anyhow!(error))?;
            let current = tts.get_volume().map_err(|error| anyhow::anyhow!(error))?;
            Some(SettingState { current, min, max })
        } else {
            None
        };
        let selected_voice = voice_id
            .map(|voice_id| select_voice(&tts, voice_id).map(|voice| voice.id))
            .transpose()?;
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
            backend,
            backend_info: BackendInfo {
                id: backend.id().to_owned(),
                name: backend.name().to_owned(),
                extensions: BTreeMap::new(),
            },
            rate,
            pitch,
            volume,
            selected_voice,
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
        let native_id = match classify_backend_attempt(self.tts.speak(&text, false))? {
            BackendAttempt::Completed(native_id) => native_id,
            BackendAttempt::Unavailable(message) => {
                // The selected backend remains authoritative while its external
                // service is absent. Drop this utterance instead of changing
                // engines or killing the protocol host, and provide correlated
                // terminal evidence so the client does not retain phantom work.
                self.emit(id, 0, "ended", None, Some("failed"), Some(message));
                return Ok(());
            }
        };
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
    /// `AVFoundation` has historically returned from `stop` without always
    /// issuing its cancellation callback. In that state it may silently drop
    /// the next utterance, so lack of terminal evidence causes a conservative
    /// synthesizer replacement.
    fn stop_native(&mut self, stopping: Option<NativeUtteranceId>) -> Result<(), RpcError> {
        match classify_backend_attempt(self.tts.stop())? {
            BackendAttempt::Completed(()) => {}
            BackendAttempt::Unavailable(_) => return Ok(()),
        }
        let Some(stopping) = stopping else {
            return Ok(());
        };
        let deadline = Instant::now() + Duration::from_millis(30);
        let mut confirmed = false;
        while Instant::now() < deadline {
            settle_platform_runloop();
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
        let tts =
            Tts::new(self.backend).map_err(|error| RpcError::internal_error(error.to_string()))?;
        let test_event_log = configure_test_observation(&tts)
            .map_err(|error| RpcError::internal_error(error.to_string()))?;
        if let Some(rate) = self.rate {
            tts.set_rate(rate.current)
                .map_err(|error| RpcError::internal_error(error.to_string()))?;
        }
        if let Some(pitch) = self.pitch {
            tts.set_pitch(pitch.current)
                .map_err(|error| RpcError::internal_error(error.to_string()))?;
        }
        if let Some(volume) = self.volume {
            tts.set_volume(volume.current)
                .map_err(|error| RpcError::internal_error(error.to_string()))?;
        }
        if let Some(voice_id) = &self.selected_voice {
            select_voice(&tts, voice_id)
                .map_err(|error| RpcError::internal_error(error.to_string()))?;
        }
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

    fn voices(&self) -> Result<VoiceListResult, RpcError> {
        if !self.capabilities.voices.list {
            return Err(RpcError::method_not_found("speech.listVoices"));
        }
        let voices = self
            .tts
            .voices()
            .map_err(|error| RpcError::internal_error(error.to_string()))?
            .iter()
            .map(voice_info)
            .collect();
        Ok(VoiceListResult { voices })
    }

    fn current_voice(&self) -> Result<CurrentVoiceResult, RpcError> {
        if !self.capabilities.voices.current {
            return Err(RpcError::method_not_found("speech.getVoice"));
        }
        let voice = self
            .tts
            .voice()
            .map_err(|error| RpcError::internal_error(error.to_string()))?
            .as_ref()
            .map(voice_info);
        Ok(CurrentVoiceResult { voice })
    }

    fn current_rate(&mut self) -> Result<RateResult, RpcError> {
        if self.capabilities.settings.rate != SettingSupport::ReadWrite {
            return Err(RpcError::method_not_found("speech.getRate"));
        }
        let Some(mut rate) = self.rate else {
            return Err(RpcError::method_not_found("speech.getRate"));
        };
        rate.current = self
            .tts
            .get_rate()
            .map_err(|error| RpcError::internal_error(error.to_string()))?;
        if !rate.current.is_finite() {
            return Err(RpcError::internal_error(
                "speech backend returned a non-finite rate",
            ));
        }
        self.rate = Some(rate);
        Ok(RateResult { rate: rate.current })
    }

    fn set_rate(&mut self, rate: f32) -> Result<RateResult, RpcError> {
        if !self.capabilities.settings.rate.can_write() {
            return Err(RpcError::method_not_found("speech.setRate"));
        }
        if !rate.is_finite() {
            return Err(RpcError::invalid_params("rate must be finite"));
        }
        let Some(mut effective) = self.rate else {
            return Err(RpcError::method_not_found("speech.setRate"));
        };
        effective.current = rate.clamp(effective.min, effective.max);
        self.tts
            .set_rate(effective.current)
            .map_err(|error| RpcError::internal_error(error.to_string()))?;
        self.rate = Some(effective);
        Ok(RateResult {
            rate: effective.current,
        })
    }

    fn current_pitch(&mut self) -> Result<PitchResult, RpcError> {
        if self.capabilities.settings.pitch != SettingSupport::ReadWrite {
            return Err(RpcError::method_not_found("speech.getPitch"));
        }
        let Some(mut pitch) = self.pitch else {
            return Err(RpcError::method_not_found("speech.getPitch"));
        };
        pitch.current = self
            .tts
            .get_pitch()
            .map_err(|error| RpcError::internal_error(error.to_string()))?;
        if !pitch.current.is_finite() {
            return Err(RpcError::internal_error(
                "speech backend returned a non-finite pitch",
            ));
        }
        self.pitch = Some(pitch);
        Ok(PitchResult {
            pitch: pitch.current,
        })
    }

    fn set_pitch(&mut self, pitch: f32) -> Result<PitchResult, RpcError> {
        if !self.capabilities.settings.pitch.can_write() {
            return Err(RpcError::method_not_found("speech.setPitch"));
        }
        if !pitch.is_finite() {
            return Err(RpcError::invalid_params("pitch must be finite"));
        }
        let Some(mut effective) = self.pitch else {
            return Err(RpcError::method_not_found("speech.setPitch"));
        };
        effective.current = pitch.clamp(effective.min, effective.max);
        self.tts
            .set_pitch(effective.current)
            .map_err(|error| RpcError::internal_error(error.to_string()))?;
        self.pitch = Some(effective);
        Ok(PitchResult {
            pitch: effective.current,
        })
    }

    fn current_volume(&mut self) -> Result<VolumeResult, RpcError> {
        if self.capabilities.settings.volume != SettingSupport::ReadWrite {
            return Err(RpcError::method_not_found("speech.getVolume"));
        }
        let Some(mut volume) = self.volume else {
            return Err(RpcError::method_not_found("speech.getVolume"));
        };
        volume.current = self
            .tts
            .get_volume()
            .map_err(|error| RpcError::internal_error(error.to_string()))?;
        if !volume.current.is_finite() {
            return Err(RpcError::internal_error(
                "speech backend returned a non-finite volume",
            ));
        }
        self.volume = Some(volume);
        Ok(VolumeResult {
            volume: volume.current,
        })
    }

    fn set_volume(&mut self, volume: f32) -> Result<VolumeResult, RpcError> {
        if !self.capabilities.settings.volume.can_write() {
            return Err(RpcError::method_not_found("speech.setVolume"));
        }
        if !volume.is_finite() {
            return Err(RpcError::invalid_params("volume must be finite"));
        }
        let Some(mut effective) = self.volume else {
            return Err(RpcError::method_not_found("speech.setVolume"));
        };
        effective.current = volume.clamp(effective.min, effective.max);
        self.tts
            .set_volume(effective.current)
            .map_err(|error| RpcError::internal_error(error.to_string()))?;
        self.volume = Some(effective);
        Ok(VolumeResult {
            volume: effective.current,
        })
    }

    fn set_voice(&mut self, voice_id: &str) -> Result<CurrentVoiceResult, RpcError> {
        if !self.capabilities.voices.select {
            return Err(RpcError::method_not_found("speech.setVoice"));
        }
        let voice = select_voice(&self.tts, voice_id)
            .map_err(|error| RpcError::invalid_params(error.to_string()))?;
        self.selected_voice = Some(voice.id.clone());
        Ok(CurrentVoiceResult { voice: Some(voice) })
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
            Err(error) => eprintln!("lector-tts: serialize speech event: {error}"),
        }
    }
}

fn resumable_suffix(text: &str, offset: usize) -> Option<&str> {
    (offset <= text.len() && text.is_char_boundary(offset)).then(|| &text[offset..])
}

fn native_capabilities(features: Features) -> SpeechCapabilities {
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
            pitch: if features.pitch {
                SettingSupport::ReadWrite
            } else {
                SettingSupport::Unsupported
            },
            volume: if features.volume {
                SettingSupport::ReadWrite
            } else {
                SettingSupport::Unsupported
            },
            ..Default::default()
        },
        voices: VoiceCapabilities {
            list: features.voice,
            current: features.get_voice,
            select: features.voice,
            ..Default::default()
        },
        ..Default::default()
    }
}

fn classify_backend_attempt<T>(
    result: std::result::Result<T, TtsError>,
) -> Result<BackendAttempt<T>, RpcError> {
    match result {
        Ok(value) => Ok(BackendAttempt::Completed(value)),
        Err(error @ TtsError::BackendUnavailable(_)) => {
            Ok(BackendAttempt::Unavailable(error.to_string()))
        }
        Err(error) => Err(RpcError::internal_error(error.to_string())),
    }
}

fn voice_info(voice: &Voice) -> VoiceInfo {
    VoiceInfo {
        id: voice.id().to_owned(),
        name: voice.name().to_owned(),
        language: voice.language().to_string(),
        gender: voice.gender().map(|gender| match gender {
            Gender::Male => "male".to_owned(),
            Gender::Female => "female".to_owned(),
        }),
        extensions: BTreeMap::new(),
    }
}

fn select_voice(tts: &Tts, voice_id: &str) -> Result<VoiceInfo> {
    let voice = tts
        .voices()
        .map_err(|error| anyhow::anyhow!(error))?
        .into_iter()
        .find(|voice| voice.id() == voice_id)
        .ok_or_else(|| anyhow::anyhow!("voice not found: {voice_id}"))?;
    tts.set_voice(&voice)
        .map_err(|error| anyhow::anyhow!(error))?;
    Ok(voice_info(&voice))
}

fn install_native_callbacks(
    tts: &Tts,
    events: &mpsc::Sender<NativeEvent>,
    supported: bool,
    event_log: Option<SpeechEventLog>,
) -> std::result::Result<(), TtsError> {
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
    if let Some(result) = crate::server::handle_protocol_request(
        &request,
        "lector-tts",
        env!("CARGO_PKG_VERSION"),
        Some(&state.backend_info),
        &state.capabilities,
    ) {
        if request.method == "initialize"
            && let Ok(value) = &result
        {
            let initialized: InitializeResult = serde_json::from_value(value.clone())
                .map_err(|error| RpcError::internal_error(error.to_string()))?;
            state.capabilities = initialized.capabilities;
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
            let params: SpeakParams = params(request.params)?;
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
        "speech.getRate" => value(state.current_rate()?),
        "speech.setRate" => {
            let rate = params::<RateParams>(request.params)?.rate;
            value(state.set_rate(rate)?)
        }
        "speech.getPitch" => value(state.current_pitch()?),
        "speech.setPitch" => {
            let pitch = params::<PitchParams>(request.params)?.pitch;
            value(state.set_pitch(pitch)?)
        }
        "speech.getVolume" => value(state.current_volume()?),
        "speech.setVolume" => {
            let volume = params::<VolumeParams>(request.params)?.volume;
            value(state.set_volume(volume)?)
        }
        "speech.listVoices" => value(state.voices()?),
        "speech.getVoice" => value(state.current_voice()?),
        "speech.setVoice" => {
            let params: SetVoiceParams = params(request.params)?;
            if params.voice_id.is_empty() {
                return Err(RpcError::invalid_params("voiceId must not be empty"));
            }
            value(state.set_voice(&params.voice_id)?)
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
    let time_unix_us = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros(),
    )
    .unwrap_or(u64::MAX);
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
#[cfg(unix)]
fn start_parent_watchdog(expected_parent_pid: u32) -> Result<()> {
    std::thread::Builder::new()
        .name("lector-speech-parent-watchdog".to_owned())
        .spawn(move || {
            loop {
                let actual_parent_pid = nix::unistd::getppid().as_raw().cast_unsigned();
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

#[cfg(not(unix))]
fn start_parent_watchdog(_expected_parent_pid: u32) -> Result<()> {
    Ok(())
}

#[cfg(target_vendor = "apple")]
fn settle_platform_runloop() {
    use core_foundation::runloop;
    unsafe {
        let _ = runloop::CFRunLoopRunInMode(runloop::kCFRunLoopDefaultMode, 0.01, 0);
    }
}

#[cfg(not(target_vendor = "apple"))]
fn settle_platform_runloop() {}

fn configure_test_observation(tts: &Tts) -> Result<Option<SpeechEventLog>> {
    if std::env::var_os("LECTOR_SPEECH_TEST_MUTE").is_some() && tts.supported_features().volume {
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
    let time_unix_us = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros(),
    )
    .unwrap_or(u64::MAX);
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
    use super::{BackendAttempt, classify_backend_attempt, native_capabilities, resumable_suffix};
    use crate::{Error, Features, protocol::SettingSupport};

    #[test]
    fn resume_suffix_starts_at_the_reported_utf8_word_boundary() {
        assert_eq!(resumable_suffix("héllo world", 7), Some("world"));
        assert_eq!(resumable_suffix("héllo world", 2), None);
        assert_eq!(resumable_suffix("héllo world", 99), None);
    }

    #[test]
    fn native_capabilities_do_not_invent_settings_or_voice_controls() {
        let unsupported = native_capabilities(Features::default());
        assert_eq!(unsupported.settings.rate, SettingSupport::Unsupported);
        assert_eq!(unsupported.settings.pitch, SettingSupport::Unsupported);
        assert_eq!(unsupported.settings.volume, SettingSupport::Unsupported);
        assert!(!unsupported.voices.list);
        assert!(!unsupported.voices.current);
        assert!(!unsupported.voices.select);

        let independent_voice_operations = native_capabilities(Features {
            voice: true,
            get_voice: false,
            ..Features::default()
        });
        assert!(independent_voice_operations.voices.list);
        assert!(!independent_voice_operations.voices.current);
        assert!(independent_voice_operations.voices.select);

        let configurable = native_capabilities(Features {
            rate: true,
            pitch: true,
            volume: true,
            get_voice: true,
            ..Features::default()
        });
        assert_eq!(configurable.settings.rate, SettingSupport::ReadWrite);
        assert_eq!(configurable.settings.pitch, SettingSupport::ReadWrite);
        assert_eq!(configurable.settings.volume, SettingSupport::ReadWrite);
        assert!(!configurable.voices.list);
        assert!(configurable.voices.current);
        assert!(!configurable.voices.select);
    }

    #[test]
    fn transient_backend_absence_is_not_a_host_transport_failure() {
        let unavailable = classify_backend_attempt::<()>(Err(Error::BackendUnavailable(
            "external service stopped",
        )))
        .expect("transient absence remains an ordinary host result");
        assert!(matches!(unavailable, BackendAttempt::Unavailable(_)));

        let failure = classify_backend_attempt::<()>(Err(Error::OperationFailed("speak")))
            .expect_err("a real backend failure remains an RPC error");
        assert_eq!(failure.code, -32603);
    }
}
