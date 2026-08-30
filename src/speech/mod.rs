use anyhow::Result as DriverResult;
use regex::Regex;
use std::{fmt::Write, sync::LazyLock};
use unicode_segmentation::UnicodeSegmentation;

use protocol::UtteranceId;

pub mod proc_driver;
pub mod protocol;
pub mod supervisor;
pub mod symbols;
pub mod tts;
pub mod worker;

mod config;
pub mod manager;
pub use config::SpeechServerSpec;

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

/// Internal timing relationship between adjacent utterances. This is Lector
/// presentation metadata and never crosses the speech-host protocol boundary.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum UtteranceBoundary {
    #[default]
    Immediate,
    Paragraph,
}

pub trait Driver {
    fn speak(&mut self, text: &str, interrupt: bool) -> DriverResult<()>;

    /// Submit an utterance carrying Lector's session-wide logical identifier.
    /// In-process test and compatibility drivers may ignore the identifier.
    fn speak_utterance(
        &mut self,
        _id: &UtteranceId,
        text: &str,
        interrupt: bool,
    ) -> DriverResult<()> {
        self.speak(text, interrupt)
    }

    /// Submit an identified utterance with host-independent timing metadata.
    /// Compatibility drivers can ignore the boundary and retain the ordinary
    /// identified-submission behavior.
    fn speak_utterance_with_boundary(
        &mut self,
        id: &UtteranceId,
        text: &str,
        interrupt: bool,
        _boundary: UtteranceBoundary,
    ) -> DriverResult<()> {
        self.speak_utterance(id, text, interrupt)
    }

    fn stop(&mut self) -> DriverResult<()>;

    /// Suspend speech without discarding retained work. Compatibility drivers
    /// have no retained state and degrade to a one-way backend stop.
    fn pause(&mut self) -> DriverResult<()> {
        self.stop()
    }

    /// Resume explicitly suspended speech. Compatibility drivers which cannot
    /// retain speech have nothing to resume.
    fn resume(&mut self) -> DriverResult<()> {
        Ok(())
    }

    /// Toggle explicit suspension. Managed drivers override this so the
    /// current logical state, rather than backend audio timing, is decisive.
    fn toggle(&mut self) -> DriverResult<()> {
        self.pause()
    }

    /// Let an asynchronous backend consume lifecycle events while idle.
    fn poll(&mut self) -> DriverResult<()> {
        Ok(())
    }

    /// Whether separate non-interrupting submissions have an evidence-backed
    /// ordering mechanism. False is the conservative default: callers can
    /// still speak, but must keep one announcement in one utterance.
    fn supports_ordered_utterances(&self) -> bool {
        false
    }

    fn get_rate(&self) -> f32;
    fn set_rate(&mut self, rate: f32) -> DriverResult<()>;

    /// Finish starting a deferred backend.
    ///
    /// Ordinary drivers are already ready. Process-backed speech overrides
    /// this so Lua can select the exact server before any process or worker I/O
    /// is allowed to delay startup.
    fn start(&mut self) -> DriverResult<()> {
        Ok(())
    }

    /// Select or transactionally replace a process-backed speech server.
    fn configure_server(&mut self, _spec: SpeechServerSpec) -> DriverResult<()> {
        Err(anyhow::anyhow!(
            "this speech backend does not support server configuration"
        ))
    }

    /// Interrupt backend work during explicit lifecycle teardown.
    fn shutdown(&mut self) {}
}

pub struct Speech {
    driver: Box<dyn Driver>,
    symbol_level: symbols::Level,
    symbols_map: symbols::SymbolMap,
    processed: String,
    run: String,
    next_utterance_id: u64,
}

impl Speech {
    pub fn new(driver: Box<dyn Driver>) -> Speech {
        Speech {
            driver,
            symbol_level: symbols::Level::Some,
            symbols_map: symbols::SymbolMap::default_map(),
            processed: String::new(),
            run: String::new(),
            next_utterance_id: 1,
        }
    }

