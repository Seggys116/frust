//! User JSON roots, workspace namespaces, and the optional artifact cache.
//!
//! This module is standard-library only so the Cargo patch can compile the same file.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const CACHE_MARKER: &str = "frust-cache-v1";

/// Accepted range for the optional `cache_max_bytes` budget: 1 GiB up to 2^53 (exact in a double).
pub const MIN_CACHE_MAX_BYTES: u64 = 1 << 30;
pub const MAX_CACHE_MAX_BYTES: u64 = 1 << 53;

/// How long a reader waits for a concurrent writer before calling a partial marker corrupt.
const MARKER_SETTLE: Duration = Duration::from_millis(100);

/// Deepest object/array nesting the config parser accepts; it recurses on the native stack.
const MAX_JSON_DEPTH: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostOs {
    Linux,
    Macos,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserConfig {
    pub target_root: Option<PathBuf>,
    pub cache_root: Option<PathBuf>,
    pub cache_max_bytes: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigError {
    pub path: String,
    pub reason: String,
}

impl ConfigError {
    pub fn diagnostic(&self) -> String {
        format!(
            "fRust rejected {}: {}. \
             Write {{\"version\":1}} and, if set, absolute \"target_root\" and \"cache_root\" strings \
             and an integer \"cache_max_bytes\" from {} to {}.",
            self.path, self.reason, MIN_CACHE_MAX_BYTES, MAX_CACHE_MAX_BYTES
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedRoots {
    pub config_path: PathBuf,
    pub target_root: PathBuf,
    pub cache_root: PathBuf,
    pub targets_dir: PathBuf,
    pub artifacts_dir: PathBuf,
    pub cache_max_bytes: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CacheOpen {
    Ready,
    Fallback { reason: &'static str },
}

pub fn config_path(os: HostOs, home: &Path, xdg_config_home: Option<&str>) -> PathBuf {
    match os {
        HostOs::Macos => home
            .join("Library")
            .join("Application Support")
            .join("fRust")
            .join("config.json"),
        HostOs::Linux => xdg_dir(xdg_config_home, home, ".config").join("frust").join("config.json"),
    }
}

pub fn default_target_root(os: HostOs, home: &Path, xdg_cache_home: Option<&str>) -> PathBuf {
    default_data_root(os, home, xdg_cache_home)
}

pub fn default_cache_root(os: HostOs, home: &Path, xdg_cache_home: Option<&str>) -> PathBuf {
    default_data_root(os, home, xdg_cache_home)
}

fn default_data_root(os: HostOs, home: &Path, xdg_cache_home: Option<&str>) -> PathBuf {
    match os {
        HostOs::Macos => home.join("Library").join("Caches").join("fRust"),
        HostOs::Linux => xdg_dir(xdg_cache_home, home, ".cache").join("frust"),
    }
}

/// Absolute XDG values win. Empty or relative values use the home default.
fn xdg_dir(raw: Option<&str>, home: &Path, default_child: &str) -> PathBuf {
    if let Some(value) = raw {
        if !value.is_empty() {
            let path = Path::new(value);
            if path.is_absolute() {
                return path.to_path_buf();
            }
        }
    }
    home.join(default_child)
}

pub fn parse_user_config(path: &Path, bytes: &[u8]) -> Result<UserConfig, ConfigError> {
    let text = std::str::from_utf8(bytes).map_err(|_| error(path, "file is not UTF-8"))?;
    let value = parse_json(text).map_err(|reason| error(path, &reason))?;
    let pairs = match value {
        Json::Object(pairs) => pairs,
        _ => return Err(error(path, "top-level value must be an object")),
    };
    let mut version = None;
    let mut target_root = None;
    let mut cache_root = None;
    let mut cache_max_bytes = None;
    for (key, item) in pairs {
        match key.as_str() {
            "version" => version = Some(item),
            "target_root" => target_root = Some(item),
            "cache_root" => cache_root = Some(item),
            "cache_max_bytes" => cache_max_bytes = Some(item),
            other => {
                return Err(error(path, &format!("unknown field `{other}`")));
            }
        }
    }
    match version {
        Some(Json::Number(raw)) if raw == "1" => {}
        Some(other) => {
            let found = other.describe();
            return Err(error(path, &format!("`version` must be the integer 1, found {found}")));
        }
        None => return Err(error(path, "missing `version`")),
    }
    Ok(UserConfig {
        target_root: optional_absolute(path, "target_root", target_root)?,
        cache_root: optional_absolute(path, "cache_root", cache_root)?,
        cache_max_bytes: optional_budget(path, cache_max_bytes)?,
    })
}

/// A plain non-negative integer literal inside the accepted budget range.
fn optional_budget(path: &Path, value: Option<Json>) -> Result<Option<u64>, ConfigError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let reject = |found: String| {
        error(
            path,
            &format!(
                "`cache_max_bytes` must be an integer number of bytes from {MIN_CACHE_MAX_BYTES} \
                 (1 GiB) to {MAX_CACHE_MAX_BYTES} (2^53), found {found}"
            ),
        )
    };
    let Json::Number(raw) = &value else {
        return Err(reject(value.describe()));
    };
    let parsed = if raw.bytes().all(|byte| byte.is_ascii_digit()) { raw.parse::<u64>().ok() } else { None };
    match parsed {
        Some(bytes) if (MIN_CACHE_MAX_BYTES..=MAX_CACHE_MAX_BYTES).contains(&bytes) => Ok(Some(bytes)),
        _ => Err(reject(value.describe())),
    }
}

fn optional_absolute(
    path: &Path,
    field: &str,
    value: Option<Json>,
) -> Result<Option<PathBuf>, ConfigError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let text = match value {
        Json::String(text) => text,
        other => {
            let found = other.describe();
            return Err(error(path, &format!("`{field}` must be a string, found {found}")));
        }
    };
    if text.contains('\0') {
        return Err(error(path, &format!("`{field}` must not contain a NUL character")));
    }
    let candidate = PathBuf::from(&text);
    if text.is_empty() || !candidate.is_absolute() {
        return Err(error(
            path,
            &format!("`{field}` must be an absolute path, got `{text}`"),
        ));
    }
    Ok(Some(candidate))
}

pub fn resolve_roots(
    os: HostOs,
    home: &Path,
    xdg_config_home: Option<&str>,
    xdg_cache_home: Option<&str>,
    config_bytes: Option<&[u8]>,
) -> Result<ResolvedRoots, ConfigError> {
    let config_path = absolute_config_path(os, home, xdg_config_home)?;
    let config = match config_bytes {
        None => None,
        Some(bytes) => Some(parse_user_config(&config_path, bytes)?),
    };
    let target_root = config
        .as_ref()
        .and_then(|cfg| cfg.target_root.clone())
        .unwrap_or_else(|| default_target_root(os, home, xdg_cache_home));
    let cache_root = config
        .as_ref()
        .and_then(|cfg| cfg.cache_root.clone())
        .unwrap_or_else(|| default_cache_root(os, home, xdg_cache_home));
    for root in [&target_root, &cache_root] {
        require_absolute_root(&config_path, root, home)?;
    }
    Ok(ResolvedRoots {
        config_path,
        targets_dir: target_root.join("targets"),
        artifacts_dir: cache_root.join("artifacts"),
        cache_max_bytes: config.as_ref().and_then(|cfg| cfg.cache_max_bytes),
        target_root,
        cache_root,
    })
}

/// A relative home would make every derived root depend on the working directory.
fn absolute_config_path(
    os: HostOs,
    home: &Path,
    xdg_config_home: Option<&str>,
) -> Result<PathBuf, ConfigError> {
    let path = config_path(os, home, xdg_config_home);
    require_absolute_root(&path, &path, home)?;
    Ok(path)
}

fn require_absolute_root(config_path: &Path, root: &Path, home: &Path) -> Result<(), ConfigError> {
    if root.is_absolute() {
        return Ok(());
    }
    Err(error(
        config_path,
        &format!(
            "home directory `{}` is not absolute, so `{}` would depend on the working directory; \
             set HOME or the XDG variables to absolute paths",
            home.display(),
            root.display()
        ),
    ))
}

/// Name of the environment variable that moves every fRust root to one directory.
pub const HOME_ENV: &str = "FRUST_HOME";

/// Apply `FRUST_HOME`: when set and non-empty it replaces both roots, so targets live in
/// `$FRUST_HOME/targets` and the artifact cache in `$FRUST_HOME/artifacts`. It outranks the
/// roots in `config.json`, which keeps the other settings of that file.
pub fn apply_frust_home(
    roots: ResolvedRoots,
    frust_home: Option<&str>,
) -> Result<ResolvedRoots, ConfigError> {
    let Some(text) = frust_home.filter(|text| !text.is_empty()) else {
        return Ok(roots);
    };
    let home = PathBuf::from(text);
    if text.contains('\0') || !home.is_absolute() {
        return Err(error(
            &roots.config_path,
            &format!("{HOME_ENV} must be an absolute path, got `{text}`"),
        ));
    }
    Ok(ResolvedRoots {
        targets_dir: home.join("targets"),
        artifacts_dir: home.join("artifacts"),
        target_root: home.clone(),
        cache_root: home,
        ..roots
    })
}

/// Load `config.json` from the host location, then apply `FRUST_HOME`. A missing file uses
/// defaults.
pub fn load_roots_from_host(
    os: HostOs,
    home: &Path,
    xdg_config_home: Option<&str>,
    xdg_cache_home: Option<&str>,
) -> Result<ResolvedRoots, ConfigError> {
    let path = absolute_config_path(os, home, xdg_config_home)?;
    let roots = match read_file(&path) {
        Ok(bytes) => resolve_roots(os, home, xdg_config_home, xdg_cache_home, Some(&bytes))?,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            resolve_roots(os, home, xdg_config_home, xdg_cache_home, None)?
        }
        Err(err) => return Err(error(&path, &format!("could not read file ({err})"))),
    };
    apply_frust_home(roots, std::env::var(HOME_ENV).ok().as_deref())
}

pub fn canonical_workspace(path: &Path) -> PathBuf {
    if let Ok(canonical) = fs::canonicalize(path) {
        return canonical;
    }
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

pub fn namespaced_target_dir(targets_dir: &Path, toolchain_id: &str, workspace: &Path) -> PathBuf {
    let canonical = canonical_workspace(workspace);
    let mut material = Vec::with_capacity(toolchain_id.len() + canonical.as_os_str().len() + 1);
    material.extend_from_slice(toolchain_id.as_bytes());
    material.push(0);
    material.extend_from_slice(canonical.as_os_str().as_encoded_bytes());
    let digest = sha256(&material);
    let hex = hex32(&digest);
    targets_dir.join(&hex[..2]).join(&hex[2..])
}

/// Never fails the build: any problem with the cache directory reports `Fallback`.
pub fn open_artifact_cache(dir: &Path) -> CacheOpen {
    open_artifact_cache_with(dir, &|| std::thread::sleep(MARKER_SETTLE))
}

fn open_artifact_cache_with(dir: &Path, settle: &dyn Fn()) -> CacheOpen {
    if dir.exists() && !dir.is_dir() {
        return CacheOpen::Fallback { reason: "cache path is not a directory" };
    }
    if fs::create_dir_all(dir).is_err() {
        return CacheOpen::Fallback { reason: "cache directory is unwritable" };
    }
    let marker = dir.join("MARKER");
    let mut state = read_marker(&marker);
    if state == MarkerState::Missing {
        match publish_marker(dir, &marker) {
            Ok(true) => return CacheOpen::Ready,
            // Another process published first, so judge its marker instead.
            Ok(false) => state = read_marker(&marker),
            Err(_) => return CacheOpen::Fallback { reason: "cache directory is unwritable" },
        }
    }
    if state == MarkerState::Unfinished {
        // Markers from older builds were created empty before being filled.
        settle();
        state = read_marker(&marker);
    }
    match state {
        MarkerState::Valid => CacheOpen::Ready,
        MarkerState::Unfinished | MarkerState::Corrupt => {
            CacheOpen::Fallback { reason: "cache marker is corrupt" }
        }
        MarkerState::Missing | MarkerState::Unreadable => {
            CacheOpen::Fallback { reason: "cache marker is unreadable" }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MarkerState {
    Valid,
    Missing,
    /// Empty or a strict prefix of the marker: a writer may still be filling it.
    Unfinished,
    Corrupt,
    Unreadable,
}

fn read_marker(path: &Path) -> MarkerState {
    match fs::read(path) {
        Ok(bytes) => classify_marker(&bytes),
        Err(err) if err.kind() == io::ErrorKind::NotFound => MarkerState::Missing,
        Err(_) => MarkerState::Unreadable,
    }
}

fn classify_marker(bytes: &[u8]) -> MarkerState {
    let line = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let expected = CACHE_MARKER.as_bytes();
    if line == expected {
        MarkerState::Valid
    } else if expected.starts_with(bytes) {
        MarkerState::Unfinished
    } else {
        MarkerState::Corrupt
    }
}

/// Publish the marker atomically so readers never see an empty or partial file.
/// Returns false when another process already published one.
fn publish_marker(dir: &Path, marker: &Path) -> io::Result<bool> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let temp = dir.join(format!(".MARKER.{}.{id}.{nanos}.tmp", std::process::id()));
    let result = write_temp_marker(&temp).and_then(|()| match fs::hard_link(&temp, marker) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        // Filesystems without hard links still get an atomic rename.
        Err(_) => fs::rename(&temp, marker).map(|()| true),
    });
    let _ = fs::remove_file(&temp);
    result
}

fn write_temp_marker(path: &Path) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(CACHE_MARKER.as_bytes())?;
    file.sync_all()
}

fn error(path: &Path, reason: &str) -> ConfigError {
    ConfigError { path: path.display().to_string(), reason: reason.to_string() }
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut state = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut chunk = [0u8; 64];
    let mut offset = 0;
    while offset + 64 <= data.len() {
        chunk.copy_from_slice(&data[offset..offset + 64]);
        compress(&mut state, &chunk, &K);
        offset += 64;
    }
    let rest = data.len() - offset;
    chunk[..rest].copy_from_slice(&data[offset..]);
    chunk[rest] = 0x80;
    if rest >= 56 {
        for byte in chunk.iter_mut().take(64).skip(rest + 1) {
            *byte = 0;
        }
        compress(&mut state, &chunk, &K);
        chunk = [0u8; 64];
    } else {
        for byte in chunk.iter_mut().take(56).skip(rest + 1) {
            *byte = 0;
        }
    }
    let bits = (data.len() as u64).wrapping_mul(8);
    chunk[56..].copy_from_slice(&bits.to_be_bytes());
    compress(&mut state, &chunk, &K);
    let mut out = [0u8; 32];
    for (index, word) in state.iter().enumerate() {
        out[index * 4..][..4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

fn compress(state: &mut [u32; 8], chunk: &[u8; 64], k: &[u32; 64]) {
    let mut w = [0u32; 64];
    for index in 0..16 {
        w[index] = u32::from_be_bytes(chunk[index * 4..][..4].try_into().unwrap());
    }
    for index in 16..64 {
        let s0 = w[index - 15].rotate_right(7) ^ w[index - 15].rotate_right(18) ^ (w[index - 15] >> 3);
        let s1 = w[index - 2].rotate_right(17) ^ w[index - 2].rotate_right(19) ^ (w[index - 2] >> 10);
        w[index] = w[index - 16]
            .wrapping_add(s0)
            .wrapping_add(w[index - 7])
            .wrapping_add(s1);
    }
    let mut a = state[0];
    let mut b = state[1];
    let mut c = state[2];
    let mut d = state[3];
    let mut e = state[4];
    let mut f = state[5];
    let mut g = state[6];
    let mut h = state[7];
    for index in 0..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ (!e & g);
        let temp1 = h.wrapping_add(s1).wrapping_add(ch).wrapping_add(k[index]).wrapping_add(w[index]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let temp2 = s0.wrapping_add(maj);
        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(temp1);
        d = c;
        c = b;
        b = a;
        a = temp1.wrapping_add(temp2);
    }
    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
    state[4] = state[4].wrapping_add(e);
    state[5] = state[5].wrapping_add(f);
    state[6] = state[6].wrapping_add(g);
    state[7] = state[7].wrapping_add(h);
}

pub fn hex32(bytes: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0xf) as usize] as char);
    }
    out
}

#[derive(Clone, Debug, PartialEq)]
enum Json {
    Number(String),
    String(String),
    Object(Vec<(String, Json)>),
    // Config validation never reads array items or literal values, only their kind.
    Array,
    Bool,
    Null,
}

impl Json {
    /// Name the kind of value for diagnostics, quoting numbers because they are short.
    fn describe(&self) -> String {
        match self {
            Json::Number(raw) if raw.len() > 40 => format!("number `{}...`", &raw[..40]),
            Json::Number(raw) => format!("number `{raw}`"),
            Json::String(_) => "string".to_string(),
            Json::Object(_) => "object".to_string(),
            Json::Array => "array".to_string(),
            Json::Bool => "boolean".to_string(),
            Json::Null => "null".to_string(),
        }
    }
}

/// Strict RFC 8259 parsing: no trailing commas, leading zeros, comments or bare words.
fn parse_json(input: &str) -> Result<Json, String> {
    let mut parser = Parser { bytes: input.as_bytes(), index: 0 };
    parser.skip();
    let parsed = parser.value(0).and_then(|value| {
        parser.skip();
        if parser.index == parser.bytes.len() {
            Ok(value)
        } else {
            Err("trailing data after the JSON value")
        }
    });
    parsed.map_err(|reason| format!("{reason} at byte {}", parser.index))
}

struct Parser<'a> {
    bytes: &'a [u8],
    index: usize,
}

impl<'a> Parser<'a> {
    fn skip(&mut self) {
        while matches!(self.bytes.get(self.index), Some(b' ' | b'\n' | b'\r' | b'\t')) {
            self.index += 1;
        }
    }

    /// `depth` is the number of containers already open around this value.
    fn value(&mut self, depth: usize) -> Result<Json, &'static str> {
        self.skip();
        match self.bytes.get(self.index).copied() {
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => Ok(Json::String(self.string()?)),
            Some(b'-' | b'0'..=b'9') => Ok(Json::Number(self.number()?)),
            Some(b't') => self.literal(b"true", Json::Bool),
            Some(b'f') => self.literal(b"false", Json::Bool),
            Some(b'n') => self.literal(b"null", Json::Null),
            Some(_) => Err("invalid JSON value"),
            None => Err("unexpected end of JSON"),
        }
    }

    fn object(&mut self, depth: usize) -> Result<Json, &'static str> {
        let depth = open_container(depth)?;
        self.index += 1;
        let mut pairs: Vec<(String, Json)> = Vec::new();
        self.skip();
        if self.bytes.get(self.index) == Some(&b'}') {
            self.index += 1;
            return Ok(Json::Object(pairs));
        }
        loop {
            self.skip();
            match self.bytes.get(self.index) {
                Some(b'"') => {}
                Some(b'}') => return Err("trailing comma in object"),
                Some(_) => return Err("object field name must be a string"),
                None => return Err("unexpected end of JSON"),
            }
            let key = self.string()?;
            if pairs.iter().any(|(existing, _)| existing == &key) {
                return Err("duplicate object field");
            }
            self.skip();
            self.expect(b':', "expected ':' after an object field")?;
            let item = self.value(depth)?;
            pairs.push((key, item));
            self.skip();
            match self.bytes.get(self.index) {
                Some(b',') => self.index += 1,
                Some(b'}') => {
                    self.index += 1;
                    return Ok(Json::Object(pairs));
                }
                Some(_) => return Err("expected a comma between object fields"),
                None => return Err("unexpected end of JSON"),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<Json, &'static str> {
        let depth = open_container(depth)?;
        self.index += 1;
        self.skip();
        if self.bytes.get(self.index) == Some(&b']') {
            self.index += 1;
            return Ok(Json::Array);
        }
        loop {
            self.value(depth)?;
            self.skip();
            match self.bytes.get(self.index) {
                Some(b',') => self.index += 1,
                Some(b']') => {
                    self.index += 1;
                    return Ok(Json::Array);
                }
                Some(_) => return Err("expected a comma between array items"),
                None => return Err("unexpected end of JSON"),
            }
            self.skip();
            if self.bytes.get(self.index) == Some(&b']') {
                return Err("trailing comma in array");
            }
        }
    }

    fn expect(&mut self, byte: u8, message: &'static str) -> Result<(), &'static str> {
        match self.bytes.get(self.index) {
            Some(&found) if found == byte => {
                self.index += 1;
                Ok(())
            }
            Some(_) => Err(message),
            None => Err("unexpected end of JSON"),
        }
    }

    fn literal(&mut self, word: &[u8], value: Json) -> Result<Json, &'static str> {
        if !self.bytes[self.index..].starts_with(word) {
            return Err("invalid JSON value");
        }
        self.index += word.len();
        Ok(value)
    }

    fn string(&mut self) -> Result<String, &'static str> {
        self.index += 1;
        let (text, consumed) = crate::json_text::decode_json_string_body(&self.bytes[self.index..])?;
        self.index += consumed;
        Ok(text)
    }

    /// Accepts exactly `-? (0 | [1-9][0-9]*) (. [0-9]+)? ([eE] [+-]? [0-9]+)?`.
    /// Which fields may hold a number is the config layer's decision, not the parser's.
    fn number(&mut self) -> Result<String, &'static str> {
        let start = self.index;
        if self.bytes.get(self.index) == Some(&b'-') {
            self.index += 1;
        }
        match self.bytes.get(self.index) {
            Some(b'0') => {
                self.index += 1;
                if matches!(self.bytes.get(self.index), Some(b'0'..=b'9')) {
                    return Err("invalid number: leading zeros are not allowed");
                }
            }
            Some(b'1'..=b'9') => {
                self.digits();
            }
            _ => return Err("invalid number"),
        }
        if self.bytes.get(self.index) == Some(&b'.') {
            self.index += 1;
            if !self.digits() {
                return Err("invalid number: digits are required after '.'");
            }
        }
        if matches!(self.bytes.get(self.index), Some(b'e' | b'E')) {
            self.index += 1;
            if matches!(self.bytes.get(self.index), Some(b'+' | b'-')) {
                self.index += 1;
            }
            if !self.digits() {
                return Err("invalid number: digits are required in the exponent");
            }
        }
        std::str::from_utf8(&self.bytes[start..self.index])
            .map(str::to_owned)
            .map_err(|_| "invalid number")
    }

    /// Consume ASCII digits and report whether there was at least one.
    fn digits(&mut self) -> bool {
        let start = self.index;
        while matches!(self.bytes.get(self.index), Some(b'0'..=b'9')) {
            self.index += 1;
        }
        self.index > start
    }
}

/// Returns the depth inside a container opened at `depth`, or an error past the cap.
fn open_container(depth: usize) -> Result<usize, &'static str> {
    if depth >= MAX_JSON_DEPTH {
        return Err("JSON is nested deeper than 64 levels");
    }
    Ok(depth + 1)
}

