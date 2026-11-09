//! Cargo supervision releases build locks before executing user programs.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::cache::Cache;
use crate::cargo_args::{self, CargoArgs, Class, ExecKind, os};
use crate::config::{Config, RenderMode, TargetMode};
use crate::events::{self, CacheResult, Event, Feed};
use crate::history;
use crate::lock::{self, Acquired, BuildLock, BuildResult, Joined, LockError, LockEvent, Progress, Record, Scope, SharedFailure, WaitInfo, Waiting};
use crate::paths::Dirs;
use crate::signals;
use crate::sys::{self, StdStream};
use crate::toolchain::{self, DefaultLinker, Probe, env_sets_linker, linker_opted_out, resolve_tool, target_linker_var, tuning_label};
use crate::ui::{Outcome, RunStats, Style, Ui, UiOptions, term};
use crate::workspace;

/// `--config` on the command line, and `package."*"` overrides, are stable from this version.
const TUNING_MIN_CARGO: (u32, u32, u32) = (1, 63, 0);
const TICK: Duration = Duration::from_millis(60);
const READ_CHUNK: usize = 64 * 1024;
const MAX_LINE: usize = 16 * 1024 * 1024;
/// Draining after cargo exits is bounded so a grandchild that inherited the pipes cannot hang frust.
const LINGER: Duration = Duration::from_secs(2);
const PUBLISH_TICK: Duration = Duration::from_millis(200);
const EVICT_REGISTRY_FLAG: &str = "--frust-evict-registry";

pub fn cargo_main(args: Vec<OsString>) -> i32 {
    let started = Instant::now();
    let cfg = Config::from_env();
    let dirs = Dirs::discover();
    let mut parsed = CargoArgs::parse(args);
    // One stat when not due; at most once a day it trims the cache, metadata and scratch files.
    crate::gc::maybe_auto_gc(&dirs, &cfg);

    let Some(cargo) = resolve_tool("cargo") else {
        eprintln!("frust: cannot find `cargo` on PATH. Install Rust from https://rustup.rs or set FRUST_CARGO.");
        return 127;
    };
    let class = cargo_args::classify(parsed.subcommand.as_deref());
    if class == Class::Inert || env::var_os("FRUST_BYPASS").is_some() {
        return exec(&cargo, &parsed.original(), &[]);
    }
    // Any other subcommand leaves the flag for cargo to judge.
    let evict_registry = class == Class::Clean && parsed.strip_flag(EVICT_REGISTRY_FLAG);

    let cwd = working_dir(&parsed);
    // Both answers are memoised and revalidated each run, so a warm build spawns neither the probe nor `locate-project`.
    let manifest = parsed.value_of("--manifest-path");
    // A probe that has to run starts `locate-project` beside its rustc queries, since a new toolchain id rarely meets a memoised location.
    let mut early = None;
    let Some((probe, probed)) = Probe::cached_then(&dirs, parsed.toolchain.as_deref(), &cwd, || {
        early = Some(workspace::EarlyLocate::start(&cwd, parsed.toolchain.as_deref(), manifest.as_deref()));
    }) else {
        return exec(&cargo, &parsed.original(), &[]);
    };
    trace(&cfg, started, &format!("toolchain probed (probe cache {})", probed.label()));
    let (root, located) = workspace::locate_cached_early(&dirs, &cwd, parsed.toolchain.as_deref(), manifest.as_deref(), &probe.id, early);
    trace(&cfg, started, &format!("workspace located (locate cache {})", located.label()));

    let configs = ConfigInputs::new(root.as_deref(), &cwd);
    let mut target = pick_target_dir(&cfg, &dirs, &parsed, root.as_deref(), &probe, &cwd, &configs);
    if class == Class::CacheOnly {
        target.central = None;
    }
    if class == Class::Compile && matches!(parsed.subcommand.as_deref(), Some("check" | "c" | "clippy")) && crate::config::check_dir() {
        target = target.for_check();
    }
    let mut env_vars = accelerator_env(&cfg, &probe);
    if let Some(dir) = &target.central {
        env_vars.push((os("CARGO_TARGET_DIR"), dir.as_os_str().to_os_string()));
        let _ = fs::create_dir_all(dir);
        if let (Some(root), Some(parent)) = (&root, dir.parent()) {
            let marker = parent.join("workspace");
            if !marker.exists() {
                let _ = fs::write(marker, root.to_string_lossy().as_bytes());
            }
        }
    }
    let tuning = tuning_args(&cfg, &probe, &parsed, root.as_deref(), &cwd, class, &configs);
    if matches!(class, Class::Compile | Class::Exec(_))
        && let Some((key, value)) = default_linker(&probe, &parsed, &cwd, &configs).1
    {
        env_vars.push((key, value));
    }
    let owns_clean = class == Class::Clean && target.central.is_some() && parsed.cleans_whole_target();

    let session = Session {
        cfg,
        dirs,
        cargo,
        probe,
        root,
        cwd,
        lock_target: target.lock_key,
        env_vars,
        tuning,
        parsed,
        configs,
    };
    match class {
        Class::Compile | Class::Exec(_) => session.build_and_maybe_exec(class),
        Class::Clean if owns_clean => session.clean(evict_registry),
        _ => {
            if class == Class::Clean {
                let code = session.clean_check_directory();
                if code != 0 {
                    return code;
                }
            }
            let args = session.parsed.assemble(&session.tuning, session.parsed.subcommand.as_deref().unwrap_or(""), &session.parsed.rest);
            exec(&session.cargo, &args, &session.env_vars)
        }
    }
}

/// A workspace directory still holding another toolchain's target is left alone; the marker file only names the project.
fn remove_empty_workspace(target: &Path) {
    let Some(workspace) = target.parent() else { return };
    let holds_target = fs::read_dir(workspace).map(|entries| entries.flatten().any(|e| e.path().is_dir())).unwrap_or(true);
    if holds_target {
        return;
    }
    let _ = fs::remove_file(workspace.join("workspace"));
    let _ = fs::remove_dir(workspace);
}

fn trace(cfg: &Config, since: Instant, what: &str) {
    if cfg.debug {
        eprintln!("frust: {what} at +{}ms", since.elapsed().as_millis());
    }
}

fn working_dir(parsed: &CargoArgs) -> PathBuf {
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    match parsed.chdir() {
        Some(dir) => cwd.join(dir),
        None => cwd,
    }
}

/// Serialises every `doc` run of a target directory, whichever profile it builds in.
const DOC_SLOT: &str = "doc@output";

struct TargetChoice {
    central: Option<PathBuf>,
    lock_key: PathBuf,
}

impl TargetChoice {
    /// A frust-owned target moves to its sibling check directory; a user's target is left alone.
    fn for_check(self) -> TargetChoice {
        match self.central {
            Some(build) => {
                let check = workspace::check_target(&build);
                TargetChoice { lock_key: check.clone(), central: Some(check) }
            }
            None => self,
        }
    }
}

fn pick_target_dir(
    cfg: &Config,
    dirs: &Dirs,
    parsed: &CargoArgs,
    root: Option<&Path>,
    probe: &Probe,
    cwd: &Path,
    configs: &ConfigInputs,
) -> TargetChoice {
    let fallback = root.map(|r| r.join("target")).unwrap_or_else(|| cwd.join("target"));
    if let Some(explicit) = parsed.value_of("--target-dir") {
        return TargetChoice { central: None, lock_key: cwd.join(explicit) };
    }
    // Cargo reads both spellings; an empty value counts as unset, as it does for cargo.
    for var in ["CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR"] {
        if let Some(explicit) = env::var_os(var).filter(|v| !v.is_empty()) {
            return TargetChoice { central: None, lock_key: cwd.join(explicit) };
        }
    }
    if let Some(configured) = configured_target_dir(parsed, cwd, configs) {
        return TargetChoice { central: None, lock_key: configured.unwrap_or(fallback) };
    }
    if cfg.target == TargetMode::Local {
        return TargetChoice { central: None, lock_key: fallback };
    }
    let dir = workspace::central_target(dirs, root, &probe.id);
    TargetChoice { lock_key: dir.clone(), central: Some(dir) }
}

/// The outer `Some` means a `target-dir` is configured; the inner path is where, when it can be read.
fn configured_target_dir(parsed: &CargoArgs, cwd: &Path, configs: &ConfigInputs) -> Option<Option<PathBuf>> {
    for value in cli_configs(parsed) {
        let text = value.to_string_lossy();
        match cli_config_file(&text, cwd) {
            Some(file) => {
                if let Some(body) = Some(configs.read(&file)).filter(|b| b.contains("target-dir")) {
                    return Some(target_dir_setting(&body).map(|dir| config_relative(&file, &dir)));
                }
            }
            None if text.contains("target-dir") => return Some(target_dir_setting(&text).map(|dir| cwd.join(dir))),
            None => {}
        }
    }
    for file in &configs.files {
        let body = configs.read(file);
        if body.contains("target-dir") {
            return Some(target_dir_setting(&body).map(|dir| config_relative(file, &dir)));
        }
    }
    None
}

fn cli_configs(parsed: &CargoArgs) -> Vec<OsString> {
    let (own, _) = parsed.split_rest();
    let all: Vec<&OsString> = parsed.globals.iter().chain(own.iter()).collect();
    let mut values = Vec::new();
    let mut i = 0;
    while i < all.len() {
        let text = all[i].to_string_lossy();
        if text == "--config" {
            if let Some(value) = all.get(i + 1) {
                values.push((*value).clone());
            }
            i += 2;
            continue;
        }
        if let Some(value) = text.strip_prefix("--config=") {
            values.push(OsString::from(value));
        }
        i += 1;
    }
    values
}

fn cli_config_file(value: &str, cwd: &Path) -> Option<PathBuf> {
    let path = cwd.join(value);
    (!value.contains('=') && path.is_file()).then_some(path)
}

fn target_dir_setting(text: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some(at) = line.find("target-dir") else { continue };
        let rest = line[at + "target-dir".len()..].trim_start();
        let Some(value) = rest.strip_prefix('=') else { continue };
        let value = value.trim();
        let quote = value.chars().next()?;
        if quote != '"' && quote != '\'' {
            continue;
        }
        let inner = &value[1..];
        let end = inner.find(quote)?;
        return Some(inner[..end].to_string());
    }
    None
}

/// Cargo resolves relative config paths against the directory that holds `.cargo/`.
fn config_relative(file: &Path, dir: &str) -> PathBuf {
    let base = file.parent().and_then(Path::parent).unwrap_or(Path::new("."));
    base.join(dir)
}

/// Cargo's read order: from the working directory upwards (not the workspace root), then the root's ancestors, then `CARGO_HOME`.
pub(crate) fn config_files(root: Option<&Path>, cwd: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for start in [Some(cwd), root].into_iter().flatten() {
        for dir in start.ancestors() {
            if !dirs.iter().any(|d| d == dir) {
                dirs.push(dir.to_path_buf());
            }
        }
    }
    let mut files = Vec::new();
    for d in &dirs {
        files.push(d.join(".cargo").join("config.toml"));
        files.push(d.join(".cargo").join("config"));
    }
    let cargo_home = env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| sys::home_var().map(|h| PathBuf::from(h).join(".cargo")));
    if let Some(home) = cargo_home {
        for file in [home.join("config.toml"), home.join("config")] {
            if !files.contains(&file) {
                files.push(file);
            }
        }
    }
    files
}

/// Shares one config snapshot across target selection, tuning and lock scoping.
struct ConfigInputs {
    files: Vec<PathBuf>,
    contents: RefCell<HashMap<PathBuf, String>>,
}

impl ConfigInputs {
    fn new(root: Option<&Path>, cwd: &Path) -> Self {
        Self { files: config_files(root, cwd), contents: RefCell::new(HashMap::new()) }
    }

    fn read(&self, path: &Path) -> String {
        self.contents.borrow_mut().entry(path.to_path_buf())
            .or_insert_with(|| fs::read_to_string(path).unwrap_or_default()).clone()
    }
}

/// Splits a TOML dotted key at the `=`, returning its segments and the value text; `None` when the line is not a key assignment.
fn split_key(text: &str) -> Option<(Vec<String>, &str)> {
    let mut segments = Vec::new();
    let mut rest = text.trim_start();
    loop {
        let quote = rest.chars().next().filter(|c| matches!(c, '"' | '\''));
        let (segment, after) = match quote {
            Some(q) => {
                let inner = &rest[1..];
                let end = inner.find(q)?;
                (&inner[..end], &inner[end + 1..])
            }
            None => {
                let end = rest.find(|c: char| c.is_whitespace() || c == '.' || c == '=')?;
                (&rest[..end], &rest[end..])
            }
        };
        if segment.is_empty() && quote.is_none() {
            return None;
        }
        segments.push(segment.to_string());
        rest = after.trim_start();
        if let Some(next) = rest.strip_prefix('.') {
            rest = next.trim_start();
        } else {
            let value = rest.strip_prefix('=')?;
            return Some((segments, value.trim()));
        }
    }
}

