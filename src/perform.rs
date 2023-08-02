use vte::{Params, Perform};

/// Processes text from VTE, storing new text to be printed.
/// If Anything other than new text is printed, or attributes are changed, the text will be
/// cleared, as it might otherwise be meaningless.
pub struct TextReporter {
    text: String,
    state_changes: u64,
}

impl TextReporter {
    pub fn new() -> Self {
        TextReporter {
            text: String::new(),
            state_changes: 0,
        }
    }

    pub fn backspace(&mut self) {
        self.text.pop();
    }

    /// returns a copy of the text seen so far, and clears it.
    pub fn get_text(&mut self) -> String {
        let text = self.text.clone();
        self.text.clear();
        self.state_changes = 0;
        text
    }

    pub fn get_state_changes(&self) -> u64 {
        self.state_changes
    }
}

impl Perform for TextReporter {
    fn print(&mut self, c: char) {
        self.text.push(c);
    }

    fn execute(&mut self, byte: u8) {
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
            _ => {
                self.text.push(' ');
                self.state_changes += 1;
            }
        }
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, _byte: u8) {
        self.text.push(' ');
        self.state_changes += 1;
    }
}
