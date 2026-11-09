//! Linked entries store a large output as a second name of the target file, and never let a rewritten shared file produce a build.

mod common;

use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use common::{Sandbox, assert_success, run_binary, stdout};
use frust::cache::EntryMeta;
use frust::cache::payload::Encoding;

const MAIN: &str = "fn main() { println!(\"{}\", leaf::value()); }\n";

// Enough code that the library and its metadata exceed the smallest size worth linking.
fn leaf_source() -> String {
    leaf_source_with(41)
}

fn leaf_source_with(value: u32) -> String {
    let mut text = format!("pub fn value() -> u32 {{ {value} }}\n");
    for n in 0..300u32 {
        text.push_str(&format!("pub fn filler_{n}(x: u32) -> u32 {{ x.wrapping_mul({}).wrapping_add({n}) }}\n", n + 3));
    }
    text
}

fn scene(tag: &str) -> (Sandbox, PathBuf, PathBuf) {
    let sb = Sandbox::new(tag);
    sb.vendor_crate("leaf", "1.0.0", &leaf_source());
    let a = sb.project("a", &[("leaf", "1.0.0")], MAIN);
    let b = sb.project("b", &[("leaf", "1.0.0")], MAIN);
    (sb, a, b)
}

fn build(sb: &Sandbox, project: &Path, mode: &str) {
    assert_success(&sb.bin("fcargo").arg("build").env("FRUST_CACHE_BACKED", mode).current_dir(project).output().unwrap());
}

fn app_output(sb: &Sandbox, project: &Path) -> String {
    let out = run_binary(&sb.central_target(project), "debug", "app");
    assert_success(&out);
    stdout(&out).trim().to_string()
}

fn entries(sb: &Sandbox, krate: &str) -> Vec<(PathBuf, EntryMeta)> {
    let mut found = Vec::new();
    for entry in frust::cache::keyed_items(&sb.home().join("store")) {
        let Ok(bytes) = fs::read(entry.join("meta.json")) else { continue };
        if let Ok(meta) = serde_json::from_slice::<EntryMeta>(&bytes)
            && meta.crate_name == krate
        {
            found.push((entry, meta));
        }
    }
    found
}

fn manifests(sb: &Sandbox) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for file in frust::cache::keyed_items(&sb.home().join("manifests")) {
        if file.extension().is_some_and(|e| e == "json") {
            found.push(file);
        }
    }
    found
}

fn manifest_formats(sb: &Sandbox) -> Vec<u64> {
    manifests(sb)
        .iter()
        .filter_map(|path| serde_json::from_slice::<serde_json::Value>(&fs::read(path).ok()?).ok()?["format"].as_u64())
        .collect()
}

fn leaf_library(target: &Path, extension: &str) -> PathBuf {
    let deps = target.join("debug").join("deps");
    fs::read_dir(&deps)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| {
            let name = path.file_name().unwrap().to_string_lossy();
            name.starts_with("libleaf-") && name.ends_with(extension)
        })
        .unwrap_or_else(|| panic!("no leaf {extension} in {}", deps.display()))
}

fn leaf_entry(sb: &Sandbox) -> (PathBuf, EntryMeta) {
    let mut found = entries(sb, "leaf");
    assert_eq!(found.len(), 1, "exactly one leaf entry: {found:#?}");
    found.remove(0)
}

fn rlib_record(meta: &EntryMeta) -> &frust::cache::StoredFile {
    meta.files.iter().find(|file| file.name.ends_with(".rlib")).unwrap()
}

fn ino(path: &Path) -> u64 {
    frust::sys::lstat(path).unwrap().ino()
}

// A workspace of path crates (leaf, mid depends on leaf, app depends on mid), whose units are versioned across edits.
fn path_workspace(sb: &Sandbox) -> PathBuf {
    let root = sb.root.join("ws");
    let members = [
        ("leaf", "", leaf_source_with(0)),
        ("mid", "leaf = { path = \"../leaf\" }\n", "pub fn value() -> u32 { leaf::value() + 1 }\n".to_string()),
        ("app", "mid = { path = \"../mid\" }\n", "fn main() { println!(\"{}\", mid::value()); }\n".to_string()),
    ];
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = [\"leaf\", \"mid\", \"app\"]\nresolver = \"2\"\n").unwrap();
    for (name, deps, source) in members {
        let dir = root.join(name);
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("Cargo.toml"), format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n{deps}")).unwrap();
        let file = if name == "app" { "main.rs" } else { "lib.rs" };
        fs::write(dir.join("src").join(file), source).unwrap();
    }
    root
}

