//! Physical-terminal probing, application reply routing, and protocol policy.
//!
//! The physical terminal and the terminal Lector exposes to applications are
//! deliberately different profiles. Outer replies are consumed here; source
//! application queries are answered from the virtual profile attached to that
//! source's Ghostty engine.

use crate::{
    host_command::run_bounded_output,
    terminal::{TerminalEvent, TerminalGeometry},
};
use std::collections::BTreeMap;

const PROBE_INACTIVITY_TIMEOUT_MS: u128 = 50;
const MAX_PROBE_REPLY_BYTES: usize = 4_096;

/// A terminal-processing fence used after Lector's final physical output.
/// The matching primary device-attributes response is consumed locally and
/// never becomes input for the shell that regains the terminal after Lector.
pub const SHUTDOWN_FENCE_QUERY: &[u8] = b"\x1b[c";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ColorScheme {
    Light,
    Dark,
}

/// One exact terminal default colour, normalized to OSC's 16-bit RGB space.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DefaultColor {
    pub red: u16,
    pub green: u16,
    pub blue: u16,
}

impl DefaultColor {
    pub const BLACK: Self = Self::new(0, 0, 0);
    pub const WHITE: Self = Self::new(u16::MAX, u16::MAX, u16::MAX);

    pub const fn new(red: u16, green: u16, blue: u16) -> Self {
        Self { red, green, blue }
    }

    const fn luminance(self) -> u32 {
        self.red as u32 * 299 + self.green as u32 * 587 + self.blue as u32 * 114
    }
}

/// The colour contract exposed by every Lector-owned virtual terminal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VirtualTerminalColors {
    pub color_scheme: ColorScheme,
    pub default_foreground: DefaultColor,
    pub default_background: DefaultColor,
}

impl VirtualTerminalColors {
    pub const fn for_scheme(color_scheme: ColorScheme) -> Self {
        match color_scheme {
            ColorScheme::Light => Self {
                color_scheme,
                default_foreground: DefaultColor::BLACK,
                default_background: DefaultColor::WHITE,
            },
            ColorScheme::Dark => Self {
                color_scheme,
                default_foreground: DefaultColor::WHITE,
                default_background: DefaultColor::BLACK,
            },
        }
    }

    pub const fn new(
        color_scheme: ColorScheme,
        default_foreground: DefaultColor,
        default_background: DefaultColor,
    ) -> Self {
        Self {
            color_scheme,
            default_foreground,
            default_background,
        }
    }
}

/// Reconcile already-generated child replies with a colour profile learned
/// after the child issued its query. This closes the bounded startup race in
/// which the child exists before Lector receives the outer terminal's probe
/// replies. Input is exclusively trusted terminal-engine output, not arbitrary
/// child data.
pub(crate) fn rewrite_virtual_color_replies(
    replies: &[u8],
    colors: VirtualTerminalColors,
) -> Vec<u8> {
    let mut rewritten = Vec::with_capacity(replies.len());
    let mut index = 0;
    while index < replies.len() {
        let remaining = &replies[index..];
        let color_slot = if remaining.starts_with(b"\x1b]10;") {
            Some((10_u8, colors.default_foreground))
        } else if remaining.starts_with(b"\x1b]11;") {
            Some((11_u8, colors.default_background))
        } else {
            None
        };
        if let Some((slot, color)) = color_slot
            && let Some(length) = complete_escape_len(remaining)
        {
            rewritten.extend_from_slice(
                format!(
                    "\x1b]{slot};rgb:{:04x}/{:04x}/{:04x}\x1b\\",
                    color.red, color.green, color.blue
                )
                .as_bytes(),
            );
            index += length;
            continue;
        }
        let scheme_report_len = [b"\x1b[?997;1n".as_slice(), b"\x1b[?997;2n".as_slice()]
            .into_iter()
            .find_map(|report| remaining.starts_with(report).then_some(report.len()));
        if let Some(report_len) = scheme_report_len {
            let state = match colors.color_scheme {
                ColorScheme::Dark => 1,
                ColorScheme::Light => 2,
            };
            rewritten.extend_from_slice(format!("\x1b[?997;{state}n").as_bytes());
            index += report_len;
            continue;
        }
        rewritten.push(replies[index]);
        index += 1;
    }
    rewritten
}

