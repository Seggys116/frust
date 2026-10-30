//! The `RUSTC_WRAPPER`: anything not understood or not cacheable is exec'd straight through, so new rustc flags cannot break a build.

use std::collections::{BTreeSet, HashMap};
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Instant;

use crate::cache::hashing::{self, DirHash, Hasher, Stamp};
use crate::cache::rustc_args::{self, PathArg, RustcArgs};
use crate::cache::{self, Cache, Candidate, EntryMeta, PROFILE_TOKEN, StoredFile};
use crate::config::Config;
use crate::events::{self, CacheResult, Event};
use crate::fsx;
use crate::paths::Dirs;

const ENV_DENY_PREFIXES: &[&str] =
    &["CARGO_TERM_", "CARGO_BUILD_", "CARGO_NET_", "CARGO_HTTP_", "CARGO_REGISTRIES_", "CARGO_PROFILE_", "CARGO_LOG"];
const ENV_DENY_EXACT: &[&str] =
    &["CARGO", "CARGO_MAKEFLAGS", "CARGO_TARGET_DIR", "CARGO_HOME", "CARGO_INCREMENTAL", "CARGO_PRIMARY_PACKAGE"];

/// Enough for an undo, a switch between two branches and one more state, while bounding edit-loop growth.
pub const UNIT_VERSIONS: usize = 4;

/// Non-`CARGO_*` variables that change rustc or linker output without appearing in dep-info.
const ENV_AFFECTS_OUTPUT: &[&str] = &[
    "RUSTC_BOOTSTRAP",
    "RUSTC_FORCE_RUSTC_VERSION",
    "RUST_TARGET_PATH",
    "MACOSX_DEPLOYMENT_TARGET",
    "IPHONEOS_DEPLOYMENT_TARGET",
    "TVOS_DEPLOYMENT_TARGET",
    "WATCHOS_DEPLOYMENT_TARGET",
    "XROS_DEPLOYMENT_TARGET",
    "SDKROOT",
    "DEVELOPER_DIR",
    "LIBRARY_PATH",
    "ZERO_AR_DATE",
];

pub fn run(args: Vec<OsString>) -> i32 {
    let started = Instant::now();
    let cfg = Config::from_env();
    if cfg.cache
        && let Some(plan) = Plan::build(&cfg, &args)
    {
        let code = plan.execute();
        plan.debug(&format!("{} done in {}ms", plan.parsed.crate_name, started.elapsed().as_millis()));
        return code;
    }
    passthrough(&args)
}

fn passthrough(args: &[OsString]) -> i32 {
    let Some((tool, rest)) = args.split_first() else {
        eprintln!("frust-wrap: expected a compiler command");
        return 2;
    };
    let err = Command::new(tool).args(rest).exec();
    eprintln!("frust-wrap: cannot run {}: {err}", Path::new(tool).display());
    127
}

struct Plan {
    cfg: Config,
    dirs: Dirs,
    cache: Cache,
    args: Vec<OsString>,
    parsed: RustcArgs,
    cwd: PathBuf,
    profile: String,
    profile_abs: PathBuf,
    prekey: String,
    /// Identity of this unit across edits; `None` for registry and git crates.
    unit: Option<String>,
    /// A compile is stored only if none of these changed meanwhile.
    watched: Vec<(PathBuf, Stamp)>,
    extern_hashes: HashMap<PathBuf, String>,
    hasher: Hasher,
}

