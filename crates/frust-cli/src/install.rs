//! Non-root toolchain install, upgrade, rollback, and uninstall.
//!
//! An archive is copied into a private staging file while it is hashed, so the
//! bytes that were verified are the bytes that are extracted. An update becomes
//! active only after the digest matches, the tar headers pass validation, the
//! tree is extracted and checked, and the `current` symlink is swapped. The
//! symlink is the source of truth for the active version; `state.json` only
//! adds the rollback target and is reconciled against it. It also records the
//! build variant (`baseline`, `v3`) that each of the two versions was packaged
//! as, taken from the optional `variant` member of their `manifest.json`.
//! On Linux the large files of the extracted tree are rewritten with large
//! writes before activation, so the page cache holds them in large folios (see
//! [`relayout`]).

use std::collections::HashSet;
use std::fmt::Display;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use frust_core::{decode_json_string_body, hex32, sha256};

const LOCK_NAME: &str = ".frust-install.lock";
const WORK_PREFIX: &str = ".partial-work.";
const ASIDE_PREFIX: &str = ".partial-aside.";
const COPY_BUFFER: usize = 64 * 1024;
const MAX_ID_LEN: usize = 128;
const MAX_VARIANT_LEN: usize = 32;
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_JSON_DEPTH: usize = 32;
const REQUIRED_FILES: [&str; 3] = ["bin/rustc", "bin/cargo", "bin/rustdoc"];

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
    /// Variant of `current`; `None` when its manifest names none (macOS, installs made before variants).
    pub current_variant: Option<String>,
    /// Variant of `previous`, with the same meaning.
    pub previous_variant: Option<String>,
}

/// What the installer reads from a tree's `manifest.json`.
struct Manifest {
    id: String,
    variant: Option<String>,
}

fn io_context(what: impl Display) -> impl FnOnce(io::Error) -> InstallError {
    let what = what.to_string();
    move |err| InstallError::new(format!("{what}: {err}"))
}

pub fn install(prefix: &Path, archive: &Path, checksum: &Path) -> Result<PrefixState, InstallError> {
    let expected = read_expected_digest(checksum)?;
    let mut source = open_archive(archive)?;
    let _lock = lock_prefix(prefix, true)?;
    clear_leftovers(prefix)?;
    let recorded = reconcile(prefix)?;
    let work = Work::create(prefix)?;
    let tree = unpack(&work, &mut source, archive, &expected)?;
    let manifest = read_manifest(&tree)?;
    require_tree(&tree)?;
    let aside = publish(prefix, &tree, &manifest.id)?;
    let result = activate(prefix, &manifest, &recorded);
    if let Some(aside) = aside {
        let _ = fs::remove_dir_all(aside);
    }
    result
}

pub fn stage_without_activate(prefix: &Path, archive: &Path, checksum: &Path) -> Result<PathBuf, InstallError> {
    let expected = read_expected_digest(checksum)?;
    let mut source = open_archive(archive)?;
    let _lock = lock_prefix(prefix, true)?;
    clear_leftovers(prefix)?;
    let work = Work::create(prefix)?;
    let tree = unpack(&work, &mut source, archive, &expected)?;
    read_manifest(&tree)?;
    require_tree(&tree)?;
    let staged = prefix.join("versions").join(format!(".partial-{}", archive_token(archive)));
    remove_path(&staged)?;
    fs::rename(&tree, &staged).map_err(io_context(format!("could not stage {}", staged.display())))?;
    Ok(staged)
}

pub fn rollback(prefix: &Path) -> Result<PrefixState, InstallError> {
    let _lock = lock_prefix(prefix, false)?;
    clear_leftovers(prefix)?;
    let state = reconcile(prefix)?;
    let Some(previous) = state.previous.clone() else {
        return Err(InstallError::new("no previous version is recorded"));
    };
    let dir = prefix.join("versions").join(&previous);
    if !dir.join("manifest.json").is_file() {
        return Err(InstallError::new(format!("previous version {previous} is not installed")));
    }
    switch_current(prefix, &previous)?;
    // A state written before variants existed names none; the manifests fill them in.
    let current_variant = state.previous_variant.clone().or_else(|| installed_variant(prefix, &previous));
    let previous_variant = state.current_variant.clone().or_else(|| state.current.as_deref().and_then(|id| installed_variant(prefix, id)));
    let next = PrefixState { current: Some(previous), previous: state.current, current_variant, previous_variant };
    write_state(prefix, &next)?;
    Ok(next)
}

pub fn uninstall(prefix: &Path) -> Result<(), InstallError> {
    match fs::symlink_metadata(prefix) {
        Ok(_) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(InstallError::new(format!("could not inspect {}: {err}", prefix.display()))),
    }
    // The lock file stays: deleting it would let a waiting process lock a dead inode.
    let _lock = lock_prefix(prefix, false)?;
    clear_temporaries(prefix)?;
    for child in ["current", "versions", "state.json"] {
        remove_path(&prefix.join(child))?;
    }
    Ok(())
}

pub fn current_version(prefix: &Path) -> Result<Option<String>, InstallError> {
    active_version(prefix)
}

/// The variant of the active version, or `None` when nothing is installed or its manifest names none.
pub fn current_variant(prefix: &Path) -> Result<Option<String>, InstallError> {
    let Some(active) = active_version(prefix)? else {
        return Ok(None);
    };
    // An unreadable state.json is not an error here: the install that follows reports it. A state
    // written before variants existed names none, so the manifest of the tree answers then.
    if let Ok(recorded) = read_state(prefix) {
        if recorded.current.as_deref() == Some(active.as_str()) && recorded.current_variant.is_some() {
            return Ok(recorded.current_variant);
        }
    }
    Ok(installed_variant(prefix, &active))
}

pub fn verify_checksum(archive: &Path, checksum: &Path) -> Result<(), InstallError> {
    let expected = read_expected_digest(checksum)?;
    let mut file = open_archive(archive)?;
    let actual = digest_stream(&mut file, None).map_err(io_context("could not read archive"))?;
    compare_digest(archive, &expected, &actual)
}

fn activate(prefix: &Path, manifest: &Manifest, recorded: &PrefixState) -> Result<PrefixState, InstallError> {
    let id = manifest.id.as_str();
    // Re-activating the live id must not turn the rollback target into itself.
    let (previous, previous_variant) = if recorded.current.as_deref() == Some(id) {
        (recorded.previous.clone(), recorded.previous_variant.clone())
    } else {
        (recorded.current.clone(), recorded.current_variant.clone())
    };
    switch_current(prefix, id)?;
    let state = PrefixState { current: Some(id.to_string()), previous, current_variant: manifest.variant.clone(), previous_variant };
    write_state(prefix, &state)?;
    Ok(state)
}