pub(crate) fn first_virtual_color_reply_offset(replies: &[u8]) -> Option<usize> {
    [
        b"\x1b]10;".as_slice(),
        b"\x1b]11;".as_slice(),
        b"\x1b[?997;1n".as_slice(),
        b"\x1b[?997;2n".as_slice(),
    ]
    .into_iter()
    .filter_map(|prefix| {
        replies
            .windows(prefix.len())
            .position(|window| window == prefix)
    })
    .min()
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
            let capability = line.trim_end_matches(',');
            let name = capability
                .split_once(['=', '#'])
                .map_or(capability, |(name, _)| name);
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
        let mut command = std::process::Command::new("infocmp");
        command.args([std::ffi::OsStr::new("-1"), std::ffi::OsStr::new("-x"), term]);
        let output =
            run_bounded_output(&mut command, &std::env::temp_dir(), "outer-infocmp").ok()?;
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
    pub default_foreground: Option<DefaultColor>,
    pub default_background: Option<DefaultColor>,
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
    pub default_foreground: Option<DefaultColor>,
    pub default_background: Option<DefaultColor>,
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
            default_foreground: None,
            default_background: None,
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
        if let Some(color) = probe.default_foreground {
            self.default_foreground = Some(color);
        }
        if let Some(color) = probe.default_background {
            self.default_background = Some(color);
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

    /// Resolve the child-facing colours only after the outer terminal has
    /// supplied enough information to classify its default palette. Exact
    /// defaults win when available; the light/dark pair is a deterministic
    /// fallback for profiles supplied by tests or future non-OSC probes.
    pub fn virtual_terminal_colors(&self) -> Option<VirtualTerminalColors> {
        let color_scheme = self.color_scheme?;
        let fallback = VirtualTerminalColors::for_scheme(color_scheme);
        Some(VirtualTerminalColors::new(
            color_scheme,
            self.default_foreground
                .unwrap_or(fallback.default_foreground),
            self.default_background
                .unwrap_or(fallback.default_background),
        ))
    }

    pub const fn has_exact_default_colors(&self) -> bool {
        self.default_foreground.is_some() && self.default_background.is_some()
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

/// Consumes only bounded replies to Lector-owned physical-terminal probes. All
/// bytes which are not recognized as such a reply are returned as ordinary
/// input.
///
/// Reaching the DA1 fence or the startup timeout makes ordinary input ready;
/// it does not transfer ownership of delayed probe replies to the application.
/// Lector never forwards application queries to the physical terminal, so a
/// later reply in this probe vocabulary still belongs to this broker.
pub struct StartupProbeBroker {
    profile: PhysicalTerminalProfile,
    report: ProbeReport,
    policy: ProbePolicy,
    started_at_ms: u128,
    last_activity_at_ms: u128,
    started: bool,
    finished: bool,
    pending: Vec<u8>,
    discarding_oversized: Option<OversizedSequence>,
    malformed_replies: usize,
    pixel_width: Option<u32>,
    pixel_height: Option<u32>,
    foreground: Option<DefaultColor>,
    background: Option<DefaultColor>,
    semantic_color_scheme: Option<ColorScheme>,
    semantic_color_scheme_report_received: bool,
    primary_device_attributes_received: bool,
}

impl StartupProbeBroker {
    pub fn new(profile: PhysicalTerminalProfile, policy: ProbePolicy, started_at_ms: u128) -> Self {
        Self {
            profile,
            report: ProbeReport::default(),
            policy,
            started_at_ms,
            last_activity_at_ms: started_at_ms,
            started: false,
            finished: false,
            pending: Vec::new(),
            discarding_oversized: None,
            malformed_replies: 0,
            pixel_width: None,
            pixel_height: None,
            foreground: None,
            background: None,
            semantic_color_scheme: None,
            semantic_color_scheme_report_received: false,
            primary_device_attributes_received: false,
        }
    }

    pub fn startup_queries(&mut self) -> Vec<u8> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        let mut queries = b"\x1b[>c\x1b[=c\x1b[14t\x1b[16t\x1b[18t\x1b[?1004$p\x1b[?2026$p\x1b[?u\x1b[?996n\x1b]10;?\x1b\\\x1b]11;?\x1b\\".to_vec();
        if self.policy.clipboard_read {
            queries.extend_from_slice(b"\x1b]52;c;?\x1b\\");
        }
        // DA1 is a processing fence, not just another capability query. A
        // terminal's DA1 reply cannot precede replies to any of the probes
        // above, so consuming through it gives the application a clean input
        // boundary before its own terminal traffic begins.
        queries.extend_from_slice(b"\x1b[c");
        queries
    }

    pub fn ingest(&mut self, input: &[u8], now_ms: u128) -> Vec<u8> {
        // Readable input wins a race with the fallback deadline. A large
        // initial render can make an immediate terminal reply reach this
        // method after the inactivity deadline has elapsed even though it was
        // already queued. Parse the entire readable batch before considering
        // timeout; otherwise Lector-owned replies can leak into a shell or tmux -CC.
        if !input.is_empty() {
            self.last_activity_at_ms = now_ms;
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
        if now_ms.saturating_sub(self.last_activity_at_ms) <= PROBE_INACTIVITY_TIMEOUT_MS {
            return Vec::new();
        }
        if self.finished && self.pending.is_empty() && self.discarding_oversized.is_none() {
            return Vec::new();
        }
        self.finished = true;
        self.discarding_oversized = None;
        std::mem::take(&mut self.pending)
    }

    pub const fn next_deadline_ms(&self) -> Option<u128> {
        if self.finished && self.pending.is_empty() && self.discarding_oversized.is_none() {
            None
        } else {
            Some(
                self.last_activity_at_ms
                    .saturating_add(PROBE_INACTIVITY_TIMEOUT_MS + 1),
            )
        }
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

    /// Absolute deadline used only for child colour replies. Ordinary input
    /// may extend the broker's fragment-safety timeout, but can never hold a
    /// child theme negotiation indefinitely.
    pub const fn color_wait_deadline_ms(&self) -> Option<u128> {
        if self.started && !self.finished && !self.color_profile_complete() {
            Some(
                self.started_at_ms
                    .saturating_add(PROBE_INACTIVITY_TIMEOUT_MS + 1),
            )
        } else {
            None
        }
    }

    pub fn color_wait_pending(&self, now_ms: u128) -> bool {
        !self.finished
            && !self.color_profile_complete()
            && self
                .color_wait_deadline_ms()
                .is_some_and(|deadline| now_ms < deadline)
    }

    const fn color_profile_complete(&self) -> bool {
        self.profile.has_exact_default_colors() && self.semantic_color_scheme_report_received
    }

    /// The startup probe sends one DA1 request. If its response has not been
    /// observed, an orderly-shutdown fence must ignore one DA1 response before
    /// accepting the response to its fresh request. Terminal replies are
    /// ordered, but the startup response may still be queued when a child exits
    /// immediately.
    pub const fn outstanding_primary_device_attributes_replies(&self) -> usize {
        if self.started && !self.primary_device_attributes_received {
            1
        } else {
            0
        }
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
        if let Some(color_scheme) = parse_color_scheme_report(sequence) {
            self.semantic_color_scheme_report_received = true;
            if let Some(color_scheme) = color_scheme {
                self.semantic_color_scheme = Some(color_scheme);
                self.report.color_scheme = Some(color_scheme);
                self.profile.apply_probe(&self.report);
            }
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
        if let Some((slot, color)) = parse_color_report(sequence) {
            match slot {
                10 => self.report_foreground(color),
                11 => self.report_background(color),
                _ => return false,
            }
            return true;
        }
        if is_primary_device_attributes_reply(sequence) {
            self.primary_device_attributes_received = true;
            self.finished = true;
            return true;
        }
        if is_other_device_attributes_reply(sequence) {
            return true;
        }
        if self.policy.clipboard_read && is_clipboard_reply(sequence) {
            self.report.clipboard_read = Some(true);
            self.profile.apply_probe(&self.report);
            return true;
        }
        false
    }

    fn report_foreground(&mut self, color: DefaultColor) {
        self.foreground = Some(color);
        self.report.default_foreground = Some(color);
        self.resolve_color_scheme();
    }

    fn report_background(&mut self, color: DefaultColor) {
        self.background = Some(color);
        self.report.default_background = Some(color);
        self.resolve_color_scheme();
    }

    fn resolve_color_scheme(&mut self) {
        let inferred = match (self.foreground, self.background) {
            (Some(foreground), Some(background)) => {
                if background.luminance() < foreground.luminance() {
                    ColorScheme::Dark
                } else {
                    ColorScheme::Light
                }
            }
            (None, Some(background)) => {
                if background.luminance() < u32::from(u16::MAX) * 500 {
                    ColorScheme::Dark
                } else {
                    ColorScheme::Light
                }
            }
            (Some(foreground), None) => {
                if foreground.luminance() >= u32::from(u16::MAX) * 500 {
                    ColorScheme::Dark
                } else {
                    ColorScheme::Light
                }
            }
            (None, None) => return,
        };
        self.report.color_scheme = Some(self.semantic_color_scheme.unwrap_or(inferred));
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

fn parse_color_scheme_report(sequence: &[u8]) -> Option<Option<ColorScheme>> {
    let body = sequence
        .strip_prefix(b"\x1b[?997;")
        .or_else(|| sequence.strip_prefix(b"\x9b?997;"))?
        .strip_suffix(b"n")?;
    let state = std::str::from_utf8(body).ok()?.parse::<u16>().ok()?;
    Some(match state {
        1 => Some(ColorScheme::Dark),
        2 => Some(ColorScheme::Light),
        _ => None,
    })
}

fn parse_color_report(sequence: &[u8]) -> Option<(u8, DefaultColor)> {
    let body = sequence.strip_prefix(b"\x1b]")?;
    let body = body
        .strip_suffix(b"\x1b\\")
        .or_else(|| body.strip_suffix(b"\x07"))?;
    let text = std::str::from_utf8(body).ok()?;
    let (slot, rgb) = text.split_once(";rgb:")?;
    let mut channels = rgb.split('/');
    let red = parse_osc_color_channel(channels.next()?)?;
    let green = parse_osc_color_channel(channels.next()?)?;
    let blue = parse_osc_color_channel(channels.next()?)?;
    if channels.next().is_some() {
        return None;
    }
    Some((slot.parse().ok()?, DefaultColor::new(red, green, blue)))
}

fn parse_osc_color_channel(channel: &str) -> Option<u16> {
    let digits = channel.len();
    if !(1..=4).contains(&digits) || !channel.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let value = u32::from_str_radix(channel, 16).ok()?;
    let maximum = (1_u32 << (digits * 4)) - 1;
    value
        .saturating_mul(u32::from(u16::MAX))
        .saturating_add(maximum / 2)
        .checked_div(maximum)?
        .try_into()
        .ok()
}

fn is_primary_device_attributes_reply(sequence: &[u8]) -> bool {
    let body = sequence
        .strip_prefix(b"\x1b[?")
        .or_else(|| sequence.strip_prefix(b"\x9b?"));
    let Some(body) = body.and_then(|body| body.strip_suffix(b"c")) else {
        return false;
    };
    !body.is_empty()
        && body
            .split(|byte| *byte == b';')
            .all(|parameter| !parameter.is_empty() && parameter.iter().all(u8::is_ascii_digit))
}

fn is_other_device_attributes_reply(sequence: &[u8]) -> bool {
    sequence.starts_with(b"\x1b[>") && sequence.ends_with(b"c")
        || sequence.starts_with(b"\x1bP!|") && sequence.ends_with(b"\x1b\\")
}

/// Fragment-safe recognizer for the response to the final DA1 fence.
///
/// Shutdown reads one byte at a time so it cannot consume input following the
/// matching response. Everything before that response, including focus events
/// generated by the final render, remains owned and consumed by Lector.
pub struct ShutdownFenceBroker {
    pending: Vec<u8>,
    replies_to_ignore: usize,
    observed_replies: usize,
    matched: bool,
}

impl ShutdownFenceBroker {
    pub fn new(replies_to_ignore: usize) -> Self {
        Self {
            pending: Vec::new(),
            replies_to_ignore,
            observed_replies: 0,
            matched: false,
        }
    }

    pub fn ingest_byte(&mut self, byte: u8) -> bool {
        if self.matched {
            return true;
        }

        if self.pending.is_empty() {
            if matches!(byte, b'\x1b' | b'\x9b') {
                self.pending.push(byte);
            }
            return false;
        }

        self.pending.push(byte);
        if self.pending == b"\x1b" {
            return false;
        }
        if self.pending.starts_with(b"\x1b") && self.pending.get(1) != Some(&b'[') {
            self.restart_after_invalid_sequence(byte);
            return false;
        }

        let parameter_start = if self.pending.starts_with(b"\x1b[") {
            2
        } else {
            1
        };
        if self.pending.len() <= parameter_start {
            return false;
        }

        if self.pending.len() > MAX_PROBE_REPLY_BYTES {
            self.restart_after_invalid_sequence(byte);
            return false;
        }

        let final_byte = *self.pending.last().expect("pending fence sequence");
        if !(0x40..=0x7e).contains(&final_byte) {
            return false;
        }

        if is_primary_device_attributes_reply(&self.pending) {
            self.observed_replies = self.observed_replies.saturating_add(1);
            if self.observed_replies > self.replies_to_ignore {
                self.matched = true;
            }
        }
        self.pending.clear();
        self.matched
    }

    pub const fn is_matched(&self) -> bool {
        self.matched
    }

    pub const fn observed_replies(&self) -> usize {
        self.observed_replies
    }

    fn restart_after_invalid_sequence(&mut self, byte: u8) {
        self.pending.clear();
        if matches!(byte, b'\x1b' | b'\x9b') {
            self.pending.push(byte);
        }
    }
}

fn is_clipboard_reply(sequence: &[u8]) -> bool {
    sequence.starts_with(b"\x1b]52;")
        && (sequence.ends_with(b"\x1b\\") || sequence.ends_with(b"\x07"))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VirtualTerminalProfile {
    pub geometry: TerminalGeometry,
    pub colors: VirtualTerminalColors,
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
            colors: VirtualTerminalColors::for_scheme(color_scheme),
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

    pub fn with_colors(mut self, colors: VirtualTerminalColors) -> Self {
        self.colors = colors;
        self
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
