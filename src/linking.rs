//! Opt-in raw Apple linking for unoptimised incremental macOS binaries.

use std::env;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use crate::cache::rustc_args;
use crate::{sys, toolchain};

const DRIVER_ENV: &[&str] = &[
    "LIBRARY_PATH",
    "COMPILER_PATH",
    "GCC_EXEC_PREFIX",
    "CCC_OVERRIDE_OPTIONS",
    "TOOLCHAINS",
    "LDFLAGS",
];

/// Whether the user explicitly selected this policy. Other values leave linking unchanged.
pub fn requested() -> bool {
    is_requested(env::var_os("FRUST_LINK").as_deref())
}

/// Whether this platform has the native linker policy.
pub fn supported() -> bool {
    cfg!(target_os = "macos")
}

/// Prepares compiler-first argv; unsupported invocations remain unchanged.
pub fn prepare(args: &[OsString], debug: bool) -> Option<Vec<OsString>> {
    prepare_with(args, debug, requested(), supported(), || resolve_linker(debug))
}

fn is_requested(value: Option<&OsStr>) -> bool {
    value == Some(OsStr::new("smart"))
}

fn prepare_with(
    args: &[OsString],
    debug: bool,
    requested: bool,
    supported: bool,
    resolve: impl FnOnce() -> Result<PathBuf, &'static str>,
) -> Option<Vec<OsString>> {
    if !requested {
        return None;
    }
    let prepared = (|| {
        if !supported {
            return Err("unsupported platform");
        }
        eligible(args)?;
        resolve()
    })();
    match prepared {
        Ok(linker) => {
            let mut effective = args.to_vec();
            let mut flag = OsString::from("-Clinker=");
            flag.push(linker.as_os_str());
            effective.push(flag);
            // Stable rustc's legacy `ld` flavor selects the native raw linker for the host target.
            effective.push(OsString::from("-Clinker-flavor=ld"));
            if debug {
                eprintln!("frust-wrap: smart link: using {}", linker.display());
            }
            Some(effective)
        }
        Err(reason) => {
            if debug {
                eprintln!("frust-wrap: link policy skipped: {reason}");
            }
            None
        }
    }
}

fn eligible(args: &[OsString]) -> Result<(), &'static str> {
    let strings = args.iter().map(|arg| arg.to_str()).collect::<Option<Vec<_>>>().ok_or("non-UTF-8 arguments")?;
    if strings.len() < 2 || !strings[1].starts_with('-') {
        return Err("not a rustc invocation");
    }
    let parsed = rustc_args::parse(&args[1..]).map_err(|_| "not a supported compile")?;
    if parsed.crate_name.starts_with("build_script") {
        return Err("build script");
    }
    if !parsed.incremental() || !parsed.unoptimised() {
        return Err("requires an unoptimised incremental compile");
    }

    let mut binary = false;
    let mut link = false;
    let mut incremental = false;
    let mut unoptimised = true;
    let mut i = 1;
    while i < strings.len() {
        let arg = strings[i];
        let next = strings.get(i + 1).copied();
        match arg {
            "--test" | "--target" | "--sysroot" | "--" | "--help" | "-h" | "--version" | "-V" | "-vV" => {
                return Err("custom target, test, or non-compile invocation");
            }
            _ if arg.starts_with('@')
                || arg.starts_with("--target=")
                || arg.starts_with("--sysroot=")
                || arg.starts_with("--test=") =>
            {
                return Err("custom target or response file");
            }
            "--crate-type" => {
                if next != Some("bin") {
                    return Err("requires only an explicit binary crate type");
                }
                binary = true;
                i += 1;
            }
            _ if arg.starts_with("--crate-type=") => {
                if arg.strip_prefix("--crate-type=") != Some("bin") {
                    return Err("requires only an explicit binary crate type");
                }
                binary = true;
            }
            "--emit" => {
                link = next.is_some_and(emits_link);
                i += 1;
            }
            _ if arg.starts_with("--emit=") => link = emits_link(&arg["--emit=".len()..]),
            "-O" => unoptimised = false,
            "-C" | "--codegen" => {
                inspect_codegen(next.ok_or("missing codegen option")?, &mut incremental, &mut unoptimised)?;
                i += 1;
            }
            _ if arg.starts_with("--codegen=") => {
                inspect_codegen(&arg["--codegen=".len()..], &mut incremental, &mut unoptimised)?;
            }
            _ if arg.starts_with("-C") => {
                inspect_codegen(&arg[2..], &mut incremental, &mut unoptimised)?;
            }
            "-Z" => {
                if !next.is_some_and(time_passes) {
                    return Err("unstable option");
                }
                i += 1;
            }
            _ if arg.starts_with("-Z") && !time_passes(&arg[2..]) => {
                return Err("unstable option");
            }
            _ => {}
        }
        i += 1;
    }
    if !binary || !link || !incremental || !unoptimised {
        return Err("requires an explicit incremental debug binary link");
    }
    Ok(())
}

