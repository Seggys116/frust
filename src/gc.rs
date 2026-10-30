//! Every removal is safe because cargo and the compile cache rebuild whatever is missing.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::cache;
use crate::compact::{self, Category};
use crate::config::Config;
use crate::fsx;
use crate::paths::Dirs;
use crate::stats;

/// Younger manifests and lock directories are left alone even if they look orphaned: a build may be writing them.
const METADATA_GRACE: Duration = Duration::from_secs(3600);
const AUTO_GC_EVERY: Duration = Duration::from_secs(86_400);

#[derive(Debug, Clone)]
pub struct GcOptions {
    pub max_cache_bytes: u64,
    pub target_max_idle: Duration,
    pub variant_max_age: Duration,
    pub variants_to_keep: usize,
    pub manifest_max_age: Duration,
    pub lock_max_idle: Duration,
    pub history_max_age: Duration,
    pub hashmemo_max_age: Duration,
    pub startup_memo_max_age: Duration,
    pub temp_min_age: Duration,
    /// Older temp files go even if their creator's pid exists, because pids are reused.
    pub temp_max_age: Duration,
}

impl GcOptions {
    pub fn new(max_cache_bytes: u64) -> GcOptions {
        GcOptions {
            max_cache_bytes,
            target_max_idle: Duration::from_secs(30 * 86_400),
            variant_max_age: Duration::from_secs(7 * 86_400),
            variants_to_keep: 2,
            manifest_max_age: Duration::from_secs(30 * 86_400),
            lock_max_idle: Duration::from_secs(7 * 86_400),
            history_max_age: Duration::from_secs(90 * 86_400),
            hashmemo_max_age: Duration::from_secs(30 * 86_400),
            startup_memo_max_age: Duration::from_secs(30 * 86_400),
            temp_min_age: Duration::from_secs(3600),
            temp_max_age: Duration::from_secs(86_400),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    CacheEntry,
    TargetDir,
    StaleVariant,
    IncrementalSession,
    Scratch,
    Metadata,
    Ledger,
}

impl Class {
    pub const ALL: [Class; 7] = [
        Class::CacheEntry,
        Class::TargetDir,
        Class::StaleVariant,
        Class::IncrementalSession,
        Class::Scratch,
        Class::Metadata,
        Class::Ledger,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Class::CacheEntry => "cache entries over budget or superseded",
            Class::TargetDir => "idle or orphaned target directories",
            Class::StaleVariant => "superseded build artifacts",
            Class::IncrementalSession => "stale incremental state",
            Class::Scratch => "scratch and leftover temp files",
            Class::Metadata => "stale manifests, locks, history and hash memos",
            Class::Ledger => "ledger trimming",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Removal {
    pub class: Class,
    pub path: PathBuf,
    pub bytes: u64,
}

#[derive(Debug, Default)]
pub struct Plan {
    pub removals: Vec<Removal>,
    pub trim_ledger: bool,
    /// Lock directories are rechecked just before removal: a build may have started using one since planning.
    pub lock_max_idle: Duration,
}

impl Plan {
    pub fn bytes(&self) -> u64 {
        self.removals.iter().map(|r| r.bytes).sum()
    }

    pub fn by_class(&self, class: Class) -> (usize, u64) {
        self.removals.iter().filter(|r| r.class == class).fold((0, 0), |(n, b), r| (n + 1, b + r.bytes))
    }

    pub fn target_categories(&self) -> Vec<(Category, u64)> {
        let mut by: std::collections::BTreeMap<Category, u64> = std::collections::BTreeMap::new();
        for r in self.removals.iter().filter(|r| matches!(r.class, Class::StaleVariant | Class::IncrementalSession)) {
            let Some(profile) = compact::profile_of(&r.path) else { continue };
            let rel = r.path.strip_prefix(&profile).unwrap_or(&r.path);
            *by.entry(Category::of(rel)).or_default() += r.bytes;
        }
        by.into_iter().collect()
    }
}

pub fn plan(dirs: &Dirs, opts: &GcOptions) -> Plan {
    let mut plan = Plan { lock_max_idle: opts.lock_max_idle, ..Plan::default() };
    let evicted = plan_cache(dirs, opts, &mut plan);
    plan_targets(dirs, opts, &mut plan);
    plan_scratch(dirs, &mut plan);
    plan_store_temps(dirs, opts, &mut plan);
    plan_metadata(dirs, opts, &evicted, &mut plan);
    plan
}

pub fn plan_maintenance(dirs: &Dirs, opts: &GcOptions) -> Plan {
    let mut plan = Plan { lock_max_idle: opts.lock_max_idle, ..Plan::default() };
    let evicted = plan_cache(dirs, opts, &mut plan);
    plan_scratch(dirs, &mut plan);
    plan_store_temps(dirs, opts, &mut plan);
    plan_target_temps(dirs, opts, &mut plan);
    plan_metadata(dirs, opts, &evicted, &mut plan);
    plan
}

/// The current source state and one other, so switching between two branches stays warm.
pub const INCREMENTAL_ENTRIES_KEPT: usize = 2;

/// Incremental objects are `<crate>-<hash>.<cgu>.<tag>.rcgu.o`; non-incremental ones contain `-cgu.`.
fn is_incremental_object(name: &str) -> bool {
    name.ends_with(".rcgu.o") && name.split('.').count() == 5 && !name.contains("-cgu.")
}

/// Every edit of a path crate stores a new entry under the same output names, so unbounded they bloat the cache; where rustc keeps no objects beside outputs (outside macOS) nothing is recognised.
pub fn superseded_incremental_entries(dirs: &Dirs, keep: usize) -> Vec<(PathBuf, u64)> {
    let mut units: HashMap<Vec<String>, Vec<(SystemTime, PathBuf)>> = HashMap::new();
    for entry in stats::entry_dirs(dirs) {
        let Ok(bytes) = fs::read(entry.path.join("meta.json")) else { continue };
        let Ok(meta) = serde_json::from_slice::<cache::EntryMeta>(&bytes) else { continue };
        if !meta.files.iter().any(|f| is_incremental_object(&f.name)) {
            continue;
        }
        let mut unit: Vec<String> =
            meta.files.iter().map(|f| f.name.clone()).filter(|n| !n.ends_with(".o") && !n.ends_with(".d")).collect();
        if unit.is_empty() {
            continue;
        }
        unit.sort();
        units.entry(unit).or_default().push((entry.used, entry.path));
    }
    let mut out = Vec::new();
    for (_, mut entries) in units {
        entries.sort_by_key(|e| std::cmp::Reverse(e.0));
        for (_, path) in entries.into_iter().skip(keep) {
            let bytes = fsx::allocated_size(&path);
            out.push((path, bytes));
        }
    }
    out
}

fn plan_cache(dirs: &Dirs, opts: &GcOptions, plan: &mut Plan) -> HashSet<String> {
    let mut evicted = HashSet::new();
    let superseded = superseded_incremental_entries(dirs, INCREMENTAL_ENTRIES_KEPT);
    let doomed: HashSet<&Path> = superseded.iter().map(|s| s.0.as_path()).collect();
    for (path, _) in &superseded {
        if let Some(key) = path.file_name() {
            evicted.insert(key.to_string_lossy().into_owned());
        }
    }
    let mut entries: Vec<(SystemTime, PathBuf, u64)> = stats::entry_dirs(dirs)
        .into_iter()
        .filter(|e| !doomed.contains(e.path.as_path()))
        .map(|e| {
            let bytes = fsx::allocated_size(&e.path);
            (e.used, e.path, bytes)
        })
        .collect();
    for (path, bytes) in superseded {
        plan.removals.push(Removal { class: Class::CacheEntry, path, bytes });
    }
    let mut total: u64 = entries.iter().map(|e| e.2).sum();
    entries.sort_by_key(|e| e.0);
    for (_, path, bytes) in entries {
        if total <= opts.max_cache_bytes {
            break;
        }
        total = total.saturating_sub(bytes);
        if let Some(key) = path.file_name() {
            evicted.insert(key.to_string_lossy().into_owned());
        }
        plan.removals.push(Removal { class: Class::CacheEntry, path, bytes });
    }
    evicted
}

fn plan_targets(dirs: &Dirs, opts: &GcOptions, plan: &mut Plan) {
    for target in stats::targets(dirs) {
        if !target.root_exists || target.idle > opts.target_max_idle {
            plan.removals.push(Removal { class: Class::TargetDir, path: target.path, bytes: target.allocated_bytes });
            continue;
        }
        let Ok(profiles) = fs::read_dir(&target.path) else { continue };
        for profile in profiles.flatten() {
            plan_profile(&profile.path(), opts, plan);
        }
    }
}

fn plan_profile(dir: &Path, opts: &GcOptions, plan: &mut Plan) {
    let deps = dir.join("deps");
    if deps.is_dir() {
        let analysis = compact::analyze(dir);
        if let Some(a) = &analysis {
            for (path, bytes) in &a.removals {
                plan.removals.push(Removal { class: Class::StaleVariant, path: path.clone(), bytes: *bytes });
            }
        }
        plan_variants(dir, &deps, opts, plan, analysis.as_ref());
        plan_incremental(dir, analysis.as_ref(), plan);
        plan_profile_temps(dir, opts, plan);
        return;
    }
    if let Ok(children) = fs::read_dir(dir) {
        for child in children.flatten() {
            if child.path().join("deps").is_dir() {
                plan_profile(&child.path(), opts, plan);
            }
        }
    }
}

fn variant_of(file_name: &str) -> Option<(String, String)> {
    let stem = file_name.split('.').next()?;
    let (name, hash) = stem.rsplit_once('-')?;
    if hash.len() != 16 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some((name.strip_prefix("lib").unwrap_or(name).to_string(), hash.to_string()))
}

/// With an analysis, only files whose hash no fingerprint knows are aged; known units are the analysis's job.
fn plan_variants(profile: &Path, deps: &Path, opts: &GcOptions, plan: &mut Plan, analysis: Option<&compact::Analysis>) {
    #[derive(Default)]
    struct Variant {
        files: Vec<(PathBuf, u64)>,
        newest: Option<SystemTime>,
    }
    let mut by_crate: HashMap<String, HashMap<String, Variant>> = HashMap::new();
    let Ok(entries) = fs::read_dir(deps) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some((krate, hash)) = variant_of(&name) else { continue };
        if analysis.is_some_and(|a| a.known.contains(&hash)) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let variant = by_crate.entry(krate).or_default().entry(hash).or_default();
        let mtime = meta.modified().ok();
        variant.newest = variant.newest.max(mtime);
        variant.files.push((entry.path(), fsx::allocated_size(&entry.path())));
    }

    let fingerprints = profile.join(".fingerprint");
    for (_, variants) in by_crate {
        let mut ordered: Vec<(String, Variant)> = variants.into_iter().collect();
        ordered.sort_by_key(|v| std::cmp::Reverse(v.1.newest));
        for (hash, variant) in ordered.into_iter().skip(opts.variants_to_keep) {
            let age = variant.newest.and_then(|t| SystemTime::now().duration_since(t).ok()).unwrap_or_default();
            if age < opts.variant_max_age {
                continue;
            }
            for (path, bytes) in variant.files {
                plan.removals.push(Removal { class: Class::StaleVariant, path, bytes });
            }
            if let Ok(prints) = fs::read_dir(&fingerprints) {
                for print in prints.flatten() {
                    if print.file_name().to_string_lossy().ends_with(&format!("-{hash}")) {
                        let bytes = fsx::allocated_size(&print.path());
                        plan.removals.push(Removal { class: Class::StaleVariant, path: print.path(), bytes });
                    }
                }
            }
        }
    }
}

fn plan_incremental(profile: &Path, analysis: Option<&compact::Analysis>, plan: &mut Plan) {
    for (path, bytes) in superseded_incremental(profile, analysis) {
        // A crate directory already going with a dead unit takes its sessions along.
        if !plan.removals.iter().any(|r| path.starts_with(&r.path)) {
            plan.removals.push(Removal { class: Class::IncrementalSession, path, bytes });
        }
    }
}

fn superseded_incremental(profile: &Path, analysis: Option<&compact::Analysis>) -> Vec<(PathBuf, u64)> {
    let mut out: Vec<(PathBuf, u64)> = Vec::new();
    if let Some(a) = analysis {
        for d in compact::incremental_dirs(profile, a) {
            if matches!(d.owner, compact::IncrementalOwner::Dead | compact::IncrementalOwner::Orphan) {
                let bytes = fsx::allocated_size(&d.path);
                out.push((d.path, bytes));
            }
        }
    }
    let sessions: Vec<(PathBuf, u64)> =
        compact::stale_sessions(profile).into_iter().filter(|(s, _)| !out.iter().any(|(p, _)| s.starts_with(p))).collect();
    out.extend(sessions);
    out.extend(compact::stale_objects(profile));
    out
}

fn plan_scratch(dirs: &Dirs, plan: &mut Plan) {
    if let Ok(entries) = fs::read_dir(dirs.tmp()) {
        for entry in entries.flatten() {
            let old = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| SystemTime::now().duration_since(t).ok())
                .is_some_and(|age| age > Duration::from_secs(3600));
            if old {
                plan.removals.push(Removal { class: Class::Scratch, bytes: fsx::allocated_size(&entry.path()), path: entry.path() });
            }
        }
    }
    plan.trim_ledger = fs::metadata(dirs.ledger()).map(|m| m.len() > 8 * 1024 * 1024).unwrap_or(false);
}

fn plan_store_temps(dirs: &Dirs, opts: &GcOptions, plan: &mut Plan) {
    fn below(dir: &Path, depth: usize, opts: &GcOptions, plan: &mut Plan) {
        plan_temps_in(dir, opts, plan);
        if depth == 0 {
            return;
        }
        if let Ok(children) = fs::read_dir(dir) {
            for child in children.flatten() {
                if child.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    below(&child.path(), depth - 1, opts, plan);
                }
            }
        }
    }
    let Ok(shards) = fs::read_dir(dirs.store()) else { return };
    for shard in shards.flatten() {
        if shard.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            below(&shard.path(), 3, opts, plan);
        }
    }
}

