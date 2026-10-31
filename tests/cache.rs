//! The compile cache: reuse across projects, and never reusing something stale.

mod common;

use common::{Sandbox, assert_success, run_binary, stdout};

const LEAF_V1: &str = "pub fn value() -> u32 { 41 }\npub fn tag() -> &'static str { option_env!(\"FRUST_TEST_TAG\").unwrap_or(\"unset\") }\n";
const MAIN: &str = "fn main() { println!(\"{} {}\", leaf::value(), leaf::tag()); }\n";

#[test]
fn second_project_reuses_the_dependency_from_the_cache() {
    let sb = Sandbox::new("reuse");
    sb.vendor_crate("leaf", "1.0.0", LEAF_V1);
    let a = sb.project("a", &[("leaf", "1.0.0")], MAIN);
    let b = sb.project("b", &[("leaf", "1.0.0")], MAIN);

    assert_success(&sb.bin("fcargo").arg("build").current_dir(&a).output().unwrap());
    assert_eq!(sb.count_cache("miss", "leaf"), 1, "first build compiles and stores leaf");
    assert_eq!(sb.count_cache("hit", "leaf"), 0);

    assert_success(&sb.bin("fcargo").arg("build").current_dir(&b).output().unwrap());
    assert_eq!(
        sb.count_cache("hit", "leaf"),
        1,
        "second project restores leaf from the cache; events: {:#?}",
        sb.cache_events()
    );

    let out = run_binary(&sb.central_target(&b), "debug", "app");
    assert_success(&out);
    assert_eq!(stdout(&out).trim(), "41 unset");
}

#[test]
fn rebuild_after_clean_is_served_from_the_cache() {
    let sb = Sandbox::new("clean");
    sb.vendor_crate("leaf", "1.0.0", LEAF_V1);
    let a = sb.project("a", &[("leaf", "1.0.0")], MAIN);
    assert_success(&sb.bin("fcargo").arg("build").current_dir(&a).output().unwrap());
    assert_success(&sb.bin("fcargo").arg("clean").current_dir(&a).output().unwrap());
    assert_success(&sb.bin("fcargo").arg("build").current_dir(&a).output().unwrap());
    assert_eq!(sb.count_cache("hit", "leaf"), 1);
    let out = run_binary(&sb.central_target(&a), "debug", "app");
    assert_eq!(stdout(&out).trim(), "41 unset");
}

#[test]
fn changed_source_is_never_served_stale() {
    let sb = Sandbox::new("stale");
    sb.vendor_crate("leaf", "1.0.0", LEAF_V1);
    let a = sb.project("a", &[("leaf", "1.0.0")], MAIN);
    assert_success(&sb.bin("fcargo").arg("build").current_dir(&a).output().unwrap());

    // The clean below forces a rebuild, so a stale restore would show the old value.
    sb.replace_vendor_source("leaf", "1.0.0", &LEAF_V1.replace("41", "42"));
    assert_success(&sb.bin("fcargo").args(["clean"]).current_dir(&a).output().unwrap());
    assert_success(&sb.bin("fcargo").arg("build").current_dir(&a).output().unwrap());

    let out = run_binary(&sb.central_target(&a), "debug", "app");
    assert_eq!(stdout(&out).trim(), "42 unset", "edited dependency must be recompiled, not restored");
    assert_eq!(sb.count_cache("miss", "leaf"), 2);
}

#[test]
fn observed_environment_variables_are_part_of_the_key() {
    let sb = Sandbox::new("env");
    sb.vendor_crate("leaf", "1.0.0", LEAF_V1);
    let a = sb.project("a", &[("leaf", "1.0.0")], MAIN);

    // Cleaning each time makes every build consult the cache.
    for tag in ["one", "two", "one"] {
        assert_success(&sb.bin("fcargo").arg("clean").current_dir(&a).output().unwrap());
        assert_success(&sb.bin("fcargo").arg("build").env("FRUST_TEST_TAG", tag).current_dir(&a).output().unwrap());
        let out = run_binary(&sb.central_target(&a), "debug", "app");
        assert_eq!(stdout(&out).trim(), format!("41 {tag}"));
    }
    assert_eq!(sb.count_cache("miss", "leaf"), 2, "two distinct values compile once each");
    assert_eq!(sb.count_cache("hit", "leaf"), 1, "the repeated value is a hit");
}

