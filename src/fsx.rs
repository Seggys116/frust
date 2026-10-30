//! Filesystem helpers: copy-on-write clones, atomic writes, sizes and disk guards.
//! Artifacts are cloned or copied, never hard-linked: rustc and linkers may rewrite a file in place and would corrupt the cache.

use std::ffi::CString;
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

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
        let bytes = path.as_os_str().as_bytes();
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

extern "C" fn clean_temps_and_reraise(signal: libc::c_int) {
    for slot in &TEMPS {
        if slot.state.compare_exchange(ARMED, HANDLER, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
            // SAFETY: the handler owns the slot; the buffer holds a NUL-terminated path.
            unsafe { libc::unlink((*slot.path.get()).as_ptr().cast()) };
        }
    }
    // SAFETY: restore the default action and re-raise, so the process ends as it would have without the handler.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = libc::SIG_DFL;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(signal, &action, std::ptr::null_mut());
        libc::raise(signal);
    }
}

/// Handlers are reset across `exec`, so a passed-through compiler is unaffected.
pub fn install_temp_cleanup() {
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: installing a plain handler through sigaction with an initialised struct.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = clean_temps_and_reraise as extern "C" fn(libc::c_int) as usize;
            libc::sigemptyset(&mut action.sa_mask);
            action.sa_flags = libc::SA_RESTART;
            libc::sigaction(signal, &action, std::ptr::null_mut());
        }
    }
}

pub fn temp_sibling(path: &Path) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    path.with_file_name(format!(".{name}{TEMP_MARKER}{}-{n}-{nanos}", std::process::id()))
}

fn cstr(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
}

#[cfg(target_os = "macos")]
fn try_reflink(src: &Path, dst: &Path) -> io::Result<()> {
    let (s, d) = (cstr(src)?, cstr(dst)?);
    // SAFETY: both pointers are valid NUL-terminated strings for the call duration.
    let rc = unsafe { libc::clonefile(s.as_ptr(), d.as_ptr(), 0) };
    if rc == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

#[cfg(target_os = "linux")]
fn try_reflink(src: &Path, dst: &Path) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    if fs::symlink_metadata(src)?.is_dir() {
        return Err(io::Error::from(io::ErrorKind::Unsupported));
    }
    let input = File::open(src)?;
    let output = File::options().write(true).create_new(true).open(dst)?;
    const FICLONE: libc::c_ulong = 0x4004_9409;
    // SAFETY: both descriptors are open for the duration of the call.
    let rc = unsafe { libc::ioctl(output.as_raw_fd(), FICLONE, input.as_raw_fd()) };
    if rc == 0 {
        let _ = output.set_permissions(input.metadata()?.permissions());
        Ok(())
    } else {
        let err = io::Error::last_os_error();
        drop(output);
        let _ = fs::remove_file(dst);
        Err(err)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn try_reflink(_src: &Path, _dst: &Path) -> io::Result<()> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

pub fn clone_replace(src: &Path, dst: &Path) -> io::Result<()> {
    let tmp = temp_sibling(dst);
    let _guard = TempGuard::new(&tmp);
    let built = match try_reflink(src, &tmp) {
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
    fs::rename(&tmp, dst).inspect_err(|_| {
        let _ = remove_any(&tmp);
    })
}

/// For paths nobody else can see yet (a private staging directory), so no temp name and rename per file.
pub fn clone_new(src: &Path, dst: &Path) -> io::Result<()> {
    if try_reflink(src, dst).is_ok() {
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
    let supported = fs::write(&source, b"frust reflink probe").is_ok() && try_reflink(&source, &clone).is_ok();
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
        std::os::unix::fs::symlink(fs::read_link(src)?, dst)
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
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = temp_sibling(path);
    let _guard = TempGuard::new(&tmp);
    let result = (|| {
        let mut file = File::create(&tmp)?;
        file.write_all(contents)?;
        file.flush()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

pub fn touch(path: &Path) {
    if let Ok(file) = File::options().write(true).open(path) {
        let _ = file.set_modified(SystemTime::now());
    }
}

pub fn allocated_size(path: &Path) -> u64 {
    fn walk(path: &Path, seen: &mut std::collections::HashSet<(u64, u64)>) -> u64 {
        let Ok(meta) = fs::symlink_metadata(path) else { return 0 };
        if meta.is_dir() {
            let mut total = meta.blocks() * 512;
            if let Ok(entries) = fs::read_dir(path) {
                for entry in entries.flatten() {
                    total += walk(&entry.path(), seen);
                }
            }
            total
        } else if meta.nlink() > 1 && !seen.insert((meta.dev(), meta.ino())) {
            0
        } else {
            meta.blocks() * 512
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
    let Ok(meta) = fs::symlink_metadata(path) else { return 0 };
    if meta.is_dir() {
        let mut total = meta.blocks() * 512;
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
    let bytes = meta.blocks() * 512;
    match first_extent(path) {
        Some(offset) if !extents.insert((offset, meta.len())) => 0,
        _ => bytes,
    }
}

#[cfg(target_os = "macos")]
fn first_extent(path: &Path) -> Option<u64> {
    use std::os::fd::AsRawFd;
    let file = File::open(path).ok()?;
    let mut info = libc::log2phys { l2p_flags: 0, l2p_contigbytes: 1, l2p_devoffset: 0 };
    // SAFETY: `info` is a valid log2phys struct and the descriptor is open.
    let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_LOG2PHYS_EXT, &mut info) };
    (rc == 0 && info.l2p_devoffset > 0).then_some(info.l2p_devoffset as u64)
}

#[cfg(not(target_os = "macos"))]
fn first_extent(_path: &Path) -> Option<u64> {
    None
}

pub fn free_space(path: &Path) -> io::Result<u64> {
    let mut probe = path;
    while !probe.exists() {
        probe = probe.parent().ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
    }
    let c = cstr(probe)?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid C string and `stat` is a valid out pointer.
    let rc = unsafe { libc::statvfs(c.as_ptr(), &mut stat) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(stat.f_bavail as u64 * stat.f_frsize as u64)
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

pub fn pid_alive(pid: u32) -> bool {
    // SAFETY: kill with signal 0 only checks for existence.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    rc == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}
