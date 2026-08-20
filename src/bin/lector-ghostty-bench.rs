use lector::{
    app::App,
    output_scheduler::{EnqueueOutcome, OutputScheduler, OutputSchedulerConfig},
    presentation::{
        CursorOwner, FullSceneVtRenderer, GridPoint, GridRect, IncrementalVtRenderer, MediaLimits,
        OutputTransaction, PaneMediaStore, PresentedScene, RenderBatch, RenderCapabilities,
        RenderStrategy, RendererBackend, Scene, SceneDamage, SceneSurface, SurfaceId,
    },
    screen_reader::ScreenReader,
    speech::{Driver, Speech},
    terminal::{GhosttyEngine, TerminalDamage, TerminalGeometry},
    views::{PtyView, ViewStack},
};
use serde::{Deserialize, Serialize};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    env, fs,
    hint::black_box,
    io::{self, Write},
    path::PathBuf,
    process::ExitCode,
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

struct CountingAllocator;

static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);

// SAFETY: this wrapper delegates every allocation operation to `System` with
// the original pointer and layout, adding only relaxed atomic accounting.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        // SAFETY: the caller supplied this layout under `GlobalAlloc`'s
        // contract and it is forwarded unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: the pointer and layout came from the corresponding System
        // allocation and are forwarded unchanged.
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        // SAFETY: the original allocation tuple and requested size are
        // forwarded unchanged to System.
        unsafe { System.realloc(pointer, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

#[derive(Serialize)]
struct BenchmarkReport {
    schema_version: u8,
    ghostty_version: String,
    profile: &'static str,
    workloads: Vec<WorkloadReport>,
    renderer_workloads: Vec<RendererWorkloadReport>,
    compositor_workloads: Vec<CompositorWorkloadReport>,
    scheduler_workloads: Vec<SchedulerWorkloadReport>,
    media_workloads: Vec<MediaWorkloadReport>,
}

#[derive(Serialize)]
struct CompositorWorkloadReport {
    name: &'static str,
    iterations: usize,
    input_bytes: u64,
    elapsed_ns: u64,
    throughput_mib_per_second: f64,
    allocations: u64,
    allocated_bytes: u64,
    latency_p50_ns: u64,
    latency_p95_ns: u64,
    latency_max_ns: u64,
    output_bytes: u64,
    completed_renders: usize,
}

#[derive(Serialize)]
struct MediaWorkloadReport {
    name: &'static str,
    iterations: usize,
    decoded_image_bytes: usize,
    elapsed_ns: u64,
    throughput_mib_per_second: f64,
    allocations: u64,
    allocated_bytes: u64,
    latency_p50_ns: u64,
    latency_p95_ns: u64,
    latency_max_ns: u64,
    maximum_store_bytes: usize,
    maximum_scene_bytes: usize,
    output_bytes: u64,
    upload_transactions: usize,
    placement_transactions: usize,
}

#[derive(Serialize)]
struct SchedulerWorkloadReport {
    name: &'static str,
    iterations: usize,
    updates: usize,
    elapsed_ns: u64,
    latency_p50_ns: u64,
    latency_p95_ns: u64,
    latency_max_ns: u64,
    output_bytes: u64,
    maximum_pending_bytes: usize,
    replaced_renders: usize,
    blocked_writes: usize,
    completed_renders: usize,
}

#[derive(Serialize)]
struct RendererWorkloadReport {
    name: &'static str,
    iterations: usize,
    elapsed_ns: u64,
    latency_p50_ns: u64,
    latency_p95_ns: u64,
    latency_max_ns: u64,
    incremental_output_bytes: u64,
    full_output_bytes: u64,
    output_ratio: f64,
    cells_compared: u64,
    full_cells: u64,
    pure_diff_elapsed_ns: u64,
    pure_diff_latency_p95_ns: u64,
    pure_diff_output_bytes: u64,
    semantic_fast_path_iterations: usize,
    semantic_to_pure_diff_output_ratio: f64,
    semantic_to_pure_diff_latency_ratio: f64,
}

#[derive(Serialize)]
struct WorkloadReport {
    name: &'static str,
    input_bytes: u64,
    iterations: usize,
    elapsed_ns: u64,
    throughput_mib_per_second: f64,
    allocations: u64,
    allocated_bytes: u64,
    peak_rss_bytes: u64,
    latency_p50_ns: u64,
    latency_p95_ns: u64,
    latency_max_ns: u64,
    scrollback_extent: usize,
}

struct Workload {
    name: &'static str,
    chunks: Vec<Vec<u8>>,
    iterations: usize,
    scrollback: usize,
}

#[derive(Deserialize)]
struct Baseline {
    schema_version: u8,
    target: String,
    workloads: Vec<BaselineWorkload>,
    renderer_workloads: Vec<RendererBaselineWorkload>,
    compositor_workloads: Vec<CompositorBaselineWorkload>,
    scheduler_workloads: Vec<SchedulerBaselineWorkload>,
    media_workloads: Vec<MediaBaselineWorkload>,
}

#[derive(Deserialize)]
struct CompositorBaselineWorkload {
    name: String,
    limits: CompositorBaselineLimits,
}

#[derive(Deserialize)]
struct CompositorBaselineLimits {
    minimum_throughput_mib_per_second: f64,
    maximum_latency_p95_ns: u64,
    maximum_allocations: u64,
    maximum_allocated_bytes: u64,
    maximum_output_bytes: u64,
    minimum_completed_render_percent: f64,
}

#[derive(Deserialize)]
struct MediaBaselineWorkload {
    name: String,
    limits: MediaBaselineLimits,
}

#[derive(Deserialize)]
struct MediaBaselineLimits {
    minimum_throughput_mib_per_second: f64,
    maximum_latency_p95_ns: u64,
    maximum_allocated_bytes: u64,
    maximum_store_bytes: usize,
    maximum_scene_bytes: usize,
    maximum_upload_transactions: usize,
}

#[derive(Deserialize)]
struct BaselineWorkload {
    name: String,
    limits: BaselineLimits,
}

#[derive(Deserialize)]
struct BaselineLimits {
    minimum_throughput_mib_per_second: f64,
    maximum_latency_p95_ns: u64,
    maximum_allocations: u64,
    maximum_allocated_bytes: u64,
    maximum_peak_rss_bytes: u64,
    minimum_scrollback_extent: usize,
}

#[derive(Deserialize)]
struct RendererBaselineWorkload {
    name: String,
    limits: RendererBaselineLimits,
}

#[derive(Deserialize)]
struct RendererBaselineLimits {
    maximum_latency_p95_ns: u64,
    maximum_output_ratio: f64,
    maximum_cells_compared_per_iteration: u64,
    minimum_semantic_fast_path_percent: f64,
    maximum_semantic_to_pure_diff_output_ratio: f64,
    maximum_semantic_to_pure_diff_latency_ratio: f64,
}

#[derive(Deserialize)]
struct SchedulerBaselineWorkload {
    name: String,
    limits: SchedulerBaselineLimits,
}

#[derive(Deserialize)]
struct SchedulerBaselineLimits {
    maximum_latency_p95_ns: u64,
    maximum_pending_bytes: usize,
    minimum_replaced_render_percent: f64,
    minimum_completed_render_percent: f64,
}

#[derive(Clone, Copy)]
enum RendererWorkloadKind {
    SingleCell,
    StatusLine,
    CursorMove,
    Scroll,
    TmuxLike,
    ZellijLike,
}

struct RendererWorkload {
    name: &'static str,
    kind: RendererWorkloadKind,
    iterations: usize,
}

#[derive(Clone, Copy)]
enum SchedulerWorkloadKind {
    EventBoundaryCoalescing,
    BackpressureRecovery,
}

struct SchedulerWorkload {
    name: &'static str,
    kind: SchedulerWorkloadKind,
    iterations: usize,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut self_test = false;
    let mut output = None;
    let mut check_baseline = None;
    let mut validate_baseline = None;
    let mut args = env::args_os().skip(1);
    while let Some(argument) = args.next() {
        if argument == "--self-test" {
            self_test = true;
        } else if argument == "--output" {
            output = Some(PathBuf::from(
                args.next().ok_or("--output requires a path")?,
            ));
        } else if argument == "--check-baseline" {
            check_baseline = Some(PathBuf::from(
                args.next().ok_or("--check-baseline requires a path")?,
            ));
        } else if argument == "--validate-baseline" {
            validate_baseline = Some(PathBuf::from(
                args.next().ok_or("--validate-baseline requires a path")?,
            ));
        } else {
            return Err(format!("unknown argument {argument:?}"));
        }
    }

    if let Some(path) = validate_baseline {
        load_baseline(&path)?;
        println!("valid Ghostty benchmark baseline: {}", path.display());
        return Ok(());
    }

    let workloads = if self_test {
        self_test_workloads()
    } else {
        baseline_workloads()
    };
    let report = BenchmarkReport {
        schema_version: 1,
        ghostty_version: lector_ghostty::build_info::version_string()
            .map_err(|error| error.to_string())?
            .to_owned(),
        profile: if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        workloads: workloads
            .into_iter()
            .map(run_workload)
            .collect::<Result<_, _>>()?,
        renderer_workloads: renderer_workloads(self_test)
            .into_iter()
            .map(run_renderer_workload)
            .collect::<Result<_, _>>()?,
        compositor_workloads: vec![run_compositor_workload(if self_test { 4 } else { 10_000 })?],
        scheduler_workloads: scheduler_workloads(self_test)
            .into_iter()
            .map(run_scheduler_workload)
            .collect::<Result<_, _>>()?,
        media_workloads: vec![run_media_workload(if self_test { 4 } else { 1_000 })?],
    };
    if let Some(path) = check_baseline {
        check_report(&report, &load_baseline(&path)?)?;
    }
    let json = serde_json::to_vec_pretty(&report).map_err(|error| error.to_string())?;
    if let Some(path) = output {
        fs::write(&path, &json)
            .map_err(|error| format!("write benchmark report {}: {error}", path.display()))?;
    } else {
        println!("{}", String::from_utf8(json).expect("JSON is UTF-8"));
    }
    Ok(())
}

fn load_baseline(path: &PathBuf) -> Result<Baseline, String> {
    let bytes = fs::read(path)
        .map_err(|error| format!("read benchmark baseline {}: {error}", path.display()))?;
    let baseline: Baseline = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse benchmark baseline {}: {error}", path.display()))?;
    if baseline.schema_version != 1 {
        return Err(format!(
            "unsupported benchmark baseline schema {}",
            baseline.schema_version
        ));
    }
    if baseline.target != target_triple() {
        return Err(format!(
            "benchmark baseline targets {}, current target is {}",
            baseline.target,
            target_triple()
        ));
    }
    if baseline.workloads.is_empty() {
        return Err("benchmark baseline contains no workloads".to_owned());
    }
    if baseline.renderer_workloads.is_empty() {
        return Err("benchmark baseline contains no renderer workloads".to_owned());
    }
    if baseline.compositor_workloads.is_empty() {
        return Err("benchmark baseline contains no compositor workloads".to_owned());
    }
    if baseline.scheduler_workloads.is_empty() {
        return Err("benchmark baseline contains no scheduler workloads".to_owned());
    }
    if baseline.media_workloads.is_empty() {
        return Err("benchmark baseline contains no media workloads".to_owned());
    }
    let mut names = std::collections::BTreeSet::new();
    for workload in &baseline.workloads {
        if !names.insert(&workload.name) {
            return Err(format!(
                "benchmark baseline repeats workload {:?}",
                workload.name
            ));
        }
        let limits = &workload.limits;
        if !(limits.minimum_throughput_mib_per_second > 0.0
            && limits.maximum_latency_p95_ns > 0
            && limits.maximum_allocations > 0
            && limits.maximum_allocated_bytes > 0
            && limits.maximum_peak_rss_bytes > 0)
        {
            return Err(format!(
                "benchmark baseline workload {:?} has an invalid zero limit",
                workload.name
            ));
        }
    }
    for workload in &baseline.renderer_workloads {
        if !names.insert(&workload.name) {
            return Err(format!(
                "benchmark baseline repeats workload {:?}",
                workload.name
            ));
        }
        let limits = &workload.limits;
        if limits.maximum_latency_p95_ns == 0
            || !limits.maximum_output_ratio.is_finite()
            || limits.maximum_output_ratio <= 0.0
            || limits.maximum_cells_compared_per_iteration == 0
            || !limits.minimum_semantic_fast_path_percent.is_finite()
            || limits.minimum_semantic_fast_path_percent < 0.0
            || limits.minimum_semantic_fast_path_percent > 100.0
            || !limits
                .maximum_semantic_to_pure_diff_output_ratio
                .is_finite()
            || limits.maximum_semantic_to_pure_diff_output_ratio <= 0.0
            || !limits
                .maximum_semantic_to_pure_diff_latency_ratio
                .is_finite()
            || limits.maximum_semantic_to_pure_diff_latency_ratio <= 0.0
        {
            return Err(format!(
                "benchmark baseline renderer workload {:?} has an invalid zero limit",
                workload.name
            ));
        }
    }
    for workload in &baseline.compositor_workloads {
        if !names.insert(&workload.name) {
            return Err(format!(
                "benchmark baseline repeats workload {:?}",
                workload.name
            ));
        }
        let limits = &workload.limits;
        if !limits.minimum_throughput_mib_per_second.is_finite()
            || limits.minimum_throughput_mib_per_second <= 0.0
            || limits.maximum_latency_p95_ns == 0
            || limits.maximum_allocations == 0
            || limits.maximum_allocated_bytes == 0
            || limits.maximum_output_bytes == 0
            || !limits.minimum_completed_render_percent.is_finite()
            || !(0.0..=100.0).contains(&limits.minimum_completed_render_percent)
            || limits.minimum_completed_render_percent == 0.0
        {
            return Err(format!(
                "benchmark baseline compositor workload {:?} has an invalid limit",
                workload.name
            ));
        }
    }
    for workload in &baseline.scheduler_workloads {
        if !names.insert(&workload.name) {
            return Err(format!(
                "benchmark baseline repeats workload {:?}",
                workload.name
            ));
        }
        let limits = &workload.limits;
        if limits.maximum_latency_p95_ns == 0
            || limits.maximum_pending_bytes == 0
            || !limits.minimum_replaced_render_percent.is_finite()
            || !(0.0..=100.0).contains(&limits.minimum_replaced_render_percent)
            || !limits.minimum_completed_render_percent.is_finite()
            || !(0.0..=100.0).contains(&limits.minimum_completed_render_percent)
            || limits.minimum_completed_render_percent == 0.0
        {
            return Err(format!(
                "benchmark baseline scheduler workload {:?} has an invalid limit",
                workload.name
            ));
        }
    }
    for workload in &baseline.media_workloads {
        if !names.insert(&workload.name) {
            return Err(format!(
                "benchmark baseline repeats workload {:?}",
                workload.name
            ));
        }
        let limits = &workload.limits;
        if !limits.minimum_throughput_mib_per_second.is_finite()
            || limits.minimum_throughput_mib_per_second <= 0.0
            || limits.maximum_latency_p95_ns == 0
            || limits.maximum_allocated_bytes == 0
            || limits.maximum_store_bytes == 0
            || limits.maximum_scene_bytes == 0
            || limits.maximum_upload_transactions == 0
        {
            return Err(format!(
                "benchmark baseline media workload {:?} has an invalid limit",
                workload.name
            ));
        }
    }
    Ok(baseline)
}

fn check_report(report: &BenchmarkReport, baseline: &Baseline) -> Result<(), String> {
    let mut failures = Vec::new();
    for expected in &baseline.workloads {
        let Some(actual) = report
            .workloads
            .iter()
            .find(|workload| workload.name == expected.name)
        else {
            failures.push(format!("missing workload {:?}", expected.name));
            continue;
        };
        let limits = &expected.limits;
        if actual.throughput_mib_per_second < limits.minimum_throughput_mib_per_second {
            failures.push(format!(
                "{} throughput {:.3} MiB/s is below {:.3} MiB/s",
                actual.name,
                actual.throughput_mib_per_second,
                limits.minimum_throughput_mib_per_second
            ));
        }
        if actual.latency_p95_ns > limits.maximum_latency_p95_ns {
            failures.push(format!(
                "{} p95 latency {} ns exceeds {} ns",
                actual.name, actual.latency_p95_ns, limits.maximum_latency_p95_ns
            ));
        }
        if actual.allocations > limits.maximum_allocations {
            failures.push(format!(
                "{} allocations {} exceed {}",
                actual.name, actual.allocations, limits.maximum_allocations
            ));
        }
        if actual.allocated_bytes > limits.maximum_allocated_bytes {
            failures.push(format!(
                "{} allocated bytes {} exceed {}",
                actual.name, actual.allocated_bytes, limits.maximum_allocated_bytes
            ));
        }
        if actual.peak_rss_bytes > limits.maximum_peak_rss_bytes {
            failures.push(format!(
                "{} peak RSS {} exceeds {} bytes",
                actual.name, actual.peak_rss_bytes, limits.maximum_peak_rss_bytes
            ));
        }
        if actual.scrollback_extent < limits.minimum_scrollback_extent {
            failures.push(format!(
                "{} scrollback extent {} is below {}",
                actual.name, actual.scrollback_extent, limits.minimum_scrollback_extent
            ));
        }
    }
    for expected in &baseline.renderer_workloads {
        let Some(actual) = report
            .renderer_workloads
            .iter()
            .find(|workload| workload.name == expected.name)
        else {
            failures.push(format!("missing renderer workload {:?}", expected.name));
            continue;
        };
        let limits = &expected.limits;
        if actual.latency_p95_ns > limits.maximum_latency_p95_ns {
            failures.push(format!(
                "{} renderer p95 latency {} ns exceeds {} ns",
                actual.name, actual.latency_p95_ns, limits.maximum_latency_p95_ns
            ));
        }
        if actual.output_ratio > limits.maximum_output_ratio {
            failures.push(format!(
                "{} renderer output ratio {:.4} exceeds {:.4}",
                actual.name, actual.output_ratio, limits.maximum_output_ratio
            ));
        }
        let cells_per_iteration = actual
            .cells_compared
            .checked_div(actual.iterations.try_into().unwrap_or(u64::MAX))
            .unwrap_or(u64::MAX);
        if cells_per_iteration > limits.maximum_cells_compared_per_iteration {
            failures.push(format!(
                "{} renderer compared {} cells/update, exceeding {}",
                actual.name, cells_per_iteration, limits.maximum_cells_compared_per_iteration
            ));
        }
        let semantic_percent =
            actual.semantic_fast_path_iterations as f64 * 100.0 / actual.iterations.max(1) as f64;
        if semantic_percent < limits.minimum_semantic_fast_path_percent {
            failures.push(format!(
                "{} semantic fast path {:.1}% is below {:.1}%",
                actual.name, semantic_percent, limits.minimum_semantic_fast_path_percent
            ));
        }
        if actual.semantic_to_pure_diff_output_ratio
            > limits.maximum_semantic_to_pure_diff_output_ratio
        {
            failures.push(format!(
                "{} semantic/pure-diff output ratio {:.4} exceeds {:.4}",
                actual.name,
                actual.semantic_to_pure_diff_output_ratio,
                limits.maximum_semantic_to_pure_diff_output_ratio
            ));
        }
        if actual.semantic_to_pure_diff_latency_ratio
            > limits.maximum_semantic_to_pure_diff_latency_ratio
        {
            failures.push(format!(
                "{} semantic/pure-diff p95 ratio {:.4} exceeds {:.4}",
                actual.name,
                actual.semantic_to_pure_diff_latency_ratio,
                limits.maximum_semantic_to_pure_diff_latency_ratio
            ));
        }
    }
    for expected in &baseline.compositor_workloads {
        let Some(actual) = report
            .compositor_workloads
            .iter()
            .find(|workload| workload.name == expected.name)
        else {
            failures.push(format!("missing compositor workload {:?}", expected.name));
            continue;
        };
        let limits = &expected.limits;
        if actual.throughput_mib_per_second < limits.minimum_throughput_mib_per_second {
            failures.push(format!(
                "{} compositor throughput {:.3} MiB/s is below {:.3} MiB/s",
                actual.name,
                actual.throughput_mib_per_second,
                limits.minimum_throughput_mib_per_second
            ));
        }
        if actual.latency_p95_ns > limits.maximum_latency_p95_ns {
            failures.push(format!(
                "{} compositor p95 latency {} ns exceeds {} ns",
                actual.name, actual.latency_p95_ns, limits.maximum_latency_p95_ns
            ));
        }
        if actual.allocations > limits.maximum_allocations {
            failures.push(format!(
                "{} compositor allocations {} exceed {}",
                actual.name, actual.allocations, limits.maximum_allocations
            ));
        }
        if actual.allocated_bytes > limits.maximum_allocated_bytes {
            failures.push(format!(
                "{} compositor allocated bytes {} exceed {}",
                actual.name, actual.allocated_bytes, limits.maximum_allocated_bytes
            ));
        }
        if actual.output_bytes > limits.maximum_output_bytes {
            failures.push(format!(
                "{} compositor output bytes {} exceed {}",
                actual.name, actual.output_bytes, limits.maximum_output_bytes
            ));
        }
        let completed_percent =
            actual.completed_renders as f64 * 100.0 / actual.iterations.max(1) as f64;
        if completed_percent < limits.minimum_completed_render_percent {
            failures.push(format!(
                "{} compositor completion {:.1}% is below {:.1}%",
                actual.name, completed_percent, limits.minimum_completed_render_percent
            ));
        }
    }
    for expected in &baseline.scheduler_workloads {
        let Some(actual) = report
            .scheduler_workloads
            .iter()
            .find(|workload| workload.name == expected.name)
        else {
            failures.push(format!("missing scheduler workload {:?}", expected.name));
            continue;
        };
        let limits = &expected.limits;
        if actual.latency_p95_ns > limits.maximum_latency_p95_ns {
            failures.push(format!(
                "{} scheduler p95 latency {} ns exceeds {} ns",
                actual.name, actual.latency_p95_ns, limits.maximum_latency_p95_ns
            ));
        }
        if actual.maximum_pending_bytes > limits.maximum_pending_bytes {
            failures.push(format!(
                "{} scheduler pending bytes {} exceed {}",
                actual.name, actual.maximum_pending_bytes, limits.maximum_pending_bytes
            ));
        }
        let replaced_percent =
            actual.replaced_renders as f64 * 100.0 / actual.updates.max(1) as f64;
        if replaced_percent < limits.minimum_replaced_render_percent {
            failures.push(format!(
                "{} scheduler replacement {:.1}% is below {:.1}%",
                actual.name, replaced_percent, limits.minimum_replaced_render_percent
            ));
        }
        let completed_percent =
            actual.completed_renders as f64 * 100.0 / actual.iterations.max(1) as f64;
        if completed_percent < limits.minimum_completed_render_percent {
            failures.push(format!(
                "{} scheduler completion {:.1}% is below {:.1}%",
                actual.name, completed_percent, limits.minimum_completed_render_percent
            ));
        }
    }
    for expected in &baseline.media_workloads {
        let Some(actual) = report
            .media_workloads
            .iter()
            .find(|workload| workload.name == expected.name)
        else {
            failures.push(format!("missing media workload {:?}", expected.name));
            continue;
        };
        let limits = &expected.limits;
        if actual.throughput_mib_per_second < limits.minimum_throughput_mib_per_second {
            failures.push(format!(
                "{} media throughput {:.3} MiB/s is below {:.3} MiB/s",
                actual.name,
                actual.throughput_mib_per_second,
                limits.minimum_throughput_mib_per_second
            ));
        }
        if actual.latency_p95_ns > limits.maximum_latency_p95_ns {
            failures.push(format!(
                "{} media p95 latency {} ns exceeds {} ns",
                actual.name, actual.latency_p95_ns, limits.maximum_latency_p95_ns
            ));
        }
        if actual.allocated_bytes > limits.maximum_allocated_bytes {
            failures.push(format!(
                "{} media allocated bytes {} exceed {}",
                actual.name, actual.allocated_bytes, limits.maximum_allocated_bytes
            ));
        }
        if actual.maximum_store_bytes > limits.maximum_store_bytes {
            failures.push(format!(
                "{} retained image bytes {} exceed {}",
                actual.name, actual.maximum_store_bytes, limits.maximum_store_bytes
            ));
        }
        if actual.maximum_scene_bytes > limits.maximum_scene_bytes {
            failures.push(format!(
                "{} scene image bytes {} exceed {}",
                actual.name, actual.maximum_scene_bytes, limits.maximum_scene_bytes
            ));
        }
        if actual.upload_transactions > limits.maximum_upload_transactions {
            failures.push(format!(
                "{} upload transactions {} exceed {}",
                actual.name, actual.upload_transactions, limits.maximum_upload_transactions
            ));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Ghostty benchmark regression:\n{}",
            failures.join("\n")
        ))
    }
}

fn target_triple() -> &'static str {
    if cfg!(all(target_arch = "aarch64", target_os = "macos")) {
        "aarch64-apple-darwin"
    } else if cfg!(all(target_arch = "x86_64", target_os = "macos")) {
        "x86_64-apple-darwin"
    } else if cfg!(all(target_arch = "aarch64", target_os = "linux")) {
        "aarch64-unknown-linux-gnu"
    } else if cfg!(all(target_arch = "x86_64", target_os = "linux")) {
        "x86_64-unknown-linux-gnu"
    } else {
        "unknown"
    }
}

