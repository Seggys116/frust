//! Download the published toolchain for this host and install it.
//!
//! The release publishes three files per target next to each other: the archive,
//! its sha-256 file and a one-line id file. The id file lets an update find out
//! whether anything changed without downloading the archive. The archive itself
//! is verified and installed by [`crate::install`].
//!
//! Linux x86-64 has two builds of the toolchain: `baseline` for every x86-64 CPU
//! and `v3`, compiled for x86-64-v3, published under the target name with a `-v3`
//! suffix. [`choose_variant`] picks one; the other targets have a single archive.

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
/// Chooses the build on hosts that have more than one: `auto` (default), `baseline` or `v3`.
pub const VARIANT_ENV: &str = "FRUST_VARIANT";
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

/// A build of the toolchain for one target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Variant {
    /// Plain x86-64; the archive named by the bare target.
    Baseline,
    /// Compiler and tools built for x86-64-v3; the archive named by the target plus `-v3`.
    V3,
}

impl Variant {
    pub fn name(self) -> &'static str {
        match self {
            Variant::Baseline => "baseline",
            Variant::V3 => "v3",
        }
    }

    fn target_suffix(self) -> &'static str {
        match self {
            Variant::Baseline => "",
            Variant::V3 => "-v3",
        }
    }
}

/// What `FRUST_VARIANT` asks for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VariantRequest {
    #[default]
    Auto,
    Baseline,
    V3,
}

/// Tells whether this machine can run the x86-64-v3 build. A trait so tests do not depend on the host CPU.
pub trait CpuProbe {
    fn supports_v3(&self) -> bool;
}

/// The CPU this process runs on.
pub struct HostCpu;

impl CpuProbe for HostCpu {
    fn supports_v3(&self) -> bool {
        host_supports_v3()
    }
}

/// Every extension that the x86-64-v3 build can use: the x86-64-v2 set (std has no name for LAHF/SAHF,
/// which every CPU with SSE4.2 has) plus AVX, AVX2, BMI1, BMI2, FMA, MOVBE, LZCNT, F16C and XSAVE.
/// std reports AVX only when the operating system saves the wide registers too.
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
fn host_supports_v3() -> bool {
    use std::arch::is_x86_feature_detected;
    is_x86_feature_detected!("cmpxchg16b")
        && is_x86_feature_detected!("popcnt")
        && is_x86_feature_detected!("sse3")
        && is_x86_feature_detected!("ssse3")
        && is_x86_feature_detected!("sse4.1")
        && is_x86_feature_detected!("sse4.2")
        && is_x86_feature_detected!("avx")
        && is_x86_feature_detected!("avx2")
        && is_x86_feature_detected!("bmi1")
        && is_x86_feature_detected!("bmi2")
        && is_x86_feature_detected!("fma")
        && is_x86_feature_detected!("movbe")
        && is_x86_feature_detected!("lzcnt")
        && is_x86_feature_detected!("f16c")
        && is_x86_feature_detected!("xsave")
}

#[cfg(not(all(target_arch = "x86_64", target_os = "linux")))]
fn host_supports_v3() -> bool {
    false
}

fn error(message: impl Into<String>) -> InstallError {
    InstallError { message: message.into() }
}

/// Parses a `FRUST_VARIANT` value; unset and empty mean `auto`.
pub fn parse_variant_request(value: Option<&str>) -> Result<VariantRequest, InstallError> {
    match value {
        None | Some("") | Some("auto") => Ok(VariantRequest::Auto),
        Some("baseline") => Ok(VariantRequest::Baseline),
        Some("v3") => Ok(VariantRequest::V3),
        Some(other) => Err(error(format!("{VARIANT_ENV} must be auto, baseline or v3, not {other:?}"))),
    }
}

pub fn variant_request_from_environment() -> Result<VariantRequest, InstallError> {
    match env::var(VARIANT_ENV) {
        Ok(value) => parse_variant_request(Some(&value)),
        Err(env::VarError::NotPresent) => Ok(VariantRequest::Auto),
        Err(env::VarError::NotUnicode(_)) => Err(error(format!("{VARIANT_ENV} is not valid UTF-8"))),
    }
}

/// Whether releases carry more than one build for this host. Only Linux x86-64 does.
pub fn has_variants(os: &str, arch: &str) -> bool {
    matches!((os, arch), ("linux", "x86_64"))
}

/// An update keeps the variant that is installed unless the request names one. A recorded `baseline`
/// or `v3` turns `auto` into that variant, so a machine that was moved to another CPU is not switched
/// behind the user's back. No record (a fresh prefix, or one made before variants) leaves `auto`.
pub fn effective_request(request: VariantRequest, installed: Option<&str>) -> VariantRequest {
    match (request, installed) {
        (VariantRequest::Auto, Some("baseline")) => VariantRequest::Baseline,
        (VariantRequest::Auto, Some("v3")) => VariantRequest::V3,
        _ => request,
    }
}

