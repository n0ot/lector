use std::{
    env,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    process::{Command, ExitCode},
};

fn main() -> ExitCode {
    match run(env::args_os().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Vec<OsString>) -> Result<(), String> {
    let Some((task, task_args)) = args.split_first() else {
        return Err(usage());
    };

    let root = workspace_root();
    match task.to_str() {
        Some("ghostty-bootstrap") => {
            validate_bootstrap_args(task_args)?;
            bootstrap_ghostty(&root, task_args)
        }
        Some("ghostty-check") => {
            reject_args(task_args, "ghostty-check")?;
            command(
                Command::new(root.join("scripts/check_ghostty_build.sh")).current_dir(&root),
                "Ghostty integration checks",
            )
        }
        Some("ghostty-bench") => {
            bootstrap_ghostty(
                &root,
                &[OsString::from("--optimize"), OsString::from("ReleaseFast")],
            )?;
            let mut cargo_args = vec![
                OsString::from("run"),
                OsString::from("--locked"),
                OsString::from("--release"),
                OsString::from("--features"),
                OsString::from("ghostty-vt"),
                OsString::from("--bin"),
                OsString::from("lector-ghostty-bench"),
                OsString::from("--"),
            ];
            cargo_args.extend_from_slice(task_args);
            cargo(&root, cargo_args)
        }
        Some("help" | "--help" | "-h") => {
            println!("{}", usage());
            Ok(())
        }
        Some(other) => Err(format!("unknown task {other:?}\n\n{}", usage())),
        None => Err(format!("task names must be valid UTF-8\n\n{}", usage())),
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask must live directly under the workspace root")
        .to_owned()
}

fn bootstrap_ghostty(root: &Path, args: &[OsString]) -> Result<(), String> {
    command(
        Command::new(root.join("scripts/bootstrap_ghostty.sh"))
            .args(args)
            .current_dir(root),
        "Ghostty bootstrap",
    )
}

fn cargo<I, S>(root: &Path, args: I) -> Result<(), String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let cargo = env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    command(
        Command::new(cargo).args(args).current_dir(root),
        "Cargo build",
    )
}

fn command(command: &mut Command, description: &str) -> Result<(), String> {
    let status = command
        .status()
        .map_err(|error| format!("could not start {description}: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{description} failed with {status}"))
    }
}

fn validate_bootstrap_args(args: &[OsString]) -> Result<(), String> {
    let mut index = 0;
    while index < args.len() {
        match args[index].to_str() {
            Some("--target" | "--optimize") if index + 1 < args.len() => index += 2,
            _ => {
                return Err(format!(
                    "ghostty-bootstrap accepts only --target VALUE and --optimize VALUE\n\n{}",
                    usage()
                ));
            }
        }
    }
    Ok(())
}

fn reject_args(args: &[OsString], task: &str) -> Result<(), String> {
    if args.is_empty() {
        Ok(())
    } else {
        Err(format!("{task} does not accept arguments\n\n{}", usage()))
    }
}

fn usage() -> String {
    "Repository maintenance tasks:\n  cargo ghostty-bootstrap [--target TARGET] [--optimize MODE]\n  cargo ghostty-check\n  cargo ghostty-bench [--self-test] [--output PATH] [--check-baseline PATH]"
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_accepts_supported_option_shapes() {
        assert!(validate_bootstrap_args(&[]).is_ok());
        assert!(
            validate_bootstrap_args(&[
                "--target".into(),
                "aarch64-unknown-linux-musl".into(),
                "--optimize".into(),
                "Debug".into(),
            ])
            .is_ok()
        );
    }

    #[test]
    fn bootstrap_rejects_incomplete_or_unknown_options() {
        assert!(validate_bootstrap_args(&["--target".into()]).is_err());
        assert!(validate_bootstrap_args(&["--release".into()]).is_err());
    }
}
