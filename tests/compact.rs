//! `frust compact` must not change what cargo considers fresh or what a binary does.

// The variant tests are unix-only, which leaves their helpers unused on Windows.
#![cfg_attr(windows, allow(dead_code, unused_imports))]

mod common;

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{Sandbox, age, assert_success, run_binary, stderr, stdout};
use frust::compact::{self, AutoOptions};
use frust::config::Config;
use frust::gc::{self, Class, GcOptions};
use frust::paths::Dirs;

fn big_leaf() -> String {
    let mut src = String::new();
    for i in 0..300 {
        src.push_str(&format!(
            "pub fn f{i}(x: u64) -> u64 {{ let mut v = x ^ {i}; for k in 0..{n} {{ v = v.wrapping_mul(6364136223846793005).wrapping_add(k); }} v }}\n",
            n = 3 + i % 7
        ));
    }
    src.push_str("pub fn all(x: u64) -> u64 { let mut v = x;\n");
    for i in 0..300 {
        src.push_str(&format!("    v = f{i}(v);\n"));
    }
    src.push_str("    v }\n");
    src
}

const MAIN: &str = "fn main() { let tag = if cfg!(feature = \"x\") { \"x\" } else { \"plain\" }; println!(\"{} {}\", tag, leaf::all(1)); }\n";

fn project(sb: &Sandbox) -> PathBuf {
    sb.vendor_crate("leaf", "1.0.0", &big_leaf());
    let app = sb.project("app", &[("leaf", "1.0.0")], MAIN);
    let manifest = fs::read_to_string(app.join("Cargo.toml")).unwrap();
    fs::write(app.join("Cargo.toml"), format!("{manifest}\n[features]\nx = []\n")).unwrap();
    app
}

fn build(sb: &Sandbox, app: &Path, args: &[&str]) -> std::process::Output {
    let out = sb.bin("fcargo").arg("build").arg("-v").args(args).current_dir(app).output().unwrap();
    assert_success(&out);
    out
}

fn compiled(out: &std::process::Output) -> Vec<String> {
    stderr(out).lines().filter(|l| l.trim_start().starts_with("Compiling")).map(|l| l.trim().to_string()).collect()
}

fn listing(dir: &Path) -> String {
    let mut names: Vec<String> = fs::read_dir(dir).map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
    names.sort();
    format!("{}: {}", dir.display(), names.join(" "))
}

fn hashes_of(dir: &Path, prefix: &str) -> Vec<String> {
    let mut out: Vec<String> = fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with(prefix))
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

// Windows names a binary without a hash, so feature variants of a bin share one fingerprint directory and none is superseded.
#[cfg(unix)]
#[test]
fn compact_keeps_the_current_build_fresh_and_removes_superseded_variants() {
    let sb = Sandbox::new("compact");
    let app = project(&sb);
    let first = build(&sb, &app, &[]);
    let second = build(&sb, &app, &["--features", "x"]);
    build(&sb, &app, &["--release", "--features", "x"]);
    let target = sb.central_target(&app);
    let debug = target.join("debug");
    assert_eq!(
        hashes_of(&debug.join(".fingerprint"), "app-").len(),
        2,
        "two variants of app before compaction\n{}\n{}\nfirst build:\n{}\nsecond build:\n{}",
        listing(&debug.join(".fingerprint")),
        listing(&debug.join("deps")),
        stderr(&first),
        stderr(&second)
    );

    let out = sb.bin("frust").args(["compact", "--yes"]).output().unwrap();
    assert_success(&out);
    let text = stdout(&out);
    assert!(text.contains("physical size"), "{text}");

    assert_eq!(hashes_of(&debug.join(".fingerprint"), "app-").len(), 1, "{text}");
    assert_eq!(hashes_of(&debug.join(".fingerprint"), "leaf-").len(), 1, "leaf is live through the current app");

    if cfg!(target_os = "macos") {
        assert!(!text.contains("compress 0 files"), "artifacts were compressed: {text}");
    }

    for profile_args in [&["--features", "x"][..], &["--release", "--features", "x"][..]] {
        let again = build(&sb, &app, profile_args);
        assert!(compiled(&again).is_empty(), "compaction made something stale: {:?}\n{}", compiled(&again), stderr(&again));
    }
    let expected = run_binary(&target, "debug", "app");
    assert_success(&expected);
    assert!(stdout(&expected).starts_with("x "), "{}", stdout(&expected));
    let release = run_binary(&target, "release", "app");
    assert_eq!(stdout(&release), stdout(&expected));

    let back = build(&sb, &app, &[]);
    let rebuilt = compiled(&back);
    assert!(rebuilt.iter().all(|l| l.contains("app")), "only the app variant was removed: {rebuilt:?}");
    let plain = run_binary(&target, "debug", "app");
    assert!(stdout(&plain).starts_with("plain "), "{}", stdout(&plain));

    let second = sb.bin("frust").args(["compact", "--yes", "--keep-variants"]).output().unwrap();
    assert_success(&second);
    assert!(compiled(&build(&sb, &app, &[])).is_empty());
}

