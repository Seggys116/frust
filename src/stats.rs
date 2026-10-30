//! Aggregation of the event ledger and cache/target inventory for `frust stats`.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use crate::events::{CacheResult, Event};
use crate::fsx;
use crate::paths::Dirs;

#[derive(Debug, Default, Clone)]
pub struct Totals {
    pub hits: u64,
    pub misses: u64,
    pub saved_ms: u64,
    pub hit_bytes: u64,
    pub runs: u64,
    pub failed_runs: u64,
    pub run_wall_ms: u64,
}

impl Totals {
    pub fn hit_rate(&self) -> f64 {
        let total = self.hits + self.misses;
        if total == 0 { 0.0 } else { self.hits as f64 / total as f64 }
    }
}

/// Per-workspace build history: how much of each build was reused rather than recompiled.
#[derive(Debug, Default, Clone)]
pub struct WorkspaceUse {
    pub name: String,
    pub runs: u64,
    pub units: u64,
    pub fresh: u64,
    pub cached: u64,
    pub compiled: u64,
    pub idle_wall_ms: u64,
    idle_runs: u64,
    pub edit_wall_ms: u64,
    edit_runs: u64,
}

impl WorkspaceUse {
    /// Share of units that needed no compiler work: already fresh or restored from the cache.
    pub fn reuse_rate(&self) -> f64 {
        if self.units == 0 { 0.0 } else { (self.fresh + self.cached) as f64 / self.units as f64 }
    }
}

pub fn workspace_use(events: &[Event]) -> Vec<WorkspaceUse> {
    let mut by_name: std::collections::BTreeMap<String, WorkspaceUse> = std::collections::BTreeMap::new();
    for event in events {
        let Event::Run { workspace, wall_ms, units, cached, compiled, fresh, success: true, .. } = event else { continue };
        let entry = by_name.entry(workspace.clone()).or_insert_with(|| WorkspaceUse { name: workspace.clone(), ..Default::default() });
        entry.runs += 1;
        entry.units += *units as u64;
        entry.fresh += *fresh as u64;
        entry.cached += *cached as u64;
        entry.compiled += *compiled as u64;
        if *compiled == 0 {
            entry.idle_wall_ms += wall_ms;
            entry.idle_runs += 1;
        } else {
            entry.edit_wall_ms += wall_ms;
            entry.edit_runs += 1;
        }
    }
    let mut out: Vec<WorkspaceUse> = by_name.into_values().collect();
    for w in &mut out {
        w.idle_wall_ms = w.idle_wall_ms.checked_div(w.idle_runs).unwrap_or(0);
        w.edit_wall_ms = w.edit_wall_ms.checked_div(w.edit_runs).unwrap_or(0);
    }
    out.sort_by_key(|w| std::cmp::Reverse(w.runs));
    out
}

/// Bytes held by rustc's incremental session data under a central target directory.
pub fn incremental_bytes(target: &std::path::Path) -> u64 {
    let mut total = 0;
    let Ok(profiles) = fs::read_dir(target) else { return 0 };
    for profile in profiles.flatten() {
        total += fsx::allocated_size(&profile.path().join("incremental"));
    }
    total
}

