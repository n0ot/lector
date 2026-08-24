use crate::host_command::{
    BoundedOutput, HOST_TOOL_STDOUT_LIMIT, read_bounded_file, run_bounded_output,
};
use crate::terminal::TerminalGeometry;
use anyhow::{Context, Result, anyhow};
use nix::errno::Errno;
use nix::sys::{
    signal::{Signal, kill},
    termios,
};
use nix::unistd::{Pid, dup};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::collections::VecDeque;
use std::ffi::OsStr;
use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{self, ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, RawFd};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, TryLockError};
use std::thread;
use std::time::{Duration, Instant};

const CHILD_TERMINATION_REAP_TIMEOUT: Duration = Duration::from_millis(250);
const CHILD_TERMINATION_REAP_POLL_INTERVAL: Duration = Duration::from_millis(5);
const VIRTUAL_TERM: &str = "xterm-256color";
const SYNC_TERMINFO_CAPABILITY: &str = "\tSync=\\E[?2026%?%p1%{1}%-%tl%eh%;,\n";
const COMPILED_SYNC_NAME: &[u8] = b"Sync\0";
const COMPILED_SYNC_VALUE: &[u8] = b"\x1b[?2026%?%p1%{1}%-%tl%eh%;\0";
const SYNC_TERMINFO_CACHE_VERSION: &str = "sync-v3";
const SYNC_TERMINFO_MARKER: &str = "lector-xterm-256color-sync-v3\n";
const BUNDLED_SYNC_TERMINFO: &[u8] = include_bytes!("../assets/terminfo/xterm-256color-sync.b64");
const TERMINFO_CACHE_LOCK_TIMEOUT: Duration = Duration::from_secs(2);
const TERMINFO_CACHE_LOCK_POLL_INTERVAL: Duration = Duration::from_millis(5);
const UNIQUE_DIRECTORY_ATTEMPTS: usize = 128;

static NEXT_TERMINFO_DIRECTORY_ID: AtomicU64 = AtomicU64::new(1);
static TERMINFO_CACHE_PROCESS_LOCK: Mutex<()> = Mutex::new(());
static TERMINFO_CACHE_COMPONENT: OnceLock<String> = OnceLock::new();
static DECODED_BUNDLED_SYNC_TERMINFO: OnceLock<Vec<u8>> = OnceLock::new();

#[derive(Clone, Debug, Eq, PartialEq)]
enum TerminfoDirectory {
    Persistent(PathBuf),
    Temporary(Arc<TemporaryDirectory>),
}

impl TerminfoDirectory {
    fn path(&self) -> &Path {
        match self {
            Self::Persistent(path) => path,
            Self::Temporary(directory) => &directory.path,
        }
    }

