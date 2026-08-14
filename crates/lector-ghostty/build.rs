use std::{env, fs, path::PathBuf};

const GHOSTTY_COMMIT: &str = "43fe699071c7dceb161dc3b0c04fce46ade36174";
const REQUIRED_ZIG_VERSION: &str = "0.16.0";

fn main() {
    println!("cargo:rerun-if-env-changed=GHOSTTY_PREBUILT_ROOT");
    println!("cargo:rerun-if-env-changed=LECTOR_GHOSTTY_OPTIMIZE");
    println!("cargo:rerun-if-env-changed=DEBUG");
    println!("cargo:rerun-if-env-changed=OPT_LEVEL");
    println!("cargo:rerun-if-env-changed=TARGET");

    let target = env::var("TARGET").expect("Cargo must set TARGET for build scripts");
    let optimize = optimize_mode();
    let root = prebuilt_root();
    let prefix = root.join(&target).join(optimize);
    let metadata_path = prefix.join("lector-ghostty-build.txt");
    let library_path = prefix.join("static-lib").join(static_library_name(&target));

    let metadata = fs::read_to_string(&metadata_path).unwrap_or_else(|error| {
        panic!(
            "missing verified Ghostty build metadata at {} ({error}); run cargo ghostty-bootstrap --target {target} --optimize {optimize}",
            metadata_path.display()
        )
    });
    validate_metadata(&metadata, &target, optimize);
    assert!(
        library_path.is_file(),
        "missing verified static Ghostty archive at {}; run cargo ghostty-bootstrap --target {target} --optimize {optimize}",
        library_path.display()
    );

    println!("cargo:rerun-if-changed={}", metadata_path.display());
    println!("cargo:rerun-if-changed={}", library_path.display());
    println!(
        "cargo:rustc-link-search=native={}",
        prefix.join("static-lib").display()
    );
    println!("cargo:rustc-link-lib=static=ghostty-vt");
}

fn prebuilt_root() -> PathBuf {
    if let Some(path) = env::var_os("GHOSTTY_PREBUILT_ROOT") {
        return PathBuf::from(path);
    }

    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("target/ghostty-prebuilt")
}

fn optimize_mode() -> &'static str {
    if let Ok(value) = env::var("LECTOR_GHOSTTY_OPTIMIZE") {
        return match value.as_str() {
            "Debug" => "Debug",
            "ReleaseSafe" => "ReleaseSafe",
            "ReleaseFast" => "ReleaseFast",
            "ReleaseSmall" => "ReleaseSmall",
            other => panic!(
                "LECTOR_GHOSTTY_OPTIMIZE must be Debug, ReleaseSafe, ReleaseFast, or ReleaseSmall (got {other:?})"
            ),
        };
    }
    if env::var("DEBUG").as_deref() == Ok("true") {
        return "Debug";
    }
    match env::var("OPT_LEVEL").as_deref() {
        Ok("s") | Ok("z") => "ReleaseSmall",
        _ => "ReleaseFast",
    }
}

fn static_library_name(target: &str) -> &'static str {
    if target.contains("windows") {
        "ghostty-vt-static.lib"
    } else {
        "libghostty-vt.a"
    }
}

fn validate_metadata(metadata: &str, target: &str, optimize: &str) {
    let expected = [
        ("ghostty_commit", GHOSTTY_COMMIT),
        ("zig_version", REQUIRED_ZIG_VERSION),
        ("target", target),
        ("optimize", optimize),
        ("app_runtime", "none"),
        ("emit_lib_vt", "true"),
        ("kitty_graphics", "true"),
        ("abi_header_check", "passed"),
    ];
    for (key, value) in expected {
        let line = format!("{key}={value}");
        assert!(
            metadata.lines().any(|candidate| candidate == line),
            "Ghostty build metadata does not contain {line:?}; rebuild it with cargo ghostty-bootstrap"
        );
    }
}