/// Splits the body of a one-line inline table at its top-level commas.
fn inline_entries(body: &str) -> Vec<&str> {
    let (mut entries, mut depth, mut quote, mut start) = (Vec::new(), 0usize, None::<char>, 0);
    for (at, c) in body.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '{' | '[') => depth += 1,
            (None, '}' | ']') => depth = depth.saturating_sub(1),
            (None, ',') if depth == 0 => {
                entries.push(&body[start..at]);
                start = at + 1;
            }
            _ => {}
        }
    }
    entries.push(&body[start..]);
    entries
}

fn collect_assignment(prefix: &[String], key: Vec<String>, value: &str, out: &mut Vec<Vec<String>>) {
    let path: Vec<String> = prefix.iter().cloned().chain(key).collect();
    match value.strip_prefix('{') {
        Some(inner) => {
            let body = inner.rfind('}').map_or(inner, |end| &inner[..end]);
            for entry in inline_entries(body) {
                if let Some((sub, sub_value)) = split_key(entry) {
                    collect_assignment(&path, sub, sub_value, out);
                }
            }
        }
        None => out.push(path),
    }
}

/// Every assigned key of a TOML text as its full dotted path, with table headers and inline tables flattened.
fn assigned_keys(text: &str) -> Vec<Vec<String>> {
    let mut table: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(header) = line.strip_prefix('[') {
            let header = header.trim_start_matches('[');
            table = header.find(']').and_then(|end| split_header(&header[..end])).unwrap_or_default();
            continue;
        }
        if let Some((key, value)) = split_key(line) {
            collect_assignment(&table, key, value, &mut out);
        }
    }
    out
}

fn split_header(text: &str) -> Option<Vec<String>> {
    let (mut path, _) = split_key(&format!("{text}="))?;
    path.retain(|segment| !segment.is_empty());
    Some(path)
}

/// Keys of a profile that a dependency `debug=0` override would contradict.
const PROFILE_DEBUG_KEYS: [&str; 3] = ["debug", "split-debuginfo", "strip"];

fn path_conflicts(path: &[String], profiles: &[&str]) -> bool {
    let [head, name, rest @ ..] = path else { return false };
    if head != "profile" || !profiles.contains(&name.as_str()) {
        return false;
    }
    match rest {
        [key, ..] if PROFILE_DEBUG_KEYS.contains(&key.as_str()) => true,
        [package, _, key, ..] if package == "package" => PROFILE_DEBUG_KEYS.contains(&key.as_str()),
        [build, key, ..] if build == "build-override" => key == "debug",
        _ => false,
    }
}

/// True when the text sets debug-related keys of one of `profiles`; unrelated profile settings and other profiles do not count.
fn sets_profile_debug(text: &str, profiles: &[&str]) -> bool {
    assigned_keys(text).iter().any(|path| path_conflicts(path, profiles))
}

/// True when a `CARGO_PROFILE_<NAME>_...` variable sets a debug-related key of one of `profiles`.
fn env_sets_profile_debug(vars: impl IntoIterator<Item = (OsString, OsString)>, profiles: &[&str]) -> bool {
    vars.into_iter().any(|(key, _)| {
        let key = key.to_string_lossy().to_ascii_uppercase();
        profiles.iter().any(|name| {
            let prefix = format!("CARGO_PROFILE_{}_", name.to_ascii_uppercase().replace('-', "_"));
            key.strip_prefix(&prefix).is_some_and(|rest| matches!(rest, "DEBUG" | "SPLIT_DEBUGINFO" | "STRIP" | "BUILD_OVERRIDE_DEBUG"))
        })
    })
}

/// The profile chain a plain dev-like build resolves through, or `None` for release, bench, install and custom profiles.
fn tunable_profiles(parsed: &CargoArgs) -> Option<Vec<&'static str>> {
    let (own, _) = parsed.split_rest();
    let all: Vec<&OsString> = parsed.globals.iter().chain(own.iter()).collect();
    let mut chosen: Option<String> = None;
    let mut i = 0;
    while i < all.len() {
        let text = all[i].to_string_lossy();
        match text.as_ref() {
            "--release" | "-r" => return None,
            "--profile" => {
                chosen = Some(all.get(i + 1)?.to_string_lossy().into_owned());
                i += 1;
            }
            _ => {
                if let Some(name) = text.strip_prefix("--profile=") {
                    chosen = Some(name.to_string());
                }
            }
        }
        i += 1;
    }
    let sub = parsed.subcommand.as_deref().unwrap_or("");
    match (chosen.as_deref(), sub) {
        (Some("dev"), "test") | (None, "test") | (Some("test"), _) => Some(vec!["dev", "test"]),
        (Some("dev"), "bench" | "install") | (None, "bench" | "install") => None,
        (Some("dev") | None, _) => Some(vec!["dev"]),
        _ => None,
    }
}

/// `None` when unsure (`-Z`, output-moving `--config`, file targets, conflicting profiles); a wrong answer only costs queue order, as cargo locks each profile dir itself.
fn profile_and_targets(default_dir: &str, args: &[&OsStr]) -> Option<(String, Vec<String>)> {
    let mut profile: Option<String> = None;
    let mut targets: Vec<String> = Vec::new();
    let choose = |name: &str, profile: &mut Option<String>| -> Option<()> {
        match profile {
            Some(existing) if profile_dir(existing) != profile_dir(name) => None,
            _ => {
                *profile = Some(name.to_string());
                Some(())
            }
        }
    };
    let mut i = 0;
    while i < args.len() {
        let text = args[i].to_str()?;
        if text == "--" {
            break;
        }
        if text.starts_with("-Z") {
            return None;
        }
        match text {
            "--release" | "-r" => choose("release", &mut profile)?,
            "--profile" => {
                i += 1;
                choose(args.get(i)?.to_str()?, &mut profile)?;
            }
            "--target" => {
                i += 1;
                targets.push(args.get(i)?.to_str()?.to_string());
            }
            "--config" => {
                i += 1;
                if config_moves_outputs(args.get(i)?.to_str()?) {
                    return None;
                }
            }
            _ => {
                if let Some(name) = text.strip_prefix("--profile=") {
                    choose(name, &mut profile)?;
                } else if let Some(triple) = text.strip_prefix("--target=") {
                    targets.push(triple.to_string());
                } else if let Some(value) = text.strip_prefix("--config=")
                    && config_moves_outputs(value)
                {
                    return None;
                }
            }
        }
        i += 1;
    }
    let dir = match &profile {
        Some(name) => profile_dir(name)?,
        None => default_dir.to_string(),
    };
    let plain_triple = |t: &String| {
        !t.is_empty() && !t.ends_with(".json") && t.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-' | '.'))
    };
    if !targets.iter().all(plain_triple) {
        return None;
    }
    targets.sort();
    targets.dedup();
    Some((dir, targets))
}

/// `dev` and `test` share `debug`, `release` and `bench` share `release`.
fn profile_dir(name: &str) -> Option<String> {
    match name {
        "dev" | "test" => Some("debug".to_string()),
        "release" | "bench" => Some("release".to_string()),
        _ if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') => Some(name.to_string()),
        _ => None,
    }
}

fn config_moves_outputs(value: &str) -> bool {
    value.contains('=') && (value.contains("target") || value.contains("build-dir"))
}

/// `build.target` and `build.build-dir` move outputs into directories the command line does not name.
fn moves_outputs(text: &str) -> bool {
    let mut table = String::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            table = line.trim_matches(|c| c == '[' || c == ']').replace([' ', '"', '\''], "");
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        let key = key.trim().replace([' ', '"', '\''], "");
        let full = if table.is_empty() { key } else { format!("{table}.{key}") };
        let inline = full == "build" && (value.contains("target") || value.contains("build-dir"));
        if inline || matches!(full.as_str(), "build.target" | "build.build-dir") {
            return true;
        }
    }
    false
}

fn accelerator_env(cfg: &Config, probe: &Probe) -> Vec<(OsString, OsString)> {
    let mut vars = vec![(os("FRUST_RUSTC_ID"), os(&probe.id))];
    if !probe.sysroot.is_empty() {
        vars.push((os("FRUST_SYSROOT"), os(&probe.sysroot)));
        vars.push((os("FRUST_HOST"), os(&probe.host)));
    }
    if !cfg.cache && !(crate::linking::requested() && crate::linking::supported()) {
        return vars;
    }
    let foreign = env::var_os("RUSTC_WRAPPER").filter(|w| !w.is_empty());
    if foreign.is_none()
        && let Some(wrapper) = wrapper_path() {
            vars.push((os("RUSTC_WRAPPER"), wrapper.into_os_string()));
        }
    vars
}

fn prepare_store_broker(
    cfg: &Config,
    dirs: &Dirs,
    env_vars: &[(OsString, OsString)],
    feed: Option<&Feed>,
    cmd: &mut Command,
) -> Option<crate::store_broker::BrokerLaunch> {
    cmd.env_remove(crate::store_broker::CAPABILITY_ENV);
    cmd.env_remove(crate::wrapper::AFTER_BUILD_ENV);
    if feed.is_some() && cfg.cache && !cfg.store_sync && env_vars.iter().any(|(key, _)| key == "RUSTC_WRAPPER") {
        // Only a supervising front end outlives cargo to start the queued stores.
        if crate::config::store_start() == crate::config::StoreStart::AfterBuild {
            cmd.env(crate::wrapper::AFTER_BUILD_ENV, "1");
            return None;
        }
        return crate::store_broker::BrokerLaunch::prepare(dirs, cmd).ok();
    }
    None
}

fn create_phase_feed(dirs: &Dirs, mut next_path: impl FnMut() -> PathBuf) -> io::Result<Feed> {
    fs::create_dir_all(dirs.tmp())?;
    for _ in 0..8 {
        let path = next_path();
        match fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(_) => return Ok(Feed::new(path)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {},
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "event feed names collided"))
}

fn prepare_phase_feed(dirs: &Dirs, cmd: &mut Command) -> Option<Feed> {
    cmd.env_remove("FRUST_EVENTS");
    // Exclusive creation isolates this phase from stale events and writers using an earlier path.
    let feed = create_phase_feed(dirs, || crate::fsx::temp_sibling(&dirs.tmp().join("feed.jsonl"))).ok()?;
    cmd.env("FRUST_EVENTS", feed.path());
    Some(feed)
}

struct SupervisionFeed {
    feed: Option<Feed>,
    broker: Option<crate::store_broker::BrokerLaunch>,
}

impl SupervisionFeed {
    fn poll(&mut self, active: bool) -> Vec<Event> {
        let events = self.feed.as_mut().map(Feed::poll).unwrap_or_default();
        let demands = self.feed.as_mut().map(Feed::take_store_demands).unwrap_or(0);
        if active
            && let Some(broker) = &mut self.broker
        {
            broker.note_store_demands(demands);
        }
        events
    }

    fn finish(self) {
        if let Some(broker) = self.broker { broker.finish(); }
    }
}

fn wrapper_path() -> Option<PathBuf> {
    let name = format!("frust-wrap{}", env::consts::EXE_SUFFIX);
    if let Ok(exe) = env::current_exe() {
        let sibling = exe.with_file_name(&name);
        if sibling.exists() {
            return Some(sibling);
        }
    }
    crate::toolchain::find_on_path(&name)
}

/// `line-tables-only` was measured slower and larger than `debug=0`; never applied over user-written profiles from any source.
fn tuning_args(cfg: &Config, probe: &Probe, parsed: &CargoArgs, root: Option<&Path>, cwd: &Path, class: Class, configs: &ConfigInputs) -> Vec<OsString> {
    let builds = matches!(class, Class::Compile | Class::Exec(_));
    if !cfg.tune || !builds || probe.cargo_version < TUNING_MIN_CARGO || parsed.subcommand.as_deref() == Some("doc") {
        return Vec::new();
    }
    let Some(profiles) = tunable_profiles(parsed) else { return Vec::new() };
    if user_profiles(parsed, root, cwd, configs, &profiles) {
        return Vec::new();
    }
    vec![os("--config"), os("profile.dev.package.\"*\".debug=0")]
}

/// The default linker choice and, when frust picks one, the cargo variable that carries it.
fn default_linker(probe: &Probe, parsed: &CargoArgs, cwd: &Path, configs: &ConfigInputs) -> (DefaultLinker, Option<(OsString, OsString)>) {
    let user = || user_linker(parsed, cwd, configs);
    let sysroot = (!probe.sysroot.is_empty()).then(|| Path::new(probe.sysroot.as_str()));
    let opted_out = linker_opted_out();
    let msvc = probe.host.ends_with("-windows-msvc");
    let choice = toolchain::default_linker(&probe.host, opted_out, msvc && !opted_out && user(), sysroot, |p| p.is_file());
    let var = match &choice {
        DefaultLinker::Lld(path) => Some((os(&target_linker_var(&probe.host)), path.as_os_str().to_os_string())),
        _ => None,
    };
    (choice, var)
}

