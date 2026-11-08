//! Real-repository benchmark: paired cargo and frust builds of an external checkout.

use super::*;
use crate::cache::EntryMeta;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashSet};

const MAX_EDIT_FILES: usize = 8;
const STORE_TOP_JSON: usize = 20;
const STORE_TOP_MARKDOWN: usize = 10;
// The allocation helper accepts 64 unions per call: one for the targets alone plus one per probed file.
const FILES_PER_QUERY: usize = 63;
const DEFAULT_EDITS: usize = 20;
const DEFAULT_REPO_RUNS: usize = 3;
const NOOP_REPEATS: usize = 3;
const QUIESCE_TIMEOUT: Duration = Duration::from_secs(60);
const REPORT_SCHEMA_VERSION: u32 = 2;

const GATE_NOOP_SLACK_MS: f64 = 15.0;
const GATE_NOOP_SLACK_WINDOWS_MS: f64 = 25.0;
const GATE_NOOP_RATIO: f64 = 1.25;
const GATE_EDIT_RATIO: f64 = 1.05;
const GATE_EDIT_SLACK_MS: f64 = 25.0;
const GATE_EDIT_P90_RATIO: f64 = 1.15;
const GATE_EDIT_P90_SLACK_MS: f64 = 40.0;
const GATE_COLD_RATIO: f64 = 1.05;
const GATE_COLD_SLACK_MS: f64 = 150.0;
const GATE_CLEAN_RATIO: f64 = 0.5;
const GATE_CLEAN_SLACK_MS: f64 = 100.0;

const CLASSES: [&str; 5] = ["registry-git", "path", "unknown", "build-script", "proc-macro"];
const EXTENSIONS: [&str; 6] = ["rlib", "rmeta", "dylib", "bin", "d", "other"];
const TIMING_ROWS: [(&str, &str); 4] = [("cold", "cold"), ("clean rebuild", "clean_rebuild"), ("no-op", "noop"), ("first edit", "edit_first")];
const TIMING_HEADER: [&str; 19] = [
    "row", "builds", "units", "total ms", "rustc ms", "overhead ms", "prekey", "lookup", "keywait", "restore",
    "freeze", "spawn", "inline store", "drain ms", "extern files", "extern hashed", "extern memo", "native hashed",
    "store modes",
];
const MISS_TOP_JSON: usize = 20;
const MISS_TOP_MARKDOWN: usize = 10;

const TABLE_HEADER: [&str; 8] = ["tool", "cold", "clean rebuild", "no-op", "first edit", "warm edit", "edit p90", "disk"];
const EDIT_NOTE: &str = "First edit is the first touch of a crate after a restore (no incremental session); warm edit and p90 cover later edits. The ratio row is the median of per-pair frust/cargo ratios.";
const FRUST_DIRS: [&str; 5] = ["store", "targets", "manifests", "tmp", "other"];

#[derive(Debug, PartialEq)]
struct RepoOptions {
    repo: PathBuf,
    edits: usize,
    runs: usize,
    package: Option<String>,
    json: bool,
    markdown: bool,
    out: Option<PathBuf>,
    gate: bool,
    features: Option<String>,
    disk_slack: f64,
}

fn positive(name: &str, value: &str) -> Result<usize, String> {
    value.trim().parse::<usize>().ok().filter(|n| *n >= 1)
        .ok_or_else(|| format!("{name} needs a positive integer, got `{value}`"))
}

impl RepoOptions {
    fn parse(flags: &[String]) -> Result<Self, String> {
        let mut repo = None;
        let mut package = None;
        let mut out = None;
        let mut features = None;
        let mut disk_slack = 0.0;
        let (mut edits, mut runs) = (DEFAULT_EDITS, DEFAULT_REPO_RUNS);
        let (mut json, mut markdown, mut gate) = (false, false, false);
        let mut index = 0;
        while index < flags.len() {
            let flag = flags[index].as_str();
            let (name, inline) = match flag.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (flag, None),
            };
            match name {
                "--json" if inline.is_none() => json = true,
                "--markdown" if inline.is_none() => markdown = true,
                "--gate" if inline.is_none() => gate = true,
                "--repo" | "--edits" | "--runs" | "--package" | "--out" | "--features" | "--disk-slack" => {
                    let value = match inline {
                        Some(value) => value,
                        None => {
                            index += 1;
                            flags.get(index).map(String::as_str).ok_or_else(|| format!("{name} needs a value"))?
                        }
                    };
                    if value.is_empty() {
                        return Err(format!("{name} needs a value"));
                    }
                    match name {
                        "--repo" => repo = Some(PathBuf::from(value)),
                        "--edits" => edits = positive(name, value)?,
                        "--runs" => runs = positive(name, value)?,
                        "--package" => package = Some(value.to_string()),
                        "--features" => features = Some(value.to_string()),
                        "--disk-slack" => {
                            disk_slack = value.trim().parse::<f64>().ok().filter(|p| p.is_finite() && *p >= 0.0)
                                .ok_or_else(|| format!("{name} needs a non-negative percentage, got `{value}`"))?;
                        }
                        _ => out = Some(PathBuf::from(value)),
                    }
                }
                _ => return Err(format!("`{flag}` is not supported together with --repo")),
            }
            index += 1;
        }
        let repo = repo.ok_or("--repo PATH is required")?;
        Ok(Self { repo, edits, runs, package, json, markdown, out, gate, features, disk_slack })
    }
}

#[derive(Debug, Default, Clone)]
struct Samples {
    cold: Vec<f64>,
    clean: Vec<f64>,
    noop: Vec<f64>,
    first: Vec<f64>,
    warm: Vec<f64>,
}

#[derive(Debug, Serialize)]
struct Series {
    raw_ms: Vec<f64>,
    median_ms: Option<f64>,
    p90_ms: Option<f64>,
}

impl Series {
    fn new(values: &[f64]) -> Self {
        Self { raw_ms: values.to_vec(), median_ms: median_of(values), p90_ms: percentile(values, 90.0) }
    }
}

#[derive(Debug, Serialize)]
struct ToolReport {
    cold: Series,
    clean_rebuild: Series,
    noop: Series,
    edit_first: Series,
    edit_warm: Series,
    disk_bytes: u64,
}

impl Samples {
    fn report(&self, disk_bytes: u64) -> ToolReport {
        ToolReport {
            cold: Series::new(&self.cold),
            clean_rebuild: Series::new(&self.clean),
            noop: Series::new(&self.noop),
            edit_first: Series::new(&self.first),
            edit_warm: Series::new(&self.warm),
            disk_bytes,
        }
    }
}

#[derive(Debug, Serialize)]
struct DiskReport {
    cargo_bytes: u64,
    // The gate uses `frust_bytes`, measured after the maintenance pass the product runs on its own; cargo has no equivalent.
    frust_bytes: u64,
    frust_bytes_before_maintenance: u64,
    maintenance_ms: u64,
    slack_pct: f64,
    ok: bool,
    message: Option<String>,
}

#[derive(Debug, Serialize)]
struct Ratios {
    cold: Option<f64>,
    clean_rebuild: Option<f64>,
    noop: Option<f64>,
    edit_first: Option<f64>,
    edit_warm: Option<f64>,
    edit_p90: Option<f64>,
    disk: Option<f64>,
}

fn paired_ratios(cargo: &ToolReport, frust: &ToolReport) -> Ratios {
    let pair = |c: &Series, f: &Series| median_ratio(&c.raw_ms, &f.raw_ms);
    let p90 = match (cargo.edit_warm.p90_ms, frust.edit_warm.p90_ms) {
        (Some(c), Some(f)) if c > 0.0 => Some(f / c),
        _ => None,
    };
    Ratios {
        cold: pair(&cargo.cold, &frust.cold),
        clean_rebuild: pair(&cargo.clean_rebuild, &frust.clean_rebuild),
        noop: pair(&cargo.noop, &frust.noop),
        edit_first: pair(&cargo.edit_first, &frust.edit_first),
        edit_warm: pair(&cargo.edit_warm, &frust.edit_warm),
        edit_p90: p90,
        disk: (cargo.disk_bytes > 0).then(|| frust.disk_bytes as f64 / cargo.disk_bytes as f64),
    }
}

#[derive(Debug, PartialEq, Serialize)]
struct DirSize {
    name: String,
    physical_bytes: u64,
    logical_bytes: u64,
}

#[derive(Debug, PartialEq, Serialize)]
struct TreeSize {
    name: String,
    files: u64,
    directories: u64,
    physical_bytes: u64,
}

#[derive(Debug, PartialEq, Serialize)]
struct DiskBreakdown {
    cargo_physical_bytes: u64,
    cargo_logical_bytes: u64,
    frust_physical_bytes: u64,
    frust_logical_bytes: u64,
    frust_dirs: Vec<DirSize>,
    // Top-level trees under the cache home that the named directories do not cover, each measured alone.
    other_trees: Vec<TreeSize>,
    frust_physical_bytes_before_maintenance: Option<u64>,
    targets_only_physical_bytes: u64,
    cache_entries: usize,
    store_shared_with_targets_bytes: u64,
    store_shared_share: Option<f64>,
}

// Cumulative holds union sizes of store, +targets, +manifests, +tmp, +other, then targets alone.
fn build_breakdown(cargo_physical: u64, cargo_logical: u64, cumulative: [u64; 6], logical: [u64; 5], cache_entries: usize) -> DiskBreakdown {
    let frust_dirs = FRUST_DIRS.iter().enumerate().map(|(i, name)| DirSize {
        name: name.to_string(),
        physical_bytes: cumulative[i].saturating_sub(if i == 0 { 0 } else { cumulative[i - 1] }),
        logical_bytes: logical[i],
    }).collect();
    let shared = (cumulative[0] + cumulative[5]).saturating_sub(cumulative[1]);
    DiskBreakdown {
        cargo_physical_bytes: cargo_physical,
        cargo_logical_bytes: cargo_logical,
        frust_physical_bytes: cumulative[4],
        frust_logical_bytes: logical.iter().sum(),
        frust_dirs,
        other_trees: Vec::new(),
        frust_physical_bytes_before_maintenance: None,
        targets_only_physical_bytes: cumulative[5],
        cache_entries,
        store_shared_with_targets_bytes: shared,
        store_shared_share: (cumulative[0] > 0).then(|| shared as f64 / cumulative[0] as f64),
    }
}

fn logical_size(path: &Path, seen: &mut HashSet<(u64, u64)>) -> u64 {
    let Ok(meta) = sys::lstat(path) else { return 0 };
    if meta.is_dir() {
        let Ok(read) = fs::read_dir(path) else { return 0 };
        return read.flatten().map(|entry| logical_size(&entry.path(), seen)).sum();
    }
    if meta.nlink() > 1 && !seen.insert((meta.dev(), meta.ino())) {
        return 0;
    }
    fs::symlink_metadata(path).map(|m| m.len()).unwrap_or(0)
}

// Counts files and directories below a path without following symlinks; the root itself counts as a directory or a file.
fn count_tree(path: &Path) -> (u64, u64) {
    let Ok(meta) = fs::symlink_metadata(path) else { return (0, 0) };
    if !meta.is_dir() {
        return (1, 0);
    }
    let Ok(read) = fs::read_dir(path) else { return (0, 1) };
    read.flatten().fold((0, 1), |(files, dirs), entry| {
        let (f, d) = count_tree(&entry.path());
        (files + f, dirs + d)
    })
}

fn measure_other_trees(paths: &[PathBuf]) -> Result<Vec<TreeSize>, String> {
    let singles: Vec<[&Path; 1]> = paths.iter().map(|path| [path.as_path()]).collect();
    let sets: Vec<&[&Path]> = singles.iter().map(|one| &one[..]).collect();
    let sizes = if sets.is_empty() { Vec::new() } else { compact::physical_sizes_checked(&sets).map_err(|e| e.to_string())? };
    let mut trees: Vec<TreeSize> = paths.iter().zip(sizes).map(|(path, physical_bytes)| {
        let (files, directories) = count_tree(path);
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        TreeSize { name, files, directories, physical_bytes }
    }).collect();
    trees.sort_by(|a, b| b.physical_bytes.cmp(&a.physical_bytes).then_with(|| a.name.cmp(&b.name)));
    Ok(trees)
}

fn count_cache_entries(store: &Path) -> usize {
    crate::cache::keyed_items(store).into_iter().filter(|entry| entry.join("meta.json").is_file()).count()
}

fn measure_disk_breakdown(dirs: &Dirs, cargo_target: &Path, cargo_physical: u64) -> Result<DiskBreakdown, String> {
    let named = [dirs.store(), dirs.targets(), dirs.manifests(), dirs.tmp()];
    let other: Vec<PathBuf> = fs::read_dir(dirs.root()).map_err(|e| format!("{}: {e}", dirs.root().display()))?
        .flatten().map(|entry| entry.path()).filter(|path| !named.contains(path)).collect();
    let [store, targets, manifests, tmp] = named;
    let groups: Vec<Vec<PathBuf>> = vec![vec![store.clone()], vec![targets], vec![manifests], vec![tmp], other]
        .into_iter().map(|paths| paths.into_iter().filter(|p| fs::symlink_metadata(p).is_ok()).collect()).collect();
    let flat: Vec<&Path> = groups.iter().flatten().map(PathBuf::as_path).collect();
    let mut end = 0;
    let mut sets: Vec<&[&Path]> = Vec::new();
    for group in &groups {
        end += group.len();
        sets.push(&flat[..end]);
    }
    let targets_alone: Vec<&Path> = groups[1].iter().map(PathBuf::as_path).collect();
    sets.push(&targets_alone);
    let sizes = compact::physical_sizes_checked(&sets).map_err(|e| e.to_string())?;
    let cumulative: [u64; 6] = sizes.try_into().map_err(|_| "unexpected allocation result".to_string())?;
    let mut seen = HashSet::new();
    let mut logical = [0u64; 5];
    for (slot, group) in logical.iter_mut().zip(&groups) {
        *slot = group.iter().map(|path| logical_size(path, &mut seen)).sum();
    }
    let cargo_logical = logical_size(cargo_target, &mut HashSet::new());
    let mut breakdown = build_breakdown(cargo_physical, cargo_logical, cumulative, logical, count_cache_entries(&store));
    breakdown.other_trees = measure_other_trees(&groups[4])?;
    Ok(breakdown)
}

