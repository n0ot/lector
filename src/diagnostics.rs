//! Crash-oriented structured diagnostics with bounded byte previews.

use anyhow::{Context, Result};
use std::{
    collections::{HashMap, VecDeque},
    fs::File,
    io::{self, BufWriter, Write},
    path::Path,
    sync::{
        Arc, Condvar, Mutex, OnceLock, TryLockError,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

const BYTE_PREVIEW_LIMIT: usize = 192;
const DETAIL_LIMIT_BYTES: usize = 8 * 1024;
const LOG_RECORD_LIMIT_BYTES: usize = 64 * 1024;
const LOG_QUEUE_LIMIT_BYTES: usize = 2 * 1024 * 1024;
const LOG_FILE_LIMIT_BYTES: usize = 16 * 1024 * 1024;
const BYTE_THROTTLE_KEY_LIMIT: usize = 64;

struct Diagnostics {
    started: Instant,
    sequence: AtomicU64,
    queue: Arc<LogQueue>,
    dropped_records: AtomicU64,
    dropped_bytes: AtomicU64,
    byte_throttle: Mutex<HashMap<String, ByteThrottle>>,
    finished: Mutex<Option<mpsc::Receiver<()>>>,
}

#[derive(Default)]
struct LogQueueState {
    records: VecDeque<Vec<u8>>,
    pending_bytes: usize,
    shutdown: bool,
}

struct LogQueue {
    state: Mutex<LogQueueState>,
    available: Condvar,
}

struct RotatingFile {
    path: std::path::PathBuf,
    file: File,
    written: usize,
    limit: usize,
}

impl RotatingFile {
    fn create(path: &Path, limit: usize) -> io::Result<Self> {
        Ok(Self {
            path: path.to_owned(),
            file: File::create(path)?,
            written: 0,
            limit,
        })
    }
}

impl Write for RotatingFile {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.written != 0 && self.written.saturating_add(bytes.len()) > self.limit {
            self.file = File::create(&self.path)?;
            self.written = 0;
        }
        let written = self.file.write(bytes)?;
        self.written = self.written.saturating_add(written);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[derive(Default)]
struct ByteThrottle {
    last_emit_us: Option<u128>,
    suppressed_calls: usize,
    suppressed_bytes: usize,
}

static DIAGNOSTICS: OnceLock<Diagnostics> = OnceLock::new();

/// Initialize the process-wide log. A file is strongly preferred for stress
/// runs so diagnostic output can never compete with the physical terminal.
pub fn initialize(path: Option<&Path>) -> Result<()> {
    if DIAGNOSTICS.get().is_some() {
        return Ok(());
    }
    let writer: Box<dyn Write + Send> = match path {
        Some(path) => Box::new(BufWriter::new(
            RotatingFile::create(path, LOG_FILE_LIMIT_BYTES)
                .with_context(|| format!("create diagnostic log {}", path.display()))?,
        )),
        None => Box::new(io::stderr()),
    };
    let queue = Arc::new(LogQueue {
        state: Mutex::new(LogQueueState::default()),
        available: Condvar::new(),
    });
    let worker_queue = Arc::clone(&queue);
    let (finished_tx, finished_rx) = mpsc::sync_channel(1);
    let (start_tx, start_rx) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("lector-diagnostics".to_owned())
        .spawn(move || {
            let _ = start_rx.recv();
            run_writer(writer, &worker_queue, &finished_tx);
        })
        .context("start diagnostic writer")?;
    if DIAGNOSTICS
        .set(Diagnostics {
            started: Instant::now(),
            sequence: AtomicU64::new(1),
            queue: Arc::clone(&queue),
            dropped_records: AtomicU64::new(0),
            dropped_bytes: AtomicU64::new(0),
            byte_throttle: Mutex::new(HashMap::new()),
            finished: Mutex::new(Some(finished_rx)),
        })
        .is_err()
    {
        queue.shut_down();
        let _ = start_tx.try_send(());
        return Ok(());
    }

    // Queue the identity record before releasing the consumer. This record is
    // guaranteed to exist even though all steady-state producers use
    // contention-free try-lock semantics.
    event(
        "process",
        "log-start",
        &format!(
            "pid={} version={}",
            std::process::id(),
            env!("CARGO_PKG_VERSION")
        ),
    );
    let _ = start_tx.try_send(());

    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        event_nonblocking("process", "panic", &info.to_string());
        previous(info);
    }));
    Ok(())
}

pub fn event(scope: &str, kind: &str, detail: &str) {
    event_impl(scope, kind, detail, false);
}

fn event_nonblocking(scope: &str, kind: &str, detail: &str) {
    event_impl(scope, kind, detail, true);
}

