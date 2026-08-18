//! Routes a direct PTY source across an embedded top-level `tmux -CC` stream.

use crate::tmux_control::{ControlEvent, ControlParseError, TmuxControlParser};
use thiserror::Error;

pub const TMUX_CONTROL_START_MARKER: &[u8] = b"\x1bP1000p";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GatewayEvent {
    DirectOutput(Vec<u8>),
    ConnectionStarted {
        connection_id: u64,
    },
    Control {
        connection_id: u64,
        event: ControlEvent,
    },
    ConnectionEnded {
        connection_id: u64,
    },
    ConnectionFailed {
        connection_id: u64,
        reason: GatewayFailure,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GatewayFailure {
    TransportEof,
    MissingExit,
    MissingTerminator,
    TerminatorTimeout,
    Protocol(String),
}

impl std::fmt::Display for GatewayFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TransportEof => formatter.write_str("transport ended unexpectedly"),
            Self::MissingExit => formatter.write_str("control stream ended without %exit"),
            Self::MissingTerminator => {
                formatter.write_str("ordinary output arrived before the final terminator")
            }
            Self::TerminatorTimeout => {
                formatter.write_str("timed out waiting for the final terminator")
            }
            Self::Protocol(error) => write!(formatter, "invalid control protocol: {error}"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GatewayLifecycleState {
    Direct,
    Control,
    AwaitingTerminator,
    Recovering,
}

#[derive(Debug, Error)]
pub enum GatewayError {
    #[error("tmux connection id space is exhausted")]
    ConnectionIdExhausted,
    #[error("tmux control connection {connection_id} failed: {source}")]
    Control {
        connection_id: u64,
        #[source]
        source: ControlParseError,
    },
    #[error("tmux control connection {connection_id} ended before its ST marker: {source}")]
    UnterminatedControl {
        connection_id: u64,
        #[source]
        source: ControlParseError,
    },
}

#[derive(Debug)]
struct ActiveControl {
    connection_id: u64,
    parser: TmuxControlParser,
    current_record: Vec<u8>,
    saw_exit: bool,
    terminator_escape_seen: bool,
    recovering: bool,
}

#[derive(Debug)]
pub struct TmuxGatewayRouter {
    marker_prefix: Vec<u8>,
    active: Option<ActiveControl>,
    next_connection_id: u64,
}

impl Default for TmuxGatewayRouter {
    fn default() -> Self {
        Self::new()
    }
}

impl TmuxGatewayRouter {
    #[must_use]
    pub fn new() -> Self {
        Self::with_first_connection_id(1)
    }

    /// Creates a source router with a caller-assigned first connection ID.
    /// The ID becomes available again after that connection fully ends.
    #[must_use]
    pub fn with_first_connection_id(first_connection_id: u64) -> Self {
        Self {
            marker_prefix: Vec::with_capacity(TMUX_CONTROL_START_MARKER.len()),
            active: None,
            next_connection_id: first_connection_id,
        }
    }

    #[must_use]
    pub fn active_connection(&self) -> Option<u64> {
        self.active.as_ref().map(|active| active.connection_id)
    }

    #[must_use]
    pub fn lifecycle_state(&self) -> GatewayLifecycleState {
        match self.active.as_ref() {
            None => GatewayLifecycleState::Direct,
            Some(active) if active.recovering => GatewayLifecycleState::Recovering,
            Some(active) if active.saw_exit => GatewayLifecycleState::AwaitingTerminator,
            Some(_) => GatewayLifecycleState::Control,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<GatewayEvent>, GatewayError> {
        let mut events = Vec::new();
        let mut direct = Vec::new();
        let mut index = 0;

        while index < bytes.len() {
            if self.active.as_ref().is_some_and(|active| active.recovering) {
                let byte = bytes[index];
                index += 1;
                let active = self
                    .active
                    .as_mut()
                    .expect("recovering connection checked above");
                if active.terminator_escape_seen && byte == b'\\' {
                    let connection_id = active.connection_id;
                    self.active = None;
                    self.next_connection_id = connection_id;
                    events.push(GatewayEvent::ConnectionEnded { connection_id });
                } else {
                    active.terminator_escape_seen = byte == b'\x1b';
                }
                continue;
            }

            if self
                .active
                .as_ref()
                .is_some_and(|active| active.saw_exit && !active.terminator_escape_seen)
                && bytes[index] != b'\x1b'
            {
                let connection_id = self
                    .active
                    .take()
                    .expect("active connection checked above")
                    .connection_id;
                self.next_connection_id = connection_id;
                events.push(GatewayEvent::ConnectionFailed {
                    connection_id,
                    reason: GatewayFailure::MissingTerminator,
                });
                // Reconsider this byte as ordinary parent-terminal output.
                continue;
            }

            if let Some(active) = self.active.as_mut() {
                let connection_id = active.connection_id;
                let byte = bytes[index];
                if active.saw_exit && byte == b'\x1b' {
                    active.terminator_escape_seen = true;
                }
                active.current_record.push(byte);
                let parsed = match active.parser.push(&bytes[index..index + 1]) {
                    Ok(parsed) => parsed,
                    Err(source) => {
                        let mut failed = self.active.take().expect("active parser failed");
                        index += 1;
                        events.push(GatewayEvent::ConnectionFailed {
                            connection_id,
                            reason: if failed.saw_exit {
                                GatewayFailure::MissingTerminator
                            } else {
                                GatewayFailure::Protocol(source.to_string())
                            },
                        });
                        let returned_to_direct_transport = failed.saw_exit
                            || failed
                                .current_record
                                .first()
                                .is_some_and(|byte| *byte != b'%' && *byte != b'\x1b');
                        if returned_to_direct_transport {
                            self.next_connection_id = connection_id;
                            direct.extend_from_slice(&failed.current_record);
                        } else {
                            // A protocol-looking record failed while the DCS
                            // transport is still live. Do not expose the rest
                            // of that control channel as terminal text. The
                            // application will terminate the client while this
                            // bounded recovery state drains through DCS ST.
                            failed.current_record.clear();
                            failed.recovering = true;
                            failed.saw_exit = false;
                            failed.terminator_escape_seen = false;
                            self.active = Some(failed);
                        }
                        continue;
                    }
                };
                index += 1;
                let ended = parsed
                    .iter()
                    .any(|event| matches!(event, ControlEvent::Ended));
                let saw_exit = parsed
                    .iter()
                    .any(|event| matches!(event, ControlEvent::Exit { .. }));
                if saw_exit {
                    active.saw_exit = true;
                }
                if byte == b'\n' {
                    active.current_record.clear();
                }
                events.extend(parsed.into_iter().map(|event| GatewayEvent::Control {
                    connection_id,
                    event,
                }));
                if ended {
                    let active = self.active.take().expect("parser just ended");
                    self.next_connection_id = connection_id;
                    if active.saw_exit {
                        events.push(GatewayEvent::ConnectionEnded { connection_id });
                    } else {
                        events.push(GatewayEvent::ConnectionFailed {
                            connection_id,
                            reason: GatewayFailure::MissingExit,
                        });
                    }
                }
                continue;
            }

            let byte = bytes[index];
            let expected = TMUX_CONTROL_START_MARKER[self.marker_prefix.len()];
            if byte == expected {
                self.marker_prefix.push(byte);
                index += 1;
                if self.marker_prefix.len() == TMUX_CONTROL_START_MARKER.len() {
                    emit_direct(&mut events, &mut direct);
                    self.marker_prefix.clear();
                    self.start_connection(&mut events)?;
                }
                continue;
            }

            if self.marker_prefix.is_empty() {
                direct.push(byte);
                index += 1;
            } else {
                direct.append(&mut self.marker_prefix);
                // Reconsider this byte. It may itself begin the marker.
            }
        }

        emit_direct(&mut events, &mut direct);
        Ok(events)
    }

    /// Finishes an ordinary direct stream, releasing a partial marker lookalike.
    /// An active control stream must instead end with its explicit ST marker.
    pub fn finish_direct(&mut self) -> Result<Vec<u8>, GatewayError> {
        if let Some(active) = self.active.as_mut() {
            let connection_id = active.connection_id;
            if active.recovering {
                return Err(GatewayError::UnterminatedControl {
                    connection_id,
                    source: ControlParseError::ParserPoisoned,
                });
            }
            return active
                .parser
                .finish()
                .map(|_| Vec::new())
                .map_err(|source| GatewayError::UnterminatedControl {
                    connection_id,
                    source,
                });
        }
        Ok(std::mem::take(&mut self.marker_prefix))
    }

    /// Finishes the underlying transport and resets the router to a reusable
    /// direct state. Unlike `finish_direct`, abrupt control EOF is a routed
    /// lifecycle event rather than a fatal parser error.
    pub fn finish_transport(&mut self) -> Vec<GatewayEvent> {
        let mut events = Vec::new();
        if let Some(active) = self.active.take() {
            self.next_connection_id = active.connection_id;
            if active.recovering {
                events.push(GatewayEvent::ConnectionEnded {
                    connection_id: active.connection_id,
                });
            } else {
                events.push(GatewayEvent::ConnectionFailed {
                    connection_id: active.connection_id,
                    reason: if active.saw_exit {
                        GatewayFailure::MissingTerminator
                    } else {
                        GatewayFailure::TransportEof
                    },
                });
            }
        } else if !self.marker_prefix.is_empty() {
            events.push(GatewayEvent::DirectOutput(std::mem::take(
                &mut self.marker_prefix,
            )));
        }
        self.marker_prefix.clear();
        events
    }

    /// Ends a connection which emitted `%exit` but never completed DCS ST.
    pub fn expire_termination(&mut self) -> Vec<GatewayEvent> {
        let Some(active) = self.active.as_ref() else {
            return Vec::new();
        };
        if !active.saw_exit {
            return Vec::new();
        }
        let connection_id = active.connection_id;
        self.active = None;
        self.next_connection_id = connection_id;
        vec![GatewayEvent::ConnectionFailed {
            connection_id,
            reason: GatewayFailure::TerminatorTimeout,
        }]
    }

    fn start_connection(&mut self, events: &mut Vec<GatewayEvent>) -> Result<(), GatewayError> {
        let connection_id = self.next_connection_id;
        self.next_connection_id = self
            .next_connection_id
            .checked_add(1)
            .ok_or(GatewayError::ConnectionIdExhausted)?;
        let mut parser = TmuxControlParser::new();
        let parsed =
            parser
                .push(TMUX_CONTROL_START_MARKER)
                .map_err(|source| GatewayError::Control {
                    connection_id,
                    source,
                })?;
        events.push(GatewayEvent::ConnectionStarted { connection_id });
        events.extend(parsed.into_iter().map(|event| GatewayEvent::Control {
            connection_id,
            event,
        }));
        self.active = Some(ActiveControl {
            connection_id,
            parser,
            current_record: Vec::new(),
            saw_exit: false,
            terminator_escape_seen: false,
            recovering: false,
        });
        Ok(())
    }
}

fn emit_direct(events: &mut Vec<GatewayEvent>, direct: &mut Vec<u8>) {
    if !direct.is_empty() {
        events.push(GatewayEvent::DirectOutput(std::mem::take(direct)));
    }
}
