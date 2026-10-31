use std::ffi::{CStr, CString, OsStr};
use std::fs::{self, File, Metadata, OpenOptions, Permissions};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::os::fd::FromRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::time::Duration;

use super::{LockKind, StdStream};

pub type Owned = std::os::fd::OwnedFd;

#[cfg(target_os = "linux")]
pub struct DirectoryWatcher {
    file: File,
}

#[cfg(target_os = "linux")]
impl DirectoryWatcher {
    pub fn new(paths: &[&Path]) -> io::Result<DirectoryWatcher> {
        // SAFETY: inotify_init1 has no pointer arguments and returns a new descriptor on success.
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: this is the newly created descriptor, now owned only by `file`.
        let file = unsafe { File::from_raw_fd(fd) };
        let mask = libc::IN_CREATE | libc::IN_DELETE | libc::IN_MOVED_FROM | libc::IN_MOVED_TO
            | libc::IN_MODIFY | libc::IN_ATTRIB | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF | libc::IN_ONLYDIR;
        for path in paths {
            let path = cstr(path)?;
            // SAFETY: the descriptor is open and `path` is a valid NUL-terminated path.
            if unsafe { libc::inotify_add_watch(fd, path.as_ptr(), mask) } < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(DirectoryWatcher { file })
    }

    pub fn wait(&mut self, timeout: Duration) -> io::Result<bool> {
        let mut fd = libc::pollfd { fd: self.file.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        let millis = timeout.as_millis().saturating_add(u128::from(!timeout.subsec_nanos().is_multiple_of(1_000_000)));
        // SAFETY: `fd` is one live pollfd and the timeout is clamped to poll's range.
        let rc = unsafe { libc::poll(&mut fd, 1, millis.min(libc::c_int::MAX as u128) as libc::c_int) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        if rc == 0 {
            return Ok(false);
        }
        if fd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Err(io::Error::other("directory watcher is unavailable"));
        }
        // Overflow is a wakeup hint; bounded draining keeps signal checks responsive.
        let mut buf = [0u8; 8192];
        for _ in 0..16 {
            match self.file.read(&mut buf) {
                Ok(0) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
                Ok(_) => {}
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) => return Err(err),
            }
        }
        Ok(true)
    }
}

#[cfg(target_os = "macos")]
pub struct DirectoryWatcher {
    queue: Owned,
    _directories: Vec<File>,
}

#[cfg(target_os = "macos")]
impl DirectoryWatcher {
    pub fn new(paths: &[&Path]) -> io::Result<DirectoryWatcher> {
        // SAFETY: kqueue has no arguments and returns a new descriptor on success.
        let fd = unsafe { libc::kqueue() };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: this descriptor was just created and has no other owner.
        let queue = unsafe { Owned::from_raw_fd(fd) };
        // SAFETY: fcntl sets descriptor flags on the open kqueue descriptor.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut directories = Vec::with_capacity(paths.len());
        for path in paths {
            let file = File::open(path)?;
            let change = libc::kevent {
                ident: file.as_raw_fd() as libc::uintptr_t,
                filter: libc::EVFILT_VNODE,
                flags: libc::EV_ADD | libc::EV_CLEAR,
                fflags: libc::NOTE_WRITE | libc::NOTE_DELETE | libc::NOTE_RENAME | libc::NOTE_ATTRIB | libc::NOTE_REVOKE,
                data: 0,
                udata: std::ptr::null_mut(),
            };
            // SAFETY: `change` is initialized, both descriptors are open, and no output buffer is requested.
            if unsafe { libc::kevent(fd, &change, 1, std::ptr::null_mut(), 0, std::ptr::null()) } < 0 {
                return Err(io::Error::last_os_error());
            }
            directories.push(file);
        }
        Ok(DirectoryWatcher { queue, _directories: directories })
    }