fn event_impl(scope: &str, kind: &str, detail: &str, nonblocking: bool) {
    let Some(diagnostics) = DIAGNOSTICS.get() else {
        return;
    };
    let sequence = diagnostics.sequence.fetch_add(1, Ordering::Relaxed);
    let elapsed_us = diagnostics.started.elapsed().as_micros();
    let detail = bounded_str(detail, DETAIL_LIMIT_BYTES);
    let line = format!(
        "{{\"seq\":{sequence},\"elapsed_us\":{elapsed_us},\"scope\":{},\"kind\":{},\"dropped_records\":{},\"dropped_bytes\":{},\"detail\":{}}}\n",
        json_string(scope),
        json_string(kind),
        diagnostics.dropped_records.load(Ordering::Relaxed),
        diagnostics.dropped_bytes.load(Ordering::Relaxed),
        json_string(detail),
    );
    enqueue_line(diagnostics, line.into_bytes(), nonblocking);
}

pub fn bytes(scope: &str, label: &str, bytes: &[u8]) {
    let Some(diagnostics) = DIAGNOSTICS.get() else {
        return;
    };
    let elapsed_us = diagnostics.started.elapsed().as_micros();
    let (suppressed_calls, suppressed_bytes) = if label == "pty output from source" {
        let Ok(mut throttles) = diagnostics.byte_throttle.try_lock() else {
            return;
        };
        let key = format!("{scope}:{label}");
        if !throttles.contains_key(&key) && throttles.len() == BYTE_THROTTLE_KEY_LIMIT {
            return;
        }
        let throttle = throttles.entry(key).or_default();
        if throttle
            .last_emit_us
            .is_some_and(|last| elapsed_us.saturating_sub(last) < 20_000)
        {
            throttle.suppressed_calls = throttle.suppressed_calls.saturating_add(1);
            throttle.suppressed_bytes = throttle.suppressed_bytes.saturating_add(bytes.len());
            return;
        }
        throttle.last_emit_us = Some(elapsed_us);
        (
            std::mem::take(&mut throttle.suppressed_calls),
            std::mem::take(&mut throttle.suppressed_bytes),
        )
    } else {
        (0, 0)
    };
    let sequence = diagnostics.sequence.fetch_add(1, Ordering::Relaxed);
    let preview_len = bytes.len().min(BYTE_PREVIEW_LIMIT);
    let preview = escaped_bytes(&bytes[..preview_len]);
    let hash = bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    });
    let line = format!(
        "{{\"seq\":{sequence},\"elapsed_us\":{elapsed_us},\"scope\":{},\"kind\":\"bytes\",\"label\":{},\"length\":{},\"suppressed_calls\":{suppressed_calls},\"suppressed_bytes\":{suppressed_bytes},\"dropped_records\":{},\"dropped_bytes\":{},\"fnv1a64\":\"{hash:016x}\",\"truncated\":{},\"preview\":{}}}\n",
        json_string(scope),
        json_string(label),
        bytes.len(),
        diagnostics.dropped_records.load(Ordering::Relaxed),
        diagnostics.dropped_bytes.load(Ordering::Relaxed),
        bytes.len() > preview_len,
        json_string(&preview),
    );
    enqueue_line(diagnostics, line.into_bytes(), false);
}

/// Requests a best-effort final drain without ever waiting indefinitely for a
/// stuck filesystem or stderr sink.
pub fn shutdown(timeout: Duration) {
    let Some(diagnostics) = DIAGNOSTICS.get() else {
        return;
    };
    diagnostics.queue.shut_down();
    let receiver = diagnostics
        .finished
        .lock()
        .ok()
        .and_then(|mut receiver| receiver.take());
    if let Some(receiver) = receiver {
        let _ = receiver.recv_timeout(timeout);
    }
}

fn enqueue_line(diagnostics: &Diagnostics, line: Vec<u8>, nonblocking: bool) {
    let line = if line.len() <= LOG_RECORD_LIMIT_BYTES {
        line
    } else {
        b"{\"scope\":\"diagnostics\",\"kind\":\"record-too-large\"}\n".to_vec()
    };
    let (dropped_records, dropped_bytes) = if nonblocking {
        diagnostics.queue.try_enqueue(line)
    } else {
        diagnostics.queue.enqueue(line)
    };
    diagnostics
        .dropped_records
        .fetch_add(dropped_records, Ordering::Relaxed);
    diagnostics
        .dropped_bytes
        .fetch_add(dropped_bytes, Ordering::Relaxed);
}

impl LogQueue {
    fn enqueue(&self, line: Vec<u8>) -> (u64, u64) {
        let line_len = line.len();
        let Ok(state) = self.state.lock() else {
            return (1, line_len as u64);
        };
        self.enqueue_locked(state, line)
    }

