use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use frust_cli::install::{self, InstallError, PrefixState};
use frust_core::{hex32, sha256};

struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("frust-installer-{label}-{}-{n}", std::process::id()));
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

fn digest(path: &Path) -> String {
    hex32(&sha256(&fs::read(path).unwrap()))
}

fn write_sum(archive: &Path) -> PathBuf {
    let sum = PathBuf::from(format!("{}.sha256", archive.display()));
    fs::write(&sum, format!("{}\n", digest(archive))).unwrap();
    sum
}

/// A tiny toolchain tree packed with the system tar, mirroring package-toolchain.sh's layout.
fn toolchain(dir: &Path, name: &str, manifest: &str, marker: &str, omit: &[&str]) -> (PathBuf, PathBuf) {
    let root = dir.join(format!("tree-{name}"));
    fs::create_dir_all(root.join("bin")).unwrap();
    for relative in ["bin/rustc", "bin/cargo", "bin/rustdoc", "bin/fcargo"] {
        if !omit.contains(&relative) {
            fs::write(root.join(relative), marker).unwrap();
        }
    }
    if !omit.contains(&"manifest.json") {
        fs::write(root.join("manifest.json"), manifest).unwrap();
    }
    let archive = dir.join(format!("{name}.tar"));
    let status = Command::new("tar").arg("-c").arg("-f").arg(&archive).arg("-C").arg(&root).arg(".").status().unwrap();
    assert!(status.success());
    let sum = write_sum(&archive);
    (archive, sum)
}

fn simple(dir: &Path, id: &str, marker: &str) -> (PathBuf, PathBuf) {
    toolchain(dir, &format!("{id}-{marker}"), &format!("{{\"id\":\"{id}\"}}\n"), marker, &[])
}

struct Entry {
    name: String,
    kind: u8,
    link: String,
    data: Vec<u8>,
}

impl Entry {
    fn new(name: &str, kind: u8, link: &str, data: &str) -> Self {
        Self { name: name.to_string(), kind, link: link.to_string(), data: data.as_bytes().to_vec() }
    }

    fn file(name: &str, data: &str) -> Self {
        Self::new(name, b'0', "", data)
    }

    fn dir(name: &str) -> Self {
        Self::new(name, b'5', "", "")
    }

    fn symlink(name: &str, target: &str) -> Self {
        Self::new(name, b'2', target, "")
    }

    fn hardlink(name: &str, target: &str) -> Self {
        Self::new(name, b'1', target, "")
    }

    fn special(name: &str, kind: u8) -> Self {
        Self::new(name, kind, "", "")
    }
}

fn octal(field: &mut [u8], value: u64) {
    let text = format!("{value:0width$o}", width = field.len() - 1);
    field[..text.len()].copy_from_slice(text.as_bytes());
}

fn header(entry: &Entry) -> [u8; 512] {
    assert!(entry.name.len() <= 100 && entry.link.len() <= 100);
    let mut block = [0u8; 512];
    block[..entry.name.len()].copy_from_slice(entry.name.as_bytes());
    octal(&mut block[100..108], 0o755);
    octal(&mut block[108..116], 0);
    octal(&mut block[116..124], 0);
    octal(&mut block[124..136], entry.data.len() as u64);
    octal(&mut block[136..148], 0);
    block[148..156].fill(b' ');
    block[156] = entry.kind;
    block[157..157 + entry.link.len()].copy_from_slice(entry.link.as_bytes());
    block[257..263].copy_from_slice(b"ustar\0");
    block[263..265].copy_from_slice(b"00");
    let sum: u32 = block.iter().map(|byte| u32::from(*byte)).sum();
    block[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    block
}

fn raw_bytes(entries: &[Entry]) -> Vec<u8> {
    let mut out = Vec::new();
    for entry in entries {
        out.extend_from_slice(&header(entry));
        out.extend_from_slice(&entry.data);
        out.resize(out.len().div_ceil(512) * 512, 0);
    }
    out.extend_from_slice(&[0u8; 1024]);
    out
}

fn write_archive(dir: &Path, name: &str, bytes: &[u8]) -> (PathBuf, PathBuf) {
    let archive = dir.join(format!("{name}.tar"));
    fs::write(&archive, bytes).unwrap();
    let sum = write_sum(&archive);
    (archive, sum)
}

fn good_entries(id: &str) -> Vec<Entry> {
    vec![
        Entry::file("manifest.json", &format!("{{\"id\":\"{id}\"}}\n")),
        Entry::file("bin/rustc", "raw"),
        Entry::file("bin/cargo", "raw"),
        Entry::file("bin/rustdoc", "raw"),
    ]
}

fn names(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = match fs::read_dir(dir) {
        Ok(entries) => entries.map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned()).collect(),
        Err(_) => Vec::new(),
    };
    out.sort();
    out
}

fn versions(prefix: &Path) -> Vec<String> {
    names(&prefix.join("versions"))
}

fn rustc(prefix: &Path) -> String {
    fs::read_to_string(prefix.join("current/bin/rustc")).unwrap()
}

fn state_field(prefix: &Path, name: &str) -> Option<String> {
    let text = fs::read_to_string(prefix.join("state.json")).unwrap();
    let key = format!("\"{name}\": ");
    let line = text.lines().map(str::trim).find(|line| line.starts_with(&key)).unwrap_or_else(|| panic!("no {name} in {text}"));
    let value = line[key.len()..].trim_end_matches(',');
    if value == "null" {
        None
    } else {
        Some(value.trim_matches('"').to_string())
    }
}

fn assert_state(prefix: &Path, current: Option<&str>, previous: Option<&str>) {
    assert_eq!(install::current_version(prefix).unwrap().as_deref(), current);
    assert_eq!(state_field(prefix, "current").as_deref(), current);
    assert_eq!(state_field(prefix, "previous").as_deref(), previous);
}

