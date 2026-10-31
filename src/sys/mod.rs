// Backends own the raw std calls banned elsewhere by clippy.toml.
#![allow(clippy::disallowed_methods)]

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
use unix as imp;
#[cfg(windows)]
use windows as imp;

#[cfg(not(any(unix, windows)))]
compile_error!("frust supports only Unix and Windows targets");

use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::process::{ChildStderr, ChildStdout, Command, ExitStatus};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct Meta {
    inner: fs::Metadata,
    ext: imp::Ext,
}

impl Deref for Meta {
    type Target = fs::Metadata;

    fn deref(&self) -> &fs::Metadata {
        &self.inner
    }
}

impl From<fs::Metadata> for Meta {
    fn from(inner: fs::Metadata) -> Meta {
        Meta { ext: imp::Ext::from_std(&inner), inner }
    }
}

impl Meta {
    fn pair((inner, ext): (fs::Metadata, imp::Ext)) -> Meta {
        Meta { inner, ext }
    }

    pub fn dev(&self) -> u64 {
        self.ext.dev(&self.inner)
    }

    pub fn ino(&self) -> u64 {
        self.ext.ino(&self.inner)
    }

    pub fn nlink(&self) -> u64 {
        self.ext.nlink(&self.inner)
    }

    pub fn allocated(&self) -> u64 {
        self.ext.allocated(&self.inner)
    }

    /// Modification time as (seconds, nanoseconds) since the Unix epoch.
    pub fn mtime(&self) -> (i64, i64) {
        self.ext.mtime(&self.inner)
    }

    /// Change time uses Unix-epoch seconds and nanoseconds, with a current-time fallback when unavailable.
    pub fn ctime(&self) -> (i64, i64) {
        self.ext.ctime(&self.inner)
    }

    /// Access time as (seconds, nanoseconds) since the Unix epoch.
    pub fn atime(&self) -> (i64, i64) {
        self.ext.atime(&self.inner)
    }

    pub fn mode(&self) -> u32 {
        self.ext.mode(&self.inner)
    }
}

pub fn stat(path: &Path) -> io::Result<Meta> {
    imp::stat(path).map(Meta::pair)
}

pub fn lstat(path: &Path) -> io::Result<Meta> {
    imp::lstat(path).map(Meta::pair)
}

pub fn fstat(file: &File) -> io::Result<Meta> {
    imp::fstat(file).map(Meta::pair)
}

pub fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    imp::read_exact_at(file, buf, offset)
}

pub fn write_all_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    imp::write_all_at(file, buf, offset)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockKind {
    Shared,
    Exclusive,
}

pub fn lock(file: &File, kind: LockKind) -> io::Result<()> {
    imp::lock(file, kind)
}

/// Takes the lock without blocking; `WouldBlock` when another handle holds a conflicting lock.
pub fn try_lock(file: &File, kind: LockKind) -> Result<(), fs::TryLockError> {
    imp::try_lock(file, kind)
}

pub fn unlock(file: &File) -> io::Result<()> {
    imp::unlock(file)
}

static SESSION_LOCK_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub struct SessionLocks {
    files: Vec<(PathBuf, File)>,
    locked: usize,
    _serial: std::sync::MutexGuard<'static, ()>,
}

impl SessionLocks {
    pub fn metadata(&self, path: &Path) -> io::Result<Meta> {
        self.files.iter().find(|(candidate, _)| candidate == path)
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound)).and_then(|(_, file)| fstat(file))
    }

    pub fn matches_path(&self, path: &Path) -> bool {
        self.files.iter().find(|(candidate, _)| candidate == path)
            .is_some_and(|(_, file)| session_identity_matches(path, file))
    }

    pub fn revalidate(&self) -> bool {
        self.files.iter().all(|(path, file)| session_identity_matches(path, file))
    }
}

fn session_identity_matches(path: &Path, file: &File) -> bool {
    let (Ok(current), Ok(opened)) = (lstat(path), fstat(file)) else { return false };
    current.is_file() && !current.file_type().is_symlink() && current.nlink() == 1 && opened.nlink() == 1
        && current.ino() != 0 && current.ino() == opened.ino() && current.dev() == opened.dev()
}

