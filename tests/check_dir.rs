//! `check` and `clippy` build in their own directory beside the build target, so cargo's per-profile lock never makes them wait on a build.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

use common::{Sandbox, assert_success, stdout};

const LIB: &str = "pub fn early() -> u32 { 1 }\n";
const MAIN: &str = "fn main() { println!(\"{}\", early::early()); }\n";

fn scene(tag: &str) -> (Sandbox, PathBuf) {
    let sb = Sandbox::new(tag);
    sb.vendor_crate("early", "1.0.0", LIB);
    let project = sb.project("app", &[("early", "1.0.0")], MAIN);
    (sb, project)
}

fn fcargo(sb: &Sandbox, project: &Path, args: &[&str]) -> Output {
    sb.bin("fcargo").args(args).current_dir(project).output().unwrap()
}

fn deps_files(target: &Path, prefix: &str, extension: &str) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(target.join("debug").join("deps")) else { return Vec::new() };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let name = p.file_name().unwrap().to_string_lossy();
            name.starts_with(prefix) && p.extension().is_some_and(|e| e == extension)
        })
        .collect()
}

#[test]
fn check_builds_in_a_sibling_directory_and_build_keeps_its_own() {
    let (sb, project) = scene("checkdir-layout");
    let build = sb.central_target(&project);
    let check = sb.check_target(&project);
    assert_eq!(check.parent(), build.parent());
    assert_eq!(check.file_name().unwrap().to_string_lossy(), format!("{}-check", build.file_name().unwrap().to_string_lossy()));

    assert_success(&fcargo(&sb, &project, &["check"]));
    assert!(!deps_files(&check, "libearly-", "rmeta").is_empty(), "check artifacts live in the check directory");
    assert!(!build.exists(), "a check never touches the build directory");

    assert_success(&fcargo(&sb, &project, &["build"]));
    assert!(build.join("debug").join(common::exe("app")).is_file());
    assert!(!check.join("debug").join(common::exe("app")).exists());
    assert_eq!(stdout(&common::run_binary(&build, "debug", "app")).trim(), "1");
}

#[test]
fn check_artifacts_equal_plain_cargo_check() {
    let (sb, project) = scene("checkdir-oracle");
    let out = sb.bin("fcargo").arg("check").current_dir(&project).env("FRUST_TUNE", "off").env("FRUST_CACHE", "off").output().unwrap();
    assert_success(&out);
    let oracle = sb.root.join("oracle-target");
    let plain = sb.cargo().arg("check").current_dir(&project).env("CARGO_TARGET_DIR", &oracle).output().unwrap();
    assert_success(&plain);

    let ours = deps_files(&sb.check_target(&project), "libearly-", "rmeta");
    let theirs = deps_files(&oracle, "libearly-", "rmeta");
    assert_eq!((ours.len(), theirs.len()), (1, 1));
    assert_eq!(ours[0].file_name(), theirs[0].file_name());
    assert_eq!(fs::read(&ours[0]).unwrap(), fs::read(&theirs[0]).unwrap(), "the rmeta bytes equal plain cargo's");
}

#[test]
fn clippy_uses_the_check_directory() {
    let (sb, project) = scene("checkdir-clippy");
    assert_success(&fcargo(&sb, &project, &["clippy"]));
    assert!(!deps_files(&sb.check_target(&project), "libearly-", "rmeta").is_empty());
    assert!(!sb.central_target(&project).exists());
}

#[test]
fn clean_removes_both_directories_and_a_partial_clean_reaches_the_check_directory() {
    let (sb, project) = scene("checkdir-clean");
    let (build, check) = (sb.central_target(&project), sb.check_target(&project));
    assert_success(&fcargo(&sb, &project, &["build"]));
    assert_success(&fcargo(&sb, &project, &["check"]));
    assert!(build.is_dir() && check.is_dir());

    assert_success(&fcargo(&sb, &project, &["clean", "-p", "early"]));
    assert!(deps_files(&check, "libearly-", "rmeta").is_empty(), "a partial clean applies to the check directory");
    assert!(deps_files(&build, "libearly-", "rlib").is_empty());
    assert!(check.is_dir() && build.is_dir());

    assert_success(&fcargo(&sb, &project, &["clean"]));
    assert!(!build.exists() && !check.exists(), "a whole clean removes both directories");
    assert!(!build.parent().unwrap().exists(), "and the emptied workspace directory");
}

#[test]
fn the_check_directory_can_be_turned_off() {
    let (sb, project) = scene("checkdir-off");
    let out = sb.bin("fcargo").arg("check").current_dir(&project).env("FRUST_CHECK_DIR", "off").output().unwrap();
    assert_success(&out);
    assert!(!deps_files(&sb.central_target(&project), "libearly-", "rmeta").is_empty(), "check shares the build directory");
    assert!(!sb.check_target(&project).exists());
}

#[test]
fn a_user_set_target_directory_is_respected() {
    let (sb, project) = scene("checkdir-user");
    let mine = sb.root.join("mine");
    let out = sb.bin("fcargo").arg("check").current_dir(&project).env("CARGO_TARGET_DIR", &mine).output().unwrap();
    assert_success(&out);
    assert!(!deps_files(&mine, "libearly-", "rmeta").is_empty());
    assert!(!sb.central_target(&project).exists() && !sb.check_target(&project).exists());
}

#[test]
fn stats_and_doctor_name_the_check_directory() {
    let (sb, project) = scene("checkdir-report");
    assert_success(&fcargo(&sb, &project, &["check"]));
    let stats = sb.bin("frust").arg("stats").current_dir(&project).output().unwrap();
    assert_success(&stats);
    assert!(stdout(&stats).contains("-check idle"), "{}", stdout(&stats));
    let doctor = sb.bin("frust").arg("doctor").current_dir(&project).output().unwrap();
    assert!(stdout(&doctor).contains("check directory"), "{}", stdout(&doctor));
    let off = sb.bin("frust").arg("doctor").current_dir(&project).env("FRUST_CHECK_DIR", "off").output().unwrap();
    assert!(stdout(&off).contains("FRUST_CHECK_DIR=off"), "{}", stdout(&off));
}
