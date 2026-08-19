use super::{screen_reader::ScreenReader, view::View};

mod clipboard;
mod mouse;
mod review;
mod system;
mod table;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error(transparent)]
    ScreenReader(#[from] crate::screen_reader::Error),
    #[error(transparent)]
    Speech(#[from] crate::speech::Error),
    #[error("cannot get cell at row {row}, column {col}")]
    MissingCell { row: u16, col: u16 },
}

struct ActionMetadata {
    help: &'static str,
    name: &'static str,
}

macro_rules! define_actions {
    ($($variant:ident => ($help:literal, $name:literal)),+ $(,)?) => {
        #[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
        #[repr(usize)]
        pub enum Action {
            $($variant),+
        }

        const ACTION_METADATA: &[ActionMetadata] = &[
            $(ActionMetadata { help: $help, name: $name }),+
        ];

        #[cfg(test)]
        const ACTION_TABLE: &[(Action, &str, &str)] = &[
            $((Action::$variant, $help, $name)),+
        ];

        impl Action {
            fn metadata(self) -> &'static ActionMetadata {
                &ACTION_METADATA[self as usize]
            }

            pub fn help_text(&self) -> &'static str {
                self.metadata().help
            }

            /// Whether this command observes or navigates terminal contents.
            /// These commands use the last physically completed view; raw
            /// input and coordinate-dependent UI actions intentionally keep
            /// targeting the current logical view.
            pub(crate) fn uses_presented_view(self) -> bool {
                matches!(
                    self,
                    Action::SayOverlay
                        | Action::ToggleReviewCursorFollowsScreenCursor
                        | Action::OpenReview
                        | Action::RevLinePrev
                        | Action::RevLineNext
                        | Action::RevLinePrevNonBlank
                        | Action::RevLineNextNonBlank
                        | Action::RevLineRead
                        | Action::RevCharPrev
                        | Action::RevCharNext
                        | Action::RevCharRead
                        | Action::RevCharReadPhonetic
                        | Action::RevWordPrev
                        | Action::RevWordNext
                        | Action::RevWordRead
                        | Action::RevTop
                        | Action::RevBottom
                        | Action::RevFirst
                        | Action::RevLast
                        | Action::RevReadAttributes
                        | Action::SetMark
                        | Action::Copy
                        | Action::ToggleTableMode
                        | Action::StartTableSetupMode
                        | Action::CommitTableSetupMode
                        | Action::ToggleTableSetupTabstop
                        | Action::TableRowPrev
                        | Action::TableRowNext
                        | Action::TableRowTop
                        | Action::TableRowBottom
                        | Action::TableColPrev
                        | Action::TableColNext
                        | Action::TableColFirst
                        | Action::TableColLast
                        | Action::TableCellRead
                        | Action::TableHeaderRead
                        | Action::TableWordPrev
                        | Action::TableWordNext
                        | Action::TableWordRead
                        | Action::TableCharPrev
                        | Action::TableCharNext
                        | Action::TableCharRead
                )
            }
        }

        pub fn builtin_action_from_name(name: &str) -> Option<Action> {
            match name {
                $($name => Some(Action::$variant)),+,
                _ => None,
            }
        }
    };
}

