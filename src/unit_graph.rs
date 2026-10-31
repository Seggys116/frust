//! The exact number of units a build will report, from cargo's own unit graph.

use std::ffi::OsString;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

const TIMEOUT: Duration = Duration::from_secs(5);

/// Subcommands whose json stream reports one artifact per graph unit; the others keep the history estimate.
pub fn supported(sub: &str) -> bool {
    matches!(sub, "build" | "b" | "check" | "c" | "run" | "r" | "test" | "t")
}

/// Counts the units that emit an artifact; build-script runs report no artifact of their own.
pub fn count(cargo: &Path, command: &[OsString], rest: &[OsString], env: &[(OsString, OsString)]) -> Option<u32> {
    let mut cmd = Command::new(cargo);
    cmd.args(command).args(["--unit-graph", "-Z", "unstable-options"]).args(rest);
    for (k, v) in env {
        cmd.env(k, v);
    }
    // The flag is unstable, and the bootstrap switch applies to this one planning run only.
    cmd.env("RUSTC_BOOTSTRAP", "1").env("CARGO_TERM_PROGRESS_WHEN", "never");
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = cmd.spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).map(|_| bytes)
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < TIMEOUT => thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let bytes = reader.join().ok()?.ok()?;
    if !status.success() {
        return None;
    }
    count_units(&bytes)
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