fn assert_clean(prefix: &Path) {
    for name in names(prefix) {
        assert!(!name.starts_with(".current-next") && !name.starts_with(".state.json.next"), "leftover {name}");
    }
    for name in versions(prefix) {
        assert!(!name.starts_with(".partial-work.") && !name.starts_with(".partial-aside."), "leftover {name}");
    }
}

fn assert_rejected(prefix: &Path, result: Result<PrefixState, InstallError>, needle: &str) {
    let err = result.unwrap_err();
    assert!(err.message.contains(needle), "expected {needle:?} in {:?}", err.message);
    assert!(fs::symlink_metadata(prefix.join("current")).is_err(), "current was created");
    assert!(versions(prefix).is_empty(), "versions left behind: {:?}", versions(prefix));
}

fn state(current: &str, previous: Option<&str>) -> PrefixState {
    PrefixState { current: Some(current.to_string()), previous: previous.map(str::to_string), current_variant: None, previous_variant: None }
}

fn variant_state(current: (&str, Option<&str>), previous: Option<(&str, Option<&str>)>) -> PrefixState {
    PrefixState {
        current: Some(current.0.to_string()),
        previous: previous.map(|(id, _)| id.to_string()),
        current_variant: current.1.map(str::to_string),
        previous_variant: previous.and_then(|(_, variant)| variant).map(str::to_string),
    }
}

/// A toolchain whose manifest names a variant, as the release archives of Linux x86-64 do.
fn variant_toolchain(dir: &Path, id: &str, variant: Option<&str>, marker: &str) -> (PathBuf, PathBuf) {
    let manifest = match variant {
        Some(variant) => format!("{{\"id\":\"{id}\",\"variant\":\"{variant}\"}}\n"),
        None => format!("{{\"id\":\"{id}\"}}\n"),
    };
    toolchain(dir, &format!("{id}-{marker}"), &manifest, marker, &[])
}

#[test]
fn install_activates_the_version_and_records_state() {
    let scratch = Scratch::new("happy");
    let prefix = scratch.path("prefix");
    let (archive, sum) = simple(&scratch.root, "first", "one");
    assert_eq!(install::install(&prefix, &archive, &sum).unwrap(), state("first", None));
    assert_eq!(fs::read_link(prefix.join("current")).unwrap(), Path::new("versions/first"));
    assert_eq!(rustc(&prefix), "one");
    assert_state(&prefix, Some("first"), None);
    assert_eq!(versions(&prefix), ["first"]);
    assert!(prefix.join("versions/first/manifest.json").is_file());
    assert_clean(&prefix);
}

#[test]
fn upgrade_keeps_the_previous_version_for_rollback() {
    let scratch = Scratch::new("upgrade");
    let prefix = scratch.path("prefix");
    let (first, first_sum) = simple(&scratch.root, "first", "one");
    let (second, second_sum) = simple(&scratch.root, "second", "two");
    install::install(&prefix, &first, &first_sum).unwrap();
    assert_eq!(install::install(&prefix, &second, &second_sum).unwrap(), state("second", Some("first")));
    assert_eq!(rustc(&prefix), "two");
    assert_eq!(versions(&prefix), ["first", "second"]);
    assert_state(&prefix, Some("second"), Some("first"));
}

#[test]
fn accepted_checksum_spellings() {
    let scratch = Scratch::new("sum-ok");
    let (archive, sum) = simple(&scratch.root, "first", "one");
    let digest = digest(&archive);
    let spellings = [
        digest.to_uppercase(),
        format!("{digest}  first-one.tar"),
        format!("{digest} *first-one.tar"),
        format!("{digest}\r"),
        format!("\n  {digest}"),
    ];
    for (index, text) in spellings.iter().enumerate() {
        let prefix = scratch.path(&format!("prefix-{index}"));
        fs::write(&sum, format!("{text}\n")).unwrap();
        install::install(&prefix, &archive, &sum).unwrap_or_else(|err| panic!("{text:?}: {}", err.message));
        assert_state(&prefix, Some("first"), None);
    }
}

#[test]
fn bad_checksums_are_rejected_before_anything_is_installed() {
    let scratch = Scratch::new("sum-bad");
    let (archive, sum) = simple(&scratch.root, "first", "one");
    let good = digest(&archive);
    let flipped = format!("{}{}", &good[..63], if good.ends_with('0') { '1' } else { '0' });
    let cases: Vec<(&str, String, &str)> = vec![
        ("short", "abc123\n".to_string(), "does not contain a sha256 digest"),
        ("empty", String::new(), "does not contain a sha256 digest"),
        ("blank", "  \n\n".to_string(), "does not contain a sha256 digest"),
        ("non-hex", format!("{}\n", "g".repeat(64)), "does not contain a sha256 digest"),
        ("long", format!("{}\n", "a".repeat(65)), "does not contain a sha256 digest"),
        ("zero", format!("{}\n", "0".repeat(64)), "checksum mismatch"),
        ("one bit off", format!("{flipped}  first-one.tar\n"), "checksum mismatch"),
    ];
    for (label, text, needle) in cases {
        let prefix = scratch.path(&format!("prefix-{label}"));
        fs::write(&sum, text).unwrap();
        let result = install::install(&prefix, &archive, &sum);
        let err = result.unwrap_err();
        assert!(err.message.contains(needle), "{label}: {:?}", err.message);
        assert!(fs::symlink_metadata(prefix.join("current")).is_err(), "{label}");
        assert!(versions(&prefix).is_empty(), "{label}");
    }
}

