use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, ValueEnum};
use lector::{
    app, lua, platform,
    presentation::PhysicalTerminalLifecycle,
    pty,
    screen_reader::ScreenReader,
    speech,
    terminal_protocol::{CapabilityOverrides, PhysicalTerminalProfile, TerminfoCapabilities},
    views,
};
use nix::fcntl::{FcntlArg, OFlag, fcntl};
use nix::sys::termios;
use signal_hook::consts::signal::*;
use signal_hook_mio::v1_0::Signals;
use std::{
    io::{ErrorKind, Read, Write},
    os::fd::{AsFd, AsRawFd, RawFd},
    time,
};

const FOCUS_EVENTS_QUERY: &[u8] = b"\x1B[?1004$p";

struct NonblockingFdGuard {
    fd: RawFd,
    original: OFlag,
    restored: bool,
}

impl NonblockingFdGuard {
    fn enable(fd: RawFd) -> Result<Self> {
        let original = OFlag::from_bits_truncate(
            fcntl(fd, FcntlArg::F_GETFL).context("read terminal output flags")?,
        );
        fcntl(fd, FcntlArg::F_SETFL(original | OFlag::O_NONBLOCK))
            .context("make terminal output nonblocking")?;
        Ok(Self {
            fd,
            original,
            restored: false,
        })
    }

    fn restore(&mut self) -> Result<()> {
        if self.restored {
            return Ok(());
        }
        fcntl(self.fd, FcntlArg::F_SETFL(self.original))
            .context("restore terminal output flags")?;
        self.restored = true;
        Ok(())
    }

    fn enable_again(&mut self) -> Result<()> {
        if !self.restored {
            return Ok(());
        }
        fcntl(
            self.fd,
            FcntlArg::F_SETFL(self.original | OFlag::O_NONBLOCK),
        )
        .context("make terminal output nonblocking after resume")?;
        self.restored = false;
        Ok(())
    }
}

impl Drop for NonblockingFdGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

struct EmergencyTerminalGuard {
    initial_term_attrs: termios::Termios,
    focus_was_enabled: Option<bool>,
    armed: bool,
}

impl EmergencyTerminalGuard {
    fn new(initial_term_attrs: &termios::Termios) -> Self {
        Self {
            initial_term_attrs: initial_term_attrs.clone(),
            focus_was_enabled: None,
            armed: false,
        }
    }

    fn arm(&mut self) {
        self.armed = true;
    }

    fn set_prior_focus_mode(&mut self, focus_was_enabled: Option<bool>) {
        self.focus_was_enabled = focus_was_enabled;
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for EmergencyTerminalGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut stdout = std::io::stdout().lock();
        let _ = stdout.write_all(&emergency_terminal_cleanup_bytes(self.focus_was_enabled));
        let _ = stdout.flush();
        let _ = termios::tcsetattr(
            std::io::stdin().as_fd(),
            termios::SetArg::TCSADRAIN,
            &self.initial_term_attrs,
        );
    }
}

fn emergency_terminal_cleanup_bytes(focus_was_enabled: Option<bool>) -> Vec<u8> {
    let mut lifecycle = PhysicalTerminalLifecycle::new(focus_was_enabled);
    let _ = lifecycle.activate();
    lifecycle.shutdown().bytes
}

fn query_focus_mode<R: Read + AsRawFd>(stdin: &mut R, stdout: &mut dyn Write) -> Option<bool> {
    if stdout.write_all(FOCUS_EVENTS_QUERY).is_err() || stdout.flush().is_err() {
        return None;
    }

    let mut poll = mio::Poll::new().ok()?;
    let stdin_fd = stdin.as_raw_fd();
    let mut source = mio::unix::SourceFd(&stdin_fd);
    poll.registry()
        .register(&mut source, mio::Token(0), mio::Interest::READABLE)
        .ok()?;

    let mut events = mio::Events::with_capacity(8);
    let mut response = Vec::new();
    for timeout_ms in [20, 10, 10] {
        poll.poll(&mut events, Some(time::Duration::from_millis(timeout_ms)))
            .ok()?;
        if events.is_empty() {
            continue;
        }
        let mut buf = [0u8; 256];
        match stdin.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                response.extend_from_slice(&buf[..n]);
                if let Some(enabled) = parse_focus_mode_report(&response) {
                    return Some(enabled);
                }
            }
            Err(err) if err.kind() == ErrorKind::WouldBlock => continue,
            Err(_) => return None,
        }
    }
    parse_focus_mode_report(&response)
}

