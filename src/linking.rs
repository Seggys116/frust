//! Opt-in raw Apple linking for unoptimised incremental macOS binaries.

use std::env;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use crate::cache::rustc_args;
use crate::{sys, toolchain};

const DRIVER_ENV: &[&str] = &[
    "LIBRARY_PATH",
    "COMPILER_PATH",
    "GCC_EXEC_PREFIX",
    "CCC_OVERRIDE_OPTIONS",
    "TOOLCHAINS",
    "LDFLAGS",
];

/// Whether the user explicitly selected this policy. Other values leave linking unchanged.
pub fn requested() -> bool {
    is_requested(env::var_os("FRUST_LINK").as_deref())
}

/// Whether this platform has the native linker policy.
pub fn supported() -> bool {
    cfg!(target_os = "macos")
}

/// Prepares compiler-first argv; unsupported invocations remain unchanged.
pub fn prepare(args: &[OsString], debug: bool) -> Option<Vec<OsString>> {
    prepare_with(args, debug, requested(), supported(), resolve_linker)
}

fn is_requested(value: Option<&OsStr>) -> bool {
    value == Some(OsStr::new("smart"))
}

fn prepare_with(
    args: &[OsString],
    debug: bool,
    requested: bool,
    supported: bool,
    resolve: impl FnOnce() -> Result<PathBuf, &'static str>,
) -> Option<Vec<OsString>> {
    if !requested {
        return None;
    }
    let prepared = (|| {
        if !supported {
            return Err("unsupported platform");
        }
        eligible(args)?;
        resolve()
    })();
    match prepared {
        Ok(linker) => {
            let mut effective = args.to_vec();
            let mut flag = OsString::from("-Clinker=");
            flag.push(linker.as_os_str());
            effective.push(flag);
            // Stable rustc's legacy `ld` flavor selects the native raw linker for the host target.
            effective.push(OsString::from("-Clinker-flavor=ld"));
            if debug {
                eprintln!("frust-wrap: smart link: using {}", linker.display());
            }
            Some(effective)
        }
        Err(reason) => {
            if debug {
                eprintln!("frust-wrap: link policy skipped: {reason}");
            }
            None
        }
    }
}

fn eligible(args: &[OsString]) -> Result<(), &'static str> {
    let strings = args.iter().map(|arg| arg.to_str()).collect::<Option<Vec<_>>>().ok_or("non-UTF-8 arguments")?;
    if strings.len() < 2 || !strings[1].starts_with('-') {
        return Err("not a rustc invocation");
    }
    let parsed = rustc_args::parse(&args[1..]).map_err(|_| "not a supported compile")?;
    if parsed.crate_name.starts_with("build_script") {
        return Err("build script");
    }
    if !parsed.incremental() || !parsed.unoptimised() {
        return Err("requires an unoptimised incremental compile");
    }

    let mut binary = false;
    let mut link = false;
    let mut incremental = false;
    let mut unoptimised = true;
    let mut i = 1;
    while i < strings.len() {
        let arg = strings[i];
        let next = strings.get(i + 1).copied();
        match arg {
            "--test" | "--target" | "--sysroot" | "--" | "--help" | "-h" | "--version" | "-V" | "-vV" => {
                return Err("custom target, test, or non-compile invocation");
            }
            _ if arg.starts_with('@')
                || arg.starts_with("--target=")
                || arg.starts_with("--sysroot=")
                || arg.starts_with("--test=") =>
            {
                return Err("custom target or response file");
            }
            "--crate-type" => {
                if next != Some("bin") {
                    return Err("requires only an explicit binary crate type");
                }
                binary = true;
                i += 1;
            }
            _ if arg.starts_with("--crate-type=") => {
                if arg.strip_prefix("--crate-type=") != Some("bin") {
                    return Err("requires only an explicit binary crate type");
                }
                binary = true;
            }
            "--emit" => {
                link = next.is_some_and(emits_link);
                i += 1;
            }
            _ if arg.starts_with("--emit=") => link = emits_link(&arg["--emit=".len()..]),
            "-O" => unoptimised = false,
            "-C" | "--codegen" => {
                inspect_codegen(next.ok_or("missing codegen option")?, &mut incremental, &mut unoptimised)?;
                i += 1;
            }
            _ if arg.starts_with("--codegen=") => {
                inspect_codegen(&arg["--codegen=".len()..], &mut incremental, &mut unoptimised)?;
            }
            _ if arg.starts_with("-C") => {
                inspect_codegen(&arg[2..], &mut incremental, &mut unoptimised)?;
            }
            "-Z" => {
                if !next.is_some_and(time_passes) {
                    return Err("unstable option");
                }
                i += 1;
            }
            _ if arg.starts_with("-Z") && !time_passes(&arg[2..]) => {
                return Err("unstable option");
            }
            _ => {}
        }
        i += 1;
    }
    if !binary || !link || !incremental || !unoptimised {
        return Err("requires an explicit incremental debug binary link");
    }
    Ok(())
}

