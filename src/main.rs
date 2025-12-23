use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use lector::{commands, lua, perform, platform, screen_reader::ScreenReader, speech, views};
use nix::sys::termios;
use phf::phf_map;
use ptyprocess::{PtyProcess, Signal};
use signal_hook::consts::signal::*;
use signal_hook_mio::v1_0::Signals;
use std::{
    io::{ErrorKind, Read, Write},
    os::fd::{AsFd, AsRawFd},
    process::Command,
    time,
};

const DIFF_DELAY: u16 = 1;
const MAX_DIFF_DELAY: u16 = 300;

static KEYMAP: phf::Map<&'static str, commands::Action> = phf_map! {
    "\x1BOP" => commands::Action::ToggleHelp,
    "\x1B'" => commands::Action::ToggleAutoRead,
    "\x1B\"" => commands::Action::ToggleReviewCursorFollowsScreenCursor,
    "\x1Bs" => commands::Action::ToggleSymbolLevel,
    "\x1Bn" => commands::Action::PassNextKey,
    "\x1Bx" => commands::Action::StopSpeaking,
    "\x1Bu" => commands::Action::RevLinePrev,
    "\x1Bo" => commands::Action::RevLineNext,
    "\x1BU" => commands::Action::RevLinePrevNonBlank,
    "\x1BO" => commands::Action::RevLineNextNonBlank,
    "\x1Bi" => commands::Action::RevLineRead,
    "\x1Bm" => commands::Action::RevCharPrev,
    "\x1B." => commands::Action::RevCharNext,
    "\x1B," => commands::Action::RevCharRead,
    "\x1B<" => commands::Action::RevCharReadPhonetic,
    "\x1Bj" => commands::Action::RevWordPrev,
    "\x1Bl" => commands::Action::RevWordNext,
    "\x1Bk" => commands::Action::RevWordRead,
    "\x1By" => commands::Action::RevTop,
    "\x1Bp" => commands::Action::RevBottom,
    "\x1Bh" => commands::Action::RevFirst,
    "\x1B;" => commands::Action::RevLast,
    "\x1Ba" => commands::Action::RevReadAttributes,
    "\x08" => commands::Action::Backspace,
    "\x7F" => commands::Action::Backspace,
    "\x1B[3~" => commands::Action::Delete,
    "\x1B[24~" => commands::Action::SayTime,
    "\x1BL" => commands::Action::OpenLuaRepl,
    "\x1B[15~" => commands::Action::SetMark,
    "\x1B[17~" => commands::Action::Copy,
    "\x1B[18~" => commands::Action::Paste,
    "\x1Bc" => commands::Action::SayClipboard,
    "\x1B[" => commands::Action::PreviousClipboard,
    "\x1B]" => commands::Action::NextClipboard,
};
#[derive(Parser)]
#[clap(author, version, about)]
struct Cli {
    /// Lector will spawn this shell when it starts
    #[clap(long, short = 's', env)]
    shell: std::path::PathBuf,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let term_size = termsize::get().ok_or_else(|| anyhow!("cannot get terminal size"))?;
    let speech_driver = Box::new(speech::tts::TtsDriver::new().context("create tts driver")?);
    let speech = speech::Speech::new(speech_driver);
    let mut screen_reader = ScreenReader::new(speech);
    let mut view_stack = views::ViewStack::new(Box::new(views::PtyView::new(
        term_size.rows,
        term_size.cols,
    )));

    let init_term_attrs = termios::tcgetattr(std::io::stdin().as_fd())?;
    // Spawn the child process, connect it to a PTY,
    // and set the PTY to match the current terminal attributes.
    let mut process = PtyProcess::spawn(Command::new(cli.shell)).context("spawn child process")?;
    process
        .set_window_size(term_size.cols, term_size.rows)
        .context("resize PTY")?;
    termios::tcsetattr(
        process.get_raw_handle()?,
        termios::SetArg::TCSADRAIN,
        &init_term_attrs,
    )?;

    let mut conf_dir = dirs::config_dir().ok_or_else(|| anyhow!("cannot get config directory"))?;
    conf_dir.push("lector");
    let mut conf_file = conf_dir.clone();
    conf_file.push("init.lua");

