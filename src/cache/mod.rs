//! Direct-mode compile cache: a pre-key selects a manifest of recorded input sets, a match yields an entry key.

pub mod depinfo;
pub mod hashing;
pub mod rustc_args;

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::fsx;
use crate::paths::Dirs;

/// Replaces the profile directory in stored text so entries are relocatable between target directories.
pub const PROFILE_TOKEN: &str = "{{FRUST_PROFILE_DIR}}";

const MAX_CANDIDATES: usize = 8;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredFile {
    pub name: String,
    pub stored: String,
    pub text: bool,
    pub bytes: u64,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    pub inputs: Vec<(String, String)>,
    pub envs: Vec<(String, Option<String>)>,
    pub key: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Manifest {
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

    pub fn candidates(&self, prekey: &str) -> Vec<Candidate> {
        let Ok(bytes) = fs::read(self.manifest_path(prekey)) else {
            return Vec::new();
        };
        serde_json::from_slice::<Manifest>(&bytes).map(|m| m.candidates).unwrap_or_default()
    }

    /// Drops candidates whose entry is gone so lookups do not hash their inputs for nothing.
    pub fn add_candidate(&self, prekey: &str, candidate: Candidate) {
        let mut manifest = Manifest { candidates: self.candidates(prekey) };
        manifest.candidates.retain(|c| c.key != candidate.key && self.entry_dir(&c.key).join("meta.json").is_file());
        manifest.candidates.insert(0, candidate);
        manifest.candidates.truncate(MAX_CANDIDATES);
        if let Ok(json) = serde_json::to_vec(&manifest) {
            let _ = fsx::write_atomic(&self.manifest_path(prekey), &json);
        }
    }

    pub fn load_entry(&self, key: &str) -> Option<(PathBuf, EntryMeta)> {
        let dir = self.entry_dir(key);
        let meta: EntryMeta = serde_json::from_slice(&fs::read(dir.join("meta.json")).ok()?).ok()?;
        Some((dir, meta))
    }

    pub fn stage(&self) -> std::io::Result<PathBuf> {
        let dir = fsx::temp_sibling(&self.dirs.tmp().join("entry"));
        fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    pub fn publish(&self, staged: &Path, key: &str) -> std::io::Result<()> {
        let dest = self.entry_dir(key);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        match fs::rename(staged, &dest) {
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
        fsx::touch(&self.entry_dir(key).join("meta.json"));
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

    /// One rename makes the entry vanish, so a concurrent restore fails cleanly and recompiles.
    pub fn remove_entry(&self, key: &str) {
        if !valid_key(key) {
            return;
        }
        let doomed = fsx::temp_sibling(&self.dirs.tmp().join("evicted"));
        if fs::create_dir_all(self.dirs.tmp()).is_ok() && fs::rename(self.entry_dir(key), &doomed).is_ok() {
            let _ = fsx::remove_any(&doomed);
        }
    }
}

pub const UNITS_DIR: &str = "units";

/// Keys read back from disk are checked before they name a path.
pub fn valid_key(key: &str) -> bool {
    (32..=64).contains(&key.len()) && key.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

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
}
