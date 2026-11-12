//! Lua option dispatch. Reader storage/defaults and invariant-enforcing
//! setters remain owned by ScreenReader/Speech; this module owns Lua names,
//! aliases, value conversion, and error attribution.
use super::{
    access::ReaderAccess,
    meta::{lua_utf8, speech_server_spec_from_lua, speech_server_spec_to_lua, voice_info_to_lua},
};
use crate::{
    screen_reader::ScreenReader,
    speech::{SetOptionOutcome, Speech, SpeechServerSpec, protocol::VoiceInfo, symbols},
};
use anyhow::{Context, anyhow};
use mlua::{Error, IntoLua, Lua, Result, Value};

pub(super) fn get(access: &ReaderAccess, name: &str) -> Result<OptionValue> {
    access.with(|sr| get_option(sr, name).map_err(Error::external))
}

pub(super) fn set(access: &ReaderAccess, name: &str, value: Value) -> Result<()> {
    // This is the only option with metamethod-bearing input. Parse it before
    // borrowing the reader, preserving the scoped callback boundary.
    if name == "speech.server" {
        let spec = speech_server_spec_from_lua(value).map_err(Error::external)?;
        return access.with_mut(|sr| {
            sr.set_startup_speech_server_spec(spec)
                .map_err(Error::external)
        });
    }
    access.with_mut(|sr| {
        set_option(sr, name, value).map_err(|error| Error::external(anyhow!(format!("{error:#}"))))
    })
}

// One compile-time list ties the simple Lua names/aliases to their existing
// reader accessors. Expands only two ordinary matches; no registry, allocation,
// field exposure, or generated defaults. Special speech/duration policy stays
// explicit below.
macro_rules! reader_options {
    (booleans { $($bool_name:pat => $bool_get:ident, $bool_set:ident;)* }
     strings { $($text_name:pat => $text_get:ident, $text_set:ident;)* }) => {
        fn get_reader_option(sr: &ScreenReader, name: &str) -> Option<OptionValue> {
            match name {
                $($bool_name => Some(OptionValue::Boolean(sr.$bool_get())),)*
                $($text_name => Some(OptionValue::Text(Some(sr.$text_get().to_string()))),)*
                _ => None,
            }
        }

        fn set_reader_option(sr: &mut ScreenReader, name: &str, value: Value) -> anyhow::Result<()> {
            match name {
                $($bool_name => {
                    let Value::Boolean(value) = value else { return Err(anyhow!("set option: {name}: value must be a boolean")); };
                    sr.$bool_set(value);
                    Ok(())
                },)*
                $($text_name => {
                    let Value::String(value) = value else { return Err(anyhow!("set option: {name}: value must be a string")); };
                    let value = value.to_str().map_err(|error| anyhow!(error.to_string()))?.parse()?;
                    sr.$text_set(value);
                    Ok(())
                },)*
                _ => Err(anyhow!("set option: {name}: unknown option")),
            }
        }
    };
}

reader_options! {
    booleans {
        "help_mode" => help_mode, set_help_mode;
        "auto_read" => auto_read_enabled, set_auto_read_enabled;
        "suppress_key_echo" => suppress_key_echo, set_suppress_key_echo;
        "report_indentation" => indentation_reporting_enabled, set_indentation_reporting_enabled;
        "review_follows_screen_cursor" | "rev_follows" => review_follows_screen_cursor, set_review_follows_screen_cursor;
        "highlight_tracking" => highlight_tracking_enabled, set_highlight_tracking_enabled;
        "stop_speech_on_focus_loss" => stop_speech_on_focus_loss, set_stop_speech_on_focus_loss;
    }
    strings {
        "tmux_bells" => tmux_bell_mode, set_tmux_bell_mode;
        "clipboard.default_register" => clipboard_default_register, set_clipboard_default_register;
        "clipboard.system_provider" => system_clipboard_provider, set_system_clipboard_provider;
    }
}

// Snapshot reader data before allocating Lua strings/tables: allocation can
// collect userdata and run Lua finalizers.
pub(super) enum OptionValue {
    Boolean(bool),
    Integer(u64),
    Number(Option<f32>),
    Text(Option<String>),
    Server(SpeechServerSpec),
    Voices(Option<Vec<VoiceInfo>>),
}

impl IntoLua for OptionValue {
    fn into_lua(self, lua: &Lua) -> Result<Value> {
        match self {
            Self::Boolean(value) => value.into_lua(lua),
            Self::Integer(value) => value.into_lua(lua),
            Self::Number(value) => value.into_lua(lua),
            Self::Text(value) => value.into_lua(lua),
            Self::Server(value) => speech_server_spec_to_lua(lua, &value),
            Self::Voices(None) => Ok(Value::Nil),
            Self::Voices(Some(voices)) => {
                let result = lua.create_table()?;
                for (index, voice) in voices.iter().enumerate() {
                    result.set(index + 1, voice_info_to_lua(lua, voice)?)?;
                }
                Ok(Value::Table(result))
            }
        }
    }
}

