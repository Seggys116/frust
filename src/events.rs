//! Managed ledger appends and replacement share a stable lock; external event feeds remain best effort.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::paths::Dirs;
use crate::{fsx, lock, sys};

const LEDGER_LIMIT: u64 = 8 * 1024 * 1024;

struct LedgerGuard(File);

impl LedgerGuard {
    fn acquire(dirs: &Dirs) -> io::Result<Self> {
        let mode = lock::shared_mode(dirs);
        let path = dirs.root().join("ledger.lock");
        let file = match lock::open_lock_file(&path, mode) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                lock::ensure_dir(dirs.root(), mode)?;
                lock::open_lock_file(&path, mode)?
            }
            other => other?,
        };
        sys::lock(&file, sys::LockKind::Exclusive)?;
        Ok(Self(file))
    }
}

impl Drop for LedgerGuard {
    fn drop(&mut self) { let _ = sys::unlock(&self.0); }
}

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
    let open = || OpenOptions::new().create(true).append(true).open(path);
    let file = match open() {
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            open()
        }
        other => other,
    };
    if let Ok(mut file) = file {
        let _ = file.write_all(line);
    }
}

fn append_ledger(dirs: &Dirs, line: &[u8], before_open: impl FnOnce()) {
    let Ok(_guard) = LedgerGuard::acquire(dirs) else { return };
    // Open only under the stable lock so replacement cannot strand a writer on the old inode.
    before_open();
    append(&dirs.ledger(), line);
}

pub(crate) fn trim_ledger(dirs: &Dirs) -> io::Result<()> {
    trim_ledger_with(dirs, LEDGER_LIMIT, || {})
}

fn trim_ledger_with(dirs: &Dirs, limit: u64, after_snapshot: impl FnOnce()) -> io::Result<()> {
    let _guard = LedgerGuard::acquire(dirs)?;
    let path = dirs.ledger();
    if fs::metadata(&path)?.len() <= limit { return Ok(()); }
    let text = fs::read_to_string(&path)?;
    after_snapshot();
    let keep: Vec<&str> = text.lines().rev().take(20_000).collect();
    let trimmed: String = keep.into_iter().rev().map(|line| format!("{line}\n")).collect();
    fsx::write_atomic(&path, trimmed.as_bytes())
}

/// Record an event in the ledger and, for cache events, the per-run feed if one is configured.
pub fn record(dirs: &Dirs, event: &Event) {
    record_waited(dirs, event, 0);
}

