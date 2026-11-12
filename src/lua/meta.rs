use super::{access::ReaderAccess, ext::LuaResultExt};
use crate::{
    clipboard::{ClipboardRegister, SystemClipboardProvider},
    keymap::KeyBindings,
    screen_reader::ScreenReader,
    speech::{SetOptionOutcome, SpeechServerSpec, protocol::VoiceInfo, symbols},
};
use anyhow::{Context as AnyhowContext, anyhow};
use mlua::{Error, Function, IntoLua, Lua, Result, Table, Value};
use std::{collections::BTreeMap, rc::Rc};

macro_rules! add_callbacks_common {
    ($tbl:expr,
        set_option = $set_option:expr,
        get_option = $get_option:expr,
        get_config_dir = $get_config_dir:expr,
        set_symbol = $set_symbol:expr,
        set_binding = $set_binding:expr,
        get_binding = $get_binding:expr,
        get_symbol = $get_symbol:expr,
        clear_symbols = $clear_symbols:expr,
        set_hook = $set_hook:expr,
        get_hook = $get_hook:expr,
        get_clipboard_text = $get_clipboard_text:expr,
        set_clipboard_text = $set_clipboard_text:expr,
        clear_clipboard = $clear_clipboard:expr,
        get_clipboard_entries = $get_clipboard_entries:expr,
        get_clipboard_index = $get_clipboard_index:expr,
        set_clipboard_index = $set_clipboard_index:expr $(,)?
    ) => {{
        $tbl.set("set_option", $set_option)?;
        $tbl.set("get_option", $get_option)?;
        $tbl.set("get_config_dir", $get_config_dir)?;
        $tbl.set("set_symbol", $set_symbol)?;
        $tbl.set("set_binding", $set_binding)?;
        $tbl.set("get_binding", $get_binding)?;
        $tbl.set("get_symbol", $get_symbol)?;
        $tbl.set("clear_symbols", $clear_symbols)?;
        $tbl.set("set_hook", $set_hook)?;
        $tbl.set("get_hook", $get_hook)?;
        $tbl.set("get_clipboard_text", $get_clipboard_text)?;
        $tbl.set("set_clipboard_text", $set_clipboard_text)?;
        $tbl.set("clear_clipboard", $clear_clipboard)?;
        $tbl.set("get_clipboard_entries", $get_clipboard_entries)?;
        $tbl.set("get_clipboard_index", $get_clipboard_index)?;
        $tbl.set("set_clipboard_index", $set_clipboard_index)?;
        Ok(())
    }};
}

pub fn setup_static(lua: &Lua, access: ReaderAccess) -> Result<()> {
    let tbl_callbacks = lua.create_table()?;
    add_callbacks_static(lua, &tbl_callbacks, access)?;
    lua.load(include_str!("meta.lua"))
        .set_name("meta.lua")
        .call::<()>((tbl_callbacks,))?;
    Ok(())
}

