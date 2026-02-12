use anyhow::{Result, anyhow};
use std::collections::HashMap;

pub struct SymbolMap(HashMap<String, SymbolDesc>);

impl Default for SymbolMap {
    fn default() -> Self {
        Self::new()
    }
}

impl SymbolMap {
    pub fn new() -> Self {
        SymbolMap(HashMap::new())
    }

    pub fn default_map() -> Self {
        use IncludeOriginal::*;
        use Level::*;
        let mut m = Self::new();

        // Whitespace
        m.put(" ", "space", Character, Never, false);
        m.put("	", "tab", Character, Never, false);

        // Basic punctuation
        m.put("!", "bang", All, After, true);
        m.put("¡", "inverted bang", Some, After, true);
        m.put("\"", "quote", Most, Never, true);
        m.put("“", "left quote", Most, Never, true);
        m.put("”", "right quote", Most, Never, true);
        m.put("#", "number", Some, Never, true);
        m.put("%", "percent", Some, Never, true);
        m.put("&", "and", Some, Never, true);
        m.put("'", "tick", Most, Never, true);
        m.put("‘", "left tick", Most, Never, true);
        m.put("’", "right tick", Most, Never, true);
        m.put("(", "left paren", Most, After, true);
        m.put(")", "right paren", Most, Before, true);
        m.put("*", "star", Some, Never, true);
        m.put("+", "plus", Some, Never, true);
        m.put(",", "comma", All, After, true);
        m.put("-", "dash", Most, After, true);
        m.put("–", "en dash", Most, After, true);
        m.put("—", "em dash", Most, After, true);
        m.put("\u{AD}", "soft hyphen", Most, Never, true);
        m.put("⁃", "hyphen", None, Never, true);
        m.put(".", "dot", All, After, true);
        m.put("…", "dot dot dot", All, After, true);
        m.put("·", "middle dot", Most, Never, true);
        m.put("/", "slash", Some, Never, true);
        m.put(":", "colon", Most, After, true);
        m.put(";", "semi", Most, After, true);
        m.put("<", "less", Some, Never, true);
        m.put("=", "equals", Some, Never, true);
        m.put(">", "greater", Some, Never, true);
        m.put("?", "question", All, After, true);
        m.put("¿", "inverted question", Some, After, true);
        m.put("@", "at", Some, Never, true);
        m.put("[", "left bracket", Some, Never, true);
        m.put("\\", "backslash", Most, Never, true);
        m.put("]", "right bracket", Some, Never, true);
        m.put("^", "carrat", Most, Never, true);
        m.put("_", "line", Most, Never, true);
        m.put("`", "graav", Most, Never, true);
        m.put("{", "left brace", Some, Never, true);
        m.put("|", "bar", Most, Never, true);
        m.put("¦", "broken bar", Most, Never, true);
        m.put("}", "right brace", Some, Never, true);
        m.put("~", "tilde", Most, Never, true);

        // Currency
        m.put("¤", "currency", All, Never, false);
        m.put("₿", "bitcoin", All, Never, false);
        m.put("$", "dollar", All, Never, false);
        m.put("¢", "cents", All, Never, false);
        m.put("£", "pound", All, Never, false);
        m.put("€", "euro", All, Never, false);
        m.put("¥", "yen", All, Never, false);

        // Shapes
        m.put("■", "black square", Some, Never, true);
        m.put("▪", "black small square", Some, Never, true);
        m.put("◾", "black medium small square", Some, Never, true);
        m.put("□", "white square", Some, Never, true);
        m.put("◦", "white bullet", Some, Never, true);
        m.put("➔", "right arrow", Some, Never, true);
        m.put("⇨", "right white arrow", Some, Never, true);
        m.put("●", "circle", Most, Never, true);
        m.put("○", "white circle", Most, Never, true);

        // Misc
        m.put("′", "prime", None, Never, true);
        m.put("″", "double prime", None, Never, true);
        m.put("‴", "tripple prime", None, Never, true);
        m.put("•", "bullet", Some, Never, true);
        m.put("§", "section", Some, Never, true);
        m.put("°", "degrees", Some, Never, true);
        m.put("µ", "micro", Some, Never, true);
        m.put("®", "registered", Some, Never, true);
        m.put("™", "trademark", Some, Never, true);
        m.put("©", "copyright", Some, Never, true);
        m.put("℠", "service mark", Some, Never, true);

        // Box drawings
        m.put("─", "box drawing Light Horizontal", Character, Never, true);
        m.put("━", "box drawing Heavy Horizontal", Character, Never, true);
        m.put("│", "box drawing Light Vertical", Character, Never, true);
        m.put("┃", "box drawing Heavy Vertical", Character, Never, true);
        m.put(
            "┄",
            "box drawing Light Triple Dash Horizontal",
            Character,
            Never,
            true,
        );
        m.put(
            "┅",
            "box drawing Heavy Triple Dash Horizontal",
            Character,
            Never,
            true,
        );
        m.put(
            "┆",
            "box drawing Light Triple Dash Vertical",
            Character,
            Never,
            true,
        );
        m.put(
            "┇",
            "box drawing Heavy Triple Dash Vertical",
            Character,
            Never,
            true,
        );
        m.put(
            "┈",
            "box drawing Light Quadruple Dash Horizontal",
            Character,
            Never,
            true,
        );
        m.put(
            "┉",
            "box drawing Heavy Quadruple Dash Horizontal",
            Character,
            Never,
            true,
        );
        m.put(
            "┊",
            "box drawing Light Quadruple Dash Vertical",
            Character,
            Never,
            true,
        );
        m.put(
            "┋",
            "box drawing Heavy Quadruple Dash Vertical",
            Character,
            Never,
            true,
        );
        m.put(
            "┌",
            "box drawing Light Down and Right",
            Character,
            Never,
            true,
        );
        m.put(
            "┍",
            "box drawing Down Light and Right Heavy",
            Character,
            Never,
            true,
        );
        m.put(
            "┎",
            "box drawing Down Heavy and Right Light",
            Character,
            Never,
            true,
        );
        m.put(
            "┏",
            "box drawing Heavy Down and Right",
            Character,
            Never,
            true,
        );
        m.put(
            "┐",
            "box drawing Light Down and Left",
            Character,
            Never,
            true,
        );
        m.put(
            "┑",
            "box drawing Down Light and Left Heavy",
            Character,
            Never,
            true,
        );
        m.put(
            "┒",
            "box drawing Down Heavy and Left Light",
            Character,
            Never,
            true,
        );
        m.put(
            "┓",
            "box drawing Heavy Down and Left",
            Character,
            Never,
            true,
        );
        m.put(
            "└",
            "box drawing Light Up and Right",
            Character,
            Never,
            true,
        );
        m.put(
            "┕",
            "box drawing Up Light and Right Heavy",
            Character,
            Never,
            true,
        );
        m.put(
            "┖",
            "box drawing Up Heavy and Right Light",
            Character,
            Never,
            true,
        );
        m.put(
            "┗",
            "box drawing Heavy Up and Right",
            Character,
            Never,
            true,
        );
        m.put("┘", "box drawing Light Up and Left", Character, Never, true);
        m.put(
            "┙",
            "box drawing Up Light and Left Heavy",
            Character,
            Never,
            true,
        );
        m.put(
            "┚",
            "box drawing Up Heavy and Left Light",
            Character,
            Never,
            true,
        );
        m.put("┛", "box drawing Heavy Up and Left", Character, Never, true);
        m.put(
            "├",
            "box drawing Light Vertical and Right",
            Character,
            Never,
            true,
        );
        m.put(
            "┝",
            "box drawing Vertical Light and Right Heavy",
            Character,
            Never,
            true,
        );
        m.put(
            "┞",
            "box drawing Up Heavy and Right Down Light",
            Character,
            Never,
            true,
        );
        m.put(
            "┟",
            "box drawing Down Heavy and Right Up Light",
            Character,
            Never,
            true,
        );
        m.put(
            "┠",
            "box drawing Vertical Heavy and Right Light",
            Character,
            Never,
            true,
        );
        m.put(
            "┡",
            "box drawing Down Light and Right Up Heavy",
            Character,
            Never,
            true,
        );
        m.put(
            "┢",
            "box drawing Up Light and Right Down Heavy",
            Character,
            Never,
            true,
        );
        m.put(
            "┣",
            "box drawing Heavy Vertical and Right",
            Character,
            Never,
            true,
        );
        m.put(
            "┤",
            "box drawing Light Vertical and Left",
            Character,
            Never,
            true,
        );
        m.put(
            "┥",
            "box drawing Vertical Light and Left Heavy",
            Character,
            Never,
            true,
        );
        m.put(
            "┦",
            "box drawing Up Heavy and Left Down Light",
            Character,
            Never,
            true,
        );
        m.put(
            "┧",
            "box drawing Down Heavy and Left Up Light",
            Character,
            Never,
            true,
        );
        m.put(
            "┨",
            "box drawing Vertical Heavy and Left Light",
            Character,
            Never,
            true,
        );
        m.put(
            "┩",
            "box drawing Down Light and Left Up Heavy",
            Character,
            Never,
            true,
        );
        m.put(
            "┪",
            "box drawing Up Light and Left Down Heavy",
            Character,
            Never,
            true,
        );
        m.put(
            "┫",
            "box drawing Heavy Vertical and Left",
            Character,
            Never,
            true,
        );
        m.put(
            "┬",
            "box drawing Light Down and Horizontal",
            Character,
            Never,
            true,
        );
        m.put(
            "┭",
            "box drawing Left Heavy and Right Down Light",
            Character,
            Never,
            true,
        );
        m.put(
            "┮",
            "box drawing Right Heavy and Left Down Light",
            Character,
            Never,
            true,
        );
        m.put(
            "┯",
            "box drawing Down Light and Horizontal Heavy",
            Character,
            Never,
            true,
        );
        m.put(
            "┰",
            "box drawing Down Heavy and Horizontal Light",
            Character,
            Never,
            true,
        );
        m.put(
            "┱",
            "box drawing Right Light and Left Down Heavy",
            Character,
            Never,
            true,
        );
        m.put(
            "┲",
            "box drawing Left Light and Right Down Heavy",
            Character,
            Never,
            true,
        );
        m.put(
            "┳",
            "box drawing Heavy Down and Horizontal",
            Character,
            Never,
            true,
        );
        m.put(
            "┴",
            "box drawing Light Up and Horizontal",
            Character,
            Never,
            true,
        );
        m.put(
            "┵",
            "box drawing Left Heavy and Right Up Light",
            Character,
            Never,
            true,
        );
        m.put(
            "┶",
            "box drawing Right Heavy and Left Up Light",
            Character,
            Never,
            true,
        );
        m.put(
            "┷",
            "box drawing Up Light and Horizontal Heavy",
            Character,
            Never,
            true,
        );
        m.put(
            "┸",
            "box drawing Up Heavy and Horizontal Light",
            Character,
            Never,
            true,
        );
        m.put(
            "┹",
            "box drawing Right Light and Left Up Heavy",
            Character,
            Never,
            true,
        );
        m.put(
            "┺",
            "box drawing Left Light and Right Up Heavy",
            Character,
            Never,
            true,
        );
        m.put(
            "┻",
            "box drawing Heavy Up and Horizontal",
            Character,
            Never,
            true,
        );
        m.put(
            "┼",
            "box drawing Light Vertical and Horizontal",
            Character,
            Never,
            true,
        );
        m.put(
            "┽",
            "box drawing Left Heavy and Right Vertical Light",
            Character,
            Never,
            true,
        );
        m.put(
            "┾",
            "box drawing Right Heavy and Left Vertical Light",
            Character,
            Never,
            true,
        );
        m.put(
            "┿",
            "box drawing Vertical Light and Horizontal Heavy",
            Character,
            Never,
            true,
        );
        m.put(
            "╀",
            "box drawing Up Heavy and Down Horizontal Light",
            Character,
            Never,
            true,
        );
        m.put(
            "╁",
            "box drawing Down Heavy and Up Horizontal Light",
            Character,
            Never,
            true,
        );
        m.put(
            "╂",
            "box drawing Vertical Heavy and Horizontal Light",
            Character,
            Never,
            true,
        );
        m.put(
            "╃",
            "box drawing Left Up Heavy and Right Down Light",
            Character,
            Never,
            true,
        );
        m.put(
            "╄",
            "box drawing Right Up Heavy and Left Down Light",
            Character,
            Never,
            true,
        );
        m.put(
            "╅",
            "box drawing Left Down Heavy and Right Up Light",
            Character,
            Never,
            true,
        );
        m.put(
            "╆",
            "box drawing Right Down Heavy and Left Up Light",
            Character,
            Never,
            true,
        );
        m.put(
            "╇",
            "box drawing Down Light and Up Horizontal Heavy",
            Character,
            Never,
            true,
        );
        m.put(
            "╈",
            "box drawing Up Light and Down Horizontal Heavy",
            Character,
            Never,
            true,
        );
        m.put(
            "╉",
            "box drawing Right Light and Left Vertical Heavy",
            Character,
            Never,
            true,
        );
        m.put(
            "╊",
            "box drawing Left Light and Right Vertical Heavy",
            Character,
            Never,
            true,
        );
        m.put(
            "╋",
            "box drawing Heavy Vertical and Horizontal",
            Character,
            Never,
            true,
        );
        m.put(
            "╌",
            "box drawing Light Double Dash Horizontal",
            Character,
            Never,
            true,
        );
        m.put(
            "╍",
            "box drawing Heavy Double Dash Horizontal",
            Character,
            Never,
            true,
        );
        m.put(
            "╎",
            "box drawing Light Double Dash Vertical",
            Character,
            Never,
            true,
        );
        m.put(
            "╏",
            "box drawing Heavy Double Dash Vertical",
            Character,
            Never,
            true,
        );
        m.put("═", "box drawing Double Horizontal", Character, Never, true);
        m.put("║", "box drawing Double Vertical", Character, Never, true);
        m.put(
            "╒",
            "box drawing Down Single and Right Double",
            Character,
            Never,
            true,
        );
        m.put(
            "╓",
            "box drawing Down Double and Right Single",
            Character,
            Never,
            true,
        );
        m.put(
            "╔",
            "box drawing Double Down and Right",
            Character,
            Never,
            true,
        );
        m.put(
            "╕",
            "box drawing Down Single and Left Double",
            Character,
            Never,
            true,
        );
        m.put(
            "╖",
            "box drawing Down Double and Left Single",
            Character,
            Never,
            true,
        );
        m.put(
            "╗",
            "box drawing Double Down and Left",
            Character,
            Never,
            true,
        );
        m.put(
            "╘",
            "box drawing Up Single and Right Double",
            Character,
            Never,
            true,
        );
        m.put(
            "╙",
            "box drawing Up Double and Right Single",
            Character,
            Never,
            true,
        );
        m.put(
            "╚",
            "box drawing Double Up and Right",
            Character,
            Never,
            true,
        );
        m.put(
            "╛",
            "box drawing Up Single and Left Double",
            Character,
            Never,
            true,
        );
        m.put(
            "╜",
            "box drawing Up Double and Left Single",
            Character,
            Never,
            true,
        );
        m.put(
            "╝",
            "box drawing Double Up and Left",
            Character,
            Never,
            true,
        );
        m.put(
            "╞",
            "box drawing Vertical Single and Right Double",
            Character,
            Never,
            true,
        );
        m.put(
            "╟",
            "box drawing Vertical Double and Right Single",
            Character,
            Never,
            true,
        );
        m.put(
            "╠",
            "box drawing Double Vertical and Right",
            Character,
            Never,
            true,
        );
        m.put(
            "╡",
            "box drawing Vertical Single and Left Double",
            Character,
            Never,
            true,
        );
        m.put(
            "╢",
            "box drawing Vertical Double and Left Single",
            Character,
            Never,
            true,
        );
        m.put(
            "╣",
            "box drawing Double Vertical and Left",
            Character,
            Never,
            true,
        );
        m.put(
            "╤",
            "box drawing Down Single and Horizontal Double",
            Character,
            Never,
            true,
        );
        m.put(
            "╥",
            "box drawing Down Double and Horizontal Single",
            Character,
            Never,
            true,
        );
        m.put(
            "╦",
            "box drawing Double Down and Horizontal",
            Character,
            Never,
            true,
        );
        m.put(
            "╧",
            "box drawing Up Single and Horizontal Double",
            Character,
            Never,
            true,
        );
        m.put(
            "╨",
            "box drawing Up Double and Horizontal Single",
            Character,
            Never,
            true,
        );
        m.put(
            "╩",
            "box drawing Double Up and Horizontal",
            Character,
            Never,
            true,
        );
        m.put(
            "╪",
            "box drawing Vertical Single and Horizontal Double",
            Character,
            Never,
            true,
        );
        m.put(
            "╫",
            "box drawing Vertical Double and Horizontal Single",
            Character,
            Never,
            true,
        );
        m.put(
            "╬",
            "box drawing Double Vertical and Horizontal",
            Character,
            Never,
            true,
        );
        m.put(
            "╭",
            "box drawing Light Arc Down and Right",
            Character,
            Never,
            true,
        );
        m.put(
            "╮",
            "box drawing Light Arc Down and Left",
            Character,
            Never,
            true,
        );
        m.put(
            "╯",
            "box drawing Light Arc Up and Left",
            Character,
            Never,
            true,
        );
        m.put(
            "╰",
            "box drawing Light Arc Up and Right",
            Character,
            Never,
            true,
        );
        m.put(
            "╱",
            "box drawing Light Diagonal Upper Right to Lower Left",
            Character,
            Never,
            true,
        );
        m.put(
            "╲",
            "box drawing Light Diagonal Upper Left to Lower Right",
            Character,
            Never,
            true,
        );
        m.put(
            "╳",
            "box drawing Light Diagonal Cross",
            Character,
            Never,
            true,
        );
        m.put("╴", "box drawing Light Left", Character, Never, true);
        m.put("╵", "box drawing Light Up", Character, Never, true);
        m.put("╶", "box drawing Light Right", Character, Never, true);
        m.put("╷", "box drawing Light Down", Character, Never, true);
        m.put("╸", "box drawing Heavy Left", Character, Never, true);
        m.put("╹", "box drawing Heavy Up", Character, Never, true);
        m.put("╺", "box drawing Heavy Right", Character, Never, true);
        m.put("╻", "box drawing Heavy Down", Character, Never, true);
        m.put(
            "╼",
            "box drawing Light Left and Heavy Right",
            Character,
            Never,
            true,
        );
        m.put(
            "╽",
            "box drawing Light Up and Heavy Down",
            Character,
            Never,
            true,
        );
        m.put(
            "╾",
            "box drawing Heavy Left and Light Right",
            Character,
            Never,
            true,
        );
        m.put(
            "╿",
            "box drawing Heavy Up and Light Down",
            Character,
            Never,
            true,
        );

        m
    }

