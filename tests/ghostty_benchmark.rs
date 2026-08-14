#![cfg(feature = "ghostty-vt")]

use serde_json::Value;
use std::process::Command;

const BASELINE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/benchmarks/ghostty-release-baseline-macos-aarch64.json"
);

#[test]
fn benchmark_runner_emits_a_machine_readable_complete_report() {
    let output = Command::new(env!("CARGO_BIN_EXE_lector-ghostty-bench"))
        .arg("--self-test")
        .output()
        .expect("run Ghostty benchmark self-test");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).expect("parse benchmark JSON");

    assert_eq!(report["schema_version"], 1);
    assert!(
        report["ghostty_version"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
    );
    assert!(matches!(
        report["profile"].as_str(),
        Some("debug" | "release")
    ));
    let workloads = report["workloads"].as_array().expect("workload array");
    assert!(!workloads.is_empty());
    for workload in workloads {
        for field in [
            "name",
            "input_bytes",
            "iterations",
            "elapsed_ns",
            "throughput_mib_per_second",
            "allocations",
            "allocated_bytes",
            "peak_rss_bytes",
            "latency_p50_ns",
            "latency_p95_ns",
            "latency_max_ns",
            "scrollback_extent",
        ] {
            assert!(
                !workload[field].is_null(),
                "missing benchmark field {field}"
            );
        }
        assert!(
            workload["elapsed_ns"]
                .as_u64()
                .is_some_and(|value| value > 0)
        );
        assert!(
            workload["input_bytes"]
                .as_u64()
                .is_some_and(|value| value > 0)
        );
    }
}

#[test]
fn checked_in_release_baseline_has_regression_thresholds_for_every_workload() {
    let baseline: Value = serde_json::from_str(include_str!(
        "../benchmarks/ghostty-release-baseline-macos-aarch64.json"
    ))
    .expect("parse checked-in release baseline");
    assert_eq!(baseline["schema_version"], 1);
    assert_eq!(baseline["target"], "aarch64-apple-darwin");
    let workloads = baseline["workloads"]
        .as_array()
        .expect("baseline workloads");
    assert_eq!(workloads.len(), 3);
    for workload in workloads {
        assert!(
            workload["measured"]["throughput_mib_per_second"]
                .as_f64()
                .is_some_and(|value| value > 0.0)
        );
        assert!(
            workload["limits"]["minimum_throughput_mib_per_second"]
                .as_f64()
                .is_some_and(|value| value > 0.0)
        );
        assert!(
            workload["limits"]["maximum_latency_p95_ns"]
                .as_u64()
                .is_some_and(|value| value > 0)
        );
        assert!(
            workload["limits"]["maximum_allocations"]
                .as_u64()
                .is_some_and(|value| value > 0)
        );
        assert!(
            workload["limits"]["maximum_allocated_bytes"]
                .as_u64()
                .is_some_and(|value| value > 0)
        );
        assert!(
            workload["limits"]["maximum_peak_rss_bytes"]
                .as_u64()
                .is_some_and(|value| value > 0)
        );
    }
    let scrollback = workloads
        .iter()
        .find(|workload| workload["name"] == "bounded-scrollback")
        .expect("bounded scrollback workload");
    assert_eq!(
        scrollback["limits"]["minimum_scrollback_extent"], 10_000,
        "the release gate must enforce Lector's complete logical history window"
    );
}

#[test]
fn benchmark_runner_validates_the_checked_in_baseline_contract() {
    let output = Command::new(env!("CARGO_BIN_EXE_lector-ghostty-bench"))
        .args(["--validate-baseline", BASELINE])
        .output()
        .expect("validate Ghostty benchmark baseline");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
