use crate::terminal::TerminalGeometry;
use anyhow::{Context, Result, anyhow};
use nix::errno::Errno;
use nix::sys::{
    signal::{Signal, kill},
    termios,
};
use nix::unistd::{Pid, dup};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, RawFd};
use std::path::Path;

const LECTOR_TERMINFO: &[u8] = include_bytes!("../terminfo/compiled/6c/lector");

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VirtualTerminalEnvironment {
    pub term: String,
    pub terminfo_dir: std::path::PathBuf,
}

impl VirtualTerminalEnvironment {
    pub fn apply(&self, command: &mut CommandBuilder) {
        command.env("TERM", &self.term);
        command.env("TERMINFO", &self.terminfo_dir);
        // COLORTERM is deliberately untouched: callers preserve it only when
        // the selected virtual profile implements the advertised color mode.
    }
}

/// Materialize the bundled compiled terminfo under both directory conventions
/// used by supported ncurses implementations (hex and first-character).
pub fn install_lector_terminfo(root: &Path) -> Result<VirtualTerminalEnvironment> {
    for relative in ["6c/lector", "l/lector"] {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).context("create Lector terminfo directory")?;
        }
        let current = fs::read(&path).ok();
        if current.as_deref() != Some(LECTOR_TERMINFO) {
            fs::write(&path, LECTOR_TERMINFO).context("write bundled Lector terminfo")?;
        }
    }
    Ok(VirtualTerminalEnvironment {
        term: "lector".to_owned(),
        terminfo_dir: root.to_path_buf(),
    })
}

pub struct Process {
    master: Box<dyn MasterPty + Send>,
    child: Option<Box<dyn Child + Send + Sync>>,
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
        let Some(mut child) = self.child.take() else {
            return;
        };
        if let Some(pid) = child.process_id().and_then(|pid| i32::try_from(pid).ok()) {
            let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
        }
        let _ = child.wait();
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.terminate();
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
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }

    fn write_vectored(&mut self, bufs: &[io::IoSlice<'_>]) -> io::Result<usize> {
        self.inner.write_vectored(bufs)
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
    use super::{Process, install_lector_terminfo, set_raw, terminal_geometry};
    use crate::terminal::TerminalGeometry;
    use nix::sys::termios::{self, LocalFlags};
    use portable_pty::{PtySize, native_pty_system};
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::fd::{AsRawFd, BorrowedFd};
    use std::path::Path;

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

    #[test]
    fn process_stream_is_duplex_and_reports_eof_after_child_exit() {
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
    fn spawn_applies_the_virtual_terminal_environment_to_the_real_child() {
        let attrs = terminal_attrs();
        let root = Path::new("target/test-artifacts/pty-lector-terminfo");
        let environment = install_lector_terminfo(root).expect("install bundled terminfo");
        let mut process = Process::spawn_with_geometry_and_environment(
            Path::new("/bin/sh"),
            ["-c", "printf '%s|%s\\n' \"$TERM\" \"$TERMINFO\""],
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

        assert!(
            output.contains(&format!("lector|{}", root.display())),
            "{output:?}"
        );
        assert!(process.wait().expect("wait for child").success());
    }

    #[test]
    fn spawn_copies_the_requested_terminal_attributes() {
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
