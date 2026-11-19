//! Cleanup of build data that a successful build has just made stale.
//!
//! Every deletion here is safe for correctness because Cargo rebuilds a missing output.
//! The cost is a rebuild, so nothing the latest build used is ever removed.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::json_text::{decode_json_string_body, escape_json_string};

pub const GIB: u64 = 1 << 30;
/// Budget used when the volume size is unknown and the ceiling of the volume-relative default.
pub const DEFAULT_CAP_BYTES: u64 = 20 * GIB;
/// Marker file that makes a central slot eligible for whole-slot eviction.
pub const SLOT_MARKER: &str = ".frust-slot.json";
/// A collection pass over every slot is skipped when the previous one is newer than this.
pub const CENTRAL_INTERVAL: Duration = Duration::from_secs(600);

const LEDGER: &str = ".frust-live.json";
const CARGO_LOCK: &str = ".cargo-lock";
const STORE_LOCK: &str = ".frust-gc.lock";
const CENTRAL_STAMP: &str = ".frust-gc-stamp";
const SCHEMA_VERSION: u64 = 1;
const WORKING_SESSION_AGE: Duration = Duration::from_secs(3600);
/// A marker that is current and younger than this is not rewritten.
const MARKER_REFRESH: Duration = Duration::from_secs(60);
const MAX_STATE_BYTES: u64 = 16 << 20;
const MAX_MARKER_BYTES: u64 = 64 << 10;
const MAX_TREE_DEPTH: usize = 64;
const HASH_LEN: usize = 16;
const SLOT_PREFIX_LEN: usize = 2;
const SLOT_NAME_LEN: usize = 62;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GcConfig {
    /// A unit stays while it was live in the current or any of the previous `keep - 1` builds.
    pub keep_generations: u32,
    /// Cap for one profile directory; `None` leaves the size to the central budget.
    pub max_bytes: Option<u64>,
}

impl Default for GcConfig {
    fn default() -> Self {
        GcConfig { keep_generations: 3, max_bytes: None }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GcReport {
    pub removed_files: u64,
    pub removed_dirs: u64,
    pub freed_bytes: u64,
    /// Allocated bytes left in the scanned tree, hard links counted once.
    pub kept_bytes: u64,
    pub skipped_busy: bool,
}

impl GcReport {
    pub fn merge(&mut self, other: &GcReport) {
        self.removed_files += other.removed_files;
        self.removed_dirs += other.removed_dirs;
        self.freed_bytes += other.freed_bytes;
        self.kept_bytes += other.kept_bytes;
        self.skipped_busy |= other.skipped_busy;
    }
}

/// Everything one successful build used: artifact paths and build-script output directories.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LiveSet {
    artifacts: Vec<PathBuf>,
    out_dirs: Vec<PathBuf>,
}

impl LiveSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_artifact(&mut self, path: impl Into<PathBuf>) {
        self.artifacts.push(path.into());
    }

    pub fn add_out_dir(&mut self, path: impl Into<PathBuf>) {
        self.out_dirs.push(path.into());
    }

    pub fn is_empty(&self) -> bool {
        self.artifacts.is_empty() && self.out_dirs.is_empty()
    }

    pub fn artifacts(&self) -> &[PathBuf] {
        &self.artifacts
    }

    pub fn out_dirs(&self) -> &[PathBuf] {
        &self.out_dirs
    }

    /// Distinct profile directories the build wrote to, in order of first appearance.
    pub fn profile_dirs(&self) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = Vec::new();
        for path in self.artifacts.iter().chain(&self.out_dirs) {
            if dirs.iter().any(|dir| path.starts_with(dir)) {
                continue;
            }
            if let Some(dir) = profile_dir_of(path) {
                dirs.push(dir);
            }
        }
        dirs
    }
}

/// The nearest ancestor shaped like a Cargo profile directory.
fn profile_dir_of(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    path.ancestors()
        .skip(1)
        .find(|dir| is_real_dir(&dir.join("deps")) && is_real_dir(&dir.join(".fingerprint")))
        .map(Path::to_path_buf)
}

/// Prune one profile directory after a successful build. See the module docs for the safety rules.
pub fn collect_profile_dir(profile_dir: &Path, live: &LiveSet, cfg: &GcConfig, now: SystemTime) -> io::Result<GcReport> {
    let mut report = GcReport::default();
    if !is_real_dir(profile_dir) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "profile directory is missing or a symlink"));
    }
    // Cargo's own lock: while it is held a build is running and nothing may be deleted.
    let Some(_lock) = lock_exclusive(&profile_dir.join(CARGO_LOCK))? else {
        report.skipped_busy = true;
        return Ok(report);
    };
    let keep = u64::from(cfg.keep_generations.max(1));
    let ledger_path = profile_dir.join(LEDGER);
    let (mut ledger, trusted) = match Ledger::load(&ledger_path) {
        Loaded::Valid(ledger) => (ledger, true),
        Loaded::Missing => (Ledger::default(), true),
        Loaded::Corrupt => (Ledger::default(), false),
    };
    let current = ledger.generation.saturating_add(1);
    let scan = Scan { units: scan_units(profile_dir), incremental: scan_incremental(profile_dir) };
    let live_now = live_hashes(profile_dir, live, &scan.units);
    ledger.advance(current, unix_secs(now), keep, live_now, &scan.units);
    write_atomic(&ledger_path, ledger.render().as_bytes())?;

    let mut tally = Tally::default();
    // A fresh ledger knows nothing about age, so a corrupt one must not drive any deletion.
    if trusted {
        for (hash, paths) in &scan.units {
            let last = ledger.live.get(hash).copied().unwrap_or(current);
            if last.saturating_add(keep) <= current {
                for path in paths {
                    remove_tree(path, &mut tally, 0);
                }
            }
        }
        remove_working_sessions(&scan.incremental, now, &mut tally);
        let cutoff = current
            .checked_sub(keep)
            .filter(|generation| *generation >= 1)
            .and_then(|generation| ledger.stamps.get(&generation))
            .map(|secs| UNIX_EPOCH + Duration::from_secs(*secs));
        prune_old_siblings(&scan.incremental, cutoff, &mut tally);
    }
    let mut total = measure_dir(profile_dir);
    if let Some(cap) = cfg.max_bytes {
        if trusted && total > cap {
            total = enforce_cap(cap, total, current, &ledger, &scan, &mut tally);
        }
    }
    report.removed_files = tally.files;
    report.removed_dirs = tally.dirs;
    report.freed_bytes = tally.freed;
    report.kept_bytes = total;
    Ok(report)
}

/// Remove whole slots under `<target_root>/targets`, least recently used first, until the
/// total is within `max_bytes`. Only slots with a valid marker are candidates.
pub fn collect_central(target_root: &Path, max_bytes: u64, protect: &Path, now: SystemTime) -> io::Result<GcReport> {
    let mut report = GcReport::default();
    let targets = target_root.join("targets");
    if !is_real_dir(&targets) {
        return Ok(report);
    }
    let Some(_store) = lock_exclusive(&target_root.join(STORE_LOCK))? else {
        report.skipped_busy = true;
        return Ok(report);
    };
    let protect_real = fs::canonicalize(protect).ok();
    let mut seen = HashSet::new();
    let slots: Vec<Slot> = list_slots(&targets)
        .into_iter()
        .map(|path| {
            let size = measure_into(&path, &mut seen);
            let marker = read_marker(&path);
            Slot { path, size, marker }
        })
        .collect();
    let mut total: u64 = slots.iter().map(|slot| slot.size).sum();
    let mut tally = Tally::default();
    if total > max_bytes {
        let mut order: Vec<(bool, Reverse<Duration>, &Slot)> = slots
            .iter()
            .filter(|slot| !is_protected(&slot.path, protect, protect_real.as_deref()))
            .filter_map(|slot| {
                let marker = slot.marker.as_ref()?;
                let age = now.duration_since(marker.used).unwrap_or(Duration::ZERO);
                Some((!marker.workspace_is_gone(), Reverse(age), slot))
            })
            .collect();
        order.sort_by(|a, b| (a.0, &a.1, &a.2.path).cmp(&(b.0, &b.1, &b.2.path)));
        for (_, _, slot) in order {
            if total <= max_bytes {
                break;
            }
            // The guards keep a build from starting in the slot while it is being removed.
            let Some(_guards) = lock_slot(&slot.path) else { continue };
            let before = tally.freed;
            remove_tree(&slot.path, &mut tally, 0);
            if let Some(parent) = slot.path.parent() {
                let _ = fs::remove_dir(parent);
            }
            total = total.saturating_sub(tally.freed - before);
        }
    }
    if let Ok(stamp) = File::create(target_root.join(CENTRAL_STAMP)) {
        let _ = stamp.set_modified(now);
    }
    report.removed_files = tally.files;
    report.removed_dirs = tally.dirs;
    report.freed_bytes = tally.freed;
    report.kept_bytes = total;
    Ok(report)
}

