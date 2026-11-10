//! Driver-level no-op fast path: an unchanged repeat of a recorded `build` or `check` is answered without launching cargo.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::cache::depinfo;
use crate::cache::hashing::{self, Stamp};
use crate::cache::{self, TrustMode};
use crate::fsx;
use crate::lock::{self, Scope};
use crate::paths::Dirs;
use crate::sys;
use crate::ui::term;

const MAGIC: &[u8; 8] = b"FRNOOP\0\0";
pub const FORMAT: u32 = 2;
const HEADER: usize = 8 + 4 + 8;
const CHECKSUM: usize = 16;
const MAX_RECORD_BYTES: usize = 16 * 1024 * 1024;
const MAX_ENTRIES: usize = 200_000;
const MAX_RECORDS: usize = 256;
const MAX_AGE: Duration = Duration::from_secs(30 * 86_400);
const COLLECT_EVERY: Duration = Duration::from_secs(86_400);
const TEMP_GRACE: Duration = Duration::from_secs(3_600);
const PEEK_BYTES: u64 = 64 * 1024;
const MAX_CANDIDATES: usize = 8;
const AUDIT_LOG_CAP: u64 = 1024 * 1024;
const RECORD_SUFFIX: &str = ".rec";

/// Status words a build may print while its replay is still only cargo's `Finished` line.
const SILENT_STATUS: &[&str] = &["Compiling", "Checking", "Finished", "Blocking", "Downloading", "Downloaded", "Updating", "Locking", "Adding"];

/// Status lines that depend on the moment (lock contention, index or lockfile updates), never part of a replay.
const TRANSIENT_STATUS: &[&str] = &["Blocking", "Updating", "Downloading", "Downloaded", "Locking", "Adding", "Removing", "Unpacking"];

/// Variables frust sets per cargo run; a compile that observed one cannot be replayed.
const PER_RUN_VARS: &[&str] = &["FRUST_EVENTS", "CARGO_TERM_PROGRESS_WHEN", "CARGO_TERM_WIDTH", "CARGO_TERM_COLOR"];

const FLAGS: &[&str] = &[
    "--release", "-r", "--workspace", "--all", "--lib", "--bins", "--examples", "--tests", "--benches", "--all-targets",
    "--all-features", "--no-default-features", "--locked", "--offline", "--frozen", "--keep-going", "--ignore-rust-version",
];

const VALUED: &[&str] = &[
    "-p", "--package", "--exclude", "--bin", "--example", "--test", "--bench", "-F", "--features", "--profile", "--target",
    "--target-dir", "-j", "--jobs", "--manifest-path", "--config", "-C", "--color",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Off,
    On,
    Audit,
}

impl Mode {
    pub fn parse(value: Option<&str>) -> Mode {
        match value {
            Some("0" | "off" | "false" | "no") => Mode::Off,
            Some("audit") => Mode::Audit,
            _ => Mode::On,
        }
    }

    pub fn from_env() -> Mode {
        Mode::parse(env::var("FRUST_NOOP").ok().as_deref())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Out,
    Err,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Synthesised from a build that printed nothing but status lines.
    Build,
    /// Copied from a cargo run that compiled nothing.
    Noop,
}

impl Origin {
    fn label(self) -> &'static str {
        match self {
            Origin::Build => "build",
            Origin::Noop => "noop",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    /// `None` records that the path did not exist.
    pub stamp: Option<Stamp>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub key: String,
    pub display: String,
    pub cwd: String,
    pub target: String,
    pub root: String,
    pub created_unix: u64,
    pub origin: Origin,
    pub units: u32,
    pub profiles: Vec<String>,
    pub env: Vec<(String, Option<String>)>,
    pub entries: Vec<Entry>,
    pub replay: Vec<(Stream, String)>,
}

struct Writer(Vec<u8>);

impl Writer {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }

    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    fn i64(&mut self, v: i64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    fn str(&mut self, v: &str) {
        self.u32(v.len() as u32);
        self.0.extend_from_slice(v.as_bytes());
    }

    fn opt_str(&mut self, v: Option<&str>) {
        match v {
            Some(text) => {
                self.u8(1);
                self.str(text);
            }
            None => self.u8(0),
        }
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(n)?;
        let bytes = self.buf.get(self.at..end)?;
        self.at = end;
        Some(bytes)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }

    fn u32(&mut self) -> Option<u32> {
        self.take(4).map(|b| u32::from_le_bytes(b.try_into().unwrap()))
    }

    fn u64(&mut self) -> Option<u64> {
        self.take(8).map(|b| u64::from_le_bytes(b.try_into().unwrap()))
    }

    fn i64(&mut self) -> Option<i64> {
        self.take(8).map(|b| i64::from_le_bytes(b.try_into().unwrap()))
    }

    fn str(&mut self) -> Option<String> {
        let len = self.u32()? as usize;
        String::from_utf8(self.take(len)?.to_vec()).ok()
    }

    fn opt_str(&mut self) -> Option<Option<String>> {
        match self.u8()? {
            0 => Some(None),
            1 => Some(Some(self.str()?)),
            _ => None,
        }
    }

    /// A count can never exceed the bytes left, so a damaged count cannot allocate much.
    fn count(&mut self) -> Option<usize> {
        let n = self.u32()? as usize;
        (n <= self.buf.len() - self.at).then_some(n)
    }
}

pub fn encode(record: &Record) -> Vec<u8> {
    let mut head = Writer(Vec::new());
    head.str(&record.key);
    head.str(&record.display);
    head.str(&record.cwd);
    head.str(&record.target);
    head.str(&record.root);
    let mut w = Writer(Vec::new());
    w.u64(record.created_unix);
    w.u8(match record.origin {
        Origin::Build => 1,
        Origin::Noop => 2,
    });
    w.u32(record.units);
    w.u32(record.profiles.len() as u32);
    for profile in &record.profiles {
        w.str(profile);
    }
    w.u32(record.env.len() as u32);
    for (name, value) in &record.env {
        w.str(name);
        w.opt_str(value.as_deref());
    }
    w.u32(record.entries.len() as u32);
    for entry in &record.entries {
        w.str(&entry.path);
        match &entry.stamp {
            Some(s) => {
                w.u8(1);
                w.u64(s.dev);
                w.u64(s.ino);
                w.u64(s.size);
                w.i64(s.mtime_ns as i64);
                w.i64(s.ctime_ns as i64);
            }
            None => w.u8(0),
        }
    }
    w.u32(record.replay.len() as u32);
    for (stream, line) in &record.replay {
        w.u8(match stream {
            Stream::Out => 0,
            Stream::Err => 1,
        });
        w.str(line);
    }
    // The head stays plain so a prune reads the paths from a prefix; the rest, mostly paths, is LZ4-packed.
    let mut payload = head.0;
    payload.extend_from_slice(&(w.0.len() as u32).to_le_bytes());
    payload.extend_from_slice(&lz4_flex::block::compress(&w.0));
    let mut out = Vec::with_capacity(HEADER + payload.len() + CHECKSUM);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&FORMAT.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    out.extend_from_slice(&payload);
    out.extend_from_slice(&blake3::hash(&payload).as_bytes()[..CHECKSUM]);
    out
}

/// `None` for another format, a torn or damaged file, or trailing bytes.
pub fn decode(bytes: &[u8]) -> Option<Record> {
    let payload = verified_payload(bytes)?;
    let mut head = Reader { buf: payload, at: 0 };
    let key = head.str()?;
    let display = head.str()?;
    let cwd = head.str()?;
    let target = head.str()?;
    let root = head.str()?;
    let raw_len = head.u32()? as usize;
    if raw_len > MAX_RECORD_BYTES {
        return None;
    }
    let mut body = vec![0; raw_len];
    if lz4_flex::block::decompress_into(&payload[head.at..], &mut body).ok()? != raw_len {
        return None;
    }
    let mut r = Reader { buf: &body, at: 0 };
    let created_unix = r.u64()?;
    let origin = match r.u8()? {
        1 => Origin::Build,
        2 => Origin::Noop,
        _ => return None,
    };
    let units = r.u32()?;
    let mut profiles = Vec::new();
    for _ in 0..r.count()? {
        profiles.push(r.str()?);
    }
    let mut env = Vec::new();
    for _ in 0..r.count()? {
        env.push((r.str()?, r.opt_str()?));
    }
    let n = r.count()?;
    let mut entries = Vec::with_capacity(n.min(MAX_ENTRIES));
    for _ in 0..n {
        let path = r.str()?;
        let stamp = match r.u8()? {
            0 => None,
            1 => Some(Stamp {
                dev: r.u64()?,
                ino: r.u64()?,
                size: r.u64()?,
                mtime_ns: i128::from(r.i64()?),
                ctime_ns: i128::from(r.i64()?),
            }),
            _ => return None,
        };
        entries.push(Entry { path, stamp });
    }
    let mut replay = Vec::new();
    for _ in 0..r.count()? {
        let stream = match r.u8()? {
            0 => Stream::Out,
            1 => Stream::Err,
            _ => return None,
        };
        replay.push((stream, r.str()?));
    }
    (r.at == body.len()).then_some(Record { key, display, cwd, target, root, created_unix, origin, units, profiles, env, entries, replay })
}

fn verified_payload(bytes: &[u8]) -> Option<&[u8]> {
    if bytes.len() < HEADER + CHECKSUM || &bytes[..8] != MAGIC {
        return None;
    }
    if u32::from_le_bytes(bytes[8..12].try_into().unwrap()) != FORMAT {
        return None;
    }
    let len = usize::try_from(u64::from_le_bytes(bytes[12..20].try_into().unwrap())).ok()?;
    if bytes.len() != HEADER.checked_add(len)?.checked_add(CHECKSUM)? {
        return None;
    }
    let payload = &bytes[HEADER..HEADER + len];
    (blake3::hash(payload).as_bytes()[..CHECKSUM] == bytes[HEADER + len..]).then_some(payload)
}

/// The workspace root and target a record names, read from its head without the checksum; only used to discard records.
fn peek_paths(bytes: &[u8]) -> Option<(String, String)> {
    if bytes.len() < HEADER || &bytes[..8] != MAGIC || u32::from_le_bytes(bytes[8..12].try_into().unwrap()) != FORMAT {
        return None;
    }
    let mut r = Reader { buf: &bytes[HEADER..], at: 0 };
    let (_key, _display, _cwd) = (r.str()?, r.str()?, r.str()?);
    let target = r.str()?;
    let root = r.str()?;
    Some((root, target))
}

/// Cargo's own elapsed-time format, so a replayed `Finished` line reads like a fresh one.
fn elapsed_text(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs >= 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{}.{:02}s", secs, elapsed.subsec_nanos() / 10_000_000)
    }
}

fn is_elapsed(text: &str) -> bool {
    let Some(body) = text.strip_suffix('s') else { return false };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    match body.split_once("m ") {
        Some((minutes, seconds)) => digits(minutes) && seconds.len() == 2 && digits(seconds),
        None => body.split_once('.').is_some_and(|(whole, frac)| digits(whole) && frac.len() == 2 && digits(frac)),
    }
}

fn finished_split(line: &str) -> Option<(&str, &str)> {
    let plain = term::strip_ansi(line);
    if !plain.trim_start().starts_with("Finished ") {
        return None;
    }
    line.rsplit_once(" in ").filter(|(_, elapsed)| is_elapsed(elapsed))
}

/// Rewrites the elapsed time of a `Finished` status line; any other line is returned unchanged.
pub fn retime(line: &str, elapsed: Duration) -> String {
    match finished_split(line) {
        Some((head, _)) => format!("{head} in {}", elapsed_text(elapsed)),
        None => line.to_string(),
    }
}

/// The leading word of an indented cargo status line such as `   Compiling x`.
fn status_word(line: &str) -> Option<String> {
    let plain = term::strip_ansi(line);
    let body = plain.trim_start();
    let word = body.split(' ').next().unwrap_or("");
    (body.len() != plain.len() && word.chars().next().is_some_and(|c| c.is_ascii_uppercase())).then(|| word.to_string())
}

/// A cargo status line a synthesised record may stand in for; any other stderr line keeps a build from being recorded as `diagnostics`.
pub fn silent_status(line: &str) -> bool {
    status_word(line).is_some_and(|word| SILENT_STATUS.contains(&word.as_str()))
}

fn normalised(line: &str) -> String {
    match finished_split(line) {
        Some((head, _)) => format!("{head} in <t>"),
        None => line.to_string(),
    }
}

/// Only flags whose effect is fully captured by the recorded inputs; anything unknown keeps cargo in charge.
pub fn eligible_args(args: &[OsString]) -> bool {
    let mut seen_sub = false;
    let mut i = 0;
    while i < args.len() {
        let Some(text) = args[i].to_str() else { return false };
        i += 1;
        if !seen_sub && text.starts_with('+') {
            continue;
        }
        if !text.starts_with('-') {
            if seen_sub || !matches!(text, "build" | "b" | "check" | "c") {
                return false;
            }
            seen_sub = true;
            continue;
        }
        if FLAGS.contains(&text) {
            continue;
        }
        if VALUED.contains(&text) {
            match args.get(i).and_then(|v| v.to_str()) {
                Some(value) if !value.starts_with('-') => i += 1,
                _ => return false,
            }
            continue;
        }
        let attached = VALUED.iter().any(|flag| {
            text.strip_prefix(flag).is_some_and(|rest| if flag.starts_with("--") { rest.starts_with('=') } else { !rest.is_empty() })
        });
        if !attached {
            return false;
        }
    }
    seen_sub
}

fn truthy(value: &OsStr) -> bool {
    !matches!(value.to_str(), Some("" | "0" | "false" | "no" | "off"))
}

/// What shapes cargo's stderr bytes selects a separate record; cargo always writes to a pipe here, so only forced colour and terminal settings matter.
fn display_key(forced_colour: bool) -> String {
    let mut key = format!("v1|colour={forced_colour}");
    let mut vars: Vec<(String, String)> = env::vars_os()
        .filter_map(|(k, v)| {
            let k = k.into_string().ok()?;
            let relevant = (k.starts_with("CARGO_TERM_") && k != "CARGO_TERM_WIDTH")
                || matches!(k.as_str(), "NO_COLOR" | "CLICOLOR" | "CLICOLOR_FORCE" | "TERM");
            relevant.then(|| (k, v.to_string_lossy().into_owned()))
        })
        .collect();
    vars.sort();
    for (k, v) in vars {
        key.push_str(&format!("|{k}={v}"));
    }
    key
}

fn record_name(fingerprint: &str, display: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    for part in [fingerprint, display] {
        hasher.update(&(part.len() as u64).to_le_bytes());
        hasher.update(part.as_bytes());
    }
    format!("{}{RECORD_SUFFIX}", &hasher.finalize().to_hex()[..32])
}

pub fn records_dir(dirs: &Dirs) -> PathBuf {
    dirs.root().join("noop")
}

/// Output of the cargo run, in arrival order, for recording and audit.
#[derive(Debug, Default)]
pub struct Recorder {
    lines: Vec<(Stream, String)>,
    bytes: usize,
    refused: Option<&'static str>,
}

impl Recorder {
    pub fn take(&mut self, out: &[Vec<u8>], err: &[Vec<u8>]) {
        for (stream, list) in [(Stream::Out, out), (Stream::Err, err)] {
            for line in list {
                self.push(stream, line);
            }
        }
    }

