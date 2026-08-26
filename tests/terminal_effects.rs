use lector::terminal::{
    ClipboardContent, ClipboardLocation, GhosttyEngine, ProgressState, TerminalEngine,
    TerminalEvent, TerminalQuery,
};

fn engine(rows: u16, cols: u16, scrollback: usize) -> Box<dyn TerminalEngine> {
    Box::new(
        GhosttyEngine::new_with_scrollback(rows, cols, scrollback)
            .expect("create Ghostty terminal engine"),
    )
}

fn effect_stream() -> Vec<&'static [u8]> {
    vec![
        b"\x07\x1B]2;build title\x07\x1B]7;file://host/tmp/project\x1B\\",
        b"\x1B]52;c;aGVsbG8Ad29ybGQ=\x1B\\",
        b"\x1B]1337;Copy=:aVRlcm0y\x1B\\",
        b"\x1B]777;notify;Build;Needs attention\x1B\\",
        b"\x1B]9;4;1;42\x07\x05\x1B[>q\x1B[18t",
        b"\x1B[?996n\x1B[c\x1B_unknown payload\x1B\\",
    ]
}

fn expected_effects() -> Vec<TerminalEvent> {
    vec![
        TerminalEvent::Bell,
        TerminalEvent::TitleChanged("build title".into()),
        TerminalEvent::WorkingDirectoryChanged("file://host/tmp/project".into()),
        TerminalEvent::ClipboardWrite {
            location: ClipboardLocation::Standard,
            contents: vec![ClipboardContent {
                mime: "text/plain".into(),
                data: b"hello\0world".to_vec(),
            }],
        },
        TerminalEvent::ClipboardWrite {
            location: ClipboardLocation::Standard,
            contents: vec![ClipboardContent {
                mime: "text/plain".into(),
                data: b"iTerm2".to_vec(),
            }],
        },
        TerminalEvent::DesktopNotification {
            title: "Build".into(),
            body: "Needs attention".into(),
        },
        TerminalEvent::ProgressReport {
            state: ProgressState::Set,
            progress: Some(42),
        },
        TerminalEvent::Query(TerminalQuery::Enquiry),
        TerminalEvent::Query(TerminalQuery::XtVersion),
        TerminalEvent::Query(TerminalQuery::Size),
        TerminalEvent::Query(TerminalQuery::ColorScheme),
        TerminalEvent::Query(TerminalQuery::DeviceAttributes),
        TerminalEvent::UnknownSequence {
            content: b"unknown payload".to_vec(),
            truncated: false,
        },
    ]
}

#[test]
fn ghostty_preserves_typed_event_order_and_reported_values() {
    let mut engine = engine(4, 40, 100);
    let mut actual = Vec::new();
    for chunk in effect_stream() {
        actual.extend(engine.advance(chunk).effects.events);
    }

    assert_eq!(actual, expected_effects());
    assert_eq!(engine.snapshot().title.as_deref(), Some("build title"));
    assert_eq!(
        engine.snapshot().working_directory.as_deref(),
        Some("file://host/tmp/project")
    );
}

#[test]
fn mode_observation_tracks_focus_mouse_paste_and_kitty_keyboard_stack() {
    let mut engine = engine(4, 40, 100);
    engine.advance(b"\x1B[?1004h\x1B[?1002h\x1B[?1006h\x1B[?2004h\x1B[>3u\x1B[=4;2u");
    let modes = engine.snapshot().modes.clone();
    assert!(modes.focus_reporting);
    assert!(modes.bracketed_paste);
    assert_eq!(modes.kitty_keyboard_flags, 7);
    assert_ne!(modes.mouse_protocol, lector::terminal::MouseProtocol::None);
    assert_eq!(modes.mouse_encoding, lector::terminal::MouseEncoding::Sgr);

    engine.advance(b"\x1B[<u\x1B[?1004l\x1B[?1002l\x1B[?1006l\x1B[?2004l");
    let modes = &engine.snapshot().modes;
    assert!(!modes.focus_reporting);
    assert!(!modes.bracketed_paste);
    assert_eq!(modes.kitty_keyboard_flags, 0);
    assert_eq!(modes.mouse_protocol, lector::terminal::MouseProtocol::None);
}

