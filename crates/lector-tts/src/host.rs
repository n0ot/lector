//! Built-in implementation of the versioned speech-host protocol.
//!
//! Native identifiers, callback threads, and platform indexing never cross
//! this boundary. The host accepts one Lector utterance at a time and emits
//! correlated lifecycle events; Lector owns all queueing.

#[cfg(target_vendor = "apple")]
mod cue_playback;
#[cfg(unix)]
mod parent_watchdog;

#[cfg(target_vendor = "apple")]
use crate::protocol::SoundCue;
#[cfg(target_vendor = "apple")]
use crate::speech_fallback::{
    InsertedLink, MAX_LINK_ANNOUNCEMENT_BYTES, append_cue_announcement, canonical_offset,
};
#[cfg(target_vendor = "apple")]
use cue_playback::{CuePlayback, PreparedCues, ReadbackCache, ReadbackKey};

use crate::{
    Backends, Error as TtsError, Features, Gender, Tts, UtteranceId as NativeUtteranceId, Voice,
    protocol::{
        AcceptedResult, BackendInfo, ControlCapabilities, CurrentVoiceResult, DeliveryGuarantee,
        EventCapability, LifecycleCapabilities, MAX_UTTERANCE_TEXT_BYTES, NORMAL_RATE,
        NORMALIZED_RATE_PROTOCOL_MINOR, PauseResult, PauseResumeSupport, PitchResult,
        ProgressCapabilities, ProgressMode, ProtocolVersion, RateResult, SetVoiceParams,
        SettingCapabilities, SettingSupport, SoundCueAt, SpeakParams, SpeechCapabilities,
        SpeechEventNotification, SpeechEventPayload, StopSupport, TerminalCapability, TextPosition,
        UtteranceId, UtteranceParams, VoiceCapabilities, VoiceInfo, VoiceListResult, VolumeResult,
        rate_is_normalized,
    },
    server::{
        InitializeResult, Request, RpcError, ServerNotification, Waiter, Wake, run_server_with_wake,
    },
};
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
        length: usize,
    },
}

struct ActiveUtterance {
    logical_id: UtteranceId,
    native_id: Option<NativeUtteranceId>,
    text: String,
    #[cfg(target_vendor = "apple")]
    cues: Vec<SoundCueAt>,
    #[cfg(target_vendor = "apple")]
    fallback: Option<SpokenFallback>,
    #[cfg(target_vendor = "apple")]
    direction_label: Option<SoundCue>,
    /// UTF-8 offset represented by byte zero of the current native utterance.
    native_base: usize,
    current_word: usize,
    next_sequence: u64,
    started: bool,
    paused: bool,
}

#[cfg(target_vendor = "apple")]
impl ActiveUtterance {
    fn cues_from(&self, base: usize) -> Vec<SoundCueAt> {
        self.cues
            .iter()
            .filter_map(|cue| {
                Some(SoundCueAt {
                    cue: cue.cue,
                    offset: if cue.cue == SoundCue::Link {
                        cue.offset.checked_sub(base)?
                    } else {
                        // A retained direction has not announced yet. Its
                        // onset moves to the beginning of the resumed suffix.
                        0
                    },
                })
            })
            .collect()
    }
}

#[cfg(target_vendor = "apple")]
struct SpokenFallback {
    text: String,
    announcements: Vec<InsertedLink>,
    playback_base: usize,
    word: usize,
}

#[cfg(target_vendor = "apple")]
impl SpokenFallback {
    fn new(text: &str, cues: &[SoundCueAt]) -> Self {
        let mut announcements = Vec::with_capacity(cues.len());
        let spoken_text = if let [cue] = cues
            && text.is_empty()
        {
            cue.cue.spoken_label().to_owned()
        } else {
            let mut spoken_text =
                String::with_capacity(text.len() + cues.len() * MAX_LINK_ANNOUNCEMENT_BYTES);
            let mut previous = 0;
            for cue in cues {
                spoken_text.push_str(&text[previous..cue.offset]);
                announcements.push(append_cue_announcement(
                    &mut spoken_text,
                    text,
                    cue.offset,
                    cue.cue,
                ));
                previous = cue.offset;
            }
            spoken_text.push_str(&text[previous..]);
            spoken_text
        };
        Self {
            text: spoken_text,
            announcements,
            playback_base: 0,
            word: 0,
        }
    }

    fn report_word(&mut self, start: usize) -> Option<usize> {
        let absolute = self.playback_base.checked_add(start)?;
        if !self.text.is_char_boundary(absolute) {
            return None;
        }
        self.word = absolute;
        Some(canonical_offset(absolute, &self.announcements))
    }
}

#[cfg(target_vendor = "apple")]
#[derive(Clone, Copy, PartialEq, Eq)]
enum DirectionPlayback {
    Pending,
    Played,
    Failed,
}

#[cfg(target_vendor = "apple")]
impl DirectionPlayback {
    fn needs_label(self, naturally_completed: bool) -> bool {
        naturally_completed && self != Self::Played
    }

