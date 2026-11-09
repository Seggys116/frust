//! The executor replays recorded rustc calls when only workspace sources changed; audit mode and plain cargo are the oracles.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use common::*;

const WORKSPACE: &str = "[workspace]\nmembers = [\"applib\", \"app\"]\nresolver = \"2\"\n";
const APPLIB: &str = "[package]\nname = \"applib\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";
const APP: &str = "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\napplib = { path = \"../applib\" }\n";
const MAIN: &str = "fn main() {\n    println!(\"{}\", applib::value());\n}\n";

fn lib_and_bin() -> Vec<(&'static str, String)> {
    vec![
        ("Cargo.toml", WORKSPACE.to_string()),
        ("applib/Cargo.toml", APPLIB.to_string()),
        ("applib/src/lib.rs", "pub fn value() -> u32 {\n    1\n}\n".to_string()),
        ("app/Cargo.toml", APP.to_string()),
        ("app/src/main.rs", MAIN.to_string()),
    ]
}

struct Fixture {
    sb: Sandbox,
    dir: PathBuf,
    trace: PathBuf,
    env: Vec<(String, String)>,
}

impl Fixture {
    fn new(tag: &str, files: &[(&str, String)]) -> Fixture {
        let sb = Sandbox::new(tag);
        let dir = sb.root.join("ws");
        for (path, text) in files {
            write(&dir.join(path), text);
        }
        let trace = sb.root.join("trace.jsonl");
        let fx = Fixture { sb, dir, trace, env: Vec::new() };
        let out = fx.sb.cargo().args(["generate-lockfile", "--offline"]).current_dir(&fx.dir).output().unwrap();
        assert_success(&out);
        fx.settle_all();
        fx
    }

    /// Inputs written just before a build would be recorded as changed during it.
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

    fn command(&self, mode: &str) -> Command {
        let mut cmd = self.sb.bin("fcargo");
        cmd.arg("build").current_dir(&self.dir).env("FRUST_EXEC", mode).env("FRUST_NOOP_TRACE", &self.trace);
        for (key, value) in &self.env {
            cmd.env(key, value);
        }
        cmd
    }