fn add_callbacks_static(lua: &Lua, tbl_callbacks: &Table, access: ReaderAccess) -> Result<()> {
    let set_option = lua.create_function({
        let access = access.clone();
        move |_, (key, value): (String, mlua::Value)| {
            if key == "speech.server" {
                let spec = speech_server_spec_from_lua(value).map_err(Error::external)?;
                return access.with_mut(|sr| {
                    sr.set_startup_speech_server_spec(spec)
                        .map_err(Error::external)
                });
            }
            with_screen_reader_mut(&access, |sr| {
                set_option(sr, &key, value)
                    .map_err(|error| Error::external(anyhow!(format!("{error:#}"))))
            })
        }
    })?;
    let get_option = lua.create_function({
        let access = access.clone();
        move |lua, key: String| {
            let value = access.with(|sr| get_option(sr, &key).map_err(Error::external))?;
            value.into_lua(lua)
        }
    })?;
    let get_config_dir = lua.create_function({
        let access = access.clone();
        move |lua, ()| {
            access
                .with(|sr| {
                    Ok(sr
                        .config_dir()
                        .map(|path| path.to_string_lossy().into_owned()))
                })?
                .into_lua(lua)
        }
    })?;
    let set_symbol = lua.create_function({
        let access = access.clone();
        move |_, (key, value): (String, mlua::Value)| {
            let symbol = match value {
                Value::Nil => None,
                Value::Table(table) => Some((
                    table.get::<String>(1)?,
                    table
                        .get::<String>(2)?
                        .parse::<symbols::Level>()
                        .context("parse level")
                        .to_lua_result()?,
                    table
                        .get::<String>(3)?
                        .parse::<symbols::IncludeOriginal>()
                        .context("parse include_original")
                        .to_lua_result()?,
                    table.get::<bool>(4)?,
                )),
                _ => return Err(Error::runtime("symbol value must be a table or nil")),
            };
            access.with_mut(|sr| {
                if let Some((replacement, level, include_original, repeat)) = symbol {
                    sr.speech_mut()
                        .set_symbol(&key, &replacement, level, include_original, repeat);
                } else {
                    sr.speech_mut().remove_symbol(&key);
                }
                Ok(())
            })
        }
    })?;
    let set_binding = lua.create_function({
        let access = access.clone();
        move |lua, (key, value): (String, mlua::Value)| {
            let binding = parse_binding(value).map_err(Error::external)?;
            with_screen_reader_mut(&access, |sr| {
                set_binding(lua, sr, &key, binding).map_err(Error::external)
            })
        }
    })?;
    let get_binding = lua.create_function({
        let access = access.clone();
        move |lua, key: String| get_binding(&access, lua, &key)
    })?;
    let get_symbol = lua.create_function({
        let access = access.clone();
        move |lua, key: String| {
            let symbol = access.with(|sr| {
                Ok(sr.speech().symbol(&key).map(|symbol| {
                    (
                        symbol.replacement.clone(),
                        symbol.level.to_string(),
                        symbol.include_original.to_string(),
                        symbol.repeat,
                    )
                }))
            })?;
            match symbol {
                Some((replacement, level, include_original, repeat)) => {
                    let table = lua.create_table()?;
                    table.set(1, replacement)?;
                    table.set(2, level)?;
                    table.set(3, include_original)?;
                    table.set(4, repeat)?;
                    Ok(Value::Table(table))
                }
                None => Ok(Value::Nil),
            }
        }
    })?;
    let clear_symbols = lua.create_function({
        let access = access.clone();
        move |_, ()| {
            with_screen_reader_mut(&access, |sr| {
                sr.speech_mut().clear_symbols();
                Ok(())
            })
        }
    })?;
    let set_hook = lua.create_function({
        let access = access.clone();
        move |lua, (key, value): (String, Value)| {
            with_screen_reader_mut(&access, |sr| {
                sr.set_hook(lua, &key, value).map_err(Error::external)
            })
        }
    })?;
    let get_hook = lua.create_function({
        let access = access.clone();
        move |lua, key: String| {
            with_screen_reader(&access, |sr| {
                sr.get_hook(lua, &key).map_err(Error::external)
            })
        }
    })?;
    let get_clipboard_text = lua.create_function({
        let access = access.clone();
        move |lua, name: String| {
            let register = clipboard_register(&name).map_err(Error::external)?;
            access
                .with_mut(|sr| sr.read_clipboard(register).map_err(Error::external))?
                .into_lua(lua)
        }
    })?;
    let set_clipboard_text = lua.create_function({
        let access = access.clone();
        move |_, (name, text): (String, String)| {
            with_screen_reader_mut(&access, |sr| {
                let register = clipboard_register(&name).map_err(Error::external)?;
                sr.write_clipboard(register, text).map_err(Error::external)
            })
        }
    })?;
    let clear_clipboard = lua.create_function({
        let access = access.clone();
        move |_, name: String| {
            with_screen_reader_mut(&access, |sr| {
                let register = clipboard_register(&name).map_err(Error::external)?;
                sr.clear_clipboard(register).map_err(Error::external)
            })
        }
    })?;
    let get_clipboard_entries = lua.create_function({
        let access = access.clone();
        move |lua, ()| {
            let entries = access.with(|sr| Ok(sr.internal_clipboard_entries()))?;
            lua.create_sequence_from(entries)
        }
    })?;
    let get_clipboard_index = lua.create_function({
        let access = access.clone();
        move |lua, ()| {
            access
                .with(|sr| Ok(sr.internal_clipboard_index()))?
                .into_lua(lua)
        }
    })?;
    let set_clipboard_index = lua.create_function({
        let access = access.clone();
        move |_, index: usize| {
            with_screen_reader_mut(&access, |sr| {
                sr.select_internal_clipboard(index).map_err(Error::external)
            })
        }
    })?;

    add_callbacks_common!(
        tbl_callbacks,
        set_option = set_option,
        get_option = get_option,
        get_config_dir = get_config_dir,
        set_symbol = set_symbol,
        set_binding = set_binding,
        get_binding = get_binding,
        get_symbol = get_symbol,
        clear_symbols = clear_symbols,
        set_hook = set_hook,
        get_hook = get_hook,
        get_clipboard_text = get_clipboard_text,
        set_clipboard_text = set_clipboard_text,
        clear_clipboard = clear_clipboard,
        get_clipboard_entries = get_clipboard_entries,
        get_clipboard_index = get_clipboard_index,
        set_clipboard_index = set_clipboard_index,
    )
}