fn switch_current(prefix: &Path, id: &str) -> Result<(), InstallError> {
    let temporary = prefix.join(format!(".current-next.{}", unique_suffix()));
    std::os::unix::fs::symlink(Path::new("versions").join(id), &temporary).map_err(io_context(format!("could not link {id}")))?;
    if let Err(err) = fs::rename(&temporary, prefix.join("current")) {
        let _ = fs::remove_file(&temporary);
        return Err(InstallError::new(format!("could not activate {id}: {err}")));
    }
    sync_dir(prefix);
    Ok(())
}

/// Tree copies live under `versions/` so a rename into place never crosses filesystems.
struct Work {
    dir: PathBuf,
}

impl Work {
    fn create(prefix: &Path) -> Result<Self, InstallError> {
        let versions = prefix.join("versions");
        fs::create_dir_all(&versions).map_err(io_context(format!("could not create {} (the prefix must be writable)", versions.display())))?;
        let dir = versions.join(format!("{WORK_PREFIX}{}", unique_suffix()));
        fs::DirBuilder::new().mode(0o700).create(&dir).map_err(io_context(format!("could not create {}", dir.display())))?;
        Ok(Self { dir })
    }
}

impl Drop for Work {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn unpack(work: &Work, source: &mut File, archive: &Path, expected: &str) -> Result<PathBuf, InstallError> {
    let private = work.dir.join("archive.tar");
    let mut copy = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&private)
        .map_err(io_context(format!("could not create {}", private.display())))?;
    let actual = digest_stream(source, Some(&mut copy)).map_err(io_context(format!("could not copy {}", archive.display())))?;
    drop(copy);
    compare_digest(archive, expected, &actual)?;
    tar_check::validate(&private)?;
    let tree = work.dir.join("tree");
    fs::create_dir(&tree).map_err(io_context(format!("could not create {}", tree.display())))?;
    extract(&private, &tree)?;
    // Only Linux page caches are known to size folios by the write; elsewhere the pass would cost time for nothing.
    if cfg!(target_os = "linux") {
        relayout::rewrite_large_files(&tree)?;
    }
    Ok(tree)
}

/// Moves a verified tree into `versions/<id>`; returns the displaced old copy of that id, if any.
fn publish(prefix: &Path, tree: &Path, id: &str) -> Result<Option<PathBuf>, InstallError> {
    let versions = prefix.join("versions");
    let final_dir = versions.join(id);
    let mut aside = None;
    if final_dir.symlink_metadata().is_ok() {
        let moved = versions.join(format!("{ASIDE_PREFIX}{}.{id}", unique_suffix()));
        fs::rename(&final_dir, &moved).map_err(io_context(format!("could not replace {}", final_dir.display())))?;
        aside = Some(moved);
    }
    if let Err(err) = fs::rename(tree, &final_dir) {
        if let Some(moved) = &aside {
            let _ = fs::rename(moved, &final_dir);
        }
        return Err(InstallError::new(format!("could not publish {}: {err}", final_dir.display())));
    }
    Ok(aside)
}

fn open_archive(archive: &Path) -> Result<File, InstallError> {
    File::open(archive).map_err(io_context(format!("could not read archive {}", archive.display())))
}

fn read_expected_digest(checksum: &Path) -> Result<String, InstallError> {
    let text = fs::read_to_string(checksum).map_err(io_context(format!("could not read checksum {}", checksum.display())))?;
    let expected = text.split_whitespace().next().unwrap_or("");
    if expected.len() != 64 || !expected.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(InstallError::new(format!("checksum file {} does not contain a sha256 digest", checksum.display())));
    }
    Ok(expected.to_string())
}

fn compare_digest(archive: &Path, expected: &str, actual: &str) -> Result<(), InstallError> {
    if actual.eq_ignore_ascii_case(expected) {
        return Ok(());
    }
    Err(InstallError::new(format!(
        "checksum mismatch for {} before extraction: expected {expected}, computed {actual}",
        archive.display()
    )))
}

/// Hashes `source` with a fixed buffer, optionally copying every byte to `sink`.
fn digest_stream(source: &mut File, mut sink: Option<&mut File>) -> io::Result<String> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; COPY_BUFFER];
    loop {
        let count = match source.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        hasher.update(&buffer[..count]);
        if let Some(sink) = sink.as_deref_mut() {
            sink.write_all(&buffer[..count])?;
        }
    }
    if let Some(sink) = sink {
        sink.sync_all()?;
    }
    Ok(hex32(&hasher.finish()))
}

fn extract(archive: &Path, destination: &Path) -> Result<(), InstallError> {
    let status = Command::new("tar")
        .arg("-x")
        .arg("--no-same-owner")
        .arg("--file")
        .arg(archive)
        .arg("-C")
        .arg(destination)
        .status()
        .map_err(io_context("could not run tar"))?;
    if !status.success() {
        return Err(InstallError::new(format!("tar exited {status} extracting the verified archive")));
    }
    Ok(())
}

/// Rewrites the large regular files of an extracted tree with large writes.
///
/// On Linux file systems with large folio support (ext4 on recent kernels, XFS) a buffered write
/// allocates page-cache folios sized by the write, and a later `mmap` maps a cached 2 MiB folio
/// with one PMD entry. `tar` writes 10 KiB at a time, which leaves the compiler's shared libraries
/// cached in small folios, while rustup writes 16 MiB at a time and its libraries end up mapped with
/// 2 MiB pages: same bytes, measurably fewer iTLB misses for rustup's copy. Truncating a file drops
/// its cached folios, so writing the bytes back in large chunks gives the tree the same page-cache
/// state as a rustup install. Inode, hard links, mode and times are kept.
///
/// Anything that fails before a file is truncated leaves that file as `tar` wrote it; a failure
/// after the truncation fails the install, because the bytes on disk are no longer complete.
mod relayout {
    use super::*;
    use std::fs::{FileTimes, Metadata, Permissions};
    use std::io::{Seek, SeekFrom};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    /// One PMD-sized folio: smaller files cannot be mapped with 2 MiB pages anyway.
    pub(super) const MIN_BYTES: u64 = 2 << 20;
    /// Whole files are buffered; above this the file is left as extracted.
    const MAX_BYTES: u64 = 1 << 30;
    /// Large enough that every 2 MiB aligned range is covered by one write.
    const CHUNK: usize = 32 << 20;

