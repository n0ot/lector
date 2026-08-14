use super::{Result, ViewAction, ViewController, ViewKind};
use crate::{
    review::{
        document::{ReviewDocument, SearchDirection},
        parser::{
            Command, FindDirection, Key, Motion, Parser, TextObject, ViewportPlacement, VisualKind,
        },
    },
    screen_reader::ScreenReader,
    terminal::HistoryPosition,
    terminal_input::KeyInput,
    view::View,
};
use std::{any::Any, io::Write};
use terminput::{KeyCode, KeyModifiers};
use unicode_segmentation::UnicodeSegmentation;

#[derive(Copy, Clone, Debug)]
struct LastFind {
    direction: FindDirection,
    till: bool,
    target: char,
    matched: HistoryPosition,
}

#[derive(Clone, Debug)]
struct LastSearch {
    query: String,
    direction: SearchDirection,
}

#[derive(Clone, Debug)]
struct SearchPrompt {
    query: String,
    direction: SearchDirection,
}

pub struct ReviewView {
    view: View,
    title: String,
    kind: ViewKind,
    document: ReviewDocument,
    parser: Parser,
    cursor: HistoryPosition,
    viewport_top: usize,
    rows: u16,
    cols: u16,
    visual_anchor: Option<HistoryPosition>,
    visual_kind: VisualKind,
    last_find: Option<LastFind>,
    last_search: Option<LastSearch>,
    search_prompt: Option<SearchPrompt>,
}

impl ReviewView {
    pub fn new(source: &mut View) -> Self {
        Self::new_with_identity(source, "Review", ViewKind::Review)
    }

    pub(crate) fn new_table_setup(source: &mut View, title: impl Into<String>) -> Self {
        Self::new_with_identity(source, title, ViewKind::TableSetup)
    }

    fn new_with_identity(source: &mut View, title: impl Into<String>, kind: ViewKind) -> Self {
        // Review opens at the source's independent review cursor, not at the
        // source application's cursor. render() exposes this as the overlay's
        // application cursor so normal cursor tracking starts from that point.
        let (document, cursor, viewport_top) = ReviewDocument::capture(source);
        let (rows, cols) = source.size();
        let mut review = Self {
            view: View::new(rows, cols),
            title: title.into(),
            kind,
            document,
            parser: Parser::default(),
            cursor,
            viewport_top,
            rows,
            cols,
            visual_anchor: None,
            visual_kind: VisualKind::Character,
            last_find: None,
            last_search: None,
            search_prompt: None,
        };
        review.ensure_cursor_visible();
        review.render();
        review
    }

    fn document_height(&self) -> usize {
        usize::from(self.rows).saturating_sub(usize::from(self.search_prompt.is_some()))
    }

    fn ensure_cursor_visible(&mut self) {
        let height = self.document_height().max(1);
        if self.cursor.row < self.viewport_top {
            self.viewport_top = self.cursor.row;
        } else if self.cursor.row >= self.viewport_top.saturating_add(height) {
            self.viewport_top = self.cursor.row.saturating_add(1).saturating_sub(height);
        }
        self.viewport_top = self
            .viewport_top
            .min(self.document.row_count().saturating_sub(1));
    }

    fn render(&mut self) {
        self.ensure_cursor_visible();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"\x1B[2J\x1B[H");
        let height = self.document_height();
        for screen_row in 0..height {
            let absolute_row = self.viewport_top.saturating_add(screen_row);
            if absolute_row >= self.document.row_count() {
                break;
            }
            bytes.extend_from_slice(format!("\x1B[{};1H", screen_row.saturating_add(1)).as_bytes());
            bytes.extend_from_slice(&self.document.formatted_row(absolute_row, self.cols));
            bytes.extend_from_slice(b"\x1B[0m");
        }

