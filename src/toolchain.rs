//! Asks the installed `rustc` and `cargo` what they are; memoised on disk because the rustup proxies cost 30-80ms.

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::paths::Dirs;
use crate::sys;

pub fn find_on_path(name: &str) -> Option<PathBuf> {
    let paths = env::var_os("PATH")?;
    env::split_paths(&paths).find_map(|dir| sys::find_executable_in(&dir, name))
}

pub fn resolve_tool(name: &str) -> Option<PathBuf> {
    let override_var = format!("FRUST_{}", name.to_ascii_uppercase().replace('-', "_"));
    if let Some(path) = env::var_os(&override_var).filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(path));
    }
    find_on_path(name)
}

/// Prefix of the wrapper's key error when an explicit linker cannot be found, which the timing record classifies as `skip:linker`.
pub const LINKER_UNRESOLVED: &str = "cannot resolve linker";

/// The directories rustc itself searches for the linkers rustup ships.
pub fn sysroot_linker_dirs(sysroot: &Path, host: &str) -> [PathBuf; 3] {
    let bin = sysroot.join("lib").join("rustlib").join(host).join("bin");
    [bin.clone(), bin.join("gcc-ld"), bin.join("self-contained")]
}

/// PATH first, then the toolchain's own directories; `None` means the linker must not be keyed.
pub fn resolve_linker(name: &str, sysroot: Option<(&Path, &str)>) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }
    find_on_path(name).or_else(|| {
        let (root, host) = sysroot?;
        let suffixed = format!("{name}{}", env::consts::EXE_SUFFIX);
        sysroot_linker_dirs(root, host)
            .iter()
            .find_map(|dir| sys::find_executable_in(dir, &suffixed).or_else(|| sys::find_executable_in(dir, name)))
    })
}

/// The sysroot and host triple come from the front end, so no rustc is spawned per unit.
pub fn resolve_linker_env(name: &str) -> Option<PathBuf> {
    let root = env::var_os("FRUST_SYSROOT").filter(|v| !v.is_empty()).map(PathBuf::from);
    let host = env::var("FRUST_HOST").ok().filter(|v| !v.is_empty());
    resolve_linker(name, root.as_deref().zip(host.as_deref()))
}

/// The linker frust picks when the user has not: rust-lld on windows-msvc hosts, nothing elsewhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DefaultLinker {
    NotApplicable,
    OptedOut,
    UserConfigured,
    Missing,
    Lld(PathBuf),
}

impl DefaultLinker {
    /// The label recorded in the tuning string; `None` when the system linker stays in use.
    pub fn label(&self) -> Option<&'static str> {
        matches!(self, DefaultLinker::Lld(_)).then_some("lld-link")
    }

    pub fn describe(&self) -> String {
        match self {
            DefaultLinker::NotApplicable => "the platform default; frust only picks rust-lld on windows-msvc".to_string(),
            DefaultLinker::OptedOut => "link.exe (FRUST_LINKER=system)".to_string(),
            DefaultLinker::UserConfigured => "your configured linker; frust leaves it alone".to_string(),
            DefaultLinker::Missing => "link.exe; rust-lld.exe is not in the toolchain sysroot".to_string(),
            DefaultLinker::Lld(path) => format!("rust-lld ({}); FRUST_LINKER=system restores link.exe", path.display()),
        }
    }
}

/// `FRUST_LINKER=system` keeps the platform linker.
pub fn linker_opted_out() -> bool {
    env::var("FRUST_LINKER").is_ok_and(|v| v.trim().eq_ignore_ascii_case("system"))
}

/// The cargo environment variable that sets the linker for one target triple.
pub fn target_linker_var(triple: &str) -> String {
    let upper: String = triple.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' }).collect();
    format!("CARGO_TARGET_{upper}_LINKER")
}

/// True when an environment variable already chooses a linker, directly or through rustflags.
pub fn env_sets_linker(vars: impl IntoIterator<Item = (OsString, OsString)>) -> bool {
    vars.into_iter().any(|(key, value)| {
        let key = key.to_string_lossy();
        (key.starts_with("CARGO_TARGET_") && key.ends_with("_LINKER") && !value.is_empty())
            || (key.ends_with("RUSTFLAGS") && value.to_string_lossy().contains("linker"))
    })
}