/// A cache event of a compile that queued behind another process's compile of the same unit carries `waited_ms`; zero leaves the record unchanged.
pub fn record_waited(dirs: &Dirs, event: &Event, waited_ms: u64) {
    let encoded = if waited_ms > 0 {
        serde_json::to_value(event).and_then(|mut value| {
            if let Some(map) = value.as_object_mut() {
                map.insert("waited_ms".to_string(), waited_ms.into());
            }
            serde_json::to_vec(&value)
        })
    } else {
        serde_json::to_vec(event)
    };
    let Ok(mut line) = encoded else { return };
    line.push(b'\n');
    append_ledger(dirs, &line, || {});
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
        // A path stat is one syscall and still follows a replaced file, unlike a held handle.
        match fs::metadata(&self.path) {
            Ok(meta) if meta.len() != self.offset => {}
            _ => return Vec::new(),
        }
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
    use std::collections::BTreeSet;
    use std::sync::{Arc, Barrier, mpsc};
    use std::thread;
    use std::time::Duration;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let root = fsx::temp_sibling(&std::env::temp_dir().join(format!("frust-ledger-{tag}")));
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }

        fn dirs(&self) -> Dirs { Dirs::at(&self.0) }
    }

    impl Drop for Scratch {
        fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); }
    }

    struct TestChild(std::process::Child);

    impl TestChild {
        fn finish(mut self) {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                if let Some(status) = self.0.try_wait().unwrap() {
                    let mut stderr = String::new();
                    self.0.stderr.take().unwrap().read_to_string(&mut stderr).unwrap();
                    assert!(status.success(), "{stderr}");
                    return;
                }
                assert!(std::time::Instant::now() < deadline, "process writer did not finish before its deadline");
                thread::sleep(Duration::from_millis(1));
            }
        }
    }

    impl Drop for TestChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn marker(id: u64) -> Vec<u8> {
        let event = Event::Run {
            t: id, workspace: "ledger-test".into(), command: id.to_string(), wall_ms: 1,
            units: 1, cached: 0, compiled: 1, fresh: 0, saved_ms: 0, success: true,
        };
        let mut line = serde_json::to_vec(&event).unwrap();
        line.push(b'\n');
        line
    }

    fn marker_ids(dirs: &Dirs) -> Vec<u64> {
        let text = fs::read_to_string(dirs.ledger()).unwrap();
        assert!(text.ends_with('\n'));
        text.lines().map(|line| match serde_json::from_str::<Event>(line).unwrap() {
            Event::Run { t, .. } => t,
            other => panic!("unexpected event: {other:?}"),
        }).collect()
    }

    fn assert_ledger_locked(dirs: &Dirs) {
        let file = lock::open_lock_file(&dirs.root().join("ledger.lock"), None).unwrap();
        assert!(matches!(sys::try_lock(&file, sys::LockKind::Exclusive), Err(fs::TryLockError::WouldBlock)));
    }

    #[test]
    fn ledger_writer_locks_before_open_and_is_included_in_rotation() {
        let scratch = Scratch::new("writer-before-rotation");
        let dirs = scratch.dirs();
        append_ledger(&dirs, &marker(1), || {});
        let (held_tx, held_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let writer_dirs = scratch.dirs();
        let writer = thread::spawn(move || append_ledger(&writer_dirs, &marker(2), || {
            held_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        }));
        held_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_ledger_locked(&dirs);
        let (started_tx, started_rx) = mpsc::channel();
        let (snapshot_tx, snapshot_rx) = mpsc::channel();
        let trim_dirs = scratch.dirs();
        let trimming = thread::spawn(move || {
            started_tx.send(()).unwrap();
            trim_ledger_with(&trim_dirs, 0, || { snapshot_tx.send(()).unwrap(); }).unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let blocked = snapshot_rx.recv_timeout(Duration::from_millis(50)).is_err();
        release_tx.send(()).unwrap();
        writer.join().unwrap();
        trimming.join().unwrap();
        assert!(blocked, "rotation read the ledger while its writer held the lock");
        assert_eq!(marker_ids(&dirs), [1, 2]);
    }

    #[test]
    fn ledger_rotation_blocks_new_writer_until_replacement_is_visible() {
        let scratch = Scratch::new("rotation-before-writer");
        let dirs = scratch.dirs();
        append_ledger(&dirs, &marker(1), || {});
        let mut previous = File::open(dirs.ledger()).unwrap();
        let (snapshot_tx, snapshot_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let trim_dirs = scratch.dirs();
        let trimming = thread::spawn(move || trim_ledger_with(&trim_dirs, 0, || {
            snapshot_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        }).unwrap());
        snapshot_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_ledger_locked(&dirs);
        let (started_tx, started_rx) = mpsc::channel();
        let (opened_tx, opened_rx) = mpsc::channel();
        let writer_dirs = scratch.dirs();
        let writer = thread::spawn(move || {
            started_tx.send(()).unwrap();
            append_ledger(&writer_dirs, &marker(2), || { opened_tx.send(()).unwrap(); });
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let blocked = opened_rx.recv_timeout(Duration::from_millis(50)).is_err();
        release_tx.send(()).unwrap();
        trimming.join().unwrap();
        writer.join().unwrap();
        assert!(blocked, "the new writer opened its ledger before rotation finished");
        assert_eq!(marker_ids(&dirs), [1, 2]);
        let mut old_text = String::new();
        previous.read_to_string(&mut old_text).unwrap();
        assert_eq!(old_text.as_bytes(), marker(1));
    }

    #[test]
    fn ledger_rotation_preserves_complete_unique_thread_records() {
        let scratch = Scratch::new("thread-writers");
        let barrier = Arc::new(Barrier::new(5));
        let mut writers = Vec::new();
        for worker in 0..4 {
            let dirs = scratch.dirs();
            let barrier = barrier.clone();
            writers.push(thread::spawn(move || {
                barrier.wait();
                for index in 0..100 { append_ledger(&dirs, &marker(worker * 100 + index), || {}); }
            }));
        }
        append_ledger(&scratch.dirs(), &marker(400), || {});
        barrier.wait();
        for _ in 0..40 { trim_ledger_with(&scratch.dirs(), 0, || {}).unwrap(); }
        for writer in writers { writer.join().unwrap(); }
        let ids = marker_ids(&scratch.dirs());
        assert_eq!(ids.len(), 401);
        assert_eq!(ids.into_iter().collect::<BTreeSet<_>>(), (0..401).collect::<BTreeSet<_>>());
    }

    #[test]
    fn ledger_subprocess_writer() {
        let Some(root) = std::env::var_os("FRUST_LEDGER_TEST_HOME") else { return };
        let root = PathBuf::from(root);
        let base: u64 = std::env::var("FRUST_LEDGER_TEST_BASE").unwrap().parse().unwrap();
        fs::write(root.join(format!("ready-{base}")), b"").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !root.join("start").is_file() {
            assert!(std::time::Instant::now() < deadline, "process writer start barrier timed out");
            thread::sleep(Duration::from_millis(1));
        }
        for index in 0..60 { append_ledger(&Dirs::at(&root), &marker(base + index), || {}); }
    }

    #[test]
    fn ledger_rotation_preserves_complete_unique_process_records() {
        let scratch = Scratch::new("process-writers");
        append_ledger(&scratch.dirs(), &marker(240), || {});
        let mut children = Vec::new();
        for worker in 0..4 {
            let child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "events::tests::ledger_subprocess_writer"])
                .env("FRUST_LEDGER_TEST_HOME", &scratch.0)
                .env("FRUST_LEDGER_TEST_BASE", (worker * 60).to_string())
                .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped()).spawn().unwrap();
            children.push(TestChild(child));
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !(0..4).all(|worker| scratch.0.join(format!("ready-{}", worker * 60)).is_file()) {
            assert!(std::time::Instant::now() < deadline, "process writers did not reach their start barrier");
            thread::sleep(Duration::from_millis(1));
        }
        fs::write(scratch.0.join("start"), b"").unwrap();
        for _ in 0..40 { trim_ledger_with(&scratch.dirs(), 0, || {}).unwrap(); }
        for child in children { child.finish(); }
        let ids = marker_ids(&scratch.dirs());
        assert_eq!(ids.len(), 241);
        assert_eq!(ids.into_iter().collect::<BTreeSet<_>>(), (0..241).collect::<BTreeSet<_>>());
    }

    #[test]
    fn ledger_trim_rechecks_size_and_retains_the_latest_twenty_thousand_lines() {
        let scratch = Scratch::new("retention");
        let dirs = scratch.dirs();
        append_ledger(&dirs, &marker(1), || {});
        let before = sys::stat(&dirs.ledger()).unwrap();
        trim_ledger(&dirs).unwrap();
        let after = sys::stat(&dirs.ledger()).unwrap();
        assert_eq!((before.dev(), before.ino()), (after.dev(), after.ino()));
        let text: Vec<u8> = (0..20_003).flat_map(marker).collect();
        fs::write(dirs.ledger(), text).unwrap();
        trim_ledger_with(&dirs, 0, || {}).unwrap();
        assert_eq!(marker_ids(&dirs), (3..20_003).collect::<Vec<_>>());
    }

    #[test]
    fn ledger_reopens_after_home_is_recreated_and_skips_unavailable_lock() {
        let scratch = Scratch::new("recreated-home");
        let home = scratch.0.join("home");
        let dirs = Dirs::at(&home);
        append_ledger(&dirs, &marker(1), || {});
        sys::rename(&home, &scratch.0.join("previous-home")).unwrap();
        append_ledger(&dirs, &marker(2), || {});
        assert_eq!(marker_ids(&dirs), [2]);
        assert_eq!(marker_ids(&Dirs::at(scratch.0.join("previous-home"))), [1]);
        let blocked = scratch.0.join("blocked");
        fs::create_dir_all(blocked.join("ledger.lock")).unwrap();
        append_ledger(&Dirs::at(&blocked), &marker(3), || panic!("unavailable lock admitted a writer"));
        assert!(!Dirs::at(blocked).ledger().exists());
    }

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

    #[test]
    fn waited_time_is_recorded_only_when_there_was_a_wait() {
        let root = std::env::temp_dir().join(format!("frust-waited-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let dirs = Dirs::at(&root);
        let event = Event::Cache {
            t: 1,
            result: CacheResult::Miss,
            krate: "serde".into(),
            version: "1.0.0".into(),
            compile_ms: 900,
            saved_ms: 0,
            bytes: 10,
        };
        record_waited(&dirs, &event, 0);
        record_waited(&dirs, &event, 42);
        let rows: Vec<serde_json::Value> =
            fs::read_to_string(dirs.ledger()).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(rows.len(), 2);
        assert!(rows[0].get("waited_ms").is_none());
        assert_eq!(rows[1]["waited_ms"], 42);
        assert!(serde_json::from_value::<Event>(rows[1].clone()).is_ok(), "readers that do not know the field still parse the event");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn append_creates_missing_parent_and_appends() {
        let root = std::env::temp_dir().join(format!("frust-append-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let path = root.join("a").join("b").join("feed.jsonl");
        append(&path, b"one\n");
        append(&path, b"two\n");
        assert_eq!(fs::read_to_string(&path).unwrap(), "one\ntwo\n");
        fs::remove_dir_all(&root).unwrap();
    }
}