fn clipboard_register(name: &str) -> anyhow::Result<ClipboardRegister> {
    match name {
        "internal" => Ok(ClipboardRegister::Internal),
        "system" => Ok(ClipboardRegister::System),
        _ => Err(anyhow!("clipboard namespace must be internal or system")),
    }
}

// Snapshot reader data before allocating Lua strings/tables: allocation can
// collect userdata and run Lua finalizers.
enum OptionValue {
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
        "help_mode" => Boolean(sr.help_mode()),
        "auto_read" => Boolean(sr.auto_read_enabled()),
        "suppress_key_echo" => Boolean(sr.suppress_key_echo()),
        "report_indentation" => Boolean(sr.indentation_reporting_enabled()),
        "review_follows_screen_cursor" | "rev_follows" => {
            Boolean(sr.review_follows_screen_cursor())
        }
        "highlight_tracking" => Boolean(sr.highlight_tracking_enabled()),
        "stop_speech_on_focus_loss" => Boolean(sr.stop_speech_on_focus_loss()),
        "legacy_escape_timeout_ms" => Integer(sr.legacy_escape_timeout_ms().into()),
        "tmux_bells" => Text(Some(sr.tmux_bell_mode().to_string())),
        "clipboard.default_register" => Text(Some(sr.clipboard_default_register().to_string())),
        "clipboard.system_provider" => Text(Some(sr.system_clipboard_provider().to_string())),
        _ => return Err(anyhow!("get option: {option}: unknown option")),
    })
}

enum ParsedBinding {
    Clear,
    Builtin(crate::commands::Action),
    Lua(String, Function),
}

fn parse_binding(value: Value) -> anyhow::Result<ParsedBinding> {
    match value {
        Value::Nil => Ok(ParsedBinding::Clear),
        Value::String(name) => Ok(ParsedBinding::Builtin(
            KeyBindings::builtin_action_from_value(name.to_str()?.as_ref())?,
        )),
        Value::Table(table) => {
            let (help, function) = parse_binding_table(table)?;
            Ok(ParsedBinding::Lua(help, function))
        }
        _ => Err(anyhow!("binding value must be a string, table, or nil")),
    }
}

fn set_binding(
    lua: &Lua,
    sr: &mut ScreenReader,
    key: &str,
    binding: ParsedBinding,
) -> anyhow::Result<()> {
    let (mode, key) = sr.key_bindings().split_mode_key(key);
    match binding {
        ParsedBinding::Clear => sr.key_bindings_mut().clear_binding_for_mode(mode, key),
        ParsedBinding::Builtin(action) => {
            sr.key_bindings_mut()
                .set_builtin_binding_for_mode(mode, key.to_string(), action)
        }
        ParsedBinding::Lua(help, func) => {
            let ctx = Rc::clone(sr.lua_binding_context(lua).map_err(anyhow::Error::new)?);
            sr.key_bindings_mut().set_lua_binding_for_mode(
                mode,
                key.to_string(),
                help,
                ctx,
                func,
            )?;
        }
    }
    Ok(())
}

fn parse_binding_table(table: Table) -> anyhow::Result<(String, Function)> {
    let help = match table.get::<String>("help") {
        Ok(help) => help,
        Err(_) => table.get(1).map_err(|err| anyhow!(err.to_string()))?,
    };
    let func = match table.get::<Function>("fn") {
        Ok(func) => func,
        Err(_) => table.get(2).map_err(|err| anyhow!(err.to_string()))?,
    };
    Ok((help, func))
}

