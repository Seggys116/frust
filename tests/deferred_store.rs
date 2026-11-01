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
const EXECUTION_SLOTS: usize = 4;
const ADMISSION_SLOTS: usize = 8;

fn deferred_dir(sb: &Sandbox) -> PathBuf {
    sb.home().join("tmp").join("deferred")
}

/// A build with the deferral enabled, which the sandbox turns off for every other test.
fn build_deferred(sb: &Sandbox, project: &Path) -> std::process::Output {
    let out = sb
        .bin("fcargo")
        .arg("build")
        .current_dir(project)
        .env("FRUST_CACHE", "1")
        .env_remove("FRUST_STORE_SYNC")
        .output()
        .unwrap();
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

fn job_markers(sb: &Sandbox) -> Vec<PathBuf> {
    fs::read_dir(deferred_dir(sb))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("job-"))
        })
        .collect()
}

fn broker_directories(sb: &Sandbox) -> Vec<PathBuf> {
    fs::read_dir(sb.home().join("tmp"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("store-broker-"))
        })
        .collect()
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn published_entries(sb: &Sandbox) -> usize {
    fs::read_dir(sb.home().join("store"))
        .into_iter()
        .flatten()
        .flatten()
        .flat_map(|shard| fs::read_dir(shard.path()).into_iter().flatten().flatten())
        .filter(|entry| entry.path().join("meta.json").is_file())
        .count()
}

fn hold_pool(dir: &Path, prefix: &str, count: usize) -> Vec<File> {
    fs::create_dir_all(dir).unwrap();
    (0..count)
        .map(|i| {
            let file = File::create(dir.join(format!("{prefix}{i}"))).unwrap();
            frust::sys::lock(&file, frust::sys::LockKind::Exclusive).unwrap();
            file
        })
        .collect()
}

/// Stores have released their execution slots, markers and lease directories.
fn helpers_idle(sb: &Sandbox) -> bool {
    fs::read_dir(deferred_dir(sb))
        .into_iter()
        .flatten()
        .flatten()
        .all(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with("slot-") {
                return true;
            }
            let file = File::open(entry.path()).unwrap();
            frust::sys::try_lock(&file, frust::sys::LockKind::Exclusive).is_ok()
        })
        && job_markers(sb).is_empty()
        && broker_directories(sb).is_empty()
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

/// Exhausting admission and execution capacity preserves the inline fallback.
#[test]
fn exhausted_admission_and_execution_pools_store_inline() {
    let sb = Sandbox::new("ds-pool");
    sb.vendor_crate("leaf", "1.0.0", LEAF);
    let a = sb.project("a", &[("leaf", "1.0.0")], MAIN);
    let execution = hold_pool(&deferred_dir(&sb), "slot-", EXECUTION_SLOTS);
    let admission = hold_pool(
        &Dirs::at(sb.home()).locks(),
        "store-queue-",
        ADMISSION_SLOTS,
    );

    build_deferred(&sb, &a);

    assert_eq!(stored(&sb), 2, "both units were stored before the build returned");
    let jobs: Vec<_> = fs::read_dir(deferred_dir(&sb))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("job-"))
        .collect();
    assert!(jobs.is_empty(), "no job was handed to a helper: {jobs:?}");
    drop((execution, admission));
    clean(&sb, &a);
    build_deferred(&sb, &a);
    wait_for_stores(&sb, 2);
    assert_eq!(sb.count_cache("hit", "leaf"), 1);
}

