//! `frust bench`: end-to-end measurements of what frust buys over plain cargo.

use std::fmt::Write as _;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::compact::{self, AutoOptions, Category, CompactOptions};
use crate::config::Config as FrustConfig;
use crate::fsx::{self, allocated_size, human_bytes};
use crate::gc::{self, GcOptions};
use std::ops::Range;
use crate::paths::Dirs;
use crate::style::{Styler, duration_ms};
use crate::sys;
use crate::toolchain::{Probe, resolve_tool};
use crate::workspace;

const MIN_FREE_BYTES: u64 = 12 * 1024 * 1024 * 1024;
const MIN_RUNS: usize = 3;
const DEFAULT_RUNS: usize = 5;
// How long to keep reading a child's stderr after it exited, in case a grandchild still holds the pipe.
const DRAIN: Duration = Duration::from_millis(500);

const L_CARGO_A: &str = "cargo      project A, cold";
const L_CARGO_B: &str = "cargo      project B, cold";
const L_FRUST_A: &str = "frust      project A, cold cache";
const L_FRUST_B: &str = "frust      project B, warm cache";
const L_CARGO_CLEAN: &str = "cargo      project A, clean rebuild";
const L_FRUST_CLEAN: &str = "frust      project A, clean rebuild";
const L_FRUST_DISPLAY: &str = "frust      project A, clean rebuild, display on";
const L_CARGO_NOOP: &str = "cargo      no-op build";
const L_FRUST_NOOP: &str = "frust      no-op build";
const L_CARGO_INC: &str = "cargo      incremental edit";
const L_FRUST_INC: &str = "frust      incremental edit";
const L_CARGO_INC_WARM: &str = "cargo      incremental edit, warm session";
const L_FRUST_INC_WARM: &str = "frust      incremental edit, warm session";
const L_REL_CARGO_A: &str = "cargo      release A, cold";
const L_REL_CARGO_B: &str = "cargo      release B, cold";
const L_REL_FRUST_A: &str = "frust      release A, cold cache";
const L_REL_FRUST_B: &str = "frust      release B, warm cache";
const L_REL_CARGO_CLEAN: &str = "cargo      release A, clean rebuild";
const L_REL_FRUST_CLEAN: &str = "frust      release A, clean rebuild";
const L_MAINT_GC: &str = "frust      maintenance: daily gc pass (once)";
const L_MAINT_COMPACT: &str = "frust      maintenance: auto-compaction (once)";

#[derive(Debug, Clone, Serialize)]
pub struct Measurement {
    pub label: String,
    // Median of `runs` runs.
    pub millis: u64,
    pub min_millis: u64,
    pub max_millis: u64,
    pub runs: usize,
    pub target_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Machine {
    pub os: String,
    pub cpu: String,
    pub cores: usize,
    // One-minute load average; the one after includes the benchmark's own work.
    pub load_before: Option<f64>,
    pub load_after: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub rustc: String,
    pub machine: Machine,
    pub runs: usize,
    pub registry_deps: bool,
    pub local_crates: usize,
    pub cases: Vec<Measurement>,
    pub cargo_two_projects_disk: u64,
    pub frust_two_projects_disk: u64,
    pub frust_two_projects_du: u64,
    pub speedup_second_project: f64,
    pub speedup_clean_rebuild: f64,
    pub noop_overhead_ms: i64,
    pub release_speedup_second_project: f64,
    pub release_speedup_clean_rebuild: f64,
    /// Release binary built by frust from a warm cache is byte-identical to the one it compiled cold.
    pub release_binary_identical_cold_vs_restored: bool,
    pub release_binary_identical_to_cargo: bool,
    /// The Apple linker derives `LC_UUID` from a symbol table holding target-directory paths, so a different path changes the UUID.
    pub release_binary_other_dir: BinaryDiff,
    pub runtime_runs: usize,
    pub runtime_median_us_cargo: u64,
    pub runtime_median_us_frust: u64,
    pub runtime_outputs_equal: bool,
    pub churn: Option<ChurnReport>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChurnReport {
    pub configs: Vec<String>,
    pub cargo_target: u64,
    /// Compression and deduplication only: frust removes variants only in its own directories.
    pub cargo_target_compressed: u64,
    /// Plain cargo still considers everything fresh after that compression.
    pub cargo_compiled_after_compress: usize,
    pub frust_final_compacted: u64,
    pub frust_accumulated: u64,
    pub frust_target: u64,
    pub frust_store: u64,
    pub gc_variant_paths: usize,
    pub gc_variant_bytes: u64,
    pub gc_sessions: usize,
    pub gc_session_bytes: u64,
    pub frust_after_gc: u64,
    pub frust_after_compact: u64,
    pub dead_units: usize,
    pub stale_sessions: usize,
    pub compressed_files: u64,
    pub deduped_files: u64,
    /// Units recompiled by rebuilding the last configuration after compaction (must be 0).
    pub frust_compiled_after_compact: usize,
    pub binary_output_unchanged: bool,
    pub breakdown_before: Vec<(String, u64)>,
    pub breakdown_after: Vec<(String, u64)>,
    pub edits: usize,
    pub cargo_incremental_before: u64,
    pub cargo_sessions_before: usize,
    pub frust_incremental_before: u64,
    pub frust_sessions_before: usize,
    pub cargo_incremental: u64,
    pub cargo_sessions: usize,
    pub frust_incremental: u64,
    pub frust_sessions: usize,
    pub frust_incremental_after_compact: u64,
    pub frust_sessions_after_compact: usize,
    pub auto_after_churn: u64,
    pub auto_after_edits: u64,
    pub cargo_after_edits: u64,
    pub auto_compiled_on_repeat: usize,
    pub release_clean_cargo_ms: u64,
    pub release_clean_frust_ms: u64,
    pub release_clean_frust_compiled: usize,
    pub release_clean_frust_cache_hits: u64,
    pub release_clean_frust_cache_misses: u64,
    pub growth: Option<GrowthReport>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BinaryDiff {
    pub identical: bool,
    pub differing_bytes: u64,
    /// Every differing byte lies inside the Mach-O `LC_UUID` or the code signature blob.
    pub only_uuid_and_signature: bool,
}

pub fn run(_dirs: &Dirs, flags: &[String]) -> i32 {
    let quick = flags.iter().any(|f| f == "--quick");
    let json = flags.iter().any(|f| f == "--json");
    let churn_only = flags.iter().any(|f| f == "--churn");
    let s = if json { Styler::plain() } else { Styler::detect() };
    let runs = match runs_from_flags(flags, quick) {
        Ok(n) => n,
        Err(msg) => {
            eprintln!("frust bench: {msg}");
            return 1;
        }
    };

    let scratch = std::env::temp_dir().join(format!("frust-bench-{}", std::process::id()));
    remove_abandoned_scratch(&std::env::temp_dir());
    if let Err(msg) = fsx::require_free_space(&std::env::temp_dir(), MIN_FREE_BYTES) {
        eprintln!("frust bench: refusing to run: {msg}");
        return 1;
    }
    let guard = Scratch(scratch.clone());
    if flags.iter().any(|f| f == "--linking") {
        let result = linking_bench(&scratch, quick, runs);
        drop(guard);
        return match result {
            Ok(report) => {
                if json {
                    println!("{}", serde_json::to_string_pretty(&report).unwrap_or_default());
                } else {
                    print_linking(&report, &s);
                }
                0
            }
            Err(msg) => {
                eprintln!("frust bench: {msg}");
                1
            }
        };
    }
    if flags.iter().any(|f| f == "--latency") {
        let result = latency_bench(&scratch, flags, quick, &s, json);
        drop(guard);
        return match result {
            Ok(report) => {
                if json {
                    println!("{}", serde_json::to_string_pretty(&report).unwrap_or_default());
                }
                0
            }
            Err(msg) => {
                eprintln!("frust bench: {msg}");
                1
            }
        };
    }
    if flags.iter().any(|f| f == "--growth") {
        let result = growth_bench(&scratch, &GrowthOptions::from_flags(flags, quick), &s, json);
        drop(guard);
        return match result {
            Ok(report) => {
                if json {
                    println!("{}", serde_json::to_string_pretty(&report).unwrap_or_default());
                } else {
                    print_growth(&report, &s);
                }
                0
            }
            Err(msg) => {
                eprintln!("frust bench: {msg}");
                1
            }
        };
    }
    if churn_only {
        let result = churn_bench(&scratch, quick, &s, json);
        drop(guard);
        return match result {
            Ok(report) => {
                if json {
                    println!("{}", serde_json::to_string_pretty(&report).unwrap_or_default());
                } else {
                    print_churn(&report, &s);
                }
                0
            }
            Err(msg) => {
                eprintln!("frust bench: {msg}");
                1
            }
        };
    }
    let result = bench(&scratch, quick, runs, &s, json).and_then(|mut report| {
        report.churn = Some(churn_bench(&scratch.join("churn"), quick, &s, json)?);
        report.machine.load_after = load_average();
        Ok(report)
    });
    drop(guard);
    match result {
        Ok(report) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&report).unwrap_or_default());
            } else {
                print_report(&report, &s);
            }
            match failed_checks(&report) {
                Some(why) => {
                    eprintln!("frust bench: correctness check failed: {why}");
                    1
                }
                None => 0,
            }
        }
        Err(msg) => {
            eprintln!("frust bench: {msg}");
            1
        }
    }
}

