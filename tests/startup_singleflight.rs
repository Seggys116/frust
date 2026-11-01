#![cfg(target_os = "linux")]

mod common;

use std::ffi::CString;
use std::fs::{self, File};
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use common::{Sandbox, assert_success, stdout};
use frust::sys::{self, LockKind};

struct Fixture {
    sb: Sandbox,
    app: PathBuf,
    rustc: PathBuf,
    cargo: PathBuf,
    sysroot: PathBuf,
    gates: PathBuf,
    log: PathBuf,
    manifest: String,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let sb = Sandbox::new(tag);
        let app = sb.project("workspace/app", &[], "fn main() {}\n");
        fs::write(
            app.parent().unwrap().join("Cargo.toml"),
            "[workspace]\nmembers = [\"app\"]\n",
        )
        .unwrap();
        let rustc = sb.root.join("tools/rustc");
        let cargo = sb.root.join("tools/cargo");
        let sysroot = sb.root.join("sysroot");
        let gates = sb.root.join("gates");
        let log = sb.root.join("queries");
        fs::create_dir_all(&gates).unwrap();
        fs::create_dir_all(sysroot.join("bin")).unwrap();
        fs::create_dir_all(sysroot.join("lib/rustlib")).unwrap();
        fs::write(sysroot.join("bin/rustc"), "compiler").unwrap();
        fs::write(sysroot.join("bin/cargo"), "cargo").unwrap();
        fs::write(
            sysroot.join("lib/rustlib/multirust-channel-manifest.toml"),
            "channel",
        )
        .unwrap();
        fs::create_dir_all(rustc.parent().unwrap()).unwrap();
        common::write_fake_tool(
            &rustc,
            "#!/bin/sh\ncase \"$1\" in\n-vV)\n  printf 'rustc-version\\n' >> \"$TEST_LOG\"\n  release=$(cat rust-toolchain 2>/dev/null || printf '9.9.1')\n  printf 'rustc %s (fake)\\nhost: fake-host\\nrelease: %s\\n' \"$release\" \"$release\" ;;\n--print)\n  printf 'rustc-sysroot\\n' >> \"$TEST_LOG\"\n  printf '%s\\n' \"$TEST_SYSROOT\" ;;\n*) exit 1 ;;\nesac\n",
            "",
        );
        common::write_fake_tool(
            &cargo,
            "#!/bin/sh\ncase \"$1\" in\n-V) kind=cargo-version; gate=$TEST_GATE_VERSION ;;\nlocate-project) kind=cargo-locate; gate=$TEST_GATE_LOCATE ;;\n*) exit 1 ;;\nesac\nprintf '%s\\n' \"$kind\" >> \"$TEST_LOG\"\nif [ \"$gate\" = 1 ]; then\n  : > \"$TEST_GATES/$kind-ready-$$\"\n  attempts=0\n  while [ ! -f \"$TEST_GATES/$kind-release\" ]; do\n    attempts=$((attempts + 1))\n    [ \"$attempts\" -lt 2500 ] || exit 1\n    sleep 0.002\n  done\nfi\nif [ \"$kind\" = cargo-version ]; then\n  printf 'cargo 9.9.9 (fake)\\n'\nelif grep -q '^# parent' Cargo.toml; then\n  printf '%s/Cargo.toml\\n' \"$(dirname \"$(pwd -P)\")\"\nelse\n  printf '%s/Cargo.toml\\n' \"$(pwd -P)\"\nfi\n",
            "",
        );
        let manifest = fs::read_to_string(app.join("Cargo.toml")).unwrap();
        // Preserve the existing two-second rejection window for freshly modified sysroot files.
        thread::sleep(Duration::from_millis(2100));
        Self {
            sb,
            app,
            rustc,
            cargo,
            sysroot,
            gates,
            log,
            manifest,
        }
    }

    fn command(&self, gate_version: bool, gate_locate: bool) -> Command {
        let mut command = self.sb.bin("frust");
        command
            .arg("where")
            .current_dir(&self.app)
            .env("FRUST_RUSTC", &self.rustc)
            .env("FRUST_CARGO", &self.cargo)
            .env("RUSTUP_HOME", self.sb.root.join("rustup"))
            .env_remove("RUSTC")
            .env_remove("RUSTUP_TOOLCHAIN")
            .env("TEST_LOG", &self.log)
            .env("TEST_SYSROOT", &self.sysroot)
            .env("TEST_GATES", &self.gates)
            .env("TEST_GATE_VERSION", if gate_version { "1" } else { "0" })
            .env("TEST_GATE_LOCATE", if gate_locate { "1" } else { "0" });
        command
    }

    fn spawn(&self, gate_version: bool, gate_locate: bool) -> Running {
        Running::spawn(self.command(gate_version, gate_locate))
    }

    fn run(&self) -> Output {
        self.spawn(false, false).finish()
    }

    fn count(&self, kind: &str) -> usize {
        fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .filter(|line| *line == kind)
            .count()
    }

    fn ready(&self, kind: &str, count: usize) {
        wait_until(|| {
            fs::read_dir(&self.gates)
                .unwrap()
                .flatten()
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(&format!("{kind}-ready-"))
                })
                .count()
                >= count
        });
    }

    fn release(&self, kind: &str) {
        fs::write(self.gates.join(format!("{kind}-release")), "release").unwrap();
    }

    fn stripe(&self, probe: bool) -> PathBuf {
        let mut hash = blake3::Hasher::new();
        let mut field = |bytes: &[u8]| {
            hash.update(&(bytes.len() as u64).to_le_bytes());
            hash.update(bytes);
        };
        field(if probe { b"probe-slot" } else { b"locate-slot" });
        field(self.app.as_os_str().as_encoded_bytes());
        field(b"none");
        if probe {
            field(self.rustc.as_os_str().as_encoded_bytes());
            field(self.cargo.as_os_str().as_encoded_bytes());
        } else {
            field(b"none");
        }
        let slot = self
            .sb
            .home()
            .join(if probe { "probe" } else { "locate" })
            .join(format!("{}.json", &hash.finalize().to_hex()[..24]));
        let stripe = blake3::hash(slot.as_os_str().as_encoded_bytes()).as_bytes()[0] & 15;
        self.sb
            .home()
            .join("memo-locks")
            .join(format!("{stripe:02}.lock"))
    }
}