    /// Everything cargo printed, unless recording was refused.
    pub fn lines(&self) -> Option<&[(Stream, String)]> {
        self.refused.is_none().then_some(self.lines.as_slice())
    }

    pub fn refuse(&mut self, why: &'static str) {
        self.refused.get_or_insert(why);
        self.lines.clear();
    }

    fn push(&mut self, stream: Stream, line: &[u8]) {
        if self.refused.is_some() {
            return;
        }
        self.bytes += line.len() + 1;
        if self.bytes > MAX_RECORD_BYTES {
            return self.refuse("output too large");
        }
        match std::str::from_utf8(line) {
            Ok(text) if text.contains('\r') => self.refuse("progress output"),
            Ok(text) if stream == Stream::Out && !text.trim_start().starts_with('{') && !text.trim().is_empty() => self.refuse("plain stdout"),
            Ok(text) => self.lines.push((stream, text.to_string())),
            Err(_) => self.refuse("non-UTF-8 output"),
        }
    }
}

pub struct Invocation<'a> {
    pub dirs: &'a Dirs,
    pub compile: bool,
    pub args: &'a [OsString],
    pub fingerprint: &'a str,
    pub cwd: &'a Path,
    pub root: Option<&'a Path>,
    pub target: &'a Path,
    pub scope: &'a Scope,
    /// `None` for cargo's own output, otherwise the display style.
    pub style: Option<&'a str>,
    pub colour: bool,
    pub locking: bool,
    pub env_vars: &'a [(OsString, OsString)],
    pub configs: Vec<PathBuf>,
    pub debug: bool,
}

pub struct Gate {
    mode: Mode,
    ineligible: Option<&'static str>,
    observable: bool,
    dirs: Dirs,
    key: String,
    display: String,
    path: PathBuf,
    cwd: PathBuf,
    root: PathBuf,
    target: PathBuf,
    scope: Scope,
    capturable: bool,
    env_vars: Vec<(OsString, OsString)>,
    configs: Vec<PathBuf>,
    predicted: Option<Record>,
    owed: Vec<(String, String)>,
    debug: bool,
    decision: String,
    detail: String,
    recorded: String,
    exec: Option<(String, String)>,
}

impl Gate {
    pub fn new(inv: Invocation<'_>) -> Gate {
        let mode = Mode::from_env();
        let stderr_tty = io::stderr().is_terminal();
        let display = display_key(inv.style == Some("ansi") && inv.colour);
        let ineligible = ineligibility(&inv, mode, stderr_tty);
        let capturable = inv.style.is_some() || (inv.locking && !stderr_tty);
        Gate {
            mode,
            ineligible,
            observable: ineligibility(&inv, Mode::On, stderr_tty).is_none() && capturable,
            dirs: inv.dirs.clone(),
            key: inv.fingerprint.to_string(),
            path: records_dir(inv.dirs).join(record_name(inv.fingerprint, &display)),
            display,
            cwd: inv.cwd.to_path_buf(),
            root: inv.root.map(Path::to_path_buf).unwrap_or_default(),
            target: inv.target.to_path_buf(),
            scope: inv.scope.clone(),
            capturable: inv.style.is_some() || (inv.locking && !stderr_tty),
            env_vars: inv.env_vars.to_vec(),
            configs: inv.configs,
            predicted: None,
            owed: Vec::new(),
            debug: inv.debug,
            decision: String::new(),
            detail: String::new(),
            recorded: String::new(),
            exec: None,
        }
    }

    pub fn record_path(&self) -> &Path {
        &self.path
    }

    /// A pending audit needs cargo itself to run.
    pub fn auditing(&self) -> bool {
        self.predicted.is_some()
    }

    /// The executor's decision and plan outcome, reported in the same trace line.
    pub fn set_exec(&mut self, trace: (String, String)) {
        self.exec = Some(trace);
    }

    /// Whether the cargo run should be observed for recording or audit.
    pub fn wants_capture(&self) -> bool {
        self.ineligible.is_none() && self.capturable
    }

    /// The command could be recorded whether or not this fast path is switched on; the executor relies on the same observation.
    pub fn observable(&self) -> bool {
        self.observable
    }

