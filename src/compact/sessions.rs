use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::sys;

#[derive(Debug, Default)]
pub(super) struct CleanupReport {
    pub(super) removed_dirs: usize,
    pub(super) removed_finalized_dirs: usize,
    pub(super) removed_locks: usize,
    pub(super) removed_empty_dirs: usize,
    pub(super) removed: Vec<(PathBuf, u64)>,
    pub(super) errors: Vec<(PathBuf, String)>,
    pub(super) skipped: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Stamp {
    dev: u64,
    ino: u64,
    len: u64,
    links: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
    directory: bool,
    allocated: u64,
}

impl Stamp {
    fn read(path: &Path) -> io::Result<Self> {
        Self::from_meta(&sys::lstat(path)?)
    }

    fn from_meta(meta: &sys::Meta) -> io::Result<Self> {
        if meta.file_type().is_symlink() || (!meta.is_file() && !meta.is_dir()) || meta.ino() == 0 {
            return Err(io::Error::from(io::ErrorKind::InvalidData));
        }
        Ok(Self {
            dev: meta.dev(),
            ino: meta.ino(),
            len: meta.len(),
            links: meta.nlink(),
            mtime: meta.mtime(),
            ctime: meta.ctime(),
            directory: meta.is_dir(),
            allocated: meta.allocated(),
        })
    }

    fn aged(&self, now: SystemTime, grace: Duration) -> bool {
        [self.mtime, self.ctime]
            .into_iter()
            .all(|time| Self::time_aged(time, now, grace))
    }

    fn time_aged((seconds, nanos): (i64, i64), now: SystemTime, grace: Duration) -> bool {
        let (Ok(seconds), Ok(nanos)) = (u64::try_from(seconds), u32::try_from(nanos)) else {
            return false;
        };
        if nanos >= 1_000_000_000 {
            return false;
        }
        UNIX_EPOCH
            .checked_add(Duration::new(seconds, nanos))
            .and_then(|time| now.duration_since(time).ok())
            .is_some_and(|age| age >= grace)
    }
}

fn component(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte.is_ascii_lowercase())
}

// Rust 1.89 and current rustc use fixed-width base-36 fields; timestamps stay opaque here.
fn lock_base(name: &str) -> Option<&str> {
    let base = name.strip_suffix(".lock")?;
    let mut parts = base.split('-');
    (parts.next()? == "s"
        && component(parts.next()?, 10)
        && component(parts.next()?, 7)
        && parts.next().is_none())
    .then_some(base)
}

fn working_name(name: &str, base: &str) -> bool {
    name.strip_prefix(base) == Some("-working")
}

fn entries(path: &Path) -> io::Result<Vec<PathBuf>> {
    let mut paths = fs::read_dir(path)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<io::Result<Vec<_>>>()?;
    paths.sort();
    Ok(paths)
}

fn siblings(path: &Path, base: &str) -> io::Result<Vec<PathBuf>> {
    let prefix = format!("{base}-");
    Ok(entries(path)?
        .into_iter()
        .filter(|entry| {
            entry
                .file_name()
                .is_some_and(|name| name.as_encoded_bytes().starts_with(prefix.as_bytes()))
        })
        .collect())
}

fn anchors_match(anchors: &[(PathBuf, Stamp)]) -> bool {
    anchors.iter().all(|(path, before)| {
        Stamp::read(path).is_ok_and(|after| {
            after.directory && after.dev == before.dev && after.ino == before.ino
        })
    })
}

fn snapshot(path: &Path, now: SystemTime, grace: Duration) -> io::Result<BTreeMap<PathBuf, Stamp>> {
    let result = tree_snapshot(path)?;
    if !result.values().all(|stamp| stamp.aged(now, grace)) {
        return Err(io::Error::from(io::ErrorKind::WouldBlock));
    }
    Ok(result)
}

