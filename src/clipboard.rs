use std::collections::VecDeque;

pub struct Clipboard {
    idx: usize,
    clipboards: VecDeque<String>,
}

impl Default for Clipboard {
    fn default() -> Self {
        Self {
            idx: 0,
            clipboards: VecDeque::with_capacity(10),
        }
    }
}

impl Clipboard {
    /// Get the text from the selected clipboard.
    /// If there are no clipboards, None will be returned.
    pub fn get(&self) -> Option<&str> {
        self.clipboards.get(self.idx).map(String::as_str)
    }

    /// Add a clipboard with the specified text and select it.
    /// The oldest clipboards will be removed to make room for newer ones.
    pub fn put(&mut self, text: String) {
        if self.clipboards.len() >= 10 {
            self.clipboards.pop_front();
        }
        self.idx = self.clipboards.len();
        self.clipboards.push_back(text);
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

    pub fn index(&self) -> usize {
        self.idx
    }
}

#[cfg(test)]
mod tests {
    use super::Clipboard;

    #[test]
    fn keeps_the_ten_newest_entries_and_preserves_navigation() {
        let mut clipboard = Clipboard::default();
        for value in 0..12 {
            clipboard.put(value.to_string());
        }

        assert_eq!(clipboard.size(), 10);
        assert_eq!(clipboard.get(), Some("11"));
        for _ in 0..9 {
            assert!(clipboard.next());
        }
        assert_eq!(clipboard.get(), Some("2"));
        assert!(!clipboard.next());
        assert!(clipboard.prev());
        assert_eq!(clipboard.get(), Some("3"));
    }
}
