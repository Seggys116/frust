//! A stderr keeper compensates a lent Cargo slot before Cargo can reclaim its assigned slot.

use std::ffi::OsString;
use std::io;
use std::path::Path;

const HELPER: &str = "--jobserver-loan-keeper";

#[derive(Debug)]
pub(crate) enum LendError {
    Unavailable(io::Error),
    #[cfg(unix)]
    Uncertain(io::Error),
}

impl std::fmt::Display for LendError {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(error) => write!(out, "jobserver loan unavailable: {error}"),
            #[cfg(unix)]
            Self::Uncertain(error) => write!(out, "jobserver repayment uncertain: {error}"),
        }
    }
}

impl std::error::Error for LendError {}

#[cfg(unix)]
pub(crate) use native::{Loan, direct_cargo_parent, lend};

#[cfg(not(unix))]
pub(crate) struct Loan;

#[cfg(not(unix))]
pub(crate) fn lend(_helper: &Path) -> Result<Loan, LendError> {
    Err(LendError::Unavailable(io::Error::new(io::ErrorKind::Unsupported, "jobserver loans require Unix")))
}

#[cfg(not(unix))]
impl Loan {
    pub(crate) fn resume(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(not(unix))]
pub(crate) fn direct_cargo_parent(_supervisor: u32, _cargo: &Path) -> bool {
    false
}

pub(crate) fn helper_entry(args: &[OsString]) -> Option<i32> {
    if args.first().is_none_or(|arg| arg != HELPER) {
        return None;
    }
    #[cfg(unix)]
    if let [_, read, write] = args {
        let parse = |value: &OsString| value.to_str().and_then(|value| value.parse::<i32>().ok());
        if let (Some(read), Some(write)) = (parse(read), parse(write)) {
            return Some(native::serve(read, write).map_or(1, |()| 0));
        }
    }
    Some(1)
}

#[cfg(unix)]
mod native {
    use super::*;
    use std::env;
    use std::fs::{self, File, OpenOptions};
    use std::io::{Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use std::os::unix::process::CommandExt;
    use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
    use std::time::{Duration, Instant};

    const HANDSHAKE: Duration = Duration::from_secs(2);

    fn invalid(message: &str) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidInput, message)
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Auth<'a> {
        Pipe(i32, i32),
        Fifo(&'a Path),
    }

    fn auth(value: &str) -> io::Result<Auth<'_>> {
        let value = ["--jobserver-auth=", "--jobserver-fds="]
            .iter()
            .find_map(|prefix| value.rsplit_once(prefix).map(|(_, rest)| rest))
            .and_then(|rest| rest.split(' ').next())
            .ok_or_else(|| invalid("no jobserver authentication"))?;
        if let Some(path) = value.strip_prefix("fifo:") {
            if path.is_empty() {
                return Err(invalid("empty jobserver FIFO path"));
            }
            return Ok(Auth::Fifo(Path::new(path)));
        }
        let (read, write) = value.split_once(',').ok_or_else(|| invalid("invalid jobserver pipe authentication"))?;
        let read: i32 = read.parse().map_err(|_| invalid("invalid jobserver read descriptor"))?;
        let write: i32 = write.parse().map_err(|_| invalid("invalid jobserver write descriptor"))?;
        if read <= 2 || write <= 2 || read == write {
            return Err(invalid("invalid jobserver descriptor pair"));
        }
        Ok(Auth::Pipe(read, write))
    }

    fn selected(values: [Option<OsString>; 3]) -> io::Result<OsString> {
        values.into_iter().flatten().next().ok_or_else(|| invalid("no inherited jobserver"))
    }

