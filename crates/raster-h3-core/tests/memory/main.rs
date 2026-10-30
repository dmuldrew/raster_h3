//! Memory benchmark suite: peak live heap, allocation and HTTP request counts,
//! with RSS and wall time reported alongside.
//!
//! Every scenario variant runs in its own child process (this binary re-run
//! with `--scenario NAME --variant V`), so allocator peaks and RSS high-water
//! marks never mix between scenarios. Fixtures are built before measurement
//! starts. The parent compares variants and fails on any check.
//!
//! Checks use only counts that do not depend on machine speed: allocator
//! bytes (requested sizes, not RSS) relative to configured budgets or to
//! another variant, HTTP request counts, spill runs and output checksums.
//! RSS and wall time are printed for trends on stable hardware and never
//! asserted.
//!
//! ```text
//! cargo test --release -p raster-h3-core --test memory                 # all, with checks
//! cargo test --release -p raster-h3-core --test memory -- spill        # name filter
//! cargo test --release -p raster-h3-core --test memory -- --repeat 5 --json mem.json
//! ```

#[path = "alloc.rs"]
mod alloc;
#[path = "../it/helpers.rs"]
mod helpers;
mod http;
mod scenarios;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::process::{Command, ExitCode};
use std::time::Instant;

use serde_json::{json, Value};

#[global_allocator]
static GLOBAL: alloc::Counting = alloc::Counting;

/// Named measurements from one variant run.
pub type Metrics = BTreeMap<String, f64>;

/// Metrics per variant, for checks that compare variants.
pub type Results = BTreeMap<&'static str, Metrics>;

pub struct Scenario {
    pub name: &'static str,
    pub about: &'static str,
    pub variants: &'static [&'static str],
    /// Columns (metric keys) shown in the report table after the common ones.
    pub columns: &'static [&'static str],
    pub run: fn(variant: &str) -> Metrics,
    pub check: fn(&Results) -> Vec<Check>,
}

pub struct Check {
    pub name: String,
    pub pass: bool,
    pub detail: String,
}

impl Check {
    /// Passes when `value <= limit`.
    pub fn at_most(name: &str, value: f64, limit: f64) -> Self {
        Self {
            name: name.to_string(),
            pass: value <= limit,
            detail: format!("{} <= {}", fmt_num(value), fmt_num(limit)),
        }
    }

    pub fn equal(name: &str, a: f64, b: f64) -> Self {
        Self {
            name: name.to_string(),
            pass: a == b,
            detail: format!("{} == {}", fmt_num(a), fmt_num(b)),
        }
    }

    pub fn at_least(name: &str, value: f64, limit: f64) -> Self {
        Self {
            name: name.to_string(),
            pass: value >= limit,
            detail: format!("{} >= {}", fmt_num(value), fmt_num(limit)),
        }
    }
}

/// Measures the region after fixtures are built: allocator counters, wall
/// time and (where the platform exposes it) the RSS high-water mark.
pub struct Probe {
    start: Instant,
    snapshot: alloc::Snapshot,
    rss_base: Option<u64>,
}

impl Probe {
    pub fn start() -> Self {
        let rss_base = rss::reset_peak();
        let snapshot = alloc::snapshot();
        alloc::reset_peak();
        Self {
            start: Instant::now(),
            snapshot,
            rss_base,
        }
    }

    /// Record `wall_ms`, `allocs`, `alloc_mib`, `peak_live_bytes` and RSS into `m`.
    pub fn finish(self, m: &mut Metrics) {
        let wall_ms = self.start.elapsed().as_secs_f64() * 1e3;
        let end = alloc::snapshot();
        m.insert("wall_ms".into(), wall_ms);
        m.insert("allocs".into(), (end.allocs - self.snapshot.allocs) as f64);
        m.insert(
            "alloc_mib".into(),
            (end.alloc_bytes - self.snapshot.alloc_bytes) as f64 / MIB,
        );
        m.insert(
            "peak_live_bytes".into(),
            alloc::peak().saturating_sub(self.snapshot.live) as f64,
        );
        if let Some(peak) = rss::peak() {
            m.insert("rss_peak_mib".into(), peak as f64 / MIB);
            if let Some(base) = self.rss_base {
                m.insert(
                    "rss_growth_mib".into(),
                    peak.saturating_sub(base) as f64 / MIB,
                );
            }
        }
    }
}

pub const MIB: f64 = 1024.0 * 1024.0;

/// Process RSS. Linux resets the high-water mark after fixtures are built;
/// other Unix platforms report the whole process's maximum; Windows nothing.
mod rss {
    #[cfg(target_os = "linux")]
    fn status_kib(key: &str) -> Option<u64> {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        let line = status.lines().find(|l| l.starts_with(key))?;
        line[key.len()..]
            .trim()
            .trim_end_matches("kB")
            .trim()
            .parse()
            .ok()
    }

