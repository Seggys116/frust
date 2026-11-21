use std::env;
use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use frust_cli::dist::{self, Change, Dist, Spec};
use frust_cli::install;
use frust_cli::release::{self, Outcome};

fn usage_text(name: &str) -> String {
    format!(
        "usage: {name} [install|update|upgrade|stage|rollback|uninstall] [--prefix DIR] [--version VERSION] [--archive FILE --checksum FILE]
       {name} show | which TOOL [--prefix DIR]
       {name} target add|remove TRIPLE... [--prefix DIR]
       {name} target list [--installed] [--prefix DIR]
       {name} component add|remove NAME... [--prefix DIR]
       {name} component list [--installed] [--prefix DIR]
  install, update   download and install the latest release for this host (--version picks one);
                    with --archive and --checksum, install a local archive instead.
                    Targets and components added with `target add` and `component add` are added
                    again to the new version
  stage             unpack a local archive without activating it
  rollback          switch back to the previous version
  uninstall         remove the installation
  show              print the installed version, variant, host, targets and components
  which TOOL        print the path of a tool of the installed toolchain
  target add        install the std of other targets (any target of the official Rust release, for
                    example wasm32-unknown-unknown or aarch64-apple-ios) into the installed toolchain;
                    remove deletes them again; list shows the installed and the available ones
  component add     install rust-docs, rust-src or llvm-tools from the official Rust release (one
                    that is already there is left alone); remove deletes them again; list shows the
                    components. cargo, rustc, rustdoc, clippy, rustfmt and rust-analyzer are built
                    into every fRust archive and are replaced by an update
With no command, {name} installs the latest release (frustup prints this help). The default prefix is
$FRUST_PREFIX, then $XDG_DATA_HOME/frust or ~/.local/share/frust (macOS: ~/Library/Application Support/fRust).
On Linux x86-64, $FRUST_VARIANT picks the build: auto (the default) installs the v3 build when this CPU
supports it and the release has one, baseline or v3 force one. install and update keep the installed
variant unless $FRUST_VARIANT names another. Targets and components come from
https://static.rust-lang.org/dist, or from $FRUST_DIST_BASE when set; every package is checked against the
sha256 of the official channel manifest."
    )
}

/// The name this program was started as: `frustup` behaves exactly like `frust-install` and says so.
fn program_name(argv0: Option<&OsStr>) -> &'static str {
    match argv0.map(Path::new).and_then(Path::file_name) {
        Some(name) if name == "frustup" => "frustup",
        _ => "frust-install",
    }
}

const USAGE_ERROR: u8 = 2;
const OPERATION_ERROR: u8 = 1;

fn name() -> &'static str {
    program_name(env::args_os().next().as_deref())
}

