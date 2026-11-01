//! Idle target retirement retains lock identities and rechecks activity before reclaiming payloads.

#![cfg(unix)]

use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use frust::{compact, config::Config, fsx, gc, lock, paths::Dirs, sys};

// Parent tests serialize retirement and native session lock scenarios.
static SERIAL: Mutex<()> = Mutex::new(());

const SESSION: &str = "s-0000000001-0000001-0000000000000000000000000";
const SESSION_LOCK: &str = "s-0000000001-0000001.lock";

// Each fixture has independent build-lock history and Cargo profile lock inodes.
struct Target {
    root: PathBuf,
    dirs: Dirs,
    target: PathBuf,
    profile: PathBuf,
}

impl Target {
    fn new(recorded: bool) -> Self {
        let root = fsx::temp_sibling(&std::env::temp_dir().join("frust-idle-gc"));
        let dirs = Dirs::at(root.join("home"));
        let target = dirs.targets().join("workspace").join("toolchain");
        let profile = target.join("debug");
        for dir in [
            "deps",
            ".fingerprint/unit",
            "build/unit/out",
            "incremental/unit",
        ] {
            fs::create_dir_all(profile.join(dir)).unwrap();
        }
        fs::create_dir(profile.join("incremental/unit").join(SESSION)).unwrap();
        fs::write(profile.join("incremental/unit").join(SESSION_LOCK), []).unwrap();
        fs::write(
            profile.join("deps/libunit-0123456789abcdef.rlib"),
            vec![7; 2 * 1024 * 1024],
        )
        .unwrap();
        fs::write(profile.join(".fingerprint/unit/lib-unit"), b"fingerprint").unwrap();
        fs::write(profile.join("build/unit/out/generated.rs"), b"generated").unwrap();
        for name in [".cargo-lock", ".cargo-build-lock", ".cargo-artifact-lock"] {
            fs::write(profile.join(name), b"").unwrap();
        }
        let fixture = Self {
            root,
            dirs,
            target,
            profile,
        };
        if recorded {
            drop(fixture.build(&lock::Scope::All));
        }
        fixture.age(Duration::from_secs(40 * 86_400));
        fixture
    }

    fn build(&self, scope: &lock::Scope) -> Box<lock::BuildLock> {
        let request = lock::Request {
            command: "cargo build",
            fingerprint: "",
            scope,
            join: false,
            timeout: Some(Duration::from_secs(1)),
        };
        match lock::acquire(&self.dirs, &self.target, request, &mut |_| {}).unwrap() {
            lock::Acquired::Lock(guard) => guard,
            lock::Acquired::Failed(_) => panic!("unexpected shared failure"),
        }
    }

    fn age(&self, idle: Duration) {
        age_tree(&self.target, idle);
        let level = lock::dir_for(&self.dirs, &self.target).join("lock");
        if level.exists() {
            age(&level, idle);
        }
    }

    fn plan(&self) -> gc::Plan {
        gc::plan_maintenance(&self.dirs, &gc::GcOptions::new(u64::MAX))
    }

    fn has_payload(&self) -> bool {
        self.profile
            .join("deps/libunit-0123456789abcdef.rlib")
            .exists()
    }
}

