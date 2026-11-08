//! Dependency debuginfo tuning is on by default, applies to dev-like profiles, and yields to user profiles and `FRUST_TUNE=off`.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

use common::{Sandbox, assert_success, stderr};

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

#[test]
fn a_user_profile_blocks_the_default_tuning() {
    let sb = Sandbox::new("tuning-profile");
    let ws = project(&sb, "\n[profile.dev]\nopt-level = 1\n");
    let out = fcargo(&sb, &ws, &["build", "-v"], None);
    assert_success(&out);
    assert!(rustc_line(&stderr(&out), "outdep").contains("debuginfo=2"));
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
