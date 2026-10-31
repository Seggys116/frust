//! The exact number of units a build will report, from cargo's own unit graph.

use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::sys;

const TIMEOUT: Duration = Duration::from_secs(5);

/// Subcommands whose json stream reports one artifact per graph unit; the others keep the history estimate.
pub fn supported(sub: &str) -> bool {
    matches!(sub, "build" | "b" | "check" | "c" | "run" | "r" | "test" | "t")
}

/// Synchronous counting; build supervision uses `Pending` to overlap compilation.
pub fn count(cargo: &Path, command: &[OsString], rest: &[OsString], env: &[(OsString, OsString)]) -> Option<u32> {
    let mut pending = Pending::start(cargo, command, rest, env)?;
    loop {
        if let Some(count) = pending.poll() {
            return Some(count);
        }
        if pending.done {
            return None;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Owns optional planning inside the build lock and cancels it before lock handoff.
pub struct Pending {
    child: Child,
    stdout: sys::Pipe,
    bytes: Vec<u8>,
    started: Instant,
    eof: bool,
    done: bool,
}

impl Pending {
    pub fn start(cargo: &Path, command: &[OsString], rest: &[OsString], env: &[(OsString, OsString)]) -> Option<Self> {
        let mut cmd = Command::new(cargo);
        cmd.args(command).args(["--unit-graph", "-Z", "unstable-options"]).args(rest);
        for (k, v) in env {
            cmd.env(k, v);
        }
        // Bootstrap applies only to this planning invocation.
        cmd.env("RUSTC_BOOTSTRAP", "1").env("CARGO_TERM_PROGRESS_WHEN", "never");
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
        let mut child = cmd.spawn().ok()?;
        let stdout = child.stdout.take()?.into();
        Some(Self { child, stdout, bytes: Vec::new(), started: Instant::now(), eof: false, done: false })
    }

    /// Drain a bounded amount per UI tick; large graphs cannot starve cargo's output.
    pub fn poll(&mut self) -> Option<u32> {
        if self.done {
            return None;
        }
        if self.started.elapsed() >= TIMEOUT {
            self.cancel();
            return None;
        }
        let mut chunk = [0; 64 * 1024];
        for _ in 0..4 {
            match self.stdout.read(&mut chunk) {
                Ok(0) => { self.eof = true; break; }
                Ok(n) => self.bytes.extend_from_slice(&chunk[..n]),
                Err(err) if matches!(err.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => break,
                Err(_) => { self.cancel(); return None; }
            }
            if self.bytes.len() > 64 * 1024 * 1024 {
                self.cancel();
                return None;
            }
        }
        if self.eof {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    self.done = true;
                    return status.success().then(|| count_units(&self.bytes)).flatten();
                }
                Ok(None) => {}
                Err(_) => self.cancel(),
            }
        }
        None
    }

    fn cancel(&mut self) {
        if !self.done {
            let _ = self.child.kill();
            let _ = self.child.wait();
            self.done = true;
        }
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn count_units(graph: &[u8]) -> Option<u32> {
    let graph: Value = serde_json::from_slice(graph).ok()?;
    let units = graph.get("units")?.as_array()?;
    let n = units.iter().filter(|u| u.get("mode").and_then(Value::as_str) != Some("run-custom-build")).count();
    u32::try_from(n).ok().filter(|&n| n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn planning_is_polled_and_reaped_without_blocking_the_build() {
        let args = [OsString::from("-c"), OsString::from("printf '%s' '{\"units\":[{\"mode\":\"build\"}]}'")];
        let mut pending = Pending::start(Path::new("/bin/sh"), &args, &[], &[]).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(count) = pending.poll() {
                assert_eq!(count, 1);
                assert!(pending.child.try_wait().unwrap().is_some());
                break;
            }
            assert!(Instant::now() < deadline, "planner did not finish");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[cfg(unix)]
    #[test]
    fn unfinished_planning_is_cancelled_and_reaped() {
        let args = [OsString::from("-c"), OsString::from("exec sleep 30")];
        let mut pending = Pending::start(Path::new("/bin/sh"), &args, &[], &[]).unwrap();
        assert_eq!(pending.poll(), None);
        pending.cancel();
        assert!(pending.child.try_wait().unwrap().is_some());
        assert_eq!(pending.poll(), None);
    }

    #[test]
    fn build_script_runs_are_not_counted() {
        let graph = br#"{"version":1,"units":[{"mode":"build"},{"mode":"build"},{"mode":"run-custom-build"},{"mode":"check"}],"roots":[0]}"#;
        assert_eq!(count_units(graph), Some(3));
    }

    #[test]
    fn nothing_usable_yields_no_estimate() {
        assert_eq!(count_units(b"not json"), None);
        assert_eq!(count_units(br#"{"units":[]}"#), None);
        assert_eq!(count_units(br#"{"units":[{"mode":"run-custom-build"}]}"#), None);
        assert_eq!(count_units(br#"{"nothing":1}"#), None);
    }

    #[test]
    fn only_json_reporting_subcommands_are_planned() {
        for sub in ["build", "b", "check", "c", "run", "test"] {
            assert!(supported(sub), "{sub}");
        }
        for sub in ["clippy", "doc", "bench", "fmt", "update"] {
            assert!(!supported(sub), "{sub}");
        }
    }
}
