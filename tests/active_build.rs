//! `frust gc` and `frust compact` run against the whole home while a real build holds one workspace's custom profile.

mod common;

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::time::{Duration, Instant};

use common::{Sandbox, assert_success, settle, stderr, stdout};

const BUILD_RS: &str = "fn main() {\n    println!(\"cargo:rerun-if-env-changed=HOLD_SECS\");\n    if let Ok(path) = std::env::var(\"HOLD_STARTED\") { std::fs::write(path, b\"1\").unwrap(); }\n    let secs: u64 = std::env::var(\"HOLD_SECS\").ok().and_then(|s| s.parse().ok()).unwrap_or(0);\n    std::thread::sleep(std::time::Duration::from_secs(secs));\n}\n";

fn workspace(sb: &Sandbox, name: &str) -> PathBuf {
    let dir = sb.root.join(name);
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(
        dir.join("Cargo.toml"),
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\nbuild = \"build.rs\"\n\n[workspace]\n\n[profile.harness]\ninherits = \"dev\"\nincremental = true\n"),
    )
    .unwrap();
    fs::write(dir.join("build.rs"), BUILD_RS).unwrap();
    edit(&dir, 1);
    dir
}

fn edit(dir: &Path, value: u32) {
    let main = dir.join("src").join("main.rs");
    fs::write(&main, format!("fn main() {{ println!(\"{{}}\", {value}); }}\n")).unwrap();
    settle(&main);
}

fn fcargo(sb: &Sandbox, dir: &Path, args: &[&str]) -> Output {
    let out = sb.bin("fcargo").args(args).current_dir(dir).output().unwrap();
    assert_success(&out);
    out
}

/// Builds twice with an edit in between, so the profile holds a superseded incremental session for gc and compact to want.
fn warm(sb: &Sandbox, dir: &Path, args: &[&str]) {
    fcargo(sb, dir, args);
    edit(dir, 2);
    fcargo(sb, dir, args);
}

fn tree(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).unwrap().flatten() {
            if entry.file_type().unwrap().is_dir() {
                pending.push(entry.path());
            }
            out.push(entry.path());
        }
    }
    out.sort();
    out
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < Duration::from_secs(240), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Cargo 1.99 holds `.cargo-build-lock` exclusively while building (and `.cargo-lock` shared), so no other lock is possible.
fn cargo_lock_held(profile: &Path) -> bool {
    let Ok(file) = File::options().read(true).write(true).open(profile.join(".cargo-build-lock")) else { return false };
    frust::sys::try_lock(&file, frust::sys::LockKind::Shared).is_err()
}

struct Collected {
    gc: Output,
    concurrent: Output,
    sequential: Output,
    vanished: Vec<PathBuf>,
}

/// Starts the build, waits until its build script runs under cargo's lock, runs gc and compact concurrently, then compact alone.
fn collect_during(sb: &Sandbox, mut build: Command, profile: &Path, started: &Path) -> Collected {
    let mut child: Child = build.spawn().unwrap();
    wait_for("the build script", || started.exists() || child.try_wait().unwrap().is_some());
    assert!(child.try_wait().unwrap().is_none(), "the build must still be running");
    assert!(cargo_lock_held(profile), "cargo holds the profile lock while the build script runs");
    let before = tree(profile);
    let gc = sb.bin("frust").args(["gc", "--yes", "--max-gib", "0"]).spawn_output();
    let concurrent = sb.bin("frust").args(["compact", "--yes"]).output().unwrap();
    let gc = gc.join().unwrap();
    let sequential = sb.bin("frust").args(["compact", "--yes"]).output().unwrap();
    assert!(child.try_wait().unwrap().is_none(), "gc and compact finished while the build still held its lock");
    let after = tree(profile);
    let vanished: Vec<PathBuf> = before.into_iter().filter(|p| !after.contains(p)).collect();
    let status = child.wait().unwrap();
    assert!(status.success(), "the build survives concurrent gc and compact");
    Collected { gc, concurrent, sequential, vanished }
}

fn examined(report: &str) -> usize {
    let line = report.lines().find(|l| l.trim_start().starts_with("profiles")).unwrap_or_else(|| panic!("{report}"));
    line.split_whitespace().nth(1).and_then(|n| n.parse().ok()).unwrap_or_else(|| panic!("{line}"))
}

trait SpawnOutput {
    fn spawn_output(self) -> std::thread::JoinHandle<Output>;
}

impl SpawnOutput for &mut Command {
    fn spawn_output(self) -> std::thread::JoinHandle<Output> {
        let child = self.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
        std::thread::spawn(move || child.wait_with_output().unwrap())
    }
}

fn check_scenario(plain: bool) {
    let sb = Sandbox::new(if plain { "active-plain" } else { "active-fcargo" });
    let busy = workspace(&sb, "busy");
    let idle = workspace(&sb, "idle");
    warm(&sb, &busy, &["build", "--profile", "harness"]);
    warm(&sb, &idle, &["build"]);
    let busy_target = sb.central_target(&busy);
    let profile = busy_target.join("harness");
    let idle_profile = sb.central_target(&idle).join("debug");
    let started = sb.root.join("started");
    let mut build = sb.bin("fcargo");
    if plain {
        // FRUST_BYPASS runs cargo untouched: no frust build lock, only cargo's own profile lock protects this build.
        build.env("FRUST_BYPASS", "1").env("CARGO_TARGET_DIR", &busy_target);
    }
    build.args(["build", "--profile", "harness"]).current_dir(&busy).env("HOLD_SECS", "60").env("HOLD_STARTED", &started);
    build.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    let run = collect_during(&sb, build, &profile, &started);
    let (gc, concurrent) = (stdout(&run.gc), stdout(&run.concurrent));
    assert!(run.vanished.is_empty(), "removed under a running build: {:?}\ngc: {gc}\ncompact: {concurrent}", run.vanished);
    let skipped = stderr(&run.gc);
    assert!(
        skipped.lines().any(|l| l.contains(&profile.display().to_string()) && l.contains("a build holds this target directory")),
        "gc planned removals in the held profile and skipped them: {gc}\n{skipped}"
    );
    let busy_line = format!("skipped (a build holds it): {}", profile.display());
    assert!(concurrent.contains(&busy_line), "{concurrent}");
    let report = stdout(&run.sequential);
    assert!(report.contains(&busy_line), "{report}");
    assert!(!report.contains(&idle_profile.display().to_string()), "another workspace's build does not hold the idle one: {report}");
    assert!(examined(&report) >= 1, "the idle workspace is still compacted: {report}");
}

#[test]
fn plain_cargo_holding_a_custom_profile_keeps_it_through_gc_and_compact() {
    check_scenario(true);
}

#[test]
fn fcargo_holding_a_custom_profile_keeps_it_through_gc_and_compact() {
    check_scenario(false);
}