    fn temporary_owner(&self) -> Option<Arc<TemporaryDirectory>> {
        match self {
            Self::Persistent(_) => None,
            Self::Temporary(directory) => Some(Arc::clone(directory)),
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct TemporaryDirectory {
    path: PathBuf,
}

impl TemporaryDirectory {
    fn create(parent: &Path, prefix: &str) -> io::Result<Self> {
        std::fs::create_dir_all(parent)?;
        for _ in 0..UNIQUE_DIRECTORY_ATTEMPTS {
            let id = NEXT_TERMINFO_DIRECTORY_ID.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!("{prefix}-{}-{id}", std::process::id()));
            match private_directory_builder().create(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            ErrorKind::AlreadyExists,
            "could not reserve a unique terminfo directory",
        ))
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ = remove_path(&self.path);
    }
}

struct TerminfoCacheLock {
    _process: MutexGuard<'static, ()>,
    file: File,
}

impl Drop for TerminfoCacheLock {
    fn drop(&mut self) {
        // SAFETY: `file` remains open for this call and is owned by the guard.
        let _ = unsafe { nix::libc::flock(self.file.as_raw_fd(), nix::libc::LOCK_UN) };
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VirtualTerminalEnvironment {
    pub term: String,
    terminfo: Option<TerminfoDirectory>,
}

impl VirtualTerminalEnvironment {
    pub fn apply(&self, command: &mut CommandBuilder) {
        command.env("TERM", &self.term);
        if let Some(terminfo) = &self.terminfo {
            command.env("TERMINFO", terminfo.path());
        } else {
            command.env_remove("TERMINFO");
        }
        command.env("TERM_PROGRAM", "Lector");
        command.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
        // COLORTERM is deliberately untouched: callers preserve it only when
        // the selected virtual profile implements the advertised color mode.
    }
}

/// The application-facing contract implemented by Lector. Inheriting a
/// vendor TERM (including xterm-ghostty) would promise protocols owned by the
/// physical terminal rather than by Lector's compositor.
#[must_use]
pub fn compatible_terminal_environment() -> VirtualTerminalEnvironment {
    VirtualTerminalEnvironment {
        term: VIRTUAL_TERM.to_owned(),
        terminfo: synchronized_output_terminfo().ok(),
    }
}

fn synchronized_output_terminfo() -> io::Result<TerminfoDirectory> {
    let cache = dirs::cache_dir().map(|cache| {
        cache
            .join("lector")
            .join("terminfo")
            .join(terminfo_cache_component())
    });
    match cache {
        Some(cache) => synchronized_output_terminfo_with_cache(&cache),
        None => temporary_synchronized_output_terminfo(),
    }
}

fn synchronized_output_terminfo_with_cache(cache: &Path) -> io::Result<TerminfoDirectory> {
    finish_terminfo_installation(install_synchronized_output_terminfo_with(cache, true))
}

#[cfg(test)]
fn synchronized_output_terminfo_with_cache_options(
    cache: &Path,
    use_host_tools: bool,
    lock_timeout: Duration,
) -> io::Result<TerminfoDirectory> {
    finish_terminfo_installation(install_synchronized_output_terminfo_with_timeout(
        cache,
        use_host_tools,
        lock_timeout,
    ))
}

fn finish_terminfo_installation(installed: io::Result<PathBuf>) -> io::Result<TerminfoDirectory> {
    match installed {
        Ok(path) => Ok(TerminfoDirectory::Persistent(path)),
        Err(cache_error) => temporary_synchronized_output_terminfo().map_err(|temporary_error| {
            io::Error::other(format!(
                "private terminfo cache failed ({cache_error}); temporary fallback failed ({temporary_error})"
            ))
        }),
    }
}

fn temporary_synchronized_output_terminfo() -> io::Result<TerminfoDirectory> {
    let prefix = format!("lector-terminfo-{}", terminfo_cache_component());
    let directory = TemporaryDirectory::create(&std::env::temp_dir(), &prefix)?;
    write_bundled_terminfo(&directory.path)?;
    write_terminfo_marker(&directory.path)?;
    if !installed_terminfo_ready(&directory.path) {
        return Err(io::Error::other(
            "temporary synchronized-output terminfo entry is incomplete",
        ));
    }
    Ok(TerminfoDirectory::Temporary(Arc::new(directory)))
}

fn install_synchronized_output_terminfo_with(
    cache: &Path,
    use_host_tools: bool,
) -> io::Result<PathBuf> {
    install_synchronized_output_terminfo_with_timeout(
        cache,
        use_host_tools,
        TERMINFO_CACHE_LOCK_TIMEOUT,
    )
}

fn install_synchronized_output_terminfo_with_timeout(
    cache: &Path,
    use_host_tools: bool,
    lock_timeout: Duration,
) -> io::Result<PathBuf> {
    let installed = cache.join("db");
    if installed_terminfo_ready(&installed) {
        return Ok(installed);
    }

    std::fs::create_dir_all(cache)?;
    let _lock = acquire_terminfo_cache_lock(cache, lock_timeout)?;
    if installed_terminfo_ready(&installed) {
        return Ok(installed);
    }
    remove_stale_terminfo_staging_directories(cache)?;
    remove_path(&installed)?;

    let staging = TemporaryDirectory::create(cache, ".db")?;

    let used_host_entry = use_host_tools && build_host_terminfo(&staging.path).is_ok();
    if !used_host_entry {
        reset_private_directory(&staging.path)?;
        write_bundled_terminfo(&staging.path)?;
    }
    write_terminfo_marker(&staging.path)?;

    match std::fs::rename(&staging.path, &installed) {
        Ok(()) => {}
        Err(_) if installed_terminfo_ready(&installed) => {
            return Ok(installed);
        }
        Err(error) => return Err(error),
    }
    if installed_terminfo_ready(&installed) {
        Ok(installed)
    } else {
        let _ = remove_path(&installed);
        Err(io::Error::other(
            "private synchronized-output terminfo entry is incomplete",
        ))
    }
}

fn acquire_terminfo_cache_lock(cache: &Path, timeout: Duration) -> io::Result<TerminfoCacheLock> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(Instant::now);
    let process = loop {
        match TERMINFO_CACHE_PROCESS_LOCK.try_lock() {
            Ok(lock) => break lock,
            Err(TryLockError::Poisoned(error)) => break error.into_inner(),
            Err(TryLockError::WouldBlock) => wait_for_terminfo_cache_lock(deadline)?,
        }
    };
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(cache.join(".lock"))?;
    loop {
        // SAFETY: `file` is open and owned by this function. `LOCK_NB` keeps
        // acquisition bounded; the resulting advisory lock is released by the
        // returned guard.
        let result =
            unsafe { nix::libc::flock(file.as_raw_fd(), nix::libc::LOCK_EX | nix::libc::LOCK_NB) };
        if result == 0 {
            return Ok(TerminfoCacheLock {
                _process: process,
                file,
            });
        }
        let error = io::Error::last_os_error();
        let blocked = error
            .raw_os_error()
            .is_some_and(|code| code == nix::libc::EWOULDBLOCK || code == nix::libc::EAGAIN);
        if !blocked {
            return Err(error);
        }
        wait_for_terminfo_cache_lock(deadline)?;
    }
}

fn wait_for_terminfo_cache_lock(deadline: Instant) -> io::Result<()> {
    let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
        return Err(io::Error::new(
            ErrorKind::TimedOut,
            "private terminfo cache lock timed out",
        ));
    };
    thread::sleep(remaining.min(TERMINFO_CACHE_LOCK_POLL_INTERVAL));
    Ok(())
}

fn remove_stale_terminfo_staging_directories(cache: &Path) -> io::Result<()> {
    for entry in std::fs::read_dir(cache)? {
        let entry = entry?;
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(".db-"))
        {
            remove_path(&entry.path())?;
        }
    }
    Ok(())
}

fn remove_path(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn private_directory_builder() -> DirBuilder {
    let mut builder = DirBuilder::new();
    builder.mode(0o700);
    builder
}

fn reset_private_directory(path: &Path) -> io::Result<()> {
    remove_path(path)?;
    private_directory_builder().create(path)
}

fn write_terminfo_marker(directory: &Path) -> io::Result<()> {
    std::fs::write(directory.join(".lector-sync"), SYNC_TERMINFO_MARKER)
}

fn terminfo_cache_component() -> &'static str {
    TERMINFO_CACHE_COMPONENT.get_or_init(|| {
        format!(
            "{}-{SYNC_TERMINFO_CACHE_VERSION}-{}-{}-{}-{}-{:016x}",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH,
            target_abi(),
            if cfg!(target_endian = "little") {
                "little"
            } else {
                "big"
            },
            terminfo_content_digest()
        )
    })
}

fn target_abi() -> &'static str {
    if cfg!(target_env = "musl") {
        "musl"
    } else if cfg!(target_env = "gnu") {
        "gnu"
    } else {
        "native"
    }
}

