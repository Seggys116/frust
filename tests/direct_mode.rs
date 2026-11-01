//! Direct mode: recorded input stamps speed up hits but never let a changed input through.

mod common;

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use common::{Sandbox, assert_success, run_binary, settle, stdout};

const LEAF_V1: &str = "pub fn value() -> u32 { 41 }\n";
const MAIN: &str = "fn main() { println!(\"{}\", leaf::value()); }\n";

fn manifests(sb: &Sandbox) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(shards) = fs::read_dir(sb.home().join("manifests")) else { return found };
    for shard in shards.flatten() {
        for file in fs::read_dir(shard.path()).into_iter().flatten().flatten() {
            if file.path().extension().is_some_and(|e| e == "json") {
                found.push(file.path());
            }
        }
    }
    found
}

fn manifest_values(sb: &Sandbox) -> Vec<serde_json::Value> {
    manifests(sb).iter().filter_map(|p| serde_json::from_slice(&fs::read(p).ok()?).ok()).collect()
}

fn recorded_stamps(sb: &Sandbox) -> usize {
    manifest_values(sb)
        .iter()
        .flat_map(|m| m["candidates"].as_array().cloned().unwrap_or_default())
        .flat_map(|c| c["inputs"].as_array().cloned().unwrap_or_default())
        .filter(|input| input.get("stamp").is_some_and(|s| s.is_object()))
        .count()
}

// A recorded stamp requires the file to have been idle for the settle margin before it was hashed.
fn wait_for_settle_margin() {
    std::thread::sleep(Duration::from_millis(2300));
}

fn build(sb: &Sandbox, project: &Path) {
    assert_success(&sb.bin("fcargo").arg("build").current_dir(project).output().unwrap());
}

fn clean(sb: &Sandbox, project: &Path) {
    assert_success(&sb.bin("fcargo").arg("clean").current_dir(project).output().unwrap());
}

fn app_output(sb: &Sandbox, project: &Path) -> String {
    let out = run_binary(&sb.central_target(project), "debug", "app");
    assert_success(&out);
    stdout(&out).trim().to_string()
}

#[test]
fn settled_inputs_are_recorded_with_stamps_and_serve_the_next_build() {
    let sb = Sandbox::new("dm-record");
    sb.vendor_crate("leaf", "1.0.0", LEAF_V1);
    let a = sb.project("a", &[("leaf", "1.0.0")], MAIN);
    wait_for_settle_margin();

    build(&sb, &a);
    assert_eq!(sb.count_cache("miss", "leaf"), 1);
    assert!(recorded_stamps(&sb) > 0, "settled inputs carry stamps: {:#?}", manifest_values(&sb));
    for manifest in manifest_values(&sb) {
        assert_eq!(manifest["format"], 5, "manifests are written in the current format");
    }

    clean(&sb, &a);
    build(&sb, &a);
    assert_eq!(sb.count_cache("hit", "leaf"), 1, "{:#?}", sb.cache_events());
    assert_eq!(app_output(&sb, &a), "41");
}

#[test]
fn an_edit_that_keeps_size_and_mtime_is_not_served_from_a_recorded_stamp() {
    let sb = Sandbox::new("dm-edit");
    sb.vendor_crate("leaf", "1.0.0", LEAF_V1);
    let a = sb.project("a", &[("leaf", "1.0.0")], MAIN);
    wait_for_settle_margin();
    build(&sb, &a);
    assert!(recorded_stamps(&sb) > 0);

    // The same length and the old modification time: only ctime and the content betray the edit.
    let source = sb.vendor().join("leaf-1.0.0").join("src").join("lib.rs");
    let mtime: SystemTime = fs::metadata(&source).unwrap().modified().unwrap();
    fs::write(&source, LEAF_V1.replace("41", "42")).unwrap();
    File::options().write(true).open(&source).unwrap().set_modified(mtime).unwrap();
    settle(&source);

    clean(&sb, &a);
    build(&sb, &a);
    assert_eq!(app_output(&sb, &a), "42", "the edited dependency must be recompiled");
    assert_eq!(sb.count_cache("hit", "leaf"), 0);
}