fn usage_error(message: &str) -> ExitCode {
    eprintln!("{}: {message}", name());
    eprintln!("{}", usage_text(name()));
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

/// The arguments of `target`, `component`, `show` and `which`: names, `--prefix DIR` and `--installed`.
#[derive(Default)]
struct Words {
    names: Vec<String>,
    prefix: Option<PathBuf>,
    installed_only: bool,
}

fn parse_words(args: impl Iterator<Item = OsString>) -> Result<Words, String> {
    let mut words = Words::default();
    let mut args = args;
    while let Some(arg) = args.next() {
        let text = arg.to_string_lossy().into_owned();
        match text.as_str() {
            "--prefix" => match args.next().filter(|value| !value.is_empty()) {
                Some(value) => words.prefix = Some(PathBuf::from(value)),
                None => return Err("--prefix requires a value".to_string()),
            },
            "--installed" => words.installed_only = true,
            other if other.starts_with("--") => return Err(format!("unknown argument {other}")),
            _ => words.names.push(text),
        }
    }
    Ok(words)
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
    eprintln!("{}: put {} on PATH to use it:", name(), prefix.join("current").join("bin").display());
    eprintln!("  export PATH=\"{}:$PATH\"", prefix.join("current").join("bin").display());
}

fn install_latest(prefix: &Path, version: Option<&str>, variant: release::VariantRequest) -> Result<String, install::InstallError> {
    match release::install_release(prefix, version, variant)? {
        Outcome::UpToDate(id) => {
            eprintln!("{}: {id} is already installed and up to date", name());
            Ok(format!("current={id}\n"))
        }
        Outcome::Installed { current, previous } => {
            eprintln!("{}: installed {current}{}", name(), previous.map(|id| format!(" (previous {id})")).unwrap_or_default());
            path_hint(prefix);
            Ok(format!("current={current}\n"))
        }
    }
}

/// Adds the targets and components the user installed earlier to the version that is now active.
/// Nothing here fails the install: a failure is reported, with the command that retries it.
fn reapply_extras(prefix: &Path) {
    if !dist::has_extras(prefix) {
        return;
    }
    eprintln!("{}: adding the targets and components you installed earlier to the new version", name());
    match dist::reapply(prefix, &Dist::from_environment()) {
        Ok(batch) => {
            for (spec, change) in &batch.done {
                eprintln!("{}: {spec}: {}", name(), describe(*change));
            }
            for (spec, err) in &batch.failed {
                let command = match spec {
                    Spec::Target(triple) => format!("target add {triple}"),
                    Spec::Component(component) => format!("component add {component}"),
                };
                eprintln!("{}: WARNING: could not add {spec} to the new version: {}", name(), err.message);
                eprintln!("{}:   retry with: {} {command}", name(), name());
            }
        }
        Err(err) => eprintln!("{}: WARNING: could not add your targets and components to the new version: {}", name(), err.message),
    }
}

fn describe(change: Change) -> &'static str {
    match change {
        Change::Installed => "installed",
        Change::AlreadyInstalled => "already installed",
        Change::Removed => "removed",
        Change::NotInstalled => "not installed",
    }
}

fn resolve_prefix(explicit: Option<PathBuf>) -> Result<PathBuf, ExitCode> {
    match explicit {
        Some(prefix) => Ok(prefix),
        None => release::prefix_from_environment().map_err(|err| usage_error(&err.message)),
    }
}

fn finish(batch: Result<dist::Batch, install::InstallError>) -> ExitCode {
    match batch {
        Ok(batch) => {
            for (spec, change) in &batch.done {
                eprintln!("{}: {spec}: {}", name(), describe(*change));
            }
            for (spec, err) in &batch.failed {
                eprintln!("error: {spec}: {}", err.message);
            }
            if batch.failed.is_empty() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(OPERATION_ERROR)
            }
        }
        Err(err) => {
            eprintln!("error: {}", err.message);
            ExitCode::from(OPERATION_ERROR)
        }
    }
}

/// `target` and `component`: add, remove and list.
fn extras_command(command: &str, args: impl Iterator<Item = OsString>) -> ExitCode {
    let mut args = args.peekable();
    let Some(action) = args.next().map(|arg| arg.to_string_lossy().into_owned()) else {
        return usage_error(&format!("{command} needs add, remove or list"));
    };
    if !matches!(action.as_str(), "add" | "remove" | "list") {
        return usage_error(&format!("unknown {command} command {action}"));
    }
    let words = match parse_words(args) {
        Ok(words) => words,
        Err(message) => return usage_error(&message),
    };
    let prefix = match resolve_prefix(words.prefix.clone()) {
        Ok(prefix) => prefix,
        Err(code) => return code,
    };
    let dist = Dist::from_environment();
    if action == "list" {
        if !words.names.is_empty() {
            return usage_error(&format!("{command} list takes no names"));
        }
        return list(command, &prefix, &dist, !words.installed_only);
    }
    if words.installed_only {
        return usage_error("--installed only goes with list");
    }
    if words.names.is_empty() {
        return usage_error(&format!("{command} {action} needs at least one name"));
    }
    let mut specs = Vec::new();
    for item in &words.names {
        let spec = if command == "target" { Spec::target(item) } else { Spec::component(item) };
        match spec {
            Ok(spec) => specs.push(spec),
            Err(err) => return usage_error(&err.message),
        }
    }
    if action == "add" {
        finish(dist::add(&prefix, &dist, &specs))
    } else {
        finish(dist::remove(&prefix, &dist, &specs))
    }
}