#[test]
fn kitty_keyboard_flags_follow_the_active_screen_stack() {
    let mut engine = engine(4, 40, 100);
    engine.advance(b"\x1B[>3u");
    assert_eq!(engine.snapshot().kitty_keyboard_flags(), 3);

    engine.advance(b"\x1B[?1049h");
    assert_eq!(engine.snapshot().kitty_keyboard_flags(), 0);
    engine.advance(b"\x1B[>5u");
    assert_eq!(engine.snapshot().kitty_keyboard_flags(), 5);

    engine.advance(b"\x1B[?1049l");
    assert_eq!(engine.snapshot().kitty_keyboard_flags(), 3);
    engine.advance(b"\x1B[?1049h");
    assert_eq!(engine.snapshot().kitty_keyboard_flags(), 5);
}

#[test]
fn unknown_sequences_are_fragment_safe_bounded_and_abortable() {
    let mut engine = engine(2, 20, 10);
    assert!(engine.advance(b"\x1B_fragmented").effects.events.is_empty());
    let completed = engine.advance(b" payload\x1B\\");
    assert_eq!(
        completed.effects.events,
        [TerminalEvent::UnknownSequence {
            content: b"fragmented payload".to_vec(),
            truncated: false,
        }]
    );

    let mut oversized = b"\x1B_".to_vec();
    oversized.extend(std::iter::repeat_n(b'x', 5_000));
    oversized.extend_from_slice(b"\x1B\\");
    let update = engine.advance(&oversized);
    assert_eq!(
        update.effects.events,
        [TerminalEvent::UnknownSequence {
            content: vec![b'x'; 4_096],
            truncated: true,
        }]
    );

    assert!(
        engine
            .advance(b"\x1B_aborted\x18")
            .effects
            .events
            .is_empty()
    );
}

#[test]
fn terminal_reset_boundaries_are_exact() {
    let mut engine = engine(2, 20, 10);
    assert!(!engine.advance(b"\x1B[0m\x1B[2J").terminal_reset);
    assert!(engine.advance(b"\x1B[!p").terminal_reset);
    assert!(engine.advance(b"\x1Bc").terminal_reset);
}

#[test]
fn malformed_and_incomplete_effects_do_not_escape_as_partial_events() {
    let mut engine = engine(2, 20, 10);
    for (index, malformed) in [
        b"\x1B]52;c;%%%\x1B\\".as_slice(),
        b"\x1B]52;c;YQ==;ignored\x1B\\",
        b"\x1B]1337;Copy=:YQ==;ignored\x1B\\",
        b"\x1B]1337;CurrentDir=\x1B\\",
        b"\x1B]777;notify;title\x1B\\",
        b"\x1B]777;other;title;body\x1B\\",
    ]
    .into_iter()
    .enumerate()
    {
        let events = engine.advance(malformed).effects.events;
        assert!(events.is_empty(), "malformed case {index}: {events:?}");
    }

    let clipboard_query = engine.advance(b"\x1B]52;c;?\x1B\\");
    assert_eq!(
        clipboard_query.effects.events,
        [TerminalEvent::Query(TerminalQuery::Clipboard)]
    );
    assert_eq!(clipboard_query.pty_replies, b"\x1B]52;c;\x1B\\");

    // OSC 9 is also Ghostty's desktop-notification protocol. An invalid
    // progress subcommand is a complete notification and must not be dropped.
    assert_eq!(
        engine.advance(b"\x1B]9;4;9;50\x1B\\").effects.events,
        [TerminalEvent::DesktopNotification {
            title: String::new(),
            body: "4;9;50".into(),
        }]
    );

    assert!(
        engine
            .advance(b"\x1B]777;notify;split;")
            .effects
            .events
            .is_empty()
    );
    assert_eq!(
        engine.advance(b"body\x07").effects.events,
        [TerminalEvent::DesktopNotification {
            title: "split".into(),
            body: "body".into(),
        }]
    );
}