#[derive(Debug, Clone, PartialEq)]
struct StoreFile {
    key: String,
    krate: String,
    version: String,
    source: Option<String>,
    name: String,
    bytes: u64,
    encoding: String,
    // The store file is its inode's only name, so no target can share it.
    lone: bool,
}

#[derive(Debug, PartialEq, Serialize)]
struct GroupBytes {
    name: String,
    bytes: u64,
    files: usize,
}

#[derive(Debug, PartialEq, Serialize)]
struct EntryUse {
    key: String,
    crate_name: String,
    version: String,
    source: Option<String>,
    kinds: Vec<String>,
    files: usize,
    bytes: u64,
    // Encodings of the store-only files: `linked` means the restore or a later step failed to share them, anything else that the store step copied.
    encodings: Vec<String>,
    lone_files: usize,
    // How the last restore of this entry placed its files, from the timing records.
    last_restore: Option<String>,
}

#[derive(Debug, PartialEq, Serialize)]
struct StoreOnly {
    status: String,
    reason: Option<String>,
    total_bytes: u64,
    entries: usize,
    unreadable_entries: usize,
    files: usize,
    top_entries: Vec<EntryUse>,
    by_class: Vec<GroupBytes>,
    by_extension: Vec<GroupBytes>,
}

impl StoreOnly {
    fn unsupported(reason: String) -> Self {
        Self {
            status: "unsupported".to_string(),
            reason: Some(reason),
            total_bytes: 0,
            entries: 0,
            unreadable_entries: 0,
            files: 0,
            top_entries: Vec::new(),
            by_class: Vec::new(),
            by_extension: Vec::new(),
        }
    }
}

fn extension_group(name: &str) -> &'static str {
    let ext = Path::new(name).extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).unwrap_or_default();
    match ext.as_str() {
        "rlib" => "rlib",
        "rmeta" => "rmeta",
        "so" | "dylib" | "dll" => "dylib",
        "exe" | "bin" | "" => "bin",
        "d" => "d",
        _ => "other",
    }
}

// Host dylibs of an entry mark proc-macro crates; the name patterns catch the rest.
fn crate_class(krate: &str, source: Option<&str>, host_dylib: bool) -> &'static str {
    if krate.starts_with("build_script_") {
        "build-script"
    } else if host_dylib || krate.ends_with("_derive") || krate.ends_with("_macros") || krate.ends_with("_macro") {
        "proc-macro"
    } else {
        match source {
            Some("registry" | "git") => "registry-git",
            Some("path") => "path",
            _ => "unknown",
        }
    }
}

fn group_list(names: &[&str], totals: &BTreeMap<&str, (u64, usize)>) -> Vec<GroupBytes> {
    names.iter().map(|name| {
        let (bytes, files) = totals.get(name).copied().unwrap_or((0, 0));
        GroupBytes { name: name.to_string(), bytes, files }
    }).collect()
}

fn summarize_store_only(files: &[StoreFile], unreadable_entries: usize, top: usize, restores: &BTreeMap<String, String>) -> StoreOnly {
    let mut entries: BTreeMap<&str, Vec<&StoreFile>> = BTreeMap::new();
    for file in files {
        entries.entry(file.key.as_str()).or_default().push(file);
    }
    let mut classes: BTreeMap<&str, (u64, usize)> = BTreeMap::new();
    let mut extensions: BTreeMap<&str, (u64, usize)> = BTreeMap::new();
    let mut uses = Vec::new();
    for (key, group) in &entries {
        let first = group[0];
        let host_dylib = group.iter().any(|f| extension_group(&f.name) == "dylib");
        let class = crate_class(&first.krate, first.source.as_deref(), host_dylib);
        let bytes: u64 = group.iter().map(|f| f.bytes).sum();
        let total = classes.entry(class).or_default();
        *total = (total.0 + bytes, total.1 + group.len());
        let mut kinds: Vec<String> = Vec::new();
        let mut encodings: Vec<String> = Vec::new();
        for file in group {
            let ext = extension_group(&file.name);
            let total = extensions.entry(ext).or_default();
            *total = (total.0 + file.bytes, total.1 + 1);
            if !kinds.iter().any(|k| k == ext) {
                kinds.push(ext.to_string());
            }
            if file.bytes > 0 && !encodings.contains(&file.encoding) {
                encodings.push(file.encoding.clone());
            }
        }
        kinds.sort();
        encodings.sort();
        uses.push(EntryUse {
            key: key.to_string(),
            crate_name: first.krate.clone(),
            version: first.version.clone(),
            source: first.source.clone(),
            kinds,
            files: group.len(),
            bytes,
            encodings,
            lone_files: group.iter().filter(|f| f.lone).count(),
            last_restore: restores.get(*key).cloned(),
        });
    }
    uses.retain(|u| u.bytes > 0);
    uses.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.key.cmp(&b.key)));
    uses.truncate(top);
    StoreOnly {
        status: "ok".to_string(),
        reason: None,
        total_bytes: files.iter().map(|f| f.bytes).sum(),
        entries: entries.len(),
        unreadable_entries,
        files: files.len(),
        top_entries: uses,
        by_class: group_list(&CLASSES, &classes),
        by_extension: group_list(&EXTENSIONS, &extensions),
    }
}

// Returns the payload files of every readable entry with their paths, plus the count of entries whose record could not be read.
fn collect_store_files(store: &Path) -> (Vec<StoreFile>, Vec<PathBuf>, usize) {
    let (mut files, mut paths, mut unreadable) = (Vec::new(), Vec::new(), 0);
    let mut entry_dirs: Vec<PathBuf> = crate::cache::keyed_items(store).into_iter().filter(|path| path.is_dir()).collect();
    entry_dirs.sort();
    for dir in entry_dirs {
        let Some(key) = dir.file_name().map(|n| n.to_string_lossy().into_owned()) else { continue };
        let meta: Option<EntryMeta> = fs::read(dir.join("meta.json")).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok());
        let Some(meta) = meta else {
            unreadable += 1;
            continue;
        };
        for stored in &meta.files {
            let path = dir.join(&stored.stored);
            if fs::symlink_metadata(&path).is_err() {
                continue;
            }
            let encoding = serde_json::to_value(stored.encoding).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
            files.push(StoreFile {
                key: key.clone(),
                krate: meta.crate_name.clone(),
                version: meta.version.clone(),
                source: meta.source.clone(),
                name: stored.name.clone(),
                bytes: 0,
                encoding,
                lone: sys::lstat(&path).is_ok_and(|m| m.is_file() && m.nlink() == 1),
            });
            paths.push(path);
        }
    }
    (files, paths, unreadable)
}

// Bytes of each file that no file under the targets shares by hardlink or extent, from the same allocation helper as the disk totals.
fn store_only_bytes(targets: &Path, files: &[PathBuf]) -> Result<Vec<u64>, String> {
    let base = [targets];
    let mut out = Vec::with_capacity(files.len());
    for chunk in files.chunks(FILES_PER_QUERY) {
        let roots: Vec<[&Path; 2]> = chunk.iter().map(|path| [targets, path.as_path()]).collect();
        let mut sets: Vec<&[&Path]> = vec![&base[..]];
        sets.extend(roots.iter().map(|pair| &pair[..]));
        let sizes = compact::physical_sizes_checked(&sets).map_err(|e| e.to_string())?;
        out.extend(sizes[1..].iter().map(|size| size.saturating_sub(sizes[0])));
    }
    Ok(out)
}

fn scan_store(dirs: &Dirs, restores: &BTreeMap<String, String>) -> StoreOnly {
    let (mut files, paths, unreadable) = collect_store_files(&dirs.store());
    match store_only_bytes(&dirs.targets(), &paths) {
        Ok(sizes) => {
            for (file, bytes) in files.iter_mut().zip(sizes) {
                file.bytes = bytes;
            }
            summarize_store_only(&files, unreadable, STORE_TOP_JSON, restores)
        }
        Err(error) => StoreOnly::unsupported(format!("allocation sharing could not be measured: {error}")),
    }
}

// Read with defaults on every field so records from older and newer wrappers both parse.
#[derive(Debug, Default, serde::Deserialize)]
struct TimingLine {
    #[serde(rename = "crate", default)]
    krate: String,
    #[serde(default)]
    path_unit: bool,
    #[serde(default)]
    primary: bool,
    #[serde(default)]
    incremental: bool,
    #[serde(default)]
    prekey_ms: u64,
    #[serde(default)]
    lookup_ms: u64,
    #[serde(default)]
    keywait_ms: u64,
    #[serde(default)]
    restore_ms: u64,
    #[serde(default)]
    rustc_ms: u64,
    #[serde(default)]
    freeze_ms: u64,
    #[serde(default)]
    spawn_ms: u64,
    #[serde(default)]
    store_mode: String,
    #[serde(default)]
    inline_store_ms: u64,
    #[serde(default)]
    total_ms: u64,
    #[serde(default)]
    extern_files: u64,
    #[serde(default)]
    extern_bytes_hashed: u64,
    #[serde(default)]
    extern_bytes_memo: u64,
    #[serde(default)]
    native_dir_bytes_hashed: u64,
    #[serde(default)]
    restore_paths: BTreeMap<String, u64>,
    #[serde(default)]
    restore_fallbacks: Vec<String>,
    #[serde(default)]
    restore_key: String,
    #[serde(default)]
    miss_reason: String,
    #[serde(default)]
    kind: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct MissUnit {
    #[serde(rename = "crate")]
    krate: String,
    path_unit: bool,
    primary: bool,
    incremental: bool,
    rustc_ms: u64,
    store_mode: String,
    miss_reason: String,
}

#[derive(Debug, Default, PartialEq, Serialize)]
struct TimingSummary {
    builds: usize,
    units: usize,
    total_ms: u64,
    rustc_ms: u64,
    overhead_ms: u64,
    prekey_ms: u64,
    lookup_ms: u64,
    keywait_ms: u64,
    restore_ms: u64,
    freeze_ms: u64,
    spawn_ms: u64,
    inline_store_ms: u64,
    // Time between each frust build's exit and its stores finishing, which the timed rows exclude.
    drain_ms: u64,
    extern_files: u64,
    extern_bytes_hashed: u64,
    extern_bytes_memo: u64,
    native_dir_bytes_hashed: u64,
    store_modes: BTreeMap<String, usize>,
    miss_units: usize,
    miss_crates: usize,
    misses: Vec<MissUnit>,
    #[serde(skip)]
    miss_crate_set: BTreeSet<String>,
    // Restored files by path (kept, link, adopt, copy, clone, ...), and the first distinct reasons a linked file was copied.
    restore_paths: BTreeMap<String, u64>,
    restore_fallbacks: Vec<String>,
    // First distinct reasons a lookup missed, as `crate: reason`.
    miss_reasons: Vec<String>,
    // Uncached compiler calls such as Cargo's `-vV` probe, which the unit counts above exclude.
    passthrough_spawns: usize,
    passthrough_ms: u64,
    // Outcomes reported by store steps and helpers, and the first distinct ones that left no linked entry, as `crate: outcome`.
    store_outcomes: BTreeMap<String, usize>,
    store_problems: Vec<String>,
}

impl TimingSummary {
    fn add_build(&mut self, records: &[TimingLine], drain_ms: u64) {
        self.builds += 1;
        self.drain_ms += drain_ms;
        for r in records {
            match r.kind.as_str() {
                "passthrough" => {
                    self.passthrough_spawns += 1;
                    self.passthrough_ms += r.total_ms;
                    continue;
                }
                "store" => {
                    self.add_store_outcome(&r.krate, &r.store_mode);
                    continue;
                }
                _ => {}
            }
            self.units += 1;
            self.total_ms += r.total_ms;
            self.rustc_ms += r.rustc_ms;
            self.overhead_ms += r.total_ms.saturating_sub(r.rustc_ms);
            self.prekey_ms += r.prekey_ms;
            self.lookup_ms += r.lookup_ms;
            self.keywait_ms += r.keywait_ms;
            self.restore_ms += r.restore_ms;
            self.freeze_ms += r.freeze_ms;
            self.spawn_ms += r.spawn_ms;
            self.inline_store_ms += r.inline_store_ms;
            self.extern_files += r.extern_files;
            self.extern_bytes_hashed += r.extern_bytes_hashed;
            self.extern_bytes_memo += r.extern_bytes_memo;
            self.native_dir_bytes_hashed += r.native_dir_bytes_hashed;
            *self.store_modes.entry(r.store_mode.clone()).or_default() += 1;
            for (path, count) in &r.restore_paths {
                *self.restore_paths.entry(path.clone()).or_default() += count;
            }
            for reason in &r.restore_fallbacks {
                crate::timings::note_reason(&mut self.restore_fallbacks, reason);
            }
            if r.store_mode != "hit" && !r.miss_reason.is_empty() {
                crate::timings::note_reason(&mut self.miss_reasons, &format!("{}: {}", r.krate, r.miss_reason));
            }
            if r.store_mode != "hit" {
                self.miss_units += 1;
                self.miss_crate_set.insert(r.krate.clone());
                self.misses.push(MissUnit {
                    krate: r.krate.clone(),
                    path_unit: r.path_unit,
                    primary: r.primary,
                    incremental: r.incremental,
                    rustc_ms: r.rustc_ms,
                    store_mode: r.store_mode.clone(),
                    miss_reason: r.miss_reason.clone(),
                });
            }
        }
        self.misses.sort_by(|a, b| b.rustc_ms.cmp(&a.rustc_ms).then_with(|| a.krate.cmp(&b.krate)));
        self.misses.truncate(MISS_TOP_JSON);
        self.miss_crates = self.miss_crate_set.len();
    }

