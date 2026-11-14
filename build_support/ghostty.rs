pub const ADAPTER_VERSION: &str = "0.1.0";
pub const GHOSTTY_COMMIT: &str = "0fa3d577a5614150ff8e2c152de5fb9d5c6d2dec";
pub const GHOSTTY_VERSION: &str = "1.0.0";
pub const REQUIRED_ZIG_VERSION: &str = "0.17.0";
pub const GHOSTTY_ARCHIVE_SHA256: &str =
    "4a6ef758be721923e1969e7b81b443dcc92d8dd3fb5a39600c03a8817a753676";

pub const SUPPORTED_TARGETS: &[&str] = &[
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
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
            "Lector's Ghostty adapter does not support target {target}; Linux builds require musl (run scripts/setup once, then retry Cargo); supported targets: {}",
            SUPPORTED_TARGETS.join(", ")
        ))
    }
}
