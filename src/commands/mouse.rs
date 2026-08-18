use super::{CommandResult, Result};
use crate::{
    screen_reader::ScreenReader,
    terminal::{MouseEncoding, MouseProtocol},
    view::View,
};

#[derive(Copy, Clone)]
pub(super) enum Button {
    Left,
    Right,
}

pub(super) fn click(sr: &mut ScreenReader, view: &View, button: Button) -> Result<CommandResult> {
    // The coordinate comes from the user's presented review cursor, but the
    // protocol belongs to the mutable application endpoint. A mode change
    // immediately before the click has the same ordinary UI race as an
    // element moving immediately before a physical mouse press.
    let screen = view.live_screen();
    if screen.mouse_protocol_mode() == MouseProtocol::None {
        sr.speak("mouse input unavailable", false)?;
        return Ok(CommandResult::Handled);
    }

    let Some(input) = encode_click(
        screen.mouse_protocol_mode(),
        screen.mouse_protocol_encoding(),
        button,
        view.review_cursor_position(),
    ) else {
        sr.speak("mouse position unavailable", false)?;
        return Ok(CommandResult::Handled);
    };

    Ok(CommandResult::PtyInput(input))
}

fn encode_click(
    mode: MouseProtocol,
    encoding: MouseEncoding,
    button: Button,
    (row, col): (u16, u16),
) -> Option<Vec<u8>> {
    let button_code = match button {
        Button::Left => 0,
        Button::Right => 2,
    };
    let include_release = mode != MouseProtocol::Press;
    let mut input = Vec::new();

    match encoding {
        MouseEncoding::Sgr => {
            input.extend_from_slice(
                format!(
                    "\x1B[<{button_code};{};{}M",
                    u32::from(col) + 1,
                    u32::from(row) + 1
                )
                .as_bytes(),
            );
            if include_release {
                input.extend_from_slice(
                    format!(
                        "\x1B[<{button_code};{};{}m",
                        u32::from(col) + 1,
                        u32::from(row) + 1
                    )
                    .as_bytes(),
                );
            }
        }
        MouseEncoding::Default => {
            let col = u8::try_from(u32::from(col) + 33).ok()?;
            let row = u8::try_from(u32::from(row) + 33).ok()?;
            input.extend_from_slice(&[0x1B, b'[', b'M', button_code + 32, col, row]);
            if include_release {
                input.extend_from_slice(&[0x1B, b'[', b'M', 35, col, row]);
            }
        }
        MouseEncoding::Utf8 => {
            let col = char::from_u32(u32::from(col) + 33)?;
            let row = char::from_u32(u32::from(row) + 33)?;
            let mut col_buf = [0; 4];
            let mut row_buf = [0; 4];
            let col = col.encode_utf8(&mut col_buf).as_bytes();
            let row = row.encode_utf8(&mut row_buf).as_bytes();
            input.extend_from_slice(b"\x1B[M");
            input.push(button_code + 32);
            input.extend_from_slice(col);
            input.extend_from_slice(row);
            if include_release {
                input.extend_from_slice(b"\x1B[M#");
                input.extend_from_slice(col);
                input.extend_from_slice(row);
            }
        }
    }

    Some(input)
}

#[cfg(test)]
mod tests {
    use super::{Button, encode_click};
    use crate::terminal::{MouseEncoding, MouseProtocol};

    #[test]
    fn encodes_sgr_left_click_with_one_based_coordinates() {
        let input = encode_click(
            MouseProtocol::PressRelease,
            MouseEncoding::Sgr,
            Button::Left,
            (4, 7),
        )
        .unwrap();
        assert_eq!(input, b"\x1B[<0;8;5M\x1B[<0;8;5m");
    }

    #[test]
    fn encodes_sgr_right_click() {
        let input = encode_click(
            MouseProtocol::ButtonMotion,
            MouseEncoding::Sgr,
            Button::Right,
            (0, 0),
        )
        .unwrap();
        assert_eq!(input, b"\x1B[<2;1;1M\x1B[<2;1;1m");
    }

    #[test]
    fn x10_mode_sends_only_button_press() {
        let input = encode_click(
            MouseProtocol::Press,
            MouseEncoding::Default,
            Button::Left,
            (1, 2),
        )
        .unwrap();
        assert_eq!(input, b"\x1B[M #\"");
    }

    #[test]
    fn default_encoding_rejects_coordinates_it_cannot_represent() {
        assert!(
            encode_click(
                MouseProtocol::PressRelease,
                MouseEncoding::Default,
                Button::Left,
                (0, 223),
            )
            .is_none()
        );
    }

    #[test]
    fn encodes_utf8_mouse_click() {
        let input = encode_click(
            MouseProtocol::PressRelease,
            MouseEncoding::Utf8,
            Button::Right,
            (95, 95),
        )
        .unwrap();
        assert_eq!(input, b"\x1B[M\"\xC2\x80\xC2\x80\x1B[M#\xC2\x80\xC2\x80");
    }
}