fn emits_link(emit: &str) -> bool {
    emit.split(',').any(|kind| kind == "link")
}

fn time_passes(value: &str) -> bool {
    // Timing diagnostics let benchmarks observe linker work without changing code generation.
    matches!(
        value.strip_prefix('=').unwrap_or(value),
        "time-passes"
            | "time-passes=yes"
            | "time-passes=on"
            | "time-passes=true"
            | "time-passes=1"
            | "time-passes=no"
            | "time-passes=off"
            | "time-passes=false"
            | "time-passes=0"
    )
}

fn inspect_codegen(value: &str, incremental: &mut bool, unoptimised: &mut bool) -> Result<(), &'static str> {
    let value = value.strip_prefix('=').unwrap_or(value);
    let (key, value) = value.split_once('=').unwrap_or((value, ""));
    let key = key.replace('_', "-");
    match key.as_str() {
        "linker"
        | "linker-flavor"
        | "link-arg"
        | "link-args"
        | "pre-link-arg"
        | "pre-link-args"
        | "prefer-dynamic"
        | "rpath"
        | "link-self-contained"
        | "linker-plugin-lto" => return Err("explicit linker option"),
        "incremental" => *incremental = !value.is_empty(),
        "opt-level" => *unoptimised = value == "0",
        _ => {}
    }
    Ok(())
}

fn driver_environment_supported(mut get: impl FnMut(&str) -> Option<OsString>) -> bool {
    DRIVER_ENV.iter().all(|name| get(name).is_none_or(|value| value.is_empty()))
}

fn default_driver_is_system(found: Option<&Path>, canonicalize: impl Fn(&Path) -> Option<PathBuf>) -> bool {
    let Some(found) = found.and_then(&canonicalize) else {
        return false;
    };
    canonicalize(Path::new("/usr/bin/cc")).is_some_and(|system| found == system)
}

fn resolve_linker(debug: bool) -> Result<PathBuf, &'static str> {
    if !driver_environment_supported(|name| env::var_os(name)) {
        return Err("custom compiler driver environment");
    }
    let driver = toolchain::find_on_path("cc");
    if !default_driver_is_system(driver.as_deref(), |path| sys::canonicalize(path).ok()) {
        return Err("default cc is not the system driver");
    }
    discover_linker(debug)
}

#[cfg(not(target_os = "macos"))]
fn discover_linker(_debug: bool) -> Result<PathBuf, &'static str> {
    Err("native linker discovery failed")
}

#[cfg(target_os = "macos")]
fn discover_linker(debug: bool) -> Result<PathBuf, &'static str> {
    discovery::discover().map_err(|failure| {
        if debug {
            eprintln!("frust-wrap: native linker discovery: {}", failure.diagnostic());
        }
        "native linker discovery failed"
    })
}

#[cfg(any(target_os = "macos", test))]
mod discovery {
    use std::io;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    const TIMEOUT: Duration = Duration::from_secs(2);
    const MAX_OUTPUT: usize = 64 * 1024;
    const STDERR_LIMIT: usize = 4 * 1024;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum Stage {
        Spawn, MissingStdout, MissingStderr, ReadStdout, ReadStderr, TryWait,
        Deadline, StdoutLimit, ExitStatus, InvalidUtf8, InvalidLine, NonAbsolute, NotRegularFile,
    }

    #[derive(Debug)]
    struct IoFailure {
        kind: io::ErrorKind,
        errno: Option<i32>,
    }

    impl From<io::Error> for IoFailure {
        fn from(error: io::Error) -> Self {
            Self { kind: error.kind(), errno: error.raw_os_error() }
        }
    }

    #[derive(Clone, Debug)]
    struct Status {
        success: bool,
        code: Option<i32>,
        description: String,
    }

    #[cfg(target_os = "macos")]
    impl From<std::process::ExitStatus> for Status {
        fn from(status: std::process::ExitStatus) -> Self {
            Self { success: status.success(), code: status.code(), description: status.to_string() }
        }
    }

    #[derive(Debug)]
    struct Evidence {
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        bytes: [usize; 2],
        eof: [bool; 2],
        status: Option<Status>,
        spawn_elapsed: Duration,
        elapsed: Duration,
    }

    impl Evidence {
        fn new(spawn_elapsed: Duration) -> Self {
            Self { stdout: Vec::new(), stderr: Vec::new(), bytes: [0; 2], eof: [false; 2],
                status: None, spawn_elapsed, elapsed: spawn_elapsed }
        }

        fn failed(mut self, stage: Stage, error: Option<io::Error>, elapsed: Duration) -> Box<Failure> {
            self.elapsed = elapsed;
            Box::new(Failure { stage, io: error.map(IoFailure::from), evidence: self, child_pid: None, total_elapsed: elapsed, cleanup: None })
        }
    }

