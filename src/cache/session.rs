//! The rustc incremental session an incremental unit's compile left, kept with its entry so a restore leaves the next edit an incremental rebuild.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::fsx;
use crate::sys;

/// The entry subdirectory holding the session's files.
pub const DIR: &str = "session";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredSession {
    /// rustc's directory for the crate under the incremental directory: `<crate>-<stable crate id>`.
    pub crate_dir: String,
    /// The finalized session directory: `s-<time>-<random>-<svh>`.
    pub name: String,
    pub bytes: u64,
}

impl StoredSession {
    pub fn valid(&self) -> bool {
        crate_id(&self.crate_dir).is_some() && lock_name(&self.name).is_some()
    }

    fn lock_name(&self) -> Option<String> {
        lock_name(&self.name)
    }
}

fn crate_id(name: &str) -> Option<&str> {
    let (krate, id) = name.rsplit_once('-')?;
    let word = |s: &str, underscore: bool| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || (underscore && b == b'_'));
    (word(krate, true) && word(id, false)).then_some(krate)
}

/// The lock rustc pairs with a finalized session; `None` for any other name, a working directory included.
fn lock_name(session: &str) -> Option<String> {
    let parts: Vec<&str> = session.strip_prefix("s-")?.split('-').collect();
    let alnum = |s: &&str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric());
    (parts.len() == 3 && parts.iter().all(alnum) && parts[2] != "working").then(|| format!("s-{}-{}.lock", parts[0], parts[1]))
}

/// Finalized session names per crate directory of `crate_name`.
pub type Snapshot = BTreeMap<String, BTreeSet<String>>;

pub fn snapshot(incremental: &Path, crate_name: &str) -> Snapshot {
    let mut found = Snapshot::new();
    let Ok(crates) = fs::read_dir(incremental) else { return found };
    for entry in crates.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else { continue };
        if crate_id(&name) != Some(crate_name) {
            continue;
        }
        let sessions = fs::read_dir(entry.path())
            .map(|rd| rd.flatten().filter_map(|e| e.file_name().to_str().filter(|n| lock_name(n).is_some()).map(str::to_owned)).collect())
            .unwrap_or_default();
        found.insert(name, sessions);
    }
    found
}

/// A finalized session a compile left under the incremental directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Produced {
    pub crate_dir: String,
    pub name: String,
}

impl Produced {
    pub fn valid(&self) -> bool {
        crate_id(&self.crate_dir).is_some() && lock_name(&self.name).is_some()
    }
}

/// `None` unless exactly one crate directory gained a session since `before`, so a concurrent compile of a same-named crate is never mistaken for this one.
pub fn produced(incremental: &Path, crate_name: &str, before: &Snapshot) -> Option<Produced> {
    let mut gained = snapshot(incremental, crate_name).into_iter().filter_map(|(crate_dir, now)| {
        let earlier = before.get(&crate_dir);
        let name = now.into_iter().filter(|name| earlier.is_none_or(|set| !set.contains(name))).max()?;
        Some(Produced { crate_dir, name })
    });
    let one = gained.next()?;
    gained.next().is_none().then_some(one)
}

/// Shares the session into `entry` by clone or hard link, never by copy; rustc deletes a session only under an exclusive lock, so the shared one keeps it whole meanwhile.
pub fn keep(incremental: &Path, produced: &Produced, entry: &Path) -> io::Result<StoredSession> {
    let mut session = StoredSession { crate_dir: produced.crate_dir.clone(), name: produced.name.clone(), bytes: 0 };
    let Some(lock) = session.lock_name().filter(|_| session.valid()) else {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid session name"));
    };
    let crate_dir = incremental.join(&session.crate_dir);
    let _held = sys::try_session_lock_shared(&crate_dir.join(lock))?;
    let dst = entry.join(DIR);
    share_tree(&crate_dir.join(&session.name), &dst, false)?;
    session.bytes = tree_bytes(&dst);
    Ok(session)
}

