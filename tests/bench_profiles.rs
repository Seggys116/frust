//! Release profile benchmarks must prove compilation, restoration and runtime equivalence.

#![cfg(unix)]

mod common;

use common::{Sandbox, assert_success, stdout};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

#[test]
fn growth_reports_fixed_maintenance_policy_despite_caller_overrides() {
    let sb = Sandbox::new("bench-growth-policy");
    let output = sb
        .bin("frust")
        .args(["bench", "--growth", "--edits=1", "--libs=2", "--variants=cargo,auto", "--keep-scratch", "--json"])
        .env("TMPDIR", &sb.root)
        .env("TMP", &sb.root)
        .env("TEMP", &sb.root)
        .env("FRUST_CACHE", "off")
        .env("FRUST_AUTO_COMPACT", "off")
        .env("FRUST_COMPACT_KEEP_DAYS", "0")
        .env("FRUST_COMPACT_INCREMENTAL_IDLE_DAYS", "0")
        .env("FRUST_TARGET_MAX_GIB", "0.001")
        .output()
        .unwrap();
    assert_success(&output);
    let report: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(report["evidence_schema_version"], 2);
    let policy = &report["auto_compaction_policy"];
    assert_eq!(policy["enabled"], true);
    assert_eq!(policy["cache_enabled"], true);
    assert_eq!(policy["keep_other_profile_seconds"], 14 * 86_400);
    assert_eq!(policy["incremental_idle_seconds"], 7 * 86_400);
    assert_eq!(policy["max_target_bytes"], 8u64 * 1024 * 1024 * 1024);
    assert_eq!(policy["pass_budget_ms"], 2000);
    assert_eq!(policy["pass_bytes"], 512 * 1024 * 1024);
    assert_eq!(report["stopped"].as_array().unwrap().len(), 0);
    let repeats = report["repeat_builds"].as_array().unwrap();
    assert_eq!(repeats.len(), 2);
    for repeat in repeats {
        let build = &repeat[1];
        assert_eq!(build["cargo_compile_notices"], 0);
        assert_eq!(build["cache_hits"], 0);
        assert_eq!(build["cache_misses"], 0);
        assert_eq!(build["rustc_invocations"].as_array().unwrap().len(), 0);
        assert_eq!(build["rustc_compilation_counts"]["total"], 0);
        assert_no_backend_calls(build);
        assert!(build.get("cargo_scheduled_units").is_none());
    }
    assert!(report.get("repeat_compiled").is_none());
    assert_eq!(report["edit_policy"], "immediate");
    assert!(report["source_settle_timeout_ms"].is_null());
    for row in report["rows"].as_array().unwrap() {
        assert_managed_allocation(&row["managed_allocation"], row["variant"] != "Cargo");
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let retained = stderr.lines().find_map(|line| line.strip_prefix("frust bench: retained scratch at ")).unwrap();
    let retained = std::path::Path::new(retained);
    assert!(retained.starts_with(&sb.root) && retained.is_dir());
    assert!(retained.join("growth-cargo-target/debug/app").is_file());
    assert!(retained.join("growth-home-cargo").is_dir());
    let home = retained.join("growth-home-frust-auto");
    let dirs = frust::paths::Dirs::at(&home);
    assert!(dirs.store().is_dir());
    assert!(find_retained_file(&dirs.store(), "meta.json").is_file());
    assert!(find_retained_file(&dirs.targets(), "app").is_file());
    let project = retained.join("growth/app");
    assert!(project.join("src/main.rs").is_file());
}

// Plain frust isolates the writer retention policy from Auto's stricter incremental-entry pruning.
#[test]
fn settled_growth_caches_edited_sources_retires_versions_and_keeps_final_build_fresh() {
    let sb = Sandbox::new("bench-growth-settled");
    let output = sb.bin("frust")
        .args(["bench", "--growth", "--settle-edits", "--edits=13", "--every=1", "--libs=2", "--variants=cargo,frust,auto", "--keep-scratch", "--json"])
        .env("TMPDIR", &sb.root).env("TMP", &sb.root).env("TEMP", &sb.root).output().unwrap();
    assert_success(&output);
    let report: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(report["edit_policy"], "settled");
    assert_eq!(report["source_settle_timeout_ms"], 15_000);
    for variant in report["source_settlements"].as_array().unwrap() {
        let checks = variant[1].as_array().unwrap();
        assert_eq!(checks.len(), 14);
        assert!(checks.iter().all(|check| check["sources"] == 3 && check["minimum_settle_slack_ns"].as_i64().unwrap() > 0));
    }
    let rows = report["rows"].as_array().unwrap();
    let last = rows.iter().rfind(|row| row["variant"] == "Frust").unwrap();
    let evidence = &last["cache_evidence"];
    assert!(evidence["observed_entries_by_crate"]["lib0"].as_u64().unwrap() >= 6);
    assert!(evidence["nonzero_byte_miss_records"].as_u64().unwrap() > 0);
    assert_eq!(evidence["zero_byte_miss_records"], 0);
    assert_eq!(evidence["max_retained_keys_per_unit"], 4, "writer-only retention evidence: {evidence}");
    assert_eq!(evidence["retained_keys_per_unit_limit"], 4);
    assert_eq!(evidence["retired_payload_entries"], 0);
    assert_eq!(last["managed_allocation"]["retired_payload_bytes"], 0);
    assert!(evidence["observed_entry_keys"].as_u64().unwrap() > evidence["retained_complete_entries"].as_u64().unwrap());
    assert!(rows.iter().filter(|row| row["variant"] == "Cargo").all(|row| row["cache_evidence"].is_null()));
    for repeat in report["repeat_builds"].as_array().unwrap() {
        let build = &repeat[1];
        for field in ["cargo_compile_notices", "cache_hits", "cache_misses"] { assert_eq!(build[field], 0); }
        assert!(build["rustc_invocations"].as_array().unwrap().is_empty());
        assert_no_backend_calls(build);
    }
    let retained = String::from_utf8_lossy(&output.stderr).lines()
        .find_map(|line| line.strip_prefix("frust bench: retained scratch at ")).unwrap().to_string();
    let retained = std::path::Path::new(&retained);
    assert!(retained.starts_with(&sb.root));
    for (variant, name) in [("Frust", "growth-home-frust"), ("Auto", "growth-home-frust-auto")] {
        let row = rows.iter().rfind(|row| row["variant"] == variant).unwrap();
        assert_eq!(row["edits"], 13);
        assert_retained_growth_snapshot(&retained.join(name), &row["cache_evidence"], variant == "Auto");
    }
    let home = retained.join("growth-home-frust");
    let source = retained.join("growth/app/libs/lib0/src/lib.rs");
    let expected = frust::cache::hash_file(&source).unwrap();
    let mut latest_cached = false;
    let mut lib0_records = std::collections::HashSet::new();
    for shard in fs::read_dir(home.join("manifests")).unwrap() {
        for manifest in fs::read_dir(shard.unwrap().path()).unwrap() {
            let value: serde_json::Value = serde_json::from_slice(&fs::read(manifest.unwrap().path()).unwrap()).unwrap();
            for candidate in value["candidates"].as_array().unwrap() {
                let key = candidate["key"].as_str().unwrap();
                let entry = home.join("store").join(&key[..2]).join(key);
                let Ok(bytes) = fs::read(entry.join("meta.json")) else { continue };
                let meta: frust::cache::EntryMeta = serde_json::from_slice(&bytes).unwrap();
                if meta.crate_name != "lib0" { continue; }
                lib0_records.insert(key.to_string());
                if candidate["inputs"].as_array().unwrap().iter().any(|input| input["path"] == "libs/lib0/src/lib.rs" && input["hash"] == expected) {
                    for group in frust::cache::metadata_slice::validate_groups(&meta.files).unwrap() {
                        frust::cache::metadata_slice::verify_group(&entry, &meta.files, &group).unwrap();
                    }
                    latest_cached = true;
                }
            }
        }
    }
    assert!(latest_cached, "the final edited source must have a current, verified cache entry");
    assert_eq!(lib0_records.len(), 4, "only the four retained lib0 versions remain in the store");
    let observed: Vec<_> = evidence["observed_entry_keys_by_crate"]["lib0"].as_array().unwrap()
        .iter().map(|key| key.as_str().unwrap()).collect();
    assert!(observed.len() >= 6 && observed.is_sorted());
    let retired: Vec<_> = observed.iter().filter(|key| !lib0_records.contains(**key)).collect();
    assert!(retired.len() >= 2, "named versions observed during prior builds must no longer be live");
    for key in retired {
        assert!(!home.join("store").join(&key[..2]).join(key).exists(), "retired observed key {key} still has payload");
    }
}

// Enumerate the retained fixture after the final no-op, independently of the collector's pre-pass history.
fn assert_retained_growth_snapshot(home: &std::path::Path, evidence: &serde_json::Value, auto: bool) {
    let mut complete = std::collections::HashSet::new();
    let mut incremental_units = std::collections::HashMap::<String, usize>::new();
    for shard in fs::read_dir(home.join("store")).unwrap() {
        for entry in fs::read_dir(shard.unwrap().path()).unwrap() {
            let entry = entry.unwrap().path();
            let metadata: frust::cache::EntryMeta = serde_json::from_slice(&fs::read(entry.join("meta.json")).unwrap()).unwrap();
            assert!(entry.join(&metadata.stderr).is_file());
            for group in frust::cache::metadata_slice::validate_groups(&metadata.files).unwrap() {
                let backing = &metadata.files[group.base_index];
                let path = entry.join(&backing.stored);
                assert!(if backing.dir { path.is_dir() } else { path.is_file() });
            }
            if let Some(unit) = metadata.incremental_unit { *incremental_units.entry(unit).or_default() += 1; }
            complete.insert(entry.file_name().unwrap().to_str().unwrap().to_owned());
        }
    }
    let mut unit_records = 0;
    let mut unit_keys = 0;
    let mut missing = 0;
    let mut maximum = 0;
    for shard in fs::read_dir(home.join(frust::cache::UNITS_DIR)).unwrap() {
        for record in fs::read_dir(shard.unwrap().path()).unwrap() {
            let text = fs::read_to_string(record.unwrap().path()).unwrap();
            let keys: Vec<_> = text.lines().collect();
            assert!(keys.iter().all(|key| frust::cache::valid_key(key)));
            unit_records += 1;
            unit_keys += keys.len();
            missing += keys.iter().filter(|key| !complete.contains(**key)).count();
            maximum = maximum.max(keys.len());
        }
    }
    let dirs = frust::paths::Dirs::at(home);
    let retired = match fs::read_dir(frust::cache::evicted_dir(&dirs)) {
        Ok(mut entries) => entries.try_fold(0usize, |count, entry| entry.map(|_| count + 1)).unwrap(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => panic!("cannot inspect retained payload queue: {error}"),
    };
    for (field, expected) in [
        ("retained_complete_entries", complete.len()), ("retained_unit_records", unit_records),
        ("retained_unit_keys", unit_keys), ("retained_unit_missing_entries", missing),
        ("max_retained_keys_per_unit", maximum), ("retained_keys_per_unit_limit", 4),
        ("retired_payload_entries", retired),
    ] { assert_eq!(evidence[field].as_u64().unwrap(), expected as u64, "{field}: {evidence}"); }
    if auto {
        assert!(!incremental_units.is_empty());
        assert!(incremental_units.values().all(|count| *count <= 2), "Auto must retain at most two actual states per incremental unit");
        assert!(evidence["observed_entry_keys"].as_u64().unwrap() > complete.len() as u64);
    }
}

#[test]
fn settled_growth_flag_rejects_unrelated_or_malformed_modes_before_building() {
    let sb = Sandbox::new("bench-growth-settle-flags");
    for flags in [vec!["--settle-edits"], vec!["--profiles", "--settle-edits"],
        vec!["--growth", "--profiles", "--settle-edits"], vec!["--growth", "--settle-edits=true"],
        vec!["--growth", "--settle-edits", "--settle-edits"]] {
        let output = sb.bin("frust").arg("bench").args(&flags).output().unwrap();
        assert!(!output.status.success(), "{flags:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("--settle-edits"));
        assert!(output.stdout.is_empty());
    }
}

fn find_retained_file(root: &std::path::Path, name: &str) -> std::path::PathBuf {
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                pending.push(entry.path());
            } else if entry.file_name() == name {
                return entry.path();
            }
        }
    }
    panic!("missing {name} under {}", root.display());
}

