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
    Playing(Sender<bool>),
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

    pub(super) fn is_playing(&self) -> bool {
        self.request(Command::Playing).unwrap_or(false)
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

struct Device {
    endpoints: IMMDeviceEnumerator,
    id: String,
    streams: Vec<Stream>,
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
        })
    }

    fn play(&mut self, samples: &[i16]) -> Result<(), Error> {
        // Recheck the default so headset changes do not keep sending audio
        // to the previously selected endpoint.
        let endpoint = unsafe { self.endpoints.GetDefaultAudioEndpoint(eRender, eConsole)? };
        let id = endpoint_id(&endpoint)?;
        if self.id != id {
            self.streams.clear();
            self.id = id;
        }
        self.update_playback();
        let index = match self
            .streams
            .iter()
            .position(|stream| stream.active.is_none())
        {
            Some(index) => index,
            None if self.streams.len() < MAX_OVERLAPPING_CUES => {
                self.streams.push(Stream::new(&endpoint)?);
                self.streams.len() - 1
            }
            None => return Err(Error::OperationFailed("overlap additional Windows cues")),
        };
        // A second nearby marker can start while the first cue decays.
        // Reuse the prepared streams and let WASAPI mix their output.
        let result = self.streams[index].play(samples);
        if result.is_err() {
            // Device invalidation and incomplete submission must not leave
            // queued cue audio attached to the next utterance.
            self.streams.remove(index);
        }
        result
    }

    fn update_playback(&mut self) {
        self.streams
            .retain_mut(|stream| match stream.update_playback() {
                Ok(()) => true,
                Err(error) => {
                    warn!(%error, "Windows cue playback ended unexpectedly");
                    false
                }
            });
    }

    fn is_playing(&mut self) -> bool {
        self.update_playback();
        self.streams.iter().any(|stream| stream.active.is_some())
    }

    fn stop(&mut self) {
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