    fn traces(&self) -> Vec<serde_json::Value> {
        let text = fs::read_to_string(&self.trace).unwrap_or_default();
        text.lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    fn run(&self, mode: &str) -> (Output, serde_json::Value) {
        let before = self.traces().len();
        let out = self.command(mode).output().unwrap();
        let traces = self.traces();
        assert_eq!(traces.len(), before + 1, "one trace line per build: {}", stderr(&out));
        (out, traces.last().cloned().unwrap())
    }

    fn exec(trace: &serde_json::Value) -> &str {
        trace["exec"].as_str().unwrap_or("")
    }

    /// Served by the executor with `dirty` of the plan's units rebuilt.
    fn replayed(&self, dirty: usize) -> Output {
        let (out, trace) = self.run("on");
        assert_success(&out);
        let exec = Fixture::exec(&trace);
        assert!(exec.starts_with(&format!("replay:{dirty}/")), "expected {dirty} replayed units, trace {trace}; stderr:\n{}", stderr(&out));
        out
    }

    fn declined(&self, why: &str) -> Output {
        let (out, trace) = self.run("on");
        assert_success(&out);
        assert_eq!(Fixture::exec(&trace), format!("decline:{why}"), "trace {trace}; stderr:\n{}", stderr(&out));
        assert_eq!(trace["plan"], "written", "the cargo run records a new plan: {trace}");
        out
    }

    /// Cargo runs, the executor's prediction is checked against it, and a cache-free replay must produce the same bytes.
    fn audited(&self) -> Output {
        let (out, trace) = self.run("audit");
        assert_success(&out);
        assert_eq!(Fixture::exec(&trace), "audit:ok", "trace {trace}; stderr:\n{}", stderr(&out));
        out
    }

    fn edit(&self, path: &str, text: &str) {
        let full = self.dir.join(path);
        write(&full, text);
        settle(&full);
        settle(full.parent().unwrap());
    }

    fn target(&self) -> PathBuf {
        self.sb.central_target(&self.dir)
    }

    fn output(&self, name: &str) -> String {
        let out = run_binary(&self.target(), "debug", name);
        assert!(out.status.success());
        stdout(&out).trim().to_string()
    }

    /// Plain cargo in its own target, with neither frust nor its wrapper.
    fn plain(&self, target: &Path) -> Output {
        let mut cmd = self.sb.cargo();
        cmd.arg("build").current_dir(&self.dir).env("CARGO_TARGET_DIR", target);
        for (key, value) in &self.env {
            cmd.env(key, value);
        }
        cmd.output().unwrap()
    }
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

/// What cargo itself compiles now: `--message-format=json` keeps both fast paths out of the way.
fn cargo_rebuilds(fx: &Fixture) -> Vec<String> {
    let mut cmd = fx.command("on");
    cmd.arg("--message-format=json");
    let out = cmd.output().unwrap();
    assert_success(&out);
    stdout(&out)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|msg| msg["reason"] == "compiler-artifact" && msg["fresh"] != true)
        .map(|msg| msg["target"]["name"].as_str().unwrap_or("?").to_string())
        .collect()
}

/// Everything but the status lines cargo and the executor time differently.
fn diagnostics(out: &Output) -> Vec<String> {
    stderr(out)
        .lines()
        .filter(|l| !l.trim_start().starts_with("Compiling ") && !l.trim_start().starts_with("Finished "))
        .map(str::to_string)
        .collect()
}

#[test]
fn an_edited_library_is_replayed_without_cargo_and_the_binary_sees_it() {
    let fx = Fixture::new("exec-lib", &lib_and_bin());
    let (out, trace) = fx.run("on");
    assert_success(&out);
    assert_eq!(Fixture::exec(&trace), "decline:none");
    assert_eq!(trace["plan"], "written", "trace {trace}");
    assert_eq!(fx.output("app"), "1");

    fx.edit("applib/src/lib.rs", "pub fn value() -> u32 {\n    2\n}\n");
    let out = fx.replayed(2);
    let text = stderr(&out);
    assert!(text.contains("   Compiling applib v0.1.0 (") && text.contains("   Compiling app v0.1.0 ("), "stderr: {text}");
    assert!(text.lines().last().unwrap().starts_with("    Finished `dev` profile [unoptimized + debuginfo] target(s) in "), "stderr: {text}");
    assert_eq!(fx.output("app"), "2");

    let (out, trace) = fx.run("on");
    assert_success(&out);
    assert_eq!(trace["noop"], "fast", "the replay recorded the no-op answer: {trace}");

    fx.edit("app/src/main.rs", &format!("{MAIN}// edited\n"));
    fx.replayed(1);
    assert_eq!(fx.output("app"), "2");

    fx.edit("applib/src/lib.rs", "pub fn value() -> u32 {\n    3\n}\n");
    fx.audited();
    assert_eq!(fx.output("app"), "3");

    fx.edit("app/Cargo.toml", &format!("{APP}# edited\n"));
    fx.declined("manifest or config changed");

    fx.edit("applib/src/lib.rs", "pub fn value() -> u32 {\n    4\n}\n");
    fx.replayed(2);
    assert_eq!(fx.output("app"), "4");
    assert_eq!(cargo_rebuilds(&fx), Vec::<String>::new(), "cargo itself finds the replayed units fresh");

    fx.edit("app/src/bin/extra.rs", "fn main() {}\n");
    fx.declined("targets changed");
    assert!(fx.target().join("debug").join(exe("extra")).is_file());
}

#[test]
fn replayed_outputs_are_byte_identical_to_plain_cargo() {
    let mut fx = Fixture::new("exec-bytes", &lib_and_bin());
    fx.env.push(("CARGO_INCREMENTAL".into(), "0".into()));
    assert_success(&fx.run("on").0);
    fx.edit("applib/src/lib.rs", "pub fn value() -> u32 {\n    5\n}\n");
    fx.replayed(2);
    let deps = fx.target().join("debug").join("deps");
    // An msvc link stamps the time and a fresh PDB signature into the executable, so no two links there are byte identical.
    let linked_reproducibly = !cfg!(windows);
    let compared = |name: &fs::DirEntry| {
        let n = name.file_name().to_string_lossy().into_owned();
        (n.starts_with("libapplib-") && (n.ends_with(".rlib") || n.ends_with(".rmeta"))) || (linked_reproducibly && n.starts_with("app-") && !n.contains('.'))
    };
    let snapshot = |dir: &Path| -> Vec<(String, Vec<u8>)> {
        let mut files: Vec<(String, Vec<u8>)> = fs::read_dir(dir).unwrap().flatten().filter(compared)
            .map(|e| (e.file_name().to_string_lossy().into_owned(), fs::read(e.path()).unwrap())).collect();
        files.sort();
        files
    };
    let replayed = snapshot(&deps);
    let expected = if linked_reproducibly { 3 } else { 2 };
    assert_eq!(replayed.len(), expected, "rlib, rmeta and executable: {:?}", replayed.iter().map(|f| &f.0).collect::<Vec<_>>());
    // The replay left cargo nothing to do, so cargo's record of the two units is removed to make it compile them.
    for entry in fs::read_dir(fx.target().join("debug").join(".fingerprint")).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("applib-") || name.starts_with("app-") {
            fs::remove_dir_all(entry.path()).unwrap();
        }
    }
    let out = fx.plain(&fx.target());
    assert_success(&out);
    assert!(stderr(&out).contains("Compiling applib") && stderr(&out).contains("Compiling app "), "cargo recompiles both units: {}", stderr(&out));
    let plain = snapshot(&deps);
    for ((name, mine), (_, theirs)) in replayed.iter().zip(&plain) {
        assert!(mine == theirs, "{name} differs from plain cargo's");
    }
    assert_eq!(replayed.len(), plain.len());
}

