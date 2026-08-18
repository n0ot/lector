//! Binary-safe input and outer-coordinate translation for tmux control mode.

use crate::{
    terminal::{MouseEncoding, MouseProtocol, TerminalGeometry},
    tmux_model::PaneId,
    tmux_panes::LayoutPane,
};
use terminput::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind, ScrollDirection};
use thiserror::Error;

/// Keeps a control command small enough for predictable batching while still
/// amortizing the fixed command/reply overhead across ordinary typing.
pub const MAX_SEND_KEYS_COMMAND_BYTES: usize = 4096;

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum TmuxInputError {
    #[error("tmux send-keys command prefix exceeds its configured bound")]
    CommandPrefixTooLong,
}

/// Encode arbitrary pane input without interpreting it as tmux command syntax.
///
/// `send-keys -H` accepts one hexadecimal ASCII byte per argument. Keeping all
/// payload bytes out of the command grammar also makes NUL, newlines, quotes,
/// semicolons, and invalid UTF-8 safe.
pub fn encode_send_keys(pane_id: PaneId, input: &[u8]) -> Result<Vec<Vec<u8>>, TmuxInputError> {
    if input.is_empty() {
        return Ok(Vec::new());
    }
    let prefix = format!("send-keys -H -t %{} ", pane_id.0);
    let available = MAX_SEND_KEYS_COMMAND_BYTES
        .checked_sub(prefix.len() + 1)
        .ok_or(TmuxInputError::CommandPrefixTooLong)?;
    let chunk_bytes = available / 3;
    if chunk_bytes == 0 {
        return Err(TmuxInputError::CommandPrefixTooLong);
    }

    let mut commands = Vec::with_capacity(input.len().div_ceil(chunk_bytes));
    for chunk in input.chunks(chunk_bytes) {
        let mut command = Vec::with_capacity(prefix.len() + chunk.len() * 3);
        command.extend_from_slice(prefix.as_bytes());
        for (index, byte) in chunk.iter().enumerate() {
            if index != 0 {
                command.push(b' ');
            }
            command.push(hex_digit(byte >> 4));
            command.push(hex_digit(byte & 0x0f));
        }
        command.push(b'\n');
        debug_assert!(command.len() <= MAX_SEND_KEYS_COMMAND_BYTES);
        commands.push(command);
    }
    Ok(commands)
}

#[must_use]
pub fn refresh_client_command(geometry: TerminalGeometry) -> Vec<u8> {
    format!(
        "refresh-client -C {}x{}\n",
        geometry.cols.max(1),
        geometry.rows.max(1)
    )
    .into_bytes()
}

#[must_use]
pub fn continue_pane_command(pane_id: PaneId) -> Vec<u8> {
    // Control-mode stdin is parsed as tmux command language, where the
    // pane:state argument must remain one token. Unlike argv passed to the
    // tmux executable, the colon-bearing value therefore needs quoting.
    format!("refresh-client -A '%{}:continue'\n", pane_id.0).into_bytes()
}

#[must_use]
pub fn pause_pane_command(pane_id: PaneId) -> Vec<u8> {
    format!("refresh-client -A '%{}:pause'\n", pane_id.0).into_bytes()
}

/// Convert only OSC 10/11 replies produced by a pane's shadow terminal into
/// control-client reports. Other shadow replies still belong to tmux and are
/// deliberately discarded rather than injected into the pane as input.
#[must_use]
pub fn refresh_client_report_commands(pane_id: PaneId, replies: &[u8]) -> Vec<Vec<u8>> {
    let mut commands = Vec::new();
    let mut offset = 0;
    while offset < replies.len() {
        let Some(relative_start) = replies[offset..]
            .windows(2)
            .position(|window| window == b"\x1b]")
        else {
            break;
        };
        let start = offset + relative_start;
        let mut cursor = start + 2;
        let end = loop {
            let Some(byte) = replies.get(cursor).copied() else {
                return commands;
            };
            if byte == b'\x07' {
                break cursor + 1;
            }
            if byte == b'\x1b' && replies.get(cursor + 1) == Some(&b'\\') {
                break cursor + 2;
            }
            cursor += 1;
        };
        let report = &replies[start..end];
        let eligible = report.starts_with(b"\x1b]10;") || report.starts_with(b"\x1b]11;");
        if eligible
            && report.len() <= 256
            && !report
                .iter()
                .any(|byte| matches!(byte, b'\'' | b'\n' | b'\r'))
        {
            let mut command = format!("refresh-client -r '%{}:", pane_id.0).into_bytes();
            command.extend_from_slice(report);
            command.extend_from_slice(b"'\n");
            commands.push(command);
        }
        offset = end;
    }
    commands
}

