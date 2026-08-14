#[path = "support/terminal.rs"]
mod terminal;

use lector::{harness::Harness, view::View};
use std::{fs, path::Path};
use terminal::{
    FailureArtifact, NormalizedCell, NormalizedColor, NormalizedCursor, NormalizedModes,
    NormalizedRow, NormalizedSemanticMark, NormalizedStyle, NormalizedTerminalSnapshot,
    ScreenIdentity, TerminalSize, assert_raw_presentation, assert_snapshot, capture_snapshot,
    decode_hex, encode_hex, every_byte_split, execute_fixture, load_fixtures,
    write_failure_artifact,
};

const FIXTURE_DIRECTORY: &str = "tests/fixtures/terminal";

struct RawPresentationCase<'a> {
    name: &'a str,
    intended_scene: &'a str,
    title: Option<&'a str>,
    chunks: &'a [&'a [u8]],
}

#[test]
fn terminal_regression_fixtures_match_normalized_state() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixtures = load_fixtures(&root.join(FIXTURE_DIRECTORY)).expect("load terminal fixtures");
    assert!(
        fixtures.len() >= 3,
        "expected primary, alternate, and scrollback fixtures"
    );

    for (path, fixture) in fixtures {
        let (actual, source, boundaries) = execute_fixture(&fixture)
            .unwrap_or_else(|error| panic!("execute {}: {error}", path.display()));
        assert_snapshot(
            &fixture.name,
            &fixture.intended_scene,
            &source,
            &boundaries,
            // Phase 0 characterizes the existing raw-output presentation path.
            &source,
            &fixture.expected,
            &actual,
        );
    }
}

#[test]
fn raw_presentation_preserves_current_terminal_protocols_and_bells() {
    let cases = [
        RawPresentationCase {
            name: "raw-title-link-semantics-kitty-mouse-bell",
            intended_scene: "Linked text with a title, semantic prompt, mouse mode, Kitty keyboard mode, and bell",
            title: Some("lector title"),
            chunks: &[
                b"\x1B]0;lector title\x07\x1B]8;;https://example.test\x1B\\linked",
                b"\x1B]8;;\x1B\\\x1B]133;A\x07\x1B[>31u",
                b"\x1B[?1000h\x07",
            ],
        },
        RawPresentationCase {
            name: "raw-fragmented-control-sequences",
            intended_scene: "A title and RGB-styled cell assembled across arbitrary PTY read boundaries",
            title: Some("fragmented"),
            chunks: &[b"\x1B]0;frag", b"mented\x1B", b"\\\x1B[38;2;1;", b"2;3mX"],
        },
        RawPresentationCase {
            name: "raw-effects-and-physical-terminal-queries",
            intended_scene: "Side effects and terminal queries remain byte-for-byte physical-terminal passthrough",
            title: Some("effects"),
            chunks: &[
                b"\x07\x1B]2;effects\x07\x1B]7;file://host/tmp\x1B\\",
                b"\x1B]52;c;aGVsbG8=\x1B\\\x1B]777;notify;Build;Done\x1B\\",
                b"\x1B]9;4;1;50\x07\x1B[c\x1B[18t\x1B[?996n\x1B[>q",
            ],
        },
    ];

    for case in cases {
        let mut harness = Harness::new(4, 40).expect("create harness");
        let mut expected_view = View::new(4, 40);
        let mut source = Vec::new();
        let mut boundaries = Vec::new();
        for chunk in case.chunks {
            harness
                .handle_pty_output(chunk)
                .unwrap_or_else(|error| panic!("process {}: {error}", case.name));
            expected_view.process_changes(chunk);
            source.extend_from_slice(chunk);
            boundaries.push(source.len());
        }

        let emitted = harness.terminal_output();
        let mut oracle = View::new(4, 40);
        oracle.process_changes(emitted);
        let expected = capture_snapshot(&mut expected_view, case.title.map(str::to_owned));
        let oracle_result = capture_snapshot(&mut oracle, case.title.map(str::to_owned));
        assert_raw_presentation(
            case.name,
            case.intended_scene,
            &source,
            &boundaries,
            emitted,
            &expected,
            &oracle_result,
        );
    }
}

#[test]
fn representative_utf8_and_vt_sequences_are_chunk_boundary_invariant() {
    let sequences: &[(&str, &[u8])] = &[
        (
            "multi-byte and combining graphemes",
            "A界e\u{301}Z".as_bytes(),
        ),
        (
            "CSI RGB styling",
            b"\x1B[38;2;12;34;56;48;5;200;1;3;4;7mX\x1B[0m",
        ),
        (
            "OSC title terminated by BEL",
            b"\x1B]0;fragmented title\x07",
        ),
        (
            "OSC 8 hyperlink terminated by ST",
            b"\x1B]8;;https://example.test\x1B\\link\x1B]8;;\x1B\\",
        ),
        ("OSC 133 semantic marker", b"\x1B]133;D;17\x07"),
        ("alternate-screen transition", b"\x1B[?1049halt\x1B[?1049l"),
        ("mouse modes", b"\x1B[?1002h\x1B[?1006h"),
        ("Kitty keyboard mode", b"\x1B[>31u"),
        ("DCS control string", b"\x1BP1;2|payload\x1B\\"),
    ];

    for (name, source) in sequences {
        let mut whole = View::new(4, 24);
        whole.process_changes(source);
        let expected = capture_snapshot(&mut whole, None);
        for chunks in every_byte_split(source) {
            let mut fragmented = View::new(4, 24);
            let mut boundaries = Vec::new();
            let mut consumed = 0;
            for chunk in chunks {
                fragmented.process_changes(chunk);
                consumed += chunk.len();
                boundaries.push(consumed);
            }
            let actual = capture_snapshot(&mut fragmented, None);
            assert_snapshot(
                &format!("fragmented-{name}"),
                name,
                source,
                &boundaries,
                source,
                &expected,
                &actual,
            );
        }
    }
}