impl Drop for Target {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn age(path: &Path, idle: Duration) {
    File::open(path)
        .unwrap()
        .set_modified(SystemTime::now() - idle)
        .unwrap();
}

fn age_tree(path: &Path, idle: Duration) {
    let meta = fs::symlink_metadata(path).unwrap();
    if meta.file_type().is_symlink() {
        return;
    }
    if meta.is_dir() {
        for child in fs::read_dir(path).unwrap() {
            age_tree(&child.unwrap().path(), idle);
        }
    }
    age(path, idle);
}

fn retirement_count(plan: &gc::Plan) -> usize {
    plan.removals
        .iter()
        .filter(|r| r.class == gc::Class::TargetDir)
        .count()
}

fn empty_directory(path: &Path) -> bool {
    path.is_dir() && fs::read_dir(path).unwrap().next().is_none()
}

#[test]
fn daily_retirement_reclaims_payload_and_preserves_original_lock_handles() {
    let _serial = SERIAL.lock().unwrap();
    let fixture = Target::new(true);
    let level = lock::dir_for(&fixture.dirs, &fixture.target).join("lock");
    let level_identity = sys::lstat(&level).unwrap();
    let cargo_path = fixture.profile.join(".cargo-lock");
    let cargo_identity = sys::lstat(&cargo_path).unwrap();
    let waiting_handle = File::options()
        .read(true)
        .write(true)
        .open(&cargo_path)
        .unwrap();
    // A pre-opened waiter must still synchronize with later Cargo users after retirement.
    let plan = fixture.plan();
    assert_eq!(retirement_count(&plan), 1);
    let (freed, errors) = gc::execute(&fixture.dirs, &plan);
    assert!(errors.is_empty(), "{errors:?}");
    assert!(freed >= 2 * 1024 * 1024);
    assert!(!fixture.has_payload());
    assert!(fixture.target.is_dir() && fixture.profile.is_dir());
    assert_eq!(sys::lstat(&level).unwrap().ino(), level_identity.ino());
    assert_eq!(sys::lstat(&cargo_path).unwrap().ino(), cargo_identity.ino());
    for name in [".cargo-lock", ".cargo-build-lock", ".cargo-artifact-lock"] {
        assert!(fixture.profile.join(name).is_file());
    }
    sys::try_lock(&waiting_handle, sys::LockKind::Exclusive).unwrap();
    assert!(compact::ProfileLock::try_acquire(&fixture.profile).is_none());
    sys::unlock(&waiting_handle).unwrap();
    assert!(compact::ProfileLock::try_acquire(&fixture.profile).is_some());
    assert!(
        lock::try_target_maintenance(&fixture.dirs, &fixture.target)
            .unwrap()
            .is_some()
    );
    fixture.age(Duration::from_secs(40 * 86_400));
    assert_eq!(retirement_count(&fixture.plan()), 0, "preserved empty roots are not reclaimable payloads");
}

#[test]
fn active_scoped_and_exclusive_builds_and_plain_cargo_block_retirement() {
    let _serial = SERIAL.lock().unwrap();
    for scope in [lock::Scope::All, lock::Scope::slots(["debug@host".into()])] {
        let fixture = Target::new(true);
        let held = fixture.build(&scope);
        fixture.age(Duration::from_secs(40 * 86_400));
        assert!(
            lock::try_target_maintenance(&fixture.dirs, &fixture.target)
                .unwrap()
                .is_none()
        );
        gc::execute(&fixture.dirs, &fixture.plan());
        assert!(fixture.has_payload());
        drop(held);
    }
    let fixture = Target::new(true);
    let held = File::options()
        .read(true)
        .write(true)
        .open(fixture.profile.join(".cargo-lock"))
        .unwrap();
    sys::lock(&held, sys::LockKind::Exclusive).unwrap();
    gc::execute(&fixture.dirs, &fixture.plan());
    assert!(fixture.has_payload());
    sys::unlock(&held).unwrap();
}

#[test]
fn a_completed_noop_failure_or_interruption_refreshes_activity_after_planning() {
    let _serial = SERIAL.lock().unwrap();
    for result in [None, Some((0, false)), Some((1, false)), Some((130, true))] {
        let fixture = Target::new(true);
        let plan = fixture.plan();
        assert_eq!(retirement_count(&plan), 1);
        let held = fixture.build(&lock::Scope::All);
        age(
            &lock::dir_for(&fixture.dirs, &fixture.target).join("lock"),
            Duration::from_secs(40 * 86_400),
        );
        if let Some((code, interrupted)) = result {
            held.finish(lock::BuildResult {
                code,
                units: 0,
                interrupted,
            });
        } else {
            drop(held);
        }
        assert!(
            lock::target_activity(&fixture.dirs, &fixture.target)
                .unwrap()
                .unwrap()
                > SystemTime::now() - Duration::from_secs(60)
        );
        gc::execute(&fixture.dirs, &plan);
        assert!(fixture.has_payload());
    }
}

#[test]
fn recent_deep_cargo_activity_missing_history_and_unsafe_targets_stay() {
    let _serial = SERIAL.lock().unwrap();
    let legacy = Target::new(false);
    assert!(
        lock::target_activity(&legacy.dirs, &legacy.target)
            .unwrap()
            .is_none()
    );
    assert_eq!(retirement_count(&legacy.plan()), 0);
    assert!(!lock::dir_for(&legacy.dirs, &legacy.target).exists());
    for update in [".fingerprint/unit/lib-unit", "build/unit/out/generated.rs"] {
        let fixture = Target::new(true);
        let plan = fixture.plan();
        fs::write(
            fixture.profile.join(update),
            b"a recent Cargo build changed this file",
        )
        .unwrap();
        gc::execute(&fixture.dirs, &plan);
        assert!(fixture.has_payload());
        assert_eq!(retirement_count(&fixture.plan()), 0);
    }
    let fixture = Target::new(true);
    let plan = fixture.plan();
    fs::create_dir(fixture.profile.join("user-data")).unwrap();
    fs::write(fixture.profile.join("user-data/keep"), b"user content").unwrap();
    gc::execute(&fixture.dirs, &plan);
    assert!(fixture.has_payload());
    assert_eq!(
        fs::read(fixture.profile.join("user-data/keep")).unwrap(),
        b"user content"
    );
    let fixture = Target::new(true);
    let plan = fixture.plan();
    fs::create_dir_all(fixture.target.join("release/deps")).unwrap();
    gc::execute(&fixture.dirs, &plan);
    assert!(fixture.has_payload() && fixture.target.join("release/deps").is_dir());
}

#[test]
fn structural_aliases_stay_while_nested_links_are_unlinked_without_following_them() {
    let _serial = SERIAL.lock().unwrap();
    let fixture = Target::new(true);
    let external = fixture.root.join("external");
    fs::create_dir(&external).unwrap();
    fs::write(external.join("keep"), b"external user content").unwrap();
    std::os::unix::fs::symlink(&external, fixture.profile.join("build/unit/out/external")).unwrap();
    fixture.age(Duration::from_secs(40 * 86_400));
    assert_eq!(retirement_count(&fixture.plan()), 0);
    let mut eager = gc::GcOptions::new(u64::MAX);
    eager.target_max_idle = Duration::ZERO;
    let plan = gc::plan_maintenance(&fixture.dirs, &eager);
    assert_eq!(
        retirement_count(&plan),
        1,
        "a nested link is owned as a link, not followed"
    );
    gc::execute(&fixture.dirs, &plan);
    assert!(!fixture.has_payload());
    assert_eq!(
        fs::read(external.join("keep")).unwrap(),
        b"external user content"
    );
    let fixture = Target::new(true);
    let saved = fixture.root.join("saved-build");
    sys::rename(&fixture.profile.join("build"), &saved).unwrap();
    std::os::unix::fs::symlink(&saved, fixture.profile.join("build")).unwrap();
    assert_eq!(
        retirement_count(&gc::plan_maintenance(&fixture.dirs, &eager)),
        0,
        "a structural alias is ambiguous"
    );
    let fixture = Target::new(true);
    let plan = fixture.plan();
    let previous = fixture.target.with_file_name("previous");
    sys::rename(&fixture.target, &previous).unwrap();
    fs::create_dir_all(fixture.profile.join("deps")).unwrap();
    gc::execute(&fixture.dirs, &plan);
    assert!(
        previous
            .join("debug/deps/libunit-0123456789abcdef.rlib")
            .exists()
    );
    assert!(fixture.profile.join("deps").is_dir());
}

#[test]
fn seven_day_metadata_collection_keeps_history_for_thirty_day_expiry() {
    let _serial = SERIAL.lock().unwrap();
    // Metadata expiry must retain the older activity evidence needed by payload expiry.
    let fixture = Target::new(true);
    fixture.age(Duration::from_secs(8 * 86_400));
    let namespace = lock::dir_for(&fixture.dirs, &fixture.target);
    age_tree(&namespace, Duration::from_secs(8 * 86_400));
    let first = fixture.plan();
    assert_eq!(retirement_count(&first), 0);
    assert!(!first.removals.iter().any(|r| r.path == namespace));
    gc::execute(&fixture.dirs, &first);
    assert!(namespace.join("lock").exists());
    fixture.age(Duration::from_secs(40 * 86_400));
    let second = fixture.plan();
    assert_eq!(retirement_count(&second), 1);
    gc::execute(&fixture.dirs, &second);
    assert!(!fixture.has_payload());
    assert!(namespace.join("lock").exists());
}

#[test]
fn maintenance_guard_is_nonblocking_and_does_not_publish_or_refresh_activity() {
    let _serial = SERIAL.lock().unwrap();
    let fixture = Target::new(true);
    let namespace = lock::dir_for(&fixture.dirs, &fixture.target);
    let before = lock::target_activity(&fixture.dirs, &fixture.target).unwrap();
    let children: Vec<_> = fs::read_dir(&namespace)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    let held = lock::try_target_maintenance(&fixture.dirs, &fixture.target)
        .unwrap()
        .unwrap();
    assert_eq!(Some(held.activity().unwrap()), before);
    assert!(
        lock::try_target_maintenance(&fixture.dirs, &fixture.target)
            .unwrap()
            .is_none()
    );
    drop(held);
    assert_eq!(
        lock::target_activity(&fixture.dirs, &fixture.target).unwrap(),
        before
    );
    assert_eq!(
        fs::read_dir(&namespace)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect::<Vec<_>>(),
        children
    );
    let mut unverified = gc::Plan::default();
    unverified.removals.push(gc::Removal {
        class: gc::Class::TargetDir,
        path: fixture.target.clone(),
        bytes: 1,
    });
    gc::execute(&fixture.dirs, &unverified);
    assert!(
        fixture.has_payload(),
        "an unverified target removal must never unlink its root"
    );
    let explicit = gc::plan(&fixture.dirs, &gc::GcOptions::new(u64::MAX));
    assert_eq!(retirement_count(&explicit), 1);
    gc::execute(&fixture.dirs, &explicit);
    assert!(fixture.target.is_dir() && fixture.profile.is_dir());
    assert!(!fixture.has_payload());
}

#[test]
fn foreground_sampling_is_bounded_and_background_drains_the_remaining_backlog() {
    let _serial = SERIAL.lock().unwrap();
    let fixture = Target::new(true);
    let mut targets = vec![fixture.target.clone()];
    for name in [
        "second-toolchain",
        "third-toolchain",
        "fourth-toolchain",
        "fifth-toolchain",
        "sixth-toolchain",
    ] {
        let target = fixture.target.with_file_name(name);
        fsx::clone_new(&fixture.profile, &target.join("debug")).unwrap();
        let scope = lock::Scope::All;
        let request = lock::Request {
            command: "cargo build",
            fingerprint: "",
            scope: &scope,
            join: false,
            timeout: Some(Duration::from_secs(1)),
        };
        match lock::acquire(&fixture.dirs, &target, request, &mut |_| {}).unwrap() {
            lock::Acquired::Lock(guard) => drop(guard),
            lock::Acquired::Failed(_) => panic!("unexpected shared failure"),
        }
        age_tree(&target, Duration::from_secs(40 * 86_400));
        age(
            &lock::dir_for(&fixture.dirs, &target).join("lock"),
            Duration::from_secs(40 * 86_400),
        );
        targets.push(target);
    }
    let first = fixture.plan();
    assert_eq!(retirement_count(&first), 2);
    gc::execute(&fixture.dirs, &first);
    assert_eq!(
        targets
            .iter()
            .filter(|t| t.join("debug/deps/libunit-0123456789abcdef.rlib").exists())
            .count(),
        4
    );
    let mut cfg = Config::from_env();
    cfg.cache_max_bytes = u64::MAX;
    assert_eq!(gc::run_auto_child(&fixture.dirs, &cfg), 0);
    assert!(
        targets
            .iter()
            .all(|t| t.is_dir() && empty_directory(&t.join("debug/deps")) && empty_directory(&t.join("debug/.fingerprint")))
    );
}

#[test]
fn background_retirement_has_no_foreground_node_or_depth_ceiling() {
    let _serial = SERIAL.lock().unwrap();
    let fixture = Target::new(true);
    let unit = fixture.profile.join(".fingerprint/unit");
    // Hardlinks exceed the foreground entry budget without allocating thousands of payload blocks.
    for index in 0..32_769 {
        fs::hard_link(unit.join("lib-unit"), unit.join(format!("node-{index}"))).unwrap();
    }
    let mut deep = fixture.profile.join("build/unit/out");
    for _ in 0..70 {
        deep.push("d");
    }
    fs::create_dir_all(&deep).unwrap();
    fs::write(deep.join("generated.rs"), b"deep generated output").unwrap();
    let idle = Duration::from_secs(40 * 86_400);
    age(&unit, idle);
    age_tree(&fixture.profile.join("build"), idle);
    assert_eq!(
        retirement_count(&fixture.plan()),
        0,
        "foreground sampling must decline an incomplete activity proof"
    );
    let mut cfg = Config::from_env();
    cfg.cache_max_bytes = u64::MAX;
    assert_eq!(gc::run_auto_child(&fixture.dirs, &cfg), 0);
    assert!(!fixture.has_payload());
    assert!(fixture.target.is_dir() && fixture.profile.is_dir());
    assert!(empty_directory(&fixture.profile.join(".fingerprint")));
    assert!(empty_directory(&fixture.profile.join("deps")));
}

#[test]
fn synchronous_collection_drains_more_than_the_foreground_target_quota() {
    let _serial = SERIAL.lock().unwrap();
    let fixture = Target::new(true);
    let mut targets = vec![fixture.target.clone()];
    for name in ["second-toolchain", "third-toolchain", "fourth-toolchain"] {
        let target = fixture.target.with_file_name(name);
        fsx::clone_new(&fixture.profile, &target.join("debug")).unwrap();
        let scope = lock::Scope::All;
        let request = lock::Request {
            command: "cargo build",
            fingerprint: "",
            scope: &scope,
            join: false,
            timeout: Some(Duration::from_secs(1)),
        };
        match lock::acquire(&fixture.dirs, &target, request, &mut |_| {}).unwrap() {
            lock::Acquired::Lock(guard) => drop(guard),
            lock::Acquired::Failed(_) => panic!("unexpected shared failure"),
        }
        age_tree(&target, Duration::from_secs(40 * 86_400));
        age(
            &lock::dir_for(&fixture.dirs, &target).join("lock"),
            Duration::from_secs(40 * 86_400),
        );
        targets.push(target);
    }
    let mut cfg = Config::from_env();
    cfg.cache_max_bytes = u64::MAX;
    cfg.gc_sync = true;
    assert!(gc::maybe_auto_gc(&fixture.dirs, &cfg));
    assert!(
        targets
            .iter()
            .all(|t| t.is_dir() && empty_directory(&t.join("debug/deps")) && empty_directory(&t.join("debug/.fingerprint")))
    );
    assert!(!gc::maybe_auto_gc(&fixture.dirs, &cfg));
}

#[test]
fn cargo_uplift_hardlinks_and_dep_info_are_owned_profile_payloads() {
    let _serial = SERIAL.lock().unwrap();
    let fixture = Target::new(true);
    // Cargo uplifts share a dependency inode; their standalone dep-info remains disposable.
    let original = fixture.profile.join("deps/libunit-0123456789abcdef.rlib");
    fs::hard_link(&original, fixture.profile.join("unit")).unwrap();
    fs::write(fixture.profile.join("unit.d"), b"unit: src/main.rs\n").unwrap();
    fixture.age(Duration::from_secs(40 * 86_400));
    let plan = fixture.plan();
    assert_eq!(retirement_count(&plan), 1);
    let (freed, errors) = gc::execute(&fixture.dirs, &plan);
    assert!(errors.is_empty(), "{errors:?}");
    assert!(freed >= 2 * 1024 * 1024);
    assert!(!original.exists() && !fixture.profile.join("unit").exists());
    assert!(!fixture.profile.join("unit.d").exists());
    assert!(fixture.profile.is_dir());
}

#[test]
fn gc_session_lock_holder_child() {
    let Some(path) = std::env::var_os("FRUST_GC_SESSION_LOCK_CHILD") else { return };
    let file = File::options().read(true).write(true).open(path).unwrap();
    use std::os::fd::AsRawFd;
    // SAFETY: the initialized record and live descriptor remain valid throughout F_SETLK.
    let mut record: libc::flock = unsafe { std::mem::zeroed() };
    record.l_type = libc::F_RDLCK as _;
    record.l_whence = libc::SEEK_SET as _;
    // SAFETY: F_SETLK reads this initialized whole-file record without retaining its pointer.
    assert_eq!(unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &record) }, 0);
    println!("GC_SESSION_LOCK_READY");
    io::stdout().flush().unwrap();
    let _ = io::stdin().read_exact(&mut [0]);
    drop(file);
}