#[test]
fn the_kill_switch_verifies_every_input_by_content_and_still_hits() {
    let sb = Sandbox::new("dm-switch");
    sb.vendor_crate("leaf", "1.0.0", LEAF_V1);
    let a = sb.project("a", &[("leaf", "1.0.0")], MAIN);
    wait_for_settle_margin();
    build(&sb, &a);

    clean(&sb, &a);
    assert_success(&sb.bin("fcargo").arg("build").env("FRUST_CACHE_TRUST_STAMPS", "off").current_dir(&a).output().unwrap());
    assert_eq!(sb.count_cache("hit", "leaf"), 1, "content verification accepts an unchanged input");

    sb.replace_vendor_source("leaf", "1.0.0", &LEAF_V1.replace("41", "43"));
    clean(&sb, &a);
    assert_success(&sb.bin("fcargo").arg("build").env("FRUST_CACHE_TRUST_STAMPS", "off").current_dir(&a).output().unwrap());
    assert_eq!(app_output(&sb, &a), "43");
}

#[test]
fn manifests_in_an_older_format_are_ignored_and_rewritten() {
    let sb = Sandbox::new("dm-format");
    sb.vendor_crate("leaf", "1.0.0", LEAF_V1);
    let a = sb.project("a", &[("leaf", "1.0.0")], MAIN);
    wait_for_settle_margin();
    build(&sb, &a);

    for path in manifests(&sb) {
        let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        value.as_object_mut().unwrap().remove("format");
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    }
    clean(&sb, &a);
    build(&sb, &a);
    assert_eq!(sb.count_cache("hit", "leaf"), 0, "a manifest without the current format serves nothing");
    assert_eq!(app_output(&sb, &a), "41");
    for manifest in manifest_values(&sb) {
        assert_eq!(manifest["format"], 5, "the manifest is rewritten in the current format");
    }

    clean(&sb, &a);
    build(&sb, &a);
    assert_eq!(sb.count_cache("hit", "leaf"), 1, "the rewritten manifest serves the next build");
}

#[test]
fn a_row_whose_entry_was_evicted_does_not_serve_and_the_unit_recompiles() {
    let sb = Sandbox::new("dm-evicted");
    sb.vendor_crate("leaf", "1.0.0", LEAF_V1);
    let a = sb.project("a", &[("leaf", "1.0.0")], MAIN);
    wait_for_settle_margin();
    build(&sb, &a);

    let store = sb.home().join("store");
    for shard in fs::read_dir(&store).unwrap().flatten() {
        for entry in fs::read_dir(shard.path()).unwrap().flatten() {
            fs::remove_dir_all(entry.path()).unwrap();
        }
    }
    clean(&sb, &a);
    build(&sb, &a);
    assert_eq!(sb.count_cache("hit", "leaf"), 0);
    assert_eq!(sb.count_cache("miss", "leaf"), 2);
    assert_eq!(app_output(&sb, &a), "41");
}

fn build_in(sb: &Sandbox, project: &Path, mode: &str) {
    let out = sb.bin("fcargo").arg("build").env("FRUST_CACHE_TRUST_STAMPS", mode).current_dir(project).output().unwrap();
    assert_success(&out);
}

fn trust_dir(sb: &Sandbox) -> PathBuf {
    sb.home().join("stamp-trust")
}

fn violations(sb: &Sandbox) -> Vec<serde_json::Value> {
    let text = fs::read_to_string(trust_dir(sb).join("violations.jsonl")).unwrap_or_default();
    text.lines().map(|l| serde_json::from_str(l).unwrap()).collect()
}

fn quarantine_markers(sb: &Sandbox) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(trust_dir(sb)) else { return Vec::new() };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("quarantine-")))
        .collect()
}

// One byte is appended per input that was re-hashed against its stamp, so growth proves a stamp was consulted.
fn audits_passed(sb: &Sandbox) -> u64 {
    fs::metadata(trust_dir(sb).join("audits-passed")).map_or(0, |m| m.len())
}

fn leaf_source_dev(sb: &Sandbox) -> u64 {
    frust::sys::stat(&sb.vendor().join("leaf-1.0.0").join("src").join("lib.rs")).unwrap().dev()
}

// Rewrites the recorded content hash of every stamped `lib.rs` input and leaves the stamps themselves untouched.
fn forge_leaf_hashes(sb: &Sandbox) -> usize {
    let mut forged = 0;
    for path in manifests(sb) {
        let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        for candidate in value["candidates"].as_array_mut().unwrap() {
            for input in candidate["inputs"].as_array_mut().unwrap() {
                if input["path"].as_str().is_some_and(|p| Path::new(p).ends_with(Path::new("src").join("lib.rs"))) && input["stamp"].is_object() {
                    input["hash"] = serde_json::json!("0".repeat(64));
                    forged += 1;
                }
            }
        }
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    }
    forged
}

