//! User JSON roots, workspace namespaces, and the optional artifact cache.
//!
//! This module is standard-library only so the Cargo patch can compile the same file.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

pub const CACHE_MARKER: &str = "frust-cache-v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostOs {
    Linux,
    Macos,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserConfig {
    pub target_root: Option<PathBuf>,
    pub cache_root: Option<PathBuf>,
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
             Write {{\"version\":1}} and, if set, absolute \"target_root\" and \"cache_root\" strings.",
            self.path, self.reason
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
    let value = parse_json(text).map_err(|reason| error(path, reason))?;
    let pairs = match value {
        Json::Object(pairs) => pairs,
        _ => return Err(error(path, "top-level value must be an object")),
    };
    let mut version = None;
    let mut target_root = None;
    let mut cache_root = None;
    for (key, item) in pairs {
        match key.as_str() {
            "version" => version = Some(item),
            "target_root" => target_root = Some(item),
            "cache_root" => cache_root = Some(item),
            other => {
                return Err(error(path, &format!("unknown field `{other}`")));
            }
        }
    }
    match version {
        Some(Json::Number(raw)) if raw == "1" => {}
        Some(_) => return Err(error(path, "`version` must be the integer 1")),
        None => return Err(error(path, "missing `version`")),
    }
    Ok(UserConfig {
        target_root: optional_absolute(path, "target_root", target_root)?,
        cache_root: optional_absolute(path, "cache_root", cache_root)?,
    })
}

fn optional_absolute(
    path: &Path,
    field: &str,
    value: Option<Json>,
) -> Result<Option<PathBuf>, ConfigError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let Json::String(text) = value else {
        return Err(error(path, &format!("`{field}` must be a string")));
    };
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
    let config_path = config_path(os, home, xdg_config_home);
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
    Ok(ResolvedRoots {
        config_path,
        targets_dir: target_root.join("targets"),
        artifacts_dir: cache_root.join("artifacts"),
        target_root,
        cache_root,
    })
}

/// Load `config.json` from the host location. A missing file uses defaults.
pub fn load_roots_from_host(
    os: HostOs,
    home: &Path,
    xdg_config_home: Option<&str>,
    xdg_cache_home: Option<&str>,
) -> Result<ResolvedRoots, ConfigError> {
    let path = config_path(os, home, xdg_config_home);
    match read_file(&path) {
        Ok(bytes) => resolve_roots(os, home, xdg_config_home, xdg_cache_home, Some(&bytes)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            resolve_roots(os, home, xdg_config_home, xdg_cache_home, None)
        }
        Err(err) => Err(error(&path, &format!("could not read file ({err})"))),
    }
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

pub fn open_artifact_cache(dir: &Path) -> CacheOpen {
    if dir.exists() && !dir.is_dir() {
        return CacheOpen::Fallback { reason: "cache path is not a directory" };
    }
    if let Err(err) = fs::create_dir_all(dir) {
        let _ = err;
        return CacheOpen::Fallback { reason: "cache directory is unwritable" };
    }
    let marker = dir.join("MARKER");
    match fs::read_to_string(&marker) {
        Ok(text) if text == CACHE_MARKER => CacheOpen::Ready,
        Ok(_) => CacheOpen::Fallback { reason: "cache marker is corrupt" },
        Err(err) if err.kind() == io::ErrorKind::NotFound => match write_new_marker(&marker) {
            Ok(()) => CacheOpen::Ready,
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => match fs::read_to_string(&marker) {
                Ok(text) if text == CACHE_MARKER => CacheOpen::Ready,
                Ok(_) => CacheOpen::Fallback { reason: "cache marker is corrupt" },
                Err(_) => CacheOpen::Fallback { reason: "cache marker is unreadable" },
            },
            Err(_) => CacheOpen::Fallback { reason: "cache directory is unwritable" },
        },
        Err(_) => CacheOpen::Fallback { reason: "cache marker is unreadable" },
    }
}

fn write_new_marker(path: &Path) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(CACHE_MARKER.as_bytes())?;
    file.sync_all()?;
    Ok(())
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
    Other,
}

fn parse_json(input: &str) -> Result<Json, &'static str> {
    let mut parser = Parser { bytes: input.as_bytes(), index: 0 };
    parser.skip();
    let value = parser.value()?;
    parser.skip();
    if parser.index != parser.bytes.len() {
        return Err("trailing data after the JSON value");
    }
    Ok(value)
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

    fn value(&mut self) -> Result<Json, &'static str> {
        self.skip();
        match self.bytes.get(self.index).copied() {
            Some(b'{') => self.object(),
            Some(b'"') => Ok(Json::String(self.string()?)),
            Some(b'-' | b'0'..=b'9') => Ok(Json::Number(self.number()?)),
            Some(_) => {
                self.skip_other()?;
                Ok(Json::Other)
            }
            None => Err("unexpected end of JSON"),
        }
    }

    fn object(&mut self) -> Result<Json, &'static str> {
        self.index += 1;
        let mut pairs = Vec::new();
        loop {
            self.skip();
            if self.bytes.get(self.index) == Some(&b'}') {
                self.index += 1;
                break;
            }
            if !pairs.is_empty() {
                match self.bytes.get(self.index) {
                    Some(&b',') => {
                        self.index += 1;
                        self.skip();
                    }
                    None => return Err("unexpected end of JSON"),
                    _ => return Err("expected a comma between object fields"),
                }
            }
            if self.bytes.get(self.index) != Some(&b'"') {
                return Err("object field name must be a string");
            }
            let key = self.string()?;
            if pairs.iter().any(|(existing, _)| existing == &key) {
                return Err("duplicate object field");
            }
            self.skip();
            if self.bytes.get(self.index) != Some(&b':') {
                return Err("expected ':' after an object field");
            }
            self.index += 1;
            pairs.push((key, self.value()?));
        }
        Ok(Json::Object(pairs))
    }

    fn string(&mut self) -> Result<String, &'static str> {
        self.index += 1;
        let (text, consumed) = crate::json_text::decode_json_string_body(&self.bytes[self.index..])?;
        self.index += consumed;
        Ok(text)
    }

    fn number(&mut self) -> Result<String, &'static str> {
        let start = self.index;
        if self.bytes.get(self.index) == Some(&b'-') {
            self.index += 1;
        }
        let digits = self.index;
        while matches!(self.bytes.get(self.index), Some(b'0'..=b'9')) {
            self.index += 1;
        }
        if self.index == digits {
            return Err("invalid number");
        }
        if matches!(self.bytes.get(self.index), Some(b'.' | b'e' | b'E')) {
            return Err("`version` must be an integer");
        }
        Ok(std::str::from_utf8(&self.bytes[start..self.index]).unwrap().to_string())
    }

    fn skip_other(&mut self) -> Result<(), &'static str> {
        let rest = &self.bytes[self.index..];
        for literal in [b"true".as_slice(), b"false".as_slice(), b"null".as_slice()] {
            if rest.starts_with(literal) {
                self.index += literal.len();
                return Ok(());
            }
        }
        if rest.first() == Some(&b'[') {
            self.index += 1;
            loop {
                self.skip();
                if self.bytes.get(self.index) == Some(&b']') {
                    self.index += 1;
                    return Ok(());
                }
                let _ = self.value()?;
                self.skip();
                if self.bytes.get(self.index) == Some(&b',') {
                    self.index += 1;
                    continue;
                }
                if self.bytes.get(self.index) == Some(&b']') {
                    self.index += 1;
                    return Ok(());
                }
                return Err("invalid array");
            }
        }
        Err("invalid JSON value")
    }
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
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn scratch() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir().join(format!("frust-paths-{nanos}-{n}"));
        fs::create_dir_all(&path).unwrap();
        path
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
}