fn terminfo_content_digest() -> u64 {
    const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = FNV_OFFSET_BASIS;
    for bytes in [
        SYNC_TERMINFO_CACHE_VERSION.as_bytes(),
        SYNC_TERMINFO_CAPABILITY.as_bytes(),
        SYNC_TERMINFO_MARKER.as_bytes(),
        BUNDLED_SYNC_TERMINFO,
    ] {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        hash ^= u64::from(b'|');
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn build_host_terminfo(staging: &Path) -> io::Result<()> {
    let source_path = staging.join("lector-xterm-256color.ti");
    let mut infocmp = Command::new("infocmp");
    infocmp
        .env_remove("TERMINFO")
        .env_remove("TERMINFO_DIRS")
        .args(["-x", VIRTUAL_TERM]);
    let infocmp_output = run_bounded_output(&mut infocmp, staging, "infocmp")?;
    if !infocmp_output.status.success() {
        return Err(host_terminfo_tool_failure("infocmp", &infocmp_output));
    }
    let mut source = infocmp_output.stdout;
    if !String::from_utf8_lossy(&source)
        .lines()
        .any(|line| line.trim_start().starts_with("Sync="))
    {
        source.extend_from_slice(SYNC_TERMINFO_CAPABILITY.as_bytes());
    }
    std::fs::write(&source_path, source)?;
    let mut tic = Command::new("tic");
    tic.env_remove("TERMINFO")
        .env_remove("TERMINFO_DIRS")
        .arg("-x")
        .arg("-o")
        .arg(staging)
        .arg(&source_path);
    let tic_output = run_bounded_output(&mut tic, staging, "tic")?;
    if !tic_output.status.success() {
        return Err(host_terminfo_tool_failure("tic", &tic_output));
    }
    let _ = std::fs::remove_file(&source_path);
    if !compiled_terminfo_advertises_sync(staging) {
        return Err(io::Error::other(
            "host tic output does not advertise synchronized output",
        ));
    }
    Ok(())
}

fn write_bundled_terminfo(staging: &Path) -> io::Result<()> {
    let entry = decoded_bundled_sync_terminfo()?;
    // ncurses installations use either the initial character or its two-digit
    // hexadecimal value as the first directory component. Supplying both
    // layouts makes the same private entry work on macOS, glibc Linux, and
    // musl Linux without invoking tic on the target machine.
    for directory in ["x", "78"] {
        let directory = staging.join(directory);
        std::fs::create_dir(&directory)?;
        std::fs::write(directory.join(VIRTUAL_TERM), entry)?;
    }
    Ok(())
}

fn decoded_bundled_sync_terminfo() -> io::Result<&'static [u8]> {
    if let Some(entry) = DECODED_BUNDLED_SYNC_TERMINFO.get() {
        return Ok(entry);
    }
    let decoded = decode_base64(BUNDLED_SYNC_TERMINFO)?;
    let _ = DECODED_BUNDLED_SYNC_TERMINFO.set(decoded);
    Ok(DECODED_BUNDLED_SYNC_TERMINFO
        .get()
        .expect("decoded bundled terminfo was initialized"))
}

fn decode_base64(encoded: &[u8]) -> io::Result<Vec<u8>> {
    fn digit(byte: u8) -> Option<u8> {
        match byte {
            b'A'..=b'Z' => Some(byte - b'A'),
            b'a'..=b'z' => Some(byte - b'a' + 26),
            b'0'..=b'9' => Some(byte - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }

    let digits = encoded
        .iter()
        .copied()
        .filter(|byte| !byte.is_ascii_whitespace())
        .map(|byte| {
            digit(byte).ok_or_else(|| {
                io::Error::new(ErrorKind::InvalidData, "invalid bundled terminfo base64")
            })
        })
        .collect::<io::Result<Vec<_>>>()?;
    if digits.len() % 4 == 1 {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "truncated bundled terminfo base64",
        ));
    }

    let mut decoded = Vec::with_capacity(digits.len().saturating_mul(3) / 4);
    for chunk in digits.chunks(4) {
        decoded.push((chunk[0] << 2) | (chunk[1] >> 4));
        if let Some(third) = chunk.get(2) {
            decoded.push((chunk[1] << 4) | (third >> 2));
            if let Some(fourth) = chunk.get(3) {
                decoded.push((third << 6) | fourth);
            }
        }
    }
    Ok(decoded)
}

fn installed_terminfo_ready(terminfo: &Path) -> bool {
    std::fs::read_to_string(terminfo.join(".lector-sync"))
        .is_ok_and(|marker| marker == SYNC_TERMINFO_MARKER)
        && compiled_terminfo_advertises_sync(terminfo)
}

fn compiled_terminfo_advertises_sync(terminfo: &Path) -> bool {
    ["x", "78"].iter().any(|directory| {
        read_bounded_file(
            &terminfo.join(directory).join(VIRTUAL_TERM),
            HOST_TOOL_STDOUT_LIMIT,
        )
        .is_ok_and(|entry| {
            entry
                .windows(COMPILED_SYNC_NAME.len())
                .any(|window| window == COMPILED_SYNC_NAME)
                && entry
                    .windows(COMPILED_SYNC_VALUE.len())
                    .any(|window| window == COMPILED_SYNC_VALUE)
        })
    })
}

fn host_terminfo_tool_failure(name: &str, output: &BoundedOutput) -> io::Error {
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if detail.is_empty() {
        io::Error::other(format!("{name} failed with {}", output.status))
    } else {
        io::Error::other(format!("{name} failed with {}: {detail}", output.status))
    }
}

pub struct Process {
    master: Box<dyn MasterPty + Send>,
    child: Option<Box<dyn Child + Send + Sync>>,
    _temporary_terminfo: Option<Arc<TemporaryDirectory>>,
}

impl Process {
    pub fn spawn<I, S>(
        program: &Path,
        args: I,
        rows: u16,
        cols: u16,
        terminal_attrs: &termios::Termios,
    ) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Self::spawn_with_geometry(
            program,
            args,
            TerminalGeometry::from_cells(rows, cols),
            terminal_attrs,
        )
    }

    pub fn spawn_with_geometry<I, S>(
        program: &Path,
        args: I,
        geometry: TerminalGeometry,
        terminal_attrs: &termios::Termios,
    ) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Self::spawn_with_geometry_and_environment(program, args, geometry, terminal_attrs, None)
    }

    pub fn spawn_with_geometry_and_environment<I, S>(
        program: &Path,
        args: I,
        geometry: TerminalGeometry,
        terminal_attrs: &termios::Termios,
        environment: Option<&VirtualTerminalEnvironment>,
    ) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let current_dir = std::env::current_dir().context("resolve current directory")?;
        let pair = native_pty_system()
            .openpty(pty_size(geometry))
            .context("open PTY")?;
        let master_fd = pair
            .master
            .as_raw_fd()
            .ok_or_else(|| anyhow!("PTY master does not expose a file descriptor"))?;
        termios::tcsetattr(
            unsafe { BorrowedFd::borrow_raw(master_fd) },
            termios::SetArg::TCSANOW,
            terminal_attrs,
        )
        .context("copy terminal settings to PTY")?;

        let mut command = CommandBuilder::new(program);
        command.args(args);
        command.cwd(current_dir);
        let temporary_terminfo = environment
            .and_then(|environment| environment.terminfo.as_ref())
            .and_then(TerminfoDirectory::temporary_owner);
        if let Some(environment) = environment {
            environment.apply(&mut command);
        }
        let child = pair
            .slave
            .spawn_command(command)
            .context("spawn PTY child")?;
        drop(pair.slave);

        Ok(Self {
            master: pair.master,
            child: Some(child),
            _temporary_terminfo: temporary_terminfo,
        })
    }

    pub fn stream(&self) -> Result<PtyStream> {
        let master_fd = self
            .master
            .as_raw_fd()
            .ok_or_else(|| anyhow!("PTY master does not expose a file descriptor"))?;
        let stream_fd = dup(master_fd).context("duplicate PTY master")?;
        Ok(PtyStream {
            inner: unsafe { File::from_raw_fd(stream_fd) },
            pending_write: VecDeque::new(),
            dropped_write_bytes: 0,
        })
    }

    pub fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        self.resize_with_geometry(TerminalGeometry::from_cells(rows, cols))
    }

    pub fn resize_with_geometry(&self, geometry: TerminalGeometry) -> Result<()> {
        self.master
            .resize(pty_size(geometry))
            .context("resize PTY")?;
        if let Some(pid) = self
            .child
            .as_ref()
            .and_then(|child| child.process_id())
            .and_then(|pid| i32::try_from(pid).ok())
        {
            kill(Pid::from_raw(pid), Signal::SIGWINCH).context("notify PTY child of resize")?;
        }
        Ok(())
    }

    pub fn wait(&mut self) -> Result<portable_pty::ExitStatus> {
        let child = self
            .child
            .as_mut()
            .ok_or_else(|| anyhow!("PTY child has already been reaped"))?;
        let status = child.wait().context("wait for PTY child")?;
        self.child = None;
        Ok(status)
    }

    pub fn terminate(&mut self) {
        self.terminate_with_timeout(CHILD_TERMINATION_REAP_TIMEOUT);
    }

    /// Kill the child process group and spend no longer than `reap_timeout`
    /// checking for the direct child to exit.
    pub fn terminate_with_timeout(&mut self, reap_timeout: Duration) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        terminate_child(
            &mut *child,
            self.master.process_group_leader(),
            reap_timeout,
        );
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.terminate();
    }
}