#[test]
fn progress_reports_preserve_ghostty_defaults_clamping_and_missing_values() {
    let mut engine = engine(2, 20, 10);
    let mut events = Vec::new();
    for sequence in [
        b"\x1B]9;4;0;50\x1B\\".as_slice(),
        b"\x1B]9;4;1\x1B\\",
        b"\x1B]9;4;1;150\x1B\\",
        b"\x1B]9;4;1;bad\x1B\\",
        b"\x1B]9;4;2;9\x1B\\",
        b"\x1B]9;4;3;50\x1B\\",
        b"\x1B]9;4;4\x1B\\",
    ] {
        events.extend(engine.advance(sequence).effects.events);
    }
    assert_eq!(
        events,
        [
            TerminalEvent::ProgressReport {
                state: ProgressState::Remove,
                progress: None,
            },
            TerminalEvent::ProgressReport {
                state: ProgressState::Set,
                progress: Some(0),
            },
            TerminalEvent::ProgressReport {
                state: ProgressState::Set,
                progress: Some(100),
            },
            TerminalEvent::ProgressReport {
                state: ProgressState::Set,
                progress: None,
            },
            TerminalEvent::ProgressReport {
                state: ProgressState::Error,
                progress: Some(9),
            },
            TerminalEvent::ProgressReport {
                state: ProgressState::Indeterminate,
                progress: None,
            },
            TerminalEvent::ProgressReport {
                state: ProgressState::Pause,
                progress: None,
            },
        ]
    );
}

