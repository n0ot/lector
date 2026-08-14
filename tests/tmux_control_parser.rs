use lector::tmux_control::{
    CommandStatus, ControlEvent, ControlParseError, ParserLimits, TmuxControlParser,
};
use serde_json::Value;

const DOCUMENTED: &str = include_str!("fixtures/tmux-control/documented.json");
const LOCAL_CAPTURE: &str = include_str!("fixtures/tmux-control/local-tmux-3.7b.json");

fn fixture_stream(fixture: &str) -> Vec<u8> {
    serde_json::from_str::<Value>(fixture).unwrap()["stream"]
        .as_str()
        .unwrap()
        .as_bytes()
        .to_vec()
}

fn parse_chunks<'a>(chunks: impl IntoIterator<Item = &'a [u8]>) -> Vec<ControlEvent> {
    let mut parser = TmuxControlParser::new();
    let mut events = Vec::new();
    for chunk in chunks {
        events.extend(parser.push(chunk).unwrap());
    }
    events.extend(parser.finish().unwrap());
    events
}

fn assert_all_fragmentations(stream: &[u8], expected: &[ControlEvent]) {
    assert_eq!(parse_chunks([stream]), expected);

    for split in 0..=stream.len() {
        assert_eq!(
            parse_chunks([&stream[..split], &stream[split..]]),
            expected,
            "different result when the stream was split at byte {split}"
        );
    }

    assert_eq!(
        parse_chunks(stream.iter().map(std::slice::from_ref)),
        expected,
        "different result when every byte was a separate read"
    );
}

#[test]
fn documented_fixture_is_streaming_and_binary_safe() {
    let stream = fixture_stream(DOCUMENTED);
    let expected = vec![
        ControlEvent::Started,
        ControlEvent::Command {
            timestamp: 1_363_006_971,
            number: 2,
            flags: 1,
            status: CommandStatus::Success,
            output: vec![
                b"0: ksh* (1 panes) [80x24] [layout b25f,80x24,0,0,2] @2 (active)".to_vec(),
            ],
        },
        ControlEvent::Output {
            pane_id: 7,
            bytes: vec![b'A', 1, b'\\', 255, b'Z'],
        },
        ControlEvent::ExtendedOutput {
            pane_id: 7,
            age_ms: 42,
            future_fields: vec![b"future".to_vec(), b"field".to_vec()],
            bytes: vec![b'B', 0, b'C'],
        },
        ControlEvent::Pause { pane_id: 7 },
        ControlEvent::Continue { pane_id: 7 },
        ControlEvent::Notification {
            name: b"message".to_vec(),
            arguments: b"fixture ready".to_vec(),
        },
        ControlEvent::Exit {
            reason: Some(b"fixture complete".to_vec()),
        },
        ControlEvent::Ended,
    ];

    assert_all_fragmentations(&stream, &expected);
}

#[test]
fn captured_local_fixture_accepts_crlf_and_preserves_notifications() {
    let stream = fixture_stream(LOCAL_CAPTURE);
    let expected = vec![
        ControlEvent::Started,
        ControlEvent::Command {
            timestamp: 1_786_697_214,
            number: 286,
            flags: 0,
            status: CommandStatus::Success,
            output: Vec::new(),
        },
        ControlEvent::Notification {
            name: b"session-changed".to_vec(),
            arguments: b"$0 lector_fixture".to_vec(),
        },
        ControlEvent::Output {
            pane_id: 0,
            bytes: b"printf 'B\\001\\177\\\\Q\\n'\r\n".to_vec(),
        },
        ControlEvent::Notification {
            name: b"sessions-changed".to_vec(),
            arguments: Vec::new(),
        },
        ControlEvent::Exit { reason: None },
        ControlEvent::Ended,
    ];

    assert_all_fragmentations(&stream, &expected);
}

#[test]
fn repeated_pty_carriage_returns_do_not_corrupt_control_records() {
    let stream = b"\x1bP1000p%begin 1 2 0\r\r\nB\tM-6\t0\tselect-layout main-horizontal-mirrored\r\r\n%end 1 2 0\r\r\n\x1b\\";
    let expected = vec![
        ControlEvent::Started,
        ControlEvent::Command {
            timestamp: 1,
            number: 2,
            flags: 0,
            status: CommandStatus::Success,
            output: vec![b"B\tM-6\t0\tselect-layout main-horizontal-mirrored".to_vec()],
        },
        ControlEvent::Ended,
    ];

    assert_all_fragmentations(stream, &expected);
}

