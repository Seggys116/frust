//! Non-root toolchain install, upgrade, rollback, and uninstall.
//!
//! An update becomes active only after checksum verification, extraction, and an
//! atomic symlink swap. A partial tree is never the `current` version.

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::Command;

use frust_core::{hex32, sha256};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallError {
    pub message: String,
}

impl InstallError {
    fn new(message: impl Into<String>) -> Self {
        Self { message: message.into() }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrefixState {
    pub current: Option<String>,
    pub previous: Option<String>,
}

pub fn install(prefix: &Path, archive: &Path, checksum: &Path) -> Result<PrefixState, InstallError> {
    verify_checksum(archive, checksum)?;
    let staging = prefix.join("versions").join(format!(".partial-{}", archive_token(archive)));
    if staging.exists() {
        fs::remove_dir_all(&staging).map_err(|err| InstallError::new(format!("could not clear {}: {err}", staging.display())))?;
    }
    fs::create_dir_all(&staging).map_err(|err| InstallError::new(format!("could not create {}: {err}", staging.display())))?;
    extract(archive, &staging)?;
    let manifest = read_manifest(&staging)?;
    require_tree(&staging)?;
    let final_dir = prefix.join("versions").join(&manifest);
    if final_dir.exists() {
        fs::remove_dir_all(&final_dir).map_err(|err| InstallError::new(format!("could not replace {}: {err}", final_dir.display())))?;
    }
    fs::rename(&staging, &final_dir).map_err(|err| InstallError::new(format!("could not publish {}: {err}", final_dir.display())))?;
    activate(prefix, &manifest)
}

pub fn stage_without_activate(prefix: &Path, archive: &Path, checksum: &Path) -> Result<PathBuf, InstallError> {
    verify_checksum(archive, checksum)?;
    let staging = prefix.join("versions").join(format!(".partial-{}", archive_token(archive)));
    fs::create_dir_all(staging.parent().unwrap()).map_err(|err| InstallError::new(err.to_string()))?;
    if staging.exists() {
        fs::remove_dir_all(&staging).map_err(|err| InstallError::new(err.to_string()))?;
    }
    fs::create_dir_all(&staging).map_err(|err| InstallError::new(err.to_string()))?;
    extract(archive, &staging)?;
    Ok(staging)
}

pub fn rollback(prefix: &Path) -> Result<PrefixState, InstallError> {
    let state = read_state(prefix)?;
    let Some(previous) = state.previous.clone() else {
        return Err(InstallError::new("no previous version is recorded"));
    };
    let dir = prefix.join("versions").join(&previous);
    if !dir.join("manifest.json").is_file() {
        return Err(InstallError::new(format!("previous version {previous} is not installed")));
    }
    switch_current(prefix, &previous)?;
    write_state(prefix, &PrefixState { current: Some(previous), previous: state.current })?;
    read_state(prefix)
}

pub fn uninstall(prefix: &Path) -> Result<(), InstallError> {
    for child in ["current", "versions", "state.json"] {
        let path = prefix.join(child);
        if path.is_symlink() || path.is_file() {
            fs::remove_file(&path).map_err(|err| InstallError::new(format!("could not remove {}: {err}", path.display())))?;
        } else if path.is_dir() {
            fs::remove_dir_all(&path).map_err(|err| InstallError::new(format!("could not remove {}: {err}", path.display())))?;
        }
    }
    Ok(())
}

pub fn current_version(prefix: &Path) -> Result<Option<String>, InstallError> {
    Ok(read_state(prefix)?.current)
}

fn activate(prefix: &Path, id: &str) -> Result<PrefixState, InstallError> {
    let previous = read_state(prefix)?.current;
    switch_current(prefix, id)?;
    let state = PrefixState { current: Some(id.to_string()), previous };
    write_state(prefix, &state)?;
    Ok(state)
}

fn switch_current(prefix: &Path, id: &str) -> Result<(), InstallError> {
    let current = prefix.join("current");
    let temporary = prefix.join(".current-next");
    if temporary.exists() || temporary.symlink_metadata().is_ok() {
        let _ = fs::remove_file(&temporary);
    }
    std::os::unix::fs::symlink(Path::new("versions").join(id), &temporary)
        .map_err(|err| InstallError::new(format!("could not link {id}: {err}")))?;
    fs::rename(&temporary, &current).map_err(|err| InstallError::new(format!("could not activate {id}: {err}")))?;
    Ok(())
}

pub fn verify_checksum(archive: &Path, checksum: &Path) -> Result<(), InstallError> {
    let expected = fs::read_to_string(checksum).map_err(|err| InstallError::new(format!("could not read checksum: {err}")))?;
    let expected = expected.split_whitespace().next().unwrap_or("").trim();
    if expected.len() != 64 || !expected.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(InstallError::new(format!("checksum file {} does not contain a sha256 digest", checksum.display())));
    }
    let mut file = File::open(archive).map_err(|err| InstallError::new(format!("could not read archive: {err}")))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(|err| InstallError::new(format!("could not read archive: {err}")))?;
    let actual = hex32(&sha256(&bytes));
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(InstallError::new(format!(
            "checksum mismatch for {} before extraction: expected {expected}, computed {actual}",
            archive.display()
        )));
    }
    Ok(())
}

