//! First-party PTY child used by the macOS end-to-end tests.
//!
//! Each mode emits the exact bytes needed by a test without introducing an
//! external host dependency.

use anyhow::{Context, Result, bail};
use nix::{
    sys::{
        signal::{Signal, kill},
        termios,
    },
    unistd::getppid,
};
use std::{
    fs::OpenOptions,
    io::{self, Read, Write},
    os::{fd::AsFd, unix::process::CommandExt},
    path::Path,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

const MODE_ENV: &str = "LECTOR_TEST_PTY_MODE";

fn main() -> Result<()> {
    match std::env::var(MODE_ENV).as_deref() {
        Ok("bell") => bell(),
        Ok("flood") => flood(),
        Ok("interactive") => interactive(),
        Ok("latency") => latency(),
        Ok("nested-lector") => nested_lector(),
        Ok("parent") => parent(),
        Ok("prompt") => prompt(),
        Ok(mode) => bail!("unknown PTY test fixture mode {mode:?}"),
        Err(_) => bail!("{MODE_ENV} is required"),
    }
}

fn make_stdin_raw() -> Result<()> {
    let stdin = io::stdin();
    let mut attributes = termios::tcgetattr(stdin.as_fd()).context("read stdin termios")?;
    termios::cfmakeraw(&mut attributes);
    termios::tcsetattr(stdin.as_fd(), termios::SetArg::TCSANOW, &attributes)
        .context("set stdin raw")
}

fn write_bytes(bytes: &[u8]) -> Result<()> {
    let mut output = io::stdout().lock();
    output.write_all(bytes)?;
    output.flush()?;
    Ok(())
}

fn read_byte() -> Result<Option<u8>> {
    let mut byte = [0_u8; 1];
    match io::stdin().lock().read(&mut byte)? {
        0 => Ok(None),
        _ => Ok(Some(byte[0])),
    }
}

fn bell() -> Result<()> {
    make_stdin_raw()?;
    let has_private_terminfo = std::env::var("TERM").as_deref() == Ok("xterm-256color")
        && std::env::var_os("TERMINFO").is_some_and(|path| Path::new(&path).is_dir());
    let capability = if has_private_terminfo {
        "sync"
    } else {
        "missing-sync"
    };
    write_bytes(
        format!(
            "LECTOR-BELL-READY|ENV:{}:{capability}",
            std::env::var("TERM").unwrap_or_default()
        )
        .as_bytes(),
    )?;

    if std::env::var("LECTOR_TEST_PRELOAD_OUTPUT").as_deref() == Ok("1") {
        let parent = getppid();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            let _ = kill(parent, Signal::SIGCONT);
        });
        kill(parent, Signal::SIGSTOP).context("pause parent before poll registration")?;
    }

    while let Some(byte) = read_byte()? {
        match byte {
            0x7f => write_bytes(b"\x07")?,
            b'q' => return Ok(()),
            _ => {}
        }
    }
    Ok(())
}

fn latency() -> Result<()> {
    make_stdin_raw()?;
    if std::env::var("LECTOR_TEST_ATOMIC_STARTUP").as_deref() == Ok("1") {
        write_bytes(b"\x1b[?2026h\x1b[2J\x1b[HLECTOR-ATOMIC-TRANSIENT\x1b[6n")?;
        while read_byte()? != Some(b'R') {}
        thread::sleep(Duration::from_millis(40));
        write_bytes(b"\r\x1b[2KLECTOR-ATOMIC-FINAL\x1b[?2026l")?;
    } else {
        write_bytes(b"\x1b[2J\x1b[HLECTOR-LATENCY-READY")?;
    }

    let mut counter = 0_u64;
    while let Some(byte) = read_byte()? {
        match byte {
            b'p' => {
                if let Some(path) = std::env::var_os("LECTOR_TEST_STARTUP_ORDER_LOG") {
                    let mut log = OpenOptions::new().create(true).append(true).open(path)?;
                    writeln!(log, "input")?;
                    log.flush()?;
                }
                counter += 1;
                write_bytes(format!("\r\x1b[2KLECTOR-LATENCY-ACK-{counter:02}").as_bytes())?;
            }
            b's' => {
                let line = "x".repeat(79);
                let screen = std::iter::repeat_n(line.as_str(), 24)
                    .collect::<Vec<_>>()
                    .join("\r\n");
                write_bytes(format!("\x1b[2J\x1b[H{screen}").as_bytes())?;
            }
            b'q' => return Ok(()),
            _ => {}
        }
    }
    Ok(())
}