/// Pure choice of the default linker; the sysroot lookup is injected so it can be tested off Windows.
pub fn default_linker(host: &str, opted_out: bool, user_configured: bool, sysroot: Option<&Path>, exists: impl Fn(&Path) -> bool) -> DefaultLinker {
    if !host.ends_with("-windows-msvc") {
        return DefaultLinker::NotApplicable;
    }
    if opted_out {
        return DefaultLinker::OptedOut;
    }
    if user_configured {
        return DefaultLinker::UserConfigured;
    }
    let Some(root) = sysroot else { return DefaultLinker::Missing };
    let [bin, ..] = sysroot_linker_dirs(root, host);
    let path = bin.join("rust-lld.exe");
    if exists(&path) { DefaultLinker::Lld(path) } else { DefaultLinker::Missing }
}

/// The effective tuning as one string such as `deps-debug0,lld-link`, or `none`.
pub fn tuning_label(deps_debug0: bool, linker: &DefaultLinker) -> String {
    let parts: Vec<&str> = [deps_debug0.then_some("deps-debug0"), linker.label()].into_iter().flatten().collect();
    if parts.is_empty() { "none".to_string() } else { parts.join(",") }
}

fn probe_rustc() -> Option<PathBuf> {
    env::var_os("RUSTC").map(PathBuf::from).or_else(|| resolve_tool("rustc"))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Probe {
    pub rustc_verbose: String,
    pub release: String,
    pub host: String,
    pub cargo_version: (u32, u32, u32),
    pub id: String,
    /// Empty when `rustc --print sysroot` failed.
    pub sysroot: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheOutcome {
    Hit,
    Miss,
    /// The inputs could not be captured reliably, so nothing was looked up or stored.
    Bypass,
}

impl CacheOutcome {
    pub fn label(self) -> &'static str {
        match self {
            CacheOutcome::Hit => "hit",
            CacheOutcome::Miss => "miss",
            CacheOutcome::Bypass => "bypass",
        }
    }
}

const PROBE_FORMAT: &str = "frust-probe-v2";
/// Backstop for selection inputs the key cannot see, such as a hand-written `rustc` wrapper script.
const PROBE_TTL: Duration = Duration::from_secs(10 * 60);
/// A validated file modified this close to the probe start may have changed mid-probe, so the result is not memoised.
const RACY_SLACK: Duration = Duration::from_secs(2);

impl Probe {
    pub fn run(toolchain: Option<&OsStr>, cwd: &Path) -> Option<Probe> {
        let rustc = probe_rustc()?;
        let cargo = resolve_tool("cargo")?;
        probe_now(&rustc, &cargo, toolchain, cwd, false, || {}).map(|(probe, _)| probe)
    }

    /// The key covers toolchain selection; sysroot file stats validate the toolchain bytes, since `rustup update` leaves the proxy unchanged.
    pub fn cached(dirs: &Dirs, toolchain: Option<&OsStr>, cwd: &Path) -> Option<(Probe, CacheOutcome)> {
        Probe::cached_then(dirs, toolchain, cwd, || {})
    }

    /// As [`Probe::cached`]; a probe that has to run calls `on_cold` once no proxy can still be installing the toolchain, so other cargo queries can start beside the rest.
    pub fn cached_then(dirs: &Dirs, toolchain: Option<&OsStr>, cwd: &Path, on_cold: impl FnOnce()) -> Option<(Probe, CacheOutcome)> {
        let rustc = probe_rustc()?;
        let cargo = resolve_tool("cargo")?;
        let Some(key) = probe_key(&rustc, &cargo, toolchain, cwd) else {
            return probe_now(&rustc, &cargo, toolchain, cwd, false, on_cold).map(|(p, _)| (p, CacheOutcome::Bypass));
        };
        let slot = probe_slot(dirs, &rustc, &cargo, toolchain, cwd);
        let earlier = memo::load::<ProbeEntry>(&slot);
        if let Some(entry) = &earlier
            && entry.valid(&key)
        {
            return earlier.map(|entry| (entry.probe, CacheOutcome::Hit));
        }
        // Only an unseen toolchain selection is likely to change the id that keys the other memos; an expired entry keeps it.
        let unseen = earlier.is_none_or(|entry| entry.key != key);
        let on_cold = move || if unseen { on_cold() };

        let _guard = memo::cold_miss(dirs, &slot);
        let Some(key) = probe_key(&rustc, &cargo, toolchain, cwd) else {
            return probe_now(&rustc, &cargo, toolchain, cwd, false, on_cold)
                .map(|(p, _)| (p, CacheOutcome::Bypass));
        };
        if let Some(entry) = memo::load::<ProbeEntry>(&slot)
            && entry.valid(&key)
        {
            return Some((entry.probe, CacheOutcome::Hit));
        }

        let started = SystemTime::now();
        let (probe, sysroots) = probe_now(&rustc, &cargo, toolchain, cwd, true, on_cold)?;
        let validate = validation_set(&sysroots);
        let racy = validate.iter().any(|(_, sig)| sig.as_ref().is_some_and(|s| s.changed_after(started, RACY_SLACK)));
        let stable = probe_key(&rustc, &cargo, toolchain, cwd).as_deref() == Some(key.as_str());
        if !sysroots.is_empty() && !racy && stable {
            let entry = ProbeEntry { format: PROBE_FORMAT.to_string(), key, created_unix: unix_now(), validate, probe: probe.clone() };
            memo::store(dirs, &slot, &entry);
        }
        Some((probe, CacheOutcome::Miss))
    }
}

/// An empty sysroot list means the result cannot be validated and must not be memoised.
fn probe_now(rustc: &Path, cargo: &Path, toolchain: Option<&OsStr>, cwd: &Path, want_sysroot: bool, after_cargo: impl FnOnce()) -> Option<(Probe, Vec<PathBuf>)> {
    let tc: Option<OsString> = toolchain.map(|t| t.to_os_string());
    let installed = installed_selection(toolchain, cwd);
    let spawn = |tool: &Path, name: &str, args: &'static [&'static str]| {
        let (tool, tc) = match installed.as_deref().and_then(|dir| proxied_binary(dir, tool, name)) {
            Some(exe) => (exe, None),
            None => (tool.to_path_buf(), tc.clone()),
        };
        let cwd = cwd.to_path_buf();
        thread::spawn(move || query(&tool, tc.as_deref(), args, &cwd))
    };
    // Run one query alone first unless the toolchain is installed: parallel proxies would each start rustup's auto-install of a missing toolchain and corrupt it.
    let cargo_handle = installed.is_some().then(|| spawn(cargo, "cargo", &["-V"]));
    let cargo_out = match cargo_handle {
        Some(_) => None,
        None => query(cargo, tc.as_deref(), &["-V"], cwd),
    };
    after_cargo();
    let rustc_handle = spawn(rustc, "rustc", &["-vV"]);
    let own_handle = spawn(rustc, "rustc", &["--print", "sysroot"]);
    let mut sysroot_handles = Vec::new();
    // `RUSTC` may name a compiler other than the proxied one `cargo` belongs to.
    if want_sysroot && let Some(proxy) = resolve_tool("rustc").filter(|p| p != rustc) {
        sysroot_handles.push(spawn(&proxy, "rustc", &["--print", "sysroot"]));
    }
    let rustc_out = rustc_handle.join().ok().flatten();
    let cargo_out = match cargo_handle {
        Some(handle) => handle.join().ok().flatten(),
        None => cargo_out,
    };
    let own_sysroot = own_handle.join().ok().flatten().map(|s| PathBuf::from(s.trim())).filter(|s| s.is_absolute());
    let mut sysroots = Vec::new();
    let mut complete = true;
    if want_sysroot {
        match &own_sysroot {
            Some(dir) => sysroots.push(dir.clone()),
            None => complete = false,
        }
    }
    for handle in sysroot_handles {
        match handle.join().ok().flatten().map(|s| PathBuf::from(s.trim())).filter(|s| s.is_absolute()) {
            Some(dir) => sysroots.push(dir),
            None => complete = false,
        }
    }
    if !complete {
        sysroots.clear();
    }
    let (cargo_out, rustc_out) = (cargo_out?, rustc_out?);
    let host = field(&rustc_out, "host:").unwrap_or_default();
    let release = field(&rustc_out, "release:").unwrap_or_default();
    let id = crate::paths::short_hash(&rustc_out);
    let probe = Probe {
        cargo_version: parse_version(cargo_out.split_whitespace().nth(1).unwrap_or("")),
        rustc_verbose: rustc_out,
        release,
        host,
        id,
        sysroot: own_sysroot.map(|dir| dir.to_string_lossy().into_owned()).unwrap_or_default(),
    };
    Some((probe, sysroots))
}

