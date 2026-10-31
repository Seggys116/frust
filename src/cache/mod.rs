//! Direct-mode compile cache: a pre-key selects a manifest of recorded input sets, a match yields an entry key.

pub mod depinfo;
pub mod hashing;
pub mod rustc_args;

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
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

/// Manifests of any other format (such as ones written before stamps carried a birth time) are ignored and rewritten.
const MANIFEST_FORMAT: u32 = 3;

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

/// Directory under the cache's tmp area that holds entries removed from the store until `sweep_evicted` deletes them.
const EVICTED_DIR: &str = "evicted";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredFile {
    pub name: String,
    pub stored: String,
    pub text: bool,
    pub bytes: u64,
    /// A directory output (such as a `.dSYM` bundle); entries stored before this field existed read as `false`.
    #[serde(default)]
    pub dir: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryMeta {
    pub crate_name: String,
    pub version: String,
    pub compile_ms: u64,
    pub created_unix: u64,
    pub files: Vec<StoredFile>,
    pub stderr: String,
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

    fn shard(root: PathBuf, key: &str) -> PathBuf {
        root.join(&key[..2.min(key.len())]).join(key)
    }

    pub fn entry_dir(&self, key: &str) -> PathBuf {
        Self::shard(self.dirs.store(), key)
    }

    fn manifest_path(&self, prekey: &str) -> PathBuf {
        Self::shard(self.dirs.manifests(), prekey).with_extension("json")
    }

    /// Empty for a missing, unreadable or other-format manifest.
    pub fn candidates(&self, prekey: &str) -> Vec<Candidate> {
        let Ok(bytes) = fs::read(self.manifest_path(prekey)) else {
            return Vec::new();
        };
        match serde_json::from_slice::<Manifest>(&bytes) {
            Ok(manifest) if manifest.format == MANIFEST_FORMAT => manifest.candidates,
            _ => Vec::new(),
        }
    }

    /// Drops candidates whose entry is gone so lookups do not hash their inputs for nothing; the manifest is left alone when nothing changes.
    pub fn add_candidate(&self, prekey: &str, candidate: Candidate) {
        let existing = self.candidates(prekey);
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
        if unchanged_front && rows.len() == before {
            return;
        }
        let manifest = Manifest { format: MANIFEST_FORMAT, candidates: rows };
        if let Ok(json) = serde_json::to_vec(&manifest) {
            let _ = fsx::write_atomic(&self.manifest_path(prekey), &json);
        }
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
        let meta: EntryMeta = serde_json::from_slice(&fs::read(dir.join("meta.json")).ok()?).ok()?;
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
        let dest = self.entry_dir(key);
        let mut renamed = sys::rename(staged, &dest);
        // The shard directory is created only when the first rename finds it missing.
        if renamed.as_ref().is_err_and(|e| e.kind() == io::ErrorKind::NotFound)
            && let Some(parent) = dest.parent()
            && fs::create_dir_all(parent).is_ok()
        {
            renamed = sys::rename(staged, &dest);
        }
        match renamed {
            Ok(()) => Ok(()),
            Err(_) if dest.join("meta.json").exists() => {
                let _ = fsx::remove_any(staged);
                Ok(())
            }
            Err(err) => {
                let _ = fsx::remove_any(staged);
                Err(err)
            }
        }
    }

    pub fn mark_used(&self, key: &str) {
        let meta = self.entry_dir(key).join("meta.json");
        let recent = fs::metadata(&meta)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age < MARK_USED_INTERVAL);
        if !recent {
            fsx::touch(&meta);
        }
    }

    pub fn units_dir(&self) -> PathBuf {
        self.dirs.root().join(UNITS_DIR)
    }

    /// Concurrent updates of one record can lose a write; the orphaned entry stays until `frust gc`.
    pub fn note_unit_entry(&self, unit: &str, key: &str, keep: usize) -> Vec<String> {
        if !valid_key(unit) || !valid_key(key) {
            return Vec::new();
        }
        let path = Self::shard(self.units_dir(), unit);
        let mut keys: Vec<String> = fs::read_to_string(&path)
            .map(|text| text.lines().filter(|l| valid_key(l)).map(str::to_owned).collect())
            .unwrap_or_default();
        if keys.first().is_some_and(|k| k == key) && keys.len() <= keep {
            return Vec::new();
        }
        keys.retain(|k| k != key);
        keys.insert(0, key.to_string());
        let evicted = if keys.len() > keep { keys.split_off(keep) } else { Vec::new() };
        let _ = fsx::write_atomic(&path, format!("{}\n", keys.join("\n")).as_bytes());
        evicted
    }

    /// One rename makes the entry vanish, so a concurrent restore fails cleanly and recompiles; the files are deleted later by `sweep_evicted`.
    pub fn remove_entry(&self, key: &str) {
        if !valid_key(key) {
            return;
        }
        let evicted = evicted_dir(&self.dirs);
        let doomed = fsx::temp_sibling(&evicted.join("entry"));
        let source = self.entry_dir(key);
        let renamed = sys::rename(&source, &doomed);
        if renamed.as_ref().is_err_and(|e| e.kind() == io::ErrorKind::NotFound) && fs::create_dir_all(&evicted).is_ok() {
            let _ = sys::rename(&source, &doomed);
        }
    }
}

