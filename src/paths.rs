use std::env;
use std::io;
use std::path::{Path, PathBuf};

use crate::sys;

/// Deferred-store jobs share `Dirs::deferred` with slot locks whose file-name prefix gc keys on.
pub const DEFERRED_SLOT_PREFIX: &str = "slot-";

#[derive(Debug, Clone)]
pub struct Dirs {
    root: PathBuf,
}

impl Dirs {
    pub fn discover() -> Dirs {
        match Dirs::try_discover() {
            Ok(dirs) => dirs,
            Err(err) => {
                eprintln!("frust: {err}");
                std::process::exit(1);
            }
        }
    }

    /// Without `HOME` or `XDG_CACHE_HOME` the fallback is a private per-user directory, never the shared temp directory itself.
    pub fn try_discover() -> Result<Dirs, String> {
        if let Some(home) = env::var_os("FRUST_HOME").filter(|v| !v.is_empty()) {
            return Ok(Dirs { root: PathBuf::from(home) });
        }
        Ok(Dirs { root: platform_cache_dir()?.join("frust") })
    }

    pub fn at(root: impl Into<PathBuf>) -> Dirs {
        Dirs { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn store(&self) -> PathBuf {
        self.root.join("store")
    }

    pub fn manifests(&self) -> PathBuf {
        self.root.join("manifests")
    }

    pub fn targets(&self) -> PathBuf {
        self.root.join("targets")
    }

    pub fn locks(&self) -> PathBuf {
        self.root.join("locks")
    }

    pub fn tmp(&self) -> PathBuf {
        self.root.join("tmp")
    }

    /// Per-prekey compile locks; kept apart from `locks()` because gc treats every entry there as a build-lock directory.
    pub fn keylocks(&self) -> PathBuf {
        self.root.join("keylocks")
    }

    pub fn deferred(&self) -> PathBuf {
        self.tmp().join("deferred")
    }

    pub fn ledger(&self) -> PathBuf {
        self.root.join("ledger.jsonl")
    }

    pub fn history(&self) -> PathBuf {
        self.root.join("history")
    }

    pub fn hashmemo(&self) -> PathBuf {
        self.root.join("hashmemo")
    }

    /// Violation log, quarantine markers and the audit counter of stamp-validated direct mode.
    pub fn stamp_trust(&self) -> PathBuf {
        self.root.join("stamp-trust")
    }

    pub fn auto_gc_marker(&self) -> PathBuf {
        self.root.join("auto-gc")
    }

    pub fn compact_memo_dir(&self) -> PathBuf {
        self.root.join("compact-memo")
    }

    /// One memo per target directory; the version in the name retires old formats without reading them.
    pub fn compact_memo(&self, target: &Path) -> PathBuf {
        self.compact_memo_dir().join(format!("{}-v1.bin", short_hash(target.as_os_str().as_encoded_bytes())))
    }
}

fn platform_cache_dir() -> Result<PathBuf, String> {
    let home = env::var_os("HOME").filter(|v| !v.is_empty()).map(PathBuf::from);
    if cfg!(target_os = "macos") {
        if let Some(home) = home {
            return Ok(home.join("Library").join("Caches"));
        }
    } else if cfg!(windows) {
        if let Some(local) = env::var_os("LOCALAPPDATA") {
            return Ok(PathBuf::from(local));
        }
    } else {
        if let Some(xdg) = env::var_os("XDG_CACHE_HOME").filter(|v| !v.is_empty()) {
            return Ok(PathBuf::from(xdg));
        }
        if let Some(home) = home {
            return Ok(home.join(".cache"));
        }
    }
    private_temp_dir(&env::temp_dir())
}

/// The shared temp dir is world-writable: another user could pre-create this directory and plant cache entries, so unverified ones are refused.
pub fn private_temp_dir(base: &Path) -> Result<PathBuf, String> {
    let dir = base.join(format!("frust-{}", sys::user_id()));
    match sys::create_dir_with_mode(&dir, 0o700) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
        Err(err) => return Err(format!("cannot create private state directory {}: {err}", dir.display())),
    }
    sys::verify_private_dir(&dir)?;
    Ok(dir)
}

pub fn short_hash(input: impl AsRef<[u8]>) -> String {
    let hash = blake3::hash(input.as_ref());
    hash.to_hex()[..12].to_string()
}

pub fn slug(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' })
        .collect();
    let trimmed = cleaned.trim_matches('.');
    if trimmed.is_empty() { "workspace".to_string() } else { trimmed.chars().take(48).collect() }
}
