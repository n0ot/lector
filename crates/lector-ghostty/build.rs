use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

const GHOSTTY_COMMIT: &str = "43fe699071c7dceb161dc3b0c04fce46ade36174";
const REQUIRED_ZIG_VERSION: &str = "0.16.0";

fn main() {
    println!("cargo:rerun-if-env-changed=GHOSTTY_PREBUILT_ROOT");
    println!("cargo:rerun-if-env-changed=LECTOR_GHOSTTY_OPTIMIZE");
    println!("cargo:rerun-if-env-changed=OPT_LEVEL");
    println!("cargo:rerun-if-env-changed=TARGET");

    let target = env::var("TARGET").expect("Cargo must set TARGET for build scripts");
    let optimize = optimize_mode();
    let root = prebuilt_root();
    let prefix = root.join(&target).join(optimize);
    let metadata_path = prefix.join("lector-ghostty-build.txt");
    let library_path = prefix.join("static-lib").join(static_library_name(&target));
    let repository_root = repository_root();

    for path in [
        repository_root.join("scripts/bootstrap_ghostty.sh"),
        repository_root.join("scripts/bootstrap_zig.sh"),
        repository_root.join("crates/lector-ghostty/abi/build_info_probe.c"),
    ] {
        println!("cargo:rerun-if-changed={}", path.display());
    }

    ensure_verified_archive(&repository_root, &target, optimize);

    let metadata = fs::read_to_string(&metadata_path).unwrap_or_else(|error| {
        panic!(
            "Ghostty bootstrap did not produce verified build metadata at {} ({error})",
            metadata_path.display()
        )
    });
    validate_metadata(&metadata, &target, optimize)
        .unwrap_or_else(|error| panic!("invalid Ghostty build metadata: {error}"));
    assert!(
        library_path.is_file(),
        "Ghostty bootstrap did not produce the static archive at {}",
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

fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn ensure_verified_archive(repository_root: &Path, target: &str, optimize: &str) {
    let script = repository_root.join("scripts/bootstrap_ghostty.sh");
    let status = Command::new(&script)
        .arg("--target")
        .arg(target)
        .arg("--optimize")
        .arg(optimize)
        .current_dir(repository_root)
        .status()
        .unwrap_or_else(|error| {
            panic!(
                "could not start the automatic Ghostty bootstrap at {}: {error}",
                script.display()
            )
        });
    assert!(
        status.success(),
        "automatic Ghostty bootstrap failed with {status}"
    );
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

fn validate_metadata(metadata: &str, target: &str, optimize: &str) -> Result<(), String> {
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
        if !metadata.lines().any(|candidate| candidate == line) {
            return Err(format!("missing {line:?}"));
        }
    }
    for key in ["abi_probe_sha256", "archive_sha256"] {
        if !metadata
            .lines()
            .any(|candidate| candidate.starts_with(&format!("{key}=")))
        {
            return Err(format!("missing {key:?}"));
        }
    }
    Ok(())
}