#[derive(Debug, Serialize, Deserialize)]
struct ProbeEntry {
    format: String,
    key: String,
    created_unix: u64,
    validate: Vec<(PathBuf, Option<memo::Sig>)>,
    probe: Probe,
}

impl ProbeEntry {
    fn valid(&self, key: &str) -> bool {
        let age = unix_now().checked_sub(self.created_unix);
        self.format == PROBE_FORMAT
            && self.key == key
            && age.is_some_and(|a| a < PROBE_TTL.as_secs())
            && !self.validate.is_empty()
            && self.validate.iter().all(|(path, sig)| memo::Sig::of(path) == *sig)
    }
}

fn validation_set(sysroots: &[PathBuf]) -> Vec<(PathBuf, Option<memo::Sig>)> {
    let mut paths = Vec::new();
    for root in sysroots {
        let lib = root.join("lib");
        let dylibs = root.join(sys::SYSROOT_DYLIB_DIR);
        paths.push(root.join("bin").join(format!("rustc{}", env::consts::EXE_SUFFIX)));
        paths.push(root.join("bin").join(format!("cargo{}", env::consts::EXE_SUFFIX)));
        paths.push(lib.join("rustlib").join("multirust-channel-manifest.toml"));
        paths.push(lib.clone());
        if dylibs != lib {
            paths.push(dylibs.clone());
        }
        let driver_prefix = format!("{}rustc_driver", env::consts::DLL_PREFIX);
        if let Ok(entries) = fs::read_dir(&dylibs) {
            let mut drivers: Vec<PathBuf> = entries
                .flatten()
                .filter(|e| {
                    let name = e.file_name();
                    let name = name.as_encoded_bytes();
                    name.starts_with(driver_prefix.as_bytes()) && name.ends_with(env::consts::DLL_SUFFIX.as_bytes())
                })
                .map(|e| e.path())
                .collect();
            drivers.sort();
            paths.extend(drivers);
        }
    }
    paths.dedup();
    paths.into_iter().map(|p| {
        let sig = memo::Sig::of(&p);
        (p, sig)
    }).collect()
}

