//! Asks the installed `rustc` and `cargo` what they are; memoised on disk because the rustup proxies cost 30-80ms.

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::paths::Dirs;

pub fn find_on_path(name: &str) -> Option<PathBuf> {
    let paths = env::var_os("PATH")?;
    env::split_paths(&paths)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata().map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

pub fn resolve_tool(name: &str) -> Option<PathBuf> {
    let override_var = format!("FRUST_{}", name.to_ascii_uppercase().replace('-', "_"));
    if let Some(path) = env::var_os(&override_var).filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(path));
    }
    find_on_path(name)
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

const PROBE_FORMAT: &str = "frust-probe-v1";
/// Backstop for selection inputs the key cannot see, such as a hand-written `rustc` wrapper script.
const PROBE_TTL: Duration = Duration::from_secs(10 * 60);
/// A validated file modified this close to the probe start may have changed mid-probe, so the result is not memoised.
const RACY_SLACK: Duration = Duration::from_secs(2);

impl Probe {
    pub fn run(toolchain: Option<&OsStr>, cwd: &Path) -> Option<Probe> {
        let rustc = probe_rustc()?;
        let cargo = resolve_tool("cargo")?;
        probe_now(&rustc, &cargo, toolchain, cwd, false).map(|(probe, _)| probe)
    }

    /// The key covers toolchain selection; sysroot file stats validate the toolchain bytes, since `rustup update` leaves the proxy unchanged.
    pub fn cached(dirs: &Dirs, toolchain: Option<&OsStr>, cwd: &Path) -> Option<(Probe, CacheOutcome)> {
        let rustc = probe_rustc()?;
        let cargo = resolve_tool("cargo")?;
        let Some(key) = probe_key(&rustc, &cargo, toolchain, cwd) else {
            return probe_now(&rustc, &cargo, toolchain, cwd, false).map(|(p, _)| (p, CacheOutcome::Bypass));
        };
        let slot = probe_slot(dirs, &rustc, &cargo, toolchain, cwd);
        if let Some(entry) = memo::load::<ProbeEntry>(&slot)
            && entry.valid(&key)
        {
            return Some((entry.probe, CacheOutcome::Hit));
        }

        let started = SystemTime::now();
        let (probe, sysroots) = probe_now(&rustc, &cargo, toolchain, cwd, true)?;
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
fn probe_now(rustc: &Path, cargo: &Path, toolchain: Option<&OsStr>, cwd: &Path, want_sysroot: bool) -> Option<(Probe, Vec<PathBuf>)> {
    let tc: Option<OsString> = toolchain.map(|t| t.to_os_string());
    let spawn = |tool: PathBuf, args: &'static [&'static str]| {
        let (tc, cwd) = (tc.clone(), cwd.to_path_buf());
        thread::spawn(move || query(&tool, tc.as_deref(), args, &cwd))
    };
    let rustc_handle = spawn(rustc.to_path_buf(), &["-vV"]);
    let mut sysroot_handles = Vec::new();
    if want_sysroot {
        sysroot_handles.push(spawn(rustc.to_path_buf(), &["--print", "sysroot"]));
        // `RUSTC` may name a compiler other than the proxied one `cargo` belongs to.
        if let Some(proxy) = resolve_tool("rustc").filter(|p| p != rustc) {
            sysroot_handles.push(spawn(proxy, &["--print", "sysroot"]));
        }
    }
    let cargo_out = query(cargo, tc.as_deref(), &["-V"], cwd);
    let rustc_out = rustc_handle.join().ok().flatten();
    let mut sysroots = Vec::new();
    let mut complete = true;
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
        paths.push(root.join("bin").join("rustc"));
        paths.push(root.join("bin").join("cargo"));
        paths.push(lib.join("rustlib").join("multirust-channel-manifest.toml"));
        paths.push(lib.clone());
        if let Ok(entries) = fs::read_dir(&lib) {
            let mut drivers: Vec<PathBuf> = entries
                .flatten()
                .filter(|e| e.file_name().as_bytes().starts_with(b"librustc_driver"))
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
    d.bytes(cwd.as_os_str().as_bytes());
    d.opt(toolchain.map(OsStr::as_bytes));
    d.bytes(rustc.as_os_str().as_bytes());
    d.bytes(cargo.as_os_str().as_bytes());
    dirs.root().join("probe").join(format!("{}.json", d.short()))
}

fn probe_key(rustc: &Path, cargo: &Path, toolchain: Option<&OsStr>, cwd: &Path) -> Option<String> {
    let mut d = memo::Digest::new(PROBE_FORMAT);
    d.opt(toolchain.map(OsStr::as_bytes));
    let mut vars: Vec<(OsString, OsString)> = env::vars_os()
        .filter(|(k, _)| {
            let k = k.as_bytes();
            k.starts_with(b"RUSTUP_") || matches!(k, b"RUSTC" | b"FRUST_RUSTC" | b"FRUST_CARGO" | b"HOME" | b"CARGO_HOME")
        })
        .collect();
    vars.sort();
    for (k, v) in &vars {
        d.bytes(k.as_bytes());
        d.bytes(v.as_bytes());
    }
    for tool in [rustc, cargo] {
        d.bytes(tool.as_os_str().as_bytes());
        let canonical = tool.canonicalize().ok()?;
        d.bytes(canonical.as_os_str().as_bytes());
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

fn rustup_home() -> Option<PathBuf> {
    match env::var_os("RUSTUP_HOME").filter(|v| !v.is_empty()) {
        Some(home) => Some(PathBuf::from(home)),
        None => env::var_os("HOME").filter(|v| !v.is_empty()).map(|h| PathBuf::from(h).join(".rustup")),
    }
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub(crate) mod memo {
    use std::fs;
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use serde::de::DeserializeOwned;
    use serde::{Deserialize, Serialize};

    use crate::paths::Dirs;

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
            let m = fs::metadata(path).ok()?;
            Some(Sig {
                dev: m.dev(),
                ino: m.ino(),
                size: m.size(),
                mtime: (m.mtime(), m.mtime_nsec()),
                ctime: (m.ctime(), m.ctime_nsec()),
            })
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
            self.bytes(path.as_os_str().as_bytes());
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
        let canonical = dir.canonicalize().ok();
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
        fs::rename(&staged, &file).unwrap();
        assert_ne!(memo::Sig::of(&file).unwrap(), first, "same bytes, new inode: rustup's install pattern");
        assert!(memo::Sig::of(&file).unwrap().changed_after(SystemTime::now(), Duration::from_secs(2)));
        let _ = fs::remove_file(&file);
        let _ = fs::remove_dir(&dir);
    }
}