    let result = match lua::setup(conf_file.clone(), &mut screen_reader, |screen_reader| {
        do_events(screen_reader, &mut view_stack, &mut process, None)
    }) {
        Ok(()) => Ok(()),
        Err(err) => do_events(
            &mut screen_reader,
            &mut view_stack,
            &mut process,
            Some(format!(
                "Error loading config file: {}\n\n{}",
                conf_file.display(),
                err
            )),
        ),
    };
    // Clean up before returning the above result.
    termios::tcsetattr(
        std::io::stdin().as_fd(),
        termios::SetArg::TCSADRAIN,
        &init_term_attrs,
    )
    .unwrap();
    let _ = process.kill(ptyprocess::Signal::SIGKILL);
    let _ = process.wait();
    result.map_err(|e| anyhow!("{}", e))
}

fn do_events(
    sr: &mut ScreenReader,
    view_stack: &mut views::ViewStack,
    process: &mut ptyprocess::PtyProcess,
    initial_message: Option<String>,
) -> Result<()> {
    let mut pty_stream = process.get_pty_stream().context("get PTY stream")?;
    // Set stdin to raw, so that input is read character by character,
    // and so that signals like SIGINT aren't send when pressing keys like ^C.
    ptyprocess::set_raw(0).context("set STDIN to raw")?;

    // We also want to separately keep track of incoming bytes, for auto read.
    let mut vte_parser = vte::Parser::new();
    // Store new text to be read.
    let mut reporter = perform::Reporter::new();
    let ansi_csi_re =
        regex::bytes::Regex::new(r"^\x1B\[[\x30-\x3F]*[\x20-\x2F]*[\x40-\x7E--[A-D~]]$")
            .context("compile ansi csi regex")?;

    // Set up a mio poll, to select between reading from stdin, and the PTY.
    let mut signals = Signals::new([SIGWINCH])?;
    const STDIN_TOKEN: mio::Token = mio::Token(0);
    const PTY_TOKEN: mio::Token = mio::Token(1);
    const SIGNALS_TOKEN: mio::Token = mio::Token(2);
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
    let mut events = mio::Events::with_capacity(1024);
    let mut poll_timeout = None;
    let mut last_stdin_update = None;
    let mut last_pty_update = None;

    if let Some(message) = initial_message {
        let (rows, cols) = view_stack.root_mut().model().size();
        view_stack.push(Box::new(views::MessageView::new(
            rows,
            cols,
            "Lector Error",
            message,
        )));
        render_active_view(&mut stdout, view_stack)?;
        announce_view_change(sr, view_stack)?;
    }
    loop {
        poll_timeout = platform::adjust_poll_timeout(poll_timeout);
        if view_stack.active_mut().wants_tick() {
            poll_timeout = Some(time::Duration::from_millis(0));
        }
        poll.poll(&mut events, poll_timeout).or_else(|e| {
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
                        Ok(n) if n == 0 => return Ok(()),
                        Ok(n) => n,
                        Err(e) => bail!("error reading from input: {}", e),
                    };
                    if !ansi_csi_re.is_match(&buf[0..n]) {
                        sr.last_key = buf[0..n].to_owned();
                        sr.speech.stop()?;
                    }
                    if sr.pass_through {
                        sr.pass_through = false;
                        dispatch_to_view(
                            &buf[0..n],
                            sr,
                            view_stack,
                            &mut pty_stream,
                            &mut stdout,
                            &mut last_stdin_update,
                        )?;
                        continue;
                    }

                    let action = std::str::from_utf8(&buf[0..n])
                        .ok()
                        .and_then(|key| KEYMAP.get(key).copied());
                    if let Some(action) = action {
                        if matches!(action, commands::Action::OpenLuaRepl) {
                            if view_stack.active_mut().kind() == views::ViewKind::LuaRepl {
                                sr.speech.speak("Lua REPL already open", false)?;
                                continue;
                            }
                            let (rows, cols) = view_stack.active_mut().model().size();
                            let repl = views::LuaReplView::new(rows, cols)?;
                            handle_view_action(
                                sr,
                                views::ViewAction::Push(Box::new(repl)),
                                view_stack,
                                &mut stdout,
                                &mut last_stdin_update,
                            )?;
                            continue;
                        }
                        match commands::handle(sr, view_stack.active_mut().model(), action)? {
                            commands::CommandResult::Handled => {}
                            commands::CommandResult::ForwardInput => {
                                dispatch_to_view(
                                    &buf[0..n],
                                    sr,
                                    view_stack,
                                    &mut pty_stream,
                                    &mut stdout,
                                    &mut last_stdin_update,
                                )?;
                            }
                            commands::CommandResult::Paste(contents) => {
                                let view_action = view_stack
                                    .active_mut()
                                    .handle_paste(sr, &contents, &mut pty_stream)?;
                                handle_view_action(
                                    sr,
                                    view_action,
                                    view_stack,
                                    &mut stdout,
                                    &mut last_stdin_update,
                                )?;
                            }
                        }
                    } else if sr.help_mode {
                        sr.speech.speak("this key is unmapped", false)?;
                    } else {
                        dispatch_to_view(
                            &buf[0..n],
                            sr,
                            view_stack,
                            &mut pty_stream,
                            &mut stdout,
                            &mut last_stdin_update,
                        )?;
                    }
                }
                PTY_TOKEN => {
                    let mut buf = [0; 8192];
                    let n = match pty_stream.read(&mut buf) {
                        Ok(n) if n == 0 => return Ok(()), // The child process exited
                        Ok(n) => n,
                        Err(e) => bail!("error reading from PTY: {}", e),
                    };
                    let overlay_active = view_stack.has_overlay();
                    view_stack
                        .root_mut()
                        .handle_pty_output(&buf[0..n])?;
                    if !overlay_active {
                        stdout.write_all(&buf[0..n]).context("write PTY output")?;
                        stdout.flush().context("flush output")?;
                        if sr.auto_read {
                            vte_parser.advance(&mut reporter, &buf[0..n]);
                        }
                    }
                    // Stop blocking indefinitely until this screen is old enough to be
                    // auto read.
                    poll_timeout = Some(time::Duration::from_millis(DIFF_DELAY as u64));
                    last_pty_update = Some(time::Instant::now());
                }
                SIGNALS_TOKEN => {
                    for signal in signals.pending() {
                        match signal {
                            SIGWINCH => {
                                let term_size = termsize::get()
                                    .ok_or_else(|| anyhow!("cannot get terminal size"))?;
                                process
                                    .set_window_size(term_size.cols, term_size.rows)
                                    .context("resize PTY")?;
                                process.signal(Signal::SIGWINCH)?;
                                view_stack.on_resize(term_size.rows, term_size.cols);
                                if view_stack.has_overlay() {
                                    render_active_view(&mut stdout, view_stack)?;
                                }
                            }
                            _ => unreachable!("unknown signal"),
                        }
                    }
                }
                _ => unreachable!("encountered unknown event"),
            }
        }

        let tick_action = view_stack
            .active_mut()
            .tick(sr, &mut pty_stream)?;
        handle_view_action(
            sr,
            tick_action,
            view_stack,
            &mut stdout,
            &mut last_stdin_update,
        )?;

        // We want to wait till the PTY has stopped sending us data for awhile before reading
        // updates, to give the screen time to stabilize.
        // But if we never stop getting updates, we want to read what we have eventually.
        if let Some(lpu) = last_pty_update {
            let overlay_active = view_stack.has_overlay();
            let root_view = view_stack.root_mut();
            let view = root_view.model();
            if lpu.elapsed().as_millis() > DIFF_DELAY as u128
                || view.prev_screen_time.elapsed().as_millis() > MAX_DIFF_DELAY as u128
            {
                poll_timeout = None; // No need to wakeup until we get more updates.
                last_pty_update = None;
                if !overlay_active {
                    if sr.highlight_tracking {
                        sr.track_highlighting(view)?;
                    }
                    let read_text = if sr.auto_read {
                        sr.auto_read(view, &mut reporter)?
                    } else {
                        false
                    };
                    // Don't announce cursor changes if there are other textual changes being read,
                    // or the cursor is moving without user interaction.
                    // The latter makes disabling auto read truly be silent.
                    if let Some(lsu) = last_stdin_update {
                        if lsu.elapsed().as_millis() <= MAX_DIFF_DELAY as u128 && !read_text {
                            sr.track_cursor(view)?;
                        }
                    }
                }

                // Track screen cursor movements here, instead of every time the screen
                // updates,
                // to give the screen time to stabilize.
                if sr.review_follows_screen_cursor
                    && view.screen().cursor_position() != view.prev_screen().cursor_position()
                {
                    view.review_cursor_position = view.screen().cursor_position();
                }

                view.finalize_changes();
            }
        }

        platform::tick_runloop()?;
    }
}