#[test]
fn correlates_sequential_replies_with_notifications_between_them() {
    let stream = b"\x1bP1000p%begin 10 41 0\nfirst\n%end 10 41 0\n%window-add @3\n%begin 11 42 7\nsecond\n%error 11 42 7\n\x1b\\";
    let events = parse_chunks(stream.iter().map(std::slice::from_ref));

    assert_eq!(
        events,
        vec![
            ControlEvent::Started,
            ControlEvent::Command {
                timestamp: 10,
                number: 41,
                flags: 0,
                status: CommandStatus::Success,
                output: vec![b"first".to_vec()],
            },
            ControlEvent::Notification {
                name: b"window-add".to_vec(),
                arguments: b"@3".to_vec(),
            },
            ControlEvent::Command {
                timestamp: 11,
                number: 42,
                flags: 7,
                status: CommandStatus::Error,
                output: vec![b"second".to_vec()],
            },
            ControlEvent::Ended,
        ]
    );
}

#[test]
fn decoded_nested_control_marker_is_only_pane_data() {
    let stream = b"\x1bP1000p%output %9 \\033P1000p%exit\\015\\012\\033\\134\n%exit\n\x1b\\";
    assert_eq!(
        parse_chunks(stream.iter().map(std::slice::from_ref)),
        vec![
            ControlEvent::Started,
            ControlEvent::Output {
                pane_id: 9,
                bytes: b"\x1bP1000p%exit\r\n\x1b\\".to_vec(),
            },
            ControlEvent::Exit { reason: None },
            ControlEvent::Ended,
        ]
    );
}

fn parse_error(stream: &[u8]) -> ControlParseError {
    let mut parser = TmuxControlParser::new();
    parser.push(stream).unwrap_err()
}

#[test]
fn rejects_malformed_framing_ids_tags_and_octal_data() {
    let cases = [
        b"no marker".as_slice(),
        b"\x1bP1000p%begin nope 1 0\n".as_slice(),
        b"\x1bP1000p%begin 1 18446744073709551616 0\n".as_slice(),
        b"\x1bP1000p%begin 1 2 0\n%end 1 3 0\n".as_slice(),
        b"\x1bP1000p%output nope value\n".as_slice(),
        b"\x1bP1000p%output %1 \\12x\n".as_slice(),
        b"\x1bP1000p%output %1 \\400\n".as_slice(),
        b"\x1bP1000p%extended-output %1 nope : x\n".as_slice(),
        b"\x1bP1000p%extended-output %1 2 missing-colon\n".as_slice(),
        b"\x1bP1000p%begin\n".as_slice(),
        b"\x1bP1000p%end\n".as_slice(),
        b"\x1bP1000p%error\n".as_slice(),
        b"\x1bP1000p%output\n".as_slice(),
        b"\x1bP1000p%extended-output\n".as_slice(),
        b"\x1bP1000p%pause\n".as_slice(),
        b"\x1bP1000p%continue\n".as_slice(),
    ];

    for stream in cases {
        assert!(
            parse_error(stream).is_malformed(),
            "expected a classified malformed-stream error for {stream:?}"
        );
    }
}

#[test]
fn bounds_each_untrusted_memory_category() {
    let limits = ParserLimits {
        max_line_bytes: 24,
        max_command_output_bytes: 9,
        max_command_output_lines: 2,
        max_notification_bytes: 8,
    };

    let cases = [
        (
            b"\x1bP1000pabcdefghijklmnopqrstuvwxy".as_slice(),
            ControlParseError::LineTooLong { limit: 24 },
        ),
        (
            b"\x1bP1000p%begin 1 2 0\n12345\n67890\n".as_slice(),
            ControlParseError::CommandOutputTooLong { limit: 9 },
        ),
        (
            b"\x1bP1000p%message 123456789\n".as_slice(),
            ControlParseError::NotificationTooLong { limit: 8 },
        ),
    ];

    for (stream, expected) in cases {
        let mut parser = TmuxControlParser::with_limits(limits);
        assert_eq!(parser.push(stream), Err(expected));
    }
}

