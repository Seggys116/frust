//! Smart linking is an opt-in compile policy; executable behaviour and Cargo's cache stay valid.

mod common;

#[cfg(target_os = "macos")]
mod macos {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output};

    use super::common::{Sandbox, assert_success, settle, stderr, stdout};

    const APPLIED: &str = "frust-wrap: smart link:";

    fn source(bias: u32) -> String {
        format!(
            "#[link(name = \"CoreFoundation\", kind = \"framework\")]\n\
             extern \"C\" {{ fn CFArrayGetTypeID() -> usize; }}\n\
             fn calculate(values: &[u32]) -> u32 {{\n\
                 values.iter().map(|value| value * value).sum::<u32>() + {bias}\n\
             }}\n\
             fn main() {{\n\
                 assert!(unsafe {{ CFArrayGetTypeID() }} > 0);\n\
                 println!(\"{{}}\", calculate(&[1, 2, 3]));\n\
             }}\n"
        )
    }

    fn edit(project: &Path, bias: u32) {
        let main = project.join("src/main.rs");
        fs::write(&main, source(bias)).unwrap();
        settle(&main);
    }

    fn cargo(sb: &Sandbox, project: &Path) -> Command {
        let mut command = sb.bin("fcargo");
        command
            .current_dir(project)
            .env("FRUST_LINK", "smart")
            .env("FRUST_CACHE", "on")
            .env("FRUST_DEBUG", "1")
            .env("CARGO_INCREMENTAL", "1");
        command
    }

    fn build(sb: &Sandbox, project: &Path) -> Output {
        let out = cargo(sb, project).arg("build").output().unwrap();
        assert_success(&out);
        out
    }

    fn assert_applied(out: &Output, expected: bool) {
        assert_eq!(stderr(out).contains(APPLIED), expected, "{}", stderr(out));
    }

    fn assert_program(target: &Path, profile: &str, expected: &str) {
        // Direct execution must not rely on Cargo's loader environment.
        let binary = target.join(profile).join("app");
        let mut command = Command::new(&binary);
        for (name, _) in std::env::vars_os() {
            let text = name.to_string_lossy();
            if text.starts_with("DYLD_") || text.starts_with("LD_") {
                command.env_remove(name);
            }
        }
        let out = command.output().unwrap();
        assert_success(&out);
        assert_eq!(stdout(&out).trim(), expected);

        let dependencies = Command::new("/usr/bin/otool")
            .arg("-L")
            .arg(&binary)
            .output()
            .unwrap();
        assert_success(&dependencies);
        let linked = stdout(&dependencies);
        assert!(
            linked.contains("CoreFoundation.framework/"),
            "framework call must be linked: {linked}"
        );
        assert!(
            !linked.contains("libstd-"),
            "Rust's standard library must remain static: {linked}"
        );
    }

    // Session identity must survive edits even when the link backend changes.
    fn sessions(target: &Path) -> Vec<PathBuf> {
        let mut sessions: Vec<_> = fs::read_dir(target.join("debug/incremental"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("app-")
            })
            .collect();
        sessions.sort();
        sessions
    }

    // External wrappers keep control of the compiler invocation.
    fn script(path: &Path, body: &str) {
        fs::write(path, body).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn cache_disabled_smart_link_builds_frameworks_and_observes_body_edits_across_mode_changes() {
        let sb = Sandbox::new("smart-off-cache");
        let project = sb.project("app", &[], &source(7));
        let target = sb.central_target(&project);

        let first = cargo(&sb, &project)
            .arg("build")
            .env("FRUST_CACHE", "off")
            .output()
            .unwrap();
        assert_success(&first);
        assert_applied(&first, true);
        assert_program(&target, "debug", "21");
        assert!(
            !sessions(&target).is_empty(),
            "the fixture must exercise an incremental dev compile"
        );

        edit(&project, 8);
        let plain = cargo(&sb, &project)
            .arg("build")
            .env("FRUST_CACHE", "off")
            .env("FRUST_LINK", "off")
            .output()
            .unwrap();
        assert_success(&plain);
        assert_applied(&plain, false);
        assert_program(&target, "debug", "22");

        edit(&project, 9);
        let smart = cargo(&sb, &project)
            .arg("build")
            .env("FRUST_CACHE", "off")
            .output()
            .unwrap();
        assert_success(&smart);
        assert_applied(&smart, true);
        assert_program(&target, "debug", "23");
        assert_eq!(
            sb.central_target(&project),
            target,
            "link policy must not create another target namespace"
        );
        assert!(
            sb.cache_events().is_empty(),
            "smart linking must also work without the compile cache"
        );
    }

    #[test]
    fn smart_cache_entries_are_distinct_and_clean_restores_allow_subsequent_incremental_edits() {
        let sb = Sandbox::new("smart-cache");
        let project = sb.project("app", &[], &source(7));
        let target = sb.central_target(&project);
        let plain = cargo(&sb, &project)
            .arg("build")
            .env("FRUST_LINK", "off")
            .output()
            .unwrap();
        assert_success(&plain);
        assert_applied(&plain, false);
        assert_eq!(sb.count_cache("miss", "app"), 1);

        assert_success(&cargo(&sb, &project).arg("clean").output().unwrap());
        assert_applied(&build(&sb, &project), true);
        assert_eq!(
            sb.count_cache("hit", "app"),
            0,
            "a plain-link entry must not satisfy smart-link flags"
        );
        assert_eq!(sb.count_cache("miss", "app"), 2);
        assert_program(&target, "debug", "21");

        assert_success(&cargo(&sb, &project).arg("clean").output().unwrap());
        build(&sb, &project);
        assert_eq!(
            sb.count_cache("hit", "app"),
            1,
            "the smart-linked executable must restore after clean"
        );
        assert_program(&target, "debug", "21");

        edit(&project, 8);
        assert_applied(&build(&sb, &project), true);
        assert_program(&target, "debug", "22");
        let previous = sessions(&target);
        assert!(
            !previous.is_empty(),
            "a cache miss must leave rustc's incremental session available"
        );
        edit(&project, 9);
        assert_applied(&build(&sb, &project), true);
        assert_program(&target, "debug", "23");
        assert_eq!(
            sessions(&target),
            previous,
            "the next edit must retain the same incremental unit directory"
        );
        assert_eq!(sb.count_cache("miss", "app"), 4);
        assert_eq!(sb.count_cache("hit", "app"), 1);
    }

    #[test]
    fn default_release_and_explicit_linker_options_keep_the_normal_link_path() {
        let sb = Sandbox::new("smart-guards");
        let project = sb.project("app", &[], &source(7));
        let target = sb.central_target(&project);
        let default = cargo(&sb, &project)
            .arg("build")
            .env_remove("FRUST_LINK")
            .output()
            .unwrap();
        assert_success(&default);
        assert_applied(&default, false);
        assert_program(&target, "debug", "21");

        let release = cargo(&sb, &project)
            .args(["build", "--release"])
            .env("CARGO_INCREMENTAL", "0")
            .output()
            .unwrap();
        assert_success(&release);
        assert_applied(&release, false);
        assert_program(&target, "release", "21");

        let linker = sb.root.join("custom-cc");
        let log = sb.root.join("custom-cc.log");
        script(
            &linker,
            "#!/bin/sh\nprintf '%s\\n' used >> \"$FRUST_TEST_LINKER_LOG\"\nexec /usr/bin/cc \"$@\"\n",
        );
        let custom = cargo(&sb, &project)
            .arg("build")
            .env("RUSTFLAGS", format!("-Clinker={}", linker.display()))
            .env("FRUST_TEST_LINKER_LOG", &log)
            .output()
            .unwrap();
        assert_success(&custom);
        assert_applied(&custom, false);
        assert!(
            fs::read_to_string(log)
                .unwrap()
                .lines()
                .any(|line| line == "used"),
            "the selected custom linker must run"
        );
        assert_program(&target, "debug", "21");

        let link_arg = cargo(&sb, &project)
            .arg("build")
            .env("RUSTFLAGS", "-Clink-arg=-Wl,-dead_strip")
            .output()
            .unwrap();
        assert_success(&link_arg);
        assert_applied(&link_arg, false);
        assert_program(&target, "debug", "21");
    }

    #[test]
    fn smart_mode_respects_an_existing_rustc_wrapper_when_cache_is_disabled() {
        let sb = Sandbox::new("smart-foreign-wrapper");
        let project = sb.project("app", &[], &source(7));
        let wrapper = sb.root.join("foreign-wrapper");
        let log = sb.root.join("foreign-wrapper.log");
        script(
            &wrapper,
            "#!/bin/sh\nprintf '%s\\n' used >> \"$FRUST_TEST_WRAPPER_LOG\"\nexec \"$@\"\n",
        );
        let out = cargo(&sb, &project)
            .arg("build")
            .env("FRUST_CACHE", "off")
            .env("RUSTC_WRAPPER", &wrapper)
            .env("FRUST_TEST_WRAPPER_LOG", &log)
            .output()
            .unwrap();
        assert_success(&out);
        assert_applied(&out, false);
        assert!(
            fs::read_to_string(log)
                .unwrap()
                .lines()
                .any(|line| line == "used"),
            "the foreign wrapper must be invoked"
        );
        assert_program(&sb.central_target(&project), "debug", "21");
    }
}

#[cfg(not(target_os = "macos"))]
#[test]
fn smart_link_on_an_unsupported_host_preserves_normal_builds() {
    use common::{Sandbox, assert_success, run_binary, stderr, stdout};

    let sb = Sandbox::new("smart-unsupported");
    let project = sb.project(
        "app",
        &[],
        "fn square(x: u32) -> u32 { x * x }\nfn main() { println!(\"{}\", square(7)); }\n",
    );
    let out = sb
        .bin("fcargo")
        .arg("build")
        .current_dir(&project)
        .env("FRUST_LINK", "smart")
        .env("FRUST_CACHE", "off")
        .env("FRUST_DEBUG", "1")
        .env("CARGO_INCREMENTAL", "1")
        .output()
        .unwrap();
    assert_success(&out);
    assert!(
        !stderr(&out).contains("frust-wrap: smart link:"),
        "{}",
        stderr(&out)
    );
    let executable = run_binary(&sb.central_target(&project), "debug", "app");
    assert_success(&executable);
    assert_eq!(stdout(&executable).trim(), "49");
}
