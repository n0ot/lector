#![allow(dead_code)]

use lector::{
    terminal::{
        Cell as TerminalCell, Color as TerminalColor, MouseEncoding, MouseProtocol,
        Row as TerminalRow, SemanticKind as Osc133Kind, SemanticMark as Osc133Mark,
    },
    view::View,
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct TerminalFixture {
    pub name: String,
    pub rows: u16,
    pub cols: u16,
    pub title: Option<String>,
    pub intended_scene: String,
    pub operations: Vec<FixtureOperation>,
    pub expected: NormalizedTerminalSnapshot,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum FixtureOperation {
    Process { hex: String },
    Resize { rows: u16, cols: u16 },
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct NormalizedTerminalSnapshot {
    pub size: TerminalSize,
    pub rows: Vec<NormalizedRow>,
    pub scrollback: Vec<NormalizedRow>,
    pub cursor: NormalizedCursor,
    pub screen: ScreenIdentity,
    pub modes: NormalizedModes,
    pub title: Option<String>,
    pub semantic_marks: Vec<NormalizedSemanticMark>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct TerminalSize {
    pub rows: u16,
    pub cols: u16,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct NormalizedRow {
    pub cells: Vec<NormalizedCell>,
    pub wrapped: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct NormalizedCell {
    pub grapheme: String,
    pub width: u8,
    pub continuation: bool,
    pub style: NormalizedStyle,
    pub hyperlink: Option<String>,
}

impl Default for NormalizedCell {
    fn default() -> Self {
        Self {
            grapheme: String::new(),
            width: 1,
            continuation: false,
            style: NormalizedStyle::default(),
            hyperlink: None,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct NormalizedStyle {
    pub foreground: NormalizedColor,
    pub background: NormalizedColor,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NormalizedColor {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct NormalizedCursor {
    pub row: u16,
    pub col: u16,
    pub visible: bool,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScreenIdentity {
    #[default]
    Primary,
    Alternate,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct NormalizedModes {
    pub application_keypad: bool,
    pub application_cursor: bool,
    pub bracketed_paste: bool,
    pub mouse_protocol: String,
    pub mouse_encoding: String,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct NormalizedSemanticMark {
    pub kind: String,
    pub row: usize,
    pub col: u16,
    pub alternate_screen: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FailureArtifact {
    pub schema_version: u8,
    pub test_name: String,
    pub intended_scene: String,
    pub source_hex: String,
    pub chunk_boundaries: Vec<usize>,
    pub emitted_hex: String,
    pub expected: NormalizedTerminalSnapshot,
    pub oracle_result: NormalizedTerminalSnapshot,
}

pub fn load_fixtures(path: &Path) -> Result<Vec<(PathBuf, TerminalFixture)>, String> {
    let entries = fs::read_dir(path)
        .map_err(|error| format!("read fixture directory {}: {error}", path.display()))?;
    let mut paths = entries
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("read fixture entry: {error}"))?;
    paths.retain(|path| {
        path.extension()
            .is_some_and(|extension| extension == "json")
    });
    paths.sort();

    paths
        .into_iter()
        .map(|path| {
            let source = fs::read_to_string(&path)
                .map_err(|error| format!("read fixture {}: {error}", path.display()))?;
            let fixture = serde_json::from_str(&source)
                .map_err(|error| format!("parse fixture {}: {error}", path.display()))?;
            Ok((path, fixture))
        })
        .collect()
}

pub fn execute_fixture(
    fixture: &TerminalFixture,
) -> Result<(NormalizedTerminalSnapshot, Vec<u8>, Vec<usize>), String> {
    let mut view = View::new(fixture.rows, fixture.cols);
    let mut source = Vec::new();
    let mut chunk_boundaries = Vec::new();
    for operation in &fixture.operations {
        match operation {
            FixtureOperation::Process { hex } => {
                let bytes = decode_hex(hex)?;
                view.process_changes(&bytes);
                source.extend_from_slice(&bytes);
                chunk_boundaries.push(source.len());
            }
            FixtureOperation::Resize { rows, cols } => view.set_size(*rows, *cols),
        }
    }
    Ok((
        capture_snapshot(&mut view, fixture.title.clone()),
        source,
        chunk_boundaries,
    ))
}

pub fn capture_snapshot(view: &mut View, title: Option<String>) -> NormalizedTerminalSnapshot {
    let screen = view.snapshot_with_history();
    let (rows, cols) = screen.size();
    let screen_identity = if screen.alternate_screen() {
        ScreenIdentity::Alternate
    } else {
        ScreenIdentity::Primary
    };
    let visible_rows = screen.rows.iter().map(normalize_row).collect();
    let scrollback = if screen_identity == ScreenIdentity::Primary {
        screen.scrollback.iter().map(normalize_row).collect()
    } else {
        Vec::new()
    };

    let (cursor_row, cursor_col) = screen.cursor_position();
    NormalizedTerminalSnapshot {
        size: TerminalSize { rows, cols },
        rows: visible_rows,
        scrollback,
        cursor: NormalizedCursor {
            row: cursor_row,
            col: cursor_col,
            visible: !screen.hide_cursor(),
        },
        screen: screen_identity,
        modes: NormalizedModes {
            application_keypad: screen.application_keypad(),
            application_cursor: screen.application_cursor(),
            bracketed_paste: screen.bracketed_paste(),
            mouse_protocol: mouse_protocol(screen.mouse_protocol_mode()).to_owned(),
            mouse_encoding: mouse_encoding(screen.mouse_protocol_encoding()).to_owned(),
        },
        title: title.or(screen.title),
        semantic_marks: view
            .osc133_marks()
            .iter()
            .map(normalize_semantic_mark)
            .collect(),
    }
}

fn normalize_row(row: &TerminalRow) -> NormalizedRow {
    let mut cells = row.cells.iter().map(normalize_cell).collect::<Vec<_>>();
    while cells
        .last()
        .is_some_and(|cell| cell == &NormalizedCell::default())
    {
        cells.pop();
    }
    NormalizedRow {
        cells,
        wrapped: row.wrapped,
    }
}

fn normalize_cell(cell: &TerminalCell) -> NormalizedCell {
    NormalizedCell {
        grapheme: cell.grapheme.clone(),
        width: cell.width,
        continuation: cell.continuation,
        style: NormalizedStyle {
            foreground: normalize_color(cell.fgcolor()),
            background: normalize_color(cell.bgcolor()),
            bold: cell.bold(),
            dim: cell.dim(),
            italic: cell.italic(),
            underline: cell.underline(),
            inverse: cell.inverse(),
        },
        // Fixture snapshots do not model OSC 8 link targets.
        hyperlink: None,
    }
}

fn normalize_color(color: TerminalColor) -> NormalizedColor {
    match color {
        TerminalColor::Default => NormalizedColor::Default,
        TerminalColor::Indexed(index) => NormalizedColor::Indexed(index),
        TerminalColor::Rgb(red, green, blue) => NormalizedColor::Rgb(red, green, blue),
    }
}

fn normalize_semantic_mark(mark: &Osc133Mark) -> NormalizedSemanticMark {
    let kind = match mark.kind {
        Osc133Kind::PromptStart => "prompt_start".to_owned(),
        Osc133Kind::InputStart => "input_start".to_owned(),
        Osc133Kind::CommandStart => "command_start".to_owned(),
        Osc133Kind::CommandFinished { exit_code } => match exit_code {
            Some(code) => format!("command_finished:{code}"),
            None => "command_finished".to_owned(),
        },
    };
    NormalizedSemanticMark {
        kind,
        row: mark.position.row,
        col: mark.position.col,
        alternate_screen: mark.alternate_screen,
    }
}

fn mouse_protocol(mode: MouseProtocol) -> &'static str {
    match mode {
        MouseProtocol::None => "none",
        MouseProtocol::Press => "press",
        MouseProtocol::PressRelease => "press_release",
        MouseProtocol::ButtonMotion => "button_motion",
        MouseProtocol::AnyMotion => "any_motion",
    }
}

fn mouse_encoding(encoding: MouseEncoding) -> &'static str {
    match encoding {
        MouseEncoding::Default => "default",
        MouseEncoding::Utf8 => "utf8",
        MouseEncoding::Sgr => "sgr",
    }
}

pub fn every_byte_split(source: &[u8]) -> Vec<Vec<&[u8]>> {
    let mut variants = vec![vec![source]];
    variants
        .extend((1..source.len()).map(|boundary| vec![&source[..boundary], &source[boundary..]]));
    variants
}

pub fn assert_snapshot(
    test_name: &str,
    intended_scene: &str,
    source: &[u8],
    chunk_boundaries: &[usize],
    emitted: &[u8],
    expected: &NormalizedTerminalSnapshot,
    actual: &NormalizedTerminalSnapshot,
) {
    if expected == actual {
        return;
    }
    let artifact = FailureArtifact {
        schema_version: 1,
        test_name: test_name.to_owned(),
        intended_scene: intended_scene.to_owned(),
        source_hex: encode_hex(source),
        chunk_boundaries: chunk_boundaries.to_vec(),
        emitted_hex: encode_hex(emitted),
        expected: expected.clone(),
        oracle_result: actual.clone(),
    };
    let path = artifact_path(test_name);
    let write_result = write_failure_artifact(&path, &artifact);
    panic!(
        "normalized terminal mismatch for {test_name}; artifact: {}{}\nexpected: {expected:#?}\nactual: {actual:#?}",
        path.display(),
        write_result
            .err()
            .map(|error| format!(" (artifact write failed: {error})"))
            .unwrap_or_default()
    );
}

pub fn write_failure_artifact(path: &Path, artifact: &FailureArtifact) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("create artifact directory {}: {error}", parent.display()))?;
    }
    let json = serde_json::to_vec_pretty(artifact)
        .map_err(|error| format!("serialize failure artifact: {error}"))?;
    fs::write(path, json)
        .map_err(|error| format!("write failure artifact {}: {error}", path.display()))
}

fn artifact_path(test_name: &str) -> PathBuf {
    let safe_name = test_name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/terminal-test-artifacts")
        .join(format!("{safe_name}.json"))
}

pub fn decode_hex(input: &str) -> Result<Vec<u8>, String> {
    let compact = input
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .collect::<String>();
    if compact.len() % 2 != 0 {
        return Err("hex input has an odd number of digits".to_owned());
    }
    compact
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let pair = std::str::from_utf8(pair).expect("hex pairs are ASCII");
            u8::from_str_radix(pair, 16).map_err(|_| format!("invalid hex byte {pair:?}"))
        })
        .collect()
}

pub fn encode_hex(input: &[u8]) -> String {
    input.iter().map(|byte| format!("{byte:02X}")).collect()
}
