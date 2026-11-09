//! The executor: a repeat build of an unchanged graph where only workspace sources changed runs the recorded rustc calls without cargo.

pub mod child;
pub mod journal;
pub mod plan;

use std::collections::{HashMap, HashSet};
use std::env;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use crate::noop::{Recorder, Stream};
use crate::paths::Dirs;
use crate::ui::term;
use plan::Plan;

pub const CHILD_ARG: &str = "--frust-exec-child";
const PLAN_SUFFIX: &str = ".plan";
const MAX_AGE: Duration = Duration::from_secs(30 * 86_400);
const TEMP_GRACE: Duration = Duration::from_secs(3_600);
const MAX_PLANS: usize = 256;
const AUDIT_LOG_CAP: u64 = 1024 * 1024;

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
}

pub fn enabled() -> bool {
    Mode::parse(env::var("FRUST_EXEC").ok().as_deref()) != Mode::Off
}

pub fn dir(dirs: &Dirs) -> PathBuf {
    dirs.root().join("exec")
}

/// The plan sits beside the no-op record of the same command and display.
fn plan_path(dirs: &Dirs, record: &Path) -> PathBuf {
    let name = record.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    dir(dirs).join(format!("{name}{PLAN_SUFFIX}"))
}

/// Units whose fingerprints a replay removed, with everything above them: cargo recompiles these through the cache on its next run.
fn owed_units(plan: &Plan) -> Vec<bool> {
    with_dependents(plan, plan.units.iter().map(|u| !Path::new(&u.fingerprint).exists()).collect())
}

/// `(unit key, package)` of each owed unit of the command recorded at `record`, for the no-op audit.
pub fn owed(dirs: &Dirs, record: &Path) -> Vec<(String, String)> {
    let Some(plan) = plan::load(&plan_path(dirs, record)) else { return Vec::new() };
    let owed = owed_units(&plan);
    plan.messages.iter()
        .filter_map(|(u, text)| {
            let u = (*u)?;
            let msg = serde_json::from_str::<serde_json::Value>(text).ok()?;
            owed[u].then(|| (unit_key(&msg), plan.units[u].package.clone()))
        })
        .collect()
}

pub struct Invocation<'a> {
    pub dirs: &'a Dirs,
    pub record: &'a Path,
    pub fingerprint: &'a str,
    pub cwd: &'a Path,
    pub root: Option<&'a Path>,
    pub target: &'a Path,
    /// The no-op gate would record this command, it is not auditing, and the build lock is held.
    pub eligible: bool,
    pub env_vars: &'a [(OsString, OsString)],
    pub configs: Vec<PathBuf>,
    pub args: &'a [OsString],
    pub debug: bool,
}

pub struct Gate {
    mode: Mode,
    eligible: bool,
    dirs: Dirs,
    plan_path: PathBuf,
    key: String,
    cwd: PathBuf,
    root: Option<PathBuf>,
    target: PathBuf,
    configs: Vec<PathBuf>,
    base: HashMap<String, String>,
    wrapper: Option<PathBuf>,
    jobs: usize,
    debug: bool,
    journal: Option<PathBuf>,
    request: Option<PathBuf>,
    served: bool,
    predicted: Option<(Plan, Vec<usize>)>,
    owed: HashSet<String>,
    loaded: Option<Plan>,
    launched_ns: i128,
    decision: String,
    planned: String,
}

/// What the child would see: this process's environment under frust's own settings.
fn base_env(env_vars: &[(OsString, OsString)]) -> HashMap<String, String> {
    let mut base: HashMap<String, String> = HashMap::new();
    for (k, v) in env::vars_os() {
        if let (Ok(k), Ok(v)) = (k.into_string(), v.into_string()) {
            base.insert(k, v);
        }
    }
    for (k, v) in env_vars {
        if let (Some(k), Some(v)) = (k.to_str(), v.to_str()) {
            base.insert(k.to_string(), v.to_string());
        }
    }
    base.remove(crate::wrapper::SLOT_SUPERVISOR_ENV);
    base.retain(|k, _| !journal::per_run(k));
    base
}