fn age_of(time: std::io::Result<SystemTime>) -> Option<Duration> {
    time.ok().and_then(|t| SystemTime::now().duration_since(t).ok())
}

fn mtime_age(path: &Path) -> Option<Duration> {
    age_of(fs::symlink_metadata(path).and_then(|m| m.modified()))
}

/// Unlike mtime, ctime cannot be inherited from a cloned source file, so a fresh clone never looks old.
fn ctime_age(meta: &fs::Metadata) -> Option<Duration> {
    let ctime = SystemTime::UNIX_EPOCH + Duration::new(meta.ctime().max(0) as u64, meta.ctime_nsec().clamp(0, 999_999_999) as u32);
    SystemTime::now().duration_since(ctime).ok()
}

fn stale_temp(path: &Path, name: &str, opts: &GcOptions) -> bool {
    let Some(pid) = fsx::temp_owner(name) else { return false };
    let Ok(meta) = fs::symlink_metadata(path) else { return false };
    let Some(age) = ctime_age(&meta) else { return false };
    if pid == std::process::id() {
        return false;
    }
    age > opts.temp_max_age || (age > opts.temp_min_age && !fsx::pid_alive(pid))
}

fn plan_temps_in(dir: &Path, opts: &GcOptions, plan: &mut Plan) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if stale_temp(&entry.path(), &name, opts) {
            plan.removals.push(Removal { class: Class::Scratch, bytes: fsx::allocated_size(&entry.path()), path: entry.path() });
        }
    }
}