fn leaf_hash() -> String {
    blake3::hash(LEAF_V1.as_bytes()).to_hex().to_string()
}

fn doctor_line(sb: &Sandbox, project: &Path) -> String {
    let out = sb.bin("frust").arg("doctor").current_dir(project).output().unwrap();
    stdout(&out).lines().find(|l| l.contains("stamp trust")).unwrap_or_default().to_string()
}

#[test]
fn audit_mode_still_hits_and_reports_nothing() {
    let sb = Sandbox::new("dm-audit");
    sb.vendor_crate("leaf", "1.0.0", LEAF_V1);
    let a = sb.project("a", &[("leaf", "1.0.0")], MAIN);
    wait_for_settle_margin();

    build_in(&sb, &a, "audit");
    assert_eq!(sb.count_cache("miss", "leaf"), 1);
    assert!(recorded_stamps(&sb) > 0);
    clean(&sb, &a);
    build_in(&sb, &a, "audit");
    assert_eq!(sb.count_cache("hit", "leaf"), 1, "{:#?}", sb.cache_events());
    assert_eq!(app_output(&sb, &a), "41");
    assert!(violations(&sb).is_empty(), "honest stamps are never reported: {:?}", violations(&sb));
    assert!(quarantine_markers(&sb).is_empty());
    assert!(audits_passed(&sb) > 0, "audit mode compared the predicted stamps with fresh hashes");
}

#[test]
fn a_stamp_that_lies_forces_a_recompile_is_logged_and_quarantines_the_volume() {
    let sb = Sandbox::new("dm-violation");
    sb.vendor_crate("leaf", "1.0.0", LEAF_V1);
    let a = sb.project("a", &[("leaf", "1.0.0")], MAIN);
    wait_for_settle_margin();
    build_in(&sb, &a, "on");
    assert_eq!(sb.count_cache("miss", "leaf"), 1);
    assert!(forge_leaf_hashes(&sb) > 0, "the leaf's source carries a recorded stamp");

    clean(&sb, &a);
    build_in(&sb, &a, "on");
    assert_eq!(sb.count_cache("hit", "leaf"), 0, "a candidate whose stamp disagrees with the content is not served");
    assert_eq!(app_output(&sb, &a), "41");

    let logged = violations(&sb);
    assert_eq!(logged.len(), 1, "{logged:#?}");
    let dev = leaf_source_dev(&sb);
    assert_eq!(logged[0]["crate"], "leaf");
    assert_eq!(logged[0]["recorded"], "0".repeat(64));
    assert_eq!(logged[0]["fresh"], leaf_hash());
    assert_eq!(logged[0]["dev"], dev);
    assert!(logged[0]["input"].as_str().is_some_and(|p| Path::new(p).ends_with(Path::new("src").join("lib.rs"))));
    assert!(logged[0]["t"].as_u64().is_some_and(|t| t > 0));
    let markers = quarantine_markers(&sb);
    assert_eq!(markers.len(), 1, "{markers:?}");
    assert_eq!(markers[0].file_name().unwrap().to_string_lossy(), format!("quarantine-{dev}"));

    // The recompile stored a correct row, so the next build is served by content alone.
    for manifest in manifest_values(&sb) {
        for candidate in manifest["candidates"].as_array().unwrap() {
            for input in candidate["inputs"].as_array().unwrap() {
                if input["path"].as_str().is_some_and(|p| Path::new(p).ends_with(Path::new("src").join("lib.rs"))) {
                    assert_eq!(input["hash"], leaf_hash(), "the manifest was repaired");
                }
            }
        }
    }
    clean(&sb, &a);
    build_in(&sb, &a, "on");
    assert_eq!(sb.count_cache("hit", "leaf"), 1);
    assert_eq!(violations(&sb).len(), 1, "the repaired row raises no further violation");
    assert_eq!(app_output(&sb, &a), "41");
}

