use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use frust::sys;

mod sessions {
    include!("../src/compact/sessions.rs");

    pub(super) fn at(profile: &Path, now: SystemTime, grace: Duration) -> CleanupReport {
        cleanup_with(profile, grace, now, &mut |path| fs::remove_dir_all(path))
    }

    pub(super) fn remove_with(
        profile: &Path,
        remove: &mut impl FnMut(&Path) -> io::Result<()>,
    ) -> CleanupReport {
        cleanup_with(
            profile,
            Duration::ZERO,
            SystemTime::now() + Duration::from_secs(7200),
            remove,
        )
    }

    pub(super) fn retire_at(
        root: &Path,
        candidates: &[PathBuf],
        now: SystemTime,
    ) -> io::Result<Option<RetirementGuard>> {
        admit_retirement_at(&[(root.to_path_buf(), candidates.to_vec(), true)], now)
    }

    pub(super) fn retire_sets_at(
        sets: &[(PathBuf, Vec<PathBuf>)],
        now: SystemTime,
    ) -> io::Result<Option<RetirementGuard>> {
        let sets: Vec<_> = sets
            .iter()
            .map(|(root, candidates)| (root.clone(), candidates.clone(), true))
            .collect();
        admit_retirement_at(&sets, now)
    }

    pub(super) fn retire_with(
        guard: &mut RetirementGuard,
        candidate: &Path,
        remove: &mut impl FnMut(&Path) -> io::Result<()>,
    ) -> io::Result<()> {
        guard.remove_with(candidate, remove)
    }

    pub(super) fn prune_at(profile: &Path, now: SystemTime, ttl: Duration) -> CleanupReport {
        cleanup_policy_with(
            profile,
            CleanupPolicy {
                grace: Duration::from_secs(3600),
                finalized_ttl: Some(ttl),
            },
            now,
            &mut |path| fs::remove_dir_all(path),
        )
    }

    pub(super) fn detects_activity(path: &Path) -> bool {
        let now = SystemTime::now() + Duration::from_secs(7200);
        let before = snapshot(path, now, Duration::ZERO).unwrap();
        fs::write(path.join("dep-graph.bin"), b"changed activity").unwrap();
        snapshot(path, now, Duration::ZERO).unwrap() != before
    }
}

static SERIAL: Mutex<()> = Mutex::new(());
const BASE: &str = "s-0000000001-0000001";
const GRACE: Duration = Duration::from_secs(3600);

struct Fixture {
    root: PathBuf,
    profile: PathBuf,
    crate_dir: PathBuf,
    lock: PathBuf,
    working: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "frust-session-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let profile = frust::paths::Dirs::at(root.join("home"))
            .targets()
            .join("workspace/toolchain/debug");
        let crate_dir = profile.join("incremental/app-0123456789abc");
        fs::create_dir_all(&crate_dir).unwrap();
        let lock = crate_dir.join(format!("{BASE}.lock"));
        File::create(&lock).unwrap();
        let working = crate_dir.join(format!("{BASE}-working"));
        Self {
            root,
            profile,
            crate_dir,
            lock,
            working,
        }
    }

    fn payload(&self) {
        fs::create_dir(&self.working).unwrap();
        fs::write(self.working.join("dep-graph.bin"), vec![42; 8192]).unwrap();
    }

    // Observe settled fixtures later without decoding or rewriting rustc timestamps.
    fn clean(&self) -> sessions::CleanupReport {
        sessions::at(
            &self.profile,
            SystemTime::now() + Duration::from_secs(7200),
            GRACE,
        )
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "Exercise rustc's whole-file lock protocol independently of the maintenance guard"
)]
fn protocol_lock(file: &File, protocol: &str) -> io::Result<()> {
    #[cfg(unix)]
    if protocol.starts_with("posix") {
        use std::os::fd::AsRawFd;
        // SAFETY: the record is initialized and the live descriptor remains owned by the caller.
        let mut record: libc::flock = unsafe { std::mem::zeroed() };
        record.l_type = if protocol.ends_with("read") {
            libc::F_RDLCK as _
        } else {
            libc::F_WRLCK as _
        };
        record.l_whence = libc::SEEK_SET as _;
        // SAFETY: F_SETLK reads the initialized whole-file record without retaining its pointer.
        return if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &record) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        };
    }
    if protocol.ends_with("read") {
        file.try_lock_shared()
    } else {
        file.try_lock()
    }
    .map_err(io::Error::from)
}