fn tree_snapshot(path: &Path) -> io::Result<BTreeMap<PathBuf, Stamp>> {
    let root = Stamp::read(path)?;
    if !root.directory {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    let mut result = BTreeMap::from([(path.to_path_buf(), root.clone())]);
    let mut pending = vec![path.to_path_buf()];
    let mut directories = HashSet::from([(root.dev, root.ino)]);
    // The held session locks permit a complete iterative walk without permanent size exclusions.
    while let Some(directory) = pending.pop() {
        for child in entries(&directory)? {
            let stamp = Stamp::read(&child)?;
            if stamp.dev != root.dev {
                return Err(io::Error::from(io::ErrorKind::WouldBlock));
            }
            if stamp.directory {
                if !directories.insert((stamp.dev, stamp.ino)) {
                    return Err(io::Error::from(io::ErrorKind::InvalidData));
                }
                pending.push(child.clone());
            }
            result.insert(child, stamp);
        }
    }
    Ok(result)
}

// Hard-linked payloads count as freed only if this removal drops every remaining link.
fn freed(snapshot: &BTreeMap<PathBuf, Stamp>) -> u64 {
    let mut files: HashMap<(u64, u64), (u64, u64, u64)> = HashMap::new();
    let mut bytes: u64 = 0;
    for stamp in snapshot.values() {
        if stamp.directory {
            bytes = bytes.saturating_add(stamp.allocated);
        } else {
            let record =
                files
                    .entry((stamp.dev, stamp.ino))
                    .or_insert((stamp.links, stamp.allocated, 0));
            record.2 += 1;
        }
    }
    bytes.saturating_add(
        files
            .values()
            .filter(|(links, _, removed)| links == removed)
            .fold(0u64, |sum, (_, bytes, _)| sum.saturating_add(*bytes)),
    )
}

fn retained(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock
            | io::ErrorKind::InvalidData
            | io::ErrorKind::InvalidInput
            | io::ErrorKind::Unsupported
            | io::ErrorKind::NotFound
    )
}

fn record_error(report: &mut CleanupReport, path: &Path, error: io::Error) {
    report.skipped += 1;
    if !retained(&error) {
        report.errors.push((path.to_path_buf(), error.to_string()));
    }
}

struct LockCandidate {
    path: PathBuf,
    base: String,
    before: Stamp,
    siblings: Vec<PathBuf>,
    finalized: bool,
}

#[derive(Clone, Copy)]
struct CleanupPolicy {
    grace: Duration,
    finalized_ttl: Option<Duration>,
}

impl CleanupPolicy {
    fn snapshot(
        &self,
        path: &Path,
        finalized: bool,
        now: SystemTime,
    ) -> io::Result<BTreeMap<PathBuf, Stamp>> {
        if finalized {
            let tree = tree_snapshot(path)?;
            let ttl = self.finalized_ttl.ok_or(io::ErrorKind::WouldBlock)?;
            // Own hard-link removals refresh ctime without extending finalized reuse lifetime.
            if !tree
                .values()
                .all(|stamp| Stamp::time_aged(stamp.mtime, now, ttl))
            {
                return Err(io::Error::from(io::ErrorKind::WouldBlock));
            }
            Ok(tree)
        } else {
            snapshot(path, now, self.grace)
        }
    }
}

type SiblingMap = HashMap<String, Vec<PathBuf>>;

fn sibling_map(paths: &[PathBuf]) -> SiblingMap {
    let mut result: SiblingMap = HashMap::new();
    for path in paths {
        let Some(name) = path.file_name() else {
            continue;
        };
        let bytes = name.as_encoded_bytes();
        if bytes.len() <= 20 || bytes[20] != b'-' {
            continue;
        }
        let Ok(base) = std::str::from_utf8(&bytes[..20]) else {
            continue;
        };
        if lock_base(&format!("{base}.lock")).is_some() {
            result
                .entry(base.to_owned())
                .or_default()
                .push(path.clone());
        }
    }
    result
}

fn guarded_candidate(
    candidate: &LockCandidate,
    guard: &sys::SessionLocks,
    anchors: &[(PathBuf, Stamp)],
) -> bool {
    guard.matches_path(&candidate.path)
        && anchors_match(anchors)
        && guard
            .metadata(&candidate.path)
            .and_then(|meta| Stamp::from_meta(&meta))
            .is_ok_and(|stamp| stamp == candidate.before)
}

