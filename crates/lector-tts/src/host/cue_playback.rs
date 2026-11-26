//! `AVFoundation` rendering and sound cues share a single PCM playback clock.

use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    io::Cursor,
    ptr::NonNull,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{Receiver, sync_channel},
    },
    time::{Duration, Instant},
};

use block2::RcBlock;
use hound::{SampleFormat, WavReader, WavSpec, WavWriter};
use objc2::{AllocAnyThread, MainThreadMarker, rc::Retained, sel};
use objc2_avf_audio::{
    AVAudioBuffer, AVAudioPlayer, AVSpeechBoundary, AVSpeechSynthesisMarker,
    AVSpeechSynthesisMarkerMark, AVSpeechSynthesizer, AVSpeechUtterance,
};
use objc2_foundation::{NSArray, NSData, NSObjectProtocol};
use tracing::warn;

use super::{NativeEvent, NativeUtteranceId};
use crate::{
    Error, Tts,
    backends::{Chunk, build_utterance, extract_chunk},
    protocol::{SoundCue, SoundCueAt},
};

// Limit extra startup latency and memory before falling back to native speech.
// Each queued callback holds only one buffer from AVFoundation; it must never
// block the main run loop waiting for the host to receive it.
mod engine;
use engine::{Clip, CueEngine, NodePlayback};

const MIX_PEAK_CEILING: f32 = 0.98;

const RENDER_STARTUP_TIMEOUT: Duration = Duration::from_secs(4);
const RENDER_IDLE_TIMEOUT: Duration = Duration::from_millis(750);
const RENDER_TOTAL_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_RENDER_SAMPLES: usize = 1_048_576;
const MAX_QUEUED_EVENTS: usize = 1024;
const MAX_CACHED_READBACKS: usize = 4;
const MAX_CACHED_TEXT_BYTES: usize = 256;
// 10 seconds at 48 kHz, with lower-rate voices constrained by the
// duration limit as well. A mono entry routed to stereo still has at most
// two channels, bounding retained PCM to ~15 MiB across four entries.
const MAX_CACHED_SAMPLES: usize = 480_000;
const MAX_CACHED_SECONDS: usize = 10;
const STANDALONE_CUES: [SoundCue; 5] = [
    SoundCue::Top,
    SoundCue::Bottom,
    SoundCue::Left,
    SoundCue::Right,
    SoundCue::Blank,
];

fn playback_id() -> NativeUtteranceId {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    NativeUtteranceId::SoundCue(NEXT.fetch_add(1, Ordering::Relaxed))
}

fn cue_wav(cue: SoundCue) -> &'static [u8] {
    match cue {
        SoundCue::Top => include_bytes!("../../assets/top.wav"),
        SoundCue::Bottom => include_bytes!("../../assets/bottom.wav"),
        SoundCue::Left => include_bytes!("../../assets/left.wav"),
        SoundCue::Right => include_bytes!("../../assets/right.wav"),
        SoundCue::Blank => include_bytes!("../../assets/blank.wav"),
        SoundCue::Link => include_bytes!("../../assets/link.wav"),
    }
}

fn new_player(wav: &[u8], volume: f32) -> Result<Retained<AVAudioPlayer>, Error> {
    let data = NSData::with_bytes(wav);
    let player = unsafe { AVAudioPlayer::initWithData_error(AVAudioPlayer::alloc(), &data) }
        .map_err(|error| {
            warn!(%error, "Unable to open cue playback");
            Error::OperationFailed("open cue playback")
        })?;
    unsafe { player.setVolume(volume) };
    Ok(player)
}

struct CuePlayer {
    cue: SoundCue,
    audio: Retained<AVAudioPlayer>,
    prepared: Cell<bool>,
}

#[derive(Default)]
pub(super) struct PreparedCues {
    players: Vec<CuePlayer>,
    engine: RefCell<Option<CueEngine>>,
    clips: RefCell<Vec<(SoundCue, Clip)>>,
    blank_pairs: Vec<(SoundCue, Clip)>,
}

impl PreparedCues {
    pub(super) fn new(volume: f32) -> Self {
        let players = STANDALONE_CUES
            .into_iter()
            .filter_map(|cue| {
                let player = new_player(cue_wav(cue), volume).ok()?;
                Some(CuePlayer {
                    cue,
                    audio: player,
                    prepared: Cell::new(false),
                })
            })
            .collect();
        // Decode the embedded clips without touching an audio device. The
        // output graph is opened on the first actual playback request.
        let clips: Vec<_> = STANDALONE_CUES
            .into_iter()
            .filter_map(|cue| Clip::from_wave(cue_wav(cue)).ok().map(|clip| (cue, clip)))
            .collect();
        let mut blank_pairs = Vec::new();
        if let Some((_, blank)) = clips.iter().find(|(cue, _)| *cue == SoundCue::Blank) {
            blank_pairs.extend(
                clips
                    .iter()
                    .filter(|(cue, _)| cue.is_direction())
                    .filter_map(|(cue, clip)| clip.mix_at_start(blank).ok().map(|mix| (*cue, mix))),
            );
        }
        Self {
            players,
            clips: RefCell::new(clips),
            blank_pairs,
            ..Self::default()
        }
    }

