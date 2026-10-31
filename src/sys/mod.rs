//! Platform seam: everything that differs between Unix and Windows lives behind this module.

// The backends are the one place allowed to call the raw std functions that clippy.toml bans elsewhere.
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
use std::time::Duration;

/// File metadata plus the platform fields `std::fs::Metadata` does not expose portably.
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

    /// Device (volume) identifier.
    pub fn dev(&self) -> u64 {
        self.ext.dev(&self.inner)
    }

    /// File identifier within the device.
    pub fn ino(&self) -> u64 {
        self.ext.ino(&self.inner)
    }

    /// Number of hard links.
    pub fn nlink(&self) -> u64 {
        self.ext.nlink(&self.inner)
    }

    /// Bytes allocated on disk.
    pub fn allocated(&self) -> u64 {
        self.ext.allocated(&self.inner)
    }

    /// Modification time as (seconds, nanoseconds) since the Unix epoch.
    pub fn mtime(&self) -> (i64, i64) {
        self.ext.mtime(&self.inner)
    }

    /// Status change time as (seconds, nanoseconds) since the Unix epoch.
    pub fn ctime(&self) -> (i64, i64) {
        self.ext.ctime(&self.inner)
    }

    /// Access time as (seconds, nanoseconds) since the Unix epoch.
    pub fn atime(&self) -> (i64, i64) {
        self.ext.atime(&self.inner)
    }

    /// Raw permission and file type bits.
    pub fn mode(&self) -> u32 {
        self.ext.mode(&self.inner)
    }
}

/// Metadata of the target of `path`, following symlinks.
pub fn stat(path: &Path) -> io::Result<Meta> {
    imp::stat(path).map(Meta::pair)
}

/// Metadata of `path` itself, without following a final symlink.
pub fn lstat(path: &Path) -> io::Result<Meta> {
    imp::lstat(path).map(Meta::pair)
}

/// Metadata of an open file.
pub fn fstat(file: &File) -> io::Result<Meta> {
    imp::fstat(file).map(Meta::pair)
}

/// Fills `buf` from `offset` without moving the file cursor.
pub fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    imp::read_exact_at(file, buf, offset)
}

/// Writes all of `buf` at `offset` without moving the file cursor.
pub fn write_all_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    imp::write_all_at(file, buf, offset)
}

/// Advisory whole-file lock mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockKind {
    /// Any number of holders may share the lock.
    Shared,
    /// A single holder, excluding shared ones.
    Exclusive,
}

/// Takes the lock, blocking until it is available.
pub fn lock(file: &File, kind: LockKind) -> io::Result<()> {
    imp::lock(file, kind)
}

/// Takes the lock without blocking; `WouldBlock` when another handle holds a conflicting lock.
pub fn try_lock(file: &File, kind: LockKind) -> Result<(), fs::TryLockError> {
    imp::try_lock(file, kind)
}

/// Releases a lock taken on this handle.
pub fn unlock(file: &File) -> io::Result<()> {
    imp::unlock(file)
}

/// Sets the creation permission bits on `opts` (a no-op where the platform has none).
pub fn with_mode(opts: &mut OpenOptions, mode: u32) -> &mut OpenOptions {
    imp::with_mode(opts, mode)
}

/// Sets the permission bits of an open file.
pub fn set_file_mode(file: &File, mode: u32) -> io::Result<()> {
    imp::set_file_mode(file, mode)
}

/// Sets the permission bits of the file or directory at `path`.
pub fn set_path_mode(path: &Path, mode: u32) -> io::Result<()> {
    imp::set_path_mode(path, mode)
}

/// Creates one directory with exactly `mode`, regardless of the umask.
pub fn create_dir_with_mode(path: &Path, mode: u32) -> io::Result<()> {
    imp::create_dir_with_mode(path, mode)
}

/// Whether `path` itself (not its target) is owned by the effective user.
pub fn owned_by_me(path: &Path) -> bool {
    imp::owned_by_me(path)
}