// Windows names a binary without a hash, so feature variants of a bin share one fingerprint directory and none is superseded.
#[cfg(unix)]
#[test]
fn compact_refuses_to_remove_without_confirmation_but_dry_run_changes_nothing() {
    let sb = Sandbox::new("compact-confirm");
    let app = project(&sb);
    build(&sb, &app, &[]);
    build(&sb, &app, &["--features", "x"]);
    let fingerprints = sb.central_target(&app).join("debug").join(".fingerprint");
    let before = hashes_of(&fingerprints, "app-");

    let dry = sb.bin("frust").args(["compact", "--dry-run"]).output().unwrap();
    assert_success(&dry);
    assert!(stdout(&dry).contains("would remove"), "{}", stdout(&dry));
    assert_eq!(hashes_of(&fingerprints, "app-"), before);

    let refused = sb.bin("frust").arg("compact").stdin(std::process::Stdio::null()).output().unwrap();
    assert!(!refused.status.success(), "non-interactive compact must not delete unasked");
    assert_eq!(hashes_of(&fingerprints, "app-"), before);
}

// Windows names a binary without a hash, so feature variants of a bin share one fingerprint directory and none is superseded.
#[cfg(unix)]
#[test]
fn compact_skips_a_profile_a_build_is_using() {
    let sb = Sandbox::new("compact-busy");
    let app = project(&sb);
    build(&sb, &app, &[]);
    build(&sb, &app, &["--features", "x"]);
    let debug = sb.central_target(&app).join("debug");
    let before = hashes_of(&debug.join(".fingerprint"), "app-");

    // Hold cargo's own lock the way a running build does.
    let lock = File::options().read(true).write(true).open(debug.join(".cargo-lock")).unwrap();
    frust::sys::lock(&lock, frust::sys::LockKind::Exclusive).unwrap();
    let out = sb.bin("frust").args(["compact", "--yes"]).output().unwrap();
    assert_success(&out);
    assert!(stdout(&out).contains("skipped"), "{}", stdout(&out));
    assert_eq!(hashes_of(&debug.join(".fingerprint"), "app-"), before, "nothing removed under a held lock");
    drop(lock);

    let lock = File::options().read(true).write(true).open(debug.join(".cargo-lock")).unwrap();
    frust::sys::lock(&lock, frust::sys::LockKind::Exclusive).unwrap();
    let dirs = Dirs::at(sb.home());
    let plan = gc::plan(&dirs, &GcOptions::new(u64::MAX));
    assert!(plan.removals.iter().any(|r| r.class == Class::StaleVariant), "the superseded variant is planned");
    let (_, failures) = gc::execute(&dirs, &plan);
    assert!(!failures.is_empty(), "removal under a held lock is reported, not done");
    assert_eq!(hashes_of(&debug.join(".fingerprint"), "app-"), before);
}

fn plan_has(plan: &gc::Plan, class: Class, path: &Path) -> bool {
    plan.removals.iter().any(|r| r.class == class && r.path == path)
}