#[test]
fn clipboard_writes_accept_ghostty_base64_variants_and_reads_are_brokered() {
    let mut engine = engine(2, 20, 10);
    let mut events = Vec::new();
    for sequence in [
        b"\x1B]52;c;YQ==\x1B\\".as_slice(),
        b"\x1B]52;s;YWI=\x1B\\",
        b"\x1B]52;p;YWJj\x1B\\",
        b"\x1B]52;c;YQ\x1B\\",
        b"\x1B]52;c;YWI\x1B\\",
        b"\x1B]1337;Copy=:aVRlcm0y\x1B\\",
        b"\x1B]52;c;?\x1B\\",
        b"\x1B]52;c;A\x1B\\",
        b"\x1B]52;c;Y=Q\x1B\\",
    ] {
        events.extend(engine.advance(sequence).effects.events);
    }

    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, TerminalEvent::Query(TerminalQuery::Clipboard)))
            .count(),
        1
    );
    let writes = events
        .into_iter()
        .filter_map(|event| match event {
            TerminalEvent::ClipboardWrite { location, contents } => {
                Some((location, contents.into_iter().next().unwrap().data))
            }
            TerminalEvent::Query(TerminalQuery::Clipboard) => None,
            other => panic!("unexpected event: {other:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        writes,
        [
            (ClipboardLocation::Standard, b"a".to_vec()),
            (ClipboardLocation::Selection, b"ab".to_vec()),
            (ClipboardLocation::Primary, b"abc".to_vec()),
            (ClipboardLocation::Standard, b"a".to_vec()),
            (ClipboardLocation::Standard, b"ab".to_vec()),
            (ClipboardLocation::Standard, b"iTerm2".to_vec()),
        ]
    );
}

#[test]
fn supported_and_incomplete_apcs_are_not_reported_as_unknown() {
    let mut engine = engine(2, 20, 10);
    for sequence in [
        b"\x1B_Ga=t,t=d,f=24,i=1,s=1,v=2,c=10,r=1;////////\x1B\\".as_slice(),
        b"\x1B_25a1;garbage\x1B\\",
        b"\x1B_25a\x1B\\",
        b"\x1B_\x1B\\",
    ] {
        assert!(engine.advance(sequence).effects.events.is_empty());
    }
}

#[test]
fn fragmented_queries_emit_only_after_their_final_byte() {
    let mut engine = engine(2, 20, 10);
    let mut events = Vec::new();
    for byte in b"\x05\x1B[>q\x1B[18t\x1B[?996n\x1B[c" {
        events.extend(engine.advance(&[*byte]).effects.events);
    }
    assert_eq!(
        events,
        [
            TerminalEvent::Query(TerminalQuery::Enquiry),
            TerminalEvent::Query(TerminalQuery::XtVersion),
            TerminalEvent::Query(TerminalQuery::Size),
            TerminalEvent::Query(TerminalQuery::ColorScheme),
            TerminalEvent::Query(TerminalQuery::DeviceAttributes),
        ]
    );
}

#[test]
fn every_exposed_size_and_device_query_form_is_typed_in_order() {
    let mut engine = engine(2, 20, 10);
    let events = engine
        .advance(b"\x1B[14t\x1B[16t\x1B[18t\x1B[c\x1B[>c\x1B[=c\x1B[?996n")
        .effects
        .events;
    assert_eq!(
        events,
        [
            TerminalEvent::Query(TerminalQuery::Size),
            TerminalEvent::Query(TerminalQuery::Size),
            TerminalEvent::Query(TerminalQuery::Size),
            TerminalEvent::Query(TerminalQuery::DeviceAttributes),
            TerminalEvent::Query(TerminalQuery::DeviceAttributes),
            TerminalEvent::Query(TerminalQuery::DeviceAttributes),
            TerminalEvent::Query(TerminalQuery::ColorScheme),
        ]
    );
}

#[test]
fn pane_events_are_owned_until_that_view_finalizes_its_update() {
    let mut view = lector::view::View::new(2, 20);
    view.process_changes(b"\x07\x1B]2;pane title\x07");
    assert_eq!(
        view.terminal_events(),
        [
            TerminalEvent::Bell,
            TerminalEvent::TitleChanged("pane title".into()),
        ]
    );
    view.finalize_changes(1);
    assert!(view.terminal_events().is_empty());
}

#[test]
fn empty_title_and_working_directory_are_preserved_as_reported_clears() {
    let mut engine = engine(2, 20, 10);
    engine.advance(b"\x1B]2;set\x07\x1B]7;file://host/tmp\x1B\\");
    let update = engine.advance(b"\x1B]2;\x07\x1B]7;\x1B\\");

    assert_eq!(
        update.effects.events,
        [
            TerminalEvent::TitleChanged(String::new()),
            TerminalEvent::WorkingDirectoryChanged(String::new()),
        ]
    );
    assert_eq!(engine.snapshot().title.as_deref(), Some(""));
    assert_eq!(engine.snapshot().working_directory.as_deref(), Some(""));
}

#[test]
fn oversized_title_is_truncated_and_oversized_working_directory_is_ignored() {
    let mut sequence = b"\x1B]2;".to_vec();
    sequence.extend(std::iter::repeat_n(b't', 1100));
    sequence.extend_from_slice(b"\x1B\\\x1B]7;");
    sequence.extend(std::iter::repeat_n(b'p', 2050));
    sequence.extend_from_slice(b"\x1B\\");

    let mut engine = engine(2, 20, 10);
    let events = engine.advance(&sequence).effects.events;
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], TerminalEvent::TitleChanged(value) if value.len() == 1024));
    assert_eq!(engine.snapshot().title.as_ref().unwrap().len(), 1024);
    assert!(engine.snapshot().working_directory.is_none());
}

#[test]
fn ghostty_native_callbacks_match_typed_effects_and_keep_replies_brokered() {
    use lector::terminal::GhosttyEngine;

    let mut engine = GhosttyEngine::new(4, 40).expect("create Ghostty engine");
    let mut actual = Vec::new();
    let mut replies = Vec::new();
    for chunk in effect_stream() {
        let update = engine.advance(chunk).expect("advance Ghostty effects");
        replies.extend_from_slice(&update.pty_replies);
        actual.extend(update.effects.events);
    }

    assert_eq!(actual, expected_effects());
    assert!(
        !replies.is_empty(),
        "XTVERSION should produce a brokered reply"
    );
    assert!(
        engine.pty_replies().is_empty(),
        "shadow replies are not live PTY output"
    );
    assert_eq!(
        engine.snapshot().working_directory.as_deref(),
        Some("file://host/tmp/project")
    );
}

