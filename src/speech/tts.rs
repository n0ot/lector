use super::{CapabilityStatus, Driver, OptionState, SetOptionOutcome};
use anyhow::Result as DriverResult;
use tts::{
    Tts,
    host::RateScale,
    protocol::{NORMAL_RATE, rate_is_normalized},
};

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
    rate_scale: Option<RateScale>,
    pitch: Option<f32>,
    pitch_bounds: Option<(f32, f32)>,
    volume: Option<f32>,
    volume_bounds: Option<(f32, f32)>,
}

impl TtsDriver {
    pub fn new() -> Result<Self> {
        let tts = Tts::default().map_err(backend_error)?;
        let (rate, rate_scale) = if tts.supported_features().rate {
            let min_rate = tts.min_rate().map_err(backend_error)?;
            let max_rate = tts.max_rate().map_err(backend_error)?;
            let normal_rate = tts.normal_rate().map_err(backend_error)?;
            let scale = RateScale::new(min_rate, normal_rate, max_rate)
                .ok_or_else(|| Error::Backend("invalid native speech-rate domain".to_owned()))?;
            let native_rate = tts.get_rate().map_err(backend_error)?;
            if !native_rate.is_finite() {
                return Err(Error::Backend(
                    "native speech backend returned a non-finite current rate".to_owned(),
                ));
            }
            let rate = scale.normalize(native_rate);
            (rate, Some(scale))
        } else {
            (NORMAL_RATE, None)
        };
        let (pitch, pitch_bounds) = if tts.supported_features().pitch {
            let min = tts.min_pitch().map_err(backend_error)?;
            let max = tts.max_pitch().map_err(backend_error)?;
            let current = tts.get_pitch().map_err(backend_error)?;
            (Some(current), Some((min, max)))
        } else {
            (None, None)
        };
        let (volume, volume_bounds) = if tts.supported_features().volume {
            let min = tts.min_volume().map_err(backend_error)?;
            let max = tts.max_volume().map_err(backend_error)?;
            let current = tts.get_volume().map_err(backend_error)?;
            (Some(current), Some((min, max)))
        } else {
            (None, None)
        };
        Ok(TtsDriver {
            tts,
            rate,
            rate_scale,
            pitch,
            pitch_bounds,
            volume,
            volume_bounds,
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
        if !rate_is_normalized(rate) {
            return Err(anyhow::anyhow!(
                "speech rate must be between 0 and 100 inclusive"
            ));
        }
        let Some(scale) = self.rate_scale else {
            return Ok(());
        };
        self.tts
            .set_rate(scale.to_native(rate))
            .map_err(backend_error)?;
        let native_rate = self.tts.get_rate().map_err(backend_error)?;
        if !native_rate.is_finite() {
            return Err(Error::Backend(
                "native speech backend returned a non-finite current rate".to_owned(),
            )
            .into());
        }
        self.rate = scale.normalize(native_rate);
        Ok(())
    }

    fn option_state(&self) -> OptionState {
        if self.rate_scale.is_some() {
            OptionState {
                rate: Some(self.rate),
                rate_status: CapabilityStatus::Supported,
                pitch: self.pitch,
                pitch_status: if self.pitch_bounds.is_some() {
                    CapabilityStatus::Supported
                } else {
                    CapabilityStatus::Unsupported
                },
                volume: self.volume,
                volume_status: if self.volume_bounds.is_some() {
                    CapabilityStatus::Supported
                } else {
                    CapabilityStatus::Unsupported
                },
                ..OptionState::default()
            }
        } else {
            OptionState {
                rate_status: CapabilityStatus::Unsupported,
                pitch: self.pitch,
                pitch_status: if self.pitch_bounds.is_some() {
                    CapabilityStatus::Supported
                } else {
                    CapabilityStatus::Unsupported
                },
                volume: self.volume,
                volume_status: if self.volume_bounds.is_some() {
                    CapabilityStatus::Supported
                } else {
                    CapabilityStatus::Unsupported
                },
                ..OptionState::default()
            }
        }
    }

    fn set_rate_option(&mut self, rate: f32) -> DriverResult<SetOptionOutcome> {
        if self.rate_scale.is_none() {
            return Ok(SetOptionOutcome::Unsupported);
        }
        self.set_rate(rate)?;
        Ok(SetOptionOutcome::Accepted)
    }

    fn set_pitch_option(&mut self, pitch: f32) -> DriverResult<SetOptionOutcome> {
        let Some((min, max)) = self.pitch_bounds else {
            return Ok(SetOptionOutcome::Unsupported);
        };
        if !pitch.is_finite() {
            return Err(anyhow::anyhow!("speech pitch must be finite"));
        }
        let pitch = pitch.clamp(min, max);
        self.tts.set_pitch(pitch).map_err(backend_error)?;
        self.pitch = Some(pitch);
        Ok(SetOptionOutcome::Accepted)
    }

    fn set_volume_option(&mut self, volume: f32) -> DriverResult<SetOptionOutcome> {
        let Some((min, max)) = self.volume_bounds else {
            return Ok(SetOptionOutcome::Unsupported);
        };
        if !volume.is_finite() {
            return Err(anyhow::anyhow!("speech volume must be finite"));
        }
        let volume = volume.clamp(min, max);
        self.tts.set_volume(volume).map_err(backend_error)?;
        self.volume = Some(volume);
        Ok(SetOptionOutcome::Accepted)
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