fn terminate_child(
    child: &mut dyn Child,
    foreground_process_group: Option<nix::libc::pid_t>,
    reap_timeout: Duration,
) {
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => break,
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }

    let child_pid = child
        .process_id()
        .and_then(|pid| nix::libc::pid_t::try_from(pid).ok());

    // portable-pty makes the direct child a session and process-group leader.
    // A shell may give the foreground PTY to a different job group, so signal
    // both groups before falling back to the direct PID. This prevents a job
    // which inherited the slave PTY from surviving Lector's cleanup.
    if let Some(group) = foreground_process_group {
        kill_process_group(group);
    }
    if let Some(pid) = child_pid {
        if Some(pid) != foreground_process_group {
            kill_process_group(pid);
        }
        let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
    }

    reap_child_until(child, reap_timeout);
}

fn kill_process_group(group: nix::libc::pid_t) {
    if group > 0 && group != nix::unistd::getpgrp().as_raw() {
        let _ = kill(Pid::from_raw(-group), Signal::SIGKILL);
    }
}

fn reap_child_until(child: &mut dyn Child, timeout: Duration) {
    let deadline = Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(Instant::now);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => {}
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(_) => return,
        }

        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return;
        };
        thread::sleep(remaining.min(CHILD_TERMINATION_REAP_POLL_INTERVAL));
    }
}

fn pty_size(geometry: TerminalGeometry) -> PtySize {
    PtySize {
        rows: geometry.rows,
        cols: geometry.cols,
        pixel_width: geometry.width_px().try_into().unwrap_or(u16::MAX),
        pixel_height: geometry.height_px().try_into().unwrap_or(u16::MAX),
    }
}

/// Reads cell and pixel geometry from a terminal file descriptor.
pub fn terminal_geometry(fd: RawFd) -> Result<TerminalGeometry> {
    let mut size = std::mem::MaybeUninit::<nix::libc::winsize>::zeroed();
    // SAFETY: `size` points to writable `winsize` storage and `fd` is borrowed
    // only for this ioctl. Success guarantees the structure was initialized.
    let result = unsafe { nix::libc::ioctl(fd, nix::libc::TIOCGWINSZ, size.as_mut_ptr()) };
    Errno::result(result).context("read terminal geometry")?;
    // SAFETY: the successful ioctl initialized the complete `winsize`.
    let size = unsafe { size.assume_init() };
    if size.ws_row == 0 || size.ws_col == 0 {
        return Err(anyhow!("terminal reported zero cell dimensions"));
    }
    Ok(TerminalGeometry::from_grid_pixels(
        size.ws_row,
        size.ws_col,
        u32::from(size.ws_xpixel),
        u32::from(size.ws_ypixel),
    ))
}

