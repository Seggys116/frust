//! Concurrent cold groups prove cooperative reuse against the observed Cargo unit baseline.

#![cfg(any(target_os = "linux", target_os = "macos"))]

mod common;

use common::{Sandbox, assert_success, stdout};
use serde_json::Value;

fn copy_evidence(
    source: &std::path::Path,
    destination: &std::path::Path,
    budget: &mut (u64, usize),
) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(source)?;
    budget.1 = budget
        .1
        .checked_sub(1)
        .ok_or_else(|| std::io::Error::other("failure export entry limit exceeded"))?;
    if metadata.is_dir() {
        std::fs::create_dir(destination)?;
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            copy_evidence(&entry.path(), &destination.join(entry.file_name()), budget)?;
        }
    } else if metadata.is_file() {
        budget.0 = budget
            .0
            .checked_sub(metadata.len())
            .ok_or_else(|| std::io::Error::other("failure export byte limit exceeded"))?;
        std::fs::copy(source, destination)?;
    } else {
        return Err(std::io::Error::other(format!(
            "nonregular failure evidence: {}",
            source.display()
        )));
    }
    Ok(())
}

fn export_failure(sandbox: &Sandbox, output: &std::process::Output) -> std::io::Result<()> {
    let Some(root) = std::env::var_os("FRUST_TEST_FAILURE_DIR") else {
        return Ok(());
    };
    let root = std::path::PathBuf::from(root);
    std::fs::create_dir_all(&root)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(std::io::Error::other)?
        .as_nanos();
    for attempt in 0..32 {
        let destination = root.join(format!(
            "bench-concurrent-{}-{stamp}-{attempt}",
            std::process::id()
        ));
        match std::fs::create_dir(&destination) {
            Ok(()) => {
                let retained = destination.join("sandbox");
                // A new destination owns this failure; cross-filesystem exports copy only bounded diagnostic inputs.
                if let Err(error) = frust::sys::rename(&sandbox.root, &retained) {
                    eprintln!(
                        "concurrent failure rename {} -> {} failed: {error}; copying diagnostic evidence",
                        sandbox.root.display(),
                        retained.display()
                    );
                    std::fs::create_dir(&retained)?;
                    let mut budget = (256 * 1024 * 1024, 16_384);
                    for entry in std::fs::read_dir(&sandbox.root)? {
                        let entry = entry?;
                        if !entry.file_type()?.is_dir()
                            || !entry
                                .file_name()
                                .to_string_lossy()
                                .starts_with("frust-bench-")
                        {
                            continue;
                        }
                        let exported = retained.join(entry.file_name());
                        std::fs::create_dir(&exported)?;
                        // Targets and cache homes are unnecessary once both disputed binaries have been retained.
                        for name in ["binary-diagnostics", "audit", "app", "audited-rustc"] {
                            let source = entry.path().join(name);
                            if source.try_exists()? {
                                copy_evidence(&source, &exported.join(name), &mut budget)?;
                            }
                        }
                    }
                }
                std::fs::write(destination.join("command.stdout"), &output.stdout)?;
                std::fs::write(destination.join("command.stderr"), &output.stderr)?;
                let paths =
                    serde_json::json!({"original_root": sandbox.root, "exported_root": retained});
                std::fs::write(
                    destination.join("paths.json"),
                    serde_json::to_vec_pretty(&paths).map_err(std::io::Error::other)?,
                )?;
                eprintln!(
                    "concurrent failure exported: {} -> {}",
                    sandbox.root.display(),
                    destination.join("sandbox").display()
                );
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "no unique concurrent failure directory",
    ))
}

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
    if !output.status.success()
        && let Err(error) = export_failure(&sandbox, &output)
    {
        eprintln!("concurrent failure export failed: {error}");
    }
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
    #[cfg(target_os = "macos")]
    {
        assert!(
            report["native_debug_policy"]
                .as_str()
                .unwrap()
                .contains("split-debuginfo=packed")
        );
        for group in report["warmups"].as_array().unwrap().iter().chain(groups) {
            for role in group["roles"].as_array().unwrap() {
                assert_native_debug(&role["native_debug"]);
                for args in role["rustc_invocations"].as_array().unwrap() {
                    let args = args.as_array().unwrap();
                    if args.iter().any(|arg| arg == "app") {
                        for flag in ["debuginfo=2", "split-debuginfo=packed", "strip=debuginfo"] {
                            assert!(
                                args.iter().any(|arg| arg == flag),
                                "missing {flag}: {args:?}"
                            );
                        }
                        assert!(args.iter().any(|arg| {
                            arg.as_str()
                                .is_some_and(|arg| arg.starts_with("incremental="))
                        }));
                    }
                }
            }
        }
        let restores = report["native_debug_cache_restores"].as_array().unwrap();
        assert_eq!(restores.len(), 2);
        for (index, restored) in restores.iter().enumerate() {
            assert_eq!(restored["profile"], ["coldone", "coldtwo"][index]);
            assert_eq!(restored["rustc_compilation_counts"]["cargo_units"], 0);
            assert_eq!(restored["rustc_compilation_counts"]["unknown"], 0);
            assert!(restored["cache_hits"].as_u64().unwrap() > 0);
            assert_eq!(restored["cache_misses"], 0);
            assert_eq!(
                restored["original_binary_hash"],
                restored["restored_binary_hash"]
            );
            assert_eq!(
                restored["original_runtime_stdout_hash"],
                restored["restored_runtime_stdout_hash"]
            );
            assert_eq!(restored["original"]["files"], restored["restored"]["files"]);
            assert_eq!(restored["original"]["uuid"], restored["restored"]["uuid"]);
            assert_eq!(
                restored["original"]["executable_mode"],
                restored["restored"]["executable_mode"]
            );
            assert_native_debug(&restored["original"]);
            assert_native_debug(&restored["restored"]);
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        assert!(report.get("native_debug_policy").is_none());
        assert!(report.get("native_debug_cache_restores").is_none());
    }
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

#[cfg(target_os = "macos")]
fn assert_native_debug(proof: &Value) {
    assert!(proof["bundle"].as_str().unwrap().ends_with(".dSYM"));
    assert_eq!(proof["uuid"].as_str().unwrap().len(), 36);
    assert_eq!(
        proof["sources_with_types_and_locals"],
        serde_json::json!([
            "libs/lib0/src/lib.rs",
            "libs/lib1/src/lib.rs",
            "src/main.rs"
        ])
    );
    assert_ne!(proof["executable_mode"].as_u64().unwrap() & 0o111, 0);
    let files = proof["files"].as_object().unwrap();
    assert!(files.contains_key("Contents/Info.plist"));
    assert!(files.iter().any(
        |(name, file)| name.starts_with("Contents/Resources/DWARF/app-")
            && file["bytes"].as_u64().unwrap() > 0
    ));
    for file in files.values() {
        assert_eq!(file["hash"].as_str().unwrap().len(), 64);
        assert_ne!(file["mode"].as_u64().unwrap() & 0o444, 0);
    }
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
