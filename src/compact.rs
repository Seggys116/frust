//! A profile directory is mutated only while holding cargo's own locks, taken without blocking, so one in use is skipped.

mod sessions;

use std::cell::{OnceCell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::fsx;
use crate::paths::Dirs;
use crate::stats;
use crate::sys;

/// The most compression or deduplication could save below this is a few 4 KiB blocks, not worth a rewrite.
pub const MIN_FILE_BYTES: u64 = 16 * 1024;
/// Compression reads the whole file into memory, so larger files are skipped.
const MAX_FILE_BYTES: u64 = 1024 * 1024 * 1024;
const CARGO_LOCKS: [&str; 3] = [".cargo-lock", ".cargo-build-lock", ".cargo-artifact-lock"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Category {
    Incremental,
    Rlib,
    Rmeta,
    Objects,
    Dylib,
    DepsExecutables,
    BuildScripts,
    Fingerprint,
    DepInfo,
    Examples,
    Dsym,
    Uplifted,
    Other,
}

impl Category {
    pub const ALL: [Category; 13] = [
        Category::Incremental,
        Category::Rlib,
        Category::Rmeta,
        Category::Objects,
        Category::Dylib,
        Category::DepsExecutables,
        Category::BuildScripts,
        Category::Fingerprint,
        Category::DepInfo,
        Category::Examples,
        Category::Dsym,
        Category::Uplifted,
        Category::Other,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Category::Incremental => "incremental caches",
            Category::Rlib => "deps .rlib",
            Category::Rmeta => "deps .rmeta",
            Category::Objects => "deps .o (debug info)",
            Category::Dylib => "proc-macro dylibs",
            Category::DepsExecutables => "deps executables",
            Category::BuildScripts => "build scripts + out",
            Category::Fingerprint => ".fingerprint",
            Category::DepInfo => "dep-info .d",
            Category::Examples => "examples",
            Category::Dsym => "dSYM bundles",
            Category::Uplifted => "uplifted top-level",
            Category::Other => "other",
        }
    }

    pub fn of(rel: &Path) -> Category {
        let parts: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
        let Some(first) = parts.first() else { return Category::Other };
        if parts.iter().any(|p| p.ends_with(".dSYM")) {
            return Category::Dsym;
        }
        let name = parts.last().map(String::as_str).unwrap_or("");
        match first.as_str() {
            "incremental" => Category::Incremental,
            ".fingerprint" => Category::Fingerprint,
            "build" => Category::BuildScripts,
            "examples" => Category::Examples,
            "deps" if parts.len() == 2 => match Path::new(name).extension().and_then(|e| e.to_str()) {
                Some("rlib") => Category::Rlib,
                Some("rmeta") => Category::Rmeta,
                Some("d") => Category::DepInfo,
                Some("o") => Category::Objects,
                Some("dylib" | "so" | "dll") => Category::Dylib,
                None | Some("exe") => Category::DepsExecutables,
                Some(_) => Category::Other,
            },
            _ if parts.len() == 1 && name.ends_with(".d") => Category::DepInfo,
            _ if parts.len() == 1 && !name.starts_with('.') => Category::Uplifted,
            _ => Category::Other,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Tally {
    pub files: u64,
    pub logical: u64,
    pub allocated: u64,
    pub compressed: u64,
}

impl Tally {
    fn add(&mut self, other: &Tally) {
        self.files += other.files;
        self.logical += other.logical;
        self.allocated += other.allocated;
        self.compressed += other.compressed;
    }
}

#[derive(Debug, Default, Clone)]
pub struct Breakdown {
    pub rows: BTreeMap<Category, Tally>,
}

impl Breakdown {
    pub fn total(&self) -> Tally {
        let mut t = Tally::default();
        for row in self.rows.values() {
            t.add(row);
        }
        t
    }

    pub fn merge(&mut self, other: &Breakdown) {
        for (cat, row) in &other.rows {
            self.rows.entry(*cat).or_default().add(row);
        }
    }

    pub fn get(&self, cat: Category) -> Tally {
        self.rows.get(&cat).copied().unwrap_or_default()
    }
}

pub fn profile_dirs(target: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if !sys::lstat(target).is_ok_and(|meta| meta.is_dir()) {
        return out;
    }
    let Ok(children) = fs::read_dir(target) else { return out };
    for child in children.flatten() {
        let path = child.path();
        if !child.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        if path.join("deps").is_dir() {
            out.push(path);
            continue;
        }
        if let Ok(grand) = fs::read_dir(&path) {
            for g in grand.flatten() {
                if g.file_type().is_ok_and(|t| t.is_dir()) && g.path().join("deps").is_dir() {
                    out.push(g.path());
                }
            }
        }
    }
    out.sort();
    out
}

/// Reflink clones are each counted in full, as `du` does; [`physical_size`] is the clone-aware total.
pub fn breakdown(target: &Path) -> Breakdown {
    let mut b = Breakdown::default();
    let mut seen = HashSet::new();
    let profiles = profile_dirs(target);
    for profile in &profiles {
        walk_files(profile, &mut |path, meta| {
            if meta.nlink() > 1 && !seen.insert((meta.dev(), meta.ino())) {
                return;
            }
            let rel = path.strip_prefix(profile).unwrap_or(path);
            let row = b.rows.entry(Category::of(rel)).or_default();
            row.add(&tally_of(meta));
        });
    }
    walk_files(target, &mut |path, meta| {
        if profiles.iter().any(|p| path.starts_with(p)) {
            return;
        }
        b.rows.entry(Category::Other).or_default().add(&tally_of(meta));
    });
    b
}

fn tally_of(meta: &sys::Meta) -> Tally {
    Tally { files: 1, logical: meta.len(), allocated: meta.allocated(), compressed: u64::from(apfs::is_compressed(meta)) }
}

fn walk_files(root: &Path, visit: &mut dyn FnMut(&Path, &sys::Meta)) {
    if !sys::lstat(root).is_ok_and(|meta| meta.is_dir()) {
        return;
    }
    let Ok(entries) = fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            walk_files(&entry.path(), visit);
        } else if kind.is_file()
            && let Ok(meta) = sys::lstat(&entry.path())
        {
            visit(&entry.path(), &meta);
        }
    }
}

const WALK_THREADS: usize = 4;

type Skip<'a> = &'a (dyn Fn(&str, bool) -> bool + Sync);
type Visit<'a, T> = &'a (dyn Fn(&mut T, &Path, &sys::Meta) + Sync);

fn walk_into<T>(dir: &Path, skip: Skip<'_>, acc: &mut T, visit: Visit<'_, T>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else { continue };
        if skip(&entry.file_name().to_string_lossy(), kind.is_dir()) {
            continue;
        }
        if kind.is_dir() {
            walk_into(&entry.path(), skip, acc, visit);
        } else if kind.is_file()
            && let Ok(meta) = sys::lstat(&entry.path())
        {
            visit(acc, &entry.path(), &meta);
        }
    }
}

/// The subtrees two levels down are shared among a few scoped threads; the union of the results equals a serial walk.
fn walk_parallel<T: Send>(root: &Path, skip: Skip<'_>, init: &(dyn Fn() -> T + Sync), visit: Visit<'_, T>) -> Vec<T> {
    let mut first = init();
    let mut jobs = vec![root.to_path_buf()];
    for _ in 0..2 {
        let mut next = Vec::new();
        for dir in &jobs {
            let Ok(entries) = fs::read_dir(dir) else { continue };
            for entry in entries.flatten() {
                let Ok(kind) = entry.file_type() else { continue };
                if skip(&entry.file_name().to_string_lossy(), kind.is_dir()) {
                    continue;
                }
                if kind.is_dir() {
                    next.push(entry.path());
                } else if kind.is_file()
                    && let Ok(meta) = sys::lstat(&entry.path())
                {
                    visit(&mut first, &entry.path(), &meta);
                }
            }
        }
        jobs = next;
    }
    jobs.sort();
    let claimed = AtomicUsize::new(0);
    let workers = WALK_THREADS.min(jobs.len());
    let mut out = vec![first];
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    let mut acc = init();
                    while let Some(dir) = jobs.get(claimed.fetch_add(1, Ordering::Relaxed)) {
                        walk_into(dir, skip, &mut acc, visit);
                    }
                    acc
                })
            })
            .collect();
        for handle in handles {
            match handle.join() {
                Ok(acc) => out.push(acc),
                Err(panic) => std::panic::resume_unwind(panic),
            }
        }
    });
    out
}

/// Runs `f` on its own thread at background CPU and I/O priority, so the settings end with the thread.
pub fn at_background_priority<R: Send>(f: impl FnOnce() -> R + Send) -> R {
    std::thread::scope(|scope| {
        let handle = scope.spawn(|| {
            sys::lower_thread_priority();
            f()
        });
        match handle.join() {
            Ok(result) => result,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    })
}

/// Compressed files keep their data in the resource fork, where extent probing cannot see it, so APFS uses `getattrlist`.
pub fn physical_size(roots: &[&Path]) -> u64 {
    if !cfg!(target_os = "macos") {
        return fsx::physical_size(roots);
    }
    let mut inodes = HashSet::new();
    let mut groups = HashSet::new();
    let mut total = 0;
    for root in roots {
        walk_files(root, &mut |path, meta| {
            if !inodes.insert((meta.dev(), meta.ino())) {
                return;
            }
            let allocated = meta.allocated();
            total += match apfs::clone_info(path) {
                Some((group, private)) if !groups.insert((meta.dev(), group)) => private.min(allocated),
                _ => allocated,
            };
        });
    }
    total
}

struct AllocationNode {
    meta: sys::Meta,
    members: u64,
    children: Vec<PathBuf>,
    clone_group: Option<(u64, u64)>,
}

fn allocation_unchanged(before: &sys::Meta, after: &sys::Meta) -> bool {
    (before.dev(), before.ino(), before.len(), before.nlink(), before.allocated(), before.mtime(), before.ctime(), before.file_type())
        == (after.dev(), after.ino(), after.len(), after.nlink(), after.allocated(), after.mtime(), after.ctime(), after.file_type())
}

fn allocation_error(path: &Path, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{}: {error}", path.display()))
}

fn allocation_add(total: &mut u64, bytes: u64) -> io::Result<()> {
    *total = total.checked_add(bytes).ok_or_else(|| io::Error::other("allocated byte count overflowed"))?;
    Ok(())
}

/// Checked allocated-block estimate including directories and symlinks; detected APFS sharing is counted once.
pub fn physical_size_checked(roots: &[&Path]) -> io::Result<u64> {
    physical_sizes_checked(&[roots]).map(|totals| totals[0])
}

pub(crate) fn physical_sizes_checked(root_sets: &[&[&Path]]) -> io::Result<Vec<u64>> {
    if root_sets.len() > 64 { return Err(io::Error::new(io::ErrorKind::InvalidInput, "at most 64 allocation unions are supported")); }
    let mut nodes: BTreeMap<(u64, u64), AllocationNode> = BTreeMap::new();
    let mut paths: BTreeMap<PathBuf, (u64, u64)> = BTreeMap::new();
    let mut pending: Vec<_> = root_sets.iter().enumerate().flat_map(|(index, roots)|
        roots.iter().map(move |root| (root.to_path_buf(), 1u64 << index))).collect();
    while let Some((path, membership)) = pending.pop() {
        let identity = if let Some(identity) = paths.get(&path) { *identity } else {
            let meta = sys::lstat(&path).map_err(|error| allocation_error(&path, error))?;
            if meta.ino() == 0 || !(meta.is_file() || meta.is_dir() || meta.file_type().is_symlink()) {
                return Err(allocation_error(&path, io::Error::new(io::ErrorKind::Unsupported, "unverifiable inode or file type")));
            }
            let identity = (meta.dev(), meta.ino());
            if let Some(existing) = nodes.get(&identity) {
                if !allocation_unchanged(&existing.meta, &meta) {
                    return Err(allocation_error(&path, io::Error::other("allocation changed during inventory")));
                }
            } else {
                let children = if meta.is_dir() && !meta.file_type().is_symlink() {
                    fs::read_dir(&path).map_err(|error| allocation_error(&path, error))?
                        .map(|entry| entry.map(|entry| entry.path()).map_err(|error| allocation_error(&path, error)))
                        .collect::<io::Result<Vec<_>>>()?
                } else { Vec::new() };
                let clone_group = if meta.is_file() { apfs::clone_info(&path).filter(|(group, _)| *group != 0) } else { None };
                nodes.insert(identity, AllocationNode { meta, members: 0, children, clone_group });
            }
            paths.insert(path.clone(), identity);
            identity
        };
        let node = nodes.get_mut(&identity).unwrap();
        let added = membership & !node.members;
        if added == 0 { continue; }
        node.members |= added;
        pending.extend(node.children.iter().map(|child| (child.clone(), added)));
    }
    // One inventory supplies every union; any observed change invalidates the allocation sample.
    for (path, identity) in paths {
        let current = sys::lstat(&path).map_err(|error| allocation_error(&path, error))?;
        if !allocation_unchanged(&nodes[&identity].meta, &current) {
            return Err(allocation_error(&path, io::Error::other("allocation changed during inventory")));
        }
    }
    (0..root_sets.len()).map(|index| {
        let mut total = 0;
        let mut clones: HashMap<(u64, u64), (u64, u64)> = HashMap::new();
        for node in nodes.values().filter(|node| node.members & (1u64 << index) != 0) {
            let allocated = node.meta.allocated();
            if let Some((group, private)) = node.clone_group {
                let private = private.min(allocated);
                let (private_total, shared_max) = clones.entry((node.meta.dev(), group)).or_default();
                allocation_add(private_total, private)?;
                *shared_max = (*shared_max).max(allocated - private);
            } else { allocation_add(&mut total, allocated)?; }
        }
        // Private blocks sum, while the largest detected shared portion is charged once per clone group.
        for (private, shared) in clones.into_values() {
            allocation_add(&mut total, private)?;
            allocation_add(&mut total, shared)?;
        }
        Ok(total)
    }).collect()
}

#[derive(Debug, Clone)]
struct Unit {
    hash: String,
    identity: String,
    fingerprint: Option<u64>,
    deps: Vec<u64>,
    newest: SystemTime,
}

#[derive(Debug, Default, Clone)]
pub struct Analysis {
    pub live: HashSet<String>,
    pub dead: HashSet<String>,
    pub known: HashSet<String>,
    pub removals: Vec<(PathBuf, u64)>,
    pub built: HashMap<String, SystemTime>,
}

fn dir_hash(name: &str) -> Option<&str> {
    let (_, hash) = name.rsplit_once('-')?;
    (hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit())).then_some(hash)
}

pub fn artifact_hash(name: &str) -> Option<&str> {
    dir_hash(name.split('.').next()?)
}

/// cargo writes a unit's fingerprint as the hex of its little-endian bytes.
fn parse_fingerprint(text: &str) -> Option<u64> {
    let text = text.trim();
    if text.len() != 16 {
        return None;
    }
    let mut bytes = [0u8; 8];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = u8::from_str_radix(text.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(u64::from_le_bytes(bytes))
}

fn newest_mtime(dir: &Path) -> SystemTime {
    let mut newest = SystemTime::UNIX_EPOCH;
    if let Ok(entries) = fs::read_dir(dir) {
        for e in entries.flatten() {
            if let Ok(t) = e.metadata().and_then(|m| m.modified()) {
                newest = newest.max(t);
            }
        }
    }
    newest
}

/// `None` when anything is not in the expected shape, so nothing is classified on partial information.
fn read_units(profile: &Path) -> Option<(Vec<Unit>, HashSet<String>)> {
    let mut units = Vec::new();
    let mut unclassified = HashSet::new();
    let Ok(dirs) = fs::read_dir(profile.join(".fingerprint")) else { return Some((units, unclassified)) };
    for dir in dirs.flatten() {
        let name = dir.file_name().to_string_lossy().into_owned();
        let Some(hash) = dir_hash(&name) else { continue };
        if !dir.file_type().ok()?.is_dir() {
            unclassified.insert(hash.to_string());
            continue;
        }
        let pkg = &name[..name.len() - 17];
        let mut found = false;
        for file in fs::read_dir(dir.path()).ok()?.flatten() {
            let fname = file.file_name().to_string_lossy().into_owned();
            let Some(stem) = fname.strip_suffix(".json") else { continue };
            found = true;
            let json: serde_json::Value = serde_json::from_slice(&fs::read(file.path()).ok()?).ok()?;
            let field = |k: &str| json.get(k).map(|v| v.to_string());
            let deps = json
                .get("deps")?
                .as_array()?
                .iter()
                .map(|d| d.as_array().and_then(|a| a.get(3)).and_then(|v| v.as_u64()))
                .collect::<Option<Vec<u64>>>()?;
            let identity =
                format!("{pkg}\0{stem}\0{}\0{}\0{}", field("rustc")?, field("profile")?, field("compile_kind")?);
            let fingerprint = fs::read_to_string(dir.path().join(stem)).ok().and_then(|t| parse_fingerprint(&t));
            units.push(Unit { hash: hash.to_string(), identity, fingerprint, deps, newest: newest_mtime(&dir.path()) });
        }
        if !found {
            unclassified.insert(hash.to_string());
        }
    }
    Some((units, unclassified))
}

fn uplifted_hashes(profile: &Path) -> HashSet<String> {
    let mut out = HashSet::new();
    for (top, inner) in [(profile.to_path_buf(), profile.join("deps")), (profile.join("examples"), profile.join("examples"))] {
        let Ok(entries) = fs::read_dir(&top) else { continue };
        let inner_names: Vec<String> = fs::read_dir(&inner)
            .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_default();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Ok(meta) = sys::lstat(&entry.path()) else { continue };
            if !meta.is_file() || name.starts_with('.') || name.ends_with(".d") || artifact_hash(&name).is_some() {
                continue;
            }
            let (stem, ext) = match name.split_once('.') {
                Some((s, e)) => (s.to_string(), format!(".{e}")),
                None => (name.clone(), String::new()),
            };
            for cand in &inner_names {
                let Some(hash) = artifact_hash(cand) else { continue };
                if *cand != format!("{stem}-{hash}{ext}") {
                    continue;
                }
                let path = inner.join(cand);
                if same_content(&entry.path(), &meta, &path) {
                    out.insert(hash.to_string());
                }
            }
        }
    }
    out
}

fn same_content(a: &Path, a_meta: &sys::Meta, b: &Path) -> bool {
    let Ok(b_meta) = sys::lstat(b) else { return false };
    if (a_meta.dev(), a_meta.ino()) == (b_meta.dev(), b_meta.ino()) {
        return true;
    }
    a_meta.len() == b_meta.len() && matches!((content_hash(a), content_hash(b)), (Ok(x), Ok(y)) if x == y)
}

pub fn analyze(profile: &Path) -> Option<Analysis> {
    let (units, unclassified) = read_units(profile)?;
    let mut a = Analysis::default();
    a.known.extend(units.iter().map(|u| u.hash.clone()));
    a.known.extend(unclassified.iter().cloned());

    let mut roots: HashSet<usize> = HashSet::new();
    let mut newest: HashMap<&str, SystemTime> = HashMap::new();
    for u in &units {
        let t = newest.entry(&u.identity).or_insert(u.newest);
        *t = (*t).max(u.newest);
    }
    let uplifted = uplifted_hashes(profile);
    for (i, u) in units.iter().enumerate() {
        if newest.get(u.identity.as_str()) == Some(&u.newest) || uplifted.contains(&u.hash) {
            roots.insert(i);
        }
    }
    let mut by_fingerprint: HashMap<u64, Vec<usize>> = HashMap::new();
    for (i, u) in units.iter().enumerate() {
        if let Some(f) = u.fingerprint {
            by_fingerprint.entry(f).or_default().push(i);
        }
    }
    let mut stack: Vec<usize> = roots.iter().copied().collect();
    let mut live_units: HashSet<usize> = roots;
    while let Some(i) = stack.pop() {
        for dep in &units[i].deps {
            for &j in by_fingerprint.get(dep).map(Vec::as_slice).unwrap_or(&[]) {
                if live_units.insert(j) {
                    stack.push(j);
                }
            }
        }
    }
    for i in &live_units {
        a.live.insert(units[*i].hash.clone());
    }
    a.live.extend(unclassified);
    for u in &units {
        if !a.live.contains(&u.hash) {
            a.dead.insert(u.hash.clone());
        }
    }
    for u in &units {
        let t = a.built.entry(u.hash.clone()).or_insert(u.newest);
        *t = (*t).max(u.newest);
    }
    a.removals = unit_paths(profile, &a.dead).into_values().flatten().collect();
    Some(a)
}

/// rustc hard-links objects between an incremental session and `deps/`, which ties a crate directory to its unit.
pub fn unit_paths(profile: &Path, units: &HashSet<String>) -> HashMap<String, Vec<(PathBuf, u64)>> {
    let mut out: HashMap<String, Vec<(PathBuf, u64)>> = HashMap::new();
    if units.is_empty() {
        return out;
    }
    let mut push = |hash: &str, path: PathBuf| {
        let bytes = fsx::allocated_size(&path);
        out.entry(hash.to_string()).or_default().push((path, bytes));
    };
    for sub in [".fingerprint", "build"] {
        if let Ok(entries) = fs::read_dir(profile.join(sub)) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if let Some(h) = dir_hash(&name).filter(|h| units.contains(*h)) {
                    push(h, e.path());
                }
            }
        }
    }
    let mut mine: HashMap<(u64, u64), String> = HashMap::new();
    let mut others: HashSet<(u64, u64)> = HashSet::new();
    for sub in ["deps", "examples"] {
        if let Ok(entries) = fs::read_dir(profile.join(sub)) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let Some(hash) = artifact_hash(&name) else { continue };
                let meta = sys::lstat(&e.path()).ok();
                let linked = meta.as_ref().filter(|m| m.is_file() && m.nlink() > 1).map(|m| (m.dev(), m.ino()));
                if units.contains(hash) {
                    if let Some(i) = linked {
                        mine.insert(i, hash.to_string());
                    }
                    push(hash, e.path());
                } else {
                    others.extend(linked);
                }
            }
        }
    }
    if !mine.is_empty()
        && let Ok(crates) = fs::read_dir(profile.join("incremental"))
    {
        for krate in crates.flatten() {
            if !krate.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let mut inodes = HashSet::new();
            walk_files(&krate.path(), &mut |_, m| {
                inodes.insert((m.dev(), m.ino()));
            });
            let owner = inodes.iter().find_map(|i| mine.get(i)).cloned();
            if let Some(owner) = owner
                && !inodes.iter().any(|i| others.contains(i))
            {
                push(&owner, krate.path());
            }
        }
    }
    out
}

/// rustc loads only the newest session; in-progress `-working` ones are kept.
pub fn stale_sessions(profile: &Path) -> Vec<(PathBuf, u64)> {
    let mut out = Vec::new();
    let Ok(crates) = fs::read_dir(profile.join("incremental")) else { return out };
    for krate in crates.flatten() {
        if !krate.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let mut sessions: Vec<(SystemTime, PathBuf)> = fs::read_dir(krate.path())
            .map(|rd| {
                rd.flatten()
                    .filter(|e| {
                        let name = e.file_name().to_string_lossy().into_owned();
                        name.starts_with("s-") && !name.ends_with("-working") && e.file_type().is_ok_and(|kind| kind.is_dir())
                    })
                    .map(|e| (e.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH), e.path()))
                    .collect()
            })
            .unwrap_or_default();
        sessions.sort_by_key(|s| std::cmp::Reverse(s.0));
        for (_, path) in sessions.into_iter().skip(1) {
            let bytes = fsx::allocated_size(&path);
            out.push((path, bytes));
        }
    }
    out
}