fn parse_focus_mode_report(buf: &[u8]) -> Option<bool> {
    let prefix = b"\x1B[?1004;";
    let mut i = 0usize;
    while i < buf.len() {
        if !buf[i..].starts_with(prefix) {
            i += 1;
            continue;
        }
        let start = i + prefix.len();
        let mut end = start;
        while end < buf.len() && buf[end].is_ascii_digit() {
            end += 1;
        }
        if end <= start || end + 1 >= buf.len() || buf[end] != b'$' || buf[end + 1] != b'y' {
            i += 1;
            continue;
        }
        let code = std::str::from_utf8(&buf[start..end])
            .ok()?
            .parse::<u8>()
            .ok()?;
        return match code {
            1 | 3 => Some(true),
            2 | 4 => Some(false),
            _ => None,
        };
    }
    None
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TerminalSignalAction {
    Resize,
    Suspend,
    Resume,
    Shutdown(i32),
}

fn terminal_signal_action(signal: i32) -> Option<TerminalSignalAction> {
    match signal {
        SIGWINCH => Some(TerminalSignalAction::Resize),
        SIGTSTP => Some(TerminalSignalAction::Suspend),
        SIGCONT => Some(TerminalSignalAction::Resume),
        SIGHUP | SIGINT | SIGQUIT | SIGTERM => Some(TerminalSignalAction::Shutdown(signal)),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SetupFailureAction {
    ShowConfigurationError,
    ReturnRuntimeError,
}

const fn setup_failure_action(event_loop_started: bool) -> SetupFailureAction {
    if event_loop_started {
        SetupFailureAction::ReturnRuntimeError
    } else {
        SetupFailureAction::ShowConfigurationError
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Cli, NonblockingFdGuard, SetupFailureAction, TerminalSignalAction,
        emergency_terminal_cleanup_bytes, parse_focus_mode_report, setup_failure_action,
        terminal_signal_action,
    };
    use clap::{CommandFactory, Parser};
    use nix::fcntl::{FcntlArg, OFlag, fcntl};
    use signal_hook::consts::signal::*;
    use std::os::fd::AsRawFd;

    #[test]
    fn compositor_is_mandatory_and_has_no_runtime_mode_flag() {
        Cli::try_parse_from(["lector", "--shell", "/bin/sh"]).expect("parse default CLI");
        assert!(Cli::try_parse_from(["lector", "--shell", "/bin/sh", "--full-renderer"]).is_err());
        let help = Cli::command().render_long_help().to_string();
        assert!(!help.contains("full-renderer"), "{help}");
        assert!(!help.contains("LECTOR_FULL_RENDERER"), "{help}");
    }

    #[test]
    fn parse_focus_mode_report_enabled_code_1() {
        assert_eq!(parse_focus_mode_report(b"\x1B[?1004;1$y"), Some(true));
    }

    #[test]
    fn parse_focus_mode_report_enabled_code_3() {
        assert_eq!(parse_focus_mode_report(b"xx\x1B[?1004;3$yyy"), Some(true));
    }

    #[test]
    fn parse_focus_mode_report_disabled_code_2() {
        assert_eq!(parse_focus_mode_report(b"\x1B[?1004;2$y"), Some(false));
    }

    #[test]
    fn parse_focus_mode_report_disabled_code_4() {
        assert_eq!(parse_focus_mode_report(b"\x1B[?1004;4$y"), Some(false));
    }

    #[test]
    fn parse_focus_mode_report_none_for_invalid_or_missing() {
        assert_eq!(parse_focus_mode_report(b""), None);
        assert_eq!(parse_focus_mode_report(b"\x1B[?1004$p"), None);
        assert_eq!(parse_focus_mode_report(b"\x1B[?1004;9$y"), None);
    }

    #[test]
    fn nonblocking_output_guard_sets_and_restores_the_original_descriptor_flags() {
        let (_read, write) = nix::unistd::pipe().expect("create pipe");
        let original = OFlag::from_bits_truncate(
            fcntl(write.as_raw_fd(), FcntlArg::F_GETFL).expect("read original flags"),
        );
        {
            let _guard =
                NonblockingFdGuard::enable(write.as_raw_fd()).expect("enable nonblocking output");
            let active = OFlag::from_bits_truncate(
                fcntl(write.as_raw_fd(), FcntlArg::F_GETFL).expect("read active flags"),
            );
            assert!(active.contains(OFlag::O_NONBLOCK));
        }
        let restored = OFlag::from_bits_truncate(
            fcntl(write.as_raw_fd(), FcntlArg::F_GETFL).expect("read restored flags"),
        );
        assert_eq!(restored, original);
    }

    #[test]
    fn nonblocking_output_guard_can_be_reenabled_after_a_suspend_boundary() {
        let (_read, write) = nix::unistd::pipe().expect("create pipe");
        let original = OFlag::from_bits_truncate(
            fcntl(write.as_raw_fd(), FcntlArg::F_GETFL).expect("read original flags"),
        );
        let mut guard =
            NonblockingFdGuard::enable(write.as_raw_fd()).expect("enable nonblocking output");
        guard.restore().expect("restore for suspend");
        assert_eq!(
            OFlag::from_bits_truncate(
                fcntl(write.as_raw_fd(), FcntlArg::F_GETFL).expect("read suspended flags")
            ),
            original
        );

        guard.enable_again().expect("reenable after resume");
        let resumed = OFlag::from_bits_truncate(
            fcntl(write.as_raw_fd(), FcntlArg::F_GETFL).expect("read resumed flags"),
        );
        assert!(resumed.contains(OFlag::O_NONBLOCK));

        guard.restore().expect("restore after resumed run");
        assert_eq!(
            OFlag::from_bits_truncate(
                fcntl(write.as_raw_fd(), FcntlArg::F_GETFL).expect("read final flags")
            ),
            original
        );
    }

    #[test]
    fn terminal_signals_have_explicit_lifecycle_actions() {
        assert_eq!(
            terminal_signal_action(SIGWINCH),
            Some(TerminalSignalAction::Resize)
        );
        assert_eq!(
            terminal_signal_action(SIGTSTP),
            Some(TerminalSignalAction::Suspend)
        );
        assert_eq!(
            terminal_signal_action(SIGCONT),
            Some(TerminalSignalAction::Resume)
        );
        for signal in [SIGHUP, SIGINT, SIGQUIT, SIGTERM] {
            assert_eq!(
                terminal_signal_action(signal),
                Some(TerminalSignalAction::Shutdown(signal))
            );
        }
        assert_eq!(terminal_signal_action(SIGUSR1), None);
    }

    #[test]
    fn panic_fallback_cleanup_resets_every_owned_terminal_mode() {
        let cleanup = emergency_terminal_cleanup_bytes(None);
        for reset in [
            b"\x1b[?2026l".as_slice(),
            b"\x1b[0m",
            b"\x1b]8;;\x1b\\",
            b"\x1b>",
            b"\x1b[?1l",
            b"\x1b[?2004l",
            b"\x1b[?1000l",
            b"\x1b[?1002l",
            b"\x1b[?1003l",
            b"\x1b[?1005l",
            b"\x1b[?1006l",
            b"\x1b[=0u",
            b"\x1b[?25h",
            b"\x1b[?1004l",
        ] {
            assert!(
                cleanup.windows(reset.len()).any(|window| window == reset),
                "missing emergency reset {reset:?}"
            );
        }
        assert!(cleanup.ends_with(b"\x1b[?1004l"));

        let cleanup_preserving_focus = emergency_terminal_cleanup_bytes(Some(true));
        assert!(
            !cleanup_preserving_focus
                .windows(b"\x1b[?1004l".len())
                .any(|window| window == b"\x1b[?1004l")
        );
        assert!(cleanup_preserving_focus.ends_with(b"\x1b[?25h"));
    }

    #[test]
    fn only_configuration_failures_start_the_error_overlay_event_loop() {
        assert_eq!(
            setup_failure_action(false),
            SetupFailureAction::ShowConfigurationError
        );
        assert_eq!(
            setup_failure_action(true),
            SetupFailureAction::ReturnRuntimeError
        );
    }
}

#[derive(Parser)]
#[clap(author, version, about)]
struct Cli {
    /// Lector will spawn this shell when it starts
    #[clap(long, short = 's', env)]
    shell: std::path::PathBuf,
    /// Speech driver backend
    #[clap(long, value_enum, default_value = "tts", env)]
    speech_driver: SpeechDriverKind,
    /// Path to the proc driver server (required when --speech-driver=proc)
    #[clap(long, env)]
    speech_server: Option<std::path::PathBuf>,
    /// Enable debug logging
    #[clap(long, env)]
    log: bool,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum SpeechDriverKind {
    Tts,
    Proc,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let terminal_geometry =
        pty::terminal_geometry(std::io::stdin().as_raw_fd()).context("cannot get terminal size")?;

    // Keep PTY creation ahead of anything that may start threads. The macOS
    // AVFoundation speech backend owns its synthesizer on a dedicated thread,
    // and Unix PTY launchers still require a small post-fork setup window.
    let init_term_attrs =
        termios::tcgetattr(std::io::stdin().as_fd()).context("read terminal settings")?;
    let mut terminfo_root = dirs::cache_dir().unwrap_or_else(std::env::temp_dir);
    terminfo_root.push("lector");
    terminfo_root.push("terminfo");
    let virtual_environment =
        pty::install_lector_terminfo(&terminfo_root).context("install bundled terminfo")?;
    let mut process = pty::Process::spawn_with_geometry_and_environment(
        &cli.shell,
        std::iter::empty::<&str>(),
        terminal_geometry,
        &init_term_attrs,
        Some(&virtual_environment),
    )
    .context("spawn child process")?;

    let mut physical_profile = PhysicalTerminalProfile::conservative(terminal_geometry);
    if let Some(term) = std::env::var_os("TERM")
        && let Some(terminfo) = TerminfoCapabilities::detect(&term)
    {
        physical_profile.apply_terminfo(&terminfo);
    }
    if std::env::var("COLORTERM")
        .ok()
        .is_some_and(|value| matches!(value.to_ascii_lowercase().as_str(), "truecolor" | "24bit"))
    {
        physical_profile.true_color = true;
    }
    let overrides = CapabilityOverrides::from_environment().map_err(anyhow::Error::msg)?;
    physical_profile.apply_overrides(&overrides);

    let speech_driver: Box<dyn speech::Driver> = match cli.speech_driver {
        SpeechDriverKind::Tts => {
            Box::new(speech::tts::TtsDriver::new().context("create tts driver")?)
        }
        SpeechDriverKind::Proc => {
            let path = cli
                .speech_server
                .ok_or_else(|| anyhow!("--speech-server is required when --speech-driver=proc"))?;
            Box::new(speech::proc_driver::ProcDriver::new(&path).context("create proc driver")?)
        }
    };
    let speech = speech::Speech::new(speech_driver);
    let mut screen_reader = ScreenReader::new(speech);
    let view_stack = views::ViewStack::new(Box::new(views::PtyView::new_with_geometry(
        terminal_geometry,
    )));
    let mut app = app::App::new(view_stack)?;
    app.set_physical_profile(physical_profile);
    app.set_logging(cli.log);
    app.enable_output_scheduler(Default::default());

    let mut conf_dir = dirs::config_dir().ok_or_else(|| anyhow!("cannot get config directory"))?;
    conf_dir.push("lector");
    let mut conf_file = conf_dir.clone();
    conf_file.push("init.lua");

    let mut event_loop_started = false;
    let setup_result = lua::setup(conf_file.clone(), &mut screen_reader, |screen_reader| {
        event_loop_started = true;
        do_events(
            screen_reader,
            &mut app,
            &mut process,
            None,
            &init_term_attrs,
        )
    });
    let result = match setup_result {
        Ok(()) => Ok(()),
        Err(err)
            if setup_failure_action(event_loop_started)
                == SetupFailureAction::ShowConfigurationError =>
        {
            do_events(
                &mut screen_reader,
                &mut app,
                &mut process,
                Some(format!(
                    "Error loading config file: {}\n\n{}",
                    conf_file.display(),
                    err
                )),
                &init_term_attrs,
            )
        }
        Err(err) => Err(anyhow!("{err}")),
    };
    // Clean up before returning the above result.
    if let Err(err) = termios::tcsetattr(
        std::io::stdin().as_fd(),
        termios::SetArg::TCSADRAIN,
        &init_term_attrs,
    ) {
        eprintln!("failed to restore terminal settings: {err}");
    }
    process.terminate();
    result.map_err(|e| anyhow!("{}", e))
}

fn do_events(
    sr: &mut ScreenReader,
    app: &mut app::App,
    process: &mut pty::Process,
    initial_message: Option<String>,
    initial_term_attrs: &termios::Termios,
) -> Result<()> {
    // This fallback is deliberately declared before the nonblocking-output
    // guard below. Rust drops locals in reverse order, so an unwind restores
    // the descriptor flags before this writes its final terminal reset.
    let mut emergency_terminal = EmergencyTerminalGuard::new(initial_term_attrs);
    let mut pty_stream = process.stream().context("get PTY stream")?;
    // Set stdin to raw, so that input is read character by character,
    // and so that signals like SIGINT aren't send when pressing keys like ^C.
    pty::set_raw(std::io::stdin().as_raw_fd()).context("set STDIN to raw")?;
    emergency_terminal.arm();

    // Set up a mio poll, to select between reading from stdin, and the PTY.
    let mut signals = Signals::new([SIGWINCH, SIGTSTP, SIGCONT, SIGHUP, SIGINT, SIGQUIT, SIGTERM])?;
    const STDIN_TOKEN: mio::Token = mio::Token(0);
    const PTY_TOKEN: mio::Token = mio::Token(1);
    const SIGNALS_TOKEN: mio::Token = mio::Token(2);
    const STDOUT_TOKEN: mio::Token = mio::Token(3);
    let mut poll = mio::Poll::new()?;
    poll.registry().register(
        &mut mio::unix::SourceFd(&std::io::stdin().as_raw_fd()),
        STDIN_TOKEN,
        mio::Interest::READABLE,
    )?;
    poll.registry().register(
        &mut mio::unix::SourceFd(&pty_stream.as_raw_fd()),
        PTY_TOKEN,
        mio::Interest::READABLE,
    )?;
    poll.registry()
        .register(&mut signals, SIGNALS_TOKEN, mio::Interest::READABLE)?;

    // Main event loop
    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    let focus_was_enabled = query_focus_mode(&mut stdin, &mut stdout);
    emergency_terminal.set_prior_focus_mode(focus_was_enabled);
    let stdout_fd = stdout.as_raw_fd();
    let mut nonblocking_output = NonblockingFdGuard::enable(stdout_fd)?;
    let mut stdout_source = mio::unix::SourceFd(&stdout_fd);
    let mut stdout_registered = false;
    let mut events = mio::Events::with_capacity(1024);
    let mut poll_timeout = None;
    app.configure_physical_terminal(focus_was_enabled);
    app.activate_physical_terminal(&mut stdout)?;
    let mut termination_signal = None;

    let event_result = (|| -> Result<()> {
        app.start_capability_probes(&mut stdout)?;
        let startup_output = app.drain_scheduled_output(&mut stdout, true)?;
        if startup_output.blocked {
            poll.registry()
                .register(&mut stdout_source, STDOUT_TOKEN, mio::Interest::WRITABLE)?;
            stdout_registered = true;
        }
        if let Some(message) = initial_message {
            app.show_message(sr, "Lector Error", &message, &mut stdout)?;
        }
        loop {
            let mut effective_poll_timeout = platform::adjust_poll_timeout(poll_timeout);
            if let Some(output_timeout) = app.scheduled_output_timeout() {
                effective_poll_timeout = Some(
                    effective_poll_timeout
                        .map_or(output_timeout, |current| current.min(output_timeout)),
                );
            }
            if app.wants_tick() {
                effective_poll_timeout = Some(time::Duration::from_millis(0));
            }
            poll.poll(&mut events, effective_poll_timeout)
                .or_else(|e| {
                    if e.kind() == ErrorKind::Interrupted {
                        events.clear();
                        Ok(())
                    } else {
                        Err(e)
                    }
                })?;

            for event in events.iter() {
                match event.token() {
                    STDIN_TOKEN => {
                        let mut buf = [0; 8192];
                        let n = match stdin.read(&mut buf) {
                            Ok(0) => return Ok(()),
                            Ok(n) => n,
                            Err(e) => bail!("error reading from input: {}", e),
                        };
                        app.handle_stdin(sr, &buf[0..n], &mut pty_stream, &mut stdout)?;
                    }
                    PTY_TOKEN => {
                        let mut buf = [0; 8192];
                        let n = match pty_stream.read(&mut buf) {
                            Ok(0) => return Ok(()), // The child process exited
                            Ok(n) => n,
                            Err(e) => bail!("error reading from PTY: {}", e),
                        };
                        app.handle_pty(sr, &buf[0..n], &mut stdout)?;
                        // Stop blocking indefinitely until this screen is old enough to be
                        // auto read.
                        poll_timeout = Some(time::Duration::from_millis(app::DIFF_DELAY as u64));
                    }
                    SIGNALS_TOKEN => {
                        for signal in signals.pending() {
                            match terminal_signal_action(signal) {
                                Some(TerminalSignalAction::Resize) => {
                                    let geometry =
                                        pty::terminal_geometry(std::io::stdin().as_raw_fd())?;
                                    process.resize_with_geometry(geometry)?;
                                    app.on_resize_with_geometry(geometry, &mut stdout)?;
                                }
                                Some(TerminalSignalAction::Suspend) => {
                                    app.suspend_physical_terminal(&mut stdout)?;
                                    nonblocking_output.restore()?;
                                    drain_scheduled_output_to_boundary(app, &mut stdout)?;
                                    termios::tcsetattr(
                                        std::io::stdin().as_fd(),
                                        termios::SetArg::TCSADRAIN,
                                        initial_term_attrs,
                                    )
                                    .context("restore terminal settings before suspend")?;
                                    signal_hook::low_level::emulate_default_handler(SIGTSTP)
                                        .context("suspend lector")?;

                                    // SIGCONT is necessarily what lets execution return here.
                                    // Reclaim immediately so no input or output is processed
                                    // while the terminal remains in its restored state. The
                                    // queued SIGCONT action below is deliberately idempotent.
                                    pty::set_raw(std::io::stdin().as_raw_fd())
                                        .context("set STDIN to raw after resume")?;
                                    nonblocking_output.enable_again()?;
                                    app.resume_physical_terminal(&mut stdout)?;
                                }
                                Some(TerminalSignalAction::Resume) => {
                                    pty::set_raw(std::io::stdin().as_raw_fd())
                                        .context("set STDIN to raw after SIGCONT")?;
                                    nonblocking_output.enable_again()?;
                                    app.resume_physical_terminal(&mut stdout)?;
                                }
                                Some(TerminalSignalAction::Shutdown(signal)) => {
                                    termination_signal = Some(signal);
                                    return Ok(());
                                }
                                None => {}
                            }
                        }
                    }
                    STDOUT_TOKEN => app.notify_scheduled_output_writable(),
                    _ => unreachable!("encountered unknown event"),
                }
            }

            app.handle_tick(sr, &mut pty_stream, &mut stdout)?;
            let output = app.drain_scheduled_output(&mut stdout, false)?;
            if output.blocked && !stdout_registered {
                poll.registry().register(
                    &mut stdout_source,
                    STDOUT_TOKEN,
                    mio::Interest::WRITABLE,
                )?;
                stdout_registered = true;
            } else if !output.blocked && stdout_registered {
                poll.registry().deregister(&mut stdout_source)?;
                stdout_registered = false;
            }

            // We want to wait till the PTY has stopped sending us data for awhile before reading
            // updates, to give the screen time to stabilize.
            // But if we never stop getting updates, we want to read what we have eventually.
            if app.maybe_finalize_changes(sr)? {
                poll_timeout = None; // No need to wakeup until we get more updates.
            }

            platform::tick_runloop();
        }
    })();

    let restore_result = nonblocking_output.restore();
    let cleanup_result = (|| -> Result<()> {
        app.shutdown_physical_terminal(&mut stdout)?;
        drain_scheduled_output_to_boundary(app, &mut stdout)?;
        termios::tcsetattr(
            std::io::stdin().as_fd(),
            termios::SetArg::TCSADRAIN,
            initial_term_attrs,
        )
        .context("restore terminal settings after event loop")?;
        Ok(())
    })();

    if let Err(error) = event_result {
        if let Err(restore_error) = &restore_result {
            eprintln!("failed to restore terminal output flags: {restore_error:#}");
        }
        if let Err(cleanup_error) = &cleanup_result {
            eprintln!("failed to clean up physical terminal: {cleanup_error:#}");
        }
        if restore_result.is_ok() && cleanup_result.is_ok() {
            emergency_terminal.disarm();
        }
        return Err(error);
    }
    restore_result?;
    cleanup_result?;
    emergency_terminal.disarm();
    if let Some(signal) = termination_signal {
        signal_hook::low_level::emulate_default_handler(signal)
            .with_context(|| format!("terminate lector with signal {signal}"))?;
    }
    Ok(())
}

fn drain_scheduled_output_to_boundary(app: &mut app::App, term_out: &mut dyn Write) -> Result<()> {
    loop {
        let report = app.drain_scheduled_output(term_out, true)?;
        if !report.blocked && !report.write_budget_exhausted {
            return Ok(());
        }
        app.notify_scheduled_output_writable();
    }
}
