use anyhow::{Context, Result};
use unicode_segmentation::UnicodeSegmentation;

pub mod drivers;
mod punctuation;

pub struct Speech {
    driver: Box<dyn drivers::Driver>,
}

impl Speech {
    pub fn speak(&mut self, text: &str, interrupt: bool) -> Result<()> {
        let text = describe_repeated_graphemes(text);

        // If the text is a single character, increase the punctuation level to Level::Character to
        // read the symbol no matter what.
        let punct_level = match text.chars().count() {
            1 => punctuation::Level::Character,
            _ => punctuation::Level::All,
        };

        let text = UnicodeSegmentation::graphemes(text.as_str(), true)
            .map(|s| {
                let result =
                    emojis::get(s).map_or_else(|| String::from(s), |v| format!(" {} ", v.name()));
                let result =
                    punctuation::get(s, punct_level).map_or(result, |v| format!(" {} ", v));
                result
            })
            .collect::<String>();
        self.driver.speak(&text, interrupt)
    }

    pub fn stop(&mut self) -> Result<()> {
        self.driver.stop()
    }

    pub fn get_rate(&self) -> f32 {
        self.driver.get_rate()
    }

    pub fn set_rate(&mut self, rate: f32) -> Result<()> {
        self.driver.set_rate(rate)
    }
}

pub fn new() -> Result<Speech> {
    let driver = Box::new(drivers::tdsr::Tdsr::new("./mac").context("create tdsr driver")?);

    Ok(Speech { driver })
}

/// If a grapheme g is repeated at least 4 times,
/// the entire run will be replaced with " n g ".
/// For example, "hello....world" will become "hello 4 . world".
fn describe_repeated_graphemes(s: &str) -> String {
    let n = 4;
    // We are comparing each grapheme to the one before it.
    // If they're not equal, we will reset the count to 1,
    // otherwise, we will increase it.
    // We are using Option here because there is no previous grapheme before the first one,
    // and because we need to iterate one time pass the end of the string to report the count of
    // the last grapheme run.
    UnicodeSegmentation::graphemes(s, true)
        .map(|s| Some(s))
        .chain(std::iter::once(None))
        .scan((0, None), |(count, prev_g), g| {
            let result = match g {
                Some(c) if Some(c) == *prev_g => {
                    *count += 1;
                    Some((0, None))
                }
                Some(_) => {
                    // This is a new grapheme
                    let result = (*count, *prev_g);
                    *count = 1;
                    Some(result)
                }
                None => Some((*count, *prev_g)), // This is the end of the string
            };
            *prev_g = g;
            result
        })
        // Only yield the last count/grapheme in the run,
        .filter_map(|(count, g)| g.map(|v| (count, v)))
        .map(|(count, g)| {
            if count >= n && !g.trim().is_empty() && !g.chars().any(char::is_alphanumeric) {
                // we want to describe this run in terms of how many times it was repeated.
                // adding spaces around it ensures it's read correctly.
                format!(" {} {} ", count, g)
            } else {
                // just reproduce the grapheme run as it was in the original string.
                String::from(g).repeat(count)
            }
        })
        .collect()
}