/// Picks the build to install. `release_lists_v3` is asked only when the answer matters, so a host
/// without variants, a `baseline` request and an `auto` choice on a CPU without the v3 extensions
/// never probe the release for a v3 archive.
pub fn choose_variant(
    os: &str,
    arch: &str,
    request: VariantRequest,
    cpu: &dyn CpuProbe,
    release_lists_v3: impl FnOnce() -> bool,
) -> Result<Variant, InstallError> {
    if !has_variants(os, arch) {
        return match request {
            VariantRequest::V3 => Err(error(format!("{VARIANT_ENV}=v3 is only available for Linux on x86-64, not {os} on {arch}"))),
            _ => Ok(Variant::Baseline),
        };
    }
    match request {
        VariantRequest::Baseline => Ok(Variant::Baseline),
        VariantRequest::V3 => {
            if release_lists_v3() {
                Ok(Variant::V3)
            } else {
                Err(error(format!("this release has no v3 build; set {VARIANT_ENV}=baseline to use the baseline build")))
            }
        }
        VariantRequest::Auto => Ok(if cpu.supports_v3() && release_lists_v3() { Variant::V3 } else { Variant::Baseline }),
    }
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

/// The three files of one build: the baseline names are those of [`assets`], `v3` adds `-v3` to the target.
pub fn assets_for(target: &str, variant: Variant) -> Assets {
    assets(&format!("{target}{}", variant.target_suffix()))
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
    fetch_with(url, dest, restricted, false)
}

/// Whether a file exists at `url`, asked by downloading it. The downloader's own error output is hidden
/// because a missing file is an expected answer here.
fn exists_at(url: &str, dest: &Path, restricted: bool) -> bool {
    fetch_with(url, dest, restricted, true).is_ok()
}

fn fetch_with(url: &str, dest: &Path, restricted: bool, quiet: bool) -> Result<(), InstallError> {
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
    if quiet {
        command.stderr(Stdio::null());
    }
    let status = command.status().map_err(|err| error(format!("could not run the downloader: {err}")))?;
    if !status.success() {
        return Err(error(format!("could not download {url} ({status})")));
    }
    Ok(())
}

/// Installs the published toolchain, or reports that the active one already matches it. On hosts with
/// more than one build, `request` and the installed variant decide which one (see [`effective_request`]).
pub fn install_release(prefix: &Path, version: Option<&str>, request: VariantRequest) -> Result<Outcome, InstallError> {
    let target = host_target()?;
    let override_base = env::var(BASE_ENV).ok();
    let base = base_url(version, override_base.as_deref())?;
    let restricted = override_base.as_deref().is_none_or(str::is_empty);
    let downloads = Downloads::create()?;

    let request = effective_request(request, install::current_variant(prefix)?.as_deref());
    let v3_id = assets_for(target, Variant::V3).id;
    let variant = choose_variant(env::consts::OS, env::consts::ARCH, request, &HostCpu, || {
        exists_at(&format!("{base}/{v3_id}"), &downloads.dir.join(&v3_id), restricted)
    })?;
    if has_variants(env::consts::OS, env::consts::ARCH) {
        eprintln!("frust-install: using the {} build", variant.name());
        if variant == Variant::V3 && !HostCpu.supports_v3() {
            eprintln!("frust-install: warning: this CPU does not report every x86-64-v3 extension, so the v3 toolchain may not run here");
        }
    }
    let names = assets_for(target, variant);

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

    struct Cpu(bool);

    impl CpuProbe for Cpu {
        fn supports_v3(&self) -> bool {
            self.0
        }
    }

    const LINUX_X86: (&str, &str) = ("linux", "x86_64");

    fn choose(host: (&str, &str), request: VariantRequest, cpu: bool, listed: bool) -> Result<Variant, InstallError> {
        choose_variant(host.0, host.1, request, &Cpu(cpu), || listed)
    }

    #[test]
    fn the_v3_asset_names_extend_the_target() {
        let baseline = assets_for("x86_64-unknown-linux-gnu", Variant::Baseline);
        assert_eq!(baseline, assets("x86_64-unknown-linux-gnu"));
        let fast = assets_for("x86_64-unknown-linux-gnu", Variant::V3);
        assert_eq!(fast.archive, "frust-toolchain-x86_64-unknown-linux-gnu-v3.tar.gz");
        assert_eq!(fast.checksum, "frust-toolchain-x86_64-unknown-linux-gnu-v3.tar.gz.sha256");
        assert_eq!(fast.id, "frust-toolchain-x86_64-unknown-linux-gnu-v3.id");
        assert_eq!((Variant::Baseline.name(), Variant::V3.name()), ("baseline", "v3"));
    }

    #[test]
    fn auto_picks_v3_only_when_the_cpu_supports_it_and_the_release_lists_it() {
        assert_eq!(choose(LINUX_X86, VariantRequest::Auto, true, true).unwrap(), Variant::V3);
        assert_eq!(choose(LINUX_X86, VariantRequest::Auto, false, true).unwrap(), Variant::Baseline);
        assert_eq!(choose(LINUX_X86, VariantRequest::Auto, true, false).unwrap(), Variant::Baseline);
        assert_eq!(choose(LINUX_X86, VariantRequest::Auto, false, false).unwrap(), Variant::Baseline);
    }

    #[test]
    fn explicit_requests_override_the_cpu() {
        for cpu in [true, false] {
            assert_eq!(choose(LINUX_X86, VariantRequest::Baseline, cpu, true).unwrap(), Variant::Baseline);
            assert_eq!(choose(LINUX_X86, VariantRequest::V3, cpu, true).unwrap(), Variant::V3);
        }
        let err = choose(LINUX_X86, VariantRequest::V3, true, false).unwrap_err();
        assert!(err.message.contains("no v3 build") && err.message.contains("FRUST_VARIANT=baseline"), "{}", err.message);
    }

    #[test]
    fn the_release_is_probed_only_when_the_answer_can_matter() {
        let probe = || -> bool { panic!("the release must not be probed") };
        assert_eq!(choose_variant("linux", "x86_64", VariantRequest::Baseline, &Cpu(true), probe).unwrap(), Variant::Baseline);
        assert_eq!(choose_variant("linux", "x86_64", VariantRequest::Auto, &Cpu(false), probe).unwrap(), Variant::Baseline);
        for (os, arch) in [("macos", "aarch64"), ("macos", "x86_64"), ("linux", "aarch64")] {
            for request in [VariantRequest::Auto, VariantRequest::Baseline] {
                assert_eq!(choose_variant(os, arch, request, &Cpu(true), probe).unwrap(), Variant::Baseline, "{os} {arch}");
            }
        }
    }

    #[test]
    fn hosts_other_than_linux_x86_64_never_get_variants() {
        assert!(has_variants("linux", "x86_64"));
        for (os, arch) in [("macos", "x86_64"), ("macos", "aarch64"), ("linux", "aarch64"), ("windows", "x86_64")] {
            assert!(!has_variants(os, arch), "{os} {arch}");
            assert_eq!(choose((os, arch), VariantRequest::Auto, true, true).unwrap(), Variant::Baseline, "{os} {arch}");
            let err = choose((os, arch), VariantRequest::V3, true, true).unwrap_err();
            assert!(err.message.contains("FRUST_VARIANT=v3") && err.message.contains("Linux"), "{}", err.message);
        }
    }

    #[test]
    fn the_variant_environment_value_is_validated() {
        assert_eq!(parse_variant_request(None).unwrap(), VariantRequest::Auto);
        assert_eq!(parse_variant_request(Some("")).unwrap(), VariantRequest::Auto);
        assert_eq!(parse_variant_request(Some("auto")).unwrap(), VariantRequest::Auto);
        assert_eq!(parse_variant_request(Some("baseline")).unwrap(), VariantRequest::Baseline);
        assert_eq!(parse_variant_request(Some("v3")).unwrap(), VariantRequest::V3);
        for bad in ["V3", "v4", "x86-64-v3", " v3", "baseline ", "none", "1"] {
            let err = parse_variant_request(Some(bad)).unwrap_err();
            assert!(err.message.contains("FRUST_VARIANT") && err.message.contains("auto, baseline or v3"), "{bad:?}: {}", err.message);
        }
    }

    #[test]
    fn an_update_keeps_the_installed_variant_unless_the_request_names_one() {
        use VariantRequest::{Auto, Baseline, V3};
        assert_eq!(effective_request(Auto, Some("baseline")), Baseline);
        assert_eq!(effective_request(Auto, Some("v3")), V3);
        assert_eq!(effective_request(Auto, None), Auto);
        assert_eq!(effective_request(Auto, Some("something-new")), Auto);
        for installed in [None, Some("baseline"), Some("v3")] {
            assert_eq!(effective_request(Baseline, installed), Baseline);
            assert_eq!(effective_request(V3, installed), V3);
        }
    }

    #[test]
    fn the_host_probe_only_accepts_cpus_that_report_the_v3_extensions() {
        #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
        {
            if HostCpu.supports_v3() {
                assert!(std::arch::is_x86_feature_detected!("avx2"));
                assert!(std::arch::is_x86_feature_detected!("bmi1"));
                assert!(std::arch::is_x86_feature_detected!("bmi2"));
                assert!(std::arch::is_x86_feature_detected!("fma"));
                assert!(std::arch::is_x86_feature_detected!("movbe"));
                assert!(std::arch::is_x86_feature_detected!("lzcnt"));
                assert!(std::arch::is_x86_feature_detected!("f16c"));
            }
        }
        #[cfg(not(all(target_arch = "x86_64", target_os = "linux")))]
        assert!(!HostCpu.supports_v3());
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