/// One slot per working directory and binary pair, so a changed input overwrites rather than accumulates.
fn probe_slot(dirs: &Dirs, rustc: &Path, cargo: &Path, toolchain: Option<&OsStr>, cwd: &Path) -> PathBuf {
    let mut d = memo::Digest::new("probe-slot");
    d.bytes(cwd.as_os_str().as_encoded_bytes());
    d.opt(toolchain.map(OsStr::as_encoded_bytes));
    d.bytes(rustc.as_os_str().as_encoded_bytes());
    d.bytes(cargo.as_os_str().as_encoded_bytes());
    dirs.root().join("probe").join(format!("{}.json", d.short()))
}

fn probe_key(rustc: &Path, cargo: &Path, toolchain: Option<&OsStr>, cwd: &Path) -> Option<String> {
    let mut d = memo::Digest::new(PROBE_FORMAT);
    d.opt(toolchain.map(OsStr::as_encoded_bytes));
    let mut vars: Vec<(OsString, OsString)> = env::vars_os()
        .filter(|(k, _)| {
            let k = k.as_encoded_bytes();
            k.starts_with(b"RUSTUP_")
                || matches!(k, b"RUSTC" | b"FRUST_RUSTC" | b"FRUST_CARGO" | b"CARGO_HOME")
                || sys::HOME_VARS.iter().any(|home| home.as_bytes() == k)
        })
        .collect();
    vars.sort();
    for (k, v) in &vars {
        d.bytes(k.as_encoded_bytes());
        d.bytes(v.as_encoded_bytes());
    }
    for tool in [rustc, cargo] {
        d.bytes(tool.as_os_str().as_encoded_bytes());
        let canonical = sys::canonicalize(tool).ok()?;
        d.bytes(canonical.as_os_str().as_encoded_bytes());
        d.sig(memo::Sig::of(&canonical).as_ref());
    }
    for dir in memo::ancestor_chains(cwd) {
        for name in ["rust-toolchain.toml", "rust-toolchain"] {
            d.file(&dir.join(name))?;
        }
    }
    d.file(&rustup_home()?.join("settings.toml"))?;
    Some(d.finish())
}