/// Read a file's bytes for tests and the Cargo patch without buffering the whole disk on error.
pub fn read_file(path: &Path) -> io::Result<Vec<u8>> {
    let mut file = File::open(path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::sync::Barrier;
    use std::sync::atomic::AtomicUsize;

    fn scratch() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("frust-paths-{pid}-{nanos}-{n}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    /// Fixed-seed xorshift64 so generated cases are identical on every run.
    struct Rng(u64);

    impl Rng {
        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn below(&mut self, bound: usize) -> usize {
            (self.next_u64() % bound as u64) as usize
        }
    }

    fn config_path_of(root: &Path) -> PathBuf {
        let path = root.join(".config").join("frust").join("config.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        path
    }

    fn set_mode(path: &Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    /// Permission tests are meaningless when the user bypasses mode bits.
    fn running_as_root(probe_dir: &Path) -> bool {
        set_mode(probe_dir, 0o555);
        let writable = fs::write(probe_dir.join("probe"), b"x").is_ok();
        set_mode(probe_dir, 0o755);
        let _ = fs::remove_file(probe_dir.join("probe"));
        writable
    }

    #[test]
    fn sha256_matches_known_digests() {
        assert_eq!(
            hex32(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex32(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn linux_and_macos_defaults_and_explicit_roots() {
        let home = Path::new("/home/builder");
        let absent = resolve_roots(HostOs::Linux, home, None, None, None).unwrap();
        assert_eq!(absent.config_path, PathBuf::from("/home/builder/.config/frust/config.json"));
        assert_eq!(absent.target_root, PathBuf::from("/home/builder/.cache/frust"));
        assert_eq!(absent.targets_dir, PathBuf::from("/home/builder/.cache/frust/targets"));
        assert_eq!(absent.artifacts_dir, PathBuf::from("/home/builder/.cache/frust/artifacts"));

        let xdg = resolve_roots(
            HostOs::Linux,
            home,
            Some("/xdg/config"),
            Some("/xdg/cache"),
            Some(br#"{"version":1,"cache_root":"/opt/frust-cache"}"#),
        )
        .unwrap();
        assert_eq!(xdg.config_path, PathBuf::from("/xdg/config/frust/config.json"));
        assert_eq!(xdg.target_root, PathBuf::from("/xdg/cache/frust"));
        assert_eq!(xdg.cache_root, PathBuf::from("/opt/frust-cache"));
        assert_eq!(xdg.artifacts_dir, PathBuf::from("/opt/frust-cache/artifacts"));

        let relative = resolve_roots(HostOs::Linux, home, Some("relative"), Some(""), None).unwrap();
        assert_eq!(relative.config_path, absent.config_path);
        assert_eq!(relative.cache_root, absent.cache_root);

        let mac = resolve_roots(HostOs::Macos, Path::new("/Users/builder"), Some("/xdg"), None, None).unwrap();
        assert_eq!(
            mac.config_path,
            PathBuf::from("/Users/builder/Library/Application Support/fRust/config.json")
        );
        assert_eq!(mac.cache_root, PathBuf::from("/Users/builder/Library/Caches/fRust"));
    }

    #[test]
    fn malformed_config_names_the_problem() {
        let path = Path::new("/home/builder/.config/frust/config.json");
        let cases = [
            (br#"[]"#.as_slice(), "top-level"),
            (br#"{"version":2}"#, "integer 1"),
            (br#"{"version":1,"target_root":"relative"}"#, "absolute"),
            (br#"{"version":1,"extra":true}"#, "unknown field"),
            (br#"{"version":1"#, "end of JSON"),
            (br#"not json"#, "invalid JSON"),
        ];
        for (bytes, needle) in cases {
            let err = parse_user_config(path, bytes).unwrap_err();
            let text = err.diagnostic();
            assert!(text.contains(needle), "{text}");
            assert!(text.contains(&path.display().to_string()), "{text}");
        }
    }

    #[test]
    fn unicode_escapes_round_trip_into_absolute_roots() {
        let path = Path::new("/home/builder/.config/frust/config.json");
        let bytes = br#"{"version":1,"target_root":"/tmp/caf\u00e9","cache_root":"/tmp/\uD83D\uDE00"}"#;
        let config = parse_user_config(path, bytes).unwrap();
        assert_eq!(config.target_root.unwrap(), PathBuf::from("/tmp/caf\u{e9}"));
        assert_eq!(config.cache_root.unwrap(), PathBuf::from("/tmp/\u{1F600}"));
    }

    #[test]
    fn namespaces_follow_toolchain_and_canonical_workspace() {
        let root = scratch();
        let targets = root.join("targets");
        let workspace = root.join("app");
        fs::create_dir_all(&workspace).unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&workspace, &link).unwrap();
        let direct = namespaced_target_dir(&targets, "toolchain-a", &workspace);
        let via_link = namespaced_target_dir(&targets, "toolchain-a", &link);
        let other_tool = namespaced_target_dir(&targets, "toolchain-b", &workspace);
        let other_tree = root.join("other");
        fs::create_dir_all(&other_tree).unwrap();
        let other = namespaced_target_dir(&targets, "toolchain-a", &other_tree);
        assert_eq!(direct, via_link);
        assert_ne!(direct, other_tool);
        assert_ne!(direct, other);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn corrupt_or_unwritable_cache_falls_back() {
        let root = scratch();
        let ready = root.join("ready");
        assert_eq!(open_artifact_cache(&ready), CacheOpen::Ready);
        assert_eq!(open_artifact_cache(&ready), CacheOpen::Ready);
        fs::write(ready.join("MARKER"), "frust-cache-v0").unwrap();
        assert_eq!(
            open_artifact_cache(&ready),
            CacheOpen::Fallback { reason: "cache marker is corrupt" }
        );
        let blocked = root.join("blocked");
        fs::write(&blocked, b"file").unwrap();
        assert_eq!(
            open_artifact_cache(&blocked),
            CacheOpen::Fallback { reason: "cache path is not a directory" }
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn frust_home_replaces_both_roots_and_keeps_other_settings() {
        let home = Path::new("/home/builder");
        let config = br#"{"version":1,"target_root":"/opt/t","cache_root":"/opt/c","cache_max_bytes":2147483648}"#;
        let roots = resolve_roots(HostOs::Linux, home, None, None, Some(config)).unwrap();
        let moved = apply_frust_home(roots, Some("/Volumes/Secure/Frust")).unwrap();
        assert_eq!(moved.target_root, PathBuf::from("/Volumes/Secure/Frust"));
        assert_eq!(moved.cache_root, PathBuf::from("/Volumes/Secure/Frust"));
        assert_eq!(moved.targets_dir, PathBuf::from("/Volumes/Secure/Frust/targets"));
        assert_eq!(moved.artifacts_dir, PathBuf::from("/Volumes/Secure/Frust/artifacts"));
        assert_eq!(moved.cache_max_bytes, Some(2147483648));
    }

    #[test]
    fn unset_or_empty_frust_home_changes_nothing() {
        let home = Path::new("/home/builder");
        let roots = resolve_roots(HostOs::Linux, home, None, None, None).unwrap();
        for value in [None, Some("")] {
            let same = apply_frust_home(roots.clone(), value).unwrap();
            assert_eq!(same.targets_dir, roots.targets_dir);
            assert_eq!(same.artifacts_dir, roots.artifacts_dir);
        }
    }

    #[test]
    fn relative_frust_home_is_rejected_with_its_name() {
        let home = Path::new("/home/builder");
        let roots = resolve_roots(HostOs::Linux, home, None, None, None).unwrap();
        let text = apply_frust_home(roots, Some("relative/dir")).unwrap_err().diagnostic();
        assert!(text.contains("FRUST_HOME must be an absolute path"), "{text}");
    }

    const VALID_DOC: &[u8] = br#"{"version":1,"target_root":"/tmp/a b","cache_root":"/tmp/caf\u00e9"}"#;

    fn config_file() -> &'static Path {
        Path::new("/home/builder/.config/frust/config.json")
    }

    fn rejection(bytes: &[u8]) -> String {
        parse_user_config(config_file(), bytes).unwrap_err().diagnostic()
    }

    fn nested_arrays(depth: usize) -> String {
        format!("{}{}", "[".repeat(depth), "]".repeat(depth))
    }

    fn nested_objects(depth: usize) -> String {
        format!("{}1{}", "{\"a\":".repeat(depth), "}".repeat(depth))
    }

    #[test]
    fn sha256_handles_the_padding_boundary() {
        let two_blocks = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
        assert_eq!(
            hex32(&sha256(two_blocks)),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn number_errors_name_the_offending_field() {
        let text = rejection(br#"{"version":1,"target_root":1.5}"#);
        assert!(text.contains("`target_root` must be a string, found number `1.5`"), "{text}");
        assert!(!text.contains("`version` must be an integer"), "{text}");
        let cases = [
            (br#"{"version":1.0}"#.as_slice(), "`version` must be the integer 1, found number `1.0`"),
            (br#"{"version":-1}"#, "found number `-1`"),
            (br#"{"version":1e0}"#, "found number `1e0`"),
            (br#"{"version":"1"}"#, "found string"),
            (br#"{"version":true}"#, "found boolean"),
            (br#"{"version":null}"#, "found null"),
            (br#"{"version":[1]}"#, "found array"),
            (br#"{"version":{}}"#, "found object"),
            (br#"{"version":1,"cache_root":null}"#, "`cache_root` must be a string, found null"),
            (br#"{"version":1,"cache_root":7}"#, "`cache_root` must be a string, found number `7`"),
        ];
        for (doc, needle) in cases {
            let text = rejection(doc);
            assert!(text.contains(needle), "{text}");
        }
        let long = format!(r#"{{"version":{}}}"#, "1".repeat(50));
        let text = rejection(long.as_bytes());
        assert!(text.contains(&format!("found number `{}...`", "1".repeat(40))), "{text}");
    }

    #[test]
    fn roots_reject_nul_and_empty_paths() {
        let text = rejection(br#"{"version":1,"target_root":"/tmp/a\u0000b"}"#);
        assert!(text.contains("`target_root` must not contain a NUL"), "{text}");
        let text = rejection(br#"{"version":1,"cache_root":""}"#);
        assert!(text.contains("`cache_root` must be an absolute path"), "{text}");
        let text = rejection(br#"{"cache_root":"/tmp/x"}"#);
        assert!(text.contains("missing `version`"), "{text}");
        let text = rejection(br#"{"version":1,"version":1}"#);
        assert!(text.contains("duplicate object field"), "{text}");
    }

    #[test]
    fn parser_accepts_only_strict_json() {
        let good = [
            "0",
            "-0",
            "0.5",
            "-1.5e+10",
            "1E5",
            "1e-2",
            "[]",
            "[ ]",
            "{}",
            "{ }",
            "\"s\"",
            "[1,[2,{\"a\":[]}],\"x\",true,false,null]",
            " \t\r\n{\"a\" : \"\\u00e9\" ,\"b\":[ 1 , 2 ]}\n",
        ];
        for text in good {
            assert!(parse_json(text).is_ok(), "{text:?}");
        }
        let bad = [
            "", "[", "{", "[1,]", "[,]", "[,1]", "[1 2]", "{\"a\":1,}", "{,}", "{\"a\" 1}",
            "{\"a\":}", "{\"a\"", "{\"a\":", "{a:1}", "{1:1}", "{\"a\":1 \"b\":2}", "01", "-01",
            "00", "1.", ".5", "-.5", "1e", "1e+", "1.e5", "-", "+1", "1.5.2", "0x10", "tru",
            "truee", "True", "nul", "NaN", "Infinity", "'a'", "\"a\" \"b\"", "{\"a\":1}x",
            "// c\n{}", "{\"a\":1} // c", "/* c */ {}", "\"a\nb\"", "\"\\x\"", "\u{b}1", "\u{c}1",
            "\u{a0}1", "\u{feff}{}",
        ];
        for text in bad {
            assert!(parse_json(text).is_err(), "{text:?}");
        }
    }

    #[test]
    fn syntax_errors_say_what_and_where() {
        assert_eq!(parse_json("[1,]").unwrap_err(), "trailing comma in array at byte 3");
        let text = rejection(br#"{"version":1,}"#);
        assert!(text.contains("trailing comma in object"), "{text}");
        let text = rejection(br#"{"version":01}"#);
        assert!(text.contains("leading zeros"), "{text}");
        let text = rejection(br#"{"version":1"#);
        assert!(text.contains("unexpected end of JSON at byte 12"), "{text}");
        let text = rejection(b"");
        assert!(text.contains("unexpected end of JSON at byte 0"), "{text}");
        let text = rejection(br#"1"#);
        assert!(text.contains("top-level value must be an object"), "{text}");
    }

    #[test]
    fn nesting_is_capped_instead_of_overflowing_the_stack() {
        assert!(parse_json(&nested_arrays(MAX_JSON_DEPTH)).is_ok());
        assert!(parse_json(&nested_objects(MAX_JSON_DEPTH)).is_ok());
        for text in [nested_arrays(MAX_JSON_DEPTH + 1), nested_objects(MAX_JSON_DEPTH + 1)] {
            let reason = parse_json(&text).unwrap_err();
            assert!(reason.contains("nested deeper than 64 levels"), "{reason}");
        }
        let mixed = format!("{}1{}", "[{\"a\":".repeat(40), "}]".repeat(40));
        assert!(parse_json(&mixed).unwrap_err().contains("nested deeper"));
        // The config object is level one, so the field value may nest 63 more.
        let fits = format!(r#"{{"version":1,"cache_root":{}}}"#, nested_arrays(MAX_JSON_DEPTH - 1));
        assert!(rejection(fits.as_bytes()).contains("found array"));
        let over = format!(r#"{{"version":1,"cache_root":{}}}"#, nested_arrays(MAX_JSON_DEPTH));
        assert!(rejection(over.as_bytes()).contains("nested deeper"));
        let huge = format!(r#"{{"version":1,"cache_root":{}1}}"#, "[".repeat(200_000));
        assert!(rejection(huge.as_bytes()).contains("nested deeper"));
    }

    #[test]
    fn every_strict_prefix_of_a_valid_document_is_rejected() {
        let config = parse_user_config(config_file(), VALID_DOC).unwrap();
        assert_eq!(config.target_root, Some(PathBuf::from("/tmp/a b")));
        assert_eq!(config.cache_root, Some(PathBuf::from("/tmp/caf\u{e9}")));
        for end in 0..VALID_DOC.len() {
            assert!(parse_user_config(config_file(), &VALID_DOC[..end]).is_err(), "prefix {end}");
        }
    }

    #[test]
    fn parser_survives_mutated_documents_and_byte_soup() {
        let mut rng = Rng(0x2545_F491_4F6C_DD1D);
        let insertions = b"{}[],:\"\\-+.eE0123456789tfnul \t\n\x00\xff";
        let mut accepted = 0;
        for round in 0..4000 {
            let bytes: Vec<u8> = if round % 4 == 0 {
                (0..rng.below(64)).map(|_| rng.next_u64() as u8).collect()
            } else {
                let mut bytes = VALID_DOC.to_vec();
                for _ in 0..=rng.below(4) {
                    match rng.below(4) {
                        0 if !bytes.is_empty() => {
                            let at = rng.below(bytes.len());
                            bytes[at] = rng.next_u64() as u8;
                        }
                        1 if !bytes.is_empty() => {
                            bytes.remove(rng.below(bytes.len()));
                        }
                        2 => {
                            let at = rng.below(bytes.len() + 1);
                            bytes.insert(at, insertions[rng.below(insertions.len())]);
                        }
                        _ => bytes.truncate(rng.below(bytes.len() + 1)),
                    }
                }
                bytes
            };
            if let Ok(config) = parse_user_config(config_file(), &bytes) {
                accepted += 1;
                for root in [&config.target_root, &config.cache_root].into_iter().flatten() {
                    assert!(root.is_absolute(), "{root:?} from {bytes:?}");
                }
            }
        }
        assert!(accepted > 0, "mutations never produced a valid document");

        let tokens = [
            "{", "}", "[", "]", ",", ":", "\"", "\\", "\\u", "version", "\"version\"", "1", "0",
            "-", ".", "e", "true", "null", " ", "\u{e9}",
        ];
        for _ in 0..4000 {
            let doc: String = (0..rng.below(16)).map(|_| tokens[rng.below(tokens.len())]).collect();
            let _ = parse_json(&doc);
        }
    }

    #[test]
    fn escaped_generated_paths_round_trip_through_the_config() {
        let mut rng = Rng(0xC0FF_EE12_3456_789B);
        let pool = ['a', 'Z', '0', ' ', '/', '"', '\\', '\u{1}', '\u{1f}', '\n', '\u{e9}', '\u{2603}', '\u{1F600}'];
        for _ in 0..1000 {
            let tail: String = (0..rng.below(12)).map(|_| pool[rng.below(pool.len())]).collect();
            let path = format!("/{tail}");
            let escaped = crate::json_text::escape_json_string(&path);
            let doc = format!(r#"{{"version":1,"target_root":"{escaped}"}}"#);
            let config = parse_user_config(config_file(), doc.as_bytes()).unwrap();
            assert_eq!(config.target_root, Some(PathBuf::from(&path)), "{doc}");
        }
    }

    #[test]
    fn resolved_roots_are_absolute_for_generated_environments() {
        let mut rng = Rng(0xA5A5_1234_9876_5432);
        let homes = ["/home/u", "/home/with space", "/", "/home/u/", "/home//double"];
        let pool = ["", "relative", "./x", "../x", " ", "/", "/abs/with space", "/abs", "~", "$HOME"];
        let alphabet = ['/', ' ', 'a', '.', 'b', '~', '\\', '\u{e9}'];
        let random_xdg = |rng: &mut Rng| -> Option<String> {
            match rng.below(4) {
                0 => None,
                1 => Some(pool[rng.below(pool.len())].to_string()),
                _ => Some((0..rng.below(8)).map(|_| alphabet[rng.below(alphabet.len())]).collect()),
            }
        };
        for _ in 0..500 {
            let home = Path::new(homes[rng.below(homes.len())]);
            let config_xdg = random_xdg(&mut rng);
            let cache_xdg = random_xdg(&mut rng);
            for os in [HostOs::Linux, HostOs::Macos] {
                let roots =
                    resolve_roots(os, home, config_xdg.as_deref(), cache_xdg.as_deref(), None).unwrap();
                for path in [
                    &roots.config_path,
                    &roots.target_root,
                    &roots.cache_root,
                    &roots.targets_dir,
                    &roots.artifacts_dir,
                ] {
                    assert!(path.is_absolute(), "{path:?} for {home:?} {config_xdg:?} {cache_xdg:?}");
                }
                if os == HostOs::Linux {
                    let absolute = |value: &Option<String>| {
                        value.as_deref().filter(|raw| Path::new(raw).is_absolute()).map(PathBuf::from)
                    };
                    let want_cache = absolute(&cache_xdg).unwrap_or_else(|| home.join(".cache"));
                    let want_config = absolute(&config_xdg).unwrap_or_else(|| home.join(".config"));
                    assert_eq!(roots.cache_root, want_cache.join("frust"));
                    assert_eq!(roots.config_path, want_config.join("frust").join("config.json"));
                }
            }
        }
    }

    #[test]
    fn relative_home_is_rejected_unless_every_path_is_otherwise_absolute() {
        for home in ["", "rel", ".", "../up"] {
            let home = Path::new(home);
            for os in [HostOs::Linux, HostOs::Macos] {
                let err = resolve_roots(os, home, None, None, None).unwrap_err();
                assert!(err.diagnostic().contains("not absolute"), "{}", err.diagnostic());
                let err = load_roots_from_host(os, home, None, None).unwrap_err();
                assert!(err.diagnostic().contains("not absolute"), "{}", err.diagnostic());
            }
            let covered = resolve_roots(HostOs::Linux, home, Some("/c"), Some("/k"), None).unwrap();
            assert_eq!(covered.cache_root, PathBuf::from("/k/frust"));
            let doc = br#"{"version":1,"target_root":"/t","cache_root":"/k"}"#;
            let explicit = resolve_roots(HostOs::Linux, home, Some("/c"), None, Some(doc)).unwrap();
            assert_eq!(explicit.target_root, PathBuf::from("/t"));
        }
    }

    #[test]
    fn symlinked_and_spaced_xdg_roots_are_kept_verbatim() {
        let root = scratch();
        let real = root.join("real cache");
        fs::create_dir_all(&real).unwrap();
        let link = root.join("link cache");
        symlink(&real, &link).unwrap();
        let link_text = link.to_str().unwrap();
        let roots = resolve_roots(HostOs::Linux, &root, None, Some(link_text), None).unwrap();
        assert_eq!(roots.target_root, link.join("frust"));
        assert_eq!(roots.artifacts_dir, link.join("frust").join("artifacts"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn host_loading_reads_defaults_files_and_reports_unreadable_config() {
        let root = scratch();
        let home = root.join("home with space");
        fs::create_dir_all(&home).unwrap();

        let missing = load_roots_from_host(HostOs::Linux, &home, None, None).unwrap();
        assert_eq!(missing.target_root, home.join(".cache").join("frust"));

        let config = config_path_of(&home);
        fs::create_dir(&config).unwrap();
        let err = load_roots_from_host(HostOs::Linux, &home, None, None).unwrap_err();
        assert!(err.reason.contains("could not read file"), "{}", err.diagnostic());
        assert!(err.diagnostic().contains(&config.display().to_string()));
        fs::remove_dir(&config).unwrap();

        fs::write(&config, b"{\"version\":1,\"target_root\":\"/t\xff\"}").unwrap();
        let err = load_roots_from_host(HostOs::Linux, &home, None, None).unwrap_err();
        assert!(err.reason.contains("not UTF-8"), "{}", err.diagnostic());

        fs::write(&config, b"").unwrap();
        let err = load_roots_from_host(HostOs::Linux, &home, None, None).unwrap_err();
        assert!(err.reason.contains("unexpected end of JSON"), "{}", err.diagnostic());

        fs::write(&config, br#"{"version":1,"target_root":"/srv/t","cache_root":"/srv/c"}"#).unwrap();
        let loaded = load_roots_from_host(HostOs::Linux, &home, None, None).unwrap();
        assert_eq!(loaded.targets_dir, PathBuf::from("/srv/t/targets"));
        assert_eq!(loaded.artifacts_dir, PathBuf::from("/srv/c/artifacts"));

        let link = root.join("home link");
        symlink(&home, &link).unwrap();
        let via_link = load_roots_from_host(HostOs::Linux, &link, None, None).unwrap();
        assert_eq!(via_link.config_path, link.join(".config").join("frust").join("config.json"));
        assert_eq!(via_link.cache_root, PathBuf::from("/srv/c"));

        let xdg = root.join("xdg");
        let xdg_config = xdg.join("frust").join("config.json");
        fs::create_dir_all(xdg_config.parent().unwrap()).unwrap();
        fs::write(&xdg_config, br#"{"version":1}"#).unwrap();
        let xdg_text = xdg.to_str().unwrap();
        let from_xdg = load_roots_from_host(HostOs::Linux, &home, Some(xdg_text), None).unwrap();
        assert_eq!(from_xdg.config_path, xdg_config);
        assert_eq!(from_xdg.target_root, home.join(".cache").join("frust"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn unreadable_config_file_is_reported() {
        let root = scratch();
        if running_as_root(&root) {
            let _ = fs::remove_dir_all(&root);
            return;
        }
        let config = config_path_of(&root);
        fs::write(&config, br#"{"version":1}"#).unwrap();
        set_mode(&config, 0o000);
        let err = load_roots_from_host(HostOs::Linux, &root, None, None).unwrap_err();
        assert!(err.reason.contains("could not read file"), "{}", err.diagnostic());
        set_mode(&config, 0o644);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn canonical_workspace_resolves_dotdot_symlinks_and_spaces() {
        let root = scratch();
        let canonical_root = fs::canonicalize(&root).unwrap();
        let project = root.join("my project");
        fs::create_dir_all(project.join("sub dir")).unwrap();
        let link = root.join("link to project");
        symlink(&project, &link).unwrap();
        let expected = canonical_root.join("my project");
        assert_eq!(canonical_workspace(&link), expected);
        assert_eq!(canonical_workspace(&project.join("sub dir").join("..")), expected);

        let cwd = std::env::current_dir().unwrap();
        assert_eq!(canonical_workspace(Path::new(".")), fs::canonicalize(&cwd).unwrap());
        // Paths that cannot be canonicalized still become absolute and are otherwise untouched.
        let missing = Path::new("frust-no-such-workspace/child");
        assert_eq!(canonical_workspace(missing), cwd.join(missing));
        let absent = root.join("absent dir");
        assert_eq!(canonical_workspace(&absent), absent);
        let dangling = root.join("dangling");
        symlink(root.join("nowhere"), &dangling).unwrap();
        assert_eq!(canonical_workspace(&dangling), dangling);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn namespaced_dirs_are_stable_distinct_and_shaped() {
        let root = scratch();
        let targets = root.join("targets");
        let mut seen = std::collections::BTreeSet::new();
        for index in 0..50 {
            let workspace = root.join(format!("worktree {index}"));
            fs::create_dir_all(&workspace).unwrap();
            let first = namespaced_target_dir(&targets, "toolchain", &workspace);
            assert_eq!(first, namespaced_target_dir(&targets, "toolchain", &workspace));
            let parts: Vec<_> = first.strip_prefix(&targets).unwrap().iter().collect();
            assert_eq!(parts.len(), 2);
            assert_eq!(parts[0].len(), 2);
            assert_eq!(parts[1].len(), 62);
            let hex = first.strip_prefix(&targets).unwrap().to_str().unwrap().replace('/', "");
            assert!(hex.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)));
            assert!(seen.insert(first), "worktree {index} collided");
        }
        let workspace = root.join("worktree 0");
        let mut by_toolchain = std::collections::BTreeSet::new();
        for id in ["a", "ab", "a\0b", "", "a b", "a/b"] {
            assert!(by_toolchain.insert(namespaced_target_dir(&targets, id, &workspace)), "{id:?}");
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn marker_with_trailing_newline_is_accepted_and_left_alone() {
        let root = scratch();
        let marker = root.join("MARKER");
        let cases = [
            (b"frust-cache-v1\n".as_slice(), true),
            (b"frust-cache-v1\r\n", true),
            (b"frust-cache-v1", true),
            (b"frust-cache-v1\n\n", false),
            (b"frust-cache-v1 ", false),
            (b" frust-cache-v1", false),
            (b"frust-cache-v10", false),
            (b"FRUST-CACHE-V1", false),
            (b"frust-cache-v1\0", false),
            (b"\xff\xfe\x00", false),
        ];
        for (bytes, valid) in cases {
            fs::write(&marker, bytes).unwrap();
            let settles = AtomicUsize::new(0);
            let outcome = open_artifact_cache_with(&root, &|| {
                settles.fetch_add(1, Ordering::Relaxed);
            });
            let expected = if valid {
                CacheOpen::Ready
            } else {
                CacheOpen::Fallback { reason: "cache marker is corrupt" }
            };
            assert_eq!(outcome, expected, "{bytes:?}");
            // Definite verdicts never wait for a writer, and the file is never rewritten.
            assert_eq!(settles.load(Ordering::Relaxed), 0, "{bytes:?}");
            assert_eq!(fs::read(&marker).unwrap(), bytes);
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn partial_markers_get_one_settle_retry_before_counting_as_corrupt() {
        let root = scratch();
        let marker = root.join("MARKER");
        for partial in ["", "f", "frust-cache", "frust-cache-v"] {
            fs::write(&marker, partial).unwrap();
            let settles = AtomicUsize::new(0);
            let outcome = open_artifact_cache_with(&root, &|| {
                settles.fetch_add(1, Ordering::Relaxed);
            });
            assert_eq!(outcome, CacheOpen::Fallback { reason: "cache marker is corrupt" });
            assert_eq!(settles.load(Ordering::Relaxed), 1, "{partial:?}");
        }
        // A writer that finishes during the wait is not reported as corruption.
        fs::write(&marker, "").unwrap();
        let outcome = open_artifact_cache_with(&root, &|| fs::write(&marker, CACHE_MARKER).unwrap());
        assert_eq!(outcome, CacheOpen::Ready);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn marker_that_is_a_directory_or_unreadable_falls_back() {
        let root = scratch();
        fs::create_dir(root.join("MARKER")).unwrap();
        assert_eq!(
            open_artifact_cache(&root),
            CacheOpen::Fallback { reason: "cache marker is unreadable" }
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn crashed_writer_leftovers_do_not_block_the_cache() {
        let root = scratch();
        let dir = root.join("cache");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(".MARKER.1.2.3.tmp"), b"").unwrap();
        assert_eq!(open_artifact_cache(&dir), CacheOpen::Ready);
        assert_eq!(open_artifact_cache(&dir), CacheOpen::Ready);
        assert_eq!(fs::read(dir.join("MARKER")).unwrap(), CACHE_MARKER.as_bytes());
        let mut names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, [".MARKER.1.2.3.tmp", "MARKER"]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn concurrent_openers_all_see_one_valid_marker() {
        const THREADS: usize = 16;
        let root = scratch();
        for round in 0..20 {
            let dir = root.join(format!("cache {round}"));
            let barrier = Barrier::new(THREADS);
            let settles = AtomicUsize::new(0);
            let outcomes: Vec<CacheOpen> = std::thread::scope(|scope| {
                let mut handles = Vec::new();
                for _ in 0..THREADS {
                    handles.push(scope.spawn(|| {
                        barrier.wait();
                        open_artifact_cache_with(&dir, &|| {
                            settles.fetch_add(1, Ordering::Relaxed);
                        })
                    }));
                }
                handles.into_iter().map(|handle| handle.join().unwrap()).collect()
            });
            assert!(outcomes.iter().all(|outcome| *outcome == CacheOpen::Ready), "{outcomes:?}");
            // Atomic publication means no reader ever observes an unfinished marker.
            assert_eq!(settles.load(Ordering::Relaxed), 0);
            assert_eq!(fs::read(dir.join("MARKER")).unwrap(), CACHE_MARKER.as_bytes());
            let names: Vec<_> = fs::read_dir(&dir)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect();
            assert_eq!(names, ["MARKER"]);
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn unwritable_cache_directory_falls_back_without_side_effects() {
        let root = scratch();
        let locked = root.join("locked");
        fs::create_dir_all(&locked).unwrap();
        if running_as_root(&locked) {
            let _ = fs::remove_dir_all(&root);
            return;
        }
        set_mode(&locked, 0o555);
        let unwritable = CacheOpen::Fallback { reason: "cache directory is unwritable" };
        assert_eq!(open_artifact_cache(&locked), unwritable);
        assert_eq!(open_artifact_cache(&locked.join("child")), unwritable);
        assert!(fs::read_dir(&locked).unwrap().next().is_none());
        set_mode(&locked, 0o755);
        assert_eq!(open_artifact_cache(&locked), CacheOpen::Ready);

        let marker = locked.join("MARKER");
        set_mode(&marker, 0o000);
        assert_eq!(
            open_artifact_cache(&locked),
            CacheOpen::Fallback { reason: "cache marker is unreadable" }
        );
        set_mode(&marker, 0o644);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn cache_max_bytes_accepts_the_documented_range() {
        let parse = |doc: &str| parse_user_config(config_file(), doc.as_bytes()).unwrap().cache_max_bytes;
        assert_eq!(parse(r#"{"version":1}"#), None);
        assert_eq!(parse(r#"{"version":1,"cache_max_bytes":1073741824}"#), Some(1 << 30));
        assert_eq!(parse(r#"{"version":1,"cache_max_bytes":9007199254740992}"#), Some(1 << 53));
        assert_eq!(parse(r#"{"cache_max_bytes":21474836480,"version":1}"#), Some(20 << 30));
        let roots = resolve_roots(
            HostOs::Linux,
            Path::new("/home/builder"),
            None,
            None,
            Some(br#"{"version":1,"cache_max_bytes":5368709120}"#),
        )
        .unwrap();
        assert_eq!(roots.cache_max_bytes, Some(5 << 30));
        let unset = resolve_roots(HostOs::Linux, Path::new("/home/builder"), None, None, None).unwrap();
        assert_eq!(unset.cache_max_bytes, None);
    }

    #[test]
    fn cache_max_bytes_rejections_say_how_to_fix_the_value() {
        let cases = [
            (r#"{"version":1,"cache_max_bytes":1073741823}"#, "found number `1073741823`"),
            (r#"{"version":1,"cache_max_bytes":9007199254740993}"#, "found number `9007199254740993`"),
            (r#"{"version":1,"cache_max_bytes":0}"#, "found number `0`"),
            (r#"{"version":1,"cache_max_bytes":-5}"#, "found number `-5`"),
            (r#"{"version":1,"cache_max_bytes":2147483648.5}"#, "found number `2147483648.5`"),
            (r#"{"version":1,"cache_max_bytes":2e10}"#, "found number `2e10`"),
            (r#"{"version":1,"cache_max_bytes":99999999999999999999999}"#, "found number"),
            (r#"{"version":1,"cache_max_bytes":"5368709120"}"#, "found string"),
            (r#"{"version":1,"cache_max_bytes":null}"#, "found null"),
            (r#"{"version":1,"cache_max_bytes":true}"#, "found boolean"),
            (r#"{"version":1,"cache_max_bytes":[1]}"#, "found array"),
        ];
        for (doc, found) in cases {
            let text = rejection(doc.as_bytes());
            assert!(text.contains("`cache_max_bytes` must be an integer number of bytes"), "{text}");
            assert!(text.contains("1073741824 (1 GiB) to 9007199254740992 (2^53)"), "{text}");
            assert!(text.contains(found), "{text}");
        }
        let text = rejection(br#"{"version":1,"cache_max_bytes":1,"cache_max_bytes":1}"#);
        assert!(text.contains("duplicate object field"), "{text}");
    }
}