fn render_active_view(
    stdout: &mut impl Write,
    view_stack: &mut views::ViewStack,
) -> Result<()> {
    let view = view_stack.active_mut().model();
    stdout
        .write_all(b"\x1B[2J\x1B[H")
        .context("clear screen")?;
    stdout
        .write_all(&view.screen().contents_formatted())
        .context("render view contents")?;
    stdout
        .write_all(&view.screen().cursor_state_formatted())
        .context("render cursor state")?;
    stdout
        .write_all(&view.screen().input_mode_formatted())
        .context("render input modes")?;
    stdout.flush().context("flush view render")?;
    Ok(())
}

fn dispatch_to_view(
    input: &[u8],
    sr: &mut ScreenReader,
    view_stack: &mut views::ViewStack,
    pty_stream: &mut ptyprocess::stream::Stream,
    stdout: &mut impl Write,
    last_stdin_update: &mut Option<time::Instant>,
) -> Result<()> {
    *last_stdin_update = Some(time::Instant::now());
    let action = view_stack
        .active_mut()
        .handle_input(sr, input, pty_stream)?;
    handle_view_action(sr, action, view_stack, stdout, last_stdin_update)
}

fn handle_view_action(
    sr: &mut ScreenReader,
    action: views::ViewAction,
    view_stack: &mut views::ViewStack,
    stdout: &mut impl Write,
    last_stdin_update: &mut Option<time::Instant>,
) -> Result<()> {
    match action {
        views::ViewAction::PtyInput => {
            *last_stdin_update = Some(time::Instant::now());
        }
        views::ViewAction::Bell => {
            stdout.write_all(b"\x07").context("write bell")?;
            stdout.flush().context("flush bell")?;
        }
        views::ViewAction::Push(view) => {
            view_stack.push(view);
            render_active_view(stdout, view_stack)?;
            announce_view_change(sr, view_stack)?;
        }
        views::ViewAction::Pop => {
            if view_stack.pop() {
                render_active_view(stdout, view_stack)?;
                announce_view_change(sr, view_stack)?;
            }
        }
        views::ViewAction::Redraw => {
            render_active_view(stdout, view_stack)?;
            read_active_view_changes(sr, view_stack, last_stdin_update)?;
        }
        views::ViewAction::None => {}
    }
    Ok(())
}

