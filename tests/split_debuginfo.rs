//! Real unpacked split debuginfo must survive a complete cache restore, including unannounced outputs.

#![cfg(target_os = "linux")]

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

use common::{Sandbox, assert_success, run_binary, settle, stderr, stdout};
use frust::cache::EntryMeta;

struct Fixture {
    sandbox: Sandbox,
    project: PathBuf,
    compiler: PathBuf,
}

impl Fixture {
    fn command(&self, operation: &str, phase: &str) -> Output {
        let audit = self.sandbox.root.join(phase);
        fs::create_dir_all(&audit).unwrap();
        let output = self
            .sandbox
            .bin("fcargo")
            .args([operation, "-v"])
            .current_dir(&self.project)
            .env("RUSTC", &self.compiler)
            .env("FRUST_RUSTC", &self.compiler)
            .env("SPLIT_AUDIT", audit)
            .env("RUSTFLAGS", "-Csplit-debuginfo=unpacked -Csave-temps=yes")
            .env("FRUST_AUTO_COMPACT", "off")
            .env("FRUST_TUNE", "off")
            .env("FRUST_LINK", "off")
            .env_remove("CARGO_BUILD_TARGET")
            .output()
            .unwrap();
        assert_success(&output);
        output
    }

    fn calls(&self, phase: &str) -> Vec<Vec<String>> {
        fs::read_dir(self.sandbox.root.join(phase))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "args"))
            .map(|path| {
                fs::read(path)
                    .unwrap()
                    .split(|byte| *byte == 0)
                    .filter(|arg| !arg.is_empty())
                    .map(|arg| String::from_utf8(arg.to_vec()).unwrap())
                    .collect()
            })
            .collect()
    }

    fn compilations(&self, phase: &str) -> usize {
        self.calls(phase)
            .iter()
            .filter(|args| {
                args.iter().any(|arg| arg == "--emit" || arg.starts_with("--emit="))
                    && !args.iter().any(|arg| arg == "--print" || arg.starts_with("--print="))
            })
            .count()
    }

    fn entries(&self) -> BTreeMap<PathBuf, EntryMeta> {
        let mut entries = BTreeMap::new();
        for shard in fs::read_dir(self.sandbox.home().join("store")).unwrap() {
            for entry in fs::read_dir(shard.unwrap().path()).unwrap() {
                let path = entry.unwrap().path();
                let meta: EntryMeta = serde_json::from_slice(&fs::read(path.join("meta.json")).unwrap()).unwrap();
                assert_eq!(meta.crate_name, "app");
                entries.insert(path, meta);
            }
        }
        entries
    }

    fn output(&self, target: &Path) -> String {
        let output = run_binary(target, "debug", "app");
        assert_success(&output);
        stdout(&output)
    }
}

fn hashes(deps: &Path, meta: &EntryMeta) -> BTreeMap<String, blake3::Hash> {
    meta.files
        .iter()
        .map(|file| {
            let bytes = fs::read(deps.join(&file.name)).unwrap();
            assert!(!bytes.is_empty(), "{} was not emitted", file.name);
            (file.name.clone(), blake3::hash(&bytes))
        })
        .collect()
}