/// Translate a physical-terminal mouse event into the active pane's local
/// coordinate space and requested encoding. Events outside the pane and events
/// not selected by its mouse protocol are rejected.
#[must_use]
pub fn translate_mouse(
    event: MouseEvent,
    pane: LayoutPane,
    protocol: MouseProtocol,
    encoding: MouseEncoding,
) -> Option<Vec<u8>> {
    if !protocol_reports(protocol, event.kind) {
        return None;
    }
    let col = i32::from(event.column).checked_sub(pane.origin.col)?;
    let row = i32::from(event.row).checked_sub(pane.origin.row)?;
    if col < 0 || row < 0 || col >= i32::from(pane.cols) || row >= i32::from(pane.rows) {
        return None;
    }
    let col = u32::try_from(col).ok()?;
    let row = u32::try_from(row).ok()?;
    encode_mouse(event, row, col, encoding)
}

fn protocol_reports(protocol: MouseProtocol, kind: MouseEventKind) -> bool {
    match kind {
        MouseEventKind::Down(_) | MouseEventKind::Scroll(_) => protocol != MouseProtocol::None,
        MouseEventKind::Up(_) => !matches!(protocol, MouseProtocol::None | MouseProtocol::Press),
        MouseEventKind::Drag(_) => {
            matches!(
                protocol,
                MouseProtocol::ButtonMotion | MouseProtocol::AnyMotion
            )
        }
        MouseEventKind::Moved => protocol == MouseProtocol::AnyMotion,
    }
}

fn encode_mouse(event: MouseEvent, row: u32, col: u32, encoding: MouseEncoding) -> Option<Vec<u8>> {
    let mut code: u32 = match event.kind {
        MouseEventKind::Down(MouseButton::Left | MouseButton::Unknown)
        | MouseEventKind::Up(MouseButton::Left | MouseButton::Unknown) => 0,
        MouseEventKind::Down(MouseButton::Middle) | MouseEventKind::Up(MouseButton::Middle) => 1,
        MouseEventKind::Down(MouseButton::Right) | MouseEventKind::Up(MouseButton::Right) => 2,
        MouseEventKind::Drag(MouseButton::Left | MouseButton::Unknown) => 32,
        MouseEventKind::Drag(MouseButton::Middle) => 33,
        MouseEventKind::Drag(MouseButton::Right) => 34,
        MouseEventKind::Moved => 35,
        MouseEventKind::Scroll(ScrollDirection::Up) => 64,
        MouseEventKind::Scroll(ScrollDirection::Down) => 65,
        MouseEventKind::Scroll(ScrollDirection::Left) => 66,
        MouseEventKind::Scroll(ScrollDirection::Right) => 67,
    };
    if event.modifiers.contains(KeyModifiers::SHIFT) {
        code += 4;
    }
    if event.modifiers.contains(KeyModifiers::ALT) {
        code += 8;
    }
    if event.modifiers.contains(KeyModifiers::CTRL) {
        code += 16;
    }

    match encoding {
        MouseEncoding::Sgr => Some(
            format!(
                "\x1b[<{code};{};{}{}",
                col + 1,
                row + 1,
                if matches!(event.kind, MouseEventKind::Up(_)) {
                    'm'
                } else {
                    'M'
                }
            )
            .into_bytes(),
        ),
        MouseEncoding::Default | MouseEncoding::Utf8 => {
            if matches!(event.kind, MouseEventKind::Up(_)) {
                code = 3;
                if event.modifiers.contains(KeyModifiers::SHIFT) {
                    code += 4;
                }
                if event.modifiers.contains(KeyModifiers::ALT) {
                    code += 8;
                }
                if event.modifiers.contains(KeyModifiers::CTRL) {
                    code += 16;
                }
            }
            let code = code.checked_add(32)?;
            let col = col.checked_add(33)?;
            let row = row.checked_add(33)?;
            let mut bytes = b"\x1b[M".to_vec();
            if encoding == MouseEncoding::Default {
                bytes.extend_from_slice(&[
                    u8::try_from(code).ok()?,
                    u8::try_from(col).ok()?,
                    u8::try_from(row).ok()?,
                ]);
            } else {
                push_utf8_scalar(&mut bytes, code)?;
                push_utf8_scalar(&mut bytes, col)?;
                push_utf8_scalar(&mut bytes, row)?;
            }
            Some(bytes)
        }
    }
}

fn push_utf8_scalar(bytes: &mut Vec<u8>, value: u32) -> Option<()> {
    let character = char::from_u32(value)?;
    let mut buffer = [0; 4];
    bytes.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
    Some(())
}

const fn hex_digit(value: u8) -> u8 {
    match value {
        0..=9 => b'0' + value,
        _ => b'a' + value - 10,
    }
}
