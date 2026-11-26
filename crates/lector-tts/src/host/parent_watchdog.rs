//! A native helper must exit with its owning Lector, even if a backend stalls.
use std::{io, thread, time::Duration};

use anyhow::{Context, Result};

pub(super) fn start(parent: u32) -> Result<()> {
    check_parent(parent);
    thread::Builder::new()
        .name("lector-speech-parent-watchdog".to_owned())
        .spawn(move || {
            if wait_for_exit(parent).is_err() {
                // Older kernels and restricted environments may deny process watches.
                loop {
                    check_parent(parent);
                    thread::sleep(Duration::from_millis(100));
                }
            }
            exit_helper();
        })
        .map(|_| ())
        .context("start native speech parent watchdog")
}

fn exit_helper() -> ! {
    // Native destructors can block indefinitely after the owner exits.
    unsafe { nix::libc::_exit(1) }
}

fn check_parent(expected: u32) {
    if nix::unistd::getppid().as_raw().cast_unsigned() != expected {
        exit_helper();
    }
}

#[cfg(target_vendor = "apple")]
fn wait_for_exit(parent: u32) -> io::Result<()> {
    use nix::sys::event::{EvFlags, EventFilter, FilterFlag, KEvent, Kqueue};
    let queue = Kqueue::new()?;
    let watch = KEvent::new(
        parent as usize,
        EventFilter::EVFILT_PROC,
        EvFlags::EV_ADD | EvFlags::EV_ONESHOT,
        FilterFlag::NOTE_EXIT,
        0,
        0,
    );
    queue.kevent(&[watch], &mut [], None)?;
    // Close the race between checking getppid and registering the process watch.
    check_parent(parent);
    let mut events = [watch];
    loop {
        match queue.kevent(&[], &mut events, None) {
            Ok(1) if events[0].fflags().contains(FilterFlag::NOTE_EXIT) => return Ok(()),
            Err(nix::errno::Errno::EINTR) | Ok(0) => {}
            Ok(_) => return Err(io::Error::other("unexpected parent watch event")),
            Err(err) => return Err(err.into()),
        }
    }
}

#[cfg(target_os = "linux")]
fn wait_for_exit(parent: u32) -> io::Result<()> {
    use rustix::{
        event::{PollFd, PollFlags, poll},
        io::Errno,
        process::{Pid, PidfdFlags, pidfd_open},
    };
    let pid = i32::try_from(parent)
        .ok()
        .and_then(Pid::from_raw)
        .ok_or_else(|| io::Error::other("invalid parent process ID"))?;
    let fd = pidfd_open(pid, PidfdFlags::empty())?;
    check_parent(parent);
    let mut events = [PollFd::new(&fd, PollFlags::IN)];
    loop {
        match poll(&mut events, None) {
            Ok(1)
                if events[0]
                    .revents()
                    .intersects(PollFlags::IN | PollFlags::HUP) =>
            {
                return Ok(());
            }
            Ok(0) | Err(Errno::INTR) => {}
            Ok(_) => return Err(io::Error::other("unexpected parent watch event")),
            Err(err) => return Err(err.into()),
        }
    }
}

#[cfg(not(any(target_vendor = "apple", target_os = "linux")))]
fn wait_for_exit(_: u32) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}
