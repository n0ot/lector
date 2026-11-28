//! tmux prefix key naming and safe binding classification.

use crate::tmux_model::TmuxTopology;
use terminput::{KeyCode, KeyEvent, KeyModifiers};
use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BindingAction {
    Execute(String),
    OpenReview {
        page_up: bool,
    },
    Detach,
    Confirm {
        prompt: String,
        command: String,
    },
    SendPrefix,
    ChooseTree(ChooseTree),
    UnsupportedChooseTree(String),
    ChoosePane,
    CommandPrompt,
    SetKeyTable {
        command: String,
        table: String,
        persistent: bool,
    },
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum PrefixError {
    #[error("tmux binding command is empty or contains unsafe control bytes")]
    UnsafeCommand,
}

pub fn classify_binding(command: &str) -> Result<BindingAction, PrefixError> {
    let command = command.trim();
    if command.is_empty()
        || command
            .bytes()
            .any(|byte| matches!(byte, b'\0' | b'\r' | b'\n'))
    {
        return Err(PrefixError::UnsafeCommand);
    }
    if command == "send-prefix" {
        return Ok(BindingAction::SendPrefix);
    }
    if command == "detach-client" {
        return Ok(BindingAction::Detach);
    }
    if command == "copy-mode" {
        return Ok(BindingAction::OpenReview { page_up: false });
    }
    if command == "copy-mode -u" {
        return Ok(BindingAction::OpenReview { page_up: true });
    }
    if let Some(arguments) = command
        .strip_prefix("choose-tree")
        .filter(|rest| rest.is_empty() || rest.starts_with([' ', '\t', ';']))
    {
        return Ok(match ChooseTree::parse(arguments) {
            Ok(options) => BindingAction::ChooseTree(options),
            Err(reason) => BindingAction::UnsupportedChooseTree(reason),
        });
    }
    if command == "command-prompt" || command.starts_with("command-prompt ") {
        return Ok(BindingAction::CommandPrompt);
    }
    if command == "display-panes" || command.starts_with("display-panes ") {
        return Ok(BindingAction::ChoosePane);
    }
    if command.starts_with("confirm-before ") {
        for candidate in ["kill-pane", "kill-window"] {
            if command.ends_with(candidate) {
                return Ok(BindingAction::Confirm {
                    prompt: command.to_owned(),
                    command: candidate.to_owned(),
                });
            }
        }
    }
    if let Some((table, persistent)) = configured_key_table(command) {
        return Ok(BindingAction::SetKeyTable {
            command: command.to_owned(),
            table,
            persistent,
        });
    }
    Ok(BindingAction::Execute(command.to_owned()))
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ChooseTreeLevel {
    #[default]
    Panes,
    Windows,
    Sessions,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ChooseTree {
    pub level: ChooseTreeLevel,
    // Keep the original tmux spelling (including quotes) so the server
    // interprets escapes in filter and format operands exactly once.
    pub(crate) filter: Option<String>,
    pub(crate) format: Option<String>,
}

impl ChooseTree {
    fn parse(mut arguments: &str) -> Result<Self, String> {
        let mut options = Self::default();
        while let Some(word) = choose_tree_word(&mut arguments)? {
            if word == "--" && arguments.trim().is_empty() {
                break;
            }
            let Some(flags) = word
                .strip_prefix('-')
                .filter(|flags| !flags.is_empty() && !flags.starts_with('-'))
            else {
                return Err(
                    "choose-tree command templates are unavailable in the accessible picker".into(),
                );
            };
            for (index, flag) in flags.char_indices() {
                match flag {
                    's' => options.level = ChooseTreeLevel::Sessions,
                    'w' if options.level != ChooseTreeLevel::Sessions => {
                        options.level = ChooseTreeLevel::Windows;
                    }
                    // The Lector overlay already covers the terminal, so it
                    // needs no temporary zoom of the underlying tmux pane.
                    'w' | 'Z' => {}
                    'f' | 'F' => {
                        let attached = &flags[index + 1..];
                        let operand = if attached.is_empty() {
                            choose_tree_word(&mut arguments)?
                                .ok_or_else(|| format!("choose-tree -{flag} requires a value"))?
                        } else {
                            attached
                        };
                        let destination = if flag == 'f' {
                            &mut options.filter
                        } else {
                            &mut options.format
                        };
                        *destination = Some(operand.to_owned());
                        break;
                    }
                    _ => {
                        return Err(format!(
                            "choose-tree option -{flag} is unavailable in the accessible picker"
                        ));
                    }
                }
            }
        }
        for (flag, operand) in [('f', &options.filter), ('F', &options.format)] {
            if let Some(operand) = operand
                && let Err(reason) = check_choose_tree_format(operand)
            {
                return Err(format!("choose-tree -{flag} {reason}"));
            }
        }
        Ok(options)
    }
}

/// Read one word in the command syntax printed by tmux list-keys. This only
/// locates the operand boundaries. tmux still interprets the operand itself.
fn choose_tree_word<'a>(arguments: &mut &'a str) -> Result<Option<&'a str>, String> {
    let input = arguments.trim_start_matches([' ', '\t']);
    if input.is_empty() {
        return Ok(None);
    }
    let mut quote = None;
    let mut chars = input.char_indices();
    while let Some((index, character)) = chars.next() {
        match character {
            ' ' | '\t' if quote.is_none() => {
                *arguments = &input[index..];
                return Ok(Some(&input[..index]));
            }
            '\\' if quote != Some('\'') => {
                if chars.next().is_none() {
                    return Err("incomplete escape in choose-tree binding".into());
                }
            }
            '\'' | '"' if quote == Some(character) => quote = None,
            '\'' | '"' if quote.is_none() => quote = Some(character),
            ';' | '{' | '}' if quote.is_none() => {
                return Err("choose-tree command sequences and templates are unavailable in the accessible picker".into());
            }
            '#' if index == 0 => {
                return Err("expected a choose-tree value before the comment".into());
            }
            _ => {}
        }
    }
    if quote.is_some() {
        return Err("unterminated quote in choose-tree binding".into());
    }
    *arguments = "";
    Ok(Some(input))
}

/// The list commands expose context missing from the tree, including their
/// row ordinal and sometimes the command's client. Forward only operands
/// whose format text can be checked for dependencies on that extra context.
fn check_choose_tree_format(operand: &str) -> Result<(), String> {
    let mut source = String::new();
    let mut quote = None;
    let mut segment_start = true;
    let mut chars = operand.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '\'' | '"' if quote.is_none() || quote == Some(character) => {
                quote = quote.is_none().then_some(character);
                segment_start = true;
                continue;
            }
            '\\' if quote != Some('\'') => match chars.next() {
                // Tmux treats these C-style escapes as non-word characters.
                Some('a' | 'b' | 'e' | 'f' | 'n' | 'r' | 's' | 't' | 'v') => source.push(' '),
                Some('0'..='7' | 'u' | 'U') | None => {
                    return Err("uses an encoded escape the accessible picker cannot check".into());
                }
                Some(escaped) => source.push(escaped),
            },
            '$' if quote != Some('\'')
                && chars.peek().is_some_and(|character| {
                    character.is_ascii_alphabetic() || matches!(character, '_' | '{')
                }) =>
            {
                return Err("uses a dynamic argument the accessible picker cannot check".into());
            }
            '~' if quote != Some('\'') && segment_start => {
                return Err("uses a dynamic argument the accessible picker cannot check".into());
            }
            _ => source.push(character),
        }
        segment_start = false;
    }

    // Check the expression headers, including nested expressions inside
    // comparisons and branches. tmux still expands the original operand.
    let mut format = source.as_str();
    while let Some((_, after_hash)) = format.split_once('#') {
        let Some(marker) = after_hash.chars().next() else {
            break;
        };
        format = &after_hash[marker.len_utf8()..];
        match marker {
            '{' => check_choose_tree_expression(format)?,
            '(' => return Err("uses a shell format unavailable in the accessible picker".into()),
            // These shortcuts expand to the same fields as the named forms.
            'D' | 'F' | 'H' | 'I' | 'P' | 'S' | 'T' | 'W' | 'h' => {}
            marker if marker.is_ascii_alphabetic() => {
                return Err(format!(
                    "uses shorthand #{marker} unavailable in the accessible picker"
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

fn check_choose_tree_expression(expression: &str) -> Result<(), String> {
    if let Some(condition) = expression.strip_prefix('?') {
        if condition.starts_with("#{") {
            return Ok(());
        }
        let field = condition.split([',', '}']).next().unwrap_or_default();
        return check_choose_tree_field(field);
    }
    if ["==:", "!=:", "<=:", ">=:", "&&:", "||:", "m:"]
        .iter()
        .any(|modifier| expression.starts_with(modifier))
    {
        return Ok(());
    }
    let (part, rest) = expression
        .find([':', '}'])
        .map_or((expression, ""), |end| expression.split_at(end));
    if let Some(field) = rest.strip_prefix(':') {
        let width = part
            .strip_prefix('=')
            .and_then(|width| width.parse::<i32>().ok());
        if width.is_some() {
            return check_choose_tree_field(field.split('}').next().unwrap_or_default());
        }
        return Err(format!(
            "uses the {part} modifier unavailable in the accessible picker"
        ));
    }
    check_choose_tree_field(part)
}

fn check_choose_tree_field(field: &str) -> Result<(), String> {
    // These values depend only on the server or explicit row/descendant
    // objects. Similar-looking names (such as session_active) can also
    // depend on the invoking client, which differs in list commands.
    if matches!(
        field,
        "host"
            | "host_short"
            | "session_attached"
            | "session_format"
            | "session_id"
            | "session_name"
            | "session_windows"
            | "window_active"
            | "window_flags"
            | "window_format"
            | "window_id"
            | "window_index"
            | "window_name"
            | "window_panes"
            | "window_zoomed_flag"
            | "pane_active"
            | "pane_current_command"
            | "pane_current_path"
            | "pane_dead"
            | "pane_format"
            | "pane_id"
            | "pane_index"
            | "pane_program_message"
            | "pane_program_name"
            | "pane_program_state"
            | "pane_title"
    ) {
        Ok(())
    } else {
        Err(format!(
            "uses {field}, which is unavailable in the accessible picker’s format context"
        ))
    }
}

/// Whether a successful discovered command can invalidate prefix discovery.
///
/// False positives only cost one inventory transaction, while a false negative
/// would leave Lector emulating stale key configuration.
#[must_use]
pub fn command_may_change_key_configuration(command: &str) -> bool {
    command
        .split_ascii_whitespace()
        .map(|word| word.trim_matches(|character| matches!(character, '\'' | '"' | '\\' | ';')))
        .any(|word| {
            matches!(
                word,
                "source-file"
                    | "bind"
                    | "bind-key"
                    | "unbind"
                    | "unbind-key"
                    | "set"
                    | "set-option"
            )
        })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SelectWindowScope {
    NotApplicable,
    Resolved(String),
    Missing(u32),
}

/// Resolve a simple numeric `select-window` binding against the session to
/// which this control client is attached. The stable window ID avoids tmux
/// interpreting an unqualified number in some other client/session context.
#[must_use]
pub fn scope_select_window_command(topology: &TmuxTopology, command: &str) -> SelectWindowScope {
    let mut words = command.split_ascii_whitespace();
    if words.next() != Some("select-window") || words.next() != Some("-t") {
        return SelectWindowScope::NotApplicable;
    }
    let Some(raw_target) = words.next() else {
        return SelectWindowScope::NotApplicable;
    };
    if words.next().is_some() {
        return SelectWindowScope::NotApplicable;
    }
    let target = raw_target.trim_matches(|character| matches!(character, '\'' | '"'));
    let Ok(index) = target.trim_prefix(":=").parse() else {
        return SelectWindowScope::NotApplicable;
    };
    let Some(session) = topology
        .attached_session()
        .and_then(|session_id| topology.session(session_id))
    else {
        return SelectWindowScope::NotApplicable;
    };
    match session.windows.get(&index) {
        Some(window_id) => {
            SelectWindowScope::Resolved(format!("select-window -t @{}", window_id.0))
        }
        None => SelectWindowScope::Missing(index),
    }
}

fn configured_key_table(command: &str) -> Option<(String, bool)> {
    let mut transition = None;
    for segment in command.split("\\;").flat_map(|chunk| chunk.split(';')) {
        let words = segment
            .split_ascii_whitespace()
            .map(|word| word.trim_matches(['\'', '"']))
            .filter(|word| !word.is_empty())
            .collect::<Vec<_>>();
        let Some(program) = words.first().copied() else {
            continue;
        };
        if program == "switch-client" {
            for (index, word) in words.iter().enumerate().skip(1) {
                let table = if *word == "-T" {
                    words.get(index + 1).copied()
                } else {
                    word.strip_prefix("-T").filter(|table| !table.is_empty())
                };
                if let Some(table) = table.and_then(valid_key_table) {
                    transition = Some((table.to_owned(), false));
                }
            }
        } else if matches!(program, "set" | "set-option") {
            for (index, word) in words.iter().enumerate().skip(1) {
                if *word == "key-table"
                    && let Some(table) = words
                        .get(index + 1)
                        .and_then(|table| valid_key_table(table))
                {
                    transition = Some((table.to_owned(), true));
                }
            }
        }
    }
    transition
}

fn valid_key_table(table: &str) -> Option<&str> {
    (!table.is_empty()
        && table.len() <= 128
        && !table
            .bytes()
            .any(|byte| byte.is_ascii_control() || matches!(byte, b'\\' | b';' | b'\'' | b'"')))
    .then_some(table)
}

#[must_use]
pub fn tmux_key_name(event: KeyEvent) -> Option<String> {
    let event = event.normalize_case();
    let mut name = String::new();
    let is_char = matches!(event.code, KeyCode::Char(_));
    if event.modifiers.contains(KeyModifiers::CTRL) {
        name.push_str("C-");
    }
    if event
        .modifiers
        .intersects(KeyModifiers::ALT | KeyModifiers::META)
    {
        name.push_str("M-");
    }
    if event.modifiers.contains(KeyModifiers::SUPER) {
        name.push_str("Super-");
    }
    if event.modifiers.contains(KeyModifiers::HYPER) {
        name.push_str("Hyper-");
    }
    if !is_char && event.modifiers.contains(KeyModifiers::SHIFT) {
        name.push_str("S-");
    }
    match event.code {
        KeyCode::Char(' ') => name.push_str("Space"),
        KeyCode::Char(character) => name.push(character),
        KeyCode::Backspace => name.push_str("BSpace"),
        KeyCode::Delete => name.push_str("DC"),
        KeyCode::Esc => name.push_str("Escape"),
        KeyCode::Enter => name.push_str("Enter"),
        KeyCode::Tab => name.push_str("Tab"),
        KeyCode::Up => name.push_str("Up"),
        KeyCode::Down => name.push_str("Down"),
        KeyCode::Left => name.push_str("Left"),
        KeyCode::Right => name.push_str("Right"),
        KeyCode::Home => name.push_str("Home"),
        KeyCode::End => name.push_str("End"),
        KeyCode::PageUp => name.push_str("PPage"),
        KeyCode::PageDown => name.push_str("NPage"),
        KeyCode::Insert => name.push_str("IC"),
        KeyCode::F(number) => name.push_str(&format!("F{number}")),
        _ => return None,
    }
    Some(name)
}

#[must_use]
pub fn tmux_key_bytes(name: &str) -> Option<Vec<u8>> {
    if let Some(rest) = name.strip_prefix("M-") {
        let mut bytes = vec![b'\x1b'];
        bytes.extend(tmux_key_bytes(rest)?);
        return Some(bytes);
    }
    if let Some(rest) = name.strip_prefix("C-") {
        let character = if rest == "Space" {
            ' '
        } else {
            let mut characters = rest.chars();
            let character = characters.next()?;
            if characters.next().is_some() {
                return None;
            }
            character
        };
        let character = character.to_ascii_lowercase();
        let byte = match character {
            '@' | ' ' | '2' => 0x00,
            'a'..='z' => (character as u8) - b'a' + 1,
            '[' | '3' => 0x1b,
            '\\' | '4' => 0x1c,
            ']' | '5' => 0x1d,
            '^' | '6' => 0x1e,
            '_' | '7' | '/' => 0x1f,
            '?' | '8' => 0x7f,
            _ => return None,
        };
        return Some(vec![byte]);
    }
    if name == "Space" {
        return Some(vec![b' ']);
    }
    let mut characters = name.chars();
    let character = characters.next()?;
    if characters.next().is_some() {
        return None;
    }
    let mut buffer = [0; 4];
    Some(character.encode_utf8(&mut buffer).as_bytes().to_vec())
}