    fn try_enqueue(&self, line: Vec<u8>) -> (u64, u64) {
        let line_len = line.len();
        let state = match self.state.try_lock() {
            Ok(state) => state,
            Err(TryLockError::WouldBlock | TryLockError::Poisoned(_)) => {
                return (1, line_len as u64);
            }
        };
        self.enqueue_locked(state, line)
    }

    fn enqueue_locked(
        &self,
        mut state: std::sync::MutexGuard<'_, LogQueueState>,
        line: Vec<u8>,
    ) -> (u64, u64) {
        let line_len = line.len();
        if state.shutdown {
            return (1, line_len as u64);
        }

        let mut dropped_records = 0u64;
        let mut dropped_bytes = 0u64;
        while state.pending_bytes.saturating_add(line_len) > LOG_QUEUE_LIMIT_BYTES {
            let Some(stale) = state.records.pop_front() else {
                return (1, line_len as u64);
            };
            state.pending_bytes = state.pending_bytes.saturating_sub(stale.len());
            dropped_records = dropped_records.saturating_add(1);
            dropped_bytes = dropped_bytes.saturating_add(stale.len() as u64);
        }
        state.pending_bytes = state.pending_bytes.saturating_add(line_len);
        state.records.push_back(line);
        drop(state);
        self.available.notify_one();
        (dropped_records, dropped_bytes)
    }

    fn next_record(&self) -> Option<Vec<u8>> {
        let mut state = self.state.lock().ok()?;
        loop {
            if let Some(record) = state.records.pop_front() {
                state.pending_bytes = state.pending_bytes.saturating_sub(record.len());
                return Some(record);
            }
            if state.shutdown {
                return None;
            }
            state = self.available.wait(state).ok()?;
        }
    }

    fn shut_down(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.shutdown = true;
        }
        self.available.notify_all();
    }

    #[cfg(test)]
    fn usage(&self) -> (usize, usize) {
        let state = self.state.lock().expect("diagnostic queue lock");
        (state.records.len(), state.pending_bytes)
    }
}

fn run_writer(
    mut writer: Box<dyn Write + Send>,
    queue: &LogQueue,
    finished: &mpsc::SyncSender<()>,
) {
    let mut failed = false;
    while let Some(record) = queue.next_record() {
        if !failed && (writer.write_all(&record).is_err() || writer.flush().is_err()) {
            // Continue draining the bounded queue so shutdown and producers
            // never depend on a failed logging device.
            failed = true;
        }
    }
    if !failed {
        let _ = writer.flush();
    }
    let _ = finished.try_send(());
}

fn bounded_str(value: &str, limit: usize) -> &str {
    if value.len() <= limit {
        return value;
    }
    let mut end = limit;
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    &value[..end]
}

fn escaped_bytes(bytes: &[u8]) -> String {
    let mut output = String::new();
    for byte in bytes {
        match byte {
            b'\x1b' => output.push_str("\\e"),
            b'\r' => output.push_str("\\r"),
            b'\n' => output.push_str("\\n"),
            b'\t' => output.push_str("\\t"),
            0x20..=0x7e => output.push(char::from(*byte)),
            _ => output.push_str(&format!("\\x{byte:02x}")),
        }
    }
    output
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"<json encoding failed>\"".to_owned())
}

#[cfg(test)]
mod tests {
    use super::{LOG_QUEUE_LIMIT_BYTES, LogQueue, LogQueueState, RotatingFile};
    use std::{
        fs,
        io::Write,
        sync::{Condvar, Mutex},
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn diagnostic_queue_is_bounded_and_preserves_the_newest_tail() {
        let queue = LogQueue {
            state: Mutex::new(LogQueueState::default()),
            available: Condvar::new(),
        };
        let record = vec![b'x'; 64 * 1024];
        let mut dropped = 0;
        for _ in 0..100 {
            dropped += queue.enqueue(record.clone()).0;
        }
        let (records, bytes) = queue.usage();
        assert!(records > 0);
        assert!(bytes <= LOG_QUEUE_LIMIT_BYTES);
        assert!(dropped > 0);
    }

    #[test]
    fn panic_path_never_waits_for_the_queue_mutex() {
        let queue = LogQueue {
            state: Mutex::new(LogQueueState::default()),
            available: Condvar::new(),
        };
        let _locked = queue.state.lock().unwrap();
        assert_eq!(queue.try_enqueue(b"record".to_vec()), (1, 6));
    }

    #[test]
    fn diagnostic_file_restarts_at_a_hard_size_boundary() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "lector-diagnostic-rotation-{}-{unique}.log",
            std::process::id()
        ));
        let mut file = RotatingFile::create(&path, 16).unwrap();
        file.write_all(b"first-record").unwrap();
        file.flush().unwrap();
        file.write_all(b"new-tail").unwrap();
        file.flush().unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"new-tail");
        fs::remove_file(path).unwrap();
    }
}