#[test]
fn a_quarantined_volume_reads_every_input_and_doctor_reports_it() {
    let sb = Sandbox::new("dm-quarantine");
    sb.vendor_crate("leaf", "1.0.0", LEAF_V1);
    let a = sb.project("a", &[("leaf", "1.0.0")], MAIN);
    wait_for_settle_margin();
    build_in(&sb, &a, "on");
    assert!(doctor_line(&sb, &a).contains("no volume quarantined"), "{}", doctor_line(&sb, &a));

    clean(&sb, &a);
    build_in(&sb, &a, "on");
    assert_eq!(sb.count_cache("hit", "leaf"), 1);
    let trusted = audits_passed(&sb);
    assert!(trusted > 0, "an unquarantined volume skips the read and audits the input instead");

    frust::cache::quarantine(&frust::paths::Dirs::at(sb.home()), leaf_source_dev(&sb));
    clean(&sb, &a);
    build_in(&sb, &a, "on");
    assert_eq!(sb.count_cache("hit", "leaf"), 2, "content hashing still accepts an unchanged input");
    assert_eq!(audits_passed(&sb), trusted, "no stamp was trusted, so no read was skipped and nothing was audited");
    let line = doctor_line(&sb, &a);
    assert!(line.contains("warn") && line.contains("volume(s) quarantined"), "{line}");

    for marker in quarantine_markers(&sb) {
        fs::remove_file(marker).unwrap();
    }
    clean(&sb, &a);
    build_in(&sb, &a, "on");
    assert_eq!(sb.count_cache("hit", "leaf"), 3);
    assert!(audits_passed(&sb) > trusted, "stamps are trusted again once the quarantine is gone");
    assert!(doctor_line(&sb, &a).contains("no volume quarantined"));
}

#[test]
fn with_stamp_trust_off_a_forged_record_is_just_a_miss_and_no_violation_is_recorded() {
    let sb = Sandbox::new("dm-off-forged");
    sb.vendor_crate("leaf", "1.0.0", LEAF_V1);
    let a = sb.project("a", &[("leaf", "1.0.0")], MAIN);
    wait_for_settle_margin();
    build_in(&sb, &a, "off");
    assert!(forge_leaf_hashes(&sb) > 0);

    clean(&sb, &a);
    build_in(&sb, &a, "off");
    assert_eq!(sb.count_cache("hit", "leaf"), 0, "the recorded hash disagrees with the content");
    assert_eq!(app_output(&sb, &a), "41");
    assert!(violations(&sb).is_empty());
    assert!(quarantine_markers(&sb).is_empty());
    assert_eq!(audits_passed(&sb), 0);
}

