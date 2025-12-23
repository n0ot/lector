#[cfg(target_os = "macos")]
mod macos;
#[cfg(not(target_os = "macos"))]
mod other;

#[cfg(target_os = "macos")]
pub use macos::{adjust_poll_timeout, tick_runloop};
#[cfg(not(target_os = "macos"))]
pub use other::{adjust_poll_timeout, tick_runloop};
