//! Content hashing; a memoised hash is reused only if the stamp is unchanged and the file had settled before hashing.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::fsx;

/// Covers the coarsest common timestamp granularity (2 s on FAT) and clock-tick lag.
pub const SETTLE_MARGIN_NS: i128 = 2_000_000_000;

/// Reading a memo record costs about as much as hashing a smaller file.
const PERSIST_MIN_BYTES: u64 = 32 * 1024;

/// Beyond these limits the invocation is not cached rather than keyed by an incomplete hash.
pub const DIR_MAX_FILES: usize = 20_000;
pub const DIR_MAX_BYTES: u64 = 4 * 1024 * 1024 * 1024;

const READ_BUF: usize = 256 * 1024;

thread_local! {
    static BUFFER: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime_ns: i128,
    pub ctime_ns: i128,
}

impl Stamp {
    pub fn of(meta: &fs::Metadata) -> Stamp {
        Stamp {
            dev: meta.dev(),
            ino: meta.ino(),
            size: meta.len(),
            mtime_ns: meta.mtime() as i128 * 1_000_000_000 + meta.mtime_nsec() as i128,
            ctime_ns: meta.ctime() as i128 * 1_000_000_000 + meta.ctime_nsec() as i128,
        }
    }

    pub fn read(path: &Path) -> io::Result<Stamp> {
        fs::metadata(path).map(|m| Stamp::of(&m))
    }

    /// Whole-second timestamps suggest a coarse-granularity filesystem.
    pub fn coarse(&self) -> bool {
        self.mtime_ns % 1_000_000_000 == 0 && self.ctime_ns % 1_000_000_000 == 0
    }

    pub fn changed_since(&self, t_ns: i128, margin_ns: i128) -> bool {
        let limit = t_ns - margin_ns;
        self.mtime_ns >= limit || self.ctime_ns >= limit
    }

    /// ccache's "file too new" test: a file changed after the compiler started may have been read in its old state.
    pub fn too_new_for(&self, started_ns: i128) -> bool {
        let margin = if self.coarse() { SETTLE_MARGIN_NS } else { 100_000_000 };
        self.changed_since(started_ns, margin)
    }
}

pub fn now_ns() -> i128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as i128).unwrap_or(0)
}

pub fn hash_reader(reader: &mut impl Read) -> io::Result<String> {
    BUFFER.with(|cell| {
        let mut buf = cell.borrow_mut();
        if buf.len() < READ_BUF {
            buf.resize(READ_BUF, 0);
        }
        let mut hasher = blake3::Hasher::new();
        loop {
            let n = match reader.read(&mut buf[..]) {
                Ok(0) => break,
                Ok(n) => n,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(err),
            };
            hasher.update(&buf[..n]);
        }
        Ok(hasher.finalize().to_hex().to_string())
    })
}

#[derive(Debug, Clone)]
pub struct Hashed {
    pub hash: String,
    pub stamp: Stamp,
    pub stable: bool,
    pub at_ns: i128,
}

impl Hashed {
    fn reusable(&self) -> bool {
        self.stable && !self.stamp.changed_since(self.at_ns, SETTLE_MARGIN_NS)
    }
}

pub fn hash_with_stamp(path: &Path) -> io::Result<Hashed> {
    let at_ns = now_ns();
    let mut file = File::open(path)?;
    let stamp = Stamp::of(&file.metadata()?);
    let hash = hash_reader(&mut file)?;
    let stable = Stamp::read(path).map(|after| after == stamp).unwrap_or(false);
    Ok(Hashed { hash, stamp, stable, at_ns })
}

pub struct Hasher {
    memo: RefCell<HashMap<PathBuf, Hashed>>,
    persist_dir: Option<PathBuf>,
    immutable_roots: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirHash {
    Hash(String),
    Unverifiable(String),
}

impl Hasher {
    pub fn new(persist_dir: Option<PathBuf>, immutable_roots: Vec<PathBuf>) -> Hasher {
        Hasher { memo: RefCell::new(HashMap::new()), persist_dir, immutable_roots }
    }

    pub fn ephemeral() -> Hasher {
        Hasher::new(None, Vec::new())
    }

    pub fn in_immutable_root(&self, path: &Path) -> bool {
        self.immutable_roots.iter().any(|root| path.starts_with(root))
    }

