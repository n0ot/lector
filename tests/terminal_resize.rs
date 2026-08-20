use lector::{
    terminal::{GhosttyEngine, TerminalEngine, TerminalGeometry},
    view::View,
};

#[test]
fn engine_geometry_keeps_cells_and_pixels_separate() {
    let mut engine = GhosttyEngine::new_with_scrollback(2, 8, 20).expect("create Ghostty engine");
    let geometry = TerminalGeometry::new(4, 12, 9, 18);

    TerminalEngine::resize_with_geometry(&mut engine, geometry);

    assert_eq!(engine.snapshot().geometry, geometry);
    assert_eq!(engine.snapshot().size(), (4, 12));
    assert_eq!(engine.snapshot().geometry.width_px(), 108);
    assert_eq!(engine.snapshot().geometry.height_px(), 72);
}

#[test]
fn resize_does_not_discard_fragmented_utf8_or_control_sequences() {
    let mut view = View::new(2, 12);
    view.process_changes(b"\xe7\x95");
    view.set_size_with_geometry(TerminalGeometry::new(3, 10, 8, 16));
    view.process_changes(b"\x8c\x1b]2;res");
    view.set_size(4, 16);
    view.process_changes(b"ized\x07done");

    assert!(view.screen().contents().contains("\u{754c}done"));
    assert_eq!(view.screen().title.as_deref(), Some("resized"));
    assert_eq!(view.screen().geometry, TerminalGeometry::from_cells(4, 16));
}

#[test]
fn zero_sized_startup_clamps_until_valid_geometry_arrives() {
    let mut view = View::new(0, 0);
    assert_eq!(view.size(), (1, 1));

    let geometry = TerminalGeometry::new(24, 80, 10, 20);
    view.set_size_with_geometry(geometry);

    assert_eq!(view.size(), (24, 80));
    assert_eq!(view.screen().geometry, geometry);
}

mod ghostty {
    use lector::terminal::{Color, GhosttyEngine, TerminalGeometry};

    #[test]
    fn ghostty_resize_tracks_pixel_geometry() {
        let mut engine = GhosttyEngine::new(2, 8).expect("create Ghostty engine");
        let geometry = TerminalGeometry::new(4, 12, 9, 18);

        engine
            .resize_with_geometry(geometry)
            .expect("resize Ghostty engine");

        assert_eq!(engine.normalized_snapshot().geometry, geometry);
        assert_eq!(engine.ghostty_snapshot().width_px, 108);
        assert_eq!(engine.ghostty_snapshot().height_px, 72);
    }

    #[test]
    fn repeated_rapid_resize_keeps_the_last_geometry_and_live_parser() {
        let mut engine = GhosttyEngine::new(1, 1).expect("create Ghostty engine");
        engine.advance(b"\xe7\x95").expect("write UTF-8 prefix");

        for geometry in [
            TerminalGeometry::new(2, 7, 7, 13),
            TerminalGeometry::new(40, 120, 10, 20),
            TerminalGeometry::new(3, 9, 8, 16),
            TerminalGeometry::new(24, 80, 9, 18),
        ] {
            engine.resize_with_geometry(geometry).expect("rapid resize");
        }
        engine.advance(b"\x8c").expect("finish UTF-8");

        let snapshot = engine.normalized_snapshot();
        assert_eq!(snapshot.geometry, TerminalGeometry::new(24, 80, 9, 18));
        assert_eq!(snapshot.cell(0, 0).unwrap().grapheme, "\u{754c}");
    }

    #[test]
    fn diagnostic_snapshot_restores_unfinished_utf8_and_vt_continuations() {
        let cases: &[(&[u8], &[u8], &str)] =
            &[(b"\xe7\x95", b"\x8c", "\u{754c}"), (b"\x1b[31", b"mR", "R")];

        for (prefix, suffix, expected) in cases {
            let mut source = GhosttyEngine::new(2, 12).expect("create source engine");
            source
                .resize_with_geometry(TerminalGeometry::new(2, 12, 9, 18))
                .expect("set source geometry");
            source.advance(prefix).expect("write unfinished prefix");
            let diagnostic = source
                .diagnostic_snapshot()
                .expect("encode diagnostic snapshot");
            let mut restored = GhosttyEngine::restore_diagnostic_snapshot(diagnostic)
                .expect("restore diagnostic snapshot");

            restored.advance(suffix).expect("finish restored input");
            let snapshot = restored.normalized_snapshot();
            assert_eq!(snapshot.geometry, TerminalGeometry::new(2, 12, 9, 18));
            assert_eq!(snapshot.cell(0, 0).unwrap().grapheme, *expected);
            if *expected == "R" {
                assert_eq!(
                    snapshot.cell(0, 0).unwrap().style.foreground,
                    Color::Indexed(1)
                );
            }
        }
    }

