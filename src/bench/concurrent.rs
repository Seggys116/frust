//! Paired cold groups measure cooperation between two equivalent Cargo profiles.

use super::*;
use std::collections::BTreeMap;

const ROLES: [&str; 2] = ["coldone", "coldtwo"];
type Units = BTreeMap<String, usize>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum ConcurrentProfile {
    Dev,
    Release,
    Thin,
    Fat,
}

#[derive(Serialize)]
struct EffectiveProfile {
    inherits: &'static str,
    lto: &'static str,
    codegen_units: usize,
    panic: &'static str,
    incremental: bool,
    debug: bool,
}

impl ConcurrentProfile {
    fn parse(flags: &[String]) -> Result<Self, String> {
        let mut selected = None;
        for flag in flags {
            if flag == "--concurrent-profile" {
                return Err("--concurrent-profile requires =dev|release|thin|fat".into());
            }
            if let Some(value) = flag.strip_prefix("--concurrent-profile=") {
                if selected.is_some() {
                    return Err("--concurrent-profile must be specified once".into());
                }
                selected = Some(match value {
                    "dev" => Self::Dev,
                    "release" => Self::Release,
                    "thin" => Self::Thin,
                    "fat" => Self::Fat,
                    _ => {
                        return Err(
                            "--concurrent-profile requires dev, release, thin or fat".into()
                        );
                    }
                });
            }
        }
        Ok(selected.unwrap_or(Self::Dev))
    }

    fn incremental(self, flags: &[String]) -> Result<bool, String> {
        let mut selected = None;
        for flag in flags {
            if flag == "--concurrent-incremental" {
                return Err("--concurrent-incremental requires =on|off".into());
            }
            if let Some(value) = flag.strip_prefix("--concurrent-incremental=") {
                if selected.is_some() {
                    return Err("--concurrent-incremental must be specified once".into());
                }
                selected = Some(match value {
                    "on" => true,
                    "off" => false,
                    _ => return Err("--concurrent-incremental requires on or off".into()),
                });
            }
        }
        if self == Self::Dev && selected.is_some() {
            return Err(
                "--concurrent-incremental requires a release, thin or fat concurrent profile"
                    .into(),
            );
        }
        Ok(selected.unwrap_or(self == Self::Dev))
    }

    fn effective(self) -> EffectiveProfile {
        EffectiveProfile {
            inherits: if self == Self::Dev { "dev" } else { "release" },
            lto: match self {
                Self::Thin => "thin",
                Self::Fat => "fat",
                _ => "false",
            },
            codegen_units: match self {
                Self::Dev => 256,
                Self::Fat => 1,
                _ => 16,
            },
            panic: if self == Self::Fat { "abort" } else { "unwind" },
            incremental: self == Self::Dev,
            debug: self == Self::Dev,
        }
    }

    fn declarations(self, incremental: bool) -> String {
        let mut text = String::new();
        for role in ROLES {
            let settings = self.effective();
            let _ = write!(
                text,
                "\n[profile.{role}]\ninherits = \"{}\"\n",
                settings.inherits
            );
            if self == Self::Dev {
                #[cfg(target_os = "macos")]
                text.push_str("debug = true\nincremental = true\nsplit-debuginfo = \"packed\"\nstrip = \"debuginfo\"\n");
            } else {
                // Override the fixture's fat release defaults for both equivalent roles.
                let lto = if settings.lto == "false" {
                    "false".to_string()
                } else {
                    format!("\"{}\"", settings.lto)
                };
                let _ = write!(
                    text,
                    "lto = {lto}\ncodegen-units = {}\npanic = \"{}\"\nincremental = {incremental}\ndebug = false\n",
                    settings.codegen_units, settings.panic
                );
            }
        }
        text
    }
}

#[derive(Serialize)]
struct BinaryBaseline {
    hash: String,
    bytes: Option<u64>,
    tool: &'static str,
    phase: &'static str,
    repetition: usize,
    source_binary: PathBuf,
    binary: PathBuf,
    audit_directory: PathBuf,
    command: String,
}

#[cfg(target_os = "macos")]
#[derive(Clone, PartialEq, Eq, Serialize)]
struct NativeDebugFile {
    hash: String,
    bytes: u64,
    mode: u32,
}

#[cfg(target_os = "macos")]
#[derive(Clone, Serialize)]
struct NativeDebugProof {
    bundle: PathBuf,
    files: BTreeMap<String, NativeDebugFile>,
    executable_mode: u32,
    uuid: String,
    sources_with_types_and_locals: Vec<String>,
    inspection_logs: PathBuf,
}

#[derive(Serialize)]
struct Role {
    profile: String,
    target: PathBuf,
    command: String,
    pid: u32,
    spawn_begin_ns: u64,
    spawn_return_ns: u64,
    observed_exit_ns: u64,
    exit_code: Option<i32>,
    cargo_compile_notices: usize,
    cargo_stderr: String,
    rustc_invocations: Vec<Vec<String>>,
    rustc_query_invocations: Vec<Vec<String>>,
    rustc_exit_codes: Vec<i32>,
    rustc_calls: Vec<serde_json::Value>,
    rustc_invocation_classes: Vec<CompilationClass>,
    rustc_compilation_counts: CompilationCounts,
    rustc_call_count: usize,
    binary_hash: String,
    runtime_stdout_hash: String,
    runtime_stdout: String,
    #[cfg(target_os = "macos")]
    native_debug: Option<NativeDebugProof>,
}

#[derive(Serialize)]
struct Group {
    tool: &'static str,
    phase: &'static str,
    repetition: usize,
    roles_start_order: Vec<String>,
    foreground_ns: u64,
    store_drain_ns: u64,
    settled_ns: u64,
    spawn_skew_ns: u64,
    load_before: Option<f64>,
    load_after: Option<f64>,
    cold_start: Option<serde_json::Value>,
    cache_events: Vec<serde_json::Value>,
    roles: Vec<Role>,
    cross_role_reuse: Vec<serde_json::Value>,
    unit_multiplicity: Option<serde_json::Value>,
    managed_allocation: serde_json::Value,
}

#[derive(Serialize)]
struct Summary {
    raw_ns: Vec<i64>,
    median_ns: i64,
    min_ns: i64,
    max_ns: i64,
}

impl Summary {
    fn new(raw_ns: Vec<i64>) -> Self {
        let mut sorted = raw_ns.clone();
        sorted.sort_unstable();
        let middle = sorted.len() / 2;
        let median_ns = if sorted.len().is_multiple_of(2) {
            ((sorted[middle - 1] as i128 + sorted[middle] as i128) / 2) as i64
        } else {
            sorted[middle]
        };
        Self {
            raw_ns,
            median_ns,
            min_ns: sorted[0],
            max_ns: sorted[sorted.len() - 1],
        }
    }
}

#[derive(Serialize)]
struct ConcurrentReport {
    evidence_schema_version: u32,
    hash_algorithm: &'static str,
    mode: &'static str,
    method: &'static str,
    limits: [&'static str; 3],
    store_policy: StorePolicy,
    store_completion_method: &'static str,
    runs: usize,
    registry_deps: bool,
    local_crates: usize,
    rustc: String,
    rustc_verbose: String,
    toolchain_probe_id: String,
    cargo_version: String,
    cache_compression: String,
    filesystem: Option<String>,
    machine: Machine,
    profiles: [&'static str; 2],
    selected_profile: ConcurrentProfile,
    effective_profile: EffectiveProfile,
    input_hashes: BTreeMap<String, String>,
    compiler_info_hashes: BTreeMap<String, String>,
    source_settlement: SourceSettlement,
    warmups: Vec<Group>,
    groups: Vec<Group>,
    paired_foreground_frust_minus_cargo: Summary,
    paired_settled_frust_minus_cargo: Summary,
    #[cfg(target_os = "macos")]
    native_debug_policy: &'static str,
    #[cfg(target_os = "macos")]
    native_debug_cache_restores: Vec<serde_json::Value>,
}

pub(super) fn run(flags: &[String]) -> i32 {
    let result = (|| {
        validate_flags(flags)?;
        let profile = ConcurrentProfile::parse(flags)?;
        let incremental = profile.incremental(flags)?;
        let quick = flags.iter().any(|flag| flag == "--quick");
        let runs = runs_from_flags(flags, quick)?;
        let compression = compression_from_flags(flags)?;
        let store = if flags
            .iter()
            .any(|flag| flag == "--store" || flag.starts_with("--store="))
        {
            store_from_flags(flags)?
        } else {
            StorePolicy::Deferred
        };
        let keep = flags.iter().any(|flag| flag == "--keep-scratch");
        fsx::require_free_space(&std::env::temp_dir(), MIN_FREE_BYTES)?;
        let scratch = std::env::temp_dir().join(format!(
            "{}-{}",
            store.scratch_prefix(keep),
            std::process::id()
        ));
        let mut guard = Scratch(scratch.clone(), keep);
        let result = benchmark(
            &scratch,
            quick,
            runs,
            &compression,
            store,
            profile,
            incremental,
        );
        // A failed group can leave deferred writers holding its fixture files.
        if result.is_err() {
            guard.1 = true;
        }
        result
    })();
    match result {
        Ok(report) => {
            if flags.iter().any(|flag| flag == "--json") {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report).unwrap_or_default()
                );
            } else {
                println!(
                    "Concurrent cold groups: {} paired runs, {} storage",
                    report.runs,
                    report.store_policy.label()
                );
                println!(
                    "Frust minus Cargo foreground: median {:.3} ms (min {:.3}, max {:.3})",
                    report.paired_foreground_frust_minus_cargo.median_ns as f64 / 1e6,
                    report.paired_foreground_frust_minus_cargo.min_ns as f64 / 1e6,
                    report.paired_foreground_frust_minus_cargo.max_ns as f64 / 1e6
                );
                println!(
                    "Frust minus Cargo settled: median {:.3} ms; validated peer reuse and unchanged builds in every pair",
                    report.paired_settled_frust_minus_cargo.median_ns as f64 / 1e6
                );
            }
            0
        }
        Err(error) => {
            eprintln!("frust bench: {error}");
            1
        }
    }
}

