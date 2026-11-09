//! The recorded unit graph: each rustc call with its inputs, outputs, edges and what cargo printed for it.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::journal::Record;
use crate::cache::depinfo;
use crate::cache::hashing::Stamp;
use crate::cache::rustc_args::profile_root;
use crate::sys;

pub const FORMAT: u32 = 2;
const MAX_PLAN_BYTES: u64 = 64 * 1024 * 1024;
const MAX_TREE: usize = 20_000;

pub type Packed = [i64; 5];
pub type EnvDeps = Vec<(String, Option<String>)>;
pub type Layout = Vec<(String, bool, Vec<String>)>;

/// Never equal to a real stamp: the input changed while it was being read, so its unit stays dirty.
const POISON: Packed = [-1; 5];

pub fn stamp(path: &Path) -> Option<Packed> {
    let s = Stamp::of(&sys::stat(path).ok()?);
    Some([s.dev as i64, s.ino as i64, s.size as i64, s.mtime_ns as i64, s.ctime_ns as i64])
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    /// `None` records an absent path.
    pub stamp: Option<Packed>,
}

impl Entry {
    pub fn now(path: &Path) -> Entry {
        Entry { path: path.to_string_lossy().into_owned(), stamp: stamp(path) }
    }

    /// An input changed since `since_ns` (or missing, when it must exist) is recorded poisoned.
    fn input(path: &Path, since_ns: i128, absent_ok: bool) -> Entry {
        let mut entry = Entry::now(path);
        let fresh = |s: &Packed| {
            let stamp = Stamp { dev: 0, ino: 0, size: 0, mtime_ns: i128::from(s[3]), ctime_ns: i128::from(s[4]) };
            stamp.too_new_for(since_ns)
        };
        match &entry.stamp {
            Some(s) if fresh(s) => entry.stamp = Some(POISON),
            None if !absent_ok => entry.stamp = Some(POISON),
            _ => {}
        }
        entry
    }

    pub fn current(&self) -> bool {
        stamp(Path::new(&self.path)) == self.stamp
    }

