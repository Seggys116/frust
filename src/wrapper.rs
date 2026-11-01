//! The `RUSTC_WRAPPER`: anything not understood or not cacheable is exec'd straight through, so new rustc flags cannot break a build.

use std::cell::Cell;
use std::collections::{BTreeSet, HashMap};
use std::env;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::cache::hashing::{self, DirHash, Hasher, Stamp, StampRecord, StampTrust};
use crate::cache::rustc_args::{self, PathArg, RustcArgs};
use crate::cache::{
    self, Cache, Candidate, EntryMeta, Input, PROFILE_TOKEN, StoredFile, TrustMode,
};
use crate::compact;
use crate::config::Config;
use crate::events::{self, CacheResult, Event};
use crate::fsx;
use crate::lock;
use crate::paths::{DEFERRED_SLOT_PREFIX, Dirs};
use crate::sys::{self, LockKind};

const ENV_DENY_PREFIXES: &[&str] = &[
    "CARGO_TERM_",
    "CARGO_BUILD_",
    "CARGO_NET_",
    "CARGO_HTTP_",
    "CARGO_REGISTRIES_",
    "CARGO_PROFILE_",
    "CARGO_LOG",
];
const ENV_DENY_EXACT: &[&str] = &[
    "CARGO",
    "CARGO_MAKEFLAGS",
    "CARGO_TARGET_DIR",
    "CARGO_HOME",
    "CARGO_INCREMENTAL",
    "CARGO_PRIMARY_PACKAGE",
];

/// Enough for an undo, a switch between two branches and one more state, while bounding edit-loop growth.
pub const UNIT_VERSIONS: usize = 4;

/// Candidates with more inputs than this are verified on a small pool of threads.
const PARALLEL_VERIFY_MIN_INPUTS: usize = 16;

// Bounded and private: cargo runs many wrappers at once, so the global rayon pool would oversubscribe.
const VERIFY_THREADS: usize = 4;

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
    "COMPILER_PATH",
    "GCC_EXEC_PREFIX",
    "CCC_OVERRIDE_OPTIONS",
    "TOOLCHAINS",
    "LDFLAGS",
    "LIBRARY_PATH",
    "ZERO_AR_DATE",
];

/// First argument of the detached helper that finishes a store; a compiler command line never starts with it.
const DEFERRED_ARG: &str = "--deferred-store";
const JOB_LEASE_ENV: &str = "FRUST_DEFERRED_JOB_LEASE";

// A lock held by a process that is working finishes within seconds, so the poll starts fast and settles at 40 ms.
const KEY_POLL_START: Duration = Duration::from_millis(5);
const KEY_POLL_MAX: Duration = Duration::from_millis(40);

/// Bumped whenever `StoreJob` changes incompatibly; a helper meeting another format stores nothing.
const JOB_FORMAT: u32 = 3;

static JOBSERVER: OnceLock<Option<jobserver::Client>> = OnceLock::new();
static DETACHED_STORE: AtomicBool = AtomicBool::new(false);
pub(crate) const SLOT_SUPERVISOR_ENV: &str = "FRUST_JOBSERVER_SUPERVISOR";

pub fn run(args: Vec<OsString>) -> i32 {
    if let Some(code) = crate::store_broker::broker_entry(&args) {
        return code;
    }
    if let Some(code) = sys::jobserver_loan::helper_entry(&args) {
        return code;
    }
    if let Some(job) = deferred_entry(&args) {
        DETACHED_STORE.store(true, Ordering::Relaxed);
        return deferred_main(job);
    }
    // SAFETY: startup has opened no files; pipe validation rejects stale descriptor numbers.
    JOBSERVER.get_or_init(|| unsafe { jobserver::Client::from_env_ext(true).client.ok() });
    let started = Instant::now();
    let cfg = Config::from_env();
    let args = crate::linking::prepare(&args, cfg.debug).unwrap_or(args);
    if cfg.cache
        && let Some(plan) = Plan::build(&cfg, &args)
    {
        let code = plan.execute();
        plan.debug(&format!(
            "{} done in {}ms",
            plan.parsed.crate_name,
            started.elapsed().as_millis()
        ));
        return code;
    }
    passthrough(&args)
}

fn passthrough(args: &[OsString]) -> i32 {
    let Some((tool, rest)) = args.split_first() else {
        eprintln!("frust-wrap: expected a compiler command");
        return 2;
    };
    let err = sys::exec_replace(compiler_command(tool).args(rest));
    eprintln!(
        "frust-wrap: cannot run {}: {err}",
        Path::new(tool).display()
    );
    127
}

fn compiler_command(tool: &OsString) -> Command {
    let mut cmd = Command::new(tool);
    if let Some(Some(client)) = JOBSERVER.get() {
        client.configure(&mut cmd);
    }
    cmd
}

struct Plan {
    cfg: Config,
    context: StoreContext,
    dirs: Dirs,
    cache: Cache,
    args: Vec<OsString>,
    parsed: RustcArgs,
    cwd: PathBuf,
    output_dir_abs: PathBuf,
    profile: String,
    profile_abs: PathBuf,
    prekey: String,
    /// Identity of this unit across edits; `None` for registry and git crates.
    unit: Option<String>,
    /// A compile is stored only if none of these changed meanwhile.
    watched: Vec<(PathBuf, Stamp)>,
    extern_hashes: HashMap<PathBuf, String>,
    hasher: Hasher,
    /// Inputs whose recorded stamp is unchanged are accepted without reading them, on volumes where that is safe.
    trust: Arc<TrustGate>,
    /// Milliseconds spent queued behind another process's compile of this unit; zero when there was no wait.
    waited_ms: AtomicU64,
    /// A helper finishing a store for a wrapper that has already returned; the wrapper recorded the cache event, so the helper records none.
    deferred: bool,
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
            // Cache only artifacts; a miss still runs rustc with its original incremental session.
            let reason = if !incremental_units_enabled() {
                Some("incremental compile (FRUST_CACHE_INCREMENTAL=off)")
            } else if primary && !primary_incremental_enabled() {
                Some(
                    "incremental compile of a primary package (FRUST_CACHE_PRIMARY_INCREMENTAL=off)",
                )
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
        let profile = rustc_args::profile_root(&parsed.out_dir)
            .to_string_lossy()
            .into_owned();
        let profile_abs = cwd.join(&profile);
        let output_dir_abs = cwd.join(&parsed.out_dir);
        let context = StoreContext::capture(&cwd);
        let trust = Arc::new(TrustGate::new(TrustMode::from_env(), &dirs, cfg.debug));
        let hasher = Hasher::new(
            Some(dirs.hashmemo()),
            context.immutable_source_roots.clone(),
        )
        .with_stamp_trust(stamp_policy(&trust));
        let key = KeyContext {
            hasher: &hasher,
            cwd: &cwd,
            profile: &profile,
            profile_abs: &profile_abs,
        };
        let keyed = Instant::now();
        let PreKey {
            key: prekey,
            unit,
            watched,
            extern_hashes,
        } = match key.prekey(&args[0], &args[1..], &parsed) {
            Ok(found) => found,
            Err(reason) => {
                debug(format!("not cacheable: {} ({reason})", parsed.crate_name));
                return None;
            }
        };
        debug(format!(
            "pre-key for {} in {}ms",
            parsed.crate_name,
            keyed.elapsed().as_millis()
        ));
        // Registry and git crates never change, so they accumulate no versions and need no bookkeeping.
        let unit = env::var_os("CARGO_MANIFEST_DIR")
            .filter(|dir| !dir.is_empty() && !hasher.in_immutable_root(&cwd.join(dir)))
            .map(|_| unit);
        Some(Plan {
            cfg: cfg.clone(),
            context,
            cache: Cache::new(&dirs),
            dirs,
            args: args.to_vec(),
            parsed,
            cwd,
            output_dir_abs,
            profile,
            profile_abs,
            prekey,
            unit,
            watched,
            extern_hashes,
            hasher,
            trust,
            waited_ms: AtomicU64::new(0),
            deferred: false,
        })
    }

    /// Rebuilds the plan a deferred store needs from the job; the key pieces come from the job, never recomputed.
    fn from_job(job: &StoreJob, worker_config: &Config) -> Option<Plan> {
        // Serialized paths retain the producer's lexical identity across every job.
        if job.format != JOB_FORMAT
            || job.args.len() < 2
            || !job.cwd.is_absolute()
            || job
                .context
                .immutable_source_roots
                .iter()
                .any(|path| !path.is_absolute())
            || job.watched.iter().any(|(path, _)| !path.is_absolute())
            || job
                .extern_hashes
                .iter()
                .any(|(path, _)| !path.is_absolute())
            || job.inputs.iter().any(|(path, _)| !path.is_absolute())
        {
            return None;
        }
        let args: Vec<OsString> = job.args.iter().map(OsString::from).collect();
        let parsed = rustc_args::parse(&args[1..]).ok()?;
        let mut cfg = worker_config.clone();
        // The helper is detached and silent.
        cfg.debug = false;
        let dirs = Dirs::at(job.cwd.join(&job.home));
        let profile = rustc_args::profile_root(&parsed.out_dir)
            .to_string_lossy()
            .into_owned();
        let profile_abs = job.cwd.join(&profile);
        let output_dir_abs = job.cwd.join(&parsed.out_dir);
        let watched = job
            .watched
            .iter()
            .map(|(path, wire)| Some((path.clone(), wire.stamp()?)))
            .collect::<Option<Vec<_>>>()?;
        let extern_hashes = job.extern_hashes.iter().cloned().collect();
        let trust = Arc::new(TrustGate::new(
            TrustMode::parse(Some(job.trust.as_str())),
            &dirs,
            false,
        ));
        let hasher = Hasher::new(
            Some(dirs.hashmemo()),
            job.context.immutable_source_roots.clone(),
        )
        .with_stamp_trust(stamp_policy(&trust));
        let cache = Cache::new(&dirs);
        Some(Plan {
            cfg,
            context: job.context.clone(),
            cache,
            dirs,
            args,
            parsed,
            cwd: job.cwd.clone(),
            output_dir_abs,
            profile,
            profile_abs,
            prekey: job.prekey.clone(),
            unit: job.unit.clone(),
            watched,
            extern_hashes,
            hasher,
            trust,
            waited_ms: AtomicU64::new(0),
            deferred: true,
        })
    }

    fn execute(&self) -> i32 {
        self.execute_after_miss(|| {})
    }

    fn execute_after_miss(&self, after_miss: impl FnOnce()) -> i32 {
        fsx::install_temp_cleanup();
        let started = Instant::now();
        if self.try_hit(started) {
            self.debug(&format!(
                "hit {} in {}ms",
                self.parsed.crate_name,
                started.elapsed().as_millis()
            ));
            return 0;
        }
        self.debug(&format!(
            "lookup {} missed after {}ms",
            self.parsed.crate_name,
            started.elapsed().as_millis()
        ));
        after_miss();
        let (key_lock, waited) = match self.lock_key() {
            Ok(locked) => locked,
            Err(error) => {
                eprintln!("frust-wrap: cannot settle compiler slot: {error}");
                return 1;
            }
        };
        // The process that held the lock has published its entry by now if it stored one.
        if (key_lock.is_some() || waited) && self.try_hit(started) {
            self.debug(&format!(
                "hit {} after key lock, in {}ms",
                self.parsed.crate_name,
                started.elapsed().as_millis()
            ));
            return 0;
        }
        self.compile_and_store(key_lock)
    }

    /// Returns the key lock and wait state; lock failures allow compilation, while unsettled slot loans fail the build.
    fn lock_key(&self) -> io::Result<(Option<File>, bool)> {
        let path = keylock_path(&self.dirs, &self.prekey);
        let file = match open_key_lock(&self.dirs, &path) {
            Ok(file) => file,
            Err(err) => {
                self.debug(&format!(
                    "no key lock for {}: {err}",
                    self.parsed.crate_name
                ));
                return Ok((None, false));
            }
        };
        match try_key_lock(&file) {
            Ok(true) => Ok((Some(file), false)),
            Ok(false) => {
                self.debug(&format!(
                    "waiting for key lock for {}",
                    self.parsed.crate_name
                ));
                let begun = Instant::now();
                let client = if DETACHED_STORE.load(Ordering::Relaxed) {
                    None
                } else {
                    JOBSERVER.get().and_then(Option::as_ref)
                };
                let mut watcher = path.parent().and_then(sys::DirectoryWatcher::for_key_lock);
                let got = wait_key_with_slot(
                    file,
                    self.cfg.keylock_wait,
                    || lend_cargo_slot(client),
                    |file, cap| match watcher.as_mut() {
                        Some(watcher) => wait_key_lock_hints(file, cap, |pause| watcher.wait(pause)),
                        None => wait_key_lock(file, cap),
                    },
                    || open_key_lock(&self.dirs, &path),
                );
                let waited_ms = (begun.elapsed().as_millis() as u64).max(1);
                self.waited_ms.store(waited_ms, Ordering::Relaxed);
                self.debug(&format!(
                    "waited {waited_ms}ms for {}",
                    self.parsed.crate_name
                ));
                match got {
                    Ok(Some(file)) => Ok((Some(file), true)),
                    Err(KeyWaitError::Loan(error)) => Err(error),
                    _ => Ok((None, true)),
                }
            }
            Err(err) => {
                self.debug(&format!("cannot lock {}: {err}", path.display()));
                Ok((None, false))
            }
        }
    }

    fn waited(&self) -> u64 {
        self.waited_ms.load(Ordering::Relaxed)
    }

    fn norm(&self, text: &str) -> String {
        text.replace(&self.profile, PROFILE_TOKEN)
    }

    fn resolve(&self, normalised: &str) -> PathBuf {
        self.cwd
            .join(normalised.replace(PROFILE_TOKEN, &self.profile))
    }

    fn version(&self) -> String {
        self.context.package_version.clone()
    }