/// Killed benchmarks never ran their [`Scratch`] guard and hold hundreds of megabytes, so they are removed first.
fn remove_abandoned_scratch(tmp: &Path) {
    let Ok(entries) = fs::read_dir(tmp) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(pid) = name.strip_prefix("frust-bench-").and_then(|p| p.parse::<u32>().ok()) else { continue };
        if pid == 0 || pid == std::process::id() || !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        if !sys::pid_alive(pid) {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// Refuses to remove anything not created by this module.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let name = self.0.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if name.starts_with("frust-bench-") && self.0.starts_with(std::env::temp_dir()) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}

struct Ctx {
    cargo: PathBuf,
    fcargo: PathBuf,
    home: PathBuf,
    probe_id: String,
}

fn runs_from_flags(flags: &[String], quick: bool) -> Result<usize, String> {
    let raw = flags.iter().enumerate().find_map(|(i, f)| {
        if f == "--runs" { Some(flags.get(i + 1).cloned().unwrap_or_default()) } else { f.strip_prefix("--runs=").map(str::to_string) }
    });
    match raw {
        None => Ok(if quick { MIN_RUNS } else { DEFAULT_RUNS }),
        Some(v) => v.trim().parse::<usize>().ok().filter(|n| *n >= MIN_RUNS).ok_or_else(|| format!("--runs needs an integer of at least {MIN_RUNS}, got `{v}`")),
    }
}

#[derive(Debug, Serialize)]
struct LinkStats {
    median_ms: f64,
    min_ms: f64,
    max_ms: f64,
}

impl LinkStats {
    fn from_values(mut values: Vec<f64>) -> Self {
        values.sort_by(f64::total_cmp);
        let n = values.len();
        let median_ms = if n.is_multiple_of(2) { (values[n / 2 - 1] + values[n / 2]) / 2.0 } else { values[n / 2] };
        Self { median_ms, min_ms: values[0], max_ms: values[n - 1] }
    }
}

#[derive(Debug, Serialize)]
struct LinkSample {
    edit: usize,
    order_in_pair: usize,
    total_build_ms: u64,
    native_link_ms: f64,
    compiled_units: usize,
    main_bin_compiled_units: usize,
    binary_bytes: u64,
    smart_policy_applied: bool,
    expected_output_present: bool,
}

#[derive(Debug, Serialize)]
struct LinkMode {
    mode: &'static str,
    total_build: LinkStats,
    native_link: LinkStats,
    binary_bytes: u64,
    samples: Vec<LinkSample>,
}

#[derive(Debug, Serialize)]
struct LinkSources {
    local_crates: usize,
    registry_deps: bool,
    edited_source: &'static str,
    original_source_bytes: usize,
    original_source_blake3: String,
    shared_source_path: bool,
    separate_target_directories: bool,
}

#[derive(Debug, Serialize)]
struct LinkReport {
    description: &'static str,
    policy: &'static str,
    timing: &'static str,
    rustc: String,
    machine: Machine,
    runs: usize,
    sources: LinkSources,
    artifact_cache: &'static str,
    build_lock: bool,
    runtime_outputs_equal: bool,
    expected_edit_outputs_present: bool,
    modes: Vec<LinkMode>,
}

// Always derive the next body from the original, retaining the workload without accumulating edits.
fn link_edit_source(original: &str, edit: usize) -> Result<String, String> {
    let signature = "fn main() {";
    let at = original.find(signature).ok_or("link fixture has no main body")? + signature.len();
    Ok(format!(
        "{}\n    println!(\"frust-link-edit {{}}\", std::hint::black_box({edit}usize));{}",
        &original[..at], &original[at..]
    ))
}

// Primary-only profiling avoids replayed dependency timings; reject ambiguous phases.
fn native_link_millis(stderr: &str) -> Result<f64, String> {
    let mut phases = stderr.lines()
        .filter(|line| line.split_whitespace().last() == Some("link_binary"));
    let line = phases.next()
        .ok_or("rustc did not report a link_binary phase with -Ztime-passes")?;
    if phases.next().is_some() {
        return Err("rustc reported multiple link_binary phases; the current main binary timing is ambiguous".into());
    }
    let seconds = line.trim_start().strip_prefix("time:")
        .and_then(|rest| rest.split(';').next())
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v >= 0.0)
        .ok_or("invalid rustc link_binary timing")?;
    Ok(seconds * 1000.0)
}

fn validate_measured_link(compiled_units: usize, main_bin_compiled_units: usize, native_link_ms: f64, total_build_ms: u64) -> Result<(), String> {
    if compiled_units != 1 || main_bin_compiled_units != 1 {
        return Err(format!("linking benchmark measured edit compiled {compiled_units} units ({main_bin_compiled_units} main binaries); timing attribution requires only the main binary to compile"));
    }
    // rustc rounds seconds to three decimals and the wall clock truncates milliseconds.
    if native_link_ms > total_build_ms as f64 + 1.0 {
        return Err(format!("linking benchmark main link timing {native_link_ms:.3}ms exceeds total build wall time {total_build_ms}ms"));
    }
    Ok(())
}

fn link_profile_script(wrapper: &Path) -> Result<String, String> {
    let path = wrapper.to_str().ok_or("linking benchmark wrapper path is not UTF-8")?;
    let quoted = format!("'{}'", path.replace('\'', "'\\''"));
    Ok(format!(
        "#!/bin/sh\nif [ \"${{CARGO_PRIMARY_PACKAGE:-}}\" = 1 ]; then\n    exec {quoted} \"$@\" -Ztime-passes\nfi\nexec {quoted} \"$@\"\n"
    ))
}

fn linking_build(ctx: &Ctx, wrapper: &Path, project: &Path, target: &Path, smart: bool, edit: usize, order: usize) -> Result<(LinkSample, Vec<u8>), String> {
    fs::create_dir_all(&ctx.home).map_err(|e| e.to_string())?;
    fs::write(Dirs::at(&ctx.home).auto_gc_marker(), b"").map_err(|e| e.to_string())?;
    let mut cmd = Command::new(&ctx.fcargo);
    cmd.args(["build", "--offline", "--message-format=json", "--config", "profile.dev.opt-level=0", "--config", "profile.dev.incremental=true"])
        .current_dir(project)
        .env("CARGO_TARGET_DIR", target)
        .env("CARGO_INCREMENTAL", "1")
        .env("CARGO_NET_OFFLINE", "true")
        .env("RUSTC_WRAPPER", wrapper)
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("CARGO_BUILD_TARGET")
        .env("RUSTFLAGS", "")
        .env("RUSTC_BOOTSTRAP", "1")
        .env("FRUST_HOME", &ctx.home)
        .env("FRUST_LINK", if smart { "smart" } else { "off" })
        .env("FRUST_CACHE", "off")
        .env("FRUST_LOCK", "on")
        .env("FRUST_RENDER", "off")
        .env("FRUST_TUNE", "off")
        .env("FRUST_AUTO_COMPACT", "off")
        .env("FRUST_DEBUG", "1")
        .env("FRUST_UPDATE_CHECK", "off")
        .env_remove("FRUST_BYPASS")
        .stdin(Stdio::null());
    let started = Instant::now();
    let output = cmd.output().map_err(|e| e.to_string())?;
    let total_build_ms = started.elapsed().as_millis() as u64;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Err(format!("linking benchmark {} build failed:\n{}", if smart { "smart" } else { "off" }, failure_tail(&stderr)));
    }
    let smart_policy_applied = stderr.lines().any(|l| l.starts_with("frust-wrap: smart link:"));
    if smart != smart_policy_applied {
        return Err(format!("linking benchmark policy evidence missing or unexpected (FRUST_LINK={}); native macOS host linking must be eligible", if smart { "smart" } else { "off" }));
    }
    let mut compiled_units = 0;
    let mut main_bin_compiled_units = 0;
    let mut binary = None;
    for line in output.stdout.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
        let message: serde_json::Value = serde_json::from_slice(line).map_err(|e| format!("invalid Cargo JSON in linking benchmark: {e}"))?;
        if message["reason"] != "compiler-artifact" || message["fresh"] != false {
            continue;
        }
        compiled_units += 1;
        let main = message["target"]["name"] == "app"
            && message["target"]["kind"].as_array().is_some_and(|k| k.iter().any(|v| v == "bin"))
            && message["profile"]["test"] == false;
        if main {
            main_bin_compiled_units += 1;
            binary = message["executable"].as_str().map(PathBuf::from);
        }
    }
    if main_bin_compiled_units == 0 {
        return Err("linking benchmark edit compiled no main binary; refusing a fresh/no-op sample".into());
    }
    let binary = binary.ok_or("Cargo did not report the compiled main executable")?;
    let native_link_ms = native_link_millis(&stderr)?;
    let stdout = run_output(&binary)?;
    let marker = format!("frust-link-edit {edit}");
    let expected_output_present = stdout.split(|b| *b == b'\n').any(|line| line == marker.as_bytes());
    if !expected_output_present {
        return Err(format!("linking benchmark binary did not print expected edit {edit}"));
    }
    let binary_bytes = fs::metadata(&binary).map_err(|e| e.to_string())?.len();
    Ok((LinkSample { edit, order_in_pair: order, total_build_ms, native_link_ms, compiled_units, main_bin_compiled_units, binary_bytes, smart_policy_applied, expected_output_present }, stdout))
}

fn linking_bench(scratch: &Path, quick: bool, runs: usize) -> Result<LinkReport, String> {
    if !cfg!(target_os = "macos") {
        return Err("--linking requires macOS with Apple's native linker; smart linking is unsupported on this platform".into());
    }
    let load_before = load_average();
    fs::create_dir_all(scratch).map_err(|e| e.to_string())?;
    let cargo = resolve_tool("cargo").ok_or("cargo not found")?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let fcargo = fcargo_beside(&exe);
    let actual_wrapper = exe.with_file_name(format!("frust-wrap{}", std::env::consts::EXE_SUFFIX));
    if !fcargo.exists() || !actual_wrapper.exists() {
        return Err("fcargo and frust-wrap must be installed next to frust for --linking".into());
    }
    // A shared wrapper keeps Cargo's compiler-information cache stable between arms.
    let wrapper = scratch.join("link-profile-wrapper");
    fs::write(&wrapper, link_profile_script(&actual_wrapper)?).map_err(|e| e.to_string())?;
    sys::set_path_mode(&wrapper, 0o755).map_err(|e| e.to_string())?;
    let probe = Probe::run(None, scratch).ok_or("could not query rustc")?;
    if !probe.host.ends_with("-apple-darwin") {
        return Err("--linking requires a native Apple Darwin rustc host".into());
    }
    let ctx = Ctx { cargo, fcargo, home: scratch.join("link-home"), probe_id: probe.id };
    let project = scratch.join("linking").join("app");
    let libs = if quick { 4 } else { 12 };
    let registry = write_growth_project(&project, libs, true).and_then(|_| resolve_offline(&ctx, &project)).is_ok();
    if !registry {
        write_growth_project(&project, libs, false)?;
    }
    let source = project.join("src").join("main.rs");
    let original = fs::read_to_string(&source).map_err(|e| e.to_string())?;
    let targets = [scratch.join("link-off-target"), scratch.join("link-smart-target")];
    let mut samples: [Vec<LinkSample>; 2] = [Vec::new(), Vec::new()];
    // Cold build and a nonzero body edit settle both incremental states before measurement.
    for step in 0..runs + 2 {
        fs::write(&source, link_edit_source(&original, step)?).map_err(|e| e.to_string())?;
        let order = if step % 2 == 0 { [0, 1] } else { [1, 0] };
        let mut outputs: [Vec<u8>; 2] = [Vec::new(), Vec::new()];
        for (position, mode) in order.into_iter().enumerate() {
            let (sample, output) = linking_build(&ctx, &wrapper, &project, &targets[mode], mode == 1, step, position + 1)?;
            outputs[mode] = output;
            if step >= 2 {
                validate_measured_link(sample.compiled_units, sample.main_bin_compiled_units, sample.native_link_ms, sample.total_build_ms)?;
                samples[mode].push(sample);
            }
        }
        if outputs[0] != outputs[1] {
            return Err(format!("linking benchmark binaries produced different output for edit {step}"));
        }
        guard_scratch(scratch, max_scratch_bytes())?;
    }
    let modes = samples.into_iter().enumerate().map(|(mode, samples)| LinkMode {
        mode: if mode == 0 { "off" } else { "smart" },
        total_build: LinkStats::from_values(samples.iter().map(|s| s.total_build_ms as f64).collect()),
        native_link: LinkStats::from_values(samples.iter().map(|s| s.native_link_ms).collect()),
        binary_bytes: samples.last().map_or(0, |s| s.binary_bytes),
        samples,
    }).collect();
    Ok(LinkReport {
        description: "Paired real main-function body edits of one application graph; identical source path, separate warmed incremental targets, alternating build order, direct binary execution after every build.",
        policy: "FRUST_LINK=smart selects Apple's native ld directly for eligible host debug binaries. The standard library remains statically linked; this is not native incremental linking.",
        timing: "Total build wall time includes the frust frontend and Cargo; native_link_ms is the single current main-binary link_binary phase, with exactly one main-binary unit compiled per measured edit. Both arms use the same wrapper script, which profiles only the primary application using benchmark-only RUSTC_BOOTSTRAP=1 and -Ztime-passes; dependencies receive no profiling flags. Load and instrumentation can affect results.",
        rustc: probe.release,
        machine: machine_context(load_before),
        runs,
        sources: LinkSources {
            local_crates: libs,
            registry_deps: registry,
            edited_source: "app/src/main.rs",
            original_source_bytes: original.len(),
            original_source_blake3: blake3::hash(original.as_bytes()).to_hex().to_string(),
            shared_source_path: true,
            separate_target_directories: true,
        },
        artifact_cache: "off",
        build_lock: true,
        runtime_outputs_equal: true,
        expected_edit_outputs_present: true,
        modes,
    })
}

fn print_linking(report: &LinkReport, s: &Styler) {
    println!("{}", s.bold(&format!("Native linking: {} paired body edits, {} local crates{}", report.runs, report.sources.local_crates, if report.sources.registry_deps { " + registry dependencies" } else { "" })));
    println!("  {:<8} {:>22} {:>22} {:>10}", "mode", "build ms med [min,max]", "link ms med [min,max]", "binary");
    for mode in &report.modes {
        let stats = |v: &LinkStats| format!("{:.1} [{:.1},{:.1}]", v.median_ms, v.min_ms, v.max_ms);
        println!("  {:<8} {:>22} {:>22} {:>10}", mode.mode, stats(&mode.total_build), stats(&mode.native_link), human_bytes(mode.binary_bytes));
    }
    let m = &report.machine;
    let load = |v: Option<f64>| v.map_or("unavailable".to_string(), |l| format!("{l:.2}"));
    println!("  {}, {} ({} cores), rustc {}; load before {}, after {}", m.os, m.cpu, m.cores, report.rustc, load(m.load_before), load(m.load_after));
    println!("  Outputs match for every edit; main binaries recompiled in every sample.");
    println!("  {}", report.policy);
    println!("  {}", report.timing);
}

// An untimed build of a trivial project per tool and profile, so first-launch costs of rustc, std, the linker and fcargo are not charged to whichever row runs first.
fn warm_up(ctx: &Ctx, scratch: &Path) -> Result<(), String> {
    let dir = scratch.join("warmup");
    let project = dir.join("app");
    write_project(&project, 0, false)?;
    let warm = Ctx { cargo: ctx.cargo.clone(), fcargo: ctx.fcargo.clone(), home: dir.join("home"), probe_id: ctx.probe_id.clone() };
    let target = dir.join("target");
    let profiles: [&[&str]; 2] = [&[], &["--release"]];
    for extra in profiles {
        plain_build_with(&warm, &project, &target, extra)?;
        frust_build_with(&warm, &project, extra)?;
    }
    fs::remove_dir_all(&dir).map_err(|e| e.to_string())
}

struct Row {
    label: &'static str,
    millis: Vec<u64>,
    bytes: u64,
}

#[derive(Default)]
struct Table {
    rows: Vec<Row>,
}

impl Table {
    fn add(&mut self, label: &'static str, run: Run, bytes: Option<u64>) {
        let row = match self.rows.iter().position(|r| r.label == label) {
            Some(i) => &mut self.rows[i],
            None => {
                self.rows.push(Row { label, millis: Vec::new(), bytes: 0 });
                self.rows.last_mut().expect("row was just pushed")
            }
        };
        row.millis.push(run.millis);
        if let Some(b) = bytes {
            row.bytes = b;
        }
    }

    fn stats(&self, label: &str) -> (u64, u64, u64) {
        let Some(row) = self.rows.iter().find(|r| r.label == label) else { return (0, 0, 0) };
        let mut sorted = row.millis.clone();
        let med = median(&mut sorted);
        (med, sorted.first().copied().unwrap_or(0), sorted.last().copied().unwrap_or(0))
    }

    fn median(&self, label: &str) -> u64 {
        self.stats(label).0
    }

    fn measurements(&self) -> Vec<Measurement> {
        self.rows
            .iter()
            .map(|r| {
                let (med, min, max) = self.stats(r.label);
                Measurement { label: r.label.to_string(), millis: med, min_millis: min, max_millis: max, runs: r.millis.len(), target_bytes: r.bytes }
            })
            .collect()
    }
}

fn print_row(m: &Measurement, s: &Styler) {
    println!(
        "  {:<44} {:>9}  {:>10}  {}",
        m.label,
        duration_ms(m.millis),
        human_bytes(m.target_bytes),
        s.dim(&format!("(min {}, max {}, n={})", duration_ms(m.min_millis), duration_ms(m.max_millis), m.runs))
    );
}

fn clear(path: &Path) -> Result<(), String> {
    fsx::remove_any(path).map_err(|e| format!("{}: {e}", path.display()))
}

fn move_dir(from: &Path, to: &Path) -> Result<(), String> {
    clear(to)?;
    sys::rename(from, to).map_err(|e| format!("{}: {e}", from.display()))
}

