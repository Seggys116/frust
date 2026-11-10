//! An append-only log of keyed records in one file: the newest frame of a key wins, a tombstone drops the key, and a frame torn by a crash is skipped.
//! Writers serialize on a lock the caller holds; readers take no lock and see either a whole frame or none of it.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Write};
use std::ops::Range;
use std::path::Path;

use crate::fsx;
use crate::lock::{self, Mode};
use crate::sys;

const MAGIC: [u8; 4] = *b"fk1\n";
const HEADER: usize = MAGIC.len() + 4;
const SUM: usize = 8;
const KIND_RECORD: u8 = 1;
const KIND_TOMBSTONE: u8 = 2;
const MAX_KEY: usize = 64;
const MIN_PAYLOAD: usize = 1 + 8 + 1 + 1;
const MAX_PAYLOAD: usize = 1 + 8 + 1 + MAX_KEY + 32 * 1024 * 1024;
/// A log past this size is not read, so a runaway file cannot stall every lookup.
pub const MAX_LOG_BYTES: u64 = 256 * 1024 * 1024;
/// Appends rewrite the log once superseded frames outweigh live ones and it has grown past this.
const COMPACT_MIN_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Live {
    written_s: u64,
    frame: (usize, usize),
    body: (usize, usize),
}

/// One read of the log: the live frame of every key.
#[derive(Debug, Default)]
pub struct Log {
    bytes: Vec<u8>,
    live: HashMap<String, Live>,
}

impl Log {
    /// A missing log reads as empty; an oversized or unreadable one is an error so callers leave it alone.
    pub fn read(path: &Path) -> io::Result<Log> {
        let bytes = read_bounded(path)?;
        let mut live = HashMap::new();
        scan(&bytes, true, |frame, kind, written_s, key, body| {
            if kind == KIND_TOMBSTONE {
                live.remove(key);
            } else {
                live.insert(key.to_owned(), Live { written_s, frame: (frame.start, frame.end), body: (body.start, body.end) });
            }
        });
        Ok(Log { bytes, live })
    }

    pub fn contains(&self, key: &str) -> bool {
        self.live.contains_key(key)
    }

    /// The unpacked record of `key` and when it was written, in unix seconds.
    pub fn get(&self, key: &str, max: usize) -> Option<(u64, Vec<u8>)> {
        let live = self.live.get(key)?;
        let raw = fsx::unpack(&self.bytes[live.body.0..live.body.1], max)?;
        Some((live.written_s, raw.into_owned()))
    }

    /// The exact bytes of the live frame of `key`, which change whenever the key is written again.
    pub fn frame(&self, key: &str) -> Option<&[u8]> {
        self.live.get(key).map(|live| &self.bytes[live.frame.0..live.frame.1])
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.live.keys().map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.live.is_empty()
    }

    /// Bytes on disk that no live frame holds.
    pub fn dead_bytes(&self) -> u64 {
        (self.bytes.len() - self.live_bytes()) as u64
    }

    fn live_bytes(&self) -> usize {
        self.live.values().map(|live| live.frame.1 - live.frame.0).sum()
    }
}

/// The newest intact record of `key`, checksumming only the frames of that key so a lookup stays cheap as the log grows; a damaged newest frame falls back to the one before it.
pub fn find(path: &Path, key: &str, max: usize) -> Option<(u64, Vec<u8>)> {
    let bytes = read_bounded(path).ok()?;
    let mut frames = Vec::new();
    scan(&bytes, false, |frame, kind, written_s, found, body| {
        if found == key {
            frames.push((frame, kind, written_s, body));
        }
    });
    let (_, kind, written_s, body) = frames.into_iter().rev().find(|(frame, ..)| intact(&bytes[frame.clone()]))?;
    if kind != KIND_RECORD {
        return None;
    }
    Some((written_s, fsx::unpack(&bytes[body], max)?.into_owned()))
}

