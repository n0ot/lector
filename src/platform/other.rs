use std::time::Duration;

pub fn tick_runloop() {}

pub fn settle_speech_runloop() {}

pub fn adjust_poll_timeout(current: Option<Duration>) -> Option<Duration> {
    current
}