#[test]
fn lock_holder_child() {
    let Some(path) = std::env::var_os("FRUST_SESSION_LOCK_CHILD") else {
        return;
    };
    let protocol = std::env::var("FRUST_SESSION_LOCK_PROTOCOL").unwrap();
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    let locked = protocol_lock(&file, &protocol);
    if std::env::var_os("FRUST_SESSION_LOCK_BLOCKED").is_some() {
        assert!(
            locked.is_err(),
            "{protocol} unexpectedly acquired a held session"
        );
        return;
    }
    locked.unwrap();
    println!("SESSION_LOCK_READY");
    io::stdout().flush().unwrap();
    let _ = io::stdin().read_exact(&mut [0]);
}

fn child_command(path: &Path, protocol: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "lock_holder_child", "--nocapture"])
        .env("FRUST_SESSION_LOCK_CHILD", path)
        .env("FRUST_SESSION_LOCK_PROTOCOL", protocol)
        .env_remove("FRUST_SESSION_LOCK_BLOCKED");
    command
}

struct Holder {
    child: Child,
    // The child test harness still writes its completion record after the lock is released.
    _output: BufReader<std::process::ChildStdout>,
}

impl Holder {
    fn start(path: &Path, protocol: &str) -> Self {
        let mut child = child_command(path, protocol)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap());
        loop {
            let mut line = String::new();
            assert!(
                output.read_line(&mut line).unwrap() > 0,
                "{protocol} child exited before acquiring its lock"
            );
            if line.trim() == "SESSION_LOCK_READY" {
                break;
            }
        }
        Self {
            child,
            _output: output,
        }
    }
}

impl Drop for Holder {
    fn drop(&mut self) {
        drop(self.child.stdin.take());
        let status = self.child.wait().unwrap();
        assert!(status.success(), "lock holder failed: {status}");
    }
}

fn protocols() -> Vec<&'static str> {
    let mut result = vec!["file-read", "file-write"];
    if cfg!(unix) {
        result.extend(["posix-read", "posix-write"])
    }
    result
}

// Separate processes are required because POSIX record locks belong to the process, not a thread.
#[test]
fn child_readers_and_writers_protect_orphans_and_working_payloads() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for protocol in protocols() {
        for working in [false, true] {
            let fixture = Fixture::new();
            if working {
                fixture.payload()
            }
            let holder = Holder::start(&fixture.lock, protocol);
            let report = fixture.clean();
            assert_eq!(
                (report.removed_dirs, report.removed_locks),
                (0, 0),
                "{protocol}: {report:?}"
            );
            assert!(fixture.lock.exists());
            assert_eq!(fixture.working.exists(), working);
            drop(holder);
            let report = fixture.clean();
            assert_eq!(
                (report.removed_dirs, report.removed_locks),
                (usize::from(working), 1),
                "{protocol}: {report:?}"
            );
            assert!(report.errors.is_empty(), "{report:?}");
        }
    }
}

#[test]
fn one_guard_holds_both_protocols_and_drops_them_explicitly() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let fixture = Fixture::new();
    let guard = sys::try_session_lock(&fixture.lock).unwrap();
    assert!(guard.matches_path(&fixture.lock));
    assert!(sys::try_session_lock(&fixture.lock).is_err());
    for protocol in protocols() {
        let output = child_command(&fixture.lock, protocol)
            .env("FRUST_SESSION_LOCK_BLOCKED", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{protocol}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    drop(guard);
    for protocol in protocols() {
        drop(Holder::start(&fixture.lock, protocol))
    }
}

#[test]
fn recent_change_times_and_recent_payloads_are_retained() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let fixture = Fixture::new();
    File::options()
        .write(true)
        .open(&fixture.lock)
        .unwrap()
        .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1))
        .unwrap();
    let report = sessions::cleanup(&fixture.profile, GRACE);
    assert_eq!((report.removed_dirs, report.removed_locks), (0, 0));
    assert!(fixture.lock.exists());
    fixture.payload();
    let future = SystemTime::now() + Duration::from_secs(10_000);
    File::options()
        .write(true)
        .open(fixture.working.join("dep-graph.bin"))
        .unwrap()
        .set_modified(future)
        .unwrap();
    assert_eq!(
        fixture.clean().removed_dirs,
        0,
        "fresh payload must protect an old-looking directory"
    );
    assert!(fixture.working.exists());
}