fn fcargo_beside(exe: &Path) -> PathBuf {
    exe.with_file_name(format!("fcargo{}", std::env::consts::EXE_SUFFIX))
}

fn app_binary(dir: &Path) -> PathBuf {
    dir.join(format!("app{}", std::env::consts::EXE_SUFFIX))
}

fn bench(scratch: &Path, quick: bool, runs: usize, s: &Styler, quiet: bool) -> Result<Report, String> {
    fs::create_dir_all(scratch).map_err(|e| e.to_string())?;
    let cargo = resolve_tool("cargo").ok_or("cargo not found")?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let fcargo = fcargo_beside(&exe);
    if !fcargo.exists() {
        return Err("fcargo binary not found next to frust; install all binaries".into());
    }
    let probe = Probe::run(None, scratch).ok_or("could not query rustc")?;
    let ctx = Ctx { cargo, fcargo, home: scratch.join("frust-home"), probe_id: probe.id.clone() };
    let load_before = load_average();
    warm_up(&ctx, scratch)?;

    let libs = if quick { 4 } else { 12 };
    let proj_a = scratch.join("a").join("app");
    let proj_b = scratch.join("b").join("app");
    let registry = write_project(&proj_a, libs, true).and_then(|_| resolve_offline(&ctx, &proj_a));
    let registry_deps = registry.is_ok();
    if !registry_deps {
        write_project(&proj_a, libs, false)?;
    }
    write_project(&proj_b, libs, registry_deps)?;
    if registry_deps {
        resolve_offline(&ctx, &proj_b)?;
    }
    if !quiet {
        println!("{}", s.dim(&format!("benchmarking {libs} local crates{} on rustc {}", if registry_deps { " + registry deps" } else { "" }, probe.release)));
    }

    guard_disk(scratch)?;

    let rel = ["--release"];
    let (ta, tb) = (scratch.join("cargo-target-a"), scratch.join("cargo-target-b"));
    let (rta, rtb) = (scratch.join("cargo-rel-a"), scratch.join("cargo-rel-b"));
    let dirs = Dirs::at(&ctx.home);
    let ca = workspace::central_target(&dirs, Some(&canonical(&proj_a)), &ctx.probe_id);
    let cb = workspace::central_target(&dirs, Some(&canonical(&proj_b)), &ctx.probe_id);
    let (cargo_same, frust_same) = (scratch.join("cargo-rel-same-dir"), scratch.join("frust-rel-same-dir"));

    let mut t = Table::default();
    let (mut cargo_disk, mut frust_disk, mut frust_du) = (0, 0, 0);
    let (mut identical_cold, mut identical_cargo) = (true, true);
    let mut other_dir: Option<BinaryDiff> = None;
    let acceptable = |d: &BinaryDiff| d.identical || d.only_uuid_and_signature;
    for rep in 0..runs {
        // The tool that goes first alternates so neither one always pays for the other's cold page cache.
        let cargo_first = rep % 2 == 0;
        let order = if cargo_first { [true, false] } else { [false, true] };
        let last = rep + 1 == runs;
        if !quiet {
            println!("{}", s.dim(&format!("  run {} of {runs}, {} first", rep + 1, if cargo_first { "cargo" } else { "frust" })));
        }
        guard_disk(scratch)?;

        for is_cargo in order {
            if is_cargo {
                clear(&ta)?;
                clear(&tb)?;
                t.add(L_CARGO_A, plain_build(&ctx, &proj_a, &ta)?, last.then(|| profile_size(&ta, "debug")));
                t.add(L_CARGO_B, plain_build(&ctx, &proj_b, &tb)?, last.then(|| profile_size(&tb, "debug")));
            } else {
                clear(&ctx.home)?;
                t.add(L_FRUST_A, frust_build(&ctx, &proj_a)?, last.then(|| profile_size(&ca, "debug")));
                t.add(L_FRUST_B, frust_build(&ctx, &proj_b)?, last.then(|| profile_size(&cb, "debug")));
            }
        }
        if last {
            cargo_disk = fsx::physical_size(&[&ta, &tb]);
            frust_disk = fsx::physical_size(&[&ca, &cb, &dirs.store()]);
            frust_du = allocated_size(&ca) + allocated_size(&cb) + allocated_size(&dirs.store());
        }
        guard_disk(scratch)?;

        for is_cargo in order {
            if is_cargo {
                clear(&ta)?;
                t.add(L_CARGO_CLEAN, plain_build(&ctx, &proj_a, &ta)?, last.then(|| profile_size(&ta, "debug")));
            } else {
                clear(&ca)?;
                t.add(L_FRUST_CLEAN, frust_build(&ctx, &proj_a)?, last.then(|| profile_size(&ca, "debug")));
            }
        }
        // The display pipeline is off in every other row; this one shows what turning it on costs.
        clear(&ca)?;
        t.add(L_FRUST_DISPLAY, frust_build_env(&ctx, &proj_a, &[], "plain")?, last.then(|| profile_size(&ca, "debug")));

        for is_cargo in order {
            if is_cargo {
                t.add(L_CARGO_NOOP, plain_build(&ctx, &proj_a, &ta)?, None);
            } else {
                t.add(L_FRUST_NOOP, frust_build(&ctx, &proj_a)?, None);
            }
        }

        touch_app(&proj_a, rep as u32 + 1)?;
        for is_cargo in order {
            if is_cargo {
                t.add(L_CARGO_INC, plain_build(&ctx, &proj_a, &ta)?, None);
            } else {
                t.add(L_FRUST_INC, frust_build(&ctx, &proj_a)?, None);
            }
        }

        // Measure warm-session edits separately from the first edit after artifact restoration.
        touch_app(&proj_a, rep as u32 + 10_000)?;
        for is_cargo in order {
            if is_cargo {
                t.add(L_CARGO_INC_WARM, plain_build(&ctx, &proj_a, &ta)?, None);
            } else {
                t.add(L_FRUST_INC_WARM, frust_build(&ctx, &proj_a)?, None);
            }
        }

        if last {
            let (gc_ms, compact_ms) = measure_maintenance(&ctx, &dirs, &ca)?;
            t.add(L_MAINT_GC, Run { millis: gc_ms }, None);
            t.add(L_MAINT_COMPACT, Run { millis: compact_ms }, None);
        }

        let mut cold_hash = None;
        for is_cargo in order {
            if is_cargo {
                clear(&rta)?;
                clear(&rtb)?;
                t.add(L_REL_CARGO_A, plain_build_with(&ctx, &proj_a, &rta, &rel)?, last.then(|| profile_size(&rta, "release")));
                t.add(L_REL_CARGO_B, plain_build_with(&ctx, &proj_b, &rtb, &rel)?, last.then(|| profile_size(&rtb, "release")));
            } else {
                clear(&ctx.home)?;
                t.add(L_REL_FRUST_A, frust_build_with(&ctx, &proj_a, &rel)?, last.then(|| profile_size(&ca, "release")));
                cold_hash = fsx_hash(&app_binary(&ca.join("release")));
                t.add(L_REL_FRUST_B, frust_build_with(&ctx, &proj_b, &rel)?, last.then(|| profile_size(&cb, "release")));
            }
        }
        guard_disk(scratch)?;

        // Plain cargo builds into frust's central directory so both see the same CARGO_TARGET_DIR path.
        let (mut cargo_hash, mut restored_hash) = (None, None);
        for is_cargo in order {
            clear(&ca.join("release"))?;
            if is_cargo {
                t.add(L_REL_CARGO_CLEAN, plain_build_with(&ctx, &proj_a, &ca, &rel)?, last.then(|| profile_size(&ca, "release")));
                move_dir(&ca.join("release"), &cargo_same)?;
                cargo_hash = fsx_hash(&app_binary(&cargo_same));
            } else {
                t.add(L_REL_FRUST_CLEAN, frust_build_with(&ctx, &proj_a, &rel)?, last.then(|| profile_size(&ca, "release")));
                move_dir(&ca.join("release"), &frust_same)?;
                restored_hash = fsx_hash(&app_binary(&frust_same));
            }
        }
        identical_cold &= cold_hash.is_some() && cold_hash == restored_hash;
        identical_cargo &= cargo_hash.is_some() && cargo_hash == restored_hash;
        let diff = binary_diff(&app_binary(&rta.join("release")), &app_binary(&frust_same))?;
        if other_dir.as_ref().is_none_or(acceptable) {
            other_dir = Some(diff);
        }
    }

    let runtime_runs = if quick { 10 } else { RUNTIME_RUNS };
    let runtime = compare_runtime(&app_binary(&cargo_same), &app_binary(&frust_same), runtime_runs)?;
    let cases = t.measurements();
    if !quiet {
        println!();
        println!("{}", s.bold(&format!("Timed rows: median of {runs} runs, spread is min and max")));
        for m in &cases {
            print_row(m, s);
        }
        println!("  {:<44} {:>9}", format!("cargo      release binary, median of {runtime_runs}"), micros(runtime.cargo_us));
        println!("  {:<44} {:>9}", format!("frust      release binary, median of {runtime_runs}"), micros(runtime.frust_us));
    }

    let ratio = |a: u64, b: u64| if b == 0 { 0.0 } else { a as f64 / b as f64 };
    Ok(Report {
        rustc: probe.release,
        machine: machine_context(load_before),
        runs,
        registry_deps,
        local_crates: libs,
        cases,
        cargo_two_projects_disk: cargo_disk,
        frust_two_projects_disk: frust_disk,
        frust_two_projects_du: frust_du,
        speedup_second_project: ratio(t.median(L_CARGO_B), t.median(L_FRUST_B)),
        speedup_clean_rebuild: ratio(t.median(L_CARGO_CLEAN), t.median(L_FRUST_CLEAN)),
        noop_overhead_ms: t.median(L_FRUST_NOOP) as i64 - t.median(L_CARGO_NOOP) as i64,
        release_speedup_second_project: ratio(t.median(L_REL_CARGO_B), t.median(L_REL_FRUST_B)),
        release_speedup_clean_rebuild: ratio(t.median(L_REL_CARGO_CLEAN), t.median(L_REL_FRUST_CLEAN)),
        release_binary_identical_cold_vs_restored: identical_cold,
        release_binary_identical_to_cargo: identical_cargo,
        release_binary_other_dir: other_dir.ok_or("no release comparison ran")?,
        runtime_runs,
        runtime_median_us_cargo: runtime.cargo_us,
        runtime_median_us_frust: runtime.frust_us,
        runtime_outputs_equal: runtime.outputs_equal,
        churn: None,
    })
}

// Run once on the populated state, outside the speed rows, which turn both inline passes off.
fn measure_maintenance(ctx: &Ctx, dirs: &Dirs, target: &Path) -> Result<(u64, u64), String> {
    let mut cfg = FrustConfig::from_env();
    cfg.gc_sync = true;
    clear(&dirs.auto_gc_marker())?;
    let started = Instant::now();
    let ran = gc::maybe_auto_gc(dirs, &cfg);
    let gc_ms = started.elapsed().as_millis() as u64;
    if !ran {
        return Err(format!("the daily gc pass did not run in {}", ctx.home.display()));
    }
    let mut opts = AutoOptions::from_env(&cfg);
    opts.enabled = true;
    opts.every = Duration::ZERO;
    opts.min_growth = 0;
    opts.pass_budget = Duration::from_secs(3600);
    opts.pass_bytes = u64::MAX;
    let started = Instant::now();
    compact::auto_compact_with(dirs, target, &opts);
    Ok((gc_ms, started.elapsed().as_millis() as u64))
}

fn load_average() -> Option<f64> {
    sys::load_average()
}

#[cfg(unix)]
fn machine_os_cpu() -> (String, String) {
    let run = |program: &str, args: &[&str]| -> Option<String> {
        let out = Command::new(program).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|t| !t.is_empty())
    };
    let os = run("uname", &["-srm"]).unwrap_or_else(|| format!("{} {}", std::env::consts::OS, std::env::consts::ARCH));
    let cpu = run("sysctl", &["-n", "machdep.cpu.brand_string"])
        .or_else(|| {
            fs::read_to_string("/proc/cpuinfo").ok().and_then(|t| {
                t.lines().find_map(|l| l.strip_prefix("model name").and_then(|r| r.split_once(':')).map(|(_, v)| v.trim().to_string()))
            })
        })
        .unwrap_or_else(|| "unknown".to_string());
    (os, cpu)
}

#[cfg(not(unix))]
fn machine_os_cpu() -> (String, String) {
    let os = format!("{} {}", std::env::consts::OS, std::env::consts::ARCH);
    let cpu = std::env::var("PROCESSOR_IDENTIFIER").ok().filter(|v| !v.trim().is_empty()).unwrap_or_else(|| "unknown".to_string());
    (os, cpu)
}

fn machine_cores() -> usize {
    let from_env = || std::env::var("NUMBER_OF_PROCESSORS").ok()?.trim().parse::<usize>().ok().filter(|n| *n > 0);
    if cfg!(windows)
        && let Some(n) = from_env()
    {
        return n;
    }
    thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
}

fn machine_context(load_before: Option<f64>) -> Machine {
    let (os, cpu) = machine_os_cpu();
    Machine { os, cpu, cores: machine_cores(), load_before, load_after: load_average() }
}

fn failed_checks(r: &Report) -> Option<String> {
    let mut failed = Vec::new();
    if !r.release_binary_identical_cold_vs_restored {
        failed.push("release binary restored from the cache differs from the cold build");
    }
    if !r.release_binary_identical_to_cargo {
        failed.push("release binary differs from plain cargo in the same target directory");
    }
    if !r.runtime_outputs_equal {
        failed.push("release binaries print different output");
    }
    (!failed.is_empty()).then(|| failed.join("; "))
}