fn run_workload(workload: Workload) -> Result<WorkloadReport, String> {
    let latency_capacity = workload.chunks.len().saturating_mul(workload.iterations);
    let mut latencies = Vec::with_capacity(latency_capacity);
    let mut engine = GhosttyEngine::new_with_scrollback(24, 80, workload.scrollback)
        .map_err(|error| error.to_string())?;
    let bytes_per_iteration = workload.chunks.iter().map(Vec::len).sum::<usize>();

    ALLOCATIONS.store(0, Ordering::Relaxed);
    ALLOCATED_BYTES.store(0, Ordering::Relaxed);
    let started = Instant::now();
    for iteration in 0..workload.iterations {
        if iteration != 0 {
            engine.reset().map_err(|error| error.to_string())?;
        }
        for chunk in &workload.chunks {
            let chunk_started = Instant::now();
            engine.advance(chunk).map_err(|error| error.to_string())?;
            latencies.push(nanos(chunk_started.elapsed()));
        }
    }
    let elapsed_ns = nanos(started.elapsed()).max(1);
    let allocations = ALLOCATIONS.load(Ordering::Relaxed);
    let allocated_bytes = ALLOCATED_BYTES.load(Ordering::Relaxed);
    let scrollback_extent = engine.scrollback_extent();
    black_box(engine.ghostty_snapshot());

    latencies.sort_unstable();
    let input_bytes = bytes_per_iteration
        .saturating_mul(workload.iterations)
        .try_into()
        .unwrap_or(u64::MAX);
    let seconds = elapsed_ns as f64 / 1_000_000_000.0;
    Ok(WorkloadReport {
        name: workload.name,
        input_bytes,
        iterations: workload.iterations,
        elapsed_ns,
        throughput_mib_per_second: input_bytes as f64 / (1024.0 * 1024.0) / seconds,
        allocations,
        allocated_bytes,
        peak_rss_bytes: peak_rss_bytes(),
        latency_p50_ns: percentile(&latencies, 50),
        latency_p95_ns: percentile(&latencies, 95),
        latency_max_ns: latencies.last().copied().unwrap_or(0),
        scrollback_extent,
    })
}

