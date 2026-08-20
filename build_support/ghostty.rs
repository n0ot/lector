pub const ADAPTER_VERSION: &str = "0.1.0";
pub const GHOSTTY_COMMIT: &str = "43fe699071c7dceb161dc3b0c04fce46ade36174";
pub const GHOSTTY_APP_VERSION: &str = "1.3.2-dev";
pub const REQUIRED_ZIG_VERSION: &str = "0.16.0";
pub const GHOSTTY_ARCHIVE_SHA256: &str =
    "fbff942fc10b4d0a9de146e805922ef2b763226813fc449fdbb22c9ac7dd0f4a";

pub const SUPPORTED_TARGETS: &[&str] = &[
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-musl",
    "x86_64-unknown-linux-musl",
    "aarch64-alpine-linux-musl",
    "x86_64-alpine-linux-musl",
];

pub fn validate_target(target: &str) -> Result<(), String> {
    if SUPPORTED_TARGETS.contains(&target) {
        Ok(())
    } else {
        Err(format!(
            "Lector's ghostty-vt feature does not support target {target}; supported targets: {}",
            SUPPORTED_TARGETS.join(", ")
        ))
    }
}