    /// The replay when nothing changed and the fast path may answer; in audit mode the verdict is kept for [`Gate::finish`].
    pub fn check(&mut self) -> Option<Vec<(Stream, String)>> {
        if let Some(why) = self.ineligible {
            self.decision = why.to_string();
            return None;
        }
        if self.mode == Mode::Audit && !self.capturable {
            self.decision = "miss:audit-unobservable".into();
            return None;
        }
        match self.verify() {
            Ok(record) if self.mode == Mode::Audit => {
                self.decision = "audit:pending".into();
                self.owed = crate::exec::owed(&self.dirs, &self.path);
                self.predicted = Some(record);
                None
            }
            Ok(record) => {
                self.decision = "fast".into();
                Some(record.replay)
            }
            Err(why) => {
                self.decision = format!("miss:{why}");
                if self.debug {
                    eprintln!("frust: no-op fast path declined: {why} {}", self.detail);
                }
                None
            }
        }
    }

    fn verify(&mut self) -> Result<Record, &'static str> {
        let bytes = read_capped(&self.path).map_err(|e| if e.kind() == io::ErrorKind::NotFound { "none" } else { "unreadable" })?;
        let record = decode(&bytes).ok_or("corrupt")?;
        if record.key != self.key || record.display != self.display || Some(record.cwd.as_str()) != self.cwd.to_str()
            || Some(record.target.as_str()) != self.target.to_str()
        {
            return Err("key");
        }
        for (name, value) in &record.env {
            if effective(&self.env_vars, name).map(|v| v.into_string()) != value.clone().map(Ok) {
                self.detail = name.clone();
                return Err("env");
            }
        }
        if build_in_flight(&self.dirs, &self.target, &self.scope) {
            return Err("busy");
        }
        let mut held = Vec::new();
        for profile in &record.profiles {
            held.push(crate::compact::ProfileLock::try_acquire(Path::new(profile)).ok_or("busy")?);
        }
        let mut devs = HashSet::new();
        for entry in &record.entries {
            let same = match (sys::stat(Path::new(&entry.path)), &entry.stamp) {
                (Ok(meta), Some(stamp)) => {
                    let current = Stamp::of(&meta);
                    devs.insert(current.dev);
                    current == *stamp
                }
                (Err(e), None) => e.kind() == io::ErrorKind::NotFound,
                _ => false,
            };
            if !same {
                self.detail = entry.path.clone();
                return Err("changed");
            }
        }
        if devs.iter().any(|dev| cache::quarantined(&self.dirs, *dev)) {
            return Err("quarantined");
        }
        drop(held);
        Ok(record)
    }

    /// Records after a successful run and settles an audit; returns an audit failure to report.
    pub fn finish(&mut self, code: i32, interrupted: bool, launched_ns: i128, recorder: Option<&Recorder>) -> Option<String> {
        let mut failure = None;
        if let Some(predicted) = self.predicted.take() {
            failure = audit(&predicted, code, recorder, &self.owed);
            self.decision = if failure.is_some() { "audit:failed".into() } else { "audit:ok".into() };
            if let Some(text) = &failure {
                self.log_audit(text);
            }
        }
        self.recorded = match self.record(code, interrupted, launched_ns, recorder) {
            Ok(origin) => origin.label().to_string(),
            Err(why) => format!("skip:{why}"),
        };
        failure
    }

    fn record(&self, code: i32, interrupted: bool, launched_ns: i128, recorder: Option<&Recorder>) -> Result<Origin, &'static str> {
        if let Some(why) = self.ineligible {
            return Err(why);
        }
        if code != 0 || interrupted {
            return Err("failed");
        }
        let built = recorder.ok_or("uncaptured").and_then(|recorder| self.build_record(recorder, launched_ns));
        match built {
            Ok(record) => {
                let bytes = encode(&record);
                if bytes.len() > MAX_RECORD_BYTES {
                    let _ = fs::remove_file(&self.path);
                    return Err("record too large");
                }
                fsx::write_atomic(&self.path, &bytes).map_err(|_| "unwritable")?;
                prune(&records_dir(&self.dirs), Some(&self.path));
                Ok(record.origin)
            }
            Err(why) => {
                // Cargo just ran, so an older record of this command no longer describes the target.
                let _ = fs::remove_file(&self.path);
                Err(why)
            }
        }
    }

    fn build_record(&self, recorder: &Recorder, launched_ns: i128) -> Result<Record, &'static str> {
        if let Some(why) = recorder.refused {
            return Err(why);
        }
        let mut artifacts = Vec::new();
        let mut scripts = Vec::new();
        let mut finished = false;
        for (stream, line) in &recorder.lines {
            if *stream != Stream::Out || line.trim().is_empty() {
                continue;
            }
            let msg: serde_json::Value = serde_json::from_str(line).map_err(|_| "unparsable stdout")?;
            match msg.get("reason").and_then(|r| r.as_str()) {
                Some("compiler-artifact") => artifacts.push(msg),
                Some("build-script-executed") => scripts.push(msg),
                Some("build-finished") => finished = msg.get("success").and_then(|s| s.as_bool()) == Some(true),
                _ => {}
            }
        }
        if artifacts.is_empty() || !finished {
            return Err("no units");
        }
        let all_fresh = artifacts.iter().all(|a| a.get("fresh").and_then(|f| f.as_bool()) == Some(true));
        let (origin, replay) = if all_fresh {
            let transient = recorder.lines.iter().any(|(s, l)| *s == Stream::Err && status_word(l).is_some_and(|w| TRANSIENT_STATUS.contains(&w.as_str())));
            if transient {
                return Err("transient status");
            }
            let replay = recorder.lines.clone();
            (Origin::Noop, replay)
        } else {
            (Origin::Build, self.synthesise(recorder)?)
        };
        if self.root.as_os_str().is_empty() {
            return Err("no workspace");
        }
        let mut c = Collector::new(self, launched_ns);
        c.collect(&artifacts, &scripts)?;
        if c.inputs.len() + c.targets.len() > MAX_ENTRIES {
            return Err("too many inputs");
        }
        let text = |p: &Path| p.to_str().map(str::to_string).ok_or("non-UTF-8 path");
        let mut entries = c.inputs;
        entries.append(&mut c.targets);
        Ok(Record {
            key: self.key.clone(),
            display: self.display.clone(),
            cwd: text(&self.cwd)?,
            target: text(&self.target)?,
            root: text(&self.root)?,
            created_unix: fsx::unix_now(),
            origin,
            units: artifacts.len() as u32,
            profiles: c.profiles.iter().map(|p| text(p)).collect::<Result<_, _>>()?,
            env: c.env,
            entries,
            replay,
        })
    }

    /// A build that printed only status lines replays as cargo's no-op does: the `Finished` line alone.
    fn synthesise(&self, recorder: &Recorder) -> Result<Vec<(Stream, String)>, &'static str> {
        let mut finished = None;
        for (stream, line) in &recorder.lines {
            if *stream != Stream::Err {
                continue;
            }
            if !silent_status(line) {
                return Err("diagnostics");
            }
            let word = status_word(line).unwrap_or_default();
            if finished.is_some() {
                return Err("output after Finished");
            }
            if word == "Finished" {
                finished = Some(line.clone());
            }
        }
        let finished = finished.ok_or("no Finished line")?;
        // Cargo's JSON is kept, marked fresh, so a display-enabled build can replay the units too.
        let mut replay = Vec::new();
        for (stream, line) in &recorder.lines {
            if *stream != Stream::Out || line.trim().is_empty() {
                continue;
            }
            let mut msg: serde_json::Value = serde_json::from_str(line).map_err(|_| "unparsable stdout")?;
            if msg.get("reason").and_then(|r| r.as_str()) == Some("compiler-artifact") {
                msg["fresh"] = serde_json::Value::Bool(true);
            }
            replay.push((Stream::Out, msg.to_string()));
        }
        replay.push((Stream::Err, finished));
        Ok(replay)
    }

    fn log_audit(&self, text: &str) {
        let log = records_dir(&self.dirs).join("audit.jsonl");
        if fs::metadata(&log).is_ok_and(|m| m.len() > AUDIT_LOG_CAP) {
            return;
        }
        let line = serde_json::json!({ "t": fsx::unix_now(), "key": self.key, "cwd": self.cwd.to_string_lossy(), "failure": text });
        let _ = fs::create_dir_all(records_dir(&self.dirs));
        append_line(&log, &line.to_string());
    }

    /// One JSON line per decision when `FRUST_NOOP_TRACE` names a file.
    pub fn trace(&self) {
        let Some(path) = env::var_os("FRUST_NOOP_TRACE").filter(|p| !p.is_empty()) else { return };
        let decision = if self.decision.is_empty() { "unchecked" } else { &self.decision };
        let recorded = if self.recorded.is_empty() { "none" } else { &self.recorded };
        let mut line = serde_json::json!({ "t": crate::events::now_ms(), "noop": decision, "record": recorded, "detail": self.detail });
        if let Some((exec, plan)) = &self.exec {
            line["exec"] = serde_json::Value::from(exec.as_str());
            line["plan"] = serde_json::Value::from(plan.as_str());
        }
        append_line(Path::new(&path), &line.to_string());
    }
}