    /// An output is the same file with the same contents; its change time moves when the cache links it into an entry.
    pub fn current_output(&self) -> bool {
        match (stamp(Path::new(&self.path)), self.stamp) {
            (Some(now), Some(then)) => now[..4] == then[..4],
            (now, then) => now == then,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Script {
    pub package_id: String,
    pub profile: String,
    pub package_dir: String,
    /// Absolute `rerun-if-changed` paths, which cargo also lists in the uplifted dep-info files.
    pub rerun: Vec<String>,
    pub triggers: Vec<Entry>,
    pub env: Vec<(String, Option<String>)>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Unit {
    pub id: String,
    pub package_id: String,
    pub package: String,
    pub pkg_name: String,
    pub desc: String,
    pub fix: Option<String>,
    pub local: bool,
    pub script: bool,
    pub argv: Vec<String>,
    pub cwd: String,
    pub env_set: Vec<(String, String)>,
    pub env_unset: Vec<String>,
    pub outputs: Vec<String>,
    pub inputs: Vec<Entry>,
    pub env_deps: Vec<(String, Option<String>)>,
    pub stamps: Vec<Entry>,
    pub deps: Vec<(usize, bool)>,
    pub uplifts: Vec<(String, String)>,
    pub depfiles: Vec<(String, String)>,
    pub depfiles_ok: bool,
    pub fingerprint: String,
    pub diagnostics: Vec<String>,
    pub run: Option<usize>,
    pub script_unit: Option<usize>,
    pub incremental: bool,
    pub ms: u64,
}

impl Unit {
    pub fn argv(&self) -> Option<Argv> {
        parse_argv(&self.argv, Path::new(&self.cwd))
    }

    pub fn checks(&self) -> bool {
        self.argv().is_some_and(|a| !a.emit.iter().any(|e| e == "link"))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Plan {
    pub format: u32,
    pub key: String,
    pub cwd: String,
    pub target: String,
    pub finished: String,
    pub graph: Vec<Entry>,
    pub layout: Layout,
    pub scripts: Vec<Script>,
    pub units: Vec<Unit>,
    /// Cargo's JSON messages in order; artifacts name their unit.
    pub messages: Vec<(Option<usize>, String)>,
    /// Variables cargo set identically for every unit, kept once.
    pub env_common: Vec<(String, String)>,
}

impl Plan {
    /// Everything cargo set for the unit's call.
    pub fn env_of(&self, unit: &Unit) -> Vec<(String, String)> {
        let mut env = self.env_common.clone();
        env.extend(unit.env_set.iter().cloned());
        env.sort();
        env
    }

    fn intern_env(&mut self) {
        let Some(first) = self.units.first() else { return };
        let common: Vec<(String, String)> = first.env_set.iter().filter(|pair| self.units.iter().all(|u| u.env_set.contains(pair))).cloned().collect();
        for unit in &mut self.units {
            unit.env_set.retain(|pair| !common.contains(pair));
        }
        self.env_common.extend(common);
    }
}

pub fn load(path: &Path) -> Option<Plan> {
    let meta = fs::metadata(path).ok()?;
    if meta.len() > MAX_PLAN_BYTES {
        return None;
    }
    let plan: Plan = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    let sound = plan.format == FORMAT
        && plan.units.iter().all(|u| u.deps.iter().all(|(d, _)| *d < plan.units.len()) && u.run.is_none_or(|r| r < plan.scripts.len()))
        && plan.messages.iter().all(|(u, _)| u.is_none_or(|u| u < plan.units.len()));
    sound.then_some(plan)
}

pub fn save(path: &Path, plan: &Plan) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    crate::fsx::write_atomic(path, &serde_json::to_vec(plan).map_err(io::Error::other)?)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Argv {
    pub crate_name: String,
    pub extra: String,
    pub out_dir: PathBuf,
    pub types: Vec<String>,
    pub test: bool,
    pub emit: Vec<String>,
    pub externs: Vec<PathBuf>,
    pub incremental: bool,
    pub packed: bool,
}

/// `None` for anything the executor cannot reproduce exactly: argument files, explicit output paths, non-UTF-8.
pub fn parse_argv(argv: &[String], cwd: &Path) -> Option<Argv> {
    let mut a = Argv::default();
    let mut i = 1;
    let value = |i: &mut usize, arg: &str, flag: &str| -> Option<Option<String>> {
        if arg == flag {
            *i += 1;
            return Some(Some(argv.get(*i)?.clone()));
        }
        Some(arg.strip_prefix(flag).and_then(|r| r.strip_prefix('=')).map(str::to_string))
    };
    while i < argv.len() {
        let arg = argv[i].as_str();
        if arg.starts_with('@') || arg.starts_with("-o") {
            return None;
        }
        if let Some(v) = value(&mut i, arg, "--crate-name")? {
            a.crate_name = v;
        } else if let Some(v) = value(&mut i, arg, "--crate-type")? {
            a.types.extend(v.split(',').map(str::to_string));
        } else if let Some(v) = value(&mut i, arg, "--emit")? {
            for kind in v.split(',') {
                if kind.contains('=') {
                    return None;
                }
                a.emit.push(kind.to_string());
            }
        } else if let Some(v) = value(&mut i, arg, "--out-dir")? {
            a.out_dir = cwd.join(v);
        } else if let Some(v) = value(&mut i, arg, "--extern")? {
            if let Some((_, path)) = v.split_once('=') {
                a.externs.push(cwd.join(path));
            }
        } else if arg == "--test" {
            a.test = true;
        } else if let Some(v) = value(&mut i, arg, "--codegen")?.or(value(&mut i, arg, "-C")?).or(arg.strip_prefix("-C").filter(|r| !r.is_empty()).map(str::to_string)) {
            match v.split_once('=') {
                Some(("extra-filename", x)) => a.extra = x.to_string(),
                Some(("incremental", _)) => a.incremental = true,
                Some(("split-debuginfo", "packed")) => a.packed = true,
                _ => {}
            }
        }
        i += 1;
    }
    if a.crate_name.is_empty() || a.out_dir.as_os_str().is_empty() {
        return None;
    }
    if a.emit.is_empty() {
        a.emit.push("link".into());
    }
    if a.types.is_empty() {
        a.types.push(if a.test { "bin".into() } else { "lib".into() });
    }
    Some(a)
}

/// The files rustc writes into `--out-dir`, named as rustc names them on this platform.
pub fn outputs(a: &Argv) -> Vec<PathBuf> {
    let base = format!("{}{}", a.crate_name, a.extra);
    let mut out = Vec::new();
    for emit in &a.emit {
        match emit.as_str() {
            "dep-info" => out.push(a.out_dir.join(format!("{base}.d"))),
            "metadata" => out.push(a.out_dir.join(format!("lib{base}.rmeta"))),
            "link" => {
                for kind in &a.types {
                    let name = match kind.as_str() {
                        _ if a.test => format!("{base}{}", std::env::consts::EXE_SUFFIX),
                        "lib" | "rlib" => format!("lib{base}.rlib"),
                        "bin" => format!("{base}{}", std::env::consts::EXE_SUFFIX),
                        "proc-macro" | "dylib" | "cdylib" => format!("{}{base}{}", std::env::consts::DLL_PREFIX, std::env::consts::DLL_SUFFIX),
                        "staticlib" if cfg!(windows) => format!("{base}.lib"),
                        "staticlib" => format!("lib{base}.a"),
                        _ => continue,
                    };
                    let path = a.out_dir.join(name);
                    if !out.contains(&path) {
                        out.push(path);
                    }
                }
            }
            _ => {}
        }
    }
    if cfg!(windows) {
        let pdb = a.out_dir.join(format!("{base}.pdb"));
        if pdb.is_file() {
            out.push(pdb);
        }
    }
    out
}

/// One candidate rustc call for an artifact: from this run's journal, or carried from the previous plan.
struct Candidate {
    argv: Vec<String>,
    cwd: String,
    env_set: Vec<(String, String)>,
    env_unset: Vec<String>,
    t: i64,
    ms: u64,
    parsed: Argv,
    manifest: Option<String>,
    previous: Option<Unit>,
}

/// Why no plan was built, and the fingerprint files of fresh units no journal covers.
#[derive(Debug)]
pub struct Refused {
    pub why: &'static str,
    pub adopt: Vec<PathBuf>,
}

impl From<&'static str> for Refused {
    fn from(why: &'static str) -> Refused {
        Refused { why, adopt: Vec::new() }
    }
}

pub struct Recorded<'a> {
    pub key: &'a str,
    pub cwd: &'a Path,
    pub target: &'a Path,
    pub root: &'a Path,
    pub messages: Vec<serde_json::Value>,
    pub finished: String,
    pub journal: Vec<Record>,
    /// Earlier plans whose units may stand in for fresh ones: this command's first, then others of the same target.
    pub previous: Vec<Plan>,
    pub base_env: &'a HashMap<String, String>,
    pub configs: &'a [PathBuf],
    pub launched_ns: i128,
    /// Graph stamps and layout the check just found unchanged under the same lock; cargo never changes them.
    pub verified: Option<(Vec<Entry>, Layout)>,
}

fn delta(env: &[(String, String)], base: &HashMap<String, String>) -> (Vec<(String, String)>, Vec<String>) {
    let mut set = Vec::new();
    for (k, v) in env {
        if base.get(k) != Some(v) {
            set.push((k.clone(), v.clone()));
        }
    }
    let mut unset: Vec<String> = base.keys().filter(|k| !super::journal::per_run(k) && !env.iter().any(|(e, _)| e == *k)).cloned().collect();
    unset.sort();
    (set, unset)
}

fn manifest_dir(msg: &serde_json::Value) -> Option<String> {
    let manifest = msg.get("manifest_path")?.as_str()?;
    Some(Path::new(manifest).parent()?.to_string_lossy().into_owned())
}

fn strings(v: Option<&serde_json::Value>) -> Vec<String> {
    v.and_then(|v| v.as_array()).into_iter().flatten().filter_map(|s| s.as_str().map(str::to_string)).collect()
}

fn describe(msg: &serde_json::Value, local: bool) -> (String, Option<String>) {
    let kind = msg.pointer("/target/kind/0").and_then(|k| k.as_str()).unwrap_or("");
    let name = msg.pointer("/target/name").and_then(|k| k.as_str()).unwrap_or("");
    let test = msg.pointer("/profile/test").and_then(|t| t.as_bool()) == Some(true);
    let named = match kind {
        "lib" | "rlib" | "dylib" | "cdylib" | "staticlib" | "proc-macro" => "lib".to_string(),
        "custom-build" => "build script".to_string(),
        other => format!("{other} \"{name}\""),
    };
    let tests = test && !matches!(kind, "test" | "bench");
    let desc = if tests { format!("{named} test") } else { named.clone() };
    let fix = (local && kind != "custom-build").then(|| {
        let mut args = if named == "lib" { "--lib".to_string() } else { format!("--{named}") };
        if tests {
            args.push_str(" --tests");
        }
        args
    });
    (desc, fix)
}

/// The uplifted name of a hashed output: cargo drops the hash and, for executables, uses the target name.
fn uplift_names(output: &Path, a: &Argv, target_name: &str) -> Vec<String> {
    let Some(name) = output.file_name().and_then(OsStr::to_str) else { return Vec::new() };
    let bare = if a.extra.is_empty() { name.to_string() } else { name.replacen(&a.extra, "", 1) };
    let mut names = vec![bare.clone()];
    if target_name != a.crate_name {
        names.push(bare.replacen(&a.crate_name, target_name, 1));
    }
    names
}

/// Pairs every artifact file with an output of the call, or rejects the call.
fn uplifts_for(msg: &serde_json::Value, a: &Argv, outputs: &[PathBuf]) -> Option<Vec<(String, String)>> {
    let target_name = msg.pointer("/target/name").and_then(|n| n.as_str()).unwrap_or("");
    let mut files = strings(msg.get("filenames"));
    if let Some(exe) = msg.get("executable").and_then(|e| e.as_str()) {
        files.push(exe.to_string());
    }
    let mut pairs = Vec::new();
    for file in files {
        let path = PathBuf::from(&file);
        if outputs.contains(&path) {
            continue;
        }
        let name = path.file_name()?.to_str()?;
        let source = outputs.iter().find(|o| uplift_names(o, a, target_name).iter().any(|n| n == name))?;
        let pair = (source.to_string_lossy().into_owned(), file);
        if !pairs.contains(&pair) {
            pairs.push(pair);
        }
    }
    Some(pairs)
}

/// What names a unit in cargo's artifact message, compared with each candidate call.
struct Identity {
    crate_name: String,
    types: Vec<String>,
    test: bool,
    manifest: Option<String>,
}

impl Identity {
    fn of(msg: &serde_json::Value) -> Identity {
        let mut types = strings(msg.pointer("/target/crate_types"));
        types.sort();
        Identity {
            crate_name: msg.pointer("/target/name").and_then(|n| n.as_str()).unwrap_or("").replace('-', "_"),
            types,
            test: msg.pointer("/profile/test").and_then(|t| t.as_bool()) == Some(true),
            manifest: manifest_dir(msg),
        }
    }

    fn matches(&self, a: &Argv, manifest: Option<&str>) -> bool {
        if a.crate_name != self.crate_name || a.test != self.test || manifest.is_none() || manifest != self.manifest.as_deref() {
            return false;
        }
        let mut mine = a.types.clone();
        mine.sort();
        a.test || mine == self.types
    }
}

pub fn unit_id(a: &Argv) -> String {
    format!("{}|{}{}|{}", a.out_dir.display(), a.crate_name, a.extra, a.test)
}

fn fingerprint_file(msg: &serde_json::Value) -> Option<String> {
    crate::noop::fingerprint_file(msg)
}

/// Builds the plan from a successful cargo run; `Err` names why this run cannot be replayed.
pub fn build(rec: Recorded<'_>) -> Result<Plan, Refused> {
    let mut candidates: Vec<Candidate> = Vec::new();
    for record in rec.journal.iter().rev() {
        if record.code.is_some_and(|c| c != 0) {
            continue;
        }
        let parsed = parse_argv(&record.argv, Path::new(&record.cwd)).ok_or("unreplayable rustc arguments")?;
        if candidates.iter().any(|c| c.previous.is_none() && unit_id(&c.parsed) == unit_id(&parsed)) {
            continue;
        }
        let (env_set, env_unset) = delta(&record.env, rec.base_env);
        let manifest = record.env.iter().find(|(k, _)| k == "CARGO_MANIFEST_DIR").map(|(_, v)| v.clone());
        candidates.push(Candidate { argv: record.argv.clone(), cwd: record.cwd.clone(), env_set, env_unset, t: record.t, ms: record.ms, parsed, manifest, previous: None });
    }
    let fresh_from = candidates.len();
    for previous in &rec.previous {
        for unit in &previous.units {
            let Some(parsed) = unit.argv() else { continue };
            let env_set = previous.env_of(unit);
            candidates.push(Candidate {
                argv: unit.argv.clone(),
                cwd: unit.cwd.clone(),
                manifest: env_set.iter().find(|(k, _)| k == "CARGO_MANIFEST_DIR").map(|(_, v)| v.clone()),
                env_set,
                env_unset: unit.env_unset.clone(),
                t: 0,
                ms: unit.ms,
                parsed,
                previous: Some(unit.clone()),
            });
        }
    }
    let mut units: Vec<Unit> = Vec::new();
    let mut messages = Vec::new();
    let mut scripts_msgs = Vec::new();
    let mut produced: HashMap<PathBuf, usize> = HashMap::new();
    let mut unit_msgs: Vec<serde_json::Value> = Vec::new();
    let mut uncovered: Vec<PathBuf> = Vec::new();
    let mut gaps = 0usize;
    for msg in &rec.messages {
        match msg.get("reason").and_then(|r| r.as_str()) {
            Some("compiler-artifact") => {}
            Some("build-script-executed") => {
                scripts_msgs.push(msg.clone());
                messages.push((None, msg.to_string()));
                continue;
            }
            _ => continue,
        }
        let fresh = msg.get("fresh").and_then(|f| f.as_bool()) == Some(true);
        if strings(msg.get("filenames")).iter().any(|f| f.ends_with(".dSYM")) {
            return Err("packed debug info".into());
        }
        let range = if fresh { 0..candidates.len() } else { 0..fresh_from };
        let identity = Identity::of(msg);
        let mut found = None;
        for ci in range {
            let c = &candidates[ci];
            if !identity.matches(&c.parsed, c.manifest.as_deref()) {
                continue;
            }
            let outs = outputs(&c.parsed);
            if let Some(pairs) = uplifts_for(msg, &c.parsed, &outs) {
                found = Some((ci, outs, pairs));
                break;
            }
        }
        let Some((ci, outs, uplifts)) = found else {
            if !fresh {
                return Err("unrecorded compile".into());
            }
            uncovered.extend(crate::noop::fingerprint_files(msg));
            gaps += 1;
            continue;
        };
        if outs.iter().any(|o| !o.exists()) {
            return Err("unpredicted outputs".into());
        }
        let c = &candidates[ci];
        let a = &c.parsed;
        if a.packed && a.types.iter().any(|t| t != "lib" && t != "rlib") {
            return Err("packed debug info".into());
        }
        let package_id = msg.get("package_id").and_then(|p| p.as_str()).ok_or("unit without package")?.to_string();
        let local = package_id.starts_with("path+");
        let pkg_name = crate::noop::package_name(&package_id);
        let version = c.env_set.iter().find(|(k, _)| k == "CARGO_PKG_VERSION").map(|(_, v)| v.clone()).unwrap_or_default();
        let dir = manifest_dir(msg).unwrap_or_default();
        let package = if local { format!("{pkg_name} v{version} ({dir})") } else { format!("{pkg_name} v{version}") };
        let (desc, fix) = describe(msg, local);
        let profile = profile_root(&a.out_dir);
        let hash = a.extra.trim_start_matches('-');
        let fp_dir = profile.join(".fingerprint").join(format!("{pkg_name}-{hash}"));
        let fp_name = fingerprint_file(msg).ok_or("unknown unit kind")?;
        if hash.is_empty() || !fp_dir.is_dir() {
            return Err("unrecognised fingerprint layout".into());
        }
        let script = msg.pointer("/target/kind/0").and_then(|k| k.as_str()) == Some("custom-build");
        let (mut inputs, env_deps) = match &c.previous {
            Some(prev) => (prev.inputs.clone(), prev.env_deps.clone()),
            None => read_inputs(a, Path::new(&c.cwd), rec.target, i128::from(c.t))?,
        };
        // Cargo never checks registry or git sources for changes either; their units still follow their dependencies.
        if !local {
            inputs.clear();
        }
        let diagnostics = match fs::read_to_string(fp_dir.join(format!("output-{fp_name}"))) {
            Ok(text) => text.lines().map(str::to_string).collect(),
            Err(_) => Vec::new(),
        };
        let index = units.len();
        for out in &outs {
            produced.insert(out.clone(), index);
        }
        messages.push((Some(index), msg.to_string()));
        unit_msgs.push(msg.clone());
        units.push(Unit {
            id: unit_id(a),
            package_id,
            package,
            pkg_name,
            desc,
            fix,
            local,
            script,
            argv: c.argv.clone(),
            cwd: c.cwd.clone(),
            env_set: c.env_set.clone(),
            env_unset: c.env_unset.clone(),
            outputs: outs.iter().map(|o| o.to_string_lossy().into_owned()).collect(),
            inputs,
            env_deps,
            stamps: Vec::new(),
            deps: Vec::new(),
            uplifts,
            depfiles: Vec::new(),
            depfiles_ok: true,
            fingerprint: fp_dir.join(fp_name).to_string_lossy().into_owned(),
            diagnostics,
            run: None,
            script_unit: None,
            incremental: a.incremental,
            ms: c.ms,
        });
    }
    if gaps > 0 {
        return Err(Refused { why: "uncovered unit", adopt: uncovered });
    }
    if units.is_empty() {
        return Err("no units".into());
    }
    for unit in &mut units {
        let a = unit.argv().ok_or("unreplayable rustc arguments")?;
        for ext in &a.externs {
            if let Some(&dep) = produced.get(ext) {
                let rmeta = ext.extension().is_some_and(|e| e == "rmeta");
                unit.deps.push((dep, rmeta));
            }
        }
    }
    let mut scripts = Vec::new();
    for msg in &scripts_msgs {
        scripts.push(script_of(msg, &units, &unit_msgs, rec.base_env, rec.launched_ns, rec.target)?);
    }
    let profiles: Vec<String> = units.iter().map(|u| profile_root(&u.argv().map(|a| a.out_dir).unwrap_or_default()).to_string_lossy().into_owned()).collect();
    let script_units: Vec<usize> = (0..units.len()).filter(|&i| units[i].script).collect();
    for i in 0..units.len() {
        let pid = units[i].package_id.clone();
        units[i].run = scripts.iter().position(|s| s.package_id == pid && s.profile == profiles[i]).filter(|_| !units[i].script);
        units[i].script_unit = script_units.iter().copied().find(|&s| units[s].package_id == pid && profiles[s] == profiles[i]);
    }
    let mut plan = Plan {
        format: FORMAT,
        key: rec.key.to_string(),
        cwd: rec.cwd.to_string_lossy().into_owned(),
        target: rec.target.to_string_lossy().into_owned(),
        finished: rec.finished.clone(),
        graph: Vec::new(),
        layout: Vec::new(),
        scripts,
        units,
        messages,
        env_common: Vec::new(),
    };
    for i in 0..plan.units.len() {
        let mut depfiles = Vec::new();
        let mut ok = true;
        for (_, dest) in &plan.units[i].uplifts {
            let file = Path::new(dest).with_extension("d");
            if !file.is_file() || file == Path::new(dest) {
                continue;
            }
            let expected = depfile_text(&plan, i, dest);
            ok &= fs::read_to_string(&file).is_ok_and(|t| t == expected);
            depfiles.push((dest.clone(), file.to_string_lossy().into_owned()));
        }
        plan.units[i].depfiles = depfiles;
        plan.units[i].depfiles_ok = ok;
        restamp(&mut plan.units[i]);
    }
    graph_inputs(&mut plan, &rec)?;
    plan.intern_env();
    Ok(plan)
}

/// Stamps of everything the unit leaves in the target, checked before a replay trusts it.
pub fn restamp(unit: &mut Unit) {
    let mut stamps: Vec<Entry> = unit.outputs.iter().map(|o| Entry::now(Path::new(o))).collect();
    stamps.extend(unit.uplifts.iter().map(|(_, d)| Entry::now(Path::new(d))));
    stamps.extend(unit.depfiles.iter().map(|(_, f)| Entry::now(Path::new(f))));
    unit.stamps = stamps;
}

pub fn read_inputs(a: &Argv, cwd: &Path, target: &Path, since_ns: i128) -> Result<(Vec<Entry>, EnvDeps), &'static str> {
    let file = a.out_dir.join(format!("{}{}.d", a.crate_name, a.extra));
    let text = fs::read_to_string(&file).map_err(|_| "unreadable dep-info")?;
    let info = depinfo::parse(&text);
    let mut seen = HashSet::new();
    let mut inputs = Vec::new();
    for f in &info.files {
        let path = cwd.join(f);
        if seen.insert(path.clone()) {
            // Build script outputs are written before the unit starts and only change when the script reruns.
            let mut entry = Entry::input(&path, since_ns, false);
            if path.starts_with(target) && entry.stamp == Some(POISON) {
                entry.stamp = stamp(&path).or(Some(POISON));
            }
            inputs.push(entry);
        }
    }
    if inputs.is_empty() {
        return Err("empty dep-info");
    }
    Ok((inputs, info.env))
}

fn script_of(
    msg: &serde_json::Value,
    units: &[Unit],
    unit_msgs: &[serde_json::Value],
    base_env: &HashMap<String, String>,
    launched_ns: i128,
    target: &Path,
) -> Result<Script, &'static str> {
    let package_id = msg.get("package_id").and_then(|p| p.as_str()).ok_or("script without package")?.to_string();
    let out_dir = PathBuf::from(msg.get("out_dir").and_then(|o| o.as_str()).ok_or("script without out_dir")?);
    let run_dir = out_dir.parent().ok_or("script without run directory")?;
    let profile = run_dir.parent().and_then(Path::parent).ok_or("unrecognised build script")?.to_string_lossy().into_owned();
    let at = units.iter().position(|u| u.package_id == package_id).ok_or("build script without package")?;
    let package_dir = PathBuf::from(manifest_dir(&unit_msgs[at]).ok_or("build script without package")?);
    let text = fs::read_to_string(run_dir.join("output")).map_err(|_| "unreadable build script output")?;
    let (files, vars) = crate::noop::build_directives(&text);
    let mut triggers = Vec::new();
    let mut rerun = Vec::new();
    // Registry and git sources never change in place, as the compile cache also assumes; only paths outside them are stamped.
    let immutable = !package_id.starts_with("path+");
    for file in &files {
        let path = package_dir.join(file);
        rerun.push(path.to_string_lossy().into_owned());
        if immutable && path.starts_with(&package_dir) && !path.components().any(|c| c == std::path::Component::ParentDir) {
            continue;
        }
        if path.is_dir() {
            tree(&path, false, target, launched_ns, &mut triggers)?;
        } else {
            triggers.push(Entry::input(&path, launched_ns, true));
        }
    }
    if files.is_empty() && package_id.starts_with("path+") {
        tree(&package_dir, true, target, launched_ns, &mut triggers)?;
    }
    let env = vars.iter().map(|v| (v.clone(), base_env.get(v).cloned())).collect();
    Ok(Script { package_id, profile, package_dir: package_dir.to_string_lossy().into_owned(), rerun, triggers, env })
}

/// Every entry below `top`, symlinked directories followed once; a package walk skips `target`, `.git` and nested packages.
fn tree(top: &Path, package: bool, target: &Path, since_ns: i128, out: &mut Vec<Entry>) -> Result<(), &'static str> {
    let mut stack = vec![(top.to_path_buf(), 0usize)];
    let mut walked = HashSet::new();
    while let Some((dir, depth)) = stack.pop() {
        out.push(Entry::input(&dir, since_ns, false));
        let meta = sys::stat(&dir).map_err(|_| "unreadable directory")?;
        if !walked.insert((meta.dev(), meta.ino())) {
            continue;
        }
        let mut entries: Vec<fs::DirEntry> = fs::read_dir(&dir).map_err(|_| "unreadable directory")?.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            if out.len() > MAX_TREE {
                return Err("tree too large");
            }
            let path = entry.path();
            let kind = entry.file_type().map_err(|_| "unreadable directory")?;
            if !(kind.is_dir() || kind.is_symlink() && path.is_dir()) {
                out.push(Entry::input(&path, since_ns, true));
                continue;
            }
            let name = entry.file_name();
            let skip = name == ".git" || path.starts_with(target) || (package && depth == 0 && name == "target") || (package && path.join("Cargo.toml").exists());
            if !skip {
                stack.push((path, depth + 1));
            }
        }
    }
    Ok(())
}

