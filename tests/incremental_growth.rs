//! Incremental state stays bounded under a long edit loop, and compaction never costs incrementality or freshness.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{Sandbox, assert_success, run_binary, stderr, stdout};
use frust::compact::{self, AutoOptions};
use frust::config::Config;
use frust::paths::Dirs;

const EDITS: usize = 40;

fn eager() -> AutoOptions {
    let mut opts = AutoOptions::from_env(&Config::from_env());
    opts.enabled = true;
    opts.every = Duration::ZERO;
    opts.min_growth = 0;
    opts.uncached_dead_grace = Duration::ZERO;
    opts.dead_incremental_grace = Duration::ZERO;
    opts.max_target_bytes = u64::MAX;
    opts.pass_budget = Duration::from_secs(3600);
    opts.pass_bytes = u64::MAX;
    opts
}

/// One crate of the chain `base <- mid <- app`; renaming the `sig` function is an interface change.
fn base_src(value: u64, sig: usize) -> String {
    format!(
        "pub fn base() -> u64 {{ {value} }}\npub fn sig_{sig}(x: u64) -> u64 {{ x }}\n{}",
        filler("base")
    )
}

fn mid_src(value: u64, sig: usize) -> String {
    format!(
        "pub fn mid() -> u64 {{ base::base() * 1000 + {value} + if cfg!(feature = \"bump\") {{ 500 }} else {{ 0 }} }}\npub fn sig_{sig}(x: u64) -> u64 {{ x }}\n{}",
        filler("mid")
    )
}

fn app_src(value: u64, sig: usize) -> String {
    format!(
        "fn main() {{ println!(\"{{}}\", mid::mid() * 1000 + {value} + leaf::all(1) % 7); }}\n#[allow(dead_code)]\nfn sig_{sig}(x: u64) -> u64 {{ x }}\n{}",
        filler("app")
    )
}

fn filler(tag: &str) -> String {
    let mut s = String::new();
    for i in 0..60 {
        s.push_str(&format!(
            "pub mod m{i} {{ pub fn {tag}_f{i}(x: u64) -> u64 {{ let mut v = x ^ {i}; for k in 0..{n} {{ v = v.wrapping_mul(6364136223846793005).wrapping_add(k); }} v }} }}\n",
            n = 3 + i % 5
        ));
    }
    s.push_str(&format!("#[allow(dead_code)]\nfn {tag}_use() -> u64 {{ 0"));
    for i in 0..60 {
        s.push_str(&format!(" + m{i}::{tag}_f{i}(1)"));
    }
    s.push_str(" }\n");
    s
}

struct Chain {
    app: PathBuf,
    values: [u64; 3],
    sigs: [usize; 3],
    bump: bool,
}

impl Chain {
    fn new(sb: &Sandbox) -> Chain {
        let mut leaf = String::new();
        for i in 0..40 {
            leaf.push_str(&format!("pub fn f{i}(x: u64) -> u64 {{ x.wrapping_mul({i} + 3) ^ {i} }}\n"));
        }
        leaf.push_str("pub fn all(x: u64) -> u64 { let mut v = x;");
        for i in 0..40 {
            leaf.push_str(&format!(" v = f{i}(v);"));
        }
        leaf.push_str(" v }\n");
        sb.vendor_crate("leaf", "1.0.0", &leaf);
        let app = sb.project("chain", &[("leaf", "1.0.0")], "");
        let manifest = fs::read_to_string(app.join("Cargo.toml")).unwrap();
        fs::write(
            app.join("Cargo.toml"),
            format!("{manifest}mid = {{ path = \"mid\" }}\n\n[features]\nbump = [\"mid/bump\"]\n"),
        )
        .unwrap();
        for (name, deps, features) in
            [("base", "", ""), ("mid", "base = { path = \"../base\" }\n", "\n[features]\nbump = []\n")]
        {
            fs::create_dir_all(app.join(name).join("src")).unwrap();
            fs::write(
                app.join(name).join("Cargo.toml"),
                format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n{deps}{features}"),
            )
            .unwrap();
        }
        let chain = Chain { app, values: [1, 2, 3], sigs: [0, 0, 0], bump: false };
        for i in 0..3 {
            chain.write(i);
        }
        chain
    }

