use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use lector::{commands, lua, perform, screen_reader::ScreenReader, speech, view::View};
use nix::sys::termios;
use phf::phf_map;
use ptyprocess::PtyProcess;
use signal_hook::consts::signal::*;
use signal_hook_mio::v0_8::Signals;
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
    /// Path to the speech program
    #[clap(long, short = 'p')]
    speech_program: String,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let term_size = termsize::get().ok_or_else(|| anyhow!("cannot get terminal size"))?;
    let speech_driver =
        Box::new(speech::tdsr::Tdsr::new(cli.speech_program).context("create tdsr driver")?);
    let speech = speech::Speech::new(speech_driver);
    let view = View::new(term_size.rows, term_size.cols);
    let mut screen_reader = ScreenReader::new(speech, view);

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

    let result = lua::setup(conf_file, &mut screen_reader, |screen_reader| {
        do_events(screen_reader, &mut process)
    });
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

fn do_events(sr: &mut ScreenReader, process: &mut ptyprocess::PtyProcess) -> Result<()> {
    let mut pty_stream = process.get_pty_stream().context("get PTY stream")?;
    // Set stdin to raw, so that input is read character by character,
    // and so that signals like SIGINT aren't send when pressing keys like ^C.
    ptyprocess::set_raw(0).context("set STDIN to raw")?;

    // We also want to separately keep track of incoming bytes, for auto read.
    let mut vte_parser = vte::Parser::new();
    // Store new text to be read.
    let mut reporter = perform::Reporter::new();
    let ansi_csi_re =
        regex::bytes::Regex::new(r"^\x1B\[[\x30-\x3F]*[\x20-\x2F]*[\x40-\x7E--[A-D~]]$").unwrap();

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
    loop {
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
                    // Don't silence speech or set the last key for key echo,
                    // when receiving a CSI dispatch.
                    if !ansi_csi_re.is_match(&buf[0..n]) {
                        sr.last_key = buf[0..n].to_owned();
                        sr.speech.stop()?;
                    }
                    let pass_through = match sr.pass_through {
                        false => match KEYMAP.get(std::str::from_utf8(&buf[0..n])?) {
                            Some(&v) => commands::handle(sr, &mut pty_stream, v)?,
                            None => {
                                if sr.help_mode {
                                    sr.speech.speak("this key is unmapped", false)?;
                                    false
                                } else {
                                    true
                                }
                            }
                        },
                        true => {
                            // Turning pass through on should only apply for one keystroke.
                            sr.pass_through = false;
                            true
                        }
                    };
                    if pass_through {
                        last_stdin_update = Some(time::Instant::now());
                        pty_stream
                            .write_all(&buf[0..n])
                            .context("copy STDIN to PTY")?;
                        pty_stream.flush().context("flush write to PTY")?;
                    }
                }
                PTY_TOKEN => {
                    let mut buf = [0; 8192];
                    let n = match pty_stream.read(&mut buf) {
                        Ok(n) if n == 0 => return Ok(()), // The child process exited
                        Ok(n) => n,
                        Err(e) => bail!("error reading from PTY: {}", e),
                    };
                    stdout.write_all(&buf[0..n]).context("write PTY output")?;
                    stdout.flush().context("flush output")?;
                    if sr.auto_read {
                        for b in &buf[0..n] {
                            vte_parser.advance(&mut reporter, *b);
                        }
                    }

                    sr.view.process_changes(&buf[0..n]);
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
                                sr.view.set_size(term_size.rows, term_size.cols);
                            }
                            _ => unreachable!("unknown signal"),
                        }
                    }
                }
                _ => unreachable!("encountered unknown event"),
            }
        }

        // We want to wait till the PTY has stopped sending us data for awhile before reading
        // updates, to give the screen time to stabilize.
        // But if we never stop getting updates, we want to read what we have eventually.
        if let Some(lpu) = last_pty_update {
            if lpu.elapsed().as_millis() > DIFF_DELAY as u128
                || sr.view.prev_screen_time.elapsed().as_millis() > MAX_DIFF_DELAY as u128
            {
                poll_timeout = None; // No need to wakeup until we get more updates.
                last_pty_update = None;
                if sr.highlight_tracking {
                    sr.track_highlighting()?;
                }
                let read_text = if sr.auto_read {
                    sr.auto_read(&mut reporter)?
                } else {
                    false
                };
                // Don't announce cursor changes if there are other textual changes being read,
                // or the cursor is moving without user interaction.
                // The latter makes disabling auto read truly be silent.
                if let Some(lsu) = last_stdin_update {
                    if lsu.elapsed().as_millis() <= MAX_DIFF_DELAY as u128 && !read_text {
                        sr.track_cursor()?;
                    }
                }

                // Track screen cursor movements here, instead of every time the screen
                // updates,
                // to give the screen time to stabilize.
                if sr.review_follows_screen_cursor
                    && sr.view.screen().cursor_position() != sr.view.prev_screen().cursor_position()
                {
                    sr.view.review_cursor_position = sr.view.screen().cursor_position();
                }

                sr.view.finalize_changes();
            }
        }
    }
}
