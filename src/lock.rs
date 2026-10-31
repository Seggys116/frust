//! Scoped FIFO build locks with live progress and joins for identical builds.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::events::Event;
use crate::fsx;
use crate::paths::{Dirs, short_hash};
use crate::sys::{self, LockKind, signal};

/// Notifications wake waiters early; these limits also cover missed events and signal checks.
const FRONT_POLL: Duration = Duration::from_millis(15);
const FOLLOW_POLL: Duration = Duration::from_millis(10);
const FOLLOW_CHECK: Duration = Duration::from_millis(50);
const FIRST_POLL: Duration = Duration::from_millis(20);
/// Waiters behind the first only need to notice a change in their position, and each poll opens every ticket.
const MAX_POLL: Duration = Duration::from_millis(40);
const LOG_FLUSH_EVERY: Duration = Duration::from_millis(25);
const FOLLOWER_FLUSH_EVERY: Duration = Duration::from_millis(10);
const LOG_FLUSH_BYTES: usize = 64 * 1024;
const FOLLOWER_SCAN_EVERY: Duration = Duration::from_millis(50);
const ANNOUNCE_AFTER: Duration = Duration::from_millis(150);
const NOTIFY_EVERY: Duration = Duration::from_millis(100);
const PUBLISH_EVERY: Duration = Duration::from_millis(200);
/// A half-created ticket (crash between create and rename) older than this is removed.
const STAGING_STALE: Duration = Duration::from_secs(30);
const ACTIVE_LIMIT: usize = 8;
/// Output lines stop being logged at this size; cache events and the final record still are.
pub const LOG_CAP: u64 = 8 * 1024 * 1024;
/// Bytes a follower reads per poll; bounds the work between signal checks.
const TAIL_CHUNK: usize = 64 * 1024;
const TAIL_CHUNKS_PER_POLL: usize = 16;
const MAX_RECORD: usize = 2 * 1024 * 1024;
pub const ALL: &str = "_all";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Owner {
    pub pid: u32,
    pub command: String,
    pub cwd: String,
    pub since_unix: u64,
}

/// The holder in `owner.json`; `Owner` is flattened so older readers that only know `Owner` still parse it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Holder {
    #[serde(flatten)]
    pub owner: Owner,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub uid: Option<u32>,
    #[serde(default)]
    pub fingerprint: String,
    #[serde(default)]
    pub slot: String,
    /// Every slot this build holds; empty for a whole-directory build.
    #[serde(default)]
    pub scope: Vec<String>,
    #[serde(default)]
    pub log: String,
    #[serde(default)]
    pub since_ns: u64,
}

impl Holder {
    pub fn label(&self) -> String {
        if self.user.is_empty() { format!("pid {}", self.owner.pid) } else { format!("{} pid {}", self.user, self.owner.pid) }
    }

    fn same_build(&self, other: &Holder) -> bool {
        self.owner.pid == other.owner.pid && self.since_ns == other.since_ns && self.log == other.log
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    pub pid: u32,
    pub done: u32,
    pub expected: Option<u32>,
    pub fresh: u32,
    pub cached: u32,
    pub active: Vec<String>,
    #[serde(default)]
    pub more_active: u32,
    pub updated_ms: u64,
    /// Filled in by [`list`], never published.
    #[serde(default, skip_serializing)]
    pub joined: u32,
}

impl Progress {
    pub fn summary(&self) -> String {
        let units = |n: u32| if n == 1 { "unit" } else { "units" };
        let mut text = match self.expected.filter(|&e| e > 0) {
            Some(expected) => {
                let total = expected.max(self.done);
                format!("{}/~{total} {}", self.done, units(total))
            }
            None => format!("{} {} done", self.done, units(self.done)),
        };
        if !self.active.is_empty() {
            text.push_str(", compiling ");
            text.push_str(&self.active.join(", "));
            if self.more_active > 0 {
                text.push_str(&format!(" +{}", self.more_active));
            }
        }
        if self.joined > 0 {
            text.push_str(&format!(", {} joined", self.joined));
        }
        text
    }

    pub fn start(&mut self, name: &str) {
        if self.active.len() < ACTIVE_LIMIT {
            self.active.push(name.to_string());
        } else {
            self.more_active += 1;
        }
    }