struct Config {
    label: &'static str,
    args: &'static [&'static str],
    rustflags: Option<&'static str>,
    serde: Option<&'static str>,
}

const CHURN: [Config; 6] = [
    Config { label: "debug", args: &[], rustflags: None, serde: None },
    Config { label: "debug, feature toggled on", args: &["--features", "extra"], rustflags: None, serde: None },
    Config { label: "debug, RUSTFLAGS=-Copt-level=1", args: &["--features", "extra"], rustflags: Some("-Copt-level=1"), serde: None },
    Config { label: "release", args: &["--release", "--features", "extra"], rustflags: None, serde: None },
    Config { label: "debug, serde 1.0.228 (dependency change)", args: &["--features", "extra"], rustflags: None, serde: Some("1.0.228") },
    Config { label: "debug, feature toggled off", args: &[], rustflags: None, serde: None },
];

const EDITS: usize = 20;

fn churn_bench(scratch: &Path, quick: bool, s: &Styler, quiet: bool) -> Result<ChurnReport, String> {
    fs::create_dir_all(scratch).map_err(|e| e.to_string())?;
    let cargo = resolve_tool("cargo").ok_or("cargo not found")?;
    let fcargo = fcargo_beside(&std::env::current_exe().map_err(|e| e.to_string())?);
    let probe = Probe::run(None, scratch).ok_or("could not query rustc")?;
    let ctx = Ctx { cargo, fcargo, home: scratch.join("frust-home"), probe_id: probe.id.clone() };
    let libs = if quick { 3 } else { 6 };
    let proj = scratch.join("churn").join("app");
    let registry = write_project(&proj, libs, true).and_then(|_| resolve_offline(&ctx, &proj)).is_ok();
    if !registry {
        write_project(&proj, libs, false)?;
    }
    let cargo_target = scratch.join("cargo-churn-target");
    let dirs = Dirs::at(&ctx.home);
    let central = workspace::central_target(&dirs, Some(&canonical(&proj)), &ctx.probe_id);
    // Every build is a compaction pass here, showing the steady state the defaults converge to.
    let auto_ctx = Ctx { cargo: ctx.cargo.clone(), fcargo: ctx.fcargo.clone(), home: scratch.join("frust-auto-home"), probe_id: ctx.probe_id.clone() };
    let auto_dirs = Dirs::at(&auto_ctx.home);
    let auto_central = workspace::central_target(&auto_dirs, Some(&canonical(&proj)), &ctx.probe_id);
    let mut auto_opts = AutoOptions::from_env(&FrustConfig::from_env());
    auto_opts.enabled = true;
    auto_opts.every = Duration::ZERO;
    auto_opts.min_growth = 0;
    let auto_build = |args: &[&str], envs: &[(&str, &str)]| -> Result<usize, String> {
        let (_, compiled) = build_logged(&auto_ctx, true, &proj, &auto_central, args, envs)?;
        compact::auto_compact_with(&auto_dirs, &auto_central, &auto_opts);
        Ok(compiled)
    };
    if !quiet {
        println!();
        println!("{}", s.bold("Churn: one target directory through six configuration changes"));
    }
    let mut configs = Vec::new();
    for config in &CHURN {
        if let Some(version) = config.serde
            && (!registry || !pin_serde(&ctx, &proj, version))
        {
            continue;
        }
        guard_disk(scratch)?;
        let envs: Vec<(&str, &str)> = config.rustflags.map(|f| vec![("RUSTFLAGS", f)]).unwrap_or_default();
        let (c, _) = build_logged(&ctx, false, &proj, &cargo_target, config.args, &envs)?;
        let (f, _) = build_logged(&ctx, true, &proj, &central, config.args, &envs)?;
        auto_build(config.args, &envs)?;
        if !quiet {
            println!(
                "  {:<44} cargo {:>9} {:>10}   frust {:>9} {:>10}",
                config.label,
                duration_ms(c.millis),
                human_bytes(compact::physical_size(&[&cargo_target])),
                duration_ms(f.millis),
                human_bytes(compact::physical_size(&[&central, &dirs.store()]))
            );
        }
        configs.push(config.label.to_string());
    }
    let last = CHURN.iter().rfind(|c| configs.iter().any(|l| l == c.label)).ok_or("no configuration built")?;
    let auto_after_churn = compact::physical_size(&[&auto_central, &auto_dirs.store()]);
    let profile = if last.args.contains(&"--release") { "release" } else { "debug" };
    let output_before = run_output(&app_binary(&central.join(profile)))?;

    let cargo_size = compact::physical_size(&[&cargo_target]);
    let frust_target = compact::physical_size(&[&central]);
    let frust_store = compact::physical_size(&[&dirs.store()]);
    let frust_accumulated = compact::physical_size(&[&central, &dirs.store()]);
    let breakdown_before = rows(&compact::breakdown(&central));

    let mut opts = GcOptions::new(u64::MAX);
    opts.variant_max_age = Duration::ZERO;
    opts.variants_to_keep = 1;
    let plan = gc::plan(&dirs, &opts);
    let (gc_variant_paths, gc_variant_bytes) = plan.by_class(gc::Class::StaleVariant);
    let (gc_sessions, gc_session_bytes) = plan.by_class(gc::Class::IncrementalSession);
    let _ = gc::execute(&dirs, &plan);
    let frust_after_gc = compact::physical_size(&[&central, &dirs.store()]);

    let report = compact::compact(&dirs, std::slice::from_ref(&central), &CompactOptions::full());
    let frust_after_compact = compact::physical_size(&[&central, &dirs.store()]);
    let breakdown_after = rows(&compact::breakdown(&central));
    let envs: Vec<(&str, &str)> = last.rustflags.map(|f| vec![("RUSTFLAGS", f)]).unwrap_or_default();
    let (_, recompiled) = build_logged(&ctx, true, &proj, &central, last.args, &envs)?;
    let output_after = run_output(&app_binary(&central.join(profile)))?;

    let auto_compiled_on_repeat = auto_build(last.args, &envs)?;
    guard_disk(scratch)?;

    let rel: &[&str] = &["--release", "--features", "extra"];
    build_logged(&ctx, false, &proj, &cargo_target, rel, &[])?;
    build_logged(&ctx, true, &proj, &central, rel, &[])?;
    compact::compact(&dirs, std::slice::from_ref(&central), &CompactOptions::after_build());
    fsx::remove_any(&cargo_target.join("release")).map_err(|e| e.to_string())?;
    fsx::remove_any(&central.join("release")).map_err(|e| e.to_string())?;
    let (rel_cargo, _) = build_logged(&ctx, false, &proj, &cargo_target, rel, &[])?;
    let hits_before = crate::stats::totals(&crate::stats::read_ledger(&dirs));
    let (rel_frust, rel_frust_compiled) = build_logged(&ctx, true, &proj, &central, rel, &[])?;
    let hits_after = crate::stats::totals(&crate::stats::read_ledger(&dirs));
    guard_disk(scratch)?;

    let base: &[&str] = &[];
    build_logged(&ctx, false, &proj, &cargo_target, base, &[])?;
    build_logged(&ctx, true, &proj, &central, base, &[])?;
    let (cargo_incremental_before, cargo_sessions_before) = incremental_of(&cargo_target.join("debug"));
    let (frust_incremental_before, frust_sessions_before) = incremental_of(&central.join("debug"));
    auto_build(base, &[])?;
    for i in 0..EDITS {
        touch_app(&proj, 100 + i as u32)?;
        build_logged(&ctx, false, &proj, &cargo_target, base, &[])?;
        build_logged(&ctx, true, &proj, &central, base, &[])?;
        auto_build(base, &[])?;
    }
    let auto_after_edits = compact::physical_size(&[&auto_central, &auto_dirs.store()]);
    let cargo_after_edits = compact::physical_size(&[&cargo_target]);
    let (cargo_incremental, cargo_sessions) = incremental_of(&cargo_target.join("debug"));
    let (frust_incremental, frust_sessions) = incremental_of(&central.join("debug"));
    compact::compact(&dirs, std::slice::from_ref(&central), &CompactOptions::full());
    let (frust_incremental_after, frust_sessions_after) = incremental_of(&central.join("debug"));
    let frust_final = compact::physical_size(&[&central, &dirs.store()]);

    // Last, because it changes plain cargo's directory; cargo must still find everything fresh afterwards.
    compact::compact(&dirs, std::slice::from_ref(&cargo_target), &CompactOptions { include_store: false, ..CompactOptions::after_build() });
    let cargo_compressed = compact::physical_size(&[&cargo_target]);
    let (_, cargo_recompiled) = build_logged(&ctx, false, &proj, &cargo_target, base, &[])?;
    guard_disk(scratch)?;

    let growth_opts = GrowthOptions {
        edits: if quick { 30 } else { 100 },
        every: if quick { 10 } else { 20 },
        libs: libs.max(2),
        variants: vec![GrowthVariant::Cargo, GrowthVariant::Auto],
        max_scratch: max_scratch_bytes(),
    };
    let growth = growth_bench(&scratch.join("growth-loop"), &growth_opts, s, quiet)?;

    Ok(ChurnReport {
        configs,
        cargo_target: cargo_size,
        cargo_target_compressed: cargo_compressed,
        cargo_compiled_after_compress: cargo_recompiled,
        frust_final_compacted: frust_final,
        frust_accumulated,
        frust_target,
        frust_store,
        gc_variant_paths,
        gc_variant_bytes,
        gc_sessions,
        gc_session_bytes,
        frust_after_gc,
        frust_after_compact,
        dead_units: report.dead_units,
        stale_sessions: report.stale_sessions,
        compressed_files: report.compressed_files,
        deduped_files: report.deduped_files,
        frust_compiled_after_compact: recompiled,
        binary_output_unchanged: output_before == output_after,
        breakdown_before,
        breakdown_after,
        edits: EDITS,
        cargo_incremental_before,
        cargo_sessions_before,
        frust_incremental_before,
        frust_sessions_before,
        cargo_incremental,
        cargo_sessions,
        frust_incremental,
        frust_sessions,
        frust_incremental_after_compact: frust_incremental_after,
        frust_sessions_after_compact: frust_sessions_after,
        auto_after_churn,
        auto_after_edits,
        cargo_after_edits,
        auto_compiled_on_repeat,
        release_clean_cargo_ms: rel_cargo.millis,
        release_clean_frust_ms: rel_frust.millis,
        release_clean_frust_compiled: rel_frust_compiled,
        release_clean_frust_cache_hits: hits_after.hits - hits_before.hits,
        release_clean_frust_cache_misses: hits_after.misses - hits_before.misses,
        growth: Some(growth),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum GrowthVariant {
    Cargo,
    Frust,
    Auto,
}

impl GrowthVariant {
    fn label(self) -> &'static str {
        match self {
            GrowthVariant::Cargo => "cargo",
            GrowthVariant::Frust => "frust",
            GrowthVariant::Auto => "frust+auto",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct GrowthRow {
    pub variant: GrowthVariant,
    pub edits: usize,
    pub target: u64,
    pub store: u64,
    pub incremental_dir: u64,
    pub anatomy: compact::IncrementalAnatomy,
    pub crate_dirs: Vec<(String, u64)>,
    pub compiled: usize,
    pub build_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct GrowthReport {
    pub libs: usize,
    pub registry_deps: bool,
    pub edits: usize,
    pub every: usize,
    pub rows: Vec<GrowthRow>,
    pub repeat_compiled: Vec<(GrowthVariant, usize)>,
    pub outputs: Vec<(GrowthVariant, String)>,
    pub stopped: Vec<(GrowthVariant, String)>,
}

#[derive(Debug, Clone)]
pub struct GrowthOptions {
    pub edits: usize,
    pub every: usize,
    pub libs: usize,
    pub variants: Vec<GrowthVariant>,
    /// Bytes; from `FRUST_BENCH_MAX_SCRATCH_MIB`, default 4096 MiB.
    pub max_scratch: u64,
}

impl GrowthOptions {
    pub fn from_flags(flags: &[String], quick: bool) -> GrowthOptions {
        let value = |name: &str| flags.iter().find_map(|f| f.strip_prefix(&format!("--{name}=")).map(str::to_string));
        let num = |name: &str, default: usize| value(name).and_then(|v| v.parse().ok()).filter(|n: &usize| *n > 0).unwrap_or(default);
        let variants = value("variants")
            .map(|v| {
                v.split(',')
                    .filter_map(|n| match n.trim() {
                        "cargo" => Some(GrowthVariant::Cargo),
                        "frust" => Some(GrowthVariant::Frust),
                        "auto" | "frust+auto" => Some(GrowthVariant::Auto),
                        _ => None,
                    })
                    .collect()
            })
            .filter(|v: &Vec<GrowthVariant>| !v.is_empty())
            .unwrap_or_else(|| vec![GrowthVariant::Cargo, GrowthVariant::Frust, GrowthVariant::Auto]);
        GrowthOptions {
            edits: num("edits", if quick { 40 } else { 200 }),
            every: num("every", if quick { 10 } else { 20 }),
            libs: num("libs", if quick { 4 } else { 12 }).max(2),
            variants,
            max_scratch: max_scratch_bytes(),
        }
    }
}

fn max_scratch_bytes() -> u64 {
    std::env::var("FRUST_BENCH_MAX_SCRATCH_MIB").ok().and_then(|v| v.trim().parse::<u64>().ok()).filter(|m| *m > 0).unwrap_or(4096) * 1024 * 1024
}

/// Physical bytes, so clones of cache entries in target directories are counted once.
fn guard_scratch(scratch: &Path, max: u64) -> Result<(), String> {
    let used = compact::physical_size(&[scratch]);
    if used > max {
        return Err(format!("scratch directory reached {} (budget {}, FRUST_BENCH_MAX_SCRATCH_MIB)", human_bytes(used), human_bytes(max)));
    }
    Ok(())
}

/// Every 40th step restores the snapshot taken 20 steps earlier; other steps edit `lib0`, `lib1` or the app in turn, alternating body and signature edits.
struct GrowthScript {
    project: PathBuf,
    libs: usize,
    snapshot: Vec<(PathBuf, String)>,
    features: bool,
}

impl GrowthScript {
    fn sources(&self) -> Vec<PathBuf> {
        let mut out = vec![self.project.join("src").join("main.rs")];
        out.extend((0..self.libs).map(|i| self.project.join("libs").join(format!("lib{i}")).join("src").join("lib.rs")));
        out
    }

    fn step(&mut self, step: usize) -> Result<(), String> {
        let io = |e: std::io::Error| e.to_string();
        if step.is_multiple_of(25) {
            self.features = !self.features;
        }
        if step % 40 == 20 {
            self.snapshot = self.sources().into_iter().map(|p| fs::read_to_string(&p).map(|t| (p, t))).collect::<Result<_, _>>().map_err(io)?;
        }
        if step.is_multiple_of(40) && !self.snapshot.is_empty() {
            for (path, text) in &self.snapshot {
                fs::write(path, text).map_err(io)?;
            }
            return Ok(());
        }
        let file = match step % 3 {
            1 => self.project.join("libs").join("lib0").join("src").join("lib.rs"),
            2 => self.project.join("libs").join("lib1").join("src").join("lib.rs"),
            _ => self.project.join("src").join("main.rs"),
        };
        let signature = (step / 3) % 2 == 1;
        let text = fs::read_to_string(&file).map_err(io)?;
        let mut out = String::with_capacity(text.len() + 16);
        for line in text.lines() {
            if !signature && line.starts_with("fn tweak(") || !signature && line.starts_with("pub fn tweak(") {
                let vis = if line.starts_with("pub ") { "pub " } else { "" };
                let _ = writeln!(out, "{vis}fn tweak(x: u64) -> u64 {{ x ^ {step} }}");
            } else if signature && line.starts_with("pub fn sig_") {
                let _ = writeln!(out, "pub fn sig_{step}(x: u64) -> u64 {{ x }}");
            } else {
                out.push_str(line);
                out.push('\n');
            }
        }
        fs::write(&file, out).map_err(io)
    }

    fn args(&self) -> &'static [&'static str] {
        if self.features { &["--features", "extra"] } else { &[] }
    }
}

fn growth_bench(scratch: &Path, opts: &GrowthOptions, s: &Styler, quiet: bool) -> Result<GrowthReport, String> {
    fs::create_dir_all(scratch).map_err(|e| e.to_string())?;
    let cargo = resolve_tool("cargo").ok_or("cargo not found")?;
    let fcargo = fcargo_beside(&std::env::current_exe().map_err(|e| e.to_string())?);
    if !fcargo.exists() {
        return Err("fcargo binary not found next to frust; install all binaries".into());
    }
    let probe = Probe::run(None, scratch).ok_or("could not query rustc")?;
    let proj = scratch.join("growth").join("app");
    let registry = write_growth_project(&proj, opts.libs, true)
        .and_then(|_| resolve_offline(&Ctx { cargo: cargo.clone(), fcargo: fcargo.clone(), home: scratch.join("unused"), probe_id: probe.id.clone() }, &proj))
        .is_ok();
    let mut report = GrowthReport {
        libs: opts.libs,
        registry_deps: registry,
        edits: opts.edits,
        every: opts.every,
        rows: Vec::new(),
        repeat_compiled: Vec::new(),
        outputs: Vec::new(),
        stopped: Vec::new(),
    };
    if !quiet {
        println!();
        println!(
            "{}",
            s.bold(&format!(
                "Incremental growth: {} local crates{}, {} edits (bottom/middle/app crate, body and signature edits, feature toggles, reverts)",
                opts.libs,
                if registry { " + registry deps" } else { "" },
                opts.edits
            ))
        );
        println!(
            "  {:<11} {:>5} {:>10} {:>10} {:>9} {:>5} {:>10} {:>10} {:>10} {:>10} {:>9}",
            "variant", "edits", "target", "incr dir", "sessions", "dirs", "stale objs", "stale sess", "dead dirs", "store", "compiled"
        );
    }
    for &variant in &opts.variants {
        // Each variant replays the same edit script on a fresh project and removes its directories afterwards.
        let project_parent = scratch.join("growth");
        if project_parent.exists() {
            fs::remove_dir_all(&project_parent).map_err(|e| e.to_string())?;
        }
        write_growth_project(&proj, opts.libs, registry)?;
        let home = scratch.join(format!("growth-home-{}", variant.label().replace('+', "-")));
        let ctx = Ctx { cargo: cargo.clone(), fcargo: fcargo.clone(), home: home.clone(), probe_id: probe.id.clone() };
        if registry {
            resolve_offline(&ctx, &proj)?;
        }
        let dirs = Dirs::at(&home);
        let target = match variant {
            GrowthVariant::Cargo => scratch.join("growth-cargo-target"),
            _ => workspace::central_target(&dirs, Some(&canonical(&proj)), &probe.id),
        };
        let mut run = GrowthRun::new(&ctx, variant, &proj, &target, &dirs);
        let mut script = GrowthScript { project: proj.clone(), libs: opts.libs, snapshot: Vec::new(), features: false };
        let mut outcome: Result<(), String> = run.build(script.args()).map(|_| ());
        let mut step = 0;
        while outcome.is_ok() {
            if step % opts.every == 0 || step == opts.edits {
                let row = run.row(step);
                if !quiet {
                    print_growth_row(&row);
                }
                report.rows.push(row);
            }
            if step == opts.edits {
                break;
            }
            outcome = guard_scratch(scratch, opts.max_scratch).and_then(|_| {
                step += 1;
                script.step(step)?;
                run.build(script.args()).map(|_| ())
            });
        }
        match outcome {
            Ok(()) => {
                report.repeat_compiled.push((variant, run.build(script.args())?));
                let bin = app_binary(&target.join("debug"));
                report.outputs.push((variant, String::from_utf8_lossy(&run_output(&bin)?).trim().to_string()));
            }
            Err(msg) => {
                if !quiet {
                    println!("  {}", s.yellow(&format!("{} stopped after {step} edits: {msg}", variant.label())));
                }
                report.stopped.push((variant, format!("after {step} edits: {msg}")));
            }
        }
        if home.exists() {
            fs::remove_dir_all(&home).map_err(|e| e.to_string())?;
        }
        if target.exists() {
            fs::remove_dir_all(&target).map_err(|e| e.to_string())?;
        }
    }
    Ok(report)
}

struct GrowthRun<'a> {
    ctx: &'a Ctx,
    variant: GrowthVariant,
    project: &'a Path,
    target: &'a Path,
    dirs: &'a Dirs,
    auto: AutoOptions,
    compiled: usize,
    build_ms: u64,
}

impl<'a> GrowthRun<'a> {
    fn new(ctx: &'a Ctx, variant: GrowthVariant, project: &'a Path, target: &'a Path, dirs: &'a Dirs) -> GrowthRun<'a> {
        let mut auto = AutoOptions::from_env(&FrustConfig::from_env());
        auto.enabled = true;
        auto.every = Duration::ZERO;
        auto.min_growth = 0;
        GrowthRun { ctx, variant, project, target, dirs, auto, compiled: 0, build_ms: 0 }
    }

    /// fcargo's own automatic compaction is off here so only the variant's policy applies.
    fn build(&mut self, args: &[&str]) -> Result<usize, String> {
        let frust = self.variant != GrowthVariant::Cargo;
        let (run, n) = build_logged(self.ctx, frust, self.project, self.target, args, &[("FRUST_AUTO_COMPACT", "off")])?;
        if self.variant == GrowthVariant::Auto {
            compact::auto_compact_with(self.dirs, self.target, &self.auto);
        }
        self.compiled += n;
        self.build_ms += run.millis;
        Ok(n)
    }

    fn row(&self, edits: usize) -> GrowthRow {
        let profile = self.target.join("debug");
        let incremental = profile.join("incremental");
        let crate_dirs = ["lib0", "lib1", "app"]
            .iter()
            .map(|name| {
                let bytes = fs::read_dir(&incremental)
                    .map(|rd| {
                        rd.flatten()
                            .filter(|e| e.file_name().to_string_lossy().rsplit_once('-').is_some_and(|(c, _)| c == *name))
                            .map(|e| allocated_size(&e.path()))
                            .sum()
                    })
                    .unwrap_or(0);
                (name.to_string(), bytes)
            })
            .collect();
        let store = if self.variant == GrowthVariant::Cargo { 0 } else { compact::physical_size(&[&self.dirs.store()]) };
        GrowthRow {
            variant: self.variant,
            edits,
            target: compact::physical_size(&[self.target]),
            store,
            incremental_dir: compact::physical_size(&[&incremental]),
            anatomy: compact::incremental_anatomy(&profile),
            crate_dirs,
            compiled: self.compiled,
            build_ms: self.build_ms,
        }
    }
}

fn print_growth_row(r: &GrowthRow) {
    let a = &r.anatomy;
    println!(
        "  {:<11} {:>5} {:>10} {:>10} {:>9} {:>5} {:>10} {:>10} {:>10} {:>10} {:>9}",
        r.variant.label(),
        r.edits,
        human_bytes(r.target),
        human_bytes(r.incremental_dir),
        a.sessions,
        a.crate_dirs,
        human_bytes(a.deps_objects_stale),
        human_bytes(a.stale_sessions),
        human_bytes(a.dead_dir_bytes),
        human_bytes(r.store),
        r.compiled
    );
}

fn print_growth(g: &GrowthReport, s: &Styler) {
    println!();
    println!("{}", s.bold(&format!("Growth after {} edits", g.edits)));
    let last = |v: GrowthVariant| g.rows.iter().rfind(|r| r.variant == v);
    let first = |v: GrowthVariant| g.rows.iter().find(|r| r.variant == v);
    for v in [GrowthVariant::Cargo, GrowthVariant::Frust, GrowthVariant::Auto] {
        let (Some(a), Some(b)) = (first(v), last(v)) else { continue };
        println!(
            "  {:<11} target {} -> {} after {} edits (incremental state {} -> {}), {} units compiled in {}",
            v.label(),
            human_bytes(a.target),
            human_bytes(b.target),
            b.edits,
            human_bytes(a.anatomy.total()),
            human_bytes(b.anatomy.total()),
            b.compiled,
            duration_ms(b.build_ms)
        );
    }
    for (v, n) in &g.repeat_compiled {
        println!("  {}", s.dim(&format!("{}: repeating the last build recompiled {n} units", v.label())));
    }
    let outputs: std::collections::HashSet<&str> = g.outputs.iter().map(|o| o.1.as_str()).collect();
    if outputs.len() > 1 {
        println!("  {}", s.yellow(&format!("final outputs differ between variants: {:?}", g.outputs)));
    }
    for (v, why) in &g.stopped {
        println!("  {}", s.yellow(&format!("{} stopped {why}", v.label())));
    }
}

/// Incremental state is split because rustc reads only each crate's newest session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum LatencyCategory {
    NewestSessions,
    Objects,
    BuildOutputs,
    Fingerprint,
    DepInfo,
    Executables,
    Rmeta,
    Rlib,
    Dylib,
}

impl LatencyCategory {
    const ALL: [LatencyCategory; 9] = [
        LatencyCategory::NewestSessions,
        LatencyCategory::Objects,
        LatencyCategory::BuildOutputs,
        LatencyCategory::Fingerprint,
        LatencyCategory::DepInfo,
        LatencyCategory::Executables,
        LatencyCategory::Rmeta,
        LatencyCategory::Rlib,
        LatencyCategory::Dylib,
    ];

    fn label(self) -> &'static str {
        match self {
            LatencyCategory::NewestSessions => "newest incremental sessions",
            LatencyCategory::Objects => "deps .o (debug info)",
            LatencyCategory::BuildOutputs => "build scripts + out",
            LatencyCategory::Fingerprint => ".fingerprint",
            LatencyCategory::DepInfo => "dep-info .d",
            LatencyCategory::Executables => "deps executables",
            LatencyCategory::Rmeta => "deps .rmeta",
            LatencyCategory::Rlib => "deps .rlib",
            LatencyCategory::Dylib => "proc-macro dylibs",
        }
    }

    fn key(self) -> &'static str {
        match self {
            LatencyCategory::NewestSessions => "sessions",
            LatencyCategory::Objects => "objects",
            LatencyCategory::BuildOutputs => "build",
            LatencyCategory::Fingerprint => "fingerprint",
            LatencyCategory::DepInfo => "depinfo",
            LatencyCategory::Executables => "executables",
            LatencyCategory::Rmeta => "rmeta",
            LatencyCategory::Rlib => "rlib",
            LatencyCategory::Dylib => "dylib",
        }
    }

    fn matches(self, cat: Category) -> bool {
        matches!(
            (self, cat),
            (LatencyCategory::NewestSessions, Category::Incremental)
                | (LatencyCategory::Objects, Category::Objects)
                | (LatencyCategory::BuildOutputs, Category::BuildScripts)
                | (LatencyCategory::Fingerprint, Category::Fingerprint)
                | (LatencyCategory::DepInfo, Category::DepInfo)
                | (LatencyCategory::Executables, Category::DepsExecutables)
                | (LatencyCategory::Rmeta, Category::Rmeta)
                | (LatencyCategory::Rlib, Category::Rlib)
                | (LatencyCategory::Dylib, Category::Dylib)
        )
    }
}