#[test]
fn unpacked_split_debuginfo_restores_unannounced_outputs_and_excludes_unchanged_leftovers() {
    let sandbox = Sandbox::new("split-debuginfo");
    let source = |value| {
        format!("#[inline(never)] fn value() -> u64 {{ std::hint::black_box({value}) }}\nfn main() {{ println!(\"{{}}\", value()); }}\n")
    };
    let project = sandbox.project("app", &[], &source(41));
    let manifest = project.join("Cargo.toml");
    fs::write(
        &manifest,
        format!("{}\n[profile.dev]\ndebug = 2\nincremental = false\ncodegen-units = 4\n", fs::read_to_string(&manifest).unwrap()),
    )
    .unwrap();
    // The stable audit shim executes the original compiler and replays its exact diagnostics.
    let compiler = common::write_fake_tool(
        &sandbox.root.join("audited-rustc"),
        "#!/bin/sh\nprintf '%s\\0' \"$@\" > \"$SPLIT_AUDIT/$$.args\" || exit 125\nrustc \"$@\" 2> \"$SPLIT_AUDIT/$$.stderr\"\nstatus=$?\ncat \"$SPLIT_AUDIT/$$.stderr\" >&2\nexit \"$status\"\n",
        "",
    );
    let fixture = Fixture { sandbox, project, compiler };
    fixture.command("build", "cold");
    assert_eq!(fixture.compilations("cold"), 1);
    let target = fixture.sandbox.central_target(&fixture.project);
    let deps = target.join("debug/deps");
    assert_eq!(fixture.output(&target), "41\n");
    assert_eq!(fixture.sandbox.count_cache("miss", "app"), 1);
    let first = fixture.entries();
    assert_eq!(first.len(), 1);
    let previous = first.values().next().unwrap();
    let dwo = previous.files.iter().find(|file| file.name.ends_with(".dwo")).expect("rustc emitted unpacked .dwo files");
    assert!(previous.files.iter().any(|file| file.name.ends_with(".rcgu.o")), "save-temps retained real object files");
    let announced: BTreeSet<_> = fs::read_dir(fixture.sandbox.root.join("cold"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "stderr"))
        .flat_map(|path| {
            fs::read_to_string(path)
                .unwrap()
                .lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok()?.get("artifact")?.as_str().map(str::to_owned))
                .collect::<Vec<_>>()
        })
        .collect();
    assert!(!announced.contains(&deps.join(&dwo.name).to_string_lossy().into_owned()), "the regression needs an unannounced .dwo output");
    let prefix = previous.files.iter().find(|file| file.name.ends_with(".d")).unwrap().name.trim_end_matches(".d");
    let leftover = format!("{prefix}.obsolete.rcgu.dwo");
    fs::copy(deps.join(&dwo.name), deps.join(&leftover)).unwrap();
    let unrelated = deps.join("unrelated-sibling.bin");
    fs::write(&unrelated, b"unrelated bytes").unwrap();

    fs::write(fixture.project.join("src/main.rs"), source(42)).unwrap();
    settle(&fixture.project.join("src/main.rs"));
    fixture.command("build", "edited");
    assert_eq!(fixture.compilations("edited"), 1);
    assert_eq!(fixture.output(&target), "42\n");
    assert_eq!(fixture.sandbox.count_cache("miss", "app"), 2);
    assert_eq!(fs::read(&unrelated).unwrap(), b"unrelated bytes");
    let entries = fixture.entries();
    let current: Vec<_> = entries.iter().filter(|(path, _)| !first.contains_key(*path)).collect();
    assert_eq!(current.len(), 1, "the body edit published a distinct cache entry");
    let (_, meta) = current[0];
    assert!(!meta.files.iter().any(|file| file.name == leftover), "unchanged prefixed leftovers are excluded");
    assert!(meta.files.iter().any(|file| file.name.ends_with(".dwo")));
    assert!(meta.files.iter().any(|file| file.name.ends_with(".rcgu.o")));
    let expected = hashes(&deps, meta);
    let executable = blake3::hash(&fs::read(target.join("debug/app")).unwrap());

    fixture.command("clean", "clean");
    fs::create_dir_all(&deps).unwrap();
    fs::write(&unrelated, b"unrelated bytes after clean").unwrap();
    fs::write(deps.join(&leftover), b"a retained sibling must not be overwritten").unwrap();
    fixture.command("build", "restored");
    assert_eq!(fixture.sandbox.count_cache("hit", "app"), 1);
    assert_eq!(fixture.sandbox.count_cache("miss", "app"), 2);
    assert_eq!(fixture.compilations("restored"), 0, "restoration executes no crate compilation");
    assert_eq!(hashes(&deps, meta), expected, "every current output is byte-identical after restoration");
    assert_eq!(blake3::hash(&fs::read(target.join("debug/app")).unwrap()), executable);
    assert_eq!(fixture.output(&target), "42\n");
    assert_eq!(fs::read(&unrelated).unwrap(), b"unrelated bytes after clean");
    assert_eq!(fs::read(deps.join(&leftover)).unwrap(), b"a retained sibling must not be overwritten");

    let events = fixture.sandbox.cache_events();
    let unchanged = fixture.command("build", "unchanged");
    assert!(fixture.calls("unchanged").is_empty(), "the final no-op executes no compiler calls, including queries");
    assert!(!stderr(&unchanged).lines().any(|line| line.trim_start().starts_with("Compiling ")));
    assert_eq!(fixture.sandbox.cache_events(), events, "the no-op performs no cache restore or store");
    assert_eq!(hashes(&deps, meta), expected);
    assert_eq!(fixture.output(&target), "42\n");
}
