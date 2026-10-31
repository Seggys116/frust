//! Automatic maintenance: the detached daily collector and the cheap, resumable automatic compaction pass.

mod common;

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{Sandbox, assert_success, stderr, stdout};
use frust::compact::{self, AutoOptions};
use frust::config::Config;
use frust::gc;
use frust::paths::Dirs;

fn fake_entry(home: &Path, key: &str) -> PathBuf {
    let dir = home.join("store").join(&key[..2]).join(key);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("meta.json"), b"{}").unwrap();
    fs::write(dir.join("f0"), vec![7u8; 64 * 1024]).unwrap();
    dir
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn the_hidden_collector_entry_collects_and_prints_nothing() {
    let sb = Sandbox::new("gc-child");
    let entry = fake_entry(&sb.home(), "aa11111111111111111111111111111111111111");
    let out = sb.bin("frust").args(["gc", gc::AUTO_CHILD_FLAG]).env("FRUST_CACHE_MAX_GIB", "0").output().unwrap();
    assert_success(&out);
    assert!(stdout(&out).is_empty() && stderr(&out).is_empty(), "stdout {:?} stderr {:?}", stdout(&out), stderr(&out));
    assert!(!entry.exists(), "entries over the budget are evicted");
}

#[test]
fn a_second_collector_yields_to_a_running_one() {
    let sb = Sandbox::new("gc-child-lock");
    let entry = fake_entry(&sb.home(), "bb22222222222222222222222222222222222222");
    let marker = File::options().create(true).truncate(false).write(true).open(sb.home().join("auto-gc")).unwrap();
    frust::sys::lock(&marker, frust::sys::LockKind::Exclusive).unwrap();
    let out = sb.bin("frust").args(["gc", gc::AUTO_CHILD_FLAG]).env("FRUST_CACHE_MAX_GIB", "0").output().unwrap();
    assert_success(&out);
    assert!(entry.exists(), "the collector must not run while another holds the marker");
    drop(marker);
    assert_success(&sb.bin("frust").args(["gc", gc::AUTO_CHILD_FLAG]).env("FRUST_CACHE_MAX_GIB", "0").output().unwrap());
    assert!(!entry.exists());
}

#[test]
fn the_daily_collection_is_claimed_once_and_completes_in_the_background() {
    let sb = Sandbox::new("gc-claim");
    let home = sb.home();
    let dirs = Dirs::at(&home);
    let entry = fake_entry(&home, "cc33333333333333333333333333333333333333");
    let mut cfg = Config::from_env();
    cfg.cache_max_bytes = 0;

    assert!(gc::maybe_auto_gc(&dirs, &cfg), "the first caller claims the day");
    assert!(home.join("auto-gc").is_file(), "the claim is recorded before any work");
    assert!(!gc::maybe_auto_gc(&dirs, &cfg), "a later caller sees the claim and does nothing");
    wait_until("the collector to evict the entry", || !entry.exists());
}

fn fake_target(sb: &Sandbox) -> (Dirs, PathBuf, PathBuf) {
    let dirs = Dirs::at(sb.home());
    let target = sb.home().join("targets").join("ws-0123456789ab").join("tc");
    let deps = target.join("debug").join("deps");
    fs::create_dir_all(&deps).unwrap();
    for n in 0..3 {
        fs::write(deps.join(format!("libx{n}-0123456789abcdef.rlib")), vec![n as u8; 40_000]).unwrap();
    }
    (dirs, target, deps)
}

fn eager() -> AutoOptions {
    let mut opts = AutoOptions::from_env(&Config::from_env());
    opts.enabled = true;
    opts.every = Duration::ZERO;
    opts
}

#[test]
fn an_unchanged_target_is_not_walked_again_and_a_build_wakes_it() {
    let sb = Sandbox::new("maint-unchanged");
    let (dirs, target, deps) = fake_target(&sb);

    let first = compact::auto_compact_with(&dirs, &target, &eager());
    assert!(first.skipped.is_none(), "{:?}", first.skipped);
    let second = compact::auto_compact_with(&dirs, &target, &eager());
    assert_eq!(second.skipped, Some("nothing changed since the last pass"));
    assert_eq!(second.allocated_after, first.allocated_after);

    fs::write(deps.join("libnew-fedcba9876543210.rlib"), vec![9u8; 40_000]).unwrap();
    let third = compact::auto_compact_with(&dirs, &target, &eager());
    // A busy skip does not assess the new tree.
    assert!(matches!(third.skipped, None | Some("grew too little since the last pass")), "a new artifact must be assessed: {third:?}");
    let fourth = compact::auto_compact_with(&dirs, &target, &eager());
    assert_eq!(fourth.skipped, Some("nothing changed since the last pass"), "the assessment of the new state is remembered");

    let mut other = eager();
    other.compress = false;
    assert_ne!(compact::auto_compact_with(&dirs, &target, &other).skipped, Some("nothing changed since the last pass"), "different settings are a different pass");
}

