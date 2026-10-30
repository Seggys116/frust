//! Cache-safety regressions: mid-compile edits, unreported inputs, crafted metadata, state directory fallback, garbage collection.

mod common;

use std::fs::{self, File};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use common::{Sandbox, assert_success, run_binary, stdout};
use frust::gc::{self, Class, GcOptions};
use frust::paths::Dirs;

const LEAF_V1: &str = "pub fn value() -> u32 { 41 }\npub fn tag() -> &'static str { option_env!(\"FRUST_TEST_TAG\").unwrap_or(\"unset\") }\n";
const MAIN: &str = "fn main() { println!(\"{} {}\", leaf::value(), leaf::tag()); }\n";

fn write_executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn age(path: &Path, by: Duration) {
    // A read-only descriptor suffices for the owner, and works for directories too.
    File::open(path).unwrap().set_modified(SystemTime::now() - by).unwrap();
}

/// A source edited while rustc runs must not be stored under its new hash with output from the old content.
#[test]
fn source_edited_during_compile_is_not_stored_under_its_new_hash() {
    let sb = Sandbox::new("edit-during");
    sb.vendor_crate("leaf", "1.0.0", LEAF_V1);
    let a = sb.project("a", &[("leaf", "1.0.0")], MAIN);

    // A RUSTC that rewrites leaf's source after compiling it, like an editor saving mid-compile.
    let edited = sb.root.join("leaf-v2.rs");
    fs::write(&edited, LEAF_V1.replace("41", "42")).unwrap();
    let leaf_src = sb.vendor().join("leaf-1.0.0").join("src").join("lib.rs");
    let script = sb.root.join("editing-rustc");
    write_executable(
        &script,
        &format!(
            "#!/bin/sh\nrustc \"$@\"\nstatus=$?\ncase \" $* \" in\n  *\" --crate-name leaf \"*) cp '{}' '{}' ;;\nesac\nexit $status\n",
            edited.display(),
            leaf_src.display()
        ),
    );

    assert_success(&sb.bin("fcargo").arg("build").env("RUSTC", &script).current_dir(&a).output().unwrap());
    let out = run_binary(&sb.central_target(&a), "debug", "app");
    assert_eq!(stdout(&out).trim(), "41 unset", "the first build compiled the old source");
    assert_eq!(fs::read_to_string(&leaf_src).unwrap(), LEAF_V1.replace("41", "42"));

    assert_success(&sb.bin("fcargo").arg("clean").current_dir(&a).output().unwrap());
    assert_success(&sb.bin("fcargo").arg("build").current_dir(&a).output().unwrap());
    let out = run_binary(&sb.central_target(&a), "debug", "app");
    assert_eq!(stdout(&out).trim(), "42 unset", "the edited source must be compiled, not served the stale output");
    assert_eq!(sb.count_cache("hit", "leaf"), 0);
}

