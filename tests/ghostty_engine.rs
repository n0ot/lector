use lector::terminal::{
    Color, GhosttyEngine, HistoryPosition, MouseEncoding, MouseProtocol, ScreenIdentity,
    TerminalEngine,
};
use lector::view::View;
use std::sync::Arc;

const ROWS: u16 = 4;
const COLS: u16 = 20;

#[test]
fn ghostty_engine_creates_resets_and_drops_repeatedly() {
    for iteration in 0..100 {
        let mut engine = GhosttyEngine::new(ROWS, COLS).expect("create Ghostty engine");
        engine
            .advance(format!("iteration {iteration}").as_bytes())
            .expect("advance Ghostty engine");
        assert!(
            engine.normalized_snapshot().rows[0]
                .contents()
                .starts_with("iteration ")
        );

        engine.reset().expect("reset Ghostty engine");
        assert_eq!(engine.normalized_snapshot().size(), (ROWS, COLS));
        assert!(
            engine
                .normalized_snapshot()
                .rows
                .iter()
                .all(|row| row.contents().is_empty())
        );
    }
}

#[test]
fn ghostty_engine_rejects_zero_dimensions_without_leaving_a_handle() {
    for (rows, cols) in [(0, 1), (1, 0), (0, 0)] {
        let error = GhosttyEngine::new(rows, cols)
            .err()
            .expect("zero dimensions must fail");
        assert!(error.to_string().contains("InvalidValue"), "{error:#}");
    }

    let engine = GhosttyEngine::new(1, 1).expect("create after rejected dimensions");
    assert_eq!(engine.normalized_snapshot().size(), (1, 1));
}

#[test]
fn ghostty_wrapper_reports_allocation_failure_at_every_handle_boundary() {
    lector_ghostty::allocation_failure_probe().expect("allocation failure probe");
}

#[test]
fn ghostty_engine_tracks_basic_state_without_producing_pty_replies() {
    let mut engine = GhosttyEngine::new(3, 16).expect("create Ghostty engine");
    engine
        .advance(
            b"hello\r\n\x1b]2;engine title\x07\x1b=\x1b[?1h\x1b[?1002h\x1b[?1006h\x1b[?2004h\x1b[H\x1b[?1049halternate\x1b[6n",
        )
        .expect("advance Ghostty engine");

    let snapshot = engine.normalized_snapshot();
    assert_eq!(snapshot.rows[0].contents(), "alternate");
    assert_eq!(snapshot.cursor.row, 0);
    assert_eq!(snapshot.cursor.col, 9);
    assert!(snapshot.cursor.visible);
    assert!(snapshot.alternate_screen());
    assert_eq!(snapshot.title.as_deref(), Some("engine title"));
    assert!(snapshot.modes.application_keypad);
    assert!(snapshot.modes.application_cursor);
    assert!(snapshot.modes.bracketed_paste);
    assert_eq!(snapshot.modes.mouse_protocol, MouseProtocol::ButtonMotion);
    assert_eq!(snapshot.modes.mouse_encoding, MouseEncoding::Sgr);
    assert!(engine.pty_replies().is_empty());
}

#[test]
fn ghostty_fragmented_fixtures_match_one_shot_parsing() {
    let fixtures: &[&[&[u8]]] = &[
        &[b"plain text\r\nsecond line"],
        &[b"\x1b[31;1mred", b"\x1b[0m normal"],
        &[b"abc", b"\x1b[2D", b"Z", b"\x1b[K"],
        &[b"one\r\ntwo\r\nthree\r\nfour\r\nfive"],
        &[b"primary", b"\r\x1b[?1049h", b"alternate", b"\x1b[?1049l"],
        &[
            b"\x1b]2;frag",
            b"mented title",
            b"\x1b\\",
            b"\x1b=",
            b"\x1b[?1h",
            b"\x1b[?1002h",
            b"\x1b[?1006h",
            b"\x1b[?2004h",
        ],
    ];

    for (fixture_index, chunks) in fixtures.iter().enumerate() {
        let input = chunks.concat();
        let mut one_shot = GhosttyEngine::new(ROWS, COLS).expect("create one-shot engine");
        one_shot.advance(&input).expect("parse one-shot fixture");
        let expected = one_shot.normalized_snapshot_with_history().unwrap();
        let mut fragmented = GhosttyEngine::new(ROWS, COLS).expect("create fragmented engine");

        for (chunk_index, chunk) in chunks.iter().enumerate() {
            fragmented.advance(chunk).expect("advance Ghostty engine");
            assert_eq!(
                fragmented.normalized_snapshot().size(),
                (ROWS, COLS),
                "fixture {fixture_index}, chunk {chunk_index}"
            );
        }
        assert_eq!(
            fragmented.normalized_snapshot_with_history().unwrap(),
            expected,
            "fixture {fixture_index} differs when fragmented"
        );
    }
}