    // Outcome kinds stay few (`skip:<file> changed ...` collapses to `skip`); every outcome but a fully linked store or a present entry is kept as a problem.
    fn add_store_outcome(&mut self, krate: &str, outcome: &str) {
        *self.store_outcomes.entry(outcome.split(':').next().unwrap_or("").to_string()).or_default() += 1;
        if outcome != "stored:copied=0" && outcome != "exists" {
            crate::timings::note_reason(&mut self.store_problems, &format!("{krate}: {outcome}"));
        }
    }
}

// `clean_rebuild: link 2, copy 1 (copy:link:os32;adopt:store-nlink=2)`.
fn restore_text(row: &str, record: &TimingLine) -> String {
    let paths: Vec<String> = record.restore_paths.iter().map(|(path, count)| format!("{path} {count}")).collect();
    let mut text = format!("{row}: {}", if paths.is_empty() { "none".to_string() } else { paths.join(", ") });
    if !record.restore_fallbacks.is_empty() {
        text.push_str(&format!(" ({})", record.restore_fallbacks.join(", ")));
    }
    text
}

fn parse_timings(text: &str) -> Vec<TimingLine> {
    text.lines().filter_map(|line| serde_json::from_str(line.trim()).ok()).collect()
}

fn miss_rows(summary: &TimingSummary, limit: usize) -> Vec<Vec<String>> {
    let mut rows = vec![["crate", "path unit", "primary", "incremental", "rustc ms", "store mode", "miss reason"].iter().map(|h| h.to_string()).collect::<Vec<_>>()];
    for m in summary.misses.iter().take(limit) {
        rows.push(vec![
            m.krate.clone(), m.path_unit.to_string(), m.primary.to_string(), m.incremental.to_string(),
            m.rustc_ms.to_string(), m.store_mode.clone(),
            if m.miss_reason.is_empty() { "-".to_string() } else { m.miss_reason.clone() },
        ]);
    }
    rows
}

// Store modes collapse to their kind, so `inline:slots-busy` and `inline:error` count together.
fn store_mode_line(os: &str, cold: Option<&TimingSummary>) -> String {
    let mut kinds: BTreeMap<&str, usize> = BTreeMap::new();
    for (mode, count) in cold.iter().flat_map(|c| c.store_modes.iter()) {
        *kinds.entry(mode.split(':').next().unwrap_or("")).or_default() += count;
    }
    if kinds.is_empty() {
        return format!("Cold row store modes on {os}: no records.");
    }
    let parts: Vec<String> = kinds.iter().map(|(kind, count)| format!("{kind} {count}")).collect();
    format!("Cold row store modes on {os}: {}.", parts.join(", "))
}

fn markdown_table(rows: &[Vec<String>]) -> String {
    let mut out = String::new();
    for (index, row) in rows.iter().enumerate() {
        out.push_str(&markdown_row(row));
        out.push('\n');
        if index == 0 {
            out.push_str(&markdown_row(&vec!["---".to_string(); row.len()]));
            out.push('\n');
        }
    }
    out
}

fn timing_rows(timings: &BTreeMap<String, TimingSummary>) -> Vec<Vec<String>> {
    let mut rows = vec![TIMING_HEADER.iter().map(|h| h.to_string()).collect::<Vec<_>>()];
    for (label, key) in TIMING_ROWS {
        let Some(t) = timings.get(key) else { continue };
        let modes: Vec<String> = t.store_modes.iter().map(|(mode, count)| format!("{mode} {count}")).collect();
        rows.push(vec![
            label.to_string(), t.builds.to_string(), t.units.to_string(), t.total_ms.to_string(), t.rustc_ms.to_string(),
            t.overhead_ms.to_string(), t.prekey_ms.to_string(), t.lookup_ms.to_string(), t.keywait_ms.to_string(),
            t.restore_ms.to_string(), t.freeze_ms.to_string(), t.spawn_ms.to_string(), t.inline_store_ms.to_string(),
            t.drain_ms.to_string(), t.extern_files.to_string(), human_bytes(t.extern_bytes_hashed),
            human_bytes(t.extern_bytes_memo), human_bytes(t.native_dir_bytes_hashed),
            if modes.is_empty() { "none".to_string() } else { modes.join(", ") },
        ]);
    }
    rows
}

// Per row: how restores placed files, why links fell back, why lookups and stores missed, and how many uncached compiler calls ran.
fn sharing_lines(timings: &BTreeMap<String, TimingSummary>) -> Vec<String> {
    let counts = |map: &BTreeMap<String, u64>| map.iter().map(|(k, v)| format!("{k} {v}")).collect::<Vec<_>>().join(", ");
    let mut lines = Vec::new();
    for (label, key) in TIMING_ROWS.iter().chain(&[("warm edit", "edit_warm")]) {
        let Some(t) = timings.get(*key) else { continue };
        if !t.restore_paths.is_empty() {
            let mut line = format!("{label}: restored files by path: {}", counts(&t.restore_paths));
            if !t.restore_fallbacks.is_empty() {
                line.push_str(&format!("; link fallbacks: {}", t.restore_fallbacks.join(" | ")));
            }
            lines.push(line);
        }
        if !t.miss_reasons.is_empty() {
            lines.push(format!("{label}: miss reasons: {}", t.miss_reasons.join(" | ")));
        }
        if !t.store_problems.is_empty() {
            lines.push(format!("{label}: store outcomes without a linked entry: {}", t.store_problems.join(" | ")));
        }
        if t.passthrough_spawns > 0 {
            lines.push(format!("{label}: {} uncached compiler calls, {} ms of wrapper startup", t.passthrough_spawns, t.passthrough_ms));
        }
    }
    lines
}

fn store_group_text(groups: &[GroupBytes]) -> String {
    groups.iter().map(|g| format!("{} {}", g.name, human_bytes(g.bytes))).collect::<Vec<_>>().join(", ")
}

fn store_only_lines(s: &StoreOnly) -> Vec<String> {
    if s.status != "ok" {
        return vec![format!("Store-only bytes: {} ({}).", s.status, s.reason.as_deref().unwrap_or("no detail"))];
    }
    vec![
        format!("Store-only bytes: {} in {} payload files of {} entries, not shared with targets by hardlink or extent.",
            human_bytes(s.total_bytes), s.files, s.entries),
        format!("Store-only by crate class: {}.", store_group_text(&s.by_class)),
        format!("Store-only by file kind: {}.", store_group_text(&s.by_extension)),
    ]
}

fn store_top_rows(s: &StoreOnly, limit: usize) -> Vec<Vec<String>> {
    let header = ["crate", "version", "source", "kinds", "files", "store-only", "encodings", "lone", "last restore"];
    let mut rows = vec![header.iter().map(|h| h.to_string()).collect::<Vec<_>>()];
    for e in s.top_entries.iter().take(limit) {
        rows.push(vec![
            e.crate_name.clone(), e.version.clone(), e.source.clone().unwrap_or_else(|| "-".to_string()),
            e.kinds.join(","), e.files.to_string(), human_bytes(e.bytes), e.encodings.join(","), e.lone_files.to_string(),
            e.last_restore.clone().unwrap_or_else(|| "-".to_string()),
        ]);
    }
    rows
}

#[derive(Debug, Serialize)]
struct GateCheck {
    name: String,
    ratio: f64,
    cargo_ms: f64,
    frust_ms: f64,
    limit: String,
    pass: bool,
    ratio_min: Option<f64>,
    ratio_max: Option<f64>,
}

#[derive(Debug, Serialize)]
struct RepoReport {
    schema_version: u32,
    repo: String,
    rustc: String,
    runs: usize,
    edits: usize,
    package: Option<String>,
    edit_files: Vec<String>,
    cargo: ToolReport,
    frust: ToolReport,
    disk: DiskReport,
    ratios: Ratios,
    disk_breakdown: Option<DiskBreakdown>,
    store_only: StoreOnly,
    frust_timings: BTreeMap<String, TimingSummary>,
    features: Option<String>,
    gate: Option<Vec<GateCheck>>,
}

fn median_of(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let middle = sorted.len() / 2;
    Some(if sorted.len().is_multiple_of(2) { (sorted[middle - 1] + sorted[middle]) / 2.0 } else { sorted[middle] })
}

fn percentile(values: &[f64], p: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = (p * sorted.len() as f64 / 100.0).ceil() as usize;
    Some(sorted[rank.clamp(1, sorted.len()) - 1])
}

fn pair_ratios(cargo: &[f64], frust: &[f64]) -> Vec<f64> {
    cargo.iter().zip(frust).filter(|(c, _)| **c > 0.0).map(|(c, f)| f / c).collect()
}

fn median_ratio(cargo: &[f64], frust: &[f64]) -> Option<f64> {
    median_of(&pair_ratios(cargo, frust))
}

// Shows how noisy the paired runs were next to the median the gate uses.
fn with_spread(mut check: GateCheck, cargo: &[f64], frust: &[f64]) -> GateCheck {
    let ratios = pair_ratios(cargo, frust);
    check.ratio_min = ratios.iter().copied().min_by(f64::total_cmp);
    check.ratio_max = ratios.iter().copied().max_by(f64::total_cmp);
    check
}

fn ratio_check(name: &str, ratio: f64, cargo_ms: f64, factor: f64, slack_ms: f64) -> GateCheck {
    let limit = if slack_ms > 0.0 { format!("<= {factor:.2}x + {slack_ms:.0} ms") } else { format!("<= {factor:.2}x") };
    GateCheck {
        name: name.to_string(),
        ratio,
        cargo_ms,
        frust_ms: ratio * cargo_ms,
        limit,
        pass: (ratio - factor) * cargo_ms <= slack_ms,
        ratio_min: None,
        ratio_max: None,
    }
}

fn evaluate_gate(cargo: &Samples, frust: &Samples) -> Vec<GateCheck> {
    evaluate_gate_for(cargo, frust, cfg!(windows))
}

fn evaluate_gate_for(cargo: &Samples, frust: &Samples, windows: bool) -> Vec<GateCheck> {
    let mut checks = Vec::new();
    let paired = |c: &[f64], f: &[f64]| median_ratio(c, f).zip(median_of(c));
    if let Some((ratio, base)) = paired(&cargo.cold, &frust.cold) {
        checks.push(with_spread(ratio_check("cold", ratio, base, GATE_COLD_RATIO, GATE_COLD_SLACK_MS), &cargo.cold, &frust.cold));
    }
    if let Some((ratio, base)) = paired(&cargo.clean, &frust.clean) {
        checks.push(with_spread(ratio_check("clean-rebuild", ratio, base, GATE_CLEAN_RATIO, GATE_CLEAN_SLACK_MS), &cargo.clean, &frust.clean));
    }
    if let Some((ratio, base)) = paired(&cargo.noop, &frust.noop) {
        let slack = if windows { GATE_NOOP_SLACK_WINDOWS_MS } else { GATE_NOOP_SLACK_MS };
        checks.push(with_spread(ratio_check("noop-slack", ratio, base, 1.0, slack), &cargo.noop, &frust.noop));
        checks.push(with_spread(ratio_check("noop-ratio", ratio, base, GATE_NOOP_RATIO, 0.0), &cargo.noop, &frust.noop));
    }
    if let Some((ratio, base)) = paired(&cargo.warm, &frust.warm) {
        checks.push(with_spread(ratio_check("edit-median", ratio, base, GATE_EDIT_RATIO, GATE_EDIT_SLACK_MS), &cargo.warm, &frust.warm));
    }
    if let (Some(base), Some(measured)) = (percentile(&cargo.warm, 90.0), percentile(&frust.warm, 90.0))
        && base > 0.0
    {
        let mut check = ratio_check("edit-p90", measured / base, base, GATE_EDIT_P90_RATIO, GATE_EDIT_P90_SLACK_MS);
        check.frust_ms = measured;
        checks.push(check);
    }
    checks
}

fn disk_verdict(cargo_bytes: u64, frust_bytes: u64, slack_pct: f64) -> Result<(), String> {
    let allowed = if slack_pct > 0.0 { (cargo_bytes as f64 * (1.0 + slack_pct / 100.0)) as u64 } else { cargo_bytes };
    if frust_bytes > allowed {
        return Err(format!(
            "disk guard: frust uses {} in total, {} more than cargo's {} (allowed slack {slack_pct}%)",
            human_bytes(frust_bytes),
            human_bytes(frust_bytes - cargo_bytes),
            human_bytes(cargo_bytes)
        ));
    }
    Ok(())
}

fn exit_code(report: &RepoReport) -> i32 {
    if !report.disk.ok {
        1
    } else if report.gate.as_ref().is_some_and(|checks| checks.iter().any(|c| !c.pass)) {
        2
    } else {
        0
    }
}

fn overall_status(report: &RepoReport) -> String {
    let mut failed: Vec<&str> = Vec::new();
    if !report.disk.ok {
        failed.push("disk");
    }
    if let Some(checks) = &report.gate {
        failed.extend(checks.iter().filter(|c| !c.pass).map(|c| c.name.as_str()));
    }
    if failed.is_empty() { "pass".to_string() } else { format!("fail: {}", failed.join(", ")) }
}

fn fmt_ms(ms: Option<f64>) -> String {
    match ms {
        None => "n/a".to_string(),
        Some(ms) if ms >= 1000.0 => format!("{:.2} s", ms / 1000.0),
        Some(ms) => format!("{ms:.0} ms"),
    }
}

fn fmt_ratio(ratio: Option<f64>) -> String {
    ratio.map_or_else(|| "n/a".to_string(), |r| format!("{r:.2}x"))
}

fn table_cells(tool: &str, report: &ToolReport, status: Option<&str>) -> Vec<String> {
    let mut cells = vec![
        tool.to_string(),
        fmt_ms(report.cold.median_ms),
        fmt_ms(report.clean_rebuild.median_ms),
        fmt_ms(report.noop.median_ms),
        fmt_ms(report.edit_first.median_ms),
        fmt_ms(report.edit_warm.median_ms),
        fmt_ms(report.edit_warm.p90_ms),
        human_bytes(report.disk_bytes),
    ];
    if let Some(status) = status {
        cells.push(status.to_string());
    }
    cells
}

fn ratio_cells(ratios: &Ratios, status: Option<&str>) -> Vec<String> {
    let mut cells = vec![
        "frust / cargo".to_string(),
        fmt_ratio(ratios.cold),
        fmt_ratio(ratios.clean_rebuild),
        fmt_ratio(ratios.noop),
        fmt_ratio(ratios.edit_first),
        fmt_ratio(ratios.edit_warm),
        fmt_ratio(ratios.edit_p90),
        fmt_ratio(ratios.disk),
    ];
    if let Some(status) = status {
        cells.push(status.to_string());
    }
    cells
}

fn markdown_row(cells: &[String]) -> String {
    format!("| {} |", cells.join(" | "))
}

fn table_rows(report: &RepoReport) -> Vec<Vec<String>> {
    let status = report.gate.as_ref().map(|_| overall_status(report));
    let mut header: Vec<String> = TABLE_HEADER.iter().map(|h| h.to_string()).collect();
    if status.is_some() {
        header.push("gate".to_string());
    }
    let dash = status.as_ref().map(|_| "-");
    vec![
        header,
        table_cells("cargo", &report.cargo, dash),
        table_cells("frust", &report.frust, status.as_deref()),
        ratio_cells(&report.ratios, dash),
    ]
}

fn breakdown_lines(b: &DiskBreakdown) -> Vec<String> {
    let parts = |physical: bool| {
        let sizes: Vec<String> = b.frust_dirs.iter()
            .map(|d| format!("{} {}", d.name, human_bytes(if physical { d.physical_bytes } else { d.logical_bytes })))
            .collect();
        sizes.join(", ")
    };
    let targets_ratio = (b.cargo_physical_bytes > 0).then(|| b.targets_only_physical_bytes as f64 / b.cargo_physical_bytes as f64);
    let share = b.store_shared_share.map_or_else(|| "n/a".to_string(), |s| format!("{:.1}%", s * 100.0));
    let trees: Vec<String> = b.other_trees.iter()
        .map(|t| format!("{} {} ({} files, {} dirs)", t.name, human_bytes(t.physical_bytes), t.files, t.directories))
        .collect();
    let mut lines = vec![
        format!("Disk, physical: cargo target {}; frust total {} ({}).",
            human_bytes(b.cargo_physical_bytes), human_bytes(b.frust_physical_bytes), parts(true)),
        format!("Disk, logical: cargo target {}; frust total {} ({}).",
            human_bytes(b.cargo_logical_bytes), human_bytes(b.frust_logical_bytes), parts(false)),
        format!("Targets only: frust central targets {} against cargo target {} ({}).",
            human_bytes(b.targets_only_physical_bytes), human_bytes(b.cargo_physical_bytes), fmt_ratio(targets_ratio)),
        format!("Cache: {} entries; {} of the store is shared with targets ({}).",
            b.cache_entries, human_bytes(b.store_shared_with_targets_bytes), share),
    ];
    if let Some(before) = b.frust_physical_bytes_before_maintenance {
        lines.push(format!("Maintenance: frust total {} before the gc and compaction pass, {} after; cargo has no equivalent.",
            human_bytes(before), human_bytes(b.frust_physical_bytes)));
    }
    if !trees.is_empty() {
        lines.push(format!("Other, by tree: {}.", trees.join(", ")));
    }
    lines
}

fn gate_failure_lines(report: &RepoReport) -> Vec<String> {
    report.gate.iter().flatten().filter(|c| !c.pass).map(|c| {
        format!("{}: frust {:.1} ms against cargo {:.1} ms is {:.2}x, limit {}", c.name, c.frust_ms, c.cargo_ms, c.ratio, c.limit)
    }).collect()
}

fn repo_markdown(report: &RepoReport) -> String {
    let mut out = format!(
        "### {} (rustc {}, {} runs, {} edits per run)\n\n",
        report.repo, report.rustc, report.runs, report.edits
    );
    let rows = table_rows(report);
    let separator = vec!["---".to_string(); rows[0].len()];
    for (index, row) in rows.iter().enumerate() {
        if index == 1 {
            out.push_str(&markdown_row(&separator));
            out.push('\n');
        }
        out.push_str(&markdown_row(row));
        out.push('\n');
    }
    out.push_str(&format!("\n{}\n", store_mode_line(std::env::consts::OS, report.frust_timings.get("cold"))));
    out.push_str(&format!("\n{EDIT_NOTE}\n"));
    for line in report.disk_breakdown.iter().flat_map(breakdown_lines).chain(store_only_lines(&report.store_only)) {
        out.push_str(&format!("\n- {line}"));
    }
    out.push('\n');
    if report.store_only.status == "ok" && !report.store_only.top_entries.is_empty() {
        out.push_str(&format!("\n#### Largest store-only entries (top {STORE_TOP_MARKDOWN})\n\n"));
        out.push_str(&markdown_table(&store_top_rows(&report.store_only, STORE_TOP_MARKDOWN)));
    }
    let timing = timing_rows(&report.frust_timings);
    if timing.len() > 1 {
        out.push_str("\n#### Where the time goes (frust wrapper records, summed over the builds in each row, ms)\n\n");
        out.push_str(&markdown_table(&timing));
    }
    for line in sharing_lines(&report.frust_timings) {
        out.push_str(&format!("\n- {line}"));
    }
    out.push('\n');
    for (label, key) in [("clean rebuild", "clean_rebuild"), ("no-op", "noop")] {
        let Some(summary) = report.frust_timings.get(key).filter(|s| s.miss_units > 0) else { continue };
        out.push_str(&format!(
            "\n#### Units that compiled in the {label} row ({} units, {} distinct crates, top {MISS_TOP_MARKDOWN} by rustc ms)\n\n",
            summary.miss_units, summary.miss_crates
        ));
        out.push_str(&markdown_table(&miss_rows(summary, MISS_TOP_MARKDOWN)));
    }
    if let Some(message) = &report.disk.message {
        out.push_str(&format!("\n{message}\n"));
    }
    for line in gate_failure_lines(report) {
        out.push_str(&format!("\n- {line}"));
    }
    out.push('\n');
    out
}

fn aligned(rows: &[Vec<String>]) -> Vec<String> {
    let columns = rows.first().map_or(0, Vec::len);
    let widths: Vec<usize> = (0..columns).map(|c| rows.iter().map(|r| r[c].len()).max().unwrap_or(0)).collect();
    rows.iter().map(|row| {
        let cells: Vec<String> = row.iter().zip(&widths).map(|(cell, &width)| format!("{cell:<width$}")).collect();
        cells.join("  ").trim_end().to_string()
    }).collect()
}

fn print_repo(report: &RepoReport) {
    println!("{} (rustc {}, {} runs, {} edits per run)", report.repo, report.rustc, report.runs, report.edits);
    for line in aligned(&table_rows(report)) {
        println!("  {line}");
    }
    println!("{EDIT_NOTE}");
    println!("{}", store_mode_line(std::env::consts::OS, report.frust_timings.get("cold")));
    for line in report.disk_breakdown.iter().flat_map(breakdown_lines).chain(store_only_lines(&report.store_only)) {
        println!("{line}");
    }
    if !report.store_only.top_entries.is_empty() {
        for line in aligned(&store_top_rows(&report.store_only, STORE_TOP_MARKDOWN)) {
            println!("  {line}");
        }
    }
    let timing = timing_rows(&report.frust_timings);
    if timing.len() > 1 {
        println!("Where the time goes (frust wrapper records, summed over builds, ms):");
        for line in aligned(&timing) {
            println!("  {line}");
        }
    }
    for line in sharing_lines(&report.frust_timings) {
        println!("{line}");
    }
    if let Some(message) = &report.disk.message {
        println!("{message}");
    }
}

#[derive(Debug)]
struct RepoPackage {
    name: String,
    // Unambiguous `cargo build -p` spec: the path package id, or name@version for old metadata ids.
    spec: String,
    root: PathBuf,
    entries: Vec<PathBuf>,
}

fn package_matches(name: &str, version: &str, wanted: &str) -> bool {
    wanted == name || wanted == format!("{name}@{version}")
}

fn parse_packages(meta: &serde_json::Value, package: Option<&str>) -> Result<Vec<RepoPackage>, String> {
    let all = meta["packages"].as_array().ok_or("cargo metadata returned no package list")?;
    let members: Option<Vec<&str>> = meta["workspace_members"].as_array()
        .map(|ids| ids.iter().filter_map(|id| id.as_str()).collect());
    let mut found = Vec::new();
    for entry in all {
        let name = entry["name"].as_str().unwrap_or_default();
        let id = entry["id"].as_str().unwrap_or_default();
        let version = entry["version"].as_str().unwrap_or_default();
        if !entry["source"].is_null() {
            continue;
        }
        if let Some(members) = &members
            && !members.contains(&id)
        {
            continue;
        }
        if let Some(wanted) = package
            && !package_matches(name, version, wanted)
        {
            continue;
        }
        let spec = if id.starts_with("path+") { id.to_string() } else { format!("{name}@{version}") };
        let Some(manifest) = entry["manifest_path"].as_str() else { continue };
        let root = Path::new(manifest).parent().map(Path::to_path_buf).unwrap_or_default();
        let entries = entry["targets"].as_array().into_iter().flatten().filter(|target| {
            target["kind"].as_array().into_iter().flatten().filter_map(|k| k.as_str())
                .any(|kind| matches!(kind, "lib" | "rlib" | "dylib" | "cdylib" | "staticlib" | "proc-macro" | "bin"))
        }).filter_map(|target| target["src_path"].as_str().map(PathBuf::from)).collect();
        found.push(RepoPackage { name: name.to_string(), spec, root, entries });
    }
    if let Some(wanted) = package
        && found.is_empty()
    {
        return Err(format!("package `{wanted}` is not a path member of this workspace"));
    }
    if found.is_empty() {
        return Err("the workspace has no path packages to edit".into());
    }
    Ok(found)
}

fn build_args(packages: &[RepoPackage], package: Option<&str>, features: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = if package.is_none() {
        vec!["--workspace".to_string()]
    } else {
        packages.iter().flat_map(|p| ["-p".to_string(), p.spec.clone()]).collect()
    };
    if let Some(list) = features {
        args.extend(["--features".to_string(), list.to_string()]);
    }
    args
}

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(read) = fs::read_dir(dir) else { return };
    let mut paths: Vec<PathBuf> = read.flatten().map(|entry| entry.path()).collect();
    paths.sort();
    for path in paths {
        let Ok(kind) = fs::symlink_metadata(&path).map(|meta| meta.file_type()) else { continue };
        if kind.is_dir() {
            rust_sources(&path, out);
        } else if kind.is_file() && path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

impl RepoPackage {
    // Entry points come first because they are guaranteed to be part of the build.
    fn candidates(&self) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = self.entries.iter().filter(|p| p.is_file()).cloned().collect();
        let mut extra = Vec::new();
        rust_sources(&self.root.join("src"), &mut extra);
        for path in extra {
            if !files.contains(&path) {
                files.push(path);
            }
        }
        files
    }
}

fn select_edit_files(candidates: &[Vec<PathBuf>], limit: usize) -> Vec<(usize, PathBuf)> {
    let mut picked = Vec::new();
    let depth = candidates.iter().map(Vec::len).max().unwrap_or(0);
    for level in 0..depth {
        for (package, files) in candidates.iter().enumerate() {
            if picked.len() >= limit {
                return picked;
            }
            if let Some(file) = files.get(level) {
                picked.push((package, file.clone()));
            }
        }
    }
    picked
}

fn edit_source(original: &[u8], n: usize) -> Vec<u8> {
    let mut edited = original.to_vec();
    edited.extend_from_slice(format!("\n#[allow(dead_code)]\nconst FRUST_BENCH_EDIT: usize = {n};\n").as_bytes());
    edited
}

/// Saves every edit target into scratch and writes the original bytes back on restore or drop.
struct EditBackup {
    entries: Vec<(PathBuf, PathBuf)>,
}

impl EditBackup {
    fn create(dir: &Path, files: &[PathBuf]) -> Result<Self, String> {
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let mut entries = Vec::new();
        for (index, file) in files.iter().enumerate() {
            let saved = dir.join(format!("{index}.orig"));
            fs::copy(file, &saved).map_err(|e| format!("cannot back up {}: {e}", file.display()))?;
            entries.push((file.clone(), saved));
        }
        Ok(Self { entries })
    }

