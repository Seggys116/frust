//! Extra targets and components for an installed toolchain, taken from the official Rust release channel.
//!
//! The fRust compiler reports the same `rustc -vV` as the official release it is built from, so the
//! official `rust-std` packages for other targets, `rust-docs`, `rust-src` and the other
//! compiler-independent components work with it unchanged. Clippy, rustfmt and rust-analyzer (with its
//! proc-macro server) link the compiler's private crates, so the official ones cannot start next to the
//! fRust compiler; they are built from source into every fRust archive and are never added from here.
//!
//! Everything installed here comes from `<dist>/channel-rust-<release>.toml`, where `<release>` is the
//! Rust release of the installed toolchain and `<dist>` is `https://static.rust-lang.org/dist` or
//! `$FRUST_DIST_BASE`. The channel manifest is checked against its published sha-256, the package
//! archive against the sha-256 listed in the manifest, and the archive headers are validated before
//! anything is extracted, exactly as for a toolchain archive. A package is unpacked in a private
//! work directory under `versions/`, then its files are renamed into the active version in one step
//! that is undone if any part fails. An existing file is never overwritten: the host target's std
//! stays the one fRust built.
//!
//! Each `component add` and `target add` is remembered per prefix in [`EXTRAS_FILE`], so that
//! [`reapply`] can put the same extras into a newer toolchain after an update.

use std::collections::HashMap;
use std::env;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::install::{self, InstallError, Work};
use crate::release::{self, Downloads};

/// Replaces the official download location, for mirrors and for tests; any scheme curl accepts works.
pub const DIST_BASE_ENV: &str = "FRUST_DIST_BASE";
pub const OFFICIAL_DIST: &str = "https://static.rust-lang.org/dist";
/// The user's extra targets and components, one `target <triple>` or `component <name>` per line.
pub const EXTRAS_FILE: &str = "extras.list";

const MAX_CHANNEL_BYTES: u64 = 64 << 20;
const MAX_SMALL_FILE_BYTES: u64 = 1 << 20;
const MAX_NAME_LEN: usize = 96;

fn error(message: impl Into<String>) -> InstallError {
    InstallError::new(message)
}

/// Where official packages come from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dist {
    base: String,
    overridden: bool,
}

impl Dist {
    pub fn official() -> Self {
        Self { base: OFFICIAL_DIST.to_string(), overridden: false }
    }

    /// A mirror or a local directory; unlike the official location it may use any URL scheme.
    pub fn with_base(base: &str) -> Self {
        Self { base: base.trim_end_matches('/').to_string(), overridden: true }
    }

    /// `$FRUST_DIST_BASE` when set and not empty, the official location otherwise.
    pub fn from_environment() -> Self {
        match env::var(DIST_BASE_ENV) {
            Ok(base) if !base.is_empty() => Self::with_base(&base),
            _ => Self::official(),
        }
    }

    /// The location a URL listed in the channel manifest is downloaded from.
    fn resolve(&self, listed: &str) -> Result<String, InstallError> {
        if !self.overridden {
            return if listed.starts_with("https://static.rust-lang.org/") {
                Ok(listed.to_string())
            } else {
                Err(error(format!("the channel manifest lists {listed}, which is not on static.rust-lang.org")))
            };
        }
        Ok(match listed.strip_prefix(OFFICIAL_DIST) {
            Some(rest) if rest.starts_with('/') => format!("{}{rest}", self.base),
            _ => listed.to_string(),
        })
    }

    fn fetch(&self, url: &str, dest: &Path) -> Result<(), InstallError> {
        release::fetch(url, dest, !self.overridden)
    }
}

/// One downloadable package of the channel manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Package {
    pub available: bool,
    pub url: String,
    pub hash: String,
}

/// The parts of `channel-rust-<release>.toml` that the installer reads.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Channel {
    packages: HashMap<(String, String), Package>,
    commits: HashMap<String, String>,
}

impl Channel {
    /// Reads the `[pkg.<name>.target.<target>]` tables and the `git_commit_hash` of each `[pkg.<name>]`.
    /// The manifest is a small, regular subset of TOML: tables, and `key = "string"` or `key = bool` lines.
    pub fn parse(text: &str) -> Channel {
        let mut channel = Channel::default();
        // The table being read: package name and, for a target table, its target.
        let mut current: Option<(String, Option<String>)> = None;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(header) = line.strip_prefix('[') {
                current = header.strip_suffix(']').filter(|inner| !inner.starts_with('[')).and_then(parse_header);
                if let Some((package, Some(target))) = &current {
                    channel.packages.entry((package.clone(), target.clone())).or_insert(Package { available: false, url: String::new(), hash: String::new() });
                }
                continue;
            }
            let (Some((package, target)), Some((key, value))) = (&current, line.split_once('=')) else { continue };
            let (key, value) = (key.trim(), value.trim());
            match target {
                Some(target) => {
                    if let Some(entry) = channel.packages.get_mut(&(package.clone(), target.clone())) {
                        match key {
                            "available" => entry.available = value == "true",
                            "url" => entry.url = unquote(value).unwrap_or_default(),
                            "hash" => entry.hash = unquote(value).unwrap_or_default(),
                            _ => {}
                        }
                    }
                }
                None => {
                    if key == "git_commit_hash" {
                        if let Some(commit) = unquote(value) {
                            channel.commits.insert(package.clone(), commit);
                        }
                    }
                }
            }
        }
        channel
    }

    pub fn package(&self, package: &str, target: &str) -> Option<&Package> {
        self.packages.get(&(package.to_string(), target.to_string()))
    }

    pub fn commit(&self, package: &str) -> Option<&str> {
        self.commits.get(package).map(String::as_str)
    }

    /// The targets with an available package of this name, sorted.
    pub fn targets_of(&self, package: &str) -> Vec<String> {
        let mut targets: Vec<String> =
            self.packages.iter().filter(|((name, _), entry)| name == package && entry.available).map(|((_, target), _)| target.clone()).collect();
        targets.sort();
        targets
    }
}