#[test]
fn finalized_and_uncertain_siblings_protect_lock_identity() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for suffix in ["0000000000000000000000000", "unknown-shape"] {
        let fixture = Fixture::new();
        let finalized = fixture.crate_dir.join(format!("{BASE}-{suffix}"));
        fs::create_dir(&finalized).unwrap();
        assert_eq!(fixture.clean().removed_locks, 0);
        assert!(fixture.lock.exists() && finalized.exists());
        fs::remove_dir(&finalized).unwrap();
        assert_eq!(
            fixture.clean().removed_locks,
            1,
            "orphan after completed-session removal is reclaimed"
        );
    }
}

#[test]
fn working_without_a_lock_and_malformed_or_nonregular_locks_are_retained() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let fixture = Fixture::new();
    fixture.payload();
    fs::remove_file(&fixture.lock).unwrap();
    for name in [
        "s-1-1.lock",
        "s-0000000001-000000A.lock",
        "s-0000000001-0000001-extra.lock",
        "s-0000000001-0000002.lock",
    ] {
        if name.ends_with("0000002.lock") {
            fs::create_dir(fixture.crate_dir.join(name)).unwrap()
        } else {
            fs::write(fixture.crate_dir.join(name), []).unwrap()
        }
    }
    let report = fixture.clean();
    assert_eq!((report.removed_dirs, report.removed_locks), (0, 0));
    assert!(fixture.working.exists());
    assert!(
        !fixture.lock.exists(),
        "cleanup must not create the missing lock"
    );
}

#[test]
fn lock_aliases_and_replaced_paths_are_not_unlinked() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let fixture = Fixture::new();
    let alias = fixture.root.join("lock-alias");
    fs::hard_link(&fixture.lock, &alias).unwrap();
    assert!(sys::try_session_lock(&fixture.lock).is_err());
    assert_eq!(fixture.clean().removed_locks, 0);
    fs::remove_file(alias).unwrap();
    let guard = sys::try_session_lock(&fixture.lock).unwrap();
    sys::rename(&fixture.lock, &fixture.root.join("original.lock")).unwrap();
    fs::write(&fixture.lock, b"replacement").unwrap();
    assert!(!guard.matches_path(&fixture.lock));
    drop(guard);
    assert_eq!(fs::read(&fixture.lock).unwrap(), b"replacement");
}

#[test]
fn removal_failure_keeps_the_lock_and_success_rechecks_path_identity() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let fixture = Fixture::new();
    fixture.payload();
    let report = sessions::remove_with(&fixture.profile, &mut |_| {
        Err(io::Error::from(io::ErrorKind::PermissionDenied))
    });
    assert_eq!((report.removed_dirs, report.removed_locks), (0, 0));
    assert!(!report.errors.is_empty());
    assert!(fixture.lock.exists() && fixture.working.exists());
    let report = sessions::remove_with(&fixture.profile, &mut |path| {
        fs::remove_dir_all(path)?;
        sys::rename(&fixture.lock, &fixture.root.join("replaced.lock"))?;
        fs::write(&fixture.lock, b"replacement")
    });
    assert_eq!((report.removed_dirs, report.removed_locks), (1, 0));
    assert_eq!(fs::read(&fixture.lock).unwrap(), b"replacement");
}

#[test]
fn activity_changes_are_detected_and_external_hardlinks_are_not_counted_as_freed() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let fixture = Fixture::new();
    fixture.payload();
    assert!(sessions::detects_activity(&fixture.working));
    let source = fixture.working.join("dep-graph.bin");
    let outside = fixture.root.join("shared.bin");
    fs::hard_link(&source, &outside).unwrap();
    let directory_bytes = sys::lstat(&fixture.working).unwrap().allocated();
    let report = fixture.clean();
    assert_eq!(report.removed_dirs, 1, "{report:?}");
    assert_eq!(
        report
            .removed
            .iter()
            .find(|(path, _)| path == &fixture.working)
            .unwrap()
            .1,
        directory_bytes
    );
    assert!(outside.exists());
}

