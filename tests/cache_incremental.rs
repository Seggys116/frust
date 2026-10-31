//! Incremental units are cached keyed without the session directory; a miss runs rustc's own incremental compile.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::{Sandbox, assert_success, run_binary, settle, stdout};

fn lib_source(value: u32) -> String {
    format!("pub fn value() -> u32 {{ {value} }}\npub fn name() -> &'static str {{ \"pathdep\" }}\n")
}

const MAIN: &str = "fn main() { println!(\"{} {}\", pathdep::name(), pathdep::value()); }\n";

fn path_crate(dir: &Path, value: u32) {
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(dir.join("Cargo.toml"), "[package]\nname = \"pathdep\"\nversion = \"0.1.0\"\nedition = \"2021\"\n")
        .unwrap();
    fs::write(dir.join("src").join("lib.rs"), lib_source(value)).unwrap();
    settle(&dir.join("src").join("lib.rs"));
}

fn app_with_path_dep(sb: &Sandbox, dir: &str, dep_path: &str) -> PathBuf {
    let app = sb.root.join(dir);
    fs::create_dir_all(app.join("src")).unwrap();
    fs::write(
        app.join("Cargo.toml"),
        format!(
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n\n[dependencies]\npathdep = {{ path = \"{dep_path}\" }}\n"
        ),
    )
    .unwrap();
    fs::write(app.join("src").join("main.rs"), MAIN).unwrap();
    settle(&app.join("src").join("main.rs"));
    app
}

/// The pause keeps the edit clear of the "file too new" rule, which declines to store a compile whose inputs just changed.
fn edit(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    settle(path);
}

fn build(sb: &Sandbox, project: &Path) {
    assert_success(&sb.bin("fcargo").arg("build").current_dir(project).output().unwrap());
}

fn output(sb: &Sandbox, project: &Path) -> String {
    let out = run_binary(&sb.central_target(project), "debug", "app");
    assert_success(&out);
    stdout(&out).trim().to_string()
}

fn sessions(sb: &Sandbox, project: &Path, krate: &str) -> usize {
    let dir = sb.central_target(project).join("debug").join("incremental");
    fs::read_dir(dir)
        .map(|rd| rd.flatten().filter(|e| e.file_name().to_string_lossy().starts_with(&format!("{krate}-"))).count())
        .unwrap_or(0)
}

#[test]
fn path_dependency_is_restored_after_clean_and_in_another_project() {
    let sb = Sandbox::new("inc-reuse");
    path_crate(&sb.root.join("shared").join("pathdep"), 7);
    let a = app_with_path_dep(&sb, "a", "../shared/pathdep");
    let b = app_with_path_dep(&sb, "b", "../shared/pathdep");

    build(&sb, &a);
    assert!(
        sessions(&sb, &a, "pathdep") > 0,
        "the path dependency was compiled incrementally, so this exercises incremental units"
    );
    assert_eq!(sb.count_cache("miss", "pathdep"), 1, "the incremental path dependency is stored");
    assert_eq!(output(&sb, &a), "pathdep 7");

    assert_success(&sb.bin("fcargo").arg("clean").current_dir(&a).output().unwrap());
    build(&sb, &a);
    assert_eq!(sb.count_cache("hit", "pathdep"), 1, "restored after a clean");
    assert_eq!(output(&sb, &a), "pathdep 7");

    build(&sb, &b);
    assert_eq!(sb.count_cache("hit", "pathdep"), 2, "restored for another project using the same path crate");
    assert_eq!(output(&sb, &b), "pathdep 7");

    // `b` is a different workspace, so its `app` is a different unit.
    assert_eq!(sb.count_cache("hit", "app"), 1, "the primary package is restored after a clean");
    assert_eq!(sb.count_cache("miss", "app"), 2, "compiled once per workspace");
}

#[test]
fn restored_path_dependency_and_its_session_survive_later_edits() {
    let sb = Sandbox::new("inc-edit");
    let app = app_with_path_dep(&sb, "ws", "crates/pathdep");
    let dep = app.join("crates").join("pathdep");
    path_crate(&dep, 1);
    let lib_rs = dep.join("src").join("lib.rs");
    let main_rs = app.join("src").join("main.rs");

    build(&sb, &app);
    assert_eq!(output(&sb, &app), "pathdep 1");
    edit(&lib_rs, &lib_source(2));
    build(&sb, &app);
    assert_eq!(output(&sb, &app), "pathdep 2");
    assert_eq!(sb.count_cache("miss", "pathdep"), 2);
    assert_eq!(sb.count_cache("hit", "pathdep"), 0);

    // After restoring version 1, rustc's session directory still describes version 2.
    edit(&lib_rs, &lib_source(1));
    build(&sb, &app);
    assert_eq!(sb.count_cache("hit", "pathdep"), 1, "the reverted source is served from the cache");
    assert_eq!(output(&sb, &app), "pathdep 1");
    assert!(sessions(&sb, &app, "pathdep") > 0, "the earlier session directory is still there");

    edit(&lib_rs, &lib_source(3).replace("\"pathdep\"", "\"edited\""));
    build(&sb, &app);
    assert_eq!(output(&sb, &app), "edited 3");
    assert_eq!(sb.count_cache("miss", "pathdep"), 3);
    edit(&lib_rs, &lib_source(4).replace("\"pathdep\"", "\"edited\""));
    build(&sb, &app);
    assert_eq!(output(&sb, &app), "edited 4");

    edit(&main_rs, &MAIN.replace("{} {}", "{}-{}"));
    build(&sb, &app);
    assert_eq!(output(&sb, &app), "edited-4");
    edit(&main_rs, MAIN);
    build(&sb, &app);
    assert_eq!(output(&sb, &app), "edited 4");
    assert!(sessions(&sb, &app, "app") > 0, "the primary package keeps its incremental session");
    assert_eq!(sb.count_cache("hit", "app"), 2, "the reverted path dependency and the reverted main are restored");

    let before = sb.cache_events().len();
    build(&sb, &app);
    assert_eq!(sb.cache_events().len(), before);
}

#[test]
fn a_path_dependency_selected_for_building_is_cached_unless_turned_off() {
    let sb = Sandbox::new("inc-primary");
    let app = app_with_path_dep(&sb, "ws", "crates/pathdep");
    path_crate(&app.join("crates").join("pathdep"), 5);

    // `-p pathdep` makes the member a primary package; it is still stored and restored.
    assert_success(&sb.bin("fcargo").args(["build", "-p", "pathdep"]).current_dir(&app).output().unwrap());
    assert_eq!(sb.count_cache("miss", "pathdep"), 1);
    assert!(sessions(&sb, &app, "pathdep") > 0, "the miss was rustc's own incremental compile");
    assert_success(&sb.bin("fcargo").arg("clean").current_dir(&app).output().unwrap());
    assert_success(&sb.bin("fcargo").args(["build", "-p", "pathdep"]).current_dir(&app).output().unwrap());
    assert_eq!(sb.count_cache("hit", "pathdep"), 1);
    assert_success(&sb.bin("fcargo").arg("clean").current_dir(&app).output().unwrap());
    build(&sb, &app);
    assert_eq!(
        (sb.count_cache("hit", "pathdep"), sb.count_cache("miss", "pathdep")),
        (2, 1),
        "shared with the non-primary unit"
    );
    assert_eq!(output(&sb, &app), "pathdep 5");

    // FRUST_CACHE_PRIMARY_INCREMENTAL=off leaves the selected package to rustc.
    assert_success(&sb.bin("fcargo").arg("clean").current_dir(&app).output().unwrap());
    let off = sb
        .bin("fcargo")
        .args(["build", "-p", "pathdep"])
        .env("FRUST_CACHE_PRIMARY_INCREMENTAL", "off")
        .current_dir(&app)
        .output()
        .unwrap();
    assert_success(&off);
    assert_eq!((sb.count_cache("hit", "pathdep"), sb.count_cache("miss", "pathdep")), (2, 1));
}

#[test]
fn incremental_entries_are_switchable_and_never_serve_non_incremental_builds() {
    let sb = Sandbox::new("inc-domain");
    let app = app_with_path_dep(&sb, "ws", "crates/pathdep");
    path_crate(&app.join("crates").join("pathdep"), 9);

    let off = sb.bin("fcargo").arg("build").env("FRUST_CACHE_INCREMENTAL", "off").current_dir(&app).output().unwrap();
    assert_success(&off);
    assert_eq!(sb.count_cache("miss", "pathdep"), 0, "FRUST_CACHE_INCREMENTAL=off leaves incremental units uncached");

    assert_success(&sb.bin("fcargo").arg("clean").current_dir(&app).output().unwrap());
    build(&sb, &app);
    assert_eq!(sb.count_cache("miss", "pathdep"), 1);

    // Incremental outputs are not byte-identical to non-incremental ones, so they are a different entry.
    assert_success(&sb.bin("fcargo").arg("clean").current_dir(&app).output().unwrap());
    let plain = sb.bin("fcargo").arg("build").env("CARGO_INCREMENTAL", "0").current_dir(&app).output().unwrap();
    assert_success(&plain);
    assert_eq!(sb.count_cache("hit", "pathdep"), 0, "a non-incremental build is not served an incremental entry");
    assert_eq!(sb.count_cache("miss", "pathdep"), 2);
    assert_eq!(output(&sb, &app), "pathdep 9");
}
