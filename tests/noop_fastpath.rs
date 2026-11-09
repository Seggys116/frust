//! The no-op fast path answers an unchanged repeat build without cargo and declines on every kind of change; audit mode is the oracle.

mod common;

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, SystemTime};

use common::*;

const ROOT_MANIFEST: &str = "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\nmembers = [\"dep\"]\n\n[features]\nextra = []\n\n[dependencies]\ndep = { path = \"dep\" }\n";

const MAIN: &str = "include!(concat!(env!(\"OUT_DIR\"), \"/gen.rs\"));\n\nfn main() {\n    println!(\"{}|{}|{}|{}\", DATA, BVAR, env!(\"NOOP_EVAR\"), dep::value());\n}\n";

const BUILD: &str = r#"fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=data.txt");
    println!("cargo:rerun-if-env-changed=NOOP_BVAR");
    let data = std::fs::read_to_string("data.txt").unwrap();
    let bvar = std::env::var("NOOP_BVAR").unwrap_or_default();
    let out = std::env::var("OUT_DIR").unwrap();
    std::fs::write(format!("{out}/gen.rs"), format!("pub const DATA: &str = {:?};\npub const BVAR: &str = {:?};\n", data.trim(), bvar)).unwrap();
}
"#;

const DEP_MANIFEST: &str = "[package]\nname = \"dep\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";

fn app_files() -> Vec<(&'static str, &'static str)> {
    vec![
        ("Cargo.toml", ROOT_MANIFEST),
        ("build.rs", BUILD),
        ("data.txt", "one\n"),
        ("src/main.rs", MAIN),
        ("dep/Cargo.toml", DEP_MANIFEST),
        ("dep/src/lib.rs", "pub fn value() -> u32 {\n    1\n}\n"),
    ]
}

struct Fixture {
    sb: Sandbox,
    dir: PathBuf,
    trace: PathBuf,
    env: Vec<(String, String)>,
    unset: Vec<String>,
}

impl Fixture {
    fn new(tag: &str, files: &[(&str, &str)]) -> Fixture {
        Fixture::with_vendor(tag, files, &[])
    }

    /// Every input must predate the build that records it by the "file too new" margin.
    fn settle_all(&self) {
        settle(&self.sb.cargo_home().join("config.toml"));
        let mut stack = vec![self.dir.clone()];
        while let Some(dir) = stack.pop() {
            settle(&dir);
            for entry in fs::read_dir(&dir).unwrap().flatten() {
                if entry.file_type().unwrap().is_dir() {
                    stack.push(entry.path());
                } else {
                    settle(&entry.path());
                }
            }
        }
    }

    fn command(&self, args: &[&str], mode: &str) -> Command {
        let mut cmd = self.sb.bin("fcargo");
        cmd.args(args).current_dir(&self.dir).env("FRUST_NOOP", mode).env("FRUST_NOOP_TRACE", &self.trace);
        for (key, value) in &self.env {
            cmd.env(key, value);
        }
        for key in &self.unset {
            cmd.env_remove(key);
        }
        cmd
    }

    fn last_trace(&self) -> serde_json::Value {
        let text = fs::read_to_string(&self.trace).unwrap_or_default();
        text.lines().last().map(|l| serde_json::from_str(l).unwrap()).unwrap_or(serde_json::Value::Null)
    }

    fn run(&self, args: &[&str], mode: &str) -> (Output, serde_json::Value) {
        let before = fs::read_to_string(&self.trace).map(|t| t.lines().count()).unwrap_or(0);
        let out = self.command(args, mode).output().unwrap();
        let after = fs::read_to_string(&self.trace).map(|t| t.lines().count()).unwrap_or(0);
        assert_eq!(after, before + 1, "one trace line per build: {}", stderr(&out));
        (out, self.last_trace())
    }