#[test]
fn cache_can_be_disabled() {
    let sb = Sandbox::new("off");
    sb.vendor_crate("leaf", "1.0.0", LEAF_V1);
    let a = sb.project("a", &[("leaf", "1.0.0")], MAIN);
    assert_success(&sb.bin("fcargo").arg("build").env("FRUST_CACHE", "off").current_dir(&a).output().unwrap());
    assert!(sb.cache_events().is_empty());
}

#[test]
fn different_profiles_do_not_share_entries() {
    let sb = Sandbox::new("profiles");
    sb.vendor_crate("leaf", "1.0.0", LEAF_V1);
    let a = sb.project("a", &[("leaf", "1.0.0")], MAIN);
    assert_success(&sb.bin("fcargo").arg("build").current_dir(&a).output().unwrap());
    assert_success(&sb.bin("fcargo").args(["build", "--release"]).current_dir(&a).output().unwrap());
    assert_eq!(sb.count_cache("miss", "leaf"), 2);
    assert_eq!(sb.count_cache("hit", "leaf"), 0);
}

#[test]
fn build_after_a_restored_build_is_a_no_op() {
    let sb = Sandbox::new("noop");
    sb.vendor_crate("leaf", "1.0.0", LEAF_V1);
    let a = sb.project("a", &[("leaf", "1.0.0")], MAIN);
    assert_success(&sb.bin("fcargo").arg("build").current_dir(&a).output().unwrap());
    assert_success(&sb.bin("fcargo").arg("clean").current_dir(&a).output().unwrap());
    assert_success(&sb.bin("fcargo").arg("build").current_dir(&a).output().unwrap());
    assert_eq!(sb.count_cache("hit", "leaf"), 1);

    let events_before = sb.cache_events().len();
    let out = sb.bin("fcargo").arg("build").current_dir(&a).output().unwrap();
    assert_success(&out);
    assert_eq!(sb.cache_events().len(), events_before, "restored outputs must be fresh to cargo: nothing may recompile or re-restore");
    assert!(common::stderr(&out).contains("Finished"), "{}", common::stderr(&out));
}

#[cfg(unix)]
#[test]
fn restored_build_scripts_stay_fresh_across_repeated_clean_rebuilds() {
    use std::fs;

    let sb = Sandbox::new("build-script-freshness");
    sb.vendor_crate("leaf", "1.0.0", "pub fn value() -> u32 { include!(concat!(env!(\"OUT_DIR\"), \"/value.rs\")) }\n");
    let build_script = sb.vendor().join("leaf-1.0.0/build.rs");
    fs::write(&build_script, r#"use std::io::Write;
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=FRUST_TEST_BUILD_RUNS");
    let calls = std::env::var_os("FRUST_TEST_BUILD_RUNS").unwrap();
    let mut calls = std::fs::OpenOptions::new().create(true).append(true).open(calls).unwrap();
    writeln!(calls, "run").unwrap();
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    std::fs::write(out.join("value.rs"), "41u32").unwrap();
}
"#).unwrap();
    common::settle(&build_script);
    let project = sb.project("app", &[("leaf", "1.0.0")], "fn main() { println!(\"{}\", leaf::value()); }\n");
    let calls = sb.root.join("build-script-runs");
    let build = || sb.bin("fcargo").args(["build", "--release"])
        .env("FRUST_TEST_BUILD_RUNS", &calls).env("FRUST_AUTO_COMPACT", "off")
        .current_dir(&project).output().unwrap();
    let script_runs = || fs::read_to_string(&calls).unwrap().lines().count();
    assert_success(&build());
    assert_eq!(script_runs(), 1);
    let target = sb.central_target(&project);

    // A restored executable must not be newer than Cargo's subsequently created build-script output marker.
    for round in 0..8 {
        assert_success(&sb.bin("fcargo").arg("clean").current_dir(&project).output().unwrap());
        assert_success(&build());
        assert_eq!(sb.count_cache("hit", "build_script_build"), round + 1, "the build-script executable must be restored");
        assert_eq!(script_runs(), round + 2, "a clean rebuild runs the restored build script exactly once");
        let events_before = sb.cache_events().len();
        let noop = build();
        assert_success(&noop);
        assert_eq!(sb.cache_events().len(), events_before, "round {round}: a no-op must neither compile nor restore again");
        assert_eq!(script_runs(), round + 2, "round {round}: a no-op must not rerun the build script");
        assert!(!common::stderr(&noop).lines().any(|line| line.trim_start().starts_with("Compiling ")), "{}", common::stderr(&noop));
        let output = run_binary(&target, "release", "app");
        assert_success(&output);
        assert_eq!(stdout(&output).trim(), "41");
    }
}
