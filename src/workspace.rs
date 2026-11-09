//! Central target layout `<root>/targets/<name>-<hash>/<toolchain id>/`, keyed by toolchain so switching rustc never invalidates the other's artifacts.

use std::env;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::paths::{Dirs, short_hash, slug};
use crate::toolchain::memo::{self, Digest};
use crate::toolchain::{CacheOutcome, direct_binary, resolve_tool};

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
    locate_cached_early(dirs, cwd, toolchain, manifest_path, toolchain_id, None)
}

/// A `cargo locate-project` started before the toolchain id was known, with the key its inputs had when it started.
pub struct EarlyLocate {
    before: Option<String>,
    handle: std::thread::JoinHandle<Option<PathBuf>>,
}

impl EarlyLocate {
    pub fn start(cwd: &Path, toolchain: Option<&OsStr>, manifest_path: Option<&OsStr>) -> EarlyLocate {
        let before = locate_key(cwd, toolchain, manifest_path, "").map(|(key, _)| key);
        let (cwd, toolchain, manifest_path) = (cwd.to_path_buf(), toolchain.map(OsStr::to_os_string), manifest_path.map(OsStr::to_os_string));
        let handle = std::thread::spawn(move || locate(&cwd, toolchain.as_deref(), manifest_path.as_deref()));
        EarlyLocate { before, handle }
    }

    /// The answer, and whether the inputs stayed as they were from before it started until now.
    fn finish(self, cwd: &Path, toolchain: Option<&OsStr>, manifest_path: Option<&OsStr>) -> (Option<PathBuf>, bool) {
        let root = self.handle.join().ok().flatten();
        let settled = self.before.is_some() && locate_key(cwd, toolchain, manifest_path, "").map(|(key, _)| key) == self.before;
        (root, settled)
    }
}

/// As [`locate_cached`]; a miss takes `early`'s answer instead of asking cargo again.
pub fn locate_cached_early(
    dirs: &Dirs,
    cwd: &Path,
    toolchain: Option<&OsStr>,
    manifest_path: Option<&OsStr>,
    toolchain_id: &str,
    early: Option<EarlyLocate>,
) -> (Option<PathBuf>, CacheOutcome) {
    let run = |early: Option<EarlyLocate>| match early {
        Some(early) => early.finish(cwd, toolchain, manifest_path),
        None => (locate(cwd, toolchain, manifest_path), true),
    };
    let Some((key, chain)) = locate_key(cwd, toolchain, manifest_path, toolchain_id) else {
        return (run(early).0, CacheOutcome::Bypass);
    };
    let slot = locate_slot(dirs, cwd, toolchain, manifest_path);
    if let Some(entry) = memo::load::<LocateEntry>(&slot)
        && entry.format == LOCATE_FORMAT
        && entry.key == key
        && chain.contains(&entry.root)
    {
        return (Some(entry.root), CacheOutcome::Hit);
    }
    let _guard = memo::cold_miss(dirs, &slot);
    let Some((key, chain)) = locate_key(cwd, toolchain, manifest_path, toolchain_id) else {
        return (run(early).0, CacheOutcome::Bypass);
    };
    if let Some(entry) = memo::load::<LocateEntry>(&slot)
        && entry.format == LOCATE_FORMAT
        && entry.key == key
        && chain.contains(&entry.root)
    {
        return (Some(entry.root), CacheOutcome::Hit);
    }
    let (root, settled) = run(early);
    if let Some(root) = root.as_ref().filter(|r| chain.contains(r)) {
        // Inputs edited while cargo ran: use the answer, but do not memoise it.
        let unchanged = settled && locate_key(cwd, toolchain, manifest_path, toolchain_id).is_some_and(|(again, _)| again == key);
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
    let mut cmd = match direct_binary(&cargo, "cargo", toolchain, cwd) {
        Some(direct) => Command::new(direct),
        None => {
            let mut cmd = Command::new(cargo);
            cmd.args(toolchain);
            cmd
        }
    };
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

/// Suffix of the sibling directory that holds `check` and `clippy` artifacts of one toolchain.
const CHECK_SUFFIX: &str = "-check";

/// Cargo locks each profile directory of a target directory exclusively, so check and build only run side by side when they use different target directories.
pub fn check_target(build: &Path) -> PathBuf {
    let mut name = build.file_name().map(OsStr::to_os_string).unwrap_or_default();
    name.push(CHECK_SUFFIX);
    build.with_file_name(name)
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
    fn check_target_is_a_sibling_of_the_build_target() {
        let dirs = Dirs::at("/cache/frust");
        let build = central_target(&dirs, Some(Path::new("/w/app")), "abc123");
        let check = check_target(&build);
        assert_eq!(check.parent(), build.parent());
        assert_eq!(check.file_name().unwrap(), "abc123-check");
    }

    #[test]
    fn locating_answers_what_the_cargo_proxy_answers() {
        let Some(cargo) = resolve_tool("cargo") else { return };
        let cwd = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let out = Command::new(cargo).args(["locate-project", "--workspace", "--message-format", "plain"]).current_dir(&cwd).output().unwrap();
        let proxied = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
        assert_eq!(locate(&cwd, None, None), Some(crate::sys::canonicalize(proxied.parent().unwrap()).unwrap()));
    }

    #[test]
    fn central_layout_nests_toolchain() {
        let dirs = Dirs::at("/cache/frust");
        let dir = central_target(&dirs, Some(Path::new("/w/app")), "abc123");
        assert!(dir.starts_with("/cache/frust/targets"));
        assert!(dir.ends_with("abc123"));
    }
}