    /// Names compare with `-` and `_` equal.
    pub fn finish(&mut self, names: &[&str]) {
        let norm = |s: &str| s.replace('-', "_");
        let keys: Vec<String> = names.iter().filter(|n| !n.is_empty()).map(|n| norm(n)).collect();
        if let Some(i) = self.active.iter().position(|a| keys.contains(&norm(a))) {
            self.active.remove(i);
        } else if self.more_active > 0 {
            self.more_active -= 1;
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finished {
    pub pid: u32,
    #[serde(default)]
    pub user: String,
    pub command: String,
    pub fingerprint: String,
    pub success: bool,
    pub units: u32,
    pub finished_ns: u64,
    #[serde(default)]
    pub code: Option<i32>,
}

impl Finished {
    pub fn label(&self) -> String {
        if self.user.is_empty() { format!("pid {}", self.pid) } else { format!("{} pid {}", self.user, self.pid) }
    }
}

#[derive(Debug, Clone)]
pub struct Joined {
    pub build: Finished,
    pub identical: bool,
    pub followed: bool,
}

#[derive(Debug, Clone)]
pub struct WaitInfo {
    pub owner: Option<Owner>,
    pub ahead: usize,
    pub waited: Duration,
}

#[derive(Debug, Clone)]
pub struct Waiting {
    pub info: WaitInfo,
    pub holder: Option<Holder>,
    /// Only present when it belongs to `holder`.
    pub progress: Option<Progress>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "k", rename_all = "snake_case")]
pub enum Record {
    /// `complete`: the log holds all output, so a follower can replay the build from it.
    Start { json: bool, complete: bool },
    Out { l: String },
    Err { l: String },
    Cache { e: Event },
    Trunc,
    /// Written after `last.json` and before the lock is released.
    End { code: i32, interrupted: bool },
}

#[derive(Debug, Clone, Copy)]
pub struct BuildResult {
    pub code: i32,
    pub units: u32,
    /// Stopped by a signal: not a result a follower should adopt.
    pub interrupted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Abandon {
    Died,
    Interrupted,
    /// The holder failed but its log cannot reproduce its output (not captured, or capped).
    Unreplayable,
}

impl Abandon {
    pub fn describe(self) -> &'static str {
        match self {
            Abandon::Died => "it stopped without finishing",
            Abandon::Interrupted => "it was interrupted",
            Abandon::Unreplayable => "it failed and its output was not recorded",
        }
    }
}

#[derive(Debug)]
pub enum LockEvent<'a> {
    Waiting(&'a Waiting),
    Joined(&'a Holder),
    Record(&'a Record),
    Abandoned(&'a Holder, Abandon),
}

#[derive(Debug, Clone)]
pub struct SharedFailure {
    pub holder: Holder,
    pub code: i32,
}

#[derive(Debug)]
pub enum Acquired {
    Lock(Box<BuildLock>),
    Failed(SharedFailure),
}

#[derive(Debug)]
pub enum LockError {
    Timeout(Box<Waiting>),
    Interrupted(i32),
    Io(io::Error),
}

impl LockError {
    /// Shell convention (128 + signal) for signals, `EX_TEMPFAIL` otherwise.
    pub fn exit_code(&self) -> i32 {
        match self {
            LockError::Interrupted(signal) => 128 + signal,
            _ => 75,
        }
    }
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LockError::Timeout(waiting) => match &waiting.holder {
                Some(h) => write!(f, "timed out waiting for build lock held by {} (`{}`)", h.label(), h.owner.command),
                None => write!(f, "timed out waiting for build lock"),
            },
            LockError::Interrupted(_) => write!(f, "interrupted while waiting for build lock"),
            LockError::Io(err) => write!(f, "build lock error: {err}"),
        }
    }
}

impl From<io::Error> for LockError {
    fn from(err: io::Error) -> Self {
        LockError::Io(err)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// The profile could not be determined.
    All,
    /// Sorted, distinct slots such as `debug@host`.
    Slots(Vec<String>),
}

impl Scope {
    pub fn slots<I: IntoIterator<Item = String>>(names: I) -> Scope {
        let mut list: Vec<String> = names.into_iter().collect();
        if list.is_empty() || !list.iter().all(|n| valid_slot(n)) {
            return Scope::All;
        }
        list.sort();
        list.dedup();
        Scope::Slots(list)
    }

    fn names(&self) -> Vec<String> {
        match self {
            Scope::All => vec![ALL.to_string()],
            Scope::Slots(list) => list.clone(),
        }
    }

    /// Empty means everything.
    fn stored(&self) -> Vec<String> {
        match self {
            Scope::All => Vec::new(),
            Scope::Slots(list) => list.clone(),
        }
    }

    pub fn label(&self) -> String {
        match self {
            Scope::All => "whole target directory".to_string(),
            Scope::Slots(list) => stored_label(list),
        }
    }
}

fn stored_label(list: &[String]) -> String {
    if list.is_empty() {
        return "whole target directory".to_string();
    }
    list.iter().map(|s| s.strip_suffix("@host").unwrap_or(s)).collect::<Vec<_>>().join(", ")
}

/// Slot names become directory names: plain characters only, nothing reserved.
fn valid_slot(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.starts_with(['.', '_', '-'])
        && name.contains('@')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@'))
}

/// An empty scope is the whole directory.
fn conflicts(a: &[String], b: &[String]) -> bool {
    a.is_empty() || b.is_empty() || a.iter().any(|s| b.contains(s))
}

fn primary_slot(stored: &[String]) -> &str {
    stored.first().map(String::as_str).unwrap_or(ALL)
}

#[derive(Debug, Clone, Copy)]
pub struct Request<'a> {
    pub command: &'a str,
    /// Empty disables joining.
    pub fingerprint: &'a str,
    pub scope: &'a Scope,
    pub join: bool,
    pub timeout: Option<Duration>,
}

#[derive(Debug)]
pub struct BuildLock {
    /// Explicitly unlocked by `Drop::drop` before these files close.
    _level: File,
    _slots: Vec<File>,
    slot_dirs: Vec<PathBuf>,
    mode: Option<Mode>,
    holder: Holder,
    joined: Option<Joined>,
    last_publish: Option<Instant>,
    published: Option<Progress>,
    log: Option<LogWriter>,
}

impl BuildLock {
    pub fn joined(&self) -> Option<&Joined> {
        self.joined.as_ref()
    }

    pub fn holder(&self) -> &Holder {
        &self.holder
    }

    fn records_dir(&self) -> &Path {
        &self.slot_dirs[0]
    }

    /// Cheap to call often: writes are rate-limited and skipped when nothing changed.
    pub fn publish(&mut self, progress: &Progress) {
        let now = Instant::now();
        if self.last_publish.is_some_and(|t| now.duration_since(t) < PUBLISH_EVERY) {
            return;
        }
        if self.published.as_ref().is_some_and(|p| p.done == progress.done && p.active == progress.active && p.cached == progress.cached) {
            return;
        }
        let mut record = progress.clone();
        record.pid = self.holder.owner.pid;
        record.updated_ms = now_ms();
        if let Ok(json) = serde_json::to_vec(&record) {
            let _ = write_shared(&self.records_dir().join("progress.json"), &json, self.mode);
        }
        self.last_publish = Some(now);
        self.published = Some(record);
    }

    pub fn log_start(&mut self, json: bool, complete: bool) {
        self.append(&Record::Start { json, complete });
    }

    pub fn log_out(&mut self, line: &[u8]) {
        if self.log.as_ref().is_some_and(|l| !l.truncated) {
            self.append(&Record::Out { l: String::from_utf8_lossy(line).into_owned() });
        }
    }

    pub fn log_err(&mut self, line: &[u8]) {
        if self.log.as_ref().is_some_and(|l| !l.truncated) {
            self.append(&Record::Err { l: String::from_utf8_lossy(line).into_owned() });
        }
    }

    pub fn log_cache(&mut self, event: &Event) {
        self.append(&Record::Cache { e: event.clone() });
    }

    fn append(&mut self, record: &Record) {
        if let Some(log) = self.log.as_mut() {
            log.append(record);
        }
    }

    /// Writes buffered records once they are old enough; call from the supervision loop so a quiet build still reaches followers.
    pub fn flush_log(&mut self) {
        if let Some(log) = self.log.as_mut() {
            log.flush_if_due();
        }
    }

    /// Dropping without this records nothing, which followers treat as a holder that died.
    pub fn finish(mut self, result: BuildResult) {
        let record = Finished {
            pid: self.holder.owner.pid,
            user: self.holder.user.clone(),
            command: self.holder.owner.command.clone(),
            fingerprint: self.holder.fingerprint.clone(),
            success: result.code == 0 && !result.interrupted,
            units: result.units,
            finished_ns: now_ns(),
            code: Some(result.code),
        };
        // Written under the lock and before the final log record, so the next holder and any follower see it.
        if let Ok(json) = serde_json::to_vec(&record) {
            let _ = write_shared(&self.records_dir().join("last.json"), &json, self.mode);
        }
        if let Some(log) = self.log.as_mut() {
            log.end(result.code, result.interrupted);
        }
    }
}

impl Drop for BuildLock {
    fn drop(&mut self) {
        let mut log = self.log.take();
        if let Some(log) = log.as_mut() {
            log.flush();
        }
        let _ = fs::remove_file(self.records_dir().join("progress.json"));
        for dir in &self.slot_dirs {
            let _ = fs::remove_file(dir.join("owner.json"));
        }
        // A concurrent fork can keep cloned descriptors alive until exec; closing ours is not enough.
        release(&self._level, &self._slots);
        if let Some(log) = log {
            // Removing this holder's unique log wakes waiters while followers retain their open handles.
            let _ = fs::remove_file(&log.path);
        }
    }
}

#[derive(Debug)]
struct LogWriter {
    file: File,
    path: PathBuf,
    /// Bytes accepted so far, written or still pending.
    written: u64,
    truncated: bool,
    /// Whole records waiting for the next flush.
    pending: Vec<u8>,
    last_flush: Option<Instant>,
    queue: PathBuf,
    last_scan: Option<Instant>,
    /// Sticky: once someone follows this build, use the shorter flush interval.
    follower: bool,
}

impl LogWriter {
    fn create(dir: &Path, queue: PathBuf, name: &str, mode: Option<Mode>) -> io::Result<LogWriter> {
        let path = dir.join(name);
        let mut options = File::options();
        options.append(true).create_new(true);
        sys::with_mode(&mut options, mode.map_or(0o644, |m| m.file));
        let file = options.open(&path)?;
        if let Some(m) = mode {
            let _ = sys::set_file_mode(&file, m.file);
        }
        Ok(LogWriter {
            file,
            path,
            written: 0,
            truncated: false,
            pending: Vec::new(),
            last_flush: None,
            queue,
            last_scan: None,
            follower: false,
        })
    }

    fn append(&mut self, record: &Record) {
        let output = matches!(record, Record::Out { .. } | Record::Err { .. });
        if output && self.truncated {
            return;
        }
        let Ok(mut line) = serde_json::to_vec(record) else { return };
        line.push(b'\n');
        if output && self.written + line.len() as u64 > LOG_CAP {
            self.truncated = true;
            self.queue_line(b"{\"k\":\"trunc\"}\n");
            self.flush();
            return;
        }
        self.queue_line(&line);
        // A follower needs the start record to know whether the log is complete.
        if matches!(record, Record::Start { .. }) {
            self.flush();
        } else {
            self.flush_if_due();
        }
    }

    fn end(&mut self, code: i32, interrupted: bool) {
        if let Ok(mut line) = serde_json::to_vec(&Record::End { code, interrupted }) {
            line.push(b'\n');
            self.queue_line(&line);
        }
        self.flush();
    }

    fn queue_line(&mut self, line: &[u8]) {
        self.pending.extend_from_slice(line);
        self.written += line.len() as u64;
    }

    fn flush_if_due(&mut self) {
        self.flush_if_due_at(Instant::now());
    }

    fn flush_if_due_at(&mut self, now: Instant) {
        if self.pending.is_empty() {
            return;
        }
        if !self.follower && self.last_scan.is_none_or(|t| now.duration_since(t) >= FOLLOWER_SCAN_EVERY) {
            self.last_scan = Some(now);
            self.follower = has_follower(&self.queue, std::process::id());
        }
        let interval = if self.follower { FOLLOWER_FLUSH_EVERY } else { LOG_FLUSH_EVERY };
        let aged = self.last_flush.is_none_or(|t| now.duration_since(t) >= interval);
        if aged || self.pending.len() >= LOG_FLUSH_BYTES {
            self.flush();
        }
    }

    /// Only whole records are ever pending, and they go out in one `write` on an `O_APPEND` file; a follower buffers any partial tail itself.
    fn flush(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let _ = self.file.write_all(&self.pending);
        self.pending.clear();
        self.last_flush = Some(Instant::now());
    }
}

/// Reads tickets without their flocks: a stale ticket naming us only keeps the shorter flush interval.
fn has_follower(queue: &Path, pid: u32) -> bool {
    let Ok(entries) = fs::read_dir(queue) else { return false };
    entries.flatten().any(|entry| {
        if entry.file_name().to_string_lossy().starts_with('.') {
            return false;
        }
        fs::read(entry.path()).ok().and_then(|text| serde_json::from_slice::<TicketInfo>(&text).ok()).is_some_and(|t| t.joined == Some(pid))
    })
}

fn lock_dir(dirs: &Dirs, target_dir: &Path) -> PathBuf {
    dirs.locks().join(short_hash(target_dir.to_string_lossy().as_bytes()))
}

pub fn dir_for(dirs: &Dirs, target_dir: &Path) -> PathBuf {
    lock_dir(dirs, target_dir)
}

pub fn slot_dir_for(dirs: &Dirs, target_dir: &Path, slot: &str) -> PathBuf {
    lock_dir(dirs, target_dir).join("slots").join(slot)
}

pub fn acquire(
    dirs: &Dirs,
    target_dir: &Path,
    request: Request<'_>,
    on_event: &mut dyn FnMut(LockEvent<'_>),
) -> Result<Acquired, LockError> {
    let mode = shared_mode(dirs);
    let dir = lock_dir(dirs, target_dir);
    let queue = dir.join("queue");
    ensure_dir(&queue, mode)?;
    let level = open_lock_file(&dir.join("lock"), mode)?;
    let target_file = dir.join("target");
    if fs::read(&target_file).map(|t| t != target_dir.as_os_str().as_encoded_bytes()).unwrap_or(true) {
        let _ = write_shared(&target_file, target_dir.as_os_str().as_encoded_bytes(), mode);
    }
    let slot_dirs: Vec<PathBuf> = request.scope.names().iter().map(|n| dir.join("slots").join(n)).collect();
    let mut slot_files = Vec::with_capacity(slot_dirs.len());
    for slot in &slot_dirs {
        ensure_dir(slot, mode)?;
        slot_files.push(open_lock_file(&slot.join("lock"), mode)?);
    }
    let exclusive = matches!(request.scope, Scope::All);

    let (user, uid) = current_user();
    let me = TicketInfo {
        pid: std::process::id(),
        user: user.clone(),
        command: request.command.to_string(),
        fingerprint: request.fingerprint.to_string(),
        since_ns: now_ns(),
        scope: request.scope.stored(),
        joined: None,
    };
    // Without a ticket (unwritable queue directory) we still get mutual exclusion from the flock, just not FIFO.
    let mut ticket = match Ticket::create(&queue, &me, mode) {
        Ok(ticket) => Some(ticket),
        Err(err) if err.kind() == io::ErrorKind::PermissionDenied => None,
        Err(err) => return Err(err.into()),
    };
    let since_ns = ticket.as_ref().map(|t| t.info.since_ns).unwrap_or(me.since_ns);
    let watch = SignalWatch::start();
    let can_follow = request.join && !request.fingerprint.is_empty();

    let mut changes: Option<sys::DirectoryWatcher> = None;

    let started = Instant::now();
    let mut announced = false;
    let mut last_notify: Option<Instant> = None;
    let mut delay = FIRST_POLL;
    let mut last_ahead = usize::MAX;
    let mut follow: Option<Follow> = None;
    let mut followed: Option<Holder> = None;
    // A build we stopped following; never followed again (it may linger for a moment).
    let mut skip: Option<Holder> = None;
    loop {
        if let Some(signal) = watch.interrupted() {
            return Err(LockError::Interrupted(signal));
        }

        if follow.is_none() && followed.is_none() && can_follow
            && let Some(f) = Follow::attach(&slot_dirs[0], request.fingerprint, skip.as_ref())
        {
            if let Some(t) = ticket.as_mut() {
                t.set_joined(Some(f.holder.owner.pid));
            }
            on_event(LockEvent::Joined(&f.holder));
            follow = Some(f);
        }
        if let Some(f) = follow.as_mut() {
            match f.step(&slot_dirs[0], on_event) {
                Step::Running => {}
                Step::Succeeded => {
                    followed = Some(f.holder.clone());
                    follow = None;
                }
                Step::Failed(code) => {
                    let holder = f.holder.clone();
                    return Ok(Acquired::Failed(SharedFailure { holder, code }));
                }
                Step::Abandoned(why) => {
                    on_event(LockEvent::Abandoned(&f.holder, why));
                    skip = Some(f.holder.clone());
                    follow = None;
                    if let Some(t) = ticket.as_mut() {
                        t.set_joined(None);
                    }
                }
            }
        }

        let mut ahead = 0;
        if follow.is_none() {
            let mine = ticket.as_ref().map(|t| t.name.as_str());
            let position = queue_position(&dir, mine, &me, since_ns);
            ahead = position.ahead;
            if position.first && try_take(&level, &slot_files, exclusive)? {
                if !queue_position(&dir, mine, &me, since_ns).first {
                    // The previous holder's final record promoted a joiner ahead of us; give the lock back.
                    release(&level, &slot_files);
                } else {
                    let lock = take_ownership(&dir, level, slot_files, slot_dirs, mode, &request, user, uid, since_ns, followed.as_ref());
                    // Withdrawn only once published as the holder, so others always see us as one or the other.
                    drop(ticket.take());
                    return Ok(Acquired::Lock(Box::new(lock)));
                }
            }
        }

        let waited = started.elapsed();
        let timed_out = follow.is_none() && request.timeout.is_some_and(|limit| waited >= limit);
        if waited >= ANNOUNCE_AFTER || follow.is_some() {
            announced = true;
        }
        let due = last_notify.is_none_or(|t| t.elapsed() >= NOTIFY_EVERY);
        if timed_out || (announced && due) {
            last_notify = Some(Instant::now());
            let waiting = match &follow {
                Some(f) => observe_holder(&slot_dirs[0], Some(f.holder.clone()), ahead, waited),
                None => observe(&dir, &request.scope.stored(), ahead, waited),
            };
            if timed_out {
                return Err(LockError::Timeout(Box::new(waiting)));
            }
            on_event(LockEvent::Waiting(&waiting));
        }
        let pause = if follow.is_some() || followed.is_some() {
            FOLLOW_POLL
        } else if ahead == 0 {
            FRONT_POLL
        } else {
            delay
        };
        match changes.as_mut() {
            Some(watcher) => {
                watcher.wait(pause);
            }
            None => {
                changes = Some(watch_lock_dirs(&dir, &queue, &slot_dirs));
                // Recheck after subscribing: a release before setup must not be lost before our first wait.
                continue;
            }
        }
        if follow.is_none() && ahead < last_ahead {
            delay = FIRST_POLL;
        } else {
            delay = (delay * 2).min(MAX_POLL);
        }
        if follow.is_none() {
            last_ahead = ahead;
        }
    }
}

fn watch_lock_dirs(dir: &Path, queue: &Path, slot_dirs: &[PathBuf]) -> sys::DirectoryWatcher {
    // Include existing sibling slots: whole-directory builds and multi-slot holders may publish there.
    let slots = dir.join("slots");
    let mut watched_dirs = vec![dir.to_path_buf(), queue.to_path_buf(), slots.clone()];
    watched_dirs.extend(slot_dirs.iter().cloned());
    if let Ok(entries) = fs::read_dir(&slots) {
        watched_dirs.extend(entries.flatten().filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir())).map(|entry| entry.path()));
    }
    watched_dirs.sort();
    watched_dirs.dedup();
    let paths: Vec<&Path> = watched_dirs.iter().map(PathBuf::as_path).collect();
    sys::DirectoryWatcher::new(&paths)
}

/// Level one is shared, or exclusive for a whole-directory build; on any refusal everything taken is released.
fn try_take(level: &File, slots: &[File], exclusive: bool) -> io::Result<bool> {
    let first = sys::try_lock(level, if exclusive { LockKind::Exclusive } else { LockKind::Shared });
    match first {
        Ok(()) => {}
        Err(fs::TryLockError::WouldBlock) => return Ok(false),
        Err(fs::TryLockError::Error(err)) => return Err(err),
    }
    for (i, slot) in slots.iter().enumerate() {
        let refused = match sys::try_lock(slot, LockKind::Exclusive) {
            Ok(()) => continue,
            Err(fs::TryLockError::WouldBlock) => None,
            Err(fs::TryLockError::Error(err)) => Some(err),
        };
        release(level, &slots[..i]);
        return match refused {
            Some(err) => Err(err),
            None => Ok(false),
        };
    }
    Ok(true)
}

fn release(level: &File, slots: &[File]) {
    for slot in slots {
        let _ = sys::unlock(slot);
    }
    let _ = sys::unlock(level);
}

#[allow(clippy::too_many_arguments)]
fn take_ownership(
    dir: &Path,
    level: File,
    slot_files: Vec<File>,
    slot_dirs: Vec<PathBuf>,
    mode: Option<Mode>,
    request: &Request<'_>,
    user: String,
    uid: u32,
    since_ns: u64,
    followed: Option<&Holder>,
) -> BuildLock {
    let records = &slot_dirs[0];
    let pid = std::process::id();
    let started_ns = now_ns();
    // A crashed predecessor may have left its progress and log behind.
    let _ = fs::remove_file(records.join("progress.json"));
    remove_logs(records);
    // Keeps the lock directory recent for `frust gc` while scoped builds hold level one shared, which its probe cannot see.
    let _ = sys::set_mtime_now(&dir.join("lock"));
    let log_name = format!("build-{pid}-{started_ns}.log");
    let log = LogWriter::create(records, dir.join("queue"), &log_name, mode).ok();
    let holder = Holder {
        owner: Owner {
            pid,
            command: request.command.to_string(),
            cwd: std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default(),
            since_unix: fsx::unix_now(),
        },
        user,
        uid: Some(uid),
        fingerprint: request.fingerprint.to_string(),
        slot: request.scope.names()[0].clone(),
        scope: request.scope.stored(),
        log: if log.is_some() { log_name } else { String::new() },
        since_ns: started_ns,
    };
    if let Ok(json) = serde_json::to_vec(&holder) {
        for slot in &slot_dirs {
            let _ = write_shared(&slot.join("owner.json"), &json, mode);
        }
    }
    let joined = read_last(records).filter(|l| l.success && l.finished_ns >= since_ns).map(|build| Joined {
        identical: !request.fingerprint.is_empty() && build.fingerprint == request.fingerprint,
        followed: followed.is_some_and(|h| h.owner.pid == build.pid),
        build,
    });
    BuildLock { _level: level, _slots: slot_files, slot_dirs, mode, holder, joined, last_publish: None, published: None, log }
}

fn remove_logs(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("build-") && name.ends_with(".log") {
            let _ = fs::remove_file(entry.path());
        }
    }
}

enum Step {
    Running,
    Succeeded,
    Failed(i32),
    Abandoned(Abandon),
}

struct Follow {
    holder: Holder,
    /// Kept open: the holder removes the file when it is done, and we read on to the end.
    file: File,
    buf: Vec<u8>,
    /// Skipping the rest of a line longer than [`MAX_RECORD`].
    skipping: bool,
    complete: bool,
    last_check: Instant,
}

impl Follow {
    fn attach(slot: &Path, fingerprint: &str, skip: Option<&Holder>) -> Option<Follow> {
        let holder = read_holder(slot)?;
        if holder.fingerprint != fingerprint || holder.log.is_empty() || skip.is_some_and(|s| s.same_build(&holder)) {
            return None;
        }
        let records = slot.parent()?.join(&holder.slot);
        if lock_held(&records) != Some(true) {
            return None;
        }
        let file = File::open(records.join(&holder.log)).ok()?;
        Some(Follow { holder, file, buf: Vec::new(), skipping: false, complete: false, last_check: Instant::now() })
    }