    fn raw_stat(fd: i32) -> io::Result<libc::stat> {
        let mut result = std::mem::MaybeUninit::uninit();
        // SAFETY: fstat initializes the supplied stat buffer only on success.
        if unsafe { libc::fstat(fd, result.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful fstat initialized every field.
        Ok(unsafe { result.assume_init() })
    }

    fn flags(fd: i32) -> io::Result<i32> {
        // SAFETY: F_GETFL takes no pointer or variadic argument.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 { Err(io::Error::last_os_error()) } else { Ok(flags) }
    }

    fn duplicate(fd: i32) -> io::Result<File> {
        // SAFETY: F_DUPFD_CLOEXEC returns a new, uniquely owned descriptor on success.
        let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
        if copy < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: ownership of the successful duplicate is transferred exactly once.
        Ok(unsafe { File::from_raw_fd(copy) })
    }

    struct Endpoints {
        read: File,
        write: File,
    }

    impl Endpoints {
        fn checked(read: File, write: File) -> io::Result<Self> {
            let a = raw_stat(read.as_raw_fd())?;
            let b = raw_stat(write.as_raw_fd())?;
            let read_flags = flags(read.as_raw_fd())?;
            let write_flags = flags(write.as_raw_fd())?;
            if a.st_mode & libc::S_IFMT != libc::S_IFIFO
                || b.st_mode & libc::S_IFMT != libc::S_IFIFO
                || (a.st_dev, a.st_ino) != (b.st_dev, b.st_ino)
                || read_flags & libc::O_ACCMODE != libc::O_RDONLY
                || write_flags & libc::O_ACCMODE != libc::O_WRONLY
                || read_flags & libc::O_NONBLOCK != 0
                || write_flags & libc::O_NONBLOCK != 0
            {
                return Err(invalid("jobserver endpoints must be matching blocking readonly/writeonly pipes"));
            }
            Ok(Self { read, write })
        }

        fn pipe(read: i32, write: i32) -> io::Result<Self> {
            if read <= 2 || write <= 2 || read == write {
                return Err(invalid("invalid jobserver descriptor pair"));
            }
            Self::checked(duplicate(read)?, duplicate(write)?)
        }

        fn fifo(path: &Path) -> io::Result<Self> {
            let before = fs::symlink_metadata(path)?;
            if before.mode() & 0o170000 != 0o010000 {
                return Err(invalid("jobserver FIFO is not a real FIFO"));
            }
            let read = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC).open(path)?;
            let write = OpenOptions::new().write(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC).open(path)?;
            let after = fs::symlink_metadata(path)?;
            for file in [&read, &write] {
                let meta = file.metadata()?;
                if (meta.dev(), meta.ino()) != (before.dev(), before.ino())
                    || (meta.dev(), meta.ino()) != (after.dev(), after.ino())
                    || meta.mode() & 0o170000 != 0o010000
                {
                    return Err(invalid("jobserver FIFO changed while opening"));
                }
                // SAFETY: these fresh FIFO descriptions are owned here and shared with nobody yet.
                if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags(file.as_raw_fd())? & !libc::O_NONBLOCK) } < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            Self::checked(read, write)
        }

        fn environment() -> io::Result<Self> {
            let value = selected([env::var_os("CARGO_MAKEFLAGS"), env::var_os("MAKEFLAGS"), env::var_os("MFLAGS")])?;
            match auth(value.to_str().ok_or_else(|| invalid("non-UTF8 jobserver authentication"))?)? {
                Auth::Pipe(read, write) => Self::pipe(read, write),
                Auth::Fifo(path) => Self::fifo(path),
            }
        }
    }

    #[derive(Clone, Copy)]
    struct Descriptor {
        fd: i32,
        dev: libc::dev_t,
        ino: libc::ino_t,
    }