impl Plan {
    fn build(cfg: &Config, args: &[OsString]) -> Option<Plan> {
        // `<rustc> --flag ...`: a second positional means a tool chain such as clippy-driver.
        if args.len() < 2 || !args[1].to_string_lossy().starts_with('-') {
            return None;
        }
        let debug = |message: String| {
            if cfg.debug {
                eprintln!("frust-wrap: {message}");
            }
        };
        let parsed = match rustc_args::parse(&args[1..]) {
            Ok(parsed) => parsed,
            Err(skip) => {
                debug(format!("not cacheable: {skip:?}"));
                return None;
            }
        };
        let primary = env::var_os("CARGO_PRIMARY_PACKAGE").is_some();
        if primary && !cfg.cache_primary {
            return None;
        }
        if parsed.incremental() {
            // Cached in unoptimised builds only; a miss still runs rustc's own incremental compile.
            let reason = if !parsed.unoptimised() {
                Some("optimised incremental compile")
            } else if !incremental_units_enabled() {
                Some("incremental compile (FRUST_CACHE_INCREMENTAL=off)")
            } else if primary && !primary_incremental_enabled() {
                Some("incremental compile of a primary package (FRUST_CACHE_PRIMARY_INCREMENTAL=off)")
            } else {
                None
            };
            if let Some(reason) = reason {
                debug(format!("not cacheable: {} ({reason})", parsed.crate_name));
                return None;
            }
        }
        let dirs = match Dirs::try_discover() {
            Ok(dirs) => dirs,
            Err(err) => {
                debug(format!("cache disabled: {err}"));
                return None;
            }
        };
        let cwd = env::current_dir().ok()?;
        let profile = rustc_args::profile_root(&parsed.out_dir).to_string_lossy().into_owned();
        let profile_abs = cwd.join(&profile);
        let hasher = Hasher::new(Some(dirs.hashmemo()), immutable_source_roots());
        let key = KeyContext { hasher: &hasher, cwd: &cwd, profile: &profile, profile_abs: &profile_abs };
        let keyed = Instant::now();
        let PreKey { key: prekey, unit, watched, extern_hashes } = match key.prekey(&args[0], &args[1..], &parsed) {
            Ok(found) => found,
            Err(reason) => {
                debug(format!("not cacheable: {} ({reason})", parsed.crate_name));
                return None;
            }
        };
        debug(format!("pre-key for {} in {}ms", parsed.crate_name, keyed.elapsed().as_millis()));
        // Registry and git crates never change, so they accumulate no versions and need no bookkeeping.
        let unit = env::var_os("CARGO_MANIFEST_DIR")
            .filter(|dir| !dir.is_empty() && !hasher.in_immutable_root(Path::new(dir)))
            .map(|_| unit);
        Some(Plan {
            cfg: cfg.clone(),
            cache: Cache::new(&dirs),
            dirs,
            args: args.to_vec(),
            parsed,
            cwd,
            profile,
            profile_abs,
            prekey,
            unit,
            watched,
            extern_hashes,
            hasher,
        })
    }

    fn execute(&self) -> i32 {
        fsx::install_temp_cleanup();
        let started = Instant::now();
        if self.try_hit(started) {
            self.debug(&format!("hit {} in {}ms", self.parsed.crate_name, started.elapsed().as_millis()));
            return 0;
        }
        self.debug(&format!("lookup {} missed after {}ms", self.parsed.crate_name, started.elapsed().as_millis()));
        self.compile_and_store()
    }

    fn norm(&self, text: &str) -> String {
        text.replace(&self.profile, PROFILE_TOKEN)
    }

    fn resolve(&self, normalised: &str) -> PathBuf {
        self.cwd.join(normalised.replace(PROFILE_TOKEN, &self.profile))
    }

    fn version(&self) -> String {
        env::var("CARGO_PKG_VERSION").unwrap_or_default()
    }

    fn try_hit(&self, started: Instant) -> bool {
        for candidate in self.cache.candidates(&self.prekey) {
            if !self.candidate_matches(&candidate) {
                continue;
            }
            let Some((dir, meta)) = self.cache.load_entry(&candidate.key) else {
                continue;
            };
            match self.restore(&dir, &meta) {
                Ok(bytes) => {
                    self.cache.mark_used(&candidate.key);
                    self.note_unit(&candidate.key);
                    let elapsed = started.elapsed().as_millis() as u64;
                    events::record(
                        &self.dirs,
                        &Event::Cache {
                            t: events::now_ms(),
                            result: CacheResult::Hit,
                            krate: self.parsed.crate_name.clone(),
                            version: meta.version.clone(),
                            compile_ms: meta.compile_ms,
                            saved_ms: meta.compile_ms.saturating_sub(elapsed),
                            bytes,
                        },
                    );
                    return true;
                }
                Err(err) => self.debug(&format!("restore failed for {}: {err}", self.parsed.crate_name)),
            }
        }
        false
    }

    fn candidate_matches(&self, candidate: &Candidate) -> bool {
        for (name, expected) in &candidate.envs {
            let current = env::var(name).ok().map(|v| cache::hash_str(&self.norm(&v)));
            if &current != expected {
                return false;
            }
        }
        candidate.inputs.iter().all(|(path, expected)| {
            let path = self.resolve(path);
            self.hasher.hash(&path, self.persistable_source(&path)).map(|h| &h.hash == expected).unwrap_or(false)
        })
    }