#[test]
fn a_three_crate_chain_rebuilds_the_middle_and_its_dependents() {
    let files = vec![
        ("Cargo.toml", "[workspace]\nmembers = [\"a\", \"b\", \"c\"]\nresolver = \"2\"\n".to_string()),
        ("a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n".to_string()),
        ("a/src/lib.rs", "pub fn a() -> u32 {\n    1\n}\n".to_string()),
        ("b/Cargo.toml", "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\na = { path = \"../a\" }\n".to_string()),
        ("b/src/lib.rs", "pub fn b() -> u32 {\n    a::a() * 10\n}\n".to_string()),
        ("c/Cargo.toml", "[package]\nname = \"c\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nb = { path = \"../b\" }\n".to_string()),
        ("c/src/main.rs", "fn main() {\n    println!(\"{}\", b::b());\n}\n".to_string()),
    ];
    let fx = Fixture::new("exec-chain", &files);
    assert_success(&fx.run("on").0);
    assert_eq!(fx.output("c"), "10");
    fx.edit("b/src/lib.rs", "pub fn b() -> u32 {\n    a::a() * 20\n}\n");
    let out = fx.replayed(2);
    assert!(!stderr(&out).contains("Compiling a "), "a is unchanged: {}", stderr(&out));
    assert_eq!(fx.output("c"), "20");
    fx.edit("a/src/lib.rs", "pub fn a() -> u32 {\n    2\n}\n");
    fx.replayed(3);
    assert_eq!(fx.output("c"), "40");
    fx.edit("b/src/lib.rs", "pub fn b() -> u32 {\n    a::a() * 30\n}\n");
    fx.audited();
    assert_eq!(fx.output("c"), "60");
}

