//! Automatic compaction runs detached after a build and yields to builds between short slices of work.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::time::{Duration, Instant};

use common::{Sandbox, age, assert_success, run_binary, stderr};
use frust::compact;

const SLICE_MS: &str = "100";

/// The sandbox runs due passes inline for determinism; these tests exercise the detached default.
fn build(sb: &Sandbox, app: &Path, envs: &[(&str, &str)]) -> (Output, Duration) {
    let started = Instant::now();
    let out = sb.bin("fcargo").args(["build", "-v"]).env_remove("FRUST_COMPACT_SYNC").envs(envs.iter().copied()).current_dir(app).output().unwrap();
    let wall = started.elapsed();
    assert_success(&out);
    (out, wall)
}

fn compiled(out: &Output) -> Vec<String> {
    stderr(out).lines().filter(|l| l.trim_start().starts_with("Compiling")).map(|l| l.trim().to_string()).collect()
}

fn edit(app: &Path, n: usize) {
    fs::write(app.join("src").join("main.rs"), format!("fn main() {{ println!(\"edit {n}\"); }}\n")).unwrap();
}

fn base36(mut value: u64, width: usize) -> String {
    let mut digits = Vec::new();
    while value > 0 {
        digits.push(char::from_digit((value % 36) as u32, 36).unwrap());
        value /= 36;
    }
    let text: String = digits.iter().rev().collect();
    format!("{text:0>width$}")
}

/// The path library's only finalized incremental session and its crate directory; editing the app never recompiles it.
fn current_session(target: &Path) -> (PathBuf, String) {
    let incremental = target.join("debug").join("incremental");
    for krate in fs::read_dir(&incremental).unwrap().flatten() {
        if !krate.file_name().to_string_lossy().starts_with("util-") {
            continue;
        }
        for session in fs::read_dir(krate.path()).unwrap().flatten() {
            let name = session.file_name().to_string_lossy().into_owned();
            if name.starts_with("s-") && session.path().is_dir() && !name.ends_with("-working") {
                return (krate.path(), name);
            }
        }
    }
    panic!("no finalized util session under {}", incremental.display());
}

/// Older finalized sessions, each with its lock file as rustc leaves them: superseded state removed one directory at a time.
fn superseded_sessions(target: &Path, count: usize) -> PathBuf {
    let (krate, name) = current_session(target);
    let parts: Vec<&str> = name.split('-').collect();
    let newest = u64::from_str_radix(parts[1], 36).unwrap();
    for i in 0..count {
        let base = format!("s-{}-{}", base36(newest - 1 - i as u64, 10), base36(i as u64, 7));
        let dir = krate.join(format!("{base}-{}", parts[3]));
        fs::create_dir(&dir).unwrap();
        for f in 0..40 {
            fs::write(dir.join(format!("q{f}.bin")), [f as u8; 256]).unwrap();
        }
        fs::write(krate.join(format!("{base}.lock")), b"").unwrap();
    }
    krate
}

fn sessions_in(krate: &Path) -> usize {
    fs::read_dir(krate).map(|rd| rd.flatten().filter(|e| e.file_name().to_string_lossy().starts_with("s-") && e.path().is_dir()).count()).unwrap_or(0)
}

fn marker(target: &Path) -> Option<serde_json::Value> {
    serde_json::from_slice(&fs::read(target.join(compact::AUTO_MARKER)).ok()?).ok()
}

fn background(sb: &Sandbox, target: &Path) -> Child {
    let mut cmd: Command = sb.bin("frust");
    cmd.args(["compact", compact::AUTO_CHILD_FLAG]).arg(target).env("FRUST_COMPACT_SLICE_MS", SLICE_MS);
    cmd.spawn().unwrap()
}

/// An incomplete or interrupted pass is retried after 30 seconds.
fn retry_due(target: &Path) {
    age(&target.join(compact::AUTO_MARKER), Duration::from_secs(31));
}

