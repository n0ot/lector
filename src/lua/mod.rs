use self::ext::LuaResultExt;
use crate::screen_reader::ScreenReader;
use anyhow::{Context as AnyhowContext, anyhow};
use mlua::{Error, Function, Lua, LuaOptions, Result, StdLib, Table, Value};
use std::{
    cell::RefCell,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    rc::Rc,
};

pub(crate) mod automation;
mod ext;
mod inspect;
mod meta;

pub fn setup<F>(
    init_lua_file: PathBuf,
    load_init_file: bool,
    screen_reader: &mut ScreenReader,
    after: F,
) -> Result<()>
where
    F: FnOnce(&mut ScreenReader) -> anyhow::Result<()>,
{
    let config_dir = effective_config_dir(&init_lua_file)?;
    screen_reader.set_config_dir(config_dir);
    let lua = Rc::new(Lua::new_with(
        StdLib::ALL_SAFE | StdLib::JIT,
        LuaOptions::default(),
    )?);
    screen_reader.set_lua_context(Rc::clone(&lua));
    let sr_ptr = Rc::new(RefCell::new(screen_reader as *mut ScreenReader));
    install_api_static(&lua, Rc::clone(&sr_ptr))?;
    install_config_module_searcher(&lua, Rc::clone(&sr_ptr))?;
    meta::setup_static(&lua, Rc::clone(&sr_ptr))?;

    let configuration_result = if load_init_file && init_lua_file.is_file() {
        load_file(&lua, &init_lua_file).and_then(|function| function.call::<()>(()))
    } else {
        Ok(())
    };
    screen_reader.finish_lua_configuration();
    configuration_result?;

    let result = after(screen_reader);
    match result {
        Ok(()) => {
            screen_reader
                .hook_on_shutdown("exit")
                .map_err(Error::external)?;
            Ok(())
        }
        Err(err) => {
            let detail = format!("{err:#}");
            let _ = screen_reader.hook_on_error(&detail, "runtime");
            let _ = screen_reader.hook_on_shutdown("error");
            Err(Error::external(anyhow!(detail)))
        }
    }
}

pub fn setup_repl(lua: &Lua, sr_ptr: Rc<RefCell<*mut ScreenReader>>) -> Result<()> {
    install_api_static(lua, Rc::clone(&sr_ptr))?;
    install_config_module_searcher(lua, Rc::clone(&sr_ptr))?;
    meta::setup_static(lua, sr_ptr)?;
    inspect::install(lua)?;
    Ok(())
}

fn effective_config_dir(init_lua_file: &Path) -> Result<Option<PathBuf>> {
    if init_lua_file.as_os_str().is_empty() {
        return Ok(None);
    }
    let parent = init_lua_file.parent().unwrap_or_else(|| Path::new(""));
    std::path::absolute(parent)
        .map(Some)
        .map_err(Error::external)
}

/// Add a config-rooted loader after Lua's preload loader and before its
/// ordinary filesystem loaders. `require("a.b")` therefore checks
/// `<config_dir>/a/b.lua` and `<config_dir>/a/b/init.lua` without changing the
/// process working directory or interpolating the config path into
/// `package.path`.
fn install_config_module_searcher(lua: &Lua, sr_ptr: Rc<RefCell<*mut ScreenReader>>) -> Result<()> {
    let searcher = lua.create_function(move |lua, module_name: String| {
        let ptr = *sr_ptr.borrow();
        let config_dir = if ptr.is_null() {
            None
        } else {
            // Safety: the pointer is installed by the terminal thread before
            // config or REPL code can call require.
            unsafe { (&*ptr).config_dir().map(Path::to_path_buf) }
        };
        let Some(config_dir) = config_dir else {
            return config_searcher_miss(lua, "no Lector configuration directory");
        };
        let Some(relative) = config_module_relative_path(&module_name) else {
            return config_searcher_miss(lua, "invalid Lector config module name");
        };

        let mut module_file = relative.clone();
        module_file.set_extension("lua");
        let candidates = [
            config_dir.join(module_file),
            config_dir.join(relative).join("init.lua"),
        ];
        for candidate in &candidates {
            if candidate.is_file() {
                return load_file(lua, candidate).map(Value::Function);
            }
        }
        let detail = candidates
            .iter()
            .map(|candidate| format!("no file '{}'", candidate.display()))
            .collect::<Vec<_>>()
            .join("\n\t");
        config_searcher_miss(lua, &detail)
    })?;

    let package: Table = lua.globals().get("package")?;
    let searchers = match package.get::<Value>("searchers")? {
        Value::Table(searchers) => searchers,
        _ => package.get::<Table>("loaders")?,
    };
    for index in (2..=searchers.raw_len()).rev() {
        let existing: Value = searchers.raw_get(index)?;
        searchers.raw_set(index + 1, existing)?;
    }
    searchers.raw_set(2, searcher)
}

