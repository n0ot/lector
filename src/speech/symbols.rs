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
        m.put(" ", "space", Level::Character, false);
        m.put("!", "bang!", Level::All, true);
        m.put("¡", "inverted bang¡", Level::Some, true);
        m.put("\"", "quote", Level::Most, true);
        m.put("“", "left quote", Level::Most, true);
        m.put("”", "right quote", Level::Most, true);
        m.put("#", "number", Level::Some, true);
        m.put("$", "dollar", Level::All, false);
        m.put("¢", "cents", Level::All, false);
        m.put("¤", "currency", Level::All, false);
        m.put("£", "pound", Level::All, false);
        m.put("€", "euro", Level::All, false);
        m.put("¥", "yen", Level::All, false);
        m.put("%", "percent", Level::Some, true);
        m.put("&", "and", Level::Some, true);
        m.put("'", "tick", Level::Most, true);
        m.put("‘", "left tick", Level::Most, true);
        m.put("’", "right tick", Level::Most, true);
        m.put("(", "left paren(", Level::Most, true);
        m.put(")", ")right paren", Level::Most, true);
        m.put("*", "star", Level::Some, true);
        m.put("+", "plus", Level::Some, true);
        m.put(",", "comma,", Level::All, true);
        m.put("-", "dash-", Level::Most, true);
        m.put("–", "en dash–", Level::Most, true);
        m.put("—", "em dash—", Level::Most, true);
        m.put("­", "soft hyphen", Level::Most, true);
        m.put("⁃", "hyphen", Level::None, true);
        m.put(".", "dot.", Level::All, true);
        m.put("…", "dot dot dot…", Level::All, true);
        m.put("·", "middle dot", Level::Most, true);
        m.put("/", "slash", Level::Some, true);
        m.put(":", "colon:", Level::Most, true);
        m.put(";", "semi;", Level::Most, true);
        m.put("<", "less", Level::Some, true);
        m.put("=", "equals", Level::Some, true);
        m.put(">", "greater", Level::Some, true);
        m.put("?", "question?", Level::All, true);
        m.put("¿", "inverted question¿", Level::Some, true);
        m.put("@", "at", Level::Some, true);
        m.put("[", "left bracket", Level::Some, true);
        m.put("\\", "backslash", Level::Most, true);
        m.put("]", "right bracket", Level::Some, true);
        m.put("^", "carrat", Level::Most, true);
        m.put("_", "line", Level::Most, true);
        m.put("`", "graav", Level::Most, true);
        m.put("{", "left brace", Level::Some, true);
        m.put("|", "bar", Level::Most, true);
        m.put("¦", "broken bar", Level::Most, true);
        m.put("}", "right brace", Level::Some, true);
        m.put("~", "tilde", Level::Most, true);
        m.put("■", "black square", Level::Some, true);
        m.put("▪", "black small square", Level::Some, true);
        m.put("◾", "black medium small square", Level::Some, true);
        m.put("□", "white square", Level::Some, true);
        m.put("◦", "white bullet", Level::Some, true);
        m.put("➔", "right arrow", Level::Some, true);
        m.put("⇨", "right white arrow", Level::Some, true);
        m.put("●", "circle", Level::Most, true);
        m.put("○", "white circle", Level::Most, true);
        m.put("′", "prime", Level::None, true);
        m.put("″", "double prime", Level::None, true);
        m.put("‴", "tripple prime", Level::None, true);
        m.put("•", "bullet", Level::Some, true);
        m.put("§", "section", Level::Some, true);
        m.put("°", "degrees", Level::Some, true);
        m.put("µ", "micro", Level::Some, true);
        m.put("®", "registered", Level::Some, true);
        m.put("™", "trademark", Level::Some, true);
        m.put("©", "copyright", Level::Some, true);
        m.put("℠", "service mark", Level::Some, true);

        m
    }

    pub fn put(&mut self, symbol: &str, replacement: &str, level: Level, repeat: bool) {
        self.map.insert(
            symbol.into(),
            SymbolDesc::new(replacement.into(), level, repeat),
        );
    }

    pub fn get_level(&self, symbol: &str, level: Level) -> Option<&SymbolDesc> {
        match self.map.get(symbol) {
            Some(s) if level >= s.level => Some(s),
            _ => None,
        }
    }
}

/// Describes how a mapped symbol should be replaced
pub struct SymbolDesc {
    /// mapped symbols will be replaced with this string
    pub replacement: String,
    /// Replacement will take place at this symbol level or above
    level: Level,
    /// If true, repeated runs of symbols mapped to this SymbolDesc will be transformed to
    /// `<count> <replacement>`
    repeat: bool,
}

impl SymbolDesc {
    pub fn new(replacement: String, level: Level, repeat: bool) -> SymbolDesc {
        SymbolDesc {
            replacement,
            level,
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