fn wait_until(what: &str, limit: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_exit(child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(120);
    while child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "the background pass did not finish");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn fixture(tag: &str, sessions: usize) -> (Sandbox, PathBuf, PathBuf, PathBuf) {
    let sb = Sandbox::new(tag);
    let app = sb.project("app", &[], "fn main() { println!(\"edit 0\"); }\n");
    let manifest = fs::read_to_string(app.join("Cargo.toml")).unwrap();
    fs::write(app.join("Cargo.toml"), format!("{manifest}util = {{ path = \"util\" }}\n")).unwrap();
    fs::create_dir_all(app.join("util").join("src")).unwrap();
    fs::write(app.join("util").join("Cargo.toml"), "[package]\nname = \"util\"\nversion = \"0.1.0\"\nedition = \"2021\"\n").unwrap();
    fs::write(app.join("util").join("src").join("lib.rs"), "pub fn tag() -> &'static str { \"util\" }\n").unwrap();
    build(&sb, &app, &[("FRUST_AUTO_COMPACT", "off")]);
    let target = sb.central_target(&app);
    let krate = superseded_sessions(&target, sessions);
    let _ = fs::remove_file(target.join(compact::AUTO_MARKER));
    (sb, app, target, krate)
}

#[test]
fn a_build_returns_before_due_compaction_runs_in_its_own_process() {
    let (sb, app, target, krate) = fixture("bg-detached", 150);
    edit(&app, 1);
    let (out, _) = build(&sb, &app, &[("FRUST_COMPACT_SLICE_MS", SLICE_MS), ("FRUST_DEBUG", "1")]);
    let left = sessions_in(&krate);
    assert!(stderr(&out).contains("auto-compact started in the background"), "{}", stderr(&out));
    assert!(left > 75, "the build returned before compaction did its work, {left} sessions remain");
    wait_until("the background pass", Duration::from_secs(120), || sessions_in(&krate) == 1 && marker(&target).is_some_and(|m| m["complete"] == true));
    let (out, _) = build(&sb, &app, &[]);
    assert!(compiled(&out).is_empty(), "compaction keeps the build fresh: {}", stderr(&out));
    assert_eq!(String::from_utf8_lossy(&run_binary(&target, "debug", "app").stdout).trim(), "edit 1");
}

#[test]
fn a_build_started_during_compaction_waits_at_most_one_slice() {
    let (sb, app, target, krate) = fixture("bg-yield", 150);
    edit(&app, 1);
    let (_, quiet) = build(&sb, &app, &[("FRUST_AUTO_COMPACT", "off")]);
    let total = sessions_in(&krate);
    let mut pass = background(&sb, &target);
    wait_until("the pass to start removing", Duration::from_secs(60), || sessions_in(&krate) < total - 3);
    edit(&app, 2);
    let (out, contended) = build(&sb, &app, &[("FRUST_AUTO_COMPACT", "off"), ("FRUST_DEBUG", "1")]);
    let acquired = stderr(&out)
        .lines()
        .find_map(|l| l.strip_prefix("frust: lock acquired, starting cargo at +").and_then(|r| r.strip_suffix("ms")).and_then(|ms| ms.parse::<u64>().ok()))
        .expect("lock trace");
    assert!(acquired < 1000, "frust waited {acquired} ms for the compaction slice");
    assert!(!stderr(&out).contains("Blocking waiting for file lock"), "cargo never waits for compaction: {}", stderr(&out));
    assert!(contended < quiet + Duration::from_secs(1), "contended {contended:?} vs quiet {quiet:?}");
    wait_exit(&mut pass);
    let after_abort = sessions_in(&krate);
    assert!(after_abort > 1, "the pass stopped for the build and left work for the next one");
    assert_eq!(marker(&target).unwrap()["complete"], false);

    retry_due(&target);
    let mut resumed = background(&sb, &target);
    wait_exit(&mut resumed);
    assert_eq!(sessions_in(&krate), 1, "the next pass finishes the work");
    assert_eq!(marker(&target).unwrap()["complete"], true);
    let (out, _) = build(&sb, &app, &[("FRUST_AUTO_COMPACT", "off")]);
    assert!(compiled(&out).is_empty(), "{}", stderr(&out));
    assert_eq!(String::from_utf8_lossy(&run_binary(&target, "debug", "app").stdout).trim(), "edit 2");
}

#[test]
fn a_killed_pass_leaves_a_valid_target_and_the_next_pass_resumes() {
    let (sb, app, target, krate) = fixture("bg-kill", 150);
    let total = sessions_in(&krate);
    let mut pass = background(&sb, &target);
    wait_until("the pass to start removing", Duration::from_secs(60), || sessions_in(&krate) < total - 3);
    pass.kill().unwrap();
    let _ = pass.wait();
    let left = sessions_in(&krate);
    assert!(left > 1, "killed mid-pass");
    let (out, _) = build(&sb, &app, &[("FRUST_AUTO_COMPACT", "off")]);
    assert!(compiled(&out).is_empty(), "an interrupted pass keeps the build fresh: {}", stderr(&out));
    assert_eq!(String::from_utf8_lossy(&run_binary(&target, "debug", "app").stdout).trim(), "edit 0");

    retry_due(&target);
    let mut resumed = background(&sb, &target);
    wait_exit(&mut resumed);
    assert_eq!(sessions_in(&krate), 1);
    assert_eq!(marker(&target).unwrap()["complete"], true);
    retry_due(&target);
    let mut again = background(&sb, &target);
    wait_exit(&mut again);
    assert_eq!(marker(&target).unwrap()["complete"], true, "a repeated pass is idempotent");
    let (out, _) = build(&sb, &app, &[("FRUST_AUTO_COMPACT", "off")]);
    assert!(compiled(&out).is_empty(), "{}", stderr(&out));
}

#[test]
fn only_one_background_pass_runs_per_target() {
    let (sb, _app, target, krate) = fixture("bg-single", 150);
    let total = sessions_in(&krate);
    let mut first = background(&sb, &target);
    wait_until("the first pass to start removing", Duration::from_secs(60), || sessions_in(&krate) < total - 3);
    let started = Instant::now();
    let mut second = background(&sb, &target);
    wait_exit(&mut second);
    assert!(first.try_wait().unwrap().is_none(), "the second pass returned at once ({:?}) while the first still runs", started.elapsed());
    assert!(sessions_in(&krate) > 1);
    wait_exit(&mut first);
    assert_eq!(sessions_in(&krate), 1);
}