    fn try_hit(&self, started: Instant) -> bool {
        for candidate in self.cache.candidates(&self.prekey) {
            // A row whose entry was evicted is skipped before any of its inputs is read.
            if !self.cache.has_entry(&candidate.key) || !self.candidate_matches(&candidate) {
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
                    events::record_waited(
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
                        self.waited(),
                    );
                    return true;
                }
                Err(err) => self.debug(&format!(
                    "restore failed for {}: {err}",
                    self.parsed.crate_name
                )),
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
        let checks: Vec<(usize, PathBuf, bool, &Input)> = newest_first(&candidate.inputs)
            .into_iter()
            .enumerate()
            .map(|(i, input)| {
                let path = self.resolve(&input.path);
                let persist = self.persistable_source(&path);
                (i, path, persist, input)
            })
            .collect();
        let (hasher, gate, now, krate) = (
            &self.hasher,
            &self.trust,
            hashing::now_ns(),
            self.parsed.crate_name.as_str(),
        );
        let skipped = Mutex::new(Vec::new());
        let confirmed = AtomicUsize::new(0);
        let check = |(i, path, persist, input): &(usize, PathBuf, bool, &Input)| match input_matches(
            hasher, path, *persist, input, gate, now,
        ) {
            Verdict::Match => true,
            Verdict::Trusted => {
                skipped
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(*i);
                true
            }
            Verdict::Confirmed => {
                confirmed.fetch_add(1, Ordering::Relaxed);
                true
            }
            Verdict::Mismatch => false,
            Verdict::Violation { fresh, dev } => {
                gate.violation(krate, dev, input, &fresh);
                false
            }
        };
        let all = if checks.len() > PARALLEL_VERIFY_MIN_INPUTS {
            all_parallel(&checks, check)
        } else {
            checks.iter().all(check)
        };
        if !all {
            return false;
        }
        let skipped: Vec<usize> = skipped.into_inner().unwrap_or_else(PoisonError::into_inner);
        let mut audited = confirmed.into_inner();
        // A hit that skipped reads re-hashes a sample of what it trusted, so a stamp that lies is caught on ordinary builds.
        let trusted: Vec<&Path> = skipped.iter().map(|&i| checks[i].1.as_path()).collect();
        for pick in audit_selection(&trusted, audit_seed()) {
            let (_, path, _, input) = &checks[skipped[pick]];
            match audit_trusted(path, input, now) {
                Verdict::Confirmed => audited += 1,
                Verdict::Violation { fresh, dev } => {
                    gate.violation(krate, dev, input, &fresh);
                    return false;
                }
                _ => return false,
            }
        }
        cache::note_audits_passed(&self.dirs, audited);
        true
    }

    fn persistable_source(&self, path: &Path) -> bool {
        self.hasher.in_immutable_root(path)
    }

    fn restore(&self, entry: &Path, meta: &EntryMeta) -> io::Result<u64> {
        let names_ok = meta
            .files
            .iter()
            .all(|f| cache::safe_file_name(&f.name) && cache::safe_file_name(&f.stored))
            && cache::safe_file_name(&meta.stderr);
        if !names_ok {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "entry metadata names a file outside its directory",
            ));
        }
        if meta
            .files
            .iter()
            .any(|file| !file.encoding.is_raw() && (file.text || file.dir))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "encoded entry metadata marks an artifact as text or a directory",
            ));
        }
        let groups = cache::metadata_slice::validate_groups(&meta.files)?;
        if !self.parsed.out_dir.is_dir() {
            fs::create_dir_all(&self.parsed.out_dir)?;
        }
        // A temp name plus rename per file lets them be restored concurrently.
        let (out_dir, profile) = (&self.parsed.out_dir, &self.profile);
        let restored = parallel(&groups, |_, group| {
            let file = &meta.files[group.base_index];
            if !group.slice_indices.is_empty() {
                cache::metadata_slice::restore_group(entry, out_dir, &meta.files, group)?;
                let mut bytes = 0;
                for index in
                    std::iter::once(group.base_index).chain(group.slice_indices.iter().copied())
                {
                    let output = &meta.files[index];
                    fsx::touch(&out_dir.join(&output.name));
                    bytes += output.bytes;
                }
                return Ok(bytes);
            }
            let src = entry.join(&file.stored);
            let dst = out_dir.join(&file.name);
            if file.text {
                let text = dep_info_resolved(&fs::read_to_string(&src)?, profile);
                fsx::write_atomic(&dst, text.as_bytes())?;
            } else {
                cache::payload::restore(&src, &dst, file)?;
                // Clones keep the old mtime, but cargo treats outputs older than their dependencies' outputs as stale.
                if !file.dir {
                    fsx::touch(&dst);
                }
            }
            Ok(file.bytes)
        })?;
        let bytes = restored.iter().sum();
        let stderr =
            fs::read_to_string(entry.join(&meta.stderr))?.replace(PROFILE_TOKEN, &self.profile);
        let mut out = io::stderr().lock();
        out.write_all(stderr.as_bytes())?;
        out.flush()?;
        Ok(bytes)
    }

    /// `key_lock` is held until the store has published, so a process queued behind it finds the entry.
    fn compile_and_store(&self, key_lock: Option<File>) -> i32 {
        if self.can_defer_store()
            && env::var_os(crate::store_broker::CAPABILITY_ENV).is_some_and(|value| !value.is_empty())
        {
            events::request_store();
        }
        let started = Instant::now();
        // Taken after any wait and before rustc starts: an input changed after this may have been read in its old state.
        let spawned_ns = hashing::now_ns();
        let preexisting: HashMap<String, Stamp> = self
            .prefixed_outputs(|_| true)
            .into_iter()
            .filter_map(|(name, stamp)| Some((name, stamp?)))
            .collect();
        self.debug(&format!(
            "{} earlier outputs scanned in {}ms",
            preexisting.len(),
            started.elapsed().as_millis()
        ));
        let mut child = match compiler_command(&self.args[0])
            .args(&self.args[1..])
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(err) => {
                eprintln!(
                    "frust-wrap: cannot run {}: {err}",
                    Path::new(&self.args[0]).display()
                );
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
            if self.defer_store(
                &captured,
                compile_ms,
                spawned_ns,
                &preexisting,
                key_lock.as_ref(),
            ) {
                self.debug(&format!(
                    "compiled {} in {compile_ms}ms, store deferred",
                    self.parsed.crate_name
                ));
            } else {
                if let Err(err) = self.store(&captured, compile_ms, spawned_ns, &preexisting) {
                    self.debug(&format!(
                        "store failed for {}: {err}",
                        self.parsed.crate_name
                    ));
                }
                self.debug(&format!(
                    "compiled {} in {compile_ms}ms, stored in {}ms",
                    self.parsed.crate_name,
                    store_started.elapsed().as_millis()
                ));
            }
        }
        // Dropped after the store: the helper of a deferred store holds its own copy of the lock.
        drop(key_lock);
        code
    }

    /// Returns true on acknowledged deferral; false requests inline fallback, possibly after a lost broker acknowledgement.
    fn defer_store(
        &self,
        captured: &[u8],
        compile_ms: u64,
        spawned_ns: i128,
        preexisting: &HashMap<String, Stamp>,
        key_lock: Option<&File>,
    ) -> bool {
        // Capability probes reuse or delete these exact output names immediately after the wrapper returns.
        if !self.can_defer_store() {
            return false;
        }
        match self.spawn_deferred(captured, compile_ms, spawned_ns, preexisting, key_lock) {
            Ok(handed_over) => handed_over,
            Err(err) => {
                self.debug(&format!(
                    "store of {} not deferred: {err}",
                    self.parsed.crate_name
                ));
                false
            }
        }
    }

    fn can_defer_store(&self) -> bool {
        !self.cfg.store_sync
            && !(self.parsed.metadata_only_library && self.parsed.extra_filename.is_empty())
    }

    fn spawn_deferred(
        &self,
        captured: &[u8],
        compile_ms: u64,
        spawned_ns: i128,
        preexisting: &HashMap<String, Stamp>,
        key_lock: Option<&File>,
    ) -> io::Result<bool> {
        let deferred = self.dirs.deferred();
        let broker = crate::store_broker::AdmissionPermit::try_reserve(&self.dirs)?;
        let slot = if broker.is_none() {
            acquire_slot(&deferred, slot_count())
        } else {
            None
        };
        if broker.is_none() && slot.is_none() {
            self.debug(&format!(
                "deferred store slots busy or unavailable for {}; storing inline",
                self.parsed.crate_name
            ));
            return Ok(false);
        };
        let Some(frozen) = self.freeze(captured, spawned_ns, preexisting) else {
            return Ok(false);
        };
        // Both locks travel to the helper as inherited handles, which stdio must not be mistaken for.
        let mut keep: Vec<&File> = slot.iter().map(|(file, _)| file).collect();
        keep.extend(key_lock);
        if broker.is_none() && !sys::inheritable_clear_of_stdio(&keep) {
            return Ok(false);
        }
        // Where handles are not inherited the helper takes both locks again by path.
        let helper_relocks = broker.is_none() && !sys::INHERITS_DESCRIPTORS;
        let Some(args) = self
            .args
            .iter()
            .map(|a| a.to_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
        else {
            return Ok(false);
        };
        let job = StoreJob {
            format: JOB_FORMAT,
            ownership_version: u32::from(sys::INHERITS_DESCRIPTORS),
            context: self.context.clone(),
            home: self.cwd.join(self.dirs.root()),
            cwd: self.cwd.clone(),
            args,
            prekey: self.prekey.clone(),
            unit: self.unit.clone(),
            trust: self.trust.mode.name().to_string(),
            compile_ms,
            spawned_ns: spawned_ns.to_string(),
            captured: String::from_utf8_lossy(captured).into_owned(),
            watched: self
                .watched
                .iter()
                .map(|(path, stamp)| (path.clone(), StampWire::of(stamp)))
                .collect(),
            extern_hashes: self
                .extern_hashes
                .iter()
                .map(|(path, hash)| (path.clone(), hash.clone()))
                .collect(),
            names: frozen.names,
            dep_name: frozen.dep_name,
            dep_text: frozen.dep_text,
            outputs: frozen
                .outputs
                .iter()
                .map(|(name, stamp)| (name.clone(), StampWire::of(stamp)))
                .collect(),
            commitments: frozen.commitments,
            inputs: frozen
                .inputs
                .iter()
                .map(|(path, stamp)| (path.clone(), StampWire::of(stamp)))
                .collect(),
            slot_lock: slot
                .as_ref()
                .filter(|_| helper_relocks)
                .map(|(_, path)| self.cwd.join(path)),
            key_lock: key_lock
                .filter(|_| helper_relocks)
                .map(|_| self.cwd.join(keylock_path(&self.dirs, &self.prekey))),
        };
        let bytes: u64 = job.outputs.iter().map(|(_, stamp)| stamp.size).sum();
        lock::ensure_dir(&deferred, None)?;
        let (job_path, job_lease) = write_leased_job(&deferred, &job)?;
        keep.push(&job_lease);
        let handed_over = match broker {
            Some(permit) => submit_broker_job(&job_path, |path| {
                permit.submit(&self.cwd.join(path), key_lock)
            }),
            None => spawn_helper(&job_path, &self.cwd, &keep, &job_lease).map(|()| true),
        };
        match handed_over {
            Ok(true) => {
                // Recorded now so the run's feed sees the compile; the helper stores quietly.
                events::record_waited(
                    &self.dirs,
                    &Event::Cache {
                        t: events::now_ms(),
                        result: CacheResult::Miss,
                        krate: self.parsed.crate_name.clone(),
                        version: self.version(),
                        compile_ms,
                        saved_ms: 0,
                        bytes,
                    },
                    self.waited(),
                );
                Ok(true)
            }
            Ok(false) => Ok(false),
            Err(err) => {
                let _ = fs::remove_file(&job_path);
                Err(err)
            }
        }
    }

    /// Records, before the wrapper returns, what a later store must still find: the output set and the identity of every output and input. `None` where a store must stay inline.
    fn freeze(
        &self,
        captured: &[u8],
        spawned_ns: i128,
        preexisting: &HashMap<String, Stamp>,
    ) -> Option<Frozen> {
        self.freeze_with_capture(captured, spawned_ns, preexisting, cache::payload::capture)
    }

    fn freeze_with_capture(
        &self,
        captured: &[u8],
        spawned_ns: i128,
        preexisting: &HashMap<String, Stamp>,
        mut capture: impl FnMut(&Path) -> io::Result<(sys::Meta, cache::payload::Commitment)>,
    ) -> Option<Frozen> {
        if self.declines_session(captured) {
            return None;
        }
        // A deferred store leans on stamps of files that may be rewritten in place, so the profile volume must keep faithful ones.
        let profile = Stamp::read(&self.profile_abs).ok()?;
        if !self.trust.volume_trusted(profile.dev, &self.profile_abs) {
            return None;
        }
        let names = self.output_files(captured, preexisting);
        let dep_name = names.iter().find(|n| n.ends_with(".d"))?.clone();
        let dep_text = fs::read_to_string(self.output_dir_abs.join(&dep_name)).ok()?;
        let mut inputs = HashMap::new();
        for file in &cache::depinfo::parse(&dep_text).files {
            let path = self.resolve(&self.norm(file));
            let meta = sys::stat(&path).ok()?;
            if meta.is_dir() {
                continue;
            }
            let stamp = Stamp::of(&meta);
            // Reject inputs that cannot settle for this compile before reading output commitments.
            if !path.starts_with(&self.profile_abs) && stamp.too_new_for(spawned_ns) {
                return None;
            }
            inputs.insert(path, stamp);
        }
        let mut outputs = Vec::with_capacity(names.len());
        let mut commitments = HashMap::new();
        let compress = self.context.compression_requested
            && cache::payload::compression_supported(&self.dirs.tmp());
        for name in &names {
            let path = self.output_dir_abs.join(name);
            // A bundle's inner files can change without its own stamp moving.
            let mut meta = sys::stat(&path).ok()?;
            if meta.is_dir() {
                return None;
            }
            if compress
                && !name.ends_with(".d")
                && meta.is_file()
                && meta.len() >= cache::payload::MIN_BYTES
            {
                let (committed_meta, commitment) = capture(&path).ok()?;
                if Stamp::of(&meta) != Stamp::of(&committed_meta)
                    || meta.mode() != committed_meta.mode()
                {
                    return None;
                }
                meta = committed_meta;
                commitments.insert(name.clone(), commitment);
            }
            outputs.push((name.clone(), Stamp::of(&meta)));
        }
        let frozen = Frozen {
            names,
            dep_name,
            dep_text,
            outputs,
            inputs,
            commitments,
        };
        (!frozen.has_coarse_stamp()).then_some(frozen)
    }

    /// Session-directory warnings describe this target directory, not the crate, so hits elsewhere must not replay them.
    fn declines_session(&self, captured: &[u8]) -> bool {
        self.parsed.incremental()
            && String::from_utf8_lossy(captured).contains("incremental compilation")
    }

    /// Catches outputs rustc writes without an artifact notification, such as split debuginfo objects; only names accepted by `wants_stamp` are stat'd.
    fn prefixed_outputs(&self, wants_stamp: impl Fn(&str) -> bool) -> Vec<(String, Option<Stamp>)> {
        let mut found = Vec::new();
        let extra = &self.parsed.extra_filename;
        // Cargo's flat hash suffix gives metadata compiles two exact output names.
        let cargo_hash = extra
            .strip_prefix('-')
            .is_some_and(|hash| !hash.is_empty() && hash.bytes().all(|b| b.is_ascii_hexdigit()));
        if self.parsed.metadata_only_library && (extra.is_empty() || cargo_hash) {
            for name in [
                format!("{}{}.d", self.parsed.crate_name, extra),
                format!("lib{}{}.rmeta", self.parsed.crate_name, extra),
            ] {
                let path = self.output_dir_abs.join(&name);
                if let Ok(meta) = sys::lstat(&path)
                    && meta.is_file()
                {
                    let stamp = wants_stamp(&name).then(|| Stamp::of(&meta));
                    found.push((name, stamp));
                }
            }
            return found;
        }
        if extra.is_empty() {
            if self.parsed.unhashed_windows_pdb() {
                let name = format!("{}.pdb", self.parsed.crate_name);
                if let Ok(meta) = sys::lstat(&self.output_dir_abs.join(&name))
                    && meta.is_file()
                {
                    let stamp = wants_stamp(&name).then(|| Stamp::of(&meta));
                    found.push((name, stamp));
                }
            }
            return found;
        }
        // Codegen and custom suffixes still need discovery of unannounced sibling outputs.
        let prefixes = [
            format!("{}{}", self.parsed.crate_name, self.parsed.extra_filename),
            format!(
                "lib{}{}",
                self.parsed.crate_name, self.parsed.extra_filename
            ),
        ];
        if let Ok(entries) = fs::read_dir(&self.output_dir_abs) {
            for entry in entries.flatten() {
                let filename = entry.file_name();
                let name = filename.to_string_lossy();
                if prefixes.iter().any(|p| name.starts_with(p.as_str())) {
                    let stamp = if wants_stamp(&name) {
                        Stamp::read(&entry.path()).ok()
                    } else {
                        None
                    };
                    found.push((name.into_owned(), stamp));
                }
            }
        }
        found
    }

    /// Incremental rustc never removes earlier per-compile names, so files unchanged since before rustc started are left out of the entry.
    fn output_files(&self, captured: &[u8], preexisting: &HashMap<String, Stamp>) -> Vec<String> {
        let mut names = BTreeSet::new();
        let pdb = self.parsed.unhashed_windows_pdb().then(|| format!("{}.pdb", self.parsed.crate_name));
        let mut changed_pdb = false;
        let mut linked_output = false;
        // A file that did not exist before the compile cannot be a leftover, so only earlier names need a stamp.
        for (name, stamp) in self.prefixed_outputs(|name| preexisting.contains_key(name)) {
            let leftover = stamp.as_ref().is_some_and(|now| {
                preexisting
                    .get(&name)
                    .is_some_and(|before| same_file(before, now))
            });
            if !leftover {
                if pdb.as_ref() == Some(&name) { changed_pdb = true; }
                else { names.insert(name); }
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
                    if pdb.is_some() && value.get("emit").and_then(|value| value.as_str()) == Some("link") {
                        linked_output |= ["exe", "dll"].iter().any(|extension| {
                            name == std::ffi::OsStr::new(&format!("{}.{}", self.parsed.crate_name, extension))
                        });
                    }
                    names.insert(name.to_string_lossy().into_owned());
                }
            }
        }
        // An unchanged PDB can belong to an earlier compile; only a matching announced link output owns a changed companion.
        if changed_pdb && linked_output {
            names.insert(pdb.unwrap());
        }
        // An unhashed metadata entry is useful only when both of its mandatory outputs belong to this compile.
        if self.parsed.extra_filename.is_empty()
            && self.parsed.metadata_only_library
            && ![
                format!("{}.d", self.parsed.crate_name),
                format!("lib{}.rmeta", self.parsed.crate_name),
            ]
            .iter()
            .all(|name| names.contains(name))
        {
            return Vec::new();
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
        if self.declines_session(captured) {
            return self.skip_store(
                "rustc reported a problem with its incremental session",
                compile_ms,
            );
        }
        let names = self.output_files(captured, preexisting);
        let dep_name = names
            .iter()
            .find(|n| n.ends_with(".d"))
            .ok_or_else(|| io::Error::other("no dep-info among outputs"))?;
        let dep_text = fs::read_to_string(self.output_dir_abs.join(dep_name))?;
        self.store_outputs(
            captured,
            compile_ms,
            spawned_ns,
            &names,
            (dep_name.as_str(), dep_text.as_str()),
            None,
        )
    }

    /// With `frozen` (a deferred store) every input and output must still be the file that existed when the wrapper returned, because the build that owned the target directory has since moved on.
    fn store_outputs(
        &self,
        captured: &[u8],
        compile_ms: u64,
        spawned_ns: i128,
        names: &[String],
        dep_info: (&str, &str),
        frozen: Option<&Frozen>,
    ) -> io::Result<()> {
        let (dep_name, dep_text) = dep_info;
        let info = cache::depinfo::parse(dep_text);
        let started = Instant::now();

        for (path, stamp) in &self.watched {
            if Stamp::read(path).ok().as_ref() != Some(stamp) && !self.extern_unchanged(path) {
                return self.skip_store(
                    &format!("{} changed during the compile", path.display()),
                    compile_ms,
                );
            }
        }
        let paths: Vec<_> = info
            .files
            .iter()
            .map(|file| self.resolve(&self.norm(file)))
            .collect();
        // Keep failures beside their input so validation and entry keys retain dep-info order.
        let hash_input = |_: usize, path: &PathBuf| {
            Ok(if path.is_dir() {
                None
            } else {
                Some(self.hasher.hash(path, self.persistable_source(path)))
            })
        };
        // Deferred work owns one worker; Cargo's implicit slot belongs to the compiler.
        let hashed = if self.deferred {
            parallel_with_limit(&paths, 1, hash_input)
        } else {
            parallel(&paths, hash_input)
        }?;
        let mut inputs = Vec::with_capacity(paths.len());
        for ((file, path), hashed) in info.files.iter().zip(&paths).zip(hashed) {
            let Some(hashed) = hashed else { continue };
            let hashed = hashed?;
            if !hashed.stable {
                return self.skip_store(
                    &format!("{} changed while being hashed", path.display()),
                    compile_ms,
                );
            }
            // Other units' outputs gain hard links as cargo uplifts them, which moves ctime without touching content.
            if frozen.is_some_and(|f| {
                !f.input_unchanged(path, &hashed.stamp, !path.starts_with(&self.profile_abs))
            }) {
                return self.skip_store(
                    &format!("{} changed after the compile", path.display()),
                    compile_ms,
                );
            }
            // Target-profile files come from units that finished under cargo's lock; the rest get the "file too new" check.
            if !path.starts_with(&self.profile_abs) && hashed.stamp.too_new_for(spawned_ns) {
                return self.skip_store(
                    &format!("{} was modified during the compile", path.display()),
                    compile_ms,
                );
            }
            // Recorded only for a file that had settled when hashed, and not on a volume whose stamps are distrusted.
            let stamp = if self.trust.records_stamps(hashed.stamp.dev, path) {
                StampRecord::of(&hashed)
            } else {
                None
            };
            inputs.push(Input {
                path: self.norm(file),
                stamp,
                hash: hashed.hash,
            });
        }
        let envs: Vec<(String, Option<String>)> = info
            .env
            .iter()
            .map(|(name, value)| {
                (
                    name.clone(),
                    value.as_ref().map(|v| cache::hash_str(&self.norm(v))),
                )
            })
            .collect();
        let key = entry_key(&self.prekey, &inputs, &envs);
        let hashed_ms = started.elapsed().as_millis();

        if !self.cache.has_entry(&key) {
            if frozen.is_some_and(|f| !f.outputs_unchanged(&self.output_dir_abs)) {
                return self.skip_store("an output changed after the compile", compile_ms);
            }
            let staged = self.cache.stage()?;
            let built = self.stage_entry(
                &staged,
                names,
                (dep_name, dep_text),
                captured,
                compile_ms,
                frozen.map(|f| &f.commitments),
            );
            match built {
                // Checked again once cloned: an output replaced while being cloned must not be published.
                Ok(_) if frozen.is_some_and(|f| !f.outputs_unchanged(&self.output_dir_abs)) => {
                    let _ = fsx::remove_any(&staged);
                    return self.skip_store("an output changed while being stored", compile_ms);
                }
                Ok(meta) => {
                    if let Err(err) = self.cache.publish(&staged, &key) {
                        let _ = fsx::remove_any(&staged);
                        return Err(err);
                    }
                    self.record_miss(
                        &meta.version,
                        compile_ms,
                        meta.files.iter().map(|f| f.bytes).sum(),
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
        self.cache
            .add_candidate(&self.prekey, Candidate { inputs, envs, key });
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
        self.record_miss(&self.version(), compile_ms, 0);
        Ok(())
    }

    fn record_miss(&self, version: &str, compile_ms: u64, bytes: u64) {
        if self.deferred {
            return;
        }
        events::record_waited(
            &self.dirs,
            &Event::Cache {
                t: events::now_ms(),
                result: CacheResult::Miss,
                krate: self.parsed.crate_name.clone(),
                version: version.to_string(),
                compile_ms,
                saved_ms: 0,
                bytes,
            },
            self.waited(),
        );
    }

    /// `dep_info` is the dep-info file's name and the text already read, so it is stored exactly as parsed for the key.
    fn stage_entry(
        &self,
        staged: &Path,
        names: &[String],
        dep_info: (&str, &str),
        captured: &[u8],
        compile_ms: u64,
        commitments: Option<&HashMap<String, cache::payload::Commitment>>,
    ) -> io::Result<EntryMeta> {
        // The staging directory is private until one rename publishes it, so files are cloned straight to final names.
        let (out_dir, profile) = (&self.output_dir_abs, &self.profile);
        let compress =
            self.context.compression_requested && cache::payload::compression_supported(staged);
        let indices: HashMap<&str, usize> = names
            .iter()
            .enumerate()
            .map(|(i, name)| (name.as_str(), i))
            .collect();
        let paired: Vec<Option<usize>> = names
            .iter()
            .map(|name| {
                if !compress {
                    return None;
                }
                name.strip_suffix(".rmeta")
                    .and_then(|stem| indices.get(format!("{stem}.rlib").as_str()).copied())
            })
            .collect();
        let captured_backings: BTreeSet<usize> = paired
            .iter()
            .enumerate()
            .filter_map(|(i, base)| {
                let base = (*base)?;
                let commitment = commitments?.get(&names[i])?;
                commitment.valid().then_some(base)
            })
            .collect();
        let store_file = |i: usize,
                          name: &String,
                          capture: bool|
         -> io::Result<(StoredFile, Option<cache::payload::StoreProof>)> {
            let src = out_dir.join(name);
            let stored = format!("f{i}");
            let text = name.ends_with(".d");
            let (payload, proof) = if text {
                let raw = if name == dep_info.0 {
                    dep_info.1.to_string()
                } else {
                    fs::read_to_string(&src)?
                };
                let body = dep_info_tokenised(&raw, profile);
                let bytes = body.len() as u64;
                fs::write(staged.join(&stored), body)?;
                (
                    cache::payload::StoredPayload {
                        bytes,
                        encoding: cache::payload::Encoding::Raw,
                        stored_bytes: None,
                        decoded_bytes: None,
                        content_hash: None,
                    },
                    None,
                )
            } else if capture {
                cache::payload::store_proven(
                    &src,
                    &staged.join(&stored),
                    compress,
                    commitments.and_then(|c| c.get(name)),
                    true,
                )?
            } else {
                (
                    cache::payload::store_committed(
                        &src,
                        &staged.join(&stored),
                        compress,
                        commitments.and_then(|c| c.get(name)),
                    )?,
                    None,
                )
            };
            let dir = !text && src.is_dir();
            Ok((
                StoredFile {
                    name: name.clone(),
                    stored,
                    text,
                    bytes: payload.bytes,
                    dir,
                    encoding: payload.encoding,
                    stored_bytes: payload.stored_bytes,
                    decoded_bytes: payload.decoded_bytes,
                    content_hash: payload.content_hash,
                    metadata_slice: None,
                },
                proof,
            ))
        };
        let store_normal = |i: usize, name: &String| {
            if paired[i].is_some() {
                Ok(None)
            } else {
                store_file(i, name, captured_backings.contains(&i)).map(Some)
            }
        };
        let normal = if self.deferred {
            parallel_with_limit(names, 1, store_normal)
        } else {
            parallel(names, store_normal)
        }?;
        // Only this exclusive staging transaction can consume its producer captures; unavailable proofs decode the staged backing.
        let store_metadata = |i: usize, name: &String| {
            let Some(base_index) = paired[i] else {
                return Ok(normal[i].as_ref().unwrap().0.clone());
            };
            let (base, proof) = normal[base_index].as_ref().unwrap();
            let src = out_dir.join(name);
            let target = StoredFile {
                name: name.clone(),
                stored: format!("f{i}"),
                text: false,
                bytes: fsx::allocated_size(&src),
                dir: src.is_dir(),
                encoding: cache::payload::Encoding::Raw,
                stored_bytes: None,
                decoded_bytes: None,
                content_hash: None,
                metadata_slice: None,
            };
            match cache::metadata_slice::build_proven(
                &staged.join(&base.stored),
                base,
                &src,
                &target,
                commitments.and_then(|c| c.get(name)),
                proof.as_ref(),
            )?
            .0
            {
                Some(file) => Ok(file),
                None => store_file(i, name, false).map(|(file, _)| file),
            }
        };
        let files = if self.deferred {
            parallel_with_limit(names, 1, store_metadata)
        } else {
            parallel(names, store_metadata)
        }?;
        cache::metadata_slice::validate_groups(&files)?;
        let stderr_text = String::from_utf8_lossy(captured).replace(&self.profile, PROFILE_TOKEN);
        fs::write(staged.join("stderr"), stderr_text)?;
        let meta = EntryMeta {
            crate_name: self.parsed.crate_name.clone(),
            version: self.version(),
            compile_ms,
            created_unix: fsx::unix_now(),
            prekey: Some(self.prekey.clone()),
            incremental_unit: if self.parsed.incremental() {
                self.unit.clone()
            } else {
                None
            },
            files,
            stderr: "stderr".to_string(),
        };
        fs::write(
            staged.join("meta.json"),
            serde_json::to_vec(&meta).map_err(io::Error::other)?,
        )?;
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
            self.debug(&format!(
                "retired superseded entry {old} of {}",
                self.parsed.crate_name
            ));
        }
    }

    fn debug(&self, message: &str) {
        if self.cfg.debug {
            eprintln!("frust-wrap: {message}");
        }
    }
}

/// Dep-info writes a space in a path as `\ `, but env-dep values are only newline-escaped, so each line kind has its own spelling of the profile.
fn rewrite_dep_info(text: &str, plain: (&str, &str), spaced: (&str, &str)) -> String {
    text.split_inclusive('\n')
        .map(|line| {
            let (from, to) = if line.starts_with("# env-dep:") {
                plain
            } else {
                spaced
            };
            line.replace(from, to)
        })
        .collect()
}

fn escape_spaces(path: &str) -> String {
    path.replace(' ', "\\ ")
}

fn dep_info_tokenised(raw: &str, profile: &str) -> String {
    rewrite_dep_info(
        raw,
        (profile, PROFILE_TOKEN),
        (escape_spaces(profile).as_str(), PROFILE_TOKEN),
    )
}

fn dep_info_resolved(stored: &str, profile: &str) -> String {
    rewrite_dep_info(
        stored,
        (PROFILE_TOKEN, profile),
        (PROFILE_TOKEN, escape_spaces(profile).as_str()),
    )
}

/// ctime is left out: it moves whenever another hard link to the inode comes or goes.
fn same_file(a: &Stamp, b: &Stamp) -> bool {
    (a.dev, a.ino, a.size, a.mtime_ns) == (b.dev, b.ino, b.size, b.mtime_ns)
}

fn cache_compression_requested() -> bool {
    !matches!(
        env::var("FRUST_CACHE_COMPRESSION").as_deref(),
        Ok("off" | "0" | "false" | "no")
    )
}

fn parallel<T: Sync, R: Send>(
    items: &[T],
    job: impl Fn(usize, &T) -> io::Result<R> + Sync,
) -> io::Result<Vec<R>> {
    let threads = extern_hash_threads_with(
        items.len(),
        DETACHED_STORE.load(Ordering::Relaxed),
        || thread::available_parallelism().map_or(1, |n| n.get()),
    );
    parallel_with_threads(items, threads, job)
}

fn parallel_with_threads<T: Sync, R: Send>(
    items: &[T],
    threads: usize,
    job: impl Fn(usize, &T) -> io::Result<R> + Sync,
) -> io::Result<Vec<R>> {
    let budget = worker_budget(threads.min(items.len()));
    parallel_with_limit(items, budget.threads, job)
}

struct WorkerBudget {
    threads: usize,
    _tokens: Vec<jobserver::Acquired>,
}

impl WorkerBudget {
    fn reserve(desired: usize, client: Option<&jobserver::Client>) -> Self {
        let mut tokens = Vec::new();
        if let Some(client) = client {
            for _ in 1..desired {
                match client.try_acquire() {
                    Ok(Some(token)) => tokens.push(token),
                    _ => break,
                }
            }
        }
        let threads = if client.is_some() {
            tokens.len() + 1
        } else {
            desired.max(1)
        };
        Self {
            threads,
            _tokens: tokens,
        }
    }
}

fn worker_budget(desired: usize) -> WorkerBudget {
    worker_budget_with(
        desired,
        DETACHED_STORE.load(Ordering::Relaxed),
        JOBSERVER.get().and_then(Option::as_ref),
        || thread::available_parallelism().map_or(1, |n| n.get()),
    )
}

fn worker_budget_with(
    desired: usize,
    detached: bool,
    client: Option<&jobserver::Client>,
    discover_cpus: impl FnOnce() -> usize,
) -> WorkerBudget {
    let cpus = if desired <= 1 || detached {
        1
    } else {
        discover_cpus()
    };
    worker_budget_for(desired, cpus, detached, client)
}

fn worker_budget_for(
    desired: usize,
    cpus: usize,
    detached: bool,
    client: Option<&jobserver::Client>,
) -> WorkerBudget {
    if detached {
        return WorkerBudget::reserve(1, None);
    }
    // The caller owns Cargo's implicit slot; every additional foreground worker needs a token.
    WorkerBudget::reserve(desired.min(cpus).clamp(1, 4), client)
}

fn parallel_with_limit<T: Sync, R: Send>(
    items: &[T],
    threads: usize,
    job: impl Fn(usize, &T) -> io::Result<R> + Sync,
) -> io::Result<Vec<R>> {
    let threads = threads.min(items.len());
    if threads <= 1 {
        return items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(i, item)))
                    .unwrap_or_else(|_| Err(io::Error::other("worker thread panicked")))
            })
            .collect();
    }
    let next = AtomicUsize::new(0);
    let failed_at = AtomicUsize::new(usize::MAX);
    let worker = || {
        let mut results = Vec::new();
        loop {
            let i = next.fetch_add(1, Ordering::Relaxed);
            if i >= items.len() || i >= failed_at.load(Ordering::Relaxed) {
                break;
            }
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(i, &items[i])))
                    .unwrap_or_else(|_| Err(io::Error::other("worker thread panicked")));
            if result.is_err() {
                failed_at.fetch_min(i, Ordering::Relaxed);
                results.push((i, result));
                break;
            }
            results.push((i, result));
        }
        results
    };
    thread::scope(|scope| {
        let workers: Vec<_> = (1..threads).map(|_| scope.spawn(worker)).collect();
        let mut results = worker();
        for handle in workers {
            results.extend(
                handle
                    .join()
                    .map_err(|_| io::Error::other("worker thread panicked"))?,
            );
        }
        // Completion order never changes output names, entry keys, or which input error is reported.
        results.sort_unstable_by_key(|(i, _)| *i);
        results.into_iter().map(|(_, result)| result).collect()
    })
}

/// A stamp in a job file; the nanosecond fields are strings because JSON numbers cannot hold them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StampWire {
    dev: u64,
    ino: u64,
    size: u64,
    mtime_ns: String,
    ctime_ns: String,
}

impl StampWire {
    fn of(stamp: &Stamp) -> StampWire {
        StampWire {
            dev: stamp.dev,
            ino: stamp.ino,
            size: stamp.size,
            mtime_ns: stamp.mtime_ns.to_string(),
            ctime_ns: stamp.ctime_ns.to_string(),
        }
    }

    fn stamp(&self) -> Option<Stamp> {
        Some(Stamp {
            dev: self.dev,
            ino: self.ino,
            size: self.size,
            mtime_ns: self.mtime_ns.parse().ok()?,
            ctime_ns: self.ctime_ns.parse().ok()?,
        })
    }
}

// Producer policy is captured once; storage capabilities stay local to the volume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoreContext {
    package_version: String,
    compression_requested: bool,
    immutable_source_roots: Vec<PathBuf>,
}

impl StoreContext {
    fn capture(cwd: &Path) -> Self {
        Self {
            package_version: env::var("CARGO_PKG_VERSION").unwrap_or_default(),
            compression_requested: cache_compression_requested(),
            immutable_source_roots: immutable_source_roots()
                .iter()
                .map(|root| cwd.join(root))
                .collect(),
        }
    }
}

/// Everything a detached helper needs to finish a store, written before the wrapper returns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoreJob {
    format: u32,
    #[serde(default)]
    ownership_version: u32,
    context: StoreContext,
    home: PathBuf,
    cwd: PathBuf,
    args: Vec<String>,
    prekey: String,
    unit: Option<String>,
    trust: String,
    compile_ms: u64,
    spawned_ns: String,
    captured: String,
    watched: Vec<(PathBuf, StampWire)>,
    extern_hashes: Vec<(PathBuf, String)>,
    names: Vec<String>,
    dep_name: String,
    dep_text: String,
    outputs: Vec<(String, StampWire)>,
    inputs: Vec<(PathBuf, StampWire)>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    commitments: HashMap<String, cache::payload::Commitment>,
    /// Lock files the helper takes again itself when handles cannot be inherited.
    slot_lock: Option<PathBuf>,
    key_lock: Option<PathBuf>,
}

pub(crate) struct BrokerJob(StoreJob);

impl BrokerJob {
    pub(crate) fn decode(bytes: &[u8], dirs: &Dirs, key: Option<&File>) -> io::Result<Self> {
        let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid broker store job");
        let job: StoreJob = serde_json::from_slice(bytes).map_err(|_| invalid())?;
        if job.format != JOB_FORMAT
            || job.ownership_version > 1
            || !job.cwd.is_absolute()
            || !job.home.is_absolute()
            || job.home != dirs.root()
            || !cache::valid_key(&job.prekey)
            || job
                .unit
                .as_ref()
                .is_some_and(|unit| !cache::valid_key(unit))
            || !matches!(job.trust.as_str(), "on" | "off" | "audit")
            || job.slot_lock.is_some()
            || job.key_lock.is_some()
            || job.spawned_ns.parse::<i128>().is_err()
            || job.args.len() < 2
            || job
                .context
                .immutable_source_roots
                .iter()
                .any(|path| !path.is_absolute())
            || job
                .watched
                .iter()
                .any(|(path, stamp)| !path.is_absolute() || stamp.stamp().is_none())
            || job
                .extern_hashes
                .iter()
                .any(|(path, _)| !path.is_absolute())
            || job
                .inputs
                .iter()
                .any(|(path, stamp)| !path.is_absolute() || stamp.stamp().is_none())
            || job.names.is_empty()
            || job.names.iter().any(|name| !cache::safe_file_name(name))
            || job.names.iter().collect::<BTreeSet<_>>().len() != job.names.len()
            || !job.dep_name.ends_with(".d")
            || !job.names.contains(&job.dep_name)
            || !job
                .outputs
                .iter()
                .map(|(name, _)| name)
                .eq(job.names.iter())
            || job
                .inputs
                .iter()
                .map(|(path, _)| path)
                .collect::<BTreeSet<_>>()
                .len()
                != job.inputs.len()
            || Frozen::from_job(&job).is_none()
        {
            return Err(invalid());
        }
        let args: Vec<OsString> = job.args[1..].iter().map(OsString::from).collect();
        let parsed = rustc_args::parse(&args).map_err(|_| invalid())?;
        let profile = rustc_args::profile_root(&parsed.out_dir)
            .to_string_lossy()
            .into_owned();
        let inputs: BTreeSet<_> = cache::depinfo::parse(&job.dep_text)
            .files
            .iter()
            .map(|file| {
                job.cwd.join(
                    file.replace(&profile, PROFILE_TOKEN)
                        .replace(PROFILE_TOKEN, &profile),
                )
            })
            .collect();
        if job.inputs.iter().any(|(path, _)| !inputs.contains(path)) {
            return Err(invalid());
        }
        if let Some(key) = key {
            // The received description must retain the producer's exact compile lock.
            let expected = sys::lstat(&keylock_path(dirs, &job.prekey))?;
            let received = sys::fstat(key)?;
            if !expected.is_file()
                || !received.is_file()
                || expected.ino() == 0
                || expected.dev() != received.dev()
                || expected.ino() != received.ino()
            {
                return Err(invalid());
            }
        }
        Ok(Self(job))
    }

    pub(crate) fn execute(self, worker_config: &Config) {
        run_deferred_with(&self.0, worker_config);
    }
}

