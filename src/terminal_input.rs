use std::borrow::Cow;
use terminput::{
    Encoding, Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, KittyFlags,
};

/// A semantic key event decoded at Lector's terminal boundary.
///
/// `event` identifies the key and modifiers. The text fields preserve Kitty's
/// optional associated-text and shifted alternate-key metadata, which
/// `terminput` does not expose.
#[derive(Clone, Debug)]
pub struct KeyInput {
    event: KeyEvent,
    associated_text: Option<String>,
    alternate_text: Option<String>,
}

impl KeyInput {
    pub(crate) fn new(event: KeyEvent, raw: &[u8]) -> Self {
        Self {
            event,
            associated_text: kitty_associated_text(raw),
            alternate_text: kitty_alternate_text(event, raw),
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
    /// Kitty associated text and shifted alternate keys are authoritative when
    /// present. Legacy input and Kitty's compatibility mode instead represent
    /// printable text directly as a character key.
    pub fn text(&self) -> Option<Cow<'_, str>> {
        if let Some(text) = &self.associated_text {
            return Some(Cow::Borrowed(text));
        }

        let event = self.normalized_event();
        if event.modifiers == KeyModifiers::SHIFT
            && let Some(text) = &self.alternate_text
        {
            return Some(Cow::Borrowed(text));
        }
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
        legacy_control_code(ch)
    }

    /// Transcodes an extended keyboard event for a child using legacy Xterm
    /// input. Input which is already legacy-compatible remains byte-exact.
    pub(crate) fn legacy_child_bytes<'a>(
        &self,
        raw: &'a [u8],
        application_cursor: bool,
        application_keypad: bool,
    ) -> Cow<'a, [u8]> {
        if !is_extended_key_encoding(self.event, raw) {
            return Cow::Borrowed(raw);
        }
        if self.is_release() {
            return Cow::Owned(Vec::new());
        }

        let mut event = self.normalized_event();
        event.kind = KeyEventKind::Press;

        // Super and Hyper have no legacy representation. Sending only the base
        // key would turn a shortcut into unintended text.
        if event
            .modifiers
            .intersects(KeyModifiers::SUPER | KeyModifiers::HYPER)
        {
            return Cow::Owned(Vec::new());
        }

        // Kitty distinguishes Meta from Alt; legacy terminals represent both
        // with an Escape prefix.
        if event.modifiers.contains(KeyModifiers::META) {
            event.modifiers.remove(KeyModifiers::META);
            event.modifiers.insert(KeyModifiers::ALT);
        }

        // Associated text and Kitty's shifted alternate key are authoritative
        // for layouts which cannot be reconstructed from the physical key.
        if let Some(text) = self
            .associated_text
            .as_ref()
            .or(self.alternate_text.as_ref())
            && !event.modifiers.contains(KeyModifiers::CTRL)
        {
            let mut encoded = Vec::with_capacity(text.len().saturating_add(1));
            if event.modifiers.contains(KeyModifiers::ALT) {
                encoded.push(b'\x1B');
            }
            encoded.extend_from_slice(text.as_bytes());
            return Cow::Owned(encoded);
        }

        if event.modifiers.contains(KeyModifiers::CTRL)
            && let KeyCode::Char(ch) = event.code
            && let Some(control) = legacy_control_code(ch)
        {
            let mut encoded = Vec::with_capacity(2);
            if event.modifiers.contains(KeyModifiers::ALT) {
                encoded.push(b'\x1B');
            }
            encoded.push(control);
            return Cow::Owned(encoded);
        }

        let is_keypad = event.state.contains(KeyEventState::KEYPAD);
        if application_keypad
            && is_keypad
            && event.modifiers.is_empty()
            && let Some(sequence) = application_keypad_sequence(event.code)
        {
            return Cow::Owned(sequence.to_vec());
        }
        if application_cursor
            && !is_keypad
            && event.modifiers.is_empty()
            && let Some(sequence) = application_cursor_sequence(event.code)
        {
            return Cow::Owned(sequence.to_vec());
        }

