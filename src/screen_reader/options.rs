use super::TmuxBellMode;
use crate::clipboard::{ClipboardRegister, SystemClipboardProvider};

#[derive(Clone)]
pub(super) struct Options {
    help_mode: bool,
    auto_read: bool,
    suppress_key_echo: bool,
    report_indentation: bool,
    review_follows_screen_cursor: bool,
    highlight_tracking: bool,
    table_header_auto: bool,
    stop_speech_on_focus_loss: bool,
    tmux_bell_mode: TmuxBellMode,
    clipboard_default_register: ClipboardRegister,
    system_clipboard_provider: SystemClipboardProvider,
    legacy_escape_timeout_ms: u16,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            help_mode: false,
            auto_read: true,
            suppress_key_echo: false,
            report_indentation: true,
            review_follows_screen_cursor: true,
            highlight_tracking: false,
            table_header_auto: true,
            stop_speech_on_focus_loss: true,
            tmux_bell_mode: TmuxBellMode::Audible,
            clipboard_default_register: ClipboardRegister::Internal,
            system_clipboard_provider: SystemClipboardProvider::Native,
            legacy_escape_timeout_ms: 30,
        }
    }
}

impl Options {
    pub(super) fn help_mode(&self) -> bool {
        self.help_mode
    }

    pub(super) fn set_help_mode(&mut self, value: bool) {
        self.help_mode = value;
    }

    pub(super) fn toggle_help_mode(&mut self) -> bool {
        self.help_mode = !self.help_mode;
        self.help_mode
    }

    pub(super) fn auto_read(&self) -> bool {
        self.auto_read
    }

    pub(super) fn set_auto_read(&mut self, value: bool) {
        self.auto_read = value;
    }

    pub(super) fn toggle_auto_read(&mut self) -> bool {
        self.auto_read = !self.auto_read;
        self.auto_read
    }

    pub(super) fn suppress_key_echo(&self) -> bool {
        self.suppress_key_echo
    }

    pub(super) fn set_suppress_key_echo(&mut self, value: bool) {
        self.suppress_key_echo = value;
    }

    pub(super) fn report_indentation(&self) -> bool {
        self.report_indentation
    }

    pub(super) fn set_report_indentation(&mut self, value: bool) {
        self.report_indentation = value;
    }

    pub(super) fn review_follows_screen_cursor(&self) -> bool {
        self.review_follows_screen_cursor
    }

    pub(super) fn set_review_follows_screen_cursor(&mut self, value: bool) {
        self.review_follows_screen_cursor = value;
    }

    pub(super) fn toggle_review_follows_screen_cursor(&mut self) -> bool {
        self.review_follows_screen_cursor = !self.review_follows_screen_cursor;
        self.review_follows_screen_cursor
    }

    pub(super) fn highlight_tracking(&self) -> bool {
        self.highlight_tracking
    }

    pub(super) fn set_highlight_tracking(&mut self, value: bool) {
        self.highlight_tracking = value;
    }

    pub(super) fn table_header_auto(&self) -> bool {
        self.table_header_auto
    }

    pub(super) fn toggle_table_header_auto(&mut self) -> bool {
        self.table_header_auto = !self.table_header_auto;
        self.table_header_auto
    }

    pub(super) fn stop_speech_on_focus_loss(&self) -> bool {
        self.stop_speech_on_focus_loss
    }

    pub(super) fn set_stop_speech_on_focus_loss(&mut self, value: bool) {
        self.stop_speech_on_focus_loss = value;
    }

    pub(super) fn toggle_stop_speech_on_focus_loss(&mut self) -> bool {
        self.stop_speech_on_focus_loss = !self.stop_speech_on_focus_loss;
        self.stop_speech_on_focus_loss
    }

    pub(super) fn tmux_bell_mode(&self) -> TmuxBellMode {
        self.tmux_bell_mode
    }

    pub(super) fn set_tmux_bell_mode(&mut self, value: TmuxBellMode) {
        self.tmux_bell_mode = value;
    }

    pub(super) fn clipboard_default_register(&self) -> ClipboardRegister {
        self.clipboard_default_register
    }

    pub(super) fn set_clipboard_default_register(&mut self, value: ClipboardRegister) {
        self.clipboard_default_register = value;
    }

    pub(super) fn system_clipboard_provider(&self) -> SystemClipboardProvider {
        self.system_clipboard_provider
    }

    pub(super) fn set_system_clipboard_provider(&mut self, value: SystemClipboardProvider) {
        self.system_clipboard_provider = value;
    }

    pub(super) fn legacy_escape_timeout_ms(&self) -> u16 {
        self.legacy_escape_timeout_ms
    }

    pub(super) fn set_legacy_escape_timeout_ms(&mut self, value: u16) {
        self.legacy_escape_timeout_ms = value;
    }
}

#[cfg(test)]
mod tests {
    use super::{Options, TmuxBellMode};
    use crate::clipboard::{ClipboardRegister, SystemClipboardProvider};

    #[test]
    fn defaults_match_the_user_facing_configuration() {
        let options = Options::default();
        assert!(!options.help_mode());
        assert!(options.auto_read());
        assert!(!options.suppress_key_echo());
        assert!(options.report_indentation());
        assert!(options.review_follows_screen_cursor());
        assert!(!options.highlight_tracking());
        assert!(options.table_header_auto());
        assert!(options.stop_speech_on_focus_loss());
        assert_eq!(options.tmux_bell_mode(), TmuxBellMode::Audible);
        assert_eq!(
            options.clipboard_default_register(),
            ClipboardRegister::Internal
        );
        assert_eq!(
            options.system_clipboard_provider(),
            SystemClipboardProvider::Native
        );
        assert_eq!(options.legacy_escape_timeout_ms(), 30);
    }

    #[test]
    fn toggles_return_the_new_value() {
        let mut options = Options::default();
        assert!(options.toggle_help_mode());
        assert!(!options.toggle_auto_read());
        assert!(!options.toggle_review_follows_screen_cursor());
        assert!(!options.toggle_table_header_auto());
        assert!(!options.toggle_stop_speech_on_focus_loss());
    }
}