    #[test]
    fn diagnostic_snapshot_preserves_visible_scrollback_and_screen_state() {
        let mut source = GhosttyEngine::new_with_scrollback(2, 8, 10).unwrap();
        source
            .resize_with_geometry(TerminalGeometry::new(2, 8, 9, 18))
            .unwrap();
        source
            .advance(b"\x1b]2;snapshot\x07\x1b[?2004h\x1b[31mone\x1b[0m\r\ntwo\r\nthree\r\nfour")
            .unwrap();
        let expected = source.normalized_snapshot_with_history().unwrap();

        let diagnostic = source.diagnostic_snapshot().unwrap();
        let restored = GhosttyEngine::restore_diagnostic_snapshot(diagnostic).unwrap();
        let actual = restored.normalized_snapshot_with_history().unwrap();

        assert_eq!(actual, expected);
        assert_eq!(actual.geometry, TerminalGeometry::new(2, 8, 9, 18));
        assert!(!actual.scrollback.is_empty());
        assert_eq!(actual.title.as_deref(), Some("snapshot"));
        assert!(actual.bracketed_paste());
    }

    #[test]
    fn primary_reflows_but_alternate_content_is_not_added_to_history() {
        let mut engine =
            GhosttyEngine::new_with_scrollback(3, 12, 30).expect("create Ghostty engine");
        engine
            .advance(b"primary-long-line\r\nsecond\r\nthird")
            .unwrap();
        engine.resize(4, 6).unwrap();
        let primary = engine.normalized_snapshot_with_history().unwrap();
        assert!(
            primary
                .scrollback
                .iter()
                .chain(primary.rows.iter())
                .filter(|row| row.wrapped)
                .count()
                > 0
        );

        engine
            .advance(b"\x1b[?1049h\x1b[2J\x1b[Halt-long-line")
            .unwrap();
        engine.resize(2, 8).unwrap();
        let alternate = engine.normalized_snapshot_with_history().unwrap();
        assert!(alternate.alternate_screen());
        assert!(alternate.scrollback.is_empty());
        assert!(alternate.rows.iter().all(|row| !row.wrapped));

        engine.advance(b"\x1b[?1049l").unwrap();
        let restored = engine.normalized_snapshot_with_history().unwrap();
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
    fn primary_resize_uses_ghostty_reflow_as_authoritative() {
        let mut ghostty = GhosttyEngine::new_with_scrollback(3, 12, 30).unwrap();
        let input = b"abcdefghijklmnop\r\nsecond";
        ghostty.advance(input).unwrap();

        let geometry = TerminalGeometry::new(4, 6, 9, 18);
        ghostty.resize_with_geometry(geometry).unwrap();

        let snapshot = ghostty.normalized_snapshot_with_history().unwrap();
        assert_eq!(snapshot.geometry, geometry);
        assert!(
            snapshot
                .rows
                .iter()
                .chain(&snapshot.scrollback)
                .any(|row| row.wrapped)
        );
        let text = snapshot
            .scrollback
            .iter()
            .chain(snapshot.rows.iter())
            .map(|row| row.contents())
            .collect::<String>();
        assert!(text.contains("abcdef"));
        assert!(text.contains("second"));
    }

    #[test]
    fn alternate_screen_resize_crops_to_the_authoritative_grid() {
        let mut ghostty = GhosttyEngine::new_with_scrollback(3, 12, 30).unwrap();
        ghostty.advance(b"\x1b[?1049h\x1b[Halternate").unwrap();
        ghostty.resize(4, 8).unwrap();

        let snapshot = ghostty.normalized_snapshot();
        assert!(snapshot.alternate_screen());
        assert_eq!(snapshot.size(), (4, 8));
        assert_eq!(snapshot.rows[0].contents(), "alternat");
    }
}
