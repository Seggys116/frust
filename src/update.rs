//! Update detection and `frust self-update`: a daily release check that runs detached and never delays a build, and a checksummed in-place upgrade.

use std::cmp::Ordering;
use std::env;
use std::fs::{self, File};
use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::fsx;
use crate::paths::Dirs;
use crate::style::Styler;
use crate::sys;

/// Hidden flag of `frust self-update` that runs one release check and prints nothing.
pub const BACKGROUND_FLAG: &str = "--check-background";
/// Exit code of `frust self-update --check` when a newer release exists.
pub const EXIT_UPDATE_AVAILABLE: i32 = 10;

const BINARIES: [&str; 8] = ["frust", "fcargo", "fclippy", "frustc", "frustdoc", "frustup", "frustfmt", "frust-wrap"];
const EXTRA_FILES: [&str; 2] = ["LICENSE", "README.md"];
const CHECK_EVERY_SECS: u64 = 86_400;
const NOTIFY_EVERY_SECS: u64 = 86_400;
const API_TIMEOUT_SECS: u32 = 30;
const DOWNLOAD_TIMEOUT_SECS: u32 = 600;
const VERSION_CHECK_TIMEOUT: Duration = Duration::from_secs(20);
const STALE_STAGING_AGE: Duration = Duration::from_secs(3600);
const STAGING_PREFIX: &str = ".frust-update-";
const DEFAULT_API_URL: &str = "https://api.github.com/repos/Seggys116/frust/releases/latest";
const DEFAULT_DOWNLOAD_BASE: &str = "https://github.com/Seggys116/frust/releases/download";
const API_ENV: &str = "FRUST_UPDATE_API_URL";
const BASE_ENV: &str = "FRUST_UPDATE_DOWNLOAD_BASE";
const CHECK_ENV: &str = "FRUST_UPDATE_CHECK";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub prerelease: bool,
}

impl Version {
    /// Accepts an optional leading `v`; a `-suffix` marks a prerelease and `+build` metadata is ignored.
    pub fn parse(text: &str) -> Option<Version> {
        let text = text.trim();
        let text = text.strip_prefix('v').unwrap_or(text);
        let text = text.split('+').next()?;
        let (core, prerelease) = match text.split_once('-') {
            Some((core, _)) => (core, true),
            None => (text, false),
        };
        let mut parts = core.split('.');
        let major = number(parts.next()?)?;
        let minor = number(parts.next()?)?;
        let patch = number(parts.next()?)?;
        if parts.next().is_some() {
            return None;
        }
        Some(Version { major, minor, patch, prerelease })
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Version) -> Ordering {
        let key = |v: &Version| (v.major, v.minor, v.patch, !v.prerelease);
        key(self).cmp(&key(other))
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Version) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

fn number(text: &str) -> Option<u64> {
    if text.is_empty() || text.len() > 9 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// The only tag shape that may reach a URL or a path: `v?MAJOR.MINOR.PATCH`.
pub fn validate_tag(tag: &str) -> bool {
    let body = tag.strip_prefix('v').unwrap_or(tag);
    let parts: Vec<&str> = body.split('.').collect();
    parts.len() == 3 && parts.iter().all(|p| number(p).is_some())
}

fn normalize_tag(text: &str) -> Option<String> {
    let text = text.trim();
    if !validate_tag(text) {
        return None;
    }
    Some(if text.starts_with('v') { text.to_string() } else { format!("v{text}") })
}

pub fn target_triple_for(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-gnu"),
        ("linux", "x86_64") => Some("x86_64-unknown-linux-gnu"),
        ("windows", "aarch64") => Some("aarch64-pc-windows-msvc"),
        ("windows", "x86_64") => Some("x86_64-pc-windows-msvc"),
        _ => None,
    }
}

pub fn current_target() -> Option<&'static str> {
    target_triple_for(env::consts::OS, env::consts::ARCH)
}

pub fn archive_name(target: &str) -> String {
    let ext = if target.contains("windows") { "zip" } else { "tar.gz" };
    format!("frust-{target}.{ext}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallMethod {
    Cargo,
    Homebrew,
    Scoop,
    WindowsApps,
    Direct,
}

pub fn detect_install_method(exe: &Path) -> InstallMethod {
    const BREW_PREFIXES: [&str; 3] = ["/opt/homebrew", "/usr/local/Cellar", "/home/linuxbrew"];
    if BREW_PREFIXES.iter().any(|p| exe.starts_with(p)) {
        return InstallMethod::Homebrew;
    }
    // Windows paths compare without regard to case.
    let parts: Vec<String> = exe
        .components()
        .map(|c| {
            let text = c.as_os_str().to_string_lossy();
            if cfg!(windows) { text.to_ascii_lowercase() } else { text.into_owned() }
        })
        .collect();
    let follows = |a: &str, b: &[&str]| parts.windows(2).any(|w| w[0] == a && b.contains(&w[1].as_str()));
    if follows(".cargo", &["bin"]) {
        InstallMethod::Cargo
    } else if follows("scoop", &["apps", "shims"]) {
        InstallMethod::Scoop
    } else if parts.iter().any(|p| p.eq_ignore_ascii_case("windowsapps")) {
        InstallMethod::WindowsApps
    } else {
        InstallMethod::Direct
    }
}

pub struct Sha256 {
    state: [u32; 8],
    buf: [u8; 64],
    buf_len: usize,
    total: u64,
}

const INITIAL_STATE: [u32; 8] = [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be,
    0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa,
    0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85,
    0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3,
    0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f,
    0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

impl Default for Sha256 {
    fn default() -> Sha256 {
        Sha256::new()
    }
}

impl Sha256 {
    pub fn new() -> Sha256 {
        Sha256 { state: INITIAL_STATE, buf: [0; 64], buf_len: 0, total: 0 }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.total = self.total.wrapping_add(data.len() as u64);
        if self.buf_len > 0 {
            let take = (64 - self.buf_len).min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len == 64 {
                let block = self.buf;
                compress(&mut self.state, &block);
                self.buf_len = 0;
            }
        }
        while data.len() >= 64 {
            let (block, rest) = data.split_at(64);
            compress(&mut self.state, block.try_into().expect("a 64 byte block"));
            data = rest;
        }
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.buf_len = data.len();
        }
    }

    pub fn finalize(mut self) -> [u8; 32] {
        let bits = self.total.wrapping_mul(8);
        let used = (self.total % 64) as usize;
        let zeros = if used < 56 { 55 - used } else { 119 - used };
        let mut pad = vec![0x80u8];
        pad.extend(std::iter::repeat_n(0u8, zeros));
        pad.extend_from_slice(&bits.to_be_bytes());
        self.update(&pad);
        let mut out = [0u8; 32];
        for (chunk, word) in out.as_chunks_mut::<4>().0.iter_mut().zip(self.state) {
            chunk.copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}

#[allow(clippy::needless_range_loop)]
fn compress(state: &mut [u32; 8], block: &[u8; 64]) {
    let mut w = [0u32; 64];
    for (i, chunk) in block.as_chunks::<4>().0.iter().enumerate() {
        w[i] = u32::from_be_bytes(*chunk);
    }
    for i in 16..64 {
        let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
        let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
    }
    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
    for i in 0..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ (!e & g);
        let t1 = h.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let t2 = s0.wrapping_add(maj);
        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }
    for (slot, v) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
        *slot = slot.wrapping_add(v);
    }
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    to_hex(&hasher.finalize())
}

pub fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        hasher.update(&chunk[..n]);
    }
    Ok(to_hex(&hasher.finalize()))
}

