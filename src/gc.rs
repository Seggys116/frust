//! Every removal is safe because cargo and the compile cache rebuild whatever is missing.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use crate::cache;
use crate::compact::{self, Category};
use crate::config::{self, Config, DEFAULT_CACHE_MAX_AGE};
use crate::fsx;
use crate::lock;
use crate::paths::{DEFERRED_SLOT_PREFIX, Dirs};
pub use crate::progress::Progress;
use crate::progress::Silent;
use crate::stats;
use crate::store_broker;
use crate::sys::{self, LockKind, Meta};
use crate::wrapper;

/// Younger manifests and lock directories are left alone even if they look orphaned: a build may be writing them.
const METADATA_GRACE: Duration = Duration::from_secs(3600);
const AUTO_GC_EVERY: Duration = Duration::from_secs(86_400);
/// Unheld prekey locks and stale deferred metadata become collectible after this age; canonical slots persist.
const DEFERRED_MAX_AGE: Duration = Duration::from_secs(86_400);
const KEYLOCK_MAX_AGE: Duration = Duration::from_secs(600);
/// Hidden flag of `frust gc` that runs one daily collection and prints nothing.
pub const AUTO_CHILD_FLAG: &str = "--auto-background";
/// Room for an entry's directory and `meta.json` on top of the stored files when sizes come from the recorded byte counts.
const ENTRY_OVERHEAD_BYTES: u64 = 8192;
const AUTO_TARGET_RETIREMENTS: usize = 2;
// Foreground sampling is bounded; detached and explicitly synchronous collectors drain complete candidates.
const TARGET_SCAN_NODES: usize = 32_768;

#[derive(Debug, Clone)]
pub struct GcOptions {
    /// Opt-in cap: `u64::MAX` means unlimited, and zero evicts every unprotected entry.
    pub max_cache_bytes: u64,
    /// Cache entries unused for longer than this are evicted.
    pub cache_max_age: Duration,
    pub target_max_idle: Duration,
    pub variant_max_age: Duration,
    pub variants_to_keep: usize,
    pub manifest_max_age: Duration,
    pub lock_max_idle: Duration,
    pub history_max_age: Duration,
    pub hashmemo_max_age: Duration,
    /// Idle compile key lock files older than this are unlinked; an idle file is recreated on its next use.
    pub keylock_max_age: Duration,
    pub startup_memo_max_age: Duration,
    pub temp_min_age: Duration,
    /// Older temp files go even if their creator's pid exists, because pids are reused.
    pub temp_max_age: Duration,
    /// Incremental states kept per unit, never more than `INCREMENTAL_ENTRIES_KEPT`; `FRUST_CACHE_UNIT_VERSIONS`.
    pub unit_versions: usize,
}

impl GcOptions {
    pub fn new(max_cache_bytes: u64) -> GcOptions {
        GcOptions {
            max_cache_bytes,
            cache_max_age: DEFAULT_CACHE_MAX_AGE,
            target_max_idle: Duration::from_secs(30 * 86_400),
            variant_max_age: Duration::from_secs(7 * 86_400),
            variants_to_keep: 2,
            manifest_max_age: Duration::from_secs(30 * 86_400),
            lock_max_idle: Duration::from_secs(7 * 86_400),
            history_max_age: Duration::from_secs(90 * 86_400),
            hashmemo_max_age: Duration::from_secs(30 * 86_400),
            keylock_max_age: KEYLOCK_MAX_AGE,
            startup_memo_max_age: Duration::from_secs(30 * 86_400),
            temp_min_age: Duration::from_secs(3600),
            temp_max_age: Duration::from_secs(86_400),
            unit_versions: INCREMENTAL_ENTRIES_KEPT,
        }
    }

    /// The configured size cap plus `FRUST_CACHE_MAX_AGE_DAYS` and `FRUST_CACHE_UNIT_VERSIONS`.
    pub fn from_config(cfg: &Config) -> GcOptions {
        GcOptions { cache_max_age: config::cache_max_age(), unit_versions: config::unit_versions(), ..GcOptions::new(cfg.cache_max_bytes) }
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
            Class::CacheEntry => "cache entries expired, superseded or over a set size cap",
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

/// One workspace of the home: its central targets' allocated size and what the plan removes inside them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceUsage {
    pub name: String,
    pub root: Option<String>,
    pub bytes: u64,
    pub items: usize,
    pub freed: u64,
}

#[derive(Debug, Default)]
pub struct Plan {
    pub removals: Vec<Removal>,
    pub trim_ledger: bool,
    /// Every workspace under `targets/`, largest first, including those with nothing to remove.
    pub workspaces: Vec<WorkspaceUsage>,
    /// The cache store's entries as sized for this plan.
    pub store_bytes: u64,
    /// Lock directories are rechecked just before removal: a build may have started using one since planning.
    pub lock_max_idle: Duration,
    target_retirements: HashMap<PathBuf, TargetRetirement>,
    manifest_retirements: HashMap<PathBuf, ManifestRetirement>,
    retired_entries: HashMap<PathBuf, cache::RetiredChildProof>,
    unit_retirements: HashMap<PathBuf, UnitRetirement>,
    cache_retirements: HashMap<PathBuf, cache::EntryRetirementSnapshot>,
    broker_retirements: HashMap<PathBuf, store_broker::StaleRunProof>,
    deferred_retirements: HashMap<PathBuf, DeferredMetadata>,
    keylock_retirements: HashSet<PathBuf>,
    /// Pack lines that listed no present entry when planned, with the age they had to exceed.
    pack_retirements: Vec<(String, u64, Vec<String>)>,
    pack_max_idle: Duration,
    /// The age beyond which the hash memo log drops its records, set when a log exists.
    hashmemo_log_age: Option<Duration>,
}

impl Plan {
    pub fn bytes(&self) -> u64 {
        self.removals.iter().map(|r| r.bytes).sum()
    }

    pub fn by_class(&self, class: Class) -> (usize, u64) {
        self.removals.iter().filter(|r| r.class == class).fold((0, 0), |(n, b), r| (n + 1, b + r.bytes))
    }

    /// Charges each removal to the workspace whose target holds it.
    fn attribute(&mut self, dirs: &Dirs) {
        for usage in &mut self.workspaces {
            (usage.items, usage.freed) = (0, 0);
        }
        for removal in &self.removals {
            let Some(name) = removal.path.strip_prefix(dirs.targets()).ok().and_then(|rest| rest.components().next()) else { continue };
            if let Some(usage) = self.workspaces.iter_mut().find(|usage| name.as_os_str() == usage.name.as_str()) {
                usage.items += 1;
                usage.freed = usage.freed.saturating_add(removal.bytes);
            }
        }
    }

    /// Removals outside every workspace's targets and outside the store: scratch, metadata, locks and history.
    pub fn outside_targets(&self, dirs: &Dirs) -> (usize, u64) {
        self.removals
            .iter()
            .filter(|r| !r.path.starts_with(dirs.targets()) && !r.path.starts_with(dirs.store()))
            .fold((0, 0), |(n, b), r| (n + 1, b + r.bytes))
    }

    pub fn store_removals(&self, dirs: &Dirs) -> (usize, u64) {
        self.removals.iter().filter(|r| r.path.starts_with(dirs.store())).fold((0, 0), |(n, b), r| (n + 1, b + r.bytes))
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
    plan_with(dirs, opts, &mut Silent)
}

type PlanStep<'a> = (&'static str, &'a dyn Fn(&mut Plan));

/// Covers the whole home: the store, every workspace's central targets and the shared metadata.
pub fn plan_with(dirs: &Dirs, opts: &GcOptions, progress: &mut dyn Progress) -> Plan {
    let mut plan = Plan { lock_max_idle: opts.lock_max_idle, ..Plan::default() };
    let evicted = plan_cache(dirs, opts, &mut plan, true, progress);
    plan_targets(dirs, opts, &mut plan, progress);
    let steps: [PlanStep<'_>; 4] = [
        ("scratch", &|plan| plan_scratch(dirs, plan)),
        ("evicted entries", &|plan| plan_retired_entries(dirs, plan)),
        ("store temporaries", &|plan| plan_store_temps(dirs, opts, plan)),
        ("manifests, locks, history and memos", &|plan| plan_metadata(dirs, opts, &evicted, plan)),
    ];
    progress.phase("plan home", Some(steps.len() as u64), "reclaimable");
    for (name, step) in steps {
        let before = plan.bytes();
        step(&mut plan);
        progress.item(name, plan.bytes().saturating_sub(before));
    }
    plan.attribute(dirs);
    plan
}

pub fn plan_maintenance(dirs: &Dirs, opts: &GcOptions) -> Plan {
    let mut plan = Plan {
        lock_max_idle: opts.lock_max_idle,
        ..Plan::default()
    };
    let evicted = plan_cache(dirs, opts, &mut plan, false, &mut Silent);
    plan_idle_targets(dirs, opts, &mut plan, AUTO_TARGET_RETIREMENTS, &mut Silent);
    plan_scratch(dirs, &mut plan);
    plan_retired_entries(dirs, &mut plan);
    plan_store_temps(dirs, opts, &mut plan);
    plan_target_temps(dirs, opts, &mut plan);
    plan_metadata(dirs, opts, &evicted, &mut plan);
    plan
}

/// The current source state and one other, so switching between two branches stays warm.
pub const INCREMENTAL_ENTRIES_KEPT: usize = 2;

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
    link: LinkState,
    /// The prekey, which names a unit across every state of its inputs.
    group: Option<String>,
    /// A path-crate entry that no unit record lists: nothing will ever supersede or retire it.
    unreferenced_path: bool,
}

/// How long an unreferenced store-only path entry is left alone, so a store that is still being recorded is never raced.
const ORPHAN_GRACE: Duration = Duration::from_secs(600);

/// How an entry's linked files relate to target names: a file still shared with one frees nothing when the entry goes.
#[derive(Debug, Default, Clone, Copy)]
struct LinkState {
    any: bool,
    /// Every linked file has no other name left.
    orphaned: bool,
    shared_alloc: u64,
    shared_estimate: u64,
}

fn link_state(entry: &Path, meta: &cache::EntryMeta) -> LinkState {
    let mut state = LinkState { orphaned: true, ..LinkState::default() };
    for file in meta.files.iter().filter(|f| f.encoding == cache::payload::Encoding::Linked && cache::safe_file_name(&f.stored)) {
        state.any = true;
        if let Ok(stored) = sys::lstat(&entry.join(&file.stored))
            && stored.is_file()
            && stored.nlink() > 1
        {
            // A stale incremental object name left in a target must not keep a superseded entry from being orphaned.
            state.orphaned &= cache::is_incremental_object(&file.name);
            state.shared_alloc += stored.allocated();
            state.shared_estimate += file.stored_bytes.unwrap_or(file.bytes).div_ceil(4096) * 4096;
        }
    }
    state.orphaned &= state.any;
    state
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
                .filter(|f| f.inline.is_none())
                .map(|f| f.stored_bytes.unwrap_or(f.bytes).div_ceil(4096) * 4096)
                .sum::<u64>(),
    )
}