fn edit_leaf(root: &Path, value: u32) {
    let path = root.join("leaf").join("src").join("lib.rs");
    fs::write(&path, leaf_source_with(value)).unwrap();
    common::settle(&path);
}

fn build_with(sb: &Sandbox, project: &Path, envs: &[(&str, &str)]) {
    let mut command = sb.bin("fcargo");
    command.arg("build").current_dir(project);
    for (name, value) in envs {
        command.env(name, value);
    }
    assert_success(&command.output().unwrap());
}

fn store_only_linked_files(sb: &Sandbox, krates: &[&str]) -> usize {
    krates
        .iter()
        .flat_map(|krate| entries(sb, krate))
        .flat_map(|(dir, meta)| {
            meta.files
                .into_iter()
                .filter(|file| file.encoding == Encoding::Linked)
                .filter(|file| frust::sys::lstat(&dir.join(&file.stored)).unwrap().nlink() == 1)
                .collect::<Vec<_>>()
        })
        .count()
}

// A linked restore keeps the store file's old mtime unless touched, and cargo then finds the dependency newer on every run.
#[test]
fn units_restored_by_link_leave_cargo_nothing_to_rebuild() {
    let sb = Sandbox::new("lk-fresh");
    let root = path_workspace(&sb);
    build(&sb, &root, "force");
    sb.lose_target(&root);
    build(&sb, &root, "force");
    // The same bytes rewritten: cargo recompiles leaf and its dependents, and every one is a store hit.
    edit_leaf(&root, 0);
    build(&sb, &root, "force");
    let out = sb.bin("fcargo").args(["build", "--message-format=json"]).env("FRUST_CACHE_BACKED", "force").current_dir(&root).output().unwrap();
    assert_success(&out);
    let rebuilt: Vec<String> = stdout(&out)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|msg| msg["reason"] == "compiler-artifact" && msg["fresh"] != true)
        .map(|msg| msg["target"]["name"].as_str().unwrap_or("?").to_string())
        .collect();
    assert!(rebuilt.is_empty(), "cargo rebuilt {rebuilt:?} on a no-op");
    assert_eq!(app_output(&sb, &root), "1");
}

#[test]
fn edits_keep_one_version_per_unit_and_leave_no_store_only_linked_bytes() {
    let sb = Sandbox::new("lk-versions");
    let ws = path_workspace(&sb);
    for value in 1..=4 {
        edit_leaf(&ws, value);
        build_with(&sb, &ws, &[("FRUST_CACHE_BACKED", "force"), ("FRUST_CACHE_UNIT_VERSIONS", "1")]);
    }
    assert_eq!(entries(&sb, "leaf").len(), 1, "the edited unit keeps only its newest state");
    assert_eq!(entries(&sb, "mid").len(), 1, "a downstream unit keeps one state too");
    assert!(entries(&sb, "app").len() <= 1);
    assert_eq!(store_only_linked_files(&sb, &["leaf", "mid", "app"]), 0, "every kept linked file is still a name of a live target file");
}

// An edit saved just before the build is too new to store, as in an editor-driven loop; a future mtime makes that certain.
fn edit_leaf_unsettled(root: &Path, value: u32) {
    let path = root.join("leaf").join("src").join("lib.rs");
    fs::write(&path, leaf_source_with(value)).unwrap();
    let future = std::time::SystemTime::now() + std::time::Duration::from_secs(30);
    OpenOptions::new().write(true).open(&path).unwrap().set_modified(future).unwrap();
}

#[test]
fn a_store_skipped_for_a_fresh_edit_retires_the_entry_its_compile_superseded() {
    let sb = Sandbox::new("lk-unsettled");
    let ws = path_workspace(&sb);
    let envs = [("FRUST_CACHE_BACKED", "force"), ("FRUST_CACHE_UNIT_VERSIONS", "1")];
    edit_leaf(&ws, 1);
    build_with(&sb, &ws, &envs);
    assert_eq!(entries(&sb, "leaf").len(), 1);
    for value in 2..=4 {
        edit_leaf_unsettled(&ws, value);
        build_with(&sb, &ws, &envs);
    }
    assert!(entries(&sb, "leaf").is_empty(), "the stored state was recompiled away and no newer state could be stored");
    assert_eq!(store_only_linked_files(&sb, &["leaf", "mid", "app"]), 0, "a skipped store leaves no store-only copy of the state it replaced");
    edit_leaf(&ws, 5);
    build_with(&sb, &ws, &envs);
    assert_eq!(entries(&sb, "leaf").len(), 1, "a settled edit stores again");
    assert_eq!(store_only_linked_files(&sb, &["leaf", "mid", "app"]), 0);
}

