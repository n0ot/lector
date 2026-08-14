use lector::{
    terminal::{HistoryPosition, SemanticKind},
    view::{SCROLLBACK_LINES, View},
};

#[test]
fn osc133_phases_keep_exact_positions_and_optional_exit_status() {
    let mut view = View::new(4, 20);
    view.process_changes(
        b"\x1b]133;A\x07$ \x1b]133;B\x07echo\x1b]133;C\x07\r\nout\x1b]133;D;7\x07\r\n\x1b]133;A\x07> \x1b]133;B\x07true\x1b]133;C\x07\x1b]133;D\x07",
    );

    let marks = view.osc133_marks();
    assert_eq!(marks.len(), 8);
    assert_eq!(marks[0].kind, SemanticKind::PromptStart);
    assert_eq!(marks[0].position, pos(0, 0));
    assert_eq!(marks[1].kind, SemanticKind::InputStart);
    assert_eq!(marks[1].position, pos(0, 2));
    assert_eq!(marks[2].kind, SemanticKind::CommandStart);
    assert_eq!(marks[2].position, pos(0, 6));
    assert_eq!(
        marks[3].kind,
        SemanticKind::CommandFinished { exit_code: Some(7) }
    );
    assert_eq!(marks[3].position, pos(1, 3));
    assert_eq!(marks[4].kind, SemanticKind::PromptStart);
    assert_eq!(marks[4].position, pos(2, 0));
    assert_eq!(marks[5].kind, SemanticKind::InputStart);
    assert_eq!(marks[5].position, pos(2, 2));
    assert_eq!(marks[6].kind, SemanticKind::CommandStart);
    assert_eq!(marks[6].position, pos(2, 6));
    assert_eq!(
        marks[7].kind,
        SemanticKind::CommandFinished { exit_code: None }
    );
    assert_eq!(marks[7].position, pos(2, 6));
    assert!(marks.iter().all(|mark| !mark.alternate_screen));
}

#[test]
fn osc133_markers_move_into_scrollback_in_the_same_pty_read() {
    let mut view = View::new(2, 12);
    view.process_changes(
        b"\x1b]133;A\x07$ \x1b]133;B\x07go\x1b]133;C\x07\r\nout\x1b]133;D;0\x07\r\nnext",
    );

    assert_eq!(view.scrollback_len(), 1);
    let marks = view.osc133_marks();
    assert_eq!(marks[0].position, pos(0, 0));
    assert_eq!(marks[1].position, pos(0, 2));
    assert_eq!(marks[2].position, pos(0, 4));
    assert_eq!(marks[3].position, pos(1, 3));
    assert_eq!(view.last_submitted_input().as_deref(), Some("go"));
}

#[test]
fn osc133_history_keeps_primary_and_alternate_markers_separate() {
    let mut view = View::new(3, 12);
    view.process_changes(b"\x1b]133;A\x07primary");
    view.process_changes(b"\x1b[?1049h\x1b]133;A\x07alternate");

    assert!(view.screen().alternate_screen());
    assert_eq!(view.osc133_marks().len(), 2);
    assert!(!view.osc133_marks()[0].alternate_screen);
    assert!(view.osc133_marks()[1].alternate_screen);

    view.process_changes(b"\x1b[?1049l");
    assert!(!view.screen().alternate_screen());
    assert_eq!(view.osc133_marks()[0].position, pos(0, 0));
    assert_eq!(view.osc133_marks()[0].kind, SemanticKind::PromptStart);
}

#[test]
fn osc133_eviction_discards_old_marks_but_keeps_new_marks_from_the_same_update() {
    let mut view = View::new(2, 4);
    view.process_changes(b"\x1b]133;A\x07x");

    let mut fill = Vec::with_capacity(SCROLLBACK_LINES * 3);
    for _ in 0..=SCROLLBACK_LINES {
        fill.extend_from_slice(b"\r\nx");
    }
    view.process_changes(&fill);
    assert_eq!(view.scrollback_len(), SCROLLBACK_LINES);
    assert_eq!(view.osc133_marks().len(), 1);

    view.process_changes(b"\r\n\x1b]133;A\x07new prompt");
    assert_eq!(view.osc133_marks().len(), 1);
    assert_eq!(view.osc133_marks()[0].kind, SemanticKind::PromptStart);
}

fn pos(row: usize, col: u16) -> HistoryPosition {
    HistoryPosition { row, col }
}

#[test]
fn ghostty_preserves_exact_osc133_boundaries_from_one_fragmented_stream() {
    use lector::terminal::GhosttyEngine;

    let chunks: &[&[u8]] = &[
        b"\x1b]133;A\x1b",
        b"\\$ \x1b]133;B\x07ec",
        b"ho\x1b]133;C\x07\r\nout\x1b]133;D;17",
        b"\x07\r\n\x1b]133;A\x07$ \x1b]133;B\x07next\x1b]133;C\x07\x1b]133;D\x07",
    ];
    let mut engine = GhosttyEngine::new_with_scrollback(3, 20, 20).unwrap();
    for chunk in chunks {
        engine.advance(chunk).unwrap();
    }
    let snapshot = engine.normalized_snapshot_with_history().unwrap();
    let marks = snapshot.semantic_marks;

    assert_eq!(marks.len(), 8);
    assert_eq!(marks[0].kind, SemanticKind::PromptStart);
    assert_eq!(marks[0].position, pos(0, 0));
    assert_eq!(marks[1].kind, SemanticKind::InputStart);
    assert_eq!(marks[1].position, pos(0, 2));
    assert_eq!(marks[2].kind, SemanticKind::CommandStart);
    assert_eq!(marks[2].position, pos(0, 6));
    assert_eq!(
        marks[3].kind,
        SemanticKind::CommandFinished {
            exit_code: Some(17)
        }
    );
    assert_eq!(marks[3].position, pos(1, 3));
    assert_eq!(marks[4].position, pos(2, 0));
    assert_eq!(marks[5].position, pos(2, 2));
    assert_eq!(marks[6].position, pos(2, 6));
    assert_eq!(
        marks[7].kind,
        SemanticKind::CommandFinished { exit_code: None }
    );
    assert_eq!(marks[7].position, pos(2, 6));
}