/// The directory of the toolchain rustup would pick, only when it is installed so no proxy can start an install; anything unclear answers `None`.
fn selected_toolchain(rustup: &Path, toolchain: Option<&OsStr>, env_toolchain: Option<&OsStr>, cwd: &Path) -> Option<PathBuf> {
    let toolchains = rustup.join("toolchains");
    let installed = |name: &str| {
        let dir = toolchains.join(name);
        (!name.is_empty() && !name.contains(['/', '\\']) && name != "." && name != ".." && dir.is_dir()).then_some(dir)
    };
    if let Some(chosen) = toolchain.or(env_toolchain) {
        let chosen = chosen.to_str()?;
        return installed(chosen.strip_prefix('+').unwrap_or(chosen));
    }
    let pinned = memo::ancestor_chains(cwd)
        .iter()
        .any(|dir| ["rust-toolchain.toml", "rust-toolchain"].iter().any(|name| fs::symlink_metadata(dir.join(name)).is_ok()));
    let settings = fs::read_to_string(rustup.join("settings.toml")).ok()?;
    let (mut section, mut default) = (String::new(), None);
    for line in settings.lines().map(str::trim).filter(|line| !line.is_empty() && !line.starts_with('#')) {
        if line.starts_with('[') {
            section = line.to_string();
        } else if section.contains("overrides") {
            return None;
        } else if section.is_empty()
            && let Some(value) = line.strip_prefix("default_toolchain").map(str::trim_start).and_then(|rest| rest.strip_prefix('='))
        {
            default = value.trim().strip_prefix('"').and_then(|v| v.strip_suffix('"')).map(str::to_string);
        }
    }
    installed(&default.filter(|_| !pinned)?)
}

fn installed_selection(toolchain: Option<&OsStr>, cwd: &Path) -> Option<PathBuf> {
    selected_toolchain(&rustup_home()?, toolchain, env::var_os("RUSTUP_TOOLCHAIN").as_deref(), cwd)
}

/// A rustup proxy of an installed toolchain only runs that toolchain's binary, so the binary can be run directly; a hard-linked proxy is not recognised.
fn proxied_binary(toolchain_dir: &Path, tool: &Path, name: &str) -> Option<PathBuf> {
    let proxy = sys::canonicalize(tool).is_ok_and(|real| real.file_stem().is_some_and(|stem| stem == "rustup"));
    let exe = toolchain_dir.join("bin").join(format!("{name}{}", env::consts::EXE_SUFFIX));
    (proxy && exe.is_file()).then_some(exe)
}

/// The binary the rustup proxy `tool` would run for `cwd`, which then takes no `+toolchain` argument; `None` keeps the proxy.
pub fn direct_binary(tool: &Path, name: &str, toolchain: Option<&OsStr>, cwd: &Path) -> Option<PathBuf> {
    proxied_binary(&installed_selection(toolchain, cwd)?, tool, name)
}

