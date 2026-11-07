//! A detached process must not keep the spawner's inheritable handles (cargo's stderr pipe) open, or cargo never sees the unit finish.
#![cfg(windows)]

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

const ROLE: &str = "FRUST_DETACHED_SPAWN_ROLE";
const SLEEP: Duration = Duration::from_secs(20);
const EOF_WITHIN: Duration = Duration::from_secs(8);

fn test_binary(role: &str, test: &str) -> Command {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["--exact", test, "--nocapture", "--test-threads=1"]).env(ROLE, role);
    cmd
}

/// Runs only as the spawned child: starts the sleeper through the function under test, then exits.
#[test]
fn spawner_role() {
    if std::env::var(ROLE).as_deref() != Ok("spawner") {
        return;
    }
    let mut sleeper = test_binary("sleeper", "sleeper_role");
    frust::sys::spawn_detached(&mut sleeper, &[]).unwrap();
}

/// Runs only as the detached process: lives long enough to expose a leaked pipe handle.
#[test]
fn sleeper_role() {
    if std::env::var(ROLE).as_deref() == Ok("sleeper") {
        std::thread::sleep(SLEEP);
    }
}

#[test]
fn a_detached_process_does_not_hold_the_spawners_stderr_pipe() {
    let mut child = test_binary("spawner", "spawner_role")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut pipe = child.stderr.take().unwrap();
    let (done, eof) = mpsc::channel();
    std::thread::spawn(move || {
        let mut sink = Vec::new();
        let _ = pipe.read_to_end(&mut sink);
        let _ = done.send(());
    });
    let status = child.wait().unwrap();
    assert!(status.success(), "the spawner failed: {status}");
    assert!(
        eof.recv_timeout(EOF_WITHIN).is_ok(),
        "the pipe stayed open after the spawner exited: a detached process inherited its handle"
    );
}