#[test]
fn release_profiles_prove_real_compiles_restores_and_observable_edits() {
    let sb = Sandbox::new("bench-profiles");
    let output = sb
        .bin("frust")
        .args(["bench", "--profiles", "--quick", "--runs=3", "--compression=off", "--json"])
        .env("RUSTC_WORKSPACE_WRAPPER", "/nonexistent/benchmark-must-clear-this")
        .env("CARGO_ENCODED_RUSTFLAGS", "--invalid-benchmark-flag")
        .env("FRUST_CACHE", "off")
        .output()
        .unwrap();
    assert_success(&output);
    let report: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(report["evidence_schema_version"], 2);
    assert_eq!(report["cache_compression"], "off");
    assert_eq!(report["store_policy"], "sync");
    assert_eq!(report["compiler_audit_available"], true);
    assert_eq!(report["registry_deps"], false);
    let profiles = report["profiles"].as_array().unwrap();
    assert_eq!(profiles.len(), 3);
    for (profile, name) in profiles.iter().zip(["release", "release-thin-lto", "release-fat-lto"]) {
        assert_eq!(profile["name"], name);
        assert_eq!(profile["same_directory_binaries_identical"], true);
        assert_eq!(profile["restored_binaries_identical"], true);
        assert_eq!(profile["runtime_outputs_equal"], true);
        assert_managed_allocation(&profile["cargo_managed_allocation"], false);
        assert_managed_allocation(&profile["frust_managed_allocation"], true);
        let samples = profile["samples"].as_array().unwrap();
        assert_eq!(samples.len(), 24);
        for sample in samples {
            assert_managed_allocation(&sample["managed_allocation"], sample["tool"] == "frust");
            assert_compiler_call_counts(&sample["build"]);
            let invocations = sample["build"]["rustc_invocations"].as_array().unwrap();
            let counts = &sample["build"]["rustc_compilation_counts"];
            assert_eq!(counts["total"].as_u64().unwrap() as usize, invocations.len());
            assert_eq!(counts["unknown"], 0);
            let compile =
                sample["phase"] == "cold" || sample["phase"] == "edit" || sample["phase"] == "clean rebuild" && sample["tool"] == "cargo";
            assert_eq!(counts["cargo_units"].as_u64().unwrap() > 0, compile, "{sample}");
            if sample["phase"] == "clean rebuild" && sample["tool"] == "frust" {
                assert!(sample["build"]["cache_hits"].as_u64().unwrap() > 0);
                assert_eq!(sample["build"]["cache_misses"], 0);
                assert_eq!(counts["cargo_units"], 0);
            }
            if sample["phase"] == "no-op" {
                assert_eq!(sample["build"]["cargo_compile_notices"], 0);
                assert_eq!(sample["build"]["cache_hits"], 0);
                assert_eq!(sample["build"]["cache_misses"], 0);
                assert_eq!(counts["total"], 0);
                assert_no_backend_calls(&sample["build"]);
            }
            let edit = sample["expected_edit"].as_u64().unwrap();
            assert!(sample["stdout"].as_str().unwrap().contains(&format!("frust-bench-edit {edit}\n")));
        }
        assert_eq!(samples[0]["tool"], "cargo");
        assert_eq!(samples[8]["tool"], "frust");
    }
}