    pub fn wait(&mut self, timeout: Duration) -> io::Result<bool> {
        // SAFETY: kevent is plain data and zero initializes valid output buffers.
        let mut events: [libc::kevent; 32] = unsafe { std::mem::zeroed() };
        let mut timeout = libc::timespec {
            tv_sec: timeout.as_secs().min(libc::time_t::MAX as u64) as libc::time_t,
            tv_nsec: timeout.subsec_nanos() as _,
        };
        let mut changed = false;
        for _ in 0..16 {
            // SAFETY: the kqueue is open, events is a writable array, and timeout is initialized.
            let rc = unsafe {
                libc::kevent(self.queue.as_raw_fd(), std::ptr::null(), 0, events.as_mut_ptr(), events.len() as _, &timeout)
            };
            if rc < 0 {
                return Err(io::Error::last_os_error());
            }
            if events[..rc as usize].iter().any(|event| event.flags & libc::EV_ERROR != 0) {
                return Err(io::Error::other("directory watcher is unavailable"));
            }
            if rc == 0 {
                break;
            }
            changed = true;
            // EV_CLEAR coalesces changes per directory; drain other ready directories without waiting again.
            timeout = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        }
        Ok(changed)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub struct DirectoryWatcher;

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
impl DirectoryWatcher {
    pub fn new(_paths: &[&Path]) -> io::Result<DirectoryWatcher> {
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }

    pub fn wait(&mut self, _timeout: Duration) -> io::Result<bool> {
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }
}

pub const HOME_VARS: &[&str] = &["HOME"];
pub const SYSROOT_DYLIB_DIR: &str = "lib";

#[derive(Debug, Clone, Copy)]
pub struct Ext;

impl Ext {
    pub fn from_std(_meta: &Metadata) -> Ext {
        Ext
    }

    pub fn dev(&self, meta: &Metadata) -> u64 {
        meta.dev()
    }

    pub fn ino(&self, meta: &Metadata) -> u64 {
        meta.ino()
    }

    pub fn nlink(&self, meta: &Metadata) -> u64 {
        meta.nlink()
    }

    pub fn allocated(&self, meta: &Metadata) -> u64 {
        meta.blocks() * 512
    }

    pub fn mtime(&self, meta: &Metadata) -> (i64, i64) {
        (meta.mtime(), meta.mtime_nsec())
    }

    pub fn ctime(&self, meta: &Metadata) -> (i64, i64) {
        (meta.ctime(), meta.ctime_nsec())
    }

    pub fn atime(&self, meta: &Metadata) -> (i64, i64) {
        (meta.atime(), meta.atime_nsec())
    }

    pub fn mode(&self, meta: &Metadata) -> u32 {
        meta.mode()
    }
}

pub fn stat(path: &Path) -> io::Result<(Metadata, Ext)> {
    fs::metadata(path).map(|meta| (meta, Ext))
}

pub fn lstat(path: &Path) -> io::Result<(Metadata, Ext)> {
    fs::symlink_metadata(path).map(|meta| (meta, Ext))
}

pub fn fstat(file: &File) -> io::Result<(Metadata, Ext)> {
    file.metadata().map(|meta| (meta, Ext))
}

pub fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    file.read_exact_at(buf, offset)
}

pub fn write_all_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    file.write_all_at(buf, offset)
}

pub fn lock(file: &File, kind: LockKind) -> io::Result<()> {
    match kind {
        LockKind::Shared => file.lock_shared(),
        LockKind::Exclusive => file.lock(),
    }
}

pub fn try_lock(file: &File, kind: LockKind) -> Result<(), fs::TryLockError> {
    match kind {
        LockKind::Shared => file.try_lock_shared(),
        LockKind::Exclusive => file.try_lock(),
    }
}

pub fn unlock(file: &File) -> io::Result<()> {
    file.unlock()
}

pub fn open_session_lock(path: &Path) -> io::Result<File> {
    OpenOptions::new().read(true).write(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path)
}

fn session_record_lock(file: &File, kind: libc::c_short) -> io::Result<()> {
    // SAFETY: flock is plain data; zero length locks the whole file, including future bytes.
    let mut record: libc::flock = unsafe { std::mem::zeroed() };
    record.l_type = kind;
    record.l_whence = libc::SEEK_SET as _;
    // SAFETY: the descriptor remains open and record is initialized for F_SETLK.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &record) } == 0 {
        Ok(())
    } else {
        let error = io::Error::last_os_error();
        if matches!(error.raw_os_error(), Some(libc::EACCES | libc::EAGAIN)) {
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        } else {
            Err(error)
        }
    }
}

pub fn try_lock_session(file: &File) -> io::Result<()> {
    file.try_lock().map_err(io::Error::from)?;
    if let Err(error) = session_record_lock(file, libc::F_WRLCK as _) {
        let _ = file.unlock();
        return Err(error);
    }
    Ok(())
}

pub fn unlock_session(file: &File) {
    let _ = session_record_lock(file, libc::F_UNLCK as _);
    let _ = file.unlock();
}

pub fn with_mode(opts: &mut OpenOptions, mode: u32) -> &mut OpenOptions {
    opts.mode(mode)
}

pub fn set_file_mode(file: &File, mode: u32) -> io::Result<()> {
    file.set_permissions(Permissions::from_mode(mode))
}

pub fn set_path_mode(path: &Path, mode: u32) -> io::Result<()> {
    fs::set_permissions(path, Permissions::from_mode(mode))
}