    #[derive(Debug)]
    struct Cleanup {
        already_reaped: bool,
        kill_error: Option<IoFailure>,
        wait_error: Option<IoFailure>,
        status: Option<Status>,
        elapsed: Duration,
    }

    #[derive(Debug)]
    pub(super) struct Failure {
        stage: Stage,
        io: Option<IoFailure>,
        evidence: Evidence,
        child_pid: Option<u32>,
        total_elapsed: Duration,
        cleanup: Option<Cleanup>,
    }

    // Cap rendered bytes too: arbitrary xcrun output cannot inject lines or enlarge an escaped excerpt.
    fn escaped(bytes: &[u8], limit: usize) -> (String, usize) {
        let mut text = String::new();
        let mut consumed = 0;
        for &byte in bytes {
            let escape = std::ascii::escape_default(byte);
            if text.len() + escape.len() > limit { break; }
            text.extend(escape.map(char::from));
            consumed += 1;
        }
        (text, consumed)
    }

    fn status_text(status: Option<&Status>) -> String {
        status.map_or_else(|| "unobserved".into(), |status| {
            format!("success={},code={:?},description=\"{}\"", status.success, status.code, escaped(status.description.as_bytes(), 128).0)
        })
    }

    fn error_text(error: Option<&IoFailure>) -> String {
        error.map_or_else(|| "none".into(), |error| format!("kind={:?},errno={:?}", error.kind, error.errno))
    }

    impl Failure {
        pub(super) fn diagnostic(&self) -> String {
            let evidence = &self.evidence;
            let (stdout, stdout_used) = escaped(&evidence.stdout, 256);
            let (stderr, stderr_used) = escaped(&evidence.stderr, STDERR_LIMIT);
            let cleanup = self.cleanup.as_ref().map_or_else(|| "not_attempted_no_child".into(), |cleanup| {
                format!("already_reaped={},kill=({}),wait=({}),status=({}),elapsed_us={}",
                    cleanup.already_reaped, error_text(cleanup.kill_error.as_ref()), error_text(cleanup.wait_error.as_ref()),
                    status_text(cleanup.status.as_ref()), cleanup.elapsed.as_micros())
            });
            let pid = self.child_pid.map_or_else(|| "not_spawned".into(), |pid| pid.to_string());
            format!("stage={:?} child_pid={} error=({}) total_us={} spawn_us={} decision_us={} eof={:?} observed_bytes={:?} status=({}) stdout_truncated={} stdout=\"{}\" stderr_truncated={} stderr=\"{}\" cleanup=({})",
                self.stage, pid, error_text(self.io.as_ref()), self.total_elapsed.as_micros(), evidence.spawn_elapsed.as_micros(),
                evidence.elapsed.as_micros(), evidence.eof, evidence.bytes, status_text(evidence.status.as_ref()),
                stdout_used < evidence.bytes[0], stdout, stderr_used < evidence.bytes[1], stderr, cleanup)
        }
    }

    // A small private seam keeps read/status/clock failures deterministic without invoking xcrun in unit tests.
    trait DiscoveryIo {
        fn elapsed(&self) -> Duration;
        fn read(&mut self, stream: usize, buffer: &mut [u8]) -> io::Result<usize>;
        fn status(&mut self) -> io::Result<Option<Status>>;
        fn wait(&mut self, eof: [bool; 2], timeout: Duration) -> [bool; 2];
    }

