//! A key-lock waiter lends Cargo's implicit slot so an independent unit can finish under -j1.

#![cfg(unix)]

mod common;

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use common::{Sandbox, assert_success, run_binary, stdout};

const DEADLINE: Duration = Duration::from_secs(60);

struct Build {
    child: Option<Child>,
    stderr: PathBuf,
}

struct GatedBuilds {
    compiler: PathBuf,
    real_compiler: PathBuf,
    events: PathBuf,
    gate: PathBuf,
    observed: PathBuf,
    home: PathBuf,
    builds: Vec<Build>,
}

impl GatedBuilds {
    fn new(sb: &Sandbox) -> Self {
        let compiler = common::write_fake_tool(
            &sb.root.join("cooperative-rustc"),
            r#"#!/bin/sh
crate=
emit=
previous=
for arg in "$@"; do
    if [ "$previous" = '--crate-name' ]; then crate=$arg; fi
    case "$arg" in
        --crate-name=*) crate=${arg#--crate-name=} ;;
        --emit|--emit=*) emit=yes ;;
    esac
    previous=$arg
done
if [ "$emit" = yes ]; then
    printf '%s\t%s\tstarted\n' "$crate" "$FRUST_TEST_ROLE" >> "$FRUST_TEST_EVENTS"
    if [ "$crate" = shared_dep ]; then
        attempts=0
        while [ ! -f "$FRUST_TEST_GATE" ]; do
            attempts=$((attempts + 1))
            if [ "$attempts" -gt 12000 ]; then exit 125; fi
            sleep 0.01
        done
    elif [ "$crate" = independent_dep ]; then
        attempts=0
        while [ ! -f "$FRUST_TEST_WAITER_OBSERVED" ]; do
            attempts=$((attempts + 1))
            if [ "$attempts" -gt 12000 ]; then exit 125; fi
            sleep 0.01
        done
        "$FRUST_TEST_REAL_RUSTC" "$@"
        result=$?
        phase=closed
        if [ -f "$FRUST_TEST_GATE" ]; then phase=open; fi
        printf '%s\t%s\tfinished-%s-%s\n' "$crate" "$FRUST_TEST_ROLE" "$phase" "$result" >> "$FRUST_TEST_EVENTS"
        exit "$result"
    fi
fi
exec "$FRUST_TEST_REAL_RUSTC" "$@"
"#,
            "",
        );
        Self {
            compiler,
            real_compiler: frust::toolchain::resolve_tool("rustc").expect("the real compiler"),
            events: sb.root.join("compiler-events"),
            gate: sb.root.join("release-shared-compiler"),
            observed: sb.root.join("shared-waiter-observed"),
            home: sb.home(),
            builds: Vec::new(),
        }
    }

    fn start(&mut self, sb: &Sandbox, project: &Path, role: &str, package: &str) -> usize {
        let stderr = sb.root.join(format!("{role}-stderr"));
        let mut command = sb.bin("fcargo");
        command
            .args([
                "build",
                "--offline",
                "-j1",
                "--profile",
                role,
                "-p",
                package,
            ])
            .current_dir(project)
            .env("CARGO_TARGET_DIR", sb.root.join(format!("target-{role}")))
            .env("RUSTC", &self.compiler)
            .env("FRUST_DEBUG", "1")
            .env("FRUST_AUTO_COMPACT", "off")
            .env("FRUST_KEYLOCK_WAIT_SECS", "120")
            .env_remove("FRUST_STORE_SYNC")
            .env("FRUST_TEST_ROLE", role)
            .env("FRUST_TEST_REAL_RUSTC", &self.real_compiler)
            .env("FRUST_TEST_EVENTS", &self.events)
            .env("FRUST_TEST_GATE", &self.gate)
            .env("FRUST_TEST_WAITER_OBSERVED", &self.observed)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(File::create(&stderr).unwrap());
        common::own_group(&mut command);
        self.builds.push(Build {
            child: Some(command.spawn().unwrap()),
            stderr,
        });
        self.builds.len() - 1
    }

    fn events(&self) -> String {
        fs::read_to_string(&self.events).unwrap_or_default()
    }

    #[cfg(target_os = "linux")]
    fn start_inherited(
        &mut self,
        sb: &Sandbox,
        project: &Path,
        client: &jobserver::Client,
        keep_going: bool,
    ) -> (usize, PathBuf) {
        let pid_file = sb.root.join("waiting-wrapper-pid");
        let wrapper = sb.root.join("record-waiting-wrapper");
        let source = sb.root.join("record-waiting-wrapper.rs");
        // Native exec preserves Cargo's non-shell environment names while retaining the wrapper PID.
        fs::write(
            &source,
            r#"use std::{env, ffi::OsString, fs, os::unix::process::CommandExt, process::Command};
fn main() {
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    let shared = args.windows(2).any(|pair| pair[0] == "--crate-name" && pair[1] == "shared_dep")
        || args.iter().any(|arg| arg == "--crate-name=shared_dep");
    let emit = args.iter().any(|arg| arg == "--emit" || arg.to_string_lossy().starts_with("--emit="));
    if shared && emit {
        fs::write(env::var_os("FRUST_TEST_WRAPPER_PID").unwrap(), format!("{}\n", std::process::id())).unwrap();
    }
    let error = Command::new(env::var_os("FRUST_TEST_REAL_WRAPPER").unwrap()).args(args).exec();
    panic!("cannot exec production wrapper: {error}");
}
"#,
        )
        .unwrap();
        let compiler_stderr = sb.root.join("record-waiting-wrapper-stderr");
        let mut compiler = std::process::Command::new(&self.real_compiler);
        compiler
            .args(["--crate-name", "record_waiting_wrapper", "--edition=2021"])
            .arg(&source)
            .arg("-o")
            .arg(&wrapper)
            .current_dir(project)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(File::create(&compiler_stderr).unwrap());
        common::own_group(&mut compiler);
        self.builds.push(Build {
            child: Some(compiler.spawn().unwrap()),
            stderr: compiler_stderr,
        });
        self.finish(self.builds.len() - 1);
        let stderr = sb.root.join("inherited-stderr");
        let mut command = sb.cargo();
        command
            .args([
                "build",
                "--offline",
                "--profile",
                "follower",
                "-p",
                "shared_dep",
            ])
            .current_dir(project)
            .env("CARGO_TARGET_DIR", sb.root.join("target-follower"))
            .env_remove("CARGO_BUILD_JOBS")
            .env("RUSTC", &self.compiler)
            .env("RUSTC_WRAPPER", wrapper)
            .env("FRUST_JOBSERVER_SUPERVISOR", std::process::id().to_string())
            .env("FRUST_TEST_WRAPPER_PID", &pid_file)
            .env("FRUST_TEST_REAL_WRAPPER", env!("CARGO_BIN_EXE_frust-wrap"))
            .env("FRUST_DEBUG", "1")
            .env("FRUST_AUTO_COMPACT", "off")
            .env("FRUST_KEYLOCK_WAIT_SECS", "120")
            .env_remove("FRUST_STORE_SYNC")
            .env("FRUST_TEST_ROLE", "follower")
            .env("FRUST_TEST_REAL_RUSTC", &self.real_compiler)
            .env("FRUST_TEST_EVENTS", &self.events)
            .env("FRUST_TEST_GATE", &self.gate)
            .env("FRUST_TEST_WAITER_OBSERVED", &self.observed)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(File::create(&stderr).unwrap());
        if keep_going {
            command.arg("--keep-going");
        }
        client.configure(&mut command);
        common::own_group(&mut command);
        self.builds.push(Build {
            child: Some(command.spawn().unwrap()),
            stderr,
        });
        (self.builds.len() - 1, pid_file)
    }

    fn stderr(&self, build: usize) -> String {
        fs::read_to_string(&self.builds[build].stderr).unwrap_or_default()
    }

    fn until(&self, what: &str, mut ready: impl FnMut() -> bool) {
        let started = Instant::now();
        while !ready() {
            assert!(
                started.elapsed() < DEADLINE,
                "waiting for {what}\n{}\n{}",
                self.events(),
                self.builds
                    .iter()
                    .map(|build| fs::read_to_string(&build.stderr).unwrap_or_default())
                    .collect::<Vec<_>>()
                    .join("\n")
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn release(&self) {
        fs::write(&self.gate, b"released").unwrap();
    }

    fn finish(&mut self, build: usize) -> String {
        let started = Instant::now();
        loop {
            if let Some(status) = self.builds[build]
                .child
                .as_mut()
                .unwrap()
                .try_wait()
                .unwrap()
            {
                let mut child = self.builds[build].child.take().unwrap();
                if !status.success() {
                    common::kill_tree(child.id());
                }
                child.wait().unwrap();
                let stderr = self.stderr(build);
                assert!(status.success(), "{stderr}\n{}", self.events());
                return stderr;
            }
            assert!(
                started.elapsed() < DEADLINE,
                "build did not finish\n{}",
                self.stderr(build)
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for GatedBuilds {
    fn drop(&mut self) {
        // Open both gates before terminating process groups so a panic cannot strand gated compilers.
        let _ = fs::write(&self.gate, b"released");
        let _ = fs::write(&self.observed, b"released");
        for build in &mut self.builds {
            if let Some(mut child) = build.child.take() {
                common::kill_tree(child.id());
                let _ = child.wait();
            }
        }
        let _ = frust::bench::drain_deferred_stores(&self.home, DEADLINE);
    }
}

fn package(root: &Path, name: &str, dependencies: &str, source: &str, binary: bool) {
    let directory = root.join(name);
    fs::create_dir_all(directory.join("src")).unwrap();
    fs::write(
        directory.join("Cargo.toml"),
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\n{dependencies}"),
    )
    .unwrap();
    let source_path = directory.join(if binary { "src/main.rs" } else { "src/lib.rs" });
    fs::write(&source_path, source).unwrap();
    common::settle(&source_path);
}

#[test]
fn cargo_j1_compiles_independent_unit_while_shared_unit_waits_for_another_build() {
    let sb = Sandbox::new("cooperative-j1");
    let project = sb.root.join("workspace");
    fs::create_dir(&project).unwrap();
    fs::write(project.join("Cargo.toml"), "[workspace]\nmembers = [\"app\", \"shared_dep\", \"independent_dep\", \"relay0\", \"relay1\", \"relay2\", \"relay3\", \"relay4\", \"relay5\", \"relay6\", \"relay7\"]\nresolver = \"2\"\n\
        [profile.owner]\ninherits = \"dev\"\ndebug = 0\nincremental = false\n\
        [profile.follower]\ninherits = \"dev\"\ndebug = 0\nincremental = false\n").unwrap();
    package(
        &project,
        "shared_dep",
        "",
        "pub fn value() -> u32 { 7 }\n",
        false,
    );
    package(
        &project,
        "independent_dep",
        "",
        "pub fn value() -> u32 { 11 }\n",
        false,
    );
    // Eight relay reverse dependencies give shared_dep strict priority over the independent leaf.
    let mut dependencies = "independent_dep = { path = \"../independent_dep\" }\n".to_string();
    let mut values = "independent_dep::value()".to_string();
    for index in 0..8 {
        let name = format!("relay{index}");
        package(
            &project,
            &name,
            "shared_dep = { path = \"../shared_dep\" }\n",
            &format!("pub fn value() -> u32 {{ shared_dep::value() + {index} }}\n"),
            false,
        );
        dependencies.push_str(&format!("{name} = {{ path = \"../{name}\" }}\n"));
        values.push_str(&format!(" + {name}::value()"));
    }
    package(
        &project,
        "app",
        &dependencies,
        &format!("fn main() {{ println!(\"{{}}\", {values}); }}\n"),
        true,
    );
    let mut builds = GatedBuilds::new(&sb);
    let owner = builds.start(&sb, &project, "owner", "shared_dep");
    builds.until(
        "the owner to enter the counted compiler under its key lock",
        || {
            builds
                .events()
                .lines()
                .any(|line| line == "shared_dep\towner\tstarted")
        },
    );
    let follower = builds.start(&sb, &project, "follower", "app");
    builds.until("explicit shared-unit key contention", || {
        builds
            .stderr(follower)
            .contains("waiting for key lock for shared_dep")
    });
    // The independent compiler proceeds only after actual key contention was observed, never a startup delay.
    fs::write(&builds.observed, b"observed shared waiter").unwrap();
    builds.until(
        "independent rustc completion with the owner gate still closed",
        || {
            builds
                .events()
                .lines()
                .any(|line| line == "independent_dep\tfollower\tfinished-closed-0")
        },
    );
    assert!(!builds.gate.exists());
    assert_eq!(
        builds
            .events()
            .lines()
            .filter(|line| line.starts_with("shared_dep\t"))
            .count(),
        1
    );
    builds.release();
    let owner_stderr = builds.finish(owner);
    let follower_stderr = builds.finish(follower);
    assert!(
        owner_stderr
            .lines()
            .any(|line| line.contains("compiled shared_dep ") && line.contains("store deferred")),
        "{owner_stderr}"
    );
    assert_eq!(
        builds
            .events()
            .lines()
            .filter(|line| line.starts_with("shared_dep\t"))
            .count(),
        1,
        "{follower_stderr}"
    );
    assert_eq!(sb.count_cache("miss", "shared_dep"), 1);
    assert_eq!(sb.count_cache("hit", "shared_dep"), 1);
    let hit = sb
        .cache_events()
        .into_iter()
        .find(|event| event["crate"] == "shared_dep" && event["result"] == "hit")
        .unwrap();
    assert!(hit["waited_ms"].as_u64().unwrap() > 0, "{hit}");
    let output = run_binary(&sb.root.join("target-follower"), "follower", "app");
    assert_success(&output);
    assert_eq!(stdout(&output), "95\n");
    frust::bench::drain_deferred_stores(&sb.home(), DEADLINE).unwrap();
}

#[cfg(target_os = "linux")]
fn killed_cargo_waiter_repays_before_cargo_finishes(signal: i32, keep_going: bool) {
    let sb = Sandbox::new("cooperative-killed-waiter");
    let project = sb.root.join("workspace");
    fs::create_dir(&project).unwrap();
    fs::write(
        project.join("Cargo.toml"),
        "[workspace]\nmembers = [\"shared_dep\"]\nresolver = \"2\"\n\
         [profile.owner]\ninherits = \"dev\"\ndebug = 0\nincremental = false\n\
         [profile.follower]\ninherits = \"dev\"\ndebug = 0\nincremental = false\n",
    )
    .unwrap();
    package(
        &project,
        "shared_dep",
        "",
        "pub fn value() -> u32 { 7 }\n",
        false,
    );
    let client = jobserver::Client::new(1).unwrap();
    // The parent retains Cargo's assigned slot until Cargo passes its completion barrier.
    let assigned = client.acquire().unwrap();
    let mut builds = GatedBuilds::new(&sb);
    let owner = builds.start(&sb, &project, "owner", "shared_dep");
    builds.until("the owner's counted compiler", || {
        builds.events().contains("shared_dep\towner\tstarted")
    });
    let (follower, pid_file) = builds.start_inherited(&sb, &project, &client, keep_going);
    builds.until(
        "the production wrapper's key contention and guarded slot loan",
        || {
            builds
                .stderr(follower)
                .contains("waiting for key lock for shared_dep")
                && client.available().unwrap() == 1
        },
    );
    // Retaining the loan forces the guardian to delay Cargo's failure until repayment.
    let borrowed = client.acquire().unwrap();
    let pid: i32 = fs::read_to_string(pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(
        fs::read_link(format!("/proc/{pid}/exe")).unwrap(),
        fs::canonicalize(env!("CARGO_BIN_EXE_frust-wrap")).unwrap(),
        "signal only the recorded production wrapper"
    );
    // SAFETY: this PID belongs to the recorded wrapper in this test's Cargo process group.
    assert_eq!(unsafe { libc::kill(pid, signal) }, 0);
    let held_until = Instant::now() + Duration::from_millis(500);
    while Instant::now() < held_until {
        assert!(
            builds.builds[follower]
                .child
                .as_mut()
                .unwrap()
                .try_wait()
                .unwrap()
                .is_none(),
            "Cargo finished before the borrowed token was repaid: {}",
            builds.stderr(follower)
        );
        assert_eq!(
            client.available().unwrap(),
            0,
            "the killed wrapper must not duplicate its loan"
        );
        assert!(
            !builds.stderr(follower).contains("could not compile"),
            "Cargo reported failure before its completion barrier"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(borrowed);
    let started = Instant::now();
    let status = loop {
        if let Some(status) = builds.builds[follower]
            .child
            .as_mut()
            .unwrap()
            .try_wait()
            .unwrap()
        {
            break status;
        }
        assert!(
            started.elapsed() < DEADLINE,
            "Cargo did not finish after repayment: {}",
            builds.stderr(follower)
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    builds.builds[follower]
        .child
        .take()
        .unwrap()
        .wait()
        .unwrap();
    assert!(
        !status.success(),
        "a killed compiler wrapper must fail its Cargo build"
    );
    assert!(
        builds.stderr(follower).contains("could not compile"),
        "{}",
        builds.stderr(follower)
    );
    assert_eq!(
        client.available().unwrap(),
        0,
        "Cargo's externally assigned slot is still owned"
    );
    drop(assigned);
    assert_eq!(
        client.available().unwrap(),
        1,
        "termination must preserve inherited pool capacity"
    );
    builds.release();
    builds.finish(owner);
}

#[cfg(target_os = "linux")]
#[test]
fn terminated_key_waiter_repays_inherited_cargo_slot_before_failure() {
    killed_cargo_waiter_repays_before_cargo_finishes(libc::SIGTERM, false);
}

#[cfg(target_os = "linux")]
#[test]
fn killed_key_waiter_repays_inherited_cargo_slot_before_failure() {
    killed_cargo_waiter_repays_before_cargo_finishes(libc::SIGKILL, false);
}

#[cfg(target_os = "linux")]
#[test]
fn keep_going_key_waiter_repays_inherited_cargo_slot_before_failure() {
    for signal in [libc::SIGTERM, libc::SIGKILL] {
        killed_cargo_waiter_repays_before_cargo_finishes(signal, true);
    }
}