#[test]
fn missing_checksum_or_archive_changes_nothing() {
    let scratch = Scratch::new("missing");
    let prefix = scratch.path("prefix");
    let (archive, sum) = simple(&scratch.root, "first", "one");
    let err = install::install(&prefix, &scratch.path("absent.tar"), &sum).unwrap_err();
    assert!(err.message.contains("could not read archive"), "{}", err.message);
    let err = install::install(&prefix, &archive, &scratch.path("absent.sha256")).unwrap_err();
    assert!(err.message.contains("could not read checksum"), "{}", err.message);
    assert!(!prefix.exists());
}

#[test]
fn verify_checksum_streams_archives_larger_than_its_buffer() {
    let scratch = Scratch::new("stream");
    let bytes: Vec<u8> = (0..3 * 65536 + 17).map(|index: usize| (index * 7 % 253) as u8).collect();
    let (archive, sum) = write_archive(&scratch.root, "big", &bytes);
    install::verify_checksum(&archive, &sum).unwrap();
    let mut changed = bytes.clone();
    changed[65536] ^= 1;
    fs::write(&archive, changed).unwrap();
    let err = install::verify_checksum(&archive, &sum).unwrap_err();
    assert!(err.message.contains("checksum mismatch"));
}

#[test]
fn unsafe_manifest_ids_are_rejected_and_nothing_is_deleted() {
    let scratch = Scratch::new("ids");
    let prefix = scratch.path("prefix");
    let (good, good_sum) = simple(&scratch.root, "good", "one");
    install::install(&prefix, &good, &good_sum).unwrap();
    let long = "a".repeat(129);
    let bad = [".", "..", ".hidden", "../x", "a/b", "", "-lead", ".partial-x", "a b", "caf\u{e9}", long.as_str()];
    for (index, id) in bad.iter().enumerate() {
        let (archive, sum) = toolchain(&scratch.root, &format!("bad-{index}"), &format!("{{\"id\":\"{id}\"}}\n"), "evil", &[]);
        let err = install::install(&prefix, &archive, &sum).unwrap_err();
        assert!(err.message.contains("not allowed"), "{id:?}: {}", err.message);
        assert_eq!(versions(&prefix), ["good"], "{id:?}");
        assert_eq!(rustc(&prefix), "one", "{id:?}");
        assert_state(&prefix, Some("good"), None);
    }
    assert!(!prefix.join("x").exists());
    assert_clean(&prefix);
}

#[test]
fn reasonable_ids_are_accepted() {
    let scratch = Scratch::new("ids-ok");
    for (index, id) in ["frust-0ef1bc386-central1", "1.0_x", "A", "v1.2.3-rc.1"].iter().enumerate() {
        let prefix = scratch.path(&format!("prefix-{index}"));
        let (archive, sum) = simple(&scratch.root, id, "one");
        assert_eq!(install::install(&prefix, &archive, &sum).unwrap(), state(id, None));
    }
}

#[test]
fn manifest_may_use_json_whitespace_escapes_and_extra_fields() {
    let scratch = Scratch::new("manifest-ws");
    let manifests = [
        ("spaced", "  {\n \"rust_commit\" : \"abc\" ,\n  \"id\"\t:\n \"spaced\" ,\n \"extra\": [1, {\"a\": null}, \"x\"] \n}\n"),
        ("escaped", "{\"id\":\"esc\\u0061ped\"}"),
        ("crlf", "{\r\n\"id\"\r\n:\r\n\"crlf\"\r\n}\r\n"),
    ];
    for (name, manifest) in manifests {
        let prefix = scratch.path(&format!("prefix-{name}"));
        let (archive, sum) = toolchain(&scratch.root, name, manifest, "one", &[]);
        assert_eq!(install::install(&prefix, &archive, &sum).unwrap(), state(name, None), "{name}");
    }
}

#[test]
fn malformed_manifests_are_rejected() {
    let scratch = Scratch::new("manifest-bad");
    let cases = [
        ("noid", "{\"host\":\"x\"}", "has no id"),
        ("numeric", "{\"id\": 5}", "is not a string"),
        ("null", "{\"id\": null}", "is not a string"),
        ("twice", "{\"id\":\"a\",\"id\":\"b\"}", "more than one id"),
        ("broken", "{\"id\":", "not a valid JSON"),
        ("array", "[\"id\"]", "not a valid JSON"),
        ("trailing", "{\"id\":\"a\"} x", "not a valid JSON"),
        ("empty", "", "not a valid JSON"),
    ];
    for (name, manifest, needle) in cases {
        let prefix = scratch.path(&format!("prefix-{name}"));
        let (archive, sum) = toolchain(&scratch.root, name, manifest, "one", &[]);
        assert_rejected(&prefix, install::install(&prefix, &archive, &sum), needle);
    }
}

#[test]
fn same_id_reinstall_keeps_the_rollback_target_and_never_leaves_current_dangling() {
    let scratch = Scratch::new("reinstall");
    let prefix = scratch.path("prefix");
    let (first, first_sum) = simple(&scratch.root, "first", "one");
    let (second, second_sum) = simple(&scratch.root, "second", "two");
    let (second_again, second_again_sum) = toolchain(&scratch.root, "second-three", "{\"id\":\"second\"}\n", "three", &[]);
    install::install(&prefix, &first, &first_sum).unwrap();
    install::install(&prefix, &second, &second_sum).unwrap();

    assert_eq!(install::install(&prefix, &second_again, &second_again_sum).unwrap(), state("second", Some("first")));
    assert_eq!(rustc(&prefix), "three");
    assert_eq!(versions(&prefix), ["first", "second"]);
    assert_state(&prefix, Some("second"), Some("first"));

    assert_eq!(install::install(&prefix, &second_again, &second_again_sum).unwrap(), state("second", Some("first")));
    assert_eq!(rustc(&prefix), "three");

    assert_eq!(install::rollback(&prefix).unwrap(), state("first", Some("second")));
    assert_eq!(rustc(&prefix), "one");
    assert_clean(&prefix);
}

