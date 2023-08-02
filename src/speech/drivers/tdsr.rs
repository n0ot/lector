use super::Driver;
use anyhow::{anyhow, Result};
use std::{
    ffi::OsStr,
    io::{BufWriter, Write},
};

pub struct Tdsr {
    child: std::process::Child,
    stdin: BufWriter<std::process::ChildStdin>,
    rate: f32,
}

impl Tdsr {
    pub fn new<S: AsRef<OsStr>>(program: S) -> Result<Self> {
        let mut child = std::process::Command::new(program)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()?;
        let stdin = BufWriter::new(child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?);

        let tts = Tdsr {
            child,
            stdin,
            rate: 200.0,
        };

        Ok(tts)
    }
}

impl Driver for Tdsr {
    fn speak(&mut self, text: &str, interrupt: bool) -> Result<()> {
        if interrupt {
            self.stop()?;
        }

        let text = text
            .chars()
            .map(|c| if c.is_whitespace() { ' ' } else { c })
            .filter(|c| !c.is_control())
            .collect::<String>();
        if !text.is_empty() {
            writeln!(self.stdin, "s{}", text)?;
            self.stdin.flush()?;
        }
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        writeln!(self.stdin, "x")?;
        self.stdin.flush()?;
        Ok(())
    }

    fn get_rate(&self) -> f32 {
        self.rate
    }

    fn set_rate(&mut self, rate: f32) -> Result<()> {
        writeln!(self.stdin, "r{}", rate)?;
        self.rate = rate;
        Ok(())
    }
}

impl Drop for Tdsr {
    fn drop(&mut self) {
        self.child.kill().unwrap();
        let _ = self.child.wait().unwrap();
    }
}