/// The first line of a `.sha256` asset is `<hex>  <filename>`; a file naming another archive is refused.
pub fn parse_checksum(text: &str, archive: &str) -> Result<String, String> {
    let line = text.lines().find(|l| !l.trim().is_empty()).ok_or("the checksum file is empty")?;
    let mut fields = line.split_whitespace();
    let hash = fields.next().unwrap_or("");
    let file = fields.next().ok_or("the checksum file does not name the archive")?.trim_start_matches('*');
    if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("the checksum file does not hold a SHA-256 digest".to_string());
    }
    if file != archive {
        return Err(format!("the checksum file is for {file:?}, not {archive}"));
    }
    Ok(hash.to_ascii_lowercase())
}

/// Rejects parent references, absolute paths and drive prefixes in an archive member name.
pub fn member_is_safe(path: &str) -> bool {
    let p = path.replace('\\', "/");
    let bytes = p.as_bytes();
    if p.starts_with('/') || p.contains('\0') || (bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic()) {
        return false;
    }
    !p.split('/').any(|part| part == "..")
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cache {
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(default)]
    pub checked_at: u64,
    #[serde(default)]
    pub notified_tag: Option<String>,
    #[serde(default)]
    pub notified_at: u64,
}

fn update_dir(dirs: &Dirs) -> PathBuf {
    dirs.root().join("update")
}

fn cache_path(dirs: &Dirs) -> PathBuf {
    update_dir(dirs).join("latest.json")
}

fn disabled_marker(dirs: &Dirs) -> PathBuf {
    update_dir(dirs).join("disabled")
}

pub fn read_cache(dirs: &Dirs) -> Cache {
    fs::read(cache_path(dirs)).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
}

fn write_cache(dirs: &Dirs, cache: &Cache) -> io::Result<()> {
    let bytes = serde_json::to_vec(cache).map_err(io::Error::other)?;
    fsx::write_atomic(&cache_path(dirs), &bytes)
}

fn record_latest(dirs: &Dirs, tag: &str) {
    let mut cache = read_cache(dirs);
    cache.tag = Some(tag.to_string());
    cache.checked_at = fsx::unix_now();
    let _ = write_cache(dirs, &cache);
}

pub fn env_disables(value: Option<&str>) -> bool {
    value.is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "off" | "0" | "false" | "no"))
}

pub fn checks_disabled(dirs: &Dirs) -> bool {
    env_disables(env::var(CHECK_ENV).ok().as_deref()) || disabled_marker(dirs).exists()
}

#[derive(Debug, Clone, Copy)]
pub struct NoticeContext<'a> {
    pub is_tty: bool,
    pub ci: bool,
    pub bypass: bool,
    pub disabled: bool,
    pub now: u64,
    pub cache: &'a Cache,
    pub current: Version,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct NoticeDecision {
    /// The tag to announce, when a newer release is known and not announced within the last day.
    pub print: Option<String>,
    /// Whether the cached check is stale and a background check should be started.
    pub refresh: bool,
}

pub fn decide_notice(ctx: &NoticeContext) -> NoticeDecision {
    if !ctx.is_tty || ctx.ci || ctx.bypass || ctx.disabled {
        return NoticeDecision::default();
    }
    let refresh = ctx.now.saturating_sub(ctx.cache.checked_at) >= CHECK_EVERY_SECS;
    let print = ctx
        .cache
        .tag
        .as_deref()
        .filter(|tag| validate_tag(tag))
        .filter(|tag| Version::parse(tag).is_some_and(|latest| latest > ctx.current))
        .filter(|tag| {
            let announced_recently = ctx.cache.notified_tag.as_deref() == Some(*tag) && ctx.now.saturating_sub(ctx.cache.notified_at) < NOTIFY_EVERY_SECS;
            !announced_recently
        })
        .map(str::to_string);
    NoticeDecision { print, refresh }
}

/// Called after a build finishes: at most one dim stderr line, and at most one detached check per day. Every failure is swallowed.
pub fn notice(dirs: &Dirs) {
    let is_tty = io::stderr().is_terminal();
    let ci = env::var_os("CI").is_some();
    let bypass = env::var_os("FRUST_BYPASS").is_some();
    if !is_tty || ci || bypass {
        return;
    }
    let Some(current) = Version::parse(env!("CARGO_PKG_VERSION")) else { return };
    let mut cache = read_cache(dirs);
    let now = fsx::unix_now();
    let ctx = NoticeContext { is_tty, ci, bypass, disabled: checks_disabled(dirs), now, cache: &cache, current };
    let decision = decide_notice(&ctx);
    let mut dirty = false;
    if decision.refresh {
        // Claimed before spawning so concurrent builds start one checker between them.
        cache.checked_at = now;
        dirty = true;
    }
    if let Some(tag) = &decision.print {
        let style = Styler { on: env::var_os("NO_COLOR").is_none_or(|v| v.is_empty()) };
        let line = format!("frust {} is available (you have {current}): run `frust self-update`", tag.trim_start_matches('v'));
        eprintln!("{}", style.dim(&line));
        cache.notified_tag = Some(tag.clone());
        cache.notified_at = now;
        dirty = true;
    }
    if dirty {
        let _ = write_cache(dirs, &cache);
    }
    if decision.refresh {
        spawn_checker(dirs);
    }
}

/// The `frust` binary next to this executable, or one level up when running from cargo's `deps` directory.
fn frust_binary() -> Option<PathBuf> {
    let name = format!("frust{}", env::consts::EXE_SUFFIX);
    let exe = env::current_exe().ok()?;
    let exe = exe.canonicalize().unwrap_or(exe);
    let dir = exe.parent()?;
    [Some(dir.join(&name)), dir.parent().map(|p| p.join(&name))].into_iter().flatten().find(|p| p.is_file())
}