define_actions! {
    ToggleHelp => ("toggle help", "toggle_help"),
    ToggleAutoRead => ("toggle auto read", "toggle_auto_read"),
    ToggleReviewCursorFollowsScreenCursor => (
        "toggle whether review cursor follows screen cursor",
        "toggle_review_cursor_follows_screen_cursor"
    ),
    ToggleSymbolLevel => ("toggle symbol level", "toggle_symbol_level"),
    SayOverlay => ("say current overlay", "say_overlay"),
    OpenLuaRepl => ("open Lua REPL", "open_lua_repl"),
    OpenReview => ("enter review mode", "open_review"),
    OpenTmuxConnectionChooser => (
        "open tmux connection chooser",
        "open_tmux_connection_chooser"
    ),
    RenameTmuxConnection => ("rename tmux connection", "rename_tmux_connection"),
    OpenTmuxSessionChooser => ("open tmux session chooser", "open_tmux_session_chooser"),
    OpenTmuxWindowChooser => ("open tmux window chooser", "open_tmux_window_chooser"),
    OpenTmuxPaneChooser => ("open tmux pane chooser", "open_tmux_pane_chooser"),
    OpenTmuxCommandPrompt => ("open tmux command prompt", "open_tmux_command_prompt"),
    DetachTmuxConnection => ("gracefully detach the active tmux connection", "detach_tmux_connection"),
    ForceAbandonTmuxGateway => ("expose a stuck active tmux gateway as raw terminal input", "force_abandon_tmux_gateway"),
    PassNextKey => ("forward next key press", "pass_next_key"),
    StopSpeaking => ("stop speaking", "stop_speaking"),
    RevLinePrev => ("previous line", "review_line_prev"),
    RevLineNext => ("next line", "review_line_next"),
    RevLinePrevNonBlank => ("previous non blank line", "review_line_prev_non_blank"),
    RevLineNextNonBlank => ("next non blank line", "review_line_next_non_blank"),
    RevLineRead => ("current line", "review_line_read"),
    RevCharPrev => ("previous character", "review_char_prev"),
    RevCharNext => ("next character", "review_char_next"),
    RevCharRead => ("current character", "review_char_read"),
    RevCharReadPhonetic => ("current character phonetically", "review_char_read_phonetic"),
    RevWordPrev => ("previous word", "review_word_prev"),
    RevWordNext => ("next word", "review_word_next"),
    RevWordRead => ("current word", "review_word_read"),
    RevTop => ("top", "review_top"),
    RevBottom => ("bottom", "review_bottom"),
    RevFirst => ("beginning of line", "review_first"),
    RevLast => ("end of line", "review_last"),
    RevReadAttributes => ("read attributes", "review_read_attributes"),
    LeftClick => ("left click at review cursor", "left_click"),
    RightClick => ("right click at review cursor", "right_click"),
    Backspace => ("backspace", "backspace"),
    Delete => ("delete", "delete"),
    SayTime => ("say the time", "say_time"),
    SetMark => ("set mark", "set_mark"),
    Copy => ("copy", "copy"),
    Paste => ("paste", "paste"),
    PasteInternal => ("paste internal clipboard", "paste_internal"),
    PasteSystem => ("paste system clipboard", "paste_system"),
    SayClipboard => ("say clipboard", "say_clipboard"),
    SayInternalClipboard => ("say internal clipboard", "say_internal_clipboard"),
    SaySystemClipboard => ("say system clipboard", "say_system_clipboard"),
    PreviousClipboard => ("previous clipboard", "previous_clipboard"),
    NextClipboard => ("next clipboard", "next_clipboard"),
    ToggleTableMode => ("toggle table mode", "toggle_table_mode"),
    ToggleStopSpeechOnFocusLoss => (
        "toggle stop speech on focus loss",
        "toggle_stop_speech_on_focus_loss"
    ),
    StartTableSetupMode => ("start table setup mode", "start_table_setup_mode"),
    CancelTableSetupMode => ("cancel table setup mode", "cancel_table_setup_mode"),
    CommitTableSetupMode => ("commit table setup mode", "commit_table_setup_mode"),
    ToggleTableSetupTabstop => (
        "toggle tabstop at review cursor",
        "toggle_table_setup_tabstop"
    ),
    ExitTableMode => ("exit table mode", "exit_table_mode"),
    TableRowPrev => ("previous table row", "table_row_prev"),
    TableRowNext => ("next table row", "table_row_next"),
    TableRowTop => ("top table row", "table_row_top"),
    TableRowBottom => ("bottom table row", "table_row_bottom"),
    TableColPrev => ("previous table column", "table_col_prev"),
    TableColNext => ("next table column", "table_col_next"),
    TableColFirst => ("first table column", "table_col_first"),
    TableColLast => ("last table column", "table_col_last"),
    TableCellRead => ("current table cell", "table_cell_read"),
    TableHeaderRead => ("current table header", "table_header_read"),
    ToggleTableHeaderRead => ("toggle table header reading", "toggle_table_header_read"),
    TableWordPrev => ("previous word in cell", "table_word_prev"),
    TableWordNext => ("next word in cell", "table_word_next"),
    TableWordRead => ("current word in cell", "table_word_read"),
    TableCharPrev => ("previous character in cell", "table_char_prev"),
    TableCharNext => ("next character in cell", "table_char_next"),
    TableCharRead => ("current character in cell", "table_char_read"),
}

pub enum CommandResult {
    Handled,
    ForwardInput,
    Paste(String),
    PtyInput(Vec<u8>),
}

pub fn builtin_action_name(action: Action) -> &'static str {
    action.metadata().name
}

