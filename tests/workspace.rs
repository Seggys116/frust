//! Every wrapped tool runs against a multi-crate workspace; artifacts must land in the central target directory.

mod common;

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use common::{Sandbox, assert_success, stderr, stdout};

const MEMBERS: &[&str] = &["ws-core", "ws-macros", "ws-app"];

const CORE_LIB: &str = r#"/// The answer, almost.
///
/// ```
/// assert_eq!(ws_core::answer(), 41);
/// ```
pub fn answer() -> u32 {
    wsdep::base() + 1
}
"#;

const MACROS_LIB: &str = r#"use proc_macro::TokenStream;

#[proc_macro]
pub fn make_answer(_input: TokenStream) -> TokenStream {
    "fn answer_from_macro() -> u32 { 42 }".parse().unwrap()
}
"#;

const APP_BUILD_RS: &str = r#"fn main() {
    println!("cargo:rustc-env=BUILD_GREETING=hello-from-build");
    println!("cargo:rerun-if-changed=build.rs");
}
"#;

const APP_MAIN: &str = r#"use std::io::Read;

ws_macros::make_answer!();

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("sleep") => {
            println!("sleeping");
            let ms: u64 = args[1].parse().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(ms));
            println!("woke");
        }
        Some("stdin") => {
            let mut text = String::new();
            std::io::stdin().read_to_string(&mut text).unwrap();
            println!("stdin={}", text.trim());
        }
        Some("exit") => std::process::exit(args[1].parse().unwrap()),
        _ => println!(
            "core={} macro={} build={} loud={}",
            ws_core::answer(),
            answer_from_macro(),
            env!("BUILD_GREETING"),
            cfg!(feature = "loud")
        ),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn marker_test() {
        println!("captured-output-marker");
        assert_eq!(super::answer_from_macro(), 42);
    }

    #[test]
    fn maybe_fails() {
        assert!(std::env::var_os("WS_FAIL").is_none(), "asked to fail");
    }
}
"#;

fn write(path: &Path, content: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn workspace_with(sb: &Sandbox, dir: &str, root_prefix: &str, root_extra: &str) -> PathBuf {
    sb.vendor_crate("wsdep", "1.0.0", "pub fn base() -> u32 { 40 }\n");
    let ws = sb.root.join(dir);
    write(
        &ws.join("Cargo.toml"),
        &format!(
            "{root_prefix}[workspace]\nmembers = [\"crates/ws-core\", \"crates/ws-macros\", \"crates/ws-app\"]\nresolver = \"2\"\n{root_extra}"
        ),
    );
    let c = ws.join("crates");
    write(
        &c.join("ws-core/Cargo.toml"),
        "[package]\nname = \"ws-core\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nwsdep = \"=1.0.0\"\n",
    );
    write(&c.join("ws-core/src/lib.rs"), CORE_LIB);
    write(
        &c.join("ws-macros/Cargo.toml"),
        "[package]\nname = \"ws-macros\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\nproc-macro = true\n",
    );
    write(&c.join("ws-macros/src/lib.rs"), MACROS_LIB);
    write(
        &c.join("ws-app/Cargo.toml"),
        "[package]\nname = \"ws-app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[features]\nloud = []\n\n[dependencies]\nws-core = { path = \"../ws-core\" }\nws-macros = { path = \"../ws-macros\" }\n",
    );
    write(&c.join("ws-app/build.rs"), APP_BUILD_RS);
    write(&c.join("ws-app/src/main.rs"), APP_MAIN);
    write(&c.join("ws-app/examples/demo.rs"), "fn main() {\n    println!(\"demo\");\n}\n");
    ws
}

fn workspace(sb: &Sandbox, dir: &str) -> PathBuf {
    workspace_with(sb, dir, "", "")
}

fn fcargo(sb: &Sandbox, dir: &Path, args: &[&str]) -> Output {
    sb.bin("fcargo").args(args).current_dir(dir).output().unwrap()
}

fn plain_cargo(sb: &Sandbox, dir: &Path, args: &[&str]) -> Output {
    sb.cargo().args(args).env("CARGO_TARGET_DIR", sb.root.join("plain-target")).current_dir(dir).output().unwrap()
}

fn assert_tree_clean(ws: &Path) {
    assert!(!ws.join("target").exists(), "workspace root grew a target/ directory");
    for m in MEMBERS {
        assert!(!ws.join("crates").join(m).join("target").exists(), "member {m} grew a target/ directory");
    }
}

fn run_exe(path: &Path, args: &[&str]) -> Output {
    Command::new(path).args(args).output().unwrap_or_else(|e| panic!("cannot run {}: {e}", path.display()))
}

fn deps_with_prefix(dir: &Path, prefix: &str) -> Vec<String> {
    fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with(prefix))
                .collect()
        })
        .unwrap_or_default()
}

