//! Physical-terminal probing, application reply routing, and protocol policy.
//!
//! The physical terminal and the terminal Lector exposes to applications are
//! deliberately different profiles. Outer replies are consumed here; source
//! application queries are answered from the virtual profile attached to that
//! source's Ghostty engine.

use crate::terminal::{TerminalEvent, TerminalGeometry};
use std::collections::BTreeMap;

const PROBE_TIMEOUT_MS: u128 = 50;
const MAX_PROBE_REPLY_BYTES: usize = 4_096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ColorScheme {
    Light,
    Dark,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TerminfoCapabilities {
    pub color_count: Option<u16>,
    pub true_color: bool,
    pub hyperlinks: bool,
    pub synchronized_output: bool,
    pub kitty_keyboard: bool,
    pub kitty_graphics: bool,
}

impl TerminfoCapabilities {
    /// Parse the stable, one-capability-per-line form emitted by
    /// `infocmp -1 -x`. Unknown capabilities remain conservative.
    pub fn from_infocmp(output: &str) -> Self {
        let mut result = Self::default();
        for line in output.lines().map(str::trim) {
            if let Some(value) = line
                .strip_prefix("colors#")
                .and_then(|value| value.strip_suffix(','))
                .and_then(|value| value.parse::<u16>().ok())
            {
                result.color_count = Some(value);
            }
            let name = line.trim_end_matches(',');
            match name {
                "Tc" | "RGB" => result.true_color = true,
                "Sync" => result.synchronized_output = true,
                "Su" => result.kitty_keyboard = true,
                "Gfx" => result.kitty_graphics = true,
                "OSC8" => result.hyperlinks = true,
                _ => {}
            }
        }
        result
    }

    pub fn detect(term: &std::ffi::OsStr) -> Option<Self> {
        let output = std::process::Command::new("infocmp")
            .args([std::ffi::OsStr::new("-1"), std::ffi::OsStr::new("-x"), term])
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| Self::from_infocmp(&String::from_utf8_lossy(&output.stdout)))
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProbeReport {
    pub geometry: Option<TerminalGeometry>,
    pub color_scheme: Option<ColorScheme>,
    pub synchronized_output: Option<bool>,
    pub kitty_keyboard: Option<bool>,
    pub kitty_graphics: Option<bool>,
    pub focus_reporting: Option<bool>,
    pub clipboard_read: Option<bool>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CapabilityOverrides {
    pub color_count: Option<u16>,
    pub true_color: Option<bool>,
    pub hyperlinks: Option<bool>,
    pub synchronized_output: Option<bool>,
    pub kitty_keyboard: Option<bool>,
    pub kitty_graphics: Option<bool>,
    pub focus_reporting: Option<bool>,
    pub clipboard_read: Option<bool>,
}

impl CapabilityOverrides {
    pub fn from_pairs<I, K, V>(pairs: I) -> Result<Self, String>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let mut result = Self::default();
        for (key, value) in pairs {
            let key = key.as_ref();
            let value = value.as_ref();
            if key == "LECTOR_OUTER_COLORS" {
                result.color_count = Some(value.parse::<u16>().map_err(|_| {
                    "LECTOR_OUTER_COLORS must be an integer from 0 through 65535".to_owned()
                })?);
                continue;
            }
            let parsed = parse_explicit_bool(key, value)?;
            match key {
                "LECTOR_OUTER_TRUE_COLOR" => result.true_color = Some(parsed),
                "LECTOR_OUTER_HYPERLINKS" => result.hyperlinks = Some(parsed),
                "LECTOR_OUTER_SYNC" => result.synchronized_output = Some(parsed),
                "LECTOR_OUTER_KITTY_KEYBOARD" => result.kitty_keyboard = Some(parsed),
                "LECTOR_OUTER_KITTY_GRAPHICS" => result.kitty_graphics = Some(parsed),
                "LECTOR_OUTER_FOCUS" => result.focus_reporting = Some(parsed),
                "LECTOR_OUTER_CLIPBOARD_READ" => result.clipboard_read = Some(parsed),
                _ => return Err(format!("unknown capability override {key}")),
            }
        }
        Ok(result)
    }

    pub fn from_environment() -> Result<Self, String> {
        const KEYS: [&str; 8] = [
            "LECTOR_OUTER_COLORS",
            "LECTOR_OUTER_TRUE_COLOR",
            "LECTOR_OUTER_HYPERLINKS",
            "LECTOR_OUTER_SYNC",
            "LECTOR_OUTER_KITTY_KEYBOARD",
            "LECTOR_OUTER_KITTY_GRAPHICS",
            "LECTOR_OUTER_FOCUS",
            "LECTOR_OUTER_CLIPBOARD_READ",
        ];
        Self::from_pairs(
            KEYS.into_iter()
                .filter_map(|key| std::env::var(key).ok().map(|value| (key, value))),
        )
    }
}

fn parse_explicit_bool(key: &str, value: &str) -> Result<bool, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(format!(
            "{key} must be one of true/false, yes/no, on/off, or 1/0"
        )),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PhysicalTerminalProfile {
    pub geometry: TerminalGeometry,
    pub color_count: u16,
    pub true_color: bool,
    pub color_scheme: Option<ColorScheme>,
    pub hyperlinks: bool,
    pub synchronized_output: bool,
    pub kitty_keyboard: bool,
    pub kitty_graphics: bool,
    pub focus_reporting: bool,
    pub clipboard_read: bool,
}

impl PhysicalTerminalProfile {
    pub const fn conservative(geometry: TerminalGeometry) -> Self {
        Self {
            geometry,
            color_count: 8,
            true_color: false,
            color_scheme: None,
            hyperlinks: false,
            synchronized_output: false,
            kitty_keyboard: false,
            kitty_graphics: false,
            focus_reporting: false,
            clipboard_read: false,
        }
    }

    pub fn apply_terminfo(&mut self, terminfo: &TerminfoCapabilities) {
        if let Some(colors) = terminfo.color_count {
            self.color_count = colors;
        }
        self.true_color |= terminfo.true_color;
        self.hyperlinks |= terminfo.hyperlinks;
        self.synchronized_output |= terminfo.synchronized_output;
        self.kitty_keyboard |= terminfo.kitty_keyboard;
        self.kitty_graphics |= terminfo.kitty_graphics;
    }

    pub fn apply_probe(&mut self, probe: &ProbeReport) {
        if let Some(geometry) = probe.geometry {
            self.geometry = geometry;
        }
        if let Some(scheme) = probe.color_scheme {
            self.color_scheme = Some(scheme);
        }
        apply_option(&mut self.synchronized_output, probe.synchronized_output);
        apply_option(&mut self.kitty_keyboard, probe.kitty_keyboard);
        apply_option(&mut self.kitty_graphics, probe.kitty_graphics);
        apply_option(&mut self.focus_reporting, probe.focus_reporting);
        apply_option(&mut self.clipboard_read, probe.clipboard_read);
    }

    pub fn apply_overrides(&mut self, overrides: &CapabilityOverrides) {
        if let Some(colors) = overrides.color_count {
            self.color_count = colors;
        }
        apply_option(&mut self.true_color, overrides.true_color);
        apply_option(&mut self.hyperlinks, overrides.hyperlinks);
        apply_option(&mut self.synchronized_output, overrides.synchronized_output);
        apply_option(&mut self.kitty_keyboard, overrides.kitty_keyboard);
        apply_option(&mut self.kitty_graphics, overrides.kitty_graphics);
        apply_option(&mut self.focus_reporting, overrides.focus_reporting);
        apply_option(&mut self.clipboard_read, overrides.clipboard_read);
    }
}

fn apply_option(target: &mut bool, value: Option<bool>) {
    if let Some(value) = value {
        *target = value;
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProbePolicy {
    pub clipboard_read: bool,
}

impl ProbePolicy {
    pub const fn safe() -> Self {
        Self {
            clipboard_read: false,
        }
    }
}

/// Consumes only bounded replies to Lector-owned startup probes. All bytes
/// which are not recognized as such a reply are returned as ordinary input.
pub struct StartupProbeBroker {
    profile: PhysicalTerminalProfile,
    report: ProbeReport,
    policy: ProbePolicy,
    started_at_ms: u128,
    started: bool,
    finished: bool,
    pending: Vec<u8>,
    discarding_oversized: Option<OversizedSequence>,
    malformed_replies: usize,
    pixel_width: Option<u32>,
    pixel_height: Option<u32>,
    foreground_luminance: Option<u32>,
    background_luminance: Option<u32>,
}

impl StartupProbeBroker {
    pub fn new(profile: PhysicalTerminalProfile, policy: ProbePolicy, started_at_ms: u128) -> Self {
        Self {
            profile,
            report: ProbeReport::default(),
            policy,
            started_at_ms,
            started: false,
            finished: false,
            pending: Vec::new(),
            discarding_oversized: None,
            malformed_replies: 0,
            pixel_width: None,
            pixel_height: None,
            foreground_luminance: None,
            background_luminance: None,
        }
    }

    pub fn startup_queries(&mut self) -> Vec<u8> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        let mut queries = b"\x1b[c\x1b[>c\x1b[=c\x1b[14t\x1b[16t\x1b[18t\x1b[?1004$p\x1b[?2026$p\x1b[?u\x1b]10;?\x1b\\\x1b]11;?\x1b\\".to_vec();
        if self.policy.clipboard_read {
            queries.extend_from_slice(b"\x1b]52;c;?\x1b\\");
        }
        queries
    }

    pub fn ingest(&mut self, input: &[u8], now_ms: u128) -> Vec<u8> {
        if self.finished || now_ms.saturating_sub(self.started_at_ms) > PROBE_TIMEOUT_MS {
            let mut output = std::mem::take(&mut self.pending);
            output.extend_from_slice(input);
            self.finished = true;
            return output;
        }
        let mut output = Vec::new();
        for &byte in input {
            if let Some(discarding) = self.discarding_oversized.as_mut() {
                if discarding.consume(byte) {
                    self.discarding_oversized = None;
                }
                continue;
            }

            self.pending.push(byte);
            loop {
                if self.pending.is_empty() {
                    break;
                }
                if self.pending[0] != b'\x1b' {
                    output.push(self.pending.remove(0));
                    continue;
                }
                let Some(sequence_len) = complete_escape_len(&self.pending) else {
                    if self.pending.len() > MAX_PROBE_REPLY_BYTES {
                        self.discarding_oversized = OversizedSequence::from_prefix(&self.pending);
                        self.pending.clear();
                        self.malformed_replies = self.malformed_replies.saturating_add(1);
                    }
                    break;
                };
                let sequence = self.pending[..sequence_len].to_vec();
                self.pending.drain(..sequence_len);
                if sequence.len() > MAX_PROBE_REPLY_BYTES {
                    self.malformed_replies = self.malformed_replies.saturating_add(1);
                    continue;
                }
                if !self.consume_reply(&sequence) {
                    output.extend_from_slice(&sequence);
                }
            }
        }
        output
    }

    pub fn finish_if_timed_out(&mut self, now_ms: u128) -> Vec<u8> {
        if self.finished || now_ms.saturating_sub(self.started_at_ms) <= PROBE_TIMEOUT_MS {
            return Vec::new();
        }
        self.finished = true;
        std::mem::take(&mut self.pending)
    }

    pub const fn is_finished(&self) -> bool {
        self.finished
    }

    pub const fn malformed_replies(&self) -> usize {
        self.malformed_replies
    }

    pub fn buffered_reply_bytes(&self) -> usize {
        self.pending.len()
    }

    pub fn profile(&self) -> &PhysicalTerminalProfile {
        &self.profile
    }

    fn consume_reply(&mut self, sequence: &[u8]) -> bool {
        if let Some((mode, state)) = parse_mode_report(sequence) {
            match mode {
                1004 => self.report.focus_reporting = Some(matches!(state, 1 | 3)),
                2026 => self.report.synchronized_output = Some(matches!(state, 1..=4)),
                _ => return false,
            }
            self.profile.apply_probe(&self.report);
            return true;
        }
        if let Some(flags) = parse_kitty_keyboard_report(sequence) {
            let _ = flags;
            self.report.kitty_keyboard = Some(true);
            self.profile.apply_probe(&self.report);
            return true;
        }
        if let Some((kind, first, second)) = parse_size_report(sequence) {
            match kind {
                4 => {
                    self.pixel_height = Some(first);
                    self.pixel_width = Some(second);
                }
                6 => {
                    self.profile.geometry.cell_height_px = first;
                    self.profile.geometry.cell_width_px = second;
                }
                8 => {
                    self.profile.geometry.rows = u16::try_from(first).unwrap_or(u16::MAX);
                    self.profile.geometry.cols = u16::try_from(second).unwrap_or(u16::MAX);
                }
                _ => return false,
            }
            if self.profile.geometry.cell_width_px == 0
                && let (Some(width), cols) = (self.pixel_width, self.profile.geometry.cols)
                && cols > 0
            {
                self.profile.geometry.cell_width_px = width / u32::from(cols);
            }
            if self.profile.geometry.cell_height_px == 0
                && let (Some(height), rows) = (self.pixel_height, self.profile.geometry.rows)
                && rows > 0
            {
                self.profile.geometry.cell_height_px = height / u32::from(rows);
            }
            self.report.geometry = Some(self.profile.geometry);
            return true;
        }
        if let Some((slot, red, green, blue)) = parse_color_report(sequence) {
            let luminance = u32::from(red) * 299 + u32::from(green) * 587 + u32::from(blue) * 114;
            match slot {
                10 => self.report_foreground(luminance),
                11 => self.report_background(luminance),
                _ => return false,
            }
            return true;
        }
        if is_device_attributes_reply(sequence) {
            return true;
        }
        if self.policy.clipboard_read && is_clipboard_reply(sequence) {
            self.report.clipboard_read = Some(true);
            self.profile.apply_probe(&self.report);
            return true;
        }
        false
    }

    fn report_foreground(&mut self, luminance: u32) {
        self.foreground_luminance = Some(luminance);
        self.resolve_color_scheme();
    }

    fn report_background(&mut self, luminance: u32) {
        self.background_luminance = Some(luminance);
        self.resolve_color_scheme();
    }

    fn resolve_color_scheme(&mut self) {
        let (Some(foreground), Some(background)) =
            (self.foreground_luminance, self.background_luminance)
        else {
            return;
        };
        self.report.color_scheme = Some(if background < foreground {
            ColorScheme::Dark
        } else {
            ColorScheme::Light
        });
        self.profile.apply_probe(&self.report);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OversizedSequence {
    Csi,
    String { previous_was_escape: bool },
}

impl OversizedSequence {
    fn from_prefix(prefix: &[u8]) -> Option<Self> {
        match prefix.get(1) {
            Some(b'[') => Some(Self::Csi),
            Some(b']' | b'P' | b'_') => Some(Self::String {
                previous_was_escape: prefix.last() == Some(&b'\x1b'),
            }),
            _ => None,
        }
    }

    fn consume(&mut self, byte: u8) -> bool {
        match self {
            Self::Csi => (0x40..=0x7e).contains(&byte),
            Self::String {
                previous_was_escape,
            } => {
                let complete = byte == 0x07 || (*previous_was_escape && byte == b'\\');
                *previous_was_escape = byte == b'\x1b';
                complete
            }
        }
    }
}

fn complete_escape_len(bytes: &[u8]) -> Option<usize> {
    let second = *bytes.get(1)?;
    match second {
        b'[' => bytes
            .iter()
            .enumerate()
            .skip(2)
            .find_map(|(index, byte)| (0x40..=0x7e).contains(byte).then_some(index + 1)),
        b']' | b'P' | b'_' => {
            let mut index = 2;
            while index < bytes.len() {
                if bytes[index] == 0x07 {
                    return Some(index + 1);
                }
                if bytes[index] == b'\x1b' && bytes.get(index + 1) == Some(&b'\\') {
                    return Some(index + 2);
                }
                index += 1;
            }
            None
        }
        _ => Some(2),
    }
}

fn parse_mode_report(sequence: &[u8]) -> Option<(u16, u8)> {
    let text = std::str::from_utf8(sequence).ok()?;
    let body = text.strip_prefix("\x1b[?")?.strip_suffix("$y")?;
    let (mode, state) = body.split_once(';')?;
    Some((mode.parse().ok()?, state.parse().ok()?))
}

fn parse_kitty_keyboard_report(sequence: &[u8]) -> Option<u8> {
    std::str::from_utf8(sequence)
        .ok()?
        .strip_prefix("\x1b[?")?
        .strip_suffix('u')?
        .parse()
        .ok()
}

fn parse_size_report(sequence: &[u8]) -> Option<(u8, u32, u32)> {
    let text = std::str::from_utf8(sequence).ok()?;
    let body = text.strip_prefix("\x1b[")?.strip_suffix('t')?;
    let mut parts = body.split(';');
    let kind = parts.next()?.parse().ok()?;
    let first = parts.next()?.parse().ok()?;
    let second = parts.next()?.parse().ok()?;
    (parts.next().is_none()).then_some((kind, first, second))
}

fn parse_color_report(sequence: &[u8]) -> Option<(u8, u16, u16, u16)> {
    let body = sequence.strip_prefix(b"\x1b]")?;
    let body = body
        .strip_suffix(b"\x1b\\")
        .or_else(|| body.strip_suffix(b"\x07"))?;
    let text = std::str::from_utf8(body).ok()?;
    let (slot, rgb) = text.split_once(";rgb:")?;
    let mut channels = rgb.split('/');
    let red = u16::from_str_radix(channels.next()?, 16).ok()?;
    let green = u16::from_str_radix(channels.next()?, 16).ok()?;
    let blue = u16::from_str_radix(channels.next()?, 16).ok()?;
    if channels.next().is_some() {
        return None;
    }
    Some((slot.parse().ok()?, red, green, blue))
}

fn is_device_attributes_reply(sequence: &[u8]) -> bool {
    (sequence.starts_with(b"\x1b[?") || sequence.starts_with(b"\x1b[>")) && sequence.ends_with(b"c")
        || sequence.starts_with(b"\x1bP!|") && sequence.ends_with(b"\x1b\\")
}

fn is_clipboard_reply(sequence: &[u8]) -> bool {
    sequence.starts_with(b"\x1b]52;")
        && (sequence.ends_with(b"\x1b\\") || sequence.ends_with(b"\x07"))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VirtualTerminalProfile {
    pub geometry: TerminalGeometry,
    pub color_scheme: ColorScheme,
    pub enquiry: Vec<u8>,
    pub version: String,
    pub da_conformance: u16,
    pub da_features: Vec<u16>,
    pub da_device_type: u16,
    pub da_firmware_version: u16,
    pub da_unit_id: u32,
    pub clipboard_read: bool,
}

impl VirtualTerminalProfile {
    pub fn lector(geometry: TerminalGeometry, color_scheme: ColorScheme) -> Self {
        Self {
            geometry,
            color_scheme,
            enquiry: b"lector".to_vec(),
            version: format!("Lector {}", env!("CARGO_PKG_VERSION")),
            da_conformance: 64,
            // ANSI color and rectangular editing are implemented by the
            // Ghostty engine plus Lector's renderer. Clipboard reads are not
            // advertised by the secure default policy.
            da_features: vec![22, 28],
            da_device_type: 41,
            da_firmware_version: 301,
            da_unit_id: 0,
            clipboard_read: false,
        }
    }
}

#[derive(Debug)]
pub struct ApplicationReplyBroker<Owner: Ord> {
    pending: BTreeMap<Owner, Vec<u8>>,
}

impl<Owner: Ord> Default for ApplicationReplyBroker<Owner> {
    fn default() -> Self {
        Self {
            pending: BTreeMap::new(),
        }
    }
}

impl<Owner: Ord> ApplicationReplyBroker<Owner> {
    pub fn queue(&mut self, owner: Owner, reply: &[u8]) {
        self.pending
            .entry(owner)
            .or_default()
            .extend_from_slice(reply);
    }

    pub fn take(&mut self, owner: Owner) -> Vec<u8> {
        self.pending.remove(&owner).unwrap_or_default()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EffectDisposition {
    Model,
    LocalClipboard,
    Internal,
    Drop,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TerminalEffectPolicy;

impl TerminalEffectPolicy {
    pub const fn secure_default() -> Self {
        Self
    }

    pub const fn disposition(&self, event: &TerminalEvent) -> EffectDisposition {
        match event {
            TerminalEvent::Bell
            | TerminalEvent::TitleChanged(_)
            | TerminalEvent::WorkingDirectoryChanged(_)
            | TerminalEvent::ProgressReport { .. } => EffectDisposition::Model,
            TerminalEvent::ClipboardWrite { .. } => EffectDisposition::LocalClipboard,
            TerminalEvent::Query(_) | TerminalEvent::PtyReply(_) => EffectDisposition::Internal,
            TerminalEvent::DesktopNotification { .. } | TerminalEvent::UnknownSequence { .. } => {
                EffectDisposition::Drop
            }
        }
    }
}