fn flood() -> Result<()> {
    make_stdin_raw()?;
    let running = Arc::new(AtomicBool::new(true));
    let writer_running = Arc::clone(&running);
    thread::spawn(move || {
        let block = [b'y'; 4096];
        let mut output = io::stdout().lock();
        while writer_running.load(Ordering::Relaxed) {
            if output.write_all(&block).is_err() || output.flush().is_err() {
                break;
            }
        }
    });
    while let Some(byte) = read_byte()? {
        if byte == 0x03 {
            running.store(false, Ordering::Relaxed);
            return Ok(());
        }
    }
    Ok(())
}

fn nested_lector() -> Result<()> {
    make_stdin_raw()?;
    write_bytes(b"LECTOR-OUTER-SHELL-READY\x1b[1;43H")?;
    if read_byte()? != Some(b'n') {
        std::process::exit(64);
    }

    let lector = required_environment("LECTOR_TEST_BINARY")?;
    let child_shell = required_environment("LECTOR_TEST_CHILD_SHELL")?;
    let config = required_environment("LECTOR_TEST_SPEECH_CONFIG")?;
    let inner_log = required_environment("LECTOR_TEST_INNER_SPEECH_LOG")?;
    let error = Command::new(lector)
        .args(["--shell", child_shell.as_str(), "--config", config.as_str()])
        .env(MODE_ENV, "bell")
        .env("LECTOR_PROC_STUB_LOG", inner_log)
        .exec();
    Err(error).context("exec nested Lector")
}

fn parent() -> Result<()> {
    let lector = required_environment("LECTOR_TEST_BINARY")?;
    let child_shell = required_environment("LECTOR_TEST_CHILD_SHELL")?;
    let child_mode = required_environment("LECTOR_TEST_CHILD_MODE")?;
    let config = required_environment("LECTOR_TEST_SPEECH_CONFIG")?;
    let status = Command::new(lector)
        .args(["--shell", child_shell.as_str(), "--config", config.as_str()])
        .env(MODE_ENV, child_mode)
        .status()
        .context("run Lector below parent fixture")?;

    make_stdin_raw()?;
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let _ = sender.send(read_byte().ok().flatten());
    });
    if receiver
        .recv_timeout(Duration::from_millis(250))
        .ok()
        .flatten()
        .is_some()
    {
        write_bytes(b"\x07LECTOR-PARENT-INPUT-LEAK")?;
    }
    write_bytes(format!("LECTOR-PARENT-AFTER:{}", exit_code(&status)).as_bytes())?;
    std::process::exit(exit_code(&status));
}

fn interactive() -> Result<()> {
    make_stdin_raw()?;
    write_bytes(b"LECTOR-OUTER-PROMPT>")?;
    let mut command = Vec::new();
    while let Some(byte) = read_byte()? {
        if matches!(byte, b'\r' | b'\n') {
            break;
        }
        command.push(byte);
    }
    if command != b"lector" {
        bail!(
            "interactive fixture expected the nested Lector command, received {:?}",
            String::from_utf8_lossy(&command)
        );
    }

    thread::sleep(Duration::from_millis(200));
    let lector = required_environment("LECTOR_TEST_BINARY")?;
    let config = required_environment("LECTOR_TEST_SPEECH_CONFIG")?;
    let inner_log = required_environment("LECTOR_TEST_INNER_SPEECH_LOG")?;
    let status = Command::new(lector)
        .args(["--config", config.as_str()])
        .env(MODE_ENV, "prompt")
        .env("LECTOR_PROC_STUB_LOG", inner_log)
        .status()
        .context("run nested Lector from interactive fixture")?;
    std::process::exit(exit_code(&status));
}

fn prompt() -> Result<()> {
    make_stdin_raw()?;
    write_bytes(b"LECTOR-INNER-PROMPT>")?;
    while read_byte()?.is_some() {}
    Ok(())
}

fn required_environment(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| format!("{name} is required"))
}

fn exit_code(status: &std::process::ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}