#[test]
fn a_binary_links_only_after_every_transitive_library_is_complete() {
    // The binary names only `thin`; `heavy` reaches its link through `-L dependency=`, as grep-cli reaches rg through grep.
    let mut heavy = String::from("pub fn value() -> u32 {\n    1\n}\n");
    for i in 0..8000 {
        heavy.push_str(&format!("pub fn f{i}(x: u64) -> u64 {{ x.wrapping_mul({i}).rotate_left(3) ^ {i} }}\n"));
    }
    let files = vec![
        ("Cargo.toml", "[workspace]\nmembers = [\"heavy\", \"thin\", \"app\"]\nresolver = \"2\"\n".to_string()),
        ("heavy/Cargo.toml", "[package]\nname = \"heavy\"\nversion = \"0.1.0\"\nedition = \"2021\"\n".to_string()),
        ("heavy/src/lib.rs", heavy.clone()),
        ("thin/Cargo.toml", "[package]\nname = \"thin\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nheavy = { path = \"../heavy\" }\n".to_string()),
        ("thin/src/lib.rs", "pub use heavy::value;\n".to_string()),
        ("app/Cargo.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nthin = { path = \"../thin\" }\n".to_string()),
        ("app/src/main.rs", "fn main() {\n    println!(\"{}\", thin::value());\n}\n".to_string()),
    ];
    let mut fx = Fixture::new("exec-transitive", &files);
    fx.env.push(("CARGO_INCREMENTAL".into(), "0".into()));
    assert_success(&fx.run("on").0);
    for n in 2..5 {
        fx.edit("heavy/src/lib.rs", &heavy.replacen("    1\n", &format!("    {n}\n"), 1));
        let out = fx.replayed(3);
        assert!(!stderr(&out).contains("rlib format"), "{}", stderr(&out));
        assert_eq!(fx.output("app"), n.to_string());
    }
}

#[test]
fn new_module_files_and_proc_macros_are_followed() {
    let files = vec![
        ("Cargo.toml", "[workspace]\nmembers = [\"macros\", \"applib\", \"app\"]\nresolver = \"2\"\n".to_string()),
        ("macros/Cargo.toml", "[package]\nname = \"macros\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\nproc-macro = true\n".to_string()),
        ("macros/src/lib.rs", "use proc_macro::TokenStream;\n\n#[proc_macro]\npub fn answer(_: TokenStream) -> TokenStream {\n    \"41\".parse().unwrap()\n}\n".to_string()),
        ("applib/Cargo.toml", APPLIB.to_string()),
        ("applib/src/lib.rs", "pub fn value() -> u32 {\n    1\n}\n".to_string()),
        ("app/Cargo.toml", format!("{APP}macros = {{ path = \"../macros\" }}\n")),
        ("app/src/main.rs", "fn main() {\n    println!(\"{} {}\", applib::value(), macros::answer!());\n}\n".to_string()),
    ];
    let fx = Fixture::new("exec-modules", &files);
    assert_success(&fx.run("on").0);
    assert_eq!(fx.output("app"), "1 41");
    fx.edit("applib/src/extra.rs", "pub fn extra() -> u32 {\n    5\n}\n");
    fx.edit("applib/src/lib.rs", "mod extra;\n\npub fn value() -> u32 {\n    extra::extra()\n}\n");
    fx.replayed(2);
    assert_eq!(fx.output("app"), "5 41");
    fx.edit("applib/src/extra.rs", "pub fn extra() -> u32 {\n    6\n}\n");
    fx.replayed(2);
    assert_eq!(fx.output("app"), "6 41");
    fx.edit("macros/src/lib.rs", "use proc_macro::TokenStream;\n\n#[proc_macro]\npub fn answer(_: TokenStream) -> TokenStream {\n    \"42\".parse().unwrap()\n}\n");
    fx.replayed(2);
    assert_eq!(fx.output("app"), "6 42");
    fx.edit("applib/src/extra.rs", "pub fn extra() -> u32 {\n    7\n}\n");
    fx.audited();
    assert_eq!(fx.output("app"), "7 42");
}

