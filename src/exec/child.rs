//! The executor process: replays dirty units through the wrapper and prints what cargo would.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::env;
use std::ffi::OsString;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::plan::{self, Plan};
use crate::sys::{self, signal};
use crate::ui::term;

const TICK: Duration = Duration::from_millis(50);

static SIGNALLED: AtomicI32 = AtomicI32::new(0);

extern "C" fn note(sig: signal::Signal) {
    SIGNALLED.store(sig, Ordering::Relaxed);
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub plan: PathBuf,
    pub dirty: Vec<usize>,
    pub jobs: usize,
    pub colour: bool,
    pub wrapper: PathBuf,
    /// Compile without the cache, for the audit's byte comparison.
    pub audit: bool,
}

/// Cargo's per-unit diagnostic bookkeeping.
#[derive(Debug, Clone, Copy)]
struct Counts {
    warnings_seen: usize,
    errors_seen: usize,
    duplicates: usize,
    fixable: Option<usize>,
}

impl Default for Counts {
    fn default() -> Counts {
        Counts { warnings_seen: 0, errors_seen: 0, duplicates: 0, fixable: Some(0) }
    }
}

enum Event {
    Err(usize, String),
    Out(Vec<u8>),
    Eof(usize),
    Token(io::Result<jobserver::Acquired>),
}

struct Running {
    child: Child,
    spawned_ns: i128,
    started: Instant,
    counts: Counts,
    diagnostics: Vec<String>,
    eof: bool,
}

struct Printer {
    colour: bool,
    seen: HashSet<u64>,
}

impl Printer {
    fn err(&self, text: &str) {
        let mut stderr = io::stderr().lock();
        let _ = stderr.write_all(text.as_bytes());
        if !text.ends_with('\n') {
            let _ = stderr.write_all(b"\n");
        }
        let _ = stderr.flush();
    }

    fn out(&self, text: &str) {
        let mut stdout = io::stdout().lock();
        let _ = writeln!(stdout, "{text}");
        let _ = stdout.flush();
    }

    fn status(&self, word: &str, rest: &str) {
        if self.colour {
            self.err(&format!("\x1b[1m\x1b[92m{word:>12}\x1b[0m {rest}"));
        } else {
            self.err(&format!("{word:>12} {rest}"));
        }
    }

    fn warn(&self, text: &str) {
        if self.colour {
            self.err(&format!("\x1b[1m\x1b[33mwarning\x1b[0m: {text}"));
        } else {
            self.err(&format!("warning: {text}"));
        }
    }

    fn error(&self, text: &str) {
        if self.colour {
            self.err(&format!("\x1b[1m\x1b[91merror\x1b[0m: {text}"));
        } else {
            self.err(&format!("error: {text}"));
        }
    }

    /// One line of rustc's stderr as cargo handles it; returns the emitted kind of an artifact notification.
    fn line(&mut self, line: &str, counts: &mut Counts, keep: Option<&mut Vec<String>>) -> Option<String> {
        if !line.starts_with('{') {
            self.err(line);
            return None;
        }
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(line) else {
            self.err(line);
            return None;
        };
        if let Some(artifact) = msg.get("artifact").and_then(|a| a.as_str()) {
            let emit = msg.get("emit").and_then(|e| e.as_str()).map(str::to_string);
            return emit.or_else(|| artifact.ends_with(".rmeta").then(|| "metadata".to_string()));
        }
        let (Some(rendered), Some(message), Some(level)) = (
            msg.get("rendered").and_then(|r| r.as_str()),
            msg.get("message").and_then(|m| m.as_str()),
            msg.get("level").and_then(|l| l.as_str()),
        ) else {
            return None;
        };
        if let Some(keep) = keep {
            keep.push(line.to_string());
        }
        if message.starts_with("aborting due to") || message.ends_with("warning emitted") || message.ends_with("warnings emitted") {
            return None;
        }
        let rendered = if self.colour { rendered.to_string() } else { term::strip_ansi(rendered) };
        let fixable = msg.get("children").and_then(|c| c.as_array()).into_iter().flatten().any(|child| {
            child.get("spans").and_then(|s| s.as_array()).into_iter().flatten()
                .any(|span| span.get("suggestion_applicability").and_then(|a| a.as_str()) == Some("MachineApplicable"))
        });
        match level {
            "warning" => counts.warnings_seen += 1,
            "error" => counts.errors_seen += 1,
            _ => {}
        }
        let mut hasher = DefaultHasher::new();
        rendered.hash(&mut hasher);
        let emitted = self.seen.insert(hasher.finish());
        if emitted {
            let mut stderr = io::stderr().lock();
            let _ = stderr.write_all(rendered.as_bytes());
            let _ = stderr.flush();
        }
        if level == "warning" {
            if !emitted {
                counts.duplicates += 1;
            } else if fixable && let Some(n) = counts.fixable.as_mut() {
                *n += 1;
            }
        } else if level == "error" {
            counts.fixable = None;
        }
        None
    }