fn have_c_toolchain() -> bool {
    Command::new("cc").arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

fn build_static_lib(dir: &Path, value: i32) {
    fs::create_dir_all(dir).unwrap();
    fs::write(dir.join("foo.c"), format!("int foo_value(void) {{ return {value}; }}\n")).unwrap();
    assert!(Command::new("cc").args(["-c", "foo.c", "-o", "foo.o"]).current_dir(dir).status().unwrap().success());
    let _ = fs::remove_file(dir.join("libfoo.a"));
    assert!(Command::new("ar").args(["rcs", "libfoo.a", "foo.o"]).current_dir(dir).status().unwrap().success());
}

/// A static library from a `-L native=` directory is bundled into the rlib, so its contents must be in the key.
#[test]
fn native_library_outside_the_target_is_keyed_by_content() {
    if !have_c_toolchain() {
        eprintln!("skipping: no C compiler");
        return;
    }
    let sb = Sandbox::new("native");
    let libdir = sb.root.join("external-lib");
    build_static_lib(&libdir, 1);

    sb.vendor_crate("nat", "1.0.0", "unsafe extern \"C\" { fn foo_value() -> i32; }\npub fn value() -> i32 { unsafe { foo_value() } }\n");
    fs::write(
        sb.vendor().join("nat-1.0.0").join("build.rs"),
        format!(
            "fn main() {{\n    println!(\"cargo:rustc-link-search=native={}\");\n    println!(\"cargo:rustc-link-lib=static=foo\");\n}}\n",
            libdir.display()
        ),
    )
    .unwrap();
    let a = sb.project("a", &[("nat", "1.0.0")], "fn main() { println!(\"{}\", nat::value()); }\n");

    assert_success(&sb.bin("fcargo").arg("build").current_dir(&a).output().unwrap());
    assert_eq!(stdout(&run_binary(&sb.central_target(&a), "debug", "app")).trim(), "1");

    build_static_lib(&libdir, 2);
    assert_success(&sb.bin("fcargo").arg("clean").current_dir(&a).output().unwrap());
    assert_success(&sb.bin("fcargo").arg("build").current_dir(&a).output().unwrap());
    assert_eq!(
        stdout(&run_binary(&sb.central_target(&a), "debug", "app")).trim(),
        "2",
        "a changed native library must not be served from an entry built against the old one"
    );
    assert_eq!(sb.count_cache("hit", "nat"), 0);
}

#[cfg(target_os = "macos")]
fn minos(binary: &Path) -> String {
    let out = Command::new("otool").arg("-l").arg(binary).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.lines()
        .find_map(|l| l.trim().strip_prefix("minos ").map(|v| v.trim().to_string()))
        .unwrap_or_else(|| panic!("no minos in otool output:\n{text}"))
}

/// MACOSX_DEPLOYMENT_TARGET changes every emitted object but never appears in dep-info, so it must be in the pre-key.
#[cfg(target_os = "macos")]
#[test]
fn deployment_target_is_part_of_the_key() {
    let sb = Sandbox::new("deploy");
    sb.vendor_crate("leaf", "1.0.0", LEAF_V1);
    let a = sb.project("a", &[("leaf", "1.0.0")], MAIN);

    let build = |target: &str| {
        assert_success(&sb.bin("fcargo").arg("clean").current_dir(&a).output().unwrap());
        assert_success(
            &sb.bin("fcargo").args(["build", "--release"]).env("MACOSX_DEPLOYMENT_TARGET", target).current_dir(&a).output().unwrap(),
        );
        minos(&sb.central_target(&a).join("release").join("app"))
    };
    let first = build("11.0");
    let second = build("13.0");
    assert_eq!(first, "11.0");
    assert_eq!(second, "13.0", "a different deployment target must not restore objects built for another");
    assert_eq!(sb.count_cache("hit", "leaf"), 0);
    assert_eq!(sb.count_cache("hit", "app"), 0);
    assert_eq!(build("11.0"), "11.0");
    assert_eq!(sb.count_cache("hit", "leaf"), 1);
}

#[test]
fn crafted_entry_metadata_cannot_escape_the_output_directory() {
    let sb = Sandbox::new("traversal");
    sb.vendor_crate("leaf", "1.0.0", LEAF_V1);
    let a = sb.project("a", &[("leaf", "1.0.0")], MAIN);
    assert_success(&sb.bin("fcargo").arg("build").current_dir(&a).output().unwrap());

    let mut patched = 0;
    for shard in fs::read_dir(sb.home().join("store")).unwrap().flatten() {
        for entry in fs::read_dir(shard.path()).unwrap().flatten() {
            let meta_path = entry.path().join("meta.json");
            let mut meta: serde_json::Value = serde_json::from_slice(&fs::read(&meta_path).unwrap()).unwrap();
            let files = meta["files"].as_array_mut().unwrap();
            let mut evil = files[0].clone();
            evil["name"] = serde_json::Value::String("../../escaped-by-frust".into());
            files.push(evil);
            fs::write(&meta_path, serde_json::to_vec(&meta).unwrap()).unwrap();
            patched += 1;
        }
    }
    assert!(patched > 0, "the first build stored entries");

    assert_success(&sb.bin("fcargo").arg("clean").current_dir(&a).output().unwrap());
    assert_success(&sb.bin("fcargo").arg("build").current_dir(&a).output().unwrap());
    let target = sb.central_target(&a);
    assert!(!target.join("escaped-by-frust").exists(), "restore wrote outside the output directory");
    assert!(!target.join("debug").join("escaped-by-frust").exists());
    assert_eq!(sb.count_cache("hit", "leaf"), 0, "a crafted entry is a miss");
    assert_eq!(stdout(&run_binary(&target, "debug", "app")).trim(), "41 unset");
}

#[test]
fn private_temp_fallback_is_0700_and_refuses_shared_directories() {
    let sb = Sandbox::new("private-tmp");
    let base = sb.root.join("tmp");
    fs::create_dir_all(&base).unwrap();
    fs::set_permissions(&base, fs::Permissions::from_mode(0o1777)).unwrap();

    let dir = frust::paths::private_temp_dir(&base).expect("a fresh private directory is accepted");
    assert!(dir.starts_with(&base) && dir != base, "never the shared directory itself");
    assert_eq!(fs::metadata(&dir).unwrap().permissions().mode() & 0o7777, 0o700);
    assert_eq!(frust::paths::private_temp_dir(&base).unwrap(), dir, "reused on the next call");

    fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).unwrap();
    let err = frust::paths::private_temp_dir(&base).unwrap_err();
    assert!(err.contains("mode"), "{err}");

    let other = sb.root.join("tmp2");
    fs::create_dir_all(&other).unwrap();
    let decoy = sb.root.join("decoy");
    fs::create_dir_all(&decoy).unwrap();
    fs::set_permissions(&decoy, fs::Permissions::from_mode(0o700)).unwrap();
    let name = dir.file_name().unwrap();
    std::os::unix::fs::symlink(&decoy, other.join(name)).unwrap();
    let err = frust::paths::private_temp_dir(&other).unwrap_err();
    assert!(err.contains("not a directory"), "a symlink planted in the shared directory is refused: {err}");
}

