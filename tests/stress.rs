//! Many people building one workspace at once, with maintenance running underneath.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use common::{Sandbox, assert_success, run_binary, stderr, stdout};

const GATE_SLEEP_MS: u64 = 1800;

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn member(root: &Path, name: &str, deps: &[&str], lib: bool, source: &str, build_rs: Option<&str>) {
    let mut manifest = format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n");
    for dep in deps {
        manifest.push_str(&format!("{dep} = {{ path = \"../{dep}\" }}\n"));
    }
    let dir = root.join(name);
    write(&dir.join("Cargo.toml"), &manifest);
    write(&dir.join("src").join(if lib { "lib.rs" } else { "main.rs" }), source);
    if let Some(script) = build_rs {
        write(&dir.join("build.rs"), script);
    }
}

/// `gate` has a slow build script so concurrent builds overlap.
fn workspace(sb: &Sandbox) -> PathBuf {
    let root = sb.root.join("ws");
    write(&root.join("Cargo.toml"), "[workspace]\nresolver = \"2\"\nmembers = [\"core\", \"util\", \"gate\", \"app\", \"tool\"]\n");
    member(&root, "core", &[], true, "pub fn base() -> u32 { 40 }\n", None);
    member(&root, "util", &["core"], true, "pub fn twice() -> u32 { core::base() * 2 }\n", None);
    member(
        &root,
        "gate",
        &[],
        true,
        "pub fn open() -> &'static str { \"open\" }\n",
        Some(&format!("fn main() {{ std::thread::sleep(std::time::Duration::from_millis({GATE_SLEEP_MS})); }}\n")),
    );
    member(&root, "app", &["util", "gate"], false, "fn main() { println!(\"app {} {}\", util::twice(), gate::open()); }\n", None);
    member(&root, "tool", &["core"], false, "fn main() { println!(\"tool {}\", core::base()); }\n", None);
    root
}

fn concurrently(sb: &Sandbox, dir: &Path, jobs: &[Vec<&str>]) -> Vec<Output> {
    concurrently_while(sb, dir, jobs, || {})
}

fn concurrently_while(sb: &Sandbox, dir: &Path, jobs: &[Vec<&str>], running: impl FnOnce()) -> Vec<Output> {
    let barrier = Arc::new(Barrier::new(jobs.len()));
    thread::scope(|scope| {
        let handles: Vec<_> = jobs
            .iter()
            .map(|args| {
                let mut cmd = sb.bin("fcargo");
                cmd.args(args).current_dir(dir).env("FRUST_RENDER", "plain");
                let barrier = barrier.clone();
                scope.spawn(move || {
                    barrier.wait();
                    cmd.output().unwrap()
                })
            })
            .collect();
        running();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    })
}

fn binary_output(sb: &Sandbox, ws: &Path, profile: &str, name: &str) -> String {
    let out = run_binary(&sb.central_target(ws), profile, name);
    assert_success(&out);
    stdout(&out).trim().to_string()
}

#[test]
fn eight_people_building_the_same_binary_share_one_build() {
    let sb = Sandbox::new("eight");
    let ws = workspace(&sb);
    write(
        &ws.join("gate/build.rs"),
        r#"use std::io::Write;
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let dir = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let mut runs = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("runs")).unwrap();
    writeln!(runs, "run").unwrap();
    let started = std::time::Instant::now();
    while !dir.join("open").exists() {
        assert!(started.elapsed() < std::time::Duration::from_secs(120), "gate was never opened");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}
"#,
    );
    let jobs: Vec<Vec<&str>> = (0..8).map(|_| vec!["build", "-p", "app"]).collect();
    let done = concurrently_while(&sb, &ws, &jobs, || {
        let dirs = frust::paths::Dirs::at(sb.home());
        let started = Instant::now();
        let locks = loop {
            let locks = frust::lock::list(&dirs);
            if (locks.len() == 1 && locks[0].joiners.len() == 7 && ws.join("gate/runs").exists())
                || started.elapsed() >= Duration::from_secs(60)
            {
                break locks;
            }
            thread::sleep(Duration::from_millis(20));
        };
        // Open even on timeout, so the scoped build threads can finish before the assertion unwinds.
        write(&ws.join("gate/open"), "open");
        assert!(locks.len() == 1 && locks[0].joiners.len() == 7, "expected one holder and seven followers: {locks:#?}");
    });

    for d in &done {
        assert_success(d);
    }
    // Followers recheck freshness in turn; process startup time is not a measure of shared compilation.
    assert_eq!(fs::read_to_string(ws.join("gate/runs")).unwrap(), "run\n", "the shared build script must run exactly once");
    for krate in ["core", "util", "gate"] {
        assert!(sb.count_cache("miss", krate) <= 1, "{krate} compiled more than once");
        assert_eq!(sb.count_cache("hit", krate), 0, "{krate} was needlessly restored");
    }
    assert_eq!(binary_output(&sb, &ws, "debug", "app"), "app 80 open");
    let again = sb.bin("fcargo").args(["build", "-p", "app"]).current_dir(&ws).output().unwrap();
    assert_success(&again);
    assert!(stderr(&again).contains("Finished"), "{}", stderr(&again));
    assert_eq!(fs::read_to_string(ws.join("gate/runs")).unwrap(), "run\n", "a fresh build must not rerun the build script");
}