    fn summary(&self, unit: &plan::Unit, counts: &Counts) {
        if counts.warnings_seen == 0 {
            return;
        }
        let total = counts.warnings_seen;
        let mut text = format!("`{}` ({}) generated {total} warning{}", unit.pkg_name, unit.desc, if total == 1 { "" } else { "s" });
        match counts.duplicates {
            0 => {}
            1 => text.push_str(" (1 duplicate)"),
            n => text.push_str(&format!(" ({n} duplicates)")),
        }
        if let (Some(args), Some(n)) = (&unit.fix, counts.fixable)
            && n > 0
        {
            text.push_str(&format!(" (run `cargo fix {args} -p {}` to apply {n} suggestion{})", unit.pkg_name, if n == 1 { "" } else { "s" }));
        }
        self.warn(&text);
    }

    fn failure(&self, unit: &plan::Unit, counts: &Counts, status: &str) {
        let warnings = match counts.warnings_seen {
            0 => String::new(),
            1 => "; 1 warning emitted".to_string(),
            n => format!("; {n} warnings emitted"),
        };
        let errors = match counts.errors_seen {
            0 => String::new(),
            1 => " due to 1 previous error".to_string(),
            n => format!(" due to {n} previous errors"),
        };
        let mut text = format!("could not compile `{}` ({}){errors}{warnings}", unit.pkg_name, unit.desc);
        if counts.errors_seen == 0 {
            text.push_str(&format!("\n\nCaused by:\n  process didn't exit successfully: `{}` ({status})", unit.argv.join(" ")));
        }
        self.error(&text);
    }
}

pub fn main(args: &[OsString]) -> i32 {
    let started = Instant::now();
    let Some(request) = args.get(1).and_then(|p| fs::read(p).ok()).and_then(|b| serde_json::from_slice::<Request>(&b).ok()) else {
        eprintln!("frust: unreadable executor request");
        return 101;
    };
    let Some(plan) = plan::load(&request.plan) else {
        eprintln!("frust: unreadable executor plan");
        return 101;
    };
    for sig in [signal::SIGINT, signal::SIGTERM, signal::SIGHUP, signal::SIGQUIT] {
        let _ = signal::install(sig, note);
    }
    let mut executor = Executor::new(plan, &request);
    let code = executor.run(&request);
    if code == 0 {
        if !request.dirty.is_empty()
            && let Err(err) = plan::save(&request.plan, &executor.plan)
        {
            eprintln!("frust: cannot save the executor plan: {err}");
        }
        executor.printer.err(&crate::noop::retime(&executor.plan.finished, started.elapsed()));
    }
    let success = code == 0;
    executor.printer.out(&serde_json::json!({ "reason": "build-finished", "success": success }).to_string());
    code
}

struct Executor {
    plan: Plan,
    printer: Printer,
    dirty: Vec<bool>,
}

impl Executor {
    fn new(plan: Plan, request: &Request) -> Executor {
        let mut dirty = vec![false; plan.units.len()];
        for &u in &request.dirty {
            if u < dirty.len() {
                dirty[u] = true;
            }
        }
        Executor { plan, printer: Printer { colour: request.colour, seen: HashSet::new() }, dirty }
    }

    /// Fresh units first, as cargo finishes them before any compile; their cached warnings replay with them.
    fn replay_fresh(&mut self) {
        let messages = self.plan.messages.clone();
        for (unit, text) in &messages {
            let Some(u) = *unit else {
                self.printer.out(text);
                continue;
            };
            if self.dirty[u] {
                continue;
            }
            let mut counts = Counts::default();
            for line in self.plan.units[u].diagnostics.clone() {
                self.printer.line(&line, &mut counts, None);
            }
            self.printer.out(&with_fresh(text, true));
            self.printer.summary(&self.plan.units[u], &counts);
        }
    }