    fn apply(&self, index: usize, n: usize) -> Result<(), String> {
        let (path, saved) = &self.entries[index];
        let original = fs::read(saved).map_err(|e| format!("{}: {e}", saved.display()))?;
        fs::write(path, edit_source(&original, n)).map_err(|e| format!("cannot edit {}: {e}", path.display()))
    }

    fn restore(&self) -> Result<(), String> {
        let mut failure = None;
        for (path, saved) in &self.entries {
            let result = fs::read(saved).and_then(|bytes| fs::write(path, bytes));
            if let Err(error) = result {
                failure.get_or_insert(format!("cannot restore {}: {error}", path.display()));
            }
        }
        failure.map_or(Ok(()), Err)
    }
}

impl Drop for EditBackup {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            eprintln!("frust bench: {error}");
        }
    }
}

struct Rig<'a> {
    ctx: &'a Ctx,
    repo: &'a Path,
    cargo_target: PathBuf,
    dirs: Dirs,
    args: Vec<String>,
    scratch: PathBuf,
    timings: RefCell<BTreeMap<String, TimingSummary>>,
    // The latest restore of each entry key, so store-only entries show how they were last placed.
    restores: RefCell<BTreeMap<String, String>>,
    sequence: Cell<u64>,
}

impl Rig<'_> {
    // A background store from the previous build must not run during the next timed row.
    fn quiesce(&self) {
        if let Err(error) = drain_deferred_for(self.ctx, QUIESCE_TIMEOUT) {
            eprintln!("frust bench: warning: {error}");
        }
    }

    fn reset(&self, frust: bool, cold: bool) -> Result<(), String> {
        if !frust {
            return clear(&self.cargo_target);
        }
        self.quiesce();
        let path = if cold { self.ctx.home.clone() } else { self.dirs.targets() };
        clear(&path)
    }

    // `row` names the table row; frust builds also record per-wrapper timings under it.
    fn build(&self, frust: bool, row: &str) -> Result<f64, String> {
        if frust {
            self.quiesce();
        }
        let args: Vec<&str> = self.args.iter().map(String::as_str).collect();
        let target = (!frust).then_some(self.cargo_target.as_path());
        let mut cmd = build_command(self.ctx, frust, self.repo, target, &args, "off");
        cmd.env("CARGO_NET_OFFLINE", "false");
        let timing_file = frust.then(|| {
            self.sequence.set(self.sequence.get() + 1);
            self.scratch.join(format!("timings-{row}-{}.jsonl", self.sequence.get()))
        });
        if let Some(file) = &timing_file {
            cmd.env(crate::timings::ENV, file);
            // Created here so passthrough calls and store helpers have somewhere to report.
            let _ = fs::write(crate::timings::events_path(file), "");
        }
        let tool = if frust { "frust" } else { "cargo" };
        // timed() waits for the deferred stores before returning, so the next row starts quiet; the wait is recorded as drain.
        let (ms, drain_ms) = timed(self.ctx, frust, &mut cmd)
            .map(|run| (run.millis as f64, run.store_drain_millis.unwrap_or(0)))
            .map_err(|e| format!("{row} build with {tool}: {e}"))?;
        if let Some(file) = timing_file {
            let events = crate::timings::events_path(&file);
            let text = fs::read_to_string(&file).unwrap_or_default() + &fs::read_to_string(&events).unwrap_or_default();
            let _ = fs::remove_file(&file);
            let _ = fs::remove_file(&events);
            let records = parse_timings(&text);
            let mut restores = self.restores.borrow_mut();
            for record in records.iter().filter(|r| !r.restore_key.is_empty()) {
                restores.insert(record.restore_key.clone(), restore_text(row, record));
            }
            self.timings.borrow_mut().entry(row.to_string()).or_default().add_build(&records, drain_ms);
        }
        Ok(ms)
    }
}

