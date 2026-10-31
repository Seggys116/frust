//! `frust self-update` against a local fake release served through `file://` URLs; nothing touches the network.

#![cfg(unix)]

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use common::{Sandbox, stderr, stdout};

const BINARIES: [&str; 8] = ["frust", "fcargo", "fclippy", "frustc", "frustdoc", "frustup", "frustfmt", "frust-wrap"];

fn fake_tag() -> String {
    let major: u64 = env!("CARGO_PKG_VERSION").split('.').next().unwrap().parse().unwrap();
    format!("v{}.0.0", major + 1)
}

fn fake_version() -> String {
    fake_tag().trim_start_matches('v').to_string()
}

struct Release {
    api_url: String,
    base_url: String,
    latest: PathBuf,
    checksum: PathBuf,
}

fn write_script(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn make_release(sb: &Sandbox, tag: &str) -> Release {
    let target = frust::update::current_target().expect("a platform with prebuilt releases");
    let archive_name = frust::update::archive_name(target);
    let root = sb.root.join("release");
    let stage = root.join("stage");
    fs::create_dir_all(&stage).unwrap();
    let version = tag.trim_start_matches('v');
    for name in BINARIES {
        write_script(&stage.join(name), &format!("echo \"frust {version}\""));
    }
    fs::write(stage.join("LICENSE"), "license\n").unwrap();
    fs::write(stage.join("README.md"), "readme\n").unwrap();

    let dist = root.join("download").join(tag);
    fs::create_dir_all(&dist).unwrap();
    let archive = dist.join(&archive_name);
    let status = Command::new("tar")
        .env("COPYFILE_DISABLE", "1")
        .arg("-czf")
        .arg(&archive)
        .arg("-C")
        .arg(&stage)
        .args(BINARIES)
        .args(["LICENSE", "README.md"])
        .status()
        .unwrap();
    assert!(status.success(), "tar could not build the fake release");
    let checksum = dist.join(format!("{archive_name}.sha256"));
    let digest = frust::update::sha256_file(&archive).unwrap();
    fs::write(&checksum, format!("{digest}  {archive_name}\n")).unwrap();

    let api = root.join("api").join("releases");
    fs::create_dir_all(api.join("tags")).unwrap();
    let body = format!("{{\"tag_name\":\"{tag}\"}}");
    fs::write(api.join("latest"), &body).unwrap();
    fs::write(api.join("tags").join(tag), &body).unwrap();
    Release {
        api_url: format!("file://{}", api.join("latest").display()),
        base_url: format!("file://{}", root.join("download").display()),
        latest: api.join("latest"),
        checksum,
    }
}

// A copy of the real frust to run, plus stand-ins for the other seven, so the build output is never touched.
fn make_install(sb: &Sandbox) -> PathBuf {
    let dir = sb.root.join("install");
    fs::create_dir_all(&dir).unwrap();
    for name in BINARIES {
        if name == "frust" {
            fs::copy(env!("CARGO_BIN_EXE_frust"), dir.join(name)).unwrap();
        } else {
            write_script(&dir.join(name), &format!("echo old-{name}"));
        }
    }
    dir
}

fn snapshot(install: &Path) -> Vec<Vec<u8>> {
    BINARIES.iter().map(|name| fs::read(install.join(name)).unwrap()).collect()
}

fn assert_no_leftovers(install: &Path) {
    for entry in fs::read_dir(install).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        assert!(!name.starts_with(".frust-update-") && !name.ends_with(".new"), "leftover {name}");
    }
}

fn run_update(sb: &Sandbox, install: &Path, release: &Release, args: &[&str]) -> Output {
    let exe = install.join("frust");
    let mut cmd = sb.bin_path(exe.to_str().unwrap());
    cmd.arg("self-update")
        .args(args)
        .env("FRUST_UPDATE_API_URL", &release.api_url)
        .env("FRUST_UPDATE_DOWNLOAD_BASE", &release.base_url)
        .stdin(Stdio::null());
    cmd.output().unwrap()
}

fn assert_replaced(install: &Path) {
    for name in BINARIES {
        let text = fs::read_to_string(install.join(name)).unwrap();
        assert!(text.starts_with("#!/bin/sh") && text.contains(&format!("frust {}", fake_version())), "{name} was not replaced: {text:?}");
        let mode = fs::metadata(install.join(name)).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "{name} must be executable");
    }
    assert_no_leftovers(install);
}

