//! Every removal is safe because cargo and the compile cache rebuild whatever is missing.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use crate::cache;
use crate::compact::{self, Category};
use crate::config::Config;
use crate::fsx;
use crate::lock;
use crate::paths::{DEFERRED_SLOT_PREFIX, Dirs};
use crate::stats;
use crate::sys::{self, LockKind, Meta};

/// Younger manifests and lock directories are left alone even if they look orphaned: a build may be writing them.
const METADATA_GRACE: Duration = Duration::from_secs(3600);
const AUTO_GC_EVERY: Duration = Duration::from_secs(86_400);
/// Per-prekey locks, deferred-store jobs and their slot locks older than this are removed when nothing holds them.
const DEFERRED_MAX_AGE: Duration = Duration::from_secs(86_400);
/// Hidden flag of `frust gc` that runs one daily collection and prints nothing.
pub const AUTO_CHILD_FLAG: &str = "--auto-background";
/// Room for an entry's directory and `meta.json` on top of the stored files when sizes come from the recorded byte counts.
const ENTRY_OVERHEAD_BYTES: u64 = 8192;
const AUTO_TARGET_RETIREMENTS: usize = 2;
// Foreground sampling is bounded; detached and explicitly synchronous collectors drain complete candidates.
const TARGET_SCAN_NODES: usize = 32_768;

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
    target_retirements: HashMap<PathBuf, TargetRetirement>,
    manifest_retirements: HashMap<PathBuf, ManifestRetirement>,
    retired_entries: HashMap<PathBuf, cache::RetiredChildProof>,
    unit_retirements: HashMap<PathBuf, UnitRetirement>,
    cache_retirements: HashMap<PathBuf, cache::EntryRetirementSnapshot>,
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
    let evicted = plan_cache(dirs, opts, &mut plan, true);
    plan_targets(dirs, opts, &mut plan);
    plan_scratch(dirs, &mut plan);
    plan_retired_entries(dirs, &mut plan);
    plan_store_temps(dirs, opts, &mut plan);
    plan_metadata(dirs, opts, &evicted, &mut plan);
    plan
}

pub fn plan_maintenance(dirs: &Dirs, opts: &GcOptions) -> Plan {
    let mut plan = Plan {
        lock_max_idle: opts.lock_max_idle,
        ..Plan::default()
    };
    let evicted = plan_cache(dirs, opts, &mut plan, false);
    plan_idle_targets(dirs, opts, &mut plan, AUTO_TARGET_RETIREMENTS);
    plan_scratch(dirs, &mut plan);
    plan_retired_entries(dirs, &mut plan);
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

/// Explicit unit identities work on every platform; legacy entries are recognised by their incremental objects.
pub fn superseded_incremental_entries(dirs: &Dirs, keep: usize) -> Vec<(PathBuf, u64)> {
    superseded_among(&scan_entries(dirs), keep)
}

pub(crate) fn superseded_cache_retirements(dirs: &Dirs, keep: usize) -> Vec<(PathBuf, u64, cache::EntryRetirementSnapshot)> {
    let scanned = scan_entries(dirs);
    let snapshots: HashMap<_, _> = scanned.iter().filter_map(|entry| entry.retirement.as_ref().map(|snapshot| (entry.path.as_path(), snapshot))).collect();
    superseded_among(&scanned, keep).into_iter().filter_map(|(path, bytes)| {
        let snapshot = (*snapshots.get(path.as_path())?).clone();
        Some((path, bytes, snapshot))
    }).collect()
}

/// What one read of an entry's `meta.json` yields: its LRU time, its unit identity if incremental, and its size by the recorded byte counts.
struct ScannedEntry {
    path: PathBuf,
    used: SystemTime,
    unit: Option<IncrementalUnit>,
    estimate: Option<u64>,
    retirement: Option<cache::EntryRetirementSnapshot>,
}

#[derive(Debug, PartialEq, Eq, Hash)]
enum IncrementalUnit {
    Explicit(String),
    Legacy(Vec<String>),
}

/// Stored files are rounded up to blocks, so the sum errs high and never hides an entry over the cap.
fn estimate_entry_bytes(meta: &cache::EntryMeta) -> Option<u64> {
    if meta.files.is_empty() {
        return None;
    }
    Some(
        ENTRY_OVERHEAD_BYTES
            + meta
                .files
                .iter()
                .map(|f| f.stored_bytes.unwrap_or(f.bytes).div_ceil(4096) * 4096)
                .sum::<u64>(),
    )
}

fn unit_identity(meta: &cache::EntryMeta) -> Option<IncrementalUnit> {
    if let Some(unit) = &meta.incremental_unit {
        return cache::valid_key(unit).then(|| IncrementalUnit::Explicit(unit.clone()));
    }
    if !meta.files.iter().any(|f| is_incremental_object(&f.name)) {
        return None;
    }
    let mut unit: Vec<String> = meta
        .files
        .iter()
        .map(|f| f.name.clone())
        .filter(|n| !n.ends_with(".o") && !n.ends_with(".d"))
        .collect();
    unit.sort();
    (!unit.is_empty()).then_some(IncrementalUnit::Legacy(unit))
}

fn scan_entries(dirs: &Dirs) -> Vec<ScannedEntry> {
    let mut out = Vec::new();
    let store = cache::Cache::new(dirs);
    let Ok(shards) = fs::read_dir(dirs.store()) else { return out };
    for shard in shards.flatten() {
        let Ok(entries) = fs::read_dir(shard.path()) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(key) = path.file_name().and_then(|name| name.to_str()).filter(|key| cache::valid_key(key)) else { continue };
            if path != store.entry_dir(key) { continue; }
            let retirement = store.retirement_snapshot(key);
            let (used, meta) = match File::open(path.join("meta.json")) {
                Ok(mut file) => {
                    let used = file.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
                    let mut bytes = Vec::new();
                    let meta = file.read_to_end(&mut bytes).ok().and_then(|_| serde_json::from_slice::<cache::EntryMeta>(&bytes).ok());
                    (used, meta)
                }
                Err(_) => (SystemTime::UNIX_EPOCH, None),
            };
            let unit = meta.as_ref().and_then(unit_identity);
            let estimate = meta.as_ref().and_then(estimate_entry_bytes);
            let retirement = retirement.filter(|snapshot| snapshot.used() == used
                && path.file_name().and_then(|name| name.to_str()).and_then(|key| store.retirement_snapshot(key)).as_ref() == Some(snapshot));
            out.push(ScannedEntry { path, used, unit, estimate, retirement });
        }
    }
    out
}

fn superseded_among(scanned: &[ScannedEntry], keep: usize) -> Vec<(PathBuf, u64)> {
    let mut units: HashMap<&IncrementalUnit, Vec<(SystemTime, &Path)>> = HashMap::new();
    for entry in scanned {
        if let Some(unit) = &entry.unit {
            units.entry(unit).or_default().push((entry.used, entry.path.as_path()));
        }
    }
    let mut out = Vec::new();
    for (_, mut entries) in units {
        entries.sort_by_key(|e| std::cmp::Reverse(e.0));
        for (_, path) in entries.into_iter().skip(keep) {
            out.push((path.to_path_buf(), fsx::allocated_size(path)));
        }
    }
    out.sort();
    out
}

