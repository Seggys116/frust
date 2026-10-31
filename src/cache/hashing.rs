//! Content hashing; a memoised hash is reused only if the stamp is unchanged and the file had settled before hashing.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::fsx;
use crate::sys;

/// Covers the coarsest common timestamp granularity (2 s on FAT) and clock-tick lag.
pub const SETTLE_MARGIN_NS: i128 = 2_000_000_000;

/// Reading a memo record costs about as much as hashing a smaller file.
const PERSIST_MIN_BYTES: u64 = 32 * 1024;

/// Beyond these limits the invocation is not cached rather than keyed by an incomplete hash.
pub const DIR_MAX_FILES: usize = 20_000;
pub const DIR_MAX_BYTES: u64 = 4 * 1024 * 1024 * 1024;

const READ_BUF: usize = 256 * 1024;

#[cfg_attr(not(unix), allow(dead_code))]
const MMAP_MIN_BYTES: u64 = 1024 * 1024;

// Below this the cost of waking rayon's global pool exceeds the gain.
#[cfg_attr(not(unix), allow(dead_code))]
const RAYON_MIN_BYTES: u64 = 4 * 1024 * 1024;

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
    pub fn of(meta: &sys::Meta) -> Stamp {
        let ((mtime_s, mtime_n), (ctime_s, ctime_n)) = (meta.mtime(), meta.ctime());
        Stamp {
            dev: meta.dev(),
            ino: meta.ino(),
            size: meta.len(),
            mtime_ns: mtime_s as i128 * 1_000_000_000 + mtime_n as i128,
            ctime_ns: ctime_s as i128 * 1_000_000_000 + ctime_n as i128,
        }
    }

    pub fn read(path: &Path) -> io::Result<Stamp> {
        sys::stat(path).map(|m| Stamp::of(&m))
    }

    /// The birth time is `None` where the platform or filesystem does not report one.
    pub fn read_with_birth(path: &Path) -> io::Result<(Stamp, Option<i64>)> {
        sys::stat(path).map(|m| (Stamp::of(&m), birth_ns(&m)))
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

/// An error, a pre-epoch time or an overflow all read as absent, on the recording and the comparing side alike.
pub fn birth_ns(meta: &fs::Metadata) -> Option<i64> {
    let born = meta.created().ok()?.duration_since(UNIX_EPOCH).ok()?;
    i64::try_from(born.as_nanos()).ok()
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
    pub birth_ns: Option<i64>,
}

impl Hashed {
    fn reusable(&self) -> bool {
        self.stable && !self.stamp.changed_since(self.at_ns, SETTLE_MARGIN_NS)
    }
}

/// What a manifest remembers about an input when its hash was recorded, so a later lookup can accept an unchanged file without reading it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StampRecord {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub hashed_ns: i64,
    /// Compared only when both the record and the current file report one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub birth_ns: Option<i64>,
}

/// Local filesystems whose timestamps are faithful; a name outside this list (shared folders, network, FUSE, unrecognised magic numbers) is never trusted.
const TRUSTED_FS: &[&str] = &[
    "ext4", "ext3", "ext2", "xfs", "btrfs", "tmpfs", "overlay", "overlayfs", "fuse.overlayfs", "zfs", "f2fs", "ramfs",
    "bcachefs", "apfs", "hfs",
];

/// Whether a stamp on a filesystem of this type may be believed.
pub fn fs_name_trusted(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    // On Windows only NTFS and ReFS keep faithful change times.
    if cfg!(windows) {
        return matches!(name.as_str(), "ntfs" | "refs");
    }
    TRUSTED_FS.contains(&name.as_str())
}

/// Whether a stamp on a filesystem of this type must never be trusted.
pub fn fs_denied(name: &str) -> bool {
    !fs_name_trusted(name)
}

/// A filesystem whose type cannot be determined is not trusted.
pub fn fs_trusted(path: &Path) -> bool {
    sys::fs_type_name(path).is_some_and(|name| fs_name_trusted(&name))
}

