use std::borrow::Cow;
use terminput::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// A semantic key event decoded at Lector's terminal boundary.
///
/// `event` identifies the key and modifiers. `text` preserves the optional
/// associated-text field from the Kitty keyboard protocol, which `terminput`
/// intentionally does not expose.
#[derive(Clone, Debug)]
pub struct KeyInput {
    event: KeyEvent,
    text: Option<String>,
}

impl KeyInput {
    pub(crate) fn new(event: KeyEvent, raw: &[u8]) -> Self {
        Self {
            event,
            text: kitty_associated_text(raw),
        }
    }

    pub fn event(&self) -> KeyEvent {
        self.event
    }

    pub fn normalized_event(&self) -> KeyEvent {
        self.event.normalize_case()
    }

    pub fn is_release(&self) -> bool {
        self.event.kind == KeyEventKind::Release
    }

    /// Text produced by this key press, independent of the terminal encoding.
    ///
    /// Kitty associated text is authoritative when present. Legacy input and
    /// Kitty's compatibility mode instead represent printable text directly as
    /// a character key.
    pub fn text(&self) -> Option<Cow<'_, str>> {
        if let Some(text) = &self.text {
            return Some(Cow::Borrowed(text));
        }

        let event = self.normalized_event();
        if event.modifiers.intersects(
            KeyModifiers::CTRL
                | KeyModifiers::ALT
                | KeyModifiers::META
                | KeyModifiers::SUPER
                | KeyModifiers::HYPER,
        ) {
            return None;
        }
        match event.code {
            KeyCode::Char(ch) => Some(Cow::Owned(ch.to_string())),
            _ => None,
        }
    }

    /// Returns the legacy C0 byte represented by an unambiguous Control-key
    /// combination. Shift is ignored because legacy terminals cannot distinguish
    /// `Ctrl+A` from `Ctrl+Shift+A`.
    pub fn control_code(&self) -> Option<u8> {
        let event = self.normalized_event();
        if !event.modifiers.contains(KeyModifiers::CTRL)
            || event.modifiers.intersects(
                KeyModifiers::ALT | KeyModifiers::META | KeyModifiers::SUPER | KeyModifiers::HYPER,
            )
        {
            return None;
        }
        let KeyCode::Char(ch) = event.code else {
            return None;
        };
        let ch = ch.to_ascii_lowercase();
        match ch {
            '@' | ' ' | '2' => Some(0x00),
            'a'..='z' => Some((ch as u8) - b'a' + 1),
            '[' | '3' => Some(0x1B),
            '\\' | '4' => Some(0x1C),
            ']' | '5' => Some(0x1D),
            '^' | '6' => Some(0x1E),
            '_' | '7' | '/' => Some(0x1F),
            '?' | '8' => Some(0x7F),
            _ => None,
        }
    }
}

fn kitty_associated_text(raw: &[u8]) -> Option<String> {
    let body = raw.strip_prefix(b"\x1B[")?.strip_suffix(b"u")?;
    let body = std::str::from_utf8(body).ok()?;
    let mut fields = body.split(';');

    let key_codes = fields.next()?;
    let modifiers = fields.next()?;
    let text = fields.next()?;
    if fields.next().is_some() || text.is_empty() {
        return None;
    }

    let mut key_code_count = 0;
    for code in key_codes.split(':') {
        code.parse::<u32>().ok()?;
        key_code_count += 1;
    }
    if !(1..=3).contains(&key_code_count) {
        return None;
    }
    if !modifiers.is_empty() {
        let mut modifier_parts = modifiers.split(':');
        modifier_parts.next()?.parse::<u16>().ok()?;
        if let Some(kind) = modifier_parts.next() {
            kind.parse::<u8>().ok()?;
        }
        if modifier_parts.next().is_some() {
            return None;
        }
    }

    let mut result = String::new();
    for codepoint in text.split(':') {
        let ch = codepoint.parse::<u32>().ok().and_then(char::from_u32)?;
        if ch.is_control() {
            return None;
        }
        result.push(ch);
    }
    (!result.is_empty()).then_some(result)
}

#[cfg(test)]
mod tests {
    use super::{KeyInput, kitty_associated_text};
    use terminput::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn extracts_single_and_multiple_kitty_text_codepoints() {
        assert_eq!(kitty_associated_text(b"\x1B[45;2;95u"), Some("_".into()));
        assert_eq!(
            kitty_associated_text(b"\x1B[0;;101:769u"),
            Some("e\u{301}".into())
        );
    }

    #[test]
    fn rejects_non_kitty_malformed_and_control_text() {
        for raw in [
            b"a".as_slice(),
            b"\x1B[97;1u",
            b"\x1B[?1;1;97u",
            b"\x1B[97;1;10u",
            b"\x1B[97;1;not-a-numberu",
            b"\x1B[97;1;97;98u",
        ] {
            assert_eq!(kitty_associated_text(raw), None, "raw={raw:?}");
        }
    }

    #[test]
    fn derives_legacy_text_and_control_codes_semantically() {
        let shifted = KeyInput::new(
            KeyEvent::new(KeyCode::Char('_')).modifiers(KeyModifiers::SHIFT),
            b"_",
        );
        assert_eq!(shifted.text().as_deref(), Some("_"));

        for (ch, modifiers, expected) in [
            ('a', KeyModifiers::CTRL, 0x01),
            ('A', KeyModifiers::CTRL | KeyModifiers::SHIFT, 0x01),
            ('h', KeyModifiers::CTRL, 0x08),
            ('4', KeyModifiers::CTRL, 0x1C),
            ('?', KeyModifiers::CTRL | KeyModifiers::SHIFT, 0x7F),
        ] {
            let input = KeyInput::new(KeyEvent::new(KeyCode::Char(ch)).modifiers(modifiers), b"");
            assert_eq!(input.control_code(), Some(expected));
            assert_eq!(input.text(), None);
        }
    }

    #[test]
    fn kitty_associated_text_overrides_the_physical_key() {
        let input = KeyInput::new(
            KeyEvent::new(KeyCode::Char('-')).modifiers(KeyModifiers::SHIFT),
            b"\x1B[45;2;95u",
        );
        assert_eq!(input.text().as_deref(), Some("_"));
    }

    #[test]
    fn parses_pure_text_with_an_omitted_modifier_field() {
        let terminput::Event::Key(event) = terminput::Event::parse_from(b"\x1B[0;;229u")
            .expect("parse Kitty event")
            .expect("complete Kitty event")
        else {
            panic!("expected key event");
        };
        let input = KeyInput::new(event, b"\x1B[0;;229u");
        assert_eq!(input.text().as_deref(), Some("å"));
    }
}