pub fn evicted_dir(dirs: &Dirs) -> PathBuf {
    dirs.tmp().join(EVICTED_DIR)
}

/// Deletes evicted entries that were removed from the store at least `older_than` ago and returns how many were deleted.
pub fn sweep_evicted(dirs: &Dirs, older_than: Duration) -> usize {
    let Ok(entries) = fs::read_dir(evicted_dir(dirs)) else {
        return 0;
    };
    let now = fsx::unix_now() as i64;
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = sys::lstat(&path) else { continue };
        // The rename updates the directory's ctime, which is the time it left the store.
        let evicted_at = meta.mtime().0.max(meta.ctime().0);
        if now - evicted_at < older_than.as_secs() as i64 {
            continue;
        }
        if fsx::remove_any(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

pub const UNITS_DIR: &str = "units";

/// Keys read back from disk are checked before they name a path.
pub fn valid_key(key: &str) -> bool {
    (32..=64).contains(&key.len()) && key.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
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
    let _ = fsx::write_atomic(&quarantine_marker(dirs, dev), format!("{}\n", fsx::unix_now()).as_bytes());
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
pub fn report_violation(dirs: &Dirs, krate: &str, input: &str, recorded: &str, fresh: &str, dev: u64) {
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
    if append_bytes(&log, format!("{record}\n").as_bytes()).is_some_and(|len| len > VIOLATIONS_MAX_BYTES)
        && let Ok(text) = fs::read_to_string(&log)
    {
        let _ = fsx::write_atomic(&log, newest_lines(&text, VIOLATIONS_MAX_BYTES as usize / 2).as_bytes());
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
    for entry in fs::read_dir(dirs.stamp_trust()).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(dev) = name.strip_prefix(QUARANTINE_PREFIX).and_then(|d| d.parse::<u64>().ok()) else {
            continue;
        };
        if let Ok(modified) = entry.metadata().and_then(|m| m.modified())
            && quarantine_active(modified, now)
        {
            summary.quarantined.push((dev, modified));
        }
    }
    summary.quarantined.sort();
    summary.violations = fs::read_to_string(dirs.stamp_trust().join(VIOLATIONS_LOG)).map_or(0, |text| text.lines().count());
    summary.audits_passed = fs::metadata(dirs.stamp_trust().join(AUDITS_FILE)).map_or(0, |m| m.len());
    summary
}

/// Lists keys of manifests in any format: the reader ignores `format`, and fields it does not know (a stamp's birth time) or lacks are skipped or defaulted.
pub fn manifest_keys(path: &Path) -> Option<Vec<String>> {
    let manifest: Manifest = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    Some(manifest.candidates.into_iter().map(|c| c.key).collect())
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
    fn unit_records_keep_the_most_recent_entries_and_report_the_rest() {
        let (dir, cache) = scratch("ring");
        let unit = key(0xaa);
        for n in 1..=3 {
            assert!(cache.note_unit_entry(&unit, &key(n), 3).is_empty());
        }
        assert!(cache.note_unit_entry(&unit, &key(1), 3).is_empty());
        assert_eq!(cache.note_unit_entry(&unit, &key(4), 3), vec![key(2)]);
        assert_eq!(cache.note_unit_entry(&unit, &key(5), 3), vec![key(3)]);
        assert!(cache.note_unit_entry(&unit, &key(5), 3).is_empty(), "noting the newest again changes nothing");
        assert!(cache.note_unit_entry(&key(0xbb), &key(9), 3).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn crafted_keys_never_name_paths() {
        let (dir, cache) = scratch("crafted");
        let outside = dir.join("outside");
        fs::create_dir_all(&outside).unwrap();
        for bad in ["../../outside", "", "ABCDEF0123456789ABCDEF0123456789", "0123456789abcdef0123456789abcdef/.."] {
            assert!(!valid_key(bad), "{bad:?}");
            cache.remove_entry(bad);
            assert!(cache.note_unit_entry(bad, &key(1), 2).is_empty());
            assert!(cache.note_unit_entry(&key(1), bad, 2).is_empty());
        }
        assert!(outside.is_dir());
        let unit = key(0xcc);
        assert!(cache.note_unit_entry(&unit, &key(1), 2).is_empty());
        let record = Cache::shard(cache.units_dir(), &unit);
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
        let candidate = |n: u8| Candidate { inputs: Vec::new(), envs: Vec::new(), key: key(n) };
        cache.add_candidate(&prekey, candidate(1));
        cache.add_candidate(&prekey, candidate(2));
        assert_eq!(cache.candidates(&prekey).len(), 2);
        cache.remove_entry(&key(1));
        assert!(!cache.entry_dir(&key(1)).exists());
        assert!(cache.entry_dir(&key(2)).join("meta.json").is_file());
        cache.add_candidate(&prekey, candidate(2));
        let keys: Vec<String> = cache.candidates(&prekey).into_iter().map(|c| c.key).collect();
        assert_eq!(keys, vec![key(2)], "the candidate of the removed entry is dropped");
        let _ = fs::remove_dir_all(&dir);
    }

    fn publish_entry(cache: &Cache, n: u8) {
        let staged = cache.stage().unwrap();
        fs::write(staged.join("meta.json"), b"{}").unwrap();
        fs::write(staged.join("f0"), vec![n; 1024]).unwrap();
        cache.publish(&staged, &key(n)).unwrap();
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
    fn manifest_format_three_is_current_and_format_two_is_ignored_yet_listed() {
        assert_eq!(MANIFEST_FORMAT, 3);
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
        assert!(cache.candidates(&prekey).is_empty(), "a format 2 manifest serves nothing");
        assert_eq!(manifest_keys(&path), Some(vec![key(1)]), "gc still lists the keys of a format 2 manifest");

        let candidate = Candidate {
            inputs: vec![Input { path: "a.rs".into(), hash: "ab".repeat(32), stamp: Some(stamp_record()) }],
            envs: Vec::new(),
            key: key(1),
        };
        cache.add_candidate(&prekey, candidate.clone());
        assert!(fs::read_to_string(&path).unwrap().contains("\"format\":3"));
        assert_eq!(cache.candidates(&prekey), vec![candidate]);
        assert_eq!(manifest_keys(&path), Some(vec![key(1)]), "and of a format 3 manifest");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn trust_modes_parse_every_spelling() {
        for off in ["0", "off", "false", "no"] {
            assert_eq!(TrustMode::parse(Some(off)), TrustMode::Off, "{off}");
        }
        assert_eq!(TrustMode::parse(Some("audit")), TrustMode::Audit);
        for on in [None, Some("on"), Some("1"), Some("true"), Some("yes"), Some(""), Some("bogus"), Some("OFF")] {
            assert_eq!(TrustMode::parse(on), TrustMode::On, "{on:?}");
        }
        assert_eq!(["off", "on", "audit"], [TrustMode::Off, TrustMode::On, TrustMode::Audit].map(TrustMode::name));
    }

    fn age_marker(dirs: &Dirs, dev: u64, age: Duration) {
        let when = SystemTime::now() - age;
        fs::File::options().write(true).open(quarantine_marker(dirs, dev)).unwrap().set_modified(when).unwrap();
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
        assert!(trust_summary(&dirs, SystemTime::now()).quarantined.is_empty());

        age_marker(&dirs, 7, Duration::from_secs(6 * 24 * 3600 + 3600));
        report_violation(&dirs, "leaf", "src/lib.rs", "aa", "bb", 7);
        let refreshed = quarantined_since(&dirs, 7, SystemTime::now()).expect("a violation quarantines the volume");
        assert!(refreshed.elapsed().unwrap_or_default() < Duration::from_secs(600), "the seven days restart");

        let now = SystemTime::now();
        assert!(quarantined_since(&dirs, 7, now + Duration::from_secs(6 * 24 * 3600)).is_some());
        assert!(quarantined_since(&dirs, 7, now + Duration::from_secs(8 * 24 * 3600)).is_none());
        assert!(quarantined_since(&dirs, 7, now - Duration::from_secs(3600)).is_some(), "a marker dated in the future still counts");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn violations_are_logged_as_json_lines_and_the_log_stays_bounded() {
        let (dir, _) = scratch("violations");
        let dirs = Dirs::at(&dir);
        report_violation(&dirs, "leaf", "src/lib.rs", "aa", "bb", 12);
        let log = dirs.stamp_trust().join(VIOLATIONS_LOG);
        let first: serde_json::Value = serde_json::from_str(fs::read_to_string(&log).unwrap().trim()).unwrap();
        assert_eq!(first["crate"], "leaf");
        assert_eq!(first["input"], "src/lib.rs");
        assert_eq!((first["recorded"].as_str(), first["fresh"].as_str()), (Some("aa"), Some("bb")));
        assert_eq!(first["dev"], 12);
        assert!(first["t"].as_u64().is_some_and(|t| t > 0));

        for i in 0..60 {
            report_violation(&dirs, "leaf", &format!("{i}-{}", "x".repeat(10_000)), "aa", "bb", 12);
        }
        let text = fs::read_to_string(&log).unwrap();
        assert!(text.len() as u64 <= VIOLATIONS_MAX_BYTES, "{} bytes", text.len());
        assert!(text.lines().all(|l| serde_json::from_str::<serde_json::Value>(l).is_ok()), "only whole lines remain");
        assert!(text.lines().last().unwrap().contains("\"input\":\"59-"), "the newest line is kept");
        assert!(!text.contains("\"input\":\"src/lib.rs\""), "the oldest lines are dropped first");

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
        fs::File::options().write(true).open(&path).unwrap().set_len(AUDITS_MAX_BYTES).unwrap();
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
                Input { path: "src/lib.rs".into(), hash: "ab".repeat(32), stamp: Some(stamp_record()) },
                Input { path: "src/new.rs".into(), hash: "cd".repeat(32), stamp: None },
            ],
            envs: vec![("A".into(), None)],
            key: key(1),
        };
        cache.add_candidate(&prekey, candidate.clone());
        assert_eq!(cache.candidates(&prekey), vec![candidate.clone()]);

        let path = cache.manifest_path(&prekey);
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains(&format!("\"format\":{MANIFEST_FORMAT}")), "{text}");
        assert!(!text.contains("\"stamp\":null"), "an unrecorded stamp is left out");

        let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
        value.as_object_mut().unwrap().remove("format");
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(cache.candidates(&prekey).is_empty(), "a manifest without the current format is ignored");
        assert_eq!(manifest_keys(&path), Some(vec![key(1)]), "gc can still list its keys");

        cache.add_candidate(&prekey, candidate.clone());
        assert_eq!(cache.candidates(&prekey), vec![candidate], "adding rewrites it in the current format");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn adding_an_identical_front_candidate_leaves_the_manifest_alone() {
        let (dir, cache) = scratch("idempotent");
        publish_entry(&cache, 1);
        publish_entry(&cache, 2);
        let prekey = key(0x79);
        let candidate = |n: u8, stamp: Option<StampRecord>| Candidate {
            inputs: vec![Input { path: "a.rs".into(), hash: "ef".repeat(32), stamp }],
            envs: Vec::new(),
            key: key(n),
        };
        cache.add_candidate(&prekey, candidate(1, None));
        cache.add_candidate(&prekey, candidate(2, None));
        let path = cache.manifest_path(&prekey);
        let inode = sys::stat(&path).unwrap().ino();

        cache.add_candidate(&prekey, candidate(2, None));
        assert_eq!(sys::stat(&path).unwrap().ino(), inode, "nothing changed, so nothing was rewritten");

        cache.add_candidate(&prekey, candidate(2, Some(stamp_record())));
        assert_eq!(cache.candidates(&prekey)[0], candidate(2, Some(stamp_record())), "a newly recorded stamp is kept");

        cache.add_candidate(&prekey, candidate(1, None));
        let keys: Vec<String> = cache.candidates(&prekey).into_iter().map(|c| c.key).collect();
        assert_eq!(keys, vec![key(1), key(2)], "a different front reorders");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn candidates_are_capped_and_a_missing_shard_directory_is_created_on_demand() {
        let (dir, cache) = scratch("cap");
        let prekey = key(0x7a);
        for n in 1..=(MAX_CANDIDATES as u8 + 3) {
            publish_entry(&cache, n);
            cache.add_candidate(&prekey, Candidate { inputs: Vec::new(), envs: Vec::new(), key: key(n) });
        }
        let keys: Vec<String> = cache.candidates(&prekey).into_iter().map(|c| c.key).collect();
        assert_eq!(keys.len(), MAX_CANDIDATES);
        assert_eq!(keys[0], key(MAX_CANDIDATES as u8 + 3), "newest first");
        assert!(cache.has_entry(&key(1)) && !cache.has_entry("../x") && !cache.has_entry(&key(0x42)));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn mark_used_touches_only_entries_unused_for_an_hour() {
        let (dir, cache) = scratch("used");
        publish_entry(&cache, 1);
        let meta = cache.entry_dir(&key(1)).join("meta.json");
        let set = |age: Duration| {
            let when = std::time::SystemTime::now() - age;
            fs::File::options().write(true).open(&meta).unwrap().set_modified(when).unwrap();
        };
        let age = || fs::metadata(&meta).unwrap().modified().unwrap().elapsed().unwrap();
        set(Duration::from_secs(600));
        cache.mark_used(&key(1));
        assert!(age() >= Duration::from_secs(300), "a recently used entry is not touched");

        set(Duration::from_secs(2 * 3600));
        cache.mark_used(&key(1));
        assert!(age() < Duration::from_secs(300), "an old entry is refreshed");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn removed_entries_wait_in_the_evicted_directory_until_swept() {
        let (dir, cache) = scratch("sweep");
        let dirs = Dirs::at(&dir);
        publish_entry(&cache, 1);
        publish_entry(&cache, 2);
        cache.remove_entry(&key(1));
        assert!(!cache.entry_dir(&key(1)).exists() && !cache.has_entry(&key(1)), "gone from the store at once");
        assert!(cache.has_entry(&key(2)));
        let parked = || fs::read_dir(evicted_dir(&dirs)).unwrap().count();
        assert_eq!(parked(), 1);
        assert!(evicted_dir(&dirs).starts_with(dirs.tmp()));

        assert_eq!(sweep_evicted(&dirs, Duration::from_secs(3600)), 0, "too recent to delete");
        assert_eq!(parked(), 1);
        cache.remove_entry(&key(1));
        assert_eq!(parked(), 1, "removing an absent entry parks nothing");
        cache.remove_entry(&key(2));
        assert_eq!(parked(), 2);
        assert_eq!(sweep_evicted(&dirs, Duration::ZERO), 2);
        assert_eq!(parked(), 0);
        assert_eq!(sweep_evicted(&Dirs::at(dir.join("nowhere")), Duration::ZERO), 0);
        let _ = fs::remove_dir_all(&dir);
    }
}