fn unquote(value: &str) -> Option<String> {
    let inner = value.strip_prefix('"')?.strip_suffix('"')?;
    (!inner.contains(['"', '\\'])).then(|| inner.to_string())
}

/// `pkg.<name>` and `pkg.<name>.target.<target>` tables; the target may be quoted (`"*"`).
fn parse_header(inner: &str) -> Option<(String, Option<String>)> {
    let rest = inner.trim().strip_prefix("pkg.")?;
    match rest.split_once(".target.") {
        Some((package, target)) => {
            let target = target.trim().trim_matches('"');
            (!package.is_empty() && !target.is_empty()).then(|| (package.to_string(), Some(target.to_string())))
        }
        None => (!rest.is_empty() && !rest.contains('.')).then(|| (rest.to_string(), None)),
    }
}

/// An installed toolchain, as far as the extras need to know it.
#[derive(Clone, Debug)]
struct Toolchain {
    dir: PathBuf,
    id: String,
    host: String,
    release: String,
    commit: Option<String>,
}

/// The active version of the prefix. Host, Rust release and commit come from its `manifest.json`
/// (`host` or `target`, `rust_release`, `rust_commit`); whatever it lacks, the toolchain's own
/// `rustc -vV` answers.
fn toolchain(prefix: &Path) -> Result<Toolchain, InstallError> {
    let Some(id) = install::active_version(prefix)? else {
        return Err(error(format!("no toolchain is installed in {}; run an install first", prefix.display())));
    };
    let dir = prefix.join("versions").join(&id);
    let fields = install::manifest_text_fields(&dir)?;
    let field = |name: &str| fields.iter().find(|(key, _)| key == name).map(|(_, value)| value.clone());
    let mut host = field("host").or_else(|| field("target"));
    let mut release = field("rust_release");
    let mut commit = field("rust_commit");
    if host.is_none() || release.is_none() || commit.is_none() {
        if let Ok(output) = Command::new(dir.join("bin").join("rustc")).arg("-vV").output() {
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                match line.split_once(": ") {
                    Some(("host", value)) if host.is_none() => host = Some(value.trim().to_string()),
                    Some(("release", value)) if release.is_none() => release = Some(value.trim().to_string()),
                    Some(("commit-hash", value)) if commit.is_none() && value.trim() != "unknown" => commit = Some(value.trim().to_string()),
                    _ => {}
                }
            }
        }
    }
    let host = host.ok_or_else(|| error(format!("could not tell the host target of {id}: its manifest.json names none and rustc -vV failed")))?;
    let release = release.ok_or_else(|| error(format!("could not tell the Rust release of {id}: its manifest.json names none and rustc -vV failed")))?;
    validate_name(&host, "host target")?;
    validate_name(&release, "Rust release")?;
    Ok(Toolchain { dir, id, host, release, commit })
}

/// Names end up in URLs and paths, so only a plain charset is allowed.
fn validate_name(name: &str, what: &str) -> Result<(), InstallError> {
    let first_ok = name.chars().next().is_some_and(|ch| ch.is_ascii_alphanumeric());
    if name.len() > MAX_NAME_LEN || !first_ok || !name.chars().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-')) {
        return Err(error(format!("{name:?} is not a valid {what}: use letters, digits, '.', '_' and '-'")));
    }
    Ok(())
}

/// A component of the toolchain that comes from the official channel.
struct ComponentDef {
    /// The name users type.
    name: &'static str,
    /// The package in the channel manifest, and the name of the component inside it.
    package: &'static str,
    /// Whether the package exists once per host target rather than once for all (`*`).
    per_host: bool,
    /// Paths that show the component is there when no manifest records it (a toolchain that bundles it).
    probes: &'static [&'static str],
}

const COMPONENTS: [ComponentDef; 3] = [
    ComponentDef { name: "rust-docs", package: "rust-docs", per_host: true, probes: &["share/doc/rust/html/index.html"] },
    ComponentDef { name: "rust-src", package: "rust-src", per_host: false, probes: &["lib/rustlib/src/rust/library"] },
    ComponentDef { name: "llvm-tools", package: "llvm-tools-preview", per_host: true, probes: &[] },
];

/// Components that are part of the fRust archive, with the file that shows each is there.
const BUILT_IN: [(&str, &str); 6] = [
    ("cargo", "bin/cargo"),
    ("clippy", "bin/cargo-clippy"),
    ("rust-analyzer", "bin/rust-analyzer"),
    ("rustc", "bin/rustc"),
    ("rustdoc", "bin/rustdoc"),
    ("rustfmt", "bin/rustfmt"),
];

/// Other names of the built-in components in the official channel.
const BUILT_IN_ALIASES: [&str; 2] = ["rust-analyzer-preview", "clippy-preview"];