    /// Reset the high-water mark where possible; returns the current RSS.
    pub fn reset_peak() -> Option<u64> {
        #[cfg(target_os = "linux")]
        {
            // "5" resets VmHWM to the current RSS (Linux 4.0+).
            let _ = std::fs::write("/proc/self/clear_refs", "5");
            status_kib("VmRSS:").map(|k| k * 1024)
        }
        #[cfg(not(target_os = "linux"))]
        {
            None
        }
    }

    pub fn peak() -> Option<u64> {
        #[cfg(target_os = "linux")]
        {
            status_kib("VmHWM:").map(|k| k * 1024)
        }
        #[cfg(all(unix, not(target_os = "linux")))]
        {
            // SAFETY: `getrusage` only writes into the zeroed struct we pass.
            let usage = unsafe {
                let mut usage: libc::rusage = std::mem::zeroed();
                (libc::getrusage(libc::RUSAGE_SELF, &mut usage) == 0).then_some(usage)
            }?;
            // ru_maxrss is in bytes on macOS and kilobytes on the BSDs.
            let scale = if cfg!(target_os = "macos") { 1 } else { 1024 };
            u64::try_from(usage.ru_maxrss).ok().map(|v| v * scale)
        }
        #[cfg(not(unix))]
        {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Child: run one variant and print its metrics
// ---------------------------------------------------------------------------

const RESULT_PREFIX: &str = "MEMBENCH_RESULT ";

fn run_child(name: &str, variant: &str) -> ExitCode {
    let Some(scenario) = scenarios::all().into_iter().find(|s| s.name == name) else {
        eprintln!("unknown scenario {name}");
        return ExitCode::FAILURE;
    };
    let metrics = (scenario.run)(variant);
    println!(
        "{RESULT_PREFIX}{}",
        serde_json::to_string(&metrics).expect("metrics json")
    );
    ExitCode::SUCCESS
}

fn spawn_child(name: &str, variant: &str) -> Result<Metrics, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let output = Command::new(exe)
        .args(["--scenario", name, "--variant", variant])
        .output()
        .map_err(|e| e.to_string())?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().find_map(|l| l.strip_prefix(RESULT_PREFIX));
    match (output.status.success(), line) {
        (true, Some(line)) => serde_json::from_str(line).map_err(|e| e.to_string()),
        _ => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let tail: Vec<&str> = stderr.lines().rev().take(15).collect();
            Err(format!(
                "child exited with {}: {}",
                output.status,
                tail.into_iter().rev().collect::<Vec<_>>().join("\n")
            ))
        }
    }
}

/// Per-key median over repeated runs (checks and the report use the median run).
fn median(runs: Vec<Metrics>) -> Metrics {
    let mut out = Metrics::new();
    for key in runs[0].keys() {
        let mut values: Vec<f64> = runs.iter().filter_map(|m| m.get(key).copied()).collect();
        values.sort_by(f64::total_cmp);
        out.insert(key.clone(), values[values.len() / 2]);
    }
    out
}

// ---------------------------------------------------------------------------
// Parent: run every variant, check and report
// ---------------------------------------------------------------------------

struct Args {
    filters: Vec<String>,
    json: Option<String>,
    repeat: usize,
    list: bool,
    child: Option<(String, String)>,
}

fn parse_args() -> Args {
    let mut args = Args {
        filters: Vec::new(),
        json: None,
        repeat: 1,
        list: false,
        child: None,
    };
    let mut scenario = None;
    let mut variant = None;
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--scenario" => scenario = it.next(),
            "--variant" => variant = it.next(),
            "--json" => args.json = it.next(),
            "--repeat" => args.repeat = it.next().and_then(|v| v.parse().ok()).unwrap_or(1),
            "--list" => args.list = true,
            // libtest options that take a value; the rest are ignored.
            "--test-threads" | "--skip" | "--logfile" | "--format" | "--color" | "-Z" => {
                it.next();
            }
            a if a.starts_with('-') => {}
            a => args.filters.push(a.to_string()),
        }
    }
    if let (Some(s), Some(v)) = (scenario, variant) {
        args.child = Some((s, v));
    }
    args
}

pub fn fmt_num(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{v:.0}")
    } else if v.abs() >= 100.0 {
        format!("{v:.1}")
    } else {
        format!("{v:.3}")
    }
}

fn fmt_cell(m: &Metrics, key: &str) -> String {
    match m.get(key) {
        Some(v) if key.ends_with("_bytes") => format!("{:.2}", v / MIB),
        Some(&v) => fmt_num(v),
        None => "—".into(),
    }
}