/// Whether enough time has passed since the last central pass to justify walking every slot.
pub fn central_due(target_root: &Path, now: SystemTime) -> bool {
    let stamped = fs::metadata(target_root.join(CENTRAL_STAMP)).and_then(|meta| meta.modified());
    match stamped {
        Ok(when) => now.duration_since(when).map_or(true, |age| age >= CENTRAL_INTERVAL),
        Err(_) => true,
    }
}

/// Record that `slot_root` serves `workspace` and was just used. The marker's mtime is the
/// last-used time, so a current marker younger than a minute is left alone.
pub fn touch_slot_marker(slot_root: &Path, workspace: &Path) -> io::Result<()> {
    let text = workspace.to_str().unwrap_or("");
    let body = format!("{{\"version\":{SCHEMA_VERSION},\"workspace\":\"{}\"}}\n", escape_json_string(text));
    let marker = slot_root.join(SLOT_MARKER);
    let fresh = fs::symlink_metadata(&marker)
        .ok()
        .filter(|meta| meta.is_file())
        .and_then(|meta| meta.modified().ok())
        .and_then(|when| SystemTime::now().duration_since(when).ok())
        .is_some_and(|age| age < MARKER_REFRESH);
    if fresh && fs::read(&marker).is_ok_and(|bytes| bytes == body.as_bytes()) {
        return Ok(());
    }
    write_atomic(&marker, body.as_bytes())
}

/// The `targets/<2hex>/<62hex>` slot containing `path`, decided by name alone.
pub fn slot_root(targets_dir: &Path, path: &Path) -> Option<PathBuf> {
    let mut parts = path.strip_prefix(targets_dir).ok()?.iter();
    let prefix = parts.next()?.to_str()?;
    let name = parts.next()?.to_str()?;
    (is_hex(prefix.as_bytes(), SLOT_PREFIX_LEN) && is_hex(name.as_bytes(), SLOT_NAME_LEN))
        .then(|| targets_dir.join(prefix).join(name))
}

/// Default central budget: a tenth of the volume, never more than 20 GiB.
pub fn default_max_bytes(volume_total: u64) -> u64 {
    DEFAULT_CAP_BYTES.min(volume_total / 10)
}

/// The configured budget, else the volume-relative default, else 20 GiB when the volume is unknown.
pub fn resolve_max_bytes(target_root: &Path, configured: Option<u64>) -> u64 {
    configured.unwrap_or_else(|| {
        volume_total_bytes(target_root)
            .filter(|total| *total > 0)
            .map_or(DEFAULT_CAP_BYTES, default_max_bytes)
    })
}

/// Total size of the volume holding `path` (or its nearest existing ancestor).
pub fn volume_total_bytes(path: &Path) -> Option<u64> {
    let existing = path.ancestors().find(|ancestor| ancestor.exists())?;
    sys::volume_total(existing)
}

#[cfg(any(all(target_os = "linux", target_pointer_width = "64"), target_os = "macos"))]
mod sys {
    use std::ffi::{c_char, c_int, c_ulong, CString};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    #[cfg(target_os = "linux")]
    type Count = u64;
    #[cfg(target_os = "macos")]
    type Count = u32;

    /// `struct statvfs` for 64-bit Linux and for macOS, whose block counts are 32-bit.
    #[repr(C)]
    #[allow(dead_code)] // fields that are never read only complete the C layout
    struct StatVfs {
        bsize: c_ulong,
        frsize: c_ulong,
        blocks: Count,
        bfree: Count,
        bavail: Count,
        files: Count,
        ffree: Count,
        favail: Count,
        fsid: c_ulong,
        flag: c_ulong,
        namemax: c_ulong,
        // Spare room so a libc with a longer tail cannot write past the allocation.
        spare: [u64; 16],
    }

    extern "C" {
        fn statvfs(path: *const c_char, buf: *mut StatVfs) -> c_int;
    }

    pub fn volume_total(path: &Path) -> Option<u64> {
        let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
        let mut buf = StatVfs {
            bsize: 0,
            frsize: 0,
            blocks: 0,
            bfree: 0,
            bavail: 0,
            files: 0,
            ffree: 0,
            favail: 0,
            fsid: 0,
            flag: 0,
            namemax: 0,
            spare: [0; 16],
        };
        // SAFETY: the path is NUL-terminated and `buf` is a writable, larger-than-needed statvfs.
        let status = unsafe { statvfs(c_path.as_ptr(), &mut buf) };
        if status != 0 {
            return None;
        }
        let unit = if buf.frsize != 0 { buf.frsize } else { buf.bsize };
        // The statvfs fields are 32-bit on macOS and 64-bit on Linux.
        #[allow(clippy::useless_conversion)]
        let total = u64::from(buf.blocks).checked_mul(u64::from(unit));
        total
    }
}

#[cfg(not(any(all(target_os = "linux", target_pointer_width = "64"), target_os = "macos")))]
mod sys {
    use std::path::Path;

    pub fn volume_total(_path: &Path) -> Option<u64> {
        None
    }
}

/// Directory listings gathered once so removal and cap eviction agree on what exists.
struct Scan {
    units: BTreeMap<String, Vec<PathBuf>>,
    incremental: Vec<IncDir>,
}

struct IncDir {
    path: PathBuf,
    krate: String,
    modified: SystemTime,
}

fn is_real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir())
}

fn unix_secs(when: SystemTime) -> u64 {
    when.duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_secs()).unwrap_or(0)
}

fn is_hex(bytes: &[u8], len: usize) -> bool {
    bytes.len() == len && bytes.iter().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// Split `<name>-<16 hex>` followed by nothing or a `.extension`, at the first such hash.
fn stem_parts(name: &str) -> Option<(&str, &str)> {
    let bytes = name.as_bytes();
    let last_dash = (bytes.len() + 1).saturating_sub(HASH_LEN + 1);
    (1..last_dash).find_map(|dash| {
        let end = dash + 1 + HASH_LEN;
        let found = bytes[dash] == b'-'
            && is_hex(&bytes[dash + 1..end], HASH_LEN)
            && bytes.get(end).is_none_or(|next| *next == b'.');
        found.then(|| (&name[..dash], &name[dash + 1..end]))
    })
}

/// Hash of a directory named exactly `<name>-<16 hex>`.
fn dir_hash(name: &str) -> Option<&str> {
    let (stem, hash) = stem_parts(name)?;
    (stem.len() + 1 + HASH_LEN == name.len()).then_some(hash)
}

/// Normalised crate name used to match an uplifted file when its inode cannot be matched.
fn crate_key(name: &str) -> String {
    let base = name.split('.').next().unwrap_or(name);
    base.strip_prefix("lib").unwrap_or(base).replace('-', "_")
}

/// Every deps/ entry, .fingerprint dir and build dir of the profile, grouped by unit hash.
fn scan_units(profile: &Path) -> BTreeMap<String, Vec<PathBuf>> {
    let mut units: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for (sub, dirs_only) in [("deps", false), (".fingerprint", true), ("build", true)] {
        let dir = profile.join(sub);
        if !is_real_dir(&dir) {
            continue;
        }
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_symlink() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let hash = if dirs_only {
                dir_hash(name).filter(|_| kind.is_dir())
            } else {
                stem_parts(name).map(|(_, hash)| hash)
            };
            if let Some(hash) = hash {
                units.entry(hash.to_string()).or_default().push(entry.path());
            }
        }
    }
    units
}

/// An artifact outside deps/ and build/, found through its inode or crate name.
struct Uplift {
    dev: u64,
    ino: u64,
    key: String,
}

