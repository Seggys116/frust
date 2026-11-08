//! With `FRUST_CACHE_SHARE_ROOTS=on`, workspace crates that embed no checkout path hit across two checkouts of one repository, incremental or not; the rest stay specific to their checkout.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::{Sandbox, assert_success, run_binary, stdout};

const SHARED_LIB: &str = "pub fn greet() -> &'static str { \"hello\" }\n\
    pub fn here() -> &'static str { file!() }\n";
const ROOTED_LIB: &str = "pub fn manifest() -> &'static str { env!(\"CARGO_MANIFEST_DIR\") }\n";
const APP: &str = "fn main() {\n    println!(\"{}\", shared_lib::greet());\n    println!(\"{}\", shared_lib::here());\n    println!(\"{}\", rooted::manifest());\n}\n";

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
    common::settle(path);
}

fn manifest(name: &str, deps: &str) -> String {
    format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n{deps}")
}

/// The same workspace, byte for byte, in a new directory.
fn checkout(sb: &Sandbox, dir: &str) -> PathBuf {
    let root = sb.root.join(dir);
    write(&root.join("Cargo.toml"), "[workspace]\nmembers = [\"app\", \"libs/shared_lib\", \"libs/rooted\"]\nresolver = \"2\"\n");
    write(&root.join("libs/shared_lib/Cargo.toml"), &manifest("shared_lib", ""));
    write(&root.join("libs/shared_lib/src/lib.rs"), SHARED_LIB);
    write(&root.join("libs/rooted/Cargo.toml"), &manifest("rooted", ""));
    write(&root.join("libs/rooted/src/lib.rs"), ROOTED_LIB);
    let deps = "shared_lib = { path = \"../libs/shared_lib\" }\nrooted = { path = \"../libs/rooted\" }\n";
    write(&root.join("app/Cargo.toml"), &manifest("app", deps));
    write(&root.join("app/src/main.rs"), APP);
    root
}

/// The default dev profile compiles workspace crates incrementally with debuginfo; `incremental` false forces `CARGO_INCREMENTAL=0` and `debuginfo` false turns debuginfo off.
fn build_with(sb: &Sandbox, root: &Path, share: bool, incremental: bool, debuginfo: bool) {
    let mut cmd = sb.bin("fcargo");
    cmd.arg("build").current_dir(root);
    if !debuginfo {
        cmd.env("CARGO_PROFILE_DEV_DEBUG", "0");
    }
    if !incremental {
        cmd.env("CARGO_INCREMENTAL", "0");
    }
    if share {
        cmd.env("FRUST_CACHE_SHARE_ROOTS", "on");
    }
    assert_success(&cmd.output().unwrap());
}

fn build(sb: &Sandbox, root: &Path, share: bool, incremental: bool) {
    build_with(sb, root, share, incremental, false);
}

fn run(sb: &Sandbox, root: &Path) -> Vec<String> {
    let out = run_binary(&sb.central_target(root), "debug", "app");
    assert_success(&out);
    stdout(&out).lines().map(str::to_string).collect()
}

fn library(sb: &Sandbox, root: &Path, name: &str) -> Vec<u8> {
    let deps = sb.central_target(root).join("debug").join("deps");
    let prefix = format!("lib{name}-");
    let path = fs::read_dir(&deps)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| {
            let file = path.file_name().unwrap().to_string_lossy().into_owned();
            file.starts_with(&prefix) && file.ends_with(".rlib")
        })
        .unwrap_or_else(|| panic!("no rlib for {name} in {}", deps.display()));
    fs::read(path).unwrap()
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack.windows(needle.len()).any(|window| window == needle.as_bytes())
}

#[test]
fn clean_workspace_crates_hit_in_another_checkout_and_the_rest_stay_specific() {
    across_checkouts("share-roots", false, false);
}

#[test]
fn incremental_workspace_crates_of_the_default_dev_profile_share_too() {
    across_checkouts("share-roots-incremental", true, false);
}

#[test]
fn the_default_dev_profile_with_debuginfo_embeds_the_placeholder_and_no_checkout_path() {
    across_checkouts("share-roots-debuginfo", true, true);
}

fn across_checkouts(tag: &str, incremental: bool, debuginfo: bool) {
    let sb = Sandbox::new(tag);
    let (a, b) = (checkout(&sb, "checkout-a"), checkout(&sb, "checkout-b"));

    build_with(&sb, &a, true, incremental, debuginfo);
    assert_eq!(sb.count_cache("miss", "shared_lib"), 1);
    let first = run(&sb, &a);

    build_with(&sb, &b, true, incremental, debuginfo);
    assert_eq!(sb.count_cache("hit", "shared_lib"), 1, "{:#?}", sb.cache_events());
    assert_eq!(sb.count_cache("miss", "shared_lib"), 1, "the second checkout must not launch the compiler for it");
    let second = run(&sb, &b);

    for output in [&first, &second] {
        assert_eq!(output[0], "hello");
        for dir in [&a, &b] {
            assert!(!output[1].contains(&*dir.to_string_lossy()), "file!() names no checkout directory: {output:?}");
        }
    }
    for root in [&a, &b] {
        let rlib = library(&sb, root, "shared_lib");
        for dir in [&a, &b] {
            assert!(!contains(&rlib, &dir.to_string_lossy()), "the shared library in {} embeds no checkout path", root.display());
        }
        #[cfg(target_os = "linux")]
        {
            assert!(!debuginfo || contains(&rlib, "/frust-root"), "debug information carries the placeholder in {}", root.display());
        }
    }

    assert_eq!(sb.count_cache("miss", "rooted"), 2, "a crate that reads CARGO_MANIFEST_DIR is never shared");
    assert_eq!(sb.count_cache("hit", "rooted"), 0);
    assert!(first[2].contains("checkout-a"), "{first:?}");
    assert!(second[2].contains("checkout-b"), "the second checkout's binary reports its own directory: {second:?}");
    #[cfg(unix)]
    assert!(contains(&library(&sb, &b, "rooted"), &b.to_string_lossy()));
}

#[test]
fn without_the_knob_each_checkout_compiles_its_own_crates() {
    let sb = Sandbox::new("share-roots-off");
    let (a, b) = (checkout(&sb, "checkout-a"), checkout(&sb, "checkout-b"));
    build(&sb, &a, false, true);
    build(&sb, &b, false, true);
    assert_eq!(sb.count_cache("miss", "shared_lib"), 2, "{:#?}", sb.cache_events());
    assert_eq!(sb.count_cache("hit", "shared_lib"), 0);
}

#[test]
fn a_shared_checkout_still_hits_its_own_entries() {
    let sb = Sandbox::new("share-roots-same");
    let a = checkout(&sb, "checkout-a");
    build(&sb, &a, true, true);
    sb.lose_target(&a);
    build(&sb, &a, true, true);
    assert_eq!(sb.count_cache("hit", "shared_lib"), 1, "{:#?}", sb.cache_events());
    assert_eq!(sb.count_cache("hit", "rooted"), 1, "an unshared unit hits its checkout-specific entry");
    assert!(run(&sb, &a)[2].contains("checkout-a"));
}