#[test]
fn build_scripts_decline_only_when_cargo_would_rerun_them() {
    let gen_build = "fn main() {\n    println!(\"cargo:rerun-if-changed=build.rs\");\n    println!(\"cargo:rerun-if-changed=data.txt\");\n    let data = std::fs::read_to_string(\"data.txt\").unwrap();\n    let out = std::env::var(\"OUT_DIR\").unwrap();\n    std::fs::write(format!(\"{out}/gen.rs\"), format!(\"pub const DATA: &str = {:?};\\n\", data.trim())).unwrap();\n}\n";
    let files = vec![
        ("Cargo.toml", "[workspace]\nmembers = [\"gen\", \"plain\", \"app\"]\nresolver = \"2\"\n".to_string()),
        ("gen/Cargo.toml", "[package]\nname = \"gen\"\nversion = \"0.1.0\"\nedition = \"2021\"\n".to_string()),
        ("gen/build.rs", gen_build.to_string()),
        ("gen/data.txt", "one\n".to_string()),
        ("gen/src/lib.rs", "include!(concat!(env!(\"OUT_DIR\"), \"/gen.rs\"));\npub fn tag() -> &'static str {\n    \"g1\"\n}\n".to_string()),
        ("plain/Cargo.toml", "[package]\nname = \"plain\"\nversion = \"0.1.0\"\nedition = \"2021\"\n".to_string()),
        ("plain/build.rs", "fn main() {}\n".to_string()),
        ("plain/src/lib.rs", "pub fn value() -> u32 {\n    1\n}\n".to_string()),
        ("app/Cargo.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\ngen = { path = \"../gen\" }\nplain = { path = \"../plain\" }\n".to_string()),
        ("app/src/main.rs", "fn main() {\n    println!(\"{} {} {}\", gen::DATA, gen::tag(), plain::value());\n}\n".to_string()),
    ];
    let fx = Fixture::new("exec-scripts", &files);
    assert_success(&fx.run("on").0);
    assert_eq!(fx.output("app"), "one g1 1");
    fx.edit("gen/src/lib.rs", "include!(concat!(env!(\"OUT_DIR\"), \"/gen.rs\"));\npub fn tag() -> &'static str {\n    \"g2\"\n}\n");
    fx.replayed(2);
    assert_eq!(fx.output("app"), "one g2 1");
    fx.edit("gen/data.txt", "two\n");
    fx.declined("build script would rerun");
    assert_eq!(fx.output("app"), "two g2 1");
    fx.edit("plain/src/lib.rs", "pub fn value() -> u32 {\n    2\n}\n");
    fx.declined("build script would rerun");
    assert_eq!(fx.output("app"), "two g2 2");
    fx.edit("app/src/main.rs", "fn main() {\n    println!(\"{} {} {}!\", gen::DATA, gen::tag(), plain::value());\n}\n");
    fx.replayed(1);
    assert_eq!(fx.output("app"), "two g2 2!");
    fx.edit("gen/src/lib.rs", "include!(concat!(env!(\"OUT_DIR\"), \"/gen.rs\"));\npub fn tag() -> &'static str {\n    \"g3\"\n}\n");
    fx.audited();
    assert_eq!(fx.output("app"), "two g3 2!");
}