fn clean_selected(
    candidates: &[LockCandidate],
    anchors: &[(PathBuf, Stamp)],
    now: SystemTime,
    policy: CleanupPolicy,
    remove: &mut impl FnMut(&Path) -> io::Result<()>,
    report: &mut CleanupReport,
) {
    let paths: Vec<_> = candidates
        .iter()
        .map(|candidate| candidate.path.clone())
        .collect();
    let guard = match sys::try_session_locks(&paths) {
        Ok(guard) => guard,
        Err(_) if candidates.len() > 1 => {
            let middle = candidates.len() / 2;
            clean_selected(&candidates[..middle], anchors, now, policy, remove, report);
            clean_selected(&candidates[middle..], anchors, now, policy, remove, report);
            return;
        }
        Err(error) => {
            record_error(report, &candidates[0].path, error);
            return;
        }
    };
    let parent = candidates[0].path.parent().unwrap();
    let observed = match entries(parent) {
        Ok(paths) => sibling_map(&paths),
        Err(error) => {
            record_error(report, parent, error);
            return;
        }
    };
    let mut unlinked = Vec::new();
    for candidate in candidates {
        let result = (|| {
            let current = observed
                .get(&candidate.base)
                .map(Vec::as_slice)
                .unwrap_or_default();
            if current != candidate.siblings || !guarded_candidate(candidate, &guard, anchors) {
                return Err(io::Error::from(io::ErrorKind::WouldBlock));
            }
            if let Some(payload) = current.first() {
                let tree = policy.snapshot(payload, candidate.finalized, now)?;
                if policy.snapshot(payload, candidate.finalized, now)? != tree
                    || !guarded_candidate(candidate, &guard, anchors)
                {
                    return Err(io::Error::from(io::ErrorKind::WouldBlock));
                }
                remove(payload)?;
                if candidate.finalized {
                    report.removed_finalized_dirs += 1;
                } else {
                    report.removed_dirs += 1;
                }
                report.removed.push((payload.clone(), freed(&tree)));
            }
            Ok(())
        })();
        match result {
            Ok(()) => unlinked.push(candidate),
            Err(error) => record_error(report, &candidate.path, error),
        }
    }
    // One authoritative rescan covers the batch while every corresponding compiler lock remains held.
    let after = match entries(parent) {
        Ok(paths) => sibling_map(&paths),
        Err(error) => {
            record_error(report, parent, error);
            return;
        }
    };
    for candidate in unlinked {
        if after
            .get(&candidate.base)
            .is_some_and(|siblings| !siblings.is_empty())
            || !guarded_candidate(candidate, &guard, anchors)
        {
            record_error(
                report,
                &candidate.path,
                io::Error::from(io::ErrorKind::WouldBlock),
            );
            continue;
        }
        match fs::remove_file(&candidate.path) {
            Ok(()) => {
                report.removed_locks += 1;
                report
                    .removed
                    .push((candidate.path.clone(), candidate.before.allocated));
            }
            Err(error) => record_error(report, &candidate.path, error),
        }
    }
}

fn clean_candidates(
    paths: &[PathBuf],
    anchors: &[(PathBuf, Stamp)],
    now: SystemTime,
    policy: CleanupPolicy,
    remove: &mut impl FnMut(&Path) -> io::Result<()>,
    report: &mut CleanupReport,
) {
    let known = sibling_map(paths);
    let mut candidates = Vec::new();
    for path in paths {
        let Some(base) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(lock_base)
        else {
            continue;
        };
        let result = (|| {
            let before = Stamp::read(path)?;
            let siblings = known.get(base).cloned().unwrap_or_default();
            let finalized = siblings
                .first()
                .and_then(|path| path.file_name())
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    policy.finalized_ttl.is_some() && finalized_base(name) == Some(base)
                });
            if before.directory
                || before.links != 1
                || before.len != 0
                || (!finalized && !before.aged(now, policy.grace))
                || siblings.len() > 1
                || (!finalized
                    && siblings.first().is_some_and(|path| {
                        !path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| working_name(name, base))
                    }))
            {
                return Err(io::Error::from(io::ErrorKind::WouldBlock));
            }
            Ok(LockCandidate {
                path: path.clone(),
                base: base.to_owned(),
                before,
                siblings,
                finalized,
            })
        })();
        match result {
            Ok(candidate) => candidates.push(candidate),
            Err(error) => record_error(report, path, error),
        }
    }
    // Batches bound descriptor use; every batch drains in this pass, including around held or unreadable locks.
    for batch in candidates.chunks(64) {
        clean_selected(batch, anchors, now, policy, remove, report);
    }
}

/// The caller holds Cargo's profile locks; finalized sessions remain the caller's responsibility.
pub(super) fn cleanup(profile: &Path, grace: Duration) -> CleanupReport {
    cleanup_with(profile, grace, SystemTime::now(), &mut |path| {
        fs::remove_dir_all(path)
    })
}

fn cleanup_with(
    profile: &Path,
    grace: Duration,
    now: SystemTime,
    remove: &mut impl FnMut(&Path) -> io::Result<()>,
) -> CleanupReport {
    cleanup_policy_with(
        profile,
        CleanupPolicy {
            grace,
            finalized_ttl: None,
        },
        now,
        remove,
    )
}

pub(super) fn prune_expired(profile: &Path, ttl: Duration) -> CleanupReport {
    cleanup_policy_with(
        profile,
        CleanupPolicy {
            grace: Duration::from_secs(3600),
            finalized_ttl: Some(ttl),
        },
        SystemTime::now(),
        &mut |path| fs::remove_dir_all(path),
    )
}

struct ParentTimes(Vec<(PathBuf, fs::File, SystemTime)>);