/// A linker named in the environment, a cargo config file or `--config` always wins over frust's default.
fn user_linker(parsed: &CargoArgs, cwd: &Path, configs: &ConfigInputs) -> bool {
    env_sets_linker(env::vars_os())
        || configs.files.iter().any(|p| crate::doctor::config_sets_linker(&configs.read(p)))
        || cli_configs(parsed).iter().any(|value| {
            let text = value.to_string_lossy();
            match cli_config_file(&text, cwd) {
                Some(file) => crate::doctor::config_sets_linker(&configs.read(&file)),
                None => crate::doctor::config_sets_linker(&text),
            }
        })
}

/// The tuning frust applies to a plain `build` in `cwd`, such as `deps-debug0,lld-link`, or `none`.
pub fn effective_tuning(dirs: &Dirs, cwd: &Path) -> String {
    let cfg = Config::from_env();
    let Some((probe, _)) = Probe::cached(dirs, None, cwd) else { return "none".to_string() };
    let parsed = CargoArgs::parse([os("build")]);
    let (root, _) = workspace::locate_cached(dirs, cwd, None, None, &probe.id);
    let configs = ConfigInputs::new(root.as_deref(), cwd);
    let deps = !tuning_args(&cfg, &probe, &parsed, root.as_deref(), cwd, Class::Compile, &configs).is_empty();
    let (linker, _) = default_linker(&probe, &parsed, cwd, &configs);
    tuning_label(deps, &linker)
}

/// True when any configuration source already sets debug-related keys for the profiles this build uses.
fn user_profiles(parsed: &CargoArgs, root: Option<&Path>, cwd: &Path, configs: &ConfigInputs, profiles: &[&str]) -> bool {
    let read = |p: &Path| configs.read(p);
    let cli = cli_configs(parsed).iter().any(|value| {
        let text = value.to_string_lossy();
        match cli_config_file(&text, cwd) {
            Some(file) => sets_profile_debug(&read(&file), profiles),
            None => sets_profile_debug(&text, profiles),
        }
    });
    cli || env_sets_profile_debug(env::vars_os(), profiles)
        || configs.files.iter().any(|p| sets_profile_debug(&read(p), profiles))
        || root.is_some_and(|r| sets_profile_debug(&read(&r.join("Cargo.toml")), profiles))
}

fn exec(cargo: &Path, args: &[OsString], env_vars: &[(OsString, OsString)]) -> i32 {
    let mut cmd = Command::new(cargo);
    cmd.env_remove(crate::wrapper::SLOT_SUPERVISOR_ENV);
    cmd.args(args);
    for (k, v) in env_vars {
        cmd.env(k, v);
    }
    let err = sys::exec_replace(&mut cmd);
    eprintln!("frust: cannot execute {}: {err}", cargo.display());
    127
}

/// Terminal and display settings are deliberately absent, so a rendered and a piped run of the same command coalesce.
fn affects_build(key: &str) -> bool {
    if key.starts_with("CARGO_TERM_") {
        return false;
    }
    key.starts_with("CARGO_")
        || key.starts_with("RUSTC")
        || key.starts_with("RUSTDOC")
        || key.starts_with("RUSTFLAGS")
        || key.starts_with("CC_")
        || key.starts_with("CXX_")
        || key.starts_with("CFLAGS")
        || key.starts_with("CXXFLAGS")
        || key.starts_with("PKG_CONFIG")
        || matches!(
            key,
            "RUSTUP_TOOLCHAIN"
                | "FRUST_CACHE"
                | "FRUST_CACHE_PRIMARY"
                | "FRUST_TUNE"
                | "FRUST_LINKER"
                | "FRUST_TARGET"
                | "FRUST_LINK"
                | "CC"
                | "CXX"
                | "AR"
                | "LD"
                | "CPPFLAGS"
                | "LDFLAGS"
                | "SDKROOT"
                | "MACOSX_DEPLOYMENT_TARGET"
        )
}