    pub fn speak(&mut self, text: &str, interrupt: bool) -> Result<UtteranceId> {
        let id = UtteranceId::new(self.next_utterance_id.to_string());
        self.next_utterance_id = self.next_utterance_id.wrapping_add(1);
        if text.is_empty() {
            return Ok(id);
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
            let chunks = if self.driver.supports_ordered_utterances() {
                split_paragraphs(expanded_end.as_ref())
            } else {
                let text = split_paragraphs(expanded_end.as_ref()).join(" ");
                (!text.is_empty()).then_some(text).into_iter().collect()
            };
            let multiple = chunks.len() > 1;
            let mut result = Ok(());
            for (index, chunk) in chunks.iter().enumerate() {
                let chunk_id = if multiple {
                    id.chunk(index)
                } else {
                    id.clone()
                };
                let boundary = if index == 0 {
                    UtteranceBoundary::Immediate
                } else {
                    UtteranceBoundary::Paragraph
                };
                if let Err(error) = self.driver.speak_utterance_with_boundary(
                    &chunk_id,
                    chunk,
                    interrupt && index == 0,
                    boundary,
                ) {
                    result = Err(Error::Driver(error));
                    break;
                }
            }
            result
        };
        self.processed = processed;
        self.run = run_string;
        result.map(|()| id)
    }

    pub fn cancel(&mut self) -> Result<()> {
        self.driver.stop().map_err(Error::Driver)
    }

    pub fn pause(&mut self) -> Result<()> {
        self.driver.pause().map_err(Error::Driver)
    }

    pub fn resume(&mut self) -> Result<()> {
        self.driver.resume().map_err(Error::Driver)
    }

    pub fn toggle(&mut self) -> Result<()> {
        self.driver.toggle().map_err(Error::Driver)
    }

    pub fn get_rate(&self) -> f32 {
        self.driver.get_rate()
    }

    pub fn set_rate(&mut self, rate: f32) -> Result<()> {
        self.driver.set_rate(rate).map_err(Error::Driver)
    }

    pub fn start(&mut self) -> Result<()> {
        self.driver.start().map_err(Error::Driver)
    }

    pub fn configure_server(&mut self, spec: SpeechServerSpec) -> Result<()> {
        self.driver.configure_server(spec).map_err(Error::Driver)
    }

