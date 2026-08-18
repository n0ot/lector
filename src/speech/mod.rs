use anyhow::Result as DriverResult;
use regex::Regex;
use std::{fmt::Write, sync::LazyLock};
use unicode_segmentation::UnicodeSegmentation;

pub mod proc_driver;
pub mod symbols;
pub mod tts;
pub mod worker;

const MIN_REPEAT_COUNT: usize = 4;
static EXPAND_START_CAPS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(\p{Lowercase})(\p{Uppercase})").expect("camel-case pattern must be valid")
});
static EXPAND_END_CAPS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(\p{Uppercase})(\p{Uppercase}\p{Lowercase})")
        .expect("capital boundary pattern must be valid")
});

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("speech driver: {0}")]
    Driver(#[source] anyhow::Error),
}

pub trait Driver {
    fn speak(&mut self, text: &str, interrupt: bool) -> DriverResult<()>;
    fn stop(&mut self) -> DriverResult<()>;
    fn get_rate(&self) -> f32;
    fn set_rate(&mut self, rate: f32) -> DriverResult<()>;
}

pub struct Speech {
    driver: Box<dyn Driver>,
    symbol_level: symbols::Level,
    symbols_map: symbols::SymbolMap,
    processed: String,
    run: String,
}

impl Speech {
    pub fn new(driver: Box<dyn Driver>) -> Speech {
        Speech {
            driver,
            symbol_level: symbols::Level::Some,
            symbols_map: symbols::SymbolMap::default_map(),
            processed: String::new(),
            run: String::new(),
        }
    }