struct SessionHolder {
    child: Child,
    _output: BufReader<std::process::ChildStdout>,
}

impl SessionHolder {
    fn start(path: &Path) -> Self {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "gc_session_lock_holder_child", "--nocapture"])
            .env("FRUST_GC_SESSION_LOCK_CHILD", path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn().unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap());
        loop {
            let mut line = String::new();
            assert!(output.read_line(&mut line).unwrap() > 0, "child exited before acquiring its rustc-compatible read lock");
            if line.trim() == "GC_SESSION_LOCK_READY" {
                break;
            }
        }
        Self { child, _output: output }
    }
}

impl Drop for SessionHolder {
    fn drop(&mut self) {
        drop(self.child.stdin.take());
        assert!(self.child.wait().unwrap().success());
    }
}

#[test]
fn orphan_rustc_session_reader_protects_explicit_gc_until_release() {
    let _serial = SERIAL.lock().unwrap();
    let fixture = Target::new(true);
    let krate = fixture.profile.join("incremental/unit");
    let older = krate.join(SESSION);
    let newer = krate.join("s-0000000002-0000002-0000000000000000000000000");
    fs::create_dir(&newer).unwrap();
    fs::write(krate.join("s-0000000002-0000002.lock"), []).unwrap();
    fs::write(older.join("dep-graph.bin"), b"old session graph").unwrap();
    fixture.age(Duration::from_secs(40 * 86_400));
    age(&newer, Duration::from_secs(39 * 86_400));
    let holder = SessionHolder::start(&krate.join(SESSION_LOCK));
    let mut opts = gc::GcOptions::new(u64::MAX);
    opts.target_max_idle = Duration::from_secs(365 * 86_400);
    let plan = gc::plan(&fixture.dirs, &opts);
    assert!(plan.removals.iter().any(|r| r.class == gc::Class::IncrementalSession && r.path == older));
    gc::execute(&fixture.dirs, &plan);
    assert!(older.is_dir() && newer.is_dir());
    assert!(fixture.has_payload());
    drop(holder);
    gc::execute(&fixture.dirs, &plan);
    assert!(!older.exists() && newer.is_dir());
    assert!(fixture.has_payload());
}

