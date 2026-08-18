use lector::{
    app::{App, Clock},
    screen_reader::ScreenReader,
    speech,
    tmux_model::TmuxTopology,
    tmux_prefix::{
        BindingAction, classify_binding, command_may_change_key_configuration,
        scope_select_window_command, tmux_key_name,
    },
    views,
};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::{
    cell::{Cell, RefCell},
    io::{Read, Write},
    path::PathBuf,
    rc::Rc,
    sync::mpsc,
    thread,
    time::{Duration, SystemTime},
};
use terminput::{KeyCode, KeyEvent, KeyModifiers};

const ONE_PANE: &str = "b25f,80x24,0,0,21";

// Representative machine-format records supplied by tmux at runtime. They
// cover a custom prefix and key table without reading a configuration file.
const USER_PREFIX_FIXTURE: &[&[u8]] = &[
    b"O\tprefix\tC-a",
    b"O\tprefix2\tNone",
    b"O\tkey-table\troot",
    b"O\trepeat-time\t500",
    b"B\t1\t0\tselect-window -t :=1",
    b"B\tn\t0\tnext-window",
    b"B\tp\t0\tprevious-window",
    b"B\tl\t0\tlast-window",
    b"B\tLeft\t1\tselect-pane -L",
    b"B\tRight\t1\tselect-pane -R",
    b"B\to\t1\tselect-pane -t :.+",
    b"B\t;\t1\tlast-pane",
    b"B\t(\t0\tswitch-client -p",
    b"B\t)\t0\tswitch-client -n",
    b"B\tc\t0\tnew-window",
    b"B\tC-c\t0\tnew-window -c \"#{pane_current_path}\"",
    b"B\td\t0\tdetach-client",
    b"B\t\"\t0\tsplit-window",
    b"B\t%\t0\tsplit-window -h",
    b"B\tx\t0\tconfirm-before -p \"kill-pane #P? (y/n)\" kill-pane",
    b"B\t&\t0\tconfirm-before -p \"kill-window #W? (y/n)\" kill-window",
    b"B\tC-a\t0\tsend-prefix",
    b"B\ts\t0\tchoose-tree -Zs",
    b"B\tw\t0\tchoose-tree -Zw",
    b"B\t:\t0\tcommand-prompt",
    b"B\t/\t0\tset-option -g key-table root \\; set-option -g status-right \"#W.#P#{?client_prefix, PR,}\"",
    b"B\t\\\t0\tset-option -g key-table passthrough \\; set-option -g status-right \"#W.#P#{?client_prefix, PR,} PASS\"",
    b"B\tZ\t0\tdisplay-message -p -F \"#{session_name}:#{window_index}:#{pane_index}\"",
    b"B\tr\t0\tsource-file ~/.tmux.conf \\; display-message Reloaded!",
    b"B\tpassthrough\tq\t0\tdisplay-message passthrough-key",
];

fn topology_with_user_prefix() -> TmuxTopology {
    let mut records = vec![
        b"S\t$1\twork".to_vec(),
        format!("W\t$1\t@10\t1\t1\t{ONE_PANE}\t{ONE_PANE}\t*\tinput").into_bytes(),
        b"P\t@10\t%21\t1\t1\t0\t0\t80\t24\t0\t0\t0\t1\t0\t0\t0\t0\tright".to_vec(),
        b"A\t$1".to_vec(),
        b"O\tbase-index\t1".to_vec(),
        b"O\tpane-base-index\t1".to_vec(),
        b"C\tclient_name\ttest".to_vec(),
    ];
    records.extend(USER_PREFIX_FIXTURE.iter().map(|line| line.to_vec()));
    let mut topology = TmuxTopology::new(1);
    topology.replace_inventory(&records).unwrap();
    topology
}

