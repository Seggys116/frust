//! `fcargo clean` releases the cache entries only the cleaned workspace needs, and changes nothing else about cargo's behaviour.

#![cfg(unix)]

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

use common::{Sandbox, stderr, stdout};
use frust::cache::Cache;
use frust::paths::Dirs;

const CARGO: &str = r#"#!/bin/sh
case "$1" in
  -V) printf 'cargo 9.9.9 (clean test)\n'; exit 0 ;;
  locate-project) printf '%s/Cargo.toml\n' "$PWD"; exit 0 ;;
  clean|metadata)
    { printf 'call\n'; for a in "$@"; do printf 'arg=%s\n' "$a"; done; } >> "$FRUST_TEST_LOG"
    printf 'CARGO-OUT\n'
    printf 'CARGO-ERR\n' >&2
    if [ "$1" = clean ] && [ -n "$CARGO_TARGET_DIR" ]; then rm -rf "$CARGO_TARGET_DIR"; fi
    exit "${FRUST_TEST_EXIT:-0}" ;;
esac
exit 0
"#;

struct Scene {
    sb: Sandbox,
    cargo: PathBuf,
    log: PathBuf,
}

impl Scene {
    fn new(tag: &str) -> Self {
        let sb = Sandbox::new(tag);
        let cargo = common::write_fake_tool(&sb.root.join("cargo"), CARGO, "");
        let log = sb.root.join("cargo-calls");
        // A fresh marker keeps the daily collector from running over the entries a test publishes.
        fs::write(Dirs::at(sb.home()).auto_gc_marker(), b"").unwrap();
        Self { sb, cargo, log }
    }

    fn cache(&self) -> Cache {
        Cache::new(&Dirs::at(self.sb.home()))
    }

    fn workspace(&self, project: &Path) -> String {
        let out = self.sb.bin("frust").arg("where").current_dir(project).env("FRUST_CARGO", &self.cargo).output().unwrap();
        common::assert_success(&out);
        let target = PathBuf::from(stdout(&out).trim());
        target.parent().unwrap().file_name().unwrap().to_string_lossy().into_owned()
    }

    fn fcargo(&self, project: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
        let mut cmd = self.sb.bin("fcargo");
        cmd.args(args).current_dir(project).env("FRUST_CARGO", &self.cargo).env("FRUST_TEST_LOG", &self.log);
        for (name, value) in envs {
            cmd.env(name, value);
        }
        cmd.output().unwrap()
    }

    /// The arguments of every recorded cargo call, with calls separated by `call`.
    fn calls(&self) -> Vec<String> {
        let text = fs::read_to_string(&self.log).unwrap_or_default();
        let calls = text.lines().map(str::to_owned).collect();
        let _ = fs::remove_file(&self.log);
        calls
    }

    fn workspace_dir(&self, workspace: &str) -> PathBuf {
        self.sb.home().join("targets").join(workspace)
    }
}

/// Arguments and extra environment of one fcargo run.
type Case<'a> = (Vec<&'a str>, Vec<(&'a str, &'a str)>);

fn key(n: u8) -> String {
    format!("{n:02x}").repeat(20)
}

fn publish(cache: &Cache, n: u8, attribution: Option<(&str, &[&str])>) {
    let mut meta = serde_json::json!({"crate_name": "c", "version": "1", "compile_ms": 1, "created_unix": 1, "files": [], "stderr": "stderr"});
    if let Some((source, workspaces)) = attribution {
        meta["source"] = source.into();
        meta["workspaces"] = serde_json::json!(workspaces);
    }
    let staged = cache.stage().unwrap();
    fs::write(staged.join("meta.json"), serde_json::to_vec(&meta).unwrap()).unwrap();
    fs::write(staged.join("f0"), vec![n; 8192]).unwrap();
    cache.publish(&staged, &key(n)).unwrap();
}

fn users(cache: &Cache, n: u8) -> Vec<String> {
    cache.load_entry(&key(n)).unwrap().1.workspaces
}

#[test]
fn clean_retires_path_entries_only_the_workspace_used_and_keeps_registry_entries() {
    let scene = Scene::new("clean-whole");
    let a = scene.sb.project("a", &[], "fn main() {}\n");
    let b = scene.sb.project("b", &[], "fn main() {}\n");
    let (wa, wb) = (scene.workspace(&a), scene.workspace(&b));
    let cache = scene.cache();
    publish(&cache, 1, Some(("path", &[wa.as_str()])));
    publish(&cache, 2, Some(("registry", &[wa.as_str(), wb.as_str()])));
    publish(&cache, 3, Some(("registry", &[wa.as_str()])));
    publish(&cache, 4, None);
    publish(&cache, 5, Some(("path", &[wb.as_str()])));

    let out = scene.fcargo(&a, &["clean"], &[]);

    assert_eq!(out.status.code(), Some(0));
    assert_eq!(stdout(&out), "CARGO-OUT\n");
    let err = stderr(&out);
    assert!(err.starts_with("CARGO-ERR\n"), "{err}");
    assert_eq!(err.matches("released").count(), 1, "{err}");
    assert!(err.contains("released 1 cache entry ("), "{err}");
    assert_eq!(scene.calls(), vec!["call", "arg=clean"]);
    assert!(!cache.has_entry(&key(1)), "the path entry only this workspace used is released");
    for kept in [2, 3, 4, 5] {
        assert!(cache.has_entry(&key(kept)), "entry {kept}");
    }
    assert_eq!(users(&cache, 2), vec![wb.clone()], "workspace b still finds the shared entry");
    assert_eq!(users(&cache, 3), vec![wa.clone()], "registry entries stay by default");
    assert!(users(&cache, 4).is_empty(), "entries without attribution are untouched");
    assert!(!scene.workspace_dir(&wa).exists(), "the emptied workspace directory goes with the target");
}