pub struct PtyStream {
    inner: File,
    pending_write: VecDeque<u8>,
    dropped_write_bytes: usize,
}

const PTY_WRITE_BUFFER_LIMIT: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PtyWriteReport {
    pub bytes_written: usize,
    pub blocked: bool,
}

impl PtyStream {
    #[must_use]
    pub fn pending_write_bytes(&self) -> usize {
        self.pending_write.len()
    }

    #[must_use]
    pub fn has_pending_writes(&self) -> bool {
        !self.pending_write.is_empty()
    }

    pub fn take_dropped_write_bytes(&mut self) -> usize {
        std::mem::take(&mut self.dropped_write_bytes)
    }

    /// Make bounded progress on bytes previously accepted from the app. A
    /// blocked child is a readiness condition, not an application error.
    pub fn drain_pending_writes(&mut self) -> io::Result<PtyWriteReport> {
        let mut report = PtyWriteReport::default();
        while !self.pending_write.is_empty() {
            let contiguous = self.pending_write.make_contiguous();
            match self.inner.write(contiguous) {
                Ok(0) => {
                    report.blocked = true;
                    break;
                }
                Ok(count) => {
                    self.pending_write.drain(..count);
                    report.bytes_written = report.bytes_written.saturating_add(count);
                    log_child_pty_write(count, self.pending_write.len());
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    report.blocked = true;
                    break;
                }
                Err(error) => return Err(error),
            }
        }
        Ok(report)
    }

    fn accept_write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let _ = self.drain_pending_writes()?;
        if self.pending_write.is_empty() {
            match self.inner.write(bytes) {
                Ok(count) if count == bytes.len() => {
                    log_child_pty_write(count, 0);
                    return Ok(count);
                }
                Ok(count) => {
                    let accepted = self.queue_remainder(bytes, count);
                    log_child_pty_write(count, self.pending_write.len());
                    return Ok(accepted);
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error) if error.kind() == ErrorKind::WouldBlock => {}
                Err(error) => return Err(error),
            }
        }
        Ok(self.queue_remainder(bytes, 0))
    }

    fn queue_remainder(&mut self, bytes: &[u8], written: usize) -> usize {
        let remainder = &bytes[written..];
        let capacity = PTY_WRITE_BUFFER_LIMIT.saturating_sub(self.pending_write.len());
        let retained = capacity.min(remainder.len());
        self.pending_write.extend(&remainder[..retained]);
        self.dropped_write_bytes = self
            .dropped_write_bytes
            .saturating_add(remainder.len().saturating_sub(retained));
        // Report logical acceptance so existing write_all callers remain
        // responsive. The event loop observes overflow and terminates only the
        // failed child transport before later protocol commands can overtake it.
        bytes.len()
    }
}

fn log_child_pty_write(bytes: usize, pending: usize) {
    if bytes != 0 && crate::diagnostics::enabled() {
        crate::diagnostics::event(
            "latency",
            "child-pty-write",
            &format!("bytes={bytes} pending={pending}"),
        );
    }
}

impl Read for PtyStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.inner.read(buf) {
            Err(err) if err.raw_os_error() == Some(Errno::EIO as i32) => Ok(0),
            result => result,
        }
    }
}

impl Write for PtyStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.accept_write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        let _ = self.drain_pending_writes()?;
        if self.pending_write.is_empty() {
            self.inner.flush()
        } else {
            Ok(())
        }
    }

    fn write_vectored(&mut self, bufs: &[io::IoSlice<'_>]) -> io::Result<usize> {
        let mut accepted = 0_usize;
        for buffer in bufs {
            self.accept_write(buffer)?;
            accepted = accepted.saturating_add(buffer.len());
        }
        Ok(accepted)
    }
}

impl AsRawFd for PtyStream {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

pub fn set_raw(fd: RawFd) -> Result<()> {
    let fd = unsafe { BorrowedFd::borrow_raw(fd) };
    let mut attrs = termios::tcgetattr(fd).context("read terminal settings")?;
    termios::cfmakeraw(&mut attrs);
    termios::tcsetattr(fd, termios::SetArg::TCSANOW, &attrs)
        .context("apply raw terminal settings")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        PTY_WRITE_BUFFER_LIMIT, Process, PtyStream, TerminfoDirectory, VIRTUAL_TERM,
        VirtualTerminalEnvironment, acquire_terminfo_cache_lock, compatible_terminal_environment,
        compiled_terminfo_advertises_sync, install_synchronized_output_terminfo_with, set_raw,
        synchronized_output_terminfo_with_cache, synchronized_output_terminfo_with_cache_options,
        target_abi, terminal_geometry, terminate_child, terminfo_cache_component,
        terminfo_content_digest,
    };
    use crate::terminal::TerminalGeometry;
    use nix::fcntl::{FcntlArg, OFlag, fcntl};
    use nix::sys::termios::{self, LocalFlags};
    use nix::unistd::pipe;
    use portable_pty::{
        Child, ChildKiller, CommandBuilder, ExitStatus, PtySize, native_pty_system,
    };
    use std::ffi::OsStr;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::fd::{AsRawFd, BorrowedFd};
    use std::path::Path;
    use std::sync::{
        Arc, Barrier, Mutex, MutexGuard,
        atomic::{AtomicUsize, Ordering},
    };
    use std::thread;
    use std::time::{Duration, Instant};