struct Running {
    child: Child,
    owns_group: bool,
}

impl Running {
    fn spawn(mut command: Command) -> Self {
        let child = common::own_group(&mut command)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        Self {
            child,
            owns_group: true,
        }
    }

    fn waiting_on(&self, path: &Path) {
        // An open guard descriptor proves the child reached coordination before inputs change.
        wait_until(|| {
            fs::read_dir(format!("/proc/{}/fd", self.child.id()))
                .unwrap()
                .flatten()
                .any(|entry| fs::read_link(entry.path()).ok().as_deref() == Some(path))
        });
    }

    fn exited_unreaped(&self) -> bool {
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        // SAFETY: waitid writes a valid buffer and WNOWAIT leaves our owned child unreaped.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                self.child.id(),
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                return false;
            }
            panic!("observing owned child failed: {error}");
        }
        // SAFETY: successful waitid initialized the SIGCHLD buffer, zeroed when no child exited.
        unsafe { info.assume_init().si_pid() != 0 }
    }

    fn reap_group(&mut self) -> std::io::Result<ExitStatus> {
        if self.owns_group {
            // The unreaped leader pins the group identity until its tool children are killed.
            common::kill_tree(self.child.id());
            self.owns_group = false;
        }
        self.child.wait()
    }

    fn finish(mut self) -> Output {
        wait_until(|| self.exited_unreaped());
        let status = self.reap_group().unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        self.child
            .stdout
            .take()
            .unwrap()
            .read_to_end(&mut stdout)
            .unwrap();
        self.child
            .stderr
            .take()
            .unwrap()
            .read_to_end(&mut stderr)
            .unwrap();
        let output = Output {
            status,
            stdout,
            stderr,
        };
        assert_success(&output);
        output
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.reap_group();
    }
}

