//! Coalesced notification between protocol input, backend callbacks, and the host.

use std::time::Duration;

#[cfg(target_vendor = "apple")]
mod platform {
    use core_foundation::{
        base::TCFType,
        runloop::{
            CFRunLoop, CFRunLoopGetCurrent, CFRunLoopRunResult, CFRunLoopSource,
            CFRunLoopSourceContext, CFRunLoopSourceCreate, CFRunLoopSourceInvalidate,
            CFRunLoopSourceSignal, CFRunLoopWakeUp, kCFRunLoopDefaultMode,
        },
    };
    use std::{
        ffi::c_void,
        io, ptr,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };

    // A source callback can run inside an unrelated nested run loop. Use a
    // practical indefinite wait that returns after a source, without stopping
    // either that run loop or our own from the source callback.
    const IDLE_WAIT: Duration = Duration::from_hours(100 * 365 * 24);

    struct Signal {
        run_loop: CFRunLoop,
        run_loop_source: CFRunLoopSource,
        pending: AtomicBool,
    }

    // Core Foundation permits signaling and invalidating a run loop source
    // from other threads. Only the calling thread waits on its run loop.
    unsafe impl Send for Signal {}
    unsafe impl Sync for Signal {}

    #[derive(Clone)]
    pub(crate) struct Wake(Arc<Signal>);

    pub(super) struct Wait {
        wake: Wake,
    }

    impl Wait {
        pub(super) fn new() -> io::Result<Self> {
            extern "C" fn perform(_: *const c_void) {}

            let run_loop = CFRunLoop::get_current();
            let mut context = CFRunLoopSourceContext {
                version: 0,
                info: ptr::null_mut(),
                retain: None,
                release: None,
                copyDescription: None,
                equal: None,
                hash: None,
                schedule: None,
                cancel: None,
                perform,
            };
            let raw = unsafe { CFRunLoopSourceCreate(ptr::null(), 100, &raw mut context) };
            if raw.is_null() {
                return Err(io::Error::other("create speech host wake source"));
            }
            let source = unsafe { CFRunLoopSource::wrap_under_create_rule(raw) };
            run_loop.add_source(&source, unsafe { kCFRunLoopDefaultMode });
            Ok(Self {
                wake: Wake(Arc::new(Signal {
                    run_loop,
                    run_loop_source: source,
                    pending: AtomicBool::new(false),
                })),
            })
        }

        pub(super) fn wake(&self) -> Wake {
            self.wake.clone()
        }

        pub(super) fn wait(&self, timeout: Option<Duration>) -> bool {
            debug_assert_eq!(
                unsafe { CFRunLoopGetCurrent() },
                self.wake.0.run_loop.as_concrete_TypeRef()
            );
            let pending = self.wake.0.pending.swap(false, Ordering::AcqRel);
            let timeout = if pending {
                Duration::ZERO
            } else {
                timeout.unwrap_or(IDLE_WAIT)
            };
            let result = CFRunLoop::run_in_mode(
                unsafe { kCFRunLoopDefaultMode },
                timeout,
                !timeout.is_zero(),
            );
            // The caller drains the payload queues after every wait, including
            // notifications sent while Core Foundation was running.
            self.wake.0.pending.swap(false, Ordering::AcqRel)
                || pending
                || matches!(result, CFRunLoopRunResult::HandledSource)
        }
    }

    impl Drop for Wait {
        fn drop(&mut self) {
            unsafe { CFRunLoopSourceInvalidate(self.wake.0.run_loop_source.as_concrete_TypeRef()) };
        }
    }

    impl Wake {
        pub(crate) fn notify(&self) {
            // Signal alone is not durable if a nested run loop already ran
            // the source before the host entered its own wait.
            self.0.pending.store(true, Ordering::Release);
            unsafe {
                CFRunLoopSourceSignal(self.0.run_loop_source.as_concrete_TypeRef());
                CFRunLoopWakeUp(self.0.run_loop.as_concrete_TypeRef());
            }
        }
    }
}

#[cfg(not(target_vendor = "apple"))]
mod platform {
    use std::{sync::mpsc, time::Duration};

    #[derive(Clone)]
    pub(crate) struct Wake(mpsc::SyncSender<()>);

    pub(super) struct Wait {
        wake: Wake,
        receiver: mpsc::Receiver<()>,
    }

    impl Wait {
        pub(super) fn new() -> Self {
            let (sender, receiver) = mpsc::sync_channel(1);
            Self {
                wake: Wake(sender),
                receiver,
            }
        }

        pub(super) fn wake(&self) -> Wake {
            self.wake.clone()
        }

        pub(super) fn wait(&self, timeout: Option<Duration>) -> bool {
            if let Some(timeout) = timeout {
                self.receiver.recv_timeout(timeout).is_ok()
            } else {
                self.receiver.recv().is_ok()
            }
        }
    }

    impl Wake {
        pub(crate) fn notify(&self) {
            let _ = self.0.try_send(());
        }
    }
}

pub(crate) use platform::Wake;

pub(crate) struct Waiter(platform::Wait);

impl Waiter {
    // The Core Foundation source is fallible. The signal channel used by
    // other platforms preserves the same API.
    #[cfg_attr(not(target_vendor = "apple"), allow(clippy::unnecessary_wraps))]
    pub(crate) fn new() -> std::io::Result<Self> {
        #[cfg(target_vendor = "apple")]
        let waiter = platform::Wait::new()?;
        #[cfg(not(target_vendor = "apple"))]
        let waiter = platform::Wait::new();
        Ok(Self(waiter))
    }

    pub(crate) fn wake(&self) -> Wake {
        self.0.wake()
    }

    pub(super) fn wait(&self, timeout: Option<Duration>) -> bool {
        self.0.wait(timeout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn notifications_are_latched_and_coalesced_before_waiting() {
        let waiter = Waiter::new().expect("host waiter");
        let sender = waiter.wake();
        sender.notify();
        sender.notify();
        assert!(waiter.wait(Some(Duration::from_secs(1))));
        assert!(!waiter.wait(Some(Duration::from_millis(1))));
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn source_serviced_before_wait_does_not_consume_pending_notification() {
        use core_foundation::runloop::{CFRunLoop, CFRunLoopRunResult, kCFRunLoopDefaultMode};

        let waiter = Waiter::new().expect("host waiter");
        waiter.wake().notify();
        let nested = CFRunLoop::run_in_mode(
            unsafe { kCFRunLoopDefaultMode },
            Duration::from_secs(1),
            true,
        );
        assert_eq!(nested, CFRunLoopRunResult::HandledSource);
        assert!(waiter.wait(Some(Duration::from_millis(1))));
        assert!(!waiter.wait(Some(Duration::from_millis(1))));
    }

    #[test]
    fn another_thread_can_wake_a_blocked_host() {
        let waiter = Waiter::new().expect("host waiter");
        let sender = waiter.wake();
        let producer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(10));
            sender.notify();
        });
        assert!(waiter.wait(Some(Duration::from_secs(1))));
        producer.join().expect("wake producer");
    }
}