#[test]
fn check_reports_an_available_update_with_exit_ten() {
    let sb = Sandbox::new("update-check");
    let release = make_release(&sb, &fake_tag());
    let install = make_install(&sb);
    let before = snapshot(&install);
    let out = run_update(&sb, &install, &release, &["--check"]);
    assert_eq!(out.status.code(), Some(10), "{}{}", stdout(&out), stderr(&out));
    assert!(stdout(&out).contains(&format!("frust {} is available", fake_version())), "{}", stdout(&out));
    assert_eq!(snapshot(&install), before, "--check must not install anything");
    let cached: serde_json::Value = serde_json::from_slice(&fs::read(sb.home().join("update").join("latest.json")).unwrap()).unwrap();
    assert_eq!(cached["tag"], fake_tag());
}

#[test]
fn an_up_to_date_install_exits_zero() {
    let sb = Sandbox::new("update-current");
    let release = make_release(&sb, &format!("v{}", env!("CARGO_PKG_VERSION")));
    let install = make_install(&sb);
    for args in [&["--check"][..], &["--yes"][..]] {
        let out = run_update(&sb, &install, &release, args);
        assert_eq!(out.status.code(), Some(0), "{}{}", stdout(&out), stderr(&out));
        assert!(stdout(&out).contains(&format!("already up to date ({})", env!("CARGO_PKG_VERSION"))), "{}", stdout(&out));
    }
}

#[test]
fn a_successful_update_replaces_all_eight_binaries() {
    let sb = Sandbox::new("update-apply");
    let release = make_release(&sb, &fake_tag());
    let install = make_install(&sb);
    let out = run_update(&sb, &install, &release, &["--yes"]);
    assert_eq!(out.status.code(), Some(0), "{}{}", stdout(&out), stderr(&out));
    assert!(stdout(&out).contains("frust doctor"), "{}", stdout(&out));
    assert_replaced(&install);
    let version = Command::new(install.join("frust")).arg("--version").output().unwrap();
    assert!(stdout(&version).contains(&format!("frust {}", fake_version())));
}

#[test]
fn an_explicit_version_installs_that_release() {
    let sb = Sandbox::new("update-explicit");
    let release = make_release(&sb, &fake_tag());
    let install = make_install(&sb);
    let version = fake_version();
    let out = run_update(&sb, &install, &release, &["--version", version.as_str(), "--yes"]);
    assert_eq!(out.status.code(), Some(0), "{}{}", stdout(&out), stderr(&out));
    assert_replaced(&install);
}

#[test]
fn a_corrupted_checksum_leaves_the_installation_untouched() {
    let sb = Sandbox::new("update-badsum");
    let release = make_release(&sb, &fake_tag());
    let install = make_install(&sb);
    let name = fs::read_to_string(&release.checksum).unwrap().split_whitespace().nth(1).unwrap().to_string();
    fs::write(&release.checksum, format!("{}  {name}\n", "0".repeat(64))).unwrap();
    let before = snapshot(&install);
    let out = run_update(&sb, &install, &release, &["--yes"]);
    assert_eq!(out.status.code(), Some(1), "{}{}", stdout(&out), stderr(&out));
    assert!(stderr(&out).contains("checksum mismatch"), "{}", stderr(&out));
    assert_eq!(snapshot(&install), before);
    assert_no_leftovers(&install);
}

#[test]
fn a_checksum_file_for_another_archive_is_refused() {
    let sb = Sandbox::new("update-wrongname");
    let release = make_release(&sb, &fake_tag());
    let install = make_install(&sb);
    let digest = fs::read_to_string(&release.checksum).unwrap().split_whitespace().next().unwrap().to_string();
    fs::write(&release.checksum, format!("{digest}  frust-something-else.tar.gz\n")).unwrap();
    let before = snapshot(&install);
    let out = run_update(&sb, &install, &release, &["--yes"]);
    assert_eq!(out.status.code(), Some(1), "{}{}", stdout(&out), stderr(&out));
    assert!(stderr(&out).contains("checksum file is for"), "{}", stderr(&out));
    assert_eq!(snapshot(&install), before);
    assert_no_leftovers(&install);
}

