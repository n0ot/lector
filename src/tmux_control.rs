//! Standalone streaming parser for the tmux control-mode wire protocol.
//!
//! This module deliberately knows nothing about Lector's UI or tmux topology.
//! It turns one framed `tmux -CC` byte stream into bounded, binary-safe events.

use thiserror::Error;

const CONTROL_START: &[u8] = b"\x1bP1000p";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ParserLimits {
    pub max_line_bytes: usize,
    pub max_command_output_bytes: usize,
    pub max_command_output_lines: usize,
    pub max_notification_bytes: usize,
}

impl Default for ParserLimits {
    fn default() -> Self {
        Self {
            max_line_bytes: 64 * 1024,
            max_command_output_bytes: 4 * 1024 * 1024,
            max_command_output_lines: 65_536,
            max_notification_bytes: 64 * 1024,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandStatus {
    Success,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ControlEvent {
    Started,
    Command {
        timestamp: u64,
        number: u64,
        flags: u64,
        status: CommandStatus,
        output: Vec<Vec<u8>>,
    },
    Output {
        pane_id: u64,
        bytes: Vec<u8>,
    },
    ExtendedOutput {
        pane_id: u64,
        age_ms: u64,
        future_fields: Vec<Vec<u8>>,
        bytes: Vec<u8>,
    },
    Pause {
        pane_id: u64,
    },
    Continue {
        pane_id: u64,
    },
    Exit {
        reason: Option<Vec<u8>>,
    },
    Notification {
        name: Vec<u8>,
        arguments: Vec<u8>,
    },
    Ended,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnterminatedState {
    StartMarker,
    Record,
    Command,
    ControlStream,
    StringTerminator,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ControlParseError {
    #[error("invalid tmux control-mode start marker at byte {offset}")]
    InvalidStartMarker { offset: usize },
    #[error("invalid tmux control-mode string terminator")]
    InvalidStringTerminator,
    #[error("bytes followed the tmux control-mode string terminator")]
    TrailingData,
    #[error("the parser must be reset after an error")]
    ParserPoisoned,
    #[error("malformed tmux control record")]
    MalformedRecord,
    #[error("invalid numeric {field} in tmux control record")]
    InvalidNumber { field: &'static str },
    #[error("tmux command terminator did not match its begin record")]
    MismatchedCommand,
    #[error("tmux command block began inside another command block")]
    NestedCommand,
    #[error("tmux command terminator appeared without a command block")]
    UnexpectedCommandTerminator,
    #[error("invalid tmux pane id")]
    InvalidPaneId,
    #[error("invalid tmux octal escape")]
    InvalidOctalEscape,
    #[error("tmux control line exceeded {limit} bytes")]
    LineTooLong { limit: usize },
    #[error("tmux command output exceeded {limit} bytes")]
    CommandOutputTooLong { limit: usize },
    #[error("tmux command output exceeded {limit} lines")]
    TooManyCommandOutputLines { limit: usize },
    #[error("tmux notification exceeded {limit} bytes")]
    NotificationTooLong { limit: usize },
    #[error("unterminated tmux control-mode {state:?}")]
    Unterminated { state: UnterminatedState },
}

impl ControlParseError {
    #[must_use]
    pub fn is_malformed(&self) -> bool {
        !matches!(
            self,
            Self::LineTooLong { .. }
                | Self::CommandOutputTooLong { .. }
                | Self::TooManyCommandOutputLines { .. }
                | Self::NotificationTooLong { .. }
                | Self::Unterminated { .. }
                | Self::ParserPoisoned
        )
    }

    #[must_use]
    pub fn is_unterminated(&self) -> bool {
        matches!(self, Self::Unterminated { .. })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CommandTag {
    timestamp: u64,
    number: u64,
    flags: u64,
}

#[derive(Debug)]
struct PendingCommand {
    tag: CommandTag,
    output: Vec<Vec<u8>>,
    output_bytes: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ParseState {
    Start { matched: usize },
    Records,
    PossibleStringTerminator,
    Ended,
    Failed,
}

#[derive(Debug)]
pub struct TmuxControlParser {
    limits: ParserLimits,
    state: ParseState,
    line: Vec<u8>,
    pending_command: Option<PendingCommand>,
}

impl Default for TmuxControlParser {
    fn default() -> Self {
        Self::new()
    }
}

impl TmuxControlParser {
    #[must_use]
    pub fn new() -> Self {
        Self::with_limits(ParserLimits::default())
    }

    #[must_use]
    pub fn with_limits(limits: ParserLimits) -> Self {
        Self {
            limits,
            state: ParseState::Start { matched: 0 },
            line: Vec::new(),
            pending_command: None,
        }
    }

    pub fn reset(&mut self) {
        self.state = ParseState::Start { matched: 0 };
        self.line.clear();
        self.pending_command = None;
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<ControlEvent>, ControlParseError> {
        if self.state == ParseState::Failed {
            return Err(ControlParseError::ParserPoisoned);
        }

        let mut events = Vec::new();
        for &byte in bytes {
            if let Err(error) = self.push_byte(byte, &mut events) {
                self.state = ParseState::Failed;
                self.line.clear();
                self.pending_command = None;
                return Err(error);
            }
        }
        Ok(events)
    }

    pub fn finish(&mut self) -> Result<Vec<ControlEvent>, ControlParseError> {
        let error = match self.state {
            ParseState::Ended => return Ok(Vec::new()),
            ParseState::Failed => ControlParseError::ParserPoisoned,
            ParseState::Start { .. } => ControlParseError::Unterminated {
                state: UnterminatedState::StartMarker,
            },
            ParseState::PossibleStringTerminator => ControlParseError::Unterminated {
                state: UnterminatedState::StringTerminator,
            },
            ParseState::Records if !self.line.is_empty() => ControlParseError::Unterminated {
                state: UnterminatedState::Record,
            },
            ParseState::Records if self.pending_command.is_some() => {
                ControlParseError::Unterminated {
                    state: UnterminatedState::Command,
                }
            }
            ParseState::Records => ControlParseError::Unterminated {
                state: UnterminatedState::ControlStream,
            },
        };
        self.state = ParseState::Failed;
        self.line.clear();
        self.pending_command = None;
        Err(error)
    }

    fn push_byte(
        &mut self,
        byte: u8,
        events: &mut Vec<ControlEvent>,
    ) -> Result<(), ControlParseError> {
        match self.state {
            ParseState::Start { matched } => {
                if byte != CONTROL_START[matched] {
                    return Err(ControlParseError::InvalidStartMarker { offset: matched });
                }
                let matched = matched + 1;
                if matched == CONTROL_START.len() {
                    self.state = ParseState::Records;
                    events.push(ControlEvent::Started);
                } else {
                    self.state = ParseState::Start { matched };
                }
            }
            ParseState::Records if self.line.is_empty() && byte == 0x1b => {
                if self.pending_command.is_some() {
                    return Err(ControlParseError::MalformedRecord);
                }
                self.state = ParseState::PossibleStringTerminator;
            }
            ParseState::Records if byte == b'\n' => {
                while self.line.last() == Some(&b'\r') {
                    self.line.pop();
                }
                let line = std::mem::take(&mut self.line);
                self.process_line(line, events)?;
            }
            ParseState::Records => {
                if self.line.len() == self.limits.max_line_bytes {
                    return Err(ControlParseError::LineTooLong {
                        limit: self.limits.max_line_bytes,
                    });
                }
                self.line.push(byte);
            }
            ParseState::PossibleStringTerminator => {
                if byte != b'\\' {
                    return Err(ControlParseError::InvalidStringTerminator);
                }
                self.state = ParseState::Ended;
                events.push(ControlEvent::Ended);
            }
            ParseState::Ended => return Err(ControlParseError::TrailingData),
            ParseState::Failed => return Err(ControlParseError::ParserPoisoned),
        }
        Ok(())
    }

    fn process_line(
        &mut self,
        line: Vec<u8>,
        events: &mut Vec<ControlEvent>,
    ) -> Result<(), ControlParseError> {
        if let Some(pending) = self.pending_command.as_mut() {
            if let Some(rest) = line.strip_prefix(b"%end ") {
                let tag = parse_command_tag(rest)?;
                if tag != pending.tag {
                    return Err(ControlParseError::MismatchedCommand);
                }
                self.finish_command(CommandStatus::Success, events);
                return Ok(());
            }
            if let Some(rest) = line.strip_prefix(b"%error ") {
                let tag = parse_command_tag(rest)?;
                if tag != pending.tag {
                    return Err(ControlParseError::MismatchedCommand);
                }
                self.finish_command(CommandStatus::Error, events);
                return Ok(());
            }
            if line.starts_with(b"%begin ") {
                return Err(ControlParseError::NestedCommand);
            }
            if pending.output.len() == self.limits.max_command_output_lines {
                return Err(ControlParseError::TooManyCommandOutputLines {
                    limit: self.limits.max_command_output_lines,
                });
            }

            let output_bytes = pending.output_bytes.checked_add(line.len()).ok_or(
                ControlParseError::CommandOutputTooLong {
                    limit: self.limits.max_command_output_bytes,
                },
            )?;
            if output_bytes > self.limits.max_command_output_bytes {
                return Err(ControlParseError::CommandOutputTooLong {
                    limit: self.limits.max_command_output_bytes,
                });
            }
            pending.output_bytes = output_bytes;
            pending.output.push(line);
            return Ok(());
        }

        if let Some(rest) = line.strip_prefix(b"%begin ") {
            let tag = parse_command_tag(rest)?;
            self.pending_command = Some(PendingCommand {
                tag,
                output: Vec::new(),
                output_bytes: 0,
            });
            return Ok(());
        }
        if line.starts_with(b"%end ") || line.starts_with(b"%error ") {
            return Err(ControlParseError::UnexpectedCommandTerminator);
        }
        if [
            b"%begin".as_slice(),
            b"%end".as_slice(),
            b"%error".as_slice(),
            b"%output".as_slice(),
            b"%extended-output".as_slice(),
            b"%pause".as_slice(),
            b"%continue".as_slice(),
        ]
        .contains(&line.as_slice())
        {
            return Err(ControlParseError::MalformedRecord);
        }
        if !line.starts_with(b"%") {
            return Err(ControlParseError::MalformedRecord);
        }
        if line.len() > self.limits.max_notification_bytes {
            return Err(ControlParseError::NotificationTooLong {
                limit: self.limits.max_notification_bytes,
            });
        }

        if let Some(rest) = line.strip_prefix(b"%output ") {
            let (pane, value) = split_once_space(rest)?;
            events.push(ControlEvent::Output {
                pane_id: parse_pane_id(pane)?,
                bytes: decode_octal(value)?,
            });
            return Ok(());
        }
        if let Some(rest) = line.strip_prefix(b"%extended-output ") {
            events.push(parse_extended_output(rest)?);
            return Ok(());
        }
        if let Some(rest) = line.strip_prefix(b"%pause ") {
            events.push(ControlEvent::Pause {
                pane_id: parse_single_pane(rest)?,
            });
            return Ok(());
        }
        if let Some(rest) = line.strip_prefix(b"%continue ") {
            events.push(ControlEvent::Continue {
                pane_id: parse_single_pane(rest)?,
            });
            return Ok(());
        }
        if line == b"%exit" {
            events.push(ControlEvent::Exit { reason: None });
            return Ok(());
        }
        if let Some(reason) = line.strip_prefix(b"%exit ") {
            events.push(ControlEvent::Exit {
                reason: (!reason.is_empty()).then(|| reason.to_vec()),
            });
            return Ok(());
        }

        let body = &line[1..];
        let (name, arguments) = match body.iter().position(|&byte| byte == b' ') {
            Some(index) => (&body[..index], &body[index + 1..]),
            None => (body, &[][..]),
        };
        if name.is_empty()
            || !name
                .iter()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
        {
            return Err(ControlParseError::MalformedRecord);
        }
        events.push(ControlEvent::Notification {
            name: name.to_vec(),
            arguments: arguments.to_vec(),
        });
        Ok(())
    }

    fn finish_command(&mut self, status: CommandStatus, events: &mut Vec<ControlEvent>) {
        let pending = self
            .pending_command
            .take()
            .expect("command completion requires a pending command");
        events.push(ControlEvent::Command {
            timestamp: pending.tag.timestamp,
            number: pending.tag.number,
            flags: pending.tag.flags,
            status,
            output: pending.output,
        });
    }
}

fn parse_command_tag(bytes: &[u8]) -> Result<CommandTag, ControlParseError> {
    let mut fields = bytes.split(|&byte| byte == b' ');
    let timestamp = parse_u64(fields.next(), "command timestamp")?;
    let number = parse_u64(fields.next(), "command number")?;
    let flags = parse_u64(fields.next(), "command flags")?;
    if fields.next().is_some() {
        return Err(ControlParseError::MalformedRecord);
    }
    Ok(CommandTag {
        timestamp,
        number,
        flags,
    })
}

fn parse_u64(bytes: Option<&[u8]>, field: &'static str) -> Result<u64, ControlParseError> {
    let bytes = bytes.ok_or(ControlParseError::MalformedRecord)?;
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return Err(ControlParseError::InvalidNumber { field });
    }
    let text =
        std::str::from_utf8(bytes).map_err(|_| ControlParseError::InvalidNumber { field })?;
    text.parse()
        .map_err(|_| ControlParseError::InvalidNumber { field })
}

fn parse_pane_id(bytes: &[u8]) -> Result<u64, ControlParseError> {
    let digits = bytes
        .strip_prefix(b"%")
        .ok_or(ControlParseError::InvalidPaneId)?;
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return Err(ControlParseError::InvalidPaneId);
    }
    let text = std::str::from_utf8(digits).map_err(|_| ControlParseError::InvalidPaneId)?;
    text.parse().map_err(|_| ControlParseError::InvalidPaneId)
}

fn parse_single_pane(bytes: &[u8]) -> Result<u64, ControlParseError> {
    if bytes.contains(&b' ') {
        return Err(ControlParseError::MalformedRecord);
    }
    parse_pane_id(bytes)
}

fn split_once_space(bytes: &[u8]) -> Result<(&[u8], &[u8]), ControlParseError> {
    let index = bytes
        .iter()
        .position(|&byte| byte == b' ')
        .ok_or(ControlParseError::MalformedRecord)?;
    Ok((&bytes[..index], &bytes[index + 1..]))
}

fn parse_extended_output(bytes: &[u8]) -> Result<ControlEvent, ControlParseError> {
    let separator = bytes
        .windows(3)
        .position(|window| window == b" : ")
        .ok_or(ControlParseError::MalformedRecord)?;
    let header = &bytes[..separator];
    let value = &bytes[separator + 3..];
    let mut fields = header.split(|&byte| byte == b' ');
    let pane = fields.next().ok_or(ControlParseError::MalformedRecord)?;
    let age = fields.next();
    let future_fields = fields
        .map(|field| {
            if field.is_empty() {
                Err(ControlParseError::MalformedRecord)
            } else {
                Ok(field.to_vec())
            }
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(ControlEvent::ExtendedOutput {
        pane_id: parse_pane_id(pane)?,
        age_ms: parse_u64(age, "extended-output age")?,
        future_fields,
        bytes: decode_octal(value)?,
    })
}

fn decode_octal(bytes: &[u8]) -> Result<Vec<u8>, ControlParseError> {
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'\\' {
            decoded.push(bytes[index]);
            index += 1;
            continue;
        }
        let digits = bytes
            .get(index + 1..index + 4)
            .ok_or(ControlParseError::InvalidOctalEscape)?;
        if !digits.iter().all(|digit| matches!(digit, b'0'..=b'7')) {
            return Err(ControlParseError::InvalidOctalEscape);
        }
        let value = u16::from(digits[0] - b'0') * 64
            + u16::from(digits[1] - b'0') * 8
            + u16::from(digits[2] - b'0');
        decoded.push(u8::try_from(value).map_err(|_| ControlParseError::InvalidOctalEscape)?);
        index += 4;
    }
    Ok(decoded)
}