fn ineligibility(inv: &Invocation<'_>, mode: Mode, stderr_tty: bool) -> Option<&'static str> {
    let checks: [(bool, &'static str); 7] = [
        (mode == Mode::Off, "off"),
        (!inv.compile || !eligible_args(inv.args), "ineligible:command"),
        (TrustMode::from_env() == TrustMode::Off, "ineligible:trust"),
        (inv.root.is_none(), "ineligible:workspace"),
        (inv.style.is_none() && stderr_tty, "ineligible:terminal"),
        (["CARGO_TERM_VERBOSE", "CARGO_TERM_QUIET"].iter().any(|v| env::var_os(v).is_some_and(|x| truthy(&x))), "ineligible:verbosity"),
        (env::var_os("CARGO_TERM_PROGRESS_WHEN").is_some_and(|v| v == "always") || env::var_os("CARGO_LOG").is_some(), "ineligible:logging"),
    ];
    checks.iter().find(|(hit, _)| *hit).map(|(_, why)| *why)
}

fn effective(env_vars: &[(OsString, OsString)], name: &str) -> Option<OsString> {
    match env_vars.iter().rev().find(|(k, _)| k == name) {
        Some((_, v)) => Some(v.clone()),
        None => env::var_os(name),
    }
}

fn per_run(name: &str) -> bool {
    PER_RUN_VARS.contains(&name) || name == crate::store_broker::CAPABILITY_ENV || name == crate::wrapper::SLOT_SUPERVISOR_ENV
}

/// `owed` lists units the executor rebuilt after removing their fingerprints: cargo recompiles them through the cache without any input having changed.
fn audit(predicted: &Record, code: i32, recorder: Option<&Recorder>, owed: &[(String, String)]) -> Option<String> {
    if code != 0 {
        return Some(format!("no-op audit failed: cargo exited with {code} where the fast path predicted an unchanged build"));
    }
    let Some(recorder) = recorder.filter(|r| r.refused.is_none()) else {
        return Some("no-op audit failed: cargo's output could not be observed".into());
    };
    let mut rebuilt = Vec::new();
    for (stream, line) in &recorder.lines {
        if *stream != Stream::Out {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if msg.get("reason").and_then(|r| r.as_str()) == Some("compiler-artifact")
            && msg.get("fresh").and_then(|f| f.as_bool()) != Some(true)
            && !owed.iter().any(|(key, _)| *key == crate::exec::unit_key(&msg))
        {
            rebuilt.push(msg.pointer("/target/name").and_then(|n| n.as_str()).unwrap_or("?").to_string());
        }
    }
    if !rebuilt.is_empty() {
        return Some(format!("no-op audit failed: cargo rebuilt {} while the fast path would have skipped the build", rebuilt.join(", ")));
    }
    let owed_status = |l: &str| {
        let plain = term::strip_ansi(l);
        plain.trim_start().strip_prefix("Compiling ").is_some_and(|rest| owed.iter().any(|(_, package)| package == rest))
    };
    let replayable = |(s, l): &&(Stream, String)| *s == Stream::Err && !status_word(l).is_some_and(|w| TRANSIENT_STATUS.contains(&w.as_str())) && !owed_status(l);
    let lines = |list: &mut dyn Iterator<Item = &(Stream, String)>| list.filter(replayable).map(|(_, l)| normalised(l)).collect::<Vec<_>>();
    let expected = lines(&mut predicted.replay.iter());
    let actual = lines(&mut recorder.lines.iter());
    if expected != actual {
        return Some(format!("no-op audit failed: the replay {expected:?} differs from cargo's output {actual:?}"));
    }
    None
}

fn append_line(path: &Path, line: &str) {
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(format!("{line}\n").as_bytes());
    }
}

fn read_capped(path: &Path) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?.take(MAX_RECORD_BYTES as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(io::Error::other("record too large"));
    }
    Ok(bytes)
}

/// A file compaction replaced with identical bytes in a new inode: its stamp just before and just after.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rewrite {
    pub path: PathBuf,
    pub before: Stamp,
    pub after: Stamp,
}

/// Caller holds the profile locks of every rewritten file. A record is rewritten only when each entry either still
/// matches or is a listed rewrite whose recorded stamp is its stamp before and whose current stamp is its stamp after;
/// any other mismatch leaves the record alone. `gated` means the caller already holds frust's target gate.
pub fn restamp(dirs: &Dirs, rewrites: &[Rewrite], gated: bool) -> usize {
    let by_path: HashMap<&str, &Rewrite> = rewrites.iter().filter_map(|r| Some((r.path.to_str()?, r))).collect();
    if by_path.is_empty() {
        return 0;
    }
    crate::exec::restamp(dirs, rewrites, gated);
    let Ok(entries) = fs::read_dir(records_dir(dirs)) else { return 0 };
    let mut updated = 0;
    for file in entries.flatten() {
        let path = file.path();
        if !file.file_name().to_string_lossy().ends_with(RECORD_SUFFIX) {
            continue;
        }
        let Ok(bytes) = read_capped(&path) else { continue };
        let Some((_, target)) = peek_paths(&bytes) else { continue };
        if !by_path.keys().any(|p| within(p, &target, cfg!(windows))) {
            continue;
        }
        let Some(mut record) = decode(&bytes) else { continue };
        let mut touched = false;
        let mut valid = true;
        for entry in &mut record.entries {
            let current = match sys::stat(Path::new(&entry.path)) {
                Ok(meta) => Some(Stamp::of(&meta)),
                Err(e) if e.kind() == io::ErrorKind::NotFound => None,
                Err(_) => {
                    valid = false;
                    break;
                }
            };
            if current == entry.stamp {
                continue;
            }
            match (by_path.get(entry.path.as_str()), entry.stamp, current) {
                (Some(r), Some(old), Some(now)) if old == r.before && now == r.after => {
                    entry.stamp = Some(now);
                    touched = true;
                }
                _ => {
                    valid = false;
                    break;
                }
            }
        }
        if !valid || !touched {
            continue;
        }
        // Without the gate a build that just released Cargo's locks could be writing this record.
        let _gate = if gated {
            None
        } else {
            match lock::target_activity(dirs, Path::new(&target)) {
                Ok(None) => None,
                Ok(Some(_)) => match lock::try_target_maintenance(dirs, Path::new(&target)) {
                    Ok(Some(guard)) => Some(guard),
                    _ => continue,
                },
                Err(_) => continue,
            }
        };
        if read_capped(&path).ok().as_deref() != Some(bytes.as_slice()) {
            continue;
        }
        if fsx::write_atomic(&path, &encode(&record)).is_ok() {
            updated += 1;
        }
    }
    updated
}

/// Waiting tickets or a held build lock mean another build may be changing the target.
fn build_in_flight(dirs: &Dirs, target: &Path, scope: &Scope) -> bool {
    let dir = lock::dir_for(dirs, target);
    if let Ok(entries) = fs::read_dir(dir.join("queue"))
        && entries.flatten().any(|e| !e.file_name().to_string_lossy().starts_with('.'))
    {
        return true;
    }
    let level = match File::open(dir.join("lock")) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return false,
        Err(_) => return true,
    };
    let (level_kind, slots) = match scope {
        Scope::All => (sys::LockKind::Exclusive, Vec::new()),
        Scope::Slots(list) => (sys::LockKind::Shared, list.clone()),
    };
    if sys::try_lock(&level, level_kind).is_err() {
        return true;
    }
    let mut held = Vec::new();
    let mut busy = false;
    for slot in slots {
        match File::open(dir.join("slots").join(&slot).join("lock")) {
            Ok(file) if sys::try_lock(&file, sys::LockKind::Exclusive).is_ok() => held.push(file),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            _ => {
                busy = true;
                break;
            }
        }
    }
    for file in held.iter().chain([&level]) {
        let _ = sys::unlock(file);
    }
    busy
}

/// The lexical components of a path as written by cargo, rustc or a dep-info file; Windows forms unify separators, drop the verbatim prefix and fold case.
fn components(path: &str, windows: bool) -> Vec<String> {
    let mut text = if windows { path.replace('\\', "/") } else { path.to_string() };
    if windows {
        for (prefix, keep) in [("//?/UNC/", "//"), ("//?/", ""), ("//./", "")] {
            if let Some(rest) = text.strip_prefix(prefix) {
                text = format!("{keep}{rest}");
                break;
            }
        }
        text = text.to_lowercase();
    }
    let rooted = text.starts_with('/');
    let mut parts: Vec<String> = Vec::new();
    for part in text.split('/') {
        match part {
            "" | "." => {}
            ".." if parts.last().is_some_and(|last| last != "..") && !(windows && parts.len() == 1 && parts[0].ends_with(':')) => {
                parts.pop();
            }
            ".." if rooted || (windows && parts.len() == 1 && parts[0].ends_with(':')) => {}
            other => parts.push(other.to_string()),
        }
    }
    if text.starts_with("//") && windows {
        parts.insert(0, "//".into());
    } else if rooted {
        parts.insert(0, "/".into());
    }
    parts
}

/// Whether `path` is `base` or lies below it, compared lexically.
fn within(path: &str, base: &str, windows: bool) -> bool {
    let (path, base) = (components(path, windows), components(base, windows));
    !base.is_empty() && path.starts_with(&base)
}

