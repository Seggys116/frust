//! Real-repository benchmark: paired cargo and frust builds of an external checkout.

use super::*;
use std::collections::HashSet;

const MAX_EDIT_FILES: usize = 8;
const DEFAULT_EDITS: usize = 20;
const DEFAULT_REPO_RUNS: usize = 3;
const NOOP_REPEATS: usize = 3;
const QUIESCE_TIMEOUT: Duration = Duration::from_secs(60);
const REPORT_SCHEMA_VERSION: u32 = 1;

const GATE_NOOP_SLACK_MS: f64 = 15.0;
const GATE_NOOP_RATIO: f64 = 1.25;
const GATE_EDIT_RATIO: f64 = 1.05;
const GATE_EDIT_SLACK_MS: f64 = 10.0;
const GATE_EDIT_P90_RATIO: f64 = 1.15;
const GATE_COLD_RATIO: f64 = 1.05;
const GATE_CLEAN_RATIO: f64 = 0.5;

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
                "--repo" | "--edits" | "--runs" | "--package" | "--out" => {
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
                        _ => out = Some(PathBuf::from(value)),
                    }
                }
                _ => return Err(format!("`{flag}` is not supported together with --repo")),
            }
            index += 1;
        }
        let repo = repo.ok_or("--repo PATH is required")?;
        Ok(Self { repo, edits, runs, package, json, markdown, out, gate })
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
    frust_bytes: u64,
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
struct DiskBreakdown {
    cargo_physical_bytes: u64,
    cargo_logical_bytes: u64,
    frust_physical_bytes: u64,
    frust_logical_bytes: u64,
    frust_dirs: Vec<DirSize>,
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

fn count_cache_entries(store: &Path) -> usize {
    let Ok(shards) = fs::read_dir(store) else { return 0 };
    shards.flatten().filter_map(|shard| fs::read_dir(shard.path()).ok())
        .flat_map(|entries| entries.flatten())
        .filter(|entry| entry.path().join("meta.json").is_file())
        .count()
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
    Ok(build_breakdown(cargo_physical, cargo_logical, cumulative, logical, count_cache_entries(&store)))
}

#[derive(Debug, Serialize)]
struct GateCheck {
    name: String,
    ratio: f64,
    cargo_ms: f64,
    frust_ms: f64,
    limit: String,
    pass: bool,
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

fn median_ratio(cargo: &[f64], frust: &[f64]) -> Option<f64> {
    let ratios: Vec<f64> = cargo.iter().zip(frust).filter(|(c, _)| **c > 0.0).map(|(c, f)| f / c).collect();
    median_of(&ratios)
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
    }
}

fn evaluate_gate(cargo: &Samples, frust: &Samples) -> Vec<GateCheck> {
    let mut checks = Vec::new();
    let paired = |c: &[f64], f: &[f64]| median_ratio(c, f).zip(median_of(c));
    if let Some((ratio, base)) = paired(&cargo.cold, &frust.cold) {
        checks.push(ratio_check("cold", ratio, base, GATE_COLD_RATIO, 0.0));
    }
    if let Some((ratio, base)) = paired(&cargo.clean, &frust.clean) {
        checks.push(ratio_check("clean-rebuild", ratio, base, GATE_CLEAN_RATIO, 0.0));
    }
    if let Some((ratio, base)) = paired(&cargo.noop, &frust.noop) {
        checks.push(ratio_check("noop-slack", ratio, base, 1.0, GATE_NOOP_SLACK_MS));
        checks.push(ratio_check("noop-ratio", ratio, base, GATE_NOOP_RATIO, 0.0));
    }
    if let Some((ratio, base)) = paired(&cargo.warm, &frust.warm) {
        checks.push(ratio_check("edit-median", ratio, base, GATE_EDIT_RATIO, GATE_EDIT_SLACK_MS));
    }
    if let (Some(base), Some(measured)) = (percentile(&cargo.warm, 90.0), percentile(&frust.warm, 90.0))
        && base > 0.0
    {
        let mut check = ratio_check("edit-p90", measured / base, base, GATE_EDIT_P90_RATIO, 0.0);
        check.frust_ms = measured;
        checks.push(check);
    }
    checks
}

fn disk_verdict(cargo_bytes: u64, frust_bytes: u64) -> Result<(), String> {
    if frust_bytes > cargo_bytes {
        return Err(format!(
            "disk guard: frust uses {} in total, {} more than cargo's {}",
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
    vec![
        format!("Disk, physical: cargo target {}; frust total {} ({}).",
            human_bytes(b.cargo_physical_bytes), human_bytes(b.frust_physical_bytes), parts(true)),
        format!("Disk, logical: cargo target {}; frust total {} ({}).",
            human_bytes(b.cargo_logical_bytes), human_bytes(b.frust_logical_bytes), parts(false)),
        format!("Targets only: frust central targets {} against cargo target {} ({}).",
            human_bytes(b.targets_only_physical_bytes), human_bytes(b.cargo_physical_bytes), fmt_ratio(targets_ratio)),
        format!("Cache: {} entries; {} of the store is shared with targets ({}).",
            b.cache_entries, human_bytes(b.store_shared_with_targets_bytes), share),
    ]
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
    out.push_str(&format!("\n{EDIT_NOTE}\n"));
    for line in report.disk_breakdown.iter().flat_map(breakdown_lines) {
        out.push_str(&format!("\n- {line}"));
    }
    out.push('\n');
    if let Some(message) = &report.disk.message {
        out.push_str(&format!("\n{message}\n"));
    }
    for line in gate_failure_lines(report) {
        out.push_str(&format!("\n- {line}"));
    }
    out.push('\n');
    out
}

fn print_repo(report: &RepoReport) {
    let rows = table_rows(report);
    let widths: Vec<usize> = (0..rows[0].len()).map(|c| rows.iter().map(|r| r[c].len()).max().unwrap_or(0)).collect();
    println!("{} (rustc {}, {} runs, {} edits per run)", report.repo, report.rustc, report.runs, report.edits);
    for row in &rows {
        let line: Vec<String> = row.iter().zip(&widths).map(|(cell, &width)| format!("{cell:<width$}")).collect();
        println!("  {}", line.join("  ").trim_end());
    }
    println!("{EDIT_NOTE}");
    for line in report.disk_breakdown.iter().flat_map(breakdown_lines) {
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

fn build_args(packages: &[RepoPackage], package: Option<&str>) -> Vec<String> {
    if package.is_none() {
        return vec!["--workspace".to_string()];
    }
    packages.iter().flat_map(|p| ["-p".to_string(), p.spec.clone()]).collect()
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
}

impl Rig<'_> {
    // A background store from the previous build must not run during the next timed row.
    fn quiesce(&self) {
        if let Err(error) = drain_deferred_stores(&self.ctx.home, QUIESCE_TIMEOUT) {
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

    fn build(&self, frust: bool, label: &str) -> Result<f64, String> {
        if frust {
            self.quiesce();
        }
        let args: Vec<&str> = self.args.iter().map(String::as_str).collect();
        let target = (!frust).then_some(self.cargo_target.as_path());
        let mut cmd = build_command(self.ctx, frust, self.repo, target, &args, "off");
        cmd.env("CARGO_NET_OFFLINE", "false");
        let tool = if frust { "frust" } else { "cargo" };
        timed(self.ctx, frust, &mut cmd).map(|run| run.millis as f64).map_err(|e| format!("{label} with {tool}: {e}"))
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
            samples[usize::from(frust)].cold.push(rig.build(frust, "cold build")?);
        }
        for frust in order {
            rig.reset(frust, false)?;
            samples[usize::from(frust)].clean.push(rig.build(frust, "clean rebuild")?);
        }
        for frust in order {
            let mut repeats = Vec::new();
            for _ in 0..NOOP_REPEATS {
                repeats.push(rig.build(frust, "no-op build")?);
            }
            samples[usize::from(frust)].noop.extend(median_of(&repeats));
        }
        let mut touched = HashSet::new();
        for edit in 0..opts.edits {
            let slot = edit % owners.len();
            backup.apply(slot, edit + 1)?;
            let first = touched.insert(owners[slot]);
            for frust in tool_order(run + edit) {
                let ms = rig.build(frust, "edit build")?;
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
        args: build_args(&packages, opts.package.as_deref()),
    };
    let samples = measure(&rig, &backup, &owners, opts)?;
    rig.quiesce();
    let cargo_bytes = compact::physical_size(&[rig.cargo_target.as_path()]);
    let frust_bytes = compact::physical_size(&[ctx.home.as_path()]);
    let verdict = disk_verdict(cargo_bytes, frust_bytes);
    let disk_breakdown = measure_disk_breakdown(&rig.dirs, &rig.cargo_target, cargo_bytes)
        .inspect_err(|error| eprintln!("frust bench: warning: disk breakdown unavailable: {error}")).ok();
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
        disk: DiskReport { cargo_bytes, frust_bytes, ok: verdict.is_ok(), message: verdict.err() },
        disk_breakdown,
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
        let verdict = disk_verdict(disk.0, disk.1);
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
            disk: DiskReport { cargo_bytes: disk.0, frust_bytes: disk.1, ok: verdict.is_ok(), message: verdict.err() },
            disk_breakdown: None,
            gate: gate.then(|| evaluate_gate(cargo, frust)),
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
            json: false, markdown: false, out: None, gate: false,
        });
        let full = RepoOptions::parse(&flags(&[
            "--repo=/work/tokio", "--edits", "5", "--runs=2", "--package", "tokio",
            "--json", "--markdown", "--out", "r.json", "--gate",
        ])).unwrap();
        assert_eq!(full, RepoOptions {
            repo: PathBuf::from("/work/tokio"), edits: 5, runs: 2, package: Some("tokio".into()),
            json: true, markdown: true, out: Some(PathBuf::from("r.json")), gate: true,
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
        assert_eq!(build_args(&all, None), ["--workspace"]);
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
            assert_eq!(build_args(&found, Some(wanted)), ["-p", "path+file:///w/tokio#tokio@1.40.0"]);
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
        assert!(disk_verdict(100, 100).is_ok());
        assert!(disk_verdict(100, 40).is_ok());
        let error = disk_verdict(1024 * 1024, 2 * 1024 * 1024).unwrap_err();
        assert!(error.contains("2.0 MiB") && error.contains("1.0 MiB"), "{error}");
    }

    #[test]
    fn gate_passes_when_every_threshold_holds() {
        let cargo = samples(1000.0, 1000.0, 20.0, 100.0);
        let frust = samples(1040.0, 400.0, 24.0, 110.0);
        let checks = evaluate_gate(&cargo, &frust);
        let names: Vec<&str> = checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["cold", "clean-rebuild", "noop-slack", "noop-ratio", "edit-median", "edit-p90"]);
        assert!(failing(&checks).is_empty(), "{checks:?}");
    }

    #[test]
    fn gate_flags_each_regression_separately() {
        let cargo = samples(1000.0, 1000.0, 20.0, 100.0);
        assert_eq!(failing(&evaluate_gate(&cargo, &samples(1100.0, 400.0, 20.0, 100.0))), ["cold"]);
        assert_eq!(failing(&evaluate_gate(&cargo, &samples(1000.0, 600.0, 20.0, 100.0))), ["clean-rebuild"]);
        assert_eq!(failing(&evaluate_gate(&cargo, &samples(1000.0, 400.0, 40.0, 100.0))), ["noop-slack", "noop-ratio"]);
        assert_eq!(failing(&evaluate_gate(&cargo, &samples(1000.0, 400.0, 24.0, 100.0))), Vec::<&str>::new());
        assert_eq!(failing(&evaluate_gate(&cargo, &samples(1000.0, 400.0, 20.0, 116.0))), ["edit-median", "edit-p90"]);
        assert_eq!(failing(&evaluate_gate(&cargo, &samples(1000.0, 400.0, 20.0, 114.0))), Vec::<&str>::new());
    }

    #[test]
    fn gate_uses_the_median_of_paired_ratios_not_pooled_medians() {
        let cargo = Samples { cold: vec![10.0, 20.0, 30.0], clean: vec![1.0], noop: vec![20.0], first: vec![], warm: vec![] };
        let frust = Samples { cold: vec![30.0, 25.0, 31.0], clean: vec![1.0], noop: vec![20.0], first: vec![], warm: vec![] };
        let cold = evaluate_gate(&cargo, &frust).into_iter().find(|c| c.name == "cold").unwrap();
        assert_eq!(cold.ratio, 1.25);
        assert!(!cold.pass);
    }

    #[test]
    fn gate_skips_edit_checks_without_warm_samples() {
        let mut cargo = samples(1000.0, 1000.0, 20.0, 100.0);
        let mut frust = cargo.clone();
        cargo.warm.clear();
        frust.warm.clear();
        let names: Vec<String> = evaluate_gate(&cargo, &frust).into_iter().map(|c| c.name).collect();
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
