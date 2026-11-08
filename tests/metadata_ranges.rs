//! Real compiler coverage for metadata ranges, exact cache restores, and edit invalidation.

#![cfg(target_os = "linux")]

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use common::{Sandbox, assert_success, run_binary, stdout};
use frust::cache::{EntryMeta, payload::Encoding};

fn source(value: u32) -> String {
    let mut source = format!("pub fn value() -> u32 {{ {value} }}\n");
    for n in 0..1400 {
        source.push_str(&format!("pub fn metadata_fixture_function_{n}<T: Copy>(value: T) -> (T, u32) {{ (value, {n}) }}\n"));
    }
    source
}

// Cache restores must reproduce both artifact contents and permissions.
fn artifacts(deps: &Path) -> BTreeMap<String, (Vec<u8>, u32)> {
    fs::read_dir(deps)
        .unwrap()
        .map(Result::unwrap)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            (name.starts_with("libslice_fixture-")
                && (name.ends_with(".rlib") || name.ends_with(".rmeta")))
            .then(|| {
                (
                    name,
                    (
                        fs::read(entry.path()).unwrap(),
                        entry.metadata().unwrap().mode(),
                    ),
                )
            })
        })
        .collect()
}

#[test]
fn real_rustc_metadata_shares_its_archive_and_restores_without_compiling() {
    let sb = Sandbox::new("metadata-ranges");
    if !frust::cache::payload::compression_supported(&sb.home()) {
        return;
    }
    sb.vendor_crate("slice_fixture", "0.1.0", &source(19));
    let project = sb.project(
        "app",
        &[("slice_fixture", "0.1.0")],
        "fn main() { println!(\"{}\", slice_fixture::value()); }\n",
    );
    let compiler = sb.root.join("counted-rustc");
    let calls = sb.root.join("compiler-calls");
    fs::write(
        &compiler,
        r#"#!/bin/sh
for arg do
    case "$arg" in
        --emit=*) printf 'compile\n' >> "$METADATA_RANGE_CALLS"; break ;;
    esac
done
exec rustc "$@"
"#,
    )
    .unwrap();
    fs::set_permissions(&compiler, fs::Permissions::from_mode(0o755)).unwrap();
    let run = |args: &[&str]| {
        assert_success(
            &sb.bin("fcargo")
                .args(args)
                .current_dir(&project)
                .env("RUSTC", &compiler)
                .env("METADATA_RANGE_CALLS", &calls)
                .env("FRUST_CACHE_COMPRESSION", "on")
                .output()
                .unwrap(),
        );
    };
    run(&["build"]);
    let target = sb.central_target(&project);
    let before = artifacts(&target.join("debug/deps"));
    assert_eq!(before.len(), 2);
    let mut found = false;
    {
        for entry in frust::cache::keyed_items(&sb.home().join("store")) {
            let meta: EntryMeta =
                serde_json::from_slice(&fs::read(entry.join("meta.json")).unwrap()).unwrap();
            if meta.crate_name != "slice_fixture" {
                continue;
            }
            let slice = meta
                .files
                .iter()
                .find(|f| f.encoding == Encoding::InlineMetadataSliceV1)
                .expect("real metadata range");
            let base = meta
                .files
                .iter()
                .find(|f| f.name.ends_with(".rlib"))
                .unwrap();
            assert_eq!(slice.stored, base.stored);
            assert_eq!(slice.stored_bytes, Some(0));
            assert_eq!(base.encoding, Encoding::Lz4BlocksV1);
            assert_eq!(
                meta.files
                    .iter()
                    .filter(|f| f.stored == base.stored)
                    .count(),
                2
            );
            assert_eq!(
                fs::read_dir(&entry).unwrap().count(),
                meta.files.iter().filter(|f| f.inline.is_none()).map(|f| &f.stored).collect::<std::collections::HashSet<_>>().len() + 1,
                "only normal artifacts and metadata are stored; dep-info and diagnostics live in the record"
            );
            found = true;
        }
    }
    assert!(found, "the real dependency was cached");
    let compiled = fs::read(&calls).unwrap();
    // `app` is a path crate, so `fcargo clean` would release its entry; losing the target keeps this a restore test.
    sb.lose_target(&project);
    run(&["build"]);
    assert_eq!(
        fs::read(&calls).unwrap(),
        compiled,
        "a clean cache hit launches no compiler"
    );
    assert_eq!(artifacts(&target.join("debug/deps")), before);
    let output = run_binary(&target, "debug", "app");
    assert_success(&output);
    assert_eq!(stdout(&output).trim(), "19");
    sb.replace_vendor_source("slice_fixture", "0.1.0", &source(23));
    sb.lose_target(&project);
    run(&["build"]);
    assert!(
        fs::read(&calls).unwrap().len() > compiled.len(),
        "an edit invokes rustc"
    );
    let output = run_binary(&target, "debug", "app");
    assert_success(&output);
    assert_eq!(stdout(&output).trim(), "23");
}