    fn file(&self, krate: usize) -> PathBuf {
        match krate {
            0 => self.app.join("base").join("src").join("lib.rs"),
            1 => self.app.join("mid").join("src").join("lib.rs"),
            _ => self.app.join("src").join("main.rs"),
        }
    }

    fn write(&self, krate: usize) {
        let (v, s) = (self.values[krate], self.sigs[krate]);
        let text = match krate {
            0 => base_src(v, s),
            1 => mid_src(v, s),
            _ => app_src(v, s),
        };
        fs::write(self.file(krate), text).unwrap();
    }

    fn expected(&self) -> u64 {
        let leaf = {
            let mut v = 1u64;
            for i in 0..40u64 {
                v = v.wrapping_mul(i + 3) ^ i;
            }
            v
        };
        let mid = self.values[0] * 1000 + self.values[1] + if self.bump { 500 } else { 0 };
        mid * 1000 + self.values[2] + leaf % 7
    }

    fn args(&self) -> &'static [&'static str] {
        if self.bump { &["--features", "bump"] } else { &[] }
    }
}

fn build(sb: &Sandbox, app: &Path, args: &[&str]) -> Vec<String> {
    let out = sb.bin("fcargo").arg("build").arg("-v").args(args).env("FRUST_AUTO_COMPACT", "off").current_dir(app).output().unwrap();
    assert_success(&out);
    let mut compiled: Vec<String> = stderr(&out)
        .lines()
        .filter_map(|l| l.trim_start().strip_prefix("Compiling "))
        .filter_map(|rest| rest.split_whitespace().next().map(str::to_string))
        .collect();
    compiled.sort();
    compiled
}

#[test]
fn maintenance_retains_the_two_latest_incremental_states_for_restores() {
    let sb = Sandbox::new("inc-retained");
    let project = sb.project("app", &[], "fn main() {}\n");
    let source = project.join("src/main.rs");
    let target = sb.central_target(&project);
    let dirs = Dirs::at(sb.home());
    let opts = eager();
    for value in 0..5 {
        fs::write(&source, format!("fn main() {{ println!(\"{value}\"); }}\n")).unwrap();
        common::settle(&source);
        build(&sb, &project, &[]);
        let report = compact::auto_compact_with(&dirs, &target, &opts);
        assert!(report.errors.is_empty(), "{report:?}");
        assert_eq!(stdout(&run_binary(&target, "debug", "app")).trim(), value.to_string());
        assert!(build(&sb, &project, &[]).is_empty(), "maintenance must leave the current state fresh");
    }
    let entries = fs::read_dir(dirs.store()).unwrap().flatten()
        .flat_map(|shard| fs::read_dir(shard.path()).unwrap().flatten())
        .filter_map(|entry| fs::read(entry.path().join("meta.json")).ok())
        .filter_map(|bytes| serde_json::from_slice::<frust::cache::EntryMeta>(&bytes).ok())
        .filter(|meta| meta.incremental_unit.is_some())
        .count();
    assert_eq!(entries, 2);
    let misses = sb.count_cache("miss", "app");
    for value in [3, 4] {
        fs::write(&source, format!("fn main() {{ println!(\"{value}\"); }}\n")).unwrap();
        common::settle(&source);
        assert_success(&sb.bin("fcargo").arg("clean").current_dir(&project).output().unwrap());
        build(&sb, &project, &[]);
        assert_eq!(stdout(&run_binary(&target, "debug", "app")).trim(), value.to_string());
    }
    assert_eq!(sb.count_cache("miss", "app"), misses, "both retained states must restore without compiling");
    assert!(sb.count_cache("hit", "app") >= 2);
}

