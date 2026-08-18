#[allow(dead_code)]
#[path = "../build_support/ghostty.rs"]
mod ghostty;

use lector_ghostty::{OptimizeMode, build_info};

#[test]
fn linked_build_matches_the_pinned_source_and_feature_contract() {
    assert_eq!(
        env!("LECTOR_GHOSTTY_ADAPTER_VERSION"),
        ghostty::ADAPTER_VERSION
    );
    assert_eq!(env!("LECTOR_GHOSTTY_COMMIT"), ghostty::GHOSTTY_COMMIT);
    assert_eq!(
        env!("LECTOR_GHOSTTY_ZIG_VERSION"),
        ghostty::REQUIRED_ZIG_VERSION
    );

    assert!(build_info::supports_kitty_graphics().expect("query Kitty graphics build support"));
    assert_eq!(build_info::major_version().unwrap(), 0);
    assert_eq!(build_info::minor_version().unwrap(), 1);
    assert_eq!(build_info::patch_version().unwrap(), 0);
    assert_eq!(build_info::pre_version().unwrap(), "dev");
    // Ghostty gives the C library its own semantic version without source
    // metadata. The exact source revision is recorded separately above.
    assert_eq!(build_info::build_version().unwrap(), "");
    assert_eq!(build_info::version_string().unwrap(), "0.1.0-dev");
}

#[test]
fn linked_build_uses_the_cargo_profile_and_supported_architecture() {
    let configured_mode = std::env::var("LECTOR_GHOSTTY_OPTIMIZE").ok();
    let expected_mode = match configured_mode.as_deref().unwrap_or("ReleaseFast") {
        "Debug" => OptimizeMode::Debug,
        "ReleaseSafe" => OptimizeMode::ReleaseSafe,
        "ReleaseSmall" => OptimizeMode::ReleaseSmall,
        "ReleaseFast" => OptimizeMode::ReleaseFast,
        mode => panic!("unexpected linked Ghostty optimization mode {mode:?}"),
    };
    assert_eq!(build_info::optimize_mode().unwrap(), expected_mode);
    assert!(ghostty::validate_target(env!("LECTOR_GHOSTTY_TARGET")).is_ok());
    assert!(matches!(std::env::consts::ARCH, "aarch64" | "x86_64"));

    // The C ABI exposes optimize mode as a c_int. This catches an enum
    // representation drift before Lector begins storing wrapper handles.
    assert_eq!(std::mem::size_of::<OptimizeMode>(), 4);
}
