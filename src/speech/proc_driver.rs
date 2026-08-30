use super::{
    manager::Host,
    protocol::{
        AcceptedResult, ClientCapabilities, ControlCapabilities, MAX_JSON_SAFE_INTEGER,
        PauseResult, ProtocolRange, SettingCapabilities, SettingSupport, SpeechCapabilities,
        SpeechEventNotification, StopSupport, UtteranceId, UtteranceParams,
    },
};
use crate::proc_server_common::{
    InitializeParams, InitializeResult, MAX_RPC_FRAME_BYTES, PeerInfo, SPEECH_PROTOCOL_VERSION,
};
use anyhow::Result as DriverResult;
use mio::{Events, Interest, Poll, Token};
use nix::fcntl::{FcntlArg, OFlag, fcntl};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const DEFAULT_INITIALIZE_TIMEOUT: Duration = Duration::from_secs(5);
pub const DEFAULT_RPC_TIMEOUT: Duration = Duration::from_secs(1);

const STDOUT_TOKEN: Token = Token(0);
const STDIN_TOKEN: Token = Token(1);

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("spawn proc driver {path}")]
    Spawn {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("capture proc driver stdin")]
    MissingStdin,
    #[error("capture proc driver stdout")]
    MissingStdout,
    #[error("serialize RPC request")]
    Serialize(#[source] serde_json::Error),
    #[error("RPC request frame is {size} bytes; maximum is {limit}")]
    RequestFrameTooLarge { size: usize, limit: usize },
    #[error("{operation}")]
    Io {
        operation: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("proc driver closed stdout while waiting for response")]
    Closed,
    #[error("RPC {method:?} timed out after {timeout:?}")]
    Timeout { method: String, timeout: Duration },
    #[error("RPC response frame exceeds {limit} bytes")]
    ResponseFrameTooLarge { limit: usize },
    #[error("parse RPC response")]
    Parse(#[source] serde_json::Error),
    #[error("proc driver returned unsupported JSON-RPC version {0:?}")]
    ProtocolVersion(String),
    #[error("invalid RPC response: {0}")]
    InvalidResponse(String),
    #[error("speech protocol version {actual:?} is incompatible; expected {expected:?}")]
    SpeechProtocolVersion {
        expected: &'static str,
        actual: String,
    },
    #[error("speech server did not advertise required capability {0:?}")]
    MissingCapability(&'static str),
    #[error("proc driver transport is no longer usable")]
    Unavailable,
    #[error("proc driver RPC error {code}: {message}{data}")]
    Rpc {
        code: i64,
        message: String,
        data: String,
    },
}

impl Error {
    /// Whether the process/transport must be replaced before another call.
    #[must_use]
    pub fn is_transport_failure(&self) -> bool {
        matches!(
            self,
            Self::Spawn { .. }
                | Self::MissingStdin
                | Self::MissingStdout
                | Self::Io { .. }
                | Self::Closed
                | Self::Timeout { .. }
                | Self::ResponseFrameTooLarge { .. }
                | Self::Parse(_)
                | Self::ProtocolVersion(_)
                | Self::InvalidResponse(_)
                | Self::SpeechProtocolVersion { .. }
                | Self::MissingCapability(_)
                | Self::Unavailable
        )
    }
}

fn io_error(operation: &'static str) -> impl FnOnce(std::io::Error) -> Error {
    move |source| Error::Io { operation, source }
}

#[derive(Clone, Copy, Debug)]
pub struct RpcTimeouts {
    pub initialize: Duration,
    pub call: Duration,
}

impl Default for RpcTimeouts {
    fn default() -> Self {
        Self {
            initialize: DEFAULT_INITIALIZE_TIMEOUT,
            call: DEFAULT_RPC_TIMEOUT,
        }
    }
}

#[derive(Serialize)]
struct JsonRpcRequest<'a> {
    jsonrpc: &'static str,
    id: u64,
    method: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
}

pub struct ProcDriver {
    child: Arc<Mutex<Child>>,
    stdin: ChildStdin,
    stdout: ChildStdout,
    poll: Poll,
    events: Events,
    request_buf: Vec<u8>,
    response_buf: Vec<u8>,
    next_id: u64,
    rate: f32,
    timeouts: RpcTimeouts,
    legacy_protocol: bool,
    capabilities: SpeechCapabilities,
    pending_events: VecDeque<SpeechEventNotification>,
    next_compatibility_id: u64,
    last_compatibility_id: Option<UtteranceId>,
    unavailable: bool,
}

#[derive(Clone)]
pub struct TerminationHandle(Arc<Mutex<Child>>);

impl TerminationHandle {
    /// Interrupts a driver blocked in pipe I/O without waiting for its worker.
    pub fn terminate(&self) {
        // The worker may already hold this lock while reaping a child it has
        // killed after an RPC failure. Shutdown is a foreground operation and
        // must not wait behind that reap; in the contended case the child has
        // already received its terminating signal.
        match self.0.try_lock() {
            Ok(mut child) => {
                let _ = child.kill();
            }
            Err(std::sync::TryLockError::Poisoned(error)) => {
                let _ = error.into_inner().kill();
            }
            Err(std::sync::TryLockError::WouldBlock) => {}
        }
    }

    /// Terminates and synchronously reaps the speech server process.
    pub fn terminate_and_reap(&self) -> std::io::Result<ExitStatus> {
        let mut child = self
            .0
            .lock()
            .map_err(|_| std::io::Error::other("proc driver child lock is poisoned"))?;
        let _ = child.kill();
        child.wait()
    }
}

impl ProcDriver {
    pub fn new(path: &Path) -> Result<Self> {
        Self::new_with_args_and_timeouts(path, std::iter::empty::<&OsStr>(), RpcTimeouts::default())
    }

    pub fn new_with_args<I, S>(path: &Path, args: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Self::new_with_args_and_timeouts(path, args, RpcTimeouts::default())
    }

    pub fn new_with_timeout(path: &Path, timeout: Duration) -> Result<Self> {
        Self::new_with_args_and_timeouts(
            path,
            std::iter::empty::<&OsStr>(),
            RpcTimeouts {
                initialize: timeout,
                call: timeout,
            },
        )
    }

    pub fn new_with_args_and_timeouts<I, S>(
        path: &Path,
        args: I,
        timeouts: RpcTimeouts,
    ) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Self::new_with_args_and_registration(path, args, timeouts, |_| {})
    }

    /// Spawns a driver and publishes its termination handle before initialize.
    ///
    /// The registration callback must not block. It runs after both child
    /// pipes are captured but before nonblocking setup and the initialize RPC,
    /// allowing another thread to interrupt a startup handshake during
    /// shutdown.
    pub fn new_with_args_and_registration<I, S, F>(
        path: &Path,
        args: I,
        timeouts: RpcTimeouts,
        register: F,
    ) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
        F: FnOnce(TerminationHandle),
    {
        let mut child = Command::new(path)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|source| Error::Spawn {
                path: path.display().to_string(),
                source,
            })?;
        let Some(stdin) = child.stdin.take() else {
            terminate_and_reap_child(&mut child);
            return Err(Error::MissingStdin);
        };
        let Some(stdout) = child.stdout.take() else {
            terminate_and_reap_child(&mut child);
            return Err(Error::MissingStdout);
        };
        let child = Arc::new(Mutex::new(child));
        register(TerminationHandle(Arc::clone(&child)));

        if let Err(error) = make_nonblocking(stdin.as_raw_fd(), "make RPC stdin nonblocking") {
            terminate_and_reap_shared_child(&child);
            return Err(error);
        }
        if let Err(error) = make_nonblocking(stdout.as_raw_fd(), "make RPC stdout nonblocking") {
            terminate_and_reap_shared_child(&child);
            return Err(error);
        }
        let poll = match Poll::new().map_err(io_error("create RPC poll")) {
            Ok(poll) => poll,
            Err(error) => {
                terminate_and_reap_shared_child(&child);
                return Err(error);
            }
        };
        let stdout_fd = stdout.as_raw_fd();
        if let Err(error) = poll
            .registry()
            .register(
                &mut mio::unix::SourceFd(&stdout_fd),
                STDOUT_TOKEN,
                Interest::READABLE,
            )
            .map_err(io_error("register RPC stdout"))
        {
            terminate_and_reap_shared_child(&child);
            return Err(error);
        }

        let mut driver = Self {
            child,
            stdin,
            stdout,
            poll,
            events: Events::with_capacity(4),
            request_buf: Vec::with_capacity(256),
            response_buf: Vec::with_capacity(256),
            next_id: 1,
            rate: 1.0,
            timeouts,
            legacy_protocol: false,
            capabilities: SpeechCapabilities::default(),
            pending_events: VecDeque::new(),
            next_compatibility_id: 1,
            last_compatibility_id: None,
            unavailable: false,
        };
        driver.initialize()?;
        Ok(driver)
    }

    #[must_use]
    pub fn termination_handle(&self) -> TerminationHandle {
        TerminationHandle(Arc::clone(&self.child))
    }

    #[must_use]
    pub fn is_legacy_protocol(&self) -> bool {
        self.legacy_protocol
    }

    /// Compatibility convenience for direct process-driver tests and tools.
    /// Production sequencing uses [`Host`] through `SpeechManager`.
    pub fn speak(&mut self, text: &str, interrupt: bool) -> DriverResult<()> {
        let id = UtteranceId::new(format!("direct-{}", self.next_compatibility_id));
        self.next_compatibility_id = self.next_compatibility_id.wrapping_add(1);
        Host::speak(self, &id, text, interrupt)?;
        self.last_compatibility_id = Some(id);
        Ok(())
    }

    pub fn stop(&mut self) -> DriverResult<()> {
        let id = self
            .last_compatibility_id
            .take()
            .unwrap_or_else(|| UtteranceId::new("direct-none"));
        Host::stop(self, &id)
    }

    #[must_use]
    pub fn get_rate(&self) -> f32 {
        self.rate
    }

    pub fn set_rate(&mut self, rate: f32) -> DriverResult<()> {
        Host::set_rate(self, rate).map(|_| ())
    }

    fn initialize(&mut self) -> Result<()> {
        let result = match self.call_with_timeout(
            "initialize",
            Some(
                serde_json::to_value(InitializeParams {
                    protocol: ProtocolRange::current(),
                    client: PeerInfo {
                        name: "lector".to_owned(),
                        version: env!("CARGO_PKG_VERSION").to_owned(),
                    },
                    client_capabilities: ClientCapabilities::default(),
                })
                .map_err(Error::Serialize)?,
            ),
            self.timeouts.initialize,
        ) {
            Ok(result) => result,
            Err(Error::Rpc { code: -32601, .. }) => {
                return self.accept_unversioned_legacy();
            }
            Err(Error::Rpc { code: -32001, .. }) => {
                return self.initialize_version_one();
            }
            Err(error) => return Err(error),
        };
        let initialized: InitializeResult = serde_json::from_value(result).map_err(|error| {
            Error::InvalidResponse(format!("invalid initialize result: {error}"))
        })?;
        if !ProtocolRange::current().supports(initialized.protocol) {
            return Err(Error::SpeechProtocolVersion {
                expected: SPEECH_PROTOCOL_VERSION,
                actual: format!(
                    "{}.{}",
                    initialized.protocol.major, initialized.protocol.minor
                ),
            });
        }
        if initialized.server.name.is_empty() || initialized.server.version.is_empty() {
            return Err(Error::InvalidResponse(
                "initialize result has an empty server name or version".to_owned(),
            ));
        }
        if !initialized.capabilities.controls.stop.is_supported() {
            return Err(Error::MissingCapability("controls.stop"));
        }
        self.capabilities = initialized.capabilities;
        Ok(())
    }

    fn accept_unversioned_legacy(&mut self) -> Result<()> {
        self.legacy_protocol = true;
        self.capabilities = legacy_capabilities();
        Ok(())
    }

    fn initialize_version_one(&mut self) -> Result<()> {
        let result = self.call_with_timeout(
            "initialize",
            Some(json!({
                "protocol_version": "1.0",
                "client": {
                    "name": "lector",
                    "version": env!("CARGO_PKG_VERSION")
                }
            })),
            self.timeouts.initialize,
        )?;
        let protocol = result
            .get("protocol_version")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                Error::InvalidResponse(
                    "version 1 initialize result is missing protocol_version".to_owned(),
                )
            })?;
        if protocol != "1.0" {
            return Err(Error::SpeechProtocolVersion {
                expected: "1.0",
                actual: protocol.to_owned(),
            });
        }
        let capabilities = result
            .get("capabilities")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                Error::InvalidResponse(
                    "version 1 initialize result is missing capabilities".to_owned(),
                )
            })?;
        for name in ["speak", "stop", "set_rate", "rpc_discover"] {
            if capabilities.get(name).and_then(Value::as_bool) != Some(true) {
                return Err(Error::MissingCapability(name));
            }
        }
        self.legacy_protocol = true;
        self.capabilities = legacy_capabilities();
        Ok(())
    }

    fn call(&mut self, method: &str, params: Option<Value>) -> Result<Value> {
        self.call_with_timeout(method, params, self.timeouts.call)
    }

    fn call_with_timeout(
        &mut self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
    ) -> Result<Value> {
        if self.unavailable {
            return Err(Error::Unavailable);
        }
        let result = self.call_inner(method, params, timeout);
        if result
            .as_ref()
            .is_err_and(|error| error.is_transport_failure())
        {
            self.unavailable = true;
            if let Ok(mut child) = self.child.lock() {
                terminate_and_reap_child(&mut child);
            }
        }
        result
    }

    fn call_inner(
        &mut self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
    ) -> Result<Value> {
        let started = Instant::now();
        let deadline = started.checked_add(timeout).unwrap_or(started);
        let id = self.next_id;
        self.next_id = if id >= MAX_JSON_SAFE_INTEGER {
            1
        } else {
            id + 1
        };
        let request = JsonRpcRequest {
            jsonrpc: "2.0",
            id,
            method,
            params,
        };
        self.request_buf.clear();
        serde_json::to_writer(&mut self.request_buf, &request).map_err(Error::Serialize)?;
        self.request_buf.push(b'\n');
        if self.request_buf.len() > MAX_RPC_FRAME_BYTES {
            return Err(Error::RequestFrameTooLarge {
                size: self.request_buf.len(),
                limit: MAX_RPC_FRAME_BYTES,
            });
        }

        self.write_request(deadline, method, timeout)?;
        loop {
            let frame = self.read_response(deadline, method, timeout)?;
            let message: Value = serde_json::from_slice(&frame).map_err(Error::Parse)?;
            if message.get("method").is_some() {
                self.handle_notification(message)?;
                continue;
            }
            return parse_response_value(message, id);
        }
    }

    fn write_request(&mut self, deadline: Instant, method: &str, timeout: Duration) -> Result<()> {
        let mut written = 0;
        while written < self.request_buf.len() {
            check_deadline(deadline, method, timeout)?;
            match self.stdin.write(&self.request_buf[written..]) {
                Ok(0) => {
                    return Err(io_error("write RPC request")(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "speech server accepted zero bytes",
                    )));
                }
                Ok(count) => written += count,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    self.wait_for_stdin(deadline, method, timeout)?;
                }
                Err(error) => return Err(io_error("write RPC request")(error)),
            }
        }
        Ok(())
    }

    fn wait_for_stdin(&mut self, deadline: Instant, method: &str, timeout: Duration) -> Result<()> {
        let stdin_fd = self.stdin.as_raw_fd();
        self.poll
            .registry()
            .register(
                &mut mio::unix::SourceFd(&stdin_fd),
                STDIN_TOKEN,
                Interest::WRITABLE,
            )
            .map_err(io_error("register RPC stdin"))?;
        let waited = self.wait_for(STDIN_TOKEN, deadline, method, timeout);
        let deregistered = self
            .poll
            .registry()
            .deregister(&mut mio::unix::SourceFd(&stdin_fd))
            .map_err(io_error("deregister RPC stdin"));
        waited.and(deregistered)
    }

    fn read_response(
        &mut self,
        deadline: Instant,
        method: &str,
        timeout: Duration,
    ) -> Result<Vec<u8>> {
        loop {
            check_deadline(deadline, method, timeout)?;
            if let Some(newline) = self.response_buf.iter().position(|byte| *byte == b'\n') {
                if newline.saturating_add(1) > MAX_RPC_FRAME_BYTES {
                    return Err(Error::ResponseFrameTooLarge {
                        limit: MAX_RPC_FRAME_BYTES,
                    });
                }
                let remaining = self.response_buf.split_off(newline + 1);
                let frame = std::mem::replace(&mut self.response_buf, remaining);
                return Ok(frame);
            }
            if self.response_buf.len() >= MAX_RPC_FRAME_BYTES {
                return Err(Error::ResponseFrameTooLarge {
                    limit: MAX_RPC_FRAME_BYTES,
                });
            }

            let mut chunk = [0u8; 8192];
            match self.stdout.read(&mut chunk) {
                Ok(0) => return Err(Error::Closed),
                Ok(read) => {
                    if let Some(newline) = chunk[..read].iter().position(|byte| *byte == b'\n') {
                        if self
                            .response_buf
                            .len()
                            .saturating_add(newline)
                            .saturating_add(1)
                            > MAX_RPC_FRAME_BYTES
                        {
                            return Err(Error::ResponseFrameTooLarge {
                                limit: MAX_RPC_FRAME_BYTES,
                            });
                        }
                    } else if self.response_buf.len().saturating_add(read) > MAX_RPC_FRAME_BYTES {
                        return Err(Error::ResponseFrameTooLarge {
                            limit: MAX_RPC_FRAME_BYTES,
                        });
                    }
                    self.response_buf.extend_from_slice(&chunk[..read]);
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    self.wait_for(STDOUT_TOKEN, deadline, method, timeout)?;
                }
                Err(error) => return Err(io_error("read RPC response")(error)),
            }
        }
    }

    fn wait_for(
        &mut self,
        token: Token,
        deadline: Instant,
        method: &str,
        timeout: Duration,
    ) -> Result<()> {
        loop {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Err(Error::Timeout {
                    method: method.to_owned(),
                    timeout,
                });
            };
            match self.poll.poll(&mut self.events, Some(remaining)) {
                Ok(()) => {
                    if self.events.iter().any(|event| event.token() == token) {
                        return Ok(());
                    }
                    if Instant::now() >= deadline {
                        return Err(Error::Timeout {
                            method: method.to_owned(),
                            timeout,
                        });
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(io_error("poll RPC pipe")(error)),
            }
        }
    }

    fn handle_notification(&mut self, message: Value) -> Result<()> {
        let object = message.as_object().ok_or_else(|| {
            Error::InvalidResponse("server notification must be an object".to_owned())
        })?;
        if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Err(Error::InvalidResponse(
                "server notification jsonrpc must be 2.0".to_owned(),
            ));
        }
        if object.contains_key("id") {
            return Err(Error::InvalidResponse(
                "server-to-client requests are not supported".to_owned(),
            ));
        }
        let Some(method) = object.get("method").and_then(Value::as_str) else {
            return Err(Error::InvalidResponse(
                "server notification is missing method".to_owned(),
            ));
        };
        if method != "speech.event" {
            // Additive notifications are deliberately ignorable.
            return Ok(());
        }
        let params = object.get("params").cloned().ok_or_else(|| {
            Error::InvalidResponse("speech.event notification is missing params".to_owned())
        })?;
        let event: SpeechEventNotification = serde_json::from_value(params).map_err(|error| {
            Error::InvalidResponse(format!("invalid speech.event notification: {error}"))
        })?;
        if !event.utterance_id.is_valid() {
            return Err(Error::InvalidResponse(
                "speech.event has an invalid utteranceId".to_owned(),
            ));
        }
        if event.sequence > MAX_JSON_SAFE_INTEGER {
            return Err(Error::InvalidResponse(
                "speech.event sequence exceeds the JSON safe-integer range".to_owned(),
            ));
        }
        self.pending_events.push_back(event);
        Ok(())
    }

    fn read_available_notifications(&mut self) -> Result<()> {
        loop {
            while let Some(newline) = self.response_buf.iter().position(|byte| *byte == b'\n') {
                if newline.saturating_add(1) > MAX_RPC_FRAME_BYTES {
                    return Err(Error::ResponseFrameTooLarge {
                        limit: MAX_RPC_FRAME_BYTES,
                    });
                }
                let remaining = self.response_buf.split_off(newline + 1);
                let frame = std::mem::replace(&mut self.response_buf, remaining);
                let message: Value = serde_json::from_slice(&frame).map_err(Error::Parse)?;
                if message.get("method").is_none() {
                    return Err(Error::InvalidResponse(
                        "received an RPC response with no outstanding request".to_owned(),
                    ));
                }
                self.handle_notification(message)?;
            }
            if self.response_buf.len() >= MAX_RPC_FRAME_BYTES {
                return Err(Error::ResponseFrameTooLarge {
                    limit: MAX_RPC_FRAME_BYTES,
                });
            }

            let mut chunk = [0u8; 8192];
            match self.stdout.read(&mut chunk) {
                Ok(0) => return Err(Error::Closed),
                Ok(read) => {
                    if self.response_buf.len().saturating_add(read) > MAX_RPC_FRAME_BYTES {
                        return Err(Error::ResponseFrameTooLarge {
                            limit: MAX_RPC_FRAME_BYTES,
                        });
                    }
                    self.response_buf.extend_from_slice(&chunk[..read]);
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) => return Err(io_error("read RPC notification")(error)),
            }
        }
    }
}