fn fingerprint(
    parts: &[&OsStr],
    args: &[OsString],
    env_vars: &[(OsString, OsString)],
    ambient: impl IntoIterator<Item = (OsString, OsString)>,
) -> String {
    let mut hasher = blake3::Hasher::new();
    let mut field = |bytes: &[u8]| {
        hasher.update(&(bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    };
    for part in parts {
        field(part.as_encoded_bytes());
    }
    field(b"--args");
    for arg in args {
        field(arg.as_encoded_bytes());
    }
    let mut vars: Vec<(OsString, OsString)> = ambient.into_iter().filter(|(k, _)| affects_build(&k.to_string_lossy())).collect();
    // Variables frust sets win over ambient ones, as they do for the child.
    vars.retain(|(k, _)| !env_vars.iter().any(|(ek, _)| ek == k));
    vars.extend(env_vars.iter().cloned());
    if !vars.iter().any(|(k, _)| k == "CARGO_HOME")
        && let Some(home) = sys::home_var() {
            vars.push((os("CARGO_HOME"), PathBuf::from(home).join(".cargo").into_os_string()));
        }
    vars.sort();
    field(b"--env");
    for (k, v) in &vars {
        field(k.as_encoded_bytes());
        field(v.as_encoded_bytes());
    }
    hasher.finalize().to_hex()[..32].to_string()
}

struct Session {
    cfg: Config,
    dirs: Dirs,
    cargo: PathBuf,
    probe: Probe,
    root: Option<PathBuf>,
    cwd: PathBuf,
    lock_target: PathBuf,
    env_vars: Vec<(OsString, OsString)>,
    tuning: Vec<OsString>,
    parsed: CargoArgs,
    configs: ConfigInputs,
}

impl Session {
    fn sub(&self) -> &str {
        self.parsed.subcommand.as_deref().unwrap_or("")
    }

    fn build_and_maybe_exec(&self, class: Class) -> i32 {
        let (phase_sub, phase_rest) = self.build_phase_command(class);
        let code = self.build_phase(class, &phase_sub, &phase_rest);
        if code != 0 || matches!(class, Class::Compile) {
            return code;
        }
        // Our lock is already released, so user code never blocks other builds.
        let args = self.parsed.assemble(&self.tuning, self.sub(), &self.parsed.rest);
        exec(&self.cargo, &args, &self.env_vars)
    }

    /// A partial clean (`-p`, `--release`, `--target`) applies to the check directory too, which cargo sees as a separate target directory.
    fn clean_check_directory(&self) -> i32 {
        let check = workspace::check_target(&self.lock_target);
        if !self.lock_target.starts_with(self.dirs.targets()) || !check.is_dir() {
            return 0;
        }
        let args = self.parsed.assemble(&self.tuning, self.sub(), &self.parsed.rest);
        let mut cmd = Command::new(&self.cargo);
        cmd.args(&args);
        for (k, v) in &self.env_vars {
            cmd.env(k, v);
        }
        cmd.env("CARGO_TARGET_DIR", &check);
        cmd.env_remove(crate::wrapper::SLOT_SUPERVISOR_ENV);
        supervise_plain(cmd)
    }

    /// Holds the whole-target build lock of the build and check directories from releasing cache entries until both are gone, so no build starts in between.
    fn clean(&self, evict_registry: bool) -> i32 {
        let check = workspace::check_target(&self.lock_target);
        let check_present = check.is_dir();
        let mut held = Vec::new();
        if self.cfg.lock {
            let scope = Scope::All;
            let mut announced = false;
            let mut on_event = |event: LockEvent<'_>| {
                if let LockEvent::Waiting(waiting) = event
                    && !announced
                {
                    announced = true;
                    eprintln!("frust: {}", describe_wait(waiting));
                }
            };
            let mut targets = vec![&self.lock_target];
            if check_present {
                targets.push(&check);
            }
            for target in targets {
                let request = lock::Request { command: "cargo clean", fingerprint: "", scope: &scope, join: false, timeout: self.cfg.lock_timeout };
                match lock::acquire(&self.dirs, target, request, &mut on_event) {
                    Ok(Acquired::Lock(guard)) => held.push(*guard),
                    Ok(Acquired::Failed(failure)) => return failure.code,
                    Err(err) => {
                        eprintln!("frust: {err}");
                        return err.exit_code();
                    }
                }
            }
        }

        let (entries, bytes) = crate::cache::clean::workspace_of(&self.dirs, &self.lock_target)
            .map(|id| Cache::new(&self.dirs).release_workspace(&id, evict_registry))
            .unwrap_or((0, 0));

        let args = self.parsed.assemble(&self.tuning, self.sub(), &self.parsed.rest);
        let mut cmd = Command::new(&self.cargo);
        cmd.args(&args);
        for (k, v) in &self.env_vars {
            cmd.env(k, v);
        }
        cmd.env_remove(crate::wrapper::SLOT_SUPERVISOR_ENV);
        let mut code = supervise_plain(cmd);
        if code == 0
            && check_present
            && let Err(err) = crate::fsx::remove_any(&check)
        {
            eprintln!("frust: cannot remove {}: {err}", check.display());
            code = 1;
        }
        if code == 0 {
            remove_empty_workspace(&self.lock_target);
        }
        if entries > 0 {
            let noun = if entries == 1 { "entry" } else { "entries" };
            eprintln!("frust: released {entries} cache {noun} ({})", crate::fsx::human_bytes(bytes));
        }
        let interrupted = signals::received().is_some() || !(0..=128).contains(&code);
        for guard in held {
            guard.finish(BuildResult { code, units: 0, interrupted });
        }
        code
    }

    fn build_phase_command(&self, class: Class) -> (String, Vec<OsString>) {
        let (own, _) = self.parsed.split_rest();
        match class {
            Class::Exec(ExecKind::Run) => ("build".to_string(), own.to_vec()),
            // Cargo rejects `--doc` with `--no-run`; build the library instead so the compile still happens under the lock.
            Class::Exec(ExecKind::Test) if own.iter().any(|a| a == "--doc") => {
                let rest = own
                    .iter()
                    .filter(|a| *a != "--no-fail-fast")
                    .map(|a| if a == "--doc" { os("--lib") } else { a.clone() })
                    .collect();
                ("build".to_string(), rest)
            }
            Class::Exec(ExecKind::Test) | Class::Exec(ExecKind::Bench) => {
                let mut rest = own.to_vec();
                if !rest.iter().any(|a| a == "--no-run") {
                    rest.push(os("--no-run"));
                }
                (self.sub().to_string(), rest)
            }
            _ => {
                let (own, tail) = self.parsed.split_rest();
                let mut rest = own.to_vec();
                rest.extend(tail.iter().cloned());
                (self.sub().to_string(), rest)
            }
        }
    }

    fn render_style(&self) -> Option<Style> {
        if !cargo_args::renderer_compatible(&self.parsed) {
            return None;
        }
        match self.cfg.render {
            RenderMode::Off => None,
            RenderMode::Plain => Some(Style::Plain),
            RenderMode::Ansi => Some(Style::Ansi),
            RenderMode::Auto => sys::ansi_terminal(StdStream::Stderr).then_some(Style::Ansi),
        }
    }

    fn fingerprint(&self, args: &[OsString]) -> String {
        let parts = [
            self.cargo.as_os_str(),
            OsStr::new(&self.probe.id),
            self.cwd.as_os_str(),
            self.lock_target.as_os_str(),
        ];
        fingerprint(&parts, args, &self.env_vars, env::vars_os())
    }

    /// Cross builds also take the host profile, which holds their build scripts and proc macros.
    fn lock_scope(&self, sub: &str, rest: &[OsString]) -> Scope {
        let (default_dir, writes_docs) = match sub {
            "build" | "b" | "check" | "c" | "clippy" | "test" | "t" => ("debug", false),
            "bench" => ("release", false),
            // `doc` builds its dependencies in the profile directory and writes the shared `target/doc`, which no profile lock covers.
            "doc" | "d" => ("debug", true),
            _ => return Scope::All,
        };
        if ["CARGO_BUILD_TARGET", "CARGO_BUILD_BUILD_DIR"].iter().any(|v| env::var_os(v).is_some_and(|x| !x.is_empty())) {
            return Scope::All;
        }
        let own = rest.iter().position(|a| a == "--").map_or(rest, |i| &rest[..i]);
        let args: Vec<&OsStr> = self.parsed.globals.iter().chain(own.iter()).map(OsString::as_os_str).collect();
        let Some((dir, triples)) = profile_and_targets(default_dir, &args) else { return Scope::All };
        let read = |p: &Path| self.configs.read(p);
        let cli_file_moves = cli_configs(&self.parsed)
            .iter()
            .any(|value| cli_config_file(&value.to_string_lossy(), &self.cwd).is_some_and(|file| moves_outputs(&read(&file))));
        if cli_file_moves || self.configs.files.iter().any(|file| moves_outputs(&read(file))) {
            return Scope::All;
        }
        let mut slots = vec![format!("{dir}@host")];
        slots.extend(triples.iter().map(|t| format!("{dir}@{t}")));
        if writes_docs {
            slots.push(DOC_SLOT.to_string());
        }
        Scope::slots(slots)
    }

    /// `check`, `build`, `test` and `clippy` see different unit counts, so they must not share one record.
    fn history_keys(&self, workspace_key: &str, sub: &str, rest: &[OsString]) -> Vec<String> {
        let mut exact = self.parsed.assemble(&self.tuning, sub, &[]);
        exact.extend(rest.iter().cloned());
        let digest = crate::paths::short_hash(exact.iter().flat_map(|a| a.as_encoded_bytes().iter().copied().chain([0])).collect::<Vec<u8>>());
        vec![format!("{workspace_key}-{sub}-{digest}"), format!("{workspace_key}-{sub}")]
    }

    fn build_phase(&self, class: Class, sub: &str, rest: &[OsString]) -> i32 {
        let started = Instant::now();
        let style = self.render_style();
        let workspace_name = workspace::display_name(self.root.as_deref());
        let key = self.root.as_deref().map(workspace::workspace_key).unwrap_or_else(|| "_loose".into());
        let history_keys = self.history_keys(&key, sub, rest);
        let expected = history::expected_units(&self.dirs, &history_keys);

        let mut ui = style.map(|style| {
            Ui::new(
                UiOptions {
                    style,
                    width: term::terminal_width(),
                    osc: self.cfg.osc,
                    workspace: workspace_name.clone(),
                    command: sub.to_string(),
                    expected_units: expected,
                    toolchain: format!("rustc {}", self.probe.release),
                },
                Box::new(std::io::stderr()),
                started,
            )
        });

        let command = self.parsed.assemble(&self.tuning, sub, &[]);
        let mut phase_args = command.clone();
        phase_args.extend(rest.iter().cloned());
        let fingerprint = self.fingerprint(&phase_args);
        let scope = self.lock_scope(sub, rest);
        trace(&self.cfg, started, &format!("lock scope: {}", scope.label()));

        let mut noop = crate::noop::Gate::new(crate::noop::Invocation {
            dirs: &self.dirs,
            compile: class == Class::Compile,
            args: &phase_args,
            fingerprint: &fingerprint,
            cwd: &self.cwd,
            root: self.root.as_deref(),
            target: &self.lock_target,
            scope: &scope,
            style: style.map(|s| if s == Style::Ansi { "ansi" } else { "plain" }),
            colour: term::colour_enabled(),
            locking: self.cfg.lock,
            env_vars: &self.env_vars,
            configs: self.noop_configs(),
            debug: self.cfg.debug,
        });
        // Decided before the lock wait: an unchanged repeat never queues behind anything or launches cargo.
        if let Some(replay) = noop.check() {
            trace(&self.cfg, started, "no-op fast path");
            return self.serve_noop(&noop, &replay, ui.as_mut(), started, sub, workspace_name, &history_keys);
        }
        let chosen_linker = os(&target_linker_var(&self.probe.host));
        if let Some((_, linker)) = self.env_vars.iter().find(|(key, _)| *key == chosen_linker) {
            crate::wrapper::warm_tool_hash(&self.dirs, Path::new(linker), self.cfg.debug);
        }

        let mut joining = Joining::new(style);
        let mut guard = match self.acquire_lock(sub, &fingerprint, &scope, ui.as_mut(), &mut joining) {
            Ok(Some(Acquired::Lock(guard))) => Some(*guard),
            Ok(None) => None,
            Ok(Some(Acquired::Failed(failure))) => {
                return self.report_shared_failure(&failure, ui.as_mut(), &joining, style, started, sub, workspace_name);
            }
            Err(err) => {
                // Restore the terminal (cursor, OSC state) before reporting.
                if let Some(ui) = ui.as_mut() {
                    ui.finish(Outcome { success: false, exit_code: err.exit_code() }, Instant::now());
                }
                eprintln!("frust: {err}");
                return err.exit_code();
            }
        };
        let joined = guard.as_ref().and_then(|g| g.joined().cloned());

        // Plain output is observed through cargo's JSON only when it is logged anyway; stderr is identical in both formats.
        let observed = noop.wants_capture() || (noop.observable() && crate::exec::enabled());
        let mut recorder = (observed && (style.is_some() || guard.is_some())).then(crate::noop::Recorder::default);
        let mut exec_gate = crate::exec::Gate::new(crate::exec::Invocation {
            dirs: &self.dirs,
            record: noop.record_path(),
            fingerprint: &fingerprint,
            cwd: &self.cwd,
            root: self.root.as_deref(),
            target: &self.lock_target,
            eligible: recorder.is_some() && guard.is_some() && !noop.auditing(),
            env_vars: &self.env_vars,
            configs: self.noop_configs(),
            args: &phase_args,
            debug: self.cfg.debug,
        });
        // Decided under the lock: the executor stands in for cargo and speaks its output format.
        let replay = exec_gate.check();
        let executing = replay.is_some();
        let mut cmd = match replay {
            Some(cmd) => cmd,
            None => {
                let mut cmd = Command::new(&self.cargo);
                cmd.args(&command);
                if style.is_some() || recorder.is_some() {
                    cmd.arg("--message-format=json-render-diagnostics");
                }
                cmd.args(rest);
                exec_gate.journal(&mut cmd);
                cmd
            }
        };
        if executing {
            trace(&self.cfg, started, "executor replays the plan");
        }
        for (k, v) in &self.env_vars {
            cmd.env(k, v);
        }
        cmd.env(crate::wrapper::CARGO_COMMAND_ENV, sub);
        cmd.env_remove(crate::wrapper::SLOT_SUPERVISOR_ENV);
        if !executing
            && self.env_vars.iter().any(|(key, _)| key == "RUSTC_WRAPPER")
            && (1, 89, 0) <= self.probe.cargo_version
            && self.probe.cargo_version < (1, 100, 0)
        {
            cmd.env(crate::wrapper::SLOT_SUPERVISOR_ENV, std::process::id().to_string());
        }
        let tracked = style.is_some() || guard.is_some();
        let feed = if tracked { prepare_phase_feed(&self.dirs, &mut cmd) } else { None };
        let feed_path = feed.as_ref().map(|feed| feed.path().to_owned());

        let broker = prepare_store_broker(&self.cfg, &self.dirs, &self.env_vars, feed.as_ref(), &mut cmd);
        let drain_after = cmd.get_envs().any(|(key, value)| key == crate::wrapper::AFTER_BUILD_ENV && value.is_some());
        let mut feed = SupervisionFeed { feed, broker };
        let mut progress = Progress {
            expected,
            ..Progress::default()
        };
        trace(&self.cfg, started, "lock acquired, starting cargo");
        let launched_ns = crate::cache::hashing::now_ns();
        let code = match (style, ui.as_mut()) {
            (Some(style), Some(ui)) => {
                cmd.env("CARGO_TERM_PROGRESS_WHEN", "never");
                cmd.env("CARGO_TERM_WIDTH", term::terminal_width().to_string());
                if style == Style::Ansi && term::colour_enabled() {
                    cmd.env("CARGO_TERM_COLOR", "always");
                }
                self.supervise_rendered(cmd, ui, &mut feed, &mut progress, guard.as_mut(), &mut joining.shown, recorder.as_mut())
            }
            _ => match guard.as_mut() {
                Some(lock) => supervise_plain_logged(cmd, &mut feed, &mut progress, lock, recorder.as_mut()),
                None => supervise_plain(cmd),
            },
        };
        // Frontend EOF closes admission; queued stores outlive this Cargo phase.
        feed.finish();
        if drain_after {
            crate::wrapper::drain_after_build(&self.dirs, &self.cwd);
        }
        trace(&self.cfg, started, "cargo finished");

        let interrupted = signals::received().is_some() || !(0..=128).contains(&code);
        let exec_failure = exec_gate.finish(code, interrupted, recorder.as_ref());
        noop.set_exec(exec_gate.trace());
        trace(&self.cfg, started, "executor plan settled");
        // Recorded before the lock passes on, so no other build changes the target in between.
        let audit_failure = noop.finish(code, interrupted, launched_ns, recorder.as_ref());
        trace(&self.cfg, started, "no-op record settled");
        let audit_failure = match (exec_failure, audit_failure) {
            (Some(a), Some(b)) => Some(format!("{a}; {b}")),
            (a, b) => a.or(b),
        };
        let code = if audit_failure.is_some() && code == 0 { 1 } else { code };
        // A signalled build is not a result to share.
        if let Some(guard) = guard.take() {
            guard.finish(BuildResult { code, units: progress.done, interrupted });
        }
        if code == 0 {
            self.start_compaction();
        }
        let stats = ui.as_mut().map(|ui| ui.finish(Outcome { success: code == 0, exit_code: code }, Instant::now()));
        if let Some(failure) = &audit_failure {
            eprintln!("frust: {failure}");
        }
        noop.trace();
        if let Some(path) = feed_path {
            let _ = fs::remove_file(path);
        }
        if code == 0
            && let Some(joined) = &joined {
                report_joined(joined, style, stats.as_ref());
            }
        if let Some(stats) = stats {
            if code == 0 {
                history::remember_units(&self.dirs, &history_keys, stats.units);
            }
            events::record(
                &self.dirs,
                &Event::Run {
                    t: events::now_ms(),
                    workspace: workspace_name,
                    command: sub.to_string(),
                    wall_ms: started.elapsed().as_millis() as u64,
                    units: stats.units,
                    cached: stats.cached,
                    compiled: stats.compiled,
                    fresh: stats.fresh,
                    saved_ms: stats.saved_ms,
                    success: code == 0,
                },
            );
        } else if code == 0 && progress.done > 0 && expected.is_none() {
            // Without the display only cache events are counted, which undercounts; use only to seed a first estimate.
            history::remember_units(&self.dirs, &history_keys, progress.done);
        }
        if code == 0 {
            crate::update::notice(&self.dirs);
        }
        code
    }

    /// Every config file cargo may read, including `--config` files, absent ones as well.
    fn noop_configs(&self) -> Vec<PathBuf> {
        let mut files = self.configs.files.clone();
        files.extend(cli_configs(&self.parsed).iter().filter_map(|value| cli_config_file(&value.to_string_lossy(), &self.cwd)));
        files
    }

    /// Prints what cargo would for an unchanged build; daily gc already ran at startup and due compaction runs afterwards.
    #[allow(clippy::too_many_arguments)]
    fn serve_noop(
        &self,
        noop: &crate::noop::Gate,
        replay: &[(crate::noop::Stream, String)],
        ui: Option<&mut Ui>,
        started: Instant,
        sub: &str,
        workspace_name: String,
        history_keys: &[String],
    ) -> i32 {
        let elapsed = started.elapsed();
        match ui {
            Some(ui) => {
                let (mut progress, mut shown) = (Progress::default(), Shown::default());
                let now = Instant::now();
                for (stream, line) in replay {
                    let line = vec![crate::noop::retime(line, elapsed).into_bytes()];
                    match stream {
                        crate::noop::Stream::Out => render_lines(ui, &mut progress, &mut shown, &line, &[], now),
                        crate::noop::Stream::Err => render_lines(ui, &mut progress, &mut shown, &[], &line, now),
                    }
                }
                let stats = ui.finish(Outcome { success: true, exit_code: 0 }, Instant::now());
                history::remember_units(&self.dirs, history_keys, stats.units);
                events::record(
                    &self.dirs,
                    &Event::Run {
                        t: events::now_ms(),
                        workspace: workspace_name,
                        command: sub.to_string(),
                        wall_ms: started.elapsed().as_millis() as u64,
                        units: stats.units,
                        cached: stats.cached,
                        compiled: stats.compiled,
                        fresh: stats.fresh,
                        saved_ms: stats.saved_ms,
                        success: true,
                    },
                );
            }
            None => {
                let mut stderr = io::stderr().lock();
                for (stream, line) in replay {
                    if *stream == crate::noop::Stream::Err {
                        let _ = writeln!(stderr, "{}", crate::noop::retime(line, elapsed));
                    }
                }
                let _ = stderr.flush();
            }
        }
        // The compaction schedule is kept as after a cargo no-op; a pass that changes the target only makes the next check decline.
        self.start_compaction();
        noop.trace();
        crate::update::notice(&self.dirs);
        0
    }

    /// Due compaction runs detached once this build no longer holds the target, so it never delays this build or the next.
    fn start_compaction(&self) {
        if !self.lock_target.starts_with(self.dirs.targets()) {
            return;
        }
        let outcome = crate::compact::start_auto(&self.dirs, &self.lock_target, &self.cfg);
        if self.cfg.debug {
            eprintln!("frust: auto-compact {outcome}");
        }
    }

    fn acquire_lock(&self, sub: &str, fingerprint: &str, scope: &Scope, mut ui: Option<&mut Ui>, joining: &mut Joining) -> Result<Option<Acquired>, LockError> {
        if !self.cfg.lock {
            return Ok(None);
        }
        let description = format!("cargo {sub}");
        let request = lock::Request { command: &description, fingerprint, scope, join: true, timeout: self.cfg.lock_timeout };
        let acquired = lock::acquire(&self.dirs, &self.lock_target, request, &mut |event| joining.handle(ui.as_deref_mut(), event))?;
        if joining.waited
            && let Some(ui) = ui {
                ui.lock_acquired(Instant::now());
            }
        Ok(Some(acquired))
    }

    #[allow(clippy::too_many_arguments)]
    fn report_shared_failure(
        &self,
        failure: &SharedFailure,
        ui: Option<&mut Ui>,
        joining: &Joining,
        style: Option<Style>,
        started: Instant,
        sub: &str,
        workspace_name: String,
    ) -> i32 {
        let code = failure.code;
        if ui.is_none() {
            joining.replay();
        }
        let stats = ui.map(|ui| ui.finish(Outcome { success: false, exit_code: code }, Instant::now()));
        let text = format!("build of {} (identical command) failed with exit code {code}", failure.holder.label());
        match style {
            Some(Style::Plain) => eprintln!("shared    {text}"),
            Some(Style::Ansi) => eprintln!("  {}", term::Paint::new(term::colour_enabled()).dim(&format!("shared {text}"))),
            None => eprintln!("frust: shared {text}"),
        }
        if let Some(stats) = stats {
            events::record(
                &self.dirs,
                &Event::Run {
                    t: events::now_ms(),
                    workspace: workspace_name,
                    command: sub.to_string(),
                    wall_ms: started.elapsed().as_millis() as u64,
                    units: stats.units,
                    cached: stats.cached,
                    compiled: stats.compiled,
                    fresh: stats.fresh,
                    saved_ms: stats.saved_ms,
                    success: false,
                },
            );
        }
        code
    }

    /// Both pipes are polled on this thread, so a slow terminal back-pressures cargo and a lingering grandchild cannot hang the build (see [`LINGER`]).
    #[allow(clippy::too_many_arguments)]
    fn supervise_rendered(
        &self,
        mut cmd: Command,
        ui: &mut Ui,
        feed: &mut SupervisionFeed,
        progress: &mut Progress,
        mut lock: Option<&mut BuildLock>,
        shown: &mut Shown,
        mut recorder: Option<&mut crate::noop::Recorder>,
    ) -> i32 {
        let planning_args: Vec<OsString> = cmd.get_args()
            .filter(|arg| *arg != "--message-format=json-render-diagnostics")
            .map(OsStr::to_os_string)
            .collect();
        cmd.stdin(Stdio::inherit()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(err) => {
                eprintln!("frust: cannot run {}: {err}", self.cargo.display());
                return 127;
            }
        };
        let child_started = Instant::now();
        signals::supervise(child.id());
        let sub = self.parsed.subcommand.as_deref().unwrap_or("");
        let executor = planning_args.first().is_some_and(|a| a == crate::exec::CHILD_ARG);
        let mut planning_gate = PlanningGate { pending: crate::unit_graph::supported(sub) && !executor };
        let mut planning = None;
        if let Some(lock) = lock.as_deref_mut() {
            lock.log_start(true, true);
        }

        let mut streams = [Stream::new(child.stdout.take().map(sys::Pipe::from)), Stream::new(child.stderr.take().map(sys::Pipe::from))];
        let mut chunk = vec![0u8; READ_CHUNK];
        let mut status = None;
        let mut exited_at = None::<Instant>;
        loop {
            if streams.iter().all(|s| s.file.is_none()) {
                break;
            }
            if status.is_none()
                && let Ok(Some(done)) = child.try_wait() {
                    status = Some(done);
                    exited_at = Some(Instant::now());
                }
            if exited_at.is_some_and(|t| t.elapsed() >= LINGER) {
                break;
            }
            if let Some(pending) = planning_gate.start_if_due(child_started.elapsed(), status.is_none(), || {
                crate::unit_graph::Pending::start(&self.cargo, &planning_args, &[], &self.env_vars)
            }) {
                planning = Some(pending);
            }
            let ready = wait_readable(&streams, TICK);
            let mut lines: [Vec<Vec<u8>>; 2] = [Vec::new(), Vec::new()];
            for (i, stream) in streams.iter_mut().enumerate() {
                if ready[i] {
                    stream.read_lines(&mut chunk, &mut lines[i]);
                }
            }
            if let Some(exact) = planning.as_mut().and_then(crate::unit_graph::Pending::poll) {
                progress.expected = Some(exact);
                ui.set_expected_units(exact);
                planning = None;
            }
            let now = Instant::now();
            // Cache events lag the JSON stream by a poll interval; drain them first.
            for ev in feed.poll(status.is_none()) {
                track_cache(progress, &ev, false);
                if let Some(lock) = lock.as_deref_mut() {
                    lock.log_cache(&ev);
                }
                ui.on_cache_event(&ev, now);
            }
            if lines.iter().all(Vec::is_empty) {
                ui.tick(now);
            }
            let [out, err] = lines;
            if let Some(recorder) = recorder.as_deref_mut() {
                recorder.take(&out, &err);
            }
            log_lines(lock.as_deref_mut(), &out, &err);
            render_lines(ui, progress, shown, &out, &err, now);
            if let Some(lock) = lock.as_deref_mut() {
                lock.publish(progress);
                lock.flush_log();
            }
        }
        // Whatever is left without a trailing newline is still a line.
        let tails: Vec<Vec<Vec<u8>>> = streams.iter_mut().map(|s| s.take_partial().into_iter().collect()).collect();
        drop(streams);
        let now = Instant::now();
        if let Some(recorder) = recorder {
            recorder.take(&tails[0], &tails[1]);
        }
        log_lines(lock.as_deref_mut(), &tails[0], &tails[1]);
        render_lines(ui, progress, shown, &tails[0], &tails[1], now);
        for ev in feed.poll(false) {
            track_cache(progress, &ev, false);
            if let Some(lock) = lock.as_deref_mut() {
                lock.log_cache(&ev);
            }
            ui.on_cache_event(&ev, now);
        }
        let status = match status {
            Some(status) => Ok(status),
            None => child.wait(),
        };
        signals::clear();
        status.map(signals::exit_code).unwrap_or(1)
    }
}

struct PlanningGate {
    pending: bool,
}

impl PlanningGate {
    // The clock and launcher are supplied at the boundary so tests need no scheduler timing assumptions.
    fn start_if_due<T>(&mut self, elapsed: Duration, running: bool, launch: impl FnOnce() -> Option<T>) -> Option<T> {
        if !running {
            self.pending = false;
        }
        if !self.pending || elapsed < TICK {
            return None;
        }
        self.pending = false;
        launch()
    }
}

fn log_lines(lock: Option<&mut BuildLock>, out: &[Vec<u8>], err: &[Vec<u8>]) {
    if let Some(lock) = lock {
        for line in out {
            lock.log_out(line);
        }
        for line in err {
            lock.log_err(line);
        }
    }
}

fn render_lines(ui: &mut Ui, progress: &mut Progress, shown: &mut Shown, out: &[Vec<u8>], err: &[Vec<u8>], now: Instant) {
    for line in out {
        let text = String::from_utf8_lossy(line);
        match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(value) => {
                if shown.skip_json(&value) {
                    continue;
                }
                track_json(progress, &value);
                ui.on_cargo_json(&value, now);
            }
            Err(_) if !text.trim().is_empty() => {
                let _ = writeln!(std::io::stdout(), "{text}");
            }
            Err(_) => {}
        }
    }
    for line in err {
        let text = String::from_utf8_lossy(line);
        if shown.skip_line(&text) {
            continue;
        }
        track_stderr(progress, &text);
        ui.on_cargo_stderr(&text, now);
    }
}

