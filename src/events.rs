//! Small `O_APPEND` writes are atomic on local filesystems, so concurrent writers never interleave.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::paths::Dirs;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheResult {
    Hit,
    Miss,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Cache {
        t: u64,
        result: CacheResult,
        #[serde(rename = "crate")]
        krate: String,
        version: String,
        compile_ms: u64,
        saved_ms: u64,
        bytes: u64,
    },
    Run {
        t: u64,
        workspace: String,
        command: String,
        wall_ms: u64,
        units: u32,
        cached: u32,
        compiled: u32,
        fresh: u32,
        saved_ms: u64,
        success: bool,
    },
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn append(path: &Path, line: &[u8]) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(line);
    }
}

/// Record an event in the ledger and, for cache events, the per-run feed if one is configured.
pub fn record(dirs: &Dirs, event: &Event) {
    let Ok(mut line) = serde_json::to_vec(event) else { return };
    line.push(b'\n');
    append(&dirs.ledger(), &line);
    if matches!(event, Event::Cache { .. })
        && let Some(feed) = std::env::var_os("FRUST_EVENTS").filter(|v| !v.is_empty()) {
            append(Path::new(&feed), &line);
        }
}

pub struct Feed {
    path: PathBuf,
    offset: u64,
}

impl Feed {
    pub fn new(path: PathBuf) -> Feed {
        Feed { path, offset: 0 }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn poll(&mut self) -> Vec<Event> {
        let Ok(mut file) = File::open(&self.path) else { return Vec::new() };
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return Vec::new();
        }
        let mut text = String::new();
        if file.read_to_string(&mut text).is_err() {
            return Vec::new();
        }
        let complete = text.rfind('\n').map(|i| i + 1).unwrap_or(0);
        self.offset += complete as u64;
        text[..complete].lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feed_reads_only_complete_new_lines() {
        let path = std::env::temp_dir().join(format!("frust-feed-{}.jsonl", std::process::id()));
        let event = Event::Cache {
            t: 1,
            result: CacheResult::Hit,
            krate: "serde".into(),
            version: "1.0.0".into(),
            compile_ms: 900,
            saved_ms: 850,
            bytes: 10,
        };
        let line = serde_json::to_string(&event).unwrap();
        let mut feed = Feed::new(path.clone());
        assert!(feed.poll().is_empty());
        fs::write(&path, format!("{line}\n{}", &line[..10])).unwrap();
        assert_eq!(feed.poll().len(), 1);
        assert!(feed.poll().is_empty());
        fs::write(&path, format!("{line}\n{line}\n")).unwrap();
        assert_eq!(feed.poll().len(), 1);
        fs::remove_file(&path).unwrap();
    }
}