        let mut encoded = [0; 32];
        match Event::Key(event).encode(&mut encoded, Encoding::Xterm) {
            Ok(length) => Cow::Owned(encoded[..length].to_vec()),
            // Releases and keys without a legacy representation must not leak
            // their extended escape syntax into a legacy application's input.
            Err(_) => Cow::Owned(Vec::new()),
        }
    }

    /// Encode a semantic physical key for a child which enabled Kitty's
    /// keyboard protocol. Kitty input already carrying the child's requested
    /// detail is retained byte-for-byte; legacy and modifyOtherKeys input is
    /// upgraded at this boundary.
    pub(crate) fn kitty_child_bytes<'a>(
        &self,
        raw: &'a [u8],
        kitty_keyboard_flags: u8,
        application_cursor: bool,
        application_keypad: bool,
    ) -> Cow<'a, [u8]> {
        if is_kitty_key_encoding(raw) {
            if self.is_release() && kitty_keyboard_flags & 2 == 0 {
                return Cow::Owned(Vec::new());
            }
            return Cow::Borrowed(raw);
        }

        let event = self.normalized_event();
        let report_all_keys = kitty_keyboard_flags & 8 != 0;
        let is_keypad = event.state.contains(KeyEventState::KEYPAD);
        // Kitty retains the legacy application-cursor/keypad encodings for
        // ordinary presses unless all-keys reporting was requested. The raw
        // bytes already reflect the physical mode composed by the renderer.
        if !report_all_keys
            && event.kind == KeyEventKind::Press
            && event.modifiers.is_empty()
            && (application_cursor && !is_keypad || application_keypad && is_keypad)
        {
            return Cow::Borrowed(raw);
        }

        let mut flags = KittyFlags::empty();
        if kitty_keyboard_flags & 1 != 0 {
            flags.insert(KittyFlags::DISAMBIGUATE_ESCAPE_CODES);
        }
        if kitty_keyboard_flags & 2 != 0 {
            flags.insert(KittyFlags::REPORT_EVENT_TYPES);
        }
        if kitty_keyboard_flags & 4 != 0 {
            flags.insert(KittyFlags::REPORT_ALTERNATE_KEYS);
        }
        if kitty_keyboard_flags & 8 != 0 {
            flags.insert(KittyFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES);
        }

        let mut encoded = [0; 64];
        match Event::Key(event).encode(&mut encoded, Encoding::Kitty(flags)) {
            Ok(length) => Cow::Owned(encoded[..length].to_vec()),
            // A legacy representation is preferable to dropping a key which
            // the encoder does not yet model; Kitty applications continue to
            // accept the protocol's legacy-compatible forms.
            Err(_) => Cow::Borrowed(raw),
        }
    }
}

fn is_extended_key_encoding(event: KeyEvent, raw: &[u8]) -> bool {
    let csi_body = raw
        .strip_prefix(b"\x1B[")
        .or_else(|| raw.strip_prefix(b"\x9B"));
    event.kind != KeyEventKind::Press
        || csi_body.is_some_and(|body| body.ends_with(b"u") || body.contains(&b':'))
        || raw
            .strip_prefix(b"\x1B[27;")
            .is_some_and(|body| body.ends_with(b"~"))
}

fn is_kitty_key_encoding(raw: &[u8]) -> bool {
    raw.strip_prefix(b"\x1B[")
        .or_else(|| raw.strip_prefix(b"\x9B"))
        .is_some_and(|body| body.ends_with(b"u") || body.contains(&b':'))
}

fn legacy_control_code(ch: char) -> Option<u8> {
    match ch.to_ascii_lowercase() {
        '@' | ' ' | '2' => Some(0x00),
        'a'..='z' => Some((ch.to_ascii_lowercase() as u8) - b'a' + 1),
        '[' | '3' => Some(0x1B),
        '\\' | '4' => Some(0x1C),
        ']' | '5' => Some(0x1D),
        '^' | '6' => Some(0x1E),
        '_' | '7' | '/' => Some(0x1F),
        '?' | '8' => Some(0x7F),
        _ => None,
    }
}

fn application_cursor_sequence(code: KeyCode) -> Option<&'static [u8]> {
    match code {
        KeyCode::Left => Some(b"\x1BOD"),
        KeyCode::Right => Some(b"\x1BOC"),
        KeyCode::Up => Some(b"\x1BOA"),
        KeyCode::Down => Some(b"\x1BOB"),
        KeyCode::Home => Some(b"\x1BOH"),
        KeyCode::End => Some(b"\x1BOF"),
        _ => None,
    }
}

