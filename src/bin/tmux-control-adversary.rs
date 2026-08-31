//! A deliberately hostile tmux control-mode peer for Lector integration tests.
//!
//! Lector can launch this program directly as its `--shell`. Select a behavior
//! with `LECTOR_TMUX_ADVERSARY` (`normal`, `malformed`, `silent`, `no-read`,
//! `flood`, `hidden-flood`, or `nested`). The program owns no tmux server or
//! socket, so a failed run cannot poison the user's real tmux server.

use anyhow::{Context, Result, bail};
use nix::sys::termios;
use std::{
    fs::OpenOptions,
    io::{self, BufRead, Write},
    os::fd::AsFd,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

const START: &[u8] = b"\x1bP1000p";
const END: &[u8] = b"\x1b\\";
const ACTIVE_PANE: u64 = 20;
const HIDDEN_PANE: u64 = 21;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Scenario {
    Normal,
    Malformed,
    Silent,
    NoRead,
    Flood,
    HiddenFlood,
    Nested,
}

impl Scenario {
    fn from_environment() -> Result<Self> {
        match std::env::var("LECTOR_TMUX_ADVERSARY")
            .unwrap_or_else(|_| "normal".to_owned())
            .as_str()
        {
            "normal" => Ok(Self::Normal),
            "malformed" => Ok(Self::Malformed),
            "silent" => Ok(Self::Silent),
            "no-read" => Ok(Self::NoRead),
            "flood" => Ok(Self::Flood),
            "hidden-flood" => Ok(Self::HiddenFlood),
            "nested" => Ok(Self::Nested),
            value => bail!("unknown LECTOR_TMUX_ADVERSARY scenario {value:?}"),
        }
    }
}

#[derive(Clone)]
struct Wire(Arc<Mutex<io::Stdout>>);

impl Wire {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(io::stdout())))
    }

    fn write(&self, bytes: &[u8]) -> Result<()> {
        let mut output = self.0.lock().expect("adversary stdout mutex poisoned");
        output
            .write_all(bytes)
            .context("write adversarial control output")?;
        output.flush().context("flush adversarial control output")
    }

    fn control_output(&self, pane_id: u64, bytes: &[u8]) -> Result<()> {
        let mut record = format!("%output %{pane_id} ").into_bytes();
        encode_octal(bytes, &mut record);
        record.push(b'\n');
        self.write(&record)
    }

    fn reply(&self, serial: &mut u64, lines: &[&[u8]]) -> Result<()> {
        let current = *serial;
        *serial = serial.saturating_add(1);
        let mut reply = format!("%begin {current} {current} 0\n").into_bytes();
        for line in lines {
            reply.extend_from_slice(line);
            reply.push(b'\n');
        }
        reply.extend_from_slice(format!("%end {current} {current} 0\n").as_bytes());
        self.write(&reply)
    }

    fn exit(&self, reason: &str) -> Result<()> {
        self.write(format!("%exit {reason}\n").as_bytes())?;
        self.write(END)
    }
}

fn main() -> Result<()> {
    if let Some(pid_file) = std::env::var_os("LECTOR_TEST_BLOCKING_APP_PID_FILE") {
        std::fs::write(pid_file, format!("{}\n", std::process::id()))?;
        loop {
            thread::park_timeout(Duration::from_secs(60));
        }
    }
    let scenario = Scenario::from_environment()?;
    make_stdin_raw().context("put adversarial control PTY in raw mode")?;
    let wire = Wire::new();
    wire.write(START)?;
    let mut serial = 1;
    wire.reply(&mut serial, &[])?;

    let mut peer = Peer::new(scenario, wire.clone(), serial, 0);
    peer.run()?;
    Ok(())
}

struct Peer {
    scenario: Scenario,
    wire: Wire,
    serial: u64,
    depth: usize,
    bootstrapped: bool,
    active_window: u64,
    nested: Option<NestedPeer>,
    flood_stop: Option<Arc<std::sync::atomic::AtomicBool>>,
}

impl Peer {
    fn new(scenario: Scenario, wire: Wire, serial: u64, depth: usize) -> Self {
        Self {
            scenario,
            wire,
            serial,
            depth,
            bootstrapped: false,
            active_window: 10,
            nested: None,
            flood_stop: None,
        }
    }