/// With `exact` false, sizes come from the recorded byte counts unless the cache is over its cap, when they are measured so eviction stays exact.
fn plan_cache(dirs: &Dirs, opts: &GcOptions, plan: &mut Plan, exact: bool) -> HashSet<String> {
    let mut evicted = HashSet::new();
    let scanned = scan_entries(dirs);
    let superseded = superseded_among(&scanned, INCREMENTAL_ENTRIES_KEPT);
    let doomed: HashSet<&Path> = superseded.iter().map(|s| s.0.as_path()).collect();
    for (path, _) in &superseded {
        if let Some(key) = path.file_name() {
            evicted.insert(key.to_string_lossy().into_owned());
        }
    }
    let mut entries: Vec<(SystemTime, PathBuf, u64)> = scanned
        .iter()
        .filter(|e| !doomed.contains(e.path.as_path()))
        .map(|e| {
            let bytes = match e.estimate {
                Some(estimate) if !exact => estimate,
                _ => fsx::allocated_size(&e.path),
            };
            (e.used, e.path.clone(), bytes)
        })
        .collect();
    for (path, bytes) in superseded.iter().cloned() {
        plan.removals.push(Removal { class: Class::CacheEntry, path, bytes });
    }
    let mut total: u64 = entries.iter().map(|e| e.2).sum();
    if !exact && total > opts.max_cache_bytes {
        for entry in &mut entries {
            entry.2 = fsx::allocated_size(&entry.1);
        }
        total = entries.iter().map(|e| e.2).sum();
    }
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
    let selected: HashSet<_> = plan.removals.iter().filter(|removal| removal.class == Class::CacheEntry).map(|removal| removal.path.as_path()).collect();
    for entry in scanned {
        if selected.contains(entry.path.as_path()) && let Some(snapshot) = entry.retirement { plan.cache_retirements.insert(entry.path, snapshot); }
    }
    evicted
}

