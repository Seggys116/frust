//! A profile directory is mutated only while holding cargo's own locks, taken without blocking, so one in use is skipped.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::fsx;
use crate::paths::Dirs;
use crate::stats;

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
                if g.path().join("deps").is_dir() {
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

fn tally_of(meta: &fs::Metadata) -> Tally {
    Tally { files: 1, logical: meta.len(), allocated: meta.blocks() * 512, compressed: u64::from(apfs::is_compressed(meta)) }
}

fn walk_files(root: &Path, visit: &mut dyn FnMut(&Path, &fs::Metadata)) {
    let Ok(entries) = fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else { continue };
        if meta.is_dir() {
            walk_files(&path, visit);
        } else if meta.is_file() {
            visit(&path, &meta);
        }
    }
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
            let allocated = meta.blocks() * 512;
            total += match apfs::clone_info(path) {
                Some((group, private)) if !groups.insert((meta.dev(), group)) => private.min(allocated),
                _ => allocated,
            };
        });
    }
    total
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
            let Ok(meta) = fs::symlink_metadata(entry.path()) else { continue };
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

fn same_content(a: &Path, a_meta: &fs::Metadata, b: &Path) -> bool {
    let Ok(b_meta) = fs::symlink_metadata(b) else { return false };
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
                let meta = fs::symlink_metadata(e.path()).ok();
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
        let mut sessions: Vec<(SystemTime, PathBuf)> = fs::read_dir(krate.path())
            .map(|rd| {
                rd.flatten()
                    .filter(|e| {
                        let name = e.file_name().to_string_lossy().into_owned();
                        name.starts_with("s-") && !name.ends_with("-working") && e.path().is_dir()
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
    fs::read_dir(krate)
        .ok()?
        .flatten()
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.starts_with("s-") && !name.ends_with("-working") && e.path().is_dir()
        })
        .map(|e| (e.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH), e.path()))
        .max_by_key(|s| s.0)
}

// Each incremental compile hard-links its objects into `deps/` under a new name and never removes older names; this is the dominant growth.
fn archive_members(path: &Path) -> Option<Vec<String>> {
    use std::os::unix::fs::FileExt;
    let file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let mut magic = [0u8; 8];
    file.read_exact_at(&mut magic, 0).ok()?;
    if &magic != b"!<arch>\n" {
        return None;
    }
    let mut out = Vec::new();
    let mut gnu_names: Vec<u8> = Vec::new();
    let mut pos = 8u64;
    while pos + 60 <= len {
        let mut header = [0u8; 60];
        file.read_exact_at(&mut header, pos).ok()?;
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
            file.read_exact_at(&mut buf, data).ok()?;
            out.push(String::from_utf8_lossy(&buf).trim_end_matches('\0').to_string());
        } else if raw == "//" {
            if size > 64 * 1024 * 1024 {
                return None;
            }
            gnu_names = vec![0u8; size as usize];
            file.read_exact_at(&mut gnu_names, data).ok()?;
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
    use std::os::unix::fs::FileExt;
    const MH_MAGIC_64: u32 = 0xfeed_facf;
    const LC_SYMTAB: u32 = 0x2;
    const N_OSO: u8 = 0x66;
    let file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let u32_at = |b: &[u8], o: usize| b.get(o..o + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]));
    let mut header = [0u8; 32];
    file.read_exact_at(&mut header, 0).ok()?;
    if u32_at(&header, 0)? != MH_MAGIC_64 {
        return None;
    }
    let ncmds = u32_at(&header, 16)?;
    let sizeofcmds = u32_at(&header, 20)? as usize;
    if sizeofcmds as u64 + 32 > len || sizeofcmds > 16 * 1024 * 1024 {
        return None;
    }
    let mut cmds = vec![0u8; sizeofcmds];
    file.read_exact_at(&mut cmds, 32).ok()?;
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
            file.read_exact_at(&mut syms, symoff).ok()?;
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
                file.read_exact_at(&mut buf, stroff + strx).ok()?;
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
        let mut units: HashMap<String, Vec<(String, fs::Metadata)>> = HashMap::new();
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".o") || artifact_hash(&name).is_none() {
                continue;
            }
            let Ok(meta) = e.metadata() else { continue };
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
            let in_use: Vec<&fs::Metadata> = objects.iter().filter(|(n, _)| current.contains(n)).map(|o| &o.1).collect();
            if in_use.is_empty() {
                continue;
            }
            let attributable = in_use.iter().any(|m| m.nlink() > 1);
            for (name, meta) in &objects {
                if current.contains(name) || !(attributable || meta.nlink() == 1) {
                    continue;
                }
                let bytes = if meta.nlink() == 1 { meta.blocks() * 512 } else { 0 };
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
            if let Ok(meta) = e.metadata()
                && meta.is_file()
                && meta.nlink() > 1
            {
                owners.insert((meta.dev(), meta.ino()), hash.to_string());
            }
        }
    }
    let mut out = Vec::new();
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
        if let Ok(files) = fs::read_dir(&session) {
            for f in files.flatten() {
                if !f.file_name().to_string_lossy().ends_with(".o") {
                    continue;
                }
                let Ok(meta) = f.metadata() else { continue };
                objects += 1;
                if let Some(h) = owners.get(&(meta.dev(), meta.ino())) {
                    units.insert(h.clone());
                }
            }
        }
        let owner = if units.iter().any(|h| analysis.live.contains(h)) {
            IncrementalOwner::Live
        } else if !units.is_empty() && units.iter().all(|h| analysis.dead.contains(h)) {
            IncrementalOwner::Dead
        } else if units.is_empty() && objects > 0 {
            IncrementalOwner::Orphan
        } else {
            IncrementalOwner::Unknown
        };
        out.push(IncrementalDir { path, crate_name, owner, units, touched });
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
                total += m.blocks() * 512;
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
            standalone += m.blocks() * 512;
            if !seen.insert((m.dev(), m.ino())) {
                return;
            }
            if path.extension().is_some_and(|e| e == "o") {
                a.newest_objects += m.blocks() * 512;
            } else {
                a.newest_metadata += m.blocks() * 512;
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
            let Ok(m) = fs::symlink_metadata(&path) else { continue };
            a.deps_object_files += 1;
            if !seen.insert((m.dev(), m.ino())) {
                continue;
            }
            if stale.contains(&path) {
                a.deps_objects_stale += m.blocks() * 512;
            } else {
                a.deps_objects_current += m.blocks() * 512;
            }
        }
    }
    a.newest_by_crate.sort();
    a
}