fn extract(archive: &Path, destination: &Path) -> Result<(), InstallError> {
    let status = Command::new("tar")
        .arg("-x")
        .arg("--file")
        .arg(archive)
        .arg("-C")
        .arg(destination)
        .status()
        .map_err(|err| InstallError::new(format!("could not run tar: {err}")))?;
    if !status.success() {
        return Err(InstallError::new(format!("tar exited {status} extracting {}", archive.display())));
    }
    Ok(())
}

fn read_manifest(tree: &Path) -> Result<String, InstallError> {
    let bytes = fs::read(tree.join("manifest.json")).map_err(|err| InstallError::new(format!("archive has no manifest.json ({err})")))?;
    let text = String::from_utf8(bytes).map_err(|_| InstallError::new("manifest.json is not UTF-8"))?;
    let marker = "\"id\":\"";
    let start = text.find(marker).ok_or_else(|| InstallError::new("manifest.json has no id"))? + marker.len();
    let end = text[start..].find('"').ok_or_else(|| InstallError::new("manifest.json id is unterminated"))?;
    let id = &text[start..start + end];
    if id.is_empty() || id.contains('/') || id.contains("..") {
        return Err(InstallError::new("manifest id is not a single path segment"));
    }
    Ok(id.to_string())
}

fn require_tree(tree: &Path) -> Result<(), InstallError> {
    for relative in ["bin/rustc", "bin/cargo", "bin/rustdoc"] {
        if !tree.join(relative).is_file() {
            return Err(InstallError::new(format!("archive is missing {relative}")));
        }
    }
    Ok(())
}

fn read_state(prefix: &Path) -> Result<PrefixState, InstallError> {
    let path = prefix.join("state.json");
    match fs::read_to_string(&path) {
        Ok(text) => parse_state(&text),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(PrefixState { current: None, previous: None }),
        Err(err) => Err(InstallError::new(format!("could not read state: {err}"))),
    }
}

fn parse_state(text: &str) -> Result<PrefixState, InstallError> {
    Ok(PrefixState {
        current: json_string_field(text, "current")?,
        previous: json_string_field(text, "previous")?,
    })
}

fn json_string_field(text: &str, field: &str) -> Result<Option<String>, InstallError> {
    let pattern = format!("\"{field}\":");
    let Some(start) = text.find(&pattern) else {
        return Ok(None);
    };
    let rest = text[start + pattern.len()..].trim_start();
    if rest.starts_with("null") {
        return Ok(None);
    }
    if !rest.starts_with('"') {
        return Err(InstallError::new(format!("state field {field} is not a string")));
    }
    let mut out = String::new();
    for ch in rest[1..].chars() {
        if ch == '"' {
            return Ok(Some(out));
        }
        out.push(ch);
    }
    Err(InstallError::new(format!("state field {field} is unterminated")))
}