fn component_def(name: &str) -> Option<&'static ComponentDef> {
    let canonical = match name {
        "llvm-tools-preview" => "llvm-tools",
        other => other,
    };
    COMPONENTS.iter().find(|def| def.name == canonical)
}

/// What the user asks to add or remove.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Spec {
    Target(String),
    Component(String),
}

impl Spec {
    pub fn target(triple: &str) -> Result<Spec, InstallError> {
        validate_name(triple, "target triple")?;
        Ok(Spec::Target(triple.to_string()))
    }

    /// The canonical name of a component that can be added from the official channel.
    pub fn component(name: &str) -> Result<Spec, InstallError> {
        if let Some(def) = component_def(name) {
            return Ok(Spec::Component(def.name.to_string()));
        }
        if name == "rust-std" {
            return Err(error("rust-std is added per target: use `target add <triple>`"));
        }
        if BUILT_IN_ALIASES.contains(&name) || BUILT_IN.iter().any(|(built_in, _)| *built_in == name) {
            return Err(error(format!("{name} is built into the fRust toolchain archive; it is replaced by an update, not added or removed")));
        }
        let known: Vec<&str> = COMPONENTS.iter().map(|def| def.name).collect();
        Err(error(format!("unknown component {name:?}; the components that can be added are {}", known.join(", "))))
    }
}

impl fmt::Display for Spec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Spec::Target(triple) => write!(f, "target {triple}"),
            Spec::Component(name) => write!(f, "component {name}"),
        }
    }
}

/// What happened to one item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    Installed,
    AlreadyInstalled,
    Removed,
    NotInstalled,
}

/// The outcome of an operation on several items: each one succeeded or failed on its own.
#[derive(Debug, Default)]
pub struct Batch {
    pub done: Vec<(Spec, Change)>,
    pub failed: Vec<(Spec, InstallError)>,
}

/// Where a spec lives in an installed tree.
struct Placement {
    /// The package in the channel manifest.
    package: String,
    /// `*` or a target.
    package_target: String,
    /// The component name inside the package, and so the `manifest-<name>` file that records its files.
    component: String,
    probes: Vec<String>,
}

fn placement(spec: &Spec, tc: &Toolchain) -> Placement {
    match spec {
        Spec::Target(triple) => Placement {
            package: "rust-std".to_string(),
            package_target: triple.clone(),
            component: format!("rust-std-{triple}"),
            probes: Vec::new(),
        },
        Spec::Component(name) => {
            let def = component_def(name).expect("component specs are built from the component table");
            Placement {
                package: def.package.to_string(),
                package_target: if def.per_host { tc.host.clone() } else { "*".to_string() },
                component: def.package.to_string(),
                probes: def.probes.iter().map(|probe| probe.to_string()).collect(),
            }
        }
    }
}

fn manifest_path(tc: &Toolchain, component: &str) -> PathBuf {
    tc.dir.join("lib").join("rustlib").join(format!("manifest-{component}"))
}

/// A target counts as installed when its std libraries are in `lib/rustlib/<target>/lib`.
fn has_std(tc: &Toolchain, triple: &str) -> bool {
    let Ok(entries) = fs::read_dir(tc.dir.join("lib").join("rustlib").join(triple).join("lib")) else { return false };
    entries.flatten().any(|entry| entry.file_name().to_string_lossy().starts_with("libstd-"))
}

fn is_installed(tc: &Toolchain, spec: &Spec) -> bool {
    let place = placement(spec, tc);
    if manifest_path(tc, &place.component).is_file() {
        return true;
    }
    match spec {
        Spec::Target(triple) => has_std(tc, triple),
        Spec::Component(_) => place.probes.iter().any(|probe| tc.dir.join(probe).symlink_metadata().is_ok()),
    }
}

/// The installed targets of the active toolchain, sorted.
fn installed_targets(tc: &Toolchain) -> Vec<String> {
    let mut found = Vec::new();
    if let Ok(entries) = fs::read_dir(tc.dir.join("lib").join("rustlib")) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if validate_name(&name, "target").is_ok() && entry.path().is_dir() && has_std(tc, &name) {
                found.push(name);
            }
        }
    }
    found.sort();
    found
}

// ---- the channel manifest ----

fn fetch_channel(dist: &Dist, release: &str) -> Result<Channel, InstallError> {
    let url = format!("{}/channel-rust-{release}.toml", dist.base);
    let downloads = Downloads::create()?;
    let manifest = downloads.dir.join("channel.toml");
    let checksum = downloads.dir.join("channel.toml.sha256");
    dist.fetch(&url, &manifest)?;
    dist.fetch(&format!("{url}.sha256"), &checksum)?;
    let expected = read_digest_file(&checksum)?;
    let mut file = File::open(&manifest).map_err(install::io_context("could not read the channel manifest"))?;
    let length = file.metadata().map_err(install::io_context("could not read the channel manifest"))?.len();
    if length > MAX_CHANNEL_BYTES {
        return Err(error(format!("the channel manifest {url} is too large")));
    }
    let actual = install::digest_stream(&mut file, None).map_err(install::io_context("could not read the channel manifest"))?;
    if !actual.eq_ignore_ascii_case(&expected) {
        return Err(error(format!("checksum mismatch for the channel manifest {url}: expected {expected}, computed {actual}")));
    }
    let text = fs::read_to_string(&manifest).map_err(|_| error(format!("the channel manifest {url} is not UTF-8")))?;
    Ok(Channel::parse(&text))
}