fn spawn_checker(dirs: &Dirs) -> bool {
    let Some(exe) = frust_binary() else { return false };
    let mut cmd = Command::new(exe);
    cmd.args(["self-update", BACKGROUND_FLAG]).env("FRUST_HOME", dirs.root());
    sys::detach_command(&mut cmd, &[]);
    cmd.spawn().is_ok()
}

fn overrides_active() -> bool {
    env::var_os(API_ENV).is_some() || env::var_os(BASE_ENV).is_some()
}

fn api_url() -> String {
    env::var(API_ENV).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| DEFAULT_API_URL.to_string())
}

fn download_base() -> String {
    let base = env::var(BASE_ENV).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| DEFAULT_DOWNLOAD_BASE.to_string());
    base.trim_end_matches('/').to_string()
}

fn release_url(requested: Option<&str>) -> Result<String, String> {
    let api = api_url();
    match requested {
        None => Ok(api),
        Some(tag) => match api.strip_suffix("latest") {
            Some(prefix) => Ok(format!("{prefix}tags/{tag}")),
            None => Err(format!("cannot derive a per-tag URL from {api}")),
        },
    }
}

pub fn parse_release_tag(body: &[u8]) -> Result<String, String> {
    let value: serde_json::Value = serde_json::from_slice(body).map_err(|e| format!("unreadable release data: {e}"))?;
    let tag = value.get("tag_name").and_then(|t| t.as_str()).ok_or("release data has no tag_name")?;
    if !validate_tag(tag) {
        return Err(format!("invalid release tag {tag:?}"));
    }
    Ok(tag.to_string())
}

fn fetch_release_tag(requested: Option<&str>) -> Result<String, String> {
    let body = transfer(&release_url(requested)?, None, API_TIMEOUT_SECS)?;
    let tag = parse_release_tag(&body)?;
    if let Some(want) = requested
        && tag != want
    {
        return Err(format!("requested {want} but the server answered {tag}"));
    }
    Ok(tag)
}

/// Fetches `url` to stdout bytes, or into `out` (returning no bytes), with curl first and a platform fallback after.
fn transfer(url: &str, out: Option<&Path>, max_secs: u32) -> Result<Vec<u8>, String> {
    if !overrides_active() && !url.starts_with("https://") {
        return Err(format!("refusing a non-https URL: {url}"));
    }
    let ua = format!("frust/{}", env!("CARGO_PKG_VERSION"));
    if let Some(result) = run_curl(url, out, max_secs, &ua) {
        return result;
    }
    if let Some(result) = fallback_transfer(url, out, max_secs, &ua) {
        return result;
    }
    Err("no download tool found; install curl".to_string())
}

/// `None` means the program is not installed, so the next backend is tried.
fn run_tool(mut cmd: Command, name: &str) -> Option<Result<Vec<u8>, String>> {
    cmd.stdin(Stdio::null());
    match cmd.output() {
        Err(err) if err.kind() == io::ErrorKind::NotFound => None,
        Err(err) => Some(Err(format!("cannot run {name}: {err}"))),
        Ok(output) if output.status.success() => Some(Ok(output.stdout)),
        Ok(output) => Some(Err(format!("{name} failed ({}): {}", output.status, String::from_utf8_lossy(&output.stderr).trim()))),
    }
}

fn run_curl(url: &str, out: Option<&Path>, max_secs: u32, ua: &str) -> Option<Result<Vec<u8>, String>> {
    let program = if cfg!(windows) { "curl.exe" } else { "curl" };
    let proto = if overrides_active() { "=https,file" } else { "=https" };
    let mut cmd = Command::new(program);
    cmd.args(["-fsSL", "--proto", proto, "--tlsv1.2", "--connect-timeout", "5", "--max-time"])
        .arg(max_secs.to_string())
        .arg("-H")
        .arg(format!("User-Agent: {ua}"));
    if let Some(out) = out {
        cmd.arg("-o").arg(out);
    }
    cmd.arg(url);
    run_tool(cmd, program)
}

#[cfg(not(windows))]
fn fallback_transfer(url: &str, out: Option<&Path>, max_secs: u32, ua: &str) -> Option<Result<Vec<u8>, String>> {
    let mut cmd = Command::new("wget");
    cmd.args(["-q", "--tries=1", "--secure-protocol=TLSv1_2"]).arg(format!("--timeout={max_secs}")).arg(format!("--user-agent={ua}"));
    if !overrides_active() {
        cmd.arg("--https-only");
    }
    cmd.arg("-O").arg(out.map(|p| p.as_os_str().to_os_string()).unwrap_or_else(|| std::ffi::OsString::from("-")));
    cmd.arg(url);
    run_tool(cmd, "wget")
}

#[cfg(windows)]
fn fallback_transfer(url: &str, out: Option<&Path>, max_secs: u32, ua: &str) -> Option<Result<Vec<u8>, String>> {
    const SCRIPT: &str = "$ProgressPreference='SilentlyContinue'; $ErrorActionPreference='Stop'; \
        [Net.ServicePointManager]::SecurityProtocol=[Net.SecurityProtocolType]::Tls12; \
        if ($env:FRUST_OUT) { Invoke-WebRequest -UseBasicParsing -UserAgent $env:FRUST_UA -TimeoutSec $env:FRUST_SECS -Uri $env:FRUST_URL -OutFile $env:FRUST_OUT } \
        else { [Console]::Out.Write((Invoke-WebRequest -UseBasicParsing -UserAgent $env:FRUST_UA -TimeoutSec $env:FRUST_SECS -Uri $env:FRUST_URL).Content) }";
    let mut cmd = Command::new("powershell");
    cmd.args(["-NoProfile", "-NonInteractive", "-Command", SCRIPT])
        .env("FRUST_URL", url)
        .env("FRUST_UA", ua)
        .env("FRUST_SECS", max_secs.to_string())
        .env_remove("FRUST_OUT");
    if let Some(out) = out {
        cmd.env("FRUST_OUT", out);
    }
    run_tool(cmd, "powershell")
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Options {
    yes: bool,
    check: bool,
    disable: bool,
    enable: bool,
    background: bool,
    version: Option<String>,
}

fn parse_options(flags: &[String]) -> Result<Options, String> {
    let mut opts = Options::default();
    let mut it = flags.iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--yes" | "-y" => opts.yes = true,
            "--check" => opts.check = true,
            "--disable" => opts.disable = true,
            "--enable" => opts.enable = true,
            BACKGROUND_FLAG => opts.background = true,
            "--version" => opts.version = Some(it.next().ok_or("--version needs a release tag such as v0.2.0")?.clone()),
            other => match other.strip_prefix("--version=") {
                Some(value) => opts.version = Some(value.to_string()),
                None => return Err(format!("unknown option {other}")),
            },
        }
    }
    if opts.disable && opts.enable {
        return Err("--disable and --enable cannot be combined".to_string());
    }
    Ok(opts)
}