fn tool_order(flip: usize) -> [bool; 2] {
    if flip.is_multiple_of(2) { [false, true] } else { [true, false] }
}

fn measure(rig: &Rig, backup: &EditBackup, owners: &[usize], opts: &RepoOptions) -> Result<[Samples; 2], String> {
    let mut samples = [Samples::default(), Samples::default()];
    for run in 0..opts.runs {
        eprintln!("frust bench: run {} of {}", run + 1, opts.runs);
        let order = tool_order(run);
        for frust in order {
            rig.reset(frust, true)?;
            samples[usize::from(frust)].cold.push(rig.build(frust, "cold")?);
        }
        for frust in order {
            rig.reset(frust, false)?;
            samples[usize::from(frust)].clean.push(rig.build(frust, "clean_rebuild")?);
        }
        for frust in order {
            let mut repeats = Vec::new();
            for _ in 0..NOOP_REPEATS {
                repeats.push(rig.build(frust, "noop")?);
            }
            samples[usize::from(frust)].noop.extend(median_of(&repeats));
        }
        let mut touched = HashSet::new();
        for edit in 0..opts.edits {
            let slot = edit % owners.len();
            backup.apply(slot, edit + 1)?;
            let first = touched.insert(owners[slot]);
            for frust in tool_order(run + edit) {
                let ms = rig.build(frust, if first { "edit_first" } else { "edit_warm" })?;
                let target = &mut samples[usize::from(frust)];
                if first { target.first.push(ms) } else { target.warm.push(ms) }
            }
        }
        backup.restore()?;
    }
    Ok(samples)
}

