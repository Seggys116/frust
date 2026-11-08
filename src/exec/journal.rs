//! The wrapper's record of each rustc call cargo makes while `FRUST_EXEC_JOURNAL` names a file.

use std::env;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::{Deserialize, Serialize};

pub const ENV: &str = "FRUST_EXEC_JOURNAL";

pub fn per_run(name: &str) -> bool {
    matches!(name, "CARGO_MAKEFLAGS" | "MAKEFLAGS" | "MFLAGS" | "FRUST_EVENTS" | ENV)
        || name.starts_with("CARGO_TERM_")
        || name.starts_with("FRUST_EXEC_")
        || name == crate::store_broker::CAPABILITY_ENV
        || name == crate::wrapper::SLOT_SUPERVISOR_ENV
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Record {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: String,
    /// Wall-clock nanoseconds when the wrapper started, for the "changed during the build" test.
    pub t: i64,
    pub ms: u64,
    pub code: Option<i32>,
}

pub struct Pending {
    path: PathBuf,
    started: Instant,
    record: Record,
}

pub fn begin(args: &[OsString]) -> Option<Pending> {
    let path = env::var_os(ENV).filter(|p| !p.is_empty())?;
    env::var_os("CARGO_PKG_NAME")?;
    let argv = args.iter().map(|a| a.to_str().map(str::to_string)).collect::<Option<Vec<_>>>()?;
    let name = argv.iter().position(|a| a == "--crate-name").and_then(|i| argv.get(i + 1))?;
    if name == "___" {
        return None;
    }
    let mut vars = Vec::new();
    for (key, value) in env::vars_os() {
        let (Ok(key), Ok(value)) = (key.into_string(), value.into_string()) else { return None };
        if !per_run(&key) {
            vars.push((key, value));
        }
    }
    vars.sort();
    let cwd = env::current_dir().ok()?.to_str()?.to_string();
    let t = i64::try_from(crate::cache::hashing::now_ns()).unwrap_or(i64::MAX);
    Some(Pending { path: PathBuf::from(path), started: Instant::now(), record: Record { argv, env: vars, cwd, t, ms: 0, code: None } })
}

impl Pending {
    /// One `write` of one line in append mode, so concurrent wrappers never interleave.
    pub fn finish(mut self, code: Option<i32>) {
        self.record.ms = self.started.elapsed().as_millis() as u64;
        self.record.code = code;
        let Ok(mut line) = serde_json::to_vec(&self.record) else { return };
        line.push(b'\n');
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&self.path) {
            let _ = file.write_all(&line);
        }
    }
}

pub fn read(path: &Path) -> io::Result<Vec<Record>> {
    let text = fs::read_to_string(path)?;
    Ok(text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect())
}