fn inventory_groups() -> Vec<Vec<Vec<u8>>> {
    vec![
        vec![b"S\t$1\twork".to_vec()],
        vec![format!("W\t$1\t@10\t1\t1\t{ONE_PANE}\t{ONE_PANE}\t*\tinput").into_bytes()],
        vec![b"P\t@10\t%21\t1\t1\t0\t0\t80\t24\t0\t0\t0\t1\t0\t0\t0\t0\tright".to_vec()],
        vec![b"A\t$1".to_vec()],
        vec![b"O\tbase-index\t1".to_vec()],
        vec![b"O\tpane-base-index\t1".to_vec()],
        vec![b"C\tclient_name\ttest".to_vec()],
        vec![b"O\tprefix\tC-a".to_vec()],
        vec![b"O\tprefix2\tNone".to_vec()],
        vec![b"O\tkey-table\troot".to_vec()],
        vec![b"O\trepeat-time\t500".to_vec()],
        USER_PREFIX_FIXTURE[4..]
            .iter()
            .map(|line| line.to_vec())
            .collect(),
    ]
}

#[test]
fn user_prefix_fixture_preserves_options_repeatability_commands_and_quotes() {
    let topology = topology_with_user_prefix();
    assert_eq!(topology.option("prefix"), Some("C-a"));
    assert_eq!(topology.option("prefix2"), Some("None"));
    assert_eq!(topology.option("key-table"), Some("root"));
    assert_eq!(topology.option("repeat-time"), Some("500"));
    assert_eq!(topology.bindings().len(), 25);
    assert!(topology.binding("Left").unwrap().repeatable);
    assert_eq!(
        topology.binding("Z").unwrap().command,
        "display-message -p -F \"#{session_name}:#{window_index}:#{pane_index}\""
    );
    assert!(
        topology
            .binding("\\")
            .unwrap()
            .command
            .contains("key-table passthrough")
    );

    let mut malformed = USER_PREFIX_FIXTURE
        .iter()
        .map(|line| line.to_vec())
        .collect::<Vec<_>>();
    malformed.push(b"B\tbad\t2\tnew-window".to_vec());
    assert!(TmuxTopology::new(2).replace_inventory(&malformed).is_err());
    assert!(
        TmuxTopology::new(3)
            .replace_inventory(&[b"B\tn\t0\tnew-window\0kill-server".to_vec()])
            .is_err()
    );
}

#[test]
fn binding_classifier_recognizes_accessible_and_safe_passthrough_actions() {
    assert!(matches!(
        classify_binding("confirm-before -p \"kill-pane #P? (y/n)\" kill-pane").unwrap(),
        BindingAction::Confirm { command, .. } if command == "kill-pane"
    ));
    assert_eq!(
        classify_binding("send-prefix").unwrap(),
        BindingAction::SendPrefix
    );
    assert_eq!(
        classify_binding("detach-client").unwrap(),
        BindingAction::Detach
    );
    assert!(matches!(
        classify_binding("choose-tree -Zs").unwrap(),
        BindingAction::ChooseSession
    ));
    assert!(matches!(
        classify_binding("choose-tree -Zw").unwrap(),
        BindingAction::ChooseWindow
    ));
    assert!(matches!(
        classify_binding("command-prompt").unwrap(),
        BindingAction::CommandPrompt
    ));
    assert!(matches!(
        classify_binding(
            "set-option -g key-table passthrough \\; set-option -g status-right \"PASS\""
        )
        .unwrap(),
        BindingAction::SetKeyTable {
            ref table,
            persistent: true,
            ..
        } if table == "passthrough"
    ));
    assert!(matches!(
        classify_binding(
            "set-option -g key-table root \\; set-option -g status-right \"normal\""
        )
        .unwrap(),
        BindingAction::SetKeyTable {
            ref table,
            persistent: true,
            ..
        } if table == "root"
    ));
    assert!(matches!(
        classify_binding("select-window -t :=2").unwrap(),
        BindingAction::Execute(ref command) if command == "select-window -t :=2"
    ));
    assert!(classify_binding("new-window\nkill-server").is_err());
    assert!(classify_binding("display-message \0bad").is_err());
    assert!(command_may_change_key_configuration(
        "source-file ~/.tmux.conf \\; display-message Reloaded!"
    ));
    assert!(command_may_change_key_configuration(
        "bind-key -T prefix n next-window"
    ));
    assert!(command_may_change_key_configuration("set -g prefix C-a"));
    assert!(!command_may_change_key_configuration("next-window"));
}

