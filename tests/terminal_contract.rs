use lector::terminal::{
    Cell, Color, Cursor, GhosttyEngine, MouseEncoding, MouseProtocol, PrintBoundary, PrintedRun,
    ScreenIdentity, Style, TerminalDamage, TerminalEffects, TerminalEngine, TerminalModes,
    TerminalSnapshot, UpdateSummary, Viewport,
};

fn assert_engine_contract<T: TerminalEngine>() {}

fn engine(rows: u16, cols: u16, scrollback: usize) -> Box<dyn TerminalEngine> {
    Box::new(
        GhosttyEngine::new_with_scrollback(rows, cols, scrollback)
            .expect("create Ghostty terminal engine"),
    )
}

#[test]
fn ghostty_engine_implements_the_engine_neutral_contract() {
    assert_engine_contract::<GhosttyEngine>();

    let _: TerminalSnapshot = TerminalSnapshot::default();
    let _: Cell = Cell::default();
    let _: Style = Style::default();
    let _: Cursor = Cursor::default();
    let _: TerminalModes = TerminalModes::default();
    let _: TerminalEffects = TerminalEffects::default();
    let _: TerminalDamage = TerminalDamage::default();
    let _: UpdateSummary = UpdateSummary::default();
}

#[test]
fn contract_exposes_cells_modes_semantics_effects_damage_and_replies() {
    let mut engine = engine(3, 10, 20);
    let update = engine.advance(
        b"\x1B]0;engine title\x07\x1B]133;A\x07\x1B[1;31mA\x1B[0m e\xCC\x81\x07\x1B[?2004h\x1B[?1000h\x1B[?1006h",
    );
    let snapshot = engine.snapshot();

    assert_eq!(snapshot.size(), (3, 10));
    assert_eq!(snapshot.screen, ScreenIdentity::Primary);
    assert_eq!(snapshot.title.as_deref(), Some("engine title"));
    assert_eq!(snapshot.cell(0, 0).unwrap().grapheme, "A");
    assert_eq!(
        snapshot.cell(0, 0).unwrap().style.foreground,
        Color::Indexed(1)
    );
    assert!(snapshot.cell(0, 0).unwrap().style.bold);
    assert_eq!(snapshot.cell(0, 2).unwrap().grapheme, "e\u{301}");
    assert_eq!(
        snapshot.cursor,
        Cursor {
            row: 0,
            col: 3,
            visible: true
        }
    );
    assert!(snapshot.modes.bracketed_paste);
    assert_eq!(snapshot.modes.mouse_protocol, MouseProtocol::PressRelease);
    assert_eq!(snapshot.modes.mouse_encoding, MouseEncoding::Sgr);
    assert_eq!(snapshot.semantic_marks.len(), 1);
    assert_eq!(update.effects.bells, 1);
    assert!(update.damage.is_dirty());
    assert!(update.pty_replies.is_empty());
    assert_eq!(update.printed_text(), "A e\u{301}");
    assert_eq!(update.changed_rows.len(), 1);
    assert_eq!(update.changed_rows[0].start(), &0);
    assert_eq!(update.changed_rows[0].end(), &0);
    assert_eq!((update.cursor_before.row, update.cursor_before.col), (0, 0));
    assert_eq!((update.cursor_after.row, update.cursor_after.col), (0, 3));
}

#[test]
fn update_summary_tracks_fragmented_prints_operations_screens_and_sync() {
    let mut engine = engine(3, 10, 20);
    let mut update = engine.advance(b"one\r");
    update.merge(engine.advance(b"\ntwo\x1B["));
    update.merge(engine.advance(b"1D!\x1B[S\x1B[?2026h"));

    assert_eq!(update.batch_count, 3);
    assert_eq!(update.printed_text(), "one\ntwo!");
    assert_eq!(update.cursor_operations, 1);
    assert_eq!(update.scroll_operations, 1);
    assert_eq!((update.cursor_before.row, update.cursor_before.col), (0, 0));
    assert_eq!((update.cursor_after.row, update.cursor_after.col), (1, 3));
    assert_eq!(update.screen_before, ScreenIdentity::Primary);
    assert_eq!(update.screen_after, ScreenIdentity::Primary);
    assert!(update.synchronized_output);
    assert!(engine.snapshot().modes.synchronized_output);
    assert!(!update.changed_rows.is_empty());

    let alternate = engine.advance(b"\x1B[?2026l\x1B[?1049h alt");
    assert_eq!(alternate.screen_before, ScreenIdentity::Primary);
    assert_eq!(alternate.screen_after, ScreenIdentity::Alternate);
    assert!(!alternate.synchronized_output);
    assert_eq!(alternate.printed_text(), " alt");
}

#[test]
fn update_summary_merges_print_boundaries_without_raw_byte_replay() {
    let mut summary = UpdateSummary::default();
    summary.merge(UpdateSummary {
        printed_runs: vec![PrintedRun {
            text: "partial".into(),
            boundary: PrintBoundary::Continue,
        }],
        batch_count: 1,
        ..UpdateSummary::default()
    });
    summary.merge(UpdateSummary {
        printed_runs: vec![
            PrintedRun {
                text: " line".into(),
                boundary: PrintBoundary::Continue,
            },
            PrintedRun {
                text: "replacement".into(),
                boundary: PrintBoundary::CarriageReturn,
            },
            PrintedRun {
                text: "next".into(),
                boundary: PrintBoundary::LineFeed,
            },
        ],
        batch_count: 1,
        ..UpdateSummary::default()
    });

    assert_eq!(summary.printed_runs.len(), 3);
    assert_eq!(summary.printed_text(), "replacement\nnext");

    let mut engine = engine(4, 20, 0);
    let blank_lines = engine.advance(b"one\r\n\r\nthree");
    assert_eq!(blank_lines.printed_text(), "one\n\nthree");
}