    fn shows_missing_word_ranges(self, saw_range: bool) -> bool {
        self == Self::Pending && !saw_range
    }
}

#[cfg(target_vendor = "apple")]
struct LiveDirection {
    native_id: NativeUtteranceId,
    cue: SoundCue,
    playback: DirectionPlayback,
    clock_logged: bool,
    saw_range: bool,
    cache_key: Option<ReadbackKey>,
}

#[cfg(target_vendor = "apple")]
#[derive(Default)]
struct DirectionalBatch {
    saw_range: bool,
    ended: bool,
}

#[cfg(target_vendor = "apple")]
impl DirectionalBatch {
    fn inspect(events: &[NativeEvent], id: NativeUtteranceId) -> Self {
        let mut batch = Self::default();
        for event in events {
            match *event {
                NativeEvent::Range {
                    id: native_id,
                    length,
                    ..
                } if native_id == id && length != 0 => {
                    batch.saw_range = true;
                }
                NativeEvent::Completed(native_id) | NativeEvent::Cancelled(native_id)
                    if native_id == id =>
                {
                    batch.ended = true;
                }
                _ => {}
            }
        }
        batch
    }
}

struct State {
    tts: Tts,
    #[cfg(target_vendor = "apple")]
    cue_playback: Option<CuePlayback>,
    #[cfg(target_vendor = "apple")]
    prepared_cues: PreparedCues,
    #[cfg(target_vendor = "apple")]
    readback_cache: ReadbackCache,
    #[cfg(target_vendor = "apple")]
    live_direction: Option<LiveDirection>,
    #[cfg(target_vendor = "apple")]
    directional_word_ranges: bool,
    #[cfg(target_vendor = "apple")]
    cue_event_log: Option<SpeechEventLog>,
    backend: Backends,
    backend_info: BackendInfo,
    rate: Option<RateState>,
    pitch: Option<SettingState>,
    volume: Option<SettingState>,
    selected_voice: Option<String>,
    initialized: bool,
    protocol: Option<ProtocolVersion>,
    capabilities: SpeechCapabilities,
    rpc_log: Option<File>,
    active: Option<ActiveUtterance>,
    native_events: mpsc::Receiver<NativeEvent>,
    wake: Wake,
    notifications: VecDeque<ServerNotification>,
}

#[derive(Clone, Copy)]
struct SettingState {
    current: f32,
    min: f32,
    max: f32,
}

#[derive(Clone, Copy)]
struct RateState {
    current: f32,
    scale: RateScale,
}

/// Piecewise-linear translation between Lector's normalized rate and one
/// backend's native domain. A normal rate strictly inside the native bounds is
/// anchored at 50 even when it is not their arithmetic midpoint.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RateScale {
    min: f32,
    normal: f32,
    max: f32,
    split_at_normal: bool,
}

impl RateScale {
    #[must_use]
    pub fn new(min: f32, normal: f32, max: f32) -> Option<Self> {
        (min.is_finite()
            && normal.is_finite()
            && max.is_finite()
            && min < max
            && (min..=max).contains(&normal))
        .then_some(Self {
            min,
            normal,
            max,
            split_at_normal: normal > min && normal < max,
        })
    }

    #[must_use]
    pub fn normalize(self, native: f32) -> f32 {
        let native = native.clamp(self.min, self.max);
        if !self.split_at_normal {
            return crate::protocol::MAX_RATE * (native - self.min) / (self.max - self.min);
        }
        if native <= self.normal {
            NORMAL_RATE * (native - self.min) / (self.normal - self.min)
        } else {
            NORMAL_RATE + NORMAL_RATE * (native - self.normal) / (self.max - self.normal)
        }
    }