        if let Some(prompt) = &self.search_prompt {
            let marker = match prompt.direction {
                SearchDirection::Forward => '/',
                SearchDirection::Backward => '?',
            };
            let row = self.rows.max(1);
            bytes.extend_from_slice(format!("\x1B[{row};1H\x1B[2K{marker}").as_bytes());
            bytes.extend_from_slice(prompt.query.as_bytes());
            let col = 2usize
                .saturating_add(prompt.query.graphemes(true).count())
                .min(usize::from(self.cols).max(1));
            bytes.extend_from_slice(format!("\x1B[{row};{col}H\x1B[?25h").as_bytes());
        } else {
            let row = self
                .cursor
                .row
                .saturating_sub(self.viewport_top)
                .saturating_add(1)
                .min(usize::from(self.rows).max(1));
            let col = usize::from(self.cursor.col)
                .saturating_add(1)
                .min(usize::from(self.cols).max(1));
            bytes.extend_from_slice(format!("\x1B[{row};{col}H\x1B[?25h").as_bytes());
        }

        // Keep one View alive for the lifetime of the overlay. Its application
        // cursor is the visible terminal cursor, and preserving prev_screen lets
        // the shared cursor tracker report review motions just like PTY motions.
        self.view.clear_update_summary();
        self.view.process_changes(&bytes);
        self.view.clear_update_summary();
    }

    fn handle_search_key(&mut self, key: Key) -> Result<ViewAction> {
        match key {
            Key::Escape => {
                self.search_prompt = None;
                self.render();
                Ok(ViewAction::Redraw)
            }
            Key::Backspace => {
                let prompt = self.search_prompt.as_mut().expect("search prompt");
                let Some((index, _)) = prompt.query.grapheme_indices(true).next_back() else {
                    return Ok(ViewAction::Bell);
                };
                prompt.query.truncate(index);
                self.render();
                Ok(ViewAction::Redraw)
            }
            Key::Ctrl('u') => {
                let prompt = self.search_prompt.as_mut().expect("search prompt");
                if prompt.query.is_empty() {
                    return Ok(ViewAction::Bell);
                }
                prompt.query.clear();
                self.render();
                Ok(ViewAction::Redraw)
            }
            Key::Enter => self.finish_search(),
            Key::Char(ch) if !ch.is_control() => {
                self.search_prompt
                    .as_mut()
                    .expect("search prompt")
                    .query
                    .push(ch);
                self.render();
                Ok(ViewAction::Redraw)
            }
            _ => Ok(ViewAction::Bell),
        }
    }

    fn finish_search(&mut self) -> Result<ViewAction> {
        let prompt = self.search_prompt.as_ref().expect("search prompt");
        let query = if prompt.query.is_empty() {
            let Some(last) = &self.last_search else {
                return Ok(ViewAction::Bell);
            };
            last.query.clone()
        } else {
            prompt.query.clone()
        };
        let direction = prompt.direction;
        let Some(target) = self.document.search(&query, self.cursor, direction, 1) else {
            return Ok(ViewAction::Bell);
        };
        self.last_search = Some(LastSearch { query, direction });
        self.search_prompt = None;
        Ok(self.move_to(target))
    }

    fn handle_command(&mut self, sr: &mut ScreenReader, command: Command) -> Result<ViewAction> {
        match command {
            Command::None => Ok(ViewAction::None),
            Command::Bell => Ok(ViewAction::Bell),
            Command::Exit => Ok(ViewAction::Pop),
            Command::StartVisual(kind) => {
                self.visual_anchor = Some(self.cursor);
                self.visual_kind = kind;
                sr.speak(
                    match kind {
                        VisualKind::Character => "visual",
                        VisualKind::Line => "visual line",
                    },
                    false,
                )?;
                Ok(ViewAction::None)
            }
            Command::CancelVisual => {
                self.visual_anchor = None;
                sr.speak("visual cancelled", false)?;
                Ok(ViewAction::None)
            }
            Command::Move(motion, count) | Command::MoveVisual(motion, count) => {
                let Some(target) = self.motion_target(motion, count) else {
                    return Ok(ViewAction::Bell);
                };
                Ok(self.move_to(target))
            }
            Command::ScrollPage { forward, count } => Ok(self.scroll_page(forward, count)),
            Command::RepositionViewport {
                placement,
                line,
                first_nonblank,
            } => Ok(self.reposition_viewport(placement, line, first_nonblank)),
            Command::YankMotion(motion, count) => {
                let Some(target) = self.motion_target(motion, count) else {
                    return Ok(ViewAction::Bell);
                };
                let Some(text) = self.yank_motion_text(motion, target) else {
                    return Ok(ViewAction::Bell);
                };
                self.yank(sr, text)
            }
            Command::YankLine(count) => {
                let last_row = self
                    .cursor
                    .row
                    .saturating_add(count.saturating_sub(1))
                    .min(self.document.row_count().saturating_sub(1));
                let Some(text) = self.document.yank_range(
                    self.cursor,
                    HistoryPosition {
                        row: last_row,
                        col: self.document.line_last_col(last_row),
                    },
                    true,
                ) else {
                    return Ok(ViewAction::Bell);
                };
                self.yank(sr, text)
            }
            Command::YankTextObject(TextObject::Word { style, around }, count) => {
                let Some((first, last)) =
                    self.document
                        .inner_word_range(self.cursor, style, around, count)
                else {
                    return Ok(ViewAction::Bell);
                };
                let Some(text) = self.document.yank_range(first, last, false) else {
                    return Ok(ViewAction::Bell);
                };
                self.yank(sr, text)
            }
            Command::YankVisual => {
                let Some(anchor) = self.visual_anchor.take() else {
                    return Ok(ViewAction::Bell);
                };
                let Some(text) = self.document.yank_range(
                    anchor,
                    self.cursor,
                    self.visual_kind == VisualKind::Line,
                ) else {
                    return Ok(ViewAction::Bell);
                };
                self.yank(sr, text)
            }
            Command::StartSearch(direction) => {
                self.search_prompt = Some(SearchPrompt {
                    query: String::new(),
                    direction,
                });
                self.render();
                Ok(ViewAction::Redraw)
            }
            Command::RepeatSearch { reverse, count } => {
                let Some(last) = self.last_search.clone() else {
                    return Ok(ViewAction::Bell);
                };
                let direction = if reverse {
                    last.direction.reverse()
                } else {
                    last.direction
                };
                let Some(target) = self
                    .document
                    .search(&last.query, self.cursor, direction, count)
                else {
                    return Ok(ViewAction::Bell);
                };
                Ok(self.move_to(target))
            }
        }
    }

    fn motion_target(&mut self, motion: Motion, count: usize) -> Option<HistoryPosition> {
        let mut position = self.cursor;
        match motion {
            Motion::Left => self.document.move_horizontal(position, false, count),
            Motion::Right => self.document.move_horizontal(position, true, count),
            Motion::Up => self.document.move_vertical(position, false, count),
            Motion::Down => self.document.move_vertical(position, true, count),
            Motion::LineStart => {
                position.col = 0;
                (position != self.cursor).then_some(position)
            }
            Motion::FirstNonblank => {
                position.col = self.document.line_first_nonblank(position.row);
                (position != self.cursor).then_some(position)
            }
            Motion::LineEnd => {
                position.col = self.document.line_last_col(position.row);
                (position != self.cursor).then_some(position)
            }
            Motion::Word(movement, style) => {
                self.document.move_word(position, movement, style, count)
            }
            Motion::DocumentStart => {
                position.row = count
                    .saturating_sub(1)
                    .min(self.document.row_count().saturating_sub(1));
                position.col = self.document.line_first_nonblank(position.row);
                (position != self.cursor).then_some(position)
            }
            Motion::DocumentEnd => {
                position.row = if count == 1 {
                    self.document.row_count().saturating_sub(1)
                } else {
                    count
                        .saturating_sub(1)
                        .min(self.document.row_count().saturating_sub(1))
                };
                position.col = self.document.line_first_nonblank(position.row);
                (position != self.cursor).then_some(position)
            }
            Motion::MatchingBrace => {
                for _ in 0..count {
                    position = self.document.matching_brace(position)?;
                }
                (position != self.cursor).then_some(position)
            }
            Motion::Find {
                direction,
                till,
                target,
            } => {
                let forward = direction == FindDirection::Forward;
                let matched = self
                    .document
                    .find_character(position, target, forward, false, count)?;
                let destination = if till {
                    self.document.adjacent(matched, !forward)?
                } else {
                    matched
                };
                if destination == self.cursor {
                    return None;
                }
                self.last_find = Some(LastFind {
                    direction,
                    till,
                    target,
                    matched,
                });
                Some(destination)
            }
            Motion::RepeatFind { reverse } => {
                let mut find = self.last_find?;
                let forward = (find.direction == FindDirection::Forward) != reverse;
                let start = if reverse { position } else { find.matched };
                let matched =
                    self.document
                        .find_character(start, find.target, forward, false, count)?;
                let destination = if find.till {
                    self.document.adjacent(matched, !forward)?
                } else {
                    matched
                };
                if destination == self.cursor {
                    return None;
                }
                find.matched = matched;
                self.last_find = Some(find);
                Some(destination)
            }
            Motion::Prompt { forward } => self.document.prompt(position, forward, count),
        }
    }

    fn scroll_page(&mut self, forward: bool, count: usize) -> ViewAction {
        let distance = self.document_height().max(1).saturating_mul(count);
        let target = self.document.move_vertical(self.cursor, forward, distance);
        let Some(target) = target else {
            return ViewAction::Bell;
        };
        self.move_to(target)
    }

    fn reposition_viewport(
        &mut self,
        placement: ViewportPlacement,
        line: Option<usize>,
        first_nonblank: bool,
    ) -> ViewAction {
        if let Some(line) = line {
            self.cursor.row = line
                .saturating_sub(1)
                .min(self.document.row_count().saturating_sub(1));
            self.cursor = self.document.clamp(self.cursor);
        }
        if first_nonblank {
            self.cursor.col = self.document.line_first_nonblank(self.cursor.row);
        }

        let height = self.document_height().max(1);
        self.viewport_top = match placement {
            ViewportPlacement::Top => self.cursor.row,
            ViewportPlacement::Center => self.cursor.row.saturating_sub(height / 2),
            ViewportPlacement::Bottom => self.cursor.row.saturating_sub(height.saturating_sub(1)),
        };
        self.render();
        ViewAction::Redraw
    }

    fn yank_motion_text(&self, motion: Motion, target: HistoryPosition) -> Option<String> {
        if matches!(
            motion,
            Motion::Up | Motion::Down | Motion::DocumentStart | Motion::DocumentEnd
        ) {
            return self.document.yank_range(self.cursor, target, true);
        }

        let exclusive = matches!(
            motion,
            Motion::Left
                | Motion::Right
                | Motion::LineStart
                | Motion::FirstNonblank
                | Motion::Word(crate::review::document::WordMove::ForwardStart, _)
                | Motion::Word(crate::review::document::WordMove::BackwardStart, _)
                | Motion::Prompt { .. }
        );
        if !exclusive {
            return self.document.yank_range(self.cursor, target, false);
        }

        let (first, last) = if target > self.cursor {
            (self.cursor, self.document.previous_position(target)?)
        } else {
            (target, self.document.previous_position(self.cursor)?)
        };
        self.document.yank_range(first, last, false)
    }

    fn move_to(&mut self, target: HistoryPosition) -> ViewAction {
        self.cursor = self.document.clamp(target);
        self.render();
        ViewAction::Redraw
    }

    fn yank(&mut self, sr: &mut ScreenReader, text: String) -> Result<ViewAction> {
        sr.push_clipboard(text)?;
        sr.speak("copied", false)?;
        self.visual_anchor = None;
        Ok(ViewAction::None)
    }

    fn handle_review_key(&mut self, sr: &mut ScreenReader, key: Key) -> Result<ViewAction> {
        if self.search_prompt.is_some() {
            self.handle_search_key(key)
        } else {
            let command = self.parser.feed(key);
            self.handle_command(sr, command)
        }
    }

    fn handle_keys(
        &mut self,
        sr: &mut ScreenReader,
        keys: impl IntoIterator<Item = Key>,
    ) -> Result<ViewAction> {
        let mut result = ViewAction::None;
        for key in keys {
            let action = self.handle_review_key(sr, key)?;
            match action {
                ViewAction::Pop | ViewAction::Bell => return Ok(action),
                ViewAction::Redraw => result = ViewAction::Redraw,
                ViewAction::None => {}
                ViewAction::PtyInput
                | ViewAction::Push(_)
                | ViewAction::PopupResponse(_)
                | ViewAction::ActivateTmuxConnection(_)
                | ViewAction::ActivateTerminal
                | ViewAction::TmuxConnectionRename { .. }
                | ViewAction::TmuxChooserSelect { .. }
                | ViewAction::TmuxCommandSubmit { .. }
                | ViewAction::TmuxInput { .. } => {
                    unreachable!()
                }
            }
        }
        Ok(result)
    }
}

