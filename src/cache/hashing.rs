//! Content hashing; a memoised hash is reused only if the stamp is unchanged and the file had settled before hashing.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::fsx;
use crate::sys;

/// Covers the coarsest common timestamp granularity (2 s on FAT) and clock-tick lag.
pub const SETTLE_MARGIN_NS: i128 = 2_000_000_000;

/// Reading a memo record costs about as much as hashing a smaller file.
const PERSIST_MIN_BYTES: u64 = 32 * 1024;

/// Smaller files are cheaper to read than to look up, so nothing records them.
pub const ATTEST_MIN_BYTES: u64 = PERSIST_MIN_BYTES;

/// Beyond these limits the invocation is not cached rather than keyed by an incomplete hash.
pub const DIR_MAX_FILES: usize = 20_000;
pub const DIR_MAX_BYTES: u64 = 4 * 1024 * 1024 * 1024;

const READ_BUF: usize = 256 * 1024;

#[cfg_attr(not(unix), allow(dead_code))]
const MMAP_MIN_BYTES: u64 = 1024 * 1024;

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
        Stamp::of_entry(&sys::EntryStat::of(meta))
    }

    pub fn read(path: &Path) -> io::Result<Stamp> {
        sys::stat(path).map(|m| Stamp::of(&m))
    }

    pub fn of_entry(entry: &sys::EntryStat) -> Stamp {
        Stamp {
            dev: entry.dev,
            ino: entry.ino,
            size: entry.size,
            mtime_ns: entry.mtime.0 as i128 * 1_000_000_000 + entry.mtime.1 as i128,
            ctime_ns: entry.ctime.0 as i128 * 1_000_000_000 + entry.ctime.1 as i128,
        }
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

#[cfg(test)]
thread_local! {
    static HASHED_SIZES: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
}

/// Sizes of the files this thread has read whole to hash them, oldest first; clears the list.
#[cfg(test)]
pub(crate) fn take_hashed_sizes() -> Vec<u64> {
    HASHED_SIZES.with(|sizes| std::mem::take(&mut *sizes.borrow_mut()))
}

fn note_hashed(_size: u64) {
    #[cfg(test)]
    HASHED_SIZES.with(|sizes| sizes.borrow_mut().push(_size));
}

pub fn hash_reader(reader: &mut impl Read) -> io::Result<String> {
    BUFFER.with(|cell| {
        let mut buf = cell.borrow_mut();
        if buf.len() < READ_BUF {
            buf.resize(READ_BUF, 0);
        }
        let mut hasher = blake3::Hasher::new();
        let mut total = 0u64;
        loop {
            let n = match reader.read(&mut buf[..]) {
                Ok(0) => break,
                Ok(n) => n,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(err),
            };
            hasher.update(&buf[..n]);
            total += n as u64;
        }
        note_hashed(total);
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
    let hash = match archive_hash(path, &file, stamp.size)? {
        Some(hash) => hash,
        None => match hash_mapped(path, &stamp, at_ns, immutable) {
            Some(hash) => hash,
            None => hash_sized(&mut file, stamp.size)?,
        },
    };
    note_hashed(stamp.size);
    let stable = Stamp::read(path).map(|after| after == stamp).unwrap_or(false);
    Ok(Hashed { hash, stamp, stable, at_ns, birth_ns: None })
}

/// Archives larger than this are hashed as plain bytes rather than held in memory.
const ARCHIVE_MAX_BYTES: u64 = 256 * 1024 * 1024;

const AR_MAGIC: &[u8; 8] = b"!<arch>\n";
const AR_HEADER: usize = 60;
const AR_DOMAIN: &[u8] = b"frust-canonical-ar-v1\0";

/// Linkers read static libraries through their members, so only these extensions have their archive metadata normalised.
fn is_static_library(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("a") || e.eq_ignore_ascii_case("lib"))
}

/// A static library whose bytes differ from another only in member dates, owners or COFF timestamps hashes the same; `None` means hash the plain bytes.
fn archive_hash(path: &Path, file: &File, size: u64) -> io::Result<Option<String>> {
    if size < (AR_MAGIC.len() + AR_HEADER) as u64 || size > ARCHIVE_MAX_BYTES || !is_static_library(path) {
        return Ok(None);
    }
    let mut bytes = Vec::with_capacity(size as usize + 1);
    Read::take(file, size + 1).read_to_end(&mut bytes)?;
    if !bytes.starts_with(AR_MAGIC) {
        return Ok(Some(blake3::hash(&bytes).to_hex().to_string()));
    }
    Ok(Some(canonical_archive_hash(&bytes).unwrap_or_else(|| blake3::hash(&bytes).to_hex().to_string())))
}

fn ar_field(header: &[u8], range: std::ops::Range<usize>) -> Option<u64> {
    let text = std::str::from_utf8(&header[range]).ok()?.trim_end_matches(' ');
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

fn le16(data: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(data.get(at..at + 2)?.try_into().ok()?))
}

fn le32(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

/// Where the 4-byte build timestamp sits in a COFF object member, if the member looks like one.
fn coff_timestamp_offset(data: &[u8]) -> Option<usize> {
    const MACHINES: [u16; 6] = [0x8664, 0x014c, 0xaa64, 0x01c4, 0xa641, 0x0200];
    // Short import members and anonymous (bigobj) headers start with 0x0000 0xFFFF and keep the stamp at offset 8.
    if le16(data, 0)? == 0 && le16(data, 2)? == 0xFFFF {
        return (data.len() >= 12 && MACHINES.contains(&le16(data, 6)?)).then_some(8);
    }
    if !MACHINES.contains(&le16(data, 0)?) || data.len() < 20 {
        return None;
    }
    let symbols = u64::from(le32(data, 8)?);
    let count = u64::from(le32(data, 12)?);
    let table_end = if symbols == 0 { 0 } else { symbols.checked_add(count.checked_mul(18)?)? };
    (table_end <= data.len() as u64).then_some(4)
}

/// Hashes member names, order, sizes, modes and data but not date, owner or group, nor a COFF member's timestamp; `None` for a malformed archive.
fn canonical_archive_hash(bytes: &[u8]) -> Option<String> {
    if bytes.get(..AR_MAGIC.len())? != AR_MAGIC {
        return None;
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(AR_DOMAIN);
    hasher.update(AR_MAGIC);
    let mut at = AR_MAGIC.len();
    let mut members = 0usize;
    let limit = bytes.len() / AR_HEADER;
    while at < bytes.len() {
        members += 1;
        let header = bytes.get(at..at.checked_add(AR_HEADER)?)?;
        if members > limit || &header[58..60] != b"`\n" {
            return None;
        }
        let size = usize::try_from(ar_field(header, 48..58)?).ok()?;
        // Date, owner and group are the volatile fields; the rest of the header is kept.
        let mut kept = [0u8; AR_HEADER];
        kept.copy_from_slice(header);
        kept[16..40].fill(0);
        hasher.update(&kept);
        let start = at + AR_HEADER;
        let end = start.checked_add(size)?;
        let data = bytes.get(start..end)?;
        match coff_timestamp_offset(data) {
            Some(stamp) => {
                hasher.update(&data[..stamp]);
                hasher.update(&[0u8; 4]);
                hasher.update(&data[stamp + 4..]);
            }
            None => {
                hasher.update(data);
            }
        }
        at = end;
        if size % 2 == 1 && at < bytes.len() {
            hasher.update(&bytes[at..at + 1]);
            at += 1;
        }
    }
    Some(hasher.finalize().to_hex().to_string())
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
    // Parallelism belongs to the bounded outer workers, including Cargo's jobserver budget.
    let mapped = hasher.update_mmap(path).is_ok();
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

/// For a file under a build script's `build/<unit>/out`, the stamp of cargo's `output` file beside it, which cargo writes only after the script exited.
fn build_output_witness(path: &Path) -> Option<Stamp> {
    let out = path.ancestors().skip(1).find(|dir| dir.file_name().is_some_and(|name| name == "out"))?;
    let unit = out.parent()?;
    if unit.parent()?.file_name()? != "build" {
        return None;
    }
    Stamp::read(&unit.join("output")).ok()
}

/// Says whether a stamp on `path`, which lies on volume `dev`, may be believed.
pub type StampTrust = Box<dyn Fn(u64, &Path) -> bool + Send + Sync>;

/// Told of a file whose stamp matched a record while its content did not: path, device, recorded hash, fresh hash.
pub type ViolationSink = Box<dyn Fn(&Path, u64, &str, &str) + Send + Sync>;

pub struct Hasher {
    on_violation: Option<ViolationSink>,
    memo: Mutex<HashMap<PathBuf, Hashed>>,
    persist_dir: Option<PathBuf>,
    log: Mutex<memolog::View>,
    log_cap: u64,
    immutable_roots: Vec<PathBuf>,
    stamp_trust: Option<StampTrust>,
    /// Attested records are proven by content on every use instead of being believed.
    audit_attested: bool,
    read_bytes: AtomicU64,
    memo_bytes: AtomicU64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirHash {
    Hash(String),
    Unverifiable(String),
}

impl Hasher {
    pub fn new(persist_dir: Option<PathBuf>, immutable_roots: Vec<PathBuf>) -> Hasher {
        Hasher {
            on_violation: None,
            memo: Mutex::new(HashMap::new()),
            persist_dir,
            log: Mutex::new(memolog::View::default()),
            log_cap: memolog::MAX_LOG_BYTES,
            immutable_roots,
            stamp_trust: None,
            audit_attested: false,
            read_bytes: AtomicU64::new(0),
            memo_bytes: AtomicU64::new(0),
        }
    }

    pub fn audit_attested(mut self, audit: bool) -> Hasher {
        self.audit_attested = audit;
        self
    }

    #[cfg(test)]
    fn with_log_cap(mut self, cap: u64) -> Hasher {
        self.log_cap = cap;
        self
    }

    pub fn with_violation_sink(mut self, sink: ViolationSink) -> Hasher {
        self.on_violation = Some(sink);
        self
    }

    /// Bytes read and hashed so far, and bytes served from a memo instead.
    pub fn tally(&self) -> (u64, u64) {
        (self.read_bytes.load(Ordering::Relaxed), self.memo_bytes.load(Ordering::Relaxed))
    }

    fn read(&self, path: &Path, immutable: bool) -> io::Result<Hashed> {
        let hashed = hash_with_stamp_in(path, immutable)?;
        self.read_bytes.fetch_add(hashed.stamp.size, Ordering::Relaxed);
        Ok(hashed)
    }

    fn served(&self, size: u64) {
        self.memo_bytes.fetch_add(size, Ordering::Relaxed);
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
            let hashed = self.read(path, immutable)?;
            self.memo().insert(path.to_path_buf(), hashed.clone());
            return Ok(hashed);
        }
        self.hash_observed(path, persist, Stamp::read(path)?, PERSIST_MIN_BYTES)
    }

    /// Files below `persist_min` are never recorded; a caller that already stat'd the file may record small ones too.
    fn hash_observed(&self, path: &Path, persist: bool, current: Stamp, persist_min: u64) -> io::Result<Hashed> {
        let immutable = self.in_immutable_root(path);
        if !self.believes(path, &current) {
            return self.read(path, immutable);
        }
        if let Some(known) = self.memo().get(path)
            && known.stamp == current
            && known.reusable()
        {
            self.served(current.size);
            return Ok(known.clone());
        }
        let persist = persist && self.persist_dir.is_some() && current.size >= persist_min;
        if persist && let Some((hash, attested)) = self.persisted(path, &current) {
            if attested && self.audit_attested {
                let fresh = self.read(path, immutable)?;
                if fresh.hash != hash || !fresh.stable || fresh.stamp != current {
                    self.forget(path);
                    // Only a file that still carries the recorded stamp proves the stamp lied; a changed stamp is an ordinary edit.
                    if fresh.hash != hash
                        && fresh.stable
                        && fresh.stamp == current
                        && let Some(report) = &self.on_violation
                    {
                        report(path, current.dev, &hash, &fresh.hash);
                    }
                    self.memo().insert(path.to_path_buf(), fresh.clone());
                    return Ok(fresh);
                }
            }
            self.served(current.size);
            // Recorded only after settling, so date it just past the settle margin to keep it reusable.
            let at_ns = current.mtime_ns.max(current.ctime_ns) + SETTLE_MARGIN_NS + 1;
            let hashed = Hashed { hash, stamp: current, stable: true, at_ns, birth_ns: None };
            self.memo().insert(path.to_path_buf(), hashed.clone());
            return Ok(hashed);
        }
        // Stat'd before the read: the proof needs the witness's change time set before any byte is read.
        let young = persist && current.changed_since(now_ns(), SETTLE_MARGIN_NS);
        let witness = young.then(|| build_output_witness(path)).flatten();
        let hashed = self.read(path, immutable)?;
        if persist && hashed.reusable() {
            self.remember(path, &hashed);
        } else if persist
            && hashed.stable
            && hashed.stamp.ctime_ns > 0
            && witness.is_some_and(|w| w.dev == hashed.stamp.dev && !w.coarse() && w.ctime_ns > hashed.stamp.ctime_ns)
        {
            // The timestamp clock had passed the file's change time before the read, so any later write moves the stamp; audit mode proves it as attested.
            self.write_record(path, &hashed.stamp, &hashed.hash, true);
        }
        self.memo().insert(path.to_path_buf(), hashed.clone());
        Ok(hashed)
    }

    fn log_path(&self) -> Option<PathBuf> {
        Some(self.persist_dir.as_ref()?.join(memolog::LOG_NAME))
    }

    fn view(&self) -> MutexGuard<'_, memolog::View> {
        self.log.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The hash and whether the record was attested by the process that produced the file.
    fn persisted(&self, path: &Path, stamp: &Stamp) -> Option<(String, bool)> {
        let log = self.log_path()?;
        let key = path.to_str()?;
        let mut view = self.view();
        view.refresh(&log);
        let live = view.get(key)?;
        (live.stamp == *stamp).then(|| (blake3::Hash::from_bytes(live.hash).to_hex().to_string(), live.attested))
    }

    fn remember(&self, path: &Path, hashed: &Hashed) {
        self.write_record(path, &hashed.stamp, &hashed.hash, false);
    }

    fn write_record(&self, path: &Path, s: &Stamp, hash: &str, attested: bool) -> bool {
        let (Some(log), Some(text_path)) = (self.log_path(), path.to_str()) else {
            return false;
        };
        let Ok(hash) = blake3::Hash::from_hex(hash) else {
            return false;
        };
        let live = memolog::Live { stamp: *s, hash: *hash.as_bytes(), attested, written_s: fsx::unix_now() };
        match memolog::encode(text_path, Some(&live)) {
            Some(frame) => memolog::append(&log, &frame, self.log_cap),
            None => false,
        }
    }

    /// A tombstone voids the key for every reader of the log, this process included.
    fn forget(&self, path: &Path) {
        let (Some(log), Some(text_path)) = (self.log_path(), path.to_str()) else {
            return;
        };
        self.view().remove(text_path);
        if let Some(frame) = memolog::encode(text_path, None) {
            memolog::append(&log, &frame, self.log_cap);
        }
    }

    /// Records the hash of a file this process just produced or restored, skipping the settle rule (see "Stamp trust and audits" in the docs).
    pub fn attest(&self, path: &Path, hash: &str, stamp: &Stamp) -> bool {
        // The file must still carry the stamp it had while hashed, on a believed volume with fine timestamps.
        let sound = hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit());
        // A static library's canonical hash is not the plain hash the producer computed.
        if !sound || is_static_library(path) || self.persist_dir.is_none() || stamp.size < PERSIST_MIN_BYTES || stamp.ctime_ns <= 0 || !self.believes(path, stamp) {
            return false;
        }
        Stamp::read(path).is_ok_and(|now| now == *stamp) && self.write_record(path, stamp, hash, true)
    }

    /// Linkers read only the top level and `*.framework` bundles, so other sub-directories count by name unless `recursive`.
    /// With `only`, just those top-level file names are read (a compile that never runs the linker opens nothing else); absent ones count by omission.
    pub fn hash_dir(&self, dir: &Path, recursive: bool, only: Option<&[String]>, watch: &mut Vec<(PathBuf, Stamp)>) -> DirHash {
        let mut walk = DirWalk { hasher: self, entries: Vec::new(), files: 0, bytes: 0, seen: HashSet::new(), watch };
        match fs::metadata(dir) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return DirHash::Hash("absent".to_string());
            }
            Err(err) => return DirHash::Unverifiable(format!("{}: {err}", dir.display())),
            Ok(meta) if !meta.is_dir() => return DirHash::Hash("not-a-directory".to_string()),
            Ok(_) => {}
        }
        let visited = match only {
            Some(names) => walk.visit_named(dir, names),
            None => walk.visit(dir, "", recursive),
        };
        if let Err(reason) = visited {
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
    fn visit_named(&mut self, dir: &Path, names: &[String]) -> Result<(), String> {
        for name in names {
            let path = dir.join(name);
            let meta = match sys::stat(&path) {
                Ok(meta) => meta,
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => return Err(format!("{}: {err}", path.display())),
            };
            if !meta.is_file() {
                self.entries.push((name.clone(), "not-a-file".to_string()));
                continue;
            }
            self.read_file(path, name.clone(), &meta)?;
        }
        Ok(())
    }

    fn read_file(&mut self, path: PathBuf, rel: String, meta: &sys::Meta) -> Result<(), String> {
        self.files += 1;
        self.bytes += meta.len();
        if self.files > DIR_MAX_FILES || self.bytes > DIR_MAX_BYTES {
            return Err(format!("{} is too large to hash", path.display()));
        }
        // Shared by every dependent and already stat'd, so even a small file is cheaper to look up than to read.
        let hashed = self.hasher.hash_observed(&path, true, Stamp::of(meta), 0).map_err(|e| format!("{}: {e}", path.display()))?;
        self.watch.push((path, hashed.stamp));
        self.entries.push((rel, hashed.hash));
        Ok(())
    }

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
                self.read_file(path, rel, &meta)?;
            } else {
                self.entries.push((rel, "special".to_string()));
            }
        }
        Ok(())
    }
}

/// The persistent memo: one append-only log of checksummed frames per memo directory, last valid frame per key wins.
mod memolog {
    use std::collections::HashMap;
    use std::fs::{self, OpenOptions};
    use std::io::{self, Read, Seek, SeekFrom, Write};
    use std::path::Path;

    use super::Stamp;
    use crate::fsx;
    use crate::sys;

    /// The version lives in the name, so per-file `frust-hashmemo-v2` records are never read.
    pub const LOG_NAME: &str = "memo-v3.log";
    pub const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;
    pub const MAX_AGE_S: u64 = 30 * 86_400;

    const MAGIC: [u8; 4] = *b"fh3\n";
    const HEADER: usize = 8;
    const SUM: usize = 8;
    const MAX_PATH_BYTES: usize = 4096;
    const MIN_PAYLOAD: usize = 1 + 8 + 4;
    const MAX_PAYLOAD: usize = MAX_PATH_BYTES + 1 + 8 + 4 + 8 * 3 + 16 * 2 + 32;
    const KIND_PLAIN: u8 = 1;
    const KIND_ATTESTED: u8 = 2;
    const KIND_TOMBSTONE: u8 = 3;

    #[derive(Clone, Copy)]
    pub struct Live {
        pub stamp: Stamp,
        pub hash: [u8; 32],
        pub attested: bool,
        pub written_s: u64,
    }

    /// What this process has read of the log so far.
    #[derive(Default)]
    pub struct View {
        map: HashMap<String, Live>,
        ino: u64,
        parsed_to: u64,
        seen: u64,
        loaded: bool,
    }

    impl View {
        pub fn get(&self, key: &str) -> Option<&Live> {
            self.map.get(key)
        }

        pub fn remove(&mut self, key: &str) {
            self.map.remove(key);
        }

        /// A stat decides whether anything is new; only the unread tail is parsed unless the file was replaced or shrank.
        pub fn refresh(&mut self, log: &Path) {
            let Ok(meta) = sys::lstat(log) else {
                *self = View::default();
                return;
            };
            if self.loaded && meta.ino() == self.ino && meta.len() == self.seen {
                return;
            }
            if !self.loaded || meta.ino() != self.ino || meta.len() < self.parsed_to {
                *self = View::default();
            }
            let start = self.parsed_to;
            let mut buf = Vec::new();
            let read = fs::File::open(log).and_then(|mut file| {
                file.seek(SeekFrom::Start(start))?;
                file.read_to_end(&mut buf)
            });
            if read.is_err() {
                return;
            }
            let consumed = scan(&buf, |_, _, frame| match frame {
                Frame::Record(key, live) => {
                    self.map.insert(key, live);
                }
                Frame::Tombstone(key) => {
                    self.map.remove(&key);
                }
            });
            self.parsed_to = start + consumed as u64;
            self.seen = start + buf.len() as u64;
            self.ino = meta.ino();
            self.loaded = true;
        }
    }

    enum Frame {
        Record(String, Live),
        Tombstone(String),
    }

    /// `None` for a record means a tombstone; a path too long to be bounded is not framed.
    pub fn encode(path: &str, live: Option<&Live>) -> Option<Vec<u8>> {
        if path.len() > MAX_PATH_BYTES {
            return None;
        }
        let kind = match live {
            None => KIND_TOMBSTONE,
            Some(l) if l.attested => KIND_ATTESTED,
            Some(_) => KIND_PLAIN,
        };
        let mut payload = Vec::with_capacity(MAX_PAYLOAD.min(path.len() + 120));
        payload.push(kind);
        payload.extend_from_slice(&live.map_or_else(fsx::unix_now, |l| l.written_s).to_le_bytes());
        payload.extend_from_slice(&(path.len() as u32).to_le_bytes());
        payload.extend_from_slice(path.as_bytes());
        if let Some(l) = live {
            for field in [l.stamp.dev, l.stamp.ino, l.stamp.size] {
                payload.extend_from_slice(&field.to_le_bytes());
            }
            payload.extend_from_slice(&l.stamp.mtime_ns.to_le_bytes());
            payload.extend_from_slice(&l.stamp.ctime_ns.to_le_bytes());
            payload.extend_from_slice(&l.hash);
        }
        let mut frame = Vec::with_capacity(HEADER + payload.len() + SUM);
        frame.extend_from_slice(&MAGIC);
        frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        frame.extend_from_slice(&payload);
        let sum = blake3::hash(&frame[4..]);
        frame.extend_from_slice(&sum.as_bytes()[..SUM]);
        Some(frame)
    }

    fn decode(payload: &[u8]) -> Option<Frame> {
        let kind = *payload.first()?;
        let written_s = u64::from_le_bytes(payload.get(1..9)?.try_into().ok()?);
        let path_len = u32::from_le_bytes(payload.get(9..13)?.try_into().ok()?) as usize;
        let key = std::str::from_utf8(payload.get(13..13usize.checked_add(path_len)?)?).ok()?.to_string();
        let rest = &payload[13 + path_len..];
        if kind == KIND_TOMBSTONE {
            return rest.is_empty().then_some(Frame::Tombstone(key));
        }
        if !matches!(kind, KIND_PLAIN | KIND_ATTESTED) || rest.len() != 24 + 32 + 32 {
            return None;
        }
        let word = |at: usize| u64::from_le_bytes(rest[at..at + 8].try_into().unwrap());
        let wide = |at: usize| i128::from_le_bytes(rest[at..at + 16].try_into().unwrap());
        let stamp = Stamp { dev: word(0), ino: word(8), size: word(16), mtime_ns: wide(24), ctime_ns: wide(40) };
        let hash: [u8; 32] = rest[56..88].try_into().unwrap();
        Some(Frame::Record(key, Live { stamp, hash, attested: kind == KIND_ATTESTED, written_s }))
    }

    /// Visits each valid frame and returns the bytes dealt with; a frame running past the end may still be arriving, so it is left.
    fn scan(buf: &[u8], mut visit: impl FnMut(usize, usize, Frame)) -> usize {
        let mut at = 0;
        while at < buf.len() {
            let Some(rel) = buf[at..].windows(MAGIC.len()).position(|w| w == MAGIC) else {
                return buf.len().saturating_sub(MAGIC.len() - 1).max(at);
            };
            at += rel;
            let Some(len_bytes) = buf.get(at + 4..at + HEADER) else {
                return at;
            };
            let len = u32::from_le_bytes(len_bytes.try_into().unwrap()) as usize;
            if !(MIN_PAYLOAD..=MAX_PAYLOAD).contains(&len) {
                at += 1;
                continue;
            }
            let end = at + HEADER + len + SUM;
            if end > buf.len() {
                return at;
            }
            let sum = blake3::hash(&buf[at + 4..end - SUM]);
            match decode(&buf[at + HEADER..end - SUM]) {
                Some(frame) if sum.as_bytes()[..SUM] == buf[end - SUM..end] => {
                    visit(at, end, frame);
                    at = end;
                }
                _ => at += 1,
            }
        }
        at
    }

    /// One write call on an append handle keeps frames whole across processes; a log past `cap` is compacted afterwards.
    pub fn append(log: &Path, frame: &[u8], cap: u64) -> bool {
        let open = || OpenOptions::new().create(true).append(true).open(log);
        let file = match open() {
            Err(err) if err.kind() == io::ErrorKind::NotFound => match log.parent() {
                Some(parent) => fs::create_dir_all(parent).and_then(|()| open()),
                None => Err(err),
            },
            other => other,
        };
        let Ok(mut file) = file else { return false };
        if file.write_all(frame).is_err() {
            return false;
        }
        if file.metadata().is_ok_and(|m| m.len() > cap) {
            let _ = compact(log, MAX_AGE_S, cap);
        }
        true
    }

    /// Keeps live, unexpired frames (oldest dropped first past `cap`) and returns the sizes before and after; skipped if the log grew meanwhile.
    pub fn compact(log: &Path, max_age_s: u64, cap: u64) -> io::Result<(u64, u64)> {
        let bytes = match fs::read(log) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok((0, 0)),
            Err(err) => return Err(err),
        };
        let now = fsx::unix_now();
        let mut frames: Vec<(usize, usize, String, Option<Live>)> = Vec::new();
        scan(&bytes, |start, end, frame| match frame {
            Frame::Record(key, live) => frames.push((start, end, key, Some(live))),
            Frame::Tombstone(key) => frames.push((start, end, key, None)),
        });
        let mut last: HashMap<&str, usize> = HashMap::new();
        for (index, (_, _, key, _)) in frames.iter().enumerate() {
            last.insert(key, index);
        }
        let mut kept: Vec<(usize, usize)> = frames
            .iter()
            .enumerate()
            .filter(|(index, (_, _, key, live))| {
                last[key.as_str()] == *index && live.is_some_and(|l| l.written_s > now || now - l.written_s <= max_age_s)
            })
            .map(|(_, (start, end, _, _))| (*start, *end))
            .collect();
        let mut total: u64 = kept.iter().map(|(s, e)| (e - s) as u64).sum();
        if total > cap {
            let target = cap / 4 * 3;
            let mut drop = 0;
            while total > target && drop < kept.len() {
                total -= (kept[drop].1 - kept[drop].0) as u64;
                drop += 1;
            }
            kept.drain(..drop);
        }
        let before = bytes.len() as u64;
        if total >= before || fs::metadata(log).map(|m| m.len())? != before {
            return Ok((before, before));
        }
        if kept.is_empty() {
            fs::remove_file(log)?;
            return Ok((before, 0));
        }
        let mut out = Vec::with_capacity(total as usize);
        for (start, end) in kept {
            out.extend_from_slice(&bytes[start..end]);
        }
        fsx::write_atomic(log, &out)?;
        Ok((before, total))
    }
}

