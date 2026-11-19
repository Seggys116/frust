//! `frust-install` downloading, updating and rolling back, against a release directory on disk.

use std::env::consts::OS;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

use frust_cli::release;
use frust_core::{hex32, sha256};

struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("frust-release-{label}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn assets() -> release::Assets {
    release::assets(release::host_target().expect("tests run on a supported host"))
}

/// Writes one release into `dir`: a toy toolchain archive, its checksum and its id file.
fn publish(dir: &Path, id: &str, marker: &str) {
    let names = assets();
    let tree = dir.join(format!("tree-{id}"));
    let _ = fs::remove_dir_all(&tree);
    fs::create_dir_all(tree.join("bin")).unwrap();
    for tool in ["rustc", "cargo", "rustdoc"] {
        fs::write(tree.join("bin").join(tool), marker).unwrap();
    }
    fs::write(tree.join("manifest.json"), format!("{{\"id\":\"{id}\"}}\n")).unwrap();
    let archive = dir.join(&names.archive);
    let status = Command::new("tar").args(["-czf"]).arg(&archive).arg("-C").arg(&tree).arg(".").status().unwrap();
    assert!(status.success());
    fs::remove_dir_all(&tree).unwrap();
    let digest = hex32(&sha256(&fs::read(&archive).unwrap()));
    fs::write(dir.join(&names.checksum), format!("{digest}  {}\n", names.archive)).unwrap();
    fs::write(dir.join(&names.id), format!("{id}\n")).unwrap();
}

fn installer(release_dir: &Path, home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_frust-install"));
    command
        .env("FRUST_RELEASE_BASE", format!("file://{}", release_dir.display()))
        .env("HOME", home)
        .env_remove("XDG_DATA_HOME")
        .env_remove("FRUST_PREFIX");
    command
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn current(prefix: &Path) -> String {
    fs::read_link(prefix.join("current")).unwrap().to_string_lossy().into_owned()
}

#[test]
fn the_bare_command_installs_the_latest_release_into_the_default_prefix() {
    let scratch = Scratch::new("bare");
    let (releases, home) = (scratch.path("releases"), scratch.path("home"));
    fs::create_dir_all(&releases).unwrap();
    fs::create_dir_all(&home).unwrap();
    publish(&releases, "frust-1.0.0-aaa", "one");

    let output = installer(&releases, &home).output().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "current=frust-1.0.0-aaa\n");
    assert!(stderr(&output).contains("current/bin"), "{}", stderr(&output));
    let prefix = release::default_prefix(OS, Some(home.clone()), None, None).unwrap();
    assert_eq!(current(&prefix), "versions/frust-1.0.0-aaa");
    assert_eq!(fs::read_to_string(prefix.join("current/bin/rustc")).unwrap(), "one");
}

#[test]
fn update_skips_a_current_install_installs_a_newer_one_and_rolls_back() {
    let scratch = Scratch::new("update");
    let (releases, home, prefix) = (scratch.path("releases"), scratch.path("home"), scratch.path("prefix"));
    fs::create_dir_all(&releases).unwrap();
    publish(&releases, "frust-1.0.0-aaa", "one");
    let prefix_arg = prefix.to_str().unwrap();

    let first = installer(&releases, &home).args(["install", "--prefix", prefix_arg]).output().unwrap();
    assert!(first.status.success(), "{}", stderr(&first));

    let same = installer(&releases, &home).args(["update", "--prefix", prefix_arg]).output().unwrap();
    assert!(same.status.success(), "{}", stderr(&same));
    assert_eq!(stdout(&same), "current=frust-1.0.0-aaa\n");
    assert!(stderr(&same).contains("up to date"), "{}", stderr(&same));
    assert_eq!(fs::read_dir(prefix.join("versions")).unwrap().count(), 1);

    publish(&releases, "frust-1.1.0-bbb", "two");
    let newer = installer(&releases, &home).args(["update", "--prefix", prefix_arg]).output().unwrap();
    assert!(newer.status.success(), "{}", stderr(&newer));
    assert_eq!(stdout(&newer), "current=frust-1.1.0-bbb\n");
    assert!(stderr(&newer).contains("previous frust-1.0.0-aaa"), "{}", stderr(&newer));
    assert_eq!(fs::read_to_string(prefix.join("current/bin/cargo")).unwrap(), "two");

    let back = installer(&releases, &home).args(["rollback", "--prefix", prefix_arg]).output().unwrap();
    assert!(back.status.success(), "{}", stderr(&back));
    assert_eq!(stdout(&back), "current=frust-1.0.0-aaa\n");
    assert_eq!(fs::read_to_string(prefix.join("current/bin/cargo")).unwrap(), "one");
}

