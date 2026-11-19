//! Download the published toolchain for this host and install it.
//!
//! The release publishes three files per target next to each other: the archive,
//! its sha-256 file and a one-line id file. The id file lets an update find out
//! whether anything changed without downloading the archive. The archive itself
//! is verified and installed by [`crate::install`].

use std::env;
use std::fs;
use std::io::IsTerminal;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::install::{self, InstallError};

pub const REPOSITORY: &str = "Seggys116/frust";
/// Replaces the release location. Used for mirrors and for tests; any scheme curl accepts works.
pub const BASE_ENV: &str = "FRUST_RELEASE_BASE";
/// Replaces the default install prefix.
pub const PREFIX_ENV: &str = "FRUST_PREFIX";
const MAX_ID_LEN: usize = 128;
const MAX_ID_BYTES: usize = 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assets {
    pub archive: String,
    pub checksum: String,
    pub id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    UpToDate(String),
    Installed { current: String, previous: Option<String> },
}

fn error(message: impl Into<String>) -> InstallError {
    InstallError { message: message.into() }
}

/// The release target triple for a host, or `None` when no toolchain is built for it.
pub fn target_for(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("linux", "x86_64") => Some("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-gnu"),
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        _ => None,
    }
}

pub fn host_target() -> Result<&'static str, InstallError> {
    target_for(env::consts::OS, env::consts::ARCH)
        .ok_or_else(|| error(format!("fRust has no toolchain for {} on {}", env::consts::OS, env::consts::ARCH)))
}

pub fn assets(target: &str) -> Assets {
    let archive = format!("frust-toolchain-{target}.tar.gz");
    Assets { checksum: format!("{archive}.sha256"), id: format!("frust-toolchain-{target}.id"), archive }
}

/// A release tag is `v` plus a version; `0.5.0` and `v0.5.0` name the same release.
pub fn tag_for(version: &str) -> Result<String, InstallError> {
    let bare = version.strip_prefix('v').unwrap_or(version);
    let first_ok = bare.chars().next().is_some_and(|ch| ch.is_ascii_digit());
    if !first_ok || !bare.chars().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '+')) {
        return Err(error(format!("{version:?} is not a release version such as 0.5.0")));
    }
    Ok(format!("v{bare}"))
}

pub fn base_url(version: Option<&str>, override_base: Option<&str>) -> Result<String, InstallError> {
    if let Some(base) = override_base.filter(|base| !base.is_empty()) {
        return Ok(base.trim_end_matches('/').to_string());
    }
    Ok(match version {
        Some(version) => format!("https://github.com/{REPOSITORY}/releases/download/{}", tag_for(version)?),
        None => format!("https://github.com/{REPOSITORY}/releases/latest/download"),
    })
}

/// Where the toolchain goes when `--prefix` is absent. Absolute explicit and XDG values win.
pub fn default_prefix(os: &str, home: Option<PathBuf>, xdg_data: Option<PathBuf>, explicit: Option<PathBuf>) -> Result<PathBuf, InstallError> {
    if let Some(explicit) = explicit.filter(|path| path.is_absolute()) {
        return Ok(explicit);
    }
    let home = home.filter(|path| path.is_absolute());
    let prefix = match os {
        "macos" => home.map(|home| home.join("Library").join("Application Support").join("fRust")),
        _ => xdg_data.filter(|path| path.is_absolute()).or_else(|| home.map(|home| home.join(".local").join("share"))).map(|dir| dir.join("frust")),
    };
    prefix.ok_or_else(|| error(format!("could not choose an install prefix: set HOME, {PREFIX_ENV} or pass --prefix")))
}

pub fn prefix_from_environment() -> Result<PathBuf, InstallError> {
    default_prefix(env::consts::OS, env::var_os("HOME").map(PathBuf::from), env::var_os("XDG_DATA_HOME").map(PathBuf::from), env::var_os(PREFIX_ENV).map(PathBuf::from))
}

/// Ids name directories, so the id file gets the same conservative check as the manifest.
fn parse_id(text: &str) -> Result<String, InstallError> {
    let id = text.trim();
    let first_ok = id.chars().next().is_some_and(|ch| ch.is_ascii_alphanumeric());
    if text.len() > MAX_ID_BYTES || id.len() > MAX_ID_LEN || !first_ok || !id.chars().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-')) {
        return Err(error("the release id file does not hold a toolchain id"));
    }
    Ok(id.to_string())
}

struct Downloads {
    dir: PathBuf,
}

impl Downloads {
    fn create() -> Result<Self, InstallError> {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_nanos()).unwrap_or(0);
        let dir = env::temp_dir().join(format!("frust-install-{}-{nanos}", std::process::id()));
        fs::DirBuilder::new().mode(0o700).create(&dir).map_err(|err| error(format!("could not create {}: {err}", dir.display())))?;
        Ok(Self { dir })
    }
}