    pub fn hash(&self, path: &Path, persist: bool) -> io::Result<Hashed> {
        let current = Stamp::read(path)?;
        if let Some(known) = self.memo.borrow().get(path)
            && known.stamp == current
            && known.reusable()
        {
            return Ok(known.clone());
        }
        let persist = persist && current.size >= PERSIST_MIN_BYTES && self.persist_dir.is_some();
        if persist && let Some(hash) = self.persisted(path, &current) {
            // Recorded only after settling, so date it just past the settle margin to keep it reusable.
            let at_ns = current.mtime_ns.max(current.ctime_ns) + SETTLE_MARGIN_NS + 1;
            let hashed = Hashed { hash, stamp: current, stable: true, at_ns };
            self.memo.borrow_mut().insert(path.to_path_buf(), hashed.clone());
            return Ok(hashed);
        }
        let hashed = hash_with_stamp(path)?;
        if persist && hashed.reusable() {
            self.remember(path, &hashed);
        }
        self.memo.borrow_mut().insert(path.to_path_buf(), hashed.clone());
        Ok(hashed)
    }

    fn record_path(&self, path: &Path) -> Option<PathBuf> {
        let dir = self.persist_dir.as_ref()?;
        let key = blake3::hash(path.as_os_str().as_encoded_bytes()).to_hex();
        Some(dir.join(&key[..2]).join(&key[2..34]))
    }

    fn persisted(&self, path: &Path, stamp: &Stamp) -> Option<String> {
        let text = fs::read_to_string(self.record_path(path)?).ok()?;
        let mut lines = text.lines();
        if lines.next()? != "frust-hashmemo-v1" || lines.next()? != path.to_str()? {
            return None;
        }
        let fields: Vec<i128> = lines.next()?.split(' ').map(|f| f.parse().ok()).collect::<Option<_>>()?;
        let recorded = match fields[..] {
            [dev, ino, size, mtime_ns, ctime_ns] => {
                Stamp { dev: dev as u64, ino: ino as u64, size: size as u64, mtime_ns, ctime_ns }
            }
            _ => return None,
        };
        let hash = lines.next()?;
        (recorded == *stamp && hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| hash.to_string())
    }

    fn remember(&self, path: &Path, hashed: &Hashed) {
        let (Some(record), Some(text_path)) = (self.record_path(path), path.to_str()) else {
            return;
        };
        let s = &hashed.stamp;
        let body = format!(
            "frust-hashmemo-v1\n{text_path}\n{} {} {} {} {}\n{}\n",
            s.dev, s.ino, s.size, s.mtime_ns, s.ctime_ns, hashed.hash
        );
        let _ = fsx::write_atomic(&record, body.as_bytes());
    }

    /// Linkers read only the top level and `*.framework` bundles, so other sub-directories count by name unless `recursive`.
    pub fn hash_dir(&self, dir: &Path, recursive: bool, watch: &mut Vec<(PathBuf, Stamp)>) -> DirHash {
        let mut walk = DirWalk { hasher: self, entries: Vec::new(), files: 0, bytes: 0, seen: HashSet::new(), watch };
        match fs::metadata(dir) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return DirHash::Hash("absent".to_string());
            }
            Err(err) => return DirHash::Unverifiable(format!("{}: {err}", dir.display())),
            Ok(meta) if !meta.is_dir() => return DirHash::Hash("not-a-directory".to_string()),
            Ok(_) => {}
        }
        if let Err(reason) = walk.visit(dir, "", recursive) {
            return DirHash::Unverifiable(reason);
        }
        walk.entries.sort();
        let mut hasher = blake3::Hasher::new();
        for (name, hash) in &walk.entries {
            hasher.update(name.as_bytes());
            hasher.update(b"\0");
            hasher.update(hash.as_bytes());
            hasher.update(b"\n");
        }
        DirHash::Hash(hasher.finalize().to_hex()[..32].to_string())
    }
}

struct DirWalk<'a> {
    hasher: &'a Hasher,
    entries: Vec<(String, String)>,
    files: usize,
    bytes: u64,
    seen: HashSet<(u64, u64)>,
    watch: &'a mut Vec<(PathBuf, Stamp)>,
}