    /// Served in-process: success, no stdout, and exactly cargo's `Finished` line.
    fn fast(&self, args: &[&str]) -> Output {
        let (out, trace) = self.run(args, "on");
        assert_success(&out);
        assert_eq!(trace["noop"], "fast", "trace {trace}; stderr:\n{}", stderr(&out));
        assert!(out.stdout.is_empty(), "stdout: {}", stdout(&out));
        out
    }

    /// The fast path must decline; cargo runs under audit, which would fail the build had the fast path said "unchanged".
    fn declines(&self, args: &[&str]) -> serde_json::Value {
        let (out, trace) = self.run(args, "audit");
        assert_success(&out);
        let noop = trace["noop"].as_str().unwrap_or("");
        assert!(noop.starts_with("miss:"), "expected a decline, trace {trace}; stderr:\n{}", stderr(&out));
        trace
    }

    /// The fast path would have answered and cargo agrees: nothing rebuilt and the same output.
    fn audited(&self, args: &[&str]) {
        let (out, trace) = self.run(args, "audit");
        assert_success(&out);
        assert_eq!(trace["noop"], "audit:ok", "trace {trace}; stderr:\n{}", stderr(&out));
    }

    /// A decline followed by a served build: cargo's run re-recorded the command.
    fn declines_then_fast(&self, args: &[&str]) -> serde_json::Value {
        let trace = self.declines(args);
        let recorded = trace["record"].as_str().unwrap_or("");
        assert!(recorded == "build" || recorded == "noop", "the declined build should record, trace {trace}");
        self.fast(args);
        trace
    }

    fn target(&self) -> PathBuf {
        self.sb.central_target(&self.dir)
    }

    fn app_output(&self, profile: &str) -> String {
        let out = run_binary(&self.target(), profile, "app");
        assert!(out.status.success());
        stdout(&out).trim().to_string()
    }

    fn edit(&self, path: &str, text: &str) {
        let full = self.dir.join(path);
        write(&full, text);
        settle(&full);
        if let Some(parent) = full.parent() {
            settle(parent);
        }
    }
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn set_mtime(path: &Path, when: SystemTime) {
    File::options().write(true).open(path).unwrap().set_modified(when).unwrap();
}

fn assert_finished_only(out: &Output) {
    let text = stderr(out);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 1, "stderr: {text}");
    assert!(lines[0].starts_with("    Finished `dev` profile [unoptimized + debuginfo] target(s) in "), "stderr: {text}");
    assert!(lines[0].ends_with('s'));
}

#[test]
fn a_repeat_build_is_served_and_matches_cargo_until_a_source_changes() {
    let fx = Fixture::new("noop-sources", &app_files());
    let (out, trace) = fx.run(&["build"], "on");
    assert_success(&out);
    assert_eq!(trace["noop"], "miss:none");
    assert_eq!(trace["record"], "build", "a warning-free build records directly: {trace}");
    assert_eq!(fx.app_output("debug"), "one||e1|1");
    let served = fx.fast(&["build"]);
    assert_finished_only(&served);
    fx.audited(&["build"]);
    fx.fast(&["build"]);

    fx.edit("src/main.rs", &format!("{MAIN}// edited\n"));
    fx.declines_then_fast(&["build"]);

    let main = fx.dir.join("src/main.rs");
    set_mtime(&main, SystemTime::now());
    settle(&main);
    fx.declines_then_fast(&["build"]);

    fx.edit("src/bin/x.rs", "fn main() {}\n");
    fx.declines_then_fast(&["build"]);
    assert!(fx.target().join("debug").join(exe("x")).is_file());

    fx.edit("src/extra.rs", "pub fn unused() {}\n");
    let trace = fx.declines_then_fast(&["build"]);
    assert_eq!(trace["record"], "noop", "an unreferenced file changes nothing for cargo: {trace}");

    fx.edit("dep/src/lib.rs", "pub fn value() -> u32 {\n    2\n}\n");
    fx.declines_then_fast(&["build"]);
    assert_eq!(fx.app_output("debug"), "one||e1|2");

    fx.edit("data.txt", "two\n");
    fx.declines_then_fast(&["build"]);
    assert_eq!(fx.app_output("debug"), "two||e1|2");

    fs::remove_file(fx.target().join("debug").join(exe("app"))).unwrap();
    fx.declines_then_fast(&["build"]);
    assert_eq!(fx.app_output("debug"), "two||e1|2");

    // Cargo rebuilds a crate whose source is newer than its dep-info on every run, so nothing may be recorded meanwhile.
    set_mtime(&main, SystemTime::now() + Duration::from_secs(3_600));
    let trace = fx.declines(&["build"]);
    assert!(trace["record"].as_str().unwrap().starts_with("skip:"), "{trace}");
    fx.declines(&["build"]);
    set_mtime(&main, SystemTime::now() - Duration::from_secs(60));
    settle(&main);
    fx.declines_then_fast(&["build"]);
    fx.audited(&["build"]);
}

