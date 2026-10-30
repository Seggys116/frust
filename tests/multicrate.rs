//! Unchanged members are restored from the cache after a clean or branch switch; edited members and their dependents always compile.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

use common::{Sandbox, assert_success, run_binary, stdout};

#[derive(Clone)]
struct State {
    base_value: u32,
    left_wrap: &'static str,
    right_offset: u32,
    app_prefix: &'static str,
}

const A: State = State { base_value: 10, left_wrap: "[{}]", right_offset: 1, app_prefix: "app" };

fn manifest(name: &str, deps: &[&str], extra: &str) -> String {
    let mut m =
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{extra}\n[dependencies]\n");
    for dep in deps {
        m.push_str(&format!("{dep} = {{ path = \"../{dep}\" }}\n"));
    }
    m
}

struct Ws {
    sb: Sandbox,
    root: PathBuf,
}

impl Ws {
    fn new(tag: &str) -> Ws {
        let sb = Sandbox::new(tag);
        let root = sb.root.join("ws");
        let members = ["base", "left", "right", "top", "pm", "gen", "app"];
        let listed: Vec<String> = members.iter().map(|m| format!("\"{m}\"")).collect();
        write(&root.join("Cargo.toml"), &format!("[workspace]\nresolver = \"2\"\nmembers = [{}]\n", listed.join(", ")));
        write(&root.join("left/Cargo.toml"), &manifest("left", &["base"], ""));
        write(&root.join("right/Cargo.toml"), &manifest("right", &["base"], ""));
        write(&root.join("base/Cargo.toml"), &manifest("base", &[], ""));
        write(&root.join("top/Cargo.toml"), &manifest("top", &["left", "right"], ""));
        write(&root.join("pm/Cargo.toml"), &manifest("pm", &[], "\n[lib]\nproc-macro = true\n"));
        write(&root.join("gen/Cargo.toml"), &manifest("gen", &[], "build = \"build.rs\"\n"));
        write(&root.join("app/Cargo.toml"), &manifest("app", &["top", "pm", "gen"], ""));
        write(
            &root.join("pm/src/lib.rs"),
            "use proc_macro::TokenStream;\n#[proc_macro]\npub fn answer(_: TokenStream) -> TokenStream { \"42u32\".parse().unwrap() }\n",
        );
        write(
            &root.join("gen/build.rs"),
            "fn main() {\n    let out = std::env::var(\"OUT_DIR\").unwrap();\n    std::fs::write(std::path::Path::new(&out).join(\"gen.rs\"), \"pub const GENERATED: u32 = 7;\\n\").unwrap();\n    println!(\"cargo:rerun-if-changed=build.rs\");\n}\n",
        );
        write(&root.join("gen/src/lib.rs"), "include!(concat!(env!(\"OUT_DIR\"), \"/gen.rs\"));\n");
        write(
            &root.join("top/src/lib.rs"),
            "pub fn describe() -> String { left::wrap(left::scaled() + right::shifted()) }\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn describes() { assert!(!super::describe().is_empty()); }\n}\n",
        );
        write(&root.join("top/tests/it.rs"), "#[test]\nfn integration() { assert!(top::describe().len() > 2); }\n");
        let ws = Ws { sb, root };
        ws.apply(&A);
        ws
    }

    /// The pause keeps edits clear of the store's "file too new" rule.
    fn apply(&self, s: &State) {
        let files = [
            ("base/src/lib.rs", format!("pub fn value() -> u32 {{ {} }}\n", s.base_value)),
            (
                "left/src/lib.rs",
                format!(
                    "pub fn scaled() -> u32 {{ base::value() * 2 }}\npub fn wrap<T: std::fmt::Display>(t: T) -> String {{ format!(\"{}\", t) }}\n",
                    s.left_wrap
                ),
            ),
            ("right/src/lib.rs", format!("pub fn shifted() -> u32 {{ base::value() + {} }}\n", s.right_offset)),
            (
                "app/src/main.rs",
                format!(
                    "fn main() {{ println!(\"{} {{}} {{}} {{}}\", top::describe(), pm::answer!(), gen::GENERATED); }}\n",
                    s.app_prefix
                ),
            ),
        ];
        let mut changed = false;
        for (path, text) in files {
            let path = self.root.join(path);
            if fs::read_to_string(&path).ok().as_deref() != Some(text.as_str()) {
                write(&path, &text);
                changed = true;
            }
        }
        if changed {
            thread::sleep(Duration::from_millis(400));
        }
    }

    fn fcargo(&self, args: &[&str]) -> Command {
        let mut cmd = self.sb.bin("fcargo");
        cmd.args(args).current_dir(&self.root);
        cmd
    }

    fn run(&self, args: &[&str]) {
        assert_success(&self.fcargo(args).output().unwrap());
    }