/// Compaction rewrites files at any depth under `build/` and `incremental/`, so temps are sought there too.
fn plan_profile_temps(profile: &Path, opts: &GcOptions, plan: &mut Plan) {
    fn below(dir: &Path, depth: usize, opts: &GcOptions, plan: &mut Plan) {
        plan_temps_in(dir, opts, plan);
        if depth == 0 {
            return;
        }
        if let Ok(children) = fs::read_dir(dir) {
            for child in children.flatten() {
                if child.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    below(&child.path(), depth - 1, opts, plan);
                }
            }
        }
    }
    plan_temps_in(profile, opts, plan);
    plan_temps_in(&profile.join("deps"), opts, plan);
    plan_temps_in(&profile.join("examples"), opts, plan);
    plan_temps_in(&profile.join(".fingerprint"), opts, plan);
    if let Ok(units) = fs::read_dir(profile.join("build")) {
        for unit in units.flatten() {
            if unit.path().is_dir() {
                below(&unit.path(), 6, opts, plan);
            }
        }
    }
    below(&profile.join("incremental"), 2, opts, plan);
}

pub fn profile_temp_files(profile: &Path) -> Vec<(PathBuf, u64)> {
    let mut plan = Plan::default();
    plan_profile_temps(profile, &GcOptions::new(u64::MAX), &mut plan);
    plan.removals.into_iter().map(|r| (r.path, r.bytes)).collect()
}