#[test]
fn orphan_rustc_session_reader_prevents_every_profile_in_idle_target_retirement() {
    let _serial = SERIAL.lock().unwrap();
    let fixture = Target::new(true);
    let release = fixture.target.join("release");
    fsx::clone_new(&fixture.profile, &release).unwrap();
    let release_payload = release.join("deps/libunit-0123456789abcdef.rlib");
    fixture.age(Duration::from_secs(40 * 86_400));
    // Cargo and frust have exited; only an independent compiler session reader remains.
    let holder = SessionHolder::start(&release.join("incremental/unit").join(SESSION_LOCK));
    let plan = fixture.plan();
    assert_eq!(retirement_count(&plan), 1);
    let (freed, errors) = gc::execute(&fixture.dirs, &plan);
    assert!(freed < 2 * 1024 * 1024);
    assert!(errors.iter().any(|(_, error)| error.kind() == io::ErrorKind::WouldBlock));
    assert!(fixture.has_payload() && release_payload.exists());
    assert!(release.join("incremental/unit").join(SESSION).is_dir());
    drop(holder);
    let plan = fixture.plan();
    assert_eq!(retirement_count(&plan), 1);
    let (freed, errors) = gc::execute(&fixture.dirs, &plan);
    assert!(errors.is_empty(), "{errors:?}");
    assert!(freed >= 2 * 1024 * 1024);
    assert!(!fixture.has_payload() && !release_payload.exists());
    assert!(fixture.profile.is_dir() && release.is_dir());
}