    pub fn put(
        &mut self,
        symbol: &str,
        replacement: &str,
        level: Level,
        include_original: IncludeOriginal,
        repeat: bool,
    ) {
        self.0.insert(
            symbol.into(),
            SymbolDesc::new(replacement.into(), level, include_original, repeat),
        );
    }

    pub fn get(&self, symbol: &str) -> Option<&SymbolDesc> {
        self.0.get(symbol)
    }

    pub fn remove(&mut self, symbol: &str) {
        self.0.remove(symbol);
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }
}

/// Describes how a mapped symbol should be replaced
pub struct SymbolDesc {
    /// mapped symbols will be replaced with this string
    pub replacement: String,
    /// Replacement will take place at this symbol level or above
    pub level: Level,
    /// determines if and when the original symbol should be set to the synth,
    /// if being replaced
    pub include_original: IncludeOriginal,
    /// If true, repeated runs of symbols mapped to this SymbolDesc will be transformed to
    /// `<count> <replacement>`
    pub repeat: bool,
}

impl SymbolDesc {
    pub fn new(
        replacement: String,
        level: Level,
        include_original: IncludeOriginal,
        repeat: bool,
    ) -> SymbolDesc {
        SymbolDesc {
            replacement,
            level,
            include_original,
            repeat,
        }
    }
}

