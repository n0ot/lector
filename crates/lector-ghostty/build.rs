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
    println!("cargo:rerun-if-env-changed=DOCS_RS");
    println!("cargo:rerun-if-env-changed=OPT_LEVEL");
    println!("cargo:rerun-if-env-changed=TARGET");

    // docs.rs cannot fetch or build the pinned native archive, and rustdoc
    // only needs the Rust declarations rather than a linked executable.
    if env::var_os("DOCS_RS").is_some() {
        return;
    }

    let target = env::var("TARGET").expect("Cargo must set TARGET for build scripts");
    let optimize = optimize_mode();
    let build_root = build_root();
    let root = prebuilt_root(&build_root);
    let prefix = root.join(&target).join(optimize);
    let metadata_path = prefix.join("lector-ghostty-build.txt");
    let library_path = prefix.join("static-lib").join(static_library_name(&target));
    let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let bootstrap_script = bootstrap_script(&crate_root);

    let mut watched_paths = vec![
        bootstrap_script.clone(),
        crate_root.join("bootstrap/bootstrap_zig.sh"),
        crate_root.join("bootstrap/lock.sh"),
        crate_root.join("abi/build_info_probe.c"),
    ];
    let workspace_root = workspace_root(&crate_root);
    if bootstrap_script.starts_with(&workspace_root) {
        watched_paths.push(workspace_root.join("scripts/bootstrap_zig.sh"));
        watched_paths.push(workspace_root.join("scripts/lib/lock.sh"));
    }
    for path in watched_paths {
        println!("cargo:rerun-if-changed={}", path.display());
    }

    ensure_verified_archive(&bootstrap_script, &build_root, &target, optimize);

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

fn workspace_root(crate_root: &Path) -> PathBuf {
    crate_root.join("../..")
}

fn bootstrap_script(crate_root: &Path) -> PathBuf {
    let workspace_script = workspace_root(crate_root).join("scripts/bootstrap_ghostty.sh");
    if workspace_script.is_file() {
        workspace_script
    } else {
        crate_root.join("bootstrap/bootstrap_ghostty.sh")
    }
}

fn ensure_verified_archive(script: &Path, build_root: &Path, target: &str, optimize: &str) {
    let status = Command::new("bash")
        .arg(script)
        .arg("--target")
        .arg(target)
        .arg("--optimize")
        .arg(optimize)
        .env("LECTOR_GHOSTTY_BUILD_ROOT", build_root)
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

fn build_root() -> PathBuf {
    if let Some(path) = env::var_os("LECTOR_GHOSTTY_BUILD_ROOT") {
        return PathBuf::from(path);
    }

    let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = workspace_root(&crate_root);
    if workspace_root
        .join("scripts/bootstrap_ghostty.sh")
        .is_file()
    {
        workspace_root.join("target")
    } else {
        PathBuf::from(env::var_os("OUT_DIR").expect("Cargo must set OUT_DIR")).join("bootstrap")
    }
}

fn prebuilt_root(build_root: &Path) -> PathBuf {
    if let Some(path) = env::var_os("GHOSTTY_PREBUILT_ROOT") {
        return PathBuf::from(path);
    }

    build_root.join("ghostty-prebuilt")
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