#[test]
fn ghostty_update_facts_merge_across_every_fragment() {
    let chunks: &[&[u8]] = &[
        b"plain ",
        b"\xE7\x95",
        b"\x8C\x1B[",
        b"2D!",
        b"\r\nnext",
        b"\x1B[S",
        b"\x1B[?2026h",
        b"\x1B[?2026l",
    ];
    let mut ghostty = GhosttyEngine::new(4, 20).expect("create Ghostty engine");
    let mut merged = lector::terminal::UpdateSummary::default();

    for chunk in chunks {
        merged.merge(ghostty.advance(chunk).expect("advance Ghostty engine"));
    }
    assert_eq!(merged.batch_count, chunks.len());
    assert_eq!(merged.printed_text(), "plain 界!\nnext");
    assert!(merged.cursor_operations >= 1);
    assert!(merged.scroll_operations >= 1);
    assert_eq!(merged.screen_before, ScreenIdentity::Primary);
    assert_eq!(merged.screen_after, ScreenIdentity::Primary);
    assert!(!merged.synchronized_output);
    assert!(merged.synchronized_output_opened);
    assert!(merged.synchronized_output_closed);
    assert!(!merged.changed_rows.is_empty());
}

#[test]
fn ghostty_update_reports_actual_screen_transition_and_synchronized_output() {
    let mut ghostty = GhosttyEngine::new(3, 12).expect("create Ghostty engine");
    let synchronized = ghostty
        .advance(b"\x1B[?2026hworking")
        .expect("enable synchronized output");
    assert!(synchronized.synchronized_output);
    assert!(synchronized.synchronized_output_opened);
    assert!(!synchronized.synchronized_output_closed);
    assert_eq!(synchronized.printed_text(), "working");

    let alternate = ghostty
        .advance(b"\x1B[?2026l\x1B[?1049halt")
        .expect("enter alternate screen");
    assert_eq!(alternate.screen_before, ScreenIdentity::Primary);
    assert_eq!(alternate.screen_after, ScreenIdentity::Alternate);
    assert!(!alternate.synchronized_output);
    assert!(!alternate.synchronized_output_opened);
    assert!(!alternate.synchronized_output_closed);
    assert_eq!(alternate.printed_text(), "alt");

    let reopened = ghostty
        .advance(b"\x1B[?2026hfirst\x1B[?2026l\x1B[?2026hsecond")
        .expect("close and reopen synchronized output in one batch");
    assert!(reopened.synchronized_output);
    assert!(reopened.synchronized_output_opened);
    assert!(!reopened.synchronized_output_closed);
}

#[test]
fn synchronized_close_is_stable_only_when_it_ends_the_update() {
    let mut ghostty = GhosttyEngine::new(3, 20).expect("create Ghostty engine");
    ghostty
        .advance(b"\x1b[?2026hworking")
        .expect("open synchronized output");

    let exact_close = ghostty
        .advance(b" final\x1b[?2026l")
        .expect("close at update boundary");
    assert!(exact_close.synchronized_output_closed);

    ghostty
        .advance(b"\x1b[?2026hworking")
        .expect("reopen synchronized output");
    let trailing_output = ghostty
        .advance(b" final\x1b[?2026lmore")
        .expect("close before ordinary output");
    assert!(!trailing_output.synchronized_output_closed);
}