/// Unit hashes the build used. Uplifted binaries are hard links of deps/ files, so they are
/// resolved by inode, and by crate name when the uplift is a copy.
fn live_hashes(profile: &Path, live: &LiveSet, units: &BTreeMap<String, Vec<PathBuf>>) -> HashSet<String> {
    let mut hashes = HashSet::new();
    let mut uplifted: Vec<Uplift> = Vec::new();
    for path in &live.artifacts {
        let Ok(rel) = path.strip_prefix(profile) else { continue };
        let mut parts = rel.iter().map(OsStr::to_str);
        let hash = match parts.next().flatten() {
            Some("deps") => parts.next().flatten().and_then(stem_parts).map(|(_, hash)| hash),
            Some("build") => parts.next().flatten().and_then(dir_hash),
            _ => None,
        };
        match hash {
            Some(hash) => {
                hashes.insert(hash.to_string());
            }
            None => note_uplift(path, &mut hashes, &mut uplifted),
        }
    }
    for path in &live.out_dirs {
        let Ok(rel) = path.strip_prefix(profile) else { continue };
        let mut parts = rel.iter().map(OsStr::to_str);
        if parts.next().flatten() == Some("build") {
            if let Some(hash) = parts.next().flatten().and_then(dir_hash) {
                hashes.insert(hash.to_string());
            }
        }
    }
    if !uplifted.is_empty() {
        resolve_uplifted(&uplifted, units, &mut hashes);
    }
    hashes
}

fn note_uplift(path: &Path, hashes: &mut HashSet<String>, uplifted: &mut Vec<Uplift>) {
    let Some(file) = path.file_name().and_then(OsStr::to_str) else { return };
    if let Some((_, hash)) = stem_parts(file) {
        hashes.insert(hash.to_string());
        return;
    }
    if let Ok(meta) = fs::symlink_metadata(path) {
        if meta.is_file() {
            uplifted.push(Uplift { dev: meta.dev(), ino: meta.ino(), key: crate_key(file) });
        }
    }
}

fn resolve_uplifted(uplifted: &[Uplift], units: &BTreeMap<String, Vec<PathBuf>>, hashes: &mut HashSet<String>) {
    let wanted: HashSet<(u64, u64)> = uplifted.iter().map(|uplift| (uplift.dev, uplift.ino)).collect();
    let mut matched: HashSet<(u64, u64)> = HashSet::new();
    for (hash, paths) in units {
        for path in paths {
            let Ok(meta) = fs::symlink_metadata(path) else { continue };
            let id = (meta.dev(), meta.ino());
            if meta.is_file() && wanted.contains(&id) {
                hashes.insert(hash.clone());
                matched.insert(id);
            }
        }
    }
    let by_name: HashSet<&str> = uplifted
        .iter()
        .filter(|uplift| !matched.contains(&(uplift.dev, uplift.ino)))
        .map(|uplift| uplift.key.as_str())
        .collect();
    if by_name.is_empty() {
        return;
    }
    for (hash, paths) in units {
        for path in paths {
            let stem = path.file_name().and_then(OsStr::to_str).and_then(stem_parts).map(|(stem, _)| stem);
            if stem.is_some_and(|stem| by_name.contains(crate_key(stem).as_str())) {
                hashes.insert(hash.clone());
            }
        }
    }
}

/// `<crate>-<id>` dir names under incremental/; the id is rustc's base-36 session hash.
fn crate_of(name: &str) -> Option<&str> {
    let (krate, id) = name.rsplit_once('-')?;
    (!krate.is_empty() && !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_alphanumeric())).then_some(krate)
}

fn scan_incremental(profile: &Path) -> Vec<IncDir> {
    let dir = profile.join("incremental");
    if !is_real_dir(&dir) {
        return Vec::new();
    }
    let Ok(entries) = fs::read_dir(&dir) else { return Vec::new() };
    let mut dirs = Vec::new();
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let name = entry.file_name();
        let Some(krate) = name.to_str().and_then(crate_of) else { continue };
        let Ok(modified) = entry.metadata().and_then(|meta| meta.modified()) else { continue };
        dirs.push(IncDir { path: entry.path(), krate: krate.to_string(), modified });
    }
    dirs
}

/// Sessions still marked `-working` after an hour belong to a build that was killed.
fn remove_working_sessions(dirs: &[IncDir], now: SystemTime, tally: &mut Tally) {
    let Some(limit) = now.checked_sub(WORKING_SESSION_AGE) else { return };
    for dir in dirs {
        let Ok(entries) = fs::read_dir(&dir.path) else { continue };
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let working = name.starts_with("s-") && name.ends_with("-working");
            let old = entry.metadata().and_then(|meta| meta.modified()).is_ok_and(|when| when < limit);
            if working && old {
                remove_tree(&entry.path(), tally, 0);
            }
        }
    }
}

/// An idle dir is dropped only while a more recently modified sibling of the same crate exists.
fn prune_old_siblings(dirs: &[IncDir], cutoff: Option<SystemTime>, tally: &mut Tally) {
    let Some(cutoff) = cutoff else { return };
    let mut newest: HashMap<&str, SystemTime> = HashMap::new();
    for dir in dirs {
        let entry = newest.entry(dir.krate.as_str()).or_insert(dir.modified);
        *entry = (*entry).max(dir.modified);
    }
    for dir in dirs {
        let superseded = newest.get(dir.krate.as_str()).is_some_and(|latest| *latest > dir.modified);
        if dir.modified < cutoff && superseded {
            remove_tree(&dir.path, tally, 0);
        }
    }
}

/// Evict least recently live units, then incremental dirs idle since the previous build.
/// Units live in the current generation are never candidates.
fn enforce_cap(cap: u64, mut total: u64, current: u64, ledger: &Ledger, scan: &Scan, tally: &mut Tally) -> u64 {
    let mut order: Vec<(u64, &String)> = scan
        .units
        .keys()
        .filter_map(|hash| ledger.live.get(hash).filter(|last| **last < current).map(|last| (*last, hash)))
        .collect();
    order.sort();
    for (_, hash) in order {
        if total <= cap {
            return total;
        }
        let before = tally.freed;
        for path in &scan.units[hash] {
            remove_tree(path, tally, 0);
        }
        total = total.saturating_sub(tally.freed - before);
    }
    let previous = current
        .checked_sub(1)
        .filter(|generation| *generation >= 1)
        .and_then(|generation| ledger.stamps.get(&generation))
        .map(|secs| UNIX_EPOCH + Duration::from_secs(*secs));
    let Some(previous) = previous else { return total };
    let mut idle: Vec<&IncDir> = scan.incremental.iter().filter(|dir| dir.modified <= previous).collect();
    idle.sort_by_key(|dir| dir.modified);
    for dir in idle {
        if total <= cap {
            break;
        }
        let before = tally.freed;
        remove_tree(&dir.path, tally, 0);
        total = total.saturating_sub(tally.freed - before);
    }
    total
}

struct Slot {
    path: PathBuf,
    size: u64,
    marker: Option<SlotMarker>,
}

struct SlotMarker {
    /// `None` when the recorded path was not valid UTF-8, so liveness is unknown.
    workspace: Option<PathBuf>,
    used: SystemTime,
}

impl SlotMarker {
    fn workspace_is_gone(&self) -> bool {
        self.workspace
            .as_ref()
            .is_some_and(|path| fs::symlink_metadata(path).is_err_and(|err| err.kind() == io::ErrorKind::NotFound))
    }
}

fn child_dirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else { return Vec::new() };
    entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .collect()
}

fn name_is_hex(path: &Path, len: usize) -> bool {
    path.file_name().and_then(OsStr::to_str).is_some_and(|name| is_hex(name.as_bytes(), len))
}

fn list_slots(targets: &Path) -> Vec<PathBuf> {
    let mut slots = Vec::new();
    for prefix in child_dirs(targets).into_iter().filter(|dir| name_is_hex(dir, SLOT_PREFIX_LEN)) {
        slots.extend(child_dirs(&prefix).into_iter().filter(|dir| name_is_hex(dir, SLOT_NAME_LEN)));
    }
    slots
}

fn is_protected(slot: &Path, protect: &Path, protect_real: Option<&Path>) -> bool {
    slot == protect || protect_real.is_some_and(|real| fs::canonicalize(slot).is_ok_and(|path| path == real))
}

/// A marker that is missing, unreadable, not a regular file or of an unknown version yields `None`.
fn read_marker(slot: &Path) -> Option<SlotMarker> {
    let path = slot.join(SLOT_MARKER);
    let meta = fs::symlink_metadata(&path).ok()?;
    if !meta.is_file() || meta.len() > MAX_MARKER_BYTES {
        return None;
    }
    let bytes = fs::read(&path).ok()?;
    let Val::Obj(fields) = parse_doc(&bytes)? else { return None };
    let mut version = None;
    let mut workspace = None;
    for (key, value) in fields {
        match (key.as_str(), value) {
            ("version", Val::Num(number)) => version = Some(number),
            ("workspace", Val::Str(text)) => workspace = Some(text),
            ("version" | "workspace", _) => return None,
            _ => {}
        }
    }
    if version? != SCHEMA_VERSION {
        return None;
    }
    let workspace = workspace?;
    Some(SlotMarker {
        workspace: Some(workspace).filter(|text| !text.is_empty()).map(PathBuf::from),
        used: meta.modified().ok()?,
    })
}

