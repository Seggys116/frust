//! Pure build progress state machine without I/O or its own clock; Cargo stdout, stderr and cache events may arrive in any order.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::RunStats;
use super::term::strip_ansi;
use crate::events::{CacheResult, Event};

const STATUS_VERBS: &[&str] = &[
    "Compiling",
    "Checking",
    "Documenting",
    "Fresh",
    "Downloading",
    "Downloaded",
    "Updating",
    "Locking",
    "Adding",
    "Removing",
    "Blocking",
    "Finished",
    "Running",
    "Packaging",
    "Generated",
    "Building",
    "Waiting",
];

#[derive(Debug, Clone)]
pub struct ActiveUnit {
    pub name: String,
    pub version: Option<String>,
    pub local: bool,
    pub build_script: bool,
    pub started: Instant,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Completion {
    pub name: String,
    pub version: Option<String>,
    pub local: bool,
    pub build_script: bool,
    pub duration: Duration,
    pub cached: bool,
    pub saved_ms: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LockView {
    pub pid: Option<u32>,
    pub command: String,
    pub held: Duration,
    pub ahead: usize,
}

pub struct Model {
    pub stats: RunStats,
    pub active: Vec<ActiveUnit>,
    pub transient: Option<String>,
    pub lock: Option<LockView>,
    pub profile: Option<String>,
    pub build_finished: Option<bool>,
    pub cache_hits: u32,
    pub cache_misses: u32,
    expected: Option<u32>,
    /// Hits not yet matched to a completion, by normalised crate name, with their saved time.
    pending_hits: HashMap<String, VecDeque<u64>>,
    /// Name keys of completed compiles that a late hit may still convert to cached.
    late: Vec<Vec<String>>,
    /// Artifacts that finished before their `Compiling` line was seen, by normalised name.
    orphans: HashMap<String, u32>,
}

pub fn norm(name: &str) -> String {
    name.replace('-', "_")
}

impl Model {
    pub fn new(expected: Option<u32>) -> Model {
        Model {
            stats: RunStats::default(),
            active: Vec::new(),
            transient: None,
            lock: None,
            profile: None,
            build_finished: None,
            cache_hits: 0,
            cache_misses: 0,
            expected: expected.filter(|&n| n > 0),
            pending_hits: HashMap::new(),
            late: Vec::new(),
            orphans: HashMap::new(),
        }
    }

    /// An exact count replaces the history estimate once cargo has planned the build.
    pub fn set_expected(&mut self, expected: u32) {
        self.expected = Some(expected).filter(|&n| n > 0);
    }

    pub fn done(&self) -> u32 {
        self.stats.units
    }

    /// Denominator for display. Never below the finished count, since it is only an estimate.
    pub fn total(&self) -> Option<u32> {
        self.expected.map(|e| e.max(self.stats.units))
    }

    /// Held at 99 while running: the estimate is from an earlier build, so 100 would claim an unreported finish.
    pub fn percent(&self) -> Option<u32> {
        let expected = self.expected?;
        let done = u64::from(self.stats.units.min(expected));
        Some(((done * 100 / u64::from(expected)) as u32).min(99))
    }

    pub fn on_stderr(&mut self, line: &str, now: Instant) -> Option<String> {
        let plain = strip_ansi(line);
        let Some((verb, rest)) = parse_status(&plain) else {
            self.count_diagnostic(&plain);
            return Some(line.to_string());
        };
        match verb {
            "Compiling" | "Checking" | "Documenting" => self.start_unit(rest, now),
            "Fresh" => {}
            "Finished" => {
                self.transient = None;
                self.profile = parse_profile(rest);
            }
            "Blocking" => self.transient = Some("waiting for cargo's own file lock".to_string()),
            "Running" | "Packaging" | "Generated" => return Some(line.to_string()),
            _ => self.transient = Some(format!("{verb} {rest}")),
        }
        None
    }

    fn count_diagnostic(&mut self, plain: &str) {
        if plain.starts_with("warning:") {
            // The per-crate roll-up repeats warnings already counted; "build failed" is a status note.
            let rollup = plain.starts_with("warning: `") && plain.contains(") generated ");
            if !rollup && !plain.starts_with("warning: build failed") {
                self.stats.warnings += 1;
            }
        } else if plain.starts_with("error[") || plain.starts_with("error:") {
            let summary = plain.starts_with("error: could not compile") || plain.starts_with("error: aborting due to");
            if !summary {
                self.stats.errors += 1;
            }
        }
    }

    fn start_unit(&mut self, rest: &str, now: Instant) {
        let (head, build_script) = match rest.strip_suffix("(build script)") {
            Some(h) => (h.trim_end(), true),
            None => (rest, false),
        };
        let mut parts = head.splitn(3, ' ');
        let Some(name) = parts.next().filter(|n| !n.is_empty()) else { return };
        let version = parts.next().and_then(|v| v.strip_prefix('v')).map(str::to_string);
        let local = parts.next().is_some_and(|r| r.trim_start().starts_with('('));
        if let Some(n) = self.orphans.get_mut(&norm(name)).filter(|n| **n > 0) {
            *n -= 1;
            return;
        }
        self.transient = None;
        self.active.push(ActiveUnit { name: name.to_string(), version, local, build_script, started: now });
    }

    pub fn on_json(&mut self, msg: &Value, now: Instant) -> Option<Completion> {
        match msg.get("reason")?.as_str()? {
            "compiler-artifact" => self.on_artifact(msg, now),
            "build-finished" => {
                self.build_finished = msg.get("success").and_then(Value::as_bool);
                None
            }
            _ => None,
        }
    }

    fn on_artifact(&mut self, msg: &Value, now: Instant) -> Option<Completion> {
        let id = msg.get("package_id").and_then(Value::as_str).unwrap_or("");
        let target_name = msg.pointer("/target/name").and_then(Value::as_str).unwrap_or("");
        let custom_build = msg
            .pointer("/target/kind")
            .and_then(Value::as_array)
            .is_some_and(|kinds| kinds.iter().any(|k| k.as_str() == Some("custom-build")));
        let fresh = msg.get("fresh").and_then(Value::as_bool).unwrap_or(false);
        let (id_name, id_version) = parse_package_id(id);
        let path_dep = id.starts_with("path+");

        self.transient = None;
        if fresh {
            self.stats.fresh += 1;
            self.stats.units += 1;
            return None;
        }

        let display = id_name.unwrap_or_else(|| target_name.to_string());
        if display.is_empty() {
            return None;
        }
        // Path-dependency ids may carry only a directory name, so the target name is a second match key.
        let keys: Vec<String> = [display.as_str(), target_name].iter().filter(|n| !n.is_empty()).map(|n| norm(n)).collect();
        let position = self
            .active
            .iter()
            .position(|u| u.build_script == custom_build && keys.contains(&norm(&u.name)))
            .or_else(|| self.active.iter().position(|u| keys.contains(&norm(&u.name))));

        let (name, version, local, build_script, duration) = match position.map(|i| self.active.remove(i)) {
            Some(u) => (
                u.name,
                id_version.or(u.version),
                u.local || path_dep,
                u.build_script || custom_build,
                now.saturating_duration_since(u.started),
            ),
            None => {
                *self.orphans.entry(norm(&display)).or_default() += 1;
                (display, id_version, path_dep, custom_build, Duration::ZERO)
            }
        };

        let hit = keys.iter().find_map(|k| self.pending_hits.get_mut(k).and_then(|q| q.pop_front()));
        self.stats.units += 1;
        if hit.is_some() {
            self.stats.cached += 1;
        } else {
            self.stats.compiled += 1;
            self.late.push(keys);
        }
        Some(Completion { name, version, local, build_script, duration, cached: hit.is_some(), saved_ms: hit.unwrap_or(0) })
    }

    pub fn on_cache(&mut self, ev: &Event) {
        let Event::Cache { result, krate, saved_ms, .. } = ev else { return };
        match result {
            CacheResult::Miss => self.cache_misses += 1,
            CacheResult::Hit => {
                self.cache_hits += 1;
                self.stats.saved_ms += saved_ms;
                let key = norm(krate);
                let in_flight = self.active.iter().any(|u| norm(&u.name) == key);
                let completed = if in_flight { None } else { self.late.iter().position(|keys| keys.contains(&key)) };
                match completed {
                    Some(i) => {
                        self.late.remove(i);
                        self.stats.compiled = self.stats.compiled.saturating_sub(1);
                        self.stats.cached += 1;
                    }
                    None => self.pending_hits.entry(key).or_default().push_back(*saved_ms),
                }
            }
        }
    }
}

/// Cargo right-aligns the verb, so leading spaces are skipped; a first word that is not a known verb is free text.
fn parse_status(line: &str) -> Option<(&str, &str)> {
    let (verb, rest) = line.trim_start_matches(' ').split_once(' ')?;
    STATUS_VERBS.contains(&verb).then_some((verb, rest.trim()))
}

fn parse_profile(rest: &str) -> Option<String> {
    let head = rest.rfind(" in ").map_or(rest, |i| &rest[..i]);
    let cleaned = head.replace('`', "").replace(" target(s)", "");
    let cleaned = cleaned.trim();
    (!cleaned.is_empty()).then(|| cleaned.to_string())
}

/// Accepts the old (`serde 1.0.200 (registry+...)`) and new (`registry+...#serde@1.0.200`) package id forms.
pub fn parse_package_id(id: &str) -> (Option<String>, Option<String>) {
    let some = |s: &str| (!s.is_empty()).then(|| s.to_string());
    let mut words = id.split_whitespace();
    let first = words.next().unwrap_or("");
    // The old form starts with a bare name; its parenthesised source may itself contain '#'.
    if id.contains(' ') && !first.contains("://") && !first.contains(['+', '#']) {
        let version = words.next().filter(|v| v.starts_with(|c: char| c.is_ascii_digit()));
        return (some(first), version.and_then(some));
    }
    let Some((url, frag)) = id.split_once('#') else { return (None, None) };
    if let Some((name, version)) = frag.split_once('@') {
        return (some(name), some(version));
    }
    let url = url.split('?').next().unwrap_or(url);
    let dir = url.trim_end_matches('/').rsplit('/').next().unwrap_or("").trim_end_matches(".git");
    // A bare fragment is a version for path dependencies and a commit hash for git ones.
    let is_version = frag.starts_with(|c: char| c.is_ascii_digit()) && frag.contains('.');
    (some(dir), if is_version { some(frag) } else { None })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn t0() -> Instant {
        Instant::now()
    }

    fn artifact(id: &str, target: &str, kind: &str, fresh: bool) -> Value {
        json!({"reason": "compiler-artifact", "package_id": id, "target": {"name": target, "kind": [kind]}, "fresh": fresh})
    }

    fn hit(krate: &str, saved_ms: u64) -> Event {
        Event::Cache {
            t: 0,
            result: CacheResult::Hit,
            krate: krate.to_string(),
            version: "1.0.0".to_string(),
            compile_ms: saved_ms,
            saved_ms,
            bytes: 10,
        }
    }

    fn ms(base: Instant, n: u64) -> Instant {
        base + Duration::from_millis(n)
    }

    #[test]
    fn package_id_forms() {
        let reg = "registry+https://github.com/rust-lang/crates.io-index";
        assert_eq!(parse_package_id(&format!("{reg}#serde@1.0.200")), (Some("serde".into()), Some("1.0.200".into())));
        assert_eq!(parse_package_id(&format!("serde 1.0.200 ({reg})")), (Some("serde".into()), Some("1.0.200".into())));
        assert_eq!(parse_package_id("path+file:///work/foo#0.1.0"), (Some("foo".into()), Some("0.1.0".into())));
        assert_eq!(parse_package_id("path+file:///work/foo#bar@0.1.0"), (Some("bar".into()), Some("0.1.0".into())));
        assert_eq!(
            parse_package_id("git+https://github.com/x/y.git?branch=main#0123abcd"),
            (Some("y".into()), None)
        );
        assert_eq!(parse_package_id(""), (None, None));
    }

    #[test]
    fn status_lines_are_not_diagnostics() {
        let mut m = Model::new(None);
        let now = t0();
        assert_eq!(m.on_stderr("\x1b[1m\x1b[32m   Compiling\x1b[0m syn v2.0.1", now), None);
        assert_eq!(m.active.len(), 1);
        assert_eq!(m.active[0].name, "syn");
        assert_eq!(m.active[0].version.as_deref(), Some("2.0.1"));
        assert!(!m.active[0].local);
        assert_eq!(m.on_stderr("   Compiling app v0.1.0 (/work/app)", now), None);
        assert!(m.active[1].local);
        assert_eq!(m.on_stderr("   Compiling serde v1.0.0 (build script)", now), None);
        assert!(m.active[2].build_script && !m.active[2].local);
        assert_eq!(m.on_stderr("  Downloading crates ...", now), None);
        assert_eq!(m.transient.as_deref(), Some("Downloading crates ..."));
        assert_eq!(m.on_stderr("    Blocking waiting for file lock on package cache", now), None);
        assert_eq!(m.transient.as_deref(), Some("waiting for cargo's own file lock"));
        assert_eq!(m.on_stderr("    Finished `dev` profile [unoptimized + debuginfo] target(s) in 4.21s", now), None);
        assert_eq!(m.profile.as_deref(), Some("dev profile [unoptimized + debuginfo]"));
        assert!(m.transient.is_none());
    }

    #[test]
    fn diagnostics_are_returned_and_counted() {
        let mut m = Model::new(None);
        let now = t0();
        let warn = "\x1b[33mwarning\x1b[0m: unused variable: `x`";
        assert_eq!(m.on_stderr(warn, now).as_deref(), Some(warn));
        assert!(m.on_stderr("warning: `app` (lib) generated 1 warning (run `cargo fix`)", now).is_some());
        assert!(m.on_stderr("warning: build failed, waiting for other jobs to finish...", now).is_some());
        assert!(m.on_stderr("error[E0425]: cannot find value `y`", now).is_some());
        assert!(m.on_stderr("error: could not compile `app` (lib) due to 1 previous error", now).is_some());
        assert!(m.on_stderr("   = note: warning: not at line start", now).is_some());
        assert_eq!(m.stats.warnings, 1);
        assert_eq!(m.stats.errors, 1);
    }

    #[test]
    fn fresh_units_are_counted_and_never_active() {
        let mut m = Model::new(None);
        let now = t0();
        assert!(m.on_json(&artifact("registry+x#itoa@1.0.0", "itoa", "lib", true), now).is_none());
        assert_eq!((m.stats.fresh, m.stats.units), (1, 1));
        assert!(m.active.is_empty());
    }

    #[test]
    fn completion_matches_oldest_unit_and_measures_duration() {
        let mut m = Model::new(None);
        let base = t0();
        m.on_stderr("   Compiling syn v2.0.1", ms(base, 0));
        m.on_stderr("   Compiling syn v2.0.1", ms(base, 100));
        let c = m.on_json(&artifact("registry+x#syn@2.0.1", "syn", "lib", false), ms(base, 1500)).unwrap();
        assert_eq!(c.duration, Duration::from_millis(1500));
        assert!(!c.cached);
        assert_eq!(m.active.len(), 1);
        assert_eq!((m.stats.compiled, m.stats.units), (1, 1));
    }

    #[test]
    fn build_script_artifact_prefers_the_build_script_unit() {
        let mut m = Model::new(None);
        let base = t0();
        m.on_stderr("   Compiling serde v1.0.0", ms(base, 0));
        m.on_stderr("   Compiling serde v1.0.0 (build script)", ms(base, 50));
        let c = m.on_json(&artifact("registry+x#serde@1.0.0", "build_script_build", "custom-build", false), ms(base, 400)).unwrap();
        assert!(c.build_script);
        assert_eq!(c.duration, Duration::from_millis(350));
        assert_eq!(m.active.len(), 1);
        assert!(!m.active[0].build_script);
    }

    #[test]
    fn path_dependency_matches_through_target_name() {
        let mut m = Model::new(None);
        let base = t0();
        m.on_stderr("   Compiling my-core v0.1.0 (/work/crates/core)", ms(base, 0));
        let c = m.on_json(&artifact("path+file:///work/crates/core#0.1.0", "my_core", "lib", false), ms(base, 2000)).unwrap();
        assert_eq!(c.name, "my-core");
        assert!(c.local);
        assert!(m.active.is_empty());
    }

    #[test]
    fn hit_before_artifact_is_consumed_at_completion() {
        let mut m = Model::new(None);
        let base = t0();
        m.on_stderr("   Compiling serde-json v1.0.0", ms(base, 0));
        m.on_cache(&hit("serde_json", 900));
        let c = m.on_json(&artifact("registry+x#serde-json@1.0.0", "serde_json", "lib", false), ms(base, 100)).unwrap();
        assert!(c.cached);
        assert_eq!(c.saved_ms, 900);
        assert_eq!((m.stats.cached, m.stats.compiled, m.stats.saved_ms), (1, 0, 900));
    }

    #[test]
    fn hit_after_artifact_moves_unit_from_compiled_to_cached() {
        let mut m = Model::new(None);
        let base = t0();
        m.on_stderr("   Compiling serde-json v1.0.0", ms(base, 0));
        let c = m.on_json(&artifact("registry+x#serde-json@1.0.0", "serde_json", "lib", false), ms(base, 100)).unwrap();
        assert!(!c.cached);
        assert_eq!((m.stats.cached, m.stats.compiled), (0, 1));
        m.on_cache(&hit("serde_json", 900));
        assert_eq!((m.stats.cached, m.stats.compiled, m.stats.units, m.stats.saved_ms), (1, 0, 1, 900));
        // A second hit for the same name has nothing left to convert and must not underflow.
        m.on_cache(&hit("serde_json", 100));
        assert_eq!((m.stats.cached, m.stats.compiled), (1, 0));
    }

    #[test]
    fn artifact_before_compiling_line_does_not_leave_a_stuck_unit() {
        let mut m = Model::new(None);
        let base = t0();
        let c = m.on_json(&artifact("registry+x#fast@1.0.0", "fast", "lib", false), ms(base, 10)).unwrap();
        assert_eq!(c.duration, Duration::ZERO);
        m.on_stderr("   Compiling fast v1.0.0", ms(base, 20));
        assert!(m.active.is_empty());
    }

    #[test]
    fn unknown_reasons_and_fields_are_ignored() {
        let mut m = Model::new(None);
        let now = t0();
        assert!(m.on_json(&json!({"reason": "build-script-executed", "package_id": "x"}), now).is_none());
        assert!(m.on_json(&json!({"reason": "something-new", "extra": [1, 2]}), now).is_none());
        assert!(m.on_json(&json!({"no_reason": true}), now).is_none());
        m.on_json(&json!({"reason": "build-finished", "success": false}), now);
        assert_eq!(m.build_finished, Some(false));
    }

    #[test]
    fn percent_and_total() {
        let mut m = Model::new(Some(4));
        assert_eq!((m.percent(), m.total()), (Some(0), Some(4)));
        m.stats.units = 1;
        assert_eq!(m.percent(), Some(25));
        m.stats.units = 9;
        assert_eq!((m.percent(), m.total()), (Some(99), Some(9)));
        assert_eq!(Model::new(None).percent(), None);
        assert_eq!(Model::new(Some(0)).total(), None);
    }
}