/// Old memos and temp files left by killed processes are pruned; fresh memos stay.
#[test]
fn gc_prunes_old_startup_memos() {
    let sb = Sandbox::new("gc-memos");
    let home = sb.home();
    let dirs = Dirs::at(&home);
    for sub in ["probe", "locate"] {
        fs::create_dir_all(home.join(sub)).unwrap();
    }
    let old_probe = home.join("probe").join("0123456789abcdef.json");
    let fresh_probe = home.join("probe").join("fedcba9876543210.json");
    let old_locate = home.join("locate").join("00112233445566ff.json");
    let fresh_locate = home.join("locate").join("ffeeddccbbaa0099.json");
    for p in [&old_probe, &fresh_probe, &old_locate, &fresh_locate] {
        fs::write(p, b"{}").unwrap();
    }
    age(&old_probe, Duration::from_secs(31 * 86_400));
    age(&old_locate, Duration::from_secs(31 * 86_400));
    age(&fresh_locate, Duration::from_secs(29 * 86_400));
    // A temp sibling from a process that is long gone (pid far beyond any real one).
    let temp = home.join("locate").join(".00112233445566ff.json.frust-99999999-1-1");
    fs::write(&temp, b"{").unwrap();

    let plan = gc::plan(&dirs, &GcOptions::new(u64::MAX));
    assert!(plan_has(&plan, Class::Metadata, &old_probe), "old probe memo");
    assert!(plan_has(&plan, Class::Metadata, &old_locate), "old locate memo");
    assert!(!plan.removals.iter().any(|r| r.path == fresh_probe || r.path == fresh_locate), "fresh memos stay");
    // The temp file is younger than the minimum temp age, so it stays for now.
    assert!(!plan.removals.iter().any(|r| r.path == temp), "a fresh temp file is never removed");

    let maintenance = gc::plan_maintenance(&dirs, &GcOptions::new(u64::MAX));
    assert!(plan_has(&maintenance, Class::Metadata, &old_probe), "automatic maintenance prunes memos too");
    let (_, failures) = gc::execute(&dirs, &plan);
    assert!(failures.is_empty(), "{failures:?}");
    assert!(!old_probe.exists() && !old_locate.exists() && fresh_probe.exists() && fresh_locate.exists());
}

fn eager() -> AutoOptions {
    let mut opts = AutoOptions::from_env(&Config::from_env());
    opts.enabled = true;
    opts.every = Duration::ZERO;
    opts.min_growth = 0;
    opts.uncached_dead_grace = Duration::ZERO;
    opts.max_target_bytes = u64::MAX;
    opts.pass_budget = Duration::from_secs(3600);
    opts.pass_bytes = u64::MAX;
    opts
}

fn allocated(dir: &Path) -> u64 {
    compact::breakdown(dir).total().allocated
}

fn auto_marker(target: &Path) -> serde_json::Value {
    serde_json::from_slice(&fs::read(target.join(compact::AUTO_MARKER)).unwrap()).unwrap()
}

fn write_auto_marker(target: &Path, marker: &serde_json::Value) {
    fs::write(target.join(compact::AUTO_MARKER), serde_json::to_vec(marker).unwrap()).unwrap();
}