    fn persistable_source(&self, path: &Path) -> bool {
        self.hasher.in_immutable_root(path)
    }

    fn restore(&self, entry: &Path, meta: &EntryMeta) -> io::Result<u64> {
        let names_ok = meta.files.iter().all(|f| cache::safe_file_name(&f.name) && cache::safe_file_name(&f.stored))
            && cache::safe_file_name(&meta.stderr);
        if !names_ok {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "entry metadata names a file outside its directory",
            ));
        }
        fs::create_dir_all(&self.parsed.out_dir)?;
        // A temp name plus rename per file lets them be restored concurrently.
        let (out_dir, profile) = (&self.parsed.out_dir, &self.profile);
        let restored = parallel(&meta.files, |_, file| {
            let src = entry.join(&file.stored);
            let dst = out_dir.join(&file.name);
            if file.text {
                let text = fs::read_to_string(&src)?.replace(PROFILE_TOKEN, profile);
                fsx::write_atomic(&dst, text.as_bytes())?;
            } else {
                fsx::clone_replace(&src, &dst)?;
                // Clones keep the old mtime, but cargo treats outputs older than their dependencies' outputs as stale.
                if !dst.is_dir() {
                    fsx::touch(&dst);
                }
            }
            Ok(file.bytes)
        })?;
        let bytes = restored.iter().sum();
        let stderr = fs::read_to_string(entry.join(&meta.stderr))?.replace(PROFILE_TOKEN, &self.profile);
        let mut out = io::stderr().lock();
        out.write_all(stderr.as_bytes())?;
        out.flush()?;
        Ok(bytes)
    }

    fn compile_and_store(&self) -> i32 {
        let started = Instant::now();
        // Taken before rustc starts: an input changed after this may have been read in its old state.
        let spawned_ns = hashing::now_ns();
        let preexisting: HashMap<String, Stamp> =
            self.prefixed_outputs().into_iter().filter_map(|(name, stamp)| Some((name, stamp?))).collect();
        self.debug(&format!("{} earlier outputs scanned in {}ms", preexisting.len(), started.elapsed().as_millis()));
        let mut child = match Command::new(&self.args[0]).args(&self.args[1..]).stderr(Stdio::piped()).spawn() {
            Ok(child) => child,
            Err(err) => {
                eprintln!("frust-wrap: cannot run {}: {err}", Path::new(&self.args[0]).display());
                return 127;
            }
        };
        let pipe = child.stderr.take().expect("stderr was piped");
        let forwarder = thread::spawn(move || forward(pipe));
        let status = child.wait();
        let captured = forwarder.join().unwrap_or_default();
        let Ok(status) = status else { return 1 };
        let code = status.code().unwrap_or(1);
        if status.success() {
            let compile_ms = started.elapsed().as_millis() as u64;
            let store_started = Instant::now();
            if let Err(err) = self.store(&captured, compile_ms, spawned_ns, &preexisting) {
                self.debug(&format!("store failed for {}: {err}", self.parsed.crate_name));
            }
            self.debug(&format!(
                "compiled {} in {compile_ms}ms, stored in {}ms",
                self.parsed.crate_name,
                store_started.elapsed().as_millis()
            ));
        }
        code
    }

    /// Catches outputs rustc writes without an artifact notification, such as split debuginfo objects.
    fn prefixed_outputs(&self) -> Vec<(String, Option<Stamp>)> {
        let mut found = Vec::new();
        if self.parsed.extra_filename.is_empty() {
            return found;
        }
        let prefixes = [
            format!("{}{}", self.parsed.crate_name, self.parsed.extra_filename),
            format!("lib{}{}", self.parsed.crate_name, self.parsed.extra_filename),
        ];
        if let Ok(entries) = fs::read_dir(&self.parsed.out_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if prefixes.iter().any(|p| name.starts_with(p.as_str())) {
                    found.push((name, Stamp::read(&entry.path()).ok()));
                }
            }
        }
        found
    }

    /// Incremental rustc never removes earlier per-compile names, so files unchanged since before rustc started are left out of the entry.
    fn output_files(&self, captured: &[u8], preexisting: &HashMap<String, Stamp>) -> Vec<String> {
        let mut names = BTreeSet::new();
        for (name, stamp) in self.prefixed_outputs() {
            let leftover =
                stamp.as_ref().is_some_and(|now| preexisting.get(&name).is_some_and(|before| same_file(before, now)));
            if !leftover {
                names.insert(name);
            }
        }
        for line in String::from_utf8_lossy(captured).lines() {
            if !line.contains("\"artifact\"") {
                continue;
            }
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(line)
                && let Some(path) = value.get("artifact").and_then(|v| v.as_str())
            {
                let path = Path::new(path);
                if (path.parent() == Some(self.parsed.out_dir.as_path()) || path.parent().is_none())
                    && let Some(name) = path.file_name()
                {
                    names.insert(name.to_string_lossy().into_owned());
                }
            }
        }
        names.into_iter().collect()
    }

    fn store(
        &self,
        captured: &[u8],
        compile_ms: u64,
        spawned_ns: i128,
        preexisting: &HashMap<String, Stamp>,
    ) -> io::Result<()> {
        // Session-directory warnings describe this target directory, not the crate, so hits elsewhere must not replay them.
        if self.parsed.incremental() && String::from_utf8_lossy(captured).contains("incremental compilation") {
            return self.skip_store("rustc reported a problem with its incremental session", compile_ms);
        }
        let names = self.output_files(captured, preexisting);
        let dep_name =
            names.iter().find(|n| n.ends_with(".d")).ok_or_else(|| io::Error::other("no dep-info among outputs"))?;
        let dep_text = fs::read_to_string(self.parsed.out_dir.join(dep_name))?;
        let info = cache::depinfo::parse(&dep_text);
        let started = Instant::now();

        for (path, stamp) in &self.watched {
            if Stamp::read(path).ok().as_ref() != Some(stamp) && !self.extern_unchanged(path) {
                return self.skip_store(&format!("{} changed during the compile", path.display()), compile_ms);
            }
        }
        let mut inputs = Vec::new();
        for file in &info.files {
            let path = self.resolve(&self.norm(file));
            if path.is_dir() {
                continue;
            }
            let hashed = self.hasher.hash(&path, self.persistable_source(&path))?;
            if !hashed.stable {
                return self.skip_store(&format!("{} changed while being hashed", path.display()), compile_ms);
            }
            // Target-profile files come from units that finished under cargo's lock; the rest get the "file too new" check.
            if !path.starts_with(&self.profile_abs) && hashed.stamp.too_new_for(spawned_ns) {
                return self.skip_store(&format!("{} was modified during the compile", path.display()), compile_ms);
            }
            inputs.push((self.norm(file), hashed.hash));
        }
        let envs: Vec<(String, Option<String>)> = info
            .env
            .iter()
            .map(|(name, value)| (name.clone(), value.as_ref().map(|v| cache::hash_str(&self.norm(v)))))
            .collect();
        let key = entry_key(&self.prekey, &inputs, &envs);
        let hashed_ms = started.elapsed().as_millis();

        if self.cache.load_entry(&key).is_none() {
            let staged = self.cache.stage()?;
            let built = self.stage_entry(&staged, &names, (dep_name.as_str(), dep_text.as_str()), captured, compile_ms);
            match built {
                Ok(meta) => {
                    self.cache.publish(&staged, &key)?;
                    events::record(
                        &self.dirs,
                        &Event::Cache {
                            t: events::now_ms(),
                            result: CacheResult::Miss,
                            krate: self.parsed.crate_name.clone(),
                            version: meta.version.clone(),
                            compile_ms,
                            saved_ms: 0,
                            bytes: meta.files.iter().map(|f| f.bytes).sum(),
                        },
                    );
                }
                Err(err) => {
                    let _ = fsx::remove_any(&staged);
                    return Err(err);
                }
            }
        }
        let entry_ms = started.elapsed().as_millis();
        self.note_unit(&key);
        self.cache.add_candidate(&self.prekey, Candidate { inputs, envs, key });
        self.debug(&format!(
            "store {}: {} inputs hashed in {hashed_ms}ms, entry at {entry_ms}ms, manifest at {}ms",
            self.parsed.crate_name,
            info.files.len(),
            started.elapsed().as_millis()
        ));
        Ok(())
    }

    /// Deliberately not storing: an input may not match what rustc read.
    fn skip_store(&self, reason: &str, compile_ms: u64) -> io::Result<()> {
        self.debug(&format!("not storing {}: {reason}", self.parsed.crate_name));
        events::record(
            &self.dirs,
            &Event::Cache {
                t: events::now_ms(),
                result: CacheResult::Miss,
                krate: self.parsed.crate_name.clone(),
                version: self.version(),
                compile_ms,
                saved_ms: 0,
                bytes: 0,
            },
        );
        Ok(())
    }

    /// `dep_info` is the dep-info file's name and the text already read, so it is stored exactly as parsed for the key.
    fn stage_entry(
        &self,
        staged: &Path,
        names: &[String],
        dep_info: (&str, &str),
        captured: &[u8],
        compile_ms: u64,
    ) -> io::Result<EntryMeta> {
        // The staging directory is private until one rename publishes it, so files are cloned straight to final names.
        let (out_dir, profile) = (&self.parsed.out_dir, &self.profile);
        let files = parallel(names, |i, name| {
            let src = out_dir.join(name);
            let stored = format!("f{i}");
            let text = name.ends_with(".d");
            let bytes;
            if text {
                let raw = if name == dep_info.0 { dep_info.1.to_string() } else { fs::read_to_string(&src)? };
                let body = raw.replace(profile.as_str(), PROFILE_TOKEN);
                bytes = body.len() as u64;
                fs::write(staged.join(&stored), body)?;
            } else {
                fsx::clone_new(&src, &staged.join(&stored))?;
                bytes = fsx::allocated_size(&src);
            }
            Ok(StoredFile { name: name.clone(), stored, text, bytes })
        })?;
        let stderr_text = String::from_utf8_lossy(captured).replace(&self.profile, PROFILE_TOKEN);
        fs::write(staged.join("stderr"), stderr_text)?;
        let meta = EntryMeta {
            crate_name: self.parsed.crate_name.clone(),
            version: self.version(),
            compile_ms,
            created_unix: fsx::unix_now(),
            files,
            stderr: "stderr".to_string(),
        };
        fs::write(staged.join("meta.json"), serde_json::to_vec(&meta).map_err(io::Error::other)?)?;
        Ok(meta)
    }

    /// With pipelining a dependency's rustc may rewrite its `.rmeta` identically after dependents start, so equal fresh content means the keyed content was read.
    fn extern_unchanged(&self, path: &Path) -> bool {
        let Some(keyed) = self.extern_hashes.get(path) else {
            return false;
        };
        match hashing::hash_with_stamp(path) {
            Ok(now) => now.stable && &now.hash == keyed,
            Err(_) => false,
        }
    }

    fn note_unit(&self, key: &str) {
        let Some(unit) = &self.unit else { return };
        for old in self.cache.note_unit_entry(unit, key, UNIT_VERSIONS) {
            self.debug(&format!("removing superseded entry {old} of {}", self.parsed.crate_name));
            self.cache.remove_entry(&old);
        }
    }

    fn debug(&self, message: &str) {
        if self.cfg.debug {
            eprintln!("frust-wrap: {message}");
        }
    }
}