fn emits_link(emit: &str) -> bool {
    emit.split(',').any(|kind| kind == "link")
}

fn time_passes(value: &str) -> bool {
    // Timing diagnostics let benchmarks observe linker work without changing code generation.
    matches!(
        value.strip_prefix('=').unwrap_or(value),
        "time-passes"
            | "time-passes=yes"
            | "time-passes=on"
            | "time-passes=true"
            | "time-passes=1"
            | "time-passes=no"
            | "time-passes=off"
            | "time-passes=false"
            | "time-passes=0"
    )
}

fn inspect_codegen(value: &str, incremental: &mut bool, unoptimised: &mut bool) -> Result<(), &'static str> {
    let value = value.strip_prefix('=').unwrap_or(value);
    let (key, value) = value.split_once('=').unwrap_or((value, ""));
    let key = key.replace('_', "-");
    match key.as_str() {
        "linker"
        | "linker-flavor"
        | "link-arg"
        | "link-args"
        | "pre-link-arg"
        | "pre-link-args"
        | "prefer-dynamic"
        | "rpath"
        | "link-self-contained"
        | "linker-plugin-lto" => return Err("explicit linker option"),
        "incremental" => *incremental = !value.is_empty(),
        "opt-level" => *unoptimised = value == "0",
        _ => {}
    }
    Ok(())
}

fn driver_environment_supported(mut get: impl FnMut(&str) -> Option<OsString>) -> bool {
    DRIVER_ENV.iter().all(|name| get(name).is_none_or(|value| value.is_empty()))
}

fn default_driver_is_system(found: Option<&Path>, canonicalize: impl Fn(&Path) -> Option<PathBuf>) -> bool {
    let Some(found) = found.and_then(&canonicalize) else {
        return false;
    };
    canonicalize(Path::new("/usr/bin/cc")).is_some_and(|system| found == system)
}

fn resolve_linker() -> Result<PathBuf, &'static str> {
    if !driver_environment_supported(|name| env::var_os(name)) {
        return Err("custom compiler driver environment");
    }
    let driver = toolchain::find_on_path("cc");
    if !default_driver_is_system(driver.as_deref(), |path| sys::canonicalize(path).ok()) {
        return Err("default cc is not the system driver");
    }
    discover_linker().ok_or("native linker discovery failed")
}

#[cfg(not(target_os = "macos"))]
fn discover_linker() -> Option<PathBuf> {
    None
}

