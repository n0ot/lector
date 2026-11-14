#[allow(dead_code)]
#[path = "build_support/ghostty.rs"]
mod ghostty;

use std::env;

fn main() {
    let target = env::var("TARGET").expect("Cargo must set TARGET for build scripts");
    ghostty::validate_target(&target).unwrap_or_else(|error| panic!("{error}"));
    println!("cargo:rerun-if-changed=build_support/eh_frames.c");
    if target.ends_with("-linux-musl") {
        println!("cargo:rerun-if-changed=scripts/zig-cc");
        cc::Build::new()
            .file("build_support/eh_frames.c")
            .warnings_into_errors(true)
            .compile("lector_eh_frames");
    }
    println!(
        "cargo:rustc-env=LECTOR_GHOSTTY_ADAPTER_VERSION={}",
        ghostty::ADAPTER_VERSION
    );
    println!(
        "cargo:rustc-env=LECTOR_GHOSTTY_COMMIT={}",
        ghostty::GHOSTTY_COMMIT
    );
    println!(
        "cargo:rustc-env=LECTOR_GHOSTTY_ZIG_VERSION={}",
        ghostty::REQUIRED_ZIG_VERSION
    );
    println!("cargo:rustc-env=LECTOR_GHOSTTY_TARGET={target}");
}
