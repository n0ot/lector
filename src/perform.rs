use vte::{Params, Perform};

/// Processes text from VTE, storing new text to be printed.
pub struct TextReporter {
    /// Stores characters printed to the screen
    text: String,
    /// True if the next call to `[print]` should clear the text.
    reset: bool,
    pub csi_dispatches: usize,
}

impl TextReporter {
    pub fn new() -> Self {
        TextReporter {
            text: String::new(),
            reset: false,
            csi_dispatches: 0,
        }
    }

    /// returns a reference of the text seen so far.
    /// Future interactions with this TextReporter may overwrite this text.
    pub fn get_text(&mut self) -> &str {
        if self.reset {
            self.text.clear();
        }
        self.reset = true;
        self.csi_dispatches = 0;
        &self.text
    }
}

impl Perform for TextReporter {
    fn print(&mut self, c: char) {
        if self.reset {
            self.text.clear();
            self.reset = false;
        }
        self.text.push(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            10 | 13 => self.text.push('\n'),
            _ => {},
        }
    }

    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _c: char) {
        // Nothing to do
    }

    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {
        // Nothing to do
    }

    fn csi_dispatch(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _c: char) {
        // Nothing to do
        self.csi_dispatches += 1;
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, _byte: u8) {
        // Nothing to do
    }
}
