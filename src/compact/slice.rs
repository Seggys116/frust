use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use super::ProfileLock;
use crate::lock::{self, TargetMaintenanceGuard};
use crate::paths::Dirs;
use crate::sys;

/// Long enough for a Cargo process blocked on a profile lock to take it once released.
const HANDOFF: Duration = Duration::from_millis(15);
const BACKOFF_FIRST: Duration = Duration::from_millis(25);
const BACKOFF_MAX: Duration = Duration::from_millis(1600);
const BACKOFF_TOTAL: Duration = Duration::from_secs(30);
const POLL_EVERY: Duration = Duration::from_millis(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Yield {
    Kept,
    /// The locks were released and taken again over an unchanged target; cached directory state must be dropped.
    Resumed(Duration),
    Abort,
}

/// Why a sliced pass stopped early; the marker stays incomplete so the next due pass continues.
pub const ABORT_BUILD: &str = "a build started";
pub const ABORT_CHANGED: &str = "the target changed between slices";
pub const ABORT_BUSY: &str = "profiles stayed busy";

/// Holds Cargo's profile locks and frust's target gate for one bounded slice of work at a time.
pub(super) struct Slices<'a> {
    dirs: &'a Dirs,
    target: &'a Path,
    profiles: Vec<PathBuf>,
    locks: Vec<ProfileLock>,
    gate: Option<TargetMaintenanceGuard>,
    gated: bool,
    length: Option<Duration>,
    began: Instant,
    activity: Option<SystemTime>,
    polled: Instant,
    pub slices: u32,
    pub longest: Duration,
    pub aborted: Option<&'static str>,
}

/// Modification times of the directories a compile, uplift or new incremental session writes into; stats only, so cheap under the locks.
fn stamp(target: &Path, profiles: &[PathBuf]) -> Option<Vec<(i64, i64)>> {
    let mut times = Vec::new();
    let mut add = |dir: &Path, optional: bool| match sys::stat(dir) {
        Ok(meta) => {
            times.push(meta.mtime());
            Some(())
        }
        Err(error) if optional && error.kind() == std::io::ErrorKind::NotFound => {
            times.push((0, 0));
            Some(())
        }
        Err(_) => None,
    };
    add(target, false)?;
    for profile in profiles {
        add(profile, false)?;
        for sub in ["deps", ".fingerprint", "incremental", "build", "examples"] {
            add(&profile.join(sub), true)?;
        }
        let mut crates: Vec<PathBuf> = std::fs::read_dir(profile.join("incremental")).map(|rd| rd.flatten().map(|e| e.path()).collect()).unwrap_or_default();
        crates.sort();
        for krate in crates {
            add(&krate, true)?;
        }
    }
    Some(times)
}

/// None when frust has no lock history for the target: plain Cargo locks are then the only exclusion.
fn try_gate(dirs: &Dirs, target: &Path) -> Result<Option<TargetMaintenanceGuard>, ()> {
    match lock::target_activity(dirs, target) {
        Ok(None) => Ok(None),
        Ok(Some(_)) => match lock::try_target_maintenance(dirs, target) {
            Ok(Some(guard)) => Ok(Some(guard)),
            _ => Err(()),
        },
        Err(_) => Err(()),
    }
}

impl<'a> Slices<'a> {
    /// Takes the gate and every free profile; returns the busy profiles, or an abort reason when a build holds the target.
    pub fn begin(dirs: &'a Dirs, target: &'a Path, candidates: Vec<PathBuf>, length: Option<Duration>) -> (Slices<'a>, Vec<PathBuf>) {
        let mut slices = Slices {
            dirs,
            target,
            profiles: Vec::new(),
            locks: Vec::new(),
            gate: None,
            gated: false,
            length: length.filter(|l| !l.is_zero()),
            began: Instant::now(),
            activity: None,
            polled: Instant::now(),
            slices: 1,
            longest: Duration::ZERO,
            aborted: None,
        };
        if slices.length.is_some() {
            if lock::build_pending(dirs, target) {
                slices.aborted = Some(ABORT_BUILD);
                return (slices, Vec::new());
            }
            match try_gate(dirs, target) {
                Ok(gate) => {
                    slices.gated = gate.is_some();
                    slices.gate = gate;
                }
                Err(()) => {
                    slices.aborted = Some(ABORT_BUILD);
                    return (slices, Vec::new());
                }
            }
            slices.activity = slices.gate.as_ref().and_then(|g| g.activity().ok());
        }
        let mut busy = Vec::new();
        for profile in candidates {
            match ProfileLock::try_acquire(&profile) {
                Some(lock) => {
                    slices.locks.push(lock);
                    slices.profiles.push(profile);
                }
                None => busy.push(profile),
            }
        }
        (slices, busy)
    }