#[test]
fn finish_rejects_every_unterminated_parser_state() {
    let cases = [
        b"\x1bP10".as_slice(),
        b"\x1bP1000p%output %1 abc".as_slice(),
        b"\x1bP1000p%begin 1 2 0\nvalue\n".as_slice(),
        b"\x1bP1000p%exit\n\x1b".as_slice(),
    ];

    for stream in cases {
        let mut parser = TmuxControlParser::new();
        parser.push(stream).unwrap();
        assert!(
            parser.finish().unwrap_err().is_unterminated(),
            "expected an unterminated-stream error for {stream:?}"
        );
    }
}

#[test]
fn parser_can_be_reset_after_an_error_without_retaining_partial_data() {
    let mut parser = TmuxControlParser::new();
    assert!(parser.push(b"not control mode").is_err());
    parser.reset();

    let mut events = parser
        .push(b"\x1bP1000p%output %4 recovered\n%exit recovered\n\x1b\\")
        .unwrap();
    events.extend(parser.finish().unwrap());
    assert_eq!(
        events,
        vec![
            ControlEvent::Started,
            ControlEvent::Output {
                pane_id: 4,
                bytes: b"recovered".to_vec(),
            },
            ControlEvent::Exit {
                reason: Some(b"recovered".to_vec()),
            },
            ControlEvent::Ended,
        ]
    );
}

#[test]
fn command_output_is_not_misclassified_as_an_async_notification() {
    let stream = b"\x1bP1000p%begin 5 8 0\n%output is command text\n\n%end 5 8 0\n\x1b\\";
    assert_eq!(
        parse_chunks(stream.iter().map(std::slice::from_ref)),
        vec![
            ControlEvent::Started,
            ControlEvent::Command {
                timestamp: 5,
                number: 8,
                flags: 0,
                status: CommandStatus::Success,
                output: vec![b"%output is command text".to_vec(), Vec::new()],
            },
            ControlEvent::Ended,
        ]
    );
}

#[test]
fn exact_limits_are_accepted_and_one_byte_more_is_rejected() {
    let limits = ParserLimits {
        max_line_bytes: 18,
        max_command_output_bytes: 4,
        max_command_output_lines: 2,
        max_notification_bytes: 18,
    };
    let mut parser = TmuxControlParser::with_limits(limits);
    assert_eq!(
        parser.push(b"\x1bP1000p%message 123456789\n").unwrap(),
        vec![
            ControlEvent::Started,
            ControlEvent::Notification {
                name: b"message".to_vec(),
                arguments: b"123456789".to_vec(),
            },
        ]
    );

    parser.reset();
    assert_eq!(
        parser.push(b"\x1bP1000p%message 1234567890"),
        Err(ControlParseError::LineTooLong { limit: 18 })
    );

    let mut parser = TmuxControlParser::with_limits(limits);
    parser.push(b"\x1bP1000p%begin 1 2 0\nabcd\n").unwrap();
    assert_eq!(
        parser.push(b"x\n"),
        Err(ControlParseError::CommandOutputTooLong { limit: 4 })
    );
}

#[test]
fn empty_command_lines_cannot_bypass_the_output_memory_bound() {
    let limits = ParserLimits {
        max_line_bytes: 32,
        max_command_output_bytes: 1_024,
        max_command_output_lines: 2,
        max_notification_bytes: 32,
    };
    let mut parser = TmuxControlParser::with_limits(limits);
    parser.push(b"\x1bP1000p%begin 1 2 0\n\n\n").unwrap();
    assert_eq!(
        parser.push(b"\n"),
        Err(ControlParseError::TooManyCommandOutputLines { limit: 2 })
    );
}

#[test]
fn errors_poison_the_parser_until_an_explicit_reset() {
    let mut parser = TmuxControlParser::new();
    assert!(parser.push(b"bad").is_err());
    assert_eq!(
        parser.push(b"\x1bP1000p%exit\n\x1b\\"),
        Err(ControlParseError::ParserPoisoned)
    );
    assert_eq!(parser.finish(), Err(ControlParseError::ParserPoisoned));
}