impl ParentTimes {
    fn capture(&mut self, path: &Path) -> io::Result<()> {
        let file = sys::open_directory_for_times(path)?;
        let held = sys::fstat(&file)?;
        let named = Stamp::read(path)?;
        if !held.is_dir() || !named.directory || (held.dev(), held.ino()) != (named.dev, named.ino)
        {
            return Err(io::Error::from(io::ErrorKind::WouldBlock));
        }
        self.0.push((path.to_path_buf(), file, held.modified()?));
        Ok(())
    }

    fn restore(&mut self, report: &mut CleanupReport) {
        for (path, file, modified) in self.0.drain(..) {
            if let Err(error) = file.set_modified(modified) {
                record_error(report, &path, error);
            }
        }
    }
}

impl Drop for ParentTimes {
    fn drop(&mut self) {
        for (_, file, modified) in self.0.drain(..) {
            let _ = file.set_modified(modified);
        }
    }
}

fn cleanup_policy_with(
    profile: &Path,
    policy: CleanupPolicy,
    now: SystemTime,
    remove: &mut impl FnMut(&Path) -> io::Result<()>,
) -> CleanupReport {
    let mut report = CleanupReport::default();
    let grace = policy.grace;
    let incremental = profile.join("incremental");
    let mut anchors = Vec::new();
    for path in [profile, incremental.as_path()] {
        match Stamp::read(path) {
            Ok(stamp) if stamp.directory => anchors.push((path.to_path_buf(), stamp)),
            Ok(_) => return report,
            Err(error) => {
                record_error(&mut report, path, error);
                return report;
            }
        }
    }
    let mut root_times = ParentTimes(Vec::new());
    if policy.finalized_ttl.is_some() {
        for path in [profile, incremental.as_path()] {
            if let Err(error) = root_times.capture(path) {
                record_error(&mut report, path, error);
                return report;
            }
        }
    }
    let crates = match entries(&incremental) {
        Ok(entries) => entries,
        Err(error) => {
            record_error(&mut report, &incremental, error);
            return report;
        }
    };
    for directory in crates {
        let stamp = match Stamp::read(&directory) {
            Ok(stamp) if stamp.directory && stamp.dev == anchors[1].1.dev => stamp,
            _ => {
                report.skipped += 1;
                continue;
            }
        };
        let candidates = match entries(&directory) {
            Ok(entries) => entries,
            Err(error) => {
                record_error(&mut report, &directory, error);
                continue;
            }
        };
        let mut crate_times = ParentTimes(Vec::new());
        if policy.finalized_ttl.is_some()
            && let Err(error) = crate_times.capture(&directory)
        {
            record_error(&mut report, &directory, error);
            continue;
        }
        let empty_was_old = stamp.aged(now, grace);
        let removed_before = report.removed_locks;
        anchors.push((directory.clone(), stamp));
        clean_candidates(&candidates, &anchors, now, policy, remove, &mut report);
        if empty_was_old || report.removed_locks > removed_before {
            let result = (|| {
                if !anchors_match(&anchors) || !entries(&directory)?.is_empty() {
                    return Ok(());
                }
                let current = Stamp::read(&directory)?;
                if !anchors_match(&anchors) || !entries(&directory)?.is_empty() {
                    return Ok(());
                }
                fs::remove_dir(&directory)?;
                report.removed_empty_dirs += 1;
                report.removed.push((directory.clone(), current.allocated));
                Ok(())
            })();
            if let Err(error) = result {
                record_error(&mut report, &directory, error)
            }
        }
        crate_times.restore(&mut report);
        anchors.pop();
    }
    root_times.restore(&mut report);
    report
}

#[derive(Clone)]
struct RetirementPlan {
    root: PathBuf,
    candidates: Vec<PathBuf>,
    complete: bool,
    observed: BTreeMap<PathBuf, Stamp>,
}

pub(super) struct RetirementGuard {
    locks: sys::SessionLocks,
    plans: Vec<RetirementPlan>,
    lock_stamps: BTreeMap<PathBuf, Stamp>,
    retired_locks: HashSet<PathBuf>,
    absent: HashSet<PathBuf>,
    inode_paths: HashMap<(u64, u64), Vec<(usize, PathBuf)>>,
    started: bool,
}