    fn records_dir(&self, slot: &Path) -> PathBuf {
        slot.parent().map(|p| p.join(&self.holder.slot)).unwrap_or_else(|| slot.to_path_buf())
    }

    fn step(&mut self, slot: &Path, on_event: &mut dyn FnMut(LockEvent<'_>)) -> Step {
        if let Some(end) = self.drain(on_event) {
            return end;
        }
        if self.last_check.elapsed() < FOLLOW_CHECK {
            return Step::Running;
        }
        self.last_check = Instant::now();
        let records = self.records_dir(slot);
        let gone = lock_held(&records) == Some(false) || !read_holder(&records).is_some_and(|h| h.same_build(&self.holder));
        if !gone {
            return Step::Running;
        }
        // It may have finished between our last read and the check.
        self.drain(on_event).unwrap_or(Step::Abandoned(Abandon::Died))
    }

    fn drain(&mut self, on_event: &mut dyn FnMut(LockEvent<'_>)) -> Option<Step> {
        let mut chunk = vec![0u8; TAIL_CHUNK];
        for _ in 0..TAIL_CHUNKS_PER_POLL {
            let n = match self.file.read(&mut chunk) {
                Ok(0) | Err(_) => return None,
                Ok(n) => n,
            };
            let mut rest = &chunk[..n];
            while let Some(i) = rest.iter().position(|&b| b == b'\n') {
                let piece = &rest[..i];
                rest = &rest[i + 1..];
                if self.skipping {
                    self.skipping = false;
                    self.buf.clear();
                    continue;
                }
                self.buf.extend_from_slice(piece);
                let line = std::mem::take(&mut self.buf);
                if let Some(end) = self.deliver(&line, on_event) {
                    return Some(end);
                }
            }
            if !self.skipping {
                self.buf.extend_from_slice(rest);
                if self.buf.len() > MAX_RECORD {
                    self.buf.clear();
                    self.skipping = true;
                    self.complete = false;
                }
            }
        }
        None
    }

    fn deliver(&mut self, line: &[u8], on_event: &mut dyn FnMut(LockEvent<'_>)) -> Option<Step> {
        let Ok(record) = serde_json::from_slice::<Record>(line) else { return None };
        match &record {
            Record::Start { complete, .. } => self.complete = *complete,
            Record::Trunc => self.complete = false,
            _ => {}
        }
        on_event(LockEvent::Record(&record));
        match record {
            Record::End { interrupted: true, .. } => Some(Step::Abandoned(Abandon::Interrupted)),
            Record::End { code: 0, .. } => Some(Step::Succeeded),
            Record::End { code, .. } if self.complete => Some(Step::Failed(code)),
            Record::End { .. } => Some(Step::Abandoned(Abandon::Unreplayable)),
            _ => None,
        }
    }
}

fn observe(dir: &Path, scope: &[String], ahead: usize, waited: Duration) -> Waiting {
    let slots = dir.join("slots");
    let mut candidates: Vec<PathBuf> = vec![slots.join(ALL)];
    if scope.is_empty() {
        if let Ok(entries) = fs::read_dir(&slots) {
            let mut names: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
            names.sort();
            candidates.extend(names);
        }
    } else {
        candidates.extend(scope.iter().map(|s| slots.join(s)));
    }
    // A frust version without slots keeps its owner record at the top.
    candidates.push(dir.to_path_buf());
    for candidate in &candidates {
        if let Some(holder) = read_holder(candidate) {
            return observe_holder(candidate, Some(holder), ahead, waited);
        }
    }
    Waiting { info: WaitInfo { owner: None, ahead, waited }, holder: None, progress: None }
}

fn observe_holder(slot: &Path, holder: Option<Holder>, ahead: usize, waited: Duration) -> Waiting {
    let progress = holder.as_ref().and_then(|h| {
        let records = match (h.slot.is_empty(), slot.parent()) {
            (false, Some(parent)) if parent.file_name().is_some_and(|n| n == "slots") => parent.join(&h.slot),
            _ => slot.to_path_buf(),
        };
        read_progress(&records).filter(|p| p.pid == h.owner.pid)
    });
    Waiting { info: WaitInfo { owner: holder.as_ref().map(|h| h.owner.clone()), ahead, waited }, holder, progress }
}

pub fn current_user() -> (String, u32) {
    static USER: OnceLock<(String, u32)> = OnceLock::new();
    USER.get_or_init(|| {
        let uid = sys::user_id();
        let name = sys::user_name().unwrap_or_else(|| format!("uid {uid}"));
        (name, uid)
    })
    .clone()
}

fn now_ns() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0)
}

fn now_ms() -> u64 {
    now_ns() / 1_000_000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TicketInfo {
    pid: u32,
    #[serde(default)]
    user: String,
    command: String,
    #[serde(default)]
    fingerprint: String,
    since_ns: u64,
    /// Empty (and absent, for older versions) is everything.
    #[serde(default)]
    scope: Vec<String>,
    #[serde(default)]
    joined: Option<u32>,
}

/// The open, flocked file is what proves we are alive to the other waiters.
struct Ticket {
    name: String,
    path: PathBuf,
    info: TicketInfo,
    file: File,
}

impl Ticket {
    fn create(queue: &Path, info: &TicketInfo, mode: Option<Mode>) -> io::Result<Ticket> {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed) % 10_000;
        // The pid stays the last `-` field so older frust versions can still parse it.
        let name = format!("{:024}.{seq:04}-{}", info.since_ns, info.pid);
        let staging = queue.join(format!(".{name}"));
        let path = queue.join(&name);
        let mut options = File::options();
        options.write(true).create_new(true);
        sys::with_mode(&mut options, mode.map_or(0o644, |m| m.file));
        let mut file = options.open(&staging)?;
        // Locked before it becomes visible, so nobody can mistake it for a dead ticket.
        let result = (|| {
            sys::lock(&file, LockKind::Exclusive)?;
            if let Some(m) = mode {
                let _ = sys::set_file_mode(&file, m.file);
            }
            file.write_all(&serde_json::to_vec(info).map_err(io::Error::other)?)?;
            sys::rename(&staging, &path)
        })();
        if let Err(err) = result {
            let _ = fs::remove_file(&staging);
            return Err(err);
        }
        Ok(Ticket { name, path, info: info.clone(), file })
    }

    /// Rewritten in place under our flock; a reader catching it half-written falls back to the pid in the name.
    fn set_joined(&mut self, pid: Option<u32>) {
        if self.info.joined == pid {
            return;
        }
        self.info.joined = pid;
        if let Ok(json) = serde_json::to_vec(&self.info) {
            let _ = self.file.set_len(0);
            let _ = sys::write_all_at(&self.file, &json, 0);
        }
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        // Remove before the file (and its flock) closes, so a listed ticket is always locked.
        let _ = fs::remove_file(&self.path);
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Position {
    ahead: usize,
    first: bool,
}

/// A waiter may jump the queue if it is identical to the build that just succeeded and queued before that build finished.
fn is_joiner(fingerprint: &str, since_ns: u64, last: Option<&Finished>) -> bool {
    last.is_some_and(|l| l.success && !fingerprint.is_empty() && l.fingerprint == fingerprint && since_ns <= l.finished_ns)
}

/// Joiners (see [`is_joiner`]) rank first, then ticket order; disjoint scopes never block each other; dead tickets are pruned on the way.
fn queue_position(dir: &Path, mine: Option<&str>, me: &TicketInfo, since_ns: u64) -> Position {
    let queue = dir.join("queue");
    let Ok(entries) = fs::read_dir(&queue) else { return Position { ahead: 0, first: true } };
    let mut lasts: Vec<(String, Option<Finished>)> = Vec::new();
    let mut last_of = |scope: &[String]| -> Option<Finished> {
        let slot = primary_slot(scope);
        if let Some((_, last)) = lasts.iter().find(|(s, _)| s == slot) {
            return last.clone();
        }
        let last = read_last(&dir.join("slots").join(slot));
        lasts.push((slot.to_string(), last.clone()));
        last
    };
    let my_rank = (!is_joiner(&me.fingerprint, since_ns, last_of(&me.scope).as_ref()), mine.unwrap_or("~").to_string());
    let mut ahead = 0;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if Some(name.as_str()) == mine {
            continue;
        }
        if name.starts_with('.') {
            prune_staging(&entry.path());
            continue;
        }
        let Some(info) = live_ticket(&entry.path(), &name) else { continue };
        let scope = info.as_ref().map(|t| t.scope.clone()).unwrap_or_default();
        if !conflicts(&scope, &me.scope) {
            continue;
        }
        let joiner = info.as_ref().is_some_and(|t| is_joiner(&t.fingerprint, t.since_ns, last_of(&t.scope).as_ref()));
        if (!joiner, name) < my_rank {
            ahead += 1;
        }
    }
    Position { ahead, first: ahead == 0 }
}

/// `None` when the ticket is dead (and removed); inner `None` for tickets written by older frust versions.
fn live_ticket(path: &Path, name: &str) -> Option<Option<TicketInfo>> {
    let legacy_alive = || name.rsplit('-').next().and_then(ticket_pid).is_some_and(sys::pid_alive);
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return None,
        // Unreadable (another user's private file): fall back to the pid in its name.
        Err(_) => return legacy_alive().then_some(None),
    };
    let mut text = Vec::new();
    let parsed = file.read_to_end(&mut text).ok().and_then(|_| serde_json::from_slice::<TicketInfo>(&text).ok());
    let alive = match &parsed {
        Some(_) => match sys::try_lock(&file, LockKind::Shared) {
            Ok(()) => false,
            Err(fs::TryLockError::WouldBlock) => true,
            Err(fs::TryLockError::Error(_)) => legacy_alive(),
        },
        None => legacy_alive(),
    };
    drop(file);
    if alive {
        Some(parsed)
    } else {
        let _ = fs::remove_file(path);
        None
    }
}

/// Pid 0 and pids above `i32::MAX` address process groups in `kill`, always look alive and would block the queue forever.
fn ticket_pid(text: &str) -> Option<u32> {
    text.parse::<u32>().ok().filter(|&pid| pid > 0 && pid <= i32::MAX as u32)
}

fn prune_staging(path: &Path) {
    let old = fs::metadata(path).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age >= STAGING_STALE);
    if !old {
        return;
    }
    if let Ok(file) = File::open(path)
        && sys::try_lock(&file, LockKind::Shared).is_ok() {
            drop(file);
            let _ = fs::remove_file(path);
        }
}

fn read_holder(dir: &Path) -> Option<Holder> {
    let holder: Holder = serde_json::from_slice(&fs::read(dir.join("owner.json")).ok()?).ok()?;
    sys::pid_alive(holder.owner.pid).then_some(holder)
}

fn read_progress(dir: &Path) -> Option<Progress> {
    serde_json::from_slice(&fs::read(dir.join("progress.json")).ok()?).ok()
}

fn read_last(dir: &Path) -> Option<Finished> {
    serde_json::from_slice(&fs::read(dir.join("last.json")).ok()?).ok()
}

/// Probed by briefly taking the lock; a waiter that loses that race simply retries on its next poll.
fn lock_held(dir: &Path) -> Option<bool> {
    let file = File::open(dir.join("lock")).ok()?;
    match sys::try_lock(&file, LockKind::Shared) {
        Ok(()) => Some(false),
        Err(fs::TryLockError::WouldBlock) => Some(true),
        Err(fs::TryLockError::Error(_)) => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Mode {
    dir: u32,
    file: u32,
}

/// Used when several OS users share the root; `None` keeps the umask defaults.
pub(crate) fn shared_mode(dirs: &Dirs) -> Option<Mode> {
    let mode = sys::stat(dirs.root()).ok()?.mode();
    if mode & 0o022 == 0 {
        return None;
    }
    let world = if mode & 0o002 != 0 { 0o002 } else { 0 };
    Some(Mode { dir: 0o2775 | world, file: 0o664 })
}

/// `create_dir_all` that applies `mode` despite the umask.
pub(crate) fn ensure_dir(path: &Path, mode: Option<Mode>) -> io::Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    let Some(mode) = mode else { return fs::create_dir_all(path) };
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        ensure_dir(parent, Some(mode))?;
    }
    match sys::create_dir_with_mode(path, mode.dir) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
        Err(err) => Err(err),
    }
}

pub(crate) fn write_shared(path: &Path, contents: &[u8], mode: Option<Mode>) -> io::Result<()> {
    let tmp = fsx::temp_sibling(path);
    let result = (|| {
        let mut options = File::options();
        options.write(true).create_new(true);
        sys::with_mode(&mut options, mode.map_or(0o666, |m| m.file));
        let mut file = match options.open(&tmp) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                if let Some(parent) = path.parent() {
                    ensure_dir(parent, mode)?;
                }
                options.open(&tmp)?
            }
            other => other?,
        };
        if let Some(m) = mode {
            let _ = sys::set_file_mode(&file, m.file);
        }
        file.write_all(contents)?;
        file.flush()?;
        sys::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// A file another user created without group write is opened read-only, which is all `flock` needs.
pub(crate) fn open_lock_file(path: &Path, mode: Option<Mode>) -> io::Result<File> {
    let mut options = File::options();
    options.read(true).write(true).create(true).truncate(false);
    sys::with_mode(&mut options, mode.map_or(0o666, |m| m.file));
    match options.open(path) {
        Ok(file) => {
            if let Some(m) = mode
                && sys::owned_by_me(path)
            {
                let _ = sys::set_file_mode(&file, m.file);
            }
            Ok(file)
        }
        Err(err) if err.kind() == io::ErrorKind::PermissionDenied => File::open(path),
        Err(err) => Err(err),
    }
}

static INTERRUPTED: AtomicI32 = AtomicI32::new(0);

struct WatchState {
    count: usize,
    saved: Vec<signal::Saved>,
}

static WATCH: Mutex<WatchState> = Mutex::new(WatchState { count: 0, saved: Vec::new() });

extern "C" fn note_signal(signal: signal::Signal) {
    INTERRUPTED.store(signal, Ordering::Relaxed);
}

/// Records SIGINT/SIGTERM/SIGHUP so a queued build can withdraw its ticket; already-ignored signals stay ignored.
struct SignalWatch;

impl SignalWatch {
    fn start() -> SignalWatch {
        let mut state = WATCH.lock().unwrap_or_else(|e| e.into_inner());
        if state.count == 0 {
            INTERRUPTED.store(0, Ordering::Relaxed);
            for sig in [signal::SIGINT, signal::SIGTERM, signal::SIGHUP] {
                if signal::disposition(sig) == signal::Disposition::Ignore {
                    continue;
                }
                if let Some(old) = signal::install(sig, note_signal) {
                    state.saved.push(old);
                }
            }
        }
        state.count += 1;
        SignalWatch
    }

    fn interrupted(&self) -> Option<i32> {
        let signal = INTERRUPTED.load(Ordering::Relaxed);
        (signal != 0).then_some(signal)
    }
}

impl Drop for SignalWatch {
    fn drop(&mut self) {
        let mut state = WATCH.lock().unwrap_or_else(|e| e.into_inner());
        state.count -= 1;
        if state.count == 0 {
            for old in state.saved.drain(..) {
                signal::restore(old);
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct LockStatus {
    pub target: String,
    pub owner: Option<Owner>,
    pub holder: Option<Holder>,
    pub progress: Option<Progress>,
    /// Counted on the first row of the target directory.
    pub waiting: usize,
    pub scope: String,
    pub joiners: Vec<String>,
}

/// Removes stale owner and progress records (holder dead, or the lock demonstrably free).
pub fn list(dirs: &Dirs) -> Vec<LockStatus> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dirs.locks()) else { return out };
    let mut lock_dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    lock_dirs.sort();
    for dir in lock_dirs {
        let target = fs::read_to_string(dir.join("target")).unwrap_or_default();
        let tickets: Vec<(String, TicketInfo)> = fs::read_dir(dir.join("queue"))
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| {
                        let name = e.file_name().to_string_lossy().into_owned();
                        if name.starts_with('.') {
                            return None;
                        }
                        let info = live_ticket(&e.path(), &name)?;
                        let pid = name.rsplit('-').next().and_then(ticket_pid).unwrap_or(0);
                        let info = info.unwrap_or(TicketInfo {
                            pid,
                            user: String::new(),
                            command: String::new(),
                            fingerprint: String::new(),
                            since_ns: 0,
                            scope: Vec::new(),
                            joined: None,
                        });
                        Some((name, info))
                    })
                    .collect()
            })
            .unwrap_or_default();

        let mut candidates: Vec<PathBuf> = fs::read_dir(dir.join("slots")).map(|rd| rd.flatten().map(|e| e.path()).collect()).unwrap_or_default();
        candidates.sort();
        candidates.push(dir.clone());
        let mut rows: Vec<LockStatus> = Vec::new();
        let mut followed_pids = Vec::new();
        for slot in candidates {
            let mut holder = read_holder(&slot);
            // A record while the lock is free is stale even when its pid has been reused.
            if slot.join("owner.json").exists() && lock_held(&slot) == Some(false) {
                let _ = fs::remove_file(slot.join("owner.json"));
                let _ = fs::remove_file(slot.join("progress.json"));
                holder = None;
            }
            let Some(holder) = holder else { continue };
            // A multi-slot build is listed once, at the slot holding its records.
            let is_records = slot == dir || slot.file_name().is_some_and(|n| *n == *holder.slot) || holder.slot.is_empty();
            if !is_records {
                continue;
            }
            let joiners: Vec<String> = tickets
                .iter()
                .filter(|(_, t)| t.joined == Some(holder.owner.pid))
                .map(|(_, t)| if t.user.is_empty() { format!("pid {}", t.pid) } else { format!("{} pid {}", t.user, t.pid) })
                .collect();
            followed_pids.push(holder.owner.pid);
            let mut progress = read_progress(&slot).filter(|p| p.pid == holder.owner.pid);
            if !joiners.is_empty() {
                progress.get_or_insert_with(|| Progress { pid: holder.owner.pid, ..Progress::default() }).joined = joiners.len() as u32;
            }
            let scope = if slot == dir { String::new() } else { stored_label(&holder.scope) };
            let shown = if scope.is_empty() { target.clone() } else { format!("{target}  [{scope}]") };
            rows.push(LockStatus { target: shown, owner: Some(holder.owner.clone()), holder: Some(holder), progress, waiting: 0, scope, joiners });
        }
        let waiting = tickets.iter().filter(|(_, t)| !t.joined.is_some_and(|pid| followed_pids.contains(&pid))).count();
        match rows.first_mut() {
            Some(first) => first.waiting = waiting,
            None if waiting > 0 => rows.push(LockStatus {
                target: target.clone(),
                owner: None,
                holder: None,
                progress: None,
                waiting,
                scope: String::new(),
                joiners: Vec::new(),
            }),
            None => {}
        }
        out.extend(rows);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex as StdMutex};
    use std::thread;

    /// Tests that install signal watchers or raise signals must not overlap.
    static SERIAL: StdMutex<()> = StdMutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn temp_dirs(tag: &str) -> (Dirs, PathBuf) {
        let root = std::env::temp_dir().join(format!("frust-lock-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        (Dirs::at(&root), root)
    }

    static ALL_SCOPE: Scope = Scope::All;

    fn debug() -> Scope {
        Scope::slots(["debug@host".to_string()])
    }

    fn req<'a>(command: &'a str, fingerprint: &'a str) -> Request<'a> {
        Request { command, fingerprint, scope: &ALL_SCOPE, join: false, timeout: None }
    }

    fn lock(result: Result<Acquired, LockError>) -> BuildLock {
        match result.unwrap() {
            Acquired::Lock(lock) => *lock,
            Acquired::Failed(f) => panic!("unexpected shared failure {f:?}"),
        }
    }

    fn take(dirs: &Dirs, target: &Path, request: Request<'_>) -> BuildLock {
        lock(acquire(dirs, target, request, &mut |_| {}))
    }

    fn ok(units: u32) -> BuildResult {
        BuildResult { code: 0, units, interrupted: false }
    }

    fn queue_len(dirs: &Dirs, target: &Path) -> usize {
        fs::read_dir(lock_dir(dirs, target).join("queue")).unwrap().count()
    }

    #[test]
    fn exclusive_and_fifo() {
        let _serial = serial();
        let (dirs, root) = temp_dirs("fifo");
        let target = root.join("target");
        let first = take(&dirs, &target, req("first", ""));

        let order = Arc::new(StdMutex::new(Vec::new()));
        let mut handles = Vec::new();
        for name in ["second", "third"] {
            let (dirs, target, order) = (dirs.clone(), target.clone(), order.clone());
            handles.push(thread::spawn(move || {
                let guard = take(&dirs, &target, req(name, ""));
                order.lock().unwrap().push(name);
                thread::sleep(Duration::from_millis(30));
                drop(guard);
            }));
            thread::sleep(Duration::from_millis(80));
        }
        assert!(order.lock().unwrap().is_empty(), "nobody may enter while the first holds the lock");
        drop(first);
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(*order.lock().unwrap(), vec!["second", "third"]);
        assert_eq!(queue_len(&dirs, &target), 0, "no ticket outlives its waiter");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scopes_overlap_only_where_cargo_would_block() {
        let _serial = serial();
        let (dirs, root) = temp_dirs("scopes");
        let target = root.join("target");
        let debug = debug();
        let release = Scope::slots(["release@host".to_string()]);
        let d = take(&dirs, &target, Request { scope: &debug, ..req("debug", "") });
        // A different profile is free at once.
        let quick = Some(Duration::from_millis(300));
        let r = take(&dirs, &target, Request { scope: &release, timeout: quick, ..req("release", "") });
        // The same profile, and the whole directory, are not.
        let same = acquire(&dirs, &target, Request { scope: &debug, timeout: quick, ..req("debug 2", "") }, &mut |_| {});
        assert!(matches!(same, Err(LockError::Timeout(_))));
        let all = acquire(&dirs, &target, Request { timeout: quick, ..req("unknown", "") }, &mut |_| {});
        assert!(matches!(all, Err(LockError::Timeout(_))), "a whole-directory build excludes scoped ones");
        drop(d);
        drop(r);
        let a = take(&dirs, &target, req("unknown", ""));
        let blocked = acquire(&dirs, &target, Request { scope: &release, timeout: quick, ..req("release", "") }, &mut |_| {});
        assert!(matches!(blocked, Err(LockError::Timeout(_))), "and scoped builds wait for a whole-directory one");
        drop(a);
        assert_eq!(queue_len(&dirs, &target), 0);
        assert_eq!(Scope::slots(Vec::new()), Scope::All);
        assert_eq!(Scope::slots(["../x".to_string()]), Scope::All);
        assert_eq!(Scope::slots(["b@host".to_string(), "a@host".to_string(), "b@host".to_string()]).label(), "a, b");
        assert!(conflicts(&[], &["x@host".into()]) && !conflicts(&["a@host".into()], &["b@host".into()]));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn disjoint_waiter_is_not_held_back_by_an_earlier_one() {
        let _serial = serial();
        let (dirs, root) = temp_dirs("disjoint");
        let target = root.join("target");
        let debug = debug();
        let release = Scope::slots(["release@host".to_string()]);
        let holder = take(&dirs, &target, Request { scope: &debug, ..req("debug", "") });
        let queued = {
            let (dirs, target, debug) = (dirs.clone(), target.clone(), debug.clone());
            thread::spawn(move || drop(take(&dirs, &target, Request { scope: &debug, ..req("debug 2", "") })))
        };
        thread::sleep(Duration::from_millis(80));
        // The debug waiter is ahead in the queue but does not overlap a release build.
        let started = Instant::now();
        drop(take(&dirs, &target, Request { scope: &release, timeout: Some(Duration::from_secs(2)), ..req("release", "") }));
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(holder);
        queued.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn times_out_reports_owner_and_withdraws_ticket() {
        let _serial = serial();
        let (dirs, root) = temp_dirs("timeout");
        let target = root.join("target");
        let _held = take(&dirs, &target, req("cargo build", "f1"));
        let request = Request { timeout: Some(Duration::from_millis(200)), ..req("other", "f2") };
        let err = acquire(&dirs, &target, request, &mut |_| {}).unwrap_err();
        match &err {
            LockError::Timeout(waiting) => {
                let holder = waiting.holder.as_ref().unwrap();
                assert_eq!(holder.owner.command, "cargo build");
                assert_eq!(holder.user, current_user().0);
                assert_eq!(waiting.info.owner.as_ref().unwrap().pid, std::process::id());
            }
            other => panic!("unexpected {other}"),
        }
        assert_eq!(err.exit_code(), 75);
        assert!(err.to_string().contains(&format!("held by {} pid {}", current_user().0, std::process::id())));
        assert_eq!(queue_len(&dirs, &target), 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn waiter_sees_holder_progress() {
        let _serial = serial();
        let (dirs, root) = temp_dirs("progress");
        let target = root.join("target");
        let scope = debug();
        let mut held = take(&dirs, &target, Request { scope: &scope, ..req("cargo build", "f") });
        let mut progress = Progress { done: 5, expected: Some(9), ..Progress::default() };
        progress.start("serde");
        held.publish(&progress);

        let seen = Arc::new(StdMutex::new(None::<Waiting>));
        let waiter = {
            let (dirs, target, seen, scope) = (dirs.clone(), target.clone(), seen.clone(), scope.clone());
            thread::spawn(move || {
                let request = Request { scope: &scope, timeout: Some(Duration::from_millis(600)), ..req("cargo build", "g") };
                acquire(&dirs, &target, request, &mut |e| {
                    if let LockEvent::Waiting(w) = e {
                        *seen.lock().unwrap() = Some(w.clone());
                    }
                })
                .map(|_| ())
            })
        };
        assert!(matches!(waiter.join().unwrap(), Err(LockError::Timeout(_))));
        let waiting = seen.lock().unwrap().clone().expect("waiter was notified");
        let shown = waiting.progress.expect("progress is visible to waiters");
        assert_eq!((shown.done, shown.expected, shown.active.clone()), (5, Some(9), vec!["serde".to_string()]));
        assert_eq!(shown.summary(), "5/~9 units, compiling serde");
        assert_eq!(waiting.holder.unwrap().label(), format!("{} pid {}", current_user().0, std::process::id()));

        drop(held);
        let slot = slot_dir_for(&dirs, &target, "debug@host");
        assert!(!slot.join("progress.json").exists() && !slot.join("owner.json").exists());
        assert!(fs::read_dir(&slot).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().ends_with(".log")), "log removed");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn identical_waiter_jumps_the_queue_and_joins() {
        let _serial = serial();
        let (dirs, root) = temp_dirs("join");
        let target = root.join("target");
        let holder = take(&dirs, &target, req("cargo build", "same"));

        let order = Arc::new(StdMutex::new(Vec::new()));
        let mut handles = Vec::new();
        for (name, fingerprint) in [("different", "other"), ("identical", "same")] {
            let (dirs, target, order) = (dirs.clone(), target.clone(), order.clone());
            handles.push(thread::spawn(move || {
                // Not following: the identical waiter queues and is promoted when the holder succeeds.
                let guard = take(&dirs, &target, req(name, fingerprint));
                let joined = guard.joined().map(|j| (j.identical, j.build.units, j.followed));
                order.lock().unwrap().push((name, joined));
                thread::sleep(Duration::from_millis(30));
                guard.finish(ok(7));
            }));
            thread::sleep(Duration::from_millis(80));
        }
        holder.finish(ok(7));
        for h in handles {
            h.join().unwrap();
        }
        let order = order.lock().unwrap().clone();
        assert_eq!(order[0], ("identical", Some((true, 7, false))), "identical command goes first and joins: {order:?}");
        assert_eq!(order[1].0, "different");
        assert_eq!(order[1].1.map(|j| j.0), Some(false), "a different command still starts from the shared artifacts");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_build_is_not_joined() {
        let _serial = serial();
        let (dirs, root) = temp_dirs("failed");
        let target = root.join("target");
        let holder = take(&dirs, &target, req("cargo build", "same"));
        let waiter = {
            let (dirs, target) = (dirs.clone(), target.clone());
            thread::spawn(move || take(&dirs, &target, req("cargo build", "same")).joined().is_some())
        };
        thread::sleep(Duration::from_millis(80));
        holder.finish(BuildResult { code: 101, units: 3, interrupted: false });
        assert!(!waiter.join().unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    /// Run a follower in a thread, collecting the records it receives and how it ended.
    fn follower(dirs: &Dirs, target: &Path, scope: &Scope) -> thread::JoinHandle<(Vec<String>, Result<String, String>)> {
        let (dirs, target, scope) = (dirs.clone(), target.to_path_buf(), scope.clone());
        thread::spawn(move || {
            let mut seen = Vec::new();
            let request = Request { scope: &scope, join: true, timeout: Some(Duration::from_secs(10)), ..req("cargo build", "same") };
            let result = acquire(&dirs, &target, request, &mut |e| match e {
                LockEvent::Joined(h) => seen.push(format!("joined {}", h.owner.pid)),
                LockEvent::Record(Record::Err { l }) => seen.push(format!("err {l}")),
                LockEvent::Record(Record::End { code, .. }) => seen.push(format!("end {code}")),
                LockEvent::Abandoned(_, why) => seen.push(format!("abandoned {why:?}")),
                _ => {}
            });
            let result = match result {
                Ok(Acquired::Lock(lock)) => {
                    let j = lock.joined().map(|j| (j.identical, j.followed));
                    Ok(format!("lock {j:?}"))
                }
                Ok(Acquired::Failed(f)) => Ok(format!("failed {}", f.code)),
                Err(e) => Err(e.to_string()),
            };
            (seen, result)
        })
    }

    fn wait_joined(dirs: &Dirs, target: &Path) {
        let queue = lock_dir(dirs, target).join("queue");
        let begun = Instant::now();
        loop {
            let joined = fs::read_dir(&queue).unwrap().flatten().any(|e| {
                fs::read(e.path()).ok().and_then(|b| serde_json::from_slice::<TicketInfo>(&b).ok()).is_some_and(|t| t.joined.is_some())
            });
            if joined {
                return;
            }
            assert!(begun.elapsed() < Duration::from_secs(5), "follower never joined");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn follower_sees_the_log_live_and_finishes_with_the_holder() {
        let _serial = serial();
        let (dirs, root) = temp_dirs("follow");
        let target = root.join("target");
        let scope = debug();
        let mut holder = take(&dirs, &target, Request { scope: &scope, ..req("cargo build", "same") });
        holder.log_start(true, true);
        holder.log_err(b"   Compiling early v1.0.0");
        let f = follower(&dirs, &target, &scope);
        wait_joined(&dirs, &target);
        let listed = list(&dirs);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].joiners.len(), 1, "{listed:?}");
        assert_eq!(listed[0].waiting, 0, "a follower is not a waiter");
        assert!(listed[0].target.ends_with("[debug]"), "{}", listed[0].target);
        holder.log_err(b"warning: unused");
        holder.finish(ok(2));
        let (seen, result) = f.join().unwrap();
        assert_eq!(seen[0], format!("joined {}", std::process::id()));
        assert_eq!(&seen[1..], ["err    Compiling early v1.0.0", "err warning: unused", "end 0"]);
        assert_eq!(result.unwrap(), "lock Some((true, true))", "it takes the lock for its no-op run, knowing it followed");
        assert_eq!(queue_len(&dirs, &target), 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn follower_reports_a_failure_with_the_holder_code() {
        let _serial = serial();
        let (dirs, root) = temp_dirs("follow-fail");
        let target = root.join("target");
        let scope = debug();
        let mut holder = take(&dirs, &target, Request { scope: &scope, ..req("cargo build", "same") });
        holder.log_start(true, true);
        let f = follower(&dirs, &target, &scope);
        wait_joined(&dirs, &target);
        holder.log_err(b"error[E0308]: mismatched types");
        holder.finish(BuildResult { code: 101, units: 1, interrupted: false });
        let (seen, result) = f.join().unwrap();
        assert!(seen.contains(&"err error[E0308]: mismatched types".to_string()), "{seen:?}");
        assert_eq!(result.unwrap(), "failed 101");
        assert_eq!(queue_len(&dirs, &target), 0);

        // A failure whose output was not captured cannot be replayed: the follower builds.
        let mut holder = take(&dirs, &target, Request { scope: &scope, ..req("cargo build", "same") });
        holder.log_start(false, false);
        let f = follower(&dirs, &target, &scope);
        wait_joined(&dirs, &target);
        holder.finish(BuildResult { code: 101, units: 1, interrupted: false });
        let (seen, result) = f.join().unwrap();
        assert!(seen.contains(&"abandoned Unreplayable".to_string()), "{seen:?}");
        assert_eq!(result.unwrap(), "lock None");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn follower_takes_over_from_a_holder_that_dies_or_is_interrupted() {
        let _serial = serial();
        let (dirs, root) = temp_dirs("follow-died");
        let target = root.join("target");
        let scope = debug();
        for interrupted in [false, true] {
            let mut holder = take(&dirs, &target, Request { scope: &scope, ..req("cargo build", "same") });
            holder.log_start(true, true);
            let f = follower(&dirs, &target, &scope);
            wait_joined(&dirs, &target);
            if interrupted {
                holder.finish(BuildResult { code: 130, units: 0, interrupted: true });
            } else {
                // Released without a final record: what a killed holder looks like.
                drop(holder);
            }
            let (seen, result) = f.join().unwrap();
            let why = if interrupted { "abandoned Interrupted" } else { "abandoned Died" };
            assert!(seen.contains(&why.to_string()), "{seen:?}");
            assert_eq!(result.unwrap(), "lock None", "it builds itself");
        }
        assert_eq!(queue_len(&dirs, &target), 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn log_is_capped_and_marks_the_truncation() {
        let _serial = serial();
        let (dirs, root) = temp_dirs("cap");
        let target = root.join("target");
        let scope = debug();
        let mut holder = take(&dirs, &target, Request { scope: &scope, ..req("cargo build", "same") });
        holder.log_start(true, true);
        let line = vec![b'x'; 64 * 1024];
        for _ in 0..(LOG_CAP / line.len() as u64 + 4) {
            holder.log_err(&line);
        }
        holder.log_cache(&Event::Cache {
            t: 1,
            result: crate::events::CacheResult::Hit,
            krate: "serde".into(),
            version: "1".into(),
            compile_ms: 1,
            saved_ms: 1,
            bytes: 1,
        });
        holder.log.as_mut().unwrap().flush();
        let path = slot_dir_for(&dirs, &target, "debug@host").join(&holder.holder().log);
        let size = fs::metadata(&path).unwrap().len();
        assert!(size <= LOG_CAP + 4096, "log stays bounded: {size}");
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches("\"k\":\"trunc\"").count(), 1);
        assert!(text.trim_end().ends_with("}}"), "cache events are still logged after the cap");
        drop(holder);
        assert!(!path.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn buffered_log_keeps_record_order_and_end_flushes_everything() {
        let (_dirs, root) = temp_dirs("logbuf");
        let queue = root.join("queue");
        fs::create_dir_all(&queue).unwrap();
        let mut log = LogWriter::create(&root, queue, "build-test.log", None).unwrap();
        log.append(&Record::Start { json: true, complete: true });
        let on_disk = || fs::read_to_string(root.join("build-test.log")).unwrap();
        assert_eq!(on_disk().lines().count(), 1, "the start record is written at once");
        for i in 0..50 {
            log.append(&Record::Out { l: format!("line {i}") });
        }
        log.append(&Record::Err { l: "oops".into() });
        log.end(0, false);
        let text = on_disk();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 53);
        for (i, line) in lines[1..51].iter().enumerate() {
            assert_eq!(*line, format!("{{\"k\":\"out\",\"l\":\"line {i}\"}}"));
        }
        assert_eq!(lines[51], "{\"k\":\"err\",\"l\":\"oops\"}");
        assert!(lines[52].contains("\"k\":\"end\""));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn follower_log_batches_for_ten_milliseconds() {
        let (_dirs, root) = temp_dirs("log-follower-batch");
        let mut log = LogWriter::create(&root, root.join("queue"), "build-test.log", None).unwrap();
        log.follower = true;
        log.append(&Record::Start { json: true, complete: true });
        let now = log.last_flush.unwrap();
        log.queue_line(b"{\"k\":\"out\",\"l\":\"first\"}\n");
        log.queue_line(b"{\"k\":\"err\",\"l\":\"second\"}\n");
        log.flush_if_due_at(now + Duration::from_millis(9));
        assert_eq!(fs::read_to_string(&log.path).unwrap().lines().count(), 1, "output stays buffered before 10ms");
        log.flush_if_due_at(now + Duration::from_millis(10));
        assert_eq!(fs::read_to_string(&log.path).unwrap().lines().count(), 3, "a quiet build flushes at 10ms");
        log.queue_line(b"{\"k\":\"out\",\"l\":\"last\"}\n");
        log.end(0, false);
        let text = fs::read_to_string(&log.path).unwrap();
        assert_eq!(text.lines().count(), 5);
        assert!(text.lines().last().unwrap().contains("\"k\":\"end\""));
        drop(log);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unfollowed_log_keeps_the_twenty_five_millisecond_interval_and_byte_cap() {
        let (_dirs, root) = temp_dirs("log-normal-batch");
        let mut log = LogWriter::create(&root, root.join("queue"), "build-test.log", None).unwrap();
        log.append(&Record::Start { json: true, complete: true });
        let now = log.last_flush.unwrap();
        log.queue_line(b"{\"k\":\"out\",\"l\":\"buffered\"}\n");
        log.flush_if_due_at(now + Duration::from_millis(24));
        assert_eq!(fs::read_to_string(&log.path).unwrap().lines().count(), 1);
        log.flush_if_due_at(now + Duration::from_millis(25));
        assert_eq!(fs::read_to_string(&log.path).unwrap().lines().count(), 2);
        let now = log.last_flush.unwrap();
        let line = format!("{{\"k\":\"out\",\"l\":\"{}\"}}\n", "x".repeat(LOG_FLUSH_BYTES));
        log.queue_line(line.as_bytes());
        log.flush_if_due_at(now);
        assert_eq!(fs::read_to_string(&log.path).unwrap().lines().count(), 3, "the byte cap flushes without waiting");
        drop(log);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dead_tickets_are_pruned() {
        let _serial = serial();
        let (dirs, root) = temp_dirs("prune");
        let target = root.join("target");
        let queue = lock_dir(&dirs, &target).join("queue");
        fs::create_dir_all(&queue).unwrap();
        // A crashed waiter with our (live) pid: only the missing flock reveals it is dead.
        let info = TicketInfo {
            pid: std::process::id(),
            user: String::new(),
            command: "x".into(),
            fingerprint: String::new(),
            since_ns: 1,
            scope: Vec::new(),
            joined: None,
        };
        let dead = queue.join(format!("{:024}.0000-{}", 1, std::process::id()));
        fs::write(&dead, serde_json::to_vec(&info).unwrap()).unwrap();
        // A legacy ticket from a pid that cannot exist.
        let legacy = queue.join(format!("{:024}-{}", 2, i32::MAX));
        fs::write(&legacy, "cargo build").unwrap();
        // Legacy tickets whose "pid" would probe a process group, which always looks alive.
        let group = queue.join(format!("{:024}-0", 3));
        fs::write(&group, "cargo build").unwrap();
        let negative = queue.join(format!("{:024}-{}", 4, u32::MAX));
        fs::write(&negative, "cargo build").unwrap();

        let guard = take(&dirs, &target, Request { timeout: Some(Duration::from_secs(5)), ..req("cargo build", "") });
        assert!(!dead.exists() && !legacy.exists() && !group.exists() && !negative.exists());
        drop(guard);
        assert_eq!(queue_len(&dirs, &target), 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn signal_while_queued_withdraws_ticket() {
        let _serial = serial();
        let (dirs, root) = temp_dirs("signal");
        let target = root.join("target");
        let _held = take(&dirs, &target, req("cargo build", ""));
        let mut raised = false;
        let err = acquire(&dirs, &target, req("waiter", ""), &mut |e| {
            if matches!(e, LockEvent::Waiting(_)) && !raised {
                raised = true;
                signal::raise(signal::SIGTERM);
            }
        })
        .unwrap_err();
        assert!(matches!(err, LockError::Interrupted(s) if s == signal::SIGTERM), "{err}");
        assert_eq!(err.exit_code(), 128 + signal::SIGTERM);
        assert_eq!(queue_len(&dirs, &target), 0);
        // The default disposition is back once nobody is queued.
        assert_eq!(signal::disposition(signal::SIGTERM), signal::Disposition::Default);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn shared_root_gets_group_writable_state() {
        let _serial = serial();
        let (dirs, root) = temp_dirs("shared");
        sys::set_path_mode(&root, 0o775).unwrap();
        let target = root.join("target");
        let guard = take(&dirs, &target, req("cargo build", ""));
        let dir = lock_dir(&dirs, &target);
        let slot = dir.join("slots").join(ALL);
        // The setgid bit is dropped by the kernel when we are not in the directory's group.
        let mode = |p: &Path| sys::stat(p).unwrap().mode() & 0o777;
        assert_eq!(mode(&dirs.locks()), 0o775);
        assert_eq!(mode(&dir.join("queue")), 0o775);
        assert_eq!(mode(&dir.join("lock")), 0o664);
        assert_eq!(mode(&slot), 0o775);
        assert_eq!(mode(&slot.join("lock")), 0o664);
        assert_eq!(mode(&slot.join("owner.json")), 0o664);
        assert_eq!(mode(&slot.join(&guard.holder().log)), 0o664);
        drop(guard);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn list_reports_holder_and_clears_stale_owner() {
        let _serial = serial();
        let (dirs, root) = temp_dirs("list");
        let target = root.join("target");
        let guard = take(&dirs, &target, req("cargo build", ""));
        let rows = list(&dirs);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].holder.as_ref().unwrap().user, current_user().0);
        assert_eq!(rows[0].scope, "whole target directory");
        drop(guard);

        // An owner record left behind while the lock is free is stale, whatever its pid.
        let slot = slot_dir_for(&dirs, &target, ALL);
        let stale = Holder {
            owner: Owner { pid: std::process::id(), command: "ghost".into(), cwd: String::new(), since_unix: 0 },
            user: "ghost".into(),
            uid: None,
            fingerprint: String::new(),
            slot: ALL.into(),
            scope: Vec::new(),
            log: String::new(),
            since_ns: 0,
        };
        fs::write(slot.join("owner.json"), serde_json::to_vec(&stale).unwrap()).unwrap();
        assert!(list(&dirs).is_empty());
        assert!(!slot.join("owner.json").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn drop_releases_locks_while_cloned_descriptors_stay_open() {
        let _serial = serial();
        for scope in [Scope::All, Scope::slots(["debug@host".to_string(), "release@host".to_string()])] {
            let (dirs, root) = temp_dirs("drop-cloned");
            let target = root.join("target");
            let guard = take(&dirs, &target, Request { scope: &scope, ..req("cargo build", "") });
            // A concurrent fork keeps the same open file descriptions alive until its exec.
            let inherited_level = guard._level.try_clone().unwrap();
            let inherited_slots: Vec<File> = guard._slots.iter().map(|file| file.try_clone().unwrap()).collect();
            let records = guard.records_dir().to_path_buf();
            let stale = guard.holder.clone();
            let mut locks: Vec<PathBuf> = guard.slot_dirs.iter().map(|dir| dir.join("lock")).collect();
            locks.push(dir_for(&dirs, &target).join("lock"));
            drop(guard);

            fs::write(records.join("owner.json"), serde_json::to_vec(&stale).unwrap()).unwrap();
            assert!(list(&dirs).is_empty(), "cloned descriptors must not keep the old holder active");
            assert!(!records.join("owner.json").exists());
            for path in locks {
                let next = open_lock_file(&path, None).unwrap();
                sys::try_lock(&next, LockKind::Exclusive).expect("dropping the guard releases every lock");
            }

            drop((inherited_level, inherited_slots));
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn progress_summary_and_bounded_active_list() {
        let mut p = Progress::default();
        assert_eq!(p.summary(), "0 units done");
        for i in 0..(ACTIVE_LIMIT + 2) {
            p.start(&format!("c{i}"));
        }
        assert_eq!(p.active.len(), ACTIVE_LIMIT);
        assert_eq!(p.more_active, 2);
        p.finish(&["c0"]);
        p.finish(&["not-listed"]);
        assert_eq!((p.active.len(), p.more_active), (ACTIVE_LIMIT - 1, 1));
        p.done = 1;
        p.active = vec!["serde-json".into()];
        p.more_active = 0;
        p.finish(&["serde_json"]);
        assert_eq!(p.summary(), "1 unit done");
        p.expected = Some(1);
        p.done = 3;
        assert_eq!(p.summary(), "3/~3 units");
        p.joined = 2;
        assert_eq!(p.summary(), "3/~3 units, 2 joined");
    }
}
