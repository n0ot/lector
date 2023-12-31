use vte::{Params, Perform};

/// Processes text from VTE, storing new text to be printed.
pub struct TextReporter {
    /// Stores characters printed to the screen
    text: String,
    /// True if the next call to `[print]` should clear the text.
    reset: bool,
    pub cursor_moves: usize,
    pub scrolled: bool,
}

impl TextReporter {
    pub fn new() -> Self {
        TextReporter {
            text: String::new(),
            reset: false,
            cursor_moves: 0,
            scrolled: false,
        }
    }

    /// returns a reference of the text seen so far.
    /// Future interactions with this TextReporter may overwrite this text.
    pub fn get_text(&mut self) -> &str {
        if self.reset {
            self.text.clear();
        }
        self.reset = true;
        self.cursor_moves = 0;
        self.scrolled = false;
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
            8 => self.cursor_moves += 1,
            10 | 13 => self.text.push('\n'),
            _ => {}
        }
    }

    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _c: char) {
        // Nothing to do
    }

    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {
        // Nothing to do
    }

    fn csi_dispatch(&mut self, _params: &Params, intermediates: &[u8], _ignore: bool, c: char) {
        if intermediates.first().is_none() {
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