#[test]
fn interrupted_retirement_keeps_profile_discovery_and_retries_remaining_payloads() {
    let _serial = SERIAL.lock().unwrap();
    let fixture = Target::new(true);
    let incremental = fixture.profile.join("incremental/unit").join(SESSION).join("dep-graph.bin");
    fs::write(&incremental, vec![9; 64 * 1024]).unwrap();
    fixture.age(Duration::from_secs(40 * 86_400));
    let level = lock::dir_for(&fixture.dirs, &fixture.target).join("lock");
    let mut retained = vec![fixture.target.clone(), fixture.profile.clone(), level];
    for name in ["deps", ".fingerprint", ".cargo-lock", ".cargo-build-lock", ".cargo-artifact-lock"] {
        retained.push(fixture.profile.join(name));
    }
    let identities: Vec<_> = retained.iter().map(|path| {
        let meta = sys::lstat(path).unwrap();
        (path.clone(), meta.dev(), meta.ino())
    }).collect();
    let stale = fixture.plan();
    assert_eq!(retirement_count(&stale), 1);
    {
        let _target = lock::try_target_maintenance(&fixture.dirs, &fixture.target).unwrap().unwrap();
        let _profile = compact::ProfileLock::try_acquire(&fixture.profile).unwrap();
        // Model interruption after clearing structural children while retaining their activity timestamps.
        for name in ["deps", ".fingerprint"] {
            let root = fixture.profile.join(name);
            let directory = File::open(&root).unwrap();
            let modified = directory.metadata().unwrap().modified().unwrap();
            for entry in fs::read_dir(&root).unwrap() {
                fsx::remove_any(&entry.unwrap().path()).unwrap();
            }
            directory.set_modified(modified).unwrap();
        }
    }
    assert!(!fixture.has_payload());
    assert!(incremental.is_file() && fixture.profile.join("build/unit/out/generated.rs").is_file());
    let (freed, errors) = gc::execute(&fixture.dirs, &stale);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(freed, 0, "the interrupted snapshot must be replanned");
    let next = fixture.plan();
    assert_eq!(retirement_count(&next), 1, "the next daily pass still discovers the profile");
    let (freed, errors) = gc::execute(&fixture.dirs, &next);
    assert!(errors.is_empty(), "{errors:?}");
    assert!(freed >= 64 * 1024);
    assert!(!incremental.exists() && !fixture.profile.join("build").exists());
    for (path, dev, ino) in identities {
        let meta = sys::lstat(&path).unwrap();
        assert_eq!((meta.dev(), meta.ino()), (dev, ino), "{} changed identity", path.display());
    }
    assert!(empty_directory(&fixture.profile.join("deps")));
    assert!(empty_directory(&fixture.profile.join(".fingerprint")));
    assert_eq!(retirement_count(&fixture.plan()), 0);
}

