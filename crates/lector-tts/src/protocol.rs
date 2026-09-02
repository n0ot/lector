//! Wire-level types for the Lector speech host protocol.
//!
//! These types intentionally describe semantic guarantees rather than native
//! implementation details. Unknown capability values and event kinds degrade
//! to unsupported behavior so an older peer can safely talk to a newer one.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const PROTOCOL_MAJOR: u16 = 2;
pub const PROTOCOL_MIN_MINOR: u16 = 0;
pub const PROTOCOL_MAX_MINOR: u16 = 2;
/// First protocol minor whose rate domain is independent of the native backend.
pub const NORMALIZED_RATE_PROTOCOL_MINOR: u16 = 2;
pub const MIN_RATE: f32 = 0.0;
pub const NORMAL_RATE: f32 = 50.0;
pub const MAX_RATE: f32 = 100.0;
/// Largest integer represented exactly by every common JSON implementation.
pub const MAX_JSON_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
pub const MAX_UTTERANCE_TEXT_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProtocolRange {
    pub major: u16,
    pub minimum_minor: u16,
    pub maximum_minor: u16,
}

impl ProtocolRange {
    #[must_use]
    pub const fn current() -> Self {
        Self {
            major: PROTOCOL_MAJOR,
            minimum_minor: PROTOCOL_MIN_MINOR,
            maximum_minor: PROTOCOL_MAX_MINOR,
        }
    }

    /// Protocol range offered by Lector now that its public rate setting uses
    /// the normalized 0..100 domain. Older 2.x hosts remain incompatible with
    /// that setting because their rate values were backend-specific.
    #[must_use]
    pub const fn normalized_rate() -> Self {
        Self {
            major: PROTOCOL_MAJOR,
            minimum_minor: NORMALIZED_RATE_PROTOCOL_MINOR,
            maximum_minor: PROTOCOL_MAX_MINOR,
        }
    }

    #[must_use]
    pub const fn supports(self, version: ProtocolVersion) -> bool {
        self.major == version.major
            && version.minor >= self.minimum_minor
            && version.minor <= self.maximum_minor
    }

    #[must_use]
    pub const fn highest_mutual(self, other: Self) -> Option<ProtocolVersion> {
        if self.major != other.major
            || self.maximum_minor < other.minimum_minor
            || other.maximum_minor < self.minimum_minor
        {
            return None;
        }
        let minor = if self.maximum_minor < other.maximum_minor {
            self.maximum_minor
        } else {
            other.maximum_minor
        };
        Some(ProtocolVersion {
            major: self.major,
            minor,
        })
    }
}

#[must_use]
pub fn rate_is_normalized(rate: f32) -> bool {
    rate.is_finite() && (MIN_RATE..=MAX_RATE).contains(&rate)
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
}

impl ProtocolVersion {
    #[must_use]
    pub const fn current() -> Self {
        Self {
            major: PROTOCOL_MAJOR,
            minor: PROTOCOL_MAX_MINOR,
        }
    }
}

/// A Lector-assigned identifier, encoded as a string so implementations in
/// languages whose JSON numbers are IEEE-754 doubles never lose precision.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct UtteranceId(String);

impl UtteranceId {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn chunk(&self, index: usize) -> Self {
        Self(format!("{}:{index}", self.0))
    }