fn age_reassessment_cleans_unchanged_target(signature_matches: bool) {
    let sb = Sandbox::new(if signature_matches { "auto-age-signature" } else { "auto-age-growth" });
    let dirs = Dirs::at(sb.home());
    let target = dirs.targets().join("age-reassessment");
    let profile = target.join("debug");
    let deps = profile.join("deps");
    let session = profile.join("incremental").join("app-1abc").join("s-0000000001-0000001-0000000000000000000000000");
    fs::create_dir_all(&deps).unwrap();
    fs::create_dir_all(&session).unwrap();
    fs::write(session.join("dep-graph.bin"), b"current session").unwrap();
    fs::write(session.parent().unwrap().join("s-0000000001-0000001.lock"), b"").unwrap();
    let fingerprints = profile.join(".fingerprint");
    let old_unit = fingerprints.join("app-0000000000000001");
    let current_unit = fingerprints.join("app-0000000000000002");
    for unit in [&old_unit, &current_unit] {
        fs::create_dir_all(unit).unwrap();
        fs::write(unit.join("bin-app.json"), r#"{"rustc":1,"profile":1,"compile_kind":0,"deps":[]}"#).unwrap();
        fs::write(unit.join("bin-app"), "1111111111111111").unwrap();
    }
    let old_output = deps.join("app-0000000000000001");
    let current_output = deps.join("app-0000000000000002");
    fs::write(&old_output, b"superseded output").unwrap();
    fs::write(&current_output, b"current output").unwrap();

    let mut opts = eager();
    opts.min_growth = u64::MAX;
    opts.compress = false;
    opts.cache_enabled = false;
    opts.uncached_dead_grace = Duration::from_secs(3600);
    let first = compact::auto_compact_with(&dirs, &target, &opts);
    assert!(first.skipped.is_none() && first.errors.is_empty(), "{first:?}");
    assert!(old_output.exists(), "a recent uncached variant stays warm for its grace period");
    let mut marker = auto_marker(&target);
    let last_pass = marker["at_unix"].as_u64().unwrap();
    assert!(last_pass > 7200);
    assert_ne!(marker["signature"], 0);

    // Fingerprint leaf mtimes determine retention without changing the shallow directory signature or target size.
    age(&old_unit.join("bin-app.json"), Duration::from_secs(7200));
    age(&old_unit.join("bin-app"), Duration::from_secs(7200));
    if !signature_matches {
        marker["signature"] = 0.into();
        write_auto_marker(&target, &marker);
    }
    let skipped = compact::auto_compact_with(&dirs, &target, &opts);
    assert_eq!(
        skipped.skipped,
        Some(if signature_matches { "nothing changed since the last pass" } else { "grew too little since the last pass" }),
        "{skipped:?}"
    );
    assert_eq!(auto_marker(&target)["at_unix"], last_pass, "cheap skips preserve the actual pass time");
    assert!(old_output.exists());

    let mut marker = auto_marker(&target);
    marker["at_unix"] = (last_pass - 7200).into();
    if !signature_matches {
        marker["signature"] = 0.into();
    }
    write_auto_marker(&target, &marker);
    let cleaned = compact::auto_compact_with(&dirs, &target, &opts);
    assert!(cleaned.skipped.is_none() && cleaned.errors.is_empty(), "{cleaned:?}");
    assert_eq!(cleaned.dead_units, 1, "{cleaned:?}");
    assert!(cleaned.removed.iter().any(|(p, _)| p == &old_output), "{cleaned:?}");
    assert!(!old_output.exists() && !old_unit.exists(), "age-based cleanup runs despite an unchanged or barely grown target");
    assert!(current_unit.exists());
    assert_eq!(fs::read(current_output).unwrap(), b"current output");
    assert_eq!(fs::read(session.join("dep-graph.bin")).unwrap(), b"current session");
    assert!(auto_marker(&target)["at_unix"].as_u64().unwrap() >= last_pass);
    assert_eq!(auto_marker(&target)["complete"], true);
}

#[test]
fn auto_compaction_reassesses_age_despite_an_unchanged_signature() {
    age_reassessment_cleans_unchanged_target(true);
}

#[test]
fn auto_compaction_reassesses_age_despite_insufficient_growth() {
    age_reassessment_cleans_unchanged_target(false);
}

const MAIN_WITH_TEST: &str = "fn main() { let tag = if cfg!(feature = \"x\") { \"x\" } else { \"plain\" }; println!(\"{} {}\", tag, leaf::all(1)); }\n#[test]\nfn adds() { assert_eq!(leaf::all(1), leaf::all(1)); }\n";

// Windows names a binary without a hash, so feature variants of a bin share one fingerprint directory and none is superseded.
#[cfg(unix)]
#[test]
fn auto_compaction_shrinks_the_target_and_keeps_every_build_warm() {
    let sb = Sandbox::new("auto-compact");
    let app = project(&sb);
    fs::write(app.join("src").join("main.rs"), MAIN_WITH_TEST).unwrap();
    let x: &[&str] = &["--features", "x"];
    let rx: &[&str] = &["--release", "--features", "x"];
    build(&sb, &app, &[]);
    for i in 0..4 {
        fs::write(app.join("src").join("main.rs"), format!("{MAIN_WITH_TEST}// edit {i}\n")).unwrap();
        build(&sb, &app, &[]);
    }
    build(&sb, &app, rx);
    build(&sb, &app, x);
    let target = sb.central_target(&app);
    let dirs = Dirs::at(sb.home());
    let release_out = stdout(&run_binary(&target, "release", "app"));

    let before = allocated(&target);
    let report = compact::auto_compact_with(&dirs, &target, &eager());
    assert!(report.skipped.is_none(), "{:?}", report.skipped);
    assert!(report.dead_units >= 1, "the feature-less app variant is superseded: {report:?}");
    assert!(report.allocated_after < before, "auto compaction shrinks the directory: {before} -> {}", report.allocated_after);
    assert!(report.errors.is_empty(), "{:?}", report.errors);

    for args in [x, rx] {
        let again = build(&sb, &app, args);
        assert!(compiled(&again).is_empty(), "auto compaction made something stale: {:?}", compiled(&again));
    }
    let test = sb.bin("fcargo").args(["test", "--features", "x"]).current_dir(&app).output().unwrap();
    assert_success(&test);
    let run = sb.bin("fcargo").args(["run", "--features", "x"]).current_dir(&app).output().unwrap();
    assert_success(&run);
    assert!(stdout(&run).starts_with("x "), "{}", stdout(&run));

    fs::write(app.join("src").join("main.rs"), format!("{MAIN_WITH_TEST}// after compaction\n")).unwrap();
    let edit = build(&sb, &app, x);
    let rebuilt = compiled(&edit);
    assert!(!rebuilt.is_empty() && rebuilt.iter().all(|l| l.contains("app")), "{rebuilt:?}");
    let sessions = fs::read_dir(target.join("debug").join("incremental")).unwrap().count();
    assert!(sessions > 0, "the incremental cache of the app survives");

    // A release profile unbuilt past the keep limit is removed whole and comes back from the compile cache.
    let mut demote = eager();
    demote.current_window = Duration::ZERO;
    demote.keep_other_profile = Duration::ZERO;
    let report = compact::auto_compact_with(&dirs, &target, &demote);
    assert!(report.demoted_profiles.iter().any(|p| p.ends_with("release")), "{report:?}");
    assert!(!target.join("release").join(common::exe("app")).exists());
    let hits = sb.count_cache("hit", "leaf");
    build(&sb, &app, rx);
    assert!(sb.count_cache("hit", "leaf") > hits, "release dependencies are restored from the cache");
    assert_eq!(stdout(&run_binary(&target, "release", "app")), release_out);
    assert!(compiled(&build(&sb, &app, x)).is_empty(), "debug stayed warm throughout");
}

fn artifact(dir: &Path, prefix: &str, suffix: &str) -> PathBuf {
    let found: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(prefix) && n.to_string_lossy().ends_with(suffix)))
        .collect();
    assert_eq!(found.len(), 1, "{prefix}*{suffix} in {}: {found:?}", dir.display());
    found.into_iter().next().unwrap()
}

