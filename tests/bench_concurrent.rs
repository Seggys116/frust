//! Concurrent cold groups prove cooperative reuse against the observed Cargo unit baseline.

#![cfg(any(target_os = "linux", target_os = "macos"))]

mod common;

use common::{Sandbox, assert_success, stdout};
use serde_json::Value;

fn run(store: Option<&str>) -> Value {
    let sandbox = Sandbox::new(if store.is_some() {
        "concurrent-sync"
    } else {
        "concurrent-deferred"
    });
    let mut command = sandbox.bin("frust");
    command
        .args(["bench", "--concurrent", "--quick", "--runs=3", "--json"])
        .env("TMPDIR", &sandbox.root)
        .env("TMP", &sandbox.root)
        .env("TEMP", &sandbox.root)
        .env("RUSTC_WORKSPACE_WRAPPER", "/missing/cleared-by-benchmark")
        .env("CARGO_ENCODED_RUSTFLAGS", "--invalid-benchmark-flag")
        .env("FRUST_CACHE", "off");
    if let Some(store) = store {
        command.arg(format!("--store={store}"));
    }
    let output = command.output().unwrap();
    assert_success(&output);
    serde_json::from_str(&stdout(&output)).unwrap()
}

fn assert_groups(report: &Value, store: &str) {
    assert_eq!(report["mode"], "concurrent_cold_groups");
    assert_eq!(report["store_policy"], store);
    assert_eq!(report["runs"], 3);
    assert_eq!(report["registry_deps"], false);
    assert_eq!(report["local_crates"], 2);
    assert!(report["rustc_verbose"].as_str().unwrap().contains("host:"));
    assert!(!report["toolchain_probe_id"].as_str().unwrap().is_empty());
    assert!(
        report["cargo_version"]
            .as_str()
            .unwrap()
            .starts_with("cargo ")
    );
    assert_eq!(
        report["profiles"],
        serde_json::json!(["coldone", "coldtwo"])
    );
    let groups = report["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 12);
    for group in groups {
        let foreground = group["foreground_ns"].as_u64().unwrap();
        let drain = group["store_drain_ns"].as_u64().unwrap();
        assert_eq!(group["settled_ns"].as_u64().unwrap(), foreground + drain);
        assert_eq!(group["managed_allocation"]["status"], "available");
        assert!(
            group["managed_allocation"]["total_managed_bytes"]
                .as_u64()
                .unwrap()
                > 0
        );
        let roles = group["roles"].as_array().unwrap();
        assert_eq!(roles.len(), 2);
        for role in roles {
            assert_eq!(role["exit_code"], 0);
            assert_eq!(role["rustc_compilation_counts"]["unknown"], 0);
            assert!(role["observed_exit_ns"].as_u64().unwrap() <= foreground);
            assert!(role.get("cache_events").is_none());
            assert_eq!(
                role["rustc_calls"].as_array().unwrap().len(),
                role["rustc_call_count"].as_u64().unwrap() as usize
            );
            let compiles = role["rustc_invocations"].as_array().unwrap().len();
            let queries = role["rustc_query_invocations"].as_array().unwrap().len();
            assert_eq!(
                role["rustc_call_count"].as_u64().unwrap() as usize,
                compiles + queries
            );
            assert_eq!(
                role["rustc_exit_codes"].as_array().unwrap().len(),
                compiles + queries
            );
            assert!(
                role["runtime_stdout"]
                    .as_str()
                    .unwrap()
                    .starts_with("frust-bench-edit 0\n")
            );
            assert_eq!(role["binary_hash"].as_str().unwrap().len(), 64);
            if group["phase"] == "no-op" {
                assert_eq!(role["rustc_call_count"], 0);
                assert_eq!(role["cargo_compile_notices"], 0);
                assert!(group["cache_events"].as_array().unwrap().is_empty());
            }
        }
        if group["phase"] == "cold" && group["tool"] == "frust" {
            assert!(!group["cross_role_reuse"].as_array().unwrap().is_empty());
            let units = &group["unit_multiplicity"];
            assert!(
                units["cargo_total"].as_u64().unwrap() > units["frust_total"].as_u64().unwrap()
            );
            // Counts come from compiler evidence, so fixture dependency availability cannot change the expectation.
            assert_eq!(
                units["cargo_total"].as_u64().unwrap(),
                2 * units["cargo_per_role"].as_u64().unwrap()
            );
            assert_eq!(group["cold_start"]["home_entries"], serde_json::json!([]));
        }
    }
    for repetition in 0..3 {
        let cold: Vec<_> = groups
            .iter()
            .filter(|group| group["phase"] == "cold" && group["repetition"] == repetition)
            .collect();
        assert_eq!(cold.len(), 2);
        for profile in ["coldone", "coldtwo"] {
            let role = |group: &Value| {
                group["roles"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|role| role["profile"] == profile)
                    .unwrap()
                    .clone()
            };
            assert_eq!(role(cold[0])["binary_hash"], role(cold[1])["binary_hash"]);
            assert_eq!(
                role(cold[0])["runtime_stdout_hash"],
                role(cold[1])["runtime_stdout_hash"]
            );
        }
    }
    assert_eq!(
        report["paired_foreground_frust_minus_cargo"]["raw_ns"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
}

#[test]
fn concurrent_defaults_to_deferred_and_proves_peer_reuse_and_noops() {
    assert_groups(&run(None), "deferred");
}

#[test]
fn concurrent_explicit_sync_preserves_correctness_and_reuse() {
    assert_groups(&run(Some("sync")), "sync");
}

#[test]
fn concurrent_rejects_incompatible_modes_before_workload() {
    let sandbox = Sandbox::new("concurrent-flags");
    for flag in [
        "--profiles",
        "--growth",
        "--churn",
        "--linking",
        "--latency",
        "--settle-edits",
        "--settle-edits=true",
        "--concurrent",
        "--concurrent=true",
    ] {
        let output = sandbox
            .bin("frust")
            .args(["bench", "--concurrent", flag, "--json"])
            .env("RUSTC", sandbox.root.join("missing-rustc"))
            .output()
            .unwrap();
        assert!(!output.status.success(), "{flag}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("--concurrent"));
        assert!(output.stdout.is_empty());
    }
}