fn plan_target_temps(dirs: &Dirs, opts: &GcOptions, plan: &mut Plan) {
    fn visit(dir: &Path, depth: usize, opts: &GcOptions, plan: &mut Plan) {
        if dir.join("deps").is_dir() {
            plan_profile_temps(dir, opts, plan);
            return;
        }
        if depth == 0 {
            return;
        }
        if let Ok(children) = fs::read_dir(dir) {
            for child in children.flatten() {
                if child.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    visit(&child.path(), depth - 1, opts, plan);
                }
            }
        }
    }
    let Ok(workspaces) = fs::read_dir(dirs.targets()) else { return };
    for ws in workspaces.flatten() {
        let Ok(toolchains) = fs::read_dir(ws.path()) else { continue };
        for tc in toolchains.flatten() {
            if tc.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                visit(&tc.path(), 2, opts, plan);
            }
        }
    }
}

fn plan_metadata(dirs: &Dirs, opts: &GcOptions, evicted: &HashSet<String>, plan: &mut Plan) {
    plan_manifests(dirs, opts, evicted, plan);
    plan_locks(dirs, opts, plan);
    plan_history(dirs, opts, plan);
    plan_hashmemo(dirs, opts, plan);
    plan_startup_memos(dirs, opts, plan);
}

fn plan_startup_memos(dirs: &Dirs, opts: &GcOptions, plan: &mut Plan) {
    for sub in ["probe", "locate"] {
        let Ok(entries) = fs::read_dir(dirs.root().join(sub)) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            if fsx::temp_owner(&name).is_some() {
                if stale_temp(&path, &name, opts) {
                    plan.removals.push(Removal { class: Class::Scratch, bytes: fsx::allocated_size(&path), path });
                }
                continue;
            }
            if entry.file_type().map(|t| t.is_file()).unwrap_or(false)
                && mtime_age(&path).is_some_and(|age| age > opts.startup_memo_max_age)
            {
                push_metadata(plan, path);
            }
        }
    }
}