impl Host for ProcDriver {
    fn capabilities(&self) -> &SpeechCapabilities {
        &self.capabilities
    }

    fn has_legacy_queue(&self) -> bool {
        self.legacy_protocol
    }

    fn speak(&mut self, id: &UtteranceId, text: &str, legacy_interrupt: bool) -> DriverResult<()> {
        let (method, params) = if self.legacy_protocol {
            (
                "speak",
                json!({ "text": text, "interrupt": legacy_interrupt }),
            )
        } else {
            ("speech.speak", json!({ "utteranceId": id, "text": text }))
        };
        let result = self.call(method, Some(params))?;
        if self.legacy_protocol {
            expect_null_result("speak", result).map_err(Into::into)
        } else {
            expect_accepted_result("speech.speak", result).map_err(Into::into)
        }
    }

    fn stop(&mut self, id: &UtteranceId) -> DriverResult<()> {
        let result = if self.legacy_protocol {
            self.call("stop", None)?
        } else {
            self.call(
                "speech.stop",
                Some(
                    serde_json::to_value(UtteranceParams {
                        utterance_id: id.clone(),
                    })
                    .map_err(Error::Serialize)?,
                ),
            )?
        };
        if self.legacy_protocol {
            expect_null_result("stop", result).map_err(Into::into)
        } else {
            expect_accepted_result("speech.stop", result).map_err(Into::into)
        }
    }