impl ViewController for ReviewView {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn model(&mut self) -> &mut View {
        &mut self.view
    }

    fn title(&self) -> &str {
        &self.title
    }

    fn kind(&self) -> ViewKind {
        self.kind
    }

    fn place_application_cursor_at_review_cursor(&mut self) -> Option<ViewAction> {
        let (row, col) = self.view.review_cursor_position();
        let target = HistoryPosition {
            row: self.viewport_top.saturating_add(usize::from(row)),
            col,
        };
        Some(self.move_to(target))
    }

    fn handle_input(
        &mut self,
        sr: &mut ScreenReader,
        input: &[u8],
        _pty_stream: &mut dyn Write,
    ) -> Result<ViewAction> {
        let keys = input.iter().copied().map(raw_key);
        self.handle_keys(sr, keys)
    }

    fn handle_key_input(
        &mut self,
        sr: &mut ScreenReader,
        key: &KeyInput,
        _raw: &[u8],
        _pty_stream: &mut dyn Write,
    ) -> Result<ViewAction> {
        if key.is_release() {
            return Ok(ViewAction::None);
        }
        if let Some(text) = key.text() {
            return self.handle_keys(sr, text.chars().map(Key::Char));
        }
        self.handle_review_key(sr, semantic_key(key))
    }