fn push_metadata(plan: &mut Plan, path: PathBuf) {
    let bytes = fsx::allocated_size(&path);
    plan.removals.push(Removal { class: Class::Metadata, path, bytes });
}

fn sharded_files(root: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let Ok(shards) = fs::read_dir(root) else { return out };
    for shard in shards.flatten() {
        let Ok(files) = fs::read_dir(shard.path()) else { continue };
        for file in files.flatten() {
            out.push((file.file_name().to_string_lossy().into_owned(), file.path()));
        }
    }
    out
}

fn plan_manifests(dirs: &Dirs, opts: &GcOptions, evicted: &HashSet<String>, plan: &mut Plan) {
    let store = cache::Cache::new(dirs);
    for (name, path) in sharded_files(&dirs.manifests()) {
        if fsx::temp_owner(&name).is_some() {
            if stale_temp(&path, &name, opts) {
                plan.removals.push(Removal { class: Class::Scratch, bytes: fsx::allocated_size(&path), path });
            }
            continue;
        }
        let Some(age) = mtime_age(&path) else { continue };
        if age > opts.manifest_max_age {
            push_metadata(plan, path);
            continue;
        }
        if age < METADATA_GRACE {
            continue;
        }
        let orphaned = match cache::manifest_keys(&path) {
            None => true,
            Some(keys) => {
                keys.iter().all(|k| evicted.contains(k) || !store.entry_dir(k).join("meta.json").is_file())
            }
        };
        if orphaned {
            push_metadata(plan, path);
        }
    }
}

fn flock_held(path: &Path) -> Option<bool> {
    let file = File::open(path).ok()?;
    // Exclusive probe: scoped builds hold the lock shared, so a shared probe would succeed and report idle.
    match file.try_lock() {
        Ok(()) => Some(false),
        Err(fs::TryLockError::WouldBlock) => Some(true),
        Err(fs::TryLockError::Error(_)) => None,
    }
}

