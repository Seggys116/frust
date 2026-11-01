//! Concurrent compiles of one unit share a single rustc run through a per-pre-key lock, and no failure of that lock stops a build.

mod common;

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

use common::{Sandbox, age, assert_success, run_binary, stdout};
use frust::gc::{self, Class, GcOptions};
use frust::paths::Dirs;

const MAIN: &str = "fn main() { println!(\"{}\", leaf::value()); }\n";
const HOLD_ENV: &str = "FRUST_TEST_HOLD_LOCKS";

// Big enough that two builds started together are likely to reach it at the same time.
fn slow_leaf() -> String {
    let mut src = String::new();
    for i in 0..400 {
        src.push_str(&format!(
            "pub fn f{i}(x: u64) -> u64 {{ let mut v = x ^ {i}; for k in 0..{n} {{ v = v.wrapping_mul(6364136223846793005).wrapping_add(k); }} v }}\n",
            n = 3 + i % 7
        ));
    }
    src.push_str("pub fn value() -> u64 { let mut v = 1;\n");
    for i in 0..400 {
        src.push_str(&format!("    v = f{i}(v);\n"));
    }
    src.push_str("    v }\n");
    src
}

fn key_lock_files(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for shard in fs::read_dir(root).into_iter().flatten().flatten() {
        for file in fs::read_dir(shard.path()).into_iter().flatten().flatten() {
            found.push(file.path());
        }
    }
    found.sort();
    found
}

fn build_in(sb: &Sandbox, project: &Path, target: &Path) -> std::process::Output {
    sb.bin("fcargo").arg("build").current_dir(project).env("CARGO_TARGET_DIR", target).output().unwrap()
}

fn wipe_cache(sb: &Sandbox) {
    for dir in ["store", "manifests"] {
        let _ = fs::remove_dir_all(sb.home().join(dir));
    }
}

fn clean(sb: &Sandbox, project: &Path) {
    assert_success(&sb.bin("fcargo").arg("clean").current_dir(project).output().unwrap());
}

/// The cache event of the last compile of `leaf` that missed.
fn last_leaf_miss(sb: &Sandbox) -> serde_json::Value {
    sb.cache_events().into_iter().rfind(|e| e["crate"] == "leaf" && e["result"] == "miss").expect("a leaf miss")
}

/// A sandbox whose first build has run, so the key lock files of its units exist.
fn warmed(tag: &str) -> (Sandbox, PathBuf) {
    let sb = Sandbox::new(tag);
    sb.vendor_crate("leaf", "1.0.0", &slow_leaf());
    let a = sb.project("a", &[("leaf", "1.0.0")], MAIN);
    assert_success(&sb.bin("fcargo").arg("build").current_dir(&a).output().unwrap());
    assert_eq!(sb.count_cache("miss", "leaf"), 1);
    assert!(!key_lock_files(&sb.home().join("keylocks")).is_empty(), "compiling a unit creates its key lock file");
    (sb, a)
}

/// Re-runs this test binary as a process that holds every key lock under the directory named by `HOLD_ENV`; a plain run does nothing.
#[test]
fn hold_key_locks_child() {
    let Some(dir) = std::env::var_os(HOLD_ENV) else { return };
    let held: Vec<File> = key_lock_files(Path::new(&dir))
        .iter()
        .map(|path| {
            let file = File::open(path).unwrap();
            frust::sys::lock(&file, frust::sys::LockKind::Exclusive).unwrap();
            file
        })
        .collect();
    println!("HELD {}", held.len());
    std::io::stdout().flush().unwrap();
    std::thread::sleep(Duration::from_secs(120));
}

struct Holder(Child);