fn dead_pid() -> u32 {
    let mut child = Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

fn fake_entry(home: &Path, key: &str) -> PathBuf {
    let dir = home.join("store").join(&key[..2]).join(key);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("meta.json"), b"{}").unwrap();
    fs::write(dir.join("f0"), vec![7u8; 64 * 1024]).unwrap();
    dir
}

fn fake_manifest(home: &Path, prekey: &str, keys: &[&str]) -> PathBuf {
    let path = home.join("manifests").join(&prekey[..2]).join(format!("{prekey}.json"));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let candidates: Vec<_> = keys.iter().map(|k| serde_json::json!({"inputs": [], "envs": [], "key": k})).collect();
    fs::write(&path, serde_json::to_vec(&serde_json::json!({ "candidates": candidates })).unwrap()).unwrap();
    path
}

fn plan_has(plan: &gc::Plan, class: Class, path: &Path) -> bool {
    plan.removals.iter().any(|r| r.class == class && r.path == path)
}

#[test]
fn gc_prunes_stale_metadata_and_leftover_temp_files() {
    let sb = Sandbox::new("gc-meta");
    let home = sb.home();
    let dirs = Dirs::at(&home);

    let live_key = "aa11111111111111111111111111111111111111";
    fake_entry(&home, live_key);
    let kept_manifest = fake_manifest(&home, "bb22222222222222222222222222222222222222", &[live_key]);
    let orphan_manifest = fake_manifest(&home, "cc33333333333333333333333333333333333333", &["dd44444444444444444444444444444444444444"]);
    let fresh_orphan = fake_manifest(&home, "ee55555555555555555555555555555555555555", &["ff66666666666666666666666666666666666666"]);
    let ancient_manifest = fake_manifest(&home, "ab77777777777777777777777777777777777777", &[live_key]);
    age(&kept_manifest, Duration::from_secs(2 * 3600));
    age(&orphan_manifest, Duration::from_secs(2 * 3600));
    age(&ancient_manifest, Duration::from_secs(40 * 86_400));

    // Lock directories: one idle and unheld, one whose flock is held right now.
    let stale_lock = home.join("locks").join("stale000000");
    fs::create_dir_all(stale_lock.join("queue")).unwrap();
    for name in ["lock", "target", "last.json"] {
        fs::write(stale_lock.join(name), b"x").unwrap();
        age(&stale_lock.join(name), Duration::from_secs(10 * 86_400));
    }
    age(&stale_lock.join("queue"), Duration::from_secs(10 * 86_400));
    age(&stale_lock, Duration::from_secs(10 * 86_400));
    let held_lock = home.join("locks").join("held0000000");
    fs::create_dir_all(held_lock.join("queue")).unwrap();
    fs::write(held_lock.join("lock"), b"").unwrap();
    age(&held_lock.join("lock"), Duration::from_secs(10 * 86_400));
    age(&held_lock.join("queue"), Duration::from_secs(10 * 86_400));
    age(&held_lock, Duration::from_secs(10 * 86_400));
    let holder = File::open(held_lock.join("lock")).unwrap();
    holder.lock().unwrap();

    // History: one for a workspace that has no target directory any more.
    fs::create_dir_all(home.join("history")).unwrap();
    let orphan_history = home.join("history").join("gone-0123456789ab.json");
    fs::write(&orphan_history, b"{\"units\":3}").unwrap();
    age(&orphan_history, Duration::from_secs(8 * 86_400));
    let live_history = home.join("history").join("live-0123456789ab.json");
    fs::write(&live_history, b"{\"units\":3}").unwrap();
    age(&live_history, Duration::from_secs(8 * 86_400));
    let deps = home.join("targets").join("live-0123456789ab").join("tc").join("debug").join("deps");
    fs::create_dir_all(&deps).unwrap();

    // Temp siblings left in deps/ by a killed restore, and one owned by a live process.
    let dead = deps.join(format!(".libfoo-0123456789abcdef.rlib.frust-{}-0-12345", dead_pid()));
    fs::write(&dead, b"partial").unwrap();
    let mine = deps.join(format!(".libbar-0123456789abcdef.rlib.frust-{}-0-12345", std::process::id()));
    fs::write(&mine, b"in flight").unwrap();
    let artifact = deps.join("libfoo-0123456789abcdef.rlib");
    fs::write(&artifact, b"real").unwrap();

    let mut opts = GcOptions::new(u64::MAX);
    opts.temp_min_age = Duration::ZERO;
    let plan = gc::plan(&dirs, &opts);

    assert!(plan_has(&plan, Class::Metadata, &orphan_manifest), "manifest whose entries are gone");
    assert!(plan_has(&plan, Class::Metadata, &ancient_manifest), "manifest past the age limit");
    assert!(!plan.removals.iter().any(|r| r.path == kept_manifest), "manifest with a live entry is kept");
    assert!(!plan.removals.iter().any(|r| r.path == fresh_orphan), "a just-written manifest is never touched");
    assert!(plan_has(&plan, Class::Metadata, &stale_lock), "idle, unheld lock directory");
    assert!(!plan.removals.iter().any(|r| r.path == held_lock), "a held lock directory is kept");
    assert!(plan_has(&plan, Class::Metadata, &orphan_history), "history of a vanished workspace");
    assert!(!plan.removals.iter().any(|r| r.path == live_history));
    assert!(plan_has(&plan, Class::Scratch, &dead), "temp file of a dead process");
    assert!(!plan.removals.iter().any(|r| r.path == mine || r.path == artifact));

    let (_, failures) = gc::execute(&dirs, &plan);
    assert!(failures.is_empty(), "{failures:?}");
    assert!(!orphan_manifest.exists() && !stale_lock.exists() && !orphan_history.exists() && !dead.exists());
    assert!(kept_manifest.exists() && held_lock.exists() && live_history.exists() && mine.exists() && artifact.exists());
    drop(holder);
}