    fn handle_paste(
        &mut self,
        sr: &mut ScreenReader,
        contents: &str,
        _pty_stream: &mut dyn Write,
    ) -> Result<ViewAction> {
        if self.search_prompt.is_none() {
            return Ok(ViewAction::Bell);
        }
        let chars = contents
            .chars()
            .filter(|ch| !ch.is_control())
            .map(Key::Char);
        self.handle_keys(sr, chars)
    }

    fn on_resize(&mut self, rows: u16, cols: u16) {
        self.rows = rows;
        self.cols = cols;
        self.view.set_size(rows, cols);
        self.cursor.col = self
            .cursor
            .col
            .min(self.document.capture_cols().min(cols).saturating_sub(1));
        self.ensure_cursor_visible();
        self.render();
    }
}

fn semantic_key(key: &KeyInput) -> Key {
    let event = key.normalized_event();
    if let Some(code) = key.control_code() {
        return match code {
            0x1B => Key::Escape,
            0x02 => Key::Ctrl('b'),
            0x04 => Key::Ctrl('d'),
            0x06 => Key::Ctrl('f'),
            0x15 => Key::Ctrl('u'),
            _ => Key::Unknown,
        };
    }
    if event.modifiers.intersects(
        KeyModifiers::CTRL
            | KeyModifiers::ALT
            | KeyModifiers::META
            | KeyModifiers::SUPER
            | KeyModifiers::HYPER,
    ) {
        return Key::Unknown;
    }
    match event.code {
        KeyCode::Char(ch) => Key::Char(ch),
        KeyCode::Esc => Key::Escape,
        KeyCode::Enter => Key::Enter,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Left => Key::Left,
        KeyCode::Down => Key::Down,
        KeyCode::Up => Key::Up,
        KeyCode::Right => Key::Right,
        _ => Key::Unknown,
    }
}