fn read_digest_file(path: &Path) -> Result<String, InstallError> {
    let mut text = String::new();
    File::open(path)
        .and_then(|file| file.take(MAX_SMALL_FILE_BYTES).read_to_string(&mut text))
        .map_err(install::io_context("could not read the channel manifest checksum"))?;
    let digest = text.split_whitespace().next().unwrap_or("");
    if digest.len() != 64 || !digest.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(error("the channel manifest checksum file does not hold a sha256 digest"));
    }
    Ok(digest.to_string())
}

/// Fetches the channel manifest once per operation, however many items need it.
struct Session<'a> {
    prefix: &'a Path,
    dist: &'a Dist,
    channel: Option<Result<Channel, InstallError>>,
}

impl<'a> Session<'a> {
    fn new(prefix: &'a Path, dist: &'a Dist) -> Self {
        Self { prefix, dist, channel: None }
    }

    fn channel(&mut self, tc: &Toolchain) -> Result<&Channel, InstallError> {
        let dist = self.dist;
        self.channel.get_or_insert_with(|| fetch_channel(dist, &tc.release)).as_ref().map_err(Clone::clone)
    }

    fn add_one(&mut self, spec: &Spec) -> Result<Change, InstallError> {
        let tc = toolchain(self.prefix)?;
        if is_installed(&tc, spec) {
            remember_locked(self.prefix, spec, true)?;
            return Ok(Change::AlreadyInstalled);
        }
        let place = placement(spec, &tc);
        let dist = self.dist;
        let (url, hash) = {
            let channel = self.channel(&tc)?;
            let package = channel.package(&place.package, &place.package_target).filter(|package| package.available && !package.url.is_empty());
            let Some(package) = package else {
                let what = match spec {
                    Spec::Target(triple) => format!("target {triple}"),
                    Spec::Component(name) => format!("component {name}"),
                };
                return Err(error(format!("{what} is not available for Rust {}", tc.release)));
            };
            if package.hash.len() != 64 || !package.hash.chars().all(|ch| ch.is_ascii_hexdigit()) {
                return Err(error(format!("the channel manifest has no sha256 for {} ({})", place.package, place.package_target)));
            }
            if let (Some(listed), Some(built)) = (channel.commit(&place.package), tc.commit.as_deref()) {
                if !listed.starts_with(built) && !built.starts_with(listed) {
                    return Err(error(format!(
                        "the official {} package is built from Rust commit {listed} but this toolchain reports {built}; refusing to mix them",
                        place.package
                    )));
                }
            }
            (dist.resolve(&package.url)?, package.hash.clone())
        };

        let downloads = Downloads::create()?;
        let archive = downloads.dir.join("package.tar.gz");
        dist.fetch(&url, &archive)?;

        let _lock = install::lock_prefix(self.prefix, false)?;
        // The prefix may have changed while the package was downloading.
        let locked = toolchain(self.prefix)?;
        if locked.id != tc.id {
            return Err(error(format!("the active toolchain changed from {} to {} during the download; run the command again", tc.id, locked.id)));
        }
        if is_installed(&locked, spec) {
            remember(self.prefix, spec, true)?;
            return Ok(Change::AlreadyInstalled);
        }
        install::clear_leftovers(self.prefix)?;
        let work = Work::create(self.prefix)?;
        let unit = unpack_package(&work, &archive, &url, &hash, &place.component)?;
        commit(&locked.dir, &unit)?;
        remember(self.prefix, spec, true)?;
        Ok(Change::Installed)
    }

    fn remove_one(&mut self, spec: &Spec) -> Result<Change, InstallError> {
        let _lock = install::lock_prefix(self.prefix, false)?;
        let tc = toolchain(self.prefix)?;
        if let Spec::Target(triple) = spec {
            if *triple == tc.host {
                return Err(error(format!("{triple} is the host target of this toolchain and cannot be removed; its std is part of the fRust build")));
            }
        }
        let place = placement(spec, &tc);
        let manifest = manifest_path(&tc, &place.component);
        let installed = is_installed(&tc, spec);
        let entries = if manifest.is_file() {
            parse_entries(&read_small(&manifest)?)?
        } else if installed {
            match spec {
                Spec::Target(triple) => vec![Entry::dir(format!("lib/rustlib/{triple}"))],
                Spec::Component(name) if name == "rust-docs" => vec![Entry::dir("share/doc/rust".to_string())],
                Spec::Component(name) if name == "rust-src" => vec![Entry::dir("lib/rustlib/src/rust".to_string())],
                Spec::Component(name) => {
                    return Err(error(format!("{name} came with the toolchain archive, not from frust-install, so it cannot be removed")));
                }
            }
        } else {
            Vec::new()
        };
        if !installed && entries.is_empty() {
            remember(self.prefix, spec, false)?;
            return Ok(Change::NotInstalled);
        }
        let work = Work::create(self.prefix)?;
        take_out(&tc.dir, &work, &entries, &manifest)?;
        remember(self.prefix, spec, false)?;
        Ok(Change::Removed)
    }
}

// ---- unpacking and committing a package ----

#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    dir: bool,
    path: String,
}

impl Entry {
    fn dir(path: String) -> Self {
        Self { dir: true, path }
    }

    fn file(path: String) -> Self {
        Self { dir: false, path }
    }
}

/// An unpacked component: the directory that holds its files, and what to install from it.
struct Unit {
    root: PathBuf,
    entries: Vec<Entry>,
}

