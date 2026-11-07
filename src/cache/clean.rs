//! Which workspaces use a stored entry, and releasing what only one workspace needs.

use std::fs;
use std::path::{Component, Path};

use serde::Deserialize;

use super::{Cache, UnitRecordsGuard, valid_key};
use crate::fsx;
use crate::paths::Dirs;

pub const SOURCE_REGISTRY: &str = "registry";
pub const SOURCE_GIT: &str = "git";
pub const SOURCE_PATH: &str = "path";

/// Past this many users the oldest is forgotten; an entry then merely looks less shared than it is.
const MAX_WORKSPACES: usize = 16;
const LOOSE: &str = "_loose";
/// Recorded when a build outside any central target uses an entry, so no workspace clean counts the entry as its own.
pub const UNATTRIBUTED: &str = "_other";

#[derive(Deserialize)]
struct Attribution {
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    workspaces: Vec<String>,
}

/// The `targets/<workspace>` directory name a profile directory lives under; `None` for local, explicit and loose targets.
pub fn workspace_of(dirs: &Dirs, profile: &Path) -> Option<String> {
    let rest = profile.strip_prefix(dirs.targets()).ok()?;
    match rest.components().next()? {
        Component::Normal(name) => name.to_str().filter(|id| *id != LOOSE).map(str::to_owned),
        _ => None,
    }
}

fn mentions(bytes: &[u8], needle: &str) -> bool {
    let needle = needle.as_bytes();
    !needle.is_empty() && bytes.windows(needle.len()).any(|window| window == needle)
}

impl Cache {
    /// Adds the workspace to an attributed entry; entries stored before attribution existed are left unattributed.
    pub fn note_workspace(&self, key: &str, workspace: &str) {
        self.edit_workspaces(key, false, |list| {
            if list.iter().any(|known| known == workspace) {
                return false;
            }
            list.push(workspace.to_owned());
            if list.len() > MAX_WORKSPACES {
                list.remove(0);
            }
            true
        });
    }

    /// Rewrites `meta.json` through a generic value so fields written by newer versions survive; `keep_stamp` leaves the last-use time alone.
    fn edit_workspaces(&self, key: &str, keep_stamp: bool, edit: impl FnOnce(&mut Vec<String>) -> bool) {
        if !valid_key(key) {
            return;
        }
        let Ok(_guard) = UnitRecordsGuard::acquire(&self.dirs) else { return };
        let path = self.entry_dir(key).join("meta.json");
        let Ok(stat) = fs::symlink_metadata(&path) else { return };
        if !stat.is_file() {
            return;
        }
        let Ok(bytes) = fs::read(&path) else { return };
        let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(&bytes) else { return };
        let Some(object) = value.as_object_mut() else { return };
        if object.get("source").and_then(|source| source.as_str()).is_none() {
            return;
        }
        let mut list: Vec<String> = object
            .get("workspaces")
            .and_then(|list| serde_json::from_value(list.clone()).ok())
            .unwrap_or_default();
        if !edit(&mut list) {
            return;
        }
        object.insert("workspaces".to_owned(), serde_json::json!(list));
        let Ok(out) = serde_json::to_vec(&value) else { return };
        if fsx::write_atomic(&path, &out).is_err() {
            return;
        }
        if keep_stamp
            && let Ok(modified) = stat.modified()
            && let Ok(file) = fs::File::options().write(true).open(&path)
        {
            let _ = file.set_modified(modified);
        }
    }

    /// Retires path entries only this workspace used, and with `evict_registry` registry and git entries too; returns the entries and bytes released.
    pub fn release_workspace(&self, workspace: &str, evict_registry: bool) -> (usize, u64) {
        if workspace.is_empty() || workspace == LOOSE {
            return (0, 0);
        }
        let Ok(shards) = fs::read_dir(self.dirs.store()) else { return (0, 0) };
        let (mut entries, mut bytes) = (0, 0);
        for shard in shards.flatten() {
            let Ok(children) = fs::read_dir(shard.path()) else { continue };
            for child in children.flatten() {
                let Some(key) = child.file_name().to_str().filter(|name| valid_key(name)).map(str::to_owned) else { continue };
                if let Some(released) = self.release_entry(&key, workspace, evict_registry) {
                    entries += 1;
                    bytes += released;
                }
            }
        }
        (entries, bytes)
    }

