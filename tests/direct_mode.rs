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
        assert_eq!(manifest["format"], 3, "manifests are written in the current format");
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
        assert_eq!(manifest["format"], 3, "the manifest is rewritten in the current format");
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
                if input["path"].as_str().is_some_and(|p| p.ends_with("src/lib.rs")) && input["stamp"].is_object() {
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
    assert!(logged[0]["input"].as_str().is_some_and(|p| p.ends_with("src/lib.rs")));
    assert!(logged[0]["t"].as_u64().is_some_and(|t| t > 0));
    let markers = quarantine_markers(&sb);
    assert_eq!(markers.len(), 1, "{markers:?}");
    assert_eq!(markers[0].file_name().unwrap().to_string_lossy(), format!("quarantine-{dev}"));

    // The recompile stored a correct row, so the next build is served by content alone.
    for manifest in manifest_values(&sb) {
        for candidate in manifest["candidates"].as_array().unwrap() {
            for input in candidate["inputs"].as_array().unwrap() {
                if input["path"].as_str().is_some_and(|p| p.ends_with("src/lib.rs")) {
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
