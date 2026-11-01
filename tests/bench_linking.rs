//! Public linking benchmarks must attribute real body edits and execute their final binaries.

#![cfg(any(target_os = "linux", target_os = "macos"))]

mod common;

use common::{Sandbox, assert_success, stderr, stdout};
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::process::Command;

fn assert_stats(stats: &Value) {
    let min = stats["min_ms"].as_f64().unwrap();
    let median = stats["median_ms"].as_f64().unwrap();
    let max = stats["max_ms"].as_f64().unwrap();
    assert!(min.is_finite() && median.is_finite() && max.is_finite());
    assert!(0.0 <= min && min <= median && median <= max, "{stats}");
}

#[test]
fn public_linking_collector_profiles_real_edits_and_runs_both_native_modes() {
    let sb = Sandbox::new("bench-linking");
    let output = sb
        .bin("frust")
        .args([
            "bench",
            "--linking",
            "--quick",
            "--runs=3",
            "--keep-scratch",
            "--json",
        ])
        .current_dir(&sb.root)
        .env("TMPDIR", &sb.root)
        .env("TMP", &sb.root)
        .env("TEMP", &sb.root)
        .env(
            "CARGO_ENCODED_RUSTFLAGS",
            "--invalid-linking-benchmark-flag",
        )
        .env("RUSTC_BOOTSTRAP", "0")
        .output()
        .unwrap();
    assert_success(&output);
    let report: Value = serde_json::from_str(&stdout(&output)).unwrap();
    let paired = cfg!(target_os = "macos");
    assert_eq!(report["runs"], 3);
    assert_eq!(report["artifact_cache"], "off");
    assert_eq!(report["build_lock"], true);
    assert_eq!(report["expected_edit_outputs_present"], true);
    assert_eq!(
        report["runtime_outputs_equal"],
        if paired {
            Value::Bool(true)
        } else {
            Value::Null
        }
    );
    assert!(!report["rustc"].as_str().unwrap().is_empty());
    let policy = report["policy"].as_str().unwrap();
    assert!(policy.contains(if paired {
        "FRUST_LINK=smart"
    } else {
        "FRUST_LINK=off"
    }));
    let sources = &report["sources"];
    assert_eq!(sources["local_crates"], 4);
    assert_eq!(sources["registry_deps"], false);
    assert_eq!(sources["edited_source"], "app/src/main.rs");
    assert_eq!(sources["shared_source_path"], true);
    assert_eq!(sources["separate_target_directories"], paired);
    let modes = report["modes"].as_array().unwrap();
    assert_eq!(modes.len(), if paired { 2 } else { 1 });
    for (index, mode) in modes.iter().enumerate() {
        let smart = index == 1;
        assert_eq!(mode["mode"], if smart { "smart" } else { "off" });
        assert_stats(&mode["total_build"]);
        assert_stats(&mode["native_link"]);
        let samples = mode["samples"].as_array().unwrap();
        assert_eq!(samples.len(), 3);
        for (offset, sample) in samples.iter().enumerate() {
            let edit = offset + 2;
            let order = if paired && edit % 2 != 0 {
                2 - index
            } else {
                index + 1
            };
            assert_eq!(sample["edit"], edit);
            assert_eq!(sample["order_in_pair"], order);
            assert_eq!(sample["compiled_units"], 1, "{sample}");
            assert_eq!(sample["main_bin_compiled_units"], 1, "{sample}");
            assert_eq!(sample["smart_policy_applied"], smart);
            assert_eq!(sample["expected_output_present"], true);
            assert!(sample["binary_bytes"].as_u64().unwrap() > 0);
            let native = sample["native_link_ms"].as_f64().unwrap();
            let wall = sample["total_build_ms"].as_u64().unwrap();
            assert!(
                native.is_finite() && native >= 0.0 && native <= wall as f64 + 1.0,
                "{sample}"
            );
        }
        assert_eq!(
            mode["binary_bytes"],
            samples.last().unwrap()["binary_bytes"]
        );
    }

    let stderr = stderr(&output);
    let retained: Vec<_> = stderr
        .lines()
        .filter_map(|line| line.strip_prefix("frust bench: retained scratch at "))
        .collect();
    assert_eq!(retained.len(), 1, "{stderr}");
    let retained = Path::new(retained[0]);
    assert!(retained.starts_with(&sb.root) && retained.is_dir());
    let final_source = fs::read_to_string(retained.join("linking/app/src/main.rs")).unwrap();
    let markers: Vec<_> = final_source
        .lines()
        .filter(|line| line.contains("frust-link-edit"))
        .collect();
    assert_eq!(markers.len(), 1, "{final_source}");
    let main_body = &final_source[final_source.find("fn main() {").unwrap()..];
    assert!(main_body.starts_with(&format!("fn main() {{\n{}", markers[0])));
    // Removing the sole final body marker must recover the recorded original workload.
    let original = final_source.replacen(&format!("\n{}", markers[0]), "", 1);
    assert_eq!(
        original.len() as u64,
        sources["original_source_bytes"].as_u64().unwrap()
    );
    assert_eq!(
        blake3::hash(original.as_bytes()).to_hex().as_str(),
        sources["original_source_blake3"].as_str().unwrap()
    );

    let mut outputs = Vec::new();
    for mode in modes {
        let name = mode["mode"].as_str().unwrap();
        let binary = retained.join(format!("link-{name}-target/debug/app"));
        assert_eq!(
            fs::metadata(&binary).unwrap().len(),
            mode["binary_bytes"].as_u64().unwrap()
        );
        let mut command = Command::new(&binary);
        // Direct execution must not borrow Cargo's dynamic-loader search environment.
        for (name, _) in std::env::vars_os() {
            let key = name.to_string_lossy();
            if key.starts_with("LD_") || key.starts_with("DYLD_") {
                command.env_remove(name);
            }
        }
        let run = command.output().unwrap();
        assert_success(&run);
        let text = stdout(&run);
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert_eq!(lines[0], "frust-link-edit 4");
        assert!(lines[1].parse::<u64>().is_ok(), "{text}");
        outputs.push(run.stdout);
    }
    if paired {
        assert_eq!(outputs[0], outputs[1]);
    }
}