#[test]
fn different_binaries_and_profiles_in_one_workspace_all_succeed() {
    let sb = Sandbox::new("mixed");
    let ws = workspace(&sb);
    let jobs: Vec<Vec<&str>> = vec![
        vec!["build", "-p", "app"],
        vec!["build", "-p", "app"],
        vec!["build", "-p", "tool"],
        vec!["build", "-p", "tool"],
        vec!["build", "--release", "-p", "app"],
        vec!["build", "--release", "-p", "app"],
        vec!["check", "--workspace"],
        vec!["build", "--workspace"],
    ];
    let done = concurrently(&sb, &ws, &jobs);
    for d in &done {
        assert_success(d);
    }
    assert_eq!(binary_output(&sb, &ws, "debug", "app"), "app 80 open");
    assert_eq!(binary_output(&sb, &ws, "debug", "tool"), "tool 40");
    assert_eq!(binary_output(&sb, &ws, "release", "app"), "app 80 open");
}

#[test]
fn debug_and_release_builds_run_in_parallel() {
    let sb = Sandbox::new("parallel");
    let ws = workspace(&sb);
    let started = Instant::now();
    let done = concurrently(&sb, &ws, &[vec!["build", "-p", "app"], vec!["build", "--release", "-p", "app"]]);
    let wall = started.elapsed();
    for d in &done {
        assert_success(d);
    }
    // Two sequential builds would each pay the slow build script.
    assert!(
        wall < Duration::from_millis(GATE_SLEEP_MS * 2 + 4500),
        "debug and release took {wall:?}; they must not wait for each other"
    );
}

#[test]
fn compact_and_gc_running_during_builds_break_nothing() {
    let sb = Sandbox::new("maintenance");
    let ws = workspace(&sb);

    assert_success(&sb.bin("fcargo").args(["build", "--workspace"]).current_dir(&ws).output().unwrap());

    let stop = Arc::new(AtomicBool::new(false));
    let maintenance = {
        let stop = stop.clone();
        let mut compact = sb.bin("frust");
        compact.args(["compact", "--yes"]);
        let mut gc = sb.bin("frust");
        gc.args(["gc", "--yes", "--max-idle-days", "365"]);
        thread::spawn(move || {
            let mut runs = 0;
            while !stop.load(Ordering::Relaxed) {
                for cmd in [&mut compact, &mut gc] {
                    let out = cmd.output().unwrap();
                    assert!(out.status.code().is_some(), "maintenance was killed by a signal");
                    runs += 1;
                }
                thread::sleep(Duration::from_millis(60));
            }
            runs
        })
    };

    write(&ws.join("util/src/lib.rs"), "pub fn twice() -> u32 { core::base() * 2 + 0 }\n");
    let jobs: Vec<Vec<&str>> = vec![
        vec!["build", "-p", "app"],
        vec!["build", "-p", "app"],
        vec!["build", "-p", "app"],
        vec!["build", "-p", "tool"],
        vec!["build", "--release", "-p", "app"],
        vec!["build", "--release", "-p", "app"],
        vec!["test", "--workspace", "--no-run"],
        vec!["check", "--workspace"],
    ];
    let done = concurrently(&sb, &ws, &jobs);
    stop.store(true, Ordering::Relaxed);
    let runs = maintenance.join().unwrap();
    assert!(runs >= 2, "maintenance never ran ({runs})");

    for d in &done {
        assert_success(d);
    }
    assert_eq!(binary_output(&sb, &ws, "debug", "app"), "app 80 open");
    assert_eq!(binary_output(&sb, &ws, "debug", "tool"), "tool 40");
    assert_eq!(binary_output(&sb, &ws, "release", "app"), "app 80 open");

    let before = sb.cache_events().len();
    let after = sb.bin("fcargo").args(["build", "--workspace"]).current_dir(&ws).output().unwrap();
    assert_success(&after);
    assert!(stderr(&after).contains("Finished"), "{}", stderr(&after));
    assert_eq!(sb.cache_events().len(), before, "a build after maintenance recompiled something");
}
