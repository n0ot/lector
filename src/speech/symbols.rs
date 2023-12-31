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
        let mut m = Self::new();
        m.put(" ", "space", Level::Character, IncludeOriginal::Never, false);
        m.put("!", "bang", Level::All, IncludeOriginal::After, true);
        m.put("¡", "inverted bang", Level::Some, IncludeOriginal::After, true);
        m.put("\"", "quote", Level::Most, IncludeOriginal::Never, true);
        m.put("“", "left quote", Level::Most, IncludeOriginal::Never, true);
        m.put("”", "right quote", Level::Most, IncludeOriginal::Never, true);
        m.put("#", "number", Level::Some, IncludeOriginal::Never, true);
        m.put("$", "dollar", Level::All, IncludeOriginal::Never, false);
        m.put("¢", "cents", Level::All, IncludeOriginal::Never, false);
        m.put("¤", "currency", Level::All, IncludeOriginal::Never, false);
        m.put("£", "pound", Level::All, IncludeOriginal::Never, false);
        m.put("€", "euro", Level::All, IncludeOriginal::Never, false);
        m.put("¥", "yen", Level::All, IncludeOriginal::Never, false);
        m.put("%", "percent", Level::Some, IncludeOriginal::Never, true);
        m.put("&", "and", Level::Some, IncludeOriginal::Never, true);
        m.put("'", "tick", Level::Most, IncludeOriginal::Never, true);
        m.put("‘", "left tick", Level::Most, IncludeOriginal::Never, true);
        m.put("’", "right tick", Level::Most, IncludeOriginal::Never, true);
        m.put("(", "left paren", Level::Most, IncludeOriginal::After, true);
        m.put(")", "right paren", Level::Most, IncludeOriginal::Before, true);
        m.put("*", "star", Level::Some, IncludeOriginal::Never, true);
        m.put("+", "plus", Level::Some, IncludeOriginal::Never, true);
        m.put(",", "comma", Level::All, IncludeOriginal::After, true);
        m.put("-", "dash", Level::Most, IncludeOriginal::After, true);
        m.put("–", "en dash", Level::Most, IncludeOriginal::After, true);
        m.put("—", "em dash", Level::Most, IncludeOriginal::After, true);
        m.put("­", "soft hyphen", Level::Most, IncludeOriginal::Never, true);
        m.put("⁃", "hyphen", Level::None, IncludeOriginal::Never, true);
        m.put(".", "dot", Level::All, IncludeOriginal::After, true);
        m.put("…", "dot dot dot", Level::All, IncludeOriginal::After, true);
        m.put("·", "middle dot", Level::Most, IncludeOriginal::Never, true);
        m.put("/", "slash", Level::Some, IncludeOriginal::Never, true);
        m.put(":", "colon", Level::Most, IncludeOriginal::After, true);
        m.put(";", "semi", Level::Most, IncludeOriginal::After, true);
        m.put("<", "less", Level::Some, IncludeOriginal::Never, true);
        m.put("=", "equals", Level::Some, IncludeOriginal::Never, true);
        m.put(">", "greater", Level::Some, IncludeOriginal::Never, true);
        m.put("?", "question", Level::All, IncludeOriginal::After, true);
        m.put("¿", "inverted question", Level::Some, IncludeOriginal::After, true);
        m.put("@", "at", Level::Some, IncludeOriginal::Never, true);
        m.put("[", "left bracket", Level::Some, IncludeOriginal::Never, true);
        m.put("\\", "backslash", Level::Most, IncludeOriginal::Never, true);
        m.put("]", "right bracket", Level::Some, IncludeOriginal::Never, true);
        m.put("^", "carrat", Level::Most, IncludeOriginal::Never, true);
        m.put("_", "line", Level::Most, IncludeOriginal::Never, true);
        m.put("`", "graav", Level::Most, IncludeOriginal::Never, true);
        m.put("{", "left brace", Level::Some, IncludeOriginal::Never, true);
        m.put("|", "bar", Level::Most, IncludeOriginal::Never, true);
        m.put("¦", "broken bar", Level::Most, IncludeOriginal::Never, true);
        m.put("}", "right brace", Level::Some, IncludeOriginal::Never, true);
        m.put("~", "tilde", Level::Most, IncludeOriginal::Never, true);
        m.put("■", "black square", Level::Some, IncludeOriginal::Never, true);
        m.put("▪", "black small square", Level::Some, IncludeOriginal::Never, true);
        m.put("◾", "black medium small square", Level::Some, IncludeOriginal::Never, true);
        m.put("□", "white square", Level::Some, IncludeOriginal::Never, true);
        m.put("◦", "white bullet", Level::Some, IncludeOriginal::Never, true);
        m.put("➔", "right arrow", Level::Some, IncludeOriginal::Never, true);
        m.put("⇨", "right white arrow", Level::Some, IncludeOriginal::Never, true);
        m.put("●", "circle", Level::Most, IncludeOriginal::Never, true);
        m.put("○", "white circle", Level::Most, IncludeOriginal::Never, true);
        m.put("′", "prime", Level::None, IncludeOriginal::Never, true);
        m.put("″", "double prime", Level::None, IncludeOriginal::Never, true);
        m.put("‴", "tripple prime", Level::None, IncludeOriginal::Never, true);
        m.put("•", "bullet", Level::Some, IncludeOriginal::Never, true);
        m.put("§", "section", Level::Some, IncludeOriginal::Never, true);
        m.put("°", "degrees", Level::Some, IncludeOriginal::Never, true);
        m.put("µ", "micro", Level::Some, IncludeOriginal::Never, true);
        m.put("®", "registered", Level::Some, IncludeOriginal::Never, true);
        m.put("™", "trademark", Level::Some, IncludeOriginal::Never, true);
        m.put("©", "copyright", Level::Some, IncludeOriginal::Never, true);
        m.put("℠", "service mark", Level::Some, IncludeOriginal::Never, true);

        m
    }

    pub fn put(&mut self, symbol: &str, replacement: &str, level: Level, include_original: IncludeOriginal, repeat: bool) {
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
    pub fn new(replacement: String, level: Level, include_original: IncludeOriginal, repeat: bool) -> SymbolDesc {
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
