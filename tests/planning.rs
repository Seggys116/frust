//! Quiet builds launch one planner, and a completed build never waits for a slow planner.

#![cfg(unix)]

mod common;

use std::fs::{self, File};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use common::Sandbox;

static SERIAL: Mutex<()> = Mutex::new(());

const CARGO: &str = r#"#!/bin/sh
case "$1" in
  -V) printf 'cargo 9.9.9 (planning test)\n'; exit 0 ;;
  locate-project) printf '%s/Cargo.toml\n' "$PWD"; exit 0 ;;
esac
case " $* " in
  *" --unit-graph "*)
    printf '%s\n' "$$" > "$FRUST_TEST_PLANNER_PID"
    printf 'requested\n' >> "$FRUST_TEST_PLANNER_REQUESTS"
    if [ "$FRUST_TEST_MODE" = slow-planner ]; then exec sleep 30; fi
    printf '{"units":[{"mode":"build"}]}\n'
    exit 0 ;;
esac
while [ ! -f "$FRUST_TEST_PLANNER_REQUESTS" ]; do sleep 0.01; done
printf '{"reason":"build-finished","success":true}\n'
"#;

struct Scene {
    sb: Sandbox,
    project: PathBuf,
    cargo: PathBuf,
    requests: PathBuf,
    planner_pid: PathBuf,
}

impl Scene {
    fn new(tag: &str) -> Self {
        let sb = Sandbox::new(tag);
        let project = sb.project("app", &[], "fn main() {}\n");
        let cargo = common::write_fake_tool(&sb.root.join("cargo"), CARGO, "");
        let requests = sb.root.join("planner-requests");
        let planner_pid = sb.root.join("planner-pid");
        Self { sb, project, cargo, requests, planner_pid }
    }

    fn run(&self, mode: &str) {
        let _serial = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
        let stderr = self.sb.root.join("stderr");
        let mut cmd = self.sb.bin("fcargo");
        cmd.arg("build")
            .current_dir(&self.project)
            .env("FRUST_CARGO", &self.cargo)
            .env("FRUST_RENDER", "plain")
            .env("FRUST_TEST_MODE", mode)
            .env("FRUST_TEST_PLANNER_REQUESTS", &self.requests)
            .env("FRUST_TEST_PLANNER_PID", &self.planner_pid)
            .stdout(Stdio::null())
            .stderr(File::create(&stderr).unwrap());
        let mut child = common::own_group(&mut cmd).spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "fcargo failed: {}", fs::read_to_string(&stderr).unwrap());
                return;
            }
            if Instant::now() >= deadline {
                common::kill_tree(child.id());
                let _ = child.wait();
                panic!("fcargo did not finish; planner must not delay Cargo: {}", fs::read_to_string(&stderr).unwrap());
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}

#[test]
fn quiet_build_still_requests_a_unit_graph_once() {
    let scene = Scene::new("planning-quiet");
    // The fake build emits nothing and exits only once planning has started.
    scene.run("quiet");
    assert_eq!(fs::read_to_string(&scene.requests).unwrap(), "requested\n");
}

#[test]
fn finished_build_cancels_and_reaps_a_slow_planner() {
    let scene = Scene::new("planning-cancel");
    // Cargo exits once the planner starts, while the planner itself sleeps for thirty seconds.
    scene.run("slow-planner");
    assert_eq!(fs::read_to_string(&scene.requests).unwrap(), "requested\n");
    let pid: u32 = fs::read_to_string(&scene.planner_pid).unwrap().trim().parse().unwrap();
    assert!(!frust::sys::pid_alive(pid), "the planner must be reaped before fcargo exits");
}
