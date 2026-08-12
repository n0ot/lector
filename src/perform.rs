use vte::{Params, Perform};

/// A position in the retained primary-screen history. Rows are counted from
/// the oldest row still present in the scrollback buffer.
#[derive(Copy, Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct HistoryPosition {
    pub row: usize,
    pub col: u16,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Osc133Kind {
    PromptStart,
    InputStart,
    CommandStart,
    CommandFinished { exit_code: Option<i32> },
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Osc133Mark {
    pub kind: Osc133Kind,
    pub position: HistoryPosition,
    pub alternate_screen: bool,
}

/// Receives semantic-prompt sequences from `vt100` at the exact cursor
/// position at which they occur. Using the parser callback is important: one
/// PTY read can contain both a marker and enough output to move that marker
/// into scrollback before `View::process_changes` returns.
#[derive(Default)]
pub struct SemanticCallbacks {
    marks: Vec<Osc133Mark>,
}

impl SemanticCallbacks {
    pub fn marks(&self) -> &[Osc133Mark] {
        &self.marks
    }

    pub fn clear(&mut self) {
        self.marks.clear();
    }
}

impl vt100::Callbacks for SemanticCallbacks {
    fn unhandled_osc(&mut self, screen: &mut vt100::Screen, params: &[&[u8]]) {
        let Some(kind) = parse_osc133(params) else {
            return;
        };
        let (row, col) = screen.cursor_position();
        let history_rows = retained_scrollback_len(screen);
        self.marks.push(Osc133Mark {
            kind,
            position: HistoryPosition {
                row: history_rows + usize::from(row),
                col,
            },
            alternate_screen: screen.alternate_screen(),
        });
    }
}

fn parse_osc133(params: &[&[u8]]) -> Option<Osc133Kind> {
    let [b"133", marker, rest @ ..] = params else {
        return None;
    };
    match *marker {
        b"A" => Some(Osc133Kind::PromptStart),
        b"B" => Some(Osc133Kind::InputStart),
        b"C" => Some(Osc133Kind::CommandStart),
        b"D" => Some(Osc133Kind::CommandFinished {
            exit_code: rest
                .first()
                .and_then(|value| std::str::from_utf8(value).ok())
                .and_then(|value| value.parse().ok()),
        }),
        _ => None,
    }
}

/// `vt100` exposes the selected scrollback offset but not the number of rows
/// currently retained. Setting an arbitrarily large offset is documented to
/// clamp to that number, so briefly doing that gives us the retained length.
pub(crate) fn retained_scrollback_len(screen: &mut vt100::Screen) -> usize {
    let old_offset = screen.scrollback();
    screen.set_scrollback(usize::MAX);
    let len = screen.scrollback();
    screen.set_scrollback(old_offset);
    len
}

/// Processes text from VTE, storing new text to be printed.
pub struct Reporter {
    pub cursor_moves: usize,
    pub scrolled: bool,
}

impl Default for Reporter {
    fn default() -> Self {
        Self::new()
    }
}

impl Reporter {
    pub fn new() -> Self {
        Reporter {
            cursor_moves: 0,
            scrolled: false,
        }
    }

    pub fn reset(&mut self) {
        self.cursor_moves = 0;
        self.scrolled = false;
    }
}

impl Perform for Reporter {
    fn print(&mut self, _c: char) {
        // Nothing to do
    }

    fn execute(&mut self, byte: u8) {
        if byte == 8 {
            self.cursor_moves += 1
        }
    }

    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _c: char) {
        // Nothing to do
    }

    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {
        // Nothing to do
    }

    fn csi_dispatch(&mut self, _params: &Params, intermediates: &[u8], _ignore: bool, c: char) {
        if intermediates.is_empty() {
            match c {
                'A'..='H' => self.cursor_moves += 1,
                'S' | 'T' => self.scrolled = true,
                _ => {}
            }
        }
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, _byte: u8) {
        // Nothing to do
    }
}

#[cfg(test)]
mod tests {
    use super::{
        HistoryPosition, Osc133Kind, Reporter, SemanticCallbacks, retained_scrollback_len,
    };

    #[test]
    fn reports_cursor_movement_scrolling_and_reset() {
        let mut reporter = Reporter::default();
        let mut parser = vte::Parser::new();

        parser.advance(&mut reporter, b"text\x08\x1B[2A\x1B[H\x1B[S");

        assert_eq!(reporter.cursor_moves, 3);
        assert!(reporter.scrolled);

        reporter.reset();
        assert_eq!(reporter.cursor_moves, 0);
        assert!(!reporter.scrolled);
    }

    #[test]
    fn ignores_non_movement_and_intermediate_control_sequences() {
        let mut reporter = Reporter::new();
        let mut parser = vte::Parser::new();

        parser.advance(
            &mut reporter,
            b"plain\r\n\x1B[2J\x1B[ q\x1B]title\x07\x1B7\x1BPq\x1B\\",
        );

        assert_eq!(reporter.cursor_moves, 0);
        assert!(!reporter.scrolled);
    }

    #[test]
    fn semantic_callbacks_parse_all_osc133_markers_at_their_cursor_positions() {
        let mut parser = vt100::Parser::new_with_callbacks(2, 10, 20, SemanticCallbacks::default());
        parser.process(
            b"\x1B]133;A\x07$ \x1B]133;B\x07echo ok\r\n\x1B]133;C\x1B\\ok\r\n\x1B]133;D;7\x07",
        );

        let marks = parser.callbacks().marks();
        assert_eq!(marks.len(), 4);
        assert_eq!(marks[0].kind, Osc133Kind::PromptStart);
        assert_eq!(marks[0].position, HistoryPosition { row: 0, col: 0 });
        assert_eq!(marks[1].kind, Osc133Kind::InputStart);
        assert_eq!(marks[1].position, HistoryPosition { row: 0, col: 2 });
        assert_eq!(marks[2].kind, Osc133Kind::CommandStart);
        assert_eq!(marks[2].position, HistoryPosition { row: 1, col: 0 });
        assert_eq!(
            marks[3].kind,
            Osc133Kind::CommandFinished { exit_code: Some(7) }
        );
        assert_eq!(marks[3].position, HistoryPosition { row: 2, col: 0 });
        assert_eq!(retained_scrollback_len(parser.screen_mut()), 1);
    }

    #[test]
    fn semantic_callbacks_ignore_other_and_malformed_osc_sequences() {
        let mut parser = vt100::Parser::new_with_callbacks(2, 10, 20, SemanticCallbacks::default());
        parser.process(b"\x1B]0;title\x07\x1B]133;X\x07\x1B]133\x07");
        assert!(parser.callbacks().marks().is_empty());
    }
}