pub fn create_dir_with_mode(path: &Path, mode: u32) -> io::Result<()> {
    fs::DirBuilder::new().mode(mode).create(path)?;
    // The umask may have stripped bits from the creation mode.
    let _ = set_path_mode(path, mode);
    Ok(())
}

fn euid() -> u32 {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() }
}

pub fn owned_by_me(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.uid() == euid())
}

pub fn verify_private_dir(dir: &Path) -> Result<(), String> {
    let uid = euid();
    let meta = fs::symlink_metadata(dir).map_err(|e| format!("cannot inspect {}: {e}", dir.display()))?;
    let refuse = |why: &str| {
        Err(format!(
            "refusing to use {} as frust's state directory: {why}; set FRUST_HOME or HOME to a private location",
            dir.display()
        ))
    };
    if !meta.file_type().is_dir() {
        return refuse("it is not a directory");
    }
    if meta.uid() != uid {
        return refuse(&format!("it is owned by uid {}, not {uid}", meta.uid()));
    }
    let mode = meta.permissions().mode() & 0o7777;
    if mode != 0o700 {
        return refuse(&format!("its mode is {mode:o}, not 700"));
    }
    Ok(())
}

pub fn user_id() -> u32 {
    euid()
}

pub fn user_name() -> Option<String> {
    passwd_name(euid())
        .or_else(|| std::env::var("USER").ok().filter(|u| !u.is_empty()))
        .or_else(|| std::env::var("LOGNAME").ok().filter(|u| !u.is_empty()))
}

fn passwd_name(uid: libc::uid_t) -> Option<String> {
    let mut buf = vec![0 as libc::c_char; 4096];
    // SAFETY: getpwuid_r writes only into `pwd` and `buf` (sizes passed), and `result` is null or points at `pwd`.
    unsafe {
        let mut pwd: libc::passwd = std::mem::zeroed();
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let rc = libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr(), buf.len(), &mut result);
        if rc != 0 || result.is_null() || pwd.pw_name.is_null() {
            return None;
        }
        let name = CStr::from_ptr(pwd.pw_name).to_string_lossy().into_owned();
        (!name.is_empty()).then_some(name)
    }
}

pub fn home_var() -> Option<std::ffi::OsString> {
    std::env::var_os("HOME")
}

pub fn rename(from: &Path, to: &Path) -> io::Result<()> {
    fs::rename(from, to)
}

fn cstr(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
}

pub fn open_directory_for_times(path: &Path) -> io::Result<File> {
    File::open(path)
}

pub fn set_mtime_now(path: &Path) -> io::Result<()> {
    let c = cstr(path)?;
    // SAFETY: `c` is a valid NUL-terminated path; null times selects the filesystem's current time.
    let rc = unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), std::ptr::null(), 0) };
    if rc == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

#[cfg(target_os = "macos")]
pub fn reflink(src: &Path, dst: &Path) -> io::Result<()> {
    let (s, d) = (cstr(src)?, cstr(dst)?);
    // SAFETY: both pointers are valid NUL-terminated strings for the call duration.
    let rc = unsafe { libc::clonefile(s.as_ptr(), d.as_ptr(), 0) };
    if rc == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

#[cfg(target_os = "linux")]
pub fn reflink(src: &Path, dst: &Path) -> io::Result<()> {
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
pub fn reflink(_src: &Path, _dst: &Path) -> io::Result<()> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

pub fn copy_symlink(src: &Path, dst: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(fs::read_link(src)?, dst)
}

pub fn link_tool(exe: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(exe, link)
}

#[cfg(target_os = "macos")]
pub fn first_extent(path: &Path) -> Option<u64> {
    let file = File::open(path).ok()?;
    let mut info = libc::log2phys { l2p_flags: 0, l2p_contigbytes: 1, l2p_devoffset: 0 };
    // SAFETY: `info` is a valid log2phys struct and the descriptor is open.
    let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_LOG2PHYS_EXT, &mut info) };
    (rc == 0 && info.l2p_devoffset > 0).then_some(info.l2p_devoffset as u64)
}

#[cfg(not(target_os = "macos"))]
pub fn first_extent(_path: &Path) -> Option<u64> {
    None
}

pub fn free_space(existing: &Path) -> io::Result<u64> {
    let c = cstr(existing)?;
    // SAFETY: statvfs is plain data and all-zero is a valid value.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid C string and `stat` is a valid out pointer.
    let rc = unsafe { libc::statvfs(c.as_ptr(), &mut stat) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(stat.f_bavail as u64 * stat.f_frsize as u64)
}

pub fn canonicalize(path: &Path) -> io::Result<PathBuf> {
    fs::canonicalize(path)
}

pub fn find_executable_in(dir: &Path, name: &str) -> Option<PathBuf> {
    let candidate = dir.join(name);
    let runnable = candidate.metadata().map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false);
    runnable.then_some(candidate)
}