impl StampRecord {
    /// Only a hash taken after the file had settled is recorded, which is exactly when the in-process memo would reuse it.
    pub fn of(hashed: &Hashed) -> Option<StampRecord> {
        let s = &hashed.stamp;
        // Whole-second stamps and a zero ctime suggest a filesystem that cannot prove "unchanged".
        if !hashed.reusable() || s.coarse() || s.ctime_ns <= 0 {
            return None;
        }
        Some(StampRecord {
            dev: s.dev,
            ino: s.ino,
            size: s.size,
            mtime_ns: i64::try_from(s.mtime_ns).ok()?,
            ctime_ns: i64::try_from(s.ctime_ns).ok()?,
            hashed_ns: i64::try_from(hashed.at_ns).ok()?,
            birth_ns: hashed.birth_ns,
        })
    }

    /// Any difference in identity, size, timestamps or birth time, a clock that ran backwards, or a stamp inside the settle margin means the content must be hashed.
    pub fn trusts(&self, current: &Stamp, current_birth_ns: Option<i64>, now_ns: i128) -> bool {
        let hashed = i128::from(self.hashed_ns);
        let born_alike = match (self.birth_ns, current_birth_ns) {
            (Some(recorded), Some(current)) => recorded == current,
            _ => true,
        };
        born_alike
            && (current.dev, current.ino, current.size) == (self.dev, self.ino, self.size)
            && current.mtime_ns == i128::from(self.mtime_ns)
            && current.ctime_ns == i128::from(self.ctime_ns)
            && self.ctime_ns > 0
            && !current.coarse()
            && now_ns >= hashed
            && !current.changed_since(hashed, SETTLE_MARGIN_NS)
    }
}

/// The file is always read, never mapped.
pub fn hash_with_stamp(path: &Path) -> io::Result<Hashed> {
    hash_with_stamp_in(path, false)
}

/// `immutable` says nobody edits the file in place, which is what allows it to be mapped.
fn hash_with_stamp_in(path: &Path, immutable: bool) -> io::Result<Hashed> {
    let at_ns = now_ns();
    let file = File::open(path)?;
    let meta = sys::fstat(&file)?;
    let stamp = Stamp::of(&meta);
    let mut hashed = hash_opened(path, file, stamp, at_ns, immutable)?;
    hashed.birth_ns = birth_ns(&meta);
    Ok(hashed)
}

/// `stamp` is what the handle reported before any byte was read; a later difference makes the result unstable.
fn hash_opened(path: &Path, mut file: File, stamp: Stamp, at_ns: i128, immutable: bool) -> io::Result<Hashed> {
    let hash = match hash_mapped(path, &stamp, at_ns, immutable) {
        Some(hash) => hash,
        None => hash_sized(&mut file, stamp.size)?,
    };
    let stable = Stamp::read(path).map(|after| after == stamp).unwrap_or(false);
    Ok(Hashed { hash, stamp, stable, at_ns, birth_ns: None })
}

/// Truncating a mapped file faults the reader, so only large files nobody edits in place, untouched for the settle margin, are mapped.
#[cfg_attr(not(unix), allow(dead_code))]
fn may_map(stamp: &Stamp, at_ns: i128, immutable: bool) -> bool {
    immutable && stamp.size >= MMAP_MIN_BYTES && !stamp.changed_since(at_ns, SETTLE_MARGIN_NS)
}

#[cfg(unix)]
fn hash_mapped(path: &Path, stamp: &Stamp, at_ns: i128, immutable: bool) -> Option<String> {
    if !may_map(stamp, at_ns, immutable) {
        return None;
    }
    let mut hasher = blake3::Hasher::new();
    let mapped = if stamp.size >= RAYON_MIN_BYTES {
        hasher.update_mmap_rayon(path).is_ok()
    } else {
        hasher.update_mmap(path).is_ok()
    };
    mapped.then(|| hasher.finalize().to_hex().to_string())
}

// A mapped file cannot be replaced or deleted on Windows, which cargo does to its outputs, so files there are read.
#[cfg(not(unix))]
fn hash_mapped(_path: &Path, _stamp: &Stamp, _at_ns: i128, _immutable: bool) -> Option<String> {
    None
}

/// The first read asks for one byte more than the stamp's size, so a file of the expected size needs no read that only finds EOF.
fn hash_sized(reader: &mut impl Read, size: u64) -> io::Result<String> {
    BUFFER.with(|cell| {
        let mut buf = cell.borrow_mut();
        if buf.len() < READ_BUF {
            buf.resize(READ_BUF, 0);
        }
        let mut hasher = blake3::Hasher::new();
        let mut limit = if size < READ_BUF as u64 { size as usize + 1 } else { READ_BUF };
        let mut total = 0u64;
        loop {
            let n = match reader.read(&mut buf[..limit]) {
                Ok(0) => break,
                Ok(n) => n,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(err),
            };
            hasher.update(&buf[..n]);
            total += n as u64;
            // A file that grew meanwhile fails the stamp comparison after hashing.
            if total == size && n < limit {
                break;
            }
            limit = READ_BUF;
        }
        Ok(hasher.finalize().to_hex().to_string())
    })
}