pub fn newest_session(krate: &Path) -> Option<(SystemTime, PathBuf)> {
    if !sys::lstat(krate).ok()?.is_dir() {
        return None;
    }
    fs::read_dir(krate)
        .ok()?
        .flatten()
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.starts_with("s-") && !name.ends_with("-working") && e.file_type().is_ok_and(|kind| kind.is_dir())
        })
        .map(|e| (e.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH), e.path()))
        .max_by_key(|s| s.0)
}

// Each incremental compile hard-links its objects into `deps/` under a new name and never removes older names; this is the dominant growth.
fn archive_members(path: &Path) -> Option<Vec<String>> {
    let file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let mut magic = [0u8; 8];
    sys::read_exact_at(&file, &mut magic, 0).ok()?;
    if &magic != b"!<arch>\n" {
        return None;
    }
    let mut out = Vec::new();
    let mut gnu_names: Vec<u8> = Vec::new();
    let mut pos = 8u64;
    while pos + 60 <= len {
        let mut header = [0u8; 60];
        sys::read_exact_at(&file, &mut header, pos).ok()?;
        if &header[58..60] != b"`\n" {
            return None;
        }
        let size: u64 = std::str::from_utf8(&header[48..58]).ok()?.trim().parse().ok()?;
        let raw = std::str::from_utf8(&header[0..16]).ok()?.trim_end();
        let data = pos + 60;
        if data + size > len {
            return None;
        }
        if let Some(n) = raw.strip_prefix("#1/") {
            let n: usize = n.parse().ok()?;
            if n as u64 > size || n > 4096 {
                return None;
            }
            let mut buf = vec![0u8; n];
            sys::read_exact_at(&file, &mut buf, data).ok()?;
            out.push(String::from_utf8_lossy(&buf).trim_end_matches('\0').to_string());
        } else if raw == "//" {
            if size > 64 * 1024 * 1024 {
                return None;
            }
            gnu_names = vec![0u8; size as usize];
            sys::read_exact_at(&file, &mut gnu_names, data).ok()?;
        } else if raw == "/" || raw == "/SYM64/" {
        } else if let Some(offset) = raw.strip_prefix('/') {
            let offset: usize = offset.parse().ok()?;
            let rest = gnu_names.get(offset..)?;
            let end = rest.iter().position(|b| *b == b'\n').unwrap_or(rest.len());
            out.push(String::from_utf8_lossy(&rest[..end]).trim_end_matches('/').to_string());
        } else {
            out.push(raw.trim_end_matches('/').to_string());
        }
        pos = data + size + (size % 2);
    }
    Some(out)
}

fn macho_debug_objects(path: &Path) -> Option<Vec<String>> {
    const MH_MAGIC_64: u32 = 0xfeed_facf;
    const LC_SYMTAB: u32 = 0x2;
    const N_OSO: u8 = 0x66;
    let file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let u32_at = |b: &[u8], o: usize| b.get(o..o + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]));
    let mut header = [0u8; 32];
    sys::read_exact_at(&file, &mut header, 0).ok()?;
    if u32_at(&header, 0)? != MH_MAGIC_64 {
        return None;
    }
    let ncmds = u32_at(&header, 16)?;
    let sizeofcmds = u32_at(&header, 20)? as usize;
    if sizeofcmds as u64 + 32 > len || sizeofcmds > 16 * 1024 * 1024 {
        return None;
    }
    let mut cmds = vec![0u8; sizeofcmds];
    sys::read_exact_at(&file, &mut cmds, 32).ok()?;
    let mut off = 0usize;
    for _ in 0..ncmds {
        let cmd = u32_at(&cmds, off)?;
        let size = u32_at(&cmds, off + 4)? as usize;
        if cmd == LC_SYMTAB {
            let symoff = u64::from(u32_at(&cmds, off + 8)?);
            let nsyms = u64::from(u32_at(&cmds, off + 12)?);
            let stroff = u64::from(u32_at(&cmds, off + 16)?);
            let strsize = u64::from(u32_at(&cmds, off + 20)?);
            if symoff + nsyms * 16 > len || stroff + strsize > len || nsyms * 16 > 512 * 1024 * 1024 {
                return None;
            }
            let mut syms = vec![0u8; (nsyms * 16) as usize];
            sys::read_exact_at(&file, &mut syms, symoff).ok()?;
            let mut out = Vec::new();
            for entry in syms.as_chunks::<16>().0 {
                if entry[4] != N_OSO {
                    continue;
                }
                let strx = u64::from(u32_at(entry, 0)?);
                if strx >= strsize {
                    continue;
                }
                let mut buf = vec![0u8; (strsize - strx).min(4096) as usize];
                sys::read_exact_at(&file, &mut buf, stroff + strx).ok()?;
                let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
                out.push(String::from_utf8_lossy(&buf[..end]).into_owned());
            }
            return Some(out);
        }
        if size < 8 {
            return None;
        }
        off += size;
    }
    Some(Vec::new())
}

/// rustc names each DWARF compile unit `<source>/@/<codegen unit>`, so an ELF image lists its codegen units even after its objects are gone.
fn elf_debug_cgus(path: &Path) -> Option<Vec<String>> {
    const SHF_COMPRESSED: u64 = 0x800;
    let file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let mut header = [0u8; 64];
    sys::read_exact_at(&file, &mut header, 0).ok()?;
    if &header[0..4] != b"\x7fELF" {
        return None;
    }
    let wide = match header[4] {
        1 => false,
        2 => true,
        _ => return None,
    };
    let big = match header[5] {
        1 => false,
        2 => true,
        _ => return None,
    };
    let int = |b: &[u8], o: usize, n: usize| -> Option<u64> {
        let s = b.get(o..o + n)?;
        Some(if big { s.iter().fold(0u64, |v, x| (v << 8) | u64::from(*x)) } else { s.iter().rev().fold(0u64, |v, x| (v << 8) | u64::from(*x)) })
    };
    let word = if wide { 8 } else { 4 };
    let (shoff, at) = if wide { (int(&header, 0x28, 8)?, 0x3a) } else { (int(&header, 0x20, 4)?, 0x2e) };
    let (shentsize, shnum, shstrndx) = (int(&header, at, 2)? as usize, int(&header, at + 2, 2)? as usize, int(&header, at + 4, 2)? as usize);
    if shnum == 0 || shentsize < 4 * word + 8 || shoff + (shnum * shentsize) as u64 > len {
        return None;
    }
    let mut table = vec![0u8; shnum * shentsize];
    sys::read_exact_at(&file, &mut table, shoff).ok()?;
    // (name offset, flags, file offset, size) of each section header.
    let section = |i: usize| -> Option<(u64, u64, u64, u64)> {
        let s = table.get(i * shentsize..(i + 1) * shentsize)?;
        Some((int(s, 0, 4)?, int(s, 8, word)?, int(s, 8 + 2 * word, word)?, int(s, 8 + 3 * word, word)?))
    };
    let read = |offset: u64, size: u64| -> Option<Vec<u8>> {
        if offset.checked_add(size)? > len || size > 512 * 1024 * 1024 {
            return None;
        }
        let mut buf = vec![0u8; size as usize];
        sys::read_exact_at(&file, &mut buf, offset).ok()?;
        Some(buf)
    };
    let (_, _, names_off, names_size) = section(shstrndx)?;
    let names = read(names_off, names_size)?;
    let mut out = Vec::new();
    for i in 0..shnum {
        let (name, flags, offset, size) = section(i)?;
        let name = names.get(name as usize..).and_then(|n| n.split(|b| *b == 0).next()).unwrap_or_default();
        if !matches!(name, b".debug_str" | b".debug_line_str") || flags & SHF_COMPRESSED != 0 {
            continue;
        }
        for s in read(offset, size)?.split(|b| *b == 0) {
            if let Some(at) = s.windows(3).rposition(|w| w == b"/@/") {
                out.push(String::from_utf8_lossy(&s[at + 3..]).into_owned());
            }
        }
    }
    Some(out)
}

/// The codegen units `deps/` artifacts of `krate` name, by unit hash; objects are named `<crate>-<hash>.<cgu>.<tag>.rcgu.o`.
fn named_cgus(profile: &Path, krate: &str) -> HashMap<String, HashSet<String>> {
    let mut out: HashMap<String, HashSet<String>> = HashMap::new();
    for sub in ["deps", "examples"] {
        let Ok(entries) = fs::read_dir(profile.join(sub)) else { continue };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let (Some(hash), Some(owner)) = (artifact_hash(&name), artifact_crate(&name)) else { continue };
            if owner != krate {
                continue;
            }
            let path = e.path();
            let from_objects = |objects: Vec<String>| -> Vec<String> {
                objects
                    .iter()
                    .filter_map(|o| Path::new(o).file_name()?.to_str()?.strip_suffix(".rcgu.o")?.split('.').nth(1).map(str::to_string))
                    .collect()
            };
            let cgus = if name.ends_with(".rlib") {
                archive_members(&path).map(from_objects)
            } else if !name.contains('.') || name.ends_with(".so") || name.ends_with(".dylib") {
                elf_debug_cgus(&path).or_else(|| macho_debug_objects(&path).map(from_objects))
            } else {
                None
            };
            for cgu in cgus.into_iter().flatten() {
                out.entry(cgu).or_default().insert(hash.to_string());
            }
        }
    }
    out
}

fn current_objects(dir: &Path, stem: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    if let Some(members) = archive_members(&dir.join(format!("lib{stem}.rlib"))) {
        out.extend(members);
    }
    for image in [dir.join(stem), dir.join(format!("lib{stem}.dylib"))] {
        if let Some(objects) = macho_debug_objects(&image) {
            out.extend(objects.iter().filter_map(|o| Path::new(o).file_name()).map(|n| n.to_string_lossy().into_owned()));
        }
    }
    out
}

/// Units whose artifacts name none of their objects are left alone, since nothing then proves which are current.
pub fn stale_objects(profile: &Path) -> Vec<(PathBuf, u64)> {
    let mut out = Vec::new();
    for sub in ["deps", "examples"] {
        let dir = profile.join(sub);
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        let mut units: HashMap<String, Vec<(String, sys::Meta)>> = HashMap::new();
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".o") || artifact_hash(&name).is_none() {
                continue;
            }
            let Ok(meta) = sys::lstat(&e.path()) else { continue };
            if meta.is_file() {
                let stem = name.split('.').next().unwrap_or_default().to_string();
                units.entry(stem).or_default().push((name, meta));
            }
        }
        for (stem, objects) in units {
            let tags: HashSet<&str> = objects.iter().filter_map(|(n, _)| n.split('.').nth(2)).collect();
            if tags.len() < 2 {
                continue;
            }
            let current = current_objects(&dir, &stem);
            let in_use: Vec<&sys::Meta> = objects.iter().filter(|(n, _)| current.contains(n)).map(|o| &o.1).collect();
            if in_use.is_empty() {
                continue;
            }
            let attributable = in_use.iter().any(|m| m.nlink() > 1);
            for (name, meta) in &objects {
                if current.contains(name) || !(attributable || meta.nlink() == 1) {
                    continue;
                }
                let bytes = if meta.nlink() == 1 { meta.allocated() } else { 0 };
                out.push((dir.join(name), bytes));
            }
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum IncrementalOwner {
    Live,
    Dead,
    Orphan,
    /// Nothing proves either way (for example a `check` build has no objects): kept.
    Unknown,
}

#[derive(Debug, Clone)]
pub struct IncrementalDir {
    pub path: PathBuf,
    pub crate_name: String,
    pub owner: IncrementalOwner,
    pub units: HashSet<String>,
    pub touched: SystemTime,
}

fn artifact_crate(name: &str) -> Option<&str> {
    let stem = name.split('.').next()?;
    let (krate, _) = stem.rsplit_once('-')?;
    let is_lib = [".rlib", ".rmeta", ".dylib", ".so", ".a"].iter().any(|e| name.ends_with(e));
    Some(if is_lib { krate.strip_prefix("lib").unwrap_or(krate) } else { krate })
}

pub fn incremental_dirs(profile: &Path, analysis: &Analysis) -> Vec<IncrementalDir> {
    let mut owners: HashMap<(u64, u64), String> = HashMap::new();
    let mut live_crates: HashSet<String> = HashSet::new();
    for sub in ["deps", "examples"] {
        let Ok(entries) = fs::read_dir(profile.join(sub)) else { continue };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some(hash) = artifact_hash(&name) else { continue };
            if analysis.live.contains(hash)
                && let Some(krate) = artifact_crate(&name)
            {
                live_crates.insert(krate.to_string());
            }
            if let Ok(meta) = sys::lstat(&e.path())
                && meta.is_file()
                && meta.nlink() > 1
            {
                owners.insert((meta.dev(), meta.ino()), hash.to_string());
            }
        }
    }
    let mut out = Vec::new();
    let mut unattributable: Vec<usize> = Vec::new();
    let mut cgus_by_crate: HashMap<String, HashMap<String, HashSet<String>>> = HashMap::new();
    let Ok(crates) = fs::read_dir(profile.join("incremental")) else { return out };
    for krate in crates.flatten() {
        let path = krate.path();
        if !krate.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let dir_name = krate.file_name().to_string_lossy().into_owned();
        let crate_name = dir_name.rsplit_once('-').map(|(c, _)| c.to_string()).unwrap_or(dir_name.clone());
        let Some((touched, session)) = newest_session(&path) else {
            out.push(IncrementalDir { path, crate_name, owner: IncrementalOwner::Unknown, units: HashSet::new(), touched: SystemTime::UNIX_EPOCH });
            continue;
        };
        let mut objects = 0usize;
        let mut units = HashSet::new();
        let mut stems = Vec::new();
        if let Ok(files) = fs::read_dir(&session) {
            for f in files.flatten() {
                let file_name = f.file_name().to_string_lossy().into_owned();
                let Some(stem) = file_name.strip_suffix(".o") else { continue };
                let Ok(meta) = sys::lstat(&f.path()) else { continue };
                objects += 1;
                stems.push(stem.to_string());
                if let Some(h) = owners.get(&(meta.dev(), meta.ino())) {
                    units.insert(h.clone());
                }
            }
        }
        // Without object links in `deps/` (ELF targets keep none), session objects are matched by codegen-unit name.
        if units.is_empty() && !stems.is_empty() {
            let named = cgus_by_crate.entry(crate_name.clone()).or_insert_with(|| named_cgus(profile, &crate_name));
            units.extend(stems.iter().filter_map(|s| named.get(s)).flatten().cloned());
        }
        let owner = if units.iter().any(|h| analysis.live.contains(h)) {
            IncrementalOwner::Live
        } else if !units.is_empty() && units.iter().all(|h| analysis.dead.contains(h)) {
            IncrementalOwner::Dead
        // COFF objects carry no codegen-unit names and the MSVC linker keeps no links, so ownership is settled below by recency.
        } else if units.is_empty() && objects > 0 && !cfg!(windows) {
            IncrementalOwner::Orphan
        } else {
            IncrementalOwner::Unknown
        };
        if cfg!(windows) && units.is_empty() && objects > 0 {
            unattributable.push(out.len());
        }
        out.push(IncrementalDir { path, crate_name, owner, units, touched });
    }
    // Without any proof of ownership, a directory is superseded only when a sibling of the same crate was touched after it.
    let newest: HashMap<String, SystemTime> = unattributable.iter().fold(HashMap::new(), |mut m, &i| {
        let e = m.entry(out[i].crate_name.clone()).or_insert(SystemTime::UNIX_EPOCH);
        *e = (*e).max(out[i].touched);
        m
    });
    for &i in &unattributable {
        if out[i].touched < newest[&out[i].crate_name] {
            out[i].owner = IncrementalOwner::Orphan;
        }
    }
    // An unattributed directory is an orphan only if its crate has no live unit or another directory is the live one.
    let live_dirs: HashSet<String> = out.iter().filter(|d| d.owner == IncrementalOwner::Live).map(|d| d.crate_name.clone()).collect();
    for d in &mut out {
        if d.owner == IncrementalOwner::Orphan && live_crates.contains(&d.crate_name) && !live_dirs.contains(&d.crate_name) {
            d.owner = IncrementalOwner::Unknown;
        }
    }
    out
}

// Only completed metadata-only caches qualify; any working state, object, symlink or unreadable entry declines reclamation.
fn idle_unknown_incremental(path: &Path, now: SystemTime) -> Option<Duration> {
    let mut pending = vec![path.to_path_buf()];
    let mut newest = SystemTime::UNIX_EPOCH;
    let mut completed = false;
    while let Some(dir) = pending.pop() {
        let meta = sys::lstat(&dir).ok()?;
        if !meta.is_dir() || meta.file_type().is_symlink() {
            return None;
        }
        newest = newest.max(meta.modified().ok()?);
        for entry in fs::read_dir(&dir).ok()? {
            let entry = entry.ok()?;
            let name = entry.file_name();
            if name.to_string_lossy().ends_with("-working") {
                return None;
            }
            let child = entry.path();
            let meta = sys::lstat(&child).ok()?;
            newest = newest.max(meta.modified().ok()?);
            if meta.is_dir() {
                completed |= dir == path && name.to_string_lossy().starts_with("s-");
                pending.push(child);
            } else if !meta.is_file() || child.extension().is_some_and(|e| e == "o") {
                return None;
            }
        }
    }
    if completed { now.duration_since(newest).ok() } else { None }
}

/// Every inode is counted once, in the first bucket it falls into, in field order.
#[derive(Debug, Default, Clone, Serialize)]
pub struct IncrementalAnatomy {
    pub crate_dirs: usize,
    pub sessions: usize,
    pub dead_dirs: usize,
    pub dead_dir_bytes: u64,
    pub newest_metadata: u64,
    pub newest_objects: u64,
    pub stale_sessions: u64,
    pub deps_objects_current: u64,
    pub deps_objects_stale: u64,
    pub deps_object_files: usize,
    pub newest_by_crate: Vec<(String, u64)>,
}

impl IncrementalAnatomy {
    pub fn total(&self) -> u64 {
        self.dead_dir_bytes + self.newest_metadata + self.newest_objects + self.stale_sessions + self.deps_objects_current + self.deps_objects_stale
    }
}

pub fn incremental_anatomy(profile: &Path) -> IncrementalAnatomy {
    let mut a = IncrementalAnatomy::default();
    let analysis = analyze(profile).unwrap_or_default();
    let dirs = incremental_dirs(profile, &analysis);
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    let charge = |path: &Path, seen: &mut HashSet<(u64, u64)>| -> u64 {
        let mut total = 0;
        walk_files(path, &mut |_, m| {
            if seen.insert((m.dev(), m.ino())) {
                total += m.allocated();
            }
        });
        total
    };
    a.crate_dirs = dirs.len();
    for d in &dirs {
        a.sessions += fs::read_dir(&d.path)
            .map(|rd| rd.flatten().filter(|e| e.file_name().to_string_lossy().starts_with("s-") && e.path().is_dir()).count())
            .unwrap_or(0);
        if matches!(d.owner, IncrementalOwner::Dead | IncrementalOwner::Orphan) {
            a.dead_dirs += 1;
            a.dead_dir_bytes += charge(&d.path, &mut seen);
        }
    }
    let live_dirs: Vec<&IncrementalDir> = dirs.iter().filter(|d| !matches!(d.owner, IncrementalOwner::Dead | IncrementalOwner::Orphan)).collect();
    for d in &live_dirs {
        let Some((_, session)) = newest_session(&d.path) else { continue };
        let mut standalone = 0;
        walk_files(&session, &mut |path, m| {
            standalone += m.allocated();
            if !seen.insert((m.dev(), m.ino())) {
                return;
            }
            if path.extension().is_some_and(|e| e == "o") {
                a.newest_objects += m.allocated();
            } else {
                a.newest_metadata += m.allocated();
            }
        });
        let name = d.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        a.newest_by_crate.push((name, standalone));
    }
    for d in &live_dirs {
        a.stale_sessions += charge(&d.path, &mut seen);
    }
    let stale: HashSet<PathBuf> = stale_objects(profile).into_iter().map(|s| s.0).collect();
    for sub in ["deps", "examples"] {
        let Ok(entries) = fs::read_dir(profile.join(sub)) else { continue };
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().is_none_or(|x| x != "o") {
                continue;
            }
            let Ok(m) = sys::lstat(&path) else { continue };
            a.deps_object_files += 1;
            if !seen.insert((m.dev(), m.ino())) {
                continue;
            }
            if stale.contains(&path) {
                a.deps_objects_stale += m.allocated();
            } else {
                a.deps_objects_current += m.allocated();
            }
        }
    }
    a.newest_by_crate.sort();
    a
}

struct CompactionLock(File);

impl std::ops::Deref for CompactionLock {
    type Target = File;

    fn deref(&self) -> &File {
        &self.0
    }
}

impl Drop for CompactionLock {
    fn drop(&mut self) {
        let _ = sys::unlock(&self.0);
    }
}

pub struct ProfileLock {
    _files: Vec<CompactionLock>,
}

fn profile_roots_safe(profile: &Path) -> bool {
    if !sys::lstat(profile).is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink()) {
        return false;
    }
    let Ok(children) = fs::read_dir(profile) else { return false };
    for child in children {
        let Ok(child) = child else { return false };
        let Ok(kind) = child.file_type() else { return false };
        let name = child.file_name();
        if CARGO_LOCKS.iter().any(|n| name == *n) && !kind.is_file() {
            return false;
        }
        if ["deps", "examples", ".fingerprint", "incremental", "build"].iter().any(|n| name == *n) && !kind.is_dir() {
            return false;
        }
    }
    true
}

impl ProfileLock {
    pub fn try_acquire(profile: &Path) -> Option<ProfileLock> {
        if !profile_roots_safe(profile) {
            return None;
        }
        let mut files = Vec::new();
        for name in CARGO_LOCKS {
            let file = File::options().read(true).write(true).create(true).truncate(false).open(profile.join(name)).ok()?;
            sys::try_lock(&file, sys::LockKind::Exclusive).ok()?;
            files.push(CompactionLock(file));
        }
        Some(ProfileLock { _files: files })
    }
}

/// Caller holds ProfileLock; lock files and the profile directory retain their identities for waiting Cargo processes.
pub fn remove_profile_payload(profile: &Path) -> io::Result<()> {
    remove_profile_payloads_with(&[profile.to_path_buf()], |_, _| {})
}

struct RetainedRootTimes(Vec<(File, SystemTime)>);

impl RetainedRootTimes {
    fn restore(&mut self) -> io::Result<()> {
        let mut result = Ok(());
        for (file, modified) in self.0.drain(..) {
            if let Err(error) = file.set_modified(modified) {
                result = Err(error);
            }
        }
        result
    }
}