#[test]
fn a_tampered_download_is_refused_and_the_install_is_left_alone() {
    let scratch = Scratch::new("tamper");
    let (releases, home, prefix) = (scratch.path("releases"), scratch.path("home"), scratch.path("prefix"));
    fs::create_dir_all(&releases).unwrap();
    publish(&releases, "frust-1.0.0-aaa", "one");
    let prefix_arg = prefix.to_str().unwrap();
    assert!(installer(&releases, &home).args(["install", "--prefix", prefix_arg]).status().unwrap().success());

    publish(&releases, "frust-1.1.0-bbb", "two");
    fs::write(releases.join(assets().checksum), "0".repeat(64)).unwrap();
    let refused = installer(&releases, &home).args(["update", "--prefix", prefix_arg]).output().unwrap();
    assert_eq!(refused.status.code(), Some(1));
    assert!(stderr(&refused).contains("checksum mismatch"), "{}", stderr(&refused));
    assert_eq!(current(&prefix), "versions/frust-1.0.0-aaa");
    assert_eq!(fs::read_to_string(prefix.join("current/bin/rustc")).unwrap(), "one");
}

#[test]
fn a_release_without_this_host_names_the_missing_target() {
    let scratch = Scratch::new("missing");
    let (releases, home) = (scratch.path("releases"), scratch.path("home"));
    fs::create_dir_all(&releases).unwrap();
    let output = installer(&releases, &home).args(["install", "--prefix", scratch.path("prefix").to_str().unwrap()]).output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains(release::host_target().unwrap()), "{}", stderr(&output));
    assert!(stdout(&output).is_empty());
}

#[test]
fn a_release_id_that_disagrees_with_its_archive_is_reported() {
    let scratch = Scratch::new("mismatch");
    let (releases, home, prefix) = (scratch.path("releases"), scratch.path("home"), scratch.path("prefix"));
    fs::create_dir_all(&releases).unwrap();
    publish(&releases, "frust-1.0.0-aaa", "one");
    fs::write(releases.join(assets().id), "frust-9.9.9-zzz\n").unwrap();
    let output = installer(&releases, &home).args(["install", "--prefix", prefix.to_str().unwrap()]).output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("frust-9.9.9-zzz"), "{}", stderr(&output));
}

#[test]
fn frust_prefix_chooses_the_prefix_and_a_missing_home_is_a_usage_error() {
    let scratch = Scratch::new("env");
    let (releases, home, prefix) = (scratch.path("releases"), scratch.path("home"), scratch.path("chosen"));
    fs::create_dir_all(&releases).unwrap();
    publish(&releases, "frust-1.0.0-aaa", "one");
    let output = installer(&releases, &home).env("FRUST_PREFIX", &prefix).output().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(current(&prefix), "versions/frust-1.0.0-aaa");

    let homeless = installer(&releases, &home).env_remove("HOME").output().unwrap();
    assert_eq!(homeless.status.code(), Some(2));
    assert!(stderr(&homeless).contains("--prefix"), "{}", stderr(&homeless));
}

#[test]
fn options_that_do_not_apply_are_usage_errors() {
    let scratch = Scratch::new("usage");
    let home = scratch.path("home");
    let prefix = scratch.path("prefix");
    let prefix_arg = prefix.to_str().unwrap();
    let cases: [&[&str]; 4] = [
        &["install", "--prefix", prefix_arg, "--version", "1.0.0", "--archive", "a", "--checksum", "c"],
        &["rollback", "--prefix", prefix_arg, "--version", "1.0.0"],
        &["uninstall", "--prefix", prefix_arg, "--archive", "a", "--checksum", "c"],
        &["install", "--prefix", prefix_arg, "--version"],
    ];
    for args in cases {
        let output = installer(&scratch.root, &home).args(args).output().unwrap();
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(stdout(&output).is_empty(), "{args:?}");
    }
    let help = installer(&scratch.root, &home).arg("--help").output().unwrap();
    assert!(help.status.success());
    assert!(stdout(&help).contains("usage: frust-install"));
}

#[test]
fn uninstall_uses_the_default_prefix_and_tolerates_nothing_installed() {
    let scratch = Scratch::new("uninstall");
    let (releases, home) = (scratch.path("releases"), scratch.path("home"));
    fs::create_dir_all(&releases).unwrap();
    publish(&releases, "frust-1.0.0-aaa", "one");
    let prefix = release::default_prefix(OS, Some(home.clone()), None, None).unwrap();
    assert!(installer(&releases, &home).status().unwrap().success());
    assert!(prefix.join("current").exists());

    let removed = installer(&releases, &home).arg("uninstall").output().unwrap();
    assert!(removed.status.success(), "{}", stderr(&removed));
    assert!(!prefix.join("current").exists());
    assert!(installer(&releases, &home).arg("uninstall").status().unwrap().success());
}