#[test]
fn compile_errors_and_warnings_match_cargo() {
    let fx = Fixture::new("exec-diagnostics", &lib_and_bin());
    let plain_target = fx.sb.root.join("plain-target");
    assert_success(&fx.run("on").0);
    assert_success(&fx.plain(&plain_target));

    fx.edit("applib/src/lib.rs", "pub fn value() -> u32 {\n    \"s\"\n}\n");
    let (out, trace) = fx.run("on");
    assert!(Fixture::exec(&trace).starts_with("replay:"), "trace {trace}");
    assert_eq!(out.status.code(), Some(101), "stderr: {}", stderr(&out));
    let plain = fx.plain(&plain_target);
    assert_eq!(plain.status.code(), Some(101));
    assert_eq!(diagnostics(&out), diagnostics(&plain));
    assert!(stderr(&out).contains("error: could not compile `applib` (lib) due to 1 previous error"), "stderr: {}", stderr(&out));

    fx.edit("applib/src/lib.rs", "pub fn value() -> u32 {\n    let unused = 1;\n    8\n}\n");
    let out = fx.replayed(2);
    let plain = fx.plain(&plain_target);
    assert_success(&plain);
    assert_eq!(diagnostics(&out), diagnostics(&plain));
    assert!(stderr(&out).contains("warning: `applib` (lib) generated 1 warning"), "stderr: {}", stderr(&out));
    assert_eq!(fx.output("app"), "8");

    fx.edit("app/src/main.rs", &format!("{MAIN}// edited\n"));
    let out = fx.replayed(1);
    let plain = fx.plain(&plain_target);
    assert_eq!(diagnostics(&out), diagnostics(&plain), "the fresh library's warning is replayed as cargo does");
    fx.edit("app/src/main.rs", &format!("{MAIN}// again\n"));
    fx.audited();
}

#[cfg(unix)]
#[test]
fn an_interrupted_replay_leaves_a_correct_build() {
    let mut body = String::from("pub fn value() -> u32 {\n    1\n}\n");
    for i in 0..8000 {
        body.push_str(&format!("pub fn f{i}(x: u64) -> u64 {{ x.wrapping_mul({i}).rotate_left(3) ^ {i} }}\n"));
    }
    let mut files = lib_and_bin();
    files[2].1 = body.clone();
    let fx = Fixture::new("exec-interrupt", &files);
    assert_success(&fx.run("on").0);
    fx.edit("applib/src/lib.rs", &body.replacen("    1\n", "    9\n", 1));
    let mut cmd = fx.command("on");
    own_group(&mut cmd);
    let mut child = cmd.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped()).spawn().unwrap();
    let mut lines = std::io::BufRead::lines(std::io::BufReader::new(child.stderr.take().unwrap()));
    assert!(lines.any(|l| l.unwrap().contains("Compiling applib")), "the replay starts the edited library");
    std::thread::sleep(std::time::Duration::from_millis(200));
    // SAFETY: a terminal's ctrl-c reaches the whole foreground group, which `own_group` made for this build.
    unsafe { libc::kill(-(child.id() as i32), libc::SIGINT) };
    let rest: Vec<String> = lines.map_while(Result::ok).collect();
    let status = child.wait().unwrap();
    assert!(!status.success(), "the build was interrupted mid-compile: {rest:?}");
    let traces = fx.traces();
    assert!(Fixture::exec(traces.last().unwrap()).starts_with("replay:"), "traces {traces:?}");
    let (out, _) = fx.run("on");
    assert_success(&out);
    assert_eq!(fx.output("app"), "9", "after an interrupted build ({status:?})");
    fx.edit("applib/src/lib.rs", &body.replacen("    1\n", "    10\n", 1));
    fx.audited();
    assert_eq!(fx.output("app"), "10");
}