/// Checks that `dir` is a directory only this user can use; `Err` carries the full refusal message.
pub fn verify_private_dir(dir: &Path) -> Result<(), String> {
    imp::verify_private_dir(dir)
}

/// Numeric id of the effective user.
pub fn user_id() -> u32 {
    imp::user_id()
}

/// Name of the effective user, if one can be found.
pub fn user_name() -> Option<String> {
    imp::user_name()
}

/// The raw home directory variable, unfiltered.
pub fn home_var() -> Option<OsString> {
    imp::home_var()
}

/// Environment variables that name the home directory, in order of preference.
pub const HOME_VARS: &[&str] = imp::HOME_VARS;

/// Directory inside a toolchain sysroot that holds its dynamic libraries.
pub const SYSROOT_DYLIB_DIR: &str = imp::SYSROOT_DYLIB_DIR;

/// Renames `from` to `to`, replacing an existing file.
pub fn rename(from: &Path, to: &Path) -> io::Result<()> {
    imp::rename(from, to)
}

/// Sets the modification time of a file or directory to now.
pub fn set_mtime_now(path: &Path) -> io::Result<()> {
    imp::set_mtime_now(path)
}

/// Copy-on-write clones `src` to the new path `dst`; `Err` (with `dst` removed) when unsupported.
pub fn reflink(src: &Path, dst: &Path) -> io::Result<()> {
    imp::reflink(src, dst)
}

/// Recreates the symlink `src` at `dst` with the same target.
pub fn copy_symlink(src: &Path, dst: &Path) -> io::Result<()> {
    imp::copy_symlink(src, dst)
}

/// Makes `link` an entry point that runs `exe` under the name `link`.
pub fn link_tool(exe: &Path, link: &Path) -> io::Result<()> {
    imp::link_tool(exe, link)
}

/// Physical position of the file's first extent, which identifies whole-file clones.
pub fn first_extent(path: &Path) -> Option<u64> {
    imp::first_extent(path)
}

/// Bytes available to this user on the volume holding the existing path.
pub fn free_space(existing: &Path) -> io::Result<u64> {
    imp::free_space(existing)
}

/// Resolves `path` to an absolute path with symlinks resolved.
pub fn canonicalize(path: &Path) -> io::Result<PathBuf> {
    imp::canonicalize(path)
}

/// The runnable file `name` inside `dir`, if there is one.
pub fn find_executable_in(dir: &Path, name: &str) -> Option<PathBuf> {
    imp::find_executable_in(dir, name)
}

/// The tool name an invocation path stands for: its file name.
pub fn tool_name(argv0: &OsStr) -> String {
    imp::tool_name(argv0)
}

/// Whether a process with this id exists.
pub fn pid_alive(pid: u32) -> bool {
    imp::pid_alive(pid)
}

/// Replaces this process with `cmd` where the platform can; returns only the failure.
pub fn exec_replace(cmd: &mut Command) -> io::Error {
    imp::exec_replace(cmd)
}

/// The one-minute load average where the platform reports one.
pub fn load_average() -> Option<f64> {
    imp::load_average()
}

/// Configures `cmd` to start detached from this process's session or console with null standard streams; `inherit` files stay open in the child when `INHERITS_DESCRIPTORS` is true.
pub fn detach_command(cmd: &mut Command, inherit: &[&File]) {
    imp::detach_command(cmd, inherit)
}

/// Whether `detach_command` hands its `inherit` files to the child; callers re-acquire their locks in the child when it does not.
pub const INHERITS_DESCRIPTORS: bool = imp::INHERITS_DESCRIPTORS;

/// Shell-style exit code: the code, or 128 plus the signal.
pub fn exit_code(status: ExitStatus) -> i32 {
    imp::exit_code(status)
}

/// Read end of a child's output pipe that never blocks.
#[derive(Debug)]
pub struct Pipe {
    file: File,
}

impl Pipe {
    fn adopt(owned: imp::Owned) -> Pipe {
        Pipe { file: imp::pipe_prepare(File::from(owned)) }
    }