    fn read_discovery(io: &mut impl DiscoveryIo, spawn_elapsed: Duration) -> Result<Evidence, Box<Failure>> {
        let mut evidence = Evidence::new(spawn_elapsed);
        let mut chunk = [0; 4096];
        loop {
            let elapsed = io.elapsed();
            let unsuccessful = evidence.status.as_ref().is_some_and(|status| !status.success);
            if elapsed >= TIMEOUT {
                return Err(evidence.failed(if unsuccessful { Stage::ExitStatus } else { Stage::Deadline }, None, elapsed));
            }
            if evidence.status.is_none() {
                evidence.status = match io.status() {
                    Ok(status) => status,
                    Err(error) => return Err(evidence.failed(Stage::TryWait, Some(error), io.elapsed())),
                };
            }
            let unsuccessful = evidence.status.as_ref().is_some_and(|status| !status.success);
            if evidence.eof[0] && evidence.status.is_some() && (!unsuccessful || evidence.eof[1]) {
                if unsuccessful { return Err(evidence.failed(Stage::ExitStatus, None, io.elapsed())); }
                evidence.elapsed = io.elapsed();
                return Ok(evidence);
            }
            // After a failed exit, drain only immediately readable diagnostics; do not wait for descendants.
            let wait = if unsuccessful { Duration::ZERO } else { TIMEOUT.saturating_sub(io.elapsed()).min(Duration::from_millis(10)) };
            let ready = io.wait(evidence.eof, wait);
            if unsuccessful && !ready.into_iter().any(|ready| ready) {
                return Err(evidence.failed(Stage::ExitStatus, None, io.elapsed()));
            }
            for (stream, ready) in ready.into_iter().enumerate() {
                // Successful discovery needs stdout only; descendants may retain stderr indefinitely.
                if stream == 1 && evidence.eof[0] && evidence.status.as_ref().is_some_and(|status| status.success) { break; }
                if !ready || evidence.eof[stream] { continue; }
                // Read once after readiness, then poll again; a failed nonblocking setup cannot make a second read block.
                match io.read(stream, &mut chunk) {
                    Ok(0) => evidence.eof[stream] = true,
                    Ok(size) => {
                        evidence.bytes[stream] = evidence.bytes[stream].saturating_add(size);
                        let (buffer, limit) = if stream == 0 { (&mut evidence.stdout, MAX_OUTPUT) } else { (&mut evidence.stderr, STDERR_LIMIT) };
                        let retained = size.min(limit.saturating_sub(buffer.len()));
                        buffer.extend_from_slice(&chunk[..retained]);
                        if stream == 0 && evidence.bytes[0] > MAX_OUTPUT {
                            return Err(evidence.failed(Stage::StdoutLimit, None, io.elapsed()));
                        }
                    }
                    Err(error) if matches!(error.kind(), io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock) => {}
                    Err(error) => return Err(evidence.failed(if stream == 0 { Stage::ReadStdout } else { Stage::ReadStderr }, Some(error), io.elapsed())),
                }
            }
            if evidence.eof[0] {
                if evidence.status.is_none() {
                    evidence.status = match io.status() {
                        Ok(status) => status,
                        Err(error) => return Err(evidence.failed(Stage::TryWait, Some(error), io.elapsed())),
                    };
                }
                if let Some(status) = &evidence.status {
                    if status.success {
                        evidence.elapsed = io.elapsed();
                        return Ok(evidence);
                    }
                    if evidence.eof[1] { return Err(evidence.failed(Stage::ExitStatus, None, io.elapsed())); }
                }
            }
        }
    }

    pub(super) fn linker_path(output: &[u8], is_file: impl FnOnce(&Path) -> bool) -> Result<PathBuf, Stage> {
        let output = std::str::from_utf8(output).map_err(|_| Stage::InvalidUtf8)?;
        let line = output.strip_suffix('\n').unwrap_or(output);
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() || line.contains(['\r', '\n', '\0']) { return Err(Stage::InvalidLine); }
        let path = PathBuf::from(line);
        if !path.is_absolute() { return Err(Stage::NonAbsolute); }
        if !is_file(&path) { return Err(Stage::NotRegularFile); }
        Ok(path)
    }

