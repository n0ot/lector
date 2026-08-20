use core_foundation::runloop;
use std::time::Duration;

const MAX_POLL_INTERVAL: Duration = Duration::from_millis(10);

// AVSpeechSynthesizer still relies on the process's main CFRunLoop to advance
// utterance lifecycle state, even though tts-rs owns the synthesizer itself on
// a dedicated thread. Service sources which are already ready, but never wait
// here: the mio poll below provides the bounded idle wait.
pub fn tick_runloop() {
    unsafe {
        let _ = runloop::CFRunLoopRunInMode(runloop::kCFRunLoopDefaultMode, 0.0, 1);
    }
}

/// Lets AVFoundation finish a stop/cancel transition before a replacement
/// utterance is submitted. This runs only in the isolated speech host, never
/// on Lector's terminal event loop.
pub fn settle_speech_runloop() {
    unsafe {
        let _ = runloop::CFRunLoopRunInMode(runloop::kCFRunLoopDefaultMode, 0.01, 0);
    }
}

pub fn adjust_poll_timeout(current: Option<Duration>) -> Option<Duration> {
    Some(current.map_or(MAX_POLL_INTERVAL, |timeout| timeout.min(MAX_POLL_INTERVAL)))
}

#[cfg(test)]
mod tests {
    use super::{adjust_poll_timeout, tick_runloop};
    use std::time::{Duration, Instant};

    #[test]
    fn native_speech_runloop_pump_is_frequent_but_nonblocking() {
        assert_eq!(adjust_poll_timeout(None), Some(Duration::from_millis(10)));
        assert_eq!(
            adjust_poll_timeout(Some(Duration::from_secs(1))),
            Some(Duration::from_millis(10))
        );
        assert_eq!(
            adjust_poll_timeout(Some(Duration::from_millis(2))),
            Some(Duration::from_millis(2))
        );
        let started = Instant::now();
        for _ in 0..1_000 {
            tick_runloop();
        }
        assert!(
            started.elapsed() < Duration::from_millis(50),
            "event-loop ticks must never pump a blocking CFRunLoop"
        );
    }
}