/// Regular, single-link files of at least [`compact::MIN_FILE_BYTES`]; for incremental state, only newest sessions.
fn latency_files(profile: &Path, cat: LatencyCategory) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let newest: Vec<PathBuf> = fs::read_dir(profile.join("incremental"))
        .map(|rd| rd.flatten().filter_map(|k| compact::newest_session(&k.path()).map(|s| s.1)).collect())
        .unwrap_or_default();
    let mut stack = vec![profile.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let path = e.path();
            let Ok(meta) = sys::lstat(&path) else { continue };
            if meta.is_dir() {
                stack.push(path);
                continue;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            if !meta.is_file() || meta.nlink() != 1 || meta.len() < compact::MIN_FILE_BYTES || name.starts_with('.') {
                continue;
            }
            let rel = path.strip_prefix(profile).unwrap_or(&path);
            if !cat.matches(Category::of(rel)) {
                continue;
            }
            if cat == LatencyCategory::NewestSessions && !newest.iter().any(|s| path.starts_with(s)) {
                continue;
            }
            out.push(path);
        }
    }
    out.sort();
    out
}

#[derive(Debug, Clone, Serialize)]
pub struct LatencyRow {
    pub category: LatencyCategory,
    pub files: usize,
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub rounds: usize,
    pub untouched_ms: u64,
    pub rewritten_ms: u64,
    pub compressed_ms: u64,
    pub compressed_minus_rewritten_ms: i64,
    pub rewritten_minus_untouched_ms: i64,
    pub compressed_minus_untouched_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct LatencyReport {
    pub libs: usize,
    pub registry_deps: bool,
    pub edit: String,
    pub fresh_breakdown: Vec<(String, u64)>,
    pub rows: Vec<LatencyRow>,
}

fn signed_median(values: &mut [i64]) -> i64 {
    values.sort_unstable();
    match values.len() {
        0 => 0,
        n if n % 2 == 1 => values[n / 2],
        n => (values[n / 2 - 1] + values[n / 2]) / 2,
    }
}

fn latency_bench(scratch: &Path, flags: &[String], quick: bool, s: &Styler, quiet: bool) -> Result<LatencyReport, String> {
    let value = |name: &str| flags.iter().find_map(|f| f.strip_prefix(&format!("--{name}=")).map(str::to_string));
    let rounds = value("rounds").and_then(|v| v.parse().ok()).filter(|n: &usize| *n > 0).unwrap_or(if quick { 4 } else { 20 });
    let libs = value("libs").and_then(|v| v.parse().ok()).filter(|n: &usize| *n >= 2).unwrap_or(if quick { 4 } else { 12 });
    let cats: Vec<LatencyCategory> = match value("categories") {
        Some(list) => LatencyCategory::ALL.iter().copied().filter(|c| list.split(',').any(|k| k.trim() == c.key())).collect(),
        None => LatencyCategory::ALL.to_vec(),
    };
    let max_scratch = max_scratch_bytes();
    fs::create_dir_all(scratch).map_err(|e| e.to_string())?;
    let cargo = resolve_tool("cargo").ok_or("cargo not found")?;
    let probe = Probe::run(None, scratch).ok_or("could not query rustc")?;
    let ctx = Ctx { cargo: cargo.clone(), fcargo: cargo, home: scratch.join("unused-home"), probe_id: probe.id };
    let proj = scratch.join("latency").join("app");
    let registry = write_growth_project(&proj, libs, true).and_then(|_| resolve_offline(&ctx, &proj)).is_ok();
    if !registry {
        write_growth_project(&proj, libs, false)?;
    }
    let target = scratch.join("latency-target");
    let profile = target.join("debug");
    let top = libs - 1;
    let lib_src = proj.join("libs").join(format!("lib{top}")).join("src").join("lib.rs");
    let mut counter = 0usize;
    let mut edit_build = |ctx: &Ctx| -> Result<u64, String> {
        counter += 1;
        let text = fs::read_to_string(&lib_src).map_err(|e| e.to_string())?;
        let edited: String = text
            .lines()
            .map(|l| if l.starts_with("pub fn tweak(") { format!("pub fn tweak(x: u64) -> u64 {{ x ^ {counter} }}\n") } else { format!("{l}\n") })
            .collect();
        fs::write(&lib_src, edited).map_err(|e| e.to_string())?;
        let (run, compiled) = build_logged(ctx, false, &proj, &target, &[], &[])?;
        if compiled == 0 {
            return Err("an edit build compiled nothing".into());
        }
        Ok(run.millis)
    };
    build_logged(&ctx, false, &proj, &target, &[], &[])?;
    edit_build(&ctx)?;
    edit_build(&ctx)?;
    guard_scratch(scratch, max_scratch)?;
    let edit = format!("body edit of lib{top} (it and the app recompile)");
    if !quiet {
        println!();
        println!("{}", s.bold(&format!("Edit-build latency with compressed files: {libs} local crates, {rounds} paired rounds, {edit}")));
        println!(
            "  {:<28} {:>6} {:>10} {:>10} {:>10} {:>10} {:>10} {:>12} {:>12}",
            "category", "files", "size", "compressed", "untouched", "rewritten", "compressed", "compr-rewr", "rewr-untch"
        );
    }
    let allocated = |paths: &[PathBuf]| -> u64 { paths.iter().filter_map(|p| sys::lstat(p).ok()).map(|m| m.allocated()).sum() };
    let fresh_breakdown = rows(&compact::breakdown(&target));
    if !quiet {
        let line: Vec<String> = fresh_breakdown.iter().map(|(l, b)| format!("{l} {}", human_bytes(*b))).collect();
        println!("  {}", s.dim(&format!("fresh target: {}", line.join(", "))));
    }
    let mut report = LatencyReport { libs, registry_deps: registry, edit, fresh_breakdown, rows: Vec::new() };
    // Each order of the three arms, so drift on a loaded machine cancels out.
    const ORDERS: [[usize; 3]; 6] = [[0, 1, 2], [1, 2, 0], [2, 0, 1], [0, 2, 1], [2, 1, 0], [1, 0, 2]];
    for cat in cats {
        let mut times: [Vec<u64>; 3] = [Vec::new(), Vec::new(), Vec::new()];
        let (mut files, mut before, mut after) = (0usize, 0u64, 0u64);
        // Nothing large enough to compress: compaction never touches the category.
        let rounds = if latency_files(&profile, cat).is_empty() { 0 } else { rounds };
        for round in 0..rounds {
            for &arm in &ORDERS[round % ORDERS.len()] {
                let selected = latency_files(&profile, cat);
                match arm {
                    1 => {
                        for p in &selected {
                            compact::rewrite_plain(p).map_err(|e| format!("{}: {e}", p.display()))?;
                        }
                    }
                    2 => {
                        let size = allocated(&selected);
                        for p in &selected {
                            if !compact::compress_file(p).map_err(|e| format!("{}: {e}", p.display()))? {
                                compact::rewrite_plain(p).map_err(|e| format!("{}: {e}", p.display()))?;
                            }
                        }
                        if round == 0 {
                            (files, before, after) = (selected.len(), size, allocated(&selected));
                        }
                    }
                    _ => {}
                }
                times[arm].push(edit_build(&ctx)?);
                if arm == 2 {
                    // Restore the uncompressed state and let a build settle it so the next arm starts like a normal build.
                    for p in latency_files(&profile, cat).iter().chain(selected.iter()) {
                        if sys::lstat(p).is_ok_and(|m| compact::is_compressed(&m)) {
                            compact::rewrite_plain(p).map_err(|e| format!("{}: {e}", p.display()))?;
                        }
                    }
                    edit_build(&ctx)?;
                }
            }
            guard_scratch(scratch, max_scratch)?;
        }
        let diff = |a: usize, b: usize| -> i64 {
            let mut d: Vec<i64> = times[a].iter().zip(&times[b]).map(|(x, y)| *x as i64 - *y as i64).collect();
            signed_median(&mut d)
        };
        let row = LatencyRow {
            category: cat,
            files,
            bytes_before: before,
            bytes_after: after,
            rounds,
            untouched_ms: median(&mut times[0].clone()),
            rewritten_ms: median(&mut times[1].clone()),
            compressed_ms: median(&mut times[2].clone()),
            compressed_minus_rewritten_ms: diff(2, 1),
            rewritten_minus_untouched_ms: diff(1, 0),
            compressed_minus_untouched_ms: diff(2, 0),
        };
        if !quiet {
            println!(
                "  {:<28} {:>6} {:>10} {:>10} {:>10} {:>10} {:>10} {:>12} {:>12}",
                cat.label(),
                row.files,
                human_bytes(row.bytes_before),
                human_bytes(row.bytes_after),
                duration_ms(row.untouched_ms),
                duration_ms(row.rewritten_ms),
                duration_ms(row.compressed_ms),
                format!("{:+}ms", row.compressed_minus_rewritten_ms),
                format!("{:+}ms", row.rewritten_minus_untouched_ms)
            );
        }
        report.rows.push(row);
    }
    Ok(report)
}

fn rows(b: &compact::Breakdown) -> Vec<(String, u64)> {
    Category::ALL.iter().filter(|c| b.get(**c).files > 0).map(|c| (c.label().to_string(), b.get(*c).allocated)).collect()
}

fn incremental_of(profile: &Path) -> (u64, usize) {
    let dir = profile.join("incremental");
    let sessions = fs::read_dir(&dir)
        .map(|rd| {
            rd.flatten()
                .map(|k| {
                    fs::read_dir(k.path())
                        .map(|s| s.flatten().filter(|e| e.file_name().to_string_lossy().starts_with("s-") && e.path().is_dir()).count())
                        .unwrap_or(0)
                })
                .sum()
        })
        .unwrap_or(0);
    (compact::physical_size(&[&dir]), sessions)
}

fn pin_serde(ctx: &Ctx, project: &Path, version: &str) -> bool {
    Command::new(&ctx.cargo)
        .args(["update", "-p", "serde", "--precise", version, "--offline"])
        .current_dir(project)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn run_output(bin: &Path) -> Result<Vec<u8>, String> {
    let out = Command::new(bin).stdin(Stdio::null()).output().map_err(|e| format!("{}: {e}", bin.display()))?;
    if !out.status.success() {
        return Err(format!("{} exited with {}", bin.display(), out.status));
    }
    Ok(out.stdout)
}

fn build_logged(ctx: &Ctx, frust: bool, project: &Path, target: &Path, args: &[&str], envs: &[(&str, &str)]) -> Result<(Run, usize), String> {
    let mut cmd = Command::new(if frust { &ctx.fcargo } else { &ctx.cargo });
    cmd.arg("build").args(args).current_dir(project).env("CARGO_NET_OFFLINE", "true").env_remove("RUSTC_WRAPPER").env_remove("RUSTFLAGS");
    if frust {
        cmd.env("FRUST_HOME", &ctx.home).env("FRUST_RENDER", "off").env_remove("CARGO_TARGET_DIR");
    } else {
        cmd.env("CARGO_TARGET_DIR", target);
    }
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let captured = spawn_capture(&mut cmd)?;
    if !captured.success {
        return Err(format!("build failed:\n{}", failure_tail(&captured.stderr)));
    }
    let compiled = captured.stderr.lines().filter(|l| l.trim_start().starts_with("Compiling")).count();
    Ok((Run { millis: captured.millis }, compiled))
}

struct Captured {
    success: bool,
    stderr: String,
    millis: u64,
}

// The clock stops when the direct child exits; a grandchild still holding stderr only delays the read by DRAIN, outside the measurement.
fn spawn_capture(cmd: &mut Command) -> Result<Captured, String> {
    let started = Instant::now();
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().map_err(|e| e.to_string())?;
    let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let (done_tx, done_rx) = mpsc::channel::<()>();
    if let Some(mut pipe) = child.stderr.take() {
        let sink = Arc::clone(&buf);
        thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            while let Ok(n) = pipe.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                sink.lock().unwrap_or_else(|e| e.into_inner()).extend_from_slice(&chunk[..n]);
            }
            let _ = done_tx.send(());
        });
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    let millis = started.elapsed().as_millis() as u64;
    let _ = done_rx.recv_timeout(DRAIN);
    let bytes = buf.lock().unwrap_or_else(|e| e.into_inner()).clone();
    Ok(Captured { success: status.success(), stderr: String::from_utf8_lossy(&bytes).into_owned(), millis })
}