/// A missing log reads as empty; anything but a bounded regular file is an error.
fn read_bounded(path: &Path) -> io::Result<Vec<u8>> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let meta = named_regular_file(path, &file)?;
    if meta.len() > MAX_LOG_BYTES {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "record log is too large"));
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    io::Read::read_to_end(&mut file, &mut bytes)?;
    Ok(bytes)
}

fn intact(frame: &[u8]) -> bool {
    frame.len() > MAGIC.len() + SUM && blake3::hash(&frame[MAGIC.len()..frame.len() - SUM]).as_bytes()[..SUM] == frame[frame.len() - SUM..]
}

/// A frame holding `raw` packed under `key`, or a tombstone when `raw` is `None`; `None` for a key the log cannot hold.
pub fn encode(key: &str, written_s: u64, raw: Option<&[u8]>) -> Option<Vec<u8>> {
    if key.is_empty() || key.len() > MAX_KEY {
        return None;
    }
    let body = raw.map(fsx::pack).unwrap_or_default();
    let mut payload = Vec::with_capacity(1 + 8 + 1 + key.len() + body.len());
    payload.push(if raw.is_some() { KIND_RECORD } else { KIND_TOMBSTONE });
    payload.extend_from_slice(&written_s.to_le_bytes());
    payload.push(key.len() as u8);
    payload.extend_from_slice(key.as_bytes());
    payload.extend_from_slice(&body);
    if payload.len() > MAX_PAYLOAD {
        return None;
    }
    let mut frame = Vec::with_capacity(HEADER + payload.len() + SUM);
    frame.extend_from_slice(&MAGIC);
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(&payload);
    let sum = blake3::hash(&frame[MAGIC.len()..]);
    frame.extend_from_slice(&sum.as_bytes()[..SUM]);
    Some(frame)
}

/// Visits each whole frame in order, checksummed when `verify` is set; anything else is skipped a byte at a time until the next frame.
fn scan(buf: &[u8], verify: bool, mut visit: impl FnMut(Range<usize>, u8, u64, &str, Range<usize>)) {
    let mut at = 0;
    while at + HEADER <= buf.len() {
        let Some(rel) = buf[at..].windows(MAGIC.len()).position(|w| w == MAGIC) else { return };
        at += rel;
        let Some(len) = buf.get(at + MAGIC.len()..at + HEADER).map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize) else { return };
        let end = at + HEADER + len + SUM;
        if !(MIN_PAYLOAD..=MAX_PAYLOAD).contains(&len) || end > buf.len() {
            at += 1;
            continue;
        }
        let payload = &buf[at + HEADER..end - SUM];
        let kind = payload[0];
        let written_s = u64::from_le_bytes(payload[1..9].try_into().unwrap());
        let key_len = payload[9] as usize;
        let key = payload.get(10..10 + key_len).and_then(|key| std::str::from_utf8(key).ok());
        match key {
            Some(key) if key_len > 0 && (kind == KIND_RECORD || (kind == KIND_TOMBSTONE && payload.len() == 10 + key_len))
                && (!verify || intact(&buf[at..end])) =>
            {
                let body = at + HEADER + 10 + key_len..end - SUM;
                visit(at..end, kind, written_s, key, body);
                at = end;
            }
            _ => at += 1,
        }
    }
}

/// Appends whole frames in one write; the caller holds the writers' lock.
pub(crate) fn append(path: &Path, frames: &[u8], mode: Option<Mode>) -> io::Result<()> {
    let mut file = lock::open_shared_append(path, mode)?;
    named_regular_file(path, &file)?;
    file.write_all(frames)?;
    file.flush()
}

/// The open handle must be the regular file the path names, so a planted symlink is refused rather than followed.
fn named_regular_file(path: &Path, file: &File) -> io::Result<sys::Meta> {
    let opened = sys::fstat(file)?;
    let named = sys::lstat(path)?;
    if !opened.is_file() || !named.is_file() || (opened.dev(), opened.ino()) != (named.dev(), named.ino()) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "record log is not a regular file"));
    }
    Ok(opened)
}

