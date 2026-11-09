//! Dev tuning (dependency debuginfo, incremental codegen units) is on by default, applies to dev-like profiles, and yields to user settings and `FRUST_TUNE=off`.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

use common::{Sandbox, assert_success, stderr, stdout};

fn project(sb: &Sandbox, manifest_extra: &str) -> PathBuf {
    let dep = sb.root.join("outdep");
    fs::create_dir_all(dep.join("src")).unwrap();
    fs::write(dep.join("Cargo.toml"), "[package]\nname = \"outdep\"\nversion = \"0.1.0\"\nedition = \"2021\"\n").unwrap();
    fs::write(dep.join("src/lib.rs"), "pub fn f() -> u32 { 1 }\n").unwrap();
    let ws = sb.root.join("wsmain");
    fs::create_dir_all(ws.join("src")).unwrap();
    let manifest = format!("[package]\nname = \"wsmain\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n\n[dependencies]\noutdep = {{ path = \"../outdep\" }}\n{manifest_extra}");
    fs::write(ws.join("Cargo.toml"), manifest).unwrap();
    fs::write(ws.join("src/main.rs"), "fn main() { println!(\"{}\", outdep::f()); }\n#[test]\nfn t() { assert_eq!(outdep::f(), 1); }\n").unwrap();
    ws
}

fn fcargo(sb: &Sandbox, ws: &Path, args: &[&str], tune: Option<&str>) -> Output {
    let mut cmd = sb.bin("fcargo");
    cmd.args(args).current_dir(ws);
    match tune {
        Some(value) => cmd.env("FRUST_TUNE", value),
        None => cmd.env_remove("FRUST_TUNE"),
    };
    cmd.output().unwrap()
}

fn rustc_line<'a>(verbose_stderr: &'a str, krate: &str) -> &'a str {
    let needle = format!("--crate-name {krate} ");
    verbose_stderr.lines().find(|l| l.contains("Running") && l.contains(&needle)).unwrap_or_else(|| panic!("no rustc invocation for {krate} in:\n{verbose_stderr}"))
}

#[test]
fn dependencies_lose_debuginfo_by_default_and_the_workspace_keeps_it() {
    let sb = Sandbox::new("tuning-default");
    let ws = project(&sb, "");
    let out = fcargo(&sb, &ws, &["build", "-v"], None);
    assert_success(&out);
    let err = stderr(&out);
    assert!(!rustc_line(&err, "outdep").contains("debuginfo=2"), "{}", rustc_line(&err, "outdep"));
    assert!(rustc_line(&err, "wsmain").contains("debuginfo=2"), "{}", rustc_line(&err, "wsmain"));
}

#[test]
fn the_test_profile_inherits_the_dependency_tuning() {
    let sb = Sandbox::new("tuning-test-profile");
    let ws = project(&sb, "");
    let out = fcargo(&sb, &ws, &["test", "--no-run", "-v"], None);
    assert_success(&out);
    let err = stderr(&out);
    assert!(!rustc_line(&err, "outdep").contains("debuginfo=2"), "{}", rustc_line(&err, "outdep"));
    assert!(rustc_line(&err, "wsmain").contains("debuginfo=2"), "{}", rustc_line(&err, "wsmain"));
}

#[test]
fn tune_off_keeps_full_dependency_debuginfo() {
    let sb = Sandbox::new("tuning-off");
    let ws = project(&sb, "");
    let out = fcargo(&sb, &ws, &["build", "-v"], Some("off"));
    assert_success(&out);
    assert!(rustc_line(&stderr(&out), "outdep").contains("debuginfo=2"));
}

#[test]
fn release_builds_are_not_tuned() {
    let sb = Sandbox::new("tuning-release");
    let ws = project(&sb, "");
    let out = fcargo(&sb, &ws, &["build", "--release", "-v"], None);
    assert_success(&out);
    assert!(!stderr(&out).contains("debug=0"));
}

fn outdep_debuginfo(extra: &str, args: &[&str], name: &str) -> bool {
    let sb = Sandbox::new(name);
    let ws = project(&sb, extra);
    let out = fcargo(&sb, &ws, args, None);
    assert_success(&out);
    rustc_line(&stderr(&out), "outdep").contains("debuginfo=")
}

#[test]
fn a_user_dev_debug_setting_blocks_the_default_tuning() {
    assert!(outdep_debuginfo("\n[profile.dev]\ndebug = 1\n", &["build", "-v"], "tuning-user-debug"));
}

#[test]
fn a_release_only_profile_still_tunes_dev_builds() {
    assert!(!outdep_debuginfo("\n[profile.release]\nlto = true\nstrip = true\n", &["build", "-v"], "tuning-release-profile"));
}

#[test]
fn dev_settings_unrelated_to_debuginfo_still_tune() {
    assert!(!outdep_debuginfo("\n[profile.dev]\nopt-level = 1\n", &["build", "-v"], "tuning-dev-opt"));
}

#[test]
fn package_overrides_block_only_when_they_set_debug_keys() {
    assert!(!outdep_debuginfo("\n[profile.dev.package.foo]\nopt-level = 3\n", &["build", "-v"], "tuning-pkg-opt"));
    assert!(outdep_debuginfo("\n[profile.dev.package.\"*\"]\ndebug = 2\n", &["build", "-v"], "tuning-pkg-debug"));
}