    /// Returns how many files were rewritten.
    pub(super) fn rewrite_large_files(tree: &Path) -> Result<usize, InstallError> {
        let mut seen = HashSet::new();
        let mut rewritten = 0;
        let mut pending = vec![tree.to_path_buf()];
        while let Some(dir) = pending.pop() {
            let Ok(entries) = fs::read_dir(&dir) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(metadata) = fs::symlink_metadata(&path) else { continue };
                if metadata.is_dir() {
                    pending.push(path);
                } else if metadata.is_file() && (MIN_BYTES..=MAX_BYTES).contains(&metadata.len()) && seen.insert((metadata.dev(), metadata.ino())) {
                    rewritten += usize::from(rewrite(&path, &metadata)?);
                }
            }
        }
        Ok(rewritten)
    }

    fn rewrite(path: &Path, metadata: &Metadata) -> Result<bool, InstallError> {
        let original = metadata.permissions();
        let writable = original.mode() & 0o200 != 0;
        if !writable && fs::set_permissions(path, Permissions::from_mode(original.mode() | 0o200)).is_err() {
            return Ok(false);
        }
        let result = rewrite_writable(path, metadata);
        let restored = fs::set_permissions(path, original);
        match result {
            Ok(done) => restored.map(|()| done).map_err(io_context(format!("could not restore the mode of {}", path.display()))),
            Err(err) => Err(err),
        }
    }

    fn rewrite_writable(path: &Path, metadata: &Metadata) -> Result<bool, InstallError> {
        let length = metadata.len();
        let Ok(mut file) = OpenOptions::new().read(true).write(true).open(path) else { return Ok(false) };
        let mut data = Vec::new();
        if data.try_reserve_exact(length as usize).is_err() {
            return Ok(false);
        }
        if (&mut file).take(length + 1).read_to_end(&mut data).is_err() || data.len() as u64 != length {
            return Ok(false);
        }
        write_back(&mut file, &data).map_err(io_context(format!("could not rewrite {}", path.display())))?;
        // The bytes are complete; times only keep the tree identical to what tar extracted.
        let mut times = FileTimes::new();
        if let Ok(modified) = metadata.modified() {
            times = times.set_modified(modified);
        }
        if let Ok(accessed) = metadata.accessed() {
            times = times.set_accessed(accessed);
        }
        let _ = file.set_times(times);
        Ok(true)
    }

    /// Truncation drops every cached folio of the file, so the writes allocate fresh ones.
    fn write_back(file: &mut File, data: &[u8]) -> io::Result<()> {
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        for chunk in data.chunks(CHUNK) {
            file.write_all(chunk)?;
        }
        if file.metadata()?.len() != data.len() as u64 {
            return Err(io::Error::other("length changed while rewriting"));
        }
        Ok(())
    }
}

/// Ids become directory names, so only a conservative, non-hidden charset is allowed.
fn validate_id(id: &str) -> Result<(), InstallError> {
    let first_ok = id.chars().next().is_some_and(|ch| ch.is_ascii_alphanumeric());
    let rest_ok = id.chars().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'));
    if id.len() > MAX_ID_LEN || !first_ok || !rest_ok {
        return Err(InstallError::new(format!(
            "manifest id {id:?} is not allowed: use 1 to {MAX_ID_LEN} characters from A-Z a-z 0-9 . _ - and start with a letter or digit"
        )));
    }
    Ok(())
}

/// Variant names appear in state.json and in release asset names, so they get a plain lowercase charset.
fn validate_variant(variant: &str) -> Result<(), InstallError> {
    let first_ok = variant.chars().next().is_some_and(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit());
    let rest_ok = variant.chars().all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '_' | '-'));
    if variant.len() > MAX_VARIANT_LEN || !first_ok || !rest_ok {
        return Err(InstallError::new(format!(
            "variant {variant:?} is not allowed: use 1 to {MAX_VARIANT_LEN} characters from a-z 0-9 _ - and start with a letter or digit"
        )));
    }
    Ok(())
}

fn read_manifest(tree: &Path) -> Result<Manifest, InstallError> {
    let file = File::open(tree.join("manifest.json")).map_err(|err| InstallError::new(format!("archive has no manifest.json ({err})")))?;
    let mut bytes = Vec::new();
    file.take(MAX_MANIFEST_BYTES + 1).read_to_end(&mut bytes).map_err(io_context("could not read manifest.json"))?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(InstallError::new("manifest.json is too large"));
    }
    let text = String::from_utf8(bytes).map_err(|_| InstallError::new("manifest.json is not UTF-8"))?;
    let members = parse_object(&text).map_err(|why| InstallError::new(format!("manifest.json is not a valid JSON object ({why})")))?;
    let mut ids = members.iter().filter(|(key, _)| key == "id");
    let Some((_, value)) = ids.next() else {
        return Err(InstallError::new("manifest.json has no id"));
    };
    if ids.next().is_some() {
        return Err(InstallError::new("manifest.json has more than one id"));
    }
    let JsonValue::Text(id) = value else {
        return Err(InstallError::new("manifest.json id is not a string"));
    };
    validate_id(id)?;
    let variant = match members.iter().filter(|(key, _)| key == "variant").collect::<Vec<_>>().as_slice() {
        [] | [(_, JsonValue::Null)] => None,
        [(_, JsonValue::Text(variant))] => {
            validate_variant(variant)?;
            Some(variant.clone())
        }
        [_] => return Err(InstallError::new("manifest.json variant is not a string")),
        _ => return Err(InstallError::new("manifest.json has more than one variant")),
    };
    Ok(Manifest { id: id.clone(), variant })
}

/// The variant recorded in the manifest of an installed version, when it can be read.
fn installed_variant(prefix: &Path, id: &str) -> Option<String> {
    read_manifest(&prefix.join("versions").join(id)).ok().and_then(|manifest| manifest.variant)
}

fn require_tree(tree: &Path) -> Result<(), InstallError> {
    for relative in REQUIRED_FILES {
        if !tree.join(relative).is_file() {
            return Err(InstallError::new(format!("archive is missing {relative}")));
        }
    }
    Ok(())
}

/// The `current` symlink decides which version is active.
fn active_version(prefix: &Path) -> Result<Option<String>, InstallError> {
    let link = prefix.join("current");
    let target = match fs::read_link(&link) {
        Ok(target) => target,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(InstallError::new(format!("{} is not a symlink managed by frust-install ({err}); move it aside and retry", link.display())))
        }
    };
    let id = target.strip_prefix("versions").ok().and_then(|rest| rest.to_str()).filter(|id| validate_id(id).is_ok());
    match id {
        Some(id) => Ok(Some(id.to_string())),
        None => Err(InstallError::new(format!("{} points at {}, which is not a managed version; remove it and retry", link.display(), target.display()))),
    }
}

/// Repairs `state.json` after an interrupted swap; the symlink wins.
fn reconcile(prefix: &Path) -> Result<PrefixState, InstallError> {
    let recorded = read_state(prefix)?;
    let active = active_version(prefix)?;
    if recorded.current == active {
        return Ok(recorded);
    }
    let (previous, previous_variant) = if recorded.current.is_some() {
        (recorded.current, recorded.current_variant)
    } else {
        (recorded.previous, recorded.previous_variant)
    };
    let (previous, previous_variant) = if previous == active { (None, None) } else { (previous, previous_variant) };
    // The manifest of the active tree is the authority on its own variant.
    let current_variant = active.as_deref().and_then(|id| installed_variant(prefix, id));
    let repaired = PrefixState { current: active, previous, current_variant, previous_variant };
    write_state(prefix, &repaired)?;
    Ok(repaired)
}