#[test]
fn synchronized_open_captures_dirty_history_then_reuses_it_until_it_changes() {
    let seed = b"one\r\ntwo\r\nthree\r\nfour";

    let mut unchanged =
        GhosttyEngine::new_with_scrollback(2, 12, 2).expect("create unchanged-history engine");
    unchanged.advance(seed).expect("seed retained history");
    unchanged
        .advance(b"\x1b[?2026hpartial")
        .expect("open after history changed");
    let first_open = unchanged
        .take_synchronized_output_open_snapshot()
        .expect("first opening checkpoint");
    assert_eq!(first_open.scrollback.len(), 2);
    unchanged
        .advance(b"\x1b[?2026l\x1b[?2026hmore")
        .expect("reopen without changing history");
    let unchanged_open = unchanged
        .take_synchronized_output_open_snapshot()
        .expect("unchanged-history opening checkpoint");
    assert_eq!(unchanged_open.scrollback_extent, 2);
    assert!(
        unchanged_open.scrollback.is_empty(),
        "unchanged history should be reused by View instead of cloned per frame"
    );

    let mut scrolled =
        GhosttyEngine::new_with_scrollback(2, 12, 2).expect("create prefix-scroll engine");
    scrolled.advance(seed).expect("seed retained history");
    let update = scrolled
        .advance(b"\r\nbefore\x1b[?2026h\x1b[2Jpartial")
        .expect("scroll before opening synchronized output");
    assert!(update.history_changed);
    let scrolled_open = scrolled
        .take_synchronized_output_open_snapshot()
        .expect("history-bearing opening checkpoint");
    assert_eq!(scrolled_open.scrollback.len(), 2);
    assert_eq!(scrolled_open.scrollback[0].contents(), "two");
    assert_eq!(scrolled_open.scrollback[1].contents(), "three");
    assert_eq!(scrolled_open.rows[0].contents(), "four");
    assert_eq!(scrolled_open.rows[1].contents(), "before");
}

#[test]
fn synchronized_reopen_captures_history_changed_in_the_previous_chunk() {
    let mut engine =
        GhosttyEngine::new_with_scrollback(2, 12, 2).expect("create synchronized engine");
    engine.advance(b"one\r\ntwo").expect("seed the screen");
    engine
        .advance(b"\x1b[?2026h\r\nthree")
        .expect("scroll while the first frame is open");

    engine
        .advance(b"\x1b[?2026l\x1b[?2026hpartial")
        .expect("commit and reopen in a later chunk");
    let reopened = engine
        .take_synchronized_output_open_snapshot()
        .expect("reopened checkpoint");

    assert_eq!(reopened.scrollback.len(), 1);
    assert_eq!(reopened.scrollback[0].contents(), "one");
    assert_eq!(reopened.rows[0].contents(), "two");
    assert_eq!(reopened.rows[1].contents(), "three");
}

#[test]
fn synchronized_reopen_carries_the_capped_history_lineage() {
    let mut engine =
        GhosttyEngine::new_with_scrollback(2, 12, 2).expect("create synchronized engine");
    engine
        .advance(b"one\r\ntwo\r\nthree\r\nfour")
        .expect("fill bounded history");
    engine
        .advance(b"\x1b[?2026h\r\nfive")
        .expect("open and scroll the working frame");

    engine
        .advance(b"\x1b[?2026l\x1b[?2026hpartial")
        .expect("commit and reopen after capped eviction");
    let reopened = engine
        .take_synchronized_output_open_snapshot()
        .expect("reopened checkpoint");

    assert_eq!(reopened.history_origin, 1);
    assert_eq!(reopened.scrollback_extent, 2);
    assert_eq!(
        reopened
            .scrollback
            .iter()
            .map(|row| row.contents())
            .collect::<Vec<_>>(),
        ["two", "three"]
    );
}

#[test]
fn alternate_opener_preserves_dirty_primary_history_for_the_next_checkpoint() {
    let mut engine =
        GhosttyEngine::new_with_scrollback(2, 12, 2).expect("create synchronized engine");
    engine
        .advance(b"one\r\ntwo\r\nthree\r\nfour")
        .expect("fill bounded history");
    engine
        .advance(b"\r\nfive\x1b[?1049h\x1b[?2026h")
        .expect("scroll primary, enter alternate, and open");
    let alternate = engine
        .take_synchronized_output_open_snapshot()
        .expect("alternate opening checkpoint");
    assert!(alternate.alternate_screen());
    assert!(alternate.scrollback.is_empty());

    engine
        .advance(b"\x1b[?2026l\x1b[?1049l\x1b[?2026h")
        .expect("return to primary and reopen");
    let primary = engine
        .take_synchronized_output_open_snapshot()
        .expect("primary opening checkpoint");
    assert!(!primary.alternate_screen());
    assert_eq!(primary.history_origin, 1);
    assert_eq!(
        primary
            .scrollback
            .iter()
            .map(|row| row.contents())
            .collect::<Vec<_>>(),
        ["two", "three"]
    );
}