pub fn tool_name(argv0: &OsStr) -> String {
    Path::new(argv0).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

pub fn pid_alive(pid: u32) -> bool {
    // SAFETY: kill with signal 0 only checks for existence.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    rc == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

pub fn exec_replace(cmd: &mut Command) -> io::Error {
    cmd.exec()
}

pub fn load_average() -> Option<f64> {
    let mut load = [0f64; 3];
    // SAFETY: the buffer holds the three samples requested.
    let n = unsafe { libc::getloadavg(load.as_mut_ptr(), 3) };
    (n >= 1).then_some(load[0])
}

pub const INHERITS_DESCRIPTORS: bool = true;

pub fn detach_command(cmd: &mut Command, inherit: &[&File]) {
    use std::process::Stdio;
    let keep: Vec<i32> = inherit.iter().map(|f| f.as_raw_fd()).collect();
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    // SAFETY: the closure only calls async-signal-safe functions (setsid, fcntl) that take no pointers, and allocates nothing.
    unsafe {
        cmd.pre_exec(move || {
            libc::setsid();
            for &fd in &keep {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags == -1 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) == -1 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
}

pub fn exit_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or_else(|| status.signal().map(|s| 128 + s).unwrap_or(1))
}

pub fn pipe_prepare(file: File) -> File {
    // SAFETY: fcntl on a descriptor we own; on failure it stays blocking, which is fine as callers read only after readiness is reported.
    unsafe {
        let flags = libc::fcntl(file.as_raw_fd(), libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
    file
}

pub fn pipe_read(file: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    file.read(buf)
}

pub fn wait_readable(pipes: [Option<&File>; 2], timeout: Duration) -> [bool; 2] {
    let mut fds = Vec::with_capacity(2);
    let mut index = Vec::with_capacity(2);
    for (i, pipe) in pipes.iter().enumerate() {
        if let Some(file) = pipe {
            fds.push(libc::pollfd { fd: file.as_raw_fd(), events: libc::POLLIN, revents: 0 });
            index.push(i);
        }
    }
    let mut ready = [false; 2];
    if fds.is_empty() {
        return ready;
    }
    // SAFETY: `fds` is a valid array of `fds.len()` initialised pollfd structs.
    let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout.as_millis() as libc::c_int) };
    if rc > 0 {
        for (fd, &i) in fds.iter().zip(&index) {
            ready[i] = fd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0;
        }
    }
    ready
}

fn std_fd(s: StdStream) -> libc::c_int {
    match s {
        StdStream::Stdout => libc::STDOUT_FILENO,
        StdStream::Stderr => libc::STDERR_FILENO,
    }
}

pub fn ansi_terminal(s: StdStream) -> bool {
    // SAFETY: isatty only inspects the descriptor.
    unsafe { libc::isatty(std_fd(s)) == 1 }
}

pub fn terminal_columns() -> Option<u16> {
    let mut ws = libc::winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: TIOCGWINSZ only writes a `winsize` through the pointer we pass.
    let rc = unsafe { libc::ioctl(libc::STDERR_FILENO, libc::TIOCGWINSZ as _, &mut ws as *mut libc::winsize) };
    (rc == 0 && ws.ws_col > 0).then_some(ws.ws_col)
}

pub mod signal {
    use crate::sys::signal::{Disposition, Handler, Signal};

    pub const SIGINT: Signal = libc::SIGINT;
    pub const SIGTERM: Signal = libc::SIGTERM;
    pub const SIGHUP: Signal = libc::SIGHUP;
    pub const SIGQUIT: Signal = libc::SIGQUIT;

    pub struct Saved {
        sig: Signal,
        action: libc::sigaction,
    }

    // SAFETY: `sigaction` is plain data (handler address, mask, flags).
    unsafe impl Send for Saved {}

    pub fn disposition(sig: Signal) -> Disposition {
        // SAFETY: querying with a null new action and an initialised out struct.
        let queried = unsafe {
            let mut old: libc::sigaction = std::mem::zeroed();
            (libc::sigaction(sig, std::ptr::null(), &mut old) == 0).then_some(old.sa_sigaction)
        };
        match queried {
            None | Some(libc::SIG_IGN) => Disposition::Ignore,
            Some(libc::SIG_DFL) => Disposition::Default,
            Some(_) => Disposition::Handler,
        }
    }

    pub fn install(sig: Signal, handler: Handler) -> Option<Saved> {
        // SAFETY: installing a plain handler through sigaction with initialised structs.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = handler as *const () as usize;
            libc::sigemptyset(&mut action.sa_mask);
            action.sa_flags = libc::SA_RESTART;
            let mut old: libc::sigaction = std::mem::zeroed();
            (libc::sigaction(sig, &action, &mut old) == 0).then_some(Saved { sig, action: old })
        }
    }

    pub fn set_default(sig: Signal) {
        // SAFETY: restoring the default action through sigaction with an initialised struct.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = libc::SIG_DFL;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(sig, &action, std::ptr::null_mut());
        }
    }

    pub fn restore(saved: Saved) {
        // SAFETY: restoring a disposition previously returned by sigaction.
        unsafe { libc::sigaction(saved.sig, &saved.action, std::ptr::null_mut()) };
    }

    pub fn raise(sig: Signal) {
        // SAFETY: raise has no preconditions.
        unsafe { libc::raise(sig) };
    }

    pub fn forward(pid: u32, sig: Signal) {
        if pid == 0 || pid > i32::MAX as u32 {
            return;
        }
        // SAFETY: kill is async-signal-safe.
        unsafe { libc::kill(pid as libc::pid_t, sig) };
    }

    pub unsafe fn unlink_from_handler(path_nul: &[u8]) {
        // SAFETY: the caller guarantees a NUL-terminated path; unlink is async-signal-safe.
        unsafe { libc::unlink(path_nul.as_ptr().cast()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::{self, signal as sig};
    use std::io::Write;
    use std::sync::atomic::{AtomicI32, Ordering};

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("frust-sys-unix-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn directory_watcher_keeps_pending_changes_and_drains_them() {
        let dir = scratch("watch-pending");
        let mut watcher = DirectoryWatcher::new(&[&dir]).unwrap();
        fs::write(dir.join("first"), b"one").unwrap();
        fs::write(dir.join("second"), b"two").unwrap();
        assert!(watcher.wait(Duration::ZERO).unwrap(), "a change before waiting must stay pending");
        assert!(!watcher.wait(Duration::ZERO).unwrap(), "one wait coalesces the pending changes");
        drop(watcher);
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn directory_watcher_times_out_without_changes() {
        let dir = scratch("watch-timeout");
        let mut watcher = DirectoryWatcher::new(&[&dir]).unwrap();
        let timeout = Duration::from_millis(10);
        let started = std::time::Instant::now();
        assert!(!watcher.wait(timeout).unwrap());
        assert!(started.elapsed() >= timeout);
        drop(watcher);
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn directory_watcher_wakes_when_another_thread_changes_the_directory() {
        let dir = scratch("watch-wakeup");
        let mut watcher = DirectoryWatcher::new(&[&dir]).unwrap();
        let writer_dir = dir.clone();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            fs::write(writer_dir.join("changed"), b"ready").unwrap();
        });
        assert!(watcher.wait(Duration::from_secs(2)).unwrap(), "a change must wake the blocked wait");
        writer.join().unwrap();
        drop(watcher);
        fs::remove_dir_all(dir).unwrap();
    }

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    #[test]
    fn lstat_sees_a_symlink_and_stat_follows_it() {
        let dir = scratch("lstat");
        fs::write(dir.join("target"), vec![1u8; 65536]).unwrap();
        std::os::unix::fs::symlink(dir.join("target"), dir.join("link")).unwrap();
        assert!(sys::lstat(&dir.join("link")).unwrap().file_type().is_symlink());
        let followed = sys::stat(&dir.join("link")).unwrap();
        assert!(followed.is_file());
        assert_eq!(followed.ino(), sys::stat(&dir.join("target")).unwrap().ino());
        assert_eq!(followed.mode() & 0o170000, 0o100000);
        assert!(followed.allocated() > 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn modes_are_honoured_despite_the_umask() {
        let dir = scratch("mode");
        let file = dir.join("f");
        let mut opts = File::options();
        sys::with_mode(opts.write(true).create_new(true), 0o600);
        let handle = opts.open(&file).unwrap();
        assert_eq!(mode_of(&file), 0o600);
        sys::set_file_mode(&handle, 0o640).unwrap();
        assert_eq!(mode_of(&file), 0o640);
        sys::set_path_mode(&file, 0o604).unwrap();
        assert_eq!(mode_of(&file), 0o604);
        let sub = dir.join("sub");
        sys::create_dir_with_mode(&sub, 0o770).unwrap();
        assert_eq!(mode_of(&sub), 0o770);
        let again = sys::create_dir_with_mode(&sub, 0o770).unwrap_err();
        assert_eq!(again.kind(), io::ErrorKind::AlreadyExists);
        assert!(sys::create_dir_with_mode(&dir.join("a").join("b"), 0o700).is_err(), "only a single directory is created");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn private_dir_verification_names_the_problem() {
        let dir = scratch("private");
        let private = dir.join("p");
        sys::create_dir_with_mode(&private, 0o700).unwrap();
        assert_eq!(sys::verify_private_dir(&private), Ok(()));
        assert!(sys::owned_by_me(&private));
        sys::set_path_mode(&private, 0o755).unwrap();
        let err = sys::verify_private_dir(&private).unwrap_err();
        assert!(err.contains("its mode is 755, not 700"), "{err}");
        assert!(err.starts_with("refusing to use "), "{err}");
        fs::write(dir.join("file"), b"x").unwrap();
        assert!(sys::verify_private_dir(&dir.join("file")).unwrap_err().contains("it is not a directory"));
        assert!(sys::verify_private_dir(&dir.join("absent")).unwrap_err().starts_with("cannot inspect "));
        assert!(!sys::owned_by_me(&dir.join("absent")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn identity_matches_the_process() {
        assert_eq!(sys::user_id(), euid());
        assert!(sys::user_name().is_none_or(|n| !n.is_empty()));
        assert_eq!(sys::home_var(), std::env::var_os("HOME"));
        assert_eq!(sys::HOME_VARS, ["HOME"]);
        assert!(sys::pid_alive(std::process::id()));
    }

    #[test]
    fn executables_and_tool_names() {
        let dir = scratch("exec");
        let tool = dir.join("tool");
        fs::write(&tool, b"#!/bin/sh\n").unwrap();
        sys::set_path_mode(&tool, 0o644).unwrap();
        assert_eq!(sys::find_executable_in(&dir, "tool"), None);
        sys::set_path_mode(&tool, 0o755).unwrap();
        assert_eq!(sys::find_executable_in(&dir, "tool"), Some(tool.clone()));
        assert_eq!(sys::find_executable_in(&dir, "absent"), None);
        assert_eq!(sys::find_executable_in(dir.parent().unwrap(), dir.file_name().unwrap().to_str().unwrap()), None, "a directory is not a tool");
        assert_eq!(sys::tool_name(OsStr::new("/usr/local/bin/fcargo")), "fcargo");
        assert_eq!(sys::tool_name(OsStr::new("frust")), "frust");
        assert_eq!(sys::tool_name(OsStr::new("")), "");
        assert_eq!(sys::canonicalize(&dir.join(".").join("tool")).unwrap(), fs::canonicalize(&tool).unwrap());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn links_and_symlink_copies() {
        let dir = scratch("links");
        let exe = dir.join("frust");
        fs::write(&exe, b"x").unwrap();
        sys::link_tool(&exe, &dir.join("fcargo")).unwrap();
        assert_eq!(fs::read_link(dir.join("fcargo")).unwrap(), exe);
        sys::copy_symlink(&dir.join("fcargo"), &dir.join("copy")).unwrap();
        assert_eq!(fs::read_link(dir.join("copy")).unwrap(), exe);
        assert!(sys::link_tool(&exe, &dir.join("fcargo")).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reflink_creates_a_clone_or_leaves_nothing() {
        let dir = scratch("reflink");
        let src = dir.join("src");
        let dst = dir.join("dst");
        fs::write(&src, b"clone me").unwrap();
        match sys::reflink(&src, &dst) {
            Ok(()) => assert_eq!(fs::read(&dst).unwrap(), b"clone me"),
            Err(_) => assert!(!dst.exists(), "a failed clone leaves no destination"),
        }
        assert!(sys::reflink(&dir.join("absent"), &dir.join("other")).is_err());
        let _ = sys::first_extent(&src);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn free_space_reports_the_volume() {
        let dir = scratch("space");
        assert!(sys::free_space(&dir).unwrap() > 0);
        assert!(sys::free_space(&dir.join("absent")).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn exit_code_reports_code_or_128_plus_signal() {
        let exited = Command::new("sh").args(["-c", "exit 3"]).status().unwrap();
        assert_eq!(sys::exit_code(exited), 3);
        let killed = Command::new("sh").args(["-c", "kill -9 $$"]).status().unwrap();
        assert_eq!(sys::exit_code(killed), 137);
    }

    #[test]
    fn exec_replace_returns_only_on_failure() {
        let err = sys::exec_replace(&mut Command::new("/nonexistent/frust-sys-test"));
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn child_pipes_are_nonblocking_and_report_readiness() {
        let mut child = Command::new("sh")
            .args(["-c", "read line; echo got-$line; echo err >&2"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut out = sys::Pipe::from(child.stdout.take().unwrap());
        let mut err = sys::Pipe::from(child.stderr.take().unwrap());
        let mut buf = [0u8; 64];
        assert_eq!(out.read(&mut buf).unwrap_err().kind(), io::ErrorKind::WouldBlock);
        assert_eq!(sys::wait_readable([Some(&out), Some(&err)], Duration::from_millis(10)), [false, false]);
        child.stdin.take().unwrap().write_all(b"x\n").unwrap();
        let mut seen = Vec::new();
        while seen.len() < 6 {
            if sys::wait_readable([Some(&out), None], Duration::from_secs(5))[0] {
                match out.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => seen.extend_from_slice(&buf[..n]),
                    Err(e) => assert_eq!(e.kind(), io::ErrorKind::WouldBlock),
                }
            }
        }
        assert_eq!(seen, b"got-x\n");
        child.wait().unwrap();
        assert!(sys::wait_readable([None, Some(&err)], Duration::from_secs(5))[1]);
        let mut collected = Vec::new();
        loop {
            match err.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => collected.extend_from_slice(&buf[..n]),
                Err(e) => assert_eq!(e.kind(), io::ErrorKind::WouldBlock),
            }
        }
        assert_eq!(collected, b"err\n");
        assert_eq!(sys::wait_readable([None, None], Duration::from_millis(1)), [false, false]);
    }

    static SEEN: AtomicI32 = AtomicI32::new(0);

    extern "C" fn note(signal: i32) {
        SEEN.store(signal, Ordering::Relaxed);
    }

    #[test]
    fn signal_dispositions_round_trip_and_handlers_run() {
        let before = sig::disposition(sig::SIGHUP);
        let saved = sig::install(sig::SIGHUP, note).expect("install");
        assert_eq!(sig::disposition(sig::SIGHUP), sig::Disposition::Handler);
        sig::raise(sig::SIGHUP);
        assert_eq!(SEEN.load(Ordering::Relaxed), sig::SIGHUP);
        sig::set_default(sig::SIGHUP);
        assert_eq!(sig::disposition(sig::SIGHUP), sig::Disposition::Default);
        sig::restore(saved);
        assert_eq!(sig::disposition(sig::SIGHUP), before);
        assert!(sig::install(-1, note).is_none());
        assert_eq!(sig::disposition(-1), sig::Disposition::Ignore);
    }

    #[test]
    fn forward_delivers_a_signal_to_a_child_and_ignores_bad_pids() {
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        sig::forward(0, sig::SIGTERM);
        sig::forward(u32::MAX, sig::SIGTERM);
        assert!(child.try_wait().unwrap().is_none());
        sig::forward(child.id(), sig::SIGTERM);
        assert_eq!(sys::exit_code(child.wait().unwrap()), 128 + sig::SIGTERM);
    }

    #[test]
    fn unlink_from_handler_removes_the_file() {
        let dir = scratch("unlink");
        let file = dir.join("victim");
        fs::write(&file, b"x").unwrap();
        let c = cstr(&file).unwrap();
        // SAFETY: `to_bytes_with_nul` is NUL-terminated.
        unsafe { sig::unlink_from_handler(c.to_bytes_with_nul()) };
        assert!(!file.exists());
        let _ = fs::remove_dir_all(&dir);
    }
}

pub fn inheritable_clear_of_stdio(files: &[&File]) -> bool {
    files.iter().all(|f| f.as_raw_fd() > 2)
}

#[cfg(target_os = "macos")]
pub fn lower_thread_priority() {
    const QOS_CLASS_BACKGROUND: u32 = 0x09;
    const IOPOL_TYPE_DISK: libc::c_int = 0;
    const IOPOL_SCOPE_THREAD: libc::c_int = 1;
    const IOPOL_THROTTLE: libc::c_int = 3;
    unsafe extern "C" {
        fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: libc::c_int) -> libc::c_int;
        fn setiopolicy_np(iotype: libc::c_int, scope: libc::c_int, policy: libc::c_int) -> libc::c_int;
    }
    // SAFETY: both calls only change scheduling attributes of the calling thread.
    unsafe {
        pthread_set_qos_class_self_np(QOS_CLASS_BACKGROUND, 0);
        setiopolicy_np(IOPOL_TYPE_DISK, IOPOL_SCOPE_THREAD, IOPOL_THROTTLE);
    }
}

#[cfg(target_os = "linux")]
pub fn lower_thread_priority() {
    const IOPRIO_WHO_PROCESS: libc::c_long = 1;
    // Best-effort class with the lowest priority level: idle could starve behind a build.
    const IOPRIO_BEST_EFFORT_LOWEST: libc::c_long = (2 << 13) | 7;
    let this_thread: libc::c_long = 0;
    // SAFETY: ioprio_set and setpriority only change the scheduling attributes of the calling thread.
    unsafe {
        libc::syscall(libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, this_thread, IOPRIO_BEST_EFFORT_LOWEST);
        libc::setpriority(libc::PRIO_PROCESS, 0, 19);
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn lower_thread_priority() {}

#[cfg(target_os = "macos")]
pub fn fs_type_name(path: &Path) -> Option<String> {
    let c = cstr(path).ok()?;
    // SAFETY: statfs is plain data for which all-zero bytes are valid.
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid NUL-terminated path and `st` is a valid out pointer.
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let name: Vec<u8> = st.f_fstypename.iter().take_while(|&&b| b != 0).map(|&b| b as u8).collect();
    String::from_utf8(name).ok().map(|n| n.to_ascii_lowercase())
}

#[cfg(target_os = "linux")]
pub fn fs_type_name(path: &Path) -> Option<String> {
    let c = cstr(path).ok()?;
    // SAFETY: statfs is plain data for which all-zero bytes are valid.
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid NUL-terminated path and `st` is a valid out pointer.
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    Some(linux_fs_name(st.f_type as u64 & 0xFFFF_FFFF))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn fs_type_name(_path: &Path) -> Option<String> {
    None
}

#[cfg(target_os = "linux")]
fn linux_fs_name(magic: u64) -> String {
    let name = match magic {
        0x6969 => "nfs",
        0x517B => "smb",
        0xFF53_4D42 => "cifs",
        0xFE53_4D42 => "smb2",
        0x0102_1997 => "9p",
        0x6573_5546 => "fuse",
        0x4d44 => "msdos",
        0x2011_BAB0 => "exfat",
        0x5346_544e => "ntfs",
        0x00C3_6400 => "ceph",
        0x0BD0_0BD0 => "lustre",
        0x4750_4653 => "gpfs",
        0x5346_414F => "afs",
        0x7375_7245 => "coda",
        0x564c => "ncpfs",
        0xEF53 => "ext4",
        0x5846_5342 => "xfs",
        0x9123_683E => "btrfs",
        0x0102_1994 => "tmpfs",
        0x794c_7630 => "overlay",
        0x2fc1_2fc1 => "zfs",
        0xF2F5_2010 => "f2fs",
        0x8584_58f6 => "ramfs",
        0xCA45_1A4E => "bcachefs",
        other => return format!("magic-{other:x}"),
    };
    name.to_string()
}

#[cfg(test)]
mod compact_hashing_tests {
    use super::*;

    #[test]
    fn lowering_priority_on_a_spawned_thread_returns() {
        std::thread::spawn(lower_thread_priority).join().unwrap();
    }

    #[test]
    fn the_filesystem_of_a_directory_has_a_name_and_a_missing_path_has_none() {
        let dir = std::env::temp_dir();
        if cfg!(any(target_os = "macos", target_os = "linux")) {
            let name = fs_type_name(&dir).expect("temp dir filesystem");
            assert!(!name.is_empty() && name == name.to_ascii_lowercase(), "{name}");
        }
        assert_eq!(fs_type_name(&dir.join("frust-sys-no-such-path").join("x")), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_magic_numbers_map_to_names() {
        for (magic, name) in [
            (0x6969u64, "nfs"),
            (0xFF53_4D42, "cifs"),
            (0xFE53_4D42, "smb2"),
            (0x0102_1997, "9p"),
            (0x6573_5546, "fuse"),
            (0x4d44, "msdos"),
            (0x2011_BAB0, "exfat"),
            (0x5346_544e, "ntfs"),
            (0xEF53, "ext4"),
            (0x5846_5342, "xfs"),
            (0x9123_683E, "btrfs"),
            (0x0102_1994, "tmpfs"),
            (0x794c_7630, "overlay"),
            (0xCA45_1A4E, "bcachefs"),
        ] {
            assert_eq!(linux_fs_name(magic), name, "{magic:#x}");
        }
        assert_eq!(linux_fs_name(0xdead), "magic-dead");
    }
}

#[cfg(test)]
pub(super) fn non_utf8_os_string() -> std::ffi::OsString {
    use std::os::unix::ffi::OsStringExt;
    std::ffi::OsString::from_vec(vec![0xff])
}
