use super::Driver;
use anyhow::{Context, Result};
#[cfg(target_os = "macos")]
use cocoa_foundation::base::id;
#[cfg(target_os = "macos")]
use cocoa_foundation::foundation::NSRunLoop;
#[cfg(target_os = "macos")]
use objc::{msg_send, sel, sel_impl};

pub struct Tts {
    tts: tts::Tts,
    rate: f32,
}

impl Tts {
    pub fn new() -> Result<Self> {
        let tts = Tts {
            tts: tts::Tts::default()?,
            rate: 720.0,
        };

        #[cfg(target_os = "macos")]
        {
            let run_loop: id = unsafe { NSRunLoop::currentRunLoop() };
            unsafe {
                let _: () = msg_send![run_loop, run];
            }
        }

        Ok(tts)
    }
}

impl Driver for Tts {
    fn speak(&mut self, text: &str, interrupt: bool) -> Result<()> {
        if interrupt {
            self.stop()?;
        }

        self.tts.speak(text, interrupt)?;
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        self.tts.stop()?;
        Ok(())
    }

    fn get_rate(&self) -> f32 {
        self.rate
    }

    fn set_rate(&mut self, new_rate: f32) -> Result<()> {
        let tts::Features { rate, .. } = self.tts.supported_features();
        if rate {
            self.tts.set_rate(new_rate).context("set rate")?;
        }
        self.rate = new_rate;
        Ok(())
    }
}
