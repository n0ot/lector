use vte::{Params, Perform};

/// Processes text from VTE, storing new text to be printed.
/// If Anything other than new text is printed, or attributes are changed, the text will be
/// cleared, as it might otherwise be meaningless.
pub struct TextReporter {
    /// Stores characters printed to the screen
    text: String,
    /// True if no more text will be recorded until `[get_text]` is called.
    stop: bool,
    /// True if the next call to `[print]` should clear the text.
    reset: bool,
}

impl TextReporter {
    pub fn new() -> Self {
        TextReporter {
            text: String::new(),
            stop: false,
            reset: false,
        }
    }

    /// returns a reference of the text seen so far.
    /// Future interactions with this TextReporter may overwrite this text.
    pub fn get_text(&mut self) -> &str {
        if self.reset || self.stop {
            self.text.clear();
        }
        self.reset = true;
        self.stop = false;
        &self.text
    }
}

impl Perform for TextReporter {
    fn print(&mut self, c: char) {
        if self.stop {
            return;
        }
        if self.reset {
            self.text.clear();
            self.reset = false;
        }
        self.text.push(c);
    }

    fn execute(&mut self, _byte: u8) {
        if self.stop {
            return;
        }
        self.text.push('\n'); // Not always correct, but fine for auto reading
    }

    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _c: char) {
        // Nothing to do
    }

    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {
        // Nothing to do
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, c: char) {
        // If the cursor moves, but stays to the left, we just want to add a linefeed to the text
        // to separate chunks.
        // If attributes change, we don't have to do anything.
        // Otherwise, stop recording text.
        match intermediates.first() {
            None => match c {
                // Go toward the left
                'D' => self.text.push('\n'),
                // Move up/down a line (cursor goes to the left), or scroll up/down
                'E' | 'F' | 'S' | 'T' => self.text.push('\n'),
                // Move the cursor to the beginning of the line
                'G' if canonicalize_params_1(params, 1) == 1 => self.text.push('\n'),
                // Move the cursor to any row, but to the first column
                'H' if canonicalize_params_2(params, 1, 1).1 == 1 => self.text.push('\n'),
                // Clear to the end of the line
                'K' if canonicalize_params_1(params, 0) == 0 => self.text.push('\n'),
                // m is select graphics mode, r sets the scrolling region, the rest don't seem to do anything
                'h' | 'l' | 'm' | 'r' | 't' => {}
                //_ => self.stop = true,
                _ => self.text.push_str(&format!("  warning...   found {}  ", c)),
            },
            Some(b'?') => match c {
                // Show/hide the cursor or enable/disable bracketed paste
                'h' | 'l' if params.iter().all(|p| p == &[25] || p == &[2004]) => {}
                _ => self.stop = true,
            },
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, _byte: u8) {
        self.stop = true;
    }
}

fn canonicalize_params_1(params: &vte::Params, default: u16) -> u16 {
    let first = params.iter().next().map_or(0, |x| *x.first().unwrap_or(&0));
    if first == 0 {
        default
    } else {
        first
    }
}

fn canonicalize_params_2(params: &vte::Params, default1: u16, default2: u16) -> (u16, u16) {
    let mut iter = params.iter();
    let first = iter.next().map_or(0, |x| *x.first().unwrap_or(&0));
    let first = if first == 0 { default1 } else { first };

    let second = iter.next().map_or(0, |x| *x.first().unwrap_or(&0));
    let second = if second == 0 { default2 } else { second };

    (first, second)
}
