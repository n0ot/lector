use super::Driver;
use anyhow::{Result, anyhow};
use tts::Tts;

pub struct TtsDriver {
    tts: Tts,
    rate: f32,
    min_rate: f32,
    max_rate: f32,
}

impl TtsDriver {
    pub fn new() -> Result<Self> {
        let mut tts = Tts::default().map_err(|e| anyhow!(e))?;
        let min_rate = tts.min_rate();
        let max_rate = tts.max_rate();
        let rate = tts.normal_rate();
        tts.set_rate(rate).map_err(|e| anyhow!(e))?;
        Ok(TtsDriver {
            tts,
            rate,
            min_rate,
            max_rate,
        })
    }
}

impl Driver for TtsDriver {
    fn speak(&mut self, text: &str, interrupt: bool) -> Result<()> {
        self.tts
            .speak(text, interrupt)
            .map(|_| ())
            .map_err(|e| anyhow!(e))
    }

    fn stop(&mut self) -> Result<()> {
        self.tts.stop().map(|_| ()).map_err(|e| anyhow!(e))
    }

    fn get_rate(&self) -> f32 {
        self.rate
    }

    fn set_rate(&mut self, rate: f32) -> Result<()> {
        let clamped = rate.clamp(self.min_rate, self.max_rate);
        self.tts.set_rate(clamped).map_err(|e| anyhow!(e))?;
        self.rate = clamped;
        Ok(())
    }

}
