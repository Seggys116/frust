//! Direct-mode compile cache: a pre-key selects a manifest of recorded input sets, a match yields an entry key.

pub mod clean;
pub mod depinfo;
pub mod hashing;
pub mod metadata_slice;
pub mod pack;
pub mod payload;
pub mod rustc_args;
pub mod session;

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::events;
use crate::fsx;
use crate::paths::Dirs;
use crate::sys;
use hashing::StampRecord;

/// Replaces the profile directory in stored text so entries are relocatable between target directories.
pub const PROFILE_TOKEN: &str = "{{FRUST_PROFILE_DIR}}";

const MAX_CANDIDATES: usize = 8;

/// Format seven lists entries that keep dep-info and diagnostics inside their metadata; formats three to six remain readable.
const MANIFEST_FORMAT: u32 = 7;

/// Written only for manifests that list a backed entry, so writers that cannot restore one ignore the manifest.
const BACKED_MANIFEST_FORMAT: u32 = 8;

/// The backed format before entries held their text inline.
const LEGACY_BACKED_MANIFEST_FORMAT: u32 = 6;
/// Raw bytes a packed manifest or entry record may unpack to.
pub const MAX_MANIFEST_BYTES: usize = 16 * 1024 * 1024;

/// Text kept inside the entry record stays under this size; anything larger is a separate file in the entry.
pub const INLINE_TEXT_LIMIT: usize = 64 * 1024;

/// A volume where a stamp once lied stays distrusted this long after its latest violation.
const QUARANTINE_TTL: Duration = Duration::from_secs(7 * 24 * 3600);

const VIOLATIONS_LOG: &str = "violations.jsonl";
const VIOLATIONS_MAX_BYTES: u64 = 256 * 1024;
const QUARANTINE_PREFIX: &str = "quarantine-";

// One byte per passed audit; counting stops at the cap so the file stays small.
const AUDITS_FILE: &str = "audits-passed";
const AUDITS_MAX_BYTES: u64 = 16 * 1024 * 1024;

// Entry modification times only order entries for gc, so an hour of granularity is plenty.
const MARK_USED_INTERVAL: Duration = Duration::from_secs(3600);

/// Retry queue for retired entries whose immediate deletion failed.
const EVICTED_DIR: &str = "evicted";

const UNIT_RECORD_MAX_BYTES: u64 = 64 * 1024;

/// Name of the single file under `units/` that holds the records written by this version.
pub const UNIT_PACK: &str = "pack";
const UNIT_PACK_MAX_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredFile {
    pub name: String,
    pub stored: String,
    pub text: bool,
    pub bytes: u64,
    #[serde(default, skip_serializing_if = "payload::Encoding::is_raw")]
    pub encoding: payload::Encoding,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stored_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decoded_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata_slice: Option<metadata_slice::MetadataSlice>,
    /// A directory output (such as a `.dSYM` bundle); entries stored before this field existed read as `false`.
    #[serde(default)]
    pub dir: bool,
    /// Set exactly for `Encoding::Linked` files: the mode the output had, restored instead of whatever the shared inode has now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
    /// The text of a dep-info file kept in the record itself, so the entry holds no file for it; absent for entries that store it as `stored`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inline: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryMeta {
    pub crate_name: String,
    pub version: String,
    pub compile_ms: u64,
    pub created_unix: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incremental_unit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prekey: Option<String>,
    /// `registry`, `git` or `path`; absent for entries stored before attribution or outside a central target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Central-target workspace ids that stored or restored this entry, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspaces: Vec<String>,
    /// The incremental session the compile left, in the entry's `session` directory while no newer state of the unit holds one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<session::StoredSession>,
    pub files: Vec<StoredFile>,
    /// Name of the diagnostics file; empty when the diagnostics are `stderr_inline`, which entries stored before the inline layout never use.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub stderr: String,
    /// Diagnostics kept in the record; omitted when empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub stderr_inline: String,
}

impl EntryMeta {
    /// True when the diagnostics are readable: inline, or a regular file of the entry.
    pub fn stderr_present(&self, entry: &Path) -> bool {
        self.stderr.is_empty() || (safe_file_name(&self.stderr) && sys::lstat(&entry.join(&self.stderr)).is_ok_and(|meta| meta.is_file()))
    }

    pub fn read_stderr(&self, entry: &Path) -> io::Result<String> {
        if self.stderr.is_empty() { Ok(self.stderr_inline.clone()) } else { fs::read_to_string(entry.join(&self.stderr)) }
    }
}

/// One input file of a candidate: its content hash and, when the hash was taken after the file had settled, the stamp it had.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Input {
    pub path: String,
    pub hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stamp: Option<StampRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub inputs: Vec<Input>,
    pub envs: Vec<(String, Option<String>)>,
    pub key: String,
}

// `format` defaults to 0 so garbage and pre-format manifests read as the wrong format, while gc can still list their keys.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Manifest {
    #[serde(default)]
    format: u32,
    candidates: Vec<Candidate>,
}

#[derive(Debug, Clone)]
pub struct Cache {
    dirs: Dirs,
}

impl Cache {
    pub fn new(dirs: &Dirs) -> Cache {
        Cache { dirs: dirs.clone() }
    }

    // Entries, manifests and unit records live directly under their tree.
    fn keyed(root: PathBuf, key: &str) -> PathBuf {
        root.join(key)
    }

    // Older homes filed the same names under a two character directory.
    fn legacy(root: &Path, key: &str) -> PathBuf {
        root.join(&key[..2.min(key.len())]).join(key)
    }

    // The existing flat or legacy location of a name, else the flat location where a new one is written.
    fn resolve(root: PathBuf, key: &str, extension: Option<&str>) -> PathBuf {
        let name = |key: &str| extension.map_or_else(|| key.to_owned(), |ext| format!("{key}.{ext}"));
        let flat = root.join(name(key));
        if fs::symlink_metadata(&flat).is_ok() { return flat; }
        let legacy = root.join(&key[..2.min(key.len())]).join(name(key));
        if fs::symlink_metadata(&legacy).is_ok() { legacy } else { flat }
    }

    pub fn entry_dir(&self, key: &str) -> PathBuf {
        Self::resolve(self.dirs.store(), key, None)
    }

    fn manifest_path(&self, prekey: &str) -> PathBuf {
        Self::resolve(self.dirs.manifests(), prekey, Some("json"))
    }

    fn unit_record_path(units: PathBuf, unit: &str) -> PathBuf {
        Self::resolve(units, unit, None)
    }

    /// Empty for a missing, unreadable or other-format manifest.
    pub fn candidates(&self, prekey: &str) -> Vec<Candidate> {
        self.read_manifest(prekey)
            .map(|m| m.candidates)
            .unwrap_or_default()
    }

    /// The packed record wins; a prekey the log does not hold, or a log that cannot be read, falls back to its per-file manifest.
    fn read_manifest(&self, prekey: &str) -> Option<Manifest> {
        match pack::Log::read(&self.dirs.manifest_pack()) {
            Ok(log) if log.contains(prekey) => Self::packed_manifest(&log, prekey),
            _ => self.read_legacy_manifest(prekey),
        }
    }

    fn packed_manifest(log: &pack::Log, prekey: &str) -> Option<Manifest> {
        parse_manifest(&log.get(prekey, MAX_MANIFEST_BYTES)?.1)
    }

    fn read_legacy_manifest(&self, prekey: &str) -> Option<Manifest> {
        parse_manifest(&fsx::read_packed(&self.manifest_path(prekey), MAX_MANIFEST_BYTES).ok()?)
    }

    /// Writes the manifest of `prekey` into the log, or tombstones it when `manifest` is `None`; the per-file manifest goes once the log holds it.
    fn write_manifest(&self, log: Result<&pack::Log, ()>, prekey: &str, manifest: Option<&Manifest>) {
        let path = self.dirs.manifest_pack();
        let raw = match manifest.map(serde_json::to_vec) {
            Some(Ok(json)) => Some(json),
            Some(Err(_)) => return,
            None => None,
        };
        let packed = log.ok().zip(pack::encode(prekey, fsx::unix_now(), raw.as_deref()))
            .is_some_and(|(log, frame)| pack::commit(&path, log, &frame, crate::lock::shared_mode(&self.dirs)).is_ok());
        let legacy = self.manifest_path(prekey);
        if packed {
            let _ = fs::remove_file(legacy);
        } else if let Some(raw) = raw {
            let _ = fsx::write_atomic(&legacy, &fsx::pack(&raw));
        } else {
            let _ = fs::remove_file(legacy);
        }
    }

    /// Reads `meta.json` for the one marker a manifest format depends on; an unreadable entry counts as not linked.
    fn entry_is_linked(&self, key: &str) -> bool {
        #[derive(Deserialize)]
        struct Files { #[serde(default)] files: Vec<LinkedProbe> }
        #[derive(Deserialize)]
        struct LinkedProbe { #[serde(default)] encoding: payload::Encoding }
        read_entry_meta_bytes(&self.entry_dir(key)).ok()
            .and_then(|bytes| serde_json::from_slice::<Files>(&bytes).ok())
            .is_some_and(|meta| meta.files.iter().any(|file| file.encoding == payload::Encoding::Linked))
    }

    /// Drops candidates whose entry is gone so lookups do not hash their inputs for nothing; the manifest is left alone when nothing changes.
    pub fn add_candidate(&self, prekey: &str, candidate: Candidate) {
        if !valid_key(prekey) || !valid_key(&candidate.key) { return; }
        let Ok(guard) = UnitRecordsGuard::acquire(&self.dirs) else { return };
        self.add_candidate_guarded(&guard, prekey, candidate);
    }

    fn add_candidate_guarded(&self, guard: &UnitRecordsGuard, prekey: &str, candidate: Candidate) {
        if !self.has_entry(&candidate.key) { return; }
        self.mark_used_guarded(guard, &candidate.key);
        let log = pack::Log::read(&self.dirs.manifest_pack()).map_err(|_| ());
        let packed = log.as_ref().is_ok_and(|log| log.contains(prekey));
        let manifest = match &log {
            Ok(log) if packed => Self::packed_manifest(log, prekey),
            _ => self.read_legacy_manifest(prekey),
        };
        // A manifest that ever listed a backed entry stays in the backed format.
        let format = if manifest.as_ref().is_some_and(|m| matches!(m.format, LEGACY_BACKED_MANIFEST_FORMAT | BACKED_MANIFEST_FORMAT))
            || self.entry_is_linked(&candidate.key)
        { BACKED_MANIFEST_FORMAT } else { MANIFEST_FORMAT };
        let current_format = manifest.as_ref().is_some_and(|m| m.format == format);
        let existing = manifest.map(|m| m.candidates).unwrap_or_default();
        let unchanged_front = existing.first() == Some(&candidate);
        let before = existing.len();
        let mut rows = Vec::with_capacity(MAX_CANDIDATES);
        let key = candidate.key.clone();
        rows.push(candidate);
        for old in existing.into_iter().filter(|c| c.key != key) {
            // Rows past the cap are dropped without a stat.
            if rows.len() >= MAX_CANDIDATES {
                break;
            }
            if self.has_entry(&old.key) {
                rows.push(old);
            }
        }
        // An unchanged per-file manifest is still moved into the log.
        if current_format && unchanged_front && rows.len() == before && (packed || log.is_err()) {
            return;
        }
        let manifest = Manifest {
            format,
            candidates: rows,
        };
        self.write_manifest(log.as_ref().map_err(|_| ()), prekey, Some(&manifest));
    }

    /// A stat of `meta.json`, which is written last so its presence means the entry is complete.
    pub fn has_entry(&self, key: &str) -> bool {
        valid_key(key) && self.entry_dir(key).join("meta.json").is_file()
    }

    pub fn load_entry(&self, key: &str) -> Option<(PathBuf, EntryMeta)> {
        if !valid_key(key) {
            return None;
        }
        let dir = self.entry_dir(key);
        let meta = read_entry_meta(&dir)?;
        Some((dir, meta))
    }

    pub fn stage(&self) -> io::Result<PathBuf> {
        let dir = fsx::temp_sibling(&self.dirs.tmp().join("entry"));
        match fs::create_dir(&dir) {
            Ok(()) => Ok(dir),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                fs::create_dir_all(&dir)?;
                Ok(dir)
            }
            Err(err) => Err(err),
        }
    }

    pub fn publish(&self, staged: &Path, key: &str) -> io::Result<()> {
        let result = (|| {
            if !valid_key(key) { return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid entry key")); }
            let _guard = UnitRecordsGuard::acquire(&self.dirs)?;
            let dest = Self::keyed(self.dirs.store(), key);
            let mut renamed = sys::rename(staged, &dest);
            // The store directory is created only when the first rename finds it missing.
            if renamed.as_ref().is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
                && let Some(parent) = dest.parent()
                && fs::create_dir_all(parent).is_ok()
            { renamed = sys::rename(staged, &dest); }
            match renamed {
                Ok(()) => Ok(()),
                Err(_) if dest.join("meta.json").exists() => Ok(()),
                Err(error) => Err(error),
            }
        })();
        // Cleanup can be large and must run after releasing the publication guard.
        if staged.exists() { let _ = fsx::remove_any(staged); }
        result
    }

    pub fn mark_used(&self, key: &str) {
        if !valid_key(key) || self.recently_used(key) { return; }
        let Ok(guard) = UnitRecordsGuard::acquire(&self.dirs) else { return };
        self.mark_used_guarded(&guard, key);
    }

    fn recently_used(&self, key: &str) -> bool {
        fs::metadata(self.entry_dir(key).join("meta.json"))
            .and_then(|m| m.modified()).ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age < MARK_USED_INTERVAL)
    }

    fn mark_used_guarded(&self, _guard: &UnitRecordsGuard, key: &str) {
        if !self.recently_used(key) { fsx::touch(&self.entry_dir(key).join("meta.json")); }
    }

    /// Moves entries, manifests and unit records out of legacy two character directories into their flat trees and removes the emptied directories; a busy metadata lock defers the whole pass.
    pub fn migrate_legacy_layout(&self) {
        let Ok(Some(guard)) = UnitRecordsGuard::try_acquire(&self.dirs) else { return };
        for root in [self.dirs.store(), self.dirs.manifests(), self.units_dir()] {
            let Ok(read) = fs::read_dir(&root) else { continue };
            for shard in read.flatten() {
                let path = shard.path();
                let is_shard = shard.file_name().to_str().is_some_and(is_shard_name)
                    && sys::lstat(&path).is_ok_and(|meta| meta.is_dir() && !meta.file_type().is_symlink());
                if !is_shard { continue; }
                let Ok(children) = fs::read_dir(&path) else { continue };
                for child in children.flatten() {
                    let dest = root.join(child.file_name());
                    match sys::lstat(&dest) {
                        Err(error) if error.kind() == io::ErrorKind::NotFound => { let _ = sys::rename(&child.path(), &dest); }
                        // A complete flat entry supersedes its legacy twin.
                        Ok(_) if root == self.dirs.store() && dest.join("meta.json").is_file() => { let _ = fsx::remove_any(&child.path()); }
                        _ => {}
                    }
                }
                let _ = fs::remove_dir(&path);
            }
        }
        self.fold_legacy_manifests(&guard);
    }

    /// Moves readable per-file manifests into the log, keeping their write time, and drops the directory once nothing is left in it.
    fn fold_legacy_manifests(&self, _guard: &UnitRecordsGuard) {
        let root = self.dirs.manifests();
        let Ok(log) = pack::Log::read(&self.dirs.manifest_pack()) else { return };
        let (mut frames, mut folded, mut seen) = (Vec::new(), Vec::new(), std::collections::HashSet::new());
        for path in keyed_items(&root) {
            let Some(prekey) = path.file_name().and_then(|name| name.to_str()).and_then(|name| name.strip_suffix(".json")).filter(|key| valid_key(key)) else { continue };
            let Ok(meta) = sys::lstat(&path) else { continue };
            if !meta.is_file() || meta.file_type().is_symlink() { continue; }
            if !log.contains(prekey) && seen.insert(prekey.to_owned()) {
                let Some(manifest) = self.read_legacy_manifest(prekey) else { continue };
                let written = meta.modified().ok().and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok()).map_or(0, |age| age.as_secs());
                let Some(frame) = serde_json::to_vec(&manifest).ok().and_then(|json| pack::encode(prekey, written, Some(&json))) else { continue };
                frames.extend_from_slice(&frame);
            }
            folded.push(path);
        }
        if !frames.is_empty() && pack::commit(&self.dirs.manifest_pack(), &log, &frames, crate::lock::shared_mode(&self.dirs)).is_err() { return; }
        for path in folded {
            let _ = fs::remove_file(path);
        }
        let _ = fs::remove_dir(&root);
    }