fn read_state(prefix: &Path) -> Result<PrefixState, InstallError> {
    let path = prefix.join("state.json");
    match fs::read_to_string(&path) {
        Ok(text) => parse_state(&text),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            Ok(PrefixState { current: None, previous: None, current_variant: None, previous_variant: None })
        }
        Err(err) => Err(InstallError::new(format!("could not read {}: {err}", path.display()))),
    }
}

fn parse_state(text: &str) -> Result<PrefixState, InstallError> {
    let members = parse_object(text)
        .map_err(|why| InstallError::new(format!("state.json is corrupt ({why}); remove it to reset the recorded versions")))?;
    Ok(PrefixState {
        current: state_field(&members, "current")?,
        previous: state_field(&members, "previous")?,
        current_variant: state_variant_field(&members, "current_variant")?,
        previous_variant: state_variant_field(&members, "previous_variant")?,
    })
}

/// The variant fields were added later, so a state.json without them is valid.
fn state_variant_field(members: &[(String, JsonValue)], field: &str) -> Result<Option<String>, InstallError> {
    match members.iter().find(|(key, _)| key == field).map(|(_, value)| value) {
        None | Some(JsonValue::Null) => Ok(None),
        Some(JsonValue::Text(variant)) => {
            validate_variant(variant).map_err(|_| InstallError::new(format!("state.json field {field} holds an invalid variant; remove state.json to reset it")))?;
            Ok(Some(variant.clone()))
        }
        Some(JsonValue::Other) => Err(InstallError::new(format!("state.json field {field} is not a string; remove state.json to reset it"))),
    }
}

fn state_field(members: &[(String, JsonValue)], field: &str) -> Result<Option<String>, InstallError> {
    match members.iter().find(|(key, _)| key == field).map(|(_, value)| value) {
        None | Some(JsonValue::Null) => Ok(None),
        Some(JsonValue::Text(id)) => {
            validate_id(id).map_err(|_| InstallError::new(format!("state.json field {field} holds an invalid version id; remove state.json to reset it")))?;
            Ok(Some(id.clone()))
        }
        Some(JsonValue::Other) => Err(InstallError::new(format!("state.json field {field} is not a string; remove state.json to reset it"))),
    }
}

fn write_state(prefix: &Path, state: &PrefixState) -> Result<(), InstallError> {
    fs::create_dir_all(prefix).map_err(io_context(format!("could not create {}", prefix.display())))?;
    let body = format!(
        "{{\n  \"current\": {},\n  \"previous\": {},\n  \"current_variant\": {},\n  \"previous_variant\": {}\n}}\n",
        json_opt(state.current.as_deref()),
        json_opt(state.previous.as_deref()),
        json_opt(state.current_variant.as_deref()),
        json_opt(state.previous_variant.as_deref())
    );
    let temporary = prefix.join(format!(".state.json.next.{}", unique_suffix()));
    let commit = || -> io::Result<()> {
        let mut file = OpenOptions::new().write(true).create_new(true).open(&temporary)?;
        file.write_all(body.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, prefix.join("state.json"))
    };
    if let Err(err) = commit() {
        let _ = fs::remove_file(&temporary);
        return Err(InstallError::new(format!("could not write state.json: {err}")));
    }
    sync_dir(prefix);
    Ok(())
}

// Ids and variants are validated to a charset that needs no JSON escaping.
fn json_opt(value: Option<&str>) -> String {
    match value {
        Some(value) => format!("\"{value}\""),
        None => "null".to_string(),
    }
}

fn archive_token(path: &Path) -> String {
    hex32(&sha256(path.to_string_lossy().as_bytes()))[..12].to_string()
}

fn unique_suffix() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_nanos()).unwrap_or(0);
    format!("{:x}-{:x}-{:x}", std::process::id(), nanos, COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn sync_dir(dir: &Path) {
    if let Ok(handle) = File::open(dir) {
        let _ = handle.sync_all();
    }
}

fn remove_path(path: &Path) -> Result<(), InstallError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(InstallError::new(format!("could not inspect {}: {err}", path.display()))),
    };
    let result = if metadata.is_dir() { fs::remove_dir_all(path) } else { fs::remove_file(path) };
    result.map_err(io_context(format!("could not remove {}", path.display())))
}

/// Exclusive for the whole operation; released when the guard drops.
struct PrefixLock {
    _file: File,
}

fn lock_prefix(prefix: &Path, create: bool) -> Result<PrefixLock, InstallError> {
    if create {
        fs::create_dir_all(prefix).map_err(io_context(format!("could not create prefix {}", prefix.display())))?;
    } else {
        match fs::metadata(prefix) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return Err(InstallError::new(format!("prefix {} is not a directory", prefix.display()))),
            Err(err) => return Err(InstallError::new(format!("prefix {} is not usable: {err}", prefix.display()))),
        }
    }
    let path = prefix.join(LOCK_NAME);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(io_context(format!("could not open {} (the prefix must be writable by the current user)", path.display())))?;
    file.lock().map_err(io_context(format!("could not lock {}", path.display())))?;
    Ok(PrefixLock { _file: file })
}

fn clear_leftovers(prefix: &Path) -> Result<(), InstallError> {
    clear_temporaries(prefix)?;
    let versions = prefix.join("versions");
    let entries = match fs::read_dir(&versions) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(InstallError::new(format!("could not list {}: {err}", versions.display()))),
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if let Some(rest) = name.strip_prefix(ASIDE_PREFIX) {
            // An interrupted same-id swap: put the old tree back if the new one never landed.
            let restore = rest.split_once('.').map(|(_, id)| id).filter(|id| validate_id(id).is_ok());
            if let Some(id) = restore {
                let target = versions.join(id);
                if target.symlink_metadata().is_err() && fs::rename(&path, &target).is_ok() {
                    continue;
                }
            }
            remove_path(&path)?;
        } else if name.starts_with(WORK_PREFIX) {
            remove_path(&path)?;
        }
    }
    Ok(())
}

fn clear_temporaries(prefix: &Path) -> Result<(), InstallError> {
    let entries = match fs::read_dir(prefix) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(InstallError::new(format!("could not list {}: {err}", prefix.display()))),
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(".current-next") || name.starts_with(".state.json.next") {
            remove_path(&entry.path())?;
        }
    }
    Ok(())
}

struct Sha256 {
    state: [u32; 8],
    buffer: [u8; 64],
    filled: usize,
    length: u64,
}

