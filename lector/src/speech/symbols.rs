use std::collections::HashMap;

pub struct SymbolMap {
    map: HashMap<String, SymbolDesc>,
}

impl SymbolMap {
    pub fn new() -> Self {
        SymbolMap {
            map: HashMap::new(),
        }
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
        m.put("­", "soft hyphen", Most, Never, true);
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
        m.put("─", "box drawing Light Horizontal", None, Never, true);
        m.put("━", "box drawing Heavy Horizontal", None, Never, true);
        m.put("│", "box drawing Light Vertical", None, Never, true);
        m.put("┃", "box drawing Heavy Vertical", None, Never, true);
        m.put(
            "┄",
            "box drawing Light Triple Dash Horizontal",
            None,
            Never,
            true,
        );
        m.put(
            "┅",
            "box drawing Heavy Triple Dash Horizontal",
            None,
            Never,
            true,
        );
        m.put(
            "┆",
            "box drawing Light Triple Dash Vertical",
            None,
            Never,
            true,
        );
        m.put(
            "┇",
            "box drawing Heavy Triple Dash Vertical",
            None,
            Never,
            true,
        );
        m.put(
            "┈",
            "box drawing Light Quadruple Dash Horizontal",
            None,
            Never,
            true,
        );
        m.put(
            "┉",
            "box drawing Heavy Quadruple Dash Horizontal",
            None,
            Never,
            true,
        );
        m.put(
            "┊",
            "box drawing Light Quadruple Dash Vertical",
            None,
            Never,
            true,
        );
        m.put(
            "┋",
            "box drawing Heavy Quadruple Dash Vertical",
            None,
            Never,
            true,
        );
        m.put("┌", "box drawing Light Down and Right", None, Never, true);
        m.put(
            "┍",
            "box drawing Down Light and Right Heavy",
            None,
            Never,
            true,
        );
        m.put(
            "┎",
            "box drawing Down Heavy and Right Light",
            None,
            Never,
            true,
        );
        m.put("┏", "box drawing Heavy Down and Right", None, Never, true);
        m.put("┐", "box drawing Light Down and Left", None, Never, true);
        m.put(
            "┑",
            "box drawing Down Light and Left Heavy",
            None,
            Never,
            true,
        );
        m.put(
            "┒",
            "box drawing Down Heavy and Left Light",
            None,
            Never,
            true,
        );
        m.put("┓", "box drawing Heavy Down and Left", None, Never, true);
        m.put("└", "box drawing Light Up and Right", None, Never, true);
        m.put(
            "┕",
            "box drawing Up Light and Right Heavy",
            None,
            Never,
            true,
        );
        m.put(
            "┖",
            "box drawing Up Heavy and Right Light",
            None,
            Never,
            true,
        );
        m.put("┗", "box drawing Heavy Up and Right", None, Never, true);
        m.put("┘", "box drawing Light Up and Left", None, Never, true);
        m.put(
            "┙",
            "box drawing Up Light and Left Heavy",
            None,
            Never,
            true,
        );
        m.put(
            "┚",
            "box drawing Up Heavy and Left Light",
            None,
            Never,
            true,
        );
        m.put("┛", "box drawing Heavy Up and Left", None, Never, true);
        m.put(
            "├",
            "box drawing Light Vertical and Right",
            None,
            Never,
            true,
        );
        m.put(
            "┝",
            "box drawing Vertical Light and Right Heavy",
            None,
            Never,
            true,
        );
        m.put(
            "┞",
            "box drawing Up Heavy and Right Down Light",
            None,
            Never,
            true,
        );
        m.put(
            "┟",
            "box drawing Down Heavy and Right Up Light",
            None,
            Never,
            true,
        );
        m.put(
            "┠",
            "box drawing Vertical Heavy and Right Light",
            None,
            Never,
            true,
        );
        m.put(
            "┡",
            "box drawing Down Light and Right Up Heavy",
            None,
            Never,
            true,
        );
        m.put(
            "┢",
            "box drawing Up Light and Right Down Heavy",
            None,
            Never,
            true,
        );
        m.put(
            "┣",
            "box drawing Heavy Vertical and Right",
            None,
            Never,
            true,
        );
        m.put(
            "┤",
            "box drawing Light Vertical and Left",
            None,
            Never,
            true,
        );
        m.put(
            "┥",
            "box drawing Vertical Light and Left Heavy",
            None,
            Never,
            true,
        );
        m.put(
            "┦",
            "box drawing Up Heavy and Left Down Light",
            None,
            Never,
            true,
        );
        m.put(
            "┧",
            "box drawing Down Heavy and Left Up Light",
            None,
            Never,
            true,
        );
        m.put(
            "┨",
            "box drawing Vertical Heavy and Left Light",
            None,
            Never,
            true,
        );
        m.put(
            "┩",
            "box drawing Down Light and Left Up Heavy",
            None,
            Never,
            true,
        );
        m.put(
            "┪",
            "box drawing Up Light and Left Down Heavy",
            None,
            Never,
            true,
        );
        m.put(
            "┫",
            "box drawing Heavy Vertical and Left",
            None,
            Never,
            true,
        );
        m.put(
            "┬",
            "box drawing Light Down and Horizontal",
            None,
            Never,
            true,
        );
        m.put(
            "┭",
            "box drawing Left Heavy and Right Down Light",
            None,
            Never,
            true,
        );
        m.put(
            "┮",
            "box drawing Right Heavy and Left Down Light",
            None,
            Never,
            true,
        );
        m.put(
            "┯",
            "box drawing Down Light and Horizontal Heavy",
            None,
            Never,
            true,
        );
        m.put(
            "┰",
            "box drawing Down Heavy and Horizontal Light",
            None,
            Never,
            true,
        );
        m.put(
            "┱",
            "box drawing Right Light and Left Down Heavy",
            None,
            Never,
            true,
        );
        m.put(
            "┲",
            "box drawing Left Light and Right Down Heavy",
            None,
            Never,
            true,
        );
        m.put(
            "┳",
            "box drawing Heavy Down and Horizontal",
            None,
            Never,
            true,
        );
        m.put(
            "┴",
            "box drawing Light Up and Horizontal",
            None,
            Never,
            true,
        );
        m.put(
            "┵",
            "box drawing Left Heavy and Right Up Light",
            None,
            Never,
            true,
        );
        m.put(
            "┶",
            "box drawing Right Heavy and Left Up Light",
            None,
            Never,
            true,
        );
        m.put(
            "┷",
            "box drawing Up Light and Horizontal Heavy",
            None,
            Never,
            true,
        );
        m.put(
            "┸",
            "box drawing Up Heavy and Horizontal Light",
            None,
            Never,
            true,
        );
        m.put(
            "┹",
            "box drawing Right Light and Left Up Heavy",
            None,
            Never,
            true,
        );
        m.put(
            "┺",
            "box drawing Left Light and Right Up Heavy",
            None,
            Never,
            true,
        );
        m.put(
            "┻",
            "box drawing Heavy Up and Horizontal",
            None,
            Never,
            true,
        );
        m.put(
            "┼",
            "box drawing Light Vertical and Horizontal",
            None,
            Never,
            true,
        );
        m.put(
            "┽",
            "box drawing Left Heavy and Right Vertical Light",
            None,
            Never,
            true,
        );
        m.put(
            "┾",
            "box drawing Right Heavy and Left Vertical Light",
            None,
            Never,
            true,
        );
        m.put(
            "┿",
            "box drawing Vertical Light and Horizontal Heavy",
            None,
            Never,
            true,
        );
        m.put(
            "╀",
            "box drawing Up Heavy and Down Horizontal Light",
            None,
            Never,
            true,
        );
        m.put(
            "╁",
            "box drawing Down Heavy and Up Horizontal Light",
            None,
            Never,
            true,
        );
        m.put(
            "╂",
            "box drawing Vertical Heavy and Horizontal Light",
            None,
            Never,
            true,
        );
        m.put(
            "╃",
            "box drawing Left Up Heavy and Right Down Light",
            None,
            Never,
            true,
        );
        m.put(
            "╄",
            "box drawing Right Up Heavy and Left Down Light",
            None,
            Never,
            true,
        );
        m.put(
            "╅",
            "box drawing Left Down Heavy and Right Up Light",
            None,
            Never,
            true,
        );
        m.put(
            "╆",
            "box drawing Right Down Heavy and Left Up Light",
            None,
            Never,
            true,
        );
        m.put(
            "╇",
            "box drawing Down Light and Up Horizontal Heavy",
            None,
            Never,
            true,
        );
        m.put(
            "╈",
            "box drawing Up Light and Down Horizontal Heavy",
            None,
            Never,
            true,
        );
        m.put(
            "╉",
            "box drawing Right Light and Left Vertical Heavy",
            None,
            Never,
            true,
        );
        m.put(
            "╊",
            "box drawing Left Light and Right Vertical Heavy",
            None,
            Never,
            true,
        );
        m.put(
            "╋",
            "box drawing Heavy Vertical and Horizontal",
            None,
            Never,
            true,
        );
        m.put(
            "╌",
            "box drawing Light Double Dash Horizontal",
            None,
            Never,
            true,
        );
        m.put(
            "╍",
            "box drawing Heavy Double Dash Horizontal",
            None,
            Never,
            true,
        );
        m.put(
            "╎",
            "box drawing Light Double Dash Vertical",
            None,
            Never,
            true,
        );
        m.put(
            "╏",
            "box drawing Heavy Double Dash Vertical",
            None,
            Never,
            true,
        );
        m.put("═", "box drawing Double Horizontal", None, Never, true);
        m.put("║", "box drawing Double Vertical", None, Never, true);
        m.put(
            "╒",
            "box drawing Down Single and Right Double",
            None,
            Never,
            true,
        );
        m.put(
            "╓",
            "box drawing Down Double and Right Single",
            None,
            Never,
            true,
        );
        m.put("╔", "box drawing Double Down and Right", None, Never, true);
        m.put(
            "╕",
            "box drawing Down Single and Left Double",
            None,
            Never,
            true,
        );
        m.put(
            "╖",
            "box drawing Down Double and Left Single",
            None,
            Never,
            true,
        );
        m.put("╗", "box drawing Double Down and Left", None, Never, true);
        m.put(
            "╘",
            "box drawing Up Single and Right Double",
            None,
            Never,
            true,
        );
        m.put(
            "╙",
            "box drawing Up Double and Right Single",
            None,
            Never,
            true,
        );
        m.put("╚", "box drawing Double Up and Right", None, Never, true);
        m.put(
            "╛",
            "box drawing Up Single and Left Double",
            None,
            Never,
            true,
        );
        m.put(
            "╜",
            "box drawing Up Double and Left Single",
            None,
            Never,
            true,
        );
        m.put("╝", "box drawing Double Up and Left", None, Never, true);
        m.put(
            "╞",
            "box drawing Vertical Single and Right Double",
            None,
            Never,
            true,
        );
        m.put(
            "╟",
            "box drawing Vertical Double and Right Single",
            None,
            Never,
            true,
        );
        m.put(
            "╠",
            "box drawing Double Vertical and Right",
            None,
            Never,
            true,
        );
        m.put(
            "╡",
            "box drawing Vertical Single and Left Double",
            None,
            Never,
            true,
        );
        m.put(
            "╢",
            "box drawing Vertical Double and Left Single",
            None,
            Never,
            true,
        );
        m.put(
            "╣",
            "box drawing Double Vertical and Left",
            None,
            Never,
            true,
        );
        m.put(
            "╤",
            "box drawing Down Single and Horizontal Double",
            None,
            Never,
            true,
        );
        m.put(
            "╥",
            "box drawing Down Double and Horizontal Single",
            None,
            Never,
            true,
        );
        m.put(
            "╦",
            "box drawing Double Down and Horizontal",
            None,
            Never,
            true,
        );
        m.put(
            "╧",
            "box drawing Up Single and Horizontal Double",
            None,
            Never,
            true,
        );
        m.put(
            "╨",
            "box drawing Up Double and Horizontal Single",
            None,
            Never,
            true,
        );
        m.put(
            "╩",
            "box drawing Double Up and Horizontal",
            None,
            Never,
            true,
        );
        m.put(
            "╪",
            "box drawing Vertical Single and Horizontal Double",
            None,
            Never,
            true,
        );
        m.put(
            "╫",
            "box drawing Vertical Double and Horizontal Single",
            None,
            Never,
            true,
        );
        m.put(
            "╬",
            "box drawing Double Vertical and Horizontal",
            None,
            Never,
            true,
        );
        m.put(
            "╭",
            "box drawing Light Arc Down and Right",
            None,
            Never,
            true,
        );
        m.put(
            "╮",
            "box drawing Light Arc Down and Left",
            None,
            Never,
            true,
        );
        m.put("╯", "box drawing Light Arc Up and Left", None, Never, true);
        m.put("╰", "box drawing Light Arc Up and Right", None, Never, true);
        m.put(
            "╱",
            "box drawing Light Diagonal Upper Right to Lower Left",
            None,
            Never,
            true,
        );
        m.put(
            "╲",
            "box drawing Light Diagonal Upper Left to Lower Right",
            None,
            Never,
            true,
        );
        m.put("╳", "box drawing Light Diagonal Cross", None, Never, true);
        m.put("╴", "box drawing Light Left", None, Never, true);
        m.put("╵", "box drawing Light Up", None, Never, true);
        m.put("╶", "box drawing Light Right", None, Never, true);
        m.put("╷", "box drawing Light Down", None, Never, true);
        m.put("╸", "box drawing Heavy Left", None, Never, true);
        m.put("╹", "box drawing Heavy Up", None, Never, true);
        m.put("╺", "box drawing Heavy Right", None, Never, true);
        m.put("╻", "box drawing Heavy Down", None, Never, true);
        m.put(
            "╼",
            "box drawing Light Left and Heavy Right",
            None,
            Never,
            true,
        );
        m.put(
            "╽",
            "box drawing Light Up and Heavy Down",
            None,
            Never,
            true,
        );
        m.put(
            "╾",
            "box drawing Heavy Left and Light Right",
            None,
            Never,
            true,
        );
        m.put(
            "╿",
            "box drawing Heavy Up and Light Down",
            None,
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
        self.map.insert(
            symbol.into(),
            SymbolDesc::new(replacement.into(), level, include_original, repeat),
        );
    }

    pub fn get(&self, symbol: &str) -> Option<&SymbolDesc> {
        self.map.get(symbol)
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

pub enum IncludeOriginal {
    Never,
    Before,
    After,
}