#[test]
fn a_pass_that_ran_out_of_budget_is_resumed_not_skipped() {
    let sb = Sandbox::new("maint-budget");
    let (dirs, target, deps) = fake_target(&sb);
    let mut capped = eager();
    capped.pass_bytes = 0;
    let first = compact::auto_compact_with(&dirs, &target, &capped);
    if !first.supported {
        return;
    }
    assert!(first.budget_stopped, "{first:?}");
    assert_eq!(first.compressed_files + first.deduped_files, 0, "a stopped pass starts no new file");
    let before: Vec<_> = fs::read_dir(&deps).unwrap().flatten().map(|e| e.file_name()).collect();

    let second = compact::auto_compact_with(&dirs, &target, &eager());
    assert!(!second.budget_stopped, "{second:?}");
    assert_ne!(second.skipped, Some("nothing changed since the last pass"));
    let after: Vec<_> = fs::read_dir(&deps).unwrap().flatten().map(|e| e.file_name()).collect();
    assert_eq!(before.len(), after.len(), "no artifact is lost by stopping and resuming");
}

#[test]
fn incomplete_passes_retry_before_the_default_success_interval_without_losing_success_time() {
    let sb = Sandbox::new("maint-retry-defaults");
    let (dirs, target, deps) = fake_target(&sb);
    // Keep the production ten-minute throttle and control only the persisted marker's age.
    let mut opts = AutoOptions::from_env(&Config::from_env());
    opts.enabled = true;
    opts.compress = false;
    assert_eq!(opts.every, Duration::from_secs(600));
    let first = compact::auto_compact_with(&dirs, &target, &opts);
    assert!(first.errors.is_empty() && first.skipped.is_none(), "{first:?}");
    let marker = target.join(compact::AUTO_MARKER);
    let success: serde_json::Value = serde_json::from_slice(&fs::read(&marker).unwrap()).unwrap();
    let old_success = success["at_unix"].as_u64().unwrap() - 3600;
    let mut previous = success;
    previous["at_unix"] = old_success.into();
    fs::write(&marker, serde_json::to_vec(&previous).unwrap()).unwrap();
    common::age(&marker, Duration::from_secs(601));
    let lock = compact::ProfileLock::try_acquire(deps.parent().unwrap()).unwrap();
    let busy = compact::auto_compact_with(&dirs, &target, &opts);
    assert_eq!(busy.busy, vec![deps.parent().unwrap().to_path_buf()]);
    let pending: serde_json::Value = serde_json::from_slice(&fs::read(&marker).unwrap()).unwrap();
    assert_eq!(pending["complete"], false);
    assert_eq!(pending["at_unix"], old_success);
    drop(lock);
    assert_eq!(compact::auto_compact_with(&dirs, &target, &opts).skipped, Some("not due"));
    common::age(&marker, Duration::from_secs(31));
    let retry = compact::auto_compact_with(&dirs, &target, &opts);
    assert!(retry.busy.is_empty() && retry.skipped.is_none(), "{retry:?}");
    let completed: serde_json::Value = serde_json::from_slice(&fs::read(&marker).unwrap()).unwrap();
    assert_eq!(completed["complete"], true);
    assert!(completed["at_unix"].as_u64().unwrap() > old_success);
    common::age(&marker, Duration::from_secs(31));
    assert_eq!(compact::auto_compact_with(&dirs, &target, &opts).skipped, Some("not due"));
}

fn age_tree(path: &Path, age: Duration) {
    if path.is_dir() {
        for entry in fs::read_dir(path).unwrap() {
            age_tree(&entry.unwrap().path(), age);
        }
    }
    common::age(path, age);
}

