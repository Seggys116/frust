//! Per-process timing records: with `FRUST_TIMINGS=<file>` set, each wrapper appends one JSON line at exit.

use std::env;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Instant;

use serde::{Deserialize, Serialize};

pub const ENV: &str = "FRUST_TIMINGS";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    #[serde(rename = "crate")]
    pub krate: String,
    pub path_unit: bool,
    pub primary: bool,
    pub incremental: bool,
    pub prekey_ms: u64,
    pub extern_files: u64,
    pub extern_bytes_hashed: u64,
    pub extern_bytes_memo: u64,
    pub native_dir_bytes_hashed: u64,
    pub lookup_ms: u64,
    pub keywait_ms: u64,
    pub restore_ms: u64,
    pub rustc_ms: u64,
    pub freeze_ms: u64,
    pub spawn_ms: u64,
    /// `hit`, `deferred`, `queued`, `inline:<reason>` or `skip:<reason>`, where `skip:linker` is an explicit linker that could not be resolved.
    pub store_mode: String,
    pub inline_store_ms: u64,
    pub total_ms: u64,
}

impl Record {
    pub fn new(krate: &str, path_unit: bool, primary: bool, incremental: bool) -> Record {
        Record {
            krate: krate.to_string(),
            path_unit,
            primary,
            incremental,
            prekey_ms: 0,
            extern_files: 0,
            extern_bytes_hashed: 0,
            extern_bytes_memo: 0,
            native_dir_bytes_hashed: 0,
            lookup_ms: 0,
            keywait_ms: 0,
            restore_ms: 0,
            rustc_ms: 0,
            freeze_ms: 0,
            spawn_ms: 0,
            store_mode: "skip:incomplete".to_string(),
            inline_store_ms: 0,
            total_ms: 0,
        }
    }

    /// One newline-terminated JSON object, so a single append is one record.
    pub fn line(&self) -> String {
        let mut line = serde_json::to_string(self).unwrap_or_default();
        line.push('\n');
        line
    }
}

pub fn ms(since: Instant) -> u64 {
    since.elapsed().as_millis() as u64
}

/// The configured file, read from the environment once per process.
fn target() -> Option<&'static Path> {
    static TARGET: OnceLock<Option<PathBuf>> = OnceLock::new();
    TARGET
        .get_or_init(|| env::var_os(ENV).filter(|value| !value.is_empty()).map(PathBuf::from))
        .as_deref()
}

fn append(path: &Path, record: &Record) -> io::Result<()> {
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(record.line().as_bytes())
}

/// Appends the record of a unit that ran uncached before any plan existed.
pub fn skipped(begun: Instant, krate: &str, primary: bool, incremental: bool, mode: &str) {
    Probe::new(begun, || {
        let mut record = Record::new(krate, false, primary, incremental);
        record.store_mode = mode.to_string();
        record
    })
    .finish();
}

/// Collects the stages of one wrapper run; inert, with no file access, when no sink is configured.
pub struct Probe {
    begun: Instant,
    active: Option<(PathBuf, Mutex<Record>)>,
}

impl Probe {
    pub fn off() -> Probe {
        Probe { begun: Instant::now(), active: None }
    }

    pub fn new(begun: Instant, record: impl FnOnce() -> Record) -> Probe {
        Probe::at(target(), begun, record)
    }

    pub fn at(sink: Option<&Path>, begun: Instant, record: impl FnOnce() -> Record) -> Probe {
        Probe { begun, active: sink.map(|path| (path.to_path_buf(), Mutex::new(record()))) }
    }

    pub fn set(&self, update: impl FnOnce(&mut Record)) {
        if let Some((_, record)) = &self.active {
            update(&mut record.lock().unwrap_or_else(PoisonError::into_inner));
        }
    }

    pub fn mode(&self, mode: impl FnOnce() -> String) {
        self.set(|record| record.store_mode = mode());
    }

    /// Stamps the total and appends the record; a failed write is dropped because timing must never fail a build.
    pub fn finish(&self) {
        let Some((path, record)) = &self.active else { return };
        let mut done = record.lock().unwrap_or_else(PoisonError::into_inner).clone();
        done.total_ms = ms(self.begun);
        let _ = append(path, &done);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Scratch {
            let dir = env::temp_dir().join(format!("frust-timings-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn sample() -> Record {
        let mut record = Record::new("serde", true, false, true);
        record.prekey_ms = 1;
        record.lookup_ms = 2;
        record.keywait_ms = 3;
        record.restore_ms = 4;
        record.rustc_ms = 5;
        record.freeze_ms = 6;
        record.spawn_ms = 7;
        record.store_mode = "inline:slots-busy".to_string();
        record.inline_store_ms = 8;
        record.total_ms = 9;
        record.extern_files = 10;
        record.extern_bytes_hashed = 11;
        record.extern_bytes_memo = 12;
        record.native_dir_bytes_hashed = 13;
        record
    }

    #[test]
    fn a_record_is_one_line_that_round_trips_with_the_documented_field_names() {
        let record = sample();
        let line = record.line();
        assert!(line.ends_with('\n') && line.matches('\n').count() == 1);
        let value: serde_json::Value = serde_json::from_str(line.trim_end()).unwrap();
        let object = value.as_object().unwrap();
        for name in [
            "crate", "path_unit", "primary", "incremental", "prekey_ms", "extern_files", "extern_bytes_hashed",
            "extern_bytes_memo", "native_dir_bytes_hashed", "lookup_ms", "keywait_ms", "restore_ms", "rustc_ms",
            "freeze_ms", "spawn_ms", "store_mode", "inline_store_ms", "total_ms",
        ] {
            assert!(object.contains_key(name), "missing {name}");
        }
        assert_eq!(object.len(), 18);
        assert_eq!(object["crate"], "serde");
        assert_eq!(object["path_unit"], true);
        assert_eq!(serde_json::from_str::<Record>(line.trim_end()).unwrap(), record);
    }

    #[test]
    fn no_file_is_created_when_no_sink_is_configured() {
        let s = Scratch::new("unset");
        let probe = Probe::at(None, Instant::now(), || unreachable!("an inert probe builds no record"));
        probe.set(|record| record.rustc_ms = 5);
        probe.mode(|| unreachable!("an inert probe formats nothing"));
        probe.finish();
        assert_eq!(std::fs::read_dir(&s.0).unwrap().count(), 0);
        Probe::off().finish();
    }

    #[test]
    fn each_finished_probe_appends_exactly_one_line() {
        let s = Scratch::new("append");
        let path = s.0.join("timings.jsonl");
        for krate in ["a", "b"] {
            let probe = Probe::at(Some(&path), Instant::now(), || Record::new(krate, false, true, false));
            probe.set(|record| record.rustc_ms = 12);
            probe.mode(|| "deferred".to_string());
            probe.finish();
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let records: Vec<Record> = text.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].krate, "a");
        assert_eq!(records[1].krate, "b");
        assert!(records.iter().all(|r| r.rustc_ms == 12 && r.store_mode == "deferred" && r.primary));
    }
}
