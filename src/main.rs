use anyhow::{Context, Result};
use lector::screen_reader;

fn main() -> Result<()> {
    screen_reader::ScreenReader::new()
        .context("create new screen reader instance")?
        .run()
}