    /// Reads available bytes: `WouldBlock` when there are none yet, `Ok(0)` at end of stream.
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

/// Waits up to `timeout` for either pipe to have data, reach end of stream or fail.
pub fn wait_readable(pipes: [Option<&Pipe>; 2], timeout: Duration) -> [bool; 2] {
    imp::wait_readable([pipes[0].map(|p| &p.file), pipes[1].map(|p| &p.file)], timeout)
}

/// A standard output stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StdStream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

/// Whether the stream is a terminal that interprets ANSI escape sequences.
pub fn ansi_terminal(s: StdStream) -> bool {
    imp::ansi_terminal(s)
}

/// Width of the terminal attached to stderr, if there is one.
pub fn terminal_columns() -> Option<u16> {
    imp::terminal_columns()
}

/// Signal numbers, handlers and dispositions.
pub mod signal {
    use super::imp;

    /// A signal number.
    pub type Signal = i32;

    /// Interrupt (Ctrl-C).
    pub const SIGINT: Signal = imp::signal::SIGINT;
    /// Termination request.
    pub const SIGTERM: Signal = imp::signal::SIGTERM;
    /// Terminal hang-up.
    pub const SIGHUP: Signal = imp::signal::SIGHUP;
    /// Quit request.
    pub const SIGQUIT: Signal = imp::signal::SIGQUIT;

    /// A signal handler; it may only do async-signal-safe work.
    pub type Handler = extern "C" fn(Signal);

    /// What happens when a signal arrives.
    #[derive(PartialEq, Eq, Clone, Copy, Debug)]
    pub enum Disposition {
        /// The system default action.
        Default,
        /// The signal is ignored.
        Ignore,
        /// A handler function runs.
        Handler,
    }

    /// A previously installed disposition, for [`restore`].
    pub struct Saved(imp::signal::Saved);

    /// Current disposition of `sig`; a failed query reports `Ignore`.
    pub fn disposition(sig: Signal) -> Disposition {
        imp::signal::disposition(sig)
    }

    /// Installs `h` for `sig` with restartable syscalls; returns the previous disposition, or `None` on failure.
    pub fn install(sig: Signal, h: Handler) -> Option<Saved> {
        imp::signal::install(sig, h).map(Saved)
    }

    /// Resets `sig` to the system default action.
    pub fn set_default(sig: Signal) {
        imp::signal::set_default(sig)
    }

    /// Reinstates a disposition returned by [`install`].
    pub fn restore(saved: Saved) {
        imp::signal::restore(saved.0)
    }

    /// Sends `sig` to the calling process.
    pub fn raise(sig: Signal) {
        imp::signal::raise(sig)
    }

    /// Sends `sig` to process `pid`; safe to call from a handler.
    pub fn forward(pid: u32, sig: Signal) {
        imp::signal::forward(pid, sig)
    }

    /// Deletes the file at the NUL-terminated `path_nul`, using only async-signal-safe calls.
    ///
    /// # Safety
    /// `path_nul` must end with a NUL byte.
    pub unsafe fn unlink_from_handler(path_nul: &[u8]) {
        // SAFETY: forwarded caller guarantee.
        unsafe { imp::signal::unlink_from_handler(path_nul) }
    }
}



// wrapper
/// Whether `files` can safely be passed to `detach_command`: on Unix none may sit on a standard stream's descriptor, which the child's redirected stdio would replace.
pub fn inheritable_clear_of_stdio(files: &[&File]) -> bool {
    imp::inheritable_clear_of_stdio(files)
}

// compact and hashing
/// Lowers the CPU and I/O priority of the calling thread for as long as it lives; best effort.
pub fn lower_thread_priority() {
    imp::lower_thread_priority()
}

/// Lowercase name of the filesystem holding `path`; `None` when it cannot be determined or the volume is remote.
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
        // Only unix can read identity from a bare std Metadata; on Windows callers must use stat, lstat or fstat.
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