#[test]
fn metadata_only_unknown_state_expires_without_a_keep_floor_and_preserves_fresh_and_working_state() {
    let sb = Sandbox::new("maint-unknown-idle");
    let (dirs, target, deps) = fake_target(&sb);
    let profile = deps.parent().unwrap();
    let incremental = profile.join("incremental");
    let finalized = "s-0000000001-0000001-0000000000000000000000000";
    let working_name = "s-0000000001-0000001-working";
    let make = |name: &str, session: &str, age: Duration| {
        let path = incremental.join(name);
        fs::create_dir_all(path.join(session)).unwrap();
        fs::write(path.join(session).join("query-cache.bin"), vec![9; 64 * 1024]).unwrap();
        fs::write(path.join("s-0000000001-0000001.lock"), b"").unwrap();
        age_tree(&path, age);
        path
    };
    let old = make("old-only", finalized, Duration::from_secs(8 * 86400));
    let pressure = make("pressure-only", finalized, Duration::from_secs(2 * 3600));
    let fresh = make("fresh-only", finalized, Duration::ZERO);
    let working = make("working-only", working_name, Duration::from_secs(8 * 86400));
    let mixed = make("mixed-only", finalized, Duration::from_secs(8 * 86400));
    fs::create_dir(mixed.join("s-0000000002-0000002-working")).unwrap();
    fs::write(mixed.join("s-0000000002-0000002.lock"), b"").unwrap();
    age_tree(&mixed, Duration::from_secs(8 * 86400));
    let fingerprint = profile.join(".fingerprint/keep/fingerprint");
    fs::create_dir_all(fingerprint.parent().unwrap()).unwrap();
    fs::write(&fingerprint, b"unchanged fingerprint").unwrap();
    let outputs: Vec<_> = fs::read_dir(&deps).unwrap().map(|e| {
        let path = e.unwrap().path();
        let bytes = fs::read(&path).unwrap();
        (path, bytes)
    }).collect();
    let mut opts = AutoOptions::from_env(&Config::from_env());
    opts.enabled = true;
    opts.compress = false;
    opts.cache_enabled = false;
    assert_eq!(opts.incremental_idle, Duration::from_secs(7 * 86400));
    let report = compact::auto_compact_with(&dirs, &target, &opts);
    assert!(report.errors.is_empty(), "{report:?}");
    assert!(!old.exists(), "even a sole newest unknown session eventually expires");
    assert!(pressure.exists() && fresh.exists() && working.exists() && mixed.exists());
    opts.max_target_bytes = 1;
    common::age(&target.join(compact::AUTO_MARKER), Duration::from_secs(601));
    let report = compact::auto_compact_with(&dirs, &target, &opts);
    assert!(report.errors.is_empty(), "{report:?}");
    assert!(!pressure.exists(), "pressure can reclaim aged metadata reuse state before idle expiry");
    assert!(fresh.exists() && working.exists() && mixed.exists());
    assert!(report.allocated_after > opts.max_target_bytes, "necessary outputs and fresh state may exceed the soft cap");
    assert_eq!(fs::read(&fingerprint).unwrap(), b"unchanged fingerprint");
    for (path, bytes) in outputs {
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
}

#[test]
fn automatic_profile_demotion_preserves_profile_and_cargo_lock_identities() {
    let sb = Sandbox::new("maint-demotion-locks");
    let (dirs, target, _) = fake_target(&sb);
    let debug = target.join("debug");
    let release = target.join("release");
    for profile in [&debug, &release] {
        fs::create_dir_all(profile.join(".fingerprint")).unwrap();
        fs::create_dir_all(profile.join("deps")).unwrap();
        fs::write(profile.join(".fingerprint/stamp"), b"profile activity").unwrap();
        fs::write(profile.join("deps/artifact"), b"profile payload").unwrap();
        for name in [".cargo-lock", ".cargo-build-lock", ".cargo-artifact-lock"] {
            fs::write(profile.join(name), b"").unwrap();
        }
    }
    age_tree(&release.join(".fingerprint"), Duration::from_secs(15 * 86400));
    let identities: Vec<_> = [&release, &release.join("deps"), &release.join(".fingerprint"), &release.join(".cargo-lock"), &release.join(".cargo-build-lock"), &release.join(".cargo-artifact-lock")]
        .into_iter().map(|path| {
            let meta = frust::sys::lstat(path).unwrap();
            (path.to_path_buf(), (meta.dev(), meta.ino()))
        }).collect();
    let mut opts = AutoOptions::from_env(&Config::from_env());
    opts.enabled = true;
    opts.compress = false;
    let report = compact::auto_compact_with(&dirs, &target, &opts);
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!(report.demoted_profiles, vec![release.clone()]);
    assert!(release.join("deps").is_dir() && release.join(".fingerprint").is_dir());
    assert_eq!(fs::read_dir(release.join("deps")).unwrap().count(), 0);
    assert_eq!(fs::read_dir(release.join(".fingerprint")).unwrap().count(), 0);
    assert_eq!(fs::read(debug.join("deps/artifact")).unwrap(), b"profile payload");
    for (path, identity) in identities {
        let meta = frust::sys::lstat(&path).unwrap();
        assert_eq!((meta.dev(), meta.ino()), identity);
    }
}