#[test]
fn environment_arguments_manifests_config_and_toolchain_changes_decline() {
    let mut fx = Fixture::new("noop-inputs", &app_files());
    let (out, _) = fx.run(&["build"], "on");
    assert_success(&out);
    fx.fast(&["build"]);

    fx.env = vec![("NOOP_EVAR".into(), "e2".into())];
    fx.declines_then_fast(&["build"]);
    assert_eq!(fx.app_output("debug"), "one||e2|1");

    fx.env.push(("NOOP_BVAR".into(), "b1".into()));
    fx.declines_then_fast(&["build"]);
    assert_eq!(fx.app_output("debug"), "one|b1|e2|1");

    fx.declines_then_fast(&["build", "--features", "extra"]);
    fx.declines_then_fast(&["build", "-p", "dep"]);
    fx.declines_then_fast(&["build", "--release"]);
    assert_eq!(fx.app_output("release"), "one|b1|e2|1");
    fx.declines_then_fast(&["check"]);
    // The `--features extra` build replaced the uplifted binary, which cargo would copy back.
    fx.declines_then_fast(&["build"]);
    assert_eq!(fx.app_output("debug"), "one|b1|e2|1");

    fx.edit("Cargo.toml", &ROOT_MANIFEST.replace("extra = []", "extra = []\nother = []"));
    fx.declines_then_fast(&["build"]);

    fx.edit(".cargo/config.toml", "[build]\njobs = 2\n");
    fx.declines_then_fast(&["build"]);

    // Cargo may normalise the lockfile it was given, so the build after the decline may need one more cargo run to re-record.
    let lock = fx.dir.join("Cargo.lock");
    let text = fs::read_to_string(&lock).unwrap();
    fx.edit("Cargo.lock", &format!("{text}\n"));
    fx.declines(&["build"]);
    settle(&lock);
    let (out, trace) = fx.run(&["build"], "audit");
    assert_success(&out);
    assert!(trace["noop"] == "audit:ok" || trace["noop"].as_str().unwrap().starts_with("miss:"), "{trace}");
    fx.fast(&["build"]);

    fx.env.push(("RUSTC".into(), "rustc".into()));
    fx.declines_then_fast(&["build"]);
    // Under rustup the tests inherit `RUSTUP_TOOLCHAIN`; dropping it selects the default toolchain, setting it pins the active one.
    if std::env::var_os("RUSTUP_TOOLCHAIN").is_some() {
        fx.unset.push("RUSTUP_TOOLCHAIN".into());
        fx.declines_then_fast(&["build"]);
    } else if let Some(name) = Command::new("rustup").args(["show", "active-toolchain"]).output().ok()
        .filter(|o| o.status.success())
        .and_then(|o| stdout(&o).split_whitespace().next().map(str::to_string))
    {
        fx.env.push(("RUSTUP_TOOLCHAIN".into(), name));
        fx.declines_then_fast(&["build"]);
    }
    assert_eq!(fx.app_output("debug"), "one|b1|e2|1");
}

