use anyhow::Result;
use std::time::Duration;

pub fn tick_runloop() -> Result<()> {
    Ok(())
}

pub fn adjust_poll_timeout(current: Option<Duration>) -> Option<Duration> {
    current
}