fn cargo_target_directory(cmd: &mut Command) -> PathBuf {
    let out = cmd.args(["metadata", "--format-version", "1", "--no-deps"]).output().unwrap();
    assert_success(&out);
    let meta: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    PathBuf::from(meta["target_directory"].as_str().unwrap())
}

fn rustc_line<'a>(verbose_stderr: &'a str, krate: &str) -> &'a str {
    let needle = format!("--crate-name {krate} ");
    verbose_stderr
        .lines()
        .find(|l| l.contains("Running") && l.contains(&needle))
        .unwrap_or_else(|| panic!("no rustc invocation for {krate} in:\n{verbose_stderr}"))
}

#[test]
fn workspace_build_uses_central_target_and_keeps_tree_clean() {
    let sb = Sandbox::new("ws-build");
    let ws = workspace(&sb, "ws");
    assert_success(&fcargo(&sb, &ws, &["build"]));
    let central = sb.central_target(&ws);
    assert!(central.starts_with(sb.home().join("targets")), "{}", central.display());
    let out = run_exe(&central.join("debug/ws-app"), &[]);
    assert_eq!(stdout(&out).trim(), "core=41 macro=42 build=hello-from-build loud=false");
    assert_tree_clean(&ws);
    let marker = fs::read_to_string(central.parent().unwrap().join("workspace")).unwrap();
    assert_eq!(Path::new(&marker).canonicalize().unwrap(), ws.canonicalize().unwrap(), "workspace marker names the root");
}

#[test]
fn member_subdirectory_resolves_the_same_central_dir_as_the_root() {
    let sb = Sandbox::new("ws-member");
    let ws = workspace(&sb, "ws");
    let member = ws.join("crates/ws-app");
    let from_root = sb.central_target(&ws);
    assert_eq!(sb.central_target(&member), from_root);
    assert_eq!(sb.central_target(&ws.join("crates/ws-core/src")), from_root);
    assert_success(&fcargo(&sb, &member, &["build"]));
    assert!(from_root.join("debug/ws-app").is_file());
    assert_tree_clean(&ws);
}

#[test]
fn workspaces_with_the_same_directory_name_get_distinct_central_dirs() {
    let sb = Sandbox::new("ws-samename");
    let a = workspace(&sb, "alpha/ws");
    let b = workspace(&sb, "beta/ws");
    let (ta, tb) = (sb.central_target(&a), sb.central_target(&b));
    assert_ne!(ta, tb);
    for t in [&ta, &tb] {
        assert!(t.parent().unwrap().file_name().unwrap().to_string_lossy().starts_with("ws-"), "{}", t.display());
    }
    // Change one so the outputs differ, then make sure neither build sees the other's.
    write(&b.join("crates/ws-core/src/lib.rs"), "pub fn answer() -> u32 {\n    wsdep::base() + 2\n}\n");
    assert_success(&fcargo(&sb, &a, &["build"]));
    assert_success(&fcargo(&sb, &b, &["build"]));
    assert!(stdout(&run_exe(&ta.join("debug/ws-app"), &[])).starts_with("core=41 "));
    assert!(stdout(&run_exe(&tb.join("debug/ws-app"), &[])).starts_with("core=42 "));
    assert_tree_clean(&a);
    assert_tree_clean(&b);
}

