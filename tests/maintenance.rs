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
    assert_ne!(third.skipped, Some("nothing changed since the last pass"), "a new artifact is noticed");
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