    fn pause(&mut self, id: &UtteranceId) -> DriverResult<PauseResult> {
        let result = self.call("speech.pause", Some(json!({ "utteranceId": id })))?;
        serde_json::from_value(result)
            .map_err(|error| {
                Error::InvalidResponse(format!("invalid speech.pause result: {error}"))
            })
            .map_err(Into::into)
    }

    fn resume(&mut self, id: &UtteranceId) -> DriverResult<()> {
        let result = self.call("speech.resume", Some(json!({ "utteranceId": id })))?;
        expect_accepted_result("speech.resume", result).map_err(Into::into)
    }

    fn set_rate(&mut self, rate: f32) -> DriverResult<f32> {
        let method = if self.legacy_protocol {
            "set_rate"
        } else {
            "speech.setRate"
        };
        let result = self.call(method, Some(json!({ "rate": rate })))?;
        if self.legacy_protocol && result.is_null() {
            self.rate = rate;
            return Ok(rate);
        }
        let actual = match result.as_object() {
            Some(result) => result
                .get("rate")
                .and_then(Value::as_f64)
                .filter(|rate| rate.is_finite())
                .map(|rate| rate as f32)
                .filter(|rate| rate.is_finite()),
            None => None,
        };
        let actual = match actual {
            Some(actual) => actual,
            None => {
                self.fail_transport();
                return Err(Error::InvalidResponse(
                    "set_rate result must contain a finite rate".to_owned(),
                )
                .into());
            }
        };
        self.rate = actual;
        Ok(actual)
    }