impl Drop for Downloads {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn available(tool: &str) -> bool {
    Command::new(tool).arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok()
}

/// Without an override, only https is allowed, including across redirects.
fn fetch(url: &str, dest: &Path, restricted: bool) -> Result<(), InstallError> {
    let mut command;
    if available("curl") {
        command = Command::new("curl");
        command.args(["--fail", "--location", "--retry", "3", "--connect-timeout", "30"]);
        if std::io::stderr().is_terminal() {
            command.arg("--progress-bar");
        } else {
            command.args(["--silent", "--show-error"]);
        }
        if restricted {
            command.args(["--proto", "=https", "--proto-redir", "=https", "--tlsv1.2"]);
        }
        command.arg("--output").arg(dest).arg(url);
    } else if available("wget") {
        command = Command::new("wget");
        command.arg("--quiet").arg("--tries=3");
        if restricted {
            command.arg("--https-only");
        }
        command.arg("--output-document").arg(dest).arg(url);
    } else {
        return Err(error("curl or wget is required to download a release"));
    }
    let status = command.status().map_err(|err| error(format!("could not run the downloader: {err}")))?;
    if !status.success() {
        return Err(error(format!("could not download {url} ({status})")));
    }
    Ok(())
}

/// Installs the published toolchain, or reports that the active one already matches it.
pub fn install_release(prefix: &Path, version: Option<&str>) -> Result<Outcome, InstallError> {
    let target = host_target()?;
    let names = assets(target);
    let override_base = env::var(BASE_ENV).ok();
    let base = base_url(version, override_base.as_deref())?;
    let restricted = override_base.as_deref().is_none_or(str::is_empty);
    let downloads = Downloads::create()?;

    let id_path = downloads.dir.join(&names.id);
    fetch(&format!("{base}/{}", names.id), &id_path, restricted)
        .map_err(|err| error(format!("{}; a toolchain may not be published for {target} yet", err.message)))?;
    let id = parse_id(&fs::read_to_string(&id_path).map_err(|err| error(format!("could not read the release id: {err}")))?)?;
    if install::current_version(prefix)?.as_deref() == Some(id.as_str()) {
        return Ok(Outcome::UpToDate(id));
    }

    let archive = downloads.dir.join(&names.archive);
    let checksum = downloads.dir.join(&names.checksum);
    fetch(&format!("{base}/{}", names.archive), &archive, restricted)?;
    fetch(&format!("{base}/{}", names.checksum), &checksum, restricted)?;
    let state = install::install(prefix, &archive, &checksum)?;
    let current = state.current.unwrap_or_default();
    if current != id {
        return Err(error(format!("the release lists {id} but the archive holds {current}; {current} is installed")));
    }
    Ok(Outcome::Installed { current, previous: state.previous })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_follow_the_host() {
        assert_eq!(target_for("linux", "x86_64"), Some("x86_64-unknown-linux-gnu"));
        assert_eq!(target_for("macos", "aarch64"), Some("aarch64-apple-darwin"));
        assert_eq!(target_for("windows", "x86_64"), None);
        assert_eq!(target_for("linux", "riscv64"), None);
    }

    #[test]
    fn asset_names_share_one_stem() {
        let names = assets("x86_64-unknown-linux-gnu");
        assert_eq!(names.archive, "frust-toolchain-x86_64-unknown-linux-gnu.tar.gz");
        assert_eq!(names.checksum, "frust-toolchain-x86_64-unknown-linux-gnu.tar.gz.sha256");
        assert_eq!(names.id, "frust-toolchain-x86_64-unknown-linux-gnu.id");
    }

    #[test]
    fn versions_become_tags() {
        assert_eq!(tag_for("0.5.0").unwrap(), "v0.5.0");
        assert_eq!(tag_for("v0.5.0").unwrap(), "v0.5.0");
        assert_eq!(tag_for("1.0.0-rc.1").unwrap(), "v1.0.0-rc.1");
        for bad in ["", "v", "latest", "0.5/../x", "0.5.0 ", "v-1"] {
            assert!(tag_for(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn base_urls_pick_latest_a_tag_or_the_override() {
        assert_eq!(base_url(None, None).unwrap(), "https://github.com/Seggys116/frust/releases/latest/download");
        assert_eq!(base_url(Some("0.5.0"), None).unwrap(), "https://github.com/Seggys116/frust/releases/download/v0.5.0");
        assert_eq!(base_url(Some("0.5.0"), Some("file:///srv/frust/")).unwrap(), "file:///srv/frust");
        assert_eq!(base_url(None, Some("")).unwrap(), "https://github.com/Seggys116/frust/releases/latest/download");
        assert!(base_url(Some("../x"), None).is_err());
    }

    #[test]
    fn prefixes_follow_the_platform_and_only_accept_absolute_paths() {
        let home = Some(PathBuf::from("/home/builder"));
        assert_eq!(default_prefix("linux", home.clone(), None, None).unwrap(), PathBuf::from("/home/builder/.local/share/frust"));
        assert_eq!(default_prefix("linux", home.clone(), Some(PathBuf::from("/data")), None).unwrap(), PathBuf::from("/data/frust"));
        assert_eq!(default_prefix("linux", home.clone(), Some(PathBuf::from("data")), None).unwrap(), PathBuf::from("/home/builder/.local/share/frust"));
        assert_eq!(default_prefix("macos", Some(PathBuf::from("/Users/builder")), None, None).unwrap(), PathBuf::from("/Users/builder/Library/Application Support/fRust"));
        assert_eq!(default_prefix("linux", home.clone(), None, Some(PathBuf::from("/opt/frust"))).unwrap(), PathBuf::from("/opt/frust"));
        assert_eq!(default_prefix("linux", home, None, Some(PathBuf::from("relative"))).unwrap(), PathBuf::from("/home/builder/.local/share/frust"));
        assert!(default_prefix("linux", None, None, None).is_err());
        assert!(default_prefix("linux", Some(PathBuf::from("relative")), None, None).is_err());
    }

    #[test]
    fn ids_are_trimmed_and_checked() {
        assert_eq!(parse_id("frust-0.5.0-0ef1bc386\n").unwrap(), "frust-0.5.0-0ef1bc386");
        for bad in ["", "\n", "../escape", ".hidden", "two words", "a/b", &"x".repeat(129)] {
            assert!(parse_id(bad).is_err(), "{bad:?}");
        }
        assert!(parse_id(&format!("id{}", " ".repeat(2000))).is_err());
    }
}