#[test]
fn unobserved_lock_off_builds_keep_inline_fallback_and_restore() {
    let sb = Sandbox::new("ds-lock-off");
    sb.vendor_crate("leaf", "1.0.0", LEAF);
    let project = sb.project("app", &[("leaf", "1.0.0")], MAIN);
    let execution = hold_pool(&deferred_dir(&sb), "slot-", EXECUTION_SLOTS);
    let build = || {
        let output = sb.bin("fcargo").arg("build").current_dir(&project)
            .env("FRUST_CACHE", "1").env("FRUST_LOCK", "0").env("FRUST_RENDER", "off")
            .env_remove("FRUST_STORE_SYNC").output().unwrap();
        assert_success(&output);
    };
    build();
    assert_eq!(stored(&sb), 2);
    assert!(job_markers(&sb).is_empty() && broker_directories(&sb).is_empty());
    assert!(!sb.home().join("locks/store-queue-0").exists());
    let compiled = leaf_rlib(&sb, &project);
    assert_eq!(app_output(&sb, &project), "41");
    drop(execution);
    clean(&sb, &project);
    build();
    wait_for_stores(&sb, 2);
    assert_eq!(sb.count_cache("hit", "leaf"), 1);
    assert_eq!(sb.count_cache("hit", "app"), 1);
    assert_eq!(leaf_rlib(&sb, &project), compiled);
    assert_eq!(app_output(&sb, &project), "41");
    assert!(broker_directories(&sb).is_empty());
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn queued_stores_survive_frontend_exit_drain_and_restore_exact_outputs() {
    let sb = Sandbox::new(&format!("ds-queued-{}", "nested".repeat(24)));
    assert!(sb.root.file_name().unwrap().len() < 255);
    assert!(sb.home().as_os_str().len() > 108);
    sb.vendor_crate("leaf", "1.0.0", LEAF);
    let project = sb.project("app", &[("leaf", "1.0.0")], MAIN);
    fs::write(project.join("build.rs"), r#"fn main() {
        println!("cargo:rerun-if-changed=build.rs");
        let generated = std::path::Path::new(&std::env::var_os("OUT_DIR").unwrap()).join("generated.rs");
        std::fs::write(&generated, format!("pub const GENERATED: &str = {:?};\n", "generated".repeat(8192))).unwrap();
        let alias = generated.with_extension("alias");
        if !alias.exists() { std::fs::hard_link(&generated, &alias).unwrap(); }
        let gate = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("broker-ready");
        let start = std::time::Instant::now();
        while !gate.exists() {
            assert!(start.elapsed() < std::time::Duration::from_secs(30), "broker gate timed out");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }"#).unwrap();
    fs::write(project.join("src/main.rs"), "include!(concat!(env!(\"OUT_DIR\"), \"/generated.rs\"));\nfn main() { assert_eq!(GENERATED, \"generated\".repeat(8192)); println!(\"{}\", leaf::value()); }\n").unwrap();
    common::settle(&project.join("build.rs"));
    let execution = hold_pool(&deferred_dir(&sb), "slot-", EXECUTION_SLOTS);

    let build_output = std::thread::scope(|scope| {
        let build = scope.spawn(|| build_deferred(&sb, &project));
        let start = Instant::now();
        loop {
            let ready = broker_directories(&sb).iter().any(|run| {
                let Some(cap) = fs::read(run.join("capability.json")).ok().and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok()) else { return false };
                let Some(token) = cap["token"].as_str() else { return false };
                Path::new("/tmp").join(format!("frust-{}/b-{token}/s", frust::sys::user_id())).exists()
            });
            if ready { break; }
            assert!(start.elapsed() < WAIT_CAP, "first real miss did not activate the broker");
            std::thread::sleep(Duration::from_millis(10));
        }
        let generated = loop {
            let paths = fs::read_dir(sb.central_target(&project).join("debug/build")).unwrap();
            if let Some(path) = paths.flatten().map(|entry| entry.path().join("out/generated.rs")).find(|path| path.is_file()) { break path; }
            assert!(start.elapsed() < WAIT_CAP, "build script did not produce its native compiler input");
            std::thread::sleep(Duration::from_millis(10));
        };
        common::settle(&generated);
        fs::write(project.join("broker-ready"), b"ready").unwrap();
        build.join().unwrap()
    });

    let early = stored(&sb);
    assert!((1..3).contains(&early), "early units store inline, but the gated app stays queued: {early}");
    assert_eq!(published_entries(&sb), early);
    let queued = job_markers(&sb);
    assert_eq!(queued.len(), 3 - early, "all later units remain queued: {queued:?}");
    assert!(queued.iter().all(|path| {
        path.file_name()
            .unwrap()
            .to_string_lossy()
            .contains("-broker-")
    }));
    assert!(queued.iter().any(|path| {
        let job: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        job["args"].as_array().unwrap().windows(2).any(|pair| pair[0] == "--crate-name" && pair[1] == "app")
    }), "the app compiled after activation must remain broker-owned");
    let broker_runs = broker_directories(&sb);
    assert_eq!(
        broker_runs.len(),
        1,
        "the draining broker outlives the frontend"
    );
    let run_dir = &broker_runs[0];
    let capability: serde_json::Value =
        serde_json::from_slice(&fs::read(run_dir.join("capability.json")).unwrap()).unwrap();
    let token = capability["token"].as_str().unwrap();
    assert_eq!(token.len(), 64);
    let endpoint_dir = frust::paths::private_temp_dir(Path::new("/tmp"))
        .unwrap()
        .join(format!("b-{token}"));
    assert!(endpoint_dir.is_dir());
    frust::sys::verify_private_dir(&endpoint_dir).unwrap();
    assert!(endpoint_dir.join("s").exists());
    assert_eq!(sb.count_cache("miss", "leaf"), 1);
    assert_eq!(sb.count_cache("miss", "app"), 1);
    assert!(
        sb.cache_events()
            .iter()
            .all(|event| event["bytes"].as_u64().is_some_and(|bytes| bytes > 0))
    );
    let compiled = leaf_rlib(&sb, &project);
    assert_eq!(app_output(&sb, &project), "41");

    let target = sb.central_target(&project);
    let app_binary = target.join("debug/app");
    let compiled_app = fs::read(&app_binary).unwrap();
    let app_job: serde_json::Value = queued.iter().find_map(|path| {
        let job: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        job["args"].as_array().unwrap().windows(2).any(|pair| pair[0] == "--crate-name" && pair[1] == "app").then_some(job)
    }).unwrap();
    let generated = app_job["inputs"].as_array().unwrap().iter().filter_map(|row| row[0].as_str())
        .map(PathBuf::from).find(|path| path.ends_with("generated.rs")).expect("the deferred proof owns the actual generated input");
    let generated_stamp = frust::cache::hashing::Stamp::read(&generated).unwrap();
    let original_input = app_job["inputs"].as_array().unwrap().iter().find(|row| row[0].as_str() == generated.to_str()).unwrap();
    fs::write(sb.root.join("pending-before.json"), serde_json::to_vec_pretty(&serde_json::json!({
        "generated": generated, "frozen": original_input[1], "observed": format!("{generated_stamp:?}"),
        "stderr": stderr(&build_output).chars().take(32 * 1024).collect::<String>()
    })).unwrap()).unwrap();
    assert_eq!(original_input[1]["ino"].as_u64(), Some(generated_stamp.ino));
    assert_eq!(original_input[1]["ctime_ns"].as_str(), Some(generated_stamp.ctime_ns.to_string().as_str()), "default maintenance must preserve the frozen input");
    let generated_bytes = fs::read(&generated).unwrap();
    let alias = generated.with_extension("alias");
    assert_eq!(frust::cache::hashing::Stamp::read(&alias).unwrap(), generated_stamp);
    let unrelated = target.join("unused-profile");
    fs::create_dir_all(unrelated.join("deps")).unwrap();
    let garbage = unrelated.join("deps/unrelated.bin");
    fs::write(&garbage, b"unrelated retired output").unwrap();
    let newest = target.join("newest-profile");
    fs::create_dir_all(newest.join("deps")).unwrap();
    fs::create_dir_all(newest.join(".fingerprint")).unwrap();
    fs::write(newest.join(".fingerprint/current-stamp"), b"newer profile").unwrap();
    let dirs = Dirs::at(sb.home());
    let mut options = frust::compact::AutoOptions::from_env(&frust::config::Config::from_env());
    options.every = Duration::ZERO;
    options.min_growth = 0;
    options.current_window = Duration::ZERO;
    options.keep_other_profile = Duration::ZERO;
    let maintenance = frust::compact::auto_compact_with(&dirs, &target, &options);
    fs::write(sb.root.join("pending-after-auto.json"), serde_json::to_vec_pretty(&serde_json::json!({
        "generated": generated, "before": format!("{generated_stamp:?}"),
        "after": format!("{:?}", frust::cache::hashing::Stamp::read(&generated)),
        "retained": maintenance.retained_deferred_paths, "unrelated_removed": !garbage.exists()
    })).unwrap()).unwrap();
    assert!(maintenance.errors.is_empty(), "{maintenance:?}");
    assert!(maintenance.retained_deferred_paths > 0, "{maintenance:?}");
    assert!(!garbage.exists(), "unrelated profile cleanup must proceed: {maintenance:?}");
    assert_eq!(frust::cache::hashing::Stamp::read(&generated).unwrap(), generated_stamp);
    assert_eq!(fs::read(&generated).unwrap(), generated_bytes);
    let marker: serde_json::Value = serde_json::from_slice(&fs::read(target.join(frust::compact::AUTO_MARKER)).unwrap()).unwrap();
    assert_eq!(marker["complete"], false, "protected work must receive the normal incomplete-pass retry");
    let manual = frust::compact::compact(&dirs, std::slice::from_ref(&target), &frust::compact::CompactOptions::after_build());
    assert!(manual.errors.is_empty(), "{manual:?}");
    assert_eq!(frust::cache::hashing::Stamp::read(&generated).unwrap(), generated_stamp);
    assert_eq!(frust::cache::hashing::Stamp::read(&alias).unwrap(), generated_stamp);
    fs::write(sb.root.join("pending-maintenance.json"), serde_json::to_vec_pretty(&serde_json::json!({
        "generated": generated, "before": format!("{generated_stamp:?}"),
        "after": format!("{:?}", frust::cache::hashing::Stamp::read(&generated).unwrap()),
        "blake3": blake3::hash(&generated_bytes).to_hex().as_str(),
        "stderr": stderr(&build_output).chars().take(32 * 1024).collect::<String>(),
        "retained": maintenance.retained_deferred_paths, "unrelated_removed": !garbage.exists()
    })).unwrap()).unwrap();
    assert_eq!(fs::read(&app_binary).unwrap(), compiled_app);

    drop(execution);
    wait_for_stores(&sb, 3);
    assert_eq!(published_entries(&sb), 3);
    assert!(job_markers(&sb).is_empty() && broker_directories(&sb).is_empty());
    assert!(!endpoint_dir.exists() && !run_dir.exists());
    fs::remove_file(&alias).unwrap();
    assert_eq!(frust::sys::lstat(&generated).unwrap().nlink(), 1);
    options.current_window = Duration::from_secs(3600);
    let resumed = frust::compact::auto_compact_with(&dirs, &target, &options);
    assert!(resumed.errors.is_empty(), "{resumed:?}");
    assert_eq!(resumed.retained_deferred_paths, 0, "{resumed:?}");
    if cfg!(target_os = "macos") {
        assert!(frust::compact::is_compressed(&fs::metadata(&generated).unwrap()), "completed jobs must not permanently exempt generated inputs: {resumed:?}");
    }
    assert_eq!(fs::read(&generated).unwrap(), generated_bytes);

    let events = sb.cache_events();
    let noop = sb
        .bin("fcargo")
        .args(["build", "--message-format=json"])
        .current_dir(&project)
        .env("FRUST_CACHE", "1")
        .env_remove("FRUST_STORE_SYNC")
        .output()
        .unwrap();
    assert_success(&noop);
    let artifacts: Vec<serde_json::Value> = stdout(&noop)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|value| value["reason"] == "compiler-artifact")
        .collect();
    assert_eq!(artifacts.len(), 3, "{artifacts:?}");
    assert!(artifacts.iter().all(|artifact| artifact["fresh"] == true));
    assert_eq!(
        sb.cache_events(),
        events,
        "the no-op performs no cache work"
    );
    assert!(job_markers(&sb).is_empty() && broker_directories(&sb).is_empty());
    assert_eq!(leaf_rlib(&sb, &project), compiled);

    clean(&sb, &project);
    build_deferred(&sb, &project);
    wait_for_stores(&sb, 3);
    assert_eq!(sb.count_cache("hit", "leaf"), 1);
    assert_eq!(sb.count_cache("hit", "app"), 1);
    assert_eq!(sb.count_cache("miss", "leaf"), 1);
    assert_eq!(sb.count_cache("miss", "app"), 1);
    assert_eq!(
        leaf_rlib(&sb, &project),
        compiled,
        "the full restored leaf matches rustc's output"
    );
    assert_eq!(app_output(&sb, &project), "41");
    assert_eq!(fs::read(&app_binary).unwrap(), compiled_app, "the restored executable keeps every original byte");
    assert!(job_markers(&sb).is_empty() && broker_directories(&sb).is_empty());
}