fn application_keypad_sequence(code: KeyCode) -> Option<&'static [u8]> {
    match code {
        KeyCode::Char('0') | KeyCode::Insert => Some(b"\x1BOp"),
        KeyCode::Char('1') | KeyCode::End => Some(b"\x1BOq"),
        KeyCode::Char('2') | KeyCode::Down => Some(b"\x1BOr"),
        KeyCode::Char('3') | KeyCode::PageDown => Some(b"\x1BOs"),
        KeyCode::Char('4') | KeyCode::Left => Some(b"\x1BOt"),
        KeyCode::Char('5') | KeyCode::KeypadBegin => Some(b"\x1BOu"),
        KeyCode::Char('6') | KeyCode::Right => Some(b"\x1BOv"),
        KeyCode::Char('7') | KeyCode::Home => Some(b"\x1BOw"),
        KeyCode::Char('8') | KeyCode::Up => Some(b"\x1BOx"),
        KeyCode::Char('9') | KeyCode::PageUp => Some(b"\x1BOy"),
        KeyCode::Char('.') | KeyCode::Delete => Some(b"\x1BOn"),
        KeyCode::Char('/') => Some(b"\x1BOo"),
        KeyCode::Char('*') => Some(b"\x1BOj"),
        KeyCode::Char('-') => Some(b"\x1BOm"),
        KeyCode::Char('+') => Some(b"\x1BOk"),
        KeyCode::Enter => Some(b"\x1BOM"),
        KeyCode::Char('=') => Some(b"\x1BOX"),
        KeyCode::Char(',') => Some(b"\x1BOl"),
        _ => None,
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

fn kitty_alternate_text(event: KeyEvent, raw: &[u8]) -> Option<String> {
    if !event.modifiers.contains(KeyModifiers::SHIFT) {
        return None;
    }
    let body = raw.strip_prefix(b"\x1B[")?.strip_suffix(b"u")?;
    let body = std::str::from_utf8(body).ok()?;
    let key_codes = body.split(';').next()?;
    let mut codes = key_codes.split(':');
    codes.next()?.parse::<u32>().ok()?;
    let alternate = codes.next()?.parse::<u32>().ok()?;
    let ch = char::from_u32(alternate)?;
    (!ch.is_control()).then(|| ch.to_string())
}

#[cfg(test)]
mod tests {
    use super::{KeyInput, kitty_associated_text};
    use terminput::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

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

        let terminput::Event::Key(uppercase_event) = terminput::Event::parse_from(b"T")
            .expect("parse uppercase legacy key")
            .expect("uppercase key event")
        else {
            panic!("uppercase input was not a key event");
        };
        let uppercase = KeyInput::new(uppercase_event, b"T");
        assert_eq!(uppercase.text().as_deref(), Some("T"));

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
    fn extended_keys_fall_back_to_legacy_xterm_sequences() {
        let cases = [
            (
                KeyEvent::new(KeyCode::Enter).modifiers(KeyModifiers::SHIFT),
                b"\x1B[27;2;13~".as_slice(),
                b"\r".as_slice(),
            ),
            (
                KeyEvent::new(KeyCode::Enter).modifiers(KeyModifiers::SHIFT),
                b"\x1B[13;2u".as_slice(),
                b"\r".as_slice(),
            ),
            (
                KeyEvent::new(KeyCode::Char('d')).modifiers(KeyModifiers::CTRL),
                b"\x1B[27;5;100~".as_slice(),
                b"\x04".as_slice(),
            ),
            (
                KeyEvent::new(KeyCode::Char('b')).modifiers(KeyModifiers::ALT),
                b"\x1B[98;3u".as_slice(),
                b"\x1Bb".as_slice(),
            ),
            (
                KeyEvent::new(KeyCode::Tab).modifiers(KeyModifiers::SHIFT),
                b"\x1B[9;2u".as_slice(),
                b"\x1B[Z".as_slice(),
            ),
            (
                KeyEvent::new(KeyCode::Left),
                b"\x1B[1;1:1D".as_slice(),
                b"\x1B[D".as_slice(),
            ),
            (
                KeyEvent::new(KeyCode::Left).kind(KeyEventKind::Repeat),
                b"\x1B[1;1:2D".as_slice(),
                b"\x1B[D".as_slice(),
            ),
            (
                KeyEvent::new(KeyCode::Left).kind(KeyEventKind::Release),
                b"\x1B[1;1:3D".as_slice(),
                b"".as_slice(),
            ),
        ];

        for (event, raw, expected) in cases {
            let input = KeyInput::new(event, raw);
            assert_eq!(
                input.legacy_child_bytes(raw, false, false).as_ref(),
                expected,
                "event={event:?} raw={raw:?}"
            );
        }
    }

    #[test]
    fn legacy_fallback_covers_every_xterm_key_family() {
        let cases = [
            (KeyEvent::new(KeyCode::Esc), b"\x1B".as_slice()),
            (KeyEvent::new(KeyCode::Enter), b"\r".as_slice()),
            (KeyEvent::new(KeyCode::Tab), b"\t".as_slice()),
            (KeyEvent::new(KeyCode::Backspace), b"\x7F".as_slice()),
            (KeyEvent::new(KeyCode::Left), b"\x1B[D".as_slice()),
            (KeyEvent::new(KeyCode::Right), b"\x1B[C".as_slice()),
            (KeyEvent::new(KeyCode::Up), b"\x1B[A".as_slice()),
            (KeyEvent::new(KeyCode::Down), b"\x1B[B".as_slice()),
            (KeyEvent::new(KeyCode::Home), b"\x1B[H".as_slice()),
            (KeyEvent::new(KeyCode::End), b"\x1B[F".as_slice()),
            (KeyEvent::new(KeyCode::PageUp), b"\x1B[5~".as_slice()),
            (KeyEvent::new(KeyCode::PageDown), b"\x1B[6~".as_slice()),
            (KeyEvent::new(KeyCode::Insert), b"\x1B[2~".as_slice()),
            (KeyEvent::new(KeyCode::Delete), b"\x1B[3~".as_slice()),
            (KeyEvent::new(KeyCode::F(1)), b"\x1BOP".as_slice()),
            (KeyEvent::new(KeyCode::F(4)), b"\x1BOS".as_slice()),
            (KeyEvent::new(KeyCode::F(5)), b"\x1B[15~".as_slice()),
            (KeyEvent::new(KeyCode::F(12)), b"\x1B[24~".as_slice()),
            (KeyEvent::new(KeyCode::Char('x')), b"x".as_slice()),
        ];

        for (event, expected) in cases {
            let raw = b"\x1B[1u";
            let input = KeyInput::new(event, raw);
            assert_eq!(
                input.legacy_child_bytes(raw, false, false).as_ref(),
                expected,
                "event={event:?}"
            );
        }

        for function in 1..=12 {
            let raw = b"\x1B[1u";
            let input = KeyInput::new(KeyEvent::new(KeyCode::F(function)), raw);
            assert!(
                !input
                    .legacy_child_bytes(raw, false, false)
                    .as_ref()
                    .is_empty(),
                "F{function} had no legacy encoding"
            );
        }
    }

    #[test]
    fn legacy_fallback_drops_keys_with_no_safe_legacy_representation() {
        for event in [
            KeyEvent::new(KeyCode::CapsLock),
            KeyEvent::new(KeyCode::Char('x')).modifiers(KeyModifiers::SUPER),
            KeyEvent::new(KeyCode::Char('x')).modifiers(KeyModifiers::HYPER),
        ] {
            let raw = b"\x1B[1u";
            let input = KeyInput::new(event, raw);
            assert!(
                input
                    .legacy_child_bytes(raw, false, false)
                    .as_ref()
                    .is_empty(),
                "event={event:?}"
            );
        }
    }

    #[test]
    fn legacy_fallback_uses_kitty_text_and_application_modes() {
        let shifted = b"\x1B[45:95;2u";
        let input = KeyInput::new(
            KeyEvent::new(KeyCode::Char('-')).modifiers(KeyModifiers::SHIFT),
            shifted,
        );
        assert_eq!(
            input.legacy_child_bytes(shifted, false, false).as_ref(),
            b"_"
        );

        let cursor = b"\x1B[1;1:1D";
        let input = KeyInput::new(KeyEvent::new(KeyCode::Left), cursor);
        assert_eq!(
            input.legacy_child_bytes(cursor, true, false).as_ref(),
            b"\x1BOD"
        );

        let keypad = b"\x1B[57414u";
        let input = KeyInput::new(
            KeyEvent::new(KeyCode::Enter).state(KeyEventState::KEYPAD),
            keypad,
        );
        assert_eq!(
            input.legacy_child_bytes(keypad, false, true).as_ref(),
            b"\x1BOM"
        );
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
