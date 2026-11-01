//! A store finished by a detached helper after the build returned yields the same cache as an inline one, and every failure falls back to storing inline.

mod common;

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use common::{Sandbox, age, assert_success, run_binary, stderr, stdout};
use frust::gc::{self, Class, GcOptions};
use frust::paths::Dirs;

const MAIN: &str = "fn main() { println!(\"{}\", leaf::value()); }\n";
const LEAF: &str = "pub fn value() -> u32 { 41 }\n";
const WAIT_CAP: Duration = Duration::from_secs(30);

fn deferred_dir(sb: &Sandbox) -> PathBuf {
    sb.home().join("tmp").join("deferred")
}

/// A build with the deferral enabled, which the sandbox turns off for every other test.
fn build_deferred(sb: &Sandbox, project: &Path) -> std::process::Output {
    let out = sb.bin("fcargo").arg("build").current_dir(project).env_remove("FRUST_STORE_SYNC").output().unwrap();
    assert_success(&out);
    out
}

fn build_inline(sb: &Sandbox, project: &Path) {
    assert_success(&sb.bin("fcargo").arg("build").current_dir(project).output().unwrap());
}

fn clean(sb: &Sandbox, project: &Path) {
    assert_success(&sb.bin("fcargo").arg("clean").current_dir(project).output().unwrap());
}

fn manifests(sb: &Sandbox) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for shard in fs::read_dir(sb.home().join("manifests")).into_iter().flatten().flatten() {
        for file in fs::read_dir(shard.path()).into_iter().flatten().flatten() {
            if file.path().extension().is_some_and(|e| e == "json") {
                found.push(file.path());
            }
        }
    }
    found
}

/// Manifests that list at least one candidate, written only after the entry they name is published.
fn stored(sb: &Sandbox) -> usize {
    manifests(sb)
        .iter()
        .filter_map(|p| serde_json::from_slice::<serde_json::Value>(&fs::read(p).ok()?).ok())
        .filter(|m| m["candidates"].as_array().is_some_and(|c| !c.is_empty()))
        .count()
}

/// True when no slot lock is held, that is, when no helper is still running.
fn helpers_idle(sb: &Sandbox) -> bool {
    fs::read_dir(deferred_dir(sb)).into_iter().flatten().flatten().all(|entry| {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("slot-") {
            return true;
        }
        let file = File::open(entry.path()).unwrap();
        frust::sys::try_lock(&file, frust::sys::LockKind::Exclusive).is_ok()
    })
}

/// Waits for `expected` stored units and for every helper to finish, so nothing writes into the sandbox after the test.
fn wait_for_stores(sb: &Sandbox, expected: usize) {
    let begun = Instant::now();
    while !(stored(sb) >= expected && helpers_idle(sb)) {
        assert!(begun.elapsed() < WAIT_CAP, "stored {} of {expected} units after {WAIT_CAP:?}", stored(sb));
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn leaf_rlib(sb: &Sandbox, project: &Path) -> Vec<u8> {
    let deps = sb.central_target(project).join("debug").join("deps");
    let path = fs::read_dir(&deps)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            name.starts_with("libleaf-") && name.ends_with(".rlib")
        })
        .expect("leaf's library");
    fs::read(path).unwrap()
}

fn app_output(sb: &Sandbox, project: &Path) -> String {
    let out = run_binary(&sb.central_target(project), "debug", "app");
    assert_success(&out);
    stdout(&out).trim().to_string()
}

fn wipe_cache(sb: &Sandbox) {
    for dir in ["store", "manifests"] {
        let _ = fs::remove_dir_all(sb.home().join(dir));
    }
}

/// The store finishes after the build returned, the next build hits it, and what it restores equals what rustc itself wrote.
#[test]
fn a_deferred_store_completes_after_the_build_and_serves_the_next_one() {
    let sb = Sandbox::new("ds-basic");
    sb.vendor_crate("leaf", "1.0.0", LEAF);
    let a = sb.project("a", &[("leaf", "1.0.0")], MAIN);

    build_deferred(&sb, &a);
    wait_for_stores(&sb, 2);
    assert!(deferred_dir(&sb).join("slot-0").exists(), "the store went through the deferral path");
    assert_eq!(sb.count_cache("miss", "leaf"), 1, "{:#?}", sb.cache_events());
    let compiled = leaf_rlib(&sb, &a);
    let output = app_output(&sb, &a);
    assert_eq!(output, "41");
    let leftovers: Vec<_> = fs::read_dir(deferred_dir(&sb))
        .unwrap()
        .flatten()
        .filter(|e| !e.file_name().to_string_lossy().starts_with("slot-"))
        .collect();
    assert!(leftovers.is_empty(), "finished helpers remove their jobs: {leftovers:?}");

    clean(&sb, &a);
    build_deferred(&sb, &a);
    wait_for_stores(&sb, 2);
    assert_eq!(sb.count_cache("hit", "leaf"), 1, "{:#?}", sb.cache_events());
    assert_eq!(sb.count_cache("miss", "leaf"), 1, "the second build compiled nothing");
    assert_eq!(leaf_rlib(&sb, &a), compiled, "a restored artifact is byte-identical");
    assert_eq!(app_output(&sb, &a), output);

    wipe_cache(&sb);
    clean(&sb, &a);
    build_inline(&sb, &a);
    assert_eq!(leaf_rlib(&sb, &a), compiled, "an inline store sees the same compile output");
    assert_eq!(app_output(&sb, &a), output);
}