    fn events_of(&self, args: &[&str]) -> BTreeMap<String, (usize, usize)> {
        let before = self.sb.cache_events().len();
        self.run(args);
        let mut counts = BTreeMap::new();
        for event in &self.sb.cache_events()[before..] {
            let entry = counts.entry(event["crate"].as_str().unwrap_or("").to_string()).or_insert((0, 0));
            match event["result"].as_str() {
                Some("hit") => entry.0 += 1,
                Some("miss") => entry.1 += 1,
                _ => {}
            }
        }
        counts
    }

    fn output(&self) -> String {
        let out = run_binary(&self.sb.central_target(&self.root), "debug", "app");
        assert_success(&out);
        stdout(&out).trim().to_string()
    }

    fn stored_entries(&self, krate: &str) -> usize {
        let mut count = 0;
        for shard in fs::read_dir(self.sb.home().join("store")).into_iter().flatten().flatten() {
            for entry in fs::read_dir(shard.path()).into_iter().flatten().flatten() {
                let meta = fs::read_to_string(entry.path().join("meta.json")).unwrap_or_default();
                if meta.contains(&format!("\"crate_name\":\"{krate}\"")) {
                    count += 1;
                }
            }
        }
        count
    }
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn expected(s: &State) -> String {
    let n = s.base_value * 2 + s.base_value + s.right_offset;
    format!("{} {} 42 7", s.app_prefix, s.left_wrap.replace("{}", &n.to_string()))
}

const MEMBERS: [&str; 6] = ["base", "left", "right", "top", "pm", "app"];

fn hits(events: &BTreeMap<String, (usize, usize)>, krate: &str) -> usize {
    events.get(krate).map(|e| e.0).unwrap_or(0)
}

fn misses(events: &BTreeMap<String, (usize, usize)>, krate: &str) -> usize {
    events.get(krate).map(|e| e.1).unwrap_or(0)
}

#[test]
fn clean_rebuild_restores_every_member_and_an_edit_compiles_only_its_dependents() {
    let ws = Ws::new("mc-clean");
    let first = ws.events_of(&["build"]);
    for member in MEMBERS.iter().chain(&["gen"]) {
        assert_eq!(misses(&first, member), 1, "{member} is compiled once and stored: {first:?}");
    }
    assert_eq!(ws.output(), expected(&A));

    ws.run(&["clean"]);
    let rebuilt = ws.events_of(&["build"]);
    for member in MEMBERS.iter().chain(&["gen"]) {
        assert_eq!((hits(&rebuilt, member), misses(&rebuilt, member)), (1, 0), "{member} is restored: {rebuilt:?}");
    }
    assert_eq!(ws.output(), expected(&A));

    let edited = State { right_offset: 5, ..A };
    ws.apply(&edited);
    let after_edit = ws.events_of(&["build"]);
    for member in ["right", "top", "app"] {
        assert_eq!(misses(&after_edit, member), 1, "{member} depends on the edit and compiles: {after_edit:?}");
        assert_eq!(hits(&after_edit, member), 0, "{member} must not be served a stale entry");
    }
    for member in ["base", "left", "pm", "gen"] {
        assert!(!after_edit.contains_key(member), "{member} is fresh and untouched: {after_edit:?}");
    }
    assert_eq!(ws.output(), expected(&edited));

    assert!(ws.events_of(&["build"]).is_empty());
}

#[test]
fn branch_switch_back_is_restored_and_a_public_api_change_is_never_stale() {
    let ws = Ws::new("mc-branch");
    ws.run(&["build"]);
    assert_eq!(ws.output(), expected(&A));

    let b = State { base_value: 20, left_wrap: "<{}>", right_offset: 1, app_prefix: "bin" };
    ws.apply(&b);
    let to_b = ws.events_of(&["build"]);
    for member in ["base", "left", "right", "top", "app"] {
        assert_eq!(
            (hits(&to_b, member), misses(&to_b, member)),
            (0, 1),
            "{member} compiles on the new branch: {to_b:?}"
        );
    }
    assert_eq!(ws.output(), expected(&b));

    ws.apply(&A);
    let back = ws.events_of(&["build"]);
    for member in ["base", "left", "right", "top", "app"] {
        assert_eq!(
            (hits(&back, member), misses(&back, member)),
            (1, 0),
            "{member} is restored on the way back: {back:?}"
        );
    }
    assert_eq!(ws.output(), expected(&A));

    ws.apply(&b);
    let again = ws.events_of(&["build"]);
    assert_eq!(again.values().map(|e| e.1).sum::<usize>(), 0, "both branches are cached now: {again:?}");
    assert_eq!(ws.output(), expected(&b));

    // `app` instantiates the changed generic in `left`, so it must be recompiled rather than restored from branch B.
    let api = State { left_wrap: "({})", ..b.clone() };
    ws.apply(&api);
    let changed = ws.events_of(&["build"]);
    for member in ["left", "top", "app"] {
        assert_eq!((hits(&changed, member), misses(&changed, member)), (0, 1), "{member} recompiles: {changed:?}");
    }
    assert_eq!(ws.output(), expected(&api));
}

#[test]
fn selection_flags_targets_and_check_build_interplay() {
    let ws = Ws::new("mc-select");
    ws.run(&["build", "--workspace"]);
    assert_eq!(ws.output(), expected(&A));

    // Built as a dependency under `-p app`, the proc-macro gets cargo's build-override profile (no debug info), a different compile.
    ws.run(&["clean"]);
    let selected = ws.events_of(&["build", "-p", "app"]);
    for member in ["base", "left", "right", "top", "gen", "build_script_build"] {
        assert_eq!(
            (hits(&selected, member), misses(&selected, member)),
            (1, 0),
            "{member} restored for -p: {selected:?}"
        );
    }
    for member in ["pm", "app"] {
        assert_eq!(
            (hits(&selected, member), misses(&selected, member)),
            (0, 1),
            "{member} differs for -p: {selected:?}"
        );
    }
    assert_eq!(ws.output(), expected(&A));

    let all = ws.events_of(&["build", "--all-targets"]);
    assert_eq!(misses(&all, "top"), 1, "the unit tests of `top` compile: {all:?}");
    assert_eq!(misses(&all, "it"), 1, "the integration test of `top` compiles: {all:?}");
    ws.run(&["clean"]);
    let all_again = ws.events_of(&["build", "--all-targets"]);
    assert_eq!(all_again.values().map(|e| e.1).sum::<usize>(), 0, "every unit is restored: {all_again:?}");
    assert_success(&ws.fcargo(&["test", "--workspace"]).output().unwrap());

    // check and build are separate units; neither stands in for the other.
    ws.run(&["clean"]);
    let check = ws.events_of(&["check"]);
    assert!(misses(&check, "top") == 1 && hits(&check, "top") == 0, "check units are new: {check:?}");
    let build = ws.events_of(&["build"]);
    assert_eq!(build.values().map(|e| e.1).sum::<usize>(), 0, "build units are restored: {build:?}");
    assert_eq!(ws.output(), expected(&A));
    ws.run(&["clean"]);
    let check_again = ws.events_of(&["check"]);
    assert_eq!(check_again.values().map(|e| e.1).sum::<usize>(), 0, "check units are restored: {check_again:?}");
    assert_eq!(hits(&check_again, "top"), 1);

    let edited = State { base_value: 3, ..A };
    ws.apply(&edited);
    let checked = ws.events_of(&["check"]);
    assert_eq!(misses(&checked, "base"), 1, "{checked:?}");
    ws.run(&["build"]);
    assert_eq!(ws.output(), expected(&edited));
}

#[test]
fn edit_loops_keep_a_bounded_number_of_versions_per_member() {
    let ws = Ws::new("mc-bound");
    ws.run(&["build"]);
    for value in 1..=6 {
        let state = State { base_value: value, ..A };
        ws.apply(&state);
        ws.run(&["build"]);
        assert_eq!(ws.output(), expected(&state));
    }
    let kept = ws.stored_entries("base");
    assert!(kept <= frust::wrapper::UNIT_VERSIONS, "seven versions of `base` built, {kept} kept");
    for member in ["right", "top", "app"] {
        assert!(ws.stored_entries(member) <= frust::wrapper::UNIT_VERSIONS, "{member} is bounded too");
    }
    let previous = State { base_value: 5, ..A };
    ws.apply(&previous);
    let back = ws.events_of(&["build"]);
    assert_eq!((hits(&back, "base"), misses(&back, "base")), (1, 0), "{back:?}");
    assert_eq!(ws.output(), expected(&previous));
}

#[test]
fn primary_incremental_caching_can_be_turned_off() {
    let ws = Ws::new("mc-off");
    let off = |args: &[&str]| {
        let before = ws.sb.cache_events().len();
        assert_success(&ws.fcargo(args).env("FRUST_CACHE_PRIMARY_INCREMENTAL", "off").output().unwrap());
        ws.sb.cache_events()[before..].iter().filter(|e| MEMBERS.contains(&e["crate"].as_str().unwrap_or(""))).count()
    };
    assert_eq!(off(&["build"]), 0, "members are left to rustc");
    assert_success(&ws.fcargo(&["clean"]).output().unwrap());
    assert!(off(&["build", "-p", "app"]) >= 4);
    assert_eq!(ws.output(), expected(&A));
}
