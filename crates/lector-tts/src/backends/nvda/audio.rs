//! Embedded cue playback on the Windows default render endpoint.
//!
//! The NVDA controller supplies speech commands, not its audio endpoint or
//! volume setting. This shared-mode stream follows the Windows console
//! default and leaves the endpoint and NVDA's own settings untouched.

use std::{
    ptr,
    sync::{
        Arc,
        mpsc::{Receiver, Sender, channel},
    },
    thread,
    time::{Duration, Instant},
};

use tracing::warn;
use windows::Win32::{
    Media::Audio::{
        AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
        AUDCLNT_STREAMFLAGS_NOPERSIST, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, IAudioClient,
        IAudioClock, IAudioRenderClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
        WAVEFORMATEX, eConsole, eRender,
    },
    System::Com::{
        CLSCTX_ALL, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
        CoTaskMemFree, CoUninitialize,
    },
};

use crate::{
    Error,
    cue_pcm::{Clips, SAMPLE_RATE},
    protocol::SoundCue,
};
const FRAME_BYTES: u16 = 2;
const WAVE_FORMAT_PCM: u16 = 1;
/// In 100 ns units. Every embedded clip fits in this shared-mode buffer.
const BUFFER_DURATION: i64 = 2_500_000;
const MAX_OVERLAPPING_CUES: usize = 4;
const CUE_PLAYBACK_TIMEOUT: Duration = Duration::from_secs(2);
static FORMAT: WAVEFORMATEX = WAVEFORMATEX {
    wFormatTag: WAVE_FORMAT_PCM,
    nChannels: 1,
    nSamplesPerSec: SAMPLE_RATE,
    nAvgBytesPerSec: SAMPLE_RATE * FRAME_BYTES as u32,
    nBlockAlign: FRAME_BYTES,
    wBitsPerSample: 16,
    cbSize: 0,
};
enum Command {
    Play(Arc<[i16]>, Sender<Result<(), Error>>),
    Playing(Sender<Result<bool, Error>>),
    Stop(Sender<()>),
}

#[derive(Debug)]
pub(super) struct Audio {
    clips: Clips,
    requests: Sender<Command>,
}

impl Audio {
    pub(super) fn new() -> Result<Self, Error> {
        let clips = Clips::new()?;
        let (requests, receiver) = channel();
        let (ready, initialized) = channel();
        thread::Builder::new()
            .name("lector-nvda-cues".to_owned())
            .spawn(move || run_audio(receiver, &ready))?;
        initialized.recv().map_err(|_| audio_thread_closed())??;
        Ok(Self { clips, requests })
    }

    pub(super) fn play(&mut self, cues: &[SoundCue]) -> Result<(), Error> {
        let clip = self.clips.select(cues)?;
        self.request(|reply| Command::Play(clip, reply))?
    }

    pub(super) fn is_playing(&self) -> Result<bool, Error> {
        self.request(Command::Playing)?
    }

    pub(super) fn stop(&mut self) {
        let _ = self.request(Command::Stop);
    }

    fn request<T>(&self, command: impl FnOnce(Sender<T>) -> Command) -> Result<T, Error> {
        let (reply, result) = channel();
        self.requests
            .send(command(reply))
            .map_err(|_| audio_thread_closed())?;
        result.recv().map_err(|_| audio_thread_closed())
    }
}

fn audio_thread_closed() -> Error {
    Error::BackendUnavailable("the Windows cue audio thread stopped")
}

struct Apartment;