/// Lock every `.cargo-lock` a build in the slot could hold. `None` means a build is running or
/// a lock file cannot be probed.
fn lock_slot(slot: &Path) -> Option<Vec<File>> {
    let mut candidates = vec![slot.join(CARGO_LOCK)];
    for first in child_dirs(slot) {
        candidates.push(first.join(CARGO_LOCK));
        candidates.extend(child_dirs(&first).into_iter().map(|second| second.join(CARGO_LOCK)));
    }
    let mut held = Vec::new();
    for path in candidates {
        match File::open(&path) {
            Ok(file) => match file.try_lock() {
                Ok(()) => held.push(file),
                Err(_) => return None,
            },
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(_) => return None,
        }
    }
    Some(held)
}

/// `Ok(None)` when another process holds the lock.
fn lock_exclusive(path: &Path) -> io::Result<Option<File>> {
    if fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return Err(io::Error::other("lock path is a symlink"));
    }
    let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(err)) => Err(err),
    }
}

/// Atomic replace: unique temp file, fsync, rename.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_nanos()).unwrap_or(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let name = path.file_name().and_then(OsStr::to_str).unwrap_or("state");
    let temp = path.with_file_name(format!("{name}.{}.{id}.{nanos}.tmp", std::process::id()));
    let result = (|| {
        let mut file = OpenOptions::new().write(true).create_new(true).open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// What was deleted. Hard-linked files free their blocks only when the last link goes.
#[derive(Default)]
struct Tally {
    files: u64,
    dirs: u64,
    freed: u64,
    links_left: HashMap<(u64, u64), u64>,
}

impl Tally {
    fn file_gone(&mut self, meta: &fs::Metadata) {
        self.files += 1;
        if meta.nlink() <= 1 {
            self.freed += allocated(meta);
            return;
        }
        let left = self.links_left.entry((meta.dev(), meta.ino())).or_insert(meta.nlink());
        *left = left.saturating_sub(1);
        if *left == 0 {
            self.freed += allocated(meta);
        }
    }
}

fn allocated(meta: &fs::Metadata) -> u64 {
    meta.blocks().saturating_mul(512)
}

/// Counts a hard-linked file once per (device, inode).
fn charge(meta: &fs::Metadata, seen: &mut HashSet<(u64, u64)>) -> u64 {
    if !meta.is_dir() && meta.nlink() > 1 && !seen.insert((meta.dev(), meta.ino())) {
        return 0;
    }
    allocated(meta)
}

fn measure_dir(root: &Path) -> u64 {
    measure_into(root, &mut HashSet::new())
}

/// Allocated bytes under `root` without following symlinks.
fn measure_into(root: &Path, seen: &mut HashSet<(u64, u64)>) -> u64 {
    let mut total = fs::symlink_metadata(root).map_or(0, |meta| charge(&meta, seen));
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            total += charge(&meta, seen);
            if meta.is_dir() {
                stack.push(entry.path());
            }
        }
    }
    total
}

/// Remove a file or directory tree, unlinking symlinks instead of following them.
/// Returns whether the path is gone; failures leave the entry and are not errors.
fn remove_tree(path: &Path, tally: &mut Tally, depth: usize) -> bool {
    let Ok(meta) = fs::symlink_metadata(path) else { return false };
    if !meta.is_dir() {
        if fs::remove_file(path).is_ok() {
            tally.file_gone(&meta);
            return true;
        }
        return false;
    }
    if depth >= MAX_TREE_DEPTH {
        return fs::remove_dir_all(path).is_ok();
    }
    let Ok(entries) = fs::read_dir(path) else { return false };
    let mut clean = true;
    for entry in entries.flatten() {
        clean &= remove_tree(&entry.path(), tally, depth + 1);
    }
    if clean && fs::remove_dir(path).is_ok() {
        tally.dirs += 1;
        tally.freed += allocated(&meta);
        return true;
    }
    false
}

enum Loaded {
    Valid(Ledger),
    Missing,
    Corrupt,
}

/// Which build last used each unit hash, with the finish time of the newest builds.
#[derive(Debug, Default, PartialEq, Eq)]
struct Ledger {
    generation: u64,
    /// Generation to Unix seconds, for the newest `keep + 1` generations.
    stamps: BTreeMap<u64, u64>,
    /// Unit hash to the last generation that used it.
    live: BTreeMap<String, u64>,
}

impl Ledger {
    fn load(path: &Path) -> Loaded {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Loaded::Missing,
            Err(_) => return Loaded::Corrupt,
        };
        let mut bytes = Vec::new();
        if file.take(MAX_STATE_BYTES + 1).read_to_end(&mut bytes).is_err() || bytes.len() as u64 > MAX_STATE_BYTES {
            return Loaded::Corrupt;
        }
        Ledger::parse(&bytes).map_or(Loaded::Corrupt, Loaded::Valid)
    }

    fn parse(bytes: &[u8]) -> Option<Ledger> {
        let Val::Obj(fields) = parse_doc(bytes)? else { return None };
        let (mut version, mut generation, mut stamps, mut live) = (None, None, None, None);
        for (key, value) in fields {
            let duplicate = match (key.as_str(), value) {
                ("version", Val::Num(number)) => version.replace(number).is_some(),
                ("generation", Val::Num(number)) => generation.replace(number).is_some(),
                ("stamps", Val::Obj(pairs)) => stamps.replace(pairs).is_some(),
                ("live", Val::Obj(pairs)) => live.replace(pairs).is_some(),
                _ => return None,
            };
            if duplicate {
                return None;
            }
        }
        if version? != SCHEMA_VERSION {
            return None;
        }
        let mut ledger = Ledger { generation: generation?, ..Ledger::default() };
        for (key, value) in stamps? {
            let Val::Num(secs) = value else { return None };
            ledger.stamps.insert(key.parse().ok()?, secs);
        }
        for (key, value) in live? {
            let Val::Num(last) = value else { return None };
            if !is_hex(key.as_bytes(), HASH_LEN) {
                return None;
            }
            ledger.live.insert(key, last);
        }
        Some(ledger)
    }

    fn render(&self) -> String {
        let stamps: Vec<String> = self.stamps.iter().map(|(generation, secs)| format!("\"{generation}\":{secs}")).collect();
        let live: Vec<String> = self.live.iter().map(|(hash, last)| format!("\"{hash}\":{last}")).collect();
        format!(
            "{{\"version\":{SCHEMA_VERSION},\"generation\":{},\"stamps\":{{{}}},\"live\":{{{}}}}}\n",
            self.generation,
            stamps.join(","),
            live.join(",")
        )
    }

    /// Start generation `current`. A hash first seen on disk counts as live now, so data that
    /// predates the ledger (or a lost ledger) gets a full grace period before it can age out.
    fn advance(&mut self, current: u64, secs: u64, keep: u64, live_now: HashSet<String>, units: &BTreeMap<String, Vec<PathBuf>>) {
        self.generation = current;
        self.stamps.insert(current, secs);
        self.stamps.retain(|generation, _| generation.saturating_add(keep) >= current);
        for hash in live_now {
            self.live.insert(hash, current);
        }
        for hash in units.keys() {
            self.live.entry(hash.clone()).or_insert(current);
        }
        self.live.retain(|hash, last| *last == current || units.contains_key(hash));
    }
}

/// The only JSON shapes fRust state files use: objects of unsigned integers and strings.
#[derive(Debug)]
enum Val {
    Num(u64),
    Str(String),
    Obj(Vec<(String, Val)>),
}

const MAX_STATE_DEPTH: usize = 4;