/// `-j`, then `CARGO_BUILD_JOBS`, then the available parallelism, as cargo resolves it.
fn jobs(args: &[OsString]) -> usize {
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
    let resolve = |text: &str| -> Option<usize> {
        let n: i64 = text.trim().parse().ok()?;
        Some(if n < 0 { (cpus as i64 + n).max(1) as usize } else { (n as usize).max(1) })
    };
    let mut i = 0;
    while i < args.len() {
        let text = args[i].to_string_lossy();
        if text == "--" {
            break;
        }
        let value = if text == "-j" || text == "--jobs" {
            i += 1;
            args.get(i).map(|v| v.to_string_lossy().into_owned())
        } else if let Some(rest) = text.strip_prefix("--jobs=") {
            Some(rest.to_string())
        } else {
            text.strip_prefix("-j").filter(|r| !r.is_empty()).map(str::to_string)
        };
        if let Some(n) = value.as_deref().and_then(resolve) {
            return n;
        }
        i += 1;
    }
    env::var("CARGO_BUILD_JOBS").ok().as_deref().and_then(resolve).unwrap_or(cpus)
}

impl Gate {
    pub fn new(inv: Invocation<'_>) -> Gate {
        let mode = Mode::parse(env::var("FRUST_EXEC").ok().as_deref());
        let wrapper = inv.env_vars.iter().rev().find(|(k, _)| k == "RUSTC_WRAPPER").map(|(_, v)| PathBuf::from(v))
            .or_else(|| env::var_os("RUSTC_WRAPPER").filter(|v| !v.is_empty()).map(PathBuf::from));
        Gate {
            mode,
            eligible: inv.eligible,
            dirs: inv.dirs.clone(),
            plan_path: plan_path(inv.dirs, inv.record),
            key: inv.fingerprint.to_string(),
            cwd: inv.cwd.to_path_buf(),
            root: inv.root.map(Path::to_path_buf),
            target: inv.target.to_path_buf(),
            configs: inv.configs,
            base: base_env(inv.env_vars),
            wrapper,
            jobs: jobs(inv.args),
            debug: inv.debug,
            journal: None,
            request: None,
            served: false,
            predicted: None,
            owed: HashSet::new(),
            loaded: None,
            launched_ns: 0,
            decision: String::new(),
            planned: String::new(),
        }
    }