fn announce_view_change(sr: &mut ScreenReader, view_stack: &mut views::ViewStack) -> Result<()> {
    let title = view_stack.active_mut().title().to_string();
    let view = view_stack.active_mut().model();
    sr.speech.speak(&title, false)?;
    let contents = view.contents_full();
    if contents.trim().is_empty() {
        sr.speech.speak("blank screen", false)?;
    } else {
        sr.speech.speak(&contents, false)?;
    }
    view.finalize_changes();
    Ok(())
}

fn read_active_view_changes(
    sr: &mut ScreenReader,
    view_stack: &mut views::ViewStack,
    last_stdin_update: &mut Option<time::Instant>,
) -> Result<()> {
    let view = view_stack.active_mut().model();
    let read_text = if sr.auto_read {
        let mut reporter = perform::Reporter::new();
        sr.auto_read(view, &mut reporter)?
    } else {
        false
    };
    if let Some(lsu) = last_stdin_update {
        if lsu.elapsed().as_millis() <= MAX_DIFF_DELAY as u128 && !read_text {
            sr.track_cursor(view)?;
        }
    }
    if sr.review_follows_screen_cursor
        && view.screen().cursor_position() != view.prev_screen().cursor_position()
    {
        view.review_cursor_position = view.screen().cursor_position();
    }
    view.finalize_changes();
    Ok(())
}