#[test]
fn numeric_window_bindings_are_scoped_to_the_attached_session_by_stable_id() {
    let layout = "b25f,80x24,0,0,110";
    let records = [
        b"S\t$1\tdev".to_vec(),
        format!("W\t$1\t@110\t10\t1\t{layout}\t{layout}\t*\tcodex").into_bytes(),
        b"P\t@110\t%110\t1\t1\t0\t0\t80\t24\t0\t0\t0\t1\t0\t0\t0\t0\tcodex".to_vec(),
        b"A\t$1".to_vec(),
    ];
    let mut topology = TmuxTopology::new(1);
    topology.replace_inventory(&records).unwrap();

    assert_eq!(
        scope_select_window_command(&topology, "select-window -t 10").as_deref(),
        Some("select-window -t @110")
    );
    assert_eq!(
        scope_select_window_command(&topology, "select-window -t :=10").as_deref(),
        Some("select-window -t @110")
    );
    assert_eq!(
        scope_select_window_command(&topology, "select-window -t 9"),
        None
    );
}

#[test]
fn physical_keys_map_to_tmux_names_without_conflating_lector_names() {
    assert_eq!(
        tmux_key_name(KeyEvent::new(KeyCode::Char('a')).modifiers(KeyModifiers::CTRL)).as_deref(),
        Some("C-a")
    );
    assert_eq!(
        tmux_key_name(KeyEvent::new(KeyCode::Char(' '))).as_deref(),
        Some("Space")
    );
    assert_eq!(
        tmux_key_name(KeyEvent::new(KeyCode::Delete)).as_deref(),
        Some("DC")
    );
    assert_eq!(
        tmux_key_name(KeyEvent::new(KeyCode::PageUp)).as_deref(),
        Some("PPage")
    );
    assert_eq!(
        tmux_key_name(KeyEvent::new(KeyCode::Left).modifiers(KeyModifiers::ALT)).as_deref(),
        Some("M-Left")
    );
}

#[derive(Clone, Default)]
struct TestClock(Rc<Cell<u128>>);

impl TestClock {
    fn advance(&self, milliseconds: u128) {
        self.0.set(self.0.get().saturating_add(milliseconds));
    }
}

impl Clock for TestClock {
    fn now_ms(&self) -> u128 {
        self.0.get()
    }
}

#[derive(Clone, Default)]
struct Recorder(Rc<RefCell<Vec<String>>>);