    fn descriptors() -> io::Result<Vec<Descriptor>> {
        #[cfg(target_os = "linux")]
        let directory = "/proc/self/fd";
        #[cfg(not(target_os = "linux"))]
        let directory = "/dev/fd";
        let mut fds = Vec::new();
        for entry in fs::read_dir(directory)? {
            if let Some(fd) = entry?.file_name().to_str().and_then(|name| name.parse::<i32>().ok()).filter(|fd| *fd > 2) {
                fds.push(fd);
            }
        }
        let mut result = Vec::new();
        for fd in fds {
            match raw_stat(fd) {
                Ok(meta) => result.push(Descriptor { fd, dev: meta.st_dev, ino: meta.st_ino }),
                Err(error) if error.raw_os_error() == Some(libc::EBADF) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(result)
    }

    fn close_other(descriptors: &[Descriptor], keep: [i32; 2]) -> io::Result<()> {
        for old in descriptors {
            if keep.contains(&old.fd) {
                continue;
            }
            let current = match raw_stat(old.fd) {
                Ok(current) => current,
                Err(error) if error.raw_os_error() == Some(libc::EBADF) => continue,
                Err(error) => return Err(error),
            };
            if (current.st_dev, current.st_ino) == (old.dev, old.ino) {
                // SAFETY: this child closes only an observed descriptor outside its retained protocol set.
                if unsafe { libc::close(old.fd) } < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
        }
        Ok(())
    }

    fn configure(command: &mut Command, endpoints: &Endpoints) -> io::Result<()> {
        let observed = descriptors()?;
        let keep = [endpoints.read.as_raw_fd(), endpoints.write.as_raw_fd()];
        // SAFETY: post-fork work uses only setsid, fstat, close and fcntl on preallocated descriptor data.
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                close_other(&observed, keep)?;
                for fd in keep {
                    let flags = libc::fcntl(fd, libc::F_GETFD);
                    if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        Ok(())
    }

    fn byte(reader: &mut impl Read) -> io::Result<u8> {
        let mut byte = [0];
        reader.read_exact(&mut byte)?;
        Ok(byte[0])
    }

    fn handshake(reader: &mut ChildStdout) -> io::Result<u8> {
        let deadline = Instant::now() + HANDSHAKE;
        loop {
            let mut poll = libc::pollfd { fd: reader.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "jobserver keeper handshake timed out"));
            }
            // SAFETY: poll accesses one initialized pollfd for a bounded duration.
            let result = unsafe { libc::poll(&mut poll, 1, remaining.as_millis().min(i32::MAX as u128) as i32 + 1) };
            if result > 0 {
                return byte(reader);
            }
            if result < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                return Err(io::Error::last_os_error());
            }
        }
    }

    pub(crate) struct Loan {
        child: Child,
        control: Option<ChildStdin>,
        ack: ChildStdout,
        acknowledged: bool,
        reaped: bool,
        failed: bool,
    }

    impl Loan {
        pub(crate) fn resume(&mut self) -> io::Result<()> {
            if self.reaped {
                return if self.failed { Err(io::Error::other("jobserver repayment remained uncertain")) } else { Ok(()) };
            }
            let mut failure = None;
            if let Some(mut control) = self.control.take()
                && let Err(error) = control.write_all(b"R")
            {
                failure = Some(error);
            }
            loop {
                match byte(&mut self.ack) {
                    Ok(b'L') if !self.acknowledged => self.acknowledged = true,
                    Ok(b'D') => break,
                    Ok(_) => {
                        failure = Some(invalid("invalid jobserver keeper repayment acknowledgement"));
                        break;
                    }
                    Err(error) => {
                        failure = Some(error);
                        break;
                    }
                }
            }
            let status = self.child.wait()?;
            self.reaped = true;
            self.failed = !status.success() || failure.is_some();
            if !status.success() {
                return Err(io::Error::other("jobserver keeper exited without confirmed repayment"));
            }
            match failure {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }
    }

    impl Drop for Loan {
        fn drop(&mut self) {
            if self.reaped {
                return;
            }
            if let Err(error) = self.resume() {
                eprintln!("frust-wrap: jobserver repayment is uncertain: {error}");
                std::process::exit(125);
            }
        }
    }

    fn start(mut command: Command, endpoints: Endpoints) -> Result<Loan, LendError> {
        configure(&mut command, &endpoints).map_err(LendError::Unavailable)?;
        command.stdin(Stdio::piped()).stdout(Stdio::piped());
        let mut child = command.spawn().map_err(LendError::Unavailable)?;
        drop(endpoints);
        let mut ack = child.stdout.take().expect("keeper stdout is piped");
        let mut control = child.stdin.take().expect("keeper stdin is piped");
        if !matches!(handshake(&mut ack), Ok(b'R')) {
            drop(control);
            let _ = child.kill();
            let _ = child.wait();
            return Err(LendError::Unavailable(io::Error::other("jobserver keeper was not ready")));
        }
        if let Err(error) = control.write_all(b"A") {
            drop(control);
            let _ = child.kill();
            let _ = child.wait();
            return Err(LendError::Unavailable(error));
        }
        let mut loan = Loan { child, control: Some(control), ack, acknowledged: false, reaped: false, failed: false };
        match handshake(&mut loan.ack) {
            Ok(b'L') => {
                loan.acknowledged = true;
                Ok(loan)
            }
            _ => match loan.resume() {
                Ok(()) => Err(LendError::Unavailable(io::Error::other("jobserver loan was settled after ambiguous handoff"))),
                Err(error) => Err(LendError::Uncertain(error)),
            },
        }
    }

    pub(crate) fn lend(helper: &Path) -> Result<Loan, LendError> {
        let endpoints = Endpoints::environment().map_err(LendError::Unavailable)?;
        let mut command = Command::new(helper);
        command.args([HELPER.to_owned(), endpoints.read.as_raw_fd().to_string(), endpoints.write.as_raw_fd().to_string()]);
        start(command, endpoints)
    }

    pub(super) fn serve(read: i32, write: i32) -> io::Result<()> {
        let endpoints = Endpoints::pipe(read, write)?;
        let keep = [endpoints.read.as_raw_fd(), endpoints.write.as_raw_fd()];
        close_other(&descriptors()?, keep)?;
        let stderr = raw_stat(2)?;
        if stderr.st_mode & libc::S_IFMT != libc::S_IFIFO || flags(2)? & libc::O_ACCMODE != libc::O_WRONLY {
            return Err(invalid("keeper stderr is not Cargo's captured writer"));
        }
        let mut control = io::stdin().lock();
        let mut ack = io::stdout().lock();
        ack.write_all(b"R")?;
        ack.flush()?;
        match byte(&mut control) {
            Ok(b'A') => {}
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            _ => return Err(invalid("keeper was not authorized")),
        }
        let Endpoints { mut read, mut write } = endpoints;
        write.write_all(b"+")?;
        drop(write);
        let _ = ack.write_all(b"L");
        let _ = ack.flush();
        let _ = byte(&mut control);
        byte(&mut read)?;
        let _ = ack.write_all(b"D");
        let _ = ack.flush();
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn parent_info(pid: u32) -> io::Result<(u32, std::path::PathBuf)> {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
        let parent = stat
            .rsplit_once(") ")
            .and_then(|(_, fields)| fields.split_whitespace().nth(1))
            .and_then(|parent| parent.parse().ok())
            .ok_or_else(|| invalid("invalid parent process record"))?;
        Ok((parent, format!("/proc/{pid}/exe").into()))
    }

    #[cfg(target_os = "macos")]
    fn parent_info(pid: u32) -> io::Result<(u32, std::path::PathBuf)> {
        use std::os::unix::ffi::OsStrExt;
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
        // SAFETY: proc_pidinfo receives a correctly sized writable proc_bsdinfo buffer.
        let result = unsafe {
            libc::proc_pidinfo(
                pid as i32,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                std::mem::size_of::<libc::proc_bsdinfo>() as i32,
            )
        };
        if result as usize != std::mem::size_of::<libc::proc_bsdinfo>() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: proc_pidinfo returned the complete structure.
        let info = unsafe { info.assume_init() };
        let mut path = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        // SAFETY: proc_pidpath receives the writable buffer and its exact byte capacity.
        let count = unsafe { libc::proc_pidpath(pid as i32, path.as_mut_ptr().cast(), path.len() as u32) };
        if count <= 0 {
            return Err(io::Error::last_os_error());
        }
        let len = path.iter().position(|byte| *byte == 0).ok_or_else(|| invalid("unterminated process image path"))?;
        Ok((info.pbi_ppid, Path::new(std::ffi::OsStr::from_bytes(&path[..len])).to_path_buf()))
    }

    pub(crate) fn direct_cargo_parent(supervisor: u32, cargo: &Path) -> bool {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            if supervisor <= 1 || !cargo.is_absolute() || cargo.file_name().is_none_or(|name| name != "cargo") {
                return false;
            }
            let Ok(expected) = fs::metadata(cargo) else {
                return false;
            };
            if !expected.is_file() || expected.mode() & 0o111 == 0 {
                return false;
            }
            // SAFETY: getppid has no arguments or preconditions.
            let parent = unsafe { libc::getppid() } as u32;
            let matches = || {
                parent_info(parent).ok().is_some_and(|(ppid, image)| {
                    ppid == supervisor
                        && fs::metadata(image)
                            .is_ok_and(|meta| meta.is_file() && (meta.dev(), meta.ino()) == (expected.dev(), expected.ino()))
                })
            };
            // SAFETY: getppid has no arguments or preconditions.
            matches() && unsafe { libc::getppid() } as u32 == parent && matches()
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = (supervisor, cargo);
            false
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn authentication_matches_jobserver_priority_and_last_auth() {
            assert_eq!(auth("-j --jobserver-auth=3,4 --jobserver-auth=5,6").unwrap(), Auth::Pipe(5, 6));
            assert_eq!(auth("--jobserver-auth=3,4 --jobserver-fds=5,6").unwrap(), Auth::Pipe(3, 4));
            assert_eq!(auth("--jobserver-fds=3,4").unwrap(), Auth::Pipe(3, 4));
            assert_eq!(auth("--jobserver-auth=fifo:/tmp/jobserver").unwrap(), Auth::Fifo(Path::new("/tmp/jobserver")));
            for value in [
                "",
                "--jobserver-auth=",
                "--jobserver-auth=1,2",
                "--jobserver-auth=3,3",
                "--jobserver-auth=-1,-2",
                "--jobserver-auth=fifo:",
            ] {
                assert!(auth(value).is_err(), "{value}");
            }
            let values = [Some("bad selected value".into()), Some("--jobserver-auth=3,4".into()), None];
            assert_eq!(selected(values).unwrap(), "bad selected value");
        }

        #[test]
        fn mismatched_reversed_and_regular_endpoints_are_rejected() {
            let (read, write) = io::pipe().unwrap();
            let (other, _) = io::pipe().unwrap();
            assert!(Endpoints::pipe(read.as_raw_fd(), write.as_raw_fd()).is_ok());
            assert!(Endpoints::pipe(write.as_raw_fd(), read.as_raw_fd()).is_err());
            assert!(Endpoints::pipe(other.as_raw_fd(), write.as_raw_fd()).is_err());
            let file = File::open(env::current_exe().unwrap()).unwrap();
            assert!(Endpoints::pipe(file.as_raw_fd(), write.as_raw_fd()).is_err());
            assert_eq!(flags(read.as_raw_fd()).unwrap() & libc::O_NONBLOCK, 0);
        }

        #[test]
        fn guardian_protocol_child() {
            let Some(value) = env::var_os("FRUST_TEST_LOAN_GUARDIAN") else {
                return;
            };
            let value = value.to_str().unwrap();
            let (read, write) = value.split_once(',').unwrap();
            println!("frust-loan-test-dispatch");
            io::stdout().flush().unwrap();
            std::process::exit(serve(read.parse().unwrap(), write.parse().unwrap()).map_or(1, |()| 0));
        }

        fn available(fd: i32) -> usize {
            let mut count = 0i32;
            // SAFETY: FIONREAD writes one initialized integer through the supplied pointer.
            assert_eq!(unsafe { libc::ioctl(fd, libc::FIONREAD, &mut count) }, 0);
            count.try_into().unwrap()
        }

        #[test]
        fn guardian_authorization_resume_and_writer_death_preserve_pipe_capacity() {
            struct Reaper(Child, bool);
            impl Drop for Reaper {
                fn drop(&mut self) {
                    if !self.1 {
                        let _ = self.0.kill();
                        let _ = self.0.wait();
                    }
                }
            }
            for mode in ["before-authorization", "resume", "writer-eof", "jobserver-writer-eof"] {
                let (read, mut write) = io::pipe().unwrap();
                let endpoints = Endpoints::pipe(read.as_raw_fd(), write.as_raw_fd()).unwrap();
                let mut command = Command::new(env::current_exe().unwrap());
                command
                    .args(["--exact", "sys::jobserver_loan::native::tests::guardian_protocol_child", "--nocapture"])
                    .env("FRUST_TEST_LOAN_GUARDIAN", format!("{},{}", endpoints.read.as_raw_fd(), endpoints.write.as_raw_fd()))
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                configure(&mut command, &endpoints).unwrap();
                let mut child = Reaper(command.spawn().unwrap(), false);
                drop(endpoints);
                let mut control = child.0.stdin.take().unwrap();
                let mut ack = child.0.stdout.take().unwrap();
                let mut header = Vec::new();
                let header_deadline = Instant::now() + HANDSHAKE;
                while !header.ends_with(b"frust-loan-test-dispatch\n") {
                    assert!(Instant::now() < header_deadline, "child did not reach protocol dispatch");
                    header.push(handshake(&mut ack).unwrap());
                    assert!(header.len() < 65536);
                }
                assert_eq!(handshake(&mut ack).unwrap(), b'R');
                assert_eq!(available(read.as_raw_fd()), 0, "no loan before authorization");
                if mode != "before-authorization" {
                    control.write_all(b"A").unwrap();
                    assert_eq!(handshake(&mut ack).unwrap(), b'L');
                    assert_eq!(available(read.as_raw_fd()), 1);
                    let mut borrow = &read;
                    assert_eq!(byte(&mut borrow).unwrap(), b'+');
                    if mode == "resume" {
                        control.write_all(b"R").unwrap();
                    }
                    drop(control);
                    assert_eq!(available(read.as_raw_fd()), 0);
                    assert!(child.0.try_wait().unwrap().is_none(), "keeper must survive outstanding debt");
                    if mode == "jobserver-writer-eof" {
                        drop(write);
                        assert_eq!(handshake(&mut ack).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
                    } else {
                        write.write_all(b"+").unwrap();
                        assert_eq!(handshake(&mut ack).unwrap(), b'D');
                    }
                } else {
                    drop(control);
                }
                let deadline = Instant::now() + HANDSHAKE;
                loop {
                    if let Some(status) = child.0.try_wait().unwrap() {
                        child.1 = true;
                        if mode == "jobserver-writer-eof" {
                            assert_eq!(status.code(), Some(1), "the vanished jobserver cannot repay a loan");
                        } else {
                            assert!(status.success(), "{mode}: {status}");
                        }
                        break;
                    }
                    assert!(Instant::now() < deadline, "keeper did not exit in {mode}");
                    std::thread::sleep(Duration::from_millis(1));
                }
                assert_eq!(available(read.as_raw_fd()), 0, "{mode}");
                let mut eof = [0];
                assert_eq!(ack.read(&mut eof).unwrap(), 0, "no stdout writer survives keeper exit");
                assert_eq!(child.0.stderr.as_mut().unwrap().read(&mut eof).unwrap(), 0, "no stderr writer survives keeper exit");
            }
        }

        #[test]
        fn unavailable_helper_never_authorizes_or_lends() {
            let (read, write) = io::pipe().unwrap();
            let endpoints = Endpoints::pipe(read.as_raw_fd(), write.as_raw_fd()).unwrap();
            let result = start(Command::new("/frust-no-such-loan-keeper"), endpoints);
            assert!(matches!(result, Err(LendError::Unavailable(_))));
            assert_eq!(available(read.as_raw_fd()), 0);
        }

        #[test]
        fn named_fifo_opens_readonly_compensation_and_rejects_symlink_alias() {
            use std::os::unix::ffi::OsStrExt;
            struct Scratch(std::path::PathBuf);
            impl Drop for Scratch {
                fn drop(&mut self) {
                    let _ = fs::remove_dir_all(&self.0);
                }
            }
            let scratch = Scratch(crate::fsx::temp_sibling(&env::temp_dir().join("frust-loan-fifo")));
            fs::create_dir(&scratch.0).unwrap();
            let path = scratch.0.join("jobserver");
            let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
            // SAFETY: the private, absent path is a valid NUL-terminated FIFO name.
            assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
            let mut endpoints = Endpoints::fifo(&path).unwrap();
            assert_eq!(flags(endpoints.read.as_raw_fd()).unwrap() & libc::O_ACCMODE, libc::O_RDONLY);
            assert_eq!(flags(endpoints.write.as_raw_fd()).unwrap() & libc::O_ACCMODE, libc::O_WRONLY);
            endpoints.write.write_all(b"+").unwrap();
            assert_eq!(byte(&mut endpoints.read).unwrap(), b'+');
            let alias = scratch.0.join("alias");
            std::os::unix::fs::symlink(&path, &alias).unwrap();
            assert!(Endpoints::fifo(&alias).is_err());
            fs::write(scratch.0.join("regular"), b"not a FIFO").unwrap();
            assert!(Endpoints::fifo(&scratch.0.join("regular")).is_err());
        }
    }
}