fn write_state(prefix: &Path, state: &PrefixState) -> Result<(), InstallError> {
    fs::create_dir_all(prefix).map_err(|err| InstallError::new(err.to_string()))?;
    let body = format!(
        "{{\n  \"current\": {},\n  \"previous\": {}\n}}\n",
        json_opt(state.current.as_deref()),
        json_opt(state.previous.as_deref())
    );
    let temporary = prefix.join(".state.json.next");
    fs::write(&temporary, body).map_err(|err| InstallError::new(err.to_string()))?;
    fs::rename(&temporary, prefix.join("state.json")).map_err(|err| InstallError::new(err.to_string()))?;
    Ok(())
}

fn json_opt(value: Option<&str>) -> String {
    match value {
        Some(value) => format!("\"{value}\""),
        None => "null".to_string(),
    }
}

fn archive_token(path: &Path) -> String {
    hex32(&sha256(path.to_string_lossy().as_bytes()))[..12].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn scratch() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("frust-install-{n}-{}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn archive(dir: &Path, id: &str, marker: &str) -> (PathBuf, PathBuf) {
        let root = dir.join(format!("tree-{id}"));
        fs::create_dir_all(root.join("bin")).unwrap();
        for name in ["rustc", "cargo", "rustdoc", "frustc", "fcargo"] {
            fs::write(root.join("bin").join(name), marker).unwrap();
        }
        fs::write(root.join("manifest.json"), format!("{{\"id\":\"{id}\"}}\n")).unwrap();
        let archive = dir.join(format!("{id}.tar"));
        let status = Command::new("tar")
            .arg("-c")
            .arg("-f")
            .arg(&archive)
            .arg("-C")
            .arg(&root)
            .arg(".")
            .status()
            .unwrap();
        assert!(status.success());
        let bytes = fs::read(&archive).unwrap();
        let sum = dir.join(format!("{id}.sha256"));
        fs::write(&sum, format!("{}\n", hex32(&sha256(&bytes)))).unwrap();
        (archive, sum)
    }

    #[test]
    fn checksum_is_required_before_a_tree_exists() {
        let dir = scratch();
        let prefix = dir.join("prefix");
        let (archive_path, sum) = archive(&dir, "first", "one");
        fs::write(&sum, "0000000000000000000000000000000000000000000000000000000000000000\n").unwrap();
        let err = install(&prefix, &archive_path, &sum).unwrap_err();
        assert!(err.message.contains("checksum mismatch"));
        assert!(!prefix.join("current").exists());
        assert!(!prefix.join("versions").join("first").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_upgrade_rollback_uninstall_and_interrupted_update() {
        let dir = scratch();
        let prefix = dir.join("prefix");
        let (first, first_sum) = archive(&dir, "first", "one");
        let (second, second_sum) = archive(&dir, "second", "two");
        let state = install(&prefix, &first, &first_sum).unwrap();
        assert_eq!(state.current.as_deref(), Some("first"));
        assert_eq!(fs::read_to_string(prefix.join("current/bin/rustc")).unwrap(), "one");

        let partial = stage_without_activate(&prefix, &second, &second_sum).unwrap();
        assert!(partial.join("manifest.json").is_file());
        assert_eq!(fs::read_to_string(prefix.join("current/bin/rustc")).unwrap(), "one");
        assert!(!prefix.join("versions/second").exists());

        let state = install(&prefix, &second, &second_sum).unwrap();
        assert_eq!(state.current.as_deref(), Some("second"));
        assert_eq!(state.previous.as_deref(), Some("first"));
        assert_eq!(fs::read_to_string(prefix.join("current/bin/frustc")).unwrap(), "two");

        let state = rollback(&prefix).unwrap();
        assert_eq!(state.current.as_deref(), Some("first"));
        assert_eq!(fs::read_to_string(prefix.join("current/bin/rustc")).unwrap(), "one");

        uninstall(&prefix).unwrap();
        assert!(!prefix.join("current").exists());
        assert!(!prefix.join("versions").exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
