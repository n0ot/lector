use anyhow::{Result, anyhow};
use lector::harness::{run_script_file, run_script_stdin};
use std::env;

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    let Some(path) = args.next() else {
        run_script_stdin()?;
        return Ok(());
    };
    if args.next().is_some() {
        return Err(anyhow!("usage: lector-harness [script.txt]"));
    }
    run_script_file(&path)
}