    fn run(&mut self) -> Result<()> {
        let stdin = io::stdin();
        let mut reader = stdin.lock();
        let mut line = Vec::new();
        loop {
            line.clear();
            let count = reader
                .read_until(b'\n', &mut line)
                .context("read command from Lector")?;
            if count == 0 {
                break;
            }
            if line.last() == Some(&b'\n') {
                line.pop();
            }
            if self.handle_command(&line)? {
                return Ok(());
            }
            if self.scenario == Scenario::NoRead && self.bootstrapped {
                loop {
                    thread::park_timeout(Duration::from_secs(60));
                }
            }
        }
        Ok(())
    }

    /// Returns true when the peer completed a clean detach.
    fn handle_command(&mut self, command: &[u8]) -> Result<bool> {
        let text = String::from_utf8_lossy(command);
        if text == "display-message -p -F '#{client_flags}'" {
            self.wire
                .reply(&mut self.serial, &[b"attached,control-mode,pause-after=1"])?;
            return Ok(false);
        }
        if self.send_inventory_reply(&text)? {
            return Ok(false);
        }
        if text.starts_with("capture-pane ") {
            let first_capture = !self.bootstrapped;
            let ready = if self.depth == 0 {
                b"LECTOR-ADVERSARY-READY".as_slice()
            } else {
                b"LECTOR-NESTED-READY".as_slice()
            };
            self.wire.reply(&mut self.serial, &[ready])?;
            self.bootstrapped = true;
            if self.depth == 0 && first_capture {
                self.after_bootstrap()?;
            }
            return Ok(false);
        }
        if text.starts_with("send-keys -H -t ") {
            let bytes = decode_send_keys(command)?;
            let stopped_flood = self.nested.is_none()
                && bytes.contains(&b'p')
                && matches!(self.scenario, Scenario::Flood | Scenario::HiddenFlood);
            if self.nested.is_none() && bytes.contains(&b'p') {
                self.stop_flood();
                record_event("input-p");
            }
            // A blocking flood write may own the wire lock. Do not make the
            // command-reading thread wait behind its own intentionally hostile
            // producer; dropping this one reply is itself valid bad-peer
            // behavior and lets the next interactive command be observed.
            if stopped_flood {
                return Ok(false);
            }
            self.wire.reply(&mut self.serial, &[])?;
            if let Some(nested) = self.nested.as_mut() {
                if nested.ingest(&bytes)? {
                    self.nested = None;
                }
            } else if bytes.contains(&b'p') {
                let pane = if self.active_window == 10 {
                    ACTIVE_PANE
                } else {
                    HIDDEN_PANE
                };
                self.wire
                    .control_output(pane, b"LECTOR-ADVERSARY-INPUT-ACK")?;
            }
            return Ok(false);
        }
        if text == "next-window" || text.starts_with("select-window ") {
            self.active_window = if self.active_window == 10 { 11 } else { 10 };
            record_event(&format!("window-{}", self.active_window));
            self.wire.reply(&mut self.serial, &[])?;
            self.wire.write(
                format!("%session-window-changed $1 @{}\n", self.active_window).as_bytes(),
            )?;
            return Ok(false);
        }
        if text.starts_with("detach-client") {
            record_event("detach");
            if matches!(self.scenario, Scenario::Flood | Scenario::HiddenFlood) {
                self.stop_flood();
                // Returning from main closes the hostile transport even if its
                // output thread was blocked in the middle of a flood record.
                return Ok(true);
            }
            self.wire.reply(&mut self.serial, &[])?;
            self.stop_flood();
            self.wire.exit("detached by adversary")?;
            return Ok(true);
        }

        // Flow control, resizes, pane pause/continue, and ordinary commands all
        // receive a valid empty completion unless this scenario deliberately
        // withholds output.
        if self.scenario != Scenario::Silent || !self.bootstrapped {
            self.wire.reply(&mut self.serial, &[])?;
        }
        Ok(false)
    }

    fn send_inventory_reply(&mut self, command: &str) -> Result<bool> {
        let suffix = if self.depth == 0 { "outer" } else { "nested" };
        let Some(group) = inventory_reply(command, suffix, self.active_window, true) else {
            return Ok(false);
        };
        let borrowed = group.iter().map(Vec::as_slice).collect::<Vec<_>>();
        self.wire.reply(&mut self.serial, &borrowed)?;
        Ok(true)
    }