fn wait_until(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready() {
        assert!(
            Instant::now() < deadline,
            "controlled child did not reach its expected state"
        );
        thread::sleep(Duration::from_millis(1));
    }
}

struct Held(File);

impl Held {
    fn new(path: &Path) -> Self {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .unwrap();
        sys::try_lock(&file, LockKind::Exclusive).unwrap();
        Self(file)
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        sys::unlock(&self.0).unwrap();
    }
}

#[test]
fn concurrent_where_callers_share_probe_and_workspace_queries() {
    let fixture = Fixture::new("singleflight-cohort");
    let owner = fixture.spawn(true, true);
    fixture.ready("cargo-version", 1);
    let followers: Vec<_> = (0..3).map(|_| fixture.spawn(true, true)).collect();
    for follower in &followers {
        follower.waiting_on(&fixture.stripe(true));
    }
    fixture.release("cargo-version");
    fixture.ready("cargo-locate", 1);
    for follower in &followers {
        follower.waiting_on(&fixture.stripe(false));
    }
    fixture.release("cargo-locate");
    let expected = stdout(&owner.finish());
    for follower in followers {
        assert_eq!(stdout(&follower.finish()), expected);
    }
    assert_eq!(fixture.count("cargo-version"), 1);
    assert_eq!(fixture.count("rustc-version"), 1);
    assert_eq!(fixture.count("rustc-sysroot"), 1);
    assert_eq!(fixture.count("cargo-locate"), 1);
    assert!(
        fs::read_dir(fixture.sb.home().join("memo-locks"))
            .unwrap()
            .count()
            <= 16
    );
}

#[test]
fn warm_hits_do_not_open_or_wait_for_held_guards() {
    let fixture = Fixture::new("singleflight-warm");
    let expected = stdout(&fixture.run());
    let directory = fixture.sb.home().join("memo-locks");
    let saved = fixture.sb.home().join("saved-memo-locks");
    sys::rename(&directory, &saved).unwrap();
    assert_eq!(stdout(&fixture.run()), expected);
    assert!(
        !directory.exists(),
        "a validated hit must not create guard files"
    );
    sys::rename(&saved, &directory).unwrap();
    let probe_path = fixture.stripe(true);
    let locate_path = fixture.stripe(false);
    let _probe = Held::new(&probe_path);
    let _locate = (locate_path != probe_path).then(|| Held::new(&locate_path));
    // Observe opens without a timing threshold that could fail on a busy test host.
    // SAFETY: inotify_init1 has no pointer arguments and returns an owned descriptor.
    let descriptor = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    assert!(descriptor >= 0);
    // SAFETY: the successful inotify descriptor has exactly one owner.
    let mut notifications = unsafe { File::from_raw_fd(descriptor) };
    let directory = CString::new(
        fixture
            .sb
            .home()
            .join("memo-locks")
            .as_os_str()
            .as_encoded_bytes(),
    )
    .unwrap();
    // SAFETY: the descriptor is live and the path remains NUL-terminated through the call.
    assert!(
        unsafe {
            libc::inotify_add_watch(notifications.as_raw_fd(), directory.as_ptr(), libc::IN_OPEN)
        } >= 0
    );
    assert_eq!(stdout(&fixture.run()), expected);
    let mut events = [0u8; 1024];
    assert_eq!(
        notifications.read(&mut events).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
        "a validated hit must not open a guard"
    );
    assert_eq!(fixture.count("cargo-version"), 1);
    assert_eq!(fixture.count("cargo-locate"), 1);
}

#[test]
fn a_crashed_probe_owner_releases_the_waiter() {
    let fixture = Fixture::new("singleflight-crash");
    let mut owner = fixture.spawn(true, false);
    fixture.ready("cargo-version", 1);
    let waiter = fixture.spawn(true, false);
    waiter.waiting_on(&fixture.stripe(true));
    owner.child.kill().unwrap();
    fixture.ready("cargo-version", 2);
    owner.reap_group().unwrap();
    fixture.release("cargo-version");
    waiter.finish();
    assert_eq!(fixture.count("cargo-version"), 2);
    assert_eq!(fixture.count("rustc-version"), 1);
    assert_eq!(fixture.count("cargo-locate"), 1);
}