/// ctime is left out: it moves whenever another hard link to the inode comes or goes.
fn same_file(a: &Stamp, b: &Stamp) -> bool {
    (a.dev, a.ino, a.size, a.mtime_ns) == (b.dev, b.ino, b.size, b.mtime_ns)
}

fn parallel<T: Sync, R: Send>(items: &[T], job: impl Fn(usize, &T) -> io::Result<R> + Sync) -> io::Result<Vec<R>> {
    const PER_THREAD: usize = 8;
    const MAX_THREADS: usize = 4;
    let threads = items.len().div_ceil(PER_THREAD).clamp(1, MAX_THREADS);
    if threads == 1 {
        return items.iter().enumerate().map(|(i, item)| job(i, item)).collect();
    }
    let chunk = items.len().div_ceil(threads);
    let job = &job;
    thread::scope(|scope| {
        let workers: Vec<_> = items
            .chunks(chunk)
            .enumerate()
            .map(|(c, part)| {
                scope.spawn(move || {
                    part.iter().enumerate().map(|(i, item)| job(c * chunk + i, item)).collect::<io::Result<Vec<R>>>()
                })
            })
            .collect();
        let mut results = Vec::with_capacity(items.len());
        for worker in workers {
            results.extend(worker.join().map_err(|_| io::Error::other("worker thread panicked"))??);
        }
        Ok(results)
    })
}