/// The state of a finished compile as the wrapper left it; a deferred store proceeds only while the files still match.
#[derive(Debug, Clone)]
struct Frozen {
    names: Vec<String>,
    dep_name: String,
    dep_text: String,
    outputs: Vec<(String, Stamp)>,
    inputs: HashMap<PathBuf, Stamp>,
    commitments: HashMap<String, cache::payload::Commitment>,
}

impl Frozen {
    fn from_job(job: &StoreJob) -> Option<Frozen> {
        for (name, commitment) in &job.commitments {
            let stamp = job
                .outputs
                .iter()
                .find(|(output, _)| output == name)?
                .1
                .stamp()?;
            if !cache::safe_file_name(name)
                || name.ends_with(".d")
                || !job.names.contains(name)
                || !commitment.valid()
                || !commitment.matches_identity(stamp.dev, stamp.ino, stamp.size, stamp.mtime_ns)
            {
                return None;
            }
        }
        Some(Frozen {
            commitments: job.commitments.clone(),
            names: job.names.clone(),
            dep_name: job.dep_name.clone(),
            dep_text: job.dep_text.clone(),
            outputs: job
                .outputs
                .iter()
                .map(|(name, wire)| Some((name.clone(), wire.stamp()?)))
                .collect::<Option<Vec<_>>>()?,
            inputs: job
                .inputs
                .iter()
                .map(|(path, wire)| Some((path.clone(), wire.stamp()?)))
                .collect::<Option<HashMap<_, _>>>()?,
        })
    }

    /// Whole-second stamps cannot show a rewrite within the same second.
    fn has_coarse_stamp(&self) -> bool {
        self.outputs
            .iter()
            .map(|(_, stamp)| stamp)
            .chain(self.inputs.values())
            .any(Stamp::coarse)
    }

    /// `ctime_matters` is false for files cargo hard-links after the fact, where only the content identity counts.
    fn input_unchanged(&self, path: &Path, now: &Stamp, ctime_matters: bool) -> bool {
        self.inputs.get(path).is_some_and(|then| {
            same_file(then, now) && (!ctime_matters || then.ctime_ns == now.ctime_ns)
        })
    }

    /// Cargo hard-links outputs into place after the wrapper returns, so ctime is left out of the comparison.
    fn outputs_unchanged(&self, out_dir: &Path) -> bool {
        self.outputs.iter().all(|(name, then)| {
            Stamp::read(&out_dir.join(name)).is_ok_and(|now| same_file(then, &now))
        })
    }
}

/// Removes its job file however the helper ends.
struct JobFile(PathBuf);

impl JobFile {
    /// Only `job-*.json` directly inside the deferred directory is claimed, so the argument can never name another file to read or delete.
    fn claim(path: PathBuf, deferred: &Path) -> Option<JobFile> {
        let named = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("job-") && n.ends_with(".json"));
        let cwd = env::current_dir().ok()?;
        let inside = path
            .parent()
            .is_some_and(|parent| cwd.join(parent) == cwd.join(deferred));
        // Lazy on purpose: building a JobFile for a refused path would delete it on drop.
        (named && inside).then(|| JobFile(path))
    }
}

impl Drop for JobFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn keylock_path(dirs: &Dirs, prekey: &str) -> PathBuf {
    dirs.keylocks()
        .join(prekey.get(..2).unwrap_or("00"))
        .join(prekey)
}

/// The parent directory is created only when the first open finds it missing.
fn open_key_lock(dirs: &Dirs, path: &Path) -> io::Result<File> {
    let mode = lock::shared_mode(dirs);
    match lock::open_lock_file(path, mode) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                lock::ensure_dir(parent, mode)?;
            }
            lock::open_lock_file(path, mode)
        }
        other => other,
    }
}

/// `Ok(false)` means another open file description holds the lock.
fn try_key_lock(file: &File) -> io::Result<bool> {
    match sys::try_lock(file, LockKind::Exclusive) {
        Ok(()) => Ok(true),
        Err(fs::TryLockError::WouldBlock) => Ok(false),
        Err(fs::TryLockError::Error(err)) => Err(err),
    }
}

/// Polls with a growing pause until the lock is free or `cap` has passed.
fn wait_key_lock(file: &File, cap: Duration) -> io::Result<bool> {
    let begun = Instant::now();
    let mut pause = KEY_POLL_START;
    loop {
        let elapsed = begun.elapsed();
        if elapsed >= cap {
            return Ok(false);
        }
        thread::sleep(pause.min(cap - elapsed));
        if try_key_lock(file)? {
            return Ok(true);
        }
        pause = (pause * 2).min(KEY_POLL_MAX);
    }
}

fn wait_key_lock_hints(
    file: &File,
    cap: Duration,
    mut wait: impl FnMut(Duration) -> bool,
) -> io::Result<bool> {
    let begun = Instant::now();
    // Subscription precedes this recheck; close notifications never authorize admission.
    if !cap.is_zero() && try_key_lock(file)? {
        return Ok(true);
    }
    let mut pause = KEY_POLL_START;
    loop {
        let remaining = cap.saturating_sub(begun.elapsed());
        if remaining.is_zero() {
            return Ok(false);
        }
        let changed = wait(pause.min(remaining));
        if try_key_lock(file)? {
            return Ok(true);
        }
        // Linux may report close before releasing flock, so every retry remains bounded.
        pause = if changed { KEY_POLL_START } else { (pause * 2).min(KEY_POLL_MAX) };
    }
}

fn lend_cargo_slot(
    client: Option<&jobserver::Client>,
) -> io::Result<Option<sys::jobserver_loan::Loan>> {
    let supervisor = env::var(SLOT_SUPERVISOR_ENV)
        .ok()
        .and_then(|value| value.parse().ok());
    let cargo = env::var_os("CARGO").map(PathBuf::from);
    if client.is_none()
        || !supervisor
            .zip(cargo)
            .is_some_and(|(pid, cargo)| sys::jobserver_loan::direct_cargo_parent(pid, &cargo))
    {
        return Ok(None);
    }
    let Ok(helper) = helper_exe() else {
        return Ok(None);
    };
    match sys::jobserver_loan::lend(&helper) {
        Ok(loan) => Ok(Some(loan)),
        Err(sys::jobserver_loan::LendError::Unavailable(_)) => Ok(None),
        #[cfg(unix)]
        Err(sys::jobserver_loan::LendError::Uncertain(error)) => Err(error),
    }
}

trait SlotLoan {
    fn resume(&mut self) -> io::Result<()>;
}

impl SlotLoan for sys::jobserver_loan::Loan {
    fn resume(&mut self) -> io::Result<()> {
        sys::jobserver_loan::Loan::resume(self)
    }
}

#[derive(Debug)]
enum KeyWaitError {
    Lock(io::Error),
    Loan(io::Error),
}

impl std::fmt::Display for KeyWaitError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Lock(error) | Self::Loan(error) => error.fmt(formatter),
        }
    }
}

struct WaitingKey(File);

impl Drop for WaitingKey {
    fn drop(&mut self) {
        let _ = sys::unlock(&self.0);
    }
}

/// An idle wrapper lends its implicit Cargo slot, then regains it without holding the key lock.
fn wait_key_with_slot<L: SlotLoan>(
    mut file: File,
    cap: Duration,
    mut lend: impl FnMut() -> io::Result<Option<L>>,
    mut wait: impl FnMut(&File, Duration) -> io::Result<bool>,
    mut reopen: impl FnMut() -> io::Result<File>,
) -> Result<Option<File>, KeyWaitError> {
    let begun = Instant::now();
    loop {
        let remaining = cap.saturating_sub(begun.elapsed());
        if remaining.is_zero() {
            return Ok(None);
        }
        let Some(mut slot) = lend().map_err(KeyWaitError::Loan)? else {
            return wait(&file, remaining)
                .map(|acquired| acquired.then_some(file))
                .map_err(KeyWaitError::Lock);
        };
        let acquired = {
            // Explicit unlocking also releases duplicated or inherited handles before slot repayment.
            let waiting = WaitingKey(file);
            wait(&waiting.0, remaining)
        };
        slot.resume().map_err(KeyWaitError::Loan)?;
        if !acquired.map_err(KeyWaitError::Lock)? {
            return Ok(None);
        }
        file = reopen().map_err(KeyWaitError::Lock)?;
        if try_key_lock(&file).map_err(KeyWaitError::Lock)? {
            return Ok(Some(file));
        }
    }
}

/// How many deferred stores may run at once across all processes.
fn slot_count() -> usize {
    (thread::available_parallelism().map_or(2, |n| n.get()) / 2).clamp(1, 4)
}

/// A free slot lock and its path, or `None` when all `count` are taken or none can be opened.
fn acquire_slot(dir: &Path, count: usize) -> Option<(File, PathBuf)> {
    lock::ensure_dir(dir, None).ok()?;
    (0..count).find_map(|i| {
        let path = dir.join(format!("{DEFERRED_SLOT_PREFIX}{i}"));
        let file = lock::open_lock_file(&path, None).ok()?;
        try_key_lock(&file).ok()?.then_some((file, path))
    })
}

#[cfg(test)]
fn write_job(dir: &Path, job: &StoreJob) -> io::Result<PathBuf> {
    write_leased_job(dir, job).map(|(path, _)| path)
}

fn write_leased_job(dir: &Path, job: &StoreJob) -> io::Result<(PathBuf, File)> {
    lock::ensure_dir(dir, None)?;
    let path = dir.join(format!(
        "job-{}-{}.json",
        std::process::id(),
        hashing::now_ns()
    ));
    let bytes = serde_json::to_vec(job).map_err(io::Error::other)?;
    let tmp = fsx::temp_sibling(&path);
    let result = (|| {
        let mut file = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(&bytes)?;
        sys::lock(&file, LockKind::Shared)?;
        sys::rename(&tmp, &path)?;
        Ok((path, file))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn submit_broker_job(
    path: &Path,
    submit: impl FnOnce(&Path) -> io::Result<bool>,
) -> io::Result<bool> {
    let outcome = submit(path);
    // Only the producer marker is ours; a lost ACK may already have a broker owner.
    let _ = fs::remove_file(path);
    outcome
}

fn read_job(path: &Path) -> Option<StoreJob> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

const MAX_PROTECTION_JOB_BYTES: u64 = 2 * 1024 * 1024;
const MAX_PROTECTION_BYTES: u64 = 32 * 1024 * 1024;
const MAX_PROTECTION_PATHS: usize = 100_000;
const PROTECTION_TIME: Duration = Duration::from_millis(250);

fn job_identity(path: &Path, file: &File) -> io::Result<sys::Meta> {
    let path_meta = sys::lstat(path)?;
    let opened = sys::fstat(file)?;
    if !path_meta.is_file()
        || path_meta.file_type().is_symlink()
        || !sys::owned_by_me(path)
        || !opened.is_file()
        || opened.ino() == 0
        || (
            path_meta.dev(),
            path_meta.ino(),
            path_meta.len(),
            path_meta.mtime(),
        ) != (opened.dev(), opened.ino(), opened.len(), opened.mtime())
    {
        return Err(io::Error::other("deferred job identity changed"));
    }
    Ok(opened)
}

pub(crate) fn lease_deferred_job(path: &Path) -> io::Result<File> {
    let file = sys::open_existing_lock_file(path)?;
    job_identity(path, &file)?;
    sys::try_lock(&file, LockKind::Shared).map_err(io::Error::from)?;
    job_identity(path, &file)?;
    Ok(file)
}

fn read_owned_job(path: &Path, file: &File) -> io::Result<Vec<u8>> {
    let before = job_identity(path, file)?;
    if before.len() > MAX_PROTECTION_JOB_BYTES {
        return Err(io::Error::other("deferred job exceeds metadata bound"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_PROTECTION_JOB_BYTES + 1)
        .read_to_end(&mut bytes)?;
    let after = job_identity(path, file)?;
    if before.len() != bytes.len() as u64
        || before.len() != after.len()
        || before.mtime() != after.mtime()
        || before.ctime() != after.ctime()
    {
        return Err(io::Error::other("deferred job changed while reading"));
    }
    Ok(bytes)
}

pub(crate) enum DeferredRetirement {
    Legacy(File),
    Busy,
    Dead(File),
}

/// Both file variants retain the exclusive inode lease until the caller finishes rechecking and unlinking.
pub(crate) fn deferred_retirement(path: &Path) -> io::Result<DeferredRetirement> {
    let file = sys::open_existing_lock_file(path)?;
    job_identity(path, &file)?;
    match sys::try_lock(&file, LockKind::Exclusive) {
        Ok(()) => {}
        Err(fs::TryLockError::WouldBlock) => return Ok(DeferredRetirement::Busy),
        Err(fs::TryLockError::Error(err)) => return Err(err),
    }
    if sys::fstat(&file)?.len() > MAX_PROTECTION_JOB_BYTES {
        job_identity(path, &file)?;
        return Ok(DeferredRetirement::Legacy(file));
    }
    let bytes = read_owned_job(path, &file)?;
    let leased = serde_json::from_slice::<StoreJob>(&bytes)
        .is_ok_and(|job| job.ownership_version == 1 && job.format == JOB_FORMAT);
    Ok(if leased {
        DeferredRetirement::Dead(file)
    } else {
        DeferredRetirement::Legacy(file)
    })
}

fn helper_job_lease(path: &Path, version: u32) -> io::Result<File> {
    if version == 0 {
        return lease_deferred_job(path);
    }
    if version != 1 {
        return Err(io::Error::other("unknown deferred ownership version"));
    }
    #[cfg(unix)]
    {
        inherited_job_lease(path, &env::var(JOB_LEASE_ENV).unwrap_or_default())
    }
    #[cfg(not(unix))]
    Err(io::Error::other("inherited job leases are unsupported"))
}

#[cfg(unix)]
fn inherited_job_lease(path: &Path, tag: &str) -> io::Result<File> {
    let file = sys::clone_inherited_file(tag)?;
    job_identity(path, &file)?;
    sys::try_lock(&file, LockKind::Shared).map_err(io::Error::from)?;
    Ok(file)
}

pub(crate) struct DeferredProtection {
    paths: BTreeSet<PathBuf>,
    identities: BTreeSet<(u64, u64)>,
    leases: Vec<File>,
    complete: Cell<bool>,
    inspected: Cell<usize>,
    spent: Cell<Duration>,
}

impl DeferredProtection {
    pub(crate) fn is_complete(&self) -> bool {
        self.complete.get()
    }

    fn budget(&self) -> bool {
        let n = self.inspected.get().saturating_add(1);
        self.inspected.set(n);
        if n > MAX_PROTECTION_PATHS || self.spent.get() > PROTECTION_TIME {
            self.complete.set(false);
        }
        self.complete.get()
    }

    fn overlaps(&self, path: &Path) -> bool {
        path.ancestors().any(|parent| self.paths.contains(parent))
            || self.paths.range(path.to_path_buf()..).next().is_some_and(|child| child.starts_with(path))
    }

    pub(crate) fn protects_file(&self, path: &Path, meta: &sys::Meta) -> bool {
        !self.is_complete() || !path.is_absolute() || !self.budget()
            || self.overlaps(path) || self.identities.contains(&(meta.dev(), meta.ino()))
    }

    /// Ancestors and hardlink aliases are protected; uncertain subtree inspection declines the mutation.
    pub(crate) fn protects(&self, path: &Path) -> bool {
        let begun = Instant::now();
        let protected = (|| {
            if !self.is_complete() || !path.is_absolute() {
                return true;
            }
            if self.overlaps(path) {
                return true;
            }
            if self.paths.is_empty() {
                return false;
            }
            let mut pending = vec![path.to_path_buf()];
            while let Some(path) = pending.pop() {
                if !self.budget() || begun.elapsed() > PROTECTION_TIME {
                    self.complete.set(false);
                    return true;
                }
                let meta = match sys::lstat(&path) {
                    Ok(meta) => meta,
                    Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                    Err(_) => {
                        self.complete.set(false);
                        return true;
                    }
                };
                if self.identities.contains(&(meta.dev(), meta.ino())) {
                    return true;
                }
                if meta.file_type().is_symlink() {
                    self.complete.set(false);
                    return true;
                }
                if meta.is_dir() {
                    let Ok(entries) = fs::read_dir(&path) else {
                        self.complete.set(false);
                        return true;
                    };
                    for entry in entries {
                        if !self.budget() || begun.elapsed() > PROTECTION_TIME {
                            self.complete.set(false);
                            return true;
                        }
                        let Ok(entry) = entry else {
                            self.complete.set(false);
                            return true;
                        };
                        pending.push(entry.path());
                    }
                }
            }
            false
        })();
        self.spent
            .set(self.spent.get().saturating_add(begun.elapsed()));
        if self.spent.get() > PROTECTION_TIME {
            self.complete.set(false);
        }
        protected || !self.is_complete()
    }

    fn add(&mut self, path: PathBuf, stamp: &StampWire) -> io::Result<()> {
        if !path.is_absolute()
            || path
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
            || !self.budget()
        {
            return Err(io::Error::other("invalid deferred protection path"));
        }
        let stamp = stamp
            .stamp()
            .ok_or_else(|| io::Error::other("invalid deferred stamp"))?;
        self.identities.insert((stamp.dev, stamp.ino));
        match sys::lstat(&path) {
            Ok(meta) => { self.identities.insert((meta.dev(), meta.ino())); },
            Err(err) if err.kind() == io::ErrorKind::NotFound => {},
            Err(err) => return Err(err),
        }
        self.paths.insert(path);
        Ok(())
    }
}

/// The snapshot is not an admission barrier; callers hold the relevant mutation locks through maintenance.
pub(crate) fn deferred_protection(dirs: &Dirs) -> DeferredProtection {
    let mut result = DeferredProtection {
        paths: BTreeSet::new(),
        identities: BTreeSet::new(),
        leases: Vec::new(),
        complete: Cell::new(true),
        inspected: Cell::new(0),
        spent: Cell::new(Duration::ZERO),
    };
    let begun = Instant::now();
    let scan = (|| -> io::Result<()> {
        let entries = match fs::read_dir(dirs.deferred()) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(err),
        };
        let mut bytes_read = 0u64;
        let mut jobs = 0usize;
        for entry in entries {
            if !result.budget() || begun.elapsed() > PROTECTION_TIME {
                return Err(io::Error::other("deferred snapshot budget exhausted"));
            }
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with("job-") || !name.ends_with(".json") {
                continue;
            }
            jobs += 1;
            if jobs > 1024 {
                return Err(io::Error::other("too many deferred jobs"));
            }
            let path = entry.path();
            let file = match sys::open_existing_lock_file(&path) {
                Ok(file) => file,
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => return Err(err),
            };
            let identity = job_identity(&path, &file)?;
            if bytes_read.saturating_add(identity.len()) > MAX_PROTECTION_BYTES {
                return Err(io::Error::other("deferred metadata budget exhausted"));
            }
            let live = match sys::try_lock(&file, LockKind::Exclusive) {
                Ok(()) => false,
                Err(fs::TryLockError::WouldBlock) => true,
                Err(fs::TryLockError::Error(err)) => return Err(err),
            };
            if live {
                sys::try_lock(&file, LockKind::Shared).map_err(io::Error::from)?;
            }
            let bytes = read_owned_job(&path, &file)?;
            bytes_read = bytes_read.saturating_add(bytes.len() as u64);
            if bytes_read > MAX_PROTECTION_BYTES {
                return Err(io::Error::other("deferred metadata budget exhausted"));
            }
            let job: StoreJob = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
            if job.ownership_version > 1
                || job.format != JOB_FORMAT
                || !job.cwd.is_absolute()
                || job.home != dirs.root()
            {
                return Err(io::Error::other("unknown deferred protection record"));
            }
            if job.ownership_version == 1 && !live {
                continue;
            }
            let args: Vec<OsString> = job.args.iter().skip(1).map(OsString::from).collect();
            let parsed = rustc_args::parse(&args)
                .map_err(|reason| io::Error::other(format!("invalid deferred compiler arguments: {reason:?}")))?;
            if job.outputs.is_empty() {
                return Err(io::Error::other("missing deferred outputs"));
            }
            let out = job.cwd.join(&parsed.out_dir);
            for (name, stamp) in &job.outputs {
                if !cache::safe_file_name(name) {
                    return Err(io::Error::other("unsafe deferred output"));
                }
                result.add(out.join(name), stamp)?;
            }
            for (path, stamp) in job.inputs.iter().chain(&job.watched) {
                result.add(path.clone(), stamp)?;
            }
            if !live {
                sys::unlock(&file)?;
                sys::try_lock(&file, LockKind::Shared).map_err(io::Error::from)?;
                job_identity(&path, &file)?;
            }
            if result.leases.len() >= 64 {
                return Err(io::Error::other("too many live deferred leases"));
            }
            result.leases.push(file);
        }
        Ok(())
    })();
    result.spent.set(begun.elapsed());
    if scan.is_err() || result.spent.get() > PROTECTION_TIME {
        result.complete.set(false);
    }
    result
}

fn helper_exe() -> io::Result<PathBuf> {
    helper_beside(&env::current_exe()?)
}

/// Only a binary named `frust-wrap` understands the hidden argument, so another persona hands over to its sibling or not at all.
fn helper_beside(exe: &Path) -> io::Result<PathBuf> {
    let name = format!("frust-wrap{}", env::consts::EXE_SUFFIX);
    if exe.file_name().is_some_and(|n| n == name.as_str()) {
        return Ok(exe.to_path_buf());
    }
    match exe.parent().map(|dir| dir.join(&name)) {
        Some(sibling) if sibling.is_file() => Ok(sibling),
        _ => Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no frust-wrap binary beside the running one",
        )),
    }
}

/// Starts the detached helper. The `keep` files (the held locks) are inherited where the platform allows, so the locks outlive the wrapper.
fn spawn_helper(job: &Path, cwd: &Path, keep: &[&File], lease: &File) -> io::Result<()> {
    let mut cmd = Command::new(helper_exe()?);
    // Preserve the producer directory for compatibility with one-shot helpers.
    cmd.arg(DEFERRED_ARG).arg(cwd.join(job)).current_dir(cwd);
    cmd.env_remove(JOB_LEASE_ENV);
    #[cfg(unix)]
    {
        if !sys::inheritable_clear_of_stdio(keep) {
            return Err(io::Error::other("job lease overlaps standard streams"));
        }
        cmd.env(JOB_LEASE_ENV, sys::inherited_file_tag(lease));
    }
    #[cfg(not(unix))]
    let _ = lease;
    sys::detach_command(&mut cmd, keep);
    cmd.spawn().map(drop)
}

/// Takes the locks the wrapper could not hand over; blocking, because the wrapper releases its own as it returns.
fn relock(job: &StoreJob) -> Vec<File> {
    if job.format != JOB_FORMAT || !job.cwd.is_absolute() {
        return Vec::new();
    }
    let dirs = Dirs::at(job.cwd.join(&job.home));
    let wanted = [
        (&job.slot_lock, None),
        (&job.key_lock, lock::shared_mode(&dirs)),
    ];
    let mut held = Vec::new();
    for (path, mode) in wanted {
        if let Some(path) = path
            && let Ok(file) = lock::open_lock_file(&job.cwd.join(path), mode)
            && sys::lock(&file, LockKind::Exclusive).is_ok()
        {
            held.push(file);
        }
    }
    held
}

/// The job file named by a helper command line, which is exactly `--deferred-store <job>`; a compiler command line (including one through the `frustc` persona) never matches.
fn deferred_entry(args: &[OsString]) -> Option<PathBuf> {
    match args {
        [flag, job] if flag == DEFERRED_ARG => Some(PathBuf::from(job)),
        _ => None,
    }
}

/// The helper never prints and always exits 0; whatever it cannot finish is simply not stored.
fn deferred_main(path: PathBuf) -> i32 {
    match Dirs::try_discover() {
        Ok(dirs) => deferred_main_in(path, &dirs.deferred()),
        Err(_) => 0,
    }
}

fn deferred_main_in(path: PathBuf, deferred: &Path) -> i32 {
    let Some(file) = JobFile::claim(path, deferred) else {
        return 0;
    };
    if let Some(job) = read_job(&file.0) {
        let Ok(_lease) = helper_job_lease(&file.0, job.ownership_version) else {
            return 0;
        };
        let _held = relock(&job);
        compact::at_background_priority(|| run_deferred(&job));
    }
    0
}

/// The key lock and slot lock were inherited from the wrapper (or taken again by `relock`) and stay held until this process exits.
fn run_deferred(job: &StoreJob) {
    run_deferred_with(job, &Config::from_env());
}

fn run_deferred_with(job: &StoreJob, worker_config: &Config) {
    let (Some(plan), Some(frozen), Ok(spawned_ns)) = (
        Plan::from_job(job, worker_config),
        Frozen::from_job(job),
        job.spawned_ns.parse::<i128>(),
    ) else {
        return;
    };
    let dep_info = (frozen.dep_name.as_str(), frozen.dep_text.as_str());
    let _ = plan.store_outputs(
        job.captured.as_bytes(),
        job.compile_ms,
        spawned_ns,
        &frozen.names,
        dep_info,
        Some(&frozen),
    );
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

/// Stamps are not part of the key: it names content only.
fn entry_key(prekey: &str, inputs: &[Input], envs: &[(String, Option<String>)]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"frust-entry-payload-v1\0");
    hasher.update(prekey.as_bytes());
    for input in inputs {
        hasher.update(b"\0i");
        hasher.update(input.path.as_bytes());
        hasher.update(input.hash.as_bytes());
    }
    for (name, value) in envs {
        hasher.update(b"\0e");
        hasher.update(name.as_bytes());
        hasher.update(value.as_deref().unwrap_or("<unset>").as_bytes());
    }
    hasher.finalize().to_hex()[..40].to_string()
}

/// Inputs without a recorded stamp were modified most recently, then by descending recorded mtime; an edited file is the likeliest mismatch, so a miss ends early.
fn newest_first(inputs: &[Input]) -> Vec<&Input> {
    let mut order: Vec<&Input> = inputs.iter().collect();
    order.sort_by_key(|input| std::cmp::Reverse(input.stamp.map_or(i64::MAX, |s| s.mtime_ns)));
    order
}

/// How one input of a candidate was judged.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// The content was read and equals the recorded hash.
    Match,
    /// The recorded stamp still matches and the content was not read.
    Trusted,
    /// The recorded stamp matched and a fresh content hash agreed (audit mode, or an audit of a trusted input).
    Confirmed,
    Mismatch,
    /// The stamp matched but the fresh content hash differs from the recorded one: the stamp lied.
    Violation {
        fresh: String,
        dev: u64,
    },
}

/// Decides per volume whether recorded stamps are believed, and files the evidence when one proved wrong.
struct TrustGate {
    mode: TrustMode,
    dirs: Dirs,
    debug: bool,
    // Quarantine and filesystem verdicts belong to this job only.
    volumes: Mutex<HashMap<u64, bool>>,
}

impl TrustGate {
    fn new(mode: TrustMode, dirs: &Dirs, debug: bool) -> TrustGate {
        TrustGate {
            mode,
            dirs: dirs.clone(),
            debug,
            volumes: Mutex::new(HashMap::new()),
        }
    }

    fn volumes(&self) -> MutexGuard<'_, HashMap<u64, bool>> {
        self.volumes.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `path` lies on volume `dev`; stamps there count only if the filesystem is suitable and the volume is not quarantined.
    fn volume_trusted(&self, dev: u64, path: &Path) -> bool {
        if self.mode == TrustMode::Off {
            return false;
        }
        if let Some(&known) = self.volumes().get(&dev) {
            return known;
        }
        let trusted = hashing::fs_trusted(path) && !cache::quarantined(&self.dirs, dev);
        self.volumes().insert(dev, trusted);
        trusted
    }

    /// Recording follows trust, except that `off` keeps writing stamps as it always has.
    fn records_stamps(&self, dev: u64, path: &Path) -> bool {
        self.mode == TrustMode::Off || self.volume_trusted(dev, path)
    }

    fn violation(&self, krate: &str, dev: u64, input: &Input, fresh: &str) {
        if self.debug {
            eprintln!(
                "frust-wrap: stamp violation in {krate}: {} matched its stamp but its content changed",
                input.path
            );
        }
        cache::report_violation(&self.dirs, krate, &input.path, &input.hash, fresh, dev);
        self.volumes().insert(dev, false);
    }
}

/// The hasher believes a stamp exactly where the gate does, so a distrusted volume is never served from a memo.
fn stamp_policy(gate: &Arc<TrustGate>) -> StampTrust {
    let gate = Arc::clone(gate);
    Box::new(move |dev, path| gate.volume_trusted(dev, path))
}

/// A recorded stamp that still matches proves the file unchanged since its hash was taken; anything else is decided by content.
fn input_matches(
    hasher: &Hasher,
    path: &Path,
    persist: bool,
    input: &Input,
    gate: &TrustGate,
    now_ns: i128,
) -> Verdict {
    if gate.mode != TrustMode::Off
        && let Some(record) = &input.stamp
        && let Ok((current, birth)) = Stamp::read_with_birth(path)
        && record.trusts(&current, birth, now_ns)
        && gate.volume_trusted(current.dev, path)
    {
        return if gate.mode == TrustMode::Audit {
            audit_input(path, input, &current)
        } else {
            Verdict::Trusted
        };
    }
    match hasher.hash(path, persist) {
        Ok(hashed) if hashed.hash == input.hash => Verdict::Match,
        _ => Verdict::Mismatch,
    }
}

/// Reads the content past every memo; a change after `current` was taken is an edit, not a lying stamp.
fn audit_input(path: &Path, input: &Input, current: &Stamp) -> Verdict {
    match hashing::hash_with_stamp(path) {
        Ok(hashed) if hashed.stable && hashed.stamp == *current => {
            if hashed.hash == input.hash {
                Verdict::Confirmed
            } else {
                Verdict::Violation {
                    fresh: hashed.hash,
                    dev: current.dev,
                }
            }
        }
        _ => Verdict::Mismatch,
    }
}

/// The stamp is checked again first, so a file edited since the lookup is a mismatch rather than a violation.
fn audit_trusted(path: &Path, input: &Input, now_ns: i128) -> Verdict {
    match (&input.stamp, Stamp::read_with_birth(path)) {
        (Some(record), Ok((current, birth))) if record.trusts(&current, birth, now_ns) => {
            audit_input(path, input, &current)
        }
        _ => Verdict::Mismatch,
    }
}

/// Candidates with this many trusted inputs or fewer have all of them audited.
const AUDIT_ALL_AT_OR_BELOW: usize = 4;

/// FNV-1a over the bytes, started from `seed` and folded once so the low bits depend on the high ones.
fn mix(seed: u64, bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64 ^ seed;
    for &b in bytes {
        h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
    }
    h ^ (h >> 32)
}

fn audit_seed() -> u64 {
    mix(
        u64::from(std::process::id()),
        &hashing::now_ns().to_le_bytes(),
    )
}

/// Positions in `trusted` to re-hash: all of them for a small candidate, otherwise the one whose seeded path hash is lowest.
fn audit_selection(trusted: &[&Path], seed: u64) -> Vec<usize> {
    if trusted.len() <= AUDIT_ALL_AT_OR_BELOW {
        return (0..trusted.len()).collect();
    }
    trusted
        .iter()
        .enumerate()
        .min_by_key(|(_, path)| mix(seed, path.as_os_str().as_encoded_bytes()))
        .map(|(i, _)| vec![i])
        .unwrap_or_default()
}

/// Stops handing out work after the first failure; a panicking worker counts as one.
fn all_parallel<T: Sync>(items: &[T], check: impl Fn(&T) -> bool + Sync) -> bool {
    let budget = worker_budget(VERIFY_THREADS.min(items.len()));
    let threads = budget.threads;
    if threads <= 1 {
        return std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| items.iter().all(check)))
            .unwrap_or(false);
    }
    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let worker = || {
        while !failed.load(Ordering::Relaxed) {
            let Some(item) = items.get(next.fetch_add(1, Ordering::Relaxed)) else {
                break;
            };
            if !check(item) {
                failed.store(true, Ordering::Relaxed);
            }
        }
    };
    thread::scope(|scope| {
        let handles: Vec<_> = (1..threads).map(|_| scope.spawn(worker)).collect();
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(worker)).is_err() {
            failed.store(true, Ordering::Relaxed);
        }
        for handle in handles {
            if handle.join().is_err() {
                failed.store(true, Ordering::Relaxed);
            }
        }
    });
    !failed.load(Ordering::Relaxed)
}

