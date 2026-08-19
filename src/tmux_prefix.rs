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
    ChooseSession,
    ChooseWindow,
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
    if command.starts_with("choose-tree ") {
        if command
            .split_ascii_whitespace()
            .skip(1)
            .any(|argument| argument.contains('s'))
        {
            return Ok(BindingAction::ChooseSession);
        }
        if command
            .split_ascii_whitespace()
            .skip(1)
            .any(|argument| argument.contains('w'))
        {
            return Ok(BindingAction::ChooseWindow);
        }
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
    let Ok(index) = target.strip_prefix(":=").unwrap_or(target).parse() else {
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