#[test]
fn ghostty_resize_preserves_dimensions_and_cursor() {
    let mut engine = GhosttyEngine::new(4, 12).expect("create Ghostty engine");
    engine.advance(b"\x1b[2;3H").unwrap();
    engine.resize(3, 8).expect("resize Ghostty engine");

    let snapshot = engine.normalized_snapshot();
    assert_eq!(snapshot.size(), (3, 8));
    assert_eq!(snapshot.cursor_position(), (1, 2));
}

#[test]
fn normalized_snapshots_are_owned_and_cannot_mutate_the_engine() {
    let engine = GhosttyEngine::new(2, 8).expect("create Ghostty engine");
    let mut changed = engine.normalized_snapshot();
    let original = changed.clone();
    assert!(Arc::ptr_eq(&changed.rows, &original.rows));
    changed.cursor.col = 1;
    let rows = Arc::make_mut(&mut changed.rows);
    Arc::make_mut(&mut rows[0].cells)[0].grapheme = "x".into();
    assert_eq!(changed.cursor.col, 1);
    assert_eq!(changed.rows[0].contents(), "x");
    assert_eq!(original.rows[0].contents(), "");
    assert_eq!(engine.normalized_snapshot().cursor.col, 0);
    assert_eq!(engine.normalized_snapshot().rows[0].contents(), "");
}

#[test]
fn live_adapter_snapshots_share_ghostty_rows_and_preserve_unchanged_cells() {
    let mut engine = GhosttyEngine::new(4, 20).expect("create Ghostty engine");
    assert!(Arc::ptr_eq(
        &engine.snapshot().rows,
        &engine.ghostty_snapshot().rows
    ));
    let before = engine.snapshot().clone();

    engine.advance(b"one dirty row").expect("advance one row");

    assert!(Arc::ptr_eq(
        &engine.snapshot().rows,
        &engine.ghostty_snapshot().rows
    ));
    assert!(!Arc::ptr_eq(
        &before.rows[0].cells,
        &engine.snapshot().rows[0].cells
    ));
    assert!(Arc::ptr_eq(
        &before.rows[1].cells,
        &engine.snapshot().rows[1].cells
    ));
    assert_eq!(before.rows[0].contents(), "");
    assert_eq!(engine.snapshot().rows[0].contents(), "one dirty row");
}

#[test]
fn view_uses_ghostty_as_its_authoritative_runtime_engine() {
    let chunks: &[&[u8]] = &[
        b"plain \x1b[31mred\x1b[0m",
        b"\r\nsecond",
        b"\x1b[2D!\x1b[K",
        b"\x1b]2;runtime engine\x1b\\",
        b"\x1b[?1h\x1b[?2004h",
    ];
    let mut view = View::new(ROWS, COLS);
    for chunk in chunks {
        view.process_changes(chunk);
    }

    assert_eq!(view.screen().rows[0].contents(), "plain red");
    assert_eq!(view.screen().rows[1].contents(), "seco!");
    assert_eq!(view.screen().title.as_deref(), Some("runtime engine"));
}

#[test]
fn ghostty_snapshot_preserves_review_sensitive_cells_styles_and_links() {
    let mut engine = GhosttyEngine::new(2, 16).expect("create Ghostty engine");
    engine
        .advance(
            "\x1b[1;2;3;4;31;48;2;1;2;3mA\x1b[0m e\u{301}\u{00b7}\
             \x1b]8;;https://example.test/\x1b\\\u{754c}\x1b]8;;\x1b\\\u{1f600}"
                .as_bytes(),
        )
        .expect("advance styled Unicode fixture");

    let snapshot = engine.normalized_snapshot();
    let styled = snapshot.cell(0, 0).expect("styled cell");
    assert_eq!(styled.grapheme, "A");
    assert_eq!(styled.style.foreground, Color::Indexed(1));
    assert_eq!(styled.style.background, Color::Rgb(1, 2, 3));
    assert!(styled.style.bold);
    assert!(styled.style.dim);
    assert!(styled.style.italic);
    assert!(styled.underline());

    assert_eq!(snapshot.cell(0, 2).unwrap().grapheme, "e\u{301}");
    assert_eq!(snapshot.cell(0, 2).unwrap().width, 1);
    assert_eq!(snapshot.cell(0, 3).unwrap().grapheme, "\u{00b7}");
    assert_eq!(snapshot.cell(0, 3).unwrap().width, 1);
    assert_eq!(snapshot.cell(0, 4).unwrap().grapheme, "\u{754c}");
    assert_eq!(snapshot.cell(0, 4).unwrap().width, 2);
    assert!(snapshot.cell(0, 5).unwrap().continuation);
    assert_eq!(
        snapshot.cell(0, 4).unwrap().hyperlink.as_deref(),
        Some("https://example.test/")
    );
    assert_eq!(snapshot.cell(0, 6).unwrap().grapheme, "\u{1f600}");
    assert_eq!(snapshot.cell(0, 6).unwrap().width, 2);
    assert!(snapshot.cell(0, 7).unwrap().continuation);
}