    fn take_events(&mut self) -> DriverResult<Vec<SpeechEventNotification>> {
        self.read_available_notifications()?;
        Ok(self.pending_events.drain(..).collect())
    }
}

impl ProcDriver {
    fn fail_transport(&mut self) {
        self.unavailable = true;
        if let Ok(mut child) = self.child.lock() {
            terminate_and_reap_child(&mut child);
        }
    }
}

impl Drop for ProcDriver {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            terminate_and_reap_child(&mut child);
        }
    }
}

fn make_nonblocking(fd: std::os::fd::RawFd, operation: &'static str) -> Result<()> {
    let flags = fcntl(fd, FcntlArg::F_GETFL).map_err(|error| Error::Io {
        operation,
        source: error.into(),
    })?;
    fcntl(
        fd,
        FcntlArg::F_SETFL(OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK),
    )
    .map_err(|error| Error::Io {
        operation,
        source: error.into(),
    })?;
    Ok(())
}

fn terminate_and_reap_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn terminate_and_reap_shared_child(child: &Arc<Mutex<Child>>) {
    if let Ok(mut child) = child.lock() {
        terminate_and_reap_child(&mut child);
    }
}

fn check_deadline(deadline: Instant, method: &str, timeout: Duration) -> Result<()> {
    if Instant::now() < deadline {
        Ok(())
    } else {
        Err(Error::Timeout {
            method: method.to_owned(),
            timeout,
        })
    }
}