fn rustup_home() -> Option<PathBuf> {
    match env::var_os("RUSTUP_HOME").filter(|v| !v.is_empty()) {
        Some(home) => Some(PathBuf::from(home)),
        None => sys::home_var().filter(|v| !v.is_empty()).map(|h| PathBuf::from(h).join(".rustup")),
    }
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub(crate) mod memo {
    use std::fs::{self, File};
    use std::io;
    use std::path::{Path, PathBuf};
    use std::thread;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use serde::de::DeserializeOwned;
    use serde::{Deserialize, Serialize};

    use crate::lock::{ensure_dir, open_lock_file, shared_mode};
    use crate::paths::Dirs;
    use crate::sys;

    pub struct ColdMissGuard(File);

    impl Drop for ColdMissGuard {
        fn drop(&mut self) {
            let _ = sys::unlock(&self.0);
        }
    }

    /// Stable guard inodes survive memo replacement and collection; contention only delays a miss briefly.
    pub fn cold_miss(dirs: &Dirs, slot: &Path) -> Option<ColdMissGuard> {
        if !dirs.root().is_dir() {
            return None;
        }
        let mode = shared_mode(dirs);
        let directory = dirs.root().join("memo-locks");
        ensure_dir(&directory, mode).ok()?;
        let stripe = blake3::hash(slot.as_os_str().as_encoded_bytes()).as_bytes()[0] & 15;
        let file = open_lock_file(&directory.join(format!("{stripe:02}.lock")), mode).ok()?;
        let started = Instant::now();
        loop {
            match sys::try_lock(&file, sys::LockKind::Exclusive) {
                Ok(()) => return Some(ColdMissGuard(file)),
                Err(fs::TryLockError::WouldBlock) => {
                    let remaining = Duration::from_millis(250).checked_sub(started.elapsed())?;
                    thread::sleep(remaining.min(Duration::from_millis(2)));
                }
                Err(_) => return None,
            }
        }
    }

    /// Any replacement (new inode), in-place write or metadata change makes the signature differ.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Sig {
        dev: u64,
        ino: u64,
        size: u64,
        mtime: (i64, i64),
        ctime: (i64, i64),
    }

    impl Sig {
        pub fn of(path: &Path) -> Option<Sig> {
            let m = sys::stat(path).ok()?;
            Some(Sig { dev: m.dev(), ino: m.ino(), size: m.len(), mtime: m.mtime(), ctime: m.ctime() })
        }

        pub fn changed_after(&self, since: SystemTime, slack: Duration) -> bool {
            let horizon = since.checked_sub(slack).unwrap_or(UNIX_EPOCH);
            let horizon = horizon.duration_since(UNIX_EPOCH).unwrap_or_default();
            let horizon = (horizon.as_secs() as i64, i64::from(horizon.subsec_nanos()));
            self.mtime >= horizon || self.ctime >= horizon
        }
    }

    pub struct Digest(blake3::Hasher);

    impl Digest {
        pub fn new(domain: &str) -> Digest {
            let mut d = Digest(blake3::Hasher::new());
            d.bytes(domain.as_bytes());
            d
        }

        pub fn bytes(&mut self, bytes: &[u8]) {
            self.0.update(&(bytes.len() as u64).to_le_bytes());
            self.0.update(bytes);
        }

        pub fn opt(&mut self, bytes: Option<&[u8]>) {
            match bytes {
                Some(b) => {
                    self.bytes(b"some");
                    self.bytes(b);
                }
                None => self.bytes(b"none"),
            }
        }

        pub fn sig(&mut self, sig: Option<&Sig>) {
            match sig {
                Some(s) => self.bytes(format!("{}:{}:{}:{:?}:{:?}", s.dev, s.ino, s.size, s.mtime, s.ctime).as_bytes()),
                None => self.bytes(b"absent"),
            }
        }

        /// `None` when the file exists but is unreadable, since its contents are then unknown.
        pub fn file(&mut self, path: &Path) -> Option<()> {
            self.bytes(path.as_os_str().as_encoded_bytes());
            match fs::read(path) {
                Ok(body) => {
                    self.bytes(b"present");
                    self.bytes(blake3::hash(&body).as_bytes());
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => self.bytes(b"absent"),
                // A path component that is a file reads as absent too.
                Err(err) if err.kind() == io::ErrorKind::NotADirectory => self.bytes(b"absent"),
                Err(_) => return None,
            }
            Some(())
        }

        pub fn finish(self) -> String {
            self.0.finalize().to_hex().to_string()
        }

        pub fn short(self) -> String {
            self.0.finalize().to_hex()[..24].to_string()
        }
    }

    /// Tools resolve against the physical directory while users type the logical one.
    pub fn ancestor_chains(dir: &Path) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = Vec::new();
        let canonical = sys::canonicalize(dir).ok();
        for start in [Some(dir), canonical.as_deref()].into_iter().flatten() {
            for a in start.ancestors().filter(|a| !a.as_os_str().is_empty()) {
                if !out.iter().any(|o| o == a) {
                    out.push(a.to_path_buf());
                }
            }
        }
        out
    }

    pub fn load<T: DeserializeOwned>(path: &Path) -> Option<T> {
        serde_json::from_slice(&fs::read(path).ok()?).ok()
    }

    /// Failures only cost the next run a miss.
    pub fn store<T: Serialize>(dirs: &Dirs, path: &Path, entry: &T) {
        if !dirs.root().is_dir() {
            return;
        }
        if let Ok(json) = serde_json::to_vec(entry) {
            let _ = crate::lock::write_shared(path, &json, crate::lock::shared_mode(dirs));
        }
    }
}

fn query(tool: &Path, toolchain: Option<&OsStr>, args: &[&str], cwd: &Path) -> Option<String> {
    let mut cmd = Command::new(tool);
    if let Some(tc) = toolchain {
        cmd.arg(tc);
    }
    let output = cmd.args(args).current_dir(cwd).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    output.status.success().then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

fn field(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|l| l.strip_prefix(key)).map(|v| v.trim().to_string())
}

