use lector::terminal::{GhosttyEngine, TerminalSnapshot, UpdateSummary};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Deserialize, Serialize)]
struct Recording {
    name: String,
    source: String,
    rows: u16,
    cols: u16,
    scrollback: usize,
    operations: Vec<Operation>,
    /// Historical review metadata retained with the reusable corpus. These
    /// names document why a fixture was added; they no longer alter behavior
    /// now that Ghostty is authoritative.
    expected_difference_classes: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
enum Operation {
    Write { chunks: Vec<String> },
    WriteHex { chunks: Vec<String> },
    Resize { rows: u16, cols: u16 },
    Reset,
}

#[derive(Debug, Serialize)]
struct ReplayFailure<'a> {
    schema_version: u8,
    recording: &'a Recording,
    strategy: &'a str,
    reason: String,
    expected_debug: String,
    actual_debug: String,
}

#[derive(Clone, Copy)]
enum ChunkStrategy {
    Recorded,
    Coalesced,
    Bytewise,
}

impl ChunkStrategy {
    fn name(self) -> &'static str {
        match self {
            Self::Recorded => "recorded",
            Self::Coalesced => "coalesced",
            Self::Bytewise => "bytewise",
        }
    }
}

#[test]
fn representative_recordings_are_authoritative_and_fragmentation_invariant() {
    let recordings = load_recordings();
    let names = recordings
        .iter()
        .map(|recording| recording.name.as_str())
        .collect::<BTreeSet<_>>();
    for required in [
        "shell-prompt",
        "editor-alternate-screen",
        "pager-search",
        "compiler-diagnostics",
        "test-runner",
        "high-volume-scrollback",
        "xterm-1049-cursor-inheritance",
        "primary-resize-reflow",
        "wide-grapheme-right-margin",
        "wide-and-combining-midline",
        "xterm-1049-right-margin-wrap",
        "wide-tail-metadata",
    ] {
        assert!(names.contains(required), "missing {required} recording");
    }

    for recording in &recordings {
        let expected = replay_recording(recording, ChunkStrategy::Recorded);
        for strategy in [ChunkStrategy::Coalesced, ChunkStrategy::Bytewise] {
            let actual = replay_recording(recording, strategy);
            if actual != expected {
                write_failure_artifact(
                    recording,
                    strategy,
                    "final state changed with chunk boundaries",
                    &expected,
                    &actual,
                );
                panic!(
                    "recording {:?} changed under {} replay",
                    recording.name,
                    strategy.name()
                );
            }
        }
    }
}

#[test]
fn historical_classifications_are_known_documentation_not_runtime_policy() {
    let reviewed = BTreeSet::from([
        "lector_bug",
        "ghostty_wrapper_api_gap",
        "ghostty_correctness_improvement",
        "accepted_behavior_change",
    ]);
    for recording in load_recordings() {
        for class in &recording.expected_difference_classes {
            assert!(
                reviewed.contains(class.as_str()),
                "recording {:?} has unknown historical class {class:?}",
                recording.name
            );
        }
    }
}

#[test]
fn deterministic_boundary_fuzz_is_fragmentation_invariant_and_bounded() {
    let mut inputs = vec![
        b"plain\r\n\x1b[31mbold-ish\x1b[0m".to_vec(),
        "utf8: \u{754c} e\u{301} \u{1f600}\r\n".as_bytes().to_vec(),
        b"\x1b[31;1mred\x18cancelled\x1b[0m\x1b[?25l\x1b[?25h".to_vec(),
        b"\x1b]2;unterminated title".to_vec(),
        b"\x1bP1;2;3+qfragmented\x1b\\after".to_vec(),
        b"\x1b_Ga=t,t=d,f=24,i=1,s=1,v=1;////\x1b\\after".to_vec(),
        vec![
            0, 0x1b, b'[', b'9', b'9', b'9', b'9', b'9', b'z', 0xff, 0xfe,
        ],
    ];
    for prefix in [b"\x1b]2;".as_slice(), b"\x1bPq", b"\x1b_"] {
        let mut payload = prefix.to_vec();
        payload.extend(std::iter::repeat_n(b'x', 64 * 1024 + 1));
        payload.extend_from_slice(b"\x1b\\tail");
        inputs.push(payload);
    }

    for (case, input) in inputs.iter().enumerate() {
        let expected = replay_bytes(input, input.len().max(1));
        let chunk_sizes: &[usize] = if input.len() > 4_096 {
            &[31, 257, 4093]
        } else {
            &[1, 2, 3, 7, 31, 257, 4093]
        };
        for &chunk_size in chunk_sizes {
            assert_eq!(
                replay_bytes(input, chunk_size),
                expected,
                "Ghostty fragmentation changed case {case} at chunk size {chunk_size}"
            );
        }
    }

    let mut engine = GhosttyEngine::new_with_scrollback(5, 24, 64).unwrap();
    for line in 0..2_000 {
        engine
            .advance(format!("line {line:04}\r\n").as_bytes())
            .unwrap();
    }
    assert_eq!(engine.scrollback_extent(), 64);
    engine.reset().unwrap();
    let snapshot = engine.normalized_snapshot_with_history().unwrap();
    assert_eq!(snapshot.size(), (5, 24));
    assert_eq!(snapshot.scrollback_extent, 0);
    assert!(snapshot.rows.iter().all(|row| row.contents().is_empty()));
}