fn validate_flags(flags: &[String]) -> Result<(), String> {
    if flags
        .iter()
        .filter(|flag| flag.as_str() == "--concurrent")
        .count()
        != 1
        || flags.iter().any(|flag| flag.starts_with("--concurrent="))
    {
        return Err("--concurrent must be specified once without a value".into());
    }
    for flag in flags {
        let name = flag.split('=').next().unwrap_or(flag);
        if matches!(
            name,
            "--profiles" | "--growth" | "--churn" | "--linking" | "--latency" | "--settle-edits"
        ) {
            return Err(format!("--concurrent cannot be combined with {name}"));
        }
    }
    if !cfg!(any(target_os = "linux", target_os = "macos")) {
        return Err(
            "--concurrent requires the Linux/macOS compiler audit backend; Windows is not supported"
                .into(),
        );
    }
    Ok(())
}

fn create_audit(root: &Path) -> Result<CompilerAudit, String> {
    let mut audit =
        CompilerAudit::create(root)?.ok_or("concurrent compiler audit is unavailable")?;
    audit.logs = root.join("audit");
    let rustc = std::env::var_os("RUSTC")
        .map(PathBuf::from)
        .or_else(|| resolve_tool("rustc"))
        .ok_or("rustc not found")?;
    let quoted = shell_quote(&rustc)?;
    // The stable compiler path participates in Cargo's independently warmed compiler-info memo.
    let script = format!(
        "#!/bin/sh\nif [ -n \"${{FRUST_BENCH_AUDIT:-}}\" ]; then\n printf '%s\\0' \"$@\" > \"$FRUST_BENCH_AUDIT/$$.argv\" || exit 125\nfi\n{quoted} \"$@\"\ncode=$?\nif [ -n \"${{FRUST_BENCH_AUDIT:-}}\" ]; then\n printf '%s\\n' \"$code\" > \"$FRUST_BENCH_AUDIT/$$.status\" || exit 125\nfi\nexit \"$code\"\n"
    );
    fs::write(&audit.compiler, script).map_err(|error| error.to_string())?;
    Ok(audit)
}

fn hashes(paths: &[PathBuf]) -> Result<BTreeMap<String, String>, String> {
    paths
        .iter()
        .map(|path| {
            crate::cache::hash_file(path)
                .map(|hash| (path.display().to_string(), hash))
                .map_err(|error| error.to_string())
        })
        .collect()
}

fn source_paths(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut result = Vec::new();
    for entry in fs::read_dir(root).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        if entry
            .file_type()
            .map_err(|error| error.to_string())?
            .is_dir()
        {
            result.extend(source_paths(&entry.path())?);
        } else {
            result.push(entry.path());
        }
    }
    result.sort();
    Ok(result)
}