fn read_small(path: &Path) -> Result<String, InstallError> {
    let mut text = String::new();
    File::open(path)
        .and_then(|file| file.take(MAX_SMALL_FILE_BYTES + 1).read_to_string(&mut text))
        .map_err(install::io_context(format!("could not read {}", path.display())))?;
    if text.len() as u64 > MAX_SMALL_FILE_BYTES {
        return Err(error(format!("{} is too large", path.display())));
    }
    Ok(text)
}

/// A relative path inside a tree: no `..`, no absolute path, no empty or NUL-bearing component.
fn safe_relative(path: &str) -> Result<String, InstallError> {
    if path.is_empty() || path.starts_with('/') || path.contains(['\0', '\\']) {
        return Err(error(format!("the package lists the unsafe path {path:?}")));
    }
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => return Err(error(format!("the package lists the unsafe path {path:?}"))),
            part => parts.push(part),
        }
    }
    if parts.is_empty() {
        return Err(error(format!("the package lists the unsafe path {path:?}")));
    }
    Ok(parts.join("/"))
}

/// The `file:` and `dir:` lines of a component's `manifest.in` or an installed `manifest-<component>`.
fn parse_entries(text: &str) -> Result<Vec<Entry>, InstallError> {
    let mut entries: Vec<Entry> = Vec::new();
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let entry = match line.split_once(':') {
            Some(("file", path)) => Entry::file(safe_relative(path)?),
            Some(("dir", path)) => Entry::dir(safe_relative(path)?),
            _ => return Err(error(format!("the component manifest has the unreadable line {line:?}"))),
        };
        // A directory entry already covers what is listed below it.
        let covered = entries.iter().any(|earlier| earlier.dir && entry.path.starts_with(&format!("{}/", earlier.path)));
        if !covered {
            entries.push(entry);
        }
    }
    Ok(entries)
}

/// Rejects links and special files below `path`: a symlink that is fine in the work directory could
/// point elsewhere once it is renamed into the toolchain.
fn require_plain_tree(path: &Path) -> Result<(), InstallError> {
    let metadata = fs::symlink_metadata(path).map_err(install::io_context(format!("the package lists {} but does not contain it", path.display())))?;
    if metadata.is_dir() {
        let mut pending = vec![path.to_path_buf()];
        while let Some(dir) = pending.pop() {
            let entries = fs::read_dir(&dir).map_err(install::io_context(format!("could not list {}", dir.display())))?;
            for entry in entries {
                let entry = entry.map_err(install::io_context(format!("could not list {}", dir.display())))?;
                let kind = entry.file_type().map_err(install::io_context(format!("could not inspect {}", entry.path().display())))?;
                if kind.is_dir() {
                    pending.push(entry.path());
                } else if !kind.is_file() {
                    return Err(error(format!("the package contains {}, which is a link or special file", entry.path().display())));
                }
            }
        }
        Ok(())
    } else if metadata.is_file() {
        Ok(())
    } else {
        Err(error(format!("the package contains {}, which is a link or special file", path.display())))
    }
}

/// Verifies the downloaded archive against `expected`, validates its headers, extracts it and reads
/// the component `component` from it.
fn unpack_package(work: &Work, archive: &Path, url: &str, expected: &str, component: &str) -> Result<Unit, InstallError> {
    let private = work.dir.join("package.tar.gz");
    let mut source = File::open(archive).map_err(install::io_context("could not read the downloaded package"))?;
    let mut copy = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&private)
        .map_err(install::io_context(format!("could not create {}", private.display())))?;
    let actual = install::digest_stream(&mut source, Some(&mut copy)).map_err(install::io_context("could not copy the downloaded package"))?;
    drop(copy);
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(error(format!("checksum mismatch for {url} before extraction: expected {expected}, computed {actual}")));
    }
    install::tar_check::validate(&private)?;
    let tree = work.dir.join("tree");
    fs::create_dir(&tree).map_err(install::io_context(format!("could not create {}", tree.display())))?;
    install::extract(&private, &tree)?;

    let mut tops = Vec::new();
    for entry in fs::read_dir(&tree).map_err(install::io_context("could not list the unpacked package"))? {
        let entry = entry.map_err(install::io_context("could not list the unpacked package"))?;
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            tops.push(entry.path());
        }
    }
    let [top] = tops.as_slice() else {
        return Err(error("the package does not have exactly one top-level directory"));
    };
    let listed = read_small(&top.join("components"))?;
    let names: Vec<&str> = listed.lines().map(str::trim).filter(|line| !line.is_empty()).collect();
    if names != [component] {
        return Err(error(format!("the package holds the components {names:?}, expected exactly {component:?}")));
    }
    validate_name(component, "component name")?;
    let root = top.join(component);
    if !fs::symlink_metadata(&root).is_ok_and(|metadata| metadata.is_dir()) {
        return Err(error(format!("the package has no {component} directory")));
    }
    let text = read_small(&root.join("manifest.in"))?;
    let mut entries = parse_entries(&text)?;
    if entries.is_empty() {
        return Err(error(format!("the package lists no files for {component}")));
    }
    for entry in &entries {
        require_plain_tree(&root.join(&entry.path))?;
    }
    // Official installs record what they put where; the record is what removal reads.
    let record = format!("lib/rustlib/manifest-{component}");
    if entries.iter().any(|entry| entry.path == record) {
        return Err(error(format!("the package lists {record}, which frust-install writes itself")));
    }
    let record_path = root.join(&record);
    fs::create_dir_all(record_path.parent().expect("the record path has a parent")).map_err(install::io_context("could not prepare the component record"))?;
    let body: String = entries.iter().map(|entry| format!("{}:{}\n", if entry.dir { "dir" } else { "file" }, entry.path)).collect();
    fs::write(&record_path, body).map_err(install::io_context("could not write the component record"))?;
    entries.push(Entry::file(record));
    Ok(Unit { root, entries })
}