    fn run(&mut self, request: &Request) -> i32 {
        self.replay_fresh();
        let total = self.plan.units.len();
        let mut dependents = vec![Vec::new(); total];
        for (u, unit) in self.plan.units.iter().enumerate() {
            for (dep, _) in &unit.deps {
                dependents[*dep].push(u);
            }
        }
        let weight: Vec<usize> = (0..total).map(|u| reach(&dependents, u)).collect();
        let jobs = request.jobs.max(1);
        let client = match jobserver::Client::new(jobs.saturating_sub(1).max(1)) {
            Ok(client) => client,
            Err(err) => {
                self.printer.error(&format!("cannot create a jobserver: {err}"));
                return 101;
            }
        };
        let (tx, rx) = mpsc::channel::<Event>();
        let token_tx = tx.clone();
        let helper = client.clone().into_helper_thread(move |token| {
            let _ = token_tx.send(Event::Token(token));
        });
        let Ok(helper) = helper else {
            self.printer.error("cannot start the jobserver helper");
            return 101;
        };
        let mut tokens: Vec<jobserver::Acquired> = Vec::new();
        let mut requested = 0usize;
        let mut started_units = vec![false; total];
        let mut done = vec![false; total];
        let mut rmeta = vec![false; total];
        let mut running: HashMap<usize, Running> = HashMap::new();
        let mut announced: HashSet<String> = HashSet::new();
        let mut failed = false;
        let mut waiting_noted = false;
        loop {
            let sig = SIGNALLED.load(Ordering::Relaxed);
            if sig != 0 {
                for run in running.values_mut() {
                    let _ = sys::cancel_owned_process_group(run.child.id());
                }
                for (_, mut run) in running.drain() {
                    let _ = run.child.wait();
                }
                return 128 + sig;
            }
            let mut ready: Vec<usize> = Vec::new();
            if !failed {
                for (u, unit) in self.plan.units.iter().enumerate() {
                    if !self.dirty[u] || started_units[u] {
                        continue;
                    }
                    let satisfied = unit.deps.iter().all(|&(d, meta_only)| !self.dirty[d] || done[d] || (meta_only && rmeta[d]));
                    if satisfied {
                        ready.push(u);
                    }
                }
                ready.sort_by(|a, b| weight[*b].cmp(&weight[*a]).then(a.cmp(b)));
            }
            let wanted = (running.len() + ready.len()).min(jobs).saturating_sub(1);
            while tokens.len() + requested < wanted {
                helper.request_token();
                requested += 1;
            }
            let mut blocked = false;
            for u in ready {
                if running.len() >= jobs || (!running.is_empty() && tokens.len() < running.len()) {
                    blocked = true;
                    break;
                }
                started_units[u] = true;
                let unit = &self.plan.units[u];
                if announced.insert(unit.package_id.clone()) {
                    self.printer.status(if unit.checks() { "Checking" } else { "Compiling" }, &unit.package);
                }
                match spawn(unit, &self.plan.env_common, request, &client, u, &tx) {
                    Ok(run) => {
                        running.insert(u, run);
                    }
                    Err(err) => {
                        self.printer.error(&format!("could not compile `{}` ({}): cannot run {}: {err}", unit.pkg_name, unit.desc, request.wrapper.display()));
                        failed = true;
                        break;
                    }
                }
            }
            if running.is_empty() {
                break;
            }
            // Tokens nothing can use go back at once, so rustc's own threads may take them.
            let keep = running.len() - 1 + usize::from(blocked && running.len() < jobs);
            tokens.truncate(keep);
            let event = match rx.recv_timeout(TICK) {
                Ok(event) => event,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            match event {
                Event::Token(Ok(token)) => {
                    requested = requested.saturating_sub(1);
                    tokens.push(token);
                }
                Event::Token(Err(_)) => requested = requested.saturating_sub(1),
                Event::Out(line) => {
                    let mut stdout = io::stdout().lock();
                    let _ = stdout.write_all(&line);
                    let _ = stdout.write_all(b"\n");
                }
                Event::Err(u, line) => {
                    if let Some(run) = running.get_mut(&u) {
                        let emitted = self.printer.line(&line, &mut run.counts, Some(&mut run.diagnostics));
                        if emitted.as_deref() == Some("metadata") {
                            rmeta[u] = true;
                        }
                    }
                }
                Event::Eof(u) => {
                    if let Some(run) = running.get_mut(&u) {
                        run.eof = true;
                    }
                }
            }
            let finished: Vec<usize> = running.iter().filter(|(_, r)| r.eof).map(|(u, _)| *u).collect();
            for u in finished {
                let Some(mut run) = running.remove(&u) else { continue };
                let status = run.child.wait();
                if tokens.len() > running.len().saturating_sub(1) {
                    tokens.pop();
                }
                match status {
                    Ok(status) if status.success() => {
                        if let Err(why) = self.complete(u, &run) {
                            self.printer.error(&format!("could not compile `{}` ({}): {why}", self.plan.units[u].pkg_name, self.plan.units[u].desc));
                            failed = true;
                            continue;
                        }
                        done[u] = true;
                        rmeta[u] = true;
                    }
                    other => {
                        self.printer.failure(&self.plan.units[u], &run.counts, &describe(&other));
                        failed = true;
                    }
                }
                if failed && !running.is_empty() && !waiting_noted {
                    waiting_noted = true;
                    self.printer.warn("build failed, waiting for other jobs to finish...");
                }
            }
        }
        drop(helper);
        if failed || (0..total).any(|u| self.dirty[u] && !done[u]) { 101 } else { 0 }
    }

    /// Uplifts, refreshes dep-info files and records the unit's new inputs, then reports it as cargo does.
    fn complete(&mut self, u: usize, run: &Running) -> Result<(), String> {
        let argv = self.plan.units[u].argv().ok_or("unreplayable arguments")?;
        let cwd = PathBuf::from(&self.plan.units[u].cwd);
        let (inputs, env_deps) = plan::read_inputs(&argv, &cwd, Path::new(&self.plan.target), run.spawned_ns).map_err(str::to_string)?;
        settle_fingerprint(&self.plan.units[u], &inputs, &env_deps, &run.diagnostics, run.spawned_ns).map_err(|e| format!("cannot settle cargo's fingerprint: {e}"))?;
        {
            let unit = &mut self.plan.units[u];
            unit.inputs = inputs;
            unit.env_deps = env_deps;
            unit.diagnostics = run.diagnostics.clone();
            unit.ms = run.started.elapsed().as_millis() as u64;
        }
        for (src, dst) in self.plan.units[u].uplifts.clone() {
            uplift(Path::new(&src), Path::new(&dst)).map_err(|e| format!("cannot uplift {dst}: {e}"))?;
        }
        for (dest, file) in self.plan.units[u].depfiles.clone() {
            let text = plan::depfile_text(&self.plan, u, &dest);
            if fs::read_to_string(&file).ok().as_deref() != Some(text.as_str()) {
                crate::fsx::write_atomic(Path::new(&file), text.as_bytes()).map_err(|e| format!("cannot write {file}: {e}"))?;
            }
        }
        plan::restamp(&mut self.plan.units[u]);
        let message = self.plan.messages.iter().find(|(m, _)| *m == Some(u)).map(|(_, t)| t.clone());
        if let Some(text) = message {
            self.printer.out(&with_fresh(&text, false));
        }
        self.printer.summary(&self.plan.units[u], &run.counts);
        Ok(())
    }
}

fn with_fresh(text: &str, fresh: bool) -> String {
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(mut msg) => {
            msg["fresh"] = serde_json::Value::Bool(fresh);
            msg.to_string()
        }
        Err(_) => text.to_string(),
    }
}