#[test]
fn waiting_probe_uses_fresh_selection_and_full_sysroot_validation() {
    let fixture = Fixture::new("singleflight-selection");
    fs::write(fixture.app.join("rust-toolchain"), "9.9.1").unwrap();
    let original = stdout(&fixture.run());
    fs::write(fixture.app.join("rust-toolchain"), "9.9.2").unwrap();
    let held = Held::new(&fixture.stripe(true));
    let waiter = fixture.spawn(false, false);
    waiter.waiting_on(&fixture.stripe(true));
    fs::write(fixture.app.join("rust-toolchain"), "9.9.1").unwrap();
    drop(held);
    assert_eq!(stdout(&waiter.finish()), original);
    assert_eq!(
        fixture.count("cargo-version"),
        1,
        "the restored selection can adopt its valid memo"
    );

    fs::write(fixture.app.join("rust-toolchain"), "9.9.2").unwrap();
    let held = Held::new(&fixture.stripe(true));
    let waiter = fixture.spawn(false, false);
    waiter.waiting_on(&fixture.stripe(true));
    fs::write(fixture.app.join("rust-toolchain"), "9.9.1").unwrap();
    fs::write(fixture.sysroot.join("bin/rustc"), "updated compiler").unwrap();
    drop(held);
    assert_eq!(stdout(&waiter.finish()), original);
    assert_eq!(
        fixture.count("cargo-version"),
        2,
        "the restored key cannot adopt an invalid sysroot"
    );
}

#[test]
fn waiting_workspace_query_stores_the_fresh_manifest_key() {
    let fixture = Fixture::new("singleflight-manifest");
    let original = stdout(&fixture.run());
    fs::write(
        fixture.app.join("Cargo.toml"),
        format!("{}\n# pending\n", fixture.manifest),
    )
    .unwrap();
    let held = Held::new(&fixture.stripe(false));
    let waiter = fixture.spawn(false, false);
    waiter.waiting_on(&fixture.stripe(false));
    fs::write(
        fixture.app.join("Cargo.toml"),
        format!("{}\n# parent\n", fixture.manifest),
    )
    .unwrap();
    drop(held);
    let changed = stdout(&waiter.finish());
    assert_ne!(changed, original);
    assert_eq!(stdout(&fixture.run()), changed);
    assert_eq!(fixture.count("cargo-version"), 1);
    assert_eq!(
        fixture.count("cargo-locate"),
        2,
        "the post-wait key should validate on the next call"
    );
}

#[test]
fn another_process_selection_is_never_adopted() {
    let fixture = Fixture::new("singleflight-env");
    let owner = fixture.spawn(true, false);
    fixture.ready("cargo-version", 1);
    let mut command = fixture.command(false, false);
    command.env("RUSTUP_TOOLCHAIN", "other-selection");
    let waiter = Running::spawn(command);
    waiter.waiting_on(&fixture.stripe(true));
    fixture.release("cargo-version");
    owner.finish();
    waiter.finish();
    assert_eq!(
        fixture.count("cargo-version"),
        2,
        "sharing a slot does not mean sharing a complete key"
    );
    assert_eq!(fixture.count("rustc-version"), 2);
}

#[test]
fn held_or_unusable_guards_fail_open_with_bounded_wait() {
    let fixture = Fixture::new("singleflight-timeout");
    let held = Held::new(&fixture.stripe(true));
    let waiter = fixture.spawn(false, false);
    waiter.waiting_on(&fixture.stripe(true));
    let started = Instant::now();
    waiter.finish();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "memo contention must not hold tool startup indefinitely"
    );
    assert_eq!(fixture.count("cargo-version"), 1);
    drop(held);

    let unusable = Fixture::new("singleflight-io");
    fs::write(unusable.sb.home().join("memo-locks"), "not a directory").unwrap();
    let first = stdout(&unusable.run());
    assert_eq!(stdout(&unusable.run()), first);
    assert_eq!(unusable.count("cargo-version"), 1);
    assert_eq!(unusable.count("cargo-locate"), 1);
}
