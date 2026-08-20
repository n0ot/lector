use self::ext::LuaResultExt;
use crate::screen_reader::ScreenReader;
use anyhow::{Context as AnyhowContext, anyhow};
use mlua::{Error, Function, Lua, LuaOptions, Result, StdLib, Value};
use std::{cell::RefCell, fs::File, io::Read, path::PathBuf, rc::Rc};

mod ext;
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
    let lua = Rc::new(Lua::new_with(
        StdLib::ALL_SAFE | StdLib::JIT,
        LuaOptions::default(),
    )?);
    screen_reader.set_lua_context(Rc::clone(&lua));
    let sr_ptr = Rc::new(RefCell::new(screen_reader as *mut ScreenReader));
    install_api_static(&lua, Rc::clone(&sr_ptr))?;
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
            let _ = screen_reader.hook_on_error(&err.to_string(), "runtime");
            let _ = screen_reader.hook_on_shutdown("error");
            Err(Error::external(err))
        }
    }
}

pub fn setup_repl(lua: &Lua, sr_ptr: Rc<RefCell<*mut ScreenReader>>) -> Result<()> {
    install_api_static(lua, Rc::clone(&sr_ptr))?;
    meta::setup_static(lua, sr_ptr)?;
    Ok(())
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
            sr.speak(&text, interrupt).to_lua_result()
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
        speech::{self, SpeechServerSpec, symbols::Level},
        table::{Column, TableModel, TableState},
        view::View,
    };
    use mlua::Lua;
    use std::{
        cell::RefCell,
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

    #[test]
    fn startup_speech_configuration_preserves_exact_arguments() {
        let mut screen_reader = screen_reader();
        let path = temporary_lua_file(
            "speech-config",
            r#"
                assert(lector.o.speech == "native")
                lector.o.speech = {
                    program = "/path with spaces/speech-server",
                    args = {"one argument", "'literal quotes'", "$(not a shell)"},
                }
                local configured = lector.o.speech
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
            r#"lector.o.speech = { program = "/must-not-run" }"#,
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
    fn repl_requires_explicit_nonblocking_speech_reconfiguration() {
        let mut screen_reader = screen_reader();
        let lua = Lua::new();
        let screen_reader_ptr = Rc::new(RefCell::new(&mut screen_reader as *mut ScreenReader));
        setup_repl(&lua, screen_reader_ptr).unwrap();

        lua.load(
            r#"
                local assigned, message = pcall(function()
                    lector.o.speech = {program = "/ignored"}
                end)
                assert(assigned == false)
                assert(string.find(tostring(message), "startup%-only") ~= nil)
                assert(lector.o.speech == "native")

                lector.api.set_speech({program = "/first", args = {"first arg"}})
                lector.api.set_speech({program = "/second", args = {"second arg"}})
                -- A request is transactional: the getter reports the active
                -- server until the core commits a successful replacement.
                assert(lector.o.speech == "native")
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
                local active = lector.o.speech
                assert(active.program == "/second")
                assert(#active.args == 1 and active.args[1] == "second arg")
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
                assert(lector.o.tmux_bells == "audible")
                assert(lector.o.clipboard.default_register == '"')
                assert(lector.o.clipboard.system_provider == "native")
                lector.o.auto_read = false
                lector.o.suppress_key_echo = true
                lector.o.tmux_bells = "spoken"
                lector.o.clipboard.default_register = "+"
                lector.o.clipboard.system_provider = "osc52"
                lector.o.symbol_level = "all"
                lector.symbols = { ["?"] = {"query", "all", "never", false} }
                lector.bindings["M-z"] = "lector.stop_speaking"
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
                Some(Binding::Builtin(crate::commands::Action::StopSpeaking))
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