fn fetch_dependencies(ctx: &Ctx, repo: &Path) -> Result<(), String> {
    let output = Command::new(&ctx.cargo).arg("fetch").current_dir(repo).stdin(Stdio::null()).output()
        .map_err(|e| format!("cargo fetch: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!("cargo fetch failed:\n{}", failure_tail(&String::from_utf8_lossy(&output.stderr))))
}

fn workspace_metadata(ctx: &Ctx, repo: &Path) -> Result<serde_json::Value, String> {
    let output = Command::new(&ctx.cargo).args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(repo).stdin(Stdio::null()).output().map_err(|e| format!("cargo metadata: {e}"))?;
    if !output.status.success() {
        return Err(format!("cargo metadata failed:\n{}", failure_tail(&String::from_utf8_lossy(&output.stderr))));
    }
    serde_json::from_slice(&output.stdout).map_err(|e| format!("cargo metadata output: {e}"))
}

// Runs the daily gc and a full compaction pass as the product does on its own, then an explicit gc that also sweeps every idle compile lock whatever its age; returns the pass time.
fn run_maintenance(ctx: &Ctx, dirs: &Dirs, target: &Path) -> Result<u64, String> {
    let started = Instant::now();
    measure_maintenance(ctx, dirs, target)?;
    let opts = GcOptions { keylock_max_age: Duration::ZERO, ..GcOptions::from_config(&benchmark_maintenance_config()) };
    let (_, failures) = gc::execute(dirs, &gc::plan(dirs, &opts));
    if let Some((path, error)) = failures.first() {
        return Err(format!("gc could not remove {}: {error}", path.display()));
    }
    Ok(started.elapsed().as_millis() as u64)
}

fn benchmark(scratch: &Path, opts: &RepoOptions) -> Result<RepoReport, String> {
    fs::create_dir_all(scratch).map_err(|e| e.to_string())?;
    let repo = canonical(&opts.repo);
    if !repo.join("Cargo.toml").is_file() {
        return Err(format!("{} has no Cargo.toml", repo.display()));
    }
    let cargo = resolve_tool("cargo").ok_or("cargo not found")?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let fcargo = fcargo_beside(&exe);
    if !fcargo.exists() {
        return Err("fcargo binary not found next to frust; install all binaries".into());
    }
    let probe = Probe::run(None, &repo).ok_or("could not query rustc")?;
    let ctx = Ctx {
        cargo,
        fcargo,
        home: scratch.join("frust-home"),
        probe_id: probe.id.clone(),
        audit: None,
        compression: "auto".to_string(),
        store: StorePolicy::Deferred,
    };
    fetch_dependencies(&ctx, &repo)?;
    let packages = parse_packages(&workspace_metadata(&ctx, &repo)?, opts.package.as_deref())?;
    let candidates: Vec<Vec<PathBuf>> = packages.iter().map(RepoPackage::candidates).collect();
    let picked = select_edit_files(&candidates, MAX_EDIT_FILES);
    if picked.is_empty() {
        return Err("no Rust source files found in the selected workspace packages".into());
    }
    let owners: Vec<usize> = picked.iter().map(|(package, _)| *package).collect();
    let files: Vec<PathBuf> = picked.into_iter().map(|(_, file)| file).collect();
    let names: Vec<&str> = owners.iter().map(|&i| packages[i].name.as_str()).collect();
    eprintln!("frust bench: editing {} files across crates {}", files.len(), names.join(", "));
    guard_disk(scratch)?;

    let backup = EditBackup::create(&scratch.join("edit-backup"), &files)?;
    let rig = Rig {
        ctx: &ctx,
        repo: &repo,
        cargo_target: scratch.join("cargo-target"),
        dirs: Dirs::at(&ctx.home),
        args: build_args(&packages, opts.package.as_deref(), opts.features.as_deref()),
        scratch: scratch.to_path_buf(),
        timings: RefCell::new(BTreeMap::new()),
        restores: RefCell::new(BTreeMap::new()),
        sequence: Cell::new(0),
    };
    let samples = measure(&rig, &backup, &owners, opts)?;
    rig.quiesce();
    let cargo_bytes = compact::physical_size(&[rig.cargo_target.as_path()]);
    let frust_bytes_before_maintenance = compact::physical_size(&[ctx.home.as_path()]);
    let central = workspace::central_target(&rig.dirs, Some(&repo), &ctx.probe_id);
    let maintenance_ms = run_maintenance(&ctx, &rig.dirs, &central)
        .inspect_err(|error| eprintln!("frust bench: warning: maintenance pass incomplete: {error}")).unwrap_or(0);
    let frust_bytes = compact::physical_size(&[ctx.home.as_path()]);
    let verdict = disk_verdict(cargo_bytes, frust_bytes, opts.disk_slack);
    let store_only = scan_store(&rig.dirs, &rig.restores.borrow());
    let disk_breakdown = measure_disk_breakdown(&rig.dirs, &rig.cargo_target, cargo_bytes)
        .inspect_err(|error| eprintln!("frust bench: warning: disk breakdown unavailable: {error}")).ok()
        .map(|breakdown| DiskBreakdown { frust_physical_bytes_before_maintenance: Some(frust_bytes_before_maintenance), ..breakdown });
    let (cargo_report, frust_report) = (samples[0].report(cargo_bytes), samples[1].report(frust_bytes));
    let display = |path: &PathBuf| path.strip_prefix(&repo).unwrap_or(path.as_path()).display().to_string();
    Ok(RepoReport {
        schema_version: REPORT_SCHEMA_VERSION,
        repo: repo.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| repo.display().to_string()),
        rustc: probe.release.clone(),
        runs: opts.runs,
        edits: opts.edits,
        package: opts.package.clone(),
        edit_files: files.iter().map(display).collect(),
        ratios: paired_ratios(&cargo_report, &frust_report),
        cargo: cargo_report,
        frust: frust_report,
        disk: DiskReport { cargo_bytes, frust_bytes, frust_bytes_before_maintenance, maintenance_ms, slack_pct: opts.disk_slack, ok: verdict.is_ok(), message: verdict.err() },
        disk_breakdown,
        store_only,
        frust_timings: rig.timings.take(),
        features: opts.features.clone(),
        gate: opts.gate.then(|| evaluate_gate(&samples[0], &samples[1])),
    })
}

pub(super) fn run(flags: &[String]) -> i32 {
    let opts = match RepoOptions::parse(flags) {
        Ok(opts) => opts,
        Err(error) => {
            eprintln!("frust bench: {error}");
            return 1;
        }
    };
    let tmp = std::env::temp_dir();
    remove_abandoned_scratch(&tmp);
    let scratch = tmp.join(format!("frust-bench-{}", std::process::id()));
    let guard = Scratch(scratch.clone(), false);
    let result = guard_disk(&tmp).and_then(|_| benchmark(&scratch, &opts));
    drop(guard);
    let report = match result {
        Ok(report) => report,
        Err(error) => {
            eprintln!("frust bench: {error}");
            return 1;
        }
    };
    let mut code = exit_code(&report);
    let json = serde_json::to_string_pretty(&report).unwrap_or_default();
    if let Some(path) = &opts.out
        && let Err(error) = fs::write(path, format!("{json}\n"))
    {
        eprintln!("frust bench: cannot write {}: {error}", path.display());
        code = 1;
    }
    if opts.markdown {
        print!("{}", repo_markdown(&report));
    } else if opts.json {
        println!("{json}");
    } else {
        print_repo(&report);
    }
    if let Some(message) = &report.disk.message {
        eprintln!("frust bench: {message}");
        for line in report.disk_breakdown.iter().flat_map(breakdown_lines) {
            eprintln!("frust bench: {line}");
        }
    }
    for line in gate_failure_lines(&report) {
        eprintln!("frust bench: gate failed: {line}");
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timings::Record as TimingRecord;

    fn flags(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn paths(items: &[&str]) -> Vec<PathBuf> {
        items.iter().map(PathBuf::from).collect()
    }

    fn samples(cold: f64, clean: f64, noop: f64, warm: f64) -> Samples {
        Samples { cold: vec![cold; 3], clean: vec![clean; 3], noop: vec![noop; 3], first: vec![warm * 2.0], warm: vec![warm; 5] }
    }

    fn report_with(cargo: &Samples, frust: &Samples, disk: (u64, u64), gate: bool) -> RepoReport {
        let verdict = disk_verdict(disk.0, disk.1, 0.0);
        RepoReport {
            schema_version: REPORT_SCHEMA_VERSION,
            repo: "demo".into(),
            rustc: "1.0.0".into(),
            runs: 3,
            edits: 20,
            package: None,
            edit_files: Vec::new(),
            ratios: paired_ratios(&cargo.report(disk.0), &frust.report(disk.1)),
            cargo: cargo.report(disk.0),
            frust: frust.report(disk.1),
            disk: DiskReport { cargo_bytes: disk.0, frust_bytes: disk.1, frust_bytes_before_maintenance: disk.1, maintenance_ms: 0, slack_pct: 0.0, ok: verdict.is_ok(), message: verdict.err() },
            disk_breakdown: None,
            store_only: StoreOnly::unsupported("not measured in this test".to_string()),
            frust_timings: BTreeMap::new(),
            features: None,
            gate: gate.then(|| evaluate_gate_for(cargo, frust, false)),
        }
    }

    fn failing(checks: &[GateCheck]) -> Vec<&str> {
        checks.iter().filter(|c| !c.pass).map(|c| c.name.as_str()).collect()
    }

    #[test]
    fn flags_parse_with_defaults_and_both_value_forms() {
        let parsed = RepoOptions::parse(&flags(&["--repo", "/work/rg"])).unwrap();
        assert_eq!(parsed, RepoOptions {
            repo: PathBuf::from("/work/rg"), edits: 20, runs: 3, package: None,
            json: false, markdown: false, out: None, gate: false, features: None, disk_slack: 0.0,
        });
        let full = RepoOptions::parse(&flags(&[
            "--repo=/work/tokio", "--edits", "5", "--runs=2", "--package", "tokio",
            "--json", "--markdown", "--out", "r.json", "--gate", "--features", "full", "--disk-slack=2.5",
        ])).unwrap();
        assert_eq!(full, RepoOptions {
            repo: PathBuf::from("/work/tokio"), edits: 5, runs: 2, package: Some("tokio".into()),
            json: true, markdown: true, out: Some(PathBuf::from("r.json")), gate: true,
            features: Some("full".into()), disk_slack: 2.5,
        });
    }

    #[test]
    fn flags_reject_missing_repo_bad_numbers_and_foreign_flags() {
        assert!(RepoOptions::parse(&flags(&["--gate"])).unwrap_err().contains("--repo"));
        assert!(RepoOptions::parse(&flags(&["--repo", "x", "--edits", "0"])).unwrap_err().contains("--edits"));
        assert!(RepoOptions::parse(&flags(&["--repo", "x", "--runs=abc"])).unwrap_err().contains("--runs"));
        assert!(RepoOptions::parse(&flags(&["--repo", "x", "--quick"])).unwrap_err().contains("--quick"));
        assert!(RepoOptions::parse(&flags(&["--repo", "x", "--gate=1"])).is_err());
        assert!(RepoOptions::parse(&flags(&["--repo"])).unwrap_err().contains("needs a value"));
        assert!(RepoOptions::parse(&flags(&["--repo=", "--gate"])).is_err());
        assert!(RepoOptions::parse(&flags(&["--repo", "x", "--disk-slack", "-1"])).unwrap_err().contains("--disk-slack"));
        assert!(RepoOptions::parse(&flags(&["--repo", "x", "--disk-slack=nan"])).is_err());
        assert!(RepoOptions::parse(&flags(&["--repo", "x", "--features="])).is_err());
    }

    #[test]
    fn edit_files_round_robin_across_crates_up_to_the_limit() {
        let candidates = vec![
            paths(&["a/lib.rs", "a/x.rs", "a/y.rs"]),
            paths(&["b/main.rs"]),
            paths(&["c/lib.rs", "c/z.rs"]),
        ];
        let picked: Vec<(usize, String)> = select_edit_files(&candidates, 5).into_iter()
            .map(|(package, file)| (package, file.display().to_string())).collect();
        assert_eq!(picked, vec![
            (0, "a/lib.rs".to_string()), (1, "b/main.rs".to_string()), (2, "c/lib.rs".to_string()),
            (0, "a/x.rs".to_string()), (2, "c/z.rs".to_string()),
        ]);
        assert_eq!(select_edit_files(&candidates, 100).len(), 6);
        assert!(select_edit_files(&candidates, 0).is_empty());
        assert!(select_edit_files(&[], 8).is_empty());
    }

    #[test]
    fn packages_come_from_all_path_members_or_the_named_package() {
        let meta = serde_json::json!({
            "workspace_members": ["path+file:///w#root@1.0.0", "path+file:///w/crates/inner#inner@1.0.0"],
            "workspace_default_members": ["path+file:///w#root@1.0.0"],
            "packages": [
                {"name": "root", "version": "1.0.0", "source": null, "id": "path+file:///w#root@1.0.0", "manifest_path": "/w/Cargo.toml",
                 "targets": [{"kind": ["bin"], "src_path": "/w/src/main.rs"}, {"kind": ["test"], "src_path": "/w/tests/t.rs"}]},
                {"name": "inner", "version": "1.0.0", "source": null, "id": "path+file:///w/crates/inner#inner@1.0.0", "manifest_path": "/w/crates/inner/Cargo.toml",
                 "targets": [{"kind": ["lib"], "src_path": "/w/crates/inner/src/lib.rs"}]},
            ],
        });
        let all = parse_packages(&meta, None).unwrap();
        let names: Vec<&str> = all.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["root", "inner"]);
        assert_eq!(all[0].root, PathBuf::from("/w"));
        assert_eq!(all[0].entries, paths(&["/w/src/main.rs"]));
        assert_eq!(build_args(&all, None, None), ["--workspace"]);
        assert_eq!(build_args(&all, None, Some("full")), ["--workspace", "--features", "full"]);
        let named = parse_packages(&meta, Some("inner")).unwrap();
        assert_eq!(named.len(), 1);
        assert_eq!(named[0].entries, paths(&["/w/crates/inner/src/lib.rs"]));
        assert!(parse_packages(&meta, Some("missing")).unwrap_err().contains("missing"));
    }

    #[test]
    fn package_spec_is_the_workspace_path_id_even_with_two_versions_in_the_graph() {
        let meta = serde_json::json!({
            "workspace_members": ["path+file:///w/tokio#tokio@1.40.0", "path+file:///w/tokio-util#tokio-util@0.7.0"],
            "packages": [
                {"name": "tokio", "version": "1.40.0", "source": null, "id": "path+file:///w/tokio#tokio@1.40.0", "manifest_path": "/w/tokio/Cargo.toml",
                 "targets": [{"kind": ["lib"], "src_path": "/w/tokio/src/lib.rs"}]},
                {"name": "tokio", "version": "1.39.0", "source": "registry+https://github.com/rust-lang/crates.io-index", "id": "registry+https://github.com/rust-lang/crates.io-index#tokio@1.39.0", "manifest_path": "/r/tokio-1.39.0/Cargo.toml",
                 "targets": [{"kind": ["lib"], "src_path": "/r/tokio-1.39.0/src/lib.rs"}]},
                {"name": "tokio-util", "version": "0.7.0", "source": null, "id": "path+file:///w/tokio-util#tokio-util@0.7.0", "manifest_path": "/w/tokio-util/Cargo.toml",
                 "targets": [{"kind": ["lib"], "src_path": "/w/tokio-util/src/lib.rs"}]},
            ],
        });
        for wanted in ["tokio", "tokio@1.40.0"] {
            let found = parse_packages(&meta, Some(wanted)).unwrap();
            assert_eq!(found.len(), 1, "{wanted}");
            assert_eq!(found[0].spec, "path+file:///w/tokio#tokio@1.40.0");
            assert_eq!(build_args(&found, Some(wanted), None), ["-p", "path+file:///w/tokio#tokio@1.40.0"]);
            assert_eq!(build_args(&found, Some(wanted), Some("full")), ["-p", "path+file:///w/tokio#tokio@1.40.0", "--features", "full"]);
        }
        assert!(parse_packages(&meta, Some("tokio@1.39.0")).is_err());
        assert_eq!(parse_packages(&meta, None).unwrap().len(), 2);
    }

    #[test]
    fn old_style_ids_fall_back_to_name_at_version() {
        let meta = serde_json::json!({
            "packages": [{"name": "a", "version": "0.2.0", "source": null, "id": "a 0.2.0 (path+file:///w/a)", "manifest_path": "/w/a/Cargo.toml", "targets": []}],
        });
        assert_eq!(parse_packages(&meta, Some("a")).unwrap()[0].spec, "a@0.2.0");
    }

    #[test]
    fn edit_files_span_all_members_and_are_capped() {
        let candidates: Vec<Vec<PathBuf>> = (0..12).map(|i| paths(&[&format!("c{i}/lib.rs"), &format!("c{i}/b.rs")])).collect();
        let picked = select_edit_files(&candidates, MAX_EDIT_FILES);
        assert_eq!(picked.len(), 8);
        let crates: HashSet<usize> = picked.iter().map(|(package, _)| *package).collect();
        assert_eq!(crates.len(), 8);
    }

    #[test]
    fn other_trees_report_files_directories_and_a_markdown_line() {
        let root = std::env::temp_dir().join(format!("frust-other-trees-{}", std::process::id()));
        let (locks, probe) = (root.join("locks"), root.join("probe"));
        fs::create_dir_all(locks.join("a/b")).unwrap();
        fs::create_dir_all(&probe).unwrap();
        fs::write(locks.join("a/one"), b"x").unwrap();
        fs::write(locks.join("a/b/two"), b"y").unwrap();
        fs::write(root.join("ledger"), b"z").unwrap();
        let trees = measure_other_trees(&[locks.clone(), probe.clone(), root.join("ledger")]).unwrap();
        let find = |name: &str| trees.iter().find(|t| t.name == name).unwrap();
        assert_eq!((find("locks").files, find("locks").directories), (2, 3));
        assert_eq!((find("probe").files, find("probe").directories), (0, 1));
        assert_eq!((find("ledger").files, find("ledger").directories), (1, 0));
        let mut b = build_breakdown(10, 10, [1; 6], [1; 5], 0);
        b.other_trees = trees;
        b.frust_physical_bytes_before_maintenance = Some(2048);
        let lines = breakdown_lines(&b);
        assert!(lines.iter().any(|line| line.starts_with("Maintenance: frust total 2.0 KiB before")));
        assert!(lines.last().unwrap().starts_with("Other, by tree: "));
        assert!(lines.last().unwrap().contains("locks"));
        assert!(serde_json::to_value(&b).unwrap()["other_trees"][0]["physical_bytes"].is_u64());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_disk_gate_reads_the_post_maintenance_total_and_the_report_keeps_both() {
        let report = report_with(&samples(1.0, 1.0, 1.0, 1.0), &samples(1.0, 1.0, 1.0, 1.0), (1000, 900), false);
        let json = serde_json::to_value(&report.disk).unwrap();
        assert_eq!(json["frust_bytes"], 900);
        assert!(json["frust_bytes_before_maintenance"].is_u64() && json["maintenance_ms"].is_u64());
        assert!(report.disk.ok);
    }

    #[test]
    fn breakdown_attributes_shared_bytes_once_and_formats_each_line() {
        let b = build_breakdown(1000, 1200, [600, 900, 910, 920, 950, 500], [650, 700, 10, 10, 30], 42);
        let physical: Vec<(&str, u64)> = b.frust_dirs.iter().map(|d| (d.name.as_str(), d.physical_bytes)).collect();
        assert_eq!(physical, [("store", 600), ("targets", 300), ("manifests", 10), ("tmp", 10), ("other", 30)]);
        assert_eq!(b.frust_physical_bytes, 950);
        assert_eq!(b.frust_logical_bytes, 1400);
        assert_eq!(b.targets_only_physical_bytes, 500);
        assert_eq!(b.store_shared_with_targets_bytes, 200);
        assert_eq!(b.store_shared_share, Some(200.0 / 600.0));
        let lines = breakdown_lines(&b);
        assert_eq!(lines.len(), 4);
        assert!(lines[0].starts_with("Disk, physical: cargo target 1000 B; frust total 950 B (store 600 B, targets 300 B"));
        assert!(lines[1].starts_with("Disk, logical: cargo target 1.2 KiB; frust total 1.4 KiB"));
        assert!(lines[2].contains("against cargo target 1000 B (0.50x)"));
        assert_eq!(lines[3], "Cache: 42 entries; 200 B of the store is shared with targets (33.3%).");
        let empty = build_breakdown(0, 0, [0; 6], [0; 5], 0);
        assert_eq!(empty.store_shared_share, None);
        assert!(breakdown_lines(&empty)[3].ends_with("(n/a)."));
    }

    #[test]
    fn ratios_pair_runs_and_report_disk_share() {
        let cargo = samples(1000.0, 1000.0, 20.0, 100.0).report(200);
        let frust = samples(1040.0, 400.0, 24.0, 110.0).report(100);
        let r = paired_ratios(&cargo, &frust);
        assert_eq!(r.clean_rebuild, Some(0.4));
        assert_eq!(r.edit_p90, Some(1.1));
        assert_eq!(r.disk, Some(0.5));
        assert_eq!(fmt_ratio(r.clean_rebuild), "0.40x");
        assert_eq!(fmt_ratio(None), "n/a");
    }

    fn sf(key: &str, krate: &str, source: Option<&str>, name: &str, bytes: u64) -> StoreFile {
        StoreFile { key: key.into(), krate: krate.into(), version: "1.0.0".into(), source: source.map(String::from), name: name.into(), bytes, encoding: "linked".into(), lone: bytes > 0 }
    }

    fn synthetic_store() -> Vec<StoreFile> {
        vec![
            sf("k1", "serde", Some("registry"), "libserde-h.rlib", 1000),
            sf("k1", "serde", Some("registry"), "libserde-h.rmeta", 300),
            sf("k1", "serde", Some("registry"), "serde-h.d", 10),
            sf("k2", "app", Some("path"), "libapp-h.rlib", 500),
            sf("k2", "app", Some("path"), "app", 200),
            sf("k3", "serde_derive", Some("registry"), "libserde_derive-h.so", 700),
            sf("k4", "build_script_build", Some("path"), "build_script_build-h", 40),
            sf("k5", "mystery", None, "libm-h.rlib", 0),
        ]
    }

    fn group<'a>(groups: &'a [GroupBytes], name: &str) -> &'a GroupBytes {
        groups.iter().find(|g| g.name == name).unwrap()
    }

    #[test]
    fn store_only_bytes_group_by_class_and_extension() {
        let summary = summarize_store_only(&synthetic_store(), 2, 3, &BTreeMap::new());
        assert_eq!((summary.status.as_str(), summary.total_bytes, summary.entries, summary.files, summary.unreadable_entries), ("ok", 2750, 5, 8, 2));
        let classes: Vec<(&str, u64, usize)> = summary.by_class.iter().map(|g| (g.name.as_str(), g.bytes, g.files)).collect();
        assert_eq!(classes, [("registry-git", 1310, 3), ("path", 700, 2), ("unknown", 0, 1), ("build-script", 40, 1), ("proc-macro", 700, 1)]);
        let kinds: Vec<(&str, u64, usize)> = summary.by_extension.iter().map(|g| (g.name.as_str(), g.bytes, g.files)).collect();
        assert_eq!(kinds, [("rlib", 1500, 3), ("rmeta", 300, 1), ("dylib", 700, 1), ("bin", 240, 2), ("d", 10, 1), ("other", 0, 0)]);
        assert_eq!(group(&summary.by_class, "proc-macro").bytes, 700);
    }

    #[test]
    fn store_only_top_entries_are_ordered_capped_and_skip_empty_entries() {
        let restores = BTreeMap::from([("k1".to_string(), "clean_rebuild: copy 2 (copy:link:os32;adopt:link:os32)".to_string())]);
        let summary = summarize_store_only(&synthetic_store(), 0, 3, &restores);
        assert_eq!((summary.top_entries[0].encodings.as_slice(), summary.top_entries[0].lone_files), (&["linked".to_string()][..], 3));
        assert_eq!(summary.top_entries[0].last_restore.as_deref(), restores.get("k1").map(String::as_str));
        assert_eq!(summary.top_entries[1].last_restore, None);
        let top: Vec<(&str, u64)> = summary.top_entries.iter().map(|e| (e.key.as_str(), e.bytes)).collect();
        assert_eq!(top, [("k1", 1310), ("k2", 700), ("k3", 700)]);
        assert_eq!(summary.top_entries[0].kinds, ["d", "rlib", "rmeta"]);
        assert_eq!(summary.top_entries[1].kinds, ["bin", "rlib"]);
        assert_eq!(summary.top_entries[0].files, 3);
        let all = summarize_store_only(&synthetic_store(), 0, 20, &restores);
        assert_eq!(all.top_entries.len(), 4);
        let rows = store_top_rows(&all, 2);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1][..3], ["serde", "1.0.0", "registry"]);
        assert_eq!(rows[1][6..], ["linked", "3", "clean_rebuild: copy 2 (copy:link:os32;adopt:link:os32)"]);
        assert_eq!(store_top_rows(&all, 10)[4][2], "path");
        let unsupported = StoreOnly::unsupported("no extents".into());
        assert!(store_only_lines(&unsupported)[0].contains("unsupported (no extents)"));
        assert!(store_only_lines(&all)[0].contains("2.7 KiB"));
    }

    fn deferred_dir(tag: &str, names: &[&str]) -> (PathBuf, Vec<PathBuf>) {
        let dir = std::env::temp_dir().join(format!("frust-bench-test-deferred-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for name in names {
            fs::write(dir.join(name), b"x").unwrap();
        }
        let paths = deferred_entries(&dir).unwrap().unwrap();
        (dir, paths)
    }

    #[test]
    fn pending_jobs_cover_queued_and_claimed_files_but_not_slots_or_other_files() {
        let names = ["job-1-2-qabc.json", "job-1-5.json", "claimed-9-1-2-qabc.json", "slot-0", "job-1-2.txt", "notes.json"];
        let (dir, paths) = deferred_dir("pending", &names);
        assert_eq!(pending_job_names(&paths), ["claimed-9-1-2-qabc.json", "job-1-2-qabc.json", "job-1-5.json"]);
        assert!(has_deferred_jobs(&paths));
        let (slots_dir, only_slots) = deferred_dir("slots", &["slot-0", "slot-1", "notes.json"]);
        assert!(!has_deferred_jobs(&only_slots));
        let (claimed_dir, only_claimed) = deferred_dir("claimed", &["claimed-9-1-2-qabc.json"]);
        assert!(has_deferred_jobs(&only_claimed));
        for path in [dir, slots_dir, claimed_dir] {
            let _ = fs::remove_dir_all(path);
        }
    }

    #[test]
    fn a_drainer_is_needed_only_for_queued_jobs_with_no_slot_holder() {
        let (dir, paths) = deferred_dir("needed", &["job-1-2-qabc.json", "slot-0"]);
        assert!(drainer_needed(&paths));
        let slot = fs::OpenOptions::new().read(true).write(true).open(dir.join("slot-0")).unwrap();
        sys::try_lock(&slot, sys::LockKind::Exclusive).unwrap();
        assert!(!drainer_needed(&paths), "a held slot means a helper is draining");
        let _ = sys::unlock(&slot);
        let (other, claimed_only) = deferred_dir("needed-claimed", &["claimed-9-1-2-qabc.json", "slot-0"]);
        assert!(drainer_needed(&claimed_only), "a claimed file with no slot holder is adopted by the drain helper");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&other);
    }

    #[test]
    fn extension_and_class_rules_follow_the_documented_patterns() {
        for (name, group) in [
            ("libx-h.so", "dylib"), ("x.DLL", "dylib"), ("libx.dylib", "dylib"), ("x.exe", "bin"), ("tool", "bin"),
            ("x.d", "d"), ("x.dSYM", "other"), ("libx.rlib", "rlib"), ("libx.rmeta", "rmeta"),
        ] {
            assert_eq!(extension_group(name), group, "{name}");
        }
        assert_eq!(crate_class("build_script_main", Some("path"), false), "build-script");
        assert_eq!(crate_class("foo_macros", Some("registry"), false), "proc-macro");
        assert_eq!(crate_class("anything", Some("git"), true), "proc-macro");
        assert_eq!(crate_class("regex", Some("git"), false), "registry-git");
        assert_eq!(crate_class("regex", None, false), "unknown");
    }

    #[test]
    fn timing_records_aggregate_overhead_and_store_modes() {
        let mut deferred = TimingRecord::new("serde", false, true, false);
        deferred.total_ms = 10;
        deferred.rustc_ms = 6;
        deferred.prekey_ms = 1;
        deferred.lookup_ms = 2;
        deferred.freeze_ms = 1;
        deferred.spawn_ms = 1;
        deferred.store_mode = "deferred".to_string();
        let mut hit = TimingRecord::new("app", true, true, false);
        hit.total_ms = 4;
        hit.restore_ms = 3;
        hit.keywait_ms = 1;
        hit.store_mode = "hit".to_string();
        let text = format!("{}\nnot json\n\n{}", deferred.line().trim_end(), hit.line());
        let records = parse_timings(&text);
        assert_eq!(records.len(), 2);
        let mut summary = TimingSummary::default();
        summary.add_build(&records, 0);
        assert_eq!((summary.builds, summary.units, summary.total_ms, summary.rustc_ms, summary.overhead_ms), (1, 2, 14, 6, 8));
        assert_eq!((summary.prekey_ms, summary.lookup_ms, summary.keywait_ms, summary.restore_ms), (1, 2, 1, 3));
        assert_eq!((summary.freeze_ms, summary.spawn_ms, summary.inline_store_ms), (1, 1, 0));
        assert_eq!(summary.store_modes.get("hit"), Some(&1));
        assert_eq!((summary.extern_files, summary.extern_bytes_hashed, summary.native_dir_bytes_hashed), (0, 0, 0));
        summary.add_build(&[], 0);
        assert_eq!((summary.builds, summary.units), (2, 2));
    }

    fn line(krate: &str, rustc_ms: u64, mode: &str) -> TimingLine {
        TimingLine { krate: krate.into(), rustc_ms, store_mode: mode.into(), ..TimingLine::default() }
    }

    #[test]
    fn new_wrapper_fields_are_summed_and_missing_ones_default_to_zero() {
        let text = concat!(
            r#"{"crate":"a","store_mode":"hit","extern_files":4,"extern_bytes_hashed":1000,"extern_bytes_memo":300,"native_dir_bytes_hashed":50}"#, "\n",
            r#"{"crate":"b","store_mode":"hit","extern_files":1,"extern_bytes_hashed":24}"#, "\n",
            r#"{"crate":"c","store_mode":"hit"}"#, "\n",
        );
        let records = parse_timings(text);
        assert_eq!(records.len(), 3);
        let mut summary = TimingSummary::default();
        summary.add_build(&records, 0);
        assert_eq!(
            (summary.extern_files, summary.extern_bytes_hashed, summary.extern_bytes_memo, summary.native_dir_bytes_hashed),
            (5, 1024, 300, 50)
        );
        let rows = timing_rows(&BTreeMap::from([("cold".to_string(), summary)]));
        assert_eq!(rows[1][14..18], ["5", "1.0 KiB", "300 B", "50 B"]);
    }

    #[test]
    fn drain_time_accumulates_per_build() {
        let mut summary = TimingSummary::default();
        summary.add_build(&[], 7);
        summary.add_build(&[], 5);
        assert_eq!((summary.builds, summary.drain_ms), (2, 12));
    }

    #[test]
    fn units_that_did_not_hit_are_listed_by_rustc_time_with_distinct_crates() {
        let mut summary = TimingSummary::default();
        summary.add_build(&[line("app", 0, "hit"), line("serde", 600, "deferred"), line("regex", 900, "inline:error"), line("serde", 50, "deferred")], 0);
        let listed: Vec<(&str, u64)> = summary.misses.iter().map(|m| (m.krate.as_str(), m.rustc_ms)).collect();
        assert_eq!(listed, [("regex", 900), ("serde", 600), ("serde", 50)]);
        assert_eq!((summary.miss_units, summary.miss_crates), (3, 2));
        let rows = miss_rows(&summary, 2);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1], ["regex", "false", "false", "false", "900", "inline:error", "-"]);
        let many: Vec<TimingLine> = (0..25).map(|i| line(&format!("c{i}"), i, "deferred")).collect();
        let mut capped = TimingSummary::default();
        capped.add_build(&many, 0);
        assert_eq!((capped.misses.len(), capped.miss_units, capped.miss_crates), (MISS_TOP_JSON, 25, 25));
        assert_eq!(capped.misses[0].krate, "c24");
    }

    #[test]
    fn restore_paths_fallbacks_misses_spawns_and_store_outcomes_are_summarised_apart_from_units() {
        let text = concat!(
            r#"{"crate":"a","store_mode":"hit","restore_paths":{"link":2,"copy":1},"restore_fallbacks":["copy:link:os32;adopt:link:os32"],"restore_key":"k1"}"#, "\n",
            r#"{"crate":"b","store_mode":"hit","restore_paths":{"link":1,"kept":2},"restore_fallbacks":["copy:link:os32;adopt:link:os32","copy:store-nlink=2;adopt:store-nlink=2"]}"#, "\n",
            r#"{"crate":"winapi","store_mode":"deferred","miss_reason":"input:lib.rs"}"#, "\n",
            r#"{"crate":"","store_mode":"passthrough","kind":"passthrough","total_ms":7}"#, "\n",
            r#"{"crate":"","store_mode":"passthrough","kind":"passthrough","total_ms":5}"#, "\n",
            r#"{"crate":"winapi","store_mode":"skip:lib.rs changed during the compile","kind":"store"}"#, "\n",
            r#"{"crate":"serde","store_mode":"stored:copied=0","kind":"store"}"#, "\n",
            r#"{"crate":"syn","store_mode":"exists","kind":"store"}"#, "\n",
        );
        let records = parse_timings(text);
        let mut summary = TimingSummary::default();
        summary.add_build(&records, 0);
        assert_eq!((summary.units, summary.miss_units, summary.passthrough_spawns, summary.passthrough_ms), (3, 1, 2, 12));
        assert_eq!(summary.restore_paths, BTreeMap::from([("copy".to_string(), 1), ("kept".to_string(), 2), ("link".to_string(), 3)]));
        assert_eq!(summary.restore_fallbacks, ["copy:link:os32;adopt:link:os32", "copy:store-nlink=2;adopt:store-nlink=2"]);
        assert_eq!(summary.miss_reasons, ["winapi: input:lib.rs"]);
        assert_eq!(summary.misses[0].miss_reason, "input:lib.rs");
        assert_eq!(summary.store_outcomes, BTreeMap::from([("exists".to_string(), 1), ("skip".to_string(), 1), ("stored".to_string(), 1)]));
        assert_eq!(summary.store_problems, ["winapi: skip:lib.rs changed during the compile"]);
        assert_eq!(restore_text("clean_rebuild", &records[0]), "clean_rebuild: copy 1, link 2 (copy:link:os32;adopt:link:os32)");
        let lines = sharing_lines(&BTreeMap::from([("clean_rebuild".to_string(), summary)]));
        assert_eq!(lines.len(), 4);
        assert!(lines[0].starts_with("clean rebuild: restored files by path: copy 1, kept 2, link 3; link fallbacks: copy:link:os32"));
        assert!(lines[3].contains("2 uncached compiler calls, 12 ms"));
    }

    #[test]
    fn cold_store_mode_line_groups_reasons_by_kind() {
        let mut cold = TimingSummary::default();
        cold.add_build(&[
            line("a", 1, "deferred"), line("b", 1, "deferred"), line("c", 1, "deferred"), line("d", 1, "inline:error"),
            line("e", 1, "inline:slots-busy"), line("f", 1, "inline:slots-busy"), line("g", 1, "queued"), line("h", 1, "hit"),
        ], 0);
        assert_eq!(store_mode_line("linux", Some(&cold)), "Cold row store modes on linux: deferred 3, hit 1, inline 3, queued 1.");
        assert_eq!(store_mode_line("windows", None), "Cold row store modes on windows: no records.");
    }

    #[test]
    fn gate_checks_report_the_spread_of_per_pair_ratios() {
        let cargo = Samples { cold: vec![100.0; 3], clean: vec![100.0], noop: vec![20.0], first: vec![], warm: vec![] };
        let frust = Samples { cold: vec![90.0, 100.0, 130.0], clean: vec![40.0], noop: vec![20.0], first: vec![], warm: vec![] };
        let cold = evaluate_gate_for(&cargo, &frust, false).into_iter().find(|c| c.name == "cold").unwrap();
        assert_eq!((cold.ratio, cold.ratio_min, cold.ratio_max), (1.0, Some(0.9), Some(1.3)));
    }

    #[test]
    fn timing_table_lists_only_recorded_frust_rows_in_display_order() {
        let mut timings = BTreeMap::new();
        let mut cold = TimingSummary::default();
        cold.add_build(&[], 0);
        timings.insert("cold".to_string(), cold);
        timings.insert("edit_warm".to_string(), TimingSummary::default());
        let rows = timing_rows(&timings);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].len(), TIMING_HEADER.len());
        assert_eq!(rows[1][0], "cold");
        assert_eq!(rows[1][18], "none");
        assert_eq!(timing_rows(&BTreeMap::new()).len(), 1);
        assert!(markdown_table(&rows).lines().nth(1).unwrap().starts_with("| --- |"));
    }

    #[test]
    fn percentile_uses_nearest_rank_and_median_averages_the_middle() {
        let ten: Vec<f64> = (1..=10).map(f64::from).collect();
        assert_eq!(percentile(&ten, 90.0), Some(9.0));
        assert_eq!(percentile(&ten, 100.0), Some(10.0));
        assert_eq!(percentile(&ten, 1.0), Some(1.0));
        assert_eq!(percentile(&[7.0], 90.0), Some(7.0));
        assert_eq!(percentile(&[9.0, 1.0, 5.0], 90.0), Some(9.0));
        assert_eq!(percentile(&[], 90.0), None);
        assert_eq!(median_of(&[4.0, 1.0, 3.0, 2.0]), Some(2.5));
        assert_eq!(median_of(&[3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median_of(&[]), None);
    }

    #[test]
    fn median_ratio_pairs_runs_and_skips_zero_cargo_times() {
        assert_eq!(median_ratio(&[10.0, 20.0, 40.0], &[20.0, 20.0, 20.0]), Some(1.0));
        assert_eq!(median_ratio(&[0.0, 10.0], &[5.0, 15.0]), Some(1.5));
        assert_eq!(median_ratio(&[], &[]), None);
    }

    #[test]
    fn markdown_rows_format_times_sizes_and_the_gate_column() {
        let tool = Samples { cold: vec![2000.0], clean: vec![500.0], noop: vec![20.0], first: vec![400.0], warm: vec![100.0, 200.0, 300.0] };
        let report = tool.report(1024 * 1024);
        assert_eq!(
            markdown_row(&table_cells("cargo", &report, None)),
            "| cargo | 2.00 s | 500 ms | 20 ms | 400 ms | 200 ms | 300 ms | 1.0 MiB |"
        );
        assert_eq!(
            markdown_row(&table_cells("frust", &report, Some("pass"))),
            "| frust | 2.00 s | 500 ms | 20 ms | 400 ms | 200 ms | 300 ms | 1.0 MiB | pass |"
        );
        let ratios = paired_ratios(&report, &report);
        assert_eq!(
            markdown_row(&ratio_cells(&ratios, Some("-"))),
            "| frust / cargo | 1.00x | 1.00x | 1.00x | 1.00x | 1.00x | 1.00x | 1.00x | - |"
        );
        let empty = Samples::default().report(0);
        assert!(markdown_row(&table_cells("x", &empty, None)).contains("n/a"));
    }

    #[test]
    fn markdown_report_has_a_gate_column_only_when_gated() {
        let cargo = samples(1000.0, 1000.0, 20.0, 100.0);
        let frust = samples(1000.0, 400.0, 22.0, 100.0);
        let plain = repo_markdown(&report_with(&cargo, &frust, (100, 80), false));
        assert!(plain.contains("| tool | cold | clean rebuild | no-op | first edit | warm edit | edit p90 | disk |\n"));
        assert!(plain.contains("| frust / cargo |"));
        assert!(!plain.contains("gate"));
        let gated = repo_markdown(&report_with(&cargo, &frust, (100, 80), true));
        assert!(gated.contains("| disk | gate |"));
        assert!(gated.contains("| pass |"));
        let failed = repo_markdown(&report_with(&cargo, &frust, (100, 200), true));
        assert!(failed.contains("fail: disk"));
        assert!(failed.contains("disk guard"));
    }

    #[test]
    fn disk_verdict_fails_only_when_frust_is_larger() {
        assert!(disk_verdict(100, 100, 0.0).is_ok());
        assert!(disk_verdict(100, 40, 0.0).is_ok());
        assert!(disk_verdict(100, 101, 0.0).is_err());
        let error = disk_verdict(1024 * 1024, 2 * 1024 * 1024, 0.0).unwrap_err();
        assert!(error.contains("2.0 MiB") && error.contains("1.0 MiB"), "{error}");
    }

    #[test]
    fn disk_slack_widens_the_allowance_by_a_percentage() {
        assert!(disk_verdict(1000, 1100, 10.0).is_ok());
        assert!(disk_verdict(1000, 1101, 10.0).is_err());
        assert!(disk_verdict(1000, 1000, 10.0).is_ok());
    }

    #[test]
    fn gate_passes_when_every_threshold_holds() {
        let cargo = samples(1000.0, 1000.0, 20.0, 100.0);
        let frust = samples(1040.0, 400.0, 24.0, 110.0);
        let checks = evaluate_gate_for(&cargo, &frust, false);
        let names: Vec<&str> = checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["cold", "clean-rebuild", "noop-slack", "noop-ratio", "edit-median", "edit-p90"]);
        assert!(failing(&checks).is_empty(), "{checks:?}");
    }

    #[test]
    fn gate_flags_each_regression_separately() {
        let cargo = samples(1000.0, 1000.0, 20.0, 100.0);
        let gate = |frust: &Samples| evaluate_gate_for(&cargo, frust, false);
        assert_eq!(failing(&gate(&samples(1300.0, 400.0, 20.0, 100.0))), ["cold"]);
        assert_eq!(failing(&gate(&samples(1000.0, 700.0, 20.0, 100.0))), ["clean-rebuild"]);
        assert_eq!(failing(&gate(&samples(1000.0, 400.0, 40.0, 100.0))), ["noop-slack", "noop-ratio"]);
        assert_eq!(failing(&gate(&samples(1000.0, 400.0, 24.0, 100.0))), Vec::<&str>::new());
        assert_eq!(failing(&gate(&samples(1000.0, 400.0, 20.0, 140.0))), ["edit-median"]);
        assert_eq!(failing(&gate(&samples(1000.0, 400.0, 20.0, 160.0))), ["edit-median", "edit-p90"]);
        assert_eq!(failing(&gate(&samples(1000.0, 400.0, 20.0, 120.0))), Vec::<&str>::new());
    }

    #[test]
    fn gate_floors_absorb_small_absolute_differences_on_short_builds() {
        let cargo = samples(200.0, 100.0, 20.0, 50.0);
        let frust = samples(340.0, 140.0, 20.0, 70.0);
        assert!(failing(&evaluate_gate_for(&cargo, &frust, false)).is_empty());
        let slower = samples(380.0, 170.0, 20.0, 100.0);
        assert_eq!(failing(&evaluate_gate_for(&cargo, &slower, false)), ["cold", "clean-rebuild", "edit-median", "edit-p90"]);
    }

    #[test]
    fn noop_slack_is_wider_on_windows_only() {
        let cargo = samples(1000.0, 1000.0, 20.0, 100.0);
        let frust = samples(1000.0, 400.0, 38.0, 100.0);
        assert_eq!(failing(&evaluate_gate_for(&cargo, &frust, false)), ["noop-slack", "noop-ratio"]);
        assert_eq!(failing(&evaluate_gate_for(&cargo, &frust, true)), ["noop-ratio"]);
    }

    #[test]
    fn gate_uses_the_median_of_paired_ratios_not_pooled_medians() {
        let cargo = Samples { cold: vec![1000.0, 2000.0, 3000.0], clean: vec![1.0], noop: vec![20.0], first: vec![], warm: vec![] };
        let frust = Samples { cold: vec![3000.0, 2500.0, 3100.0], clean: vec![1.0], noop: vec![20.0], first: vec![], warm: vec![] };
        let cold = evaluate_gate_for(&cargo, &frust, false).into_iter().find(|c| c.name == "cold").unwrap();
        assert_eq!(cold.ratio, 1.25);
        assert!(!cold.pass);
    }

    #[test]
    fn gate_skips_edit_checks_without_warm_samples() {
        let mut cargo = samples(1000.0, 1000.0, 20.0, 100.0);
        let mut frust = cargo.clone();
        cargo.warm.clear();
        frust.warm.clear();
        let names: Vec<String> = evaluate_gate_for(&cargo, &frust, false).into_iter().map(|c| c.name).collect();
        assert!(!names.iter().any(|n| n.starts_with("edit")));
    }

    #[test]
    fn exit_codes_rank_disk_and_build_failures_above_the_gate() {
        let cargo = samples(1000.0, 1000.0, 20.0, 100.0);
        let good = samples(1000.0, 400.0, 20.0, 100.0);
        let slow = samples(2000.0, 400.0, 20.0, 100.0);
        assert_eq!(exit_code(&report_with(&cargo, &good, (100, 80), true)), 0);
        assert_eq!(exit_code(&report_with(&cargo, &slow, (100, 80), true)), 2);
        assert_eq!(exit_code(&report_with(&cargo, &slow, (100, 80), false)), 0);
        assert_eq!(exit_code(&report_with(&cargo, &good, (100, 101), false)), 1);
        assert_eq!(exit_code(&report_with(&cargo, &slow, (100, 101), true)), 1);
    }

    #[test]
    fn edits_append_a_unique_marker_to_the_original_bytes() {
        let first = edit_source(b"fn main() {}", 1);
        let second = edit_source(b"fn main() {}", 2);
        assert!(first.starts_with(b"fn main() {}\n"));
        assert_ne!(first, second);
        assert!(String::from_utf8(second).unwrap().contains("FRUST_BENCH_EDIT: usize = 2;"));
    }

    #[test]
    fn backup_restores_original_bytes_on_restore_and_on_drop() {
        let dir = std::env::temp_dir().join(format!("frust-bench-test-repo-backup-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("lib.rs");
        let original = b"pub fn a() {}\r\n".to_vec();
        fs::write(&file, &original).unwrap();
        {
            let backup = EditBackup::create(&dir.join("saved"), std::slice::from_ref(&file)).unwrap();
            backup.apply(0, 1).unwrap();
            assert_ne!(fs::read(&file).unwrap(), original);
            backup.restore().unwrap();
            assert_eq!(fs::read(&file).unwrap(), original);
            backup.apply(0, 2).unwrap();
            assert_ne!(fs::read(&file).unwrap(), original);
        }
        assert_eq!(fs::read(&file).unwrap(), original);
        let _ = fs::remove_dir_all(&dir);
    }
}
