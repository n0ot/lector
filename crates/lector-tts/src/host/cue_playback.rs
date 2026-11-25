//! `AVFoundation` rendering and sound cues share a single PCM playback clock.

use std::{
    io::Cursor,
    ptr::NonNull,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
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
const RENDER_STARTUP_TIMEOUT: Duration = Duration::from_secs(4);
const RENDER_IDLE_TIMEOUT: Duration = Duration::from_millis(750);
const RENDER_TOTAL_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_RENDER_SAMPLES: usize = 1_048_576;
const MAX_QUEUED_EVENTS: usize = 1024;

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
    ) -> Result<(Vec<u8>, Vec<WordOnset>), Error> {
        let spec = self
            .spec
            .ok_or(Error::OperationFailed("render speech PCM"))?;
        let channels = usize::from(spec.channels);
        let frames = self.samples.len() / channels;
        let bytes_per_frame = channels * usize::from(spec.bits_per_sample / 8);
        let mut words: Vec<_> = self
            .words
            .drain(..)
            .filter_map(|word| {
                // AVFoundation reports a byte position in the rendered PCM stream.
                // Check it describes a complete frame that actually exists.
                let frame = word.byte_sample / bytes_per_frame;
                (word.byte_sample.is_multiple_of(bytes_per_frame) && frame < frames).then_some(
                    WordOnset {
                        frame,
                        start: word.start,
                        end: word.end,
                    },
                )
            })
            .collect();
        words.sort_by_key(|word| (word.frame, word.start));
        words.dedup_by_key(|word| (word.frame, word.start, word.end));

        if words.is_empty() {
            return Err(Error::OperationFailed("render speech word markers"));
        }
        let cue = link_samples(spec.sample_rate)?;
        if !mix_link_cues(&mut self.samples, channels, text, &words, cues, &cue) {
            return Err(Error::OperationFailed(
                "locate the linked text in rendered speech",
            ));
        }

        let mut wav = Cursor::new(Vec::new());
        let mut writer = WavWriter::new(
            &mut wav,
            WavSpec {
                bits_per_sample: 16,
                sample_format: SampleFormat::Int,
                ..spec
            },
        )?;
        for sample in self.samples {
            #[allow(clippy::cast_possible_truncation)]
            writer.write_sample((sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16)?;
        }
        writer.finalize()?;
        Ok((wav.into_inner(), words))
    }
}

#[derive(Debug)]
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
        let needed_samples = (word.frame + cue.len()) * channels;
        if speech.len() < needed_samples {
            speech.resize(needed_samples, 0.0);
        }
        for (frame, &sample) in speech[word.frame * channels..]
            .chunks_exact_mut(channels)
            .zip(cue)
        {
            for channel in frame {
                *channel += sample;
            }
        }
    }
    cues.next().is_none()
}

fn link_samples(sample_rate: u32) -> Result<Vec<f32>, Error> {
    let mut reader = WavReader::new(Cursor::new(cue_wav(SoundCue::Link)))?;
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
}

impl Rendering {
    fn new(
        synth: Retained<AVSpeechSynthesizer>,
        utterance: &AVSpeechUtterance,
        text: String,
        cues: Vec<SoundCueAt>,
        volume: f32,
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
        }
    }

    fn advance(&mut self) -> Result<Option<Playing>, Error> {
        if self.overflowed.load(Ordering::Relaxed) {
            return Err(Error::OperationFailed(
                "render speech within the callback queue budget",
            ));
        }
        let mut finished = false;
        let before = self.data.samples.len();
        for event in self.events.try_iter() {
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
            let sample_rate = self.data.spec.map_or(1, |spec| spec.sample_rate);
            let (wav, words) = std::mem::take(&mut self.data).finish(&self.text, &self.cues)?;
            return Playing::new(&wav, self.volume, words, sample_rate).map(Some);
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

struct Playing {
    audio: Retained<AVAudioPlayer>,
    words: std::vec::IntoIter<WordOnset>,
    sample_rate: u32,
    paused: bool,
    announced: bool,
}

impl Playing {
    fn new(
        wav: &[u8],
        volume: f32,
        words: Vec<WordOnset>,
        sample_rate: u32,
    ) -> Result<Self, Error> {
        let audio = new_player(wav, volume)?;
        if !unsafe { audio.play() } {
            return Err(Error::OperationFailed("play synchronized audio"));
        }
        Ok(Self {
            audio,
            words: words.into_iter(),
            sample_rate,
            paused: false,
            announced: false,
        })
    }

    fn advance(&mut self, id: NativeUtteranceId, mut emit: impl FnMut(NativeEvent)) -> bool {
        if !self.announced {
            emit(NativeEvent::Started(id));
            self.announced = true;
        }
        if self.paused {
            return false;
        }
        let playing = unsafe { self.audio.isPlaying() };
        let time = if playing {
            unsafe { self.audio.currentTime() }
        } else {
            // AVAudioPlayer resets currentTime when it finishes.
            unsafe { self.audio.duration() }
        };
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
    token: Retained<AVSpeechUtterance>,
    playback: Playback,
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
        let playback = if let [cue] = cues.as_slice()
            && text.is_empty()
        {
            Playback::Playing(Playing::new(cue_wav(cue.cue), volume, Vec::new(), 1)?)
        } else {
            let synth = unsafe { AVSpeechSynthesizer::new() };
            Playback::Rendering(Rendering::new(synth, &token, text.to_owned(), cues, volume))
        };
        Ok(Self { token, playback })
    }

    pub(super) fn id(&self) -> NativeUtteranceId {
        NativeUtteranceId::AvFoundation(std::ptr::from_ref(&*self.token) as usize)
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
            Playback::Playing(play) => unsafe { play.audio.stop() },
        }
    }

    pub(super) fn pause(&mut self) {
        if let Playback::Playing(play) = &mut self.playback {
            unsafe { play.audio.pause() };
            play.paused = true;
        }
    }

    pub(super) fn resume(&mut self) -> Result<(), Error> {
        if let Playback::Playing(play) = &mut self.playback {
            if !unsafe { play.audio.play() } {
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
                if allow_start && let Some(play) = rendering.advance()? {
                    self.playback = Playback::Playing(play);
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let wave = WavSpec {
            channels: 2,
            sample_rate: 44_100,
            bits_per_sample: 32,
            sample_format: SampleFormat::Float,
        };
        let mut pcm = RenderedAudio::default();
        pcm.append(wave, std::iter::repeat_n(0.125, 8192)).unwrap();
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
        let mut reader = WavReader::new(Cursor::new(wave)).unwrap();
        let samples = reader
            .samples::<i16>()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let dry = samples[0];
        assert!(samples[..400].iter().all(|sample| *sample == dry));
        let cue = link_samples(44_100).unwrap();
        for (channels, sample) in samples[400..].as_chunks::<2>().0.iter().zip(cue) {
            #[allow(clippy::cast_possible_truncation)]
            let expected = ((0.125 + sample) * f32::from(i16::MAX)).round() as i16;
            assert_eq!(*channels, [expected, expected]);
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
