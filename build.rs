#[allow(dead_code)]
#[path = "build_support/ghostty.rs"]
mod ghostty;

use std::env;

fn main() {
    let target = env::var("TARGET").expect("Cargo must set TARGET for build scripts");
    ghostty::validate_target(&target).unwrap_or_else(|error| panic!("{error}"));
    println!("cargo:rerun-if-changed=build_support/eh_frames.c");
    println!("cargo:rerun-if-env-changed=LECTOR_STATIC");
    println!("cargo:rustc-check-cfg=cfg(lector_static_musl)");
    if target.ends_with("-linux-musl") && env::var("LECTOR_STATIC").as_deref() == Ok("1") {
        println!("cargo:rustc-cfg=lector_static_musl");
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