#[test]
fn env_changes_decline_or_rebuild_what_reads_them() {
    let mut files = lib_and_bin();
    files[4].1 = "fn main() {\n    println!(\"{} {}\", applib::value(), env!(\"EXEC_TEST_VAR\"));\n}\n".to_string();
    let mut fx = Fixture::new("exec-env", &files);
    fx.env.push(("EXEC_TEST_VAR".into(), "a".into()));
    assert_success(&fx.run("on").0);
    assert_eq!(fx.output("app"), "1 a");
    fx.env[0].1 = "b".into();
    fx.replayed(1);
    assert_eq!(fx.output("app"), "1 b");
    fx.env.push(("RUSTFLAGS".into(), "-Cdebug-assertions=on".into()));
    let (out, trace) = fx.run("on");
    assert_success(&out);
    assert_eq!(Fixture::exec(&trace), "decline:none", "other flags are another build: {trace}");
    fx.edit("applib/src/lib.rs", "pub fn value() -> u32 {\n    2\n}\n");
    fx.replayed(2);
    assert_eq!(fx.output("app"), "2 b");
}

#[test]
fn the_executor_works_with_the_no_op_path_switched_off() {
    let mut fx = Fixture::new("exec-noop-off", &lib_and_bin());
    fx.env.push(("FRUST_NOOP".into(), "off".into()));
    let (out, trace) = fx.run("on");
    assert_success(&out);
    assert_eq!(trace["plan"], "written", "trace {trace}");
    fx.edit("applib/src/lib.rs", "pub fn value() -> u32 {\n    3\n}\n");
    fx.replayed(2);
    assert_eq!(fx.output("app"), "3");
    fx.replayed(0);
}

#[test]
fn a_target_built_without_the_executor_is_adopted_through_the_cache() {
    let fx = Fixture::new("exec-adopt", &lib_and_bin());
    assert_success(&fx.run("off").0);
    fx.edit("app/src/main.rs", &format!("{MAIN}// one\n"));
    let (out, trace) = fx.run("on");
    assert_success(&out);
    assert!(trace["plan"].as_str().unwrap_or("").starts_with("skip:uncovered unit, adopting "), "the fresh library is adopted: {trace}");
    fx.edit("app/src/main.rs", &format!("{MAIN}// two\n"));
    let (out, trace) = fx.run("on");
    assert_success(&out);
    assert_eq!(trace["plan"], "written", "cargo recompiled the adopted unit under a journal: {trace}");
    assert_eq!(fx.sb.count_cache("hit", "applib"), 1, "the adopted unit came from the cache");
    fx.edit("applib/src/lib.rs", "pub fn value() -> u32 {\n    4\n}\n");
    fx.replayed(2);
    assert_eq!(fx.output("app"), "4");
}

#[test]
fn another_display_of_the_same_command_borrows_the_recorded_calls() {
    let mut fx = Fixture::new("exec-display", &lib_and_bin());
    assert_success(&fx.run("on").0);
    fx.env.push(("CARGO_TERM_HYPERLINKS".into(), "false".into()));
    fx.edit("app/src/main.rs", &format!("{MAIN}// other display\n"));
    let (out, trace) = fx.run("on");
    assert_success(&out);
    assert_eq!(trace["plan"], "written", "the fresh library's call comes from the other display's plan, nothing is adopted: {trace}");
    fx.edit("applib/src/lib.rs", "pub fn value() -> u32 {\n    7\n}\n");
    fx.replayed(2);
    assert_eq!(fx.output("app"), "7");
}

#[test]
fn a_package_whose_build_script_watches_everything_declines_from_stats_alone() {
    let files = vec![
        ("Cargo.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n".to_string()),
        ("build.rs", "fn main() {}\n".to_string()),
        ("src/main.rs", "fn main() {\n    println!(\"1\");\n}\n".to_string()),
    ];
    let fx = Fixture::new("exec-bound", &files);
    let (out, trace) = fx.run("on");
    assert_success(&out);
    assert_eq!(trace["plan"], "written", "trace {trace}");
    for value in ["2", "3"] {
        fx.edit("src/main.rs", &format!("fn main() {{\n    println!(\"{value}\");\n}}\n"));
        let (out, trace) = fx.run("on");
        assert_success(&out);
        assert_eq!(Fixture::exec(&trace), "decline:build script reruns on any change", "no plan is read or built: {trace}");
        assert_eq!(trace["plan"], "none", "nothing is journaled: {trace}");
        assert_eq!(fx.output("app"), value);
    }
    fx.edit("build.rs", "fn main() {\n    println!(\"cargo:rerun-if-changed=build.rs\");\n}\n");
    let (out, trace) = fx.run("on");
    assert_success(&out);
    assert_eq!(trace["plan"], "written", "a changed build script is looked at again: {trace}");
    fx.edit("src/main.rs", "fn main() {\n    println!(\"4\");\n}\n");
    fx.replayed(1);
    assert_eq!(fx.output("app"), "4");
}