#[test]
fn default_deferred_build_script_stores_and_restores_its_executable() {
    let sb = Sandbox::new("ds-build-script");
    sb.vendor_crate("leaf", "1.0.0", "pub fn value() -> u32 { include!(concat!(env!(\"OUT_DIR\"), \"/value.rs\")) }\n");
    let script = sb.vendor().join("leaf-1.0.0/build.rs");
    fs::write(
        &script,
        r#"fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    std::fs::write(out.join("value.rs"), "41u32").unwrap();
}
"#,
    )
    .unwrap();
    common::settle(&script);
    let project = sb.project("app", &[("leaf", "1.0.0")], MAIN);
    build_deferred(&sb, &project);
    wait_for_stores(&sb, 3);
    assert_eq!(sb.count_cache("miss", "build_script_build"), 1, "{:#?}", sb.cache_events());
    assert_eq!(app_output(&sb, &project), "41");
    clean(&sb, &project);
    build_deferred(&sb, &project);
    wait_for_stores(&sb, 3);
    assert_eq!(sb.count_cache("hit", "build_script_build"), 1, "{:#?}", sb.cache_events());
    assert_eq!(app_output(&sb, &project), "41");
}

/// With every slot taken the wrapper stores before it returns, so the entry exists the moment the build ends.
#[test]
fn an_exhausted_slot_pool_stores_inline() {
    let sb = Sandbox::new("ds-pool");
    sb.vendor_crate("leaf", "1.0.0", LEAF);
    let a = sb.project("a", &[("leaf", "1.0.0")], MAIN);
    fs::create_dir_all(deferred_dir(&sb)).unwrap();
    let taken: Vec<File> = (0..16)
        .map(|i| {
            let file = File::create(deferred_dir(&sb).join(format!("slot-{i}"))).unwrap();
            frust::sys::lock(&file, frust::sys::LockKind::Exclusive).unwrap();
            file
        })
        .collect();

    build_deferred(&sb, &a);

    assert_eq!(stored(&sb), 2, "both units were stored before the build returned");
    let jobs: Vec<_> = fs::read_dir(deferred_dir(&sb))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("job-"))
        .collect();
    assert!(jobs.is_empty(), "no job was handed to a helper: {jobs:?}");
    drop(taken);
    clean(&sb, &a);
    build_deferred(&sb, &a);
    wait_for_stores(&sb, 2);
    assert_eq!(sb.count_cache("hit", "leaf"), 1);
}

/// A helper handed a job it cannot use stores nothing, prints nothing and removes the job; gc collects what a killed helper left behind.
#[test]
fn unusable_jobs_store_nothing_and_gc_collects_stray_files() {
    let sb = Sandbox::new("ds-stray");
    let dir = deferred_dir(&sb);
    fs::create_dir_all(&dir).unwrap();

    let job = dir.join("job-1-1.json");
    fs::write(&job, b"{ \"format\": 1").unwrap();
    let out = sb.bin_path(env!("CARGO_BIN_EXE_frust-wrap")).arg("--deferred-store").arg(&job).output().unwrap();
    assert_success(&out);
    assert!(stdout(&out).is_empty() && stderr(&out).is_empty(), "the helper never prints");
    assert!(!job.exists(), "the job is removed even when it cannot be read");
    assert_eq!(stored(&sb), 0);

    let stale_job = dir.join("job-2-2.json");
    let recent_job = dir.join("job-3-3.json");
    let stale_slot = dir.join("slot-8");
    let busy_slot = dir.join("slot-9");
    for path in [&stale_job, &recent_job, &stale_slot, &busy_slot] {
        fs::write(path, b"x").unwrap();
    }
    for path in [&stale_job, &stale_slot, &busy_slot] {
        age(path, Duration::from_secs(2 * 86_400));
    }
    let busy = File::open(&busy_slot).unwrap();
    frust::sys::lock(&busy, frust::sys::LockKind::Exclusive).unwrap();

    let dirs = Dirs::at(sb.home());
    let opts = GcOptions::new(u64::MAX);
    let plan = gc::plan_maintenance(&dirs, &opts);
    let removes = |path: &Path| plan.removals.iter().any(|r| r.class == Class::Metadata && r.path == path);
    assert!(removes(&stale_job) && removes(&stale_slot));
    assert!(!removes(&recent_job) && !removes(&busy_slot));
    let (_, failures) = gc::execute(&dirs, &gc::plan(&dirs, &opts));
    assert!(failures.is_empty(), "{failures:?}");
    assert!(!stale_job.exists() && !stale_slot.exists());
    assert!(recent_job.exists() && busy_slot.exists());
    drop(busy);
}

/// The helper entry is a hidden argument of the wrapper binary only; any other command line goes to the compiler as before.
#[test]
fn the_hidden_argument_is_not_mistaken_for_a_compiler_command() {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_frust-wrap"));
    #[cfg(unix)]
    cmd.arg("true").arg("--deferred-store");
    #[cfg(windows)]
    cmd.args(["cmd", "/c", "exit", "0", "--deferred-store"]);
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "a compiler command line that merely contains the flag is passed through: {}", stderr(&out));
}