/// The `frust self-update` command; returns the process exit code.
pub fn run(dirs: &Dirs, flags: &[String]) -> i32 {
    let opts = match parse_options(flags) {
        Ok(opts) => opts,
        Err(message) => {
            eprintln!("frust: {message}");
            eprintln!("usage: frust self-update [--yes] [--check] [--version vX.Y.Z] [--disable | --enable]");
            return 2;
        }
    };
    if opts.background {
        return check_in_background(dirs);
    }
    if opts.disable || opts.enable {
        return set_disabled(dirs, opts.disable);
    }
    let Some(current) = Version::parse(env!("CARGO_PKG_VERSION")) else {
        eprintln!("frust: cannot read the running version");
        return 1;
    };
    let requested = match opts.version.as_deref() {
        Some(text) => match normalize_tag(text) {
            Some(tag) => Some(tag),
            None => {
                eprintln!("frust: invalid release tag {text:?}; expected vMAJOR.MINOR.PATCH");
                return 2;
            }
        },
        None => None,
    };
    let latest = match fetch_release_tag(requested.as_deref()) {
        Ok(tag) => tag,
        Err(err) => {
            eprintln!("frust: could not look up the release: {err}");
            return 1;
        }
    };
    let Some(target_version) = Version::parse(&latest) else {
        eprintln!("frust: invalid release tag {latest:?}");
        return 1;
    };
    if requested.is_none() {
        record_latest(dirs, &latest);
    }
    let wanted = if requested.is_some() { target_version != current } else { target_version > current };
    if !wanted {
        println!("frust is already up to date ({current})");
        return 0;
    }
    let shown = latest.trim_start_matches('v').to_string();
    if opts.check {
        println!("frust {shown} is available (you have {current})");
        println!("run `frust self-update` to install it");
        return EXIT_UPDATE_AVAILABLE;
    }

    let exe = match env::current_exe().and_then(|p| p.canonicalize()) {
        Ok(exe) => exe,
        Err(err) => {
            eprintln!("frust: cannot locate the running executable: {err}");
            return 1;
        }
    };
    match detect_install_method(&exe) {
        InstallMethod::Cargo => {
            println!("installed with cargo: run `cargo install frust-rs` (or `cargo binstall frust-rs`) to update");
            return 0;
        }
        InstallMethod::Homebrew => {
            println!("installed with Homebrew: run `brew upgrade frust` to update");
            return 0;
        }
        InstallMethod::Scoop => {
            println!("installed with Scoop: run `scoop update frust`");
            return 0;
        }
        InstallMethod::WindowsApps => {
            println!("installed as a Windows app: update it with `winget upgrade frust` or through the Microsoft Store");
            return 0;
        }
        InstallMethod::Direct => {}
    }
    let Some(target) = current_target() else {
        eprintln!("frust: no prebuilt release for this platform; use cargo install frust-rs");
        return 1;
    };
    let Some(install_dir) = exe.parent().map(Path::to_path_buf) else {
        eprintln!("frust: cannot locate the install directory");
        return 1;
    };
    cleanup_leftovers(&install_dir);

    println!("frust {current} -> {shown}  ({})", install_dir.display());
    if !opts.yes {
        if !io::stdin().is_terminal() {
            eprintln!("frust: refusing to update without confirmation; re-run with --yes");
            return 2;
        }
        print!("Update now? [Y/n] ");
        let _ = io::stdout().flush();
        let mut answer = String::new();
        let _ = io::stdin().lock().read_line(&mut answer);
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "" | "y" | "yes") {
            println!("aborted");
            return 1;
        }
    }
    match install_release(&latest, target, &install_dir) {
        Ok(()) => {
            let style = Styler::detect();
            println!("{} frust {current} -> {shown}", style.green("updated"));
            println!("replaced {} in {}", BINARIES.join(", "), install_dir.display());
            println!("run `frust doctor` to check your environment");
            0
        }
        Err(err) => {
            eprintln!("frust: update failed: {err}");
            1
        }
    }
}

fn check_in_background(dirs: &Dirs) -> i32 {
    let fetched = fetch_release_tag(None).ok();
    // Read after the fetch so a notice recorded meanwhile is not overwritten.
    let mut cache = read_cache(dirs);
    cache.checked_at = fsx::unix_now();
    if let Some(tag) = fetched {
        cache.tag = Some(tag);
    }
    let _ = write_cache(dirs, &cache);
    0
}

fn set_disabled(dirs: &Dirs, disable: bool) -> i32 {
    let marker = disabled_marker(dirs);
    if disable {
        match fsx::write_atomic(&marker, b"update checks disabled\n") {
            Ok(()) => {
                println!("update checks disabled; `frust self-update --enable` turns them back on");
                0
            }
            Err(err) => {
                eprintln!("frust: cannot write {}: {err}", marker.display());
                1
            }
        }
    } else {
        match fsx::remove_any(&marker) {
            Ok(()) => {
                println!("update checks enabled");
                0
            }
            Err(err) => {
                eprintln!("frust: cannot remove {}: {err}", marker.display());
                1
            }
        }
    }
}

struct StagingGuard(PathBuf);

impl Drop for StagingGuard {
    fn drop(&mut self) {
        let _ = fsx::remove_any(&self.0);
    }
}

fn binary_files() -> Vec<String> {
    BINARIES.iter().map(|b| format!("{b}{}", env::consts::EXE_SUFFIX)).collect()
}

fn io_message(what: &str) -> impl Fn(io::Error) -> String + '_ {
    move |err| format!("{what}: {err}")
}