    pub fn shutdown(&mut self) {
        self.driver.shutdown();
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

/// Collapse one physical line boundary to a space and split a logical
/// paragraph boundary into independently sequenced utterances. CRLF is one
/// boundary; lone CR is accepted for terminal-originated text.
fn split_paragraphs(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    if !text.contains(['\r', '\n']) {
        return vec![text.to_owned()];
    }

    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut chunks = Vec::new();
    let mut chunk = String::with_capacity(normalized.len());
    let mut newlines = 0usize;
    for character in normalized.chars() {
        if character == '\n' {
            newlines = newlines.saturating_add(1);
            continue;
        }
        if newlines == 1 {
            chunk.push(' ');
        } else if newlines >= 2 {
            let paragraph = chunk.trim().to_owned();
            if !paragraph.is_empty() {
                chunks.push(paragraph);
            }
            chunk.clear();
        }
        newlines = 0;
        chunk.push(character);
    }
    let paragraph = chunk.trim().to_owned();
    if !paragraph.is_empty() {
        chunks.push(paragraph);
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::{
        Driver, Error, Speech, UtteranceBoundary, protocol::UtteranceId, split_paragraphs, symbols,
    };
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

        fn supports_ordered_utterances(&self) -> bool {
            true
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
    fn one_newline_is_space_and_paragraph_boundaries_are_separate_chunks() {
        assert_eq!(split_paragraphs("hello\nworld"), ["hello world"]);
        assert_eq!(
            split_paragraphs("first\n\nsecond\r\n\r\nthird"),
            ["first", "second", "third"]
        );
        assert_eq!(split_paragraphs("one\rline"), ["one line"]);
        assert_eq!(split_paragraphs("a\0b"), ["a\0b"]);

        let (mut speech, output) = recorder();
        speech.speak("hello\nworld", false).unwrap();
        speech.speak("first\n\nsecond", false).unwrap();
        assert_eq!(
            output.borrow().as_slice(),
            ["hello world", "first", "second"]
        );
    }

    type IdentifiedCall = (String, String, bool, UtteranceBoundary);

    struct IdDriver(Rc<RefCell<Vec<IdentifiedCall>>>);

    impl Driver for IdDriver {
        fn speak(&mut self, _text: &str, _interrupt: bool) -> anyhow::Result<()> {
            unreachable!("Speech submits identified utterances")
        }

        fn speak_utterance_with_boundary(
            &mut self,
            id: &UtteranceId,
            text: &str,
            interrupt: bool,
            boundary: UtteranceBoundary,
        ) -> anyhow::Result<()> {
            self.0.borrow_mut().push((
                id.as_str().to_owned(),
                text.to_owned(),
                interrupt,
                boundary,
            ));
            Ok(())
        }

        fn stop(&mut self) -> anyhow::Result<()> {
            Ok(())
        }

        fn supports_ordered_utterances(&self) -> bool {
            true
        }

        fn get_rate(&self) -> f32 {
            1.0
        }

        fn set_rate(&mut self, _rate: f32) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn speak_returns_a_logical_id_and_chunks_have_stable_child_ids() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let mut speech = Speech::new(Box::new(IdDriver(Rc::clone(&calls))));

        let id = speech.speak("first\n\nsecond", true).unwrap();

        assert_eq!(id.as_str(), "1");
        assert_eq!(
            calls.borrow().as_slice(),
            [
                (
                    "1:0".to_owned(),
                    "first".to_owned(),
                    true,
                    UtteranceBoundary::Immediate,
                ),
                (
                    "1:1".to_owned(),
                    "second".to_owned(),
                    false,
                    UtteranceBoundary::Paragraph,
                ),
            ]
        );
    }

    #[test]
    fn host_without_terminal_evidence_receives_one_normalized_utterance() {
        struct Unsequenced(Rc<RefCell<Vec<String>>>);

        impl Driver for Unsequenced {
            fn speak(&mut self, text: &str, _interrupt: bool) -> anyhow::Result<()> {
                self.0.borrow_mut().push(text.to_owned());
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

        let calls = Rc::new(RefCell::new(Vec::new()));
        let mut speech = Speech::new(Box::new(Unsequenced(Rc::clone(&calls))));
        speech.speak("first\n\nsecond\nthird", false).unwrap();

        assert_eq!(calls.borrow().as_slice(), ["first second third"]);
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
        cancels: usize,
        pauses: usize,
        resumes: usize,
        toggles: usize,
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
            self.0.borrow_mut().cancels += 1;
            Ok(())
        }

        fn pause(&mut self) -> anyhow::Result<()> {
            self.0.borrow_mut().pauses += 1;
            Ok(())
        }

        fn resume(&mut self) -> anyhow::Result<()> {
            self.0.borrow_mut().resumes += 1;
            Ok(())
        }

        fn toggle(&mut self) -> anyhow::Result<()> {
            self.0.borrow_mut().toggles += 1;
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
        speech.cancel().unwrap();
        speech.pause().unwrap();
        speech.resume().unwrap();
        speech.toggle().unwrap();
        speech.speak("hello", true).unwrap();

        assert_eq!(speech.get_rate(), 1.75);
        assert_eq!(state.borrow().cancels, 1);
        assert_eq!(state.borrow().pauses, 1);
        assert_eq!(state.borrow().resumes, 1);
        assert_eq!(state.borrow().toggles, 1);
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
            anyhow::bail!("cancel failed")
        }

        fn pause(&mut self) -> anyhow::Result<()> {
            anyhow::bail!("pause failed")
        }

        fn resume(&mut self) -> anyhow::Result<()> {
            anyhow::bail!("resume failed")
        }

        fn toggle(&mut self) -> anyhow::Result<()> {
            anyhow::bail!("toggle failed")
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
            speech.cancel().unwrap_err(),
            speech.pause().unwrap_err(),
            speech.resume().unwrap_err(),
            speech.toggle().unwrap_err(),
            speech.set_rate(2.0).unwrap_err(),
        ] {
            assert!(matches!(error, Error::Driver(_)));
            assert!(error.to_string().starts_with("speech driver:"));
        }
    }
}
