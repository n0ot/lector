use anyhow::Result;

pub mod tdsr;

pub trait Driver {
    fn speak(&mut self, text: &str, interrupt: bool) -> Result<()>;
    fn stop(&mut self) -> Result<()>;
    fn get_rate(&self) -> f32;
    fn set_rate(&mut self, rate: f32) -> Result<()>;
}