#[test]
fn native_session_contention_retains_payloads_until_release() {
    let _serial = SERIAL.lock().unwrap();
    let other = Target::new(true);
    let fixture = Target::new(true);
    let plan = fixture.plan();
    assert_eq!(retirement_count(&plan), 1);
    let lock_path = fixture.profile.join("incremental/unit").join(SESSION_LOCK);
    let held = sys::try_session_lock(&lock_path).unwrap();
    let (freed, errors) = gc::execute(&fixture.dirs, &plan);
    assert_eq!(freed, 0);
    assert!(errors.iter().any(|(_, error)| error.kind() == io::ErrorKind::WouldBlock));
    assert!(fixture.has_payload() && other.has_payload());
    drop(held);
    let (freed, errors) = gc::execute(&fixture.dirs, &plan);
    assert!(errors.is_empty(), "{errors:?}");
    assert!(freed >= 2 * 1024 * 1024);
    assert!(!fixture.has_payload() && other.has_payload());
    assert!(empty_directory(&fixture.profile.join("deps")));
    assert!(empty_directory(&fixture.profile.join(".fingerprint")));
}

#[cfg(target_os = "macos")]
#[test]
fn unrelated_native_session_guard_allows_idle_target_retirement() {
    let _serial = SERIAL.lock().unwrap();
    let other = Target::new(true);
    let fixture = Target::new(true);
    let plan = fixture.plan();
    assert_eq!(retirement_count(&plan), 1);
    let lock_path = other.profile.join("incremental/unit").join(SESSION_LOCK);
    let held = sys::try_session_lock(&lock_path).unwrap();
    let probe = File::options().read(true).write(true).open(&lock_path).unwrap();
    assert_eq!(io::Error::from(sys::try_lock(&probe, sys::LockKind::Exclusive).unwrap_err()).kind(), io::ErrorKind::WouldBlock);
    let (freed, errors) = gc::execute(&fixture.dirs, &plan);
    assert!(errors.is_empty(), "{errors:?}");
    assert!(freed >= 2 * 1024 * 1024);
    assert!(!fixture.has_payload() && other.has_payload());
    assert!(empty_directory(&fixture.profile.join("deps")));
    assert!(empty_directory(&fixture.profile.join(".fingerprint")));
    assert!(other.profile.join("incremental/unit").join(SESSION).is_dir());
    assert_eq!(io::Error::from(sys::try_lock(&probe, sys::LockKind::Exclusive).unwrap_err()).kind(), io::ErrorKind::WouldBlock);
    drop(held);
    sys::try_lock(&probe, sys::LockKind::Exclusive).unwrap();
    sys::unlock(&probe).unwrap();
}

