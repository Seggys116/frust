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

use crate::cargo_args::{self, CargoArgs, Class, ExecKind, os};
use crate::config::{Config, RenderMode, TargetMode};
use crate::events::{self, CacheResult, Event, Feed};
use crate::history;
use crate::lock::{self, Acquired, BuildLock, BuildResult, Joined, LockError, LockEvent, Progress, Record, Scope, SharedFailure, WaitInfo, Waiting};
use crate::paths::Dirs;
use crate::signals;
use crate::sys::{self, StdStream};
use crate::toolchain::{Probe, resolve_tool};
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

pub fn cargo_main(args: Vec<OsString>) -> i32 {
    let started = Instant::now();
    let cfg = Config::from_env();
    let dirs = Dirs::discover();
    let parsed = CargoArgs::parse(args);
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

    let cwd = working_dir(&parsed);
    // Both answers are memoised and revalidated each run, so a warm build spawns neither the probe nor `locate-project`.
    let Some((probe, probed)) = Probe::cached(&dirs, parsed.toolchain.as_deref(), &cwd) else {
        return exec(&cargo, &parsed.original(), &[]);
    };
    trace(&cfg, started, &format!("toolchain probed (probe cache {})", probed.label()));
    let manifest = parsed.value_of("--manifest-path");
    let (root, located) = workspace::locate_cached(&dirs, &cwd, parsed.toolchain.as_deref(), manifest.as_deref(), &probe.id);
    trace(&cfg, started, &format!("workspace located (locate cache {})", located.label()));

    let configs = ConfigInputs::new(root.as_deref(), &cwd);
    let mut target = pick_target_dir(&cfg, &dirs, &parsed, root.as_deref(), &probe, &cwd, &configs);
    if class == Class::CacheOnly {
        target.central = None;
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
        _ => {
            let args = session.parsed.assemble(&session.tuning, session.parsed.subcommand.as_deref().unwrap_or(""), &session.parsed.rest);
            exec(&session.cargo, &args, &session.env_vars)
        }
    }
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

struct TargetChoice {
    central: Option<PathBuf>,
    lock_key: PathBuf,
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
fn config_files(root: Option<&Path>, cwd: &Path) -> Vec<PathBuf> {
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

fn defines_profile(text: &str) -> bool {
    let mut top_level = true;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(header) = line.strip_prefix('[') {
            let name = header.trim_start_matches('[').trim_start().trim_start_matches(['"', '\'']);
            if let Some(after) = name.strip_prefix("profile")
                && (after.trim_start().starts_with(['.', ']']) || after.starts_with(['"', '\''])) {
                    return true;
                }
            top_level = false;
            continue;
        }
        if top_level {
            let key = line.trim_start_matches(['"', '\'']);
            if let Some(after) = key.strip_prefix("profile") {
                let after = after.trim_start_matches(['"', '\'']).trim_start();
                if after.starts_with('.') || after.starts_with('=') {
                    return true;
                }
            }
        }
    }
    false
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
    if user_profiles(parsed, root, cwd, configs) {
        return Vec::new();
    }
    vec![os("--config"), os("profile.dev.package.\"*\".debug=0")]
}

fn user_profiles(parsed: &CargoArgs, root: Option<&Path>, cwd: &Path, configs: &ConfigInputs) -> bool {
    let read = |p: &Path| configs.read(p);
    let cli = cli_configs(parsed).iter().any(|value| {
        let text = value.to_string_lossy();
        match cli_config_file(&text, cwd) {
            Some(file) => defines_profile(&read(&file)),
            None => defines_profile(&text),
        }
    });
    cli || configs.files.iter().any(|p| defines_profile(&read(p)))
        || root.is_some_and(|r| defines_profile(&read(&r.join("Cargo.toml"))))
}

fn exec(cargo: &Path, args: &[OsString], env_vars: &[(OsString, OsString)]) -> i32 {
    let mut cmd = Command::new(cargo);
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
        let code = self.build_phase(&phase_sub, &phase_rest);
        if code != 0 || matches!(class, Class::Compile) {
            return code;
        }
        // Our lock is already released, so user code never blocks other builds.
        let args = self.parsed.assemble(&self.tuning, self.sub(), &self.parsed.rest);
        exec(&self.cargo, &args, &self.env_vars)
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
        let default_dir = match sub {
            "build" | "b" | "check" | "c" | "clippy" | "test" | "t" => "debug",
            "bench" => "release",
            // `doc` writes the shared `target/doc`; anything else is unknown territory.
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
        Scope::slots(slots)
    }

    /// `check`, `build`, `test` and `clippy` see different unit counts, so they must not share one record.
    fn history_keys(&self, workspace_key: &str, sub: &str, rest: &[OsString]) -> Vec<String> {
        let mut exact = self.parsed.assemble(&self.tuning, sub, &[]);
        exact.extend(rest.iter().cloned());
        let digest = crate::paths::short_hash(exact.iter().flat_map(|a| a.as_encoded_bytes().iter().copied().chain([0])).collect::<Vec<u8>>());
        vec![format!("{workspace_key}-{sub}-{digest}"), format!("{workspace_key}-{sub}")]
    }

    fn build_phase(&self, sub: &str, rest: &[OsString]) -> i32 {
        let started = Instant::now();
        let style = self.render_style();
        let workspace_name = workspace::display_name(self.root.as_deref());
        let key = self.root.as_deref().map(workspace::workspace_key).unwrap_or_else(|| "_loose".into());
        let history_keys = self.history_keys(&key, sub, rest);
        let expected = history::expected_units(&self.dirs, &history_keys);

        let feed_path = self.dirs.tmp().join(format!("feed-{}.jsonl", std::process::id()));
        let mut feed = Feed::new(feed_path.clone());
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

        let mut cmd = Command::new(&self.cargo);
        cmd.args(&command);
        if style.is_some() {
            cmd.arg("--message-format=json-render-diagnostics");
        }
        cmd.args(rest);
        for (k, v) in &self.env_vars {
            cmd.env(k, v);
        }
        let tracked = style.is_some() || guard.is_some();
        if tracked {
            let _ = fs::create_dir_all(self.dirs.tmp());
            cmd.env("FRUST_EVENTS", &feed_path);
        }

        let mut progress = Progress { expected, ..Progress::default() };
        trace(&self.cfg, started, "lock acquired, starting cargo");
        let code = match (style, ui.as_mut()) {
            (Some(style), Some(ui)) => {
                cmd.env("CARGO_TERM_PROGRESS_WHEN", "never");
                cmd.env("CARGO_TERM_WIDTH", term::terminal_width().to_string());
                if style == Style::Ansi && term::colour_enabled() {
                    cmd.env("CARGO_TERM_COLOR", "always");
                }
                self.supervise_rendered(cmd, ui, &mut feed, &mut progress, guard.as_mut(), &mut joining.shown)
            }
            _ => match guard.as_mut() {
                Some(lock) => supervise_plain_logged(cmd, &mut feed, &mut progress, lock),
                None => supervise_plain(cmd),
            },
        };
        trace(&self.cfg, started, "cargo finished");

        // Hand the lock on before the summary so waiters start at once; a signalled build is not a result to share.
        if let Some(guard) = guard.take() {
            let interrupted = signals::received().is_some() || !(0..=128).contains(&code);
            guard.finish(BuildResult { code, units: progress.done, interrupted });
        }
        let stats = ui.as_mut().map(|ui| ui.finish(Outcome { success: code == 0, exit_code: code }, Instant::now()));
        if tracked {
            let _ = fs::remove_file(&feed_path);
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
        // After the summary and the lock hand-off, so waiters never pay for it; only touches directories under the frust targets dir.
        if code == 0 && self.lock_target.starts_with(self.dirs.targets()) {
            let report = crate::compact::auto_compact(&self.dirs, &self.lock_target, &self.cfg);
            if self.cfg.debug {
                eprintln!("frust: auto-compact {report:?}");
            }
        }
        if code == 0 {
            crate::update::notice(&self.dirs);
        }
        code
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
    fn supervise_rendered(
        &self,
        mut cmd: Command,
        ui: &mut Ui,
        feed: &mut Feed,
        progress: &mut Progress,
        mut lock: Option<&mut BuildLock>,
        shown: &mut Shown,
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
        let mut planning_due = crate::unit_graph::supported(sub);
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
            // Only a still-running build needs planning after the first display tick.
            if planning_due && status.is_none() && child_started.elapsed() >= TICK {
                planning_due = false;
                planning = crate::unit_graph::Pending::start(&self.cargo, &planning_args, &[], &self.env_vars);
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
            for ev in feed.poll() {
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
        log_lines(lock.as_deref_mut(), &tails[0], &tails[1]);
        render_lines(ui, progress, shown, &tails[0], &tails[1], now);
        for ev in feed.poll() {
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
fn supervise_plain_logged(cmd: Command, feed: &mut Feed, progress: &mut Progress, lock: &mut BuildLock) -> i32 {
    let capture_out = !io::stdout().is_terminal();
    let capture_err = !io::stderr().is_terminal();
    lock.log_start(false, capture_out && capture_err);
    if capture_out || capture_err {
        supervise_tee(cmd, feed, progress, lock, capture_out, capture_err)
    } else {
        supervise_plain_publishing(cmd, feed, progress, lock)
    }
}

fn supervise_tee(mut cmd: Command, feed: &mut Feed, progress: &mut Progress, lock: &mut BuildLock, capture_out: bool, capture_err: bool) -> i32 {
    cmd.stdin(Stdio::inherit());
    cmd.stdout(if capture_out { Stdio::piped() } else { Stdio::inherit() });
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
            streams[0].read_tee(&mut chunk, &mut lines[0], &mut stdout);
        }
        if ready[1] {
            streams[1].read_tee(&mut chunk, &mut lines[1], &mut stderr);
        }
        for ev in feed.poll() {
            track_cache(progress, &ev, true);
            lock.log_cache(&ev);
        }
        for line in &lines[1] {
            track_stderr(progress, &String::from_utf8_lossy(line));
        }
        log_lines(Some(&mut *lock), &lines[0], &lines[1]);
        lock.publish(progress);
        lock.flush_log();
    }
    let tails: Vec<Vec<Vec<u8>>> = streams.iter_mut().map(|s| s.take_partial().into_iter().collect()).collect();
    drop(streams);
    log_lines(Some(&mut *lock), &tails[0], &tails[1]);
    for ev in feed.poll() {
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
fn supervise_plain_publishing(cmd: Command, feed: &mut Feed, progress: &mut Progress, lock: &mut BuildLock) -> i32 {
    let (stop, stopped) = mpsc::channel::<()>();
    thread::scope(|scope| {
        let publisher = scope.spawn(move || {
            loop {
                for ev in feed.poll() {
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
            for ev in feed.poll() {
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
    fn profile_detection_covers_headers_and_dotted_keys() {
        assert!(defines_profile("[profile.dev]\nopt-level = 1\n"));
        assert!(defines_profile("[ profile.dev ]\n"));
        assert!(defines_profile("[[ profile.x ]]\n"));
        assert!(defines_profile("profile.dev.package.\"*\".debug = 1\n\n[workspace]\n"));
        assert!(defines_profile("profile = { dev = { debug = 1 } }\n"));
        assert!(defines_profile("profile.dev.debug=0"));
        assert!(!defines_profile("[workspace]\nprofile.dev = 1\n"), "a key inside another table is not a profile");
        assert!(!defines_profile("# [profile.dev]\n[package]\nname = \"profiler\"\n"));
        assert!(!defines_profile("[profiles]\n"));
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