pub(crate) fn is_hash(text: &str) -> bool {
    text.len() == 16 && text.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `(prefix, hash)` of a name like `libfoo-0123456789abcdef.rlib`.
fn hashed_name(name: &str) -> Option<(&str, &str)> {
    let stem = name.split_once('.').map_or(name, |(stem, _)| stem);
    stem.rsplit_once('-').filter(|(_, hash)| is_hash(hash))
}

fn split_suffix(name: &str) -> (&str, &str) {
    match name.find('.') {
        Some(at) => (&name[..at], &name[at..]),
        None => (name, ""),
    }
}

/// Handles the old id form (`serde 1.0.0 (registry+...)`) and the new one (`registry+https://...#serde@1.0.0`).
pub(crate) fn package_name(id: &str) -> String {
    if let Some((head, fragment)) = id.split_once('#') {
        return match fragment.split_once('@') {
            Some((name, _)) => name.to_string(),
            None => head.rsplit('/').next().unwrap_or("").to_string(),
        };
    }
    id.split_whitespace().next().unwrap_or("").to_string()
}

pub(crate) fn fingerprint_file(msg: &serde_json::Value) -> Option<String> {
    let kind = msg.pointer("/target/kind")?.as_array()?.first()?.as_str()?;
    let kind = match kind {
        "lib" | "rlib" | "dylib" | "cdylib" | "staticlib" | "proc-macro" => "lib",
        "bin" => "bin",
        "test" => "integration-test",
        "example" => "example",
        "bench" => "bench",
        "custom-build" => "build-script",
        _ => return None,
    };
    let flavor = if msg.pointer("/profile/test").and_then(|t| t.as_bool()) == Some(true) { "test-" } else { "" };
    Some(format!("{flavor}{kind}-{}", msg.pointer("/target/name")?.as_str()?))
}

/// Where cargo keeps one unit: its profile directory, fingerprint directories and rustc dep-info files.
struct Unit {
    profile: PathBuf,
    fingerprints: Vec<PathBuf>,
    depinfos: Vec<PathBuf>,
    extra_outputs: Vec<PathBuf>,
}

/// The fingerprint files of the unit an artifact message names (a superset when ambiguous); empty when none is found.
pub(crate) fn fingerprint_files(msg: &serde_json::Value) -> Vec<PathBuf> {
    let mut outputs: Vec<PathBuf> = msg.get("filenames").and_then(|f| f.as_array()).into_iter().flatten()
        .filter_map(|f| f.as_str().map(PathBuf::from)).collect();
    if let Some(exe) = msg.get("executable").and_then(|e| e.as_str()) {
        outputs.push(PathBuf::from(exe));
    }
    outputs.sort();
    outputs.dedup();
    let Some(file) = fingerprint_file(msg) else { return Vec::new() };
    locate_unit(msg, &outputs).map(|unit| unit.fingerprints.iter().map(|dir| dir.join(&file)).collect()).unwrap_or_default()
}

fn locate_unit(msg: &serde_json::Value, outputs: &[PathBuf]) -> Result<Unit, &'static str> {
    let package = package_name(msg.get("package_id").and_then(|p| p.as_str()).unwrap_or(""));
    let crate_name = msg.pointer("/target/name").and_then(|n| n.as_str()).ok_or("unit without name")?.replace('-', "_");
    let name_of = |p: &Path| p.file_name().and_then(OsStr::to_str).map(str::to_string);
    let unit_at = |profile: &Path, depdir: &Path, hash: &str, extra: Vec<PathBuf>| {
        let fingerprint = profile.join(".fingerprint").join(format!("{package}-{hash}"));
        fingerprint.is_dir().then(|| Unit {
            profile: profile.to_path_buf(),
            fingerprints: vec![fingerprint],
            depinfos: vec![depdir.join(format!("{crate_name}-{hash}.d"))],
            extra_outputs: extra,
        })
    };
    let mut profile = None;
    for out in outputs {
        let (Some(parent), Some(name)) = (out.parent(), name_of(out)) else { continue };
        let parent_name = name_of(parent).unwrap_or_default();
        let grand = parent.parent();
        if matches!(parent_name.as_str(), "deps" | "examples") {
            if let (Some(grand), Some((_, hash))) = (grand, hashed_name(&name)) {
                if let Some(unit) = unit_at(grand, parent, hash, Vec::new()) {
                    return Ok(unit);
                }
                profile.get_or_insert_with(|| grand.to_path_buf());
            }
        } else if let Some(grand) = grand.filter(|g| name_of(g).as_deref() == Some("build"))
            && let Some((_, hash)) = parent_name.rsplit_once('-').filter(|(_, h)| is_hash(h))
            && let Some(profile_dir) = grand.parent()
        {
            let original = parent.join(format!("build_script_build-{hash}{}", env::consts::EXE_SUFFIX));
            let extra = if original.is_file() { vec![original] } else { Vec::new() };
            if let Some(unit) = unit_at(profile_dir, parent, hash, extra) {
                return Ok(unit);
            }
            profile.get_or_insert_with(|| profile_dir.to_path_buf());
        }
    }
    // Uplifted outputs are copies or links of the hashed original, which keeps its size and modification time.
    let mut unhashed = Vec::new();
    for out in outputs {
        let (Some(dir), Some(name)) = (out.parent(), name_of(out)) else { continue };
        let profile_dir = if name_of(dir).as_deref() == Some("examples") { dir.parent().unwrap_or(dir) } else { dir };
        profile.get_or_insert_with(|| profile_dir.to_path_buf());
        let Ok(meta) = sys::stat(out) else { continue };
        let want = Stamp::of(&meta);
        let (stem, suffix) = split_suffix(&name);
        let stem_as_named = stem;
        let stem = stem.replace('-', "_");
        if let [(hash, original)] = fingerprinted_originals(profile_dir, dir, &package, [stem.as_str(), stem_as_named], suffix, &want).as_slice()
            && let Some(unit) = unit_at(profile_dir, original.parent().unwrap_or(dir), hash, vec![original.clone()])
        {
            return Ok(unit);
        }
        let mut found = Vec::new();
        for search in [profile_dir.join("deps"), dir.to_path_buf()] {
            let Ok(entries) = fs::read_dir(&search) else { continue };
            for entry in entries.flatten() {
                let Some(candidate) = entry.file_name().to_str().map(str::to_string) else { continue };
                let Some(body) = candidate.strip_suffix(suffix).filter(|_| candidate.len() > name.len()) else { continue };
                let Some((prefix, hash)) = body.rsplit_once('-').filter(|(_, h)| is_hash(h)) else { continue };
                if prefix.replace('-', "_") != stem {
                    continue;
                }
                let same = sys::stat(&entry.path()).map(|m| Stamp::of(&m)).is_ok_and(|s| s.size == want.size && s.mtime_ns == want.mtime_ns);
                if same && !found.iter().any(|(h, _): &(String, PathBuf)| h == hash) {
                    found.push((hash.to_string(), entry.path()));
                }
            }
        }
        if let [(hash, original)] = found.as_slice()
            && let Some(unit) = unit_at(profile_dir, original.parent().unwrap_or(dir), hash, vec![original.clone()])
        {
            return Ok(unit);
        }
        // A path package's cdylib or dylib, and its executables on windows-msvc, keep cargo's name without a hash in `deps`.
        let original = profile_dir.join("deps").join(&name);
        if found.is_empty() && dir != original.parent().unwrap_or(dir) && original.is_file() {
            unhashed.push(original);
        }
    }
    let profile = profile.ok_or("unit without outputs")?;
    let mut unit = candidates(msg, &profile, &package, &crate_name)?;
    unit.extra_outputs.extend(unhashed);
    Ok(unit)
}

/// Hashed originals of an uplifted output named after the package's fingerprint hashes, found by stat alone because `deps` piles up stale objects.
fn fingerprinted_originals(profile: &Path, dir: &Path, package: &str, stems: [&str; 2], suffix: &str, want: &Stamp) -> Vec<(String, PathBuf)> {
    let Ok(entries) = fs::read_dir(profile.join(".fingerprint")) else { return Vec::new() };
    let mut found: Vec<(String, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else { continue };
        let Some((_, hash)) = name.rsplit_once('-').filter(|(p, h)| *p == package && is_hash(h)) else { continue };
        for search in [profile.join("deps"), dir.to_path_buf()] {
            for stem in stems {
                let path = search.join(format!("{stem}-{hash}{suffix}"));
                let same = sys::stat(&path).map(|m| Stamp::of(&m)).is_ok_and(|s| s.size == want.size && s.mtime_ns == want.mtime_ns);
                if same && !found.iter().any(|(h, _)| h == hash) {
                    found.push((hash.to_string(), path));
                }
            }
        }
    }
    found
}

/// Every fingerprint directory of the package holding this unit's fingerprint file; a superset only adds stamps.
fn candidates(msg: &serde_json::Value, profile: &Path, package: &str, crate_name: &str) -> Result<Unit, &'static str> {
    let file = fingerprint_file(msg).ok_or("unknown unit kind")?;
    let kind = msg.pointer("/target/kind/0").and_then(|k| k.as_str()).unwrap_or("");
    let mut unit = Unit { profile: profile.to_path_buf(), fingerprints: Vec::new(), depinfos: Vec::new(), extra_outputs: Vec::new() };
    let entries = fs::read_dir(profile.join(".fingerprint")).map_err(|_| "no fingerprints")?;
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else { continue };
        let Some(hash) = name.rsplit_once('-').filter(|(p, h)| *p == package && is_hash(h)).map(|(_, h)| h.to_string()) else { continue };
        if !entry.path().join(&file).is_file() {
            continue;
        }
        let depdir = match kind {
            "custom-build" => profile.join("build").join(&name),
            "example" => profile.join("examples"),
            _ => profile.join("deps"),
        };
        unit.fingerprints.push(entry.path());
        let hashed = depdir.join(format!("{crate_name}-{hash}.d"));
        let plain = depdir.join(format!("{crate_name}.d"));
        let depinfo = if !hashed.exists() && plain.is_file() { plain } else { hashed };
        if !unit.depinfos.contains(&depinfo) {
            unit.depinfos.push(depinfo);
        }
    }
    if unit.fingerprints.is_empty() || unit.fingerprints.len() > MAX_CANDIDATES {
        let names: Vec<String> = fs::read_dir(profile.join(".fingerprint")).map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
        return Err(Box::leak(format!("unit not found (diag package {package:?} crate {crate_name:?} file {file:?} kind {kind:?} profile {} fingerprints {names:?} count {})", profile.display(), unit.fingerprints.len()).into_boxed_str()));
    }
    Ok(unit)
}

pub(crate) fn build_directives(text: &str) -> (Vec<String>, Vec<String>) {
    let (mut files, mut vars) = (Vec::new(), Vec::new());
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("cargo::").or_else(|| line.strip_prefix("cargo:")) else { continue };
        if let Some(path) = rest.strip_prefix("rerun-if-changed=") {
            files.push(path.to_string());
        } else if let Some(var) = rest.strip_prefix("rerun-if-env-changed=") {
            vars.push(var.to_string());
        }
    }
    (files, vars)
}

/// Gathers every stamp cargo's freshness check depends on, refusing what it cannot vouch for.
struct Collector<'a> {
    gate: &'a Gate,
    launched_ns: i128,
    inputs: Vec<Entry>,
    targets: Vec<Entry>,
    seen: HashSet<PathBuf>,
    trusted: HashMap<u64, bool>,
    env: Vec<(String, Option<String>)>,
    profiles: BTreeSet<PathBuf>,
    packages: HashSet<PathBuf>,
}