/// Whether an entry still holds the session its record names; a session given up for a newer one leaves the record as it was.
pub fn present(entry: &Path) -> bool {
    sys::lstat(&entry.join(DIR)).is_ok_and(|meta| meta.is_dir())
}

#[derive(Debug, PartialEq, Eq)]
pub enum Placed {
    Restored,
    Present,
}

/// Replaces the crate's sessions with the stored one; a lock rustc holds there fails with `WouldBlock` and leaves the directory alone.
pub fn restore(stored: &Path, incremental: &Path, session: &StoredSession) -> io::Result<Placed> {
    let Some(lock) = session.lock_name().filter(|_| session.valid()) else {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid session name"));
    };
    let crate_dir = incremental.join(&session.crate_dir);
    fs::create_dir_all(&crate_dir)?;
    let (mut locks, mut dirs) = (Vec::new(), Vec::new());
    for entry in fs::read_dir(&crate_dir)? {
        let name = entry?.file_name();
        let Some(name) = name.to_str().filter(|name| name.starts_with("s-")) else { continue };
        if name.ends_with(".lock") {
            locks.push(crate_dir.join(name));
        } else {
            dirs.push(name.to_owned());
        }
    }
    if dirs == [session.name.as_str()] && locks == [crate_dir.join(&lock)] {
        return Ok(Placed::Present);
    }
    let _held = sys::try_session_locks(&locks)?;
    let staged = fsx::temp_sibling(&crate_dir.join(&session.name));
    share_tree(stored, &staged, true)?;
    let placed = (|| {
        for name in &dirs {
            remove_if_present(&crate_dir.join(name))?;
        }
        for path in &locks {
            remove_if_present(path)?;
        }
        let target = crate_dir.join(&session.name);
        sys::rename(&staged, &target)?;
        File::create(crate_dir.join(&lock))?;
        // Clones keep the stored times, which session cleanup would read as long abandoned.
        fsx::touch(&target);
        Ok(Placed::Restored)
    })();
    if placed.is_err() {
        let _ = fsx::remove_any(&staged);
    }
    placed
}