/// Canonical slot inodes must survive GC so late acquisition cannot split the global execution limit.
#[test]
fn canonical_execution_slots_survive_gc_planned_before_acquisition() {
    let sb = Sandbox::new("ds-stable-slots");
    let dir = deferred_dir(&sb);
    fs::create_dir_all(&dir).unwrap();
    let slots: Vec<_> = (0..EXECUTION_SLOTS)
        .map(|index| {
            let path = dir.join(format!("slot-{index}"));
            fs::write(&path, b"").unwrap();
            age(&path, Duration::from_secs(2 * 86_400));
            let meta = frust::sys::lstat(&path).unwrap();
            (path, (meta.dev(), meta.ino()))
        })
        .collect();
    let dirs = Dirs::at(sb.home());
    let opts = GcOptions::new(u64::MAX);
    let plans = [gc::plan(&dirs, &opts), gc::plan_maintenance(&dirs, &opts)];
    let held: Vec<_> = slots
        .iter()
        .map(|(path, _)| {
            let file = File::open(path).unwrap();
            frust::sys::lock(&file, frust::sys::LockKind::Exclusive).unwrap();
            file
        })
        .collect();
    for plan in &plans {
        let (_, failures) = gc::execute(&dirs, plan);
        assert!(failures.is_empty(), "{failures:?}");
        for (path, identity) in &slots {
            let meta = frust::sys::lstat(path).unwrap();
            assert_eq!((meta.dev(), meta.ino()), *identity);
            let other = File::open(path).unwrap();
            assert!(matches!(
                frust::sys::try_lock(&other, frust::sys::LockKind::Exclusive),
                Err(fs::TryLockError::WouldBlock)
            ));
        }
    }
    drop(held);
    for (path, identity) in &slots {
        let next = File::open(path).unwrap();
        frust::sys::try_lock(&next, frust::sys::LockKind::Exclusive).unwrap();
        let meta = frust::sys::fstat(&next).unwrap();
        assert_eq!((meta.dev(), meta.ino()), *identity);
    }
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