    fn with_engine<T>(
        &self,
        action: impl FnOnce(&CueEngine) -> Result<T, Error>,
    ) -> Result<T, Error> {
        action(self.engine.borrow_mut().get_or_insert_with(CueEngine::new))
    }

    pub(super) fn idle_timeout(&self) -> Option<Duration> {
        self.engine
            .borrow()
            .as_ref()
            .and_then(CueEngine::idle_timeout)
    }

    pub(super) fn idle(&self) {
        if let Some(engine) = self.engine.borrow().as_ref() {
            engine.idle();
        }
    }

    fn standalone(&self, cues: &[SoundCueAt], volume: f32) -> Result<Playing, Error> {
        let cue = match cues {
            [cue] => cue.cue,
            [direction, _] => {
                let (_, clip) = self
                    .blank_pairs
                    .iter()
                    .find(|(cue, _)| *cue == direction.cue)
                    .ok_or(Error::OperationFailed("load simultaneous sound cues"))?;
                let node = self.with_engine(|engine| engine.play(clip, volume))?;
                return Ok(Playing::new(AudioOutput::Engine(node), Vec::new(), 1));
            }
            _ => return Err(Error::OperationFailed("load standalone sound cue")),
        };
        let engine_play = self.with_engine(|engine| {
            let mut clips = self.clips.borrow_mut();
            let index = if let Some(index) = clips.iter().position(|(kind, _)| *kind == cue) {
                index
            } else {
                clips.push((cue, Clip::from_wave(cue_wav(cue))?));
                clips.len() - 1
            };
            engine.play(&clips[index].1, volume)
        });
        if let Ok(node) = engine_play {
            return Ok(Playing::new(AudioOutput::Engine(node), Vec::new(), 1));
        }
        self.prepare(cue, volume)?;
        let player = self.player(cue)?;
        unsafe { player.audio.setCurrentTime(0.0) };
        Playing::start(player.audio.clone(), Vec::new(), 1)
    }

    fn player(&self, cue: SoundCue) -> Result<&CuePlayer, Error> {
        self.players
            .iter()
            .find(|player| player.cue == cue)
            .ok_or(Error::OperationFailed("load sound cue"))
    }

    pub(super) fn prepare(&self, cue: SoundCue, volume: f32) -> Result<(), Error> {
        let player = self.player(cue)?;
        unsafe {
            player.audio.setVolume(volume);
            if !player.prepared.get() {
                let prepared = player.audio.prepareToPlay();
                player.prepared.set(prepared);
                if !prepared {
                    return Err(Error::OperationFailed("prepare sound cue"));
                }
            }
        }
        Ok(())
    }

    pub(super) fn play(&self, cue: SoundCue) -> Result<(), Error> {
        let player = self.player(cue)?;
        unsafe {
            player.audio.setCurrentTime(0.0);
            if !player.audio.play() {
                player.prepared.set(false);
                return Err(Error::OperationFailed("play sound cue"));
            }
        }
        Ok(())
    }

    pub(super) fn clock_is_advancing(&self, cue: SoundCue) -> bool {
        self.player(cue).is_ok_and(|player| unsafe {
            player.audio.isPlaying() && player.audio.currentTime() > 0.0
        })
    }

    pub(super) fn stop(&self, cue: SoundCue) {
        if let Ok(player) = self.player(cue)
            && unsafe { player.audio.isPlaying() }
        {
            unsafe { player.audio.pause() };
        }
    }
}

pub(super) fn supported() -> bool {
    MainThreadMarker::new().is_some()
        && unsafe { AVSpeechSynthesizer::new() }
            .respondsToSelector(sel!(writeUtterance:toBufferCallback:toMarkerCallback:))
}

struct Word {
    byte_sample: usize,
    start: usize,
    end: usize,
}

enum RenderEvent {
    Audio(Chunk),
    Words(Vec<Word>),
}

#[derive(Default)]
struct RenderedAudio {
    spec: Option<WavSpec>,
    samples: Vec<f32>,
    words: Vec<Word>,
}

impl RenderedAudio {
    fn append(
        &mut self,
        spec: WavSpec,
        samples: impl ExactSizeIterator<Item = f32>,
    ) -> Result<(), Error> {
        if *self.spec.get_or_insert(spec) != spec || spec.channels == 0 || spec.sample_rate == 0 {
            return Err(Error::OperationFailed("inconsistent speech PCM format"));
        }
        if self.samples.len().saturating_add(samples.len()) > MAX_RENDER_SAMPLES {
            return Err(Error::OperationFailed(
                "render speech within the PCM budget",
            ));
        }
        self.samples.extend(samples);
        Ok(())
    }