/// A new inode on a proc-macro dylib makes macOS recheck its signature (about 100 ms), so fresh hot files wait to settle.
#[test]
fn auto_compaction_leaves_fresh_proc_macros_in_place_and_compresses_libraries_at_once() {
    let ino = |p: &Path| frust::sys::stat(p).unwrap().ino();
    let sb = Sandbox::new("auto-hot");
    let app = project(&sb);
    let mac = app.join("mac");
    fs::create_dir_all(mac.join("src")).unwrap();
    fs::write(mac.join("Cargo.toml"), "[package]\nname = \"mac\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\nproc-macro = true\n").unwrap();
    fs::write(mac.join("src").join("lib.rs"), "use proc_macro::TokenStream;\n#[proc_macro]\npub fn answer(_: TokenStream) -> TokenStream { \"42u64\".parse().unwrap() }\n").unwrap();
    let manifest = fs::read_to_string(app.join("Cargo.toml")).unwrap().replace("[dependencies]\n", "[dependencies]\nmac = { path = \"mac\" }\n");
    fs::write(app.join("Cargo.toml"), manifest).unwrap();
    fs::write(app.join("src").join("main.rs"), MAIN.replace("leaf::all(1)", "leaf::all(mac::answer!())")).unwrap();

    let out = sb.bin("fcargo").args(["build", "-v"]).env("FRUST_AUTO_COMPACT", "off").current_dir(&app).output().unwrap();
    assert_success(&out);
    let target = sb.central_target(&app);
    let dirs = Dirs::at(sb.home());
    let deps = target.join("debug").join("deps");
    let rlib = artifact(&deps, "libleaf-", ".rlib");
    let dylib = if cfg!(windows) {
        artifact(&deps, "mac-", ".dll")
    } else {
        artifact(&deps, "libmac-", if cfg!(target_os = "macos") { ".dylib" } else { ".so" })
    };
    let (rlib_inode, dylib_inode) = (ino(&rlib), ino(&dylib));

    let report = compact::auto_compact_with(&dirs, &target, &eager());
    assert!(report.skipped.is_none(), "{:?}", report.skipped);
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(ino(&dylib), dylib_inode, "a fresh proc-macro dylib was rewritten: {report:?}");
    if report.supported {
        assert_ne!(ino(&rlib), rlib_inode, "rlibs are compressed or deduplicated at once: {report:?}");
    }
    assert!(compiled(&build(&sb, &app, &[])).is_empty());

    // Once settled the dylib is compacted like any other idle file.
    let mut settled = eager();
    settled.hot_window = Duration::ZERO;
    let report = compact::auto_compact_with(&dirs, &target, &settled);
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    if report.supported {
        assert_ne!(ino(&dylib), dylib_inode, "settled outputs are compressed or deduplicated: {report:?}");
    }
    assert!(compiled(&build(&sb, &app, &[])).is_empty(), "compaction made something stale");
    let run = run_binary(&target, "debug", "app");
    assert_success(&run);
}

