//! `frust bench`: end-to-end measurements of what frust buys over plain cargo.

use std::fmt::Write as _;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;

mod concurrent;

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
// Schema one used compilation-like names for Cargo diagnostic counts; schema two preserves explicit repeat evidence.
const EVIDENCE_SCHEMA_VERSION: u32 = 2;
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
    pub samples: Vec<Run>,
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
    pub evidence_schema_version: u32,
    pub store_policy: StorePolicy,
    pub store_completion_method: &'static str,
    pub rustc: String,
    pub cache_compression: String,
    pub legacy_release_profile: &'static str,
    pub filesystem: Option<String>,
    pub maintenance_auto_compaction_policy: AutoCompactionPolicy,
    pub maintenance_gc_cache_max_bytes: u64,
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
    pub profile_matrix: ProfileMatrix,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChurnReport {
    pub evidence_schema_version: u32,
    pub store_policy: StorePolicy,
    pub cache_compression: String,
    pub auto_compaction_policy: AutoCompactionPolicy,
    pub configs: Vec<String>,
    pub cargo_target: u64,
    /// Compression and deduplication only: frust removes variants only in its own directories.
    pub cargo_target_compressed: u64,
    /// Plain cargo still considers everything fresh after that compression.
    pub cargo_compile_notices_after_compress: usize,
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
    /// Cargo `Compiling` notices after compaction (must be zero); actual rustc calls are distinct evidence.
    pub frust_compile_notices_after_compact: usize,
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
    pub auto_compile_notices_on_repeat: usize,
    pub release_clean_cargo_ms: u64,
    pub release_clean_frust_ms: u64,
    pub release_clean_frust_compile_notices: usize,
    pub release_clean_frust_rustc_compilations: Option<usize>,
    pub release_clean_frust_rustc_breakdown: Option<CompilationCounts>,
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
    if flags.iter().any(|flag| flag == "--concurrent" || flag.starts_with("--concurrent=")) {
        return concurrent::run(flags);
    }
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

    let compression = match compression_from_flags(flags) {
        Ok(value) => value,
        Err(error) => { eprintln!("frust bench: {error}"); return 1; }
    };
    let store = match store_from_flags(flags) {
        Ok(value) => value,
        Err(error) => { eprintln!("frust bench: {error}"); return 1; }
    };
    let settle_edits = match settle_edits_from_flags(flags) {
        Ok(value) => value,
        Err(error) => { eprintln!("frust bench: {error}"); return 1; }
    };
    let keep_scratch = flags.iter().any(|flag| flag == "--keep-scratch");
    let prefix = store.scratch_prefix(keep_scratch);
    let scratch = std::env::temp_dir().join(format!("{prefix}-{}", std::process::id()));
    remove_abandoned_scratch(&std::env::temp_dir());
    if let Err(msg) = fsx::require_free_space(&std::env::temp_dir(), MIN_FREE_BYTES) {
        eprintln!("frust bench: refusing to run: {msg}");
        return 1;
    }
    let mut guard = Scratch(scratch.clone(), keep_scratch);
    if flags.iter().any(|f| f == "--profiles") {
        let result = profile_matrix(&scratch, quick, runs, &compression, store);
        // Any deferred failure may leave a helper owning fixture files.
        if store == StorePolicy::Deferred && result.is_err() { guard.1 = true; }
        drop(guard);
        return match result {
            Ok(report) => {
                if json { println!("{}", serde_json::to_string_pretty(&report).unwrap_or_default()); }
                else { print_profile_matrix(&report); }
                0
            }
            Err(msg) => { eprintln!("frust bench: {msg}"); 1 }
        };
    }
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
        let result = growth_bench(&scratch, &GrowthOptions::from_flags(flags, quick), &s, json, keep_scratch, settle_edits);
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
        let result = churn_bench(&scratch, quick, &s, json, &compression, keep_scratch);
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
    let result = bench(&scratch, quick, runs, &s, json, &compression, store).and_then(|mut report| {
        report.churn = Some(churn_bench(&scratch.join("churn"), quick, &s, json, &compression, keep_scratch)?);
        report.machine.load_after = load_average();
        Ok(report)
    });
    let failed = result.as_ref().ok().and_then(failed_checks);
    if store == StorePolicy::Deferred && (result.is_err() || failed.is_some()) { guard.1 = true; }
    drop(guard);
    match result {
        Ok(report) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&report).unwrap_or_default());
            } else {
                print_report(&report, &s);
            }
            match failed {
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
struct Scratch(PathBuf, bool);

impl Drop for Scratch {
    fn drop(&mut self) {
        if self.1 {
            if self.0.is_dir() { eprintln!("frust bench: retained scratch at {}", self.0.display()); }
            return;
        }
        let name = self.0.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if name.starts_with("frust-bench-") && self.0.starts_with(std::env::temp_dir()) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}

#[derive(Clone)]
struct Ctx {
    cargo: PathBuf,
    fcargo: PathBuf,
    home: PathBuf,
    probe_id: String,
    audit: Option<CompilerAudit>,
    compression: String,
    store: StorePolicy,
}

#[derive(Clone)]
struct CompilerAudit {
    compiler: PathBuf,
    logs: PathBuf,
}

impl CompilerAudit {
    fn create(scratch: &Path) -> Result<Option<Self>, String> {
        if !cfg!(unix) {
            return Ok(None);
        }
        fs::create_dir_all(scratch).map_err(|e| e.to_string())?;
        let compiler = scratch.join("audited-rustc");
        let rustc = std::env::var_os("RUSTC").map(PathBuf::from)
            .or_else(|| resolve_tool("rustc")).ok_or("rustc not found")?;
        let quoted = shell_quote(&rustc)?;
        let script = format!("#!/bin/sh\nif [ -n \"${{FRUST_BENCH_AUDIT:-}}\" ]; then\n    printf '%s\\0' \"$@\" > \"$FRUST_BENCH_AUDIT/$$\" || exit 125\nfi\nexec {quoted} \"$@\"\n");
        fs::write(&compiler, script).map_err(|e| e.to_string())?;
        sys::set_path_mode(&compiler, 0o755).map_err(|e| e.to_string())?;
        Ok(Some(Self { compiler, logs: scratch.join("compiler-invocations") }))
    }
}

fn shell_quote(path: &Path) -> Result<String, String> {
    let text = path.to_str().ok_or("benchmark tool path is not UTF-8")?;
    Ok(format!("'{}'", text.replace('\'', "'\\''")))
}

#[derive(Debug, Clone, Serialize)]
pub struct AutoCompactionPolicy {
    enabled: bool,
    every_seconds: u64,
    min_growth_bytes: u64,
    keep_other_profile_seconds: u64,
    uncached_dead_grace_seconds: u64,
    dead_incremental_grace_seconds: u64,
    incremental_idle_seconds: u64,
    max_target_bytes: u64,
    current_window_seconds: u64,
    hot_window_seconds: u64,
    hot_categories: Vec<String>,
    cache_enabled: bool,
    compress: bool,
    pass_budget_ms: u64,
    pass_bytes: u64,
}

impl From<&AutoOptions> for AutoCompactionPolicy {
    fn from(options: &AutoOptions) -> Self {
        Self {
            enabled: options.enabled, every_seconds: options.every.as_secs(), min_growth_bytes: options.min_growth,
            keep_other_profile_seconds: options.keep_other_profile.as_secs(),
            uncached_dead_grace_seconds: options.uncached_dead_grace.as_secs(),
            dead_incremental_grace_seconds: options.dead_incremental_grace.as_secs(),
            incremental_idle_seconds: options.incremental_idle.as_secs(), max_target_bytes: options.max_target_bytes,
            current_window_seconds: options.current_window.as_secs(), hot_window_seconds: options.hot_window.as_secs(),
            hot_categories: options.hot_categories.iter().map(|category| format!("{category:?}")).collect(),
            cache_enabled: options.cache_enabled, compress: options.compress,
            pass_budget_ms: options.pass_budget.as_millis() as u64, pass_bytes: options.pass_bytes,
        }
    }
}

// Fixed benchmark policy: production retention defaults, assessed after every build, with caching enabled.
fn benchmark_auto_options(full_maintenance_pass: bool) -> AutoOptions {
    AutoOptions {
        enabled: true, every: Duration::ZERO, min_growth: 0,
        keep_other_profile: Duration::from_secs(14 * 86_400),
        uncached_dead_grace: Duration::from_secs(86_400), dead_incremental_grace: Duration::from_secs(3600),
        incremental_idle: Duration::from_secs(7 * 86_400), max_target_bytes: 8 * 1024 * 1024 * 1024,
        current_window: Duration::from_secs(3600), hot_window: Duration::from_secs(3600),
        hot_categories: compact::HOT_CATEGORIES.to_vec(), cache_enabled: true, compress: true,
        pass_budget: Duration::from_secs(if full_maintenance_pass { 3600 } else { 2 }),
        pass_bytes: if full_maintenance_pass { u64::MAX } else { 512 * 1024 * 1024 },
    }
}

fn benchmark_maintenance_config() -> FrustConfig {
    FrustConfig {
        cache: true, cache_primary: true, cache_max_bytes: 16 * 1024 * 1024 * 1024,
        target: crate::config::TargetMode::Central, tune: false, render: crate::config::RenderMode::Off,
        osc: crate::config::Osc::default(), lock_timeout: None, lock: true, debug: false,
        gc_sync: true, store_sync: true, keylock_wait: crate::config::KEY_WAIT_CAP,
    }
}

#[derive(Debug, Serialize)]
pub struct ProfileMatrix {
    evidence_schema_version: u32,
    store_policy: StorePolicy,
    store_completion_method: &'static str,
    rustc_verbose: String,
    cargo_version: String,
    machine: Machine,
    cache_compression: String,
    filesystem: Option<String>,
    reflink_supported: bool,
    warmup: &'static str,
    compiler_audit_available: bool,
    compiler_audit_method: &'static str,
    frontend_blake3: String,
    wrapper_blake3: String,
    local_crates: usize,
    registry_deps: bool,
    profiles: Vec<ProfileResult>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ManagedAllocation {
    status: &'static str,
    cache_home_applicable: bool,
    target_plus_store_bytes: Option<u64>,
    cache_home_bytes: Option<u64>,
    other_managed_bytes: Option<u64>,
    retired_payload_bytes: Option<u64>,
    total_managed_bytes: Option<u64>,
    method: &'static str,
    error: Option<String>,
}

#[derive(PartialEq, Eq)]
struct RetiredAllocationSnapshot {
    tmp_identity: Option<(u64, u64)>,
    queue_stamp: Option<crate::cache::hashing::Stamp>,
    children: Vec<PathBuf>,
}

fn retired_allocation_snapshot(dirs: &Dirs) -> Result<RetiredAllocationSnapshot, String> {
    let mut snapshot = RetiredAllocationSnapshot { tmp_identity: None, queue_stamp: None, children: Vec::new() };
    let directory = |path: &Path| -> Result<Option<sys::Meta>, String> {
        match sys::lstat(path) {
            Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => Ok(Some(meta)),
            Ok(_) => Err(format!("unsafe retired-payload directory {}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("{}: {error}", path.display())),
        }
    };
    let Some(tmp) = directory(&dirs.tmp())? else { return Ok(snapshot) };
    snapshot.tmp_identity = Some((tmp.dev(), tmp.ino()));
    let queue = crate::cache::evicted_dir(dirs);
    let Some(meta) = directory(&queue)? else { return Ok(snapshot) };
    snapshot.queue_stamp = Some(crate::cache::hashing::Stamp::of(&meta));
    snapshot.children = fs::read_dir(&queue).map_err(|error| error.to_string())?
        .map(|entry| entry.map(|entry| entry.path()).map_err(|error| error.to_string())).collect::<Result<_, _>>()?;
    snapshot.children.sort();
    Ok(snapshot)
}

fn managed_allocation(target: &Path, home: Option<&Path>) -> ManagedAllocation {
    let method = "Checked allocated blocks of unique device/inode entries, including directories and symlinks. Detected APFS clone sharing is estimated; unknown sharing is charged fully. All unions use one revalidated inventory. Other managed allocation is total minus selected target/store, including control files, temporary or retired payloads and other central targets. Retired payload is the standalone union of queued children, excluding the queue directory itself; it may share allocation with other roots, so do not add it to the total.";
    let measured = (|| -> Result<_, String> {
        let Some(home) = home else {
            let target = compact::physical_size_checked(&[target]).map_err(|error| error.to_string())?;
            return Ok((target, None, None, None, target));
        };
        let dirs = Dirs::at(home);
        let store = dirs.store();
        let retired = retired_allocation_snapshot(&dirs)?;
        let retired_roots: Vec<_> = retired.children.iter().map(PathBuf::as_path).collect();
        let mut target_store = vec![target];
        let store_absent = match sys::lstat(&store) {
            Ok(_) => { target_store.push(store.as_path()); false }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
            Err(error) => return Err(format!("{}: {error}", store.display())),
        };
        let home_roots = [home];
        let managed_roots = [target, home];
        let measured = compact::physical_sizes_checked(&[target_store.as_slice(), &home_roots, &managed_roots, retired_roots.as_slice()])
            .map_err(|error| error.to_string())?;
        if retired_allocation_snapshot(&dirs)? != retired {
            return Err("retired-payload queue identity or membership changed during inventory".into());
        }
        if store_absent {
            match sys::lstat(&store) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                _ => return Err(format!("optional allocation root changed during inventory: {}", store.display())),
            }
        }
        let other = measured[2].checked_sub(measured[0]).ok_or("inconsistent managed allocation: target/store exceeds full union")?;
        Ok((measured[0], Some(measured[1]), Some(other), Some(measured[3]), measured[2]))
    })();
    match measured {
        Ok((target_plus_store, cache_home, other, retired, total)) => ManagedAllocation {
            status: "available", cache_home_applicable: home.is_some(), target_plus_store_bytes: Some(target_plus_store),
            cache_home_bytes: cache_home, other_managed_bytes: other, retired_payload_bytes: retired, total_managed_bytes: Some(total), method, error: None,
        },
        Err(error) => ManagedAllocation {
            status: "unavailable", cache_home_applicable: home.is_some(), target_plus_store_bytes: None,
            cache_home_bytes: None, other_managed_bytes: None, retired_payload_bytes: None, total_managed_bytes: None, method, error: Some(error),
        },
    }
}

fn managed_allocation_text(allocation: &ManagedAllocation) -> String {
    match allocation.total_managed_bytes {
        Some(total) => match allocation.other_managed_bytes {
            Some(other) => format!("{} managed total ({} target plus store; {} other managed; {} retired payload breakdown)", human_bytes(total),
                human_bytes(allocation.target_plus_store_bytes.unwrap()), human_bytes(other), human_bytes(allocation.retired_payload_bytes.unwrap())),
            None => format!("{} managed total (Cargo target only)", human_bytes(total)),
        },
        None => format!("managed allocation unavailable: {}", allocation.error.as_deref().unwrap_or("unknown allocation error")),
    }
}

#[derive(Debug, Serialize)]
struct ProfileResult {
    name: &'static str,
    lto: &'static str,
    codegen_units: usize,
    panic: &'static str,
    incremental: bool,
    cargo_arguments: Vec<String>,
    samples: Vec<ProfileSample>,
    summaries: Vec<ProfileSummary>,
    cargo_target_bytes: u64,
    frust_target_bytes: u64,
    frust_store_bytes: u64,
    frust_target_plus_store_bytes: u64,
    cargo_managed_allocation: ManagedAllocation,
    frust_managed_allocation: ManagedAllocation,
    same_directory_binaries_identical: bool,
    restored_binaries_identical: bool,
    runtime_outputs_equal: bool,
}

#[derive(Debug, Serialize)]
struct ProfileSummary {
    phase: &'static str,
    tool: &'static str,
    median_ms: u64,
    min_ms: u64,
    max_ms: u64,
    runs: usize,
}

#[derive(Debug, Serialize)]
struct ProfileSample {
    repetition: usize,
    order_in_pair: usize,
    phase: &'static str,
    tool: &'static str,
    build: Run,
    binary_blake3: String,
    stdout: String,
    expected_edit: u32,
    target_bytes: u64,
    store_bytes: u64,
    target_plus_store_bytes: u64,
    managed_allocation: ManagedAllocation,
}

fn expected_edit_output(output: &[u8], edit: u32) -> bool {
    let marker = format!("frust-bench-edit {edit}");
    output.split(|b| *b == b'\n').any(|line| line == marker.as_bytes())
}

fn verify_edit_pair(cargo: &Path, frust: &Path, edit: u32) -> Result<(), String> {
    let cargo = run_output(cargo)?;
    let frust = run_output(frust)?;
    if cargo != frust || !expected_edit_output(&cargo, edit) {
        return Err(format!("edit {edit} produced unequal or stale program output"));
    }
    Ok(())
}

fn validate_build_evidence(run: &Run, phase: &str, frust: bool) -> Result<(), String> {
    if phase == "no-op" && run.cargo_compile_notices != 0 {
        return Err("no-op sample unexpectedly emitted Cargo compile notices".into());
    }
    if phase == "no-op" && (run.cache_hits != 0 || run.cache_misses != 0) {
        return Err(format!("no-op sample unexpectedly recorded {} cache hits and {} misses", run.cache_hits, run.cache_misses));
    }
    if phase == "no-op" {
        if run.rustc_call_count.is_some_and(|count| count != 0)
            || run.rustc_query_invocations.as_ref().is_some_and(|queries| !queries.is_empty()) {
            return Err("no-op sample unexpectedly executed rustc calls or queries".into());
        }
        let audited = run.rustc_invocations.is_some() || run.rustc_query_invocations.is_some() || run.rustc_call_count.is_some();
        if audited && (run.rustc_invocations.is_none() || run.rustc_query_invocations.is_none() || run.rustc_call_count.is_none()) {
            return Err("no-op sample is missing complete rustc call and query evidence".into());
        }
    }
    if phase == "clean rebuild" && frust && (run.cache_hits == 0 || run.cache_misses != 0) {
        return Err(format!("warm restore requires hits and zero misses, got {} hits and {} misses", run.cache_hits, run.cache_misses));
    }
    let Some(invocations) = &run.rustc_invocations else { return Ok(()) };
    let counts = CompilationCounts::from_invocations(invocations);
    let should_compile = phase == "cold" || phase == "edit" || phase == "edit, warm session" || phase == "clean rebuild" && !frust;
    if should_compile && counts.cargo_units == 0 {
        return Err(format!("{phase} sample executed no declared Cargo-unit compilation ({} build-script probes, {} unknown calls)", counts.build_script_probes, counts.unknown));
    }
    if phase == "no-op" && counts.total != 0 {
        return Err(format!("no-op sample unexpectedly executed {} rustc compilations", counts.total));
    }
    if phase == "clean rebuild" && frust && (counts.cargo_units != 0 || counts.unknown != 0) {
        return Err(format!("warm restore executed {} declared Cargo-unit compilations and {} unknown calls ({} build-script probes, {} total rustc calls)", counts.cargo_units, counts.unknown, counts.build_script_probes, counts.total));
    }
    Ok(())
}

fn evidence_failure(run: &Run, context: &str, reason: &str) -> String {
    format!("{context}: {reason}\nbuild evidence:\n{}", serde_json::to_string_pretty(run).unwrap_or_else(|error| format!("cannot serialize evidence: {error}")))
}

fn profile_matrix(scratch: &Path, quick: bool, runs: usize, compression: &str, store: StorePolicy) -> Result<ProfileMatrix, String> {
    let root = scratch.join("profile-matrix");
    fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let cargo = resolve_tool("cargo").ok_or("cargo not found")?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let fcargo = fcargo_beside(&exe);
    let wrapper = exe.with_file_name(format!("frust-wrap{}", std::env::consts::EXE_SUFFIX));
    let probe = Probe::run(None, &root).ok_or("could not query rustc")?;
    let ctx = Ctx { cargo, fcargo, home: root.join("home"), probe_id: probe.id.clone(), audit: CompilerAudit::create(&root)?, compression: compression.to_string(), store };
    let load_before = load_average();
    let libs = if quick { 2 } else { 8 };
    let project = root.join("app");
    let registry = write_project(&project, libs, true).and_then(|_| resolve_offline(&ctx, &project)).is_ok();
    if !registry {
        write_project(&project, libs, false)?;
    }
    resolve_offline(&ctx, &project)?;
    let source = project.join("src/main.rs");
    let original = fs::read_to_string(&source).map_err(|e| e.to_string())?;
    let target = root.join("shared-target");
    let snapshots = [root.join("cargo-target"), root.join("frust-target")];
    let dirs = Dirs::at(&ctx.home);
    let mut profiles = Vec::new();
    for (name, lto, units, panic) in [
        ("release", "false", 16, "unwind"),
        ("release-thin-lto", "thin", 16, "unwind"),
        ("release-fat-lto", "fat", 1, "abort"),
    ] {
        let lto_value = if lto == "false" { lto.to_string() } else { format!("\"{lto}\"") };
        let args = vec!["--release".to_string(), "--locked".to_string(),
            "--config".to_string(), format!("profile.release.lto={lto_value}"),
            "--config".to_string(), format!("profile.release.codegen-units={units}"),
            "--config".to_string(), format!("profile.release.panic=\"{panic}\""),
            "--config".to_string(), "profile.release.incremental=false".to_string()];
        let borrowed: Vec<_> = args.iter().map(String::as_str).collect();
        let warm_project = root.join("warmup-project");
        let warm_target = root.join("warmup-target");
        let warm_ctx = Ctx { home: root.join("warmup-home"), ..ctx.clone() };
        write_project(&warm_project, 0, false)?;
        resolve_offline(&warm_ctx, &warm_project)?;
        for frust in [false, true] {
            clear(&warm_target)?;
            timed_with_store(&warm_ctx, frust, &mut build_command(&warm_ctx, frust, &warm_project, Some(&warm_target), &borrowed, "off"), store)?;
        }
        clear(&warm_target)?;
        clear(&warm_ctx.home)?;
        let mut samples = Vec::new();
        for rep in 0..runs {
            fs::write(&source, &original).map_err(|e| e.to_string())?;
            clear(&ctx.home)?;
            clear(&target)?;
            for snapshot in &snapshots { clear(snapshot)?; }
            let order = if rep % 2 == 0 { [0, 1] } else { [1, 0] };
            let mut cold_hashes = [String::new(), String::new()];
            for phase in ["cold", "clean rebuild", "no-op", "edit"] {
                let edit = if phase == "edit" { rep as u32 + 1 } else { 0 };
                if phase == "edit" { touch_app(&project, edit)?; }
                let mut pair_hash = None;
                let mut pair_output = None;
                for (position, arm) in order.into_iter().enumerate() {
                    let frust = arm == 1;
                    if phase == "no-op" || phase == "edit" {
                        move_dir(&snapshots[arm], &target)?;
                    }
                    let mut cmd = build_command(&ctx, frust, &project, Some(&target), &borrowed, "off");
                    let build = timed_with_store(&ctx, frust, &mut cmd, store)?;
                    validate_build_evidence(&build, phase, frust).map_err(|reason| {
                        evidence_failure(&build, &format!("{name}, repetition {}, {}, {phase}", rep + 1, if frust { "frust" } else { "cargo" }), &reason)
                    })?;
                    let binary = app_binary(&target.join("release"));
                    let hash = crate::cache::hash_file(&binary).map_err(|e| e.to_string())?;
                    let output = run_output(&binary)?;
                    if !expected_edit_output(&output, edit) {
                        return Err(format!("{name} {phase} did not produce expected edit {edit}"));
                    }
                    if pair_hash.as_ref().is_some_and(|old| old != &hash) || pair_output.as_ref().is_some_and(|old| old != &output) {
                        return Err(format!("{name} {phase} Cargo/frust same-directory binaries or outputs differ"));
                    }
                    pair_hash = Some(hash.clone());
                    pair_output = Some(output.clone());
                    if phase == "cold" { cold_hashes[arm] = hash.clone(); }
                    if phase == "clean rebuild" && cold_hashes[arm] != hash {
                        return Err(format!("{name} restored binary differs from its cold build"));
                    }
                    if phase == "edit" && cold_hashes[arm] == hash {
                        return Err(format!("{name} observable edit did not change executable bytes"));
                    }
                    let store = dirs.store();
                    let roots = if frust { vec![target.as_path(), store.as_path()] } else { vec![target.as_path()] };
                    samples.push(ProfileSample {
                        repetition: rep + 1, order_in_pair: position + 1, phase,
                        tool: if frust { "frust" } else { "cargo" }, build, binary_blake3: hash,
                        stdout: String::from_utf8_lossy(&output).into_owned(), expected_edit: edit,
                        target_bytes: fsx::physical_size(&[&target]),
                        store_bytes: if frust { fsx::physical_size(&[&dirs.store()]) } else { 0 },
                        target_plus_store_bytes: fsx::physical_size(&roots),
                        managed_allocation: managed_allocation(&target, frust.then_some(ctx.home.as_path())),
                    });
                    move_dir(&target, &snapshots[arm])?;
                }
            }
            guard_disk(scratch)?;
        }
        let mut summaries = Vec::new();
        for phase in ["cold", "clean rebuild", "no-op", "edit"] {
            for tool in ["cargo", "frust"] {
                let mut values: Vec<_> = samples.iter().filter(|s| s.phase == phase && s.tool == tool).map(|s| s.build.millis).collect();
                let med = median(&mut values);
                summaries.push(ProfileSummary { phase, tool, median_ms: med, min_ms: values[0], max_ms: values[values.len() - 1], runs: values.len() });
            }
        }
        profiles.push(ProfileResult {
            name, lto, codegen_units: units, panic, incremental: false, cargo_arguments: args, samples, summaries,
            cargo_target_bytes: fsx::physical_size(&[&snapshots[0]]),
            frust_target_bytes: fsx::physical_size(&[&snapshots[1]]),
            frust_store_bytes: fsx::physical_size(&[&dirs.store()]),
            frust_target_plus_store_bytes: fsx::physical_size(&[&snapshots[1], &dirs.store()]),
            cargo_managed_allocation: managed_allocation(&snapshots[0], None),
            frust_managed_allocation: managed_allocation(&snapshots[1], Some(&ctx.home)),
            same_directory_binaries_identical: true, restored_binaries_identical: true, runtime_outputs_equal: true,
        });
    }
    let cargo_version = Command::new(&ctx.cargo).arg("--version").output().map_err(|e| e.to_string())?;
    if !cargo_version.status.success() { return Err("cargo --version failed".into()); }
    Ok(ProfileMatrix {
        evidence_schema_version: EVIDENCE_SCHEMA_VERSION,
        store_policy: store,
        store_completion_method: store.completion_method(),
        rustc_verbose: probe.rustc_verbose,
        cargo_version: String::from_utf8_lossy(&cargo_version.stdout).trim().to_string(),
        machine: machine_context(load_before), cache_compression: compression.to_string(),
        filesystem: sys::fs_type_name(&root), reflink_supported: fsx::reflink_supported(&root),
        warmup: "One untimed trivial compile per tool and profile; isolated targets and cache, outside the measured graph.",
        compiler_audit_available: ctx.audit.is_some(),
        compiler_audit_method: "Stable RUSTC shim retains every backend call. rustc_query_invocations contains version/help and known inspection-only --print queries, including queries with --emit; rustc_invocations and compilation counts contain the remaining calls. Compilation-bearing, unknown or malformed print requests are never hidden as queries. rustc_call_count counts both. Declared Cargo units have artifact-notification flags and normal unit output directories. Build-script capability probes require metadata-only output strictly inside profile/build/<unit-hash>/out/<subdirectory>, without Cargo artifact notifications; all other non-query calls are unknown. Warm restores require zero declared-unit and unknown compilations, zero ledger misses and positive cache hits; queries and compiler capability probes remain visible and contribute to wall time. cargo_compile_notices counts stderr Compiling diagnostics, not actual compiler invocations. No-ops require zero notices, cache hits/misses and all audited backend calls. Build wall times include audit shell startup/logging. Byte identity uses one source and target path; every measured binary is executed.",
        frontend_blake3: crate::cache::hash_file(&ctx.fcargo).map_err(|e| e.to_string())?,
        wrapper_blake3: crate::cache::hash_file(&wrapper).map_err(|e| e.to_string())?,
        local_crates: libs, registry_deps: registry, profiles,
    })
}

fn print_profile_matrix(report: &ProfileMatrix) {
    println!("Store policy: {} (build times measure foreground completion)", report.store_policy.label());
    for profile in &report.profiles {
        println!("{} (lto={}, codegen-units={}, panic={})", profile.name, profile.lto, profile.codegen_units, profile.panic);
        for summary in &profile.summaries {
            println!("  {:<14} {:<6} {}ms [{}–{}], n={}", summary.phase, summary.tool, summary.median_ms, summary.min_ms, summary.max_ms, summary.runs);
        }
        if report.store_policy == StorePolicy::Deferred {
            for phase in ["cold", "clean rebuild", "no-op", "edit"] {
                let mut drains: Vec<_> = profile.samples.iter().filter(|sample| sample.tool == "frust" && sample.phase == phase)
                    .filter_map(|sample| sample.build.store_drain_millis).collect();
                let mut settled: Vec<_> = profile.samples.iter().filter(|sample| sample.tool == "frust" && sample.phase == phase)
                    .filter_map(|sample| sample.build.settled_millis).collect();
                if !drains.is_empty() {
                    let drain_median = median(&mut drains);
                    let settled_median = median(&mut settled);
                    println!("  {phase:<14} frust observed store drain {}ms [{}–{}]; settled {}ms [{}–{}]", drain_median, drains[0], drains[drains.len()-1], settled_median, settled[0], settled[settled.len()-1]);
                }
            }
        }
        println!("  Cargo target {}; frust target plus store {}; binaries and runtime outputs matched", human_bytes(profile.cargo_target_bytes), human_bytes(profile.frust_target_plus_store_bytes));
        println!("  Cargo {}; frust {}", managed_allocation_text(&profile.cargo_managed_allocation), managed_allocation_text(&profile.frust_managed_allocation));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StorePolicy { Sync, Deferred }

impl StorePolicy {
    fn label(self) -> &'static str {
        match self { Self::Sync => "sync", Self::Deferred => "deferred" }
    }

    fn scratch_prefix(self, keep: bool) -> &'static str {
        if keep || self == Self::Deferred { "frust-bench-retained" } else { "frust-bench" }
    }

    fn completion_method(self) -> &'static str {
        match self {
            Self::Sync => "Build samples use synchronous cache storage; millis measures foreground completion including storage. Drain and settled fields are omitted.",
            Self::Deferred => "Build samples retain foreground millis, plus observed time from child exit until no job files remain and all existing slot locks are held exclusively together. Settled millis run from child start through that proof; helper counts and fallback frequency are not inferred. Maintenance rows and the separate churn section remain synchronous.",
        }
    }
}

fn store_from_flags(flags: &[String]) -> Result<StorePolicy, String> {
    let mut selected = None;
    for (index, flag) in flags.iter().enumerate() {
        let value = if flag == "--store" { Some(flags.get(index + 1).map(String::as_str).unwrap_or("")) }
            else { flag.strip_prefix("--store=") };
        let Some(value) = value else { continue };
        if selected.is_some() { return Err("--store may be specified only once".into()); }
        selected = Some(match value {
            "sync" => StorePolicy::Sync,
            "deferred" => StorePolicy::Deferred,
            _ => return Err(format!("--store requires sync or deferred, got `{value}`")),
        });
    }
    if selected.is_some() && flags.iter().any(|flag| matches!(flag.as_str(), "--growth" | "--churn" | "--linking" | "--latency")) {
        return Err("--store is supported only with the general benchmark or --profiles, without --growth, --churn, --linking or --latency".into());
    }
    Ok(selected.unwrap_or(StorePolicy::Sync))
}

fn compression_from_flags(flags: &[String]) -> Result<String, String> {
    let value = flags.iter().enumerate().find_map(|(i, flag)| {
        if flag == "--compression" { Some(flags.get(i + 1).map(String::as_str).unwrap_or("")) }
        else { flag.strip_prefix("--compression=") }
    }).unwrap_or("auto");
    match value {
        "auto" | "off" => Ok(value.to_string()),
        _ => Err(format!("--compression requires auto or off, got `{value}`")),
    }
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
    runtime_outputs_equal: Option<bool>,
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
    controlled_environment(&mut cmd);
    cmd.env("FRUST_CARGO", &ctx.cargo);
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
    let observations = target.join("link-observations");
    fs::create_dir_all(&observations)
        .map_err(|e| format!("cannot create {}: {e}", observations.display()))?;
    let stdout_path = observations.join(format!("edit-{edit}-order-{order}.stdout"));
    let stderr_path = observations.join(format!("edit-{edit}-order-{order}.stderr"));
    for (path, bytes) in [(&stdout_path, &output.stdout), (&stderr_path, &output.stderr)] {
        fs::write(path, bytes).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    }
    let raw_logs = format!("raw stdout: {}; raw stderr: {}", stdout_path.display(), stderr_path.display());
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Err(format!("linking benchmark {} build failed; {raw_logs}:\n{}", if smart { "smart" } else { "off" }, failure_tail(&stderr)));
    }
    let smart_policy_applied = stderr.lines().any(|l| l.starts_with("frust-wrap: smart link:"));
    if smart != smart_policy_applied {
        let mut diagnostics: Vec<String> = stderr.lines().rev()
            .filter(|line| line.starts_with("frust-wrap: smart link:") || line.starts_with("frust-wrap: link policy skipped:"))
            .take(8).map(|line| line.chars().take(512).collect()).collect();
        diagnostics.reverse();
        if diagnostics.is_empty() {
            diagnostics.push("no frust-wrap link policy diagnostics were emitted".into());
        }
        return Err(format!("linking benchmark policy evidence missing or unexpected (FRUST_LINK={}); native macOS host linking must be eligible; {raw_logs}:\n{}", if smart { "smart" } else { "off" }, diagnostics.join("\n")));
    }
    let mut compiled_units = 0;
    let mut main_bin_compiled_units = 0;
    let mut binary = None;
    for line in output.stdout.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
        let message: serde_json::Value = serde_json::from_slice(line).map_err(|e| format!("invalid Cargo JSON in linking benchmark: {e}; {raw_logs}"))?;
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
        return Err(format!("linking benchmark edit compiled no main binary; refusing a fresh/no-op sample; {raw_logs}"));
    }
    let binary = binary.ok_or_else(|| format!("Cargo did not report the compiled main executable; {raw_logs}"))?;
    let native_link_ms = native_link_millis(&stderr).map_err(|e| format!("{e}; {raw_logs}"))?;
    let stdout = run_output(&binary).map_err(|e| format!("{e}; {raw_logs}"))?;
    let marker = format!("frust-link-edit {edit}");
    let expected_output_present = stdout.split(|b| *b == b'\n').any(|line| line == marker.as_bytes());
    if !expected_output_present {
        return Err(format!("linking benchmark binary did not print expected edit {edit}; {raw_logs}"));
    }
    let binary_bytes = fs::metadata(&binary).map_err(|e| format!("cannot inspect {}: {e}; {raw_logs}", binary.display()))?.len();
    Ok((LinkSample { edit, order_in_pair: order, total_build_ms, native_link_ms, compiled_units, main_bin_compiled_units, binary_bytes, smart_policy_applied, expected_output_present }, stdout))
}

fn linking_bench(scratch: &Path, quick: bool, runs: usize) -> Result<LinkReport, String> {
    if !cfg!(any(target_os = "macos", target_os = "linux")) {
        return Err("--linking supports native macOS comparisons and Linux baseline profiling".into());
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
    let paired = cfg!(target_os = "macos");
    if paired && !probe.host.ends_with("-apple-darwin")
        || !paired && !probe.host.contains("-linux-")
    {
        return Err("--linking requires a compiler native to the current host".into());
    }
    let ctx = Ctx { cargo, fcargo, home: scratch.join("link-home"), probe_id: probe.id, audit: CompilerAudit::create(scratch)?, compression: "auto".into(), store: StorePolicy::Sync };
    let project = scratch.join("linking").join("app");
    let libs = if quick { 4 } else { 12 };
    let registry = write_growth_project(&project, libs, true).and_then(|_| resolve_offline(&ctx, &project)).is_ok();
    if !registry {
        write_growth_project(&project, libs, false)?;
    }
    let source = project.join("src").join("main.rs");
    let original = fs::read_to_string(&source).map_err(|e| e.to_string())?;
    let targets = [scratch.join("link-off-target"), scratch.join("link-smart-target")];
    let modes = if paired { 2 } else { 1 };
    let mut samples: Vec<Vec<LinkSample>> = (0..modes).map(|_| Vec::new()).collect();
    // Cold build and a nonzero body edit settle both incremental states before measurement.
    for step in 0..runs + 2 {
        fs::write(&source, link_edit_source(&original, step)?).map_err(|e| e.to_string())?;
        let order: Vec<_> = if paired && step % 2 != 0 { vec![1, 0] } else { (0..modes).collect() };
        let mut outputs: Vec<Vec<u8>> = (0..modes).map(|_| Vec::new()).collect();
        for (position, mode) in order.into_iter().enumerate() {
            let (sample, output) = linking_build(&ctx, &wrapper, &project, &targets[mode], mode == 1, step, position + 1)?;
            outputs[mode] = output;
            if step >= 2 {
                validate_measured_link(sample.compiled_units, sample.main_bin_compiled_units, sample.native_link_ms, sample.total_build_ms)?;
                samples[mode].push(sample);
            }
        }
        if paired && outputs[0] != outputs[1] {
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
        description: if paired {
            "Paired real main-function body edits of one application graph; identical source path, separate warmed incremental targets, alternating build order, direct binary execution after every build."
        } else {
            "Linux baseline: real main-function body edits of one application graph with warmed incremental state and direct binary execution after every build."
        },
        policy: if paired {
            "FRUST_LINK=smart selects Apple's native ld directly for eligible host debug binaries. The standard library remains statically linked; this is not native incremental linking."
        } else {
            "FRUST_LINK=off preserves the configured Linux linker. Only the baseline is measured; no smart-link transformation or incremental native-link reuse is claimed."
        },
        timing: "Total build wall time includes the frust frontend and Cargo; native_link_ms is the single current main-binary link_binary phase, with exactly one main-binary unit compiled per measured edit. A shared wrapper script profiles only the primary application using benchmark-only RUSTC_BOOTSTRAP=1 and -Ztime-passes; dependencies receive no profiling flags. Load and instrumentation can affect results.",
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
            separate_target_directories: paired,
        },
        artifact_cache: "off",
        build_lock: true,
        runtime_outputs_equal: paired.then_some(true),
        expected_edit_outputs_present: true,
        modes,
    })
}

fn print_linking(report: &LinkReport, s: &Styler) {
    let workload = if report.modes.len() == 2 { "paired body edits" } else { "baseline body edits" };
    println!("{}", s.bold(&format!("Native linking: {} {workload}, {} local crates{}", report.runs, report.sources.local_crates, if report.sources.registry_deps { " + registry dependencies" } else { "" })));
    println!("  {:<8} {:>22} {:>22} {:>10}", "mode", "build ms med [min,max]", "link ms med [min,max]", "binary");
    for mode in &report.modes {
        let stats = |v: &LinkStats| format!("{:.1} [{:.1},{:.1}]", v.median_ms, v.min_ms, v.max_ms);
        println!("  {:<8} {:>22} {:>22} {:>10}", mode.mode, stats(&mode.total_build), stats(&mode.native_link), human_bytes(mode.binary_bytes));
    }
    let m = &report.machine;
    let load = |v: Option<f64>| v.map_or("unavailable".to_string(), |l| format!("{l:.2}"));
    println!("  {}, {} ({} cores), rustc {}; load before {}, after {}", m.os, m.cpu, m.cores, report.rustc, load(m.load_before), load(m.load_after));
    if report.runtime_outputs_equal == Some(true) {
        println!("  Outputs match between modes for every edit.");
    }
    println!("  Changed runtime output verified; main binaries recompiled in every sample.");
    println!("  {}", report.policy);
    println!("  {}", report.timing);
}

// An untimed build of a trivial project per tool and profile, so first-launch costs of rustc, std, the linker and fcargo are not charged to whichever row runs first.
fn warm_up(ctx: &Ctx, scratch: &Path) -> Result<(), String> {
    let dir = scratch.join("warmup");
    let project = dir.join("app");
    write_project(&project, 0, false)?;
    let warm = Ctx { home: dir.join("home"), ..ctx.clone() };
    let target = dir.join("target");
    let profiles: [&[&str]; 2] = [&[], &["--release"]];
    for extra in profiles {
        plain_build_with(&warm, &project, &target, extra)?;
        frust_build_with(&warm, &project, extra)?;
    }
    fs::remove_dir_all(&dir).map_err(|e| e.to_string())
}

struct Row {
    samples: Vec<Run>,
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
                self.rows.push(Row { label, millis: Vec::new(), bytes: 0, samples: Vec::new() });
                self.rows.last_mut().expect("row was just pushed")
            }
        };
        row.millis.push(run.millis);
        row.samples.push(run);
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
                Measurement { label: r.label.to_string(), millis: med, min_millis: min, max_millis: max, runs: r.millis.len(), target_bytes: r.bytes, samples: r.samples.clone() }
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