#[test]
fn the_evict_flag_also_retires_registry_and_git_entries_only_the_workspace_used() {
    let scene = Scene::new("clean-evict");
    let a = scene.sb.project("a", &[], "fn main() {}\n");
    let b = scene.sb.project("b", &[], "fn main() {}\n");
    let (wa, wb) = (scene.workspace(&a), scene.workspace(&b));
    let cache = scene.cache();
    publish(&cache, 1, Some(("registry", &[wa.as_str()])));
    publish(&cache, 2, Some(("registry", &[wa.as_str(), wb.as_str()])));
    publish(&cache, 3, Some(("git", &[wa.as_str()])));
    publish(&cache, 4, None);

    let out = scene.fcargo(&a, &["clean", "--frust-evict-registry"], &[]);

    assert_eq!(out.status.code(), Some(0));
    assert_eq!(scene.calls(), vec!["call", "arg=clean"], "cargo never sees the frust flag");
    assert!(stderr(&out).contains("released 2 cache entries ("), "{}", stderr(&out));
    assert!(!cache.has_entry(&key(1)) && !cache.has_entry(&key(3)));
    assert!(cache.has_entry(&key(2)) && cache.has_entry(&key(4)));
    assert_eq!(users(&cache, 2), vec![wb], "an entry another workspace uses is kept");
}

#[test]
fn clean_returns_cargos_exit_code_and_prints_nothing_extra_when_nothing_is_released() {
    let scene = Scene::new("clean-exit");
    let a = scene.sb.project("a", &[], "fn main() {}\n");
    let out = scene.fcargo(&a, &["clean", "-q"], &[("FRUST_TEST_EXIT", "3")]);
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(stdout(&out), "CARGO-OUT\n");
    assert_eq!(stderr(&out), "CARGO-ERR\n");
    assert_eq!(scene.calls(), vec!["call", "arg=clean", "arg=-q"]);
}

#[test]
fn clean_that_frust_cannot_attribute_is_passed_straight_to_cargo() {
    let scene = Scene::new("clean-passthrough");
    let a = scene.sb.project("a", &[], "fn main() {}\n");
    let wa = scene.workspace(&a);
    let cache = scene.cache();
    publish(&cache, 1, Some(("path", &[wa.as_str()])));
    publish(&cache, 2, Some(("registry", &[wa.as_str()])));
    let elsewhere = scene.sb.root.join("elsewhere");
    let elsewhere = elsewhere.to_str().unwrap();

    let cases: Vec<Case> = vec![
        (vec!["clean", "--target-dir", elsewhere], vec![]),
        (vec!["clean"], vec![("CARGO_TARGET_DIR", elsewhere)]),
        (vec!["clean"], vec![("FRUST_TARGET", "local")]),
        (vec!["clean", "-p", "app", "--frust-evict-registry"], vec![]),
        (vec!["clean", "--release", "--frust-evict-registry"], vec![]),
        (vec!["clean", "--profile", "dev"], vec![]),
        (vec!["clean", "--doc"], vec![]),
        (vec!["clean", "--dry-run"], vec![]),
    ];
    for (args, envs) in cases {
        let out = scene.fcargo(&a, &args, &envs);
        assert_eq!(out.status.code(), Some(0), "{args:?}");
        assert!(!stderr(&out).contains("released"), "{args:?}: {}", stderr(&out));
        let mut expected = vec!["call".to_string()];
        expected.extend(args.iter().filter(|arg| **arg != "--frust-evict-registry").map(|arg| format!("arg={arg}")));
        assert_eq!(scene.calls(), expected, "{args:?}");
        assert!(cache.has_entry(&key(1)) && cache.has_entry(&key(2)), "{args:?}");
        assert_eq!(users(&cache, 1), vec![wa.clone()], "{args:?}");
    }
}

#[test]
fn the_evict_flag_means_nothing_to_other_commands() {
    let scene = Scene::new("clean-flag-elsewhere");
    let a = scene.sb.project("a", &[], "fn main() {}\n");
    let out = scene.fcargo(&a, &["metadata", "--frust-evict-registry"], &[]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(scene.calls(), vec!["call", "arg=metadata", "arg=--frust-evict-registry"]);
}