fn raw_key(byte: u8) -> Key {
    match byte {
        0x1B => Key::Escape,
        b'\r' | b'\n' => Key::Enter,
        0x08 | 0x7F => Key::Backspace,
        0x02 => Key::Ctrl('b'),
        0x04 => Key::Ctrl('d'),
        0x06 => Key::Ctrl('f'),
        0x15 => Key::Ctrl('u'),
        value if value.is_ascii() && !value.is_ascii_control() => Key::Char(value as char),
        _ => Key::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::ReviewView;
    use crate::{
        screen_reader::ScreenReader,
        speech,
        terminal::HistoryPosition,
        view::View,
        views::{ViewAction, ViewController, ViewKind},
    };
    use std::{cell::RefCell, rc::Rc};

    struct RecordingDriver(Rc<RefCell<Vec<String>>>);

    impl speech::Driver for RecordingDriver {
        fn speak(&mut self, text: &str, _interrupt: bool) -> anyhow::Result<()> {
            self.0.borrow_mut().push(text.to_string());
            Ok(())
        }
        fn stop(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
        fn get_rate(&self) -> f32 {
            1.0
        }
        fn set_rate(&mut self, _rate: f32) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn setup(text: &[u8]) -> (ReviewView, ScreenReader, Rc<RefCell<Vec<String>>>) {
        let mut source = View::new(3, 30);
        source.process_changes(text);
        source.set_review_history_position(crate::terminal::HistoryPosition { row: 0, col: 0 });
        let output = Rc::new(RefCell::new(Vec::new()));
        let sr = ScreenReader::new(speech::Speech::new(Box::new(RecordingDriver(
            output.clone(),
        ))));
        (ReviewView::new(&mut source), sr, output)
    }

    fn input(view: &mut ReviewView, sr: &mut ScreenReader, bytes: &[u8]) -> ViewAction {
        view.handle_input(sr, bytes, &mut Vec::new()).unwrap()
    }

    #[test]
    fn review_is_read_only_and_only_q_exits() {
        let (mut view, mut sr, _) = setup(b"abc");
        assert_eq!(view.kind(), ViewKind::Review);
        assert!(matches!(input(&mut view, &mut sr, b"x"), ViewAction::Bell));
        assert!(matches!(
            input(&mut view, &mut sr, b"\x1b"),
            ViewAction::Bell
        ));
        assert!(matches!(input(&mut view, &mut sr, b"q"), ViewAction::Pop));
    }

    #[test]
    fn review_application_cursor_starts_at_the_source_review_cursor() {
        let mut source = View::new(3, 20);
        source.process_changes(b"zero\r\none\r\ntwo\r\nthree\r\nfour\r\nfive");
        source.set_review_history_position(HistoryPosition { row: 1, col: 2 });
        let expected = source.review_history_position();

        let review = ReviewView::new(&mut source);

        assert_eq!(review.cursor, expected);
        assert_eq!(
            review.view.screen().cursor_position(),
            (
                expected.row.saturating_sub(review.viewport_top) as u16,
                expected.col
            )
        );
    }

    #[test]
    fn escape_cancels_pending_command_without_closing() {
        let (mut view, mut sr, _) = setup(b"abc");
        assert!(matches!(input(&mut view, &mut sr, b"f"), ViewAction::None));
        assert!(matches!(
            input(&mut view, &mut sr, b"\x1b"),
            ViewAction::None
        ));
        assert!(matches!(
            input(&mut view, &mut sr, b"\x1b"),
            ViewAction::Bell
        ));
    }

    #[test]
    fn failed_motion_and_find_bell() {
        let (mut view, mut sr, _) = setup(b"abc");
        assert!(matches!(input(&mut view, &mut sr, b"h"), ViewAction::Bell));
        assert!(matches!(input(&mut view, &mut sr, b"fz"), ViewAction::Bell));
    }

    #[test]
    fn vi_motions_do_not_use_or_move_the_independent_review_cursor() {
        let (mut view, mut sr, _) = setup(b"abcd");
        view.model().set_review_cursor_col(2);

        assert!(matches!(
            input(&mut view, &mut sr, b"l"),
            ViewAction::Redraw
        ));
        assert_eq!(view.cursor.col, 1);
        assert_eq!(view.model().review_cursor_position(), (0, 2));
    }

    #[test]
    fn z_commands_place_the_cursor_line_in_the_viewport() {
        let mut source = View::new(5, 20);
        source.process_changes(
            b"zero\r\none\r\ntwo\r\nthree\r\nfour\r\nfive\r\n  six\r\nseven\r\neight",
        );
        source.set_review_history_position(HistoryPosition { row: 6, col: 4 });
        let output = Rc::new(RefCell::new(Vec::new()));
        let mut sr = ScreenReader::new(speech::Speech::new(Box::new(RecordingDriver(output))));
        let mut view = ReviewView::new(&mut source);

        assert!(matches!(
            input(&mut view, &mut sr, b"zt"),
            ViewAction::Redraw
        ));
        assert_eq!(view.viewport_top, 6);
        assert_eq!(view.model().screen().cursor_position(), (0, 4));

        assert!(matches!(
            input(&mut view, &mut sr, b"zz"),
            ViewAction::Redraw
        ));
        assert_eq!(view.viewport_top, 4);
        assert_eq!(view.model().screen().cursor_position(), (2, 4));

        assert!(matches!(
            input(&mut view, &mut sr, b"zb"),
            ViewAction::Redraw
        ));
        assert_eq!(view.viewport_top, 2);
        assert_eq!(view.model().screen().cursor_position(), (4, 4));

        assert!(matches!(
            input(&mut view, &mut sr, b"z\r"),
            ViewAction::Redraw
        ));
        assert_eq!(view.viewport_top, 6);
        assert_eq!(view.cursor.col, 2);

        assert!(matches!(
            input(&mut view, &mut sr, b"3z."),
            ViewAction::Redraw
        ));
        assert_eq!(view.cursor, HistoryPosition { row: 2, col: 0 });
        assert_eq!(view.viewport_top, 0);
        assert_eq!(view.model().screen().cursor_position(), (2, 0));
    }

    #[test]
    fn find_and_till_repeats_track_the_matched_character() {
        let (mut view, mut sr, _) = setup(b"a1x2x");
        assert!(matches!(
            input(&mut view, &mut sr, b"tx"),
            ViewAction::Redraw
        ));
        assert_eq!(view.cursor.col, 1);
        assert!(matches!(
            input(&mut view, &mut sr, b";"),
            ViewAction::Redraw
        ));
        assert_eq!(view.cursor.col, 3);

        let (mut view, mut sr, _) = setup(b"a1x2x");
        assert!(matches!(
            input(&mut view, &mut sr, b"fx;"),
            ViewAction::Redraw
        ));
        assert_eq!(view.cursor.col, 4);
        assert!(matches!(
            input(&mut view, &mut sr, b","),
            ViewAction::Redraw
        ));
        assert_eq!(view.cursor.col, 2);

        assert!(matches!(input(&mut view, &mut sr, b"fz"), ViewAction::Bell));
        assert!(matches!(
            input(&mut view, &mut sr, b";"),
            ViewAction::Redraw
        ));
        assert_eq!(view.cursor.col, 4);
    }

    #[test]
    fn count_motion_and_percent_move_the_cursor() {
        let (mut view, mut sr, _) = setup(b"one two {x}");
        assert!(matches!(
            input(&mut view, &mut sr, b"2w"),
            ViewAction::Redraw
        ));
        assert_eq!(view.cursor.col, 8);
        assert!(matches!(
            input(&mut view, &mut sr, b"%"),
            ViewAction::Redraw
        ));
        assert_eq!(view.cursor.col, 10);
    }

    #[test]
    fn inner_word_yank_uses_the_internal_clipboard() {
        let (mut view, mut sr, spoken) = setup(b"alpha beta");
        assert!(matches!(
            input(&mut view, &mut sr, b"yiw"),
            ViewAction::None
        ));
        assert_eq!(sr.clipboard_text(), Some("alpha"));
        assert!(spoken.borrow().iter().any(|text| text == "copied"));
    }

    #[test]
    fn yank_motions_counts_lines_and_visual_ranges_use_vi_boundaries() {
        let (mut view, mut sr, _) = setup(b"alpha beta\r\ngamma");
        assert!(matches!(
            input(&mut view, &mut sr, b"y2l"),
            ViewAction::None
        ));
        assert_eq!(sr.clipboard_text(), Some("al"));

        assert!(matches!(
            input(&mut view, &mut sr, b"2yiw"),
            ViewAction::None
        ));
        assert_eq!(sr.clipboard_text(), Some("alpha beta"));

        assert!(matches!(
            input(&mut view, &mut sr, b"vey"),
            ViewAction::Redraw
        ));
        assert_eq!(sr.clipboard_text(), Some("alpha"));

        assert!(matches!(
            input(&mut view, &mut sr, b"2yy"),
            ViewAction::None
        ));
        assert_eq!(sr.clipboard_text(), Some("alpha beta\ngamma\n"));
    }

    #[test]
    fn page_and_edge_line_motions_reveal_frozen_scrollback() {
        let mut source = View::new(3, 20);
        source.process_changes(b"zero\r\none\r\ntwo\r\nthree\r\nfour\r\nfive");
        source.follow_application_cursor();
        let output = Rc::new(RefCell::new(Vec::new()));
        let mut sr = ScreenReader::new(speech::Speech::new(Box::new(RecordingDriver(output))));
        let mut view = ReviewView::new(&mut source);
        assert_eq!(view.viewport_top, source.scrollback_len());

        assert!(matches!(
            input(&mut view, &mut sr, b"\x02"),
            ViewAction::Redraw
        ));
        assert!(view.viewport_top < source.scrollback_len());
        assert!(view.model().contents_full().contains("two"));

        assert!(matches!(
            input(&mut view, &mut sr, b"\x06"),
            ViewAction::Redraw
        ));
        assert_eq!(view.cursor.row, view.document.row_count() - 1);

        source.set_review_history_position(HistoryPosition {
            row: source.scrollback_len(),
            col: 0,
        });
        let mut view = ReviewView::new(&mut source);
        let old_top = view.viewport_top;
        assert!(matches!(
            input(&mut view, &mut sr, b"k"),
            ViewAction::Redraw
        ));
        assert_eq!(view.viewport_top + 1, old_top);
    }

    #[test]
    fn forward_backward_and_repeat_search_work() {
        let (mut view, mut sr, _) = setup(b"alpha beta alpha");
        assert!(matches!(
            input(&mut view, &mut sr, b"/alpha\r"),
            ViewAction::Redraw
        ));
        assert_eq!(view.cursor.col, 11);
        assert!(matches!(
            input(&mut view, &mut sr, b"n"),
            ViewAction::Redraw
        ));
        assert_eq!(view.cursor.col, 0);
        assert!(matches!(
            input(&mut view, &mut sr, b"N"),
            ViewAction::Redraw
        ));
        assert_eq!(view.cursor.col, 11);

        let (mut view, mut sr, _) = setup(b"alpha beta alpha");
        assert!(matches!(
            input(&mut view, &mut sr, b"?alpha\r"),
            ViewAction::Redraw
        ));
        assert_eq!(view.cursor.col, 11);
    }

    #[test]
    fn search_errors_bell_and_escape_only_cancels_the_prompt() {
        let (mut view, mut sr, _) = setup(b"alpha");
        assert!(matches!(input(&mut view, &mut sr, b"n"), ViewAction::Bell));
        assert!(matches!(
            input(&mut view, &mut sr, b"/missing\r"),
            ViewAction::Bell
        ));
        assert!(view.search_prompt.is_some());
        assert!(matches!(
            input(&mut view, &mut sr, b"\x1b"),
            ViewAction::Redraw
        ));
        assert!(view.search_prompt.is_none());

        assert!(matches!(
            input(&mut view, &mut sr, b"/[\r"),
            ViewAction::Bell
        ));
    }

    #[test]
    fn missing_prompt_and_unmatched_brace_are_bell_errors() {
        let (mut view, mut sr, _) = setup(b"{broken");
        assert!(matches!(input(&mut view, &mut sr, b"]p"), ViewAction::Bell));
        assert!(matches!(input(&mut view, &mut sr, b"%"), ViewAction::Bell));
    }

    #[test]
    fn screen_snapshot_stays_frozen() {
        let mut source = View::new(2, 20);
        source.process_changes(b"before");
        let mut review = ReviewView::new(&mut source);
        source.process_changes(b" after");
        assert!(review.model().contents_full().contains("before"));
        assert!(!review.model().contents_full().contains("after"));
    }

    #[test]
    fn resize_crops_the_snapshot_without_reflowing_or_reopening_it() {
        let (mut view, _sr, _) = setup(b"abcdefghij\r\nsecond");
        view.cursor.col = 9;

        view.on_resize(2, 5);

        assert_eq!(view.model().size(), (2, 5));
        assert_eq!(view.cursor.col, 4);
        assert!(view.model().contents_full().contains("abcde"));
        assert!(!view.model().contents_full().contains("fghij"));
    }
}