    #[cfg(target_os = "macos")]
    struct NativeIo<'a> {
        child: &'a mut std::process::Child,
        pipes: [crate::sys::Pipe; 2],
        started: std::time::Instant,
    }

    #[cfg(target_os = "macos")]
    impl DiscoveryIo for NativeIo<'_> {
        fn elapsed(&self) -> Duration { self.started.elapsed() }
        fn read(&mut self, stream: usize, buffer: &mut [u8]) -> io::Result<usize> { self.pipes[stream].read(buffer) }
        fn status(&mut self) -> io::Result<Option<Status>> { self.child.try_wait().map(|status| status.map(Status::from)) }
        fn wait(&mut self, eof: [bool; 2], timeout: Duration) -> [bool; 2] {
            if eof == [true; 2] {
                std::thread::sleep(timeout);
                [false; 2]
            } else {
                crate::sys::wait_readable([(!eof[0]).then_some(&self.pipes[0]), (!eof[1]).then_some(&self.pipes[1])], timeout)
            }
        }
    }

    #[cfg(target_os = "macos")]
    pub(super) fn discover() -> Result<PathBuf, Box<Failure>> {
        use std::process::{Command, Stdio};
        use std::time::Instant;

        let started = Instant::now();
        let mut child = match Command::new("/usr/bin/xcrun").args(["--find", "ld"])
            .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
            Ok(child) => child,
            Err(error) => return Err(Evidence::new(started.elapsed()).failed(Stage::Spawn, Some(error), started.elapsed())),
        };
        let spawn_elapsed = started.elapsed();
        let result = match (child.stdout.take(), child.stderr.take()) {
            (Some(stdout), Some(stderr)) => {
                let mut io = NativeIo { child: &mut child, pipes: [crate::sys::Pipe::from(stdout), crate::sys::Pipe::from(stderr)], started };
                read_discovery(&mut io, spawn_elapsed).and_then(|evidence| {
                    linker_path(&evidence.stdout, |path| path.is_file()).map_err(|stage| evidence.failed(stage, None, started.elapsed()))
                })
            }
            (None, _) => Err(Evidence::new(spawn_elapsed).failed(Stage::MissingStdout, None, started.elapsed())),
            (_, None) => Err(Evidence::new(spawn_elapsed).failed(Stage::MissingStderr, None, started.elapsed())),
        };
        result.map_err(|mut failure| {
            failure.child_pid = Some(child.id());
            let cleanup_started = Instant::now();
            let already_reaped = failure.evidence.status.is_some();
            let kill_error = if already_reaped { None } else { child.kill().err().map(IoFailure::from) };
            let waited = loop {
                match child.wait() {
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    result => break result,
                }
            };
            let (status, wait_error) = match waited {
                Ok(status) => (Some(Status::from(status)), None),
                Err(error) => (None, Some(IoFailure::from(error))),
            };
            failure.cleanup = Some(Cleanup { already_reaped, kill_error, wait_error, status, elapsed: cleanup_started.elapsed() });
            failure.total_elapsed = started.elapsed();
            failure
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::collections::VecDeque;

        struct FakeIo {
            streams: [VecDeque<io::Result<Vec<u8>>>; 2],
            statuses: VecDeque<io::Result<Option<Status>>>,
            default_status: Option<Status>,
            elapsed: Duration,
            read_elapsed: Duration,
            waits: Vec<Duration>,
        }

        impl FakeIo {
            fn new(stdout: &[u8], stderr: &[u8], code: i32) -> Self {
                Self { streams: [VecDeque::from([Ok(stdout.to_vec()), Ok(Vec::new())]), VecDeque::from([Ok(stderr.to_vec()), Ok(Vec::new())])],
                    statuses: VecDeque::new(), default_status: Some(Status { success: code == 0, code: Some(code), description: format!("exit {code}") }),
                    elapsed: Duration::ZERO, read_elapsed: Duration::ZERO, waits: Vec::new() }
            }
        }

        impl DiscoveryIo for FakeIo {
            fn elapsed(&self) -> Duration { self.elapsed }
            fn read(&mut self, stream: usize, buffer: &mut [u8]) -> io::Result<usize> {
                self.elapsed += self.read_elapsed;
                let mut bytes = self.streams[stream].pop_front().unwrap_or_else(|| Err(io::ErrorKind::WouldBlock.into()))?;
                let size = bytes.len().min(buffer.len());
                if bytes.len() > size {
                    let tail = bytes.split_off(size);
                    self.streams[stream].push_front(Ok(tail));
                }
                buffer[..size].copy_from_slice(&bytes);
                Ok(size)
            }
            fn status(&mut self) -> io::Result<Option<Status>> { self.statuses.pop_front().unwrap_or_else(|| Ok(self.default_status.clone())) }
            fn wait(&mut self, eof: [bool; 2], timeout: Duration) -> [bool; 2] {
                self.waits.push(timeout);
                let ready = [!eof[0] && !self.streams[0].is_empty(), !eof[1] && !self.streams[1].is_empty()];
                if !ready.into_iter().any(|ready| ready) { self.elapsed += timeout; }
                ready
            }
        }

        #[test]
        fn successful_discovery_drains_both_streams_and_retries_transient_reads() {
            let mut io = FakeIo::new(b"/Developer Tools/ld\n", b"note\n", 0);
            io.elapsed = Duration::from_millis(3);
            io.streams[0].push_front(Err(io::ErrorKind::Interrupted.into()));
            io.streams[1].push_front(Err(io::ErrorKind::WouldBlock.into()));
            let result = read_discovery(&mut io, Duration::from_millis(3)).unwrap();
            assert_eq!(result.stdout, b"/Developer Tools/ld\n");
            assert_eq!(result.stderr, b"note\n");
            assert_eq!(result.eof, [true, false]);
            assert_eq!(result.spawn_elapsed, Duration::from_millis(3));
            assert_eq!(result.status.unwrap().code, Some(0));
        }

        #[test]
        fn successful_exit_does_not_wait_for_open_or_continuously_ready_stderr() {
            for continuously_ready in [false, true] {
                let mut io = FakeIo::new(b"/Developer Tools/ld\n", b"note", 0);
                io.streams[1].pop_back();
                if continuously_ready {
                    io.streams[1].extend((0..8).map(|_| Ok(b"descendant".to_vec())));
                    io.read_elapsed = TIMEOUT / 4;
                }
                let result = read_discovery(&mut io, Duration::ZERO).unwrap();
                assert_eq!(result.stdout, b"/Developer Tools/ld\n");
                assert_eq!(result.stderr, b"note");
                assert_eq!(result.eof, [true, false]);
                assert!(result.elapsed < TIMEOUT);
                assert_eq!(io.streams[1].len(), if continuously_ready { 8 } else { 0 });
                assert_eq!(io.waits.len(), 2);
            }
        }

        #[test]
        fn failed_exit_retains_bounded_stderr_and_does_not_wait_for_open_descendants() {
            let stderr = vec![b'x'; STDERR_LIMIT * 3 + 17];
            let mut io = FakeIo::new(b"", &stderr, 7);
            io.streams[1].pop_back();
            let failure = read_discovery(&mut io, Duration::ZERO).unwrap_err();
            assert_eq!(failure.stage, Stage::ExitStatus);
            assert_eq!(failure.evidence.status.as_ref().unwrap().code, Some(7));
            assert_eq!(failure.evidence.stderr.len(), STDERR_LIMIT);
            assert_eq!(failure.evidence.bytes[1], stderr.len());
            assert!(!failure.evidence.eof[1]);
            assert!(io.waits.iter().all(Duration::is_zero));
            let diagnostic = failure.diagnostic();
            assert!(diagnostic.contains("stderr_truncated=true"));
            assert!(diagnostic.len() < STDERR_LIMIT + 1024);
        }

        #[test]
        fn deadline_is_deterministic_and_includes_spawn_time() {
            let mut io = FakeIo::new(b"", b"", 0);
            io.elapsed = TIMEOUT;
            let failure = read_discovery(&mut io, TIMEOUT).unwrap_err();
            assert_eq!(failure.stage, Stage::Deadline);
            assert_eq!(failure.evidence.spawn_elapsed, TIMEOUT);
            assert!(io.waits.is_empty());

            let mut io = FakeIo::new(b"", b"", 0);
            io.streams = [VecDeque::new(), VecDeque::new()];
            io.default_status = None;
            let failure = read_discovery(&mut io, Duration::ZERO).unwrap_err();
            assert_eq!(failure.stage, Stage::Deadline);
            assert_eq!(failure.evidence.elapsed, TIMEOUT);
        }

        #[test]
        fn read_and_wait_errors_keep_their_stage_and_errno() {
            for (stream, stage) in [(0, Stage::ReadStdout), (1, Stage::ReadStderr)] {
                let mut io = FakeIo::new(b"data", b"", 0);
                io.streams[stream].push_front(Err(io::Error::from_raw_os_error(9)));
                let failure = read_discovery(&mut io, Duration::ZERO).unwrap_err();
                assert_eq!(failure.stage, stage);
                assert_eq!(failure.io.unwrap().errno, Some(9));
            }
            let mut io = FakeIo::new(b"", b"", 0);
            io.statuses.push_back(Err(io::Error::from_raw_os_error(10)));
            let failure = read_discovery(&mut io, Duration::ZERO).unwrap_err();
            assert_eq!(failure.stage, Stage::TryWait);
            assert_eq!(failure.io.unwrap().errno, Some(10));
        }

        #[test]
        fn stdout_limit_and_escaped_diagnostics_remain_bounded() {
            let mut io = FakeIo::new(&vec![b'a'; MAX_OUTPUT + 1], b"", 0);
            let failure = read_discovery(&mut io, Duration::ZERO).unwrap_err();
            assert_eq!(failure.stage, Stage::StdoutLimit);
            assert_eq!(failure.evidence.stdout.len(), MAX_OUTPUT);
            assert_eq!(failure.evidence.bytes[0], MAX_OUTPUT + 1);
            assert!(failure.diagnostic().contains("stdout_truncated=true"));
            let (escaped, used) = escaped(&[0xff, b'\n', b'"'], 5);
            assert_eq!(escaped, "\\xff");
            assert_eq!(used, 1);
        }

        #[test]
        fn path_and_pre_read_failures_have_distinct_diagnostics() {
            for (bytes, stage) in [(b"\xff".as_slice(), Stage::InvalidUtf8), (b"/ld\n/other\n", Stage::InvalidLine),
                                  (b"\n", Stage::InvalidLine), (b"ld\n", Stage::NonAbsolute)] {
                assert_eq!(linker_path(bytes, |_| true), Err(stage));
            }
            let absolute = PathBuf::from(if cfg!(windows) { r"C:\Developer Tools\ld" } else { "/Developer Tools/ld" });
            assert_eq!(linker_path(absolute.to_str().unwrap().as_bytes(), |_| false), Err(Stage::NotRegularFile));
            for stage in [Stage::Spawn, Stage::MissingStdout, Stage::MissingStderr] {
                let mut failure = Evidence::new(Duration::from_millis(2)).failed(stage, Some(io::Error::from_raw_os_error(2)), Duration::from_millis(3));
                assert!(failure.diagnostic().contains("child_pid=not_spawned"));
                failure.child_pid = Some(123);
                failure.cleanup = Some(Cleanup { already_reaped: false, kill_error: Some(io::Error::from_raw_os_error(3).into()),
                    wait_error: None, status: Some(Status { success: false, code: Some(7), description: "exit 7".into() }), elapsed: Duration::from_millis(1) });
                failure.total_elapsed = Duration::from_millis(4);
                let diagnostic = failure.diagnostic();
                assert!(diagnostic.contains(&format!("stage={stage:?}")));
                assert!(diagnostic.contains("child_pid=123"));
                assert!(diagnostic.contains("errno=Some(2)"));
                assert!(diagnostic.contains("spawn_us=2000 decision_us=3000"));
                assert!(diagnostic.contains("total_us=4000"));
                assert!(diagnostic.contains("kill=(kind="));
            }
        }
    }
}