fn install_release(tag: &str, target: &str, install_dir: &Path) -> Result<(), String> {
    let staging = install_dir.join(format!("{STAGING_PREFIX}{}", std::process::id()));
    let _ = fsx::remove_any(&staging);
    fs::create_dir(&staging).map_err(|e| format!("cannot create a staging directory in {}: {e}", install_dir.display()))?;
    let _guard = StagingGuard(staging.clone());
    let download = staging.join("download");
    let extract = staging.join("extract");
    let backup = staging.join("backup");
    let home = staging.join("home");
    for dir in [&download, &extract, &backup, &home] {
        fs::create_dir(dir).map_err(io_message("cannot create a staging directory"))?;
    }

    let archive = archive_name(target);
    let base = download_base();
    let archive_path = download.join(&archive);
    let checksum_path = download.join(format!("{archive}.sha256"));
    transfer(&format!("{base}/{tag}/{archive}"), Some(&archive_path), DOWNLOAD_TIMEOUT_SECS).map_err(|e| format!("download failed: {e}"))?;
    transfer(&format!("{base}/{tag}/{archive}.sha256"), Some(&checksum_path), API_TIMEOUT_SECS).map_err(|e| format!("checksum download failed: {e}"))?;

    let checksum_text = fs::read_to_string(&checksum_path).map_err(io_message("cannot read the checksum file"))?;
    let expected = parse_checksum(&checksum_text, &archive)?;
    let actual = sha256_file(&archive_path).map_err(io_message("cannot hash the download"))?;
    if expected != actual {
        return Err(format!("checksum mismatch for {archive} (expected {expected}, got {actual}); nothing was installed"));
    }

    extract_archive(&archive_path, &extract)?;
    verify_extracted(&extract)?;
    verify_staged_version(&extract, &home, tag.trim_start_matches('v'))?;
    swap_binaries(&extract, install_dir, &backup)
}

fn extract_archive(archive: &Path, dest: &Path) -> Result<(), String> {
    let listing = Command::new("tar")
        .arg("-tf")
        .arg(archive)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run tar: {e}"))?;
    if !listing.status.success() {
        return Err("the downloaded archive cannot be read".to_string());
    }
    for member in String::from_utf8_lossy(&listing.stdout).lines() {
        if !member_is_safe(member) {
            return Err(format!("the archive contains an unsafe path {member:?}"));
        }
    }
    let extracted = Command::new("tar")
        .arg("-xf")
        .arg(archive)
        .arg("-C")
        .arg(dest)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run tar: {e}"))?;
    if !extracted.status.success() {
        return Err(format!("extracting the archive failed: {}", String::from_utf8_lossy(&extracted.stderr).trim()));
    }
    Ok(())
}

#[cfg(unix)]
fn looks_executable(_name: &str, meta: &fs::Metadata) -> bool {
    sys::Meta::from(meta.clone()).mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn looks_executable(name: &str, _meta: &fs::Metadata) -> bool {
    let lower = name.to_ascii_lowercase();
    [".exe", ".bat", ".cmd", ".ps1", ".com", ".dll"].iter().any(|ext| lower.ends_with(ext))
}

/// The extracted tree must hold exactly the expected binaries as regular files, plus only non-executable extras.
fn verify_extracted(dir: &Path) -> Result<(), String> {
    let wanted = binary_files();
    for name in &wanted {
        match fs::symlink_metadata(dir.join(name)) {
            Ok(meta) if meta.is_file() => {}
            _ => return Err(format!("the archive is missing {name}")),
        }
    }
    for entry in fs::read_dir(dir).map_err(io_message("cannot list the extracted files"))? {
        let entry = entry.map_err(io_message("cannot list the extracted files"))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let meta = fs::symlink_metadata(entry.path()).map_err(io_message("cannot inspect the extracted files"))?;
        let expected = wanted.contains(&name) || EXTRA_FILES.contains(&name.as_str());
        if !meta.is_file() || (!expected && looks_executable(&name, &meta)) {
            return Err(format!("the archive contains an unexpected entry {name:?}"));
        }
    }
    Ok(())
}

/// The first line must be exactly `frust <version>`, so 0.1.10 never satisfies 0.1.1.
fn reports_version(output: &str, version: &str) -> bool {
    output.lines().next().is_some_and(|line| line.trim() == format!("frust {version}"))
}

/// The one downloaded program that is ever run, and only to read its version.
fn verify_staged_version(extract: &Path, home: &Path, version: &str) -> Result<(), String> {
    let exe = extract.join(format!("frust{}", env::consts::EXE_SUFFIX));
    let out_path = home.join("version.out");
    let out_file = File::create(&out_path).map_err(io_message("cannot create the version probe file"))?;
    let mut child = Command::new(&exe)
        .arg("--version")
        .env("FRUST_HOME", home)
        .env("FRUST_BYPASS", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_file))
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot run the downloaded frust: {e}"))?;
    let deadline = Instant::now() + VERSION_CHECK_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("the downloaded frust did not answer --version in time".to_string());
            }
            Err(err) => return Err(format!("cannot wait for the downloaded frust: {err}")),
        }
    }
    let text = fs::read_to_string(&out_path).unwrap_or_default();
    if !reports_version(&text, version) {
        return Err(format!("the downloaded frust reports {:?}, expected version {version}", text.trim()));
    }
    Ok(())
}

/// Backs every live binary up first, then replaces them one by one; a failure restores the ones already touched.
fn swap_binaries(extract: &Path, install_dir: &Path, backup: &Path) -> Result<(), String> {
    let names = binary_files();
    for name in &names {
        let live = install_dir.join(name);
        if live.exists() {
            fs::copy(&live, backup.join(name)).map_err(|e| format!("cannot back up {name}: {e}"))?;
        }
    }
    let mut touched: Vec<String> = Vec::new();
    for name in &names {
        touched.push(name.clone());
        if let Err(err) = install_file(&extract.join(name), &install_dir.join(name)) {
            let problems = rollback(&touched, install_dir, backup);
            if problems.is_empty() {
                cleanup_leftovers(install_dir);
            }
            return Err(if problems.is_empty() {
                format!("cannot replace {name}: {err}; the previous binaries were restored")
            } else {
                format!("cannot replace {name}: {err}; restoring failed for {}; reinstall with install.sh or install.ps1", problems.join(", "))
            });
        }
    }
    // The running executable's `.old` cannot be deleted on Windows; the next update removes it.
    cleanup_leftovers(install_dir);
    Ok(())
}

fn rollback(touched: &[String], install_dir: &Path, backup: &Path) -> Vec<String> {
    let mut problems = Vec::new();
    for name in touched.iter().rev() {
        let live = install_dir.join(name);
        let saved = backup.join(name);
        let result = if saved.exists() { install_file(&saved, &live) } else { fsx::remove_any(&live) };
        if let Err(err) = result {
            problems.push(format!("{name}: {err}"));
        }
    }
    problems
}