fn list(command: &str, prefix: &Path, dist: &Dist, with_available: bool) -> ExitCode {
    let mut out = String::new();
    let unavailable;
    if command == "target" {
        match dist::list_targets(prefix, dist, with_available) {
            Ok(list) => {
                unavailable = list.unavailable;
                for target in list.targets {
                    match (target.installed, with_available) {
                        (true, true) => out.push_str(&format!("{} (installed)\n", target.name)),
                        (true, false) => out.push_str(&format!("{}\n", target.name)),
                        (false, _) => out.push_str(&format!("{}\n", target.name)),
                    }
                }
            }
            Err(err) => return report(Err(err)),
        }
    } else {
        match dist::list_components(prefix, dist, with_available) {
            Ok(list) => {
                unavailable = list.unavailable;
                for component in list.components.iter().filter(|component| component.installed || with_available) {
                    if component.installed && with_available {
                        out.push_str(&format!("{} (installed)\n", component.name));
                    } else {
                        out.push_str(&format!("{}\n", component.name));
                    }
                }
            }
            Err(err) => return report(Err(err)),
        }
    }
    if let Some(reason) = unavailable {
        eprintln!("{}: could not fetch the list of available {command}s, showing the installed ones: {reason}", name());
    }
    report(Ok(out))
}

fn main() -> ExitCode {
    let frustup = name() == "frustup";
    let mut args = env::args_os().skip(1).peekable();
    let command = match args.peek().map(|arg| arg.to_string_lossy().into_owned()) {
        None if frustup => return usage_error("a command is required"),
        None => "install".to_string(),
        Some(first) if first == "-h" || first == "--help" || first == "help" => {
            println!("{}", usage_text(name()));
            return ExitCode::SUCCESS;
        }
        Some(first) if first.starts_with("--") => "install".to_string(),
        Some(first) => {
            args.next();
            first
        }
    };
    match command.as_str() {
        "target" | "component" => return extras_command(&command, args),
        "show" | "which" => {
            let words = match parse_words(args) {
                Ok(words) => words,
                Err(message) => return usage_error(&message),
            };
            let expected = usize::from(command == "which");
            if words.names.len() != expected || words.installed_only {
                return usage_error(&if expected == 1 { "which takes exactly one tool name".to_string() } else { "show takes no arguments".to_string() });
            }
            let prefix = match resolve_prefix(words.prefix) {
                Ok(prefix) => prefix,
                Err(code) => return code,
            };
            return report(if command == "show" {
                dist::show(&prefix)
            } else {
                dist::which(&prefix, &words.names[0]).map(|path| format!("{}\n", path.display()))
            });
        }
        "install" | "update" | "upgrade" | "stage" | "rollback" | "uninstall" => {}
        _ => return usage_error(&format!("unknown command {command}")),
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
    let prefix = match resolve_prefix(options.prefix.clone()) {
        Ok(prefix) => prefix,
        Err(code) => return code,
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
    if downloads && result.is_ok() {
        reapply_extras(&prefix);
    }
    report(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_program_name_follows_argv0() {
        assert_eq!(program_name(Some(OsStr::new("frustup"))), "frustup");
        assert_eq!(program_name(Some(OsStr::new("/home/me/.local/share/frust/current/bin/frustup"))), "frustup");
        assert_eq!(program_name(Some(OsStr::new("frust-install"))), "frust-install");
        assert_eq!(program_name(Some(OsStr::new("./frustup-extra"))), "frust-install");
        assert_eq!(program_name(Some(OsStr::new("myfrustup"))), "frust-install");
        assert_eq!(program_name(None), "frust-install");
    }

    #[test]
    fn the_usage_names_the_program() {
        assert!(usage_text("frustup").starts_with("usage: frustup "));
        assert!(!usage_text("frustup").contains("frust-install"));
        assert!(usage_text("frust-install").starts_with("usage: frust-install "));
    }
}