impl RetirementGuard {
    pub(super) fn revalidate(&self) -> io::Result<bool> {
        if !self.locks_match() {
            return Ok(false);
        }
        for path in &self.absent {
            match sys::lstat(path) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                _ => return Ok(false),
            }
        }
        for plan in &self.plans {
            if self.absent.contains(&plan.root) {
                continue;
            }
            let (current, _) =
                match observe_retirement(&plan.root, &plan.candidates, plan.complete, None) {
                    Ok(Some(current)) => current,
                    Ok(None) => return Ok(false),
                    Err(error) if retained(&error) => return Ok(false),
                    Err(error) => return Err(error),
                };
            if current != plan.observed {
                return Ok(false);
            }
        }
        Ok(self.locks_match())
    }

    fn locks_match(&self) -> bool {
        self.lock_stamps
            .iter()
            .filter(|(path, _)| !self.retired_locks.contains(*path))
            .all(|(path, before)| {
                self.locks.matches_path(path)
                    && self
                        .locks
                        .metadata(path)
                        .and_then(|meta| Stamp::from_meta(&meta))
                        .is_ok_and(|stamp| stamp == *before)
            })
    }

    // Joint proof runs once; held compiler locks let later removals validate only their affected subtree.
    fn begin_removal(&mut self) -> io::Result<()> {
        if !self.started {
            if !self.revalidate()? {
                return Err(io::Error::from(io::ErrorKind::WouldBlock));
            }
            self.started = true;
        }
        Ok(())
    }

    fn anchors_for(&self, path: &Path) -> io::Result<()> {
        for ancestor in path.ancestors().skip(1) {
            for plan in &self.plans {
                if let Some(before) = plan.observed.get(ancestor) {
                    let after = Stamp::read(ancestor)?;
                    if !after.directory || after.dev != before.dev || after.ino != before.ino {
                        return Err(io::Error::from(io::ErrorKind::WouldBlock));
                    }
                }
            }
        }
        Ok(())
    }

    fn check_tree(&self, path: &Path) -> io::Result<()> {
        self.anchors_for(path)?;
        let current = tree_snapshot(path)?;
        let expected: BTreeMap<_, _> = self
            .plans
            .iter()
            .flat_map(|plan| {
                plan.observed
                    .range(path.to_path_buf()..)
                    .take_while(|(known, _)| known.starts_with(path))
                    .map(|(known, stamp)| (known.clone(), stamp.clone()))
            })
            .collect();
        if current == expected {
            Ok(())
        } else {
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        }
    }

    fn removed(&mut self, path: &Path) -> io::Result<()> {
        match sys::lstat(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            _ => return Err(io::Error::from(io::ErrorKind::WouldBlock)),
        }
        if self.lock_stamps.contains_key(path) {
            self.retired_locks.insert(path.to_path_buf());
        }
        self.absent.retain(|known| !known.starts_with(path));
        self.absent.insert(path.to_path_buf());
        let mut removed_links: HashMap<(u64, u64), u64> = HashMap::new();
        for plan in &mut self.plans {
            let keys: Vec<_> = plan
                .observed
                .range(path.to_path_buf()..)
                .take_while(|(known, _)| known.starts_with(path))
                .map(|(known, _)| known.clone())
                .collect();
            for key in keys {
                let stamp = plan.observed.remove(&key).unwrap();
                if !stamp.directory {
                    *removed_links.entry((stamp.dev, stamp.ino)).or_default() += 1;
                }
            }
            plan.candidates.retain(|known| !known.starts_with(path));
            if let Some(parent) = path.parent()
                && let Some(before) = plan.observed.get_mut(parent)
            {
                let after = Stamp::read(parent)?;
                if !after.directory || after.dev != before.dev || after.ino != before.ino {
                    return Err(io::Error::from(io::ErrorKind::WouldBlock));
                }
                *before = after;
            }
            if plan.candidates.is_empty() && !plan.complete {
                plan.observed.retain(|known, _| known == &plan.root);
            }
        }
        // Owned hard-link unlinks change only surviving aliases' link count and change time.
        for (identity, removed) in removed_links {
            let Some(aliases) = self.inode_paths.get(&identity) else {
                continue;
            };
            let survivor = aliases.iter().find_map(|(index, path)| {
                self.plans[*index]
                    .observed
                    .get(path)
                    .map(|stamp| (path, stamp))
            });
            let Some((survivor_path, before)) = survivor else {
                continue;
            };
            let after = Stamp::read(survivor_path)?;
            let mut expected = before.clone();
            expected.links = expected
                .links
                .checked_sub(removed)
                .ok_or_else(|| io::Error::from(io::ErrorKind::WouldBlock))?;
            expected.ctime = after.ctime;
            if after != expected {
                return Err(io::Error::from(io::ErrorKind::WouldBlock));
            }
            for (index, alias) in aliases {
                if let Some(stamp) = self.plans[*index].observed.get_mut(alias) {
                    stamp.links = expected.links;
                    stamp.ctime = expected.ctime;
                }
            }
        }
        Ok(())
    }

    fn remove_payload(
        &mut self,
        path: &Path,
        remove: &mut impl FnMut(&Path) -> io::Result<()>,
    ) -> io::Result<()> {
        self.check_tree(path)?;
        let base = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| finalized_base(name).or_else(|| working_base(name)))
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        let lock = path.parent().unwrap().join(format!("{base}.lock"));
        if !self.locks.matches_path(&lock)
            || self
                .locks
                .metadata(&lock)
                .and_then(|meta| Stamp::from_meta(&meta))?
                != self.lock_stamps[&lock]
        {
            return Err(io::Error::from(io::ErrorKind::WouldBlock));
        }
        remove(path)?;
        self.removed(path)
    }

    fn remove_with(
        &mut self,
        candidate: &Path,
        remove: &mut impl FnMut(&Path) -> io::Result<()>,
    ) -> io::Result<()> {
        self.begin_removal()?;
        let plan = self
            .plans
            .iter()
            .find(|plan| plan.candidates.iter().any(|path| path == candidate))
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        let whole_crate = candidate.parent() == Some(plan.root.as_path());
        if !whole_crate {
            let Some(selection) = finalized_selection(candidate.parent().unwrap())? else {
                return Err(io::Error::from(io::ErrorKind::WouldBlock));
            };
            if !selection.older.contains(&candidate.to_path_buf()) || !self.locks_match() {
                return Err(io::Error::from(io::ErrorKind::WouldBlock));
            }
            for (_, survivor) in selection.newest {
                if !plan.observed.get(&survivor).is_some_and(|before| Stamp::read(&survivor).is_ok_and(|after| after == *before)) {
                    return Err(io::Error::from(io::ErrorKind::WouldBlock));
                }
            }
            return self.remove_payload(candidate, remove);
        }
        self.check_tree(candidate)?;
        // All session payloads disappear before any paired lock can be unlinked.
        let payloads: Vec<_> = entries(candidate)?
            .into_iter()
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        finalized_base(name)
                            .or_else(|| working_base(name))
                            .is_some()
                    })
            })
            .collect();
        for payload in payloads {
            self.remove_payload(&payload, remove)?;
        }
        self.check_tree(candidate)?;
        let locks: Vec<_> = self
            .lock_stamps
            .keys()
            .filter(|path| path.parent() == Some(candidate))
            .cloned()
            .collect();
        for lock in locks {
            self.anchors_for(&lock)?;
            if !self.locks.matches_path(&lock)
                || self
                    .locks
                    .metadata(&lock)
                    .and_then(|meta| Stamp::from_meta(&meta))?
                    != self.lock_stamps[&lock]
            {
                return Err(io::Error::from(io::ErrorKind::WouldBlock));
            }
            fs::remove_file(&lock)?;
            self.removed(&lock)?;
        }
        self.anchors_for(candidate)?;
        fs::remove_dir(candidate)?;
        self.removed(candidate)
    }

    pub(super) fn remove(&mut self, candidate: &Path) -> io::Result<()> {
        self.remove_with(candidate, &mut |path| fs::remove_dir_all(path))
    }

    fn remove_root_with(
        &mut self,
        root: &Path,
        remove: &mut impl FnMut(&Path) -> io::Result<()>,
    ) -> io::Result<()> {
        self.begin_removal()?;
        let candidates = self
            .plans
            .iter()
            .find(|plan| plan.root == root && plan.complete)
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?
            .candidates
            .clone();
        let mut expected = candidates.clone();
        expected.sort();
        if entries(root)? != expected {
            return Err(io::Error::from(io::ErrorKind::WouldBlock));
        }
        for candidate in candidates {
            self.remove_with(&candidate, remove)?;
        }
        self.anchors_for(root)?;
        fs::remove_dir(root)?;
        self.removed(root)
    }

    pub(super) fn remove_root(&mut self, root: &Path) -> io::Result<()> {
        self.remove_root_with(root, &mut |path| fs::remove_dir_all(path))
    }
}