/// A path package's cdylib keeps cargo's name without a hash, as executables do on windows-msvc, where every test here covers it.
#[cfg(not(windows))]
#[test]
fn an_output_named_without_a_hash_is_planned_and_replayed() {
    let lib = |n: u32| format!("#[no_mangle]\npub extern \"C\" fn value() -> u32 {{\n    {n}\n}}\n");
    let files = vec![
        ("Cargo.toml", "[workspace]\nmembers = [\"plugin\"]\nresolver = \"2\"\n".to_string()),
        ("plugin/Cargo.toml", "[package]\nname = \"plugin\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\ncrate-type = [\"cdylib\"]\n".to_string()),
        ("plugin/src/lib.rs", lib(1)),
    ];
    let fx = Fixture::new("exec-unhashed", &files);
    let (out, trace) = fx.run("on");
    assert_success(&out);
    assert_eq!(trace["plan"], "written", "trace {trace}");
    fx.edit("plugin/src/lib.rs", &lib(2));
    fx.replayed(1);
    fx.edit("plugin/src/lib.rs", &lib(3));
    fx.audited();
    fx.edit("plugin/src/lib.rs", &lib(4));
    fx.replayed(1);
    assert_eq!(cargo_rebuilds(&fx), Vec::<String>::new(), "cargo itself finds the replayed unit fresh");
}

#[test]
fn concurrent_builds_join_and_both_succeed() {
    let fx = Fixture::new("exec-concurrent", &lib_and_bin());
    assert_success(&fx.run("on").0);
    fx.edit("applib/src/lib.rs", "pub fn value() -> u32 {\n    6\n}\n");
    let first = fx.command("on").spawn().unwrap();
    let second = fx.command("on").spawn().unwrap();
    let (a, b) = (first.wait_with_output().unwrap(), second.wait_with_output().unwrap());
    assert_success(&a);
    assert_success(&b);
    let traces = fx.traces();
    let rebuilt = traces.iter().rev().take(2).filter(|t| Fixture::exec(t).starts_with("replay:2/")).count();
    assert!(rebuilt <= 1, "only one build compiles: {traces:?}");
    assert_eq!(fx.output("app"), "6");
}

/// A manifest written as cargo starts poisons the recorded graph, but the plan keeps cargo's calls, so the next edit's
/// cargo run finds its fresh units covered and the target is never mistaken for one built before the executor.
#[test]
fn a_manifest_written_as_the_build_starts_leaves_cargo_fingerprints_alone() {
    let fx = Fixture::new("exec-young-manifest", &lib_and_bin());
    let manifest = fx.dir.join("Cargo.toml");
    let soon = std::time::SystemTime::now() + std::time::Duration::from_secs(1);
    fs::File::options().write(true).open(&manifest).unwrap().set_modified(soon).unwrap();
    assert_success(&fx.run("on").0);
    settle(&manifest);
    fx.edit("app/src/main.rs", &format!("{MAIN}// edited\n"));
    let (out, trace) = fx.run("on");
    assert_success(&out);
    assert_eq!(trace["plan"], "written", "the edit's cargo run is planned: {trace}");
    assert_eq!(cargo_rebuilds(&fx), Vec::<String>::new(), "no run removed a fingerprint of a unit cargo built under a journal");
}
