//! tmux prefix key naming and safe binding classification.

use terminput::{KeyCode, KeyEvent, KeyModifiers};
use thiserror::Error;

pub const PREFIX_TIMEOUT_MS: u128 = 1_000;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BindingAction {
    Execute(String),
    Detach,
    Confirm { prompt: String, command: String },
    SendPrefix,
    ChooseSession,
    ChooseWindow,
    ChoosePane,
    CommandPrompt,
    UnsupportedKeyTable(String),
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
    if let Some(table) = configured_key_table(command)
        && table != "prefix"
    {
        return Ok(BindingAction::UnsupportedKeyTable(table.to_owned()));
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

fn configured_key_table(command: &str) -> Option<&str> {
    let marker = "key-table ";
    let rest = command.split_once(marker)?.1;
    rest.split(|character: char| character.is_ascii_whitespace() || character == '\\')
        .find(|field| !field.is_empty())
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