impl Sha256 {
    fn new() -> Self {
        Self {
            state: [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19],
            buffer: [0; 64],
            filled: 0,
            length: 0,
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        self.length = self.length.wrapping_add(data.len() as u64);
        if self.filled > 0 {
            let take = (64 - self.filled).min(data.len());
            self.buffer[self.filled..self.filled + take].copy_from_slice(&data[..take]);
            self.filled += take;
            data = &data[take..];
            if self.filled < 64 {
                return;
            }
            let block = self.buffer;
            compress(&mut self.state, &block);
            self.filled = 0;
        }
        while data.len() >= 64 {
            compress(&mut self.state, data[..64].try_into().unwrap());
            data = &data[64..];
        }
        self.buffer[..data.len()].copy_from_slice(data);
        self.filled = data.len();
    }

    fn finish(mut self) -> [u8; 32] {
        let bits = self.length.wrapping_mul(8);
        let mut block = self.buffer;
        block[self.filled] = 0x80;
        for byte in &mut block[self.filled + 1..] {
            *byte = 0;
        }
        if self.filled >= 56 {
            compress(&mut self.state, &block);
            block = [0; 64];
        }
        block[56..].copy_from_slice(&bits.to_be_bytes());
        compress(&mut self.state, &block);
        let mut out = [0u8; 32];
        for (index, word) in self.state.iter().enumerate() {
            out[index * 4..][..4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}

fn compress(state: &mut [u32; 8], chunk: &[u8; 64]) {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be,
        0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa,
        0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85,
        0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3,
        0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f,
        0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut w = [0u32; 64];
    for index in 0..16 {
        w[index] = u32::from_be_bytes(chunk[index * 4..][..4].try_into().unwrap());
    }
    for index in 16..64 {
        let s0 = w[index - 15].rotate_right(7) ^ w[index - 15].rotate_right(18) ^ (w[index - 15] >> 3);
        let s1 = w[index - 2].rotate_right(17) ^ w[index - 2].rotate_right(19) ^ (w[index - 2] >> 10);
        w[index] = w[index - 16].wrapping_add(s0).wrapping_add(w[index - 7]).wrapping_add(s1);
    }
    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
    for index in 0..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ (!e & g);
        let temp1 = h.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[index]).wrapping_add(w[index]);
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
    for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
        *slot = slot.wrapping_add(value);
    }
}

#[derive(Debug, PartialEq)]
enum JsonValue {
    Text(String),
    Null,
    Other,
}

/// Parses a top-level JSON object into its members; nested values are validated and dropped.
fn parse_object(text: &str) -> Result<Vec<(String, JsonValue)>, &'static str> {
    let bytes = text.as_bytes();
    let mut index = 0;
    skip_space(bytes, &mut index);
    if bytes.get(index) != Some(&b'{') {
        return Err("expected an object");
    }
    index += 1;
    let members = parse_members(bytes, &mut index, 0)?;
    skip_space(bytes, &mut index);
    if index != bytes.len() {
        return Err("unexpected trailing characters");
    }
    Ok(members)
}

fn skip_space(bytes: &[u8], index: &mut usize) {
    while matches!(bytes.get(*index), Some(b' ' | b'\t' | b'\n' | b'\r')) {
        *index += 1;
    }
}

fn parse_members(bytes: &[u8], index: &mut usize, depth: usize) -> Result<Vec<(String, JsonValue)>, &'static str> {
    let mut members = Vec::new();
    skip_space(bytes, index);
    if bytes.get(*index) == Some(&b'}') {
        *index += 1;
        return Ok(members);
    }
    loop {
        skip_space(bytes, index);
        if bytes.get(*index) != Some(&b'"') {
            return Err("expected a member name");
        }
        *index += 1;
        let (key, used) = decode_json_string_body(&bytes[*index..])?;
        *index += used;
        skip_space(bytes, index);
        if bytes.get(*index) != Some(&b':') {
            return Err("expected ':' after a member name");
        }
        *index += 1;
        skip_space(bytes, index);
        let value = parse_value(bytes, index, depth)?;
        members.push((key, value));
        skip_space(bytes, index);
        match bytes.get(*index) {
            Some(b',') => *index += 1,
            Some(b'}') => {
                *index += 1;
                return Ok(members);
            }
            _ => return Err("expected ',' or '}'"),
        }
    }
}

fn parse_value(bytes: &[u8], index: &mut usize, depth: usize) -> Result<JsonValue, &'static str> {
    if depth > MAX_JSON_DEPTH {
        return Err("nested too deeply");
    }
    match bytes.get(*index) {
        Some(b'"') => {
            *index += 1;
            let (text, used) = decode_json_string_body(&bytes[*index..])?;
            *index += used;
            Ok(JsonValue::Text(text))
        }
        Some(b'{') => {
            *index += 1;
            parse_members(bytes, index, depth + 1)?;
            Ok(JsonValue::Other)
        }
        Some(b'[') => {
            *index += 1;
            skip_space(bytes, index);
            if bytes.get(*index) == Some(&b']') {
                *index += 1;
                return Ok(JsonValue::Other);
            }
            loop {
                skip_space(bytes, index);
                parse_value(bytes, index, depth + 1)?;
                skip_space(bytes, index);
                match bytes.get(*index) {
                    Some(b',') => *index += 1,
                    Some(b']') => {
                        *index += 1;
                        return Ok(JsonValue::Other);
                    }
                    _ => return Err("expected ',' or ']'"),
                }
            }
        }
        Some(b'n') if take_literal(bytes, index, "null") => Ok(JsonValue::Null),
        Some(b't') if take_literal(bytes, index, "true") => Ok(JsonValue::Other),
        Some(b'f') if take_literal(bytes, index, "false") => Ok(JsonValue::Other),
        Some(b'-' | b'0'..=b'9') => {
            while matches!(bytes.get(*index), Some(b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')) {
                *index += 1;
            }
            Ok(JsonValue::Other)
        }
        _ => Err("unexpected character in a value"),
    }
}

fn take_literal(bytes: &[u8], index: &mut usize, word: &str) -> bool {
    if bytes[*index..].starts_with(word.as_bytes()) {
        *index += word.len();
        true
    } else {
        false
    }
}

/// Header-level validation of the verified archive, before anything is extracted.
mod tar_check {
    use super::*;

    const BLOCK: usize = 512;
    const MAX_META_BYTES: u64 = 1 << 20;

    #[derive(Default)]
    struct Pax {
        path: Option<String>,
        linkpath: Option<String>,
        size: Option<u64>,
    }