fn get_option(sr: &ScreenReader, option: &str) -> anyhow::Result<OptionValue> {
    use OptionValue::*;
    Ok(match option {
        "speech.server" => Server(sr.speech_server_spec().clone()),
        "speech.rate" => Number(sr.speech().rate()),
        "speech.pitch" => Number(sr.speech().pitch()),
        "speech.volume" => Number(sr.speech().volume()),
        "speech.paragraph_pause_ms" => Integer(sr.speech().paragraph_pause_ms()),
        "speech.voice" => Text(sr.speech().voice().map(|voice| voice.id)),
        "speech.voices" => Voices(sr.speech().voices()),
        "symbol_level" => Text(Some(sr.speech().symbol_level().to_string())),
        "legacy_escape_timeout_ms" => Integer(sr.legacy_escape_timeout_ms().into()),
        _ => {
            return get_reader_option(sr, option)
                .ok_or_else(|| anyhow!("get option: {option}: unknown option"));
        }
    })
}

fn set_option(sr: &mut ScreenReader, option: &str, value: mlua::Value) -> anyhow::Result<()> {
    use mlua::Value::*;
    if option == "legacy_escape_timeout_ms" {
        let Integer(value) = value else {
            return Err(anyhow!(
                "set option: legacy_escape_timeout_ms: value must be an integer from 0 through 1000"
            ));
        };
        let milliseconds = u16::try_from(value).map_err(|_| {
            anyhow!(
                "set option: legacy_escape_timeout_ms: value must be an integer from 0 through 1000"
            )
        })?;
        if milliseconds > 1_000 {
            return Err(anyhow!(
                "set option: legacy_escape_timeout_ms: value must be an integer from 0 through 1000"
            ));
        }
        sr.set_legacy_escape_timeout_ms(milliseconds);
        return Ok(());
    }
    if option == "speech.paragraph_pause_ms" {
        let Integer(value) = value else {
            return Err(anyhow!(
                "set option: speech.paragraph_pause_ms: value must be a non-negative integer"
            ));
        };
        let milliseconds = u64::try_from(value).map_err(|_| {
            anyhow!("set option: speech.paragraph_pause_ms: value must be a non-negative integer")
        })?;
        return sr
            .speech_mut()
            .set_paragraph_pause_ms(milliseconds)
            .map_err(anyhow::Error::new)
            .context("set option: speech.paragraph_pause_ms");
    }
    type NumericSpeechSetter = fn(&mut Speech, f32) -> crate::speech::Result<SetOptionOutcome>;
    let speech_number_setter = match option {
        "speech.rate" => Some(Speech::set_rate_option as NumericSpeechSetter),
        "speech.pitch" => Some(Speech::set_pitch_option as NumericSpeechSetter),
        "speech.volume" => Some(Speech::set_volume_option as NumericSpeechSetter),
        _ => None,
    };
    if let Some(setter) = speech_number_setter {
        let number = match value {
            Number(value) => value as f32,
            Integer(value) => value as f32,
            _ => return Err(anyhow!("set option: {option}: value must be a number")),
        };
        // The speech layer still owns range/capability checks and dynamic
        // application. Keep Lua's option-specific attribution unchanged.
        let name = option
            .strip_prefix("speech.")
            .expect("matched speech option");
        return setter(sr.speech_mut(), number)
            .and_then(|outcome| match outcome {
                SetOptionOutcome::Accepted => Ok(()),
                SetOptionOutcome::Unsupported => Err(crate::speech::Error::Driver(anyhow!(
                    "lector.o.{option} is unavailable: speech host does not support setting {name}"
                ))),
            })
            .map_err(anyhow::Error::new)
            .with_context(|| format!("set option: {option}"));
    }
    if option == "speech.voice" {
        let String(value) = value else {
            return Err(anyhow!("set option: speech.voice: value must be a string"));
        };
        let voice_id = lua_utf8(&value, "speech voice ID")?;
        return sr
            .speech_mut()
            .set_voice_option(&voice_id)
            .and_then(|outcome| match outcome {
                SetOptionOutcome::Accepted => Ok(()),
                SetOptionOutcome::Unsupported => Err(crate::speech::Error::Driver(anyhow!(
                    "lector.o.speech.voice is unavailable: speech host does not support selecting a voice"
                ))),
            })
            .map_err(anyhow::Error::new)
            .context("set option: speech.voice");
    }
    (match option {
        "symbol_level" => match value {
            String(v) => {
                let level = v
                    .to_str()
                    .map_err(|e| anyhow!("{}", e))?
                    .parse::<symbols::Level>()?;
                sr.speech_mut().set_symbol_level(level);
                Ok(())
            }
            _ => Err(anyhow!("value must be a string")),
        },
        _ => return set_reader_option(sr, option, value),
    })
    .map_err(|e| anyhow!("set option: {}: {:?}", option, e))
}
