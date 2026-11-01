//! Cache-safety regressions: mid-compile edits, unreported inputs, crafted metadata, state directory fallback, garbage collection.

mod common;

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use common::{Sandbox, age, assert_success, run_binary, stdout};
use frust::gc::{self, Class, GcOptions};
use frust::paths::Dirs;

const LEAF_V1: &str = "pub fn value() -> u32 { 41 }\npub fn tag() -> &'static str { option_env!(\"FRUST_TEST_TAG\").unwrap_or(\"unset\") }\n";
const MAIN: &str = "fn main() { println!(\"{} {}\", leaf::value(), leaf::tag()); }\n";

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
    let script = common::write_fake_tool(
        &script,
        &format!(
            "#!/bin/sh\nrustc \"$@\"\nstatus=$?\ncase \" $* \" in\n  *\" --crate-name leaf \"*) cp '{}' '{}' ;;\nesac\nexit $status\n",
            edited.display(),
            leaf_src.display()
        ),
        &format!(
            "@echo off\r\nrustc %*\r\nset status=%ERRORLEVEL%\r\nfor %%a in (%*) do if \"%%a\"==\"leaf\" type \"{}\" > \"{}\"\r\nexit /b %status%\r\n",
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

// The `cc` and `ar` static-library setup is Unix-specific; Windows toolchains use cl and lib.
#[cfg(unix)]
fn have_c_toolchain() -> bool {
    Command::new("cc").arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

#[cfg(unix)]
fn build_static_lib(dir: &Path, value: i32) {
    fs::create_dir_all(dir).unwrap();
    fs::write(dir.join("foo.c"), format!("int foo_value(void) {{ return {value}; }}\n")).unwrap();
    assert!(Command::new("cc").args(["-c", "foo.c", "-o", "foo.o"]).current_dir(dir).status().unwrap().success());
    let _ = fs::remove_file(dir.join("libfoo.a"));
    assert!(Command::new("ar").args(["rcs", "libfoo.a", "foo.o"]).current_dir(dir).status().unwrap().success());
}

/// A static library from a `-L native=` directory is bundled into the rlib, so its contents must be in the key.
#[cfg(unix)]
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

#[cfg(windows)]
#[test]
fn private_temp_fallback_accepts_a_fresh_directory_and_refuses_a_junction() {
    let sb = Sandbox::new("private-tmp");
    let base = sb.root.join("tmp");
    fs::create_dir_all(&base).unwrap();

    let dir = frust::paths::private_temp_dir(&base).expect("a fresh private directory is accepted");
    assert!(dir.starts_with(&base) && dir != base, "never the shared directory itself");
    assert_eq!(frust::paths::private_temp_dir(&base).unwrap(), dir, "reused on the next call");

    let other = sb.root.join("tmp2");
    fs::create_dir_all(&other).unwrap();
    let decoy = sb.root.join("decoy");
    fs::create_dir_all(&decoy).unwrap();
    let link = other.join(dir.file_name().unwrap());
    // A junction needs no privilege, unlike a symlink.
    let made = Command::new("cmd").arg("/c").arg("mklink").arg("/J").arg(&link).arg(&decoy).output().unwrap();
    assert!(made.status.success(), "mklink /J failed: {}", String::from_utf8_lossy(&made.stdout));
    let err = frust::paths::private_temp_dir(&other).unwrap_err();
    assert!(err.contains("not a directory"), "a junction planted in the shared directory is refused: {err}");
}

// File modes and symlink privileges make the mode-based refusals unix-only.
#[cfg(unix)]
#[test]
fn private_temp_fallback_is_0700_and_refuses_shared_directories() {
    use std::os::unix::fs::PermissionsExt;
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
    let mut child = common::spawn_exiting();
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
    fs::write(&path, serde_json::to_vec(&serde_json::json!({ "format": 4, "candidates": candidates })).unwrap()).unwrap();
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
    frust::sys::lock(&holder, frust::sys::LockKind::Exclusive).unwrap();

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
    assert!(!plan_has(&plan, Class::Metadata, &ancient_manifest), "old manifest with a recently used entry is kept");
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
    assert!(ancient_manifest.exists() && kept_manifest.exists() && held_lock.exists() && live_history.exists() && mine.exists() && artifact.exists());
    drop(holder);
}

#[test]
fn gc_retains_old_manifests_that_still_serve_current_cache_entries() {
    let sb = Sandbox::new("gc-active-manifest");
    sb.vendor_crate("leaf", "1.0.0", LEAF_V1);
    let project = sb.project("a", &[("leaf", "1.0.0")], MAIN);
    assert_success(
        &sb.bin("fcargo")
            .arg("build")
            .current_dir(&project)
            .output()
            .unwrap(),
    );
    let dirs = Dirs::at(sb.home());
    let mut manifests = Vec::new();
    for shard in fs::read_dir(dirs.manifests()).unwrap().flatten() {
        for manifest in fs::read_dir(shard.path()).unwrap().flatten() {
            let path = manifest.path();
            let data: serde_json::Value =
                serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            assert_eq!(data["format"], 5);
            age(&path, Duration::from_secs(40 * 86_400));
            manifests.push(path);
        }
    }
    assert!(!manifests.is_empty());
    let plan = gc::plan(&dirs, &GcOptions::new(u64::MAX));
    for path in &manifests {
        assert!(!plan_has(&plan, Class::Metadata, path));
    }
    let (_, failures) = gc::execute(&dirs, &plan);
    assert!(failures.is_empty(), "{failures:?}");
    assert_success(
        &sb.bin("fcargo")
            .arg("clean")
            .current_dir(&project)
            .output()
            .unwrap(),
    );
    assert_success(
        &sb.bin("fcargo")
            .arg("build")
            .current_dir(&project)
            .output()
            .unwrap(),
    );
    assert!(
        sb.count_cache("hit", "leaf") > 0,
        "the old lookup manifest must still serve the compiled leaf"
    );
    assert_eq!(
        stdout(&run_binary(&sb.central_target(&project), "debug", "app")).trim(),
        "41 unset"
    );
}

#[test]
fn gc_expires_idle_manifests_and_rechecks_usage_replacement_and_republication() {
    let sb = Sandbox::new("gc-manifest-recheck");
    let home = sb.home();
    let dirs = Dirs::at(&home);
    let store = frust::cache::Cache::new(&dirs);
    let mut fixtures = Vec::new();
    for index in 1..=4 {
        let key = format!("{index:040x}");
        let prekey = format!("{:040x}", index + 10);
        let entry = fake_entry(&home, &key);
        let manifest = fake_manifest(&home, &prekey, &[&key]);
        age(&entry.join("meta.json"), Duration::from_secs(40 * 86_400));
        age(&manifest, Duration::from_secs(40 * 86_400));
        fixtures.push((key, prekey, manifest));
    }
    let plan = gc::plan(&dirs, &GcOptions::new(u64::MAX));
    for (_, _, path) in &fixtures {
        assert!(plan_has(&plan, Class::Metadata, path));
    }
    store.mark_used(&fixtures[1].0);
    let replacement = fixtures[2].2.with_extension("replacement");
    fs::write(&replacement, fs::read(&fixtures[2].2).unwrap()).unwrap();
    age(&replacement, Duration::from_secs(40 * 86_400));
    frust::sys::rename(&replacement, &fixtures[2].2).unwrap();
    store.add_candidate(
        &fixtures[3].1,
        frust::cache::Candidate {
            inputs: Vec::new(),
            envs: Vec::new(),
            key: fixtures[3].0.clone(),
        },
    );
    let (_, failures) = gc::execute(&dirs, &plan);
    assert!(failures.is_empty(), "{failures:?}");
    assert!(!fixtures[0].2.exists(), "a genuinely idle manifest expires");
    assert!(
        fixtures[1].2.exists(),
        "a hit after planning preserves its manifest"
    );
    assert!(
        fixtures[2].2.exists(),
        "replacement invalidates the planned file identity"
    );
    assert!(
        fixtures[3].2.exists(),
        "unchanged candidate republication refreshes usage"
    );
}

#[test]
fn gc_rechecks_orphan_manifests_against_entries_published_after_planning() {
    let sb = Sandbox::new("gc-orphan-recheck");
    let home = sb.home();
    let dirs = Dirs::at(&home);
    let key = "aa11111111111111111111111111111111111111";
    let manifest = fake_manifest(&home, "bb22222222222222222222222222222222222222", &[key]);
    age(&manifest, Duration::from_secs(2 * 3600));
    let plan = gc::plan(&dirs, &GcOptions::new(u64::MAX));
    assert!(plan_has(&plan, Class::Metadata, &manifest));
    fake_entry(&home, key);
    let (_, failures) = gc::execute(&dirs, &plan);
    assert!(failures.is_empty(), "{failures:?}");
    assert!(
        manifest.exists(),
        "a newly surviving reference invalidates orphan removal"
    );
}

#[test]
fn gc_requires_actual_eviction_before_removing_an_active_manifest() {
    let sb = Sandbox::new("gc-planned-eviction");
    let home = sb.home();
    let dirs = Dirs::at(&home);
    let key = "aa11111111111111111111111111111111111111";
    let entry = fake_entry(&home, key);
    let manifest = fake_manifest(&home, "bb22222222222222222222222222222222222222", &[key]);
    age(&manifest, Duration::from_secs(2 * 3600));
    let mut plan = gc::plan(&dirs, &GcOptions::new(0));
    assert!(plan_has(&plan, Class::CacheEntry, &entry));
    assert!(plan_has(&plan, Class::Metadata, &manifest));
    plan.removals
        .retain(|removal| removal.class != Class::CacheEntry);
    let (_, failures) = gc::execute(&dirs, &plan);
    assert!(failures.is_empty(), "{failures:?}");
    assert!(
        manifest.exists(),
        "a planned eviction does not prove the reference disappeared"
    );
}

#[test]
fn gc_preserves_manifests_when_reference_metadata_cannot_be_read_as_a_file() {
    let sb = Sandbox::new("gc-uncertain-reference");
    let home = sb.home();
    let dirs = Dirs::at(&home);
    let key = "aa11111111111111111111111111111111111111";
    let entry = home.join("store").join(&key[..2]).join(key);
    fs::create_dir_all(entry.join("meta.json")).unwrap();
    let manifest = fake_manifest(&home, "bb22222222222222222222222222222222222222", &[key]);
    age(&manifest, Duration::from_secs(40 * 86_400));
    let plan = gc::plan(&dirs, &GcOptions::new(u64::MAX));
    assert!(!plan_has(&plan, Class::Metadata, &manifest));
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
    cfg.gc_sync = true;

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

#[test]
fn gc_retries_fresh_retired_children_and_accounts_only_successful_namespace_removals() {
    let sb = Sandbox::new("retired-retry-accounting");
    let dirs = Dirs::at(sb.home());
    let retired = frust::cache::evicted_dir(&dirs);
    let child = frust::fsx::temp_sibling(&retired.join("entry"));
    fs::create_dir_all(&child).unwrap();
    fs::write(child.join("f0"), vec![0x59; 16384]).unwrap();
    let bytes = frust::fsx::allocated_size(&child);
    let plan = gc::plan_maintenance(&dirs, &GcOptions::new(u64::MAX));
    assert!(plan_has(&plan, Class::Scratch, &child), "fresh retry children need no age grace");
    assert!(!plan.removals.iter().any(|r| r.path == retired));
    let (freed, failures) = gc::execute(&dirs, &plan);
    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(freed, bytes, "report allocations removed from the namespace");
    assert!(!child.exists());
    assert!(retired.is_dir());
    let (freed_again, failures) = gc::execute(&dirs, &plan);
    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(freed_again, 0, "a stale plan must not count an absent retired child twice");
}

#[test]
fn gc_retired_queue_parent_never_swallows_arrivals_after_planning() {
    let sb = Sandbox::new("retired-parent-arrival");
    let dirs = Dirs::at(sb.home());
    let retired = frust::cache::evicted_dir(&dirs);
    let old = frust::fsx::temp_sibling(&retired.join("entry"));
    fs::create_dir_all(&old).unwrap();
    fs::write(old.join("f0"), b"old retired payload").unwrap();
    age(&retired, Duration::from_secs(2 * 3600));
    let plan = gc::plan(&dirs, &GcOptions::new(u64::MAX));
    assert!(!plan.removals.iter().any(|r| r.path == retired));
    let new = frust::fsx::temp_sibling(&retired.join("entry"));
    fs::create_dir(&new).unwrap();
    fs::write(new.join("f0"), b"new retired payload").unwrap();
    let (_, failures) = gc::execute(&dirs, &plan);
    assert!(failures.is_empty(), "{failures:?}");
    assert!(!old.exists());
    assert_eq!(fs::read(new.join("f0")).unwrap(), b"new retired payload");
    assert!(retired.is_dir());
    let retry = gc::plan_maintenance(&dirs, &GcOptions::new(u64::MAX));
    assert!(plan_has(&retry, Class::Scratch, &new));
    let (_, failures) = gc::execute(&dirs, &retry);
    assert!(failures.is_empty(), "{failures:?}");
    assert!(!new.exists());
    assert!(retired.is_dir());
}

#[cfg(unix)]
#[test]
fn retired_queue_and_tmp_symlinks_decline_planning_sweeping_and_retirement() {
    for bad_tmp in [false, true] {
        let sb = Sandbox::new("retired-symlink-boundary");
        let dirs = Dirs::at(sb.home());
        let key = "aa11111111111111111111111111111111111111";
        let live = fake_entry(&sb.home(), key);
        let outside = sb.root.join("outside");
        let queue_target = if bad_tmp { outside.join("evicted") } else { outside.clone() };
        fs::create_dir_all(&queue_target).unwrap();
        let protected = frust::fsx::temp_sibling(&queue_target.join("entry"));
        fs::create_dir(&protected).unwrap();
        fs::write(protected.join("f0"), b"external bytes").unwrap();
        if bad_tmp {
            std::os::unix::fs::symlink(&outside, dirs.tmp()).unwrap();
        } else {
            fs::create_dir_all(dirs.tmp()).unwrap();
            std::os::unix::fs::symlink(&outside, frust::cache::evicted_dir(&dirs)).unwrap();
        }
        let plan = gc::plan_maintenance(&dirs, &GcOptions::new(u64::MAX));
        assert!(!plan.removals.iter().any(|r| r.path.starts_with(frust::cache::evicted_dir(&dirs))));
        let (_, errors) = gc::execute(&dirs, &plan);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(frust::cache::sweep_evicted(&dirs, Duration::ZERO), 0);
        frust::cache::Cache::new(&dirs).remove_entry(key);
        assert!(live.is_dir(), "unsafe queue boundaries leave the live entry alone");
        assert_eq!(fs::read(protected.join("f0")).unwrap(), b"external bytes");
    }
}

#[cfg(unix)]
#[test]
fn retired_queue_replacement_after_planning_does_not_remove_external_children() {
    let sb = Sandbox::new("retired-queue-replaced");
    let dirs = Dirs::at(sb.home());
    let queue = frust::cache::evicted_dir(&dirs);
    fs::create_dir_all(&queue).unwrap();
    let child = frust::fsx::temp_sibling(&queue.join("entry"));
    fs::create_dir(&child).unwrap();
    fs::write(child.join("f0"), b"original retired bytes").unwrap();
    let plan = gc::plan_maintenance(&dirs, &GcOptions::new(u64::MAX));
    assert!(plan_has(&plan, Class::Scratch, &child));
    let parked = dirs.tmp().join("original-queue");
    frust::sys::rename(&queue, &parked).unwrap();
    let outside = sb.root.join("outside");
    let decoy = outside.join(child.file_name().unwrap());
    fs::create_dir_all(&decoy).unwrap();
    fs::write(decoy.join("f0"), b"external replacement bytes").unwrap();
    std::os::unix::fs::symlink(&outside, &queue).unwrap();
    let (freed, errors) = gc::execute(&dirs, &plan);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(freed, 0);
    assert_eq!(fs::read(decoy.join("f0")).unwrap(), b"external replacement bytes");
    assert_eq!(fs::read(parked.join(child.file_name().unwrap()).join("f0")).unwrap(), b"original retired bytes");
    assert_eq!(frust::cache::sweep_evicted(&dirs, Duration::ZERO), 0);
}

#[cfg(unix)]
#[test]
fn retired_queue_keeps_unknown_children_and_symlink_targets() {
    let sb = Sandbox::new("retired-unknown-and-symlink");
    let dirs = Dirs::at(sb.home());
    let queue = frust::cache::evicted_dir(&dirs);
    let unknown = queue.join("unrecognized-child");
    fs::create_dir_all(&unknown).unwrap();
    fs::write(unknown.join("f0"), b"unknown bytes").unwrap();
    let outside = sb.root.join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("f0"), b"symlink target bytes").unwrap();
    let link = frust::fsx::temp_sibling(&queue.join("entry"));
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    let plan = gc::plan_maintenance(&dirs, &GcOptions::new(u64::MAX));
    assert!(!plan.removals.iter().any(|r| r.path.starts_with(&queue)));
    let (_, errors) = gc::execute(&dirs, &plan);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(frust::cache::sweep_evicted(&dirs, Duration::ZERO), 0);
    assert_eq!(fs::read(unknown.join("f0")).unwrap(), b"unknown bytes");
    assert_eq!(fs::read(outside.join("f0")).unwrap(), b"symlink target bytes");
    assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
}

#[cfg(unix)]
#[test]
fn retired_queue_checks_allow_a_configured_symlinked_home() {
    let sb = Sandbox::new("retired-linked-home");
    let key = "aa11111111111111111111111111111111111111";
    let live = fake_entry(&sb.home(), key);
    let linked = sb.root.join("linked-home");
    std::os::unix::fs::symlink(sb.home(), &linked).unwrap();
    let dirs = Dirs::at(&linked);
    frust::cache::Cache::new(&dirs).remove_entry(key);
    assert!(!live.exists());
    assert_eq!(fs::read_dir(frust::cache::evicted_dir(&dirs)).unwrap().count(), 0);
}

#[cfg(unix)]
#[test]
fn retired_child_identity_replacement_invalidates_its_planned_removal() {
    for use_symlink in [false, true] {
        let sb = Sandbox::new("retired-child-replaced");
        let dirs = Dirs::at(sb.home());
        let queue = frust::cache::evicted_dir(&dirs);
        fs::create_dir_all(&queue).unwrap();
        let child = frust::fsx::temp_sibling(&queue.join("entry"));
        fs::create_dir(&child).unwrap();
        fs::write(child.join("f0"), b"planned retired bytes").unwrap();
        let plan = gc::plan_maintenance(&dirs, &GcOptions::new(u64::MAX));
        assert!(plan_has(&plan, Class::Scratch, &child));
        let parked = queue.join("parked-original-child");
        frust::sys::rename(&child, &parked).unwrap();
        if use_symlink {
            let outside = sb.root.join("outside");
            fs::create_dir(&outside).unwrap();
            fs::write(outside.join("f0"), b"replacement bytes").unwrap();
            std::os::unix::fs::symlink(&outside, &child).unwrap();
        } else {
            fs::create_dir(&child).unwrap();
            fs::write(child.join("f0"), b"replacement bytes").unwrap();
        }
        let (freed, errors) = gc::execute(&dirs, &plan);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(freed, 0);
        assert_eq!(fs::read(child.join("f0")).unwrap(), b"replacement bytes");
        assert_eq!(fs::read(parked.join("f0")).unwrap(), b"planned retired bytes");
    }
}

fn unit_record(dirs: &Dirs, unit: &str, keys: &[&str]) -> PathBuf {
    let path = dirs.root().join(frust::cache::UNITS_DIR).join(&unit[..2]).join(unit);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, format!("{}\n", keys.join("\n"))).unwrap();
    age(&path, Duration::from_secs(40 * 86_400));
    path
}

#[test]
fn gc_keeps_aged_unit_records_while_any_referenced_entry_root_exists() {
    let sb = Sandbox::new("unit-record-live-retention");
    let dirs = Dirs::at(sb.home());
    let key = "aa11111111111111111111111111111111111111";
    let unit = "bb22222222222222222222222222222222222222";
    let live = fake_entry(&sb.home(), key);
    age(&live.join("meta.json"), Duration::from_secs(80 * 86_400));
    let record = unit_record(&dirs, unit, &[key]);
    let plan = gc::plan_maintenance(&dirs, &GcOptions::new(u64::MAX));
    assert!(!plan_has(&plan, Class::Metadata, &record));
    fs::remove_file(live.join("meta.json")).unwrap();
    let incomplete = gc::plan_maintenance(&dirs, &GcOptions::new(u64::MAX));
    assert!(!plan_has(&incomplete, Class::Metadata, &record), "an existing root with missing metadata is uncertain, not absent");
    assert!(record.is_file());
}

#[test]
fn gc_retires_only_aged_unit_records_whose_references_are_demonstrably_absent() {
    let sb = Sandbox::new("unit-record-absent-retirement");
    let dirs = Dirs::at(sb.home());
    let key = "aa11111111111111111111111111111111111111";
    let unit = "bb22222222222222222222222222222222222222";
    let record = unit_record(&dirs, unit, &[key]);
    let plan = gc::plan_maintenance(&dirs, &GcOptions::new(u64::MAX));
    assert!(plan_has(&plan, Class::Metadata, &record));
    let (_, errors) = gc::execute(&dirs, &plan);
    assert!(errors.is_empty(), "{errors:?}");
    assert!(!record.exists());
}

#[test]
fn gc_unit_record_plan_is_invalidated_by_revived_entries_or_a_serialized_writer() {
    for update_record in [false, true] {
        let sb = Sandbox::new("unit-record-plan-invalidated");
        let dirs = Dirs::at(sb.home());
        let old = "aa11111111111111111111111111111111111111";
        let new = "cc33333333333333333333333333333333333333";
        let unit = "bb22222222222222222222222222222222222222";
        let record = unit_record(&dirs, unit, &[old]);
        let plan = gc::plan_maintenance(&dirs, &GcOptions::new(u64::MAX));
        assert!(plan_has(&plan, Class::Metadata, &record));
        let expected = if update_record {
            fake_entry(&sb.home(), new);
            frust::cache::Cache::new(&dirs).note_unit_entry(unit, new, 2);
            age(&record, Duration::from_secs(40 * 86_400));
            new
        } else { fake_entry(&sb.home(), old); old };
        let (_, errors) = gc::execute(&dirs, &plan);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(fs::read_to_string(&record).unwrap(), format!("{expected}\n"));
    }
}

#[test]
fn gc_unit_record_cleanup_skips_a_busy_writer_lock_and_retries_after_unlock() {
    let sb = Sandbox::new("unit-record-busy-lock");
    let dirs = Dirs::at(sb.home());
    let key = "aa11111111111111111111111111111111111111";
    let unit = "bb22222222222222222222222222222222222222";
    let record = unit_record(&dirs, unit, &[key]);
    let plan = gc::plan_maintenance(&dirs, &GcOptions::new(u64::MAX));
    assert!(plan_has(&plan, Class::Metadata, &record));
    fs::create_dir_all(dirs.keylocks()).unwrap();
    let held = File::options().read(true).write(true).create(true).truncate(false).open(dirs.keylocks().join("unit-records")).unwrap();
    frust::sys::lock(&held, frust::sys::LockKind::Exclusive).unwrap();
    let (_, errors) = gc::execute(&dirs, &plan);
    assert!(errors.is_empty(), "{errors:?}");
    assert!(record.is_file(), "busy metadata writers are skipped without waiting");
    frust::sys::unlock(&held).unwrap();
    let (_, errors) = gc::execute(&dirs, &plan);
    assert!(errors.is_empty(), "{errors:?}");
    assert!(!record.exists());
}

#[test]
fn gc_retains_unknown_unit_record_bytes_and_uncertain_reference_paths() {
    let sb = Sandbox::new("unit-record-uncertainty");
    let dirs = Dirs::at(sb.home());
    let key = "aa11111111111111111111111111111111111111";
    let unknown_unit = "bb22222222222222222222222222222222222222";
    let uncertain_unit = "cc33333333333333333333333333333333333333";
    let unknown = unit_record(&dirs, unknown_unit, &[key]);
    fs::write(&unknown, b"\xffunknown record bytes\n").unwrap();
    age(&unknown, Duration::from_secs(40 * 86_400));
    let uncertain = unit_record(&dirs, uncertain_unit, &[key]);
    fs::create_dir_all(dirs.store()).unwrap();
    fs::write(dirs.store().join(&key[..2]), b"unexpected shard file").unwrap();
    let plan = gc::plan_maintenance(&dirs, &GcOptions::new(u64::MAX));
    assert!(!plan_has(&plan, Class::Metadata, &unknown));
    assert!(!plan_has(&plan, Class::Metadata, &uncertain), "NotADirectory is not authoritative absence");
    let (_, errors) = gc::execute(&dirs, &plan);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(fs::read(&unknown).unwrap(), b"\xffunknown record bytes\n");
    assert!(uncertain.is_file());
}

#[cfg(unix)]
#[test]
fn gc_retains_unit_records_when_reference_permissions_are_denied() {
    let sb = Sandbox::new("unit-record-reference-permissions");
    let dirs = Dirs::at(sb.home());
    let key = "aa11111111111111111111111111111111111111";
    let unit = "bb22222222222222222222222222222222222222";
    fake_entry(&sb.home(), key);
    let record = unit_record(&dirs, unit, &[key]);
    let shard = dirs.store().join(&key[..2]);
    frust::sys::set_path_mode(&shard, 0).unwrap();
    let plan = gc::plan_maintenance(&dirs, &GcOptions::new(u64::MAX));
    frust::sys::set_path_mode(&shard, 0o755).unwrap();
    assert!(!plan_has(&plan, Class::Metadata, &record));
    assert!(record.is_file());
}

#[test]
fn gc_unit_record_absence_requires_directory_parents_on_every_platform() {
    for layout in ["missing-store", "missing-shard", "store-file", "shard-file"] {
        let sb = Sandbox::new("unit-record-parent-types");
        let dirs = Dirs::at(sb.home());
        let key = "aa11111111111111111111111111111111111111";
        let unit = "bb22222222222222222222222222222222222222";
        let record = unit_record(&dirs, unit, &[key]);
        match layout {
            "missing-store" => {}
            "missing-shard" => fs::create_dir_all(dirs.store()).unwrap(),
            "store-file" => fs::write(dirs.store(), b"unexpected store file").unwrap(),
            "shard-file" => {
                fs::create_dir_all(dirs.store()).unwrap();
                fs::write(dirs.store().join(&key[..2]), b"unexpected shard file").unwrap();
            }
            _ => unreachable!(),
        }
        let plan = gc::plan_maintenance(&dirs, &GcOptions::new(u64::MAX));
        let absent = layout.starts_with("missing");
        assert_eq!(plan_has(&plan, Class::Metadata, &record), absent, "layout {layout}");
        let (_, errors) = gc::execute(&dirs, &plan);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(record.exists(), !absent, "layout {layout}");
    }
}

#[cfg(unix)]
#[test]
fn gc_unit_record_absence_rejects_symlinked_store_or_shard_parents() {
    for symlink_store in [false, true] {
        let sb = Sandbox::new("unit-record-symlinked-parent");
        let dirs = Dirs::at(sb.home());
        let key = "aa11111111111111111111111111111111111111";
        let unit = "bb22222222222222222222222222222222222222";
        let record = unit_record(&dirs, unit, &[key]);
        let outside = sb.root.join("empty-outside");
        fs::create_dir(&outside).unwrap();
        if symlink_store {
            std::os::unix::fs::symlink(&outside, dirs.store()).unwrap();
        } else {
            fs::create_dir_all(dirs.store()).unwrap();
            std::os::unix::fs::symlink(&outside, dirs.store().join(&key[..2])).unwrap();
        }
        let plan = gc::plan_maintenance(&dirs, &GcOptions::new(u64::MAX));
        assert!(!plan_has(&plan, Class::Metadata, &record));
        let (_, errors) = gc::execute(&dirs, &plan);
        assert!(errors.is_empty(), "{errors:?}");
        assert!(record.is_file());
        assert!(outside.is_dir());
    }
}

#[cfg(unix)]
#[test]
fn gc_unit_record_absence_allows_an_explicit_symlinked_home() {
    let sb = Sandbox::new("unit-record-symlinked-home");
    let original = Dirs::at(sb.home());
    let key = "aa11111111111111111111111111111111111111";
    let unit = "bb22222222222222222222222222222222222222";
    let record = unit_record(&original, unit, &[key]);
    let linked = sb.root.join("linked-home");
    std::os::unix::fs::symlink(sb.home(), &linked).unwrap();
    let dirs = Dirs::at(linked);
    let linked_record = dirs.root().join(frust::cache::UNITS_DIR).join(&unit[..2]).join(unit);
    let plan = gc::plan_maintenance(&dirs, &GcOptions::new(u64::MAX));
    assert!(plan_has(&plan, Class::Metadata, &linked_record));
    let (_, errors) = gc::execute(&dirs, &plan);
    assert!(errors.is_empty(), "{errors:?}");
    assert!(!record.exists());
}