pub struct ProfileLock {
    _files: Vec<File>,
}

impl ProfileLock {
    pub fn try_acquire(profile: &Path) -> Option<ProfileLock> {
        let mut files = Vec::new();
        let existing: Vec<&str> = CARGO_LOCKS.iter().copied().filter(|n| profile.join(n).exists()).collect();
        let names = if existing.is_empty() { vec![CARGO_LOCKS[0]] } else { existing };
        for name in names {
            let file = File::options().read(true).write(true).create(true).truncate(false).open(profile.join(name)).ok()?;
            file.try_lock().ok()?;
            files.push(file);
        }
        Some(ProfileLock { _files: files })
    }
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
        shrink(candidates, opts, &mut report);
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
    meta: fs::Metadata,
    in_store: bool,
}

/// Store `meta.json` files carry the LRU time and are never touched.
fn collect_candidates(root: &Path, in_store: bool, out: &mut Vec<Candidate>) {
    fn visit(dir: &Path, in_store: bool, out: &mut Vec<Candidate>) {
        let Ok(entries) = fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') && name != ".fingerprint" {
                continue;
            }
            let path = entry.path();
            let Ok(meta) = fs::symlink_metadata(&path) else { continue };
            if meta.is_dir() {
                if !name.ends_with("-working") {
                    visit(&path, in_store, out);
                }
                continue;
            }
            if !meta.is_file()
                || meta.nlink() != 1
                || meta.len() < MIN_FILE_BYTES
                || meta.len() > MAX_FILE_BYTES
                || !apfs::only_compression_flag(&meta)
                || (in_store && name == "meta.json")
            {
                continue;
            }
            out.push(Candidate { path, meta, in_store });
        }
    }
    visit(root, in_store, out);
}