fn lock_dir_stale(dir: &Path, max_idle: Duration) -> bool {
    let Ok(meta) = fs::symlink_metadata(dir) else { return false };
    // SAFETY: geteuid has no preconditions.
    if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } {
        return false;
    }
    let lock = dir.join("lock");
    if lock.exists() && flock_held(&lock) != Some(false) {
        return false;
    }
    let queue = dir.join("queue");
    let mut newest = mtime_age(dir).unwrap_or_default();
    for name in ["lock", "target", "owner.json", "progress.json", "last.json", "queue"] {
        if let Some(age) = mtime_age(&dir.join(name)) {
            newest = newest.min(age);
        }
    }
    if let Ok(tickets) = fs::read_dir(&queue) {
        for ticket in tickets.flatten() {
            if flock_held(&ticket.path()) != Some(false) {
                return false;
            }
            if let Some(age) = mtime_age(&ticket.path()) {
                newest = newest.min(age);
            }
        }
    }
    let target_gone = fs::read(dir.join("target"))
        .ok()
        .map(|bytes| !Path::new(&*String::from_utf8_lossy(&bytes)).exists())
        .unwrap_or(false);
    newest > max_idle || (target_gone && newest > METADATA_GRACE)
}

fn plan_locks(dirs: &Dirs, opts: &GcOptions, plan: &mut Plan) {
    let Ok(entries) = fs::read_dir(dirs.locks()) else { return };
    for entry in entries.flatten() {
        if lock_dir_stale(&entry.path(), opts.lock_max_idle) {
            push_metadata(plan, entry.path());
        }
    }
}

fn plan_history(dirs: &Dirs, opts: &GcOptions, plan: &mut Plan) {
    let Ok(entries) = fs::read_dir(dirs.history()) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if fsx::temp_owner(&name).is_some() {
            if stale_temp(&path, &name, opts) {
                plan.removals.push(Removal { class: Class::Scratch, bytes: fsx::allocated_size(&path), path });
            }
            continue;
        }
        let Some(age) = mtime_age(&path) else { continue };
        let key = name.strip_suffix(".json").unwrap_or(&name);
        let orphaned = !dirs.targets().join(key).is_dir();
        if age > opts.history_max_age || (orphaned && age > Duration::from_secs(7 * 86_400)) {
            push_metadata(plan, path);
        }
    }
}

fn plan_hashmemo(dirs: &Dirs, opts: &GcOptions, plan: &mut Plan) {
    let units = dirs.root().join(cache::UNITS_DIR);
    for (name, path) in sharded_files(&dirs.hashmemo()).into_iter().chain(sharded_files(&units)) {
        if fsx::temp_owner(&name).is_some() {
            if stale_temp(&path, &name, opts) {
                plan.removals.push(Removal { class: Class::Scratch, bytes: fsx::allocated_size(&path), path });
            }
            continue;
        }
        if mtime_age(&path).is_some_and(|age| age > opts.hashmemo_max_age) {
            push_metadata(plan, path);
        }
    }
}

pub fn execute(dirs: &Dirs, plan: &Plan) -> (u64, Vec<(PathBuf, std::io::Error)>) {
    let (freed, failures) = execute_removals(dirs, plan);
    prune_empty_workspaces(dirs);
    (freed, failures)
}

fn execute_removals(dirs: &Dirs, plan: &Plan) -> (u64, Vec<(PathBuf, std::io::Error)>) {
    let mut freed = 0;
    let mut failures = Vec::new();
    let locks = dirs.locks();
    // Cargo's locks stay held on each profile for the rest of the run, and variants are rechecked under them.
    let mut profiles: HashMap<PathBuf, Option<ProfileGuard>> = HashMap::new();
    for removal in &plan.removals {
        if matches!(removal.class, Class::StaleVariant | Class::IncrementalSession)
            && let Some(profile) = compact::profile_of(&removal.path)
        {
            let guard = profiles.entry(profile.clone()).or_insert_with(|| ProfileGuard::acquire(&profile));
            let Some(guard) = guard else {
                failures.push((
                    removal.path.clone(),
                    std::io::Error::new(std::io::ErrorKind::WouldBlock, "a build holds this target directory; skipped"),
                ));
                continue;
            };
            if removal.class == Class::StaleVariant && !guard.still_superseded(&removal.path) {
                continue;
            }
            if removal.class == Class::IncrementalSession && !guard.incremental.contains(&removal.path) {
                continue;
            }
        }
        // A build may have started using a lock directory since planning.
        if removal.class == Class::Metadata
            && removal.path.parent() == Some(locks.as_path())
            && !lock_dir_stale(&removal.path, plan.lock_max_idle)
        {
            continue;
        }
        match fsx::remove_any(&removal.path) {
            Ok(()) => freed += removal.bytes,
            Err(err) => failures.push((removal.path.clone(), err)),
        }
    }
    if plan.trim_ledger
        && let Ok(text) = fs::read_to_string(dirs.ledger())
    {
        let keep: Vec<&str> = text.lines().rev().take(20_000).collect();
        let trimmed: String = keep.into_iter().rev().map(|l| format!("{l}\n")).collect();
        let _ = fsx::write_atomic(&dirs.ledger(), trimmed.as_bytes());
    }
    (freed, failures)
}

