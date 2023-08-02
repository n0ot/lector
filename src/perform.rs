use vte::{Params, Perform};

/// Processes text from VTE, storing new text to be printed.
/// If Anything other than new text is printed, or attributes are changed, the text will be
/// cleared, as it might otherwise be meaningless.
pub struct TextReporter {
    text: String,
    stop_capturing: bool,
}

impl TextReporter {
    pub fn new() -> Self {
        TextReporter {
            text: String::new(),
            stop_capturing: false,
        }
    }

    pub fn backspace(&mut self) {
        self.text.pop();
    }

    /// returns a copy of the text seen so far, and clears it.
    /// If capturing was stopped for whatever reason, None will be returned.
    pub fn get_text(&mut self) -> Option<String> {
        if self.stop_capturing {
            self.stop_capturing = false;
            None
        } else {
            self.stop_capturing = false;
            let text = self.text.clone();
            self.text.clear();
            Some(text)
        }
    }

    /// Clears the text and stops capturing anymore until [`get_text`] is called.
    fn stop_capturing(&mut self) {
        self.stop_capturing = true;
        self.text.clear()
    }
}

impl Perform for TextReporter {
    fn print(&mut self, c: char) {
        if self.stop_capturing {
            return;
        }
        self.text.push(c);
    }

    fn execute(&mut self, byte: u8) {
        if self.stop_capturing {
            return;
        }
        match byte {
            8 => self.backspace(),     // Backspace
            _ => self.text.push('\n'), // Not always correct, but fine for auto reading
        };
    }

    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _c: char) {
        // Nothing to do
    }

    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {
        // Nothing to do
    }

    fn csi_dispatch(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, c: char) {
        match c {
            'h' | 'l' | 'm' => return,
            _ => self.stop_capturing(),
        }
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, _byte: u8) {
        self.stop_capturing()
    }
}