#[cfg(unix)]
#[test]
fn symlinked_locks_and_session_contents_are_retained() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let fixture = Fixture::new();
    let outside = fixture.root.join("outside");
    fs::write(&outside, b"preserve").unwrap();
    fs::remove_file(&fixture.lock).unwrap();
    std::os::unix::fs::symlink(&outside, &fixture.lock).unwrap();
    assert!(sys::try_session_lock(&fixture.lock).is_err());
    assert_eq!(fixture.clean().removed_locks, 0);
    fs::remove_file(&fixture.lock).unwrap();
    File::create(&fixture.lock).unwrap();
    fixture.payload();
    std::os::unix::fs::symlink(&outside, fixture.working.join("alias")).unwrap();
    assert_eq!(fixture.clean().removed_dirs, 0);
    assert_eq!(fs::read(outside).unwrap(), b"preserve");
}

#[cfg(unix)]
#[test]
fn unreadable_working_content_is_preserved() {
    use std::os::unix::fs::PermissionsExt;
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if sys::user_id() == 0 {
        return;
    }
    let fixture = Fixture::new();
    fixture.payload();
    fs::set_permissions(&fixture.working, fs::Permissions::from_mode(0o0)).unwrap();
    let report = fixture.clean();
    fs::set_permissions(&fixture.working, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!((report.removed_dirs, report.removed_locks), (0, 0));
    assert!(!report.errors.is_empty(), "{report:?}");
    assert!(fixture.lock.exists() && fixture.working.exists());
}

#[test]
fn large_and_deep_abandoned_sessions_make_complete_cleanup_progress() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let fixture = Fixture::new();
    fixture.payload();
    for index in 0..16_385 {
        fs::write(fixture.working.join(format!("item-{index}.o")), []).unwrap();
    }
    let mut deepest = fixture.working.clone();
    for _ in 0..20 {
        deepest.push("d");
        fs::create_dir(&deepest).unwrap();
    }
    fs::write(deepest.join("data"), b"old nested payload").unwrap();
    let report = fixture.clean();
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!(
        (report.removed_dirs, report.removed_locks),
        (1, 1),
        "{report:?}"
    );
    assert!(!fixture.working.exists() && !fixture.lock.exists());
    assert!(
        report
            .removed
            .iter()
            .any(|(path, bytes)| path == &fixture.working && *bytes > 0)
    );
}

fn finalized(fixture: &Fixture, sequence: usize) -> (PathBuf, PathBuf) {
    let base = format!("s-{sequence:010}-0000001");
    let directory = fixture
        .crate_dir
        .join(format!("{base}-0000000000000000000000000"));
    fs::create_dir(&directory).unwrap();
    fs::write(directory.join("dep-graph.bin"), b"complete metadata").unwrap();
    let lock = fixture.crate_dir.join(format!("{base}.lock"));
    File::create(&lock).unwrap();
    (directory, lock)
}