fn finalized_base(name: &str) -> Option<&str> {
    let (base, hash) = name.rsplit_once('-')?;
    if !component(hash, 25) {
        return None;
    }
    let formatted = format!("{base}.lock");
    lock_base(&formatted).map(|_| base)
}

pub(super) struct FinalizedSelection {
    pub(super) newest: Vec<(SystemTime, PathBuf)>,
    pub(super) older: Vec<PathBuf>,
}

// Rank native creation timestamps; directory mtimes remain age evidence only.
pub(super) fn finalized_selection(directory: &Path) -> io::Result<Option<FinalizedSelection>> {
    let before = Stamp::read(directory)?;
    if !before.directory {
        return Ok(None);
    }
    let mut finalized = Vec::new();
    for path in entries(directory)? {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            return Ok(None);
        };
        let meta = sys::lstat(&path)?;
        if meta.file_type().is_symlink() {
            return Ok(None);
        }
        let modified = meta.modified()?;
        if let Some(base) = finalized_base(name) {
            if !meta.is_dir() {
                return Ok(None);
            }
            let Some(timestamp) = base.split('-').nth(1).and_then(|part| u64::from_str_radix(part, 36).ok()) else {
                return Ok(None);
            };
            finalized.push((timestamp, modified, path));
        } else if working_base(name).is_some() {
            if !meta.is_dir() {
                return Ok(None);
            }
        } else if lock_base(name).is_some() {
            if !valid_lock(&path, before.dev)? {
                return Ok(None);
            }
        } else {
            return Ok(None);
        }
    }
    if Stamp::read(directory)? != before {
        return Ok(None);
    }
    let Some(maximum) = finalized.iter().map(|entry| entry.0).max() else {
        return Ok(None);
    };
    let mut selection = FinalizedSelection { newest: Vec::new(), older: Vec::new() };
    for (timestamp, modified, path) in finalized {
        if timestamp == maximum {
            selection.newest.push((modified, path));
        } else {
            selection.older.push(path);
        }
    }
    Ok(Some(selection))
}

