use lector::terminal::{
    Cell, Color, Cursor, GhosttyEngine, MouseEncoding, MouseProtocol, PrintBoundary, PrintedRun,
    ScreenIdentity, Style, TerminalDamage, TerminalEffects, TerminalEngine, TerminalModes,
    TerminalOperation, TerminalSnapshot, UpdateSummary, Viewport,
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
            visible: true,
            ..Cursor::default()
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
fn osc133_input_boundary_is_exact_across_visible_suffixes_and_fragmentation() {
    let prompt = b"\x1b]133;A\x07$ \x1b]133;B\x07";

    let mut exact_engine = engine(3, 40, 20);
    let exact = exact_engine.advance(prompt);
    assert!(exact.semantic_input_boundary);

    let mut neutral_engine = engine(3, 40, 20);
    let neutral = neutral_engine
        .advance(b"\x1b]133;A\x07$ \x1b]133;B\x07\x1b[32m\x1b[?2004h\x1b[5 q\x1b]2;cwd\x07");
    assert!(neutral.semantic_input_boundary);

    let mut same_read_engine = engine(3, 40, 20);
    let same_read = same_read_engine.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07partial");
    assert!(!same_read.semantic_input_boundary);

    let mut fragmented_engine = engine(3, 40, 20);
    let mut fragmented = UpdateSummary::default();
    for chunk in [
        b"\x1b]133;A\x07$ \x1b]133;".as_slice(),
        b"B\x07pa".as_slice(),
        b"rtial".as_slice(),
    ] {
        fragmented.merge(fragmented_engine.advance(chunk));
    }
    assert!(!fragmented.semantic_input_boundary);

    let mut continuation_engine = engine(3, 40, 20);
    let continuation = continuation_engine.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07\x1b[");
    assert!(continuation.parser_continuation);
    assert!(!continuation.semantic_input_boundary);
}

#[test]
fn terminal_updates_are_invariant_to_byte_fragmentation() {
    let bytes = b"first red line\r\nsecond line";
    let mut whole_engine = engine(4, 40, 20);
    let whole = whole_engine.advance(bytes);

    let mut fragmented_engine = engine(4, 40, 20);
    let mut fragmented = UpdateSummary::default();
    for byte in bytes {
        fragmented.merge(fragmented_engine.advance(std::slice::from_ref(byte)));
    }

    assert!(!whole.operations.is_empty());
    assert_eq!(fragmented_engine.snapshot(), whole_engine.snapshot());
    assert_eq!(fragmented.printed_text(), whole.printed_text());

    // Batch count and raw print-run segmentation intentionally record parser
    // calls. Their normalized text and every renderer-facing field must be
    // independent of chunking.
    fragmented.batch_count = whole.batch_count;
    fragmented.printed_runs.clone_from(&whole.printed_runs);
    assert_eq!(fragmented, whole);
}

#[test]
fn safe_suffix_after_a_structural_boundary_is_invariant_to_byte_fragmentation() {
    let bytes = b"\x1b[Hfirst\r\nsecond\r\n";
    let mut whole_engine = engine(4, 40, 20);
    let whole = whole_engine.advance(bytes);

    let mut fragmented_engine = engine(4, 40, 20);
    let mut fragmented = UpdateSummary::default();
    for byte in bytes {
        fragmented.merge(fragmented_engine.advance(std::slice::from_ref(byte)));
    }

    assert_eq!(fragmented_engine.snapshot(), whole_engine.snapshot());
    assert_eq!(fragmented.linear_output_effect, whole.linear_output_effect);
    assert!(whole.completes_linear_output_record());
    let mut text = String::new();
    whole
        .linear_output_text_into(&mut text)
        .expect("retain output after the structural boundary");
    assert_eq!(text, "first\nsecond\n");
}

#[test]
fn ambiguous_mid_record_suffix_is_invariant_to_byte_fragmentation() {
    let bytes = b"item\tvalue\r\n";
    let mut whole_engine = engine(4, 40, 20);
    let whole = whole_engine.advance(bytes);

    let mut fragmented_engine = engine(4, 40, 20);
    let mut fragmented = UpdateSummary::default();
    for byte in bytes {
        fragmented.merge(fragmented_engine.advance(std::slice::from_ref(byte)));
    }

    assert_eq!(fragmented_engine.snapshot(), whole_engine.snapshot());
    assert_eq!(fragmented.linear_output_effect, whole.linear_output_effect);
    assert!(!whole.completes_linear_output_record());
}

fn synthetic_fragment(index: usize) -> UpdateSummary {
    let col = u16::try_from(index).expect("fragment column fits in u16");
    let changed_row = u16::try_from(index % 64).expect("fragment row fits in u16");
    let changed_rows = std::iter::once(changed_row..=changed_row).collect::<Vec<_>>();
    let text = if index.is_multiple_of(2) { "x" } else { "y" };
    UpdateSummary {
        operations: vec![TerminalOperation::WriteRun {
            row: 3,
            col,
            text: text.to_owned(),
        }],
        changed_rows: changed_rows.clone(),
        damage: TerminalDamage::Rows(changed_rows),
        cursor_before: Cursor {
            row: 3,
            col,
            ..Cursor::default()
        },
        cursor_after: Cursor {
            row: 3,
            col: col + 1,
            ..Cursor::default()
        },
        batch_count: 1,
        ..UpdateSummary::default()
    }
}

#[test]
fn large_fragmented_summary_merge_is_grouping_invariant_and_exact() {
    const FRAGMENTS: usize = 16_384;
    const GROUP_SIZE: usize = 257;

    let mut sequential = UpdateSummary::default();
    for index in 0..FRAGMENTS {
        sequential.merge(synthetic_fragment(index));
    }

    let mut grouped = UpdateSummary::default();
    for start in (0..FRAGMENTS).step_by(GROUP_SIZE) {
        let mut group = UpdateSummary::default();
        for index in start..FRAGMENTS.min(start + GROUP_SIZE) {
            group.merge(synthetic_fragment(index));
        }
        grouped.merge(group);
    }

    assert_eq!(grouped, sequential);
    let expected_text: String = (0..FRAGMENTS)
        .map(|index| if index.is_multiple_of(2) { 'x' } else { 'y' })
        .collect();
    let mut unfragmented = UpdateSummary::default();
    unfragmented.merge(UpdateSummary {
        operations: vec![TerminalOperation::WriteRun {
            row: 3,
            col: 0,
            text: expected_text.clone(),
        }],
        batch_count: 1,
        ..UpdateSummary::default()
    });
    assert_eq!(
        unfragmented.operations, sequential.operations,
        "canonical run boundaries must not depend on PTY fragmentation"
    );
    const MAX_RUN_COLUMNS: usize = 256;
    assert_eq!(
        sequential.operations.len(),
        FRAGMENTS.div_ceil(MAX_RUN_COLUMNS),
        "fragmented public runs must remain bounded instead of rescanning one growing String"
    );
    for (index, operation) in sequential.operations.iter().enumerate() {
        let start = index * MAX_RUN_COLUMNS;
        let end = FRAGMENTS.min(start + MAX_RUN_COLUMNS);
        assert_eq!(
            operation,
            &TerminalOperation::WriteRun {
                row: 3,
                col: start as u16,
                text: expected_text[start..end].to_owned(),
            }
        );
    }

    let expected_rows = std::iter::once(0..=63).collect::<Vec<_>>();
    assert_eq!(sequential.changed_rows, expected_rows);
    assert_eq!(sequential.damage, TerminalDamage::Rows(expected_rows));
    assert_eq!(sequential.batch_count, FRAGMENTS);
    assert_eq!(sequential.cursor_before.col, 0);
    assert_eq!(sequential.cursor_after.col, FRAGMENTS as u16);
}

#[test]
fn manually_constructed_unicode_write_runs_keep_character_column_semantics() {
    let mut summary = UpdateSummary {
        operations: vec![TerminalOperation::WriteRun {
            row: 0,
            col: 0,
            text: "é".to_owned(),
        }],
        batch_count: 1,
        ..UpdateSummary::default()
    };
    summary.merge(UpdateSummary {
        operations: vec![TerminalOperation::WriteRun {
            row: 0,
            col: 1,
            text: "x".to_owned(),
        }],
        batch_count: 1,
        ..UpdateSummary::default()
    });

    assert_eq!(
        summary.operations,
        vec![TerminalOperation::WriteRun {
            row: 0,
            col: 0,
            text: "éx".to_owned(),
        }]
    );
}

fn range_summary(ranges: Vec<std::ops::RangeInclusive<u16>>) -> UpdateSummary {
    UpdateSummary {
        changed_rows: ranges.clone(),
        damage: TerminalDamage::Rows(ranges),
        batch_count: 1,
        ..UpdateSummary::default()
    }
}

#[test]
fn row_range_merges_are_normalized_and_grouping_invariant() {
    let first = range_summary(vec![8..=9, 2..=3, 4..=6]);
    let second = range_summary(vec![12..=12, 0..=2, 7..=10]);
    let third = range_summary(vec![15..=17, 11..=13]);

    let mut sequential = first.clone();
    sequential.merge(second.clone());
    sequential.merge(third.clone());

    let mut tail = second;
    tail.merge(third);
    let mut grouped = first;
    grouped.merge(tail);

    assert_eq!(grouped, sequential);
    assert_eq!(sequential.changed_rows, vec![0..=13, 15..=17]);
    assert_eq!(
        sequential.damage,
        TerminalDamage::Rows(vec![0..=13, 15..=17])
    );
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
fn linear_output_completion_survives_fragmented_crlf_and_rejects_ambiguity() {
    let mut engine = engine(4, 20, 20);

    let mut fragmented = engine.advance(b"complete line\r");
    assert!(!fragmented.completes_linear_output_record());
    fragmented.merge(engine.advance(b"\n"));
    assert!(fragmented.completes_linear_output_record());
    assert_eq!(fragmented.printed_text(), "complete line\n");

    let styled = engine.advance(b"styled\x1b[31m line\x1b[0m\r\n\x1b[32m");
    assert!(styled.completes_linear_output_record());

    let trailing = engine.advance(b"complete\r\npartial");
    assert!(!trailing.completes_linear_output_record());

    let overwrite = engine.advance(b"10%\r100%\r\n");
    assert!(!overwrite.completes_linear_output_record());
    assert_eq!(overwrite.printed_text(), "100%\n");

    let readline = engine.advance(b"\x1b[?2004h\r\n\x1b[?2004l\redbrowse line\r\n\x1b[?2004h");
    assert!(!readline.output_report_structural);
    assert!(readline.completes_linear_output_record());
    assert_eq!(readline.printed_text(), "edbrowse line\n");

    let mut mixed_mode_engine =
        GhosttyEngine::new_with_scrollback(4, 20, 20).expect("create mixed-mode engine");
    let mixed_mode = TerminalEngine::advance(&mut mixed_mode_engine, b"\x1b[?2004;1004hrecord\r\n");
    assert!(mixed_mode.output_report_structural);
    assert!(!mixed_mode.completes_linear_output_record());

    let addressed = engine.advance(b"\x1b[Hscreen row\r\n");
    assert!(addressed.output_report_structural);
    assert!(addressed.completes_linear_output_record());

    let incomplete_escape = engine.advance(b"record\r\n\x1b[");
    assert!(incomplete_escape.parser_continuation);
    assert!(!incomplete_escape.completes_linear_output_record());

    let kitty_apc = b"record\r\n\x1b_Ga=q,i=1\x1b\\";
    for split in 1..kitty_apc.len() {
        let mut apc_engine =
            GhosttyEngine::new_with_scrollback(4, 20, 20).expect("create APC engine");
        let mut fragmented_apc = TerminalEngine::advance(&mut apc_engine, &kitty_apc[..split]);
        fragmented_apc.merge(TerminalEngine::advance(
            &mut apc_engine,
            &kitty_apc[split..],
        ));
        assert!(
            fragmented_apc.output_report_structural,
            "Kitty APC was not structural at split {split}"
        );
        assert!(!fragmented_apc.completes_linear_output_record());
    }

    let mut c1_apc_engine =
        GhosttyEngine::new_with_scrollback(4, 20, 20).expect("create C1 APC engine");
    let c1_apc = TerminalEngine::advance(&mut c1_apc_engine, b"record\r\n\x9fGa=q,i=1\x9c");
    assert!(c1_apc.output_report_structural);
    assert!(!c1_apc.completes_linear_output_record());

    let mut invalid_utf8_engine =
        GhosttyEngine::new_with_scrollback(4, 20, 20).expect("create invalid UTF-8 engine");
    let invalid_utf8_c1 =
        TerminalEngine::advance(&mut invalid_utf8_engine, b"record\r\n\xe0\x9fGa=q,i=1\x9c");
    assert!(
        invalid_utf8_c1.output_report_structural,
        "an invalid UTF-8 prefix must not hide a raw C1 APC introducer"
    );
    assert!(!invalid_utf8_c1.completes_linear_output_record());

    let unicode_record = "Straße\r\n".as_bytes();
    for split in 1..unicode_record.len() {
        let mut unicode_engine =
            GhosttyEngine::new_with_scrollback(4, 20, 20).expect("create Unicode engine");
        let mut fragmented_unicode =
            TerminalEngine::advance(&mut unicode_engine, &unicode_record[..split]);
        fragmented_unicode.merge(TerminalEngine::advance(
            &mut unicode_engine,
            &unicode_record[split..],
        ));
        assert!(
            !fragmented_unicode.output_report_structural,
            "UTF-8 continuation byte was mistaken for C1 APC at split {split}"
        );
        assert!(
            fragmented_unicode.completes_linear_output_record(),
            "Unicode record did not complete at split {split}: {fragmented_unicode:?}"
        );
    }

    let alternate = engine.advance(b"\x1b[?1049hrecord\r\n");
    assert!(!alternate.completes_linear_output_record());
    let synchronized = engine.advance(b"\x1b[?1049l\x1b[?2026hrecord\r\n\x1b[?2026l");
    assert!(!synchronized.completes_linear_output_record());
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
fn content_extraction_preserves_wrapping_ranges_and_full_row_trimming() {
    let mut engine = engine(3, 5, 4);
    engine.advance(b"abc  X");
    let snapshot = engine.snapshot();

    assert!(snapshot.row_wrapped(0));
    assert_eq!(snapshot.contents(), "abc  X");
    assert_eq!(snapshot.contents_between(0, 3, 1, 1), "  X");
    assert_eq!(snapshot.contents_full(), "abc\nX\n\n");
}

#[test]
fn visible_content_predicate_matches_trimmed_contents_without_allocating_a_screen() {
    let mut engine = engine(2, 8, 4);

    engine.advance(" \t\u{00a0}".as_bytes());
    let whitespace = engine.snapshot();
    assert!(whitespace.contents().trim().is_empty());
    assert!(!whitespace.has_visible_non_whitespace_content());

    engine.advance(b"x");
    let content = engine.snapshot();
    assert!(!content.contents().trim().is_empty());
    assert!(content.has_visible_non_whitespace_content());
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