    pub fn validate(archive: &Path) -> Result<(), InstallError> {
        let mut head = [0u8; 6];
        let mut probe = File::open(archive).map_err(io_context("could not read the staged archive"))?;
        let got = read_up_to(&mut probe, &mut head).map_err(io_context("could not read the staged archive"))?;
        let tool = match &head[..got] {
            [0x1f, 0x8b, ..] => Some("gzip"),
            [b'B', b'Z', b'h', ..] => Some("bzip2"),
            [0xfd, b'7', b'z', b'X', b'Z', 0] => Some("xz"),
            [0x28, 0xb5, 0x2f, 0xfd, ..] => Some("zstd"),
            _ => None,
        };
        // Reopen so the scan starts at byte zero.
        let mut source = File::open(archive).map_err(io_context("could not read the staged archive"))?;
        drop(probe);
        let Some(tool) = tool else {
            return scan(&mut source);
        };
        let mut child = Command::new(tool)
            .arg("-dc")
            .stdin(Stdio::from(source))
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(io_context(format!("could not run {tool}")))?;
        let mut stream = child.stdout.take().expect("child stdout is piped");
        let scanned = scan(&mut stream);
        if scanned.is_err() {
            let _ = child.kill();
        } else {
            let _ = io::copy(&mut stream, &mut io::sink());
        }
        drop(stream);
        let status = child.wait().map_err(io_context(format!("could not wait for {tool}")))?;
        scanned?;
        if !status.success() {
            return Err(InstallError::new(format!("{tool} could not decompress the archive")));
        }
        Ok(())
    }