fn shrink(candidates: Vec<Candidate>, opts: &CompactOptions, report: &mut Report) {
    let mut by_size: HashMap<u64, Vec<Candidate>> = HashMap::new();
    for c in candidates {
        by_size.entry(c.meta.len()).or_default().push(c);
    }
    let mut dir_times = DirTimes::default();
    for (_, bucket) in by_size {
        if bucket.len() == 1 || !opts.dedupe {
            for c in bucket {
                if opts.compress && !apfs::is_compressed(&c.meta) {
                    compress_one(&c, opts.dry_run, &mut dir_times, report);
                }
            }
            continue;
        }
        // One clone group of compressed files means an earlier pass already handled them.
        let clone_ids: HashSet<Option<u64>> = bucket.iter().map(|c| apfs::clone_info(&c.path).map(|i| i.0)).collect();
        if clone_ids.len() == 1 && !clone_ids.contains(&None) && bucket.iter().all(|c| apfs::is_compressed(&c.meta)) {
            continue;
        }
        let mut groups: HashMap<String, Vec<Candidate>> = HashMap::new();
        for c in bucket {
            match content_hash(&c.path) {
                Ok(h) => groups.entry(h).or_default().push(c),
                Err(err) => report.errors.push((c.path.clone(), err.to_string())),
            }
        }
        for (_, mut group) in groups {
            // Prefer a compressed canonical copy, then one in the store, whose target copies are usually clones of it already.
            group.sort_by_key(|c| (!apfs::is_compressed(&c.meta), !c.in_store, c.path.clone()));
            let mut members = group.into_iter();
            let Some(canonical) = members.next() else { continue };
            if opts.compress && !apfs::is_compressed(&canonical.meta) {
                compress_one(&canonical, opts.dry_run, &mut dir_times, report);
            }
            let canon_meta = fs::symlink_metadata(&canonical.path).ok();
            let canon_clone = apfs::clone_info(&canonical.path).map(|i| i.0);
            for m in members {
                if canon_clone.is_some() && apfs::clone_info(&m.path).map(|i| i.0) == canon_clone {
                    continue;
                }
                let saved = m.meta.blocks() * 512;
                if opts.dry_run {
                    report.deduped_files += 1;
                    report.dedupe_saved += saved;
                    continue;
                }
                let Some(cm) = canon_meta.as_ref() else { continue };
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

fn content_hash(path: &Path) -> io::Result<String> {
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
    Ok(hasher.finalize().to_hex().to_string())
}

/// Directory times are restored after each rename so the rewrite is invisible to tools comparing directory mtimes.
#[derive(Default)]
struct DirTimes(HashMap<PathBuf, (i64, i64, i64, i64)>);

impl DirTimes {
    fn capture(&mut self, file: &Path) -> Option<PathBuf> {
        let dir = file.parent()?.to_path_buf();
        if !self.0.contains_key(&dir) {
            let m = fs::symlink_metadata(&dir).ok()?;
            self.0.insert(dir.clone(), (m.atime(), m.atime_nsec(), m.mtime(), m.mtime_nsec()));
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
fn unchanged(path: &Path, meta: &fs::Metadata) -> bool {
    fs::symlink_metadata(path).is_ok_and(|now| {
        now.ino() == meta.ino() && now.len() == meta.len() && now.mtime() == meta.mtime() && now.mtime_nsec() == meta.mtime_nsec()
    })
}

fn compress_one(c: &Candidate, dry_run: bool, dir_times: &mut DirTimes, report: &mut Report) {
    let data = match fs::read(&c.path) {
        Ok(d) => d,
        Err(err) => {
            report.errors.push((c.path.clone(), err.to_string()));
            return;
        }
    };
    if data.len() as u64 != c.meta.len() {
        return;
    }
    let Some(fork) = apfs::encode_lzfse_fork(&data) else { return };
    let before = c.meta.blocks() * 512;
    let after = (fork.len() as u64).div_ceil(4096) * 4096;
    if after + 8192 > before || after > before - before / 20 {
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
        apfs::set_times(&tmp, (c.meta.atime(), c.meta.atime_nsec()), (c.meta.mtime(), c.meta.mtime_nsec()))?;
        let written = fs::symlink_metadata(&tmp)?;
        if !apfs::is_compressed(&written) {
            return Err(io::Error::other("kernel did not keep the file compressed"));
        }
        if !unchanged(&c.path, &c.meta) {
            return Err(io::Error::other("file changed while compressing"));
        }
        let dir = dir_times.capture(&c.path);
        fs::rename(&tmp, &c.path)?;
        if let Some(dir) = dir {
            dir_times.restore(&dir);
        }
        Ok(written.blocks() * 512)
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
    let meta = fs::symlink_metadata(path)?;
    if !apfs::SUPPORTED || !meta.is_file() || meta.nlink() != 1 || apfs::is_compressed(&meta) {
        return Ok(false);
    }
    let mut report = Report::default();
    compress_one(&Candidate { path: path.to_path_buf(), meta, in_store: false }, false, &mut DirTimes::default(), &mut report);
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
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let meta = fs::symlink_metadata(path)?;
    let data = fs::read(path)?;
    let tmp = fsx::temp_sibling(path);
    let mut dir_times = DirTimes::default();
    let result = (|| -> io::Result<()> {
        let mut file = File::options().write(true).create_new(true).mode(meta.permissions().mode() & 0o7777).open(&tmp)?;
        file.write_all(&data)?;
        drop(file);
        if apfs::SUPPORTED {
            apfs::set_times(&tmp, (meta.atime(), meta.atime_nsec()), (meta.mtime(), meta.mtime_nsec()))?;
        } else {
            File::open(&tmp)?.set_modified(meta.modified()?)?;
        }
        let dir = dir_times.capture(path);
        fs::rename(&tmp, path)?;
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

fn replace_with_clone(canonical: &Path, canon_meta: &fs::Metadata, m: &Candidate, dir_times: &mut DirTimes) -> io::Result<()> {
    if !unchanged(canonical, canon_meta) {
        return Err(io::Error::other("canonical copy changed"));
    }
    let tmp = fsx::temp_sibling(&m.path);
    let result = (|| -> io::Result<()> {
        apfs::clone(canonical, &tmp)?;
        fs::set_permissions(&tmp, m.meta.permissions())?;
        apfs::set_times(&tmp, (m.meta.atime(), m.meta.atime_nsec()), (m.meta.mtime(), m.meta.mtime_nsec()))?;
        if fs::symlink_metadata(&tmp)?.len() != m.meta.len() || !unchanged(&m.path, &m.meta) {
            return Err(io::Error::other("file changed while deduplicating"));
        }
        let dir = dir_times.capture(&m.path);
        fs::rename(&tmp, &m.path)?;
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
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct AutoMarker {
    allocated_after: u64,
    at_unix: u64,
    #[serde(default)]
    store_entries: u64,
}

const STORE_ENTRIES_GROWTH: u64 = 32;

fn store_entries(dirs: &Dirs) -> u64 {
    let Ok(shards) = fs::read_dir(dirs.store()) else { return 0 };
    shards.flatten().map(|s| fs::read_dir(s.path()).map(|rd| rd.count() as u64).unwrap_or(0)).sum()
}

fn write_marker(file: &File, record: &AutoMarker) {
    if let Ok(bytes) = serde_json::to_vec(record) {
        let _ = file.set_len(0);
        let _ = std::os::unix::fs::FileExt::write_all_at(file, &bytes, 0);
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
    let age = |m: &fs::Metadata| m.modified().ok().and_then(|t| SystemTime::now().duration_since(t).ok());
    if let Ok(meta) = fs::metadata(&marker)
        && age(&meta).is_some_and(|a| a < opts.every)
    {
        report.skipped = Some("not due");
        return report;
    }
    let Ok(file) = File::options().read(true).write(true).create(true).truncate(false).open(&marker) else {
        report.skipped = Some("cannot open the marker");
        return report;
    };
    if file.try_lock().is_err() {
        report.skipped = Some("another compaction is running");
        return report;
    }
    let previous: Option<AutoMarker> = fs::read(&marker).ok().and_then(|b| serde_json::from_slice(&b).ok());
    // Another process may have finished a pass between the stat and the lock.
    if previous.is_some() && file.metadata().ok().and_then(|m| age(&m)).is_some_and(|a| a < opts.every) {
        report.skipped = Some("not due");
        return report;
    }
    report.allocated_before = allocated(target);
    if let Some(prev) = &previous
        && report.allocated_before < prev.allocated_after.saturating_add(opts.min_growth)
        && report.allocated_before <= opts.max_target_bytes
    {
        // Every path-crate edit still adds cache entries even when the target is bounded, so prune once enough appear.
        let entries = store_entries(dirs);
        let mut record = AutoMarker { allocated_after: prev.allocated_after, at_unix: prev.at_unix, store_entries: prev.store_entries };
        if entries >= prev.store_entries.saturating_add(STORE_ENTRIES_GROWTH) {
            let superseded = crate::gc::superseded_incremental_entries(dirs, crate::gc::INCREMENTAL_ENTRIES_KEPT);
            report.superseded_entries = superseded.len();
            remove_paths(&superseded, &mut report);
            record.store_entries = store_entries(dirs);
        }
        write_marker(&file, &record);
        let _ = file.set_modified(SystemTime::now());
        report.skipped = Some("grew too little since the last pass");
        report.allocated_after = report.allocated_before;
        return report;
    }

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
    // Removing a cache entry never affects a target directory: restored files are clones, not links.
    let superseded = crate::gc::superseded_incremental_entries(dirs, crate::gc::INCREMENTAL_ENTRIES_KEPT);
    report.superseded_entries = superseded.len();
    remove_paths(&superseded, &mut report);
    let store = if opts.cache_enabled { StoreIndex::new(&dirs.store()) } else { StoreIndex::default() };
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
            remove_paths(&[(profile.clone(), fsx::allocated_size(profile))], &mut report);
            report.demoted_profiles.push(profile.clone());
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
                    if store.covers(&paths) || older(when, grace) {
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
                            if store.covers(&outputs) {
                                report.idle_incremental += 1;
                                doomed.push((d.path.clone(), size()));
                            } else {
                                evictable.push((d.touched, vec![(d.path.clone(), size())]));
                            }
                        }
                        _ => {}
                    }
                }
                if !current {
                    for (hash, paths) in unit_paths(profile, &a.live) {
                        if store.covers(&paths) {
                            evictable.push((a.built.get(&hash).copied().unwrap_or(SystemTime::UNIX_EPOCH), paths));
                        }
                    }
                    if let Ok(crates) = fs::read_dir(profile.join("incremental")) {
                        for krate in crates.flatten() {
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
        // After the sessions are gone, the aliases only they shared are single links.
        let objects = stale_objects(profile);
        report.stale_objects += objects.len();
        remove_paths(&objects, &mut report);
    }

    let mut size = allocated(target);
    if size > opts.max_target_bytes {
        evictable.sort_by_key(|e| e.0);
        for (_, paths) in evictable {
            if size <= opts.max_target_bytes {
                break;
            }
            let live: Vec<(PathBuf, u64)> = paths.into_iter().filter(|(p, _)| p.exists()).collect();
            let bytes: u64 = live.iter().map(|p| p.1).sum();
            let before = report.removed_bytes;
            remove_paths(&live, &mut report);
            report.budget_evicted += report.removed_bytes - before;
            size = size.saturating_sub(bytes);
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
        for (len, paths) in &store.by_size {
            if !sizes.contains(len) {
                continue;
            }
            for path in paths {
                // Compressing a store copy alone would stop it sharing blocks with its target clone.
                if hot.get(len).is_some_and(|ids| apfs::clone_info(path).is_some_and(|(id, _)| ids.contains(&id))) {
                    continue;
                }
                if let Ok(meta) = fs::symlink_metadata(path) {
                    candidates.push(Candidate { path: path.clone(), meta, in_store: true });
                }
            }
        }
        shrink(candidates, &CompactOptions::after_build(), &mut report);
    }
    drop(locks);
    report.allocated_after = allocated(target);
    write_marker(&file, &AutoMarker { allocated_after: report.allocated_after, at_unix: fsx::unix_now(), store_entries: store_entries(dirs) });
    let _ = file.set_modified(SystemTime::now());
    report
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

fn remove_paths(paths: &[(PathBuf, u64)], report: &mut Report) {
    for (path, bytes) in paths {
        // A session inside an incremental directory removed just before is already gone.
        if !path.exists() {
            continue;
        }
        match fsx::remove_any(path) {
            Ok(()) => {
                report.removed_bytes += bytes;
                report.removed.push((path.clone(), *bytes));
            }
            Err(err) => report.errors.push((path.clone(), err.to_string())),
        }
    }
}

fn allocated(root: &Path) -> u64 {
    let mut seen = HashSet::new();
    let mut total = 0;
    walk_files(root, &mut |_, m| {
        if m.nlink() <= 1 || seen.insert((m.dev(), m.ino())) {
            total += m.blocks() * 512;
        }
    });
    total
}

/// Indexes by size to tell which target files are clones of a cache entry without hashing.
#[derive(Default)]
struct StoreIndex {
    by_size: HashMap<u64, Vec<PathBuf>>,
}

impl StoreIndex {
    fn new(store: &Path) -> StoreIndex {
        let mut by_size: HashMap<u64, Vec<PathBuf>> = HashMap::new();
        walk_files(store, &mut |path, meta| {
            if meta.len() > 0 && path.file_name().is_some_and(|n| n != "meta.json") {
                by_size.entry(meta.len()).or_default().push(path.to_path_buf());
            }
        });
        StoreIndex { by_size }
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
            let Ok(meta) = fs::symlink_metadata(path) else { continue };
            if !meta.is_file() {
                continue;
            }
            let Some((group, _)) = apfs::clone_info(path) else { return false };
            let cached = self
                .by_size
                .get(&meta.len())
                .is_some_and(|cands| cands.iter().any(|c| apfs::clone_info(c).is_some_and(|(g, _)| g == group)));
            if !cached {
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
    fn stale_objects_keep_what_the_artifacts_name() {
        use std::os::unix::fs::MetadataExt as _;
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
        let single = fs::metadata(deps.join(name("c2", "t1"))).unwrap().blocks() * 512;
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
            File::open(e.path()).unwrap().set_modified(t).unwrap();
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
}