#[test]
fn contract_selects_live_and_review_viewports_without_losing_stream_state() {
    let mut engine = engine(2, 8, 20);
    engine.advance(b"one\r\ntwo\r\nthree");
    assert_eq!(engine.scrollback_extent(), 1);
    assert_eq!(engine.snapshot().contents(), "two\nthree");

    engine.select_viewport(Viewport::Scrollback(1));
    assert_eq!(engine.viewport(), Viewport::Scrollback(1));
    assert_eq!(engine.snapshot().contents(), "one\ntwo");

    engine.advance(b"\r\npartial \xE7\x95");
    engine.select_viewport(Viewport::Live);
    engine.advance(b"\x8C");
    assert!(engine.snapshot().contents().contains("partial 界"));
}

#[test]
fn full_snapshot_captures_history_wrapping_and_alternate_screen_identity() {
    let mut engine = engine(2, 5, 20);
    engine.advance(b"12345X\r\nlast");
    let snapshot = engine.snapshot_with_history();

    assert_eq!(snapshot.scrollback.len(), 1);
    assert!(snapshot.scrollback[0].wrapped);
    assert_eq!(snapshot.scrollback[0].contents(), "12345");

    engine.advance(b"\x1B[?1049halt\x1B[?25l");
    assert_eq!(engine.snapshot().screen, ScreenIdentity::Alternate);
    assert!(!engine.snapshot().cursor.visible);
}

#[test]
fn resize_reset_and_fragmented_titles_follow_the_contract() {
    let mut engine = engine(2, 6, 20);
    let partial = engine.advance(b"before\x1B]0;frag");
    assert!(!partial.effects.title_changed);
    assert_eq!(engine.snapshot().title, None);

    let completed = engine.advance(b"mented\x1B\\");
    assert!(completed.effects.title_changed);
    assert_eq!(engine.snapshot().title.as_deref(), Some("fragmented"));

    engine.resize(4, 9);
    assert_eq!(engine.snapshot().size(), (4, 9));
    assert!(engine.snapshot().contents().contains("before"));

    engine.reset();
    assert_eq!(engine.snapshot().size(), (4, 9));
    assert_eq!(engine.snapshot().contents(), "");
    assert_eq!(engine.snapshot().title, None);
    assert_eq!(engine.scrollback_extent(), 0);
}

#[test]
fn normalized_snapshot_covers_the_screen_read_contract() {
    let mut engine = engine(2, 8, 4);
    engine.advance(b"one  two\x1B[?1h\x1B=\x1B[?25l");
    let snapshot = engine.snapshot();
    let mut full = String::from("stale");
    snapshot.contents_full_into(&mut full);

    assert_eq!(snapshot.size(), (2, 8));
    assert_eq!(snapshot.contents(), "one  two");
    assert_eq!(snapshot.rows(0, 8).next().as_deref(), Some("one  two"));
    assert_eq!(snapshot.contents_between(0, 5, 0, 8), "two");
    assert_eq!(snapshot.contents_full(), "one  two\n\n");
    assert_eq!(full, snapshot.contents_full());
    assert_eq!(snapshot.cell(0, 0).unwrap().contents(), "o");
    assert!(!snapshot.row_wrapped(0));
    // Ghostty represents a pending wrap at the final physical cell rather
    // than exposing a synthetic one-past-the-margin cursor column.
    assert_eq!(snapshot.cursor_position(), (0, 7));
    assert!(!snapshot.alternate_screen());
    assert!(snapshot.application_keypad());
    assert!(snapshot.application_cursor());
    assert!(snapshot.hide_cursor());
    assert!(!snapshot.bracketed_paste());
    assert_eq!(snapshot.mouse_protocol_mode(), MouseProtocol::None);
    assert_eq!(snapshot.mouse_protocol_encoding(), MouseEncoding::Default);
}

#[test]
fn legacy_parser_types_are_absent_from_production() {
    let consumers = [
        include_str!("../src/view.rs"),
        include_str!("../src/ext/mod.rs"),
        include_str!("../src/attributes.rs"),
        include_str!("../src/screen_reader/auto_read.rs"),
        include_str!("../src/screen_reader/tracking.rs"),
        include_str!("../src/screen_reader/hooks.rs"),
        include_str!("../src/commands/clipboard.rs"),
        include_str!("../src/commands/mouse.rs"),
        include_str!("../src/commands/review.rs"),
        include_str!("../src/commands/table.rs"),
        include_str!("../src/table.rs"),
        include_str!("../src/table/detection.rs"),
        include_str!("../src/review/document.rs"),
        include_str!("../src/lua/mod.rs"),
        include_str!("../src/lua/ext.rs"),
    ];
    assert!(consumers.iter().all(|source| !source.contains("vt100::")));
    assert!(!include_str!("../src/terminal.rs").contains("vt100::"));
    assert!(!include_str!("../src/terminal.rs").contains("Vt100Engine"));
    assert!(!include_str!("../src/app.rs").contains("vte::Parser"));
    assert!(!include_str!("../src/screen_reader/auto_read.rs").contains("pending_bytes"));
    assert!(!include_str!("../src/screen_reader/auto_read.rs").contains("Vt100Engine"));
}