fn parse_doc(bytes: &[u8]) -> Option<Val> {
    let mut reader = Reader { bytes, at: 0 };
    let value = reader.value(0)?;
    reader.skip();
    (reader.at == bytes.len()).then_some(value)
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn skip(&mut self) {
        while matches!(self.bytes.get(self.at), Some(b' ' | b'\n' | b'\r' | b'\t')) {
            self.at += 1;
        }
    }

    fn value(&mut self, depth: usize) -> Option<Val> {
        self.skip();
        match *self.bytes.get(self.at)? {
            b'{' if depth < MAX_STATE_DEPTH => self.object(depth),
            b'"' => self.string().map(Val::Str),
            b'0'..=b'9' => self.number(),
            _ => None,
        }
    }

    fn object(&mut self, depth: usize) -> Option<Val> {
        self.at += 1;
        let mut pairs = Vec::new();
        self.skip();
        if self.bytes.get(self.at) == Some(&b'}') {
            self.at += 1;
            return Some(Val::Obj(pairs));
        }
        loop {
            self.skip();
            let key = self.string()?;
            self.skip();
            if self.bytes.get(self.at) != Some(&b':') {
                return None;
            }
            self.at += 1;
            pairs.push((key, self.value(depth + 1)?));
            self.skip();
            match self.bytes.get(self.at)? {
                b',' => self.at += 1,
                b'}' => {
                    self.at += 1;
                    return Some(Val::Obj(pairs));
                }
                _ => return None,
            }
        }
    }

    fn string(&mut self) -> Option<String> {
        if self.bytes.get(self.at) != Some(&b'"') {
            return None;
        }
        let (text, used) = decode_json_string_body(&self.bytes[self.at + 1..]).ok()?;
        self.at += 1 + used;
        Some(text)
    }

    fn number(&mut self) -> Option<Val> {
        let start = self.at;
        while matches!(self.bytes.get(self.at), Some(b'0'..=b'9')) {
            self.at += 1;
        }
        std::str::from_utf8(&self.bytes[start..self.at]).ok()?.parse().ok().map(Val::Num)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    const MIB: usize = 1 << 20;

    fn scratch() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("frust-gc-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    /// A fixed clock: step `n` is `n` thousand seconds after a constant base.
    fn at(step: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_700_000_000 + step * 1_000)
    }

    fn set_mtime(path: &Path, when: SystemTime) {
        File::open(path).unwrap().set_modified(when).unwrap();
    }

    fn hash(n: u64) -> String {
        format!("{n:016x}")
    }

    fn payload(path: &Path, len: usize) {
        fs::write(path, vec![7u8; len]).unwrap();
    }

    fn make_profile(root: &Path) -> PathBuf {
        let profile = root.join("debug");
        for dir in ["deps", ".fingerprint", "build", "incremental"] {
            fs::create_dir_all(profile.join(dir)).unwrap();
        }
        profile
    }

    fn rlib(profile: &Path, name: &str, n: u64) -> PathBuf {
        profile.join("deps").join(format!("lib{name}-{}.rlib", hash(n)))
    }

    fn add_unit(profile: &Path, name: &str, n: u64) {
        let h = hash(n);
        payload(&rlib(profile, name, n), 4096);
        payload(&profile.join("deps").join(format!("lib{name}-{h}.rmeta")), 4096);
        payload(&profile.join("deps").join(format!("{name}-{h}.d")), 4096);
        let fingerprint = profile.join(".fingerprint").join(format!("{name}-{h}"));
        fs::create_dir_all(&fingerprint).unwrap();
        payload(&fingerprint.join("lib-x"), 100);
        let build = profile.join("build").join(format!("{name}-{h}"));
        fs::create_dir_all(build.join("out")).unwrap();
        payload(&build.join("output"), 100);
    }

    fn add_bin(profile: &Path, name: &str, n: u64) -> PathBuf {
        let h = hash(n);
        let bin = profile.join("deps").join(format!("{name}-{h}"));
        payload(&bin, 4096);
        let fingerprint = profile.join(".fingerprint").join(format!("{name}-{h}"));
        fs::create_dir_all(&fingerprint).unwrap();
        payload(&fingerprint.join("bin-x"), 100);
        bin
    }

    fn unit_present(profile: &Path, name: &str, n: u64) -> bool {
        rlib(profile, name, n).exists()
    }

    fn unit_gone(profile: &Path, name: &str, n: u64) -> bool {
        let h = hash(n);
        !rlib(profile, name, n).exists()
            && !profile.join("deps").join(format!("lib{name}-{h}.rmeta")).exists()
            && !profile.join("deps").join(format!("{name}-{h}.d")).exists()
            && !profile.join(".fingerprint").join(format!("{name}-{h}")).exists()
            && !profile.join("build").join(format!("{name}-{h}")).exists()
    }

    fn live(profile: &Path, units: &[(&str, u64)]) -> LiveSet {
        let mut set = LiveSet::new();
        for (name, n) in units {
            set.add_artifact(rlib(profile, name, *n));
        }
        set
    }

    fn run(profile: &Path, live: &LiveSet, cfg: &GcConfig, step: u64) -> GcReport {
        collect_profile_dir(profile, live, cfg, at(step)).unwrap()
    }

    fn ledger_generation(profile: &Path) -> u64 {
        match Ledger::load(&profile.join(LEDGER)) {
            Loaded::Valid(ledger) => ledger.generation,
            _ => panic!("ledger is not valid"),
        }
    }

    #[test]
    fn stale_units_go_after_keep_generations_and_live_ones_stay() {
        let root = scratch();
        let profile = make_profile(&root);
        add_unit(&profile, "keep", 1);
        add_unit(&profile, "gone", 2);
        let cfg = GcConfig::default();
        run(&profile, &live(&profile, &[("keep", 1), ("gone", 2)]), &cfg, 1);
        for step in 2..=3 {
            let report = run(&profile, &live(&profile, &[("keep", 1)]), &cfg, step);
            assert_eq!(report.removed_files, 0, "step {step}");
            assert!(unit_present(&profile, "gone", 2), "step {step}");
        }
        let report = run(&profile, &live(&profile, &[("keep", 1)]), &cfg, 4);
        assert!(unit_gone(&profile, "gone", 2));
        assert!(unit_present(&profile, "keep", 1));
        assert!(profile.join(".fingerprint").join(format!("keep-{}", hash(1))).is_dir());
        assert!(profile.join("build").join(format!("keep-{}", hash(1))).is_dir());
        assert!(report.removed_files >= 5, "{report:?}");
        assert!(report.removed_dirs >= 3, "{report:?}");
        assert!(report.freed_bytes > 0, "{report:?}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_unit_live_in_the_last_generation_survives_the_byte_cap() {
        let root = scratch();
        let profile = make_profile(&root);
        for (name, n) in [("a", 1), ("b", 2), ("c", 3)] {
            add_unit(&profile, name, n);
            payload(&rlib(&profile, name, n), MIB);
        }
        let cfg = GcConfig { keep_generations: 3, max_bytes: Some(1) };
        let everything = live(&profile, &[("a", 1), ("b", 2), ("c", 3)]);
        let report = run(&profile, &everything, &cfg, 1);
        assert!(report.kept_bytes > 1);
        assert!(["a", "b", "c"].iter().zip(1..).all(|(name, n)| unit_present(&profile, name, n)));
        let report = run(&profile, &live(&profile, &[("a", 1)]), &cfg, 2);
        assert!(unit_present(&profile, "a", 1));
        assert!(unit_gone(&profile, "b", 2));
        assert!(unit_gone(&profile, "c", 3));
        assert!(report.kept_bytes > 1, "the cap cannot evict what the build just used");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_cap_evicts_least_recently_live_units_first() {
        let root = scratch();
        let profile = make_profile(&root);
        for (name, n) in [("u1", 1), ("u2", 2), ("u3", 3), ("v", 4)] {
            add_unit(&profile, name, n);
            payload(&rlib(&profile, name, n), MIB);
        }
        let cfg = GcConfig::default();
        run(&profile, &live(&profile, &[("u1", 1)]), &cfg, 1);
        run(&profile, &live(&profile, &[("u2", 2)]), &cfg, 2);
        let total = measure_dir(&profile);
        let capped = GcConfig { keep_generations: 3, max_bytes: Some(total - (MIB / 2) as u64) };
        let report = run(&profile, &live(&profile, &[("u3", 3), ("v", 4)]), &capped, 3);
        assert!(unit_gone(&profile, "u1", 1));
        for (name, n) in [("u2", 2), ("u3", 3), ("v", 4)] {
            assert!(unit_present(&profile, name, n), "{name}");
        }
        assert!(report.freed_bytes >= MIB as u64);
        assert!(report.kept_bytes <= capped.max_bytes.unwrap());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn old_incremental_siblings_need_a_newer_sibling_and_a_passed_cutoff() {
        let root = scratch();
        let profile = make_profile(&root);
        add_unit(&profile, "a", 1);
        let cfg = GcConfig::default();
        for step in 1..=3 {
            run(&profile, &live(&profile, &[("a", 1)]), &cfg, step);
        }
        let incremental = profile.join("incremental");
        let dirs = [
            ("foo-aaa", 0),
            ("foo-bbb", 10),
            ("bar-ccc", 0),
            ("baz-ddd", 2),
            ("baz-eee", 3),
            ("qux-fff", 0),
            ("qux-ggg", 1),
            ("weird", 0),
        ];
        for (name, step) in dirs {
            let dir = incremental.join(name);
            fs::create_dir_all(&dir).unwrap();
            payload(&dir.join("dep-graph.bin"), 64);
            set_mtime(&dir, at(step));
        }
        // The cutoff at generation 4 is the finish time of generation 1.
        run(&profile, &live(&profile, &[("a", 1)]), &cfg, 4);
        let kept = |name: &str| incremental.join(name).exists();
        assert!(!kept("foo-aaa"), "old and superseded");
        assert!(kept("foo-bbb"), "newest sibling");
        assert!(kept("bar-ccc"), "no newer sibling");
        assert!(kept("baz-ddd"), "modified after the cutoff");
        assert!(kept("baz-eee"));
        assert!(!kept("qux-fff"), "old and superseded");
        assert!(kept("qux-ggg"), "modified exactly at the cutoff and newest");
        assert!(kept("weird"), "not a crate-id name");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn working_sessions_are_removed_after_an_hour() {
        let root = scratch();
        let profile = make_profile(&root);
        add_unit(&profile, "a", 1);
        let crate_dir = profile.join("incremental").join("foo-abc");
        let now = at(1);
        let sessions = [
            ("s-old-working", Duration::from_secs(7200)),
            ("s-new-working", Duration::from_secs(600)),
            ("s-final", Duration::from_secs(5 * 3600)),
        ];
        for (name, age) in sessions {
            let session = crate_dir.join(name);
            fs::create_dir_all(&session).unwrap();
            payload(&session.join("query-cache.bin"), 64);
            set_mtime(&session, now - age);
        }
        set_mtime(&crate_dir, now);
        collect_profile_dir(&profile, &live(&profile, &[("a", 1)]), &GcConfig::default(), now).unwrap();
        assert!(!crate_dir.join("s-old-working").exists());
        assert!(crate_dir.join("s-new-working").exists());
        assert!(crate_dir.join("s-final").exists(), "finalized sessions belong to rustc");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn hard_linked_artifacts_are_counted_once() {
        let root = scratch();
        let profile = make_profile(&root);
        let bin = add_bin(&profile, "app", 1);
        payload(&bin, MIB);
        fs::hard_link(&bin, profile.join("app")).unwrap();
        let one = fs::metadata(&bin).unwrap().blocks() * 512;
        let mut set = LiveSet::new();
        set.add_artifact(profile.join("app"));
        let report = run(&profile, &set, &GcConfig::default(), 1);
        assert!(report.kept_bytes >= one, "{report:?}");
        assert!(report.kept_bytes < 2 * one, "{report:?} against one copy of {one}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn freed_bytes_ignore_files_that_keep_another_link() {
        let root = scratch();
        let profile = make_profile(&root);
        add_unit(&profile, "a", 1);
        let bin = add_bin(&profile, "old", 2);
        payload(&bin, MIB);
        fs::hard_link(&bin, profile.join("old")).unwrap();
        let cfg = GcConfig { keep_generations: 1, max_bytes: None };
        run(&profile, &live(&profile, &[("a", 1)]), &cfg, 1);
        let report = run(&profile, &live(&profile, &[("a", 1)]), &cfg, 2);
        assert!(!bin.exists());
        assert!(profile.join("old").exists());
        assert!(report.freed_bytes < MIB as u64, "{report:?}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_held_cargo_lock_skips_the_collection() {
        let root = scratch();
        let profile = make_profile(&root);
        add_unit(&profile, "a", 1);
        add_unit(&profile, "b", 2);
        let cfg = GcConfig { keep_generations: 1, max_bytes: None };
        run(&profile, &live(&profile, &[("a", 1), ("b", 2)]), &cfg, 1);
        let held = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(profile.join(CARGO_LOCK)).unwrap();
        held.lock().unwrap();
        let report = run(&profile, &live(&profile, &[("a", 1)]), &cfg, 2);
        assert!(report.skipped_busy);
        assert_eq!(report.removed_files, 0);
        assert!(unit_present(&profile, "b", 2));
        assert_eq!(ledger_generation(&profile), 1, "a skipped pass must not advance the ledger");
        drop(held);
        let report = run(&profile, &live(&profile, &[("a", 1)]), &cfg, 2);
        assert!(!report.skipped_busy);
        assert!(unit_gone(&profile, "b", 2));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn names_that_do_not_match_are_untouched() {
        let root = scratch();
        let profile = make_profile(&root);
        add_unit(&profile, "a", 1);
        let h = hash(77);
        let deps = profile.join("deps");
        for name in [
            "README".to_string(),
            "libfoo.rlib".to_string(),
            "foo-123.d".to_string(),
            "libbar-ABCDEF0123456789.rlib".to_string(),
            "libbar-0123456789abcdeg.rlib".to_string(),
            "libbar-0123456789abcdef0.rlib".to_string(),
            format!("-{h}.d"),
        ] {
            payload(&deps.join(name), 16);
        }
        let fingerprint = profile.join(".fingerprint");
        fs::create_dir_all(fingerprint.join("notes")).unwrap();
        fs::create_dir_all(fingerprint.join(format!("foo-{h}.bak"))).unwrap();
        payload(&fingerprint.join(format!("file-{h}")), 16);
        let build = profile.join("build");
        fs::create_dir_all(build.join("foo")).unwrap();
        fs::create_dir_all(build.join(format!("x-{h}.bak"))).unwrap();
        let incremental = profile.join("incremental");
        fs::create_dir_all(incremental.join("weird")).unwrap();
        payload(&incremental.join("stray-abc"), 16);
        payload(&profile.join("app"), 16);
        fs::create_dir_all(profile.join("examples")).unwrap();
        payload(&profile.join("examples").join(format!("ex-{h}")), 16);
        let cfg = GcConfig::default();
        for step in 1..=6 {
            run(&profile, &live(&profile, &[("a", 1)]), &cfg, step);
        }
        let expected = [
            deps.join("README"),
            deps.join("libfoo.rlib"),
            deps.join("foo-123.d"),
            deps.join("libbar-ABCDEF0123456789.rlib"),
            deps.join("libbar-0123456789abcdeg.rlib"),
            deps.join("libbar-0123456789abcdef0.rlib"),
            deps.join(format!("-{h}.d")),
            fingerprint.join("notes"),
            fingerprint.join(format!("foo-{h}.bak")),
            fingerprint.join(format!("file-{h}")),
            build.join("foo"),
            build.join(format!("x-{h}.bak")),
            incremental.join("weird"),
            incremental.join("stray-abc"),
            profile.join("app"),
            profile.join("examples").join(format!("ex-{h}")),
        ];
        for path in expected {
            assert!(path.exists(), "{} was removed", path.display());
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn symlinks_are_neither_followed_nor_removed() {
        let root = scratch();
        let profile = make_profile(&root);
        let victim = root.join("victim");
        fs::create_dir_all(&victim).unwrap();
        payload(&victim.join("keep.txt"), 32);
        let h = hash(9);
        symlink(victim.join("keep.txt"), profile.join("deps").join(format!("libev-{h}.rlib"))).unwrap();
        symlink(&victim, profile.join(".fingerprint").join(format!("ev-{h}"))).unwrap();
        symlink(&victim, profile.join("build").join(format!("ev-{h}"))).unwrap();
        symlink(&victim, profile.join("incremental").join("ev-abc")).unwrap();
        add_unit(&profile, "a", 1);
        add_unit(&profile, "old", 5);
        let inside = profile.join(".fingerprint").join(format!("old-{}", hash(5)));
        symlink(&victim, inside.join("escape-dir")).unwrap();
        symlink(victim.join("keep.txt"), inside.join("escape-file")).unwrap();
        let cfg = GcConfig::default();
        run(&profile, &live(&profile, &[("a", 1), ("old", 5)]), &cfg, 1);
        for step in 2..=4 {
            run(&profile, &live(&profile, &[("a", 1)]), &cfg, step);
        }
        assert!(unit_gone(&profile, "old", 5));
        assert_eq!(fs::read(victim.join("keep.txt")).unwrap(), vec![7u8; 32]);
        for link in [
            profile.join("deps").join(format!("libev-{h}.rlib")),
            profile.join(".fingerprint").join(format!("ev-{h}")),
            profile.join("build").join(format!("ev-{h}")),
            profile.join("incremental").join("ev-abc"),
        ] {
            assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "{}", link.display());
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_corrupt_ledger_restarts_without_deleting_anything() {
        let root = scratch();
        let profile = make_profile(&root);
        add_unit(&profile, "a", 1);
        add_unit(&profile, "b", 2);
        let cfg = GcConfig { keep_generations: 1, max_bytes: Some(1) };
        run(&profile, &live(&profile, &[("a", 1), ("b", 2)]), &cfg, 1);
        let session = profile.join("incremental").join("foo-abc").join("s-1-working");
        fs::create_dir_all(&session).unwrap();
        set_mtime(&session, at(0) - Duration::from_secs(86_400));
        fs::write(profile.join(LEDGER), b"{not json").unwrap();
        let report = run(&profile, &live(&profile, &[("a", 1)]), &cfg, 2);
        assert_eq!(report.removed_files + report.removed_dirs, 0, "{report:?}");
        assert!(unit_present(&profile, "b", 2));
        assert!(session.exists());
        assert_eq!(ledger_generation(&profile), 1);
        run(&profile, &live(&profile, &[("a", 1)]), &cfg, 3);
        assert!(unit_gone(&profile, "b", 2));
        assert!(!session.exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_ledger_of_an_unknown_version_is_replaced_not_trusted() {
        let root = scratch();
        let profile = make_profile(&root);
        add_unit(&profile, "a", 1);
        add_unit(&profile, "b", 2);
        fs::write(profile.join(LEDGER), br#"{"version":2,"generation":50,"stamps":{},"live":{}}"#).unwrap();
        let cfg = GcConfig { keep_generations: 1, max_bytes: None };
        let report = run(&profile, &live(&profile, &[("a", 1)]), &cfg, 1);
        assert_eq!(report.removed_files, 0);
        assert!(unit_present(&profile, "b", 2));
        assert_eq!(ledger_generation(&profile), 1);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_ledger_round_trips_and_rejects_malformed_documents() {
        let mut ledger = Ledger { generation: 7, ..Ledger::default() };
        ledger.stamps.insert(6, 100);
        ledger.stamps.insert(7, 200);
        ledger.live.insert(hash(1), 7);
        ledger.live.insert(hash(2), 5);
        assert_eq!(Ledger::parse(ledger.render().as_bytes()), Some(ledger));
        let short = r#"{"version":1,"generation":1,"stamps":{},"live":{"abc":1}}"#;
        let upper = format!(r#"{{"version":1,"generation":1,"stamps":{{}},"live":{{"{}":1}}}}"#, "A".repeat(16));
        let bad = [
            "",
            "{}",
            "[",
            r#"{"version":1}"#,
            r#"{"version":1,"generation":1,"stamps":{},"live":{},"extra":1}"#,
            r#"{"version":1,"version":1,"generation":1,"stamps":{},"live":{}}"#,
            r#"{"version":1,"generation":-1,"stamps":{},"live":{}}"#,
            r#"{"version":1,"generation":1,"stamps":{"x":1},"live":{}}"#,
            r#"{"version":1,"generation":1,"stamps":{},"live":{}} trailing"#,
            short,
            upper.as_str(),
        ];
        for text in bad {
            assert!(Ledger::parse(text.as_bytes()).is_none(), "{text}");
        }
        let fine = r#"{"version":1,"generation":1,"stamps":{},"live":{}}"#;
        assert!(Ledger::parse(fine.as_bytes()).is_some());
    }

    #[test]
    fn uplifted_binaries_keep_their_deps_artifacts_alive() {
        let root = scratch();
        let profile = make_profile(&root);
        let linked = add_bin(&profile, "app", 1);
        fs::hard_link(&linked, profile.join("app")).unwrap();
        let copied = add_bin(&profile, "tool", 3);
        fs::copy(&copied, profile.join("tool")).unwrap();
        let stale = add_bin(&profile, "stalebin", 4);
        fs::copy(&stale, profile.join("stalebin")).unwrap();
        add_unit(&profile, "old", 2);
        let cfg = GcConfig { keep_generations: 1, max_bytes: None };
        let mut first = LiveSet::new();
        for uplift in ["app", "tool", "stalebin"] {
            first.add_artifact(profile.join(uplift));
        }
        first.add_artifact(rlib(&profile, "old", 2));
        run(&profile, &first, &cfg, 1);
        let mut second = LiveSet::new();
        second.add_artifact(profile.join("app"));
        second.add_artifact(profile.join("tool"));
        run(&profile, &second, &cfg, 2);
        assert!(linked.exists(), "matched through the hard link");
        assert!(profile.join(".fingerprint").join(format!("app-{}", hash(1))).exists());
        assert!(copied.exists(), "matched through the crate name of the copy");
        assert!(!stale.exists(), "an uplift that is no longer live must not keep it");
        assert!(unit_gone(&profile, "old", 2));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn build_script_output_dirs_keep_their_units_alive() {
        let root = scratch();
        let profile = make_profile(&root);
        add_unit(&profile, "pkg", 5);
        add_unit(&profile, "other", 6);
        let cfg = GcConfig { keep_generations: 1, max_bytes: None };
        let out_dir = profile.join("build").join(format!("pkg-{}", hash(5))).join("out");
        let mut set = LiveSet::new();
        set.add_out_dir(&out_dir);
        run(&profile, &set, &cfg, 1);
        run(&profile, &set, &cfg, 2);
        assert!(out_dir.is_dir());
        assert!(unit_present(&profile, "pkg", 5));
        assert!(unit_gone(&profile, "other", 6));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn profile_dirs_are_found_by_layout_in_order_of_first_appearance() {
        let root = scratch();
        let host = make_profile(&root);
        let cross = make_profile(&root.join("x86_64-unknown-linux-gnu"));
        fs::create_dir_all(root.join("plain").join("deps")).unwrap();
        let mut set = LiveSet::new();
        assert!(set.is_empty());
        set.add_artifact(host.join("app"));
        set.add_artifact(cross.join("deps").join("libx.rlib"));
        set.add_artifact(host.join("deps").join("liby.rlib"));
        set.add_artifact(root.join("plain").join("deps").join("libz.rlib"));
        set.add_artifact("relative/deps/libw.rlib");
        set.add_out_dir(host.join("build").join(format!("p-{}", hash(1))).join("out"));
        assert_eq!(set.profile_dirs(), vec![host, cross]);
        assert_eq!(set.artifacts().len(), 5);
        assert_eq!(set.out_dirs().len(), 1);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn stem_parsing_accepts_only_sixteen_lowercase_hex_digits() {
        let h = hash(0xabc);
        assert_eq!(stem_parts(&format!("libfoo-{h}.rlib")), Some(("libfoo", h.as_str())));
        assert_eq!(stem_parts(&format!("foo-{h}")), Some(("foo", h.as_str())));
        assert_eq!(stem_parts(&format!("foo-bar-{h}.foo.abc-cgu.0.rcgu.o")), Some(("foo-bar", h.as_str())));
        assert_eq!(stem_parts(&format!("a-{h}-b-{}.d", hash(2))), Some((format!("a-{h}-b").as_str(), hash(2).as_str())));
        assert_eq!(stem_parts(&format!("-{h}")), None);
        assert_eq!(stem_parts(&format!("foo-{h}0")), None);
        assert_eq!(stem_parts("foo-0123456789ABCDEF"), None);
        assert_eq!(dir_hash(&format!("foo-{h}")), Some(h.as_str()));
        assert_eq!(dir_hash(&format!("foo-{h}.d")), None);
        assert_eq!(crate_of("foo-1xj5ja6qzg7j2"), Some("foo"));
        assert_eq!(crate_of("foo"), None);
        assert_eq!(crate_of("foo-"), None);
        assert_eq!(crate_of("foo-a_b"), None);
        assert_eq!(crate_key("libfoo-bar.rlib"), "foo_bar");
    }

    fn make_slot(root: &Path, n: u64, marker: Option<(&PathBuf, SystemTime)>, len: usize) -> PathBuf {
        let slot = root.join("targets").join(format!("{:02x}", n % 256)).join(format!("{n:062x}"));
        fs::create_dir_all(slot.join("debug").join("deps")).unwrap();
        payload(&slot.join("debug").join("deps").join("blob"), len);
        if let Some((workspace, used)) = marker {
            touch_slot_marker(&slot, workspace).unwrap();
            set_mtime(&slot.join(SLOT_MARKER), used);
        }
        slot
    }

    #[test]
    fn central_evicts_lru_slots_but_never_protected_or_unmarked_ones() {
        let root = scratch();
        let workspace = root.clone();
        let old = make_slot(&root, 1, Some((&workspace, at(1))), MIB);
        let mid = make_slot(&root, 2, Some((&workspace, at(2))), MIB);
        let new = make_slot(&root, 3, Some((&workspace, at(3))), MIB);
        let protected = make_slot(&root, 4, Some((&workspace, at(0))), MIB);
        let unmarked = make_slot(&root, 5, None, MIB);
        let report = collect_central(&root, (MIB * 7 / 2) as u64, &protected, at(10)).unwrap();
        assert!(!old.exists() && !mid.exists());
        assert!(new.exists() && protected.exists() && unmarked.exists());
        assert!(!root.join("targets").join("01").exists(), "the emptied prefix dir is removed");
        assert!(report.freed_bytes >= 2 * MIB as u64, "{report:?}");
        assert!(report.kept_bytes <= (MIB * 7 / 2) as u64, "{report:?}");
        let report = collect_central(&root, 0, &protected, at(11)).unwrap();
        assert!(!new.exists());
        assert!(protected.exists() && unmarked.exists());
        assert!(report.kept_bytes > 0);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn central_removes_slots_of_deleted_workspaces_first() {
        let root = scratch();
        let alive = root.join("workspace");
        fs::create_dir_all(&alive).unwrap();
        let lru = make_slot(&root, 1, Some((&alive, at(1))), MIB);
        let dead = make_slot(&root, 2, Some((&root.join("deleted"), at(5))), MIB);
        let budget = (MIB * 3 / 2) as u64;
        collect_central(&root, budget, Path::new("/nonexistent"), at(10)).unwrap();
        assert!(!dead.exists(), "dead workspace goes before an older live one");
        assert!(lru.exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn central_never_touches_slots_with_unusable_markers() {
        let root = scratch();
        let valid = make_slot(&root, 9, Some((&root, at(1))), MIB);
        let garbage = make_slot(&root, 1, None, MIB);
        fs::write(garbage.join(SLOT_MARKER), "nonsense").unwrap();
        let future = make_slot(&root, 2, None, MIB);
        fs::write(future.join(SLOT_MARKER), r#"{"version":2,"workspace":"/x"}"#).unwrap();
        let no_workspace = make_slot(&root, 3, None, MIB);
        fs::write(no_workspace.join(SLOT_MARKER), r#"{"version":1}"#).unwrap();
        let linked = make_slot(&root, 4, None, MIB);
        let real = root.join("elsewhere.json");
        fs::write(&real, r#"{"version":1,"workspace":"/gone"}"#).unwrap();
        symlink(&real, linked.join(SLOT_MARKER)).unwrap();
        let directory = make_slot(&root, 5, None, MIB);
        fs::create_dir(directory.join(SLOT_MARKER)).unwrap();
        let not_a_slot = root.join("targets").join("zz").join("short");
        fs::create_dir_all(&not_a_slot).unwrap();
        payload(&not_a_slot.join("blob"), MIB);
        collect_central(&root, 0, &valid, at(10)).unwrap();
        for slot in [&valid, &garbage, &future, &no_workspace, &linked, &directory, &not_a_slot] {
            assert!(slot.exists(), "{}", slot.display());
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn central_skips_when_busy_and_skips_slots_a_build_holds() {
        let root = scratch();
        let first = make_slot(&root, 1, Some((&root, at(1))), MIB);
        let second = make_slot(&root, 2, Some((&root, at(2))), MIB);
        let nested = make_slot(&root, 3, Some((&root, at(3))), MIB);
        let free = make_slot(&root, 4, Some((&root, at(4))), MIB);
        let store = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(root.join(STORE_LOCK)).unwrap();
        store.lock().unwrap();
        let report = collect_central(&root, 0, Path::new("/nonexistent"), at(10)).unwrap();
        assert!(report.skipped_busy);
        assert!(first.exists() && second.exists() && nested.exists() && free.exists());
        drop(store);

        let build = File::create(first.join("debug").join(CARGO_LOCK)).unwrap();
        build.lock().unwrap();
        fs::create_dir_all(nested.join("x86_64").join("debug")).unwrap();
        let cross = File::create(nested.join("x86_64").join("debug").join(CARGO_LOCK)).unwrap();
        cross.lock().unwrap();
        let report = collect_central(&root, 0, Path::new("/nonexistent"), at(11)).unwrap();
        assert!(!report.skipped_busy);
        assert!(first.exists() && nested.exists(), "slots with a running build stay");
        assert!(!second.exists() && !free.exists());
        drop((build, cross));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn slot_markers_are_written_atomically_and_refreshed() {
        let root = scratch();
        let slot = make_slot(&root, 1, None, 16);
        let workspace = Path::new("/work/a b\"q");
        touch_slot_marker(&slot, workspace).unwrap();
        let marker = read_marker(&slot).unwrap();
        assert_eq!(marker.workspace.as_deref(), Some(workspace));
        let first = fs::metadata(slot.join(SLOT_MARKER)).unwrap().modified().unwrap();
        touch_slot_marker(&slot, workspace).unwrap();
        assert_eq!(fs::metadata(slot.join(SLOT_MARKER)).unwrap().modified().unwrap(), first, "a young marker is kept");
        set_mtime(&slot.join(SLOT_MARKER), at(0));
        touch_slot_marker(&slot, workspace).unwrap();
        assert!(read_marker(&slot).unwrap().used > at(0), "an old marker is refreshed");
        touch_slot_marker(&slot, Path::new("/other")).unwrap();
        assert_eq!(read_marker(&slot).unwrap().workspace.as_deref(), Some(Path::new("/other")));
        let leftovers: Vec<_> = fs::read_dir(&slot)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        assert!(touch_slot_marker(&root.join("missing"), workspace).is_err());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn central_passes_are_throttled_by_a_stamp() {
        let root = scratch();
        fs::create_dir_all(root.join("targets")).unwrap();
        assert!(central_due(&root, at(10)));
        collect_central(&root, 0, Path::new("/nonexistent"), at(10)).unwrap();
        assert!(!central_due(&root, at(10)));
        assert!(!central_due(&root, at(10) + Duration::from_secs(599)));
        assert!(central_due(&root, at(10) + CENTRAL_INTERVAL));
        assert!(central_due(&root, at(0)), "a stamp from the future cannot suppress cleanup");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn slot_roots_are_recognised_by_name() {
        let targets = Path::new("/cache/targets");
        let slot = format!("{}/{}", "ab", "c".repeat(62));
        let inside = targets.join(&slot).join("debug").join("deps");
        assert_eq!(slot_root(targets, &inside), Some(targets.join(&slot)));
        assert_eq!(slot_root(targets, &targets.join(&slot)), Some(targets.join(&slot)));
        assert_eq!(slot_root(targets, &targets.join("ab")), None);
        assert_eq!(slot_root(targets, &targets.join("ab").join("short")), None);
        assert_eq!(slot_root(targets, &targets.join("AB").join("c".repeat(62))), None);
        assert_eq!(slot_root(targets, Path::new("/elsewhere/ab").join("c".repeat(62)).as_path()), None);
    }

    #[test]
    fn the_default_budget_is_a_tenth_of_the_volume_up_to_20_gib() {
        assert_eq!(default_max_bytes(100 * GIB), 10 * GIB);
        assert_eq!(default_max_bytes(200 * GIB), 20 * GIB);
        assert_eq!(default_max_bytes(1 << 40), 20 * GIB);
        assert_eq!(default_max_bytes(9), 0);
        assert_eq!(default_max_bytes(0), 0);
        assert_eq!(default_max_bytes(u64::MAX), 20 * GIB);
        assert_eq!(resolve_max_bytes(Path::new("/"), Some(7 * GIB)), 7 * GIB);
        assert!(resolve_max_bytes(&std::env::temp_dir(), None) <= DEFAULT_CAP_BYTES);
    }

    #[cfg(any(all(target_os = "linux", target_pointer_width = "64"), target_os = "macos"))]
    #[test]
    fn the_volume_size_is_read_from_the_nearest_existing_ancestor() {
        let dir = std::env::temp_dir();
        let total = volume_total_bytes(&dir).expect("statvfs on the temp dir");
        assert!(total > 0);
        assert_eq!(volume_total_bytes(&dir.join("frust-gc-does-not-exist").join("child")), Some(total));
        assert_eq!(resolve_max_bytes(&dir, None), default_max_bytes(total));
    }
}