/// Lines are forwarded as they arrive because cargo needs artifact notifications in real time.
fn forward(pipe: impl io::Read) -> Vec<u8> {
    let mut reader = BufReader::new(pipe);
    let mut captured = Vec::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let mut out = io::stderr().lock();
                let _ = out.write_all(&line);
                let _ = out.flush();
                captured.extend_from_slice(&line);
            }
        }
    }
    captured
}

fn entry_key(prekey: &str, inputs: &[(String, String)], envs: &[(String, Option<String>)]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(prekey.as_bytes());
    for (path, hash) in inputs {
        hasher.update(b"\0i");
        hasher.update(path.as_bytes());
        hasher.update(hash.as_bytes());
    }
    for (name, value) in envs {
        hasher.update(b"\0e");
        hasher.update(name.as_bytes());
        hasher.update(value.as_deref().unwrap_or("<unset>").as_bytes());
    }
    hasher.finalize().to_hex()[..40].to_string()
}

/// `FRUST_CACHE_INCREMENTAL=off` (or `0`, `false`, `no`) keeps incremental units uncached.
fn incremental_units_enabled() -> bool {
    !matches!(env::var("FRUST_CACHE_INCREMENTAL").as_deref(), Ok("0" | "off" | "false" | "no"))
}

/// `FRUST_CACHE_PRIMARY_INCREMENTAL=off` (or `0`, `false`, `no`) leaves primary-package incremental compiles to rustc.
fn primary_incremental_enabled() -> bool {
    !matches!(env::var("FRUST_CACHE_PRIMARY_INCREMENTAL").as_deref(), Ok("0" | "off" | "false" | "no"))
}