#[cfg(unix)]
#[test]
fn default_deferred_mode_publishes_reused_metadata_probe_outputs_before_returning() {
    use std::os::unix::fs::PermissionsExt;

    struct HelperGate {
        gate: PathBuf,
        jobs: PathBuf,
    }
    impl Drop for HelperGate {
        fn drop(&mut self) {
            let _ = fs::write(&self.gate, b"open");
            let begun = std::time::Instant::now();
            while fs::read_dir(&self.jobs).is_ok_and(|mut entries| {
                entries.any(|entry| entry.is_ok_and(|entry| entry.file_name().to_string_lossy().starts_with("job-")))
            }) && begun.elapsed() < Duration::from_secs(10) {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    let sb = Sandbox::new("default-deferred-metadata-probes");
    let bin = sb.root.join("bin");
    fs::create_dir(&bin).unwrap();
    let persona = bin.join("frustc");
    fs::copy(env!("CARGO_BIN_EXE_frustc"), &persona).unwrap();
    let helper = bin.join("frust-wrap");
    fs::write(&helper, r#"#!/bin/sh
printf 'helper\n' >> "$FRUST_PROBE_HELPER_CALLS"
attempts=0
while [ ! -f "$FRUST_PROBE_HELPER_GATE" ]; do
    attempts=$((attempts + 1))
    if [ "$attempts" -gt 3000 ]; then exit 2; fi
    sleep 0.01
done
exec "$FRUST_PROBE_REAL_HELPER" "$@"
"#).unwrap();
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).unwrap();
    let helper_calls = sb.root.join("helper-calls");
    let gate = HelperGate { gate: sb.root.join("helper-gate"), jobs: frust::paths::Dirs::at(sb.home()).deferred() };
    let compiler = sb.root.join("counted-rustc");
    let calls = sb.root.join("compiler-calls");
    fs::write(&compiler, r#"#!/bin/sh
for arg do
    case "$arg" in
        --emit|--emit=*) printf 'compile\n' >> "$FRUST_PROBE_CALLS"; break ;;
    esac
done
exec rustc "$@"
"#).unwrap();
    fs::set_permissions(&compiler, fs::Permissions::from_mode(0o755)).unwrap();
    let sources = [sb.root.join("location.rs"), sb.root.join("file.rs")];
    fs::write(&sources[0], "pub fn location() -> u32 { 11 }\n").unwrap();
    fs::write(&sources[1], "pub fn file() -> u64 { 22 }\n").unwrap();
    let failed = sb.root.join("failed.rs");
    fs::write(&failed, "compile_error!(\"intentional capability probe failure\");\n").unwrap();
    settle(&failed);
    let out_dir = sb.root.join("probe");
    let command = |source: &Path| {
        let mut command = sb.bin_path(persona.to_str().unwrap());
        command.args(["--cfg=procmacro2_build_probe", "--edition=2021", "--crate-type=lib", "--emit=dep-info,metadata", "--crate-name=proc_macro2"])
            .arg("--out-dir").arg(&out_dir).arg(source).current_dir(&sb.root)
            .env("FRUST_RUSTC", &compiler).env("FRUST_PROBE_CALLS", &calls)
            .env("FRUST_PROBE_HELPER_CALLS", &helper_calls).env("FRUST_PROBE_HELPER_GATE", &gate.gate)
            .env("FRUST_PROBE_REAL_HELPER", env!("CARGO_BIN_EXE_frust-wrap"))
            .env("FRUST_CACHE", "on").env("FRUST_CACHE_TRUST_STAMPS", "on").env("FRUST_DEBUG", "1")
            .env_remove("FRUST_STORE_SYNC").env_remove("FRUST_RUSTC_ID").env_remove("RUSTC_BOOTSTRAP");
        command
    };
    let count = || fs::read_to_string(&calls).unwrap_or_default().lines().count();
    let mut artifacts = Vec::new();
    for (index, source) in sources.iter().enumerate() {
        fs::create_dir(&out_dir).unwrap();
        let output = command(source).output().unwrap();
        assert_success(&output);
        artifacts.push((fs::read(out_dir.join("proc_macro2.d")).unwrap(), fs::read(out_dir.join("libproc_macro2.rmeta")).unwrap()));
        assert_eq!(manifest_values(&sb).len(), index + 1, "each probe must be published while helpers are gated: {}", common::stderr(&output));
        fs::remove_dir_all(&out_dir).unwrap();
    }
    assert_eq!(count(), 2);
    assert_ne!(artifacts[0].1, artifacts[1].1);
    // Build scripts may delete the probe directory after every call, including cache hits.
    for (source, expected) in sources.iter().zip(&artifacts) {
        fs::create_dir(&out_dir).unwrap();
        assert_success(&command(source).output().unwrap());
        assert_eq!(&(fs::read(out_dir.join("proc_macro2.d")).unwrap(), fs::read(out_dir.join("libproc_macro2.rmeta")).unwrap()), expected);
        fs::remove_dir_all(&out_dir).unwrap();
    }
    assert_eq!(count(), 2, "both successful probes restore without real compiler calls");
    for expected in [3, 4] {
        fs::create_dir(&out_dir).unwrap();
        let output = command(&failed).output().unwrap();
        assert!(!output.status.success());
        assert!(common::stderr(&output).contains("intentional capability probe failure"));
        assert_eq!(count(), expected);
        fs::remove_dir_all(&out_dir).unwrap();
    }
    assert!(!helper_calls.exists(), "metadata-only unhashed outputs are stored inline");
    assert_eq!(sb.count_cache("hit", "proc_macro2"), 2);
}

#[cfg(unix)]
#[test]
fn direct_metadata_probes_restore_shared_names_without_running_the_compiler() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Stdio};

    let sb = Sandbox::new("direct-metadata-probes");
    let out_dir = sb.root.join("probe");
    fs::create_dir(&out_dir).unwrap();
    let compiler = sb.root.join("counted-rustc");
    let calls = sb.root.join("compiler-calls");
    fs::write(&compiler, r#"#!/bin/sh
for arg do
    case "$arg" in
        --emit|--emit=*) printf '%s\n' compile >> "$FRUST_PROBE_CALLS"; break ;;
    esac
done
exec rustc "$@"
"#).unwrap();
    fs::set_permissions(&compiler, fs::Permissions::from_mode(0o755)).unwrap();
    let a = sb.root.join("location.rs");
    let b = sb.root.join("file.rs");
    let c = sb.root.join("third.rs");
    let sibling = sb.root.join("independent.rs");
    let failed = sb.root.join("failed.rs");
    for (source, text) in [
        (&a, "pub fn location() -> u32 { 11 }\n"),
        (&b, "pub fn file() -> u32 { 22 }\n"),
        (&c, "pub fn third() -> u32 { 33 }\n"),
        (&sibling, "pub fn independent() -> u32 { 44 }\n"),
        (&failed, "compile_error!(\"intentional capability probe failure\");\n"),
    ] {
        fs::write(source, text).unwrap();
    }
    settle(&failed);
    let command = |source: &Path, krate: &str| -> Command {
        let mut cmd = sb.bin("frustc");
        cmd.args(["--edition=2021", "--crate-type=lib", "--emit=dep-info,metadata", "--crate-name"])
            .arg(krate).arg("--out-dir").arg(&out_dir).arg(source).current_dir(&sb.root)
            .env("FRUST_RUSTC", &compiler).env("FRUST_PROBE_CALLS", &calls)
            .env("FRUST_CACHE", "on").env("FRUST_CACHE_TRUST_STAMPS", "off")
            .env_remove("FRUST_RUSTC_ID").env_remove("RUSTC_BOOTSTRAP");
        cmd
    };
    let count = || fs::read_to_string(&calls).unwrap_or_default().lines().count();
    let artifacts = || (fs::read(out_dir.join("foo.d")).unwrap(), fs::read(out_dir.join("libfoo.rmeta")).unwrap());

    assert_success(&command(&a, "foo").output().unwrap());
    let first = artifacts();
    assert_success(&command(&b, "foo").output().unwrap());
    let second = artifacts();
    assert_eq!(count(), 2);
    assert_ne!(first.1, second.1);
    assert_success(&command(&a, "foo").output().unwrap());
    assert_eq!(artifacts(), first);
    assert_success(&command(&b, "foo").output().unwrap());
    assert_eq!(artifacts(), second);
    assert_eq!(count(), 2, "each successful probe restores its own metadata without invoking rustc");

    fs::write(&a, "pub fn changed_location() -> u64 { 111 }\n").unwrap();
    settle(&a);
    assert_success(&command(&a, "foo").output().unwrap());
    assert_eq!(count(), 3, "an edited probe must compile again");
    assert_ne!(artifacts().1, first.1);

    // Distinct crates may share an out-dir without sharing their dep-info or metadata filenames.
    let foo = command(&c, "foo").stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let libfoo = command(&sibling, "libfoo").stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    assert_success(&foo.wait_with_output().unwrap());
    assert_success(&libfoo.wait_with_output().unwrap());
    assert_eq!(count(), 5);
    let sibling_metadata = fs::read(out_dir.join("liblibfoo.rmeta")).unwrap();
    for name in ["libfoo.d", "libfoo", "foo.notes", "libfoo.notes", "foo-extra.d", "foo_extra.d"] {
        fs::write(out_dir.join(name), b"independent sibling changed after the compile").unwrap();
    }
    fs::create_dir(out_dir.join("foo.dSYM")).unwrap();
    fs::write(out_dir.join("foo.dSYM/member"), b"independent bundle").unwrap();
    assert_success(&command(&b, "foo").output().unwrap());
    assert_eq!(artifacts(), second);
    assert_eq!(count(), 5);
    assert_eq!(fs::read(out_dir.join("liblibfoo.rmeta")).unwrap(), sibling_metadata);
    for name in ["libfoo.d", "libfoo", "foo.notes", "libfoo.notes", "foo-extra.d", "foo_extra.d"] {
        assert_eq!(fs::read(out_dir.join(name)).unwrap(), b"independent sibling changed after the compile");
    }
    assert_eq!(fs::read(out_dir.join("foo.dSYM/member")).unwrap(), b"independent bundle");

    for expected_calls in [6, 7] {
        let output = command(&failed, "foo").output().unwrap();
        assert!(!output.status.success());
        assert!(common::stderr(&output).contains("intentional capability probe failure"));
        assert_eq!(count(), expected_calls, "failed capability probes always execute the real compiler");
    }
    let mut repeated_emit = command(&a, "foo");
    repeated_emit.args(["--emit=link", "--emit=dep-info,metadata"]);
    assert_success(&repeated_emit.output().unwrap());
    assert_eq!(count(), 8);
    assert!(out_dir.join("libfoo.rlib").is_file(), "rustc merges repeated emit options");
    frust::sys::rename(&out_dir.join("libfoo.rlib"), &out_dir.join("previous-rlib")).unwrap();
    assert_success(&repeated_emit.output().unwrap());
    assert_eq!(count(), 9, "ambiguous repeated emits use the compiler again");
    assert!(out_dir.join("libfoo.rlib").is_file(), "the repeated compile must recreate its linked artifact");
    assert_eq!(sb.count_cache("hit", "foo"), 3);
}