impl<'a> Collector<'a> {
    fn new(gate: &'a Gate, launched_ns: i128) -> Collector<'a> {
        Collector {
            gate,
            launched_ns,
            inputs: Vec::new(),
            targets: Vec::new(),
            seen: HashSet::new(),
            trusted: HashMap::new(),
            env: Vec::new(),
            profiles: BTreeSet::new(),
            packages: HashSet::new(),
        }
    }

    fn collect(&mut self, artifacts: &[serde_json::Value], scripts: &[serde_json::Value]) -> Result<(), &'static str> {
        let root = self.gate.root.clone();
        let mut manifests: HashMap<String, PathBuf> = HashMap::new();
        let mut units = Vec::new();
        for msg in artifacts {
            let id = msg.get("package_id").and_then(|p| p.as_str()).ok_or("unit without package")?.to_string();
            let manifest = PathBuf::from(msg.get("manifest_path").and_then(|p| p.as_str()).ok_or("unit without manifest")?);
            let mut outputs: Vec<PathBuf> = msg.get("filenames").and_then(|f| f.as_array()).into_iter().flatten()
                .filter_map(|f| f.as_str().map(PathBuf::from)).collect();
            if let Some(exe) = msg.get("executable").and_then(|e| e.as_str()) {
                outputs.push(PathBuf::from(exe));
            }
            outputs.sort();
            outputs.dedup();
            let unit = locate_unit(msg, &outputs)?;
            self.profiles.insert(unit.profile.clone());
            manifests.insert(id.clone(), manifest.clone());
            units.push((msg, id, manifest, outputs, unit));
        }
        self.input(&root.join("Cargo.toml"), false)?;
        self.input(&root.join("Cargo.lock"), false)?;
        for config in self.gate.configs.clone() {
            self.input(&config, true)?;
        }
        for dir in self.gate.cwd.clone().ancestors() {
            for name in ["rust-toolchain.toml", "rust-toolchain"] {
                self.input(&dir.join(name), true)?;
            }
        }
        // Rebuilt units go first: their sources are the likeliest to refuse the record, and a later check meets them first.
        units.sort_by_key(|(msg, ..)| msg.get("fresh").and_then(|f| f.as_bool()) == Some(true));
        for (msg, id, manifest, outputs, unit) in &units {
            // Every cargo run re-copies a build script's uplifted executable, which only cargo itself ever runs; its hashed original is stamped instead.
            let script = msg.pointer("/target/kind/0").and_then(|k| k.as_str()) == Some("custom-build");
            let uplifted_script = format!("build-script-build{}", env::consts::EXE_SUFFIX);
            for out in outputs.iter().chain(&unit.extra_outputs) {
                if script && !unit.extra_outputs.is_empty() && out.file_name().is_some_and(|n| *n == *uplifted_script) {
                    continue;
                }
                self.target(out)?;
            }
            for dir in &unit.fingerprints {
                self.fingerprint_dir(dir)?;
            }
            if id.starts_with("path+") {
                self.path_unit(msg, manifest, unit)?;
            }
        }
        for msg in scripts {
            self.script(msg, &manifests)?;
        }
        Ok(())
    }

    fn path_unit(&mut self, msg: &serde_json::Value, manifest: &Path, unit: &Unit) -> Result<(), &'static str> {
        let src = PathBuf::from(msg.pointer("/target/src_path").and_then(|s| s.as_str()).ok_or("unit without source")?);
        let src_id = sys::stat(&src).map(|m| (m.dev(), m.ino())).map_err(|_| "input missing")?;
        let mut sourced = false;
        let mut read_any = false;
        for depinfo_path in &unit.depinfos {
            let Ok(text) = fs::read_to_string(depinfo_path) else { continue };
            read_any = true;
            self.target(depinfo_path)?;
            let info = depinfo::parse(&text);
            for file in &info.files {
                let path = Path::new(file);
                let path = if path.is_absolute() { path.to_path_buf() } else { self.gate.root.join(path) };
                self.file(&path, false)?;
                sourced |= sys::stat(&path).is_ok_and(|m| (m.dev(), m.ino()) == src_id);
            }
            for (name, value) in &info.env {
                self.env_dep(name, value)?;
            }
        }
        if !read_any || !sourced {
            return Err("dep-info does not match the unit");
        }
        let package_dir = manifest.parent().ok_or("manifest without directory")?.to_path_buf();
        self.package(&package_dir, manifest)
    }

    /// Manifests, the directories cargo scans for targets, and the directories that hold workspace members.
    fn package(&mut self, dir: &Path, manifest: &Path) -> Result<(), &'static str> {
        if !self.packages.insert(dir.to_path_buf()) {
            return Ok(());
        }
        self.input(manifest, false)?;
        self.input(dir, false)?;
        self.input(&dir.join("src"), true)?;
        for sub in ["src/bin", "examples", "tests", "benches"] {
            let path = dir.join(sub);
            self.input(&path, true)?;
            if let Ok(entries) = fs::read_dir(&path) {
                for entry in entries.flatten() {
                    if entry.file_type().is_ok_and(|t| t.is_dir()) {
                        self.input(&entry.path(), false)?;
                    }
                }
            }
        }
        let root = self.gate.root.to_string_lossy().into_owned();
        if within(&dir.to_string_lossy(), &root, cfg!(windows)) {
            for ancestor in dir.ancestors().skip(1) {
                if !within(&ancestor.to_string_lossy(), &root, cfg!(windows)) {
                    break;
                }
                self.input(ancestor, false)?;
            }
        }
        Ok(())
    }

    fn script(&mut self, msg: &serde_json::Value, manifests: &HashMap<String, PathBuf>) -> Result<(), &'static str> {
        let id = msg.get("package_id").and_then(|p| p.as_str()).ok_or("script without package")?;
        let out_dir = PathBuf::from(msg.get("out_dir").and_then(|o| o.as_str()).ok_or("script without out_dir")?);
        let run_dir = out_dir.parent().ok_or("script without run directory")?;
        let name = run_dir.file_name().and_then(OsStr::to_str).ok_or("script without run directory")?;
        let hash = name.rsplit_once('-').map(|(_, h)| h).filter(|h| is_hash(h)).ok_or("unrecognised build script")?;
        let profile = run_dir.parent().and_then(Path::parent).ok_or("unrecognised build script")?.to_path_buf();
        let fingerprint = profile.join(".fingerprint").join(format!("{}-{hash}", package_name(id)));
        self.profiles.insert(profile);
        self.fingerprint_dir(&fingerprint)?;
        let output = run_dir.join("output");
        self.target(&output)?;
        let root_output = run_dir.join("root-output");
        if root_output.exists() {
            self.target(&root_output)?;
        }
        let text = fs::read_to_string(&output).map_err(|_| "unreadable build script output")?;
        let (files, vars) = build_directives(&text);
        let package_dir = manifests.get(id).and_then(|m| m.parent()).ok_or("build script without package")?.to_path_buf();
        for file in &files {
            let path = package_dir.join(file);
            let meta = sys::stat(&path).map_err(|_| "rerun-if-changed path missing")?;
            if meta.is_dir() {
                self.tree(&path, false)?;
            } else {
                self.file(&path, false)?;
            }
        }
        for var in &vars {
            if per_run(var) {
                return Err("per-run variable observed");
            }
            let value = match effective(&self.gate.env_vars, var) {
                Some(v) => Some(v.into_string().map_err(|_| "non-UTF-8 variable")?),
                None => None,
            };
            self.push_env(var, value);
        }
        // Without rerun-if-changed cargo reruns the script when any file of a path package changes.
        if files.is_empty() && id.starts_with("path+") {
            self.tree(&package_dir, true)?;
        }
        Ok(())
    }

    fn fingerprint_dir(&mut self, dir: &Path) -> Result<(), &'static str> {
        self.target(dir)?;
        let entries = fs::read_dir(dir).map_err(|_| "unreadable fingerprint")?;
        let mut files: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        files.sort();
        for file in files {
            self.target(&file)?;
        }
        Ok(())
    }

    /// Every entry below `top`; a symlinked directory is followed (each directory once) and stamped through the link, so a retarget or an edit behind it is seen.
    fn tree(&mut self, top: &Path, package: bool) -> Result<(), &'static str> {
        let mut stack = vec![(top.to_path_buf(), 0usize)];
        let mut walked = HashSet::new();
        let mut count = 0usize;
        while let Some((dir, depth)) = stack.pop() {
            self.file(&dir, false)?;
            let meta = sys::stat(&dir).map_err(|_| "unreadable directory")?;
            if !walked.insert((meta.dev(), meta.ino())) {
                continue;
            }
            let mut entries: Vec<fs::DirEntry> = fs::read_dir(&dir).map_err(|_| "unreadable directory")?.flatten().collect();
            entries.sort_by_key(|e| e.file_name());
            for entry in entries {
                count += 1;
                if count > hashing::DIR_MAX_FILES {
                    return Err("tree too large");
                }
                let path = entry.path();
                let kind = entry.file_type().map_err(|_| "unreadable directory")?;
                let linked_dir = kind.is_symlink() && path.is_dir();
                if !kind.is_dir() && !linked_dir {
                    self.file(&path, false)?;
                    continue;
                }
                let name = entry.file_name();
                let real = if linked_dir { fs::canonicalize(&path).map_err(|_| "unreadable directory")? } else { path.clone() };
                let skip = name == ".git"
                    || [&path, &real].iter().any(|p| self.under_target(p))
                    || (package && depth == 0 && name == "target")
                    || (package && path.join("Cargo.toml").exists());
                if skip && linked_dir {
                    self.file(&path, false)?;
                } else if !skip {
                    stack.push((path, depth + 1));
                }
            }
        }
        Ok(())
    }

    fn env_dep(&mut self, name: &str, value: &Option<String>) -> Result<(), &'static str> {
        if per_run(name) {
            return Err("per-run variable observed");
        }
        // Cargo vouched for what matches; a value that differs is one cargo sets itself and derives from recorded inputs.
        let current = effective(&self.gate.env_vars, name).map(OsString::into_string);
        match (current, value) {
            (None, None) => self.push_env(name, None),
            (Some(Ok(current)), Some(value)) if current == *value => self.push_env(name, Some(current)),
            _ => {}
        }
        Ok(())
    }

    fn push_env(&mut self, name: &str, value: Option<String>) {
        if !self.env.iter().any(|(n, v)| n == name && *v == value) {
            self.env.push((name.to_string(), value));
        }
    }

    fn under_target(&self, path: &Path) -> bool {
        let windows = cfg!(windows);
        let path = path.to_string_lossy();
        within(&path, &self.gate.target.to_string_lossy(), windows) || self.profiles.iter().any(|p| within(&path, &p.to_string_lossy(), windows))
    }

    fn file(&mut self, path: &Path, absent_ok: bool) -> Result<(), &'static str> {
        if self.under_target(path) { self.target(path) } else { self.input(path, absent_ok) }
    }

    /// A source must exist (unless allowed absent) and must not have changed since shortly before cargo started.
    fn input(&mut self, path: &Path, absent_ok: bool) -> Result<(), &'static str> {
        if !self.seen.insert(path.to_path_buf()) {
            return Ok(());
        }
        match sys::stat(path) {
            Ok(meta) => {
                let stamp = Stamp::of(&meta);
                self.vouch(&stamp, path)?;
                if stamp.too_new_for(self.launched_ns) {
                    return Err("input changed during the build");
                }
                self.inputs.push(Entry { path: path.to_str().ok_or("non-UTF-8 path")?.to_string(), stamp: Some(stamp) });
            }
            Err(e) if absent_ok && e.kind() == io::ErrorKind::NotFound => {
                self.inputs.push(Entry { path: path.to_str().ok_or("non-UTF-8 path")?.to_string(), stamp: None });
            }
            Err(_) => return Err("input missing"),
        }
        Ok(())
    }

    fn target(&mut self, path: &Path) -> Result<(), &'static str> {
        if !self.seen.insert(path.to_path_buf()) {
            return Ok(());
        }
        let meta = sys::stat(path).map_err(|_| "output missing")?;
        let stamp = Stamp::of(&meta);
        self.vouch(&stamp, path)?;
        self.targets.push(Entry { path: path.to_str().ok_or("non-UTF-8 path")?.to_string(), stamp: Some(stamp) });
        Ok(())
    }

    /// The stamp-trust rules of the compile cache: fine timestamps, a change time, an allow-listed volume that is not quarantined.
    fn vouch(&mut self, stamp: &Stamp, path: &Path) -> Result<(), &'static str> {
        if stamp.coarse() || stamp.ctime_ns <= 0 || i64::try_from(stamp.mtime_ns).is_err() || i64::try_from(stamp.ctime_ns).is_err() {
            return Err("unreliable timestamps");
        }
        let dirs = &self.gate.dirs;
        let trusted = *self.trusted.entry(stamp.dev).or_insert_with(|| hashing::fs_trusted(path) && !cache::quarantined(dirs, stamp.dev));
        if trusted { Ok(()) } else { Err("untrusted volume") }
    }
}