#[test]
fn auto_compaction_never_disturbs_a_build_and_is_cheap_when_not_due() {
    let sb = Sandbox::new("auto-busy");
    let app = project(&sb);
    build(&sb, &app, &[]);
    build(&sb, &app, &["--features", "x"]);
    build(&sb, &app, &["--release", "--features", "x"]);
    let target = sb.central_target(&app);
    let dirs = Dirs::at(sb.home());
    let debug = target.join("debug");
    let before = hashes_of(&debug.join(".fingerprint"), "app-");
    let mut opts = eager();
    opts.min_growth = u64::MAX;
    opts.compress = false;

    if target.join(compact::AUTO_MARKER).exists() {
        let mut marker = auto_marker(&target);
        marker["at_unix"] = 0.into();
        write_auto_marker(&target, &marker);
    }
    let lock = File::options().read(true).write(true).open(debug.join(".cargo-lock")).unwrap();
    frust::sys::lock(&lock, frust::sys::LockKind::Exclusive).unwrap();
    let report = compact::auto_compact_with(&dirs, &target, &opts);
    assert!(report.busy.iter().any(|p| p == &debug), "{report:?}");
    assert_eq!(report.profiles, 1, "the unlocked release profile is still processed: {report:?}");
    assert_eq!(auto_marker(&target)["complete"], false, "busy profiles leave the pass incomplete");
    assert_eq!(hashes_of(&debug.join(".fingerprint"), "app-"), before);
    let signature = auto_marker(&target)["signature"].clone();
    let retry = compact::auto_compact_with(&dirs, &target, &opts);
    assert!(retry.skipped.is_none() && retry.busy.contains(&debug), "an unchanged partial pass must retry: {retry:?}");
    assert_eq!(auto_marker(&target)["signature"], signature, "the tree did not change between partial passes");
    assert_eq!(auto_marker(&target)["complete"], false);
    drop(lock);

    let retry = compact::auto_compact_with(&dirs, &target, &opts);
    assert!(retry.skipped.is_none() && retry.busy.is_empty() && retry.errors.is_empty(), "{retry:?}");
    assert_eq!(retry.profiles, 2, "the previously busy profile is now processed");
    #[cfg(unix)]
    assert!(retry.dead_units > 0, "the previously locked profile is cleaned: {retry:?}");
    assert_eq!(auto_marker(&target)["complete"], true);
    assert!(compiled(&build(&sb, &app, &["--features", "x"])).is_empty(), "retry preserves cargo freshness");
    assert!(compiled(&build(&sb, &app, &["--release", "--features", "x"])).is_empty());

    // Another pass in progress (holding the marker): this one does nothing.
    let marker = File::options().read(true).write(true).create(true).truncate(false).open(target.join(compact::AUTO_MARKER)).unwrap();
    frust::sys::lock(&marker, frust::sys::LockKind::Exclusive).unwrap();
    let inherited_marker = marker.try_clone().unwrap();
    let report = compact::auto_compact_with(&dirs, &target, &eager());
    assert_eq!(report.skipped, Some("another compaction is running"));
    frust::sys::unlock(&marker).unwrap();
    drop(marker);

    // Default interval: right after a pass, the next is not due.
    let report = compact::auto_compact_with(&dirs, &target, &eager());
    assert!(report.skipped.is_none(), "{report:?}");
    drop(inherited_marker);
    let mut defaults = eager();
    defaults.every = Duration::from_secs(600);
    assert_eq!(compact::auto_compact_with(&dirs, &target, &defaults).skipped, Some("not due"));

    let mut off = eager();
    off.enabled = false;
    assert!(compact::auto_compact_with(&dirs, &target, &off).skipped.is_some());
    assert!(compiled(&build(&sb, &app, &["--features", "x"])).is_empty());
}