/// Says whether a stamp on `path`, which lies on volume `dev`, may be believed.
pub type StampTrust = Box<dyn Fn(u64, &Path) -> bool + Send + Sync>;

pub struct Hasher {
    memo: Mutex<HashMap<PathBuf, Hashed>>,
    persist_dir: Option<PathBuf>,
    immutable_roots: Vec<PathBuf>,
    stamp_trust: Option<StampTrust>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirHash {
    Hash(String),
    Unverifiable(String),
}

impl Hasher {
    pub fn new(persist_dir: Option<PathBuf>, immutable_roots: Vec<PathBuf>) -> Hasher {
        Hasher { memo: Mutex::new(HashMap::new()), persist_dir, immutable_roots, stamp_trust: None }
    }

    /// Where the policy answers no, or the stamp is coarse, the content is read and neither memo is consulted or written.
    pub fn with_stamp_trust(mut self, trust: StampTrust) -> Hasher {
        self.stamp_trust = Some(trust);
        self
    }

    fn believes(&self, path: &Path, stamp: &Stamp) -> bool {
        !stamp.coarse() && self.stamp_trust.as_ref().is_none_or(|trusted| trusted(stamp.dev, path))
    }

    fn memo(&self) -> MutexGuard<'_, HashMap<PathBuf, Hashed>> {
        self.memo.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn ephemeral() -> Hasher {
        Hasher::new(None, Vec::new())
    }

    // A wrong claim here costs only the SIGBUS risk of mapping a file somebody edits, so the roots are limited to sources nobody edits.
    pub fn in_immutable_root(&self, path: &Path) -> bool {
        self.immutable_roots.iter().any(|root| path.starts_with(root))
    }