/// Renames every entry of `unit` into `dest`. Nothing is overwritten: an entry whose destination exists
/// fails the whole commit before anything moves. A failure part way puts back what was moved.
fn commit(dest: &Path, unit: &Unit) -> Result<(), InstallError> {
    for entry in &unit.entries {
        let target = dest.join(&entry.path);
        if target.symlink_metadata().is_ok() {
            return Err(error(format!("{} already exists in the toolchain and was not installed by frust-install; refusing to replace it", entry.path)));
        }
    }
    let mut placed: Vec<PathBuf> = Vec::new();
    let mut made: Vec<PathBuf> = Vec::new();
    let result = (|| -> Result<(), InstallError> {
        for entry in &unit.entries {
            let target = dest.join(&entry.path);
            let parent = target.parent().expect("an entry path has a parent");
            let mut missing = Vec::new();
            let mut probe = parent;
            while probe.symlink_metadata().is_err() && probe != dest {
                missing.push(probe.to_path_buf());
                probe = probe.parent().expect("the destination is above every entry");
            }
            fs::create_dir_all(parent).map_err(install::io_context(format!("could not create {}", parent.display())))?;
            made.extend(missing.into_iter().rev());
            fs::rename(unit.root.join(&entry.path), &target).map_err(install::io_context(format!("could not install {}", entry.path)))?;
            placed.push(target);
        }
        Ok(())
    })();
    if result.is_err() {
        for target in placed.iter().rev() {
            let _ = install::remove_path(target);
        }
        for dir in made.iter().rev() {
            let _ = fs::remove_dir(dir);
        }
    }
    result
}

/// Moves the entries and the record of a component out of the toolchain into `work`, which deletes them
/// when it drops. A failure part way puts back what was moved.
fn take_out(dest: &Path, work: &Work, entries: &[Entry], record: &Path) -> Result<(), InstallError> {
    let aside = work.dir.join("removed");
    fs::create_dir(&aside).map_err(install::io_context("could not create the removal directory"))?;
    let mut moved: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut paths: Vec<PathBuf> = entries.iter().map(|entry| dest.join(&entry.path)).collect();
    paths.push(record.to_path_buf());
    let result = (|| -> Result<(), InstallError> {
        for (index, path) in paths.iter().enumerate() {
            if path.symlink_metadata().is_err() {
                continue;
            }
            let parked = aside.join(index.to_string());
            fs::rename(path, &parked).map_err(install::io_context(format!("could not remove {}", path.display())))?;
            moved.push((path.clone(), parked));
        }
        Ok(())
    })();
    if result.is_err() {
        for (path, parked) in moved.iter().rev() {
            let _ = fs::rename(parked, path);
        }
        return result;
    }
    // Directories the package emptied go too, but never the top levels of the toolchain.
    let root = dest.to_path_buf();
    for path in &paths {
        let mut parent = path.parent();
        while let Some(dir) = parent {
            let Ok(relative) = dir.strip_prefix(&root) else { break };
            if relative.components().count() <= 2 || fs::remove_dir(dir).is_err() {
                break;
            }
            parent = dir.parent();
        }
    }
    install::sync_dir(dest);
    Ok(())
}

// ---- what the user added ----

/// The extras recorded for a prefix, in the order they were added.
pub fn read_extras(prefix: &Path) -> Vec<Spec> {
    let Ok(text) = fs::read_to_string(prefix.join(EXTRAS_FILE)) else { return Vec::new() };
    let mut specs: Vec<Spec> = Vec::new();
    for line in text.lines().map(str::trim) {
        let spec = match line.split_once(' ') {
            Some(("target", triple)) => Spec::target(triple.trim()).ok(),
            Some(("component", name)) => Spec::component(name.trim()).ok(),
            _ => None,
        };
        if let Some(spec) = spec {
            if !specs.contains(&spec) {
                specs.push(spec);
            }
        }
    }
    specs
}

/// Records or forgets one extra. The caller holds the prefix lock.
fn remember(prefix: &Path, spec: &Spec, present: bool) -> Result<(), InstallError> {
    let mut specs = read_extras(prefix);
    let before = specs.clone();
    specs.retain(|known| known != spec);
    if present {
        specs.push(spec.clone());
    }
    if specs == before {
        return Ok(());
    }
    let mut body = String::from("# targets and components added with frust-install; they are added again after an update\n");
    for known in &specs {
        body.push_str(&format!("{known}\n"));
    }
    // The prefix of the temporary name is one that the installer's leftover cleanup removes.
    let temporary = prefix.join(format!(".state.json.next-extras.{}", install::unique_suffix()));
    let commit = || -> std::io::Result<()> {
        use std::io::Write;
        let mut file = OpenOptions::new().write(true).create_new(true).open(&temporary)?;
        file.write_all(body.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, prefix.join(EXTRAS_FILE))
    };
    if let Err(err) = commit() {
        let _ = fs::remove_file(&temporary);
        return Err(error(format!("could not write {EXTRAS_FILE}: {err}")));
    }
    install::sync_dir(prefix);
    Ok(())
}

