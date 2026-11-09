//! Per-process timing records: with `FRUST_TIMINGS=<file>` set, each wrapper appends one JSON line at exit.

use std::collections::BTreeMap;
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
    /// Restored files by path: `kept`, `link`, `adopt` (a copy the store then took over), `copy`, `clone`, `text` or `group`.
    #[serde(default)]
    pub restore_paths: BTreeMap<String, u64>,
    /// The first distinct reasons a linked file was copied instead of linked, such as `link:os32` or `store-nlink=2`.
    #[serde(default)]
    pub restore_fallbacks: Vec<String>,
    /// The entry restored, so a store-only entry can be traced to its last restore.
    #[serde(default)]
    pub restore_key: String,
    /// Why the lookup found nothing to restore: `no-candidate`, `entry-gone`, `input:<file>`, `env:<name>` or `restore:<error>`.
    #[serde(default)]
    pub miss_reason: String,
    /// Empty for a unit; `passthrough` for an uncached compiler call and `store` for a store helper's outcome.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kind: String,
    /// For a store outcome, the CPU time the storing thread spent on the store; `total_ms` is its wall time.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cpu_ms: u64,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

pub const FALLBACKS_KEPT: usize = 8;

/// Adds `reason` unless it is already listed or the list is full.
pub fn note_reason(list: &mut Vec<String>, reason: &str) {
    if list.len() < FALLBACKS_KEPT && !list.iter().any(|known| known == reason) {
        list.push(reason.to_string());
    }
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
            restore_paths: BTreeMap::new(),
            restore_fallbacks: Vec::new(),
            restore_key: String::new(),
            miss_reason: String::new(),
            kind: String::new(),
            cpu_ms: 0,
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

/// The sink this process writes to, handed to a store helper that may not inherit the environment.
pub fn configured() -> Option<&'static Path> {
    target()
}

/// Passthrough calls and store outcomes go beside the unit records, so the sink keeps exactly one line per compiled unit.
pub fn events_path(sink: &Path) -> PathBuf {
    let mut name = sink.as_os_str().to_owned();
    name.push(".events");
    PathBuf::from(name)
}

// Only a file the reader created is appended to, so a helper holding a stale path never recreates a consumed one.
fn append_event(sink: Option<&Path>, record: &Record) {
    let Some(path) = sink else { return };
    if let Ok(mut file) = OpenOptions::new().append(true).open(events_path(path)) {
        let _ = file.write_all(record.line().as_bytes());
    }
}

/// Records an uncached compiler call, such as Cargo's `-vV` probe, so no-op builds count their spawns.
pub fn passthrough(begun: Instant) {
    let Some(sink) = target() else { return };
    let mut record = Record::new("", false, false, false);
    record.kind = "passthrough".to_string();
    record.store_mode = "passthrough".to_string();
    record.total_ms = ms(begun);
    append_event(Some(sink), &record);
}

/// Records a store step's outcome with its wall and CPU milliseconds; a helper has no unit record, so this is its only trace.
pub fn store_outcome(sink: Option<&Path>, krate: &str, outcome: &str, spent: (u64, u64)) {
    let mut record = Record::new(krate, false, false, false);
    record.kind = "store".to_string();
    record.store_mode = outcome.to_string();
    (record.total_ms, record.cpu_ms) = spent;
    append_event(sink, &record);
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
        record.restore_paths.insert("link".to_string(), 2);
        note_reason(&mut record.restore_fallbacks, "link:os32");
        record.restore_key = "abc".to_string();
        record
    }

    #[test]
    fn reasons_are_distinct_and_capped() {
        let mut list = Vec::new();
        for n in 0..20 {
            note_reason(&mut list, &format!("r{}", n % 10));
            note_reason(&mut list, "r0");
        }
        assert_eq!(list.len(), FALLBACKS_KEPT);
        assert_eq!(list[..3], ["r0", "r1", "r2"]);
    }

    #[test]
    fn records_from_older_wrappers_parse_with_empty_new_fields() {
        let old = r#"{"crate":"a","path_unit":false,"primary":false,"incremental":false,"prekey_ms":0,"extern_files":0,"extern_bytes_hashed":0,"extern_bytes_memo":0,"native_dir_bytes_hashed":0,"lookup_ms":0,"keywait_ms":0,"restore_ms":0,"rustc_ms":0,"freeze_ms":0,"spawn_ms":0,"store_mode":"hit","inline_store_ms":0,"total_ms":0}"#;
        let record: Record = serde_json::from_str(old).unwrap();
        assert!(record.restore_paths.is_empty() && record.restore_fallbacks.is_empty() && record.kind.is_empty());
    }

    #[test]
    fn store_outcomes_go_beside_the_unit_records_and_only_into_an_existing_file() {
        let s = Scratch::new("store");
        let path = s.0.join("timings.jsonl");
        let events = events_path(&path);
        assert_eq!(events, s.0.join("timings.jsonl.events"));
        store_outcome(Some(&path), "winapi", "skip:input-changed", (0, 0));
        assert!(!events.exists() && !path.exists(), "a consumed sink is not recreated");
        std::fs::write(&events, "").unwrap();
        store_outcome(Some(&path), "winapi", "skip:input-changed", (9, 4));
        assert!(!path.exists(), "the unit records stay one line per unit");
        let record: Record = serde_json::from_str(std::fs::read_to_string(&events).unwrap().trim_end()).unwrap();
        assert_eq!((record.kind.as_str(), record.krate.as_str(), record.store_mode.as_str()), ("store", "winapi", "skip:input-changed"));
        assert_eq!((record.total_ms, record.cpu_ms), (9, 4));
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
            "freeze_ms", "spawn_ms", "store_mode", "inline_store_ms", "total_ms", "restore_paths", "restore_fallbacks",
            "restore_key", "miss_reason",
        ] {
            assert!(object.contains_key(name), "missing {name}");
        }
        assert_eq!(object.len(), 22, "a unit record carries no kind");
        assert_eq!(object["restore_paths"]["link"], 2);
        assert_eq!(object["restore_fallbacks"][0], "link:os32");
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