/// What a followed log already showed, so this build's own later cargo run does not repeat those units and diagnostics.
#[derive(Default)]
struct Shown {
    units: HashSet<String>,
    lines: HashMap<String, u32>,
}

impl Shown {
    fn add_json(&mut self, msg: &serde_json::Value) {
        if let Some(key) = unit_key(msg) {
            self.units.insert(key);
        }
    }

    fn add_line(&mut self, line: &str) {
        if !is_status_line(line) {
            *self.lines.entry(term::strip_ansi(line)).or_default() += 1;
        }
    }

    /// A fresh artifact for a unit already shown: cargo is only confirming it.
    fn skip_json(&self, msg: &serde_json::Value) -> bool {
        msg.get("fresh").and_then(|f| f.as_bool()) == Some(true) && unit_key(msg).is_some_and(|k| self.units.contains(&k))
    }

    /// A diagnostic already shown: cargo replays the warnings of fresh units.
    fn skip_line(&mut self, line: &str) -> bool {
        if self.lines.is_empty() || is_status_line(line) {
            return false;
        }
        match self.lines.get_mut(&term::strip_ansi(line)) {
            Some(n) if *n > 0 => {
                *n -= 1;
                true
            }
            _ => false,
        }
    }
}

fn unit_key(msg: &serde_json::Value) -> Option<String> {
    if msg.get("reason").and_then(|r| r.as_str()) != Some("compiler-artifact") {
        return None;
    }
    let field = |name: &str| msg.get(name).map(|v| v.to_string()).unwrap_or_default();
    Some(format!("{}|{}|{}|{}", field("package_id"), field("target"), field("profile"), field("features")))
}

fn is_status_line(line: &str) -> bool {
    let plain = term::strip_ansi(line);
    let body = plain.trim_start();
    if body.len() == plain.len() {
        return false;
    }
    let word: String = body.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    word.chars().next().is_some_and(|c| c.is_ascii_uppercase()) && body[word.len()..].starts_with(' ')
}

enum Replay {
    Out(String),
    Err(String),
}

struct Joining {
    style: Option<Style>,
    shown: Shown,
    /// Without the display: the followed build's output, replayed if it fails.
    replay: Vec<Replay>,
    /// The followed log's stdout lines are cargo JSON that frust asked for.
    json: bool,
    following: Option<String>,
    waited: bool,
    last_notice: Option<(Instant, Option<u32>)>,
}

impl Joining {
    fn new(style: Option<Style>) -> Joining {
        Joining { style, shown: Shown::default(), replay: Vec::new(), json: false, following: None, waited: false, last_notice: None }
    }

    fn handle(&mut self, ui: Option<&mut Ui>, event: LockEvent<'_>) {
        let now = Instant::now();
        match event {
            LockEvent::Waiting(waiting) => {
                self.waited = true;
                match (ui, &self.following) {
                    // The followed build's own output drives the display.
                    (Some(_), Some(_)) => {}
                    (Some(ui), None) => ui.lock_waiting(&display_wait_info(waiting), now),
                    (None, following) => {
                        let pid = waiting.info.owner.as_ref().map(|o| o.pid);
                        let due = self.last_notice.is_none_or(|(t, p)| p != pid || now.duration_since(t) >= Duration::from_secs(5));
                        if due {
                            self.last_notice = Some((now, pid));
                            match following {
                                Some(label) => eprintln!("frust: following build of {label}: {}", waiting.progress.as_ref().map(Progress::summary).unwrap_or_else(|| "starting".to_string())),
                                None => eprintln!("frust: {}", describe_wait(waiting)),
                            }
                        }
                    }
                }
            }
            LockEvent::Joined(holder) => {
                self.following = Some(holder.label());
                self.json = false;
                self.replay.clear();
                let text = format!("running build of {} (identical command)", holder.label());
                match ui {
                    Some(ui) => {
                        ui.lock_acquired(now);
                        ui.note("joined", &text, now);
                    }
                    None => eprintln!("frust: joined {text}"),
                }
            }
            LockEvent::Record(record) => self.record(ui, record, now),
            LockEvent::Abandoned(holder, why) => {
                self.following = None;
                self.replay.clear();
                let text = format!("build of {} stopped ({}); building here", holder.label(), why.describe());
                match ui {
                    Some(ui) => ui.note("takeover", &text, now),
                    None => eprintln!("frust: {text}"),
                }
            }
        }
    }

    fn record(&mut self, ui: Option<&mut Ui>, record: &Record, now: Instant) {
        match (record, ui) {
            (Record::Start { json, .. }, _) => self.json = *json,
            (Record::Out { l }, Some(ui)) => {
                if self.json
                    && let Ok(value) = serde_json::from_str::<serde_json::Value>(l)
                {
                    self.shown.add_json(&value);
                    ui.on_cargo_json(&value, now);
                }
            }
            (Record::Out { l }, None) => {
                // frust's JSON is not what cargo prints for this command; the user's own is.
                if !self.json {
                    self.replay.push(Replay::Out(l.clone()));
                }
            }
            (Record::Err { l }, Some(ui)) => {
                // Without cargo's JSON no unit completes, so take only diagnostics and the closing status from the human-output log.
                if !self.json && is_status_line(l) && !term::strip_ansi(l).trim_start().starts_with("Finished") {
                    return;
                }
                self.shown.add_line(l);
                if self.style == Some(Style::Ansi) && !term::colour_enabled() {
                    ui.on_cargo_stderr(&term::strip_ansi(l), now);
                } else {
                    ui.on_cargo_stderr(l, now);
                }
            }
            (Record::Err { l }, None) => self.replay.push(Replay::Err(l.clone())),
            (Record::Cache { e }, Some(ui)) => ui.on_cache_event(e, now),
            _ => {}
        }
    }