#[test]
fn ghostty_preserves_empty_title_and_working_directory_callback_values() {
    use lector::terminal::GhosttyEngine;

    let mut engine = GhosttyEngine::new(2, 20).expect("create Ghostty engine");
    engine
        .advance(b"\x1B]2;set\x07\x1B]7;file://host/tmp\x1B\\")
        .expect("set title and working directory");
    let update = engine
        .advance(b"\x1B]2;\x07\x1B]7;\x1B\\")
        .expect("clear title and working directory");

    assert_eq!(
        update.effects.events,
        [
            TerminalEvent::TitleChanged(String::new()),
            TerminalEvent::WorkingDirectoryChanged(String::new()),
        ]
    );
    assert_eq!(engine.snapshot().title.as_deref(), Some(""));
    assert_eq!(engine.snapshot().working_directory.as_deref(), Some(""));
}

#[test]
fn ghostty_title_and_working_directory_limits_match_the_observer() {
    let mut sequence = b"\x1B]2;".to_vec();
    sequence.extend(std::iter::repeat_n(b't', 1100));
    sequence.extend_from_slice(b"\x1B\\\x1B]7;");
    sequence.extend(std::iter::repeat_n(b'p', 2050));
    sequence.extend_from_slice(b"\x1B\\");

    let mut ghostty = GhosttyEngine::new(2, 20).expect("create Ghostty engine");
    let actual = ghostty.advance(&sequence).expect("advance long reports");
    assert_eq!(actual.effects.events.len(), 1);
    assert!(matches!(
        &actual.effects.events[0],
        TerminalEvent::TitleChanged(value) if value.len() == 1024
    ));
    assert_eq!(ghostty.snapshot().title.as_ref().unwrap().len(), 1024);
    assert!(ghostty.snapshot().working_directory.is_none());
}

#[test]
fn ghostty_modes_match_after_fragmented_push_pop_and_reset() {
    let mut ghostty = GhosttyEngine::new(4, 40).expect("create Ghostty engine");
    for chunk in [
        b"\x1B[?1004h\x1B[?1002h\x1B[?1006h".as_slice(),
        b"\x1B[?2004h\x1B[>3",
        b"u\x1B[=4;2u",
        b"\x1B[<u\x1B[?1004l",
    ] {
        ghostty.advance(chunk).expect("advance Ghostty modes");
    }
    let modes = &ghostty.snapshot().modes;
    assert!(!modes.focus_reporting);
    assert!(modes.bracketed_paste);
    assert_eq!(modes.kitty_keyboard_flags, 0);
    assert_eq!(
        modes.mouse_protocol,
        lector::terminal::MouseProtocol::ButtonMotion
    );
    assert_eq!(modes.mouse_encoding, lector::terminal::MouseEncoding::Sgr);
}

#[test]
fn ghostty_kitty_keyboard_flags_match_across_screen_switches() {
    let mut ghostty = GhosttyEngine::new(4, 40).expect("create Ghostty engine");
    for (sequence, expected) in [
        (b"\x1B[>3u".as_slice(), 3),
        (b"\x1B[?1049h", 0),
        (b"\x1B[>5u", 5),
        (b"\x1B[?1049l", 3),
        (b"\x1B[?1049h", 5),
    ] {
        ghostty.advance(sequence).expect("advance Ghostty mode");
        assert_eq!(ghostty.snapshot().kitty_keyboard_flags(), expected);
    }
}

#[test]
fn ghostty_effect_userdata_remains_valid_until_each_terminal_is_dropped() {
    use lector::terminal::GhosttyEngine;

    for iteration in 0..100 {
        let mut engine = GhosttyEngine::new(2, 20).expect("create Ghostty engine");
        let sequence = format!("\x07\x1B]2;title {iteration}\x07");
        let update = engine
            .advance(sequence.as_bytes())
            .expect("invoke callbacks before drop");
        assert_eq!(
            update.effects.events,
            [
                TerminalEvent::Bell,
                TerminalEvent::TitleChanged(format!("title {iteration}")),
            ]
        );
    }
}

