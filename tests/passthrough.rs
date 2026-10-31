//! frust must behave like cargo: same arguments, same output, same exit codes.

mod common;

use common::{Sandbox, assert_success, stderr, stdout};

#[test]
fn version_matches_cargo() {
    let sb = Sandbox::new("version");
    let ours = sb.bin("fcargo").arg("--version").output().unwrap();
    let theirs = sb.cargo().arg("--version").output().unwrap();
    assert_success(&ours);
    assert_eq!(stdout(&ours), stdout(&theirs));
}

#[test]
fn unknown_subcommand_fails_like_cargo() {
    let sb = Sandbox::new("unknown");
    let ours = sb.bin("fcargo").arg("definitely-not-a-subcommand").output().unwrap();
    let theirs = sb.cargo().arg("definitely-not-a-subcommand").output().unwrap();
    assert_eq!(ours.status.code(), theirs.status.code());
    assert!(!ours.status.success());
}

#[test]
fn compile_errors_propagate_cargos_exit_code_and_diagnostics() {
    let sb = Sandbox::new("errors");
    let p = sb.project("bad", &[], "fn main() { let x: u32 = \"no\"; }\n");
    let ours = sb.bin("fcargo").arg("build").current_dir(&p).output().unwrap();
    let theirs = sb.cargo().arg("build").env("CARGO_TARGET_DIR", sb.root.join("plain-target")).current_dir(&p).output().unwrap();
    assert_eq!(ours.status.code(), theirs.status.code());
    assert!(stderr(&ours).contains("mismatched types"));
}

#[test]
fn run_forwards_program_output_arguments_and_exit_code() {
    let sb = Sandbox::new("run");
    let p = sb.project(
        "prog",
        &[],
        "fn main() { let args: Vec<String> = std::env::args().skip(1).collect(); println!(\"args={}\", args.join(\",\")); std::process::exit(7); }\n",
    );
    let out = sb.bin("fcargo").args(["run", "--quiet", "--", "alpha", "--beta"]).current_dir(&p).output().unwrap();
    assert_eq!(out.status.code(), Some(7));
    assert_eq!(stdout(&out).trim(), "args=alpha,--beta");
}

#[test]
fn test_subcommand_builds_then_runs_tests() {
    let sb = Sandbox::new("test");
    let p = sb.project("t", &[], "fn main() {}\n#[cfg(test)] mod t { #[test] fn passes() { assert_eq!(2 + 2, 4); } }\n");
    let out = sb.bin("fcargo").arg("test").current_dir(&p).output().unwrap();
    assert_success(&out);
    assert!(stdout(&out).contains("test result: ok. 1 passed"), "{}", stdout(&out));
}

#[test]
fn central_target_directory_keeps_the_project_clean() {
    let sb = Sandbox::new("central");
    let p = sb.project("clean", &[], "fn main() {}\n");
    assert_success(&sb.bin("fcargo").arg("build").current_dir(&p).output().unwrap());
    assert!(!p.join("target").exists(), "no target directory should appear in the project");
    assert!(sb.central_target(&p).join("debug").is_dir());
    assert!(sb.central_target(&p).starts_with(sb.home()));
}

#[test]
fn local_target_mode_leaves_cargo_alone() {
    let sb = Sandbox::new("local");
    let p = sb.project("local", &[], "fn main() {}\n");
    assert_success(&sb.bin("fcargo").arg("build").env("FRUST_TARGET", "local").current_dir(&p).output().unwrap());
    assert!(p.join("target").join("debug").is_dir());
}

#[test]
fn explicit_target_dir_is_respected() {
    let sb = Sandbox::new("explicit");
    let p = sb.project("explicit", &[], "fn main() {}\n");
    let dir = sb.root.join("my-target");
    assert_success(&sb.bin("fcargo").args(["build", "--target-dir"]).arg(&dir).current_dir(&p).output().unwrap());
    assert!(dir.join("debug").is_dir());
    assert!(!sb.home().join("targets").exists());
}

#[test]
fn frustc_compiles_standalone_files() {
    let sb = Sandbox::new("frustc");
    let src = sb.root.join("hello.rs");
    std::fs::write(&src, "fn main() { println!(\"hi\"); }\n").unwrap();
    let exe = sb.root.join(common::exe("hello"));
    assert_success(&sb.bin("frustc").arg(&src).arg("-o").arg(&exe).output().unwrap());
    let out = std::process::Command::new(&exe).output().unwrap();
    assert_eq!(stdout(&out).trim(), "hi");
}

#[test]
fn frust_forwards_unknown_commands_to_cargo() {
    let sb = Sandbox::new("forward");
    let ours = sb.bin("frust").args(["--version"]).output().unwrap();
    assert_success(&ours);
    assert!(stdout(&ours).contains("cargo "), "cargo's version follows frust's: {}", stdout(&ours));
}