fn reach(dependents: &[Vec<usize>], from: usize) -> usize {
    let mut seen = HashSet::new();
    let mut stack = vec![from];
    while let Some(u) = stack.pop() {
        for &d in &dependents[u] {
            if seen.insert(d) {
                stack.push(d);
            }
        }
    }
    seen.len()
}

fn describe(status: &io::Result<ExitStatus>) -> String {
    match status {
        Ok(status) => match status.code() {
            Some(code) => format!("exit status: {code}"),
            None => "terminated by a signal".to_string(),
        },
        Err(err) => err.to_string(),
    }
}

fn spawn(unit: &plan::Unit, common: &[(String, String)], request: &Request, client: &jobserver::Client, u: usize, tx: &mpsc::Sender<Event>) -> io::Result<Running> {
    let mut cmd = Command::new(&request.wrapper);
    cmd.args(&unit.argv).current_dir(&unit.cwd);
    for name in &unit.env_unset {
        cmd.env_remove(name);
    }
    for (name, value) in common.iter().chain(&unit.env_set) {
        cmd.env(name, value);
    }
    for (name, _) in env::vars_os() {
        if name.to_str().is_some_and(|n| super::journal::per_run(n) && !n.starts_with("CARGO_TERM_") && !n.starts_with("FRUST_INTERNAL_") && n != "FRUST_EVENTS" && n != crate::store_broker::CAPABILITY_ENV) {
            cmd.env_remove(&name);
        }
    }
    if request.audit {
        cmd.env("FRUST_CACHE", "off");
    }
    client.configure(&mut cmd);
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    sys::own_process_group(&mut cmd);
    let spawned_ns = crate::cache::hashing::now_ns();
    let mut child = cmd.spawn()?;
    let stderr = child.stderr.take().ok_or_else(|| io::Error::other("no stderr"))?;
    let stdout = child.stdout.take().ok_or_else(|| io::Error::other("no stdout"))?;
    let err_tx = tx.clone();
    thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut buf = Vec::new();
        while reader.read_until(b'\n', &mut buf).is_ok_and(|n| n > 0) {
            let line = String::from_utf8_lossy(&buf).trim_end_matches(['\n', '\r']).to_string();
            if err_tx.send(Event::Err(u, line)).is_err() {
                return;
            }
            buf.clear();
        }
        let _ = err_tx.send(Event::Eof(u));
    });
    let out_tx = tx.clone();
    thread::spawn(move || {
        let mut text = Vec::new();
        let _ = BufReader::new(stdout).read_to_end(&mut text);
        for line in text.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
            let _ = out_tx.send(Event::Out(line.to_vec()));
        }
    });
    Ok(Running { child, spawned_ns, started: Instant::now(), counts: Counts::default(), diagnostics: Vec::new(), eof: false })
}