#[test]
fn normalization_records_graphemes_widths_styles_wrapping_modes_and_semantics() {
    let mut view = View::new(2, 8);
    view.process_changes(
        "\x1B[38;2;12;34;56;48;5;200;1;3;4;7mA\x1B[0m\x1B[2mB\x1B[0me\u{301}界\x1B]133;A\x07\x1B[?1h\x1B=\x1B[?2004h\x1B[?1000h\x1B[?1006h\x1B[?25l"
            .as_bytes(),
    );
    let snapshot = capture_snapshot(&mut view, Some("title".to_owned()));

    assert_eq!(snapshot.size, TerminalSize { rows: 2, cols: 8 });
    assert_eq!(snapshot.rows[0].cells[0].grapheme, "A");
    assert_eq!(
        snapshot.rows[0].cells[0].style.foreground,
        NormalizedColor::Rgb(12, 34, 56)
    );
    assert_eq!(
        snapshot.rows[0].cells[0].style.background,
        NormalizedColor::Indexed(200)
    );
    assert!(snapshot.rows[0].cells[0].style.bold);
    assert!(snapshot.rows[0].cells[0].style.italic);
    assert!(snapshot.rows[0].cells[0].style.underline);
    assert!(snapshot.rows[0].cells[0].style.inverse);
    assert!(snapshot.rows[0].cells[1].style.dim);
    assert_eq!(snapshot.rows[0].cells[2].grapheme, "e\u{301}");
    assert_eq!(snapshot.rows[0].cells[3].width, 2);
    assert_eq!(snapshot.rows[0].cells[4].width, 0);
    assert!(snapshot.rows[0].cells[4].continuation);
    assert!(snapshot.modes.application_cursor);
    assert!(snapshot.modes.application_keypad);
    assert!(snapshot.modes.bracketed_paste);
    assert_eq!(snapshot.modes.mouse_protocol, "press_release");
    assert_eq!(snapshot.modes.mouse_encoding, "sgr");
    assert!(!snapshot.cursor.visible);
    assert_eq!(snapshot.title.as_deref(), Some("title"));
    assert_eq!(snapshot.semantic_marks[0].kind, "prompt_start");
    assert_eq!(snapshot.rows[1], NormalizedRow::default());
}

#[test]
fn fixture_loader_hex_codec_and_chunk_splitter_have_stable_rules() {
    assert_eq!(decode_hex("00 1b FF").unwrap(), [0x00, 0x1B, 0xFF]);
    assert_eq!(encode_hex(&[0x00, 0x1B, 0xFF]), "001BFF");
    assert!(decode_hex("0").unwrap_err().contains("odd"));
    assert!(decode_hex("GG").unwrap_err().contains("invalid"));

    let splits = every_byte_split(b"abcd");
    assert_eq!(splits.len(), 4);
    assert_eq!(splits[0], [b"abcd".as_slice()]);
    assert_eq!(splits[1], [b"a".as_slice(), b"bcd".as_slice()]);
    assert_eq!(splits[3], [b"abc".as_slice(), b"d".as_slice()]);

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixtures = load_fixtures(&root.join(FIXTURE_DIRECTORY)).unwrap();
    let names = fixtures
        .iter()
        .map(|(_, fixture)| fixture.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        ["alternate-screen", "primary-state", "scrollback-and-resize"]
    );
}

#[test]
fn failure_artifact_format_is_reproducible_and_complete() {
    let snapshot = NormalizedTerminalSnapshot {
        size: TerminalSize { rows: 1, cols: 2 },
        rows: vec![NormalizedRow {
            cells: vec![NormalizedCell {
                grapheme: "x".to_owned(),
                width: 1,
                continuation: false,
                style: NormalizedStyle::default(),
                hyperlink: None,
            }],
            wrapped: false,
        }],
        scrollback: Vec::new(),
        cursor: NormalizedCursor {
            row: 0,
            col: 1,
            visible: true,
        },
        screen: ScreenIdentity::Primary,
        modes: NormalizedModes {
            mouse_protocol: "none".to_owned(),
            mouse_encoding: "default".to_owned(),
            ..NormalizedModes::default()
        },
        title: Some("fixture".to_owned()),
        semantic_marks: vec![NormalizedSemanticMark {
            kind: "prompt_start".to_owned(),
            row: 0,
            col: 0,
            alternate_screen: false,
        }],
    };
    let artifact = FailureArtifact {
        schema_version: 1,
        test_name: "artifact-format".to_owned(),
        intended_scene: "one x cell".to_owned(),
        source_hex: "78".to_owned(),
        chunk_boundaries: vec![1],
        emitted_hex: "1B5B4878".to_owned(),
        expected: snapshot.clone(),
        oracle_result: snapshot,
    };
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/terminal-test-artifacts/artifact-format-self-test.json");
    write_failure_artifact(&path, &artifact).expect("write artifact");
    let encoded = fs::read_to_string(&path).expect("read artifact");
    let decoded: FailureArtifact = serde_json::from_str(&encoded).expect("parse artifact");
    assert_eq!(decoded, artifact);
    assert!(encoded.contains("\"source_hex\": \"78\""));
    assert!(encoded.contains("\"chunk_boundaries\""));
    assert!(encoded.contains("\"intended_scene\""));
    assert!(encoded.contains("\"emitted_hex\""));
    assert!(encoded.contains("\"oracle_result\""));
    fs::remove_file(path).expect("remove self-test artifact");
}
