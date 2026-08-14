use lector::terminal::GhosttyEngine;
use serde::{Deserialize, Serialize};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    env, fs,
    hint::black_box,
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