#[test]
fn batched_guards_cover_every_inode_and_release_partial_acquisition() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let first = Fixture::new();
    let second = Fixture::new();
    let paths = vec![first.lock.clone(), second.lock.clone()];
    let guard = sys::try_session_locks(&paths).unwrap();
    assert!(guard.revalidate());
    for path in &paths {
        for protocol in protocols() {
            let output = child_command(path, protocol)
                .env("FRUST_SESSION_LOCK_BLOCKED", "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
    drop(guard);
    assert!(sys::try_session_locks(&[first.lock.clone(), first.lock.clone()]).is_err());
    for protocol in protocols() {
        let holder = Holder::start(&second.lock, protocol);
        assert!(sys::try_session_locks(&paths).is_err());
        for released in protocols() {
            drop(Holder::start(&first.lock, released))
        }
        drop(holder);
    }
    assert!(sys::try_session_locks(&paths).unwrap().revalidate());
}

// Whole-profile admission must protect every selected crate before any profile is touched.
#[test]
fn retirement_sets_are_atomic_and_finalized_candidates_ignore_unrelated_working_sessions() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let first = Fixture::new();
    let second = Fixture::new();
    let (old, old_lock) = finalized(&first, 2);
    first.payload();
    let root = first.profile.join("incremental");
    let guard = sessions::try_retirement_guard(&root, &old)
        .unwrap()
        .unwrap();
    assert!(guard.revalidate().unwrap());
    drop(guard);
    assert!(
        sessions::try_retirement_guard(&root, &first.crate_dir)
            .unwrap()
            .is_none()
    );
    fs::remove_dir_all(&first.working).unwrap();
    let sets = vec![
        (root.clone(), vec![first.crate_dir.clone()]),
        (
            second.profile.join("incremental"),
            vec![second.crate_dir.clone()],
        ),
    ];
    for protocol in protocols() {
        let holder = Holder::start(&second.lock, protocol);
        assert!(sessions::try_retirement_sets(&sets).unwrap().is_none());
        drop(Holder::start(&old_lock, "file-write"));
        drop(holder);
    }
    let guard = sessions::try_retirement_sets(&sets).unwrap().unwrap();
    assert!(guard.revalidate().unwrap());
    fs::write(old.join("changed"), b"new activity").unwrap();
    assert!(!guard.revalidate().unwrap());
    drop(guard);
    fs::remove_file(&old_lock).unwrap();
    assert!(
        sessions::try_retirement_guard(&root, &old)
            .unwrap()
            .is_none()
    );
}

#[test]
fn empty_retirement_root_revalidation_detects_new_children() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let fixture = Fixture::new();
    fs::remove_file(&fixture.lock).unwrap();
    fs::remove_dir(&fixture.crate_dir).unwrap();
    let root = fixture.profile.join("incremental");
    let guard = sessions::try_retirement_guards(&root, &[])
        .unwrap()
        .unwrap();
    assert!(guard.revalidate().unwrap());
    fs::create_dir(&fixture.crate_dir).unwrap();
    assert!(!guard.revalidate().unwrap());
}

// One held compiler must not prevent reclaiming every other eligible lock in its descriptor batch.
#[test]
fn orphan_batches_drain_around_held_locks_and_remove_empty_crate_skeletons() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let fixture = Fixture::new();
    for sequence in 2..131 {
        File::create(
            fixture
                .crate_dir
                .join(format!("s-{sequence:010}-0000001.lock")),
        )
        .unwrap();
    }
    let holder = Holder::start(&fixture.lock, "file-read");
    let report = fixture.clean();
    assert_eq!(report.removed_locks, 129, "{report:?}");
    assert_eq!(fs::read_dir(&fixture.crate_dir).unwrap().count(), 1);
    drop(holder);
    let report = fixture.clean();
    assert_eq!((report.removed_locks, report.removed_empty_dirs), (1, 1));
    assert!(fixture.profile.join("incremental").exists());
    assert!(!fixture.crate_dir.exists());
}

#[test]
fn distinct_crashed_variants_leave_no_empty_crate_directories() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let fixture = Fixture::new();
    let root = fixture.profile.join("incremental");
    for variant in 0..48 {
        let directory = root.join(format!("app-{variant:013x}"));
        let working = directory.join(format!("{BASE}-working"));
        fs::create_dir_all(&working).unwrap();
        fs::write(working.join("dep-graph.bin"), b"abandoned metadata").unwrap();
        File::create(directory.join(format!("{BASE}.lock"))).unwrap();
    }
    let report = fixture.clean();
    assert_eq!(report.removed_dirs, 48, "{report:?}");
    assert_eq!(report.removed_empty_dirs, 49, "{report:?}");
    assert_eq!(fs::read_dir(root).unwrap().count(), 0);
}

#[test]
fn fresh_empty_crate_is_preserved_until_both_times_pass_grace() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let fixture = Fixture::new();
    fs::remove_file(&fixture.lock).unwrap();
    let report = sessions::cleanup(&fixture.profile, GRACE);
    assert_eq!(report.removed_empty_dirs, 0);
    assert!(fixture.crate_dir.exists());
    assert_eq!(fixture.clean().removed_empty_dirs, 1);
    assert!(fixture.profile.join("incremental").exists());
}

