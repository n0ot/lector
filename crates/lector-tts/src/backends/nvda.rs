use std::sync::Arc;

use libloading::{Library, os::windows::Symbol};
use parking_lot::Mutex;
use tracing::{debug, info, instrument, warn};
use windows::{Win32::Foundation::WIN32_ERROR, core::HSTRING};

use crate::{
    Backend, Callbacks, Error, Features, SynthesizedAudio, UtteranceId, Voice, protocol::SoundCueAt,
};

mod audio;
mod marked;
use marked::{Marked, ProcessIdFn, SetMarkFn, SpeakSsmlFn};

/// `error_status_t`: a Win32 error code, 0 on success.
type StatusFn = unsafe extern "system" fn() -> u32;
type TextFn = unsafe extern "system" fn(*const u16) -> u32;
type IsSpeakingFn = unsafe extern "system" fn(*mut u8) -> u32;

/// Converts a controller client return value into a `Result`.
fn check(status: u32) -> Result<(), Error> {
    Ok(WIN32_ERROR(status).ok()?)
}

#[derive(Debug)]
struct MarkedController {
    speak: Symbol<SpeakSsmlFn>,
    set_mark: Symbol<SetMarkFn>,
}

/// Symbols resolved from `nvdaControllerClient.dll`.
#[derive(Debug)]
struct Controller {
    test_if_running: Symbol<StatusFn>,
    speak_text: Symbol<TextFn>,
    cancel_speech: Symbol<StatusFn>,
    /// Introduced in v3.0 of the controller client (NVDA 2026.3); absent from older DLLs.
    is_speaking: Option<Symbol<IsSpeakingFn>>,
    marked: Option<MarkedController>,
    // Declared last so the symbols above, which point into it, drop first.
    _library: Library,
}

// Applications ship the DLL themselves; current controller client releases name it
// `nvdaControllerClient.dll` for every architecture, while older releases suffixed
// the pointer width.
const DLL: &str = "nvdaControllerClient.dll";
#[cfg(target_pointer_width = "64")]
const LEGACY_DLL: &str = "nvdaControllerClient64.dll";
#[cfg(target_pointer_width = "32")]
const LEGACY_DLL: &str = "nvdaControllerClient32.dll";

