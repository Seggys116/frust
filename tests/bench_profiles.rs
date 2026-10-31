//! Release profile benchmarks must prove compilation, restoration and runtime equivalence.

#![cfg(unix)]

mod common;

use common::{Sandbox, assert_success, stdout};

#[test]
fn growth_reports_fixed_maintenance_policy_despite_caller_overrides() {
    let sb = Sandbox::new("bench-growth-policy");
    let output = sb.bin("frust").args(["bench", "--growth", "--edits=1", "--libs=2", "--variants=cargo,auto", "--json"])
        .env("FRUST_CACHE", "off")
        .env("FRUST_AUTO_COMPACT", "off")
        .env("FRUST_COMPACT_KEEP_DAYS", "0")
        .env("FRUST_COMPACT_INCREMENTAL_IDLE_DAYS", "0")
        .env("FRUST_TARGET_MAX_GIB", "0.001")
        .output().unwrap();
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
        assert!(build.get("cargo_scheduled_units").is_none());
    }
    assert!(report.get("repeat_compiled").is_none());
}

#[test]
fn release_profiles_prove_real_compiles_restores_and_observable_edits() {
    let sb = Sandbox::new("bench-profiles");
    let output = sb.bin("frust").args(["bench", "--profiles", "--quick", "--runs=3", "--compression=off", "--json"])
        .env("RUSTC_WORKSPACE_WRAPPER", "/nonexistent/benchmark-must-clear-this")
        .env("CARGO_ENCODED_RUSTFLAGS", "--invalid-benchmark-flag")
        .env("FRUST_CACHE", "off")
        .output().unwrap();
    assert_success(&output);
    let report: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(report["evidence_schema_version"], 2);
    assert_eq!(report["cache_compression"], "off");
    assert_eq!(report["compiler_audit_available"], true);
    assert_eq!(report["registry_deps"], false);
    let profiles = report["profiles"].as_array().unwrap();
    assert_eq!(profiles.len(), 3);
    for (profile, name) in profiles.iter().zip(["release", "release-thin-lto", "release-fat-lto"]) {
        assert_eq!(profile["name"], name);
        assert_eq!(profile["same_directory_binaries_identical"], true);
        assert_eq!(profile["restored_binaries_identical"], true);
        assert_eq!(profile["runtime_outputs_equal"], true);
        let samples = profile["samples"].as_array().unwrap();
        assert_eq!(samples.len(), 24);
        for sample in samples {
            let invocations = sample["build"]["rustc_invocations"].as_array().unwrap();
            let counts = &sample["build"]["rustc_compilation_counts"];
            assert_eq!(counts["total"].as_u64().unwrap() as usize, invocations.len());
            assert_eq!(counts["unknown"], 0);
            let compile = sample["phase"] == "cold" || sample["phase"] == "edit"
                || sample["phase"] == "clean rebuild" && sample["tool"] == "cargo";
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
            }
            let edit = sample["expected_edit"].as_u64().unwrap();
            assert!(sample["stdout"].as_str().unwrap().contains(&format!("frust-bench-edit {edit}\n")));
        }
        assert_eq!(samples[0]["tool"], "cargo");
        assert_eq!(samples[8]["tool"], "frust");
    }
}