#[cfg(target_os = "macos")]
fn discover_linker() -> Option<PathBuf> {
    use std::process::{Command, Stdio};
    use std::time::Instant;

    let started = Instant::now();
    let mut child = Command::new("/usr/bin/xcrun")
        .args(["--find", "ld"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let output = child.stdout.take().and_then(|stdout| read_discovery(&mut child, &mut sys::Pipe::from(stdout), started));
    if output.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    linker_path(&output?, |path| path.is_file())
}

#[cfg(target_os = "macos")]
fn read_discovery(child: &mut std::process::Child, stdout: &mut sys::Pipe, started: std::time::Instant) -> Option<Vec<u8>> {
    use std::io;
    use std::time::Duration;

    const TIMEOUT: Duration = Duration::from_secs(2);
    const MAX_OUTPUT: usize = 64 * 1024;
    let mut output = Vec::new();
    let mut eof = false;
    let mut chunk = [0; 4096];
    loop {
        if started.elapsed() >= TIMEOUT {
            return None;
        }
        if !eof {
            match stdout.read(&mut chunk) {
                Ok(0) => eof = true,
                Ok(n) => {
                    if output.len() + n > MAX_OUTPUT {
                        return None;
                    }
                    output.extend_from_slice(&chunk[..n]);
                    continue;
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => return None,
            }
        }
        match child.try_wait().ok()? {
            Some(status) if !status.success() => return None,
            Some(_) if eof => return Some(output),
            _ => {}
        }
        let wait = TIMEOUT.saturating_sub(started.elapsed()).min(Duration::from_millis(10));
        if eof {
            std::thread::sleep(wait);
        } else {
            sys::wait_readable([Some(stdout), None], wait);
        }
    }
}

#[cfg(any(target_os = "macos", test))]
fn linker_path(output: &[u8], is_file: impl FnOnce(&Path) -> bool) -> Option<PathBuf> {
    let output = std::str::from_utf8(output).ok()?;
    let line = output.strip_suffix('\n').unwrap_or(output);
    let line = line.strip_suffix('\r').unwrap_or(line);
    if line.contains(['\r', '\n', '\0']) {
        return None;
    }
    let path = PathBuf::from(line);
    (path.is_absolute() && is_file(&path)).then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn compile() -> Vec<OsString> {
        os(&[
            "rustc",
            "--crate-name",
            "app",
            "src/main.rs",
            "--crate-type=bin",
            "--emit=dep-info,link",
            "--out-dir",
            "/target/debug/deps",
            "-Cincremental=/target/debug/incremental",
        ])
    }

    fn codegen_forms(option: &str) -> Vec<Vec<OsString>> {
        vec![
            os(&["-C", option]),
            os(&[&format!("-C{option}")]),
            os(&["--codegen", option]),
            os(&[&format!("--codegen={option}")]),
        ]
    }

    #[test]
    fn smart_is_an_exact_opt_in() {
        assert!(!is_requested(None));
        for value in ["", "off", "on", "SMART", " smart", "smart "] {
            assert!(!is_requested(Some(OsStr::new(value))), "{value:?}");
        }
        assert!(is_requested(Some(OsStr::new("smart"))));
        assert_eq!(supported(), cfg!(target_os = "macos"));
    }

    #[test]
    fn disabled_and_unsupported_modes_do_not_resolve_a_linker() {
        for (requested, supported) in [(false, false), (false, true), (true, false)] {
            assert_eq!(prepare_with(&[], false, requested, supported, || panic!("discovery must be skipped")), None);
        }
    }

    #[test]
    fn supported_debug_binary_compiles_are_eligible() {
        assert_eq!(eligible(&compile()), Ok(()));
        let mut args = compile();
        args[4] = "--crate-type".into();
        args.insert(5, "bin".into());
        assert_eq!(eligible(&args), Ok(()));
        for form in codegen_forms("incremental=/sessions with spaces") {
            let mut args = compile();
            args.pop();
            args.extend(form);
            assert_eq!(eligible(&args), Ok(()));
        }
    }

    #[test]
    fn metadata_and_non_binary_compiles_are_skipped() {
        for emit in ["--emit=dep-info,metadata", "--emit=dep-info", "--emit=metadata", "--emit=asm,link"] {
            let mut args = compile();
            args[5] = emit.into();
            assert!(eligible(&args).is_err(), "{emit}");
        }
        for kind in ["lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro", "bin,rlib", ""] {
            for extra in [os(&["--crate-type", kind]), os(&[&format!("--crate-type={kind}")])] {
                let mut args = compile();
                args.extend(extra);
                assert!(eligible(&args).is_err(), "{kind}");
            }
        }
        let mut implicit = compile();
        implicit.remove(4);
        assert!(eligible(&implicit).is_err());
    }

    #[test]
    fn effective_emit_must_include_link() {
        let mut args = compile();
        args.extend(os(&["--emit", "dep-info,metadata"]));
        assert!(eligible(&args).is_err());
        args.extend(os(&["--emit", "dep-info,metadata,link"]));
        assert_eq!(eligible(&args), Ok(()));
    }

    #[test]
    fn all_codegen_spellings_preserve_explicit_linker_choices() {
        for option in [
            "linker=/usr/bin/cc",
            "linker-flavor=darwin-cc",
            "link-arg=-dead_strip",
            "link-args=-dead_strip -no_uuid",
            "pre-link-arg=-v",
            "pre-link-args=-v -v",
            "prefer-dynamic",
            "prefer-dynamic=no",
            "rpath",
            "rpath=no",
            "link-self-contained=no",
            "linker-plugin-lto=no",
            "linker_flavor=darwin-cc",
        ] {
            for form in codegen_forms(option) {
                let mut args = compile();
                args.extend(form.clone());
                assert!(eligible(&args).is_err(), "{form:?}");
            }
        }
        let mut args = compile();
        args.push("-C=linker=/usr/bin/cc".into());
        assert!(eligible(&args).is_err());
    }

    #[test]
    fn optimisation_and_disabled_incremental_are_skipped() {
        for option in ["opt-level=1", "opt-level=2", "opt-level=3", "opt-level=s", "opt-level=z", "incremental="] {
            for form in codegen_forms(option) {
                let mut args = compile();
                args.extend(form.clone());
                assert!(eligible(&args).is_err(), "{form:?}");
            }
        }
        let mut args = compile();
        args.push("-O".into());
        assert!(eligible(&args).is_err());
        args.pop();
        args.pop();
        assert!(eligible(&args).is_err());
        for form in codegen_forms("opt-level=0") {
            let mut args = compile();
            args.extend(form);
            assert_eq!(eligible(&args), Ok(()));
        }
        let mut args = compile();
        args.push("-C=opt-level=3".into());
        assert!(eligible(&args).is_err());
    }

    #[test]
    fn tests_build_scripts_targets_and_probes_are_skipped() {
        for flags in [
            vec!["--test"],
            vec!["--target", "aarch64-apple-darwin"],
            vec!["--target=aarch64-apple-darwin"],
            vec!["--sysroot", "/custom"],
            vec!["--sysroot=/custom"],
            vec!["@response"],
            vec!["--print", "cfg"],
            vec!["-vV"],
            vec!["--version"],
            vec!["--help"],
            vec!["-Z", "unstable-options"],
            vec!["-Z", "time-passes=invalid"],
            vec!["-Ztime-passes=yes,unstable-options"],
        ] {
            let mut args = compile();
            args.extend(os(&flags));
            assert!(eligible(&args).is_err(), "{flags:?}");
        }
        for name in ["build_script_build", "build_script_custom", "build_script"] {
            let mut args = compile();
            args[2] = name.into();
            assert!(eligible(&args).is_err(), "{name}");
        }
        let mut chain = compile();
        chain.insert(1, "clippy-driver".into());
        assert!(eligible(&chain).is_err());
        assert!(eligible(&os(&["rustc", "-vV"])).is_err());
    }

    #[test]
    fn diagnostic_timing_flags_do_not_change_eligibility() {
        for value in ["time-passes", "time-passes=yes", "time-passes=on", "time-passes=no", "time-passes=false"] {
            for flags in [os(&["-Z", value]), os(&[&format!("-Z{value}")]), os(&[&format!("-Z={value}")])] {
                let mut args = compile();
                args.extend(flags.clone());
                assert_eq!(eligible(&args), Ok(()), "{flags:?}");
            }
        }
    }

    #[test]
    fn ineligible_compiles_and_failed_discovery_leave_argv_unchanged() {
        let mut args = compile();
        args.push("--test".into());
        assert_eq!(prepare_with(&args, false, true, true, || panic!("discovery must be skipped")), None);
        assert_eq!(prepare_with(&compile(), false, true, true, || Err("unavailable")), None);
    }

    #[test]
    fn custom_driver_environment_is_declined() {
        assert!(driver_environment_supported(|_| None));
        assert!(driver_environment_supported(|_| Some(OsString::new())));
        for name in DRIVER_ENV {
            assert!(!driver_environment_supported(|key| (key == *name).then(|| OsString::from("custom"))), "{name}");
        }
    }

    #[test]
    fn path_resolved_cc_must_canonicalize_to_the_system_driver() {
        let canonicalize = |path: &Path| match path.to_str()? {
            "/usr/bin/cc" | "/alias/cc" => Some(PathBuf::from("/system/clang")),
            "/custom/cc" => Some(PathBuf::from("/custom/clang")),
            _ => None,
        };
        assert!(default_driver_is_system(Some(Path::new("/usr/bin/cc")), canonicalize));
        assert!(default_driver_is_system(Some(Path::new("/alias/cc")), canonicalize));
        assert!(!default_driver_is_system(Some(Path::new("/custom/cc")), canonicalize));
        assert!(!default_driver_is_system(Some(Path::new("/missing/cc")), canonicalize));
        assert!(!default_driver_is_system(None, canonicalize));
        assert!(!default_driver_is_system(Some(Path::new("/usr/bin/cc")), |_| None));
    }

    #[test]
    fn native_linker_paths_with_spaces_stay_in_one_argument() {
        let linker = PathBuf::from("/Developer Tools/Toolchains/Default.xctoolchain/usr/bin/ld");
        let args = compile();
        let prepared = prepare_with(&args, false, true, true, || Ok(linker)).unwrap();
        assert_eq!(&prepared[..args.len()], &args);
        assert_eq!(prepared.len(), args.len() + 2);
        assert_eq!(prepared[args.len()], OsString::from("-Clinker=/Developer Tools/Toolchains/Default.xctoolchain/usr/bin/ld"));
        assert_eq!(prepared[args.len() + 1], OsString::from("-Clinker-flavor=ld"));
    }

    #[test]
    fn discovery_output_requires_one_absolute_existing_file() {
        let absolute = PathBuf::from(if cfg!(windows) { r"C:\Developer Tools\ld" } else { "/Developer Tools/ld" });
        let mut output = absolute.to_str().unwrap().as_bytes().to_vec();
        output.push(b'\n');
        assert_eq!(linker_path(&output, |path| path == absolute), Some(absolute));
        assert_eq!(linker_path(&output, |_| false), None);
        for output in [b"ld\n".as_slice(), b"\n", b"/ld\n/other\n", b"/ld\0", b"\xff"] {
            assert_eq!(linker_path(output, |_| true), None, "{output:?}");
        }
    }

    #[test]
    fn non_utf8_arguments_are_declined() {
        let non_utf8 = sys::non_utf8_os_string();
        let mut args = compile();
        args.push(non_utf8.clone());
        assert!(eligible(&args).is_err());
        args.pop();
        args[0] = non_utf8.clone();
        assert!(eligible(&args).is_err());
        assert!(!driver_environment_supported(|_| Some(non_utf8.clone())));
    }
}
