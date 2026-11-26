//! Short PCM buffers use one output graph so replacing them never shuts down
//! the audio device at the boundary between utterances.

use std::{
    cell::Cell,
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

const RATE: u32 = 44_100;
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) struct CueEngine {
    engine: Retained<AVAudioEngine>,
    node: Retained<AVAudioPlayerNode>,
    format: Retained<AVAudioFormat>,
    last_request: Cell<Instant>,
    in_use: Rc<Cell<bool>>,
}

pub(super) struct Clip {
    buffer: Retained<AVAudioPCMBuffer>,
    source_rate: u32,
}

impl CueEngine {
    pub(super) fn new() -> Result<Self, Error> {
        let engine = unsafe { AVAudioEngine::new() };
        let node = unsafe { AVAudioPlayerNode::init(AVAudioPlayerNode::alloc()) };
        let format = unsafe {
            AVAudioFormat::initStandardFormatWithSampleRate_channels(
                AVAudioFormat::alloc(),
                f64::from(RATE),
                1,
            )
        }
        .ok_or(Error::OperationFailed("create cue output format"))?;
        unsafe {
            engine.attachNode(&node);
            engine.connect_to_format(&node, &engine.mainMixerNode(), Some(&format));
            engine.prepare();
        }
        let output = Self {
            engine,
            node,
            format,
            last_request: Cell::new(Instant::now()),
            in_use: Rc::new(Cell::new(false)),
        };
        output.start()?;
        Ok(output)
    }

    fn start(&self) -> Result<(), Error> {
        self.last_request.set(Instant::now());
        if !unsafe { self.engine.isRunning() } {
            unsafe { self.engine.startAndReturnError() }
                .map_err(|_| Error::OperationFailed("start cue output engine"))?;
        }
        Ok(())
    }

    pub(super) fn idle(&self) {
        if self.last_request.get().elapsed() >= IDLE_TIMEOUT
            && !self.in_use.get()
            && unsafe { self.engine.isRunning() }
        {
            unsafe { self.engine.stop() };
        }
    }

    pub(super) fn clip(&self, wave: &[u8]) -> Result<Clip, Error> {
        let mut wave = WavReader::new(Cursor::new(wave))?;
        let spec = wave.spec();
        let source_rate = spec.sample_rate as usize;
        let channels = usize::from(spec.channels);
        if channels == 0 || source_rate == 0 {
            return Err(Error::OperationFailed("read cue PCM format"));
        }
        let source = wave
            .samples::<i16>()
            .map(|sample| sample.map(|sample| f32::from(sample) / 32768.0))
            .collect::<Result<Vec<_>, _>>()?;
        let input: Vec<_> = source
            .chunks_exact(channels)
            .map(|frame| frame.iter().sum::<f32>() / f32::from(spec.channels))
            .collect();
        let rate = RATE as usize;
        let frames = (input.len() * rate).div_ceil(source_rate);
        if frames == 0 {
            return Err(Error::OperationFailed("read empty cue PCM"));
        }
        let count =
            u32::try_from(frames).map_err(|_| Error::OperationFailed("size cue PCM buffer"))?;
        let buffer = unsafe {
            AVAudioPCMBuffer::initWithPCMFormat_frameCapacity(
                AVAudioPCMBuffer::alloc(),
                &self.format,
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
            let out = std::slice::from_raw_parts_mut((*data).as_ptr(), frames);
            for (frame, sample) in out.iter_mut().enumerate() {
                let position = frame * source_rate;
                let offset = position / rate;
                #[allow(clippy::cast_precision_loss)]
                let fraction = (position % rate) as f32 / rate as f32;
                let current = input[offset];
                let next = input.get(offset + 1).copied().unwrap_or(0.0);
                *sample = current + (next - current) * fraction;
            }
        }
        Ok(Clip {
            buffer,
            source_rate: spec.sample_rate,
        })
    }

    pub(super) fn play(&self, clip: &Clip, volume: f32) -> Result<NodePlayback, Error> {
        self.start()?;
        let done = Arc::new(AtomicBool::new(false));
        let finished = Arc::clone(&done);
        let complete = RcBlock::new(move |_| finished.store(true, Ordering::Release));
        unsafe {
            self.node.setVolume(volume);
            self.node
                .scheduleBuffer_atTime_options_completionCallbackType_completionHandler(
                    &clip.buffer,
                    None,
                    AVAudioPlayerNodeBufferOptions::empty(),
                    AVAudioPlayerNodeCompletionCallbackType::DataPlayedBack,
                    RcBlock::as_ptr(&complete),
                );
            self.node.play();
        }
        self.in_use.set(true);
        Ok(NodePlayback {
            in_use: Rc::clone(&self.in_use),
            engine: self.engine.clone(),
            node: self.node.clone(),
            done,
            frames: unsafe { clip.buffer.frameLength() },
        })
    }
}

impl Drop for CueEngine {
    fn drop(&mut self) {
        unsafe { self.engine.stop() };
    }
}

impl Clip {
    pub(super) fn source_rate(&self) -> u32 {
        self.source_rate
    }
}

pub(super) struct NodePlayback {
    in_use: Rc<Cell<bool>>,
    engine: Retained<AVAudioEngine>,
    node: Retained<AVAudioPlayerNode>,
    done: Arc<AtomicBool>,
    frames: u32,
}

impl NodePlayback {
    pub(super) fn time(&self) -> f64 {
        unsafe {
            self.node
                .lastRenderTime()
                .and_then(|time| self.node.playerTimeForNodeTime(&time))
                .and_then(|time| u32::try_from(time.sampleTime()).ok())
                .map_or(0.0, |frame| f64::from(frame) / f64::from(RATE))
        }
    }

    pub(super) fn duration(&self) -> f64 {
        f64::from(self.frames) / f64::from(RATE)
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