/// Cargo's uplift, made atomic: a clone on macOS (which keeps the signature valid), a hard link elsewhere, a copy as the fallback.
fn uplift(src: &Path, dst: &Path) -> io::Result<()> {
    let tmp = crate::fsx::temp_sibling(dst);
    let made = if cfg!(target_os = "macos") { sys::reflink(src, &tmp) } else { fs::hard_link(src, &tmp) };
    if made.is_err() {
        let _ = fs::remove_file(&tmp);
        fs::copy(src, &tmp)?;
    }
    let renamed = sys::rename(&tmp, dst);
    let _ = fs::remove_file(&tmp);
    renamed
}

/// Leaves cargo's state as if cargo had compiled the unit: when the compile read the same files and environment and
/// printed the same diagnostics cargo recorded, cargo's dep-info timestamp moves to the compile's start, as cargo sets
/// it; otherwise the fingerprint file goes and cargo recompiles the unit through the cache. Contents are never written.
fn settle_fingerprint(unit: &plan::Unit, inputs: &[plan::Entry], env_deps: &plan::EnvDeps, diagnostics: &[String], spawned_ns: i128) -> io::Result<()> {
    let hash = Path::new(&unit.fingerprint);
    let (Some(dir), Some(name)) = (hash.parent(), hash.file_name().and_then(|n| n.to_str())) else { return Ok(()) };
    let dep = dir.join(format!("dep-{name}"));
    let cached: Vec<String> = fs::read_to_string(dir.join(format!("output-{name}"))).map(|t| t.lines().map(str::to_string).collect()).unwrap_or_default();
    let paths = |list: &[plan::Entry]| list.iter().map(|e| e.path.clone()).collect::<BTreeSet<_>>();
    let same = hash.is_file() && dep.is_file() && paths(&unit.inputs) == paths(inputs) && unit.env_deps == *env_deps && cached == diagnostics;
    if same {
        let when = UNIX_EPOCH + Duration::from_nanos(u64::try_from(spawned_ns).unwrap_or(0));
        if fs::File::options().write(true).open(&dep).and_then(|f| f.set_modified(when)).is_ok() {
            return Ok(());
        }
    }
    match fs::remove_file(hash) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}