impl Drop for RetainedRootTimes {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

fn retained_root_time(path: &Path) -> io::Result<(File, SystemTime)> {
    let file = sys::open_directory_for_times(path)?;
    let held = sys::fstat(&file)?;
    let named = sys::lstat(path)?;
    if !held.is_dir() || !named.is_dir() || (held.dev(), held.ino()) != (named.dev(), named.ino()) {
        return Err(incremental_retained());
    }
    Ok((file, held.modified()?))
}

/// Caller holds every ProfileLock; all incremental trees are admitted together before any profile payload is removed.
pub fn remove_profile_payloads_with(profiles: &[PathBuf], mut removed: impl FnMut(&Path, u64)) -> io::Result<()> {
    let mut payloads = Vec::new();
    let mut sets = Vec::new();
    let mut root_times = Vec::new();
    for profile in profiles {
        if !profile_roots_safe(profile) {
            return Err(incremental_retained());
        }
        root_times.push(retained_root_time(profile)?);
        for child in fs::read_dir(profile)? {
            let child = child?;
            if CARGO_LOCKS.iter().any(|name| child.file_name() == *name) {
                continue;
            }
            let path = child.path();
            if ["deps", ".fingerprint"].iter().any(|name| child.file_name() == *name) {
                root_times.push(retained_root_time(&path)?);
                let priority = if child.file_name() == "deps" { 4 } else { 3 };
                for entry in fs::read_dir(&path)? {
                    let entry = entry?;
                    payloads.push((priority, entry.path(), fsx::allocated_size(&entry.path())));
                }
                continue;
            }
            if child.file_name() == "incremental" {
                let mut crates = Vec::new();
                for entry in fs::read_dir(&path)? {
                    let entry = entry?;
                    if !entry.file_type()?.is_dir() {
                        return Err(incremental_retained());
                    }
                    crates.push(entry.path());
                }
                crates.sort();
                sets.push((path.clone(), crates));
            }
            let priority = if child.file_name().to_string_lossy().ends_with(".d") { 0 } else { 1 };
            payloads.push((priority, path.clone(), fsx::allocated_size(&path)));
        }
    }
    let mut guard = if sets.is_empty() {
        None
    } else {
        Some(sessions::try_retirement_sets(&sets).ok().flatten().ok_or_else(incremental_retained)?)
    };
    if guard.as_ref().is_some_and(|guard| !guard.revalidate().unwrap_or(false)) {
        return Err(incremental_retained());
    }
    // Root timestamps are restored before session guards drop, including on an interrupted callback.
    let mut root_times = RetainedRootTimes(root_times);
    payloads.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    for (_, path, bytes) in payloads {
        if sets.iter().any(|(root, _)| root == &path) {
            guard.as_mut().ok_or_else(incremental_retained)?.remove_root(&path)?;
        } else {
            fsx::remove_any(&path)?;
        }
        removed(&path, bytes);
    }
    root_times.restore()
}

fn incremental_retained() -> io::Error {
    io::Error::new(io::ErrorKind::WouldBlock, "incremental state is busy or uncertain")
}

/// Caller holds ProfileLock; the compiler-compatible guard stays held through the crate or finalized-session unlink.
pub fn remove_incremental_payload(incremental_root: &Path, path: &Path) -> io::Result<()> {
    if sys::lstat(path).is_err_and(|error| error.kind() == io::ErrorKind::NotFound) {
        return Ok(());
    }
    let mut guard = sessions::try_retirement_guard(incremental_root, path).ok().flatten().ok_or_else(incremental_retained)?;
    guard.remove(path)
}

pub fn profile_of(path: &Path) -> Option<PathBuf> {
    path.ancestors().skip(1).find(|p| p.join("deps").is_dir() && p.file_name().is_some_and(|n| n != "deps")).map(Path::to_path_buf)
}

#[derive(Debug, Clone)]
pub struct CompactOptions {
    pub remove_superseded: bool,
    pub compress: bool,
    pub dedupe: bool,
    /// The store is included so its entries and the target copies restored from them stay shared after compression.
    pub include_store: bool,
    pub dry_run: bool,
}

impl CompactOptions {
    pub fn full() -> CompactOptions {
        CompactOptions { remove_superseded: true, compress: true, dedupe: true, include_store: true, dry_run: false }
    }

    pub fn after_build() -> CompactOptions {
        CompactOptions { remove_superseded: false, ..CompactOptions::full() }
    }
}

#[derive(Debug, Default)]
pub struct Report {
    pub profiles: usize,
    pub busy: Vec<PathBuf>,
    pub unclassified: Vec<PathBuf>,
    pub dead_units: usize,
    pub removed: Vec<(PathBuf, u64)>,
    pub removed_bytes: u64,
    pub stale_sessions: usize,
    pub stale_working_sessions: usize,
    pub orphan_session_locks: usize,
    pub retained_session_states: usize,
    pub retained_incremental: usize,
    pub retained_cache_entries: usize,
    pub empty_incremental_dirs: usize,
    pub stale_objects: usize,
    pub dead_incremental: usize,
    pub idle_incremental: usize,
    pub superseded_entries: usize,
    pub compressed_files: u64,
    pub compress_before: u64,
    pub compress_after: u64,
    pub deduped_files: u64,
    pub dedupe_saved: u64,
    pub physical_before: u64,
    pub physical_after: u64,
    pub errors: Vec<(PathBuf, String)>,
    pub supported: bool,
    pub skipped: Option<&'static str>,
    pub demoted_profiles: Vec<PathBuf>,
    pub budget_evicted: u64,
    pub allocated_before: u64,
    pub allocated_after: u64,
    /// The pass ran out of its time or byte budget and left the rest for the next due pass.
    pub budget_stopped: bool,
}

pub fn central_targets(dirs: &Dirs) -> Vec<PathBuf> {
    stats::targets(dirs).into_iter().map(|t| t.path).collect()
}

pub fn compact(dirs: &Dirs, targets: &[PathBuf], opts: &CompactOptions) -> Report {
    let mut report = Report { supported: apfs::SUPPORTED, ..Report::default() };
    let store = dirs.store();
    let mut roots: Vec<PathBuf> = targets.to_vec();
    if opts.include_store && store.is_dir() {
        roots.push(store.clone());
    }
    let root_refs: Vec<&Path> = roots.iter().map(PathBuf::as_path).collect();
    report.physical_before = physical_size(&root_refs);

    // Locks are held for the whole pass because files move between target directories.
    let mut locks = Vec::new();
    let mut profiles = Vec::new();
    for target in targets {
        let ours = target.starts_with(dirs.targets());
        for profile in profile_dirs(target) {
            match ProfileLock::try_acquire(&profile) {
                Some(lock) => {
                    locks.push(lock);
                    profiles.push((profile, ours));
                }
                None => report.busy.push(profile),
            }
        }
    }
    report.profiles = profiles.len();

    if opts.remove_superseded {
        for (profile, ours) in &profiles {
            if !ours {
                continue;
            }
            let mut doomed = Vec::new();
            let analysis = analyze(profile);
            match &analysis {
                Some(a) => {
                    report.dead_units += a.dead.len();
                    doomed.extend(a.removals.iter().cloned());
                }
                None => report.unclassified.push(profile.clone()),
            }
            if let Some(a) = &analysis {
                for d in incremental_dirs(profile, a) {
                    if matches!(d.owner, IncrementalOwner::Dead | IncrementalOwner::Orphan) && !doomed.iter().any(|(p, _)| d.path.starts_with(p)) {
                        report.dead_incremental += 1;
                        let bytes = fsx::allocated_size(&d.path);
                        doomed.push((d.path, bytes));
                    }
                }
            }
            let sessions: Vec<(PathBuf, u64)> =
                stale_sessions(profile).into_iter().filter(|(s, _)| !doomed.iter().any(|(p, _)| s.starts_with(p))).collect();
            report.stale_sessions += sessions.len();
            doomed.extend(sessions);
            remove_or_plan(doomed, opts.dry_run, &mut report);
            // After the sessions are gone, the aliases only they shared are single links.
            let objects = stale_objects(profile);
            report.stale_objects += objects.len();
            remove_or_plan(objects, opts.dry_run, &mut report);
        }
    }

    if apfs::SUPPORTED && (opts.compress || opts.dedupe) {
        let mut candidates = Vec::new();
        for (profile, _) in &profiles {
            collect_candidates(profile, false, &mut candidates);
        }
        if opts.include_store && store.is_dir() {
            collect_candidates(&store, true, &mut candidates);
        }
        shrink(candidates, opts, &mut ShrinkCtx::plain(), &mut report);
    }

    drop(locks);
    report.physical_after = if opts.dry_run {
        report
            .physical_before
            .saturating_sub(report.removed_bytes + report.compress_before.saturating_sub(report.compress_after) + report.dedupe_saved)
    } else {
        physical_size(&root_refs)
    };
    report
}

#[derive(Debug)]
struct Candidate {
    path: PathBuf,
    meta: sys::Meta,
    in_store: bool,
}

/// Store `meta.json` files carry the LRU time and are never touched.
fn collect_candidates(root: &Path, in_store: bool, out: &mut Vec<Candidate>) {
    let skip = |name: &str, is_dir: bool| (name.starts_with('.') && name != ".fingerprint") || (is_dir && name.ends_with("-working"));
    let found = |acc: &mut Vec<Candidate>, path: &Path, meta: &sys::Meta| {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if meta.nlink() != 1
            || meta.len() < MIN_FILE_BYTES
            || meta.len() > MAX_FILE_BYTES
            || !apfs::only_compression_flag(meta)
            || (in_store && name == "meta.json")
        {
            return;
        }
        acc.push(Candidate { path: path.to_path_buf(), meta: meta.clone(), in_store });
    };
    let mut all: Vec<Candidate> = walk_parallel(root, &skip, &Vec::<Candidate>::new, &found).into_iter().flatten().collect();
    all.sort_by(|a, b| a.path.cmp(&b.path));
    out.extend(all);
}

/// What a pass may spend before stopping between two files; the memo makes finished files free on the next pass.
struct Budget {
    deadline: Instant,
    bytes_left: u64,
}

/// State shared by the files of one shrink pass: the optional work budget, the persistent memo and the sampler switch.
struct ShrinkCtx {
    budget: Option<Budget>,
    memo: Option<Memo>,
    memo_path: Option<PathBuf>,
    sample: bool,
    stopped: bool,
}

impl ShrinkCtx {
    /// No budget, no memo and no sampling: the explicit `frust compact` behaviour.
    fn plain() -> ShrinkCtx {
        ShrinkCtx { budget: None, memo: None, memo_path: None, sample: false, stopped: false }
    }

    fn for_auto(dirs: &Dirs, target: &Path, opts: &AutoOptions) -> ShrinkCtx {
        let path = dirs.compact_memo(target);
        ShrinkCtx {
            budget: Some(Budget { deadline: Instant::now() + opts.pass_budget, bytes_left: opts.pass_bytes }),
            memo: Some(Memo::load(&path)),
            memo_path: Some(path),
            sample: true,
            stopped: false,
        }
    }

    fn exhausted(&mut self) -> bool {
        let spent = self.budget.as_ref().is_some_and(|b| b.bytes_left == 0 || Instant::now() >= b.deadline);
        self.stopped |= spent;
        spent
    }

    fn spend(&mut self, bytes: u64) {
        if let Some(b) = &mut self.budget {
            b.bytes_left = b.bytes_left.saturating_sub(bytes);
        }
    }

    fn recall_hash(&mut self, meta: &sys::Meta) -> Option<[u8; 32]> {
        self.memo.as_mut()?.get(&FileKey::of(meta))?.hash
    }

    fn remember_hash(&mut self, path: &Path, meta: &sys::Meta, digest: [u8; 32]) {
        if unchanged(path, meta)
            && let Some(memo) = &mut self.memo
        {
            memo.update(FileKey::of(meta), |v| v.hash = Some(digest));
        }
    }

    fn known_incompressible(&mut self, meta: &sys::Meta) -> bool {
        self.memo.as_mut().and_then(|m| m.get(&FileKey::of(meta))).is_some_and(|v| v.incompressible.is_some())
    }

    fn note_incompressible(&mut self, path: &Path, meta: &sys::Meta, percent: u8) {
        if unchanged(path, meta)
            && let Some(memo) = &mut self.memo
        {
            memo.update(FileKey::of(meta), |v| v.incompressible = Some(percent));
        }
    }

    fn forget(&mut self, meta: &sys::Meta) {
        if let Some(memo) = &mut self.memo {
            memo.forget(&FileKey::of(meta));
        }
    }

    /// A pass that ran to the end also drops the entries of files it no longer found.
    fn finish(mut self) {
        if let (Some(memo), Some(path)) = (&mut self.memo, &self.memo_path) {
            memo.save(path, !self.stopped);
        }
    }
}

/// The identity a memo entry is valid for; any write to the file changes its ctime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct FileKey {
    dev: u64,
    ino: u64,
    size: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

impl FileKey {
    fn of(meta: &sys::Meta) -> FileKey {
        FileKey { dev: meta.dev(), ino: meta.ino(), size: meta.len(), mtime: meta.mtime(), ctime: meta.ctime() }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct MemoValue {
    hash: Option<[u8; 32]>,
    /// Percent of the input an encoder kept when the file was judged not worth compressing.
    incompressible: Option<u8>,
}

const MEMO_MAGIC: &[u8; 4] = b"FRCM";
const MEMO_VERSION: u32 = 1;
const MEMO_HEADER: usize = 12;
const MEMO_RECORD: usize = 90;
const MEMO_SUM: usize = 16;
const MAX_MEMO_ENTRIES: usize = 40_000;

/// A checksummed, bounded file per target; anything unreadable or damaged loads as empty, so it can only cost a recompute.
#[derive(Debug, Default)]
struct Memo {
    entries: BTreeMap<FileKey, MemoValue>,
    touched: HashSet<FileKey>,
    dirty: bool,
}

impl Memo {
    fn load(path: &Path) -> Memo {
        fs::read(path).ok().and_then(|bytes| Memo::decode(&bytes)).unwrap_or_default()
    }

    fn get(&mut self, key: &FileKey) -> Option<MemoValue> {
        let value = self.entries.get(key).copied()?;
        self.touched.insert(*key);
        Some(value)
    }

    fn update(&mut self, key: FileKey, change: impl FnOnce(&mut MemoValue)) {
        let value = self.entries.entry(key).or_default();
        let before = *value;
        change(value);
        self.dirty |= *value != before;
        self.touched.insert(key);
    }

    fn forget(&mut self, key: &FileKey) {
        self.dirty |= self.entries.remove(key).is_some();
    }

    fn save(&mut self, path: &Path, prune: bool) {
        if prune {
            let touched = &self.touched;
            let before = self.entries.len();
            self.entries.retain(|k, _| touched.contains(k));
            self.dirty |= self.entries.len() != before;
        }
        if self.entries.len() > MAX_MEMO_ENTRIES {
            let overflow = self.entries.len() - MAX_MEMO_ENTRIES;
            let untouched: Vec<FileKey> = self.entries.keys().filter(|k| !self.touched.contains(*k)).take(overflow).copied().collect();
            for key in untouched {
                self.entries.remove(&key);
            }
            while self.entries.len() > MAX_MEMO_ENTRIES {
                self.entries.pop_last();
            }
            self.dirty = true;
        }
        if !self.dirty {
            return;
        }
        if self.entries.is_empty() {
            let _ = fs::remove_file(path);
        } else {
            let _ = fsx::write_atomic(path, &self.encode());
        }
        self.dirty = false;
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(MEMO_HEADER + self.entries.len() * MEMO_RECORD + MEMO_SUM);
        out.extend_from_slice(MEMO_MAGIC);
        out.extend_from_slice(&MEMO_VERSION.to_le_bytes());
        out.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());
        for (k, v) in &self.entries {
            for n in [k.dev, k.ino, k.size] {
                out.extend_from_slice(&n.to_le_bytes());
            }
            for n in [k.mtime.0, k.mtime.1, k.ctime.0, k.ctime.1] {
                out.extend_from_slice(&n.to_le_bytes());
            }
            out.push(u8::from(v.hash.is_some()) | (u8::from(v.incompressible.is_some()) << 1));
            out.push(v.incompressible.unwrap_or(0));
            out.extend_from_slice(&v.hash.unwrap_or([0; 32]));
        }
        let sum = blake3::hash(&out);
        out.extend_from_slice(&sum.as_bytes()[..MEMO_SUM]);
        out
    }

    fn decode(bytes: &[u8]) -> Option<Memo> {
        if bytes.len() < MEMO_HEADER + MEMO_SUM {
            return None;
        }
        let (body, sum) = bytes.split_at(bytes.len() - MEMO_SUM);
        if body[..4] != MEMO_MAGIC[..] || blake3::hash(body).as_bytes()[..MEMO_SUM] != *sum {
            return None;
        }
        let word = |at: usize| {
            let mut b = [0u8; 4];
            b.copy_from_slice(&body[at..at + 4]);
            u32::from_le_bytes(b)
        };
        let count = word(8) as usize;
        if word(4) != MEMO_VERSION || body.len() != MEMO_HEADER + count.checked_mul(MEMO_RECORD)? {
            return None;
        }
        let mut entries = BTreeMap::new();
        for rec in body[MEMO_HEADER..].as_chunks::<MEMO_RECORD>().0 {
            let eight = |at: usize| {
                let mut b = [0u8; 8];
                b.copy_from_slice(&rec[at..at + 8]);
                b
            };
            let key = FileKey {
                dev: u64::from_le_bytes(eight(0)),
                ino: u64::from_le_bytes(eight(8)),
                size: u64::from_le_bytes(eight(16)),
                mtime: (i64::from_le_bytes(eight(24)), i64::from_le_bytes(eight(32))),
                ctime: (i64::from_le_bytes(eight(40)), i64::from_le_bytes(eight(48))),
            };
            let mut digest = [0u8; 32];
            digest.copy_from_slice(&rec[58..90]);
            let value = MemoValue { hash: (rec[56] & 1 != 0).then_some(digest), incompressible: (rec[56] & 2 != 0).then_some(rec[57]) };
            entries.insert(key, value);
        }
        Some(Memo { entries, touched: HashSet::new(), dirty: false })
    }
}

fn shrink(candidates: Vec<Candidate>, opts: &CompactOptions, ctx: &mut ShrinkCtx, report: &mut Report) {
    let mut by_size: BTreeMap<u64, Vec<Candidate>> = BTreeMap::new();
    for c in candidates {
        by_size.entry(c.meta.len()).or_default().push(c);
    }
    let mut dir_times = DirTimes::default();
    // Largest files first, so a pass cut short by its budget has done the most valuable work.
    'buckets: for (_, mut bucket) in by_size.into_iter().rev() {
        if ctx.exhausted() {
            break;
        }
        bucket.sort_by(|a, b| a.path.cmp(&b.path));
        if bucket.len() == 1 || !opts.dedupe {
            for c in bucket {
                if ctx.exhausted() {
                    break 'buckets;
                }
                if opts.compress && !apfs::is_compressed(&c.meta) {
                    compress_one(&c, opts.dry_run, &mut dir_times, ctx, report);
                }
            }
            continue;
        }
        // One clone group of compressed files means an earlier pass already handled them.
        let clone_ids: HashSet<Option<u64>> = bucket.iter().map(|c| apfs::clone_info(&c.path).map(|i| i.0)).collect();
        if clone_ids.len() == 1 && !clone_ids.contains(&None) && bucket.iter().all(|c| apfs::is_compressed(&c.meta)) {
            continue;
        }
        // The flag marks a digest that came from the memo rather than from reading the file just now.
        let mut groups: BTreeMap<[u8; 32], Vec<(Candidate, bool)>> = BTreeMap::new();
        for c in bucket {
            if let Some(digest) = ctx.recall_hash(&c.meta) {
                groups.entry(digest).or_default().push((c, true));
                continue;
            }
            if ctx.exhausted() {
                break 'buckets;
            }
            match content_digest(&c.path) {
                Ok(digest) => {
                    ctx.spend(c.meta.len());
                    ctx.remember_hash(&c.path, &c.meta, digest);
                    groups.entry(digest).or_default().push((c, false));
                }
                Err(err) => report.errors.push((c.path.clone(), err.to_string())),
            }
        }
        for (_, mut group) in groups {
            if ctx.exhausted() {
                break 'buckets;
            }
            // Prefer a compressed canonical copy, then one in the store, whose target copies are usually clones of it already.
            group.sort_by_key(|(c, _)| (!apfs::is_compressed(&c.meta), !c.in_store, c.path.clone()));
            let mut members = group.into_iter();
            let Some((canonical, canonical_memoised)) = members.next() else { continue };
            if opts.compress && !apfs::is_compressed(&canonical.meta) {
                compress_one(&canonical, opts.dry_run, &mut dir_times, ctx, report);
            }
            let canon_meta = sys::lstat(&canonical.path).ok();
            let canon_clone = apfs::clone_info(&canonical.path).map(|i| i.0);
            for (m, memoised) in members {
                if canon_clone.is_some() && apfs::clone_info(&m.path).map(|i| i.0) == canon_clone {
                    continue;
                }
                let saved = m.meta.allocated();
                if opts.dry_run {
                    report.deduped_files += 1;
                    report.dedupe_saved += saved;
                    continue;
                }
                if ctx.exhausted() {
                    break 'buckets;
                }
                let Some(cm) = canon_meta.as_ref() else { continue };
                // A remembered digest is only a hint: the bytes are compared before anything is replaced.
                if (canonical_memoised || memoised) && !same_bytes(&canonical.path, &m.path).unwrap_or(false) {
                    ctx.forget(&canonical.meta);
                    ctx.forget(&m.meta);
                    continue;
                }
                if canonical_memoised || memoised {
                    ctx.spend(m.meta.len());
                }
                match replace_with_clone(&canonical.path, cm, &m, &mut dir_times) {
                    Ok(()) => {
                        report.deduped_files += 1;
                        report.dedupe_saved += saved;
                    }
                    Err(err) => report.errors.push((m.path.clone(), err.to_string())),
                }
            }
        }
    }
}

fn content_digest(path: &Path) -> io::Result<[u8; 32]> {
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(*hasher.finalize().as_bytes())
}

fn content_hash(path: &Path) -> io::Result<String> {
    Ok(blake3::Hash::from_bytes(content_digest(path)?).to_hex().to_string())
}

fn same_bytes(a: &Path, b: &Path) -> io::Result<bool> {
    let (mut fa, mut fb) = (File::open(a)?, File::open(b)?);
    if fa.metadata()?.len() != fb.metadata()?.len() {
        return Ok(false);
    }
    let (mut x, mut y) = (vec![0u8; 256 * 1024], vec![0u8; 256 * 1024]);
    loop {
        let n = fa.read(&mut x)?;
        if n == 0 {
            return Ok(fb.read(&mut y)? == 0);
        }
        fb.read_exact(&mut y[..n])?;
        if x[..n] != y[..n] {
            return Ok(false);
        }
    }
}

const SAMPLE_BYTES: u64 = 64 * 1024;
/// The compression check below keeps a file only if it saves at least 5 percent.
const SAMPLE_KEEP_PERCENT: u8 = 95;

/// The head and the middle of a file big enough that sampling is cheaper than encoding all of it.
fn sample_ranges(len: u64) -> Vec<(u64, u64)> {
    if len < 4 * SAMPLE_BYTES {
        return Vec::new();
    }
    vec![(0, SAMPLE_BYTES), ((len - SAMPLE_BYTES) / 2, SAMPLE_BYTES)]
}

/// Percent of the sampled bytes the encoder keeps; `None` when the file is not sampled or cannot be read.
fn sample_ratio(path: &Path, len: u64, encode: &dyn Fn(&[u8]) -> Option<usize>) -> Option<u8> {
    let ranges = sample_ranges(len);
    if ranges.is_empty() {
        return None;
    }
    let file = File::open(path).ok()?;
    let (mut raw, mut kept) = (0u64, 0u64);
    for (offset, n) in ranges {
        let mut buf = vec![0u8; n as usize];
        sys::read_exact_at(&file, &mut buf, offset).ok()?;
        kept += encode(&buf)? as u64;
        raw += n;
    }
    Some((kept * 100 / raw).min(100) as u8)
}

/// Directory times are restored after each rename so the rewrite is invisible to tools comparing directory mtimes.
#[derive(Default)]
struct DirTimes(HashMap<PathBuf, (i64, i64, i64, i64)>);

impl DirTimes {
    fn capture(&mut self, file: &Path) -> Option<PathBuf> {
        let dir = file.parent()?.to_path_buf();
        if !self.0.contains_key(&dir) {
            let m = sys::lstat(&dir).ok()?;
            let (atime, mtime) = (m.atime(), m.mtime());
            self.0.insert(dir.clone(), (atime.0, atime.1, mtime.0, mtime.1));
        }
        Some(dir)
    }