#[test]
fn store_policy_rejects_invalid_values_and_unrelated_modes_before_building() {
    let sb = Sandbox::new("bench-store-flags");
    for flags in [
        vec!["--profiles", "--store=invalid"],
        vec!["--profiles", "--store"],
        vec!["--store=invalid"],
        vec!["--store"],
        vec!["--growth", "--store=deferred"],
        vec!["--churn", "--store=sync"],
        vec!["--linking", "--store=deferred"],
        vec!["--latency", "--store=sync"],
        vec!["--profiles", "--linking", "--store=sync"],
        vec!["--profiles", "--store=sync", "--store=deferred"],
    ] {
        let output = sb.bin("frust").arg("bench").args(&flags).output().unwrap();
        assert!(!output.status.success(), "{flags:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("--store"), "{output:?}");
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn deferred_slot_child() {
    let Some(path) = std::env::var_os("FRUST_BENCH_TEST_SLOT") else {
        return;
    };
    let file = File::options().read(true).write(true).open(path).unwrap();
    frust::sys::lock(&file, frust::sys::LockKind::Exclusive).unwrap();
    println!("slot held");
    std::io::stdout().flush().unwrap();
    let _ = std::io::stdin().read(&mut [0]);
    frust::sys::unlock(&file).unwrap();
}

// A surviving job protects the Windows relock gap even when every slot has become available.
#[test]
fn deferred_drain_requires_both_job_absence_and_authoritative_slot_locks() {
    let sb = Sandbox::new("bench-store-drain");
    let deferred = frust::paths::Dirs::at(sb.home()).deferred();
    fs::create_dir_all(&deferred).unwrap();
    let slot = deferred.join("slot-0");
    File::create(&slot).unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "deferred_slot_child", "--nocapture"])
        .env("FRUST_BENCH_TEST_SLOT", &slot)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    loop {
        let mut line = String::new();
        assert!(reader.read_line(&mut line).unwrap() > 0);
        if line.trim_end().ends_with("slot held") {
            break;
        }
    }
    assert!(frust::bench::drain_deferred_stores(&sb.home(), Duration::from_millis(20)).is_err());
    let job = deferred.join("job-123-456.json");
    fs::write(&job, b"pending store").unwrap();
    drop(child.stdin.take());
    assert!(child.wait().unwrap().success());
    assert!(frust::bench::drain_deferred_stores(&sb.home(), Duration::from_millis(20)).is_err());
    assert_eq!(fs::read(&job).unwrap(), b"pending store");
    fs::remove_file(&job).unwrap();
    frust::bench::drain_deferred_stores(&sb.home(), Duration::ZERO).unwrap();
    let slot = File::options().read(true).write(true).open(slot).unwrap();
    frust::sys::try_lock(&slot, frust::sys::LockKind::Exclusive).unwrap();
}

// Deferred reports prove usable completed entries without inferring how many helpers ran or fell back.
#[test]
fn deferred_release_profiles_report_foreground_and_settled_completion() {
    let sb = Sandbox::new("bench-profiles-deferred");
    let output = sb
        .bin("frust")
        .args(["bench", "--profiles", "--quick", "--runs=3", "--store=deferred", "--keep-scratch", "--json"])
        .env("TMPDIR", &sb.root)
        .env("TMP", &sb.root)
        .env("TEMP", &sb.root)
        .output()
        .unwrap();
    assert_success(&output);
    let report: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(report["store_policy"], "deferred");
    assert_eq!(report["profiles"].as_array().unwrap().len(), 3);
    for profile in report["profiles"].as_array().unwrap() {
        assert_eq!(profile["same_directory_binaries_identical"], true);
        assert_eq!(profile["restored_binaries_identical"], true);
        assert_eq!(profile["runtime_outputs_equal"], true);
        for sample in profile["samples"].as_array().unwrap() {
            let build = &sample["build"];
            assert_compiler_call_counts(build);
            if sample["phase"] == "no-op" { assert_no_backend_calls(build); }
            if sample["tool"] == "frust" {
                let foreground = build["millis"].as_u64().unwrap();
                let drain = build["store_drain_millis"].as_u64().unwrap();
                let settled = build["settled_millis"].as_u64().unwrap();
                assert!(settled >= foreground && settled >= drain);
                assert!((settled as i128 - foreground as i128 - drain as i128).abs() <= 1);
                if sample["phase"] == "clean rebuild" {
                    assert_eq!(build["rustc_compilation_counts"]["cargo_units"], 0);
                    assert_eq!(build["rustc_compilation_counts"]["unknown"], 0);
                    assert!(build["cache_hits"].as_u64().unwrap() > 0);
                    assert_eq!(build["cache_misses"], 0);
                }
            } else {
                assert!(build.get("store_drain_millis").is_none());
                assert!(build.get("settled_millis").is_none());
            }
        }
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let retained = stderr.lines().find_map(|line| line.strip_prefix("frust bench: retained scratch at ")).unwrap();
    let retained = std::path::Path::new(retained);
    assert!(retained.starts_with(&sb.root) && retained.is_dir());
    frust::bench::drain_deferred_stores(&retained.join("profile-matrix/home"), Duration::ZERO).unwrap();
}

#[test]
fn deferred_general_benchmark_drains_builds_and_preserves_synchronous_churn() {
    let sb = Sandbox::new("bench-general-deferred");
    let output = sb.bin("frust")
        .args(["bench", "--quick", "--runs=3", "--store=deferred", "--compression=off", "--keep-scratch", "--json"])
        .env("TMPDIR", &sb.root).env("TMP", &sb.root).env("TEMP", &sb.root)
        .output().unwrap();
    assert_success(&output);
    let report: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(report["store_policy"], "deferred");
    assert!(report["store_completion_method"].as_str().unwrap().contains("foreground"));
    assert_eq!(report["registry_deps"], false);
    assert_eq!(report["churn"]["store_policy"], "sync");
    for field in ["release_binary_identical_cold_vs_restored", "release_binary_identical_to_cargo", "runtime_outputs_equal"] {
        assert_eq!(report[field], true, "{field}");
    }
    // Inspect build samples independently of the median-only presentation.
    let cases = report["cases"].as_array().unwrap();
    for case in cases {
        let label = case["label"].as_str().unwrap();
        let frust = label.starts_with("frust");
        let maintenance = label.contains("maintenance:");
        assert_eq!(case["samples"].as_array().unwrap().len(), if maintenance { 1 } else { 3 });
        for build in case["samples"].as_array().unwrap() {
            assert_store_completion(build, frust && !maintenance);
            if maintenance { continue; }
            assert_compiler_call_counts(build);
            assert_eq!(build["rustc_compilation_counts"]["unknown"], 0, "{label}: {build}");
            if label.contains("cold") || label.contains("incremental edit") {
                assert!(build["rustc_compilation_counts"]["cargo_units"].as_u64().unwrap() > 0, "{label}: {build}");
            }
            if label.contains("clean rebuild") && frust {
                assert_warm_restore(build);
            }
            if label.contains("no-op") {
                assert_eq!(build["rustc_compilation_counts"]["total"], 0);
                assert_no_backend_calls(build);
                for field in ["cargo_compile_notices", "cache_hits", "cache_misses"] { assert_eq!(build[field], 0); }
            }
        }
    }
    assert!(cases.iter().any(|case| case["label"].as_str().unwrap().contains("incremental edit, warm session")));
    assert_eq!(report["profile_matrix"]["store_policy"], "deferred");
    for profile in report["profile_matrix"]["profiles"].as_array().unwrap() {
        for sample in profile["samples"].as_array().unwrap() {
            let frust = sample["tool"] == "frust";
            assert_compiler_call_counts(&sample["build"]);
            if sample["phase"] == "no-op" { assert_no_backend_calls(&sample["build"]); }
            assert_store_completion(&sample["build"], frust);
            if frust && sample["phase"] == "clean rebuild" { assert_warm_restore(&sample["build"]); }
        }
    }
    for repeat in report["churn"]["growth"]["repeat_builds"].as_array().unwrap() {
        assert_store_completion(&repeat[1], false);
        assert_no_backend_calls(&repeat[1]);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let retained = std::path::Path::new(stderr.lines()
        .find_map(|line| line.strip_prefix("frust bench: retained scratch at ")).unwrap());
    assert!(retained.starts_with(&sb.root) && retained.is_dir());
    for home in ["frust-home", "profile-matrix/home", "churn/frust-home", "churn/frust-auto-home"] {
        frust::bench::drain_deferred_stores(&retained.join(home), Duration::ZERO).unwrap();
    }
}

fn assert_store_completion(build: &serde_json::Value, deferred: bool) {
    if deferred {
        let foreground = build["millis"].as_u64().unwrap();
        let drain = build["store_drain_millis"].as_u64().unwrap();
        let settled = build["settled_millis"].as_u64().unwrap();
        assert!(settled >= foreground && settled >= drain);
        assert!((settled as i128 - foreground as i128 - drain as i128).abs() <= 1);
    } else {
        assert!(build.get("store_drain_millis").is_none());
        assert!(build.get("settled_millis").is_none());
    }
}

fn assert_compiler_call_counts(build: &serde_json::Value) {
    let compilations = build["rustc_invocations"].as_array().unwrap().len();
    let queries = build["rustc_query_invocations"].as_array().unwrap().len();
    assert_eq!(build["rustc_compilation_counts"]["total"].as_u64().unwrap() as usize, compilations);
    assert_eq!(build["rustc_call_count"].as_u64().unwrap() as usize, compilations + queries);
}

fn assert_no_backend_calls(build: &serde_json::Value) {
    assert_compiler_call_counts(build);
    assert_eq!(build["rustc_call_count"], 0);
    assert!(build["rustc_query_invocations"].as_array().unwrap().is_empty());
}

fn assert_warm_restore(build: &serde_json::Value) {
    assert_eq!(build["rustc_compilation_counts"]["cargo_units"], 0);
    assert_eq!(build["rustc_compilation_counts"]["unknown"], 0);
    assert!(build["cache_hits"].as_u64().unwrap() > 0);
    assert_eq!(build["cache_misses"], 0);
}

#[test]
fn failed_deferred_benchmarks_retain_scratch_without_keep_flag() {
    let sb = Sandbox::new("bench-deferred-failure");
    for profiles in [false, true] {
        let mut command = sb.bin("frust");
        command.args(["bench", "--quick", "--store=deferred", "--json"]);
        if profiles { command.arg("--profiles"); }
        // A missing compiler exercises failure retention before any helper starts.
        let output = command.env("RUSTC", sb.root.join("missing-rustc"))
            .env("TMPDIR", &sb.root).env("TMP", &sb.root).env("TEMP", &sb.root)
            .output().unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("could not query rustc"), "{stderr}");
        let retained = std::path::Path::new(stderr.lines()
            .find_map(|line| line.strip_prefix("frust bench: retained scratch at ")).unwrap());
        assert!(retained.starts_with(&sb.root) && retained.is_dir());
    }
}

fn assert_managed_allocation(allocation: &serde_json::Value, cache_home: bool) {
    assert_eq!(allocation["status"], "available", "{allocation}");
    assert_eq!(allocation["cache_home_applicable"], cache_home);
    assert!(allocation["error"].is_null());
    let total = allocation["total_managed_bytes"].as_u64().unwrap();
    let artifacts = allocation["target_plus_store_bytes"].as_u64().unwrap();
    assert!(total > 0 && total >= artifacts);
    if cache_home {
        let other = allocation["other_managed_bytes"].as_u64().unwrap();
        assert!(allocation["retired_payload_bytes"].as_u64().is_some());
        assert!(allocation["cache_home_bytes"].as_u64().unwrap() > 0);
        assert!(other > 0);
        assert_eq!(total, artifacts + other);
    } else {
        assert!(allocation["cache_home_bytes"].is_null());
        assert!(allocation["other_managed_bytes"].is_null());
        assert!(allocation["retired_payload_bytes"].is_null());
        assert_eq!(total, artifacts);
    }
}

// Manual inode sums include control directories, temporary files and symlinks without following their targets.
#[test]
fn checked_allocation_counts_complete_nested_and_external_unions_once() {
    let sb = Sandbox::new("bench-checked-allocation");
    let root = sb.root.join("allocation");
    let home = root.join("home");
    let target = home.join("targets/profile");
    let store = home.join("store");
    let external = root.join("external-target");
    let unrelated = root.join("other-variant");
    let directories = [home.clone(), home.join("targets"), target.clone(), store.clone(), home.join("manifests"), home.join("tmp")];
    for path in directories.iter().chain([&external, &unrelated]) {
        fs::create_dir_all(path).unwrap();
    }
    let payload = target.join("artifact");
    fs::write(&payload, vec![1; 8192]).unwrap();
    let stored = store.join("payload");
    let control_alias = home.join("manifests/shared-inode");
    let external_alias = external.join("binary");
    for path in [&stored, &control_alias, &external_alias] {
        fs::hard_link(&payload, path).unwrap();
    }
    let temporary = home.join("tmp/staging");
    fs::write(&temporary, vec![2; 4096]).unwrap();
    let outside = unrelated.join("excluded");
    fs::write(&outside, vec![3; 64 * 1024]).unwrap();
    let link = home.join("foreign-link");
    frust::sys::link_tool(&outside, &link).unwrap();
    let mut paths = directories.to_vec();
    paths.extend([payload, stored, control_alias, temporary, link]);
    let sum = |paths: &[std::path::PathBuf]| {
        let mut seen = std::collections::HashSet::new();
        paths
            .iter()
            .map(|path| frust::sys::lstat(path).unwrap())
            .filter(|meta| seen.insert((meta.dev(), meta.ino())))
            .map(|meta| meta.allocated())
            .sum::<u64>()
    };
    let expected = sum(&paths);
    assert_eq!(frust::compact::physical_size_checked(&[&home, &target, &store, &home]).unwrap(), expected);
    paths.extend([external.clone(), external_alias]);
    assert_eq!(frust::compact::physical_size_checked(&[&target, &home, &external, &store]).unwrap(), sum(&paths));
    assert_eq!(frust::compact::physical_size_checked(&[&home]).unwrap(), expected);
    assert!(frust::compact::physical_size_checked(&[&home, &root.join("missing")]).is_err());
}

#[test]
fn checked_allocation_reports_unreadable_directories_when_access_is_denied() {
    let sb = Sandbox::new("bench-allocation-unreadable");
    let directory = sb.root.join("private");
    fs::create_dir(&directory).unwrap();
    fs::write(directory.join("payload"), b"unreadable payload").unwrap();
    frust::sys::set_path_mode(&directory, 0o0).unwrap();
    let denied = fs::read_dir(&directory).is_err();
    let measured = frust::compact::physical_size_checked(&[&directory]);
    frust::sys::set_path_mode(&directory, 0o700).unwrap();
    if denied {
        assert!(measured.is_err());
    }
}