#[test]
fn ghostty_fragmented_queries_and_bounded_unknowns_match_the_contract() {
    use lector::terminal::GhosttyEngine;

    let mut engine = GhosttyEngine::new(2, 20).expect("create Ghostty engine");
    let mut events = Vec::new();
    for byte in b"\x05\x1B[>q\x1B[18t\x1B[?996n\x1B[c" {
        let update = engine.advance(&[*byte]).expect("advance query byte");
        events.extend(
            update
                .effects
                .events
                .into_iter()
                .filter(|event| !matches!(event, TerminalEvent::PtyReply(_))),
        );
    }
    assert_eq!(
        events,
        [
            TerminalEvent::Query(TerminalQuery::Enquiry),
            TerminalEvent::Query(TerminalQuery::XtVersion),
            TerminalEvent::Query(TerminalQuery::Size),
            TerminalEvent::Query(TerminalQuery::ColorScheme),
            TerminalEvent::Query(TerminalQuery::DeviceAttributes),
        ]
    );

    let mut oversized = b"\x1B_".to_vec();
    oversized.extend(std::iter::repeat_n(b'z', 5_000));
    oversized.extend_from_slice(b"\x1B\\");
    assert_eq!(
        engine
            .advance(&oversized)
            .expect("advance oversized unknown sequence")
            .effects
            .events,
        [TerminalEvent::UnknownSequence {
            content: vec![b'z'; 4_096],
            truncated: true,
        }]
    );
    assert!(
        engine
            .advance(b"\x1B_aborted\x18")
            .expect("abort unknown sequence")
            .effects
            .events
            .is_empty()
    );
}

#[test]
fn ghostty_reports_every_exposed_size_and_device_query_form() {
    let sequence = b"\x1B[14t\x1B[16t\x1B[18t\x1B[c\x1B[>c\x1B[=c\x1B[?996n";
    let mut ghostty = GhosttyEngine::new(2, 20).expect("create Ghostty engine");
    let actual = ghostty.advance(sequence).expect("advance Ghostty queries");
    assert_eq!(
        actual.effects.events,
        [
            TerminalEvent::Query(TerminalQuery::Size),
            TerminalEvent::Query(TerminalQuery::Size),
            TerminalEvent::Query(TerminalQuery::Size),
            TerminalEvent::Query(TerminalQuery::DeviceAttributes),
            TerminalEvent::Query(TerminalQuery::DeviceAttributes),
            TerminalEvent::Query(TerminalQuery::DeviceAttributes),
            TerminalEvent::Query(TerminalQuery::ColorScheme),
        ]
    );
}

#[test]
fn ghostty_handles_clipboard_and_known_apc_edges_without_spurious_events() {
    let mut ghostty = GhosttyEngine::new(2, 20).expect("create Ghostty engine");
    let mut events = Vec::new();
    for sequence in [
        b"\x1B]52;c;YQ\x1B\\".as_slice(),
        b"\x1B]1337;Copy=:aVRlcm0y\x1B\\",
        b"\x1B]52;c;?\x1B\\",
        b"\x1B]52;c;A\x1B\\",
        b"\x1B]52;c;YQ==;ignored\x1B\\",
        b"\x1B]1337;Copy=:YQ==;ignored\x1B\\",
        b"\x1B]1337;CurrentDir=\x1B\\",
        b"\x1B]777;notify;title\x1B\\",
        b"\x1B]9;4;1\x1B\\",
        b"\x1B]9;4;1;150\x1B\\",
        b"\x1B]9;4;0;50\x1B\\",
        b"\x1B_Ga=t,t=d,f=24,i=1,s=1,v=2,c=10,r=1;////////\x1B\\",
        b"\x1B_25a1;garbage\x1B\\",
        b"\x1B_25a\x1B\\",
        b"\x1B_\x1B\\",
    ] {
        events.extend(
            ghostty
                .advance(sequence)
                .expect("advance Ghostty edge case")
                .effects
                .events,
        );
    }
    assert_eq!(events.len(), 6);
    assert!(matches!(events[0], TerminalEvent::ClipboardWrite { .. }));
    assert!(matches!(events[1], TerminalEvent::ClipboardWrite { .. }));
    assert_eq!(events[2], TerminalEvent::Query(TerminalQuery::Clipboard));
    assert_eq!(
        &events[3..],
        [
            TerminalEvent::ProgressReport {
                state: ProgressState::Set,
                progress: Some(0),
            },
            TerminalEvent::ProgressReport {
                state: ProgressState::Set,
                progress: Some(100),
            },
            TerminalEvent::ProgressReport {
                state: ProgressState::Remove,
                progress: None,
            },
        ]
    );
}
