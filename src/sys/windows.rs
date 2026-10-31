use std::ffi::{OsStr, OsString, c_void};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{FileExt, FileTypeExt, MetadataExt, OpenOptionsExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::ptr::{null, null_mut};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::{ConvertSidToStringSidW, GetSecurityInfo};
use windows_sys::Win32::Security::{
    EqualSid, GetLengthSid, GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, LookupAccountSidW,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
    FILE_WRITE_ATTRIBUTES, GetDiskFreeSpaceExW, GetDiskFreeSpaceW, GetFileInformationByHandle,
    GetFileInformationByHandleEx, GetVolumeInformationByHandleW, GetVolumePathNameW, LOCKFILE_EXCLUSIVE_LOCK,
    LOCKFILE_FAIL_IMMEDIATELY, LockFileEx, UnlockFileEx,
};
use windows_sys::Win32::System::Console::{
    CONSOLE_SCREEN_BUFFER_INFO, GetConsoleMode, GetConsoleScreenBufferInfo, GetStdHandle, STD_ERROR_HANDLE,
    STD_OUTPUT_HANDLE, SetConsoleMode,
};
use windows_sys::Win32::System::IO::{DeviceIoControl, OVERLAPPED, OVERLAPPED_0, OVERLAPPED_0_0};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, SetInformationJobObject,
};
use windows_sys::Win32::System::Pipes::PeekNamedPipe;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcess, OpenProcessToken, WaitForSingleObject};

use super::{LockKind, StdStream};

pub type Owned = OwnedHandle;

pub const HOME_VARS: &[&str] = &["HOME", "USERPROFILE"];
pub const SYSROOT_DYLIB_DIR: &str = "bin";

type Psid = *mut c_void;

const LOCK_OFFSET: u64 = 0x7FFF_FFFF_0000_0000;
const SHARE_ALL: u32 = 7;
const READ_CONTROL: u32 = 0x0002_0000;
const SYNCHRONIZE: u32 = 0x0010_0000;
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
const FILE_ATTRIBUTE_SPARSE_FILE: u32 = 0x200;
const FILE_SUPPORTS_BLOCK_REFCOUNTING: u32 = 0x0800_0000;
const ERROR_ACCESS_DENIED: i32 = 5;
const ERROR_BROKEN_PIPE: i32 = 109;
const ERROR_MORE_DATA: i32 = 234;
const ERROR_NOT_LOCKED: i32 = 158;
const ERROR_LOCK_VIOLATION: i32 = 33;
const ERROR_PRIVILEGE_NOT_HELD: i32 = 1314;
const STATUS_CONTROL_C_EXIT: u32 = 0xC000_013A;
const WAIT_TIMEOUT: u32 = 258;
const TICKS_AT_UNIX_EPOCH: i64 = 116_444_736_000_000_000;
const FSCTL_SET_SPARSE: u32 = 0x0009_00C4;
const FSCTL_GET_RETRIEVAL_POINTERS: u32 = 0x0009_0073;
const FSCTL_GET_INTEGRITY_INFORMATION: u32 = 0x0009_027C;
const FSCTL_SET_INTEGRITY_INFORMATION: u32 = 0x0009_C280;
const FSCTL_DUPLICATE_EXTENTS_TO_FILE: u32 = 0x0009_8344;
const FILE_BASIC_INFO_CLASS: i32 = 0;
const FILE_STANDARD_INFO_CLASS: i32 = 1;
const FILE_ID_INFO_CLASS: i32 = 18;
const CLONE_CHUNK: u64 = 1 << 30;

#[repr(C)]
#[derive(Default)]
struct FileBasicInfo {
    creation_time: i64,
    last_access_time: i64,
    last_write_time: i64,
    change_time: i64,
    file_attributes: u32,
}

#[repr(C)]
#[derive(Default)]
struct FileStandardInfo {
    allocation_size: i64,
    end_of_file: i64,
    number_of_links: u32,
    delete_pending: u8,
    directory: u8,
}

#[repr(C)]
#[derive(Default)]
struct FileIdInfo {
    volume_serial_number: u64,
    file_id: [u8; 16],
}

#[repr(C)]
#[derive(Default)]
struct IntegrityInfo {
    checksum_algorithm: u16,
    reserved: u16,
    flags: u32,
    chunk_size: u32,
    cluster_size: u32,
}

#[repr(C)]
struct SetIntegrityInfo {
    checksum_algorithm: u16,
    reserved: u16,
    flags: u32,
}

#[repr(C)]
struct DuplicateExtents {
    file: HANDLE,
    source_offset: i64,
    target_offset: i64,
    byte_count: i64,
}

#[derive(Debug, Clone)]
pub struct Ext {
    dev: u64,
    ino: u64,
    nlink: u64,
    allocated: u64,
    mtime: (i64, i64),
    ctime: Option<(i64, i64)>,
    atime: (i64, i64),
}

fn ticks_to_unix(ticks: i64) -> (i64, i64) {
    let t = ticks - TICKS_AT_UNIX_EPOCH;
    (t.div_euclid(10_000_000), t.rem_euclid(10_000_000) * 100)
}

fn unix_now() -> (i64, i64) {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    (now.as_secs() as i64, i64::from(now.subsec_nanos()))
}

fn synth_mode(symlink: bool, dir: bool, readonly: bool) -> u32 {
    let base = if symlink {
        0o120777
    } else if dir {
        0o040755
    } else {
        0o100644
    };
    if readonly { base & !0o222 } else { base }
}

impl Ext {
    pub fn from_std(meta: &Metadata) -> Ext {
        Ext {
            dev: 0,
            ino: 0,
            nlink: 1,
            allocated: meta.len(),
            mtime: ticks_to_unix(meta.last_write_time() as i64),
            ctime: None,
            atime: ticks_to_unix(meta.last_access_time() as i64),
        }
    }

    pub fn dev(&self, _meta: &Metadata) -> u64 {
        self.dev
    }

    pub fn ino(&self, _meta: &Metadata) -> u64 {
        self.ino
    }

    pub fn nlink(&self, _meta: &Metadata) -> u64 {
        self.nlink
    }

    pub fn allocated(&self, _meta: &Metadata) -> u64 {
        self.allocated
    }

    pub fn mtime(&self, _meta: &Metadata) -> (i64, i64) {
        self.mtime
    }

    // FAT-style volumes report no change time; the current time never matches a stored stamp.
    pub fn ctime(&self, _meta: &Metadata) -> (i64, i64) {
        self.ctime.unwrap_or_else(unix_now)
    }

    pub fn atime(&self, _meta: &Metadata) -> (i64, i64) {
        self.atime
    }

    pub fn mode(&self, meta: &Metadata) -> u32 {
        synth_mode(meta.file_type().is_symlink(), meta.is_dir(), meta.permissions().readonly())
    }
}