fn get_binding(access: &ReaderAccess, lua: &Lua, key: &str) -> Result<Value> {
    let value = access.with(|sr| {
        let (mode, key) = sr.key_bindings().split_mode_key(key);
        sr.key_bindings()
            .binding_snapshot_for_lua_mode(mode, key, sr.owns_lua_context(lua))
    })?;
    value.into_lua(lua)
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
    if option == "speech.rate" {
        let rate = match value {
            Number(value) => value as f32,
            Integer(value) => value as f32,
            _ => return Err(anyhow!("set option: speech.rate: value must be a number")),
        };
        return sr
            .speech_mut()
            .set_rate_option(rate)
            .and_then(|outcome| match outcome {
                SetOptionOutcome::Accepted => Ok(()),
                SetOptionOutcome::Unsupported => Err(crate::speech::Error::Driver(anyhow!(
                    "lector.o.speech.rate is unavailable: speech host does not support setting rate"
                ))),
            })
            .map_err(anyhow::Error::new)
            .context("set option: speech.rate");
    }
    if option == "speech.pitch" {
        let pitch = match value {
            Number(value) => value as f32,
            Integer(value) => value as f32,
            _ => return Err(anyhow!("set option: speech.pitch: value must be a number")),
        };
        return sr
            .speech_mut()
            .set_pitch_option(pitch)
            .and_then(|outcome| match outcome {
                SetOptionOutcome::Accepted => Ok(()),
                SetOptionOutcome::Unsupported => Err(crate::speech::Error::Driver(anyhow!(
                    "lector.o.speech.pitch is unavailable: speech host does not support setting pitch"
                ))),
            })
            .map_err(anyhow::Error::new)
            .context("set option: speech.pitch");
    }
    if option == "speech.volume" {
        let volume = match value {
            Number(value) => value as f32,
            Integer(value) => value as f32,
            _ => return Err(anyhow!("set option: speech.volume: value must be a number")),
        };
        return sr
            .speech_mut()
            .set_volume_option(volume)
            .and_then(|outcome| match outcome {
                SetOptionOutcome::Accepted => Ok(()),
                SetOptionOutcome::Unsupported => Err(crate::speech::Error::Driver(anyhow!(
                    "lector.o.speech.volume is unavailable: speech host does not support setting volume"
                ))),
            })
            .map_err(anyhow::Error::new)
            .context("set option: speech.volume");
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
        "help_mode" => match value {
            Boolean(v) => {
                sr.set_help_mode(v);
                Ok(())
            }
            _ => Err(anyhow!("value must be a boolean")),
        },
        "auto_read" => match value {
            Boolean(v) => {
                sr.set_auto_read_enabled(v);
                Ok(())
            }
            _ => Err(anyhow!("value must be a boolean")),
        },
        "suppress_key_echo" => match value {
            Boolean(v) => {
                sr.set_suppress_key_echo(v);
                Ok(())
            }
            _ => Err(anyhow!("value must be a boolean")),
        },
        "report_indentation" => match value {
            Boolean(v) => {
                sr.set_indentation_reporting_enabled(v);
                Ok(())
            }
            _ => Err(anyhow!("value must be a boolean")),
        },
        "review_follows_screen_cursor" | "rev_follows" => match value {
            Boolean(v) => {
                sr.set_review_follows_screen_cursor(v);
                Ok(())
            }
            _ => Err(anyhow!("value must be a boolean")),
        },
        "highlight_tracking" => match value {
            Boolean(v) => {
                sr.set_highlight_tracking_enabled(v);
                Ok(())
            }
            _ => Err(anyhow!("value must be a boolean")),
        },
        "stop_speech_on_focus_loss" => match value {
            Boolean(v) => {
                sr.set_stop_speech_on_focus_loss(v);
                Ok(())
            }
            _ => Err(anyhow!("value must be a boolean")),
        },
        "tmux_bells" => match value {
            String(v) => {
                let mode = v
                    .to_str()
                    .map_err(|e| anyhow!(e.to_string()))?
                    .parse::<crate::screen_reader::TmuxBellMode>()?;
                sr.set_tmux_bell_mode(mode);
                Ok(())
            }
            _ => Err(anyhow!("value must be a string")),
        },
        "clipboard.default_register" => match value {
            String(v) => {
                let register = v
                    .to_str()
                    .map_err(|e| anyhow!(e.to_string()))?
                    .parse::<ClipboardRegister>()?;
                sr.set_clipboard_default_register(register);
                Ok(())
            }
            _ => Err(anyhow!("value must be a string")),
        },
        "clipboard.system_provider" => match value {
            String(v) => {
                let provider = v
                    .to_str()
                    .map_err(|e| anyhow!(e.to_string()))?
                    .parse::<SystemClipboardProvider>()?;
                sr.set_system_clipboard_provider(provider);
                Ok(())
            }
            _ => Err(anyhow!("value must be a string")),
        },
        _ => Err(anyhow!("unknown option")),
    })
    .map_err(|e| anyhow!("set option: {}: {:?}", option, e))
}

fn voice_info_to_lua(lua: &Lua, voice: &VoiceInfo) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("id", voice.id.as_str())?;
    table.set("name", voice.name.as_str())?;
    table.set("language", voice.language.as_str())?;
    table.set("gender", voice.gender.as_deref())?;
    Ok(table)
}