/// Bounds the record directory by age and count; once a day it also drops records of vanished workspaces and targets.
fn prune(dir: &Path, keep: Option<&Path>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    let now = SystemTime::now();
    let age = |modified: SystemTime| now.duration_since(modified).unwrap_or_default();
    let mut records = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else { continue };
        if name.contains(fsx::TEMP_MARKER) {
            if age(modified) > TEMP_GRACE {
                let _ = fs::remove_file(&path);
            }
        } else if name.ends_with(RECORD_SUFFIX) && Some(path.as_path()) != keep {
            if age(modified) > MAX_AGE {
                let _ = fs::remove_file(&path);
            } else {
                records.push((modified, path));
            }
        }
    }
    if records.len() >= MAX_RECORDS {
        records.sort();
        let excess = records.len() + 1 - MAX_RECORDS;
        for (_, path) in records.drain(..excess) {
            let _ = fs::remove_file(path);
        }
    }
    let marker = dir.join(".collected");
    let due = fs::metadata(&marker).and_then(|m| m.modified()).map_or(true, |m| age(m) > COLLECT_EVERY);
    if due {
        let _ = fs::write(&marker, b"");
        for (_, path) in records {
            if vanished(&path) {
                let _ = fs::remove_file(path);
            }
        }
    }
}

fn vanished(path: &Path) -> bool {
    let mut head = Vec::new();
    let read = File::open(path).and_then(|f| f.take(PEEK_BYTES).read_to_end(&mut head));
    match read.ok().and(peek_paths(&head)) {
        Some((root, target)) => !Path::new(&root).join("Cargo.toml").is_file() || !Path::new(&target).is_dir(),
        None => true,
    }
}