    fn finish(
        mut self,
        text: &str,
        cues: &[SoundCueAt],
    ) -> Result<(Vec<f32>, Vec<WordOnset>), Error> {
        let spec = self
            .spec
            .ok_or(Error::OperationFailed("render speech PCM"))?;
        let channels = usize::from(spec.channels);
        let frames = self.samples.len() / channels;
        let bytes_per_frame = channels * usize::from(spec.bits_per_sample / 8);
        let mut words = Vec::<WordOnset>::with_capacity(self.words.len());
        for word in self.words {
            let frame = word.byte_sample / bytes_per_frame;
            if !word.byte_sample.is_multiple_of(bytes_per_frame)
                || frame >= frames
                || word.start >= word.end
                || text.get(word.start..word.end).is_none()
            {
                return Err(Error::OperationFailed("validate rendered speech marker"));
            }
            let onset = WordOnset {
                frame,
                start: word.start,
                end: word.end,
            };
            if let Some(previous) = words.last() {
                if *previous == onset {
                    continue;
                }
                // The shared PCM timeline requires stream positions in
                // callback order. Sorting would conceal offset resets,
                // competing onsets, or reversed or overlapping text ranges.
                if onset.frame <= previous.frame || onset.start < previous.end {
                    return Err(Error::OperationFailed(
                        "validate rendered speech marker order",
                    ));
                }
            }
            words.push(onset);
        }

        if words.is_empty() {
            return Err(Error::OperationFailed("render speech word markers"));
        }
        let (direction, links) = match cues {
            [direction, links @ ..] if direction.cue != SoundCue::Link => (Some(direction), links),
            _ => (None, cues),
        };
        if let Some(direction) = direction {
            // The cached short-readback route can start at the first word.
            // Remove preroll only when every omitted sample is silent.
            let preroll_samples = words[0].frame * channels;
            if links.is_empty()
                && self.samples[..preroll_samples]
                    .iter()
                    .all(|sample| *sample == 0.0)
            {
                self.samples.drain(..preroll_samples);
                let preroll_frames = words[0].frame;
                for word in &mut words {
                    word.frame -= preroll_frames;
                }
            }
            let samples = cue_samples(direction.cue, spec.sample_rate)?;
            mix_at(&mut self.samples, channels, words[0].frame, &samples);
        }
        if !links.is_empty()
            && !mix_link_cues(
                &mut self.samples,
                channels,
                text,
                &words,
                links,
                &cue_samples(SoundCue::Link, spec.sample_rate)?,
            )
        {
            return Err(Error::OperationFailed(
                "locate the linked text in rendered speech",
            ));
        }

        // If the overlay causes an over, reserve headroom with one constant
        // gain for the entire mix. A constant gain preserves the waveform
        // and avoids hard clipping or a gain change at the end of the cue.
        let mix_peak = peak(&self.samples);
        if mix_peak > 1.0 {
            let gain = MIX_PEAK_CEILING / mix_peak;
            for sample in &mut self.samples {
                *sample *= gain;
            }
        }
        Ok((self.samples, words))
    }
}

fn peak(samples: &[f32]) -> f32 {
    samples
        .iter()
        .fold(0.0, |peak, sample| peak.max(sample.abs()))
}