#[cfg(test)]
fn linker_path(output: &[u8], is_file: impl FnOnce(&Path) -> bool) -> Option<PathBuf> {
    discovery::linker_path(output, is_file).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn compile() -> Vec<OsString> {
        os(&[
            "rustc",
            "--crate-name",
            "app",
            "src/main.rs",
            "--crate-type=bin",
            "--emit=dep-info,link",
            "--out-dir",
            "/target/debug/deps",
            "-Cincremental=/target/debug/incremental",
        ])
    }

    fn codegen_forms(option: &str) -> Vec<Vec<OsString>> {
        vec![
            os(&["-C", option]),
            os(&[&format!("-C{option}")]),
            os(&["--codegen", option]),
            os(&[&format!("--codegen={option}")]),
        ]
    }

    #[test]
    fn smart_is_an_exact_opt_in() {
        assert!(!is_requested(None));
        for value in ["", "off", "on", "SMART", " smart", "smart "] {
            assert!(!is_requested(Some(OsStr::new(value))), "{value:?}");
        }
        assert!(is_requested(Some(OsStr::new("smart"))));
        assert_eq!(supported(), cfg!(target_os = "macos"));
    }

    #[test]
    fn disabled_and_unsupported_modes_do_not_resolve_a_linker() {
        for (requested, supported) in [(false, false), (false, true), (true, false)] {
            assert_eq!(prepare_with(&[], false, requested, supported, || panic!("discovery must be skipped")), None);
        }
    }

    #[test]
    fn supported_debug_binary_compiles_are_eligible() {
        assert_eq!(eligible(&compile()), Ok(()));
        let mut args = compile();
        args[4] = "--crate-type".into();
        args.insert(5, "bin".into());
        assert_eq!(eligible(&args), Ok(()));
        for form in codegen_forms("incremental=/sessions with spaces") {
            let mut args = compile();
            args.pop();
            args.extend(form);
            assert_eq!(eligible(&args), Ok(()));
        }
    }

    #[test]
    fn metadata_and_non_binary_compiles_are_skipped() {
        for emit in ["--emit=dep-info,metadata", "--emit=dep-info", "--emit=metadata", "--emit=asm,link"] {
            let mut args = compile();
            args[5] = emit.into();
            assert!(eligible(&args).is_err(), "{emit}");
        }
        for kind in ["lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro", "bin,rlib", ""] {
            for extra in [os(&["--crate-type", kind]), os(&[&format!("--crate-type={kind}")])] {
                let mut args = compile();
                args.extend(extra);
                assert!(eligible(&args).is_err(), "{kind}");
            }
        }
        let mut implicit = compile();
        implicit.remove(4);
        assert!(eligible(&implicit).is_err());
    }

    #[test]
    fn effective_emit_must_include_link() {
        let mut args = compile();
        args.extend(os(&["--emit", "dep-info,metadata"]));
        assert!(eligible(&args).is_err());
        args.extend(os(&["--emit", "dep-info,metadata,link"]));
        assert_eq!(eligible(&args), Ok(()));
    }

    #[test]
    fn all_codegen_spellings_preserve_explicit_linker_choices() {
        for option in [
            "linker=/usr/bin/cc",
            "linker-flavor=darwin-cc",
            "link-arg=-dead_strip",
            "link-args=-dead_strip -no_uuid",
            "pre-link-arg=-v",
            "pre-link-args=-v -v",
            "prefer-dynamic",
            "prefer-dynamic=no",
            "rpath",
            "rpath=no",
            "link-self-contained=no",
            "linker-plugin-lto=no",
            "linker_flavor=darwin-cc",
        ] {
            for form in codegen_forms(option) {
                let mut args = compile();
                args.extend(form.clone());
                assert!(eligible(&args).is_err(), "{form:?}");
            }
        }
        let mut args = compile();
        args.push("-C=linker=/usr/bin/cc".into());
        assert!(eligible(&args).is_err());
    }

    #[test]
    fn optimisation_and_disabled_incremental_are_skipped() {
        for option in ["opt-level=1", "opt-level=2", "opt-level=3", "opt-level=s", "opt-level=z", "incremental="] {
            for form in codegen_forms(option) {
                let mut args = compile();
                args.extend(form.clone());
                assert!(eligible(&args).is_err(), "{form:?}");
            }
        }
        let mut args = compile();
        args.push("-O".into());
        assert!(eligible(&args).is_err());
        args.pop();
        args.pop();
        assert!(eligible(&args).is_err());
        for form in codegen_forms("opt-level=0") {
            let mut args = compile();
            args.extend(form);
            assert_eq!(eligible(&args), Ok(()));
        }
        let mut args = compile();
        args.push("-C=opt-level=3".into());
        assert!(eligible(&args).is_err());
    }

    #[test]
    fn tests_build_scripts_targets_and_probes_are_skipped() {
        for flags in [
            vec!["--test"],
            vec!["--target", "aarch64-apple-darwin"],
            vec!["--target=aarch64-apple-darwin"],
            vec!["--sysroot", "/custom"],
            vec!["--sysroot=/custom"],
            vec!["@response"],
            vec!["--print", "cfg"],
            vec!["-vV"],
            vec!["--version"],
            vec!["--help"],
            vec!["-Z", "unstable-options"],
            vec!["-Z", "time-passes=invalid"],
            vec!["-Ztime-passes=yes,unstable-options"],
        ] {
            let mut args = compile();
            args.extend(os(&flags));
            assert!(eligible(&args).is_err(), "{flags:?}");
        }
        for name in ["build_script_build", "build_script_custom", "build_script"] {
            let mut args = compile();
            args[2] = name.into();
            assert!(eligible(&args).is_err(), "{name}");
        }
        let mut chain = compile();
        chain.insert(1, "clippy-driver".into());
        assert!(eligible(&chain).is_err());
        assert!(eligible(&os(&["rustc", "-vV"])).is_err());
    }

    #[test]
    fn diagnostic_timing_flags_do_not_change_eligibility() {
        for value in ["time-passes", "time-passes=yes", "time-passes=on", "time-passes=no", "time-passes=false"] {
            for flags in [os(&["-Z", value]), os(&[&format!("-Z{value}")]), os(&[&format!("-Z={value}")])] {
                let mut args = compile();
                args.extend(flags.clone());
                assert_eq!(eligible(&args), Ok(()), "{flags:?}");
            }
        }
    }

    #[test]
    fn ineligible_compiles_and_failed_discovery_leave_argv_unchanged() {
        let mut args = compile();
        args.push("--test".into());
        assert_eq!(prepare_with(&args, false, true, true, || panic!("discovery must be skipped")), None);
        assert_eq!(prepare_with(&compile(), false, true, true, || Err("unavailable")), None);
    }

    #[test]
    fn custom_driver_environment_is_declined() {
        assert!(driver_environment_supported(|_| None));
        assert!(driver_environment_supported(|_| Some(OsString::new())));
        for name in DRIVER_ENV {
            assert!(!driver_environment_supported(|key| (key == *name).then(|| OsString::from("custom"))), "{name}");
        }
    }

    #[test]
    fn path_resolved_cc_must_canonicalize_to_the_system_driver() {
        let canonicalize = |path: &Path| match path.to_str()? {
            "/usr/bin/cc" | "/alias/cc" => Some(PathBuf::from("/system/clang")),
            "/custom/cc" => Some(PathBuf::from("/custom/clang")),
            _ => None,
        };
        assert!(default_driver_is_system(Some(Path::new("/usr/bin/cc")), canonicalize));
        assert!(default_driver_is_system(Some(Path::new("/alias/cc")), canonicalize));
        assert!(!default_driver_is_system(Some(Path::new("/custom/cc")), canonicalize));
        assert!(!default_driver_is_system(Some(Path::new("/missing/cc")), canonicalize));
        assert!(!default_driver_is_system(None, canonicalize));
        assert!(!default_driver_is_system(Some(Path::new("/usr/bin/cc")), |_| None));
    }

    #[test]
    fn native_linker_paths_with_spaces_stay_in_one_argument() {
        let linker = PathBuf::from("/Developer Tools/Toolchains/Default.xctoolchain/usr/bin/ld");
        let args = compile();
        let prepared = prepare_with(&args, false, true, true, || Ok(linker)).unwrap();
        assert_eq!(&prepared[..args.len()], &args);
        assert_eq!(prepared.len(), args.len() + 2);
        assert_eq!(prepared[args.len()], OsString::from("-Clinker=/Developer Tools/Toolchains/Default.xctoolchain/usr/bin/ld"));
        assert_eq!(prepared[args.len() + 1], OsString::from("-Clinker-flavor=ld"));
    }

    #[test]
    fn discovery_output_requires_one_absolute_existing_file() {
        let absolute = PathBuf::from(if cfg!(windows) { r"C:\Developer Tools\ld" } else { "/Developer Tools/ld" });
        let mut output = absolute.to_str().unwrap().as_bytes().to_vec();
        output.push(b'\n');
        assert_eq!(linker_path(&output, |path| path == absolute), Some(absolute));
        assert_eq!(linker_path(&output, |_| false), None);
        for output in [b"ld\n".as_slice(), b"\n", b"/ld\n/other\n", b"/ld\0", b"\xff"] {
            assert_eq!(linker_path(output, |_| true), None, "{output:?}");
        }
    }

    #[test]
    fn non_utf8_arguments_are_declined() {
        let non_utf8 = sys::non_utf8_os_string();
        let mut args = compile();
        args.push(non_utf8.clone());
        assert!(eligible(&args).is_err());
        args.pop();
        args[0] = non_utf8.clone();
        assert!(eligible(&args).is_err());
        assert!(!driver_environment_supported(|_| Some(non_utf8.clone())));
    }
}
