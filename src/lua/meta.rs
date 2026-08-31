use super::ext::LuaResultExt;
use crate::{
    clipboard::{ClipboardRegister, SystemClipboardProvider},
    keymap::KeyBindings,
    screen_reader::ScreenReader,
    speech::{SetOptionOutcome, SpeechServerSpec, protocol::VoiceInfo, symbols},
};
use anyhow::{Context as AnyhowContext, anyhow};
use mlua::{Error, Function, IntoLua, Lua, Result, Table, Value};
use std::{cell::RefCell, collections::BTreeMap, rc::Rc};

macro_rules! add_callbacks_common {
    ($tbl:expr,
        set_option = $set_option:expr,
        get_option = $get_option:expr,
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

pub fn setup_static(lua: &Lua, sr_ptr: Rc<RefCell<*mut ScreenReader>>) -> Result<()> {
    let tbl_callbacks = lua.create_table()?;
    add_callbacks_static(lua, &tbl_callbacks, sr_ptr)?;
    lua.load(include_str!("meta.lua"))
        .set_name("meta.lua")
        .call::<()>((tbl_callbacks,))?;
    Ok(())
}

fn add_callbacks_static(
    lua: &Lua,
    tbl_callbacks: &Table,
    sr_ptr: Rc<RefCell<*mut ScreenReader>>,
) -> Result<()> {
    let set_option = lua.create_function_mut({
        let sr_ptr = Rc::clone(&sr_ptr);
        move |_, (key, value): (String, mlua::Value)| {
            with_screen_reader_mut(&sr_ptr, |sr| {
                set_option(sr, &key, value)
                    .map_err(|error| Error::external(anyhow!(format!("{error:#}"))))
            })
        }
    })?;
    let get_option = lua.create_function({
        let sr_ptr = Rc::clone(&sr_ptr);
        move |lua, key: String| {
            with_screen_reader(&sr_ptr, |sr| {
                get_option(lua, sr, &key).map_err(Error::external)
            })
        }
    })?;
    let set_symbol = lua.create_function_mut({
        let sr_ptr = Rc::clone(&sr_ptr);
        move |_, (key, value): (String, mlua::Value)| {
            with_screen_reader_mut(&sr_ptr, |sr| match value {
                mlua::Value::Nil => {
                    sr.speech_mut().remove_symbol(&key);
                    Ok(())
                }
                mlua::Value::Table(table_value) => {
                    let replacement: String = table_value.get(1)?;
                    let level: symbols::Level = AnyhowContext::context(
                        table_value.get::<String>(2)?.parse(),
                        "parse level",
                    )
                    .to_lua_result()?;
                    let include_original: symbols::IncludeOriginal = AnyhowContext::context(
                        table_value.get::<String>(3)?.parse(),
                        "parse include_original",
                    )
                    .to_lua_result()?;
                    let repeat: bool = table_value.get(4)?;
                    sr.speech_mut()
                        .set_symbol(&key, &replacement, level, include_original, repeat);
                    Ok(())
                }
                _ => Err(Error::external(anyhow!(
                    "symbol value must be a table or nil"
                ))),
            })
        }
    })?;
    let set_binding = lua.create_function_mut({
        let sr_ptr = Rc::clone(&sr_ptr);
        move |lua, (key, value): (String, mlua::Value)| {
            with_screen_reader_mut(&sr_ptr, |sr| {
                set_binding(lua, sr, &key, value).map_err(Error::external)
            })
        }
    })?;
    let get_binding = lua.create_function({
        let sr_ptr = Rc::clone(&sr_ptr);
        move |lua, key: String| {
            with_screen_reader(&sr_ptr, |sr| {
                get_binding(lua, sr, &key).map_err(Error::external)
            })
        }
    })?;
    let get_symbol = lua.create_function({
        let sr_ptr = Rc::clone(&sr_ptr);
        move |lua, key: String| {
            with_screen_reader(&sr_ptr, |sr| {
                let value = match sr.speech().symbol(&key) {
                    Some(v) => {
                        let tbl = lua.create_table()?;
                        tbl.set(1, v.replacement.clone())?;
                        tbl.set(2, v.level.to_string())?;
                        tbl.set(3, v.include_original.to_string())?;
                        tbl.set(4, v.repeat)?;
                        Value::Table(tbl)
                    }
                    None => Value::Nil,
                };
                Ok(value)
            })
        }
    })?;
    let clear_symbols = lua.create_function_mut({
        let sr_ptr = Rc::clone(&sr_ptr);
        move |_, ()| {
            with_screen_reader_mut(&sr_ptr, |sr| {
                sr.speech_mut().clear_symbols();
                Ok(())
            })
        }
    })?;
    let set_hook = lua.create_function_mut({
        let sr_ptr = Rc::clone(&sr_ptr);
        move |lua, (key, value): (String, Value)| {
            with_screen_reader_mut(&sr_ptr, |sr| {
                sr.set_hook(lua, &key, value).map_err(Error::external)
            })
        }
    })?;
    let get_hook = lua.create_function({
        let sr_ptr = Rc::clone(&sr_ptr);
        move |lua, key: String| {
            with_screen_reader(&sr_ptr, |sr| {
                sr.get_hook(lua, &key).map_err(Error::external)
            })
        }
    })?;
    let get_clipboard_text = lua.create_function_mut({
        let sr_ptr = Rc::clone(&sr_ptr);
        move |lua, name: String| {
            with_screen_reader_mut(&sr_ptr, |sr| {
                let register = clipboard_register(&name).map_err(Error::external)?;
                sr.read_clipboard(register)
                    .map_err(Error::external)?
                    .into_lua(lua)
            })
        }
    })?;
    let set_clipboard_text = lua.create_function_mut({
        let sr_ptr = Rc::clone(&sr_ptr);
        move |_, (name, text): (String, String)| {
            with_screen_reader_mut(&sr_ptr, |sr| {
                let register = clipboard_register(&name).map_err(Error::external)?;
                sr.write_clipboard(register, text).map_err(Error::external)
            })
        }
    })?;
    let clear_clipboard = lua.create_function_mut({
        let sr_ptr = Rc::clone(&sr_ptr);
        move |_, name: String| {
            with_screen_reader_mut(&sr_ptr, |sr| {
                let register = clipboard_register(&name).map_err(Error::external)?;
                sr.clear_clipboard(register).map_err(Error::external)
            })
        }
    })?;
    let get_clipboard_entries = lua.create_function({
        let sr_ptr = Rc::clone(&sr_ptr);
        move |lua, ()| {
            with_screen_reader(&sr_ptr, |sr| {
                lua.create_sequence_from(sr.internal_clipboard_entries())
            })
        }
    })?;
    let get_clipboard_index = lua.create_function({
        let sr_ptr = Rc::clone(&sr_ptr);
        move |lua, ()| with_screen_reader(&sr_ptr, |sr| sr.internal_clipboard_index().into_lua(lua))
    })?;
    let set_clipboard_index = lua.create_function_mut({
        let sr_ptr = Rc::clone(&sr_ptr);
        move |_, index: usize| {
            with_screen_reader_mut(&sr_ptr, |sr| {
                sr.select_internal_clipboard(index).map_err(Error::external)
            })
        }
    })?;

    add_callbacks_common!(
        tbl_callbacks,
        set_option = set_option,
        get_option = get_option,
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

fn get_option(lua: &Lua, sr: &ScreenReader, option: &str) -> anyhow::Result<mlua::Value> {
    match option {
        "speech.server" => speech_server_spec_to_lua(lua, sr.speech_server_spec()),
        "speech.rate" => sr.speech().rate().into_lua(lua),
        "speech.pitch" => sr.speech().pitch().into_lua(lua),
        "speech.volume" => sr.speech().volume().into_lua(lua),
        "speech.paragraph_pause_ms" => sr.speech().paragraph_pause_ms().into_lua(lua),
        "speech.voice" => sr.speech().voice().map(|voice| voice.id).into_lua(lua),
        "speech.voices" => match sr.speech().voices() {
            Some(voices) => {
                let result = lua
                    .create_table()
                    .map_err(|error| anyhow!(error.to_string()))?;
                for (index, voice) in voices.iter().enumerate() {
                    let voice = voice_info_to_lua(lua, voice)
                        .map_err(|error| anyhow!(error.to_string()))?;
                    result
                        .set(index + 1, voice)
                        .map_err(|error| anyhow!(error.to_string()))?;
                }
                Ok(Value::Table(result))
            }
            None => Ok(Value::Nil),
        },
        "symbol_level" => sr.speech().symbol_level().to_string().into_lua(lua),
        "help_mode" => sr.help_mode().into_lua(lua),
        "auto_read" => sr.auto_read_enabled().into_lua(lua),
        "suppress_key_echo" => sr.suppress_key_echo().into_lua(lua),
        "report_indentation" => sr.indentation_reporting_enabled().into_lua(lua),
        "review_follows_screen_cursor" | "rev_follows" => {
            sr.review_follows_screen_cursor().into_lua(lua)
        }
        "highlight_tracking" => sr.highlight_tracking_enabled().into_lua(lua),
        "stop_speech_on_focus_loss" => sr.stop_speech_on_focus_loss().into_lua(lua),
        "tmux_bells" => sr.tmux_bell_mode().to_string().into_lua(lua),
        "clipboard.default_register" => sr.clipboard_default_register().to_string().into_lua(lua),
        "clipboard.system_provider" => sr.system_clipboard_provider().to_string().into_lua(lua),
        _ => Err(Error::external(anyhow!("unknown option"))),
    }
    .map_err(|e| anyhow!("{}", e))
    .context(format!("get option: {}", option))
}

fn set_binding(lua: &Lua, sr: &mut ScreenReader, key: &str, value: Value) -> anyhow::Result<()> {
    let (mode, key) = sr.key_bindings().split_mode_key(key);
    match value {
        Value::Nil => {
            sr.key_bindings_mut().clear_binding_for_mode(mode, key);
            Ok(())
        }
        Value::String(name) => {
            let name = name.to_str().map_err(|err| anyhow!(err.to_string()))?;
            let action = KeyBindings::builtin_action_from_value(name.as_ref())?;
            sr.key_bindings_mut()
                .set_builtin_binding_for_mode(mode, key.to_string(), action);
            Ok(())
        }
        Value::Table(table) => {
            let (help, func) = parse_binding_table(table)?;
            let ctx = Rc::clone(sr.lua_binding_context(lua).map_err(anyhow::Error::new)?);
            sr.key_bindings_mut().set_lua_binding_for_mode(
                mode,
                key.to_string(),
                help,
                ctx,
                func,
            )?;
            Ok(())
        }
        _ => Err(anyhow!("binding value must be a string, table, or nil")),
    }
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

fn get_binding(lua: &Lua, sr: &ScreenReader, key: &str) -> anyhow::Result<Value> {
    let allow_function = sr.owns_lua_context(lua);
    let (mode, key) = sr.key_bindings().split_mode_key(key);
    sr.key_bindings()
        .binding_value_for_lua_mode(mode, key, lua, allow_function)
        .map_err(|err| anyhow!(err.to_string()))
}

fn set_option(sr: &mut ScreenReader, option: &str, value: mlua::Value) -> anyhow::Result<()> {
    use mlua::Value::*;
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
        "speech.server" => sr
            .set_startup_speech_server_spec(speech_server_spec_from_lua(value)?)
            .map_err(anyhow::Error::new),
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

fn lua_utf8(value: &mlua::String, field: &str) -> anyhow::Result<String> {
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
    sr_ptr: &Rc<RefCell<*mut ScreenReader>>,
    f: impl FnOnce(&mut ScreenReader) -> Result<T>,
) -> Result<T> {
    let ptr = *sr_ptr.borrow();
    if ptr.is_null() {
        return Err(Error::external(anyhow!("screen reader unavailable")));
    }
    // Safety: the pointer is set by the main thread before any Lua call.
    unsafe { f(&mut *ptr) }
}

fn with_screen_reader<T>(
    sr_ptr: &Rc<RefCell<*mut ScreenReader>>,
    f: impl FnOnce(&ScreenReader) -> Result<T>,
) -> Result<T> {
    let ptr = *sr_ptr.borrow();
    if ptr.is_null() {
        return Err(Error::external(anyhow!("screen reader unavailable")));
    }
    // Safety: the pointer is set by the main thread before any Lua call.
    unsafe { f(&*ptr) }
}
