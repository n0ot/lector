use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use lector::{app, lua, platform, screen_reader::ScreenReader, speech, views};
use nix::sys::termios;
use ptyprocess::{PtyProcess, Signal};
use signal_hook::consts::signal::*;
use signal_hook_mio::v1_0::Signals;
use std::{
    io::{ErrorKind, Read},
    os::fd::{AsFd, AsRawFd},
    process::Command,
    time,
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
    let view_stack = views::ViewStack::new(Box::new(views::PtyView::new(
        term_size.rows,
        term_size.cols,
    )));
    let mut app = app::App::new(view_stack)?;

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
        do_events(screen_reader, &mut app, &mut process, None)
    }) {
        Ok(()) => Ok(()),
        Err(err) => do_events(
            &mut screen_reader,
            &mut app,
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
    app: &mut app::App,
    process: &mut ptyprocess::PtyProcess,
    initial_message: Option<String>,
) -> Result<()> {
    let mut pty_stream = process.get_pty_stream().context("get PTY stream")?;
    // Set stdin to raw, so that input is read character by character,
    // and so that signals like SIGINT aren't send when pressing keys like ^C.
    ptyprocess::set_raw(0).context("set STDIN to raw")?;

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
    if let Some(message) = initial_message {
        app.show_message(sr, "Lector Error", &message, &mut stdout)?;
    }
    loop {
        poll_timeout = platform::adjust_poll_timeout(poll_timeout);
        if app.wants_tick() {
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
                    app.handle_stdin(sr, &buf[0..n], &mut pty_stream, &mut stdout)?;
                }
                PTY_TOKEN => {
                    let mut buf = [0; 8192];
                    let n = match pty_stream.read(&mut buf) {
                        Ok(n) if n == 0 => return Ok(()), // The child process exited
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
                        match signal {
                            SIGWINCH => {
                                let term_size = termsize::get()
                                    .ok_or_else(|| anyhow!("cannot get terminal size"))?;
                                process
                                    .set_window_size(term_size.cols, term_size.rows)
                                    .context("resize PTY")?;
                                process.signal(Signal::SIGWINCH)?;
                                app.on_resize(term_size.rows, term_size.cols, &mut stdout)?;
                            }
                            _ => unreachable!("unknown signal"),
                        }
                    }
                }
                _ => unreachable!("encountered unknown event"),
            }
        }

        app.handle_tick(sr, &mut pty_stream, &mut stdout)?;

        // We want to wait till the PTY has stopped sending us data for awhile before reading
        // updates, to give the screen time to stabilize.
        // But if we never stop getting updates, we want to read what we have eventually.
        if app.maybe_finalize_changes(sr)? {
            poll_timeout = None; // No need to wakeup until we get more updates.
        }

        platform::tick_runloop()?;
    }
}