    /// Every manifest record of the log as `(prekey, written unix seconds, raw json)`; per-file manifests are not included.
    pub fn packed_manifests(&self) -> Vec<(String, u64, Vec<u8>)> {
        let Ok(log) = pack::Log::read(&self.dirs.manifest_pack()) else { return Vec::new() };
        let mut out: Vec<_> = log.keys().filter_map(|prekey| log.get(prekey, MAX_MANIFEST_BYTES).map(|(written, raw)| (prekey.to_owned(), written, raw))).collect();
        out.sort();
        out
    }

    /// Replaces the log record of `prekey` with `raw` as if written at `written_s`, under the metadata lock; for tools and tests that age or rewrite records.
    pub fn put_packed_manifest(&self, prekey: &str, written_s: u64, raw: &[u8]) -> io::Result<()> {
        if !valid_key(prekey) { return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid prekey")); }
        let _guard = UnitRecordsGuard::acquire(&self.dirs)?;
        let path = self.dirs.manifest_pack();
        let log = pack::Log::read(&path)?;
        let frame = pack::encode(prekey, written_s, Some(raw)).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "record too large"))?;
        pack::commit(&path, &log, &frame, crate::lock::shared_mode(&self.dirs))
    }

    /// Rewrites the manifest log without superseded frames; returns the bytes it no longer occupies.
    pub fn compact_manifest_pack(&self) -> u64 {
        let Ok(Some(_guard)) = UnitRecordsGuard::try_acquire(&self.dirs) else { return 0 };
        let path = self.dirs.manifest_pack();
        let Ok(log) = pack::Log::read(&path) else { return 0 };
        if log.dead_bytes() == 0 { return 0; }
        let before = fsx::allocated_size(&path);
        if pack::rewrite(&path, &log, |_| false, crate::lock::shared_mode(&self.dirs)).is_err() { return 0; }
        before.saturating_sub(fsx::allocated_size(&path))
    }

    /// False only when a pack file exists but cannot be trusted.
    pub fn unit_pack_valid(&self) -> bool {
        self.read_unit_pack().is_some()
    }

    /// The pack lines for gc planning; empty when the pack is missing or untrusted.
    pub(crate) fn read_unit_pack_for_gc(&self) -> Option<Vec<(String, u64, Vec<String>)>> {
        let pack = self.read_unit_pack()?;
        (!pack.is_empty()).then(|| pack.into_iter().map(|(unit, (stamp, keys))| (unit, stamp, keys)).collect())
    }

    /// Drops pack records that list no present entry and have not been written for `max_idle`, only while every line still reads as the caller saw it; returns the number removed.
    pub(crate) fn retire_idle_unit_records(&self, expected: &[(String, u64, Vec<String>)], max_idle: Duration) -> usize {
        let Ok(Some(_guard)) = UnitRecordsGuard::try_acquire(&self.dirs) else { return 0 };
        let Some(mut pack) = self.read_unit_pack() else { return 0 };
        let now = fsx::unix_now();
        let mut removed = 0;
        for (unit, stamp, keys) in expected {
            let unchanged = pack.get(unit).is_some_and(|(current, current_keys)| current == stamp && current_keys == keys);
            if unchanged && now.saturating_sub(*stamp) > max_idle.as_secs() && keys.iter().all(|key| self.unit_reference_absent(key)) {
                pack.remove(unit);
                removed += 1;
            }
        }
        if removed > 0 && self.write_unit_pack(&pack).is_err() { return 0; }
        removed
    }

    pub fn units_dir(&self) -> PathBuf {
        self.dirs.root().join(UNITS_DIR)
    }

    /// The file holding every unit record written by this version: one line per unit, `unit unix_seconds key...`, newest key first.
    pub fn unit_pack_path(&self) -> PathBuf {
        self.units_dir().join(UNIT_PACK)
    }

    /// `None` when the pack exists but cannot be trusted (unreadable, oversized or malformed), so callers leave it alone; a missing pack is empty.
    fn read_unit_pack(&self) -> Option<std::collections::BTreeMap<String, (u64, Vec<String>)>> {
        let path = self.unit_pack_path();
        let mut map = std::collections::BTreeMap::new();
        let meta = match sys::lstat(&path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Some(map),
            Err(_) => return None,
        };
        if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > UNIT_PACK_MAX_BYTES { return None; }
        let text = fs::read_to_string(&path).ok()?;
        for line in text.lines() {
            let mut fields = line.split(' ');
            let unit = fields.next().filter(|unit| valid_key(unit))?;
            let stamp = fields.next()?.parse::<u64>().ok()?;
            let keys: Vec<String> = fields.map(str::to_owned).collect();
            if keys.iter().any(|key| !valid_key(key)) { return None; }
            map.insert(unit.to_owned(), (stamp, keys));
        }
        Some(map)
    }

    fn write_unit_pack(&self, map: &std::collections::BTreeMap<String, (u64, Vec<String>)>) -> io::Result<()> {
        let path = self.unit_pack_path();
        if map.is_empty() {
            return match fs::remove_file(&path) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
                _ => Ok(()),
            };
        }
        let mut text = String::new();
        for (unit, (stamp, keys)) in map {
            text.push_str(unit);
            text.push(' ');
            text.push_str(&stamp.to_string());
            for key in keys {
                text.push(' ');
                text.push_str(key);
            }
            text.push('\n');
        }
        crate::lock::write_shared(&path, text.as_bytes(), crate::lock::shared_mode(&self.dirs))
    }

    /// The legacy per-unit file of this unit, if one exists, as its valid keys.
    fn legacy_unit_keys(&self, unit: &str) -> Vec<String> {
        fs::read_to_string(Self::unit_record_path(self.units_dir(), unit))
            .map(|text| text.lines().filter(|line| valid_key(line)).map(str::to_owned).collect())
            .unwrap_or_default()
    }

    /// Every unit record with its last write time in unix seconds: the pack first, then legacy per-unit files for units the pack does not list.
    pub fn unit_records(&self) -> Vec<(String, u64, Vec<String>)> {
        let mut out: Vec<(String, u64, Vec<String>)> = self.read_unit_pack().unwrap_or_default()
            .into_iter().map(|(unit, (stamp, keys))| (unit, stamp, keys)).collect();
        let listed: std::collections::HashSet<String> = out.iter().map(|record| record.0.clone()).collect();
        for path in keyed_items(&self.units_dir()) {
            let Some(unit) = path.file_name().and_then(|name| name.to_str()).filter(|name| valid_key(name)).map(str::to_owned) else { continue };
            if listed.contains(&unit) { continue; }
            let Ok(text) = fs::read_to_string(&path) else { continue };
            let stamp = fs::metadata(&path).and_then(|meta| meta.modified()).ok()
                .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok()).map_or(0, |age| age.as_secs());
            out.push((unit, stamp, text.lines().filter(|line| valid_key(line)).map(str::to_owned).collect()));
        }
        out
    }

    /// The entry keys the unit record lists, newest first; empty when the record is missing or unreadable.
    pub fn unit_keys(&self, unit: &str) -> Vec<String> {
        if !valid_key(unit) { return Vec::new(); }
        match self.read_unit_pack().and_then(|mut pack| pack.remove(unit)) {
            Some((_, keys)) => keys,
            None => self.legacy_unit_keys(unit),
        }
    }

    /// Serialize version commits and retirement renames; returned retired keys are for logging.
    pub fn note_unit_entry(&self, unit: &str, key: &str, keep: usize) -> Vec<String> {
        self.note_unit_entry_with(unit, key, keep, || {}, |_| {}, fsx::remove_any)
    }

    /// A fresh compile of the unit is its newest state whether or not a store follows, so only `keep - 1` earlier entries stay; returned retired keys are for logging.
    pub fn supersede_unit_entries(&self, unit: &str, keep: usize) -> Vec<String> {
        self.record_unit_versions(unit, None, keep.saturating_sub(1), || {}, |_| {}, fsx::remove_any)
    }

    /// As `supersede_unit_entries`, but the retired directories stay queued for [`delete_retired`], so a compile need not wait for their deletion.
    pub fn supersede_unit_entries_queued(&self, unit: &str, keep: usize) -> (Vec<String>, Vec<Retired>) {
        let mut queued = Vec::new();
        let keys = self.record_unit_versions(unit, None, keep.saturating_sub(1), || {}, |_| {}, |path| {
            let (dev, ino) = real_directory(path).ok_or_else(|| io::Error::other("retired entry vanished"))?;
            queued.push(Retired { path: path.to_path_buf(), dev, ino });
            Ok(())
        });
        (keys, queued)
    }

    fn note_unit_entry_with(
        &self, unit: &str, key: &str, keep: usize,
        after_lock: impl FnOnce(), before_commit: impl FnOnce(&Path), delete: impl FnMut(&Path) -> io::Result<()>,
    ) -> Vec<String> {
        self.record_unit_versions(unit, Some(key), keep, after_lock, before_commit, delete)
    }

    fn record_unit_versions(
        &self, unit: &str, key: Option<&str>, keep: usize,
        after_lock: impl FnOnce(), before_commit: impl FnOnce(&Path), mut delete: impl FnMut(&Path) -> io::Result<()>,
    ) -> Vec<String> {
        if !valid_key(unit) || key.is_some_and(|key| !valid_key(key)) { return Vec::new(); }
        let Ok(guard) = UnitRecordsGuard::acquire(&self.dirs) else { return Vec::new() };
        after_lock();
        if key.is_some_and(|key| !self.has_entry(key)) { return Vec::new(); }
        let Some(mut pack) = self.read_unit_pack() else { return Vec::new() };
        let legacy = Self::unit_record_path(self.units_dir(), unit);
        let previous: Vec<String> = match pack.get(unit) {
            Some((_, keys)) => keys.clone(),
            None => match fs::read_to_string(&legacy) {
                Ok(text) => text.lines().filter(|line| valid_key(line)).map(str::to_owned).collect(),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
                Err(_) => return Vec::new(),
            },
        };
        let in_pack = pack.contains_key(unit);
        let mut seen = std::collections::HashSet::new();
        let mut keys: Vec<String> = previous.iter().filter(|old| Some(old.as_str()) != key && self.has_entry(old)
            && seen.insert(old.as_str())).cloned().collect();
        if let Some(key) = key { keys.insert(0, key.to_owned()); }
        let evicted = if keys.len() > keep { keys.split_off(keep) } else { Vec::new() };
        if keys == previous && evicted.is_empty() && (in_pack || !legacy.exists()) { return Vec::new(); }
        let path = self.unit_pack_path();
        before_commit(&path);
        pack.insert(unit.to_owned(), (fsx::unix_now(), keys));
        if self.write_unit_pack(&pack).is_err() { return Vec::new(); }
        // The pack now owns the unit, so its older per-unit file is redundant.
        let _ = fs::remove_file(&legacy);
        let retired: Vec<_> = evicted.into_iter().filter_map(|old| self.retire_entry_guarded(&guard, &old, None).ok().flatten().map(|entry| (old, entry))).collect();
        drop(guard);
        for (_, entry) in &retired {
            if entry.proof.valid(&self.dirs, &entry.path) { let _ = delete(&entry.path); }
        }
        retired.into_iter().map(|(old, _)| old).collect()
    }

    /// Retire by rename, then delete only that unique child; failures remain queued for maintenance.
    pub fn remove_entry(&self, key: &str) {
        self.remove_entry_with(key, fsx::remove_any);
    }

    fn retire_entry_guarded(
        &self, guard: &UnitRecordsGuard, key: &str, expected: Option<&EntryRetirementSnapshot>,
    ) -> io::Result<Option<RetiredEntry>> {
        let Some(current) = self.retirement_snapshot_checked(key)? else { return Ok(None) };
        if expected.is_some_and(|expected| expected != &current) { return Ok(None); }
        let queue = prepare_evicted_queue(&self.dirs)?;
        let doomed = fsx::temp_sibling(&evicted_dir(&self.dirs).join("entry"));
        if !queue.valid(&self.dirs) { return Err(io::Error::other("retirement queue changed before rename")); }
        if self.retirement_snapshot_checked(key)?.as_ref() != Some(&current) { return Ok(None); }
        match sys::rename(&self.entry_dir(key), &doomed) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if self.retirement_snapshot_checked(key)?.as_ref() == Some(&current) { return Err(error); }
                return Ok(None);
            }
            Err(error) => return Err(error),
        }
        let proof = queue.child(&self.dirs, &doomed).ok_or_else(|| io::Error::other("retired child identity unavailable"))?;
        if proof.identity != (current.entry.dev, current.entry.ino) { return Err(io::Error::other("retired child identity changed")); }
        #[derive(Deserialize)]
        struct Reference { prekey: Option<serde_json::Value>, incremental_unit: Option<serde_json::Value> }
        if let Ok(bytes) = read_entry_meta_bytes(&doomed)
            && let Ok(reference) = serde_json::from_slice::<Reference>(&bytes)
        {
            // A malformed ownership hint must not disable cleanup through the other hint.
            if let Some(prekey) = reference.prekey.as_ref().and_then(|value| value.as_str()).filter(|key| valid_key(key)) {
                self.prune_manifest_guarded(guard, prekey);
            }
            if let Some(unit) = reference.incremental_unit.as_ref().and_then(|value| value.as_str()).filter(|key| valid_key(key)) {
                self.prune_unit_record_guarded(guard, unit, key);
            }
        }
        Ok(Some(RetiredEntry { path: doomed, proof }))
    }

    fn prune_unit_record_guarded(&self, guard: &UnitRecordsGuard, unit: &str, retired: &str) {
        if let Some(mut pack) = self.read_unit_pack()
            && let Some((_, keys)) = pack.get(unit)
        {
            if !keys.iter().any(|key| key == retired) { return; }
            let mut seen = std::collections::HashSet::new();
            let kept: Vec<String> = keys.iter().filter(|key| !self.unit_reference_absent(key)).filter(|key| seen.insert(key.as_str())).cloned().collect();
            if kept.len() == keys.len() { return; }
            if kept.is_empty() { pack.remove(unit); } else { pack.insert(unit.to_owned(), (fsx::unix_now(), kept)); }
            let _ = self.write_unit_pack(&pack);
            return;
        }
        self.prune_unit_record_with(guard, unit, retired, |path, contents| match contents {
            Some(contents) => crate::lock::write_shared(path, contents, crate::lock::shared_mode(&self.dirs)),
            None => fs::remove_file(path),
        });
    }

    fn prune_unit_record_with(
        &self, _guard: &UnitRecordsGuard, unit: &str, retired: &str,
        commit: impl FnOnce(&Path, Option<&[u8]>) -> io::Result<()>,
    ) {
        let Some(record) = UnitRecordSnapshot::read(&self.dirs, unit) else { return };
        if !record.keys.iter().any(|key| key == retired) { return; }
        let retained: Vec<_> = record.keys.iter().filter(|key| !self.unit_reference_absent(key)).collect();
        if retained.len() == record.keys.len() { return; }
        let mut seen = std::collections::HashSet::new();
        let keys: Vec<_> = retained.into_iter().filter(|key| seen.insert(key.as_str())).map(String::as_str).collect();
        let contents = (!keys.is_empty()).then(|| format!("{}\n", keys.join("\n")));
        if !record.unchanged(&self.dirs) { return; }
        let _ = commit(&record.path, contents.as_deref().map(str::as_bytes));
    }

    fn prune_manifest_guarded(&self, _guard: &UnitRecordsGuard, prekey: &str) {
        if let Ok(log) = pack::Log::read(&self.dirs.manifest_pack())
            && log.contains(prekey)
        {
            let Some(mut manifest) = Self::packed_manifest(&log, prekey) else { return };
            if manifest.candidates.iter().any(|row| !valid_key(&row.key)) { return; }
            let before = manifest.candidates.len();
            manifest.candidates.retain(|row| !self.reference_absent(&row.key));
            if manifest.candidates.len() == before { return; }
            let kept = (!manifest.candidates.is_empty()).then_some(&manifest);
            self.write_manifest(Ok(&log), prekey, kept);
            return;
        }
        let path = self.manifest_path(prekey);
        if real_directory(&self.dirs.manifests()).is_none()
            || path.parent().and_then(real_directory).is_none()
            || !sys::lstat(&path).is_ok_and(|meta| meta.is_file() && !meta.file_type().is_symlink())
        { return; }
        let Some(mut manifest) = self.read_manifest(prekey) else { return };
        if manifest.candidates.iter().any(|row| !valid_key(&row.key)) { return; }
        let before = manifest.candidates.len();
        manifest.candidates.retain(|row| !self.reference_absent(&row.key));
        if manifest.candidates.len() == before { return; }
        if manifest.candidates.is_empty() { let _ = fs::remove_file(path); }
        else if let Ok(bytes) = serde_json::to_vec(&manifest) { let _ = fsx::write_atomic(&path, &fsx::pack(&bytes)); }
    }

    fn reference_absent(&self, key: &str) -> bool {
        let storage = self.dirs.store();
        let flat = Self::keyed(storage.clone(), key);
        let legacy = Self::legacy(&storage, key);
        prove_absent(&storage, &[&storage, &flat], &flat.join("meta.json"))
            && prove_absent(&storage, &[&storage, legacy.parent().unwrap(), &legacy], &legacy.join("meta.json"))
    }

    /// Unit records retain existing entry roots, including incomplete entries and uncertain filesystem states.
    pub(crate) fn unit_reference_absent(&self, key: &str) -> bool {
        if !valid_key(key) { return false; }
        let storage = self.dirs.store();
        let flat = Self::keyed(storage.clone(), key);
        let legacy = Self::legacy(&storage, key);
        prove_absent(&storage, &[&storage], &flat) && prove_absent(&storage, &[&storage, legacy.parent().unwrap()], &legacy)
    }

    pub(crate) fn retirement_snapshot(&self, key: &str) -> Option<EntryRetirementSnapshot> {
        self.retirement_snapshot_checked(key).ok().flatten()
    }

    fn retirement_snapshot_checked(&self, key: &str) -> io::Result<Option<EntryRetirementSnapshot>> {
        if !valid_key(key) { return Ok(None); }
        let path = self.entry_dir(key);
        let storage = self.dirs.store();
        for directory in [storage.as_path(), path.parent().unwrap(), path.as_path()] {
            match checked_directory(directory) {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error),
            }
        }
        let directory = match sys::lstat(&path) {
            Ok(directory) => directory,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if !directory.is_dir() || directory.file_type().is_symlink() { return Err(io::Error::other("entry changed directory type")); }
        let entry = EntryIdentity::read(&directory).ok_or_else(|| io::Error::other("entry timestamps unavailable"))?;
        let meta = match sys::lstat(&path.join("meta.json")) {
            Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() => Some(EntryIdentity::read(&meta).ok_or_else(|| io::Error::other("entry metadata timestamps unavailable"))?),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
            Ok(_) => return Err(io::Error::other("entry metadata is not a regular file")),
        };
        Ok(Some(EntryRetirementSnapshot { key: key.to_owned(), entry, meta }))
    }

    pub(crate) fn try_retire_entry(&self, expected: &EntryRetirementSnapshot) -> io::Result<EntryRetirementOutcome> {
        let Some(guard) = UnitRecordsGuard::try_acquire(&self.dirs)? else { return Ok(EntryRetirementOutcome::Busy) };
        let retired = self.retire_entry_guarded(&guard, &expected.key, Some(expected))?;
        drop(guard);
        let Some(entry) = retired else { return Ok(EntryRetirementOutcome::Stale) };
        if !entry.proof.valid(&self.dirs, &entry.path) { return Err(io::Error::other("retired child changed before deletion")); }
        fsx::remove_any(&entry.path)?;
        Ok(EntryRetirementOutcome::Retired)
    }

    fn remove_entry_with(&self, key: &str, delete: impl FnOnce(&Path) -> io::Result<()>) {
        if !valid_key(key) { return; }
        let Ok(guard) = UnitRecordsGuard::acquire(&self.dirs) else { return };
        let retired = self.retire_entry_guarded(&guard, key, None).ok().flatten();
        drop(guard);
        if let Some(entry) = retired && entry.proof.valid(&self.dirs, &entry.path) { let _ = delete(&entry.path); }
    }

}