    #[must_use]
    pub fn is_valid(&self) -> bool {
        !self.0.is_empty() && self.0.len() <= 128
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {
    #[serde(default)]
    pub speech_events: bool,
    #[serde(default)]
    pub progress_modes: Vec<String>,
    #[serde(default, flatten)]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

impl Default for ClientCapabilities {
    fn default() -> Self {
        Self {
            speech_events: true,
            progress_modes: vec!["marker".to_owned(), "utf8ByteOffset".to_owned()],
            extensions: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DeliveryGuarantee {
    Reliable,
    BestEffort,
    #[default]
    Unsupported,
    #[serde(other)]
    Unknown,
}

impl DeliveryGuarantee {
    #[must_use]
    pub const fn is_reliable(self) -> bool {
        matches!(self, Self::Reliable)
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum StopSupport {
    Confirmed,
    BestEffort,
    #[default]
    Unsupported,
    #[serde(other)]
    Unknown,
}

impl StopSupport {
    #[must_use]
    pub const fn is_supported(self) -> bool {
        matches!(self, Self::Confirmed | Self::BestEffort)
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PauseResumeSupport {
    /// Pausing captures the current word and resuming restarts that word.
    RestartFromWord,
    #[default]
    Unsupported,
    #[serde(other)]
    Unknown,
}

impl PauseResumeSupport {
    #[must_use]
    pub const fn restarts_from_word(self) -> bool {
        matches!(self, Self::RestartFromWord)
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SettingSupport {
    ReadWrite,
    WriteOnly,
    #[default]
    Unsupported,
    #[serde(other)]
    Unknown,
}

impl SettingSupport {
    #[must_use]
    pub const fn can_write(self) -> bool {
        matches!(self, Self::ReadWrite | Self::WriteOnly)
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct EventCapability {
    #[serde(default)]
    pub delivery: DeliveryGuarantee,
    #[serde(default, flatten)]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct TerminalCapability {
    #[serde(default)]
    pub delivery: DeliveryGuarantee,
    #[serde(default)]
    pub distinguishes: Vec<String>,
    #[serde(default, flatten)]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct LifecycleCapabilities {
    #[serde(default)]
    pub started: EventCapability,
    #[serde(default)]
    pub terminal: TerminalCapability,
    #[serde(default, flatten)]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProgressMode {
    pub kind: String,
    #[serde(default)]
    pub granularity: Vec<String>,
    #[serde(default, flatten)]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProgressCapabilities {
    #[serde(default)]
    pub modes: Vec<ProgressMode>,
    #[serde(default, flatten)]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

impl ProgressCapabilities {
    #[must_use]
    pub fn supports_utf8_word_offsets(&self) -> bool {
        self.modes.iter().any(|mode| {
            mode.kind == "utf8ByteOffset" && mode.granularity.iter().any(|value| value == "word")
        })
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlCapabilities {
    #[serde(default)]
    pub stop: StopSupport,
    #[serde(default)]
    pub pause_resume: PauseResumeSupport,
    #[serde(default, flatten)]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SettingCapabilities {
    #[serde(default)]
    pub rate: SettingSupport,
    #[serde(default)]
    pub pitch: SettingSupport,
    #[serde(default)]
    pub volume: SettingSupport,
    #[serde(default, flatten)]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

/// Independently usable voice operations. A backend such as NVDA may expose
/// none of them because its voice is managed by an external application.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct VoiceCapabilities {
    #[serde(default)]
    pub list: bool,
    #[serde(default)]
    pub current: bool,
    #[serde(default)]
    pub select: bool,
    #[serde(default, flatten)]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

/// Host guarantees negotiated for one process generation. Missing or unknown
/// members are unsupported. New optional members can therefore be added
/// without changing the protocol major version.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SpeechCapabilities {
    #[serde(default)]
    pub lifecycle: LifecycleCapabilities,
    #[serde(default)]
    pub progress: ProgressCapabilities,
    #[serde(default)]
    pub controls: ControlCapabilities,
    #[serde(default)]
    pub settings: SettingCapabilities,
    #[serde(default)]
    pub voices: VoiceCapabilities,
    #[serde(default, flatten)]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

/// The engine selected inside a multi-backend speech host. The server field
/// identifies the host implementation; this identifies what produces audio.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BackendInfo {
    pub id: String,
    pub name: String,
    #[serde(default, flatten)]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct VoiceInfo {
    pub id: String,
    pub name: String,
    pub language: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gender: Option<String>,
    #[serde(default, flatten)]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct VoiceListResult {
    pub voices: Vec<VoiceInfo>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CurrentVoiceResult {
    #[serde(default)]
    pub voice: Option<VoiceInfo>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetVoiceParams {
    pub voice_id: String,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct PitchResult {
    pub pitch: f32,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct RateResult {
    pub rate: f32,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct VolumeResult {
    pub volume: f32,
}

impl SpeechCapabilities {
    /// Restrict additive capabilities to the selected protocol minor. In 2.0,
    /// rate's historical `readWrite` value authorized only the setter and its
    /// effective result; 2.1 is the first version with numeric getters.
    #[must_use]
    pub fn for_protocol_version(mut self, version: ProtocolVersion) -> Self {
        if version.major != PROTOCOL_MAJOR {
            return Self::default();
        }
        if version.minor < 1 {
            if self.settings.rate == SettingSupport::ReadWrite {
                self.settings.rate = SettingSupport::WriteOnly;
            }
            self.settings.pitch = SettingSupport::Unsupported;
            self.settings.volume = SettingSupport::Unsupported;
        }
        self
    }

    #[must_use]
    pub fn supports_resumable_pause(&self) -> bool {
        self.controls.pause_resume.restarts_from_word()
            && self.progress.supports_utf8_word_offsets()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeakParams {
    pub utterance_id: UtteranceId,
    pub text: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UtteranceParams {
    pub utterance_id: UtteranceId,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AcceptedResult {
    pub accepted: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum TextPosition {
    Marker {
        id: String,
    },
    Utf8ByteOffset {
        offset: usize,
    },
    /// A future position encoding. Older clients must ignore events carrying
    /// one rather than treating an additive protocol extension as a broken
    /// transport.
    #[serde(other)]
    Unknown,
}

impl TextPosition {
    #[must_use]
    pub fn valid_for(&self, text: &str) -> bool {
        match self {
            Self::Marker { id } => !id.is_empty(),
            Self::Utf8ByteOffset { offset } => {
                *offset <= text.len() && text.is_char_boundary(*offset)
            }
            Self::Unknown => false,
        }
    }

    #[must_use]
    pub const fn utf8_offset(&self) -> Option<usize> {
        match self {
            Self::Utf8ByteOffset { offset } => Some(*offset),
            Self::Marker { .. } | Self::Unknown => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PauseResult {
    pub paused: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<TextPosition>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeechEventNotification {
    pub utterance_id: UtteranceId,
    pub sequence: u64,
    pub event: SpeechEventPayload,
}

/// An extensible event payload. Code must inspect `kind` and ignore unknown
/// values; known kinds validate their required fields before changing state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SpeechEventPayload {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<TextPosition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, flatten)]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KnownEventKind {
    Started,
    Progress,
    Paused,
    Resumed,
    Ended,
}

impl SpeechEventPayload {
    #[must_use]
    pub fn known_kind(&self) -> Option<KnownEventKind> {
        match self.kind.as_str() {
            "started" => Some(KnownEventKind::Started),
            "progress" if self.position.is_some() => Some(KnownEventKind::Progress),
            "paused" if self.position.is_some() => Some(KnownEventKind::Paused),
            "resumed" => Some(KnownEventKind::Resumed),
            "ended"
                if self
                    .reason
                    .as_ref()
                    .is_some_and(|reason| !reason.is_empty()) =>
            {
                Some(KnownEventKind::Ended)
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn version_ranges_choose_the_highest_mutual_minor() {
        let current = ProtocolRange::current();
        assert_eq!(
            current.highest_mutual(ProtocolRange {
                major: 2,
                minimum_minor: 0,
                maximum_minor: 0,
            }),
            Some(ProtocolVersion { major: 2, minor: 0 })
        );
        assert_eq!(
            current.highest_mutual(ProtocolRange {
                major: 2,
                minimum_minor: 0,
                maximum_minor: 9,
            }),
            Some(ProtocolVersion { major: 2, minor: 2 })
        );
        assert_eq!(
            current.highest_mutual(ProtocolRange {
                major: 3,
                minimum_minor: 0,
                maximum_minor: 1,
            }),
            None
        );
    }

    #[test]
    fn normalized_rate_range_requires_the_new_domain() {
        let range = ProtocolRange::normalized_rate();
        assert!(!range.supports(ProtocolVersion { major: 2, minor: 1 }));
        assert!(range.supports(ProtocolVersion { major: 2, minor: 2 }));
        assert!(rate_is_normalized(MIN_RATE));
        assert!(rate_is_normalized(NORMAL_RATE));
        assert!(rate_is_normalized(MAX_RATE));
        assert!(!rate_is_normalized(-f32::EPSILON));
        assert!(!rate_is_normalized(MAX_RATE + 0.01));
        assert!(!rate_is_normalized(f32::NAN));
    }

    #[test]
    fn missing_and_unknown_capabilities_degrade_to_unsupported() {
        let capabilities: SpeechCapabilities = serde_json::from_value(json!({
            "controls": {
                "stop": "futureStopMode",
                "futureControl": {"enabled": true}
            },
            "futureFamily": {"mode": "new"}
        }))
        .unwrap();

        assert_eq!(capabilities.controls.stop, StopSupport::Unknown);
        assert_eq!(capabilities.settings.pitch, SettingSupport::Unsupported);
        assert_eq!(capabilities.settings.volume, SettingSupport::Unsupported);
        assert_eq!(
            capabilities.controls.pause_resume,
            PauseResumeSupport::Unsupported
        );
        assert_eq!(capabilities.voices, VoiceCapabilities::default());
        assert!(!capabilities.supports_resumable_pause());
        assert!(capabilities.extensions.contains_key("futureFamily"));
        assert!(
            capabilities
                .controls
                .extensions
                .contains_key("futureControl")
        );

        let mut incomplete = capabilities;
        incomplete.controls.pause_resume = PauseResumeSupport::RestartFromWord;
        incomplete.progress.modes.push(ProgressMode {
            kind: "utf8ByteOffset".to_owned(),
            granularity: vec!["sentence".to_owned()],
            extensions: BTreeMap::new(),
        });
        assert!(!incomplete.supports_resumable_pause());
    }

    #[test]
    fn protocol_two_zero_exposes_only_its_setter_only_rate_contract() {
        let capabilities = SpeechCapabilities {
            settings: SettingCapabilities {
                rate: SettingSupport::ReadWrite,
                pitch: SettingSupport::ReadWrite,
                volume: SettingSupport::ReadWrite,
                ..SettingCapabilities::default()
            },
            ..SpeechCapabilities::default()
        }
        .for_protocol_version(ProtocolVersion { major: 2, minor: 0 });

        assert_eq!(capabilities.settings.rate, SettingSupport::WriteOnly);
        assert_eq!(capabilities.settings.pitch, SettingSupport::Unsupported);
        assert_eq!(capabilities.settings.volume, SettingSupport::Unsupported);
    }

    #[test]
    fn positions_are_utf8_byte_offsets_on_character_boundaries() {
        let text = "aé zebra";
        assert!(TextPosition::Utf8ByteOffset { offset: 3 }.valid_for(text));
        assert!(!TextPosition::Utf8ByteOffset { offset: 2 }.valid_for(text));
        assert!(!TextPosition::Utf8ByteOffset { offset: 99 }.valid_for(text));
    }

    #[test]
    fn unnamed_current_voice_is_explicit_null_not_a_fabricated_default() {
        let result = serde_json::to_value(CurrentVoiceResult { voice: None }).unwrap();
        assert_eq!(result, json!({"voice": null}));
    }

    #[test]
    fn unknown_events_are_ignorable_and_ended_remains_the_terminal_shape() {
        let unknown: SpeechEventPayload = serde_json::from_value(json!({
            "type": "futureEvent",
            "futureMember": 7
        }))
        .unwrap();
        assert_eq!(unknown.known_kind(), None);

        let ended: SpeechEventPayload = serde_json::from_value(json!({
            "type": "ended",
            "reason": "futureReason"
        }))
        .unwrap();
        assert_eq!(ended.known_kind(), Some(KnownEventKind::Ended));

        let future_position: TextPosition = serde_json::from_value(json!({
            "kind": "audioFrame",
            "frame": 42
        }))
        .unwrap();
        assert_eq!(future_position, TextPosition::Unknown);
        assert!(!future_position.valid_for("text"));
    }

    #[test]
    fn utterance_ids_are_strings_and_chunk_ids_are_stable() {
        let id = UtteranceId::new(u64::MAX.to_string());
        assert_eq!(
            serde_json::to_value(&id).unwrap(),
            json!(u64::MAX.to_string())
        );
        assert_eq!(id.chunk(2).as_str(), format!("{}:2", u64::MAX));
    }
}
