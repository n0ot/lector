use super::document::{SearchDirection, WordMove, WordStyle};
use super::table::CellMove;
use crate::clipboard::ClipboardRegister;

const MAX_COUNT: usize = 10_000;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum FindDirection {
    Forward,
    Backward,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum Motion {
    Left,
    Down,
    Up,
    Right,
    LineStart,
    FirstNonblank,
    LineEnd,
    Word(WordMove, WordStyle),
    DocumentStart,
    DocumentEnd,
    MatchingBrace,
    Find {
        direction: FindDirection,
        till: bool,
        target: char,
    },
    RepeatFind {
        reverse: bool,
    },
    Prompt {
        forward: bool,
    },
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum TextObject {
    Word { style: WordStyle, around: bool },
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum VisualKind {
    Character,
    Line,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum ViewportPlacement {
    Top,
    Center,
    Bottom,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Command {
    None,
    Bell,
    AnnounceMode,
    Exit,
    SetMark(char),
    JumpMark {
        name: char,
        exact: bool,
    },
    Move(Motion, usize),
    ScrollPage {
        forward: bool,
        count: usize,
    },
    RepositionViewport {
        placement: ViewportPlacement,
        line: Option<usize>,
        first_nonblank: bool,
    },
    YankMotion(Motion, usize, Option<ClipboardRegister>),
    YankLine(usize, Option<ClipboardRegister>),
    YankTextObject(TextObject, usize, Option<ClipboardRegister>),
    StartVisual(VisualKind),
    CancelVisual,
    MoveVisual(Motion, usize),
    YankVisual(Option<ClipboardRegister>),
    StartSearch(SearchDirection),
    RepeatSearch {
        reverse: bool,
        count: usize,
    },
    OpenLink,
    CopyLink(ClipboardRegister),
    DetectTable,
    StartTableSetup,
    MarkTableBottom,
    MarkTableRight,
    ToggleTableRowHeader,
    MoveTableCell(CellMove, usize),
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum Key {
    Char(char),
    Escape,
    Enter,
    Backspace,
    Left,
    Down,
    Up,
    Right,
    Ctrl(char),
    Unknown,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Prefix {
    None,
    G,
    Z,
    SetMark,
    JumpMark {
        exact: bool,
    },
    Bracket {
        forward: bool,
    },
    Brace {
        forward: bool,
    },
    Find {
        direction: FindDirection,
        till: bool,
    },
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum OperatorState {
    None,
    Yank {
        count: usize,
        register: Option<ClipboardRegister>,
        prefix: Prefix,
        motion_count: Option<usize>,
        text_object_around: Option<bool>,
    },
}

pub(crate) struct Parser {
    count: Option<usize>,
    prefix: Prefix,
    operator: OperatorState,
    visual: Option<VisualKind>,
    register: Option<ClipboardRegister>,
    awaiting_register: bool,
}

impl Default for Parser {
    fn default() -> Self {
        Self {
            count: None,
            prefix: Prefix::None,
            operator: OperatorState::None,
            visual: None,
            register: None,
            awaiting_register: false,
        }
    }
}

impl Parser {
    pub(crate) fn feed(&mut self, key: Key) -> Command {
        if key == Key::Escape {
            return self.escape();
        }
        if self.awaiting_register {
            return self.feed_register(key);
        }
        if !matches!(self.operator, OperatorState::None) {
            return self.feed_operator(key);
        }
        if self.prefix != Prefix::None {
            return self.feed_prefix(key);
        }
        if self.visual.is_some() {
            return self.feed_visual(key);
        }
        self.feed_normal(key)
    }

    fn escape(&mut self) -> Command {
        if self.visual.take().is_some() {
            self.reset_pending();
            return Command::CancelVisual;
        }
        if self.count.is_some()
            || self.prefix != Prefix::None
            || !matches!(self.operator, OperatorState::None)
            || self.register.is_some()
            || self.awaiting_register
        {
            self.reset_pending();
            return Command::None;
        }
        Command::AnnounceMode
    }

    fn feed_normal(&mut self, key: Key) -> Command {
        if let Key::Char(digit @ '0'..='9') = key
            && (digit != '0' || self.count.is_some())
        {
            self.add_count(digit);
            return Command::None;
        }

        match key {
            Key::Char('q') => self.finish(Command::Exit),
            Key::Char('m') => {
                self.prefix = Prefix::SetMark;
                Command::None
            }
            Key::Char('\'') | Key::Char('`') => {
                self.prefix = Prefix::JumpMark {
                    exact: key == Key::Char('`'),
                };
                Command::None
            }
            Key::Char('"') => {
                self.awaiting_register = true;
                Command::None
            }
            Key::Char('y') => {
                let count = self.take_count();
                self.operator = OperatorState::Yank {
                    count,
                    register: self.register.take(),
                    prefix: Prefix::None,
                    motion_count: None,
                    text_object_around: None,
                };
                Command::None
            }
            Key::Char('v') => {
                self.visual = Some(VisualKind::Character);
                self.finish(Command::StartVisual(VisualKind::Character))
            }
            Key::Char('V') => {
                self.visual = Some(VisualKind::Line);
                self.finish(Command::StartVisual(VisualKind::Line))
            }
            Key::Char('g') => {
                self.prefix = Prefix::G;
                Command::None
            }
            Key::Char('z') => {
                self.prefix = Prefix::Z;
                Command::None
            }
            Key::Char('[') => {
                self.prefix = Prefix::Bracket { forward: false };
                Command::None
            }
            Key::Char(']') => {
                self.prefix = Prefix::Bracket { forward: true };
                Command::None
            }
            Key::Char('{') => {
                self.prefix = Prefix::Brace { forward: false };
                Command::None
            }
            Key::Char('}') => {
                self.prefix = Prefix::Brace { forward: true };
                Command::None
            }
            Key::Char('f') => self.start_find(FindDirection::Forward, false),
            Key::Char('F') => self.start_find(FindDirection::Backward, false),
            Key::Char('t') => self.start_find(FindDirection::Forward, true),
            Key::Char('T') => self.start_find(FindDirection::Backward, true),
            Key::Char('/') => {
                let direction = SearchDirection::Forward;
                self.finish(Command::StartSearch(direction))
            }
            Key::Char('?') => {
                let direction = SearchDirection::Backward;
                self.finish(Command::StartSearch(direction))
            }
            Key::Char('n') => {
                let count = self.take_count();
                self.finish(Command::RepeatSearch {
                    reverse: false,
                    count,
                })
            }
            Key::Char('N') => {
                let count = self.take_count();
                self.finish(Command::RepeatSearch {
                    reverse: true,
                    count,
                })
            }
            Key::Ctrl('b') => self.page(false),
            Key::Ctrl('f') => self.page(true),
            Key::Ctrl('u') => self.page(false),
            Key::Ctrl('d') => self.page(true),
            key => match motion_for_key(key) {
                Some(motion) => {
                    let count = self.take_count();
                    self.finish(Command::Move(motion, count))
                }
                None => self.finish(Command::Bell),
            },
        }
    }

    fn feed_visual(&mut self, key: Key) -> Command {
        if let Key::Char(digit @ '0'..='9') = key
            && (digit != '0' || self.count.is_some())
        {
            self.add_count(digit);
            return Command::None;
        }
        match key {
            Key::Char('y') => {
                self.visual = None;
                self.finish(Command::YankVisual(self.register))
            }
            Key::Char('"') => {
                self.awaiting_register = true;
                Command::None
            }
            Key::Char('v') | Key::Char('V') => {
                let kind = if key == Key::Char('v') {
                    VisualKind::Character
                } else {
                    VisualKind::Line
                };
                if self.visual == Some(kind) {
                    self.visual = None;
                    self.finish(Command::CancelVisual)
                } else {
                    self.visual = Some(kind);
                    self.finish(Command::StartVisual(kind))
                }
            }
            Key::Char('g') => {
                self.prefix = Prefix::G;
                Command::None
            }
            Key::Char('z') => {
                self.prefix = Prefix::Z;
                Command::None
            }
            Key::Char('[') => {
                self.prefix = Prefix::Bracket { forward: false };
                Command::None
            }
            Key::Char(']') => {
                self.prefix = Prefix::Bracket { forward: true };
                Command::None
            }
            Key::Char('f') => self.start_find(FindDirection::Forward, false),
            Key::Char('F') => self.start_find(FindDirection::Backward, false),
            Key::Char('t') => self.start_find(FindDirection::Forward, true),
            Key::Char('T') => self.start_find(FindDirection::Backward, true),
            key => match motion_for_key(key) {
                Some(motion) => {
                    let count = self.take_count();
                    self.finish(Command::MoveVisual(motion, count))
                }
                None => self.finish(Command::Bell),
            },
        }
    }

    fn feed_prefix(&mut self, key: Key) -> Command {
        let prefix = std::mem::replace(&mut self.prefix, Prefix::None);
        let pending_count = self.count.take();
        if matches!(prefix, Prefix::SetMark | Prefix::JumpMark { .. }) {
            return self.finish(match (prefix, key) {
                (Prefix::SetMark, Key::Char(name @ 'a'..='z')) => Command::SetMark(name),
                (Prefix::JumpMark { exact }, Key::Char(name @ 'a'..='z')) => {
                    Command::JumpMark { name, exact }
                }
                _ => Command::Bell,
            });
        }
        if prefix == Prefix::Z {
            let (placement, first_nonblank) = match key {
                Key::Char('t') => (ViewportPlacement::Top, false),
                Key::Enter => (ViewportPlacement::Top, true),
                Key::Char('z') => (ViewportPlacement::Center, false),
                Key::Char('.') => (ViewportPlacement::Center, true),
                Key::Char('b') => (ViewportPlacement::Bottom, false),
                Key::Char('-') => (ViewportPlacement::Bottom, true),
                _ => {
                    self.reset_pending();
                    return Command::Bell;
                }
            };
            return self.finish(Command::RepositionViewport {
                placement,
                line: pending_count,
                first_nonblank,
            });
        }
        let count = pending_count.unwrap_or(1).max(1);
        if self.visual.is_none() {
            let command = match (prefix, key) {
                (Prefix::G, Key::Char('x')) => Some(Command::OpenLink),
                (Prefix::G, Key::Char('y')) => Some(Command::CopyLink(
                    self.register.unwrap_or(ClipboardRegister::Internal),
                )),
                (Prefix::G, Key::Char('t')) => Some(Command::DetectTable),
                (Prefix::G, Key::Char('T')) => Some(Command::StartTableSetup),
                (Prefix::G, Key::Char('B')) => Some(Command::MarkTableBottom),
                (Prefix::G, Key::Char('R')) => Some(Command::MarkTableRight),
                (Prefix::G, Key::Char('H')) => Some(Command::ToggleTableRowHeader),
                (Prefix::Bracket { forward: false }, Key::Char('|')) => {
                    Some(Command::MoveTableCell(CellMove::Previous, count))
                }
                (Prefix::Bracket { forward: true }, Key::Char('|')) => {
                    Some(Command::MoveTableCell(CellMove::Next, count))
                }
                (Prefix::Brace { forward: false }, Key::Char('|')) => {
                    Some(Command::MoveTableCell(CellMove::Up, count))
                }
                (Prefix::Brace { forward: true }, Key::Char('|')) => {
                    Some(Command::MoveTableCell(CellMove::Down, count))
                }
                _ => None,
            };
            if let Some(command) = command {
                return self.finish(command);
            }
        }
        let motion = match (prefix, key) {
            (Prefix::G, Key::Char('g')) => Some(Motion::DocumentStart),
            (Prefix::Bracket { forward }, Key::Char('p')) => Some(Motion::Prompt { forward }),
            (Prefix::Find { direction, till }, Key::Char(target)) => Some(Motion::Find {
                direction,
                till,
                target,
            }),
            (Prefix::Z, _) => None,
            (Prefix::Brace { .. }, _) => None,
            _ => None,
        };
        let Some(motion) = motion else {
            self.reset_pending();
            return Command::Bell;
        };
        if self.visual.is_some() {
            self.finish(Command::MoveVisual(motion, count))
        } else {
            self.finish(Command::Move(motion, count))
        }
    }

    fn feed_operator(&mut self, key: Key) -> Command {
        let OperatorState::Yank {
            count,
            register,
            mut prefix,
            mut motion_count,
            mut text_object_around,
        } = self.operator
        else {
            unreachable!();
        };

        if let Key::Char(digit @ '0'..='9') = key
            && (digit != '0' || motion_count.is_some())
        {
            let digit = digit.to_digit(10).unwrap_or(0) as usize;
            motion_count = Some(
                motion_count
                    .unwrap_or(0)
                    .saturating_mul(10)
                    .saturating_add(digit)
                    .min(MAX_COUNT),
            );
            self.operator = OperatorState::Yank {
                count,
                register,
                prefix,
                motion_count,
                text_object_around,
            };
            return Command::None;
        }

        if let Some(around) = text_object_around {
            let style = match key {
                Key::Char('w') => Some(WordStyle::Word),
                Key::Char('W') => Some(WordStyle::BigWord),
                _ => None,
            };
            let Some(style) = style else {
                self.reset_pending();
                return Command::Bell;
            };
            let count = multiply_counts(count, motion_count.unwrap_or(1));
            return self.finish(Command::YankTextObject(
                TextObject::Word { style, around },
                count,
                register,
            ));
        }

        if prefix != Prefix::None {
            let motion = match (prefix, key) {
                (Prefix::G, Key::Char('g')) => Some(Motion::DocumentStart),
                (Prefix::Bracket { forward }, Key::Char('p')) => Some(Motion::Prompt { forward }),
                (Prefix::Find { direction, till }, Key::Char(target)) => Some(Motion::Find {
                    direction,
                    till,
                    target,
                }),
                (Prefix::Z, _) => None,
                (Prefix::Brace { .. }, _) => None,
                _ => None,
            };
            let Some(motion) = motion else {
                self.reset_pending();
                return Command::Bell;
            };
            let count = multiply_counts(count, motion_count.unwrap_or(1));
            return self.finish(Command::YankMotion(motion, count, register));
        }

        match key {
            Key::Char('y') => {
                let count = multiply_counts(count, motion_count.unwrap_or(1));
                self.finish(Command::YankLine(count, register))
            }
            Key::Char('i') => {
                text_object_around = Some(false);
                self.operator = OperatorState::Yank {
                    count,
                    register,
                    prefix,
                    motion_count,
                    text_object_around,
                };
                Command::None
            }
            Key::Char('a') => {
                text_object_around = Some(true);
                self.operator = OperatorState::Yank {
                    count,
                    register,
                    prefix,
                    motion_count,
                    text_object_around,
                };
                Command::None
            }
            Key::Char('g') => {
                prefix = Prefix::G;
                self.operator = OperatorState::Yank {
                    count,
                    register,
                    prefix,
                    motion_count,
                    text_object_around,
                };
                Command::None
            }
            Key::Char('[') => {
                prefix = Prefix::Bracket { forward: false };
                self.operator = OperatorState::Yank {
                    count,
                    register,
                    prefix,
                    motion_count,
                    text_object_around,
                };
                Command::None
            }
            Key::Char(']') => {
                prefix = Prefix::Bracket { forward: true };
                self.operator = OperatorState::Yank {
                    count,
                    register,
                    prefix,
                    motion_count,
                    text_object_around,
                };
                Command::None
            }
            Key::Char('f') | Key::Char('F') | Key::Char('t') | Key::Char('T') => {
                let (direction, till) = match key {
                    Key::Char('f') => (FindDirection::Forward, false),
                    Key::Char('F') => (FindDirection::Backward, false),
                    Key::Char('t') => (FindDirection::Forward, true),
                    _ => (FindDirection::Backward, true),
                };
                prefix = Prefix::Find { direction, till };
                self.operator = OperatorState::Yank {
                    count,
                    register,
                    prefix,
                    motion_count,
                    text_object_around,
                };
                Command::None
            }
            key => {
                let Some(motion) = motion_for_key(key) else {
                    self.reset_pending();
                    return Command::Bell;
                };
                let count = multiply_counts(count, motion_count.unwrap_or(1));
                self.finish(Command::YankMotion(motion, count, register))
            }
        }
    }

    fn feed_register(&mut self, key: Key) -> Command {
        self.awaiting_register = false;
        self.register = match key {
            Key::Char('"') => Some(ClipboardRegister::Internal),
            Key::Char('+') => Some(ClipboardRegister::System),
            _ => {
                self.reset_pending();
                return Command::Bell;
            }
        };
        Command::None
    }

    fn start_find(&mut self, direction: FindDirection, till: bool) -> Command {
        self.prefix = Prefix::Find { direction, till };
        Command::None
    }

    fn page(&mut self, forward: bool) -> Command {
        let count = self.take_count();
        self.finish(Command::ScrollPage { forward, count })
    }

    fn add_count(&mut self, digit: char) {
        let digit = digit.to_digit(10).unwrap_or(0) as usize;
        self.count = Some(
            self.count
                .unwrap_or(0)
                .saturating_mul(10)
                .saturating_add(digit)
                .min(MAX_COUNT),
        );
    }

    fn take_count(&mut self) -> usize {
        self.count.take().unwrap_or(1).max(1)
    }

    fn finish(&mut self, command: Command) -> Command {
        self.count = None;
        self.prefix = Prefix::None;
        self.operator = OperatorState::None;
        self.register = None;
        self.awaiting_register = false;
        command
    }

    fn reset_pending(&mut self) {
        self.count = None;
        self.prefix = Prefix::None;
        self.operator = OperatorState::None;
        self.register = None;
        self.awaiting_register = false;
    }
}

fn multiply_counts(left: usize, right: usize) -> usize {
    left.saturating_mul(right).min(MAX_COUNT)
}

fn motion_for_key(key: Key) -> Option<Motion> {
    match key {
        Key::Char('h') | Key::Left => Some(Motion::Left),
        Key::Char('j') | Key::Down => Some(Motion::Down),
        Key::Char('k') | Key::Up => Some(Motion::Up),
        Key::Char('l') | Key::Right => Some(Motion::Right),
        Key::Char('0') => Some(Motion::LineStart),
        Key::Char('^') => Some(Motion::FirstNonblank),
        Key::Char('$') => Some(Motion::LineEnd),
        Key::Char('w') => Some(Motion::Word(WordMove::ForwardStart, WordStyle::Word)),
        Key::Char('W') => Some(Motion::Word(WordMove::ForwardStart, WordStyle::BigWord)),
        Key::Char('b') => Some(Motion::Word(WordMove::BackwardStart, WordStyle::Word)),
        Key::Char('B') => Some(Motion::Word(WordMove::BackwardStart, WordStyle::BigWord)),
        Key::Char('e') => Some(Motion::Word(WordMove::ForwardEnd, WordStyle::Word)),
        Key::Char('E') => Some(Motion::Word(WordMove::ForwardEnd, WordStyle::BigWord)),
        Key::Char('G') => Some(Motion::DocumentEnd),
        Key::Char('%') => Some(Motion::MatchingBrace),
        Key::Char(';') => Some(Motion::RepeatFind { reverse: false }),
        Key::Char(',') => Some(Motion::RepeatFind { reverse: true }),
        _ => None,
    }
}

/// Review grammar; runtime bindings are listed separately and take precedence.
pub(crate) const HELP_BINDINGS: &[(&str, &str)] = &[
    ("q", "close Review and return to the underlying view"),
    (
        "Escape",
        "cancel pending input, search or selection; otherwise announce review mode",
    ),
    ("[count]", "prefix a motion or yank with a decimal count"),
    ("h / Left", "move to the previous character"),
    ("j / Down", "move to the next line"),
    ("k / Up", "move to the previous line"),
    ("l / Right", "move to the next character"),
    ("w / W", "next word / whitespace-delimited word"),
    ("b / B", "previous word / whitespace-delimited word"),
    ("e / E", "end of word / whitespace-delimited word"),
    (
        "0 / ^ / $",
        "line start / first nonblank character / line end",
    ),
    (
        "gg / G",
        "first / last document line; a count selects a line",
    ),
    ("C-b / C-u", "previous page"),
    ("C-f / C-d", "next page"),
    (
        "zt / zz / zb",
        "place current line at viewport top / middle / bottom",
    ),
    (
        "zEnter / z. / z-",
        "place line at top / middle / bottom and move to first nonblank",
    ),
    ("[p / ]p", "previous / next semantic shell prompt"),
    (
        "f{char} / F{char}",
        "find character forward / backward on the logical line",
    ),
    (
        "t{char} / T{char}",
        "move up to character forward / backward on the logical line",
    ),
    (
        "; / ,",
        "repeat character find in its original / opposite direction",
    ),
    ("%", "jump to matching parenthesis, bracket or brace"),
    ("m{a-z}", "set a named mark in this source document"),
    (
        "'{a-z}",
        "jump to marked line at its first nonblank character",
    ),
    ("`{a-z}", "jump to the exact marked position"),
    (
        "/pattern / ?pattern",
        "search forward / backward with a regular expression",
    ),
    (
        "n / N",
        "repeat the last search in its original / opposite direction",
    ),
    (
        "Search: Enter",
        "accept search; empty input reuses the previous pattern",
    ),
    (
        "Search: Backspace / C-u",
        "delete a character / clear the search input",
    ),
    (
        "y{motion} / yy",
        "copy a motion range / whole lines to the default register",
    ),
    (
        "yiw / yaw / yiW / yaW",
        "copy inner / outer word or whitespace-delimited word",
    ),
    (
        "v / V",
        "start or toggle characterwise / linewise selection",
    ),
    (
        "Visual: y",
        "copy the selection to the selected or default register",
    ),
    (
        "\"\"y / \"+y",
        "prefix a yank with Lector's internal / system clipboard register",
    ),
    ("gx", "open the HTTP or HTTPS link under the Review cursor"),
    (
        "gy / \"\"gy",
        "copy the link destination to Lector's internal clipboard",
    ),
    ("\"+gy", "copy the link destination to the system clipboard"),
    ("gt / gT", "detect a table / start manual table setup"),
    ("gB / gR", "mark the table bottom row / right edge"),
    ("gH", "toggle the current table column as row header"),
    ("[| / ]|", "previous / next table cell"),
    ("{| / }|", "table cell above / below"),
    ("Table setup: Space", "toggle a column start at the cursor"),
    ("Table setup: H", "cycle header mode"),
    ("Table setup: c", "edit the current column name"),
    (
        "Column name: Enter / Escape / C-u",
        "save / cancel / clear the name",
    ),
    ("Table setup: Enter / Escape", "accept / cancel table setup"),
];

#[cfg(test)]
mod tests {
    use super::{
        Command, FindDirection, Key, Motion, Parser, TextObject, ViewportPlacement, VisualKind,
    };
    use crate::clipboard::ClipboardRegister;
    use crate::review::document::{SearchDirection, WordMove, WordStyle};
    use crate::review::table::CellMove;

    fn feed(parser: &mut Parser, keys: &[Key]) -> Vec<Command> {
        keys.iter().copied().map(|key| parser.feed(key)).collect()
    }

    #[test]
    fn link_commands_use_explicit_registers_and_reset_after_use() {
        let mut parser = Parser::default();
        for (keys, expected) in [
            ("gx", Command::OpenLink),
            ("gy", Command::CopyLink(ClipboardRegister::Internal)),
            ("\"+gy", Command::CopyLink(ClipboardRegister::System)),
            ("gy", Command::CopyLink(ClipboardRegister::Internal)),
            ("\"\"gy", Command::CopyLink(ClipboardRegister::Internal)),
        ] {
            let commands: Vec<_> = keys.chars().map(|ch| parser.feed(Key::Char(ch))).collect();
            assert_eq!(commands.last(), Some(&expected), "{keys}");
        }
        assert_eq!(parser.feed(Key::Char('y')), Command::None);
        assert_eq!(parser.feed(Key::Char('y')), Command::YankLine(1, None));
    }

    #[test]
    fn parses_counts_and_word_motions() {
        let mut parser = Parser::default();
        assert_eq!(
            feed(&mut parser, &[Key::Char('3'), Key::Char('w')]),
            vec![
                Command::None,
                Command::Move(Motion::Word(WordMove::ForwardStart, WordStyle::Word), 3)
            ]
        );
        assert_eq!(
            feed(
                &mut parser,
                &[Key::Char('1'), Key::Char('2'), Key::Char('j')]
            ),
            vec![
                Command::None,
                Command::None,
                Command::Move(Motion::Down, 12)
            ]
        );
        assert_eq!(
            parser.feed(Key::Char('0')),
            Command::Move(Motion::LineStart, 1)
        );
    }

    #[test]
    fn parses_find_prompt_brace_and_page_commands() {
        let mut parser = Parser::default();
        assert_eq!(parser.feed(Key::Char('f')), Command::None);
        assert_eq!(
            parser.feed(Key::Char('z')),
            Command::Move(
                Motion::Find {
                    direction: FindDirection::Forward,
                    till: false,
                    target: 'z'
                },
                1
            )
        );
        assert_eq!(
            feed(&mut parser, &[Key::Char('['), Key::Char('p')]),
            vec![
                Command::None,
                Command::Move(Motion::Prompt { forward: false }, 1)
            ]
        );
        assert_eq!(
            parser.feed(Key::Char('%')),
            Command::Move(Motion::MatchingBrace, 1)
        );
        assert_eq!(
            parser.feed(Key::Ctrl('f')),
            Command::ScrollPage {
                forward: true,
                count: 1
            }
        );
    }

    #[test]
    fn parses_review_table_commands_and_cell_motions() {
        let mut parser = Parser::default();
        assert_eq!(
            feed(&mut parser, &[Key::Char('g'), Key::Char('t')]),
            vec![Command::None, Command::DetectTable]
        );
        assert_eq!(
            feed(&mut parser, &[Key::Char('g'), Key::Char('T')]),
            vec![Command::None, Command::StartTableSetup]
        );
        assert_eq!(
            feed(
                &mut parser,
                &[Key::Char('3'), Key::Char(']'), Key::Char('|')]
            ),
            vec![
                Command::None,
                Command::None,
                Command::MoveTableCell(CellMove::Next, 3)
            ]
        );
        assert_eq!(
            feed(&mut parser, &[Key::Char('{'), Key::Char('|')]),
            vec![Command::None, Command::MoveTableCell(CellMove::Up, 1)]
        );
        assert_eq!(
            feed(&mut parser, &[Key::Char('g'), Key::Char('B')]),
            vec![Command::None, Command::MarkTableBottom]
        );
        assert_eq!(
            feed(&mut parser, &[Key::Char('g'), Key::Char('R')]),
            vec![Command::None, Command::MarkTableRight]
        );
        assert_eq!(
            feed(&mut parser, &[Key::Char('g'), Key::Char('H')]),
            vec![Command::None, Command::ToggleTableRowHeader]
        );
    }

    #[test]
    fn escape_cancels_state_but_announces_mode_when_idle() {
        let mut parser = Parser::default();
        assert_eq!(parser.feed(Key::Escape), Command::AnnounceMode);
        parser.feed(Key::Char('3'));
        assert_eq!(parser.feed(Key::Escape), Command::None);
        parser.feed(Key::Char('f'));
        assert_eq!(parser.feed(Key::Escape), Command::None);
        assert_eq!(parser.feed(Key::Escape), Command::AnnounceMode);
    }

    #[test]
    fn escape_reminds_only_after_pending_state_has_been_cancelled() {
        for prefix in ["2", "g", "z", "f", "y", "yi", "\"", "\"+", "[", "]"] {
            let mut parser = Parser::default();
            for character in prefix.chars() {
                assert_eq!(parser.feed(Key::Char(character)), Command::None);
            }
            assert_eq!(parser.feed(Key::Escape), Command::None, "{prefix}");
            assert_eq!(parser.feed(Key::Escape), Command::AnnounceMode, "{prefix}");
        }
    }

    #[test]
    fn parses_yank_operators_text_objects_and_multiplied_counts() {
        let mut parser = Parser::default();
        assert_eq!(
            feed(
                &mut parser,
                &[
                    Key::Char('3'),
                    Key::Char('y'),
                    Key::Char('2'),
                    Key::Char('w')
                ]
            ),
            vec![
                Command::None,
                Command::None,
                Command::None,
                Command::YankMotion(
                    Motion::Word(WordMove::ForwardStart, WordStyle::Word),
                    6,
                    None
                )
            ]
        );
        assert_eq!(
            feed(
                &mut parser,
                &[Key::Char('y'), Key::Char('i'), Key::Char('w')]
            ),
            vec![
                Command::None,
                Command::None,
                Command::YankTextObject(
                    TextObject::Word {
                        style: WordStyle::Word,
                        around: false
                    },
                    1,
                    None
                )
            ]
        );
        assert_eq!(
            feed(&mut parser, &[Key::Char('y'), Key::Char('y')]),
            vec![Command::None, Command::YankLine(1, None)]
        );
    }

    #[test]
    fn visual_mode_moves_yanks_and_escape_cancels() {
        let mut parser = Parser::default();
        assert_eq!(
            parser.feed(Key::Char('v')),
            Command::StartVisual(VisualKind::Character)
        );
        assert_eq!(
            parser.feed(Key::Char('w')),
            Command::MoveVisual(Motion::Word(WordMove::ForwardStart, WordStyle::Word), 1)
        );
        assert_eq!(parser.feed(Key::Char('y')), Command::YankVisual(None));
        assert_eq!(
            parser.feed(Key::Char('V')),
            Command::StartVisual(VisualKind::Line)
        );
        assert_eq!(parser.feed(Key::Escape), Command::CancelVisual);
    }

    #[test]
    fn parses_internal_and_system_register_prefixes() {
        let mut parser = Parser::default();
        assert_eq!(
            feed(
                &mut parser,
                &[
                    Key::Char('"'),
                    Key::Char('+'),
                    Key::Char('y'),
                    Key::Char('i'),
                    Key::Char('w')
                ]
            ),
            vec![
                Command::None,
                Command::None,
                Command::None,
                Command::None,
                Command::YankTextObject(
                    TextObject::Word {
                        style: WordStyle::Word,
                        around: false,
                    },
                    1,
                    Some(ClipboardRegister::System)
                )
            ]
        );

        assert_eq!(
            parser.feed(Key::Char('v')),
            Command::StartVisual(VisualKind::Character)
        );
        assert_eq!(parser.feed(Key::Char('"')), Command::None);
        assert_eq!(parser.feed(Key::Char('"')), Command::None);
        assert_eq!(
            parser.feed(Key::Char('y')),
            Command::YankVisual(Some(ClipboardRegister::Internal))
        );
    }

    #[test]
    fn parses_search_and_repetition() {
        let mut parser = Parser::default();
        assert_eq!(
            parser.feed(Key::Char('/')),
            Command::StartSearch(SearchDirection::Forward)
        );
        assert_eq!(
            parser.feed(Key::Char('?')),
            Command::StartSearch(SearchDirection::Backward)
        );
        parser.feed(Key::Char('3'));
        assert_eq!(
            parser.feed(Key::Char('N')),
            Command::RepeatSearch {
                reverse: true,
                count: 3
            }
        );
    }

    #[test]
    fn parses_cursor_relative_viewport_commands_and_counts() {
        let mut parser = Parser::default();
        assert_eq!(
            feed(&mut parser, &[Key::Char('z'), Key::Char('t')]),
            vec![
                Command::None,
                Command::RepositionViewport {
                    placement: ViewportPlacement::Top,
                    line: None,
                    first_nonblank: false,
                }
            ]
        );
        assert_eq!(
            feed(&mut parser, &[Key::Char('z'), Key::Enter]),
            vec![
                Command::None,
                Command::RepositionViewport {
                    placement: ViewportPlacement::Top,
                    line: None,
                    first_nonblank: true,
                }
            ]
        );
        assert_eq!(
            feed(
                &mut parser,
                &[Key::Char('2'), Key::Char('z'), Key::Char('z')]
            ),
            vec![
                Command::None,
                Command::None,
                Command::RepositionViewport {
                    placement: ViewportPlacement::Center,
                    line: Some(2),
                    first_nonblank: false,
                }
            ]
        );
        assert_eq!(
            feed(&mut parser, &[Key::Char('z'), Key::Char('.')]),
            vec![
                Command::None,
                Command::RepositionViewport {
                    placement: ViewportPlacement::Center,
                    line: None,
                    first_nonblank: true,
                }
            ]
        );
        assert_eq!(
            feed(&mut parser, &[Key::Char('z'), Key::Char('b')]),
            vec![
                Command::None,
                Command::RepositionViewport {
                    placement: ViewportPlacement::Bottom,
                    line: None,
                    first_nonblank: false,
                }
            ]
        );
        assert_eq!(
            feed(&mut parser, &[Key::Char('z'), Key::Char('-')]),
            vec![
                Command::None,
                Command::RepositionViewport {
                    placement: ViewportPlacement::Bottom,
                    line: None,
                    first_nonblank: true,
                }
            ]
        );
    }

    #[test]
    fn invalid_chords_bell_and_reset() {
        let mut parser = Parser::default();
        assert_eq!(
            feed(&mut parser, &[Key::Char('g'), Key::Char('Q')]),
            vec![Command::None, Command::Bell]
        );
        assert_eq!(
            feed(
                &mut parser,
                &[Key::Char('y'), Key::Char('i'), Key::Char('q')]
            ),
            vec![Command::None, Command::None, Command::Bell]
        );
        assert_eq!(parser.feed(Key::Char('j')), Command::Move(Motion::Down, 1));
    }
}