fn write_pcm16(spec: WavSpec, samples: &[f32]) -> Result<Vec<u8>, Error> {
    let mut wav = Cursor::new(Vec::new());
    let mut writer = WavWriter::new(
        &mut wav,
        WavSpec {
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
            ..spec
        },
    )?;
    for sample in samples {
        #[allow(clippy::cast_possible_truncation)]
        writer.write_sample((sample * f32::from(i16::MAX)).round() as i16)?;
    }
    writer.finalize()?;
    Ok(wav.into_inner())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WordOnset {
    frame: usize,
    start: usize,
    end: usize,
}

fn mix_link_cues(
    speech: &mut Vec<f32>,
    channels: usize,
    text: &str,
    words: &[WordOnset],
    cues: &[SoundCueAt],
    cue: &[f32],
) -> bool {
    let mut cues = cues.iter().peekable();
    for word in words {
        let Some(cue_at) = cues.next_if(|cue| cue.offset < word.end) else {
            continue;
        };
        // A native word onset is evidence for one cue at the start of
        // the word (or through preceding whitespace). Midword onsets,
        // multiple links within a single word, and missing markers
        // require the spoken fallback.
        if cue_at.offset > word.start
            || !text[cue_at.offset..word.start]
                .chars()
                .all(char::is_whitespace)
            || cues.peek().is_some_and(|next| next.offset < word.end)
        {
            return false;
        }
        mix_at(speech, channels, word.frame, cue);
    }
    cues.next().is_none()
}

fn mix_at(speech: &mut Vec<f32>, channels: usize, frame: usize, cue: &[f32]) {
    let needed_samples = (frame + cue.len()) * channels;
    if speech.len() < needed_samples {
        speech.resize(needed_samples, 0.0);
    }
    for (frame, &sample) in speech[frame * channels..]
        .chunks_exact_mut(channels)
        .zip(cue)
    {
        for channel in frame {
            *channel += sample;
        }
    }
}

fn cue_samples(cue: SoundCue, sample_rate: u32) -> Result<Vec<f32>, Error> {
    let mut reader = WavReader::new(Cursor::new(cue_wav(cue)))?;
    let source_rate = reader.spec().sample_rate as usize;
    let target_rate = sample_rate as usize;
    let samples = reader
        .samples::<i16>()
        .map(|sample| sample.map(|sample| f32::from(sample) / 32768.0))
        .collect::<Result<Vec<_>, _>>()?;
    // It is a short mono clip. Interpolate it into the voice's native sample
    // rate so the cue and speech can be handed to the same player.
    #[allow(clippy::cast_precision_loss)]
    let resampled = (0..(samples.len() * target_rate).div_ceil(source_rate))
        .map(|frame| {
            let source = frame * source_rate;
            let index = source / target_rate;
            let fraction = (source % target_rate) as f32 / target_rate as f32;
            let next = samples.get(index + 1).copied().unwrap_or(0.0);
            samples[index] + (next - samples[index]) * fraction
        })
        .collect();
    Ok(resampled)
}

struct RenderDeadline {
    requested: Instant,
    last_pcm: Option<Instant>,
}

impl RenderDeadline {
    fn expired(&self, now: Instant) -> bool {
        let total = now - self.requested;
        self.last_pcm
            .map_or(total >= RENDER_STARTUP_TIMEOUT, |last_pcm| {
                total >= RENDER_TOTAL_TIMEOUT || now - last_pcm >= RENDER_IDLE_TIMEOUT
            })
    }
}

struct Rendering {
    synth: Retained<AVSpeechSynthesizer>,
    events: Receiver<RenderEvent>,
    data: RenderedAudio,
    deadline: RenderDeadline,
    overflowed: Arc<AtomicBool>,
    text: String,
    cues: Vec<SoundCueAt>,
    volume: f32,
    sample_limit: usize,
}

impl Rendering {
    fn new(
        synth: Retained<AVSpeechSynthesizer>,
        utterance: &AVSpeechUtterance,
        text: String,
        cues: Vec<SoundCueAt>,
        volume: f32,
        sample_limit: usize,
    ) -> Self {
        let requested = Instant::now();
        let (send, events) = sync_channel(MAX_QUEUED_EVENTS);
        let overflowed = Arc::new(AtomicBool::new(false));
        let audio_overflow = Arc::clone(&overflowed);
        let marker_overflow = Arc::clone(&overflowed);
        let audio = send.clone();
        // Index UTF-16 boundaries once, since callbacks may report many words.
        let mut utf16_offsets = Vec::with_capacity(text.len() + 1);
        for (offset, ch) in text.char_indices() {
            utf16_offsets.push(Some(offset));
            if ch.len_utf16() == 2 {
                utf16_offsets.push(None);
            }
        }
        utf16_offsets.push(Some(text.len()));
        let buffer = RcBlock::new(move |buffer: NonNull<AVAudioBuffer>| {
            if !audio_overflow.load(Ordering::Relaxed) {
                let event = RenderEvent::Audio(extract_chunk(unsafe { buffer.as_ref() }));
                if audio.try_send(event).is_err() {
                    audio_overflow.store(true, Ordering::Relaxed);
                }
            }
        });
        let marker = RcBlock::new(move |markers: NonNull<NSArray<AVSpeechSynthesisMarker>>| {
            if marker_overflow.load(Ordering::Relaxed) {
                return;
            }
            let words = unsafe { markers.as_ref() }
                .iter()
                .filter_map(|marker| unsafe {
                    if marker.mark() != AVSpeechSynthesisMarkerMark::Word {
                        return None;
                    }
                    let range = marker.textRange();
                    let start = (*utf16_offsets.get(range.location)?)?;
                    let end = (*utf16_offsets.get(range.location.checked_add(range.length)?)?)?;
                    (start < end).then_some(Word {
                        byte_sample: marker.byteSampleOffset(),
                        start,
                        end,
                    })
                })
                .collect();
            if send.try_send(RenderEvent::Words(words)).is_err() {
                marker_overflow.store(true, Ordering::Relaxed);
            }
        });
        unsafe {
            synth.writeUtterance_toBufferCallback_toMarkerCallback(
                utterance,
                RcBlock::as_ptr(&buffer),
                RcBlock::as_ptr(&marker),
            );
        }
        Self {
            synth,
            events,
            data: RenderedAudio::default(),
            deadline: RenderDeadline {
                requested,
                last_pcm: None,
            },
            overflowed,
            text,
            cues,
            volume,
            sample_limit,
        }
    }

    fn advance(&mut self) -> Result<Option<RenderedPlayback>, Error> {
        if self.overflowed.load(Ordering::Relaxed) {
            return Err(Error::OperationFailed(
                "render speech within the callback queue budget",
            ));
        }
        let mut finished = false;
        let before = self.data.samples.len();
        for event in self.events.try_iter() {
            let (spec, sample_count) = match &event {
                RenderEvent::Audio(Chunk::F32(spec, samples)) => (Some(spec), samples.len()),
                RenderEvent::Audio(Chunk::I16(spec, samples)) => (Some(spec), samples.len()),
                _ => (None, 0),
            };
            let limit = if self.sample_limit == MAX_CACHED_SAMPLES
                && let Some(spec) = spec
            {
                self.sample_limit.min(
                    MAX_CACHED_SECONDS * spec.sample_rate as usize * usize::from(spec.channels),
                )
            } else {
                self.sample_limit
            };
            if self.data.samples.len().saturating_add(sample_count) > limit {
                return Err(Error::OperationFailed(
                    "render speech within the PCM budget",
                ));
            }
            match event {
                RenderEvent::Audio(Chunk::Done) => finished = true,
                RenderEvent::Audio(Chunk::F32(spec, samples)) => {
                    if samples.len() <= usize::from(spec.channels) {
                        finished = true;
                    } else {
                        self.data.append(spec, samples.into_iter())?;
                    }
                }
                RenderEvent::Audio(Chunk::I16(spec, samples)) => {
                    if samples.len() <= usize::from(spec.channels) {
                        finished = true;
                    } else {
                        self.data.append(
                            spec,
                            samples
                                .into_iter()
                                .map(|sample| f32::from(sample) / 32768.0),
                        )?;
                    }
                }
                RenderEvent::Audio(Chunk::Unsupported) => {
                    return Err(Error::OperationFailed("unsupported speech PCM format"));
                }
                RenderEvent::Words(mut words) => self.data.words.append(&mut words),
            }
        }

        if finished {
            let spec = self
                .data
                .spec
                .ok_or(Error::OperationFailed("render speech PCM"))?;
            let (samples, words) = std::mem::take(&mut self.data).finish(&self.text, &self.cues)?;
            return Ok(Some(RenderedPlayback {
                spec,
                samples,
                words,
            }));
        }
        let now = Instant::now();
        if self.data.samples.len() > before {
            self.deadline.last_pcm = Some(now);
        }
        if self.deadline.expired(now) {
            return Err(Error::OperationFailed("render speech PCM before timeout"));
        }
        Ok(None)
    }
}

struct RenderedPlayback {
    spec: WavSpec,
    samples: Vec<f32>,
    words: Vec<WordOnset>,
}

impl RenderedPlayback {
    fn play(&self, volume: f32) -> Result<Playing, Error> {
        let wav = write_pcm16(self.spec, &self.samples)?;
        let audio = new_player(&wav, volume)?;
        Playing::start(audio, self.words.clone(), self.spec.sample_rate)
    }
}

enum AudioOutput {
    Player(Retained<AVAudioPlayer>),
    Engine(NodePlayback),
}

impl AudioOutput {
    fn time(&self) -> f64 {
        match self {
            Self::Player(player) => unsafe { player.currentTime() },
            Self::Engine(node) => node.time(),
        }
    }

    fn duration(&self) -> f64 {
        match self {
            Self::Player(player) => unsafe { player.duration() },
            Self::Engine(node) => node.duration(),
        }
    }

    fn playing(&self) -> bool {
        match self {
            Self::Player(player) => unsafe { player.isPlaying() },
            Self::Engine(node) => node.playing(),
        }
    }

    fn interrupted(&self) -> bool {
        matches!(self, Self::Engine(node) if node.interrupted())
    }

    fn pause(&self) {
        match self {
            Self::Player(player) => unsafe { player.pause() },
            Self::Engine(node) => node.pause(),
        }
    }

    fn resume(&self) -> bool {
        match self {
            Self::Player(player) => unsafe { player.play() },
            Self::Engine(node) => node.resume(),
        }
    }
}

struct Playing {
    audio: AudioOutput,
    words: std::vec::IntoIter<WordOnset>,
    sample_rate: u32,
    paused: bool,
    announced: bool,
    first_word_frame: f64,
}

impl Playing {
    fn start(
        audio: Retained<AVAudioPlayer>,
        words: Vec<WordOnset>,
        sample_rate: u32,
    ) -> Result<Self, Error> {
        if !unsafe { audio.play() } {
            return Err(Error::OperationFailed("play synchronized audio"));
        }
        Ok(Self::new(AudioOutput::Player(audio), words, sample_rate))
    }

    fn new(audio: AudioOutput, words: Vec<WordOnset>, sample_rate: u32) -> Self {
        #[allow(clippy::cast_precision_loss)]
        let first_word_frame = words.first().map_or(0.0, |word| word.frame as f64);
        Self {
            audio,
            words: words.into_iter(),
            sample_rate,
            paused: false,
            announced: false,
            first_word_frame,
        }
    }

    fn advance(&mut self, id: NativeUtteranceId, mut emit: impl FnMut(NativeEvent)) -> bool {
        if self.audio.interrupted() {
            emit(NativeEvent::Cancelled(id));
            return true;
        }
        if self.paused {
            return false;
        }
        let playing = self.audio.playing();
        let time = if playing {
            self.audio.time()
        } else {
            // AVAudioPlayer resets currentTime when it finishes.
            self.audio.duration()
        };
        // Before the native cursor advances, a zero frame can mean queued
        // output. Do not report a word at frame zero until playback begins.
        if playing && time <= 0.0 {
            return false;
        }
        if !self.announced {
            emit(NativeEvent::Started(id));
            self.announced = true;
        }
        let frame = time * f64::from(self.sample_rate);
        #[allow(clippy::cast_precision_loss)]
        while self
            .words
            .as_slice()
            .first()
            .is_some_and(|word| word.frame as f64 <= frame)
        {
            let word = self.words.next().expect("checked word");
            emit(NativeEvent::Range {
                id,
                start: word.start,
                length: word.end - word.start,
            });
        }
        if !playing {
            emit(NativeEvent::Completed(id));
        }
        !playing
    }
}

enum Playback {
    Rendering(Rendering),
    Playing(Playing),
}

pub(super) struct CuePlayback {
    id: NativeUtteranceId,
    _utterance: Option<Retained<AVSpeechUtterance>>,
    playback: Playback,
    cached_clock_pending: bool,
}

impl CuePlayback {
    pub(super) fn new(
        tts: &Tts,
        text: &str,
        cues: Vec<SoundCueAt>,
        voice_id: Option<&str>,
    ) -> Result<Self, Error> {
        let volume = tts.get_volume()?;
        // The player applies volume once, to both the rendered speech and the
        // cue, including zero volume in the opt-in native integration tests.
        let token = build_utterance(text, tts.get_rate()?, 1.0, tts.get_pitch()?, voice_id)?;
        let synth = unsafe { AVSpeechSynthesizer::new() };
        let playback = Playback::Rendering(Rendering::new(
            synth,
            &token,
            text.to_owned(),
            cues,
            volume,
            MAX_RENDER_SAMPLES,
        ));
        Ok(Self {
            id: playback_id(),
            _utterance: Some(token),
            playback,
            cached_clock_pending: false,
        })
    }

    pub(super) fn standalone(
        cues: &[SoundCueAt],
        volume: f32,
        prepared: &PreparedCues,
    ) -> Result<Self, Error> {
        Ok(Self {
            id: playback_id(),
            _utterance: None,
            playback: Playback::Playing(prepared.standalone(cues, volume)?),
            cached_clock_pending: false,
        })
    }

    pub(super) fn observe_cached_clock(&mut self) -> bool {
        if self.cached_clock_pending
            && let Playback::Playing(play) = &self.playback
            && play.audio.playing()
            && play.audio.time() * f64::from(play.sample_rate) > play.first_word_frame
        {
            self.cached_clock_pending = false;
            return true;
        }
        false
    }

    pub(super) fn id(&self) -> NativeUtteranceId {
        self.id
    }

    fn stop(&self) {
        match &self.playback {
            Playback::Rendering(render) => {
                unsafe {
                    render
                        .synth
                        .stopSpeakingAtBoundary(AVSpeechBoundary::Immediate)
                };
            }
            Playback::Playing(play) => {
                if let AudioOutput::Player(player) = &play.audio {
                    unsafe { player.pause() };
                }
            }
        }
    }

    pub(super) fn pause(&mut self) {
        if let Playback::Playing(play) = &mut self.playback {
            play.audio.pause();
            play.paused = true;
        }
    }

    pub(super) fn resume(&mut self) -> Result<(), Error> {
        if let Playback::Playing(play) = &mut self.playback {
            if !play.audio.resume() {
                return Err(Error::OperationFailed("resume sound cue"));
            }
            play.paused = false;
        }
        Ok(())
    }

    pub(super) fn advance(
        &mut self,
        allow_start: bool,
        emit: impl FnMut(NativeEvent),
    ) -> Result<bool, Error> {
        let id = self.id();
        match &mut self.playback {
            Playback::Rendering(rendering) => {
                if allow_start && let Some(ready) = rendering.advance()? {
                    self.playback = Playback::Playing(ready.play(rendering.volume)?);
                }
                Ok(false)
            }
            Playback::Playing(play) => Ok(play.advance(id, emit)),
        }
    }
}

impl Drop for CuePlayback {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct ReadbackKey {
    text: String,
    cue: SoundCue,
    voice: Option<String>,
    rate: u32,
    pitch: u32,
}

impl ReadbackKey {
    pub(super) fn new(tts: &Tts, text: &str, cue: SoundCue, voice: Option<&str>) -> Option<Self> {
        if text.len() > MAX_CACHED_TEXT_BYTES {
            return None;
        }
        Some(Self {
            text: text.to_owned(),
            cue,
            voice: voice.map(str::to_owned),
            rate: tts.get_rate().ok()?.to_bits(),
            pitch: tts.get_pitch().ok()?.to_bits(),
        })
    }
}

struct PreparedReadback {
    clip: Clip,
    words: Vec<WordOnset>,
}

/// Process-local cache of short directional readbacks. No text or audio is
/// written to disk, and a single bounded render can be in flight.
#[derive(Default)]
pub(super) struct ReadbackCache {
    ready: VecDeque<(ReadbackKey, PreparedReadback)>,
    requested: Option<ReadbackKey>,
    rendering: Option<(ReadbackKey, Retained<AVSpeechUtterance>, Rendering)>,
}

impl ReadbackCache {
    pub(super) fn needs_poll(&self) -> bool {
        self.requested.is_some() || self.rendering.is_some()
    }

    pub(super) fn retain_request(&mut self, key: Option<&ReadbackKey>) {
        if self.requested.as_ref() != key {
            self.requested = None;
        }
        if self
            .rendering
            .as_ref()
            .is_some_and(|(in_flight, _, _)| Some(in_flight) != key)
            && let Some((_, _, render)) = self.rendering.take()
        {
            unsafe {
                render
                    .synth
                    .stopSpeakingAtBoundary(AVSpeechBoundary::Immediate)
            };
        }
    }

    pub(super) fn request(&mut self, key: ReadbackKey) {
        if !self.ready.iter().any(|(cached, _)| cached == &key)
            && !self
                .rendering
                .as_ref()
                .is_some_and(|(pending, _, _)| pending == &key)
        {
            self.requested = Some(key);
        }
    }

    pub(super) fn play(
        &mut self,
        key: &ReadbackKey,
        volume: f32,
        prepared: &PreparedCues,
    ) -> Option<CuePlayback> {
        let index = self.ready.iter().position(|(cached, _)| cached == key)?;
        let (key, mut ready) = self.ready.remove(index)?;

        let playback = Playback::Playing(Playing::new(
            AudioOutput::Engine(
                prepared
                    .with_engine(|engine| engine.play_readback(&mut ready.clip, volume))
                    .ok()?,
            ),
            ready.words.clone(),
            ready.clip.source_rate(),
        ));
        self.ready.push_front((key, ready));
        Some(CuePlayback {
            id: playback_id(),
            _utterance: None,
            playback,
            cached_clock_pending: true,
        })
    }

    pub(super) fn advance(&mut self) -> bool {
        if self.rendering.is_none()
            && let Some(key) = self.requested.take()
        {
            let Ok(token) = build_utterance(
                &key.text,
                f32::from_bits(key.rate),
                1.0,
                f32::from_bits(key.pitch),
                key.voice.as_deref(),
            ) else {
                return false;
            };
            let synth = unsafe { AVSpeechSynthesizer::new() };
            let render = Rendering::new(
                synth,
                &token,
                key.text.clone(),
                vec![SoundCueAt {
                    cue: key.cue,
                    offset: 0,
                }],
                0.0,
                MAX_CACHED_SAMPLES,
            );
            self.rendering = Some((key, token, render));
        }
        let Some((_, _, render)) = &mut self.rendering else {
            return false;
        };
        match render.advance() {
            Ok(Some(ready)) => {
                let (key, _, _) = self.rendering.take().expect("render just advanced");
                if let Ok(clip) =
                    Clip::from_pcm(ready.spec.sample_rate, ready.spec.channels, &ready.samples)
                {
                    if self.ready.len() == MAX_CACHED_READBACKS {
                        self.ready.pop_back();
                    }
                    self.ready.push_front((
                        key,
                        PreparedReadback {
                            clip,
                            words: ready.words,
                        },
                    ));
                    return true;
                }
            }
            Ok(None) => {}
            Err(_) => self.retain_request(None),
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm_overload_guard_keeps_transients_instead_of_clipping_them() {
        let wave = WavSpec {
            channels: 1,
            sample_rate: 44_100,
            bits_per_sample: 32,
            sample_format: SampleFormat::Float,
        };
        let mut pcm = RenderedAudio::default();
        pcm.append(wave, std::iter::repeat_n(0.99, 3200)).unwrap();
        pcm.words.push(Word {
            byte_sample: 0,
            start: 0,
            end: 3,
        });
        let (audio, _) = pcm
            .finish(
                "one",
                &[SoundCueAt {
                    cue: SoundCue::Link,
                    offset: 0,
                }],
            )
            .unwrap();
        let mut wave = WavReader::new(Cursor::new(write_pcm16(wave, &audio).unwrap())).unwrap();
        let samples = wave
            .samples::<i16>()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let cue = cue_samples(SoundCue::Link, 44_100).unwrap();
        let peak = samples.iter().copied().max().unwrap();
        let normalized_peak = f32::from(peak) / f32::from(i16::MAX);
        assert!((normalized_peak - MIX_PEAK_CEILING).abs() <= 0.0001);
        let baseline = f32::from(samples[cue.len()]) / f32::from(i16::MAX);
        let gain = baseline / 0.99;
        for (&actual, cue) in samples.iter().zip(cue) {
            let expected = gain * (0.99 + cue);
            let actual = f32::from(actual) / f32::from(i16::MAX);
            assert!((actual - expected).abs() <= 0.0001);
        }
    }

    #[test]
    fn cold_start_and_pcm_stalls_have_separate_render_deadlines() {
        let requested = Instant::now();
        let mut deadline = RenderDeadline {
            requested,
            last_pcm: None,
        };
        assert!(!deadline.expired(requested + Duration::from_secs(1)));
        assert!(deadline.expired(requested + RENDER_STARTUP_TIMEOUT));

        let first_pcm = requested + Duration::from_secs(2);
        deadline.last_pcm = Some(first_pcm);
        assert!(!deadline.expired(first_pcm + Duration::from_millis(250)));
        assert!(deadline.expired(first_pcm + RENDER_IDLE_TIMEOUT));

        let last_pcm = requested + RENDER_TOTAL_TIMEOUT.saturating_sub(Duration::from_millis(50));
        deadline.last_pcm = Some(last_pcm);
        assert!(deadline.expired(requested + RENDER_TOTAL_TIMEOUT));
    }

    #[test]
    fn link_is_mixed_into_speech_at_the_native_word_frame() {
        let wave_spec = WavSpec {
            channels: 2,
            sample_rate: 44_100,
            bits_per_sample: 32,
            sample_format: SampleFormat::Float,
        };
        let mut pcm = RenderedAudio::default();
        pcm.append(wave_spec, std::iter::repeat_n(0.125, 8192))
            .unwrap();
        pcm.words.push(Word {
            byte_sample: 0,
            start: 0,
            end: 3,
        });
        pcm.words.push(Word {
            byte_sample: 200 * 8,
            start: 4,
            end: 9,
        });
        let (wave, words) = pcm
            .finish(
                "one ﬁve",
                &[SoundCueAt {
                    cue: SoundCue::Link,
                    offset: 4,
                }],
            )
            .unwrap();
        assert_eq!(words[1].frame, 200);
        let mut reader =
            WavReader::new(Cursor::new(write_pcm16(wave_spec, &wave).unwrap())).unwrap();
        let samples = reader
            .samples::<i16>()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let dry = samples[0];
        assert!(samples[..400].iter().all(|sample| *sample == dry));
        let cue = cue_samples(SoundCue::Link, 44_100).unwrap();
        for (channels, sample) in samples[400..].as_chunks::<2>().0.iter().zip(cue) {
            #[allow(clippy::cast_possible_truncation)]
            let expected = ((0.125 + sample) * f32::from(i16::MAX)).round() as i16;
            assert_eq!(*channels, [expected, expected]);
        }
    }

    #[test]
    fn direction_and_link_can_share_the_first_native_word_frame() {
        let wave = WavSpec {
            channels: 1,
            sample_rate: 44_100,
            bits_per_sample: 32,
            sample_format: SampleFormat::Float,
        };
        let mut pcm = RenderedAudio::default();
        pcm.append(wave, std::iter::repeat_n(0.0, 6380)).unwrap();
        let start = 32;
        pcm.words.push(Word {
            byte_sample: start * 4,
            start: 0,
            end: 3,
        });
        let cues = [
            SoundCueAt {
                cue: SoundCue::Right,
                offset: 0,
            },
            SoundCueAt {
                cue: SoundCue::Link,
                offset: 0,
            },
        ];
        let (audio, _) = pcm.finish("one", &cues).unwrap();
        let mut reader = WavReader::new(Cursor::new(write_pcm16(wave, &audio).unwrap())).unwrap();
        let samples = reader
            .samples::<i16>()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(samples[..start].iter().all(|sample| *sample == 0));
        let direction = cue_samples(SoundCue::Right, wave.sample_rate).unwrap();
        let link = cue_samples(SoundCue::Link, wave.sample_rate).unwrap();
        for (index, (&actual, direction)) in samples[start..].iter().zip(direction).enumerate() {
            let mixed = direction + link.get(index).copied().unwrap_or(0.0);
            #[allow(clippy::cast_possible_truncation)]
            let expected = (mixed.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16;
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn ambiguous_marker_timelines_require_spoken_fallback() {
        let wave = WavSpec {
            channels: 1,
            sample_rate: 44_100,
            bits_per_sample: 32,
            sample_format: SampleFormat::Float,
        };
        for (sample, start, end) in [
            (0, 4, 7),  // reset position for a later word
            (40, 4, 7), // different words share one frame
            (80, 0, 2), // text position reverses
            (80, 2, 7), // text ranges overlap
        ] {
            let mut pcm = RenderedAudio::default();
            pcm.append(wave, std::iter::repeat_n(0.0, 64)).unwrap();
            pcm.words = vec![
                Word {
                    byte_sample: 40,
                    start: 0,
                    end: 3,
                },
                Word {
                    byte_sample: sample,
                    start,
                    end,
                },
            ];
            assert!(matches!(
                pcm.finish(
                    "one two",
                    &[SoundCueAt {
                        cue: SoundCue::Link,
                        offset: 0
                    }]
                ),
                Err(Error::OperationFailed(
                    "validate rendered speech marker order"
                ))
            ));
        }
    }

    #[test]
    fn distinct_links_inside_one_word_require_spoken_fallback() {
        let word = WordOnset {
            frame: 0,
            start: 0,
            end: 6,
        };
        let mut speech = vec![0.0; 4];
        let cues = [
            SoundCueAt {
                cue: SoundCue::Link,
                offset: 0,
            },
            SoundCueAt {
                cue: SoundCue::Link,
                offset: 3,
            },
        ];
        assert!(!mix_link_cues(
            &mut speech,
            1,
            "prefix",
            &[word],
            &cues,
            &[0.5]
        ));
        assert_eq!(speech, [0.0; 4]);
    }

    #[test]
    fn partial_markers_do_not_attach_a_cue_to_a_different_word() {
        let words = [
            WordOnset {
                frame: 0,
                start: 0,
                end: 3,
            },
            WordOnset {
                frame: 20,
                start: 9,
                end: 14,
            },
        ];
        let samples = vec![0.0; 40];
        for offset in [4, 14] {
            let mut speech = samples.clone();
            assert!(!mix_link_cues(
                &mut speech,
                1,
                "one four eight!",
                &words,
                &[SoundCueAt {
                    cue: SoundCue::Link,
                    offset
                }],
                &[0.5],
            ));
            assert_eq!(speech, samples);
        }
    }
}