pub use memolog::LOG_NAME;

/// Compacts the memo log in `dir`; returns the bytes it no longer occupies.
pub fn compact_memo_log(dir: &Path, max_age_s: u64) -> u64 {
    memolog::compact(&dir.join(memolog::LOG_NAME), max_age_s, memolog::MAX_LOG_BYTES).map_or(0, |(before, after)| before - after)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

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
        let a = Hasher::ephemeral().hash_dir(&dir, false, None, &mut Vec::new());
        fs::write(dir.join("Foo.framework").join("Foo"), b"fw2").unwrap();
        let b = Hasher::ephemeral().hash_dir(&dir, false, None, &mut Vec::new());
        assert_ne!(a, b, "framework contents are part of the hash");
        fs::write(dir.join("libreal.a"), b"two").unwrap();
        let c = Hasher::ephemeral().hash_dir(&dir, false, None, &mut Vec::new());
        assert_ne!(b, c, "a symlinked library's target contents are part of the hash");
        assert_eq!(
            Hasher::ephemeral().hash_dir(&dir.join("missing"), false, None, &mut Vec::new()),
            DirHash::Hash("absent".into())
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_named_directory_hash_reads_only_the_named_libraries() {
        let dir = scratch("named");
        fs::write(dir.join("libwanted.a"), vec![1u8; 4096]).unwrap();
        fs::write(dir.join("libbig.so"), vec![9u8; 1 << 20]).unwrap();
        let only = vec!["libwanted.a".to_string(), "wanted.lib".to_string()];
        let hasher = Hasher::ephemeral();
        let first = hasher.hash_dir(&dir, false, Some(&only), &mut Vec::new());
        assert_eq!(hasher.tally().0, 4096, "an unnamed library is never read");
        fs::write(dir.join("libbig.so"), vec![8u8; 1 << 20]).unwrap();
        assert_eq!(Hasher::ephemeral().hash_dir(&dir, false, Some(&only), &mut Vec::new()), first);
        fs::write(dir.join("libwanted.a"), vec![2u8; 4096]).unwrap();
        let changed = Hasher::ephemeral().hash_dir(&dir, false, Some(&only), &mut Vec::new());
        assert_ne!(changed, first, "a named library's contents are part of the hash");
        fs::write(dir.join("wanted.lib"), b"new").unwrap();
        assert_ne!(Hasher::ephemeral().hash_dir(&dir, false, Some(&only), &mut Vec::new()), changed, "a named library appearing changes the hash");
        assert_ne!(Hasher::ephemeral().hash_dir(&dir, false, None, &mut Vec::new()), changed);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_directory_observation_never_replaces_the_opened_files_stamp_on_a_miss() {
        let dir = scratch("observed-stamp");
        let file = dir.join("input");
        fs::write(&file, b"before").unwrap();
        let observed = Stamp::read(&file).unwrap();
        fs::write(&file, b"after, with different contents").unwrap();
        let hashed = Hasher::ephemeral().hash_observed(&file, false, observed, PERSIST_MIN_BYTES).unwrap();
        assert!(hashed.stable);
        assert_ne!(hashed.stamp, observed);
        assert_eq!(hashed.stamp, Stamp::read(&file).unwrap());
        assert_eq!(hashed.hash, plain(b"after, with different contents"));
        fs::remove_dir_all(dir).unwrap();
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i.wrapping_mul(31) % 251) as u8).collect()
    }

    fn plain(bytes: &[u8]) -> String {
        blake3::hash(bytes).to_hex().to_string()
    }

    fn member(name: &str, date: u64, uid: u64, gid: u64, data: &[u8]) -> Vec<u8> {
        let mut out = format!("{name:<16}{date:<12}{uid:<6}{gid:<6}{:<8}{:<10}`\n", "100644", data.len()).into_bytes();
        out.extend_from_slice(data);
        if data.len() % 2 == 1 {
            out.push(b'\n');
        }
        out
    }

    fn archive(members: &[Vec<u8>]) -> Vec<u8> {
        let mut out = AR_MAGIC.to_vec();
        for m in members {
            out.extend_from_slice(m);
        }
        out
    }

    fn coff(stamp: u32, body: &[u8]) -> Vec<u8> {
        let mut out = vec![0x64, 0x86, 1, 0];
        out.extend_from_slice(&stamp.to_le_bytes());
        out.extend_from_slice(&[0u8; 12]);
        out.extend_from_slice(body);
        out
    }

    fn canonical(bytes: &[u8]) -> String {
        canonical_archive_hash(bytes).expect("well-formed archive")
    }

    #[test]
    fn archives_differing_only_in_dates_owners_and_groups_hash_equal() {
        let one = archive(&[member("__.SYMDEF SORTED", 1700000000, 501, 20, b"symbols"), member("x.o", 1700000001, 501, 20, b"object data")]);
        let two = archive(&[member("__.SYMDEF SORTED", 1800000000, 0, 0, b"symbols"), member("x.o", 5, 1000, 1000, b"object data")]);
        assert_ne!(one, two);
        assert_eq!(canonical(&one), canonical(&two));
        assert_ne!(canonical(&one), plain(&one), "the canonical hash is domain separated from the plain hash");
    }

    #[test]
    fn any_link_relevant_archive_change_changes_the_hash() {
        let base = archive(&[member("a.o", 0, 0, 0, b"alpha"), member("b.o", 0, 0, 0, b"beta!")]);
        let data = archive(&[member("a.o", 0, 0, 0, b"alphx"), member("b.o", 0, 0, 0, b"beta!")]);
        let name = archive(&[member("c.o", 0, 0, 0, b"alpha"), member("b.o", 0, 0, 0, b"beta!")]);
        let order = archive(&[member("b.o", 0, 0, 0, b"beta!"), member("a.o", 0, 0, 0, b"alpha")]);
        let extra = archive(&[member("a.o", 0, 0, 0, b"alpha"), member("b.o", 0, 0, 0, b"beta!"), member("c.o", 0, 0, 0, b"")]);
        let longer = archive(&[member("a.o", 0, 0, 0, b"alpha!"), member("b.o", 0, 0, 0, b"beta!")]);
        let reference = canonical(&base);
        for changed in [data, name, order, extra, longer] {
            assert_ne!(canonical(&changed), reference);
        }
    }

    #[test]
    fn a_coff_member_timestamp_is_ignored_but_its_content_is_not() {
        let one = archive(&[member("a.obj", 0, 0, 0, &coff(1, b"code"))]);
        let two = archive(&[member("a.obj", 0, 0, 0, &coff(0xdead_beef, b"code"))]);
        let other = archive(&[member("a.obj", 0, 0, 0, &coff(1, b"cod3"))]);
        assert_eq!(canonical(&one), canonical(&two));
        assert_ne!(canonical(&one), canonical(&other));
        // Bytes that only resemble a machine type but fail the symbol table bounds are hashed as they are.
        let mut odd = coff(1, b"code");
        odd[8..12].copy_from_slice(&0xffff_0000u32.to_le_bytes());
        let (a, b) = (archive(&[member("a.obj", 0, 0, 0, &odd)]), {
            let mut changed = odd.clone();
            changed[4] = 9;
            archive(&[member("a.obj", 0, 0, 0, &changed)])
        });
        assert_ne!(canonical(&a), canonical(&b));
    }

    #[test]
    fn short_import_member_timestamps_are_ignored() {
        let import = |stamp: u32| {
            let mut data = vec![0, 0, 0xff, 0xff, 0, 0, 0x64, 0x86];
            data.extend_from_slice(&stamp.to_le_bytes());
            data.extend_from_slice(b"rest of header and names");
            archive(&[member("kernel32.dll/", 0, 0, 0, &data)])
        };
        assert_eq!(canonical(&import(1)), canonical(&import(2)));
    }

    #[test]
    fn malformed_archives_fall_back_to_plain_hashing_without_panicking() {
        let good = archive(&[member("a.o", 0, 0, 0, b"alpha"), member("b.o", 0, 0, 0, b"beta!")]);
        assert!(canonical_archive_hash(&good).is_some());
        for len in 0..good.len() {
            let cut = &good[..len];
            let _ = canonical_archive_hash(cut);
        }
        assert!(canonical_archive_hash(&good[..good.len() - 30]).is_none(), "a truncated member");
        let mut bad_end = good.clone();
        bad_end[8 + 58] = b'x';
        assert!(canonical_archive_hash(&bad_end).is_none());
        let mut bad_size = good.clone();
        bad_size[8 + 48..8 + 58].copy_from_slice(b"not-digits");
        assert!(canonical_archive_hash(&bad_size).is_none());
        let mut huge = good.clone();
        huge[8 + 48..8 + 58].copy_from_slice(b"9999999999");
        assert!(canonical_archive_hash(&huge).is_none());
        assert!(canonical_archive_hash(b"!<thin>\n").is_none());
        let mut trailing = good.clone();
        trailing.extend_from_slice(b"junk");
        assert!(canonical_archive_hash(&trailing).is_none());

        let dir = scratch("ar-corrupt");
        let file = dir.join("libbad.a");
        fs::write(&file, &good[..good.len() - 30]).unwrap();
        assert_eq!(hash_with_stamp(&file).unwrap().hash, plain(&good[..good.len() - 30]));
        fs::write(&file, vec![b'z'; 4096]).unwrap();
        assert_eq!(hash_with_stamp(&file).unwrap().hash, plain(&vec![b'z'; 4096]));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_static_libraries_are_canonicalised_and_memoised_hashing_agrees() {
        let dir = scratch("ar-memo");
        let one = archive(&[member("a.o", 11, 501, 20, &pattern(40_000)), member("b.o", 12, 501, 20, b"beta!")]);
        let two = archive(&[member("a.o", 99, 0, 0, &pattern(40_000)), member("b.o", 98, 0, 0, b"beta!")]);
        let (lib_one, lib_two, rlib) = (dir.join("libone.a"), dir.join("two.lib"), dir.join("libone.rlib"));
        fs::write(&lib_one, &one).unwrap();
        fs::write(&lib_two, &two).unwrap();
        fs::write(&rlib, &one).unwrap();
        let direct = canonical(&one);
        assert_eq!(hash_with_stamp(&lib_one).unwrap().hash, direct);
        assert_eq!(hash_with_stamp(&lib_two).unwrap().hash, direct);
        assert_eq!(hash_with_stamp(&rlib).unwrap().hash, plain(&one), "rlibs keep the plain hash");

        std::thread::sleep(std::time::Duration::from_millis(2100));
        let memo = dir.join("memo");
        let hasher = Hasher::new(Some(memo.clone()), vec![dir.clone()]);
        assert_eq!(hasher.hash(&lib_one, true).unwrap().hash, direct);
        assert_eq!(hasher.hash(&lib_one, true).unwrap().hash, direct, "in-process memo");
        assert!(fs::read_dir(&memo).map(|rd| rd.count() > 0).unwrap_or(false));
        assert_eq!(Hasher::new(Some(memo), Vec::new()).hash(&lib_one, true).unwrap().hash, direct, "persistent memo");
        let mut watch = Vec::new();
        let one_dir = Hasher::ephemeral().hash_dir(&dir, false, Some(&["libone.a".to_string()]), &mut watch);
        let two_dir = Hasher::ephemeral().hash_dir(&dir, false, Some(&["libone.a".to_string()]), &mut watch);
        assert_eq!(one_dir, two_dir);
        assert!(!Hasher::new(Some(dir.join("m2")), Vec::new()).attest(&lib_one, &plain(&one), &Stamp::read(&lib_one).unwrap()), "static libraries are never attested by plain hash");
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn mapped_hashing_of_large_settled_files_equals_plain_blake3() {
        let dir = scratch("mapped");
        let medium = dir.join("medium");
        let large = dir.join("large");
        let small = dir.join("small");
        let (medium_bytes, large_bytes) = (pattern(MMAP_MIN_BYTES as usize + 12_345), pattern(8 * MMAP_MIN_BYTES as usize + 777));
        fs::write(&medium, &medium_bytes).unwrap();
        fs::write(&large, &large_bytes).unwrap();
        fs::write(&small, pattern(4096)).unwrap();
        // A hashing time well past the files' timestamps stands in for waiting out the settle margin.
        let settled = now_ns() + 10 * SETTLE_MARGIN_NS;
        let mapped = |path: &Path| hash_mapped(path, &Stamp::read(path).unwrap(), settled, true);
        assert_eq!(mapped(&medium), Some(plain(&medium_bytes)), "update_mmap path");
        assert_eq!(mapped(&large), Some(plain(&large_bytes)), "large mappings keep the same hash");
        assert_eq!(mapped(&small), None, "small files are read, not mapped");
        // Touching the file now keeps it inside the settle margin however long the writes above took under load.
        File::options().write(true).open(&large).unwrap().set_modified(SystemTime::now()).unwrap();
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
        assert!(!may_map(&stamp(MMAP_MIN_BYTES * 32), settled, false));
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
            ("large", pattern(8 * MMAP_MIN_BYTES as usize + 777)),
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
        fs::write(&big, pattern(8 * MMAP_MIN_BYTES as usize + 1)).unwrap();
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

    const ATTESTED_LEN: usize = 64 * 1024;

    // A file written just now, so it is inside the settle margin; `None` where stamps are whole seconds.
    fn fresh_output(dir: &Path, name: &str) -> Option<(PathBuf, Vec<u8>, Stamp)> {
        let file = dir.join(name);
        let bytes = pattern(ATTESTED_LEN);
        fs::write(&file, &bytes).unwrap();
        let stamp = Stamp::read(&file).unwrap();
        (!stamp.coarse()).then_some((file, bytes, stamp))
    }

    fn dependent(memo: &Path) -> Hasher {
        Hasher::new(Some(memo.to_path_buf()), Vec::new())
    }

    #[test]
    fn a_producer_attested_file_is_not_hashed_again_by_a_dependent() {
        let dir = scratch("attest-use");
        let Some((file, bytes, stamp)) = fresh_output(&dir, "libk.rlib") else { return };
        let memo = dir.join("memo");
        assert!(Hasher::new(Some(memo.clone()), Vec::new()).attest(&file, &plain(&bytes), &stamp));
        let user = dependent(&memo);
        assert_eq!(user.hash(&file, true).unwrap().hash, plain(&bytes));
        assert_eq!(user.tally(), (0, ATTESTED_LEN as u64));

        let (other, other_bytes, _) = fresh_output(&dir, "libother.rlib").unwrap();
        let unattested = dependent(&memo);
        assert_eq!(unattested.hash(&other, true).unwrap().hash, plain(&other_bytes));
        assert_eq!(unattested.tally(), (ATTESTED_LEN as u64, 0), "an unsettled, unattested file is read");
        let _ = fs::remove_dir_all(&dir);
    }

    // A build script's output directory, its archive written now and, unless `output_first`, cargo's `output` file just after.
    fn build_script_archive(dir: &Path, output_first: bool) -> Option<(PathBuf, Vec<u8>)> {
        let script = dir.join("build").join("foo-0123abcd");
        fs::create_dir_all(script.join("out")).unwrap();
        let output = || fs::write(script.join("output"), "cargo:rustc-link-lib=static=foo\n").unwrap();
        if output_first {
            output();
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        let (archive, bytes, stamp) = fresh_output(&script.join("out"), "libfoo.a")?;
        if !output_first {
            std::thread::sleep(std::time::Duration::from_millis(30));
            output();
        }
        let witness = Stamp::read(&script.join("output")).unwrap();
        (witness.ctime_ns != stamp.ctime_ns).then_some((archive, bytes))
    }

    #[test]
    fn a_build_script_archive_older_than_its_output_file_is_remembered_while_still_young() {
        let dir = scratch("witness-settled");
        let Some((archive, bytes)) = build_script_archive(&dir, false) else { return };
        let memo = dir.join("memo");
        let first = dependent(&memo);
        let hashed = first.hash(&archive, true).unwrap();
        assert_eq!(first.tally().0, bytes.len() as u64);
        let next = dependent(&memo);
        assert_eq!(next.hash(&archive, true).unwrap().hash, hashed.hash);
        assert_eq!(next.tally(), (0, bytes.len() as u64), "the next unit takes the young archive's hash from the memo");
        let audit = dependent(&memo).audit_attested(true);
        assert_eq!(audit.hash(&archive, true).unwrap().hash, hashed.hash);
        assert_eq!(audit.tally().0, bytes.len() as u64, "audit mode proves the record by content");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn small_objects_in_a_build_script_link_directory_are_read_once() {
        let dir = scratch("witness-small");
        let Some((archive, _)) = build_script_archive(&dir, false) else { return };
        let out = archive.parent().unwrap().to_path_buf();
        let nested = out.join("obj");
        fs::create_dir_all(&nested).unwrap();
        for (i, name) in ["a.o", "b.o", "c.o"].iter().enumerate() {
            fs::write(nested.join(name), vec![i as u8; 900]).unwrap();
        }
        // Rewritten after the objects, as cargo does once the script exits.
        std::thread::sleep(std::time::Duration::from_millis(30));
        fs::write(out.parent().unwrap().join("output"), "cargo:rustc-link-search=native=out\n").unwrap();
        let memo = dir.join("memo");
        let first = dependent(&memo);
        let hash = first.hash_dir(&out, true, None, &mut Vec::new());
        assert!(first.tally().0 > 0);
        let link = dependent(&memo);
        assert_eq!(link.hash_dir(&out, true, None, &mut Vec::new()), hash);
        assert_eq!(link.tally().0, 0, "a linking dependent takes every file of the directory from the memo");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_build_script_archive_newer_than_its_output_file_waits_for_the_settle_margin() {
        let dir = scratch("witness-unsettled");
        let Some((archive, bytes)) = build_script_archive(&dir, true) else { return };
        let memo = dir.join("memo");
        dependent(&memo).hash(&archive, true).unwrap();
        let next = dependent(&memo);
        next.hash(&archive, true).unwrap();
        assert_eq!(next.tally().0, bytes.len() as u64, "an archive the script may still be writing is read again");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_attestation_is_void_once_size_mtime_or_inode_differs() {
        let dir = scratch("attest-void");
        let memo = dir.join("memo");
        for change in ["size", "mtime", "inode"] {
            let Some((file, bytes, stamp)) = fresh_output(&dir, &format!("{change}.rlib")) else { return };
            let producer = Hasher::new(Some(memo.clone()), Vec::new());
            assert!(producer.attest(&file, &plain(&bytes), &stamp));
            let mut current = bytes.clone();
            match change {
                "size" => {
                    current.push(7);
                    fs::write(&file, &current).unwrap();
                }
                "mtime" => {
                    let later = SystemTime::now() + std::time::Duration::from_secs(5);
                    fs::OpenOptions::new().write(true).open(&file).unwrap().set_modified(later).unwrap();
                }
                _ => {
                    let sibling = dir.join("replacement.rlib");
                    fs::write(&sibling, &current).unwrap();
                    sys::rename(&sibling, &file).unwrap();
                }
            }
            let user = dependent(&memo);
            assert_eq!(user.hash(&file, true).unwrap().hash, plain(&current), "{change}");
            assert_eq!(user.tally().0, current.len() as u64, "{change}: the dependent must read the file");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn attestation_is_refused_for_a_changed_file_a_small_file_and_an_unbelieved_volume() {
        let dir = scratch("attest-refused");
        let Some((file, bytes, stamp)) = fresh_output(&dir, "libk.rlib") else { return };
        let memo = dir.join("memo");
        let producer = Hasher::new(Some(memo.clone()), Vec::new());
        let later = SystemTime::now() + std::time::Duration::from_secs(5);
        fs::OpenOptions::new().write(true).open(&file).unwrap().set_modified(later).unwrap();
        assert!(!producer.attest(&file, &plain(&bytes), &stamp), "the file changed after it was hashed");

        let small = dir.join("small.rlib");
        fs::write(&small, b"tiny").unwrap();
        let small_stamp = Stamp::read(&small).unwrap();
        assert!(!producer.attest(&small, &plain(b"tiny"), &small_stamp));

        let (untrusted, bytes, stamp) = fresh_output(&dir, "libu.rlib").unwrap();
        let distrusting = Hasher::new(Some(memo), Vec::new()).with_stamp_trust(Box::new(|_, _| false));
        assert!(!distrusting.attest(&untrusted, &plain(&bytes), &stamp), "off mode never records attestations");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_distrusting_dependent_ignores_attestations() {
        let dir = scratch("attest-off");
        let Some((file, bytes, stamp)) = fresh_output(&dir, "libk.rlib") else { return };
        let memo = dir.join("memo");
        assert!(Hasher::new(Some(memo.clone()), Vec::new()).attest(&file, &plain(&bytes), &stamp));
        let off = Hasher::new(Some(memo), Vec::new()).with_stamp_trust(Box::new(|_, _| false));
        assert_eq!(off.hash(&file, true).unwrap().hash, plain(&bytes));
        assert_eq!(off.tally(), (ATTESTED_LEN as u64, 0));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn audit_mode_proves_attested_records_by_content_and_discards_a_false_one() {
        let dir = scratch("attest-audit");
        let Some((file, bytes, stamp)) = fresh_output(&dir, "libk.rlib") else { return };
        let memo = dir.join("memo");
        let producer = Hasher::new(Some(memo.clone()), Vec::new());
        assert!(producer.attest(&file, &plain(&bytes), &stamp));
        let audited = dependent(&memo).audit_attested(true);
        assert_eq!(audited.hash(&file, true).unwrap().hash, plain(&bytes));
        assert_eq!(audited.tally(), (ATTESTED_LEN as u64, ATTESTED_LEN as u64), "a true record is read once to prove it");

        assert!(producer.attest(&file, &plain(b"not the content"), &stamp));
        let reports = std::sync::Arc::new(Mutex::new(Vec::new()));
        let sink = std::sync::Arc::clone(&reports);
        let forged = dependent(&memo).audit_attested(true).with_violation_sink(Box::new(move |path, dev, recorded, fresh| {
            sink.lock().unwrap().push((path.to_path_buf(), dev, recorded.to_string(), fresh.to_string()));
        }));
        assert_eq!(forged.hash(&file, true).unwrap().hash, plain(&bytes), "the audit returns the real content hash");
        assert_eq!(
            *reports.lock().unwrap(),
            vec![(file.clone(), stamp.dev, plain(b"not the content"), plain(&bytes))],
            "a record that matched the stamp but not the content is reported as a violation"
        );
        let later = dependent(&memo);
        assert_eq!(later.hash(&file, true).unwrap().hash, plain(&bytes));
        assert_eq!(later.tally().0, ATTESTED_LEN as u64, "the false record was discarded");
        let _ = fs::remove_dir_all(&dir);
    }

    fn stamp_n(n: u64) -> Stamp {
        Stamp { dev: 1, ino: n, size: 64 * 1024 + n, mtime_ns: 1_000 + n as i128, ctime_ns: 2_000 + n as i128 }
    }

    fn live_n(n: u64, attested: bool, written_s: u64) -> memolog::Live {
        memolog::Live { stamp: stamp_n(n), hash: *blake3::hash(&n.to_le_bytes()).as_bytes(), attested, written_s }
    }

    fn put(log: &Path, key: &str, live: Option<&memolog::Live>) {
        assert!(memolog::append(log, &memolog::encode(key, live).unwrap(), u64::MAX));
    }

    fn loaded(log: &Path) -> memolog::View {
        let mut view = memolog::View::default();
        view.refresh(log);
        view
    }

    #[test]
    fn log_records_round_trip_with_stamp_hash_and_attestation() {
        let dir = scratch("log-round");
        let log = dir.join(memolog::LOG_NAME);
        let (a, b) = (live_n(1, false, 5), live_n(2, true, 6));
        put(&log, "/w/a.rlib", Some(&a));
        put(&log, "/w/b.rlib", Some(&b));
        let view = loaded(&log);
        let got = view.get("/w/a.rlib").unwrap();
        assert_eq!((got.stamp, got.hash, got.attested, got.written_s), (a.stamp, a.hash, false, 5));
        let got = view.get("/w/b.rlib").unwrap();
        assert_eq!((got.stamp, got.hash, got.attested), (b.stamp, b.hash, true));
        assert!(view.get("/w/c.rlib").is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_last_valid_record_for_a_key_wins() {
        let dir = scratch("log-last");
        let log = dir.join(memolog::LOG_NAME);
        put(&log, "/w/a", Some(&live_n(1, false, 1)));
        put(&log, "/w/a", Some(&live_n(2, true, 2)));
        let view = loaded(&log);
        assert_eq!(view.get("/w/a").unwrap().stamp, stamp_n(2));
        assert!(view.get("/w/a").unwrap().attested);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_tombstone_voids_a_key_until_it_is_recorded_again() {
        let dir = scratch("log-tomb");
        let log = dir.join(memolog::LOG_NAME);
        put(&log, "/w/a", Some(&live_n(1, true, 1)));
        put(&log, "/w/b", Some(&live_n(2, false, 1)));
        put(&log, "/w/a", None);
        let view = loaded(&log);
        assert!(view.get("/w/a").is_none() && view.get("/w/b").is_some());
        put(&log, "/w/a", Some(&live_n(3, false, 2)));
        assert_eq!(loaded(&log).get("/w/a").unwrap().stamp, stamp_n(3));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_torn_tail_is_a_miss_and_later_frames_are_still_read() {
        let dir = scratch("log-torn");
        let log = dir.join(memolog::LOG_NAME);
        put(&log, "/w/a", Some(&live_n(1, false, 1)));
        let torn = memolog::encode("/w/b", Some(&live_n(2, false, 1))).unwrap();
        for cut in [1, 3, 6, 9, torn.len() - 1] {
            let mut file = fs::OpenOptions::new().append(true).open(&log).unwrap();
            file.write_all(&torn[..cut]).unwrap();
            let view = loaded(&log);
            assert!(view.get("/w/a").is_some() && view.get("/w/b").is_none(), "cut {cut}");
            let len = fs::metadata(&log).unwrap().len();
            File::options().write(true).open(&log).unwrap().set_len(len - cut as u64).unwrap();
        }
        let mut file = fs::OpenOptions::new().append(true).open(&log).unwrap();
        file.write_all(&torn[..torn.len() - 1]).unwrap();
        put(&log, "/w/c", Some(&live_n(3, false, 1)));
        let view = loaded(&log);
        assert!(view.get("/w/a").is_some() && view.get("/w/b").is_none() && view.get("/w/c").is_some());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_middle_frame_is_skipped_and_the_rest_survive() {
        let dir = scratch("log-corrupt");
        let log = dir.join(memolog::LOG_NAME);
        put(&log, "/w/a", Some(&live_n(1, false, 1)));
        let first = fs::metadata(&log).unwrap().len() as usize;
        put(&log, "/w/b", Some(&live_n(2, false, 1)));
        let second = fs::metadata(&log).unwrap().len() as usize;
        put(&log, "/w/c", Some(&live_n(3, false, 1)));
        for at in [first + 2, first + 6, first + 20, (first + second) / 2, second - 3] {
            let original = fs::read(&log).unwrap();
            let mut bytes = original.clone();
            bytes[at] ^= 0x55;
            fs::write(&log, &bytes).unwrap();
            let view = loaded(&log);
            assert!(view.get("/w/b").is_none(), "flip at {at}");
            assert!(view.get("/w/a").is_some() && view.get("/w/c").is_some(), "flip at {at}");
            fs::write(&log, &original).unwrap();
        }
        fs::write(&log, b"not a log at all \xff\x00 fh3\n\xff\xff\xff\xff").unwrap();
        assert!(loaded(&log).get("/w/a").is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_view_sees_frames_a_peer_appends_after_it_loaded() {
        let dir = scratch("log-peer");
        let log = dir.join(memolog::LOG_NAME);
        put(&log, "/w/a", Some(&live_n(1, false, 1)));
        let mut view = loaded(&log);
        assert!(view.get("/w/b").is_none());
        put(&log, "/w/b", Some(&live_n(2, false, 1)));
        view.refresh(&log);
        assert!(view.get("/w/a").is_some() && view.get("/w/b").is_some());
        let _ = memolog::compact(&log, 0, u64::MAX);
        view.refresh(&log);
        assert!(view.get("/w/b").is_none(), "a replaced log is read afresh");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_threads_appending_lose_no_frame() {
        let dir = scratch("log-threads");
        let log = dir.join(memolog::LOG_NAME);
        let workers: Vec<_> = (0..2u64)
            .map(|t| {
                let log = log.clone();
                std::thread::spawn(move || {
                    for n in 0..300u64 {
                        put(&log, &format!("/w/{t}/{n}/{}", "x".repeat(200)), Some(&live_n(n, false, 1)));
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        let view = loaded(&log);
        for t in 0..2u64 {
            for n in 0..300u64 {
                assert_eq!(view.get(&format!("/w/{t}/{n}/{}", "x".repeat(200))).unwrap().stamp, stamp_n(n));
            }
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn compaction_keeps_live_records_and_drops_superseded_tombstoned_and_expired_ones() {
        let dir = scratch("log-compact");
        let log = dir.join(memolog::LOG_NAME);
        let now = fsx::unix_now();
        put(&log, "/w/a", Some(&live_n(1, false, now)));
        put(&log, "/w/a", Some(&live_n(2, true, now)));
        put(&log, "/w/dead", Some(&live_n(3, false, now)));
        put(&log, "/w/dead", None);
        put(&log, "/w/old", Some(&live_n(4, false, now - 1_000)));
        put(&log, "/w/b", Some(&live_n(5, false, now)));
        let before = fs::metadata(&log).unwrap().len();
        let (was, is) = memolog::compact(&log, 500, u64::MAX).unwrap();
        assert_eq!(was, before);
        assert_eq!(is, fs::metadata(&log).unwrap().len());
        assert!(is < was);
        let view = loaded(&log);
        assert_eq!(view.get("/w/a").unwrap().stamp, stamp_n(2));
        assert!(view.get("/w/b").is_some());
        assert!(view.get("/w/dead").is_none() && view.get("/w/old").is_none());
        let unchanged = memolog::compact(&log, 500, u64::MAX).unwrap();
        assert_eq!(unchanged, (is, is), "nothing left to drop leaves the file alone");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1, "no temp file is left behind");
        assert_eq!(compact_memo_log(&dir, 0), 0, "records written this second are not yet expired");
        std::thread::sleep(std::time::Duration::from_millis(2100));
        assert_eq!(compact_memo_log(&dir, 1), is, "once every record is older than the limit the file goes");
        assert!(!log.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_log_stays_bounded_and_keeps_the_newest_records() {
        let dir = scratch("log-bound");
        let memo = dir.join("memo");
        let cap = 16 * 1024;
        let hasher = Hasher::new(Some(memo.clone()), Vec::new()).with_log_cap(cap);
        let hash = plain(b"x");
        for n in 0..600u64 {
            assert!(hasher.write_record(&dir.join(format!("f{n}")), &stamp_n(n), &hash, false));
            assert!(fs::metadata(memo.join(memolog::LOG_NAME)).unwrap().len() <= cap, "after record {n}");
        }
        let view = loaded(&memo.join(memolog::LOG_NAME));
        assert!(view.get(dir.join("f599").to_str().unwrap()).is_some());
        assert!(view.get(dir.join("f0").to_str().unwrap()).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn three_hundred_records_occupy_one_file_of_frame_size() {
        let dir = scratch("log-footprint");
        let memo = dir.join("memo");
        let hasher = Hasher::new(Some(memo.clone()), Vec::new());
        let hash = plain(b"x");
        let mut expected = 0;
        for n in 0..300u64 {
            let file = dir.join(format!("target/debug/deps/libcrate{n}-0123456789abcdef.rlib"));
            assert!(hasher.write_record(&file, &stamp_n(n), &hash, n % 2 == 0));
            expected += memolog::encode(file.to_str().unwrap(), Some(&live_n(n, false, 0))).unwrap().len() as u64;
        }
        let entries: Vec<_> = fs::read_dir(&memo).unwrap().flatten().collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].metadata().unwrap().len(), expected);
        eprintln!("300 records: 1 file, {expected} bytes ({} per record)", expected / 300);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_per_file_records_are_never_trusted() {
        let dir = scratch("log-legacy");
        let file = dir.join("big.rlib");
        let memo = dir.join("memo");
        fs::write(&file, vec![1u8; 64 * 1024]).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2100));
        let stamp = Stamp::read(&file).unwrap();
        let key = blake3::hash(file.as_os_str().as_encoded_bytes()).to_hex();
        let legacy = memo.join(&key[..2]).join(&key[2..34]);
        let body = format!(
            "frust-hashmemo-v2\n{}\n{} {} {} {} {}\n{}\nattested\n",
            file.to_str().unwrap(), stamp.dev, stamp.ino, stamp.size, stamp.mtime_ns, stamp.ctime_ns, plain(b"forged")
        );
        fsx::write_atomic(&legacy, body.as_bytes()).unwrap();
        let hasher = Hasher::new(Some(memo.clone()), Vec::new());
        assert_eq!(hasher.hash(&file, true).unwrap().hash, plain(&[1u8; 64 * 1024]));
        assert!(legacy.exists(), "the reader leaves legacy files for gc");
        assert!(memo.join(memolog::LOG_NAME).exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