// Whatever the payload, a compile whose store cannot follow leaves no earlier version of the unit in the store.
fn skipped_stores_keep_no_superseded_version(tag: &str, backed: &str, incremental: &str) {
    let sb = Sandbox::new(tag);
    let ws = path_workspace(&sb);
    let envs = [("FRUST_CACHE_BACKED", backed), ("FRUST_CACHE_UNIT_VERSIONS", "1"), ("CARGO_INCREMENTAL", incremental)];
    edit_leaf(&ws, 1);
    build_with(&sb, &ws, &envs);
    assert_eq!(entries(&sb, "leaf").len(), 1);
    edit_leaf_unsettled(&ws, 2);
    build_with(&sb, &ws, &envs);
    assert!(entries(&sb, "leaf").is_empty(), "{backed}, incremental {incremental}: the stored version was superseded by a compile that could not be stored");
}

#[test]
fn a_skipped_store_supersedes_copied_entries_too() {
    skipped_stores_keep_no_superseded_version("lk-skip-copy", "off", "1");
}

#[test]
fn a_skipped_store_supersedes_linked_entries_of_non_incremental_compiles() {
    skipped_stores_keep_no_superseded_version("lk-skip-whole", "force", "0");
}

#[test]
fn the_unit_versions_setting_keeps_that_many_states() {
    let sb = Sandbox::new("lk-versions-two");
    let ws = path_workspace(&sb);
    for value in 1..=4 {
        edit_leaf(&ws, value);
        build_with(&sb, &ws, &[("FRUST_CACHE_BACKED", "force"), ("FRUST_CACHE_UNIT_VERSIONS", "2")]);
    }
    assert_eq!(entries(&sb, "leaf").len(), 2);
}

#[test]
fn a_linked_store_shares_the_target_inode_instead_of_keeping_a_second_copy() {
    let (sb, a, _) = scene("lk-store");
    build(&sb, &a, "force");
    assert_eq!(app_output(&sb, &a), "41");
    let (dir, meta) = leaf_entry(&sb);
    let rlib = rlib_record(&meta);
    assert_eq!(rlib.encoding, Encoding::Linked);
    assert!(rlib.mode.is_some());
    let target_file = leaf_library(&sb.central_target(&a), ".rlib");
    assert_eq!(ino(&dir.join(&rlib.stored)), ino(&target_file), "the store file is a second name of the output");
    assert_eq!(rlib.content_hash.clone().unwrap(), frust::cache::hash_file(&target_file).unwrap());
    let dep_info = meta.files.iter().find(|file| file.text).unwrap();
    assert!(dep_info.mode.is_none() && dep_info.inline.is_some(), "dep-info stays small text inside the entry record");
    let formats = manifest_formats(&sb);
    assert!(formats.contains(&8) && formats.iter().all(|f| matches!(f, 7 | 8)), "{formats:?}");
}

#[test]
fn every_entry_of_a_linked_build_passes_the_group_validation_the_bench_uses() {
    let (sb, a, _) = scene("lk-validate");
    build(&sb, &a, "force");
    let mut checked = 0;
    for krate in ["leaf", "app"] {
        for (dir, meta) in entries(&sb, krate) {
            for group in frust::cache::metadata_slice::validate_groups(&meta.files).unwrap() {
                frust::cache::metadata_slice::verify_group(&dir, &meta.files, &group).unwrap();
                checked += 1;
            }
        }
    }
    assert!(checked > 0);
}

#[test]
fn a_second_workspace_gets_a_copy_and_never_shares_an_inode() {
    let (sb, a, b) = scene("lk-second");
    build(&sb, &a, "force");
    let held_a = leaf_library(&sb.central_target(&a), ".rlib");
    let bytes = fs::read(&held_a).unwrap();
    build(&sb, &b, "force");
    assert_eq!(sb.count_cache("hit", "leaf"), 1, "{:#?}", sb.cache_events());
    assert_eq!(app_output(&sb, &b), "41");
    let copy = leaf_library(&sb.central_target(&b), ".rlib");
    assert_eq!(fs::read(&copy).unwrap(), bytes, "the restored output has exactly the stored bytes");
    let (dir, meta) = leaf_entry(&sb);
    assert_ne!(ino(&copy), ino(&held_a), "a copy, never a link between targets");
    assert_ne!(ino(&copy), ino(&dir.join(&rlib_record(&meta).stored)), "the store file stays shared with the first target only");
}

