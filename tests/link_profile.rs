//! Linker profiling must attribute each timing to a real current binary compilation.

#![cfg(any(target_os = "linux", target_os = "macos"))]

mod common;

use common::{Sandbox, assert_success, stdout};

#[test]
#[cfg(target_os = "linux")]
fn linux_linking_profiles_only_the_unchanged_linker_policy() {
    let sb = Sandbox::new("link-profile");
    let output = sb.bin("frust").args(["bench", "--linking", "--quick", "--runs", "3", "--json"]).output().unwrap();
    assert_success(&output);
    let report: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert!(report["runtime_outputs_equal"].is_null());
    assert_eq!(report["expected_edit_outputs_present"], true);
    let modes = report["modes"].as_array().unwrap();
    assert_eq!(modes.len(), 1);
    assert_eq!(modes[0]["mode"], "off");
    let samples = modes[0]["samples"].as_array().unwrap();
    assert_eq!(samples.len(), 3);
    for sample in samples {
        assert_eq!(sample["compiled_units"], 1);
        assert_eq!(sample["main_bin_compiled_units"], 1);
        assert_eq!(sample["smart_policy_applied"], false);
        assert_eq!(sample["expected_output_present"], true);
        assert!(sample["native_link_ms"].as_f64().unwrap() <= sample["total_build_ms"].as_f64().unwrap() + 1.0);
    }
}

#[test]
#[cfg(target_os = "macos")]
fn macos_linking_profiles_current_edits_under_both_linker_policies() {
    let sb = Sandbox::new("link-profile-macos");
    let output = sb.bin("frust").args(["bench", "--linking", "--quick", "--runs", "3", "--json"]).output().unwrap();
    assert_success(&output);
    let report: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(report["runtime_outputs_equal"], true);
    assert_eq!(report["expected_edit_outputs_present"], true);
    let modes = report["modes"].as_array().unwrap();
    assert_eq!(modes.len(), 2);
    for (mode, expected) in modes.iter().zip(["off", "smart"]) {
        assert_eq!(mode["mode"], expected);
        let samples = mode["samples"].as_array().unwrap();
        assert_eq!(samples.len(), 3);
        for sample in samples {
            assert_eq!(sample["compiled_units"], 1);
            assert_eq!(sample["main_bin_compiled_units"], 1);
            assert_eq!(sample["smart_policy_applied"], expected == "smart");
            assert_eq!(sample["expected_output_present"], true);
            assert!(sample["native_link_ms"].as_f64().unwrap() <= sample["total_build_ms"].as_f64().unwrap() + 1.0);
        }
    }
}