/// Removes stale, excess and orphaned records; returns how many records remain.
pub fn collect(dirs: &Dirs) -> usize {
    let dir = records_dir(dirs);
    let _ = fs::remove_file(dir.join(".collected"));
    prune(&dir, None);
    fs::read_dir(&dir).map(|e| e.flatten().filter(|e| e.file_name().to_string_lossy().ends_with(RECORD_SUFFIX)).count()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Record {
        Record {
            key: "k".repeat(32),
            display: "v1|style=none".into(),
            cwd: "/w".into(),
            target: "/t".into(),
            root: "/w".into(),
            created_unix: 7,
            origin: Origin::Build,
            units: 3,
            profiles: vec!["/t/debug".into()],
            env: vec![("A".into(), Some("1".into())), ("B".into(), None)],
            entries: vec![
                Entry { path: "/w/src/lib.rs".into(), stamp: Some(Stamp { dev: 1, ino: 2, size: 3, mtime_ns: 4_000_000_001, ctime_ns: -5 }) },
                Entry { path: "/w/.cargo/config.toml".into(), stamp: None },
            ],
            replay: vec![(Stream::Out, "{\"reason\":\"build-finished\"}".into()), (Stream::Err, "    Finished `dev` profile in 0.10s".into())],
        }
    }

    #[test]
    fn records_round_trip_and_reject_torn_damaged_or_foreign_files() {
        let record = sample();
        let bytes = encode(&record);
        assert_eq!(decode(&bytes), Some(record.clone()));
        assert_eq!(peek_paths(&bytes), Some(("/w".to_string(), "/t".to_string())));
        for cut in [0, 7, HEADER, bytes.len() / 2, bytes.len() - 1] {
            assert_eq!(decode(&bytes[..cut]), None, "torn at {cut}");
        }
        let mut flipped = bytes.clone();
        flipped[HEADER + 3] ^= 1;
        assert_eq!(decode(&flipped), None, "a damaged payload fails its checksum");
        let mut newer = bytes.clone();
        newer[8..12].copy_from_slice(&(FORMAT + 1).to_le_bytes());
        assert_eq!(decode(&newer), None, "another format is ignored");
        assert_eq!(peek_paths(&newer), None);
        let mut longer = bytes;
        longer.push(0);
        assert_eq!(decode(&longer), None, "trailing bytes are a torn write");
    }

    #[test]
    fn a_record_with_many_inputs_is_stored_packed_and_a_format_1_record_is_ignored() {
        let mut record = sample();
        record.entries = (0..2000)
            .map(|n| Entry { path: format!("/usr/local/cargo/registry/src/index.crates.io-1949cf8c6b5b557f/memchr-2.7.4/src/f{n}.rs"), stamp: None })
            .collect();
        let raw: usize = record.entries.iter().map(|e| e.path.len() + 5).sum();
        let bytes = encode(&record);
        assert!(bytes.len() * 4 < raw, "{} stored for {raw} raw bytes", bytes.len());
        assert_eq!(decode(&bytes), Some(record.clone()));
        assert_eq!(peek_paths(&bytes[..HEADER + 200]), Some(("/w".to_string(), "/t".to_string())), "the paths are read from the head alone");
        let mut older = bytes;
        older[8..12].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(decode(&older), None);
        assert_eq!(peek_paths(&older), None, "an older record is discarded by the next prune");
    }

    #[test]
    fn a_written_record_is_read_back_and_pruned_when_its_workspace_vanishes() {
        let root = fsx::temp_sibling(&env::temp_dir().join("frust-noop-codec"));
        let dirs = Dirs::at(&root);
        let dir = records_dir(&dirs);
        let workspace = root.join("ws");
        fs::create_dir_all(workspace.join("target")).unwrap();
        fs::write(workspace.join("Cargo.toml"), b"[workspace]\n").unwrap();
        let mut record = sample();
        record.root = workspace.to_str().unwrap().into();
        record.target = workspace.join("target").to_str().unwrap().into();
        let live = dir.join(format!("a{RECORD_SUFFIX}"));
        let gone = dir.join(format!("b{RECORD_SUFFIX}"));
        fsx::write_atomic(&live, &encode(&record)).unwrap();
        record.root = root.join("missing").to_str().unwrap().into();
        fsx::write_atomic(&gone, &encode(&record)).unwrap();
        assert_eq!(decode(&read_capped(&live).unwrap()).map(|r| r.units), Some(3));
        assert_eq!(collect(&dirs), 1);
        assert!(live.exists() && !gone.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn restamping_covers_only_listed_identical_rewrites_of_an_otherwise_valid_record() {
        let root = fsx::temp_sibling(&env::temp_dir().join("frust-noop-restamp"));
        let dirs = Dirs::at(&root);
        let target = root.join("target");
        fs::create_dir_all(&target).unwrap();
        let (out, other) = (target.join("out.rlib"), target.join("other.rlib"));
        fs::write(&out, b"same bytes").unwrap();
        fs::write(&other, b"other").unwrap();
        let mut record = sample();
        record.target = target.to_str().unwrap().into();
        record.entries = [&out, &other].iter().map(|p| Entry { path: p.to_str().unwrap().into(), stamp: Some(Stamp::read(p).unwrap()) }).collect();
        let path = records_dir(&dirs).join(format!("r{RECORD_SUFFIX}"));
        fsx::write_atomic(&path, &encode(&record)).unwrap();
        let before = Stamp::read(&out).unwrap();
        let replacement = target.join("tmp");
        fs::write(&replacement, b"same bytes").unwrap();
        sys::rename(&replacement, &out).unwrap();
        let after = Stamp::read(&out).unwrap();
        let read = || decode(&read_capped(&path).unwrap()).unwrap();

        let stale = Rewrite { path: out.clone(), before: Stamp { ino: before.ino + 1, ..before }, after };
        assert_eq!(restamp(&dirs, &[stale], true), 0, "a rewrite of a different file than the record saw is not trusted");
        assert_eq!(restamp(&dirs, &[Rewrite { path: other.clone(), before, after }], true), 0, "an unlisted change is never covered");
        fs::write(&other, b"changed").unwrap();
        assert_eq!(restamp(&dirs, &[Rewrite { path: out.clone(), before, after }], true), 0, "a record that already mismatched stays as it is");
        assert_eq!(read(), record);
        fs::write(&other, b"other").unwrap();
        record.entries[1].stamp = Some(Stamp::read(&other).unwrap());
        fsx::write_atomic(&path, &encode(&record)).unwrap();
        assert_eq!(restamp(&dirs, &[Rewrite { path: out.clone(), before, after }], true), 1);
        assert_eq!(read().entries[0].stamp, Some(after));
        assert_eq!(read().entries[1], record.entries[1]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn finished_lines_are_retimed_in_cargo_format() {
        let line = "    Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.74s";
        assert_eq!(retime(line, Duration::from_millis(3)), "    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.00s");
        assert_eq!(retime(line, Duration::from_millis(1_234)), "    Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.23s");
        assert_eq!(retime(line, Duration::from_secs(65)), "    Finished `dev` profile [unoptimized + debuginfo] target(s) in 1m 05s");
        let coloured = "\x1b[1m\x1b[92m    Finished\x1b[0m `release` profile [optimized] target(s) in 2m 03s";
        assert_eq!(retime(coloured, Duration::from_millis(20)), "\x1b[1m\x1b[92m    Finished\x1b[0m `release` profile [optimized] target(s) in 0.02s");
        assert_eq!(retime("warning: unused in 1.00s", Duration::ZERO), "warning: unused in 1.00s");
        assert_eq!(normalised(line), normalised(&retime(line, Duration::from_secs(9))));
    }

    #[test]
    fn only_known_build_and_check_flags_are_eligible() {
        let ok = |args: &[&str]| eligible_args(&args.iter().map(OsString::from).collect::<Vec<_>>());
        assert!(ok(&["build"]));
        assert!(ok(&["+stable", "--config", "profile.dev.package.\"*\".debug=0", "build", "--release", "-p", "app", "-Ffeat", "--features=a,b", "-j4"]));
        assert!(ok(&["-C", "sub", "check", "--all-targets", "--target", "x86_64-unknown-linux-gnu", "--locked"]));
        for bad in [
            &["build", "-v"][..], &["build", "--message-format=json"], &["build", "--timings"], &["-Zunstable-options", "build"],
            &["run"], &["build", "--", "x"], &["build", "--bin", "-v"], &["build", "-p"], &["test"], &["build", "extra"],
            &["build", "--future-incompat-report"], &["--version"],
        ] {
            assert!(!ok(bad), "{bad:?}");
        }
    }

    #[test]
    fn unit_names_and_build_directives_parse() {
        assert_eq!(hashed_name("libapplib-a75b6ef8d2ecb9d4.rmeta"), Some(("libapplib", "a75b6ef8d2ecb9d4")));
        assert_eq!(hashed_name("appbin-18600ec08753411b"), Some(("appbin", "18600ec08753411b")));
        assert_eq!(hashed_name("appbin"), None);
        assert_eq!(hashed_name("app-0123"), None);
        assert_eq!(package_name("path+file:///w/p1#app-pkg@0.1.0"), "app-pkg");
        assert_eq!(package_name("path+file:///w/app#0.1.0"), "app");
        let msg = serde_json::json!({"target": {"kind": ["custom-build"], "name": "build-script-build"}, "profile": {"test": false}});
        assert_eq!(fingerprint_file(&msg).as_deref(), Some("build-script-build-script-build"));
        let test = serde_json::json!({"target": {"kind": ["bin"], "name": "app"}, "profile": {"test": true}});
        assert_eq!(fingerprint_file(&test).as_deref(), Some("test-bin-app"));
        let (files, vars) = build_directives("cargo:rerun-if-changed=build.rs\ncargo::rerun-if-env-changed=X\ncargo:warning=hi\ncargo::rerun-if-changed=data\n");
        assert_eq!(files, vec!["build.rs", "data"]);
        assert_eq!(vars, vec!["X"]);
    }

    #[test]
    fn paths_compare_lexically_in_windows_and_unix_forms() {
        let target = r"C:\Users\runneradmin\AppData\Local\frust\targets\ws-1";
        for path in [
            r"C:\Users\runneradmin\AppData\Local\frust\targets\ws-1\debug\build\app-0123456789abcdef\out/gen.rs",
            r"c:\users\RUNNERADMIN\appdata\local\frust\targets\ws-1\debug",
            r"\\?\C:\Users\runneradmin\AppData\Local\frust\targets\ws-1\debug\deps\app.d",
            "C:/Users/runneradmin/AppData/Local/frust/targets/ws-1/./debug/../debug/deps",
            r"C:\Users\runneradmin\AppData\Local\frust\targets\ws-1",
        ] {
            assert!(within(path, target, true), "{path}");
        }
        for path in [
            r"C:\Users\runneradmin\AppData\Local\frust\targets\ws-10\debug",
            r"D:\Users\runneradmin\AppData\Local\frust\targets\ws-1\debug",
            r"C:\Users\runneradmin\AppData\Local\frust\targets\ws-1\..\ws-2",
            r"src\main.rs",
        ] {
            assert!(!within(path, target, true), "{path}");
        }
        assert!(within(r"\\?\UNC\server\share\t\debug", r"\\server\share\t", true));
        assert!(within(r"C:\..\t\debug", r"C:\t", true), "a drive root has no parent");
        assert!(within("/w/t/debug/./deps", "/w/t", false));
        assert!(!within("/w/T/debug", "/w/t", false), "unix paths keep their case");
        assert!(!within("/w/t/../u", "/w/t", false));
        assert!(!within(r"\w\t\debug", "/w/t", false), "a backslash is a file name character on unix");
        assert!(!within("/w/t", "", false));
    }

    #[test]
    fn recorder_refuses_what_it_cannot_replay() {
        let mut ok = Recorder::default();
        ok.take(&[b"{\"reason\":\"build-finished\"}".to_vec()], &[b"    Finished x in 0.01s".to_vec()]);
        assert!(ok.refused.is_none() && ok.lines.len() == 2);
        for (out, err) in [(b"hello".to_vec(), b"".to_vec()), (b"{}".to_vec(), b"progress\r".to_vec()), (b"{}".to_vec(), vec![0xff])] {
            let mut r = Recorder::default();
            r.take(&[out], &[err]);
            assert!(r.refused.is_some() && r.lines.is_empty());
        }
    }

    #[test]
    fn audit_detects_rebuilt_units_and_different_output() {
        let predicted = Record { replay: vec![(Stream::Err, "    Finished `dev` profile in 0.01s".into())], ..sample() };
        let mut fresh = Recorder::default();
        fresh.take(&[br#"{"reason":"compiler-artifact","fresh":true,"target":{"name":"a"}}"#.to_vec()], &[b"    Finished `dev` profile in 0.31s".to_vec()]);
        assert_eq!(audit(&predicted, 0, Some(&fresh), &[]), None);
        assert!(audit(&predicted, 101, Some(&fresh), &[]).is_some());
        let mut rebuilt = Recorder::default();
        rebuilt.take(&[br#"{"reason":"compiler-artifact","fresh":false,"target":{"name":"a"}}"#.to_vec()], &[b"    Finished `dev` profile in 0.31s".to_vec()]);
        assert!(audit(&predicted, 0, Some(&rebuilt), &[]).unwrap().contains("rebuilt a"));
        let mut noisy = Recorder::default();
        noisy.take(&[], &[b"warning: x".to_vec(), b"    Finished `dev` profile in 0.31s".to_vec()]);
        assert!(audit(&predicted, 0, Some(&noisy), &[]).unwrap().contains("differs"));
    }

    #[test]
    fn an_uplifted_binary_is_traced_to_the_original_its_fingerprint_names() {
        let root = fsx::temp_sibling(&env::temp_dir().join("frust-noop-uplift"));
        let profile = root.join("debug");
        let (named, decoy) = ("0123456789abcdef", "fedcba9876543210");
        fs::create_dir_all(profile.join(".fingerprint").join(format!("my-tool-{named}"))).unwrap();
        fs::write(profile.join(".fingerprint").join(format!("my-tool-{named}")).join("bin-my-tool"), b"").unwrap();
        fs::create_dir_all(profile.join("deps")).unwrap();
        let original = profile.join("deps").join(format!("my_tool-{named}"));
        fs::write(&original, b"binary").unwrap();
        let uplifted = profile.join("my-tool");
        fs::hard_link(&original, &uplifted).unwrap();
        // A deduplicated alias without a fingerprint of this package must not make the original ambiguous.
        fs::hard_link(&original, profile.join("deps").join(format!("my_tool-{decoy}"))).unwrap();
        let msg = serde_json::json!({
            "package_id": "path+file:///w#my-tool@0.1.0",
            "target": { "name": "my-tool", "kind": ["bin"] },
            "profile": { "test": false },
            "filenames": [uplifted.to_str().unwrap()],
            "executable": uplifted.to_str().unwrap(),
        });
        let unit = locate_unit(&msg, std::slice::from_ref(&uplifted)).unwrap();
        assert_eq!(unit.fingerprints, vec![profile.join(".fingerprint").join(format!("my-tool-{named}"))]);
        assert_eq!(unit.extra_outputs, vec![original.clone()]);
        assert_eq!(unit.depinfos, vec![profile.join("deps").join(format!("my_tool-{named}.d"))]);
        fs::remove_dir_all(root).unwrap();
    }
}