impl Apartment {
    fn new() -> Result<Self, Error> {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
        Ok(Self)
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

fn run_audio(commands: Receiver<Command>, ready: &Sender<Result<(), Error>>) {
    let apartment = match Apartment::new() {
        Ok(apartment) => apartment,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    // The COM interfaces below are created, used, and released within the
    // same apartment. Only messages cross into the speech backend.
    match Device::new() {
        Ok(mut output) => {
            if ready.send(Ok(())).is_ok() {
                for command in commands {
                    match command {
                        Command::Play(clip, reply) => {
                            let _ = reply.send(output.play(&clip));
                        }
                        Command::Playing(reply) => {
                            let _ = reply.send(output.is_playing());
                        }
                        Command::Stop(reply) => {
                            output.stop();
                            let _ = reply.send(());
                        }
                    }
                }
            }
        }
        Err(error) => {
            let _ = ready.send(Err(error));
        }
    }
    drop(apartment);
}

#[derive(Default)]
struct PlaybackFailure(Option<&'static str>);

impl PlaybackFailure {
    fn remember(&mut self, operation: &'static str) -> Error {
        Error::OperationFailed(self.0.get_or_insert(operation))
    }

    fn check(&self) -> Result<(), Error> {
        match self.0 {
            Some(operation) => Err(Error::OperationFailed(operation)),
            None => Ok(()),
        }
    }

    fn clear(&mut self) {
        self.0 = None;
    }
}

struct Device {
    endpoints: IMMDeviceEnumerator,
    id: String,
    streams: Vec<Stream>,
    failure: PlaybackFailure,
}

impl Device {
    fn new() -> Result<Self, Error> {
        let endpoints: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_INPROC_SERVER)? };
        let endpoint = unsafe { endpoints.GetDefaultAudioEndpoint(eRender, eConsole)? };
        let id = endpoint_id(&endpoint)?;
        let streams = vec![Stream::new(&endpoint)?];
        Ok(Self {
            endpoints,
            id,
            streams,
            failure: PlaybackFailure::default(),
        })
    }

    fn play(&mut self, samples: &[i16]) -> Result<(), Error> {
        self.update_playback()?;
        let endpoint = self.current_endpoint()?;
        let index = match self
            .streams
            .iter()
            .position(|stream| stream.active.is_none())
        {
            Some(index) => index,
            None if self.streams.len() < MAX_OVERLAPPING_CUES => {
                let stream = Stream::new(&endpoint).map_err(|error| self.fail_if_active(error))?;
                self.streams.push(stream);
                self.streams.len() - 1
            }
            None => return Err(Error::OperationFailed("overlap additional Windows cues")),
        };
        // A second nearby marker can start while the first cue decays.
        // Reuse the prepared streams and let WASAPI mix their output.
        if let Err(error) = self.streams[index].play(samples) {
            // Device invalidation and incomplete submission must not leave
            // queued cue audio attached to the next utterance.
            self.streams.remove(index);
            return Err(self.fail_if_active(error));
        }
        Ok(())
    }

    fn has_active_stream(&self) -> bool {
        self.streams.iter().any(|stream| stream.active.is_some())
    }

    fn fail(&mut self, cause: &Error) -> Error {
        let operation = match cause {
            Error::OperationFailed(operation) => *operation,
            _ => "continue Windows cue playback",
        };
        // One missing region is a failure for the entire cue sequence. Stop
        // its other streams and keep the failure until explicit cancellation
        // or the start of a new utterance calls stop().
        let failure = self.failure.remember(operation);
        self.streams.clear();
        warn!(%cause, "Windows cue playback ended unexpectedly");
        failure
    }

    fn fail_if_active(&mut self, cause: Error) -> Error {
        if self.has_active_stream() {
            self.fail(&cause)
        } else {
            cause
        }
    }

    fn current_endpoint(&mut self) -> Result<IMMDevice, Error> {
        let result = (|| -> Result<_, Error> {
            let endpoint = unsafe { self.endpoints.GetDefaultAudioEndpoint(eRender, eConsole)? };
            let id = endpoint_id(&endpoint)?;
            Ok((endpoint, id))
        })();
        let (endpoint, id) = result.map_err(|error| self.fail_if_active(error))?;
        if self.id != id {
            if self.has_active_stream() {
                return Err(self.fail(&Error::OperationFailed(
                    "keep the Windows cue output endpoint",
                )));
            }
            self.streams.clear();
            self.id = id;
        }
        Ok(endpoint)
    }

    fn update_playback(&mut self) -> Result<(), Error> {
        self.failure.check()?;
        for stream in &mut self.streams {
            if let Err(error) = stream.update_playback() {
                return Err(self.fail(&error));
            }
        }
        Ok(())
    }

    fn is_playing(&mut self) -> Result<bool, Error> {
        self.update_playback()?;
        if self.has_active_stream() {
            // Detect a reroute even when no further cue is appended.
            self.current_endpoint()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn stop(&mut self) {
        self.failure.clear();
        self.streams.retain_mut(|stream| match stream.stop() {
            Ok(()) => true,
            Err(error) => {
                warn!(%error, "Stopping the Windows cue stream failed");
                false
            }
        });
    }
}

fn endpoint_id(endpoint: &IMMDevice) -> Result<String, Error> {
    unsafe {
        let id = endpoint.GetId()?;
        let value = id.to_string().map_err(|_| Error::UnexpectedResponse);
        CoTaskMemFree(Some(id.as_ptr().cast()));
        value
    }
}

struct Stream {
    clock: IAudioClock,
    render: IAudioRenderClient,
    client: IAudioClient,
    capacity: u32,
    frequency: u64,
    active: Option<(u32, Instant)>,
}

impl Stream {
    fn new(endpoint: &IMMDevice) -> Result<Self, Error> {
        unsafe {
            let client: IAudioClient = endpoint.Activate(CLSCTX_ALL, None)?;
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                    | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY
                    | AUDCLNT_STREAMFLAGS_NOPERSIST,
                BUFFER_DURATION,
                0,
                &raw const FORMAT,
                None,
            )?;
            let capacity = client.GetBufferSize()?;
            let render = client.GetService()?;
            let clock: IAudioClock = client.GetService()?;
            let frequency = clock.GetFrequency()?;
            if frequency == 0 {
                return Err(Error::OperationFailed("read Windows cue clock frequency"));
            }
            Ok(Self {
                clock,
                render,
                client,
                capacity,
                frequency,
                active: None,
            })
        }
    }

    fn play(&mut self, samples: &[i16]) -> Result<(), Error> {
        self.stop()?;
        let frames = u32::try_from(samples.len())
            .map_err(|_| Error::OperationFailed("fit cue in Windows audio buffer"))?;
        if frames > self.capacity {
            return Err(Error::OperationFailed("fit cue in Windows audio buffer"));
        }
        unsafe {
            let output = self.render.GetBuffer(frames)?;
            // The shared stream has exactly one 16-bit PCM sample per frame.
            // WASAPI owns this buffer and keeps the copy after ReleaseBuffer.
            ptr::copy_nonoverlapping(
                samples.as_ptr().cast::<u8>(),
                output,
                std::mem::size_of_val(samples),
            );
            self.render.ReleaseBuffer(frames, 0)?;
            self.client.Start()?;
        }
        self.active = Some((frames, Instant::now()));
        Ok(())
    }

    fn update_playback(&mut self) -> Result<(), Error> {
        let Some((frames, started)) = self.active else {
            return Ok(());
        };
        let mut position = 0;
        unsafe { self.clock.GetPosition(&raw mut position, None)? };
        if u128::from(position) * u128::from(SAMPLE_RATE)
            >= u128::from(frames) * u128::from(self.frequency)
        {
            return self.stop();
        }
        if started.elapsed() >= CUE_PLAYBACK_TIMEOUT {
            return Err(Error::OperationFailed("advance Windows cue playback clock"));
        }
        Ok(())
    }

    fn stop(&mut self) -> Result<(), Error> {
        if self.active.is_some() {
            unsafe { self.client.Stop()? };
            self.active = None;
            unsafe { self.client.Reset()? };
        }
        Ok(())
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disconnected_worker_reports_failure_after_successful_play() {
        let (requests, commands) = channel();
        let mut audio = Audio {
            clips: Clips::new().unwrap(),
            requests,
        };
        let worker = thread::spawn(move || {
            let Command::Play(_, reply) = commands.recv().unwrap() else {
                panic!("expected a play request");
            };
            reply.send(Ok(())).unwrap();
        });
        audio.play(&[SoundCue::Top]).unwrap();
        worker.join().unwrap();
        assert!(matches!(
            audio.is_playing(),
            Err(Error::BackendUnavailable(_))
        ));
    }

    #[test]
    fn post_start_failure_survives_more_reports_until_stop() {
        let mut failure = PlaybackFailure::default();
        assert!(failure.check().is_ok());
        assert!(matches!(
            failure.remember("advance Windows cue playback clock"),
            Error::OperationFailed("advance Windows cue playback clock")
        ));
        assert!(matches!(
            failure.remember("switch Windows cue device during an overlap"),
            Error::OperationFailed("advance Windows cue playback clock")
        ));
        for _ in 0..2 {
            assert!(matches!(
                failure.check(),
                Err(Error::OperationFailed("advance Windows cue playback clock"))
            ));
        }
        failure.clear();
        assert!(failure.check().is_ok());
    }
}