fn header(key: &str) -> String {
    key.strip_suffix("_bytes")
        .map_or_else(|| key.to_string(), |k| format!("{k} MiB"))
}

fn annotate(level: &str, title: &str, body: &str) {
    if std::env::var_os("GITHUB_ACTIONS").is_some() {
        let encoded = body
            .replace('%', "%25")
            .replace('\r', "%0D")
            .replace('\n', "%0A");
        println!("::{level} title={title}::{encoded}");
    }
}

fn main() -> ExitCode {
    let args = parse_args();
    if let Some((name, variant)) = &args.child {
        return run_child(name, variant);
    }
    let selected: Vec<Scenario> = scenarios::all()
        .into_iter()
        .filter(|s| args.filters.is_empty() || args.filters.iter().any(|f| s.name.contains(f)))
        .collect();
    if args.list {
        for s in &selected {
            println!("{}: {}", s.name, s.variants.join(", "));
        }
        return ExitCode::SUCCESS;
    }

    const COMMON: [&str; 6] = [
        "peak_live_bytes",
        "allocs",
        "alloc_mib",
        "rss_peak_mib",
        "rss_growth_mib",
        "wall_ms",
    ];
    let mut markdown = String::new();
    // GitHub keeps only a few annotations per step, so one compact notice
    // covers the run; failures get their own error annotations.
    let mut notice = String::new();
    let mut json_out = serde_json::Map::new();
    let mut failures = 0;
    for scenario in &selected {
        let mut results = Results::new();
        let mut errors = Vec::new();
        for &variant in scenario.variants {
            let runs: Result<Vec<Metrics>, String> = (0..args.repeat.max(1))
                .map(|_| spawn_child(scenario.name, variant))
                .collect();
            match runs {
                Ok(runs) => {
                    results.insert(variant, median(runs));
                }
                Err(e) => errors.push(format!("{variant}: {e}")),
            }
        }
        let checks = if errors.is_empty() {
            (scenario.check)(&results)
        } else {
            Vec::new()
        };

        let columns: Vec<&str> = COMMON
            .iter()
            .chain(scenario.columns)
            .copied()
            .filter(|k| results.values().any(|m| m.contains_key(*k)))
            .collect();
        let mut table = format!(
            "| variant | {} |\n|:---|{}\n",
            columns
                .iter()
                .map(|k| header(k))
                .collect::<Vec<_>>()
                .join(" | "),
            "---:|".repeat(columns.len())
        );
        for (variant, m) in &results {
            let cells: Vec<String> = columns.iter().map(|k| fmt_cell(m, k)).collect();
            let _ = writeln!(table, "| {variant} | {} |", cells.join(" | "));
        }
        let mut lines = String::new();
        for e in &errors {
            let _ = writeln!(lines, "- ERROR {e}");
        }
        for c in &checks {
            let mark = if c.pass { "ok" } else { "FAILED" };
            let _ = writeln!(lines, "- {mark}: {} ({})", c.name, c.detail);
        }
        let failed = errors.len() + checks.iter().filter(|c| !c.pass).count();
        failures += failed;
        let body = format!("{}\n\n{table}\n{lines}", scenario.about);
        print!("### {}\n\n{body}\n", scenario.name);
        if failed > 0 {
            annotate("error", &format!("memory: {}", scenario.name), &body);
        }
        let peaks: Vec<String> = results
            .iter()
            .map(|(v, m)| format!("{v} {}", fmt_cell(m, "peak_live_bytes")))
            .collect();
        let _ = writeln!(
            notice,
            "{}: {}/{} checks ok; peak MiB {}",
            scenario.name,
            checks.len() + errors.len() - failed,
            checks.len() + errors.len(),
            peaks.join(", ")
        );
        let _ = write!(markdown, "### {}\n\n{body}\n", scenario.name);
        json_out.insert(
            scenario.name.to_string(),
            json!({
                "variants": results,
                "checks": checks.iter().map(|c| json!({"name": c.name, "pass": c.pass, "detail": c.detail})).collect::<Vec<_>>(),
                "errors": errors,
            }),
        );
    }

    annotate("notice", "memory benchmarks", &notice);
    if let Some(summary) = std::env::var_os("GITHUB_STEP_SUMMARY") {
        use std::io::Write as _;
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(summary)
        {
            let _ = writeln!(file, "## Memory benchmarks\n\n{markdown}");
        }
    }
    if let Some(path) = &args.json {
        let text = serde_json::to_string_pretty(&Value::Object(json_out)).expect("json");
        std::fs::write(path, text).expect("write json");
        eprintln!("wrote {path}");
    }
    if failures > 0 {
        eprintln!("{failures} memory check(s) failed");
        ExitCode::FAILURE
    } else {
        println!("{} scenario(s) passed", selected.len());
        ExitCode::SUCCESS
    }
}