fn unit_identity(meta: &cache::EntryMeta) -> Option<IncrementalUnit> {
    if let Some(unit) = &meta.incremental_unit {
        return cache::valid_key(unit).then(|| IncrementalUnit::Explicit(unit.clone()));
    }
    if !meta.files.iter().any(|f| cache::is_incremental_object(&f.name)) {
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

/// Entry key to unit key from the `units/` records, the authoritative grouping of a unit's versions.
fn unit_records(dirs: &Dirs) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for (unit, _, keys) in cache::Cache::new(dirs).unit_records() {
        for key in keys {
            map.insert(key, unit.clone());
        }
    }
    map
}

fn scan_entries(dirs: &Dirs) -> Vec<ScannedEntry> {
    scan_entries_with(dirs, &mut Silent)
}

fn scan_entries_with(dirs: &Dirs, progress: &mut dyn Progress) -> Vec<ScannedEntry> {
    let mut out = Vec::new();
    let store = cache::Cache::new(dirs);
    let records = unit_records(dirs);
    let items = cache::keyed_items(&dirs.store());
    progress.phase("scan store", Some(items.len() as u64), "");
    for path in items {
        progress.item(stats::STORE_SCOPE, 0);
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
        let link = meta.as_ref().map(|meta| link_state(&path, meta)).unwrap_or_default();
        let estimate = meta.as_ref().and_then(estimate_entry_bytes).map(|bytes| bytes.saturating_sub(link.shared_estimate));
        let group = meta
            .as_ref()
            .and_then(|meta| meta.incremental_unit.clone().filter(|unit| cache::valid_key(unit)))
            .or_else(|| records.get(key).cloned())
            .or_else(|| meta.as_ref().and_then(|meta| meta.prekey.clone()));
        let retirement = retirement.filter(|snapshot| snapshot.used() == used
            && path.file_name().and_then(|name| name.to_str()).and_then(|key| store.retirement_snapshot(key)).as_ref() == Some(snapshot));
        let unreferenced_path = !records.contains_key(key)
            && meta.as_ref().is_some_and(|meta| meta.source.as_deref() == Some(cache::clean::SOURCE_PATH));
        out.push(ScannedEntry { path, used, unit, estimate, retirement, link, group, unreferenced_path });
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
    // A linked entry that no target name shares costs a full copy, so every one but the newest of its unit goes at once.
    let mut groups: HashMap<&str, Vec<&ScannedEntry>> = HashMap::new();
    for entry in scanned {
        if entry.link.any && let Some(group) = entry.group.as_deref() {
            groups.entry(group).or_default().push(entry);
        }
    }
    for (_, mut entries) in groups {
        entries.sort_by_key(|e| std::cmp::Reverse(e.used));
        for entry in entries.into_iter().skip(1).filter(|e| e.link.orphaned) {
            out.push((entry.path.clone(), fsx::allocated_size(&entry.path)));
        }
    }
    // Entries only the store holds and no unit record lists (a lost record write) would otherwise wait for the age limit.
    for entry in scanned.iter().filter(|e| e.link.orphaned && e.unreferenced_path && old_enough(e.used, ORPHAN_GRACE)) {
        if !out.iter().any(|(path, _)| path == &entry.path) {
            out.push((entry.path.clone(), fsx::allocated_size(&entry.path)));
        }
    }
    out.sort();
    out
}

/// The protection check shares one inspection budget, so exhausting it must not shield every later entry: refresh the snapshot and ask again.
struct EntryGuard<'a> {
    dirs: &'a Dirs,
    protection: wrapper::DeferredProtection,
    // A snapshot that is incomplete from the start cannot be repaired by rescanning, so everything stays protected.
    unrecoverable: bool,
}

impl<'a> EntryGuard<'a> {
    fn new(dirs: &'a Dirs) -> Self {
        let protection = wrapper::deferred_protection(dirs);
        EntryGuard { dirs, unrecoverable: !protection.is_complete(), protection }
    }

    fn protects(&mut self, path: &Path) -> bool {
        if !self.protection.protects(path) {
            return false;
        }
        if self.protection.is_complete() || self.unrecoverable {
            return true;
        }
        self.protection = wrapper::deferred_protection(self.dirs);
        if !self.protection.is_complete() {
            self.unrecoverable = true;
            return true;
        }
        self.protection.protects(path)
    }
}

/// Unused for longer than the age limit; an entry whose `meta.json` is unreadable has no known last use and is left to the size cap.
fn expired(used: SystemTime, max_age: Duration) -> bool {
    used != SystemTime::UNIX_EPOCH && old_enough(used, max_age)
}

/// With `exact` false, sizes come from the recorded byte counts unless an explicit size cap is exceeded, when they are measured so eviction stays exact.
fn plan_cache(dirs: &Dirs, opts: &GcOptions, plan: &mut Plan, exact: bool, progress: &mut dyn Progress) -> HashSet<String> {
    let mut evicted = HashSet::new();
    let mut protection = EntryGuard::new(dirs);
    let scanned = scan_entries_with(dirs, progress);
    let superseded: Vec<_> = superseded_among(&scanned, opts.unit_versions.clamp(1, INCREMENTAL_ENTRIES_KEPT))
        .into_iter().filter(|(path, _)| !protection.protects(path)).collect();
    let doomed: HashSet<&Path> = superseded.iter().map(|s| s.0.as_path()).collect();
    for (path, _) in &superseded {
        if let Some(key) = path.file_name() {
            evicted.insert(key.to_string_lossy().into_owned());
        }
    }
    let shared: HashMap<PathBuf, u64> = scanned.iter().map(|e| (e.path.clone(), e.link.shared_alloc)).collect();
    let measured = |path: &Path| fsx::allocated_size(path).saturating_sub(shared.get(path).copied().unwrap_or(0));
    let sized: Vec<&ScannedEntry> = scanned.iter().filter(|e| !doomed.contains(e.path.as_path())).collect();
    progress.phase("size store entries", Some(sized.len() as u64), "sized");
    let entries: Vec<(SystemTime, PathBuf, u64)> = sized
        .into_iter()
        .map(|e| {
            let bytes = match e.estimate {
                Some(estimate) if !exact => estimate,
                _ => measured(&e.path),
            };
            progress.item(stats::STORE_SCOPE, bytes);
            (e.used, e.path.clone(), bytes)
        })
        .collect();
    plan.store_bytes = entries.iter().map(|e| e.2).chain(superseded.iter().map(|s| s.1)).fold(0u64, u64::saturating_add);
    for (path, bytes) in superseded.iter().cloned() {
        plan.removals.push(Removal { class: Class::CacheEntry, path, bytes });
    }
    let mut live = Vec::with_capacity(entries.len());
    for (used, path, bytes) in entries {
        if expired(used, opts.cache_max_age) && !protection.protects(&path) {
            if let Some(key) = path.file_name() {
                evicted.insert(key.to_string_lossy().into_owned());
            }
            plan.removals.push(Removal { class: Class::CacheEntry, path, bytes });
        } else {
            live.push((used, path, bytes));
        }
    }
    let mut entries = live;
    let sum = |entries: &[(SystemTime, PathBuf, u64)]| entries.iter().fold(0u64, |total, e| total.saturating_add(e.2));
    let mut total = sum(&entries);
    if !exact && total > opts.max_cache_bytes {
        for entry in &mut entries {
            entry.2 = measured(&entry.1);
        }
        total = sum(&entries);
    }
    entries.sort_by_key(|e| e.0);
    for (_, path, bytes) in entries {
        if total <= opts.max_cache_bytes {
            break;
        }
        if protection.protects(&path) { continue; }
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

fn plan_targets(dirs: &Dirs, opts: &GcOptions, plan: &mut Plan, progress: &mut dyn Progress) {
    plan_idle_targets(dirs, opts, plan, usize::MAX, progress);
    let targets = stats::targets_with(dirs, progress);
    for target in &targets {
        let usage = match plan.workspaces.iter().position(|usage| usage.name == target.workspace) {
            Some(at) => &mut plan.workspaces[at],
            None => {
                let root = fs::read_to_string(dirs.targets().join(&target.workspace).join("workspace")).ok().map(|root| root.trim().to_string());
                plan.workspaces.push(WorkspaceUsage { name: target.workspace.clone(), root, ..WorkspaceUsage::default() });
                plan.workspaces.last_mut().unwrap()
            }
        };
        usage.bytes = usage.bytes.saturating_add(target.allocated_bytes);
    }
    plan.workspaces.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.name.cmp(&b.name)));
    progress.phase("plan targets", Some(targets.len() as u64), "reclaimable");
    for target in targets {
        let before = plan.bytes();
        if target_path_safe(dirs, &target.path).unwrap_or(false)
            && !plan.target_retirements.contains_key(&target.path)
            && let Ok(profiles) = fs::read_dir(&target.path)
        {
            for profile in profiles.flatten() {
                plan_profile(&profile.path(), opts, plan);
            }
        }
        progress.item(&target.workspace, plan.bytes().saturating_sub(before));
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

fn plan_idle_targets(dirs: &Dirs, opts: &GcOptions, plan: &mut Plan, limit: usize, progress: &mut dyn Progress) {
    let mut candidates = idle_target_candidates(dirs, opts.target_max_idle);
    if limit != usize::MAX && !candidates.is_empty() {
        let offset = (fsx::unix_now() / 86_400) as usize % candidates.len();
        candidates.rotate_left(offset);
    }
    let attempts = limit.saturating_mul(4);
    let mut planned = 0;
    progress.phase("check idle targets", Some(candidates.len().min(attempts) as u64), "reclaimable");
    for (_, target) in candidates.into_iter().take(attempts) {
        let scope = stats::workspace_of(dirs, &target).unwrap_or_default();
        let Ok(Some(snapshot)) =
            retirement_snapshot(dirs, &target, opts.target_max_idle, limit != usize::MAX)
        else {
            progress.item(&scope, 0);
            continue;
        };
        progress.item(&scope, snapshot.bytes);
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
                if entry.file_name().to_string_lossy().starts_with("store-broker-") {
                    if let Some(proof) = store_broker::stale_run_proof(dirs, &entry.path()) {
                        plan.removals.push(Removal { class: Class::Scratch, bytes: proof.bytes, path: entry.path() });
                        plan.broker_retirements.insert(entry.path(), proof);
                    }
                    continue;
                }
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
    let Ok(children) = fs::read_dir(dirs.store()) else { return };
    for child in children.flatten() {
        if child.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            let legacy = child.file_name().to_str().is_some_and(cache::is_shard_name);
            below(&child.path(), if legacy { 3 } else { 2 }, opts, plan);
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
    plan_keylocks(dirs, opts, plan);
    plan_deferred(dirs, plan);
    plan_history(dirs, opts, plan);
    plan_hashmemo(dirs, opts, plan);
    plan_startup_memos(dirs, opts, plan);
    plan_aged_dir(&dirs.compact_memo_dir(), opts, plan);
}

/// A memo or holder log is rewritten only when its target's files change, so one untouched for a long time belongs to a target that is gone or idle.
fn plan_aged_dir(dir: &Path, opts: &GcOptions, plan: &mut Plan) {
    let Ok(entries) = fs::read_dir(dir) else { return };
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

// Flat names plus the contents of legacy shard directories.
fn keyed_files(root: &Path) -> Vec<(String, PathBuf)> {
    cache::keyed_items(root).into_iter()
        .filter_map(|path| Some((path.file_name()?.to_string_lossy().into_owned(), path)))
        .collect()
}

fn legacy_shard_files(root: &Path) -> Vec<(String, PathBuf)> {
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
    for (name, path) in keyed_files(&dirs.manifests()) {
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

/// An idle lock file is unlinked while gc holds it, and every acquirer re-checks that its file is still the one named, so retirement never lets two processes hold one key.
fn plan_keylocks(dirs: &Dirs, opts: &GcOptions, plan: &mut Plan) {
    for (name, path) in keyed_files(&dirs.keylocks()) {
        if cache::valid_key(&name) && mtime_age(&path).is_some_and(|age| age >= opts.keylock_max_age) && flock_held(&path) == Some(false) {
            plan.keylock_retirements.insert(path.clone());
            push_metadata(plan, path);
        }
    }
}

fn retire_key_lock(path: &Path) -> io::Result<bool> {
    let file = match sys::open_existing_lock_file(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    match sys::try_lock(&file, LockKind::Exclusive) {
        Ok(()) => {}
        Err(fs::TryLockError::WouldBlock) => return Ok(false),
        Err(fs::TryLockError::Error(error)) => return Err(error),
    }
    let (held, named) = (sys::fstat(&file)?, sys::lstat(path)?);
    let same = named.is_file() && held.ino() != 0 && held.dev() == named.dev() && held.ino() == named.ino();
    let removed = if same { fs::remove_file(path).map(|()| true) } else { Ok(false) };
    let _ = sys::unlock(&file);
    removed
}

/// Every slot index a store may use is canonical, so the shared worker bound never loses a lock identity.
fn canonical_slot(name: &str) -> bool {
    name.strip_prefix(DEFERRED_SLOT_PREFIX)
        .and_then(|index| index.parse::<usize>().ok().filter(|n| n.to_string() == index))
        .is_some_and(|n| n < config::MAX_STORE_SLOTS)
}

/// Collect stale jobs, temporaries and unheld noncanonical slots while preserving execution-lock identities.
fn plan_deferred(dirs: &Dirs, plan: &mut Plan) {
    let Ok(entries) = fs::read_dir(dirs.deferred()) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if !mtime_age(&path).is_some_and(|age| age > DEFERRED_MAX_AGE) {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if canonical_slot(&name) {
            continue;
        }
        let guarded = name.starts_with(DEFERRED_SLOT_PREFIX)
            || (name.starts_with("job-") || name.starts_with("claimed-")) && name.ends_with(".json");
        let Ok(meta) = sys::lstat(&path) else { continue };
        let Ok(identity) = MetadataIdentity::read(&meta) else { continue };
        let snapshot = DeferredMetadata { identity, guarded };
        let guard = if guarded {
            let Ok(Some(guard)) = deferred_file_guard(&path) else { continue };
            Some(guard)
        } else { None };
        if !snapshot.matches(&path, guard.as_ref()) { continue; }
        plan.deferred_retirements.insert(path.clone(), snapshot);
        push_metadata(plan, path);
    }
}

#[derive(Debug)]
struct DeferredMetadata {
    identity: MetadataIdentity,
    guarded: bool,
}

fn deferred_file_guard(path: &Path) -> io::Result<Option<File>> {
    match wrapper::deferred_retirement(path)? {
        wrapper::DeferredRetirement::Legacy(file) | wrapper::DeferredRetirement::Dead(file) => Ok(Some(file)),
        wrapper::DeferredRetirement::Busy => Ok(None),
    }
}

impl DeferredMetadata {
    fn matches(&self, path: &Path, guard: Option<&File>) -> bool {
        let Ok(named) = sys::lstat(path).and_then(|meta| MetadataIdentity::read(&meta)) else { return false };
        if named != self.identity || !old_enough(named.modified, DEFERRED_MAX_AGE) { return false; }
        if self.guarded {
            let Some(guard) = guard else { return false };
            if !sys::fstat(guard).and_then(|meta| MetadataIdentity::read(&meta)).is_ok_and(|opened| opened == named) { return false; }
        }
        true
    }
}

fn retire_deferred_with(path: &Path, expected: &DeferredMetadata, acquire: impl FnOnce() -> io::Result<Option<File>>) -> io::Result<bool> {
    let guard = if expected.guarded {
        let guard = match acquire() {
            Ok(guard) => guard,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        let Some(guard) = guard else { return Ok(false) };
        Some(guard)
    } else { None };
    // Both legacy and versioned jobs retain an exclusive inode lease through the final name check and unlink.
    if !expected.matches(path, guard.as_ref()) { return Ok(false); }
    match fsx::remove_any(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
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

impl UnitRetirement {
    fn references_absent(&self, store: &cache::Cache) -> bool {
        // Existing roots, unsafe parent types, and I/O uncertainty retain the record.
        self.keys.iter().all(|key| store.unit_reference_absent(key))
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
    if let Ok(entries) = fs::read_dir(dirs.hashmemo()) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if fsx::temp_owner(&name).is_some() && stale_temp(&entry.path(), &name, opts) {
                plan.removals.push(Removal { class: Class::Scratch, bytes: fsx::allocated_size(&entry.path()), path: entry.path() });
            }
        }
    }
    if dirs.hashmemo().join(cache::hashing::LOG_NAME).is_file() {
        plan.hashmemo_log_age = Some(opts.hashmemo_max_age);
    }
    for (name, path) in legacy_shard_files(&dirs.hashmemo()) {
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
    if let Some(pack) = store.read_unit_pack_for_gc() {
        let stale: Vec<_> = pack.into_iter()
            .filter(|(_, stamp, keys)| fsx::unix_now().saturating_sub(*stamp) > opts.hashmemo_max_age.as_secs() && keys.iter().all(|key| store.unit_reference_absent(key)))
            .collect();
        if !stale.is_empty() {
            let path = store.unit_pack_path();
            plan.pack_retirements = stale;
            plan.pack_max_idle = opts.hashmemo_max_age;
            plan.removals.push(Removal { class: Class::Metadata, bytes: fsx::allocated_size(&path).min(4096), path });
        }
    }
    for (name, path) in keyed_files(&store.units_dir()) {
        let units = store.units_dir();
        if !cache::valid_key(&name) || (path.parent() != Some(units.as_path()) && path.parent() != Some(units.join(&name[..2]).as_path())) { continue; }
        if !mtime_age(&path).is_some_and(|age| age > opts.hashmemo_max_age) { continue; }
        let Ok(snapshot) = unit_snapshot(&store, &path, opts.hashmemo_max_age) else { continue };
        if snapshot.eligible(&store) {
            plan.unit_retirements.insert(path.clone(), snapshot);
            push_metadata(plan, path);
        }
    }

}

pub fn execute(dirs: &Dirs, plan: &Plan) -> (u64, Vec<(PathBuf, std::io::Error)>) {
    execute_with(dirs, plan, &mut Silent)
}

/// The bytes of every `remove` and `tidy` item add up to the freed total returned.
pub fn execute_with(dirs: &Dirs, plan: &Plan, progress: &mut dyn Progress) -> (u64, Vec<(PathBuf, std::io::Error)>) {
    let (mut freed, failures) = execute_removals(dirs, plan, progress);
    progress.phase("tidy", Some(1), "freed");
    let compacted = plan.hashmemo_log_age.map_or(0, |age| cache::hashing::compact_memo_log(&dirs.hashmemo(), age.as_secs()));
    freed += compacted;
    progress.item("hash memo log", compacted);
    cache::Cache::new(dirs).migrate_legacy_layout();
    prune_empty_shards(&dirs.hashmemo());
    prune_empty_shards(&dirs.keylocks());
    prune_empty_workspaces(dirs);
    (freed, failures)
}

/// Legacy records lived in two character shard directories; once emptied they go, and a directory that still holds anything stays.
fn prune_empty_shards(root: &Path) {
    let Ok(shards) = fs::read_dir(root) else { return };
    for shard in shards.flatten() {
        if shard.file_type().is_ok_and(|kind| kind.is_dir()) && shard.file_name().to_str().is_some_and(cache::is_shard_name) {
            let _ = fs::remove_dir(shard.path());
        }
    }
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

fn execute_removals(dirs: &Dirs, plan: &Plan, progress: &mut dyn Progress) -> (u64, Vec<(PathBuf, std::io::Error)>) {
    let mut freed = 0;
    let mut failures = Vec::new();
    let locks = dirs.locks();
    let store = cache::Cache::new(dirs);
    let first_pass = |removal: &Removal| removal.class == Class::CacheEntry || removal.path.starts_with(store.units_dir())
        || plan.manifest_retirements.contains_key(&removal.path) || plan.keylock_retirements.contains(&removal.path);
    let scope_of = |removal: &Removal| match stats::workspace_of(dirs, &removal.path) {
        Some(workspace) => format!("{workspace}: {}", removal.class.label()),
        None => removal.class.label().to_string(),
    };
    progress.phase("remove", Some(plan.removals.len() as u64), "freed");
    // Finish cache and metadata retirement before acquiring any profile locks.
    for removal in plan.removals.iter().filter(|removal| first_pass(removal)) {
        let before = freed;
        'one: {
            if removal.class == Class::CacheEntry {
                let Some(snapshot) = plan.cache_retirements.get(&removal.path) else { break 'one };
                let protection = wrapper::deferred_protection(dirs);
                if protection.protects(&removal.path) { break 'one; }
                match store.try_retire_entry(snapshot) {
                    Ok(cache::EntryRetirementOutcome::Retired) => freed += removal.bytes,
                    Ok(cache::EntryRetirementOutcome::Busy | cache::EntryRetirementOutcome::Stale) => {}
                    Err(error) => failures.push((removal.path.clone(), error)),
                }
                break 'one;
            }
            if removal.path == store.unit_pack_path() {
                if store.retire_idle_unit_records(&plan.pack_retirements, plan.pack_max_idle) > 0 { freed += removal.bytes; }
                break 'one;
            }
            if plan.keylock_retirements.contains(&removal.path) {
                match retire_key_lock(&removal.path) {
                    Ok(true) => freed += removal.bytes,
                    Ok(false) => {}
                    Err(error) => failures.push((removal.path.clone(), error)),
                }
                break 'one;
            }
            let unit = plan.unit_retirements.get(&removal.path);
            let manifest = plan.manifest_retirements.get(&removal.path);
            if unit.is_none() && manifest.is_none() { break 'one; }
            let Ok(Some(_guard)) = cache::UnitRecordsGuard::try_acquire(dirs) else { break 'one };
            if unit.is_some_and(|snapshot| !snapshot.still_eligible(&store, &removal.path))
                || manifest.is_some_and(|snapshot| !snapshot.still_eligible(&store, &removal.path)) { break 'one; }
            match fs::remove_file(&removal.path) {
                Ok(()) => freed += removal.bytes,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => failures.push((removal.path.clone(), error)),
            }
        }
        progress.item(&scope_of(removal), freed.saturating_sub(before));
    }
    // Cargo's locks stay held on each profile for the rest of the run, and variants are rechecked under them.
    let mut profiles: HashMap<PathBuf, Option<ProfileGuard>> = HashMap::new();
    for removal in plan.removals.iter().filter(|removal| !first_pass(removal)) {
        let before = freed;
        'one: {
            if removal.path.parent() == Some(dirs.deferred().as_path()) {
                let Some(expected) = plan.deferred_retirements.get(&removal.path) else { break 'one };
                match retire_deferred_with(&removal.path, expected, || deferred_file_guard(&removal.path)) {
                    Ok(true) => freed += removal.bytes,
                    Ok(false) => {}
                    Err(error) => failures.push((removal.path.clone(), error)),
                }
                break 'one;
            }
            if let Some(proof) = plan.broker_retirements.get(&removal.path) {
                if let Err(error) = store_broker::retire_stale_run(dirs, &removal.path, proof, &mut freed) {
                    failures.push((removal.path.clone(), error));
                }
                break 'one;
            }
            if removal.path.parent() == Some(cache::evicted_dir(dirs).as_path()) {
                let Some(proof) = plan.retired_entries.get(&removal.path) else { break 'one };
                if !proof.valid(dirs, &removal.path) { break 'one; }
                match remove_retired_child(&removal.path, || proof.valid(dirs, &removal.path)) {
                    Ok(bytes) => freed += bytes,
                    Err(error) => failures.push((removal.path.clone(), error)),
                }
                break 'one;
            }
            let mut incremental_root = None;
            let mut deferred_protection = None;
            if removal.class == Class::TargetDir {
                if let Some(snapshot) = plan.target_retirements.get(&removal.path) {
                    let mut bytes = 0;
                    let result = retire_target(dirs, &removal.path, snapshot, &mut bytes);
                    freed += bytes;
                    if let Err(error) = result {
                        failures.push((removal.path.clone(), error));
                    }
                }
                break 'one;
            }
            let profile = compact::profile_of(&removal.path);
            if matches!(removal.class, Class::StaleVariant | Class::IncrementalSession)
                || removal.class == Class::Scratch && profile.is_some()
            {
                let Some(profile) = profile else { break 'one };
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
                    break 'one;
                };
                let protection = wrapper::deferred_protection(dirs);
                let protected = match sys::lstat(&removal.path) {
                    Ok(meta) if meta.is_file() => protection.protects_file(&removal.path, &meta),
                    _ => protection.protects(&removal.path),
                };
                if protected { break 'one; }
                deferred_protection = Some(protection);
                if removal.class == Class::StaleVariant && !guard.still_superseded(&removal.path) {
                    break 'one;
                }
                if removal.class == Class::IncrementalSession && !guard.incremental.contains(&removal.path) {
                    break 'one;
                }
                let root = profile.join("incremental");
                if removal.path.starts_with(&root) {
                    incremental_root = Some(root);
                }
            }
            // A build may have started using a lock directory since planning.
            if removal.class == Class::Metadata
                && removal.path.parent() == Some(locks.as_path())
                && (removal.path.file_name().is_some_and(|name| name.as_encoded_bytes().starts_with(b"store-queue-"))
                    || !lock_dir_stale(&removal.path, plan.lock_max_idle))
            {
                break 'one;
            }
            let result = match incremental_root {
                Some(root) => compact::remove_incremental_payload(&root, &removal.path),
                None => fsx::remove_any(&removal.path),
            };
            match result {
                Ok(()) => freed += removal.bytes,
                Err(err) => failures.push((removal.path.clone(), err)),
            }
            drop(deferred_protection);
        }
        progress.item(&scope_of(removal), freed.saturating_sub(before));
    }
    drop(profiles);
    if plan.trim_ledger {
        let _ = crate::events::trim_ledger(dirs);
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
    let protection = wrapper::deferred_protection(dirs);
    for profile in &expected.profiles {
        if protection.protects(&profile.join("incremental")) { continue; }
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
    let protection = wrapper::deferred_protection(dirs);
    let profiles: Vec<_> = expected.profiles.iter().filter(|profile| !protection.protects(profile)).cloned().collect();
    compact::remove_profile_payloads_with(&profiles, |_, bytes| {
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
        let _ = execute_removals(dirs, &plan, &mut Silent);
    }
}

fn run_maintenance(dirs: &Dirs, cfg: &Config, drain: bool) {
    let opts = GcOptions::from_config(cfg);
    let plan = plan_maintenance(dirs, &opts);
    let _ = execute_removals(dirs, &plan, &mut Silent);
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
    let mut cmd = Command::new(exe);
    cmd.args(["gc", AUTO_CHILD_FLAG]).env("FRUST_HOME", dirs.root());
    if cfg.cache_max_bytes == u64::MAX {
        cmd.env_remove("FRUST_CACHE_MAX_GIB");
    } else {
        cmd.env("FRUST_CACHE_MAX_GIB", (cfg.cache_max_bytes as f64 / (1024.0 * 1024.0 * 1024.0)).to_string());
    }
    cmd.env("FRUST_CACHE_MAX_AGE_DAYS", (config::cache_max_age().as_secs_f64() / 86_400.0).to_string());
    sys::spawn_detached(&mut cmd, &[]).is_ok()
}

/// Never recursive: a build may create its target between the check and the removal, which then fails as not empty.
fn prune_empty_workspaces(dirs: &Dirs) {
    let Ok(workspaces) = fs::read_dir(dirs.targets()) else { return };
    for ws in workspaces.flatten() {
        if !ws.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let path = ws.path();
        let files = fs::read_dir(&path).and_then(|entries| {
            entries.map(|entry| entry.and_then(|entry| Ok((entry.path(), entry.file_type()?.is_file())))).collect::<io::Result<Vec<_>>>()
        });
        let Ok(files) = files else { continue };
        if !files.iter().all(|(_, file)| *file) {
            continue;
        }
        let marker = path.join("workspace");
        let recorded = fs::read(&marker).ok();
        for (file, _) in &files {
            let _ = fs::remove_file(file);
        }
        if fs::remove_dir(&path).is_err()
            && let Some(recorded) = recorded
            && !marker.exists()
        {
            let _ = fsx::write_atomic(&marker, &recorded);
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

    #[test]
    fn workspace_pruning_never_removes_a_target_a_build_creates_after_the_check() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let root = home("prune-race");
        let dirs = Dirs::at(&root);
        fs::create_dir_all(dirs.targets()).unwrap();
        let (stop, lost, built) = (AtomicBool::new(false), AtomicUsize::new(0), AtomicUsize::new(0));
        std::thread::scope(|scope| {
            scope.spawn(|| while !stop.load(Ordering::Relaxed) { prune_empty_workspaces(&dirs); });
            for round in 0..4000 {
                // fcargo creates the workspace with its marker, then cargo creates and locks the profile it writes into.
                let workspace = dirs.targets().join(format!("ws-{}", round % 8));
                let _ = fs::create_dir_all(&workspace);
                let _ = fs::write(workspace.join("workspace"), "/w");
                let profile = workspace.join("tc").join("harness");
                if fs::create_dir_all(profile.join("deps")).is_err() { continue; }
                let Ok(lock) = File::options().read(true).write(true).create(true).truncate(false).open(profile.join(".cargo-lock")) else { continue };
                if sys::try_lock(&lock, LockKind::Exclusive).is_err() { continue; }
                built.fetch_add(1, Ordering::Relaxed);
                for unit in 0..4 {
                    if fs::write(profile.join("deps").join(format!("libu{unit}.rmeta")), b"m").is_err() {
                        lost.fetch_add(1, Ordering::Relaxed);
                        break;
                    }
                }
                drop(lock);
                let _ = fs::remove_dir_all(workspace.join("tc"));
            }
            stop.store(true, Ordering::Relaxed);
        });
        assert!(built.load(Ordering::Relaxed) > 0);
        assert_eq!(lost.load(Ordering::Relaxed), 0, "gc removed a profile while cargo held its lock");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn progress_covers_every_phase_in_order_and_adds_up_to_the_plan_and_the_freed_total() {
        use crate::progress::Recorder;
        let root = home("progress-events");
        let dirs = Dirs::at(&root);
        for key in ["aa", "bb"] {
            store_entry(&root, &key.repeat(20), &[("artifact", 16384)], 5000);
        }
        for ws in ["alpha-0000", "beta-0000"] {
            let profile = dirs.targets().join(ws).join("tc").join("harness");
            fs::create_dir_all(profile.join(".fingerprint")).unwrap();
            fs::create_dir_all(profile.join("deps")).unwrap();
            fs::write(dirs.targets().join(ws).join("workspace"), format!("/w/{ws}")).unwrap();
            for (days, hash) in [(8, "0123456789abcde0"), (9, "0123456789abcde1"), (10, "0123456789abcde2")] {
                let file = profile.join("deps").join(format!("libunit-{hash}.rlib"));
                fs::write(&file, vec![7u8; 8192]).unwrap();
                File::options().write(true).open(&file).unwrap().set_modified(SystemTime::now() - Duration::from_secs(days * 86_400)).unwrap();
            }
        }
        let mut planning = Recorder::default();
        let planned = plan_with(&dirs, &GcOptions::new(0), &mut planning);
        assert_eq!(planning.phases(), ["scan store", "size store entries", "check idle targets", "measure targets", "plan targets", "plan home"]);
        for phase in planning.phases() {
            let (totals, items) = planning.items(phase);
            assert_eq!(totals, [Some(items.len() as u64)], "{phase} announced its real item count");
        }
        let measured: Vec<String> = planning.items("measure targets").1.into_iter().map(|(scope, _)| scope).collect();
        assert_eq!(measured.len(), 2);
        assert!(measured.contains(&"alpha-0000".to_string()) && measured.contains(&"beta-0000".to_string()), "{measured:?}");
        assert_eq!(planning.bytes("size store entries"), planned.store_bytes);
        let names: Vec<&str> = planned.workspaces.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(names.len(), 2, "{names:?}");
        for usage in &planned.workspaces {
            assert_eq!((usage.items, usage.root.as_deref()), (1, Some(format!("/w/{}", usage.name).as_str())), "{usage:?}");
            assert!(usage.freed > 0 && usage.bytes >= usage.freed, "{usage:?}");
        }
        assert_eq!(planning.bytes("plan targets"), planned.workspaces.iter().map(|w| w.freed).sum::<u64>());
        assert_eq!(planned.store_removals(&dirs).0, 2);

        let mut executing = Recorder::default();
        let (freed, failures) = execute_with(&dirs, &planned, &mut executing);
        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(executing.phases(), ["remove", "tidy"]);
        let (totals, items) = executing.items("remove");
        assert_eq!(totals, [Some(planned.removals.len() as u64)]);
        assert_eq!(items.len(), planned.removals.len(), "every removal is reported exactly once");
        assert!(items.iter().any(|(scope, _)| scope.starts_with("alpha-0000: ")) && items.iter().any(|(scope, _)| scope.starts_with("beta-0000: ")), "{items:?}");
        assert_eq!(executing.bytes("remove") + executing.bytes("tidy"), freed);
        assert_eq!(freed, planned.bytes(), "everything planned was free to go");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn workspace_pruning_removes_only_workspaces_without_a_target() {
        let root = home("prune-empty");
        let dirs = Dirs::at(&root);
        let empty = dirs.targets().join("empty");
        let held = dirs.targets().join("held");
        fs::create_dir_all(&empty).unwrap();
        fs::create_dir_all(held.join("tc").join("harness").join("deps")).unwrap();
        for ws in [&empty, &held] {
            fs::write(ws.join("workspace"), "/w").unwrap();
        }
        fs::write(empty.join("leftover"), "x").unwrap();
        prune_empty_workspaces(&dirs);
        assert!(!empty.exists());
        assert!(held.join("tc").join("harness").join("deps").is_dir());
        assert_eq!(fs::read_to_string(held.join("workspace")).unwrap(), "/w");
        fs::remove_dir_all(root).unwrap();
    }

    fn home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("frust-gc-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    fn deferred_job(dirs: &Dirs, output: Option<&Path>, ownership: Option<u32>, age: Duration) -> PathBuf {
        let out = output.map_or_else(|| dirs.root().join("target/debug/deps"), |path| path.parent().unwrap().to_path_buf());
        let outputs: Vec<_> = output.into_iter().map(|path| {
            let meta = sys::lstat(path).unwrap();
            let ns = |time: (i64, i64)| (time.0 as i128 * 1_000_000_000 + time.1 as i128).to_string();
            serde_json::json!([path.file_name().unwrap().to_str().unwrap(), {
                "dev": meta.dev(), "ino": meta.ino(), "size": meta.len(),
                "mtime_ns": ns(meta.mtime()), "ctime_ns": ns(meta.ctime())
            }])
        }).collect();
        let mut job = serde_json::json!({
            "format": 3, "context": {"package_version":"test", "compression_requested":true, "immutable_source_roots":[]},
            "home": dirs.root(), "cwd": dirs.root(), "args": ["rustc", "--crate-name", "pending", "--crate-type", "lib", "--emit=dep-info,link", "--out-dir", out, "input.rs"],
            "prekey": "ab".repeat(20), "unit":null, "trust":"on", "compile_ms":1, "spawned_ns":"1", "captured":"",
            "watched":[], "extern_hashes":[], "names":[], "dep_name":"pending.d", "dep_text":"", "outputs":outputs, "inputs":[], "slot_lock":null, "key_lock":null
        });
        if let Some(version) = ownership { job["ownership_version"] = version.into(); }
        fs::create_dir_all(dirs.deferred()).unwrap();
        let path = dirs.deferred().join("job-1-1.json");
        fs::write(&path, serde_json::to_vec(&job).unwrap()).unwrap();
        File::open(&path).unwrap().set_modified(SystemTime::now() - age).unwrap();
        path
    }

    #[cfg(unix)]
    fn shared_job(path: &Path) -> File {
        let file = File::open(path).unwrap();
        sys::try_lock(&file, LockKind::Shared).unwrap();
        file
    }

    #[cfg(unix)]
    #[test]
    fn deferred_old_live_lease_survives_planning_and_released_record_collects() {
        if crate::isolated_test(module_path!(), "deferred_old_live_lease_survives_planning_and_released_record_collects") {
            return;
        }
        let root = home("deferred-live-age");
        let dirs = Dirs::at(&root);
        let path = deferred_job(&dirs, None, Some(1), DEFERRED_MAX_AGE * 2);
        assert!(matches!(wrapper::deferred_retirement(&path).unwrap(), wrapper::DeferredRetirement::Dead(_)));
        let held = shared_job(&path);
        let mut selected = Plan::default();
        plan_deferred(&dirs, &mut selected);
        assert!(selected.removals.is_empty(), "an accepted job does not expire by elapsed age");
        assert!(execute_removals(&dirs, &selected, &mut Silent).1.is_empty());
        assert!(path.exists());
        drop(held);
        plan_deferred(&dirs, &mut selected);
        assert!(selected.deferred_retirements.contains_key(&path));
        assert!(execute_removals(&dirs, &selected, &mut Silent).1.is_empty());
        assert!(!path.exists(), "an unowned old record remains collectible");
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn deferred_lease_acquired_after_planning_blocks_execution() {
        if crate::isolated_test(module_path!(), "deferred_lease_acquired_after_planning_blocks_execution") {
            return;
        }
        let root = home("deferred-late-lease");
        let dirs = Dirs::at(&root);
        let path = deferred_job(&dirs, None, Some(1), DEFERRED_MAX_AGE * 2);
        let mut selected = Plan::default();
        plan_deferred(&dirs, &mut selected);
        assert!(selected.deferred_retirements.contains_key(&path));
        let held = shared_job(&path);
        assert_eq!(execute_removals(&dirs, &selected, &mut Silent).0, 0);
        assert!(path.exists());
        drop(held);
        assert!(execute_removals(&dirs, &selected, &mut Silent).1.is_empty());
        assert!(!path.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn deferred_age_refresh_after_planning_declines_retirement() {
        if crate::isolated_test(module_path!(), "deferred_age_refresh_after_planning_declines_retirement") {
            return;
        }
        let root = home("deferred-refreshed-age");
        let dirs = Dirs::at(&root);
        let path = deferred_job(&dirs, None, Some(1), DEFERRED_MAX_AGE * 2);
        let mut selected = Plan::default();
        plan_deferred(&dirs, &mut selected);
        assert!(selected.deferred_retirements.contains_key(&path));
        File::open(&path).unwrap().set_modified(SystemTime::now()).unwrap();
        assert!(execute_removals(&dirs, &selected, &mut Silent).1.is_empty());
        assert!(path.exists(), "planning does not authorize deleting a refreshed record");
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn deferred_replacement_during_guard_acquisition_is_retained() {
        if crate::isolated_test(module_path!(), "deferred_replacement_during_guard_acquisition_is_retained") {
            return;
        }
        let root = home("deferred-guard-replacement");
        let dirs = Dirs::at(&root);
        let path = deferred_job(&dirs, None, Some(1), DEFERRED_MAX_AGE * 2);
        let mut selected = Plan::default();
        plan_deferred(&dirs, &mut selected);
        let expected = &selected.deferred_retirements[&path];
        let parked = dirs.deferred().join("old-inode");
        let removed = retire_deferred_with(&path, expected, || {
            let guard = deferred_file_guard(&path)?.unwrap();
            let contender = File::open(&path)?;
            assert!(matches!(sys::try_lock(&contender, LockKind::Shared), Err(fs::TryLockError::WouldBlock)));
            sys::rename(&path, &parked)?;
            fs::write(&path, b"replacement")?;
            File::open(&path)?.set_modified(expected.identity.modified)?;
            Ok(Some(guard))
        }).unwrap();
        assert!(!removed);
        assert_eq!(fs::read(&path).unwrap(), b"replacement");
        assert!(parked.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn every_slot_a_store_can_use_is_canonical() {
        for index in 0..config::MAX_STORE_SLOTS {
            assert!(canonical_slot(&format!("{DEFERRED_SLOT_PREFIX}{index}")), "slot {index}");
        }
        for name in ["slot-16", "slot-04", "slot-", "slot-x", "slot--1", "job-1-2.json"] {
            assert!(!canonical_slot(name), "{name}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn stale_queued_and_claimed_jobs_are_collected_but_fresh_ones_and_high_slots_stay() {
        let root = home("deferred-queued-claimed");
        let dirs = Dirs::at(&root);
        fs::create_dir_all(dirs.deferred()).unwrap();
        let key = "ab".repeat(20);
        let old = SystemTime::now() - DEFERRED_MAX_AGE * 2;
        let stale: Vec<PathBuf> = [format!("job-1-2-q{key}.json"), format!("claimed-1-2-q{key}.json"), format!("claimed-3-4-q{key}.json")]
            .iter().map(|name| dirs.deferred().join(name)).collect();
        let fresh = dirs.deferred().join(format!("claimed-5-6-q{key}.json"));
        let slot = dirs.deferred().join(format!("{DEFERRED_SLOT_PREFIX}7"));
        for file in stale.iter().chain([&fresh, &slot]) {
            fs::write(file, []).unwrap();
        }
        for file in stale.iter().chain([&slot]) {
            File::open(file).unwrap().set_modified(old).unwrap();
        }
        let mut selected = Plan::default();
        plan_deferred(&dirs, &mut selected);
        for file in &stale {
            assert!(selected.deferred_retirements.contains_key(file), "{}", file.display());
        }
        assert!(!selected.deferred_retirements.contains_key(&fresh));
        assert!(!selected.deferred_retirements.contains_key(&slot), "a slot below the maximum is never retired");
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn deferred_legacy_age_and_canonical_lock_policies_are_preserved() {
        if crate::isolated_test(module_path!(), "deferred_legacy_age_and_canonical_lock_policies_are_preserved") {
            return;
        }
        let root = home("deferred-legacy-policy");
        let dirs = Dirs::at(&root);
        let path = deferred_job(&dirs, None, None, Duration::ZERO);
        assert!(matches!(wrapper::deferred_retirement(&path).unwrap(), wrapper::DeferredRetirement::Legacy(_)));
        let mut selected = Plan::default();
        plan_deferred(&dirs, &mut selected);
        assert!(selected.removals.is_empty(), "an unlocked fresh legacy record is not dead");
        File::open(&path).unwrap().set_modified(SystemTime::now() - DEFERRED_MAX_AGE * 2).unwrap();
        let canonical = dirs.deferred().join(format!("{DEFERRED_SLOT_PREFIX}0"));
        let noncanonical = dirs.deferred().join(format!("{DEFERRED_SLOT_PREFIX}20"));
        let stray = dirs.deferred().join("orphan.tmp");
        for file in [&canonical, &noncanonical, &stray] {
            fs::write(file, []).unwrap();
            File::open(file).unwrap().set_modified(SystemTime::now() - DEFERRED_MAX_AGE * 2).unwrap();
        }
        let held = shared_job(&noncanonical);
        plan_deferred(&dirs, &mut selected);
        assert!(selected.deferred_retirements.contains_key(&path));
        assert!(!selected.deferred_retirements.contains_key(&canonical));
        assert!(!selected.deferred_retirements.contains_key(&noncanonical));
        fs::create_dir_all(dirs.locks()).unwrap();
        let reservation = dirs.locks().join("store-queue-0");
        fs::write(&reservation, []).unwrap();
        selected.removals.push(Removal { class: Class::Metadata, path: reservation.clone(), bytes: 0 });
        assert!(execute_removals(&dirs, &selected, &mut Silent).1.is_empty());
        assert!(!path.exists() && !stray.exists());
        assert!(canonical.exists() && noncanonical.exists() && reservation.exists());
        drop(held);
        fs::write(&path, b"malformed aged legacy metadata").unwrap();
        File::open(&path).unwrap().set_modified(SystemTime::now() - DEFERRED_MAX_AGE * 2).unwrap();
        let mut selected = Plan::default();
        plan_deferred(&dirs, &mut selected);
        assert!(execute_removals(&dirs, &selected, &mut Silent).1.is_empty());
        assert!(!path.exists() && !noncanonical.exists() && canonical.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    fn aged_gc_target(tag: &str) -> (PathBuf, Dirs, PathBuf, Vec<PathBuf>) {
        let root = home(tag);
        let dirs = Dirs::at(&root);
        let target = dirs.targets().join("workspace/toolchain");
        let mut payloads = Vec::new();
        for name in ["debug", "release"] {
            let profile = target.join(name);
            for child in ["deps", ".fingerprint/unit", "incremental"] { fs::create_dir_all(profile.join(child)).unwrap(); }
            let payload = profile.join("deps/libunit-0123456789abcdef.rlib");
            fs::write(&payload, vec![7; 16384]).unwrap();
            fs::write(profile.join(".fingerprint/unit/lib-unit"), b"fingerprint").unwrap();
            payloads.push(payload);
        }
        let scope = lock::Scope::All;
        let request = lock::Request { command: "cargo build", fingerprint: "", scope: &scope, join: false, timeout: Some(Duration::from_secs(1)) };
        match lock::acquire(&dirs, &target, request, &mut |_| {}).unwrap() {
            lock::Acquired::Lock(guard) => drop(guard),
            lock::Acquired::Failed(_) => panic!("unexpected build admission failure"),
        }
        age_gc_tree(&root);
        (root, dirs, target, payloads)
    }

    #[cfg(unix)]
    fn age_gc_tree(path: &Path) {
        if path.is_dir() { for child in fs::read_dir(path).unwrap() { age_gc_tree(&child.unwrap().path()); } }
        File::open(path).unwrap().set_modified(SystemTime::now() - Duration::from_secs(40 * 86_400)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn deferred_target_snapshot_refresh_protects_descendants_and_inode_aliases() {
        if crate::isolated_test(module_path!(), "deferred_target_snapshot_refresh_protects_descendants_and_inode_aliases") {
            return;
        }
        for alias in [false, true] {
            let (root, dirs, target, payloads) = aged_gc_target(if alias { "deferred-target-alias" } else { "deferred-target-descendant" });
            let selected = plan(&dirs, &GcOptions::new(u64::MAX));
            assert!(selected.target_retirements.contains_key(&target));
            let output = if alias {
                let path = root.join("external-pending.rlib");
                fs::hard_link(&payloads[0], &path).unwrap();
                path
            } else { payloads[0].clone() };
            // Publishing after planning exercises the execution snapshot under native profile admission.
            let path = deferred_job(&dirs, Some(&output), Some(1), Duration::ZERO);
            let held = shared_job(&path);
            let (_, failures) = execute_removals(&dirs, &selected, &mut Silent);
            assert!(failures.is_empty(), "{failures:?}");
            assert!(payloads[0].exists(), "a protected descendant or inode alias retains the profile payload");
            assert!(!payloads[1].exists(), "an unrelated profile remains reclaimable");
            assert!(path.exists());
            drop(held);
            age_gc_tree(&target);
            let selected = plan(&dirs, &GcOptions::new(u64::MAX));
            assert!(execute_removals(&dirs, &selected, &mut Silent).1.is_empty());
            assert!(!payloads[0].exists());
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn deferred_cache_input_alias_is_refreshed_before_entry_retirement() {
        if crate::isolated_test(module_path!(), "deferred_cache_input_alias_is_refreshed_before_entry_retirement") {
            return;
        }
        let root = home("deferred-cache-alias");
        let dirs = Dirs::at(&root);
        let protected = store_entry(&root, &"aa".repeat(20), &[("artifact", 16384)], 5000);
        let unrelated = store_entry(&root, &"bb".repeat(20), &[("artifact", 16384)], 5000);
        let selected = plan(&dirs, &GcOptions::new(0));
        assert_eq!(cache_removals(&selected).len(), 2);
        let output = root.join("external-pending.rlib");
        fs::hard_link(protected.join("artifact"), &output).unwrap();
        let path = deferred_job(&dirs, Some(&output), Some(1), Duration::ZERO);
        let held = shared_job(&path);
        assert!(execute_removals(&dirs, &selected, &mut Silent).1.is_empty());
        assert!(protected.exists() && !unrelated.exists());
        let selected = plan(&dirs, &GcOptions::new(0));
        assert!(cache_removals(&selected).is_empty(), "observed pending aliases also filter cache planning");
        drop(held);
        let selected = plan(&dirs, &GcOptions::new(0));
        assert!(execute_removals(&dirs, &selected, &mut Silent).1.is_empty());
        assert!(!protected.exists());
        fs::remove_dir_all(root).unwrap();
    }

    fn store_entry(home: &Path, key: &str, files: &[(&str, u64)], age_secs: u64) -> PathBuf {
        let dir = home.join("store").join(key);
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
        assert!(execute_removals(&dirs, &selected, &mut Silent).1.is_empty());
        assert!(store.has_entry(&key));
        assert!(misplaced.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cache_gc_retires_selected_states_and_prunes_their_manifest_and_unit_immediately() {
        let root = home("cache-manifest-retirement");
        let dirs = Dirs::at(&root);
        let store = cache::Cache::new(&dirs);
        let prekey = "ab".repeat(20);
        let unit = "ac".repeat(20);
        let mut paths = Vec::new();
        for n in 1..=3 {
            let key = format!("{n:040x}");
            let path = store_entry(&root, &key, &[("libx-0123456789abcdef.rlib", 64), ("x-0123456789abcdef.c1.t1.rcgu.o", 64)], 0);
            let mut meta: cache::EntryMeta = serde_json::from_slice(&fs::read(path.join("meta.json")).unwrap()).unwrap();
            meta.prekey = Some(prekey.clone());
            meta.incremental_unit = Some(unit.clone());
            fs::write(path.join("meta.json"), serde_json::to_vec(&meta).unwrap()).unwrap();
            store.add_candidate(&prekey, cache::Candidate { inputs: Vec::new(), envs: Vec::new(), key: key.clone() });
            store.note_unit_entry(&unit, &key, 3);
            File::options().write(true).open(path.join("meta.json")).unwrap().set_modified(SystemTime::now() - Duration::from_secs((4 - n) * 5000)).unwrap();
            paths.push(path);
        }
        let opts = GcOptions::new(u64::MAX);
        let delayed = plan(&dirs, &opts);
        assert_eq!(cache_removals(&delayed), paths[..1]);
        store.mark_used(&format!("{:040x}", 1));
        let (_, failures) = execute_removals(&dirs, &delayed, &mut Silent);
        assert!(failures.is_empty());
        assert!(paths[0].exists(), "usage after planning invalidates retirement");
        let current = plan(&dirs, &opts);
        assert_eq!(cache_removals(&current), paths[1..2]);
        let (_, failures) = execute_removals(&dirs, &current, &mut Silent);
        assert!(failures.is_empty());
        assert!(!paths[1].exists());
        assert_eq!(store.candidates(&prekey).into_iter().map(|row| row.key).collect::<Vec<_>>(), vec![format!("{:040x}", 3), format!("{:040x}", 1)]);
        assert_eq!(store.unit_keys(&unit), vec![format!("{:040x}", 3), format!("{:040x}", 1)]);
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
        let path = dirs.manifests().join(format!("{prekey}.json"));
        let old = SystemTime::now() - Duration::from_secs(60 * 86_400);
        for path in [&path, &entry.join("meta.json")] { File::options().write(true).open(path).unwrap().set_modified(old).unwrap(); }
        let selected = plan(&dirs, &GcOptions { cache_max_age: Duration::from_secs(365 * 86_400), ..GcOptions::new(u64::MAX) });
        assert!(selected.manifest_retirements.contains_key(&path));
        let guard = cache::UnitRecordsGuard::try_acquire(&dirs).unwrap().unwrap();
        assert!(execute_removals(&dirs, &selected, &mut Silent).1.is_empty());
        assert!(path.exists(), "validation and unlink require the same guard");
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let writer = store.clone();
        let writer_prekey = prekey.clone();
        let worker = std::thread::spawn(move || { ready_tx.send(()).unwrap(); writer.add_candidate(&writer_prekey, row); });
        ready_rx.recv().unwrap();
        drop(guard);
        worker.join().unwrap();
        assert!(execute_removals(&dirs, &selected, &mut Silent).1.is_empty());
        assert!(path.exists());
        assert_eq!(store.candidates(&prekey).len(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    fn cache_removals(plan: &Plan) -> Vec<PathBuf> {
        plan.removals.iter().filter(|r| r.class == Class::CacheEntry).map(|r| r.path.clone()).collect()
    }

    const FIFTEEN_DAYS: u64 = 15 * 86_400;

    #[test]
    fn entries_unused_past_the_age_limit_are_expired_by_both_planners() {
        let home = home("expiry");
        let dirs = Dirs::at(&home);
        let stale = store_entry(&home, &format!("aa{}", "1".repeat(38)), &[("f0", 64)], FIFTEEN_DAYS);
        let fresh = store_entry(&home, &format!("bb{}", "2".repeat(38)), &[("f0", 64)], 86_400);
        let opts = GcOptions::new(u64::MAX);
        assert_eq!(cache_removals(&plan(&dirs, &opts)), vec![stale.clone()]);
        assert_eq!(cache_removals(&plan_maintenance(&dirs, &opts)), vec![stale]);
        let short = GcOptions { cache_max_age: Duration::from_secs(3600), ..GcOptions::new(u64::MAX) };
        assert_eq!(cache_removals(&plan(&dirs, &short)).len(), 2);
        assert_eq!(cache_removals(&plan_maintenance(&dirs, &short)).len(), 2);
        assert!(fresh.exists());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn entries_without_readable_metadata_are_not_aged_out() {
        let home = home("expiry-no-meta");
        let dirs = Dirs::at(&home);
        let bare = home.join("store").join("aa").join("aa".repeat(20));
        fs::create_dir_all(&bare).unwrap();
        fs::write(bare.join("f0"), [1u8; 64]).unwrap();
        let opts = GcOptions::new(u64::MAX);
        assert!(cache_removals(&plan(&dirs, &opts)).is_empty());
        assert!(cache_removals(&plan_maintenance(&dirs, &opts)).is_empty());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn default_options_never_evict_fresh_entries_by_size() {
        let home = home("no-size-cap");
        let dirs = Dirs::at(&home);
        for n in 0..3 {
            store_entry(&home, &format!("{n:02x}{}", "4".repeat(38)), &[("f0", 1 << 20)], 100 * (n + 1));
        }
        let opts = GcOptions::new(u64::MAX);
        assert!(cache_removals(&plan(&dirs, &opts)).is_empty());
        assert!(cache_removals(&plan_maintenance(&dirs, &opts)).is_empty());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn an_explicit_cap_still_evicts_oldest_first() {
        let home = home("explicit-cap");
        let dirs = Dirs::at(&home);
        let oldest = store_entry(&home, &format!("aa{}", "1".repeat(38)), &[("f0", 65_536)], 3000);
        let middle = store_entry(&home, &format!("bb{}", "2".repeat(38)), &[("f0", 65_536)], 2000);
        let newest = store_entry(&home, &format!("cc{}", "3".repeat(38)), &[("f0", 65_536)], 1000);
        let cap = fsx::allocated_size(&middle) + fsx::allocated_size(&newest);
        let opts = GcOptions::new(cap);
        assert_eq!(cache_removals(&plan(&dirs, &opts)), vec![oldest.clone()]);
        assert_eq!(cache_removals(&plan_maintenance(&dirs, &opts)), vec![oldest]);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn age_limits_parse_without_panicking() {
        assert_eq!(config::days_to_duration(0.5), Some(Duration::from_secs(43_200)));
        assert_eq!(config::days_to_duration(0.0), Some(Duration::ZERO));
        assert_eq!(config::days_to_duration(-1.0), None);
        assert_eq!(config::days_to_duration(f64::NAN), None);
        assert!(config::days_to_duration(f64::INFINITY).is_some());
        assert!(config::days_to_duration(1e300).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn a_pending_store_keeps_its_expired_entry_until_released() {
        if crate::isolated_test(module_path!(), "a_pending_store_keeps_its_expired_entry_until_released") {
            return;
        }
        let root = home("expiry-protected");
        let dirs = Dirs::at(&root);
        let protected = store_entry(&root, &"aa".repeat(20), &[("artifact", 16384)], FIFTEEN_DAYS);
        let unrelated = store_entry(&root, &"bb".repeat(20), &[("artifact", 16384)], FIFTEEN_DAYS);
        let output = root.join("external-pending.rlib");
        fs::hard_link(protected.join("artifact"), &output).unwrap();
        let path = deferred_job(&dirs, Some(&output), Some(1), Duration::ZERO);
        let held = shared_job(&path);
        let opts = GcOptions::new(u64::MAX);
        assert_eq!(cache_removals(&plan(&dirs, &opts)), vec![unrelated.clone()]);
        assert_eq!(cache_removals(&plan_maintenance(&dirs, &opts)), vec![unrelated]);
        drop(held);
        assert_eq!(cache_removals(&plan(&dirs, &opts)).len(), 2);
        fs::remove_dir_all(root).unwrap();
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
        let tight = GcOptions::new(fsx::allocated_size(&old).max(fsx::allocated_size(&new)));
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

    fn linked_entry(home: &Path, key: &str, target: &Path, age_secs: u64) -> PathBuf {
        linked_entry_of(home, key, target, age_secs, &"ab".repeat(20))
    }

    fn linked_entry_of(home: &Path, key: &str, target: &Path, age_secs: u64, prekey: &str) -> PathBuf {
        let dir = home.join("store").join(key);
        fs::create_dir_all(&dir).unwrap();
        fs::hard_link(target, dir.join("f0")).unwrap();
        let len = fs::metadata(target).unwrap().len();
        let meta = serde_json::json!({
            "crate_name": "c", "version": "1", "compile_ms": 1, "created_unix": 1, "prekey": prekey, "stderr": "",
            "files": [{"name": "libx.rlib", "stored": "f0", "text": false, "bytes": len, "encoding": "linked",
                       "stored_bytes": len, "decoded_bytes": len, "content_hash": "ab".repeat(32), "mode": 0o100644}],
        });
        fs::write(dir.join("meta.json"), serde_json::to_vec(&meta).unwrap()).unwrap();
        File::options().write(true).open(dir.join("meta.json")).unwrap().set_modified(SystemTime::now() - Duration::from_secs(age_secs)).unwrap();
        dir
    }

    #[test]
    fn linked_entries_cost_nothing_while_shared_and_a_full_copy_once_orphaned() {
        let home = home("linked-sizes");
        let dirs = Dirs::at(&home);
        let target = home.join("target-file");
        fs::write(&target, vec![7u8; 65_536]).unwrap();
        linked_entry(&home, &format!("aa{}", "1".repeat(38)), &target, 100);
        let shared = scan_entries(&dirs);
        assert_eq!(shared[0].estimate, Some(ENTRY_OVERHEAD_BYTES), "a file shared with a target name frees nothing");
        assert!(!shared[0].link.orphaned);
        fs::remove_file(target).unwrap();
        let orphaned = scan_entries(&dirs);
        assert_eq!(orphaned[0].estimate, Some(ENTRY_OVERHEAD_BYTES + 65_536));
        assert!(orphaned[0].link.orphaned);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn superseded_linked_entries_are_dropped_once_no_target_name_shares_them() {
        let home = home("linked-superseded");
        let dirs = Dirs::at(&home);
        let mut targets = Vec::new();
        let mut entries = Vec::new();
        for (i, age) in [(1u8, 3000u64), (2, 2000), (3, 1000)] {
            let target = home.join(format!("target-{i}"));
            fs::write(&target, vec![i; 8192]).unwrap();
            entries.push(linked_entry(&home, &format!("{i:02x}{}", "1".repeat(38)), &target, age));
            targets.push(target);
        }
        assert!(superseded_incremental_entries(&dirs, 2).is_empty(), "entries shared with target names are kept");
        fs::remove_file(&targets[0]).unwrap();
        fs::remove_file(&targets[2]).unwrap();
        let dropped: Vec<PathBuf> = superseded_incremental_entries(&dirs, 2).into_iter().map(|s| s.0).collect();
        assert_eq!(dropped, vec![entries[0].clone()], "only older orphaned entries go, never the newest of the unit");
        assert!(cache_removals(&plan(&dirs, &GcOptions::new(u64::MAX))).contains(&entries[0]));
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn store_only_path_entries_no_unit_record_lists_are_swept_after_the_grace() {
        let home = home("linked-unreferenced");
        let dirs = Dirs::at(&home);
        let mut entries = Vec::new();
        for (i, age) in [(1u8, 3000u64), (2, 30)] {
            let target = home.join(format!("target-{i}"));
            fs::write(&target, vec![i; 8192]).unwrap();
            let entry = linked_entry_of(&home, &format!("{i:02x}{}", "1".repeat(38)), &target, age, &format!("{i:02x}").repeat(20));
            fs::remove_file(&target).unwrap();
            let meta_path = entry.join("meta.json");
            let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&meta_path).unwrap()).unwrap();
            value["source"] = serde_json::json!("path");
            fs::write(&meta_path, serde_json::to_vec(&value).unwrap()).unwrap();
            File::options().write(true).open(&meta_path).unwrap().set_modified(SystemTime::now() - Duration::from_secs(age)).unwrap();
            entries.push(entry);
        }
        let dropped: Vec<PathBuf> = superseded_incremental_entries(&dirs, 2).into_iter().map(|s| s.0).collect();
        assert_eq!(dropped, vec![entries[0].clone()], "only the entry past the grace goes");
        let unit = "cd".repeat(20);
        let record = cache::Cache::new(&dirs).units_dir().join(&unit);
        fs::create_dir_all(record.parent().unwrap()).unwrap();
        fs::write(&record, format!("{}\n", entries[0].file_name().unwrap().to_string_lossy())).unwrap();
        assert!(superseded_incremental_entries(&dirs, 2).is_empty(), "a listed entry is the unit's head and stays");
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn linked_entries_group_by_the_unit_record_and_a_stale_object_name_does_not_pin_them() {
        let home = home("linked-unit-records");
        let dirs = Dirs::at(&home);
        let unit = "cd".repeat(20);
        let mut entries = Vec::new();
        let mut targets = Vec::new();
        for (i, age) in [(1u8, 2000u64), (2, 1000)] {
            let target = home.join(format!("target-{i}"));
            fs::write(&target, vec![i; 8192]).unwrap();
            // A different prekey per state: only the unit record ties them together.
            entries.push(linked_entry_of(&home, &format!("{i:02x}{}", "1".repeat(38)), &target, age, &format!("{i:02x}").repeat(20)));
            targets.push(target);
        }
        let record = cache::Cache::new(&dirs).units_dir().join(&unit);
        fs::create_dir_all(record.parent().unwrap()).unwrap();
        fs::write(&record, format!("{}\n{}\n", entries[1].file_name().unwrap().to_string_lossy(), entries[0].file_name().unwrap().to_string_lossy())).unwrap();
        fs::remove_file(&targets[0]).unwrap();
        let dropped: Vec<PathBuf> = superseded_incremental_entries(&dirs, 2).into_iter().map(|s| s.0).collect();
        assert_eq!(dropped, vec![entries[0].clone()]);

        let meta_path = entries[0].join("meta.json");
        let mut meta: cache::EntryMeta = serde_json::from_slice(&fs::read(&meta_path).unwrap()).unwrap();
        meta.files[0].name = "x-0123456789abcdef.c1.t1.rcgu.o".into();
        let modified = fs::metadata(&meta_path).unwrap().modified().unwrap();
        fs::write(&meta_path, serde_json::to_vec(&meta).unwrap()).unwrap();
        File::options().write(true).open(&meta_path).unwrap().set_modified(modified).unwrap();
        fs::hard_link(entries[0].join("f0"), home.join("stale-object")).unwrap();
        let dropped: Vec<PathBuf> = superseded_incremental_entries(&dirs, 2).into_iter().map(|s| s.0).collect();
        assert_eq!(dropped, vec![entries[0].clone()], "a leftover object name in a target does not keep a superseded entry");
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

    #[test]
    fn idle_pack_records_without_present_entries_are_retired_and_live_ones_stay() {
        let root = home("pack-retire");
        let dirs = Dirs::at(&root);
        let store = cache::Cache::new(&dirs);
        let (live_unit, dead_unit, young_unit) = ("a1".repeat(20), "a2".repeat(20), "a3".repeat(20));
        let live_key = "11".repeat(20);
        store_entry(&root, &live_key, &[("f0", 64)], 0);
        let dead_key = "22".repeat(20);
        let old = fsx::unix_now() - 90 * 86_400;
        let pack = format!("{live_unit} {old} {live_key}\n{dead_unit} {old} {dead_key}\n{young_unit} {} {dead_key}\n", fsx::unix_now());
        fs::create_dir_all(store.units_dir()).unwrap();
        fs::write(store.unit_pack_path(), pack).unwrap();
        let plan = plan(&dirs, &GcOptions::new(u64::MAX));
        assert_eq!(plan.pack_retirements.iter().map(|r| r.0.clone()).collect::<Vec<_>>(), vec![dead_unit.clone()]);
        let (_, failures) = execute_removals(&dirs, &plan, &mut Silent);
        assert!(failures.is_empty());
        let left: Vec<String> = store.unit_records().into_iter().map(|r| r.0).collect();
        assert!(left.contains(&live_unit) && left.contains(&young_unit) && !left.contains(&dead_unit), "{left:?}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn flat_key_locks_age_out_but_the_metadata_lock_and_young_locks_stay() {
        let root = home("flat-keylocks");
        let dirs = Dirs::at(&root);
        fs::create_dir_all(dirs.keylocks()).unwrap();
        let old = SystemTime::now() - Duration::from_secs(3 * 86_400);
        let stale = dirs.keylocks().join("ab".repeat(20));
        let young = dirs.keylocks().join("cd".repeat(20));
        let records = dirs.keylocks().join("unit-records");
        for path in [&stale, &young, &records] { File::create(path).unwrap(); }
        for path in [&stale, &records] { File::options().write(true).open(path).unwrap().set_modified(old).unwrap(); }
        let mut plan = Plan::default();
        plan_keylocks(&dirs, &GcOptions::new(u64::MAX), &mut plan);
        let planned: Vec<&PathBuf> = plan.removals.iter().map(|r| &r.path).collect();
        assert_eq!(planned, vec![&stale]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn retiring_a_key_lock_unlinks_only_an_idle_file_and_executes_through_the_plan() {
        let root = home("retire-keylock");
        let dirs = Dirs::at(&root);
        fs::create_dir_all(dirs.keylocks()).unwrap();
        let idle = dirs.keylocks().join("ab".repeat(20));
        let busy = dirs.keylocks().join("cd".repeat(20));
        for path in [&idle, &busy] { File::create(path).unwrap(); }
        let held = File::open(&busy).unwrap();
        sys::try_lock(&held, LockKind::Exclusive).unwrap();
        assert!(!retire_key_lock(&busy).unwrap() && busy.exists());
        assert!(!retire_key_lock(&dirs.keylocks().join("ef".repeat(20))).unwrap());
        let opts = GcOptions { keylock_max_age: Duration::ZERO, ..GcOptions::new(u64::MAX) };
        let mut planned = Plan::default();
        plan_keylocks(&dirs, &opts, &mut planned);
        assert_eq!(planned.removals.iter().map(|r| r.path.clone()).collect::<Vec<_>>(), vec![idle.clone()]);
        let (_, failures) = execute_removals(&dirs, &planned, &mut Silent);
        assert!(failures.is_empty() && !idle.exists() && busy.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn execute_moves_legacy_layout_flat_and_prunes_emptied_shards_everywhere() {
        let root = home("legacy-migration");
        let dirs = Dirs::at(&root);
        let key = "ab".repeat(20);
        let legacy_entry = dirs.store().join("ab").join(&key);
        fs::create_dir_all(&legacy_entry).unwrap();
        fs::write(legacy_entry.join("meta.json"), b"{}").unwrap();
        let legacy_lock_dir = dirs.keylocks().join("cd");
        fs::create_dir_all(&legacy_lock_dir).unwrap();
        let (_, failures) = execute(&dirs, &Plan::default());
        assert!(failures.is_empty());
        assert!(dirs.store().join(&key).join("meta.json").is_file());
        assert!(!dirs.store().join("ab").exists() && !legacy_lock_dir.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_hashmemo_files_age_out_with_their_shards_and_the_log_is_compacted() {
        let root = home("hashmemo-legacy");
        let dirs = Dirs::at(&root);
        let memo = dirs.hashmemo();
        let old_shard = memo.join("ab");
        let young_shard = memo.join("cd");
        fs::create_dir_all(&old_shard).unwrap();
        fs::create_dir_all(&young_shard).unwrap();
        let (old, young) = (old_shard.join("0".repeat(32)), young_shard.join("1".repeat(32)));
        fs::write(&old, b"frust-hashmemo-v2\n").unwrap();
        fs::write(&young, b"frust-hashmemo-v2\n").unwrap();
        File::options().write(true).open(&old).unwrap().set_modified(SystemTime::now() - Duration::from_secs(90 * 86_400)).unwrap();
        let log = memo.join(cache::hashing::LOG_NAME);
        fs::write(&log, b"nothing in here is a valid frame").unwrap();
        let outside = root.join("outside");
        fs::write(&outside, b"keep").unwrap();

        let mut plan = Plan::default();
        plan_hashmemo(&dirs, &GcOptions::new(u64::MAX), &mut plan);
        assert!(plan.removals.iter().any(|r| r.path == old) && plan.removals.iter().all(|r| r.path != young && r.path != log));
        let (freed, failures) = execute(&dirs, &plan);
        assert!(failures.is_empty() && freed > 0);
        assert!(!old.exists() && !old_shard.exists(), "an aged legacy record and its emptied shard are removed");
        assert!(young.exists() && young_shard.exists());
        assert!(!log.exists(), "a log with no live frame is compacted away");
        assert!(outside.exists());
        fs::remove_dir_all(root).unwrap();
    }
}