fn valid_lock(path: &Path, device: u64) -> io::Result<bool> {
    let stamp = Stamp::read(path)?;
    Ok(!stamp.directory && stamp.dev == device && stamp.len == 0 && stamp.links == 1)
}

// Whole-crate retirement accepts published or proved-abandoned sessions with their existing locks.
fn crate_locks(directory: &Path, now: Option<SystemTime>) -> io::Result<Option<Vec<PathBuf>>> {
    let device = Stamp::read(directory)?.dev;
    let mut locks = BTreeMap::new();
    let mut sessions = HashSet::new();
    let mut working = Vec::new();
    for path in entries(directory)? {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            return Ok(None);
        };
        if let Some(base) = lock_base(name) {
            if !valid_lock(&path, device)? {
                return Ok(None);
            }
            locks.insert(base.to_owned(), path);
        } else if let Some(base) = finalized_base(name).or_else(|| working_base(name)) {
            if !Stamp::read(&path)?.directory || !sessions.insert(base.to_owned()) {
                return Ok(None);
            }
            if working_base(name).is_some() {
                working.push((base.to_owned(), path));
            }
        } else {
            return Ok(None);
        }
    }
    if sessions.iter().any(|base| !locks.contains_key(base)) {
        return Ok(None);
    }
    // Immutable observations carry this age proof through admission and subsequent owned changes.
    if let Some(now) = now {
        for (base, path) in working {
            if !Stamp::read(&locks[&base])?.aged(now, Duration::from_secs(3600)) {
                return Ok(None);
            }
            snapshot(&path, now, Duration::from_secs(3600))?;
        }
    }
    Ok(Some(locks.into_values().collect()))
}

fn working_base(name: &str) -> Option<&str> {
    let base = name.strip_suffix("-working")?;
    lock_base(&format!("{base}.lock")).map(|_| base)
}

type RetirementObservation = (BTreeMap<PathBuf, Stamp>, Vec<PathBuf>);

fn observe_retirement(
    root: &Path,
    candidates: &[PathBuf],
    complete: bool,
    now: Option<SystemTime>,
) -> io::Result<Option<RetirementObservation>> {
    let root_stamp = Stamp::read(root)?;
    if !root_stamp.directory {
        return Ok(None);
    }
    let mut sorted = candidates.to_vec();
    sorted.sort();
    if sorted.windows(2).any(|pair| pair[0] == pair[1]) {
        return Ok(None);
    }
    if complete && entries(root)? != sorted {
        return Ok(None);
    }
    let mut observed = BTreeMap::from([(root.to_path_buf(), root_stamp.clone())]);
    let mut lock_paths = Vec::new();
    for candidate in sorted {
        let parent = candidate
            .parent()
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        if parent == root {
            if !Stamp::read(&candidate)?.directory {
                return Ok(None);
            }
            let Some(locks) = crate_locks(&candidate, now)? else {
                return Ok(None);
            };
            lock_paths.extend(locks);
        // Unrelated working sessions do not affect a selected finalized session.
        } else if !complete && parent.parent() == Some(root) {
            let Some(base) = candidate
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(finalized_base)
            else {
                return Ok(None);
            };
            let Some(selection) = finalized_selection(parent)? else {
                return Ok(None);
            };
            if !selection.older.contains(&candidate) {
                return Ok(None);
            }
            for (_, survivor) in selection.newest {
                let Some(survivor_base) = survivor.file_name().and_then(|name| name.to_str()).and_then(finalized_base) else {
                    return Ok(None);
                };
                let survivor_lock = parent.join(format!("{survivor_base}.lock"));
                if !valid_lock(&survivor_lock, root_stamp.dev)? {
                    return Ok(None);
                }
                observed.insert(survivor.clone(), Stamp::read(&survivor)?);
                observed.insert(survivor_lock.clone(), Stamp::read(&survivor_lock)?);
                lock_paths.push(survivor_lock);
            }
            let container = Stamp::read(parent)?;
            if !container.directory || siblings(parent, base)? != [candidate.clone()] {
                return Ok(None);
            }
            let lock = parent.join(format!("{base}.lock"));
            if !valid_lock(&lock, container.dev)? {
                return Ok(None);
            }
            observed.insert(parent.to_path_buf(), container);
            observed.insert(lock.clone(), Stamp::read(&lock)?);
            lock_paths.push(lock);
        } else {
            return Ok(None);
        }
        for (path, stamp) in tree_snapshot(&candidate)? {
            if stamp.dev != root_stamp.dev {
                return Ok(None);
            }
            if observed.insert(path, stamp).is_some() {
                return Ok(None);
            }
        }
    }
    lock_paths.sort();
    lock_paths.dedup();
    Ok(Some((observed, lock_paths)))
}

