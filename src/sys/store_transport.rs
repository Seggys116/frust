use std::fs::File;
use std::io;
use std::path::Path;
use std::process::Command;
use std::time::Instant;

#[derive(Clone, Copy)]
enum Role {
    Listener,
    Connection,
    Control,
}

pub(crate) struct Listener(File);
// One caller frames each direction; discard the connection after any send or receive error.
pub(crate) struct Connection(File);
pub(crate) struct ControlReader(File);
pub(crate) struct ControlWriter(File);

macro_rules! handles {
    ($ty:ident, $role:expr) => {
        impl $ty {
            pub(crate) fn as_file(&self) -> &File {
                &self.0
            }
            pub(crate) fn descriptor_tag(&self) -> String {
                imp::tag(&self.0)
            }
            /// # Safety
            /// The tag must identify a uniquely owned, whitelisted inherited descriptor.
            pub(crate) unsafe fn from_inherited(tag: &str) -> io::Result<Self> {
                // SAFETY: ownership is supplied by the caller; the backend validates the socket.
                unsafe { imp::inherited(tag, $role).map(Self) }
            }
        }
    };
}
handles!(Listener, Role::Listener);
handles!(Connection, Role::Connection);
handles!(ControlReader, Role::Control);

impl Listener {
    pub(crate) fn bind_private(path: &Path) -> io::Result<Self> {
        imp::bind(path).map(Self)
    }
    #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
    pub(crate) fn accept_until(&self, deadline: Instant) -> io::Result<Option<Connection>> {
        imp::accept(&self.0, None, deadline).map(|v| v.map(Connection))
    }
}

impl Connection {
    #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
    pub(crate) fn try_clone(&self) -> io::Result<Self> {
        self.0.try_clone().map(Self)
    }
    pub(crate) fn connect_verified(path: &Path, deadline: Instant) -> io::Result<Self> {
        imp::connect(path, deadline).map(Self)
    }
    pub(crate) fn send(
        &self,
        bytes: &[u8],
        descriptors: &[&File],
        deadline: Instant,
    ) -> io::Result<()> {
        imp::send(&self.0, bytes, descriptors, deadline)
    }
    pub(crate) fn receive(
        &self,
        max_bytes: usize,
        deadline: Instant,
    ) -> io::Result<(Vec<u8>, Vec<File>)> {
        imp::receive(&self.0, max_bytes, deadline)
    }
}

impl ControlReader {
    pub(crate) fn is_closed(&self) -> io::Result<bool> {
        imp::closed(&self.0)
    }
}

impl ControlWriter {
    pub(crate) fn close(self) {
        drop(self.0);
    }
}

pub(crate) fn control_pair() -> io::Result<(ControlReader, ControlWriter)> {
    imp::pair().map(|(r, w)| (ControlReader(r), ControlWriter(w)))
}

pub(crate) fn random_capability() -> io::Result<String> {
    imp::random_capability()
}

pub(crate) fn detach_broker_command(command: &mut Command, keep: &[&File]) -> io::Result<()> {
    imp::detach_broker_command(command, keep)
}