/// The pre-key hasher and the unit identity hasher, fed together.
struct Both(blake3::Hasher, blake3::Hasher);

impl Both {
    fn update(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
        self.1.update(bytes);
    }

    /// A content hash: part of the pre-key, never of the identity.
    fn content(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    fn unit_only(&mut self, bytes: &[u8]) {
        self.1.update(bytes);
    }
}

struct PreKey {
    key: String,
    /// The same inputs with every content hash left out; dependencies are named by paths carrying cargo's per-unit hash.
    unit: String,
    watched: Vec<(PathBuf, Stamp)>,
    extern_hashes: HashMap<PathBuf, String>,
}

fn immutable_source_roots() -> Vec<PathBuf> {
    let home = env::var_os("CARGO_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").filter(|v| !v.is_empty()).map(|h| PathBuf::from(h).join(".cargo")));
    match home {
        Some(home) => vec![home.join("registry").join("src"), home.join("git").join("checkouts")],
        None => Vec::new(),
    }
}

struct KeyContext<'a> {
    hasher: &'a Hasher,
    cwd: &'a Path,
    profile: &'a str,
    profile_abs: &'a Path,
}

impl KeyContext<'_> {
    fn prekey(&self, tool: &OsString, rest: &[OsString], parsed: &RustcArgs) -> Result<PreKey, String> {
        let mut watched = Vec::new();
        let mut extern_hashes = HashMap::new();
        let mut hasher = Both(blake3::Hasher::new(), blake3::Hasher::new());
        hasher.update(b"frust-prekey-v2\0");
        hasher.update(toolchain_id(tool, self.cwd).ok_or("cannot identify the compiler")?.as_bytes());
        hasher.update(self.cwd.to_string_lossy().as_bytes());

        let mut native_dirs = Vec::new();
        // Incremental outputs are not byte-identical with non-incremental ones (different codegen unit split), so they never share entries.
        if parsed.incremental() {
            hasher.update(b"\0incremental-unit");
        }
        for (i, arg) in rest.iter().enumerate() {
            if parsed.out_dir_args.contains(&i) || parsed.incremental_args.contains(&i) {
                continue;
            }
            let text = arg.to_str().ok_or("non-UTF-8 argument")?;
            hasher.update(b"\0a");
            let specials: Vec<&PathArg> = parsed.paths.iter().filter(|(idx, _)| *idx == i).map(|(_, p)| p).collect();
            if let Some(PathArg::Extern { name, path }) = specials.iter().find(|p| matches!(p, PathArg::Extern { .. }))
            {
                let path = self.cwd.join(path);
                let hashed = self
                    .hasher
                    .hash(&path, path.starts_with(self.profile_abs))
                    .map_err(|e| format!("cannot read --extern {}: {e}", path.display()))?;
                hasher.update(name.as_bytes());
                hasher.content(hashed.hash.as_bytes());
                hasher.unit_only(path.to_string_lossy().replace(self.profile, PROFILE_TOKEN).as_bytes());
                if path.starts_with(self.profile_abs) {
                    extern_hashes.insert(path.clone(), hashed.hash.clone());
                }
                watched.push((path, hashed.stamp));
                continue;
            }
            hasher.update(text.replace(self.profile, PROFILE_TOKEN).as_bytes());
            for special in specials {
                match special {
                    PathArg::Extern { .. } => {}
                    PathArg::NativeDir(dir) => {
                        let dir = self.cwd.join(dir);
                        hasher.update(b"\0d");
                        hasher.content(self.dir_hash(&dir, &mut watched)?.as_bytes());
                        native_dirs.push(dir);
                    }
                    PathArg::MaybeFiles(files) => {
                        for file in files {
                            let path = self.cwd.join(file);
                            if path.is_file() {
                                hasher.update(b"\0f");
                                hasher.content(self.file_hash(&path, &mut watched)?.as_bytes());
                            } else if rustc_args::looks_like_link_input(&path) {
                                return Err(format!("linker input {} cannot be read", path.display()));
                            }
                        }
                    }
                    PathArg::Target(target) => {
                        if let Some(spec) = self.target_spec(target)? {
                            hasher.update(b"\0t");
                            hasher.content(self.file_hash(&spec, &mut watched)?.as_bytes());
                        }
                    }
                }
            }
        }

        // A bundled static library outside every -L directory comes from a system location the key does not cover.
        for lib in parsed.libs.iter().filter(|l| l.bundled()) {
            let names = lib.file_names();
            if !native_dirs.iter().any(|dir| names.iter().any(|n| dir.join(n).is_file())) {
                return Err(format!("static library `{}` is not in any -L directory", lib.name));
            }
        }

        let mut vars: Vec<(String, String)> = env::vars()
            .filter(|(name, _)| {
                (name.starts_with("CARGO_")
                    && !ENV_DENY_EXACT.contains(&name.as_str())
                    && !ENV_DENY_PREFIXES.iter().any(|p| name.starts_with(p)))
                    || ENV_AFFECTS_OUTPUT.contains(&name.as_str())
            })
            .collect();
        vars.sort();
        for (name, value) in vars {
            hasher.update(b"\0v");
            hasher.update(name.as_bytes());
            hasher.update(value.replace(self.profile, PROFILE_TOKEN).as_bytes());
        }
        let Both(key, unit) = hasher;
        Ok(PreKey {
            key: key.finalize().to_hex()[..40].to_string(),
            unit: unit.finalize().to_hex()[..40].to_string(),
            watched,
            extern_hashes,
        })
    }

    fn file_hash(&self, path: &Path, watched: &mut Vec<(PathBuf, Stamp)>) -> Result<String, String> {
        let hashed = self.hasher.hash(path, false).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        watched.push((path.to_path_buf(), hashed.stamp));
        Ok(hashed.hash)
    }

    fn dir_hash(&self, dir: &Path, watched: &mut Vec<(PathBuf, Stamp)>) -> Result<String, String> {
        if let Ok(stamp) = Stamp::read(dir) {
            watched.push((dir.to_path_buf(), stamp));
        }
        match self.hasher.hash_dir(dir, dir.starts_with(self.profile_abs), watched) {
            DirHash::Hash(hash) => Ok(hash),
            DirHash::Unverifiable(reason) => Err(reason),
        }
    }

    fn target_spec(&self, target: &str) -> Result<Option<PathBuf>, String> {
        let direct = self.cwd.join(target);
        if target.ends_with(".json") || direct.is_file() {
            return if direct.is_file() {
                Ok(Some(direct))
            } else {
                Err(format!("target specification {} cannot be read", direct.display()))
            };
        }
        if let Some(search) = env::var_os("RUST_TARGET_PATH") {
            for dir in env::split_paths(&search) {
                let spec = self.cwd.join(dir).join(format!("{target}.json"));
                if spec.is_file() {
                    return Ok(Some(spec));
                }
            }
        }
        Ok(None)
    }
}