    pub fn profiles(&self) -> &[PathBuf] {
        &self.profiles
    }

    /// When the current slice should end; None for an unsliced pass.
    pub fn deadline(&self) -> Option<Instant> {
        self.length.map(|l| self.began + l)
    }

    /// Between two files or steps: yields once the slice is used up.
    pub fn checkpoint(&mut self) -> Yield {
        if self.aborted.is_some() {
            return Yield::Abort;
        }
        let Some(length) = self.length else { return Yield::Kept };
        // A frust build that queues mid-slice waits for one step, not for the rest of the slice.
        if self.polled.elapsed() >= POLL_EVERY {
            self.polled = Instant::now();
            if lock::build_waiting(self.dirs, self.target) {
                return self.abort(ABORT_BUILD);
            }
        }
        if self.began.elapsed() >= length { self.yield_now() } else { Yield::Kept }
    }

    /// Releases every lock, lets waiting builds in, and resumes only over the same target state.
    pub fn yield_now(&mut self) -> Yield {
        self.release_while(|| ()).1
    }

    /// Runs `work` with every lock released, then resumes as a yield does; the pause excludes the work's own time.
    pub fn release_while<R>(&mut self, work: impl FnOnce() -> R) -> (Option<R>, Yield) {
        if self.aborted.is_some() {
            return (None, Yield::Abort);
        }
        if self.length.is_none() {
            return (Some(work()), Yield::Kept);
        }
        let signature = stamp(self.target, &self.profiles);
        self.finish();
        let paused = Instant::now();
        let done = work();
        let worked = paused.elapsed();
        std::thread::sleep(HANDOFF.saturating_sub(worked));
        (Some(done), self.resume(paused, worked, signature))
    }

    fn resume(&mut self, paused: Instant, worked: Duration, signature: Option<Vec<(i64, i64)>>) -> Yield {
        let mut backoff = BACKOFF_FIRST;
        loop {
            if lock::build_pending(self.dirs, self.target) {
                return self.abort(ABORT_BUILD);
            }
            if self.relock() {
                break;
            }
            if paused.elapsed() >= BACKOFF_TOTAL {
                return self.abort(ABORT_BUSY);
            }
            std::thread::sleep(backoff);
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
        let held = Instant::now();
        let resumed = (held - paused).saturating_sub(worked);
        self.began = held;
        // A build that queued while the locks were being taken would otherwise wait out this whole slice.
        let activity = self.gate.as_ref().and_then(|g| g.activity().ok());
        if activity != self.activity || lock::build_waiting(self.dirs, self.target) {
            return self.abort(ABORT_BUILD);
        }
        if signature.is_none() || stamp(self.target, &self.profiles) != signature {
            return self.abort(ABORT_CHANGED);
        }
        self.slices += 1;
        self.polled = held;
        Yield::Resumed(resumed)
    }

    /// All or nothing, so a resumed slice holds exactly the locks the pass began with.
    fn relock(&mut self) -> bool {
        let gate = match try_gate(self.dirs, self.target) {
            Ok(gate) if gate.is_some() == self.gated => gate,
            _ => return false,
        };
        let mut locks = Vec::with_capacity(self.profiles.len());
        for profile in &self.profiles {
            match ProfileLock::try_acquire(profile) {
                Some(lock) => locks.push(lock),
                None => return false,
            }
        }
        self.gate = gate;
        self.locks = locks;
        true
    }

    fn abort(&mut self, why: &'static str) -> Yield {
        self.finish();
        self.aborted = Some(why);
        Yield::Abort
    }

    /// Ends the current slice and drops every lock.
    pub fn finish(&mut self) {
        if self.locks.is_empty() && self.gate.is_none() {
            return;
        }
        self.longest = self.longest.max(self.began.elapsed());
        self.locks.clear();
        self.gate = None;
    }
}