#[test]
fn ghostty_semantic_anchors_follow_scrolling_reflow_and_eviction() {
    use lector::terminal::GhosttyEngine;

    let mut engine = GhosttyEngine::new_with_scrollback(2, 8, 2).unwrap();
    engine
        .advance(b"\x1b]133;A\x07$ \x1b]133;B\x07go\x1b]133;C\x07\r\nout\x1b]133;D;0\x07")
        .unwrap();
    engine.advance(b"\r\nnext").unwrap();
    let snapshot = engine.normalized_snapshot_with_history().unwrap();
    assert_eq!(snapshot.semantic_marks[0].position, pos(0, 0));
    assert_eq!(snapshot.semantic_marks[3].position, pos(1, 3));

    engine.resize(3, 4).unwrap();
    let snapshot = engine.normalized_snapshot_with_history().unwrap();
    assert_eq!(snapshot.semantic_marks[0].kind, SemanticKind::PromptStart);

    engine.advance(b"\r\na\r\nb\r\nc\r\nd").unwrap();
    let snapshot = engine.normalized_snapshot_with_history().unwrap();
    assert_eq!(snapshot.scrollback_extent, 2);
    assert!(snapshot.semantic_marks.is_empty());
}

#[test]
fn ghostty_semantics_survive_readline_redraw_and_screen_switching() {
    use lector::terminal::GhosttyEngine;

    let mut engine = GhosttyEngine::new_with_scrollback(3, 20, 20).unwrap();
    engine
        .advance(b"\x1b]133;A\x07$ \x1b]133;B\x07old")
        .unwrap();
    engine.advance(b"\r\x1b[K$ recalled\x1b[4D").unwrap();
    let primary = engine.normalized_snapshot();
    assert_eq!(primary.semantic_marks.len(), 2);
    assert_eq!(primary.semantic_marks[1].kind, SemanticKind::InputStart);

    engine
        .advance(b"\x1b[?1049h\x1b]133;A\x07alternate")
        .unwrap();
    let alternate = engine.normalized_snapshot();
    assert!(alternate.alternate_screen());
    assert_eq!(alternate.semantic_marks.len(), 3);
    assert!(alternate.semantic_marks[2].alternate_screen);

    engine.advance(b"\x1b[?1049l").unwrap();
    let restored = engine.normalized_snapshot();
    assert!(!restored.alternate_screen());
    assert_eq!(restored.semantic_marks[0].kind, SemanticKind::PromptStart);
    assert_eq!(restored.semantic_marks[1].kind, SemanticKind::InputStart);
}

#[test]
fn alternate_semantic_boundaries_follow_ghosttys_rendered_prompt() {
    use lector::terminal::GhosttyEngine;

    let setup = b"one\r\ntwo\r\nthree";
    let alternate = b"\x1b[?1049h\x1b]133;A\x07alt";

    let mut ghostty = GhosttyEngine::new_with_scrollback(3, 12, 20).unwrap();
    ghostty.advance(setup).unwrap();
    ghostty.advance(alternate).unwrap();
    let snapshot = ghostty.normalized_snapshot();
    // Ghostty follows xterm's 1049 behavior by copying the primary cursor to
    // the cleared alternate screen, so the semantic anchor remains exact for
    // the row where Ghostty actually rendered the prompt.
    assert_eq!(snapshot.semantic_marks[0].position, pos(2, 0));
    assert_eq!(snapshot.rows[2].contents(), "alt");
}

#[test]
fn ghostty_semantic_history_is_fragmentation_invariant() {
    use lector::terminal::GhosttyEngine;

    let chunks: &[&[u8]] = &[
        b"\x1b]133;A\x07$ ",
        b"\x1b]133;B\x07echo one",
        b"\x1b]133;C\x07\r\nout\x1b]133;D;0\x07",
        b"\r\n\x1b]133;A\x07$ \x1b]133;B\x07echo two",
        b"\r\x1b[K$ recalled",
        b"\r\n\x1b]133;C\x07done\x1b]133;D\x07",
    ];
    let mut one_shot = GhosttyEngine::new_with_scrollback(3, 20, 20).unwrap();
    one_shot.advance(&chunks.concat()).unwrap();
    let expected = one_shot.normalized_snapshot_with_history().unwrap();

    let mut fragmented = GhosttyEngine::new_with_scrollback(3, 20, 20).unwrap();
    for chunk in chunks {
        fragmented.advance(chunk).unwrap();
    }
    assert_eq!(
        fragmented.normalized_snapshot_with_history().unwrap(),
        expected
    );
}