    /// Colour is kept only when stderr is a colour terminal.
    fn replay(&self) {
        let keep_colour = sys::ansi_terminal(StdStream::Stderr) && term::colour_enabled();
        let fix = |line: &str| if keep_colour { line.to_string() } else { term::strip_ansi(line) };
        let mut stdout = io::stdout().lock();
        let mut stderr = io::stderr().lock();
        for item in &self.replay {
            let _ = match item {
                Replay::Out(line) => writeln!(stdout, "{line}"),
                Replay::Err(line) => writeln!(stderr, "{}", fix(line)),
            };
        }
        let _ = stdout.flush();
        let _ = stderr.flush();
    }
}

struct Stream {
    file: Option<sys::Pipe>,
    buf: Vec<u8>,
}

impl Stream {
    fn new(file: Option<sys::Pipe>) -> Stream {
        Stream { file, buf: Vec::new() }
    }

    fn read_lines(&mut self, chunk: &mut [u8], out: &mut Vec<Vec<u8>>) {
        self.read_into(chunk, out, None);
    }

    fn read_tee(&mut self, chunk: &mut [u8], out: &mut Vec<Vec<u8>>, sink: &mut dyn Write) {
        self.read_into(chunk, out, Some(sink));
    }

    fn read_into(&mut self, chunk: &mut [u8], out: &mut Vec<Vec<u8>>, sink: Option<&mut dyn Write>) {
        let Some(file) = self.file.as_mut() else { return };
        match file.read(chunk) {
            Ok(0) => {
                out.extend(self.take_partial());
                self.file = None;
            }
            Ok(n) => {
                if let Some(sink) = sink {
                    let _ = sink.write_all(&chunk[..n]);
                    let _ = sink.flush();
                }
                let search_from = self.buf.len();
                self.buf.extend_from_slice(&chunk[..n]);
                let mut start = 0;
                let mut search = search_from;
                while let Some(offset) = self.buf[search..].iter().position(|&b| b == b'\n') {
                    let end = search + offset;
                    out.push(self.buf[start..end].to_vec());
                    start = end + 1;
                    search = start;
                }
                self.buf.drain(..start);
                if self.buf.len() > MAX_LINE {
                    out.push(std::mem::take(&mut self.buf));
                }
            }
            Err(err) if matches!(err.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => {}
            Err(_) => {
                out.extend(self.take_partial());
                self.file = None;
            }
        }
    }

    fn take_partial(&mut self) -> Option<Vec<u8>> {
        (!self.buf.is_empty()).then(|| std::mem::take(&mut self.buf))
    }
}

fn wait_readable(streams: &[Stream; 2], timeout: Duration) -> [bool; 2] {
    if streams.iter().all(|s| s.file.is_none()) {
        return [false; 2];
    }
    sys::wait_readable([streams[0].file.as_ref(), streams[1].file.as_ref()], timeout)
}

fn track_json(progress: &mut Progress, msg: &serde_json::Value) {
    if msg.get("reason").and_then(|r| r.as_str()) != Some("compiler-artifact") {
        return;
    }
    progress.done += 1;
    if msg.get("fresh").and_then(|f| f.as_bool()) == Some(true) {
        progress.fresh += 1;
    }
    // A compiled build script is not the end of its crate: the script runs next, then the library, under one `Compiling` line.
    let custom_build = msg
        .pointer("/target/kind")
        .and_then(|k| k.as_array())
        .is_some_and(|kinds| kinds.iter().any(|k| k.as_str() == Some("custom-build")));
    if custom_build {
        return;
    }
    let target = msg.pointer("/target/name").and_then(|n| n.as_str()).unwrap_or("");
    let id = msg.get("package_id").and_then(|n| n.as_str()).unwrap_or("");
    progress.finish(&[target, &package_name(id)]);
}

/// Handles the old id form (`serde 1.0.0 (registry+...)`) and the new one (`registry+https://...#serde@1.0.0`).
fn package_name(id: &str) -> String {
    if let Some((head, fragment)) = id.split_once('#') {
        return match fragment.split_once('@') {
            Some((name, _)) => name.to_string(),
            None => head.rsplit('/').next().unwrap_or("").to_string(),
        };
    }
    id.split_whitespace().next().unwrap_or("").to_string()
}

fn track_stderr(progress: &mut Progress, line: &str) {
    let plain = term::strip_ansi(line);
    let mut words = plain.split_whitespace();
    if matches!(words.next(), Some("Compiling" | "Checking" | "Documenting")) {
        // Some cargo versions announce a crate's build script separately; one entry each.
        if let Some(name) = words.next().filter(|n| !progress.active.iter().any(|a| a == n)) {
            progress.start(name);
        }
    }
}

/// Without cargo's JSON stream, cache events are the only record of finished units (`counts_units`).
fn track_cache(progress: &mut Progress, ev: &Event, counts_units: bool) {
    let Event::Cache { result, .. } = ev else { return };
    if *result == CacheResult::Hit {
        progress.cached += 1;
    }
    if counts_units {
        progress.done += 1;
    }
}

fn supervise_plain(mut cmd: Command) -> i32 {
    match cmd.spawn() {
        Ok(mut child) => {
            signals::supervise(child.id());
            let code = child.wait().map(signals::exit_code).unwrap_or(1);
            signals::clear();
            code
        }
        Err(err) => {
            eprintln!("frust: cannot run cargo: {err}");
            127
        }
    }
}

/// Output not going to a terminal passes through byte for byte so it can be logged; terminal output stays cargo's own, so followers get no output.
fn supervise_plain_logged(cmd: Command, feed: &mut SupervisionFeed, progress: &mut Progress, lock: &mut BuildLock, mut recorder: Option<&mut crate::noop::Recorder>) -> i32 {
    let capture_out = !io::stdout().is_terminal();
    let capture_err = !io::stderr().is_terminal();
    // With a recorder, stdout is cargo's JSON (requested by frust), never shown, but complete for followers.
    let json = recorder.is_some();
    if let Some(recorder) = recorder.as_deref_mut()
        && !capture_err
    {
        recorder.refuse("stderr not captured");
    }
    lock.log_start(json, (capture_out || json) && capture_err);
    if capture_out || capture_err || json {
        supervise_tee(cmd, feed, progress, lock, capture_out, capture_err, recorder)
    } else {
        supervise_plain_publishing(cmd, feed, progress, lock)
    }
}

/// Without a recorder stdout is passed through as bytes; with one only non-JSON lines are forwarded.
fn forward_plain_stdout(stdout: &mut io::Stdout, lines: &[Vec<u8>]) {
    for line in lines.iter().filter(|l| !l.trim_ascii_start().starts_with(b"{")) {
        let _ = stdout.write_all(line);
        let _ = stdout.write_all(b"\n");
    }
    let _ = stdout.flush();
}

fn supervise_tee(
    mut cmd: Command,
    feed: &mut SupervisionFeed,
    progress: &mut Progress,
    lock: &mut BuildLock,
    capture_out: bool,
    capture_err: bool,
    mut recorder: Option<&mut crate::noop::Recorder>,
) -> i32 {
    let json = recorder.is_some();
    cmd.stdin(Stdio::inherit());
    cmd.stdout(if capture_out || json { Stdio::piped() } else { Stdio::inherit() });
    cmd.stderr(if capture_err { Stdio::piped() } else { Stdio::inherit() });
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            eprintln!("frust: cannot run cargo: {err}");
            return 127;
        }
    };
    signals::supervise(child.id());
    let mut streams = [Stream::new(child.stdout.take().map(sys::Pipe::from)), Stream::new(child.stderr.take().map(sys::Pipe::from))];
    let mut chunk = vec![0u8; READ_CHUNK];
    let mut status = None;
    let mut exited_at = None::<Instant>;
    let mut stdout = io::stdout();
    let mut stderr = io::stderr();
    while streams.iter().any(|s| s.file.is_some()) {
        if status.is_none()
            && let Ok(Some(done)) = child.try_wait() {
                status = Some(done);
                exited_at = Some(Instant::now());
            }
        if exited_at.is_some_and(|t| t.elapsed() >= LINGER) {
            break;
        }
        let ready = wait_readable(&streams, TICK);
        let mut lines: [Vec<Vec<u8>>; 2] = [Vec::new(), Vec::new()];
        if ready[0] {
            if json {
                streams[0].read_lines(&mut chunk, &mut lines[0]);
            } else {
                streams[0].read_tee(&mut chunk, &mut lines[0], &mut stdout);
            }
        }
        if ready[1] {
            streams[1].read_tee(&mut chunk, &mut lines[1], &mut stderr);
        }
        for ev in feed.poll(status.is_none()) {
            track_cache(progress, &ev, true);
            lock.log_cache(&ev);
        }
        for line in &lines[1] {
            track_stderr(progress, &String::from_utf8_lossy(line));
        }
        if let Some(recorder) = recorder.as_deref_mut() {
            forward_plain_stdout(&mut stdout, &lines[0]);
            recorder.take(&lines[0], &lines[1]);
        }
        log_lines(Some(&mut *lock), &lines[0], &lines[1]);
        lock.publish(progress);
        lock.flush_log();
    }
    let tails: Vec<Vec<Vec<u8>>> = streams.iter_mut().map(|s| s.take_partial().into_iter().collect()).collect();
    drop(streams);
    if let Some(recorder) = recorder {
        forward_plain_stdout(&mut stdout, &tails[0]);
        recorder.take(&tails[0], &tails[1]);
    }
    log_lines(Some(&mut *lock), &tails[0], &tails[1]);
    for ev in feed.poll(false) {
        track_cache(progress, &ev, true);
        lock.log_cache(&ev);
    }
    let status = match status {
        Some(status) => Ok(status),
        None => child.wait(),
    };
    signals::clear();
    status.map(signals::exit_code).unwrap_or(1)
}

/// The publisher is a scoped thread, always joined before this returns.
fn supervise_plain_publishing(cmd: Command, feed: &mut SupervisionFeed, progress: &mut Progress, lock: &mut BuildLock) -> i32 {
    let (stop, stopped) = mpsc::channel::<()>();
    thread::scope(|scope| {
        let publisher = scope.spawn(move || {
            loop {
                if !matches!(stopped.try_recv(), Err(mpsc::TryRecvError::Empty)) { break; }
                for ev in feed.poll(true) {
                    track_cache(progress, &ev, true);
                    lock.log_cache(&ev);
                }
                lock.publish(progress);
                lock.flush_log();
                match stopped.recv_timeout(PUBLISH_TICK) {
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    _ => break,
                }
            }
            for ev in feed.poll(false) {
                track_cache(progress, &ev, true);
                lock.log_cache(&ev);
            }
        });
        let code = supervise_plain(cmd);
        drop(stop);
        let _ = publisher.join();
        code
    })
}

fn report_joined(joined: &Joined, style: Option<Style>, stats: Option<&RunStats>) {
    let reused = match stats {
        Some(stats) => Some(stats.fresh + stats.cached),
        None => joined.identical.then_some(joined.build.units).filter(|&n| n > 0),
    };
    let text = joined_text(joined, reused);
    // Following a build live is sharing it; starting from one that finished is joining it.
    let verb = if joined.followed { "shared" } else { "joined" };
    match style {
        Some(Style::Plain) => eprintln!("{verb:<10}{text}"),
        Some(Style::Ansi) => eprintln!("  {}", term::Paint::new(term::colour_enabled()).dim(&format!("{verb} {text}"))),
        None => eprintln!("frust: {verb} {text}"),
    }
}

fn joined_text(joined: &Joined, reused: Option<u32>) -> String {
    let mut text = format!("build of {}", joined.build.label());
    if joined.identical {
        text.push_str(" (identical command)");
    }
    if joined.followed {
        text.push_str(", compiled once for both");
    } else if let Some(n) = reused {
        text.push_str(&format!(", {n} {} reused", if n == 1 { "unit" } else { "units" }));
    }
    text
}

/// The display knows only `WaitInfo`, so the holder's user and progress travel in the command text.
fn display_wait_info(waiting: &Waiting) -> WaitInfo {
    let mut info = waiting.info.clone();
    if let (Some(owner), Some(holder)) = (info.owner.as_mut(), &waiting.holder) {
        if !holder.user.is_empty() {
            owner.command = format!("{}: {}", holder.user, owner.command);
        }
        if let Some(progress) = &waiting.progress {
            owner.command.push_str(&format!(", {}", progress.summary()));
        }
    }
    info
}

