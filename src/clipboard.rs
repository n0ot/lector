use std::{collections::VecDeque, fmt, str::FromStr};

const INTERNAL_CLIPBOARD_CAPACITY: usize = 10;
const OSC52_PREFIX: &[u8] = b"\x1b]52;c;";
const OSC52_SUFFIX: &[u8] = b"\x1b\\";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum ClipboardRegister {
    #[default]
    Internal,
    System,
}

impl fmt::Display for ClipboardRegister {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Internal => "\"",
            Self::System => "+",
        })
    }
}

impl FromStr for ClipboardRegister {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "\"" => Ok(Self::Internal),
            "+" => Ok(Self::System),
            _ => anyhow::bail!("clipboard register must be \" or +"),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum SystemClipboardProvider {
    #[default]
    Native,
    Osc52,
}

impl fmt::Display for SystemClipboardProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Native => "native",
            Self::Osc52 => "osc52",
        })
    }
}

impl FromStr for SystemClipboardProvider {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "native" => Ok(Self::Native),
            "osc52" => Ok(Self::Osc52),
            _ => anyhow::bail!("system clipboard provider must be native or osc52"),
        }
    }
}

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
        if self.clipboards.len() >= INTERNAL_CLIPBOARD_CAPACITY {
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

    /// Returns a snapshot ordered the way users encounter the ring: newest
    /// first.
    pub fn entries(&self) -> Vec<String> {
        self.clipboards.iter().rev().cloned().collect()
    }

    /// Returns the selected entry's one-based index in [`Self::entries`].
    pub fn selected_index(&self) -> Option<usize> {
        (!self.clipboards.is_empty()).then(|| self.clipboards.len().saturating_sub(self.idx))
    }

    /// Select a one-based index in the newest-first public ordering.
    pub fn select_index(&mut self, index: usize) -> bool {
        if index == 0 || index > self.clipboards.len() {
            return false;
        }
        self.idx = self.clipboards.len() - index;
        true
    }

    pub fn clear(&mut self) {
        self.clipboards.clear();
        self.idx = 0;
    }
}

#[derive(Default)]
pub(crate) struct SystemClipboard {
    native: Option<arboard::Clipboard>,
    pending_terminal_writes: VecDeque<Vec<u8>>,
}

impl SystemClipboard {
    fn native(&mut self) -> anyhow::Result<&mut arboard::Clipboard> {
        if self.native.is_none() {
            self.native = Some(
                arboard::Clipboard::new()
                    .map_err(|error| anyhow::anyhow!("open native system clipboard: {error}"))?,
            );
        }
        Ok(self.native.as_mut().expect("native clipboard initialized"))
    }

    pub fn read(&mut self, provider: SystemClipboardProvider) -> anyhow::Result<Option<String>> {
        match provider {
            SystemClipboardProvider::Native => match self.native()?.get_text() {
                Ok(text) => Ok(Some(text)),
                Err(arboard::Error::ContentNotAvailable) => Ok(None),
                Err(error) => Err(anyhow::anyhow!("read native system clipboard: {error}")),
            },
            SystemClipboardProvider::Osc52 => {
                anyhow::bail!("system clipboard provider osc52 is write-only")
            }
        }
    }

    pub fn write(&mut self, provider: SystemClipboardProvider, text: String) -> anyhow::Result<()> {
        match provider {
            SystemClipboardProvider::Native => self
                .native()?
                .set_text(text)
                .map_err(|error| anyhow::anyhow!("write native system clipboard: {error}")),
            SystemClipboardProvider::Osc52 => {
                self.pending_terminal_writes.push_back(osc52_write(&text));
                Ok(())
            }
        }
    }

    pub fn clear(&mut self, provider: SystemClipboardProvider) -> anyhow::Result<()> {
        match provider {
            SystemClipboardProvider::Native => self
                .native()?
                .clear()
                .map_err(|error| anyhow::anyhow!("clear native system clipboard: {error}")),
            SystemClipboardProvider::Osc52 => {
                let mut sequence = Vec::with_capacity(OSC52_PREFIX.len() + OSC52_SUFFIX.len());
                sequence.extend_from_slice(OSC52_PREFIX);
                sequence.extend_from_slice(OSC52_SUFFIX);
                self.pending_terminal_writes.push_back(sequence);
                Ok(())
            }
        }
    }

    pub fn take_terminal_writes(&mut self) -> Vec<Vec<u8>> {
        self.pending_terminal_writes.drain(..).collect()
    }
}

fn osc52_write(text: &str) -> Vec<u8> {
    let encoded = encode_base64(text.as_bytes());
    let mut sequence = Vec::with_capacity(OSC52_PREFIX.len() + encoded.len() + OSC52_SUFFIX.len());
    sequence.extend_from_slice(OSC52_PREFIX);
    sequence.extend_from_slice(encoded.as_bytes());
    sequence.extend_from_slice(OSC52_SUFFIX);
    sequence
}

fn encode_base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        output.push(TABLE[usize::from(first >> 2)] as char);
        output.push(TABLE[usize::from((first & 0x03) << 4 | second >> 4)] as char);
        if chunk.len() > 1 {
            output.push(TABLE[usize::from((second & 0x0f) << 2 | third >> 6)] as char);
        } else {
            output.push('=');
        }
        if chunk.len() > 2 {
            output.push(TABLE[usize::from(third & 0x3f)] as char);
        } else {
            output.push('=');
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::{Clipboard, ClipboardRegister, SystemClipboard, SystemClipboardProvider};

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

    #[test]
    fn public_entries_and_indices_are_newest_first_and_one_based() {
        let mut clipboard = Clipboard::default();
        clipboard.put("older".into());
        clipboard.put("newer".into());

        assert_eq!(clipboard.entries(), ["newer", "older"]);
        assert_eq!(clipboard.selected_index(), Some(1));
        assert!(clipboard.select_index(2));
        assert_eq!(clipboard.get(), Some("older"));
        assert_eq!(clipboard.selected_index(), Some(2));
        assert!(!clipboard.select_index(0));
        assert!(!clipboard.select_index(3));
        clipboard.clear();
        assert_eq!(clipboard.selected_index(), None);
    }

    #[test]
    fn register_and_provider_names_are_stable() {
        assert_eq!(
            "\"".parse::<ClipboardRegister>().unwrap(),
            ClipboardRegister::Internal
        );
        assert_eq!(
            "+".parse::<ClipboardRegister>().unwrap(),
            ClipboardRegister::System
        );
        assert_eq!(
            "native".parse::<SystemClipboardProvider>().unwrap(),
            SystemClipboardProvider::Native
        );
        assert_eq!(
            "osc52".parse::<SystemClipboardProvider>().unwrap(),
            SystemClipboardProvider::Osc52
        );
    }

    #[test]
    fn osc52_writes_and_clears_are_queued_for_the_outer_terminal() {
        let mut clipboard = SystemClipboard::default();
        clipboard
            .write(SystemClipboardProvider::Osc52, "hello".into())
            .unwrap();
        clipboard.clear(SystemClipboardProvider::Osc52).unwrap();

        assert_eq!(
            clipboard.take_terminal_writes(),
            [
                b"\x1b]52;c;aGVsbG8=\x1b\\".to_vec(),
                b"\x1b]52;c;\x1b\\".to_vec()
            ]
        );
        assert!(clipboard.take_terminal_writes().is_empty());
        assert!(clipboard.read(SystemClipboardProvider::Osc52).is_err());
    }
}