/// Takes the prefix lock to record an extra when nothing else is being changed.
fn remember_locked(prefix: &Path, spec: &Spec, present: bool) -> Result<(), InstallError> {
    let _lock = install::lock_prefix(prefix, false)?;
    remember(prefix, spec, present)
}

// ---- operations ----

/// Adds targets and components to the active toolchain. Items already installed are left alone and
/// still remembered. One item failing does not stop the others.
pub fn add(prefix: &Path, dist: &Dist, specs: &[Spec]) -> Result<Batch, InstallError> {
    toolchain(prefix)?;
    let mut session = Session::new(prefix, dist);
    let mut batch = Batch::default();
    for spec in specs {
        match session.add_one(spec) {
            Ok(change) => batch.done.push((spec.clone(), change)),
            Err(err) => batch.failed.push((spec.clone(), err)),
        }
    }
    Ok(batch)
}

/// Removes targets and components from the active toolchain and forgets them.
pub fn remove(prefix: &Path, dist: &Dist, specs: &[Spec]) -> Result<Batch, InstallError> {
    toolchain(prefix)?;
    let mut session = Session::new(prefix, dist);
    let mut batch = Batch::default();
    for spec in specs {
        match session.remove_one(spec) {
            Ok(change) => batch.done.push((spec.clone(), change)),
            Err(err) => batch.failed.push((spec.clone(), err)),
        }
    }
    Ok(batch)
}

/// Adds the extras the user asked for earlier to the active toolchain, after an update or install.
/// Extras the toolchain already has, because the new archive bundles them, need no download.
pub fn reapply(prefix: &Path, dist: &Dist) -> Result<Batch, InstallError> {
    let specs = read_extras(prefix);
    if specs.is_empty() {
        return Ok(Batch::default());
    }
    add(prefix, dist, &specs)
}

/// Whether the prefix has extras recorded.
pub fn has_extras(prefix: &Path) -> bool {
    !read_extras(prefix).is_empty()
}

/// A target in a listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetStatus {
    pub name: String,
    pub installed: bool,
}

/// Installed targets and, when asked, the targets the official channel offers for this toolchain's release.
#[derive(Debug)]
pub struct TargetList {
    pub targets: Vec<TargetStatus>,
    /// Why the available targets are missing from the list, when they were asked for and could not be fetched.
    pub unavailable: Option<String>,
}

pub fn list_targets(prefix: &Path, dist: &Dist, with_available: bool) -> Result<TargetList, InstallError> {
    let tc = toolchain(prefix)?;
    let installed = installed_targets(&tc);
    let mut names = installed.clone();
    let mut unavailable = None;
    if with_available {
        match Session::new(prefix, dist).channel(&tc) {
            Ok(channel) => names.extend(channel.targets_of("rust-std")),
            Err(err) => unavailable = Some(err.message.clone()),
        }
    }
    names.sort();
    names.dedup();
    let targets = names.into_iter().map(|name| TargetStatus { installed: installed.contains(&name), name }).collect();
    Ok(TargetList { targets, unavailable })
}

/// A component in a listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComponentStatus {
    pub name: String,
    pub installed: bool,
}

#[derive(Debug)]
pub struct ComponentList {
    pub components: Vec<ComponentStatus>,
    pub unavailable: Option<String>,
}

/// The components of the toolchain: those built into the archive, and those the official channel
/// offers for this host. `with_available` fetches the channel to leave out the ones it does not have.
pub fn list_components(prefix: &Path, dist: &Dist, with_available: bool) -> Result<ComponentList, InstallError> {
    let tc = toolchain(prefix)?;
    let mut unavailable = None;
    let channel = if with_available {
        match Session::new(prefix, dist).channel(&tc) {
            Ok(channel) => Some(channel.clone()),
            Err(err) => {
                unavailable = Some(err.message.clone());
                None
            }
        }
    } else {
        None
    };
    let mut components: Vec<ComponentStatus> = BUILT_IN
        .iter()
        .map(|(name, probe)| ComponentStatus { name: name.to_string(), installed: tc.dir.join(probe).is_file() })
        .collect();
    components.push(ComponentStatus { name: "rust-std".to_string(), installed: has_std(&tc, &tc.host) });
    for def in &COMPONENTS {
        let spec = Spec::Component(def.name.to_string());
        let installed = is_installed(&tc, &spec);
        let offered = channel.as_ref().is_none_or(|channel| {
            let place = placement(&spec, &tc);
            channel.package(&place.package, &place.package_target).is_some_and(|package| package.available)
        });
        if installed || offered {
            components.push(ComponentStatus { name: def.name.to_string(), installed });
        }
    }
    components.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(ComponentList { components, unavailable })
}

/// What `show` prints: the prefix, the active and previous versions and what is installed in the active one.
pub fn show(prefix: &Path) -> Result<String, InstallError> {
    let tc = toolchain(prefix)?;
    let previous = install::previous_version(prefix)?;
    let variant = install::current_variant(prefix)?;
    let mut out = String::new();
    out.push_str(&format!("prefix: {}\n", prefix.display()));
    out.push_str(&format!("current: {}\n", tc.id));
    out.push_str(&format!("previous: {}\n", previous.as_deref().unwrap_or("none")));
    out.push_str(&format!("variant: {}\n", variant.as_deref().unwrap_or("none")));
    out.push_str(&format!("host: {}\n", tc.host));
    out.push_str(&format!("rust release: {}\n", tc.release));
    out.push_str("targets:\n");
    for target in installed_targets(&tc) {
        out.push_str(&format!("  {target}\n"));
    }
    out.push_str("components:\n");
    let list = list_components(prefix, &Dist::official(), false)?;
    for component in list.components.iter().filter(|component| component.installed) {
        out.push_str(&format!("  {}\n", component.name));
    }
    Ok(out)
}