    fn read_up_to(file: &mut File, buffer: &mut [u8]) -> io::Result<usize> {
        let mut total = 0;
        while total < buffer.len() {
            match file.read(&mut buffer[total..]) {
                Ok(0) => break,
                Ok(count) => total += count,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) => return Err(err),
            }
        }
        Ok(total)
    }

    fn scan(reader: &mut dyn Read) -> Result<(), InstallError> {
        let mut symlinks: HashSet<String> = HashSet::new();
        let mut long_name: Option<String> = None;
        let mut long_link: Option<String> = None;
        let mut pax = Pax::default();
        let mut block = [0u8; BLOCK];
        loop {
            read_block(reader, &mut block)?;
            if block.iter().all(|byte| *byte == 0) {
                read_block(reader, &mut block)?;
                if block.iter().all(|byte| *byte == 0) {
                    return Ok(());
                }
                return Err(InstallError::new("archive has a lone end-of-archive block followed by more data"));
            }
            verify_checksum_field(&block)?;
            let kind = block[156];
            let header_size = parse_number(&block[124..136]).ok_or_else(|| InstallError::new("archive entry has an unreadable size"))?;
            if matches!(kind, b'L' | b'K' | b'x' | b'g') {
                let data = read_meta(reader, header_size)?;
                match kind {
                    b'L' => long_name = Some(c_string(&data)),
                    b'K' => long_link = Some(c_string(&data)),
                    b'x' => pax = parse_pax(&data).map_err(|why| InstallError::new(format!("archive has a bad extended header ({why})")))?,
                    _ => {
                        let global = parse_pax(&data).map_err(|why| InstallError::new(format!("archive has a bad global header ({why})")))?;
                        if global.path.is_some() || global.linkpath.is_some() || global.size.is_some() {
                            return Err(InstallError::new("archive global headers that set paths or sizes are not supported"));
                        }
                    }
                }
                continue;
            }
            let entry_pax = std::mem::take(&mut pax);
            let name = entry_pax.path.or(long_name.take()).unwrap_or_else(|| header_name(&block));
            let link = entry_pax.linkpath.or(long_link.take()).unwrap_or_else(|| header_link(&block));
            let size = entry_pax.size.unwrap_or(header_size);
            let parts = checked_parts(&name, &symlinks)?;
            if symlinks.contains(&parts.join("/")) {
                return Err(unsafe_entry(&name, "redefines a symbolic link"));
            }
            match kind {
                b'0' | 0 | b'7' => skip_entry_data(reader, size)?,
                b'5' => {}
                b'1' => {
                    let target = checked_parts(&link, &symlinks).map_err(|_| unsafe_entry(&name, "is a hard link to a path outside the archive tree"))?;
                    if target.is_empty() {
                        return Err(unsafe_entry(&name, "is a hard link to the archive root"));
                    }
                }
                b'2' => {
                    if parts.is_empty() {
                        return Err(unsafe_entry(&name, "is a symbolic link with no name"));
                    }
                    check_symlink_target(&link, parts.len() - 1).map_err(|why| unsafe_entry(&name, why))?;
                    symlinks.insert(parts.join("/"));
                }
                other => return Err(unsafe_entry(&name, &format!("has unsupported entry type {:?}", other as char))),
            }
        }
    }

    fn unsafe_entry(name: &str, why: &str) -> InstallError {
        InstallError::new(format!("archive entry {name:?} is rejected: {why}"))
    }

    /// Normalized components of an entry path; rejects absolute paths, `..`, and paths through earlier symlinks.
    fn checked_parts(path: &str, symlinks: &HashSet<String>) -> Result<Vec<String>, InstallError> {
        if path.contains('\0') || path.starts_with('/') {
            return Err(unsafe_entry(path, "has an absolute path"));
        }
        let mut parts: Vec<String> = Vec::new();
        for component in path.split('/') {
            match component {
                "" | "." => {}
                ".." => return Err(unsafe_entry(path, "contains a '..' component")),
                name => {
                    if symlinks.contains(&parts.join("/")) && !parts.is_empty() {
                        return Err(unsafe_entry(path, "passes through a symbolic link"));
                    }
                    parts.push(name.to_string());
                }
            }
        }
        Ok(parts)
    }

    /// Relative targets may climb only with a leading run of `..` that stays inside the tree.
    pub(super) fn check_symlink_target(target: &str, depth: usize) -> Result<(), &'static str> {
        if target.is_empty() || target.contains('\0') || target.starts_with('/') {
            return Err("has an empty or absolute symbolic link target");
        }
        let mut climbs = 0;
        let mut named = false;
        for component in target.split('/') {
            match component {
                "" | "." => {}
                ".." if named => return Err("has a symbolic link target that climbs after descending"),
                ".." => climbs += 1,
                _ => named = true,
            }
        }
        if climbs > depth {
            return Err("has a symbolic link target that escapes the archive tree");
        }
        Ok(())
    }

    fn read_block(reader: &mut dyn Read, block: &mut [u8; BLOCK]) -> Result<(), InstallError> {
        reader.read_exact(block).map_err(read_error)
    }

    fn read_error(err: io::Error) -> InstallError {
        if err.kind() == io::ErrorKind::UnexpectedEof {
            InstallError::new("archive is truncated")
        } else {
            InstallError::new(format!("could not read the staged archive: {err}"))
        }
    }

    fn padding(size: u64) -> u64 {
        (BLOCK as u64 - size % BLOCK as u64) % BLOCK as u64
    }

    fn read_meta(reader: &mut dyn Read, size: u64) -> Result<Vec<u8>, InstallError> {
        if size > MAX_META_BYTES {
            return Err(InstallError::new("archive extended header is too large"));
        }
        let mut data = vec![0u8; size as usize];
        reader.read_exact(&mut data).map_err(read_error)?;
        skip(reader, padding(size))?;
        Ok(data)
    }

    fn skip_entry_data(reader: &mut dyn Read, size: u64) -> Result<(), InstallError> {
        let total = size.checked_add(padding(size)).ok_or_else(|| InstallError::new("archive entry size overflows"))?;
        skip(reader, total)
    }

    fn skip(reader: &mut dyn Read, mut remaining: u64) -> Result<(), InstallError> {
        let mut scratch = [0u8; 8192];
        while remaining > 0 {
            let chunk = remaining.min(scratch.len() as u64) as usize;
            reader.read_exact(&mut scratch[..chunk]).map_err(read_error)?;
            remaining -= chunk as u64;
        }
        Ok(())
    }

    fn verify_checksum_field(block: &[u8; BLOCK]) -> Result<(), InstallError> {
        let stored = parse_number(&block[148..156]).ok_or_else(|| InstallError::new("archive header has an unreadable checksum"))?;
        let mut unsigned: u64 = 0;
        let mut signed: i64 = 0;
        for (index, byte) in block.iter().enumerate() {
            let value = if (148..156).contains(&index) { b' ' } else { *byte };
            unsigned += u64::from(value);
            signed += i64::from(value as i8);
        }
        if stored == unsigned || i64::try_from(stored).is_ok_and(|stored| stored == signed) {
            Ok(())
        } else {
            Err(InstallError::new("archive header checksum does not match; the archive is corrupt"))
        }
    }

    /// Octal text or GNU base-256; empty fields read as zero.
    fn parse_number(field: &[u8]) -> Option<u64> {
        if field[0] & 0x80 != 0 {
            if field[0] == 0xff {
                return None;
            }
            let mut value = u64::from(field[0] & 0x7f);
            for byte in &field[1..] {
                value = value.checked_mul(256)?.checked_add(u64::from(*byte))?;
            }
            return Some(value);
        }
        let mut value = 0u64;
        for byte in field.iter().copied().skip_while(|byte| *byte == b' ') {
            match byte {
                b'0'..=b'7' => value = value.checked_mul(8)?.checked_add(u64::from(byte - b'0'))?,
                0 | b' ' => break,
                _ => return None,
            }
        }
        Some(value)
    }

    fn c_string(bytes: &[u8]) -> String {
        let end = bytes.iter().position(|byte| *byte == 0).unwrap_or(bytes.len());
        String::from_utf8_lossy(&bytes[..end]).into_owned()
    }

    fn header_name(block: &[u8; BLOCK]) -> String {
        let name = c_string(&block[0..100]);
        let prefix = c_string(&block[345..500]);
        // Only POSIX ustar headers use the prefix field; GNU headers reuse that space.
        if &block[257..263] == b"ustar\0" && !prefix.is_empty() {
            format!("{prefix}/{name}")
        } else {
            name
        }
    }

    fn header_link(block: &[u8; BLOCK]) -> String {
        c_string(&block[157..257])
    }

    fn parse_pax(data: &[u8]) -> Result<Pax, &'static str> {
        let mut pax = Pax::default();
        let mut rest = data;
        while !rest.is_empty() {
            let space = rest.iter().position(|byte| *byte == b' ').ok_or("record has no length")?;
            let length: usize = std::str::from_utf8(&rest[..space]).ok().and_then(|text| text.parse().ok()).ok_or("record length is not a number")?;
            if length <= space + 1 || length > rest.len() {
                return Err("record length is out of range");
            }
            let record = rest[space + 1..length].strip_suffix(b"\n").ok_or("record is not newline terminated")?;
            let equals = record.iter().position(|byte| *byte == b'=').ok_or("record has no '='")?;
            let (key, value) = (&record[..equals], &record[equals + 1..]);
            match key {
                b"path" => pax.path = Some(String::from_utf8_lossy(value).into_owned()),
                b"linkpath" => pax.linkpath = Some(String::from_utf8_lossy(value).into_owned()),
                b"size" => {
                    let size = std::str::from_utf8(value).ok().and_then(|text| text.parse().ok()).ok_or("size is not a number")?;
                    pax.size = Some(size);
                }
                _ if key.starts_with(b"GNU.sparse") => return Err("sparse files are not supported"),
                _ => {}
            }
            rest = &rest[length..];
        }
        Ok(pax)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        let path = std::env::temp_dir().join(format!("frust-install-{}", unique_suffix()));
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
        let status = Command::new("tar").arg("-c").arg("-f").arg(&archive).arg("-C").arg(&root).arg(".").status().unwrap();
        assert!(status.success());
        let bytes = fs::read(&archive).unwrap();
        let sum = dir.join(format!("{id}.sha256"));
        fs::write(&sum, format!("{}\n", hex32(&sha256(&bytes)))).unwrap();
        (archive, sum)
    }

    #[test]
    fn streaming_digest_matches_the_one_shot_digest_at_block_edges() {
        for length in [0usize, 1, 55, 56, 63, 64, 65, 119, 120, 127, 128, 1000, 4096] {
            let data: Vec<u8> = (0..length).map(|index| (index * 31 % 251) as u8).collect();
            let mut hasher = Sha256::new();
            for piece in data.chunks(37) {
                hasher.update(piece);
            }
            assert_eq!(hasher.finish(), sha256(&data), "length {length}");
        }
    }

    #[test]
    fn id_validation_accepts_only_plain_names() {
        for good in ["a", "frust-0ef1bc386-central1", "1.0_x", "A"] {
            assert!(validate_id(good).is_ok(), "{good}");
        }
        let long = "a".repeat(MAX_ID_LEN + 1);
        for bad in ["", ".", "..", ".hidden", ".partial-x", "a/b", "../x", "-x", "a b", long.as_str()] {
            assert!(validate_id(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn variant_validation_accepts_only_plain_lowercase_names() {
        for good in ["baseline", "v3", "x86-64_v4", "0"] {
            assert!(validate_variant(good).is_ok(), "{good}");
        }
        let long = "a".repeat(MAX_VARIANT_LEN + 1);
        for bad in ["", "V3", "-v3", "_v3", "v 3", "v3/x", "../v3", "v3\"", long.as_str()] {
            assert!(validate_variant(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn manifests_name_at_most_one_plain_variant() {
        let dir = scratch();
        let tree = dir.join("tree");
        fs::create_dir_all(&tree).unwrap();
        let cases = [
            ("{\"id\":\"a\"}", Some(None)),
            ("{\"id\":\"a\",\"variant\":null}", Some(None)),
            ("{\"id\":\"a\",\"variant\":\"v3\"}", Some(Some("v3"))),
            ("{\"variant\":\"baseline\",\"id\":\"a\"}", Some(Some("baseline"))),
            ("{\"id\":\"a\",\"variant\":3}", None),
            ("{\"id\":\"a\",\"variant\":\"V3\"}", None),
            ("{\"id\":\"a\",\"variant\":\"v3\",\"variant\":\"baseline\"}", None),
        ];
        for (text, expected) in cases {
            fs::write(tree.join("manifest.json"), text).unwrap();
            let read = read_manifest(&tree);
            match expected {
                Some(variant) => assert_eq!(read.unwrap().variant.as_deref(), variant, "{text}"),
                None => assert!(read.is_err(), "{text}"),
            }
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn json_objects_tolerate_whitespace_and_nested_values() {
        let members = parse_object(" {\n \"a\" : [1, {\"b\": null}] ,\t\"id\"\n:\n\"x\" , \"n\": -1.5e3, \"t\": true }\n").unwrap();
        assert_eq!(members.iter().find(|(key, _)| key == "id").map(|(_, value)| value), Some(&JsonValue::Text("x".to_string())));
        assert!(parse_object("{\"id\": }").is_err());
        assert!(parse_object("{\"id\": \"x\"} trailing").is_err());
        assert!(parse_object("[]").is_err());
        assert!(parse_object("").is_err());
    }

    #[test]
    fn symlink_targets_may_climb_only_inside_the_tree() {
        assert!(tar_check::check_symlink_target("rustc", 1).is_ok());
        assert!(tar_check::check_symlink_target("../bin/rustc", 1).is_ok());
        assert!(tar_check::check_symlink_target("../../x", 1).is_err());
        assert!(tar_check::check_symlink_target("a/../../x", 3).is_err());
        assert!(tar_check::check_symlink_target("/etc", 1).is_err());
        assert!(tar_check::check_symlink_target("", 1).is_err());
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

    fn pattern(length: usize, seed: usize) -> Vec<u8> {
        (0..length).map(|index| ((index * 131 + seed) % 251) as u8).collect()
    }

    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
    }

    fn inode_of(path: &Path) -> u64 {
        use std::os::unix::fs::MetadataExt;
        fs::symlink_metadata(path).unwrap().ino()
    }

    #[test]
    fn large_files_are_rewritten_in_place_keeping_bytes_links_modes_and_times() {
        let dir = scratch();
        let tree = dir.join("tree");
        fs::create_dir_all(tree.join("lib/nested")).unwrap();
        let min = relayout::MIN_BYTES as usize;
        let past = UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
        let files: [(&str, usize, u32); 5] = [
            ("lib/big.so", 2 * min + 12_345, 0o751),
            ("lib/readonly.so", min + 1, 0o444),
            ("lib/nested/exact.rlib", min, 0o644),
            ("lib/below.so", min - 1, 0o755),
            ("bin/small", 100, 0o640),
        ];
        for (index, (name, length, mode)) in files.iter().enumerate() {
            let path = tree.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, pattern(*length, index)).unwrap();
            File::options().write(true).open(&path).unwrap().set_times(fs::FileTimes::new().set_modified(past)).unwrap();
            set_mode(&path, *mode);
        }
        fs::hard_link(tree.join("lib/big.so"), tree.join("lib/big-link.so")).unwrap();
        std::os::unix::fs::symlink("big.so", tree.join("lib/alias.so")).unwrap();
        let inodes: Vec<u64> = files.iter().map(|(name, _, _)| inode_of(&tree.join(name))).collect();

        // big.so (once for both of its names), readonly.so and exact.rlib; the rest is too small.
        assert_eq!(relayout::rewrite_large_files(&tree).unwrap(), 3);

        for (index, (name, length, mode)) in files.iter().enumerate() {
            let path = tree.join(name);
            assert_eq!(fs::read(&path).unwrap(), pattern(*length, index), "{name}");
            assert_eq!(mode_of(&path), *mode, "{name}");
            assert_eq!(inode_of(&path), inodes[index], "{name}");
            assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), past, "{name}");
        }
        assert_eq!(inode_of(&tree.join("lib/big-link.so")), inodes[0]);
        assert_eq!(fs::read_link(tree.join("lib/alias.so")).unwrap(), Path::new("big.so"));
        assert!(fs::symlink_metadata(tree.join("lib/alias.so")).unwrap().file_type().is_symlink());
        let _ = fs::remove_dir_all(&dir);
    }

    /// A toolchain archive with a file large enough to be rewritten, a hard link and a symlink to it.
    fn large_archive(dir: &Path, id: &str) -> (PathBuf, PathBuf, Vec<u8>) {
        let (small, _) = archive(dir, id, "marker");
        let root = dir.join(format!("tree-{id}"));
        fs::remove_file(&small).unwrap();
        let big = pattern(relayout::MIN_BYTES as usize * 3 / 2 + 7, 3);
        fs::create_dir_all(root.join("lib")).unwrap();
        fs::write(root.join("lib/libbig.so"), &big).unwrap();
        set_mode(&root.join("lib/libbig.so"), 0o755);
        fs::hard_link(root.join("lib/libbig.so"), root.join("lib/libbig-link.so")).unwrap();
        std::os::unix::fs::symlink("libbig.so", root.join("lib/libbig.so.1")).unwrap();
        let archive = dir.join(format!("{id}.tar"));
        let status = Command::new("tar").arg("-c").arg("-f").arg(&archive).arg("-C").arg(&root).arg(".").status().unwrap();
        assert!(status.success());
        let sum = dir.join(format!("{id}.sha256"));
        fs::write(&sum, format!("{}\n", hex32(&sha256(&fs::read(&archive).unwrap())))).unwrap();
        (archive, sum, big)
    }

    #[test]
    fn installed_large_files_keep_bytes_modes_hard_links_and_symlinks() {
        let dir = scratch();
        let prefix = dir.join("prefix");
        let (archive_path, sum, big) = large_archive(&dir, "large");
        install(&prefix, &archive_path, &sum).unwrap();
        let lib = prefix.join("current/lib");
        assert_eq!(fs::read(lib.join("libbig.so")).unwrap(), big);
        assert_eq!(mode_of(&lib.join("libbig.so")), 0o755);
        assert_eq!(inode_of(&lib.join("libbig-link.so")), inode_of(&lib.join("libbig.so")));
        assert_eq!(fs::read_link(lib.join("libbig.so.1")).unwrap(), Path::new("libbig.so"));
        assert_eq!(fs::read_to_string(prefix.join("current/bin/rustc")).unwrap(), "marker");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn truncated_archive_with_a_large_file_is_rejected() {
        let dir = scratch();
        let prefix = dir.join("prefix");
        let (archive_path, sum, _) = large_archive(&dir, "cut");
        let bytes = fs::read(&archive_path).unwrap();
        fs::write(&archive_path, &bytes[..bytes.len() / 2]).unwrap();
        fs::write(&sum, format!("{}\n", hex32(&sha256(&bytes[..bytes.len() / 2])))).unwrap();
        let err = install(&prefix, &archive_path, &sum).unwrap_err();
        assert!(err.message.contains("truncated"), "{}", err.message);
        assert!(!prefix.join("current").exists());
        assert!(!prefix.join("versions/cut").exists());
        let _ = fs::remove_dir_all(&dir);
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
}