fn failure_tail(text: &str) -> String {
    let mut lines: Vec<&str> = text.lines().rev().take(12).collect();
    lines.reverse();
    lines.join("\n")
}

fn print_churn(c: &ChurnReport, s: &Styler) {
    println!();
    println!("{}", s.bold("Churn summary"));
    let pct = |a: u64, b: u64| if b == 0 { 0.0 } else { (1.0 - a as f64 / b as f64) * 100.0 };
    println!("  configurations built          {}", c.configs.len());
    println!("  plain cargo, accumulated      {}", human_bytes(c.cargo_target));
    println!(
        "  frust, accumulated            {}  (target {}, cache {}, shared clones counted once)",
        human_bytes(c.frust_accumulated),
        human_bytes(c.frust_target),
        human_bytes(c.frust_store)
    );
    println!(
        "  frust after gc (aggressive)   {}  {}",
        human_bytes(c.frust_after_gc),
        s.dim(&format!(
            "({} superseded-variant paths {}, {} incremental sessions {})",
            c.gc_variant_paths,
            human_bytes(c.gc_variant_bytes),
            c.gc_sessions,
            human_bytes(c.gc_session_bytes)
        ))
    );
    println!(
        "  frust after compact           {}  {}",
        human_bytes(c.frust_after_compact),
        s.green(&format!("{:.0}% smaller than cargo's directory", pct(c.frust_after_compact, c.cargo_target)))
    );
    println!(
        "  {}",
        s.dim(&format!(
            "compact (after gc): {} more dead units, {} more stale sessions, {} files compressed, {} deduplicated",
            c.dead_units, c.stale_sessions, c.compressed_files, c.deduped_files
        ))
    );
    println!(
        "  rebuild after compact         {} units recompiled, binary output unchanged: {}",
        c.frust_compiled_after_compact,
        yes_no(c.binary_output_unchanged)
    );

    println!("  {}", s.bold("frust target directory by kind, before gc -> after gc + compact"));
    for (label, before) in &c.breakdown_before {
        let after = c.breakdown_after.iter().find(|(l, _)| l == label).map(|r| r.1).unwrap_or(0);
        println!("    {:<22} {:>10} -> {:>10}", label, human_bytes(*before), human_bytes(after));
    }
    println!(
        "  automatic compaction          {} after the churn, {} after {} more edits (cargo then {})  {}",
        human_bytes(c.auto_after_churn),
        human_bytes(c.auto_after_edits),
        c.edits,
        human_bytes(c.cargo_after_edits),
        s.green(&format!("{:.0}% smaller", pct(c.auto_after_edits, c.cargo_after_edits)))
    );
    println!("  {}", s.dim(&format!("(auto-compacted repeat of the last build recompiled {} units)", c.auto_compiled_on_repeat)));
    println!(
        "  release clean rebuild         cargo {}   frust {} from the compressed cache ({} units: {} cache hits, {} misses)",
        duration_ms(c.release_clean_cargo_ms),
        duration_ms(c.release_clean_frust_ms),
        c.release_clean_frust_compiled,
        c.release_clean_frust_cache_hits,
        c.release_clean_frust_cache_misses
    );
    println!("  {}", s.bold(&format!("incremental directory over {} edits to one file (debug)", c.edits)));
    println!(
        "    cargo  {} in {} sessions -> {} in {} sessions",
        human_bytes(c.cargo_incremental_before),
        c.cargo_sessions_before,
        human_bytes(c.cargo_incremental),
        c.cargo_sessions
    );
    println!(
        "    frust  {} in {} sessions -> {} in {} sessions -> after compact {} in {} sessions",
        human_bytes(c.frust_incremental_before),
        c.frust_sessions_before,
        human_bytes(c.frust_incremental),
        c.frust_sessions,
        human_bytes(c.frust_incremental_after_compact),
        c.frust_sessions_after_compact
    );
    if let Some(g) = &c.growth {
        let last = |v: GrowthVariant| g.rows.iter().rfind(|r| r.variant == v);
        if let (Some(cargo), Some(auto)) = (last(GrowthVariant::Cargo), last(GrowthVariant::Auto)) {
            println!("  {}", s.bold(&format!("{} edits across crates (bottom, middle and app; body and signature edits, feature toggles, reverts)", g.edits)));
            println!(
                "    cargo       target {}, incremental state {} ({} sessions, {} superseded object aliases)",
                human_bytes(cargo.target),
                human_bytes(cargo.anatomy.total()),
                cargo.anatomy.sessions,
                human_bytes(cargo.anatomy.deps_objects_stale)
            );
            println!(
                "    frust+auto  target {}, incremental state {} ({} sessions), cache {}  {}",
                human_bytes(auto.target),
                human_bytes(auto.anatomy.total()),
                auto.anatomy.sessions,
                human_bytes(auto.store),
                s.green(&format!("{:.0}% smaller target", pct(auto.target, cargo.target)))
            );
            println!(
                "    {}",
                s.dim(&format!(
                    "units compiled: cargo {} in {}, frust {} in {}",
                    cargo.compiled,
                    duration_ms(cargo.build_ms),
                    auto.compiled,
                    duration_ms(auto.build_ms)
                ))
            );
        }
        for (v, why) in &g.stopped {
            println!("    {}", s.yellow(&format!("{} stopped {why}", v.label())));
        }
    }
    println!(
        "  at the end, cargo's dir compressed in place (nothing removed): {} -> {}  (cargo then recompiled {} units)",
        human_bytes(c.cargo_after_edits),
        human_bytes(c.cargo_target_compressed),
        c.cargo_compiled_after_compress
    );
    println!(
        "  {}",
        s.dim(&format!(
            "like for like, both compressed: frust {} vs cargo {} ({:.0}% smaller, from removing superseded variants and dedupe)",
            human_bytes(c.frust_final_compacted),
            human_bytes(c.cargo_target_compressed),
            pct(c.frust_final_compacted, c.cargo_target_compressed)
        ))
    );
}