impl Holder {
    fn start(locks: &Path) -> Holder {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "hold_key_locks_child", "--nocapture", "--test-threads", "1"])
            .env(HOLD_ENV, locks)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        loop {
            line.clear();
            assert!(lines.read_line(&mut line).unwrap() > 0, "the lock holder exited before holding the locks");
            // libtest prints "test <name> ... " first, so the marker is not at the start of the line.
            if let Some(at) = line.find("HELD ") {
                assert!(line[at + 5..].trim().parse::<usize>().unwrap() > 0);
                break;
            }
        }
        // The pipe stays open on our side so the child never meets a broken one.
        std::mem::forget(lines);
        Holder(child)
    }

    fn kill(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Drop for Holder {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Two builds of different projects share the dependency's compile: one rustc run, one cache hit, identical results.
#[test]
fn concurrent_builds_compile_a_shared_dependency_once() {
    let sb = Sandbox::new("sf-concurrent");
    sb.vendor_crate("leaf", "1.0.0", &slow_leaf());
    let a = sb.project("a", &[("leaf", "1.0.0")], MAIN);
    let b = sb.project("b", &[("leaf", "1.0.0")], MAIN);
    let (target_a, target_b) = (sb.root.join("target-a"), sb.root.join("target-b"));

    let (out_a, out_b) = std::thread::scope(|scope| {
        let first = scope.spawn(|| build_in(&sb, &a, &target_a));
        let second = scope.spawn(|| build_in(&sb, &b, &target_b));
        (first.join().unwrap(), second.join().unwrap())
    });
    assert_success(&out_a);
    assert_success(&out_b);

    assert_eq!(sb.count_cache("miss", "leaf"), 1, "{:#?}", sb.cache_events());
    assert_eq!(sb.count_cache("hit", "leaf"), 1, "{:#?}", sb.cache_events());
    let (run_a, run_b) = (run_binary(&target_a, "debug", "app"), run_binary(&target_b, "debug", "app"));
    assert_success(&run_a);
    assert_success(&run_b);
    assert_eq!(stdout(&run_a), stdout(&run_b));
    assert!(!stdout(&run_a).trim().is_empty());
}

#[cfg(unix)]
struct CountedCompiler {
    tool: PathBuf,
    count: PathBuf,
    gate: PathBuf,
}

#[cfg(unix)]
impl CountedCompiler {
    fn new(sb: &Sandbox) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let tool = sb.root.join("counted-rustc");
        fs::write(&tool, r#"#!/bin/sh
crate=
emit=
previous=
for arg in "$@"; do
    if [ "$previous" = '--crate-name' ]; then crate=$arg; fi
    case "$arg" in
        --crate-name=*) crate=${arg#--crate-name=} ;;
        --emit|--emit=*) emit=yes ;;
    esac
    previous=$arg
done
if [ "$crate" = leaf ] && [ "$emit" = yes ]; then
    printf 'compile\n' >> "$FRUST_TEST_COMPILER_COUNT"
    attempts=0
    while [ ! -f "$FRUST_TEST_COMPILER_GATE" ]; do
        attempts=$((attempts + 1))
        if [ "$attempts" -gt 12000 ]; then exit 2; fi
        sleep 0.01
    done
fi
exec rustc "$@"
"#).unwrap();
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        Self { tool, count: sb.root.join("compiler-count"), gate: sb.root.join("compiler-gate") }
    }

    fn launches(&self) -> usize {
        fs::read_to_string(&self.count).unwrap_or_default().lines().count()
    }

    fn start(&self, sb: &Sandbox, project: &Path, profile: &str, deferred: bool) -> CountedBuild {
        let target = sb.root.join(format!("target-{profile}"));
        let stderr = sb.root.join(format!("stderr-{profile}"));
        let mut command = sb.bin("fcargo");
        command.args(["build", "--profile", profile]).current_dir(project)
            .env("CARGO_TARGET_DIR", target)
            .env("RUSTC", &self.tool)
            .env("FRUST_DEBUG", "1")
            .env("FRUST_TEST_COMPILER_COUNT", &self.count)
            .env("FRUST_TEST_COMPILER_GATE", &self.gate)
            .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(File::create(&stderr).unwrap());
        if deferred { command.env_remove("FRUST_STORE_SYNC"); }
        common::own_group(&mut command);
        CountedBuild { child: Some(command.spawn().unwrap()), stderr }
    }
}

#[cfg(unix)]
impl Drop for CountedCompiler {
    fn drop(&mut self) {
        let _ = fs::write(&self.gate, b"open");
    }
}

#[cfg(unix)]
struct CountedBuild {
    child: Option<Child>,
    stderr: PathBuf,
}

#[cfg(unix)]
impl CountedBuild {
    fn stderr(&self) -> String {
        fs::read_to_string(&self.stderr).unwrap_or_default()
    }

    fn finish(mut self) -> String {
        let output = self.child.take().unwrap().wait_with_output().unwrap();
        let stderr = self.stderr();
        assert!(output.status.success(), "{stderr}");
        stderr
    }
}

#[cfg(unix)]
impl Drop for CountedBuild {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            common::kill_tree(child.id());
            let _ = child.wait();
        }
    }
}