fn bench(scratch: &Path, quick: bool, runs: usize, s: &Styler, quiet: bool, compression: &str, store: StorePolicy) -> Result<Report, String> {
    fs::create_dir_all(scratch).map_err(|e| e.to_string())?;
    let cargo = resolve_tool("cargo").ok_or("cargo not found")?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let fcargo = fcargo_beside(&exe);
    if !fcargo.exists() {
        return Err("fcargo binary not found next to frust; install all binaries".into());
    }
    let probe = Probe::run(None, scratch).ok_or("could not query rustc")?;
    let ctx = Ctx { cargo, fcargo, home: scratch.join("frust-home"), probe_id: probe.id.clone(), audit: CompilerAudit::create(scratch)?, compression: compression.to_string(), store };
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
        verify_edit_pair(&app_binary(&ta.join("debug")), &app_binary(&ca.join("debug")), rep as u32 + 1)?;

        // Measure warm-session edits separately from the first edit after artifact restoration.
        touch_app(&proj_a, rep as u32 + 10_000)?;
        for is_cargo in order {
            if is_cargo {
                t.add(L_CARGO_INC_WARM, plain_build(&ctx, &proj_a, &ta)?, None);
            } else {
                t.add(L_FRUST_INC_WARM, frust_build(&ctx, &proj_a)?, None);
            }
        }
        verify_edit_pair(&app_binary(&ta.join("debug")), &app_binary(&ca.join("debug")), rep as u32 + 10_000)?;

        if last {
            let (gc_ms, compact_ms) = measure_maintenance(&ctx, &dirs, &ca)?;
            t.add(L_MAINT_GC, Run { millis: gc_ms, ..Run::default() }, None);
            t.add(L_MAINT_COMPACT, Run { millis: compact_ms, ..Run::default() }, None);
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
        evidence_schema_version: EVIDENCE_SCHEMA_VERSION,
        store_policy: store,
        store_completion_method: store.completion_method(),
        rustc: probe.release,
        cache_compression: ctx.compression.clone(),
        legacy_release_profile: "opt-level=3, full/fat LTO, codegen-units=1, panic=abort, incremental=false",
        filesystem: sys::fs_type_name(scratch),
        maintenance_auto_compaction_policy: AutoCompactionPolicy::from(&benchmark_auto_options(true)),
        maintenance_gc_cache_max_bytes: benchmark_maintenance_config().cache_max_bytes,
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
        profile_matrix: profile_matrix(scratch, quick, runs, &ctx.compression, store)?,
    })
}