#[test]
fn reinstalling_the_only_version_keeps_previous_empty() {
    let scratch = Scratch::new("reinstall-only");
    let prefix = scratch.path("prefix");
    let (first, first_sum) = simple(&scratch.root, "first", "one");
    install::install(&prefix, &first, &first_sum).unwrap();
    assert_eq!(install::install(&prefix, &first, &first_sum).unwrap(), state("first", None));
    assert_eq!(rustc(&prefix), "one");
    assert_state(&prefix, Some("first"), None);
    assert_eq!(versions(&prefix), ["first"]);
}

#[test]
fn rollback_without_a_previous_version_or_prefix_fails_cleanly() {
    let scratch = Scratch::new("rollback-none");
    let missing = scratch.path("missing");
    let err = install::rollback(&missing).unwrap_err();
    assert!(err.message.contains("prefix"), "{}", err.message);
    assert!(!missing.exists());

    let prefix = scratch.path("prefix");
    let (first, first_sum) = simple(&scratch.root, "first", "one");
    install::install(&prefix, &first, &first_sum).unwrap();
    let err = install::rollback(&prefix).unwrap_err();
    assert!(err.message.contains("no previous version"), "{}", err.message);
    assert_state(&prefix, Some("first"), None);
    assert_eq!(rustc(&prefix), "one");
}

#[test]
fn rollback_toggles_between_two_versions() {
    let scratch = Scratch::new("rollback-toggle");
    let prefix = scratch.path("prefix");
    let (first, first_sum) = simple(&scratch.root, "first", "one");
    let (second, second_sum) = simple(&scratch.root, "second", "two");
    install::install(&prefix, &first, &first_sum).unwrap();
    install::install(&prefix, &second, &second_sum).unwrap();
    assert_eq!(install::rollback(&prefix).unwrap(), state("first", Some("second")));
    assert_eq!(rustc(&prefix), "one");
    assert_eq!(install::rollback(&prefix).unwrap(), state("second", Some("first")));
    assert_eq!(rustc(&prefix), "two");
    assert_state(&prefix, Some("second"), Some("first"));
}

#[test]
fn rollback_refuses_when_the_previous_directory_is_missing() {
    let scratch = Scratch::new("rollback-gone");
    let prefix = scratch.path("prefix");
    let (first, first_sum) = simple(&scratch.root, "first", "one");
    let (second, second_sum) = simple(&scratch.root, "second", "two");
    install::install(&prefix, &first, &first_sum).unwrap();
    install::install(&prefix, &second, &second_sum).unwrap();
    fs::remove_dir_all(prefix.join("versions/first")).unwrap();
    let err = install::rollback(&prefix).unwrap_err();
    assert!(err.message.contains("is not installed"), "{}", err.message);
    assert_state(&prefix, Some("second"), Some("first"));
    assert_eq!(rustc(&prefix), "two");
}

#[test]
fn uninstall_is_idempotent_and_tolerates_a_missing_prefix() {
    let scratch = Scratch::new("uninstall");
    let missing = scratch.path("missing");
    install::uninstall(&missing).unwrap();
    assert!(!missing.exists());

    let prefix = scratch.path("prefix");
    let (first, first_sum) = simple(&scratch.root, "first", "one");
    install::install(&prefix, &first, &first_sum).unwrap();
    fs::write(prefix.join(".state.json.next.stale"), "x").unwrap();
    std::os::unix::fs::symlink("versions/first", prefix.join(".current-next.stale")).unwrap();
    install::uninstall(&prefix).unwrap();
    assert!(fs::symlink_metadata(prefix.join("current")).is_err());
    assert!(!prefix.join("versions").exists());
    assert!(!prefix.join("state.json").exists());
    assert_clean(&prefix);
    install::uninstall(&prefix).unwrap();

    install::install(&prefix, &first, &first_sum).unwrap();
    assert_state(&prefix, Some("first"), None);
}

#[test]
fn corrupt_state_is_reported_without_touching_the_installation() {
    let scratch = Scratch::new("corrupt-state");
    let prefix = scratch.path("prefix");
    let (first, first_sum) = simple(&scratch.root, "first", "one");
    let (second, second_sum) = simple(&scratch.root, "second", "two");
    install::install(&prefix, &first, &first_sum).unwrap();
    for text in ["{not json", "", "[]", "{\"current\": 3}", "{\"current\": \"../x\", \"previous\": null}", "{\"previous\": {\"a\": 1}}"] {
        fs::write(prefix.join("state.json"), text).unwrap();
        let err = install::rollback(&prefix).unwrap_err();
        assert!(err.message.contains("state.json"), "{text:?}: {}", err.message);
        let err = install::install(&prefix, &second, &second_sum).unwrap_err();
        assert!(err.message.contains("state.json"), "{text:?}: {}", err.message);
        assert_eq!(versions(&prefix), ["first"], "{text:?}");
        assert_eq!(rustc(&prefix), "one", "{text:?}");
    }
    fs::remove_file(prefix.join("state.json")).unwrap();
    assert_eq!(install::install(&prefix, &second, &second_sum).unwrap(), state("second", Some("first")));
}

