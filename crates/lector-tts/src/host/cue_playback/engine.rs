//! Short PCM buffers use one output graph so replacing them never shuts down
//! the audio device at the boundary between utterances.

use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    io::Cursor,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use block2::RcBlock;
use hound::WavReader;
use objc2::{AllocAnyThread, rc::Retained};
use objc2_avf_audio::{
    AVAudioEngine, AVAudioFormat, AVAudioMixing, AVAudioPCMBuffer, AVAudioPlayerNode,
    AVAudioPlayerNodeBufferOptions, AVAudioPlayerNodeCompletionCallbackType,
};

use crate::Error;

const MAX_NODE_FORMATS: usize = 4;
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) struct CueEngine {
    engine: Retained<AVAudioEngine>,
    nodes: RefCell<VecDeque<(PcmFormat, Retained<AVAudioPlayerNode>)>>,
    last_request: Cell<Instant>,
    failed_start: Cell<Option<Instant>>,
    in_use: Rc<Cell<bool>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct PcmFormat {
    rate: u32,
    channels: u16,
}

pub(super) struct Clip {
    buffer: Retained<AVAudioPCMBuffer>,
    format: PcmFormat,
    native_channels: u16,
}

impl CueEngine {
    pub(super) fn new() -> Self {
        let engine = unsafe { AVAudioEngine::new() };
        Self {
            engine,
            nodes: RefCell::new(VecDeque::new()),
            last_request: Cell::new(Instant::now()),
            failed_start: Cell::new(None),
            in_use: Rc::new(Cell::new(false)),
        }
    }

    fn start(&self) -> Result<(), Error> {
        self.last_request.set(Instant::now());
        if !unsafe { self.engine.isRunning() } {
            if self
                .failed_start
                .get()
                .is_some_and(|at| at.elapsed() < Duration::from_secs(5))
            {
                return Err(Error::OperationFailed("start cue output engine"));
            }
            if unsafe { self.engine.startAndReturnError() }.is_err() {
                self.failed_start.set(Some(Instant::now()));
                return Err(Error::OperationFailed("start cue output engine"));
            }
            self.failed_start.set(None);
        }
        Ok(())
    }

    pub(super) fn idle_timeout(&self) -> Option<Duration> {
        (!self.in_use.get() && unsafe { self.engine.isRunning() })
            .then(|| IDLE_TIMEOUT.saturating_sub(self.last_request.get().elapsed()))
    }

    pub(super) fn idle(&self) {
        if self.idle_timeout().is_some_and(|timeout| timeout.is_zero()) {
            // Pause releases the hardware but keeps the prepared graph.
            unsafe { self.engine.pause() };
        }
    }

    fn node_for(&self, clip: &Clip) -> Retained<AVAudioPlayerNode> {
        let mut nodes = self.nodes.borrow_mut();
        if let Some(index) = nodes.iter().position(|(format, _)| *format == clip.format) {
            let entry = nodes.remove(index).expect("matching output node");
            let node = entry.1.clone();
            nodes.push_front(entry);
            return node;
        }

        // The host plays at most one clip. Connecting one node per retained
        // format preserves sample rate and speaker channels through Apple's mixer.
        if nodes.len() == MAX_NODE_FORMATS
            && let Some((_, old)) = nodes.pop_back()
        {
            unsafe { self.engine.detachNode(&old) };
        }
        let node = unsafe { AVAudioPlayerNode::init(AVAudioPlayerNode::alloc()) };
        unsafe {
            self.engine.attachNode(&node);
            self.engine.connect_to_format(
                &node,
                &self.engine.mainMixerNode(),
                Some(&clip.buffer.format()),
            );
        }
        nodes.push_front((clip.format, node.clone()));
        node
    }

    pub(super) fn play_readback(
        &self,
        clip: &mut Clip,
        volume: f32,
    ) -> Result<NodePlayback, Error> {
        if self.in_use.get() {
            return Err(Error::OperationFailed("use occupied cue output engine"));
        }
        // Verify the output on every playback. Explicit unity routing to
        // stereo avoids the mixer's equal-power mono panning attenuation.
        // Keep native channel routing on other output layouts.
        let (hardware_channels, mixer_channels) = unsafe {
            (
                self.engine
                    .outputNode()
                    .outputFormatForBus(0)
                    .channelCount(),
                self.engine
                    .mainMixerNode()
                    .outputFormatForBus(0)
                    .channelCount(),
            )
        };
        let stereo = hardware_channels == 2 && mixer_channels == 2;
        clip.route_readback(stereo)?;
        self.play(clip, volume)
    }

    pub(super) fn play(&self, clip: &Clip, volume: f32) -> Result<NodePlayback, Error> {
        if self.in_use.get() {
            return Err(Error::OperationFailed("use occupied cue output engine"));
        }
        let node = self.node_for(clip);
        self.start()?;
        let done = Arc::new(AtomicBool::new(false));
        let finished = Arc::clone(&done);
        let complete = RcBlock::new(move |_| finished.store(true, Ordering::Release));
        unsafe {
            node.setVolume(volume);
            node.scheduleBuffer_atTime_options_completionCallbackType_completionHandler(
                &clip.buffer,
                None,
                AVAudioPlayerNodeBufferOptions::empty(),
                AVAudioPlayerNodeCompletionCallbackType::DataPlayedBack,
                RcBlock::as_ptr(&complete),
            );
            node.play();
        }
        self.in_use.set(true);
        Ok(NodePlayback {
            in_use: Rc::clone(&self.in_use),
            engine: self.engine.clone(),
            node,
            done,
            sample_rate: clip.format.rate,
            frames: unsafe { clip.buffer.frameLength() },
        })
    }
}

impl Drop for CueEngine {
    fn drop(&mut self) {
        unsafe { self.engine.stop() };
    }
}

fn pcm_format(format: PcmFormat) -> Result<Retained<AVAudioFormat>, Error> {
    unsafe {
        AVAudioFormat::initStandardFormatWithSampleRate_channels(
            AVAudioFormat::alloc(),
            f64::from(format.rate),
            u32::from(format.channels),
        )
    }
    .ok_or(Error::OperationFailed("create cue PCM format"))
}

impl Clip {
    pub(super) fn from_wave(wave: &[u8]) -> Result<Self, Error> {
        let mut wave = WavReader::new(Cursor::new(wave))?;
        let spec = wave.spec();
        let samples = wave
            .samples::<i16>()
            .map(|sample| sample.map(|sample| f32::from(sample) / 32768.0))
            .collect::<Result<Vec<_>, _>>()?;
        Self::from_pcm(spec.sample_rate, spec.channels, &samples)
    }

    pub(super) fn from_pcm(rate: u32, channels: u16, samples: &[f32]) -> Result<Self, Error> {
        // A channel count alone identifies mono and stereo layouts. A
        // multichannel source must use native speech instead of guessing
        // speaker positions or collapsing its channels.
        if !(1..=2).contains(&channels)
            || rate == 0
            || !samples.len().is_multiple_of(usize::from(channels))
        {
            return Err(Error::OperationFailed("read cue PCM format"));
        }
        let format = PcmFormat { rate, channels };
        let channels = usize::from(channels);
        Self::with_frames(samples.len() / channels, format, |channel, out| {
            for (frame, dest) in samples.chunks_exact(channels).zip(out) {
                *dest = frame[channel];
            }
        })
    }

    fn route_readback(&mut self, stereo: bool) -> Result<(), Error> {
        if self.native_channels != 1 {
            return Ok(());
        }
        let channels = if stereo { 2 } else { 1 };
        if self.format.channels != channels {
            // A stereo presentation of mono is reversible. Replace the
            // previous buffer instead of retaining a second cache copy.
            let mono = self.samples(0)?;
            let format = PcmFormat {
                channels,
                ..self.format
            };
            let routed = Self::with_frames(mono.len(), format, |_, out| {
                out.copy_from_slice(mono);
            })?;
            self.buffer = routed.buffer;
            self.format = format;
        }
        Ok(())
    }

    pub(super) fn mix_at_start(&self, other: &Self) -> Result<Self, Error> {
        if self.format != other.format || self.format.channels != 1 {
            return Err(Error::OperationFailed("mix standalone cue formats"));
        }
        let first = self.samples(0)?;
        let second = other.samples(0)?;
        Self::with_frames(first.len().max(second.len()), self.format, |_, out| {
            let mut peak = 1.0_f32;
            for (index, sample) in out.iter_mut().enumerate() {
                *sample = first.get(index).copied().unwrap_or_default()
                    + second.get(index).copied().unwrap_or_default();
                peak = peak.max(sample.abs());
            }
            if peak > 1.0 {
                // Preserve the cues' relative levels and their full tails.
                for sample in out {
                    *sample /= peak;
                }
            }
        })
    }

    fn samples(&self, channel: usize) -> Result<&[f32], Error> {
        unsafe {
            let data = self.buffer.floatChannelData();
            if data.is_null() {
                return Err(Error::OperationFailed("access cue PCM buffer"));
            }
            Ok(std::slice::from_raw_parts(
                (*data.add(channel)).as_ptr(),
                self.buffer.frameLength() as usize,
            ))
        }
    }

    fn with_frames(
        frames: usize,
        format: PcmFormat,
        mut write: impl FnMut(usize, &mut [f32]),
    ) -> Result<Self, Error> {
        if frames == 0 {
            return Err(Error::OperationFailed("read empty cue PCM"));
        }
        let count =
            u32::try_from(frames).map_err(|_| Error::OperationFailed("size cue PCM buffer"))?;
        let native_format = pcm_format(format)?;
        let buffer = unsafe {
            AVAudioPCMBuffer::initWithPCMFormat_frameCapacity(
                AVAudioPCMBuffer::alloc(),
                &native_format,
                count,
            )
        }
        .ok_or(Error::OperationFailed("allocate cue PCM buffer"))?;
        unsafe {
            buffer.setFrameLength(count);
            let data = buffer.floatChannelData();
            if data.is_null() {
                return Err(Error::OperationFailed("access cue PCM buffer"));
            }
            for channel in 0..usize::from(format.channels) {
                write(
                    channel,
                    std::slice::from_raw_parts_mut((*data.add(channel)).as_ptr(), frames),
                );
            }
        }
        Ok(Self {
            buffer,
            format,
            native_channels: format.channels,
        })
    }

    pub(super) fn source_rate(&self) -> u32 {
        self.format.rate
    }
}

pub(super) struct NodePlayback {
    in_use: Rc<Cell<bool>>,
    engine: Retained<AVAudioEngine>,
    node: Retained<AVAudioPlayerNode>,
    done: Arc<AtomicBool>,
    sample_rate: u32,
    frames: u32,
}

impl NodePlayback {
    pub(super) fn time(&self) -> f64 {
        unsafe {
            self.node
                .lastRenderTime()
                .and_then(|time| self.node.playerTimeForNodeTime(&time))
                .and_then(|time| u32::try_from(time.sampleTime()).ok())
                .map_or(0.0, |frame| f64::from(frame) / f64::from(self.sample_rate))
        }
    }

    pub(super) fn duration(&self) -> f64 {
        f64::from(self.frames) / f64::from(self.sample_rate)
    }

    pub(super) fn playing(&self) -> bool {
        !self.done.load(Ordering::Acquire) && unsafe { self.node.isPlaying() }
    }

    pub(super) fn interrupted(&self) -> bool {
        !unsafe { self.engine.isRunning() }
    }

    pub(super) fn pause(&self) {
        unsafe { self.node.pause() };
    }

    pub(super) fn resume(&self) -> bool {
        if self.interrupted() {
            return false;
        }
        unsafe { self.node.play() };
        unsafe { self.node.isPlaying() }
    }
}

impl Drop for NodePlayback {
    fn drop(&mut self) {
        // Clear this buffer and reset the node timeline while the graph runs.
        unsafe { self.node.stop() };
        self.in_use.set(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mono_readback_routes_to_both_stereo_channels_without_changing_samples() {
        let source = [0.25, -0.75, 1.0];
        let mut clip = Clip::from_pcm(22_050, 1, &source).unwrap();
        clip.route_readback(true).unwrap();
        assert_eq!(clip.source_rate(), 22_050);
        assert_eq!(clip.format.channels, 2);
        assert_eq!(clip.samples(0).unwrap(), &source);
        assert_eq!(clip.samples(1).unwrap(), &source);
        clip.route_readback(false).unwrap();
        assert_eq!(clip.format.channels, 1);
        assert_eq!(clip.samples(0).unwrap(), &source);
    }

    #[test]
    fn cache_rejects_multichannel_pcm_without_a_speaker_layout() {
        assert!(Clip::from_pcm(22_050, 3, &[0.0; 9]).is_err());
    }

    #[test]
    fn native_rate_and_distinct_channel_samples_survive_cache_preparation() {
        let mut clip = Clip::from_pcm(
            22_050,
            2,
            &[
                0.123_456_7,
                -0.3,
                -0.123_456_7,
                -0.3,
                0.123_456_7,
                -0.3,
                -0.123_456_7,
                -0.3,
            ],
        )
        .unwrap();
        clip.route_readback(false).unwrap();
        assert_eq!(clip.source_rate(), 22_050);
        let format = unsafe { clip.buffer.format() };
        assert_eq!(
            unsafe { format.sampleRate() }.to_bits(),
            22_050_f64.to_bits()
        );
        assert_eq!(unsafe { format.channelCount() }, 2);
        assert_eq!(
            clip.samples(0).unwrap(),
            &[0.123_456_7, -0.123_456_7, 0.123_456_7, -0.123_456_7]
        );
        assert_eq!(clip.samples(1).unwrap(), &[-0.3, -0.3, -0.3, -0.3]);
    }

    #[test]
    fn standalone_mix_starts_both_cues_together_and_preserves_the_whole_tail() {
        let first = Clip::from_pcm(44_100, 1, &[0.75, -0.5, 0.25]).unwrap();
        let second = Clip::from_pcm(44_100, 1, &[0.5, 0.0]).unwrap();
        let mixed = first.mix_at_start(&second).unwrap();
        assert_eq!(mixed.samples(0).unwrap(), &[1.0, -0.4, 0.2]);
    }
}
