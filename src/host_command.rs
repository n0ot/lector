//! Bounded execution for optional host capability tools.
//!
//! Lector treats tools such as `infocmp` and `tic` as hints, never as trusted
//! startup dependencies. Capturing through files avoids pipe backpressure and
//! helper threads, which is important when the virtual terminfo cache is built
//! before the application PTY's fork boundary.

use std::{
    fs::{File, OpenOptions},
    io::{self, ErrorKind, Read},
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

pub(crate) const HOST_TOOL_TIMEOUT: Duration = Duration::from_millis(500);
pub(crate) const HOST_TOOL_POLL_INTERVAL: Duration = Duration::from_millis(5);
pub(crate) const HOST_TOOL_STDOUT_LIMIT: usize = 1024 * 1024;
pub(crate) const HOST_TOOL_STDERR_LIMIT: usize = 8 * 1024;

static NEXT_CAPTURE_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub(crate) struct BoundedOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

pub(crate) fn run_bounded_output(
    command: &mut Command,
    capture_directory: &Path,
    label: &str,
) -> io::Result<BoundedOutput> {
    run_bounded_output_with_timeout(command, capture_directory, label, HOST_TOOL_TIMEOUT)
}

fn run_bounded_output_with_timeout(
    command: &mut Command,
    capture_directory: &Path,
    label: &str,
    timeout: Duration,
) -> io::Result<BoundedOutput> {
    let capture = CaptureFiles::create(capture_directory, label)?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(capture.stdout.try_clone()?))
        .stderr(Stdio::from(capture.stderr.try_clone()?));
    let status = run_bounded(command, label, timeout)?;
    let stdout = read_bounded_file(&capture.stdout_path, HOST_TOOL_STDOUT_LIMIT);
    let stderr = read_bounded_file(&capture.stderr_path, HOST_TOOL_STDERR_LIMIT);
    let _ = std::fs::remove_file(&capture.stdout_path);
    let _ = std::fs::remove_file(&capture.stderr_path);
    Ok(BoundedOutput {
        status,
        stdout: stdout?,
        stderr: stderr?,
    })
}

fn run_bounded(command: &mut Command, label: &str, timeout: Duration) -> io::Result<ExitStatus> {
    let mut child = command.spawn()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(HOST_TOOL_POLL_INTERVAL),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::new(
                    ErrorKind::TimedOut,
                    format!(
                        "{label} exceeded the {} ms host-tool limit",
                        timeout.as_millis()
                    ),
                ));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        }
    }
}

pub(crate) fn read_bounded_file(path: &Path, limit: usize) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(limit.saturating_add(1).try_into().unwrap_or(u64::MAX))
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            format!("{} exceeds the {limit}-byte limit", path.display()),
        ));
    }
    Ok(bytes)
}

struct CaptureFiles {
    stdout: File,
    stderr: File,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
}

impl CaptureFiles {
    fn create(directory: &Path, label: &str) -> io::Result<Self> {
        std::fs::create_dir_all(directory)?;
        for _ in 0..64 {
            let id = NEXT_CAPTURE_ID.fetch_add(1, Ordering::Relaxed);
            let prefix = format!(".lector-{label}-{}-{id}", std::process::id());
            let stdout_path = directory.join(format!("{prefix}.stdout"));
            let stderr_path = directory.join(format!("{prefix}.stderr"));
            let stdout = match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&stdout_path)
            {
                Ok(file) => file,
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            };
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&stderr_path)
            {
                Ok(stderr) => {
                    return Ok(Self {
                        stdout,
                        stderr,
                        stdout_path,
                        stderr_path,
                    });
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    let _ = std::fs::remove_file(&stdout_path);
                }
                Err(error) => {
                    let _ = std::fs::remove_file(&stdout_path);
                    return Err(error);
                }
            }
        }
        Err(io::Error::new(
            ErrorKind::AlreadyExists,
            "could not reserve unique host-tool capture files",
        ))
    }
}

impl Drop for CaptureFiles {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.stdout_path);
        let _ = std::fs::remove_file(&self.stderr_path);
    }
}

#[cfg(test)]
mod tests {
    use super::run_bounded_output_with_timeout;
    use std::{process::Command, time::Duration};

    #[test]
    fn wedged_host_tool_is_killed_at_its_deadline() {
        let directory = tempfile::tempdir().expect("create capture directory");
        let mut command = Command::new("sh");
        command.args(["-c", "while :; do :; done"]);
        let started = std::time::Instant::now();
        let error = run_bounded_output_with_timeout(
            &mut command,
            directory.path(),
            "test-tool",
            Duration::from_millis(20),
        )
        .expect_err("the wedged tool must time out");

        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
