use lector::{
    app::{App, Clock},
    output_scheduler::OutputSchedulerConfig,
    screen_reader::ScreenReader,
    speech,
    terminal::GhosttyEngine,
    tmux_model::{PaneId, SessionId, TmuxTopology, WindowId},
    views::{
        self, TmuxChooserTarget, TmuxChooserView, TmuxCommandView, ViewAction, ViewController,
    },
};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::{
    cell::{Cell, RefCell},
    io::{Read, Write},
    path::PathBuf,
    rc::Rc,
    sync::mpsc::{self, RecvTimeoutError},
    thread,
    time::{Duration, Instant, SystemTime},
};

const SPLIT: &str = "b25f,80x24,0,0{40x24,0,0,20,39x24,41,0,23}";
const PANE_21: &str = "b25f,80x24,0,0,21";
const PANE_22: &str = "b25f,80x24,0,0,22";

fn inventory_groups(session_two_name: &str) -> Vec<Vec<Vec<u8>>> {
    vec![
        vec![
            b"S\t$1\twork".to_vec(),
            format!("S\t$2\t{session_two_name}").into_bytes(),
        ],
        vec![
            format!("W\t$1\t@10\t1\t1\t{SPLIT}\t{SPLIT}\t*\tduplicate").into_bytes(),
            format!("W\t$1\t@11\t2\t0\t{PANE_21}\t{PANE_21}\t-\tduplicate").into_bytes(),
            format!("W\t$2\t@12\t1\t1\t{PANE_22}\t{PANE_22}\t*\tduplicate").into_bytes(),
        ],
        vec![
            b"P\t@10\t%20\t1\t1\t0\t0\t40\t24\t0\t0\t0\t1\t0\t0\t0\t0\tleft".to_vec(),
            b"P\t@10\t%23\t2\t0\t41\t0\t39\t24\t0\t0\t0\t1\t0\t0\t0\t0\tright".to_vec(),
            b"P\t@11\t%21\t1\t1\t0\t0\t80\t24\t0\t0\t0\t1\t0\t0\t0\t0\tsecond-window".to_vec(),
            b"P\t@12\t%22\t1\t1\t0\t0\t80\t24\t0\t0\t0\t1\t0\t0\t0\t0\tother-session".to_vec(),
        ],
        vec![b"A\t$1".to_vec()],
        vec![b"O\tbase-index\t1".to_vec()],
        vec![b"O\tpane-base-index\t1".to_vec()],
        vec![b"C\tclient_name\ttest".to_vec()],
        vec![b"O\tprefix\tC-a".to_vec()],
        vec![b"O\tprefix2\tNone".to_vec()],
        vec![b"O\tkey-table\troot".to_vec()],
        vec![b"O\trepeat-time\t500".to_vec()],
        vec![
            b"B\ts\t0\tchoose-tree -Zs".to_vec(),
            b"B\tw\t0\tchoose-tree -Zw".to_vec(),
            b"B\tq\t0\tdisplay-panes".to_vec(),
            b"B\t:\t0\tcommand-prompt".to_vec(),
            b"B\tZ\t0\tdisplay-message -p -F \"#{session_name}:#{window_index}:#{pane_index}\""
                .to_vec(),
        ],
    ]
}

fn topology(session_two_name: &str) -> TmuxTopology {
    let records = inventory_groups(session_two_name)
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    let mut topology = TmuxTopology::new(1);
    topology.replace_inventory(&records).unwrap();
    topology
}

fn topology_with_many_sessions() -> TmuxTopology {
    let mut groups = inventory_groups("work");
    groups[0].extend((3..=12).map(|id| format!("S\t${id}\tsession-{id}").into_bytes()));
    let records = groups.into_iter().flatten().collect::<Vec<_>>();
    let mut topology = TmuxTopology::new(1);
    topology.replace_inventory(&records).unwrap();
    topology
}

#[derive(Clone, Default)]
struct TestClock(Rc<Cell<u128>>);

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

