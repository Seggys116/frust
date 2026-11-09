//! An incremental unit's entry keeps the rustc session its compile left, so a restore leaves the next edit an incremental rebuild.

mod common;

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{Sandbox, assert_success, run_binary, settle, stdout};

const MODULES: usize = 6;

fn module(index: usize, value: usize) -> String {
    format!("pub fn value() -> usize {{ {value} * {} }}\n", index + 1)
}

/// One module per file gives rustc one codegen unit per module, so an edit of one leaves the others to be reused.
fn project(sb: &Sandbox) -> PathBuf {
    let mods: String = (0..MODULES).map(|i| format!("mod m{i};\n")).collect();
    let sum: Vec<String> = (0..MODULES).map(|i| format!("m{i}::value()")).collect();
    let app = sb.project("app", &[], &format!("{mods}fn main() {{ println!(\"{{}}\", {}); }}\n", sum.join(" + ")));
    for i in 0..MODULES {
        edit(&app.join("src").join(format!("m{i}.rs")), &module(i, 1));
    }
    app
}

fn edit(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    settle(path);
}

fn build(sb: &Sandbox, project: &Path, sessions: &str) {
    assert_success(&sb.bin("fcargo").arg("build").env("FRUST_CACHE_SESSIONS", sessions).current_dir(project).output().unwrap());
}

fn output(sb: &Sandbox, project: &Path) -> String {
    let out = run_binary(&sb.central_target(project), "debug", "app");
    assert_success(&out);
    stdout(&out).trim().to_string()
}

/// rustc's finalized session directories of the crate: `s-<time>-<random>-<svh>`, never a working directory or lock.
fn sessions(sb: &Sandbox, project: &Path) -> Vec<PathBuf> {
    let incremental = sb.central_target(project).join("debug").join("incremental");
    let mut found = Vec::new();
    for krate in fs::read_dir(incremental).into_iter().flatten().flatten() {
        if !krate.file_name().to_string_lossy().starts_with("app-") {
            continue;
        }
        for session in fs::read_dir(krate.path()).unwrap().flatten() {
            let name = session.file_name().to_string_lossy().into_owned();
            if name.starts_with("s-") && !name.ends_with(".lock") && !name.ends_with("-working") {
                found.push(session.path());
            }
        }
    }
    found
}

fn inodes(dir: &Path) -> HashSet<(u64, u64)> {
    fs::read_dir(dir).unwrap().flatten().map(|e| frust::sys::lstat(&e.path()).unwrap()).map(|m| (m.dev(), m.ino())).collect()
}

/// Store entries of the `app` crate, newest first, each with whether it still holds a session.
fn app_entries(sb: &Sandbox) -> Vec<(u64, bool)> {
    let mut found = Vec::new();
    let mut dirs: Vec<PathBuf> = fs::read_dir(sb.home().join("store")).into_iter().flatten().flatten().map(|e| e.path()).collect();
    while let Some(dir) = dirs.pop() {
        let Ok(text) = fs::read_to_string(dir.join("meta.json")) else {
            dirs.extend(fs::read_dir(&dir).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_dir()));
            continue;
        };
        let meta: serde_json::Value = serde_json::from_str(&text).unwrap();
        if meta["crate_name"] == "app" {
            let created = frust::sys::lstat(&dir.join("meta.json")).unwrap().ctime();
            found.push((created.0 as u64 * 1_000_000_000 + created.1 as u64, dir.join("session").is_dir()));
        }
    }
    found.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    found
}