    fn active(&self) -> Result<(), &'static str> {
        if self.mode == Mode::Off {
            return Err("off");
        }
        if !self.eligible || self.root.is_none() {
            return Err("ineligible");
        }
        if self.wrapper.is_none() {
            return Err("ineligible:wrapper");
        }
        Ok(())
    }

    /// Under the build lock: the executor's command when the recorded plan covers this build.
    pub fn check(&mut self) -> Option<Command> {
        if let Err(why) = self.active() {
            self.decision = why.to_string();
            return None;
        }
        let loaded = plan::load(&self.plan_path);
        let verdict = loaded.as_ref().ok_or("none").and_then(|plan| self.verify(plan));
        let (plan, dirty) = match (loaded, verdict) {
            (Some(plan), Ok(dirty)) => (plan, dirty),
            (plan, verdict) => {
                let why = verdict.err().unwrap_or("none");
                // Cargo runs under the same lock, so the plan read here is still the previous one when the new one is built.
                self.loaded = plan;
                self.decision = format!("decline:{why}");
                if self.debug {
                    eprintln!("frust: executor declined: {why}");
                }
                return None;
            }
        };
        if self.mode == Mode::Audit {
            self.decision = "audit:pending".into();
            // An earlier replay removed these fingerprints, so cargo rebuilds them and everything above them through the cache.
            let owed = owed_units(&plan);
            self.owed = plan.units.iter().zip(owed).filter(|(_, o)| *o).map(|(u, _)| u.id.clone()).collect();
            self.predicted = Some((plan, dirty));
            return None;
        }
        let total = plan.units.len();
        let cmd = self.child_command(&plan, dirty.clone(), false)?;
        self.decision = format!("replay:{}/{total}", dirty.len());
        self.served = true;
        Some(cmd)
    }

    fn child_command(&mut self, plan: &Plan, dirty: Vec<usize>, audit: bool) -> Option<Command> {
        let request = child::Request {
            plan: self.plan_path.clone(),
            dirty,
            jobs: self.jobs,
            colour: plan.finished.contains('\x1b'),
            wrapper: self.wrapper.clone()?,
            audit,
        };
        let path = crate::fsx::temp_sibling(&dir(&self.dirs).join("request.json"));
        let written = serde_json::to_vec(&request).ok().is_some_and(|bytes| crate::fsx::write_atomic(&path, &bytes).is_ok());
        if !written {
            self.decision = "decline:unwritable".into();
            return None;
        }
        let exe = env::current_exe().ok()?;
        let mut cmd = Command::new(exe);
        cmd.arg(CHILD_ARG).arg(&path);
        self.request = Some(path);
        Some(cmd)
    }

    /// The replay env of a unit: frust's environment with what cargo set or removed for that call.
    fn unit_env(&self, plan: &Plan, unit: &plan::Unit, name: &str) -> Option<String> {
        if let Some((_, v)) = unit.env_set.iter().chain(&plan.env_common).find(|(k, _)| k == name) {
            return Some(v.clone());
        }
        if unit.env_unset.iter().any(|k| k == name) {
            return None;
        }
        self.base.get(name).cloned()
    }

    /// The dirty units in plan order, or why the plan cannot answer this build.
    fn verify(&self, plan: &Plan) -> Result<Vec<usize>, &'static str> {
        if plan.key != self.key || Path::new(&plan.cwd) != self.cwd || Path::new(&plan.target) != self.target {
            return Err("key");
        }
        if plan.graph.iter().any(|e| !e.current()) {
            return Err("manifest or config changed");
        }
        if plan.layout.iter().any(|(dir, package, facts)| plan::layout_facts(Path::new(dir), *package) != *facts) {
            return Err("targets changed");
        }
        for script in &plan.scripts {
            if script.triggers.iter().any(|e| !e.current()) || script.env.iter().any(|(k, v)| self.base.get(k) != v.as_ref()) {
                return Err("build script would rerun");
            }
        }
        let total = plan.units.len();
        let dirty = with_dependents(plan, plan.units.iter().map(|u| {
            u.inputs.iter().any(|e| !e.current()) || u.env_deps.iter().any(|(k, v)| self.unit_env(plan, u, k) != *v)
        }).collect());
        for (u, unit) in plan.units.iter().enumerate() {
            if !dirty[u] {
                if unit.stamps.iter().any(|e| !e.current_output()) {
                    return Err("outputs changed");
                }
                continue;
            }
            if unit.script {
                return Err("build script changed");
            }
            if !unit.local {
                return Err("dependency source changed");
            }
            if !unit.depfiles_ok {
                return Err("unreproducible dep-info file");
            }
            if cfg!(windows) && unit.uplifts.iter().any(|(_, d)| d.ends_with(".exe") && Path::new(d).exists() && OpenOptions::new().append(true).open(d).is_err()) {
                return Err("executable in use");
            }
        }
        Ok((0..total).filter(|u| dirty[*u]).collect())
    }

    /// Asks the wrapper to journal this cargo run's rustc calls, so a later build may replay them.
    pub fn journal(&mut self, cmd: &mut Command) {
        cmd.env_remove(journal::ENV);
        if self.active().is_err() || self.served {
            return;
        }
        let _ = fs::create_dir_all(dir(&self.dirs));
        let path = crate::fsx::temp_sibling(&dir(&self.dirs).join("journal.jsonl"));
        if OpenOptions::new().write(true).create_new(true).open(&path).is_ok() {
            cmd.env(journal::ENV, &path);
            self.journal = Some(path);
            self.launched_ns = crate::cache::hashing::now_ns();
        }
    }

    /// Records the plan after a successful cargo run and settles an audit; returns an audit failure.
    pub fn finish(&mut self, code: i32, interrupted: bool, recorder: Option<&Recorder>) -> Option<String> {
        if let Some(request) = self.request.take() {
            let _ = fs::remove_file(request);
        }
        if self.served {
            self.planned = if code == 0 { "updated".into() } else { "kept".into() };
        }
        let journal_path = self.journal.take()?;
        let records = journal::read(&journal_path).unwrap_or_default();
        let _ = fs::remove_file(&journal_path);
        let lines = recorder.and_then(Recorder::lines);
        if code != 0 || interrupted || lines.is_none() {
            self.planned = "skip:failed".into();
            if self.predicted.take().is_some() {
                self.decision = "audit:unsettled".into();
            }
            let _ = fs::remove_file(&self.plan_path);
            return None;
        }
        let lines = lines.unwrap_or_default();
        let previous = self.loaded.take().or_else(|| plan::load(&self.plan_path));
        let messages: Vec<serde_json::Value> = lines.iter()
            .filter(|(s, _)| *s == Stream::Out)
            .filter_map(|(_, l)| serde_json::from_str(l).ok())
            .collect();
        let finished = lines.iter().rev().find(|(s, l)| *s == Stream::Err && term::strip_ansi(l).trim_start().starts_with("Finished ")).map(|(_, l)| l.clone());
        let root = self.root.clone().unwrap_or_default();
        let built = finished.ok_or(plan::Refused::from("no Finished line")).and_then(|finished| {
            plan::build(plan::Recorded {
                key: &self.key,
                cwd: &self.cwd,
                target: &self.target,
                root: &root,
                messages,
                finished,
                journal: records.clone(),
                previous,
                base_env: &self.base,
                configs: &self.configs,
                launched_ns: self.launched_ns,
            })
        });
        let failure = match built {
            Ok(new_plan) => {
                let failure = self.predicted.take().and_then(|(old, dirty)| self.audit(&old, &dirty, &new_plan, &records, lines));
                match plan::save(&self.plan_path, &new_plan) {
                    Ok(()) => self.planned = "written".into(),
                    Err(_) => self.planned = "skip:unwritable".into(),
                }
                prune(&dir(&self.dirs), &self.plan_path);
                failure.or_else(|| self.replay_audit(&new_plan, lines))
            }
            Err(plan::Refused { why, adopt }) => {
                self.planned = format!("skip:{why}");
                let _ = fs::remove_file(&self.plan_path);
                // Units built before frust journaled them: cargo recompiles them through the cache once, under a journal.
                let adopted = adopt.iter().filter(|f| fs::remove_file(f).is_ok()).count();
                if adopted > 0 {
                    self.planned = format!("skip:{why}, adopting {adopted}");
                }
                self.predicted.take().map(|_| format!("exec audit failed: the plan could not be rebuilt ({why})"))
            }
        };
        if let Some(text) = &failure {
            self.decision = "audit:failed".into();
            self.log_audit(text);
        } else if self.decision == "audit:pending" {
            self.decision = "audit:ok".into();
        }
        failure
    }

    /// Cargo's rebuilt units must be a subset of the predicted dirty set, compiled with the recorded arguments and environment.
    fn audit(&mut self, old: &Plan, dirty: &[usize], new_plan: &Plan, records: &[journal::Record], lines: &[(Stream, String)]) -> Option<String> {
        let dirty_ids: HashSet<&str> = dirty.iter().map(|&u| old.units[u].id.as_str()).collect();
        let mut problems = Vec::new();
        for (stream, line) in lines {
            if *stream != Stream::Out {
                continue;
            }
            let Ok(msg) = serde_json::from_str::<serde_json::Value>(line) else { continue };
            if msg.get("reason").and_then(|r| r.as_str()) != Some("compiler-artifact") || msg.get("fresh").and_then(|f| f.as_bool()) == Some(true) {
                continue;
            }
            let key = unit_key(&msg);
            let Some(u) = old.messages.iter().find_map(|(u, t)| u.filter(|_| serde_json::from_str::<serde_json::Value>(t).is_ok_and(|m| unit_key(&m) == key))) else {
                problems.push(format!("cargo rebuilt {key}, which the plan does not know"));
                continue;
            };
            let unit = &old.units[u];
            if !dirty_ids.contains(unit.id.as_str()) && !self.owed.contains(&unit.id) {
                problems.push(format!("cargo rebuilt {} which the executor would have skipped", unit.id));
            }
            match records.iter().rev().find(|r| plan::parse_argv(&r.argv, Path::new(&r.cwd)).is_some_and(|a| plan::unit_id(&a) == unit.id)) {
                Some(record) if record.argv != unit.argv => problems.push(format!("{}: cargo passed different arguments", unit.id)),
                Some(_) => {
                    if let Some(now) = new_plan.units.iter().find(|n| n.id == unit.id)
                        && (new_plan.env_of(now) != old.env_of(unit) || now.env_unset != unit.env_unset)
                    {
                        problems.push(format!("{}: cargo set a different environment", unit.id));
                    }
                }
                None => problems.push(format!("{}: rebuilt without a journal record", unit.id)),
            }
        }
        let predicted: Vec<usize> = dirty.iter().filter_map(|&u| new_plan.units.iter().position(|n| n.id == old.units[u].id)).collect();
        self.predicted = Some((new_plan.clone(), predicted));
        (!problems.is_empty()).then(|| format!("exec audit failed: {}", problems.join("; ")))
    }

    /// Recompiles the predicted units without the cache and requires the same bytes and diagnostics as cargo's run.
    fn replay_audit(&mut self, new_plan: &Plan, lines: &[(Stream, String)]) -> Option<String> {
        let (_, dirty) = self.predicted.take()?;
        let mut before: Vec<(String, Vec<u8>)> = Vec::new();
        for &u in &dirty {
            let unit = &new_plan.units[u];
            for out in &unit.outputs {
                let rmeta = out.ends_with(".rmeta");
                if (rmeta || !unit.incremental) && !out.ends_with(".d") {
                    before.push((out.clone(), fs::read(out).unwrap_or_default()));
                }
            }
        }
        let mut cmd = self.child_command(new_plan, dirty, true)?;
        let output = cmd.stdin(Stdio::null()).output();
        if let Some(request) = self.request.take() {
            let _ = fs::remove_file(request);
        }
        let output = match output {
            Ok(output) => output,
            Err(err) => return Some(format!("exec audit failed: the executor did not run: {err}")),
        };
        if !output.status.success() {
            return Some(format!("exec audit failed: the executor failed where cargo succeeded: {}", String::from_utf8_lossy(&output.stderr)));
        }
        let mut problems = Vec::new();
        for (path, bytes) in &before {
            if fs::read(path).ok().as_deref() != Some(bytes.as_slice()) {
                problems.push(format!("{path} differs from cargo's"));
            }
        }
        let diagnostics = |text: &mut dyn Iterator<Item = String>| {
            let mut list: Vec<String> = text.map(|l| term::strip_ansi(&l)).filter(|l| !is_status(l)).collect();
            list.sort();
            list
        };
        let cargo = diagnostics(&mut lines.iter().filter(|(s, _)| *s == Stream::Err).map(|(_, l)| l.clone()));
        let mine = diagnostics(&mut String::from_utf8_lossy(&output.stderr).lines().map(str::to_string));
        if cargo != mine {
            problems.push(format!("diagnostics differ: cargo {cargo:?}, executor {mine:?}"));
        }
        (!problems.is_empty()).then(|| format!("exec audit failed: {}", problems.join("; ")))
    }

    fn log_audit(&self, text: &str) {
        let log = dir(&self.dirs).join("audit.jsonl");
        if fs::metadata(&log).is_ok_and(|m| m.len() > AUDIT_LOG_CAP) {
            return;
        }
        let _ = fs::create_dir_all(dir(&self.dirs));
        let line = serde_json::json!({ "t": crate::fsx::unix_now(), "key": self.key, "cwd": self.cwd.to_string_lossy(), "failure": text });
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&log) {
            let _ = writeln!(file, "{line}");
        }
    }

    /// `(decision, plan)` for the build trace.
    pub fn trace(&self) -> (String, String) {
        let decision = if self.decision.is_empty() { "unchecked".to_string() } else { self.decision.clone() };
        let planned = if self.planned.is_empty() { "none".to_string() } else { self.planned.clone() };
        (decision, planned)
    }
}