fn ready_app() -> (App, ScreenReader, Recorder, Vec<u8>) {
    let recorder = Recorder::default();
    let stack = views::ViewStack::new(Box::new(views::PtyView::new(24, 80)));
    let mut app = App::new_with_clock(stack, Box::new(TestClock::default())).unwrap();
    let mut sr = ScreenReader::new(speech::Speech::new(Box::new(recorder.clone())));
    let mut physical = Vec::new();
    let mut commands = Vec::new();
    app.handle_pty(
        &mut sr,
        b"\x1bP1000p%begin 1 1 0\n%end 1 1 0\n",
        &mut physical,
    )
    .unwrap();
    app.handle_tick(&mut sr, &mut commands, &mut physical)
        .unwrap();
    assert_eq!(
        commands,
        [
            lector::app::TMUX_FLOW_CONTROL_COMMAND,
            lector::app::TMUX_FLOW_CONTROL_VERIFY_COMMAND,
            b"refresh-client -C 80x24\n",
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
    app.handle_pty(&mut sr, &reply(4, &[], true), &mut physical)
        .unwrap();
    for (index, group) in inventory_groups("work").iter().enumerate() {
        app.handle_pty(&mut sr, &reply(index + 5, group, true), &mut physical)
            .unwrap();
    }
    commands.clear();
    app.handle_tick(&mut sr, &mut commands, &mut physical)
        .unwrap();
    let command_text = String::from_utf8_lossy(&commands);
    for pane in [20, 21, 22, 23] {
        assert!(command_text.contains(&format!("-t %{pane}\n")));
    }
    for (serial, contents) in ["left", "right", "second-window", "other-session"]
        .into_iter()
        .enumerate()
    {
        app.handle_pty(
            &mut sr,
            &reply(30 + serial, &[contents.as_bytes().to_vec()], true),
            &mut physical,
        )
        .unwrap();
    }
    (app, sr, recorder, physical)
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

fn assert_physical_scene(
    app: &App,
    oracle: &mut GhosttyEngine,
    physical: &mut Vec<u8>,
    case: &str,
) {
    oracle
        .advance(physical)
        .unwrap_or_else(|error| panic!("parse physical output for {case}: {error}"));
    physical.clear();
    let actual = oracle.normalized_snapshot();
    let intended = app.presented_scene().clone().into_terminal_snapshot();
    assert_eq!(
        actual.contents_full(),
        intended.contents_full(),
        "physical contents diverged in {case}"
    );
    assert_eq!(actual.cursor, intended.cursor, "cursor diverged in {case}");
    assert_eq!(actual.screen, intended.screen, "screen diverged in {case}");
    assert_eq!(actual.modes, intended.modes, "modes diverged in {case}");
    assert_eq!(
        actual.title.as_deref().unwrap_or_default(),
        intended.title.as_deref().unwrap_or_default(),
        "title diverged in {case}"
    );
}

#[test]
fn chooser_search_duplicate_ids_stable_selection_empty_cancel_and_resize() {
    let initial_topology = topology("work");
    let mut chooser = TmuxChooserView::sessions(8, 50, 1, &initial_topology);
    let recorder = Recorder::default();
    let mut sr = ScreenReader::new(speech::Speech::new(Box::new(recorder.clone())));
    let mut sink = Vec::new();

    assert_eq!(chooser.title(), "tmux sessions");
    let contents = chooser.model().contents_full();
    assert!(contents.contains("$1 work"));
    assert!(
        !contents.contains("> "),
        "selector marker remained: {contents:?}"
    );
    assert_eq!(chooser.model().screen().cursor_position(), (1, 0));
    assert!(chooser.model().contents_full().contains("$2 work"));
    assert_eq!(
        chooser.selected_target(),
        Some(TmuxChooserTarget::Session(SessionId(1)))
    );

    assert!(matches!(
        chooser.handle_input(&mut sr, b"2", &mut sink).unwrap(),
        ViewAction::Redraw
    ));
    assert_eq!(
        chooser.selected_target(),
        Some(TmuxChooserTarget::Session(SessionId(2)))
    );
    assert!(
        recorder
            .0
            .borrow()
            .iter()
            .any(|message| message == "$2 work")
    );
    chooser.sync_topology(&topology("renamed remote"));
    assert_eq!(
        chooser.selected_target(),
        Some(TmuxChooserTarget::Session(SessionId(2)))
    );
    assert!(chooser.model().contents_full().contains("renamed remote"));

    chooser.sync_topology(&TmuxTopology::new(1));
    assert_eq!(chooser.selected_target(), None);
    assert!(
        chooser
            .model()
            .contents_full()
            .contains("no matching sessions")
    );
    assert!(matches!(
        chooser.handle_input(&mut sr, b"\r", &mut sink).unwrap(),
        ViewAction::Bell
    ));
    assert!(matches!(
        chooser.handle_input(&mut sr, b"\x1b", &mut sink).unwrap(),
        ViewAction::Pop
    ));

    chooser.on_resize(5, 24);
    assert_eq!(chooser.model().size(), (5, 24));
    assert!(chooser.model().contents_full().contains("search:"));
}

#[test]
fn window_and_pane_choosers_never_flatten_other_scopes() {
    let topology = topology("work");
    let mut windows = TmuxChooserView::windows(8, 60, 1, &topology);
    let window_text = windows.model().contents_full();
    assert!(window_text.contains("@10 1 duplicate"));
    assert!(window_text.contains("@11 2 duplicate"));
    assert!(!window_text.contains("@12"));
    assert_eq!(
        windows.selected_target(),
        Some(TmuxChooserTarget::Window(WindowId(10)))
    );

    let mut panes = TmuxChooserView::panes(8, 60, 1, &topology);
    let pane_text = panes.model().contents_full();
    assert!(pane_text.contains("%20 1 left"));
    assert!(pane_text.contains("%23 2 right"));
    assert!(!pane_text.contains("%21"));
    assert!(!pane_text.contains("%22"));
    assert_eq!(
        panes.selected_target(),
        Some(TmuxChooserTarget::Pane(PaneId(20)))
    );
}

#[test]
fn chooser_scrolls_a_bounded_viewport_to_keep_selection_and_help_visible() {
    let topology = topology_with_many_sessions();
    let mut chooser = TmuxChooserView::sessions(5, 40, 1, &topology);
    let mut sr = ScreenReader::new(speech::Speech::new(Box::new(Recorder::default())));
    let mut sink = Vec::new();

    for _ in 0..6 {
        assert!(matches!(
            chooser.handle_input(&mut sr, b"\x1b[B", &mut sink).unwrap(),
            ViewAction::Redraw
        ));
    }

    assert_eq!(
        chooser.selected_target(),
        Some(TmuxChooserTarget::Session(SessionId(7)))
    );
    let contents = chooser.model().contents_full();
    assert!(contents.contains("$7 session-7"), "{contents:?}");
    assert!(contents.contains("Up/Down select"), "{contents:?}");
    assert!(!contents.contains("$1 work"), "{contents:?}");
    let cursor_row = chooser.model().screen().cursor_position().0;
    assert!(chooser.model().line(cursor_row).contains("$7 session-7"));
}

#[test]
fn command_view_edits_submits_history_cancels_and_resizes() {
    let mut sr = ScreenReader::new(speech::Speech::new(Box::new(Recorder::default())));
    let mut sink = Vec::new();
    let mut command = TmuxCommandView::new(6, 40, 7, vec!["list-sessions".to_owned()]);
    assert_eq!(command.title(), "tmux command");
    assert!(matches!(
        command
            .handle_input(&mut sr, b"new-window -n notes", &mut sink)
            .unwrap(),
        ViewAction::Redraw
    ));
    assert!(
        command
            .model()
            .contents_full()
            .contains("new-window -n notes")
    );
    assert!(matches!(
        command.handle_input(&mut sr, b"\r", &mut sink).unwrap(),
        ViewAction::TmuxCommandSubmit { connection_id: 7, ref command }
            if command == "new-window -n notes"
    ));

    let mut history = TmuxCommandView::new(5, 25, 7, vec!["list-sessions".to_owned()]);
    history.handle_input(&mut sr, b"\x1b[A", &mut sink).unwrap();
    assert!(history.model().contents_full().contains("list-sessions"));
    history.on_resize(4, 18);
    assert_eq!(history.model().size(), (4, 18));
    assert!(matches!(
        history.handle_input(&mut sr, b"\x1b", &mut sink).unwrap(),
        ViewAction::Pop
    ));

    let mut narrow = TmuxCommandView::new(4, 12, 7, Vec::new());
    narrow
        .handle_input(&mut sr, b"display-message", &mut sink)
        .unwrap();
    assert!(
        narrow.model().contents_full().contains("message"),
        "long input hid the cursor tail: {:?}",
        narrow.model().contents_full()
    );
    let mut pasted = TmuxCommandView::new(4, 40, 7, Vec::new());
    pasted
        .handle_paste(&mut sr, "one\nnew-window", &mut sink)
        .unwrap();
    assert!(
        pasted.model().contents_full().contains('↵'),
        "pasted control was not rendered safely: {:?}",
        pasted.model().contents_full()
    );
}

#[test]
fn discovered_and_explicit_chooser_actions_execute_stable_scoped_ids() {
    let (mut app, mut sr, _recorder, mut physical) = ready_app();
    input(&mut app, &mut sr, &mut physical, b"\x01s2\r");
    assert_eq!(
        tick(&mut app, &mut sr, &mut physical),
        b"switch-client -t $2\n"
    );

    let (mut app, mut sr, _recorder, mut physical) = ready_app();
    input(&mut app, &mut sr, &mut physical, b"\x01w\x1b[B\r");
    assert_eq!(
        tick(&mut app, &mut sr, &mut physical),
        b"select-window -t @11\n"
    );

    let (mut app, mut sr, _recorder, mut physical) = ready_app();
    input(&mut app, &mut sr, &mut physical, b"\x01q\x1b[B\r");
    assert_eq!(
        tick(&mut app, &mut sr, &mut physical),
        b"select-pane -t %23\n"
    );

    let (mut app, mut sr, recorder, mut physical) = ready_app();
    recorder.0.borrow_mut().clear();
    assert!(
        app.show_tmux_session_chooser(&mut sr, &mut physical)
            .unwrap()
    );
    assert!(
        recorder
            .0
            .borrow()
            .iter()
            .any(|message| message == "tmux sessions")
    );
    input(&mut app, &mut sr, &mut physical, b"\x1b[27;1u");
    recorder.0.borrow_mut().clear();
    assert!(
        app.show_tmux_window_chooser(&mut sr, &mut physical)
            .unwrap()
    );
    assert!(
        recorder
            .0
            .borrow()
            .iter()
            .any(|message| message == "tmux windows")
    );
    input(&mut app, &mut sr, &mut physical, b"\x1b[27;1u");
    recorder.0.borrow_mut().clear();
    assert!(app.show_tmux_pane_chooser(&mut sr, &mut physical).unwrap());
    assert!(
        recorder
            .0
            .borrow()
            .iter()
            .any(|message| message == "tmux panes")
    );
    input(&mut app, &mut sr, &mut physical, b"\x1b[27;1u");
    recorder.0.borrow_mut().clear();
    assert!(
        app.show_tmux_command_prompt(&mut sr, &mut physical)
            .unwrap()
    );
    assert!(
        recorder
            .0
            .borrow()
            .iter()
            .any(|message| message == "tmux command")
    );
}

#[test]
fn chooser_tracks_external_rename_and_destruction_without_losing_scope() {
    let (mut app, mut sr, recorder, mut physical) = ready_app();
    assert!(
        app.show_tmux_window_chooser(&mut sr, &mut physical)
            .unwrap()
    );
    input(&mut app, &mut sr, &mut physical, b"\x1b[B");
    app.handle_pty(
        &mut sr,
        b"%window-renamed @11 externally-renamed\n",
        &mut physical,
    )
    .unwrap();
    assert!(
        app.debug_active_view_contents()
            .contains("externally-renamed")
    );
    assert!(
        recorder
            .0
            .borrow()
            .iter()
            .any(|message| message.contains("externally-renamed"))
    );
    app.handle_pty(&mut sr, b"%window-close @11\n", &mut physical)
        .unwrap();
    let contents = app.debug_active_view_contents();
    assert!(!contents.contains("@11"));
    assert!(!contents.contains("@12"));
    input(&mut app, &mut sr, &mut physical, b"\r");
    assert_eq!(
        tick(&mut app, &mut sr, &mut physical),
        b"select-window -t @10\n"
    );
}

#[test]
fn command_prompt_history_replies_messages_errors_and_popup_dismissal_are_accessible() {
    let (mut app, mut sr, recorder, mut physical) = ready_app();
    input(
        &mut app,
        &mut sr,
        &mut physical,
        b"\x01:display-message -p hello\r",
    );
    assert_eq!(
        tick(&mut app, &mut sr, &mut physical),
        b"display-message -p hello\n"
    );
    app.handle_pty(
        &mut sr,
        &reply(70, &[b"hello".to_vec()], true),
        &mut physical,
    )
    .unwrap();
    assert!(app.debug_active_view_contents().contains("hello"));
    assert!(
        recorder
            .0
            .borrow()
            .iter()
            .any(|message| message == "tmux command result")
    );
    assert!(
        recorder
            .0
            .borrow()
            .iter()
            .any(|message| message.contains("Press Enter"))
    );
    input(&mut app, &mut sr, &mut physical, b"\x1b[27;1u");

    assert!(
        app.show_tmux_command_prompt(&mut sr, &mut physical)
            .unwrap()
    );
    input(&mut app, &mut sr, &mut physical, b"\x1b[A");
    assert!(
        app.debug_active_view_contents()
            .contains("display-message -p hello")
    );
    input(&mut app, &mut sr, &mut physical, b"\x1b[27;1u");

    input(&mut app, &mut sr, &mut physical, b"\x01Z");
    assert!(
        String::from_utf8_lossy(&tick(&mut app, &mut sr, &mut physical))
            .starts_with("display-message -p -F")
    );
    app.handle_pty(
        &mut sr,
        &reply(71, &[b"work:1:1".to_vec()], true),
        &mut physical,
    )
    .unwrap();
    assert!(app.debug_active_view_contents().contains("work:1:1"));
    input(&mut app, &mut sr, &mut physical, b"\r");

    recorder.0.borrow_mut().clear();
    app.handle_pty(&mut sr, b"%message external notice\n", &mut physical)
        .unwrap();
    assert!(app.debug_active_view_contents().contains("external notice"));
    assert!(
        recorder
            .0
            .borrow()
            .iter()
            .any(|message| message == "tmux message")
    );
    input(&mut app, &mut sr, &mut physical, b"\r");
    app.handle_pty(&mut sr, b"%config-error bad config line\n", &mut physical)
        .unwrap();
    assert!(app.debug_active_view_contents().contains("bad config line"));
    input(&mut app, &mut sr, &mut physical, b"\r");

    assert!(
        app.show_tmux_command_prompt(&mut sr, &mut physical)
            .unwrap()
    );
    input(&mut app, &mut sr, &mut physical, b"bad-command\r");
    assert_eq!(tick(&mut app, &mut sr, &mut physical), b"bad-command\n");
    recorder.0.borrow_mut().clear();
    app.handle_pty(
        &mut sr,
        &reply(72, &[b"unknown command".to_vec()], false),
        &mut physical,
    )
    .unwrap();
    assert!(app.debug_active_view_contents().contains("unknown command"));
    assert!(
        recorder
            .0
            .borrow()
            .iter()
            .any(|message| message == "tmux command failed")
    );
}

#[test]
fn command_prompt_rejects_multiline_paste_and_announces_empty_success() {
    let (mut app, mut sr, recorder, mut physical) = ready_app();
    assert!(
        app.show_tmux_command_prompt(&mut sr, &mut physical)
            .unwrap()
    );
    input(
        &mut app,
        &mut sr,
        &mut physical,
        b"\x1b[200~display-message one\nnew-window\x1b[201~\r",
    );
    assert!(tick(&mut app, &mut sr, &mut physical).is_empty());
    assert!(
        app.debug_active_view_contents()
            .contains("commands cannot contain")
    );
    assert!(
        recorder
            .0
            .borrow()
            .iter()
            .any(|message| message.contains("tmux command rejected"))
    );
    input(&mut app, &mut sr, &mut physical, b"\r");

    assert!(
        app.show_tmux_command_prompt(&mut sr, &mut physical)
            .unwrap()
    );
    input(&mut app, &mut sr, &mut physical, b"list-sessions\r");
    assert_eq!(tick(&mut app, &mut sr, &mut physical), b"list-sessions\n");
    app.handle_pty(&mut sr, &reply(73, &[], true), &mut physical)
        .unwrap();
    assert!(
        app.debug_active_view_contents()
            .contains("command completed: list-sessions")
    );
}

#[test]
fn chooser_and_command_popups_match_the_ghostty_physical_oracle() {
    let (mut app, mut sr, _recorder, mut physical) = ready_app();
    let mut oracle = GhosttyEngine::new_with_scrollback(24, 80, 0).unwrap();
    assert_physical_scene(&app, &mut oracle, &mut physical, "tmux base scene");

    assert!(
        app.show_tmux_session_chooser(&mut sr, &mut physical)
            .unwrap()
    );
    assert_physical_scene(&app, &mut oracle, &mut physical, "session chooser");
    input(&mut app, &mut sr, &mut physical, b"2");
    assert_physical_scene(&app, &mut oracle, &mut physical, "filtered session chooser");
    input(&mut app, &mut sr, &mut physical, b"\x1b[27;1u");
    assert_physical_scene(&app, &mut oracle, &mut physical, "chooser cancellation");

    assert!(
        app.show_tmux_command_prompt(&mut sr, &mut physical)
            .unwrap()
    );
    input(&mut app, &mut sr, &mut physical, b"list-sessions\r");
    assert_physical_scene(&app, &mut oracle, &mut physical, "command submission");
    assert_eq!(tick(&mut app, &mut sr, &mut physical), b"list-sessions\n");
    app.handle_pty(
        &mut sr,
        &reply(74, &[b"work: 1 windows".to_vec()], true),
        &mut physical,
    )
    .unwrap();
    assert_physical_scene(&app, &mut oracle, &mut physical, "command result popup");
    input(&mut app, &mut sr, &mut physical, b"\r");
    assert_physical_scene(&app, &mut oracle, &mut physical, "popup dismissal");
}

#[test]
fn tmux_m_w_announces_connection_and_window_while_terminal_wording_stays_separate() {
    let (mut app, mut sr, recorder, mut physical) = ready_app();
    recorder.0.borrow_mut().clear();
    input(&mut app, &mut sr, &mut physical, b"\x1bw");
    assert!(
        recorder
            .0
            .borrow()
            .iter()
            .any(|message| message == "tmux, work, 1.1: duplicate")
    );

    let recorder = Recorder::default();
    let stack = views::ViewStack::new(Box::new(views::PtyView::new(4, 20)));
    let mut terminal = App::new(stack).unwrap();
    let mut terminal_sr = ScreenReader::new(speech::Speech::new(Box::new(recorder.clone())));
    input(&mut terminal, &mut terminal_sr, &mut Vec::new(), b"\x1bw");
    assert!(
        recorder
            .0
            .borrow()
            .iter()
            .any(|message| message == "terminal")
    );
}

#[test]
fn window_and_session_changes_announce_the_new_location_concisely() {
    let (mut app, mut sr, recorder, mut physical) = ready_app();
    recorder.0.borrow_mut().clear();

    app.handle_pty(&mut sr, b"%session-window-changed $1 @11\n", &mut physical)
        .unwrap();
    assert_eq!(&*recorder.0.borrow(), &["duplicate", "second-window"]);

    recorder.0.borrow_mut().clear();
    app.handle_pty(&mut sr, b"%session-changed $2 remote\n", &mut physical)
        .unwrap();
    assert_eq!(
        &*recorder.0.borrow(),
        &["remote", "duplicate", "other-session"]
    );
}

#[test]
fn scheduled_location_changes_wait_for_their_receipt_and_stay_concise() {
    let (mut app, mut sr, recorder, mut physical) = ready_app();
    app.enable_output_scheduler(OutputSchedulerConfig {
        latency_budget_ms: 0,
        ..OutputSchedulerConfig::default()
    });
    recorder.0.borrow_mut().clear();
    physical.clear();

    app.handle_pty(&mut sr, b"%session-window-changed $1 @11\n", &mut physical)
        .unwrap();
    assert!(
        recorder.0.borrow().is_empty(),
        "the new window was announced before its frame flushed"
    );
    assert_eq!(
        app.drain_scheduled_output(&mut physical, false)
            .expect("flush the selected window")
            .completed_renders
            .len(),
        1
    );
    assert!(
        recorder.0.borrow().is_empty(),
        "draining pixels alone announced the selected window"
    );
    assert!(tick(&mut app, &mut sr, &mut physical).is_empty());
    assert_eq!(&*recorder.0.borrow(), &["duplicate", "second-window"]);

    recorder.0.borrow_mut().clear();
    app.handle_pty(
        &mut sr,
        b"%window-renamed @11 renamed-in-place\n",
        &mut physical,
    )
    .unwrap();
    assert!(recorder.0.borrow().is_empty());
    assert_eq!(
        app.drain_scheduled_output(&mut physical, false)
            .expect("flush the in-place rename")
            .completed_renders
            .len(),
        1
    );
    assert!(tick(&mut app, &mut sr, &mut physical).is_empty());
    assert!(
        recorder.0.borrow().is_empty(),
        "an in-place rename was announced after its frame flushed"
    );

    app.handle_pty(&mut sr, b"%session-changed $2 remote\n", &mut physical)
        .unwrap();
    assert!(
        recorder.0.borrow().is_empty(),
        "the new session was announced before its frame flushed"
    );
    assert_eq!(
        app.drain_scheduled_output(&mut physical, false)
            .expect("flush the selected session")
            .completed_renders
            .len(),
        1
    );
    assert!(recorder.0.borrow().is_empty());
    assert!(tick(&mut app, &mut sr, &mut physical).is_empty());
    assert_eq!(
        &*recorder.0.borrow(),
        &["remote", "duplicate", "other-session"]
    );
}

#[test]
fn pane_changes_read_only_the_new_application_cursor_line() {
    let (mut app, mut sr, recorder, mut physical) = ready_app();
    recorder.0.borrow_mut().clear();

    app.handle_pty(&mut sr, b"%window-pane-changed @10 %23\n", &mut physical)
        .unwrap();

    assert_eq!(&*recorder.0.borrow(), &["right"]);
}

#[test]
fn tmux_location_renames_in_place_are_not_announced() {
    let (mut app, mut sr, recorder, mut physical) = ready_app();
    recorder.0.borrow_mut().clear();

    app.handle_pty(
        &mut sr,
        b"%window-renamed @10 renamed-in-place\n",
        &mut physical,
    )
    .unwrap();

    assert!(recorder.0.borrow().is_empty());

    app.handle_pty(
        &mut sr,
        b"%session-renamed renamed-in-place\n",
        &mut physical,
    )
    .unwrap();

    assert!(recorder.0.borrow().is_empty());
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
    let mut idle_deadline = Instant::now() + Duration::from_secs(5);
    for _ in 0..800 {
        if done(app) {
            return;
        }
        write_real_commands(app, sr, writer, physical);
        let remaining = idle_deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            panic!(
                "timed out in {case}; contents={:?}; topology={:?}",
                app.debug_active_view_contents(),
                app.debug_tmux_topology(1)
            );
        }
        match receiver.recv_timeout(remaining.min(Duration::from_millis(10))) {
            Ok(chunk) => {
                idle_deadline = Instant::now() + Duration::from_secs(5);
                app.handle_pty(sr, &chunk, physical).unwrap();
            }
            Err(RecvTimeoutError::Timeout) => {
                // A pane update can race an authoritative capture and schedule
                // a quiet recapture while the tmux control channel is idle.
                // Keep driving Lector's timers until data arrives.
            }
            Err(error) => panic!(
                "tmux channel failed in {case}: {error}; contents={:?}; topology={:?}",
                app.debug_active_view_contents(),
                app.debug_tmux_topology(1)
            ),
        }
    }
    panic!("real tmux interaction fixture exceeded its bounded event count in {case}");
}

#[test]
fn real_tmux_session_chooser_and_command_prompt_cross_the_control_connection() {
    let _serial = super::serialize_real_tmux_test();
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
    let socket = socket_dir.join(format!("interaction-{}-{unique}.sock", std::process::id()));
    let first_session = format!("lector_first_{}_{unique}", std::process::id());
    let second_session = format!("lector_second_{}_{unique}", std::process::id());
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
        &first_session,
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
                Err(error) => panic!("read real tmux interaction PTY: {error}"),
            }
        }
    });

    let stack = views::ViewStack::new(Box::new(views::PtyView::new(24, 80)));
    let mut app = App::new(stack).unwrap();
    let mut sr = ScreenReader::new(speech::Speech::new(Box::new(Recorder::default())));
    let mut physical = Vec::new();
    drive_real_tmux(
        "interaction bootstrap",
        &mut app,
        &mut sr,
        &receiver,
        writer.as_mut(),
        &mut physical,
        |app| app.debug_active_view_contents().contains("FIRST"),
    );

    writer
        .write_all(
            format!(
                "new-session -d -s {second_session} \"/bin/sh -c 'read ready; printf SECOND; exec cat'\"\n"
            )
            .as_bytes(),
        )
        .unwrap();
    writer.flush().unwrap();
    drive_real_tmux(
        "second-session discovery",
        &mut app,
        &mut sr,
        &receiver,
        writer.as_mut(),
        &mut physical,
        |app| {
            app.debug_tmux_topology(1)
                .is_some_and(|dump| dump.contains(&second_session))
        },
    );
    writer
        .write_all(
            format!(
                "switch-client -t {second_session}\n\
                 send-keys -t {second_session} Enter\n"
            )
            .as_bytes(),
        )
        .unwrap();
    writer.flush().unwrap();
    drive_real_tmux(
        "second-session bootstrap",
        &mut app,
        &mut sr,
        &receiver,
        writer.as_mut(),
        &mut physical,
        |app| app.debug_active_view_contents().contains("SECOND"),
    );
    writer
        .write_all(format!("switch-client -t {first_session}\n").as_bytes())
        .unwrap();
    writer.flush().unwrap();
    drive_real_tmux(
        "restore first session",
        &mut app,
        &mut sr,
        &receiver,
        writer.as_mut(),
        &mut physical,
        |app| app.debug_active_view_contents().contains("FIRST"),
    );

    input(&mut app, &mut sr, &mut physical, b"\x01s");
    input(&mut app, &mut sr, &mut physical, second_session.as_bytes());
    input(&mut app, &mut sr, &mut physical, b"\r");
    let chooser_command = write_real_commands(&mut app, &mut sr, writer.as_mut(), &mut physical);
    assert!(String::from_utf8_lossy(&chooser_command).starts_with("switch-client -t $"));
    drive_real_tmux(
        "real session chooser",
        &mut app,
        &mut sr,
        &receiver,
        writer.as_mut(),
        &mut physical,
        |app| app.debug_active_view_contents().contains("SECOND"),
    );

    input(
        &mut app,
        &mut sr,
        &mut physical,
        b"\x01:display-message -p real-command-result\r",
    );
    let prompt_command = write_real_commands(&mut app, &mut sr, writer.as_mut(), &mut physical);
    assert_eq!(prompt_command, b"display-message -p real-command-result\n");
    drive_real_tmux(
        "real command result",
        &mut app,
        &mut sr,
        &receiver,
        writer.as_mut(),
        &mut physical,
        |app| {
            app.debug_active_view_contents()
                .contains("real-command-result")
        },
    );

    writer.write_all(b"kill-server\n").unwrap();
    writer.flush().unwrap();
    let _ = child.wait().unwrap();
    read_thread.join().unwrap();
    let _ = std::fs::remove_file(&socket);
}