    pub fn hash(&self, path: &Path, persist: bool) -> io::Result<Hashed> {
        let immutable = self.in_immutable_root(path);
        let persist_ok = persist && self.persist_dir.is_some();
        if !persist_ok && !self.memo().contains_key(path) {
            // Nothing to validate against, so the handle's own fstat is the only stat needed.
            let hashed = hash_with_stamp_in(path, immutable)?;
            self.memo().insert(path.to_path_buf(), hashed.clone());
            return Ok(hashed);
        }
        let current = Stamp::read(path)?;
        if !self.believes(path, &current) {
            return hash_with_stamp_in(path, immutable);
        }
        if let Some(known) = self.memo().get(path)
            && known.stamp == current
            && known.reusable()
        {
            return Ok(known.clone());
        }
        let persist = persist_ok && current.size >= PERSIST_MIN_BYTES;
        if persist && let Some(hash) = self.persisted(path, &current) {
            // Recorded only after settling, so date it just past the settle margin to keep it reusable.
            let at_ns = current.mtime_ns.max(current.ctime_ns) + SETTLE_MARGIN_NS + 1;
            let hashed = Hashed { hash, stamp: current, stable: true, at_ns, birth_ns: None };
            self.memo().insert(path.to_path_buf(), hashed.clone());
            return Ok(hashed);
        }
        let hashed = hash_with_stamp_in(path, immutable)?;
        if persist && hashed.reusable() {
            self.remember(path, &hashed);
        }
        self.memo().insert(path.to_path_buf(), hashed.clone());
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
        let meta = sys::stat(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
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
            let meta = match sys::stat(&path) {
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
    fn a_distrusted_stamp_reads_the_content_and_touches_neither_memo() {
        let dir = scratch("distrust");
        let file = dir.join("big.rlib");
        let memo = dir.join("memo");
        let (old, new) = (vec![1u8; 64 * 1024], vec![2u8; 64 * 1024]);
        fs::write(&file, &old).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2100));
        let trusting = || Hasher::new(Some(memo.clone()), Vec::new());
        let distrusting = || trusting().with_stamp_trust(Box::new(|_, _| false));

        assert_eq!(distrusting().hash(&file, true).unwrap().hash, plain(&old));
        assert!(!memo.exists(), "a distrusted volume gets no persistent record");

        // A record whose stamp equals the file's, holding the hash of other content.
        fs::write(&file, &new).unwrap();
        let current = Stamp::read(&file).unwrap();
        let at_ns = current.mtime_ns.max(current.ctime_ns) + SETTLE_MARGIN_NS + 1;
        let stale = Hashed { hash: plain(&old), stamp: current, stable: true, at_ns, birth_ns: None };
        trusting().remember(&file, &stale);
        assert_eq!(trusting().hash(&file, true).unwrap().hash, plain(&old), "a trusted stamp is believed");
        assert_eq!(distrusting().hash(&file, true).unwrap().hash, plain(&new), "a distrusted one is not");

        // The in-process memo is bypassed the same way.
        for (hasher, expected) in [(trusting(), plain(&old)), (distrusting(), plain(&new))] {
            hasher.memo().insert(file.clone(), stale.clone());
            assert_eq!(hasher.hash(&file, false).unwrap().hash, expected);
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn coarse_stamps_are_never_believed() {
        let hasher = Hasher::ephemeral();
        let fine = Stamp { dev: 1, ino: 2, size: 3, mtime_ns: 5_000_000_001, ctime_ns: 5_000_000_001 };
        assert!(hasher.believes(Path::new("p"), &fine));
        assert!(!hasher.believes(Path::new("p"), &Stamp { mtime_ns: 5_000_000_000, ctime_ns: 6_000_000_000, ..fine }));
        let policy = Hasher::ephemeral().with_stamp_trust(Box::new(|dev, _| dev == 1));
        assert!(policy.believes(Path::new("p"), &fine));
        assert!(!policy.believes(Path::new("p"), &Stamp { dev: 2, ..fine }));
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

    #[cfg(unix)]
    #[test]
    fn directory_hash_follows_symlinks_and_sees_content() {
        let dir = scratch("dir");
        fs::write(dir.join("libreal.a"), b"one").unwrap();
        crate::sys::link_tool(&dir.join("libreal.a"), &dir.join("liblink.a")).unwrap();
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

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i.wrapping_mul(31) % 251) as u8).collect()
    }

    fn plain(bytes: &[u8]) -> String {
        blake3::hash(bytes).to_hex().to_string()
    }

    #[cfg(unix)]
    #[test]
    fn mapped_hashing_of_large_settled_files_equals_plain_blake3() {
        let dir = scratch("mapped");
        let medium = dir.join("medium");
        let large = dir.join("large");
        let small = dir.join("small");
        let (medium_bytes, large_bytes) = (pattern(MMAP_MIN_BYTES as usize + 12_345), pattern(RAYON_MIN_BYTES as usize + 777));
        fs::write(&medium, &medium_bytes).unwrap();
        fs::write(&large, &large_bytes).unwrap();
        fs::write(&small, pattern(4096)).unwrap();
        // A hashing time well past the files' timestamps stands in for waiting out the settle margin.
        let settled = now_ns() + 10 * SETTLE_MARGIN_NS;
        let mapped = |path: &Path| hash_mapped(path, &Stamp::read(path).unwrap(), settled, true);
        assert_eq!(mapped(&medium), Some(plain(&medium_bytes)), "update_mmap path");
        assert_eq!(mapped(&large), Some(plain(&large_bytes)), "update_mmap_rayon path");
        assert_eq!(mapped(&small), None, "small files are read, not mapped");
        assert_eq!(hash_mapped(&large, &Stamp::read(&large).unwrap(), now_ns(), true), None, "a file inside the settle margin is read, not mapped");
        assert_eq!(hash_mapped(&large, &Stamp::read(&large).unwrap(), settled, false), None, "a file that may be edited in place is read, not mapped");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_settled_large_files_that_are_never_edited_are_mapped() {
        let stamp = |size: u64| Stamp { dev: 1, ino: 2, size, mtime_ns: 1_000_000_001, ctime_ns: 1_000_000_001 };
        let settled = 10 * SETTLE_MARGIN_NS;
        assert!(may_map(&stamp(MMAP_MIN_BYTES), settled, true));
        assert!(!may_map(&stamp(MMAP_MIN_BYTES), settled, false), "outside the immutable roots, however large");
        assert!(!may_map(&stamp(RAYON_MIN_BYTES * 8), settled, false));
        assert!(!may_map(&stamp(MMAP_MIN_BYTES - 1), settled, true));
        assert!(!may_map(&stamp(MMAP_MIN_BYTES), 1_000_000_001 + SETTLE_MARGIN_NS - 1, true));
    }

    #[test]
    fn files_under_an_immutable_root_hash_equal_to_plain_blake3_and_others_are_not_in_one() {
        let dir = scratch("roots");
        let root = dir.join("registry");
        fs::create_dir_all(&root).unwrap();
        let inside = root.join("big.rs");
        let outside = dir.join("big.rs");
        let bytes = pattern(MMAP_MIN_BYTES as usize + 99);
        fs::write(&inside, &bytes).unwrap();
        fs::write(&outside, &bytes).unwrap();
        let hasher = Hasher::new(None, vec![root.clone()]);
        assert!(hasher.in_immutable_root(&inside) && !hasher.in_immutable_root(&outside));
        assert_eq!(hasher.hash(&inside, false).unwrap().hash, plain(&bytes));
        assert_eq!(hasher.hash(&outside, false).unwrap().hash, plain(&bytes));
        assert_eq!(hash_with_stamp_in(&inside, true).unwrap().hash, plain(&bytes));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn hash_with_stamp_equals_plain_blake3_for_every_size_class() {
        let dir = scratch("sizes");
        for (name, bytes) in [
            ("small", pattern(4096)),
            ("medium", pattern(MMAP_MIN_BYTES as usize + 12_345)),
            ("large", pattern(RAYON_MIN_BYTES as usize + 777)),
        ] {
            let path = dir.join(name);
            fs::write(&path, &bytes).unwrap();
            assert_eq!(hash_with_stamp(&path).unwrap().hash, plain(&bytes), "{name}");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(not(unix))]
    #[test]
    fn files_are_read_rather_than_mapped_off_unix() {
        let dir = scratch("unmapped");
        let big = dir.join("big");
        fs::write(&big, pattern(RAYON_MIN_BYTES as usize + 1)).unwrap();
        let settled = now_ns() + 10 * SETTLE_MARGIN_NS;
        assert_eq!(hash_mapped(&big, &Stamp::read(&big).unwrap(), settled, true), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_mutated_during_hashing_is_reported_unstable() {
        let dir = scratch("mutated");
        let file = dir.join("f");
        fs::write(&file, pattern(10_000)).unwrap();
        let handle = File::open(&file).unwrap();
        let stamp = Stamp::of(&sys::fstat(&handle).unwrap());
        let untouched = hash_opened(&file, File::open(&file).unwrap(), stamp, now_ns(), false).unwrap();
        assert!(untouched.stable);
        fs::write(&file, pattern(10_001)).unwrap();
        let raced = hash_opened(&file, handle, stamp, now_ns(), false).unwrap();
        assert!(!raced.stable, "the stamp taken before reading no longer matches");
        assert!(!raced.reusable());
        assert!(StampRecord::of(&raced).is_none(), "an unstable hash is never recorded");

        let big = dir.join("big");
        fs::write(&big, pattern(MMAP_MIN_BYTES as usize + 1)).unwrap();
        let handle = File::open(&big).unwrap();
        let stamp = Stamp::of(&sys::fstat(&handle).unwrap());
        fs::write(&big, pattern(MMAP_MIN_BYTES as usize + 2)).unwrap();
        let settled = now_ns() + 10 * SETTLE_MARGIN_NS;
        assert!(!hash_opened(&big, handle, stamp, settled, true).unwrap().stable, "also on the mapped path");
        let _ = fs::remove_dir_all(&dir);
    }

    struct CountingReader {
        data: Vec<u8>,
        pos: usize,
        reads: usize,
    }

    impl Read for CountingReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.reads += 1;
            let n = buf.len().min(self.data.len() - self.pos);
            buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    #[test]
    fn sized_reads_need_no_read_that_only_finds_eof_and_stay_correct_when_the_size_is_wrong() {
        for len in [0usize, 1, 4096, READ_BUF - 1, READ_BUF, READ_BUF + 5, 3 * READ_BUF] {
            let data = pattern(len);
            let mut exact = CountingReader { data: data.clone(), pos: 0, reads: 0 };
            assert_eq!(hash_sized(&mut exact, len as u64).unwrap(), plain(&data), "length {len}");
            if len > 0 && len != READ_BUF && len != 3 * READ_BUF {
                assert_eq!(exact.reads, len.div_ceil(READ_BUF), "length {len}: no extra read for EOF");
            }
            for claimed in [0u64, len as u64 / 2, len as u64 + 10] {
                let mut wrong = CountingReader { data: data.clone(), pos: 0, reads: 0 };
                assert_eq!(hash_sized(&mut wrong, claimed).unwrap(), plain(&data), "length {len} claimed {claimed}");
            }
        }
    }

    fn settled(path: &Path) -> Hashed {
        let mut hashed = hash_with_stamp(path).unwrap();
        // Dated past the settle margin, as a hash taken seconds after the last write would be.
        hashed.at_ns = hashed.stamp.mtime_ns.max(hashed.stamp.ctime_ns) + SETTLE_MARGIN_NS + 1;
        hashed
    }

    #[test]
    fn stamp_records_exist_only_for_settled_files_and_trust_only_an_identical_stamp() {
        let dir = scratch("record");
        let file = dir.join("src.rs");
        fs::write(&file, b"fn main() {}").unwrap();
        let fresh = hash_with_stamp(&file).unwrap();
        assert!(StampRecord::of(&fresh).is_none(), "a file written a moment ago is not settled");

        let hashed = settled(&file);
        let record = StampRecord::of(&hashed).expect("a settled, stable file is recorded");
        let now = hashed.at_ns;
        let born = hashed.birth_ns;
        assert!(record.trusts(&hashed.stamp, born, now));
        assert!(!record.trusts(&hashed.stamp, born, now - 1), "a clock that ran backwards is not trusted");
        for changed in [
            Stamp { dev: hashed.stamp.dev + 1, ..hashed.stamp },
            Stamp { ino: hashed.stamp.ino + 1, ..hashed.stamp },
            Stamp { size: hashed.stamp.size + 1, ..hashed.stamp },
            Stamp { mtime_ns: hashed.stamp.mtime_ns + 1, ..hashed.stamp },
            Stamp { ctime_ns: hashed.stamp.ctime_ns + 1, ..hashed.stamp },
        ] {
            assert!(!record.trusts(&changed, born, now), "{changed:?}");
        }

        fs::write(&file, b"fn main() { }").unwrap();
        assert!(!record.trusts(&Stamp::read(&file).unwrap(), born, now), "an edit changes the stamp");
        let copy = dir.join("copy.rs");
        fs::copy(&file, &copy).unwrap();
        assert!(!record.trusts(&Stamp::read(&copy).unwrap(), born, now), "a copy is a different inode");

        let mut unstable = settled(&file);
        unstable.stable = false;
        assert!(StampRecord::of(&unstable).is_none());
        let mut coarse = settled(&file);
        coarse.stamp.mtime_ns = 5_000_000_000;
        coarse.stamp.ctime_ns = 5_000_000_000;
        coarse.at_ns = 8_000_000_000;
        assert!(StampRecord::of(&coarse).is_none(), "whole-second stamps are not proof of anything");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stamp_inside_the_settle_margin_of_the_recorded_time_is_not_trusted() {
        let stamp = Stamp { dev: 1, ino: 2, size: 3, mtime_ns: 10_123_456_789, ctime_ns: 10_223_456_789 };
        let record = StampRecord {
            dev: 1,
            ino: 2,
            size: 3,
            mtime_ns: 10_123_456_789,
            ctime_ns: 10_223_456_789,
            hashed_ns: 11_000_000_000,
            birth_ns: None,
        };
        assert!(!record.trusts(&stamp, None, 20_000_000_000), "the file changed less than the margin before it was hashed");
        let settled = StampRecord { hashed_ns: 12_300_000_000, ..record };
        assert!(settled.trusts(&stamp, None, 20_000_000_000));
    }

    #[test]
    fn birth_time_is_an_extra_condition_only_when_both_sides_report_one() {
        let stamp = Stamp { dev: 1, ino: 2, size: 3, mtime_ns: 10_123_456_789, ctime_ns: 10_223_456_789 };
        let record = StampRecord {
            dev: 1,
            ino: 2,
            size: 3,
            mtime_ns: 10_123_456_789,
            ctime_ns: 10_223_456_789,
            hashed_ns: 12_300_000_000,
            birth_ns: Some(9_000_000_007),
        };
        let now = 20_000_000_000;
        assert!(record.trusts(&stamp, Some(9_000_000_007), now));
        assert!(!record.trusts(&stamp, Some(9_000_000_008), now), "a different birth time is a different file");
        assert!(record.trusts(&stamp, None, now), "a side without a birth time adds no condition");
        let unrecorded = StampRecord { birth_ns: None, ..record };
        assert!(unrecorded.trusts(&stamp, Some(5), now));
    }

    #[test]
    fn birth_time_round_trips_through_a_record_and_is_left_out_when_absent() {
        let dir = scratch("birth");
        let file = dir.join("b.rs");
        fs::write(&file, b"fn b() {}").unwrap();
        let hashed = settled(&file);
        assert_eq!(hashed.birth_ns, birth_ns(&fs::metadata(&file).unwrap()));
        let record = StampRecord::of(&hashed).unwrap();
        assert_eq!(record.birth_ns, hashed.birth_ns);
        let (stamp, born) = Stamp::read_with_birth(&file).unwrap();
        assert!(record.trusts(&stamp, born, hashed.at_ns));

        let bare = StampRecord { birth_ns: None, ..record };
        let json = serde_json::to_string(&bare).unwrap();
        assert!(!json.contains("birth_ns"), "{json}");
        assert_eq!(serde_json::from_str::<StampRecord>(&json).unwrap(), bare);
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn windows_trusts_only_ntfs_and_refs() {
        for name in ["ntfs", "NTFS", "refs", "ReFS"] {
            assert!(!fs_denied(name), "{name}");
        }
        for name in ["fat", "FAT32", "exFAT", "cdfs", "udf", "magic-1234", "nfs"] {
            assert!(fs_denied(name), "{name}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn only_known_local_filesystems_are_trusted() {
        let cases = [
            ("apfs", true),
            ("hfs", true),
            ("ext4", true),
            ("ext3", true),
            ("ext2", true),
            ("xfs", true),
            ("btrfs", true),
            ("tmpfs", true),
            ("overlay", true),
            ("overlayfs", true),
            ("fuse.overlayfs", true),
            ("zfs", true),
            ("f2fs", true),
            ("ramfs", true),
            ("bcachefs", true),
            ("EXT4", true),
            ("magic-1234", false),
            ("magic-dead", false),
            ("vboxsf", false),
            ("prl_fs", false),
            ("nfs", false),
            ("nfs4", false),
            ("smbfs", false),
            ("cifs", false),
            ("9p", false),
            ("fuse", false),
            ("fuseblk", false),
            ("fuse.sshfs", false),
            ("osxfuse", false),
            ("macfuse", false),
            ("msdos", false),
            ("vfat", false),
            ("exfat", false),
            ("ntfs", false),
            ("ceph", false),
            ("", false),
        ];
        for (name, trusted) in cases {
            assert_eq!(fs_name_trusted(name), trusted, "{name}");
            assert_eq!(fs_denied(name), !trusted, "{name}");
        }
    }

    #[test]
    fn the_filesystem_holding_a_scratch_directory_is_identified() {
        let dir = scratch("fstype");
        if cfg!(any(target_os = "macos", target_os = "linux", windows)) {
            assert!(sys::fs_type_name(&dir).is_some_and(|name| !name.is_empty()));
        } else {
            assert!(!fs_trusted(&dir), "an unidentifiable filesystem is never trusted");
        }
        assert!(!fs_trusted(&dir.join("missing")), "a path that cannot be examined is not trusted");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn hasher_is_shareable_between_threads_and_memoises_per_path() {
        fn assert_sync<T: Sync + Send>() {}
        assert_sync::<Hasher>();
        let dir = scratch("threads");
        let files: Vec<PathBuf> = (0..8).map(|i| dir.join(format!("f{i}"))).collect();
        for (i, f) in files.iter().enumerate() {
            fs::write(f, pattern(100 + i)).unwrap();
        }
        let hasher = Hasher::ephemeral();
        std::thread::scope(|scope| {
            for chunk in files.chunks(2) {
                let hasher = &hasher;
                scope.spawn(move || {
                    for f in chunk {
                        assert!(hasher.hash(f, false).unwrap().stable);
                    }
                });
            }
        });
        for (i, f) in files.iter().enumerate() {
            assert_eq!(hasher.hash(f, false).unwrap().hash, plain(&pattern(100 + i)));
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