fn replay_recording(recording: &Recording, strategy: ChunkStrategy) -> TerminalSnapshot {
    let mut engine =
        GhosttyEngine::new_with_scrollback(recording.rows, recording.cols, recording.scrollback)
            .expect("create Ghostty recording engine");
    let mut rows = recording.rows;
    let mut cols = recording.cols;

    for operation in &recording.operations {
        match operation {
            Operation::Write { chunks } => {
                let chunks = chunks
                    .iter()
                    .map(|chunk| chunk.as_bytes())
                    .collect::<Vec<_>>();
                replay_chunks(
                    &mut engine,
                    &chunks,
                    strategy,
                    rows,
                    cols,
                    recording.scrollback,
                );
            }
            Operation::WriteHex { chunks } => {
                let decoded = chunks
                    .iter()
                    .map(|chunk| decode_hex(chunk))
                    .collect::<Vec<_>>();
                let chunks = decoded.iter().map(Vec::as_slice).collect::<Vec<_>>();
                replay_chunks(
                    &mut engine,
                    &chunks,
                    strategy,
                    rows,
                    cols,
                    recording.scrollback,
                );
            }
            Operation::Resize {
                rows: new_rows,
                cols: new_cols,
            } => {
                engine.resize(*new_rows, *new_cols).unwrap();
                rows = (*new_rows).max(1);
                cols = (*new_cols).max(1);
                assert_snapshot_invariants(
                    &engine.normalized_snapshot(),
                    rows,
                    cols,
                    recording.scrollback,
                );
            }
            Operation::Reset => {
                engine.reset().unwrap();
                assert_snapshot_invariants(
                    &engine.normalized_snapshot(),
                    rows,
                    cols,
                    recording.scrollback,
                );
            }
        }
    }
    engine.normalized_snapshot_with_history().unwrap()
}

fn replay_chunks(
    engine: &mut GhosttyEngine,
    chunks: &[&[u8]],
    strategy: ChunkStrategy,
    rows: u16,
    cols: u16,
    scrollback: usize,
) {
    match strategy {
        ChunkStrategy::Recorded => {
            for chunk in chunks {
                let update = engine.advance(chunk).unwrap();
                assert_update_invariants(&update, rows);
                assert_snapshot_invariants(&engine.normalized_snapshot(), rows, cols, scrollback);
            }
        }
        ChunkStrategy::Coalesced => {
            let bytes = chunks.concat();
            let update = engine.advance(&bytes).unwrap();
            assert_update_invariants(&update, rows);
            assert_snapshot_invariants(&engine.normalized_snapshot(), rows, cols, scrollback);
        }
        ChunkStrategy::Bytewise => {
            for byte in chunks.concat() {
                let update = engine.advance(&[byte]).unwrap();
                assert_update_invariants(&update, rows);
                assert_snapshot_invariants(&engine.normalized_snapshot(), rows, cols, scrollback);
            }
        }
    }
}

fn assert_update_invariants(update: &UpdateSummary, rows: u16) {
    assert_eq!(update.batch_count, 1);
    for range in &update.changed_rows {
        assert!(range.start() <= range.end());
        assert!(*range.end() < rows);
    }
}

fn assert_snapshot_invariants(
    snapshot: &TerminalSnapshot,
    rows: u16,
    cols: u16,
    scrollback: usize,
) {
    assert_eq!(snapshot.size(), (rows.max(1), cols.max(1)));
    assert!(
        snapshot
            .rows
            .iter()
            .all(|row| row.cells.len() == usize::from(cols.max(1)))
    );
    assert!(snapshot.scrollback_extent <= scrollback);
    assert!(snapshot.semantic_marks.iter().all(|mark| {
        mark.position.row
            < snapshot
                .scrollback_extent
                .saturating_add(snapshot.rows.len())
            && mark.position.col < cols.max(1)
    }));
}

fn replay_bytes(input: &[u8], chunk_size: usize) -> TerminalSnapshot {
    let mut engine = GhosttyEngine::new_with_scrollback(8, 40, 128).unwrap();
    for chunk in input.chunks(chunk_size) {
        engine.advance(chunk).unwrap();
    }
    engine.normalized_snapshot_with_history().unwrap()
}

fn load_recordings() -> Vec<Recording> {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ghostty-recordings");
    let mut paths = fs::read_dir(&directory)
        .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()))
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect::<Vec<_>>();
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let bytes = fs::read(&path).unwrap();
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()))
        })
        .collect()
}

fn decode_hex(input: &str) -> Vec<u8> {
    assert_eq!(input.len() % 2, 0, "hex input must contain pairs");
    let (pairs, remainder) = input.as_bytes().as_chunks::<2>();
    assert!(remainder.is_empty());
    pairs
        .iter()
        .map(|pair| {
            let pair = std::str::from_utf8(pair.as_slice()).unwrap();
            u8::from_str_radix(pair, 16).unwrap()
        })
        .collect()
}

fn write_failure_artifact(
    recording: &Recording,
    strategy: ChunkStrategy,
    reason: &str,
    expected: &TerminalSnapshot,
    actual: &TerminalSnapshot,
) {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target/test-artifacts/ghostty-authoritative");
    fs::create_dir_all(&directory).unwrap();
    let artifact = ReplayFailure {
        schema_version: 1,
        recording,
        strategy: strategy.name(),
        reason: reason.to_owned(),
        expected_debug: format!("{expected:#?}"),
        actual_debug: format!("{actual:#?}"),
    };
    fs::write(
        directory.join(format!("{}-{}.json", recording.name, strategy.name())),
        serde_json::to_vec_pretty(&artifact).unwrap(),
    )
    .unwrap();
}