#[cfg(unix)]
fn until(what: &str, mut ready: impl FnMut() -> bool) {
    let begun = Instant::now();
    while !ready() {
        assert!(begun.elapsed() < Duration::from_secs(60), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
fn counted_profile_pair(tag: &str, deferred: bool, different_flags: bool) {
    let sb = Sandbox::new(tag);
    sb.vendor_crate("leaf", "1.0.0", &slow_leaf());
    let project = sb.project("app", &[("leaf", "1.0.0")], MAIN);
    let manifest = project.join("Cargo.toml");
    let mut text = fs::read_to_string(&manifest).unwrap();
    for profile in ["cooperate-a", "cooperate-b"] {
        text.push_str(&format!("\n[profile.{profile}]\ninherits = \"dev\"\ndebug = 0\nincremental = false\n"));
        if different_flags && profile == "cooperate-b" { text.push_str("opt-level = 1\n"); }
    }
    fs::write(manifest, text).unwrap();
    let compiler = CountedCompiler::new(&sb);
    let first = compiler.start(&sb, &project, "cooperate-a", deferred);
    until("the first real compiler launch", || compiler.launches() == 1);
    let second = compiler.start(&sb, &project, "cooperate-b", deferred);
    if different_flags {
        until("both distinct compiler invocations before either finishes", || compiler.launches() == 2);
    } else {
        until("the follower's cold cache lookup", || second.stderr().contains("lookup leaf missed"));
    }
    fs::write(&compiler.gate, b"open").unwrap();
    let first_stderr = first.finish();
    let second_stderr = second.finish();
    assert_eq!(compiler.launches(), if different_flags { 2 } else { 1 }, "{first_stderr}\n{second_stderr}");
    if !different_flags {
        assert_eq!(sb.count_cache("miss", "leaf"), 1);
        assert_eq!(sb.count_cache("hit", "leaf"), 1);
    } else {
        assert_eq!(sb.count_cache("miss", "leaf"), 2);
        assert_eq!(sb.count_cache("hit", "leaf"), 0);
    }
    let mut expected = None;
    for profile in ["cooperate-a", "cooperate-b"] {
        let output = run_binary(&sb.root.join(format!("target-{profile}")), profile, "app");
        assert_success(&output);
        assert!(!output.stdout.is_empty());
        if let Some(expected) = &expected { assert_eq!(&output.stdout, expected); }
        expected = Some(output.stdout);
    }
    if deferred {
        assert!(first_stderr.lines().any(|line| line.contains("compiled leaf ") && line.contains("store deferred")), "{first_stderr}");
        until("deferred stores to finish", || {
            fs::read_dir(Dirs::at(sb.home()).deferred()).is_ok_and(|mut entries| {
                entries.all(|entry| entry.is_ok_and(|entry| !entry.file_name().to_string_lossy().starts_with("job-")))
            })
        });
    }
}

#[cfg(unix)]
#[test]
fn identical_custom_profiles_share_one_actual_compiler_launch() {
    counted_profile_pair("sf-profiles-counted", false, false);
}

#[cfg(unix)]
#[test]
fn default_deferred_stores_share_one_actual_compiler_launch() {
    counted_profile_pair("sf-deferred-counted", true, false);
}

#[cfg(unix)]
#[test]
fn different_profile_flags_compile_independently_without_waiting_for_the_shared_gate() {
    counted_profile_pair("sf-different-flags-counted", false, true);
}

/// A holder that dies releases its lock with it, so a build waiting on it carries on.
#[test]
fn a_lock_held_by_a_killed_process_does_not_stop_the_build() {
    let (sb, a) = warmed("sf-killed");
    let mut holder = Holder::start(&sb.home().join("keylocks"));
    wipe_cache(&sb);
    clean(&sb, &a);

    let build = sb
        .bin("fcargo")
        .arg("build")
        .current_dir(&a)
        .env("FRUST_KEYLOCK_WAIT_SECS", "120")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    holder.kill();
    let out = build.wait_with_output().unwrap();

    assert_success(&out);
    assert_eq!(sb.count_cache("miss", "leaf"), 2, "{:#?}", sb.cache_events());
}

/// A lock that stays held past the wait cap is ignored: the build compiles anyway and records the time it lost.
#[test]
fn a_lock_held_past_the_wait_cap_is_compiled_through() {
    let (sb, a) = warmed("sf-capped");
    let held: Vec<File> = key_lock_files(&sb.home().join("keylocks"))
        .iter()
        .map(|path| {
            let file = File::open(path).unwrap();
            frust::sys::lock(&file, frust::sys::LockKind::Exclusive).unwrap();
            file
        })
        .collect();
    wipe_cache(&sb);
    clean(&sb, &a);

    let out = sb.bin("fcargo").arg("build").current_dir(&a).env("FRUST_KEYLOCK_WAIT_SECS", "2").output().unwrap();

    assert_success(&out);
    assert_eq!(sb.count_cache("miss", "leaf"), 2, "{:#?}", sb.cache_events());
    let waited = last_leaf_miss(&sb)["waited_ms"].as_u64().expect("the miss records how long it waited");
    assert!(waited >= 1000, "waited {waited}ms for a 2s cap");
    drop(held);
    let run = run_binary(&sb.central_target(&a), "debug", "app");
    assert_success(&run);
}

/// Only unheld locks older than a day are collected, whether the plan is an explicit one or the automatic one.
#[test]
fn gc_removes_stale_unheld_key_locks_and_keeps_held_and_recent_ones() {
    let sb = Sandbox::new("sf-gc");
    let home = sb.home();
    let dirs = Dirs::at(&home);
    let shard = home.join("keylocks").join("ab");
    fs::create_dir_all(&shard).unwrap();
    let make = |name: &str| {
        let path = shard.join(name);
        File::create(&path).unwrap();
        path
    };
    let stale = make(&format!("ab{}", "0".repeat(38)));
    let stale_held = make(&format!("ab{}", "1".repeat(38)));
    let recent = make(&format!("ab{}", "2".repeat(38)));
    age(&stale, Duration::from_secs(2 * 86_400));
    age(&stale_held, Duration::from_secs(2 * 86_400));
    let holding = File::open(&stale_held).unwrap();
    frust::sys::lock(&holding, frust::sys::LockKind::Exclusive).unwrap();

    let removes = |plan: &gc::Plan, path: &Path| plan.removals.iter().any(|r| r.class == Class::Metadata && r.path == path);
    let opts = GcOptions::new(u64::MAX);
    for plan in [gc::plan(&dirs, &opts), gc::plan_maintenance(&dirs, &opts)] {
        assert!(removes(&plan, &stale), "an old lock nobody holds");
        assert!(!removes(&plan, &stale_held), "an old lock a live process holds");
        assert!(!removes(&plan, &recent), "a recent lock");
    }

    let (_, failures) = gc::execute(&dirs, &gc::plan(&dirs, &opts));
    assert!(failures.is_empty(), "{failures:?}");
    assert!(!stale.exists() && stale_held.exists() && recent.exists());
    drop(holding);
}