fn maintenance(fixture: &Fixture, idle: Duration) -> frust::compact::AutoOptions {
    fs::create_dir_all(fixture.profile.join("deps")).unwrap();
    fs::create_dir_all(fixture.profile.join(".fingerprint")).unwrap();
    let mut options = frust::compact::AutoOptions::from_env(&frust::config::Config::from_env());
    options.enabled = true;
    options.every = Duration::ZERO;
    options.min_growth = 0;
    options.keep_other_profile = Duration::MAX;
    options.uncached_dead_grace = Duration::MAX;
    options.dead_incremental_grace = GRACE;
    options.incremental_idle = idle;
    options.max_target_bytes = u64::MAX;
    options.cache_enabled = false;
    options.compress = false;
    options
}

// These exercise the complete automatic-maintenance removal paths, beyond the admission helper alone.
#[test]
fn automatic_stale_session_and_metadata_ttl_eviction_respect_orphan_compilers() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for protocol in protocols()
        .into_iter()
        .filter(|name| name.ends_with("read"))
    {
        for whole_crate in [false, true] {
            let fixture = Fixture::new();
            fs::remove_file(&fixture.lock).unwrap();
            let (old, lock) = finalized(&fixture, 2);
            let (newest, _) = finalized(&fixture, 3);
            sys::open_directory_for_times(&old)
                .unwrap()
                .set_modified(SystemTime::now() - Duration::from_secs(7200))
                .unwrap();
            let options = maintenance(
                &fixture,
                if whole_crate {
                    Duration::ZERO
                } else {
                    Duration::MAX
                },
            );
            let dirs = frust::paths::Dirs::at(fixture.root.join("home"));
            let holder = Holder::start(&lock, protocol);
            let report = frust::compact::auto_compact_with(
                &dirs,
                fixture.profile.parent().unwrap(),
                &options,
            );
            assert!(
                old.join("dep-graph.bin").exists(),
                "{protocol} whole={whole_crate}: {report:?}"
            );
            assert!(newest.exists(), "{report:?}");
            drop(holder);
            let report = frust::compact::auto_compact_with(
                &dirs,
                fixture.profile.parent().unwrap(),
                &options,
            );
            assert!(report.errors.is_empty(), "{report:?}");
            assert!(!old.exists(), "{protocol} whole={whole_crate}: {report:?}");
            assert_eq!(fixture.crate_dir.exists(), !whole_crate, "{report:?}");
            if !whole_crate {
                assert!(newest.exists())
            }
        }
    }
}

#[test]
fn caught_unwind_releases_both_protocols_and_recovers_the_serialization_permit() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let first = Fixture::new();
    let second = Fixture::new();
    let paths = vec![first.lock.clone(), second.lock.clone()];
    let result = std::panic::catch_unwind(|| {
        let guard = sys::try_session_locks(&paths).unwrap();
        assert!(guard.revalidate());
        panic!("exercise session guard unwind");
    });
    assert!(result.is_err());
    for path in &paths {
        for protocol in protocols() {
            drop(Holder::start(path, protocol));
        }
    }
    let recovered = sys::try_session_locks(&paths).unwrap();
    assert!(recovered.revalidate());
}

// A failed payload removal must leave every surviving payload's original lock inode available to retry.
#[test]
fn ordered_retirement_keeps_paired_lock_inodes_after_partial_payload_failure() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let fixture = Fixture::new();
    let (first, first_lock) = finalized(&fixture, 2);
    let (second, second_lock) = finalized(&fixture, 3);
    let identity = |path: &Path| {
        let meta = sys::lstat(path).unwrap();
        (meta.dev(), meta.ino())
    };
    let original = [identity(&first_lock), identity(&second_lock)];
    let root = fixture.profile.join("incremental");
    let mut guard = sessions::try_retirement_guard(&root, &fixture.crate_dir)
        .unwrap()
        .unwrap();
    let mut calls = 0;
    let result = sessions::retire_with(&mut guard, &fixture.crate_dir, &mut |path| {
        calls += 1;
        if calls == 2 {
            fs::remove_file(path.join("dep-graph.bin"))?;
            return Err(io::Error::from(io::ErrorKind::PermissionDenied));
        }
        fs::remove_dir_all(path)
    });
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    assert!(!first.exists() && second.is_dir());
    assert_eq!([identity(&first_lock), identity(&second_lock)], original);
    drop(guard);
    let mut retry = sessions::try_retirement_guard(&root, &fixture.crate_dir)
        .unwrap()
        .unwrap();
    retry.remove(&fixture.crate_dir).unwrap();
    assert!(!fixture.crate_dir.exists());
}