#[cfg(test)]
fn parse_response(frame: &[u8], expected_id: u64) -> Result<Value> {
    let response: Value = serde_json::from_slice(frame).map_err(Error::Parse)?;
    parse_response_value(response, expected_id)
}

fn parse_response_value(response: Value, expected_id: u64) -> Result<Value> {
    let object = response
        .as_object()
        .ok_or_else(|| Error::InvalidResponse("response must be an object".to_owned()))?;
    let version = object
        .get("jsonrpc")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::InvalidResponse("response is missing jsonrpc".to_owned()))?;
    if version != "2.0" {
        return Err(Error::ProtocolVersion(version.to_owned()));
    }
    let id = object.get("id").and_then(Value::as_u64).ok_or_else(|| {
        Error::InvalidResponse("response id must be an unsigned integer".to_owned())
    })?;
    if id != expected_id {
        return Err(Error::InvalidResponse(format!(
            "response id {id} does not match request id {expected_id}"
        )));
    }

    match (object.get("result"), object.get("error")) {
        (Some(result), None) => Ok(result.clone()),
        (None, Some(error)) => {
            let error = error
                .as_object()
                .ok_or_else(|| Error::InvalidResponse("error must be an object".to_owned()))?;
            let code = error.get("code").and_then(Value::as_i64).ok_or_else(|| {
                Error::InvalidResponse("error code must be an integer".to_owned())
            })?;
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::InvalidResponse("error message must be a string".to_owned()))?
                .to_owned();
            let data = error
                .get("data")
                .map(|value| format!(" ({value})"))
                .unwrap_or_default();
            Err(Error::Rpc {
                code,
                message,
                data,
            })
        }
        (Some(_), Some(_)) => Err(Error::InvalidResponse(
            "response must not contain both result and error".to_owned(),
        )),
        (None, None) => Err(Error::InvalidResponse(
            "response must contain result or error".to_owned(),
        )),
    }
}

