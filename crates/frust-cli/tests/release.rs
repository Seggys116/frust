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
    publish_as(dir, &assets(), id, &format!("{{\"id\":\"{id}\"}}\n"), marker);
}

/// Like [`publish`] for any set of asset names, with the given manifest text.
fn publish_as(dir: &Path, names: &release::Assets, id: &str, manifest: &str, marker: &str) {
    let tree = dir.join(format!("tree-{id}"));
    let _ = fs::remove_dir_all(&tree);
    fs::create_dir_all(tree.join("bin")).unwrap();
    for tool in ["rustc", "cargo", "rustdoc"] {
        fs::write(tree.join("bin").join(tool), marker).unwrap();
    }
    fs::write(tree.join("manifest.json"), manifest).unwrap();
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
        .env_remove("FRUST_PREFIX")
        .env_remove("FRUST_VARIANT");
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

#[test]
fn an_invalid_frust_variant_is_a_usage_error_and_installs_nothing() {
    let scratch = Scratch::new("bad-variant");
    let (releases, home, prefix) = (scratch.path("releases"), scratch.path("home"), scratch.path("prefix"));
    fs::create_dir_all(&releases).unwrap();
    publish(&releases, "frust-1.0.0-aaa", "one");
    for bad in ["v4", "V3", "x86-64-v3", "fast"] {
        let output = installer(&releases, &home).env("FRUST_VARIANT", bad).args(["install", "--prefix", prefix.to_str().unwrap()]).output().unwrap();
        assert_eq!(output.status.code(), Some(2), "{bad}: {}", stderr(&output));
        assert!(stderr(&output).contains("FRUST_VARIANT must be auto, baseline or v3"), "{bad}: {}", stderr(&output));
        assert!(stdout(&output).is_empty(), "{bad}");
        assert!(!prefix.join("current").exists(), "{bad}");
    }
    // An empty value is the same as unset.
    let output = installer(&releases, &home).env("FRUST_VARIANT", "").args(["install", "--prefix", prefix.to_str().unwrap()]).output().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
}

/// Linux x86-64 releases carry a baseline and a v3 archive; the choice between them is the host's.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod variants {
    use super::*;
    use frust_cli::release::Variant;

    /// Publishes both builds of one release under their own ids, as package-release.sh does.
    fn publish_build(dir: &Path, release_id: &str, variant: Variant) {
        let id = if variant == Variant::V3 { format!("{release_id}-v3") } else { release_id.to_string() };
        let manifest = format!("{{\"id\":\"{id}\",\"variant\":\"{}\"}}\n", variant.name());
        let names = release::assets_for(release::host_target().unwrap(), variant);
        publish_as(dir, &names, &id, &manifest, &format!("{release_id}-{}", variant.name()));
    }

    fn publish_release(dir: &Path, release_id: &str, with_v3: bool) {
        publish_build(dir, release_id, Variant::Baseline);
        if with_v3 {
            publish_build(dir, release_id, Variant::V3);
        }
    }

    fn state_json(prefix: &Path) -> String {
        fs::read_to_string(prefix.join("state.json")).unwrap()
    }

    fn run(releases: &Path, home: &Path, prefix: &Path, command: &str, variant: Option<&str>) -> Output {
        let mut installer = installer(releases, home);
        if let Some(variant) = variant {
            installer.env("FRUST_VARIANT", variant);
        }
        installer.args([command, "--prefix", prefix.to_str().unwrap()]).output().unwrap()
    }

    #[test]
    fn a_forced_v3_install_is_kept_by_updates_and_left_only_when_asked() {
        let scratch = Scratch::new("v3-sticky");
        let (releases, home, prefix) = (scratch.path("releases"), scratch.path("home"), scratch.path("prefix"));
        fs::create_dir_all(&releases).unwrap();
        publish_release(&releases, "frust-1.0.0-aaa", true);

        let first = run(&releases, &home, &prefix, "install", Some("v3"));
        assert!(first.status.success(), "{}", stderr(&first));
        assert_eq!(stdout(&first), "current=frust-1.0.0-aaa-v3\n");
        assert!(stderr(&first).contains("using the v3 build"), "{}", stderr(&first));
        assert_eq!(fs::read_to_string(prefix.join("current/bin/rustc")).unwrap(), "frust-1.0.0-aaa-v3");
        assert!(state_json(&prefix).contains("\"current_variant\": \"v3\""), "{}", state_json(&prefix));

        // No FRUST_VARIANT: the installed variant stays, whatever this CPU could run.
        publish_release(&releases, "frust-1.1.0-bbb", true);
        let update = run(&releases, &home, &prefix, "update", None);
        assert!(update.status.success(), "{}", stderr(&update));
        assert_eq!(stdout(&update), "current=frust-1.1.0-bbb-v3\n");
        assert!(stderr(&update).contains("previous frust-1.0.0-aaa-v3"), "{}", stderr(&update));

        // FRUST_VARIANT=baseline leaves it, and the previous version keeps its variant for a rollback.
        let down = run(&releases, &home, &prefix, "update", Some("baseline"));
        assert!(down.status.success(), "{}", stderr(&down));
        assert_eq!(stdout(&down), "current=frust-1.1.0-bbb\n");
        let state = state_json(&prefix);
        assert!(state.contains("\"current_variant\": \"baseline\"") && state.contains("\"previous_variant\": \"v3\""), "{state}");

        // The baseline now stays too, although a v3 build of the release exists.
        let same = run(&releases, &home, &prefix, "update", None);
        assert!(same.status.success(), "{}", stderr(&same));
        assert_eq!(stdout(&same), "current=frust-1.1.0-bbb\n");
        assert!(stderr(&same).contains("up to date"), "{}", stderr(&same));

        let back = run(&releases, &home, &prefix, "rollback", None);
        assert!(back.status.success(), "{}", stderr(&back));
        assert_eq!(stdout(&back), "current=frust-1.1.0-bbb-v3\n");
        let state = state_json(&prefix);
        assert!(state.contains("\"current_variant\": \"v3\"") && state.contains("\"previous_variant\": \"baseline\""), "{state}");
    }

    #[test]
    fn a_release_without_a_v3_archive_installs_the_baseline() {
        let scratch = Scratch::new("no-v3");
        let (releases, home, prefix) = (scratch.path("releases"), scratch.path("home"), scratch.path("prefix"));
        fs::create_dir_all(&releases).unwrap();
        publish_release(&releases, "frust-1.0.0-aaa", false);
        for request in [None, Some("auto"), Some("baseline")] {
            let _ = fs::remove_dir_all(&prefix);
            let output = run(&releases, &home, &prefix, "install", request);
            assert!(output.status.success(), "{request:?}: {}", stderr(&output));
            assert_eq!(stdout(&output), "current=frust-1.0.0-aaa\n", "{request:?}");
            assert!(stderr(&output).contains("using the baseline build"), "{request:?}: {}", stderr(&output));
            assert!(state_json(&prefix).contains("\"current_variant\": \"baseline\""), "{request:?}");
        }
    }

    #[test]
    fn asking_for_v3_when_the_release_has_none_fails_without_touching_the_install() {
        let scratch = Scratch::new("v3-missing");
        let (releases, home, prefix) = (scratch.path("releases"), scratch.path("home"), scratch.path("prefix"));
        fs::create_dir_all(&releases).unwrap();
        publish_release(&releases, "frust-1.0.0-aaa", false);
        let refused = run(&releases, &home, &prefix, "install", Some("v3"));
        assert_eq!(refused.status.code(), Some(1));
        assert!(stderr(&refused).contains("no v3 build") && stderr(&refused).contains("FRUST_VARIANT=baseline"), "{}", stderr(&refused));
        assert!(!prefix.join("current").exists());

        // An installed v3 is not silently replaced by a baseline when a release lacks the v3 build.
        let both = scratch.path("both");
        fs::create_dir_all(&both).unwrap();
        publish_release(&both, "frust-1.0.0-aaa", true);
        assert!(run(&both, &home, &prefix, "install", Some("v3")).status.success());
        let stuck = run(&releases, &home, &prefix, "update", None);
        assert_eq!(stuck.status.code(), Some(1));
        assert!(stderr(&stuck).contains("no v3 build"), "{}", stderr(&stuck));
        assert_eq!(current(&prefix), "versions/frust-1.0.0-aaa-v3");
    }

    #[test]
    fn an_install_from_before_variants_is_picked_afresh_by_the_first_update() {
        let scratch = Scratch::new("pre-variants");
        let (releases, home, prefix) = (scratch.path("releases"), scratch.path("home"), scratch.path("prefix"));
        fs::create_dir_all(&releases).unwrap();
        publish(&releases, "frust-1.0.0-aaa", "one");
        assert!(run(&releases, &home, &prefix, "install", None).status.success());
        assert!(!state_json(&prefix).contains("\"current_variant\": \""), "{}", state_json(&prefix));

        // The host CPU decides under auto, so only a forced baseline is checked for its id here.
        publish_release(&releases, "frust-1.1.0-bbb", true);
        let forced = run(&releases, &home, &prefix, "update", Some("baseline"));
        assert!(forced.status.success(), "{}", stderr(&forced));
        assert_eq!(stdout(&forced), "current=frust-1.1.0-bbb\n");
        assert!(state_json(&prefix).contains("\"current_variant\": \"baseline\""), "{}", state_json(&prefix));
    }
}