#[test]
fn shared_payload_hardlinks_and_prior_owned_root_removals_complete_in_one_guard() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let first = Fixture::new();
    let second = Fixture::new();
    let (old, _) = finalized(&first, 2);
    let (new, _) = finalized(&first, 3);
    let (other, _) = finalized(&second, 2);
    fs::remove_file(new.join("dep-graph.bin")).unwrap();
    fs::remove_file(other.join("dep-graph.bin")).unwrap();
    fs::hard_link(old.join("dep-graph.bin"), new.join("dep-graph.bin")).unwrap();
    fs::hard_link(old.join("dep-graph.bin"), other.join("dep-graph.bin")).unwrap();
    let roots = [
        first.profile.join("incremental"),
        second.profile.join("incremental"),
    ];
    let sets = vec![
        (roots[0].clone(), vec![first.crate_dir.clone()]),
        (roots[1].clone(), vec![second.crate_dir.clone()]),
    ];
    let mut guard = sessions::try_retirement_sets(&sets).unwrap().unwrap();
    guard.remove_root(&roots[0]).unwrap();
    assert!(guard.revalidate().unwrap());
    guard.remove_root(&roots[1]).unwrap();
    assert!(guard.revalidate().unwrap());
    assert!(!roots[0].exists() && !roots[1].exists());
}

#[test]
fn idle_target_retirement_admits_old_working_only_after_full_age_and_lock_proof() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for protocol in protocols()
        .into_iter()
        .filter(|name| name.ends_with("read"))
    {
        let fixture = Fixture::new();
        fixture.payload();
        let root = fixture.profile.join("incremental");
        assert!(
            sessions::try_retirement_guards(&root, std::slice::from_ref(&fixture.crate_dir))
                .unwrap()
                .is_none()
        );
        let future = SystemTime::now() + Duration::from_secs(7200);
        let payload = fixture.working.join("dep-graph.bin");
        File::options()
            .write(true)
            .open(&payload)
            .unwrap()
            .set_modified(future - Duration::from_secs(1))
            .unwrap();
        assert!(
            sessions::retire_at(&root, std::slice::from_ref(&fixture.crate_dir), future)
                .unwrap()
                .is_none()
        );
        File::options()
            .write(true)
            .open(&payload)
            .unwrap()
            .set_modified(SystemTime::now())
            .unwrap();
        let other = Fixture::new();
        other.payload();
        let other_root = other.profile.join("incremental");
        let sets = vec![
            (root.clone(), vec![fixture.crate_dir.clone()]),
            (other_root.clone(), vec![other.crate_dir.clone()]),
        ];
        let holder = Holder::start(&fixture.lock, protocol);
        assert!(sessions::retire_sets_at(&sets, future).unwrap().is_none());
        assert!(fixture.working.join("dep-graph.bin").exists());
        assert!(other.working.join("dep-graph.bin").exists());
        drop(holder);
        let mut guard = sessions::retire_sets_at(&sets, future).unwrap().unwrap();
        guard.remove_root(&root).unwrap();
        assert!(guard.revalidate().unwrap());
        guard.remove_root(&other_root).unwrap();
        assert!(!root.exists() && !other_root.exists());
    }
}