fn entry_names(path: &Path) -> Result<Vec<String>, String> {
    let mut names = fs::read_dir(path)
        .map_err(|error| error.to_string())?
        .map(|entry| {
            entry.map_err(|error| error.to_string()).and_then(|entry| {
                entry
                    .file_name()
                    .into_string()
                    .map_err(|_| "non-UTF-8 cold directory entry".into())
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    names.sort();
    Ok(names)
}

fn rotate(root: &Path, home: &Path, targets: &[PathBuf; 2], label: &str) -> Result<(), String> {
    let archive = root.join("retained").join(label);
    fs::create_dir_all(&archive).map_err(|error| error.to_string())?;
    // Renaming preserves exact active paths for the next arm without retaining warm artifacts there.
    for (name, path) in [
        ("home", home),
        (ROLES[0], &targets[0]),
        (ROLES[1], &targets[1]),
    ] {
        if path.exists() {
            sys::rename(path, &archive.join(name)).map_err(|error| error.to_string())?;
        }
        fs::create_dir_all(path).map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn benchmark(
    root: &Path,
    quick: bool,
    runs: usize,
    compression: &str,
    store: StorePolicy,
    profile: ConcurrentProfile,
    incremental: bool,
) -> Result<ConcurrentReport, String> {
    fs::create_dir_all(root).map_err(|error| error.to_string())?;
    let cargo = resolve_tool("cargo").ok_or("cargo not found")?;
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let fcargo = fcargo_beside(&executable);
    let probe = Probe::run(None, root).ok_or("could not query rustc")?;
    let audit = create_audit(root)?;
    let ctx = Ctx {
        cargo,
        fcargo,
        home: root.join("home"),
        probe_id: probe.id.clone(),
        audit: Some(audit.clone()),
        compression: compression.into(),
        store,
    };
    let cargo_version = Command::new(&ctx.cargo)
        .arg("--version")
        .output()
        .map_err(|error| error.to_string())?;
    if !cargo_version.status.success() {
        return Err("cargo --version failed".into());
    }
    let cargo_version = String::from_utf8_lossy(&cargo_version.stdout)
        .trim()
        .to_owned();
    let before = load_average();
    let project = root.join("app");
    let local_crates = if quick { 2 } else { 8 };
    let registry_deps = write_project(&project, local_crates, true)
        .and_then(|_| resolve_offline(&ctx, &project))
        .is_ok();
    if !registry_deps {
        write_project(&project, local_crates, false)?;
    }
    let manifest = project.join("Cargo.toml");
    let mut text = fs::read_to_string(&manifest).map_err(|error| error.to_string())?;
    text.push_str(&profile.declarations(incremental));
    fs::write(&manifest, text).map_err(|error| error.to_string())?;
    resolve_offline(&ctx, &project)?;
    let sources = source_paths(&project)?;
    // Freeze sources and executable inputs before warmups can populate any state.
    let mut inputs = sources.clone();
    inputs.extend([
        executable.clone(),
        ctx.fcargo.clone(),
        executable.with_file_name(format!("frust-wrap{}", std::env::consts::EXE_SUFFIX)),
        ctx.cargo.clone(),
        audit.compiler.clone(),
    ]);
    let real_rustc = std::env::var_os("RUSTC")
        .map(PathBuf::from)
        .or_else(|| resolve_tool("rustc"))
        .ok_or("rustc not found")?;
    inputs.push(real_rustc);
    let input_hashes = hashes(&inputs)?;
    let targets = [root.join("target-coldone"), root.join("target-coldtwo")];
    let mut warmups = Vec::new();
    let mut memos = BTreeMap::new();
    let mut compiler_info_hashes = BTreeMap::new();
    let mut expected = BTreeMap::new();
    let mut output = None;
    #[cfg(target_os = "macos")]
    let mut native_debug_cache_restores = Vec::new();
    #[cfg(target_os = "macos")]
    let _ = wait_sources_settled(&sources, SOURCE_SETTLE_TIMEOUT)?;
    for frust in [false, true] {
        // Warm each tool and role independently, copying only its compiler-info memo later.
        for role in 0..2 {
            let group = group(
                &ctx,
                &project,
                &targets,
                profile,
                &[role],
                frust,
                "warmup",
                0,
                None,
                &mut expected,
                &mut output,
            )?;
            #[cfg(target_os = "macos")]
            if frust && profile == ConcurrentProfile::Dev {
                native_debug_cache_restores.push(restore_native_debug(
                    &ctx,
                    &project,
                    &targets,
                    role,
                    &group.roles[0],
                    &mut expected,
                    &mut output,
                )?);
            }
            warmups.push(group);
            let memo = targets[role].join(".rustc_info.json");
            let key = format!("{}-{}", if frust { "frust" } else { "cargo" }, ROLES[role]);
            compiler_info_hashes.insert(
                key.clone(),
                crate::cache::hash_file(&memo).map_err(|error| error.to_string())?,
            );
            memos.insert(key, fs::read(memo).map_err(|error| error.to_string())?);
        }
        rotate(
            root,
            &ctx.home,
            &targets,
            if frust { "warm-frust" } else { "warm-cargo" },
        )?;
    }
    let (source_settlement, _) = wait_sources_settled(&sources, SOURCE_SETTLE_TIMEOUT)?;
    let mut groups = Vec::new();
    let mut foreground_deltas = Vec::new();
    let mut settled_deltas = Vec::new();
    for repetition in 0..runs {
        guard_disk(root)?;
        let start_order = if repetition % 2 == 0 { [0, 1] } else { [1, 0] };
        let arms = if repetition % 2 == 0 {
            [false, true]
        } else {
            [true, false]
        };
        let mut paired = Vec::new();
        for frust in arms {
            let tool = if frust { "frust" } else { "cargo" };
            if fs::read_dir(&ctx.home)
                .map_err(|error| error.to_string())?
                .next()
                .is_some()
            {
                return Err("cold shared home is not empty".into());
            }
            for role in 0..2 {
                if fs::read_dir(&targets[role])
                    .map_err(|error| error.to_string())?
                    .next()
                    .is_some()
                {
                    return Err("cold target is not empty".into());
                }
                fs::write(
                    targets[role].join(".rustc_info.json"),
                    &memos[&format!("{tool}-{}", ROLES[role])],
                )
                .map_err(|error| error.to_string())?;
            }
            let home_entries = entry_names(&ctx.home)?;
            let mut target_entries = BTreeMap::new();
            let mut memo_hashes = BTreeMap::new();
            for role in 0..2 {
                let entries = entry_names(&targets[role])?;
                if entries != [".rustc_info.json"] {
                    return Err("cold target contains more than its compiler-info memo".into());
                }
                target_entries.insert(ROLES[role], entries);
                memo_hashes.insert(
                    ROLES[role],
                    crate::cache::hash_file(&targets[role].join(".rustc_info.json"))
                        .map_err(|error| error.to_string())?,
                );
            }
            if !home_entries.is_empty() {
                return Err("cold shared home changed before group start".into());
            }
            let state = serde_json::json!({"home_entries": home_entries, "target_entries": target_entries, "compiler_info_hashes": memo_hashes});
            paired.push(group(
                &ctx,
                &project,
                &targets,
                profile,
                &start_order,
                frust,
                "cold",
                repetition,
                Some(state),
                &mut expected,
                &mut output,
            )?);
            groups.push(group(
                &ctx,
                &project,
                &targets,
                profile,
                &start_order,
                frust,
                "no-op",
                repetition,
                None,
                &mut expected,
                &mut output,
            )?);
            rotate(
                root,
                &ctx.home,
                &targets,
                &format!("pair-{repetition}-{tool}"),
            )?;
        }
        // Cargo sorts before frust regardless of which arm ran first.
        paired.sort_by_key(|group| group.tool);
        let (cargo, frust) = paired.split_at_mut(1);
        validate_reuse(&cargo[0], &mut frust[0])?;
        foreground_deltas.push(frust[0].foreground_ns as i64 - cargo[0].foreground_ns as i64);
        settled_deltas.push(frust[0].settled_ns as i64 - cargo[0].settled_ns as i64);
        groups.extend(paired);
    }
    if hashes(&inputs)? != input_hashes {
        return Err("benchmark inputs changed during concurrent groups".into());
    }
    Ok(ConcurrentReport {
        evidence_schema_version: EVIDENCE_SCHEMA_VERSION,
        hash_algorithm: "blake3",
        mode: "concurrent_cold_groups",
        method: "Two equivalent selected profiles; empty separate targets and shared home per group except independently warmed compiler-info. Alternating arm and spawn order. Blocking foreground waiters; one shared-home store drain before settled clock. Same compiler audit bridge, source and target paths in both arms.",
        limits: [
            "Measures two concurrent builds with intra-group peer reuse, not isolated cold-build speed.",
            "OS page and registry caches remain warm; compiler instrumentation affects scheduling and reuse.",
            "Store completion proves no pending job and simultaneous exclusive slot ownership; it does not prove all descendants have exited.",
        ],
        store_policy: store,
        store_completion_method: "After both foreground exit clocks, observe no deferred jobs and hold every existing slot lock exclusively together; record this shared-home drain once per group.",
        runs,
        registry_deps,
        local_crates,
        rustc: probe.release,
        rustc_verbose: probe.rustc_verbose,
        toolchain_probe_id: probe.id,
        cargo_version,
        cache_compression: compression.into(),
        filesystem: sys::fs_type_name(root),
        machine: machine_context(before),
        profiles: ROLES,
        selected_profile: profile,
        effective_profile: EffectiveProfile {
            incremental,
            ..profile.effective()
        },
        input_hashes,
        compiler_info_hashes,
        source_settlement,
        warmups,
        groups,
        paired_foreground_frust_minus_cargo: Summary::new(foreground_deltas),
        paired_settled_frust_minus_cargo: Summary::new(settled_deltas),
        #[cfg(target_os = "macos")]
        native_debug_policy: if profile != ConcurrentProfile::Dev {
            "Not applicable: selected release profile explicitly disables debug information; executable byte and runtime checks remain required."
        } else {
            "Both fixture profiles retain full debug information and incremental compilation, with split-debuginfo=packed and strip=debuginfo. Whole executable bytes are compared unchanged. Native DWARF verification, source/type/local-variable checks and executable/bundle UUID checks run after settled clocks for every role; separate untimed warmup clean restores prove executable, runtime and complete debug bundle preservation with zero declared-unit or unknown compilations."
        },
        #[cfg(target_os = "macos")]
        native_debug_cache_restores,
    })
}

fn read_audit(directory: &Path) -> Result<(Vec<Vec<String>>, Vec<i32>), String> {
    let mut paths = fs::read_dir(directory)
        .map_err(|error| error.to_string())?
        .map(|entry| {
            entry
                .map(|entry| entry.path())
                .map_err(|error| error.to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    paths.sort();
    let argv: Vec<_> = paths
        .iter()
        .filter(|path| path.extension().is_some_and(|ext| ext == "argv"))
        .collect();
    if argv.len() * 2 != paths.len() {
        return Err("unfinished compiler audit invocation".into());
    }
    let mut calls = Vec::new();
    let mut codes = Vec::new();
    for path in argv {
        let bytes = fs::read(path).map_err(|error| error.to_string())?;
        let bytes = bytes
            .strip_suffix(&[0])
            .ok_or("invalid compiler audit NUL framing")?;
        let args = bytes
            .split(|byte| *byte == 0)
            .map(|arg| String::from_utf8(arg.to_vec()).map_err(|error| error.to_string()))
            .collect::<Result<Vec<_>, _>>()?;
        let status =
            fs::read_to_string(path.with_extension("status")).map_err(|error| error.to_string())?;
        let code = status
            .trim()
            .parse::<i32>()
            .map_err(|error| error.to_string())?;
        let class = classify_compilation(&args);
        if !rustc_query(&args) && class == CompilationClass::Unknown {
            return Err(format!("unknown compiler invocation: {args:?}"));
        }
        if code != 0 && class != CompilationClass::BuildScriptProbe {
            return Err(format!(
                "compiler audit invocation failed ({code}): {args:?}"
            ));
        }
        calls.push(args);
        codes.push(code);
    }
    Ok((calls, codes))
}

struct Foregrounds {
    started: Instant,
    exited: Instant,
    spawned: Vec<(usize, String, u32, u64, u64)>,
    exits: Vec<(Result<std::process::ExitStatus, String>, Instant)>,
    failure: Option<String>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
use crate::sys::{ChildObserveError as ObserveError, observe_child_unreaped as child_state};

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn observe_exit(pid: u32) -> Result<Instant, ObserveError> {
    child_state(pid, true).map(|_| Instant::now())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run_foregrounds(
    commands: Vec<(usize, Command)>,
    timeout: Duration,
) -> Result<Foregrounds, String> {
    run_foregrounds_with(
        commands,
        timeout,
        Duration::from_secs(5),
        observe_exit,
        crate::sys::cancel_owned_process_group,
    )
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run_foregrounds_with(
    commands: Vec<(usize, Command)>,
    timeout: Duration,
    cleanup_timeout: Duration,
    observe: fn(u32) -> Result<Instant, ObserveError>,
    cancel: fn(u32) -> Result<(), String>,
) -> Result<Foregrounds, String> {
    let started = Instant::now();
    let (sender, receiver) = mpsc::channel();
    let mut children = Vec::new();
    let mut spawned = Vec::new();
    let mut failure = None;
    for (role, mut command) in commands {
        crate::sys::own_process_group(&mut command);
        let rendered = format!("{command:?}");
        let spawn_begin_ns = started.elapsed().as_nanos() as u64;
        match command.spawn() {
            Ok(child) => {
                let spawn_return_ns = started.elapsed().as_nanos() as u64;
                let pid = child.id();
                children.push(Some(child));
                let sender = sender.clone();
                let index = spawned.len();
                // Observers only timestamp; all leaders stay unreaped until commit or cancellation of every group.
                thread::spawn(move || {
                    let _ = sender.send((index, observe(pid)));
                });
                spawned.push((role, rendered, pid, spawn_begin_ns, spawn_return_ns));
            }
            Err(error) => {
                failure = Some(format!("concurrent spawn failed: {error}"));
                break;
            }
        }
    }
    drop(sender);
    let mut observations: Vec<Option<Instant>> = (0..spawned.len()).map(|_| None).collect();
    let mut ownership_lost = vec![false; spawned.len()];
    while failure.is_none() && observations.iter().any(Option::is_none) {
        match receiver.recv_timeout((started + timeout).saturating_duration_since(Instant::now())) {
            Ok((index, Ok(exited))) => observations[index] = Some(exited),
            Ok((index, Err(error))) => {
                ownership_lost[index] = error.ownership_lost;
                failure = Some(error.message);
            }
            Err(error) => {
                failure = Some(format!(
                    "concurrent foreground watchdog exceeded {}ms or observer disconnected: {error}",
                    timeout.as_millis()
                ))
            }
        }
    }
    if failure.is_none() {
        // Verify terminal success while all leader pins still protect cancellation of failed groups' descendants.
        for (index, child) in children.iter().enumerate() {
            match child_state(child.as_ref().unwrap().id(), false) {
                Ok(Some(true)) => {}
                Ok(Some(false)) => {
                    failure.get_or_insert_with(|| {
                        format!("concurrent foreground role {} failed", spawned[index].0)
                    });
                }
                Ok(None) => {
                    failure.get_or_insert_with(|| {
                        "foreground terminal observation became unavailable".into()
                    });
                }
                Err(error) => {
                    ownership_lost[index] = error.ownership_lost;
                    failure.get_or_insert(error.message);
                }
            }
        }
    }
    if failure.is_some() {
        // Drain already reported ownership losses before deciding which still-pinned groups may be signalled.
        for (index, observed) in receiver.try_iter() {
            match observed {
                Ok(exited) => observations[index] = Some(exited),
                Err(error) => ownership_lost[index] |= error.ownership_lost,
            }
        }
        for (index, child) in children.iter().enumerate() {
            if ownership_lost[index] {
                continue;
            }
            if let Some(child) = child
                && let Err(error) = cancel(child.id())
            {
                failure = Some(format!(
                    "{}; {error}",
                    failure.as_deref().unwrap_or("foreground failed")
                ));
            }
        }
    }
    let deadline = Instant::now() + cleanup_timeout;
    let mut exits: Vec<Option<_>> = (0..spawned.len()).map(|_| None).collect();
    loop {
        for (index, child) in children.iter_mut().enumerate() {
            if ownership_lost[index] || child.is_none() {
                continue;
            }
            let terminal = if observations[index].is_some() {
                Ok(Some(true))
            } else {
                child_state(child.as_ref().unwrap().id(), false)
            };
            match terminal {
                Ok(Some(_)) => {
                    let observed = observations[index].unwrap_or_else(Instant::now);
                    // Only a successful WNOWAIT terminal observation permits this blocking reap.
                    let status = child
                        .take()
                        .unwrap()
                        .wait()
                        .map_err(|error| error.to_string());
                    exits[index] = Some((status, observed));
                }
                Ok(None) => {}
                Err(error) => {
                    ownership_lost[index] = error.ownership_lost;
                    failure = Some(format!(
                        "{}; {}",
                        failure.as_deref().unwrap_or("foreground cleanup failed"),
                        error.message
                    ));
                }
            }
        }
        if children.iter().all(Option::is_none) {
            break;
        }
        if Instant::now() >= deadline {
            let unresolved = children.iter().filter(|child| child.is_some()).count();
            for (index, child) in children.into_iter().enumerate() {
                if let Some(mut child) = child
                    && !ownership_lost[index]
                {
                    // An unresolved leader remains owned by an exit-only reaper; no unproven blocking wait delays return.
                    thread::spawn(move || {
                        if observe_exit(child.id()).is_ok() {
                            let _ = child.wait();
                        }
                    });
                }
            }
            return Err(format!(
                "{}; unsettled cleanup: {unresolved} foreground leaders not reaped within {}ms",
                failure.as_deref().unwrap_or("foreground failed"),
                cleanup_timeout.as_millis()
            ));
        }
        // Failure cleanup may poll terminal state; successful measurements use only blocking observer exit clocks.
        thread::sleep(
            Duration::from_millis(5).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
    let exits: Vec<_> = exits.into_iter().flatten().collect();
    let exited = exits
        .iter()
        .map(|(_, exited)| *exited)
        .max()
        .unwrap_or(started);
    Ok(Foregrounds {
        started,
        exited,
        spawned,
        exits,
        failure,
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn run_foregrounds(
    _commands: Vec<(usize, Command)>,
    _timeout: Duration,
) -> Result<Foregrounds, String> {
    Err("concurrent foreground audit requires Linux or macOS".into())
}

fn retain_binary(evidence: &BinaryBaseline) -> Result<(), String> {
    let directory = evidence
        .binary
        .parent()
        .ok_or("binary evidence directory missing")?;
    fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    let bytes =
        fs::copy(&evidence.source_binary, &evidence.binary).map_err(|error| error.to_string())?;
    let hash = crate::cache::hash_file(&evidence.binary).map_err(|error| error.to_string())?;
    if hash != evidence.hash || evidence.bytes.is_some_and(|expected| expected != bytes) {
        return Err("binary changed while retaining correctness evidence".into());
    }
    let metadata = serde_json::to_vec_pretty(evidence).map_err(|error| error.to_string())?;
    fs::write(directory.join("evidence.json"), metadata).map_err(|error| error.to_string())
}

#[cfg(target_os = "macos")]
fn native_binary_diagnostics(
    baseline: &BinaryBaseline,
    actual: &BinaryBaseline,
) -> Result<(), String> {
    let directory = actual
        .binary
        .parent()
        .ok_or("binary evidence directory missing")?
        .join("native");
    fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    let mut commands = Vec::new();
    for (label, evidence) in [("baseline", baseline), ("actual", actual)] {
        for (name, tool, args) in [
            ("debug-map", "/usr/bin/dsymutil", vec!["--dump-debug-map"]),
            ("load-commands", "/usr/bin/otool", vec!["-l"]),
            ("uuid", "/usr/bin/dwarfdump", vec!["--uuid"]),
            (
                "signature",
                "/usr/bin/codesign",
                vec!["--display", "--verbose=4"],
            ),
        ] {
            let mut command = Command::new(tool);
            command
                .args(args)
                .arg(&evidence.binary)
                .stdin(Stdio::null())
                .stdout(
                    fs::File::create(directory.join(format!("{label}-{name}.stdout")))
                        .map_err(|error| error.to_string())?,
                )
                .stderr(
                    fs::File::create(directory.join(format!("{label}-{name}.stderr")))
                        .map_err(|error| error.to_string())?,
                );
            commands.push((commands.len(), command));
        }
    }
    // Native inspection shares one bounded watchdog and never changes correctness acceptance.
    let foreground = run_foregrounds(commands, Duration::from_secs(10))?;
    let statuses: Vec<_> = foreground
        .spawned
        .iter()
        .zip(&foreground.exits)
        .map(|((_, command, pid, _, _), (status, _))| {
            serde_json::json!({
                "command": command, "pid": pid,
                "exit_code": status.as_ref().ok().and_then(|status| status.code()),
                "error": status.as_ref().err(),
            })
        })
        .collect();
    let metadata = serde_json::to_vec_pretty(&serde_json::json!({
        "failure": &foreground.failure, "commands": statuses,
    }))
    .map_err(|error| error.to_string())?;
    fs::write(directory.join("status.json"), metadata).map_err(|error| error.to_string())?;
    foreground.failure.map_or(Ok(()), Err)
}

#[cfg(target_os = "macos")]
fn dwarf_named_entry(text: &str, tag: &str, name: &str, located: bool) -> bool {
    text.split("DW_TAG_").any(|entry| {
        entry.starts_with(tag)
            && entry
                .lines()
                .any(|line| line.contains("DW_AT_name") && line.contains(&format!("\"{name}\"")))
            && if located {
                entry
                    .lines()
                    .any(|line| line.contains("DW_AT_type") && line.contains("\"u64\""))
                    && entry.contains("DW_AT_location")
            } else {
                entry.contains("DW_AT_byte_size") && entry.contains("DW_AT_encoding")
            }
    })
}

#[cfg(target_os = "macos")]
fn verify_dwarf_sources(text: &str, sources: &[String]) -> Result<(), String> {
    if !dwarf_named_entry(text, "base_type", "u64", false) {
        return Err("native debug bundle lacks the resolved u64 base type".into());
    }
    for source in sources {
        // Parent compile-unit attributes bind each required local and type to its fixture source.
        let entries = text
            .split("DW_TAG_compile_unit")
            .filter(|unit| {
                unit.split("DW_TAG_").next().is_some_and(|header| {
                    header
                        .lines()
                        .any(|line| line.contains("DW_AT_name") && line.contains(source))
                })
            })
            .collect::<Vec<_>>()
            .join("\n");
        let locals = if source == "src/main.rs" {
            dwarf_named_entry(&entries, "variable", "total", true)
        } else {
            dwarf_named_entry(&entries, "formal_parameter", "seed", true)
                && dwarf_named_entry(&entries, "variable", "v", true)
        };
        if !locals {
            return Err(format!(
                "native debug bundle lacks full source/type/local-variable evidence for {source}"
            ));
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn native_debug_proof(
    binary: &Path,
    profile: &Path,
    project: &Path,
    logs: &Path,
) -> Result<NativeDebugProof, String> {
    let mut bundles = Vec::new();
    for entry in fs::read_dir(profile.join("deps")).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name
            .strip_prefix("app-")
            .and_then(|name| name.strip_suffix(".dSYM"))
            .is_some_and(|hash| {
                hash.len() == 16 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
            && entry
                .file_type()
                .map_err(|error| error.to_string())?
                .is_dir()
        {
            bundles.push(entry.path());
        }
    }
    if bundles.len() != 1 {
        return Err("native fixture must own exactly one primary app debug bundle".into());
    }
    let bundle = bundles.pop().unwrap();
    let mut files = BTreeMap::new();
    // Hash the complete regular-file bundle, including auxiliary relocation metadata.
    let paths = source_paths(&bundle)?;
    let mut bytes = 0u64;
    for path in paths {
        let meta = fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
        bytes = bytes
            .checked_add(meta.len())
            .ok_or("native debug bundle size overflow")?;
        if !meta.is_file() || files.len() >= 64 || bytes > 512 * 1024 * 1024 {
            return Err("native debug bundle exceeds regular-file inventory bounds".into());
        }
        let relative = path
            .strip_prefix(&bundle)
            .map_err(|error| error.to_string())?
            .display()
            .to_string();
        files.insert(
            relative,
            NativeDebugFile {
                hash: crate::cache::hash_file(&path).map_err(|error| error.to_string())?,
                bytes: meta.len(),
                mode: sys::lstat(&path).map_err(|error| error.to_string())?.mode(),
            },
        );
    }
    let stem = bundle
        .file_stem()
        .ok_or("native debug bundle has no stem")?;
    let dwarf = bundle.join("Contents/Resources/DWARF").join(stem);
    if !files
        .get(&format!(
            "Contents/Resources/DWARF/{}",
            stem.to_string_lossy()
        ))
        .is_some_and(|file| file.bytes > 0)
        || !files.contains_key("Contents/Info.plist")
    {
        return Err("native debug bundle lacks its DWARF payload or Info.plist".into());
    }
    fs::create_dir_all(logs).map_err(|error| error.to_string())?;
    let inspections = [
        ("verify", vec!["--verify", "--quiet"], vec![dwarf.as_path()]),
        (
            "entries",
            vec![
                "--debug-info",
                "--regex",
                "--name=^(total|seed|v|u64)$",
                "--show-parents",
            ],
            vec![dwarf.as_path()],
        ),
        ("uuid", vec!["--uuid"], vec![binary, dwarf.as_path()]),
    ];
    let mut commands = Vec::new();
    for (name, args, paths) in inspections {
        let mut command = Command::new("/usr/bin/dwarfdump");
        command
            .args(args)
            .args(paths)
            .stdin(Stdio::null())
            .stdout(
                fs::File::create(logs.join(format!("{name}.stdout")))
                    .map_err(|error| error.to_string())?,
            )
            .stderr(
                fs::File::create(logs.join(format!("{name}.stderr")))
                    .map_err(|error| error.to_string())?,
            );
        commands.push((commands.len(), command));
    }
    // Full native debug inspection runs outside every foreground and settled build clock.
    let foreground = run_foregrounds(commands, Duration::from_secs(10))?;
    if let Some(error) = foreground.failure {
        return Err(format!(
            "native debug verification failed in {}: {error}",
            logs.display()
        ));
    }
    let read = |name: &str| -> Result<String, String> {
        let mut bytes = Vec::new();
        fs::File::open(logs.join(name))
            .map_err(|error| error.to_string())?
            .take(32 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
        if bytes.len() > 32 * 1024 * 1024 {
            return Err("native debug inspection output exceeds 32 MiB".into());
        }
        String::from_utf8(bytes).map_err(|error| error.to_string())
    };
    let uuids = read("uuid.stdout")?;
    let uuids = uuids
        .lines()
        .filter_map(|line| {
            line.strip_prefix("UUID: ")
                .and_then(|line| line.split_whitespace().next())
        })
        .collect::<Vec<_>>();
    if uuids.len() != 2
        || uuids[0] != uuids[1]
        || uuids[0].len() != 36
        || !uuids[0]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
    {
        return Err("native executable and full debug bundle UUIDs do not match".into());
    }
    let mut sources = vec!["src/main.rs".into()];
    for entry in fs::read_dir(project.join("libs")).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name
            .strip_prefix("lib")
            .is_some_and(|index| index.parse::<usize>().is_ok())
        {
            sources.push(format!("libs/{name}/src/lib.rs"));
        }
    }
    sources.sort();
    verify_dwarf_sources(&read("entries.stdout")?, &sources)?;
    Ok(NativeDebugProof {
        bundle,
        files,
        executable_mode: sys::lstat(binary)
            .map_err(|error| error.to_string())?
            .mode(),
        uuid: uuids[0].into(),
        sources_with_types_and_locals: sources,
        inspection_logs: logs.into(),
    })
}

#[cfg(target_os = "macos")]
fn restore_native_debug(
    ctx: &Ctx,
    project: &Path,
    targets: &[PathBuf; 2],
    role: usize,
    original: &Role,
    expected: &mut BTreeMap<usize, BinaryBaseline>,
    output: &mut Option<Vec<u8>>,
) -> Result<serde_json::Value, String> {
    let archive = ctx
        .home
        .with_file_name("retained")
        .join(format!("native-debug-restore-{}", ROLES[role]));
    fs::create_dir_all(
        archive
            .parent()
            .ok_or("native restore archive has no parent")?,
    )
    .map_err(|error| error.to_string())?;
    // Removing the whole active profile by rename forces actual cache recovery, preserving the original proof.
    sys::rename(&targets[role].join(ROLES[role]), &archive).map_err(|error| error.to_string())?;
    let restored = group(
        ctx,
        project,
        targets,
        ConcurrentProfile::Dev,
        &[role],
        true,
        "native-debug-restore",
        0,
        None,
        expected,
        output,
    )?;
    let result = &restored.roles[0];
    let original_debug = original
        .native_debug
        .as_ref()
        .ok_or("missing original dev debug proof")?;
    let restored_debug = result
        .native_debug
        .as_ref()
        .ok_or("missing restored dev debug proof")?;
    let hits = restored
        .cache_events
        .iter()
        .filter(|event| event["result"] == "hit")
        .count();
    let misses = restored
        .cache_events
        .iter()
        .filter(|event| event["result"] == "miss")
        .count();
    if result.rustc_compilation_counts.cargo_units != 0
        || result.rustc_compilation_counts.unknown != 0
        || hits == 0
        || misses != 0
        || original.binary_hash != result.binary_hash
        || original.runtime_stdout != result.runtime_stdout
        || original_debug.files != restored_debug.files
        || original_debug.executable_mode != restored_debug.executable_mode
        || original_debug.uuid != restored_debug.uuid
    {
        return Err(format!(
            "native {} clean cache restore did not preserve executable, runtime and complete debug bundle without declared-unit or unknown compilations",
            ROLES[role]
        ));
    }
    Ok(
        serde_json::json!({"phase": "untimed native debug clean cache restore", "profile": ROLES[role],
        "original": original.native_debug, "restored": result.native_debug,
        "original_binary_hash": original.binary_hash, "restored_binary_hash": result.binary_hash,
        "original_runtime_stdout_hash": original.runtime_stdout_hash, "restored_runtime_stdout_hash": result.runtime_stdout_hash,
        "rustc_compilation_counts": result.rustc_compilation_counts,
        "rustc_invocations": result.rustc_invocations, "rustc_query_invocations": result.rustc_query_invocations,
        "cache_hits": hits, "cache_misses": misses, "command": result.command, "archive": archive}),
    )
}

#[allow(clippy::too_many_arguments)]
fn group(
    ctx: &Ctx,
    project: &Path,
    targets: &[PathBuf; 2],
    profile: ConcurrentProfile,
    order: &[usize],
    frust: bool,
    phase: &'static str,
    repetition: usize,
    cold_start: Option<serde_json::Value>,
    expected: &mut BTreeMap<usize, BinaryBaseline>,
    expected_output: &mut Option<Vec<u8>>,
) -> Result<Group, String> {
    let _ = profile;
    let tool = if frust { "frust" } else { "cargo" };
    let audit = ctx.audit.as_ref().ok_or("compiler audit unavailable")?;
    let directory = audit
        .logs
        .join(format!("{tool}-{phase}-{repetition}-{}", order[0]));
    fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    fs::create_dir_all(&ctx.home).map_err(|error| error.to_string())?;
    for target in targets {
        fs::create_dir_all(target).map_err(|error| error.to_string())?;
    }
    let dirs = Dirs::at(&ctx.home);
    let before = growth_ledger(&dirs)?;
    let mut commands = Vec::new();
    for &role in order {
        let logs = directory.join(ROLES[role]);
        fs::create_dir_all(&logs).map_err(|error| error.to_string())?;
        let mut cmd = build_command(
            ctx,
            frust,
            project,
            Some(&targets[role]),
            &["--offline", "--locked", "--profile", ROLES[role]],
            "off",
        );
        cmd.env("FRUST_BENCH_AUDIT", &logs)
            .env("FRUST_TARGET", "local")
            .env(
                "FRUST_STORE_SYNC",
                if ctx.store == StorePolicy::Sync {
                    "1"
                } else {
                    "0"
                },
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(
                fs::File::create(directory.join(format!("{}.stderr", ROLES[role])))
                    .map_err(|error| error.to_string())?,
            );
        commands.push((role, cmd));
    }
    let load_before = load_average();
    let foreground = run_foregrounds(commands, Duration::from_secs(120));
    // One ledger interval and one drain belong to the shared group, never to overlapping roles.
    let drained = drain_deferred_stores(&ctx.home, Duration::from_secs(120));
    let settled = Instant::now();
    let foreground = foreground?;
    drained?;
    if let Some(error) = foreground.failure {
        return Err(error);
    }
    let started = foreground.started;
    let exited = foreground.exited;
    let spawned = foreground.spawned;
    let exits = foreground.exits;
    let after = growth_ledger(&dirs)?;
    let mut cache_events = Vec::new();
    for line in growth_ledger_added(&before, &after)?
        .lines()
        .filter(|line| !line.trim().is_empty())
    {
        let event: serde_json::Value = serde_json::from_str(line)
            .map_err(|error| format!("invalid concurrent ledger event: {error}"))?;
        if event["type"] == "cache" {
            cache_events.push(event);
        }
    }
    let mut roles = Vec::new();
    for ((role, command, pid, spawn_begin_ns, spawn_return_ns), (status, observed)) in
        spawned.into_iter().zip(exits)
    {
        let status = status.map_err(|error| error.to_string())?;
        let stderr = fs::read_to_string(directory.join(format!("{}.stderr", ROLES[role])))
            .map_err(|error| error.to_string())?;
        if !status.success() {
            return Err(format!(
                "concurrent {tool}/{} {phase} failed: {}",
                ROLES[role],
                failure_tail(&stderr)
            ));
        }
        let (calls, codes) = read_audit(&directory.join(ROLES[role]))?;
        let rustc_call_count = calls.len();
        let rustc_calls = calls.iter().zip(&codes).map(|(argv, code)| {
            serde_json::json!({"argv": argv, "exit_code": code, "query": rustc_query(argv), "compilation_class": (!rustc_query(argv)).then(|| classify_compilation(argv))})
        }).collect();
        let (queries, compiles): (Vec<_>, Vec<_>) =
            calls.into_iter().partition(|args| rustc_query(args));
        let counts = CompilationCounts::from_invocations(&compiles);
        let notices = stderr
            .lines()
            .filter(|line| line.trim_start().starts_with("Compiling "))
            .count();
        if phase == "no-op" && (rustc_call_count != 0 || notices != 0 || !cache_events.is_empty()) {
            return Err(format!(
                "unchanged concurrent {tool}/{} executed compiler/query, compile notice or cache event",
                ROLES[role]
            ));
        }
        if phase == "cold" && !frust && counts.cargo_units == 0 {
            return Err("cold Cargo role compiled no units".into());
        }
        let binary = app_binary(&targets[role].join(ROLES[role]));
        let binary_hash = crate::cache::hash_file(&binary).map_err(|error| error.to_string())?;
        if let Some(baseline) = expected.get(&role) {
            if baseline.hash != binary_hash {
                let bytes = fs::metadata(&binary).map(|metadata| metadata.len());
                let retained = audit
                    .logs
                    .with_file_name("binary-diagnostics")
                    .join(format!("mismatch-{}", ROLES[role]))
                    .join("app");
                let mut error = format!(
                    "same-path {tool}/{} binary differs from Cargo baseline: phase={phase}, repetition={repetition}; baseline tool={}, phase={}, repetition={}, hash={}, bytes={}, retained={}; actual hash={binary_hash}, bytes={}, source={}, retained={}; audit={}",
                    ROLES[role],
                    baseline.tool,
                    baseline.phase,
                    baseline.repetition,
                    baseline.hash,
                    baseline
                        .bytes
                        .map(|bytes| bytes.to_string())
                        .unwrap_or_else(|| "unavailable".into()),
                    baseline.binary.display(),
                    bytes
                        .as_ref()
                        .map(u64::to_string)
                        .unwrap_or_else(|error| format!("unavailable ({error})")),
                    binary.display(),
                    retained.display(),
                    directory.display()
                );
                let evidence = BinaryBaseline {
                    hash: binary_hash.clone(),
                    bytes: bytes.ok(),
                    tool,
                    phase,
                    repetition,
                    source_binary: binary.clone(),
                    binary: retained,
                    audit_directory: directory.clone(),
                    command: command.clone(),
                };
                if let Err(diagnostic) = retain_binary(&evidence) {
                    error.push_str(&format!("; mismatch retention failed: {diagnostic}"));
                }
                #[cfg(target_os = "macos")]
                if let Err(diagnostic) = native_binary_diagnostics(baseline, &evidence) {
                    error.push_str(&format!("; native diagnostics failed: {diagnostic}"));
                }
                return Err(error);
            }
        } else {
            let baseline = BinaryBaseline {
                hash: binary_hash.clone(),
                bytes: Some(
                    fs::metadata(&binary)
                        .map_err(|error| error.to_string())?
                        .len(),
                ),
                tool,
                phase,
                repetition,
                source_binary: binary.clone(),
                binary: audit
                    .logs
                    .with_file_name("binary-diagnostics")
                    .join(format!("baseline-{}", ROLES[role]))
                    .join("app"),
                audit_directory: directory.clone(),
                command: command.clone(),
            };
            retain_binary(&baseline)?;
            expected.insert(role, baseline);
        }
        let runtime = Command::new(&binary)
            .stdin(Stdio::null())
            .output()
            .map_err(|error| error.to_string())?;
        let output = runtime.stdout;
        let text = std::str::from_utf8(&output).map_err(|error| error.to_string())?;
        let lines: Vec<_> = text.lines().collect();
        if !runtime.status.success()
            || !runtime.stderr.is_empty()
            || lines.len() != 2
            || lines[0] != "frust-bench-edit 0"
            || lines[1].parse::<u64>().is_err()
        {
            return Err("concurrent fixture produced incorrect meaningful runtime output".into());
        }
        if let Some(expected) = expected_output.as_ref() {
            if expected != &output {
                return Err("concurrent runtime output differs across roles or arms".into());
            }
        } else {
            *expected_output = Some(output.clone());
        }
        #[cfg(target_os = "macos")]
        let native_debug = if profile == ConcurrentProfile::Dev {
            Some(native_debug_proof(
                &binary,
                &targets[role].join(ROLES[role]),
                project,
                &directory.join(format!("native-debug-{}", ROLES[role])),
            )?)
        } else {
            None
        };
        roles.push(Role {
            profile: ROLES[role].into(),
            target: targets[role].clone(),
            command,
            pid,
            spawn_begin_ns,
            spawn_return_ns,
            observed_exit_ns: observed.duration_since(started).as_nanos() as u64,
            exit_code: status.code(),
            cargo_compile_notices: notices,
            cargo_stderr: stderr,
            rustc_invocation_classes: compiles
                .iter()
                .map(|args| classify_compilation(args))
                .collect(),
            rustc_invocations: compiles,
            rustc_query_invocations: queries,
            rustc_exit_codes: codes,
            rustc_calls,
            rustc_compilation_counts: counts,
            rustc_call_count,
            binary_hash,
            runtime_stdout_hash: blake3::hash(&output).to_hex().to_string(),
            runtime_stdout: String::from_utf8(output).map_err(|error| error.to_string())?,
            #[cfg(target_os = "macos")]
            native_debug,
        });
    }
    let first_spawn = roles
        .iter()
        .map(|role| role.spawn_return_ns)
        .min()
        .unwrap_or(0);
    let last_spawn = roles
        .iter()
        .map(|role| role.spawn_return_ns)
        .max()
        .unwrap_or(0);
    // The complete managed union counts shared inodes once and reports failed inventories as unavailable.
    let roots = [
        targets[0].as_path(),
        targets[1].as_path(),
        ctx.home.as_path(),
    ];
    let managed_allocation = match compact::physical_size_checked(&roots) {
        Ok(total) => {
            serde_json::json!({"status": "available", "total_managed_bytes": total, "roots": roots, "method": "Checked allocated blocks of both complete target trees and shared home, counting unique device/inode entries once. Detected APFS clone sharing is estimated; unknown sharing is charged fully.", "error": null})
        }
        Err(error) => {
            serde_json::json!({"status": "unavailable", "total_managed_bytes": null, "roots": roots, "method": "Checked allocated blocks of both complete target trees and shared home, counting unique device/inode entries once. Detected APFS clone sharing is estimated; unknown sharing is charged fully.", "error": error.to_string()})
        }
    };
    Ok(Group {
        tool,
        phase,
        repetition,
        roles_start_order: order.iter().map(|&role| ROLES[role].into()).collect(),
        foreground_ns: exited.duration_since(started).as_nanos() as u64,
        store_drain_ns: settled.duration_since(exited).as_nanos() as u64,
        settled_ns: settled.duration_since(started).as_nanos() as u64,
        spawn_skew_ns: last_spawn - first_spawn,
        load_before,
        load_after: load_average(),
        cold_start,
        cache_events,
        roles,
        cross_role_reuse: Vec::new(),
        unit_multiplicity: None,
        managed_allocation,
    })
}

fn identities(role: &Role) -> Result<Units, String> {
    let prefix = role
        .target
        .join(&role.profile)
        .to_str()
        .ok_or("non-UTF-8 concurrent target")?
        .to_owned();
    let mut units = Units::new();
    for args in &role.rustc_invocations {
        if classify_compilation(args) != CompilationClass::CargoUnit {
            continue;
        }
        let normalized = normalize(args, &prefix)?;
        *units
            .entry(serde_json::to_string(&normalized).map_err(|error| error.to_string())?)
            .or_default() += 1;
    }
    Ok(units)
}

fn normalize(args: &[String], prefix: &str) -> Result<Vec<String>, String> {
    let mut previous = "";
    args.iter()
        .map(|arg| {
            let eligible = matches!(previous, "--out-dir" | "--extern" | "-L")
                || arg.starts_with("--out-dir=")
                || arg.starts_with("--extern=")
                || arg.starts_with("-Ldependency=")
                || previous == "-C" && arg.starts_with("incremental=")
                || arg.starts_with("-Cincremental=");
            previous = arg;
            if arg.contains(prefix) {
                if !eligible {
                    return Err(format!(
                        "unapproved concurrent profile path normalization: {arg}"
                    ));
                }
                // Only the exact target/profile prefix in declared output operands may vary across roles.
                for suffix in arg.split(prefix).skip(1) {
                    if !suffix.is_empty() && !suffix.starts_with(std::path::MAIN_SEPARATOR) {
                        return Err(format!(
                            "concurrent profile prefix has no path boundary: {arg}"
                        ));
                    }
                }
                Ok(arg.replace(prefix, "<PROFILE_OUTPUT>"))
            } else {
                Ok(arg.clone())
            }
        })
        .collect()
}

fn validate_reuse(cargo: &Group, frust: &mut Group) -> Result<(), String> {
    let baseline = cargo
        .roles
        .iter()
        .map(identities)
        .collect::<Result<Vec<_>, _>>()?;
    if baseline.len() != 2 || baseline[0] != baseline[1] || baseline[0].is_empty() {
        return Err("equivalent concurrent profiles produced different Cargo unit identities or multiplicities".into());
    }
    let actual = frust
        .roles
        .iter()
        .map(identities)
        .collect::<Result<Vec<_>, _>>()?;
    if actual.len() != 2 {
        return Err("concurrent frust group lacks both roles".into());
    }
    let mut total = Units::new();
    for units in &actual {
        for (key, count) in units {
            if *count > *baseline[0].get(key).unwrap_or(&0) {
                return Err("frust performed extra or mismatched actual unit compilations".into());
            }
            *total.entry(key.clone()).or_default() += count;
        }
    }
    let mut crate_counts = BTreeMap::new();
    for key in baseline[0].keys() {
        let args: Vec<String> = serde_json::from_str(key).map_err(|error| error.to_string())?;
        *crate_counts
            .entry(rustc_flag(&args, "--crate-name").unwrap_or("").to_owned())
            .or_insert(0usize) += 1;
    }
    for (key, count) in &baseline[0] {
        if *count != 1 || total.get(key) != Some(&1) {
            continue;
        }
        let args: Vec<String> = serde_json::from_str(key).map_err(|error| error.to_string())?;
        let name = rustc_flag(&args, "--crate-name").ok_or("Cargo unit lacks crate name")?;
        if crate_counts[name] != 1 {
            continue;
        }
        let hit = frust
            .cache_events
            .iter()
            .any(|event| event["crate"] == name && event["result"] == "hit");
        let miss = frust.cache_events.iter().any(|event| {
            event["crate"] == name
                && event["result"] == "miss"
                && event["bytes"].as_u64().unwrap_or(0) > 0
        });
        if hit && miss {
            frust.cross_role_reuse.push(serde_json::json!({"crate": name, "unit_identity": args, "compiles_by_role": { &frust.roles[0].profile: actual[0].get(key).copied().unwrap_or(0), &frust.roles[1].profile: actual[1].get(key).copied().unwrap_or(0)}}));
        }
    }
    if frust.cross_role_reuse.is_empty() {
        return Err("no positive-byte miss plus peer-hit unit identity compiled once across concurrent roles".into());
    }
    frust.unit_multiplicity = Some(
        serde_json::json!({"cargo_per_role": baseline[0].values().sum::<usize>(), "cargo_total": baseline[0].values().sum::<usize>() * 2, "frust_total": total.values().sum::<usize>(), "baseline": baseline[0], "frust": total}),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_profile_selection_and_release_overrides_are_explicit() {
        assert_eq!(
            ConcurrentProfile::parse(&[]).unwrap(),
            ConcurrentProfile::Dev
        );
        for (name, profile, lto, units, panic) in [
            ("dev", ConcurrentProfile::Dev, "false", 256, "unwind"),
            ("release", ConcurrentProfile::Release, "false", 16, "unwind"),
            ("thin", ConcurrentProfile::Thin, "thin", 16, "unwind"),
            ("fat", ConcurrentProfile::Fat, "fat", 1, "abort"),
        ] {
            let flag = format!("--concurrent-profile={name}");
            assert_eq!(
                ConcurrentProfile::parse(std::slice::from_ref(&flag)).unwrap(),
                profile
            );
            assert!(ConcurrentProfile::parse(&[flag.clone(), flag]).is_err());
            let settings = profile.effective();
            assert_eq!(
                (settings.lto, settings.codegen_units, settings.panic),
                (lto, units, panic)
            );
            let declarations = profile.declarations(profile == ConcurrentProfile::Dev);
            for role in ROLES {
                assert!(declarations.contains(&format!(
                    "[profile.{role}]\ninherits = \"{}\"",
                    settings.inherits
                )));
            }
            if profile != ConcurrentProfile::Dev {
                let encoded = if lto == "false" {
                    lto.to_string()
                } else {
                    format!("\"{lto}\"")
                };
                for line in [
                    format!("lto = {encoded}"),
                    format!("codegen-units = {units}"),
                    format!("panic = \"{panic}\""),
                    "incremental = false".into(),
                    "debug = false".into(),
                ] {
                    assert_eq!(
                        declarations.lines().filter(|value| *value == line).count(),
                        2,
                        "{line}"
                    );
                }
            }
        }
        for flag in [
            "--concurrent-profile",
            "--concurrent-profile=",
            "--concurrent-profile=debug",
        ] {
            assert!(ConcurrentProfile::parse(&[flag.into()]).is_err());
        }
    }

    #[test]
    fn concurrent_incremental_override_is_release_only_and_explicit() {
        assert!(ConcurrentProfile::Dev.incremental(&[]).unwrap());
        for profile in [
            ConcurrentProfile::Release,
            ConcurrentProfile::Thin,
            ConcurrentProfile::Fat,
        ] {
            assert!(!profile.incremental(&[]).unwrap());
            for (value, enabled) in [("on", true), ("off", false)] {
                let flag = format!("--concurrent-incremental={value}");
                assert_eq!(
                    profile.incremental(std::slice::from_ref(&flag)).unwrap(),
                    enabled
                );
                assert!(profile.incremental(&[flag.clone(), flag]).is_err());
                let declarations = profile.declarations(enabled);
                let assignment = format!("incremental = {enabled}");
                assert_eq!(
                    declarations
                        .lines()
                        .filter(|line| *line == assignment)
                        .count(),
                    2
                );
                assert_eq!(
                    declarations
                        .lines()
                        .filter(|line| *line == "debug = false")
                        .count(),
                    2
                );
            }
        }
        for value in ["on", "off"] {
            assert!(
                ConcurrentProfile::Dev
                    .incremental(&[format!("--concurrent-incremental={value}")])
                    .is_err()
            );
        }
        for flag in [
            "--concurrent-incremental",
            "--concurrent-incremental=",
            "--concurrent-incremental=true",
        ] {
            assert!(
                ConcurrentProfile::Release
                    .incremental(&[flag.into()])
                    .is_err()
            );
        }
    }

    #[test]
    fn concurrent_normalization_accepts_only_known_output_operands_and_boundaries() {
        let target = std::env::temp_dir().join("target");
        let profile = target.join("coldone");
        let prefix = profile.display().to_string();
        let args = vec![
            "--out-dir".into(),
            profile.join("deps").display().to_string(),
            format!(
                "--extern=x={}",
                profile.join("deps").join("x.rlib").display()
            ),
            format!("-Ldependency={}", profile.join("deps").display()),
            "-C".into(),
            format!("incremental={}", profile.join("incremental").display()),
        ];
        let normalized = normalize(&args, &prefix).unwrap();
        assert!(
            normalized
                .iter()
                .any(|arg| arg.contains("<PROFILE_OUTPUT>"))
        );
        for args in [
            vec![profile.join("source.rs").display().to_string()],
            vec!["--cfg".into(), profile.join("x").display().to_string()],
            vec![format!(
                "--out-dir={}",
                target.join("coldone-extra").join("deps").display()
            )],
        ] {
            assert!(normalize(&args, &prefix).is_err());
        }
    }

    #[test]
    fn concurrent_probe_classifier_accepts_exact_profiles_only() {
        for (profile, expected) in [
            ("coldone", CompilationClass::BuildScriptProbe),
            ("coldtwo", CompilationClass::BuildScriptProbe),
            ("coldthree", CompilationClass::Unknown),
        ] {
            let args = vec![
                "--out-dir".into(),
                std::env::temp_dir()
                    .join("t")
                    .join(profile)
                    .join("build")
                    .join("crate-12345678")
                    .join("out")
                    .join("probe")
                    .display()
                    .to_string(),
                "--crate-type=lib".into(),
                "--emit=metadata".into(),
            ];
            assert_eq!(classify_compilation(&args), expected);
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn fake_command(script: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn concurrent_blocking_exit_clocks_precede_group_completion() {
        let foreground = run_foregrounds(
            vec![
                (0, fake_command("sleep 0.03")),
                (1, fake_command("sleep 0.15")),
            ],
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(foreground.failure.is_none());
        assert_eq!(foreground.exits.len(), 2);
        assert!(
            foreground
                .exits
                .iter()
                .all(|(status, _)| status.as_ref().unwrap().success())
        );
        assert!(foreground.exits[0].1 < foreground.exits[1].1);
        assert_eq!(foreground.exited, foreground.exits[1].1);
        assert!(
            foreground.exits[0].1.duration_since(foreground.started)
                < foreground.exited.duration_since(foreground.started)
        );
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn concurrent_watchdog_cancels_and_reaps_stalled_groups() {
        let started = Instant::now();
        let foreground = run_foregrounds(
            vec![
                (0, fake_command("sleep 30 & wait")),
                (1, fake_command("exec sleep 30")),
            ],
            Duration::from_millis(50),
        )
        .unwrap();
        assert!(foreground.failure.as_ref().unwrap().contains("watchdog"));
        assert_eq!(foreground.exits.len(), 2);
        assert!(
            foreground
                .exits
                .iter()
                .all(|(status, _)| crate::sys::was_force_killed(status.as_ref().unwrap()))
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn concurrent_second_spawn_failure_immediately_cancels_and_reaps_first() {
        let started = Instant::now();
        let foreground = run_foregrounds(
            vec![
                (0, fake_command("exec sleep 30")),
                (1, Command::new("/frust-concurrent-missing-foreground-bin")),
            ],
            Duration::from_secs(30),
        )
        .unwrap();
        assert!(
            foreground
                .failure
                .as_ref()
                .unwrap()
                .contains("spawn failed")
        );
        assert_eq!(foreground.exits.len(), 1);
        assert!(crate::sys::was_force_killed(
            foreground.exits[0].0.as_ref().unwrap()
        ));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn assert_failed_leader_descendant_cancelled(peer: &str) {
        let marker = std::env::temp_dir().join(format!(
            "frust-descendant-{}-{}",
            std::process::id(),
            crate::cache::hashing::now_ns()
        ));
        // The descendant expires after five seconds even if a regression fails the early-termination assertion.
        let script = format!(
            r#"sleep 5 & child=$!; printf '%s %s\n' "$$" "$child" > {}; exit 1"#,
            shell_quote(&marker).unwrap()
        );
        let started = Instant::now();
        let foreground = run_foregrounds(
            vec![(0, fake_command(&script)), (1, fake_command(peer))],
            Duration::from_millis(250),
        )
        .unwrap();
        assert!(foreground.failure.is_some());
        assert_eq!(foreground.exits[0].0.as_ref().unwrap().code(), Some(1));
        let text = fs::read_to_string(&marker).unwrap();
        fs::remove_file(&marker).unwrap();
        let child = text.split_whitespace().nth(1).unwrap();
        let deadline = started + Duration::from_secs(2);
        loop {
            let status = Command::new("ps")
                .args(["-o", "stat=", "-p", child])
                .output()
                .unwrap();
            let state = String::from_utf8(status.stdout).unwrap();
            if state.trim().is_empty() || state.trim().starts_with('Z') {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "same-group descendant still running: {state}"
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn concurrent_exited_leader_keeps_descendant_group_pinned_until_cancellation() {
        assert_failed_leader_descendant_cancelled("exec sleep 5");
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn concurrent_quick_failed_group_cancels_descendants_before_reaping_leaders() {
        assert_failed_leader_descendant_cancelled("true");
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn injected_observe_error(_pid: u32) -> Result<Instant, ObserveError> {
        Err(ObserveError {
            message: "injected observation failure".into(),
            ownership_lost: false,
        })
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn injected_cancel_error(_pid: u32) -> Result<(), String> {
        Err("injected cancellation failure".into())
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn concurrent_observe_and_cancel_errors_return_with_bounded_unsettled_cleanup() {
        let started = Instant::now();
        let result = run_foregrounds_with(
            vec![(0, fake_command("exec sleep 3"))],
            Duration::from_secs(5),
            Duration::from_millis(50),
            injected_observe_error,
            injected_cancel_error,
        );
        let error = result
            .err()
            .expect("injected lifecycle failures must be reported");
        assert!(error.contains("injected observation failure"));
        assert!(error.contains("injected cancellation failure"));
        assert!(error.contains("unsettled cleanup"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
