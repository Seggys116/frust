//! Clone or copy artifacts, never hard-link: rustc/linker in-place rewrites could corrupt the cache.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::sys::{self, signal};

static COUNTER: AtomicU64 = AtomicU64::new(0);

pub const TEMP_MARKER: &str = ".frust-";

pub fn temp_owner(name: &str) -> Option<u32> {
    if !name.starts_with('.') {
        return None;
    }
    let (_, rest) = name.rsplit_once(TEMP_MARKER)?;
    let mut fields = rest.split('-');
    let pid = fields.next()?.parse().ok()?;
    let counter_and_nanos = (fields.next()?, fields.next()?);
    if fields.next().is_some() || counter_and_nanos.0.parse::<u64>().is_err() || counter_and_nanos.1.parse::<u64>().is_err() {
        return None;
    }
    Some(pid)
}

// Slots go FREE -> BUSY -> ARMED -> FREE; the handler takes ARMED -> HANDLER so it never reads a torn path.

const SLOTS: usize = 8;
const PATH_CAP: usize = 1024;
const FREE: u8 = 0;
const BUSY: u8 = 1;
const ARMED: u8 = 2;
const HANDLER: u8 = 3;

struct TempSlot {
    state: AtomicU8,
    path: UnsafeCell<[u8; PATH_CAP]>,
}

// SAFETY: `path` is written only after FREE -> BUSY and read by the handler only after ARMED -> HANDLER, so accesses are exclusive.
unsafe impl Sync for TempSlot {}

#[allow(clippy::declare_interior_mutable_const)]
const EMPTY_SLOT: TempSlot = TempSlot { state: AtomicU8::new(FREE), path: UnsafeCell::new([0; PATH_CAP]) };
static TEMPS: [TempSlot; SLOTS] = [EMPTY_SLOT; SLOTS];

struct TempGuard(Option<usize>);

impl TempGuard {
    fn new(path: &Path) -> TempGuard {
        let bytes = path.as_os_str().as_encoded_bytes();
        if bytes.len() >= PATH_CAP || bytes.contains(&0) {
            return TempGuard(None);
        }
        for (i, slot) in TEMPS.iter().enumerate() {
            if slot.state.compare_exchange(FREE, BUSY, Ordering::Acquire, Ordering::Relaxed).is_ok() {
                // SAFETY: we own the slot in state BUSY (see `TempSlot`).
                let buf = unsafe { &mut *slot.path.get() };
                buf[..bytes.len()].copy_from_slice(bytes);
                buf[bytes.len()] = 0;
                slot.state.store(ARMED, Ordering::Release);
                return TempGuard(Some(i));
            }
        }
        TempGuard(None)
    }
}

impl Drop for TempGuard {
    fn drop(&mut self) {
        if let Some(i) = self.0 {
            // Fails only when the handler already owns the slot; the process is ending.
            let _ = TEMPS[i].state.compare_exchange(ARMED, FREE, Ordering::AcqRel, Ordering::Relaxed);
        }
    }
}

extern "C" fn clean_temps_and_reraise(sig: signal::Signal) {
    for slot in &TEMPS {
        if slot.state.compare_exchange(ARMED, HANDLER, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
            // SAFETY: the handler owns the slot; the buffer holds a NUL-terminated path.
            unsafe { signal::unlink_from_handler(&*slot.path.get()) };
        }
    }
    // Restore the default action and re-raise, so the process ends as it would have without the handler.
    signal::set_default(sig);
    signal::raise(sig);
}

/// Handlers are reset across `exec`, so a passed-through compiler is unaffected.
pub fn install_temp_cleanup() {
    for sig in [signal::SIGINT, signal::SIGTERM, signal::SIGHUP] {
        let _ = signal::install(sig, clean_temps_and_reraise);
    }
}

pub fn temp_sibling(path: &Path) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    path.with_file_name(format!(".{name}{TEMP_MARKER}{}-{n}-{nanos}", std::process::id()))
}

pub fn clone_replace(src: &Path, dst: &Path) -> io::Result<()> {
    let tmp = temp_sibling(dst);
    let _guard = TempGuard::new(&tmp);
    let built = match sys::reflink(src, &tmp) {
        Ok(()) => Ok(()),
        Err(_) => {
            let _ = remove_any(&tmp);
            copy_tree(src, &tmp)
        }
    };
    if let Err(err) = built {
        let _ = remove_any(&tmp);
        return Err(err);
    }
    if let Ok(meta) = fs::symlink_metadata(dst)
        && meta.is_dir()
    {
        fs::remove_dir_all(dst)?;
    }
    sys::rename(&tmp, dst).inspect_err(|_| {
        let _ = remove_any(&tmp);
    })
}

/// For paths nobody else can see yet (a private staging directory), so no temp name and rename per file.
pub fn clone_new(src: &Path, dst: &Path) -> io::Result<()> {
    if sys::reflink(src, dst).is_ok() {
        return Ok(());
    }
    let _ = remove_any(dst);
    copy_tree(src, dst).inspect_err(|_| {
        let _ = remove_any(dst);
    })
}

pub fn reflink_supported(dir: &Path) -> bool {
    if fs::create_dir_all(dir).is_err() {
        return false;
    }
    let source = temp_sibling(&dir.join("probe"));
    let clone = temp_sibling(&dir.join("probe-clone"));
    let supported = fs::write(&source, b"frust reflink probe").is_ok() && sys::reflink(&source, &clone).is_ok();
    let _ = fs::remove_file(&source);
    let _ = fs::remove_file(&clone);
    supported
}

fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(src)?;
    if meta.is_dir() {
        fs::create_dir_all(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            copy_tree(&entry.path(), &dst.join(entry.file_name()))?;
        }
        fs::set_permissions(dst, meta.permissions())?;
        Ok(())
    } else if meta.file_type().is_symlink() {
        sys::copy_symlink(src, dst)
    } else {
        fs::copy(src, dst).map(|_| ())
    }
}

pub fn remove_any(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

pub fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    let tmp = temp_sibling(path);
    let _guard = TempGuard::new(&tmp);
    let result = (|| {
        // The parent exists almost always, so it is created only after the first attempt finds it missing.
        let mut file = match File::create(&tmp) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => match path.parent() {
                Some(parent) => {
                    fs::create_dir_all(parent)?;
                    File::create(&tmp)?
                }
                None => return Err(err),
            },
            other => other?,
        };
        file.write_all(contents)?;
        file.flush()?;
        sys::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Sets the modification (and access) time of a file or directory to now with one syscall; failures are ignored.
pub fn touch(path: &Path) {
    let _ = sys::set_mtime_now(path);
}

pub fn allocated_size(path: &Path) -> u64 {
    fn walk(path: &Path, seen: &mut std::collections::HashSet<(u64, u64)>) -> u64 {
        let Ok(meta) = sys::lstat(path) else { return 0 };
        if meta.is_dir() {
            let mut total = meta.allocated();
            if let Ok(entries) = fs::read_dir(path) {
                for entry in entries.flatten() {
                    total += walk(&entry.path(), seen);
                }
            }
            total
        } else if meta.nlink() > 1 && !seen.insert((meta.dev(), meta.ino())) {
            0
        } else {
            meta.allocated()
        }
    }
    walk(path, &mut std::collections::HashSet::new())
}

/// `du` charges each reflinked file in full; on APFS the first extent's device offset identifies whole-file clones, so they are counted once.
pub fn physical_size(roots: &[&Path]) -> u64 {
    let mut seen_extents = std::collections::HashSet::new();
    let mut seen_inodes = std::collections::HashSet::new();
    roots.iter().map(|root| physical_walk(root, &mut seen_extents, &mut seen_inodes)).sum()
}

fn physical_walk(
    path: &Path,
    extents: &mut std::collections::HashSet<(u64, u64)>,
    inodes: &mut std::collections::HashSet<(u64, u64)>,
) -> u64 {
    let Ok(meta) = sys::lstat(path) else { return 0 };
    if meta.is_dir() {
        let mut total = meta.allocated();
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                total += physical_walk(&entry.path(), extents, inodes);
            }
        }
        return total;
    }
    if !meta.is_file() {
        return 0;
    }
    if !inodes.insert((meta.dev(), meta.ino())) {
        return 0;
    }
    let bytes = meta.allocated();
    match sys::first_extent(path) {
        Some(offset) if !extents.insert((offset, meta.len())) => 0,
        _ => bytes,
    }
}

pub fn free_space(path: &Path) -> io::Result<u64> {
    let mut probe = path;
    while !probe.exists() {
        probe = probe.parent().ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
    }
    sys::free_space(probe)
}

pub fn require_free_space(path: &Path, min_free: u64) -> Result<(), String> {
    match free_space(path) {
        Ok(free) if free >= min_free => Ok(()),
        Ok(free) => Err(format!(
            "only {} free on the volume holding {}; need at least {}",
            human_bytes(free),
            path.display(),
            human_bytes(min_free)
        )),
        Err(err) => Err(format!("cannot determine free space for {}: {err}", path.display())),
    }
}

pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} B") } else { format!("{value:.1} {}", UNITS[unit]) }
}

pub fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("frust-fsx-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn write_atomic_creates_missing_parents_only_when_needed_and_replaces_contents() {
        let dir = scratch("atomic");
        let nested = dir.join("a").join("b").join("file");
        write_atomic(&nested, b"one").unwrap();
        assert_eq!(fs::read(&nested).unwrap(), b"one");
        write_atomic(&nested, b"two").unwrap();
        assert_eq!(fs::read(&nested).unwrap(), b"two");
        let leftovers: Vec<_> = fs::read_dir(nested.parent().unwrap()).unwrap().flatten().collect();
        assert_eq!(leftovers.len(), 1, "no temp file is left beside the target");
        let blocked = dir.join("a").join("b").join("file").join("child");
        assert!(write_atomic(&blocked, b"x").is_err(), "a file in the way of the parent is an error");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn touch_moves_modification_time_to_now_for_files_and_directories() {
        let dir = scratch("touch");
        let file = dir.join("f");
        fs::write(&file, b"x").unwrap();
        let old = SystemTime::now() - Duration::from_secs(7200);
        File::options().write(true).open(&file).unwrap().set_modified(old).unwrap();
        let sub = dir.join("sub");
        fs::create_dir_all(&sub).unwrap();
        let mut targets = vec![file.clone()];
        // Windows cannot open a directory for writing through std, so only unix can age one here.
        if cfg!(unix) {
            File::open(&sub).unwrap().set_modified(old).unwrap();
            targets.push(sub);
        }
        for path in &targets {
            touch(path);
            let age = fs::metadata(path).unwrap().modified().unwrap().elapsed().unwrap_or_default();
            assert!(age < Duration::from_secs(600), "{}", path.display());
        }
        touch(&dir.join("absent"));
        assert!(!dir.join("absent").exists(), "touching a missing path creates nothing");
        let _ = fs::remove_dir_all(&dir);
    }
}