#[test]
fn expired_check_only_variants_are_reclaimed_without_cache_and_the_current_check_stays_fresh() {
    fn age_tree(path: &Path) {
        if path.is_dir() {
            for entry in fs::read_dir(path).unwrap() {
                age_tree(&entry.unwrap().path());
            }
        }
        common::age(path, Duration::from_secs(8 * 86400));
    }
    let sb = Sandbox::new("inc-check-idle");
    let app = sb.project("app", &[], "fn main() { println!(\"41\"); }\n");
    let manifest = app.join("Cargo.toml");
    let text = fs::read_to_string(&manifest).unwrap();
    fs::write(manifest, format!("{text}\n[features]\na = []\nb = []\n")).unwrap();
    let check = |feature: Option<&str>| {
        let mut command = sb.bin("fcargo");
        command.args(["check", "-v"]).env("FRUST_CACHE", "off").env("FRUST_AUTO_COMPACT", "off").current_dir(&app);
        if let Some(feature) = feature {
            command.args(["--features", feature]);
        }
        let output = command.output().unwrap();
        assert_success(&output);
        output
    };
    check(Some("a"));
    check(Some("b"));
    let target = sb.central_target(&app);
    let profile = target.join("debug");
    let prior: Vec<_> = fs::read_dir(profile.join("incremental")).unwrap().map(|e| e.unwrap().path()).collect();
    check(None);
    let current: Vec<_> = fs::read_dir(profile.join("incremental")).unwrap().map(|e| e.unwrap().path()).filter(|p| !prior.contains(p)).collect();
    assert!(!prior.is_empty() && !current.is_empty(), "real checks must create distinct incremental variants");
    let analysis = compact::analyze(&profile).unwrap();
    for dir in compact::incremental_dirs(&profile, &analysis) {
        assert_eq!(dir.owner, compact::IncrementalOwner::Unknown, "metadata-only state has no object ownership proof");
    }
    for path in &prior {
        age_tree(path);
    }
    let current_outputs: Vec<_> = fs::read_dir(profile.join("deps")).unwrap()
        .map(|e| e.unwrap().path())
        .filter(|path| path.file_name().and_then(|n| n.to_str()).and_then(compact::artifact_hash).is_some_and(|hash| analysis.live.contains(hash)))
        .map(|path| {
            let bytes = fs::read(&path).unwrap();
            (path, bytes)
        }).collect();
    let mut opts = AutoOptions::from_env(&Config::from_env());
    opts.enabled = true;
    opts.cache_enabled = false;
    opts.compress = false;
    let report = compact::auto_compact_with(&Dirs::at(sb.home()), &target, &opts);
    assert!(report.errors.is_empty(), "{report:?}");
    assert!(prior.iter().all(|p| !p.exists()), "expired check-only reuse states are reclaimed");
    assert!(current.iter().all(|p| p.exists()), "the fresh current incremental state stays");
    for (path, bytes) in current_outputs {
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
    let repeated = check(None);
    assert!(!stderr(&repeated).lines().any(|line| line.trim_start().starts_with("Compiling ") || line.trim_start().starts_with("Checking ")), "maintenance preserves Cargo freshness: {}", stderr(&repeated));
    assert_eq!(sb.count_cache("hit", "app") + sb.count_cache("miss", "app"), 0);
    build(&sb, &app, &[]);
    assert_eq!(stdout(&run_binary(&target, "debug", "app")).trim(), "41");
}

#[test]
fn incremental_state_stays_bounded_and_builds_stay_incremental_under_auto_compaction() {
    let sb = Sandbox::new("inc-growth");
    let mut chain = Chain::new(&sb);
    let dirs = Dirs::at(sb.home());
    let first = build(&sb, &chain.app, chain.args());
    assert!(first.iter().any(|c| c == "base") && first.iter().any(|c| c == "app"), "{first:?}");
    let target = sb.central_target(&chain.app);
    let profile = target.join("debug");
    let opts = eager();
    let report = compact::auto_compact_with(&dirs, &target, &opts);
    assert!(report.skipped.is_none() && report.errors.is_empty(), "{report:?}");

    let mut baseline: Option<u64> = None;
    let mut stale_objects_removed = 0usize;
    for step in 1..=EDITS {
        // Each feature toggle makes the other mid and app variant dead; their incremental directories must not pile up.
        if step == 15 || step == 30 {
            chain.bump = !chain.bump;
        }
        let krate = step % 3;
        if (step / 3) % 2 == 1 {
            chain.sigs[krate] = step;
        } else {
            chain.values[krate] = step as u64 * 10 + krate as u64;
        }
        chain.write(krate);
        let compiled = build(&sb, &chain.app, chain.args());
        let toggled = step == 15 || step == 30;
        let expected: Vec<&str> = match krate {
            0 => vec!["app", "base", "mid"],
            1 => vec!["app", "mid"],
            _ if toggled => vec!["app", "mid"],
            _ => vec!["app"],
        };
        assert_eq!(compiled, expected, "step {step}: only the edited crate and its dependents recompile");
        assert_eq!(stdout(&run_binary(&target, "debug", "app")).trim(), chain.expected().to_string(), "step {step}");

        let report = compact::auto_compact_with(&dirs, &target, &opts);
        assert!(report.skipped.is_none() && report.errors.is_empty(), "step {step}: {report:?}");
        stale_objects_removed += report.stale_objects;
        let again = build(&sb, &chain.app, chain.args());
        assert!(again.is_empty(), "step {step}: compaction made something stale: {again:?}");

        let a = compact::incremental_anatomy(&profile);
        assert_eq!(a.stale_sessions, 0, "step {step}: {a:?}");
        assert_eq!(a.deps_objects_stale, 0, "step {step}: {a:?}");
        assert_eq!(a.dead_dirs, 0, "step {step}: {a:?}");
        assert_eq!(a.sessions, a.crate_dirs, "step {step}: one session per crate directory: {a:?}");
        let names: Vec<String> = fs::read_dir(profile.join("incremental"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().rsplit_once('-').map(|(c, _)| c.to_string()).unwrap_or_default())
            .collect();
        for krate in ["base", "mid", "app"] {
            assert_eq!(names.iter().filter(|n| *n == krate).count(), 1, "step {step}: one incremental directory of {krate}: {names:?}");
        }
        let size = a.total();
        let base = *baseline.get_or_insert(size);
        assert!(size <= base + base / 2 + 512 * 1024, "step {step}: incremental state grew from {base} to {size} bytes: {a:?}");
    }
    if cfg!(target_os = "macos") {
        // On macOS rustc keeps objects in deps/ and every compile adds aliases under new names; compaction must remove them.
        assert!(stale_objects_removed > 0, "superseded object aliases were found and removed");
        // Every edit of base stores new cache entries for base and mid; only the newest few per unit stay.
        for (krate, units) in [("base", 1), ("mid", 2)] {
            let entries = frust::stats::entry_dirs(&dirs)
                .into_iter()
                .filter(|e| {
                    fs::read(e.path.join("meta.json"))
                        .ok()
                        .and_then(|b| serde_json::from_slice::<frust::cache::EntryMeta>(&b).ok())
                        .is_some_and(|m| m.crate_name == krate)
                })
                .count();
            assert!(
                entries <= units * frust::gc::INCREMENTAL_ENTRIES_KEPT,
                "{entries} cache entries of {krate}: superseded incremental entries are pruned"
            );
        }
    }

    chain.values[1] = 999;
    chain.write(1);
    assert_eq!(build(&sb, &chain.app, chain.args()), vec!["app", "mid"]);
    assert_eq!(stdout(&run_binary(&target, "debug", "app")).trim(), chain.expected().to_string());
}