fn plan_targets(dirs: &Dirs, opts: &GcOptions, plan: &mut Plan) {
    plan_idle_targets(dirs, opts, plan, usize::MAX);
    for target in stats::targets(dirs) {
        if !target_path_safe(dirs, &target.path).unwrap_or(false) {
            continue;
        }
        if plan.target_retirements.contains_key(&target.path) {
            continue;
        }
        let Ok(profiles) = fs::read_dir(&target.path) else {
            continue;
        };
        for profile in profiles.flatten() {
            plan_profile(&profile.path(), opts, plan);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TargetRetirement {
    identity: (u64, u64),
    layout: Vec<(PathBuf, u64, u64)>,
    profiles: Vec<PathBuf>,
    activity: SystemTime,
    idle: Duration,
    bytes: u64,
    bounded: bool,
}

fn old_enough(time: SystemTime, idle: Duration) -> bool {
    SystemTime::now()
        .duration_since(time)
        .is_ok_and(|age| age > idle)
}

fn idle_target_candidates(dirs: &Dirs, idle: Duration) -> Vec<(SystemTime, PathBuf)> {
    let mut candidates = Vec::new();
    let Ok(workspaces) = fs::read_dir(dirs.targets()) else {
        return candidates;
    };
    for workspace in workspaces
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
    {
        let Ok(targets) = fs::read_dir(workspace.path()) else {
            continue;
        };
        for target in targets
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        {
            let path = target.path();
            if let Ok(Some(activity)) = lock::target_activity(dirs, &path)
                && old_enough(activity, idle)
            {
                candidates.push((activity, path));
            }
        }
    }
    candidates.sort();
    candidates
}

fn plan_idle_targets(dirs: &Dirs, opts: &GcOptions, plan: &mut Plan, limit: usize) {
    let mut candidates = idle_target_candidates(dirs, opts.target_max_idle);
    if limit != usize::MAX && !candidates.is_empty() {
        let offset = (fsx::unix_now() / 86_400) as usize % candidates.len();
        candidates.rotate_left(offset);
    }
    let attempts = limit.saturating_mul(4);
    let mut planned = 0;
    for (_, target) in candidates.into_iter().take(attempts) {
        let Ok(Some(snapshot)) =
            retirement_snapshot(dirs, &target, opts.target_max_idle, limit != usize::MAX)
        else {
            continue;
        };
        if snapshot.bytes == 0 {
            continue;
        }
        plan.removals.push(Removal {
            class: Class::TargetDir,
            path: target.clone(),
            bytes: snapshot.bytes,
        });
        plan.target_retirements.insert(target, snapshot);
        planned += 1;
        if planned == limit {
            break;
        }
    }
}

fn target_path_safe(dirs: &Dirs, target: &Path) -> io::Result<bool> {
    if !sys::lstat(target)?.is_dir()
        || !sys::lstat(&dirs.targets()).is_ok_and(|m| m.is_dir())
        || target.parent().and_then(Path::parent) != Some(dirs.targets().as_path())
        || !target
            .parent()
            .is_some_and(|p| sys::lstat(p).is_ok_and(|m| m.is_dir()))
        || !sys::canonicalize(target)?.starts_with(sys::canonicalize(&dirs.targets())?)
    {
        return Ok(false);
    }
    Ok(true)
}

fn retirement_snapshot(
    dirs: &Dirs,
    target: &Path,
    idle: Duration,
    bounded: bool,
) -> io::Result<Option<TargetRetirement>> {
    if !target_path_safe(dirs, target)? {
        return Ok(None);
    }
    let root = sys::lstat(target)?;
    let Some(activity) = lock::target_activity(dirs, target)? else {
        return Ok(None);
    };
    if !old_enough(activity, idle) {
        return Ok(None);
    }
    let mut layout = Vec::new();
    let mut profiles = Vec::new();
    for child in fs::read_dir(target)? {
        let child = child?;
        let meta = sys::lstat(&child.path())?;
        layout.push((child.path(), meta.dev(), meta.ino()));
        if bounded && layout.len() > 256 {
            return Ok(None);
        }
        if !meta.is_dir() {
            continue;
        }
        if child.path().join("deps").is_dir() {
            profiles.push(child.path());
        } else {
            for nested in fs::read_dir(child.path())? {
                let nested = nested?;
                let meta = sys::lstat(&nested.path())?;
                layout.push((nested.path(), meta.dev(), meta.ino()));
                if bounded && layout.len() > 256 {
                    return Ok(None);
                }
                if meta.is_dir() && nested.path().join("deps").is_dir() {
                    profiles.push(nested.path());
                }
            }
        }
    }
    if profiles.is_empty() {
        return Ok(None);
    }
    layout.sort();
    profiles.sort();
    let mut newest = activity;
    let mut bytes = 0u64;
    let mut seen = HashSet::new();
    let mut nodes = if bounded {
        TARGET_SCAN_NODES
    } else {
        usize::MAX
    };
    let deadline = bounded.then(|| Instant::now() + Duration::from_millis(250));
    for profile in &profiles {
        if !profile_contents_known(profile, &mut nodes, deadline)? {
            return Ok(None);
        }
        for child in fs::read_dir(profile)? {
            let child = child?;
            if cargo_lock_name(&child.file_name()) {
                continue;
            }
            if !scan_retirement_activity(
                &child.path(),
                &mut newest,
                &mut bytes,
                &mut seen,
                &mut nodes,
                deadline,
            )? {
                return Ok(None);
            }
            if ["deps", ".fingerprint"].iter().any(|name| child.file_name() == *name) {
                bytes = bytes.saturating_sub(sys::lstat(&child.path())?.allocated());
            }
        }
    }
    if !old_enough(newest, idle) {
        return Ok(None);
    }
    Ok(Some(TargetRetirement {
        identity: (root.dev(), root.ino()),
        layout,
        profiles,
        activity,
        idle,
        bytes,
        bounded,
    }))
}

fn cargo_lock_name(name: &std::ffi::OsStr) -> bool {
    [".cargo-lock", ".cargo-build-lock", ".cargo-artifact-lock"]
        .iter()
        .any(|known| name == *known)
}

fn retirement_scan_budget(nodes: &mut usize, deadline: Option<Instant>) -> bool {
    if *nodes == 0 || deadline.is_some_and(|end| Instant::now() >= end) {
        return false;
    }
    *nodes -= 1;
    true
}

fn profile_contents_known(
    profile: &Path,
    nodes: &mut usize,
    deadline: Option<Instant>,
) -> io::Result<bool> {
    if !sys::lstat(&profile.join("deps")).is_ok_and(|m| m.is_dir())
        || !sys::lstat(&profile.join(".fingerprint")).is_ok_and(|m| m.is_dir())
    {
        return Ok(false);
    }
    let mut deps_inodes = HashSet::new();
    for entry in fs::read_dir(profile.join("deps"))? {
        if !retirement_scan_budget(nodes, deadline) {
            return Ok(false);
        }
        let meta = sys::lstat(&entry?.path())?;
        if meta.is_file() {
            deps_inodes.insert((meta.dev(), meta.ino()));
        }
    }
    for child in fs::read_dir(profile)? {
        if !retirement_scan_budget(nodes, deadline) {
            return Ok(false);
        }
        let child = child?;
        let name = child.file_name();
        if cargo_lock_name(&name) {
            if !sys::lstat(&child.path())?.is_file() {
                return Ok(false);
            }
            continue;
        }
        let meta = sys::lstat(&child.path())?;
        if meta.is_dir()
            && ["deps", ".fingerprint", "incremental", "build", "examples"]
                .iter()
                .any(|known| name == *known)
        {
            continue;
        }
        if meta.is_file() && deps_inodes.contains(&(meta.dev(), meta.ino())) {
            continue;
        }
        if meta.is_file() && name.to_string_lossy().ends_with(".d") {
            let binary = name
                .to_string_lossy()
                .strip_suffix(".d")
                .unwrap()
                .to_string();
            if sys::lstat(&profile.join(binary))
                .is_ok_and(|m| deps_inodes.contains(&(m.dev(), m.ino())))
            {
                continue;
            }
        }
        return Ok(false);
    }
    Ok(true)
}

fn scan_retirement_activity(
    path: &Path,
    newest: &mut SystemTime,
    bytes: &mut u64,
    seen: &mut HashSet<(u64, u64)>,
    nodes: &mut usize,
    deadline: Option<Instant>,
) -> io::Result<bool> {
    type DirectoryFrame = (PathBuf, (u64, u64), fs::ReadDir);
    let mut pending = Some(path.to_path_buf());
    let mut directories: Vec<DirectoryFrame> = Vec::new();
    loop {
        if let Some(path) = pending.take() {
            if !retirement_scan_budget(nodes, deadline) {
                return Ok(false);
            }
            let meta = sys::lstat(&path)?;
            if !meta.file_type().is_symlink() && !meta.is_file() && !meta.is_dir() {
                return Ok(false);
            }
            *newest = (*newest).max(meta.modified()?);
            let identity = (meta.dev(), meta.ino());
            let first = seen.insert(identity);
            if first {
                *bytes = bytes.saturating_add(meta.allocated());
            }
            if meta.is_dir() {
                if !first {
                    return Ok(false);
                }
                let children = fs::read_dir(&path)?;
                directories.push((path, identity, children));
            }
        }
        while let Some((_, _, entries)) = directories.last_mut() {
            if let Some(child) = entries.next() {
                pending = Some(child?.path());
                break;
            }
            let (directory, identity, _) = directories.pop().unwrap();
            let after = sys::lstat(&directory)?;
            if !after.is_dir() || (after.dev(), after.ino()) != identity {
                return Ok(false);
            }
            *newest = (*newest).max(after.modified()?);
        }
        if pending.is_none() {
            return Ok(true);
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
            // Dedicated planners own held deferred jobs and individual retired children.
            if entry.path() == dirs.deferred() || entry.path() == cache::evicted_dir(dirs) {
                continue;
            }
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

// Plan children separately so later arrivals never fall under a recursive parent removal.
fn plan_retired_entries(dirs: &Dirs, plan: &mut Plan) {
    let Some(queue) = cache::QueueProof::capture(dirs) else { return };
    let Ok(children) = fs::read_dir(cache::evicted_dir(dirs)) else { return };
    for child in children.flatten() {
        let path = child.path();
        let Some(proof) = queue.child(dirs, &path) else { continue };
        plan.retired_entries.insert(path.clone(), proof);
        plan.removals.push(Removal { class: Class::Scratch, bytes: fsx::allocated_size(&path), path });
    }
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
fn ctime_age(meta: &Meta) -> Option<Duration> {
    let (secs, nanos) = meta.ctime();
    let ctime = SystemTime::UNIX_EPOCH + Duration::new(secs.max(0) as u64, nanos.clamp(0, 999_999_999) as u32);
    SystemTime::now().duration_since(ctime).ok()
}

fn stale_temp(path: &Path, name: &str, opts: &GcOptions) -> bool {
    let Some(pid) = fsx::temp_owner(name) else { return false };
    let Ok(meta) = sys::lstat(path) else { return false };
    let Some(age) = ctime_age(&meta) else { return false };
    if pid == std::process::id() {
        return false;
    }
    age > opts.temp_max_age || (age > opts.temp_min_age && !sys::pid_alive(pid))
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
    plan_keylocks(dirs, plan);
    plan_deferred(dirs, plan);
    plan_history(dirs, opts, plan);
    plan_hashmemo(dirs, opts, plan);
    plan_startup_memos(dirs, opts, plan);
    plan_compact_memos(dirs, opts, plan);
}

/// A memo is rewritten only when its target's files change, so one untouched for a long time belongs to a target that is gone or idle.
fn plan_compact_memos(dirs: &Dirs, opts: &GcOptions, plan: &mut Plan) {
    let Ok(entries) = fs::read_dir(dirs.compact_memo_dir()) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
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

#[derive(Debug, PartialEq, Eq)]
struct MetadataIdentity {
    dev: u64,
    ino: u64,
    len: u64,
    modified: SystemTime,
    changed: (i64, i64),
}

impl MetadataIdentity {
    fn read(meta: &Meta) -> io::Result<Self> {
        Ok(Self {
            dev: meta.dev(),
            ino: meta.ino(),
            len: meta.len(),
            modified: meta.modified()?,
            changed: meta.ctime(),
        })
    }
}

#[derive(Debug)]
struct ManifestRetirement {
    identity: MetadataIdentity,
    contents: Vec<u8>,
    references: Vec<(String, Option<MetadataIdentity>)>,
    max_idle: Duration,
}

fn manifest_reference(store: &cache::Cache, key: &str) -> io::Result<Option<MetadataIdentity>> {
    if !cache::valid_key(key) {
        return Ok(None);
    }
    match sys::stat(&store.entry_dir(key).join("meta.json")) {
        Ok(meta) if meta.is_file() => MetadataIdentity::read(&meta).map(Some),
        Ok(_) => Err(io::Error::other("cache metadata is not a file")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn manifest_snapshot(
    store: &cache::Cache,
    path: &Path,
    max_idle: Duration,
) -> io::Result<ManifestRetirement> {
    #[derive(serde::Deserialize)]
    struct References {
        candidates: Vec<cache::Candidate>,
    }
    let mut file = File::open(path)?;
    let before = MetadataIdentity::read(&sys::fstat(&file)?)?;
    let mut contents = Vec::new();
    file.read_to_end(&mut contents)?;
    let mut references = Vec::new();
    if let Ok(manifest) = serde_json::from_slice::<References>(&contents) {
        for candidate in manifest.candidates {
            let usage = manifest_reference(store, &candidate.key)?;
            references.push((candidate.key, usage));
        }
    }
    let after = sys::lstat(path)?;
    if !after.is_file() || before != MetadataIdentity::read(&after)? {
        return Err(io::Error::other("manifest changed while reading"));
    }
    Ok(ManifestRetirement {
        identity: before,
        contents,
        references,
        max_idle,
    })
}

impl ManifestRetirement {
    fn eligible(&self, evicted: &HashSet<String>) -> bool {
        let Some(age) = self.identity.modified.elapsed().ok() else {
            return false;
        };
        if age < METADATA_GRACE {
            return false;
        }
        let orphaned = self
            .references
            .iter()
            .all(|(key, usage)| usage.is_none() || evicted.contains(key));
        orphaned
            || (age > self.max_idle
                && self.references.iter().all(|(_, usage)| {
                    usage.as_ref().is_none_or(|usage| {
                        usage
                            .modified
                            .elapsed()
                            .is_ok_and(|age| age > self.max_idle)
                    })
                }))
    }

    fn still_eligible(&self, store: &cache::Cache, path: &Path) -> bool {
        let Ok(current) = manifest_snapshot(store, path, self.max_idle) else {
            return false;
        };
        if current.identity != self.identity
            || current.contents != self.contents
            || current.references.len() != self.references.len()
        {
            return false;
        }
        // Disappeared entries are safe; replaced or newly used entries invalidate the plan.
        if current.references.iter().zip(&self.references).any(
            |((key, usage), (old_key, old_usage))| {
                key != old_key || (usage.is_some() && usage != old_usage)
            },
        ) {
            return false;
        }
        current.eligible(&HashSet::new())
    }
}

fn plan_manifests(dirs: &Dirs, opts: &GcOptions, evicted: &HashSet<String>, plan: &mut Plan) {
    let store = cache::Cache::new(dirs);
    for (name, path) in sharded_files(&dirs.manifests()) {
        if fsx::temp_owner(&name).is_some() {
            if stale_temp(&path, &name, opts) {
                plan.removals.push(Removal {
                    class: Class::Scratch,
                    bytes: fsx::allocated_size(&path),
                    path,
                });
            }
            continue;
        }
        if !mtime_age(&path).is_some_and(|age| age >= METADATA_GRACE) {
            continue;
        }
        let Ok(snapshot) = manifest_snapshot(&store, &path, opts.manifest_max_age) else {
            continue;
        };
        if snapshot.eligible(evicted) {
            plan.manifest_retirements.insert(path.clone(), snapshot);
            push_metadata(plan, path);
        }
    }
}

fn flock_held(path: &Path) -> Option<bool> {
    let file = File::open(path).ok()?;
    // Exclusive probe: scoped builds hold the lock shared, so a shared probe would succeed and report idle.
    match sys::try_lock(&file, LockKind::Exclusive) {
        Ok(()) => Some(false),
        Err(fs::TryLockError::WouldBlock) => Some(true),
        Err(fs::TryLockError::Error(_)) => None,
    }
}

fn lock_dir_stale(dir: &Path, max_idle: Duration) -> bool {
    let Ok(meta) = fs::symlink_metadata(dir) else { return false };
    if !meta.is_dir() || !sys::owned_by_me(dir) {
        return false;
    }
    if let Ok(recorded) = fs::read_to_string(dir.join("target")) {
        let target = Path::new(&recorded);
        if target.exists()
            && let Some(home) = dir.parent().and_then(Path::parent)
            && target.starts_with(home.join("targets"))
        {
            return false;
        }
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

/// Unlinking a lock someone waits on only loses the de-duplication of one compile, never correctness.
fn plan_keylocks(dirs: &Dirs, plan: &mut Plan) {
    for (_, path) in sharded_files(&dirs.keylocks()) {
        if mtime_age(&path).is_some_and(|age| age > DEFERRED_MAX_AGE) && flock_held(&path) == Some(false) {
            push_metadata(plan, path);
        }
    }
}

/// Job files, their temporaries and unheld slot locks left by helpers that were killed.
fn plan_deferred(dirs: &Dirs, plan: &mut Plan) {
    let Ok(entries) = fs::read_dir(dirs.deferred()) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if !mtime_age(&path).is_some_and(|age| age > DEFERRED_MAX_AGE) {
            continue;
        }
        let is_slot = entry.file_name().to_string_lossy().starts_with(DEFERRED_SLOT_PREFIX);
        if is_slot && flock_held(&path) != Some(false) {
            continue;
        }
        push_metadata(plan, path);
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

#[derive(Debug)]
struct UnitRetirement {
    identity: MetadataIdentity,
    contents: Vec<u8>,
    keys: Vec<String>,
    max_idle: Duration,
}

fn unit_snapshot(store: &cache::Cache, path: &Path, max_idle: Duration) -> io::Result<UnitRetirement> {
    let path_before = sys::lstat(path)?;
    if !path_before.is_file() || path_before.file_type().is_symlink() { return Err(io::Error::other("unit record is not a regular file")); }
    let mut file = File::open(path)?;
    let before = MetadataIdentity::read(&sys::fstat(&file)?)?;
    if before != MetadataIdentity::read(&path_before)? { return Err(io::Error::other("unit record changed before reading")); }
    if before.len > 64 * 1024 { return Err(io::Error::other("oversized unit record")); }
    let mut contents = Vec::new();
    (&mut file).take(64 * 1024 + 1).read_to_end(&mut contents)?;
    let text = std::str::from_utf8(&contents).map_err(io::Error::other)?;
    let keys: Vec<String> = if text == "\n" { Vec::new() } else { text.lines().map(str::to_owned).collect() };
    if keys.iter().any(|key| !cache::valid_key(key)) || text != format!("{}\n", keys.join("\n")) {
        return Err(io::Error::other("invalid unit record"));
    }
    let after = sys::lstat(path)?;
    if !after.is_file() || after.file_type().is_symlink() || before != MetadataIdentity::read(&after)? {
        return Err(io::Error::other("unit record changed while reading"));
    }
    let snapshot = UnitRetirement { identity: before, contents, keys, max_idle };
    if !snapshot.references_absent(store) { return Err(io::Error::other("unit record references present or uncertain entries")); }
    Ok(snapshot)
}

fn unit_reference_absent(store: &cache::Cache, key: &str) -> bool {
    let entry = store.entry_dir(key);
    let Some(shard) = entry.parent() else { return false };
    let Some(storage) = shard.parent() else { return false };
    let Some(home) = storage.parent() else { return false };
    let Ok(meta) = sys::stat(home) else { return false };
    if !meta.is_dir() { return false; }
    let mut parents = vec![(home, true, meta.dev(), meta.ino())];
    let mut absent = entry.as_path();
    for path in [storage, shard] {
        match sys::lstat(path) {
            Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {
                parents.push((path, false, meta.dev(), meta.ino()));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => { absent = path; break; }
            _ => return false,
        }
    }
    // Prove component types before interpreting portable NotFound, then recheck parent identities.
    if !matches!(sys::lstat(absent), Err(error) if error.kind() == io::ErrorKind::NotFound) { return false; }
    parents.iter().all(|&(path, follow, dev, ino)| {
        let meta = if follow { sys::stat(path) } else { sys::lstat(path) };
        meta.is_ok_and(|meta| meta.is_dir() && (follow || !meta.file_type().is_symlink()) && meta.dev() == dev && meta.ino() == ino)
    })
}

impl UnitRetirement {
    fn references_absent(&self, store: &cache::Cache) -> bool {
        // Existing roots, unsafe parent types, and I/O uncertainty retain the record.
        self.keys.iter().all(|key| unit_reference_absent(store, key))
    }

    fn eligible(&self, store: &cache::Cache) -> bool {
        self.identity.modified.elapsed().is_ok_and(|age| age > self.max_idle) && self.references_absent(store)
    }

    fn still_eligible(&self, store: &cache::Cache, path: &Path) -> bool {
        let Ok(current) = unit_snapshot(store, path, self.max_idle) else { return false };
        current.identity == self.identity && current.contents == self.contents && current.keys == self.keys && current.eligible(store)
    }
}

fn plan_hashmemo(dirs: &Dirs, opts: &GcOptions, plan: &mut Plan) {
    for (name, path) in sharded_files(&dirs.hashmemo()) {
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
    let store = cache::Cache::new(dirs);
    for (name, path) in sharded_files(&store.units_dir()) {
        if !cache::valid_key(&name) || path.parent() != Some(store.units_dir().join(&name[..2]).as_path()) { continue; }
        if !mtime_age(&path).is_some_and(|age| age > opts.hashmemo_max_age) { continue; }
        let Ok(snapshot) = unit_snapshot(&store, &path, opts.hashmemo_max_age) else { continue };
        if snapshot.eligible(&store) {
            plan.unit_retirements.insert(path.clone(), snapshot);
            push_metadata(plan, path);
        }
    }

}

pub fn execute(dirs: &Dirs, plan: &Plan) -> (u64, Vec<(PathBuf, std::io::Error)>) {
    let (freed, failures) = execute_removals(dirs, plan);
    prune_empty_workspaces(dirs);
    (freed, failures)
}

// Count the size estimate only when this attempt actually removes the child root.
fn remove_retired_child(path: &Path, before_delete: impl FnOnce() -> bool) -> io::Result<u64> {
    let bytes = fsx::allocated_size(path);
    if !before_delete() { return Ok(0); }
    let removal = match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) => Err(error),
    };
    match removal {
        Ok(()) => Ok(bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error),
    }
}

fn execute_removals(dirs: &Dirs, plan: &Plan) -> (u64, Vec<(PathBuf, std::io::Error)>) {
    let mut freed = 0;
    let mut failures = Vec::new();
    let locks = dirs.locks();
    let store = cache::Cache::new(dirs);
    // Finish cache and metadata retirement before acquiring any profile locks.
    for removal in &plan.removals {
        if removal.class == Class::CacheEntry {
            let Some(snapshot) = plan.cache_retirements.get(&removal.path) else { continue };
            match store.try_retire_entry(snapshot) {
                Ok(cache::EntryRetirementOutcome::Retired) => freed += removal.bytes,
                Ok(cache::EntryRetirementOutcome::Busy | cache::EntryRetirementOutcome::Stale) => {}
                Err(error) => failures.push((removal.path.clone(), error)),
            }
            continue;
        }
        let unit = plan.unit_retirements.get(&removal.path);
        let manifest = plan.manifest_retirements.get(&removal.path);
        if unit.is_none() && manifest.is_none() { continue; }
        let Ok(Some(_guard)) = cache::UnitRecordsGuard::try_acquire(dirs) else { continue };
        if unit.is_some_and(|snapshot| !snapshot.still_eligible(&store, &removal.path))
            || manifest.is_some_and(|snapshot| !snapshot.still_eligible(&store, &removal.path)) { continue; }
        match fs::remove_file(&removal.path) {
            Ok(()) => freed += removal.bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => failures.push((removal.path.clone(), error)),
        }
    }
    // Cargo's locks stay held on each profile for the rest of the run, and variants are rechecked under them.
    let mut profiles: HashMap<PathBuf, Option<ProfileGuard>> = HashMap::new();
    for removal in &plan.removals {
        if removal.class == Class::CacheEntry || removal.path.starts_with(store.units_dir())
            || plan.manifest_retirements.contains_key(&removal.path) { continue; }
        if removal.path.parent() == Some(cache::evicted_dir(dirs).as_path()) {
            let Some(proof) = plan.retired_entries.get(&removal.path) else { continue };
            if !proof.valid(dirs, &removal.path) { continue; }
            match remove_retired_child(&removal.path, || proof.valid(dirs, &removal.path)) {
                Ok(bytes) => freed += bytes,
                Err(error) => failures.push((removal.path.clone(), error)),
            }
            continue;
        }
        let mut incremental_root = None;
        if removal.class == Class::TargetDir {
            if let Some(snapshot) = plan.target_retirements.get(&removal.path) {
                let mut bytes = 0;
                let result = retire_target(dirs, &removal.path, snapshot, &mut bytes);
                freed += bytes;
                if let Err(error) = result {
                    failures.push((removal.path.clone(), error));
                }
            }
            continue;
        }
        if matches!(removal.class, Class::StaleVariant | Class::IncrementalSession) {
            let Some(profile) = compact::profile_of(&removal.path) else { continue };
            let guard = profiles
                .entry(profile.clone())
                .or_insert_with(|| ProfileGuard::acquire(&profile));
            let Some(guard) = guard else {
                failures.push((
                    removal.path.clone(),
                    std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        "a build holds this target directory; skipped",
                    ),
                ));
                continue;
            };
            if removal.class == Class::StaleVariant && !guard.still_superseded(&removal.path) {
                continue;
            }
            if removal.class == Class::IncrementalSession && !guard.incremental.contains(&removal.path) {
                continue;
            }
            let root = profile.join("incremental");
            if removal.path.starts_with(&root) {
                incremental_root = Some(root);
            }
        }
        // A build may have started using a lock directory since planning.
        if removal.class == Class::Metadata
            && removal.path.parent() == Some(locks.as_path())
            && !lock_dir_stale(&removal.path, plan.lock_max_idle)
        {
            continue;
        }
        let result = match incremental_root {
            Some(root) => compact::remove_incremental_payload(&root, &removal.path),
            None => fsx::remove_any(&removal.path),
        };
        match result {
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

fn retire_target(dirs: &Dirs, target: &Path, expected: &TargetRetirement, freed: &mut u64) -> io::Result<()> {
    if !target_path_safe(dirs, target)? {
        return Ok(());
    }
    let Some(guard) = lock::try_target_maintenance(dirs, target)? else {
        return Ok(());
    };
    if guard.activity()? != expected.activity {
        return Ok(());
    }
    let mut profile_locks = Vec::new();
    for profile in &expected.profiles {
        let meta = sys::lstat(profile)?;
        if !meta.is_dir()
            || !expected
                .layout
                .contains(&(profile.clone(), meta.dev(), meta.ino()))
        {
            return Ok(());
        }
        let Some(profile_lock) = compact::ProfileLock::try_acquire(profile) else {
            return Ok(());
        };
        profile_locks.push(profile_lock);
    }
    let Some(current) = retirement_snapshot(dirs, target, expected.idle, expected.bounded)? else {
        return Ok(());
    };
    if &current != expected || guard.activity()? != expected.activity {
        return Ok(());
    }
    for profile in &expected.profiles {
        let report = compact::prune_expired_incremental(profile, expected.idle);
        *freed = freed.saturating_add(report.removed_bytes).min(expected.bytes);
        if let Some((path, error)) = report.errors.first() {
            return Err(io::Error::other(format!("incremental retirement {}: {error}", path.display())));
        }
    }
    let Some(current) = retirement_snapshot(dirs, target, expected.idle, expected.bounded)? else { return Ok(()) };
    if current.identity != expected.identity || current.layout != expected.layout
        || current.profiles != expected.profiles || current.activity != expected.activity
        || guard.activity()? != expected.activity
    {
        return Ok(());
    }
    compact::remove_profile_payloads_with(&expected.profiles, |_, bytes| {
        *freed = freed.saturating_add(bytes).min(expected.bytes);
    })?;
    drop(profile_locks);
    drop(guard);
    Ok(())
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

/// Claims daily collection under a process-held lock before spawning; FRUST_GC_SYNC runs it inline.
pub fn maybe_auto_gc(dirs: &Dirs, cfg: &Config) -> bool {
    let marker = dirs.auto_gc_marker();
    if mtime_age(&marker).is_some_and(|age| age < AUTO_GC_EVERY) || !dirs.root().is_dir() {
        return false;
    }
    let Ok(file) = File::options().create(true).truncate(false).write(true).open(&marker) else { return false };
    if sys::try_lock(&file, LockKind::Exclusive).is_err() {
        return false;
    }
    let due = file.metadata().and_then(|m| m.modified()).ok().and_then(|t| SystemTime::now().duration_since(t).ok());
    // A freshly created marker has a new mtime but no recorded run: its length is zero.
    let fresh_marker = file.metadata().map(|m| m.len() == 0).unwrap_or(true);
    if !fresh_marker && due.is_some_and(|age| age < AUTO_GC_EVERY) {
        return false;
    }
    let _ = file.set_len(0);
    let _ = sys::write_all_at(&file, fsx::unix_now().to_string().as_bytes(), 0);
    let _ = file.set_modified(SystemTime::now());
    if !cfg.gc_sync {
        // The collector takes the same flock, so the claim is released first.
        drop(file);
        if spawn_collector(dirs, cfg) {
            return true;
        }
        let Ok(file) = File::options().read(true).write(true).open(&marker) else {
            return false;
        };
        if sys::try_lock(&file, LockKind::Exclusive).is_err() {
            return false;
        }
        run_maintenance(dirs, cfg, false);
        let _ = file.set_len(0);
        let _ = file.set_modified(SystemTime::UNIX_EPOCH);
        return true;
    }
    run_maintenance(dirs, cfg, true);
    let _ = file.set_modified(SystemTime::now());
    true
}

fn drain_idle_targets(dirs: &Dirs, opts: &GcOptions) {
    let candidates = idle_target_candidates(dirs, opts.target_max_idle);
    for batch in candidates.chunks(AUTO_TARGET_RETIREMENTS) {
        let mut plan = Plan::default();
        for (_, target) in batch {
            let Ok(Some(snapshot)) = retirement_snapshot(dirs, target, opts.target_max_idle, false)
            else {
                continue;
            };
            if snapshot.bytes == 0 {
                continue;
            }
            plan.removals.push(Removal {
                class: Class::TargetDir,
                path: target.clone(),
                bytes: snapshot.bytes,
            });
            plan.target_retirements.insert(target.clone(), snapshot);
        }
        let _ = execute_removals(dirs, &plan);
    }
}

fn run_maintenance(dirs: &Dirs, cfg: &Config, drain: bool) {
    let opts = GcOptions::new(cfg.cache_max_bytes);
    let plan = plan_maintenance(dirs, &opts);
    let _ = execute_removals(dirs, &plan);
    if drain {
        drain_idle_targets(dirs, &opts);
    }
}

/// The hidden `frust gc --auto-background` entry: one maintenance run at background priority that never prints.
pub fn run_auto_child(dirs: &Dirs, cfg: &Config) -> i32 {
    let Ok(file) = File::options().create(true).truncate(false).write(true).open(dirs.auto_gc_marker()) else { return 0 };
    // Held for the whole run, so a second collector exits at once.
    if sys::try_lock(&file, LockKind::Exclusive).is_err() {
        return 0;
    }
    compact::at_background_priority(|| run_maintenance(dirs, cfg, true));
    let _ = file.set_len(1);
    let _ = file.set_modified(SystemTime::now());
    0
}

/// The `frust` binary next to this executable, or one level up when running from cargo's `deps` directory.
fn collector_binary() -> Option<PathBuf> {
    let name = format!("frust{}", std::env::consts::EXE_SUFFIX);
    let exe = std::env::current_exe().ok()?;
    let exe = exe.canonicalize().unwrap_or(exe);
    let dir = exe.parent()?;
    [Some(dir.join(&name)), dir.parent().map(|p| p.join(&name))].into_iter().flatten().find(|p| p.is_file())
}

fn spawn_collector(dirs: &Dirs, cfg: &Config) -> bool {
    use std::process::Command;
    let Some(exe) = collector_binary() else { return false };
    let gib = cfg.cache_max_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    let mut cmd = Command::new(exe);
    cmd.args(["gc", AUTO_CHILD_FLAG]).env("FRUST_HOME", dirs.root()).env("FRUST_CACHE_MAX_GIB", gib.to_string());
    sys::detach_command(&mut cmd, &[]);
    cmd.spawn().is_ok()
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
    fn retired_child_disappearing_after_sizing_is_not_counted_as_removed() {
        let root = home("retired-size-delete-race");
        for directory in [false, true] {
            let child = root.join("retired-child");
            if directory {
                fs::create_dir(&child).unwrap();
                fs::write(child.join("f0"), vec![0x4b; 16384]).unwrap();
            } else { fs::write(&child, vec![0x4b; 16384]).unwrap(); }
            assert!(fsx::allocated_size(&child) > 0);
            let credited = remove_retired_child(&child, || { fsx::remove_any(&child).unwrap(); true }).unwrap();
            assert_eq!(credited, 0, "another remover owns the post-size deletion");
            assert!(!child.exists());
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn variant_names_parse() {
        assert_eq!(variant_of("libserde-0123456789abcdef.rlib"), Some(("serde".into(), "0123456789abcdef".into())));
        assert_eq!(variant_of("my_bin-0123456789abcdef"), Some(("my_bin".into(), "0123456789abcdef".into())));
        assert_eq!(variant_of("libserde-0123456789abcdef.rmeta"), Some(("serde".into(), "0123456789abcdef".into())));
        assert_eq!(variant_of("build-script-build"), None);
        assert_eq!(variant_of("README.md"), None);
    }

    fn home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("frust-gc-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn store_entry(home: &Path, key: &str, files: &[(&str, u64)], age_secs: u64) -> PathBuf {
        let dir = home.join("store").join(&key[..2]).join(key);
        fs::create_dir_all(&dir).unwrap();
        let recorded: Vec<_> = files.iter().map(|(n, b)| serde_json::json!({"name": n, "stored": n, "text": false, "bytes": b})).collect();
        let meta = serde_json::json!({"crate_name": "c", "version": "1", "compile_ms": 1, "created_unix": 1, "files": recorded, "stderr": ""});
        fs::write(dir.join("meta.json"), serde_json::to_vec(&meta).unwrap()).unwrap();
        for (name, bytes) in files {
            fs::write(dir.join(name), vec![1u8; *bytes as usize]).unwrap();
        }
        File::options().write(true).open(dir.join("meta.json")).unwrap().set_modified(SystemTime::now() - Duration::from_secs(age_secs)).unwrap();
        dir
    }

    #[test]
    fn misplaced_duplicate_keys_cannot_retire_canonical_entries_from_other_units() {
        let root = home("misplaced-cache-key");
        let dirs = Dirs::at(&root);
        let store = cache::Cache::new(&dirs);
        let key = "aa".repeat(20);
        let legitimate = store_entry(&root, &key, &[("artifact", 64)], 0);
        let common_time = SystemTime::now() - Duration::from_secs(20_000);
        let files = [("libx-0123456789abcdef.rlib", 64), ("x-0123456789abcdef.c1.t1.rcgu.o", 64)];
        let misplaced = root.join("store/bb").join(&key);
        fs::create_dir_all(&misplaced).unwrap();
        let source = store_entry(&root, &"cc".repeat(20), &files, 5000);
        fs::copy(source.join("meta.json"), misplaced.join("meta.json")).unwrap();
        store_entry(&root, &"dd".repeat(20), &files, 1000);
        for path in [&legitimate, &misplaced] {
            File::options().write(true).open(path.join("meta.json")).unwrap().set_modified(common_time).unwrap();
        }
        assert!(superseded_incremental_entries(&dirs, 2).is_empty());
        assert!(superseded_cache_retirements(&dirs, 2).is_empty());
        let selected = plan(&dirs, &GcOptions::new(u64::MAX));
        assert!(cache_removals(&selected).is_empty());
        assert!(execute_removals(&dirs, &selected).1.is_empty());
        assert!(store.has_entry(&key));
        assert!(misplaced.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cache_gc_retires_selected_states_and_prunes_their_manifest_immediately() {
        let root = home("cache-manifest-retirement");
        let dirs = Dirs::at(&root);
        let store = cache::Cache::new(&dirs);
        let prekey = "ab".repeat(20);
        let mut paths = Vec::new();
        for n in 1..=3 {
            let key = format!("{n:040x}");
            let path = store_entry(&root, &key, &[("libx-0123456789abcdef.rlib", 64), ("x-0123456789abcdef.c1.t1.rcgu.o", 64)], 0);
            let mut meta: cache::EntryMeta = serde_json::from_slice(&fs::read(path.join("meta.json")).unwrap()).unwrap();
            meta.prekey = Some(prekey.clone());
            fs::write(path.join("meta.json"), serde_json::to_vec(&meta).unwrap()).unwrap();
            store.add_candidate(&prekey, cache::Candidate { inputs: Vec::new(), envs: Vec::new(), key });
            File::options().write(true).open(path.join("meta.json")).unwrap().set_modified(SystemTime::now() - Duration::from_secs((4 - n) * 5000)).unwrap();
            paths.push(path);
        }
        let opts = GcOptions::new(u64::MAX);
        let delayed = plan(&dirs, &opts);
        assert_eq!(cache_removals(&delayed), paths[..1]);
        store.mark_used(&format!("{:040x}", 1));
        let (_, failures) = execute_removals(&dirs, &delayed);
        assert!(failures.is_empty());
        assert!(paths[0].exists(), "usage after planning invalidates retirement");
        let current = plan(&dirs, &opts);
        assert_eq!(cache_removals(&current), paths[1..2]);
        let (_, failures) = execute_removals(&dirs, &current);
        assert!(failures.is_empty());
        assert!(!paths[1].exists());
        assert_eq!(store.candidates(&prekey).into_iter().map(|row| row.key).collect::<Vec<_>>(), vec![format!("{:040x}", 3), format!("{:040x}", 1)]);
        assert!(paths[0].exists() && paths[2].exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn manifest_gc_skips_busy_guard_and_rechecks_after_unlocked_upsert() {
        let root = home("manifest-guard-upsert");
        let dirs = Dirs::at(&root);
        let store = cache::Cache::new(&dirs);
        let key = "aa".repeat(20);
        let prekey = "bb".repeat(20);
        let entry = store_entry(&root, &key, &[("f0", 64)], 0);
        let row = cache::Candidate { inputs: Vec::new(), envs: Vec::new(), key: key.clone() };
        store.add_candidate(&prekey, row.clone());
        let path = dirs.manifests().join("bb").join(format!("{prekey}.json"));
        let old = SystemTime::now() - Duration::from_secs(60 * 86_400);
        for path in [&path, &entry.join("meta.json")] { File::options().write(true).open(path).unwrap().set_modified(old).unwrap(); }
        let selected = plan(&dirs, &GcOptions::new(u64::MAX));
        assert!(selected.manifest_retirements.contains_key(&path));
        let guard = cache::UnitRecordsGuard::try_acquire(&dirs).unwrap().unwrap();
        assert!(execute_removals(&dirs, &selected).1.is_empty());
        assert!(path.exists(), "validation and unlink require the same guard");
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let writer = store.clone();
        let writer_prekey = prekey.clone();
        let worker = std::thread::spawn(move || { ready_tx.send(()).unwrap(); writer.add_candidate(&writer_prekey, row); });
        ready_rx.recv().unwrap();
        drop(guard);
        worker.join().unwrap();
        assert!(execute_removals(&dirs, &selected).1.is_empty());
        assert!(path.exists());
        assert_eq!(store.candidates(&prekey).len(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    fn cache_removals(plan: &Plan) -> Vec<PathBuf> {
        plan.removals.iter().filter(|r| r.class == Class::CacheEntry).map(|r| r.path.clone()).collect()
    }

    #[test]
    fn recorded_sizes_round_up_to_blocks() {
        let meta: cache::EntryMeta = serde_json::from_value(serde_json::json!({
            "crate_name": "c", "version": "1", "compile_ms": 1, "created_unix": 1, "stderr": "",
            "files": [{"name": "a", "stored": "a", "text": false, "bytes": 1}, {"name": "b", "stored": "b", "text": false, "bytes": 4097}]
        }))
        .unwrap();
        assert_eq!(estimate_entry_bytes(&meta), Some(ENTRY_OVERHEAD_BYTES + 4096 + 8192));
        let empty: cache::EntryMeta = serde_json::from_value(serde_json::json!({
            "crate_name": "c", "version": "1", "compile_ms": 1, "created_unix": 1, "stderr": "", "files": []
        }))
        .unwrap();
        assert_eq!(estimate_entry_bytes(&empty), None, "no recorded files means the directory is measured instead");
    }

    #[test]
    fn metadata_slice_references_add_no_second_payload_allocation() {
        let meta: cache::EntryMeta = serde_json::from_value(serde_json::json!({
            "crate_name":"app", "version":"1", "compile_ms":1, "created_unix":1, "stderr":"stderr",
            "files":[
                {"name":"libapp.rlib", "stored":"f0", "text":false, "bytes":8192, "stored_bytes":4096},
                {"name":"libapp.rmeta", "stored":"f0", "text":false, "bytes":1024, "stored_bytes":0,
                 "encoding":"inline_metadata_slice_v1", "decoded_bytes":1024, "content_hash":"0".repeat(64),
                 "metadata_slice":{"offset":128,"source_encoding":"raw","source_decoded_bytes":8192,"source_content_hash":"1".repeat(64),"mode":0o100644}}
            ]
        })).unwrap();
        cache::metadata_slice::validate_groups(&meta.files).unwrap();
        assert_eq!(estimate_entry_bytes(&meta), Some(ENTRY_OVERHEAD_BYTES + 4096));
    }

    #[test]
    fn maintenance_planning_evicts_exactly_what_the_exact_plan_does() {
        let home = home("lazy-sizes");
        let dirs = Dirs::at(&home);
        let old = store_entry(&home, &format!("aa{}", "1".repeat(38)), &[("f0", 65_536)], 5000);
        let new = store_entry(&home, &format!("bb{}", "2".repeat(38)), &[("f0", 65_536)], 10);
        let tight = GcOptions::new(100 * 1024);
        let exact = cache_removals(&plan(&dirs, &tight));
        assert_eq!(exact, vec![old.clone()]);
        assert_eq!(cache_removals(&plan_maintenance(&dirs, &tight)), exact, "over the cap, sizes are measured and the choice is the same");
        let roomy = GcOptions::new(10 * 1024 * 1024);
        assert!(cache_removals(&plan_maintenance(&dirs, &roomy)).is_empty());
        assert!(new.exists());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn encoded_entry_budgets_use_stored_allocation_instead_of_artifact_size() {
        let home = home("encoded-sizes");
        let dirs = Dirs::at(&home);
        let path = store_entry(
            &home,
            &format!("aa{}", "3".repeat(38)),
            &[("f0", 4096)],
            5000,
        );
        let meta_path = path.join("meta.json");
        let mut meta: cache::EntryMeta =
            serde_json::from_slice(&fs::read(&meta_path).unwrap()).unwrap();
        meta.files[0].bytes = 100 * 1024 * 1024;
        meta.files[0].encoding = cache::payload::Encoding::Lz4BlocksV1;
        meta.files[0].stored_bytes = Some(fsx::allocated_size(&path.join("f0")));
        meta.files[0].decoded_bytes = Some(meta.files[0].bytes);
        meta.files[0].content_hash = Some("ab".repeat(32));
        assert_eq!(
            estimate_entry_bytes(&meta),
            Some(ENTRY_OVERHEAD_BYTES + 4096)
        );
        fs::write(meta_path, serde_json::to_vec(&meta).unwrap()).unwrap();
        let options = GcOptions::new(32 * 1024);
        assert!(cache_removals(&plan_maintenance(&dirs, &options)).is_empty());
        assert!(cache_removals(&plan(&dirs, &options)).is_empty());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn only_entries_beyond_the_newest_two_of_an_incremental_unit_are_superseded() {
        let home = home("superseded");
        let dirs = Dirs::at(&home);
        let files = [("libx-0123456789abcdef.rlib", 100), ("x-0123456789abcdef.c1.t1.rcgu.o", 100)];
        let oldest = store_entry(&home, &format!("aa{}", "1".repeat(38)), &files, 3000);
        let middle = store_entry(&home, &format!("bb{}", "2".repeat(38)), &files, 2000);
        let newest = store_entry(&home, &format!("cc{}", "3".repeat(38)), &files, 1000);
        let plain = store_entry(&home, &format!("dd{}", "4".repeat(38)), &[("libx-0123456789abcdef.rlib", 100)], 4000);
        let superseded: Vec<PathBuf> = superseded_incremental_entries(&dirs, 2).into_iter().map(|s| s.0).collect();
        assert_eq!(superseded, vec![oldest]);
        assert!(middle.exists() && newest.exists() && plain.exists());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn explicit_incremental_units_keep_two_states_without_mixing_other_units_or_legacy_entries() {
        let home = home("explicit-units");
        let dirs = Dirs::at(&home);
        let files = [("libx-0123456789abcdef.rlib", 100)];
        let mut entries = Vec::new();
        for i in 0..7 {
            let path = store_entry(&home, &format!("{i:040x}"), &files, (7 - i) * 1000);
            let meta_path = path.join("meta.json");
            let modified = fs::metadata(&meta_path).unwrap().modified().unwrap();
            let mut meta: cache::EntryMeta =
                serde_json::from_slice(&fs::read(&meta_path).unwrap()).unwrap();
            assert!(
                meta.incremental_unit.is_none(),
                "legacy metadata defaults to no explicit unit"
            );
            meta.incremental_unit = match i {
                0..=3 => Some("ab".repeat(20)),
                4 => Some("cd".repeat(20)),
                5 => Some("invalid".to_string()),
                _ => None,
            };
            fs::write(&meta_path, serde_json::to_vec(&meta).unwrap()).unwrap();
            File::options()
                .write(true)
                .open(&meta_path)
                .unwrap()
                .set_modified(modified)
                .unwrap();
            entries.push(path);
        }
        let superseded: Vec<_> = superseded_incremental_entries(&dirs, 2)
            .into_iter()
            .map(|(path, _)| path)
            .collect();
        assert_eq!(superseded, entries[..2]);
        assert!(entries[2..].iter().all(|path| path.exists()));
        let meta: cache::EntryMeta =
            serde_json::from_slice(&fs::read(entries[3].join("meta.json")).unwrap()).unwrap();
        assert_eq!(meta.incremental_unit, Some("ab".repeat(20)));
        assert_ne!(
            IncrementalUnit::Explicit("ab".repeat(20)),
            IncrementalUnit::Legacy(vec!["ab".repeat(20)])
        );
        fs::remove_dir_all(home).unwrap();
    }
}