/// What decides which targets and members cargo finds, compared by name so a save that rewrites a directory is not a change.
pub fn layout_facts(dir: &Path, package: bool) -> Vec<String> {
    let mut facts = Vec::new();
    if package {
        for file in ["build.rs", "src/lib.rs", "src/main.rs"] {
            if dir.join(file).is_file() {
                facts.push(file.to_string());
            }
        }
        for sub in ["src/bin", "examples", "tests", "benches"] {
            let Ok(entries) = fs::read_dir(dir.join(sub)) else { continue };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let path = entry.path();
                if path.is_dir() && path.join("main.rs").is_file() {
                    facts.push(format!("{sub}/{name}/main.rs"));
                } else if name.ends_with(".rs") && path.is_file() {
                    facts.push(format!("{sub}/{name}"));
                }
            }
        }
    } else if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.path().join("Cargo.toml").is_file() {
                facts.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
    }
    facts.sort();
    facts
}

fn graph_inputs(plan: &mut Plan, rec: &Recorded<'_>) -> Result<(), &'static str> {
    if let Some((graph, layout)) = &rec.verified {
        plan.graph = graph.clone();
        plan.layout = layout.clone();
        return Ok(());
    }
    let mut seen = HashSet::new();
    let mut push = |plan: &mut Plan, path: PathBuf, absent_ok: bool| {
        if seen.insert(path.clone()) {
            plan.graph.push(Entry::input(&path, rec.launched_ns, absent_ok));
        }
    };
    push(plan, rec.root.join("Cargo.toml"), false);
    push(plan, rec.root.join("Cargo.lock"), true);
    for config in rec.configs {
        push(plan, config.clone(), true);
    }
    for dir in rec.cwd.ancestors() {
        for name in ["rust-toolchain.toml", "rust-toolchain"] {
            push(plan, dir.join(name), true);
        }
    }
    let mut dirs = BTreeSet::new();
    let mut members = BTreeSet::new();
    let manifests: Vec<Option<PathBuf>> = plan.units.iter().filter(|u| u.local)
        .map(|u| u.env_set.iter().find(|(k, _)| k == "CARGO_MANIFEST_DIR").map(|(_, v)| PathBuf::from(v)))
        .collect();
    for dir in manifests {
        let Some(dir) = dir else {
            return Err("unit without manifest directory");
        };
        push(plan, dir.join("Cargo.toml"), false);
        if dir.starts_with(rec.root) {
            for ancestor in dir.ancestors().skip(1) {
                members.insert(ancestor.to_path_buf());
                if ancestor == rec.root {
                    break;
                }
            }
        }
        dirs.insert(dir);
    }
    for dir in dirs {
        let facts = layout_facts(&dir, true);
        plan.layout.push((dir.to_string_lossy().into_owned(), true, facts));
    }
    for dir in members {
        let facts = layout_facts(&dir, false);
        plan.layout.push((dir.to_string_lossy().into_owned(), false, facts));
    }
    if plan.graph.iter().any(|e| e.stamp == Some(POISON)) {
        return Err("manifest changed during the build");
    }
    Ok(())
}

/// What cargo writes beside an uplifted output: every source of the unit and of its local dependencies, sorted as paths.
pub fn depfile_text(plan: &Plan, unit: usize, dest: &str) -> String {
    let mut files: BTreeSet<PathBuf> = BTreeSet::new();
    let mut stack = vec![unit];
    let mut visited = HashSet::new();
    while let Some(u) = stack.pop() {
        if !visited.insert(u) {
            continue;
        }
        let unit = &plan.units[u];
        files.extend(unit.inputs.iter().map(|e| PathBuf::from(&e.path)));
        if let Some(r) = unit.run {
            files.extend(plan.scripts[r].rerun.iter().map(PathBuf::from));
        }
        if let Some(s) = unit.script_unit {
            stack.push(s);
        }
        for (dep, _) in &unit.deps {
            if plan.units[*dep].local {
                stack.push(*dep);
            }
        }
    }
    let render = |p: &Path| p.to_string_lossy().replace(' ', "\\ ");
    let mut text = format!("{}:", render(Path::new(dest)));
    for file in &files {
        text.push(' ');
        text.push_str(&render(file));
    }
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn argv_parsing_predicts_outputs_and_rejects_what_cannot_replay() {
        let argv = args(&[
            "rustc", "--crate-name", "applib", "--edition=2021", "applib/src/lib.rs", "--crate-type", "lib",
            "--emit=dep-info,metadata,link", "-C", "extra-filename=-a4f8", "--out-dir", "/t/debug/deps", "-C", "incremental=/t/i",
            "--extern", "dep=/t/debug/deps/libdep-1.rmeta", "--extern", "noprelude:core=/s/libcore.rlib",
        ]);
        let a = parse_argv(&argv, Path::new("/w")).unwrap();
        assert_eq!((a.crate_name.as_str(), a.extra.as_str(), a.incremental, a.test), ("applib", "-a4f8", true, false));
        assert_eq!(a.externs, vec![PathBuf::from("/t/debug/deps/libdep-1.rmeta"), PathBuf::from("/s/libcore.rlib")]);
        let outs: Vec<String> = outputs(&a).iter().map(|p| p.to_string_lossy().into_owned()).collect();
        assert_eq!(outs[..3], ["/t/debug/deps/applib-a4f8.d", "/t/debug/deps/libapplib-a4f8.rmeta", "/t/debug/deps/libapplib-a4f8.rlib"]);
        assert!(parse_argv(&args(&["rustc", "@args.txt"]), Path::new("/w")).is_none());
        assert!(parse_argv(&args(&["rustc", "--crate-name", "x", "--out-dir", "o", "--emit=link=/x"]), Path::new("/w")).is_none());
        let bin = parse_argv(&args(&["rustc", "--crate-name", "app", "--crate-type", "bin", "--emit=dep-info,link", "--out-dir", "d", "-Cextra-filename=-9"]), Path::new("/w")).unwrap();
        assert_eq!(bin.out_dir, PathBuf::from("/w/d"));
        assert_eq!(outputs(&bin)[1], PathBuf::from(format!("/w/d/app-9{}", std::env::consts::EXE_SUFFIX)));
    }

    #[test]
    fn uplift_names_drop_the_hash_and_use_the_target_name_for_executables() {
        let a = Argv { crate_name: "my_app".into(), extra: "-0123".into(), out_dir: "/t/deps".into(), ..Argv::default() };
        assert_eq!(uplift_names(Path::new("/t/deps/my_app-0123"), &a, "my-app"), vec!["my_app", "my-app"]);
        assert_eq!(uplift_names(Path::new("/t/deps/libmy_app-0123.rlib"), &a, "my_app"), vec!["libmy_app.rlib"]);
    }

    #[test]
    fn env_delta_keeps_what_cargo_set_and_removed() {
        let base: HashMap<String, String> = [("PATH", "/bin"), ("KEEP", "1"), ("GONE", "x"), ("CARGO_TERM_COLOR", "always")]
            .iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let env = vec![("PATH".to_string(), "/bin".to_string()), ("KEEP".into(), "1".into()), ("CARGO_PKG_NAME".into(), "a".into())];
        let (set, unset) = delta(&env, &base);
        assert_eq!(set, vec![("CARGO_PKG_NAME".to_string(), "a".to_string())]);
        assert_eq!(unset, vec!["GONE".to_string()]);
    }
}