const RUNTIME_RUNS: usize = 20;

fn profile_size(target: &Path, profile: &str) -> u64 {
    fsx::physical_size(&[&target.join(profile)])
}

fn micros(us: u64) -> String {
    if us >= 10_000 { duration_ms(us / 1000) } else { format!("{us}us") }
}

struct Runtime {
    cargo_us: u64,
    frust_us: u64,
    outputs_equal: bool,
}

/// Alternating affects both equally; one untimed warm-up each because a new binary's first launch pays the signature check.
fn compare_runtime(cargo_bin: &Path, frust_bin: &Path, runs: usize) -> Result<Runtime, String> {
    let run = |bin: &Path| -> Result<(u64, Vec<u8>), String> {
        let started = Instant::now();
        let out = Command::new(bin).stdin(Stdio::null()).stderr(Stdio::null()).output().map_err(|e| format!("{}: {e}", bin.display()))?;
        let us = started.elapsed().as_micros() as u64;
        if !out.status.success() {
            return Err(format!("{} exited with {}", bin.display(), out.status));
        }
        Ok((us, out.stdout))
    };
    let (_, cargo_out) = run(cargo_bin)?;
    let (_, frust_out) = run(frust_bin)?;
    let (mut c, mut f) = (Vec::with_capacity(runs), Vec::with_capacity(runs));
    for i in 0..runs {
        let order: [(&Path, &mut Vec<u64>); 2] =
            if i % 2 == 0 { [(cargo_bin, &mut c), (frust_bin, &mut f)] } else { [(frust_bin, &mut f), (cargo_bin, &mut c)] };
        for (bin, times) in order {
            times.push(run(bin)?.0);
        }
    }
    Ok(Runtime { cargo_us: median(&mut c), frust_us: median(&mut f), outputs_equal: cargo_out == frust_out })
}

fn median(values: &mut [u64]) -> u64 {
    values.sort_unstable();
    match values.len() {
        0 => 0,
        n if n % 2 == 1 => values[n / 2],
        n => (values[n / 2 - 1] + values[n / 2]) / 2,
    }
}

/// Differences are attributed to Mach-O `LC_UUID` and the code signature only when they fall entirely inside those regions.
fn binary_diff(a: &Path, b: &Path) -> Result<BinaryDiff, String> {
    let read = |p: &Path| fs::read(p).map_err(|e| format!("{}: {e}", p.display()));
    Ok(diff_bytes(&read(a)?, &read(b)?))
}

fn diff_bytes(x: &[u8], y: &[u8]) -> BinaryDiff {
    let differing: Vec<usize> = x.iter().zip(y).enumerate().filter(|(_, (p, q))| p != q).map(|(i, _)| i).collect();
    if x.len() != y.len() {
        let count = differing.len() as u64 + x.len().abs_diff(y.len()) as u64;
        return BinaryDiff { identical: false, differing_bytes: count, only_uuid_and_signature: false };
    }
    let regions: Vec<Range<usize>> = macho_volatile_regions(x).into_iter().chain(macho_volatile_regions(y)).collect();
    let only = !differing.is_empty() && !regions.is_empty() && differing.iter().all(|i| regions.iter().any(|r| r.contains(i)));
    BinaryDiff { identical: differing.is_empty(), differing_bytes: differing.len() as u64, only_uuid_and_signature: only }
}