    // macOS has a finite system PTY pool. These tests each hold a real PTY
    // while a child runs, and concurrent test processes may already consume
    // much of that pool. Keeping this module's live-PTY cases serialized
    // prevents unrelated assertions from observing an early/empty child when
    // the test harness runs them in parallel. Recover a poisoned lock so one
    // useful failure does not turn every remaining PTY test into noise.
    static REAL_PTY_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn serialize_real_pty_test() -> MutexGuard<'static, ()> {
        REAL_PTY_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn terminal_attrs() -> termios::Termios {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open reference PTY");
        let fd = pair.master.as_raw_fd().expect("reference PTY raw fd");
        termios::tcgetattr(unsafe { BorrowedFd::borrow_raw(fd) })
            .expect("read reference terminal attributes")
    }

    #[derive(Debug)]
    struct NoopChildKiller;

    impl ChildKiller for NoopChildKiller {
        fn kill(&mut self) -> std::io::Result<()> {
            Ok(())
        }

        fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
            Box::new(Self)
        }
    }

    #[derive(Debug)]
    struct NeverExitsChild {
        try_wait_calls: Arc<AtomicUsize>,
    }

    impl ChildKiller for NeverExitsChild {
        fn kill(&mut self) -> std::io::Result<()> {
            Ok(())
        }

        fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
            Box::new(NoopChildKiller)
        }
    }

    impl Child for NeverExitsChild {
        fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
            self.try_wait_calls.fetch_add(1, Ordering::Relaxed);
            Ok(None)
        }

        fn wait(&mut self) -> std::io::Result<ExitStatus> {
            panic!("bounded PTY cleanup must never call blocking Child::wait")
        }

        fn process_id(&self) -> Option<u32> {
            None
        }
    }

    #[test]
    fn child_cleanup_never_uses_an_unbounded_wait() {
        let try_wait_calls = Arc::new(AtomicUsize::new(0));
        let mut child = NeverExitsChild {
            try_wait_calls: Arc::clone(&try_wait_calls),
        };
        let reap_window = Duration::from_millis(10);
        let started = Instant::now();

        terminate_child(&mut child, None, reap_window);

        assert!(try_wait_calls.load(Ordering::Relaxed) >= 2);
        assert!(started.elapsed() >= reap_window);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "short reap window was not bounded: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn compatible_environment_keeps_the_public_xterm_identity() {
        let environment = compatible_terminal_environment();
        let mut command = CommandBuilder::new("/bin/sh");
        command.env("TERMINFO", "/stale/private/terminfo");

        environment.apply(&mut command);

        assert_eq!(command.get_env("TERM"), Some(OsStr::new("xterm-256color")));
        assert_eq!(
            command.get_env("TERMINFO"),
            environment
                .terminfo
                .as_ref()
                .map(TerminfoDirectory::path)
                .map(Path::as_os_str)
        );
        assert_eq!(command.get_env("TERM_PROGRAM"), Some(OsStr::new("Lector")));
    }

    #[test]
    fn private_terminfo_overlay_advertises_synchronized_output() {
        let cache = tempfile::tempdir().expect("create terminfo test cache");
        let terminfo = install_synchronized_output_terminfo_with(cache.path(), true)
            .expect("compile synchronized-output terminfo");
        assert!(compiled_terminfo_advertises_sync(&terminfo));
    }

    #[test]
    fn bundled_terminfo_needs_no_host_compiler_or_system_entry() {
        let cache = tempfile::tempdir().expect("create bundled terminfo test cache");
        let terminfo = install_synchronized_output_terminfo_with(cache.path(), false)
            .expect("extract bundled synchronized-output terminfo");

        assert!(terminfo.join("x/xterm-256color").is_file());
        assert!(terminfo.join("78/xterm-256color").is_file());
        assert!(compiled_terminfo_advertises_sync(&terminfo));
    }

    #[test]
    fn terminfo_cache_key_names_the_target_and_embedded_content() {
        let component = terminfo_cache_component();

        assert!(component.contains(std::env::consts::OS));
        assert!(component.contains(std::env::consts::ARCH));
        assert!(component.contains(target_abi()));
        assert!(component.ends_with(&format!("{:016x}", terminfo_content_digest())));
    }

    #[test]
    fn concurrent_callers_repair_one_incomplete_cache_entry() {
        let root = tempfile::tempdir().expect("create concurrent terminfo cache");
        let cache = Arc::new(root.path().join("cache"));
        let incomplete = cache.join("db");
        std::fs::create_dir_all(incomplete.join("x")).expect("create incomplete cache entry");
        std::fs::write(incomplete.join(".lector-sync"), super::SYNC_TERMINFO_MARKER)
            .expect("write premature cache marker");
        std::fs::write(incomplete.join("x/xterm-256color"), b"incomplete")
            .expect("write incomplete terminfo entry");

        let callers = 8;
        let barrier = Arc::new(Barrier::new(callers));
        let workers = (0..callers)
            .map(|_| {
                let cache = Arc::clone(&cache);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    install_synchronized_output_terminfo_with(&cache, false)
                })
            })
            .collect::<Vec<_>>();

        for worker in workers {
            let installed = worker
                .join()
                .expect("terminfo installer thread panicked")
                .expect("concurrent terminfo installation failed");
            assert_eq!(installed, cache.join("db"));
            assert!(compiled_terminfo_advertises_sync(&installed));
        }
        assert!(
            std::fs::read_dir(&*cache)
                .expect("read repaired cache")
                .all(|entry| !entry
                    .expect("read cache entry")
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".db-"))
        );
    }

    #[test]
    fn contended_cache_lock_falls_back_within_its_deadline() {
        let root = tempfile::tempdir().expect("create contended terminfo cache");
        let cache = root.path().join("cache");
        std::fs::create_dir_all(&cache).expect("create cache directory");
        let held = acquire_terminfo_cache_lock(&cache, Duration::from_millis(20))
            .expect("hold cache lock");
        let started = Instant::now();

        let terminfo = synchronized_output_terminfo_with_cache_options(
            &cache,
            false,
            Duration::from_millis(20),
        )
        .expect("fall back from a contended cache lock");

        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(matches!(&terminfo, TerminfoDirectory::Temporary(_)));
        assert!(compiled_terminfo_advertises_sync(terminfo.path()));
        drop(held);
    }

    #[test]
    fn unusable_cache_uses_a_temporary_entry() {
        let root = tempfile::tempdir().expect("create blocked terminfo cache");
        let blocker = root.path().join("not-a-directory");
        std::fs::write(&blocker, b"block cache creation").expect("write cache blocker");

        let terminfo = synchronized_output_terminfo_with_cache(&blocker.join("cache"))
            .expect("fall back from unusable cache path");

        assert!(matches!(&terminfo, TerminfoDirectory::Temporary(_)));
        assert!(compiled_terminfo_advertises_sync(terminfo.path()));
    }

    #[test]
    fn nonblocking_pty_writes_are_buffered_and_overflow_is_reported_not_returned() {
        let (_read, write) = pipe().expect("create PTY-write stand-in pipe");
        let original = OFlag::from_bits_truncate(
            fcntl(write.as_raw_fd(), FcntlArg::F_GETFL).expect("read pipe flags"),
        );
        fcntl(
            write.as_raw_fd(),
            FcntlArg::F_SETFL(original | OFlag::O_NONBLOCK),
        )
        .expect("make pipe nonblocking");
        let mut stream = PtyStream {
            inner: std::fs::File::from(write),
            pending_write: std::collections::VecDeque::new(),
            dropped_write_bytes: 0,
        };
        let input = vec![b'x'; PTY_WRITE_BUFFER_LIMIT * 2];

        stream
            .write_all(&input)
            .expect("backpressure is queued rather than returned as EAGAIN");
        assert_eq!(stream.pending_write_bytes(), PTY_WRITE_BUFFER_LIMIT);
        assert!(stream.take_dropped_write_bytes() > 0);
        let report = stream
            .drain_pending_writes()
            .expect("retry a still-backpressured write");
        assert!(report.blocked);
        assert_eq!(stream.pending_write_bytes(), PTY_WRITE_BUFFER_LIMIT);
    }

    #[test]
    fn process_stream_is_duplex_and_reports_eof_after_child_exit() {
        let _guard = serialize_real_pty_test();
        let attrs = terminal_attrs();
        let mut process = Process::spawn(
            Path::new("/bin/sh"),
            ["-c", "stty size; read value; printf 'got:%s\\n' \"$value\""],
            7,
            19,
            &attrs,
        )
        .expect("spawn PTY child");
        let mut stream = process.stream().expect("clone PTY stream");

        stream.write_all(b"hello\n").expect("write PTY input");
        let mut output = String::new();
        stream
            .read_to_string(&mut output)
            .expect("read child output through EOF");

        assert!(output.contains("7 19"), "{output:?}");
        assert!(output.contains("got:hello"), "{output:?}");
        assert!(process.wait().expect("wait for child").success());
    }

    #[test]
    fn resize_updates_the_child_terminal_dimensions() {
        let _guard = serialize_real_pty_test();
        let attrs = terminal_attrs();
        let mut process = Process::spawn(
            Path::new("/bin/sh"),
            ["-c", "stty size; read _; stty size"],
            5,
            13,
            &attrs,
        )
        .expect("spawn PTY child");
        let stream = process.stream().expect("clone PTY stream");
        let mut stream = BufReader::new(stream);
        let mut first_size = String::new();
        stream
            .read_line(&mut first_size)
            .expect("read initial terminal size");

        process
            .resize_with_geometry(TerminalGeometry::new(11, 23, 9, 18))
            .expect("resize PTY");
        stream
            .get_mut()
            .write_all(b"\n")
            .expect("release child after resize");
        let mut remaining = String::new();
        stream
            .read_to_string(&mut remaining)
            .expect("read resized terminal size");

        assert_eq!(first_size.trim(), "5 13");
        assert!(remaining.contains("11 23"), "{remaining:?}");
        assert!(process.wait().expect("wait for child").success());
    }

    #[test]
    fn resize_notifies_the_child_process() {
        let _guard = serialize_real_pty_test();
        let attrs = terminal_attrs();
        let mut process = Process::spawn(
            &std::env::current_exe().expect("resolve test binary"),
            [
                "--ignored",
                "--exact",
                "pty::tests::resize_signal_probe",
                "--nocapture",
            ],
            5,
            13,
            &attrs,
        )
        .expect("spawn PTY child");
        let stream = process.stream().expect("clone PTY stream");
        let mut stream = BufReader::new(stream);
        let mut startup = String::new();
        for _ in 0..20 {
            let mut line = String::new();
            stream.read_line(&mut line).expect("read readiness marker");
            startup.push_str(&line);
            if line.contains("PTY_SIGNAL_READY") {
                break;
            }
        }
        assert!(startup.contains("PTY_SIGNAL_READY"), "{startup:?}");

        process
            .resize_with_geometry(TerminalGeometry::new(11, 23, 9, 18))
            .expect("resize PTY");
        let mut remaining = String::new();
        stream
            .read_to_string(&mut remaining)
            .expect("read resize notification output");

        assert!(
            remaining.contains("PTY_SIGNAL:11 23 207 198"),
            "{remaining:?}"
        );
        assert!(process.wait().expect("wait for child").success());
    }

    #[test]
    #[ignore = "helper process for resize_notifies_the_child_process"]
    fn resize_signal_probe() {
        let mut signals = signal_hook::iterator::Signals::new([signal_hook::consts::SIGWINCH])
            .expect("install SIGWINCH handler");

        println!("PTY_SIGNAL_READY");
        std::io::stdout().flush().expect("flush readiness marker");
        assert_eq!(
            signals.forever().next(),
            Some(signal_hook::consts::SIGWINCH)
        );
        let geometry = terminal_geometry(std::io::stdin().as_raw_fd())
            .expect("read resized terminal geometry");
        println!(
            "PTY_SIGNAL:{} {} {} {}",
            geometry.rows,
            geometry.cols,
            geometry.width_px(),
            geometry.height_px()
        );
    }

    #[test]
    fn spawn_preserves_the_callers_current_directory() {
        let _guard = serialize_real_pty_test();
        let attrs = terminal_attrs();
        let mut process = Process::spawn(
            Path::new("/bin/pwd"),
            std::iter::empty::<&str>(),
            5,
            13,
            &attrs,
        )
        .expect("spawn PTY child");
        let mut stream = process.stream().expect("clone PTY stream");
        let mut output = String::new();
        stream
            .read_to_string(&mut output)
            .expect("read child output");

        let actual = Path::new(output.trim())
            .canonicalize()
            .expect("canonicalize child directory");
        let expected = std::env::current_dir()
            .expect("resolve current directory")
            .canonicalize()
            .expect("canonicalize current directory");
        assert_eq!(actual, expected);
        assert!(process.wait().expect("wait for child").success());
    }

    #[test]
    fn spawn_applies_the_compatible_terminal_environment_to_the_real_child() {
        let _guard = serialize_real_pty_test();
        let attrs = terminal_attrs();
        let cache = tempfile::tempdir().expect("create terminfo test cache");
        let terminfo = install_synchronized_output_terminfo_with(cache.path(), true)
            .expect("compile synchronized-output terminfo");
        let environment = VirtualTerminalEnvironment {
            term: VIRTUAL_TERM.to_owned(),
            terminfo: Some(TerminfoDirectory::Persistent(terminfo)),
        };
        let mut process = Process::spawn_with_geometry_and_environment(
            Path::new("/bin/sh"),
            [
                "-c",
                "printf '%s|%s|%s\\n' \"$TERM\" \"${TERMINFO:+set}\" \"$TERM_PROGRAM\"",
            ],
            TerminalGeometry::new(5, 13, 8, 16),
            &attrs,
            Some(&environment),
        )
        .expect("spawn child with virtual terminal environment");
        let mut stream = process.stream().expect("clone PTY stream");
        let mut output = String::new();
        stream
            .read_to_string(&mut output)
            .expect("read child environment");

        assert!(output.contains("xterm-256color|set|Lector"), "{output:?}");
        assert!(process.wait().expect("wait for child").success());
    }

    #[test]
    fn process_keeps_a_temporary_terminfo_entry_alive_for_its_child() {
        let _guard = serialize_real_pty_test();
        let attrs = terminal_attrs();
        let root = tempfile::tempdir().expect("create blocked terminfo cache");
        let blocker = root.path().join("not-a-directory");
        std::fs::write(&blocker, b"block cache creation").expect("write cache blocker");
        let terminfo = synchronized_output_terminfo_with_cache(&blocker.join("cache"))
            .expect("create temporary terminfo fallback");
        let temporary_path = terminfo.path().to_owned();
        assert!(matches!(&terminfo, TerminfoDirectory::Temporary(_)));
        let environment = VirtualTerminalEnvironment {
            term: VIRTUAL_TERM.to_owned(),
            terminfo: Some(terminfo),
        };
        let mut process = Process::spawn_with_geometry_and_environment(
            Path::new("/bin/sh"),
            [
                "-c",
                "read _; test -r \"$TERMINFO/x/xterm-256color\" && printf 'TERMINFO_ALIVE\\n'",
            ],
            TerminalGeometry::new(5, 13, 8, 16),
            &attrs,
            Some(&environment),
        )
        .expect("spawn child with temporary terminfo");
        let mut stream = process.stream().expect("clone PTY stream");

        drop(environment);
        assert!(temporary_path.is_dir());
        stream.write_all(b"continue\n").expect("release child");
        let mut output = String::new();
        stream
            .read_to_string(&mut output)
            .expect("read temporary terminfo check");

        assert!(output.contains("TERMINFO_ALIVE"), "{output:?}");
        assert!(process.wait().expect("wait for child").success());
        assert!(temporary_path.is_dir());
        drop(process);
        assert!(!temporary_path.exists());
    }

    #[test]
    fn spawn_copies_the_requested_terminal_attributes() {
        let _guard = serialize_real_pty_test();
        let mut attrs = terminal_attrs();
        attrs.local_flags.remove(LocalFlags::ECHO);
        let mut process = Process::spawn(Path::new("/bin/sh"), ["-c", "stty -a"], 5, 13, &attrs)
            .expect("spawn PTY child");
        let mut stream = process.stream().expect("clone PTY stream");
        let mut output = String::new();
        stream
            .read_to_string(&mut output)
            .expect("read terminal attributes");

        assert!(
            output.split_whitespace().any(|flag| flag == "-echo"),
            "{output:?}"
        );
        assert!(process.wait().expect("wait for child").success());
    }

    #[test]
    fn spawn_reports_a_missing_program_before_returning_a_process() {
        let _guard = serialize_real_pty_test();
        let attrs = terminal_attrs();
        let err = Process::spawn(
            Path::new("/definitely/missing/lector-pty-test"),
            std::iter::empty::<&str>(),
            5,
            13,
            &attrs,
        )
        .err()
        .expect("missing program must fail");

        let message = format!("{err:#}");
        assert!(message.contains("spawn PTY child"), "{message}");
        assert!(message.contains("doesn't exist"), "{message}");
    }

    #[test]
    fn raw_mode_disables_canonical_input_echo_and_terminal_signals() {
        let _guard = serialize_real_pty_test();
        let pair = native_pty_system()
            .openpty(PtySize::default())
            .expect("open PTY");
        let fd = pair.master.as_raw_fd().expect("PTY raw fd");

        set_raw(fd).expect("set PTY raw mode");

        let attrs = termios::tcgetattr(unsafe { BorrowedFd::borrow_raw(fd) })
            .expect("read raw terminal attributes");
        assert!(!attrs.local_flags.contains(LocalFlags::ECHO));
        assert!(!attrs.local_flags.contains(LocalFlags::ICANON));
        assert!(!attrs.local_flags.contains(LocalFlags::ISIG));
    }
}
