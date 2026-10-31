//! Central target layout `<root>/targets/<name>-<hash>/<toolchain id>/`, keyed by toolchain so switching rustc never invalidates the other's artifacts.

use std::env;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::paths::{Dirs, short_hash, slug};
use crate::toolchain::memo::{self, Digest};
use crate::toolchain::{CacheOutcome, resolve_tool};

/// Bump when the entry layout or the meaning of its key changes.
const LOCATE_FORMAT: &str = "frust-locate-v1";

#[derive(Debug, Serialize, Deserialize)]
struct LocateEntry {
    format: String,
    key: String,
    root: PathBuf,
}

/// The key hashes the `Cargo.toml` contents (not mtimes) along the ancestor chain plus toolchain, `--manifest-path` and `CARGO_HOME`/`__CARGO*` env; a root off that chain (`package.workspace`) or a failure is never memoised.
pub fn locate_cached(
    dirs: &Dirs,
    cwd: &Path,
    toolchain: Option<&OsStr>,
    manifest_path: Option<&OsStr>,
    toolchain_id: &str,
) -> (Option<PathBuf>, CacheOutcome) {
    let Some((key, chain)) = locate_key(cwd, toolchain, manifest_path, toolchain_id) else {
        return (locate(cwd, toolchain, manifest_path), CacheOutcome::Bypass);
    };
    let slot = locate_slot(dirs, cwd, toolchain, manifest_path);
    if let Some(entry) = memo::load::<LocateEntry>(&slot)
        && entry.format == LOCATE_FORMAT
        && entry.key == key
        && chain.contains(&entry.root)
    {
        return (Some(entry.root), CacheOutcome::Hit);
    }
    let root = locate(cwd, toolchain, manifest_path);
    if let Some(root) = root.as_ref().filter(|r| chain.contains(r)) {
        // Inputs edited while cargo ran: use the answer, but do not memoise it.
        let unchanged = locate_key(cwd, toolchain, manifest_path, toolchain_id).is_some_and(|(again, _)| again == key);
        if unchanged {
            memo::store(dirs, &slot, &LocateEntry { format: LOCATE_FORMAT.to_string(), key, root: root.clone() });
        }
    }
    (root, CacheOutcome::Miss)
}

fn locate_slot(dirs: &Dirs, cwd: &Path, toolchain: Option<&OsStr>, manifest_path: Option<&OsStr>) -> PathBuf {
    let mut d = Digest::new("locate-slot");
    d.bytes(cwd.as_os_str().as_encoded_bytes());
    d.opt(toolchain.map(OsStr::as_encoded_bytes));
    d.opt(manifest_path.map(OsStr::as_encoded_bytes));
    dirs.root().join("locate").join(format!("{}.json", d.short()))
}

fn locate_key(cwd: &Path, toolchain: Option<&OsStr>, manifest_path: Option<&OsStr>, toolchain_id: &str) -> Option<(String, Vec<PathBuf>)> {
    let mut d = Digest::new(LOCATE_FORMAT);
    d.bytes(toolchain_id.as_bytes());
    d.opt(toolchain.map(OsStr::as_encoded_bytes));
    d.opt(manifest_path.map(OsStr::as_encoded_bytes));
    let mut vars: Vec<(OsString, OsString)> =
        env::vars_os().filter(|(k, _)| k.as_encoded_bytes().starts_with(b"__CARGO") || k == "CARGO_HOME").collect();
    vars.sort();
    for (k, v) in &vars {
        d.bytes(k.as_encoded_bytes());
        d.bytes(v.as_encoded_bytes());
    }
    let mut chain = memo::ancestor_chains(cwd);
    if let Some(manifest) = manifest_path {
        let file = cwd.join(manifest);
        d.file(&file)?;
        if let Some(dir) = file.parent() {
            for a in memo::ancestor_chains(dir) {
                if !chain.contains(&a) {
                    chain.push(a);
                }
            }
        }
    }
    for dir in &chain {
        d.file(&dir.join("Cargo.toml"))?;
    }
    Some((d.finish(), chain))
}

pub fn locate(cwd: &Path, toolchain: Option<&OsStr>, manifest_path: Option<&OsStr>) -> Option<PathBuf> {
    let cargo = resolve_tool("cargo")?;
    let mut cmd = Command::new(cargo);
    if let Some(tc) = toolchain {
        cmd.arg(tc);
    }
    cmd.args(["locate-project", "--workspace", "--message-format", "plain"]);
    if let Some(manifest) = manifest_path {
        cmd.arg("--manifest-path").arg(manifest);
    }
    let output = cmd.current_dir(cwd).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let manifest = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    // Cargo echoes `--manifest-path` as typed but resolves symlinks otherwise; canonicalise so both spellings share one target directory and lock.
    let root = manifest.parent()?;
    Some(crate::sys::canonicalize(root).unwrap_or_else(|_| root.to_path_buf()))
}

pub fn workspace_key(root: &Path) -> String {
    let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    format!("{}-{}", slug(&name), short_hash(root.to_string_lossy().as_bytes()))
}

pub fn central_target(dirs: &Dirs, root: Option<&Path>, toolchain_id: &str) -> PathBuf {
    let key = match root {
        Some(root) => workspace_key(root),
        None => "_loose".to_string(),
    };
    dirs.targets().join(key).join(toolchain_id)
}

pub fn display_name(root: Option<&Path>) -> String {
    root.and_then(|r| r.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "cargo".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_stable_and_distinct() {
        let a = workspace_key(Path::new("/work/alpha/app"));
        let b = workspace_key(Path::new("/work/beta/app"));
        assert!(a.starts_with("app-"));
        assert_ne!(a, b);
        assert_eq!(a, workspace_key(Path::new("/work/alpha/app")));
    }

    #[test]
    fn central_layout_nests_toolchain() {
        let dirs = Dirs::at("/cache/frust");
        let dir = central_target(&dirs, Some(Path::new("/w/app")), "abc123");
        assert!(dir.starts_with("/cache/frust/targets"));
        assert!(dir.ends_with("abc123"));
    }
}