    fn after_bootstrap(&mut self) -> Result<()> {
        match self.scenario {
            Scenario::Malformed => {
                self.wire.write(b"%output definitely-not-a-pane bad\n")?;
                // Recovery deliberately quarantines the rest of a malformed
                // control stream. Terminate that stream before emitting the
                // ordinary shell marker which must survive the handoff.
                self.wire.write(END)?;
                self.wire.write(b"LECTOR-ADVERSARY-BAD-RECOVERED\r\n")?;
                // Exit after the recovery marker. Lector must render the
                // replayed direct output, resolve the failed connection, and
                // shut down normally when the PTY reaches EOF.
                std::process::exit(0);
            }
            Scenario::Flood | Scenario::HiddenFlood => self.start_flood(),
            Scenario::Nested => self.start_nested()?,
            Scenario::Normal | Scenario::Silent | Scenario::NoRead => {}
        }
        Ok(())
    }

    fn start_flood(&mut self) {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.flood_stop = Some(stop.clone());
        let wire = self.wire.clone();
        let pane_id = if self.scenario == Scenario::HiddenFlood {
            HIDDEN_PANE
        } else {
            ACTIVE_PANE
        };
        thread::spawn(move || {
            let payload = [b'X'; 2048];
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                if wire.control_output(pane_id, &payload).is_err() {
                    break;
                }
            }
        });
    }

    fn stop_flood(&self) {
        if let Some(stop) = &self.flood_stop {
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn start_nested(&mut self) -> Result<()> {
        let nested_wire = NestedWire {
            outer: self.wire.clone(),
            pane_id: ACTIVE_PANE,
        };
        nested_wire.write(START)?;
        let mut child_serial = 1;
        nested_wire.reply(&mut child_serial, &[])?;
        self.nested = Some(NestedPeer::new(nested_wire, child_serial));
        Ok(())
    }
}

/// Adapts a child control stream into `%output` records on its parent pane.
#[derive(Clone)]
struct NestedWire {
    outer: Wire,
    pane_id: u64,
}

impl NestedWire {
    fn write(&self, bytes: &[u8]) -> Result<()> {
        self.outer.control_output(self.pane_id, bytes)
    }

    fn reply(&self, serial: &mut u64, lines: &[&[u8]]) -> Result<()> {
        let current = *serial;
        *serial = serial.saturating_add(1);
        let mut reply = format!("%begin {current} {current} 0\n").into_bytes();
        for line in lines {
            reply.extend_from_slice(line);
            reply.push(b'\n');
        }
        reply.extend_from_slice(format!("%end {current} {current} 0\n").as_bytes());
        self.write(&reply)
    }

    fn control_output(&self, pane_id: u64, bytes: &[u8]) -> Result<()> {
        let mut record = format!("%output %{pane_id} ").into_bytes();
        encode_octal(bytes, &mut record);
        record.push(b'\n');
        self.write(&record)
    }

    fn exit(&self, reason: &str) -> Result<()> {
        self.write(format!("%exit {reason}\n").as_bytes())?;
        self.write(END)
    }
}

struct NestedPeer {
    wire: NestedWire,
    serial: u64,
    active_window: u64,
    input: Vec<u8>,
}

impl NestedPeer {
    fn new(wire: NestedWire, serial: u64) -> Self {
        Self {
            wire,
            serial,
            active_window: 10,
            input: Vec::new(),
        }
    }

    fn ingest(&mut self, bytes: &[u8]) -> Result<bool> {
        self.input.extend_from_slice(bytes);
        while let Some(newline) = self.input.iter().position(|byte| *byte == b'\n') {
            let mut line = self.input.drain(..=newline).collect::<Vec<_>>();
            line.pop();
            if self.handle_command(&line)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn handle_command(&mut self, command: &[u8]) -> Result<bool> {
        let text = String::from_utf8_lossy(command);
        if text == "display-message -p -F '#{client_flags}'" {
            self.wire
                .reply(&mut self.serial, &[b"attached,control-mode,pause-after=1"])?;
        } else if self.send_inventory_reply(&text)? {
        } else if text.starts_with("capture-pane ") {
            self.wire
                .reply(&mut self.serial, &[b"LECTOR-NESTED-READY"])?;
        } else if text.starts_with("send-keys -H -t ") {
            let bytes = decode_send_keys(command)?;
            self.wire.reply(&mut self.serial, &[])?;
            if bytes.contains(&b'p') {
                record_event("nested-input-p");
                let pane = if self.active_window == 10 {
                    ACTIVE_PANE
                } else {
                    HIDDEN_PANE
                };
                self.wire.control_output(pane, b"LECTOR-NESTED-INPUT-ACK")?;
            }
        } else if text == "next-window" || text.starts_with("select-window ") {
            self.wire.reply(&mut self.serial, &[])?;
            self.active_window = if self.active_window == 10 { 11 } else { 10 };
            self.wire.write(
                format!("%session-window-changed $1 @{}\n", self.active_window).as_bytes(),
            )?;
        } else if text.starts_with("detach-client") {
            record_event("nested-detach");
            self.wire.reply(&mut self.serial, &[])?;
            self.wire.exit("nested adversary detached")?;
            return Ok(true);
        } else {
            self.wire.reply(&mut self.serial, &[])?;
        }
        Ok(false)
    }

    fn send_inventory_reply(&mut self, command: &str) -> Result<bool> {
        let Some(group) = inventory_reply(command, "nested", self.active_window, false) else {
            return Ok(false);
        };
        let borrowed = group.iter().map(Vec::as_slice).collect::<Vec<_>>();
        self.wire.reply(&mut self.serial, &borrowed)?;
        Ok(true)
    }
}

fn inventory_reply(
    command: &str,
    suffix: &str,
    active_window: u64,
    include_command_prompt: bool,
) -> Option<Vec<Vec<u8>>> {
    let index = lector::tmux_model::INVENTORY_COMMANDS
        .iter()
        .position(|candidate| candidate.trim_end_matches('\n') == command)?;
    let active_10 = u8::from(active_window == 10);
    let active_11 = u8::from(active_window == 11);
    let mut bindings = vec![
        b"B\tn\t0\tnext-window".to_vec(),
        b"B\td\t0\tdetach-client".to_vec(),
    ];
    if include_command_prompt {
        bindings.push(b"B\t:\t0\tcommand-prompt".to_vec());
    }
    let groups = vec![
        vec![format!("S\t$1\tadversary-{suffix}").into_bytes()],
        vec![
            format!("W\t$1\t@10\t1\t{active_10}\tb25f,80x24,0,0,20\tb25f,80x24,0,0,20\t*\tactive")
                .into_bytes(),
            format!("W\t$1\t@11\t2\t{active_11}\tb260,80x24,0,0,21\tb260,80x24,0,0,21\t-\thidden")
                .into_bytes(),
        ],
        vec![
            b"P\t@10\t%20\t1\t1\t0\t0\t80\t24\t0\t0\t0\t1\t0\t0\t0\t0\tactive-pane".to_vec(),
            b"P\t@11\t%21\t1\t1\t0\t0\t80\t24\t0\t0\t0\t1\t0\t0\t0\t0\thidden-pane".to_vec(),
        ],
        vec![format!("A\t$1\tadversary-{suffix}").into_bytes()],
        vec![b"O\tbase-index\t1".to_vec()],
        vec![b"O\tpane-base-index\t1".to_vec()],
        vec![format!("C\tclient_name\t/dev/lector-adversary-{suffix}").into_bytes()],
        vec![b"O\tprefix\tC-b".to_vec()],
        vec![b"O\tprefix2\tNone".to_vec()],
        vec![b"O\tkey-table\troot".to_vec()],
        vec![b"O\trepeat-time\t500".to_vec()],
        bindings,
    ];
    groups.into_iter().nth(index)
}

fn make_stdin_raw() -> Result<()> {
    let stdin = io::stdin();
    let mut attributes = termios::tcgetattr(stdin.as_fd()).context("read stdin termios")?;
    termios::cfmakeraw(&mut attributes);
    termios::tcsetattr(stdin.as_fd(), termios::SetArg::TCSANOW, &attributes)
        .context("set stdin raw")
}

fn record_event(event: &str) {
    let Some(path) = std::env::var_os("LECTOR_TMUX_ADVERSARY_EVENTS") else {
        return;
    };
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let _ = writeln!(file, "{event}");
    let _ = file.flush();
}

fn encode_octal(bytes: &[u8], output: &mut Vec<u8>) {
    for &byte in bytes {
        if (0x20..=0x7e).contains(&byte) && byte != b'\\' {
            output.push(byte);
        } else {
            output.extend_from_slice(format!("\\{byte:03o}").as_bytes());
        }
    }
}

fn decode_send_keys(command: &[u8]) -> Result<Vec<u8>> {
    let text = std::str::from_utf8(command).context("send-keys command is not UTF-8")?;
    let mut fields = text.split_ascii_whitespace();
    if fields.next() != Some("send-keys")
        || fields.next() != Some("-H")
        || fields.next() != Some("-t")
        || fields.next().is_none()
    {
        bail!("malformed send-keys command")
    }
    fields
        .map(|field| {
            if field.len() != 2 {
                bail!("malformed send-keys byte")
            }
            u8::from_str_radix(field, 16).context("parse send-keys byte")
        })
        .collect()
}