#[test]
fn check_all_targets_and_doc_use_the_central_dir() {
    let sb = Sandbox::new("ws-check-doc");
    let ws = workspace(&sb, "ws");
    let central = sb.central_target(&ws);
    assert_success(&fcargo(&sb, &ws, &["check", "--workspace", "--all-targets"]));
    assert!(central.join("debug/.fingerprint").is_dir(), "check wrote its fingerprints centrally");
    assert_success(&fcargo(&sb, &ws, &["build", "--workspace", "--all-targets"]));
    assert!(central.join("debug/examples/demo").is_file(), "--all-targets built the example");
    assert_eq!(stdout(&run_exe(&central.join("debug/examples/demo"), &[])).trim(), "demo");
    assert_success(&fcargo(&sb, &ws, &["doc", "--workspace", "--no-deps"]));
    assert!(central.join("doc/ws_core/index.html").is_file(), "docs went to the central dir");
    assert!(central.join("doc/ws_macros/index.html").is_file());
    assert_tree_clean(&ws);
}

#[test]
fn fclippy_lints_the_workspace_and_reports_clippy_findings() {
    let sb = Sandbox::new("ws-clippy");
    let ws = workspace(&sb, "ws");
    let clean = sb.bin("fclippy").args(["--workspace", "--all-targets", "--", "-D", "warnings"]).current_dir(&ws).output().unwrap();
    assert_success(&clean);
    assert!(sb.central_target(&ws).join("debug").is_dir());
    // `x == x` is clippy's deny-by-default `eq_op`; rustc itself accepts it.
    write(&ws.join("crates/ws-core/src/lib.rs"), &format!("{CORE_LIB}\npub fn same(x: u32) -> bool {{\n    x == x\n}}\n"));
    let linted = sb.bin("fclippy").args(["-p", "ws-core"]).current_dir(&ws).output().unwrap();
    assert!(!linted.status.success(), "clippy should reject eq_op");
    assert!(stderr(&linted).contains("eq_op"), "{}", stderr(&linted));
    assert_success(&fcargo(&sb, &ws, &["check", "-p", "ws-core"]));
    let via_fcargo = fcargo(&sb, &ws, &["clippy", "-p", "ws-core"]);
    assert_eq!(via_fcargo.status.code(), linted.status.code());
    assert_tree_clean(&ws);
}

#[test]
fn package_selection_and_features() {
    let sb = Sandbox::new("ws-pkg");
    let ws = workspace(&sb, "ws");
    let central = sb.central_target(&ws);
    assert_success(&fcargo(&sb, &ws, &["build", "-p", "ws-core"]));
    assert!(!deps_with_prefix(&central.join("debug/deps"), "libws_core-").is_empty());
    assert!(!central.join("debug/ws-app").exists(), "-p ws-core must not build the app");
    assert_success(&fcargo(&sb, &ws, &["build", "-p", "ws-app", "--features", "loud"]));
    assert!(stdout(&run_exe(&central.join("debug/ws-app"), &[])).ends_with("loud=true\n"));
    let run = fcargo(&sb, &ws, &["run", "-q", "-p", "ws-app", "--features", "ws-app/loud"]);
    assert_success(&run);
    assert_eq!(stdout(&run).trim(), "core=41 macro=42 build=hello-from-build loud=true");
    assert_tree_clean(&ws);
}