/// The front end sets `FRUST_RUSTC_ID` once per build; otherwise rustc is asked directly.
fn toolchain_id(tool: &OsString, cwd: &Path) -> Option<String> {
    if let Some(id) = env::var("FRUST_RUSTC_ID").ok().filter(|v| !v.is_empty()) {
        return Some(id);
    }
    let output = Command::new(tool).arg("-vV").current_dir(cwd).stdin(Stdio::null()).output().ok()?;
    output.status.success().then(|| crate::paths::short_hash(&output.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Scratch {
            let dir = env::temp_dir().join(format!("frust-wrapper-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(dir.join("target").join("debug").join("deps")).unwrap();
            Scratch(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn key(cwd: &Path, extra: &[&str]) -> Result<String, String> {
        let out_dir = cwd.join("target").join("debug").join("deps");
        let mut args: Vec<OsString> =
            ["--crate-name", "k", "--emit=dep-info,link", "--out-dir"].iter().map(OsString::from).collect();
        args.push(out_dir.clone().into_os_string());
        args.extend(extra.iter().map(OsString::from));
        let parsed = rustc_args::parse(&args).map_err(|e| format!("{e:?}"))?;
        let profile = rustc_args::profile_root(&out_dir).to_string_lossy().into_owned();
        let hasher = Hasher::ephemeral();
        let ctx = KeyContext { hasher: &hasher, cwd, profile: &profile, profile_abs: Path::new(&profile) };
        ctx.prekey(&OsString::from("rustc"), &args, &parsed).map(|p| p.key)
    }

    #[test]
    fn native_dirs_outside_the_profile_are_keyed_by_content_in_every_spelling() {
        let s = Scratch::new("native");
        let lib = s.0.join("ext");
        fs::create_dir_all(&lib).unwrap();
        fs::write(lib.join("libfoo.a"), b"v1").unwrap();
        let joined = format!("-Lnative={}", lib.display());
        let separate = format!("native={}", lib.display());
        let link_arg = format!("link-arg=-L{}", lib.display());
        let before = [
            key(&s.0, &[&joined]).unwrap(),
            key(&s.0, &["-L", &separate]).unwrap(),
            key(&s.0, &["-C", &link_arg]).unwrap(),
        ];
        fs::write(lib.join("libfoo.a"), b"v2").unwrap();
        let after = [
            key(&s.0, &[&joined]).unwrap(),
            key(&s.0, &["-L", &separate]).unwrap(),
            key(&s.0, &["-C", &link_arg]).unwrap(),
        ];
        for (b, a) in before.iter().zip(&after) {
            assert_ne!(b, a);
        }
    }

    #[test]
    fn joined_extern_is_keyed_by_content() {
        let s = Scratch::new("extern");
        let rlib = s.0.join("libdep.rlib");
        fs::write(&rlib, b"v1").unwrap();
        let arg = format!("--extern=dep={}", rlib.display());
        let before = key(&s.0, &[&arg]).unwrap();
        fs::write(&rlib, b"v2").unwrap();
        assert_ne!(before, key(&s.0, &[&arg]).unwrap());
    }

    #[test]
    fn link_inputs_and_target_specs_are_hashed_or_refused() {
        let s = Scratch::new("files");
        let obj = s.0.join("extra.o");
        fs::write(&obj, b"v1").unwrap();
        let arg = format!("link-arg={}", obj.display());
        let before = key(&s.0, &["-C", &arg]).unwrap();
        fs::write(&obj, b"v2").unwrap();
        assert_ne!(before, key(&s.0, &["-C", &arg]).unwrap());
        assert!(key(&s.0, &["-C", "link-arg=missing.o"]).is_err(), "an unreadable object is not keyed by its name");

        fs::write(s.0.join("custom.json"), b"{}").unwrap();
        let spec = key(&s.0, &["--target", "custom.json"]).unwrap();
        fs::write(s.0.join("custom.json"), b"{ }").unwrap();
        assert_ne!(spec, key(&s.0, &["--target", "custom.json"]).unwrap());
        assert!(key(&s.0, &["--target", "absent.json"]).is_err());
        assert!(key(&s.0, &["--sysroot", "/somewhere"]).is_err());
    }

    #[test]
    fn bundled_static_library_must_come_from_a_search_directory() {
        let s = Scratch::new("static");
        assert!(key(&s.0, &["-l", "static=nowhere"]).is_err());
        let lib = s.0.join("ext");
        fs::create_dir_all(&lib).unwrap();
        fs::write(lib.join("libnowhere.a"), b"x").unwrap();
        assert!(key(&s.0, &["-l", "static=nowhere", &format!("-L{}", lib.display())]).is_ok());
        assert!(key(&s.0, &["-l", "dylib=system"]).is_ok(), "dynamic libraries are only named in the output");
    }
}