/// The path of a tool of the active toolchain, when it has one.
pub fn which(prefix: &Path, tool: &str) -> Result<PathBuf, InstallError> {
    if tool.is_empty() || tool.contains(['/', '\0']) || tool.starts_with('.') {
        return Err(error(format!("{tool:?} is not a tool name")));
    }
    let Some(id) = install::active_version(prefix)? else {
        return Err(error(format!("no toolchain is installed in {}", prefix.display())));
    };
    let path = prefix.join("current").join("bin").join(tool);
    if path.is_file() {
        Ok(path)
    } else {
        Err(error(format!("{tool} is not in the active toolchain {id}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"manifest-version = "2"
date = "2026-11-12"

[pkg.rust-std]
version = "1.100.0 (0ef1bc386 2026-11-09)"
git_commit_hash = "0ef1bc386059596dada8fb65b50425aab2ea637a"

[pkg.rust-std.target.wasm32-wasip1]
available = true
url = "https://static.rust-lang.org/dist/2026-11-12/rust-std-1.100.0-wasm32-wasip1.tar.gz"
hash = "c62d050b91478a285b29a4f6fca52a261ab6dc34b76561c7bda4ace466420106"
xz_url = "https://static.rust-lang.org/dist/2026-11-12/rust-std-1.100.0-wasm32-wasip1.tar.xz"
xz_hash = "7a506aa6fcc8ed4308780fc412ba5713eb1c97bf5b1e940eaa2cf1f70299c777"

[pkg.rust-std.target.nope]
available = false
url = ""
hash = ""

[pkg.rust-src.target."*"]
available = true
url = "https://static.rust-lang.org/dist/2026-11-12/rust-src-1.100.0.tar.gz"
hash = "f37ba90547b1075da3a0953742cab9f9e33b5c08bce747bb0ff8382537b2fa8c"

[renames.clippy]
to = "clippy-preview"

[[artifacts.thing]]
url = "https://example.com/not-a-package"
"#;

    #[test]
    fn the_channel_manifest_subset_is_read() {
        let channel = Channel::parse(SAMPLE);
        let wasi = channel.package("rust-std", "wasm32-wasip1").unwrap();
        assert!(wasi.available);
        assert_eq!(wasi.url, "https://static.rust-lang.org/dist/2026-11-12/rust-std-1.100.0-wasm32-wasip1.tar.gz");
        assert_eq!(wasi.hash, "c62d050b91478a285b29a4f6fca52a261ab6dc34b76561c7bda4ace466420106");
        assert!(!channel.package("rust-std", "nope").unwrap().available);
        assert!(channel.package("rust-src", "*").unwrap().available);
        assert!(channel.package("rust-std", "missing").is_none());
        assert_eq!(channel.commit("rust-std"), Some("0ef1bc386059596dada8fb65b50425aab2ea637a"));
        assert_eq!(channel.targets_of("rust-std"), vec!["wasm32-wasip1".to_string()]);
    }

    #[test]
    fn urls_are_rewritten_only_under_an_override() {
        let listed = "https://static.rust-lang.org/dist/2026-11-12/x.tar.gz";
        assert_eq!(Dist::official().resolve(listed).unwrap(), listed);
        assert!(Dist::official().resolve("https://evil.example/x.tar.gz").is_err());
        assert!(Dist::official().resolve("file:///etc/passwd").is_err());
        let mirror = Dist::with_base("file:///srv/dist/");
        assert_eq!(mirror.resolve(listed).unwrap(), "file:///srv/dist/2026-11-12/x.tar.gz");
        assert_eq!(mirror.resolve("file:///other/x.tar.gz").unwrap(), "file:///other/x.tar.gz");
    }

    #[test]
    fn component_manifest_lines_are_checked() {
        let entries = parse_entries("file:lib/a\ndir:share/doc/rust/html\nfile:share/doc/rust/html/index.html\n\n").unwrap();
        assert_eq!(entries, vec![Entry::file("lib/a".into()), Entry::dir("share/doc/rust/html".into())]);
        for bad in ["file:../x", "file:/etc/passwd", "dir:a/../../b", "link:a", "file:", "file:a\\b", "nonsense"] {
            assert!(parse_entries(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn specs_are_validated_and_canonical() {
        for built_in in ["rust-analyzer", "rust-analyzer-preview", "clippy", "rustfmt", "rustdoc"] {
            assert!(Spec::component(built_in).unwrap_err().message.contains("built into"), "{built_in}");
        }
        assert_eq!(Spec::component("llvm-tools-preview").unwrap(), Spec::Component("llvm-tools".into()));
        assert!(Spec::component("clippy").unwrap_err().message.contains("built into"));
        assert!(Spec::component("rust-std").unwrap_err().message.contains("target add"));
        assert!(Spec::component("bogus").unwrap_err().message.contains("rust-docs"));
        assert!(Spec::target("wasm32-wasip1").is_ok());
        for bad in ["", "../x", "a/b", "-x", "a b", &"x".repeat(200)] {
            assert!(Spec::target(bad).is_err(), "{bad:?}");
        }
    }
}