#[test]
fn manifest_path_resolves_the_workspace_it_names() {
    let sb = Sandbox::new("ws-manifest");
    let ws = workspace(&sb, "ws");
    let elsewhere = sb.root.join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    // The macOS sandbox is under a /var/folders symlink, and `cargo locate-project` spells a --manifest-path differently from a working directory, so both must map to one key.
    let manifest = ws.join("crates/ws-app/Cargo.toml");
    assert_success(&fcargo(&sb, &elsewhere, &["build", "--manifest-path", manifest.to_str().unwrap()]));
    let keys = || -> Vec<String> {
        fs::read_dir(sb.home().join("targets"))
            .map(|rd| rd.filter_map(Result::ok).map(|e| e.file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_default()
    };
    assert!(
        sb.central_target(&ws).join("debug/ws-app").is_file(),
        "--manifest-path {} built under {:?}, but `frust where` in the workspace says {}",
        manifest.display(),
        keys(),
        sb.central_target(&ws).display()
    );
    assert!(!elsewhere.join("target").exists());
    assert_tree_clean(&ws);
    assert_success(&fcargo(&sb, &sb.root, &["check", "--manifest-path=ws/Cargo.toml", "--workspace"]));
    assert!(!sb.root.join("target").exists());
    let targets: Vec<_> = fs::read_dir(sb.home().join("targets")).unwrap().collect();
    assert_eq!(targets.len(), 1, "both invocations map to one workspace key");
}

#[test]
fn chdir_flag_resolves_the_target_workspace() {
    let sb = Sandbox::new("ws-chdir");
    let ws = workspace(&sb, "ws");
    // `-C` is unstable: on stable both cargo and fcargo must fail the same way.
    let ours = fcargo(&sb, &sb.root, &["-C", "ws", "build"]);
    let theirs = plain_cargo(&sb, &sb.root, &["-C", "ws", "build"]);
    assert_eq!(ours.status.code(), theirs.status.code());
    assert_eq!(stderr(&ours), stderr(&theirs));
    let out = fcargo(&sb, &sb.root, &["+nightly", "-Z", "unstable-options", "-C", "ws", "build"]);
    assert_success(&out);
    let key_dir = sb.central_target(&ws).parent().unwrap().to_path_buf();
    let built: Vec<PathBuf> = fs::read_dir(&key_dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.join("debug/ws-app").is_file())
        .collect();
    assert_eq!(built.len(), 1, "nightly build in {}", key_dir.display());
    assert!(!sb.root.join("target").exists());
    assert_tree_clean(&ws);
}

#[test]
fn toolchain_file_and_plus_toolchain_get_their_own_central_dirs() {
    let sb = Sandbox::new("ws-toolchain");
    let ws = workspace(&sb, "ws");
    write(&ws.join("rust-toolchain.toml"), "[toolchain]\nchannel = \"1.98.1\"\n");
    // `cargo test` exports RUSTUP_TOOLCHAIN, which would override rust-toolchain.toml, so commands here run without it.
    let where_ = |dir: &Path| {
        let out = sb.bin("frust").arg("where").env_remove("RUSTUP_TOOLCHAIN").current_dir(dir).output().unwrap();
        assert_success(&out);
        PathBuf::from(stdout(&out).trim())
    };
    let pinned = where_(&ws);
    let plain = sb.project("noplain", &[], "fn main() {}\n");
    let stable_id = where_(&plain).file_name().unwrap().to_owned();
    assert_ne!(pinned.file_name().unwrap(), stable_id, "the pinned toolchain has its own id");

    let out = sb.bin("fcargo").arg("build").env_remove("RUSTUP_TOOLCHAIN").current_dir(&ws).output().unwrap();
    assert_success(&out);
    assert!(pinned.join("debug/ws-app").is_file(), "rust-toolchain.toml selects {}", pinned.display());
    let version = sb.bin("fcargo").arg("--version").env_remove("RUSTUP_TOOLCHAIN").current_dir(&ws).output().unwrap();
    assert!(stdout(&version).starts_with("cargo 1.98.1"), "{}", stdout(&version));

    let out = sb.bin("fcargo").args(["+stable", "build"]).env_remove("RUSTUP_TOOLCHAIN").current_dir(&ws).output().unwrap();
    assert_success(&out);
    let stable_dir = pinned.parent().unwrap().join(&stable_id);
    assert!(stable_dir.join("debug/ws-app").is_file(), "+stable builds into {}", stable_dir.display());
    assert_tree_clean(&ws);
}

#[test]
fn explicit_target_dir_flag_is_respected_in_a_workspace() {
    let sb = Sandbox::new("ws-targetdir");
    let ws = workspace(&sb, "ws");
    let dir = sb.root.join("my-target");
    assert_success(&fcargo(&sb, &ws, &["build", "--target-dir", dir.to_str().unwrap()]));
    assert!(dir.join("debug/ws-app").is_file());
    assert!(!sb.home().join("targets").exists(), "no central dir when --target-dir is given");
    assert_tree_clean(&ws);
}

#[test]
fn cargo_build_target_dir_env_is_respected() {
    let sb = Sandbox::new("ws-envtd");
    let ws = workspace(&sb, "ws");
    let dir = sb.root.join("env-target");
    assert_eq!(cargo_target_directory(sb.cargo().env("CARGO_BUILD_TARGET_DIR", &dir).current_dir(&ws)), dir);
    let out = sb.bin("fcargo").arg("build").env("CARGO_BUILD_TARGET_DIR", &dir).current_dir(&ws).output().unwrap();
    assert_success(&out);
    assert!(dir.join("debug/ws-app").is_file(), "CARGO_BUILD_TARGET_DIR must win over the central dir");
}

#[test]
fn config_flag_target_dir_is_respected() {
    let sb = Sandbox::new("ws-cfgtd");
    let ws = workspace(&sb, "ws");
    let dir = sb.root.join("cli-config-target");
    let setting = format!("build.target-dir=\"{}\"", dir.display());
    assert_eq!(cargo_target_directory(sb.cargo().args(["--config", &setting]).current_dir(&ws)), dir);
    assert_success(&fcargo(&sb, &ws, &["--config", &setting, "build"]));
    assert!(dir.join("debug/ws-app").is_file(), "--config build.target-dir must win over the central dir");
}

#[test]
fn target_dir_in_member_cargo_config_is_respected() {
    let sb = Sandbox::new("ws-membercfg");
    let ws = workspace(&sb, "ws");
    let member = ws.join("crates/ws-app");
    let dir = sb.root.join("member-config-target");
    write(&member.join(".cargo/config.toml"), &format!("[build]\ntarget-dir = \"{}\"\n", dir.display()));
    // Plain cargo reads config from the working directory upwards, so it uses this dir.
    assert_eq!(cargo_target_directory(sb.cargo().current_dir(&member)), dir);
    assert_success(&fcargo(&sb, &member, &["build"]));
    assert!(dir.join("debug/ws-app").is_file(), "fcargo must honour the member's build.target-dir like cargo");
}

#[test]
fn alias_from_cargo_config_uses_the_central_dir() {
    let sb = Sandbox::new("ws-alias");
    let ws = workspace(&sb, "ws");
    write(&ws.join(".cargo/config.toml"), "[alias]\nbapp = \"build -p ws-app --features loud\"\n");
    assert_success(&fcargo(&sb, &ws, &["bapp"]));
    let exe = sb.central_target(&ws).join("debug/ws-app");
    assert!(exe.is_file());
    assert!(stdout(&run_exe(&exe, &[])).ends_with("loud=true\n"));
    assert_tree_clean(&ws);
}

#[test]
fn workspace_profiles_are_applied_and_tune_never_overrides_them() {
    let sb = Sandbox::new("ws-profile");
    let ws = workspace_with(&sb, "ws", "", "\n[profile.dev]\nopt-level = 1\n\n[profile.dev.package.\"*\"]\ndebug = 1\n");
    let out = sb.bin("fcargo").args(["build", "-v"]).env("FRUST_TUNE", "on").current_dir(&ws).output().unwrap();
    assert_success(&out);
    let err = stderr(&out);
    assert!(rustc_line(&err, "wsdep").contains("-C debuginfo=1"), "{}", rustc_line(&err, "wsdep"));
    assert!(rustc_line(&err, "ws_app").contains("-C opt-level=1"), "{}", rustc_line(&err, "ws_app"));
    assert!(!err.contains("debug=0"), "tuning must not be injected over a workspace profile");
}

#[test]
fn workspace_profile_written_as_dotted_keys_also_blocks_tune() {
    let sb = Sandbox::new("ws-profile-dotted");
    let ws = workspace_with(&sb, "ws", "profile.dev.package.\"*\".debug = 1\n\n", "");
    let reference = sb.cargo().args(["build", "-v"]).env("CARGO_TARGET_DIR", sb.root.join("plain-target")).current_dir(&ws).output().unwrap();
    assert_success(&reference);
    assert!(rustc_line(&stderr(&reference), "wsdep").contains("-C debuginfo=1"));
    let out = sb.bin("fcargo").args(["build", "-v"]).env("FRUST_TUNE", "on").current_dir(&ws).output().unwrap();
    assert_success(&out);
    let err = stderr(&out);
    assert!(rustc_line(&err, "wsdep").contains("-C debuginfo=1"), "{}", rustc_line(&err, "wsdep"));
}

#[test]
fn tune_applies_to_dependencies_when_no_profile_is_defined() {
    let sb = Sandbox::new("ws-tune");
    let ws = workspace(&sb, "ws");
    let tuned = sb.bin("fcargo").args(["build", "-v"]).env("FRUST_TUNE", "on").current_dir(&ws).output().unwrap();
    assert_success(&tuned);
    assert!(!rustc_line(&stderr(&tuned), "wsdep").contains("debuginfo=2"), "{}", rustc_line(&stderr(&tuned), "wsdep"));
    assert!(rustc_line(&stderr(&tuned), "ws_core").contains("debuginfo=2"));
    let sb2 = Sandbox::new("ws-tune-off");
    let ws2 = workspace(&sb2, "ws");
    let untuned = sb2.bin("fcargo").args(["build", "-v"]).env("FRUST_TUNE", "off").current_dir(&ws2).output().unwrap();
    assert_success(&untuned);
    assert!(rustc_line(&stderr(&untuned), "wsdep").contains("debuginfo=2"));
}

#[test]
fn install_from_path_uses_the_cache_but_no_central_dir() {
    let sb = Sandbox::new("ws-install");
    let ws = workspace(&sb, "ws");
    let root = sb.root.join("install-root");
    let out = fcargo(&sb, &ws, &["install", "--path", "crates/ws-app", "--root", root.to_str().unwrap()]);
    assert_success(&out);
    let exe = root.join("bin/ws-app");
    assert_eq!(stdout(&run_exe(&exe, &[])).trim(), "core=41 macro=42 build=hello-from-build loud=false");
    assert!(!sb.home().join("targets").exists(), "install must not get a central target dir");
    assert!(!sb.cache_events().is_empty(), "install compiles through the cache wrapper");
}

#[test]
fn install_from_path_keeps_the_project_clean_like_plain_cargo() {
    let sb = Sandbox::new("ws-install-clean");
    let ws = workspace(&sb, "ws");
    let reference = workspace(&sb, "ref");
    let root = sb.root.join("install-root");
    assert_success(&sb.cargo().args(["install", "--path", "crates/ws-app", "--root", root.to_str().unwrap()]).current_dir(&reference).output().unwrap());
    assert_success(&fcargo(&sb, &ws, &["install", "--force", "--path", "crates/ws-app", "--root", root.to_str().unwrap()]));
    assert_eq!(
        ws.join("target").exists(),
        reference.join("target").exists(),
        "fcargo install leaves the same footprint in the project as cargo install"
    );
}

#[test]
fn clean_package_removes_only_that_member_from_the_central_dir() {
    let sb = Sandbox::new("ws-clean");
    let ws = workspace(&sb, "ws");
    let central = sb.central_target(&ws);
    assert_success(&fcargo(&sb, &ws, &["build"]));
    let deps = central.join("debug/deps");
    assert!(!deps_with_prefix(&deps, "libws_core-").is_empty());
    assert_success(&fcargo(&sb, &ws, &["clean", "-p", "ws-core"]));
    assert!(deps_with_prefix(&deps, "libws_core-").is_empty(), "ws-core artifacts remain: {:?}", deps_with_prefix(&deps, "libws_core-"));
    assert!(!deps_with_prefix(&deps, "libwsdep-").is_empty(), "other packages are kept");
    assert_tree_clean(&ws);
    assert_success(&fcargo(&sb, &ws, &["clean"]));
    assert!(!central.join("debug").exists(), "full clean empties the central dir");
    assert_tree_clean(&ws);
}

#[test]
fn passthrough_commands_match_cargo_byte_for_byte() {
    let sb = Sandbox::new("ws-passthrough");
    let ws = workspace(&sb, "ws");
    assert_success(&sb.cargo().arg("generate-lockfile").current_dir(&ws).output().unwrap());
    let member = ws.join("crates/ws-app");
    let cases: &[(&Path, &[&str])] = &[
        (&ws, &["metadata", "--format-version", "1"]),
        (&member, &["metadata", "--format-version", "1", "--no-deps"]),
        (&ws, &["tree", "--workspace"]),
        (&ws, &["pkgid", "-p", "ws-core"]),
        (&member, &["locate-project", "--workspace"]),
        (&ws, &["read-manifest", "--manifest-path", "crates/ws-core/Cargo.toml"]),
        (&ws, &["pkgid", "-p", "does-not-exist"]),
        (&ws, &["fmt", "--check"]),
    ];
    for (dir, args) in cases {
        let ours = fcargo(&sb, dir, args);
        let theirs = sb.cargo().args(*args).current_dir(dir).output().unwrap();
        assert_eq!(ours.status.code(), theirs.status.code(), "exit code of {args:?}");
        assert_eq!(ours.stdout, theirs.stdout, "stdout of {args:?}");
        assert_eq!(stderr(&ours), stderr(&theirs), "stderr of {args:?}");
    }
    assert_tree_clean(&ws);
}

#[test]
fn run_and_test_exit_codes_and_stdout_match_cargo() {
    let sb = Sandbox::new("ws-codes");
    let ws = workspace(&sb, "ws");
    let cases: &[&[&str]] = &[
        &["run", "-q", "-p", "ws-app"],
        &["run", "-q", "-p", "ws-app", "--", "exit", "3"],
        &["run", "-q", "-p", "ws-app", "--example", "demo"],
    ];
    for args in cases {
        let ours = fcargo(&sb, &ws, args);
        let theirs = plain_cargo(&sb, &ws, args);
        assert_eq!(ours.status.code(), theirs.status.code(), "exit code of {args:?}");
        assert_eq!(ours.stdout, theirs.stdout, "stdout of {args:?}");
    }
    let failing = sb.bin("fcargo").args(["test", "-q", "-p", "ws-app"]).env("WS_FAIL", "1").current_dir(&ws).output().unwrap();
    let reference = sb.cargo().args(["test", "-q", "-p", "ws-app"]).env("WS_FAIL", "1").env("CARGO_TARGET_DIR", sb.root.join("plain-target")).current_dir(&ws).output().unwrap();
    assert!(!failing.status.success());
    assert_eq!(failing.status.code(), reference.status.code());
    assert!(stdout(&failing).contains("asked to fail"), "{}", stdout(&failing));
    write(&ws.join("crates/ws-core/src/lib.rs"), "pub fn answer() -> u32 {\n    \"not a number\"\n}\n");
    let ours = fcargo(&sb, &ws, &["build"]);
    let theirs = plain_cargo(&sb, &ws, &["build"]);
    assert_eq!(ours.status.code(), theirs.status.code());
    assert!(stderr(&ours).contains("mismatched types"), "{}", stderr(&ours));
    assert_tree_clean(&ws);
}

#[test]
fn test_forwards_harness_arguments_and_output() {
    let sb = Sandbox::new("ws-nocapture");
    let ws = workspace(&sb, "ws");
    let shown = fcargo(&sb, &ws, &["test", "-p", "ws-app", "--", "--nocapture", "marker_test"]);
    assert_success(&shown);
    assert!(stdout(&shown).contains("captured-output-marker"), "{}", stdout(&shown));
    assert!(stdout(&shown).contains("test result: ok. 1 passed"), "{}", stdout(&shown));
    let hidden = fcargo(&sb, &ws, &["test", "-p", "ws-app", "--", "marker_test"]);
    assert_success(&hidden);
    assert!(!stdout(&hidden).contains("captured-output-marker"), "output is captured without --nocapture");
    let all = fcargo(&sb, &ws, &["test", "--workspace"]);
    assert_success(&all);
    assert!(stdout(&all).contains("test result: ok. 2 passed"), "{}", stdout(&all));
    assert_tree_clean(&ws);
}

#[test]
fn test_doc_runs_doctests() {
    let sb = Sandbox::new("ws-doctest");
    let ws = workspace(&sb, "ws");
    let reference = plain_cargo(&sb, &ws, &["test", "--doc", "-p", "ws-core"]);
    assert_success(&reference);
    let out = fcargo(&sb, &ws, &["test", "--doc", "-p", "ws-core"]);
    assert_success(&out);
    assert!(stdout(&out).contains("test result: ok. 1 passed"), "{}", stdout(&out));
}

#[test]
fn run_releases_the_lock_before_the_program_runs() {
    let sb = Sandbox::new("ws-runlock");
    let ws = workspace(&sb, "ws");
    assert_success(&fcargo(&sb, &ws, &["build"]));
    let mut runner = sb
        .bin("fcargo")
        .args(["run", "-q", "-p", "ws-app", "--", "sleep", "3000"])
        .current_dir(&ws)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(runner.stdout.take().unwrap()).lines();
    assert_eq!(lines.next().unwrap().unwrap(), "sleeping", "the program started");
    write(&ws.join("crates/ws-core/src/lib.rs"), &format!("{CORE_LIB}// edited\n"));
    let started = Instant::now();
    let second = fcargo(&sb, &ws, &["build", "-p", "ws-core"]);
    let took = started.elapsed();
    assert_success(&second);
    assert!(runner.try_wait().unwrap().is_none(), "the second build finished only after the program exited ({took:?})");
    assert!(!stderr(&second).contains("waiting for build lock"), "{}", stderr(&second));
    assert!(!stderr(&second).contains("Blocking"), "{}", stderr(&second));
    assert!(took < Duration::from_millis(2500), "second build took {took:?}");
    assert_eq!(lines.next().unwrap().unwrap(), "woke");
    assert!(runner.wait().unwrap().success());
}

#[test]
fn run_passes_stdin_to_the_program() {
    let sb = Sandbox::new("ws-stdin");
    let ws = workspace(&sb, "ws");
    let mut child = sb
        .bin("fcargo")
        .args(["run", "-q", "-p", "ws-app", "--", "stdin"])
        .current_dir(&ws)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"ping from the test\n").unwrap();
    let mut out = String::new();
    child.stdout.take().unwrap().read_to_string(&mut out).unwrap();
    let status = child.wait().unwrap();
    assert!(status.success());
    assert_eq!(out.trim(), "stdin=ping from the test");
}

#[test]
fn fmt_through_fcargo_and_frustfmt_matches_rustfmt() {
    let sb = Sandbox::new("ws-fmt");
    let ws = workspace(&sb, "ws");
    let lib = ws.join("crates/ws-core/src/lib.rs");
    let ugly = "pub fn answer()->u32{wsdep::base()+1}\n";
    write(&lib, ugly);
    let frustfmt = env!("CARGO_BIN_EXE_frustfmt");

    let ours = sb.bin_path(frustfmt).args(["--edition", "2021", "--check"]).arg(&lib).output().unwrap();
    let theirs = Command::new("rustfmt").args(["--edition", "2021", "--check"]).arg(&lib).output().unwrap();
    assert_eq!(ours.status.code(), Some(1));
    assert_eq!(ours.status.code(), theirs.status.code());
    assert_eq!(ours.stdout, theirs.stdout);
    let v_ours = sb.bin_path(frustfmt).arg("--version").output().unwrap();
    let v_theirs = Command::new("rustfmt").arg("--version").output().unwrap();
    assert_eq!(v_ours.stdout, v_theirs.stdout);

    let check = fcargo(&sb, &ws, &["fmt", "--all", "--check"]);
    assert_eq!(check.status.code(), Some(1), "unformatted code fails the check");
    assert_success(&fcargo(&sb, &ws, &["fmt", "--all"]));
    assert_success(&fcargo(&sb, &ws, &["fmt", "--all", "--check"]));
    assert_eq!(fs::read_to_string(&lib).unwrap(), "pub fn answer() -> u32 {\n    wsdep::base() + 1\n}\n");

    let main = ws.join("crates/ws-app/examples/demo.rs");
    write(&main, "fn main(){println!(\"demo\");}\n");
    assert_success(&sb.bin_path(frustfmt).args(["--edition", "2021"]).arg(&main).output().unwrap());
    assert_eq!(fs::read_to_string(&main).unwrap(), "fn main() {\n    println!(\"demo\");\n}\n");
    assert_tree_clean(&ws);
}