#[test]
fn state_is_reconciled_with_the_current_symlink_after_an_interrupted_swap() {
    let scratch = Scratch::new("reconcile");
    let prefix = scratch.path("prefix");
    let (first, first_sum) = simple(&scratch.root, "first", "one");
    let (second, second_sum) = simple(&scratch.root, "second", "two");
    let (third, third_sum) = simple(&scratch.root, "third", "three");
    install::install(&prefix, &first, &first_sum).unwrap();
    install::install(&prefix, &second, &second_sum).unwrap();

    // A rollback that swapped the link and died before rewriting state.json.
    fs::remove_file(prefix.join("current")).unwrap();
    std::os::unix::fs::symlink("versions/first", prefix.join("current")).unwrap();
    assert_eq!(install::current_version(&prefix).unwrap().as_deref(), Some("first"));
    assert_eq!(install::rollback(&prefix).unwrap(), state("second", Some("first")));

    // An install that swapped to a third version and died before the state write.
    install::install(&prefix, &third, &third_sum).unwrap();
    fs::write(prefix.join("state.json"), "{\"current\": \"second\", \"previous\": \"first\"}\n").unwrap();
    assert_eq!(install::current_version(&prefix).unwrap().as_deref(), Some("third"));
    assert_eq!(install::rollback(&prefix).unwrap(), state("second", Some("third")));
    assert_eq!(rustc(&prefix), "two");
    assert_clean(&prefix);
}

#[test]
fn a_current_entry_that_is_not_a_managed_symlink_is_refused() {
    let scratch = Scratch::new("foreign-current");
    let prefix = scratch.path("prefix");
    fs::create_dir_all(prefix.join("current")).unwrap();
    fs::write(prefix.join("current/keep"), "x").unwrap();
    let (first, first_sum) = simple(&scratch.root, "first", "one");
    let err = install::install(&prefix, &first, &first_sum).unwrap_err();
    assert!(err.message.contains("not a symlink managed"), "{}", err.message);
    assert!(prefix.join("current/keep").is_file());

    let other = scratch.path("other");
    fs::create_dir_all(&other).unwrap();
    std::os::unix::fs::symlink("/usr", other.join("current")).unwrap();
    let err = install::install(&other, &first, &first_sum).unwrap_err();
    assert!(err.message.contains("not a managed version"), "{}", err.message);
}

#[test]
fn leftovers_from_interrupted_runs_are_cleaned_on_the_next_install() {
    let scratch = Scratch::new("leftovers");
    let prefix = scratch.path("prefix");
    let (first, first_sum) = simple(&scratch.root, "first", "one");
    let (second, second_sum) = simple(&scratch.root, "second", "two");
    install::install(&prefix, &first, &first_sum).unwrap();
    std::os::unix::fs::symlink("versions/first", prefix.join(".current-next.stale")).unwrap();
    fs::write(prefix.join(".state.json.next.stale"), "{").unwrap();
    fs::create_dir_all(prefix.join("versions/.partial-work.dead")).unwrap();
    fs::write(prefix.join("versions/.partial-work.dead/archive.tar"), "x").unwrap();
    install::install(&prefix, &second, &second_sum).unwrap();
    assert_clean(&prefix);
    assert_eq!(versions(&prefix), ["first", "second"]);
}

#[test]
fn an_interrupted_same_id_swap_restores_the_old_tree() {
    let scratch = Scratch::new("aside");
    let prefix = scratch.path("prefix");
    let (first, first_sum) = simple(&scratch.root, "first", "one");
    let (second, second_sum) = simple(&scratch.root, "second", "two");
    install::install(&prefix, &first, &first_sum).unwrap();
    // The old tree was renamed aside and the process died before the new one landed.
    fs::rename(prefix.join("versions/first"), prefix.join("versions/.partial-aside.dead-1-1.first")).unwrap();
    assert_eq!(install::install(&prefix, &second, &second_sum).unwrap(), state("second", Some("first")));
    assert_eq!(versions(&prefix), ["first", "second"]);
    assert_eq!(fs::read_to_string(prefix.join("versions/first/bin/rustc")).unwrap(), "one");
    assert_clean(&prefix);

    // When the new tree did land, the stale aside copy is discarded instead.
    fs::create_dir_all(prefix.join("versions/.partial-aside.dead-2-2.first")).unwrap();
    install::rollback(&prefix).unwrap();
    assert_eq!(rustc(&prefix), "one");
    assert_clean(&prefix);
}

#[test]
fn stage_leaves_the_active_version_alone() {
    let scratch = Scratch::new("stage");
    let prefix = scratch.path("prefix");
    let (first, first_sum) = simple(&scratch.root, "first", "one");
    let (second, second_sum) = simple(&scratch.root, "second", "two");
    install::install(&prefix, &first, &first_sum).unwrap();
    let staged = install::stage_without_activate(&prefix, &second, &second_sum).unwrap();
    assert!(staged.starts_with(prefix.join("versions")));
    assert!(staged.file_name().unwrap().to_string_lossy().starts_with(".partial-"));
    assert!(staged.join("manifest.json").is_file());
    assert!(staged.join("bin/rustc").is_file());
    assert!(!prefix.join("versions/second").exists());
    assert_state(&prefix, Some("first"), None);
    let again = install::stage_without_activate(&prefix, &second, &second_sum).unwrap();
    assert_eq!(staged, again);
    assert_clean(&prefix);

    let bad = scratch.path("bad.sha256");
    fs::write(&bad, "0".repeat(64)).unwrap();
    assert!(install::stage_without_activate(&prefix, &second, &bad).is_err());
    assert_eq!(rustc(&prefix), "one");
}