/// `FRUST_CACHE_INCREMENTAL=off` (or `0`, `false`, `no`) keeps incremental units uncached.
fn incremental_units_enabled() -> bool {
    !matches!(
        env::var("FRUST_CACHE_INCREMENTAL").as_deref(),
        Ok("0" | "off" | "false" | "no")
    )
}

/// `FRUST_CACHE_PRIMARY_INCREMENTAL=off` (or `0`, `false`, `no`) leaves primary-package incremental compiles to rustc.
fn primary_incremental_enabled() -> bool {
    !matches!(
        env::var("FRUST_CACHE_PRIMARY_INCREMENTAL").as_deref(),
        Ok("0" | "off" | "false" | "no")
    )
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
        .or_else(|| {
            sys::home_var()
                .filter(|v| !v.is_empty())
                .map(|h| PathBuf::from(h).join(".cargo"))
        });
    match home {
        Some(home) => vec![
            home.join("registry").join("src"),
            home.join("git").join("checkouts"),
        ],
        None => Vec::new(),
    }
}

/// The variables that enter the pre-key, sorted; names and values that are not valid Unicode are kept by their raw bytes rather than aborting the wrapper.
fn keyed_env(vars: impl IntoIterator<Item = (OsString, OsString)>) -> Vec<(OsString, OsString)> {
    let mut kept: Vec<(OsString, OsString)> = vars
        .into_iter()
        .filter(|(name, _)| {
            let name = name.as_encoded_bytes();
            let starts = |prefix: &str| name.starts_with(prefix.as_bytes());
            (starts("CARGO_")
                && !ENV_DENY_EXACT.iter().any(|exact| exact.as_bytes() == name)
                && !ENV_DENY_PREFIXES.iter().any(|prefix| starts(prefix)))
                || ENV_AFFECTS_OUTPUT
                    .iter()
                    .any(|exact| exact.as_bytes() == name)
        })
        .collect();
    kept.sort_by(|a, b| {
        (a.0.as_encoded_bytes(), a.1.as_encoded_bytes())
            .cmp(&(b.0.as_encoded_bytes(), b.1.as_encoded_bytes()))
    });
    kept
}

struct KeyContext<'a> {
    hasher: &'a Hasher,
    cwd: &'a Path,
    profile: &'a str,
    profile_abs: &'a Path,
}

fn extern_hash_threads(inputs: usize, cpus: usize) -> usize {
    // Up to eight externs stay inline to avoid thread startup for small units; leave half the CPUs for cargo's other jobs.
    inputs.div_ceil(8).clamp(1, 4).min((cpus / 2).clamp(1, 4))
}

fn extern_hash_threads_with(
    inputs: usize,
    detached: bool,
    discover_cpus: impl FnOnce() -> usize,
) -> usize {
    if inputs <= 8 || detached {
        1
    } else {
        extern_hash_threads(inputs, discover_cpus())
    }
}