#[test]
fn auto_gc_runs_once_per_day_and_spares_target_directories() {
    let sb = Sandbox::new("auto-gc");
    let home = sb.home();
    let dirs = Dirs::at(&home);
    let entry = fake_entry(&home, "aa11111111111111111111111111111111111111");
    let target = home.join("targets").join("idle-0123456789ab").join("tc").join("debug").join("deps");
    fs::create_dir_all(&target).unwrap();
    fs::write(home.join("targets").join("idle-0123456789ab").join("workspace"), "/nonexistent/workspace").unwrap();

    let mut cfg = frust::config::Config::from_env();
    cfg.cache_max_bytes = 0;

    let ran: Vec<bool> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8).map(|_| scope.spawn(|| gc::maybe_auto_gc(&dirs, &cfg))).collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert_eq!(ran.iter().filter(|r| **r).count(), 1, "exactly one concurrent caller collects: {ran:?}");
    assert!(!entry.exists(), "cache entries over budget are evicted");
    assert!(target.exists(), "target directories are never removed automatically, even orphaned ones");
    assert!(!gc::maybe_auto_gc(&dirs, &cfg), "not again within a day");

    // Children of other tests can hold a copy of the marker's flock until they exec, declining the claim, so retry.
    age(&dirs.auto_gc_marker(), Duration::from_secs(2 * 86_400));
    let ran_again = (0..50).any(|_| {
        gc::maybe_auto_gc(&dirs, &cfg) || {
            std::thread::sleep(Duration::from_millis(20));
            false
        }
    });
    assert!(ran_again, "due again after a day");
}