fn config_module_relative_path(module_name: &str) -> Option<PathBuf> {
    let mut relative = PathBuf::new();
    for segment in module_name.split('.') {
        if segment.is_empty()
            || segment.contains('\0')
            || segment
                .chars()
                .any(|character| matches!(character, '/' | '\\'))
        {
            return None;
        }
        relative.push(segment);
    }
    (!relative.as_os_str().is_empty()).then_some(relative)
}

fn config_searcher_miss(lua: &Lua, detail: &str) -> Result<Value> {
    Ok(Value::String(lua.create_string(format!("\n\t{detail}"))?))
}

fn load_file(lua: &Lua, path: &PathBuf) -> Result<Function> {
    let path_string = path
        .to_str()
        .ok_or_else(|| anyhow!("convert path to string"))
        .to_lua_result()?
        .to_string();
    let mut f = File::open(path)
        .map_err(anyhow::Error::from)
        .context(format!("open {}", path_string))
        .to_lua_result()?;
    let mut s = String::new();
    f.read_to_string(&mut s)
        .map_err(anyhow::Error::from)
        .context(format!("read {}", path_string))
        .to_lua_result()?;

    lua.load(&s).set_name(&path_string).into_function()
}

fn install_api_static(lua: &Lua, sr_ptr: Rc<RefCell<*mut ScreenReader>>) -> Result<()> {
    let tbl_lector = lua.create_table()?;
    let tbl_api = lua.create_table()?;
    let speak_fn = lua.create_function({
        let sr_ptr = Rc::clone(&sr_ptr);
        move |_, (text, interrupt): (String, bool)| {
            let ptr = *sr_ptr.borrow();
            if ptr.is_null() {
                return Err(Error::external(anyhow!("screen reader unavailable")));
            }
            // Safety: pointer is set by the main thread before any Lua call.
            let sr = unsafe { &mut *ptr };
            sr.speak(&text, interrupt)
                .map(|id| id.map(|id| id.as_str().to_owned()))
                .to_lua_result()
        }
    })?;
    let set_speech_fn = lua.create_function_mut(move |_, value: Value| {
        let ptr = *sr_ptr.borrow();
        if ptr.is_null() {
            return Err(Error::external(anyhow!("screen reader unavailable")));
        }
        // Safety: pointer is set by the main thread before any Lua call.
        let sr = unsafe { &mut *ptr };
        let spec = meta::speech_server_spec_from_lua(value).map_err(Error::external)?;
        sr.request_speech_reconfiguration(spec);
        Ok(())
    })?;
    tbl_api.set("speak", speak_fn)?;
    tbl_api.set("set_speech", set_speech_fn)?;
    tbl_lector.set("api", tbl_api)?;
    lua.globals().set("lector", tbl_lector)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{setup, setup_repl};
    use crate::{
        keymap::{Binding, InputMode},
        screen_reader::ScreenReader,
        speech::{
            self, CapabilityStatus, OptionState, SetOptionOutcome, SpeechServerSpec,
            protocol::VoiceInfo, symbols::Level,
        },
        table::{Column, TableModel, TableState},
        view::View,
    };
    use mlua::Lua;
    use std::{
        cell::{Cell, RefCell},
        collections::BTreeMap,
        fs,
        rc::Rc,
        time::{SystemTime, UNIX_EPOCH},
    };

    struct RecordingDriver(Rc<RefCell<Vec<String>>>);

    impl speech::Driver for RecordingDriver {
        fn speak(&mut self, text: &str, _interrupt: bool) -> anyhow::Result<()> {
            self.0.borrow_mut().push(text.to_string());
            Ok(())
        }

        fn stop(&mut self) -> anyhow::Result<()> {
            Ok(())
        }

        fn get_rate(&self) -> f32 {
            1.0
        }

        fn set_rate(&mut self, _rate: f32) -> anyhow::Result<()> {
            Ok(())
        }
    }

    struct SpeechOptionsDriver {
        state: OptionState,
        rates: Rc<RefCell<Vec<f32>>>,
        pitches: Rc<RefCell<Vec<f32>>>,
        volumes: Rc<RefCell<Vec<f32>>>,
        voices: Rc<RefCell<Vec<String>>>,
    }

    impl speech::Driver for SpeechOptionsDriver {
        fn speak(&mut self, _text: &str, _interrupt: bool) -> anyhow::Result<()> {
            Ok(())
        }

        fn stop(&mut self) -> anyhow::Result<()> {
            Ok(())
        }

        fn get_rate(&self) -> f32 {
            self.state.rate.unwrap_or(1.0)
        }

        fn set_rate(&mut self, rate: f32) -> anyhow::Result<()> {
            self.rates.borrow_mut().push(rate);
            Ok(())
        }

        fn option_state(&self) -> OptionState {
            self.state.clone()
        }

        fn set_rate_option(&mut self, rate: f32) -> anyhow::Result<SetOptionOutcome> {
            if self.state.rate_status == CapabilityStatus::Unsupported {
                return Ok(SetOptionOutcome::Unsupported);
            }
            self.rates.borrow_mut().push(rate);
            Ok(SetOptionOutcome::Accepted)
        }

        fn set_pitch_option(&mut self, pitch: f32) -> anyhow::Result<SetOptionOutcome> {
            if self.state.pitch_status == CapabilityStatus::Unsupported {
                return Ok(SetOptionOutcome::Unsupported);
            }
            self.pitches.borrow_mut().push(pitch);
            Ok(SetOptionOutcome::Accepted)
        }

        fn set_volume_option(&mut self, volume: f32) -> anyhow::Result<SetOptionOutcome> {
            if self.state.volume_status == CapabilityStatus::Unsupported {
                return Ok(SetOptionOutcome::Unsupported);
            }
            self.volumes.borrow_mut().push(volume);
            Ok(SetOptionOutcome::Accepted)
        }

        fn set_voice_option(&mut self, voice_id: &str) -> anyhow::Result<SetOptionOutcome> {
            if self.state.voice_selection_status == CapabilityStatus::Unsupported {
                return Ok(SetOptionOutcome::Unsupported);
            }
            self.voices.borrow_mut().push(voice_id.to_owned());
            Ok(SetOptionOutcome::Accepted)
        }
    }

    fn screen_reader() -> ScreenReader {
        let output = Rc::new(RefCell::new(Vec::new()));
        let speech = speech::Speech::new(Box::new(RecordingDriver(output)));
        ScreenReader::new(speech)
    }

    fn temporary_lua_file(name: &str, source: &str) -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("lector-{name}-{unique}.lua"));
        fs::write(&path, source).unwrap();
        path
    }

    fn voice(id: &str, name: &str) -> VoiceInfo {
        VoiceInfo {
            id: id.to_owned(),
            name: name.to_owned(),
            language: "en-US".to_owned(),
            gender: Some("neutral".to_owned()),
            extensions: BTreeMap::new(),
        }
    }

    #[test]
    fn startup_speech_configuration_preserves_exact_arguments() {
        let mut screen_reader = screen_reader();
        let path = temporary_lua_file(
            "speech-config",
            r#"
                assert(lector.o.speech.server == "native")
                lector.o.speech.server = {
                    program = "/path with spaces/speech-server",
                    args = {"one argument", "'literal quotes'", "$(not a shell)"},
                }
                local configured = lector.o.speech.server
                assert(configured.program == "/path with spaces/speech-server")
                assert(#configured.args == 3)
                assert(configured.args[1] == "one argument")
                assert(configured.args[2] == "'literal quotes'")
                assert(configured.args[3] == "$(not a shell)")
            "#,
        );

        setup(path.clone(), true, &mut screen_reader, |sr| {
            assert_eq!(
                sr.speech_server_spec(),
                &SpeechServerSpec::Process {
                    program: "/path with spaces/speech-server".to_string(),
                    args: vec![
                        "one argument".to_string(),
                        "'literal quotes'".to_string(),
                        "$(not a shell)".to_string(),
                    ],
                }
            );
            assert_eq!(sr.take_speech_reconfiguration(), None);
            Ok(())
        })
        .unwrap();

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn disabled_configuration_keeps_startup_defaults() {
        let mut screen_reader = screen_reader();
        let path = temporary_lua_file(
            "disabled-config",
            r#"lector.o.speech.server = { program = "/must-not-run" }"#,
        );

        setup(path.clone(), false, &mut screen_reader, |sr| {
            assert_eq!(sr.speech_server_spec(), &SpeechServerSpec::Native);
            assert!(!sr.has_on_startup_hook());
            Ok(())
        })
        .unwrap();

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn config_directory_is_exposed_and_supplies_relative_modules() {
        let directory = tempfile::tempdir().unwrap();
        let config_dir = std::path::absolute(directory.path()).unwrap();
        let init = config_dir.join("init.lua");
        fs::write(config_dir.join("pager.lua"), "return { enabled = true }").unwrap();
        fs::create_dir_all(config_dir.join("tools/pager")).unwrap();
        fs::write(
            config_dir.join("tools/pager/init.lua"),
            "return { nested = true }",
        )
        .unwrap();
        fs::write(
            &init,
            format!(
                r#"
                    assert(lector.config_dir == {:?})
                    local pager = require("pager")
                    assert(require("pager") == pager)
                    local nested = require("tools.pager")
                    local writable = pcall(function()
                        lector.config_dir = "different"
                    end)
                    assert(writable == false)
                    lector.o.help_mode = pager.enabled and nested.nested
                "#,
                config_dir.to_string_lossy().as_ref(),
            ),
        )
        .unwrap();

        let mut screen_reader = screen_reader();
        setup(init, true, &mut screen_reader, |sr| {
            assert_eq!(sr.config_dir(), Some(config_dir.as_path()));
            assert!(sr.help_mode());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn repl_requires_explicit_nonblocking_speech_reconfiguration() {
        let mut screen_reader = screen_reader();
        let lua = Lua::new();
        let screen_reader_ptr = Rc::new(RefCell::new(&mut screen_reader as *mut ScreenReader));
        setup_repl(&lua, screen_reader_ptr).unwrap();

        lua.load(
            r#"
                local assigned, message = pcall(function()
                    lector.o.speech.server = {program = "/ignored"}
                end)
                assert(assigned == false)
                assert(string.find(tostring(message), "startup%-only") ~= nil)
                assert(lector.o.speech.server == "native")

                lector.api.set_speech({program = "/first", args = {"first arg"}})
                lector.api.set_speech({program = "/second", args = {"second arg"}})
                -- A request is transactional: the getter reports the active
                -- server until the core commits a successful replacement.
                assert(lector.o.speech.server == "native")
            "#,
        )
        .exec()
        .unwrap();

        let requested = screen_reader.take_speech_reconfiguration().unwrap();
        assert_eq!(
            requested,
            SpeechServerSpec::Process {
                program: "/second".to_string(),
                args: vec!["second arg".to_string()],
            }
        );
        screen_reader.commit_speech_reconfiguration(requested);
        lua.load(
            r#"
                local active = lector.o.speech.server
                assert(active.program == "/second")
                assert(#active.args == 1 and active.args[1] == "second arg")
            "#,
        )
        .exec()
        .unwrap();
    }

    #[test]
    fn speech_namespace_exposes_negotiated_settings_current_voice_and_voice_list() {
        let rates = Rc::new(RefCell::new(Vec::new()));
        let pitches = Rc::new(RefCell::new(Vec::new()));
        let volumes = Rc::new(RefCell::new(Vec::new()));
        let selected = Rc::new(RefCell::new(Vec::new()));
        let current = voice("voice-a", "Voice A");
        let driver = SpeechOptionsDriver {
            state: OptionState {
                rate: Some(1.25),
                rate_status: CapabilityStatus::Supported,
                pitch: Some(0.75),
                pitch_status: CapabilityStatus::Supported,
                volume: Some(0.5),
                volume_status: CapabilityStatus::Supported,
                voice: Some(current.clone()),
                voice_status: CapabilityStatus::Supported,
                voice_selection_status: CapabilityStatus::Supported,
                voices: Some(vec![current, voice("voice-b", "Voice B")]),
            },
            rates: Rc::clone(&rates),
            pitches: Rc::clone(&pitches),
            volumes: Rc::clone(&volumes),
            voices: Rc::clone(&selected),
        };
        let speech = speech::Speech::new(Box::new(driver));
        let mut screen_reader = ScreenReader::new(speech);
        let lua = Lua::new();
        let screen_reader_ptr = Rc::new(RefCell::new(&mut screen_reader as *mut ScreenReader));
        setup_repl(&lua, screen_reader_ptr).unwrap();

        lua.load(
            r#"
                assert(lector.o.speech.rate == 1.25)
                assert(lector.o.speech.pitch == 0.75)
                assert(lector.o.speech.volume == 0.5)
                assert(lector.o.speech.voice == "voice-a")
                local voices = lector.o.speech.voices
                assert(#voices == 2)
                assert(voices[1].id == "voice-a")
                assert(voices[1].name == "Voice A")
                assert(voices[1].language == "en-US")
                assert(voices[1].gender == "neutral")
                assert(voices[2].id == "voice-b")
                lector.o.speech.rate = 1.5
                lector.o.speech.pitch = 0.8
                lector.o.speech.volume = 0.6
                lector.o.speech.voice = "voice-b"
                local ok, message = pcall(function()
                    lector.o.speech.voice = "missing-voice"
                end)
                assert(ok == false)
                assert(string.find(tostring(message), "voice ID is not available", 1, true))
            "#,
        )
        .exec()
        .unwrap();

        assert_eq!(rates.borrow().as_slice(), [1.5]);
        assert_eq!(pitches.borrow().as_slice(), [0.8]);
        assert_eq!(volumes.borrow().as_slice(), [0.6]);
        assert_eq!(selected.borrow().as_slice(), ["voice-b"]);
    }

    #[test]
    fn speech_namespace_configures_paragraph_pause_in_integer_milliseconds() {
        let mut screen_reader = screen_reader();
        let lua = Lua::new();
        let screen_reader_ptr = Rc::new(RefCell::new(&mut screen_reader as *mut ScreenReader));
        setup_repl(&lua, screen_reader_ptr).unwrap();

        lua.load(
            r#"
                assert(lector.o.speech.paragraph_pause_ms == 100)
                lector.o.speech.paragraph_pause_ms = 0
                assert(lector.o.speech.paragraph_pause_ms == 0)
                lector.o.speech.paragraph_pause_ms = 37
                assert(lector.o.speech.paragraph_pause_ms == 37)

                for _, invalid in ipairs({-1, 1.5, "100"}) do
                    local ok, message = pcall(function()
                        lector.o.speech.paragraph_pause_ms = invalid
                    end)
                    assert(ok == false)
                    assert(string.find(tostring(message), "non-negative integer", 1, true))
                    assert(lector.o.speech.paragraph_pause_ms == 37)
                end
            "#,
        )
        .exec()
        .unwrap();

        assert_eq!(screen_reader.speech().paragraph_pause_ms(), 37);
    }

    #[test]
    fn invalid_config_option_stops_the_chunk_without_rolling_back_prior_assignments() {
        let driver = SpeechOptionsDriver {
            state: OptionState {
                rate_status: CapabilityStatus::Unsupported,
                voice_status: CapabilityStatus::Unsupported,
                voice_selection_status: CapabilityStatus::Unsupported,
                ..OptionState::default()
            },
            rates: Rc::new(RefCell::new(Vec::new())),
            pitches: Rc::new(RefCell::new(Vec::new())),
            volumes: Rc::new(RefCell::new(Vec::new())),
            voices: Rc::new(RefCell::new(Vec::new())),
        };
        let speech = speech::Speech::new(Box::new(driver));
        let mut screen_reader = ScreenReader::new(speech);
        let path = temporary_lua_file(
            "invalid-option",
            r#"
                lector.o.auto_read = false
                lector.o.speech.voice = "unavailable"
                lector.o.help_mode = true
            "#,
        );
        let after_called = Cell::new(false);

        let error = setup(path.clone(), true, &mut screen_reader, |_| {
            after_called.set(true);
            Ok(())
        })
        .expect_err("invalid option must fail configuration");

        assert!(error.to_string().contains("speech.voice is unavailable"));
        assert!(!screen_reader.auto_read_enabled());
        assert!(!screen_reader.help_mode());
        assert!(!after_called.get());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn unsupported_speech_options_are_nil_and_assignments_raise_errors() {
        let driver = SpeechOptionsDriver {
            state: OptionState {
                rate_status: CapabilityStatus::Unsupported,
                pitch_status: CapabilityStatus::Unsupported,
                volume_status: CapabilityStatus::Unsupported,
                voice_status: CapabilityStatus::Unsupported,
                voice_selection_status: CapabilityStatus::Unsupported,
                ..OptionState::default()
            },
            rates: Rc::new(RefCell::new(Vec::new())),
            pitches: Rc::new(RefCell::new(Vec::new())),
            volumes: Rc::new(RefCell::new(Vec::new())),
            voices: Rc::new(RefCell::new(Vec::new())),
        };
        let speech = speech::Speech::new(Box::new(driver));
        let mut screen_reader = ScreenReader::new(speech);
        let lua = Lua::new();
        let screen_reader_ptr = Rc::new(RefCell::new(&mut screen_reader as *mut ScreenReader));
        setup_repl(&lua, screen_reader_ptr).unwrap();

        lua.load(
            r#"
                assert(lector.o.speech.rate == nil)
                assert(lector.o.speech.pitch == nil)
                assert(lector.o.speech.volume == nil)
                assert(lector.o.speech.voice == nil)
                assert(lector.o.speech.voices == nil)
                local rate_ok, rate_error = pcall(function()
                    lector.o.speech.rate = 2.0
                end)
                assert(rate_ok == false)
                assert(string.find(tostring(rate_error), "speech.rate is unavailable", 1, true))
                local pitch_ok, pitch_error = pcall(function()
                    lector.o.speech.pitch = 1.0
                end)
                assert(pitch_ok == false)
                assert(string.find(tostring(pitch_error), "speech.pitch is unavailable", 1, true))
                local volume_ok, volume_error = pcall(function()
                    lector.o.speech.volume = 1.0
                end)
                assert(volume_ok == false)
                assert(string.find(tostring(volume_error), "speech.volume is unavailable", 1, true))
                local voice_ok, voice_error = pcall(function()
                    lector.o.speech.voice = "unavailable"
                end)
                assert(voice_ok == false)
                assert(string.find(tostring(voice_error), "speech.voice is unavailable", 1, true))
            "#,
        )
        .exec()
        .unwrap();
    }

    #[test]
    fn lua_speak_returns_opaque_string_ids_and_nil_for_suppressed_text() {
        let mut screen_reader = screen_reader();
        let lua = Lua::new();
        let screen_reader_ptr = Rc::new(RefCell::new(&mut screen_reader as *mut ScreenReader));
        setup_repl(&lua, screen_reader_ptr).unwrap();

        lua.load(
            r#"
                local first = lector.api.speak("first", false)
                local second = lector.api.speak("second", false)
                assert(type(first) == "string")
                assert(type(second) == "string")
                assert(first ~= second)
                assert(lector.api.speak("", false) == nil)
            "#,
        )
        .exec()
        .unwrap();
    }

    #[test]
    fn speech_server_specs_are_strictly_validated_before_queueing() {
        let mut screen_reader = screen_reader();
        let lua = Lua::new();
        let screen_reader_ptr = Rc::new(RefCell::new(&mut screen_reader as *mut ScreenReader));
        setup_repl(&lua, screen_reader_ptr).unwrap();

        lua.load(
            r#"
                local invalid = {
                    "not-native",
                    {},
                    {program = ""},
                    {program = 42},
                    {program = "/server", unknown = true},
                    {program = "/server", args = "--not-an-array"},
                    {program = "/server", args = {[2] = "gap"}},
                    {program = "/server", args = {"valid", 42}},
                    {program = "/server", args = {named = "value"}},
                    {program = "bad\0program"},
                    {program = "/server", args = {"bad\0arg"}},
                }
                for _, spec in ipairs(invalid) do
                    assert(pcall(lector.api.set_speech, spec) == false)
                end
            "#,
        )
        .exec()
        .unwrap();

        assert_eq!(screen_reader.take_speech_reconfiguration(), None);
    }

    #[test]
    fn configuration_and_hooks_round_trip_through_the_lua_api() {
        let output = Rc::new(RefCell::new(Vec::new()));
        let speech = speech::Speech::new(Box::new(RecordingDriver(Rc::clone(&output))));
        let mut screen_reader = ScreenReader::new(speech);
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("lector-lua-{unique}.lua"));
        fs::write(
            &path,
            r#"
                assert(lector.o.suppress_key_echo == false)
                assert(lector.o.report_indentation == true)
                assert(lector.o.tmux_bells == "audible")
                assert(lector.o.clipboard.default_register == '"')
                assert(lector.o.clipboard.system_provider == "native")
                lector.o.auto_read = false
                lector.o.suppress_key_echo = true
                lector.o.report_indentation = false
                lector.o.tmux_bells = "spoken"
                lector.o.clipboard.default_register = "+"
                lector.o.clipboard.system_provider = "osc52"
                lector.o.symbol_level = "all"
                lector.symbols = { ["?"] = {"query", "all", "never", false} }
                lector.bindings["M-z"] = "lector.cancel_speaking"
                lector.bindings["M-v"] = {
                    "custom binding",
                    function() lector.o.review_follows_screen_cursor = false end,
                }
                lector.hooks.on_startup = function(_) lector.o.help_mode = true end
                lector.hooks.on_shutdown = function(_) lector.o.auto_read = true end
                lector.hooks.on_screen_update = function(_) lector.o.auto_read = false end
                lector.hooks.on_live_read = function(text, _) return text .. " hooked" end
                lector.hooks.on_review_cursor_move = function(_) lector.o.highlight_tracking = true end
                lector.hooks.on_mode_change = function(_, _) lector.o.stop_speech_on_focus_loss = false end
                lector.hooks.on_table_mode_enter = function(_) lector.o.help_mode = true end
                lector.hooks.on_table_mode_exit = function() lector.o.help_mode = false end
                lector.hooks.on_clipboard_change = function(_, _) lector.o.auto_read = true end
                lector.hooks.on_speech_start = function(_, _) lector.o.help_mode = false end
                lector.hooks.on_speech_end = function(_, _) lector.o.auto_read = false end
                lector.hooks.on_key_unhandled = function(key, mode)
                    return key == "q" and mode == "table"
                end
                lector.clipboard.internal.text = "older"
                lector.clipboard.internal.text = "newer"
                assert(lector.clipboard.internal.text == "newer")
                local entries = lector.clipboard.internal.entries
                assert(#entries == 2 and entries[1] == "newer" and entries[2] == "older")
                assert(lector.clipboard.internal.index == 1)
                lector.clipboard.internal.index = 2
                assert(lector.clipboard.internal.text == "older")
                lector.clipboard.system.text = "remote"
                local readable = pcall(function() return lector.clipboard.system.text end)
                assert(readable == false)
                lector.clipboard.system.text = nil
                lector.o.auto_read = false
            "#,
        )
        .unwrap();

        setup(path.clone(), true, &mut screen_reader, |sr| {
            assert!(sr.has_on_startup_hook());
            assert!(
                !sr.help_mode(),
                "on_startup must wait for the application's ready boundary"
            );
            sr.hook_on_startup(path.to_str().unwrap())?;
            assert!(sr.help_mode());
            assert!(!sr.auto_read_enabled());
            assert!(sr.suppress_key_echo());
            assert!(!sr.indentation_reporting_enabled());
            assert_eq!(sr.tmux_bell_mode().to_string(), "spoken");
            assert_eq!(sr.clipboard_default_register().to_string(), "+");
            assert_eq!(sr.system_clipboard_provider().to_string(), "osc52");
            assert_eq!(sr.clipboard_text(), Some("older"));
            assert_eq!(
                sr.take_terminal_clipboard_writes(),
                [
                    b"\x1b]52;c;cmVtb3Rl\x1b\\".to_vec(),
                    b"\x1b]52;c;\x1b\\".to_vec(),
                ]
            );
            assert!(sr.speech().symbol_level() == Level::All);
            assert!(matches!(
                sr.key_bindings().binding_for_mode(InputMode::Normal, "M-z"),
                Some(Binding::Builtin(crate::commands::Action::CancelSpeaking))
            ));

            let binding = sr
                .key_bindings()
                .binding_for_mode(InputMode::Normal, "M-v")
                .unwrap();
            let Binding::Lua(binding) = binding else {
                panic!("expected Lua binding");
            };
            binding.call()?;
            assert!(!sr.review_follows_screen_cursor());

            sr.speak("?", false)?;
            assert!(!sr.help_mode());
            assert!(!sr.auto_read_enabled());
            assert_eq!(
                sr.hook_on_live_read("value", 2, false)?,
                Some("value hooked".to_string())
            );
            assert!(sr.hook_on_key_unhandled(Some("q"), InputMode::Table)?);
            assert!(!sr.hook_on_key_unhandled(Some("x"), InputMode::Normal)?);

            sr.hook_on_review_cursor_move((0, 0), (0, 1))?;
            assert!(sr.highlight_tracking_enabled());
            sr.hook_on_mode_change(InputMode::Normal, InputMode::Table)?;
            assert!(!sr.stop_speech_on_focus_loss());

            let table_state = TableState::new(
                TableModel::new(
                    0,
                    1,
                    vec![Column::new(0, 1), Column::new(2, 3)],
                    Some(0),
                    None,
                ),
                0,
            );
            sr.hook_on_table_mode_enter(&table_state)?;
            assert!(sr.help_mode());
            sr.hook_on_table_mode_exit()?;
            assert!(!sr.help_mode());

            sr.push_clipboard("entry".to_string())?;
            assert!(sr.auto_read_enabled());
            let mut view = View::new(2, 8);
            view.process_changes(b"changed");
            sr.hook_on_screen_update(&view, false)?;
            assert!(!sr.auto_read_enabled());
            Ok(())
        })
        .unwrap();

        assert!(screen_reader.auto_read_enabled());
        assert_eq!(output.borrow().as_slice(), [" query "]);
        fs::remove_file(path).unwrap();
    }
}