/// Copies beside the destination and renames over it, so the replacement is atomic and works while the old file runs.
fn install_file(src: &Path, dest: &Path) -> io::Result<()> {
    let mut name = dest.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".new");
    let staged = dest.with_file_name(name);
    let result = (|| -> io::Result<()> {
        fs::copy(src, &staged)?;
        make_executable(&staged)?;
        make_room(dest)?;
        replace_file(&staged, dest)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&staged);
    }
    result
}

fn replace_file(from: &Path, to: &Path) -> io::Result<()> {
    sys::rename(from, to)
}

fn make_executable(path: &Path) -> io::Result<()> {
    sys::set_path_mode(path, 0o755)
}

#[cfg(windows)]
fn make_room(dest: &Path) -> io::Result<()> {
    if !dest.exists() {
        return Ok(());
    }
    let mut aside = dest.as_os_str().to_os_string();
    aside.push(".old");
    let aside = PathBuf::from(aside);
    let _ = fs::remove_file(&aside);
    replace_file(dest, &aside)
}

// A running executable can be renamed over on unix, so nothing has to move aside.
#[cfg(not(windows))]
fn make_room(_dest: &Path) -> io::Result<()> {
    Ok(())
}

/// Removes `.old` files from a previous Windows update and staging directories abandoned by a killed update.
fn cleanup_leftovers(install_dir: &Path) {
    let Ok(entries) = fs::read_dir(install_dir) else { return };
    let binaries = binary_files();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let old_binary = name.strip_suffix(".old").is_some_and(|stem| binaries.iter().any(|b| b == stem));
        let abandoned = name.starts_with(STAGING_PREFIX)
            && entry.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age > STALE_STAGING_AGE);
        if old_binary || abandoned {
            let _ = fsx::remove_any(&entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    #[test]
    fn the_staged_binary_must_report_exactly_the_expected_version() {
        assert!(reports_version("frust 0.1.1\n", "0.1.1"));
        assert!(reports_version("frust 0.1.1\ncargo 1.90.0\n", "0.1.1"));
        assert!(reports_version("frust 0.1.1\r\n", "0.1.1"));
        assert!(!reports_version("frust 0.1.10\n", "0.1.1"), "a longer version is another version");
        assert!(!reports_version("frust 0.1.1\n", "0.1.10"));
        assert!(!reports_version("frust 0.1.1 extra\n", "0.1.1"), "trailing text on the line");
        assert!(!reports_version("frust 0.1.1-rc.1\n", "0.1.1"));
        assert!(!reports_version("xfrust 0.1.1\n", "0.1.1"));
        assert!(!reports_version("cargo 1.90.0\nfrust 0.1.1\n", "0.1.1"), "only the first line counts");
        assert!(!reports_version("", "0.1.1"));
    }

    #[test]
    fn versions_parse_and_order() {
        assert_eq!(v("v1.2.3"), Version { major: 1, minor: 2, patch: 3, prerelease: false });
        assert_eq!(v("1.2.3"), v("v1.2.3"));
        assert!(v("0.10.0") > v("0.9.9"));
        assert!(v("1.0.0") > v("0.99.99"));
        assert!(v("0.1.1") > v("0.1.0"));
        assert!(v("1.0.0-rc.1") < v("1.0.0"));
        assert!(v("1.0.0-rc.1") > v("0.9.9"));
        assert_eq!(v("1.0.0+build.5"), v("1.0.0"));
        assert_eq!(v("1.2.3").to_string(), "1.2.3");
        for bad in ["", "1", "1.2", "1.2.3.4", "a.b.c", "1.2.x", "v", "1..3", "-1.2.3"] {
            assert!(Version::parse(bad).is_none(), "{bad:?} must not parse");
        }
    }

    #[test]
    fn tags_are_validated_strictly() {
        for good in ["v0.1.0", "0.1.0", "v10.20.30"] {
            assert!(validate_tag(good), "{good}");
        }
        for bad in ["", "v", "v1.2", "v1.2.3.4", "v1.2.3-rc1", "vv1.2.3", "v1.2.3/../x", "../v1.2.3", "v1.2.3 ", "v1.2.3\n", "v1.2.-3", "V1.2.3", "v1.2.3/"] {
            assert!(!validate_tag(bad), "{bad:?}");
        }
        assert_eq!(normalize_tag("0.2.0").as_deref(), Some("v0.2.0"));
        assert_eq!(normalize_tag("v0.2.0").as_deref(), Some("v0.2.0"));
        assert_eq!(normalize_tag("../0.2.0"), None);
    }

    #[test]
    fn targets_map_to_installer_triples() {
        assert_eq!(target_triple_for("macos", "aarch64"), Some("aarch64-apple-darwin"));
        assert_eq!(target_triple_for("macos", "x86_64"), Some("x86_64-apple-darwin"));
        assert_eq!(target_triple_for("linux", "aarch64"), Some("aarch64-unknown-linux-gnu"));
        assert_eq!(target_triple_for("linux", "x86_64"), Some("x86_64-unknown-linux-gnu"));
        assert_eq!(target_triple_for("windows", "aarch64"), Some("aarch64-pc-windows-msvc"));
        assert_eq!(target_triple_for("windows", "x86_64"), Some("x86_64-pc-windows-msvc"));
        assert_eq!(target_triple_for("freebsd", "x86_64"), None);
        assert_eq!(target_triple_for("linux", "riscv64"), None);
        assert_eq!(archive_name("x86_64-unknown-linux-gnu"), "frust-x86_64-unknown-linux-gnu.tar.gz");
        assert_eq!(archive_name("aarch64-pc-windows-msvc"), "frust-aarch64-pc-windows-msvc.zip");
    }

    #[test]
    fn sha256_matches_the_nist_vectors() {
        assert_eq!(sha256_hex(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        let mut hasher = Sha256::new();
        let thousand = [b'a'; 1000];
        for _ in 0..1000 {
            hasher.update(&thousand);
        }
        assert_eq!(to_hex(&hasher.finalize()), "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0");
    }

    #[test]
    fn sha256_is_independent_of_how_input_is_split() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i * 31 % 251) as u8).collect();
        let whole = sha256_hex(&data);
        for step in [1usize, 7, 55, 56, 63, 64, 65, 128, 999] {
            let mut hasher = Sha256::new();
            for piece in data.chunks(step) {
                hasher.update(piece);
            }
            assert_eq!(to_hex(&hasher.finalize()), whole, "step {step}");
        }
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("frust-update-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn sha256_streams_files() {
        let dir = scratch("sha-file");
        let path = dir.join("data");
        fs::write(&path, b"abc").unwrap();
        assert_eq!(sha256_file(&path).unwrap(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        fs::write(&path, vec![b'a'; 200_000]).unwrap();
        assert_eq!(sha256_file(&path).unwrap(), sha256_hex(&vec![b'a'; 200_000]));
        assert!(sha256_file(&dir.join("missing")).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn checksum_files_must_name_the_archive() {
        let hash = "ab".repeat(32);
        assert_eq!(parse_checksum(&format!("{hash}  frust-x.tar.gz\n"), "frust-x.tar.gz").unwrap(), hash);
        assert_eq!(parse_checksum(&format!("{}  *frust-x.tar.gz", hash.to_uppercase()), "frust-x.tar.gz").unwrap(), hash);
        assert!(parse_checksum(&format!("{hash}  other.tar.gz\n"), "frust-x.tar.gz").is_err());
        assert!(parse_checksum(&format!("{hash}\n"), "frust-x.tar.gz").is_err());
        assert!(parse_checksum("not-a-digest  frust-x.tar.gz", "frust-x.tar.gz").is_err());
        assert!(parse_checksum("\n\n", "frust-x.tar.gz").is_err());
    }

    #[test]
    fn archive_members_are_vetted() {
        for good in ["frust", "./frust", "./", "LICENSE", "bin/frust", "a..b"] {
            assert!(member_is_safe(good), "{good}");
        }
        for bad in ["../frust", "a/../../b", "/etc/passwd", "C:\\x", "c:/x", "..\\x", "a\\..\\b", "\\abs"] {
            assert!(!member_is_safe(bad), "{bad}");
        }
    }

    #[test]
    fn install_method_comes_from_the_path() {
        let method = |p: &str| detect_install_method(Path::new(p));
        assert_eq!(method("/home/me/.cargo/bin/frust"), InstallMethod::Cargo);
        assert_eq!(method("/Users/me/.cargo/bin/frust"), InstallMethod::Cargo);
        assert_eq!(method("/opt/homebrew/Cellar/frust/0.1.0/bin/frust"), InstallMethod::Homebrew);
        assert_eq!(method("/usr/local/Cellar/frust/0.1.0/bin/frust"), InstallMethod::Homebrew);
        assert_eq!(method("/home/linuxbrew/.linuxbrew/bin/frust"), InstallMethod::Homebrew);
        assert_eq!(method("/home/me/.local/bin/frust"), InstallMethod::Direct);
        assert_eq!(method("/home/me/cargo/bin/frust"), InstallMethod::Direct);
        assert_eq!(method("/usr/local/bin/frust"), InstallMethod::Direct);
        assert_eq!(method("/home/me/scoop/apps/frust/current/frust"), InstallMethod::Scoop);
        assert_eq!(method("/home/me/scoop/shims/frust"), InstallMethod::Scoop);
        assert_eq!(method("/home/me/scoop/other/frust"), InstallMethod::Direct);
        assert_eq!(method("/Program Files/WindowsApps/frust/frust"), InstallMethod::WindowsApps);
    }

    #[cfg(windows)]
    #[test]
    fn windows_install_paths_are_recognised() {
        let method = |p: &str| detect_install_method(Path::new(p));
        assert_eq!(method(r"C:\Users\me\.cargo\bin\frust.exe"), InstallMethod::Cargo);
        assert_eq!(method(r"C:\Users\Me\Scoop\apps\frust\current\frust.exe"), InstallMethod::Scoop);
        assert_eq!(method(r"C:\Program Files\WindowsApps\x\frust.exe"), InstallMethod::WindowsApps);
        assert_eq!(method(r"C:\tools\frust.exe"), InstallMethod::Direct);
    }

    #[test]
    fn env_values_that_disable_the_check() {
        for off in ["off", "OFF", "0", "false", "No", " no "] {
            assert!(env_disables(Some(off)), "{off}");
        }
        for on in ["", "on", "1", "yes", "true"] {
            assert!(!env_disables(Some(on)), "{on}");
        }
        assert!(!env_disables(None));
    }

    const NOW: u64 = 10_000_000;

    fn context(cache: &Cache) -> NoticeContext<'_> {
        NoticeContext { is_tty: true, ci: false, bypass: false, disabled: false, now: NOW, cache, current: v("0.1.0") }
    }

    fn known(tag: &str, checked_ago: u64) -> Cache {
        Cache { tag: Some(tag.to_string()), checked_at: NOW - checked_ago, ..Cache::default() }
    }

    #[test]
    fn an_empty_cache_asks_for_a_check_and_prints_nothing() {
        let decision = decide_notice(&context(&Cache::default()));
        assert_eq!(decision, NoticeDecision { print: None, refresh: true });
    }

    #[test]
    fn a_fresh_newer_release_is_announced_without_a_new_check() {
        let decision = decide_notice(&context(&known("v0.2.0", 60)));
        assert_eq!(decision, NoticeDecision { print: Some("v0.2.0".to_string()), refresh: false });
    }

    #[test]
    fn a_stale_check_is_refreshed_even_when_announcing() {
        let decision = decide_notice(&context(&known("v0.2.0", CHECK_EVERY_SECS + 1)));
        assert!(decision.refresh);
        assert_eq!(decision.print.as_deref(), Some("v0.2.0"));
    }

    #[test]
    fn current_or_older_releases_are_not_announced() {
        assert_eq!(decide_notice(&context(&known("v0.1.0", 60))).print, None);
        assert_eq!(decide_notice(&context(&known("v0.0.9", 60))).print, None);
    }

    #[test]
    fn a_version_is_announced_at_most_once_a_day() {
        let mut cache = known("v0.2.0", 60);
        cache.notified_tag = Some("v0.2.0".to_string());
        cache.notified_at = NOW - 3600;
        assert_eq!(decide_notice(&context(&cache)).print, None);
        cache.notified_at = NOW - NOTIFY_EVERY_SECS - 1;
        assert_eq!(decide_notice(&context(&cache)).print.as_deref(), Some("v0.2.0"));
        cache.notified_at = NOW - 3600;
        cache.tag = Some("v0.3.0".to_string());
        assert_eq!(decide_notice(&context(&cache)).print.as_deref(), Some("v0.3.0"));
    }

    #[test]
    fn nothing_happens_without_a_terminal_or_when_opted_out() {
        let cache = known("v0.2.0", CHECK_EVERY_SECS * 3);
        for gate in 0..4 {
            let mut ctx = context(&cache);
            match gate {
                0 => ctx.is_tty = false,
                1 => ctx.ci = true,
                2 => ctx.bypass = true,
                _ => ctx.disabled = true,
            }
            assert_eq!(decide_notice(&ctx), NoticeDecision::default(), "gate {gate}");
        }
    }

    #[test]
    fn a_cached_tag_that_is_not_a_plain_version_is_never_printed() {
        assert_eq!(decide_notice(&context(&known("v9.0.0\x1b[2J", 60))).print, None);
    }

    #[test]
    fn cache_round_trips_and_tolerates_missing_fields() {
        let cache = Cache { tag: Some("v1.2.3".into()), checked_at: 42, notified_tag: Some("v1.2.3".into()), notified_at: 43 };
        let bytes = serde_json::to_vec(&cache).unwrap();
        assert_eq!(serde_json::from_slice::<Cache>(&bytes).unwrap(), cache);
        let partial: Cache = serde_json::from_str(r#"{"tag":"v1.2.3","checked_at":5}"#).unwrap();
        assert_eq!(partial, Cache { tag: Some("v1.2.3".into()), checked_at: 5, notified_tag: None, notified_at: 0 });
    }

    #[test]
    fn the_cache_file_survives_a_round_trip_and_garbage_reads_as_empty() {
        let dir = scratch("cache-file");
        let dirs = Dirs::at(&dir);
        assert_eq!(read_cache(&dirs), Cache::default());
        let cache = Cache { tag: Some("v0.5.0".into()), checked_at: 7, ..Cache::default() };
        write_cache(&dirs, &cache).unwrap();
        assert_eq!(read_cache(&dirs), cache);
        fs::write(cache_path(&dirs), b"{not json").unwrap();
        assert_eq!(read_cache(&dirs), Cache::default());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn release_tags_come_only_from_valid_api_data() {
        assert_eq!(parse_release_tag(br#"{"tag_name":"v0.2.0","name":"x"}"#).unwrap(), "v0.2.0");
        assert!(parse_release_tag(br#"{"tag_name":"v0.2.0/../../x"}"#).is_err());
        assert!(parse_release_tag(br#"{"tag_name":7}"#).is_err());
        assert!(parse_release_tag(br#"{}"#).is_err());
        assert!(parse_release_tag(b"nope").is_err());
    }

    #[test]
    fn options_parse() {
        let parse = |args: &[&str]| parse_options(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>());
        assert_eq!(parse(&[]).unwrap(), Options::default());
        assert!(parse(&["-y"]).unwrap().yes);
        assert!(parse(&["--yes", "--check"]).unwrap().check);
        assert_eq!(parse(&["--version", "v0.2.0"]).unwrap().version.as_deref(), Some("v0.2.0"));
        assert_eq!(parse(&["--version=0.2.0"]).unwrap().version.as_deref(), Some("0.2.0"));
        assert!(parse(&[BACKGROUND_FLAG]).unwrap().background);
        assert!(parse(&["--version"]).is_err());
        assert!(parse(&["--bogus"]).is_err());
        assert!(parse(&["--disable", "--enable"]).is_err());
    }

    fn populate(dir: &Path, names: &[String], content: &str) {
        fs::create_dir_all(dir).unwrap();
        for name in names {
            fs::write(dir.join(name), format!("{content}{name}")).unwrap();
        }
    }

    #[test]
    fn swapping_replaces_every_binary_and_leaves_no_temporaries() {
        let root = scratch("swap-ok");
        let (install, extract, backup) = (root.join("install"), root.join("extract"), root.join("backup"));
        let names = binary_files();
        populate(&install, &names, "old-");
        populate(&extract, &names, "new-");
        fs::create_dir_all(&backup).unwrap();
        swap_binaries(&extract, &install, &backup).unwrap();
        for name in &names {
            assert_eq!(fs::read_to_string(install.join(name)).unwrap(), format!("new-{name}"));
        }
        assert_eq!(fs::read_dir(&install).unwrap().count(), names.len());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_failure_midway_restores_the_previous_binaries() {
        let root = scratch("swap-rollback");
        let (install, extract, backup) = (root.join("install"), root.join("extract"), root.join("backup"));
        let names = binary_files();
        populate(&install, &names, "old-");
        populate(&extract, &names[..names.len() - 1], "new-");
        fs::create_dir_all(&backup).unwrap();
        let err = swap_binaries(&extract, &install, &backup).unwrap_err();
        assert!(err.contains(names.last().unwrap()), "{err}");
        for name in &names {
            assert_eq!(fs::read_to_string(install.join(name)).unwrap(), format!("old-{name}"));
        }
        assert_eq!(fs::read_dir(&install).unwrap().count(), names.len());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn extraction_results_with_extras_are_rejected() {
        let root = scratch("verify");
        let names = binary_files();
        populate(&root, &names, "bin-");
        fs::write(root.join("LICENSE"), "text").unwrap();
        fs::write(root.join("README.md"), "text").unwrap();
        verify_extracted(&root).unwrap();
        fs::write(root.join("notes.txt"), "text").unwrap();
        verify_extracted(&root).unwrap();
        fs::remove_file(root.join("frustfmt".to_string() + env::consts::EXE_SUFFIX)).unwrap();
        assert!(verify_extracted(&root).unwrap_err().contains("missing"));
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn extra_executables_and_links_are_rejected() {
        let root = scratch("verify-extra");
        populate(&root, &binary_files(), "bin-");
        let extra = root.join("helper");
        fs::write(&extra, "x").unwrap();
        sys::set_path_mode(&extra, 0o755).unwrap();
        assert!(verify_extracted(&root).unwrap_err().contains("unexpected"));
        fs::remove_file(&extra).unwrap();
        sys::link_tool(Path::new("/etc/passwd"), &root.join("link")).unwrap();
        assert!(verify_extracted(&root).unwrap_err().contains("unexpected"));
        fs::remove_file(root.join("link")).unwrap();
        fs::remove_file(root.join("LICENSE")).ok();
        sys::link_tool(Path::new("/etc/passwd"), &root.join("LICENSE")).unwrap();
        assert!(verify_extracted(&root).is_err());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn leftovers_are_cleaned_but_unrelated_files_stay() {
        let root = scratch("cleanup");
        let old = format!("frust{}.old", env::consts::EXE_SUFFIX);
        fs::write(root.join(&old), "x").unwrap();
        fs::write(root.join("other.old"), "x").unwrap();
        fs::create_dir_all(root.join(".frust-update-1")).unwrap();
        cleanup_leftovers(&root);
        assert!(!root.join(&old).exists());
        assert!(root.join("other.old").exists());
        assert!(root.join(".frust-update-1").exists(), "a recent staging directory may belong to a live update");
        let _ = fs::remove_dir_all(&root);
    }
}