#[test]
fn ghostty_history_is_logically_capped_and_keeps_wrap_metadata() {
    let mut engine =
        GhosttyEngine::new_with_scrollback(2, 5, 3).expect("create bounded Ghostty engine");
    engine
        .advance(b"one\r\ntwo\r\nthree\r\nabcdeF\r\nlast")
        .expect("fill history");

    assert_eq!(engine.scrollback_extent(), 3);
    let snapshot = engine
        .normalized_snapshot_with_history()
        .expect("snapshot retained history");
    assert_eq!(snapshot.scrollback_extent, 3);
    assert_eq!(snapshot.scrollback.len(), 3);
    assert!(snapshot.scrollback.iter().any(|row| row.wrapped));
    assert_eq!(snapshot.rows.last().unwrap().contents(), "last");
}

#[test]
fn ghostty_large_history_retains_the_complete_logical_window() {
    // 80 columns makes Ghostty's standard page substantially smaller than
    // Lector's logical history window. Cross two complete physical pages so
    // both byte- and line-based pruning paths have to preserve the contract.
    let mut engine = GhosttyEngine::new_with_scrollback(2, 80, 10_000)
        .expect("create large-history Ghostty engine");
    engine.advance(b"\x1b]133;A\x07x").unwrap();
    let mut fill = Vec::with_capacity(60_300);
    for _ in 0..20_100 {
        fill.extend_from_slice(b"\r\nx");
    }
    engine.advance(&fill).expect("fill large Ghostty history");

    assert_eq!(engine.scrollback_extent(), 10_000);
    engine.advance(b"\r\n\x1b]133;A\x07new prompt").unwrap();
    assert_eq!(engine.scrollback_extent(), 10_000);
    assert_eq!(
        engine
            .normalized_snapshot()
            .semantic_marks
            .last()
            .unwrap()
            .position
            .row,
        10_001
    );
}

#[test]
fn ghostty_alternate_screen_never_enters_primary_history() {
    let mut engine = GhosttyEngine::new_with_scrollback(2, 12, 20).expect("create Ghostty engine");
    engine
        .advance(b"primary one\r\nprimary two\r\nprimary three")
        .unwrap();
    let primary = engine
        .normalized_snapshot_with_history()
        .expect("primary history");

    engine
        .advance(b"\x1b[?1049halt one\r\nalt two\r\nalt three")
        .unwrap();
    let alternate = engine
        .normalized_snapshot_with_history()
        .expect("alternate history");
    assert!(alternate.alternate_screen());
    assert!(
        alternate
            .scrollback
            .iter()
            .all(|row| !row.contents().contains("primary"))
    );

    engine.advance(b"\x1b[?1049l").unwrap();
    let restored = engine
        .normalized_snapshot_with_history()
        .expect("restored primary history");
    assert!(!restored.alternate_screen());
    assert_eq!(restored.scrollback, primary.scrollback);
    assert_eq!(restored.rows, primary.rows);
}