pub(crate) fn accept_or_closed(
    listener: &Listener,
    control: &ControlReader,
    deadline: Instant,
) -> io::Result<Option<Connection>> {
    imp::accept(&listener.0, Some(&control.0), deadline).map(|v| v.map(Connection))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod imp {
    use super::*;
    use std::fs;
    use std::io::Read;
    use std::mem::{self, size_of};
    use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::os::unix::process::CommandExt;

    fn invalid(message: &str) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, message)
    }
    fn timeout() -> io::Error {
        io::Error::new(io::ErrorKind::TimedOut, "store transport deadline elapsed")
    }
    struct Descriptor {
        fd: i32,
        dev: libc::dev_t,
        ino: libc::ino_t,
    }
    fn raw_stat(fd: i32) -> io::Result<libc::stat> {
        // SAFETY: fstat writes exactly one stat structure and does not take descriptor ownership.
        unsafe {
            let mut meta: libc::stat = mem::zeroed();
            if libc::fstat(fd, &mut meta) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(meta)
        }
    }
    fn descriptors() -> io::Result<Vec<Descriptor>> {
        #[cfg(target_os = "linux")]
        let directory = "/proc/self/fd";
        #[cfg(target_os = "macos")]
        let directory = "/dev/fd";
        let mut numbers = Vec::new();
        for entry in fs::read_dir(directory)? {
            if let Some(fd) = entry?
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<i32>().ok())
                .filter(|fd| *fd > 2)
            {
                numbers.push(fd);
            }
        }
        let mut observed = Vec::new();
        for fd in numbers {
            match raw_stat(fd) {
                Ok(meta) => observed.push(Descriptor {
                    fd,
                    dev: meta.st_dev,
                    ino: meta.st_ino,
                }),
                Err(error) if error.raw_os_error() == Some(libc::EBADF) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(observed)
    }
    pub(super) fn detach_broker_command(command: &mut Command, keep: &[&File]) -> io::Result<()> {
        let whitelist: Vec<i32> = keep.iter().map(|file| file.as_raw_fd()).collect();
        if whitelist.iter().any(|fd| *fd <= 2) {
            return Err(invalid("stdio cannot be a broker whitelist handle"));
        }
        for fd in &whitelist {
            raw_stat(*fd)?;
        }
        let observed = descriptors()?;
        super::super::detach_command(command, keep);
        // SAFETY: child work uses only fstat and close on the preallocated snapshot, preserving reused descriptors.
        unsafe {
            command.pre_exec(move || {
                for old in &observed {
                    if whitelist.contains(&old.fd) {
                        continue;
                    }
                    let current = match raw_stat(old.fd) {
                        Ok(current) => current,
                        Err(error) if error.raw_os_error() == Some(libc::EBADF) => continue,
                        Err(error) => return Err(error),
                    };
                    if (current.st_dev, current.st_ino) == (old.dev, old.ino)
                        && libc::close(old.fd) < 0
                    {
                        return Err(io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        Ok(())
    }
    pub(super) fn random_capability() -> io::Result<String> {
        let mut bytes = [0u8; 32];
        File::open("/dev/urandom")?.read_exact(&mut bytes)?;
        const HEX: &[u8; 16] = b"0123456789abcdef";
        Ok(bytes
            .into_iter()
            .flat_map(|b| {
                [
                    HEX[(b >> 4) as usize] as char,
                    HEX[(b & 15) as usize] as char,
                ]
            })
            .collect())
    }
    fn owned(fd: i32) -> io::Result<File> {
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: callers pass a newly created or received descriptor, transferring ownership once.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
    fn option(file: &File, name: i32) -> io::Result<i32> {
        let mut value = 0i32;
        let mut len = size_of::<i32>() as libc::socklen_t;
        // SAFETY: value and len describe writable storage of the advertised size.
        let rc = unsafe {
            libc::getsockopt(
                file.as_raw_fd(),
                libc::SOL_SOCKET,
                name,
                (&mut value as *mut i32).cast(),
                &mut len,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        if len as usize != size_of::<i32>() {
            return Err(invalid("invalid socket option length"));
        }
        Ok(value)
    }
    fn configure(file: &File) -> io::Result<()> {
        // SAFETY: fcntl operates on the live owned descriptor; existing status flags are preserved.
        unsafe {
            let fd = file.as_raw_fd();
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) < 0 {
                return Err(io::Error::last_os_error());
            }
            let status = libc::fcntl(fd, libc::F_GETFL);
            if status < 0 || libc::fcntl(fd, libc::F_SETFL, status | libc::O_NONBLOCK) < 0 {
                return Err(io::Error::last_os_error());
            }
            #[cfg(target_os = "macos")]
            {
                let yes = 1i32;
                if libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_NOSIGPIPE,
                    (&yes as *const i32).cast(),
                    size_of::<i32>() as _,
                ) < 0
                {
                    return Err(io::Error::last_os_error());
                }
            }
        }
        Ok(())
    }
    fn cloexec(file: &File) -> io::Result<()> {
        // SAFETY: descriptor-local CLOEXEC does not change shared open-file status or lock ownership.
        unsafe {
            let flags = libc::fcntl(file.as_raw_fd(), libc::F_GETFD);
            if flags < 0
                || libc::fcntl(file.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC) < 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }
    fn peer(file: &File) -> io::Result<()> {
        // SAFETY: credential calls receive correctly sized writable outputs and a live socket.
        unsafe {
            #[cfg(target_os = "linux")]
            let uid = {
                let mut cred: libc::ucred = mem::zeroed();
                let mut len = size_of::<libc::ucred>() as libc::socklen_t;
                if libc::getsockopt(
                    file.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_PEERCRED,
                    (&mut cred as *mut libc::ucred).cast(),
                    &mut len,
                ) < 0
                {
                    return Err(io::Error::last_os_error());
                }
                if len as usize != size_of::<libc::ucred>() {
                    return Err(invalid("invalid peer credentials"));
                }
                cred.uid
            };
            #[cfg(target_os = "macos")]
            let uid = {
                let mut uid = 0;
                let mut gid = 0;
                if libc::getpeereid(file.as_raw_fd(), &mut uid, &mut gid) < 0 {
                    return Err(io::Error::last_os_error());
                }
                uid
            };
            verify_uid(uid, libc::geteuid())?;
        }
        Ok(())
    }
    fn verify_uid(peer: libc::uid_t, own: libc::uid_t) -> io::Result<()> {
        if peer != own {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "store transport peer has another uid",
            ));
        }
        Ok(())
    }
    #[cfg(target_os = "linux")]
    fn listening(file: &File) -> io::Result<bool> {
        Ok(option(file, libc::SO_ACCEPTCONN)? != 0)
    }

    #[cfg(target_os = "macos")]
    mod darwin_socket_info {
        use super::*;

        // Public Darwin proc_info.h layout; the protocol union is not interpreted.
        #[repr(C)]
        struct ProcFileInfo {
            open_flags: u32,
            status: u32,
            offset: i64,
            kind: i32,
            guard_flags: u32,
        }

        #[repr(C)]
        struct VinfoStat {
            dev: u32,
            mode: u16,
            nlink: u16,
            ino: u64,
            uid: u32,
            gid: u32,
            atime: i64,
            atime_nsec: i64,
            mtime: i64,
            mtime_nsec: i64,
            ctime: i64,
            ctime_nsec: i64,
            birthtime: i64,
            birthtime_nsec: i64,
            size: i64,
            blocks: i64,
            block_size: i32,
            flags: u32,
            generation: u32,
            rdev: u32,
            spare: [i64; 2],
        }

        #[repr(C)]
        struct SocketPrefix {
            file: ProcFileInfo,
            stat: VinfoStat,
            socket: u64,
            pcb: u64,
            kind: i32,
            protocol: i32,
            family: i32,
            options: i16,
            linger: i16,
        }

        #[repr(C)]
        struct SocketFdInfo {
            prefix: SocketPrefix,
            tail: [u64; 75],
        }

        const _: () = {
            assert!(size_of::<ProcFileInfo>() == 24);
            assert!(size_of::<VinfoStat>() == 136);
            assert!(size_of::<SocketPrefix>() == 192);
            assert!(mem::offset_of!(SocketPrefix, options) == 188);
            assert!(size_of::<SocketFdInfo>() == 792);
        };

        pub(super) fn listening(file: &File) -> io::Result<bool> {
            // SAFETY: the zeroed public ABI record owns all 792 writable bytes; only our live fd is queried.
            let (info, count) = unsafe {
                let mut info: SocketFdInfo = mem::zeroed();
                let count = libc::proc_pidfdinfo(
                    libc::getpid(),
                    file.as_raw_fd(),
                    3,
                    (&mut info as *mut SocketFdInfo).cast(),
                    size_of::<SocketFdInfo>() as i32,
                );
                (info, count)
            };
            if count <= 0 {
                return Err(io::Error::last_os_error());
            }
            if count as usize != size_of::<SocketFdInfo>()
                || info.prefix.kind != libc::SOCK_STREAM
                || info.prefix.family != libc::AF_UNIX
            {
                return Err(invalid("invalid Darwin socket information"));
            }
            Ok(i32::from(info.prefix.options) & libc::SO_ACCEPTCONN != 0)
        }
    }

    #[cfg(target_os = "macos")]
    use darwin_socket_info::listening;

    fn validate(file: &File, role: Role) -> io::Result<()> {
        if option(file, libc::SO_TYPE)? != libc::SOCK_STREAM {
            return Err(invalid("expected stream socket"));
        }
        let listening = listening(file)?;
        if listening != matches!(role, Role::Listener) {
            return Err(invalid("incorrect socket role"));
        }
        // SAFETY: sockaddr_storage is large enough for any returned socket address.
        unsafe {
            let mut address: libc::sockaddr_storage = mem::zeroed();
            let mut len = size_of::<libc::sockaddr_storage>() as libc::socklen_t;
            if libc::getsockname(
                file.as_raw_fd(),
                (&mut address as *mut libc::sockaddr_storage).cast(),
                &mut len,
            ) < 0
            {
                return Err(io::Error::last_os_error());
            }
            if address.ss_family as i32 != libc::AF_UNIX {
                return Err(invalid("expected Unix socket"));
            }
        }
        if !listening {
            peer(file)?;
        }
        Ok(())
    }
    pub(super) fn tag(file: &File) -> String {
        file.as_raw_fd().to_string()
    }
    pub(super) unsafe fn inherited(tag: &str, role: Role) -> io::Result<File> {
        if tag.is_empty() || !tag.bytes().all(|b| b.is_ascii_digit()) {
            return Err(invalid("invalid descriptor tag"));
        }
        let fd: i32 = tag.parse().map_err(|_| invalid("invalid descriptor tag"))?;
        if fd <= 2 {
            return Err(invalid("stdio cannot be a store transport handle"));
        }
        // SAFETY: F_GETFD checks the supplied number without taking ownership or modifying it.
        if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let file = owned(fd)?;
        validate(&file, role)?;
        configure(&file)?;
        Ok(file)
    }
    fn private_parent(path: &Path) -> io::Result<()> {
        if !path.is_absolute() {
            return Err(invalid("socket path must be absolute"));
        }
        let parent = path
            .parent()
            .ok_or_else(|| invalid("socket has no parent"))?;
        super::super::verify_private_dir(parent).map_err(io::Error::other)
    }
    pub(super) fn bind(path: &Path) -> io::Result<File> {
        private_parent(path)?;
        let listener = UnixListener::bind(path)?;
        let file = owned(listener.into_raw_fd())?;
        configure(&file)?;
        Ok(file)
    }
    pub(super) fn pair() -> io::Result<(File, File)> {
        let (a, b) = UnixStream::pair()?;
        let a = owned(a.into_raw_fd())?;
        let b = owned(b.into_raw_fd())?;
        configure(&a)?;
        configure(&b)?;
        Ok((a, b))
    }
    fn poll(items: &mut [libc::pollfd], deadline: Instant) -> io::Result<bool> {
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let millis = remaining
                .as_millis()
                .saturating_add(u128::from(!remaining.subsec_nanos().is_multiple_of(1_000_000)))
                .min(i32::MAX as u128) as i32;
            // SAFETY: items is a valid mutable array and its exact length is supplied.
            let rc = unsafe { libc::poll(items.as_mut_ptr(), items.len() as _, millis) };
            if rc >= 0 {
                return Ok(rc > 0);
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
        }
    }
    fn ready(file: &File, events: i16, deadline: Instant) -> io::Result<()> {
        if Instant::now() >= deadline {
            return Err(timeout());
        }
        let mut items = [libc::pollfd {
            fd: file.as_raw_fd(),
            events,
            revents: 0,
        }];
        if !poll(&mut items, deadline)? {
            return Err(timeout());
        }
        if items[0].revents & libc::POLLNVAL != 0 {
            return Err(invalid("invalid transport descriptor"));
        }
        Ok(())
    }
    pub(super) fn connect(path: &Path, deadline: Instant) -> io::Result<File> {
        private_parent(path)?;
        let meta = fs::symlink_metadata(path)?;
        // SAFETY: geteuid has no preconditions.
        if !meta.file_type().is_socket() || meta.uid() != unsafe { libc::geteuid() } {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "untrusted store endpoint",
            ));
        }
        let bytes = path.as_os_str().as_bytes();
        // SAFETY: zeroed sockaddr_un is initialized below before connect reads it.
        let mut address: libc::sockaddr_un = unsafe { mem::zeroed() };
        if bytes.len() >= address.sun_path.len() {
            return Err(invalid("store endpoint path is too long"));
        }
        address.sun_family = libc::AF_UNIX as _;
        for (to, from) in address.sun_path.iter_mut().zip(bytes) {
            *to = *from as _;
        }
        #[cfg(target_os = "macos")]
        {
            address.sun_len = size_of::<libc::sockaddr_un>() as _;
        }
        // SAFETY: socket returns a new descriptor and connect receives a complete sockaddr_un.
        #[cfg(target_os = "linux")]
        let socket_type = libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK;
        #[cfg(target_os = "macos")]
        let socket_type = libc::SOCK_STREAM;
        let file = owned(unsafe { libc::socket(libc::AF_UNIX, socket_type, 0) })?;
        configure(&file)?;
        let rc = unsafe {
            libc::connect(
                file.as_raw_fd(),
                (&address as *const libc::sockaddr_un).cast(),
                size_of::<libc::sockaddr_un>() as _,
            )
        };
        if rc < 0 {
            let error = io::Error::last_os_error();
            if !matches!(error.raw_os_error(), Some(libc::EINPROGRESS | libc::EINTR)) {
                return Err(error);
            }
            ready(&file, libc::POLLOUT, deadline)?;
            let error = option(&file, libc::SO_ERROR)?;
            if error != 0 {
                return Err(io::Error::from_raw_os_error(error));
            }
        }
        peer(&file)?;
        Ok(file)
    }
    pub(super) fn closed(file: &File) -> io::Result<bool> {
        let mut byte = 0u8;
        loop {
            // SAFETY: one writable byte is supplied and MSG_PEEK preserves the control stream.
            let n = unsafe {
                libc::recv(
                    file.as_raw_fd(),
                    (&mut byte as *mut u8).cast(),
                    1,
                    libc::MSG_PEEK | libc::MSG_DONTWAIT,
                )
            };
            if n == 0 {
                return Ok(true);
            }
            if n > 0 {
                return Err(invalid("unexpected control payload"));
            }
            let error = io::Error::last_os_error();
            match error.kind() {
                io::ErrorKind::WouldBlock => return Ok(false),
                io::ErrorKind::Interrupted => continue,
                _ => return Err(error),
            }
        }
    }
    pub(super) fn accept(
        listener: &File,
        control: Option<&File>,
        deadline: Instant,
    ) -> io::Result<Option<File>> {
        loop {
            if let Some(control) = control
                && closed(control)?
            {
                return Ok(None);
            }
            let mut items = vec![libc::pollfd {
                fd: listener.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            }];
            if let Some(control) = control {
                items.push(libc::pollfd {
                    fd: control.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                });
            }
            if !poll(&mut items, deadline)? {
                return Ok(None);
            }
            if let Some(control) = control
                && closed(control)?
            {
                return Ok(None);
            }
            if items.iter().any(|p| p.revents & libc::POLLNVAL != 0) {
                return Err(invalid("invalid accept descriptor"));
            }
            if items[0].revents == 0 {
                continue;
            }
            // SAFETY: no peer address is requested; a successful accept returns a new descriptor.
            #[cfg(target_os = "linux")]
            let fd = unsafe {
                libc::accept4(
                    listener.as_raw_fd(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                )
            };
            #[cfg(target_os = "macos")]
            let fd = unsafe {
                libc::accept(
                    listener.as_raw_fd(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if fd >= 0 {
                let file = owned(fd)?;
                configure(&file)?;
                peer(&file)?;
                return Ok(Some(file));
            }
            let error = io::Error::last_os_error();
            if !matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) {
                return Err(error);
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
        }
    }
    #[cfg(target_os = "linux")]
    const SEND_FLAGS: i32 = libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT;
    #[cfg(target_os = "macos")]
    const SEND_FLAGS: i32 = libc::MSG_DONTWAIT;

    pub(super) fn send(
        file: &File,
        bytes: &[u8],
        descriptors: &[&File],
        deadline: Instant,
    ) -> io::Result<()> {
        if descriptors.len() > 2 {
            return Err(invalid("at most two descriptors may be sent"));
        }
        let mut marker = descriptors.len() as u8;
        let mut ancillary = [0usize; 8];
        // SAFETY: all message pointers refer to live buffers and CMSG storage is usize-aligned.
        unsafe {
            let mut vector = libc::iovec {
                iov_base: (&mut marker as *mut u8).cast(),
                iov_len: 1,
            };
            let mut message: libc::msghdr = mem::zeroed();
            message.msg_iov = &mut vector;
            message.msg_iovlen = 1;
            if !descriptors.is_empty() {
                message.msg_control = ancillary.as_mut_ptr().cast();
                message.msg_controllen =
                    libc::CMSG_SPACE((descriptors.len() * size_of::<i32>()) as _) as _;
                let header = libc::CMSG_FIRSTHDR(&message);
                (*header).cmsg_level = libc::SOL_SOCKET;
                (*header).cmsg_type = libc::SCM_RIGHTS;
                (*header).cmsg_len =
                    libc::CMSG_LEN((descriptors.len() * size_of::<i32>()) as _) as _;
                for (index, descriptor) in descriptors.iter().enumerate() {
                    std::ptr::write_unaligned(
                        libc::CMSG_DATA(header).cast::<i32>().add(index),
                        descriptor.as_raw_fd(),
                    );
                }
            }
            loop {
                ready(file, libc::POLLOUT, deadline)?;
                let n = libc::sendmsg(file.as_raw_fd(), &message, SEND_FLAGS);
                if n == 1 {
                    break;
                }
                if n == 0 {
                    return Err(io::Error::from(io::ErrorKind::WriteZero));
                }
                let error = io::Error::last_os_error();
                if !matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) {
                    return Err(error);
                }
            }
        }
        let len = u64::try_from(bytes.len())
            .map_err(|_| invalid("frame is too large"))?
            .to_be_bytes();
        for mut rest in [len.as_slice(), bytes] {
            while !rest.is_empty() {
                ready(file, libc::POLLOUT, deadline)?;
                // SAFETY: rest is readable for its full length; SIGPIPE is suppressed per platform.
                let n = unsafe {
                    libc::send(
                        file.as_raw_fd(),
                        rest.as_ptr().cast(),
                        rest.len(),
                        SEND_FLAGS,
                    )
                };
                if n > 0 {
                    rest = &rest[n as usize..];
                    continue;
                }
                if n == 0 {
                    return Err(io::Error::from(io::ErrorKind::WriteZero));
                }
                let error = io::Error::last_os_error();
                if !matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) {
                    return Err(error);
                }
            }
        }
        Ok(())
    }
    #[cfg(target_os = "linux")]
    const RECEIVE_CONTROL_WORDS: usize = 32;
    // XNU installs rights before truncating copyout; one MCLBYTES cluster covers its admitted control record.
    #[cfg(target_os = "macos")]
    const RECEIVE_CONTROL_WORDS: usize = 2048 / size_of::<usize>();
    #[cfg(target_os = "macos")]
    const _: () = {
        // SAFETY: CMSG_SPACE only computes alignment for this bounded payload length.
        let maximum = unsafe { libc::CMSG_SPACE((254 * size_of::<i32>()) as _) };
        assert!(maximum as usize <= RECEIVE_CONTROL_WORDS * size_of::<usize>());
    };

    fn receive_chunk(
        file: &File,
        bytes: &mut [u8],
        deadline: Instant,
    ) -> io::Result<(usize, Vec<File>)> {
        loop {
            ready(file, libc::POLLIN, deadline)?;
            let mut ancillary = [0usize; RECEIVE_CONTROL_WORDS];
            let mut received = Vec::new();
            // SAFETY: recvmsg uses bounded writable buffers; each received SCM_RIGHTS fd is adopted once.
            unsafe {
                let mut vector = libc::iovec {
                    iov_base: bytes.as_mut_ptr().cast(),
                    iov_len: bytes.len(),
                };
                let mut message: libc::msghdr = mem::zeroed();
                message.msg_iov = &mut vector;
                message.msg_iovlen = 1;
                message.msg_control = ancillary.as_mut_ptr().cast();
                message.msg_controllen = std::mem::size_of_val(&ancillary) as _;
                #[cfg(target_os = "linux")]
                let flags = libc::MSG_DONTWAIT | libc::MSG_CMSG_CLOEXEC;
                #[cfg(target_os = "macos")]
                let flags = libc::MSG_DONTWAIT;
                let n = libc::recvmsg(file.as_raw_fd(), &mut message, flags);
                if n < 0 {
                    let error = io::Error::last_os_error();
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) {
                        continue;
                    }
                    return Err(error);
                }
                let mut malformed = message.msg_flags & (libc::MSG_CTRUNC | libc::MSG_TRUNC) != 0;
                let mut header = libc::CMSG_FIRSTHDR(&message);
                while !header.is_null() {
                    let length = (*header).cmsg_len as usize;
                    let base = libc::CMSG_LEN(0) as usize;
                    let offset = (header as usize).saturating_sub(message.msg_control as usize);
                    if length < base
                        || offset
                            .checked_add(length)
                            .is_none_or(|end| end > message.msg_controllen as usize)
                    {
                        malformed = true;
                        break;
                    }
                    if (*header).cmsg_level == libc::SOL_SOCKET
                        && (*header).cmsg_type == libc::SCM_RIGHTS
                    {
                        let payload = length - base;
                        malformed |= !payload.is_multiple_of(size_of::<i32>());
                        for index in 0..payload / size_of::<i32>() {
                            let fd = std::ptr::read_unaligned(
                                libc::CMSG_DATA(header).cast::<i32>().add(index),
                            );
                            received.push(owned(fd)?);
                        }
                    } else {
                        malformed = true;
                    }
                    header = libc::CMSG_NXTHDR(&message, header);
                }
                // macOS sets CLOEXEC immediately here; broker workers must not concurrently spawn or exec.
                for descriptor in &received {
                    cloexec(descriptor)?;
                }
                if malformed {
                    return Err(invalid("malformed or truncated ancillary data"));
                }
                if n == 0 {
                    return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
                }
                return Ok((n as usize, received));
            }
        }
    }
    fn receive_exact(file: &File, mut bytes: &mut [u8], deadline: Instant) -> io::Result<()> {
        while !bytes.is_empty() {
            let (n, descriptors) = receive_chunk(file, bytes, deadline)?;
            if !descriptors.is_empty() {
                return Err(invalid("unexpected payload descriptors"));
            }
            bytes = &mut bytes[n..];
        }
        Ok(())
    }
    pub(super) fn receive(
        file: &File,
        max_bytes: usize,
        deadline: Instant,
    ) -> io::Result<(Vec<u8>, Vec<File>)> {
        let mut marker = [0u8];
        let (_, descriptors) = receive_chunk(file, &mut marker, deadline)?;
        if marker[0] > 2 || descriptors.len() != marker[0] as usize {
            return Err(invalid("incorrect descriptor count"));
        }
        let mut length = [0u8; 8];
        receive_exact(file, &mut length, deadline)?;
        let length = usize::try_from(u64::from_be_bytes(length))
            .map_err(|_| invalid("frame is too large"))?;
        if length > max_bytes {
            return Err(invalid("frame exceeds admission limit"));
        }
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(length).map_err(io::Error::other)?;
        bytes.resize(length, 0);
        receive_exact(file, &mut bytes, deadline)?;
        Ok((bytes, descriptors))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::fs::OpenOptions;
        use std::os::unix::fs::PermissionsExt;
        use std::path::PathBuf;
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::Duration;

        struct Scratch(PathBuf);
        impl Scratch {
            fn new() -> Self {
                static NEXT: AtomicU64 = AtomicU64::new(0);
                let base = crate::paths::private_temp_dir(Path::new("/tmp"))
                    .expect("private short socket scratch");
                let path = base.join(format!(
                    "frust-st-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                fs::create_dir(&path).unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
                Self(path)
            }
            fn locked(&self) -> (File, PathBuf) {
                let path = self.0.join("key");
                let file = OpenOptions::new()
                    .create_new(true)
                    .read(true)
                    .write(true)
                    .open(&path)
                    .unwrap();
                file.lock().unwrap();
                (file, path)
            }
        }
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        fn deadline() -> Instant {
            Instant::now() + Duration::from_secs(2)
        }
        fn assert_unlocked(path: &Path) {
            File::open(path).unwrap().try_lock().unwrap();
        }
        fn raw_marker(file: &File, marker: u8, descriptors: &[&File]) {
            let mut marker = marker;
            let mut storage = [0usize; 64];
            // SAFETY: test ancillary buffers are aligned, bounded, and contain live descriptors.
            unsafe {
                let mut vector = libc::iovec {
                    iov_base: (&mut marker as *mut u8).cast(),
                    iov_len: 1,
                };
                let mut message: libc::msghdr = mem::zeroed();
                message.msg_iov = &mut vector;
                message.msg_iovlen = 1;
                if !descriptors.is_empty() {
                    let size = descriptors.len() * size_of::<i32>();
                    message.msg_control = storage.as_mut_ptr().cast();
                    message.msg_controllen = libc::CMSG_SPACE(size as _) as _;
                    assert!(message.msg_controllen as usize <= std::mem::size_of_val(&storage));
                    let header = libc::CMSG_FIRSTHDR(&message);
                    (*header).cmsg_level = libc::SOL_SOCKET;
                    (*header).cmsg_type = libc::SCM_RIGHTS;
                    (*header).cmsg_len = libc::CMSG_LEN(size as _) as _;
                    for (i, fd) in descriptors.iter().enumerate() {
                        std::ptr::write_unaligned(
                            libc::CMSG_DATA(header).cast::<i32>().add(i),
                            fd.as_raw_fd(),
                        );
                    }
                }
                assert_eq!(libc::sendmsg(file.as_raw_fd(), &message, SEND_FLAGS), 1);
            }
        }

        #[test]
        fn frames_preserve_order_zero_and_two_descriptors_and_lock_ownership() {
            if crate::isolated_test(module_path!(), "frames_preserve_order_zero_and_two_descriptors_and_lock_ownership") {
                return;
            }
            let scratch = Scratch::new();
            let (key, path) = scratch.locked();
            let (sender, receiver) = pair().unwrap();
            let original_flags = unsafe { libc::fcntl(key.as_raw_fd(), libc::F_GETFL) };
            send(&sender, b"job", &[&key, &key], deadline()).unwrap();
            send(&sender, b"response", &[], deadline()).unwrap();
            let (body, files) = receive(&receiver, 16, deadline()).unwrap();
            assert_eq!(body, b"job");
            assert_eq!(files.len(), 2);
            for file in &files {
                assert_ne!(
                    unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
                    0
                );
                assert_eq!(
                    unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) },
                    original_flags
                );
            }
            drop(key);
            assert!(File::open(&path).unwrap().try_lock().is_err());
            drop(files);
            assert_unlocked(&path);
            let (body, files) = receive(&receiver, 16, deadline()).unwrap();
            assert_eq!(body, b"response");
            assert!(files.is_empty());
        }

        #[test]
        fn oversize_frames_and_incomplete_headers_close_received_descriptors() {
            if crate::isolated_test(module_path!(), "oversize_frames_and_incomplete_headers_close_received_descriptors") {
                return;
            }
            for incomplete in [false, true] {
                let scratch = Scratch::new();
                let (key, path) = scratch.locked();
                let (sender, receiver) = pair().unwrap();
                if incomplete {
                    raw_marker(&sender, 1, &[&key]);
                } else {
                    send(&sender, b"too large", &[&key], deadline()).unwrap();
                }
                drop(key);
                drop(sender);
                let error = receive(&receiver, 1, deadline()).unwrap_err();
                assert_eq!(
                    error.kind(),
                    if incomplete {
                        io::ErrorKind::UnexpectedEof
                    } else {
                        io::ErrorKind::InvalidData
                    }
                );
                assert_unlocked(&path);
            }
        }

        #[test]
        fn unexpected_descriptor_counts_and_truncation_close_every_received_fd() {
            if crate::isolated_test(module_path!(), "unexpected_descriptor_counts_and_truncation_close_every_received_fd") {
                return;
            }
            // Linux closes truncated tails; Darwin must receive all rights before rejecting the count.
            for count in [1, 3, 100].into_iter().cycle().take(9) {
                let scratch = Scratch::new();
                let (key, path) = scratch.locked();
                let (sender, receiver) = pair().unwrap();
                raw_marker(&sender, 0, &vec![&key; count]);
                drop(key);
                assert_eq!(
                    receive(&receiver, 16, deadline()).unwrap_err().kind(),
                    io::ErrorKind::InvalidData
                );
                File::open(&path).unwrap().try_lock().unwrap_or_else(|error| {
                    panic!("received descriptors leaked for count {count}: {error}")
                });
            }
        }

        #[test]
        fn descriptors_hidden_in_length_are_rejected_and_closed() {
            if crate::isolated_test(module_path!(), "descriptors_hidden_in_length_are_rejected_and_closed") {
                return;
            }
            let scratch = Scratch::new();
            let (key, path) = scratch.locked();
            let (sender, receiver) = pair().unwrap();
            raw_marker(&sender, 0, &[]);
            raw_marker(&sender, 0, &[&key]);
            drop(key);
            assert_eq!(
                receive(&receiver, 16, deadline()).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
            assert_unlocked(&path);
        }

        #[test]
        fn stalled_receive_and_send_are_bounded_by_deadlines() {
            let (sender, receiver) = pair().unwrap();
            assert_eq!(
                receive(&receiver, 16, Instant::now()).unwrap_err().kind(),
                io::ErrorKind::TimedOut
            );
            assert_eq!(
                send(&sender, b"", &[], Instant::now()).unwrap_err().kind(),
                io::ErrorKind::TimedOut
            );
            assert_eq!(
                send(&sender, b"", &[&sender, &sender, &sender], deadline())
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidData
            );
        }

        #[test]
        fn private_endpoint_checks_peer_credentials_and_control_eof_interrupts_accept() {
            let scratch = Scratch::new();
            let path = scratch.0.join("sock");
            let listener = Listener::bind_private(&path).unwrap();
            let client = Connection::connect_verified(&path, deadline()).unwrap();
            let server = listener.accept_until(deadline()).unwrap().unwrap();
            client.send(b"hello", &[], deadline()).unwrap();
            assert_eq!(server.receive(16, deadline()).unwrap().0, b"hello");
            let (reader, writer) = control_pair().unwrap();
            assert!(!reader.is_closed().unwrap());
            writer.close();
            assert!(reader.is_closed().unwrap());
            assert!(
                accept_or_closed(&listener, &reader, deadline())
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                verify_uid(42, 43).unwrap_err().kind(),
                io::ErrorKind::PermissionDenied
            );
            fs::set_permissions(&scratch.0, fs::Permissions::from_mode(0o755)).unwrap();
            assert!(Connection::connect_verified(&path, deadline()).is_err());
        }

        #[test]
        fn socket_roles_require_listening_or_connected_state() {
            let scratch = Scratch::new();
            let listener = bind(&scratch.0.join("role-socket")).unwrap();
            assert!(validate(&listener, Role::Listener).is_ok());
            assert!(validate(&listener, Role::Connection).is_err());
            let (connection, _peer) = pair().unwrap();
            assert!(validate(&connection, Role::Connection).is_ok());
            assert!(validate(&connection, Role::Control).is_ok());
            assert!(validate(&connection, Role::Listener).is_err());
            // SAFETY: socket returns a new owned descriptor, without a peer or listening state.
            let unconnected = owned(unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) }).unwrap();
            assert!(validate(&unconnected, Role::Listener).is_err());
            assert!(validate(&unconnected, Role::Connection).is_err());
            assert!(validate(&unconnected, Role::Control).is_err());
            let path = scratch.0.join("bound-not-listening");
            // SAFETY: the address is initialized completely before bind reads it.
            let mut address: libc::sockaddr_un = unsafe { mem::zeroed() };
            address.sun_family = libc::AF_UNIX as _;
            let bytes = path.as_os_str().as_bytes();
            assert!(bytes.len() < address.sun_path.len());
            for (to, from) in address.sun_path.iter_mut().zip(bytes) {
                *to = *from as _;
            }
            #[cfg(target_os = "macos")]
            {
                address.sun_len = size_of::<libc::sockaddr_un>() as _;
            }
            // SAFETY: bind borrows the live descriptor and the initialized bounded address.
            assert_eq!(
                unsafe {
                    libc::bind(
                        unconnected.as_raw_fd(),
                        (&address as *const libc::sockaddr_un).cast(),
                        size_of::<libc::sockaddr_un>() as _,
                    )
                },
                0
            );
            assert!(validate(&unconnected, Role::Listener).is_err());
            assert!(validate(&unconnected, Role::Connection).is_err());
            assert!(validate(&unconnected, Role::Control).is_err());
        }

        #[test]
        fn inherited_handles_reject_stdio_non_sockets_and_wrong_roles() {
            let scratch = Scratch::new();
            let (file, _) = scratch.locked();
            let number = file.into_raw_fd();
            assert!(unsafe { Connection::from_inherited(&number.to_string()) }.is_err());
            for tag in ["0", "2", "-1", "+4", "bad"] {
                assert!(unsafe { Connection::from_inherited(tag) }.is_err());
            }
            let listener = bind(&scratch.0.join("sock")).unwrap();
            let number = listener.into_raw_fd();
            assert!(unsafe { Connection::from_inherited(&number.to_string()) }.is_err());
            let (a, _b) = pair().unwrap();
            let number = a.into_raw_fd();
            let connection = unsafe { Connection::from_inherited(&number.to_string()) }.unwrap();
            assert_eq!(connection.descriptor_tag(), number.to_string());
            assert_eq!(
                option(connection.as_file(), libc::SO_TYPE).unwrap(),
                libc::SOCK_STREAM
            );
            let _clone = connection.try_clone().unwrap();
        }

        #[test]
        fn capabilities_are_full_length_random_hex() {
            let first = random_capability().unwrap();
            let second = random_capability().unwrap();
            assert_eq!(first.len(), 64);
            assert!(
                first
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            );
            assert_ne!(first, second);
        }

        #[test]
        fn detached_child_handles() {
            if std::env::var_os("FRUST_STORE_TRANSPORT_CHILD").is_none() {
                return;
            }
            let control_tag = std::env::var("FRUST_STORE_TRANSPORT_CONTROL").unwrap();
            // SAFETY: this subprocess receives unique descriptor ownership through the explicit whitelist.
            let control = unsafe { Connection::from_inherited(&control_tag) }.unwrap();
            let key_fd: i32 = std::env::var("FRUST_STORE_TRANSPORT_KEY")
                .unwrap()
                .parse()
                .unwrap();
            assert!(key_fd > 2);
            let key = owned(key_fd).unwrap();
            let unrelated: i32 = std::env::var("FRUST_STORE_TRANSPORT_UNRELATED")
                .unwrap()
                .parse()
                .unwrap();
            let dev: libc::dev_t = std::env::var("FRUST_STORE_TRANSPORT_DEV")
                .unwrap()
                .parse()
                .unwrap();
            let ino: libc::ino_t = std::env::var("FRUST_STORE_TRANSPORT_INO")
                .unwrap()
                .parse()
                .unwrap();
            assert!(
                !raw_stat(unrelated).is_ok_and(|meta| (meta.st_dev, meta.st_ino) == (dev, ino))
            );
            let expected_status: i32 = std::env::var("FRUST_STORE_TRANSPORT_STATUS")
                .unwrap()
                .parse()
                .unwrap();
            assert_eq!(
                unsafe { libc::fcntl(key.as_raw_fd(), libc::F_GETFL) },
                expected_status
            );
            control.send(b"ready", &[&key], deadline()).unwrap();
            assert_eq!(control.receive(16, deadline()).unwrap().0, b"exit");
        }

        #[test]
        fn broker_detachment_closes_unrelated_inherited_pipe_and_preserves_parent_and_whitelist() {
            if crate::isolated_test(module_path!(), "broker_detachment_closes_unrelated_inherited_pipe_and_preserves_parent_and_whitelist") {
                return;
            }
            let scratch = Scratch::new();
            let (key, key_path) = scratch.locked();
            let key_status = unsafe { libc::fcntl(key.as_raw_fd(), libc::F_GETFL) };
            let mut pipe = [-1; 2];
            // SAFETY: pipe writes two new descriptor numbers to an appropriately sized array.
            assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
            let read = owned(pipe[0]).unwrap();
            let write = owned(pipe[1]).unwrap();
            let observed = raw_stat(read.as_raw_fd()).unwrap();
            assert_eq!(
                unsafe { libc::fcntl(read.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
                0
            );
            let (child_control, parent_control) = pair().unwrap();
            let parent_control = Connection(parent_control);
            let mut command = Command::new(std::env::current_exe().unwrap());
            command.args([
                "--exact",
                "sys::store_transport::imp::tests::detached_child_handles",
            ]);
            command
                .env("FRUST_STORE_TRANSPORT_CHILD", "1")
                .env("FRUST_STORE_TRANSPORT_CONTROL", tag(&child_control))
                .env("FRUST_STORE_TRANSPORT_KEY", tag(&key))
                .env("FRUST_STORE_TRANSPORT_UNRELATED", tag(&read))
                .env("FRUST_STORE_TRANSPORT_DEV", observed.st_dev.to_string())
                .env("FRUST_STORE_TRANSPORT_INO", observed.st_ino.to_string())
                .env("FRUST_STORE_TRANSPORT_STATUS", key_status.to_string());
            detach_broker_command(&mut command, &[&child_control, &key]).unwrap();
            let mut child = command.spawn().unwrap();
            drop(child_control);
            drop(key);
            let (message, received) = parent_control.receive(16, deadline()).unwrap();
            assert_eq!(message, b"ready");
            assert_eq!(received.len(), 1);
            assert!(File::open(&key_path).unwrap().try_lock().is_err());
            let current = raw_stat(read.as_raw_fd()).unwrap();
            assert_eq!(
                (current.st_dev, current.st_ino),
                (observed.st_dev, observed.st_ino)
            );
            let mut writer = &write;
            std::io::Write::write_all(&mut writer, b"x").unwrap();
            let mut reader = &read;
            let mut byte = [0u8];
            reader.read_exact(&mut byte).unwrap();
            assert_eq!(byte, [b'x']);
            parent_control.send(b"exit", &[], deadline()).unwrap();
            assert!(child.wait().unwrap().success());
            drop(received);
            assert_unlocked(&key_path);
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod imp {
    use super::*;
    fn unsupported<T>() -> io::Result<T> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "store descriptor transport is unsupported",
        ))
    }
    pub(super) fn tag(_: &File) -> String {
        String::new()
    }
    pub(super) fn random_capability() -> io::Result<String> {
        unsupported()
    }
    pub(super) fn detach_broker_command(_: &mut Command, _: &[&File]) -> io::Result<()> {
        unsupported()
    }
    pub(super) unsafe fn inherited(_: &str, _: Role) -> io::Result<File> {
        unsupported()
    }
    pub(super) fn bind(_: &Path) -> io::Result<File> {
        unsupported()
    }
    pub(super) fn pair() -> io::Result<(File, File)> {
        unsupported()
    }
    pub(super) fn connect(_: &Path, _: Instant) -> io::Result<File> {
        unsupported()
    }
    pub(super) fn closed(_: &File) -> io::Result<bool> {
        unsupported()
    }
    pub(super) fn accept(_: &File, _: Option<&File>, _: Instant) -> io::Result<Option<File>> {
        unsupported()
    }
    pub(super) fn send(_: &File, _: &[u8], _: &[&File], _: Instant) -> io::Result<()> {
        unsupported()
    }
    pub(super) fn receive(_: &File, _: usize, _: Instant) -> io::Result<(Vec<u8>, Vec<File>)> {
        unsupported()
    }
}