impl Controller {
    fn load() -> Result<Self, libloading::Error> {
        unsafe {
            let library = Library::new(DLL).or_else(|_| Library::new(LEGACY_DLL))?;
            let test_if_running = library
                .get::<StatusFn>(b"nvdaController_testIfRunning\0")?
                .into_raw();
            let speak_text = library
                .get::<TextFn>(b"nvdaController_speakText\0")?
                .into_raw();
            let cancel_speech = library
                .get::<StatusFn>(b"nvdaController_cancelSpeech\0")?
                .into_raw();
            let is_speaking = library
                .get::<IsSpeakingFn>(b"nvdaController_isSpeaking\0")
                .ok()
                .map(|symbol| symbol.into_raw());
            // A v2 DLL can be installed alongside an older NVDA server.
            // Probe the v2 endpoint without enqueuing audible speech.
            let marked = library
                .get::<ProcessIdFn>(b"nvdaController_getProcessId\0")
                .ok()
                .filter(|get_pid| {
                    let mut pid = 0;
                    get_pid(&raw mut pid) == 0 && pid != 0
                })
                .and_then(|_| {
                    let speak = library
                        .get::<SpeakSsmlFn>(b"nvdaController_speakSsml\0")
                        .ok()?
                        .into_raw();
                    let set_mark = library
                        .get::<SetMarkFn>(b"nvdaController_setOnSsmlMarkReachedCallback\0")
                        .ok()?
                        .into_raw();
                    Some(MarkedController { speak, set_mark })
                });
            Ok(Self {
                test_if_running,
                speak_text,
                cancel_speech,
                is_speaking,
                marked,
                _library: library,
            })
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Nvda {
    controller: Arc<Controller>,
    marked: Option<Arc<Marked>>,
}

impl Nvda {
    pub(crate) const NAME: &str = "NVDA";

    /// Available only while NVDA is running and the application ships the
    /// controller client DLL.
    #[instrument(level = "debug", ret)]
    pub(crate) fn is_available() -> bool {
        match Controller::load() {
            Ok(controller) => unsafe { (controller.test_if_running)() == 0 },
            Err(error) => {
                debug!(%error, "loading the NVDA controller client failed");
                false
            }
        }
    }

    #[instrument(level = "info", skip(callbacks), err)]
    pub(crate) fn new(callbacks: Arc<Mutex<Callbacks>>) -> Result<Self, Error> {
        let mut controller = Controller::load().map_err(|error| {
            warn!(%error, "loading the NVDA controller client failed");
            Error::BackendUnavailable("the NVDA controller client DLL failed to load")
        })?;
        // A DLL that exports `isSpeaking` may still talk to an NVDA too old to
        // implement it, so probe once and drop the symbol if the call fails.
        // NVDA itself is a transient external service: explicit backend
        // selection remains valid while it is stopped and future speech calls
        // probe it again.
        controller.is_speaking = controller.is_speaking.filter(|is_speaking| {
            let mut speaking = 0u8;
            unsafe { is_speaking(&raw mut speaking) == 0 }
        });
        info!("Loaded the NVDA controller client");
        let controller = Arc::new(controller);
        let marked = controller.marked.as_ref().and_then(|_| {
            Marked::new(Arc::clone(&controller), callbacks)
                .map(Arc::new)
                .inspect_err(|error| warn!(%error, "NVDA marked speech unavailable"))
                .ok()
        });
        Ok(Self { controller, marked })
    }

    fn ensure_running(&self) -> Result<(), Error> {
        if unsafe { (self.controller.test_if_running)() } == 0 {
            Ok(())
        } else {
            Err(Error::BackendUnavailable("NVDA is not running"))
        }
    }

    fn check_operation(&self, status: u32) -> Result<(), Error> {
        if status == 0 {
            return Ok(());
        }
        self.ensure_running()?;
        check(status)
    }
}

impl Backend for Nvda {
    #[instrument(level = "trace", skip(self))]
    fn supported_features(&self) -> Features {
        Features {
            stop: true,
            is_speaking: self.controller.is_speaking.is_some(),
            utterance_callbacks: self.marked.is_some(),
            sound_cues: self
                .marked
                .as_ref()
                .is_some_and(|marked| marked.supports_cues()),
            ..Default::default()
        }
    }

    #[instrument(level = "debug", skip(self), err)]
    fn speak(&mut self, text: &str, interrupt: bool) -> Result<Option<UtteranceId>, Error> {
        if let Some(marked) = &self.marked {
            if interrupt {
                marked.cancel();
            }
            return marked.submit(text, &[]).map(Some);
        }
        self.ensure_running()?;
        if interrupt {
            self.stop()?;
        }
        let text = HSTRING::from(text);
        self.check_operation(unsafe { (self.controller.speak_text)(text.as_ptr()) })?;
        Ok(None)
    }

    fn speak_with_cues(
        &mut self,
        text: &str,
        cues: &[SoundCueAt],
    ) -> Result<Option<UtteranceId>, Error> {
        let marked = self
            .marked
            .as_ref()
            .filter(|marked| marked.supports_cues())
            .ok_or(Error::UnsupportedFeature)?;
        marked.submit(text, cues).map(Some)
    }

    #[instrument(level = "debug", skip(self, _text), err)]
    fn synthesize(&mut self, _text: &str) -> Result<SynthesizedAudio, Error> {
        unimplemented!()
    }

    #[instrument(level = "debug", skip(self), err)]
    fn stop(&mut self) -> Result<(), Error> {
        if let Some(marked) = &self.marked {
            marked.cancel();
            return Ok(());
        }
        self.ensure_running()?;
        self.check_operation(unsafe { (self.controller.cancel_speech)() })
    }

    #[instrument(level = "debug", skip(self), err)]
    fn pause(&mut self) -> Result<(), Error> {
        unimplemented!()
    }

    #[instrument(level = "debug", skip(self), err)]
    fn resume(&mut self) -> Result<(), Error> {
        unimplemented!()
    }

    #[instrument(level = "trace", skip(self), err, ret)]
    fn is_paused(&self) -> Result<bool, Error> {
        unimplemented!()
    }

    #[instrument(level = "trace", skip(self))]
    fn min_rate(&self) -> f32 {
        unimplemented!()
    }

    #[instrument(level = "trace", skip(self))]
    fn max_rate(&self) -> f32 {
        unimplemented!()
    }

    #[instrument(level = "trace", skip(self))]
    fn normal_rate(&self) -> f32 {
        unimplemented!()
    }

    #[instrument(level = "debug", skip(self), err, ret)]
    fn get_rate(&self) -> Result<f32, Error> {
        unimplemented!();
    }

    #[instrument(level = "debug", skip(self, _rate), err)]
    fn set_rate(&mut self, _rate: f32) -> Result<(), Error> {
        unimplemented!();
    }

    #[instrument(level = "trace", skip(self))]
    fn min_pitch(&self) -> f32 {
        unimplemented!()
    }

    #[instrument(level = "trace", skip(self))]
    fn max_pitch(&self) -> f32 {
        unimplemented!()
    }

    #[instrument(level = "trace", skip(self))]
    fn normal_pitch(&self) -> f32 {
        unimplemented!()
    }

    #[instrument(level = "debug", skip(self), err, ret)]
    fn get_pitch(&self) -> Result<f32, Error> {
        unimplemented!();
    }

    #[instrument(level = "debug", skip(self, _pitch), err)]
    fn set_pitch(&mut self, _pitch: f32) -> Result<(), Error> {
        unimplemented!();
    }

    #[instrument(level = "trace", skip(self))]
    fn min_volume(&self) -> f32 {
        unimplemented!()
    }

    #[instrument(level = "trace", skip(self))]
    fn max_volume(&self) -> f32 {
        unimplemented!()
    }

    #[instrument(level = "trace", skip(self))]
    fn normal_volume(&self) -> f32 {
        unimplemented!()
    }

    #[instrument(level = "debug", skip(self), err, ret)]
    fn get_volume(&self) -> Result<f32, Error> {
        unimplemented!();
    }

    #[instrument(level = "debug", skip(self, _volume), err)]
    fn set_volume(&mut self, _volume: f32) -> Result<(), Error> {
        unimplemented!();
    }

    #[instrument(level = "trace", skip(self), err, ret)]
    fn is_speaking(&self) -> Result<bool, Error> {
        let Some(is_speaking) = self.controller.is_speaking.as_ref() else {
            unimplemented!()
        };
        let mut speaking = 0u8;
        check(unsafe { is_speaking(&raw mut speaking) })?;
        Ok(speaking != 0)
    }

    #[instrument(level = "debug", skip(self), err, ret)]
    fn voice(&self) -> Result<Option<Voice>, Error> {
        unimplemented!()
    }

    #[instrument(level = "debug", skip(self), err)]
    fn voices(&self) -> Result<Vec<Voice>, Error> {
        unimplemented!()
    }

    #[instrument(level = "debug", skip(self, _voice), err)]
    fn set_voice(&mut self, _voice: &Voice) -> Result<(), Error> {
        unimplemented!()
    }
}