#[test]
fn ghostty_tracked_review_mark_follows_scroll_and_resize() {
    let mut engine = GhosttyEngine::new_with_scrollback(3, 12, 100).expect("create Ghostty engine");
    engine.advance(b"anchor\r\ntwo\r\nthree").unwrap();
    let mark = engine
        .track_review_mark(HistoryPosition { row: 0, col: 0 })
        .expect("track anchor");

    engine.advance(b"\r\nfour\r\nfive").unwrap();
    assert_eq!(
        engine.review_mark_position(&mark).expect("resolve mark"),
        Some(HistoryPosition { row: 0, col: 0 })
    );

    engine.resize(3, 6).unwrap();
    let position = engine
        .review_mark_position(&mark)
        .expect("resolve mark after reflow")
        .expect("mark survives reflow");
    let snapshot = engine
        .normalized_snapshot_with_history()
        .expect("snapshot after reflow");
    let row = snapshot
        .scrollback
        .iter()
        .chain(snapshot.rows.iter())
        .nth(position.row)
        .expect("tracked row");
    assert!(row.contents().starts_with("anchor"));
}

#[test]
fn ghostty_tracked_review_mark_expires_at_the_logical_history_boundary() {
    let mut engine = GhosttyEngine::new_with_scrollback(2, 8, 2).expect("create Ghostty engine");
    engine.advance(b"anchor\r\ntwo").unwrap();
    let mark = engine
        .track_review_mark(HistoryPosition { row: 0, col: 0 })
        .expect("track anchor");

    engine.advance(b"\r\nthree\r\nfour\r\nfive\r\nsix").unwrap();
    assert_eq!(engine.scrollback_extent(), 2);
    assert_eq!(engine.review_mark_position(&mark).unwrap(), None);
}

#[test]
fn ghostty_resize_reflows_primary_and_alternate_screens_without_mixing_history() {
    let mut engine = GhosttyEngine::new_with_scrollback(3, 10, 20).expect("create Ghostty engine");
    engine.advance(b"primary-width-text\r\nsecond").unwrap();
    engine.resize(4, 6).unwrap();
    let primary = engine
        .normalized_snapshot_with_history()
        .expect("resized primary snapshot");
    assert_eq!(primary.size(), (4, 6));
    assert!(
        primary
            .scrollback
            .iter()
            .chain(primary.rows.iter())
            .any(|row| row.contents().contains("primar"))
    );

    engine.advance(b"\x1b[?1049halt-width-text").unwrap();
    engine.resize(2, 8).unwrap();
    let alternate = engine
        .normalized_snapshot_with_history()
        .expect("resized alternate snapshot");
    assert_eq!(alternate.size(), (2, 8));
    assert!(alternate.alternate_screen());
    assert!(alternate.scrollback.is_empty());

    engine.advance(b"\x1b[?1049l").unwrap();
    let restored = engine
        .normalized_snapshot_with_history()
        .expect("restored resized primary");
    assert!(!restored.alternate_screen());
    assert!(
        restored
            .scrollback
            .iter()
            .chain(restored.rows.iter())
            .all(|row| !row.contents().contains("alt"))
    );
}

#[test]
fn logical_history_origin_advances_at_the_cap_and_survives_alternate_screen() {
    let mut engine =
        GhosttyEngine::new_with_scrollback(2, 8, 2).expect("create history probe engine");
    engine.advance(b"one\r\ntwo\r\nthree").unwrap();
    let primary = engine.normalized_snapshot_with_history().unwrap();
    engine.advance(b"\x1b[?1049h").unwrap();
    let alternate = engine.normalized_snapshot_with_history().unwrap();
    assert_eq!((primary.scrollback_extent, primary.history_origin), (1, 0));
    assert_eq!(alternate.scrollback_extent, 0);
    assert_eq!(alternate.history_origin, primary.history_origin);
    assert!(alternate.scrollback.is_empty());

    engine.advance(b"\x1b[?1049l").unwrap();
    let updates: &[(&[u8], usize, &[&str])] = &[
        (b"\r\nfour", 0, &["one", "two"]),
        (b"\r\nfive", 1, &["two", "three"]),
        (b"\r\nsix", 2, &["three", "four"]),
        (b"\r\nseven", 3, &["four", "five"]),
    ];
    for (line, expected_origin, expected_history) in updates {
        engine.advance(line).unwrap();
        let snapshot = engine.normalized_snapshot_with_history().unwrap();
        assert_eq!(snapshot.scrollback_extent, 2);
        assert_eq!(snapshot.history_origin, *expected_origin);
        assert_eq!(
            snapshot
                .scrollback
                .iter()
                .map(|row| row.contents())
                .collect::<Vec<_>>(),
            *expected_history
        );
    }
}
