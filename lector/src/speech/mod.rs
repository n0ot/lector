use anyhow::Result;
use lazy_static::lazy_static;
use regex::Regex;
use std::fmt::Write;
use unicode_segmentation::UnicodeSegmentation;

pub mod symbols;
pub mod tdsr;

const MIN_REPEAT_COUNT: usize = 4;

pub trait Driver {
    fn speak(&mut self, text: &str, interrupt: bool) -> Result<()>;
    fn stop(&mut self) -> Result<()>;
    fn get_rate(&self) -> f32;
    fn set_rate(&mut self, rate: f32) -> Result<()>;
}

pub struct Speech {
    driver: Box<dyn Driver>,
    pub symbol_level: symbols::Level,
    symbols_map: symbols::SymbolMap,
}

impl Speech {
    pub fn new(driver: Box<dyn Driver>, symbol_level: symbols::Level) -> Speech {
        Speech {
            driver,
            symbol_level,
            symbols_map: symbols::SymbolMap::default_map(),
        }
    }

    pub fn speak(&mut self, text: &str, interrupt: bool) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }

        let mut processed = String::with_capacity(text.len());

        // If the text is a single character, increase the symbol level to Level::Character to
        // read the symbol no matter what.
        let text = if text.chars().all(char::is_whitespace) {
            text
        } else {
            text.trim()
        };
        let level = match text.chars().count() {
            1 => symbols::Level::Character,
            _ => self.symbol_level,
        };

        let mut prev_g: Option<&str> = None;
        let mut run_string = String::new();
        let mut run_count = 0;
        // Loop N+1 times, where N is the number of graphemes,
        // to compute the final run at the end.
        for g in UnicodeSegmentation::graphemes(text, true)
            .map(Some)
            .chain(std::iter::once(None))
        {
            if prev_g == None || prev_g == g {
                run_count += 1;
                prev_g = g;
                continue;
            }

            // the previous run has ended
            let mut collapse_repeated = run_count >= MIN_REPEAT_COUNT;
            run_string.clear();

            if let Some(symbol) = self.symbols_map.get(prev_g.unwrap()) {
                if level >= symbol.level {
                    match symbol.include_original {
                        symbols::IncludeOriginal::Before
                            if !processed.is_empty() && level != symbols::Level::Character =>
                        {
                            write!(
                                &mut run_string,
                                "{} {} ",
                                prev_g.unwrap(),
                                symbol.replacement
                            )?
                        }
                        symbols::IncludeOriginal::After if level != symbols::Level::Character => {
                            write!(
                                &mut run_string,
                                " {}{} ",
                                symbol.replacement,
                                prev_g.unwrap()
                            )?
                        }
                        _ => write!(&mut run_string, " {} ", symbol.replacement)?,
                    }
                } else {
                    // It doesn't make sense to collapse repeated symbols that aren't expanded
                    collapse_repeated = false;
                }
                if !symbol.repeat {
                    collapse_repeated = false;
                }
            }

            if run_string.is_empty() {
                if let Some(v) = emojis::get(prev_g.unwrap()) {
                    write!(&mut run_string, " {} ", v.name())?;
                }
            }

            if run_string.is_empty() {
                collapse_repeated = false; // Only collapse for symbols and emojis
                run_string.push_str(prev_g.unwrap());
            }

            if run_string
                .chars()
                .all(|c| c.is_whitespace() || c.is_numeric())
            {
                collapse_repeated = false;
            }

            if collapse_repeated {
                write!(&mut processed, " {} {} ", run_count, run_string)?;
            } else {
                for _ in 0..run_count {
                    processed.push_str(run_string.as_str());
                }
            }

            run_count = 1;
            prev_g = g;
        }

        // Break up mixed-case words
        lazy_static! {
            static ref RE_EXPAND_START_CAPS: Regex =
                Regex::new(r"(\p{Lowercase})(\p{Uppercase})").unwrap();
            static ref RE_EXPAND_END_CAPS: Regex =
                Regex::new(r"(\p{Uppercase})(\p{Uppercase}\p{Lowercase})").unwrap();
        }

        let processed = RE_EXPAND_START_CAPS.replace_all(&processed, "$1 $2");
        let processed = RE_EXPAND_END_CAPS.replace_all(&processed, "$1 $2");

        self.driver.speak(&processed, interrupt)
    }

    pub fn stop(&mut self) -> Result<()> {
        self.driver.stop()
    }

    #[allow(dead_code)]
    pub fn get_rate(&self) -> f32 {
        self.driver.get_rate()
    }

    pub fn set_rate(&mut self, rate: f32) -> Result<()> {
        self.driver.set_rate(rate)
    }
}