#[test]
fn display_and_plain_builds_share_one_record() {
    let mut fx = Fixture::new("noop-display", &app_files());
    fx.env.push(("FRUST_RENDER".into(), "plain".into()));
    let (out, trace) = fx.run(&["build"], "on");
    assert_success(&out);
    assert_eq!(trace["record"], "build", "{trace}");
    let served = fx.fast(&["build"]);
    assert!(!stderr(&served).trim().is_empty(), "the display draws the replayed build");
    fx.audited(&["build"]);
    fx.env.pop();
    assert_finished_only(&fx.fast(&["build"]));
    fx.edit("dep/src/lib.rs", "pub fn value() -> u32 {\n    3\n}\n");
    fx.declines_then_fast(&["build"]);
    fx.env.push(("FRUST_RENDER".into(), "plain".into()));
    fx.fast(&["build"]);
    assert_eq!(fx.app_output("debug"), "one||e1|3");
}

#[test]
fn a_build_in_flight_makes_the_fast_path_decline() {
    let fx = Fixture::new("noop-busy", &app_files());
    let (out, _) = fx.run(&["build"], "on");
    assert_success(&out);
    fx.fast(&["build"]);
    let target = fx.target();
    let lock_dir = frust::lock::dir_for(&frust::paths::Dirs::at(fx.sb.home()), &target);
    let held_by_cargo = target.join("debug").join(".cargo-lock");
    let held_by_frust = lock_dir.join("lock");
    for path in [held_by_cargo, held_by_frust] {
        let file = File::options().read(true).write(true).open(&path).unwrap();
        frust::sys::lock(&file, frust::sys::LockKind::Exclusive).unwrap();
        let mut child = fx.command(&["build"], "on").stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
        // Released only once the child is past its check: it holds frust's build lock (waiting on cargo's) or queues for it.
        let pid = child.id();
        let past_check = || {
            let owner = fs::read_to_string(lock_dir.join("slots").join("debug@host").join("owner.json")).unwrap_or_default();
            let queued = fs::read_dir(lock_dir.join("queue")).map(|e| e.flatten().any(|e| e.file_name().to_string_lossy().ends_with(&format!("-{pid}")))).unwrap_or(false);
            owner.contains(&format!("\"pid\":{pid},")) || queued
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while !past_check() {
            assert!(child.try_wait().unwrap().is_none(), "the build finished without waiting for {}", path.display());
            assert!(std::time::Instant::now() < deadline, "the build never reached the build lock");
            std::thread::sleep(Duration::from_millis(10));
        }
        // Nothing may write the target while its lock is held elsewhere, so the build is still waiting.
        std::thread::sleep(Duration::from_millis(500));
        assert!(child.try_wait().unwrap().is_none(), "the build finished while {} was held", path.display());
        frust::sys::unlock(&file).unwrap();
        let out = child.wait_with_output().unwrap();
        assert_success(&out);
        assert_eq!(fx.last_trace()["noop"], "miss:busy", "{}: {}", path.display(), stderr(&out));
        // A run that printed cargo's lock-contention notice is not a replay; the next cargo run records again.
        if fx.last_trace()["record"] != "noop" {
            let (out, _) = fx.run(&["build"], "audit");
            assert_success(&out);
        }
        fx.fast(&["build"]);
    }
}

#[test]
fn a_build_script_without_rerun_directives_watches_its_whole_package() {
    let files = [
        ("Cargo.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n"),
        ("build.rs", "fn main() {}\n"),
        ("src/main.rs", "fn main() {\n    println!(\"{}\", env!(\"NOOP_EVAR\"));\n}\n"),
        ("notes/readme.txt", "first\n"),
    ];
    let fx = Fixture::new("noop-norerun", &files);
    let (out, _) = fx.run(&["build"], "on");
    assert_success(&out);
    fx.fast(&["build"]);
    fx.edit("notes/readme.txt", "second\n");
    fx.declines_then_fast(&["build"]);
    fx.edit("notes/new.txt", "new\n");
    fx.declines_then_fast(&["build"]);
    fx.audited(&["build"]);
    assert_eq!(fx.app_output("debug"), "e1");
}

#[test]
fn proc_macro_crates_are_tracked_like_any_path_unit() {
    let files = [
        ("Cargo.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\nmembers = [\"mac\"]\n\n[dependencies]\nmac = { path = \"mac\" }\n"),
        ("src/main.rs", "fn main() {\n    println!(\"{}\", mac::answer!());\n}\n"),
        ("mac/Cargo.toml", "[package]\nname = \"mac\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\nproc-macro = true\n"),
        ("mac/src/lib.rs", "use proc_macro::TokenStream;\n\n#[proc_macro]\npub fn answer(_: TokenStream) -> TokenStream {\n    \"41\".parse().unwrap()\n}\n"),
    ];
    let fx = Fixture::new("noop-procmacro", &files);
    let (out, _) = fx.run(&["build"], "on");
    assert_success(&out);
    fx.fast(&["build"]);
    assert_eq!(fx.app_output("debug"), "41");
    fx.edit("mac/src/lib.rs", "use proc_macro::TokenStream;\n\n#[proc_macro]\npub fn answer(_: TokenStream) -> TokenStream {\n    \"42\".parse().unwrap()\n}\n");
    fx.declines_then_fast(&["build"]);
    fx.audited(&["build"]);
    assert_eq!(fx.app_output("debug"), "42");
}

#[test]
fn a_workspace_with_a_vendored_registry_dependency_is_served() {
    let files = [
        ("Cargo.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n\n[dependencies]\nvdep = \"=1.0.0\"\n"),
        ("src/main.rs", "fn main() {\n    println!(\"{}\", vdep::v());\n}\n"),
    ];
    let fx = Fixture::with_vendor("noop-vendored", &files, &[("vdep", "1.0.0", "pub fn v() -> u32 {\n    7\n}\n")]);
    let (out, _) = fx.run(&["build"], "on");
    assert_success(&out);
    fx.fast(&["build"]);
    fx.audited(&["build"]);
    assert_eq!(fx.app_output("debug"), "7");
    let (out, trace) = fx.run(&["check"], "on");
    assert_success(&out);
    assert_eq!(trace["noop"], "miss:none", "check keeps its own record");
    fx.fast(&["check"]);
    fx.fast(&["build"]);
}

// Cargo names a path package's cdylib, and its executables on windows-msvc, without a hash: `deps/libcdy.dylib` beside `deps/cdy.d`.
#[test]
fn units_cargo_names_without_a_hash_are_tracked() {
    let files = [
        ("Cargo.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\nmembers = [\"cdy\"]\n"),
        ("src/main.rs", "fn main() {\n    println!(\"1\");\n}\n"),
        ("cdy/Cargo.toml", "[package]\nname = \"cdy\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\ncrate-type = [\"cdylib\"]\n"),
        ("cdy/src/lib.rs", "#[no_mangle]\npub extern \"C\" fn cdy_value() -> u32 {\n    1\n}\n"),
    ];
    let fx = Fixture::new("noop-unhashed", &files);
    let (out, trace) = fx.run(&["build", "--workspace"], "on");
    assert_success(&out);
    assert_eq!(trace["record"], "build", "{trace}");
    fx.fast(&["build", "--workspace"]);
    fx.audited(&["build", "--workspace"]);
    fx.edit("cdy/src/lib.rs", "#[no_mangle]\npub extern \"C\" fn cdy_value() -> u32 {\n    2\n}\n");
    fx.declines_then_fast(&["build", "--workspace"]);
    let name = format!("{}cdy{}", std::env::consts::DLL_PREFIX, std::env::consts::DLL_SUFFIX);
    fs::remove_file(fx.target().join("debug").join("deps").join(&name)).unwrap();
    fx.declines_then_fast(&["build", "--workspace"]);
    fx.audited(&["build", "--workspace"]);
}

// Cargo lists the files behind a symlinked directory of a package whose build script has no rerun directives.
#[cfg(unix)]
#[test]
fn a_symlinked_directory_in_a_watched_package_is_followed() {
    let files = [
        ("Cargo.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n"),
        ("build.rs", "fn main() {}\n"),
        ("src/main.rs", "fn main() {\n    println!(\"{}\", env!(\"NOOP_EVAR\"));\n}\n"),
        ("pkg/brew/formula.rb", "one\n"),
        ("pkg/other/formula.rb", "other\n"),
    ];
    let fx = Fixture::new("noop-symlinked", &files);
    std::os::unix::fs::symlink("pkg/brew", fx.dir.join("Formula")).unwrap();
    std::os::unix::fs::symlink("Formula", fx.dir.join("Again")).unwrap();
    fx.settle_all();
    let (out, trace) = fx.run(&["build"], "on");
    assert_success(&out);
    assert_eq!(trace["record"], "build", "{trace}");
    fx.fast(&["build"]);
    fx.edit("pkg/brew/formula.rb", "two\n");
    fx.declines_then_fast(&["build"]);
    fs::remove_file(fx.dir.join("Formula")).unwrap();
    std::os::unix::fs::symlink("pkg/other", fx.dir.join("Formula")).unwrap();
    fx.settle_all();
    fx.declines_then_fast(&["build"]);
    fx.audited(&["build"]);
    assert_eq!(fx.app_output("debug"), "e1");
}

impl Fixture {
    fn with_vendor(tag: &str, files: &[(&str, &str)], crates: &[(&str, &str, &str)]) -> Fixture {
        let sb = Sandbox::new(tag);
        for (name, version, source) in crates {
            sb.vendor_crate(name, version, source);
        }
        let dir = sb.root.join("ws");
        for (path, text) in files {
            write(&dir.join(path), text);
        }
        let trace = sb.root.join("noop-trace.jsonl");
        let fx = Fixture { sb, dir, trace, env: vec![("NOOP_EVAR".into(), "e1".into())], unset: Vec::new() };
        let out = fx.sb.cargo().args(["generate-lockfile", "--offline"]).current_dir(&fx.dir).output().unwrap();
        assert_success(&out);
        fx.settle_all();
        fx
    }
}

/// The hashed build script executable, which compaction compresses at once on macOS.
fn build_script_exe(target: &Path) -> PathBuf {
    let build = target.join("debug").join("build");
    for unit in fs::read_dir(&build).unwrap().flatten() {
        for file in fs::read_dir(unit.path()).unwrap().flatten() {
            let name = file.file_name().to_string_lossy().into_owned();
            if name.starts_with("build_script_build-") && !name.ends_with(".d") && file.path().is_file() {
                return file.path();
            }
        }
    }
    panic!("no build script executable under {}", build.display());
}

#[test]
fn a_compaction_pass_that_rewrites_outputs_keeps_the_record_served() {
    let mut fx = Fixture::new("noop-compact", &app_files());
    fx.env.push(("FRUST_AUTO_COMPACT".into(), "off".into()));
    let (out, trace) = fx.run(&["build"], "on");
    assert_success(&out);
    assert_eq!(trace["record"], "build", "{trace}");
    fx.fast(&["build"]);
    let target = fx.target();
    let script = build_script_exe(&target);
    let before = frust::sys::stat(&script).unwrap().ino();
    let _ = fs::remove_file(target.join(frust::compact::AUTO_MARKER));
    let pass = fx.sb.bin("frust").args(["compact", frust::compact::AUTO_CHILD_FLAG]).arg(&target).output().unwrap();
    assert_success(&pass);
    if cfg!(target_os = "macos") {
        assert_ne!(frust::sys::stat(&script).unwrap().ino(), before, "the pass rewrote a recorded output");
    }
    fx.fast(&["build"]);
    fx.audited(&["build"]);

    fx.edit("src/main.rs", &format!("{MAIN}// edited after compaction\n"));
    fx.declines_then_fast(&["build"]);
    assert_eq!(fx.app_output("debug"), "one||e1|1");
}