    #[must_use]
    pub fn to_native(self, normalized: f32) -> f32 {
        let normalized = normalized.clamp(crate::protocol::MIN_RATE, crate::protocol::MAX_RATE);
        if !self.split_at_normal {
            return self.min + (self.max - self.min) * normalized / crate::protocol::MAX_RATE;
        }
        if normalized <= NORMAL_RATE {
            self.min + (self.normal - self.min) * normalized / NORMAL_RATE
        } else {
            self.normal + (self.max - self.normal) * (normalized - NORMAL_RATE) / NORMAL_RATE
        }
    }
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
    let waiter = Waiter::new()?;
    let state = RefCell::new(State::new(
        backend,
        options.voice.as_deref(),
        waiter.wake(),
    )?);
    run_server_with_wake(
        &waiter,
        |request| handle_request(request, &mut state.borrow_mut()),
        || {
            let mut borrowed = state.borrow_mut();
            let state = &mut *borrowed;
            #[cfg(target_vendor = "apple")]
            if state.readback_cache.advance()
                && let Some(log) = &state.cue_event_log
            {
                write_speech_event(log, "directional-cue-cache-ready");
            }
            #[cfg(target_vendor = "apple")]
            state.prepared_cues.idle();
            state.advance();
            let timeout = state.next_tick();
            (state.notifications.drain(..).collect(), timeout)
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
    fn new(backend: Backends, voice_id: Option<&str>, wake: Wake) -> Result<Self> {
        let tts = Tts::new(backend).map_err(|error| anyhow::anyhow!(error))?;
        let features = tts.supported_features();
        let test_event_log = configure_test_observation(&tts)?;
        let rate = if features.rate {
            let min = tts.min_rate().map_err(|error| anyhow::anyhow!(error))?;
            let max = tts.max_rate().map_err(|error| anyhow::anyhow!(error))?;
            let normal = tts.normal_rate().map_err(|error| anyhow::anyhow!(error))?;
            let current = tts.get_rate().map_err(|error| anyhow::anyhow!(error))?;
            if !current.is_finite() {
                return Err(anyhow::anyhow!(
                    "speech backend returned a non-finite current rate"
                ));
            }
            let scale = RateScale::new(min, normal, max).ok_or_else(|| {
                anyhow::anyhow!(
                    "speech backend returned invalid rate bounds: min={min}, normal={normal}, max={max}"
                )
            })?;
            Some(RateState { current, scale })
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
        let mut capabilities = native_capabilities(features);
        if supports_sound_cues(backend) {
            capabilities.sound_cues = true;
        }
        #[cfg(target_vendor = "apple")]
        let prepared_cues = if capabilities.sound_cues {
            PreparedCues::new(tts.get_volume()?)
        } else {
            PreparedCues::default()
        };
        #[cfg(target_vendor = "apple")]
        let cue_event_log = test_event_log.clone();
        let (native_event_tx, native_events) = mpsc::channel();
        install_native_callbacks(
            &tts,
            &native_event_tx,
            &wake,
            features.utterance_callbacks,
            test_event_log,
        )
        .map_err(|error| anyhow::anyhow!(error))?;
        let rpc_log = std::env::var_os("LECTOR_SPEECH_RPC_LOG")
            .map(|path| OpenOptions::new().create(true).append(true).open(path))
            .transpose()?;
        Ok(Self {
            tts,
            #[cfg(target_vendor = "apple")]
            cue_playback: None,
            #[cfg(target_vendor = "apple")]
            prepared_cues,
            #[cfg(target_vendor = "apple")]
            readback_cache: ReadbackCache::default(),
            #[cfg(target_vendor = "apple")]
            live_direction: None,
            #[cfg(target_vendor = "apple")]
            directional_word_ranges: true,
            #[cfg(target_vendor = "apple")]
            cue_event_log,
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
            protocol: None,
            capabilities,
            rpc_log,
            active: None,
            native_events,
            wake,
            notifications: VecDeque::new(),
        })
    }

    fn speak(&mut self, params: SpeakParams) -> Result<(), RpcError> {
        let SpeakParams {
            utterance_id: id,
            text,
            cues,
        } = params;
        self.advance();
        if self.active.is_some() {
            return Err(RpcError::invalid_request(
                "speech.speak received while another utterance is active",
            ));
        }
        if !id.is_valid()
            || text.len() > MAX_UTTERANCE_TEXT_BYTES
            || !self
                .protocol
                .is_some_and(|version| SoundCueAt::valid_for_protocol(&text, &cues, version))
        {
            return Err(RpcError::invalid_params(
                "utteranceId, text, or sound cues are invalid",
            ));
        }
        if !cues.is_empty() && !self.capabilities.sound_cues {
            return Err(RpcError::invalid_params(
                "sound cues are unavailable for this session",
            ));
        }
        #[cfg(target_vendor = "apple")]
        self.stop_live_direction();
        let mut active = ActiveUtterance {
            logical_id: id,
            native_id: None,
            text,
            #[cfg(target_vendor = "apple")]
            cues,
            #[cfg(target_vendor = "apple")]
            fallback: None,
            #[cfg(target_vendor = "apple")]
            direction_label: None,
            native_base: 0,
            current_word: 0,
            next_sequence: 0,
            started: false,
            paused: false,
        };
        active.native_id = match self.start_native(&mut active, 0)? {
            BackendAttempt::Completed(native_id) => native_id,
            BackendAttempt::Unavailable(message) => {
                // The selected backend remains authoritative while its external
                // service is absent. Provide correlated terminal evidence.
                self.emit_for(&active, "ended", None, Some("failed"), Some(message));
                return Ok(());
            }
        };
        self.active = Some(active);
        Ok(())
    }

    fn start_native(
        &mut self,
        active: &mut ActiveUtterance,
        base: usize,
    ) -> Result<BackendAttempt<Option<NativeUtteranceId>>, RpcError> {
        let text = resumable_suffix(&active.text, base).ok_or_else(|| {
            RpcError::internal_error("native word position is not a UTF-8 boundary")
        })?;
        #[cfg(target_vendor = "apple")]
        {
            if let Some(cue) = active.direction_label {
                return classify_backend_attempt(self.tts.speak(cue.spoken_label(), false));
            }
            if let Some(fallback) = &mut active.fallback {
                self.readback_cache.retain_request(None);
                let result = classify_backend_attempt(
                    self.tts.speak(&fallback.text[fallback.word..], false),
                )?;
                if matches!(result, BackendAttempt::Completed(_)) {
                    fallback.playback_base = fallback.word;
                }
                return Ok(result);
            }
            let cues = active.cues_from(base);
            if text.is_empty() && !cues.is_empty() {
                return self.start_standalone_cue(active, base, &cues);
            }
            if let [cue] = cues.as_slice()
                && !text.is_empty()
                && cue.cue != SoundCue::Link
            {
                return self.start_live_direction(active, base, cue);
            }
            self.readback_cache.retain_request(None);
            if !cues.is_empty() {
                match CuePlayback::new(
                    &self.tts,
                    text,
                    cues.clone(),
                    self.selected_voice.as_deref(),
                ) {
                    Ok(playback) => {
                        let id = playback.id();
                        self.cue_playback = Some(playback);
                        active.native_base = base;
                        return Ok(BackendAttempt::Completed(Some(id)));
                    }
                    Err(error) => {
                        eprintln!("lector-tts: {error}; speaking cue labels");
                        return self.start_spoken_fallback(active, base, &cues);
                    }
                }
            }
        }
        let result = classify_backend_attempt(self.tts.speak(text, false))?;
        if matches!(result, BackendAttempt::Completed(_)) {
            active.native_base = base;
        }
        Ok(result)
    }

    #[cfg(target_vendor = "apple")]
    fn start_live_direction(
        &mut self,
        active: &mut ActiveUtterance,
        base: usize,
        cue: &SoundCueAt,
    ) -> Result<BackendAttempt<Option<NativeUtteranceId>>, RpcError> {
        let text = &active.text[base..];
        // Use labels when there is no evidence that this text and
        // voice will provide a word onset for live playback.
        if !text.chars().any(char::is_alphanumeric) {
            self.readback_cache.retain_request(None);
            return self.start_spoken_fallback(active, base, std::slice::from_ref(cue));
        }
        let cache_key = ReadbackKey::new(&self.tts, text, cue.cue, self.selected_voice.as_deref());
        self.readback_cache.retain_request(cache_key.as_ref());
        let volume = self
            .tts
            .get_volume()
            .map_err(|error| RpcError::internal_error(error.to_string()))?;
        if let Some(key) = &cache_key
            && let Some(playback) = self.readback_cache.play(key, volume, &self.prepared_cues)
        {
            if let Some(log) = &self.cue_event_log {
                write_speech_event(log, "directional-cue-cache-hit");
                write_speech_event(log, "directional-cue-begin");
            }
            let id = playback.id();
            self.cue_playback = Some(playback);
            active.native_base = base;
            return Ok(BackendAttempt::Completed(Some(id)));
        }
        if !self.directional_word_ranges {
            return self.start_spoken_fallback(active, base, std::slice::from_ref(cue));
        }
        if let Err(error) = self.prepared_cues.prepare(cue.cue, volume) {
            eprintln!("lector-tts: {error}; speaking cue label");
            return self.start_spoken_fallback(active, base, std::slice::from_ref(cue));
        }
        // The system mixes these live sources at their normal levels. The
        // host can inspect and limit peaks only on the rendered PCM route.
        let result = classify_backend_attempt(self.tts.speak(text, false))?;
        if let BackendAttempt::Completed(Some(native_id)) = result {
            self.live_direction = Some(LiveDirection {
                native_id,
                cue: cue.cue,
                playback: DirectionPlayback::Pending,
                clock_logged: false,
                saw_range: false,
                cache_key,
            });
            active.native_base = base;
        }
        Ok(result)
    }

    #[cfg(target_vendor = "apple")]
    fn start_standalone_cue(
        &mut self,
        active: &mut ActiveUtterance,
        base: usize,
        cues: &[SoundCueAt],
    ) -> Result<BackendAttempt<Option<NativeUtteranceId>>, RpcError> {
        self.readback_cache.retain_request(None);
        let volume = self.volume.as_ref().map_or(1.0, |volume| volume.current);
        match CuePlayback::standalone(cues, volume, &self.prepared_cues) {
            Ok(playback) => {
                let id = playback.id();
                self.cue_playback = Some(playback);
                active.native_base = base;
                Ok(BackendAttempt::Completed(Some(id)))
            }
            Err(error) => {
                eprintln!("lector-tts: {error}; speaking cue labels");
                self.start_spoken_fallback(active, base, cues)
            }
        }
    }

    #[cfg(target_vendor = "apple")]
    fn start_spoken_fallback(
        &self,
        active: &mut ActiveUtterance,
        base: usize,
        cues: &[SoundCueAt],
    ) -> Result<BackendAttempt<Option<NativeUtteranceId>>, RpcError> {
        let fallback = SpokenFallback::new(&active.text[base..], cues);
        let result = classify_backend_attempt(self.tts.speak(&fallback.text, false))?;
        if matches!(result, BackendAttempt::Completed(_)) {
            active.native_base = base;
            active.fallback = Some(fallback);
            active.cues.clear();
        }
        Ok(result)
    }

    fn stop(&mut self, id: &UtteranceId) -> Result<(), RpcError> {
        self.advance_for_control(id);
        let Some(active) = self.active.take() else {
            #[cfg(target_vendor = "apple")]
            self.stop_live_direction();
            return Ok(());
        };
        if active.logical_id != *id {
            self.active = Some(active);
            return Err(RpcError::invalid_params(
                "utteranceId does not identify the active utterance",
            ));
        }
        #[cfg(target_vendor = "apple")]
        let suspended_cue = active.text.is_empty() && self.cue_playback.is_some();
        #[cfg(not(target_vendor = "apple"))]
        let suspended_cue = false;
        if (!active.paused || suspended_cue)
            && let Err(error) = self.stop_native(active.native_id)
        {
            self.active = Some(active);
            return Err(error);
        }
        self.emit_for(&active, "ended", None, Some("cancelled"), None);
        Ok(())
    }

    fn pause(&mut self, id: &UtteranceId) -> Result<PauseResult, RpcError> {
        self.advance_for_control(id);
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
        let pause = self.pause_native(&active);
        if let Err(error) = pause {
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

    fn pause_native(&mut self, active: &ActiveUtterance) -> Result<(), RpcError> {
        #[cfg(target_vendor = "apple")]
        if active.text.is_empty()
            && let Some(playback) = &mut self.cue_playback
        {
            playback.pause();
            return Ok(());
        }
        self.stop_native(active.native_id)
    }

    /// Stop the native utterance and make the backend ready for another one.
    /// `AVFoundation` has historically returned from `stop` without always
    /// issuing its cancellation callback. In that state it may silently drop
    /// the next utterance, so lack of terminal evidence causes a conservative
    /// synthesizer replacement.
    fn stop_native(&mut self, stopping: Option<NativeUtteranceId>) -> Result<(), RpcError> {
        #[cfg(target_vendor = "apple")]
        self.stop_live_direction();
        #[cfg(target_vendor = "apple")]
        if self
            .cue_playback
            .as_ref()
            .is_some_and(|playback| Some(playback.id()) == stopping)
        {
            self.cue_playback.take();
            return Ok(());
        }
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
            &self.wake,
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
        let Some(mut active) = self.active.take() else {
            return Err(RpcError::invalid_request("there is no paused utterance"));
        };
        if active.logical_id != *id || !active.paused {
            self.active = Some(active);
            return Err(RpcError::invalid_params(
                "utteranceId does not identify a paused utterance",
            ));
        }
        let result = self.resume_native(&mut active);
        let start = active.current_word;
        self.active = Some(active);
        result?;
        self.emit_active(
            "resumed",
            Some(TextPosition::Utf8ByteOffset { offset: start }),
            None,
            None,
        );
        Ok(())
    }

    fn resume_native(&mut self, active: &mut ActiveUtterance) -> Result<(), RpcError> {
        #[cfg(target_vendor = "apple")]
        if active.text.is_empty()
            && let Some(playback) = &mut self.cue_playback
        {
            playback
                .resume()
                .map_err(|error| RpcError::internal_error(error.to_string()))?;
            active.paused = false;
            return Ok(());
        }
        let start = active.current_word;
        active.native_id = match self.start_native(active, start)? {
            BackendAttempt::Completed(id) => id,
            BackendAttempt::Unavailable(message) => return Err(RpcError::internal_error(message)),
        };
        active.paused = false;
        Ok(())
    }

    #[cfg_attr(not(target_vendor = "apple"), allow(clippy::unused_self))]
    fn next_tick(&self) -> Option<Duration> {
        #[cfg(target_vendor = "apple")]
        {
            // Sample-clock playback and render deadlines still need a timed
            // check while active. Native speech itself wakes through callbacks.
            let playback_active = self.cue_playback.is_some()
                && self.active.as_ref().is_some_and(|active| !active.paused);
            let observing_live_cue = self.cue_event_log.is_some()
                && self.active.is_some()
                && self.live_direction.as_ref().is_some_and(|direction| {
                    direction.playback == DirectionPlayback::Played && !direction.clock_logged
                });
            if playback_active || observing_live_cue || self.readback_cache.needs_poll() {
                return Some(Duration::from_millis(10));
            }
            self.prepared_cues.idle_timeout()
        }
        #[cfg(not(target_vendor = "apple"))]
        {
            None
        }
    }

    fn advance(&mut self) {
        #[cfg(target_vendor = "apple")]
        self.advance_cue_playback(true);
        self.drain_native_events(true);
        #[cfg(target_vendor = "apple")]
        if let (Some(log), Some(direction)) = (&self.cue_event_log, &mut self.live_direction)
            && direction.playback == DirectionPlayback::Played
            && !direction.clock_logged
            && self.prepared_cues.clock_is_advancing(direction.cue)
        {
            direction.clock_logged = true;
            write_speech_event(log, "directional-cue-playing");
        }
    }

    fn advance_for_control(&mut self, id: &UtteranceId) {
        if self
            .active
            .as_ref()
            .is_none_or(|active| &active.logical_id != id)
        {
            self.advance();
            return;
        }
        // Keep progress current, but a stop or pause that arrived during
        // rendering must not start a player even if rendering just finished.
        #[cfg(target_vendor = "apple")]
        self.advance_cue_playback(false);
        self.drain_native_events(false);
    }

    fn drain_native_events(&mut self, allow_cue: bool) {
        #[cfg(not(target_vendor = "apple"))]
        let _ = allow_cue;
        #[cfg(target_vendor = "apple")]
        if allow_cue
            && let Some(direction) = &self.live_direction
            && direction.playback == DirectionPlayback::Pending
        {
            self.drain_directional_events(direction.native_id);
            return;
        }
        while let Ok(event) = self.native_events.try_recv() {
            self.handle_native_event(event, allow_cue);
        }
    }

    #[cfg(target_vendor = "apple")]
    fn drain_directional_events(&mut self, id: NativeUtteranceId) {
        let events: Vec<_> = self.native_events.try_iter().collect();
        // A short utterance can report a word and completion between ticks.
        // Never start a directional sound once speech has ended.
        let batch = DirectionalBatch::inspect(&events, id);
        if let Some(direction) = &mut self.live_direction {
            direction.saw_range |= batch.saw_range;
        }
        for event in events {
            if !batch.ended
                && let NativeEvent::Range {
                    id: native_id,
                    length,
                    ..
                } = event
                && native_id == id
                && length != 0
            {
                self.play_live_direction(id);
            }
            self.handle_native_event(event, true);
        }
    }

    #[cfg(target_vendor = "apple")]
    fn stop_live_direction(&mut self) {
        if let Some(direction) = self.live_direction.take()
            && direction.playback == DirectionPlayback::Played
        {
            self.prepared_cues.stop(direction.cue);
        }
    }

    #[cfg(target_vendor = "apple")]
    fn play_live_direction(&mut self, id: NativeUtteranceId) {
        if let Some(direction) = &mut self.live_direction
            && direction.native_id == id
            && direction.playback == DirectionPlayback::Pending
            && self.tts.is_speaking().unwrap_or(false)
        {
            match self.prepared_cues.play(direction.cue) {
                Ok(()) => {
                    direction.playback = DirectionPlayback::Played;
                    if let Some(active) = &mut self.active {
                        active.cues.retain(|cue| cue.cue == SoundCue::Link);
                    }
                    if let Some(key) = direction.cache_key.take() {
                        self.readback_cache.request(key);
                    }
                    if let Some(log) = &self.cue_event_log {
                        write_speech_event(log, "directional-cue-begin");
                    }
                }
                Err(error) => {
                    direction.playback = DirectionPlayback::Failed;
                    eprintln!("lector-tts: {error}");
                }
            }
        }
    }

    #[cfg(target_vendor = "apple")]
    fn advance_cue_playback(&mut self, allow_start: bool) {
        let Some(mut playback) = self.cue_playback.take() else {
            return;
        };
        if let Some(log) = &self.cue_event_log
            && playback.observe_cached_clock()
        {
            write_speech_event(log, "directional-cue-playing");
        }
        match playback.advance(allow_start, |event| {
            self.handle_native_event(event, allow_start);
        }) {
            Ok(false) => self.cue_playback = Some(playback),
            Ok(true) => {}
            Err(error) => {
                let Some(mut active) = self.active.take() else {
                    return;
                };
                drop(playback);
                let base = active.native_base;
                let cues = active.cues_from(base);
                eprintln!("lector-tts: {error}; speaking cue labels");
                let failure = match self.start_spoken_fallback(&mut active, base, &cues) {
                    Ok(BackendAttempt::Completed(id)) => {
                        active.native_id = id;
                        self.active = Some(active);
                        return;
                    }
                    Ok(BackendAttempt::Unavailable(message)) => message,
                    Err(error) => error.message,
                };
                self.emit_for(&active, "ended", None, Some("failed"), Some(failure));
            }
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
        Ok(RateResult {
            rate: if self.uses_normalized_rate() {
                rate.scale.normalize(rate.current)
            } else {
                rate.current
            },
        })
    }

    fn set_rate(&mut self, rate: f32) -> Result<RateResult, RpcError> {
        if !self.capabilities.settings.rate.can_write() {
            return Err(RpcError::method_not_found("speech.setRate"));
        }
        let normalized = self.uses_normalized_rate();
        if normalized && !rate_is_normalized(rate) {
            return Err(RpcError::invalid_params(
                "rate must be between 0 and 100 inclusive",
            ));
        } else if !normalized && !rate.is_finite() {
            return Err(RpcError::invalid_params("rate must be finite"));
        }
        let Some(mut effective) = self.rate else {
            return Err(RpcError::method_not_found("speech.setRate"));
        };
        let requested = if normalized {
            effective.scale.to_native(rate)
        } else {
            rate.clamp(effective.scale.min, effective.scale.max)
        };
        self.tts
            .set_rate(requested)
            .map_err(|error| RpcError::internal_error(error.to_string()))?;
        effective.current = self
            .tts
            .get_rate()
            .map_err(|error| RpcError::internal_error(error.to_string()))?;
        if !effective.current.is_finite() {
            return Err(RpcError::internal_error(
                "speech backend returned a non-finite rate",
            ));
        }
        self.rate = Some(effective);
        Ok(RateResult {
            rate: if normalized {
                effective.scale.normalize(effective.current)
            } else {
                effective.current
            },
        })
    }

    fn uses_normalized_rate(&self) -> bool {
        self.protocol
            .is_some_and(|version| version.minor >= NORMALIZED_RATE_PROTOCOL_MINOR)
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
        #[cfg(target_vendor = "apple")]
        {
            self.directional_word_ranges = true;
        }
        Ok(CurrentVoiceResult { voice: Some(voice) })
    }

    fn handle_native_event(&mut self, event: NativeEvent, allow_cue: bool) {
        #[cfg(not(target_vendor = "apple"))]
        let _ = allow_cue;
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
            NativeEvent::Range { start, length, .. } => {
                if active.direction_label.is_some() {
                    return;
                }
                let start = if let Some(fallback) = &mut active.fallback {
                    let Some(position) = fallback.report_word(start) else {
                        return;
                    };
                    position
                } else {
                    start
                };
                if active.text.is_empty() {
                    return;
                }
                let position = active.native_base.saturating_add(start);
                if position <= active.text.len() && active.text.is_char_boundary(position) {
                    active.current_word = position;
                    // Mixed playback has reached this word, so its cues have fired.
                    let direction_pending = self
                        .live_direction
                        .as_ref()
                        .is_some_and(|direction| direction.playback != DirectionPlayback::Played);
                    active.cues.retain(|cue| {
                        (direction_pending && cue.cue != SoundCue::Link)
                            || cue.offset >= position.saturating_add(length)
                    });
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
                #[cfg(target_vendor = "apple")]
                let mut active = active;
                #[cfg(target_vendor = "apple")]
                match self.complete_live_direction(&mut active, allow_cue) {
                    Ok(true) => {
                        self.active = Some(active);
                        return;
                    }
                    Ok(false) => {}
                    Err(error) => {
                        self.emit_for(
                            &active,
                            "ended",
                            None,
                            Some("failed"),
                            Some(error.to_string()),
                        );
                        return;
                    }
                }
                self.emit_for(&active, "ended", None, Some("completed"), None);
            }
            NativeEvent::Cancelled(_) => {
                if active.paused {
                    return;
                }
                #[cfg(target_vendor = "apple")]
                self.stop_live_direction();
                let active = self.active.take().expect("active checked");
                self.emit_for(&active, "ended", None, Some("cancelled"), None);
            }
        }
    }

    #[cfg(target_vendor = "apple")]
    fn complete_live_direction(
        &mut self,
        active: &mut ActiveUtterance,
        allow_cue: bool,
    ) -> Result<bool, TtsError> {
        if !self.live_direction.as_ref().is_some_and(|direction| {
            direction.playback != DirectionPlayback::Played
                && Some(direction.native_id) == active.native_id
        }) {
            return Ok(false);
        }
        let direction = self
            .live_direction
            .take()
            .expect("unfinished direction checked");
        if !direction.playback.needs_label(allow_cue && !active.paused) {
            return Ok(false);
        }
        // Only missing word ranges establish that this voice cannot supply a
        // live onset. Delayed delivery or a player failure affects this cue.
        if direction
            .playback
            .shows_missing_word_ranges(direction.saw_range)
        {
            self.directional_word_ranges = false;
        }
        if direction.saw_range
            && let Some(key) = direction.cache_key
        {
            self.readback_cache.request(key);
        }
        let id = self
            .tts
            .speak(direction.cue.spoken_label(), false)?
            .ok_or(TtsError::OperationFailed("identify spoken directional cue"))?;
        active.native_id = Some(id);
        active.direction_label = Some(direction.cue);
        active.cues.clear();
        Ok(true)
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

fn supports_sound_cues(backend: Backends) -> bool {
    #[cfg(target_vendor = "apple")]
    {
        backend == Backends::AvFoundation && cue_playback::supported()
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        let _ = backend;
        false
    }
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
    wake: &Wake,
    supported: bool,
    event_log: Option<SpeechEventLog>,
) -> std::result::Result<(), TtsError> {
    if !supported {
        return Ok(());
    }
    let send = {
        let events = events.clone();
        let wake = wake.clone();
        move |event| {
            if events.send(event).is_ok() {
                wake.notify();
            }
        }
    };
    let started = send.clone();
    tts.on_utterance_begin(move |id| {
        if let Some(log) = &event_log {
            write_speech_event(log, "begin");
        }
        started(NativeEvent::Started(id));
    })?;
    let completed = send.clone();
    tts.on_utterance_end(move |id| completed(NativeEvent::Completed(id)))?;
    let cancelled = send.clone();
    tts.on_utterance_stop(move |id| cancelled(NativeEvent::Cancelled(id)))?;
    #[cfg(target_vendor = "apple")]
    {
        tts.on_utterance_range(move |id, start, length| {
            send(NativeEvent::Range { id, start, length });
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
            state.protocol = Some(initialized.protocol);
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
            state.speak(params)?;
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
    parent_watchdog::start(expected_parent_pid)
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
    use super::{
        BackendAttempt, RateScale, classify_backend_attempt, native_capabilities, resumable_suffix,
    };
    use crate::{Error, Features, protocol::SettingSupport};

    #[cfg(target_vendor = "apple")]
    #[test]
    fn late_or_missing_native_ranges_keep_the_direction_pending_for_spoken_fallback() {
        use super::{DirectionPlayback, DirectionalBatch, NativeEvent, NativeUtteranceId};
        let id = NativeUtteranceId::AvFoundation(1);
        let unrelated = NativeUtteranceId::AvFoundation(2);
        let range = NativeEvent::Range {
            id,
            start: 0,
            length: 3,
        };

        let live = DirectionalBatch::inspect(&[range, NativeEvent::Completed(unrelated)], id);
        assert!(live.saw_range && !live.ended);
        let coalesced = DirectionalBatch::inspect(&[range, NativeEvent::Completed(id)], id);
        assert!(coalesced.saw_range && coalesced.ended);
        let absent = DirectionalBatch::inspect(&[NativeEvent::Completed(id)], id);
        assert!(!absent.saw_range && absent.ended);
        assert!(DirectionPlayback::Pending.needs_label(true));
        assert!(DirectionPlayback::Failed.needs_label(true));
        assert!(!DirectionPlayback::Played.needs_label(true));
        assert!(!DirectionPlayback::Pending.needs_label(false));
        assert!(!DirectionPlayback::Failed.needs_label(false));

        // A coalesced range or failed audio output on A does not disable
        // its voice's live route when B reports a usable range.
        for playback in [DirectionPlayback::Pending, DirectionPlayback::Failed] {
            assert!(!playback.shows_missing_word_ranges(coalesced.saw_range));
        }
        assert!(DirectionPlayback::Pending.shows_missing_word_ranges(absent.saw_range));
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn spoken_cues_map_native_offsets_across_pause_and_resume() {
        use super::SpokenFallback;
        use crate::protocol::{SoundCue, SoundCueAt};
        let mut fallback = SpokenFallback::new(
            "éclair and apple",
            &[
                SoundCueAt {
                    cue: SoundCue::Link,
                    offset: 0,
                },
                SoundCueAt {
                    cue: SoundCue::Link,
                    offset: 12,
                },
            ],
        );
        assert_eq!(fallback.text, "link éclair and link apple");
        assert_eq!(fallback.report_word(0), Some(0));
        assert_eq!(fallback.report_word(5), Some(0));
        assert_eq!(fallback.report_word(6), None);
        assert_eq!(fallback.report_word(13), Some(8));

        fallback.playback_base = fallback.word;
        assert_eq!(&fallback.text[fallback.word..], "and link apple");
        assert_eq!(fallback.report_word(0), Some(8));
        assert_eq!(fallback.report_word(4), Some(12));
        assert_eq!(fallback.report_word(9), Some(12));

        let blank = SpokenFallback::new(
            "",
            &[SoundCueAt {
                cue: SoundCue::Blank,
                offset: 0,
            }],
        );
        assert_eq!(blank.text, "blank");
    }

    #[test]
    fn resume_suffix_starts_at_the_reported_utf8_word_boundary() {
        assert_eq!(resumable_suffix("héllo world", 7), Some("world"));
        assert_eq!(resumable_suffix("héllo world", 2), None);
        assert_eq!(resumable_suffix("héllo world", 99), None);
    }

    #[test]
    fn normalized_rate_scale_anchors_both_extremes_and_backend_normal() {
        let scale = RateScale::new(0.1, 0.5, 2.0).expect("valid backend rate domain");
        for (normalized, native) in [(0.0, 0.1), (50.0, 0.5), (100.0, 2.0)] {
            assert!((scale.to_native(normalized) - native).abs() < f32::EPSILON);
            assert!((scale.normalize(native) - normalized).abs() < f32::EPSILON);
        }
        let native = scale.to_native(75.0);
        assert!((scale.normalize(native) - 75.0).abs() < 1.0e-5);
    }

    #[test]
    fn normalized_rate_scale_handles_endpoint_normal_and_rejects_invalid_domains() {
        let endpoint_normal = RateScale::new(1.0, 1.0, 2.0).expect("valid endpoint normal");
        assert!(endpoint_normal.normalize(1.0).abs() < f32::EPSILON);
        assert!((endpoint_normal.to_native(100.0) - 2.0).abs() < f32::EPSILON);
        assert!(RateScale::new(0.0, 1.0, f32::INFINITY).is_none());
        assert!(RateScale::new(2.0, 1.0, 0.0).is_none());
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