#[test]
fn truncated_archives_are_rejected() {
    let scratch = Scratch::new("truncated");
    let prefix = scratch.path("prefix");
    let (original, _) = simple(&scratch.root, "first", "one");
    let bytes = fs::read(&original).unwrap();
    for (label, keep) in [("mid-header", 700usize), ("mid-data", 1500), ("one block", 512)] {
        let (archive, sum) = write_archive(&scratch.root, label, &bytes[..keep.min(bytes.len())]);
        assert_rejected(&prefix, install::install(&prefix, &archive, &sum), "truncated");
    }

    let raw = raw_bytes(&good_entries("first"));
    let (archive, sum) = write_archive(&scratch.root, "no-end", &raw[..raw.len() - 1024]);
    assert_rejected(&prefix, install::install(&prefix, &archive, &sum), "truncated");

    let (archive, sum) = write_archive(&scratch.root, "empty", &[]);
    assert_rejected(&prefix, install::install(&prefix, &archive, &sum), "truncated");
}

#[test]
fn a_failed_upgrade_leaves_the_active_version_untouched() {
    let scratch = Scratch::new("failed-upgrade");
    let prefix = scratch.path("prefix");
    let (first, first_sum) = simple(&scratch.root, "first", "one");
    install::install(&prefix, &first, &first_sum).unwrap();
    let bytes = fs::read(&first).unwrap();
    let (cut, cut_sum) = write_archive(&scratch.root, "cut", &bytes[..700]);
    assert!(install::install(&prefix, &cut, &cut_sum).is_err());
    assert_state(&prefix, Some("first"), None);
    assert_eq!(rustc(&prefix), "one");
    assert_eq!(versions(&prefix), ["first"]);
    assert_clean(&prefix);
}

#[test]
fn the_installed_variant_is_recorded_swapped_by_rollback_and_kept_by_a_reinstall() {
    let scratch = Scratch::new("variant-state");
    let prefix = scratch.path("prefix");
    assert_eq!(install::current_variant(&prefix).unwrap(), None);
    let (base, base_sum) = variant_toolchain(&scratch.root, "rel-1", Some("baseline"), "base");
    let (fast, fast_sum) = variant_toolchain(&scratch.root, "rel-1-v3", Some("v3"), "fast");

    assert_eq!(install::install(&prefix, &base, &base_sum).unwrap(), variant_state(("rel-1", Some("baseline")), None));
    assert_eq!(state_field(&prefix, "current_variant").as_deref(), Some("baseline"));
    assert_eq!(state_field(&prefix, "previous_variant"), None);
    assert_eq!(install::current_variant(&prefix).unwrap().as_deref(), Some("baseline"));

    let switched = install::install(&prefix, &fast, &fast_sum).unwrap();
    assert_eq!(switched, variant_state(("rel-1-v3", Some("v3")), Some(("rel-1", Some("baseline")))));
    assert_eq!(state_field(&prefix, "current_variant").as_deref(), Some("v3"));
    assert_eq!(state_field(&prefix, "previous_variant").as_deref(), Some("baseline"));
    assert_eq!(install::current_variant(&prefix).unwrap().as_deref(), Some("v3"));

    // Installing the live id again must not turn the rollback target into itself or lose its variant.
    assert_eq!(install::install(&prefix, &fast, &fast_sum).unwrap(), switched);

    let back = install::rollback(&prefix).unwrap();
    assert_eq!(back, variant_state(("rel-1", Some("baseline")), Some(("rel-1-v3", Some("v3")))));
    assert_eq!(install::current_variant(&prefix).unwrap().as_deref(), Some("baseline"));
    assert_eq!(rustc(&prefix), "base");
    let forward = install::rollback(&prefix).unwrap();
    assert_eq!(forward, switched);
    assert_clean(&prefix);
}

#[test]
fn a_state_file_without_variant_fields_is_read_and_repaired_from_the_manifests() {
    let scratch = Scratch::new("variant-legacy");
    let prefix = scratch.path("prefix");
    let (first, first_sum) = variant_toolchain(&scratch.root, "first", None, "one");
    let (second, second_sum) = variant_toolchain(&scratch.root, "second", Some("v3"), "two");
    install::install(&prefix, &first, &first_sum).unwrap();
    install::install(&prefix, &second, &second_sum).unwrap();

    // The state of an installer that predates variants.
    fs::write(prefix.join("state.json"), "{\n  \"current\": \"second\",\n  \"previous\": \"first\"\n}\n").unwrap();
    assert_eq!(install::current_variant(&prefix).unwrap().as_deref(), Some("v3"));
    assert_eq!(install::rollback(&prefix).unwrap(), variant_state(("first", None), Some(("second", Some("v3")))));
    assert_eq!(state_field(&prefix, "previous_variant").as_deref(), Some("v3"));

    // A swap that died before the state write: the state names another current than the link does.
    fs::write(prefix.join("state.json"), "{\"current\": \"first\", \"previous\": \"second\", \"current_variant\": null, \"previous_variant\": \"v3\"}\n").unwrap();
    fs::remove_file(prefix.join("current")).unwrap();
    std::os::unix::fs::symlink("versions/second", prefix.join("current")).unwrap();
    assert_eq!(install::current_variant(&prefix).unwrap().as_deref(), Some("v3"));
    assert_eq!(install::rollback(&prefix).unwrap(), variant_state(("first", None), Some(("second", Some("v3")))));
}

#[test]
fn bad_variant_names_are_rejected_in_manifests_and_in_state() {
    let scratch = Scratch::new("variant-bad");
    let prefix = scratch.path("prefix");
    for bad in ["V3", "../v3", "v 3", ""] {
        let (archive, sum) = variant_toolchain(&scratch.root, "first", Some(bad), "one");
        let err = install::install(&prefix, &archive, &sum).unwrap_err();
        assert!(err.message.contains("variant"), "{bad:?}: {}", err.message);
        assert!(fs::symlink_metadata(prefix.join("current")).is_err(), "{bad:?}: current was created");
    }
    let (first, first_sum) = variant_toolchain(&scratch.root, "first", Some("v3"), "one");
    let (second, second_sum) = variant_toolchain(&scratch.root, "second", Some("baseline"), "two");
    install::install(&prefix, &first, &first_sum).unwrap();
    for text in [
        "{\"current\": \"first\", \"current_variant\": \"V3\"}",
        "{\"current\": \"first\", \"previous_variant\": 3}",
        "{\"current\": \"first\", \"previous_variant\": {\"a\": 1}}",
    ] {
        fs::write(prefix.join("state.json"), text).unwrap();
        let err = install::install(&prefix, &second, &second_sum).unwrap_err();
        assert!(err.message.contains("state.json"), "{text:?}: {}", err.message);
        assert_eq!(rustc(&prefix), "one", "{text:?}");
    }
}

