#[allow(dead_code)]
#[path = "../build_support/ghostty.rs"]
mod ghostty;

#[test]
fn ghostty_dependency_and_source_revisions_are_exactly_pinned() {
    assert_eq!(
        ghostty::GHOSTTY_COMMIT,
        "43fe699071c7dceb161dc3b0c04fce46ade36174"
    );
    assert_eq!(
        ghostty::GHOSTTY_ARCHIVE_SHA256,
        "fbff942fc10b4d0a9de146e805922ef2b763226813fc449fdbb22c9ac7dd0f4a"
    );

    let manifest = include_str!("../Cargo.toml");
    assert!(manifest.contains("default = [\"ghostty-vt\"]"));
    assert!(manifest.contains("ghostty-vt = []"));
    assert!(manifest.contains("lector-ghostty = { path = \"crates/lector-ghostty\" }"));
    assert!(!manifest.contains("optional = true"));
    assert!(!include_str!("../build.rs").contains("CARGO_FEATURE_GHOSTTY_VT"));
    assert!(!manifest.contains("libghostty-vt ="));

    let lockfile = include_str!("../Cargo.lock");
    assert!(lockfile.contains("name = \"lector-ghostty\""));
    assert!(!lockfile.contains("name = \"libghostty-vt\""));
    assert!(!lockfile.contains("name = \"libghostty-vt-sys\""));
}

#[test]
fn cargo_build_is_network_free_and_source_bootstrap_is_explicit() {
    let wrapper_build = include_str!("../crates/lector-ghostty/build.rs");
    assert!(wrapper_build.contains("GHOSTTY_PREBUILT_ROOT"));
    assert!(wrapper_build.contains(ghostty::GHOSTTY_COMMIT));
    assert!(wrapper_build.contains(ghostty::REQUIRED_ZIG_VERSION));
    assert!(wrapper_build.contains("join(\"static-lib\")"));
    assert!(!wrapper_build.contains("Command::new(\"git\")"));
    assert!(!wrapper_build.contains("git clone"));
    assert!(!wrapper_build.contains("curl"));

    let bootstrap = include_str!("../scripts/bootstrap_ghostty.sh");
    assert!(bootstrap.contains(ghostty::GHOSTTY_COMMIT));
    assert!(bootstrap.contains(ghostty::GHOSTTY_ARCHIVE_SHA256));
    assert!(bootstrap.contains("shasum -a 256"));
    assert!(bootstrap.contains("build_info_probe.c"));
    assert!(bootstrap.contains("static_lib_dir=\"$prefix/static-lib\""));
}

#[test]
fn project_cargo_alias_bootstraps_a_verified_cached_zig_before_ghostty() {
    let cargo_config = include_str!("../.cargo/config.toml");
    assert!(cargo_config.contains("ghostty-release"));
    assert!(cargo_config.contains("ghostty-bench"));
    assert!(cargo_config.contains("--package lector-xtask"));

    let xtask = include_str!("../xtask/src/main.rs");
    assert!(xtask.contains("scripts/bootstrap_zig.sh"));
    assert!(xtask.contains("scripts/bootstrap_ghostty.sh"));
    assert!(xtask.contains("\"--release\""));
    assert!(xtask.contains("\"ghostty-vt\""));
    assert!(xtask.contains("lector-ghostty-bench"));

    let bootstrap = include_str!("../scripts/bootstrap_zig.sh");
    assert!(bootstrap.contains(ghostty::REQUIRED_ZIG_VERSION));
    assert!(bootstrap.contains("target/toolchains"));
    assert!(bootstrap.contains("shasum -a 256"));
    assert!(bootstrap.contains("ziglang.org/download"));
    for (platform, checksum) in [
        (
            "aarch64-macos",
            "b23d70deaa879b5c2d486ed3316f7eaa53e84acf6fc9cc747de152450d401489",
        ),
        (
            "x86_64-macos",
            "0387557ed1877bc6a2e1802c8391953baddba76081876301c522f52977b52ba7",
        ),
        (
            "aarch64-linux",
            "ea4b09bfb22ec6f6c6ceac57ab63efb6b46e17ab08d21f69f3a48b38e1534f17",
        ),
        (
            "x86_64-linux",
            "70e49664a74374b48b51e6f3fdfbf437f6395d42509050588bd49abe52ba3d00",
        ),
    ] {
        assert!(bootstrap.contains(platform));
        assert!(bootstrap.contains(checksum));
    }

    let gitignore = include_str!("../.gitignore");
    assert!(gitignore.lines().any(|line| line == "/target"));

    let ci = include_str!("../.github/workflows/ghostty-build.yml");
    assert!(!ci.contains("setup-zig"));
    assert!(ci.contains("cargo ghostty-check"));
    assert!(ci.contains("cargo ghostty-bootstrap"));
}