// Run once on the populated state, outside the speed rows, which turn both inline passes off.
fn measure_maintenance(ctx: &Ctx, dirs: &Dirs, target: &Path) -> Result<(u64, u64), String> {
    let cfg = benchmark_maintenance_config();
    clear(&dirs.auto_gc_marker())?;
    let started = Instant::now();
    let ran = gc::maybe_auto_gc(dirs, &cfg);
    let gc_ms = started.elapsed().as_millis() as u64;
    if !ran {
        return Err(format!("the daily gc pass did not run in {}", ctx.home.display()));
    }
    let opts = benchmark_auto_options(true);
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
    let mut errors = failed.into_iter().map(str::to_string).collect::<Vec<_>>();
    if let Some(churn) = &r.churn
        && let Err(error) = validate_churn(churn) { errors.push(error); }
    for row in &r.cases {
        let phase = match row.label.as_str() {
            L_CARGO_A | L_FRUST_A | L_REL_CARGO_A | L_REL_FRUST_A => "cold",
            L_CARGO_CLEAN | L_FRUST_CLEAN | L_FRUST_DISPLAY | L_REL_CARGO_CLEAN | L_REL_FRUST_CLEAN => "clean rebuild",
            L_CARGO_NOOP | L_FRUST_NOOP => "no-op",
            L_CARGO_INC | L_FRUST_INC | L_CARGO_INC_WARM | L_FRUST_INC_WARM => "edit",
            _ => continue,
        };
        for (rep, sample) in row.samples.iter().enumerate() {
            if let Err(error) = validate_build_evidence(sample, phase, row.label.starts_with("frust")) {
                errors.push(evidence_failure(sample, &format!("{}, repetition {}", row.label, rep + 1), &error));
            }
        }
    }
    (!errors.is_empty()).then(|| errors.join("; "))
}

fn validate_growth(report: &GrowthReport) -> Result<(), String> {
    if !report.stopped.is_empty() { return Err(format!("growth variants stopped: {:?}", report.stopped)); }
    if report.outputs.is_empty() || report.repeat_builds.len() != report.outputs.len() {
        return Err("growth benchmark did not complete every variant".into());
    }
    if report.edit_outputs.len() != report.outputs.len() {
        return Err("growth benchmark did not record every variant's edit output".into());
    }
    for (variant, build) in &report.repeat_builds {
        if report.compiler_audit_available && (build.rustc_invocations.is_none()
            || build.rustc_query_invocations.is_none() || build.rustc_call_count.is_none()) {
            return Err("unchanged growth rebuild is missing available compiler audit evidence".into());
        }
        validate_build_evidence(build, "no-op", *variant != GrowthVariant::Cargo)
            .map_err(|reason| evidence_failure(build, &format!("{} unchanged growth rebuild", variant.label()), &reason))?;
    }
    if report.edit_policy == "settled" && (report.source_settlements.len() != report.outputs.len()
        || report.source_settlements.iter().any(|(_, checks)| checks.len() != report.edits + 1
            || checks.iter().any(|check| check.sources != report.libs + 1 || check.minimum_settle_slack_ns <= 0))) {
        return Err("settled growth is missing safe source-stamp evidence for a build".into());
    }
    let expected = &report.outputs[0].1;
    if report.outputs.iter().any(|(_, output)| output != expected) {
        return Err("growth variants produced different final output".into());
    }
    if let Some((_, expected)) = report.edit_outputs.first()
        && report.edit_outputs.iter().any(|(_, outputs)| outputs != expected)
    {
        return Err("growth variants produced different output during the edit sequence".into());
    }
    Ok(())
}