#[test]
fn unexpected_session_after_payload_removal_preserves_every_original_lock() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let fixture = Fixture::new();
    let (payload, lock) = finalized(&fixture, 2);
    let original = sys::lstat(&lock).unwrap().ino();
    let root = fixture.profile.join("incremental");
    let mut guard = sessions::try_retirement_guard(&root, &fixture.crate_dir)
        .unwrap()
        .unwrap();
    let result = sessions::retire_with(&mut guard, &fixture.crate_dir, &mut |path| {
        fs::remove_dir_all(path)?;
        fs::create_dir(fixture.crate_dir.join("s-0000000002-0000001-working"))
    });
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::WouldBlock);
    assert!(!payload.exists());
    assert_eq!(sys::lstat(&lock).unwrap().ino(), original);
    assert!(fixture.lock.exists());
}

// Expired finalized reuse ignores unlink-refreshed ctime while retaining full subtree mtime proof.
#[test]
fn finalized_expiry_batches_many_sessions_and_preserves_parent_idle_times() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let fixture = Fixture::new();
    let incremental = fixture.profile.join("incremental");
    fs::remove_file(&fixture.lock).unwrap();
    let mut previous = None;
    for index in 1..=145 {
        let (session, _) = finalized(&fixture, index);
        if let Some(source) = &previous {
            fs::remove_file(session.join("dep-graph.bin")).unwrap();
            fs::hard_link(source, session.join("dep-graph.bin")).unwrap();
        }
        let old = SystemTime::now() - Duration::from_secs(31 * 86400);
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(session.join("dep-graph.bin"))
            .unwrap()
            .set_modified(old)
            .unwrap();
        sys::open_directory_for_times(&session)
            .unwrap()
            .set_modified(old)
            .unwrap();
        previous = Some(session.join("dep-graph.bin"));
    }
    let times: Vec<_> = [&fixture.profile, &incremental, &fixture.crate_dir]
        .into_iter()
        .map(|path| (path.clone(), sys::lstat(path).unwrap().mtime()))
        .collect();
    let ttl = Duration::from_secs(30 * 86400);
    let report = sessions::prune_at(&fixture.profile, SystemTime::now() + 2 * GRACE, ttl);
    assert_eq!(report.removed_finalized_dirs, 145, "{report:?}");
    assert_eq!(report.removed_locks, 145, "{report:?}");
    assert_eq!(report.removed_empty_dirs, 1, "{report:?}");
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!(fs::read_dir(&incremental).unwrap().count(), 0);
    for (path, before) in times.into_iter().filter(|(path, _)| path.exists()) {
        assert_eq!(
            sys::lstat(&path).unwrap().mtime(),
            before,
            "{}",
            path.display()
        );
    }
}

// Reuse pruning never broadens into output deletion when another compiler retains a session.
#[test]
fn finalized_expiry_preserves_held_recent_unknown_and_program_outputs() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let fixture = Fixture::new();
    fs::remove_file(&fixture.lock).unwrap();
    let (old, lock) = finalized(&fixture, 1);
    let (recent, _) = finalized(&fixture, 2);
    let ttl = Duration::from_secs(30 * 86400);
    let now = SystemTime::now() + ttl + GRACE;
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(recent.join("dep-graph.bin"))
        .unwrap()
        .set_modified(now)
        .unwrap();
    let unknown = fixture.crate_dir.join("s-unknown-format");
    fs::create_dir(&unknown).unwrap();
    for relative in [
        "program",
        "deps/payload",
        ".fingerprint/metadata",
        "build/native/out",
    ] {
        let path = fixture.profile.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"program output must survive reuse pruning").unwrap();
    }
    let holder = Holder::start(&lock, "file-read");
    let held = sessions::prune_at(&fixture.profile, now, ttl);
    assert!(held.errors.is_empty(), "{held:?}");
    assert!(old.exists() && recent.exists() && unknown.exists());
    drop(holder);
    let pruned = sessions::prune_at(&fixture.profile, now, ttl);
    assert_eq!(pruned.removed_finalized_dirs, 1, "{pruned:?}");
    assert!(!old.exists());
    assert!(recent.exists() && unknown.exists());
    assert_eq!(
        sessions::prune_expired(&fixture.profile, ttl).removed_finalized_dirs,
        0
    );
    for relative in [
        "program",
        "deps/payload",
        ".fingerprint/metadata",
        "build/native/out",
    ] {
        assert_eq!(
            fs::read(fixture.profile.join(relative)).unwrap(),
            b"program output must survive reuse pruning"
        );
    }
}