#[test]
fn corrupt_tar_headers_are_rejected() {
    let scratch = Scratch::new("corrupt-header");
    let prefix = scratch.path("prefix");
    let mut raw = raw_bytes(&good_entries("first"));
    raw[0] ^= 1;
    let (archive, sum) = write_archive(&scratch.root, "flipped", &raw);
    assert_rejected(&prefix, install::install(&prefix, &archive, &sum), "header checksum");

    let mut raw = raw_bytes(&good_entries("first"));
    let end = raw.len() - 1024;
    raw.truncate(end);
    raw.extend_from_slice(&[0u8; 512]);
    raw.extend_from_slice(&header(&Entry::file("late", "")));
    let (archive, sum) = write_archive(&scratch.root, "lone-zero", &raw);
    assert_rejected(&prefix, install::install(&prefix, &archive, &sum), "lone end-of-archive");
}

#[test]
fn hostile_tar_entries_are_rejected_before_extraction() {
    let scratch = Scratch::new("hostile");
    let prefix = scratch.path("prefix");
    let absolute = scratch.path("abs-evil");
    let absolute = absolute.to_str().unwrap();
    let cases: Vec<(&str, Vec<Entry>, &str)> = vec![
        ("parent file", vec![Entry::file("../evil", "x")], "'..'"),
        ("nested parent", vec![Entry::file("bin/../../evil", "x")], "'..'"),
        ("absolute file", vec![Entry::file(absolute, "x")], "absolute"),
        ("absolute dir", vec![Entry::dir("/tmp/frust-evil-dir")], "absolute"),
        ("escaping symlink", vec![Entry::symlink("bin/link", "../../outside")], "escapes"),
        ("root symlink escape", vec![Entry::symlink("link", "..")], "escapes"),
        ("absolute symlink", vec![Entry::symlink("link", "/etc")], "absolute"),
        ("empty symlink", vec![Entry::symlink("link", "")], "absolute"),
        ("climb after descent", vec![Entry::symlink("bin/link", "x/../..")], "climbs after descending"),
        ("through symlink", vec![Entry::symlink("d", "."), Entry::file("d/x", "x")], "passes through a symbolic link"),
        ("redefined symlink", vec![Entry::symlink("s", "bin"), Entry::dir("s")], "redefines"),
        ("hardlink parent", vec![Entry::hardlink("h", "../x")], "hard link"),
        ("hardlink absolute", vec![Entry::hardlink("h", "/etc/passwd")], "hard link"),
        ("device", vec![Entry::special("dev", b'3')], "unsupported entry type"),
        ("fifo", vec![Entry::special("fifo", b'6')], "unsupported entry type"),
    ];
    for (index, (label, extra, needle)) in cases.into_iter().enumerate() {
        let mut entries = good_entries("first");
        entries.extend(extra);
        let (archive, sum) = write_archive(&scratch.root, &format!("hostile-{index}"), &raw_bytes(&entries));
        let result = install::install(&prefix, &archive, &sum);
        let err = result.as_ref().unwrap_err();
        assert!(err.message.contains(needle), "{label}: {:?}", err.message);
        assert_rejected(&prefix, result, needle);
        assert!(!Path::new(absolute).exists(), "{label}");
        assert!(!Path::new("/tmp/frust-evil-dir").exists(), "{label}");
        assert!(!scratch.path("evil").exists(), "{label}");
        assert!(!prefix.join("evil").exists(), "{label}");
        assert!(!prefix.join("versions/evil").exists(), "{label}");
    }
}

#[test]
fn links_that_stay_inside_the_tree_are_accepted() {
    let scratch = Scratch::new("links");
    let prefix = scratch.path("prefix");
    let mut entries = good_entries("linked");
    entries.push(Entry::symlink("bin/frustc", "rustc"));
    entries.push(Entry::hardlink("bin/frustdoc", "bin/rustdoc"));
    entries.push(Entry::dir("lib/"));
    entries.push(Entry::symlink("lib/rc", "../bin/rustc"));
    let (archive, sum) = write_archive(&scratch.root, "linked", &raw_bytes(&entries));
    assert_eq!(install::install(&prefix, &archive, &sum).unwrap(), state("linked", None));
    assert_eq!(fs::read_to_string(prefix.join("current/bin/frustc")).unwrap(), "raw");
    assert_eq!(fs::read_to_string(prefix.join("current/bin/frustdoc")).unwrap(), "raw");
    assert_eq!(fs::read_to_string(prefix.join("current/lib/rc")).unwrap(), "raw");
}

#[test]
fn archives_missing_required_files_are_rejected() {
    let scratch = Scratch::new("required");
    let prefix = scratch.path("prefix");
    let cases = [
        ("rustc", "bin/rustc", "missing bin/rustc"),
        ("cargo", "bin/cargo", "missing bin/cargo"),
        ("rustdoc", "bin/rustdoc", "missing bin/rustdoc"),
        ("manifest", "manifest.json", "no manifest.json"),
    ];
    for (name, omitted, needle) in cases {
        let (archive, sum) = toolchain(&scratch.root, name, &format!("{{\"id\":\"{name}\"}}\n"), "one", &[omitted]);
        assert_rejected(&prefix, install::install(&prefix, &archive, &sum), needle);
    }
}