impl speech::Driver for Recorder {
    fn speak(&mut self, text: &str, _interrupt: bool) -> anyhow::Result<()> {
        self.0.borrow_mut().push(text.to_owned());
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

fn reply(serial: usize, lines: &[Vec<u8>], success: bool) -> Vec<u8> {
    let mut bytes = format!("%begin {serial} {serial} 0\n").into_bytes();
    for line in lines {
        bytes.extend_from_slice(line);
        bytes.push(b'\n');
    }
    bytes.extend_from_slice(
        format!(
            "%{} {serial} {serial} 0\n",
            if success { "end" } else { "error" }
        )
        .as_bytes(),
    );
    bytes
}

fn ready_app() -> (App, ScreenReader, Recorder, TestClock, Vec<u8>) {
    ready_app_with_bootstrap(None, &[b"ready".to_vec()])
}

fn ready_app_with_bootstrap(
    prebootstrap_output: Option<&[u8]>,
    capture: &[Vec<u8>],
) -> (App, ScreenReader, Recorder, TestClock, Vec<u8>) {
    let clock = TestClock::default();
    let recorder = Recorder::default();
    let stack = views::ViewStack::new(Box::new(views::PtyView::new(24, 80)));
    let mut app = App::new_with_clock(stack, Box::new(clock.clone())).unwrap();
    let mut sr = ScreenReader::new(speech::Speech::new(Box::new(recorder.clone())));
    let mut physical = Vec::new();
    app.handle_pty(
        &mut sr,
        b"\x1bP1000p%begin 1 1 0\n%end 1 1 0\n",
        &mut physical,
    )
    .unwrap();
    let mut commands = Vec::new();
    app.handle_tick(&mut sr, &mut commands, &mut physical)
        .unwrap();
    assert_eq!(
        commands,
        [
            lector::app::TMUX_FLOW_CONTROL_COMMAND,
            lector::app::TMUX_FLOW_CONTROL_VERIFY_COMMAND,
            lector::tmux_model::INVENTORY_COMMAND.as_bytes(),
        ]
        .concat()
    );
    app.handle_pty(&mut sr, &reply(2, &[], true), &mut physical)
        .unwrap();
    app.handle_pty(
        &mut sr,
        &reply(3, &[b"attached,control-mode,pause-after=1".to_vec()], true),
        &mut physical,
    )
    .unwrap();

    let groups = inventory_groups();
    assert_eq!(groups.len(), lector::tmux_model::INVENTORY_REPLY_COUNT);
    for (index, group) in groups.iter().enumerate() {
        app.handle_pty(&mut sr, &reply(index + 4, group, true), &mut physical)
            .unwrap();
    }
    commands.clear();
    app.handle_tick(&mut sr, &mut commands, &mut physical)
        .unwrap();
    assert_eq!(commands, b"capture-pane -p -e -F -J -S - -t %21\n");
    if let Some(output) = prebootstrap_output {
        let mut notification = b"%output %21 ".to_vec();
        notification.extend_from_slice(output);
        notification.push(b'\n');
        app.handle_pty(&mut sr, &notification, &mut physical)
            .unwrap();
    }
    app.handle_pty(&mut sr, &reply(20, capture, true), &mut physical)
        .unwrap();
    (app, sr, recorder, clock, physical)
}

fn input(app: &mut App, sr: &mut ScreenReader, physical: &mut Vec<u8>, bytes: &[u8]) {
    app.handle_stdin(sr, bytes, &mut Vec::new(), physical)
        .unwrap();
}

fn tick(app: &mut App, sr: &mut ScreenReader, physical: &mut Vec<u8>) -> Vec<u8> {
    let mut commands = Vec::new();
    app.handle_tick(sr, &mut commands, physical).unwrap();
    commands
}

#[test]
fn terminal_mode_keeps_ctrl_a_verbatim_but_tmux_mode_owns_the_configured_prefix() {
    let stack = views::ViewStack::new(Box::new(views::PtyView::new(3, 10)));
    let mut direct = App::new(stack).unwrap();
    let mut direct_sr = ScreenReader::new(speech::Speech::new(Box::new(Recorder::default())));
    let mut pty = Vec::new();
    direct
        .handle_stdin(&mut direct_sr, b"\x01", &mut pty, &mut Vec::new())
        .unwrap();
    assert_eq!(pty, b"\x01");

    let (mut app, mut sr, _recorder, _clock, mut physical) = ready_app();
    for byte in b"\x1b[97;5:1u" {
        input(&mut app, &mut sr, &mut physical, &[*byte]);
    }
    input(&mut app, &mut sr, &mut physical, b"\x1b[97;5:3u");
    assert!(tick(&mut app, &mut sr, &mut physical).is_empty());
    input(&mut app, &mut sr, &mut physical, b"n");
    assert_eq!(tick(&mut app, &mut sr, &mut physical), b"next-window\n");
}

#[test]
fn prefix_waits_indefinitely_but_repeat_timeout_cancel_and_unbound_keys_are_deterministic() {
    let (mut app, mut sr, recorder, clock, mut physical) = ready_app();

    input(&mut app, &mut sr, &mut physical, b"\x01\x1b[D");
    assert_eq!(tick(&mut app, &mut sr, &mut physical), b"select-pane -L\n");
    input(&mut app, &mut sr, &mut physical, b"\x1b[D");
    assert_eq!(tick(&mut app, &mut sr, &mut physical), b"select-pane -L\n");
    clock.advance(501);
    input(&mut app, &mut sr, &mut physical, b"\x1b[D");
    let raw_left = tick(&mut app, &mut sr, &mut physical);
    assert!(String::from_utf8_lossy(&raw_left).contains("1b 5b 44"));

    recorder.0.borrow_mut().clear();
    input(&mut app, &mut sr, &mut physical, b"\x01");
    assert_eq!(&*recorder.0.borrow(), &["tmux"]);
    clock.advance(60_000);
    input(&mut app, &mut sr, &mut physical, b"n");
    assert_eq!(tick(&mut app, &mut sr, &mut physical), b"next-window\n");

    input(&mut app, &mut sr, &mut physical, b"\x01\x1b[27;1u");
    input(&mut app, &mut sr, &mut physical, b"n");
    assert!(String::from_utf8_lossy(&tick(&mut app, &mut sr, &mut physical)).contains(" 6e\n"));

    input(&mut app, &mut sr, &mut physical, b"\x01\x01");
    assert!(
        String::from_utf8_lossy(&tick(&mut app, &mut sr, &mut physical))
            .contains("send-keys -H -t %21 01")
    );

    input(&mut app, &mut sr, &mut physical, b"\x01");
    clock.advance(60_000);
    input(&mut app, &mut sr, &mut physical, b"v");
    assert!(tick(&mut app, &mut sr, &mut physical).is_empty());
    assert!(
        recorder
            .0
            .borrow()
            .iter()
            .any(|message| message.contains("unbound"))
    );
}

#[test]
fn connection_bootstrap_speaks_only_the_short_entry_cue_and_ready_pane() {
    let (_app, _sr, recorder, _clock, _physical) = ready_app();
    let messages = recorder.0.borrow();
    assert_eq!(&*messages, &["tmux", "1: input", "ready"]);
    assert!(messages.iter().all(|message| {
        !message.contains("connection is active")
            && !message.contains("Waiting for tmux")
            && !message.contains("becoming ready")
    }));
}

#[test]
fn empty_initial_capture_uses_live_prompt_and_never_announces_blank_screen() {
    let (mut app, _sr, recorder, _clock, _physical) =
        ready_app_with_bootstrap(Some(b"ncarpenter:~$"), &[]);
    assert!(app.debug_active_view_contents().contains("ncarpenter:~$"));
    let messages = recorder.0.borrow();
    assert_eq!(&*messages, &["tmux", "1: input", "ncarpenter:~$"]);
    assert!(messages.iter().all(|message| message != "blank screen"));
}

#[test]
fn confirmations_and_command_failures_are_accessible() {
    let (mut app, mut sr, _recorder, _clock, mut physical) = ready_app();

    input(&mut app, &mut sr, &mut physical, b"\x01x");
    assert!(app.has_overlay());
    assert!(tick(&mut app, &mut sr, &mut physical).is_empty());
    input(&mut app, &mut sr, &mut physical, b"\r");
    assert_eq!(
        tick(&mut app, &mut sr, &mut physical),
        b"kill-pane -t %21\n"
    );
    app.handle_pty(&mut sr, &reply(80, &[], true), &mut physical)
        .unwrap();

    input(&mut app, &mut sr, &mut physical, b"\x01Z");
    assert!(
        String::from_utf8_lossy(&tick(&mut app, &mut sr, &mut physical))
            .starts_with("display-message -p -F")
    );
    app.handle_pty(
        &mut sr,
        &reply(90, &[b"bad format".to_vec()], false),
        &mut physical,
    )
    .unwrap();
    assert!(app.has_overlay());
    let contents = app.debug_active_view_contents();
    assert!(
        contents
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>()
            .contains("badformat"),
        "unexpected popup: {contents:?}"
    );
}

#[test]
fn discovered_custom_key_table_is_entered_without_local_configuration_knowledge() {
    let (mut app, mut sr, _recorder, _clock, mut physical) = ready_app();

    input(&mut app, &mut sr, &mut physical, b"\x01\\");
    let transition = tick(&mut app, &mut sr, &mut physical);
    assert!(
        String::from_utf8_lossy(&transition).starts_with("set-option -g key-table passthrough"),
        "transition={transition:?}"
    );

    // The runtime-discovered table takes effect immediately, even before the
    // server replies and Lector refreshes its transactional inventory.
    input(&mut app, &mut sr, &mut physical, b"q");
    assert_eq!(
        tick(&mut app, &mut sr, &mut physical),
        b"display-message passthrough-key\n"
    );
}

#[test]
fn everyday_discovered_bindings_execute_their_exact_tmux_commands() {
    let (mut app, mut sr, _recorder, _clock, mut physical) = ready_app();
    let cases: &[(&[u8], &[u8])] = &[
        (b"1", b"select-window -t @10\n"),
        (b"p", b"previous-window\n"),
        (b"n", b"next-window\n"),
        (b"l", b"last-window\n"),
        (b"(", b"switch-client -p\n"),
        (b")", b"switch-client -n\n"),
        (b"c", b"new-window\n"),
        (b"\x03", b"new-window -c \"#{pane_current_path}\"\n"),
        (b"d", b"detach-client\n"),
        (b"\"", b"split-window\n"),
        (b"%", b"split-window -h\n"),
        (b"o", b"select-pane -t :.+\n"),
        (b";", b"last-pane\n"),
    ];
    for (key, expected) in cases {
        input(&mut app, &mut sr, &mut physical, b"\x01");
        input(&mut app, &mut sr, &mut physical, key);
        assert_eq!(tick(&mut app, &mut sr, &mut physical), *expected);
    }
}

#[test]
fn secondary_prefix_confirmation_cancel_and_portal_navigation_clear_state() {
    let (mut app, mut sr, _recorder, _clock, mut physical) = ready_app();

    app.handle_pty(&mut sr, b"%sessions-changed\n", &mut physical)
        .unwrap();
    assert_eq!(
        tick(&mut app, &mut sr, &mut physical),
        lector::tmux_model::INVENTORY_COMMAND.as_bytes()
    );
    let mut groups = inventory_groups();
    groups[8] = vec![b"O\tprefix2\tC-b".to_vec()];
    for (index, group) in groups.iter().enumerate() {
        app.handle_pty(&mut sr, &reply(40 + index, group, true), &mut physical)
            .unwrap();
    }

    input(&mut app, &mut sr, &mut physical, b"\x02n");
    assert_eq!(tick(&mut app, &mut sr, &mut physical), b"next-window\n");

    input(&mut app, &mut sr, &mut physical, b"\x01x");
    assert!(
        app.debug_active_view_contents()
            .contains("Press Enter to confirm")
    );
    input(&mut app, &mut sr, &mut physical, b"\x1b[27;1u");
    assert!(
        !app.debug_active_view_contents()
            .contains("Press Enter to confirm")
    );
    assert!(tick(&mut app, &mut sr, &mut physical).is_empty());

    input(&mut app, &mut sr, &mut physical, b"\x01");
    assert!(app.show_tmux_gateway(1, &mut sr, &mut physical).unwrap());
    input(&mut app, &mut sr, &mut physical, b"\r");
    input(&mut app, &mut sr, &mut physical, b"n");
    let commands = tick(&mut app, &mut sr, &mut physical);
    assert!(String::from_utf8_lossy(&commands).contains("send-keys -H -t %21 6e"));
    assert!(!String::from_utf8_lossy(&commands).contains("next-window"));
}

#[test]
fn binding_reload_replaces_the_prefix_table_transactionally() {
    let (mut app, mut sr, _recorder, _clock, mut physical) = ready_app();
    input(&mut app, &mut sr, &mut physical, b"\x01r");
    assert_eq!(
        tick(&mut app, &mut sr, &mut physical),
        b"source-file ~/.tmux.conf \\; display-message Reloaded!\n"
    );
    app.handle_pty(&mut sr, &reply(55, &[], true), &mut physical)
        .unwrap();
    assert_eq!(
        tick(&mut app, &mut sr, &mut physical),
        lector::tmux_model::INVENTORY_COMMAND.as_bytes()
    );
    let mut groups = inventory_groups();
    let binding = groups[11]
        .iter_mut()
        .find(|line| line.starts_with(b"B\tn\t"))
        .expect("n binding");
    *binding = b"B\tn\t0\tprevious-window".to_vec();
    for (index, group) in groups.iter().enumerate() {
        app.handle_pty(&mut sr, &reply(60 + index, group, true), &mut physical)
            .unwrap();
    }

    input(&mut app, &mut sr, &mut physical, b"\x01n");
    assert_eq!(tick(&mut app, &mut sr, &mut physical), b"previous-window\n");
}

#[test]
fn failed_binding_reload_reports_the_error_and_refreshes_partial_changes() {
    let (mut app, mut sr, _recorder, _clock, mut physical) = ready_app();
    input(&mut app, &mut sr, &mut physical, b"\x01r");
    assert!(
        String::from_utf8_lossy(&tick(&mut app, &mut sr, &mut physical))
            .starts_with("source-file ")
    );
    app.handle_pty(
        &mut sr,
        &reply(55, &[b"unknown command on line 9".to_vec()], false),
        &mut physical,
    )
    .unwrap();

    assert!(
        app.debug_active_view_contents()
            .contains("unknown command on line 9")
    );
    assert_eq!(
        tick(&mut app, &mut sr, &mut physical),
        lector::tmux_model::INVENTORY_COMMAND.as_bytes(),
        "source-file can apply earlier lines before returning an error"
    );
}

fn write_real_commands(
    app: &mut App,
    sr: &mut ScreenReader,
    writer: &mut dyn Write,
    physical: &mut Vec<u8>,
) -> Vec<u8> {
    let commands = tick(app, sr, physical);
    if !commands.is_empty() {
        writer.write_all(&commands).unwrap();
        writer.flush().unwrap();
    }
    commands
}

fn drive_real_tmux(
    case: &str,
    app: &mut App,
    sr: &mut ScreenReader,
    receiver: &mpsc::Receiver<Vec<u8>>,
    writer: &mut dyn Write,
    physical: &mut Vec<u8>,
    mut done: impl FnMut(&mut App) -> bool,
) {
    for _ in 0..800 {
        if done(app) {
            return;
        }
        let chunk = receiver
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or_else(|error| {
                panic!(
                    "timed out in {case}: {error}; contents={:?}; topology={:?}",
                    app.debug_active_view_contents(),
                    app.debug_tmux_topology(1)
                )
            });
        app.handle_pty(sr, &chunk, physical).unwrap();
        write_real_commands(app, sr, writer, physical);
    }
    panic!("real tmux prefix fixture exceeded its bounded event count in {case}");
}

#[test]
fn real_tmux_discovers_c_a_and_executes_next_window_through_control_mode() {
    let tmux = std::process::Command::new("tmux")
        .arg("-V")
        .output()
        .expect("tmux integration tests require tmux on PATH");
    assert!(tmux.status.success(), "tmux -V failed");

    let unique = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let socket_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-tmux");
    std::fs::create_dir_all(&socket_dir).unwrap();
    let socket = socket_dir.join(format!("prefix-{}-{unique}.sock", std::process::id()));
    let session = format!("lector_prefix_{}_{unique}", std::process::id());
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut command = CommandBuilder::new("tmux");
    command.args([
        "-S",
        socket.to_str().unwrap(),
        "-f",
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/tmux-prefix.conf"
        ),
        "-CC",
        "new-session",
        "-s",
        &session,
        "/bin/sh -c 'printf FIRST; exec cat'",
    ]);
    command.env("TERM", "xterm-256color");
    let mut child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let mut writer = pair.master.take_writer().unwrap();
    let (sender, receiver) = mpsc::channel();
    let read_thread = thread::spawn(move || {
        let mut buffer = [0_u8; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    if sender.send(buffer[..count].to_vec()).is_err() {
                        break;
                    }
                }
                Err(error) if error.raw_os_error() == Some(5) => break,
                Err(error) => panic!("read real tmux prefix PTY: {error}"),
            }
        }
    });

    let stack = views::ViewStack::new(Box::new(views::PtyView::new(24, 80)));
    let clock = TestClock::default();
    let recorder = Recorder::default();
    let mut app = App::new_with_clock(stack, Box::new(clock.clone())).unwrap();
    let mut sr = ScreenReader::new(speech::Speech::new(Box::new(recorder.clone())));
    let mut physical = Vec::new();
    drive_real_tmux(
        "prefix bootstrap",
        &mut app,
        &mut sr,
        &receiver,
        writer.as_mut(),
        &mut physical,
        |app| app.debug_active_view_contents().contains("FIRST"),
    );

    writer
        .write_all(
            b"new-window -d -n second \"/bin/sh -c 'printf SECOND; exec cat'\"\n\
new-window -d -t :10 -n tenth \"/bin/sh -c 'printf TENTH; exec cat'\"\n",
        )
        .unwrap();
    writer.flush().unwrap();
    drive_real_tmux(
        "second-window discovery",
        &mut app,
        &mut sr,
        &receiver,
        writer.as_mut(),
        &mut physical,
        |app| {
            app.debug_tmux_topology(1).is_some_and(|topology| {
                topology.contains(": second") && topology.contains("index 10: tenth")
            })
        },
    );

    // Let the bootstrap capture for the newly discovered pane complete before
    // exercising input. Visiting both windows also proves the following
    // prefix action, rather than setup, caused the final transition.
    writer.write_all(b"select-window -t :second\n").unwrap();
    writer.flush().unwrap();
    drive_real_tmux(
        "second-window bootstrap",
        &mut app,
        &mut sr,
        &receiver,
        writer.as_mut(),
        &mut physical,
        |app| app.debug_active_view_contents().contains("SECOND"),
    );
    writer.write_all(b"previous-window\n").unwrap();
    writer.flush().unwrap();
    drive_real_tmux(
        "restore first window",
        &mut app,
        &mut sr,
        &receiver,
        writer.as_mut(),
        &mut physical,
        |app| app.debug_active_view_contents().contains("FIRST"),
    );

    recorder.0.borrow_mut().clear();
    input(&mut app, &mut sr, &mut physical, b"\x01");
    assert_eq!(&*recorder.0.borrow(), &["tmux"]);
    clock.advance(60_000);
    input(&mut app, &mut sr, &mut physical, b"n");
    let commands = write_real_commands(&mut app, &mut sr, writer.as_mut(), &mut physical);
    assert_eq!(commands, b"next-window\n");
    drive_real_tmux(
        "next-window binding",
        &mut app,
        &mut sr,
        &receiver,
        writer.as_mut(),
        &mut physical,
        |app| app.debug_active_view_contents().contains("SECOND"),
    );
    assert!(
        recorder
            .0
            .borrow()
            .iter()
            .any(|message| message == "1: second"),
        "the newly active window title was not announced"
    );

    recorder.0.borrow_mut().clear();
    input(&mut app, &mut sr, &mut physical, b"\x010");
    let commands = write_real_commands(&mut app, &mut sr, writer.as_mut(), &mut physical);
    assert!(
        String::from_utf8_lossy(&commands).starts_with("select-window -t @"),
        "numeric binding was not resolved to a stable window ID: {:?}",
        String::from_utf8_lossy(&commands)
    );
    drive_real_tmux(
        "window ten binding",
        &mut app,
        &mut sr,
        &receiver,
        writer.as_mut(),
        &mut physical,
        |app| app.debug_active_view_contents().contains("TENTH"),
    );
    assert!(
        recorder
            .0
            .borrow()
            .iter()
            .any(|message| message == "10: tenth"),
        "window ten was selected but its title was not announced"
    );

    writer.write_all(b"kill-server\n").unwrap();
    writer.flush().unwrap();
    let _ = child.wait().unwrap();
    read_thread.join().unwrap();
    let _ = std::fs::remove_file(&socket);
}