pub fn parse_version(text: &str) -> (u32, u32, u32) {
    let core = text.split(['-', '+']).next().unwrap_or("");
    let mut parts = core.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    (parts.next().unwrap_or(0), parts.next().unwrap_or(0), parts.next().unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_linker_is_rust_lld_only_on_windows_msvc_without_user_choice() {
        let sysroot = Path::new("sys");
        let lld = sysroot.join("lib").join("rustlib").join("x86_64-pc-windows-msvc").join("bin").join("rust-lld.exe");
        let present = |p: &Path| p == lld;
        let msvc = "x86_64-pc-windows-msvc";
        assert_eq!(default_linker(msvc, false, false, Some(sysroot), present), DefaultLinker::Lld(lld.clone()));
        assert_eq!(default_linker(msvc, true, false, Some(sysroot), present), DefaultLinker::OptedOut);
        assert_eq!(default_linker(msvc, false, true, Some(sysroot), present), DefaultLinker::UserConfigured);
        assert_eq!(default_linker(msvc, false, false, Some(sysroot), |_| false), DefaultLinker::Missing);
        assert_eq!(default_linker(msvc, false, false, None, |_| true), DefaultLinker::Missing);
        for host in ["aarch64-apple-darwin", "x86_64-unknown-linux-gnu", "x86_64-pc-windows-gnu"] {
            assert_eq!(default_linker(host, false, false, Some(sysroot), |_| true), DefaultLinker::NotApplicable);
        }
        assert_eq!(default_linker("aarch64-pc-windows-msvc", false, false, Some(sysroot), |_| true).label(), Some("lld-link"));
    }

    #[test]
    fn linker_variable_and_user_detection_follow_cargo_naming() {
        assert_eq!(target_linker_var("x86_64-pc-windows-msvc"), "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER");
        let var = |k: &str, v: &str| vec![(OsString::from(k), OsString::from(v))];
        assert!(env_sets_linker(var("CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER", "lld-link")));
        assert!(!env_sets_linker(var("CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER", "")));
        assert!(env_sets_linker(var("RUSTFLAGS", "-C linker=foo")));
        assert!(env_sets_linker(var("CARGO_BUILD_RUSTFLAGS", "-Clinker-flavor=lld-link")));
        assert!(!env_sets_linker(var("RUSTFLAGS", "-C target-cpu=native")));
        assert!(!env_sets_linker(var("PATH", "linker")));
    }

    #[test]
    fn tuning_label_lists_active_tuning() {
        assert_eq!(tuning_label(true, &DefaultLinker::Lld(PathBuf::from("x"))), "deps-debug0,lld-link");
        assert_eq!(tuning_label(true, &DefaultLinker::NotApplicable), "deps-debug0");
        assert_eq!(tuning_label(false, &DefaultLinker::Lld(PathBuf::from("x"))), "lld-link");
        assert_eq!(tuning_label(false, &DefaultLinker::OptedOut), "none");
    }

    #[test]
    fn parses_release_and_prerelease_versions() {
        assert_eq!(parse_version("1.99.0"), (1, 99, 0));
        assert_eq!(parse_version("1.101.0-nightly"), (1, 101, 0));
        assert_eq!(parse_version("2.0.0+meta"), (2, 0, 0));
        assert_eq!(parse_version("garbage"), (0, 0, 0));
    }

    #[test]
    fn digest_fields_are_length_prefixed_and_files_hash_contents() {
        let mut a = memo::Digest::new("t");
        a.bytes(b"ab");
        a.bytes(b"c");
        let mut b = memo::Digest::new("t");
        b.bytes(b"a");
        b.bytes(b"bc");
        assert_ne!(a.finish(), b.finish());

        let dir = env::temp_dir().join(format!("frust-memo-unit-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("rust-toolchain.toml");
        let digest = |p: &Path| {
            let mut d = memo::Digest::new("t");
            d.file(p).unwrap();
            d.finish()
        };
        let absent = digest(&file);
        fs::write(&file, "[toolchain]\nchannel = \"stable\"\n").unwrap();
        let stable = digest(&file);
        fs::write(&file, "[toolchain]\nchannel = \"beta\"\n").unwrap();
        let beta = digest(&file);
        assert!(absent != stable && stable != beta && absent != beta);
        assert_eq!(digest(&file.join("nested")), digest(&file.join("nested")), "a file used as a directory reads as absent");
        let _ = fs::remove_file(&file);
        let _ = fs::remove_dir(&dir);
    }

    fn fake_linker(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"linker").unwrap();
        sys::make_executable(path).unwrap();
    }

    fn same_path(a: Option<PathBuf>, b: PathBuf) -> bool {
        a.is_some_and(|a| a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase())
    }

    #[test]
    fn linkers_resolve_from_the_sysroot_after_path_and_not_at_all_otherwise() {
        let root = env::temp_dir().join(format!("frust-linker-unit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let bin = root.join("lib").join("rustlib").join("x86_64-test").join("bin");
        let suffix = env::consts::EXE_SUFFIX;
        fake_linker(&bin.join(format!("frust-test-lld{suffix}")));
        fake_linker(&bin.join("gcc-ld").join(format!("frust-test-ld.lld{suffix}")));
        fake_linker(&bin.join("self-contained").join(format!("frust-test-sc{suffix}")));

        let sysroot = Some((root.as_path(), "x86_64-test"));
        assert!(same_path(resolve_linker("frust-test-lld", sysroot), bin.join(format!("frust-test-lld{suffix}"))));
        assert!(same_path(resolve_linker("frust-test-ld.lld", sysroot), bin.join("gcc-ld").join(format!("frust-test-ld.lld{suffix}"))));
        assert!(same_path(resolve_linker("frust-test-sc", sysroot), bin.join("self-contained").join(format!("frust-test-sc{suffix}"))));
        assert_eq!(resolve_linker("frust-test-lld", None), None, "without a sysroot only PATH is searched");
        assert_eq!(resolve_linker("frust-test-lld", Some((root.as_path(), "other-host"))), None);
        assert_eq!(resolve_linker("frust-test-missing", sysroot), None);
        assert_eq!(resolve_linker("", sysroot), None);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn queries_run_together_only_when_the_selected_toolchain_is_installed() {
        let root = env::temp_dir().join(format!("frust-selection-unit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let (rustup, project) = (root.join("rustup"), root.join("work").join("project"));
        fs::create_dir_all(rustup.join("toolchains").join("stable-x")).unwrap();
        fs::create_dir_all(&project).unwrap();
        let settings = |text: &str| fs::write(rustup.join("settings.toml"), text).unwrap();
        let selected = |tc: Option<&str>, env_tc: Option<&str>| selected_toolchain(&rustup, tc.map(OsStr::new), env_tc.map(OsStr::new), &project);
        let installed = |tc: Option<&str>, env_tc: Option<&str>| selected(tc, env_tc).is_some();
        assert!(!installed(None, None), "no settings file");
        settings("version = \"12\"\ndefault_toolchain = \"stable-x\"\nprofile = \"default\"\n\n[overrides]\n");
        assert_eq!(selected(None, None), Some(rustup.join("toolchains").join("stable-x")));
        assert!(installed(Some("+stable-x"), None) && installed(None, Some("stable-x")));
        assert!(!installed(Some("+beta-x"), None) && !installed(None, Some("beta-x")) && !installed(Some("+stable"), None));
        assert!(!installed(None, Some("../toolchains/stable-x")) && !installed(None, Some("")));
        settings("default_toolchain = \"beta-x\"\n");
        assert!(!installed(None, None), "default not installed");
        settings("default_toolchain = \"stable-x\"\n[overrides]\n\"/elsewhere\" = \"beta-x\"\n");
        assert!(!installed(None, None), "any directory override");
        settings("default_toolchain = \"stable-x\"\n");
        for name in ["rust-toolchain", "rust-toolchain.toml"] {
            fs::write(root.join("work").join(name), "stable-x").unwrap();
            assert!(!installed(None, None), "{name} in an ancestor");
            assert!(installed(None, Some("stable-x")), "the variable outranks {name}");
            fs::remove_file(root.join("work").join(name)).unwrap();
        }
        assert!(installed(None, None));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_probe_answers_what_the_proxies_answer() {
        let (Some(rustc), Some(cargo)) = (probe_rustc(), resolve_tool("cargo")) else { return };
        let cwd = env::current_dir().unwrap();
        let Some((probe, sysroots)) = probe_now(&rustc, &cargo, None, &cwd, true, || {}) else { return };
        assert_eq!(Some(probe.rustc_verbose.clone()), query(&rustc, None, &["-vV"], &cwd));
        let sysroot = query(&rustc, None, &["--print", "sysroot"], &cwd).unwrap();
        assert_eq!(probe.sysroot, sysroot.trim());
        assert_eq!(sysroots.first().map(|dir| dir.to_string_lossy().into_owned()), Some(sysroot.trim().to_string()));
        let version = query(&cargo, None, &["-V"], &cwd).unwrap();
        assert_eq!(probe.cargo_version, parse_version(version.split_whitespace().nth(1).unwrap()));
    }

    #[test]
    fn signatures_follow_replacement() {
        let dir = env::temp_dir().join(format!("frust-sig-unit-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("bin");
        assert_eq!(memo::Sig::of(&file), None);
        fs::write(&file, "one").unwrap();
        let first = memo::Sig::of(&file).unwrap();
        let staged = dir.join("bin.new");
        fs::write(&staged, "one").unwrap();
        sys::rename(&staged, &file).unwrap();
        assert_ne!(memo::Sig::of(&file).unwrap(), first, "same bytes, new inode: rustup's install pattern");
        assert!(memo::Sig::of(&file).unwrap().changed_after(SystemTime::now(), Duration::from_secs(2)));
        let _ = fs::remove_file(&file);
        let _ = fs::remove_dir(&dir);
    }
}