impl DirWalk<'_> {
    fn visit(&mut self, dir: &Path, prefix: &str, recursive: bool) -> Result<(), String> {
        let meta = fs::metadata(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        if !self.seen.insert((meta.dev(), meta.ino())) {
            self.entries.push((prefix.to_string(), "cycle".to_string()));
            return Ok(());
        }
        let reader = fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        for entry in reader {
            let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let rel = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
            let path = entry.path();
            let meta = match fs::metadata(&path) {
                Ok(meta) => meta,
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    self.entries.push((rel, "dangling".to_string()));
                    continue;
                }
                Err(err) => return Err(format!("{}: {err}", path.display())),
            };
            if meta.is_dir() {
                if recursive || name.ends_with(".framework") {
                    self.watch.push((path.clone(), Stamp::of(&meta)));
                    self.visit(&path, &rel, true)?;
                } else {
                    self.entries.push((rel, "dir".to_string()));
                }
            } else if meta.is_file() {
                self.files += 1;
                self.bytes += meta.len();
                if self.files > DIR_MAX_FILES || self.bytes > DIR_MAX_BYTES {
                    return Err(format!("{} is too large to hash", dir.display()));
                }
                // Recursive dirs are build script outputs every dependent links, so the persistent memo avoids rehashing per dependent.
                let hashed = self.hasher.hash(&path, recursive).map_err(|e| format!("{}: {e}", path.display()))?;
                self.watch.push((path, hashed.stamp));
                self.entries.push((rel, hashed.hash));
            } else {
                self.entries.push((rel, "special".to_string()));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("frust-hashing-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn hash_matches_plain_blake3_and_reuses_settled_files() {
        let dir = scratch("memo");
        let file = dir.join("f");
        fs::write(&file, b"hello").unwrap();
        let hasher = Hasher::ephemeral();
        let first = hasher.hash(&file, false).unwrap();
        assert_eq!(first.hash, blake3::hash(b"hello").to_hex().to_string());
        assert!(first.stable);
        fs::write(&file, b"world").unwrap();
        assert_eq!(hasher.hash(&file, false).unwrap().hash, blake3::hash(b"world").to_hex().to_string());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn persistent_memo_is_used_only_while_the_stamp_holds() {
        let dir = scratch("persist");
        let file = dir.join("big.rlib");
        let memo = dir.join("memo");
        fs::write(&file, vec![1u8; 64 * 1024]).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2100));
        let first = Hasher::new(Some(memo.clone()), Vec::new()).hash(&file, true).unwrap();
        assert!(fs::read_dir(&memo).map(|rd| rd.count() > 0).unwrap_or(false), "a settled large file is remembered");
        assert_eq!(Hasher::new(Some(memo.clone()), Vec::new()).hash(&file, true).unwrap().hash, first.hash);

        let mtime = fs::metadata(&file).unwrap().modified().unwrap();
        fs::write(&file, vec![2u8; 64 * 1024]).unwrap();
        File::options().write(true).open(&file).unwrap().set_modified(mtime).unwrap();
        let second = Hasher::new(Some(memo), Vec::new()).hash(&file, true).unwrap();
        assert_ne!(second.hash, first.hash);
        assert_eq!(second.hash, blake3::hash(&[2u8; 64 * 1024]).to_hex().to_string());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn too_new_uses_a_wider_margin_for_whole_second_stamps() {
        let fine = Stamp { dev: 0, ino: 0, size: 0, mtime_ns: 10_500_000_000, ctime_ns: 10_500_000_000 };
        assert!(fine.too_new_for(10_550_000_000));
        assert!(!fine.too_new_for(10_700_000_000));
        let coarse = Stamp { mtime_ns: 10_000_000_000, ctime_ns: 10_000_000_000, ..fine };
        assert!(coarse.too_new_for(11_500_000_000));
        assert!(!coarse.too_new_for(12_100_000_000));
    }

    #[test]
    fn directory_hash_follows_symlinks_and_sees_content() {
        let dir = scratch("dir");
        fs::write(dir.join("libreal.a"), b"one").unwrap();
        std::os::unix::fs::symlink(dir.join("libreal.a"), dir.join("liblink.a")).unwrap();
        fs::create_dir_all(dir.join("Foo.framework")).unwrap();
        fs::write(dir.join("Foo.framework").join("Foo"), b"fw").unwrap();
        let a = Hasher::ephemeral().hash_dir(&dir, false, &mut Vec::new());
        fs::write(dir.join("Foo.framework").join("Foo"), b"fw2").unwrap();
        let b = Hasher::ephemeral().hash_dir(&dir, false, &mut Vec::new());
        assert_ne!(a, b, "framework contents are part of the hash");
        fs::write(dir.join("libreal.a"), b"two").unwrap();
        let c = Hasher::ephemeral().hash_dir(&dir, false, &mut Vec::new());
        assert_ne!(b, c, "a symlinked library's target contents are part of the hash");
        assert_eq!(
            Hasher::ephemeral().hash_dir(&dir.join("missing"), false, &mut Vec::new()),
            DirHash::Hash("absent".into())
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