fn expect_null_result(method: &str, result: Value) -> Result<()> {
    if result.is_null() {
        Ok(())
    } else {
        Err(Error::InvalidResponse(format!(
            "{method} result must be null"
        )))
    }
}

fn expect_accepted_result(method: &str, result: Value) -> Result<()> {
    let accepted: AcceptedResult = serde_json::from_value(result)
        .map_err(|error| Error::InvalidResponse(format!("invalid {method} result: {error}")))?;
    if accepted.accepted {
        Ok(())
    } else {
        Err(Error::InvalidResponse(format!(
            "{method} returned accepted=false without an RPC error"
        )))
    }
}

fn legacy_capabilities() -> SpeechCapabilities {
    SpeechCapabilities {
        controls: ControlCapabilities {
            stop: StopSupport::BestEffort,
            ..Default::default()
        },
        settings: SettingCapabilities {
            rate: SettingSupport::WriteOnly,
            ..Default::default()
        },
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::{Error, TerminationHandle, parse_response};
    use serde_json::json;
    use std::{
        process::Command,
        sync::{Arc, Mutex, mpsc},
        thread,
        time::{Duration, Instant},
    };

    #[test]
    fn asynchronous_termination_never_waits_for_a_worker_reap_lock() {
        let child = Arc::new(Mutex::new(
            Command::new("/bin/sleep")
                .arg("60")
                .spawn()
                .expect("spawn test child"),
        ));
        let handle = TerminationHandle(Arc::clone(&child));
        let held_child = Arc::clone(&child);
        let (locked_tx, locked_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let holder = thread::spawn(move || {
            let _child = held_child.lock().expect("hold child lock");
            locked_tx.send(()).expect("report held child lock");
            release_rx.recv().expect("release child lock");
        });
        locked_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("worker acquired child lock");

        let started = Instant::now();
        handle.terminate();
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "foreground termination blocked for {:?}",
            started.elapsed()
        );

        release_tx.send(()).expect("release child lock");
        holder.join().expect("join child-lock holder");
        handle
            .terminate_and_reap()
            .expect("terminate and reap test child");
    }

    #[test]
    fn response_envelope_requires_exactly_one_result_or_error() {
        for response in [
            json!({"jsonrpc":"2.0", "id":1}),
            json!({"jsonrpc":"2.0", "id":1, "result":null, "error":{"code":-1,"message":"bad"}}),
        ] {
            let error = parse_response(response.to_string().as_bytes(), 1).unwrap_err();
            assert!(matches!(error, Error::InvalidResponse(_)));
            assert!(error.is_transport_failure());
        }
    }

    #[test]
    fn response_envelope_validates_version_id_and_error_shape() {
        for response in [
            json!({"jsonrpc":"1.0", "id":1, "result":null}),
            json!({"jsonrpc":"2.0", "id":2, "result":null}),
            json!({"jsonrpc":"2.0", "id":1, "error":{"code":"bad","message":"bad"}}),
            json!({"jsonrpc":"2.0", "id":1, "error":{"code":-1,"message":7}}),
        ] {
            assert!(parse_response(response.to_string().as_bytes(), 1).is_err());
        }
    }

    #[test]
    fn valid_rpc_errors_are_not_transport_failures() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": {"code": -32602, "message": "invalid params"},
        });
        let error = parse_response(response.to_string().as_bytes(), 1).unwrap_err();
        assert!(matches!(error, Error::Rpc { code: -32602, .. }));
        assert!(!error.is_transport_failure());
    }

    #[test]
    fn additive_response_and_error_members_are_ignored() {
        let result = parse_response(
            br#"{"jsonrpc":"2.0","id":1,"result":{"ok":true},"future":7}"#,
            1,
        )
        .unwrap();
        assert_eq!(result, json!({"ok": true}));

        let error = parse_response(
            br#"{"jsonrpc":"2.0","id":1,"error":{"code":-1,"message":"bad","future":7}}"#,
            1,
        )
        .unwrap_err();
        assert!(matches!(error, Error::Rpc { code: -1, .. }));
    }
}