pub fn describe_wait(waiting: &Waiting) -> String {
    let mut text = String::from("waiting for build lock");
    if let Some(holder) = &waiting.holder {
        let held = fsx_since(holder.owner.since_unix);
        text.push_str(&format!(" held by {} (`{}`, {}s)", holder.label(), holder.owner.command, held));
        if let Some(progress) = &waiting.progress {
            text.push_str(&format!(": {}", progress.summary()));
        }
    }
    if waiting.info.ahead > 0 {
        text.push_str(&format!(", {} ahead in queue", waiting.info.ahead));
    }
    text
}

fn fsx_since(unix: u64) -> u64 {
    crate::fsx::unix_now().saturating_sub(unix)
}

pub fn tool_main(tool: &str, args: Vec<OsString>) -> i32 {
    match resolve_tool(tool) {
        Some(path) => exec(&path, &args, &[]),
        None => {
            eprintln!("frust: cannot find `{tool}` on PATH");
            127
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broker_lease_bypasses_cache_off_sync_and_foreign_wrappers() {
        let root = crate::fsx::temp_sibling(&env::temp_dir().join("frust-broker-bypass"));
        let dirs = Dirs::at(&root);
        let feed = prepare_phase_feed(&dirs, &mut Command::new("cargo")).unwrap();
        let wrapper = vec![(os("RUSTC_WRAPPER"), os("frust-wrap"))];
        let mut cfg = Config::from_env();
        for (cache, synchronous, own_wrapper) in [
            (false, false, true),
            (true, true, true),
            (true, false, false),
        ] {
            cfg.cache = cache;
            cfg.store_sync = synchronous;
            let mut cmd = Command::new("cargo");
            cmd.env(crate::store_broker::CAPABILITY_ENV, "inherited-capability");
            let env_vars = if own_wrapper { wrapper.as_slice() } else { &[] };
            assert!(prepare_store_broker(&cfg, &dirs, env_vars, Some(&feed), &mut cmd).is_none());
            assert!(
                cmd.get_envs()
                    .any(|(key, value)| key == crate::store_broker::CAPABILITY_ENV
                        && value.is_none())
            );
        }
        assert_eq!(fs::read_dir(dirs.tmp()).unwrap().count(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn phase_feed_creation_preserves_collisions_and_disables_observation_on_failure() {
        let root = crate::fsx::temp_sibling(&env::temp_dir().join("frust-feed-create"));
        let dirs = Dirs::at(root.clone());
        fs::create_dir_all(dirs.tmp()).unwrap();
        let collision = dirs.tmp().join("existing-feed");
        fs::write(&collision, b"preserve old events\n").unwrap();
        let fresh = dirs.tmp().join("new-feed");
        let mut attempts = 0;
        let mut feed = create_phase_feed(&dirs, || {
            attempts += 1;
            if attempts == 1 { collision.clone() } else { fresh.clone() }
        }).unwrap();
        assert_eq!(attempts, 2);
        assert_eq!(feed.path(), fresh);
        assert!(feed.poll().is_empty());
        assert_eq!(fs::read(&collision).unwrap(), b"preserve old events\n");
        attempts = 0;
        let failed = create_phase_feed(&dirs, || { attempts += 1; collision.clone() });
        assert_eq!(failed.err().unwrap().kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(attempts, 8);
        assert_eq!(fs::read(&collision).unwrap(), b"preserve old events\n");
        let blocked = Dirs::at(root.join("blocked-home"));
        fs::create_dir(blocked.root()).unwrap();
        fs::write(blocked.tmp(), b"preserve obstruction").unwrap();
        let mut command = Command::new("unused-cargo");
        command.env("FRUST_EVENTS", &collision);
        command.env(crate::store_broker::CAPABILITY_ENV, "inherited-capability");
        let feed = prepare_phase_feed(&blocked, &mut command);
        assert!(feed.is_none());
        assert!(command.get_envs().any(|(key, value)| key == "FRUST_EVENTS" && value.is_none()));
        let mut cfg = Config::from_env();
        cfg.cache = true;
        cfg.store_sync = false;
        let wrapper = [(os("RUSTC_WRAPPER"), os("frust-wrap"))];
        let broker = prepare_store_broker(&cfg, &blocked, &wrapper, feed.as_ref(), &mut command);
        assert!(broker.is_none());
        assert!(command.get_envs().any(|(key, value)| key == crate::store_broker::CAPABILITY_ENV && value.is_none()));
        let mut feed = SupervisionFeed { feed, broker };
        assert!(feed.poll(true).is_empty());
        assert_eq!(fs::read(blocked.tmp()).unwrap(), b"preserve obstruction");
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn only_two_active_private_demands_activate_and_publisher_returns_lease_ownership() {
        let root = crate::fsx::temp_sibling(&env::temp_dir().join("frust-lazy-feed"));
        fs::create_dir(&root).unwrap();
        let dirs = Dirs::at(root.join("home"));
        let event = |result| Event::Cache {
            t: 0, result, krate: "fixture".into(), version: "1".into(), compile_ms: 1, saved_ms: 0, bytes: 1,
        };
        fs::create_dir_all(dirs.tmp()).unwrap();
        let stale = dirs.tmp().join(format!("feed-{}.jsonl", std::process::id()));
        let stale_miss = format!("{}\n", serde_json::to_string(&event(CacheResult::Miss)).unwrap());
        let demand = "{\"type\":\"store_demand\"}\n";
        let stale_contents = format!("{stale_miss}{demand}{demand}");
        fs::write(&stale, &stale_contents).unwrap();
        assert_eq!(Feed::new(stale.clone()).poll().len(), 1);
        let mut command = Command::new("unused-cargo");
        let fresh = prepare_phase_feed(&dirs, &mut command).unwrap();
        let path = fresh.path().to_owned();
        let mut cfg = Config::from_env();
        cfg.cache = true;
        cfg.store_sync = false;
        let wrapper = [(os("RUSTC_WRAPPER"), os("frust-wrap"))];
        let broker = prepare_store_broker(&cfg, &dirs, &wrapper, Some(&fresh), &mut command).unwrap();
        let encoded = command.get_envs().find(|(key, _)| *key == crate::store_broker::CAPABILITY_ENV).unwrap().1.unwrap();
        let cap: serde_json::Value = serde_json::from_str(encoded.to_str().unwrap()).unwrap();
        let run = PathBuf::from(cap["run"].as_str().unwrap());
        let mut feed = SupervisionFeed { feed: Some(fresh), broker: Some(broker) };
        let append = |event: Event| {
            let mut file = fs::OpenOptions::new().create(true).append(true).open(&path).unwrap();
            writeln!(file, "{}", serde_json::to_string(&event).unwrap()).unwrap();
        };
        assert_ne!(path, stale);
        assert!(feed.poll(true).is_empty());
        fs::OpenOptions::new().append(true).open(&stale).unwrap().write_all(demand.as_bytes()).unwrap();
        assert!(feed.poll(true).is_empty());
        append(event(CacheResult::Hit));
        assert_eq!(feed.poll(true).len(), 1);
        append(event(CacheResult::Miss));
        assert_eq!(feed.poll(true).len(), 1);
        assert!(!run.exists());
        let request = || fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(demand.as_bytes()).unwrap();
        request();
        request();
        assert!(feed.poll(false).is_empty());
        assert!(feed.poll(true).is_empty());
        assert!(!run.exists());
        request();
        assert!(feed.poll(true).is_empty());
        assert!(!run.exists());
        append(event(CacheResult::Miss));
        assert_eq!(feed.poll(true).len(), 1);
        assert!(!run.exists());
        request();
        let feed = thread::scope(|scope| scope.spawn(move || {
            assert!(feed.poll(true).is_empty());
            assert!(run.is_dir());
            feed
        }).join().unwrap());
        feed.finish();
        assert_eq!(fs::read_to_string(&stale).unwrap(), format!("{stale_contents}{demand}"));
        assert_eq!(fs::read_dir(dirs.tmp()).unwrap().count(), 2);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn unobserved_phase_clears_capability_and_preserves_external_events() {
        let root = crate::fsx::temp_sibling(&env::temp_dir().join("frust-unobserved-feed"));
        fs::create_dir(&root).unwrap();
        let external = root.join("external-events.jsonl");
        fs::write(&external, b"preserve external events\n").unwrap();
        let dirs = Dirs::at(root.join("unobserved-home"));
        let mut cfg = Config::from_env();
        cfg.cache = true;
        cfg.store_sync = false;
        let wrapper = [(os("RUSTC_WRAPPER"), os("frust-wrap"))];
        let mut command = Command::new("unused-cargo");
        command.env("FRUST_EVENTS", &external).env(crate::store_broker::CAPABILITY_ENV, "inherited-capability");
        let broker = prepare_store_broker(&cfg, &dirs, &wrapper, None, &mut command);
        assert!(broker.is_none());
        assert!(command.get_envs().any(|(key, value)| key == crate::store_broker::CAPABILITY_ENV && value.is_none()));
        assert!(command.get_envs().any(|(key, value)| key == "FRUST_EVENTS" && value == Some(external.as_os_str())));
        let mut feed = SupervisionFeed { feed: None, broker };
        assert!(feed.poll(true).is_empty());
        feed.finish();
        assert!(!dirs.root().exists());
        assert_eq!(fs::read(&external).unwrap(), b"preserve external events\n");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn fast_build_does_not_launch_a_planner_before_the_tick_or_after_exit() {
        let mut gate = PlanningGate { pending: true };
        let mut launches = 0;
        for elapsed in [Duration::ZERO, Duration::from_millis(20), TICK - Duration::from_nanos(1)] {
            assert!(gate.start_if_due(elapsed, true, || {
                launches += 1;
                Some(())
            }).is_none());
        }
        for elapsed in [TICK, TICK * 10] {
            assert!(gate.start_if_due(elapsed, false, || {
                launches += 1;
                Some(())
            }).is_none());
        }
        assert_eq!(launches, 0, "the production launch callback never runs for a completed short build");
    }

    #[test]
    fn quiet_build_launches_once_at_the_tick_and_failed_launches_do_not_repeat() {
        for succeeds in [false, true] {
            let mut gate = PlanningGate { pending: true };
            let mut launches = 0;
            assert_eq!(gate.start_if_due(TICK, true, || {
                launches += 1;
                succeeds.then_some(7)
            }), succeeds.then_some(7));
            assert!(gate.start_if_due(TICK * 2, true, || {
                launches += 1;
                Some(9)
            }).is_none());
            assert_eq!(launches, 1);
        }
        let mut unsupported = PlanningGate { pending: false };
        assert!(unsupported.start_if_due::<()>(TICK * 10, true, || panic!("unsupported command launched planning")).is_none());
    }

    fn pairs(list: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        list.iter().map(|(k, v)| (os(k), os(v))).collect()
    }

    #[test]
    fn fingerprint_ignores_display_settings_but_not_build_settings() {
        let parts = [OsStr::new("/bin/cargo"), OsStr::new("id"), OsStr::new("/w"), OsStr::new("/t")];
        let args = vec![os("build"), os("--release")];
        let base = fingerprint(&parts, &args, &[], pairs(&[("CARGO_HOME", "/h"), ("PATH", "/a")]));
        let display = pairs(&[("CARGO_HOME", "/h"), ("PATH", "/b"), ("TERM", "x"), ("CARGO_TERM_COLOR", "always"), ("FRUST_RENDER", "plain")]);
        assert_eq!(fingerprint(&parts, &args, &[], display), base, "display-only variables do not matter");
        let flags = pairs(&[("CARGO_HOME", "/h"), ("RUSTFLAGS", "-Ctarget-cpu=native")]);
        assert_ne!(fingerprint(&parts, &args, &[], flags), base);
        assert_ne!(fingerprint(&parts, &[os("build")], &[], pairs(&[("CARGO_HOME", "/h")])), base);
        let other_dir = [OsStr::new("/bin/cargo"), OsStr::new("id"), OsStr::new("/w2"), OsStr::new("/t")];
        assert_ne!(fingerprint(&other_dir, &args, &[], pairs(&[("CARGO_HOME", "/h")])), base);
        // Length-prefixed fields: moving bytes between arguments changes the identity.
        assert_ne!(
            fingerprint(&parts, &[os("ab"), os("c")], &[], Vec::new()),
            fingerprint(&parts, &[os("a"), os("bc")], &[], Vec::new())
        );
    }

    #[test]
    fn tracks_units_and_active_crates() {
        let mut p = Progress::default();
        track_stderr(&mut p, "\x1b[1m\x1b[32m   Compiling\x1b[0m serde-json v1.0.0");
        track_stderr(&mut p, "   Compiling app v0.1.0 (/w)");
        track_stderr(&mut p, "warning: unused");
        assert_eq!(p.active, vec!["serde-json".to_string(), "app".to_string()]);
        track_json(&mut p, &serde_json::json!({"reason": "compiler-artifact", "package_id": "registry+https://x#serde-json@1.0.0", "target": {"name": "serde_json"}, "fresh": false}));
        track_json(&mut p, &serde_json::json!({"reason": "compiler-artifact", "package_id": "path+file:///w/app#0.1.0", "target": {"name": "app"}, "fresh": true}));
        track_json(&mut p, &serde_json::json!({"reason": "build-finished", "success": true}));
        assert_eq!((p.done, p.fresh), (2, 1));
        assert!(p.active.is_empty());
        assert_eq!(package_name("serde 1.0.0 (registry+https://x)"), "serde");

        // gate: build script compiled, then run, then the library; active throughout.
        track_stderr(&mut p, "   Compiling gate v1.0.0");
        track_stderr(&mut p, "   Compiling gate v1.0.0 (build script)");
        track_json(&mut p, &serde_json::json!({"reason": "compiler-artifact", "package_id": "registry+https://x#gate@1.0.0", "target": {"name": "build-script-build", "kind": ["custom-build"]}, "fresh": false}));
        assert_eq!(p.active, vec!["gate".to_string()]);
        track_json(&mut p, &serde_json::json!({"reason": "compiler-artifact", "package_id": "registry+https://x#gate@1.0.0", "target": {"name": "gate", "kind": ["lib"]}, "fresh": false}));
        assert!(p.active.is_empty());
    }

    #[test]
    fn streams_split_lines_across_reads_and_flush_the_tail() {
        let (reader, mut writer) = io::pipe().unwrap();
        let mut stream = Stream::new(Some(sys::Pipe::from(reader)));
        let mut chunk = vec![0u8; 4];
        let mut lines = Vec::new();
        writer.write_all(b"ab\ncdefg\nhi").unwrap();
        drop(writer);
        while stream.file.is_some() {
            stream.read_lines(&mut chunk, &mut lines);
        }
        assert_eq!(lines, vec![b"ab".to_vec(), b"cdefg".to_vec(), b"hi".to_vec()]);
    }

    #[test]
    fn target_dir_setting_reads_toml_values() {
        assert_eq!(target_dir_setting("[build]\ntarget-dir = \"/x/y\"\n").as_deref(), Some("/x/y"));
        assert_eq!(target_dir_setting("build.target-dir='rel'").as_deref(), Some("rel"));
        assert_eq!(target_dir_setting("# target-dir = \"no\"\n"), None);
        assert_eq!(config_relative(Path::new("/w/m/.cargo/config.toml"), "out"), PathBuf::from("/w/m/out"));
    }

    #[test]
    fn profile_debug_detection_ignores_unrelated_profile_settings() {
        let dev = ["dev"];
        assert!(!sets_profile_debug("[profile.release]\nlto = true\ndebug = 1\n", &dev), "release does not block dev");
        assert!(!sets_profile_debug("[profile.dev]\nopt-level = 1\n", &dev));
        assert!(!sets_profile_debug("[profile.dev.package.foo]\nopt-level = 3\n", &dev));
        assert!(!sets_profile_debug("[profile.dev.package.\"*\"]\nopt-level = 3\n", &dev));
        assert!(!sets_profile_debug("[workspace]\nprofile.dev = 1\n", &dev));
        assert!(!sets_profile_debug("# [profile.dev]\n# debug = 1\n[package]\nname = \"profiler\"\n", &dev));
        assert!(sets_profile_debug("[profile.dev]\ndebug = 1\n", &dev));
        assert!(sets_profile_debug("[ profile.dev ]\nstrip = \"symbols\"\n", &dev));
        assert!(sets_profile_debug("[profile.dev]\nsplit-debuginfo = \"unpacked\"\n", &dev));
        assert!(sets_profile_debug("profile.dev.debug=0", &dev));
        assert!(sets_profile_debug("profile.dev.package.\"*\".debug = 1\n\n[workspace]\n", &dev));
        assert!(sets_profile_debug("[profile.dev.package.foo]\ndebug = 2\n", &dev));
        assert!(sets_profile_debug("[profile.dev.build-override]\ndebug = true\n", &dev));
        assert!(!sets_profile_debug("[profile.dev.build-override]\nopt-level = 3\n", &dev));
        assert!(sets_profile_debug("profile = { dev = { debug = 1 } }\n", &dev));
        assert!(sets_profile_debug("[profile]\ndev = { package = { \"*\" = { debug = 1 } } }\n", &dev));
        assert!(!sets_profile_debug("profile = { dev = { opt-level = 1, package = { foo = { opt-level = 2 } } } }\n", &dev));
        assert!(!sets_profile_debug("[profile.test]\ndebug = 1\n", &dev), "test only matters for test builds");
        assert!(sets_profile_debug("[profile.test]\ndebug = 1\n", &["dev", "test"]));
    }

    #[test]
    fn profile_environment_variables_block_only_debug_keys() {
        let var = |k: &str| vec![(OsString::from(k), OsString::from("1"))];
        assert!(env_sets_profile_debug(var("CARGO_PROFILE_DEV_DEBUG"), &["dev"]));
        assert!(env_sets_profile_debug(var("CARGO_PROFILE_DEV_STRIP"), &["dev"]));
        assert!(env_sets_profile_debug(var("CARGO_PROFILE_DEV_BUILD_OVERRIDE_DEBUG"), &["dev"]));
        assert!(!env_sets_profile_debug(var("CARGO_PROFILE_DEV_OPT_LEVEL"), &["dev"]));
        assert!(!env_sets_profile_debug(var("CARGO_PROFILE_RELEASE_DEBUG"), &["dev"]));
        assert!(!env_sets_profile_debug(var("CARGO_PROFILE_TEST_DEBUG"), &["dev"]));
        assert!(env_sets_profile_debug(var("CARGO_PROFILE_TEST_DEBUG"), &["dev", "test"]));
    }

    #[test]
    fn only_dev_like_builds_are_tunable() {
        let parse = |args: &[&str]| tunable_profiles(&CargoArgs::parse(args.iter().map(|a| os(a))));
        assert_eq!(parse(&["build"]), Some(vec!["dev"]));
        assert_eq!(parse(&["check", "--profile", "dev"]), Some(vec!["dev"]));
        assert_eq!(parse(&["test"]), Some(vec!["dev", "test"]));
        assert_eq!(parse(&["build", "--release"]), None);
        assert_eq!(parse(&["build", "-r"]), None);
        assert_eq!(parse(&["build", "--profile=custom"]), None);
        assert_eq!(parse(&["bench"]), None);
        assert_eq!(parse(&["install", "x"]), None);
        assert_eq!(parse(&["--release", "build"]), None);
    }

    #[test]
    fn config_files_start_at_the_working_directory() {
        let files = config_files(Some(Path::new("/w")), Path::new("/w/crates/m"));
        let first = files.iter().position(|f| f == Path::new("/w/crates/m/.cargo/config.toml")).unwrap();
        let root = files.iter().position(|f| f == Path::new("/w/.cargo/config.toml")).unwrap();
        assert!(first < root);
        assert_eq!(files.iter().filter(|f| *f == Path::new("/w/.cargo/config.toml")).count(), 1, "deduplicated");
    }

    #[test]
    fn joined_text_mentions_holder_and_reuse() {
        let build = lock::Finished {
            pid: 7,
            user: "zak".into(),
            command: "cargo build".into(),
            fingerprint: "f".into(),
            success: true,
            units: 3,
            finished_ns: 1,
            code: Some(0),
        };
        let joined = Joined { build, identical: true, followed: false };
        assert_eq!(joined_text(&joined, Some(3)), "build of zak pid 7 (identical command), 3 units reused");
        assert_eq!(joined_text(&Joined { identical: false, ..joined.clone() }, None), "build of zak pid 7");
        assert_eq!(joined_text(&Joined { followed: true, ..joined }, Some(3)), "build of zak pid 7 (identical command), compiled once for both");
    }

    fn scope_of(default_dir: &str, args: &[&str]) -> Option<(String, Vec<String>)> {
        let args: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
        profile_and_targets(default_dir, &args)
    }

    #[test]
    fn profile_and_targets_follow_cargo_directory_rules() {
        let plain = |dir: &str| Some((dir.to_string(), Vec::new()));
        assert_eq!(scope_of("debug", &[]), plain("debug"));
        assert_eq!(scope_of("debug", &["--release"]), plain("release"));
        assert_eq!(scope_of("debug", &["-r", "-p", "a"]), plain("release"));
        assert_eq!(scope_of("debug", &["--profile", "test"]), plain("debug"));
        assert_eq!(scope_of("release", &["--profile=dev"]), plain("debug"));
        assert_eq!(scope_of("debug", &["--profile", "bench"]), plain("release"));
        assert_eq!(scope_of("debug", &["--profile", "fast-ci"]), plain("fast-ci"));
        assert_eq!(scope_of("debug", &["--release", "--profile", "release"]), plain("release"), "agreeing flags are fine");
        assert_eq!(scope_of("debug", &["--release", "--profile", "dev"]), None, "conflicting profiles: cargo decides");
        assert_eq!(scope_of("debug", &["--all-targets", "--features", "x", "--", "--release"]), plain("debug"), "after `--` is not cargo's");
        assert_eq!(
            scope_of("debug", &["--target", "x86_64-unknown-linux-gnu", "--target=aarch64-apple-darwin"]),
            Some(("debug".into(), vec!["aarch64-apple-darwin".into(), "x86_64-unknown-linux-gnu".into()]))
        );
        assert_eq!(scope_of("debug", &["--target", "specs/custom.json"]), None);
        assert_eq!(scope_of("debug", &["--target"]), None);
        assert_eq!(scope_of("debug", &["-Zbuild-std"]), None);
        assert_eq!(scope_of("debug", &["--config", "build.target=\"x\""]), None);
        assert_eq!(scope_of("debug", &["--config=profile.dev.opt-level=1"]), plain("debug"));
        assert_eq!(scope_of("debug", &["--profile", "../x"]), None);
    }

    #[test]
    fn check_moves_a_frust_owned_target_to_its_sibling_and_leaves_a_user_target_alone() {
        let owned = TargetChoice { central: Some(PathBuf::from("/h/targets/app-1/id")), lock_key: PathBuf::from("/h/targets/app-1/id") }.for_check();
        assert_eq!(owned.central.as_deref(), Some(Path::new("/h/targets/app-1/id-check")));
        assert_eq!(owned.lock_key, Path::new("/h/targets/app-1/id-check"));
        let user = TargetChoice { central: None, lock_key: PathBuf::from("/work/target") }.for_check();
        assert!(user.central.is_none());
        assert_eq!(user.lock_key, Path::new("/work/target"));
    }

    #[test]
    fn config_that_moves_outputs_is_detected() {
        assert!(moves_outputs("[build]\ntarget = \"wasm32-unknown-unknown\"\n"));
        assert!(moves_outputs("build.target = 'x'\n"));
        assert!(moves_outputs("[ build ]\nbuild-dir = \"/b\"\n"));
        assert!(moves_outputs("build = { target = \"x\" }\n"));
        assert!(!moves_outputs("[build]\njobs = 4\n[target.x86_64-unknown-linux-gnu]\nlinker = \"cc\"\n"));
        assert!(!moves_outputs("# [build]\n# target = \"x\"\n"));
        assert!(!moves_outputs("[build]\ntarget-dir = \"/t\"\n"), "target-dir chooses the lock directory itself");
    }

    #[test]
    fn shown_units_and_diagnostics_are_not_shown_twice() {
        let mut shown = Shown::default();
        let artifact = |fresh: bool| {
            serde_json::json!({"reason": "compiler-artifact", "package_id": "p#a@1.0.0", "target": {"name": "a"}, "profile": {"test": false}, "features": [], "fresh": fresh})
        };
        assert!(!shown.skip_json(&artifact(true)), "nothing shown yet");
        shown.add_json(&artifact(false));
        assert!(shown.skip_json(&artifact(true)), "a fresh confirmation of a shown unit");
        assert!(!shown.skip_json(&artifact(false)), "a real rebuild is shown");
        let other = serde_json::json!({"reason": "compiler-artifact", "package_id": "p#b@1.0.0", "target": {"name": "b"}, "fresh": true});
        assert!(!shown.skip_json(&other));

        shown.add_line("\x1b[33mwarning\x1b[0m: unused variable");
        shown.add_line("   Compiling a v1.0.0");
        assert!(shown.skip_line("warning: unused variable"), "compared without colour");
        assert!(!shown.skip_line("warning: unused variable"), "only as often as it was shown");
        assert!(!shown.skip_line("   Compiling a v1.0.0"), "status lines always pass");
        assert!(is_status_line("    Finished `dev` profile"));
        assert!(is_status_line("\x1b[1m\x1b[32m   Compiling\x1b[0m x"));
        assert!(!is_status_line("warning: x") && !is_status_line("  --> src/main.rs:1:1") && !is_status_line("   |") && !is_status_line("   = note: x"));
    }
}
