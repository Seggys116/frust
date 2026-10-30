//! Everything lives under one root so it can be inspected, moved or deleted as a unit.

use std::env;
use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

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

    pub fn ledger(&self) -> PathBuf {
        self.root.join("ledger.jsonl")
    }

    pub fn history(&self) -> PathBuf {
        self.root.join("history")
    }

    pub fn hashmemo(&self) -> PathBuf {
        self.root.join("hashmemo")
    }

    pub fn auto_gc_marker(&self) -> PathBuf {
        self.root.join("auto-gc")
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
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    let dir = base.join(format!("frust-{uid}"));
    match fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
        Err(err) => return Err(format!("cannot create private state directory {}: {err}", dir.display())),
    }
    let meta = fs::symlink_metadata(&dir).map_err(|e| format!("cannot inspect {}: {e}", dir.display()))?;
    let refuse = |why: &str| {
        Err(format!(
            "refusing to use {} as frust's state directory: {why}; set FRUST_HOME or HOME to a private location",
            dir.display()
        ))
    };
    if !meta.file_type().is_dir() {
        return refuse("it is not a directory");
    }
    if meta.uid() != uid {
        return refuse(&format!("it is owned by uid {}, not {uid}", meta.uid()));
    }
    let mode = meta.permissions().mode() & 0o7777;
    if mode != 0o700 {
        return refuse(&format!("its mode is {mode:o}, not 700"));
    }
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