#[test]
fn gc_low_fd_retirement_child() {
    let Some(case) = std::env::var_os("FRUST_GC_LOW_FD_CASE") else { return };
    let case = case.to_str().unwrap();
    let ceiling = if case == "single-crate" { 64 } else { 96 };
    // SAFETY: initialized storage receives this child process's descriptor limits.
    let mut limit: libc::rlimit = unsafe { std::mem::zeroed() };
    // SAFETY: getrlimit writes only the live initialized record.
    assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) }, 0);
    limit.rlim_cur = limit.rlim_cur.min(ceiling);
    // SAFETY: only this dedicated child process's soft limit changes; its hard limit is retained.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
    let fixture = Target::new(true);
    let incremental = fixture.profile.join("incremental");
    for index in 2..=161 {
        let krate = incremental.join(if case == "many-crates" { format!("unit-{index}") } else { "unit".into() });
        fs::create_dir_all(&krate).unwrap();
        let base = format!("s-{index:010}-{index:07}");
        let session = krate.join(format!("{base}-0000000000000000000000000"));
        fs::create_dir(&session).unwrap();
        fs::write(session.join("dep-graph.bin"), vec![9; 8192]).unwrap();
        fs::write(krate.join(format!("{base}.lock")), []).unwrap();
    }
    fixture.age(Duration::from_secs(40 * 86_400));
    let mut retained = vec![fixture.target.clone(), fixture.profile.clone(), lock::dir_for(&fixture.dirs, &fixture.target).join("lock")];
    for name in ["deps", ".fingerprint", ".cargo-lock", ".cargo-build-lock", ".cargo-artifact-lock"] {
        retained.push(fixture.profile.join(name));
    }
    let identities: Vec<_> = retained.iter().map(|path| {
        let meta = sys::lstat(path).unwrap();
        (path.clone(), meta.dev(), meta.ino())
    }).collect();
    let holder = (case == "held-session").then(|| SessionHolder::start(&incremental.join("unit").join(SESSION_LOCK)));
    let plan = fixture.plan();
    assert_eq!(retirement_count(&plan), 1);
    let (freed, errors) = gc::execute(&fixture.dirs, &plan);
    assert!(freed >= 8192, "expired reuse must progress below the descriptor limit: {errors:?}");
    if let Some(holder) = holder {
        assert!(errors.iter().any(|(_, error)| error.kind() == io::ErrorKind::WouldBlock));
        assert!(fixture.has_payload());
        assert!(fixture.profile.join(".fingerprint/unit/lib-unit").is_file());
        assert!(fixture.profile.join("build/unit/out/generated.rs").is_file());
        assert!(incremental.join("unit").join(SESSION).is_dir());
        assert!(!incremental.join("unit/s-0000000161-0000161-0000000000000000000000000").exists());
        drop(holder);
        let retry = fixture.plan();
        assert_eq!(retirement_count(&retry), 1);
        let (_, errors) = gc::execute(&fixture.dirs, &retry);
        assert!(errors.is_empty(), "{errors:?}");
    } else {
        assert!(errors.is_empty(), "{errors:?}");
    }
    assert!(!fixture.has_payload() && !incremental.exists());
    assert!(empty_directory(&fixture.profile.join("deps")));
    assert!(empty_directory(&fixture.profile.join(".fingerprint")));
    for (path, dev, ino) in identities {
        let meta = sys::lstat(&path).unwrap();
        assert_eq!((meta.dev(), meta.ino()), (dev, ino), "{} changed identity", path.display());
    }
}

#[test]
fn expired_incremental_retirement_scales_below_child_descriptor_limits() {
    let _serial = SERIAL.lock().unwrap();
    for case in ["single-crate", "many-crates", "held-session"] {
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "gc_low_fd_retirement_child", "--nocapture"])
            .env("FRUST_GC_LOW_FD_CASE", case).output().unwrap();
        assert!(output.status.success(), "{case}: {}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    }
}
