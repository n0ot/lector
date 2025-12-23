use anyhow::Result;
use core_foundation::runloop;
use std::time::Duration;

const MAX_POLL_INTERVAL: Duration = Duration::from_millis(10);

pub fn tick_runloop() -> Result<()> {
    unsafe {
        let _ = runloop::CFRunLoopRunInMode(runloop::kCFRunLoopDefaultMode, 0.01, 0);
    }
    Ok(())
}

pub fn adjust_poll_timeout(current: Option<Duration>) -> Option<Duration> {
    Some(current.map_or(MAX_POLL_INTERVAL, |c| c.min(MAX_POLL_INTERVAL)))
}