/// Every item of a keyed tree: the flat names and the contents of legacy two character shard directories.
pub fn keyed_items(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(read) = fs::read_dir(root) else { return out };
    for item in read.flatten() {
        let is_shard = item.file_name().to_str().is_some_and(is_shard_name) && item.file_type().is_ok_and(|kind| kind.is_dir());
        if !is_shard {
            out.push(item.path());
        } else if let Ok(inner) = fs::read_dir(item.path()) {
            out.extend(inner.flatten().map(|file| file.path()));
        }
    }
    out
}

/// True for the two character directory names older homes used to shard entries.
pub fn is_shard_name(name: &str) -> bool {
    name.len() == 2 && name.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

// Walks the chain from the parent of its first path, which must be real directories until one is missing, then proves the leaf absent and the parents unchanged.
fn prove_absent(first: &Path, chain: &[&Path], leaf: &Path) -> bool {
    let Some(home) = first.parent() else { return false };
    let Ok(meta) = sys::stat(home) else { return false };
    if !meta.is_dir() { return false; }
    let mut parents = vec![(home, true, meta.dev(), meta.ino())];
    let mut absent = leaf;
    for path in chain {
        match sys::lstat(path) {
            Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => parents.push((path, false, meta.dev(), meta.ino())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => { absent = path; break; }
            _ => return false,
        }
    }
    // Recheck proven parent identities before treating portable NotFound as absence.
    if !matches!(sys::lstat(absent), Err(error) if error.kind() == io::ErrorKind::NotFound) { return false; }
    parents.iter().all(|&(path, follow, dev, ino)| {
        let meta = if follow { sys::stat(path) } else { sys::lstat(path) };
        meta.is_ok_and(|meta| meta.is_dir() && (follow || !meta.file_type().is_symlink()) && meta.dev() == dev && meta.ino() == ino)
    })
}

/// A per-codegen-unit object of an incremental compile, whose stale names rustc leaves in the target (split debuginfo).
pub fn is_incremental_object(name: &str) -> bool {
    name.ends_with(".rcgu.o") && name.split('.').count() == 5 && !name.contains("-cgu.")
}

/// True for an entry with linked files, every one of which has lost its target name, so the store holds the only copy; a stale incremental object name left in a target does not count as live.
pub fn orphaned_linked(entry: &Path, meta: &EntryMeta) -> bool {
    let mut linked = meta.files.iter().filter(|file| file.encoding == payload::Encoding::Linked && safe_file_name(&file.stored)).peekable();
    linked.peek().is_some()
        && linked.all(|file| {
            sys::lstat(&entry.join(&file.stored)).is_ok_and(|stored| stored.is_file() && (stored.nlink() == 1 || is_incremental_object(&file.name)))
        })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EntryIdentity {
    dev: u64,
    ino: u64,
    len: u64,
    modified: SystemTime,
    changed: (i64, i64),
}

impl EntryIdentity {
    fn read(meta: &sys::Meta) -> Option<Self> {
        Some(Self { dev: meta.dev(), ino: meta.ino(), len: meta.len(), modified: meta.modified().ok()?, changed: meta.ctime() })
    }
}

struct UnitRecordSnapshot {
    path: PathBuf,
    root: (u64, u64),
    units: (u64, u64),
    parent: (u64, u64),
    identity: EntryIdentity,
    keys: Vec<String>,
}

impl UnitRecordSnapshot {
    fn read(dirs: &Dirs, unit: &str) -> Option<Self> {
        if !valid_key(unit) { return None; }
        let path = Cache::unit_record_path(dirs.root().join(UNITS_DIR), unit);
        let root = sys::stat(dirs.root()).ok()?;
        if !root.is_dir() { return None; }
        let units = checked_directory(&dirs.root().join(UNITS_DIR)).ok()?;
        let parent = checked_directory(path.parent()?).ok()?;
        let meta = sys::lstat(&path).ok()?;
        if !meta.is_file() || meta.file_type().is_symlink() { return None; }
        let identity = EntryIdentity::read(&meta)?;
        if identity.len > UNIT_RECORD_MAX_BYTES { return None; }
        let mut file = File::open(&path).ok()?;
        if EntryIdentity::read(&sys::fstat(&file).ok()?).as_ref() != Some(&identity) { return None; }
        let mut contents = String::new();
        (&mut file).take(UNIT_RECORD_MAX_BYTES + 1).read_to_string(&mut contents).ok()?;
        if contents.len() as u64 > UNIT_RECORD_MAX_BYTES { return None; }
        let keys: Vec<String> = if contents == "\n" { Vec::new() } else { contents.lines().map(str::to_owned).collect() };
        if keys.iter().any(|key| !valid_key(key)) || contents != format!("{}\n", keys.join("\n")) { return None; }
        let record = Self { path, root: (root.dev(), root.ino()), units, parent, identity, keys };
        record.unchanged(dirs).then_some(record)
    }

    fn unchanged(&self, dirs: &Dirs) -> bool {
        // The global guard serializes writers; these identities reject changed paths and unsafe boundaries.
        sys::stat(dirs.root()).is_ok_and(|meta| meta.is_dir() && (meta.dev(), meta.ino()) == self.root)
            && checked_directory(&dirs.root().join(UNITS_DIR)).ok() == Some(self.units)
            && self.path.parent().and_then(|path| checked_directory(path).ok()) == Some(self.parent)
            && sys::lstat(&self.path).is_ok_and(|meta| meta.is_file() && !meta.file_type().is_symlink()
                && EntryIdentity::read(&meta).as_ref() == Some(&self.identity))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EntryRetirementOutcome {
    Busy,
    Stale,
    Retired,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EntryRetirementSnapshot {
    key: String,
    entry: EntryIdentity,
    meta: Option<EntryIdentity>,
}

impl EntryRetirementSnapshot {
    pub(crate) fn used(&self) -> SystemTime { self.meta.as_ref().map_or(SystemTime::UNIX_EPOCH, |meta| meta.modified) }
}

pub(crate) struct UnitRecordsGuard(File);

impl UnitRecordsGuard {
    fn open(dirs: &Dirs) -> io::Result<File> {
        let mode = crate::lock::shared_mode(dirs);
        crate::lock::ensure_dir(&dirs.keylocks(), mode)?;
        // This stable name stays outside per-prekey lock garbage collection.
        crate::lock::open_lock_file(&dirs.keylocks().join("unit-records"), mode)
    }

    fn acquire(dirs: &Dirs) -> io::Result<Self> {
        let file = Self::open(dirs)?;
        sys::lock(&file, sys::LockKind::Exclusive)?;
        Ok(Self(file))
    }

    pub(crate) fn try_acquire(dirs: &Dirs) -> io::Result<Option<Self>> {
        let file = Self::open(dirs)?;
        match sys::try_lock(&file, sys::LockKind::Exclusive) {
            Ok(()) => Ok(Some(Self(file))),
            Err(fs::TryLockError::WouldBlock) => Ok(None),
            Err(fs::TryLockError::Error(error)) => Err(error),
        }
    }
}

impl Drop for UnitRecordsGuard {
    fn drop(&mut self) { let _ = sys::unlock(&self.0); }
}

struct RetiredEntry {
    path: PathBuf,
    proof: RetiredChildProof,
}

/// A retired entry directory waiting in the eviction queue, named with the identity it had when it was queued.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retired {
    pub path: PathBuf,
    pub dev: u64,
    pub ino: u64,
}

/// Deletes a queued entry only while it is still the same directory in the same queue; anything else stays for maintenance.
pub fn delete_retired(dirs: &Dirs, retired: &Retired) -> bool {
    let Some(queue) = QueueProof::capture(dirs) else { return false };
    let Some(proof) = queue.child(dirs, &retired.path) else { return false };
    proof.identity == (retired.dev, retired.ino) && proof.valid(dirs, &retired.path) && fsx::remove_any(&retired.path).is_ok()
}

pub fn evicted_dir(dirs: &Dirs) -> PathBuf {
    dirs.tmp().join(EVICTED_DIR)
}

// Path identity checks detect observed replacements; deletion still uses path-based APIs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QueueProof {
    tmp: (u64, u64),
    queue: (u64, u64),
}

#[derive(Debug, Clone)]
pub(crate) struct RetiredChildProof {
    queue: QueueProof,
    identity: (u64, u64),
}

fn real_directory(path: &Path) -> Option<(u64, u64)> {
    let meta = sys::lstat(path).ok()?;
    (meta.is_dir() && !meta.file_type().is_symlink()).then_some((meta.dev(), meta.ino()))
}

fn retired_child_name(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else { return false };
    let Some(suffix) = name.strip_prefix(&format!(".entry{}", fsx::TEMP_MARKER)) else { return false };
    let parts: Vec<_> = suffix.split('-').collect();
    parts.len() == 3
        && parts.iter().all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
        && parts[0].parse::<u32>().is_ok_and(|pid| pid > 0 && pid.to_string() == parts[0])
        && parts[1].parse::<u64>().is_ok_and(|counter| counter.to_string() == parts[1])
        && parts[2].parse::<u32>().is_ok_and(|nanos| nanos < 1_000_000_000 && nanos.to_string() == parts[2])
}

impl QueueProof {
    pub(crate) fn capture(dirs: &Dirs) -> Option<Self> {
        Some(Self { tmp: real_directory(&dirs.tmp())?, queue: real_directory(&evicted_dir(dirs))? })
    }

    fn valid(&self, dirs: &Dirs) -> bool {
        Self::capture(dirs).as_ref() == Some(self)
    }

    pub(crate) fn child(&self, dirs: &Dirs, path: &Path) -> Option<RetiredChildProof> {
        if path.parent() != Some(evicted_dir(dirs).as_path()) || !retired_child_name(path) || !self.valid(dirs) {
            return None;
        }
        Some(RetiredChildProof { queue: self.clone(), identity: real_directory(path)? })
    }
}

impl RetiredChildProof {
    pub(crate) fn valid(&self, dirs: &Dirs, path: &Path) -> bool {
        self.queue.valid(dirs) && path.parent() == Some(evicted_dir(dirs).as_path())
            && retired_child_name(path) && real_directory(path) == Some(self.identity)
    }
}

// Reject links and wrong types before using a retirement boundary.
fn checked_directory(path: &Path) -> io::Result<(u64, u64)> {
    let meta = sys::lstat(path)?;
    if !meta.is_dir() || meta.file_type().is_symlink() { return Err(io::Error::other("retirement boundary is not a real directory")); }
    Ok((meta.dev(), meta.ino()))
}

fn prepare_evicted_queue(dirs: &Dirs) -> io::Result<QueueProof> {
    let mut boundaries = Vec::with_capacity(2);
    for path in [dirs.tmp(), evicted_dir(dirs)] {
        match sys::lstat(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match fs::create_dir(&path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error),
                }
            }
            Err(error) => return Err(error),
            Ok(_) => {}
        }
        boundaries.push(checked_directory(&path)?);
    }
    let proof = QueueProof { tmp: boundaries[0], queue: boundaries[1] };
    if !proof.valid(dirs) { return Err(io::Error::other("retirement queue changed while preparing")); }
    Ok(proof)
}

/// Deletes evicted entries that were removed from the store at least `older_than` ago and returns how many were deleted.
pub fn sweep_evicted(dirs: &Dirs, older_than: Duration) -> usize {
    let Some(queue) = QueueProof::capture(dirs) else { return 0 };
    let Ok(entries) = fs::read_dir(evicted_dir(dirs)) else { return 0 };
    let now = fsx::unix_now() as i64;
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(proof) = queue.child(dirs, &path) else { continue };
        let Ok(meta) = sys::lstat(&path) else { continue };
        // The rename updates the directory's ctime, which is the time it left the store.
        let evicted_at = meta.mtime().0.max(meta.ctime().0);
        if now - evicted_at < older_than.as_secs() as i64 {
            continue;
        }
        if proof.valid(dirs, &path) && fs::remove_dir_all(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

pub const UNITS_DIR: &str = "units";

/// Keys read back from disk are checked before they name a path.
pub fn valid_key(key: &str) -> bool {
    (32..=64).contains(&key.len())
        && key
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// How far recorded file stamps are believed; see `FRUST_CACHE_TRUST_STAMPS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustMode {
    Off,
    On,
    Audit,
}

impl TrustMode {
    /// Unknown values mean `On`; the spellings of `Off` are the ones every other `FRUST_*` switch uses.
    pub fn parse(value: Option<&str>) -> TrustMode {
        match value {
            Some("0" | "off" | "false" | "no") => TrustMode::Off,
            Some("audit") => TrustMode::Audit,
            _ => TrustMode::On,
        }
    }

    pub fn from_env() -> TrustMode {
        TrustMode::parse(std::env::var("FRUST_CACHE_TRUST_STAMPS").ok().as_deref())
    }

    pub fn name(self) -> &'static str {
        match self {
            TrustMode::Off => "off",
            TrustMode::On => "on",
            TrustMode::Audit => "audit",
        }
    }
}

fn quarantine_marker(dirs: &Dirs, dev: u64) -> PathBuf {
    dirs.stamp_trust().join(format!("{QUARANTINE_PREFIX}{dev}"))
}

fn quarantine_active(modified: SystemTime, now: SystemTime) -> bool {
    match now.duration_since(modified) {
        Ok(age) => age < QUARANTINE_TTL,
        // A marker dated in the future (the clock moved) still counts.
        Err(_) => true,
    }
}

/// The time of the latest violation on this volume while its quarantine lasts; one stat. A marker that cannot be examined counts as fresh.
pub fn quarantined_since(dirs: &Dirs, dev: u64, now: SystemTime) -> Option<SystemTime> {
    let modified = match fs::metadata(quarantine_marker(dirs, dev)) {
        Ok(meta) => meta.modified().unwrap_or(now),
        Err(err) if err.kind() == io::ErrorKind::NotFound => return None,
        Err(_) => return Some(now),
    };
    quarantine_active(modified, now).then_some(modified)
}

pub fn quarantined(dirs: &Dirs, dev: u64) -> bool {
    quarantined_since(dirs, dev, SystemTime::now()).is_some()
}

/// Renaming a new file over the marker restarts the seven days on every violation.
pub fn quarantine(dirs: &Dirs, dev: u64) {
    let _ = fsx::write_atomic(
        &quarantine_marker(dirs, dev),
        format!("{}\n", fsx::unix_now()).as_bytes(),
    );
}

/// Returns the file's length after the write, or `None` if it could not be written.
fn append_bytes(path: &Path, bytes: &[u8]) -> Option<u64> {
    let open = || OpenOptions::new().create(true).append(true).open(path);
    let mut file = match open() {
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            open().ok()?
        }
        other => other.ok()?,
    };
    file.write_all(bytes).ok()?;
    file.metadata().ok().map(|m| m.len())
}

/// The longest suffix of whole lines within `budget` bytes.
fn newest_lines(text: &str, budget: usize) -> &str {
    let mut start = text.len();
    for line in text.split_inclusive('\n').rev() {
        if text.len() - start + line.len() > budget {
            break;
        }
        start -= line.len();
    }
    &text[start..]
}

/// A stamp matched but the content differs: the volume stops being trusted and the event is logged, oldest lines dropped past the cap.
pub fn report_violation(
    dirs: &Dirs,
    krate: &str,
    input: &str,
    recorded: &str,
    fresh: &str,
    dev: u64,
) {
    quarantine(dirs, dev);
    let record = serde_json::json!({
        "t": events::now_ms(),
        "crate": krate,
        "input": input,
        "recorded": recorded,
        "fresh": fresh,
        "dev": dev,
    });
    let log = dirs.stamp_trust().join(VIOLATIONS_LOG);
    if append_bytes(&log, format!("{record}\n").as_bytes())
        .is_some_and(|len| len > VIOLATIONS_MAX_BYTES)
        && let Ok(text) = fs::read_to_string(&log)
    {
        let _ = fsx::write_atomic(
            &log,
            newest_lines(&text, VIOLATIONS_MAX_BYTES as usize / 2).as_bytes(),
        );
    }
}

/// One byte per audited input that agreed with its stamp; `frust doctor` reports the total.
pub fn note_audits_passed(dirs: &Dirs, count: usize) {
    let path = dirs.stamp_trust().join(AUDITS_FILE);
    if count == 0 || fs::metadata(&path).is_ok_and(|m| m.len() >= AUDITS_MAX_BYTES) {
        return;
    }
    let _ = append_bytes(&path, &vec![b'.'; count]);
}

#[derive(Debug, Default)]
pub struct TrustSummary {
    /// Volumes (by device number) still quarantined, with the time of their latest violation.
    pub quarantined: Vec<(u64, SystemTime)>,
    pub violations: usize,
    pub audits_passed: u64,
}

pub fn trust_summary(dirs: &Dirs, now: SystemTime) -> TrustSummary {
    let mut summary = TrustSummary::default();
    for entry in fs::read_dir(dirs.stamp_trust())
        .into_iter()
        .flatten()
        .flatten()
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(dev) = name
            .strip_prefix(QUARANTINE_PREFIX)
            .and_then(|d| d.parse::<u64>().ok())
        else {
            continue;
        };
        if let Ok(modified) = entry.metadata().and_then(|m| m.modified())
            && quarantine_active(modified, now)
        {
            summary.quarantined.push((dev, modified));
        }
    }
    summary.quarantined.sort();
    summary.violations = fs::read_to_string(dirs.stamp_trust().join(VIOLATIONS_LOG))
        .map_or(0, |text| text.lines().count());
    summary.audits_passed =
        fs::metadata(dirs.stamp_trust().join(AUDITS_FILE)).map_or(0, |m| m.len());
    summary
}

fn parse_manifest(bytes: &[u8]) -> Option<Manifest> {
    let manifest: Manifest = serde_json::from_slice(bytes).ok()?;
    matches!(manifest.format, 3 | 4 | 5 | LEGACY_BACKED_MANIFEST_FORMAT | MANIFEST_FORMAT | BACKED_MANIFEST_FORMAT).then_some(manifest)
}

/// Lists keys of manifests in any format: the reader ignores `format`, and fields it does not know (a stamp's birth time) or lacks are skipped or defaulted.
pub fn manifest_keys(path: &Path) -> Option<Vec<String>> {
    let manifest: Manifest = serde_json::from_slice(&fsx::read_packed(path, MAX_MANIFEST_BYTES).ok()?).ok()?;
    Some(manifest.candidates.into_iter().map(|c| c.key).collect())
}

/// The raw `meta.json` of an entry directory, packed or written plain by an older frust.
pub fn read_entry_meta_bytes(dir: &Path) -> std::io::Result<Vec<u8>> {
    fsx::read_packed(&dir.join("meta.json"), MAX_MANIFEST_BYTES)
}

pub fn read_entry_meta(dir: &Path) -> Option<EntryMeta> {
    serde_json::from_slice(&read_entry_meta_bytes(dir).ok()?).ok()
}

pub fn hash_file(path: &Path) -> std::io::Result<String> {
    hashing::hash_reader(&mut fs::File::open(path)?)
}

pub fn hash_str(text: &str) -> String {
    blake3::hash(text.as_bytes()).to_hex()[..32].to_string()
}

/// Entry metadata is read from disk, so a crafted `meta.json` must not write outside the output or entry directory.
pub fn safe_file_name(name: &str) -> bool {
    let mut components = Path::new(name).components();
    matches!(components.next(), Some(std::path::Component::Normal(_)))
        && components.next().is_none()
        && !name.contains('/')
        && !name.contains('\0')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packed_text(cache: &Cache, prekey: &str) -> String {
        let log = pack::Log::read(&cache.dirs.manifest_pack()).unwrap();
        String::from_utf8(log.get(prekey, MAX_MANIFEST_BYTES).unwrap().1).unwrap()
    }

    fn manifest_present(cache: &Cache, prekey: &str) -> bool {
        pack::Log::read(&cache.dirs.manifest_pack()).unwrap().contains(prekey) || cache.manifest_path(prekey).exists()
    }

    fn key(n: u8) -> String {
        format!("{n:02x}").repeat(20)
    }

    fn scratch(tag: &str) -> (PathBuf, Cache) {
        let dir = std::env::temp_dir().join(format!("frust-units-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let cache = Cache::new(&Dirs::at(&dir));
        (dir, cache)
    }

    #[test]
    fn a_stale_incremental_object_name_does_not_keep_an_entry_from_being_orphaned() {
        let (dir, _) = scratch("orphan-objects");
        let (entry, target) = (dir.join("entry"), dir.join("target"));
        fs::create_dir_all(&entry).unwrap();
        fs::create_dir_all(&target).unwrap();
        let linked = |name: &str, stored: &str| StoredFile {
            name: name.into(), stored: stored.into(), text: false, bytes: 8192, encoding: payload::Encoding::Linked,
            stored_bytes: None, decoded_bytes: None, content_hash: None, metadata_slice: None, dir: false, mode: None, inline: None,
        };
        let object = "k-0123456789abcdef.0a1b2c3d4e5f.1x2y3z.rcgu.o";
        let meta = EntryMeta {
            crate_name: "k".into(), version: "0.1.0".into(), compile_ms: 1, created_unix: 0, incremental_unit: None, prekey: None, session: None,
            source: None, workspaces: Vec::new(), files: vec![linked("libk.rlib", "f0"), linked(object, "f1")], stderr: "stderr".into(), stderr_inline: String::new(),
        };
        for (stored, name) in [("f0", "libk.rlib"), ("f1", object)] {
            fs::write(entry.join(stored), vec![1u8; 8192]).unwrap();
            fs::hard_link(entry.join(stored), target.join(name)).unwrap();
        }
        assert!(!orphaned_linked(&entry, &meta), "the library is still a target file");
        fs::remove_file(target.join("libk.rlib")).unwrap();
        assert!(orphaned_linked(&entry, &meta), "only a leftover object name shares the entry once the library is replaced");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unit_records_keep_the_most_recent_entries_and_report_the_rest() {
        let (dir, cache) = scratch("ring");
        let unit = key(0xaa);
        for n in 1..=3 {
            publish_entry(&cache, n);
            assert!(cache.note_unit_entry(&unit, &key(n), 3).is_empty());
        }
        assert!(cache.note_unit_entry(&unit, &key(1), 3).is_empty());
        publish_entry(&cache, 4);
        assert_eq!(cache.note_unit_entry(&unit, &key(4), 3), vec![key(2)]);
        publish_entry(&cache, 5);
        assert_eq!(cache.note_unit_entry(&unit, &key(5), 3), vec![key(3)]);
        assert!(
            cache.note_unit_entry(&unit, &key(5), 3).is_empty(),
            "noting the newest again changes nothing"
        );
        publish_entry(&cache, 9);
        assert!(cache.note_unit_entry(&key(0xbb), &key(9), 3).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    fn wait_for_file(path: &Path) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !path.exists() {
            assert!(std::time::Instant::now() < deadline, "timed out waiting for {}", path.display());
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn unit_record_process_child() {
        let Some(home) = std::env::var_os("FRUST_UNIT_RECORD_TEST_HOME") else { return };
        let cache = Cache::new(&Dirs::at(PathBuf::from(home)));
        let entry: u8 = std::env::var("FRUST_UNIT_RECORD_TEST_KEY").unwrap().parse().unwrap();
        let unit: u8 = std::env::var("FRUST_UNIT_RECORD_TEST_UNIT").unwrap().parse().unwrap();
        let marker = PathBuf::from(std::env::var_os("FRUST_UNIT_RECORD_TEST_MARKER").unwrap());
        fs::write(marker.with_extension("ready"), b"ready").unwrap();
        cache.note_unit_entry_with(&key(unit), &key(entry), 2, || {
            fs::write(marker.with_extension("locked"), b"locked").unwrap();
            if std::env::var_os("FRUST_UNIT_RECORD_TEST_PAUSE").is_some() {
                wait_for_file(&marker.with_extension("release"));
            }
        }, |_| {}, fsx::remove_any);
    }

    #[test]
    fn os_record_lock_serializes_same_unit_writers_and_preserves_the_version_cap() {
        let (dir, cache) = scratch("record-process-race");
        for entry in 1..=3 { publish_entry(&cache, entry); }
        cache.note_unit_entry(&key(0xaa), &key(1), 2);
        let spawn = |entry: u8, marker: &Path, pause: bool| {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command.args(["--exact", "cache::tests::unit_record_process_child", "--nocapture"])
                .env("FRUST_UNIT_RECORD_TEST_HOME", &dir)
                .env("FRUST_UNIT_RECORD_TEST_KEY", entry.to_string())
                .env("FRUST_UNIT_RECORD_TEST_UNIT", "170")
                .env("FRUST_UNIT_RECORD_TEST_MARKER", marker);
            if pause { command.env("FRUST_UNIT_RECORD_TEST_PAUSE", "1"); }
            command.spawn().unwrap()
        };
        let first_marker = dir.join("first");
        let second_marker = dir.join("second");
        let mut first = spawn(2, &first_marker, true);
        wait_for_file(&first_marker.with_extension("locked"));
        let probe = crate::lock::open_lock_file(&Dirs::at(&dir).keylocks().join("unit-records"), None).unwrap();
        assert!(matches!(sys::try_lock(&probe, sys::LockKind::Exclusive), Err(fs::TryLockError::WouldBlock)));
        let mut second = spawn(3, &second_marker, false);
        wait_for_file(&second_marker.with_extension("ready"));
        assert!(!second_marker.with_extension("locked").exists());
        drop(probe);
        fs::write(first_marker.with_extension("release"), b"release").unwrap();
        assert!(first.wait().unwrap().success());
        assert!(second.wait().unwrap().success());
        assert_eq!(cache.unit_keys(&key(0xaa)), vec![key(3), key(2)]);
        assert!(!cache.has_entry(&key(1)));
        assert!(cache.has_entry(&key(2)) && cache.has_entry(&key(3)));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cleanup_outside_record_lock_allows_updates_and_preserves_republished_keys() {
        let (dir, cache) = scratch("record-cleanup-unlocked");
        publish_entry(&cache, 1);
        publish_entry(&cache, 2);
        cache.note_unit_entry(&key(0xaa), &key(1), 1);
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let writer = cache.clone();
        let handle = std::thread::spawn(move || writer.note_unit_entry_with(&key(0xaa), &key(2), 1,
            || {}, |_| {}, |retired| {
                entered_tx.send(retired.to_owned()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                fsx::remove_any(retired)
            }));
        let retired = entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let probe = crate::lock::open_lock_file(&Dirs::at(&dir).keylocks().join("unit-records"), None).unwrap();
        sys::try_lock(&probe, sys::LockKind::Exclusive).unwrap();
        sys::unlock(&probe).unwrap();
        drop(probe);
        publish_entry(&cache, 3);
        assert!(cache.note_unit_entry(&key(0xbb), &key(3), 1).is_empty());
        publish_entry(&cache, 1);
        fs::write(cache.entry_dir(&key(1)).join("f0"), b"republished bytes").unwrap();
        assert_eq!(cache.note_unit_entry(&key(0xaa), &key(1), 1), vec![key(2)]);
        assert!(retired.join("f0").is_file());
        release_tx.send(()).unwrap();
        assert_eq!(handle.join().unwrap(), vec![key(1)]);
        assert_eq!(fs::read(cache.entry_dir(&key(1)).join("f0")).unwrap(), b"republished bytes");
        assert_eq!(cache.unit_keys(&key(0xaa)), vec![key(1)]);
        assert!(cache.has_entry(&key(3)));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn record_lock_denial_and_commit_failure_retire_no_entries() {
        let (dir, cache) = scratch("record-errors");
        for entry in 1..=3 { publish_entry(&cache, entry); }
        let unit = key(0xaa);
        cache.note_unit_entry(&unit, &key(1), 1);
        let record = cache.unit_pack_path();
        let original = fs::read(&record).unwrap();
        let denied = Dirs::at(&dir).keylocks().join("unit-records");
        let saved = denied.with_extension("saved");
        sys::rename(&denied, &saved).unwrap();
        fs::create_dir(&denied).unwrap();
        assert!(cache.note_unit_entry(&unit, &key(2), 1).is_empty());
        assert_eq!(fs::read(&record).unwrap(), original);
        fs::remove_dir(&denied).unwrap();
        sys::rename(&saved, &denied).unwrap();
        assert!(cache.note_unit_entry_with(&unit, &key(2), 1, || {}, |path| {
            sys::rename(path, &path.with_extension("saved")).unwrap();
            fs::create_dir(path).unwrap();
        }, fsx::remove_any).is_empty());
        for entry in 1..=3 { assert!(cache.has_entry(&key(entry))); }
        assert_eq!(fs::read_dir(evicted_dir(&Dirs::at(&dir))).map(|e| e.count()).unwrap_or(0), 0);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn queued_supersession_retires_now_and_deletes_only_the_same_queued_directory_later() {
        let (dir, cache) = scratch("supersede-queued");
        let dirs = Dirs::at(&dir);
        let unit = key(0xaa);
        for n in 1..=3 {
            publish_entry(&cache, n);
            cache.note_unit_entry(&unit, &key(n), 3);
        }
        let (keys, queued) = cache.supersede_unit_entries_queued(&unit, 2);
        assert_eq!(keys, vec![key(2), key(1)]);
        assert_eq!(cache.unit_keys(&unit), vec![key(3)]);
        assert!(!cache.has_entry(&key(1)) && !cache.has_entry(&key(2)));
        assert_eq!(queued.len(), 2);
        assert!(queued.iter().all(|entry| entry.path.is_dir() && entry.path.parent() == Some(evicted_dir(&dirs).as_path())));
        // A directory that replaced the queued one, or a path outside the queue, is never deleted.
        let swapped = Retired { ino: queued[0].ino.wrapping_add(1), ..queued[0].clone() };
        assert!(!delete_retired(&dirs, &swapped));
        let outside = Retired { path: dir.join("tmp"), ..queued[0].clone() };
        assert!(!delete_retired(&dirs, &outside));
        assert!(queued[0].path.is_dir());
        for entry in &queued {
            assert!(delete_retired(&dirs, entry));
            assert!(!entry.path.exists());
        }
        assert!(!delete_retired(&dirs, &queued[0]));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_unit_entries_are_not_promoted_and_keep_zero_retires_present_entries() {
        let (dir, cache) = scratch("record-missing-and-zero");
        let unit = key(0xaa);
        publish_entry(&cache, 1);
        cache.note_unit_entry(&unit, &key(1), 2);
        cache.remove_entry(&key(1));
        assert!(cache.note_unit_entry(&unit, &key(1), 2).is_empty());
        publish_entry(&cache, 2);
        cache.note_unit_entry(&unit, &key(2), 2);
        assert_eq!(cache.unit_keys(&unit), vec![key(2)]);
        assert_eq!(cache.note_unit_entry(&unit, &key(2), 0), vec![key(2)]);
        assert!(!cache.has_entry(&key(2)));
        assert!(cache.unit_keys(&unit).is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn overlapping_stores_of_one_unit_leave_exactly_one_entry_in_either_order() {
        for order in [[1u8, 2], [2, 1]] {
            let (dir, cache) = scratch("overlap");
            let unit = key(0xaa);
            for n in order { publish_entry(&cache, n); }
            for n in order { cache.note_unit_entry(&unit, &key(n), 1); }
            let survivors: Vec<u8> = [1u8, 2].into_iter().filter(|n| cache.has_entry(&key(*n))).collect();
            assert_eq!(survivors, vec![order[1]], "the entry recorded last is the head");
            assert_eq!(cache.unit_keys(&unit), vec![key(order[1])]);
            let _ = fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn crafted_keys_never_name_paths() {
        let (dir, cache) = scratch("crafted");
        let outside = dir.join("outside");
        fs::create_dir_all(&outside).unwrap();
        for bad in [
            "../../outside",
            "",
            "ABCDEF0123456789ABCDEF0123456789",
            "0123456789abcdef0123456789abcdef/..",
        ] {
            assert!(!valid_key(bad), "{bad:?}");
            cache.remove_entry(bad);
            assert!(cache.note_unit_entry(bad, &key(1), 2).is_empty());
            assert!(cache.note_unit_entry(&key(1), bad, 2).is_empty());
        }
        assert!(outside.is_dir());
        let unit = key(0xcc);
        publish_entry(&cache, 1);
        publish_entry(&cache, 2);
        assert!(cache.note_unit_entry(&unit, &key(1), 2).is_empty());
        let record = Cache::keyed(cache.units_dir(), &unit);
        fs::write(&record, format!("../../outside\n{}\n", key(1))).unwrap();
        assert_eq!(cache.note_unit_entry(&unit, &key(2), 1), vec![key(1)]);
        assert!(outside.is_dir());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn removed_entries_disappear_and_manifests_drop_them() {
        let (dir, cache) = scratch("remove");
        for n in [1u8, 2] {
            let staged = cache.stage().unwrap();
            fs::write(staged.join("meta.json"), b"{}").unwrap();
            cache.publish(&staged, &key(n)).unwrap();
        }
        let prekey = key(0x77);
        let candidate = |n: u8| Candidate {
            inputs: Vec::new(),
            envs: Vec::new(),
            key: key(n),
        };
        cache.add_candidate(&prekey, candidate(1));
        cache.add_candidate(&prekey, candidate(2));
        assert_eq!(cache.candidates(&prekey).len(), 2);
        cache.remove_entry(&key(1));
        assert!(!cache.entry_dir(&key(1)).exists());
        assert!(cache.entry_dir(&key(2)).join("meta.json").is_file());
        cache.add_candidate(&prekey, candidate(2));
        let keys: Vec<String> = cache
            .candidates(&prekey)
            .into_iter()
            .map(|c| c.key)
            .collect();
        assert_eq!(
            keys,
            vec![key(2)],
            "the candidate of the removed entry is dropped"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    fn publish_entry(cache: &Cache, n: u8) {
        let staged = cache.stage().unwrap();
        fs::write(staged.join("meta.json"), b"{}").unwrap();
        fs::write(staged.join("f0"), vec![n; 1024]).unwrap();
        cache.publish(&staged, &key(n)).unwrap();
    }

    fn publish_referenced_entry(cache: &Cache, n: u8, prekey: &str) {
        let staged = cache.stage().unwrap();
        fs::write(staged.join("meta.json"), serde_json::to_vec(&serde_json::json!({"prekey": prekey})).unwrap()).unwrap();
        fs::write(staged.join("f0"), [n; 32]).unwrap();
        cache.publish(&staged, &key(n)).unwrap();
    }

    fn empty_candidate(n: u8) -> Candidate {
        Candidate { inputs: Vec::new(), envs: Vec::new(), key: key(n) }
    }

    fn publish_unit_entry(cache: &Cache, n: u8, unit: &str) {
        publish_entry(cache, n);
        fs::write(cache.entry_dir(&key(n)).join("meta.json"),
            serde_json::to_vec(&serde_json::json!({"incremental_unit": unit})).unwrap()).unwrap();
    }

    fn write_unit_record(cache: &Cache, unit: &str, keys: &[u8]) -> PathBuf {
        let path = Cache::keyed(cache.units_dir(), unit);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, format!("{}\n", keys.iter().map(|n| key(*n)).collect::<Vec<_>>().join("\n"))).unwrap();
        path
    }

    #[test]
    fn unit_records_share_one_pack_and_a_write_folds_the_legacy_file_into_it() {
        let (dir, cache) = scratch("pack");
        let (first, second) = (key(0xa1), key(0xa2));
        for n in 1..=3 { publish_unit_entry(&cache, n, &first); }
        let legacy = write_unit_record(&cache, &first, &[1, 2]);
        assert_eq!(cache.unit_keys(&first), vec![key(1), key(2)], "a legacy file is read until the pack lists the unit");
        assert_eq!(cache.note_unit_entry(&first, &key(3), 3), Vec::<String>::new());
        assert_eq!(cache.unit_keys(&first), vec![key(3), key(1), key(2)]);
        assert!(!legacy.exists(), "the pack owns the unit once written");
        cache.note_unit_entry(&second, &key(2), 1);
        let files: Vec<_> = fs::read_dir(cache.units_dir()).unwrap().flatten().map(|entry| entry.file_name()).collect();
        assert_eq!(files, vec![std::ffi::OsString::from(UNIT_PACK)], "every unit lives in the one file");
        let mut listed: Vec<_> = cache.unit_records().into_iter().map(|(unit, stamp, keys)| (unit, stamp > 0, keys)).collect();
        listed.sort();
        assert_eq!(listed, vec![(first.clone(), true, vec![key(3), key(1), key(2)]), (second.clone(), true, vec![key(2)])]);
        cache.remove_entry(&key(1));
        assert_eq!(cache.unit_keys(&first), vec![key(3), key(2)], "retirement prunes the pack line");
        cache.remove_entry(&key(3));
        cache.remove_entry(&key(2));
        assert!(!cache.unit_records().iter().any(|record| record.0 == first), "a record with no entries left is dropped");
        assert!(cache.unit_keys(&second) == vec![key(2)] || cache.unit_keys(&second).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_untrusted_pack_is_left_alone_and_blocks_writes() {
        let (dir, cache) = scratch("pack-bad");
        publish_unit_entry(&cache, 1, &key(0xa1));
        fs::create_dir_all(cache.units_dir()).unwrap();
        fs::write(cache.unit_pack_path(), b"not a record\n").unwrap();
        assert!(!cache.unit_pack_valid());
        assert!(cache.note_unit_entry(&key(0xa1), &key(1), 1).is_empty());
        assert_eq!(fs::read(cache.unit_pack_path()).unwrap(), b"not a record\n");
        assert!(cache.unit_keys(&key(0xa1)).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn new_names_are_flat_and_legacy_shard_locations_still_resolve() {
        let (dir, cache) = scratch("flat-legacy");
        publish_entry(&cache, 1);
        assert_eq!(cache.entry_dir(&key(1)), dir.join("store").join(key(1)));
        let legacy = Cache::legacy(&cache.dirs.store(), &key(2));
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("meta.json"), b"{}").unwrap();
        assert!(cache.has_entry(&key(2)) && cache.entry_dir(&key(2)) == legacy);
        assert!(!cache.reference_absent(&key(2)) && !cache.unit_reference_absent(&key(2)));
        assert!(cache.reference_absent(&key(3)) && cache.unit_reference_absent(&key(3)));
        let manifest = Cache::legacy(&cache.dirs.manifests(), &key(9)).with_extension("json");
        fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        fs::write(&manifest, serde_json::to_vec(&Manifest { format: 4, candidates: vec![empty_candidate(1)] }).unwrap()).unwrap();
        assert_eq!(cache.candidates(&key(9)), vec![empty_candidate(1)]);
        assert_eq!(cache.manifest_path(&key(9)), manifest);
        let record = Cache::legacy(&cache.units_dir(), &key(8));
        fs::create_dir_all(record.parent().unwrap()).unwrap();
        fs::write(&record, format!("{}\n", key(1))).unwrap();
        assert_eq!(cache.unit_keys(&key(8)), vec![key(1)]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn migration_moves_legacy_names_flat_and_drops_emptied_shards() {
        let (dir, cache) = scratch("migrate");
        let legacy = Cache::legacy(&cache.dirs.store(), &key(2));
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("meta.json"), b"{}").unwrap();
        publish_entry(&cache, 3);
        let twin = Cache::legacy(&cache.dirs.store(), &key(3));
        fs::create_dir_all(&twin).unwrap();
        fs::write(twin.join("meta.json"), b"{}").unwrap();
        let manifest = Cache::legacy(&cache.dirs.manifests(), &key(9)).with_extension("json");
        fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        fs::write(&manifest, serde_json::to_vec(&Manifest { format: 4, candidates: vec![empty_candidate(2)] }).unwrap()).unwrap();
        let record = Cache::legacy(&cache.units_dir(), &key(8));
        fs::create_dir_all(record.parent().unwrap()).unwrap();
        fs::write(&record, format!("{}\n", key(2))).unwrap();
        fs::create_dir_all(cache.dirs.store().join("zz")).unwrap();
        cache.migrate_legacy_layout();
        assert_eq!(cache.entry_dir(&key(2)), cache.dirs.store().join(key(2)));
        assert!(cache.has_entry(&key(2)) && cache.has_entry(&key(3)));
        assert!(cache.dirs.store().join(key(3)).join("f0").is_file(), "the flat twin is kept");
        assert_eq!(cache.candidates(&key(9)), vec![empty_candidate(2)]);
        assert_eq!(cache.unit_keys(&key(8)), vec![key(2)]);
        assert!(!legacy.parent().unwrap().exists() && !twin.parent().unwrap().exists());
        assert!(!manifest.parent().unwrap().exists() && !record.parent().unwrap().exists());
        assert!(cache.dirs.store().join("zz").exists(), "only hex shard names are touched");
        assert_eq!(keyed_items(&cache.dirs.store()).len(), 3);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn retirement_prunes_owned_unit_references_in_order_and_deduplicates_live_keys() {
        let (dir, cache) = scratch("prompt-unit");
        let unit = key(0xa1);
        for n in [1, 2, 3] { publish_unit_entry(&cache, n, &unit); }
        let path = write_unit_record(&cache, &unit, &[3, 1, 4, 2, 3]);
        cache.remove_entry_with(&key(1), |_| {
            assert_eq!(fs::read_to_string(&path).unwrap(), format!("{}\n{}\n", key(3), key(2)));
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        });
        assert!(!cache.has_entry(&key(1)));
        assert_eq!(fs::read_to_string(&path).unwrap(), format!("{}\n{}\n", key(3), key(2)));
        let before = EntryIdentity::read(&sys::lstat(&path).unwrap()).unwrap();
        let guard = UnitRecordsGuard::acquire(&cache.dirs).unwrap();
        cache.prune_unit_record_guarded(&guard, &unit, &key(4));
        assert_eq!(EntryIdentity::read(&sys::lstat(&path).unwrap()), Some(before));
        drop(guard);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn retirement_removes_empty_unit_records_and_republication_recreates_them() {
        let (dir, cache) = scratch("unit-republish");
        let unit = key(0xa2);
        publish_unit_entry(&cache, 1, &unit);
        let path = write_unit_record(&cache, &unit, &[1]);
        cache.remove_entry(&key(1));
        assert!(!path.exists());
        publish_unit_entry(&cache, 1, &unit);
        assert!(cache.note_unit_entry(&unit, &key(1), 1).is_empty());
        assert_eq!(cache.unit_keys(&unit), vec![key(1)]);
        publish_unit_entry(&cache, 2, &unit);
        assert_eq!(cache.note_unit_entry(&unit, &key(2), 1), vec![key(1)]);
        assert_eq!(cache.unit_keys(&unit), vec![key(2)]);
        cache.remove_entry(&key(2));
        assert!(cache.unit_keys(&unit).is_empty() && !cache.unit_records().iter().any(|record| record.0 == unit), "a pruned empty record leaves the pack");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn retirement_leaves_wrong_unit_records_without_the_retired_key_unchanged() {
        let (dir, cache) = scratch("unit-membership");
        let unit = key(0xa3);
        publish_unit_entry(&cache, 1, &unit);
        for n in [2, 3] { publish_entry(&cache, n); }
        let nominated = write_unit_record(&cache, &unit, &[2, 4]);
        let actual = write_unit_record(&cache, &key(0xb3), &[3, 1]);
        let bytes = [fs::read(&nominated).unwrap(), fs::read(&actual).unwrap()];
        cache.remove_entry(&key(1));
        assert_eq!(fs::read(nominated).unwrap(), bytes[0]);
        assert_eq!(fs::read(actual).unwrap(), bytes[1]);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn retirement_retains_existing_incomplete_and_wrong_type_unit_references() {
        let (dir, cache) = scratch("unit-incomplete");
        let unit = key(0xa4);
        publish_unit_entry(&cache, 1, &unit);
        fs::create_dir_all(cache.entry_dir(&key(2))).unwrap();
        fs::create_dir_all(cache.entry_dir(&key(3)).parent().unwrap()).unwrap();
        fs::write(cache.entry_dir(&key(3)), b"not a directory").unwrap();
        let path = write_unit_record(&cache, &unit, &[2, 1, 3]);
        cache.remove_entry(&key(1));
        assert_eq!(fs::read_to_string(path).unwrap(), format!("{}\n{}\n", key(2), key(3)));
        assert!(!cache.unit_reference_absent(&key(2)));
        assert!(!cache.unit_reference_absent(&key(3)));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn retirement_preserves_malformed_oversized_and_legacy_unit_records() {
        let (dir, cache) = scratch("unit-invalid");
        let unit = key(0xa5);
        let path = write_unit_record(&cache, &unit, &[1]);
        let valid = format!("{}\n", key(1));
        for contents in [b"broken\n".to_vec(), key(1).into_bytes(), format!("{valid}\n").into_bytes(),
            vec![0xff], valid.repeat(UNIT_RECORD_MAX_BYTES as usize / valid.len() + 1).into_bytes()] {
            publish_unit_entry(&cache, 1, &unit);
            fs::write(&path, &contents).unwrap();
            cache.remove_entry(&key(1));
            assert_eq!(fs::read(&path).unwrap(), contents);
        }
        publish_entry(&cache, 1);
        fs::write(&path, &valid).unwrap();
        cache.remove_entry(&key(1));
        assert_eq!(fs::read(path).unwrap(), valid.as_bytes());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn retirement_prunes_manifest_and_unit_hints_independently() {
        let (dir, cache) = scratch("independent-hints");
        let prekey = key(0x99);
        for malformed in [serde_json::json!(17), serde_json::json!({"invalid": true}), serde_json::json!("../bad")] {
            publish_referenced_entry(&cache, 1, &prekey);
            cache.add_candidate(&prekey, empty_candidate(1));
            fs::write(cache.entry_dir(&key(1)).join("meta.json"), serde_json::to_vec(
                &serde_json::json!({"prekey": prekey, "incremental_unit": malformed})).unwrap()).unwrap();
            cache.remove_entry(&key(1));
            assert!(cache.candidates(&prekey).is_empty());
        }
        let unit = key(0xa6);
        publish_unit_entry(&cache, 1, &unit);
        let path = write_unit_record(&cache, &unit, &[1]);
        fs::write(cache.entry_dir(&key(1)).join("meta.json"), serde_json::to_vec(
            &serde_json::json!({"prekey": 17, "incremental_unit": unit})).unwrap()).unwrap();
        cache.remove_entry(&key(1));
        assert!(!path.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unit_pruning_retains_records_on_commit_failure_or_identity_changes() {
        let (dir, cache) = scratch("unit-commit-proof");
        let unit = key(0xa7);
        publish_entry(&cache, 2);
        let path = write_unit_record(&cache, &unit, &[2, 1]);
        let original = fs::read(&path).unwrap();
        let guard = UnitRecordsGuard::acquire(&cache.dirs).unwrap();
        let mut attempted = false;
        cache.prune_unit_record_with(&guard, &unit, &key(1), |_, contents| {
            attempted = true;
            assert_eq!(contents.unwrap(), format!("{}\n", key(2)).as_bytes());
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        });
        assert!(attempted);
        assert_eq!(fs::read(&path).unwrap(), original);
        let snapshot = UnitRecordSnapshot::read(&cache.dirs, &unit).unwrap();
        crate::lock::write_shared(&path, &original, crate::lock::shared_mode(&cache.dirs)).unwrap();
        assert!(!snapshot.unchanged(&cache.dirs));
        let snapshot = UnitRecordSnapshot::read(&cache.dirs, &unit).unwrap();
        let parent = path.parent().unwrap();
        sys::rename(parent, &dir.join("previous-unit-parent")).unwrap();
        fs::create_dir(parent).unwrap();
        fs::write(&path, &original).unwrap();
        assert!(!snapshot.unchanged(&cache.dirs));
        assert_eq!(fs::read(path).unwrap(), original);
        drop(guard);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unit_pruning_waits_for_the_retirement_guard() {
        let (dir, cache) = scratch("unit-prune-guard");
        let unit = key(0xa9);
        publish_unit_entry(&cache, 1, &unit);
        let path = write_unit_record(&cache, &unit, &[1]);
        let snapshot = cache.retirement_snapshot(&key(1)).unwrap();
        let guard = UnitRecordsGuard::acquire(&cache.dirs).unwrap();
        assert_eq!(cache.try_retire_entry(&snapshot).unwrap(), EntryRetirementOutcome::Busy);
        assert_eq!(fs::read_to_string(&path).unwrap(), format!("{}\n", key(1)));
        assert!(cache.has_entry(&key(1)));
        drop(guard);
        assert_eq!(cache.try_retire_entry(&snapshot).unwrap(), EntryRetirementOutcome::Retired);
        assert!(!path.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn unit_pruning_keeps_shared_cache_record_permissions() {
        let (dir, cache) = scratch("unit-shared-mode");
        sys::set_path_mode(&dir, 0o777).unwrap();
        let unit = key(0xaa);
        for n in [1, 2] { publish_unit_entry(&cache, n, &unit); }
        let path = write_unit_record(&cache, &unit, &[2, 1]);
        cache.remove_entry(&key(1));
        assert_eq!(fs::read_to_string(&path).unwrap(), format!("{}\n", key(2)));
        assert_eq!(sys::stat(&path).unwrap().mode() & 0o777, 0o664);
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn unit_pruning_preserves_symlink_references_and_record_boundaries() {
        let (dir, cache) = scratch("unit-symlinks");
        let unit = key(0xa8);
        publish_unit_entry(&cache, 1, &unit);
        fs::create_dir_all(cache.entry_dir(&key(2)).parent().unwrap()).unwrap();
        sys::link_tool(&dir.join("absent"), &cache.entry_dir(&key(2))).unwrap();
        let path = write_unit_record(&cache, &unit, &[2, 1]);
        cache.remove_entry(&key(1));
        assert_eq!(fs::read_to_string(&path).unwrap(), format!("{}\n", key(2)));
        let outside = dir.join("outside-unit");
        sys::rename(&path, &outside).unwrap();
        sys::link_tool(&outside, &path).unwrap();
        publish_unit_entry(&cache, 1, &unit);
        fs::write(&outside, format!("{}\n", key(1))).unwrap();
        cache.remove_entry(&key(1));
        assert_eq!(fs::read_to_string(&outside).unwrap(), format!("{}\n", key(1)));
        let guard = UnitRecordsGuard::acquire(&cache.dirs).unwrap();
        let parent = path.parent().unwrap();
        let outside_parent = dir.join("outside-unit-parent");
        sys::rename(parent, &outside_parent).unwrap();
        sys::link_tool(&outside_parent, parent).unwrap();
        assert!(UnitRecordSnapshot::read(&cache.dirs, &unit).is_none());
        cache.prune_unit_record_guarded(&guard, &unit, &key(1));
        assert_eq!(fs::read_to_string(&outside).unwrap(), format!("{}\n", key(1)));
        drop(guard);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_manifest_is_written_packed_and_a_plain_one_from_an_older_frust_still_reads() {
        let (dir, cache) = scratch("packed-manifest");
        let prekey = key(0x97);
        publish_referenced_entry(&cache, 1, &prekey);
        let inputs: Vec<Input> = (0..100)
            .map(|n| Input { path: format!("/usr/local/cargo/registry/src/index.crates.io-1949cf8c6b5b557f/memchr-2.7.4/src/f{n}.rs"), hash: "8a".repeat(32), stamp: None })
            .collect();
        let candidate = Candidate { inputs, envs: Vec::new(), key: key(1) };
        cache.add_candidate(&prekey, candidate.clone());
        let json = serde_json::to_vec(&Manifest { format: MANIFEST_FORMAT, candidates: vec![candidate.clone()] }).unwrap();
        let frame = pack::Log::read(&cache.dirs.manifest_pack()).unwrap().frame(&prekey).unwrap().len();
        assert!(frame * 2 < json.len(), "the manifest is packed");
        assert!(!cache.manifest_path(&prekey).exists(), "it lives in the log, not in a file of its own");
        assert_eq!(cache.candidates(&prekey), vec![candidate.clone()]);
        for (older, bytes) in [(key(0x98), json.clone()), (key(0x99), fsx::pack(&json))] {
            let path = cache.manifest_path(&older);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, &bytes).unwrap();
            assert_eq!(cache.candidates(&older), vec![candidate.clone()], "a per-file manifest from an older frust still reads");
            assert_eq!(manifest_keys(&path), Some(vec![key(1)]));
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn maintenance_folds_per_file_manifests_into_the_log_and_keeps_unreadable_ones() {
        let (dir, cache) = scratch("fold-manifests");
        publish_entry(&cache, 1);
        let json = serde_json::to_vec(&Manifest { format: MANIFEST_FORMAT, candidates: vec![empty_candidate(1)] }).unwrap();
        let (flat, sharded, broken) = (key(0x81), key(0x82), key(0x83));
        let flat_path = cache.dirs.manifests().join(format!("{flat}.json"));
        let sharded_path = Cache::legacy(&cache.dirs.manifests(), &sharded).with_extension("json");
        fs::create_dir_all(sharded_path.parent().unwrap()).unwrap();
        fs::write(&flat_path, fsx::pack(&json)).unwrap();
        fs::write(&sharded_path, &json).unwrap();
        fs::write(cache.dirs.manifests().join(format!("{broken}.json")), b"interrupted").unwrap();
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
        File::options().write(true).open(&flat_path).unwrap().set_modified(old).unwrap();
        cache.migrate_legacy_layout();
        assert_eq!(cache.candidates(&flat), vec![empty_candidate(1)]);
        assert_eq!(cache.candidates(&sharded), vec![empty_candidate(1)]);
        assert!(!flat_path.exists() && !sharded_path.exists());
        let log = pack::Log::read(&cache.dirs.manifest_pack()).unwrap();
        assert_eq!(log.get(&flat, MAX_MANIFEST_BYTES).unwrap().0, 1_600_000_000, "the write time carries over for gc");
        assert!(!log.contains(&broken));
        assert!(cache.dirs.manifests().join(format!("{broken}.json")).exists(), "an unreadable manifest is left for gc");
        fs::remove_file(cache.dirs.manifests().join(format!("{broken}.json"))).unwrap();
        cache.migrate_legacy_layout();
        assert!(!cache.dirs.manifests().exists(), "the emptied directory goes");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn retirement_prunes_missing_rows_in_order_and_leaves_unchanged_manifests_alone() {
        let (dir, cache) = scratch("prompt-manifest");
        let prekey = key(0x91);
        for n in [1, 2, 4] { publish_referenced_entry(&cache, n, &prekey); }
        let path = cache.manifest_path(&prekey);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let manifest = Manifest { format: 3, candidates: vec![empty_candidate(2), empty_candidate(1), empty_candidate(3), empty_candidate(4)] };
        fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        cache.remove_entry(&key(1));
        assert_eq!(cache.candidates(&prekey), vec![empty_candidate(2), empty_candidate(4)]);
        let bytes = fs::read(&path).unwrap();
        assert_eq!(serde_json::from_slice::<Manifest>(&fsx::read_packed(&path, MAX_MANIFEST_BYTES).unwrap()).unwrap().format, 3);
        let identity = sys::lstat(&path).unwrap();
        let guard = UnitRecordsGuard::acquire(&cache.dirs).unwrap();
        cache.prune_manifest_guarded(&guard, &prekey);
        drop(guard);
        assert_eq!(sys::lstat(&path).unwrap().ino(), identity.ino());
        assert_eq!(fs::read(&path).unwrap(), bytes);
        cache.remove_entry(&key(2));
        cache.remove_entry(&key(4));
        assert!(!path.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn delayed_unlocked_upsert_cannot_resurrect_a_retired_reference() {
        let (dir, cache) = scratch("delayed-upsert");
        let prekey = key(0x92);
        publish_referenced_entry(&cache, 1, &prekey);
        cache.add_candidate(&prekey, empty_candidate(1));
        let guard = UnitRecordsGuard::acquire(&cache.dirs).unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let writer = cache.clone();
        let writer_prekey = prekey.clone();
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            writer.add_candidate(&writer_prekey, empty_candidate(1));
        });
        started_rx.recv().unwrap();
        let retired = cache.retire_entry_guarded(&guard, &key(1), None).unwrap().unwrap();
        drop(guard);
        worker.join().unwrap();
        assert!(!manifest_present(&cache, &prekey));
        assert!(!cache.has_entry(&key(1)));
        fsx::remove_any(&retired.path).unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn publication_serializes_with_empty_destination_retirement() {
        let (dir, cache) = scratch("empty-publication-retirement");
        let prekey = key(0x9a);
        fs::create_dir_all(cache.entry_dir(&key(1))).unwrap();
        let selected = cache.retirement_snapshot(&key(1)).unwrap();
        let staged = cache.stage().unwrap();
        fs::write(staged.join("meta.json"), serde_json::to_vec(&serde_json::json!({"prekey": prekey})).unwrap()).unwrap();
        fs::write(staged.join("f0"), b"published payload").unwrap();
        let guard = UnitRecordsGuard::acquire(&cache.dirs).unwrap();
        let writer = cache.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || { ready_tx.send(()).unwrap(); writer.publish(&staged, &key(1)).unwrap(); });
        ready_rx.recv().unwrap();
        let retired = cache.retire_entry_guarded(&guard, &key(1), Some(&selected)).unwrap().unwrap();
        assert!(!retired.path.join("meta.json").exists());
        drop(guard);
        worker.join().unwrap();
        assert_eq!(fs::read(cache.entry_dir(&key(1)).join("f0")).unwrap(), b"published payload");
        assert_eq!(cache.try_retire_entry(&selected).unwrap(), EntryRetirementOutcome::Stale);
        cache.add_candidate(&prekey, empty_candidate(1));
        assert_eq!(cache.candidates(&prekey), vec![empty_candidate(1)]);
        fsx::remove_any(&retired.path).unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn publication_guard_denial_leaves_no_entry_or_candidate() {
        let (dir, cache) = scratch("publish-guard-denied");
        let staged = cache.stage().unwrap();
        fs::write(staged.join("meta.json"), b"{}").unwrap();
        fs::create_dir_all(cache.dirs.keylocks().join("unit-records")).unwrap();
        assert!(cache.publish(&staged, &key(1)).is_err());
        cache.add_candidate(&key(0x9b), empty_candidate(1));
        assert!(!staged.exists());
        assert!(!cache.has_entry(&key(1)));
        assert!(!manifest_present(&cache, &key(0x9b)));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn concurrent_unlocked_publishers_preserve_both_manifest_candidates() {
        let (dir, cache) = scratch("concurrent-upserts");
        let prekey = key(0x99);
        let start = std::sync::Arc::new(std::sync::Barrier::new(3));
        let mut workers = Vec::new();
        for n in [1, 2] {
            let writer = cache.clone();
            let writer_prekey = prekey.clone();
            let start = start.clone();
            workers.push(std::thread::spawn(move || {
                publish_referenced_entry(&writer, n, &writer_prekey);
                start.wait();
                writer.add_candidate(&writer_prekey, empty_candidate(n));
            }));
        }
        start.wait();
        for worker in workers { worker.join().unwrap(); }
        let mut keys: Vec<_> = cache.candidates(&prekey).into_iter().map(|row| row.key).collect();
        keys.sort();
        assert_eq!(keys, vec![key(1), key(2)]);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn publication_and_upsert_during_payload_deletion_survive_without_recursive_locking() {
        let (dir, cache) = scratch("delete-republish-manifest");
        let prekey = key(0x93);
        publish_referenced_entry(&cache, 1, &prekey);
        cache.add_candidate(&prekey, empty_candidate(1));
        cache.remove_entry_with(&key(1), |retired| {
            assert!(UnitRecordsGuard::try_acquire(&cache.dirs).unwrap().is_some());
            assert!(!manifest_present(&cache, &prekey));
            publish_referenced_entry(&cache, 1, &prekey);
            cache.add_candidate(&prekey, empty_candidate(1));
            assert_eq!(fs::read(retired.join("f0")).unwrap(), [1; 32]);
            Err(io::Error::other("controlled deletion failure"))
        });
        assert!(cache.has_entry(&key(1)));
        assert_eq!(cache.candidates(&prekey), vec![empty_candidate(1)]);
        assert_eq!(fs::read_dir(evicted_dir(&cache.dirs)).unwrap().count(), 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn maintenance_snapshot_rejects_usage_updates_and_republication() {
        let (dir, cache) = scratch("retirement-snapshots");
        let prekey = key(0x94);
        publish_referenced_entry(&cache, 1, &prekey);
        let meta = cache.entry_dir(&key(1)).join("meta.json");
        File::options().write(true).open(&meta).unwrap().set_modified(SystemTime::UNIX_EPOCH).unwrap();
        let before_use = cache.retirement_snapshot(&key(1)).unwrap();
        cache.mark_used(&key(1));
        assert_eq!(cache.try_retire_entry(&before_use).unwrap(), EntryRetirementOutcome::Stale);
        let before_republish = cache.retirement_snapshot(&key(1)).unwrap();
        cache.remove_entry(&key(1));
        publish_referenced_entry(&cache, 1, &prekey);
        assert_eq!(cache.try_retire_entry(&before_republish).unwrap(), EntryRetirementOutcome::Stale);
        assert!(cache.has_entry(&key(1)));
        let guard = UnitRecordsGuard::acquire(&cache.dirs).unwrap();
        assert_eq!(cache.try_retire_entry(&cache.retirement_snapshot(&key(1)).unwrap()).unwrap(), EntryRetirementOutcome::Busy);
        drop(guard);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn retirement_retains_legacy_malformed_and_unsupported_metadata() {
        let (dir, cache) = scratch("conservative-prune");
        let prekey = key(0x95);
        let path = cache.manifest_path(&prekey);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        for metadata in [b"{}".as_slice(), br#"{"prekey":"../../bad"}"#, b"broken"] {
            publish_entry(&cache, 1);
            fs::write(cache.entry_dir(&key(1)).join("meta.json"), metadata).unwrap();
            let bytes = serde_json::to_vec(&Manifest { format: MANIFEST_FORMAT, candidates: vec![empty_candidate(1)] }).unwrap();
            fs::write(&path, &bytes).unwrap();
            cache.remove_entry(&key(1));
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
        publish_referenced_entry(&cache, 1, &prekey);
        fs::write(&path, b"interrupted manifest").unwrap();
        cache.remove_entry(&key(1));
        assert_eq!(fs::read(&path).unwrap(), b"interrupted manifest");
        for (format, rows) in [(2, vec![empty_candidate(1)]), (MANIFEST_FORMAT, vec![Candidate { key: "../bad".into(), ..empty_candidate(1) }])] {
            publish_referenced_entry(&cache, 1, &prekey);
            let bytes = serde_json::to_vec(&Manifest { format, candidates: rows }).unwrap();
            fs::write(&path, &bytes).unwrap();
            cache.remove_entry(&key(1));
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn pruning_retains_symlink_and_wrong_type_references_and_manifest_boundaries() {
        let (dir, cache) = scratch("prune-types");
        let prekey = key(0x96);
        publish_referenced_entry(&cache, 1, &prekey);
        fs::create_dir_all(cache.entry_dir(&key(2)).parent().unwrap()).unwrap();
        sys::link_tool(&dir.join("absent"), &cache.entry_dir(&key(2))).unwrap();
        fs::create_dir_all(cache.entry_dir(&key(3)).parent().unwrap()).unwrap();
        fs::write(cache.entry_dir(&key(3)), b"wrong type").unwrap();
        let path = cache.manifest_path(&prekey);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, serde_json::to_vec(&Manifest { format: MANIFEST_FORMAT, candidates: vec![empty_candidate(1), empty_candidate(2), empty_candidate(3)] }).unwrap()).unwrap();
        cache.remove_entry(&key(1));
        assert_eq!(cache.candidates(&prekey), vec![empty_candidate(2), empty_candidate(3)]);
        let outside = dir.join("outside-manifest");
        sys::rename(&path, &outside).unwrap();
        sys::link_tool(&outside, &path).unwrap();
        let bytes = fs::read(&outside).unwrap();
        publish_referenced_entry(&cache, 1, &prekey);
        cache.remove_entry(&key(1));
        assert_eq!(fs::read(&outside).unwrap(), bytes);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn guard_denial_skips_manifest_publication_usage_and_retirement() {
        let (dir, cache) = scratch("manifest-guard-denied");
        let prekey = key(0x97);
        publish_referenced_entry(&cache, 1, &prekey);
        let meta = cache.entry_dir(&key(1)).join("meta.json");
        File::options().write(true).open(&meta).unwrap().set_modified(SystemTime::UNIX_EPOCH).unwrap();
        let lock = cache.dirs.keylocks().join("unit-records");
        sys::rename(&lock, &lock.with_extension("saved")).unwrap();
        fs::create_dir(&lock).unwrap();
        cache.add_candidate(&prekey, empty_candidate(1));
        cache.mark_used(&key(1));
        cache.remove_entry(&key(1));
        assert!(!manifest_present(&cache, &prekey));
        assert_eq!(fs::metadata(meta).unwrap().modified().unwrap(), SystemTime::UNIX_EPOCH);
        assert!(cache.has_entry(&key(1)));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn writer_eviction_prunes_before_interrupted_payload_deletion() {
        let (dir, cache) = scratch("writer-prune-interrupted");
        let prekey = key(0x98);
        for n in [1, 2] { publish_referenced_entry(&cache, n, &prekey); cache.add_candidate(&prekey, empty_candidate(n)); }
        cache.note_unit_entry(&key(0xaa), &key(1), 1);
        let result = std::panic::catch_unwind(|| cache.note_unit_entry_with(&key(0xaa), &key(2), 1, || {}, |_| {}, |_| panic!("controlled interruption")));
        assert!(result.is_err());
        assert_eq!(cache.candidates(&prekey), vec![empty_candidate(2)]);
        assert!(!cache.has_entry(&key(1)));
        assert_eq!(fs::read_dir(evicted_dir(&cache.dirs)).unwrap().count(), 1);
        assert!(UnitRecordsGuard::try_acquire(&cache.dirs).unwrap().is_some());
        fs::remove_dir_all(dir).unwrap();
    }

    fn stamp_record() -> StampRecord {
        StampRecord {
            dev: 1,
            ino: 2,
            size: 3,
            mtime_ns: 4_000_000_001,
            ctime_ns: 5_000_000_002,
            hashed_ns: 9_000_000_003,
            birth_ns: Some(3_000_000_004),
        }
    }

    #[test]
    fn manifest_format_seven_writes_and_legacy_formats_migrate_without_losing_raw_entries() {
        assert_eq!((MANIFEST_FORMAT, BACKED_MANIFEST_FORMAT), (7, 8));
        let (dir, cache) = scratch("format-bump");
        publish_entry(&cache, 1);
        let prekey = key(0x7b);
        let path = cache.manifest_path(&prekey);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let older = format!(
            "{{\"format\":2,\"candidates\":[{{\"inputs\":[{{\"path\":\"a.rs\",\"hash\":\"{}\",\"stamp\":{{\"dev\":1,\"ino\":2,\"size\":3,\"mtime_ns\":4,\"ctime_ns\":5,\"hashed_ns\":9}}}}],\"envs\":[],\"key\":\"{}\"}}]}}",
            "ab".repeat(32),
            key(1)
        );
        fs::write(&path, older).unwrap();
        assert!(
            cache.candidates(&prekey).is_empty(),
            "a format 2 manifest serves nothing"
        );
        assert_eq!(
            manifest_keys(&path),
            Some(vec![key(1)]),
            "gc still lists the keys of a format 2 manifest"
        );

        let candidate = Candidate {
            inputs: vec![Input {
                path: "a.rs".into(),
                hash: "ab".repeat(32),
                stamp: Some(stamp_record()),
            }],
            envs: Vec::new(),
            key: key(1),
        };
        cache.add_candidate(&prekey, candidate.clone());
        let current = packed_text(&cache, &prekey);
        assert!(current.contains("\"format\":7"));
        assert!(!path.exists(), "the per-file manifest moved into the log");
        assert_eq!(cache.candidates(&prekey), vec![candidate.clone()]);
        for legacy_format in [3, 4, 5] {
            fs::remove_file(cache.dirs.manifest_pack()).unwrap();
            fs::write(&path, current.replace("\"format\":7", &format!("\"format\":{legacy_format}"))).unwrap();
            assert_eq!(cache.candidates(&prekey), vec![candidate.clone()], "format {legacy_format} entries remain reusable");
            cache.add_candidate(&prekey, candidate.clone());
            assert!(packed_text(&cache, &prekey).contains("\"format\":7"), "even an unchanged legacy candidate migrates");
            assert!(!path.exists());
        }
        let stored: Manifest = serde_json::from_str(&packed_text(&cache, &prekey)).unwrap();
        assert_ne!(stored.format, 3, "old readers reject the new manifest");
        assert_eq!(stored.candidates.iter().map(|c| c.key.clone()).collect::<Vec<_>>(), vec![key(1)], "gc can list format seven");
        assert_eq!(cache.candidates(&prekey), vec![candidate]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn trust_modes_parse_every_spelling() {
        for off in ["0", "off", "false", "no"] {
            assert_eq!(TrustMode::parse(Some(off)), TrustMode::Off, "{off}");
        }
        assert_eq!(TrustMode::parse(Some("audit")), TrustMode::Audit);
        for on in [
            None,
            Some("on"),
            Some("1"),
            Some("true"),
            Some("yes"),
            Some(""),
            Some("bogus"),
            Some("OFF"),
        ] {
            assert_eq!(TrustMode::parse(on), TrustMode::On, "{on:?}");
        }
        assert_eq!(
            ["off", "on", "audit"],
            [TrustMode::Off, TrustMode::On, TrustMode::Audit].map(TrustMode::name)
        );
    }

    fn age_marker(dirs: &Dirs, dev: u64, age: Duration) {
        let when = SystemTime::now() - age;
        fs::File::options()
            .write(true)
            .open(quarantine_marker(dirs, dev))
            .unwrap()
            .set_modified(when)
            .unwrap();
    }

    #[test]
    fn quarantine_is_per_volume_lasts_seven_days_and_is_refreshed_by_a_violation() {
        let (dir, _) = scratch("quarantine");
        let dirs = Dirs::at(&dir);
        assert!(!quarantined(&dirs, 7), "nothing is quarantined by default");
        quarantine(&dirs, 7);
        assert!(quarantined(&dirs, 7));
        assert!(!quarantined(&dirs, 8), "another volume is unaffected");
        assert!(quarantine_marker(&dirs, 7).starts_with(dirs.stamp_trust()));

        age_marker(&dirs, 7, Duration::from_secs(6 * 24 * 3600));
        assert!(quarantined(&dirs, 7), "still inside the seven days");
        age_marker(&dirs, 7, Duration::from_secs(8 * 24 * 3600));
        assert!(!quarantined(&dirs, 7), "expired");
        assert!(
            trust_summary(&dirs, SystemTime::now())
                .quarantined
                .is_empty()
        );

        age_marker(&dirs, 7, Duration::from_secs(6 * 24 * 3600 + 3600));
        report_violation(&dirs, "leaf", "src/lib.rs", "aa", "bb", 7);
        let refreshed = quarantined_since(&dirs, 7, SystemTime::now())
            .expect("a violation quarantines the volume");
        assert!(
            refreshed.elapsed().unwrap_or_default() < Duration::from_secs(600),
            "the seven days restart"
        );

        let now = SystemTime::now();
        assert!(quarantined_since(&dirs, 7, now + Duration::from_secs(6 * 24 * 3600)).is_some());
        assert!(quarantined_since(&dirs, 7, now + Duration::from_secs(8 * 24 * 3600)).is_none());
        assert!(
            quarantined_since(&dirs, 7, now - Duration::from_secs(3600)).is_some(),
            "a marker dated in the future still counts"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn violations_are_logged_as_json_lines_and_the_log_stays_bounded() {
        let (dir, _) = scratch("violations");
        let dirs = Dirs::at(&dir);
        report_violation(&dirs, "leaf", "src/lib.rs", "aa", "bb", 12);
        let log = dirs.stamp_trust().join(VIOLATIONS_LOG);
        let first: serde_json::Value =
            serde_json::from_str(fs::read_to_string(&log).unwrap().trim()).unwrap();
        assert_eq!(first["crate"], "leaf");
        assert_eq!(first["input"], "src/lib.rs");
        assert_eq!(
            (first["recorded"].as_str(), first["fresh"].as_str()),
            (Some("aa"), Some("bb"))
        );
        assert_eq!(first["dev"], 12);
        assert!(first["t"].as_u64().is_some_and(|t| t > 0));

        for i in 0..60 {
            report_violation(
                &dirs,
                "leaf",
                &format!("{i}-{}", "x".repeat(10_000)),
                "aa",
                "bb",
                12,
            );
        }
        let text = fs::read_to_string(&log).unwrap();
        assert!(
            text.len() as u64 <= VIOLATIONS_MAX_BYTES,
            "{} bytes",
            text.len()
        );
        assert!(
            text.lines()
                .all(|l| serde_json::from_str::<serde_json::Value>(l).is_ok()),
            "only whole lines remain"
        );
        assert!(
            text.lines().last().unwrap().contains("\"input\":\"59-"),
            "the newest line is kept"
        );
        assert!(
            !text.contains("\"input\":\"src/lib.rs\""),
            "the oldest lines are dropped first"
        );

        let summary = trust_summary(&dirs, SystemTime::now());
        assert_eq!(summary.violations, text.lines().count());
        assert_eq!(summary.quarantined.len(), 1);
        assert_eq!(summary.quarantined[0].0, 12);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn newest_lines_keeps_whole_lines_within_the_budget() {
        assert_eq!(newest_lines("a\nb\nc\n", 4), "b\nc\n");
        assert_eq!(newest_lines("a\nb\nc\n", 100), "a\nb\nc\n");
        assert_eq!(newest_lines("a\nb\nc\n", 1), "");
        assert_eq!(newest_lines("a\nbc", 3), "bc");
        assert_eq!(newest_lines("", 10), "");
    }

    #[test]
    fn audit_counter_adds_one_byte_per_passed_audit_and_stops_at_its_cap() {
        let (dir, _) = scratch("audits");
        let dirs = Dirs::at(&dir);
        assert_eq!(trust_summary(&dirs, SystemTime::now()).audits_passed, 0);
        note_audits_passed(&dirs, 2);
        note_audits_passed(&dirs, 0);
        note_audits_passed(&dirs, 1);
        assert_eq!(trust_summary(&dirs, SystemTime::now()).audits_passed, 3);
        let path = dirs.stamp_trust().join(AUDITS_FILE);
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(AUDITS_MAX_BYTES)
            .unwrap();
        note_audits_passed(&dirs, 5);
        assert_eq!(fs::metadata(&path).unwrap().len(), AUDITS_MAX_BYTES);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn candidates_round_trip_their_stamps_and_other_formats_are_ignored() {
        let (dir, cache) = scratch("format");
        publish_entry(&cache, 1);
        let prekey = key(0x78);
        let candidate = Candidate {
            inputs: vec![
                Input {
                    path: "src/lib.rs".into(),
                    hash: "ab".repeat(32),
                    stamp: Some(stamp_record()),
                },
                Input {
                    path: "src/new.rs".into(),
                    hash: "cd".repeat(32),
                    stamp: None,
                },
            ],
            envs: vec![("A".into(), None)],
            key: key(1),
        };
        cache.add_candidate(&prekey, candidate.clone());
        assert_eq!(cache.candidates(&prekey), vec![candidate.clone()]);

        let path = cache.manifest_path(&prekey);
        let text = packed_text(&cache, &prekey);
        assert!(
            text.contains(&format!("\"format\":{MANIFEST_FORMAT}")),
            "{text}"
        );
        assert!(
            !text.contains("\"stamp\":null"),
            "an unrecorded stamp is left out"
        );

        let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
        value.as_object_mut().unwrap().remove("format");
        fs::remove_file(cache.dirs.manifest_pack()).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(
            cache.candidates(&prekey).is_empty(),
            "a manifest without the current format is ignored"
        );
        assert_eq!(
            manifest_keys(&path),
            Some(vec![key(1)]),
            "gc can still list its keys"
        );

        cache.add_candidate(&prekey, candidate.clone());
        assert_eq!(
            cache.candidates(&prekey),
            vec![candidate],
            "adding rewrites it in the current format"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_manifests_listing_a_linked_entry_use_the_backed_format() {
        let (dir, cache) = scratch("backed-format");
        publish_entry(&cache, 1);
        let backed = cache.stage().unwrap();
        let meta = serde_json::json!({
            "crate_name": "k", "version": "1", "compile_ms": 1, "created_unix": 1, "stderr": "stderr",
            "files": [{
                "name": "libk.rlib", "stored": "f0", "text": false, "bytes": 8192, "encoding": "linked",
                "decoded_bytes": 8192, "content_hash": "ab".repeat(32), "mode": 33188,
            }],
        });
        fs::write(backed.join("meta.json"), serde_json::to_vec(&meta).unwrap()).unwrap();
        cache.publish(&backed, &key(2)).unwrap();
        let format_of = |prekey: &str| {
            let value: serde_json::Value = serde_json::from_str(&packed_text(&cache, prekey)).unwrap();
            value["format"].as_u64().unwrap()
        };
        let (plain, mixed) = (key(0x7a), key(0x7b));
        cache.add_candidate(&plain, empty_candidate(1));
        assert_eq!(format_of(&plain), u64::from(MANIFEST_FORMAT));
        cache.add_candidate(&mixed, empty_candidate(1));
        cache.add_candidate(&mixed, empty_candidate(2));
        assert_eq!(format_of(&mixed), u64::from(BACKED_MANIFEST_FORMAT));
        assert_eq!(cache.candidates(&mixed).len(), 2, "the backed format stays readable");
        cache.add_candidate(&mixed, empty_candidate(1));
        assert_eq!(format_of(&mixed), u64::from(BACKED_MANIFEST_FORMAT), "a backed manifest never drops back");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn adding_an_identical_front_candidate_leaves_the_manifest_alone() {
        let (dir, cache) = scratch("idempotent");
        publish_entry(&cache, 1);
        publish_entry(&cache, 2);
        let prekey = key(0x79);
        let candidate = |n: u8, stamp: Option<StampRecord>| Candidate {
            inputs: vec![Input {
                path: "a.rs".into(),
                hash: "ef".repeat(32),
                stamp,
            }],
            envs: Vec::new(),
            key: key(n),
        };
        cache.add_candidate(&prekey, candidate(1, None));
        cache.add_candidate(&prekey, candidate(2, None));
        let log = fs::read(cache.dirs.manifest_pack()).unwrap();

        cache.add_candidate(&prekey, candidate(2, None));
        assert_eq!(
            fs::read(cache.dirs.manifest_pack()).unwrap(),
            log,
            "nothing changed, so nothing was written"
        );

        cache.add_candidate(&prekey, candidate(2, Some(stamp_record())));
        assert_eq!(
            cache.candidates(&prekey)[0],
            candidate(2, Some(stamp_record())),
            "a newly recorded stamp is kept"
        );

        cache.add_candidate(&prekey, candidate(1, None));
        let keys: Vec<String> = cache
            .candidates(&prekey)
            .into_iter()
            .map(|c| c.key)
            .collect();
        assert_eq!(keys, vec![key(1), key(2)], "a different front reorders");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn candidates_are_capped_and_a_missing_store_directory_is_created_on_demand() {
        let (dir, cache) = scratch("cap");
        let prekey = key(0x7a);
        for n in 1..=(MAX_CANDIDATES as u8 + 3) {
            publish_entry(&cache, n);
            cache.add_candidate(
                &prekey,
                Candidate {
                    inputs: Vec::new(),
                    envs: Vec::new(),
                    key: key(n),
                },
            );
        }
        let keys: Vec<String> = cache
            .candidates(&prekey)
            .into_iter()
            .map(|c| c.key)
            .collect();
        assert_eq!(keys.len(), MAX_CANDIDATES);
        assert_eq!(keys[0], key(MAX_CANDIDATES as u8 + 3), "newest first");
        assert!(
            cache.has_entry(&key(1)) && !cache.has_entry("../x") && !cache.has_entry(&key(0x42))
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn republishing_an_unchanged_candidate_refreshes_entry_usage() {
        let (dir, cache) = scratch("republish-used");
        publish_entry(&cache, 1);
        let prekey = key(0x7b);
        let candidate = Candidate {
            inputs: Vec::new(),
            envs: Vec::new(),
            key: key(1),
        };
        cache.add_candidate(&prekey, candidate.clone());
        let before = fs::read(cache.dirs.manifest_pack()).unwrap();
        let meta = cache.entry_dir(&key(1)).join("meta.json");
        let old = SystemTime::now() - Duration::from_secs(40 * 86_400);
        fs::File::options()
            .write(true)
            .open(&meta)
            .unwrap()
            .set_modified(old)
            .unwrap();

        cache.add_candidate(&prekey, candidate);

        assert!(
            fs::metadata(&meta).unwrap().modified().unwrap() > old + Duration::from_secs(86_400)
        );
        assert_eq!(fs::read(cache.dirs.manifest_pack()).unwrap(), before);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn mark_used_touches_only_entries_unused_for_an_hour() {
        let (dir, cache) = scratch("used");
        publish_entry(&cache, 1);
        let meta = cache.entry_dir(&key(1)).join("meta.json");
        let set = |age: Duration| {
            let when = std::time::SystemTime::now() - age;
            fs::File::options()
                .write(true)
                .open(&meta)
                .unwrap()
                .set_modified(when)
                .unwrap();
        };
        let age = || {
            fs::metadata(&meta)
                .unwrap()
                .modified()
                .unwrap()
                .elapsed()
                .unwrap()
        };
        set(Duration::from_secs(600));
        cache.mark_used(&key(1));
        assert!(
            age() >= Duration::from_secs(300),
            "a recently used entry is not touched"
        );

        set(Duration::from_secs(2 * 3600));
        cache.mark_used(&key(1));
        assert!(
            age() < Duration::from_secs(300),
            "an old entry is refreshed"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn removed_entries_are_deleted_promptly_and_failed_deletions_are_retryable() {
        let (dir, cache) = scratch("sweep");
        let dirs = Dirs::at(&dir);
        publish_entry(&cache, 1);
        publish_entry(&cache, 2);
        cache.remove_entry(&key(1));
        assert!(!cache.has_entry(&key(1)));
        assert!(cache.has_entry(&key(2)));
        let parked = || fs::read_dir(evicted_dir(&dirs)).unwrap().count();
        assert_eq!(parked(), 0, "successful retirement leaves no queued payload");
        cache.remove_entry_with(&key(2), |retired| {
            assert!(retired.join("f0").is_file());
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "controlled deletion denial"))
        });
        assert!(!cache.has_entry(&key(2)));
        assert_eq!(parked(), 1, "failed deletion stays discoverable");
        assert_eq!(sweep_evicted(&dirs, Duration::from_secs(3600)), 0, "explicit legacy age filter is preserved");
        cache.remove_entry(&key(2));
        assert_eq!(parked(), 1, "absent keys create no additional retirement");
        assert_eq!(sweep_evicted(&dirs, Duration::ZERO), 1);
        assert_eq!(parked(), 0);
        assert!(evicted_dir(&dirs).is_dir(), "retry queue parent survives");
        assert_eq!(sweep_evicted(&Dirs::at(dir.join("nowhere")), Duration::ZERO), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn republication_after_retirement_cannot_be_deleted_with_old_entry() {
        let (dir, cache) = scratch("retire-republish");
        publish_entry(&cache, 1);
        cache.remove_entry_with(&key(1), |retired| {
            publish_entry(&cache, 1);
            fs::write(cache.entry_dir(&key(1)).join("f0"), b"newly published bytes").unwrap();
            assert_eq!(fs::read(retired.join("f0")).unwrap(), vec![1; 1024]);
            fsx::remove_any(retired)
        });
        assert_eq!(fs::read(cache.entry_dir(&key(1)).join("f0")).unwrap(), b"newly published bytes");
        assert!(cache.has_entry(&key(1)));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn interruption_after_rename_leaves_a_recoverable_retired_child() {
        let (dir, cache) = scratch("retire-interrupted");
        publish_entry(&cache, 1);
        let result = std::panic::catch_unwind(|| cache.remove_entry_with(&key(1), |_| panic!("controlled interruption")));
        assert!(result.is_err());
        assert!(!cache.has_entry(&key(1)));
        assert_eq!(sweep_evicted(&Dirs::at(&dir), Duration::ZERO), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn opened_reader_keeps_exact_bytes_after_prompt_retirement() {
        use std::io::Read;
        let (dir, cache) = scratch("retire-open-reader");
        publish_entry(&cache, 1);
        let mut reader = fs::File::open(cache.entry_dir(&key(1)).join("f0")).unwrap();
        cache.remove_entry(&key(1));
        assert!(!cache.has_entry(&key(1)));
        assert_eq!(fs::read_dir(evicted_dir(&Dirs::at(&dir))).unwrap().count(), 0);
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, vec![1; 1024]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reader_paused_before_open_declines_retired_path_without_replacing_output() {
        let (dir, cache) = scratch("retire-paused-reader");
        publish_entry(&cache, 1);
        let source = cache.entry_dir(&key(1)).join("f0");
        let output = dir.join("reader-output");
        fs::write(&output, b"previous output").unwrap();
        let meta: StoredFile = serde_json::from_str(r#"{"name":"artifact","stored":"f0","text":false,"bytes":1024}"#).unwrap();
        cache.remove_entry_with(&key(1), |retired| {
            assert!(payload::restore(&source, &output, &meta).is_err());
            assert_eq!(fs::read(&output).unwrap(), b"previous output");
            fsx::remove_any(retired)
        });
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn windows_delete_denial_stays_queued_until_handle_closes() {
        let (dir, cache) = scratch("retire-denied-handle");
        publish_entry(&cache, 1);
        let mut reader = None;
        cache.remove_entry_with(&key(1), |retired| {
            reader = Some(sys::open_read_without_delete(&retired.join("f0")).unwrap());
            fsx::remove_any(retired)
        });
        let dirs = Dirs::at(&dir);
        assert!(!cache.has_entry(&key(1)));
        assert_eq!(fs::read_dir(evicted_dir(&dirs)).unwrap().count(), 1);
        assert_eq!(sweep_evicted(&dirs, Duration::ZERO), 0);
        drop(reader);
        assert_eq!(sweep_evicted(&dirs, Duration::ZERO), 1);
        let _ = fs::remove_dir_all(&dir);
    }

}