#[derive(Copy, Clone, PartialEq, PartialOrd)]
pub enum Level {
    None,
    Some,
    Most,
    All,
    Character,
}

impl std::fmt::Display for Level {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Level::None => "none",
                Level::Some => "some",
                Level::Most => "most",
                Level::All => "all",
                Level::Character => "character",
            }
        )
    }
}

impl std::str::FromStr for Level {
    type Err = anyhow::Error;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        match input {
            "none" => Ok(Level::None),
            "some" => Ok(Level::Some),
            "most" => Ok(Level::Most),
            "all" => Ok(Level::All),
            "character" => Ok(Level::Character),
            _ => Err(anyhow!("unknown symbol level")),
        }
    }
}

#[derive(Copy, Clone)]
pub enum IncludeOriginal {
    Never,
    Before,
    After,
}

impl std::fmt::Display for IncludeOriginal {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                IncludeOriginal::Never => "never",
                IncludeOriginal::Before => "before",
                IncludeOriginal::After => "after",
            }
        )
    }
}

impl std::str::FromStr for IncludeOriginal {
    type Err = anyhow::Error;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        match input {
            "never" => Ok(IncludeOriginal::Never),
            "before" => Ok(IncludeOriginal::Before),
            "after" => Ok(IncludeOriginal::After),
            _ => Err(anyhow!("unknown variant")),
        }
    }
}