fn query_info<T: Default>(file: &File, class: i32) -> Option<T> {
    let mut info = T::default();
    // SAFETY: `info` is a live, writable buffer of exactly the size passed.
    let ok = unsafe {
        GetFileInformationByHandleEx(file.as_raw_handle(), class, (&mut info as *mut T).cast(), size_of::<T>() as u32)
    };
    (ok != 0).then_some(info)
}

fn file_identity(file: &File) -> io::Result<(u64, u64)> {
    if let Some(id) = query_info::<FileIdInfo>(file, FILE_ID_INFO_CLASS) {
        let lo = u64::from_le_bytes(id.file_id[..8].try_into().unwrap());
        let hi = u64::from_le_bytes(id.file_id[8..].try_into().unwrap());
        return Ok((id.volume_serial_number, lo ^ hi));
    }
    // SAFETY: BY_HANDLE_FILE_INFORMATION is plain data and all-zero is a valid value.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: the handle is open and `info` is a valid out pointer.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
    Ok((u64::from(info.dwVolumeSerialNumber), index))
}

fn query_meta(file: &File) -> io::Result<(Metadata, Ext)> {
    let meta = file.metadata()?;
    let mut ext = Ext::from_std(&meta);
    let (dev, ino) = file_identity(file)?;
    ext.dev = dev;
    ext.ino = ino;
    if let Some(std_info) = query_info::<FileStandardInfo>(file, FILE_STANDARD_INFO_CLASS) {
        ext.nlink = u64::from(std_info.number_of_links);
        ext.allocated = std_info.allocation_size.max(0) as u64;
    }
    if let Some(basic) = query_info::<FileBasicInfo>(file, FILE_BASIC_INFO_CLASS) {
        ext.mtime = ticks_to_unix(basic.last_write_time);
        ext.atime = ticks_to_unix(basic.last_access_time);
        ext.ctime = (basic.change_time != 0).then(|| ticks_to_unix(basic.change_time));
    }
    Ok((meta, ext))
}

fn open_handle(path: &Path, access: u32, follow: bool) -> io::Result<File> {
    let reparse = if follow { 0 } else { FILE_FLAG_OPEN_REPARSE_POINT };
    OpenOptions::new()
        .access_mode(access)
        .share_mode(SHARE_ALL)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | reparse)
        .open(path)
}

pub fn stat(path: &Path) -> io::Result<(Metadata, Ext)> {
    query_meta(&open_handle(path, FILE_READ_ATTRIBUTES, true)?)
}

pub fn lstat(path: &Path) -> io::Result<(Metadata, Ext)> {
    query_meta(&open_handle(path, FILE_READ_ATTRIBUTES, false)?)
}

pub fn fstat(file: &File) -> io::Result<(Metadata, Ext)> {
    query_meta(file)
}