#[test]
fn an_unwritable_prefix_is_an_error_not_a_panic() {
    struct Restore(PathBuf);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
        }
    }

    let scratch = Scratch::new("readonly");
    let prefix = scratch.path("prefix");
    fs::create_dir_all(&prefix).unwrap();
    let _restore = Restore(prefix.clone());
    fs::set_permissions(&prefix, fs::Permissions::from_mode(0o555)).unwrap();
    if fs::write(prefix.join("probe"), b"").is_ok() {
        // Running as root: permissions do not apply.
        return;
    }
    let (archive, sum) = simple(&scratch.root, "first", "one");
    let err = install::install(&prefix, &archive, &sum).unwrap_err();
    assert!(err.message.contains("writable"), "{}", err.message);
    assert!(install::rollback(&prefix).is_err());
    assert!(names(&prefix).is_empty());
}

fn assert_consistent(prefix: &Path, ids: &[&str]) {
    let current = install::current_version(prefix).unwrap().expect("current is set");
    assert_eq!(state_field(prefix, "current").as_deref(), Some(current.as_str()));
    let previous = state_field(prefix, "previous").expect("previous is set");
    assert_ne!(previous, current);
    assert!(ids.contains(&current.as_str()) && ids.contains(&previous.as_str()));
    assert_eq!(rustc(prefix), current);
    assert_eq!(versions(prefix), ids);
    assert_clean(prefix);
}

#[test]
fn concurrent_installs_in_threads_leave_a_consistent_prefix() {
    let scratch = Scratch::new("threads");
    let prefix = scratch.path("prefix");
    let ids = ["c0", "c1", "c2", "c3"];
    let archives: Vec<(PathBuf, PathBuf)> = ids.iter().map(|id| simple(&scratch.root, id, id)).collect();
    let prefix_ref = &prefix;
    std::thread::scope(|scope| {
        let handles: Vec<_> = archives.iter().map(|(archive, sum)| scope.spawn(move || install::install(prefix_ref, archive, sum))).collect();
        for handle in handles {
            handle.join().unwrap().unwrap();
        }
    });
    assert_consistent(&prefix, &ids);
}

#[test]
fn concurrent_installs_in_processes_leave_a_consistent_prefix() {
    let scratch = Scratch::new("processes");
    let prefix = scratch.path("prefix");
    let ids = ["p0", "p1", "p2"];
    let archives: Vec<(PathBuf, PathBuf)> = ids.iter().map(|id| simple(&scratch.root, id, id)).collect();
    let children: Vec<_> = archives
        .iter()
        .map(|(archive, sum)| {
            installer()
                .args(["install", "--prefix"])
                .arg(&prefix)
                .arg("--archive")
                .arg(archive)
                .arg("--checksum")
                .arg(sum)
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    for child in children {
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }
    assert_consistent(&prefix, &ids);
}

fn installer() -> Command {
    Command::new(env!("CARGO_BIN_EXE_frust-install"))
}

fn run(args: &[&str]) -> Output {
    installer().args(args).output().unwrap()
}

#[test]
fn usage_errors_exit_with_two_and_explain_on_stderr() {
    let cases: [&[&str]; 8] = [
        &["frobnicate", "--prefix", "p"],
        &["frobnicate"],
        &["install", "--prefix"],
        &["rollback", "--prefix"],
        &["install", "--prefix", "p", "--archive", "a"],
        &["stage", "--prefix", "p", "--checksum", "c"],
        &["rollback", "--prefix", "p", "--bogus", "x"],
        &["rollback", "--prefix", "p", "extra"],
    ];
    for args in cases {
        let output = run(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(!output.stderr.is_empty(), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
    }
}

#[test]
fn operational_errors_exit_with_one_and_report_on_stderr() {
    let scratch = Scratch::new("exit-one");
    let missing = scratch.path("missing");
    let prefix_arg = missing.to_str().unwrap();
    let output = run(&["rollback", "--prefix", prefix_arg]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).starts_with("error: "));
    assert!(output.stdout.is_empty());

    let (archive, sum) = simple(&scratch.root, "first", "one");
    let bad = scratch.path("bad.sha256");
    fs::write(&bad, "0".repeat(64)).unwrap();
    let output = run(&["install", "--prefix", prefix_arg, "--archive", archive.to_str().unwrap(), "--checksum", bad.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("checksum mismatch"));

    let gone = scratch.path("gone.tar");
    let output = run(&["install", "--prefix", prefix_arg, "--archive", gone.to_str().unwrap(), "--checksum", sum.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(1));
}

#[test]
fn the_binary_installs_rolls_back_and_uninstalls() {
    let scratch = Scratch::new("binary");
    let prefix = scratch.path("prefix");
    let prefix_arg = prefix.to_str().unwrap();
    let (first, first_sum) = simple(&scratch.root, "first", "one");
    let (second, second_sum) = simple(&scratch.root, "second", "two");
    for (archive, sum, expected) in [(&first, &first_sum, "current=first\n"), (&second, &second_sum, "current=second\n")] {
        let output = run(&["install", "--prefix", prefix_arg, "--archive", archive.to_str().unwrap(), "--checksum", sum.to_str().unwrap()]);
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(String::from_utf8_lossy(&output.stdout), expected);
    }
    let output = run(&["rollback", "--prefix", prefix_arg]);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "current=first\n");
    assert_eq!(rustc(&prefix), "one");
    assert!(run(&["uninstall", "--prefix", prefix_arg]).status.success());
    assert!(run(&["uninstall", "--prefix", prefix_arg]).status.success());
    assert!(!prefix.join("versions").exists());
}
