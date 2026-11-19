mod ghostty_bench;
mod pty;
mod speech;
mod tmux;

use anyhow::{Result, anyhow, bail};
use lector::harness::{run_script_file, run_script_stdin};
use std::env;

fn main() -> Result<()> {
    let mut args = env::args_os().skip(1);
    // Lector supplies only the executable path when it launches this as its
    // shell. In that case the test selects the shell fixture by environment.
    let mode = args
        .next()
        .or_else(|| env::var_os("LECTOR_TEST_SHELL_FIXTURE"));
    match mode.as_deref().and_then(|mode| mode.to_str()) {
        Some("script") => match (args.next(), args.next()) {
            (None, _) => run_script_stdin(),
            (Some(path), None) => run_script_file(
                path.to_str()
                    .ok_or_else(|| anyhow!("script path must be UTF-8"))?,
            ),
            _ => bail!("usage: lector-harness script [FILE]"),
        },
        Some("speech") => speech::run(env::args().skip(2)),
        Some("ghostty-bench") => ghostty_bench::run(args).map_err(anyhow::Error::msg),
        Some("pty") => pty::run(),
        Some("tmux") => tmux::run(),
        None => bail!("usage: lector-harness <script|speech|ghostty-bench|pty|tmux>"),
        Some(mode) => bail!("unknown harness mode {mode:?}"),
    }
}