/// The plans' counterpart of `noop::restamp`: a plan is rewritten only when every stamp that moved is a listed
/// identical-bytes rewrite from its recorded stamp to its current one; any other difference leaves it alone.
pub fn restamp(dirs: &Dirs, rewrites: &[crate::noop::Rewrite], gated: bool) -> usize {
    let pack = |s: &crate::cache::hashing::Stamp| -> plan::Packed { [s.dev as i64, s.ino as i64, s.size as i64, s.mtime_ns as i64, s.ctime_ns as i64] };
    let by_path: HashMap<&Path, (plan::Packed, plan::Packed)> = rewrites.iter().map(|r| (r.path.as_path(), (pack(&r.before), pack(&r.after)))).collect();
    if by_path.is_empty() {
        return 0;
    }
    let Ok(entries) = fs::read_dir(dir(dirs)) else { return 0 };
    let mut updated = 0;
    for file in entries.flatten() {
        let path = file.path();
        if !file.file_name().to_string_lossy().ends_with(PLAN_SUFFIX) {
            continue;
        }
        let Ok(bytes) = fs::read(&path) else { continue };
        let Some(mut plan) = plan::load(&path) else { continue };
        let target = PathBuf::from(&plan.target);
        if !by_path.keys().any(|p| p.starts_with(&target)) {
            continue;
        }
        let (mut touched, mut valid) = (false, true);
        let entries = plan.units.iter_mut().flat_map(|u| u.stamps.iter_mut().map(|e| (e, true)).chain(u.inputs.iter_mut().map(|e| (e, false))));
        for (entry, output) in entries {
            if if output { entry.current_output() } else { entry.current() } {
                continue;
            }
            let now = plan::stamp(Path::new(&entry.path));
            match (by_path.get(Path::new(&entry.path)), entry.stamp, now) {
                (Some((before, after)), Some(old), Some(now)) if old == *before && now == *after => {
                    entry.stamp = Some(now);
                    touched = true;
                }
                (Some((before, after)), Some(old), Some(now)) if output && old[..4] == before[..4] && now[..4] == after[..4] => {
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
        // Without the gate a build that just released cargo's locks could be writing this plan.
        let _gate = if gated {
            None
        } else {
            match crate::lock::target_activity(dirs, &target) {
                Ok(None) => None,
                Ok(Some(_)) => match crate::lock::try_target_maintenance(dirs, &target) {
                    Ok(Some(guard)) => Some(guard),
                    _ => continue,
                },
                Err(_) => continue,
            }
        };
        if fs::read(&path).ok().as_deref() != Some(bytes.as_slice()) {
            continue;
        }
        if plan::save(&path, &plan).is_ok() {
            updated += 1;
        }
    }
    updated
}

/// Cargo's rule: a unit rebuilds when any dependency did.
fn with_dependents(plan: &Plan, mut marked: Vec<bool>) -> Vec<bool> {
    let mut dependents = vec![Vec::new(); plan.units.len()];
    for (u, unit) in plan.units.iter().enumerate() {
        for (dep, _) in &unit.deps {
            dependents[*dep].push(u);
        }
    }
    let mut stack: Vec<usize> = (0..marked.len()).filter(|u| marked[*u]).collect();
    while let Some(u) = stack.pop() {
        for &d in &dependents[u] {
            if !marked[d] {
                marked[d] = true;
                stack.push(d);
            }
        }
    }
    marked
}

pub(crate) fn unit_key(msg: &serde_json::Value) -> String {
    let field = |name: &str| msg.get(name).map(|v| v.to_string()).unwrap_or_default();
    format!("{}|{}|{}|{}", field("package_id"), field("target"), field("profile"), field("features"))
}

fn is_status(line: &str) -> bool {
    let body = line.trim_start();
    body.is_empty() || (body.len() != line.len() && body.split(' ').next().is_some_and(|w| w.chars().next().is_some_and(|c| c.is_ascii_uppercase())))
}

/// Bounds the plan directory by age and count and drops stale temporary files.
fn prune(dir: &Path, keep: &Path) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    let now = SystemTime::now();
    let age = |m: SystemTime| now.duration_since(m).unwrap_or_default();
    let mut plans = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else { continue };
        if name.contains(crate::fsx::TEMP_MARKER) {
            if age(modified) > TEMP_GRACE {
                let _ = fs::remove_file(&path);
            }
        } else if name.ends_with(PLAN_SUFFIX) && path != keep {
            if age(modified) > MAX_AGE {
                let _ = fs::remove_file(&path);
            } else {
                plans.push((modified, path));
            }
        }
    }
    if plans.len() >= MAX_PLANS {
        plans.sort();
        for (_, path) in plans.drain(..plans.len() + 1 - MAX_PLANS) {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jobs_follow_cargo_precedence() {
        let os = |list: &[&str]| list.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(jobs(&os(&["build", "-j", "3"])), 3);
        assert_eq!(jobs(&os(&["build", "-j5"])), 5);
        assert_eq!(jobs(&os(&["build", "--jobs=2"])), 2);
        let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
        assert_eq!(jobs(&os(&["build", "-j", "-1"])), (cpus as i64 - 1).max(1) as usize);
        assert_eq!(Mode::parse(Some("off")), Mode::Off);
        assert_eq!(Mode::parse(Some("audit")), Mode::Audit);
        assert_eq!(Mode::parse(None), Mode::On);
    }

    #[test]
    fn plans_follow_only_listed_identical_rewrites() {
        use crate::cache::hashing::Stamp;
        let root = crate::fsx::temp_sibling(&env::temp_dir().join("frust-exec-restamp"));
        let dirs = Dirs::at(&root);
        let target = root.join("target");
        fs::create_dir_all(&target).unwrap();
        let (out, other) = (target.join("libk.rlib"), target.join("libo.rlib"));
        fs::write(&out, b"same bytes").unwrap();
        fs::write(&other, b"other").unwrap();
        let unit = plan::Unit {
            id: "k".into(), package_id: "path+file:///w#k@0.1.0".into(), package: "k v0.1.0 (/w)".into(), pkg_name: "k".into(),
            desc: "lib".into(), fix: None, local: true, script: false, argv: vec![], cwd: "/w".into(), env_set: vec![], env_unset: vec![],
            outputs: vec![], inputs: vec![], env_deps: vec![], stamps: vec![plan::Entry::now(&out), plan::Entry::now(&other)], deps: vec![],
            uplifts: vec![], depfiles: vec![], depfiles_ok: true, fingerprint: String::new(), diagnostics: vec![], run: None,
            script_unit: None, incremental: false, ms: 0,
        };
        let recorded = Plan {
            format: plan::FORMAT, key: "k".into(), cwd: "/w".into(), target: target.to_string_lossy().into_owned(), finished: String::new(),
            graph: vec![], layout: vec![], scripts: vec![], units: vec![unit], messages: vec![], env_common: vec![],
        };
        let path = dir(&dirs).join(format!("p{PLAN_SUFFIX}"));
        plan::save(&path, &recorded).unwrap();
        let before = Stamp::read(&out).unwrap();
        let replacement = target.join("tmp");
        fs::write(&replacement, b"same bytes").unwrap();
        crate::sys::rename(&replacement, &out).unwrap();
        let after = Stamp::read(&out).unwrap();
        let rewrite = |before: Stamp| crate::noop::Rewrite { path: out.clone(), before, after };
        assert_eq!(restamp(&dirs, &[rewrite(Stamp { ino: before.ino + 1, ..before })], true), 0, "a stale before-stamp is not covered");
        assert!(plan::load(&path).unwrap().units[0].stamps.iter().any(|e| !e.current_output()));
        assert_eq!(restamp(&dirs, &[rewrite(before)], true), 1);
        assert!(plan::load(&path).unwrap().units[0].stamps.iter().all(plan::Entry::current_output), "the rewritten output verifies again");
        fs::write(&other, b"changed").unwrap();
        assert_eq!(restamp(&dirs, &[rewrite(after)], true), 0, "an unlisted change is never covered");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn status_lines_are_not_diagnostics() {
        assert!(is_status("   Compiling app v0.1.0 (/w)"));
        assert!(is_status("    Finished `dev` profile in 0.1s"));
        assert!(!is_status("warning: unused variable"));
        assert!(!is_status(" --> src/lib.rs:1:2"));
    }
}