impl Drop for SessionLocks {
    fn drop(&mut self) {
        for (_, file) in &self.files[..self.locked] {
            imp::unlock_session(file);
        }
        self.files.clear();
    }
}

/// Open and validate every descriptor before locking, so a duplicate can never close a held POSIX alias.
pub fn try_session_locks(paths: &[PathBuf]) -> io::Result<SessionLocks> {
    let serial = match SESSION_LOCK_SERIAL.try_lock() {
        Ok(guard) => guard,
        Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return Err(io::Error::from(io::ErrorKind::WouldBlock)),
    };
    let mut guard = SessionLocks { files: Vec::new(), locked: 0, _serial: serial };
    let mut identities = std::collections::HashSet::new();
    let mut names = std::collections::HashSet::new();
    for path in paths {
        if !names.insert(path) {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        if !matches!(fs_type_name(path).as_deref(), Some("ext4" | "xfs" | "btrfs" | "tmpfs" | "overlay" | "zfs" | "f2fs" | "ramfs" | "bcachefs" | "apfs" | "hfs" | "ntfs" | "refs")) {
            return Err(io::Error::from(io::ErrorKind::Unsupported));
        }
        let before = lstat(path)?;
        if !before.is_file() || before.file_type().is_symlink() || before.nlink() != 1 || before.ino() == 0 {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        let file = imp::open_session_lock(path)?;
        let opened = fstat(&file)?;
        if opened.dev() != before.dev() || opened.ino() != before.ino() || !opened.is_file() || opened.nlink() != 1 {
            return Err(io::Error::from(io::ErrorKind::WouldBlock));
        }
        if !identities.insert((opened.dev(), opened.ino())) {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        guard.files.push((path.clone(), file));
    }
    for (_, file) in &guard.files {
        imp::try_lock_session(file)?;
        guard.locked += 1;
    }
    if !guard.revalidate() {
        return Err(io::Error::from(io::ErrorKind::WouldBlock));
    }
    Ok(guard)
}

pub struct SessionLock {
    inner: SessionLocks,
    path: PathBuf,
}

impl SessionLock {
    pub fn metadata(&self) -> io::Result<Meta> {
        self.inner.metadata(&self.path)
    }

    pub fn matches_path(&self, path: &Path) -> bool {
        self.inner.matches_path(path)
    }
}

pub fn try_session_lock(path: &Path) -> io::Result<SessionLock> {
    Ok(SessionLock { inner: try_session_locks(&[path.to_path_buf()])?, path: path.to_path_buf() })
}

/// Notifications are scheduling hints; callers must recheck their state.
pub struct DirectoryWatcher {
    inner: Option<imp::DirectoryWatcher>,
}

impl DirectoryWatcher {
    /// Register before checking the state to avoid missing changes before a wait.
    pub fn new(paths: &[&Path]) -> DirectoryWatcher {
        DirectoryWatcher { inner: imp::DirectoryWatcher::new(paths).ok() }
    }

    /// An interrupted wait returns promptly so the caller can check its signal state.
    pub fn wait(&mut self, timeout: Duration) -> bool {
        let started = Instant::now();
        if let Some(inner) = self.inner.as_mut() {
            match inner.wait(timeout) {
                Ok(changed) => return changed,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => return false,
                Err(_) => self.inner = None,
            }
        }
        std::thread::sleep(timeout.saturating_sub(started.elapsed()));
        false
    }
}

pub fn with_mode(opts: &mut OpenOptions, mode: u32) -> &mut OpenOptions {
    imp::with_mode(opts, mode)
}

pub fn set_file_mode(file: &File, mode: u32) -> io::Result<()> {
    imp::set_file_mode(file, mode)
}

pub fn set_path_mode(path: &Path, mode: u32) -> io::Result<()> {
    imp::set_path_mode(path, mode)
}

pub fn create_dir_with_mode(path: &Path, mode: u32) -> io::Result<()> {
    imp::create_dir_with_mode(path, mode)
}

pub fn owned_by_me(path: &Path) -> bool {
    imp::owned_by_me(path)
}

pub fn verify_private_dir(dir: &Path) -> Result<(), String> {
    imp::verify_private_dir(dir)
}

pub fn user_id() -> u32 {
    imp::user_id()
}

pub fn user_name() -> Option<String> {
    imp::user_name()
}

pub fn home_var() -> Option<OsString> {
    imp::home_var()
}

pub const HOME_VARS: &[&str] = imp::HOME_VARS;

pub const SYSROOT_DYLIB_DIR: &str = imp::SYSROOT_DYLIB_DIR;

pub fn rename(from: &Path, to: &Path) -> io::Result<()> {
    imp::rename(from, to)
}

pub fn open_directory_for_times(path: &Path) -> io::Result<File> {
    imp::open_directory_for_times(path)
}

pub fn set_mtime_now(path: &Path) -> io::Result<()> {
    imp::set_mtime_now(path)
}

pub fn reflink(src: &Path, dst: &Path) -> io::Result<()> {
    imp::reflink(src, dst)
}

pub fn copy_symlink(src: &Path, dst: &Path) -> io::Result<()> {
    imp::copy_symlink(src, dst)
}

pub fn link_tool(exe: &Path, link: &Path) -> io::Result<()> {
    imp::link_tool(exe, link)
}

pub fn first_extent(path: &Path) -> Option<u64> {
    imp::first_extent(path)
}

pub fn free_space(existing: &Path) -> io::Result<u64> {
    imp::free_space(existing)
}

pub fn canonicalize(path: &Path) -> io::Result<PathBuf> {
    imp::canonicalize(path)
}

pub fn find_executable_in(dir: &Path, name: &str) -> Option<PathBuf> {
    imp::find_executable_in(dir, name)
}

pub fn tool_name(argv0: &OsStr) -> String {
    imp::tool_name(argv0)
}

pub fn pid_alive(pid: u32) -> bool {
    imp::pid_alive(pid)
}

pub fn exec_replace(cmd: &mut Command) -> io::Error {
    imp::exec_replace(cmd)
}

pub fn load_average() -> Option<f64> {
    imp::load_average()
}

pub fn detach_command(cmd: &mut Command, inherit: &[&File]) {
    imp::detach_command(cmd, inherit)
}

/// When false, `detach_command` cannot pass files to the child, which must re-acquire their locks.
pub const INHERITS_DESCRIPTORS: bool = imp::INHERITS_DESCRIPTORS;

pub fn exit_code(status: ExitStatus) -> i32 {
    imp::exit_code(status)
}

#[derive(Debug)]
pub struct Pipe {
    file: File,
}

impl Pipe {
    fn adopt(owned: imp::Owned) -> Pipe {
        Pipe { file: imp::pipe_prepare(File::from(owned)) }
    }

    /// Reads never block: `WouldBlock` means no data yet; `Ok(0)` means EOF or an empty buffer.
    pub fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        imp::pipe_read(&mut self.file, buf)
    }
}

impl From<ChildStdout> for Pipe {
    fn from(pipe: ChildStdout) -> Pipe {
        Pipe::adopt(imp::Owned::from(pipe))
    }
}

impl From<ChildStderr> for Pipe {
    fn from(pipe: ChildStderr) -> Pipe {
        Pipe::adopt(imp::Owned::from(pipe))
    }
}

impl From<io::PipeReader> for Pipe {
    fn from(pipe: io::PipeReader) -> Pipe {
        Pipe::adopt(imp::Owned::from(pipe))
    }
}

/// Readiness includes EOF and failure, so callers must still read the pipe.
pub fn wait_readable(pipes: [Option<&Pipe>; 2], timeout: Duration) -> [bool; 2] {
    imp::wait_readable([pipes[0].map(|p| &p.file), pipes[1].map(|p| &p.file)], timeout)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StdStream {
    Stdout,
    Stderr,
}

pub fn ansi_terminal(s: StdStream) -> bool {
    imp::ansi_terminal(s)
}

pub fn terminal_columns() -> Option<u16> {
    imp::terminal_columns()
}

pub mod signal {
    use super::imp;

    pub type Signal = i32;

    pub const SIGINT: Signal = imp::signal::SIGINT;
    pub const SIGTERM: Signal = imp::signal::SIGTERM;
    pub const SIGHUP: Signal = imp::signal::SIGHUP;
    pub const SIGQUIT: Signal = imp::signal::SIGQUIT;

    /// Handlers may only do async-signal-safe work.
    pub type Handler = extern "C" fn(Signal);

    #[derive(PartialEq, Eq, Clone, Copy, Debug)]
    pub enum Disposition {
        Default,
        Ignore,
        Handler,
    }

    pub struct Saved(imp::signal::Saved);

    pub fn disposition(sig: Signal) -> Disposition {
        imp::signal::disposition(sig)
    }

    pub fn install(sig: Signal, h: Handler) -> Option<Saved> {
        imp::signal::install(sig, h).map(Saved)
    }

    pub fn set_default(sig: Signal) {
        imp::signal::set_default(sig)
    }

    pub fn restore(saved: Saved) {
        imp::signal::restore(saved.0)
    }

    pub fn raise(sig: Signal) {
        imp::signal::raise(sig)
    }

    /// Forwarding is safe from a handler and does nothing on Windows.
    pub fn forward(pid: u32, sig: Signal) {
        imp::signal::forward(pid, sig)
    }

    /// # Safety
    /// Use NUL-terminated `OsStr::as_encoded_bytes`; Unix deletion is async-signal-safe.
    pub unsafe fn unlink_from_handler(path_nul: &[u8]) {
        // SAFETY: the caller supplies the backend's NUL termination and encoding guarantees.
        unsafe { imp::signal::unlink_from_handler(path_nul) }
    }
}

/// Unix inheritance requires descriptors above stdio, which child redirection replaces.
pub fn inheritable_clear_of_stdio(files: &[&File]) -> bool {
    imp::inheritable_clear_of_stdio(files)
}

pub fn lower_thread_priority() {
    imp::lower_thread_priority()
}

pub fn fs_type_name(path: &Path) -> Option<String> {
    imp::fs_type_name(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("frust-sys-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn directory_watcher_falls_back_to_a_bounded_wait() {
        let mut watcher = DirectoryWatcher { inner: None };
        let timeout = Duration::from_millis(10);
        let started = Instant::now();
        assert!(!watcher.wait(timeout));
        assert!(started.elapsed() >= timeout);
    }

    #[test]
    fn directory_watcher_unavailable_path_uses_fallback() {
        let dir = scratch("watch-unavailable");
        let mut watcher = DirectoryWatcher::new(&[&dir.join("absent")]);
        assert!(watcher.inner.is_none());
        assert!(!watcher.wait(Duration::ZERO));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn meta_reports_identity_links_and_times() {
        let dir = scratch("meta");
        let file = dir.join("f");
        fs::write(&file, b"hello").unwrap();
        let meta = stat(&file).unwrap();
        assert_eq!(meta.len(), 5);
        assert_eq!(meta.nlink(), 1);
        assert!(meta.ino() != 0);
        let (secs, nsec) = meta.mtime();
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
        assert!((now - secs).abs() < 600);
        assert!((0..1_000_000_000).contains(&nsec));
        let open = File::open(&file).unwrap();
        let by_handle = fstat(&open).unwrap();
        assert_eq!((by_handle.dev(), by_handle.ino()), (meta.dev(), meta.ino()));
        fs::hard_link(&file, dir.join("g")).unwrap();
        assert_eq!(stat(&file).unwrap().nlink(), 2);
        // Windows identity requires stat, lstat or fstat rather than bare std metadata.
        if cfg!(unix) {
            assert_eq!(Meta::from(fs::metadata(&file).unwrap()).ino(), meta.ino());
        }
        assert!(stat(&dir.join("absent")).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn positional_io_leaves_the_cursor_alone() {
        let dir = scratch("positional");
        let path = dir.join("f");
        let file = File::options().read(true).write(true).create(true).truncate(true).open(&path).unwrap();
        write_all_at(&file, b"world", 6).unwrap();
        write_all_at(&file, b"hello ", 0).unwrap();
        let mut buf = [0u8; 5];
        read_exact_at(&file, &mut buf, 6).unwrap();
        assert_eq!(&buf, b"world");
        assert_eq!(fs::read(&path).unwrap(), b"hello world");
        assert!(read_exact_at(&file, &mut [0u8; 8], 8).is_err(), "reading past the end fails");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn locks_conflict_between_handles() {
        let dir = scratch("lock");
        let path = dir.join("f");
        fs::write(&path, b"").unwrap();
        let (a, b) = (File::open(&path).unwrap(), File::open(&path).unwrap());
        lock(&a, LockKind::Exclusive).unwrap();
        assert!(matches!(try_lock(&b, LockKind::Exclusive), Err(fs::TryLockError::WouldBlock)));
        assert!(matches!(try_lock(&b, LockKind::Shared), Err(fs::TryLockError::WouldBlock)));
        unlock(&a).unwrap();
        try_lock(&b, LockKind::Shared).unwrap();
        try_lock(&a, LockKind::Shared).unwrap();
        assert!(matches!(try_lock(&File::open(&path).unwrap(), LockKind::Exclusive), Err(fs::TryLockError::WouldBlock)));
        unlock(&a).unwrap();
        unlock(&b).unwrap();
        lock(&b, LockKind::Shared).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rename_replaces_an_existing_target() {
        let dir = scratch("rename");
        fs::write(dir.join("from"), b"new").unwrap();
        fs::write(dir.join("to"), b"old").unwrap();
        rename(&dir.join("from"), &dir.join("to")).unwrap();
        assert_eq!(fs::read(dir.join("to")).unwrap(), b"new");
        assert!(!dir.join("from").exists());
        assert!(rename(&dir.join("from"), &dir.join("other")).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_mtime_now_moves_files_and_directories_forward() {
        let dir = scratch("mtime");
        let file = dir.join("f");
        fs::write(&file, b"x").unwrap();
        let sub = dir.join("sub");
        fs::create_dir_all(&sub).unwrap();
        let old = SystemTime::now() - Duration::from_secs(7200);
        File::options().write(true).open(&file).unwrap().set_modified(old).unwrap();
        // Opening directories on Windows requires backup semantics, so only Unix ages this one.
        #[cfg(unix)]
        File::open(&sub).unwrap().set_modified(old).unwrap();
        for path in [&file, &sub] {
            set_mtime_now(path).unwrap();
            let age = fs::metadata(path).unwrap().modified().unwrap().elapsed().unwrap_or_default();
            assert!(age < Duration::from_secs(600), "{}", path.display());
        }
        assert!(set_mtime_now(&dir.join("absent")).is_err());
        assert!(!dir.join("absent").exists(), "touching a missing path creates nothing");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn pipe_reads_never_block_and_report_end_of_stream() {
        let (reader, mut writer) = io::pipe().unwrap();
        let mut pipe = Pipe::from(reader);
        let mut buf = [0u8; 16];
        assert_eq!(pipe.read(&mut buf).unwrap_err().kind(), io::ErrorKind::WouldBlock);
        assert_eq!(wait_readable([Some(&pipe), None], Duration::from_millis(10)), [false, false]);
        writer.write_all(b"hi").unwrap();
        assert_eq!(wait_readable([None, Some(&pipe)], Duration::from_secs(5)), [false, true]);
        assert_eq!(pipe.read(&mut buf).unwrap(), 2);
        assert_eq!(&buf[..2], b"hi");
        drop(writer);
        assert_eq!(wait_readable([Some(&pipe), None], Duration::from_secs(5)), [true, false]);
        assert_eq!(pipe.read(&mut buf).unwrap(), 0);
    }
}
