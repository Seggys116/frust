use std::env;
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use frust_cli::install;
use frust_cli::release::{self, Outcome};

const USAGE: &str = "usage: frust-install [install|update|upgrade|stage|rollback|uninstall] [--prefix DIR] [--version VERSION] [--archive FILE --checksum FILE]
  install, update   download and install the latest release for this host (--version picks one);
                    with --archive and --checksum, install a local archive instead
  stage             unpack a local archive without activating it
  rollback          switch back to the previous version
  uninstall         remove the installation
With no command, frust-install installs the latest release. The default prefix is
$FRUST_PREFIX, then $XDG_DATA_HOME/frust or ~/.local/share/frust (macOS: ~/Library/Application Support/fRust).
On Linux x86-64, $FRUST_VARIANT picks the build: auto (the default) installs the v3 build when this CPU
supports it and the release has one, baseline or v3 force one. install and update keep the installed
variant unless $FRUST_VARIANT names another.";
const USAGE_ERROR: u8 = 2;
const OPERATION_ERROR: u8 = 1;

fn usage_error(message: &str) -> ExitCode {
    eprintln!("frust-install: {message}");
    eprintln!("{USAGE}");
    ExitCode::from(USAGE_ERROR)
}

#[derive(Default)]
struct Options {
    prefix: Option<PathBuf>,
    version: Option<String>,
    archive: Option<PathBuf>,
    checksum: Option<PathBuf>,
}

fn parse_options(args: impl Iterator<Item = OsString>) -> Result<Options, String> {
    let mut options = Options::default();
    let mut args = args;
    while let Some(arg) = args.next() {
        let name = arg.to_string_lossy().into_owned();
        if !matches!(name.as_str(), "--prefix" | "--version" | "--archive" | "--checksum") {
            return Err(format!("unknown argument {name}"));
        }
        let Some(value) = args.next().filter(|value| !value.is_empty()) else {
            return Err(format!("{name} requires a value"));
        };
        match name.as_str() {
            "--prefix" => options.prefix = Some(PathBuf::from(value)),
            "--version" => options.version = Some(value.to_string_lossy().into_owned()),
            "--archive" => options.archive = Some(PathBuf::from(value)),
            _ => options.checksum = Some(PathBuf::from(value)),
        }
    }
    Ok(options)
}

fn report(result: Result<String, install::InstallError>) -> ExitCode {
    match result {
        Ok(text) => match std::io::stdout().write_all(text.as_bytes()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("error: could not write output: {err}");
                ExitCode::from(OPERATION_ERROR)
            }
        },
        Err(err) => {
            eprintln!("error: {}", err.message);
            ExitCode::from(OPERATION_ERROR)
        }
    }
}

fn path_hint(prefix: &Path) {
    eprintln!("frust-install: put {} on PATH to use it:", prefix.join("current").join("bin").display());
    eprintln!("  export PATH=\"{}:$PATH\"", prefix.join("current").join("bin").display());
}

fn install_latest(prefix: &Path, version: Option<&str>, variant: release::VariantRequest) -> Result<String, install::InstallError> {
    match release::install_release(prefix, version, variant)? {
        Outcome::UpToDate(id) => {
            eprintln!("frust-install: {id} is already installed and up to date");
            Ok(format!("current={id}\n"))
        }
        Outcome::Installed { current, previous } => {
            eprintln!("frust-install: installed {current}{}", previous.map(|id| format!(" (previous {id})")).unwrap_or_default());
            path_hint(prefix);
            Ok(format!("current={current}\n"))
        }
    }
}

fn main() -> ExitCode {
    let mut args = env::args_os().skip(1).peekable();
    let command = match args.peek().map(|arg| arg.to_string_lossy().into_owned()) {
        None => "install".to_string(),
        Some(first) if first == "-h" || first == "--help" || first == "help" => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Some(first) if first.starts_with("--") => "install".to_string(),
        Some(first) => {
            args.next();
            first
        }
    };
    if !matches!(command.as_str(), "install" | "update" | "upgrade" | "stage" | "rollback" | "uninstall") {
        return usage_error(&format!("unknown command {command}"));
    }
    let options = match parse_options(args) {
        Ok(options) => options,
        Err(message) => return usage_error(&message),
    };
    let local = match (&options.archive, &options.checksum) {
        (Some(archive), Some(checksum)) => Some((archive.clone(), checksum.clone())),
        (None, None) => None,
        _ => return usage_error("--archive and --checksum go together"),
    };
    if local.is_some() && options.version.is_some() {
        return usage_error("--version picks a release to download and cannot be combined with --archive");
    }
    let downloads = matches!(command.as_str(), "install" | "update" | "upgrade");
    if !downloads && (options.version.is_some() || (command != "stage" && local.is_some())) {
        return usage_error(&format!("{command} does not take that option"));
    }
    if command == "stage" && local.is_none() {
        return usage_error("stage requires --archive and --checksum");
    }
    let prefix = match options.prefix.clone() {
        Some(prefix) => prefix,
        None => match release::prefix_from_environment() {
            Ok(prefix) => prefix,
            Err(err) => return usage_error(&err.message),
        },
    };
    // FRUST_VARIANT only matters for a download; a local archive carries its own variant.
    let variant = if downloads && local.is_none() {
        match release::variant_request_from_environment() {
            Ok(variant) => variant,
            Err(err) => return usage_error(&err.message),
        }
    } else {
        release::VariantRequest::default()
    };
    let result = match (command.as_str(), local) {
        ("install" | "update" | "upgrade", Some((archive, checksum))) => {
            install::install(&prefix, &archive, &checksum).map(|state| format!("current={}\n", state.current.unwrap_or_default()))
        }
        ("install" | "update" | "upgrade", None) => install_latest(&prefix, options.version.as_deref(), variant),
        ("stage", Some((archive, checksum))) => install::stage_without_activate(&prefix, &archive, &checksum).map(|path| {
            let current = install::current_version(&prefix).ok().flatten().unwrap_or_default();
            format!("partial={}\ncurrent={current}\n", path.display())
        }),
        ("rollback", _) => install::rollback(&prefix).map(|state| format!("current={}\n", state.current.unwrap_or_default())),
        _ => install::uninstall(&prefix).map(|()| String::new()),
    };
    report(result)
}