#[test]
fn a_rewrite_through_the_target_name_is_a_miss_and_the_dead_entry_is_replaced() {
    let (sb, a, b) = scene("lk-mutated");
    build(&sb, &a, "force");
    let held = leaf_library(&sb.central_target(&a), ".rlib");
    let mut file = OpenOptions::new().read(true).write(true).open(&held).unwrap();
    file.seek(SeekFrom::Start(2048)).unwrap();
    file.write_all(&[0xA5, 0x5A]).unwrap();
    drop(file);

    build(&sb, &b, "force");
    assert_eq!(sb.count_cache("hit", "leaf"), 0, "{:#?}", sb.cache_events());
    assert_eq!(sb.count_cache("miss", "leaf"), 2, "rustc ran again");
    assert_eq!(app_output(&sb, &b), "41");
    let (dir, meta) = leaf_entry(&sb);
    let fresh = leaf_library(&sb.central_target(&b), ".rlib");
    assert_eq!(ino(&dir.join(&rlib_record(&meta).stored)), ino(&fresh), "the entry now rests on the fresh build");
}

#[test]
fn a_deleted_target_still_restores_from_the_store_file_and_the_store_adopts_the_new_output() {
    let (sb, a, b) = scene("lk-deleted");
    build(&sb, &a, "force");
    let bytes = fs::read(leaf_library(&sb.central_target(&a), ".rlib")).unwrap();
    sb.lose_target(&a);
    build(&sb, &b, "force");
    assert_eq!(sb.count_cache("hit", "leaf"), 1, "{:#?}", sb.cache_events());
    assert_eq!(sb.count_cache("miss", "leaf"), 1);
    assert_eq!(app_output(&sb, &b), "41");
    let restored = leaf_library(&sb.central_target(&b), ".rlib");
    assert_eq!(fs::read(&restored).unwrap(), bytes);
    let (dir, meta) = leaf_entry(&sb);
    assert_eq!(ino(&dir.join(&rlib_record(&meta).stored)), ino(&restored), "an orphaned store file shares the restored output");
}

// A keyed variable stands in for a build script whose static library differs on every run: the clean rebuild misses and recompiles.
fn rebuild_with_new_key(tag: &str, backed: &str, share: bool) -> Sandbox {
    let (sb, a, b) = scene(tag);
    // Sources under CARGO_HOME/registry/src are what frust treats as registry crates, which keep no unit versions.
    let registry = sb.cargo_home().join("registry").join("src").join("vendored");
    fs::create_dir_all(registry.parent().unwrap()).unwrap();
    frust::sys::rename(&sb.vendor(), &registry).unwrap();
    let config = sb.cargo_home().join("config.toml");
    let text = fs::read_to_string(&config).unwrap().replace(&sb.vendor().display().to_string(), &registry.display().to_string());
    fs::write(&config, text).unwrap();
    let build_keyed = |project: &Path, target: &str| {
        let mut command = sb.bin("fcargo");
        command.arg("build").current_dir(project).env("FRUST_CACHE_BACKED", backed).env("MACOSX_DEPLOYMENT_TARGET", target);
        assert_success(&command.output().unwrap());
    };
    build_keyed(&a, "11.0");
    if share {
        build_keyed(&b, "11.0");
    }
    sb.lose_target(&a);
    build_keyed(&a, "12.0");
    assert_eq!(sb.count_cache("miss", "leaf"), 2, "{:#?}", sb.cache_events());
    assert_eq!(app_output(&sb, &a), "41");
    sb
}

#[test]
fn a_registry_recompile_retires_the_entry_only_its_workspace_used() {
    for backed in ["force", "off"] {
        let sb = rebuild_with_new_key(&format!("lk-lineage-{backed}"), backed, false);
        assert_eq!(entries(&sb, "leaf").len(), 1, "{backed}: the replaced state is retired with the recompile");
        assert_eq!(store_only_linked_files(&sb, &["leaf"]), 0);
    }
}

#[test]
fn a_registry_recompile_keeps_an_entry_another_workspace_restored() {
    let sb = rebuild_with_new_key("lk-lineage-shared", "off", true);
    assert_eq!(entries(&sb, "leaf").len(), 2, "the other checkout still uses the first state");
}