pub fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        match file.seek_read(buf, offset) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "failed to fill whole buffer")),
            Ok(n) => {
                buf = &mut buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

pub fn write_all_at(file: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        match file.seek_write(buf, offset) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "failed to write whole buffer")),
            Ok(n) => {
                buf = &buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn sentinel() -> OVERLAPPED {
    OVERLAPPED {
        Internal: 0,
        InternalHigh: 0,
        Anonymous: OVERLAPPED_0 {
            Anonymous: OVERLAPPED_0_0 { Offset: LOCK_OFFSET as u32, OffsetHigh: (LOCK_OFFSET >> 32) as u32 },
        },
        hEvent: null_mut(),
    }
}

fn lock_range(file: &File, flags: u32) -> io::Result<()> {
    let mut overlapped = sentinel();
    // SAFETY: the handle is open and `overlapped` outlives the synchronous call.
    let ok = unsafe { LockFileEx(file.as_raw_handle(), flags, 0, 1, 0, &mut overlapped) };
    if ok != 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

fn lock_flags(kind: LockKind) -> u32 {
    match kind {
        LockKind::Shared => 0,
        LockKind::Exclusive => LOCKFILE_EXCLUSIVE_LOCK,
    }
}

pub fn lock(file: &File, kind: LockKind) -> io::Result<()> {
    lock_range(file, lock_flags(kind))
}

pub fn try_lock(file: &File, kind: LockKind) -> Result<(), fs::TryLockError> {
    match lock_range(file, lock_flags(kind) | LOCKFILE_FAIL_IMMEDIATELY) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(ERROR_LOCK_VIOLATION) => Err(fs::TryLockError::WouldBlock),
        Err(e) => Err(fs::TryLockError::Error(e)),
    }
}

pub fn unlock(file: &File) -> io::Result<()> {
    let mut overlapped = sentinel();
    // SAFETY: the handle is open and `overlapped` outlives the synchronous call.
    let ok = unsafe { UnlockFileEx(file.as_raw_handle(), 0, 1, 0, &mut overlapped) };
    if ok != 0 {
        return Ok(());
    }
    // Unlocking a handle that holds nothing succeeds, as flock does.
    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(ERROR_NOT_LOCKED) { Ok(()) } else { Err(err) }
}

pub fn with_mode(opts: &mut OpenOptions, _mode: u32) -> &mut OpenOptions {
    opts
}

pub fn set_file_mode(_file: &File, _mode: u32) -> io::Result<()> {
    Ok(())
}

pub fn set_path_mode(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

pub fn create_dir_with_mode(path: &Path, _mode: u32) -> io::Result<()> {
    fs::create_dir(path)
}

struct Sid {
    words: Vec<u32>,
}

impl Sid {
    fn from_raw(raw: Psid) -> Option<Sid> {
        if raw.is_null() {
            return None;
        }
        // SAFETY: `raw` points at a valid SID supplied by the system.
        let len = unsafe { GetLengthSid(raw) } as usize;
        if len == 0 {
            return None;
        }
        // Word storage keeps the copy 4-byte aligned, as SID consumers expect.
        let mut words = vec![0u32; len.div_ceil(4)];
        // SAFETY: the source holds `len` bytes and the destination has room for at least `len`.
        unsafe { std::ptr::copy_nonoverlapping(raw as *const u8, words.as_mut_ptr().cast::<u8>(), len) };
        Some(Sid { words })
    }

    fn psid(&self) -> Psid {
        self.words.as_ptr() as Psid
    }

    fn same_as(&self, other: &Sid) -> bool {
        // SAFETY: both pointers refer to valid SID copies owned by the callers.
        unsafe { EqualSid(self.psid(), other.psid()) != 0 }
    }

    fn rid(&self) -> Option<u32> {
        // SAFETY: the pointer is a valid SID; both accessors return null for an invalid one.
        unsafe {
            let count = GetSidSubAuthorityCount(self.psid());
            if count.is_null() || *count == 0 {
                return None;
            }
            let sub = GetSidSubAuthority(self.psid(), u32::from(*count) - 1);
            (!sub.is_null()).then(|| *sub)
        }
    }

    fn to_text(&self) -> Option<String> {
        let mut text: *mut u16 = null_mut();
        // SAFETY: the SID is valid and `text` receives a system-allocated string.
        if unsafe { ConvertSidToStringSidW(self.psid(), &mut text) } == 0 || text.is_null() {
            return None;
        }
        // SAFETY: `text` is NUL-terminated, so the scan stays inside the allocation.
        let units = unsafe {
            let mut n = 0;
            while *text.add(n) != 0 {
                n += 1;
            }
            std::slice::from_raw_parts(text, n)
        };
        let out = String::from_utf16_lossy(units);
        // SAFETY: `text` was allocated by the system for us to free with LocalFree.
        unsafe { LocalFree(text.cast()) };
        Some(out)
    }
}

struct TokenSids {
    user: Sid,
    owner: Option<Sid>,
}

fn token_sid(class: i32) -> Option<Sid> {
    let mut raw: HANDLE = null_mut();
    // SAFETY: querying the current process token with a valid out pointer.
    if unsafe { OpenProcessToken(GetCurrentProcess(), 0x8, &mut raw) } == 0 {
        return None;
    }
    // SAFETY: `raw` is a fresh token handle that we own; the wrapper closes it.
    let token = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut needed = 0u32;
    // SAFETY: a zero-length query only reports the required size.
    unsafe { GetTokenInformation(token.as_raw_handle(), class, null_mut(), 0, &mut needed) };
    if needed == 0 {
        return None;
    }
    let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
    // SAFETY: `buf` holds at least `needed` bytes and is 8-byte aligned.
    let ok = unsafe { GetTokenInformation(token.as_raw_handle(), class, buf.as_mut_ptr().cast(), needed, &mut needed) };
    if ok == 0 {
        return None;
    }
    // SAFETY: TOKEN_USER and TOKEN_OWNER both begin with a SID pointer, valid while `buf` lives.
    let sid = unsafe { std::ptr::read(buf.as_ptr().cast::<Psid>()) };
    Sid::from_raw(sid)
}

fn token_sids() -> Option<&'static TokenSids> {
    static SIDS: OnceLock<Option<TokenSids>> = OnceLock::new();
    // TokenUser is class 1 and TokenOwner class 4.
    SIDS.get_or_init(|| Some(TokenSids { user: token_sid(1)?, owner: token_sid(4) })).as_ref()
}

fn is_me(sid: &Sid) -> bool {
    token_sids().is_some_and(|me| me.user.same_as(sid) || me.owner.as_ref().is_some_and(|o| o.same_as(sid)))
}

fn file_owner(file: &File) -> Option<Sid> {
    let mut owner: Psid = null_mut();
    let mut descriptor: *mut c_void = null_mut();
    // SAFETY: the handle is open with READ_CONTROL; the out pointers are valid.
    let rc = unsafe {
        GetSecurityInfo(file.as_raw_handle(), 1, 1, &mut owner, null_mut(), null_mut(), null_mut(), &mut descriptor)
    };
    if rc != 0 {
        return None;
    }
    let sid = Sid::from_raw(owner);
    // SAFETY: the descriptor was allocated by GetSecurityInfo and is no longer referenced.
    unsafe { LocalFree(descriptor) };
    sid
}

fn path_owner(path: &Path) -> Option<Sid> {
    file_owner(&open_handle(path, READ_CONTROL, false).ok()?)
}

pub fn owned_by_me(path: &Path) -> bool {
    path_owner(path).is_some_and(|owner| is_me(&owner))
}

pub fn verify_private_dir(dir: &Path) -> Result<(), String> {
    let meta = fs::symlink_metadata(dir).map_err(|e| format!("cannot inspect {}: {e}", dir.display()))?;
    let refuse = |why: &str| {
        Err(format!(
            "refusing to use {} as frust's state directory: {why}; set FRUST_HOME or HOME to a private location",
            dir.display()
        ))
    };
    if meta.file_type().is_symlink() || !meta.file_type().is_dir() {
        return refuse("it is not a directory");
    }
    let Some(owner) = path_owner(dir) else {
        return refuse("its owner cannot be determined");
    };
    if !is_me(&owner) {
        let name = |sid: Option<&Sid>| sid.and_then(Sid::to_text).unwrap_or_else(|| "unknown".to_string());
        let mine = token_sids().map(|me| &me.user);
        return refuse(&format!("it is owned by {}, not {}", name(Some(&owner)), name(mine)));
    }
    Ok(())
}

pub fn user_id() -> u32 {
    token_sids().and_then(|me| me.user.rid()).unwrap_or(0)
}

pub fn user_name() -> Option<String> {
    account_name().or_else(|| std::env::var("USERNAME").ok().filter(|u| !u.is_empty()))
}

fn account_name() -> Option<String> {
    let me = token_sids()?;
    let mut name = [0u16; 256];
    let mut domain = [0u16; 256];
    let (mut name_len, mut domain_len) = (name.len() as u32, domain.len() as u32);
    let mut kind = 0i32;
    // SAFETY: both buffers are writable with the capacities passed; the SID is valid.
    let ok = unsafe {
        LookupAccountSidW(
            null(),
            me.user.psid(),
            name.as_mut_ptr(),
            &mut name_len,
            domain.as_mut_ptr(),
            &mut domain_len,
            &mut kind,
        )
    };
    if ok == 0 {
        return None;
    }
    let text = String::from_utf16_lossy(&name[..name_len as usize]);
    (!text.is_empty()).then_some(text)
}

pub fn home_var() -> Option<OsString> {
    std::env::home_dir().map(PathBuf::into_os_string)
}

pub fn rename(from: &Path, to: &Path) -> io::Result<()> {
    let mut delay = 5u64;
    let mut attempt = 1;
    loop {
        match fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) if attempt < 8 && matches!(e.raw_os_error(), Some(5 | 32 | 33)) => {
                std::thread::sleep(Duration::from_millis(delay));
                delay = (delay * 2).min(100);
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

pub fn set_mtime_now(path: &Path) -> io::Result<()> {
    open_handle(path, FILE_WRITE_ATTRIBUTES, true)?.set_modified(SystemTime::now())
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

fn ioctl<I, O>(file: &File, code: u32, input: Option<&I>, output: Option<&mut O>) -> io::Result<u32> {
    let (in_ptr, in_len) = match input {
        Some(i) => ((i as *const I).cast::<c_void>(), size_of::<I>() as u32),
        None => (null(), 0),
    };
    let (out_ptr, out_len) = match output {
        Some(o) => ((o as *mut O).cast::<c_void>(), size_of::<O>() as u32),
        None => (null_mut(), 0),
    };
    let mut returned = 0u32;
    // SAFETY: the pointers and lengths come from live typed references of plain data.
    let ok = unsafe {
        DeviceIoControl(file.as_raw_handle(), code, in_ptr, in_len, out_ptr, out_len, &mut returned, null_mut())
    };
    if ok != 0 { Ok(returned) } else { Err(io::Error::last_os_error()) }
}

fn supports_block_refcounting(file: &File) -> bool {
    let mut flags = 0u32;
    // SAFETY: the handle is open and only the flags out pointer is requested.
    let ok = unsafe {
        GetVolumeInformationByHandleW(file.as_raw_handle(), null_mut(), 0, null_mut(), null_mut(), &mut flags, null_mut(), 0)
    };
    ok != 0 && flags & FILE_SUPPORTS_BLOCK_REFCOUNTING != 0
}

fn cluster_size(path: &Path) -> io::Result<u64> {
    let full = wide(&std::path::absolute(path)?);
    let mut root = [0u16; 261];
    // SAFETY: `full` is NUL-terminated and `root` has the capacity passed.
    if unsafe { GetVolumePathNameW(full.as_ptr(), root.as_mut_ptr(), root.len() as u32) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let (mut sectors, mut bytes, mut free, mut total) = (0u32, 0u32, 0u32, 0u32);
    // SAFETY: `root` is NUL-terminated and all out pointers are valid.
    if unsafe { GetDiskFreeSpaceW(root.as_ptr(), &mut sectors, &mut bytes, &mut free, &mut total) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(u64::from(sectors) * u64::from(bytes))
}

pub fn reflink(src: &Path, dst: &Path) -> io::Result<()> {
    if fs::symlink_metadata(src)?.is_dir() {
        return Err(io::Error::from(io::ErrorKind::Unsupported));
    }
    let input = File::open(src)?;
    if !supports_block_refcounting(&input) {
        return Err(io::Error::from(io::ErrorKind::Unsupported));
    }
    let cluster = cluster_size(src)?;
    if cluster == 0 {
        return Err(io::Error::from(io::ErrorKind::Unsupported));
    }
    let output = File::options().read(true).write(true).create_new(true).open(dst)?;
    match clone_extents(&input, &output, cluster) {
        Ok(()) => Ok(()),
        Err(err) => {
            drop(output);
            let _ = fs::remove_file(dst);
            Err(err)
        }
    }
}

fn clone_extents(input: &File, output: &File, cluster: u64) -> io::Result<()> {
    let meta = input.metadata()?;
    let len = meta.len();
    if meta.file_attributes() & FILE_ATTRIBUTE_SPARSE_FILE != 0 {
        ioctl::<(), ()>(output, FSCTL_SET_SPARSE, None, None)?;
    }
    let mut integrity = IntegrityInfo::default();
    if ioctl::<(), _>(input, FSCTL_GET_INTEGRITY_INFORMATION, None, Some(&mut integrity)).is_ok() {
        let wanted = SetIntegrityInfo {
            checksum_algorithm: integrity.checksum_algorithm,
            reserved: 0,
            flags: integrity.flags,
        };
        ioctl::<_, ()>(output, FSCTL_SET_INTEGRITY_INFORMATION, Some(&wanted), None)?;
    }
    let rounded = len.div_ceil(cluster) * cluster;
    output.set_len(rounded)?;
    let mut offset = 0u64;
    while offset < rounded {
        let count = CLONE_CHUNK.min(rounded - offset);
        let request = DuplicateExtents {
            file: input.as_raw_handle(),
            source_offset: offset as i64,
            target_offset: offset as i64,
            byte_count: count as i64,
        };
        ioctl::<_, ()>(output, FSCTL_DUPLICATE_EXTENTS_TO_FILE, Some(&request), None)?;
        offset += count;
    }
    output.set_len(len)?;
    if meta.permissions().readonly() {
        let mut perms = output.metadata()?.permissions();
        perms.set_readonly(true);
        output.set_permissions(perms)?;
    }
    Ok(())
}

pub fn copy_symlink(src: &Path, dst: &Path) -> io::Result<()> {
    let target = fs::read_link(src)?;
    if fs::symlink_metadata(src)?.file_type().is_symlink_dir() {
        return std::os::windows::fs::symlink_dir(target, dst);
    }
    match std::os::windows::fs::symlink_file(&target, dst) {
        Err(e) if e.raw_os_error() == Some(ERROR_PRIVILEGE_NOT_HELD) => fs::copy(src, dst).map(|_| ()),
        other => other,
    }
}

pub fn link_tool(exe: &Path, link: &Path) -> io::Result<()> {
    if fs::symlink_metadata(link).is_ok() {
        return Err(io::Error::from(io::ErrorKind::AlreadyExists));
    }
    std::os::windows::fs::symlink_file(exe, link)
        .or_else(|_| fs::hard_link(exe, link))
        .or_else(|_| fs::copy(exe, link).map(|_| ()))
}

pub fn first_extent(path: &Path) -> Option<u64> {
    let file = File::open(path).ok()?;
    if !supports_block_refcounting(&file) {
        return None;
    }
    let start = 0i64;
    // Header (count, starting VCN) then extents of (next VCN, LCN); three extents fit.
    let mut out = [0u64; 8];
    match ioctl(&file, FSCTL_GET_RETRIEVAL_POINTERS, Some(&start), Some(&mut out)) {
        Ok(_) => {}
        Err(e) if e.raw_os_error() == Some(ERROR_MORE_DATA) => {}
        Err(_) => return None,
    }
    let count = out[0] as u32;
    let lcn = out[3] as i64;
    if count == 0 || lcn < 0 {
        return None;
    }
    Some(lcn as u64 * cluster_size(path).ok()?)
}

pub fn free_space(existing: &Path) -> io::Result<u64> {
    let mut path = std::path::absolute(existing)?;
    if fs::metadata(&path)?.is_file() {
        path.pop();
    }
    let wide_path = wide(&path);
    let mut available = 0u64;
    // SAFETY: `wide_path` is NUL-terminated and the out pointer is valid.
    let ok = unsafe { GetDiskFreeSpaceExW(wide_path.as_ptr(), &mut available, null_mut(), null_mut()) };
    if ok != 0 { Ok(available) } else { Err(io::Error::last_os_error()) }
}

fn is_device_name(component: &str) -> bool {
    let stem = component.split('.').next().unwrap_or("").trim_end().to_ascii_uppercase();
    match stem.as_str() {
        "CON" | "PRN" | "AUX" | "NUL" => true,
        _ => {
            let bytes = stem.as_bytes();
            bytes.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && (b'1'..=b'9').contains(&bytes[3])
        }
    }
}

// Returns the plain form of a verbatim path only when Win32 would read it back identically.
fn simplified(path: &str) -> Option<String> {
    let plain = if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else {
        let rest = path.strip_prefix(r"\\?\")?;
        let b = rest.as_bytes();
        if b.len() < 2 || !b[0].is_ascii_alphabetic() || b[1] != b':' {
            return None;
        }
        rest.to_string()
    };
    if plain.chars().count() >= 260 {
        return None;
    }
    let risky = |c: &str| !c.is_empty() && (c.ends_with('.') || c.ends_with(' ') || is_device_name(c));
    if plain.split('\\').any(risky) {
        return None;
    }
    Some(plain)
}

pub fn canonicalize(path: &Path) -> io::Result<PathBuf> {
    let full = fs::canonicalize(path)?;
    Ok(full.to_str().and_then(simplified).map(PathBuf::from).unwrap_or(full))
}

fn executable_candidates(name: &str, pathext: &str) -> Vec<String> {
    if Path::new(name).extension().is_some() {
        return vec![name.to_string()];
    }
    pathext.split(';').map(str::trim).filter(|e| !e.is_empty()).map(|e| format!("{name}{e}")).collect()
}

pub fn find_executable_in(dir: &Path, name: &str) -> Option<PathBuf> {
    let pathext = std::env::var("PATHEXT").ok().filter(|v| !v.trim().is_empty());
    let pathext = pathext.as_deref().unwrap_or(".COM;.EXE;.BAT;.CMD");
    executable_candidates(name, pathext)
        .into_iter()
        .map(|candidate| dir.join(candidate))
        .find(|candidate| candidate.metadata().is_ok_and(|m| m.is_file()))
}

pub fn tool_name(argv0: &OsStr) -> String {
    let name = Path::new(argv0).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    match name.len().checked_sub(4).and_then(|at| name.get(at..)) {
        Some(tail) if tail.eq_ignore_ascii_case(".exe") => name[..name.len() - 4].to_string(),
        _ => name,
    }
}

pub fn pid_alive(pid: u32) -> bool {
    // SAFETY: OpenProcess has no memory preconditions.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        return io::Error::last_os_error().raw_os_error() == Some(ERROR_ACCESS_DENIED);
    }
    // SAFETY: `handle` is a valid process handle we own.
    let alive = unsafe { WaitForSingleObject(handle, 0) } == WAIT_TIMEOUT;
    // SAFETY: closing the handle opened above exactly once.
    unsafe { CloseHandle(handle) };
    alive
}

struct Job(OwnedHandle);

impl Job {
    // Kill-on-close takes the whole tree down if frust itself is terminated.
    fn contain_current() -> Option<Job> {
        // SAFETY: creating an unnamed job object with default security.
        let raw = unsafe { CreateJobObjectW(null(), null()) };
        if raw.is_null() {
            return None;
        }
        // SAFETY: `raw` is a fresh handle that we own.
        let job = Job(unsafe { OwnedHandle::from_raw_handle(raw) });
        job.set_limit(JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE).ok()?;
        // SAFETY: both handles are valid; failure (already in an incompatible job) is tolerated.
        let assigned = unsafe { AssignProcessToJobObject(raw, GetCurrentProcess()) };
        (assigned != 0).then_some(job)
    }

    fn set_limit(&self, flags: u32) -> io::Result<()> {
        // SAFETY: the struct is plain data and all-zero is a valid value.
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags = flags;
        // SAFETY: the handle is a job object and the buffer is exactly one extended-limit struct (class 9).
        let ok = unsafe {
            SetInformationJobObject(
                self.0.as_raw_handle(),
                9,
                (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok != 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
    }

    fn disarm(&self) {
        let _ = self.set_limit(0);
    }
}

const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;

// Windows has no load average, so callers report it as unavailable.
pub fn load_average() -> Option<f64> {
    None
}

pub const INHERITS_DESCRIPTORS: bool = false;

// CREATE_NO_WINDOW is ignored together with DETACHED_PROCESS, so only the first and the process-group flag are set.
pub fn detach_command(cmd: &mut Command, _inherit: &[&File]) {
    use std::os::windows::process::CommandExt;
    use std::process::Stdio;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

pub fn exec_replace(cmd: &mut Command) -> io::Error {
    let previous = signal::ignore_all();
    let job = Job::contain_current();
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            if let Some(job) = &job {
                job.disarm();
            }
            signal::restore_all(previous);
            return err;
        }
    };
    let code = child.wait().ok().and_then(|status| status.code()).unwrap_or(1);
    if let Some(job) = &job {
        job.disarm();
    }
    let _ = io::stdout().flush();
    let _ = io::stderr().flush();
    std::process::exit(code)
}

fn map_exit(code: i32) -> i32 {
    if code as u32 == STATUS_CONTROL_C_EXIT { 130 } else { code }
}

pub fn exit_code(status: ExitStatus) -> i32 {
    status.code().map(map_exit).unwrap_or(1)
}

pub fn pipe_prepare(file: File) -> File {
    file
}

enum Peek {
    Available(u32),
    Eof,
    Failed(io::Error),
}

fn peek(file: &File) -> Peek {
    let mut available = 0u32;
    // SAFETY: the handle is open and only the available-bytes out pointer is requested.
    let ok = unsafe { PeekNamedPipe(file.as_raw_handle(), null_mut(), 0, null_mut(), &mut available, null_mut()) };
    if ok != 0 {
        return Peek::Available(available);
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(ERROR_BROKEN_PIPE) { Peek::Eof } else { Peek::Failed(err) }
}

pub fn pipe_read(file: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    if buf.is_empty() {
        return Ok(0);
    }
    match peek(file) {
        Peek::Available(0) => Err(io::Error::from(io::ErrorKind::WouldBlock)),
        Peek::Available(n) => {
            let take = (n as usize).min(buf.len());
            file.read(&mut buf[..take])
        }
        Peek::Eof => Ok(0),
        Peek::Failed(err) => Err(err),
    }
}

pub fn wait_readable(pipes: [Option<&File>; 2], timeout: Duration) -> [bool; 2] {
    let deadline = Instant::now() + timeout;
    loop {
        let mut ready = [false; 2];
        for (slot, pipe) in ready.iter_mut().zip(pipes) {
            if let Some(file) = pipe {
                *slot = !matches!(peek(file), Peek::Available(0));
            }
        }
        if ready.iter().any(|&r| r) || pipes.iter().all(Option::is_none) {
            return ready;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return ready;
        }
        std::thread::sleep(remaining.min(Duration::from_millis(4)));
    }
}

fn probe_ansi(stream: StdStream) -> bool {
    let (id, terminal) = match stream {
        StdStream::Stdout => (STD_OUTPUT_HANDLE, io::stdout().is_terminal()),
        StdStream::Stderr => (STD_ERROR_HANDLE, io::stderr().is_terminal()),
    };
    // SAFETY: GetStdHandle has no preconditions.
    let handle = unsafe { GetStdHandle(id) };
    if handle.is_null() || handle as isize == -1 {
        return false;
    }
    let mut mode = 0u32;
    // SAFETY: `mode` is a valid out pointer.
    if unsafe { GetConsoleMode(handle, &mut mode) } == 0 {
        // A pty such as mintty is a terminal without being a console.
        return terminal;
    }
    // 0x4 is ENABLE_VIRTUAL_TERMINAL_PROCESSING and 0x1 is ENABLE_PROCESSED_OUTPUT.
    // SAFETY: the handle is a console handle.
    unsafe { SetConsoleMode(handle, mode | 0x4 | 0x1) != 0 }
}

pub fn ansi_terminal(s: StdStream) -> bool {
    static STDOUT: OnceLock<bool> = OnceLock::new();
    static STDERR: OnceLock<bool> = OnceLock::new();
    match s {
        StdStream::Stdout => *STDOUT.get_or_init(|| probe_ansi(s)),
        StdStream::Stderr => *STDERR.get_or_init(|| probe_ansi(s)),
    }
}

pub fn terminal_columns() -> Option<u16> {
    // SAFETY: GetStdHandle has no preconditions.
    let handle = unsafe { GetStdHandle(STD_ERROR_HANDLE) };
    if handle.is_null() || handle as isize == -1 {
        return None;
    }
    // SAFETY: the struct is plain data and all-zero is a valid value.
    let mut info: CONSOLE_SCREEN_BUFFER_INFO = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is a valid out pointer.
    if unsafe { GetConsoleScreenBufferInfo(handle, &mut info) } == 0 {
        return None;
    }
    let width = i32::from(info.srWindow.Right) - i32::from(info.srWindow.Left) + 1;
    u16::try_from(width).ok().filter(|&w| w > 0)
}

pub mod signal {
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use windows_sys::Win32::System::Console::{
        CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT, SetConsoleCtrlHandler,
    };
    use windows_sys::Win32::System::Threading::ExitProcess;

    use crate::sys::signal::{Disposition, Handler, Signal};

    pub const SIGINT: Signal = 2;
    pub const SIGTERM: Signal = 15;
    pub const SIGHUP: Signal = 1;
    pub const SIGQUIT: Signal = 3;

    const DEFAULT: usize = 0;
    const IGNORE: usize = 1;
    const SIGNALS: [Signal; 4] = [SIGINT, SIGTERM, SIGHUP, SIGQUIT];

    // 0 is Default, 1 is Ignore, anything else is a handler address.
    static TABLE: [AtomicUsize; 4] = [AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0)];

    pub struct Saved {
        index: usize,
        word: usize,
    }

    fn index_of(sig: Signal) -> Option<usize> {
        SIGNALS.iter().position(|&s| s == sig)
    }

    fn event_signal(ctrl: u32) -> Option<Signal> {
        match ctrl {
            CTRL_C_EVENT | CTRL_BREAK_EVENT => Some(SIGINT),
            CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT => Some(SIGHUP),
            _ => None,
        }
    }

    fn call(word: usize, sig: Signal) {
        // SAFETY: any word other than DEFAULT and IGNORE was stored from a `Handler` by `install`.
        let handler = unsafe { std::mem::transmute::<usize, Handler>(word) };
        handler(sig);
    }

    unsafe extern "system" fn routine(ctrl: u32) -> i32 {
        let Some(index) = event_signal(ctrl).and_then(index_of) else {
            return 0;
        };
        match TABLE[index].load(Ordering::SeqCst) {
            DEFAULT => 0,
            IGNORE => 1,
            word => {
                call(word, SIGNALS[index]);
                1
            }
        }
    }

    fn ensure_registered() -> bool {
        static REGISTERED: OnceLock<bool> = OnceLock::new();
        // SAFETY: registering a routine with the `system` ABI that lives for the whole process.
        *REGISTERED.get_or_init(|| unsafe { SetConsoleCtrlHandler(Some(routine), 1) } != 0)
    }

    pub fn disposition(sig: Signal) -> Disposition {
        match index_of(sig).map(|i| TABLE[i].load(Ordering::SeqCst)) {
            None | Some(IGNORE) => Disposition::Ignore,
            Some(DEFAULT) => Disposition::Default,
            Some(_) => Disposition::Handler,
        }
    }

    pub fn install(sig: Signal, handler: Handler) -> Option<Saved> {
        let index = index_of(sig)?;
        if !ensure_registered() {
            return None;
        }
        let word = TABLE[index].swap(handler as *const () as usize, Ordering::SeqCst);
        Some(Saved { index, word })
    }

    pub fn set_default(sig: Signal) {
        if let Some(index) = index_of(sig) {
            TABLE[index].store(DEFAULT, Ordering::SeqCst);
        }
    }

    pub fn restore(saved: Saved) {
        TABLE[saved.index].store(saved.word, Ordering::SeqCst);
    }

    pub fn raise(sig: Signal) {
        let Some(index) = index_of(sig) else {
            return;
        };
        match TABLE[index].load(Ordering::SeqCst) {
            DEFAULT => {
                // SAFETY: terminating the process is always permitted.
                unsafe { ExitProcess(if sig == SIGINT { super::STATUS_CONTROL_C_EXIT } else { 1 }) }
            }
            IGNORE => {}
            word => call(word, sig),
        }
    }

    pub fn forward(_pid: u32, _sig: Signal) {}

    // Ignores all four signals so Ctrl-C reaches only the child; returns the words to restore.
    pub fn ignore_all() -> [usize; 4] {
        ensure_registered();
        std::array::from_fn(|i| TABLE[i].swap(IGNORE, Ordering::SeqCst))
    }

    pub fn restore_all(words: [usize; 4]) {
        for (slot, word) in TABLE.iter().zip(words) {
            slot.store(word, Ordering::SeqCst);
        }
    }

    /// # Safety
    /// `path_nul` must end with a NUL byte and hold bytes from `OsStr::as_encoded_bytes`.
    pub unsafe fn unlink_from_handler(path_nul: &[u8]) {
        let end = path_nul.iter().position(|&b| b == 0).unwrap_or(path_nul.len());
        // SAFETY: the caller guarantees the bytes came from `OsStr::as_encoded_bytes`.
        let path = unsafe { std::ffi::OsStr::from_encoded_bytes_unchecked(&path_nul[..end]) };
        let _ = std::fs::remove_file(path);
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::atomic::AtomicI32;

        static SEEN: AtomicI32 = AtomicI32::new(0);

        extern "C" fn note(sig: i32) {
            SEEN.store(sig, Ordering::SeqCst);
        }

        #[test]
        fn events_map_to_signals() {
            assert_eq!(event_signal(CTRL_C_EVENT), Some(SIGINT));
            assert_eq!(event_signal(CTRL_BREAK_EVENT), Some(SIGINT));
            assert_eq!(event_signal(CTRL_CLOSE_EVENT), Some(SIGHUP));
            assert_eq!(event_signal(CTRL_LOGOFF_EVENT), Some(SIGHUP));
            assert_eq!(event_signal(CTRL_SHUTDOWN_EVENT), Some(SIGHUP));
            assert_eq!(event_signal(99), None);
        }

        #[test]
        fn dispositions_round_trip_and_raise_calls_the_handler() {
            let saved = install(SIGQUIT, note).expect("install");
            assert_eq!(disposition(SIGQUIT), Disposition::Handler);
            raise(SIGQUIT);
            assert_eq!(SEEN.load(Ordering::SeqCst), SIGQUIT);
            set_default(SIGQUIT);
            assert_eq!(disposition(SIGQUIT), Disposition::Default);
            restore(saved);
            assert_eq!(disposition(SIGQUIT), Disposition::Default);
            assert!(install(-1, note).is_none());
            assert_eq!(disposition(-1), Disposition::Ignore);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("frust-sys-win-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn filetime_converts_to_unix_time() {
        assert_eq!(ticks_to_unix(TICKS_AT_UNIX_EPOCH), (0, 0));
        assert_eq!(ticks_to_unix(TICKS_AT_UNIX_EPOCH + 10_000_000 + 5), (1, 500));
        assert_eq!(ticks_to_unix(TICKS_AT_UNIX_EPOCH - 1), (-1, 999_999_900));
    }

    #[test]
    fn modes_are_synthesised_from_type_and_readonly() {
        assert_eq!(synth_mode(false, false, false), 0o100644);
        assert_eq!(synth_mode(false, false, true), 0o100444);
        assert_eq!(synth_mode(false, true, false), 0o040755);
        assert_eq!(synth_mode(false, true, true), 0o040555);
        assert_eq!(synth_mode(true, false, false), 0o120777);
        assert_eq!(synth_mode(true, true, true), 0o120555);
    }

    #[test]
    fn lock_offset_sits_far_past_eof() {
        assert_eq!(LOCK_OFFSET, 0x7FFF_FFFF_0000_0000);
        assert_eq!(LOCK_OFFSET as u32, 0);
        assert_eq!((LOCK_OFFSET >> 32) as u32, 0x7FFF_FFFF);
    }

    #[test]
    fn exit_codes_map_ctrl_c_and_keep_other_statuses() {
        assert_eq!(map_exit(STATUS_CONTROL_C_EXIT as i32), 130);
        assert_eq!(map_exit(0), 0);
        assert_eq!(map_exit(101), 101);
        assert_eq!(map_exit(0xC000_0005u32 as i32), 0xC000_0005u32 as i32);
        assert!(map_exit(0xC000_0005u32 as i32) < 0);
    }

    #[test]
    fn verbatim_prefixes_are_stripped_only_when_safe() {
        assert_eq!(simplified(r"\\?\C:\foo\bar").as_deref(), Some(r"C:\foo\bar"));
        assert_eq!(simplified(r"\\?\UNC\srv\share\x").as_deref(), Some(r"\\srv\share\x"));
        assert_eq!(simplified(r"C:\plain"), None);
        assert_eq!(simplified(r"\\?\Volume{1234}\x"), None);
        assert_eq!(simplified(r"\\?\C:\dir\nul"), None);
        assert_eq!(simplified(r"\\?\C:\dir\COM3.txt"), None);
        assert_eq!(simplified(r"\\?\C:\dir\console").as_deref(), Some(r"C:\dir\console"));
        assert_eq!(simplified(r"\\?\C:\a.\b"), None);
        assert_eq!(simplified(r"\\?\C:\a \b"), None);
        let long = format!(r"\\?\C:\{}", "x".repeat(300));
        assert_eq!(simplified(&long), None);
    }

    #[test]
    fn pathext_candidates_follow_the_extension_rules() {
        assert_eq!(executable_candidates("cargo", ".COM;.EXE;.BAT"), ["cargo.COM", "cargo.EXE", "cargo.BAT"]);
        assert_eq!(executable_candidates("cargo.exe", ".COM;.EXE"), ["cargo.exe"]);
        assert_eq!(executable_candidates("cargo", ".EXE;;"), ["cargo.EXE"]);
        assert!(executable_candidates("cargo", "").is_empty());
    }

    #[test]
    fn executables_are_found_through_pathext() {
        let dir = scratch("exec");
        fs::write(dir.join("tool.exe"), b"x").unwrap();
        fs::create_dir_all(dir.join("folder.exe")).unwrap();
        let found = find_executable_in(&dir, "tool").expect("found through PATHEXT");
        assert!(found.to_string_lossy().eq_ignore_ascii_case(&dir.join("tool.exe").to_string_lossy()));
        assert_eq!(find_executable_in(&dir, "tool.exe"), Some(dir.join("tool.exe")));
        assert_eq!(find_executable_in(&dir, "absent"), None);
        assert_eq!(find_executable_in(&dir, "folder"), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn tool_names_drop_exe_case_insensitively() {
        assert_eq!(tool_name(OsStr::new(r"C:\bin\fcargo.exe")), "fcargo");
        assert_eq!(tool_name(OsStr::new("FRUST.EXE")), "FRUST");
        assert_eq!(tool_name(OsStr::new("frust")), "frust");
        assert_eq!(tool_name(OsStr::new("tool.cmd")), "tool.cmd");
        assert_eq!(tool_name(OsStr::new(".exe")), "");
        assert_eq!(tool_name(OsStr::new("")), "");
    }

    #[test]
    fn locks_conflict_between_handles_but_not_readers() {
        let dir = scratch("lock");
        let path = dir.join("f");
        fs::write(&path, b"ticket").unwrap();
        let (a, b) = (File::open(&path).unwrap(), File::open(&path).unwrap());
        lock(&a, LockKind::Exclusive).unwrap();
        assert!(matches!(try_lock(&b, LockKind::Exclusive), Err(fs::TryLockError::WouldBlock)));
        assert!(matches!(try_lock(&b, LockKind::Shared), Err(fs::TryLockError::WouldBlock)));
        assert_eq!(fs::read(&path).unwrap(), b"ticket", "the sentinel byte leaves the content readable");
        unlock(&a).unwrap();
        try_lock(&b, LockKind::Shared).unwrap();
        try_lock(&a, LockKind::Shared).unwrap();
        assert!(matches!(try_lock(&File::open(&path).unwrap(), LockKind::Exclusive), Err(fs::TryLockError::WouldBlock)));
        unlock(&a).unwrap();
        unlock(&b).unwrap();
        unlock(&b).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn metadata_comes_from_the_handle() {
        let dir = scratch("meta");
        let file = dir.join("f");
        fs::write(&file, b"hello").unwrap();
        let (meta, ext) = stat(&file).unwrap();
        assert_eq!(meta.len(), 5);
        assert_eq!(ext.nlink(&meta), 1);
        assert!(ext.ino(&meta) != 0 && ext.dev(&meta) != 0);
        assert_eq!(ext.mode(&meta), 0o100644);
        fs::hard_link(&file, dir.join("g")).unwrap();
        let (meta, ext) = lstat(&file).unwrap();
        assert_eq!(ext.nlink(&meta), 2);
        let open = File::open(&file).unwrap();
        let (open_meta, open_ext) = fstat(&open).unwrap();
        assert_eq!((open_ext.dev(&open_meta), open_ext.ino(&open_meta)), (ext.dev(&meta), ext.ino(&meta)));
        let (dir_meta, dir_ext) = stat(&dir).unwrap();
        assert_eq!(dir_ext.mode(&dir_meta), 0o040755);
        assert!(stat(&dir.join("absent")).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ownership_and_private_dirs() {
        let dir = scratch("owner");
        let private = dir.join("p");
        create_dir_with_mode(&private, 0o700).unwrap();
        assert!(owned_by_me(&private));
        assert_eq!(verify_private_dir(&private), Ok(()));
        fs::write(dir.join("file"), b"x").unwrap();
        assert!(verify_private_dir(&dir.join("file")).unwrap_err().contains("it is not a directory"));
        assert!(verify_private_dir(&dir.join("absent")).unwrap_err().starts_with("cannot inspect "));
        assert!(!owned_by_me(&dir.join("absent")));
        assert!(user_id() != 0 || user_name().is_some());
        assert!(pid_alive(std::process::id()));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn link_tool_refuses_to_replace_an_existing_entry() {
        let dir = scratch("link");
        let exe = dir.join("frust.exe");
        fs::write(&exe, b"x").unwrap();
        link_tool(&exe, &dir.join("fcargo.exe")).unwrap();
        assert_eq!(fs::read(dir.join("fcargo.exe")).unwrap(), b"x");
        assert_eq!(link_tool(&exe, &dir.join("fcargo.exe")).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reflink_clones_or_leaves_nothing() {
        let dir = scratch("reflink");
        let src = dir.join("src");
        let dst = dir.join("dst");
        fs::write(&src, b"clone me").unwrap();
        match reflink(&src, &dst) {
            Ok(()) => assert_eq!(fs::read(&dst).unwrap(), b"clone me"),
            Err(_) => assert!(!dst.exists()),
        }
        assert!(reflink(&dir, &dir.join("d2")).is_err());
        let _ = first_extent(&src);
        assert!(free_space(&dir).unwrap() > 0);
        assert!(free_space(&src).unwrap() > 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn pipe_reads_never_block() {
        let (reader, mut writer) = io::pipe().unwrap();
        let mut file = pipe_prepare(File::from(OwnedHandle::from(reader)));
        let mut buf = [0u8; 16];
        assert_eq!(pipe_read(&mut file, &mut buf).unwrap_err().kind(), io::ErrorKind::WouldBlock);
        assert_eq!(wait_readable([Some(&file), None], Duration::from_millis(10)), [false, false]);
        writer.write_all(b"hi").unwrap();
        assert_eq!(wait_readable([None, Some(&file)], Duration::from_secs(5)), [false, true]);
        assert_eq!(pipe_read(&mut file, &mut buf[..1]).unwrap(), 1);
        assert_eq!(pipe_read(&mut file, &mut buf[1..]).unwrap(), 1);
        assert_eq!(&buf[..2], b"hi");
        drop(writer);
        assert_eq!(wait_readable([Some(&file), None], Duration::from_secs(5)), [true, false]);
        assert_eq!(pipe_read(&mut file, &mut buf).unwrap(), 0);
    }
}

// wrapper
// Handles are never renumbered onto the standard streams, and nothing is inherited here anyway.
pub fn inheritable_clear_of_stdio(_files: &[&File]) -> bool {
    true
}

// compact and hashing
pub fn lower_thread_priority() {
    use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority};
    const THREAD_MODE_BACKGROUND_BEGIN: i32 = 0x0001_0000;
    // SAFETY: the pseudo handle of the current thread is always valid; failure only leaves the priority unchanged.
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) };
}

pub fn fs_type_name(path: &Path) -> Option<String> {
    use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetVolumeInformationW};
    const DRIVE_UNKNOWN: u32 = 0;
    const DRIVE_NO_ROOT_DIR: u32 = 1;
    const DRIVE_REMOTE: u32 = 4;
    // GetVolumePathNameW accepts paths that do not exist, which must not read as a trusted volume.
    fs::metadata(path).ok()?;
    let full = wide(&std::path::absolute(path).ok()?);
    let mut root = [0u16; 261];
    // SAFETY: `full` is NUL-terminated and `root` has the capacity passed.
    if unsafe { GetVolumePathNameW(full.as_ptr(), root.as_mut_ptr(), root.len() as u32) } == 0 {
        return None;
    }
    // SAFETY: `root` is NUL-terminated.
    if matches!(unsafe { GetDriveTypeW(root.as_ptr()) }, DRIVE_UNKNOWN | DRIVE_NO_ROOT_DIR | DRIVE_REMOTE) {
        return None;
    }
    let mut name = [0u16; 64];
    // SAFETY: `root` is NUL-terminated and the name buffer has the capacity passed; the other outputs are not requested.
    let ok = unsafe {
        GetVolumeInformationW(root.as_ptr(), null_mut(), 0, null_mut(), null_mut(), null_mut(), name.as_mut_ptr(), name.len() as u32)
    };
    if ok == 0 {
        return None;
    }
    let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    let text = String::from_utf16_lossy(&name[..len]).to_ascii_lowercase();
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod compact_hashing_tests {
    use super::*;

    #[test]
    fn lowering_priority_on_a_spawned_thread_returns() {
        std::thread::spawn(lower_thread_priority).join().unwrap();
    }

    #[test]
    fn the_filesystem_of_a_directory_has_a_lowercase_name_and_a_missing_path_has_none() {
        let dir = std::env::temp_dir();
        if let Some(name) = fs_type_name(&dir) {
            assert!(!name.is_empty() && name == name.to_ascii_lowercase(), "{name}");
        }
        assert_eq!(fs_type_name(&dir.join("frust-sys-no-such-path").join("x")), None);
    }
}
