#[derive(Default)]
pub struct Clipboard {
    pub mark: Option<(u16, u16)>,
    idx: usize,
    clipboards: Vec<String>,
}

impl Clipboard {
    /// Get the text from the selected clipboard.
    /// If there are no clipboards, None will be returned.
    pub fn get(&self) -> Option<&str> {
        if self.clipboards.is_empty() {
            return None;
        }
        Some(&self.clipboards[self.idx])
    }

    /// Add a clipboard with the specified text and select it.
    /// The oldest clipboards will be removed to make room for newer ones.
    pub fn put(&mut self, text: String) {
        if self.clipboards.len() >= 10 {
            self.clipboards.remove(0);
        }
        self.idx = self.clipboards.len();
        self.clipboards.push(text);
    }

    /// Try to select the previous clipboard, and return whether a different clipboard has been selected.
    /// If there is no previous clipboard, this method will have no effect.
    pub fn prev(&mut self) -> bool {
        if self.idx + 1 >= self.size() {
            false
        } else {
            self.idx += 1;
            true
        }
    }

    /// Try to select the next clipboard, and return whether a different clipboard has been selected.
    /// If there is no next clipboard, this method will have no effect.
    pub fn next(&mut self) -> bool {
        if self.idx == 0 {
            false
        } else {
            self.idx -= 1;
            true
        }
    }

    /// Returns the number of clipboards.
    pub fn size(&self) -> usize {
        self.clipboards.len()
    }
}
