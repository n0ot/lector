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

    let renderer_workloads = report["renderer_workloads"]
        .as_array()
        .expect("renderer workload array");
    assert!(!renderer_workloads.is_empty());
    for workload in renderer_workloads {
        for field in [
            "name",
            "iterations",
            "elapsed_ns",
            "latency_p50_ns",
            "latency_p95_ns",
            "latency_max_ns",
            "incremental_output_bytes",
            "full_output_bytes",
            "output_ratio",
            "cells_compared",
            "full_cells",
            "pure_diff_elapsed_ns",
            "pure_diff_latency_p95_ns",
            "pure_diff_output_bytes",
            "semantic_fast_path_iterations",
            "semantic_to_pure_diff_output_ratio",
            "semantic_to_pure_diff_latency_ratio",
        ] {
            assert!(
                !workload[field].is_null(),
                "missing renderer benchmark field {field}"
            );
        }
        assert!(
            workload["iterations"]
                .as_u64()
                .is_some_and(|value| value > 0)
        );
        assert!(
            workload["latency_p95_ns"]
                .as_u64()
                .is_some_and(|value| value > 0)
        );
        assert!(
            workload["full_output_bytes"]
                .as_u64()
                .is_some_and(|value| value > 0)
        );
        assert!(
            workload["pure_diff_output_bytes"]
                .as_u64()
                .is_some_and(|value| value > 0)
        );
        assert!(
            workload["semantic_fast_path_iterations"]
                .as_u64()
                .is_some_and(|value| value > 0)
        );
    }

    let scheduler_workloads = report["scheduler_workloads"]
        .as_array()
        .expect("scheduler workload array");
    assert!(!scheduler_workloads.is_empty());
    for workload in scheduler_workloads {
        for field in [
            "name",
            "iterations",
            "updates",
            "elapsed_ns",
            "latency_p50_ns",
            "latency_p95_ns",
            "latency_max_ns",
            "output_bytes",
            "maximum_pending_bytes",
            "replaced_renders",
            "blocked_writes",
            "completed_renders",
        ] {
            assert!(
                !workload[field].is_null(),
                "missing scheduler benchmark field {field}"
            );
        }
        assert!(
            workload["iterations"]
                .as_u64()
                .is_some_and(|value| value > 0)
        );
        assert!(
            workload["completed_renders"]
                .as_u64()
                .is_some_and(|value| value > 0)
        );
    }

    let media_workloads = report["media_workloads"]
        .as_array()
        .expect("media workload array");
    assert!(!media_workloads.is_empty());
    for workload in media_workloads {
        for field in [
            "name",
            "iterations",
            "decoded_image_bytes",
            "elapsed_ns",
            "throughput_mib_per_second",
            "allocations",
            "allocated_bytes",
            "latency_p50_ns",
            "latency_p95_ns",
            "latency_max_ns",
            "maximum_store_bytes",
            "maximum_scene_bytes",
            "output_bytes",
            "upload_transactions",
            "placement_transactions",
        ] {
            assert!(
                !workload[field].is_null(),
                "missing media benchmark field {field}"
            );
        }
        assert!(
            workload["decoded_image_bytes"]
                .as_u64()
                .is_some_and(|value| value > 0)
        );
        assert_eq!(workload["upload_transactions"], 1);
        assert!(
            workload["placement_transactions"]
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

    let renderer_workloads = baseline["renderer_workloads"]
        .as_array()
        .expect("renderer baseline workloads");
    assert_eq!(renderer_workloads.len(), 5);
    assert!(
        renderer_workloads
            .iter()
            .any(|workload| { workload["name"].as_str() == Some("tmux-like-structural-edits") })
    );
    for workload in renderer_workloads {
        for field in ["latency_p95_ns", "pure_diff_latency_p95_ns"] {
            assert!(
                workload["measured"][field]
                    .as_u64()
                    .is_some_and(|value| value > 0),
                "missing measured renderer latency {field}"
            );
        }
        for field in [
            "output_ratio",
            "semantic_to_pure_diff_output_ratio",
            "semantic_to_pure_diff_latency_ratio",
        ] {
            assert!(
                workload["measured"][field]
                    .as_f64()
                    .is_some_and(|value| value > 0.0),
                "missing measured renderer ratio {field}"
            );
        }
        assert!(
            workload["measured"]["semantic_fast_path_percent"]
                .as_f64()
                .is_some_and(|value| (0.0..=100.0).contains(&value))
        );
        assert!(
            workload["limits"]["maximum_latency_p95_ns"]
                .as_u64()
                .is_some_and(|value| value > 0)
        );
        assert!(
            workload["limits"]["maximum_output_ratio"]
                .as_f64()
                .is_some_and(|value| value > 0.0)
        );
        assert!(
            workload["limits"]["maximum_cells_compared_per_iteration"]
                .as_u64()
                .is_some_and(|value| value > 0)
        );
        assert!(
            workload["limits"]["minimum_semantic_fast_path_percent"]
                .as_f64()
                .is_some_and(|value| (0.0..=100.0).contains(&value))
        );
        assert!(
            workload["limits"]["maximum_semantic_to_pure_diff_output_ratio"]
                .as_f64()
                .is_some_and(|value| value > 0.0)
        );
        assert!(
            workload["limits"]["maximum_semantic_to_pure_diff_latency_ratio"]
                .as_f64()
                .is_some_and(|value| value > 0.0)
        );
    }

    let scheduler_workloads = baseline["scheduler_workloads"]
        .as_array()
        .expect("scheduler baseline workloads");
    assert_eq!(scheduler_workloads.len(), 2);
    for workload in scheduler_workloads {
        assert!(
            workload["measured"]["latency_p95_ns"]
                .as_u64()
                .is_some_and(|value| value > 0)
        );
        assert!(
            workload["measured"]["maximum_pending_bytes"]
                .as_u64()
                .is_some_and(|value| value > 0)
        );
        assert!(
            workload["measured"]["completed_render_percent"]
                .as_f64()
                .is_some_and(|value| value == 100.0)
        );
        for field in [
            "maximum_latency_p95_ns",
            "maximum_pending_bytes",
            "minimum_completed_render_percent",
        ] {
            assert!(
                workload["limits"][field]
                    .as_f64()
                    .is_some_and(|value| value > 0.0),
                "missing scheduler limit {field}"
            );
        }
        assert!(
            workload["limits"]["minimum_replaced_render_percent"]
                .as_f64()
                .is_some_and(|value| (0.0..=100.0).contains(&value))
        );
    }

    let media_workloads = baseline["media_workloads"]
        .as_array()
        .expect("media baseline workloads");
    assert_eq!(media_workloads.len(), 1);
    let media = &media_workloads[0];
    assert_eq!(media["name"], "kitty-media-recomposition");
    for field in [
        "throughput_mib_per_second",
        "latency_p95_ns",
        "allocated_bytes",
        "maximum_store_bytes",
        "maximum_scene_bytes",
        "upload_transactions",
    ] {
        assert!(
            media["measured"][field]
                .as_f64()
                .is_some_and(|value| value > 0.0),
            "missing measured media field {field}"
        );
    }
    for field in [
        "minimum_throughput_mib_per_second",
        "maximum_latency_p95_ns",
        "maximum_allocated_bytes",
        "maximum_store_bytes",
        "maximum_scene_bytes",
        "maximum_upload_transactions",
    ] {
        assert!(
            media["limits"][field]
                .as_f64()
                .is_some_and(|value| value > 0.0),
            "missing media limit {field}"
        );
    }
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