    /// The snapshot is taken before `meta.json` is read, so a concurrent hit that adds a user makes the retirement decline.
    fn release_entry(&self, key: &str, workspace: &str, evict_registry: bool) -> Option<u64> {
        let snapshot = self.retirement_snapshot(key)?;
        let dir = self.entry_dir(key);
        let bytes = fs::read(dir.join("meta.json")).ok()?;
        if !mentions(&bytes, workspace) {
            return None;
        }
        let attribution: Attribution = serde_json::from_slice(&bytes).ok()?;
        let source = attribution.source?;
        if !attribution.workspaces.iter().any(|known| known == workspace) {
            return None;
        }
        let exclusive = attribution.workspaces.len() == 1;
        let droppable = exclusive
            && match source.as_str() {
                SOURCE_PATH => true,
                SOURCE_REGISTRY | SOURCE_GIT => evict_registry,
                _ => false,
            };
        if !droppable {
            // A kept entry only this workspace used keeps its id, so a later clean with the flag can still release it.
            if !exclusive {
                self.edit_workspaces(key, true, |list| {
                    let before = list.len();
                    list.retain(|known| known != workspace);
                    list.len() != before
                });
            }
            return None;
        }
        let size = fsx::allocated_size(&dir);
        let guard = UnitRecordsGuard::acquire(&self.dirs).ok()?;
        let retired = self.retire_entry_guarded(&guard, key, Some(&snapshot)).ok().flatten();
        drop(guard);
        let entry = retired?;
        // A failed delete stays queued under tmp/evicted for maintenance.
        if entry.proof.valid(&self.dirs, &entry.path) {
            let _ = fsx::remove_any(&entry.path);
        }
        Some(size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    fn key(n: u8) -> String {
        format!("{n:02x}").repeat(20)
    }

    fn scratch(tag: &str) -> (PathBuf, Cache) {
        let dir = std::env::temp_dir().join(format!("frust-clean-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let cache = Cache::new(&Dirs::at(&dir));
        (dir, cache)
    }

    fn publish(cache: &Cache, n: u8, attribution: Option<(&str, &[&str])>) {
        let mut meta = serde_json::json!({"crate_name": "c", "version": "1", "compile_ms": 1, "created_unix": 1, "files": [], "stderr": "stderr"});
        if let Some((source, workspaces)) = attribution {
            meta["source"] = source.into();
            meta["workspaces"] = serde_json::json!(workspaces);
        }
        let staged = cache.stage().unwrap();
        fs::write(staged.join("meta.json"), serde_json::to_vec(&meta).unwrap()).unwrap();
        fs::write(staged.join("f0"), vec![n; 4096]).unwrap();
        cache.publish(&staged, &key(n)).unwrap();
    }

    fn workspaces(cache: &Cache, n: u8) -> Vec<String> {
        let (_, meta) = cache.load_entry(&key(n)).unwrap();
        meta.workspaces
    }

    fn set_age(cache: &Cache, n: u8, age: Duration) {
        let file = fs::File::options().write(true).open(cache.entry_dir(&key(n)).join("meta.json")).unwrap();
        file.set_modified(SystemTime::now() - age).unwrap();
    }

    #[test]
    fn release_drops_path_entries_owned_only_by_the_workspace() {
        let (dir, cache) = scratch("path");
        publish(&cache, 1, Some((SOURCE_PATH, &["a"])));
        publish(&cache, 2, Some((SOURCE_PATH, &["a", "b"])));
        publish(&cache, 3, Some((SOURCE_PATH, &["b"])));
        publish(&cache, 4, Some((SOURCE_REGISTRY, &["a"])));
        publish(&cache, 5, None);
        let (entries, bytes) = cache.release_workspace("a", false);
        assert_eq!(entries, 1);
        assert!(bytes > 0);
        assert!(!cache.has_entry(&key(1)));
        for kept in [2, 3, 4, 5] {
            assert!(cache.has_entry(&key(kept)), "entry {kept}");
        }
        assert_eq!(workspaces(&cache, 2), vec!["b"], "the released workspace leaves shared entries");
        assert_eq!(workspaces(&cache, 3), vec!["b"]);
        assert_eq!(workspaces(&cache, 4), vec!["a"], "registry entries stay by default");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn release_with_the_flag_drops_exclusive_registry_and_git_entries_only() {
        let (dir, cache) = scratch("registry");
        publish(&cache, 1, Some((SOURCE_REGISTRY, &["a"])));
        publish(&cache, 2, Some((SOURCE_GIT, &["a"])));
        publish(&cache, 3, Some((SOURCE_REGISTRY, &["a", "b"])));
        publish(&cache, 4, None);
        let (entries, _) = cache.release_workspace("a", true);
        assert_eq!(entries, 2);
        assert!(!cache.has_entry(&key(1)) && !cache.has_entry(&key(2)));
        assert!(cache.has_entry(&key(3)) && cache.has_entry(&key(4)));
        assert_eq!(workspaces(&cache, 3), vec!["b"]);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn release_ignores_entries_without_attribution_and_the_loose_workspace() {
        let (dir, cache) = scratch("legacy");
        publish(&cache, 1, None);
        publish(&cache, 2, Some((SOURCE_PATH, &[LOOSE])));
        assert_eq!(cache.release_workspace("a", true), (0, 0));
        assert_eq!(cache.release_workspace(LOOSE, true), (0, 0));
        assert_eq!(cache.release_workspace("", true), (0, 0));
        assert!(cache.has_entry(&key(1)) && cache.has_entry(&key(2)));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn removing_a_user_from_a_shared_entry_keeps_its_last_use_time() {
        let (dir, cache) = scratch("stamp");
        publish(&cache, 1, Some((SOURCE_REGISTRY, &["a", "b"])));
        set_age(&cache, 1, Duration::from_secs(30 * 24 * 3600));
        cache.release_workspace("a", false);
        let modified = fs::metadata(cache.entry_dir(&key(1)).join("meta.json")).unwrap().modified().unwrap();
        assert!(modified.elapsed().unwrap() >= Duration::from_secs(29 * 24 * 3600));
        assert_eq!(workspaces(&cache, 1), vec!["b"]);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn noting_a_workspace_appends_once_caps_the_list_and_skips_unattributed_entries() {
        let (dir, cache) = scratch("note");
        publish(&cache, 1, Some((SOURCE_REGISTRY, &["a"])));
        publish(&cache, 2, None);
        cache.note_workspace(&key(1), "b");
        cache.note_workspace(&key(1), "b");
        assert_eq!(workspaces(&cache, 1), vec!["a", "b"]);
        for n in 0..MAX_WORKSPACES {
            cache.note_workspace(&key(1), &format!("w{n}"));
        }
        let list = workspaces(&cache, 1);
        assert_eq!(list.len(), MAX_WORKSPACES);
        assert_eq!(list.last().unwrap(), &format!("w{}", MAX_WORKSPACES - 1));
        assert!(!list.contains(&"a".to_string()), "the oldest user is forgotten first");
        cache.note_workspace(&key(2), "a");
        assert!(workspaces(&cache, 2).is_empty());
        let meta = fs::read_to_string(cache.entry_dir(&key(2)).join("meta.json")).unwrap();
        assert!(!meta.contains("workspaces"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rewriting_attribution_preserves_fields_this_version_does_not_know() {
        let (dir, cache) = scratch("unknown");
        publish(&cache, 1, Some((SOURCE_REGISTRY, &["a"])));
        let path = cache.entry_dir(&key(1)).join("meta.json");
        let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        value["future"] = serde_json::json!({"x": 1});
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        cache.note_workspace(&key(1), "b");
        let after: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(after["future"], serde_json::json!({"x": 1}));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn workspace_names_come_from_the_central_target_layout() {
        let dirs = Dirs::at("/cache/frust");
        let targets = dirs.targets();
        assert_eq!(workspace_of(&dirs, &targets.join("app-1a2b").join("tc").join("debug")).as_deref(), Some("app-1a2b"));
        assert_eq!(workspace_of(&dirs, &targets.join("_loose").join("tc").join("debug")), None);
        assert_eq!(workspace_of(&dirs, Path::new("/work/app/target/debug")), None);
        assert_eq!(workspace_of(&dirs, &targets), None);
    }
}