#[test]
fn owned_adapter_keeps_raw_ffi_private_and_auditable() {
    let manifest = include_str!("../crates/lector-ghostty/Cargo.toml");
    assert!(!manifest.contains("libghostty-vt"));
    assert!(!manifest.contains("libghostty-vt-sys"));
    assert!(manifest.contains("test = false"));
    assert!(manifest.contains("doctest = false"));

    let safe_boundary = include_str!("../crates/lector-ghostty/src/lib.rs");
    assert!(safe_boundary.contains("#![deny(unsafe_op_in_unsafe_fn)]"));
    assert!(safe_boundary.contains("mod ffi;"));
    assert!(!safe_boundary.contains("pub mod ffi;"));
    assert!(safe_boundary.contains("// SAFETY:"));

    let raw_ffi = include_str!("../crates/lector-ghostty/src/ffi.rs");
    assert!(raw_ffi.contains("pub(crate) fn ghostty_build_info"));
    assert!(!raw_ffi.contains("pub fn ghostty_build_info"));
    assert!(raw_ffi.contains("IO_ERROR: ResultCode = -5"));
    assert!(raw_ffi.contains("LIMIT_EXCEEDED: ResultCode = -6"));

    assert!(safe_boundary.contains("ffi::IO_ERROR => Err(Error::IoError)"));
    assert!(safe_boundary.contains("ffi::LIMIT_EXCEEDED => Err(Error::LimitExceeded)"));
}

#[test]
fn zig_policy_pins_an_exact_release_and_has_an_actionable_error() {
    assert_eq!(ghostty::REQUIRED_ZIG_VERSION, "0.16.0");
    let bootstrap = include_str!("../scripts/bootstrap_ghostty.sh");
    assert!(bootstrap.contains("actual_zig=$(zig version"));
    assert!(bootstrap.contains("Ghostty bootstrap requires Zig $REQUIRED_ZIG_VERSION on PATH"));
    assert!(bootstrap.contains("found ${actual_zig:-nothing}"));
}

#[test]
fn target_policy_matches_the_documented_tier_one_and_tier_two_matrix() {
    assert_eq!(
        ghostty::SUPPORTED_TARGETS,
        [
            "aarch64-apple-darwin",
            "x86_64-apple-darwin",
            "aarch64-unknown-linux-gnu",
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-musl",
            "x86_64-unknown-linux-musl",
        ]
    );
    for target in ghostty::SUPPORTED_TARGETS {
        assert!(ghostty::validate_target(target).is_ok());
    }
    let error = ghostty::validate_target("riscv64gc-unknown-linux-gnu").unwrap_err();
    assert!(error.contains("does not support target"));
    assert!(error.contains("x86_64-unknown-linux-musl"));
}

#[test]
fn ghostty_is_the_only_production_terminal_engine() {
    let manifest = include_str!("../Cargo.toml");
    assert!(!manifest.lines().any(|line| line.starts_with("vt100 =")));
    assert!(!manifest.lines().any(|line| line.starts_with("vte =")));
    assert!(manifest.contains("lector-ghostty = { path = \"crates/lector-ghostty\" }"));

    let terminal = include_str!("../src/terminal.rs");
    let view = include_str!("../src/view.rs");
    assert!(!terminal.contains("Vt100Engine"));
    assert!(!terminal.contains("vt100::"));
    assert!(!terminal.contains("compare_ghostty_shadow"));
    assert!(!view.contains("ghostty_shadow"));
    assert!(view.contains("engine: GhosttyEngine"));
}

#[test]
fn every_workspace_package_declares_its_license() {
    for (name, manifest) in [
        ("lector", include_str!("../Cargo.toml")),
        (
            "lector-ghostty",
            include_str!("../crates/lector-ghostty/Cargo.toml"),
        ),
        ("lector-xtask", include_str!("../xtask/Cargo.toml")),
    ] {
        assert!(
            manifest.lines().any(|line| line == "license = \"MIT\""),
            "{name} must declare its MIT license"
        );
    }
}