    fn restore(&self, dir: &Path) {
        if let Some(&(a, an, m, mn)) = self.0.get(dir) {
            let _ = apfs::set_times(dir, (a, an), (m, mn));
        }
    }
}

/// Guards against replacing a file a build rewrote after it was examined.
fn unchanged(path: &Path, meta: &sys::Meta) -> bool {
    sys::lstat(path).is_ok_and(|now| now.ino() == meta.ino() && now.len() == meta.len() && now.mtime() == meta.mtime())
}

fn compress_one(c: &Candidate, dry_run: bool, dir_times: &mut DirTimes, ctx: &mut ShrinkCtx, report: &mut Report) {
    if ctx.known_incompressible(&c.meta) {
        return;
    }
    if ctx.sample
        && let Some(kept) = sample_ratio(&c.path, c.meta.len(), &|b| apfs::encode_lzfse_fork(b).map(|f| f.len()))
    {
        ctx.spend(2 * SAMPLE_BYTES);
        if kept > SAMPLE_KEEP_PERCENT {
            ctx.note_incompressible(&c.path, &c.meta, kept);
            return;
        }
    }
    let data = match fs::read(&c.path) {
        Ok(d) => d,
        Err(err) => {
            report.errors.push((c.path.clone(), err.to_string()));
            return;
        }
    };
    ctx.spend(data.len() as u64);
    if data.len() as u64 != c.meta.len() {
        return;
    }
    let Some(fork) = apfs::encode_lzfse_fork(&data) else { return };
    let before = c.meta.allocated();
    let after = (fork.len() as u64).div_ceil(4096) * 4096;
    if after + 8192 > before || after > before - before / 20 {
        ctx.note_incompressible(&c.path, &c.meta, (fork.len() as u64 * 100 / (data.len() as u64).max(1)).min(100) as u8);
        return;
    }
    if dry_run {
        report.compressed_files += 1;
        report.compress_before += before;
        report.compress_after += after;
        return;
    }
    let tmp = fsx::temp_sibling(&c.path);
    let result = (|| -> io::Result<u64> {
        apfs::write_compressed(&tmp, &c.path, &c.meta, &data, &fork)?;
        verify_same(&tmp, &data)?;
        apfs::set_times(&tmp, c.meta.atime(), c.meta.mtime())?;
        let written = sys::lstat(&tmp)?;
        if !apfs::is_compressed(&written) {
            return Err(io::Error::other("kernel did not keep the file compressed"));
        }
        if !unchanged(&c.path, &c.meta) {
            return Err(io::Error::other("file changed while compressing"));
        }
        let dir = dir_times.capture(&c.path);
        sys::rename(&tmp, &c.path)?;
        if let Some(dir) = dir {
            dir_times.restore(&dir);
        }
        Ok(written.allocated())
    })();
    match result {
        Ok(after) => {
            report.compressed_files += 1;
            report.compress_before += before;
            report.compress_after += after;
        }
        Err(err) => {
            let _ = fsx::remove_any(&tmp);
            report.errors.push((c.path.clone(), err.to_string()));
        }
    }
}

pub fn compress_file(path: &Path) -> io::Result<bool> {
    let meta = sys::lstat(path)?;
    if !apfs::SUPPORTED || !meta.is_file() || meta.nlink() != 1 || apfs::is_compressed(&meta) {
        return Ok(false);
    }
    let mut report = Report::default();
    let candidate = Candidate { path: path.to_path_buf(), meta, in_store: false };
    compress_one(&candidate, false, &mut DirTimes::default(), &mut ShrinkCtx::plain(), &mut report);
    match report.errors.pop() {
        Some((_, err)) => Err(io::Error::other(err)),
        None => Ok(report.compressed_files == 1),
    }
}

pub fn is_compressed(meta: &fs::Metadata) -> bool {
    apfs::is_compressed(meta)
}

/// Control arm of the latency measurements: changes the inode the way compression does, without compressing.
pub fn rewrite_plain(path: &Path) -> io::Result<()> {
    use std::io::Write as _;
    let meta = sys::lstat(path)?;
    let data = fs::read(path)?;
    let tmp = fsx::temp_sibling(path);
    let mut dir_times = DirTimes::default();
    let result = (|| -> io::Result<()> {
        let mut file = sys::with_mode(File::options().write(true).create_new(true), meta.mode() & 0o7777).open(&tmp)?;
        file.write_all(&data)?;
        if !apfs::SUPPORTED {
            file.set_modified(meta.modified()?)?;
        }
        drop(file);
        if apfs::SUPPORTED {
            apfs::set_times(&tmp, meta.atime(), meta.mtime())?;
        }
        let dir = dir_times.capture(path);
        sys::rename(&tmp, path)?;
        if let Some(dir) = dir {
            dir_times.restore(&dir);
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fsx::remove_any(&tmp);
    }
    result
}

fn verify_same(path: &Path, expected: &[u8]) -> io::Result<()> {
    let mut file = File::open(path)?;
    let mut buf = vec![0u8; 256 * 1024];
    let mut at = 0usize;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        if at + n > expected.len() || buf[..n] != expected[at..at + n] {
            return Err(io::Error::other("compressed copy reads back differently; left uncompressed"));
        }
        at += n;
    }
    if at != expected.len() {
        return Err(io::Error::other("compressed copy reads back short; left uncompressed"));
    }
    Ok(())
}

fn replace_with_clone(canonical: &Path, canon_meta: &sys::Meta, m: &Candidate, dir_times: &mut DirTimes) -> io::Result<()> {
    if !unchanged(canonical, canon_meta) {
        return Err(io::Error::other("canonical copy changed"));
    }
    let tmp = fsx::temp_sibling(&m.path);
    let result = (|| -> io::Result<()> {
        apfs::clone(canonical, &tmp)?;
        fs::set_permissions(&tmp, m.meta.permissions())?;
        apfs::set_times(&tmp, m.meta.atime(), m.meta.mtime())?;
        if fs::symlink_metadata(&tmp)?.len() != m.meta.len() || !unchanged(&m.path, &m.meta) {
            return Err(io::Error::other("file changed while deduplicating"));
        }
        let dir = dir_times.capture(&m.path);
        sys::rename(&tmp, &m.path)?;
        if let Some(dir) = dir {
            dir_times.restore(&dir);
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fsx::remove_any(&tmp);
    }
    result
}

pub type CompactReport = Report;

/// The marker's mtime is the last time automatic compaction considered the directory.
pub const AUTO_MARKER: &str = ".frust-compact";

/// A new inode for a proc-macro dylib cost the next edit about 110 ms because macOS rechecks the code signature.
pub const HOT_CATEGORIES: [Category; 3] = [Category::Dylib, Category::DepsExecutables, Category::Examples];

#[derive(Debug, Clone)]
pub struct AutoOptions {
    pub enabled: bool,
    pub every: Duration,
    pub min_growth: u64,
    pub keep_other_profile: Duration,
    /// Kept so switching a feature or flag back and forth stays warm.
    pub uncached_dead_grace: Duration,
    /// Incremental directories of dead variants only matter if the variant returns and is edited, yet are the largest part of a path crate.
    pub dead_incremental_grace: Duration,
    pub incremental_idle: Duration,
    pub max_target_bytes: u64,
    /// Alternating debug and release builds keeps both profiles warm.
    pub current_window: Duration,
    /// Either rewrite gives the file a new inode, and macOS rechecks the code signature of a new proc-macro dylib.
    pub hot_window: Duration,
    pub hot_categories: Vec<Category>,
    pub cache_enabled: bool,
    pub compress: bool,
    /// Wall time the dedupe and compression stage of one automatic pass may take before it stops between files.
    pub pass_budget: Duration,
    /// Bytes that stage may read or encode in one pass.
    pub pass_bytes: u64,
}

impl AutoOptions {
    pub fn from_env(cfg: &Config) -> AutoOptions {
        let var = |k: &str| std::env::var(k).ok().map(|v| v.trim().to_ascii_lowercase());
        let enabled = !matches!(var("FRUST_AUTO_COMPACT").as_deref(), Some("off" | "0" | "false" | "no"));
        let days = var("FRUST_COMPACT_KEEP_DAYS").and_then(|v| v.parse::<f64>().ok()).filter(|d| *d >= 0.0).unwrap_or(14.0);
        let gib = var("FRUST_TARGET_MAX_GIB").and_then(|v| v.parse::<f64>().ok()).filter(|g| *g > 0.0).unwrap_or(8.0);
        let idle_days =
            var("FRUST_COMPACT_INCREMENTAL_IDLE_DAYS").and_then(|v| v.parse::<f64>().ok()).filter(|d| *d >= 0.0).unwrap_or(7.0);
        AutoOptions {
            enabled,
            every: Duration::from_secs(10 * 60),
            min_growth: 64 * 1024 * 1024,
            keep_other_profile: Duration::from_secs_f64(days * 86_400.0),
            uncached_dead_grace: Duration::from_secs(86_400),
            dead_incremental_grace: Duration::from_secs(3600),
            incremental_idle: Duration::from_secs_f64(idle_days * 86_400.0),
            max_target_bytes: (gib * 1024.0 * 1024.0 * 1024.0) as u64,
            current_window: Duration::from_secs(3600),
            hot_window: Duration::from_secs(3600),
            hot_categories: HOT_CATEGORIES.to_vec(),
            cache_enabled: cfg.cache,
            compress: true,
            pass_budget: Duration::from_secs(2),
            pass_bytes: 512 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct AutoMarker {
    allocated_after: u64,
    /// Time of the last completed pass; skips and incomplete retries preserve it.
    #[serde(default)]
    at_unix: u64,
    #[serde(default)]
    store_entries: u64,
    /// Zero when unknown; otherwise the change signature of the state this marker describes.
    #[serde(default)]
    signature: u64,
    /// False after a partial or failed pass, so the next due pass retries instead of being skipped.
    #[serde(default = "complete_by_default")]
    complete: bool,
}

impl AutoMarker {
    fn age_reassessment_due(&self, now: u64) -> bool {
        self.at_unix == 0 || now.checked_sub(self.at_unix).is_none_or(|age| age >= AUTO_REASSESS_EVERY.as_secs())
    }

    fn interval(&self, opts: &AutoOptions) -> Duration {
        if self.complete { opts.every } else { opts.every.min(AUTO_RETRY_EVERY) }
    }
}

const AUTO_REASSESS_EVERY: Duration = Duration::from_secs(3600);
const AUTO_RETRY_EVERY: Duration = Duration::from_secs(30);

fn complete_by_default() -> bool {
    true
}

fn auto_pass_complete(report: &Report) -> bool {
    !report.budget_stopped && report.busy.is_empty() && report.errors.is_empty() && report.retained_incremental == 0 && report.retained_cache_entries == 0
}

const STORE_ENTRIES_GROWTH: u64 = 32;

fn store_entries(dirs: &Dirs) -> u64 {
    let Ok(shards) = fs::read_dir(dirs.store()) else { return 0 };
    shards.flatten().map(|s| fs::read_dir(s.path()).map(|rd| rd.count() as u64).unwrap_or(0)).sum()
}

fn fold_hash(hasher: &blake3::Hasher) -> u64 {
    let digest = hasher.finalize();
    let mut word = [0u8; 8];
    word.copy_from_slice(&digest.as_bytes()[..8]);
    u64::from_le_bytes(word).max(1)
}

/// Mixes a directory's mtime and the names it lists (and, if `deep`, its children's mtimes); `None` on any error but an absent optional directory.
fn stamp_dir(hasher: &mut blake3::Hasher, dir: &Path, optional: bool, deep: bool) -> Option<()> {
    hasher.update(dir.as_os_str().as_encoded_bytes());
    let meta = match sys::stat(dir) {
        Ok(meta) => meta,
        Err(err) if optional && err.kind() == io::ErrorKind::NotFound => {
            hasher.update(b"-");
            return Some(());
        }
        Err(_) => return None,
    };
    let (secs, nanos) = meta.mtime();
    hasher.update(&secs.to_le_bytes());
    hasher.update(&nanos.to_le_bytes());
    let mut listed = Vec::new();
    for entry in fs::read_dir(dir).ok()? {
        let entry = entry.ok()?;
        let stamp = if deep { Some(sys::lstat(&entry.path()).ok()?.mtime()) } else { None };
        listed.push((entry.file_name(), stamp));
    }
    listed.sort();
    for (name, stamp) in listed {
        hasher.update(name.as_encoded_bytes());
        hasher.update(b"\0");
        if let Some((secs, nanos)) = stamp {
            hasher.update(&secs.to_le_bytes());
            hasher.update(&nanos.to_le_bytes());
        }
    }
    Some(())
}

/// Covers the few directories that change when a build writes into a profile; builds always add or rename entries there.
fn tree_signature(target: &Path) -> Option<u64> {
    let mut hasher = blake3::Hasher::new();
    stamp_dir(&mut hasher, target, false, false)?;
    for profile in profile_dirs(target) {
        stamp_dir(&mut hasher, &profile, false, false)?;
        for (sub, optional, deep) in [("deps", false, false), (".fingerprint", true, false), ("incremental", true, true), ("build", true, false), ("examples", true, false)] {
            stamp_dir(&mut hasher, &profile.join(sub), optional, deep)?;
        }
    }
    Some(fold_hash(&hasher))
}

/// `options` is part of the signature so a pass with different settings is never mistaken for a repeat.
fn change_signature(dirs: &Dirs, target: &Path, options: &str) -> Option<u64> {
    let tree = tree_signature(target)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(&tree.to_le_bytes());
    hasher.update(&store_entries(dirs).to_le_bytes());
    hasher.update(options.as_bytes());
    Some(fold_hash(&hasher))
}

fn write_marker(file: &File, record: &AutoMarker) {
    if let Ok(bytes) = serde_json::to_vec(record) {
        let _ = file.set_len(0);
        let _ = sys::write_all_at(file, &bytes, 0);
    }
}

/// The caller must hold frust's build lock; cargo's profile locks are also taken without blocking so a plain cargo build is never disturbed.
pub fn auto_compact(dirs: &Dirs, target_dir: &Path, cfg: &Config) -> CompactReport {
    auto_compact_with(dirs, target_dir, &AutoOptions::from_env(cfg))
}

pub fn auto_compact_with(dirs: &Dirs, target: &Path, opts: &AutoOptions) -> CompactReport {
    let mut report = Report { supported: apfs::SUPPORTED, ..Report::default() };
    if !opts.enabled {
        report.skipped = Some("disabled by FRUST_AUTO_COMPACT");
        return report;
    }
    if !target.starts_with(dirs.targets()) {
        report.skipped = Some("not a frust central target directory");
        return report;
    }
    let marker = target.join(AUTO_MARKER);
    if let Ok(meta) = fs::metadata(&marker)
        && marker_age(&meta).is_some_and(|a| a < opts.every)
    {
        if marker_age(&meta).is_some_and(|age| age < opts.every.min(AUTO_RETRY_EVERY)) {
            report.skipped = Some("not due");
            return report;
        }
        let previous = fs::read(&marker).ok().and_then(|b| serde_json::from_slice::<AutoMarker>(&b).ok());
        if previous.as_ref().is_some_and(|p| marker_age(&meta).is_some_and(|age| age < p.interval(opts))) {
            report.skipped = Some("not due");
            return report;
        }
    }
    let central = dirs.targets();
    if target.ancestors().take_while(|p| p.starts_with(&central))
        .any(|p| !sys::lstat(p).is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink()))
    {
        report.skipped = Some("unsafe central target path");
        return report;
    }
    at_background_priority(|| auto_compact_pass(dirs, target, opts, report))
}

fn marker_age(m: &fs::Metadata) -> Option<Duration> {
    m.modified().ok().and_then(|t| SystemTime::now().duration_since(t).ok())
}

fn auto_compact_pass(dirs: &Dirs, target: &Path, opts: &AutoOptions, mut report: Report) -> Report {
    let marker = target.join(AUTO_MARKER);
    let Ok(file) = File::options().read(true).write(true).create(true).truncate(false).open(&marker) else {
        report.skipped = Some("cannot open the marker");
        return report;
    };
    if sys::try_lock(&file, sys::LockKind::Exclusive).is_err() {
        report.skipped = Some("another compaction is running");
        return report;
    }
    let file = CompactionLock(file);
    let previous: Option<AutoMarker> = fs::read(&marker).ok().and_then(|b| serde_json::from_slice(&b).ok());
    // Another process may have finished a pass between the stat and the lock.
    if previous.as_ref().is_some_and(|p| file.metadata().ok().and_then(|m| marker_age(&m)).is_some_and(|a| a < p.interval(opts))) {
        report.skipped = Some("not due");
        return report;
    }
    let options_key = format!("{opts:?}");
    let signature = change_signature(dirs, target, &options_key);
    let reassess_age = previous.as_ref().is_none_or(|prev| prev.age_reassessment_due(fsx::unix_now()));
    // Mirrors the growth check below without walking the target: an unchanged tree cannot have grown.
    if let (Some(prev), Some(sig)) = (&previous, signature)
        && opts.min_growth > 0
        && prev.complete
        && !reassess_age
        && prev.signature == sig
        && prev.allocated_after <= opts.max_target_bytes
    {
        let record = AutoMarker { allocated_after: prev.allocated_after, at_unix: prev.at_unix, store_entries: prev.store_entries, signature: sig, complete: true };
        write_marker(&file, &record);
        let _ = file.set_modified(SystemTime::now());
        report.skipped = Some("nothing changed since the last pass");
        report.allocated_before = prev.allocated_after;
        report.allocated_after = prev.allocated_after;
        return report;
    }
    report.allocated_before = allocated(target);
    if let Some(prev) = &previous
        && prev.complete
        && !reassess_age
        && report.allocated_before < prev.allocated_after.saturating_add(opts.min_growth)
        && report.allocated_before <= opts.max_target_bytes
    {
        // Every path-crate edit still adds cache entries even when the target is bounded, so prune once enough appear.
        let entries = store_entries(dirs);
        let mut record = AutoMarker { allocated_after: prev.allocated_after, at_unix: prev.at_unix, store_entries: prev.store_entries, signature: 0, complete: true };
        if entries >= prev.store_entries.saturating_add(STORE_ENTRIES_GROWTH) {
            retire_superseded_cache(dirs, &mut report);
            record.store_entries = store_entries(dirs);
        }
        record.signature = change_signature(dirs, target, &options_key).unwrap_or(0);
        record.complete = auto_pass_complete(&report);
        write_marker(&file, &record);
        let _ = file.set_modified(SystemTime::now());
        report.skipped = Some("grew too little since the last pass");
        report.allocated_after = report.allocated_before;
        return report;
    }

    retire_superseded_cache(dirs, &mut report);
    let mut locks = Vec::new();
    let mut profiles = Vec::new();
    for profile in profile_dirs(target) {
        match ProfileLock::try_acquire(&profile) {
            Some(lock) => {
                locks.push(lock);
                profiles.push(profile);
            }
            None => report.busy.push(profile),
        }
    }
    report.profiles = profiles.len();
    let store = OnceCell::new();
    let store_index = || store.get_or_init(|| StoreIndex::new(&dirs.store()).with_budget(opts.pass_bytes, opts.pass_budget));
    let covered = |paths: &[(PathBuf, u64)]| opts.cache_enabled && store_index().covers(paths);
    let now = SystemTime::now();
    let older = |t: SystemTime, d: Duration| now.duration_since(t).is_ok_and(|a| a > d);

    let built: Vec<(PathBuf, Option<Analysis>, SystemTime)> = profiles
        .iter()
        .map(|p| {
            let a = analyze(p);
            let newest = a.as_ref().and_then(|a| a.built.values().max().copied()).unwrap_or_else(|| newest_mtime(&p.join(".fingerprint")));
            (p.clone(), a, newest)
        })
        .collect();
    let latest = built.iter().map(|b| b.2).max().unwrap_or(now);
    let is_current = |t: SystemTime| t >= latest || !older(t, opts.current_window);

    let mut evictable: Vec<(SystemTime, Vec<(PathBuf, u64)>)> = Vec::new();
    for (profile, analysis, newest) in &built {
        let current = is_current(*newest);
        if !current && older(*newest, opts.keep_other_profile) {
            cleanup_session_leftovers(profile, opts.dead_incremental_grace, &mut report);
            let result = remove_profile_payloads_with(std::slice::from_ref(profile), |path, bytes| {
                report.removed_bytes += bytes;
                report.removed.push((path.to_path_buf(), bytes));
            });
            match result {
                Ok(()) => report.demoted_profiles.push(profile.clone()),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => report.retained_incremental += 1,
                Err(error) => report.errors.push((profile.clone(), error.to_string())),
            }
            continue;
        }
        let mut doomed: Vec<(PathBuf, u64)> = Vec::new();
        let incremental_root = profile.join("incremental");
        match analysis {
            Some(a) => {
                let by_unit = unit_paths(profile, &a.dead);
                for (hash, paths) in by_unit {
                    let when = a.built.get(&hash).copied().unwrap_or(SystemTime::UNIX_EPOCH);
                    let grace = if current { opts.uncached_dead_grace } else { opts.keep_other_profile };
                    if older(when, grace) || covered(&paths) {
                        report.dead_units += 1;
                        doomed.extend(paths);
                    } else if older(when, opts.dead_incremental_grace) {
                        // Outputs stay warm for a switch back; the incremental directories, needed only for an edit after it, go now.
                        let (incremental, rest): (Vec<_>, Vec<_>) = paths.into_iter().partition(|(p, _)| p.starts_with(&incremental_root));
                        report.dead_incremental += incremental.len();
                        doomed.extend(incremental);
                        evictable.push((when, rest));
                    } else {
                        evictable.push((when, paths));
                    }
                }
                for d in incremental_dirs(profile, a) {
                    if doomed.iter().any(|(p, _)| d.path.starts_with(p)) {
                        continue;
                    }
                    let size = || fsx::allocated_size(&d.path);
                    match d.owner {
                        IncrementalOwner::Orphan => {
                            report.dead_incremental += 1;
                            doomed.push((d.path.clone(), size()));
                        }
                        IncrementalOwner::Dead if older(d.touched, opts.dead_incremental_grace) => {
                            report.dead_incremental += 1;
                            doomed.push((d.path.clone(), size()));
                        }
                        IncrementalOwner::Live if current && older(d.touched, opts.incremental_idle) => {
                            let live: HashSet<String> = d.units.intersection(&a.live).cloned().collect();
                            let outputs: Vec<(PathBuf, u64)> = unit_paths(profile, &live).into_values().flatten().collect();
                            if covered(&outputs) {
                                report.idle_incremental += 1;
                                doomed.push((d.path.clone(), size()));
                            } else {
                                evictable.push((d.touched, vec![(d.path.clone(), size())]));
                            }
                        }
                        IncrementalOwner::Unknown => {
                            if let Some(age) = idle_unknown_incremental(&d.path, now) {
                                if age > opts.incremental_idle {
                                    report.idle_incremental += 1;
                                    doomed.push((d.path.clone(), size()));
                                } else if age > opts.incremental_idle.min(opts.dead_incremental_grace) {
                                    evictable.push((now - age, vec![(d.path.clone(), size())]));
                                }
                            }
                        }
                        _ => {}
                    }
                }
                if !current {
                    for (hash, paths) in unit_paths(profile, &a.live) {
                        if covered(&paths) {
                            evictable.push((a.built.get(&hash).copied().unwrap_or(SystemTime::UNIX_EPOCH), paths));
                        }
                    }
                    if let Ok(crates) = fs::read_dir(profile.join("incremental")) {
                        for krate in crates.flatten() {
                            if !krate.file_type().is_ok_and(|kind| kind.is_dir()) {
                                continue;
                            }
                            let when = newest_mtime(&krate.path());
                            evictable.push((when, vec![(krate.path(), fsx::allocated_size(&krate.path()))]));
                        }
                    }
                }
            }
            None => report.unclassified.push(profile.clone()),
        }
        let sessions: Vec<(PathBuf, u64)> =
            stale_sessions(profile).into_iter().filter(|(s, _)| !doomed.iter().any(|(p, _)| s.starts_with(p))).collect();
        report.stale_sessions += sessions.len();
        doomed.extend(sessions);
        doomed.extend(crate::gc::profile_temp_files(profile));
        remove_paths(&doomed, &mut report);
        cleanup_session_leftovers(profile, opts.dead_incremental_grace, &mut report);
        // After the sessions are gone, the aliases only they shared are single links.
        let objects = stale_objects(profile);
        report.stale_objects += objects.len();
        remove_paths(&objects, &mut report);
    }

    // Removals only shrink the target, so one that began under the cap is still under it.
    let mut size = if report.allocated_before <= opts.max_target_bytes || report.removed.is_empty() {
        report.allocated_before
    } else {
        allocated(target)
    };
    if size > opts.max_target_bytes {
        evictable.sort_by_key(|e| e.0);
        for (_, paths) in evictable {
            if size <= opts.max_target_bytes {
                break;
            }
            let live: Vec<(PathBuf, u64)> = paths.into_iter().filter(|(p, _)| p.exists()).collect();
            let before = report.removed_bytes;
            remove_paths(&live, &mut report);
            let freed = report.removed_bytes - before;
            report.budget_evicted += freed;
            size = size.saturating_sub(freed);
        }
    }

    if apfs::SUPPORTED && opts.compress {
        let mut candidates = Vec::new();
        let mut hot: HashMap<u64, HashSet<u64>> = HashMap::new();
        for (profile, _, newest) in built.iter().filter(|b| b.0.exists()) {
            let from = candidates.len();
            collect_candidates(profile, false, &mut candidates);
            if !is_current(*newest) {
                continue;
            }
            for c in candidates.split_off(from) {
                let category = c.path.strip_prefix(profile).map(Category::of).unwrap_or(Category::Other);
                let fresh = c.meta.modified().is_ok_and(|t| !older(t, opts.hot_window));
                if !(opts.hot_categories.contains(&category) && fresh) {
                    candidates.push(c);
                    continue;
                }
                if let Some((id, _)) = apfs::clone_info(&c.path) {
                    hot.entry(c.meta.len()).or_default().insert(id);
                }
            }
        }
        let sizes: HashSet<u64> = candidates.iter().map(|c| c.meta.len()).collect();
        if opts.cache_enabled && !sizes.is_empty() {
            for (len, paths) in &store_index().by_size {
                if !sizes.contains(len) {
                    continue;
                }
                for path in paths {
                    // Compressing a store copy alone would stop it sharing blocks with its target clone.
                    if hot.get(len).is_some_and(|ids| apfs::clone_info(path).is_some_and(|(id, _)| ids.contains(&id))) {
                        continue;
                    }
                    if let Ok(meta) = sys::lstat(path) {
                        candidates.push(Candidate { path: path.clone(), meta, in_store: true });
                    }
                }
            }
        }
        let mut ctx = ShrinkCtx::for_auto(dirs, target, opts);
        shrink(candidates, &CompactOptions::after_build(), &mut ctx, &mut report);
        report.budget_stopped = ctx.stopped;
        ctx.finish();
    }
    // Taken under cargo's locks so no build can change the tree between the pass and its signature.
    let signature = change_signature(dirs, target, &options_key);
    drop(locks);
    let changed = !report.removed.is_empty() || report.compressed_files > 0 || report.deduped_files > 0 || !report.errors.is_empty();
    report.allocated_after = if changed { allocated(target) } else { report.allocated_before };
    let complete = auto_pass_complete(&report);
    let record = AutoMarker {
        allocated_after: report.allocated_after,
        at_unix: if complete { fsx::unix_now() } else { previous.as_ref().map_or(0, |p| p.at_unix) },
        store_entries: store_entries(dirs),
        signature: signature.unwrap_or(0),
        complete,
    };
    write_marker(&file, &record);
    let _ = file.set_modified(SystemTime::now());
    report
}

fn retire_superseded_cache(dirs: &Dirs, report: &mut Report) {
    let superseded = crate::gc::superseded_cache_retirements(dirs, crate::gc::INCREMENTAL_ENTRIES_KEPT);
    report.superseded_entries = superseded.len();
    let store = crate::cache::Cache::new(dirs);
    for (path, bytes, snapshot) in superseded {
        match store.try_retire_entry(&snapshot) {
            Ok(crate::cache::EntryRetirementOutcome::Retired) => { report.removed_bytes += bytes; report.removed.push((path, bytes)); }
            Ok(crate::cache::EntryRetirementOutcome::Busy) => report.retained_cache_entries += 1,
            Ok(crate::cache::EntryRetirementOutcome::Stale) => {}
            Err(error) => report.errors.push((path, error.to_string())),
        }
    }
}

fn remove_or_plan(paths: Vec<(PathBuf, u64)>, dry_run: bool, report: &mut Report) {
    if dry_run {
        for (path, bytes) in paths {
            report.removed_bytes += bytes;
            report.removed.push((path, bytes));
        }
    } else {
        remove_paths(&paths, report);
    }
}

/// Caller holds the target gate and every Cargo profile lock after revalidating target idle age.
pub(crate) fn prune_expired_incremental(profile: &Path, ttl: Duration) -> Report {
    let sessions = sessions::prune_expired(profile, ttl);
    Report {
        stale_sessions: sessions.removed_finalized_dirs,
        stale_working_sessions: sessions.removed_dirs,
        orphan_session_locks: sessions.removed_locks,
        empty_incremental_dirs: sessions.removed_empty_dirs,
        retained_session_states: sessions.skipped,
        removed_bytes: sessions.removed.iter().map(|(_, bytes)| *bytes).sum(),
        removed: sessions.removed,
        errors: sessions.errors,
        ..Report::default()
    }
}

fn cleanup_session_leftovers(profile: &Path, grace: Duration, report: &mut Report) {
    let sessions = sessions::cleanup(profile, grace);
    report.stale_working_sessions += sessions.removed_dirs;
    report.orphan_session_locks += sessions.removed_locks;
    report.empty_incremental_dirs += sessions.removed_empty_dirs;
    report.retained_session_states += sessions.skipped;
    report.removed_bytes += sessions.removed.iter().map(|(_, bytes)| *bytes).sum::<u64>();
    report.removed.extend(sessions.removed);
    report.errors.extend(sessions.errors);
}

fn remove_paths(paths: &[(PathBuf, u64)], report: &mut Report) {
    for (path, bytes) in paths {
        // A session inside an incremental directory removed just before is already gone.
        if !path.exists() {
            continue;
        }
        let incremental = profile_of(path).map(|profile| profile.join("incremental")).filter(|root| path.starts_with(root));
        let result = match incremental {
            Some(root) => remove_incremental_payload(&root, path),
            None => fsx::remove_any(path),
        };
        match result {
            Ok(()) => {
                report.removed_bytes += bytes;
                report.removed.push((path.clone(), *bytes));
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => report.retained_incremental += 1,
            Err(err) => report.errors.push((path.clone(), err.to_string())),
        }
    }
}

/// Files with several links are listed rather than summed, so a link seen from two subtrees is counted once.
fn allocated(root: &Path) -> u64 {
    type Part = (u64, Vec<(u64, u64, u64)>);
    let parts = walk_parallel(
        root,
        &|_: &str, _: bool| false,
        &Part::default,
        &|acc: &mut Part, _: &Path, m: &sys::Meta| {
            if m.nlink() <= 1 {
                acc.0 += m.allocated();
            } else {
                acc.1.push((m.dev(), m.ino(), m.allocated()));
            }
        },
    );
    let mut seen = HashSet::new();
    let mut total = 0;
    for (single, linked) in parts {
        total += single;
        for (dev, ino, bytes) in linked {
            if seen.insert((dev, ino)) {
                total += bytes;
            }
        }
    }
    total
}

struct CoverageGroup {
    group: crate::cache::metadata_slice::RestoreGroup,
    signature: String,
    cost: u64,
}

struct CoverageEntry {
    path: PathBuf,
    manifest: FileKey,
    stderr: String,
    files: Vec<crate::cache::StoredFile>,
    groups: Vec<CoverageGroup>,
}

impl CoverageEntry {
    fn current(&self) -> bool {
        regular_key(&self.path.join("meta.json")) == Some(self.manifest)
            && regular_key(&self.path.join(&self.stderr)).is_some()
    }
}

#[derive(Clone)]
struct StoredArtifact {
    path: PathBuf,
    entry: Arc<CoverageEntry>,
    group: usize,
    file: usize,
}

type StoreScan = (Vec<(u64, PathBuf)>, Vec<StoredArtifact>);

/// Clone groups prove coverage cheaply; other filesystems verify only needed artifacts within a pass budget.
#[derive(Default)]
struct StoreIndex {
    by_size: HashMap<u64, Vec<PathBuf>>,
    raw_context: HashMap<PathBuf, StoredArtifact>,
    encoded: HashMap<u64, Vec<StoredArtifact>>,
    proofs: RefCell<CoverageProofs>,
}

struct CoverageProofs {
    budget: Budget,
    hashes: HashMap<FileKey, String>,
    encoded: HashMap<(FileKey, String), bool>,
}

impl Default for CoverageProofs {
    fn default() -> Self {
        Self {
            budget: Budget { deadline: Instant::now() + Duration::from_secs(2), bytes_left: 256 * 1024 * 1024 },
            hashes: HashMap::new(), encoded: HashMap::new(),
        }
    }
}

impl CoverageProofs {
    fn spend(&mut self, bytes: u64) -> bool {
        if bytes > self.budget.bytes_left || Instant::now() >= self.budget.deadline || self.hashes.len() + self.encoded.len() >= 4096 {
            return false;
        }
        self.budget.bytes_left -= bytes;
        true
    }

    fn hash(&mut self, path: &Path, expected: FileKey) -> Option<String> {
        if let Some(hash) = self.hashes.get(&expected) {
            return Some(hash.clone());
        }
        if !self.spend(expected.size) {
            return None;
        }
        let file = File::open(path).ok()?;
        if FileKey::of(&sys::fstat(&file).ok()?) != expected {
            return None;
        }
        let hash = crate::cache::hashing::hash_reader(&mut (&file).take(expected.size.saturating_add(1))).ok()?;
        if FileKey::of(&sys::fstat(&file).ok()?) != expected || regular_key(path)? != expected {
            return None;
        }
        self.hashes.insert(expected, hash.clone());
        Some(hash)
    }

    // One verified group can serve several outputs only with the same backing identity and complete descriptor.
    fn encoded_valid(&mut self, artifact: &StoredArtifact, key: FileKey) -> bool {
        let entry = &artifact.entry;
        let group = &entry.groups[artifact.group];
        if !entry.current() || regular_key(&artifact.path) != Some(key) { return false; }
        let proof_key = (key, group.signature.clone());
        if let Some(valid) = self.encoded.get(&proof_key) { return *valid; }
        let Some(cost) = key.size.checked_add(group.cost) else { return false };
        if !self.spend(cost) { return false; }
        let valid = crate::cache::metadata_slice::verify_group(&entry.path, &entry.files, &group.group).is_ok()
            && regular_key(&artifact.path) == Some(key) && entry.current();
        self.encoded.insert(proof_key, valid);
        valid
    }
}

fn regular_key(path: &Path) -> Option<FileKey> {
    let meta = sys::lstat(path).ok()?;
    meta.is_file().then(|| FileKey::of(&meta))
}

impl StoreIndex {
    fn new(store: &Path) -> StoreIndex {
        let mut index = StoreIndex::default();
        let parts = walk_parallel(
            store,
            &|_: &str, _: bool| false,
            &StoreScan::default,
            &|acc: &mut StoreScan, path: &Path, meta: &sys::Meta| {
                if path.file_name().is_some_and(|n| n == "meta.json") {
                    if let Ok(bytes) = fs::read(path)
                        && let Ok(entry) = serde_json::from_slice::<crate::cache::EntryMeta>(&bytes)
                        && let Some(parent) = path.parent()
                        && crate::cache::safe_file_name(&entry.stderr)
                        && sys::lstat(&parent.join(&entry.stderr)).is_ok_and(|m| m.is_file())
                        && let Ok(groups) = crate::cache::metadata_slice::validate_groups(&entry.files)
                        && (cfg!(unix) || groups.iter().all(|group| group.slice_indices.is_empty()))
                        && regular_key(path) == Some(FileKey::of(meta))
                    {
                        let groups = groups.into_iter().map(|group| {
                            Ok(CoverageGroup {
                                signature: crate::cache::metadata_slice::proof_signature(&entry.files, &group)?,
                                cost: crate::cache::metadata_slice::cost(&entry.files, &group)?, group,
                            })
                        }).collect::<io::Result<Vec<_>>>();
                        if let Ok(groups) = groups {
                            let context = Arc::new(CoverageEntry {
                                path: parent.to_path_buf(), manifest: FileKey::of(meta), stderr: entry.stderr,
                                files: entry.files, groups,
                            });
                            for (group, records) in context.groups.iter().enumerate() {
                                for file in std::iter::once(records.group.base_index).chain(records.group.slice_indices.iter().copied()) {
                                    let record = &context.files[file];
                                    if !record.text {
                                        acc.1.push(StoredArtifact { path: parent.join(&record.stored), entry: Arc::clone(&context), group, file });
                                    }
                                }
                            }
                        }
                    }
                } else if meta.len() > 0 {
                    acc.0.push((meta.len(), path.to_path_buf()));
                }
            },
        );
        let raw: HashMap<PathBuf, StoredArtifact> = parts.iter().flat_map(|part| part.1.iter())
            .filter(|artifact| artifact.entry.files[artifact.file].encoding.is_raw())
            .map(|artifact| (artifact.path.clone(), artifact.clone())).collect();
        for (files, entries) in parts {
            for (len, path) in files {
                if let Some(context) = path.ancestors().find_map(|ancestor| raw.get(ancestor)) {
                    index.raw_context.insert(path.clone(), context.clone());
                    index.by_size.entry(len).or_default().push(path);
                }
            }
            for artifact in entries {
                let file = &artifact.entry.files[artifact.file];
                if file.encoding.is_raw() || file.dir { continue; }
                if let (Some(len), Some(hash)) = (file.decoded_bytes, file.content_hash.as_ref())
                    && len > 0 && hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
                    && regular_key(&artifact.path).is_some()
                {
                    index.encoded.entry(len).or_default().push(artifact);
                }
            }
        }
        for paths in index.by_size.values_mut() {
            paths.sort();
        }
        for paths in index.encoded.values_mut() {
            paths.sort_by(|a, b| a.path.cmp(&b.path));
        }
        index
    }

    fn with_budget(self, bytes: u64, duration: Duration) -> Self {
        let mut proof = self.proofs.borrow_mut();
        proof.budget.bytes_left = proof.budget.bytes_left.min(bytes);
        proof.budget.deadline = proof.budget.deadline.min(Instant::now() + duration);
        drop(proof);
        self
    }

    fn raw_restorable(&self, source: &Path, expected: FileKey, proofs: &mut CoverageProofs) -> bool {
        let Some(artifact) = self.raw_context.get(source) else { return false };
        artifact.entry.current() && (artifact.entry.groups[artifact.group].group.slice_indices.is_empty()
            || proofs.encoded_valid(artifact, expected))
    }

    fn covers_file(&self, path: &Path, meta: &sys::Meta) -> bool {
        self.covers_file_with_raw_proof(path, meta, |_| {})
    }

    fn covers_file_with_raw_proof(&self, path: &Path, meta: &sys::Meta, mut after_raw_proof: impl FnMut(&Path)) -> bool {
        let candidates = self.by_size.get(&meta.len());
        if let Some((group, _)) = apfs::clone_info(path)
            && candidates.is_some_and(|cands| cands.iter().any(|candidate| {
                let Ok(source) = sys::lstat(candidate) else { return false };
                source.is_file() && source.len() == meta.len() && source.mode() & 0o7777 == meta.mode() & 0o7777
                    && self.raw_restorable(candidate, FileKey::of(&source), &mut self.proofs.borrow_mut())
                    && apfs::clone_info(candidate).is_some_and(|(found, _)| found == group)
                    && regular_key(candidate) == Some(FileKey::of(&source))
                    && self.raw_context[candidate].entry.current()
            }))
        {
            return regular_key(path) == Some(FileKey::of(meta));
        }
        if candidates.is_none() && !self.encoded.contains_key(&meta.len()) {
            return false;
        }
        let key = FileKey::of(meta);
        let mut proof = self.proofs.borrow_mut();
        let Some(hash) = proof.hash(path, key) else { return false };
        if let Some(encoded) = self.encoded.get(&meta.len()) {
            for artifact in encoded {
                let payload = &artifact.entry.files[artifact.file];
                if payload.content_hash.as_ref() != Some(&hash) || !artifact.entry.current() { continue; }
                let Ok(source_meta) = sys::lstat(&artifact.path) else { continue };
                let Ok(mode) = crate::cache::metadata_slice::restored_mode(&artifact.entry.path, &artifact.entry.files, artifact.file) else { continue };
                if source_meta.is_file() && mode & 0o7777 == meta.mode() & 0o7777
                    && proof.encoded_valid(artifact, FileKey::of(&source_meta))
                {
                    return regular_key(path) == Some(key) && artifact.entry.current()
                        && regular_key(&artifact.path) == Some(FileKey::of(&source_meta));
                }
            }
        }
        if let Some(candidates) = candidates {
            for source in candidates {
                let Ok(source_meta) = sys::lstat(source) else { continue };
                if !source_meta.is_file() || source_meta.len() != meta.len() || source_meta.mode() & 0o7777 != meta.mode() & 0o7777 {
                    continue;
                }
                let source_key = FileKey::of(&source_meta);
                if !self.raw_restorable(source, source_key, &mut proof) { continue; }
                after_raw_proof(source);
                let context = &self.raw_context[source].entry;
                if proof.hash(source, source_key).as_ref() == Some(&hash) {
                    return regular_key(path) == Some(key) && context.current()
                        && regular_key(source) == Some(FileKey::of(&source_meta));
                }
            }
        }
        false
    }

    /// Build-script output is never cached, so a unit with only `build/` files is not covered.
    fn covers(&self, paths: &[(PathBuf, u64)]) -> bool {
        let mut any = false;
        for (path, _) in paths {
            let Some(parent) = path.parent().and_then(|p| p.file_name()) else { continue };
            if parent != "deps" && parent != "examples" {
                continue;
            }
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if name.ends_with(".d") || name.ends_with(".o") {
                continue;
            }
            let Ok(meta) = sys::lstat(path) else { return false };
            if !meta.is_file() {
                continue;
            }
            if !self.covers_file(path, &meta) {
                return false;
            }
            any = true;
        }
        any
    }
}

#[cfg(target_os = "macos")]
mod apfs {
    use std::ffi::CString;
    use std::fs;
    use std::io;
    use std::os::macos::fs::MetadataExt as _;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    pub const SUPPORTED: bool = true;

    const DECMPFS_XATTR: &str = "com.apple.decmpfs";
    const RESOURCE_FORK_XATTR: &str = "com.apple.ResourceFork";
    const CMP_LZFSE_RSRC: u32 = 12;
    const CHUNK: usize = 64 * 1024;
    const COMPRESSION_LZFSE: libc::c_int = 0x801;
    const CLONE_NOFOLLOW: u32 = 0x0001;

    #[link(name = "compression")]
    unsafe extern "C" {
        fn compression_encode_buffer(
            dst: *mut u8,
            dst_size: usize,
            src: *const u8,
            src_size: usize,
            scratch: *mut libc::c_void,
            algorithm: libc::c_int,
        ) -> usize;
    }

    fn cstr(path: &Path) -> io::Result<CString> {
        CString::new(path.as_os_str().as_bytes()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
    }

    pub fn is_compressed(meta: &fs::Metadata) -> bool {
        meta.st_flags() & libc::UF_COMPRESSED != 0
    }

    /// Immutable, hidden and other flagged files are left alone.
    pub fn only_compression_flag(meta: &fs::Metadata) -> bool {
        meta.st_flags() & !libc::UF_COMPRESSED == 0
    }

    /// LZFSE stores incompressible input as raw blocks itself, so every chunk decodes alone.
    pub fn encode_lzfse_fork(data: &[u8]) -> Option<Vec<u8>> {
        let chunks: Vec<Vec<u8>> = data
            .chunks(CHUNK)
            .map(|chunk| {
                let mut out = vec![0u8; chunk.len() + 4096];
                // SAFETY: both buffers are valid for the lengths passed; a null scratch buffer makes the library allocate.
                let n = unsafe {
                    compression_encode_buffer(out.as_mut_ptr(), out.len(), chunk.as_ptr(), chunk.len(), std::ptr::null_mut(), COMPRESSION_LZFSE)
                };
                (n > 0).then(|| {
                    out.truncate(n);
                    out
                })
            })
            .collect::<Option<_>>()?;
        let table = 4 * (chunks.len() + 1);
        let total = table + chunks.iter().map(Vec::len).sum::<usize>();
        u32::try_from(total).ok()?;
        let mut fork = Vec::with_capacity(total);
        let mut offset = table;
        for c in &chunks {
            fork.extend_from_slice(&(offset as u32).to_le_bytes());
            offset += c.len();
        }
        fork.extend_from_slice(&(offset as u32).to_le_bytes());
        for c in &chunks {
            fork.extend_from_slice(c);
        }
        Some(fork)
    }

    fn set_xattr(fd: libc::c_int, name: &str, value: &[u8]) -> io::Result<()> {
        let name = CString::new(name).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        // SAFETY: valid descriptor, NUL-terminated name and a buffer of the given length.
        let rc = unsafe { libc::fsetxattr(fd, name.as_ptr(), value.as_ptr().cast(), value.len(), 0, 0) };
        if rc == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
    }

    fn xattrs(fd: libc::c_int) -> Vec<(CString, Vec<u8>)> {
        let mut out = Vec::new();
        // SAFETY: a null buffer asks for the size of the name list.
        let len = unsafe { libc::flistxattr(fd, std::ptr::null_mut(), 0, 0) };
        if len <= 0 {
            return out;
        }
        let mut names = vec![0u8; len as usize];
        // SAFETY: the buffer holds `names.len()` bytes.
        let len = unsafe { libc::flistxattr(fd, names.as_mut_ptr().cast(), names.len(), 0) };
        if len <= 0 {
            return out;
        }
        for name in names[..len as usize].split(|b| *b == 0).filter(|n| !n.is_empty()) {
            if name == DECMPFS_XATTR.as_bytes() || name == RESOURCE_FORK_XATTR.as_bytes() {
                continue;
            }
            let Ok(cname) = CString::new(name) else { continue };
            // SAFETY: size query with a null buffer.
            let size = unsafe { libc::fgetxattr(fd, cname.as_ptr(), std::ptr::null_mut(), 0, 0, 0) };
            if size < 0 {
                continue;
            }
            let mut value = vec![0u8; size as usize];
            // SAFETY: the buffer holds `value.len()` bytes.
            let got = unsafe { libc::fgetxattr(fd, cname.as_ptr(), value.as_mut_ptr().cast(), value.len(), 0, 0) };
            if got >= 0 {
                value.truncate(got as usize);
                out.push((cname, value));
            }
        }
        out
    }

    pub fn write_compressed(tmp: &Path, original: &Path, meta: &fs::Metadata, data: &[u8], fork: &[u8]) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let attrs = fs::File::open(original).map(|f| xattrs(f.as_raw_fd())).unwrap_or_default();
        let file = fs::File::options().write(true).create_new(true).mode(meta.permissions().mode() & 0o7777).open(tmp)?;
        let fd = file.as_raw_fd();
        for (name, value) in &attrs {
            // SAFETY: valid descriptor, name and buffer; failures are ignored since these attributes do not affect builds.
            unsafe { libc::fsetxattr(fd, name.as_ptr(), value.as_ptr().cast(), value.len(), 0, 0) };
        }
        set_xattr(fd, RESOURCE_FORK_XATTR, fork)?;
        let mut header = Vec::with_capacity(16);
        header.extend_from_slice(b"fpmc");
        header.extend_from_slice(&CMP_LZFSE_RSRC.to_le_bytes());
        header.extend_from_slice(&(data.len() as u64).to_le_bytes());
        set_xattr(fd, DECMPFS_XATTR, &header)?;
        drop(file);
        let c = cstr(tmp)?;
        // SAFETY: valid path; the data fork is empty and both attributes are in place.
        if unsafe { libc::chflags(c.as_ptr(), libc::UF_COMPRESSED) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn clone(src: &Path, dst: &Path) -> io::Result<()> {
        let (s, d) = (cstr(src)?, cstr(dst)?);
        // SAFETY: valid NUL-terminated paths.
        if unsafe { libc::clonefile(s.as_ptr(), d.as_ptr(), CLONE_NOFOLLOW) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    /// Opening a compressed file to set its times could make the kernel decompress it.
    pub fn set_times(path: &Path, atime: (i64, i64), mtime: (i64, i64)) -> io::Result<()> {
        let c = cstr(path)?;
        let times = [
            libc::timespec { tv_sec: atime.0, tv_nsec: atime.1 },
            libc::timespec { tv_sec: mtime.0, tv_nsec: mtime.1 },
        ];
        // SAFETY: valid path and a two-element timespec array.
        let rc = unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) };
        if rc == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
    }

    #[repr(C, packed(4))]
    struct CloneAttrs {
        length: u32,
        returned: libc::attribute_set_t,
        private_size: libc::off_t,
        clone_id: u64,
    }

    pub fn clone_info(path: &Path) -> Option<(u64, u64)> {
        let c = cstr(path).ok()?;
        // SAFETY: zeroed plain-old-data structs, filled in below.
        let mut list: libc::attrlist = unsafe { std::mem::zeroed() };
        list.bitmapcount = libc::ATTR_BIT_MAP_COUNT;
        list.commonattr = libc::ATTR_CMN_RETURNED_ATTRS;
        list.forkattr = libc::ATTR_CMNEXT_PRIVATESIZE | libc::ATTR_CMNEXT_CLONEID;
        let mut buf: CloneAttrs = unsafe { std::mem::zeroed() };
        // SAFETY: valid path, attribute list and output buffer of the size passed.
        let rc = unsafe {
            libc::getattrlist(
                c.as_ptr(),
                (&mut list as *mut libc::attrlist).cast(),
                (&mut buf as *mut CloneAttrs).cast(),
                std::mem::size_of::<CloneAttrs>(),
                libc::FSOPT_ATTR_CMN_EXTENDED | libc::FSOPT_NOFOLLOW,
            )
        };
        let returned = buf.returned;
        let wanted = libc::ATTR_CMNEXT_PRIVATESIZE | libc::ATTR_CMNEXT_CLONEID;
        if rc != 0 || returned.forkattr & wanted != wanted {
            return None;
        }
        let (private, id) = (buf.private_size, buf.clone_id);
        Some((id, private.max(0) as u64))
    }
}

#[cfg(not(target_os = "macos"))]
mod apfs {
    use std::fs;
    use std::io;
    use std::path::Path;

    pub const SUPPORTED: bool = false;

    pub fn is_compressed(_meta: &fs::Metadata) -> bool {
        false
    }

    pub fn only_compression_flag(_meta: &fs::Metadata) -> bool {
        true
    }

    pub fn encode_lzfse_fork(_data: &[u8]) -> Option<Vec<u8>> {
        None
    }

    pub fn write_compressed(_tmp: &Path, _original: &Path, _meta: &fs::Metadata, _data: &[u8], _fork: &[u8]) -> io::Result<()> {
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }

    pub fn clone(_src: &Path, _dst: &Path) -> io::Result<()> {
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }

    pub fn set_times(_path: &Path, _atime: (i64, i64), _mtime: (i64, i64)) -> io::Result<()> {
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }

    pub fn clone_info(_path: &Path) -> Option<(u64, u64)> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compaction_retires_only_superseded_cache_states_and_prunes_known_manifests() {
        let root = fsx::temp_sibling(&std::env::temp_dir().join("frust-compact-cache-retirement"));
        let dirs = Dirs::at(&root);
        let store = crate::cache::Cache::new(&dirs);
        let prekey = "ac".repeat(20);
        let mut paths = Vec::new();
        for n in 1..=4 {
            let key = format!("{n:040x}");
            let staged = store.stage().unwrap();
            fs::write(staged.join("meta.json"), serde_json::to_vec(&serde_json::json!({
                "crate_name":"c", "version":"1", "compile_ms":1, "created_unix":1, "stderr":"",
                "incremental_unit":"ab".repeat(20), "prekey":prekey,
                "files":[{"name":"artifact", "stored":"f0", "text":false, "bytes":32}]
            })).unwrap()).unwrap();
            fs::write(staged.join("f0"), [n as u8; 32]).unwrap();
            store.publish(&staged, &key).unwrap();
            store.add_candidate(&prekey, crate::cache::Candidate { inputs: Vec::new(), envs: Vec::new(), key: key.clone() });
            let path = store.entry_dir(&key);
            File::options().write(true).open(path.join("meta.json")).unwrap().set_modified(SystemTime::now() - Duration::from_secs((5 - n) * 5000)).unwrap();
            paths.push(path);
        }
        let mut report = Report::default();
        retire_superseded_cache(&dirs, &mut report);
        assert!(report.errors.is_empty());
        assert_eq!(report.superseded_entries, 2);
        assert_eq!(report.removed.len(), 2);
        assert!(paths[..2].iter().all(|path| !path.exists()));
        assert!(paths[2..].iter().all(|path| path.exists()));
        assert_eq!(store.candidates(&prekey).into_iter().map(|row| row.key).collect::<Vec<_>>(), vec![format!("{:040x}", 4), format!("{:040x}", 3)]);
        let mut second = Report::default();
        retire_superseded_cache(&dirs, &mut second);
        assert!(second.removed.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn busy_cache_retirement_marks_auto_pass_incomplete_and_retries_without_growth() {
        cache_retirement_retry_after_obstruction(false);
    }

    #[test]
    fn blocked_retirement_queue_marks_auto_pass_incomplete_and_retries_after_repair() {
        cache_retirement_retry_after_obstruction(true);
    }

    fn cache_retirement_retry_after_obstruction(queue_obstructed: bool) {
        let tag = if queue_obstructed { "frust-compact-blocked-queue" } else { "frust-compact-busy-cache" };
        let root = fsx::temp_sibling(&std::env::temp_dir().join(tag));
        let dirs = Dirs::at(&root);
        let store = crate::cache::Cache::new(&dirs);
        let target = dirs.targets().join("workspace/toolchain");
        fs::create_dir_all(&target).unwrap();
        for n in 1..=3 {
            let key = format!("{n:040x}");
            let staged = store.stage().unwrap();
            fs::write(staged.join("meta.json"), serde_json::to_vec(&serde_json::json!({
                "crate_name":"c", "version":"1", "compile_ms":1, "created_unix":1, "stderr":"",
                "incremental_unit":"ab".repeat(20),
                "files":[{"name":"artifact", "stored":"f0", "text":false, "bytes":32}]
            })).unwrap()).unwrap();
            fs::write(staged.join("f0"), [n as u8; 32]).unwrap();
            store.publish(&staged, &key).unwrap();
            File::options().write(true).open(store.entry_dir(&key).join("meta.json")).unwrap().set_modified(SystemTime::now() - Duration::from_secs((4 - n) * 5000)).unwrap();
        }
        let mut opts = AutoOptions::from_env(&Config::from_env());
        opts.enabled = true;
        opts.every = Duration::from_secs(600);
        opts.min_growth = u64::MAX;
        opts.compress = false;
        opts.cache_enabled = false;
        let guard = if queue_obstructed {
            fs::write(crate::cache::evicted_dir(&dirs), b"owned queue obstruction").unwrap();
            None
        } else { Some(crate::cache::UnitRecordsGuard::try_acquire(&dirs).unwrap().unwrap()) };
        let first = auto_compact_with(&dirs, &target, &opts);
        assert_eq!(first.retained_cache_entries, usize::from(!queue_obstructed));
        assert_eq!(first.errors.len(), usize::from(queue_obstructed));
        assert!(first.removed.is_empty());
        assert_eq!(first.removed_bytes, 0);
        assert!(store.has_entry(&format!("{:040x}", 1)));
        let marker_path = target.join(AUTO_MARKER);
        let marker: AutoMarker = serde_json::from_slice(&fs::read(&marker_path).unwrap()).unwrap();
        assert!(!marker.complete);
        assert_eq!(marker.interval(&opts), AUTO_RETRY_EVERY);
        drop(guard);
        if queue_obstructed {
            let saved = dirs.tmp().join("saved-owned-obstruction");
            sys::rename(&crate::cache::evicted_dir(&dirs), &saved).unwrap();
            assert_eq!(fs::read(saved).unwrap(), b"owned queue obstruction");
        }
        File::options().write(true).open(&marker_path).unwrap().set_modified(SystemTime::now() - AUTO_RETRY_EVERY - Duration::from_secs(1)).unwrap();
        let retry = auto_compact_with(&dirs, &target, &opts);
        assert_eq!(retry.retained_cache_entries, 0);
        assert_eq!(retry.removed.len(), 1);
        assert!(retry.errors.is_empty());
        assert!(!store.has_entry(&format!("{:040x}", 1)));
        assert!(store.has_entry(&format!("{:040x}", 2)) && store.has_entry(&format!("{:040x}", 3)));
        let marker: AutoMarker = serde_json::from_slice(&fs::read(&marker_path).unwrap()).unwrap();
        assert!(marker.complete);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn store_index_excludes_encoded_payloads_and_keeps_legacy_raw_files() {
        let root = fsx::temp_sibling(&std::env::temp_dir().join("frust-store-index"));
        let entry = root.join("aa").join("a".repeat(40));
        fs::create_dir_all(entry.join("bundle")).unwrap();
        fs::write(entry.join("f0"), b"encoded bytes").unwrap();
        fs::write(entry.join("f1"), b"raw bytes").unwrap();
        fs::write(entry.join("stderr"), b"").unwrap();
        fs::write(entry.join("bundle/member"), b"bundle bytes").unwrap();
        fs::write(entry.join("meta.json"), serde_json::to_vec(&serde_json::json!({
            "crate_name":"c", "version":"1", "compile_ms":1, "created_unix":1, "stderr":"stderr",
            "files":[
                {"name":"artifact", "stored":"f0", "text":false, "bytes":65536, "encoding":"lz4_blocks_v1", "decoded_bytes":65536, "content_hash":"0".repeat(64)},
                {"name":"legacy", "stored":"f1", "text":false, "bytes":9},
                {"name":"bundle", "stored":"bundle", "text":false, "bytes":12, "dir":true}
            ]
        })).unwrap()).unwrap();
        let index = StoreIndex::new(&root);
        let indexed: HashSet<_> = index.by_size.values().flatten().cloned().collect();
        assert!(!indexed.contains(&entry.join("f0")));
        assert!(!indexed.contains(&entry.join("meta.json")));
        assert!(indexed.contains(&entry.join("f1")));
        assert!(indexed.contains(&entry.join("bundle/member")));
        let _ = fs::remove_dir_all(root);
    }

    fn coverage_entry(store: &Path, source: &Path, compress: bool) -> PathBuf {
        let entry = store.join("aa").join("a".repeat(40));
        fs::create_dir_all(&entry).unwrap();
        let file = crate::cache::payload::store(source, &entry.join("f0"), compress).unwrap();
        let meta = crate::cache::EntryMeta {
            crate_name: "app".into(), version: "1".into(), compile_ms: 1, created_unix: 1, incremental_unit: None, prekey: None,
            files: vec![crate::cache::StoredFile {
                name: source.file_name().unwrap().to_str().unwrap().into(), stored: "f0".into(), text: false, bytes: file.bytes, dir: false,
                encoding: file.encoding, stored_bytes: file.stored_bytes, decoded_bytes: file.decoded_bytes, content_hash: file.content_hash, metadata_slice: None,
            }], stderr: "stderr".into(),
        };
        fs::write(entry.join("meta.json"), serde_json::to_vec(&meta).unwrap()).unwrap();
        fs::write(entry.join("stderr"), b"").unwrap();
        entry
    }

    #[cfg(unix)]
    fn coverage_slice_entry(root: &Path, compress: bool) -> (PathBuf, PathBuf, crate::cache::EntryMeta) {
        let artifact = root.join("debug/deps/libapp-0123456789abcdef.rmeta");
        fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        let metadata: Vec<_> = (0..128 * 1024 + 211).map(|index| (index % 251) as u8).collect();
        fs::write(&artifact, &metadata).unwrap();
        sys::set_path_mode(&artifact, 0o751).unwrap();
        let mut member = vec![0x7f; 37];
        member.extend_from_slice(&metadata);
        member.extend_from_slice(&[0x39; 113]);
        let mut archive = b"!<arch>\n".to_vec();
        archive.extend_from_slice(format!("{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n", "lib.rmeta/", 0, 0, 0, "100644", member.len()).as_bytes());
        archive.extend_from_slice(&member);
        if !member.len().is_multiple_of(2) { archive.push(b'\n'); }
        let source = root.join("source.rlib");
        fs::write(&source, archive).unwrap();
        sys::set_path_mode(&source, 0o640).unwrap();
        let entry = root.join("store/aa").join("a".repeat(40));
        fs::create_dir_all(&entry).unwrap();
        let stored = crate::cache::payload::store(&source, &entry.join("f0"), compress).unwrap();
        let base = crate::cache::StoredFile {
            name: "libapp-0123456789abcdef.rlib".into(), stored: "f0".into(), text: false, bytes: stored.bytes,
            encoding: stored.encoding, stored_bytes: stored.stored_bytes, decoded_bytes: stored.decoded_bytes,
            content_hash: stored.content_hash, metadata_slice: None, dir: false,
        };
        let target = crate::cache::StoredFile {
            name: artifact.file_name().unwrap().to_string_lossy().into_owned(), stored: "f1".into(), text: false,
            bytes: fsx::allocated_size(&artifact), encoding: crate::cache::payload::Encoding::Raw, stored_bytes: None,
            decoded_bytes: None, content_hash: None, metadata_slice: None, dir: false,
        };
        let slice = crate::cache::metadata_slice::build(&entry.join("f0"), &base, &artifact, &target, None).unwrap().unwrap();
        let meta = crate::cache::EntryMeta {
            crate_name: "app".into(), version: "1".into(), compile_ms: 1, created_unix: 1, incremental_unit: None, prekey: None,
            files: vec![base, slice], stderr: "stderr".into(),
        };
        fs::write(entry.join("meta.json"), serde_json::to_vec(&meta).unwrap()).unwrap();
        fs::write(entry.join("stderr"), b"").unwrap();
        (artifact, entry, meta)
    }

    #[cfg(unix)]
    #[test]
    fn metadata_slice_coverage_checks_full_backing_modes_and_replaced_sources() {
        for compress in [false, true] {
            let root = fsx::temp_sibling(&std::env::temp_dir().join("frust-slice-coverage"));
            let (artifact, entry, meta) = coverage_slice_entry(&root, compress);
            assert_ne!(sys::stat(&artifact).unwrap().mode() & 0o7777, sys::stat(&entry.join("f0")).unwrap().mode() & 0o7777);
            let index = StoreIndex::new(&root.join("store"));
            let paths = [(artifact.clone(), 0)];
            assert!(index.covers(&paths));
            let left = index.proofs.borrow().budget.bytes_left;
            assert!(index.covers(&paths));
            assert_eq!(index.proofs.borrow().budget.bytes_left, left);
            let backing = entry.join("f0");
            let original = fs::read(&backing).unwrap();
            let mut corrupt = original.clone();
            corrupt[0] ^= 1;
            fs::write(&backing, &corrupt).unwrap();
            assert!(!index.covers(&paths), "a raw backing rewrite outside the slice still invalidates its full commitment");
            fs::write(&backing, &original).unwrap();
            assert!(index.covers(&paths));
            fsx::write_atomic(&backing, &corrupt).unwrap();
            assert!(!index.covers(&paths), "an old successful inode proof cannot cover a replacement");
            fs::remove_file(&backing).unwrap();
            assert!(!index.covers(&paths));
            assert_eq!(meta.files[1].stored_bytes, Some(0));
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn metadata_slice_raw_coverage_rejects_backing_replacement_after_group_proof() {
        let root = fsx::temp_sibling(&std::env::temp_dir().join("frust-slice-proof-race"));
        let (_, entry, _) = coverage_slice_entry(&root, false);
        let backing = entry.join("f0");
        let original_key = regular_key(&backing).unwrap();
        let mut replacement = fs::read(&backing).unwrap();
        replacement[0] ^= 1;
        let target = root.join("debug/deps/libapp-0123456789abcdef.rlib");
        fs::write(&target, &replacement).unwrap();
        sys::set_path_mode(&target, 0o640).unwrap();
        let index = StoreIndex::new(&root.join("store"));
        let mut reached_proof = false;
        let covered = index.covers_file_with_raw_proof(&target, &sys::lstat(&target).unwrap(), |source| {
            assert_eq!(source, backing);
            assert_eq!(regular_key(source), Some(original_key));
            fsx::write_atomic(source, &replacement).unwrap();
            sys::set_path_mode(source, 0o640).unwrap();
            reached_proof = true;
        });
        assert!(reached_proof, "the mutation must happen after a complete successful group proof");
        assert!(index.proofs.borrow().encoded.values().any(|valid| *valid));
        assert!(!covered, "matching replacement bytes cannot inherit a different backing's group proof");
        assert_eq!(fs::read(&backing).unwrap(), fs::read(&target).unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn metadata_slice_group_signatures_prevent_descriptor_proof_aliases() {
        let root = fsx::temp_sibling(&std::env::temp_dir().join("frust-slice-descriptor-proof"));
        let (artifact, entry, original) = coverage_slice_entry(&root, false);
        let store = root.join("store");
        let index = StoreIndex::new(&store);
        let len = sys::stat(&artifact).unwrap().len();
        let original_artifact = &index.encoded[&len][0];
        let key = regular_key(&entry.join("f0")).unwrap();
        let mut proofs = CoverageProofs::default();
        assert!(proofs.encoded_valid(original_artifact, key));
        proofs.budget.bytes_left = 0;
        for change in 0..4 {
            let mut modified = original.clone();
            let slice = &mut modified.files[1];
            match change {
                0 => slice.metadata_slice.as_mut().unwrap().offset += 1,
                1 => slice.content_hash = Some("0".repeat(64)),
                2 => slice.metadata_slice.as_mut().unwrap().mode = 0o100600,
                _ => slice.metadata_slice.as_mut().unwrap().source_content_hash = "1".repeat(64),
            }
            fsx::write_atomic(&entry.join("meta.json"), &serde_json::to_vec(&modified).unwrap()).unwrap();
            let changed = StoreIndex::new(&store);
            let candidate = &changed.encoded[&len][0];
            assert_eq!(regular_key(&candidate.path), Some(key));
            assert!(!proofs.encoded_valid(candidate, key), "changed descriptor {change} cannot use an old same-inode proof");
            assert!(!index.covers(&[(artifact.clone(), 0)]), "a changed manifest invalidates the original entry context");
        }
        let mut invalid = original;
        invalid.files[1].metadata_slice.as_mut().unwrap().mode = 0o040700;
        fs::write(entry.join("meta.json"), serde_json::to_vec(&invalid).unwrap()).unwrap();
        let invalid = StoreIndex::new(&store);
        assert!(invalid.by_size.is_empty() && invalid.encoded.is_empty(), "invalid modes reject the whole entry, including raw backing");
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn metadata_slice_verification_charges_complete_source_and_preserves_uncovered_artifacts() {
        for compress in [false, true] {
            let root = fsx::temp_sibling(&std::env::temp_dir().join("frust-slice-budget"));
            let (artifact, entry, meta) = coverage_slice_entry(&root, compress);
            let target_len = sys::stat(&artifact).unwrap().len();
            let backing_len = sys::stat(&entry.join("f0")).unwrap().len();
            let slice_only_budget = target_len + backing_len + target_len;
            let index = StoreIndex::new(&root.join("store")).with_budget(slice_only_budget, Duration::from_secs(10));
            assert!(!index.covers(&[(artifact.clone(), 0)]));
            assert!(artifact.is_file());
            assert!(index.proofs.borrow().encoded.is_empty());
            assert!(meta.files[1].metadata_slice.as_ref().unwrap().source_decoded_bytes > target_len);
            if !compress {
                let backing_target = root.join("debug/deps/libapp-0123456789abcdef.rlib");
                fsx::clone_new(&entry.join("f0"), &backing_target).unwrap();
                let mut broken = meta;
                broken.files[1].content_hash = Some("0".repeat(64));
                fs::write(entry.join("meta.json"), serde_json::to_vec(&broken).unwrap()).unwrap();
                assert!(!StoreIndex::new(&root.join("store")).covers(&[(backing_target, 0)]), "a raw base cannot cover an artifact when its grouped restore would fail");
            }
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn portable_coverage_verifies_raw_and_encoded_bytes_and_invalidates_mutations() {
        for compress in [false, true] {
            let root = fsx::temp_sibling(&std::env::temp_dir().join("frust-coverage"));
            let artifact = root.join("debug/deps/libapp-0123456789abcdef.rlib");
            fs::create_dir_all(artifact.parent().unwrap()).unwrap();
            fs::write(&artifact, vec![7; 128 * 1024]).unwrap();
            let entry = coverage_entry(&root.join("store"), &artifact, compress);
            let index = StoreIndex::new(&root.join("store"));
            let paths = [(artifact.clone(), 0)];
            assert!(index.covers(&paths));
            let left = index.proofs.borrow().budget.bytes_left;
            assert!(index.covers(&paths));
            assert_eq!(left, index.proofs.borrow().budget.bytes_left, "unchanged proofs cost no more reads");
            fs::write(&artifact, vec![8; 128 * 1024]).unwrap();
            assert!(!index.covers(&paths), "an equal-sized rewrite invalidates the target proof");
            fs::write(&artifact, vec![7; 128 * 1024]).unwrap();
            let mut damaged = fs::read(entry.join("f0")).unwrap();
            let middle = damaged.len() / 2;
            damaged[middle] ^= 0x55;
            fs::write(entry.join("f0"), damaged).unwrap();
            assert!(!index.covers(&paths), "payload mutations invalidate source proofs");
            let _ = fs::remove_dir_all(root);
        }
    }

    #[test]
    fn portable_coverage_declines_missing_unsafe_and_over_budget_payloads() {
        let root = fsx::temp_sibling(&std::env::temp_dir().join("frust-coverage-limits"));
        let artifact = root.join("debug/deps/libapp-0123456789abcdef.rlib");
        fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        fs::write(&artifact, vec![7; 128 * 1024]).unwrap();
        let store = root.join("store");
        let entry = coverage_entry(&store, &artifact, true);
        let paths = [(artifact.clone(), 0)];
        let limited = StoreIndex::new(&store).with_budget(128 * 1024 + 4096, Duration::from_secs(10));
        assert!(!limited.covers(&paths), "decoded verification counts against the budget");
        assert!(!StoreIndex::new(&store).with_budget(u64::MAX, Duration::ZERO).covers(&paths));
        let meta_path = entry.join("meta.json");
        let mut meta: crate::cache::EntryMeta = serde_json::from_slice(&fs::read(&meta_path).unwrap()).unwrap();
        meta.files[0].content_hash = Some("0".repeat(64));
        fs::write(&meta_path, serde_json::to_vec(&meta).unwrap()).unwrap();
        assert!(!StoreIndex::new(&store).covers(&paths), "a metadata hash mismatch cannot cover an artifact");
        meta.files[0].stored = "../../outside".into();
        fs::write(&meta_path, serde_json::to_vec(&meta).unwrap()).unwrap();
        assert!(!StoreIndex::new(&store).covers(&paths), "unsafe paths invalidate entry coverage");
        meta.files[0].stored = "missing".into();
        fs::write(&meta_path, serde_json::to_vec(&meta).unwrap()).unwrap();
        assert!(!StoreIndex::new(&store).covers(&paths), "metadata without a payload never covers");
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn coverage_requires_matching_modes_for_raw_clones_and_encoded_payloads() {
        use std::os::unix::fs::PermissionsExt;
        for compress in [false, true] {
            let root = fsx::temp_sibling(&std::env::temp_dir().join("frust-coverage-modes"));
            let artifact = root.join("debug/deps/app-0123456789abcdef");
            fs::create_dir_all(artifact.parent().unwrap()).unwrap();
            fs::write(&artifact, vec![7; 128 * 1024]).unwrap();
            fs::set_permissions(&artifact, fs::Permissions::from_mode(0o755)).unwrap();
            let entry = coverage_entry(&root.join("store"), &artifact, compress);
            let index = StoreIndex::new(&root.join("store"));
            let paths = [(artifact.clone(), 0)];
            assert!(index.covers(&paths));
            fs::set_permissions(entry.join("f0"), fs::Permissions::from_mode(0o644)).unwrap();
            assert!(!index.covers(&paths), "identical stored bytes with different permissions cannot cover the executable");
            let _ = fs::remove_dir_all(root);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn portable_coverage_prunes_dead_units_while_current_outputs_and_newest_session_stay() {
        for compress in [false, true] {
            let root = fsx::temp_sibling(&std::env::temp_dir().join("frust-covered-maintenance"));
            let dirs = Dirs::at(root.join("home"));
            let target = dirs.targets().join("workspace").join("toolchain");
            let profile = target.join("debug");
            unit_dir(&profile, "app", "aaaaaaaaaaaaaaaa", "bin-app", "0100000000000000", &[], 7);
            unit_dir(&profile, "app", "bbbbbbbbbbbbbbbb", "bin-app", "0200000000000000", &[], 7);
            set_mtime(&profile.join(".fingerprint/app-aaaaaaaaaaaaaaaa"), 30);
            let dead = profile.join("deps/libapp-aaaaaaaaaaaaaaaa.rlib");
            let live = profile.join("deps/libapp-bbbbbbbbbbbbbbbb.rlib");
            fs::write(&dead, vec![7; 128 * 1024]).unwrap();
            coverage_entry(&dirs.store(), &dead, compress);
            let old = profile.join("incremental/app-live/s-0000000001-0000001-0000000000000000000000000");
            let newest = profile.join("incremental/app-live/s-0000000002-0000002-0000000000000000000000000");
            fs::create_dir_all(&old).unwrap();
            fs::create_dir_all(&newest).unwrap();
            fs::write(old.join("dep-graph.bin"), b"old session").unwrap();
            fs::write(old.parent().unwrap().join("s-0000000001-0000001.lock"), b"").unwrap();
            fs::write(newest.parent().unwrap().join("s-0000000002-0000002.lock"), b"").unwrap();
            fs::hard_link(&live, newest.join("current.o")).unwrap();
            File::open(&old).unwrap().set_modified(SystemTime::now() - Duration::from_secs(60)).unwrap();
            let mut opts = AutoOptions::from_env(&Config::from_env());
            opts.enabled = true;
            opts.every = Duration::ZERO;
            opts.compress = false;
            opts.cache_enabled = true;
            opts.uncached_dead_grace = Duration::from_secs(3600);
            opts.dead_incremental_grace = Duration::from_secs(3600);
            let report = auto_compact_with(&dirs, &target, &opts);
            assert_eq!(report.dead_units, 1, "{report:?}");
            assert!(!dead.exists());
            assert!(live.exists());
            assert!(newest.join("current.o").exists());
            assert!(!old.exists(), "ordinary stale-session pruning still applies");
            let _ = fs::remove_dir_all(root);
        }
    }

    #[test]
    fn categories_follow_cargo_layout() {
        let c = |p: &str| Category::of(Path::new(p));
        assert_eq!(c("deps/libserde-0123456789abcdef.rlib"), Category::Rlib);
        assert_eq!(c("deps/libserde-0123456789abcdef.rmeta"), Category::Rmeta);
        assert_eq!(c("deps/serde-0123456789abcdef.serde.abc-cgu.0.rcgu.o"), Category::Objects);
        assert_eq!(c("deps/libserde_derive-0123456789abcdef.dylib"), Category::Dylib);
        assert_eq!(c("deps/app-0123456789abcdef"), Category::DepsExecutables);
        assert_eq!(c("deps/app-0123456789abcdef.d"), Category::DepInfo);
        assert_eq!(c("deps/app-0123456789abcdef.dSYM/Contents/Resources/DWARF/app"), Category::Dsym);
        assert_eq!(c("incremental/app-1x/s-abc/dep-graph.bin"), Category::Incremental);
        assert_eq!(c("build/serde-0123456789abcdef/out/private.rs"), Category::BuildScripts);
        assert_eq!(c(".fingerprint/serde-0123456789abcdef/lib-serde.json"), Category::Fingerprint);
        assert_eq!(c("app"), Category::Uplifted);
        assert_eq!(c("app.d"), Category::DepInfo);
        assert_eq!(c(".cargo-lock"), Category::Other);
    }

    #[test]
    fn artifact_hashes_parse() {
        assert_eq!(artifact_hash("libserde-0123456789abcdef.rlib"), Some("0123456789abcdef"));
        assert_eq!(artifact_hash("serde_json-0123456789abcdef.serde_json.3cca23bbf82c25d5-cgu.0.rcgu.o"), Some("0123456789abcdef"));
        assert_eq!(artifact_hash("app-0123456789abcdef"), Some("0123456789abcdef"));
        assert_eq!(artifact_hash("app"), None);
        assert_eq!(artifact_hash("build-script-build"), None);
    }

    fn ar_header(name: &str, size: usize) -> Vec<u8> {
        let h = format!("{name:<16}{:<12}{:<6}{:<6}{:<8}{size:<10}`\n", 0, 0, 0, 644);
        assert_eq!(h.len(), 60);
        h.into_bytes()
    }

    #[test]
    fn archive_members_read_bsd_and_gnu_names() {
        let dir = std::env::temp_dir().join(format!("frust-ar-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let long = "app-0123456789abcdef.0aaaaaaaaaaaaaaaaaaaaaaaa.1bbbbbb.rcgu.o";
        let mut bsd = b"!<arch>\n".to_vec();
        bsd.extend(ar_header("__.SYMDEF", 4));
        bsd.extend(b"abcd");
        bsd.extend(ar_header(&format!("#1/{}", long.len()), long.len() + 3));
        bsd.extend(long.as_bytes());
        bsd.extend(b"xyz");
        if bsd.len() % 2 == 1 {
            bsd.push(b'\n');
        }
        bsd.extend(ar_header("lib.rmeta/", 2));
        bsd.extend(b"mm");
        fs::write(dir.join("bsd.rlib"), &bsd).unwrap();
        let members = archive_members(&dir.join("bsd.rlib")).unwrap();
        assert_eq!(members, vec!["__.SYMDEF".to_string(), long.to_string(), "lib.rmeta".to_string()]);

        let table = format!("{long}/\nshort.o/\n");
        let mut gnu = b"!<arch>\n".to_vec();
        gnu.extend(ar_header("/", 4));
        gnu.extend(b"0000");
        gnu.extend(ar_header("//", table.len()));
        gnu.extend(table.as_bytes());
        if gnu.len() % 2 == 1 {
            gnu.push(b'\n');
        }
        gnu.extend(ar_header("/0", 1));
        gnu.extend(b"z\n");
        gnu.extend(ar_header(&format!("/{}", long.len() + 2), 2));
        gnu.extend(b"zz");
        fs::write(dir.join("gnu.rlib"), &gnu).unwrap();
        assert_eq!(archive_members(&dir.join("gnu.rlib")).unwrap(), vec![long.to_string(), "short.o".to_string()]);

        fs::write(dir.join("plain"), b"hello").unwrap();
        assert!(archive_members(&dir.join("plain")).is_none());
        fs::write(dir.join("short.rlib"), &bsd[..bsd.len() - 1]).unwrap();
        assert!(archive_members(&dir.join("short.rlib")).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn macho_debug_map_lists_object_stabs() {
        let dir = std::env::temp_dir().join(format!("frust-macho-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let strings = b"\0/t/deps/app-0123456789abcdef.x.1aaaaaa.rcgu.o\0_main\0";
        let symoff = 32 + 24;
        let stroff = symoff + 3 * 16;
        let mut b = vec![0u8; stroff];
        b[0..4].copy_from_slice(&0xfeed_facfu32.to_le_bytes());
        b[16..20].copy_from_slice(&1u32.to_le_bytes());
        b[20..24].copy_from_slice(&24u32.to_le_bytes());
        let cmd = [2u32, 24, symoff as u32, 3, stroff as u32, strings.len() as u32];
        for (i, v) in cmd.iter().enumerate() {
            b[32 + 4 * i..36 + 4 * i].copy_from_slice(&v.to_le_bytes());
        }
        let entries = [(1u32, 0x66u8), (47, 0x0f), (1, 0x24)];
        for (i, (strx, ty)) in entries.iter().enumerate() {
            let at = symoff + 16 * i;
            b[at..at + 4].copy_from_slice(&strx.to_le_bytes());
            b[at + 4] = *ty;
        }
        b.extend_from_slice(strings);
        fs::write(dir.join("app"), &b).unwrap();
        assert_eq!(macho_debug_objects(&dir.join("app")).unwrap(), vec!["/t/deps/app-0123456789abcdef.x.1aaaaaa.rcgu.o".to_string()]);
        fs::write(dir.join("text"), b"#!/bin/sh\n").unwrap();
        assert!(macho_debug_objects(&dir.join("text")).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn elf_debug_strings_list_codegen_units() {
        let dir = std::env::temp_dir().join(format!("frust-elf-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let names = b"\0.shstrtab\0.debug_str\0.debug_line_str\0";
        let strings = b"src/main.rs/@/0abc\0clang LLVM\0library/std/src/lib.rs/@/std.1x\0";
        let packed = b"x/@/compressed\0";
        let mut b = vec![0u8; 64];
        b[0..6].copy_from_slice(b"\x7fELF\x02\x01");
        let mut sections = vec![(0u32, 0u64, 0usize, 0usize)];
        for (name, flags, body) in [(1u32, 0u64, &names[..]), (11, 0, &strings[..]), (22, 0x800, &packed[..])] {
            sections.push((name, flags, b.len(), body.len()));
            b.extend_from_slice(body);
        }
        let shoff = b.len();
        for (name, flags, offset, size) in &sections {
            let mut h = vec![0u8; 64];
            h[0..4].copy_from_slice(&name.to_le_bytes());
            h[8..16].copy_from_slice(&flags.to_le_bytes());
            h[24..32].copy_from_slice(&(*offset as u64).to_le_bytes());
            h[32..40].copy_from_slice(&(*size as u64).to_le_bytes());
            b.extend(h);
        }
        b[0x28..0x30].copy_from_slice(&(shoff as u64).to_le_bytes());
        b[0x3a..0x40].copy_from_slice(&[64, 0, 4, 0, 1, 0]);
        fs::write(dir.join("app"), &b).unwrap();
        assert_eq!(elf_debug_cgus(&dir.join("app")).unwrap(), vec!["0abc".to_string(), "std.1x".to_string()]);
        fs::write(dir.join("text"), b"#!/bin/sh\n").unwrap();
        assert!(elf_debug_cgus(&dir.join("text")).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_objects_keep_what_the_artifacts_name() {
        let root = std::env::temp_dir().join(format!("frust-stale-objects-{}", std::process::id()));
        let deps = root.join("debug").join("deps");
        let session = root.join("debug").join("incremental").join("lib-1abc").join("s-x-y-z");
        fs::create_dir_all(&deps).unwrap();
        fs::create_dir_all(&session).unwrap();
        let stem = "lib-0123456789abcdef";
        let name = |cgu: &str, tag: &str| format!("{stem}.{cgu}.{tag}.rcgu.o");
        for cgu in ["c1", "c2"] {
            fs::write(session.join(format!("{cgu}.o")), vec![1u8; 20_000]).unwrap();
            fs::hard_link(session.join(format!("{cgu}.o")), deps.join(name(cgu, "t2"))).unwrap();
        }
        fs::hard_link(session.join("c1.o"), deps.join(name("c1", "t1"))).unwrap();
        fs::write(deps.join(name("c2", "t1")), vec![2u8; 20_000]).unwrap();
        let mut ar = b"!<arch>\n".to_vec();
        for cgu in ["c1", "c2"] {
            let n = name(cgu, "t2");
            ar.extend(ar_header(&format!("#1/{}", n.len()), n.len()));
            ar.extend(n.as_bytes());
            if n.len() % 2 == 1 {
                ar.push(b'\n');
            }
        }
        fs::write(deps.join(format!("lib{stem}.rlib")), &ar).unwrap();
        fs::write(deps.join("other-1111111111111111.other.abc-cgu.0.rcgu.o"), vec![3u8; 20_000]).unwrap();

        let mut stale = stale_objects(&root.join("debug"));
        stale.sort();
        let paths: Vec<PathBuf> = stale.iter().map(|s| s.0.clone()).collect();
        assert_eq!(paths, vec![deps.join(name("c1", "t1")), deps.join(name("c2", "t1"))]);
        let single = sys::stat(&deps.join(name("c2", "t1"))).unwrap().allocated();
        assert_eq!(stale.iter().map(|s| s.1).sum::<u64>(), single);

        // Without an artifact naming any of them, nothing is proven stale.
        fs::remove_file(deps.join(format!("lib{stem}.rlib"))).unwrap();
        assert!(stale_objects(&root.join("debug")).is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn fingerprints_are_little_endian_hex() {
        assert_eq!(parse_fingerprint("ae144fc74a942c38\n"), Some(4047773213991572654));
        assert_eq!(parse_fingerprint("zz"), None);
    }

    fn unit_dir(profile: &Path, pkg: &str, hash: &str, stem: &str, fingerprint: &str, deps: &[u64], profile_hash: u64) {
        let dir = profile.join(".fingerprint").join(format!("{pkg}-{hash}"));
        fs::create_dir_all(&dir).unwrap();
        let deps: Vec<String> = deps.iter().map(|d| format!("[1,\"x\",false,{d}]")).collect();
        let json = format!(
            "{{\"rustc\":1,\"features\":\"[]\",\"target\":2,\"profile\":{profile_hash},\"path\":3,\"deps\":[{}],\"local\":[],\"rustflags\":[],\"config\":4,\"compile_kind\":0}}",
            deps.join(",")
        );
        fs::write(dir.join(format!("{stem}.json")), json).unwrap();
        fs::write(dir.join(stem), fingerprint).unwrap();
        fs::create_dir_all(profile.join("deps")).unwrap();
        fs::write(profile.join("deps").join(format!("lib{pkg}-{hash}.rlib")), hash).unwrap();
    }

    fn set_mtime(path: &Path, secs_ago: u64) {
        let t = SystemTime::now() - std::time::Duration::from_secs(secs_ago);
        for e in fs::read_dir(path).unwrap().flatten() {
            File::options().write(true).open(e.path()).unwrap().set_modified(t).unwrap();
        }
    }

    #[test]
    fn superseded_units_die_and_referenced_ones_live() {
        let root = std::env::temp_dir().join(format!("frust-compact-unit-{}", std::process::id()));
        let profile = root.join("debug");
        // app v1 is superseded and unreferenced; dep v1 lives through app v2.
        unit_dir(&profile, "dep", "1111111111111111", "lib-dep", "0100000000000000", &[], 7);
        unit_dir(&profile, "dep", "2222222222222222", "lib-dep", "0200000000000000", &[], 7);
        unit_dir(&profile, "app", "aaaaaaaaaaaaaaaa", "bin-app", "0a00000000000000", &[2], 7);
        unit_dir(&profile, "app", "bbbbbbbbbbbbbbbb", "bin-app", "0b00000000000000", &[1], 7);
        // A check-mode unit (different profile hash) is a separate identity and stays.
        unit_dir(&profile, "app", "cccccccccccccccc", "bin-app", "0c00000000000000", &[], 9);
        set_mtime(&profile.join(".fingerprint/dep-1111111111111111"), 300);
        set_mtime(&profile.join(".fingerprint/dep-2222222222222222"), 200);
        set_mtime(&profile.join(".fingerprint/app-aaaaaaaaaaaaaaaa"), 100);
        set_mtime(&profile.join(".fingerprint/app-bbbbbbbbbbbbbbbb"), 10);
        set_mtime(&profile.join(".fingerprint/app-cccccccccccccccc"), 400);
        let a = analyze(&profile).unwrap();
        let live: HashSet<&str> = a.live.iter().map(String::as_str).collect();
        assert_eq!(live, HashSet::from(["1111111111111111", "2222222222222222", "bbbbbbbbbbbbbbbb", "cccccccccccccccc"]));
        assert_eq!(a.dead, HashSet::from(["aaaaaaaaaaaaaaaa".to_string()]));
        assert!(a.removals.iter().any(|(p, _)| p.ends_with("deps/libapp-aaaaaaaaaaaaaaaa.rlib")));
        assert!(a.removals.iter().any(|(p, _)| p.ends_with(".fingerprint/app-aaaaaaaaaaaaaaaa")));
        assert!(!a.removals.iter().any(|(p, _)| p.to_string_lossy().contains("1111111111111111")));

        fs::write(profile.join(".fingerprint/app-aaaaaaaaaaaaaaaa/bin-app.json"), "{").unwrap();
        assert!(analyze(&profile).is_none());
        let _ = fs::remove_dir_all(&root);
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("frust-maint-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn key(n: u64) -> FileKey {
        FileKey { dev: 1, ino: n, size: 20_000, mtime: (n as i64, 5), ctime: (n as i64 + 1, 6) }
    }

    #[test]
    fn memo_round_trips_and_rejects_damage() {
        let mut memo = Memo::default();
        memo.update(key(1), |v| v.hash = Some([7; 32]));
        memo.update(key(2), |v| v.incompressible = Some(99));
        memo.update(key(3), |v| {
            v.hash = Some([9; 32]);
            v.incompressible = Some(40);
        });
        let bytes = memo.encode();
        let back = Memo::decode(&bytes).unwrap();
        assert_eq!(back.entries, memo.entries);

        let mut flipped = bytes.clone();
        flipped[MEMO_HEADER + 3] ^= 1;
        assert!(Memo::decode(&flipped).is_none(), "a damaged record is never trusted");
        assert!(Memo::decode(&bytes[..bytes.len() - 1]).is_none());
        assert!(Memo::decode(b"").is_none());
        let mut other_version = bytes.clone();
        other_version[4] = 2;
        assert!(Memo::decode(&other_version).is_none());
    }

    #[test]
    fn memo_prunes_vanished_files_after_a_complete_pass_only() {
        let dir = scratch("memo-prune");
        let path = dir.join("memo.bin");
        let mut memo = Memo::default();
        memo.update(key(1), |v| v.hash = Some([1; 32]));
        memo.update(key(2), |v| v.hash = Some([2; 32]));
        memo.save(&path, false);

        let mut next = Memo::load(&path);
        assert_eq!(next.entries.len(), 2);
        assert!(next.get(&key(1)).is_some());
        next.save(&path, false);
        assert_eq!(Memo::load(&path).entries.len(), 2, "a stopped pass keeps what it did not reach");
        next.save(&path, true);
        let kept = Memo::load(&path);
        assert_eq!(kept.entries.keys().copied().collect::<Vec<_>>(), vec![key(1)], "key 2 vanished");

        let mut empty = Memo::load(&path);
        empty.save(&path, true);
        assert!(!path.exists(), "an empty memo leaves no file");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn memo_stays_bounded() {
        let mut memo = Memo::default();
        for n in 0..(MAX_MEMO_ENTRIES as u64 + 25) {
            memo.update(key(n), |v| v.incompressible = Some(100));
        }
        let dir = scratch("memo-bound");
        memo.save(&dir.join("memo.bin"), false);
        assert_eq!(Memo::load(&dir.join("memo.bin")).entries.len(), MAX_MEMO_ENTRIES);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn memo_keys_follow_every_change_of_a_file() {
        let dir = scratch("memo-key");
        let file = dir.join("f");
        fs::write(&file, vec![1u8; 20_000]).unwrap();
        let before = FileKey::of(&sys::lstat(&file).unwrap());
        assert_eq!(before, FileKey::of(&sys::lstat(&file).unwrap()));
        std::thread::sleep(Duration::from_millis(30));
        fs::write(&file, vec![2u8; 20_000]).unwrap();
        assert_ne!(before, FileKey::of(&sys::lstat(&file).unwrap()),"a rewrite of equal size changes ctime");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn budget_stops_a_pass_between_files() {
        let mut unlimited = ShrinkCtx::plain();
        unlimited.spend(u64::MAX);
        assert!(!unlimited.exhausted() && !unlimited.stopped);

        let mut by_bytes = ShrinkCtx::plain();
        by_bytes.budget = Some(Budget { deadline: Instant::now() + Duration::from_secs(3600), bytes_left: 100 });
        assert!(!by_bytes.exhausted());
        by_bytes.spend(60);
        assert!(!by_bytes.exhausted());
        by_bytes.spend(60);
        assert!(by_bytes.exhausted() && by_bytes.stopped);

        let mut by_time = ShrinkCtx::plain();
        by_time.budget = Some(Budget { deadline: Instant::now(), bytes_left: u64::MAX });
        assert!(by_time.exhausted() && by_time.stopped);
    }

    #[test]
    fn an_exhausted_budget_leaves_every_file_untouched() {
        let dir = scratch("budget-shrink");
        let mut candidates = Vec::new();
        for name in ["a", "b", "c"] {
            let path = dir.join(name);
            fs::write(&path, vec![5u8; 40_000]).unwrap();
            let meta = sys::lstat(&path).unwrap();
            candidates.push(Candidate { path, meta, in_store: false });
        }
        let inodes: Vec<u64> = candidates.iter().map(|c| c.meta.ino()).collect();
        let mut ctx = ShrinkCtx::plain();
        ctx.budget = Some(Budget { deadline: Instant::now(), bytes_left: 0 });
        let mut report = Report::default();
        shrink(candidates, &CompactOptions::after_build(), &mut ctx, &mut report);
        assert!(ctx.stopped);
        assert_eq!((report.deduped_files, report.compressed_files), (0, 0));
        assert!(report.errors.is_empty());
        for (name, ino) in ["a", "b", "c"].iter().zip(inodes) {
            assert_eq!(sys::stat(&dir.join(name)).unwrap().ino(), ino);
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sampler_reads_the_head_and_the_middle_of_large_files_only() {
        assert!(sample_ranges(4 * SAMPLE_BYTES - 1).is_empty());
        let len = 10 * SAMPLE_BYTES;
        let ranges = sample_ranges(len);
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0], (0, SAMPLE_BYTES));
        assert!(ranges[1].0 >= SAMPLE_BYTES && ranges[1].0 + ranges[1].1 <= len, "the middle sample is disjoint from the head");

        let dir = scratch("sampler");
        let big = dir.join("big");
        fs::write(&big, vec![3u8; len as usize]).unwrap();
        let sampled = std::cell::Cell::new(0usize);
        let same = |b: &[u8]| {
            sampled.set(sampled.get() + b.len());
            Some(b.len())
        };
        assert_eq!(sample_ratio(&big, len, &same), Some(100));
        assert_eq!(sampled.get(), 2 * SAMPLE_BYTES as usize, "only the two samples were read");
        let quarter = |b: &[u8]| Some(b.len() / 4);
        assert_eq!(sample_ratio(&big, len, &quarter), Some(25));
        let small = dir.join("small");
        fs::write(&small, vec![3u8; 1000]).unwrap();
        assert_eq!(sample_ratio(&small, 1000, &same), None, "small files are encoded whole instead");
        let refuses = |_: &[u8]| None::<usize>;
        assert_eq!(sample_ratio(&big, len, &refuses), None);
        let _ = fs::remove_dir_all(&dir);
    }

    fn fake_target(tag: &str) -> (PathBuf, PathBuf) {
        let root = scratch(tag);
        let profile = root.join("debug");
        fs::create_dir_all(profile.join("deps")).unwrap();
        fs::write(profile.join("deps").join("libx-0123456789abcdef.rlib"), vec![1u8; 20_000]).unwrap();
        (root, profile)
    }

    #[test]
    fn change_signature_tracks_the_directories_builds_write_to() {
        let (root, profile) = fake_target("signature");
        let first = tree_signature(&root).unwrap();
        assert_eq!(first, tree_signature(&root).unwrap());

        fs::write(profile.join("deps").join("libx-fedcba9876543210.rlib"), b"new").unwrap();
        let after_deps = tree_signature(&root).unwrap();
        assert_ne!(first, after_deps);

        fs::create_dir_all(profile.join("incremental").join("x-1abc").join("s-1")).unwrap();
        let after_incremental = tree_signature(&root).unwrap();
        assert_ne!(after_deps, after_incremental);
        std::thread::sleep(Duration::from_millis(30));
        fs::create_dir_all(profile.join("incremental").join("x-1abc").join("s-2")).unwrap();
        assert_ne!(after_incremental, tree_signature(&root).unwrap(), "a new session changes its crate directory");

        fs::create_dir_all(profile.join(".fingerprint")).unwrap();
        assert_ne!(after_incremental, tree_signature(&root).unwrap());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn maintenance_locks_release_even_when_their_descriptors_were_duplicated() {
        let (target, profile) = fake_target("lock-release");
        let profile_lock = ProfileLock::try_acquire(&profile).unwrap();
        let inherited_profile = profile_lock._files[0].try_clone().unwrap();
        let marker = target.join(AUTO_MARKER);
        let file = File::options().read(true).write(true).create_new(true).open(&marker).unwrap();
        sys::try_lock(&file, sys::LockKind::Exclusive).unwrap();
        let marker_lock = CompactionLock(file);
        let inherited_marker = marker_lock.try_clone().unwrap();
        let probe = File::options().read(true).write(true).open(&marker).unwrap();
        assert!(sys::try_lock(&probe, sys::LockKind::Exclusive).is_err());
        assert!(ProfileLock::try_acquire(&profile).is_none());
        drop(marker_lock);
        drop(profile_lock);
        assert!(sys::try_lock(&probe, sys::LockKind::Exclusive).is_ok());
        assert!(ProfileLock::try_acquire(&profile).is_some());
        drop((probe, inherited_marker, inherited_profile));
        fs::remove_dir_all(target).unwrap();
    }

    #[test]
    fn change_signature_gives_up_on_unreadable_state_and_depends_on_options() {
        let (root, profile) = fake_target("signature-doubt");
        let dirs = Dirs::at(root.join("home"));
        let a = change_signature(&dirs, &root, "a").unwrap();
        assert_eq!(a, change_signature(&dirs, &root, "a").unwrap());
        assert_ne!(a, change_signature(&dirs, &root, "b").unwrap());
        fs::write(profile.join("incremental"), b"not a directory").unwrap();
        assert!(change_signature(&dirs, &root, "a").is_none(), "a directory that cannot be listed forces a full pass");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn markers_written_before_the_signature_existed_still_parse_as_complete() {
        let marker: AutoMarker = serde_json::from_str("{\"allocated_after\":5,\"at_unix\":7}").unwrap();
        assert!(marker.complete);
        assert_eq!((marker.signature, marker.store_entries), (0, 0));
    }

    #[test]
    fn marker_age_reassessment_uses_the_last_actual_pass() {
        let marker = AutoMarker { at_unix: 10_000, ..AutoMarker::default() };
        assert!(!marker.age_reassessment_due(10_000));
        assert!(!marker.age_reassessment_due(13_599));
        assert!(marker.age_reassessment_due(13_600));
        assert!(marker.age_reassessment_due(9_999), "a backwards clock forces reassessment");
        let unknown: AutoMarker = serde_json::from_str("{\"allocated_after\":5}").unwrap();
        assert!(unknown.age_reassessment_due(10_000), "a missing timestamp forces reassessment");
        assert!(unknown.age_reassessment_due(0), "zero never represents a known pass time");
    }

    #[test]
    fn incomplete_auto_passes_are_retried() {
        let mut report = Report::default();
        assert!(auto_pass_complete(&report));
        report.budget_stopped = true;
        assert!(!auto_pass_complete(&report));
        report.budget_stopped = false;
        report.busy.push(PathBuf::from("debug"));
        assert!(!auto_pass_complete(&report));
        report.busy.clear();
        report.retained_incremental = 1;
        assert!(!auto_pass_complete(&report));
        report.retained_incremental = 0;
        report.retained_cache_entries = 1;
        assert!(!auto_pass_complete(&report));
        report.retained_cache_entries = 0;
        report.errors.push((PathBuf::from("artifact"), "cannot remove".into()));
        assert!(!auto_pass_complete(&report));
    }

    #[cfg(unix)]
    #[test]
    fn unknown_idle_proof_declines_objects_working_links_unreadable_and_future_state() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let root = scratch("unknown-idle-proof");
        let cache = root.join("app-check");
        let session = cache.join("s-complete");
        fs::create_dir_all(&session).unwrap();
        fs::write(session.join("query-cache.bin"), b"metadata reuse state").unwrap();
        let later = SystemTime::now() + Duration::from_secs(8 * 86400);
        assert!(idle_unknown_incremental(&cache, later).is_some_and(|age| age > Duration::from_secs(7 * 86400)));
        assert!(idle_unknown_incremental(&cache, SystemTime::UNIX_EPOCH).is_none());
        fs::write(session.join("unknown.o"), b"unclassified object").unwrap();
        assert!(idle_unknown_incremental(&cache, later).is_none());
        fs::remove_file(session.join("unknown.o")).unwrap();
        fs::create_dir(cache.join("s-other-working")).unwrap();
        assert!(idle_unknown_incremental(&cache, later).is_none());
        fs::remove_dir(cache.join("s-other-working")).unwrap();
        symlink(root.join("missing"), session.join("pointer")).unwrap();
        assert!(idle_unknown_incremental(&cache, later).is_none());
        fs::remove_file(session.join("pointer")).unwrap();
        let permissions = fs::metadata(&session).unwrap().permissions();
        fs::set_permissions(&session, fs::Permissions::from_mode(0o0)).unwrap();
        if fs::read_dir(&session).is_err() {
            assert!(idle_unknown_incremental(&cache, later).is_none());
        }
        fs::set_permissions(&session, permissions).unwrap();
        assert!(idle_unknown_incremental(&cache, later).is_some());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn profile_payload_reclamation_preserves_lock_inodes_and_does_not_follow_links() {
        use std::os::unix::fs::symlink;
        let root = scratch("profile-payload");
        let profile = root.join("debug");
        fs::create_dir_all(profile.join("deps")).unwrap();
        fs::create_dir_all(profile.join(".fingerprint")).unwrap();
        fs::write(profile.join("deps/artifact"), b"payload").unwrap();
        for name in CARGO_LOCKS {
            fs::write(profile.join(name), b"").unwrap();
        }
        let outside = root.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep"), b"outside profile").unwrap();
        symlink(&outside, profile.join("symlink")).unwrap();
        let identity = |path: &Path| {
            let meta = sys::lstat(path).unwrap();
            (meta.dev(), meta.ino())
        };
        let before: Vec<_> = [profile.clone(), profile.join("deps"), profile.join(".fingerprint")].into_iter().chain(CARGO_LOCKS.map(|n| profile.join(n))).map(|p| {
            let id = identity(&p);
            (p, id)
        }).collect();
        let guard = ProfileLock::try_acquire(&profile).unwrap();
        remove_profile_payload(&profile).unwrap();
        for (path, id) in before {
            assert_eq!(identity(&path), id);
        }
        assert!(profile.join("deps").is_dir() && profile.join(".fingerprint").is_dir());
        assert_eq!(fs::read_dir(profile.join("deps")).unwrap().count(), 0);
        assert!(!profile.join("symlink").exists());
        assert_eq!(fs::read(outside.join("keep")).unwrap(), b"outside profile");
        assert!(ProfileLock::try_acquire(&profile).is_none(), "waiting Cargo still sees the held locks");
        drop(guard);
        assert!(ProfileLock::try_acquire(&profile).is_some());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn interrupted_profile_payload_removal_preserves_structural_identity_activity_and_retryability() {
        let root = scratch("profile-payload-retry");
        let profile = root.join("debug");
        let deps = profile.join("deps");
        let fingerprints = profile.join(".fingerprint");
        fs::create_dir_all(&deps).unwrap();
        fs::create_dir_all(&fingerprints).unwrap();
        fs::write(deps.join("a"), b"first payload").unwrap();
        fs::write(deps.join("b"), b"remaining payload").unwrap();
        fs::hard_link(deps.join("a"), profile.join("app")).unwrap();
        fs::write(profile.join("app.d"), b"dep info").unwrap();
        fs::write(fingerprints.join("old"), b"fingerprint").unwrap();
        let guard = ProfileLock::try_acquire(&profile).unwrap();
        let lock_ids = CARGO_LOCKS.map(|name| {
            let path = profile.join(name);
            let meta = sys::lstat(&path).unwrap();
            (path, meta.dev(), meta.ino())
        });
        let old = SystemTime::now() - Duration::from_secs(31 * 86400);
        let mut before = Vec::new();
        for path in [profile.clone(), deps.clone(), fingerprints.clone()] {
            sys::open_directory_for_times(&path).unwrap().set_modified(old).unwrap();
            let meta = sys::lstat(&path).unwrap();
            before.push((path, meta.dev(), meta.ino(), meta.modified().unwrap()));
        }
        let interrupted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            remove_profile_payloads_with(std::slice::from_ref(&profile), |path, _| {
                if path == deps.join("a") {
                    panic!("simulated interruption after a successful unlink");
                }
            }).unwrap();
        }));
        assert!(interrupted.is_err());
        assert!(!profile.join("app").exists() && !profile.join("app.d").exists());
        assert!(deps.join("b").exists() && !deps.join("a").exists());
        for (path, dev, ino, modified) in &before {
            let meta = sys::lstat(path).unwrap();
            assert_eq!((meta.dev(), meta.ino(), meta.modified().unwrap()), (*dev, *ino, *modified));
        }
        remove_profile_payload(&profile).unwrap();
        assert_eq!(fs::read_dir(&deps).unwrap().count(), 0);
        assert_eq!(fs::read_dir(&fingerprints).unwrap().count(), 0);
        for (path, dev, ino, modified) in before {
            let meta = sys::lstat(&path).unwrap();
            assert_eq!((meta.dev(), meta.ino(), meta.modified().unwrap()), (dev, ino, modified));
        }
        for (path, dev, ino) in lock_ids {
            let meta = sys::lstat(&path).unwrap();
            assert_eq!((meta.dev(), meta.ino()), (dev, ino));
        }
        assert!(ProfileLock::try_acquire(&profile).is_none());
        drop(guard);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn profile_lock_reserves_absent_alternative_cargo_names_before_waiters_open_them() {
        let root = scratch("profile-alternative-locks");
        let profile = root.join("debug");
        fs::create_dir_all(profile.join("deps")).unwrap();
        fs::write(profile.join(CARGO_LOCKS[0]), b"existing lock").unwrap();
        let before = sys::lstat(&profile.join(CARGO_LOCKS[0])).unwrap();
        assert!(CARGO_LOCKS[1..].iter().all(|name| !profile.join(name).exists()));
        let guard = ProfileLock::try_acquire(&profile).unwrap();
        let mut waiters = Vec::new();
        for name in CARGO_LOCKS {
            let file = File::options().read(true).write(true).create(true).truncate(false).open(profile.join(name)).unwrap();
            assert!(sys::try_lock(&file, sys::LockKind::Exclusive).is_err(), "an alternative Cargo name is already reserved: {name}");
            waiters.push(file);
        }
        let after = sys::lstat(&profile.join(CARGO_LOCKS[0])).unwrap();
        assert_eq!((before.dev(), before.ino()), (after.dev(), after.ino()));
        assert_eq!(fs::read(profile.join(CARGO_LOCKS[0])).unwrap(), b"existing lock");
        drop(guard);
        for file in waiters {
            sys::try_lock(&file, sys::LockKind::Exclusive).unwrap();
            sys::unlock(&file).unwrap();
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn profile_discovery_and_locks_decline_symlinked_profile_payload_and_lock_paths() {
        use std::os::unix::fs::symlink;
        let root = scratch("profile-symlinks");
        let target = root.join("target");
        let outside = root.join("outside");
        fs::create_dir_all(outside.join("deps")).unwrap();
        fs::write(outside.join("deps/sentinel"), b"outside payload").unwrap();
        fs::create_dir_all(target.join("triple")).unwrap();
        symlink(&outside, target.join("triple/debug")).unwrap();
        assert!(profile_dirs(&target).is_empty());
        assert!(ProfileLock::try_acquire(&target.join("triple/debug")).is_none());
        let profile = target.join("debug");
        fs::create_dir(&profile).unwrap();
        for name in ["deps", ".fingerprint", "incremental", "build", "examples", ".cargo-lock", ".cargo-build-lock", ".cargo-artifact-lock"] {
            symlink(&outside, profile.join(name)).unwrap();
            assert!(ProfileLock::try_acquire(&profile).is_none(), "unsafe {name}");
            assert!(remove_profile_payload(&profile).is_err());
            assert!(profile.join(name).is_symlink());
            fs::remove_file(profile.join(name)).unwrap();
        }
        fs::create_dir(profile.join("incremental")).unwrap();
        symlink(&outside, profile.join("incremental/crate-alias")).unwrap();
        assert!(ProfileLock::try_acquire(&profile).is_some());
        assert!(newest_session(&profile.join("incremental/crate-alias")).is_none());
        assert!(stale_sessions(&profile).is_empty());
        assert!(incremental_dirs(&profile, &Analysis::default()).is_empty());
        let dirs = Dirs::at(root.join("home"));
        fs::create_dir_all(dirs.targets()).unwrap();
        fs::create_dir_all(outside.join("toolchain/debug/deps")).unwrap();
        symlink(&outside, dirs.targets().join("workspace")).unwrap();
        let mut opts = AutoOptions::from_env(&Config::from_env());
        opts.enabled = true;
        let report = auto_compact_with(&dirs, &dirs.targets().join("workspace/toolchain"), &opts);
        assert_eq!(report.skipped, Some("unsafe central target path"));
        assert!(!outside.join("toolchain").join(AUTO_MARKER).exists());
        assert_eq!(fs::read(outside.join("deps/sentinel")).unwrap(), b"outside payload");
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn nested_native_output_links_do_not_block_stale_cleanup_or_touch_external_sessions() {
        use std::os::unix::fs::symlink;
        let root = scratch("native-output-links");
        let dirs = Dirs::at(root.join("home"));
        let target = dirs.targets().join("workspace/toolchain");
        let profile = target.join("debug");
        let outside = root.join("outside");
        let old_session = "s-0000000001-0000001-0000000000000000000000000";
        let current_session = "s-0000000002-0000002-0000000000000000000000000";
        for base in [profile.join("incremental/app-cache"), outside.clone()] {
            for session in [old_session, current_session] {
                fs::create_dir_all(base.join(session)).unwrap();
                fs::write(base.join(session).join("query-cache.bin"), b"preserved metadata").unwrap();
            }
            fs::write(base.join("s-0000000001-0000001.lock"), b"").unwrap();
            fs::write(base.join("s-0000000002-0000002.lock"), b"").unwrap();
            File::open(base.join(old_session)).unwrap().set_modified(SystemTime::now() - Duration::from_secs(60)).unwrap();
        }
        fs::create_dir_all(profile.join("deps")).unwrap();
        fs::create_dir_all(profile.join("build/native/out")).unwrap();
        symlink(&outside, profile.join("build/native/out/library")).unwrap();
        symlink(&outside, profile.join("incremental/foreign-cache")).unwrap();
        let mut opts = AutoOptions::from_env(&Config::from_env());
        opts.enabled = true;
        opts.cache_enabled = false;
        opts.compress = false;
        let report = auto_compact_with(&dirs, &target, &opts);
        assert!(report.errors.is_empty() && report.busy.is_empty(), "{report:?}");
        assert_eq!(report.stale_sessions, 1);
        assert!(!profile.join("incremental/app-cache").join(old_session).exists());
        assert!(profile.join("incremental/app-cache").join(current_session).join("query-cache.bin").exists());
        assert!(profile.join("build/native/out/library").is_symlink());
        assert_eq!(fs::read(outside.join(old_session).join("query-cache.bin")).unwrap(), b"preserved metadata");
        assert_eq!(fs::read(outside.join(current_session).join("query-cache.bin")).unwrap(), b"preserved metadata");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parallel_walk_visits_every_file_once() {
        let dir = scratch("walk");
        let mut expected = 0u64;
        for a in 0..6 {
            for b in 0..3 {
                let leaf = dir.join(format!("a{a}")).join(format!("b{b}")).join("deep");
                fs::create_dir_all(&leaf).unwrap();
                for f in 0..4 {
                    fs::write(leaf.join(format!("f{f}")), [0u8; 10]).unwrap();
                    expected += 1;
                }
            }
            fs::write(dir.join(format!("a{a}")).join("shallow"), [0u8; 10]).unwrap();
            expected += 1;
        }
        fs::write(dir.join("top"), [0u8; 10]).unwrap();
        expected += 1;
        let parts = walk_parallel(&dir, &|_: &str, _: bool| false, &|| 0u64, &|n: &mut u64, _: &Path, _: &sys::Meta| *n += 1);
        assert_eq!(parts.iter().sum::<u64>(), expected);
        let skipping = walk_parallel(&dir, &|name: &str, _: bool| name == "deep", &|| 0u64, &|n: &mut u64, _: &Path, _: &sys::Meta| *n += 1);
        assert_eq!(skipping.iter().sum::<u64>(), 6 + 1);
        let _ = fs::remove_dir_all(&dir);
    }
}
