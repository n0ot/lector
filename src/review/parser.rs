use super::document::{SearchDirection, WordMove, WordStyle};

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
    Exit,
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
    YankMotion(Motion, usize),
    YankLine(usize),
    YankTextObject(TextObject, usize),
    StartVisual(VisualKind),
    CancelVisual,
    MoveVisual(Motion, usize),
    YankVisual,
    StartSearch(SearchDirection),
    RepeatSearch {
        reverse: bool,
        count: usize,
    },
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
    Bracket {
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
}

impl Default for Parser {
    fn default() -> Self {
        Self {
            count: None,
            prefix: Prefix::None,
            operator: OperatorState::None,
            visual: None,
        }
    }
}

impl Parser {
    pub(crate) fn feed(&mut self, key: Key) -> Command {
        if key == Key::Escape {
            return self.escape();
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
        {
            self.reset_pending();
            return Command::None;
        }
        Command::Bell
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
            Key::Char('y') => {
                let count = self.take_count();
                self.operator = OperatorState::Yank {
                    count,
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
                self.finish(Command::YankVisual)
            }
            Key::Char('v') | Key::Char('V') => {
                self.visual = None;
                self.finish(Command::CancelVisual)
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
        let motion = match (prefix, key) {
            (Prefix::G, Key::Char('g')) => Some(Motion::DocumentStart),
            (Prefix::Bracket { forward }, Key::Char('p')) => Some(Motion::Prompt { forward }),
            (Prefix::Find { direction, till }, Key::Char(target)) => Some(Motion::Find {
                direction,
                till,
                target,
            }),
            (Prefix::Z, _) => None,
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
                _ => None,
            };
            let Some(motion) = motion else {
                self.reset_pending();
                return Command::Bell;
            };
            let count = multiply_counts(count, motion_count.unwrap_or(1));
            return self.finish(Command::YankMotion(motion, count));
        }

        match key {
            Key::Char('y') => {
                let count = multiply_counts(count, motion_count.unwrap_or(1));
                self.finish(Command::YankLine(count))
            }
            Key::Char('i') => {
                text_object_around = Some(false);
                self.operator = OperatorState::Yank {
                    count,
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
                self.finish(Command::YankMotion(motion, count))
            }
        }
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
        command
    }

    fn reset_pending(&mut self) {
        self.count = None;
        self.prefix = Prefix::None;
        self.operator = OperatorState::None;
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

#[cfg(test)]
mod tests {
    use super::{
        Command, FindDirection, Key, Motion, Parser, TextObject, ViewportPlacement, VisualKind,
    };
    use crate::review::document::{SearchDirection, WordMove, WordStyle};

    fn feed(parser: &mut Parser, keys: &[Key]) -> Vec<Command> {
        keys.iter().copied().map(|key| parser.feed(key)).collect()
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
    fn escape_cancels_state_but_bells_when_idle() {
        let mut parser = Parser::default();
        assert_eq!(parser.feed(Key::Escape), Command::Bell);
        parser.feed(Key::Char('3'));
        assert_eq!(parser.feed(Key::Escape), Command::None);
        parser.feed(Key::Char('f'));
        assert_eq!(parser.feed(Key::Escape), Command::None);
        assert_eq!(parser.feed(Key::Escape), Command::Bell);
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
                Command::YankMotion(Motion::Word(WordMove::ForwardStart, WordStyle::Word), 6)
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
                    1
                )
            ]
        );
        assert_eq!(
            feed(&mut parser, &[Key::Char('y'), Key::Char('y')]),
            vec![Command::None, Command::YankLine(1)]
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
        assert_eq!(parser.feed(Key::Char('y')), Command::YankVisual);
        assert_eq!(
            parser.feed(Key::Char('V')),
            Command::StartVisual(VisualKind::Line)
        );
        assert_eq!(parser.feed(Key::Escape), Command::CancelVisual);
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
            feed(&mut parser, &[Key::Char('g'), Key::Char('x')]),
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