fn macho_volatile_regions(bin: &[u8]) -> Vec<Range<usize>> {
    const MH_MAGIC_64: u32 = 0xfeed_facf;
    const LC_UUID: u32 = 0x1b;
    const LC_CODE_SIGNATURE: u32 = 0x1d;
    let u32_at = |off: usize| bin.get(off..off + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let mut regions = Vec::new();
    if u32_at(0) != Some(MH_MAGIC_64) {
        return regions;
    }
    let Some(ncmds) = u32_at(16) else { return regions };
    let mut off = 32usize;
    for _ in 0..ncmds {
        let (Some(cmd), Some(size)) = (u32_at(off), u32_at(off + 4)) else { break };
        match cmd {
            LC_UUID => regions.push(off + 8..off + 24),
            LC_CODE_SIGNATURE => {
                if let (Some(data), Some(len)) = (u32_at(off + 8), u32_at(off + 12)) {
                    regions.push(data as usize..data as usize + len as usize);
                }
            }
            _ => {}
        }
        if size < 8 {
            break;
        }
        off += size as usize;
    }
    regions
}

fn fsx_hash(path: &Path) -> Option<String> {
    crate::cache::hash_file(path).ok()
}

fn canonical(path: &Path) -> PathBuf {
    sys::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn guard_disk(scratch: &Path) -> Result<(), String> {
    fsx::require_free_space(scratch, MIN_FREE_BYTES / 2)
}

#[derive(Debug, Clone)]
struct Run {
    millis: u64,
}

fn timed(cmd: &mut Command) -> Result<Run, String> {
    let captured = spawn_capture(cmd)?;
    if std::env::var_os("FRUST_DEBUG").is_some() {
        eprintln!("--- {:?} took {}ms\n{}", cmd.get_program(), captured.millis, captured.stderr);
    }
    if !captured.success {
        return Err(format!("build failed:\n{}", failure_tail(&captured.stderr)));
    }
    Ok(Run { millis: captured.millis })
}

fn plain_build(ctx: &Ctx, project: &Path, target: &Path) -> Result<Run, String> {
    plain_build_with(ctx, project, target, &[])
}

fn plain_build_with(ctx: &Ctx, project: &Path, target: &Path, extra: &[&str]) -> Result<Run, String> {
    timed(
        Command::new(&ctx.cargo)
            .arg("build")
            .args(extra)
            .current_dir(project)
            .env("CARGO_TARGET_DIR", target)
            .env("CARGO_NET_OFFLINE", "true")
            .env_remove("RUSTC_WRAPPER"),
    )
}

fn frust_build(ctx: &Ctx, project: &Path) -> Result<Run, String> {
    frust_build_with(ctx, project, &[])
}

fn frust_build_with(ctx: &Ctx, project: &Path, extra: &[&str]) -> Result<Run, String> {
    frust_build_env(ctx, project, extra, "off")
}

// Inline compaction is switched off and the daily gc marker is made fresh so the speed rows never absorb maintenance; it is timed separately.
fn frust_build_env(ctx: &Ctx, project: &Path, extra: &[&str], render: &str) -> Result<Run, String> {
    fs::create_dir_all(&ctx.home).map_err(|e| e.to_string())?;
    fs::write(Dirs::at(&ctx.home).auto_gc_marker(), b"").map_err(|e| e.to_string())?;
    timed(
        Command::new(&ctx.fcargo)
            .arg("build")
            .args(extra)
            .current_dir(project)
            .env("FRUST_HOME", &ctx.home)
            .env("FRUST_RENDER", render)
            .env("FRUST_AUTO_COMPACT", "off")
            .env("CARGO_NET_OFFLINE", "true")
            .env_remove("RUSTC_WRAPPER")
            .env_remove("CARGO_TARGET_DIR"),
    )
}

fn resolve_offline(ctx: &Ctx, project: &Path) -> Result<(), String> {
    let status = Command::new(&ctx.cargo)
        .args(["generate-lockfile", "--offline"])
        .current_dir(project)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| e.to_string())?;
    if status.success() { Ok(()) } else { Err("registry crates are not available offline".into()) }
}

fn touch_app(project: &Path, n: u32) -> Result<(), String> {
    let main = project.join("src").join("main.rs");
    let mut text = fs::read_to_string(&main).map_err(|e| e.to_string())?;
    let _ = write!(text, "\n// edit {n}\n");
    fs::write(&main, text).map_err(|e| e.to_string())
}

fn write_project(dir: &Path, libs: usize, registry: bool) -> Result<(), String> {
    let io = |e: std::io::Error| e.to_string();
    fs::create_dir_all(dir.join("src")).map_err(io)?;
    let mut manifest = String::from("[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n\n[profile.release]\nopt-level = 3\nlto = \"fat\"\ncodegen-units = 1\npanic = \"abort\"\n\n[dependencies]\n");
    if registry {
        manifest.push_str("serde = { version = \"1\", features = [\"derive\"] }\nserde_json = \"1\"\n");
    }
    if libs > 0 {
        manifest = manifest.replace("\n[dependencies]\n", "\n[features]\nextra = [\"lib0/extra\"]\n\n[dependencies]\n");
    }
    let mut main = String::new();
    for i in 0..libs {
        let _ = writeln!(manifest, "lib{i} = {{ path = \"libs/lib{i}\" }}");
        write_lib(&dir.join("libs").join(format!("lib{i}")), i, registry)?;
        let _ = writeln!(main, "use lib{i}::checksum as sum{i};");
    }
    main.push_str("fn main() {\n    let mut total = 0u64;\n");
    for i in 0..libs {
        let _ = writeln!(main, "    total = total.wrapping_add(sum{i}(total));");
    }
    main.push_str("    println!(\"{total}\");\n}\n");
    fs::write(dir.join("Cargo.toml"), manifest).map_err(io)?;
    fs::write(dir.join("src").join("main.rs"), main).map_err(io)
}

fn write_growth_project(dir: &Path, libs: usize, registry: bool) -> Result<(), String> {
    let io = |e: std::io::Error| e.to_string();
    fs::create_dir_all(dir.join("src")).map_err(io)?;
    let mut manifest = String::from("[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n\n[features]\nextra = [\"lib1/extra\"]\n\n[dependencies]\n");
    if registry {
        manifest.push_str("serde = { version = \"1\", features = [\"derive\"] }\nserde_json = \"1\"\n");
    }
    let mut main = String::new();
    for i in 0..libs {
        let _ = writeln!(manifest, "lib{i} = {{ path = \"libs/lib{i}\" }}");
        write_lib_with(&dir.join("libs").join(format!("lib{i}")), i, registry, true)?;
        let _ = writeln!(main, "use lib{i}::checksum as sum{i};");
    }
    main.push_str("fn tweak(x: u64) -> u64 { x ^ 0 }\n#[allow(dead_code)]\npub fn sig_0(x: u64) -> u64 { x }\n");
    main.push_str("fn main() {\n    let mut total = tweak(1);\n");
    for i in 0..libs {
        let _ = writeln!(main, "    total = total.wrapping_add(sum{i}(total));");
    }
    main.push_str("    println!(\"{total}\");\n}\n");
    fs::write(dir.join("Cargo.toml"), manifest).map_err(io)?;
    fs::write(dir.join("src").join("main.rs"), main).map_err(io)
}

fn write_lib(dir: &Path, index: usize, registry: bool) -> Result<(), String> {
    write_lib_with(dir, index, registry, false)
}

fn write_lib_with(dir: &Path, index: usize, registry: bool, growth: bool) -> Result<(), String> {
    let parent = (index > 0).then(|| (index - 1) / 2);
    let io = |e: std::io::Error| e.to_string();
    fs::create_dir_all(dir.join("src")).map_err(io)?;
    let mut manifest = format!("[package]\nname = \"lib{index}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[features]\nextra = []\n\n[dependencies]\n");
    if registry {
        manifest.push_str("serde = { version = \"1\", features = [\"derive\"] }\nserde_json = \"1\"\n");
    }
    let mut code = String::new();
    if growth {
        if let Some(p) = parent {
            let _ = writeln!(manifest, "lib{p} = {{ path = \"../lib{p}\" }}");
        }
        code.push_str("pub fn tweak(x: u64) -> u64 { x ^ 0 }\npub fn sig_0(x: u64) -> u64 { x }\n");
    }
    if registry {
        code.push_str("use serde::{Deserialize, Serialize};\n");
    }
    for k in 0..24 {
        if registry {
            code.push_str("#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]\n");
        }
        let _ = writeln!(code, "pub struct Rec{k} {{ pub a: u64, pub b: String, pub c: Vec<u32>, pub d: Option<i64> }}");
        let _ = writeln!(
            code,
            "pub fn step{k}(x: u64) -> u64 {{ let mut v = x ^ {salt}; for i in 0..{n} {{ v = v.wrapping_mul(6364136223846793005).wrapping_add(i); }} v }}",
            salt = index * 31 + k,
            n = 8 + k
        );
    }
    code.push_str("#[cfg(feature = \"extra\")]\npub fn extra(x: u64) -> u64 { x.rotate_left(7) }\n");
    code.push_str("pub fn checksum(seed: u64) -> u64 {\n    let mut v = seed;\n");
    for k in 0..24 {
        let _ = writeln!(code, "    v = step{k}(v);");
    }
    if registry {
        code.push_str("    let rec = Rec0 { a: v, b: v.to_string(), c: vec![1, 2, 3], d: None };\n    v ^= serde_json::to_string(&rec).map(|s| s.len() as u64).unwrap_or(0);\n");
    }
    if growth {
        if let Some(p) = parent {
            let _ = writeln!(code, "    v ^= lib{p}::checksum(v);");
        }
        code.push_str("    v = tweak(v);\n");
    }
    code.push_str("    v\n}\n");
    fs::write(dir.join("Cargo.toml"), manifest).map_err(io)?;
    fs::write(dir.join("src").join("lib.rs"), code).map_err(io)
}

fn print_report(report: &Report, s: &Styler) {
    println!();
    println!("{}", s.bold("Summary"));
    let m = &report.machine;
    println!("  machine                       {}, {} ({} cores), rustc {}", m.os, m.cpu, m.cores, report.rustc);
    let load = |v: Option<f64>| v.map_or("unavailable".to_string(), |l| format!("{l:.2}"));
    let busy = m.load_before.is_some_and(|l| l > m.cores as f64 * 0.25);
    println!(
        "  load average (1 min)          before {}, after {}   {}",
        load(m.load_before),
        load(m.load_after),
        s.dim(if busy { "(machine was already loaded at the start: treat times as noisy)" } else { "(machine was idle at the start; the after figure includes the benchmark)" })
    );
    println!("  {}", s.dim(&format!("(every timed row: median of {} runs; ratios are of medians; tool order alternates per run)", report.runs)));
    println!(
        "  second project (warm cache)   {}",
        s.green(&format!("{:.1}x faster than cargo", report.speedup_second_project))
    );
    println!(
        "  clean rebuild (warm cache)    {}",
        s.green(&format!("{:.1}x faster than cargo", report.speedup_clean_rebuild))
    );
    println!("  release, second project       {}", s.green(&format!("{:.1}x faster than cargo", report.release_speedup_second_project)));
    println!("  release, clean rebuild        {}", s.green(&format!("{:.1}x faster than cargo", report.release_speedup_clean_rebuild)));
    println!(
        "  release binary identical      restored vs cold: {}   frust vs cargo (same target dir): {}",
        yes_no(report.release_binary_identical_cold_vs_restored),
        yes_no(report.release_binary_identical_to_cargo)
    );
    let other = &report.release_binary_other_dir;
    let other_text = if other.identical {
        "identical".to_string()
    } else if other.only_uuid_and_signature {
        format!("{} bytes differ, all in LC_UUID and code signature (linker hashes object paths)", other.differing_bytes)
    } else {
        format!("{} bytes differ outside LC_UUID and code signature", other.differing_bytes)
    };
    println!("  {}", s.dim(&format!("(vs cargo in another target dir: {other_text})")));
    println!(
        "  release binary runtime        cargo {}   frust {}   (median of {}, outputs equal: {})",
        micros(report.runtime_median_us_cargo),
        micros(report.runtime_median_us_frust),
        report.runtime_runs,
        yes_no(report.runtime_outputs_equal)
    );
    println!("  no-op build overhead          {}", signed_ms(report.noop_overhead_ms));
    let saved = report.cargo_two_projects_disk.saturating_sub(report.frust_two_projects_disk);
    println!(
        "  disk, two projects            cargo {}   frust {}   {}",
        human_bytes(report.cargo_two_projects_disk),
        human_bytes(report.frust_two_projects_disk),
        s.green(&format!("{} smaller", human_bytes(saved)))
    );
    println!("  {}", s.dim(&format!("(frust by naive du, clones counted in full: {})", human_bytes(report.frust_two_projects_du))));
    if let Some(churn) = &report.churn {
        print_churn(churn, s);
    }
}

fn signed_ms(ms: i64) -> String {
    if ms >= 0 { format!("+{}", duration_ms(ms as u64)) } else { format!("-{}", duration_ms((-ms) as u64)) }
}

fn yes_no(v: bool) -> &'static str {
    if v { "yes" } else { "NO" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_link_timing_requires_one_current_phase() {
        let stderr = "time:   0.500; rss: 100MB -> 110MB (10MB) codegen_to_LLVM_IR\n\
                      time:   0.012; rss: 110MB -> 111MB (1MB) run_linker\n\
                      time:   0.250; rss: 112MB -> 113MB (1MB) link_binary\n";
        assert_eq!(native_link_millis(stderr), Ok(250.0));
        let replayed = format!("time: 1.200; rss: 0MB link_binary\n{stderr}");
        assert!(native_link_millis(&replayed).is_err());
        assert_eq!(native_link_millis("time: 0.000; rss: 0MB link_binary"), Ok(0.0));
        assert!(native_link_millis("time: 1.0; rss: 0MB link_binary_extra").is_err());
        assert!(native_link_millis("time: NaN; rss: 0MB link_binary").is_err());
        assert!(native_link_millis("time: -1; rss: 0MB link_binary").is_err());
        assert!(native_link_millis("link_binary").is_err());
    }

    #[test]
    fn link_profile_script_quotes_the_shared_wrapper_path() {
        let script = link_profile_script(Path::new("/tools/it's a wrapper/frust-wrap")).unwrap();
        assert!(script.contains("exec '/tools/it'\\''s a wrapper/frust-wrap' \"$@\" -Ztime-passes"));
        assert!(script.ends_with("exec '/tools/it'\\''s a wrapper/frust-wrap' \"$@\"\n"));
        assert_eq!(script.matches("-Ztime-passes").count(), 1);
        assert!(script.contains("\"${CARGO_PRIMARY_PACKAGE:-}\" = 1"));
    }

    #[test]
    fn measured_link_requires_only_current_main_and_plausible_wall_time() {
        assert!(validate_measured_link(1, 1, 250.0, 303).is_ok());
        assert!(validate_measured_link(1, 1, 303.9, 303).is_ok());
        assert!(validate_measured_link(2, 1, 250.0, 303).is_err());
        assert!(validate_measured_link(1, 0, 250.0, 303).is_err());
        assert!(validate_measured_link(0, 0, 0.0, 303).is_err());
        assert!(validate_measured_link(1, 1, 1444.0, 303).is_err());
        assert!(validate_measured_link(1, 1, 304.1, 303).is_err());
    }

    #[test]
    fn linking_edits_preserve_workload_and_replace_body_input() {
        let original = "use lib0::checksum;\nfn main() {\n    println!(\"{}\", checksum(7));\n}\n";
        let first = link_edit_source(original, 17).unwrap();
        let second = link_edit_source(original, 18).unwrap();
        assert!(first.contains("std::hint::black_box(17usize)"));
        assert!(second.contains("std::hint::black_box(18usize)"));
        assert!(!second.contains("17usize"));
        assert!(first.starts_with("use lib0::checksum;\nfn main() {\n"));
        assert!(first.ends_with("\n    println!(\"{}\", checksum(7));\n}\n"));
        assert!(second.ends_with("\n    println!(\"{}\", checksum(7));\n}\n"));
        assert_ne!(first, second);
        assert!(link_edit_source("pub fn helper() {}", 1).is_err());
    }

    /// A minimal 64-bit Mach-O: LC_UUID at 32, LC_CODE_SIGNATURE at 56 pointing at a 32-byte blob at 96.
    fn image() -> Vec<u8> {
        let mut b = vec![0u8; 128];
        b[0..4].copy_from_slice(&0xfeed_facfu32.to_le_bytes());
        b[16..20].copy_from_slice(&2u32.to_le_bytes());
        b[32..36].copy_from_slice(&0x1bu32.to_le_bytes());
        b[36..40].copy_from_slice(&24u32.to_le_bytes());
        b[56..60].copy_from_slice(&0x1du32.to_le_bytes());
        b[60..64].copy_from_slice(&16u32.to_le_bytes());
        b[64..68].copy_from_slice(&96u32.to_le_bytes());
        b[68..72].copy_from_slice(&32u32.to_le_bytes());
        b
    }

    #[test]
    fn uuid_and_signature_differences_are_attributed() {
        let a = image();
        let mut b = image();
        b[40] ^= 0xff;
        b[100] ^= 0xff;
        let d = diff_bytes(&a, &b);
        assert!(!d.identical);
        assert_eq!(d.differing_bytes, 2);
        assert!(d.only_uuid_and_signature);
    }

    #[test]
    fn differences_elsewhere_are_not_attributed() {
        let a = image();
        let mut b = image();
        b[40] ^= 0xff;
        b[80] ^= 0xff;
        let d = diff_bytes(&a, &b);
        assert_eq!(d.differing_bytes, 2);
        assert!(!d.only_uuid_and_signature);
        assert!(diff_bytes(&a, &a).identical);
        assert!(!diff_bytes(&a, &a[..100]).only_uuid_and_signature);
    }

    #[test]
    fn runs_flag_defaults_and_minimum() {
        let flags = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(runs_from_flags(&flags(&[]), false), Ok(5));
        assert_eq!(runs_from_flags(&flags(&["--quick"]), true), Ok(3));
        assert_eq!(runs_from_flags(&flags(&["--runs", "7"]), false), Ok(7));
        assert_eq!(runs_from_flags(&flags(&["--runs=4", "--json"]), true), Ok(4));
        assert!(runs_from_flags(&flags(&["--runs", "2"]), false).is_err());
        assert!(runs_from_flags(&flags(&["--runs"]), false).is_err());
        assert!(runs_from_flags(&flags(&["--runs=x"]), false).is_err());
    }

    #[test]
    fn table_reports_median_and_spread() {
        let mut t = Table::default();
        for ms in [30, 10, 50, 20, 40] {
            t.add("row", Run { millis: ms }, None);
        }
        assert_eq!(t.stats("row"), (30, 10, 50));
        assert_eq!(t.median("missing"), 0);
        let m = &t.measurements()[0];
        assert_eq!((m.millis, m.min_millis, m.max_millis, m.runs), (30, 10, 50, 5));
    }

    #[cfg(unix)]
    #[test]
    fn lingering_grandchild_does_not_extend_the_measurement() {
        let started = Instant::now();
        let captured = spawn_capture(Command::new("sh").args(["-c", "sleep 4 & echo hello >&2"])).unwrap();
        assert!(captured.success);
        assert!(captured.stderr.contains("hello"));
        assert!(captured.millis < 1000);
        assert!(started.elapsed() < Duration::from_millis(3000));
    }

    #[test]
    fn median_of_even_and_odd() {
        assert_eq!(median(&mut [5, 1, 3]), 3);
        assert_eq!(median(&mut [4, 1, 3, 2]), 2);
        assert_eq!(median(&mut []), 0);
    }
}