fn admit_retirement(sets: &[(PathBuf, Vec<PathBuf>, bool)]) -> io::Result<Option<RetirementGuard>> {
    admit_retirement_at(sets, SystemTime::now())
}

fn admit_retirement_at(
    sets: &[(PathBuf, Vec<PathBuf>, bool)],
    now: SystemTime,
) -> io::Result<Option<RetirementGuard>> {
    let mut plans = Vec::new();
    let mut paths = Vec::new();
    let mut directories = HashMap::new();
    // Reject aliases across roots before the shared guard opens and locks any session files.
    for (root, candidates, complete) in sets {
        let observed = match observe_retirement(root, candidates, *complete, Some(now)) {
            Ok(Some(observed)) => observed,
            Ok(None) => return Ok(None),
            Err(error) if retained(&error) => return Ok(None),
            Err(error) => return Err(error),
        };
        for (path, stamp) in &observed.0 {
            if stamp.directory
                && let Some(prior) = directories.insert((stamp.dev, stamp.ino), path.clone())
                && prior != *path
            {
                return Ok(None);
            }
        }
        paths.extend(observed.1);
        plans.push(RetirementPlan {
            root: root.clone(),
            candidates: candidates.clone(),
            complete: *complete,
            observed: observed.0,
        });
    }
    let locks = match sys::try_session_locks(&paths) {
        Ok(locks) => locks,
        Err(error) if retained(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    let lock_stamps = paths
        .into_iter()
        .map(|path| {
            let stamp = Stamp::from_meta(&locks.metadata(&path)?)?;
            Ok((path, stamp))
        })
        .collect::<io::Result<_>>()?;
    let mut inode_paths: HashMap<_, Vec<_>> = HashMap::new();
    for (index, plan) in plans.iter().enumerate() {
        for (path, stamp) in &plan.observed {
            if !stamp.directory {
                inode_paths
                    .entry((stamp.dev, stamp.ino))
                    .or_default()
                    .push((index, path.clone()));
            }
        }
    }
    let guard = RetirementGuard {
        locks,
        plans,
        lock_stamps,
        retired_locks: HashSet::new(),
        absent: HashSet::new(),
        inode_paths,
        started: false,
    };
    if !guard.revalidate()? {
        return Ok(None);
    }
    Ok(Some(guard))
}

/// Admission covers the selected payloads; callers keep the guard until their removals finish.
pub(super) fn try_retirement_guard(
    root: &Path,
    candidate: &Path,
) -> io::Result<Option<RetirementGuard>> {
    admit_retirement(&[(root.to_path_buf(), vec![candidate.to_path_buf()], false)])
}

pub(super) fn try_retirement_guards(
    root: &Path,
    candidates: &[PathBuf],
) -> io::Result<Option<RetirementGuard>> {
    admit_retirement(&[(root.to_path_buf(), candidates.to_vec(), true)])
}

pub(super) fn try_retirement_sets(
    sets: &[(PathBuf, Vec<PathBuf>)],
) -> io::Result<Option<RetirementGuard>> {
    if let [(root, candidates)] = sets {
        return try_retirement_guards(root, candidates);
    }
    let sets: Vec<_> = sets
        .iter()
        .map(|(root, candidates)| (root.clone(), candidates.clone(), true))
        .collect();
    admit_retirement(&sets)
}