#[test]
fn a_test_profile_debug_setting_blocks_only_test_builds() {
    let extra = "\n[profile.test]\ndebug = 1\n";
    assert!(!outdep_debuginfo(extra, &["build", "-v"], "tuning-test-profile-build"));
    assert!(outdep_debuginfo(extra, &["test", "--no-run", "-v"], "tuning-test-profile-test"));
}

#[test]
fn a_cli_config_debug_setting_blocks_the_default_tuning() {
    assert!(outdep_debuginfo("", &["build", "-v", "--config", "profile.dev.debug=1"], "tuning-cli-debug"));
}

#[test]
fn tuned_and_untuned_builds_share_one_cache_without_mixing_entries() {
    let sb = Sandbox::new("tuning-cache");
    let ws = project(&sb, "");
    let tuned = fcargo(&sb, &ws, &["build", "-v"], Some("on"));
    assert_success(&tuned);
    assert!(!rustc_line(&stderr(&tuned), "outdep").contains("debuginfo=2"));
    let untuned = fcargo(&sb, &ws, &["build", "-v"], Some("off"));
    assert_success(&untuned);
    let err = stderr(&untuned);
    let line = rustc_line(&err, "outdep");
    assert!(line.contains("debuginfo=2"), "{line}");
    let again = fcargo(&sb, &ws, &["build", "-v"], Some("on"));
    assert_success(&again);
    assert!(!stderr(&again).lines().any(|l| l.contains("Running") && l.contains("--crate-name outdep ") && l.contains("debuginfo=2")));
}

/// Objects in the newest incremental session of `krate`: one per codegen unit rustc emitted.
fn session_objects(sb: &Sandbox, ws: &Path, krate: &str) -> usize {
    let incremental = sb.central_target(ws).join("debug").join("incremental");
    let crate_dir = fs::read_dir(&incremental).unwrap().flatten().map(|e| e.path()).find(|p| p.file_name().unwrap().to_string_lossy().starts_with(&format!("{krate}-"))).unwrap();
    let session = fs::read_dir(&crate_dir).unwrap().flatten().map(|e| e.path()).filter(|p| p.is_dir()).max_by_key(|p| fs::metadata(p).unwrap().modified().unwrap()).unwrap();
    fs::read_dir(session).unwrap().flatten().filter(|e| e.path().extension().is_some_and(|x| x == "o" || x == "obj")).count()
}

/// A workspace binary of 80 modules, which rustc's default of 256 incremental units keeps as about 80 objects.
fn many_modules(sb: &Sandbox) -> PathBuf {
    let ws = project(sb, "");
    let mut main = String::new();
    for i in 0..80 {
        main.push_str(&format!("mod m{i} {{ pub fn f() -> u32 {{ {i} }} }}\n"));
    }
    main.push_str("fn main() {\n    let mut sum = outdep::f();\n");
    for i in 0..80 {
        main.push_str(&format!("    sum += m{i}::f();\n"));
    }
    main.push_str("    println!(\"{sum}\");\n}\n");
    fs::write(ws.join("src/main.rs"), main).unwrap();
    ws
}

#[test]
fn incremental_dev_compiles_use_64_codegen_units() {
    let sb = Sandbox::new("tuning-units");
    let ws = many_modules(&sb);
    let out = fcargo(&sb, &ws, &["build"], None);
    assert_success(&out);
    let objects = session_objects(&sb, &ws, "wsmain");
    assert!((1..=64).contains(&objects), "{objects} objects");
    let run = common::run_binary(&sb.central_target(&ws), "debug", "wsmain");
    assert_eq!(stdout(&run).trim(), "3161");
}

#[test]
fn tune_off_and_explicit_units_keep_rustc_default_units() {
    let sb = Sandbox::new("tuning-units-off");
    let ws = many_modules(&sb);
    let out = fcargo(&sb, &ws, &["build"], Some("off"));
    assert_success(&out);
    assert!(session_objects(&sb, &ws, "wsmain") > 64);
    let sb = Sandbox::new("tuning-units-explicit");
    let ws = many_modules(&sb);
    let out = fcargo(&sb, &ws, &["build", "--config", "profile.dev.codegen-units=128"], None);
    assert_success(&out);
    assert!(session_objects(&sb, &ws, "wsmain") > 64);
}

#[test]
fn tuning_prints_nothing_new() {
    let sb = Sandbox::new("tuning-quiet");
    let ws = project(&sb, "");
    let tuned = fcargo(&sb, &ws, &["build"], None);
    assert_success(&tuned);
    let sb_off = Sandbox::new("tuning-quiet-off");
    let ws_off = project(&sb_off, "");
    let untuned = fcargo(&sb_off, &ws_off, &["build"], Some("off"));
    assert_success(&untuned);
    let normalize = |out: &Output| stderr(out).lines().filter(|l| !l.contains("Finished") && !l.contains("Compiling")).map(str::to_string).collect::<Vec<_>>();
    assert_eq!(normalize(&tuned), normalize(&untuned));
}