struct NoopSpeechDriver;

impl Driver for NoopSpeechDriver {
    fn speak(&mut self, _text: &str, _interrupt: bool) -> anyhow::Result<()> {
        Ok(())
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    fn get_rate(&self) -> f32 {
        1.0
    }

    fn set_rate(&mut self, _rate: f32) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Measures the production direct-output path from one already-read PTY chunk
/// through Ghostty mutation, scene composition, accessibility receipt capture,
/// scheduler enqueue, physical write, flush confirmation, and publication.
/// Unlike the renderer microbenchmarks, this deliberately includes every
/// allocation and clone owned by the compositor pipeline.
fn run_compositor_workload(iterations: usize) -> Result<CompositorWorkloadReport, String> {
    let geometry = TerminalGeometry::from_cells(24, 80);
    let stack = ViewStack::new(Box::new(PtyView::new(geometry.rows, geometry.cols)));
    let mut app = App::new(stack).map_err(|error| error.to_string())?;
    app.enable_output_scheduler(OutputSchedulerConfig::default());
    let mut screen_reader = ScreenReader::new(Speech::new(Box::new(NoopSpeechDriver)));
    let mut writer = BenchmarkWriter::default();

    let mut initial = Vec::new();
    for row in 1..=geometry.rows {
        initial.extend_from_slice(format!("\x1b[{row};1Hbaseline row {row:02}").as_bytes());
    }
    app.handle_pty(&mut screen_reader, &initial, &mut writer)
        .map_err(|error| error.to_string())?;
    let initial_report = app
        .drain_scheduled_output(&mut writer, true)
        .map_err(|error| error.to_string())?;
    if initial_report.completed_renders.len() != 1 {
        return Err("initial compositor render did not complete".to_owned());
    }

    let updates = (0..iterations)
        .map(|iteration| {
            format!(
                "\x1b[{};{}H{}",
                iteration % usize::from(geometry.rows) + 1,
                iteration % usize::from(geometry.cols - 1) + 1,
                char::from(b'a' + (iteration % 26) as u8),
            )
            .into_bytes()
        })
        .collect::<Vec<_>>();
    let input_bytes = updates
        .iter()
        .map(Vec::len)
        .sum::<usize>()
        .try_into()
        .unwrap_or(u64::MAX);
    let mut latencies = Vec::with_capacity(iterations);
    let output_before = writer.bytes;
    let mut completed_renders = 0usize;

    ALLOCATIONS.store(0, Ordering::Relaxed);
    ALLOCATED_BYTES.store(0, Ordering::Relaxed);
    let started = Instant::now();
    for update in &updates {
        let iteration_started = Instant::now();
        app.handle_pty(&mut screen_reader, update, &mut writer)
            .map_err(|error| error.to_string())?;
        let report = app
            .drain_scheduled_output(&mut writer, true)
            .map_err(|error| error.to_string())?;
        completed_renders = completed_renders.saturating_add(report.completed_renders.len());
        latencies.push(nanos(iteration_started.elapsed()).max(1));
    }
    let elapsed_ns = nanos(started.elapsed()).max(1);
    let allocations = ALLOCATIONS.load(Ordering::Relaxed);
    let allocated_bytes = ALLOCATED_BYTES.load(Ordering::Relaxed);
    latencies.sort_unstable();
    let seconds = elapsed_ns as f64 / 1_000_000_000.0;

    Ok(CompositorWorkloadReport {
        name: "direct-compositor-pipeline",
        iterations,
        input_bytes,
        elapsed_ns,
        throughput_mib_per_second: input_bytes as f64 / (1024.0 * 1024.0) / seconds,
        allocations,
        allocated_bytes,
        latency_p50_ns: percentile(&latencies, 50),
        latency_p95_ns: percentile(&latencies, 95),
        latency_max_ns: latencies.last().copied().unwrap_or(0),
        output_bytes: writer.bytes.saturating_sub(output_before),
        completed_renders,
    })
}

fn run_renderer_workload(workload: RendererWorkload) -> Result<RendererWorkloadReport, String> {
    const ROOT: SurfaceId = SurfaceId(1);
    let geometry = TerminalGeometry::from_cells(24, 80);
    let mut source =
        GhosttyEngine::new(geometry.rows, geometry.cols).map_err(|error| error.to_string())?;
    let mut initial = Vec::new();
    for row in 1..=geometry.rows {
        initial.extend_from_slice(format!("\x1b[{row};1Hbaseline row {row:02}").as_bytes());
    }
    source
        .advance(&initial)
        .map_err(|error| error.to_string())?;
    let mut scene = renderer_scene(&source, ROOT);
    let mut presented = PresentedScene::blank(geometry);
    let capabilities = RenderCapabilities::default();
    let mut renderer = IncrementalVtRenderer::new(capabilities);
    let initial_batch = renderer
        .render(&scene, &SceneDamage::Full, &presented)
        .map_err(|error| error.to_string())?;
    renderer.confirm(&initial_batch.predicted);
    presented = initial_batch.predicted;
    let mut pure_diff_renderer = IncrementalVtRenderer::new(capabilities);
    let pure_diff_initial = pure_diff_renderer
        .render(&scene, &SceneDamage::Full, &PresentedScene::blank(geometry))
        .map_err(|error| error.to_string())?;
    pure_diff_renderer.confirm(&pure_diff_initial.predicted);
    let mut pure_diff_presented = pure_diff_initial.predicted;

    let mut latencies = Vec::with_capacity(workload.iterations);
    let mut pure_diff_latencies = Vec::with_capacity(workload.iterations);
    let mut incremental_output_bytes = 0u64;
    let mut pure_diff_output_bytes = 0u64;
    let mut full_output_bytes = 0u64;
    let mut cells_compared = 0u64;
    let mut semantic_fast_path_iterations = 0usize;
    for iteration in 0..workload.iterations {
        let bytes = renderer_update(workload.kind, iteration, geometry);
        let update = source.advance(&bytes).map_err(|error| error.to_string())?;
        scene = renderer_scene(&source, ROOT);
        let damage = SceneDamage::from_terminal_update(&scene.panes[0], &update, scene.geometry);

        let started = Instant::now();
        let batch = renderer
            .render(&scene, &damage, &presented)
            .map_err(|error| error.to_string())?;
        let latency = nanos(started.elapsed()).max(1);
        latencies.push(latency);
        black_box(&batch);
        let stats = renderer.last_stats();
        if renderer.last_strategy() == RenderStrategy::SemanticFastPath {
            semantic_fast_path_iterations = semantic_fast_path_iterations.saturating_add(1);
        }
        cells_compared = cells_compared.saturating_add(stats.cells_compared as u64);
        incremental_output_bytes = incremental_output_bytes.saturating_add(
            batch
                .transactions
                .iter()
                .map(|transaction| transaction.bytes.len() as u64)
                .sum(),
        );

        let pure_damage = match &update.damage {
            TerminalDamage::Full => SceneDamage::regions([GridRect::new(
                GridPoint::new(0, 0),
                geometry.rows,
                geometry.cols,
            )]),
            damage => SceneDamage::from_terminal_damage(&scene.panes[0], damage, scene.geometry),
        };
        let pure_started = Instant::now();
        let pure_batch = pure_diff_renderer
            .render(&scene, &pure_damage, &pure_diff_presented)
            .map_err(|error| error.to_string())?;
        pure_diff_latencies.push(nanos(pure_started.elapsed()).max(1));
        pure_diff_output_bytes = pure_diff_output_bytes.saturating_add(
            pure_batch
                .transactions
                .iter()
                .map(|transaction| transaction.bytes.len() as u64)
                .sum(),
        );

        let mut full = FullSceneVtRenderer::new(capabilities);
        let full_batch = full
            .render(&scene, &SceneDamage::Full, &presented)
            .map_err(|error| error.to_string())?;
        full_output_bytes = full_output_bytes.saturating_add(
            full_batch
                .transactions
                .iter()
                .map(|transaction| transaction.bytes.len() as u64)
                .sum(),
        );
        renderer.confirm(&batch.predicted);
        presented = batch.predicted;
        pure_diff_renderer.confirm(&pure_batch.predicted);
        pure_diff_presented = pure_batch.predicted;
    }
    latencies.sort_unstable();
    pure_diff_latencies.sort_unstable();
    let elapsed_ns = latencies.iter().copied().fold(0u64, u64::saturating_add);
    let pure_diff_elapsed_ns = pure_diff_latencies
        .iter()
        .copied()
        .fold(0u64, u64::saturating_add);
    let latency_p95_ns = percentile(&latencies, 95);
    let pure_diff_latency_p95_ns = percentile(&pure_diff_latencies, 95);
    let full_cells = u64::from(geometry.rows)
        .saturating_mul(u64::from(geometry.cols))
        .saturating_mul(workload.iterations.try_into().unwrap_or(u64::MAX));
    Ok(RendererWorkloadReport {
        name: workload.name,
        iterations: workload.iterations,
        elapsed_ns,
        latency_p50_ns: percentile(&latencies, 50),
        latency_p95_ns,
        latency_max_ns: latencies.last().copied().unwrap_or(0),
        incremental_output_bytes,
        full_output_bytes,
        output_ratio: if full_output_bytes == 0 {
            0.0
        } else {
            incremental_output_bytes as f64 / full_output_bytes as f64
        },
        cells_compared,
        full_cells,
        pure_diff_elapsed_ns,
        pure_diff_latency_p95_ns,
        pure_diff_output_bytes,
        semantic_fast_path_iterations,
        semantic_to_pure_diff_output_ratio: ratio(incremental_output_bytes, pure_diff_output_bytes),
        semantic_to_pure_diff_latency_ratio: ratio(latency_p95_ns, pure_diff_latency_p95_ns),
    })
}

fn run_media_workload(iterations: usize) -> Result<MediaWorkloadReport, String> {
    const ROOT: SurfaceId = SurfaceId(1);
    const PIXEL_WIDTH: usize = 64;
    const PIXEL_HEIGHT: usize = 64;
    let geometry = TerminalGeometry::new(24, 80, 10, 20);
    let pixels = (0..PIXEL_WIDTH * PIXEL_HEIGHT)
        .flat_map(|index| {
            let value = u8::try_from(index % 251).expect("bounded media byte");
            [value, value.wrapping_add(17), value.wrapping_add(31), 255]
        })
        .collect::<Vec<_>>();
    let command = format!(
        "\x1b_Ga=T,f=32,s={PIXEL_WIDTH},v={PIXEL_HEIGHT},i=17,p=23,c=8,r=4,q=2;{}\x1b\\",
        encode_base64(&pixels)
    );
    let mut source =
        GhosttyEngine::new(geometry.rows, geometry.cols).map_err(|error| error.to_string())?;
    source
        .resize_with_geometry(geometry)
        .map_err(|error| error.to_string())?;
    source
        .advance(command.as_bytes())
        .map_err(|error| error.to_string())?;
    let mut store = PaneMediaStore::new(MediaLimits::default());
    let initial_scene = media_scene(&source, &mut store, ROOT)?;
    let mut renderer = IncrementalVtRenderer::new(RenderCapabilities {
        kitty_graphics: true,
        ..RenderCapabilities::default()
    });
    let initial = renderer
        .render(
            &initial_scene,
            &SceneDamage::Full,
            &PresentedScene::blank(geometry),
        )
        .map_err(|error| error.to_string())?;
    renderer.confirm(&initial.predicted);
    let mut output_bytes = render_batch_bytes(&initial);
    let mut upload_transactions = count_batch_pattern(&initial, b"\x1b_Ga=t");
    let mut placement_transactions = count_batch_pattern(&initial, b"\x1b_Ga=p");
    let mut presented = initial.predicted;
    let mut maximum_store_bytes = store.total_bytes();
    let mut maximum_scene_bytes = initial_scene
        .image_uploads
        .iter()
        .map(|upload| upload.data.len())
        .sum::<usize>();
    let mut latencies = Vec::with_capacity(iterations);

    ALLOCATIONS.store(0, Ordering::Relaxed);
    ALLOCATED_BYTES.store(0, Ordering::Relaxed);
    let started = Instant::now();
    for iteration in 0..iterations {
        let update_started = Instant::now();
        let update = source
            .advance(format!("\x1b[20;1Hmedia frame {iteration:06}\x1b[K").as_bytes())
            .map_err(|error| error.to_string())?;
        let scene = media_scene(&source, &mut store, ROOT)?;
        let damage = SceneDamage::from_terminal_update(&scene.panes[0], &update, geometry);
        let batch = renderer
            .render(&scene, &damage, &presented)
            .map_err(|error| error.to_string())?;
        upload_transactions =
            upload_transactions.saturating_add(count_batch_pattern(&batch, b"\x1b_Ga=t"));
        placement_transactions =
            placement_transactions.saturating_add(count_batch_pattern(&batch, b"\x1b_Ga=p"));
        output_bytes = output_bytes.saturating_add(render_batch_bytes(&batch));
        maximum_store_bytes = maximum_store_bytes.max(store.total_bytes());
        maximum_scene_bytes = maximum_scene_bytes.max(
            scene
                .image_uploads
                .iter()
                .map(|upload| upload.data.len())
                .sum(),
        );
        renderer.confirm(&batch.predicted);
        presented = batch.predicted;
        latencies.push(nanos(update_started.elapsed()).max(1));
    }
    let elapsed_ns = nanos(started.elapsed()).max(1);
    let allocations = ALLOCATIONS.load(Ordering::Relaxed);
    let allocated_bytes = ALLOCATED_BYTES.load(Ordering::Relaxed);
    latencies.sort_unstable();
    let inspected_bytes = pixels.len().saturating_mul(iterations) as f64;
    let seconds = elapsed_ns as f64 / 1_000_000_000.0;
    black_box(presented);

    Ok(MediaWorkloadReport {
        name: "kitty-media-recomposition",
        iterations,
        decoded_image_bytes: pixels.len(),
        elapsed_ns,
        throughput_mib_per_second: inspected_bytes / (1024.0 * 1024.0) / seconds,
        allocations,
        allocated_bytes,
        latency_p50_ns: percentile(&latencies, 50),
        latency_p95_ns: percentile(&latencies, 95),
        latency_max_ns: latencies.last().copied().unwrap_or(0),
        maximum_store_bytes,
        maximum_scene_bytes,
        output_bytes,
        upload_transactions,
        placement_transactions,
    })
}

fn media_scene(
    source: &GhosttyEngine,
    store: &mut PaneMediaStore,
    owner: SurfaceId,
) -> Result<Scene, String> {
    let snapshot = source.normalized_snapshot();
    store
        .synchronize(
            &source
                .kitty_image_placements()
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
    let mut scene = Scene::new(snapshot.geometry);
    scene
        .panes
        .push(SceneSurface::new(owner, GridPoint::new(0, 0), snapshot));
    scene.cursor_owner = CursorOwner::Pane(owner);
    store
        .append_to_scene(
            owner,
            GridPoint::new(0, 0),
            GridRect::new(
                GridPoint::new(0, 0),
                scene.geometry.rows,
                scene.geometry.cols,
            ),
            &mut scene,
        )
        .map_err(|error| error.to_string())?;
    Ok(scene)
}

fn render_batch_bytes(batch: &RenderBatch) -> u64 {
    batch
        .transactions
        .iter()
        .map(|transaction| transaction.bytes.len() as u64)
        .sum()
}

fn count_batch_pattern(batch: &RenderBatch, pattern: &[u8]) -> usize {
    batch
        .transactions
        .iter()
        .map(|transaction| {
            transaction
                .bytes
                .windows(pattern.len())
                .filter(|window| *window == pattern)
                .count()
        })
        .sum()
}

fn encode_base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let a = chunk[0];
        let b = chunk.get(1).copied().unwrap_or(0);
        let c = chunk.get(2).copied().unwrap_or(0);
        encoded.push(TABLE[usize::from(a >> 2)] as char);
        encoded.push(TABLE[usize::from((a & 0x03) << 4 | b >> 4)] as char);
        encoded.push(if chunk.len() > 1 {
            TABLE[usize::from((b & 0x0f) << 2 | c >> 6)] as char
        } else {
            '='
        });
        encoded.push(if chunk.len() > 2 {
            TABLE[usize::from(c & 0x3f)] as char
        } else {
            '='
        });
    }
    encoded
}

fn run_scheduler_workload(workload: SchedulerWorkload) -> Result<SchedulerWorkloadReport, String> {
    const UPDATES_PER_BOUNDARY: usize = 8;
    let config = OutputSchedulerConfig::default();
    let mut scheduler = OutputScheduler::new(config, true);
    let mut writer = BenchmarkWriter::default();
    let mut latencies = Vec::with_capacity(workload.iterations);
    let mut maximum_pending_bytes = 0usize;
    let mut replaced_renders = 0usize;
    let mut blocked_writes = 0usize;
    let mut completed_renders = 0usize;
    let updates_per_iteration = match workload.kind {
        SchedulerWorkloadKind::EventBoundaryCoalescing => UPDATES_PER_BOUNDARY,
        SchedulerWorkloadKind::BackpressureRecovery => 1,
    };
    let started = Instant::now();
    for iteration in 0..workload.iterations {
        let boundary_started = Instant::now();
        match workload.kind {
            SchedulerWorkloadKind::EventBoundaryCoalescing => {
                for update in 0..UPDATES_PER_BOUNDARY {
                    let batch = scheduler_benchmark_batch(iteration, update);
                    if scheduler.enqueue_render(batch, iteration as u128)
                        == EnqueueOutcome::ReplacedObsoleteRender
                    {
                        replaced_renders = replaced_renders.saturating_add(1);
                    }
                    maximum_pending_bytes = maximum_pending_bytes.max(scheduler.pending_bytes());
                }
                let report = scheduler
                    .drain_ready(iteration as u128, true, &mut writer)
                    .map_err(|error| error.to_string())?;
                completed_renders =
                    completed_renders.saturating_add(report.completed_renders.len());
            }
            SchedulerWorkloadKind::BackpressureRecovery => {
                scheduler
                    .enqueue_render(scheduler_benchmark_batch(iteration, 0), iteration as u128);
                maximum_pending_bytes = maximum_pending_bytes.max(scheduler.pending_bytes());
                writer.block_next_write = true;
                let blocked = scheduler
                    .drain_ready(iteration as u128, true, &mut writer)
                    .map_err(|error| error.to_string())?;
                if blocked.blocked {
                    blocked_writes = blocked_writes.saturating_add(1);
                }
                scheduler.notify_writable();
                let completed = scheduler
                    .drain_ready(iteration as u128, true, &mut writer)
                    .map_err(|error| error.to_string())?;
                completed_renders =
                    completed_renders.saturating_add(completed.completed_renders.len());
            }
        }
        latencies.push(nanos(boundary_started.elapsed()).max(1));
    }
    let elapsed_ns = nanos(started.elapsed()).max(1);
    latencies.sort_unstable();
    Ok(SchedulerWorkloadReport {
        name: workload.name,
        iterations: workload.iterations,
        updates: workload.iterations.saturating_mul(updates_per_iteration),
        elapsed_ns,
        latency_p50_ns: percentile(&latencies, 50),
        latency_p95_ns: percentile(&latencies, 95),
        latency_max_ns: latencies.last().copied().unwrap_or(0),
        output_bytes: writer.bytes,
        maximum_pending_bytes,
        replaced_renders,
        blocked_writes,
        completed_renders,
    })
}

fn scheduler_benchmark_batch(iteration: usize, update: usize) -> RenderBatch {
    let marker = format!("frame {iteration:06}/{update} {:<96}", iteration % 100);
    RenderBatch::new(
        vec![OutputTransaction::new(marker.as_bytes())],
        PresentedScene::blank(TerminalGeometry::from_cells(24, 80)),
    )
}

#[derive(Default)]
struct BenchmarkWriter {
    bytes: u64,
    block_next_write: bool,
}

impl Write for BenchmarkWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.block_next_write {
            self.block_next_write = false;
            return Err(io::Error::from(io::ErrorKind::WouldBlock));
        }
        self.bytes = self.bytes.saturating_add(bytes.len() as u64);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn ratio(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

fn renderer_scene(engine: &GhosttyEngine, root: SurfaceId) -> Scene {
    let snapshot = engine.normalized_snapshot();
    let mut scene = Scene::new(snapshot.geometry);
    scene.effects.title.clone_from(&snapshot.title);
    scene
        .effects
        .working_directory
        .clone_from(&snapshot.working_directory);
    scene
        .panes
        .push(SceneSurface::new(root, GridPoint::new(0, 0), snapshot));
    scene.cursor_owner = CursorOwner::Pane(root);
    scene
}

fn renderer_update(
    kind: RendererWorkloadKind,
    iteration: usize,
    geometry: TerminalGeometry,
) -> Vec<u8> {
    match kind {
        RendererWorkloadKind::SingleCell => format!(
            "\x1b[{};{}H{}",
            iteration % usize::from(geometry.rows) + 1,
            iteration % usize::from(geometry.cols - 1) + 1,
            char::from(b'a' + (iteration % 26) as u8),
        )
        .into_bytes(),
        RendererWorkloadKind::StatusLine => format!(
            "\x1b[{};1Hstatus {iteration:06} {:<60}\x1b[K",
            geometry.rows,
            iteration % 100,
        )
        .into_bytes(),
        RendererWorkloadKind::CursorMove => format!(
            "\x1b[{};{}H",
            iteration % usize::from(geometry.rows) + 1,
            iteration % usize::from(geometry.cols) + 1,
        )
        .into_bytes(),
        RendererWorkloadKind::Scroll => format!(
            "\x1b[S\x1b[{};1Hscroll line {iteration:06}\x1b[K",
            geometry.rows,
        )
        .into_bytes(),
        RendererWorkloadKind::TmuxLike => match iteration % 7 {
            0 => format!(
                "\x1b[r\x1b[S\x1b[{};1Htmux scroll {iteration:06}\x1b[K",
                geometry.rows,
            ),
            1 => format!("\x1b[3;22r\x1b[S\x1b[22;1Htmux partial {iteration:06}\x1b[K"),
            2 => format!("\x1b[6;8H\x1b[3@I{iteration:06}"),
            3 => format!("\x1b[7;9H\x1b[3PD{iteration:06}"),
            4 => format!("\x1b[3;22r\x1b[8;1H\x1b[Linsert {iteration:06}"),
            5 => format!("\x1b[3;22r\x1b[9;1H\x1b[Mdelete {iteration:06}"),
            _ => format!("\x1b[10;5H\x1b[12X{iteration:06}"),
        }
        .into_bytes(),
        RendererWorkloadKind::ZellijLike => match iteration % 6 {
            0 => format!(
                "\x1b[1;1H\x1b[1;38;5;15;48;5;4m tab-{iteration:06} {:<56}\x1b[0m\x1b[K",
                "active pane"
            ),
            1 => format!(
                "\x1b[2;1H\x1b[38;5;8m+{}+\x1b[0m",
                "-".repeat(usize::from(geometry.cols.saturating_sub(2)))
            ),
            2 => format!(
                "\x1b[{};1H\x1b[7m status {iteration:06} {:<54}\x1b[0m\x1b[K",
                geometry.rows,
                iteration % 100
            ),
            3 => format!("\x1b[4;3Hpane title {iteration:06}\x1b[K"),
            4 => format!(
                "\x1b[3;{}r\x1b[S\x1b[{};2Hzellij scroll {iteration:06}\x1b[K\x1b[r",
                geometry.rows.saturating_sub(1),
                geometry.rows.saturating_sub(1),
            ),
            _ => format!(
                "\x1b[?2026h\x1b[5;5Hlayer-a {iteration:06}\x1b[6;5Hlayer-b {:06}\x1b[?2026l",
                iteration.wrapping_mul(17)
            ),
        }
        .into_bytes(),
    }
}

fn renderer_workloads(self_test: bool) -> Vec<RendererWorkload> {
    if self_test {
        return vec![RendererWorkload {
            name: "renderer-self-test",
            kind: RendererWorkloadKind::SingleCell,
            iterations: 8,
        }];
    }
    vec![
        RendererWorkload {
            name: "single-cell-edits",
            kind: RendererWorkloadKind::SingleCell,
            iterations: 10_000,
        },
        RendererWorkload {
            name: "status-line-replacements",
            kind: RendererWorkloadKind::StatusLine,
            iterations: 5_000,
        },
        RendererWorkload {
            name: "cursor-moves",
            kind: RendererWorkloadKind::CursorMove,
            iterations: 10_000,
        },
        RendererWorkload {
            name: "scrolling-output",
            kind: RendererWorkloadKind::Scroll,
            iterations: 2_000,
        },
        RendererWorkload {
            name: "tmux-like-structural-edits",
            kind: RendererWorkloadKind::TmuxLike,
            iterations: 5_000,
        },
        RendererWorkload {
            name: "zellij-like-layered-redraws",
            kind: RendererWorkloadKind::ZellijLike,
            iterations: 5_000,
        },
    ]
}

fn scheduler_workloads(self_test: bool) -> Vec<SchedulerWorkload> {
    let iterations = if self_test { 4 } else { 10_000 };
    vec![
        SchedulerWorkload {
            name: "event-boundary-coalescing",
            kind: SchedulerWorkloadKind::EventBoundaryCoalescing,
            iterations,
        },
        SchedulerWorkload {
            name: "backpressure-recovery",
            kind: SchedulerWorkloadKind::BackpressureRecovery,
            iterations,
        },
    ]
}

fn self_test_workloads() -> Vec<Workload> {
    vec![Workload {
        name: "self-test",
        chunks: vec![
            b"shell$ printf hello\r\n".to_vec(),
            b"\x1b[31mhello\x1b[0m\r\n".to_vec(),
        ],
        iterations: 4,
        scrollback: 64,
    }]
}

fn baseline_workloads() -> Vec<Workload> {
    let shell = vec![
        b"\x1b]133;A\x07dev@host:~/lector$ \x1b]133;B\x07cargo test\x1b]133;C\x07\r\n".to_vec(),
        b"\x1b[32mtest result: ok. 383 passed; 0 failed\x1b[0m\r\n\x1b]133;D;0\x07".to_vec(),
    ];
    let control_heavy = (0..512)
        .map(|index| {
            format!(
                "\x1b[{};1H\x1b[2K\x1b[38;5;{}mprogress {index:04}\x1b[0m",
                index % 24 + 1,
                index % 256
            )
            .into_bytes()
        })
        .collect();
    let scrollback = (0..20_100)
        .map(|index| format!("build line {index:05}: compiling dependency\r\n").into_bytes())
        .collect();
    vec![
        Workload {
            name: "shell-and-semantic-prompts",
            chunks: shell,
            iterations: 2_000,
            scrollback: 10_000,
        },
        Workload {
            name: "control-heavy-redraw",
            chunks: control_heavy,
            iterations: 20,
            scrollback: 10_000,
        },
        Workload {
            name: "bounded-scrollback",
            chunks: scrollback,
            iterations: 1,
            scrollback: 10_000,
        },
    ]
}

fn percentile(values: &[u64], percentile: usize) -> u64 {
    if values.is_empty() {
        return 0;
    }
    let index = (values.len() - 1).saturating_mul(percentile) / 100;
    values[index]
}

fn nanos(duration: std::time::Duration) -> u64 {
    duration.as_nanos().try_into().unwrap_or(u64::MAX)
}

#[cfg(unix)]
fn peak_rss_bytes() -> u64 {
    let mut usage = std::mem::MaybeUninit::<nix::libc::rusage>::zeroed();
    // SAFETY: `usage` points to writable storage for exactly one `rusage`,
    // and `getrusage` initializes it on a successful return.
    let result = unsafe { nix::libc::getrusage(nix::libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    if result != 0 {
        return 0;
    }
    // SAFETY: the successful call above initialized `usage`.
    let raw = unsafe { usage.assume_init() }.ru_maxrss.max(0) as u64;
    if cfg!(target_os = "macos") {
        raw
    } else {
        raw.saturating_mul(1024)
    }
}

#[cfg(not(unix))]
fn peak_rss_bytes() -> u64 {
    0
}
