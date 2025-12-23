use vte::{Params, Perform};

/// Processes text from VTE, storing new text to be printed.
pub struct Reporter {
    pub cursor_moves: usize,
    pub scrolled: bool,
}

impl Reporter {
    pub fn new() -> Self {
        Reporter {
            cursor_moves: 0,
            scrolled: false,
        }
    }

    pub fn reset(&mut self) {
        self.cursor_moves = 0;
        self.scrolled = false;
    }
}

impl Perform for Reporter {
    fn print(&mut self, _c: char) {
        // Nothing to do
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            8 => self.cursor_moves += 1,
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
