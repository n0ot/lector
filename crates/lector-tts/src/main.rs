use anyhow::Result;
use clap::Parser;

#[derive(Parser)]
#[command(
    author,
    version,
    about = "Lector's standalone cross-platform speech host"
)]
struct Cli {
    #[command(flatten)]
    options: tts::host::Options,
}

fn main() -> Result<()> {
    tts::host::run_with_options(&Cli::parse().options)
}