/// Replaces the log with its live frames, dropping every key `retire` names; an empty result removes the file.
pub(crate) fn rewrite(path: &Path, log: &Log, retire: impl Fn(&str) -> bool, mode: Option<Mode>) -> io::Result<()> {
    let mut kept: Vec<&Live> = log.live.iter().filter(|(key, _)| !retire(key)).map(|(_, live)| live).collect();
    kept.sort_by_key(|live| live.frame.0);
    if kept.is_empty() {
        return match fs::remove_file(path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        };
    }
    let mut out = Vec::with_capacity(kept.iter().map(|live| live.frame.1 - live.frame.0).sum());
    for live in kept {
        out.extend_from_slice(&log.bytes[live.frame.0..live.frame.1]);
    }
    lock::write_shared(path, &out, mode)
}

/// Appends `frames` to a log the caller read under the writers' lock, rewriting it instead once superseded frames outweigh live ones.
pub(crate) fn commit(path: &Path, log: &Log, frames: &[u8], mode: Option<Mode>) -> io::Result<()> {
    let mut newest: HashMap<String, Option<Range<usize>>> = HashMap::new();
    scan(frames, true, |frame, kind, _, key, _| {
        newest.insert(key.to_owned(), (kind == KIND_RECORD).then_some(frame));
    });
    let superseded: usize = newest.keys().filter_map(|key| log.live.get(key)).map(|live| live.frame.1 - live.frame.0).sum();
    let added: usize = newest.values().flatten().map(|frame| frame.len()).sum();
    let live = log.live_bytes() - superseded + added;
    let total = log.bytes.len() + frames.len();
    if (total as u64) < COMPACT_MIN_BYTES || total - live <= live {
        return append(path, frames, mode);
    }
    let mut kept: Vec<&Live> = log.live.iter().filter(|(key, _)| !newest.contains_key(*key)).map(|(_, live)| live).collect();
    kept.sort_by_key(|live| live.frame.0);
    let mut out = Vec::with_capacity(live);
    for live in kept {
        out.extend_from_slice(&log.bytes[live.frame.0..live.frame.1]);
    }
    let mut fresh: Vec<&Range<usize>> = newest.values().flatten().collect();
    fresh.sort_by_key(|frame| frame.start);
    for frame in fresh {
        out.extend_from_slice(&frames[frame.clone()]);
    }
    if out.is_empty() {
        return match fs::remove_file(path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        };
    }
    lock::write_shared(path, &out, mode)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("frust-pack-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_newest_frame_wins_and_a_tombstone_drops_the_key() {
        let dir = scratch("newest");
        let path = dir.join("log");
        append(&path, &encode("a", 1, Some(b"one")).unwrap(), None).unwrap();
        append(&path, &encode("b", 2, Some(b"two")).unwrap(), None).unwrap();
        append(&path, &encode("a", 3, Some(b"three")).unwrap(), None).unwrap();
        let log = Log::read(&path).unwrap();
        assert_eq!(log.get("a", 1024), Some((3, b"three".to_vec())));
        assert_eq!(log.get("b", 1024), Some((2, b"two".to_vec())));
        assert_eq!(find(&path, "a", 1024), Some((3, b"three".to_vec())));
        append(&path, &encode("a", 4, None).unwrap(), None).unwrap();
        let log = Log::read(&path).unwrap();
        assert!(!log.contains("a"));
        assert!(log.contains("b"));
        assert_eq!(find(&path, "a", 1024), None);
        assert!(find(&dir.join("missing"), "a", 1024).is_none());
        assert_eq!(log.keys().collect::<Vec<_>>(), vec!["b"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_log_is_empty_and_torn_or_corrupt_frames_are_skipped() {
        let dir = scratch("torn");
        let path = dir.join("log");
        assert!(Log::read(&path).unwrap().is_empty());
        let first = encode("a", 1, Some(b"one")).unwrap();
        let second = encode("b", 2, Some(b"two")).unwrap();
        let mut bytes = first.clone();
        bytes.extend_from_slice(&second[..second.len() - 3]);
        let mut flipped = encode("c", 3, Some(b"three")).unwrap();
        let last = flipped.len() - 1;
        flipped[last] ^= 0xff;
        bytes.extend_from_slice(&flipped);
        bytes.extend_from_slice(&encode("d", 4, Some(b"four")).unwrap());
        fs::write(&path, &bytes).unwrap();
        let log = Log::read(&path).unwrap();
        let mut keys: Vec<_> = log.keys().collect();
        keys.sort();
        assert_eq!(keys, vec!["a", "d"]);
        assert_eq!(find(&path, "a", 16), Some((1, b"one".to_vec())));
        assert_eq!(find(&path, "c", 16), None, "a frame failing its checksum is never served");
        assert_eq!(find(&path, "d", 16), Some((4, b"four".to_vec())));
        fs::write(&path, &second[..second.len() - 1]).unwrap();
        assert!(Log::read(&path).unwrap().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rewriting_keeps_live_frames_in_order_and_an_empty_log_is_removed() {
        let dir = scratch("rewrite");
        let path = dir.join("log");
        for (n, key) in ["a", "b", "a", "c"].iter().enumerate() {
            append(&path, &encode(key, n as u64, Some(key.as_bytes())).unwrap(), None).unwrap();
        }
        let log = Log::read(&path).unwrap();
        assert!(log.dead_bytes() > 0);
        rewrite(&path, &log, |key| key == "c", None).unwrap();
        let after = Log::read(&path).unwrap();
        assert_eq!(after.dead_bytes(), 0);
        assert_eq!(after.get("a", 16), Some((2, b"a".to_vec())));
        assert!(after.contains("b") && !after.contains("c"));
        rewrite(&path, &after, |_| true, None).unwrap();
        assert!(!path.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn commits_append_until_superseded_frames_outweigh_live_ones() {
        let dir = scratch("commit");
        let path = dir.join("log");
        let mut sizes = Vec::new();
        for n in 0..6u64 {
            let log = Log::read(&path).unwrap();
            let mut unique = vec![0u8; 40 * 1024];
            blake3::Hasher::new().update(&n.to_le_bytes()).finalize_xof().fill(&mut unique);
            commit(&path, &log, &encode("same", n, Some(&unique)).unwrap(), None).unwrap();
            sizes.push(fs::metadata(&path).unwrap().len());
        }
        let log = Log::read(&path).unwrap();
        assert_eq!(log.get("same", 1 << 20).map(|(written, _)| written), Some(5));
        assert!(sizes.iter().all(|size| *size < 3 * sizes[0]), "{sizes:?}");
        assert!(log.dead_bytes() <= sizes[0]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn keys_the_log_cannot_hold_are_refused() {
        assert!(encode("", 0, Some(b"x")).is_none());
        assert!(encode(&"k".repeat(MAX_KEY + 1), 0, Some(b"x")).is_none());
        assert!(encode(&"k".repeat(MAX_KEY), 0, Some(b"x")).is_some());
    }
}

#[cfg(test)]
mod damaged {
    use super::*;

    #[test]
    fn a_lookup_falls_back_to_the_last_intact_frame_of_its_key() {
        let dir = std::env::temp_dir().join(format!("frust-pack-damaged-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("log");
        let mut bytes = encode("k", 1, Some(b"old")).unwrap();
        let mut newer = encode("k", 2, Some(b"new")).unwrap();
        let middle = newer.len() / 2;
        newer[middle] ^= 0xff;
        bytes.extend_from_slice(&newer);
        fs::write(&path, &bytes).unwrap();
        assert_eq!(find(&path, "k", 16), Some((1, b"old".to_vec())));
        assert_eq!(Log::read(&path).unwrap().get("k", 16), Some((1, b"old".to_vec())));
        let _ = fs::remove_dir_all(&dir);
    }
}