#[test]
fn off_keeps_ordinary_payloads_and_older_manifests_and_entries_still_restore() {
    let (sb, a, b) = scene("lk-off");
    build(&sb, &a, "off");
    let (dir, meta) = leaf_entry(&sb);
    for file in &meta.files {
        assert!(file.mode.is_none() && file.encoding != Encoding::Linked, "{file:?}");
        assert!(file.inline.is_some() || dir.join(&file.stored).exists(), "{} keeps its payload", file.name);
    }
    assert_eq!(manifest_formats(&sb).iter().filter(|f| **f == 8).count(), 0);
    for path in manifests(&sb) {
        let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        value["format"] = serde_json::json!(4);
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    }
    build(&sb, &b, "force");
    assert_eq!(sb.count_cache("hit", "leaf"), 1, "{:#?}", sb.cache_events());
    assert_eq!(app_output(&sb, &b), "41");
    assert_eq!(entries(&sb, "leaf").len(), 1, "an older entry is used, not shadowed by a linked one");
}

#[test]
fn auto_links_outputs_exactly_when_the_cache_volume_has_no_clones() {
    let (sb, a, _) = scene("lk-auto");
    build(&sb, &a, "auto");
    let (_, meta) = leaf_entry(&sb);
    let linked = meta.files.iter().any(|file| file.encoding == Encoding::Linked);
    assert_eq!(linked, frust::cache::payload::clones_unavailable(&sb.home().join("tmp")), "{meta:#?}");
    assert_eq!(app_output(&sb, &a), "41");
}

// A vendored crate whose build script is big enough to be stored as a link.
fn scene_with_build_script(tag: &str) -> (Sandbox, PathBuf) {
    let sb = Sandbox::new(tag);
    sb.vendor_crate("leaf", "1.0.0", &leaf_source());
    let build_rs = format!("fn main() {{ println!(\"cargo:rerun-if-changed=build.rs\"); let mut s = 0u64; {} println!(\"{{}}\", s); }}\n", (0..400).map(|n| format!("s = s.wrapping_mul({}).wrapping_add({n});", n + 7)).collect::<String>());
    fs::write(sb.vendor().join("leaf-1.0.0").join("build.rs"), build_rs).unwrap();
    let a = sb.project("a", &[("leaf", "1.0.0")], MAIN);
    (sb, a)
}

fn script_entry_files(sb: &Sandbox) -> Vec<(PathBuf, frust::cache::StoredFile)> {
    entries(sb, "build_script_build")
        .into_iter()
        .flat_map(|(dir, meta)| meta.files.into_iter().filter(|f| f.encoding == Encoding::Linked).map(move |f| (dir.clone(), f)).collect::<Vec<_>>())
        .collect()
}

#[test]
fn a_restore_beside_cargos_alias_of_a_removed_output_links_instead_of_copying() {
    let (sb, a) = scene_with_build_script("lk-alias");
    build(&sb, &a, "force");
    let profile = sb.central_target(&a).join("debug");
    let script = walk(&profile.join("build")).into_iter().find(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("build_script_build-") && p.extension().is_none_or(|e| e == std::env::consts::EXE_EXTENSION))).expect("the build script executable");
    let hashed = script.file_stem().unwrap().to_string_lossy().rsplit_once('-').unwrap().1.to_string();
    // Cargo hard-links the executable to `build-script-build` and removes the hashed name before the next compile of the unit.
    let alias = script.with_file_name(format!("build-script-build{}", std::env::consts::EXE_SUFFIX));
    let _ = fs::remove_file(&alias);
    fs::hard_link(&script, &alias).unwrap();
    fs::remove_file(&script).unwrap();
    for entry in fs::read_dir(profile.join(".fingerprint")).unwrap().flatten() {
        if entry.file_name().to_string_lossy().ends_with(&hashed) {
            fs::remove_dir_all(entry.path()).unwrap();
        }
    }
    build(&sb, &a, "force");
    // On windows-msvc the script's PDB is stored as a link beside it.
    let executable = |name: &str| Path::new(name).extension().is_none_or(|e| e == std::env::consts::EXE_EXTENSION);
    let stored: Vec<_> = script_entry_files(&sb).into_iter().filter(|(_, f)| executable(&f.name)).collect();
    assert_eq!(stored.len(), 1, "{stored:?}");
    let (dir, file) = &stored[0];
    assert_eq!(ino(&dir.join(&file.stored)), ino(&script), "the restored executable is the store file's inode, not a copy");
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() { out.extend(walk(&p)) } else { out.push(p) }
    }
    out
}