pub fn read_ledger(dirs: &Dirs) -> Vec<Event> {
    let Ok(text) = fs::read_to_string(dirs.ledger()) else { return Vec::new() };
    text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

pub fn totals(events: &[Event]) -> Totals {
    let mut t = Totals::default();
    for event in events {
        match event {
            Event::Cache { result: CacheResult::Hit, saved_ms, bytes, .. } => {
                t.hits += 1;
                t.saved_ms += saved_ms;
                t.hit_bytes += bytes;
            }
            Event::Cache { result: CacheResult::Miss, .. } => t.misses += 1,
            Event::Run { wall_ms, success, .. } => {
                t.runs += 1;
                t.run_wall_ms += wall_ms;
                if !success {
                    t.failed_runs += 1;
                }
            }
        }
    }
    t
}

#[derive(Debug, Clone)]
pub struct StoreInfo {
    pub entries: u64,
    pub logical_bytes: u64,
    pub allocated_bytes: u64,
}

pub fn store_info(dirs: &Dirs) -> StoreInfo {
    let mut info = StoreInfo { entries: 0, logical_bytes: 0, allocated_bytes: 0 };
    for entry in entry_dirs(dirs) {
        info.entries += 1;
        info.logical_bytes += logical_size(&entry.path);
        info.allocated_bytes += fsx::allocated_size(&entry.path);
    }
    info
}

#[derive(Debug, Clone)]
pub struct EntryRef {
    pub path: PathBuf,
    pub used: SystemTime,
}

pub fn entry_dirs(dirs: &Dirs) -> Vec<EntryRef> {
    let mut out = Vec::new();
    let Ok(shards) = fs::read_dir(dirs.store()) else { return out };
    for shard in shards.flatten() {
        let Ok(entries) = fs::read_dir(shard.path()) else { continue };
        for entry in entries.flatten() {
            let used = fs::metadata(entry.path().join("meta.json"))
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            out.push(EntryRef { path: entry.path(), used });
        }
    }
    out
}

pub fn logical_size(path: &std::path::Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else { return 0 };
    if meta.is_dir() {
        fs::read_dir(path).map(|rd| rd.flatten().map(|e| logical_size(&e.path())).sum()).unwrap_or(0)
    } else {
        meta.len()
    }
}

#[derive(Debug, Clone)]
pub struct TargetInfo {
    pub workspace: String,
    pub toolchain: String,
    pub path: PathBuf,
    pub allocated_bytes: u64,
    pub idle: Duration,
    pub root_exists: bool,
}

pub fn targets(dirs: &Dirs) -> Vec<TargetInfo> {
    let mut out = Vec::new();
    let Ok(workspaces) = fs::read_dir(dirs.targets()) else { return out };
    for ws in workspaces.flatten() {
        let marker = fs::read_to_string(ws.path().join("workspace")).ok();
        let root_exists = marker.as_deref().map(|p| std::path::Path::new(p.trim()).exists()).unwrap_or(true);
        let Ok(toolchains) = fs::read_dir(ws.path()) else { continue };
        for tc in toolchains.flatten() {
            if !tc.path().is_dir() {
                continue;
            }
            out.push(TargetInfo {
                workspace: ws.file_name().to_string_lossy().into_owned(),
                toolchain: tc.file_name().to_string_lossy().into_owned(),
                allocated_bytes: fsx::allocated_size(&tc.path()),
                idle: idle_time(&tc.path()),
                path: tc.path(),
                root_exists,
            });
        }
    }
    out.sort_by_key(|t| std::cmp::Reverse(t.allocated_bytes));
    out
}

/// Time since anything at the top two levels of a target directory changed.
fn idle_time(dir: &std::path::Path) -> Duration {
    let mut newest = SystemTime::UNIX_EPOCH;
    let mut consider = |path: &std::path::Path| {
        if let Ok(m) = fs::metadata(path).and_then(|m| m.modified()) {
            newest = newest.max(m);
        }
    };
    consider(dir);
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            consider(&entry.path());
        }
    }
    SystemTime::now().duration_since(newest).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn totals_count_hits_misses_and_runs() {
        let events = vec![
            Event::Cache { t: 1, result: CacheResult::Hit, krate: "a".into(), version: "1".into(), compile_ms: 100, saved_ms: 90, bytes: 5 },
            Event::Cache { t: 2, result: CacheResult::Miss, krate: "b".into(), version: "1".into(), compile_ms: 100, saved_ms: 0, bytes: 5 },
            Event::Run { t: 3, workspace: "w".into(), command: "build".into(), wall_ms: 1000, units: 2, cached: 1, compiled: 1, fresh: 0, saved_ms: 90, success: false },
        ];
        let t = totals(&events);
        assert_eq!((t.hits, t.misses, t.runs, t.failed_runs, t.saved_ms), (1, 1, 1, 1, 90));
        assert!((t.hit_rate() - 0.5).abs() < f64::EPSILON);
    }
}

#[cfg(test)]
mod workspace_tests {
    use super::*;

    fn run(workspace: &str, wall_ms: u64, units: u32, cached: u32, compiled: u32, fresh: u32, success: bool) -> Event {
        Event::Run { t: 0, workspace: workspace.into(), command: "build".into(), wall_ms, units, cached, compiled, fresh, saved_ms: 0, success }
    }

    #[test]
    fn reuse_counts_fresh_and_cached_and_ignores_failures() {
        let events = vec![
            run("app", 4000, 10, 2, 8, 0, true),
            run("app", 100, 10, 0, 0, 10, true),
            run("app", 900, 10, 0, 1, 9, true),
            run("app", 50, 10, 0, 0, 0, false),
        ];
        let w = &workspace_use(&events)[0];
        assert_eq!((w.runs, w.units, w.fresh, w.cached, w.compiled), (3, 30, 19, 2, 9));
        assert!((w.reuse_rate() - 21.0 / 30.0).abs() < 1e-9);
        assert_eq!(w.idle_wall_ms, 100);
        assert_eq!(w.edit_wall_ms, 2450);
    }
}