    pub fn speak(&mut self, text: &str, interrupt: bool) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }

        let mut processed = std::mem::take(&mut self.processed);
        processed.clear();
        processed.reserve(text.len());

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
        let mut run_string = std::mem::take(&mut self.run);
        run_string.clear();
        let mut run_count = 0;
        // Loop N+1 times, where N is the number of graphemes,
        // to compute the final run at the end.
        for g in UnicodeSegmentation::graphemes(text, true)
            .map(Some)
            .chain(std::iter::once(None))
        {
            if prev_g.is_none() || prev_g == g {
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
                            )
                            .expect("writing to a String cannot fail")
                        }
                        symbols::IncludeOriginal::After if level != symbols::Level::Character => {
                            write!(
                                &mut run_string,
                                " {}{} ",
                                symbol.replacement,
                                prev_g.unwrap()
                            )
                            .expect("writing to a String cannot fail")
                        }
                        _ => write!(&mut run_string, " {} ", symbol.replacement)
                            .expect("writing to a String cannot fail"),
                    }
                } else {
                    // It doesn't make sense to collapse repeated symbols that aren't expanded
                    collapse_repeated = false;
                }
                if !symbol.repeat {
                    collapse_repeated = false;
                }
            }

            if run_string.is_empty()
                && let Some(v) = emojis::get(prev_g.unwrap())
            {
                write!(&mut run_string, " {} ", v.name()).expect("writing to a String cannot fail");
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
                write!(&mut processed, " {} {} ", run_count, run_string)
                    .expect("writing to a String cannot fail");
            } else {
                for _ in 0..run_count {
                    processed.push_str(run_string.as_str());
                }
            }

            run_count = 1;
            prev_g = g;
        }

        // Break up mixed-case words
        let result = {
            let expanded_start = EXPAND_START_CAPS.replace_all(&processed, "$1 $2");
            let expanded_end = EXPAND_END_CAPS.replace_all(&expanded_start, "$1 $2");
            self.driver
                .speak(expanded_end.as_ref(), interrupt)
                .map_err(Error::Driver)
        };
        self.processed = processed;
        self.run = run_string;
        result
    }

    pub fn stop(&mut self) -> Result<()> {
        self.driver.stop().map_err(Error::Driver)
    }

    pub fn get_rate(&self) -> f32 {
        self.driver.get_rate()
    }

    pub fn set_rate(&mut self, rate: f32) -> Result<()> {
        self.driver.set_rate(rate).map_err(Error::Driver)
    }

    pub fn symbol_level(&self) -> symbols::Level {
        self.symbol_level
    }

    pub fn set_symbol_level(&mut self, level: symbols::Level) {
        self.symbol_level = level;
    }

    pub fn cycle_symbol_level(&mut self) -> symbols::Level {
        use symbols::Level;
        self.symbol_level = match self.symbol_level {
            Level::None => Level::Some,
            Level::Some => Level::Most,
            Level::Most => Level::All,
            Level::All | Level::Character => Level::None,
        };
        self.symbol_level
    }

    pub fn symbol(&self, key: &str) -> Option<&symbols::SymbolDesc> {
        self.symbols_map.get(key)
    }

    pub fn set_symbol(
        &mut self,
        key: &str,
        replacement: &str,
        level: symbols::Level,
        include_original: symbols::IncludeOriginal,
        repeat: bool,
    ) {
        self.symbols_map
            .put(key, replacement, level, include_original, repeat);
    }

    pub fn remove_symbol(&mut self, key: &str) {
        self.symbols_map.remove(key);
    }

    pub fn clear_symbols(&mut self) {
        self.symbols_map.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::{Driver, Error, Speech, symbols};
    use std::{cell::RefCell, rc::Rc};

    struct RecordingDriver(Rc<RefCell<Vec<String>>>);

    impl Driver for RecordingDriver {
        fn speak(&mut self, text: &str, _interrupt: bool) -> anyhow::Result<()> {
            self.0.borrow_mut().push(text.to_string());
            Ok(())
        }

        fn stop(&mut self) -> anyhow::Result<()> {
            Ok(())
        }

        fn get_rate(&self) -> f32 {
            1.0
        }

        fn set_rate(&mut self, _rate: f32) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn recorder() -> (Speech, Rc<RefCell<Vec<String>>>) {
        let output = Rc::new(RefCell::new(Vec::new()));
        let speech = Speech::new(Box::new(RecordingDriver(Rc::clone(&output))));
        (speech, output)
    }

    #[test]
    fn normalizes_symbols_repetitions_casing_and_whitespace() {
        let (mut speech, output) = recorder();

        speech
            .speak("  camelCase HTTPServer ####  ", false)
            .unwrap();
        speech.speak(" ", false).unwrap();

        assert_eq!(
            output.borrow().as_slice(),
            ["camel Case HTTP Server  4  number  ", " space "]
        );
    }

    #[test]
    fn empty_input_does_not_reach_the_driver() {
        let (mut speech, output) = recorder();

        speech.speak("", false).unwrap();

        assert!(output.borrow().is_empty());
    }

    struct FailsOnceDriver {
        failed: bool,
        output: Rc<RefCell<Vec<String>>>,
    }

    impl Driver for FailsOnceDriver {
        fn speak(&mut self, text: &str, _interrupt: bool) -> anyhow::Result<()> {
            if !self.failed {
                self.failed = true;
                anyhow::bail!("expected failure");
            }
            self.output.borrow_mut().push(text.to_string());
            Ok(())
        }

        fn stop(&mut self) -> anyhow::Result<()> {
            Ok(())
        }

        fn get_rate(&self) -> f32 {
            1.0
        }

        fn set_rate(&mut self, _rate: f32) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn normalization_buffers_remain_usable_after_driver_error() {
        let output = Rc::new(RefCell::new(Vec::new()));
        let mut speech = Speech::new(Box::new(FailsOnceDriver {
            failed: false,
            output: Rc::clone(&output),
        }));

        assert!(speech.speak("firstValue", false).is_err());
        speech.speak("secondValue", false).unwrap();

        assert_eq!(output.borrow().as_slice(), ["second Value"]);
    }

    #[test]
    fn repetition_threshold_and_symbol_level_boundaries_are_exact() {
        let (mut speech, output) = recorder();

        speech.speak("###", false).unwrap();
        speech.speak("####", false).unwrap();
        speech.speak("1111", false).unwrap();
        speech.set_symbol_level(symbols::Level::None);
        speech.speak("####", false).unwrap();

        assert_eq!(
            output.borrow().as_slice(),
            [" number  number  number ", " 4  number  ", "1111", "####"]
        );
    }

    #[test]
    fn custom_symbols_support_original_placement_removal_and_clear() {
        let (mut speech, output) = recorder();
        speech.set_symbol(
            "!",
            "before",
            symbols::Level::Some,
            symbols::IncludeOriginal::Before,
            false,
        );
        speech.set_symbol(
            "?",
            "after",
            symbols::Level::Some,
            symbols::IncludeOriginal::After,
            false,
        );

        let before = speech.symbol("!").unwrap();
        assert_eq!(before.replacement, "before");
        assert!(before.level == symbols::Level::Some);
        assert_eq!(before.include_original.to_string(), "before");
        assert!(!before.repeat);

        speech.speak("a!", false).unwrap();
        speech.speak("?a", true).unwrap();
        speech.speak("!", false).unwrap();
        speech.remove_symbol("!");
        assert!(speech.symbol("!").is_none());
        speech.clear_symbols();
        assert!(speech.symbol("?").is_none());

        assert_eq!(
            output.borrow().as_slice(),
            ["a! before ", " after? a", " before "]
        );
    }

    #[derive(Default)]
    struct DriverState {
        speaks: Vec<(String, bool)>,
        stops: usize,
        rate: f32,
    }

    struct StatefulDriver(Rc<RefCell<DriverState>>);

    impl Driver for StatefulDriver {
        fn speak(&mut self, text: &str, interrupt: bool) -> anyhow::Result<()> {
            self.0
                .borrow_mut()
                .speaks
                .push((text.to_owned(), interrupt));
            Ok(())
        }

        fn stop(&mut self) -> anyhow::Result<()> {
            self.0.borrow_mut().stops += 1;
            Ok(())
        }

        fn get_rate(&self) -> f32 {
            self.0.borrow().rate
        }

        fn set_rate(&mut self, rate: f32) -> anyhow::Result<()> {
            self.0.borrow_mut().rate = rate;
            Ok(())
        }
    }

    #[test]
    fn control_operations_and_interrupt_flag_reach_the_driver() {
        let state = Rc::new(RefCell::new(DriverState {
            rate: 1.0,
            ..DriverState::default()
        }));
        let mut speech = Speech::new(Box::new(StatefulDriver(Rc::clone(&state))));

        assert_eq!(speech.get_rate(), 1.0);
        speech.set_rate(1.75).unwrap();
        speech.stop().unwrap();
        speech.speak("hello", true).unwrap();

        assert_eq!(speech.get_rate(), 1.75);
        assert_eq!(state.borrow().stops, 1);
        assert_eq!(state.borrow().speaks.as_slice(), [("hello".into(), true)]);

        speech.set_symbol_level(symbols::Level::Character);
        assert!(speech.cycle_symbol_level() == symbols::Level::None);
    }

    struct FailingControlDriver;

    impl Driver for FailingControlDriver {
        fn speak(&mut self, _text: &str, _interrupt: bool) -> anyhow::Result<()> {
            anyhow::bail!("speak failed")
        }

        fn stop(&mut self) -> anyhow::Result<()> {
            anyhow::bail!("stop failed")
        }

        fn get_rate(&self) -> f32 {
            1.0
        }

        fn set_rate(&mut self, _rate: f32) -> anyhow::Result<()> {
            anyhow::bail!("rate failed")
        }
    }

    #[test]
    fn every_driver_failure_is_wrapped_as_a_speech_error() {
        let mut speech = Speech::new(Box::new(FailingControlDriver));

        for error in [
            speech.speak("text", false).unwrap_err(),
            speech.stop().unwrap_err(),
            speech.set_rate(2.0).unwrap_err(),
        ] {
            assert!(matches!(error, Error::Driver(_)));
            assert!(error.to_string().starts_with("speech driver:"));
        }
    }
}
