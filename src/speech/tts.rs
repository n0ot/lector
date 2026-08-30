use super::Driver;
use anyhow::Result as DriverResult;
use tts::Tts;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("TTS backend: {0}")]
    Backend(String),
}

fn backend_error(error: impl std::fmt::Display) -> Error {
    Error::Backend(error.to_string())
}

pub struct TtsDriver {
    tts: Tts,
    rate: f32,
    rate_bounds: Option<(f32, f32)>,
}

impl TtsDriver {
    pub fn new() -> Result<Self> {
        let tts = Tts::default().map_err(backend_error)?;
        let (rate, rate_bounds) = if tts.supported_features().rate {
            let min_rate = tts.min_rate().map_err(backend_error)?;
            let max_rate = tts.max_rate().map_err(backend_error)?;
            let rate = tts.normal_rate().map_err(backend_error)?;
            tts.set_rate(rate).map_err(backend_error)?;
            (rate, Some((min_rate, max_rate)))
        } else {
            (1.0, None)
        };
        Ok(TtsDriver {
            tts,
            rate,
            rate_bounds,
        })
    }
}

impl Driver for TtsDriver {
    fn speak(&mut self, text: &str, interrupt: bool) -> DriverResult<()> {
        self.tts
            .speak(text, interrupt)
            .map(|_| ())
            .map_err(backend_error)
            .map_err(Into::into)
    }

    fn stop(&mut self) -> DriverResult<()> {
        self.tts
            .stop()
            .map(|_| ())
            .map_err(backend_error)
            .map_err(Into::into)
    }

    fn get_rate(&self) -> f32 {
        self.rate
    }

    fn set_rate(&mut self, rate: f32) -> DriverResult<()> {
        let Some((min_rate, max_rate)) = self.rate_bounds else {
            return Ok(());
        };
        let clamped = rate.clamp(min_rate, max_rate);
        self.tts.set_rate(clamped).map_err(backend_error)?;
        self.rate = clamped;
        Ok(())
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::{
        super::{Driver, worker::BoundedAsyncDriver},
        TtsDriver,
    };
    use std::time::{Duration, Instant};

    #[test]
    fn native_speech_enqueue_does_not_wait_for_utterance_completion() {
        let native = TtsDriver::new().expect("create native speech driver");
        if native.tts.supported_features().volume {
            native.tts.set_volume(0.0).expect("mute native speech test");
        }
        let mut driver = BoundedAsyncDriver::new(native).expect("start native speech worker");
        let deliberately_long = "Lector native speech remains asynchronous. ".repeat(40);

        let started = Instant::now();
        Driver::speak(&mut driver, &deliberately_long, true).expect("enqueue native speech");
        let elapsed = started.elapsed();
        Driver::stop(&mut driver).expect("stop native speech test");

        assert!(
            elapsed < Duration::from_millis(100),
            "native speak blocked for {elapsed:?} instead of returning after enqueue"
        );
    }
}