/// Gives up an entry's session (a newer state of its unit now holds one); the directory leaves the entry by one rename into `tmp`.
pub fn strip(entry: &Path, tmp: &Path) -> bool {
    let from = entry.join(DIR);
    if !present(entry) || fs::create_dir_all(tmp).is_err() {
        return false;
    }
    let parked = fsx::temp_sibling(&tmp.join(DIR));
    if sys::rename(&from, &parked).is_err() {
        return false;
    }
    let _ = fs::remove_dir_all(&parked);
    true
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match fsx::remove_any(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// A clone of the whole tree where the volume has them, else hard links (rustc never rewrites a session file in place), else copies when `copy` allows.
fn share_tree(src: &Path, dst: &Path, copy: bool) -> io::Result<()> {
    if sys::reflink(src, dst).is_ok() {
        return Ok(());
    }
    let _ = fsx::remove_any(dst);
    link_tree(src, dst, copy).inspect_err(|_| {
        let _ = fsx::remove_any(dst);
    })
}

fn link_tree(src: &Path, dst: &Path, copy: bool) -> io::Result<()> {
    fs::create_dir(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let (from, to, kind) = (entry.path(), dst.join(entry.file_name()), entry.file_type()?);
        if kind.is_dir() {
            link_tree(&from, &to, copy)?;
        } else if !kind.is_file() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "session holds a special file"));
        } else if let Err(error) = fs::hard_link(&from, &to) {
            if !copy {
                return Err(error);
            }
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

fn tree_bytes(path: &Path) -> u64 {
    let Ok(meta) = sys::lstat(path) else { return 0 };
    if !meta.is_dir() {
        return meta.len();
    }
    fs::read_dir(path).map(|rd| rd.flatten().map(|e| tree_bytes(&e.path())).sum()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("frust-session-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn session(dir: &Path, crate_dir: &str, name: &str) {
        let path = dir.join(crate_dir).join(name);
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("dep-graph.bin"), name).unwrap();
        fs::write(dir.join(crate_dir).join(lock_name(name).unwrap()), "").unwrap();
    }

    #[test]
    fn names_follow_rustc_layout() {
        assert_eq!(lock_name("s-hnzl07lxxl-180pwel-cfa29u7e4u7ifoptazbhcx8qo").as_deref(), Some("s-hnzl07lxxl-180pwel.lock"));
        for bad in ["s-hnzl07lxxl-180pwel-working", "s-a-b", "s-a-b-c-d", "s-a-b-c.lock", "x-a-b-c", "s-a--c", "s-../b-c"] {
            assert_eq!(lock_name(bad), None, "{bad}");
        }
        assert_eq!(crate_id("bat-0k803ssrxdfis"), Some("bat"));
        assert_eq!(crate_id("build_script_build-3082tqza3666j"), Some("build_script_build"));
        for bad in ["bat", "-x", "bat-", "a/b-c", "a-b.c"] {
            assert_eq!(crate_id(bad), None, "{bad}");
        }
    }

    #[test]
    fn only_a_single_gaining_crate_directory_is_attributed() {
        let dir = scratch("produced");
        session(&dir, "app-aaa", "s-1-1-x");
        session(&dir, "app-bbb", "s-1-2-x");
        session(&dir, "other-ccc", "s-1-3-x");
        let before = snapshot(&dir, "app");
        assert_eq!(before.len(), 2);
        assert_eq!(produced(&dir, "app", &before), None, "nothing new");
        session(&dir, "app-aaa", "s-2-1-y");
        session(&dir, "other-ccc", "s-2-3-y");
        assert_eq!(produced(&dir, "app", &before), Some(Produced { crate_dir: "app-aaa".into(), name: "s-2-1-y".into() }));
        session(&dir, "app-ddd", "s-3-1-z");
        assert_eq!(produced(&dir, "app", &before), None, "two crate directories gained a session");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn restore_replaces_the_crate_sessions_and_strip_gives_the_entry_session_up() {
        let dir = scratch("restore");
        let incremental = dir.join("incremental");
        session(&incremental, "app-aaa", "s-1-1-old");
        fs::create_dir_all(incremental.join("app-aaa").join("s-1-2-working")).unwrap();
        session(&dir.join("src"), "app-aaa", "s-5-5-new");
        let entry = dir.join("entry");
        fs::create_dir_all(&entry).unwrap();
        let produced = Produced { crate_dir: "app-aaa".into(), name: "s-5-5-new".into() };
        let stored = keep(&dir.join("src"), &produced, &entry).unwrap();
        assert_eq!(stored, StoredSession { crate_dir: "app-aaa".into(), name: "s-5-5-new".into(), bytes: "s-5-5-new".len() as u64 });
        let missing = Produced { name: "s-6-6-gone".into(), ..produced.clone() };
        assert!(keep(&dir.join("src"), &missing, &dir.join("other")).is_err(), "a session without its lock file is not read");
        assert_eq!(restore(&entry.join(DIR), &incremental, &stored).unwrap(), Placed::Restored);
        let mut names: Vec<String> = fs::read_dir(incremental.join("app-aaa")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        assert_eq!(names, ["s-5-5-new", "s-5-5.lock"]);
        assert_eq!(fs::read_to_string(incremental.join("app-aaa").join("s-5-5-new").join("dep-graph.bin")).unwrap(), "s-5-5-new");
        assert_eq!(restore(&entry.join(DIR), &incremental, &stored).unwrap(), Placed::Present);
        let bad = StoredSession { name: "s-5-5-working".into(), ..stored.clone() };
        assert!(restore(&entry.join(DIR), &incremental, &bad).is_err());
        assert!(present(&entry) && strip(&entry, &dir.join("tmp")) && !present(&entry));
        assert!(!strip(&entry, &dir.join("tmp")));
        assert_eq!(fs::read_dir(dir.join("tmp")).unwrap().count(), 0, "the parked directory is removed");
        fs::remove_dir_all(&dir).unwrap();
    }
}