pub(super) fn speech_server_spec_from_lua(value: Value) -> anyhow::Result<SpeechServerSpec> {
    match value {
        Value::String(value) => {
            let value = lua_utf8(&value, "speech server")?;
            if value == "native" {
                Ok(SpeechServerSpec::Native)
            } else {
                Err(anyhow!(
                    "speech server string must be \"native\" or a process table"
                ))
            }
        }
        Value::Table(table) => parse_process_speech_server(table),
        _ => Err(anyhow!(
            "speech server must be \"native\" or a table with program and args"
        )),
    }
}

pub(super) fn speech_server_spec_to_lua(lua: &Lua, spec: &SpeechServerSpec) -> mlua::Result<Value> {
    match spec {
        SpeechServerSpec::Native => "native".into_lua(lua),
        SpeechServerSpec::Process { program, args } => {
            let table = lua.create_table()?;
            table.set("program", program.as_str())?;
            table.set(
                "args",
                lua.create_sequence_from(args.iter().map(String::as_str))?,
            )?;
            Ok(Value::Table(table))
        }
    }
}

fn parse_process_speech_server(table: Table) -> anyhow::Result<SpeechServerSpec> {
    for pair in table.clone().pairs::<Value, Value>() {
        let (key, _) = pair.map_err(|error| anyhow!(error.to_string()))?;
        let Value::String(key) = key else {
            return Err(anyhow!("speech server table keys must be strings"));
        };
        match lua_utf8(&key, "speech server table key")?.as_str() {
            "program" | "args" => {}
            key => return Err(anyhow!("unknown speech server field: {key}")),
        }
    }

    let program_value = table
        .get::<Value>("program")
        .map_err(|error| anyhow!(error.to_string()))?;
    let Value::String(program_value) = program_value else {
        return Err(anyhow!("speech server program must be a string"));
    };
    let program = lua_utf8(&program_value, "speech server program")?;
    if program.is_empty() {
        return Err(anyhow!("speech server program must not be empty"));
    }
    reject_nul(&program, "speech server program")?;

    let args = match table
        .get::<Value>("args")
        .map_err(|error| anyhow!(error.to_string()))?
    {
        Value::Nil => Vec::new(),
        Value::Table(args) => parse_speech_server_args(args)?,
        _ => return Err(anyhow!("speech server args must be an array of strings")),
    };

    Ok(SpeechServerSpec::Process { program, args })
}

fn parse_speech_server_args(table: Table) -> anyhow::Result<Vec<String>> {
    let mut indexed = BTreeMap::new();
    for pair in table.pairs::<Value, Value>() {
        let (key, value) = pair.map_err(|error| anyhow!(error.to_string()))?;
        let Value::Integer(index) = key else {
            return Err(anyhow!(
                "speech server args must have consecutive integer indexes starting at 1"
            ));
        };
        let index = usize::try_from(index)
            .ok()
            .filter(|index| *index > 0)
            .ok_or_else(|| {
                anyhow!("speech server args must have consecutive integer indexes starting at 1")
            })?;
        let Value::String(value) = value else {
            return Err(anyhow!("speech server argument {index} must be a string"));
        };
        let value = lua_utf8(&value, &format!("speech server argument {index}"))?;
        reject_nul(&value, &format!("speech server argument {index}"))?;
        indexed.insert(index, value);
    }

    let mut args = Vec::with_capacity(indexed.len());
    for expected in 1..=indexed.len() {
        let Some(value) = indexed.remove(&expected) else {
            return Err(anyhow!(
                "speech server args must have consecutive integer indexes starting at 1"
            ));
        };
        args.push(value);
    }
    Ok(args)
}

fn lua_utf8(value: &mlua::LuaString, field: &str) -> anyhow::Result<String> {
    value
        .to_str()
        .map(|value| value.to_string())
        .map_err(|_| anyhow!("{field} must be valid UTF-8"))
}

fn reject_nul(value: &str, field: &str) -> anyhow::Result<()> {
    if value.contains('\0') {
        Err(anyhow!("{field} must not contain a NUL byte"))
    } else {
        Ok(())
    }
}

fn with_screen_reader_mut<T>(
    access: &ReaderAccess,
    f: impl FnOnce(&mut ScreenReader) -> Result<T>,
) -> Result<T> {
    access.with_mut(f)
}

fn with_screen_reader<T>(
    access: &ReaderAccess,
    f: impl FnOnce(&ScreenReader) -> Result<T>,
) -> Result<T> {
    access.with(f)
}