#[test]
fn tags_that_could_escape_a_path_are_rejected() {
    let sb = Sandbox::new("update-traversal");
    let release = make_release(&sb, &fake_tag());
    let install = make_install(&sb);
    let before = snapshot(&install);

    fs::write(&release.latest, r#"{"tag_name":"v9.0.0/../../evil"}"#).unwrap();
    let out = run_update(&sb, &install, &release, &["--yes"]);
    assert_eq!(out.status.code(), Some(1), "{}{}", stdout(&out), stderr(&out));
    assert!(stderr(&out).contains("invalid release tag"), "{}", stderr(&out));

    let out = run_update(&sb, &install, &release, &["--version", "../../evil", "--yes"]);
    assert_eq!(out.status.code(), Some(2), "{}{}", stdout(&out), stderr(&out));
    assert!(stderr(&out).contains("invalid release tag"), "{}", stderr(&out));

    assert_eq!(snapshot(&install), before);
    assert_no_leftovers(&install);
}

#[test]
fn without_a_terminal_and_without_yes_the_update_is_refused() {
    let sb = Sandbox::new("update-noyes");
    let release = make_release(&sb, &fake_tag());
    let install = make_install(&sb);
    let before = snapshot(&install);
    let out = run_update(&sb, &install, &release, &[]);
    assert_eq!(out.status.code(), Some(2), "{}{}", stdout(&out), stderr(&out));
    assert!(stderr(&out).contains("--yes"), "{}", stderr(&out));
    assert_eq!(snapshot(&install), before);
    assert_no_leftovers(&install);
}

#[test]
fn disabling_writes_a_marker_and_enabling_removes_it() {
    let sb = Sandbox::new("update-disable");
    let marker = sb.home().join("update").join("disabled");
    let out = sb.bin("frust").args(["self-update", "--disable"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(marker.is_file());

    // A newer release is already known and fresh, so a terminal build would announce it if checks were on.
    let seeded = format!("{{\"tag\":\"v99.0.0\",\"checked_at\":{}}}", frust::fsx::unix_now());
    fs::write(sb.home().join("update").join("latest.json"), seeded).unwrap();
    let project = sb.project("quiet", &[], "fn main() {}\n");
    let build = sb.bin("frust").arg("build").current_dir(&project).output().unwrap();
    assert!(build.status.success(), "{}", stderr(&build));
    assert!(!stderr(&build).contains("self-update"), "{}", stderr(&build));

    let out = sb.bin("frust").args(["self-update", "--enable"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(!marker.exists());
}

#[test]
fn builds_without_a_terminal_print_no_notice_and_start_no_check() {
    let sb = Sandbox::new("update-quiet");
    let project = sb.project("quiet", &[], "fn main() {}\n");
    let build = sb.bin("frust").arg("build").current_dir(&project).output().unwrap();
    assert!(build.status.success(), "{}", stderr(&build));
    assert!(!stderr(&build).contains("is available"), "{}", stderr(&build));
    assert!(!sb.home().join("update").join("latest.json").exists(), "no check may run when stderr is not a terminal");
}

#[test]
fn the_background_check_records_the_latest_tag_silently() {
    let sb = Sandbox::new("update-background");
    let release = make_release(&sb, &fake_tag());
    let out = sb
        .bin("frust")
        .args(["self-update", "--check-background"])
        .env("FRUST_UPDATE_API_URL", &release.api_url)
        .env("FRUST_UPDATE_DOWNLOAD_BASE", &release.base_url)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty() && out.stderr.is_empty(), "{}{}", stdout(&out), stderr(&out));
    let cached: serde_json::Value = serde_json::from_slice(&fs::read(sb.home().join("update").join("latest.json")).unwrap()).unwrap();
    assert_eq!(cached["tag"], fake_tag());
    assert!(cached["checked_at"].as_u64().unwrap() > 0);
}

#[test]
fn a_failed_background_check_backs_off_without_output() {
    let sb = Sandbox::new("update-background-fail");
    let missing = format!("file://{}", sb.root.join("nowhere").join("latest").display());
    let out = sb
        .bin("frust")
        .args(["self-update", "--check-background"])
        .env("FRUST_UPDATE_API_URL", missing)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty() && out.stderr.is_empty(), "{}{}", stdout(&out), stderr(&out));
    let cached: serde_json::Value = serde_json::from_slice(&fs::read(sb.home().join("update").join("latest.json")).unwrap()).unwrap();
    assert!(cached["tag"].is_null());
    assert!(cached["checked_at"].as_u64().unwrap() > 0, "the failed attempt must still be recorded so it is not retried at once");
}

#[test]
fn unknown_options_are_a_usage_error() {
    let sb = Sandbox::new("update-usage");
    let out = sb.bin("frust").args(["self-update", "--bogus"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("unknown option"), "{}", stderr(&out));
}