struct ProfileGuard {
    _lock: compact::ProfileLock,
    analysis: Option<compact::Analysis>,
    dead: HashSet<PathBuf>,
    /// A build since planning may have made a dead variant's directory live again, so planned removals must still be in here.
    incremental: HashSet<PathBuf>,
}

impl ProfileGuard {
    fn acquire(profile: &Path) -> Option<ProfileGuard> {
        let lock = compact::ProfileLock::try_acquire(profile)?;
        let analysis = compact::analyze(profile);
        let dead = analysis.as_ref().map(|a| a.removals.iter().map(|(p, _)| p.clone()).collect()).unwrap_or_default();
        let incremental = superseded_incremental(profile, analysis.as_ref()).into_iter().map(|(p, _)| p).collect();
        Some(ProfileGuard { _lock: lock, analysis, dead, incremental })
    }

    fn still_superseded(&self, path: &Path) -> bool {
        if self.dead.contains(path) {
            return true;
        }
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        match (&self.analysis, variant_of(&name)) {
            (Some(a), Some((_, hash))) => !a.known.contains(&hash),
            (Some(_), None) => false,
            (None, _) => true,
        }
    }
}

/// The marker flock lets exactly one of several concurrent processes collect, and a crashed collector releases it with its process.
pub fn maybe_auto_gc(dirs: &Dirs, cfg: &Config) -> bool {
    let marker = dirs.auto_gc_marker();
    if mtime_age(&marker).is_some_and(|age| age < AUTO_GC_EVERY) || !dirs.root().is_dir() {
        return false;
    }
    let Ok(file) = File::options().create(true).truncate(false).write(true).open(&marker) else { return false };
    if file.try_lock().is_err() {
        return false;
    }
    let due = file.metadata().and_then(|m| m.modified()).ok().and_then(|t| SystemTime::now().duration_since(t).ok());
    // A freshly created marker has a new mtime but no recorded run: its length is zero.
    let fresh_marker = file.metadata().map(|m| m.len() == 0).unwrap_or(true);
    if !fresh_marker && due.is_some_and(|age| age < AUTO_GC_EVERY) {
        return false;
    }
    let _ = file.set_len(0);
    let _ = file.write_all_at(fsx::unix_now().to_string().as_bytes(), 0);
    let _ = file.set_modified(SystemTime::now());
    let plan = plan_maintenance(dirs, &GcOptions::new(cfg.cache_max_bytes));
    let _ = execute_removals(dirs, &plan);
    true
}

fn prune_empty_workspaces(dirs: &Dirs) {
    let Ok(workspaces) = fs::read_dir(dirs.targets()) else { return };
    for ws in workspaces.flatten() {
        let has_target = fs::read_dir(ws.path()).map(|rd| rd.flatten().any(|e| e.path().is_dir())).unwrap_or(false);
        if !has_target {
            let _ = fsx::remove_any(&ws.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variant_names_parse() {
        assert_eq!(variant_of("libserde-0123456789abcdef.rlib"), Some(("serde".into(), "0123456789abcdef".into())));
        assert_eq!(variant_of("my_bin-0123456789abcdef"), Some(("my_bin".into(), "0123456789abcdef".into())));
        assert_eq!(variant_of("libserde-0123456789abcdef.rmeta"), Some(("serde".into(), "0123456789abcdef".into())));
        assert_eq!(variant_of("build-script-build"), None);
        assert_eq!(variant_of("README.md"), None);
    }
}