pub fn handle(
    sr: &mut ScreenReader,
    title: &str,
    view: &mut View,
    action: Action,
) -> Result<CommandResult> {
    if let Action::ToggleHelp = action {
        return system::toggle_help(sr);
    }
    if sr.help_mode() {
        sr.speak(action.help_text(), false)?;
        return Ok(CommandResult::Handled);
    }

    match action {
        Action::ToggleAutoRead => system::toggle_auto_read(sr),
        Action::ToggleReviewCursorFollowsScreenCursor => {
            system::toggle_review_follows_screen_cursor(sr, view)
        }
        Action::ToggleSymbolLevel => system::toggle_symbol_level(sr),
        Action::SayOverlay => system::say_overlay(sr, title),
        Action::PassNextKey => system::pass_next_key(sr),
        Action::StopSpeaking => system::stop(sr),
        Action::RevLinePrev => review::line_previous(sr, view, false),
        Action::RevLineNext => review::line_next(sr, view, false),
        Action::RevLinePrevNonBlank => review::line_previous(sr, view, true),
        Action::RevLineNextNonBlank => review::line_next(sr, view, true),
        Action::RevLineRead => review::line_read(sr, view),
        Action::RevWordPrev => review::word_previous(sr, view),
        Action::RevWordNext => review::word_next(sr, view),
        Action::RevWordRead => review::word_read(sr, view),
        Action::RevCharPrev => review::character_previous(sr, view),
        Action::RevCharNext => review::character_next(sr, view),
        Action::RevCharRead => review::character_read(sr, view),
        Action::RevCharReadPhonetic => review::character_read_phonetic(sr, view),
        Action::RevTop => review::top(sr, view),
        Action::RevBottom => review::bottom(sr, view),
        Action::RevFirst => review::first(sr, view),
        Action::RevLast => review::last(sr, view),
        Action::RevReadAttributes => review::read_attributes(sr, view),
        Action::LeftClick => mouse::click(sr, view, mouse::Button::Left),
        Action::RightClick => mouse::click(sr, view, mouse::Button::Right),
        Action::Backspace => system::backspace(sr, view),
        Action::Delete => system::delete(sr, view),
        Action::SayTime => system::say_time(sr),
        Action::SetMark => clipboard::set_mark(sr, view),
        Action::Copy => clipboard::copy(sr, view),
        Action::Paste => clipboard::paste(sr),
        Action::PasteInternal => clipboard::paste_internal(sr),
        Action::PasteSystem => clipboard::paste_system(sr),
        Action::SayClipboard => clipboard::say(sr),
        Action::SayInternalClipboard => clipboard::say_internal(sr),
        Action::SaySystemClipboard => clipboard::say_system(sr),
        Action::PreviousClipboard => clipboard::previous(sr),
        Action::NextClipboard => clipboard::next(sr),
        Action::ToggleTableMode => table::toggle_mode(sr, view),
        Action::ToggleStopSpeechOnFocusLoss => system::toggle_stop_speech_on_focus_loss(sr),
        Action::StartTableSetupMode => table::start_setup(sr, view),
        Action::CancelTableSetupMode => table::cancel_setup(sr),
        Action::CommitTableSetupMode => table::commit_setup(sr, view),
        Action::ToggleTableSetupTabstop => table::toggle_setup_tabstop(sr, view),
        Action::ExitTableMode => table::exit_mode(sr),
        Action::TableRowPrev => table::row_move(sr, view, table::RowMove::Previous),
        Action::TableRowNext => table::row_move(sr, view, table::RowMove::Next),
        Action::TableRowTop => table::row_move(sr, view, table::RowMove::First),
        Action::TableRowBottom => table::row_move(sr, view, table::RowMove::Last),
        Action::TableColPrev => table::column_move(sr, view, table::ColumnMove::Previous),
        Action::TableColNext => table::column_move(sr, view, table::ColumnMove::Next),
        Action::TableColFirst => table::column_move(sr, view, table::ColumnMove::First),
        Action::TableColLast => table::column_move(sr, view, table::ColumnMove::Last),
        Action::TableCellRead => table::cell_read(sr, view),
        Action::TableHeaderRead => table::header_read(sr, view),
        Action::ToggleTableHeaderRead => table::toggle_header_read(sr),
        Action::TableWordPrev => table::word_previous(sr, view),
        Action::TableWordNext => table::word_next(sr, view),
        Action::TableWordRead => table::word_read(sr, view),
        Action::TableCharPrev => table::character_previous(sr, view),
        Action::TableCharNext => table::character_next(sr, view),
        Action::TableCharRead => table::character_read(sr, view),
        Action::ToggleHelp
        | Action::OpenLuaRepl
        | Action::OpenReview
        | Action::OpenTmuxConnectionChooser
        | Action::RenameTmuxConnection
        | Action::OpenTmuxSessionChooser
        | Action::OpenTmuxWindowChooser
        | Action::OpenTmuxPaneChooser
        | Action::OpenTmuxCommandPrompt
        | Action::DetachTmuxConnection
        | Action::ForceAbandonTmuxGateway => {
            sr.speak("not implemented", false)?;
            Ok(CommandResult::Handled)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ACTION_TABLE, Action, builtin_action_from_name, builtin_action_name};
    use std::collections::HashSet;

    #[test]
    fn action_metadata_is_complete_unique_and_reversible() {
        let mut actions = Vec::new();
        let mut names = HashSet::new();

        for (action, help, name) in ACTION_TABLE {
            assert!(!actions.contains(action), "duplicate action: {action:?}");
            actions.push(*action);
            assert!(names.insert(*name), "duplicate action name: {name}");
            assert!(!help.is_empty());
            assert!(!name.is_empty());
            assert_eq!(builtin_action_name(*action), *name);
            assert_eq!(builtin_action_from_name(name), Some(*action));
        }
    }

    #[test]
    fn tmux_gateway_actions_have_stable_configuration_names() {
        for (name, action) in [
            ("detach_tmux_connection", Action::DetachTmuxConnection),
            (
                "force_abandon_tmux_gateway",
                Action::ForceAbandonTmuxGateway,
            ),
        ] {
            assert_eq!(builtin_action_from_name(name), Some(action));
            assert_eq!(builtin_action_name(action), name);
        }
    }

    #[test]
    fn removed_direct_scrollback_actions_are_not_configurable() {
        for name in [
            "review_page_prev",
            "review_page_next",
            "review_prompt_prev",
            "review_prompt_next",
        ] {
            assert_eq!(builtin_action_from_name(name), None, "name={name}");
        }
    }
}