fn validate_churn(report: &ChurnReport) -> Result<(), String> {
    if !report.binary_output_unchanged || report.frust_compile_notices_after_compact != 0
        || report.cargo_compile_notices_after_compress != 0 || report.auto_compile_notices_on_repeat != 0
    {
        return Err("churn maintenance changed program output or invalidated unchanged build freshness".into());
    }
    if report.release_clean_frust_cache_hits == 0 || report.release_clean_frust_cache_misses != 0 {
        return Err("churn release clean rebuild did not restore entirely from warm cache".into());
    }
    if report.release_clean_frust_rustc_breakdown.is_some_and(|counts| counts.cargo_units != 0 || counts.unknown != 0) {
        return Err("churn release warm restore executed declared Cargo-unit or unknown rustc compilations".into());
    }
    if let Some(growth) = &report.growth { validate_growth(growth)?; }
    Ok(())
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

fn churn_bench(scratch: &Path, quick: bool, s: &Styler, quiet: bool, compression: &str, keep_scratch: bool) -> Result<ChurnReport, String> {
    fs::create_dir_all(scratch).map_err(|e| e.to_string())?;
    let cargo = resolve_tool("cargo").ok_or("cargo not found")?;
    let fcargo = fcargo_beside(&std::env::current_exe().map_err(|e| e.to_string())?);
    let probe = Probe::run(None, scratch).ok_or("could not query rustc")?;
    let ctx = Ctx { cargo, fcargo, home: scratch.join("frust-home"), probe_id: probe.id.clone(), audit: CompilerAudit::create(scratch)?, compression: compression.to_string(), store: StorePolicy::Sync };
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
    let auto_ctx = Ctx { home: scratch.join("frust-auto-home"), ..ctx.clone() };
    let auto_dirs = Dirs::at(&auto_ctx.home);
    let auto_central = workspace::central_target(&auto_dirs, Some(&canonical(&proj)), &ctx.probe_id);
    let auto_opts = benchmark_auto_options(false);
    let auto_build = |args: &[&str], envs: &[(&str, &str)]| -> Result<usize, String> {
        let (_, notices) = build_logged(&auto_ctx, true, &proj, &auto_central, args, envs)?;
        compact::auto_compact_with(&auto_dirs, &auto_central, &auto_opts);
        Ok(notices)
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
        let profile = if config.args.contains(&"--release") { "release" } else { "debug" };
        verify_edit_pair(&app_binary(&cargo_target.join(profile)), &app_binary(&central.join(profile)), 0)?;
        verify_edit_pair(&app_binary(&cargo_target.join(profile)), &app_binary(&auto_central.join(profile)), 0)?;
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
    let (_, notices_after_compact) = build_logged(&ctx, true, &proj, &central, last.args, &envs)?;
    let output_after = run_output(&app_binary(&central.join(profile)))?;

    let auto_compile_notices_on_repeat = auto_build(last.args, &envs)?;
    guard_disk(scratch)?;

    let rel: &[&str] = &["--release", "--features", "extra"];
    build_logged(&ctx, false, &proj, &cargo_target, rel, &[])?;
    build_logged(&ctx, true, &proj, &central, rel, &[])?;
    compact::compact(&dirs, std::slice::from_ref(&central), &CompactOptions::after_build());
    fsx::remove_any(&cargo_target.join("release")).map_err(|e| e.to_string())?;
    fsx::remove_any(&central.join("release")).map_err(|e| e.to_string())?;
    let (rel_cargo, _) = build_logged(&ctx, false, &proj, &cargo_target, rel, &[])?;
    let hits_before = crate::stats::totals(&crate::stats::read_ledger(&dirs));
    let (rel_frust, rel_frust_notices) = build_logged(&ctx, true, &proj, &central, rel, &[])?;
    validate_build_evidence(&rel_frust, "clean rebuild", true)
        .map_err(|reason| evidence_failure(&rel_frust, "churn release warm restore", &reason))?;
    verify_edit_pair(&app_binary(&cargo_target.join("release")), &app_binary(&central.join("release")), 0)?;
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
        verify_edit_pair(&app_binary(&cargo_target.join("debug")), &app_binary(&central.join("debug")), 100 + i as u32)?;
        verify_edit_pair(&app_binary(&cargo_target.join("debug")), &app_binary(&auto_central.join("debug")), 100 + i as u32)?;
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
    let (_, cargo_notices_after_compress) = build_logged(&ctx, false, &proj, &cargo_target, base, &[])?;
    guard_disk(scratch)?;

    let growth_opts = GrowthOptions {
        edits: if quick { 30 } else { 100 },
        every: if quick { 10 } else { 20 },
        libs: libs.max(2),
        variants: vec![GrowthVariant::Cargo, GrowthVariant::Auto],
        max_scratch: max_scratch_bytes(),
        compression: compression.to_string(),
    };
    let growth = growth_bench(&scratch.join("growth-loop"), &growth_opts, s, quiet, keep_scratch, false)?;

    let result = ChurnReport {
        evidence_schema_version: EVIDENCE_SCHEMA_VERSION,
        store_policy: StorePolicy::Sync,
        cache_compression: compression.to_string(),
        auto_compaction_policy: AutoCompactionPolicy::from(&auto_opts),
        configs,
        cargo_target: cargo_size,
        cargo_target_compressed: cargo_compressed,
        cargo_compile_notices_after_compress: cargo_notices_after_compress,
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
        frust_compile_notices_after_compact: notices_after_compact,
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
        auto_compile_notices_on_repeat,
        release_clean_cargo_ms: rel_cargo.millis,
        release_clean_frust_ms: rel_frust.millis,
        release_clean_frust_compile_notices: rel_frust_notices,
        release_clean_frust_rustc_compilations: rel_frust.rustc_invocations.as_ref().map(Vec::len),
        release_clean_frust_rustc_breakdown: rel_frust.rustc_compilation_counts,
        release_clean_frust_cache_hits: hits_after.hits - hits_before.hits,
        release_clean_frust_cache_misses: hits_after.misses - hits_before.misses,
        growth: Some(growth),
    };
    validate_churn(&result)?;
    Ok(result)
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
    pub target_plus_store: u64,
    pub managed_allocation: ManagedAllocation,
    pub rustc_compilations: Option<usize>,
    pub source_settle_ms: u64,
    pub cache_evidence: Option<GrowthCacheEvidence>,
    pub incremental_dir: u64,
    pub anatomy: compact::IncrementalAnatomy,
    pub crate_dirs: Vec<(String, u64)>,
    pub cargo_compile_notices: usize,
    pub build_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct GrowthReport {
    pub evidence_schema_version: u32,
    pub cache_compression: String,
    pub edit_policy: &'static str,
    pub source_settle_timeout_ms: Option<u64>,
    pub source_settlements: Vec<(GrowthVariant, Vec<SourceSettlement>)>,
    pub cache_evidence_method: &'static str,
    pub compiler_audit_available: bool,
    pub auto_compaction_policy: AutoCompactionPolicy,
    pub libs: usize,
    pub registry_deps: bool,
    pub edits: usize,
    pub every: usize,
    pub rows: Vec<GrowthRow>,
    pub repeat_builds: Vec<(GrowthVariant, Run)>,
    pub outputs: Vec<(GrowthVariant, String)>,
    pub edit_outputs: Vec<(GrowthVariant, Vec<String>)>,
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
    pub compression: String,
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
            compression: compression_from_flags(flags).unwrap_or_else(|_| "auto".into()),
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

const SOURCE_SETTLE_TIMEOUT: Duration = Duration::from_secs(15);

fn settle_edits_from_flags(flags: &[String]) -> Result<bool, String> {
    let selected: Vec<_> = flags.iter().filter(|flag| flag.starts_with("--settle-edits")).collect();
    if selected.is_empty() { return Ok(false); }
    if selected.len() != 1 || selected[0] != "--settle-edits" || !flags.iter().any(|flag| flag == "--growth")
        || flags.iter().any(|flag| matches!(flag.as_str(), "--profiles" | "--linking" | "--latency" | "--churn")) {
        return Err("--settle-edits is a single flag supported only with --growth".into());
    }
    Ok(true)
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceSettlement {
    sources: usize,
    coarse_sources: usize,
    checked_ns: i128,
    minimum_settle_slack_ns: i128,
    wait_millis: u64,
}

fn source_settle_slack(stamp: &crate::cache::hashing::Stamp, now: i128) -> i128 {
    let margin = if stamp.coarse() { crate::cache::hashing::SETTLE_MARGIN_NS } else { 100_000_000 };
    now - margin - stamp.mtime_ns.max(stamp.ctime_ns)
}

type GrowthSourceStamps = Vec<(PathBuf, crate::cache::hashing::Stamp)>;

// Settlement waits observe real stamps; the build clock starts only after the unchanged sources pass the compiler guard.
fn wait_sources_settled(paths: &[PathBuf], timeout: Duration) -> Result<(SourceSettlement, GrowthSourceStamps), String> {
    let started = Instant::now();
    loop {
        let stamps = paths.iter().map(|path| crate::cache::hashing::Stamp::read(path)
            .map(|stamp| (path.clone(), stamp)).map_err(|error| format!("{}: {error}", path.display())))
            .collect::<Result<Vec<_>, _>>()?;
        let now = crate::cache::hashing::now_ns();
        if started.elapsed() > timeout {
            return Err(format!("growth sources did not settle within {}ms; timestamps or source activity remain unsafe", timeout.as_millis()));
        }
        if stamps.iter().all(|(_, stamp)| !stamp.too_new_for(now))
            && stamps.iter().all(|(path, stamp)| crate::cache::hashing::Stamp::read(path).ok().as_ref() == Some(stamp)) {
            return Ok((SourceSettlement {
                sources: stamps.len(), coarse_sources: stamps.iter().filter(|(_, stamp)| stamp.coarse()).count(), checked_ns: now,
                minimum_settle_slack_ns: stamps.iter().map(|(_, stamp)| source_settle_slack(stamp, now)).min().unwrap_or(0),
                wait_millis: started.elapsed().as_millis() as u64,
            }, stamps));
        }
        let Some(remaining) = timeout.checked_sub(started.elapsed()).filter(|remaining| !remaining.is_zero()) else {
            return Err(format!("growth sources did not settle within {}ms; timestamps or source activity remain unsafe", timeout.as_millis()));
        };
        thread::sleep(remaining.min(Duration::from_millis(10)));
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct GrowthCacheEvidence {
    nonzero_byte_miss_records: u64,
    zero_byte_miss_records: u64,
    observed_entry_keys: usize,
    observed_entries_by_crate: std::collections::BTreeMap<String, usize>,
    observed_entry_keys_by_crate: std::collections::BTreeMap<String, Vec<String>>,
    retained_complete_entries: usize,
    retained_unit_records: usize,
    retained_unit_keys: usize,
    retained_unit_missing_entries: usize,
    max_retained_keys_per_unit: usize,
    retained_keys_per_unit_limit: usize,
    retired_payload_entries: usize,
}

fn growth_directory_entries(path: &Path) -> Result<Vec<PathBuf>, String> {
    match sys::lstat(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("{}: {error}", path.display())),
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {}
        Ok(_) => return Err(format!("unexpected growth evidence directory {}", path.display())),
    }
    fs::read_dir(path).map_err(|error| format!("{}: {error}", path.display()))?
        .map(|entry| entry.map(|entry| entry.path()).map_err(|error| error.to_string())).collect()
}

struct GrowthLedgerSnapshot {
    text: String,
    identity: Option<(u64, u64)>,
    _file: Option<fs::File>,
}

fn growth_ledger(dirs: &Dirs) -> Result<GrowthLedgerSnapshot, String> {
    let path = dirs.ledger();
    let named = match sys::lstat(&path) {
        Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() && meta.ino() != 0 => meta,
        Ok(_) => return Err("growth ledger is not an identifiable regular file".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(GrowthLedgerSnapshot { text: String::new(), identity: None, _file: None });
        }
        Err(error) => return Err(error.to_string()),
    };
    let expected = crate::cache::hashing::Stamp::of(&named);
    let mut file = fs::File::open(&path).map_err(|error| error.to_string())?;
    let matches = |meta: &sys::Meta| meta.is_file() && !meta.file_type().is_symlink()
        && crate::cache::hashing::Stamp::of(meta) == expected;
    if !matches(&sys::fstat(&file).map_err(|error| error.to_string())?) {
        return Err("growth ledger changed while opening its snapshot".into());
    }
    let mut text = String::new();
    file.read_to_string(&mut text).map_err(|error| error.to_string())?;
    if !matches(&sys::fstat(&file).map_err(|error| error.to_string())?)
        || !matches(&sys::lstat(&path).map_err(|error| error.to_string())?) {
        return Err("growth ledger changed while reading its snapshot".into());
    }
    Ok(GrowthLedgerSnapshot { text, identity: Some((expected.dev, expected.ino)), _file: Some(file) })
}

fn growth_ledger_added<'a>(before: &GrowthLedgerSnapshot, after: &'a GrowthLedgerSnapshot) -> Result<&'a str, String> {
    if before.identity.is_some() && before.identity != after.identity {
        return Err("growth ledger identity changed; cache outcome counts are unavailable".into());
    }
    after.text.strip_prefix(&before.text)
        .ok_or_else(|| "growth ledger changed instead of appending; cache outcome counts are unavailable".into())
}

fn growth_store_records(dirs: &Dirs) -> Result<Vec<(String, crate::cache::EntryMeta)>, String> {
    let mut records = Vec::new();
    for shard in growth_directory_entries(&dirs.store())? {
        for entry in growth_directory_entries(&shard)? {
            let key = entry.file_name().and_then(|name| name.to_str()).ok_or("non-UTF-8 growth entry key")?;
            if !crate::cache::valid_key(key) { return Err("invalid growth cache entry key".into()); }
            if !sys::lstat(&entry).is_ok_and(|meta| meta.is_dir() && !meta.file_type().is_symlink()) {
                return Err("invalid growth cache entry directory".into());
            }
            let metadata = entry.join("meta.json");
            if !sys::lstat(&metadata).is_ok_and(|meta| meta.is_file()) { return Err(format!("missing growth entry record {}", metadata.display())); }
            let record: crate::cache::EntryMeta = serde_json::from_slice(&fs::read(&metadata).map_err(|error| error.to_string())?)
                .map_err(|error| format!("{}: {error}", metadata.display()))?;
            let groups = crate::cache::metadata_slice::validate_groups(&record.files).map_err(|error| error.to_string())?;
            if !crate::cache::safe_file_name(&record.stderr)
                || !sys::lstat(&entry.join(&record.stderr)).is_ok_and(|meta| meta.is_file()) {
                return Err("invalid growth entry stderr".into());
            }
            for group in groups {
                let backing = &record.files[group.base_index];
                let meta = sys::lstat(&entry.join(&backing.stored)).map_err(|error| error.to_string())?;
                if meta.file_type().is_symlink() || if backing.dir { !meta.is_dir() } else { !meta.is_file() } {
                    return Err("invalid growth entry backing".into());
                }
            }
            records.push((key.to_string(), record));
        }
    }
    Ok(records)
}

// Retention and allocation describe the same post-maintenance sampling point.
fn growth_cache_retention(dirs: &Dirs, evidence: &mut GrowthCacheEvidence) -> Result<(), String> {
    let records = growth_store_records(dirs)?;
    let current: std::collections::HashSet<_> = records.iter().map(|(key, _)| key.as_str()).collect();
    evidence.retained_complete_entries = records.len();
    let mut unit_records = 0;
    let mut unit_keys = 0;
    let mut missing = 0;
    let mut maximum = 0;
    for shard in growth_directory_entries(&dirs.root().join(crate::cache::UNITS_DIR))? {
        for path in growth_directory_entries(&shard)? {
            if !sys::lstat(&path).is_ok_and(|meta| meta.is_file()) { return Err("invalid growth unit record".into()); }
            let text = fs::read_to_string(&path).map_err(|error| error.to_string())?;
            let keys: Vec<_> = text.lines().collect();
            if keys.iter().any(|key| !crate::cache::valid_key(key)) { return Err("invalid growth unit entry key".into()); }
            unit_records += 1;
            unit_keys += keys.len();
            missing += keys.iter().filter(|key| !current.contains(**key)).count();
            maximum = maximum.max(keys.len());
        }
    }
    if maximum > crate::wrapper::UNIT_VERSIONS { return Err("growth unit version retention exceeded its limit".into()); }
    evidence.retained_unit_records = unit_records;
    evidence.retained_unit_keys = unit_keys;
    evidence.retained_unit_missing_entries = missing;
    evidence.max_retained_keys_per_unit = maximum;
    evidence.retained_keys_per_unit_limit = crate::wrapper::UNIT_VERSIONS;
    evidence.retired_payload_entries = growth_directory_entries(&crate::cache::evicted_dir(dirs))?.len();
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

fn growth_bench(scratch: &Path, opts: &GrowthOptions, s: &Styler, quiet: bool, keep_scratch: bool, settle_edits: bool) -> Result<GrowthReport, String> {
    fs::create_dir_all(scratch).map_err(|e| e.to_string())?;
    let cargo = resolve_tool("cargo").ok_or("cargo not found")?;
    let fcargo = fcargo_beside(&std::env::current_exe().map_err(|e| e.to_string())?);
    if !fcargo.exists() {
        return Err("fcargo binary not found next to frust; install all binaries".into());
    }
    let probe = Probe::run(None, scratch).ok_or("could not query rustc")?;
    let audit = CompilerAudit::create(scratch)?;
    let proj = scratch.join("growth").join("app");
    let registry = write_growth_project(&proj, opts.libs, true)
        .and_then(|_| resolve_offline(&Ctx { cargo: cargo.clone(), fcargo: fcargo.clone(), home: scratch.join("unused"), probe_id: probe.id.clone(), audit: None, compression: opts.compression.clone(), store: StorePolicy::Sync }, &proj))
        .is_ok();
    let mut report = GrowthReport {
        evidence_schema_version: EVIDENCE_SCHEMA_VERSION,
        cache_compression: opts.compression.clone(),
        edit_policy: if settle_edits { "settled" } else { "immediate" },
        source_settle_timeout_ms: settle_edits.then_some(SOURCE_SETTLE_TIMEOUT.as_millis() as u64),
        source_settlements: Vec::new(),
        cache_evidence_method: "Settled mode observes new entry keys after each synchronous build, validating descriptors and declared backing/stderr presence; this is not a full restore integrity test. Checked ledger append deltas count nonzero-byte and zero-byte misses without claiming publication from miss bytes; replacement or truncation fails the diagnostic. Cumulative key and miss observations precede automatic maintenance. Retention and allocation are sampled together after maintenance; the final snapshot includes the unchanged rebuild without adding its latency or compilation counters to the edit row. Unit records expose retained keys and missing entries. Other modes do not collect this diagnostic. Allocation and source-settle waiting are outside the build clock.",
        compiler_audit_available: audit.is_some(),
        auto_compaction_policy: AutoCompactionPolicy::from(&benchmark_auto_options(false)),
        libs: opts.libs,
        registry_deps: registry,
        edits: opts.edits,
        every: opts.every,
        rows: Vec::new(),
        repeat_builds: Vec::new(),
        outputs: Vec::new(),
        edit_outputs: Vec::new(),
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
            "  {:<11} {:>5} {:>10} {:>10} {:>9} {:>5} {:>10} {:>10} {:>10} {:>10} {:>13}",
            "variant", "edits", "target", "incr dir", "sessions", "dirs", "stale objs", "stale sess", "dead dirs", "store", "Cargo notices"
        );
    }
    for &variant in &opts.variants {
        // Each variant replays the same edit script on a fresh project at the same source path.
        let project_parent = scratch.join("growth");
        if project_parent.exists() {
            fs::remove_dir_all(&project_parent).map_err(|e| e.to_string())?;
        }
        write_growth_project(&proj, opts.libs, registry)?;
        let home = scratch.join(format!("growth-home-{}", variant.label().replace('+', "-")));
        let ctx = Ctx { cargo: cargo.clone(), fcargo: fcargo.clone(), home: home.clone(), probe_id: probe.id.clone(), audit: audit.clone(), compression: opts.compression.clone(), store: StorePolicy::Sync };
        if registry {
            resolve_offline(&ctx, &proj)?;
        }
        let dirs = Dirs::at(&home);
        let target = match variant {
            GrowthVariant::Cargo => scratch.join("growth-cargo-target"),
            _ => workspace::central_target(&dirs, Some(&canonical(&proj)), &probe.id),
        };
        let mut run = GrowthRun::new(&ctx, variant, &proj, &target, &dirs);
        run.settle_edits = settle_edits;
        let mut script = GrowthScript { project: proj.clone(), libs: opts.libs, snapshot: Vec::new(), features: false };
        let mut outcome = run.settle_sources(&script.sources()).and_then(|_| run.build(script.args()).map(|_| ()));
        let mut step = 0;
        while outcome.is_ok() {
            if step % opts.every == 0 || step == opts.edits {
                let row = match run.row(step) {
                    Ok(row) => row,
                    Err(error) => { outcome = Err(error); break; }
                };
                if !quiet && step != opts.edits { print_growth_row(&row); }
                report.rows.push(row);
            }
            if step == opts.edits {
                break;
            }
            outcome = guard_scratch(scratch, opts.max_scratch).and_then(|_| {
                step += 1;
                script.step(step)?;
                run.settle_sources(&script.sources())?;
                run.build(script.args()).map(|_| ())
            });
        }
        match outcome {
            Ok(()) => {
                report.repeat_builds.push((variant, run.build(script.args())?));
                let final_row = report.rows.last_mut().ok_or("growth benchmark is missing its final row")?;
                run.refresh_row_storage(final_row)?;
                if !quiet { print_growth_row(final_row); }
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
        report.edit_outputs.push((variant, run.outputs.clone()));
        report.source_settlements.push((variant, run.source_settlements.clone()));
        cleanup_growth_variant(&home, &target, keep_scratch)?;
    }
    validate_growth(&report)?;
    Ok(report)
}

fn cleanup_growth_variant(home: &Path, target: &Path, keep_scratch: bool) -> Result<(), String> {
    if !keep_scratch {
        for path in [home, target] {
            if path.exists() { fs::remove_dir_all(path).map_err(|error| error.to_string())?; }
        }
    }
    Ok(())
}

struct GrowthRun<'a> {
    ctx: &'a Ctx,
    variant: GrowthVariant,
    project: &'a Path,
    target: &'a Path,
    dirs: &'a Dirs,
    auto: AutoOptions,
    cargo_compile_notices: usize,
    build_ms: u64,
    rustc_compilations: Option<usize>,
    outputs: Vec<String>,
    settle_edits: bool,
    source_settle_ms: u64,
    source_settlements: Vec<SourceSettlement>,
    settled_sources: GrowthSourceStamps,
    cache_evidence: GrowthCacheEvidence,
    observed_entries: std::collections::HashSet<String>,
}

impl<'a> GrowthRun<'a> {
    fn new(ctx: &'a Ctx, variant: GrowthVariant, project: &'a Path, target: &'a Path, dirs: &'a Dirs) -> GrowthRun<'a> {
        let auto = benchmark_auto_options(false);
        GrowthRun { ctx, variant, project, target, dirs, auto, cargo_compile_notices: 0, build_ms: 0, rustc_compilations: ctx.audit.as_ref().map(|_| 0), outputs: Vec::new(),
            settle_edits: false, source_settle_ms: 0, source_settlements: Vec::new(), settled_sources: Vec::new(),
            cache_evidence: GrowthCacheEvidence::default(), observed_entries: std::collections::HashSet::new() }
    }

    fn settle_sources(&mut self, sources: &[PathBuf]) -> Result<(), String> {
        if !self.settle_edits { return Ok(()); }
        let (proof, stamps) = wait_sources_settled(sources, SOURCE_SETTLE_TIMEOUT)?;
        self.source_settle_ms += proof.wait_millis;
        self.source_settlements.push(proof);
        self.settled_sources = stamps;
        Ok(())
    }

    fn revalidate_settled_sources(&self) -> Result<(), String> {
        for (path, stamp) in &self.settled_sources {
            if crate::cache::hashing::Stamp::read(path).ok().as_ref() != Some(stamp) {
                return Err(format!("settled growth source changed unexpectedly: {}", path.display()));
            }
        }
        Ok(())
    }

    fn observe_cache(&mut self, before: &GrowthLedgerSnapshot) -> Result<(), String> {
        let after = growth_ledger(self.dirs)?;
        let added = growth_ledger_added(before, &after)?;
        for line in added.lines() {
            let event: crate::events::Event = serde_json::from_str(line).map_err(|error| error.to_string())?;
            if let crate::events::Event::Cache { result: crate::events::CacheResult::Miss, bytes, .. } = event {
                if bytes == 0 { self.cache_evidence.zero_byte_miss_records += 1; }
                else { self.cache_evidence.nonzero_byte_miss_records += 1; }
            }
        }
        let records = growth_store_records(self.dirs)?;
        for (key, record) in &records {
            if self.observed_entries.insert(key.clone()) {
                *self.cache_evidence.observed_entries_by_crate.entry(record.crate_name.clone()).or_default() += 1;
                self.cache_evidence.observed_entry_keys_by_crate.entry(record.crate_name.clone()).or_default().push(key.clone());
            }
        }
        for keys in self.cache_evidence.observed_entry_keys_by_crate.values_mut() { keys.sort(); }
        self.cache_evidence.observed_entry_keys = self.observed_entries.len();
        Ok(())
    }

    /// fcargo's own automatic compaction is off here so only the variant's policy applies.
    fn build(&mut self, args: &[&str]) -> Result<Run, String> {
        let frust = self.variant != GrowthVariant::Cargo;
        let before = if self.settle_edits && frust { Some(growth_ledger(self.dirs)?) } else { None };
        self.revalidate_settled_sources()?;
        let (run, n) = build_logged(self.ctx, frust, self.project, self.target, args, &[("FRUST_AUTO_COMPACT", "off")])?;
        self.revalidate_settled_sources()?;
        if let Some(before) = before { self.observe_cache(&before)?; }
        if self.variant == GrowthVariant::Auto {
            compact::auto_compact_with(self.dirs, self.target, &self.auto);
        }
        self.cargo_compile_notices += n;
        self.rustc_compilations = self.rustc_compilations.zip(run.rustc_invocations.as_ref())
            .map(|(compiled, invocations)| compiled + invocations.len());
        let output = run_output(&app_binary(&self.target.join("debug")))?;
        self.outputs.push(String::from_utf8_lossy(&output).into_owned());
        self.build_ms += run.millis;
        Ok(run)
    }

    fn row(&self, edits: usize) -> Result<GrowthRow, String> {
        let mut cache_evidence = (self.settle_edits && self.variant != GrowthVariant::Cargo).then(|| self.cache_evidence.clone());
        if let Some(evidence) = &mut cache_evidence { growth_cache_retention(self.dirs, evidence)?; }
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
        Ok(GrowthRow {
            variant: self.variant,
            edits,
            target: compact::physical_size(&[self.target]),
            store,
            target_plus_store: if self.variant == GrowthVariant::Cargo { compact::physical_size(&[self.target]) } else { compact::physical_size(&[self.target, &self.dirs.store()]) },
            managed_allocation: managed_allocation(self.target, (self.variant != GrowthVariant::Cargo).then_some(self.ctx.home.as_path())),
            rustc_compilations: self.rustc_compilations,
            source_settle_ms: self.source_settle_ms,
            cache_evidence,
            incremental_dir: compact::physical_size(&[&incremental]),
            anatomy: compact::incremental_anatomy(&profile),
            crate_dirs,
            cargo_compile_notices: self.cargo_compile_notices,
            build_ms: self.build_ms,
        })
    }

    // A no-op can finish maintenance; its storage snapshot must preserve the edit-build counters.
    fn refresh_row_storage(&self, row: &mut GrowthRow) -> Result<(), String> {
        let snapshot = self.row(row.edits)?;
        row.target = snapshot.target;
        row.store = snapshot.store;
        row.target_plus_store = snapshot.target_plus_store;
        row.managed_allocation = snapshot.managed_allocation;
        row.cache_evidence = snapshot.cache_evidence;
        row.incremental_dir = snapshot.incremental_dir;
        row.anatomy = snapshot.anatomy;
        row.crate_dirs = snapshot.crate_dirs;
        Ok(())
    }
}

fn print_growth_row(r: &GrowthRow) {
    let a = &r.anatomy;
    println!(
        "  {:<11} {:>5} {:>10} {:>10} {:>9} {:>5} {:>10} {:>10} {:>10} {:>10} {:>13}",
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
        r.cargo_compile_notices
    );
    println!("    {}", managed_allocation_text(&r.managed_allocation));
    if let Some(evidence) = &r.cache_evidence {
        println!("    {}ms source-settle wait outside builds; {} observed entry keys, {} retained entries; unit keys max {}/{}; misses {} nonzero-byte / {} zero-byte; {} retired payload entries",
            r.source_settle_ms, evidence.observed_entry_keys, evidence.retained_complete_entries, evidence.max_retained_keys_per_unit,
            evidence.retained_keys_per_unit_limit, evidence.nonzero_byte_miss_records, evidence.zero_byte_miss_records, evidence.retired_payload_entries);
    }
}

fn print_growth(g: &GrowthReport, s: &Styler) {
    println!();
    println!("{}", s.bold(&format!("Growth after {} edits", g.edits)));
    let last = |v: GrowthVariant| g.rows.iter().rfind(|r| r.variant == v);
    let first = |v: GrowthVariant| g.rows.iter().find(|r| r.variant == v);
    for v in [GrowthVariant::Cargo, GrowthVariant::Frust, GrowthVariant::Auto] {
        let (Some(a), Some(b)) = (first(v), last(v)) else { continue };
        println!(
            "  {:<11} target {} -> {} after {} edits (incremental state {} -> {}), {} Cargo compile notices, {} actual rustc calls in {}",
            v.label(),
            human_bytes(a.target),
            human_bytes(b.target),
            b.edits,
            human_bytes(a.anatomy.total()),
            human_bytes(b.anatomy.total()),
            b.cargo_compile_notices,
            b.rustc_compilations.map_or("unavailable".to_string(), |calls| calls.to_string()),
            duration_ms(b.build_ms)
        );
        println!("    {}", managed_allocation_text(&b.managed_allocation));
    }
    for (v, build) in &g.repeat_builds {
        let actual = build.rustc_invocations.as_ref().map_or("unavailable".to_string(), |calls| calls.len().to_string());
        println!("  {}", s.dim(&format!("{} unchanged rebuild: {} Cargo compile notices, {actual} actual rustc calls, {} cache hits, {} misses",
            v.label(), build.cargo_compile_notices, build.cache_hits, build.cache_misses)));
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
    let ctx = Ctx { cargo: cargo.clone(), fcargo: cargo, home: scratch.join("unused-home"), probe_id: probe.id, audit: CompilerAudit::create(scratch)?, compression: "auto".into(), store: StorePolicy::Sync };
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
        let (run, notices) = build_logged(ctx, false, &proj, &target, &[], &[])?;
        if notices == 0 {
            return Err("an edit build emitted no Cargo compile notices".into());
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
    let mut cmd = build_command(ctx, frust, project, (!frust).then_some(target), args, "off");
    for (key, value) in envs {
        cmd.env(key, value);
    }
    let run = timed(ctx, frust, &mut cmd)?;
    let notices = run.cargo_compile_notices;
    Ok((run, notices))
}

struct Captured {
    started: Instant,
    exited: Instant,
    success: bool,
    stderr: String,
    millis: u64,
}

// The clock stops when the direct child exits; a grandchild still holding stderr only delays the read by DRAIN, outside the measurement.
fn spawn_capture(cmd: &mut Command) -> Result<Captured, String> {
    let started = Instant::now();
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn()
        .map_err(|e| format!("failed to launch {}: {e}", cmd.get_program().to_string_lossy()))?;
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
    let exited = Instant::now();
    let millis = exited.duration_since(started).as_millis() as u64;
    let _ = done_rx.recv_timeout(DRAIN);
    let bytes = buf.lock().unwrap_or_else(|e| e.into_inner()).clone();
    Ok(Captured { started, exited, success: status.success(), stderr: String::from_utf8_lossy(&bytes).into_owned(), millis })
}

fn failure_tail(text: &str) -> String {
    let mut lines: Vec<&str> = text.lines().rev().take(12).collect();
    lines.reverse();
    lines.join("\n")
}

fn print_churn(c: &ChurnReport, s: &Styler) {
    println!();
    println!("{}", s.bold("Churn summary"));
    println!("  store policy                  {}", c.store_policy.label());
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
        "  rebuild after compact         {} Cargo compile notices, binary output unchanged: {}",
        c.frust_compile_notices_after_compact,
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
    println!("  {}", s.dim(&format!("(auto-compacted repeat emitted {} Cargo compile notices)", c.auto_compile_notices_on_repeat)));
    println!(
        "  release clean rebuild         cargo {}   frust {} from the compressed cache ({} Cargo compile notices, {} cache hits, {} misses)",
        duration_ms(c.release_clean_cargo_ms),
        duration_ms(c.release_clean_frust_ms),
        c.release_clean_frust_compile_notices,
        c.release_clean_frust_cache_hits,
        c.release_clean_frust_cache_misses
    );
    if let Some(counts) = c.release_clean_frust_rustc_breakdown {
        println!("  actual rustc invocations      {} total: {} Cargo units, {} build-script probes, {} unknown", counts.total, counts.cargo_units, counts.build_script_probes, counts.unknown);
    }
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
                    "Cargo compile notices: cargo {} in {}, frust {} in {}",
                    cargo.cargo_compile_notices,
                    duration_ms(cargo.build_ms),
                    auto.cargo_compile_notices,
                    duration_ms(auto.build_ms)
                ))
            );
        }
        for (v, why) in &g.stopped {
            println!("    {}", s.yellow(&format!("{} stopped {why}", v.label())));
        }
    }
    println!(
        "  at the end, cargo's dir compressed in place (nothing removed): {} -> {}  (Cargo then emitted {} compile notices)",
        human_bytes(c.cargo_after_edits),
        human_bytes(c.cargo_target_compressed),
        c.cargo_compile_notices_after_compress
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
    let mut outputs_equal = cargo_out == frust_out;
    let (mut c, mut f) = (Vec::with_capacity(runs), Vec::with_capacity(runs));
    for i in 0..runs {
        let order: [(&Path, &mut Vec<u64>); 2] =
            if i % 2 == 0 { [(cargo_bin, &mut c), (frust_bin, &mut f)] } else { [(frust_bin, &mut f), (cargo_bin, &mut c)] };
        for (bin, times) in order {
            let (us, output) = run(bin)?;
            outputs_equal &= output == cargo_out;
            times.push(us);
        }
    }
    Ok(Runtime { cargo_us: median(&mut c), frust_us: median(&mut f), outputs_equal })
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

#[derive(Debug, Clone, Default, Serialize)]
pub struct Run {
    millis: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    store_drain_millis: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    settled_millis: Option<u64>,
    sequence: u64,
    cargo_compile_notices: usize,
    // Queries stay separate so existing compilation totals keep their meaning.
    rustc_invocations: Option<Vec<Vec<String>>>,
    rustc_query_invocations: Option<Vec<Vec<String>>>,
    rustc_call_count: Option<usize>,
    rustc_invocation_classes: Option<Vec<CompilationClass>>,
    rustc_compilation_counts: Option<CompilationCounts>,
    cache_hits: u64,
    cache_misses: u64,
    cargo_stderr: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum CompilationClass {
    CargoUnit,
    BuildScriptProbe,
    Unknown,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct CompilationCounts {
    total: usize,
    cargo_units: usize,
    build_script_probes: usize,
    unknown: usize,
}

impl CompilationCounts {
    fn from_invocations(invocations: &[Vec<String>]) -> Self {
        let mut counts = Self { total: invocations.len(), ..Self::default() };
        for args in invocations {
            match classify_compilation(args) {
                CompilationClass::CargoUnit => counts.cargo_units += 1,
                CompilationClass::BuildScriptProbe => counts.build_script_probes += 1,
                CompilationClass::Unknown => counts.unknown += 1,
            }
        }
        counts
    }
}

fn rustc_flag<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter().enumerate().rev().find_map(|(index, arg)| {
        if arg == flag { args.get(index + 1).map(String::as_str) }
        else { arg.strip_prefix(flag).and_then(|value| value.strip_prefix('=')) }
    })
}

fn rustc_query(args: &[String]) -> bool {
    const INSPECTION_PRINTS: &[&str] = &[
        "calling-conventions", "cfg", "code-models", "crate-name", "deployment-target", "file-names", "host-tuple",
        "relocation-models", "split-debuginfo", "stack-protector-strategies", "sysroot", "target-cpus", "target-features",
        "target-libdir", "target-list", "tls-models",
    ];
    const VALUE_OPTIONS: &[&str] = &[
        "--cap-lints", "--cfg", "--check-cfg", "--codegen", "--color", "--crate-name", "--crate-type", "--diagnostic-width",
        "--edition", "--emit", "--error-format", "--extern", "--force-warn", "--json", "--out-dir", "--remap-path-prefix",
        "--remap-path-scope", "--sysroot", "--target", "-A", "-C", "-D", "-F", "-L", "-W", "-Z", "-l", "-o",
    ];
    let mut query = false;
    let mut print_paths = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" || arg.starts_with('@') { return false; }
        let print = if arg == "--print" {
            let Some(value) = iter.next() else { return false };
            Some(value.as_str())
        } else { arg.strip_prefix("--print=") };
        if let Some(print) = print {
            // rustc takes one kind per flag; commas and later '=' belong to an output path.
            let (kind, output) = print.split_once('=').map_or((print, None), |(kind, path)| (kind, Some(path)));
            if !INSPECTION_PRINTS.contains(&kind) { return false; }
            if let Some(path) = output {
                if path.is_empty() || path != "-" && print_paths.contains(&path) { return false; }
                if path != "-" { print_paths.push(path); }
            }
            query = true;
        } else if matches!(arg.as_str(), "-V" | "-vV" | "--version" | "-h" | "--help") {
            query = true;
        } else if VALUE_OPTIONS.contains(&arg.as_str()) {
            if iter.next().is_none() { return false; }
        } else if arg.starts_with("--") {
            if !arg.split_once('=').is_some_and(|(name, _)| VALUE_OPTIONS.contains(&name)) && arg != "--test" && arg != "--verbose" {
                return false;
            }
        } else if arg.starts_with('-') && arg != "-" && !["-A", "-C", "-D", "-F", "-L", "-W", "-Z", "-l", "-o"]
            .iter().any(|&prefix| arg.starts_with(prefix) && arg.len() > prefix.len()) {
            return false;
        }
    }
    query
}

fn classify_compilation(args: &[String]) -> CompilationClass {
    let Some(out) = rustc_flag(args, "--out-dir").map(Path::new) else { return CompilationClass::Unknown };
    let artifact_notifications = rustc_flag(args, "--json").is_some_and(|values| values.split(',').any(|value| value == "artifacts"));
    let unit_directory = out.file_name().is_some_and(|name| name == "deps")
        || out.parent().and_then(Path::file_name).is_some_and(|name| name == "build");
    if artifact_notifications && unit_directory {
        return CompilationClass::CargoUnit;
    }
    let emit_occurrences = args.iter().filter(|arg| arg.as_str() == "--emit" || arg.starts_with("--emit=")).count();
    if emit_occurrences != 1 {
        return CompilationClass::Unknown;
    }
    let emits: Vec<_> = rustc_flag(args, "--emit").map(|value| value.split(',').collect()).unwrap_or_default();
    if artifact_notifications || rustc_flag(args, "--crate-type") != Some("lib") || !emits.contains(&"metadata")
        || emits.iter().any(|emit| !matches!(*emit, "metadata" | "dep-info")) {
        return CompilationClass::Unknown;
    }
    if !out.is_absolute() || out.components().any(|part| matches!(part, std::path::Component::ParentDir)) {
        return CompilationClass::Unknown;
    }
    let parts = out.iter().map(|part| part.to_str()).collect::<Option<Vec<_>>>();
    let Some(parts) = parts else { return CompilationClass::Unknown };
    let probe_directory = parts.windows(5).any(|parts| {
        let profile = matches!(parts[0], "debug" | "release" | "coldone" | "coldtwo");
        let unit_hash = parts[2].rsplit_once('-').is_some_and(|(name, hash)| {
            !name.is_empty() && hash.len() >= 8 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        });
        profile && parts[1] == "build" && unit_hash && parts[3] == "out" && !parts[4].is_empty()
    });
    if probe_directory { CompilationClass::BuildScriptProbe } else { CompilationClass::Unknown }
}

fn controlled_environment(cmd: &mut Command) {
    for name in ["RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER",
        "CARGO_INCREMENTAL", "RUSTC_BOOTSTRAP", "RUSTC", "CARGO_TARGET_DIR", "CARGO_BUILD_TARGET"] {
        cmd.env_remove(name);
    }
    for (name, _) in std::env::vars_os() {
        let key = name.to_string_lossy();
        if key.starts_with("FRUST_") || key.starts_with("CARGO_PROFILE_")
            || key.starts_with("CARGO_BUILD_") || key.starts_with("CARGO_TARGET_")
            || matches!(key.as_ref(), "RUSTFLAGS" | "CARGO_ENCODED_RUSTFLAGS" | "RUSTC_WRAPPER"
                | "RUSTC_WORKSPACE_WRAPPER" | "CARGO_INCREMENTAL" | "RUSTC_BOOTSTRAP" | "RUSTC")
        {
            cmd.env_remove(name);
        }
    }
    cmd.env("CARGO_NET_OFFLINE", "true").env("CARGO_TERM_COLOR", "never").env("RUSTFLAGS", "")
        .env("FRUST_TUNE", "off").env("FRUST_LINK", "off").env("FRUST_CACHE", "on")
        .env("FRUST_LOCK", "on").env("FRUST_STORE_SYNC", "1").env("FRUST_GC_SYNC", "1")
        .env("FRUST_AUTO_COMPACT", "off").env("FRUST_UPDATE_CHECK", "off");
}

fn build_command(ctx: &Ctx, frust: bool, project: &Path, target: Option<&Path>, extra: &[&str], render: &str) -> Command {
    let mut cmd = Command::new(if frust { &ctx.fcargo } else { &ctx.cargo });
    controlled_environment(&mut cmd);
    cmd.arg("build").args(extra).current_dir(project)
        .env("FRUST_HOME", &ctx.home).env("FRUST_RENDER", render).env("FRUST_CACHE_COMPRESSION", &ctx.compression);
    if frust { cmd.env("FRUST_CARGO", &ctx.cargo); }
    if let Some(target) = target {
        cmd.env("CARGO_TARGET_DIR", target);
    }
    if let Some(audit) = &ctx.audit {
        cmd.env("RUSTC", &audit.compiler).env("FRUST_RUSTC", &audit.compiler);
    } else if let Some(rustc) = std::env::var_os("RUSTC").map(PathBuf::from).or_else(|| resolve_tool("rustc")) {
        cmd.env("RUSTC", &rustc).env("FRUST_RUSTC", rustc);
    }
    cmd
}

struct DeferredSlots(Vec<(PathBuf, fs::File)>);

impl Drop for DeferredSlots {
    fn drop(&mut self) {
        for (_, file) in &self.0 { let _ = sys::unlock(file); }
    }
}

fn deferred_entries(directory: &Path) -> Result<Option<Vec<PathBuf>>, String> {
    match fs::read_dir(directory) {
        Ok(entries) => entries.map(|entry| entry.map(|entry| entry.path()))
            .collect::<Result<Vec<_>, _>>().map(Some).map_err(|error| format!("{}: {error}", directory.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("{}: {error}", directory.display())),
    }
}

fn deferred_slots(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut slots: Vec<_> = paths.iter().filter(|path| path.file_name()
        .is_some_and(|name| name.as_encoded_bytes().starts_with(b"slot-")))
        .cloned().collect();
    slots.sort(); slots
}

fn has_deferred_jobs(paths: &[PathBuf]) -> bool {
    paths.iter().any(|path| path.file_name()
        .is_some_and(|name| name.as_encoded_bytes().starts_with(b"job-") && name.as_encoded_bytes().ends_with(b".json")))
}

fn deferred_stores_idle(home: &Path) -> Result<bool, String> {
    let directory = Dirs::at(home).deferred();
    let Some(paths) = deferred_entries(&directory)? else { return Ok(true) };
    if has_deferred_jobs(&paths) { return Ok(false); }
    let slots = deferred_slots(&paths);
    let mut held = DeferredSlots(Vec::new());
    for path in &slots {
        let named = sys::lstat(path).map_err(|error| format!("{}: {error}", path.display()))?;
        if !named.is_file() || named.file_type().is_symlink() || named.nlink() != 1 || named.ino() == 0 {
            return Err(format!("unsafe deferred slot {}", path.display()));
        }
        let file = fs::OpenOptions::new().read(true).write(true).open(path).map_err(|error| error.to_string())?;
        let opened = sys::fstat(&file).map_err(|error| error.to_string())?;
        if (opened.dev(), opened.ino()) != (named.dev(), named.ino()) || !opened.is_file() || opened.nlink() != 1 { return Ok(false); }
        match sys::try_lock(&file, sys::LockKind::Exclusive) {
            Ok(()) => held.0.push((path.clone(), file)),
            Err(fs::TryLockError::WouldBlock) => return Ok(false),
            Err(fs::TryLockError::Error(error)) => return Err(format!("{}: {error}", path.display())),
        }
    }
    // All existing slots stay locked together through the final job and pathname recheck.
    let Some(after) = deferred_entries(&directory)? else { return Ok(false) };
    if has_deferred_jobs(&after) || deferred_slots(&after) != slots { return Ok(false); }
    for (path, file) in &held.0 {
        let current = sys::lstat(path).map_err(|error| error.to_string())?;
        let opened = sys::fstat(file).map_err(|error| error.to_string())?;
        if !current.is_file() || current.file_type().is_symlink() || current.nlink() != 1
            || (current.dev(), current.ino()) != (opened.dev(), opened.ino()) { return Ok(false); }
    }
    Ok(true)
}

/// Waits only on this benchmark's isolated home; completion proves no pending job and exclusive ownership of every existing slot.
pub fn drain_deferred_stores(home: &Path, timeout: Duration) -> Result<(), String> {
    let started = Instant::now();
    loop {
        if deferred_stores_idle(home)? { return Ok(()); }
        let Some(remaining) = timeout.checked_sub(started.elapsed()) else {
            return Err(format!("deferred stores did not drain within {}ms in {}", timeout.as_millis(), home.display()));
        };
        if remaining.is_zero() { return Err(format!("deferred stores did not drain in {}", home.display())); }
        thread::sleep(remaining.min(Duration::from_millis(10)));
    }
}

fn timed(ctx: &Ctx, frust: bool, cmd: &mut Command) -> Result<Run, String> {
    timed_with_store(ctx, frust, cmd, ctx.store)
}

fn timed_with_store(ctx: &Ctx, frust: bool, cmd: &mut Command, store: StorePolicy) -> Result<Run, String> {
    cmd.env("FRUST_STORE_SYNC", if store == StorePolicy::Sync { "1" } else { "0" });
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed) + 1;
    let audit_dir = ctx.audit.as_ref().map(|audit| audit.logs.join(sequence.to_string()));
    if let Some(dir) = &audit_dir {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        cmd.env("FRUST_BENCH_AUDIT", dir);
    }
    fs::create_dir_all(&ctx.home).map_err(|e| e.to_string())?;
    let dirs = Dirs::at(&ctx.home);
    fs::write(dirs.auto_gc_marker(), b"").map_err(|e| e.to_string())?;
    let before = crate::stats::totals(&crate::stats::read_ledger(&dirs));
    let captured = spawn_capture(cmd)?;
    let (store_drain_millis, settled_millis) = if frust && store == StorePolicy::Deferred {
        drain_deferred_stores(&ctx.home, Duration::from_secs(120)).map_err(|error| format!("deferred-store drain failed: {error}"))?;
        let settled = Instant::now();
        (Some(settled.duration_since(captured.exited).as_millis() as u64), Some(settled.duration_since(captured.started).as_millis() as u64))
    } else { (None, None) };
    if !captured.success {
        return Err(format!("build failed:\n{}", failure_tail(&captured.stderr)));
    }
    let after = crate::stats::totals(&crate::stats::read_ledger(&dirs));
    let rustc_calls = audit_dir.map(|dir| {
        let mut paths = fs::read_dir(dir).map_err(|e| e.to_string())?
            .map(|e| e.map(|e| e.path()).map_err(|e| e.to_string())).collect::<Result<Vec<_>, _>>()?;
        paths.sort();
        paths.into_iter().map(|p| fs::read(p).map_err(|e| e.to_string()).map(|bytes| {
            bytes.split(|b| *b == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).into_owned()).collect()
        })).collect::<Result<Vec<Vec<String>>, String>>()
    }).transpose()?;
    let rustc_call_count = rustc_calls.as_ref().map(Vec::len);
    let (rustc_queries, rustc_compiles): (Vec<_>, Vec<_>) = rustc_calls.into_iter().flatten().partition(|args| rustc_query(args));
    let rustc_invocations = rustc_call_count.map(|_| rustc_compiles);
    let rustc_query_invocations = rustc_call_count.map(|_| rustc_queries);
    let rustc_invocation_classes = rustc_invocations.as_ref().map(|invocations| invocations.iter().map(|args| classify_compilation(args)).collect());
    let rustc_compilation_counts = rustc_invocations.as_ref().map(|invocations| CompilationCounts::from_invocations(invocations));
    Ok(Run {
        millis: captured.millis,
        store_drain_millis,
        settled_millis,
        sequence,
        cargo_compile_notices: captured.stderr.lines().filter(|l| l.trim_start().starts_with("Compiling ")).count(),
        rustc_invocations,
        rustc_query_invocations,
        rustc_call_count,
        rustc_invocation_classes,
        rustc_compilation_counts,
        cache_hits: if frust { after.hits.saturating_sub(before.hits) } else { 0 },
        cache_misses: if frust { after.misses.saturating_sub(before.misses) } else { 0 },
        cargo_stderr: captured.stderr,
    })
}

fn plain_build(ctx: &Ctx, project: &Path, target: &Path) -> Result<Run, String> {
    plain_build_with(ctx, project, target, &[])
}

fn plain_build_with(ctx: &Ctx, project: &Path, target: &Path, extra: &[&str]) -> Result<Run, String> {
    timed(ctx, false, &mut build_command(ctx, false, project, Some(target), extra, "off"))
}

fn frust_build(ctx: &Ctx, project: &Path) -> Result<Run, String> {
    frust_build_with(ctx, project, &[])
}

fn frust_build_with(ctx: &Ctx, project: &Path, extra: &[&str]) -> Result<Run, String> {
    frust_build_env(ctx, project, extra, "off")
}

fn frust_build_env(ctx: &Ctx, project: &Path, extra: &[&str], render: &str) -> Result<Run, String> {
    timed(ctx, true, &mut build_command(ctx, true, project, None, extra, render))
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
    let text = fs::read_to_string(&main).map_err(|e| e.to_string())?;
    let marker = "    println!(\"frust-bench-edit ";
    if !text.lines().any(|line| line.starts_with(marker)) {
        return Err("benchmark fixture has no observable edit marker".into());
    }
    let edited: String = text.lines().map(|line| {
        if line.starts_with(marker) {
            format!("    println!(\"frust-bench-edit {{}}\", std::hint::black_box({n}u32));\n")
        } else {
            format!("{line}\n")
        }
    }).collect();
    fs::write(&main, edited).map_err(|e| e.to_string())
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
    main.push_str("fn main() {\n    println!(\"frust-bench-edit {}\", std::hint::black_box(0u32));\n    let mut total = 0u64;\n");
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
    println!("  store policy                  {} (build times measure foreground completion)", report.store_policy.label());
    println!("  {}", s.dim(report.store_completion_method));
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
    print_profile_matrix(&report.profile_matrix);
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

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn isolated_compiler_audit_test(test_name: &str) -> bool {
        const MARKER: &str = "FRUST_TEST_COMPILER_AUDIT";
        if std::env::var(MARKER).is_ok_and(|value| value == test_name) {
            return true;
        }
        struct OwnedChild(Option<std::process::Child>);
        impl Drop for OwnedChild {
            fn drop(&mut self) {
                if let Some(child) = &mut self.0 {
                    if let Err(error) = sys::cancel_owned_process_group(child.id()) {
                        eprintln!("isolated compiler audit cleanup failed: {error}");
                    }
                    let _ = child.wait();
                }
            }
        }
        // Create executable scripts after exec so unrelated parallel tests cannot inherit their writers.
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", test_name, "--nocapture", "--test-threads=1"])
            .env(MARKER, test_name)
            .stdin(Stdio::null()).stdout(Stdio::inherit()).stderr(Stdio::inherit());
        sys::own_process_group(&mut command);
        let mut child = OwnedChild(Some(command.spawn()
            .unwrap_or_else(|error| panic!("cannot launch isolated {test_name}: {error}"))));
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let pid = child.0.as_ref().unwrap().id();
            if sys::observe_child_unreaped(pid, false)
                .unwrap_or_else(|error| panic!("cannot observe isolated {test_name}: {}", error.message)).is_some()
            {
                sys::cancel_owned_process_group(pid)
                    .unwrap_or_else(|error| panic!("cannot clean up isolated {test_name}: {error}"));
                let status = child.0.as_mut().unwrap().wait().unwrap();
                child.0 = None;
                assert!(status.success(), "isolated {test_name} failed: {status}");
                return false;
            }
            assert!(Instant::now() < deadline, "isolated {test_name} exceeded 30 seconds");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn store_policy_defaults_to_sync_and_accepts_general_and_profile_modes() {
        let flags = |values: &[&str]| values.iter().map(|value| value.to_string()).collect::<Vec<_>>();
        assert_eq!(store_from_flags(&[]).unwrap(), StorePolicy::Sync);
        assert_eq!(store_from_flags(&flags(&["--store=sync"])).unwrap(), StorePolicy::Sync);
        assert_eq!(store_from_flags(&flags(&["--store", "deferred"])).unwrap(), StorePolicy::Deferred);
        assert_eq!(store_from_flags(&flags(&["--profiles", "--store=sync"])).unwrap(), StorePolicy::Sync);
        assert_eq!(store_from_flags(&flags(&["--profiles", "--store", "deferred"])).unwrap(), StorePolicy::Deferred);
    }


    #[test]
    fn retained_deferred_scratch_survives_abandoned_sweep_but_success_still_cleans() {
        let root = std::env::temp_dir().join(format!("frust-bench-retention-test-{}", std::process::id()));
        let dead = (1_000_000_000..1_000_000_100).find(|pid| !sys::pid_alive(*pid)).expect("a verified dead positive PID");
        let ordinary = root.join(format!("{}-{dead}", StorePolicy::Sync.scratch_prefix(false)));
        let retained = root.join(format!("{}-{dead}", StorePolicy::Deferred.scratch_prefix(false)));
        assert_eq!(StorePolicy::Sync.scratch_prefix(true), StorePolicy::Deferred.scratch_prefix(false));
        for path in [&ordinary, &retained] {
            fs::create_dir_all(path).unwrap();
            fs::write(path.join("payload"), b"owned benchmark fixture").unwrap();
        }
        remove_abandoned_scratch(&root);
        assert!(!ordinary.exists());
        assert_eq!(fs::read(retained.join("payload")).unwrap(), b"owned benchmark fixture");
        let success = root.join(format!("{}-{}", StorePolicy::Deferred.scratch_prefix(false), std::process::id()));
        fs::create_dir(&success).unwrap();
        drop(Scratch(success.clone(), false));
        assert!(!success.exists(), "deferred success without --keep-scratch still cleans normally");
        fs::create_dir(&success).unwrap();
        drop(Scratch(success.clone(), true));
        remove_abandoned_scratch(&root);
        assert!(success.is_dir(), "a failed deferred drain preserves its original paths");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn growth_variant_cleanup_retains_original_data_only_when_requested() {
        let root = std::env::temp_dir().join(format!("frust-bench-growth-retention-test-{}", std::process::id()));
        let home = root.join("home");
        let target = root.join("target");
        for directory in [&home, &target] {
            fs::create_dir_all(directory).unwrap();
            fs::write(directory.join("payload"), b"original dataset").unwrap();
        }
        let before = sys::lstat(&home.join("payload")).unwrap();
        cleanup_growth_variant(&home, &target, true).unwrap();
        let retained = sys::lstat(&home.join("payload")).unwrap();
        assert_eq!((before.dev(), before.ino()), (retained.dev(), retained.ino()));
        assert_eq!(fs::read(target.join("payload")).unwrap(), b"original dataset");
        cleanup_growth_variant(&home, &target, false).unwrap();
        assert!(!home.exists() && !target.exists());
        fs::remove_dir(root).unwrap();
    }

    #[test]
    fn managed_allocation_separates_other_and_retired_from_nested_target_and_shared_payload() {
        let home = std::env::temp_dir().join(format!("frust-bench-managed-allocation-test-{}", std::process::id()));
        let target = home.join("targets/current");
        let store = home.join("store");
        let control = home.join("manifests");
        let temporary = home.join("tmp");
        let retired = temporary.join("evicted");
        for path in [&target, &store, &control, &temporary, &retired] { fs::create_dir_all(path).unwrap(); }
        let empty = managed_allocation(&target, Some(&home));
        assert_eq!(empty.retired_payload_bytes, Some(0), "an empty persistent queue has no stranded payload");
        let payload = target.join("artifact");
        let pending = temporary.join("staged");
        fs::write(&payload, vec![7; 4096]).unwrap();
        fs::write(&pending, vec![9; 4096]).unwrap();
        fs::hard_link(&payload, store.join("payload")).unwrap();
        fs::hard_link(&payload, control.join("shared")).unwrap();
        fs::hard_link(&payload, retired.join("shared-retired")).unwrap();
        let retired_file = retired.join("old-payload");
        fs::write(&retired_file, vec![11; 4096]).unwrap();
        let bytes = |path: &Path| sys::lstat(path).unwrap().allocated();
        let artifacts = bytes(&target) + bytes(&store) + bytes(&payload);
        let metadata = bytes(&home) + bytes(&home.join("targets")) + bytes(&control) + bytes(&temporary) + bytes(&pending)
            + bytes(&retired) + bytes(&retired_file);
        let measured = managed_allocation(&target, Some(&home));
        assert_eq!(measured.status, "available", "{:?}", measured.error);
        assert_eq!(measured.target_plus_store_bytes, Some(artifacts));
        assert_eq!(measured.other_managed_bytes, Some(metadata));
        assert_eq!(measured.retired_payload_bytes, Some(bytes(&payload) + bytes(&retired_file)));
        assert_eq!(measured.cache_home_bytes, Some(artifacts + metadata));
        assert_eq!(measured.total_managed_bytes, Some(artifacts + metadata));
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn missing_managed_allocation_serializes_as_unavailable_not_zero() {
        let root = std::env::temp_dir().join(format!("frust-bench-missing-allocation-{}", std::process::id()));
        assert!(!root.exists());
        let measured = serde_json::to_value(managed_allocation(&root, None)).unwrap();
        assert_eq!(measured["status"], "unavailable");
        assert!(measured["total_managed_bytes"].is_null());
        assert!(measured["cache_home_bytes"].is_null());
        assert!(measured["error"].as_str().is_some_and(|error| error.contains("frust-bench-missing-allocation")));
    }

    #[test]
    fn growth_ledger_rejects_same_prefix_replacement_and_accepts_real_append() {
        let root = fsx::temp_sibling(&std::env::temp_dir().join("frust-growth-ledger-proof"));
        fs::create_dir(&root).unwrap();
        let dirs = Dirs::at(&root);
        let absent = growth_ledger(&dirs).unwrap();
        fs::write(dirs.ledger(), b"first record\n").unwrap();
        let first = growth_ledger(&dirs).unwrap();
        assert_eq!(growth_ledger_added(&absent, &first).unwrap(), "first record\n");
        {
            use std::io::Write as _;
            let mut file = fs::OpenOptions::new().append(true).open(dirs.ledger()).unwrap();
            file.write_all(b"second record\n").unwrap();
        }
        let appended = growth_ledger(&dirs).unwrap();
        assert_eq!(growth_ledger_added(&first, &appended).unwrap(), "second record\n");
        let replacement = root.join("replacement");
        fs::write(&replacement, format!("{}third record\n", appended.text)).unwrap();
        sys::rename(&replacement, &dirs.ledger()).unwrap();
        let replaced = growth_ledger(&dirs).unwrap();
        assert_ne!(appended.identity, replaced.identity);
        assert!(replaced.text.starts_with(&appended.text));
        assert!(growth_ledger_added(&appended, &replaced).unwrap_err().contains("identity changed"));
        fs::write(dirs.ledger(), b"first record\n").unwrap();
        let truncated = growth_ledger(&dirs).unwrap();
        assert!(growth_ledger_added(&replaced, &truncated).is_err());
        drop((first, appended, replaced, truncated));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn source_settle_checks_match_fine_and_coarse_stamp_safety() {
        let fine = crate::cache::hashing::Stamp { dev: 1, ino: 1, size: 1, mtime_ns: 10_500_000_001, ctime_ns: 10_500_000_001 };
        let coarse = crate::cache::hashing::Stamp { mtime_ns: 10_000_000_000, ctime_ns: 10_000_000_000, ..fine };
        for stamp in [fine, coarse] {
            for now in [10_550_000_000, 10_700_000_000, 12_000_000_000, 12_100_000_000] {
                assert_eq!(source_settle_slack(&stamp, now) > 0, !stamp.too_new_for(now));
            }
        }
    }

    #[test]
    fn source_settle_timeout_preserves_fresh_inputs_and_success_records_real_stamps() {
        let root = fsx::temp_sibling(&std::env::temp_dir().join("frust-source-settlement"));
        fs::create_dir(&root).unwrap();
        let source = root.join("lib.rs");
        fs::write(&source, b"pub fn value() -> u32 { 7 }\n").unwrap();
        let original = crate::cache::hashing::Stamp::read(&source).unwrap();
        assert!(wait_sources_settled(std::slice::from_ref(&source), Duration::ZERO).is_err());
        let (proof, stamps) = wait_sources_settled(std::slice::from_ref(&source), SOURCE_SETTLE_TIMEOUT).unwrap();
        assert_eq!(proof.sources, 1);
        assert!(proof.minimum_settle_slack_ns > 0);
        assert_eq!(stamps[0].1, original);
        assert_eq!(crate::cache::hashing::Stamp::read(&source).unwrap(), original);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn observable_edits_replace_the_body_input_without_growing_source() {
        let root = std::env::temp_dir().join(format!("frust-bench-edit-test-{}", std::process::id()));
        write_project(&root, 0, false).unwrap();
        touch_app(&root, 17).unwrap();
        let first = fs::read_to_string(root.join("src/main.rs")).unwrap();
        touch_app(&root, 18).unwrap();
        let second = fs::read_to_string(root.join("src/main.rs")).unwrap();
        assert!(first.contains("std::hint::black_box(17u32)"));
        assert!(second.contains("std::hint::black_box(18u32)"));
        assert!(!second.contains("17u32"));
        assert_eq!(first.len(), second.len());
        assert!(second.contains("let mut total = 0u64"));
        assert!(expected_edit_output(b"frust-bench-edit 18\n1234\n", 18));
        assert!(!expected_edit_output(b"frust-bench-edit 17\n1234\n", 18));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn compiler_evidence_distinguishes_scheduling_compilation_and_restore() {
        let actual = vec![["--crate-name", "app", "--emit=metadata,link", "--json=artifacts", "--out-dir", "/target/release/deps"].into_iter().map(str::to_string).collect()];
        let compiled = Run { rustc_invocations: Some(actual), cargo_compile_notices: 1, ..Run::default() };
        let restored = Run { rustc_invocations: Some(Vec::new()), cargo_compile_notices: 1, cache_hits: 1, ..Run::default() };
        let noop = Run { rustc_invocations: Some(Vec::new()), rustc_query_invocations: Some(Vec::new()), rustc_call_count: Some(0), ..Run::default() };
        assert!(validate_build_evidence(&compiled, "cold", false).is_ok());
        assert!(validate_build_evidence(&compiled, "edit", true).is_ok());
        assert!(validate_build_evidence(&restored, "clean rebuild", true).is_ok());
        assert!(validate_build_evidence(&restored, "edit", true).is_err());
        assert!(validate_build_evidence(&compiled, "clean rebuild", true).is_err());
        assert!(validate_build_evidence(&restored, "no-op", true).is_err());
        assert!(validate_build_evidence(&noop, "no-op", true).is_ok());
        assert!(validate_build_evidence(&noop, "clean rebuild", true).is_err());
    }

    #[test]
    fn no_op_rejects_cache_activity_even_without_compile_notices_or_actual_compiler_calls() {
        for invocations in [None, Some(Vec::new())] {
            let audited = invocations.is_some();
            let noop = Run {
                rustc_invocations: invocations, rustc_query_invocations: audited.then(Vec::new),
                rustc_call_count: audited.then_some(0), ..Run::default()
            };
            assert!(validate_build_evidence(&noop, "no-op", true).is_ok());
            for (cache_hits, cache_misses) in [(1, 0), (0, 1), (2, 3)] {
                let cached_work = Run { cache_hits, cache_misses, ..noop.clone() };
                assert!(validate_build_evidence(&cached_work, "no-op", true).is_err());
            }
        }
    }

    #[test]
    fn no_op_rejects_queries_and_missing_call_evidence_while_restores_keep_queries_visible() {
        let mut run = Run {
            rustc_invocations: Some(Vec::new()), rustc_query_invocations: Some(Vec::new()),
            rustc_call_count: Some(0), ..Run::default()
        };
        assert!(validate_build_evidence(&run, "no-op", true).is_ok());
        let queries = [vec!["-V"], vec!["-vV"], vec!["--version"], vec!["--help"], vec!["-h"],
            vec!["--emit=metadata", "--print=file-names"], vec!["--emit", "link", "--print", "cfg"]];
        for args in queries {
            let args = args.into_iter().map(str::to_string).collect::<Vec<_>>();
            assert!(rustc_query(&args));
            run.rustc_query_invocations = Some(vec![args]);
            run.rustc_call_count = Some(1);
            assert!(validate_build_evidence(&run, "no-op", true).is_err());
            run.cache_hits = 1;
            assert!(validate_build_evidence(&run, "clean rebuild", true).is_ok());
            run.cache_hits = 0;
        }
        run.rustc_query_invocations = Some(Vec::new());
        run.rustc_call_count = None;
        assert!(validate_build_evidence(&run, "no-op", true).is_err());
        run.rustc_call_count = Some(0);
        run.rustc_query_invocations = None;
        assert!(validate_build_evidence(&run, "no-op", true).is_err());
        run.rustc_query_invocations = Some(Vec::new());
        run.rustc_invocations = None;
        assert!(validate_build_evidence(&run, "no-op", true).is_err());
        let unknown = vec!["--crate-name=unexpected".to_string()];
        assert!(!rustc_query(&unknown));
        assert_eq!(classify_compilation(&unknown), CompilationClass::Unknown);
    }

    #[test]
    fn inspection_queries_require_every_print_kind_and_respect_option_operands() {
        let flags = |values: &[&str]| values.iter().map(|value| value.to_string()).collect::<Vec<_>>();
        for args in [
            vec!["--emit=obj", "--print=file-names"],
            vec!["--print", "cfg", "--print=file-names=out,with=equals"],
            vec!["--print=file-names=out,link-args=filename"],
            vec!["--crate-name", "___", "--crate-type=lib", "-Copt-level=1", "--print=file-names", "-"],
            vec!["--version", "--print=cfg"],
        ] {
            assert!(rustc_query(&flags(&args)), "{args:?}");
        }
        // Unknown and compilation-bearing requests win over every recognized inspection.
        for args in [
            vec!["--print=link-args"], vec!["--print", "native-static-libs"],
            vec!["--print=cfg", "--print=link-args"], vec!["--print=link-args", "--print=file-names"],
            vec!["--version", "--print=native-static-libs=output"],
            vec!["--help", "--print=future-unknown-kind"],
            vec!["--print=cfg", "--print=file-names,cfg"], vec!["--print=cfg,file-names"],
            vec!["--print=cfg", "--print"], vec!["--print", "--print=cfg"],
            vec!["--print="], vec!["--print=file-names="],
            vec!["--print=cfg=output", "--print=file-names=output"],
            vec!["--printer=cfg"], vec!["--printcfg"],
            vec!["--remap-path-prefix", "--print=cfg"], vec!["-o", "--print=sysroot"],
            vec!["--", "--print=cfg"], vec!["--unknown-option", "--print=cfg"], vec!["-vV", "@response"],
        ] {
            assert!(!rustc_query(&flags(&args)), "{args:?}");
        }
    }

    #[test]
    fn compile_notice_schema_does_not_claim_restores_are_actual_compilations() {
        let restored = Run {
            cargo_compile_notices: 3, cache_hits: 3,
            rustc_invocations: Some(Vec::new()), rustc_compilation_counts: Some(CompilationCounts::default()),
            ..Run::default()
        };
        assert!(validate_build_evidence(&restored, "clean rebuild", true).is_ok());
        assert!(validate_build_evidence(&restored, "no-op", true).is_err());
        let value = serde_json::to_value(restored).unwrap();
        assert_eq!(value["cargo_compile_notices"], 3);
        assert_eq!(value["rustc_compilation_counts"]["total"], 0);
        assert_eq!(value["rustc_invocations"].as_array().unwrap().len(), 0);
        assert!(value.get("cargo_scheduled_units").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn capability_probes_remain_visible_without_counting_as_restored_cargo_units() {
        let args = ["--cfg=procmacro2_build_probe", "--edition=2021", "--crate-name=proc_macro2", "--crate-type=lib",
            "--cap-lints=allow", "--emit=dep-info,metadata", "--out-dir",
            "/target/release/build/proc-macro2-ec16ffd2fcd7792e/out/probe", "src/probe/proc_macro_span.rs", "--target", "x86_64-unknown-linux-gnu"];
        let probe: Vec<_> = args.into_iter().map(str::to_string).collect();
        assert_eq!(classify_compilation(&probe), CompilationClass::BuildScriptProbe);
        let mut invocations = Vec::new();
        for source in ["proc_macro_span.rs", "proc_macro_span_location.rs", "proc_macro_span_file.rs"] {
            let mut invocation = probe.clone();
            invocation[8] = format!("src/probe/{source}");
            invocations.push(invocation);
        }
        let counts = CompilationCounts::from_invocations(&invocations);
        assert_eq!((counts.total, counts.cargo_units, counts.build_script_probes, counts.unknown), (3, 0, 3, 0));
        let mut run = Run { rustc_invocations: Some(invocations), cache_hits: 18, ..Run::default() };
        assert!(validate_build_evidence(&run, "clean rebuild", true).is_ok());
        assert!(validate_build_evidence(&run, "no-op", true).is_err());
        assert!(validate_build_evidence(&run, "edit", true).is_err());
        run.rustc_invocations.as_mut().unwrap().push(["--crate-name=app", "--crate-type=bin", "--emit=dep-info,link", "--out-dir=/target/release/deps", "--json=artifacts"].into_iter().map(str::to_string).collect());
        assert!(validate_build_evidence(&run, "clean rebuild", true).is_err());
        run.rustc_invocations.as_mut().unwrap().pop();
        run.rustc_invocations.as_mut().unwrap().push(["--crate-name=app", "--emit=metadata", "--out-dir=/target/release/deps"].into_iter().map(str::to_string).collect());
        assert!(validate_build_evidence(&run, "clean rebuild", true).is_err());
        let context = evidence_failure(&run, "release repetition 2 frust", "unexpected compiler execution");
        assert!(context.contains("release repetition 2 frust"));
        assert!(context.contains("--crate-name=app"));
        assert!(context.contains("cache_hits"));
    }

    #[cfg(unix)]
    #[test]
    fn repeated_emits_cannot_hide_linking_or_custom_outputs_as_capability_probes() {
        for last_emit in [vec!["--emit=dep-info,metadata"], vec!["--emit", "dep-info,metadata"]] {
            for earlier_emit in [
                vec!["--emit=link"], vec!["--emit", "link"],
                vec!["--emit=metadata=custom.rmeta"], vec!["--emit", "dep-info=custom.d"],
                vec!["--emit=metadata"],
            ] {
                let mut args: Vec<_> = ["--crate-name=proc_macro2", "--crate-type=lib",
                    "--out-dir=/target/release/build/proc-macro2-ec16ffd2fcd7792e/out/probe"]
                    .into_iter().map(str::to_string).collect();
                args.extend(earlier_emit.iter().chain(&last_emit).map(|arg| (*arg).to_string()));
                assert_eq!(classify_compilation(&args), CompilationClass::Unknown, "{args:?}");
                let run = Run { rustc_invocations: Some(vec![args.clone()]), cache_hits: 1, ..Run::default() };
                assert!(validate_build_evidence(&run, "clean rebuild", true).is_err());
                args[2] = "--out-dir=/target/release/deps".into();
                args.push("--json=artifacts".into());
                assert_eq!(classify_compilation(&args), CompilationClass::CargoUnit, "declared Cargo-unit recognition is independent of repeated emits");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn compiler_probe_classification_requires_every_probe_boundary() {
        let probe: Vec<_> = ["--crate-name=proc_macro2", "--crate-type=lib", "--emit=dep-info,metadata",
            "--out-dir=/target/release/build/proc-macro2-ec16ffd2fcd7792e/out/probe"].into_iter().map(str::to_string).collect();
        for out in ["/target/release/deps", "/target/release/build/proc-macro2-ec16ffd2fcd7792e",
            "/target/release/build/proc-macro2-ec16ffd2fcd7792e/out", "/target/release/build/not-a-unit/out/probe",
            "/target/release/build/proc-macro2-ec16ffd2fcd7792e/out/probe/../../deps"] {
            let mut args = probe.clone();
            args[3] = format!("--out-dir={out}");
            assert_eq!(classify_compilation(&args), CompilationClass::Unknown, "{out}");
        }
        for (index, arg) in [(1, "--crate-type=bin"), (2, "--emit=dep-info,metadata,link"), (2, "--emit=link")] {
            let mut args = probe.clone();
            args[index] = arg.to_string();
            assert_eq!(classify_compilation(&args), CompilationClass::Unknown);
        }
        let mut args = probe;
        args.push("--json=artifacts".into());
        assert_eq!(classify_compilation(&args), CompilationClass::Unknown);
        args[3] = "--out-dir=/target/release/build/proc-macro2-ec16ffd2fcd7792e".into();
        assert_eq!(classify_compilation(&args), CompilationClass::CargoUnit);
    }

    #[test]
    fn growth_validation_rejects_stops_stale_outputs_and_recompilation() {
        let mut report = GrowthReport {
            evidence_schema_version: EVIDENCE_SCHEMA_VERSION,
            cache_compression: "auto".into(), edit_policy: "immediate", source_settle_timeout_ms: None,
            source_settlements: Vec::new(), cache_evidence_method: "test", libs: 2, registry_deps: false, edits: 2, every: 1,
            compiler_audit_available: true,
            auto_compaction_policy: AutoCompactionPolicy::from(&benchmark_auto_options(false)),
            rows: Vec::new(), repeat_builds: vec![
                (GrowthVariant::Cargo, Run { rustc_invocations: Some(Vec::new()), rustc_query_invocations: Some(Vec::new()), rustc_call_count: Some(0), ..Run::default() }),
                (GrowthVariant::Auto, Run { rustc_invocations: Some(Vec::new()), rustc_query_invocations: Some(Vec::new()), rustc_call_count: Some(0), ..Run::default() }),
            ],
            outputs: vec![(GrowthVariant::Cargo, "same".into()), (GrowthVariant::Auto, "same".into())],
            edit_outputs: vec![(GrowthVariant::Cargo, vec!["a".into(), "b".into()]), (GrowthVariant::Auto, vec!["a".into(), "b".into()])],
            stopped: Vec::new(),
        };
        assert!(validate_growth(&report).is_ok());
        report.edit_outputs[1].1[0] = "stale".into();
        assert!(validate_growth(&report).is_err());
        report.edit_outputs[1].1[0] = "a".into();
        report.repeat_builds[1].1.cargo_compile_notices = 1;
        assert!(validate_growth(&report).is_err());
        report.repeat_builds[1].1.cargo_compile_notices = 0;
        report.repeat_builds[1].1.cache_hits = 1;
        assert!(validate_growth(&report).is_err());
        report.repeat_builds[1].1.cache_hits = 0;
        report.repeat_builds[1].1.cache_misses = 1;
        assert!(validate_growth(&report).is_err());
        report.repeat_builds[1].1.cache_misses = 0;
        report.repeat_builds[1].1.rustc_invocations = Some(vec![vec!["--emit=metadata".into()]]);
        assert!(validate_growth(&report).is_err(), "even a compiler call without Cargo notices invalidates a no-op");
        report.repeat_builds[1].1.rustc_invocations = None;
        assert!(validate_growth(&report).is_err(), "an available audit cannot be omitted from repeat evidence");
        report.repeat_builds[1].1.rustc_invocations = Some(Vec::new());
        assert!(validate_growth(&report).is_ok());
        report.repeat_builds[1].1.rustc_query_invocations = Some(vec![vec!["--version".into()]]);
        report.repeat_builds[1].1.rustc_call_count = Some(1);
        assert!(validate_growth(&report).is_err(), "compiler queries invalidate an unchanged growth rebuild");
        report.repeat_builds[1].1.rustc_query_invocations = None;
        report.repeat_builds[1].1.rustc_call_count = Some(0);
        assert!(validate_growth(&report).is_err(), "available query evidence cannot be omitted");
        report.repeat_builds[1].1.rustc_query_invocations = Some(Vec::new());
        assert!(validate_growth(&report).is_ok());
        report.stopped.push((GrowthVariant::Auto, "budget exhausted".into()));
        assert!(validate_growth(&report).is_err());
    }

    #[test]
    fn growth_retention_samples_after_maintenance_and_refreshes_final_noop_storage() {
        for blocked in [false, true] {
            let root = fsx::temp_sibling(&std::env::temp_dir().join("frust-growth-retention-snapshot"));
            let dirs = Dirs::at(root.join("home"));
            let store = crate::cache::Cache::new(&dirs);
            let project = root.join("app");
            let target = dirs.targets().join("workspace/toolchain");
            fs::create_dir_all(&target).unwrap();
            let before = growth_ledger(&dirs).unwrap();
            let unit = "ab".repeat(20);
            for n in 1..=4 {
                let key = format!("{n:040x}");
                let staged = store.stage().unwrap();
                // Legacy metadata deliberately leaves explicit unit references for the missing-entry diagnostic.
                fs::write(staged.join("meta.json"), serde_json::to_vec(&serde_json::json!({
                    "crate_name":"lib0", "version":"1", "compile_ms":1, "created_unix":1, "stderr":"stderr",
                    "files":[
                        {"name":"lib0-hash.0.tag.rcgu.o", "stored":"f0", "text":false, "bytes":32},
                        {"name":"liblib0.rlib", "stored":"f1", "text":false, "bytes":32}
                    ]
                })).unwrap()).unwrap();
                for name in ["f0", "f1"] { fs::write(staged.join(name), [n as u8; 32]).unwrap(); }
                fs::write(staged.join("stderr"), b"").unwrap();
                store.publish(&staged, &key).unwrap();
                assert!(store.note_unit_entry(&unit, &key, crate::wrapper::UNIT_VERSIONS).is_empty());
                fs::File::options().write(true).open(store.entry_dir(&key).join("meta.json")).unwrap()
                    .set_modified(std::time::SystemTime::now() - Duration::from_secs((5 - n) * 5000)).unwrap();
            }
            let event = |bytes| serde_json::json!({"type":"cache", "t":0, "result":"miss", "crate":"lib0",
                "version":"1", "compile_ms":1, "saved_ms":0, "bytes":bytes}).to_string();
            fs::write(dirs.ledger(), format!("{}\n{}\n", event(32), event(0))).unwrap();
            let ctx = Ctx {
                cargo: "cargo".into(), fcargo: "fcargo".into(), home: dirs.root().to_path_buf(),
                probe_id: "test".into(), audit: None, compression: "off".into(), store: StorePolicy::Sync,
            };
            let mut run = GrowthRun::new(&ctx, GrowthVariant::Auto, &project, &target, &dirs);
            run.settle_edits = true;
            run.build_ms = 41;
            run.cargo_compile_notices = 3;
            run.rustc_compilations = Some(3);
            run.source_settle_ms = 17;
            run.observe_cache(&before).unwrap();
            let guard = blocked.then(|| crate::cache::UnitRecordsGuard::try_acquire(&dirs).unwrap().unwrap());
            let first = compact::auto_compact_with(&dirs, &target, &run.auto);
            assert!(first.errors.is_empty(), "{:?}", first.errors);
            assert_eq!(first.removed.len(), if blocked { 0 } else { 2 });
            let mut row = run.row(13).unwrap();
            let evidence = row.cache_evidence.as_ref().unwrap();
            assert_eq!(evidence.retained_complete_entries, if blocked { 4 } else { 2 });
            assert_eq!(evidence.retained_unit_missing_entries, if blocked { 0 } else { 2 });
            drop(guard);
            let no_op_before = growth_ledger(&dirs).unwrap();
            run.observe_cache(&no_op_before).unwrap();
            let repeat = compact::auto_compact_with(&dirs, &target, &run.auto);
            assert!(repeat.errors.is_empty(), "{:?}", repeat.errors);
            assert_eq!(repeat.removed.len(), if blocked { 2 } else { 0 });
            run.build_ms += 7;
            run.refresh_row_storage(&mut row).unwrap();
            let evidence = row.cache_evidence.as_ref().unwrap();
            assert_eq!(evidence.observed_entry_keys, 4);
            assert_eq!(evidence.observed_entries_by_crate["lib0"], 4);
            assert_eq!((evidence.nonzero_byte_miss_records, evidence.zero_byte_miss_records), (1, 1));
            assert_eq!(evidence.retained_complete_entries, growth_store_records(&dirs).unwrap().len());
            assert_eq!((evidence.retained_complete_entries, evidence.retained_unit_records, evidence.retained_unit_keys), (2, 1, 4));
            assert_eq!((evidence.retained_unit_missing_entries, evidence.max_retained_keys_per_unit), (2, 4));
            assert_eq!(row.store, compact::physical_size(&[&dirs.store()]));
            assert_eq!(serde_json::to_value(&row.managed_allocation).unwrap(),
                serde_json::to_value(managed_allocation(&target, Some(dirs.root()))).unwrap());
            assert_eq!((row.build_ms, row.cargo_compile_notices, row.rustc_compilations, row.source_settle_ms), (41, 3, Some(3), 17));
            drop((before, no_op_before));
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn growth_serializes_unavailable_compiler_evidence_as_null_and_audited_zero_as_zero() {
        let root = std::env::temp_dir().join(format!("frust-growth-audit-report-test-{}", std::process::id()));
        let project = root.join("app");
        let target = root.join("target");
        let dirs = Dirs::at(root.join("home"));
        let mut ctx = Ctx {
            cargo: PathBuf::from("cargo"), fcargo: PathBuf::from("fcargo"), home: dirs.root().to_path_buf(),
            probe_id: "test".into(), audit: None, compression: "auto".into(), store: StorePolicy::Sync,
        };
        let run = GrowthRun::new(&ctx, GrowthVariant::Cargo, &project, &target, &dirs);
        let unknown = serde_json::to_value(run.row(0).unwrap()).unwrap();
        assert!(unknown["rustc_compilations"].is_null());
        assert_eq!(unknown["cargo_compile_notices"], 0);
        assert!(unknown.get("compiled").is_none());
        ctx.audit = Some(CompilerAudit { compiler: root.join("rustc"), logs: root.join("logs") });
        let run = GrowthRun::new(&ctx, GrowthVariant::Cargo, &project, &target, &dirs);
        let zero = serde_json::to_value(run.row(0).unwrap()).unwrap();
        assert_eq!(zero["rustc_compilations"], 0);
    }

    #[test]
    fn benchmark_environment_controls_compiler_and_policy_overrides() {
        let mut command = Command::new("cargo");
        controlled_environment(&mut command);
        let vars: std::collections::BTreeMap<_, _> = command.get_envs().collect();
        for name in ["CARGO_ENCODED_RUSTFLAGS", "RUSTC_WORKSPACE_WRAPPER", "RUSTC_WRAPPER", "CARGO_INCREMENTAL"] {
            assert_eq!(vars.get(std::ffi::OsStr::new(name)), Some(&None));
        }
        for (name, value) in [("RUSTFLAGS", ""), ("FRUST_TUNE", "off"), ("FRUST_STORE_SYNC", "1"), ("FRUST_LINK", "off")] {
            assert_eq!(vars.get(std::ffi::OsStr::new(name)), Some(&Some(std::ffi::OsStr::new(value))));
        }
        let flags = |args: &[&str]| args.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(compression_from_flags(&flags(&[])), Ok("auto".into()));
        assert_eq!(compression_from_flags(&flags(&["--compression=off"])), Ok("off".into()));
        assert_eq!(compression_from_flags(&flags(&["--compression", "auto"])), Ok("auto".into()));
        assert!(compression_from_flags(&flags(&["--compression", "invalid"])).is_err());
    }

    #[test]
    fn maintenance_policy_matches_controlled_cached_builds_and_reports_its_limits() {
        let options = benchmark_auto_options(false);
        let policy = serde_json::to_value(AutoCompactionPolicy::from(&options)).unwrap();
        assert!(options.cache_enabled);
        assert!(benchmark_maintenance_config().cache);
        assert_eq!(policy["enabled"], true);
        assert_eq!(policy["every_seconds"], 0);
        assert_eq!(policy["min_growth_bytes"], 0);
        assert_eq!(policy["keep_other_profile_seconds"], 14 * 86_400);
        assert_eq!(policy["incremental_idle_seconds"], 7 * 86_400);
        assert_eq!(policy["max_target_bytes"], 8u64 * 1024 * 1024 * 1024);
        assert_eq!(policy["cache_enabled"], true);
        assert_eq!(policy["pass_budget_ms"], 2000);
        assert_eq!(policy["pass_bytes"], 512 * 1024 * 1024);
        let maintenance = benchmark_auto_options(true);
        assert_eq!(maintenance.pass_budget, Duration::from_secs(3600));
        assert_eq!(maintenance.pass_bytes, u64::MAX);
    }

    #[cfg(unix)]
    #[test]
    fn compiler_audit_records_real_compilations_and_separates_queries_including_emit() {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if !isolated_compiler_audit_test("bench::tests::compiler_audit_records_real_compilations_and_separates_queries_including_emit") {
            return;
        }
        let root = std::env::temp_dir().join(format!("frust-bench-audit-test-{}", std::process::id()));
        let audit = CompilerAudit::create(&root).unwrap().unwrap();
        fs::create_dir_all(&audit.logs).unwrap();
        let probe = Command::new(&audit.compiler).arg("--version").env("FRUST_BENCH_AUDIT", &audit.logs).output().unwrap();
        assert!(probe.status.success());
        assert_eq!(fs::read_dir(&audit.logs).unwrap().count(), 1);
        let source = root.join("probe.rs");
        fs::write(&source, "pub fn answer() -> u32 { 42 }\n").unwrap();
        let compile = Command::new(&audit.compiler).args(["--crate-name", "probe", "--crate-type=lib", "--emit=metadata", "--out-dir"])
            .arg(&root).arg(&source).env("FRUST_BENCH_AUDIT", &audit.logs).output().unwrap();
        assert!(compile.status.success(), "{}", String::from_utf8_lossy(&compile.stderr));
        let print = Command::new(&audit.compiler)
            .args(["--crate-name", "probe", "--crate-type=lib", "--emit=metadata", "--print=file-names"])
            .arg(&source).env("FRUST_BENCH_AUDIT", &audit.logs).output().unwrap();
        assert!(print.status.success(), "{}", String::from_utf8_lossy(&print.stderr));
        let paths: Vec<_> = fs::read_dir(&audit.logs).unwrap().map(|e| e.unwrap().path()).collect();
        assert_eq!(paths.len(), 3);
        let calls: Vec<Vec<String>> = paths.iter().map(|path| fs::read(path).unwrap()
            .split(|byte| *byte == 0).filter(|arg| !arg.is_empty())
            .map(|arg| String::from_utf8(arg.to_vec()).unwrap()).collect()).collect();
        let (queries, compiles): (Vec<_>, Vec<_>) = calls.into_iter().partition(|args| rustc_query(args));
        assert_eq!(queries.len(), 2);
        assert_eq!(compiles.len(), 1);
        assert_eq!(CompilationCounts::from_invocations(&compiles).total, 1);
        assert!(queries.iter().any(|args| args.iter().any(|arg| arg == "--emit=metadata")));
        assert!(compiles[0].iter().any(|arg| arg == "--emit=metadata"));
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn compilation_prints_and_query_looking_operands_emit_objects_and_fail_restore_audit() {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if !isolated_compiler_audit_test("bench::tests::compilation_prints_and_query_looking_operands_emit_objects_and_fail_restore_audit") {
            return;
        }
        let root = std::env::temp_dir().join(format!("frust-bench-print-work-test-{}", std::process::id()));
        let _scratch = Scratch(root.clone(), false);
        let source = root.join("probe.rs");
        let audit = CompilerAudit::create(&root).unwrap().unwrap();
        fs::write(&source, "pub fn answer() -> u32 { 42 }\n").unwrap();
        let ctx = Ctx {
            cargo: PathBuf::from("cargo"), fcargo: PathBuf::from("fcargo"), home: root.join("home"),
            probe_id: "test".into(), audit: Some(audit.clone()), compression: "off".into(), store: StorePolicy::Sync,
        };
        // Each audited command must produce real code, including option operands that resemble queries.
        for (name, kind, extra, filename) in [
            ("link_args", "lib", vec!["--print=link-args"], "probe.o"),
            ("native_static_libs", "staticlib", vec!["--print", "native-static-libs"], "probe.o"),
            ("remap_operand", "lib", vec!["--remap-path-prefix", "--print=cfg"], "probe.o"),
            ("output_operand", "lib", vec!["-o", "--print=sysroot"], "--print=sysroot"),
            ("after_sentinel", "lib", vec!["--"], "probe.o"),
        ] {
            let directory = root.join(name).join("deps");
            fs::create_dir_all(&directory).unwrap();
            let input = if name == "after_sentinel" {
                fs::copy(&source, directory.join("--print=cfg")).unwrap();
                PathBuf::from("--print=cfg")
            } else { source.clone() };
            let mut command = Command::new(&audit.compiler);
            command.current_dir(&directory).args(["--crate-name=probe", "--crate-type", kind, "--emit=obj",
                "--error-format=json", "--json=artifacts", "--out-dir"])
                .arg(&directory).args(extra).arg(input);
            let mut run = timed(&ctx, false, &mut command).unwrap();
            assert!(fs::metadata(directory.join(filename)).unwrap().len() > 0, "{name}");
            assert_eq!(run.rustc_call_count, Some(1));
            assert!(run.rustc_query_invocations.as_ref().unwrap().is_empty(), "{name}");
            let counts = run.rustc_compilation_counts.unwrap();
            assert_eq!(counts.total, 1);
            assert_eq!(counts.cargo_units, 1);
            assert!(validate_build_evidence(&run, "cold", false).is_ok());
            run.cache_hits = 1;
            assert!(validate_build_evidence(&run, "clean rebuild", true).is_err(), "{name}");
        }
    }

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
            t.add("row", Run { millis: ms, ..Run::default() }, None);
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