impl KeyContext<'_> {
    fn prekey(
        &self,
        tool: &OsString,
        rest: &[OsString],
        parsed: &RustcArgs,
    ) -> Result<PreKey, String> {
        let inputs = parsed
            .paths
            .iter()
            .filter(|(_, path)| matches!(path, PathArg::Extern { .. }))
            .count();
        let threads = extern_hash_threads_with(
            inputs,
            DETACHED_STORE.load(Ordering::Relaxed),
            || thread::available_parallelism().map_or(1, |n| n.get()),
        );
        self.prekey_with_threads(tool, rest, parsed, threads)
    }

    fn prekey_with_threads(
        &self,
        tool: &OsString,
        rest: &[OsString],
        parsed: &RustcArgs,
        threads: usize,
    ) -> Result<PreKey, String> {
        let mut watched = Vec::new();
        let mut extern_hashes = HashMap::new();
        let mut tools = Vec::new();
        let mut hasher = Both(blake3::Hasher::new(), blake3::Hasher::new());
        hasher.update(b"frust-prekey-v2\0");
        if parsed.unhashed_windows_pdb() {
            hasher.update(b"unhashed-windows-pdb-v1\0");
        }
        hasher.update(
            toolchain_id(tool, self.cwd)
                .ok_or("cannot identify the compiler")?
                .as_bytes(),
        );
        hasher.update(self.cwd.to_string_lossy().as_bytes());

        let mut externs = if threads > 1 {
            let paths: Vec<_> = rest
                .iter()
                .enumerate()
                .filter(|(i, _)| {
                    !parsed.out_dir_args.contains(i) && !parsed.incremental_args.contains(i)
                })
                .filter_map(|(i, _)| {
                    parsed.paths.iter().find_map(|(idx, arg)| match arg {
                        PathArg::Extern { path, .. } if *idx == i => Some(self.cwd.join(path)),
                        _ => None,
                    })
                })
                .collect();
            // Keep errors with their argument so an earlier native input failure still takes precedence.
            Some(
                parallel_with_threads(&paths, threads, |_, path| {
                    Ok(self.hasher.hash(path, path.starts_with(self.profile_abs)))
                })
                .map_err(|e| format!("cannot hash --extern inputs: {e}"))?
                .into_iter(),
            )
        } else {
            None
        };

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
            let specials: Vec<&PathArg> = parsed
                .paths
                .iter()
                .filter(|(idx, _)| *idx == i)
                .map(|(_, p)| p)
                .collect();
            if let Some(PathArg::Extern { name, path }) = specials
                .iter()
                .find(|p| matches!(p, PathArg::Extern { .. }))
            {
                let path = self.cwd.join(path);
                let hashed = match &mut externs {
                    Some(externs) => externs.next().expect("one hash per extern argument"),
                    None => self.hasher.hash(&path, path.starts_with(self.profile_abs)),
                }
                .map_err(|e| format!("cannot read --extern {}: {e}", path.display()))?;
                hasher.update(name.as_bytes());
                hasher.content(hashed.hash.as_bytes());
                hasher.unit_only(
                    path.to_string_lossy()
                        .replace(self.profile, PROFILE_TOKEN)
                        .as_bytes(),
                );
                if path.starts_with(self.profile_abs) {
                    extern_hashes.insert(path.clone(), hashed.hash.clone());
                }
                watched.push((path, hashed.stamp));
                continue;
            }
            if specials.iter().any(|path| matches!(path, PathArg::Tool(_))) {
                hasher.update(text.as_bytes());
            } else {
                hasher.update(text.replace(self.profile, PROFILE_TOKEN).as_bytes());
            }
            for special in specials {
                match special {
                    PathArg::Extern { .. } => {}
                    PathArg::Tool(program) => {
                        let path = self.linker_tool(program)?;
                        let hashed = self
                            .hasher
                            .hash(&path, true)
                            .map_err(|e| format!("cannot read linker {}: {e}", path.display()))?;
                        if !hashed.stable {
                            return Err(format!(
                                "linker {} changed while being hashed",
                                path.display()
                            ));
                        }
                        hasher.update(b"\0tool");
                        hasher.update(path.as_os_str().as_encoded_bytes());
                        hasher.content(hashed.hash.as_bytes());
                        tools.push(path.clone());
                        watched.push((path, hashed.stamp));
                    }
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
                                return Err(format!(
                                    "linker input {} cannot be read",
                                    path.display()
                                ));
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

        for path in tools {
            extern_hashes.remove(&path);
        }

        // A bundled static library outside every -L directory comes from a system location the key does not cover.
        for lib in parsed.libs.iter().filter(|l| l.bundled()) {
            let names = lib.file_names();
            if !native_dirs
                .iter()
                .any(|dir| names.iter().any(|n| dir.join(n).is_file()))
            {
                return Err(format!(
                    "static library `{}` is not in any -L directory",
                    lib.name
                ));
            }
        }

        for (name, value) in keyed_env(env::vars_os()) {
            hasher.update(b"\0v");
            hasher.update(name.as_encoded_bytes());
            match value.to_str() {
                Some(text) => hasher.update(text.replace(self.profile, PROFILE_TOKEN).as_bytes()),
                None => hasher.update(value.as_encoded_bytes()),
            }
        }
        let Both(key, unit) = hasher;
        Ok(PreKey {
            key: key.finalize().to_hex()[..40].to_string(),
            unit: unit.finalize().to_hex()[..40].to_string(),
            watched,
            extern_hashes,
        })
    }

    fn file_hash(
        &self,
        path: &Path,
        watched: &mut Vec<(PathBuf, Stamp)>,
    ) -> Result<String, String> {
        let hashed = self
            .hasher
            .hash(path, false)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        watched.push((path.to_path_buf(), hashed.stamp));
        Ok(hashed.hash)
    }

    fn linker_tool(&self, program: &Path) -> Result<PathBuf, String> {
        if program.is_absolute()
            || program
                .parent()
                .is_some_and(|parent| !parent.as_os_str().is_empty())
        {
            return Ok(self.cwd.join(program));
        }
        program
            .to_str()
            .filter(|name| !name.is_empty())
            .and_then(crate::toolchain::find_on_path)
            .map(|path| self.cwd.join(path))
            .ok_or_else(|| format!("cannot resolve linker {} on PATH", program.display()))
    }

    fn dir_hash(&self, dir: &Path, watched: &mut Vec<(PathBuf, Stamp)>) -> Result<String, String> {
        if let Ok(stamp) = Stamp::read(dir) {
            watched.push((dir.to_path_buf(), stamp));
        }
        match self
            .hasher
            .hash_dir(dir, dir.starts_with(self.profile_abs), watched)
        {
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
                Err(format!(
                    "target specification {} cannot be read",
                    direct.display()
                ))
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
    let output = compiler_command(tool)
        .arg("-vV")
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| crate::paths::short_hash(&output.stdout))
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
        keyed(cwd, extra).map(|p| p.key)
    }

    fn keyed(cwd: &Path, extra: &[&str]) -> Result<PreKey, String> {
        let out_dir = cwd.join("target").join("debug").join("deps");
        let mut args: Vec<OsString> = ["--crate-name", "k", "--emit=dep-info,link", "--out-dir"]
            .iter()
            .map(OsString::from)
            .collect();
        args.push(out_dir.clone().into_os_string());
        args.extend(extra.iter().map(OsString::from));
        let parsed = rustc_args::parse(&args).map_err(|e| format!("{e:?}"))?;
        let profile = rustc_args::profile_root(&out_dir)
            .to_string_lossy()
            .into_owned();
        let hasher = Hasher::ephemeral();
        let ctx = KeyContext {
            hasher: &hasher,
            cwd,
            profile: &profile,
            profile_abs: Path::new(&profile),
        };
        ctx.prekey(&OsString::from("rustc"), &args, &parsed)
    }

    #[test]
    fn explicit_linker_bytes_change_the_key_but_keep_the_unit_in_every_spelling() {
        let s = Scratch::new("linker-content");
        let driver = s.0.join("target").join("debug").join("driver");
        fs::write(&driver, b"driver one").unwrap();
        let option = format!("linker={}", driver.display());
        let short = format!("-C{option}");
        let long = format!("--codegen={option}");
        let spellings = [
            vec!["-C", option.as_str()],
            vec![short.as_str()],
            vec!["--codegen", option.as_str()],
            vec![long.as_str()],
        ];
        let before: Vec<_> = spellings
            .iter()
            .map(|flags| keyed(&s.0, flags).unwrap())
            .collect();
        fs::write(&driver, b"driver two").unwrap();
        for (flags, before) in spellings.iter().zip(&before) {
            let after = keyed(&s.0, flags).unwrap();
            assert_ne!(before.key, after.key, "{flags:?}");
            assert_eq!(
                before.unit, after.unit,
                "content edits retain the unit's version group"
            );
            assert_eq!(before.watched.len(), 1);
            assert_eq!(before.watched[0].0, driver);
            assert_ne!(before.watched[0].1, after.watched[0].1);
            assert!(
                after.extern_hashes.is_empty(),
                "a linker never receives extern replacement tolerance"
            );
        }
        for pair in before.windows(2) {
            assert_ne!(
                pair[0].key, pair[1].key,
                "original argument spelling stays opaque"
            );
            assert_ne!(pair[0].unit, pair[1].unit);
        }
        let other = s.0.join("other-driver");
        fs::write(&other, b"driver one").unwrap();
        let other = keyed(&s.0, &[&format!("-Clinker={}", other.display())]).unwrap();
        assert_ne!(
            before[1].key, other.key,
            "equal bytes at different tool paths are distinct"
        );
        assert_ne!(before[1].unit, other.unit);
    }

    #[test]
    fn explicit_linkers_resolve_relative_paths_and_refuse_missing_tools() {
        let s = Scratch::new("linker-resolve");
        let tools = s.0.join("tools");
        fs::create_dir_all(&tools).unwrap();
        let driver = tools.join("driver");
        fs::write(&driver, b"driver one").unwrap();
        let before = keyed(&s.0, &["-Clinker=tools/driver"]).unwrap();
        assert_eq!(before.watched[0].0, driver);
        fs::write(&driver, b"driver two").unwrap();
        assert_ne!(before.key, key(&s.0, &["-Clinker=tools/driver"]).unwrap());
        assert!(
            key(&s.0, &["-Clinker=tools/missing-driver"])
                .unwrap_err()
                .contains("cannot read linker")
        );
        assert!(
            key(&s.0, &["-Clinker=frust-missing-linker-91e2c054"])
                .unwrap_err()
                .contains("cannot resolve linker")
        );
        assert!(key(&s.0, &["-Clinker="]).is_err());
        assert!(
            key(&s.0, &["-Clinker=./tools"]).is_err(),
            "directories cannot stand in for executable content"
        );
    }

    #[test]
    fn bare_linkers_use_the_actual_path() {
        let cwd = env::current_dir().unwrap();
        let driver =
            crate::toolchain::find_on_path("rustc").expect("the test toolchain is on PATH");
        let keyed = keyed(&cwd, &["-Clinker=rustc"]).unwrap();
        assert_eq!(keyed.watched[0].0, cwd.join(driver));
        assert!(keyed.extern_hashes.is_empty());
    }

    #[test]
    fn a_linker_also_named_as_an_extern_never_gets_replacement_tolerance() {
        let s = Scratch::new("linker-extern");
        let driver =
            s.0.join("target")
                .join("debug")
                .join("deps")
                .join("driver.rlib");
        fs::write(&driver, b"driver").unwrap();
        let linker = format!("-Clinker={}", driver.display());
        let dependency = format!("--extern=driver={}", driver.display());
        for flags in [
            [linker.as_str(), dependency.as_str()],
            [dependency.as_str(), linker.as_str()],
        ] {
            assert!(keyed(&s.0, &flags).unwrap().extern_hashes.is_empty());
        }
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
    fn inherited_jobserver_budget_child() {
        let Some(mode) = env::var_os("FRUST_TEST_JOBSERVER_CHILD") else {
            return;
        };
        // SAFETY: the parent configured valid inherited pipes for this isolated test process.
        let client = JOBSERVER
            .get_or_init(|| unsafe { jobserver::Client::from_env_ext(true).client.ok() })
            .as_ref()
            .expect("inherited jobserver");
        let initial = client.available().unwrap();
        assert_eq!(initial, 2);
        if mode == "probe" {
            return;
        }
        let budget = WorkerBudget::reserve(4, Some(client));
        assert!((1..=3).contains(&budget.threads));
        assert_eq!(client.available().unwrap(), initial - (budget.threads - 1));
        #[cfg(any(target_os = "linux", windows))]
        assert_eq!(
            budget.threads, 3,
            "supported nonblocking clients use only the available tokens"
        );
        drop(budget);
        assert_eq!(client.available().unwrap(), initial);

        let mut held = Vec::new();
        while let Ok(Some(token)) = client.try_acquire() {
            held.push(token);
        }
        let budget = WorkerBudget::reserve(4, Some(client));
        assert_eq!(
            budget.threads, 1,
            "an exhausted or unsupported jobserver stays on the caller"
        );
        drop((budget, held));
        assert_eq!(client.available().unwrap(), initial);
        assert_eq!(worker_budget_for(4, 1, false, None).threads, 1);
        assert_eq!(worker_budget_for(99, 64, false, None).threads, 4);
        assert_eq!(worker_budget_for(4, 8, true, Some(client)).threads, 1);
        assert_eq!(
            client.available().unwrap(),
            initial,
            "a detached store owns no implicit Cargo token"
        );

        let mut probe = compiler_command(&env::current_exe().unwrap().into_os_string());
        probe
            .args([
                "--exact",
                "wrapper::tests::inherited_jobserver_budget_child",
                "--nocapture",
            ])
            .env("FRUST_TEST_JOBSERVER_CHILD", "probe");
        let result = probe.output().unwrap();
        assert!(
            result.status.success(),
            "compiler handoff failed: {}",
            String::from_utf8_lossy(&result.stdout)
        );
        assert_eq!(client.available().unwrap(), initial);
    }

    #[test]
    fn inherited_jobserver_tokens_are_returned_and_compiler_handoff_preserves_them() {
        let client = jobserver::Client::new(2).unwrap();
        let mut child = Command::new(env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "wrapper::tests::inherited_jobserver_budget_child",
                "--nocapture",
            ])
            .env("FRUST_TEST_JOBSERVER_CHILD", "budget");
        client.configure(&mut child);
        let result = child.output().unwrap();
        assert!(
            result.status.success(),
            "jobserver child failed: {}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(client.available().unwrap(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn lent_jobserver_slot_child() {
        if env::var_os("FRUST_TEST_LENT_SLOT_CHILD").is_none() {
            return;
        }
        sys::signal::set_default(sys::signal::SIGTERM);
        // SAFETY: this isolated child inherits the parent's configured, still-open jobserver pipes.
        let client = JOBSERVER
            .get_or_init(|| unsafe { jobserver::Client::from_env_ext(true).client.ok() })
            .as_ref()
            .expect("inherited jobserver");
        assert_eq!(client.available().unwrap(), 0);
        let lent = lend_cargo_slot(Some(client)).unwrap();
        assert!(
            lent.is_none(),
            "an arbitrary parent cannot supply Cargo's completion barrier"
        );
        assert_eq!(client.available().unwrap(), 0);
        println!("frust-lent-slot-ready");
        io::stdout().flush().unwrap();
        let mut command = [0];
        assert_eq!(io::Read::read(&mut io::stdin(), &mut command).unwrap(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn killed_key_waiter_does_not_inflate_its_surviving_jobserver() {
        struct ReapChild {
            child: std::process::Child,
            reaped: bool,
        }
        impl Drop for ReapChild {
            fn drop(&mut self) {
                if !self.reaped {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                }
            }
        }
        let mut capacities = Vec::new();
        for signal in ["SIGTERM", "SIGKILL"] {
            let client = jobserver::Client::new(1).unwrap();
            assert_eq!(client.available().unwrap(), 1);
            // Cargo owns this assigned slot and returns it when its child terminates.
            let assigned = client.acquire().unwrap();
            assert_eq!(client.available().unwrap(), 0);
            let mut command = Command::new(env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "wrapper::tests::lent_jobserver_slot_child",
                    "--nocapture",
                ])
                .env("FRUST_TEST_LENT_SLOT_CHILD", "1")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped());
            client.configure(&mut command);
            let mut child = ReapChild {
                child: command.spawn().unwrap(),
                reaped: false,
            };
            let output = child.child.stdout.take().unwrap();
            let (send, ready) = std::sync::mpsc::channel();
            let reader = thread::spawn(move || {
                let mut observed = String::new();
                for line in BufReader::new(output).lines() {
                    match line {
                        Ok(line) if line.contains("frust-lent-slot-ready") => {
                            let _ = send.send(Ok(()));
                            return;
                        }
                        Ok(line) => {
                            observed.push_str(&line);
                            observed.push('\n');
                        }
                        Err(error) => {
                            let _ = send.send(Err(error.to_string()));
                            return;
                        }
                    }
                }
                let _ = send.send(Err(format!(
                    "child exited before lending its slot: {observed}"
                )));
            });
            ready
                .recv_timeout(Duration::from_secs(10))
                .expect("child did not report its slot handoff")
                .unwrap();
            reader.join().unwrap();
            assert_eq!(
                client.available().unwrap(),
                0,
                "an unguarded child must retain its assigned slot"
            );
            if signal == "SIGTERM" {
                sys::signal::forward(child.child.id(), sys::signal::SIGTERM);
            } else {
                child.child.kill().unwrap();
            }
            let deadline = Instant::now() + Duration::from_secs(10);
            let status = loop {
                if let Some(status) = child.child.try_wait().unwrap() {
                    child.reaped = true;
                    break status;
                }
                assert!(
                    Instant::now() < deadline,
                    "{signal} did not terminate the child"
                );
                thread::sleep(Duration::from_millis(1));
            };
            assert!(
                !status.success(),
                "{signal} must bypass the child's normal return"
            );
            if signal == "SIGTERM" {
                assert_eq!(sys::exit_code(status), 128 + sys::signal::SIGTERM);
            }
            drop(assigned);
            capacities.push((signal, client.available().unwrap()));
        }
        assert_eq!(
            capacities,
            [("SIGTERM", 1), ("SIGKILL", 1)],
            "termination must not mint jobserver capacity"
        );
    }

    struct TestSlot<'a> {
        client: &'a jobserver::Client,
        released: bool,
    }

    impl<'a> TestSlot<'a> {
        fn release(client: &'a jobserver::Client) -> io::Result<Option<Self>> {
            client.release_raw()?;
            Ok(Some(Self {
                client,
                released: true,
            }))
        }
    }

    impl SlotLoan for TestSlot<'_> {
        fn resume(&mut self) -> io::Result<()> {
            while self.released {
                match self.client.acquire_raw() {
                    Ok(()) => self.released = false,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error),
                }
            }
            Ok(())
        }
    }

    impl Drop for TestSlot<'_> {
        fn drop(&mut self) {
            let _ = self.resume();
        }
    }

    #[test]
    fn uncertain_slot_accounting_stops_key_waiting_before_work_can_resume() {
        struct FailedSlot;
        impl SlotLoan for FailedSlot {
            fn resume(&mut self) -> io::Result<()> {
                Err(io::Error::other("repayment failed"))
            }
        }
        let s = Scratch::new("key-slot-fatal");
        let path = s.0.join("key");
        let open = || {
            File::options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
        };
        let authorized = wait_key_with_slot::<FailedSlot>(
            open().unwrap(),
            Duration::from_secs(1),
            || Err(io::Error::other("authorization uncertain")),
            |_, _| panic!("uncertain authorization must not start waiting"),
            || panic!("uncertain authorization must not reopen the key"),
        );
        assert!(matches!(authorized, Err(KeyWaitError::Loan(_))));
        let repaid = wait_key_with_slot(
            open().unwrap(),
            Duration::from_secs(1),
            || Ok(Some(FailedSlot)),
            |file, _| {
                assert!(try_key_lock(file).unwrap());
                Ok(true)
            },
            || panic!("failed repayment must not reopen the key"),
        );
        assert!(matches!(repaid, Err(KeyWaitError::Loan(_))));
        assert!(
            try_key_lock(&open().unwrap()).unwrap(),
            "repayment must not retain the key"
        );
    }

    #[test]
    fn key_wait_lends_capacity_and_drops_its_lock_before_reclaiming_the_slot() {
        let s = Scratch::new("key-slot-progress");
        let path = s.0.join("key");
        let open = || {
            File::options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
        };
        let client = jobserver::Client::new(0).unwrap();
        let mut worker = None;
        let acquired = wait_key_with_slot(
            open().unwrap(),
            Duration::from_secs(5),
            || TestSlot::release(&client),
            |file, _| {
                assert_eq!(client.available().unwrap(), 1);
                let token = client.acquire().unwrap();
                assert!(try_key_lock(file).unwrap());
                let inherited = file.try_clone().unwrap();
                let path = path.clone();
                worker = Some(thread::spawn(move || {
                    let _inherited = inherited;
                    let other = File::options().read(true).write(true).open(path).unwrap();
                    assert!(
                        wait_key_lock(&other, Duration::from_secs(2)).unwrap(),
                        "token reacquisition held the key lock"
                    );
                    sys::unlock(&other).unwrap();
                    drop(other);
                    drop(token);
                }));
                Ok(true)
            },
            open,
        )
        .unwrap()
        .expect("key acquired after regaining the slot");
        worker.unwrap().join().unwrap();
        assert_eq!(client.available().unwrap(), 0);
        assert!(!try_key_lock(&open().unwrap()).unwrap());
        drop(acquired);
    }

    #[test]
    fn key_wait_balances_slots_and_unlocks_on_timeout_error_and_unwind() {
        for outcome in ["timeout", "error", "panic"] {
            let s = Scratch::new(&format!("key-slot-{outcome}"));
            let path = s.0.join("key");
            let open = || {
                File::options()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(&path)
            };
            let client = jobserver::Client::new(0).unwrap();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                wait_key_with_slot(
                    open().unwrap(),
                    Duration::from_secs(5),
                    || TestSlot::release(&client),
                    |file, _| {
                        assert_eq!(client.available().unwrap(), 1);
                        assert!(try_key_lock(file).unwrap());
                        match outcome {
                            "timeout" => Ok(false),
                            "error" => Err(io::Error::other("wait failed")),
                            _ => panic!("wait panicked"),
                        }
                    },
                    open,
                )
            }));
            match outcome {
                "timeout" => assert!(result.unwrap().unwrap().is_none()),
                "error" => assert_eq!(result.unwrap().unwrap_err().to_string(), "wait failed"),
                _ => assert!(result.is_err()),
            }
            assert_eq!(client.available().unwrap(), 0, "{outcome}");
            assert!(try_key_lock(&open().unwrap()).unwrap(), "{outcome}");
        }
    }

    #[test]
    fn key_wait_retries_when_a_peer_wins_during_token_reacquisition() {
        let s = Scratch::new("key-slot-retry");
        let path = s.0.join("key");
        let open = || {
            File::options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
        };
        let client = jobserver::Client::new(0).unwrap();
        let (release, released) = std::sync::mpsc::channel();
        let mut released = Some(released);
        let mut worker = None;
        let mut calls = 0;
        let acquired = wait_key_with_slot(
            open().unwrap(),
            Duration::from_secs(5),
            || TestSlot::release(&client),
            |file, cap| {
                calls += 1;
                assert_eq!(client.available().unwrap(), 1);
                if calls == 1 {
                    let token = client.acquire().unwrap();
                    assert!(try_key_lock(file).unwrap());
                    let path = path.clone();
                    let released = released.take().unwrap();
                    worker = Some(thread::spawn(move || {
                        let other = File::options().read(true).write(true).open(path).unwrap();
                        assert!(wait_key_lock(&other, Duration::from_secs(2)).unwrap());
                        drop(token);
                        released.recv_timeout(Duration::from_secs(2)).unwrap();
                        drop(other);
                    }));
                    return Ok(true);
                }
                release.send(()).unwrap();
                wait_key_lock(file, cap)
            },
            open,
        )
        .unwrap()
        .expect("peer released the key");
        worker.unwrap().join().unwrap();
        assert_eq!(calls, 2);
        assert_eq!(client.available().unwrap(), 0);
        drop(acquired);
    }

    #[test]
    fn dynamic_workers_finish_other_inputs_while_one_is_slow_and_preserve_order() {
        let items: Vec<_> = (0..32).collect();
        let finished = AtomicUsize::new(0);
        let (release, released) = std::sync::mpsc::channel();
        let released = Mutex::new(released);
        let results = parallel_with_limit(&items, 2, |i, item| {
            if i == 0 {
                released
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .map_err(io::Error::other)?;
            } else if finished.fetch_add(1, Ordering::Relaxed) == items.len() - 2 {
                release.send(()).unwrap();
            }
            Ok(item * 7)
        })
        .unwrap();
        assert_eq!(results, items.iter().map(|i| i * 7).collect::<Vec<_>>());
        assert_eq!(finished.into_inner(), items.len() - 1);
    }

    #[test]
    fn worker_limit_includes_the_caller_and_every_item_runs_once() {
        let caller = thread::current().id();
        let barrier = std::sync::Barrier::new(4);
        let identities = Mutex::new(std::collections::HashSet::new());
        let seen: Vec<_> = (0..64).map(|_| AtomicUsize::new(0)).collect();
        let values = parallel_with_limit(&seen, 4, |i, visits| {
            identities.lock().unwrap().insert(thread::current().id());
            if i < 4 {
                barrier.wait();
            }
            assert_eq!(visits.fetch_add(1, Ordering::Relaxed), 0);
            Ok(i)
        })
        .unwrap();
        assert_eq!(values, (0..seen.len()).collect::<Vec<_>>());
        let identities = identities.into_inner().unwrap();
        assert_eq!(identities.len(), 4);
        assert!(identities.contains(&caller));
        assert!(
            seen.iter()
                .all(|visits| visits.load(Ordering::Relaxed) == 1)
        );
    }

    #[test]
    fn serial_and_parallel_jobs_turn_panics_into_the_same_io_error() {
        for threads in [1, 3] {
            let error = parallel_with_limit(&[0, 1, 2, 3, 4], threads, |i, _| {
                if i == 2 {
                    panic!("hashing failed");
                }
                Ok(i)
            })
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::Other);
            assert_eq!(error.to_string(), "worker thread panicked");
        }
    }

    #[test]
    fn dynamic_workers_report_the_first_input_failure_after_later_work_fails() {
        let (release, released) = std::sync::mpsc::channel();
        let released = Mutex::new(released);
        let err = parallel_with_limit(&[0, 1, 2, 3], 2, |i, _| -> io::Result<()> {
            if i == 0 {
                released
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .map_err(io::Error::other)?;
                Err(io::Error::other("first input"))
            } else {
                release.send(()).unwrap();
                Err(io::Error::other("later input"))
            }
        })
        .unwrap_err();
        assert_eq!(err.to_string(), "first input");
        assert!(
            parallel_with_limit(&[] as &[u32], 4, |_, _| Ok(()))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn forced_serial_workers_skip_cpu_discovery() {
        for detached in [false, true] {
            for inputs in 0..=8 {
                assert_eq!(
                    extern_hash_threads_with(inputs, detached, || {
                        panic!("serial inputs queried CPUs")
                    }),
                    1
                );
            }
        }
        for inputs in [9, 32, usize::MAX] {
            assert_eq!(
                extern_hash_threads_with(inputs, true, || panic!("detached inputs queried CPUs")),
                1
            );
        }
        for desired in [0, 1] {
            let budget = worker_budget_with(desired, false, None, || {
                panic!("serial budget queried CPUs")
            });
            assert_eq!(budget.threads, 1);
            assert!(budget._tokens.is_empty());
        }
        for desired in [0, 1, 2, 4, usize::MAX] {
            let budget = worker_budget_with(desired, true, None, || {
                panic!("detached store queried CPUs")
            });
            assert_eq!(budget.threads, 1);
            assert!(budget._tokens.is_empty());
        }
    }

    #[test]
    fn parallel_workers_rediscover_cpu_limits_and_preserve_bounds() {
        let calls = std::cell::Cell::new(0);
        for cpus in [1, 2, 4, 6, 8, 64] {
            for inputs in [9, 16, 32, 100, usize::MAX] {
                let before = calls.get();
                let threads = extern_hash_threads_with(inputs, false, || {
                    calls.set(calls.get() + 1);
                    cpus
                });
                assert_eq!(calls.get(), before + 1);
                assert_eq!(threads, extern_hash_threads(inputs, cpus));
                assert!((1..=4).contains(&threads));
            }
            for desired in [2, 4, usize::MAX] {
                let before = calls.get();
                let budget = worker_budget_with(desired, false, None, || {
                    calls.set(calls.get() + 1);
                    cpus
                });
                assert_eq!(calls.get(), before + 1);
                assert_eq!(
                    budget.threads,
                    worker_budget_for(desired, cpus, false, None).threads
                );
                assert!((1..=4).contains(&budget.threads));
                assert!(budget._tokens.is_empty());
            }
        }
    }

    #[test]
    fn extern_hash_workers_are_bounded_and_small_units_stay_inline() {
        assert_eq!(extern_hash_threads(0, 64), 1);
        assert_eq!(extern_hash_threads(8, 64), 1);
        assert_eq!(extern_hash_threads(9, 64), 2);
        assert_eq!(extern_hash_threads(32, 64), 4);
        assert_eq!(extern_hash_threads(100, 1), 1);
        assert_eq!(extern_hash_threads(100, 2), 1);
        assert_eq!(extern_hash_threads(100, 4), 2);
        assert_eq!(extern_hash_threads(100, 6), 3);
        assert_eq!(extern_hash_threads(100, 8), 4);
    }

    #[test]
    fn parallel_extern_hashes_preserve_serial_keys_watches_and_errors() {
        let s = Scratch::new("extern-parallel");
        let deps = s.0.join("target").join("debug").join("deps");
        let profile = rustc_args::profile_root(&deps)
            .to_string_lossy()
            .into_owned();
        let mut args = vec![
            OsString::from("--crate-name=k"),
            OsString::from("--emit=dep-info,link"),
            OsString::from(format!("--out-dir={}", deps.display())),
            OsString::from("-C"),
            OsString::from("link-arg=extra.o"),
        ];
        fs::write(s.0.join("extra.o"), b"object").unwrap();
        for i in [7, 2, 11, 5, 0, 9, 1, 10, 6, 3, 8, 4] {
            let path = if i < 9 {
                deps.join(format!("libdep{i}.rlib"))
            } else {
                s.0.join(format!("libdep{i}.rlib"))
            };
            fs::write(&path, vec![i as u8; (i + 1) * 1024]).unwrap();
            if i % 2 == 0 {
                args.push(OsString::from(format!(
                    "--extern=dep{i}={}",
                    path.display()
                )));
            } else {
                args.push(OsString::from("--extern"));
                args.push(OsString::from(format!("dep{i}={}", path.display())));
            }
        }
        fs::write(s.0.join("custom.json"), b"{}").unwrap();
        args.extend([
            OsString::from("--target=custom.json"),
            OsString::from("-Cincremental=session"),
        ]);
        let prekey = |args: &[OsString], threads| {
            let parsed = rustc_args::parse(args).unwrap();
            let hasher = Hasher::ephemeral();
            let ctx = KeyContext {
                hasher: &hasher,
                cwd: &s.0,
                profile: &profile,
                profile_abs: Path::new(&profile),
            };
            ctx.prekey_with_threads(&OsString::from("rustc"), args, &parsed, threads)
        };
        let serial = prekey(&args, 1).unwrap();
        let parallel = prekey(&args, 4).unwrap();
        assert_eq!(parallel.key, serial.key);
        assert_eq!(parallel.unit, serial.unit);
        assert_eq!(parallel.watched, serial.watched);
        assert_eq!(parallel.extern_hashes, serial.extern_hashes);
        assert_eq!(
            parallel.watched.len(),
            14,
            "twelve externs, an object and a target specification"
        );
        assert_eq!(
            parallel.extern_hashes.len(),
            9,
            "only externs under the profile are tracked for replacement"
        );

        args[4] = OsString::from("link-arg=missing.o");
        args.push(OsString::from("--extern=first=missing-first.rlib"));
        args.push(OsString::from("--extern=second=missing-second.rlib"));
        let serial_error = prekey(&args, 1).err().unwrap();
        assert_eq!(prekey(&args, 4).err().unwrap(), serial_error);
        assert!(serial_error.contains("linker input") && serial_error.contains("missing.o"));
        args[4] = OsString::from("link-arg=extra.o");
        let serial_error = prekey(&args, 1).err().unwrap();
        assert_eq!(prekey(&args, 4).err().unwrap(), serial_error);
        assert!(
            serial_error.contains("cannot read --extern")
                && serial_error.contains("missing-first.rlib")
        );
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
        assert!(
            key(&s.0, &["-C", "link-arg=missing.o"]).is_err(),
            "an unreadable object is not keyed by its name"
        );

        fs::write(s.0.join("custom.json"), b"{}").unwrap();
        let spec = key(&s.0, &["--target", "custom.json"]).unwrap();
        fs::write(s.0.join("custom.json"), b"{ }").unwrap();
        assert_ne!(spec, key(&s.0, &["--target", "custom.json"]).unwrap());
        assert!(key(&s.0, &["--target", "absent.json"]).is_err());
        assert!(key(&s.0, &["--sysroot", "/somewhere"]).is_err());
    }

    fn input(path: &Path, hash: &str, stamp: Option<StampRecord>) -> Input {
        Input {
            path: path.to_string_lossy().into_owned(),
            hash: hash.to_string(),
            stamp,
        }
    }

    /// A hash dated past the settle margin, as one taken seconds after the last write would be.
    fn settled(path: &Path) -> hashing::Hashed {
        let mut hashed = hashing::hash_with_stamp(path).unwrap();
        hashed.at_ns =
            hashed.stamp.mtime_ns.max(hashed.stamp.ctime_ns) + hashing::SETTLE_MARGIN_NS + 1;
        hashed
    }

    #[test]
    fn an_unchanged_stamp_is_accepted_without_reading_and_anything_else_is_hashed() {
        let s = Scratch::new("trust");
        let file = s.0.join("lib.rs");
        fs::write(&file, b"pub fn a() {}").unwrap();
        let hashed = settled(&file);
        let record = StampRecord::of(&hashed).expect("settled files are recorded");
        let now = hashed.at_ns;
        let hasher = Hasher::ephemeral();

        // A deliberately wrong hash proves the content was not consulted.
        let bogus = "0".repeat(64);
        let (on, off) = (gate(&s, TrustMode::On), gate(&s, TrustMode::Off));
        let verdict =
            |path: &Path, hash: &str, stamp: Option<StampRecord>, gate: &TrustGate, at: i128| {
                input_matches(&hasher, path, false, &input(path, hash, stamp), gate, at)
            };
        assert_eq!(
            verdict(&file, &bogus, Some(record), &on, now),
            Verdict::Trusted
        );
        assert_eq!(
            verdict(&file, &bogus, Some(record), &off, now),
            Verdict::Mismatch,
            "the kill switch forces hashing"
        );
        assert_eq!(
            verdict(&file, &bogus, None, &on, now),
            Verdict::Mismatch,
            "no record, no trust"
        );
        assert_eq!(verdict(&file, &hashed.hash, None, &on, now), Verdict::Match);

        fs::write(&file, b"pub fn b() { 1 }").unwrap();
        let edited = blake3::hash(b"pub fn b() { 1 }").to_hex().to_string();
        assert_eq!(
            verdict(&file, &hashed.hash, Some(record), &on, now),
            Verdict::Mismatch,
            "an edit is never served from the old hash"
        );
        assert_eq!(
            verdict(&file, &edited, Some(record), &on, now),
            Verdict::Match,
            "a changed stamp falls back to the content"
        );

        let copy = s.0.join("copy.rs");
        fs::copy(&file, &copy).unwrap();
        let again = settled(&file);
        let (record, later) = (StampRecord::of(&again).unwrap(), again.at_ns);
        assert_eq!(
            verdict(&file, &bogus, Some(record), &on, later),
            Verdict::Trusted,
            "the rewritten file is trusted under its new record"
        );
        assert_eq!(
            verdict(&copy, &bogus, Some(record), &on, later),
            Verdict::Mismatch,
            "another inode never matches a record"
        );
        assert_eq!(
            verdict(&s.0.join("gone.rs"), &bogus, Some(record), &on, later),
            Verdict::Mismatch,
            "a missing file is a mismatch"
        );
    }

    fn gate(s: &Scratch, mode: TrustMode) -> TrustGate {
        TrustGate::new(mode, &Dirs::at(s.0.join("state")), false)
    }

    #[test]
    fn a_stamp_that_lies_is_a_violation_and_quarantines_its_volume() {
        let s = Scratch::new("lie");
        let file = s.0.join("lib.rs");
        fs::write(&file, b"pub fn a() {}").unwrap();
        let hashed = settled(&file);
        let record = StampRecord::of(&hashed).unwrap();
        let (now, dev) = (hashed.at_ns, hashed.stamp.dev);
        let hasher = Hasher::ephemeral();
        let bogus = "0".repeat(64);
        let dirs = Dirs::at(s.0.join("state"));

        let audit = gate(&s, TrustMode::Audit);
        let verdict = |hash: &str, gate: &TrustGate| {
            input_matches(
                &hasher,
                &file,
                false,
                &input(&file, hash, Some(record)),
                gate,
                now,
            )
        };
        assert_eq!(
            verdict(&hashed.hash, &audit),
            Verdict::Confirmed,
            "audit mode reads every predicted input"
        );
        assert_eq!(
            verdict(&bogus, &audit),
            Verdict::Violation {
                fresh: hashed.hash.clone(),
                dev
            }
        );
        assert!(
            !cache::quarantined(&dirs, dev),
            "detection alone writes nothing"
        );

        audit.violation(
            "lib",
            dev,
            &input(&file, &bogus, Some(record)),
            &hashed.hash,
        );
        assert!(cache::quarantined(&dirs, dev));
        assert_eq!(
            verdict(&bogus, &audit),
            Verdict::Mismatch,
            "the gate that saw the violation stops predicting"
        );
        assert_eq!(verdict(&hashed.hash, &audit), Verdict::Match);
        for mode in [TrustMode::On, TrustMode::Audit] {
            let later = gate(&s, mode);
            assert_eq!(
                verdict(&bogus, &later),
                Verdict::Mismatch,
                "a later process also hashes content under {mode:?}"
            );
            assert!(!later.records_stamps(dev, &file), "and records no stamps");
        }
        assert!(
            gate(&s, TrustMode::Off).records_stamps(dev, &file),
            "off records as it always did"
        );
    }

    #[test]
    fn an_audit_of_a_trusted_input_confirms_catches_a_lie_and_ignores_an_edit() {
        let s = Scratch::new("audit");
        let file = s.0.join("lib.rs");
        fs::write(&file, b"pub fn a() {}").unwrap();
        let hashed = settled(&file);
        let record = StampRecord::of(&hashed).unwrap();
        let (now, dev) = (hashed.at_ns, hashed.stamp.dev);
        let bogus = "0".repeat(64);
        assert_eq!(
            audit_trusted(&file, &input(&file, &hashed.hash, Some(record)), now),
            Verdict::Confirmed
        );
        assert_eq!(
            audit_trusted(&file, &input(&file, &bogus, Some(record)), now),
            Verdict::Violation {
                fresh: hashed.hash.clone(),
                dev
            }
        );
        assert_eq!(
            audit_trusted(&file, &input(&file, &hashed.hash, None), now),
            Verdict::Mismatch,
            "an input without a record cannot be audited"
        );
        fs::write(&file, b"pub fn b() { 1 }").unwrap();
        assert_eq!(
            audit_trusted(&file, &input(&file, &bogus, Some(record)), now),
            Verdict::Mismatch,
            "an edit is a miss, not a violation"
        );
    }

    #[test]
    fn audits_cover_every_trusted_input_of_a_small_candidate_and_one_of_a_large_one() {
        let paths: Vec<PathBuf> = (0..12)
            .map(|i| PathBuf::from(format!("/src/file{i}.rs")))
            .collect();
        let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
        for n in 0..=AUDIT_ALL_AT_OR_BELOW {
            assert_eq!(
                audit_selection(&refs[..n], 99),
                (0..n).collect::<Vec<_>>(),
                "{n} trusted inputs"
            );
        }
        assert!(audit_selection(&[], 1).is_empty());
        let mut seen = BTreeSet::new();
        for seed in 0..400u64 {
            let picked = audit_selection(&refs, seed);
            assert_eq!(picked.len(), 1, "seed {seed}");
            assert!(picked[0] < refs.len(), "only a trusted input can be chosen");
            assert_eq!(
                picked,
                audit_selection(&refs, seed),
                "a seed always picks the same input"
            );
            seen.insert(picked[0]);
        }
        assert!(
            seen.len() > 3,
            "different seeds spread over the inputs: {seen:?}"
        );
        let five = &refs[..AUDIT_ALL_AT_OR_BELOW + 1];
        assert_eq!(audit_selection(five, 7).len(), 1);
    }

    #[test]
    fn inputs_are_verified_newest_first() {
        let record = |mtime_ns: i64| StampRecord {
            dev: 1,
            ino: 1,
            size: 1,
            mtime_ns,
            ctime_ns: mtime_ns + 1,
            hashed_ns: mtime_ns + 10_000_000_000,
            birth_ns: None,
        };
        let path = Path::new("p");
        let inputs = vec![
            input(path, "old", Some(record(1_000_000_000_123))),
            input(path, "unrecorded", None),
            input(path, "new", Some(record(9_000_000_000_123))),
            input(path, "mid", Some(record(5_000_000_000_123))),
        ];
        let order: Vec<&str> = newest_first(&inputs)
            .iter()
            .map(|i| i.hash.as_str())
            .collect();
        assert_eq!(order, ["unrecorded", "new", "mid", "old"]);
    }

    #[test]
    fn serial_verification_treats_a_caller_panic_as_a_failed_candidate() {
        let caller = thread::current().id();
        let visits = AtomicUsize::new(0);
        assert!(!all_parallel(&[42], |_| {
            assert_eq!(thread::current().id(), caller);
            visits.fetch_add(1, Ordering::Relaxed);
            panic!("verification failed");
        }));
        assert_eq!(visits.into_inner(), 1);
    }

    #[test]
    fn parallel_verification_checks_each_item_once_and_stops_on_a_failure() {
        let items: Vec<u32> = (0..200).collect();
        let seen = AtomicUsize::new(0);
        assert!(all_parallel(&items, |_| {
            seen.fetch_add(1, Ordering::Relaxed);
            true
        }));
        assert_eq!(seen.load(Ordering::Relaxed), items.len());

        let seen = AtomicUsize::new(0);
        assert!(!all_parallel(&items, |&n| {
            seen.fetch_add(1, Ordering::Relaxed);
            n != 3
        }));
        assert!(seen.load(Ordering::Relaxed) <= items.len());
        assert!(
            all_parallel(&[] as &[u32], |_| false),
            "nothing to check is a match"
        );
        assert!(
            !all_parallel(&items, |&n| n < 199),
            "the last item decides too"
        );
    }

    #[test]
    fn bundled_static_library_must_come_from_a_search_directory() {
        let s = Scratch::new("static");
        assert!(key(&s.0, &["-l", "static=nowhere"]).is_err());
        let lib = s.0.join("ext");
        fs::create_dir_all(&lib).unwrap();
        fs::write(lib.join("libnowhere.a"), b"x").unwrap();
        assert!(
            key(
                &s.0,
                &["-l", "static=nowhere", &format!("-L{}", lib.display())]
            )
            .is_ok()
        );
        assert!(
            key(&s.0, &["-l", "dylib=system"]).is_ok(),
            "dynamic libraries are only named in the output"
        );
    }

    fn wire(path: &Path) -> StampWire {
        StampWire::of(&Stamp::read(path).unwrap())
    }

    // A finished compile of crate `k`: one source, one dep-info file and one library, stamped as the wrapper would freeze them.
    fn fixture(s: &Scratch) -> (StoreJob, PathBuf, PathBuf) {
        let deps = s.0.join("target").join("debug").join("deps");
        let src = s.0.join("lib.rs");
        fs::write(&src, b"pub fn a() {}").unwrap();
        let rlib = deps.join("libk.rlib");
        fs::write(&rlib, b"object").unwrap();
        let dep_text = format!(
            "{}: {}\n\n{}:\n",
            deps.join("k.d").display(),
            src.display(),
            src.display()
        );
        fs::write(deps.join("k.d"), &dep_text).unwrap();
        let names = vec!["k.d".to_string(), "libk.rlib".to_string()];
        let mut args: Vec<String> = [
            "rustc",
            "--crate-name",
            "k",
            "--emit=dep-info,link",
            "--out-dir",
        ]
        .iter()
        .map(|a| a.to_string())
        .collect();
        args.push(deps.display().to_string());
        let job = StoreJob {
            format: JOB_FORMAT,
            ownership_version: 0,
            context: StoreContext::capture(&s.0),
            home: s.0.join("home"),
            cwd: s.0.clone(),
            args,
            prekey: "ab".repeat(20),
            unit: None,
            trust: "on".to_string(),
            compile_ms: 7,
            // Far enough ahead that the freshly written source is not "too new" for this compile.
            spawned_ns: (hashing::now_ns() + 10_000_000_000).to_string(),
            captured: String::new(),
            watched: Vec::new(),
            extern_hashes: Vec::new(),
            outputs: names
                .iter()
                .map(|n| (n.clone(), wire(&deps.join(n))))
                .collect(),
            names,
            dep_name: "k.d".to_string(),
            dep_text,
            inputs: vec![(src.clone(), wire(&src))],
            commitments: HashMap::new(),
            slot_lock: None,
            key_lock: None,
        };
        (job, src, rlib)
    }

    fn rows(job: &StoreJob) -> usize {
        Cache::new(&Dirs::at(job.cwd.join(&job.home)))
            .candidates(&job.prekey)
            .len()
    }

    #[test]
    fn store_demand_uses_the_same_deferral_guard_as_the_actual_handoff() {
        let scratch = Scratch::new("store-demand-guard");
        let (job, _, _) = fixture(&scratch);
        let mut config = Config::from_env();
        config.store_sync = false;
        let mut plan = Plan::from_job(&job, &config).unwrap();
        plan.parsed.metadata_only_library = false;
        plan.parsed.extra_filename.clear();
        assert!(plan.can_defer_store());
        plan.cfg.store_sync = true;
        assert!(!plan.can_defer_store());
        plan.cfg.store_sync = false;
        plan.parsed.metadata_only_library = true;
        assert!(!plan.can_defer_store());
        plan.parsed.extra_filename = "-fixture".into();
        assert!(plan.can_defer_store());
    }

    #[test]
    fn a_peer_publishing_between_lookup_and_uncontended_lock_prevents_compiler_launch() {
        let s = Scratch::new("published-before-lock");
        let (job, _, _) = fixture(&s);
        let counter = s.0.join("compiler-launches");
        let mut plan = Plan::from_job(&job, &Config::from_env()).unwrap();
        #[cfg(unix)]
        {
            plan.args = vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf launch >> \"$1\"; exit 1".into(),
                "compiler".into(),
                counter.clone().into_os_string(),
            ];
        }
        #[cfg(windows)]
        {
            let compiler = s.0.join("compiler.cmd");
            fs::write(
                &compiler,
                "@echo off\r\necho launch>>\"%~dp0compiler-launches\"\r\nexit /B 1\r\n",
            )
            .unwrap();
            plan.args = vec![compiler.into_os_string()];
        }
        assert_eq!(plan.compile_and_store(None), 1);
        assert!(fs::metadata(&counter).unwrap().len() > 0);
        fs::remove_file(&counter).unwrap();
        assert_eq!(
            plan.execute_after_miss(|| {
                assert_eq!(rows(&job), 0);
                run_deferred(&job);
                assert_eq!(rows(&job), 1);
            }),
            0
        );
        assert!(
            !counter.exists(),
            "the follower must never launch its compiler"
        );
        assert_eq!(
            plan.waited(),
            0,
            "the key lock was acquired without contention"
        );
    }

    #[test]
    fn a_profile_path_with_spaces_round_trips_through_dep_info_in_its_escaped_form() {
        let old = "/work/my project/target/debug";
        let new = "/other/new place/target/debug";
        let raw = format!(
            "{old}/deps/k.d: src/lib.rs {old}/build/out.rs\n\n{old}/build/out.rs:\n# env-dep:OUT={old}/build\n"
        )
        .replace(old, &escape_spaces(old))
        .replace(&format!("OUT={}", escape_spaces(old)), &format!("OUT={old}"));
        assert!(raw.contains("my\\ project") && raw.contains("OUT=/work/my project/"));

        let stored = dep_info_tokenised(&raw, old);
        assert!(!stored.contains("my"), "{stored}");
        assert_eq!(stored.matches(PROFILE_TOKEN).count(), 4);

        let restored = dep_info_resolved(&stored, new);
        assert!(
            restored.contains("/other/new\\ place/target/debug/deps/k.d: src/lib.rs"),
            "{restored}"
        );
        assert!(
            restored.contains("# env-dep:OUT=/other/new place/target/debug/build\n"),
            "{restored}"
        );
        assert_eq!(dep_info_tokenised(&restored, new), stored);
    }

    #[test]
    fn a_profile_path_without_spaces_is_replaced_exactly_as_before() {
        let profile = "/work/target/debug";
        let raw = format!("{profile}/deps/k.d: {profile}/deps/a.rs\n# env-dep:X={profile}\n");
        assert_eq!(
            dep_info_tokenised(&raw, profile),
            raw.replace(profile, PROFILE_TOKEN)
        );
        let stored = dep_info_tokenised(&raw, profile);
        assert_eq!(
            dep_info_resolved(&stored, "/else/target/debug"),
            raw.replace(profile, "/else/target/debug")
        );
    }

    #[test]
    fn a_stamp_survives_the_wire_with_its_nanoseconds() {
        let stamp = Stamp {
            dev: 3,
            ino: 4,
            size: 5,
            mtime_ns: 1_700_000_000_123_456_789,
            ctime_ns: -42,
        };
        let wire = StampWire::of(&stamp);
        let again: StampWire = serde_json::from_slice(&serde_json::to_vec(&wire).unwrap()).unwrap();
        assert_eq!(again.stamp(), Some(stamp));
        assert!(
            StampWire {
                mtime_ns: "soon".to_string(),
                ..wire
            }
            .stamp()
            .is_none()
        );
    }

    #[test]
    fn a_job_survives_json_and_rebuilds_the_plan_it_came_from() {
        let s = Scratch::new("job");
        let (job, _, _) = fixture(&s);
        let again: StoreJob = serde_json::from_slice(&serde_json::to_vec(&job).unwrap()).unwrap();
        assert_eq!(again, job);

        let plan = Plan::from_job(&again, &Config::from_env()).expect("plan");
        assert_eq!(plan.prekey, job.prekey);
        assert_eq!(plan.cwd, job.cwd);
        assert_eq!(plan.parsed.crate_name, "k");
        assert!(plan.deferred && !plan.cfg.debug);
        assert_eq!(plan.dirs.root(), job.home.as_path());
        assert!(Frozen::from_job(&again).is_some());

        let other_format = StoreJob {
            format: JOB_FORMAT + 1,
            ..job.clone()
        };
        assert!(
            Plan::from_job(&other_format, &Config::from_env()).is_none(),
            "a job of another format stores nothing"
        );
        let no_args = StoreJob {
            args: vec!["rustc".to_string()],
            ..job.clone()
        };
        assert!(Plan::from_job(&no_args, &Config::from_env()).is_none());
        let mut bad = job.clone();
        bad.watched.push((
            s.0.join("lib.rs"),
            StampWire {
                ctime_ns: "x".to_string(),
                ..bad.inputs[0].1.clone()
            },
        ));
        assert!(
            Plan::from_job(&bad, &Config::from_env()).is_none(),
            "an unreadable stamp is never guessed"
        );
        bad.inputs[0].1.mtime_ns = "x".to_string();
        assert!(Frozen::from_job(&bad).is_none());
    }

    #[test]
    fn broker_jobs_validate_context_names_and_preserve_the_received_key_lock() {
        let s = Scratch::new("broker-decode");
        let (job, _, _) = fixture(&s);
        let dirs = Dirs::at(&job.home);
        let encode = |job: &StoreJob| serde_json::to_vec(job).unwrap();
        let key_path = keylock_path(&dirs, &job.prekey);
        let key = open_key_lock(&dirs, &key_path).unwrap();
        assert!(try_key_lock(&key).unwrap());
        let decoded = BrokerJob::decode(&encode(&job), &dirs, Some(&key)).unwrap();
        let other = open_key_lock(&dirs, &key_path).unwrap();
        assert!(!try_key_lock(&other).unwrap());
        decoded.execute(&Config::from_env());
        assert_eq!(rows(&job), 1);
        assert!(!try_key_lock(&other).unwrap());
        let wrong = File::create(s.0.join("wrong-key")).unwrap();
        assert!(BrokerJob::decode(&encode(&job), &dirs, Some(&wrong)).is_err());
        let mutations: [fn(&mut StoreJob); 12] = [
            |job: &mut StoreJob| job.home = PathBuf::from("relative-home"),
            |job: &mut StoreJob| job.cwd = PathBuf::from("relative-cwd"),
            |job: &mut StoreJob| {
                job.context
                    .immutable_source_roots
                    .push(PathBuf::from("relative-root"))
            },
            |job: &mut StoreJob| job.names.push("../escape".to_string()),
            |job: &mut StoreJob| job.names.push(job.names[0].clone()),
            |job: &mut StoreJob| job.dep_name = "missing.d".to_string(),
            |job: &mut StoreJob| job.outputs.reverse(),
            |job: &mut StoreJob| job.slot_lock = Some(PathBuf::from("slot-0")),
            |job: &mut StoreJob| job.key_lock = Some(PathBuf::from("another-key")),
            |job: &mut StoreJob| job.prekey = "../bad-key".to_string(),
            |job: &mut StoreJob| job.trust = "unknown".to_string(),
            |job: &mut StoreJob| job.format -= 1,
        ];
        for mutate in mutations {
            let mut malformed = job.clone();
            mutate(&mut malformed);
            assert!(BrokerJob::decode(&encode(&malformed), &dirs, None).is_err());
        }
        assert!(BrokerJob::decode(&encode(&job), &Dirs::at(s.0.join("other-home")), None).is_err());
    }

    #[test]
    fn producer_marker_cleanup_never_removes_an_independent_broker_marker() {
        let s = Scratch::new("broker-marker");
        let (job, _, _) = fixture(&s);
        let deferred = Dirs::at(&job.home).deferred();
        lock::ensure_dir(&deferred, None).unwrap();
        for outcome in [Some(true), Some(false), None] {
            let producer = write_job(&deferred, &job).unwrap();
            let independent = deferred.join(format!(
                "job-{}-broker-independent.json",
                std::process::id()
            ));
            let result = submit_broker_job(&producer, |path| {
                fs::write(&independent, fs::read(path).unwrap()).unwrap();
                match outcome {
                    Some(accepted) => Ok(accepted),
                    None => Err(io::Error::new(io::ErrorKind::TimedOut, "ACK unknown")),
                }
            });
            assert!(!producer.exists());
            assert!(independent.is_file());
            assert_eq!(read_job(&independent).unwrap(), job);
            match outcome {
                Some(accepted) => assert_eq!(result.unwrap(), accepted),
                None => assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut),
            }
        }
    }

    #[test]
    fn broker_capability_fallback_child() {
        let Some(home) = env::var_os("FRUST_TEST_BROKER_FALLBACK_HOME") else {
            return;
        };
        let dirs = Dirs::at(home);
        assert!(
            crate::store_broker::AdmissionPermit::try_reserve(&dirs)
                .unwrap()
                .is_none()
        );
        let (held, _) = acquire_slot(&dirs.deferred(), 1).unwrap();
        assert!(acquire_slot(&dirs.deferred(), 1).is_none());
        drop(held);
        assert!(acquire_slot(&dirs.deferred(), 1).is_some());
    }

    #[test]
    fn direct_and_foreign_builds_keep_legacy_slots_without_a_valid_capability() {
        let s = Scratch::new("broker-fallback");
        for capability in [None, Some("untrusted-foreign-capability")] {
            let mut child = Command::new(env::current_exe().unwrap());
            child
                .args([
                    "--exact",
                    "wrapper::tests::broker_capability_fallback_child",
                    "--nocapture",
                ])
                .env("FRUST_TEST_BROKER_FALLBACK_HOME", s.0.join("home"))
                .env_remove(crate::store_broker::CAPABILITY_ENV);
            if let Some(value) = capability {
                child.env(crate::store_broker::CAPABILITY_ENV, value);
            }
            let result = child.output().unwrap();
            assert!(
                result.status.success(),
                "{}{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
        }
    }

    fn relative_job(s: &Scratch) -> (StoreJob, PathBuf, PathBuf) {
        let (mut job, src, rlib) = fixture(s);
        *job.args.last_mut().unwrap() = "target/debug/deps".to_string();
        job.home = PathBuf::from("cache-home");
        job.dep_text = "target/debug/deps/k.d: lib.rs\n\nlib.rs:\n".to_string();
        fs::write(rlib.with_file_name("k.d"), &job.dep_text).unwrap();
        job.outputs = job
            .names
            .iter()
            .map(|name| (name.clone(), wire(&rlib.with_file_name(name))))
            .collect();
        (job, src, rlib)
    }

    #[test]
    fn sequential_deferred_jobs_use_their_own_paths_versions_and_compression() {
        let first = Scratch::new("context-first");
        let second = Scratch::new("context-second");
        let cwd = env::current_dir().unwrap();
        let environment: std::collections::BTreeMap<_, _> = env::vars_os().collect();
        let mut worker_config = Config::from_env();
        worker_config.debug = true;
        worker_config.keylock_wait = Duration::from_millis(123);
        for (scratch, version, compress) in [(&first, "1.2.3", false), (&second, "4.5.6", true)] {
            let (mut job, _, rlib) = relative_job(scratch);
            let expected = vec![b'p'; cache::payload::MIN_BYTES as usize * 2 + 13];
            fs::write(&rlib, &expected).unwrap();
            #[cfg(unix)]
            sys::set_path_mode(&rlib, 0o751).unwrap();
            job.outputs[1].1 = wire(&rlib);
            job.context = StoreContext {
                package_version: version.to_string(),
                compression_requested: compress,
                immutable_source_roots: vec![job.cwd.join("immutable")],
            };
            let plan = Plan::from_job(&job, &worker_config).unwrap();
            assert!(!plan.cfg.debug);
            assert!(worker_config.debug);
            assert_eq!(plan.cfg.keylock_wait, worker_config.keylock_wait);
            assert_eq!(plan.profile, "target/debug");
            assert_eq!(plan.output_dir_abs, job.cwd.join("target/debug/deps"));
            assert_eq!(plan.dirs.root(), job.cwd.join("cache-home"));
            run_deferred_with(&job, &worker_config);
            let candidates = plan.cache.candidates(&job.prekey);
            assert_eq!(candidates.len(), 1);
            assert_eq!(candidates[0].inputs[0].path, "lib.rs");
            let (entry, meta) = plan.cache.load_entry(&candidates[0].key).unwrap();
            assert_eq!(meta.version, version);
            assert_eq!(meta.prekey.as_ref(), Some(&job.prekey));
            let binary = meta
                .files
                .iter()
                .find(|file| file.name == "libk.rlib")
                .unwrap();
            assert_eq!(
                !binary.encoding.is_raw(),
                compress && cache::payload::compression_supported(&entry)
            );
            let restored = scratch.0.join("restored");
            cache::payload::restore(&entry.join(&binary.stored), &restored, binary).unwrap();
            assert_eq!(fs::read(&restored).unwrap(), expected);
            assert_eq!(
                sys::lstat(&restored).unwrap().mode(),
                sys::lstat(&rlib).unwrap().mode()
            );
            let dep = meta.files.iter().find(|file| file.name == "k.d").unwrap();
            assert!(
                fs::read_to_string(entry.join(&dep.stored))
                    .unwrap()
                    .contains(PROFILE_TOKEN)
            );
        }
        assert_eq!(env::current_dir().unwrap(), cwd);
        assert_eq!(
            env::vars_os().collect::<std::collections::BTreeMap<_, _>>(),
            environment
        );
    }

    #[test]
    fn frozen_compression_uses_the_captured_policy_and_absolute_inputs() {
        let s = Scratch::new("context-freeze");
        let (mut job, src, rlib) = relative_job(&s);
        fs::write(&rlib, vec![b'f'; cache::payload::MIN_BYTES as usize * 2]).unwrap();
        let cfg = Config::from_env();
        let captured = ["k.d", "libk.rlib"]
            .iter()
            .map(|name| {
                format!(
                    "{}\n",
                    serde_json::json!({"artifact": format!("target/debug/deps/{name}")})
                )
            })
            .collect::<String>();
        for compress in [false, true] {
            job.context.compression_requested = compress;
            let plan = Plan::from_job(&job, &cfg).unwrap();
            fs::create_dir_all(plan.dirs.tmp()).unwrap();
            let supported = cache::payload::compression_supported(&plan.dirs.tmp());
            let profile = Stamp::read(&plan.profile_abs).unwrap();
            if !plan.trust.volume_trusted(profile.dev, &plan.profile_abs)
                || job
                    .inputs
                    .iter()
                    .any(|(_, stamp)| stamp.stamp().unwrap().coarse())
                || job
                    .names
                    .iter()
                    .any(|name| Stamp::read(&rlib.with_file_name(name)).unwrap().coarse())
            {
                return;
            }
            let mut captures = 0;
            let frozen = plan
                .freeze_with_capture(
                    captured.as_bytes(),
                    job.spawned_ns.parse().unwrap(),
                    &HashMap::new(),
                    |path| {
                        assert!(path.is_absolute());
                        captures += 1;
                        cache::payload::capture(path)
                    },
                )
                .unwrap();
            assert_eq!(captures, usize::from(compress && supported));
            assert_eq!(frozen.commitments.len(), captures);
            assert!(frozen.inputs.contains_key(&src));
            assert!(frozen.inputs.keys().all(|path| path.is_absolute()));
        }
    }

    #[test]
    fn deferred_context_and_absolute_identities_are_required() {
        let s = Scratch::new("context-required");
        let (job, _, _) = fixture(&s);
        let cfg = Config::from_env();
        let mut encoded = serde_json::to_value(&job).unwrap();
        encoded.as_object_mut().unwrap().remove("context");
        assert!(serde_json::from_value::<StoreJob>(encoded).is_err());
        let mut old = job.clone();
        old.format = JOB_FORMAT - 1;
        assert!(Plan::from_job(&old, &cfg).is_none());
        let mut relative = job.clone();
        relative.cwd = PathBuf::from("relative");
        assert!(Plan::from_job(&relative, &cfg).is_none());
        relative = job.clone();
        relative.context.immutable_source_roots = vec![PathBuf::from("registry/src")];
        assert!(Plan::from_job(&relative, &cfg).is_none());
        relative = job.clone();
        relative.inputs[0].0 = PathBuf::from("lib.rs");
        assert!(Plan::from_job(&relative, &cfg).is_none());
        let mut context = serde_json::to_value(&job).unwrap();
        context["context"]
            .as_object_mut()
            .unwrap()
            .remove("compression_requested");
        assert!(serde_json::from_value::<StoreJob>(context).is_err());
    }

    #[test]
    fn deferred_jobs_refresh_immutable_roots_and_quarantine_state() {
        let s = Scratch::new("context-state");
        let (mut job, src, _) = fixture(&s);
        let cfg = Config::from_env();
        job.context.immutable_source_roots = vec![s.0.clone()];
        let first = Plan::from_job(&job, &cfg).unwrap();
        assert!(first.hasher.in_immutable_root(&src));
        let stamp = Stamp::read(&src).unwrap();
        first.trust.volume_trusted(stamp.dev, &src);
        assert_eq!(first.trust.volumes().len(), 1);
        cache::quarantine(&first.dirs, stamp.dev);
        job.context.immutable_source_roots.clear();
        let second = Plan::from_job(&job, &cfg).unwrap();
        assert!(!second.hasher.in_immutable_root(&src));
        assert!(second.trust.volumes().is_empty());
        assert!(!Arc::ptr_eq(&first.trust, &second.trust));
        assert!(!second.trust.volume_trusted(stamp.dev, &src));
        assert_eq!(
            second.hasher.hash(&src, false).unwrap().hash,
            cache::hash_file(&src).unwrap()
        );
    }

    #[test]
    fn relative_deferred_jobs_refuse_replaced_inputs_outputs_and_watched_files() {
        let cfg = Config::from_env();
        for changed in ["input", "output", "watched"] {
            let s = Scratch::new(&format!("relative-replaced-{changed}"));
            let (mut job, src, rlib) = relative_job(&s);
            job.watched.push((src.clone(), wire(&src)));
            let replaced = if changed == "output" { &rlib } else { &src };
            if changed == "input" {
                job.watched.clear();
            }
            let replacement = replaced.with_extension("replacement");
            fs::write(&replacement, fs::read(replaced).unwrap()).unwrap();
            sys::rename(&replacement, replaced).unwrap();
            run_deferred_with(&job, &cfg);
            assert_eq!(rows(&job), 0, "a replaced {changed} must not be published");
        }
    }

    #[test]
    fn deferred_input_hashing_stays_on_the_worker_without_global_detachment() {
        assert!(!DETACHED_STORE.load(Ordering::Relaxed));
        let s = Scratch::new("context-serial");
        let (mut job, src, rlib) = relative_job(&s);
        let mut paths = vec![src];
        for i in 0..32 {
            let path = s.0.join(format!("input-{i}.rs"));
            fs::write(&path, vec![b'a'; 128 * 1024]).unwrap();
            paths.push(path);
        }
        job.dep_text = format!(
            "target/debug/deps/k.d: {}\n",
            paths
                .iter()
                .map(|path| path.strip_prefix(&job.cwd).unwrap().display().to_string())
                .collect::<Vec<_>>()
                .join(" ")
        );
        fs::write(rlib.with_file_name("k.d"), &job.dep_text).unwrap();
        job.outputs[0].1 = wire(&rlib.with_file_name("k.d"));
        job.inputs = paths
            .iter()
            .map(|path| (path.clone(), wire(path)))
            .collect();
        let mut plan = Plan::from_job(&job, &Config::from_env()).unwrap();
        let caller = thread::current().id();
        let checked = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&checked);
        plan.hasher = Hasher::new(Some(plan.dirs.hashmemo()), vec![s.0.clone()]).with_stamp_trust(
            Box::new(move |_, _| {
                assert_eq!(thread::current().id(), caller);
                observed.fetch_add(1, Ordering::Relaxed);
                false
            }),
        );
        let frozen = Frozen::from_job(&job).unwrap();
        plan.store_outputs(
            job.captured.as_bytes(),
            job.compile_ms,
            job.spawned_ns.parse().unwrap(),
            &job.names,
            (&job.dep_name, &job.dep_text),
            Some(&frozen),
        )
        .unwrap();
        assert_eq!(rows(&job), 1);
        if paths
            .iter()
            .any(|path| !Stamp::read(path).unwrap().coarse())
        {
            assert!(checked.load(Ordering::Relaxed) > 0);
        }
        assert!(!DETACHED_STORE.load(Ordering::Relaxed));
    }

    #[test]
    fn a_deferred_store_publishes_only_the_state_it_was_given() {
        let s = Scratch::new("deferred-ok");
        let (job, _, rlib) = fixture(&s);
        // Cargo hard-links finished outputs after the wrapper returns; that must not stop the store.
        fs::hard_link(&rlib, rlib.with_file_name("linked.rlib")).unwrap();
        run_deferred(&job);
        assert_eq!(rows(&job), 1);
        let events = fs::read_to_string(Dirs::at(&job.home).ledger()).unwrap_or_default();
        assert!(
            events.is_empty(),
            "the wrapper records the event of a deferred store, not the helper"
        );
    }

    #[test]
    fn encoded_entry_keys_cannot_alias_legacy_raw_entry_keys() {
        let prekey = "ab".repeat(20);
        let inputs = vec![Input {
            path: "src/lib.rs".to_string(),
            hash: "cd".repeat(32),
            stamp: None,
        }];
        let envs = vec![("WATCHED".to_string(), Some("value".to_string()))];
        let mut legacy = blake3::Hasher::new();
        legacy.update(prekey.as_bytes());
        for input in &inputs {
            legacy.update(b"\0i");
            legacy.update(input.path.as_bytes());
            legacy.update(input.hash.as_bytes());
        }
        for (name, value) in &envs {
            legacy.update(b"\0e");
            legacy.update(name.as_bytes());
            legacy.update(value.as_deref().unwrap_or("<unset>").as_bytes());
        }
        let old_key = legacy.finalize().to_hex()[..40].to_string();
        assert_ne!(entry_key(&prekey, &inputs, &envs), old_key);
        let old_empty_key = blake3::hash(prekey.as_bytes()).to_hex()[..40].to_string();
        assert_ne!(entry_key(&prekey, &[], &[]), old_empty_key);
    }

    #[test]
    fn staged_payloads_restore_exact_bytes_and_raw_dep_info() {
        let s = Scratch::new("payload-restore");
        let (job, _, rlib) = fixture(&s);
        let expected = vec![b'k'; 2 * 1024 * 1024 + 333];
        fs::write(&rlib, &expected).unwrap();
        let plan = Plan::from_job(&job, &Config::from_env()).unwrap();
        let staged = plan.cache.stage().unwrap();
        let meta = plan
            .stage_entry(
                &staged,
                &job.names,
                (&job.dep_name, &job.dep_text),
                b"",
                7,
                None,
            )
            .unwrap();
        let binary = meta.files.iter().find(|f| f.name == "libk.rlib").unwrap();
        let dep = meta.files.iter().find(|f| f.name == "k.d").unwrap();
        assert!(dep.text && dep.encoding.is_raw());
        if cache_compression_requested() && cache::payload::compression_supported(&staged) {
            assert!(!binary.encoding.is_raw());
            assert!(binary.stored_bytes.unwrap() < binary.bytes);
        }
        fs::write(&rlib, b"changed").unwrap();
        fs::write(rlib.parent().unwrap().join("k.d"), b"changed").unwrap();
        plan.restore(&staged, &meta).unwrap();
        assert_eq!(fs::read(&rlib).unwrap(), expected);
        assert_eq!(
            fs::read_to_string(rlib.parent().unwrap().join("k.d")).unwrap(),
            job.dep_text
        );
    }

    #[test]
    fn committed_paired_outputs_publish_and_restore_without_capture_files() {
        let s = Scratch::new("paired-publish-restore");
        let (mut job, _, rlib) = fixture(&s);
        let rmeta = rlib.with_file_name("libk.rmeta");
        let metadata = vec![0x5a; 96 * 1024 + 17];
        let mut archive = b"!<arch>\n".to_vec();
        for (name, bytes) in [
            ("lib.rmeta/", metadata.clone()),
            ("code.o/", vec![0x65; 96 * 1024 + 1]),
        ] {
            archive.extend_from_slice(
                format!(
                    "{name:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
                    0,
                    0,
                    0,
                    "644",
                    bytes.len()
                )
                .as_bytes(),
            );
            archive.extend_from_slice(&bytes);
            if bytes.len() % 2 != 0 {
                archive.push(b'\n');
            }
        }
        fs::write(&rlib, &archive).unwrap();
        fs::write(&rmeta, &metadata).unwrap();
        #[cfg(unix)]
        {
            sys::set_path_mode(&rlib, 0o640).unwrap();
            sys::set_path_mode(&rmeta, 0o751).unwrap();
        }
        job.names.push("libk.rmeta".to_string());
        job.outputs = job
            .names
            .iter()
            .map(|name| (name.clone(), wire(&rlib.with_file_name(name))))
            .collect();
        for name in ["libk.rlib", "libk.rmeta"] {
            let (_, commitment) = cache::payload::capture(&rlib.with_file_name(name)).unwrap();
            assert!(commitment.valid());
            job.commitments.insert(name.to_string(), commitment);
        }
        let frozen = Frozen::from_job(&job).unwrap();
        let modes: Vec<_> = job
            .names
            .iter()
            .map(|name| sys::lstat(&rlib.with_file_name(name)).unwrap().mode())
            .collect();
        let plan = Plan::from_job(&job, &Config::from_env()).unwrap();
        let staged = plan.cache.stage().unwrap();
        let compressed =
            cache_compression_requested() && cache::payload::compression_supported(&staged);
        let meta = plan
            .stage_entry(
                &staged,
                &job.names,
                (&job.dep_name, &job.dep_text),
                b"",
                7,
                Some(&frozen.commitments),
            )
            .unwrap();
        let backing = &meta.files[1];
        let target = &meta.files[2];
        assert_eq!(
            meta.files
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>(),
            ["k.d", "libk.rlib", "libk.rmeta"]
        );
        assert!(meta.files[0].text && meta.files[0].encoding.is_raw());
        if compressed && cfg!(unix) {
            assert_eq!(backing.encoding, cache::payload::Encoding::Lz4BlocksV1);
            assert_eq!(
                target.encoding,
                cache::payload::Encoding::InlineMetadataSliceV1
            );
            assert_eq!(target.stored, backing.stored);
            assert_eq!(backing.decoded_bytes, Some(archive.len() as u64));
            assert_eq!(
                backing.content_hash,
                Some(blake3::hash(&archive).to_hex().to_string())
            );
            assert_eq!(target.decoded_bytes, Some(metadata.len() as u64));
            assert_eq!(
                target.content_hash,
                Some(blake3::hash(&metadata).to_hex().to_string())
            );
            let slice = target.metadata_slice.as_ref().unwrap();
            assert_eq!(slice.offset, 68);
            assert_eq!(
                slice.source_encoding,
                cache::metadata_slice::SourceEncoding::Lz4BlocksV1
            );
            assert_eq!(slice.source_decoded_bytes, archive.len() as u64);
            assert_eq!(
                Some(&slice.source_content_hash),
                backing.content_hash.as_ref()
            );
            assert_eq!(slice.mode, modes[2]);
            assert_eq!(target.stored_bytes, Some(0));
        } else {
            assert!(backing.encoding.is_raw() && target.encoding.is_raw());
            assert_ne!(target.stored, backing.stored);
            assert!(backing.content_hash.is_none() && target.content_hash.is_none());
            assert!(backing.metadata_slice.is_none() && target.metadata_slice.is_none());
        }
        let key = "de".repeat(20);
        plan.cache.publish(&staged, &key).unwrap();
        assert!(!staged.exists());
        let entry = plan.cache.entry_dir(&key);
        let mut declared: BTreeSet<String> = meta.files.iter().map(|f| f.stored.clone()).collect();
        declared.extend(["meta.json".to_string(), "stderr".to_string()]);
        let expected: BTreeSet<_> = if compressed && cfg!(unix) {
            ["f0", "f1", "meta.json", "stderr"]
                .into_iter()
                .map(str::to_string)
                .collect()
        } else {
            ["f0", "f1", "f2", "meta.json", "stderr"]
                .into_iter()
                .map(str::to_string)
                .collect()
        };
        assert_eq!(declared, expected);
        let actual: BTreeSet<_> = fs::read_dir(&entry)
            .unwrap()
            .map(|item| {
                let item = item.unwrap();
                assert!(item.file_type().unwrap().is_file());
                item.file_name().into_string().unwrap()
            })
            .collect();
        assert_eq!(actual, declared);
        let published: EntryMeta =
            serde_json::from_slice(&fs::read(entry.join("meta.json")).unwrap()).unwrap();
        assert_eq!(
            serde_json::to_value(&published).unwrap(),
            serde_json::to_value(&meta).unwrap()
        );
        for name in &job.names {
            fs::write(rlib.with_file_name(name), b"later build").unwrap();
        }
        plan.restore(&entry, &published).unwrap();
        assert_eq!(fs::read(&rlib).unwrap(), archive);
        assert_eq!(fs::read(&rmeta).unwrap(), metadata);
        assert_eq!(
            fs::read(rlib.with_file_name("k.d")).unwrap(),
            job.dep_text.as_bytes()
        );
        for (name, mode) in job.names.iter().zip(modes) {
            assert_eq!(sys::lstat(&rlib.with_file_name(name)).unwrap().mode(), mode);
        }
    }

    #[test]
    fn malformed_encoded_flags_reject_before_any_destination_is_changed() {
        let s = Scratch::new("payload-flags");
        let (job, _, rlib) = fixture(&s);
        let mut plan = Plan::from_job(&job, &Config::from_env()).unwrap();
        let staged = plan.cache.stage().unwrap();
        let mut meta = plan
            .stage_entry(
                &staged,
                &job.names,
                (&job.dep_name, &job.dep_text),
                b"",
                7,
                None,
            )
            .unwrap();
        let binary = meta
            .files
            .iter()
            .position(|file| file.name == "libk.rlib")
            .unwrap();
        let dep_info = rlib.parent().unwrap().join("k.d");
        let sibling = rlib.parent().unwrap().join("unrelated-output");
        fs::write(&rlib, b"existing artifact").unwrap();
        fs::write(&dep_info, b"existing dep-info").unwrap();
        fs::write(&sibling, b"existing sibling").unwrap();
        fs::write(
            staged.join(&meta.files[binary].stored),
            b"UTF-8 text that is not an encoded payload",
        )
        .unwrap();
        meta.files[binary].encoding = cache::payload::Encoding::Lz4BlocksV1;
        meta.files[binary].decoded_bytes = Some(1);
        meta.files[binary].content_hash = Some("0".repeat(64));
        for (text, dir) in [(true, false), (false, true), (true, true)] {
            meta.files[binary].text = text;
            meta.files[binary].dir = dir;
            assert_eq!(
                plan.restore(&staged, &meta).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
            assert_eq!(fs::read(&rlib).unwrap(), b"existing artifact");
            assert_eq!(fs::read(&dep_info).unwrap(), b"existing dep-info");
            assert_eq!(fs::read(&sibling).unwrap(), b"existing sibling");
        }
        plan.parsed.out_dir = s.0.join("absent-output-directory");
        assert!(plan.restore(&staged, &meta).is_err());
        assert!(!plan.parsed.out_dir.exists());
    }

    #[test]
    fn many_store_inputs_keep_dep_info_order_and_reject_later_edits() {
        let s = Scratch::new("store-many");
        let (mut job, src, _) = fixture(&s);
        let mut files = vec![src];
        for i in (0..33).rev() {
            let path = s.0.join(format!("input-{i}.rs"));
            fs::write(&path, format!("pub fn value() -> usize {{ {i} }}\n")).unwrap();
            files.push(path);
        }
        let deps = PathBuf::from(job.args.last().unwrap());
        job.dep_text = format!(
            "{}: {}\n",
            deps.join("k.d").display(),
            files
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(" ")
        );
        fs::write(deps.join("k.d"), &job.dep_text).unwrap();
        job.outputs = job
            .names
            .iter()
            .map(|name| (name.clone(), wire(&deps.join(name))))
            .collect();
        job.inputs = files
            .iter()
            .map(|path| (path.clone(), wire(path)))
            .collect();
        run_deferred(&job);
        let cache = Cache::new(&Dirs::at(&job.home));
        let candidates = cache.candidates(&job.prekey);
        assert_eq!(candidates.len(), 1);
        let inputs = &candidates[0].inputs;
        assert_eq!(inputs.len(), files.len());
        for (input, path) in inputs.iter().zip(&files) {
            assert_eq!(input.path, path.to_string_lossy());
            assert_eq!(input.hash, cache::hash_file(path).unwrap());
        }
        assert_eq!(candidates[0].key, entry_key(&job.prekey, inputs, &[]));
        job.home = s.0.join("after-edit");
        fs::write(&files[17], b"pub fn changed() {}\n").unwrap();
        run_deferred(&job);
        assert_eq!(
            rows(&job),
            0,
            "a changed frozen input must prevent publication"
        );
    }

    #[test]
    fn a_changed_input_output_or_watched_file_means_nothing_is_stored() {
        let s = Scratch::new("deferred-changed");
        let (job, src, _) = fixture(&s);
        fs::write(&src, b"pub fn a() { 1 }").unwrap();
        run_deferred(&job);
        assert_eq!(rows(&job), 0, "an input edited after the compile");

        let s = Scratch::new("deferred-output");
        let (job, _, rlib) = fixture(&s);
        fs::write(&rlib, b"object, rebuilt by the next build").unwrap();
        run_deferred(&job);
        assert_eq!(rows(&job), 0, "an output replaced after the compile");

        let s = Scratch::new("deferred-watched");
        let (mut job, _, _) = fixture(&s);
        let watched = s.0.join("watched.txt");
        fs::write(&watched, b"one").unwrap();
        job.watched.push((watched.clone(), wire(&watched)));
        fs::write(&watched, b"two!").unwrap();
        run_deferred(&job);
        assert_eq!(rows(&job), 0, "a file the key was built from");
    }

    #[test]
    fn an_identically_replaced_linker_refuses_a_deferred_store() {
        let s = Scratch::new("deferred-linker");
        let (mut job, _, _) = fixture(&s);
        let driver =
            s.0.join("target")
                .join("debug")
                .join("deps")
                .join("driver.rlib");
        fs::write(&driver, b"linker").unwrap();
        let linker = format!("-Clinker={}", driver.display());
        let dependency = format!("--extern=driver={}", driver.display());
        let keyed = keyed(&s.0, &[&linker, &dependency]).unwrap();
        job.args.extend([linker, dependency]);
        job.watched = keyed
            .watched
            .iter()
            .map(|(path, stamp)| (path.clone(), StampWire::of(stamp)))
            .collect();
        job.extern_hashes = keyed.extern_hashes.into_iter().collect();
        let replacement = driver.with_file_name("replacement-driver");
        fs::write(&replacement, b"linker").unwrap();
        sys::rename(&replacement, &driver).unwrap();
        run_deferred(&job);
        assert_eq!(
            rows(&job),
            0,
            "even equal bytes cannot excuse a changed linker stamp"
        );
    }

    #[test]
    fn a_source_modified_during_the_compile_is_still_refused_by_a_deferred_store() {
        let s = Scratch::new("deferred-new");
        let (mut job, _, _) = fixture(&s);
        job.spawned_ns = (hashing::now_ns() - 10_000_000_000).to_string();
        run_deferred(&job);
        assert_eq!(rows(&job), 0);
    }

    fn pair(name: &str, value: &str) -> (OsString, OsString) {
        (OsString::from(name), OsString::from(value))
    }

    #[test]
    fn only_cargo_and_output_affecting_variables_enter_the_key_and_in_a_stable_order() {
        let kept = keyed_env([
            pair("PATH", "/bin"),
            pair("CARGO_PKG_NAME", "k"),
            pair("CARGO_HOME", "/h"),
            pair("CARGO_TERM_COLOR", "always"),
            pair("CARGO_LOG", "debug"),
            pair("CARGO_REGISTRIES_X_TOKEN", "t"),
            pair("CARGO", "/cargo"),
            pair("RUSTC_BOOTSTRAP", "1"),
            pair("CARGO_PKG_NAME", "a"),
            pair("SDKROOT", "/sdk"),
        ]);
        let names: Vec<(&str, &str)> = kept
            .iter()
            .map(|(n, v)| (n.to_str().unwrap(), v.to_str().unwrap()))
            .collect();
        assert_eq!(
            names,
            [
                ("CARGO_PKG_NAME", "a"),
                ("CARGO_PKG_NAME", "k"),
                ("RUSTC_BOOTSTRAP", "1"),
                ("SDKROOT", "/sdk")
            ]
        );
        assert!(keyed_env(Vec::new()).is_empty());
    }

    #[test]
    fn linker_driver_environment_is_keyed_without_opaque_path_or_display_settings() {
        let kept = keyed_env([
            pair("COMPILER_PATH", "/compiler/tools"),
            pair("GCC_EXEC_PREFIX", "/gcc/prefix"),
            pair("CCC_OVERRIDE_OPTIONS", "+-custom-option"),
            pair("TOOLCHAINS", "custom.toolchain"),
            pair("LDFLAGS", "-L/custom/libs"),
            pair("LIBRARY_PATH", "/custom/libs"),
            pair("PATH", "/ordinary/tools"),
            pair("NO_COLOR", "1"),
        ]);
        assert_eq!(
            kept,
            [
                pair("CCC_OVERRIDE_OPTIONS", "+-custom-option"),
                pair("COMPILER_PATH", "/compiler/tools"),
                pair("GCC_EXEC_PREFIX", "/gcc/prefix"),
                pair("LDFLAGS", "-L/custom/libs"),
                pair("LIBRARY_PATH", "/custom/libs"),
                pair("TOOLCHAINS", "custom.toolchain"),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn variables_that_are_not_unicode_are_kept_by_their_bytes_when_filtered_in_and_dropped_otherwise()
     {
        // SAFETY: on unix the encoded form of an OsString is its raw bytes, so any byte string is valid.
        let bad = |bytes: &[u8]| unsafe { OsString::from_encoded_bytes_unchecked(bytes.to_vec()) };
        let kept = keyed_env([
            (OsString::from("CARGO_PKG_NAME"), bad(b"na\xffme")),
            (bad(b"CARGO_\xfe"), OsString::from("v")),
            (OsString::from("PATH"), bad(b"/\xff")),
            (bad(b"\xff_PATH"), OsString::from("v")),
            (OsString::from("CARGO_TERM_COLOR"), bad(b"\xff")),
            (bad(b"CARGO_HOME\xff"), OsString::from("v")),
            pair("CARGO_OK", "fine"),
        ]);
        assert_eq!(
            kept,
            [
                (bad(b"CARGO_HOME\xff"), OsString::from("v")),
                (OsString::from("CARGO_OK"), OsString::from("fine")),
                (OsString::from("CARGO_PKG_NAME"), bad(b"na\xffme")),
                (bad(b"CARGO_\xfe"), OsString::from("v")),
            ]
        );
    }

    #[test]
    fn deferred_output_commitments_validate_names_identity_and_legacy_jobs() {
        let s = Scratch::new("commitment-job");
        let (mut job, _, rlib) = fixture(&s);
        let (_, commitment) = cache::payload::capture(&rlib).unwrap();
        job.commitments
            .insert("libk.rlib".into(), commitment.clone());
        let frozen = Frozen::from_job(&job).unwrap();
        assert_eq!(frozen.commitments.len(), 1);
        let encoded = serde_json::to_value(&job).unwrap();
        let decoded: StoreJob = serde_json::from_value(encoded.clone()).unwrap();
        assert_eq!(decoded, job);
        let mut legacy = encoded;
        legacy.as_object_mut().unwrap().remove("commitments");
        let legacy: StoreJob = serde_json::from_value(legacy).unwrap();
        assert!(Frozen::from_job(&legacy).unwrap().commitments.is_empty());
        job.commitments.insert("../outside".into(), commitment);
        assert!(Frozen::from_job(&job).is_none());
        job.commitments.remove("../outside");
        job.outputs
            .iter_mut()
            .find(|(name, _)| name == "libk.rlib")
            .unwrap()
            .1
            .ino += 1;
        assert!(Frozen::from_job(&job).is_none());
        let replacement = rlib.with_extension("replacement");
        fs::write(&replacement, b"object").unwrap();
        sys::rename(&replacement, &rlib).unwrap();
        assert!(!frozen.outputs_unchanged(rlib.parent().unwrap()));
    }

    #[test]
    fn a_coarse_stamp_in_a_frozen_state_is_detected() {
        let fine = Stamp {
            dev: 1,
            ino: 2,
            size: 3,
            mtime_ns: 5_000_000_001,
            ctime_ns: 5_000_000_002,
        };
        let coarse = Stamp {
            mtime_ns: 5_000_000_000,
            ctime_ns: 6_000_000_000,
            ..fine
        };
        let frozen = |output: Stamp, input: Stamp| Frozen {
            names: Vec::new(),
            dep_name: String::new(),
            dep_text: String::new(),
            commitments: HashMap::new(),
            outputs: vec![("o".to_string(), output)],
            inputs: HashMap::from([(PathBuf::from("i"), input)]),
        };
        assert!(!frozen(fine, fine).has_coarse_stamp());
        assert!(frozen(coarse, fine).has_coarse_stamp(), "a coarse output");
        assert!(frozen(fine, coarse).has_coarse_stamp(), "a coarse input");
    }

    #[test]
    fn hashed_metadata_discovery_excludes_siblings_and_preserves_output_provenance() {
        let s = Scratch::new("hashed-metadata");
        let (mut job, _, rlib) = fixture(&s);
        for arg in &mut job.args {
            if arg == "--emit=dep-info,link" {
                *arg = "--emit=dep-info,metadata".into();
            }
        }
        job.args.extend([
            "--crate-type=rlib".into(),
            "-Cextra-filename=-abc123".into(),
        ]);
        let plan = Plan::from_job(&job, &Config::from_env()).unwrap();
        let deps = rlib.parent().unwrap();
        for name in [
            "k-abc123.d",
            "libk-abc123.rmeta",
            "k-abc123.d.other",
            "libk-abc1234.rmeta",
            "k-abc123.old.rcgu.dwo",
        ] {
            fs::write(deps.join(name), b"earlier compile").unwrap();
        }
        let before: HashMap<_, _> = plan
            .prefixed_outputs(|_| true)
            .into_iter()
            .map(|(name, stamp)| (name, stamp.unwrap()))
            .collect();
        assert_eq!(before.len(), 2);
        assert!(plan.output_files(&[], &before).is_empty());
        fs::write(deps.join("k-abc123.d"), b"new dependency information").unwrap();
        let unstamped = plan.prefixed_outputs(|_| false);
        assert_eq!(
            unstamped,
            [
                ("k-abc123.d".to_string(), None),
                ("libk-abc123.rmeta".to_string(), None),
            ]
        );
        assert_eq!(plan.output_files(&[], &before), ["k-abc123.d"]);
        let captured = ["libk-abc123.rmeta", "reported-output"]
            .map(|name| serde_json::json!({ "artifact": deps.join(name) }).to_string())
            .join("\n");
        assert_eq!(
            plan.output_files(captured.as_bytes(), &before),
            ["k-abc123.d", "libk-abc123.rmeta", "reported-output"]
        );
        sys::rename(&deps.join("libk-abc123.rmeta"), &deps.join("previous-rmeta")).unwrap();
        fs::create_dir(deps.join("libk-abc123.rmeta")).unwrap();
        assert_eq!(plan.prefixed_outputs(|_| false), [unstamped[0].clone()]);
        #[cfg(unix)]
        {
            sys::rename(&deps.join("libk-abc123.rmeta"), &deps.join("unrelated-directory")).unwrap();
            sys::link_tool(&deps.join("previous-rmeta"), &deps.join("libk-abc123.rmeta")).unwrap();
            assert_eq!(plan.prefixed_outputs(|_| false), [unstamped[0].clone()]);
        }
    }

    #[test]
    fn metadata_discovery_with_custom_suffix_keeps_prefix_scan() {
        let s = Scratch::new("custom-suffix-metadata");
        let (mut job, _, rlib) = fixture(&s);
        for arg in &mut job.args {
            if arg == "--emit=dep-info,link" {
                *arg = "--emit=dep-info,metadata".into();
            }
        }
        job.args.push("--crate-type=lib".into());
        let deps = rlib.parent().unwrap();
        for extra in ["-abc123.part", "-", "-custom"] {
            let mut custom_job = job.clone();
            custom_job.args.push(format!("-Cextra-filename={extra}"));
            let plan = Plan::from_job(&custom_job, &Config::from_env()).unwrap();
            let name = format!("k{extra}.d");
            fs::write(deps.join(&name), b"custom output").unwrap();
            assert!(
                plan.prefixed_outputs(|_| true)
                    .iter()
                    .any(|(found, stamp)| found == &name && stamp.is_some())
            );
        }
    }

    #[test]
    fn unhashed_metadata_discovery_owns_only_two_exact_regular_files() {
        let s = Scratch::new("unhashed-metadata");
        let (mut job, _, rlib) = fixture(&s);
        for arg in &mut job.args {
            if arg == "--emit=dep-info,link" {
                *arg = "--emit=dep-info,metadata".into();
            }
        }
        job.args.push("--crate-type=lib".into());
        let plan = Plan::from_job(&job, &Config::from_env()).unwrap();
        let deps = rlib.parent().unwrap();
        fs::write(deps.join("libk.rmeta"), b"old metadata").unwrap();
        let before: HashMap<_, _> = plan
            .prefixed_outputs(|_| true)
            .into_iter()
            .map(|(name, stamp)| (name, stamp.unwrap()))
            .collect();
        assert_eq!(before.len(), 2);
        fs::write(deps.join("k.d"), b"new dependency information").unwrap();
        fs::write(
            deps.join("libk.rmeta"),
            b"new metadata with a different size",
        )
        .unwrap();
        for name in [
            "libk.d",
            "liblibk.rmeta",
            "libk",
            "k_extra.d",
            "k-other.d",
            "k.notes",
            "libk.notes",
            "k.d.other",
        ] {
            fs::write(deps.join(name), b"another compile wrote this sibling").unwrap();
        }
        fs::create_dir(deps.join("k.dSYM")).unwrap();
        assert_eq!(plan.output_files(&[], &before), ["k.d", "libk.rmeta"]);
        let now: HashMap<_, _> = plan
            .prefixed_outputs(|_| true)
            .into_iter()
            .map(|(name, stamp)| (name, stamp.unwrap()))
            .collect();
        assert!(
            plan.output_files(&[], &now).is_empty(),
            "unchanged outputs do not belong to a new compile"
        );
        let captured = serde_json::json!({ "artifact": deps.join("k.notes") }).to_string();
        assert_eq!(
            plan.output_files(captured.as_bytes(), &before),
            ["k.d", "k.notes", "libk.rmeta"]
        );
        sys::rename(&deps.join("libk.rmeta"), &deps.join("previous-rmeta")).unwrap();
        fs::create_dir(deps.join("libk.rmeta")).unwrap();
        assert!(
            plan.output_files(&[], &before).is_empty(),
            "a missing regular metadata output prevents storage"
        );
        #[cfg(unix)]
        {
            sys::rename(&deps.join("libk.rmeta"), &deps.join("unrelated-directory")).unwrap();
            sys::link_tool(&deps.join("previous-rmeta"), &deps.join("libk.rmeta")).unwrap();
            assert_eq!(
                plan.prefixed_outputs(|_| true)
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>(),
                ["k.d"]
            );
            assert!(
                plan.output_files(&[], &before).is_empty(),
                "a symlink is not owned by metadata discovery"
            );
        }
    }

    #[test]
    fn unhashed_link_outputs_still_require_authoritative_artifact_notifications() {
        let s = Scratch::new("unhashed-link");
        let (job, _, _) = fixture(&s);
        let plan = Plan::from_job(&job, &Config::from_env()).unwrap();
        assert!(plan.prefixed_outputs(|_| true).is_empty());
        assert!(plan.output_files(&[], &HashMap::new()).is_empty());
        let captured =
            serde_json::json!({ "artifact": plan.parsed.out_dir.join("libk.rlib") }).to_string();
        assert_eq!(
            plan.output_files(captured.as_bytes(), &HashMap::new()),
            ["libk.rlib"]
        );
    }

    #[test]
    fn unhashed_windows_pdb_discovery_requires_a_changed_exact_link_companion() {
        let s = Scratch::new("unhashed-pdb");
        let (mut job, _, rlib) = fixture(&s);
        job.args.extend(["--target=x86_64-pc-windows-msvc".into(), "--crate-type=bin".into()]);
        let plan = Plan::from_job(&job, &Config::from_env()).unwrap();
        let deps = rlib.parent().unwrap();
        for name in ["k.pdb", "k_extra.pdb", "k-old.pdb", "libk.pdb", "k.notes"] {
            fs::write(deps.join(name), b"earlier unrelated output").unwrap();
        }
        let before: HashMap<_, _> = plan.prefixed_outputs(|_| true).into_iter()
            .map(|(name, stamp)| (name, stamp.unwrap())).collect();
        assert_eq!(before.len(), 1);
        assert!(before.contains_key("k.pdb"));
        let announced = |name: &str, emit: &str| serde_json::json!({"artifact": deps.join(name), "emit": emit}).to_string();
        let exe = announced("k.exe", "link");
        assert_eq!(plan.output_files(exe.as_bytes(), &before), ["k.exe"]);
        fs::write(deps.join("k.pdb"), b"newly linked matching debug information").unwrap();
        assert!(plan.output_files(&[], &before).is_empty());
        for captured in [announced("k_extra.exe", "link"), announced("libk.dll", "link"), announced("k.exe", "metadata")] {
            assert!(!plan.output_files(captured.as_bytes(), &before).iter().any(|name| name.ends_with(".pdb")));
        }
        let outside = serde_json::json!({"artifact": s.0.join("k.exe"), "emit": "link"}).to_string();
        assert!(plan.output_files(outside.as_bytes(), &before).is_empty());
        assert_eq!(plan.output_files(exe.as_bytes(), &before), ["k.exe", "k.pdb"]);
        assert_eq!(plan.output_files(announced("k.dll", "link").as_bytes(), &before), ["k.dll", "k.pdb"]);
        assert_eq!(plan.output_files(exe.as_bytes(), &HashMap::new()), ["k.exe", "k.pdb"]);
        sys::rename(&deps.join("k.pdb"), &deps.join("saved-pdb")).unwrap();
        fs::create_dir(deps.join("k.pdb")).unwrap();
        assert_eq!(plan.output_files(exe.as_bytes(), &before), ["k.exe"]);
        #[cfg(unix)]
        {
            sys::rename(&deps.join("k.pdb"), &deps.join("unrelated-directory")).unwrap();
            sys::link_tool(&deps.join("saved-pdb"), &deps.join("k.pdb")).unwrap();
            assert_eq!(plan.output_files(exe.as_bytes(), &before), ["k.exe"]);
        }
        job.args.push("-Cextra-filename=-abcd".into());
        let hashed = Plan::from_job(&job, &Config::from_env()).unwrap();
        fs::write(deps.join("k-abcd.pdb"), b"hashed companion").unwrap();
        assert!(hashed.output_files(&[], &HashMap::new()).iter().any(|name| name == "k-abcd.pdb"));
    }

    #[test]
    fn windows_pdb_key_domain_changes_only_corrected_unhashed_link_invocations() {
        let s = Scratch::new("pdb-key-domain");
        let out = s.0.join("target/debug/deps");
        for (flags, separates) in [
            (vec!["--target=x86_64-pc-windows-msvc", "--emit=dep-info,link"], true),
            (vec!["--target=x86_64-pc-windows-msvc", "--crate-type=dylib", "--emit=dep-info,link"], true),
            (vec!["--target=x86_64-pc-windows-msvc", "--emit=dep-info,link", "--emit=dep-info,metadata"], true),
            (vec!["--target=x86_64-pc-windows-msvc", "--emit=dep-info,metadata"], false),
            (vec!["--target=x86_64-pc-windows-msvc", "--emit=dep-info,link", "-Cextra-filename=-abcd"], false),
            (vec!["--target=x86_64-unknown-linux-gnu", "--emit=dep-info,link"], false),
        ] {
            let mut args: Vec<OsString> = ["--crate-name=k", "--out-dir"].into_iter().map(OsString::from).collect();
            args.push(out.clone().into_os_string());
            args.extend(flags.iter().map(OsString::from));
            let parsed = rustc_args::parse(&args).unwrap();
            let mut legacy = parsed.clone();
            legacy.windows_pdb_output = false;
            let profile = s.0.join("target/debug");
            let profile_text = profile.to_string_lossy();
            let hasher = Hasher::ephemeral();
            let context = KeyContext { hasher: &hasher, cwd: &s.0, profile: &profile_text, profile_abs: &profile };
            let tool = OsString::from("rustc");
            let current = context.prekey(&tool, &args, &parsed).unwrap();
            let old = context.prekey(&tool, &args, &legacy).unwrap();
            assert_eq!(current.key != old.key, separates, "{flags:?}");
            assert_eq!(current.unit != old.unit, separates, "{flags:?}");
        }
    }

    #[test]
    fn a_store_stays_inline_unless_the_profile_volume_is_trusted() {
        let s = Scratch::new("freeze");
        let (job, src, rlib) = fixture(&s);
        let deps = rlib.parent().unwrap().to_path_buf();
        let captured: String = ["k.d", "libk.rlib"]
            .iter()
            .map(|n| {
                format!(
                    "{}\n",
                    serde_json::json!({ "artifact": deps.join(n).to_string_lossy() })
                )
            })
            .collect();
        let freeze = |job: &StoreJob| {
            Plan::from_job(job, &Config::from_env())
                .expect("plan")
                .freeze(
                    captured.as_bytes(),
                    job.spawned_ns.parse().unwrap(),
                    &HashMap::new(),
                )
        };

        let off = StoreJob {
            trust: "off".to_string(),
            ..job.clone()
        };
        assert!(
            freeze(&off).is_none(),
            "stamps are not believed, so the store is not deferred"
        );

        let fine = [&src, &rlib, &deps.join("k.d")]
            .iter()
            .all(|p| !Stamp::read(p).unwrap().coarse());
        if hashing::fs_trusted(&s.0) && fine {
            let frozen = freeze(&job).expect("a trusted volume with fine stamps defers");
            assert_eq!(frozen.names, job.names);
            assert_eq!(frozen.inputs.get(&src), Some(&Stamp::read(&src).unwrap()));
            for (name, observed) in &frozen.outputs {
                assert_eq!(*observed, Stamp::read(&deps.join(name)).unwrap());
            }
            let directory = s.0.join("include");
            fs::create_dir(&directory).unwrap();
            let dep_text = format!(
                "{}: {} {}\n",
                deps.join("k.d").display(),
                src.display(),
                directory.display()
            );
            fs::write(deps.join("k.d"), &dep_text).unwrap();
            let frozen = freeze(&job).expect("directory inputs are ignored");
            assert_eq!(frozen.inputs.len(), 1);
            let dep_text = format!(
                "{}: {} {}\n",
                deps.join("k.d").display(),
                src.display(),
                s.0.join("missing.rs").display()
            );
            fs::write(deps.join("k.d"), dep_text).unwrap();
            assert!(freeze(&job).is_none(), "missing files cannot be frozen");
        }
    }

    #[test]
    fn fresh_external_inputs_decline_freezing_before_output_commitments() {
        let s = Scratch::new("freeze-fresh-input");
        let (job, src, rlib) = fixture(&s);
        fs::write(&rlib, vec![b'o'; cache::payload::MIN_BYTES as usize]).unwrap();
        let plan = Plan::from_job(&job, &Config::from_env()).unwrap();
        fs::create_dir_all(plan.dirs.tmp()).unwrap();
        let deps = rlib.parent().unwrap();
        let captured = ["k.d", "libk.rlib"]
            .iter()
            .map(|name| format!("{}\n", serde_json::json!({ "artifact": deps.join(name) })))
            .collect::<String>();
        let stamp = Stamp::read(&src).unwrap();
        let newest = stamp.mtime_ns.max(stamp.ctime_ns);
        let inside = newest + 100_000_000 - 1;
        let boundary = newest + 100_000_000;
        let outside = boundary + 1;
        for spawned_ns in [inside, boundary] {
            assert!(
                plan.freeze_with_capture(captured.as_bytes(), spawned_ns, &HashMap::new(), |_| {
                    panic!("a fresh external input must decline before output capture")
                })
                .is_none()
            );
        }
        let fine = [&src, &rlib, &deps.join("k.d")]
            .iter()
            .all(|path| !Stamp::read(path).unwrap().coarse());
        if !hashing::fs_trusted(&s.0) || !fine {
            return;
        }
        let compress = cache_compression_requested()
            && cache::payload::compression_supported(&plan.dirs.tmp());
        let mut captures = Vec::new();
        let frozen = plan
            .freeze_with_capture(captured.as_bytes(), outside, &HashMap::new(), |path| {
                captures.push(path.to_path_buf());
                cache::payload::capture(path)
            })
            .expect("a settled external input remains eligible for deferral");
        assert_eq!(frozen.inputs.get(&src), Some(&stamp));
        assert_eq!(
            captures,
            if compress {
                vec![rlib.clone()]
            } else {
                Vec::new()
            }
        );
        assert_eq!(frozen.commitments.len(), usize::from(compress));

        let generated = plan.profile_abs.join("generated.rs");
        fs::write(&generated, b"pub fn generated() {}").unwrap();
        fs::write(
            deps.join("k.d"),
            format!("{}: {}\n", deps.join("k.d").display(), generated.display()),
        )
        .unwrap();
        let generated_stamp = Stamp::read(&generated).unwrap();
        if !generated_stamp.coarse() && !Stamp::read(&deps.join("k.d")).unwrap().coarse() {
            let spawned_ns = generated_stamp.mtime_ns.max(generated_stamp.ctime_ns);
            assert!(generated_stamp.too_new_for(spawned_ns));
            let frozen = plan
                .freeze(captured.as_bytes(), spawned_ns, &HashMap::new())
                .expect("fresh profile inputs retain their exemption");
            assert_eq!(frozen.inputs, HashMap::from([(generated, generated_stamp)]));
        }
    }

    #[test]
    fn fine_input_freshness_checks_both_mtime_and_ctime_at_the_settle_boundary() {
        let old = 10_000_000_001;
        let recent = 10_500_000_001;
        for (mtime_ns, ctime_ns) in [(recent, old), (old, recent)] {
            let stamp = Stamp {
                dev: 1,
                ino: 2,
                size: 3,
                mtime_ns,
                ctime_ns,
            };
            assert!(!stamp.coarse());
            assert!(stamp.too_new_for(recent + 100_000_000 - 1));
            assert!(stamp.too_new_for(recent + 100_000_000));
            assert!(!stamp.too_new_for(recent + 100_000_000 + 1));
        }
    }

    #[test]
    fn slots_are_exclusive_bounded_and_reusable() {
        let s = Scratch::new("slots");
        let dir = s.0.join("deferred");
        let first = acquire_slot(&dir, 2).expect("first");
        let second = acquire_slot(&dir, 2).expect("second");
        assert!(acquire_slot(&dir, 2).is_none(), "the pool is exhausted");
        drop(first);
        // A child forked by another test thread keeps a copy of the descriptor until it execs, so the release can lag briefly.
        let again = (0..100).find_map(|_| {
            acquire_slot(&dir, 2)
                .or_else(|| (std::thread::sleep(Duration::from_millis(20)), None).1)
        });
        assert!(again.is_some(), "a released slot is free again");
        drop(again);
        assert!(acquire_slot(&dir, 0).is_none());
        drop(second);
        assert!((1..=4).contains(&slot_count()));
    }

    #[test]
    fn a_store_reserves_a_slot_before_freezing_and_releases_it_when_freezing_declines() {
        let s = Scratch::new("freeze-slot");
        let (mut job, _, _) = fixture(&s);
        job.trust = "off".to_string();
        let plan = Plan::from_job(&job, &Config::from_env()).unwrap();
        assert!(
            !plan
                .spawn_deferred(&[], 0, 0, &HashMap::new(), None)
                .unwrap()
        );
        let dir = plan.dirs.deferred();
        assert!(
            dir.join("slot-0").exists(),
            "capacity is reserved before the untrusted profile declines freezing"
        );
        let released = (0..100).find_map(|_| {
            acquire_slot(&dir, 1).or_else(|| (thread::sleep(Duration::from_millis(20)), None).1)
        });
        assert!(released.is_some(), "the early return releases its slot");
    }

    #[test]
    fn the_hidden_argument_selects_the_helper_only_in_its_exact_forms() {
        let os = |parts: &[&str]| parts.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(
            deferred_entry(&os(&[DEFERRED_ARG, "/j"])),
            Some(PathBuf::from("/j"))
        );
        assert_eq!(
            deferred_entry(&os(&["rustc", DEFERRED_ARG, "/j"])),
            None,
            "the persona form is a compiler command line"
        );
        assert_eq!(deferred_entry(&os(&["rustc", "--crate-name", "k"])), None);
        assert_eq!(deferred_entry(&os(&[DEFERRED_ARG, "/j", "extra"])), None);
        assert_eq!(
            deferred_entry(&os(&["rustc", "--crate-name", DEFERRED_ARG, "/j"])),
            None
        );
        assert_eq!(deferred_entry(&os(&[DEFERRED_ARG])), None);
        assert_eq!(deferred_entry(&os(&[])), None);
    }

    #[test]
    fn a_helper_given_a_job_it_cannot_use_exits_cleanly_and_removes_it() {
        let s = Scratch::new("helper-bad");
        let deferred = s.0.join("deferred");
        fs::create_dir_all(&deferred).unwrap();
        let job = deferred.join("job-1-1.json");
        fs::write(&job, b"{ not a job").unwrap();
        assert_eq!(deferred_main_in(job.clone(), &deferred), 0);
        assert!(!job.exists());
        assert_eq!(
            deferred_main_in(deferred.join("job-2-2.json"), &deferred),
            0
        );
    }

    #[test]
    fn the_helper_entry_stores_and_removes_a_valid_job() {
        let s = Scratch::new("helper-ok");
        let (job, _, _) = fixture(&s);
        let deferred = s.0.join("deferred");
        fs::create_dir_all(&deferred).unwrap();
        let path = deferred.join("job-1-1.json");
        fs::write(&path, serde_json::to_vec(&job).unwrap()).unwrap();
        assert_eq!(deferred_main_in(path.clone(), &deferred), 0);
        assert!(!path.exists());
        assert_eq!(rows(&job), 1);
    }

    #[test]
    fn a_file_that_is_not_a_job_in_the_deferred_directory_is_neither_read_nor_deleted() {
        let s = Scratch::new("helper-refuse");
        let (job, _, _) = fixture(&s);
        let deferred = s.0.join("deferred");
        fs::create_dir_all(&deferred).unwrap();
        let bytes = serde_json::to_vec(&job).unwrap();

        let outside = s.0.join("job-1-1.json");
        let misnamed = deferred.join("notes.json");
        let wrong_suffix = deferred.join("job-1-1.txt");
        let nested = deferred.join("sub").join("job-1-1.json");
        fs::create_dir_all(nested.parent().unwrap()).unwrap();
        let dotted = deferred.join("..").join("deferred").join("job-1-1.json");
        for path in [&outside, &misnamed, &wrong_suffix, &nested] {
            fs::write(path, &bytes).unwrap();
            assert!(
                JobFile::claim(path.clone(), &deferred).is_none(),
                "{}",
                path.display()
            );
            assert_eq!(deferred_main_in(path.clone(), &deferred), 0);
            assert!(path.exists(), "{} was deleted", path.display());
        }
        assert_eq!(rows(&job), 0, "a refused file stores nothing");

        let valid = deferred.join("job-1-1.json");
        fs::write(&valid, &bytes).unwrap();
        assert!(
            JobFile::claim(dotted, &deferred).is_none(),
            "a path that only resolves into the directory is refused"
        );
        assert!(
            JobFile::claim(valid, &deferred).is_some(),
            "the exact form is accepted"
        );
    }

    #[test]
    fn the_helper_is_started_only_from_a_binary_named_frust_wrap() {
        let s = Scratch::new("helper-exe");
        let bin = s.0.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let name = format!("frust-wrap{}", env::consts::EXE_SUFFIX);
        let persona = bin.join(format!("frustc{}", env::consts::EXE_SUFFIX));
        assert!(helper_beside(&persona).is_err(), "no sibling, no helper");
        assert!(helper_beside(&bin.join(&name)).is_ok_and(|p| p == bin.join(&name)));
        fs::write(bin.join(&name), b"").unwrap();
        assert_eq!(helper_beside(&persona).unwrap(), bin.join(&name));
        assert_eq!(helper_beside(&bin.join(&name)).unwrap(), bin.join(&name));
    }

    #[test]
    fn leased_jobs_protect_inputs_outputs_aliases_and_then_become_dead() {
        let s = Scratch::new("job-protection");
        let (mut job, src, output) = fixture(&s);
        job.ownership_version = 1;
        let watched = s.0.join("watched");
        fs::write(&watched, b"generated input").unwrap();
        job.watched.push((watched.clone(), wire(&watched)));
        let dirs = Dirs::at(&job.home);
        let (path, lease) = write_leased_job(&dirs.deferred(), &job).unwrap();
        assert!(matches!(
            deferred_retirement(&path).unwrap(),
            DeferredRetirement::Busy
        ));
        let aliases = s.0.join("aliases");
        fs::create_dir(&aliases).unwrap();
        fs::hard_link(&output, aliases.join("same-object")).unwrap();
        let protection = deferred_protection(&dirs);
        assert!(protection.is_complete() && !protection.leases.is_empty());
        for path in [src.as_path(), watched.as_path(), output.as_path(), aliases.as_path(), output.parent().unwrap()] {
            assert!(protection.protects(path), "{}", path.display());
        }
        let unrelated = s.0.join("unrelated");
        fs::write(&unrelated, b"other").unwrap();
        assert!(!protection.protects(&unrelated));
        assert!(!protection.protects_file(&unrelated, &sys::lstat(&unrelated).unwrap()));
        assert!(protection.protects_file(&aliases.join("same-object"), &sys::lstat(&output).unwrap()));
        drop(lease);
        assert!(matches!(
            deferred_retirement(&path).unwrap(),
            DeferredRetirement::Busy
        ));
        drop(protection);
        assert!(matches!(
            deferred_retirement(&path).unwrap(),
            DeferredRetirement::Dead(_)
        ));
        let dead = deferred_protection(&dirs);
        assert!(dead.is_complete() && dead.leases.is_empty() && !dead.protects(&output));
    }

    #[test]
    fn legacy_and_uncertain_jobs_remain_protected_but_dead_junk_can_age_out() {
        let s = Scratch::new("legacy-protection");
        let (job, _, output) = fixture(&s);
        let dirs = Dirs::at(&job.home);
        let path = write_job(&dirs.deferred(), &job).unwrap();
        assert!(matches!(
            deferred_retirement(&path).unwrap(),
            DeferredRetirement::Legacy(_)
        ));
        let legacy = deferred_protection(&dirs);
        assert!(legacy.is_complete() && legacy.protects(&output));
        drop(legacy);
        fs::write(&path, b"{invalid").unwrap();
        let uncertain = deferred_protection(&dirs);
        assert!(!uncertain.is_complete() && uncertain.protects(&s.0.join("unrelated")));
        assert!(matches!(
            deferred_retirement(&path).unwrap(),
            DeferredRetirement::Legacy(_)
        ));
        let file = File::options().write(true).open(&path).unwrap();
        file.set_len(MAX_PROTECTION_JOB_BYTES + 1).unwrap();
        assert!(!deferred_protection(&dirs).is_complete());
        assert!(matches!(
            deferred_retirement(&path).unwrap(),
            DeferredRetirement::Legacy(_)
        ));
    }

    #[test]
    fn protection_budget_failure_and_replaced_marker_are_conservative() {
        let s = Scratch::new("protection-budget");
        let (mut job, _, output) = fixture(&s);
        job.ownership_version = 1;
        let dirs = Dirs::at(&job.home);
        let (path, lease) = write_leased_job(&dirs.deferred(), &job).unwrap();
        let protection = deferred_protection(&dirs);
        protection.inspected.set(MAX_PROTECTION_PATHS);
        assert!(protection.protects(&s.0.join("unrelated")) && !protection.is_complete());
        let replacement = path.with_extension("replacement");
        fs::write(&replacement, serde_json::to_vec(&job).unwrap()).unwrap();
        sys::rename(&replacement, &path).unwrap();
        assert!(job_identity(&path, &lease).is_err());
        assert!(protection.protects(&output));
    }

    #[cfg(unix)]
    #[test]
    fn inherited_job_lease_checks_descriptor_identity_and_survives_producer_drop() {
        let s = Scratch::new("inherited-lease");
        let (mut job, _, _) = fixture(&s);
        job.ownership_version = 1;
        let dirs = Dirs::at(&job.home);
        let (path, producer) = write_leased_job(&dirs.deferred(), &job).unwrap();
        let inherited = inherited_job_lease(&path, &sys::inherited_file_tag(&producer)).unwrap();
        let other = File::open(&path).unwrap();
        assert!(inherited_job_lease(&path, "1").is_err());
        assert!(inherited_job_lease(&path, "invalid").is_err());
        let foreign = s.0.join("foreign");
        fs::write(&foreign, b"not job metadata").unwrap();
        assert!(inherited_job_lease(&foreign, &sys::inherited_file_tag(&other)).is_err());
        drop(producer);
        assert!(matches!(
            deferred_retirement(&path).unwrap(),
            DeferredRetirement::Busy
        ));
        drop(inherited);
        assert!(matches!(
            deferred_retirement(&path).unwrap(),
            DeferredRetirement::Dead(_)
        ));
    }

    #[test]
    fn published_job_roundtrip_retains_its_platform_ownership_lease() {
        let s = Scratch::new("published-job-lease");
        let (mut job, _, _) = fixture(&s);
        job.ownership_version = u32::from(sys::INHERITS_DESCRIPTORS);
        let dir = Dirs::at(&job.home).deferred();
        let (path, lease) = write_leased_job(&dir, &job).unwrap();
        assert_eq!(read_job(&path), Some(job.clone()));
        let published = job_identity(&path, &lease).unwrap();
        assert_eq!(published.len(), serde_json::to_vec(&job).unwrap().len() as u64);
        assert!(matches!(deferred_retirement(&path).unwrap(), DeferredRetirement::Busy));
        drop(lease);
        match deferred_retirement(&path).unwrap() {
            DeferredRetirement::Dead(_) => assert_eq!(job.ownership_version, 1),
            DeferredRetirement::Legacy(_) => assert_eq!(job.ownership_version, 0),
            DeferredRetirement::Busy => panic!("the completed producer must release its lease"),
        }
        assert_eq!(read_job(&path), Some(job));
    }

    #[test]
    fn written_jobs_are_read_back_and_unreadable_ones_are_not() {
        let s = Scratch::new("write-job");
        let (job, _, _) = fixture(&s);
        let dir = s.0.join("jobs");
        let path = write_job(&dir, &job).unwrap();
        assert!(path.starts_with(&dir));
        assert_eq!(read_job(&path), Some(job));
        assert!(read_job(&dir.join("absent.json")).is_none());
    }

    #[test]
    fn key_locks_are_sharded_by_prekey_exclusive_and_waited_for() {
        let s = Scratch::new("keylock");
        let dirs = Dirs::at(s.0.join("home"));
        let prekey = "cd".repeat(20);
        let path = keylock_path(&dirs, &prekey);
        assert_eq!(path, dirs.keylocks().join("cd").join(&prekey));
        assert!(
            !path.starts_with(dirs.locks()),
            "gc treats everything under locks as a build lock directory"
        );

        let held = open_key_lock(&dirs, &path).unwrap();
        assert!(try_key_lock(&held).unwrap());
        let other = open_key_lock(&dirs, &path).unwrap();
        assert!(!try_key_lock(&other).unwrap());
        assert!(
            !wait_key_lock(&other, Duration::from_millis(30)).unwrap(),
            "gives up at the cap"
        );
        assert!(!wait_key_lock(&other, Duration::ZERO).unwrap());

        let releaser = thread::spawn(move || {
            thread::sleep(Duration::from_millis(60));
            drop(held);
        });
        assert!(
            wait_key_lock(&other, Duration::from_secs(60)).unwrap(),
            "takes the lock once it is released"
        );
        releaser.join().unwrap();
        assert!(
            !try_key_lock(&open_key_lock(&dirs, &path).unwrap()).unwrap(),
            "now held by `other`"
        );
    }

    #[test]
    fn key_lock_hint_wait_rechecks_before_waiting() {
        let s = Scratch::new("key-hint-recheck");
        let dirs = Dirs::at(s.0.join("home"));
        let path = keylock_path(&dirs, &"cd".repeat(20));
        let owner = open_key_lock(&dirs, &path).unwrap();
        let follower = open_key_lock(&dirs, &path).unwrap();
        assert!(try_key_lock(&owner).unwrap());
        assert!(!try_key_lock(&follower).unwrap());
        sys::unlock(&owner).unwrap();
        assert!(wait_key_lock_hints(&follower, Duration::from_secs(2), |_| {
            panic!("release during subscription must be found before waiting")
        }).unwrap());
        assert!(!try_key_lock(&owner).unwrap());
    }

    #[test]
    fn key_lock_hint_retries_reset_backoff_and_keep_authoritative_locking() {
        let s = Scratch::new("key-hint-retry");
        let dirs = Dirs::at(s.0.join("home"));
        let path = keylock_path(&dirs, &"cd".repeat(20));
        let owner = open_key_lock(&dirs, &path).unwrap();
        let follower = open_key_lock(&dirs, &path).unwrap();
        assert!(try_key_lock(&owner).unwrap());
        let mut pauses = Vec::new();
        assert!(wait_key_lock_hints(&follower, Duration::from_secs(2), |pause| {
            pauses.push(pause);
            match pauses.len() {
                1 => false,
                2 => true,
                3 => {
                    sys::unlock(&owner).unwrap();
                    false
                }
                _ => panic!("unlock must admit on the next authoritative retry"),
            }
        }).unwrap());
        assert_eq!(pauses, [KEY_POLL_START, KEY_POLL_START * 2, KEY_POLL_START]);
        assert!(!try_key_lock(&owner).unwrap());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn key_lock_watcher_rechecks_release_during_subscription() {
        let s = Scratch::new("key-watch-subscribe");
        let dirs = Dirs::at(s.0.join("home"));
        let path = keylock_path(&dirs, &"cd".repeat(20));
        let owner = open_key_lock(&dirs, &path).unwrap();
        let follower = open_key_lock(&dirs, &path).unwrap();
        assert!(try_key_lock(&owner).unwrap());
        assert!(!try_key_lock(&follower).unwrap());
        sys::unlock(&owner).unwrap();
        let mut watcher = sys::DirectoryWatcher::for_key_lock(path.parent().unwrap()).unwrap();
        assert!(!watcher.wait(Duration::ZERO));
        assert!(wait_key_lock_hints(&follower, Duration::from_secs(2), |_| {
            panic!("a release before subscription has no event and requires an immediate recheck")
        }).unwrap());
        assert!(!try_key_lock(&owner).unwrap());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn key_lock_watcher_bounds_retry_after_unlock_with_open_owner() {
        let s = Scratch::new("key-watch-unlock");
        let dirs = Dirs::at(s.0.join("home"));
        let path = keylock_path(&dirs, &"cd".repeat(20));
        let owner = open_key_lock(&dirs, &path).unwrap();
        let follower = open_key_lock(&dirs, &path).unwrap();
        assert!(try_key_lock(&owner).unwrap());
        let mut watcher = sys::DirectoryWatcher::for_key_lock(path.parent().unwrap()).unwrap();
        let mut waits = 0;
        assert!(wait_key_lock_hints(&follower, Duration::from_secs(2), |pause| {
            waits += 1;
            assert!(pause <= KEY_POLL_MAX);
            sys::unlock(&owner).unwrap();
            let changed = watcher.wait(pause);
            assert!(!changed, "unlock while the owner stays open produces no close event");
            changed
        }).unwrap());
        assert_eq!(waits, 1);
        assert!(!try_key_lock(&owner).unwrap());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn key_lock_watcher_ignores_close_of_an_unlocked_carrier() {
        let s = Scratch::new("key-watch-carrier");
        let dirs = Dirs::at(s.0.join("home"));
        let path = keylock_path(&dirs, &"cd".repeat(20));
        let owner = open_key_lock(&dirs, &path).unwrap();
        let follower = open_key_lock(&dirs, &path).unwrap();
        assert!(try_key_lock(&owner).unwrap());
        let mut watcher = sys::DirectoryWatcher::for_key_lock(path.parent().unwrap()).unwrap();
        let carrier = open_key_lock(&dirs, &path).unwrap();
        let duplicate = carrier.try_clone().unwrap();
        drop(carrier);
        drop(duplicate);
        let mut waits = 0;
        assert!(wait_key_lock_hints(&follower, Duration::from_secs(2), |pause| {
            waits += 1;
            match waits {
                1 => assert!(watcher.wait(Duration::ZERO), "the duplicate carrier's final close is pending"),
                2 => {
                    assert_eq!(pause, KEY_POLL_START);
                    sys::unlock(&owner).unwrap();
                }
                _ => panic!("owner release must admit the follower"),
            }
            true
        }).unwrap());
        assert_eq!(waits, 2, "the close event alone cannot admit the follower");
        assert!(!try_key_lock(&owner).unwrap());
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "manual real-lock handoff timing; run with --release --nocapture"]
    fn key_lock_handoff_measurements() {
        let s = Scratch::new("key-handoff-measure");
        let dirs = Dirs::at(s.0.join("home"));
        let path = keylock_path(&dirs, &"cd".repeat(20));
        let mut samples = Vec::new();
        for pair in 0..31 {
            for watched in if pair % 2 == 0 { [false, true] } else { [true, false] } {
                let owner = open_key_lock(&dirs, &path).unwrap();
                let follower = open_key_lock(&dirs, &path).unwrap();
                assert!(try_key_lock(&owner).unwrap());
                assert!(!try_key_lock(&follower).unwrap());
                let mut watcher = watched.then(|| {
                    sys::DirectoryWatcher::for_key_lock(path.parent().unwrap()).unwrap()
                });
                let (ready_send, ready) = std::sync::mpsc::channel();
                let (result_send, result) = std::sync::mpsc::channel();
                let (finish, finished) = std::sync::mpsc::channel();
                let waiter = thread::spawn(move || {
                    ready_send.send(()).unwrap();
                    let acquired = match watcher.as_mut() {
                        Some(watcher) => wait_key_lock_hints(&follower, Duration::from_secs(2), |pause| watcher.wait(pause)),
                        None => wait_key_lock(&follower, Duration::from_secs(2)),
                    }.unwrap();
                    result_send.send((acquired, Instant::now())).unwrap();
                    finished.recv_timeout(Duration::from_secs(2)).unwrap();
                    drop(follower);
                });
                ready.recv_timeout(Duration::from_secs(2)).unwrap();
                let release_delay = Duration::from_millis([7, 19, 37, 61][pair % 4]);
                thread::sleep(release_delay);
                let released = Instant::now();
                drop(owner);
                let (acquired, admitted) = result.recv_timeout(Duration::from_secs(3)).unwrap();
                assert!(acquired);
                let probe = open_key_lock(&dirs, &path).unwrap();
                assert!(!try_key_lock(&probe).unwrap(), "the timed follower must still hold the real lock");
                finish.send(()).unwrap();
                waiter.join().unwrap();
                samples.push(serde_json::json!({
                    "pair": pair,
                    "watched": watched,
                    "release_delay_ns": release_delay.as_nanos() as u64,
                    "release_to_admission_ns": admitted.checked_duration_since(released).expect("admission precedes owner release").as_nanos() as u64,
                }));
            }
        }
        println!("{}", serde_json::json!({ "samples": samples }));
    }
}