#[test]
fn a_restored_primary_crate_gets_its_session_back_and_the_next_edit_reuses_it() {
    let sb = Sandbox::new("session-restore");
    let app = project(&sb);
    build(&sb, &app, "on");
    let compiled: Vec<_> = sessions(&sb, &app).iter().map(|p| p.file_name().unwrap().to_owned()).collect();
    assert_eq!(compiled.len(), 1, "the compile left one session");

    sb.lose_target(&app);
    build(&sb, &app, "on");
    assert_eq!(sb.count_cache("hit", "app"), 1, "the primary package is restored after the target was lost");
    let restored = sessions(&sb, &app);
    assert_eq!(restored.iter().map(|p| p.file_name().unwrap().to_owned()).collect::<Vec<_>>(), compiled, "the stored session is back under its own name");
    assert!(restored[0].join("dep-graph.bin").is_file());
    let carried = inodes(&restored[0]);

    edit(&app.join("src").join("m0.rs"), &module(0, 100));
    build(&sb, &app, "on");
    assert_eq!(output(&sb, &app), (100 + (2..=MODULES).sum::<usize>()).to_string());
    // rustc keeps the session it started from beside the one it finalized until its next compile.
    let next: Vec<PathBuf> = sessions(&sb, &app).into_iter().filter(|session| session != &restored[0]).collect();
    assert_eq!(next.len(), 1, "the edit finalized a new session");
    // rustc hard-links the files of the session it starts from, and keeps them for every codegen unit the edit left alone.
    assert!(!inodes(&next[0]).is_disjoint(&carried), "the edit reused codegen units of the restored session");
}

#[test]
fn only_the_newest_entry_of_a_unit_keeps_a_session_and_the_switch_turns_them_off() {
    let sb = Sandbox::new("session-retention");
    let app = project(&sb);
    build(&sb, &app, "on");
    for (i, value) in [(1, 2), (2, 3)] {
        edit(&app.join("src").join(format!("m{i}.rs")), &module(i, value));
        build(&sb, &app, "on");
    }
    let entries = app_entries(&sb);
    assert!(entries.len() >= 2, "earlier states are still stored: {entries:?}");
    assert_eq!(entries.iter().map(|e| e.1).collect::<Vec<_>>()[..2], [true, false], "the newest entry holds the session");
    assert_eq!(entries.iter().filter(|e| e.1).count(), 1, "one session per unit: {entries:?}");

    edit(&app.join("src").join("m3.rs"), &module(3, 4));
    build(&sb, &app, "off");
    assert!(!app_entries(&sb)[0].1, "with sessions off the entry holds artifacts only");
    sb.lose_target(&app);
    build(&sb, &app, "off");
    assert!(sessions(&sb, &app).is_empty(), "with sessions off a restore leaves no session");
    assert_eq!(output(&sb, &app), (1 + 2 * 2 + 3 * 3 + 4 * 4 + 5 + 6).to_string());
}

#[test]
fn a_deferred_store_keeps_the_session_its_compile_finalized() {
    let sb = Sandbox::new("session-deferred");
    let app = project(&sb);
    assert_success(&sb.bin("fcargo").arg("build").env_remove("FRUST_STORE_SYNC").current_dir(&app).output().unwrap());
    let deferred = sb.home().join("tmp").join("deferred");
    let pending = || fs::read_dir(&deferred).into_iter().flatten().flatten().any(|e| e.file_name().to_string_lossy().starts_with("job-"));
    let begun = Instant::now();
    while pending() || !app_entries(&sb).first().is_some_and(|entry| entry.1) {
        assert!(begun.elapsed() < Duration::from_secs(120), "the deferred store kept no session: {:?}", app_entries(&sb));
        std::thread::sleep(Duration::from_millis(50));
    }
    sb.lose_target(&app);
    build(&sb, &app, "on");
    assert_eq!(sb.count_cache("hit", "app"), 1);
    assert_eq!(sessions(&sb, &app).len(), 1, "the session the helper stored came back with the outputs");
}

#[test]
fn an_edit_after_the_sessions_were_retired_starts_from_the_stored_one() {
    let sb = Sandbox::new("session-seed");
    let app = project(&sb);
    build(&sb, &app, "on");
    // As session cleanup of an idle target would: the outputs stay, the incremental directory goes.
    fs::remove_dir_all(sb.central_target(&app).join("debug").join("incremental")).unwrap();
    edit(&app.join("src").join("m2.rs"), &module(2, 7));
    build(&sb, &app, "on");
    assert_eq!(output(&sb, &app), (1 + 2 + 7 * 3 + 4 + 5 + 6).to_string());
    let both = sessions(&sb, &app);
    assert_eq!(both.len(), 2, "the seeded session stays beside the one the edit finalized");
    assert!(!inodes(&both[0]).is_disjoint(&inodes(&both[1])), "the edit reused codegen units of the seeded session");
}
