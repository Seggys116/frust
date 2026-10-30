//! Reads just enough of a rustc command line to decide cacheability; unknown flags are hashed verbatim.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct RustcArgs {
    pub crate_name: String,
    pub out_dir: PathBuf,
    pub extra_filename: String,
    pub paths: Vec<(usize, PathArg)>,
    pub libs: Vec<NativeLib>,
    pub out_dir_args: Vec<usize>,
    /// The directory holds rustc's private session state, never an input to the outputs.
    pub incremental_args: Vec<usize>,
    /// The last `-C opt-level` value, `-O` counting as `2`. `None` means the default (0).
    pub opt_level: Option<String>,
}

impl RustcArgs {
    pub fn incremental(&self) -> bool {
        !self.incremental_args.is_empty()
    }

    pub fn unoptimised(&self) -> bool {
        self.opt_level.as_deref().is_none_or(|level| level == "0")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathArg {
    /// `name` is the argument text minus the path, so the key is independent of where the library lives.
    Extern { name: String, path: PathBuf },
    NativeDir(PathBuf),
    /// Missing ones that look like objects or libraries make the invocation uncacheable.
    MaybeFiles(Vec<PathBuf>),
    Target(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeLib {
    pub kind: String,
    pub modifiers: String,
    pub name: String,
}

impl NativeLib {
    pub fn bundled(&self) -> bool {
        self.kind == "static" && !self.modifiers.split(',').any(|m| m == "-bundle")
    }

    pub fn file_names(&self) -> Vec<String> {
        if self.modifiers.split(',').any(|m| m == "+verbatim") {
            return vec![self.name.clone()];
        }
        let n = &self.name;
        vec![
            format!("lib{n}.a"),
            format!("{n}.lib"),
            format!("lib{n}.so"),
            format!("lib{n}.dylib"),
            format!("lib{n}.tbd"),
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skip {
    NotACompile,
    UnsupportedEmit,
    NonUtf8,
    Unverifiable,
}

const SUPPORTED_EMIT: &[&str] = &["dep-info", "metadata", "link"];

/// `linker` names a program keyed by its text; `incremental` is rustc's session directory, excluded from the key.
const CODEGEN_NOT_PATHS: &[&str] =
    &["extra-filename", "metadata", "opt-level", "debuginfo", "codegen-units", "strip", "linker", "incremental"];

pub fn parse(args: &[OsString]) -> Result<RustcArgs, Skip> {
    let strings: Vec<String> =
        args.iter().map(|a| a.to_str().map(str::to_owned)).collect::<Option<_>>().ok_or(Skip::NonUtf8)?;

    let mut crate_name = None;
    let mut out_dir = None;
    let mut extra_filename = String::new();
    let mut emit: Option<Vec<String>> = None;
    let mut paths = Vec::new();
    let mut libs = Vec::new();
    let mut out_dir_args = Vec::new();
    let mut codegen_state = Codegen::default();

    let mut i = 0;
    while i < strings.len() {
        let arg = strings[i].as_str();
        let next = strings.get(i + 1).map(String::as_str);
        match arg {
            "-" => return Err(Skip::NotACompile),
            _ if arg.starts_with('@') => return Err(Skip::NotACompile),
            "--print" | "-o" | "--explain" => return Err(Skip::NotACompile),
            _ if arg.starts_with("--print=") || arg.starts_with("--explain=") => {
                return Err(Skip::NotACompile);
            }
            _ if arg.starts_with("-o") && arg.len() > 2 => return Err(Skip::NotACompile),
            "--sysroot" => return Err(Skip::Unverifiable),
            _ if arg.starts_with("--sysroot=") => return Err(Skip::Unverifiable),
            "--crate-name" => {
                crate_name = next.map(str::to_owned);
                i += 1;
            }
            _ if arg.starts_with("--crate-name=") => crate_name = Some(arg["--crate-name=".len()..].to_string()),
            "--out-dir" => {
                out_dir = next.map(PathBuf::from);
                out_dir_args.extend([i, i + 1]);
                i += 1;
            }
            _ if arg.starts_with("--out-dir=") => {
                out_dir = Some(PathBuf::from(&arg["--out-dir=".len()..]));
                out_dir_args.push(i);
            }
            "--emit" => {
                emit = next.map(split_list);
                i += 1;
            }
            _ if arg.starts_with("--emit=") => emit = Some(split_list(&arg["--emit=".len()..])),
            "--extern" => {
                if let Some((name, path)) = next.and_then(|v| v.split_once('=')) {
                    paths.push((i + 1, PathArg::Extern { name: name.to_string(), path: PathBuf::from(path) }));
                }
                i += 1;
            }
            _ if arg.starts_with("--extern=") => {
                if let Some((name, path)) = arg["--extern=".len()..].split_once('=') {
                    paths.push((i, PathArg::Extern { name: format!("--extern={name}"), path: PathBuf::from(path) }));
                }
            }
            "-L" => {
                if let Some(dir) = next.and_then(search_dir) {
                    paths.push((i + 1, PathArg::NativeDir(dir)));
                }
                i += 1;
            }
            _ if arg.starts_with("-L") => {
                let value = arg[2..].strip_prefix('=').unwrap_or(&arg[2..]);
                if let Some(dir) = search_dir(value) {
                    paths.push((i, PathArg::NativeDir(dir)));
                }
            }
            "-l" => {
                if let Some(value) = next {
                    libs.push(native_lib(value));
                }
                i += 1;
            }
            _ if arg.starts_with("-l") => libs.push(native_lib(&arg[2..])),
            "-C" | "--codegen" => {
                if let Some(value) = next {
                    codegen_state.option(&[i, i + 1], value, &mut extra_filename);
                    push_option_paths(i + 1, value, &mut paths);
                }
                i += 1;
            }
            _ if arg.starts_with("--codegen=") => {
                let value = &arg["--codegen=".len()..];
                codegen_state.option(&[i], value, &mut extra_filename);
                push_option_paths(i, value, &mut paths);
            }
            _ if arg.starts_with("-C") => {
                codegen_state.option(&[i], &arg[2..], &mut extra_filename);
                push_option_paths(i, &arg[2..], &mut paths);
            }
            "-O" => codegen_state.opt_level = Some("2".to_string()),
            "-Z" => {
                if let Some(value) = next {
                    push_option_paths(i + 1, value, &mut paths);
                }
                i += 1;
            }
            _ if arg.starts_with("-Z") => push_option_paths(i, &arg[2..], &mut paths),
            "--target" => {
                if let Some(value) = next {
                    paths.push((i + 1, PathArg::Target(value.to_string())));
                }
                i += 1;
            }
            _ if arg.starts_with("--target=") => paths.push((i, PathArg::Target(arg["--target=".len()..].to_string()))),
            "--cfg"
            | "--check-cfg"
            | "--crate-type"
            | "--edition"
            | "--cap-lints"
            | "--json"
            | "--error-format"
            | "--color"
            | "--diagnostic-width"
            | "--remap-path-prefix"
            | "-W"
            | "-A"
            | "-D"
            | "-F"
            | "--force-warn" => i += 1,
            _ => {}
        }
        i += 1;
    }

    let emit = emit.ok_or(Skip::UnsupportedEmit)?;
    if emit.is_empty()
        || !emit.iter().all(|e| SUPPORTED_EMIT.contains(&e.as_str()))
        || !emit.iter().any(|e| e == "dep-info")
    {
        return Err(Skip::UnsupportedEmit);
    }
    Ok(RustcArgs {
        crate_name: crate_name.ok_or(Skip::NotACompile)?,
        out_dir: out_dir.ok_or(Skip::NotACompile)?,
        extra_filename,
        paths,
        libs,
        out_dir_args,
        incremental_args: codegen_state.incremental_args,
        opt_level: codegen_state.opt_level,
    })
}

fn search_dir(value: &str) -> Option<PathBuf> {
    let (kind, dir) = match value.split_once('=') {
        Some((kind, dir)) if matches!(kind, "native" | "framework" | "all" | "dependency" | "crate") => (kind, dir),
        _ => ("all", value),
    };
    matches!(kind, "native" | "framework" | "all").then(|| PathBuf::from(dir))
}

fn native_lib(value: &str) -> NativeLib {
    let (spec, name) = match value.split_once('=') {
        Some((spec, name)) => (spec, name),
        None => ("", value),
    };
    let (kind, modifiers) = spec.split_once(':').unwrap_or((spec, ""));
    let name = name.split_once(':').map(|(n, _)| n).unwrap_or(name);
    NativeLib { kind: kind.to_string(), modifiers: modifiers.to_string(), name: name.to_string() }
}

fn push_option_paths(index: usize, value: &str, paths: &mut Vec<(usize, PathArg)>) {
    let Some((key, val)) = value.split_once('=') else {
        return;
    };
    if CODEGEN_NOT_PATHS.contains(&key) || val.is_empty() {
        return;
    }
    let linker_words = matches!(key, "link-arg" | "link-args" | "pre-link-arg" | "pre-link-args");
    if !linker_words {
        paths.push((index, PathArg::MaybeFiles(vec![PathBuf::from(val)])));
        return;
    }
    let words: Vec<&str> = if key.ends_with('s') { val.split_whitespace().collect() } else { vec![val] };
    let mut files = Vec::new();
    for word in words {
        let parts: Vec<&str> = match word.strip_prefix("-Wl,") {
            Some(list) => list.split(',').collect(),
            None => vec![word],
        };
        for part in parts {
            if let Some(dir) = part.strip_prefix("-L").filter(|d| !d.is_empty()) {
                paths.push((index, PathArg::NativeDir(PathBuf::from(dir))));
            } else if let Some(file) = part.strip_prefix("-T").filter(|f| !f.is_empty()) {
                files.push(PathBuf::from(file));
            } else if !part.starts_with('-') && !part.is_empty() {
                files.push(PathBuf::from(part));
            }
        }
    }
    if !files.is_empty() {
        paths.push((index, PathArg::MaybeFiles(files)));
    }
}

#[derive(Default)]
struct Codegen {
    incremental_args: Vec<usize>,
    opt_level: Option<String>,
}

impl Codegen {
    fn option(&mut self, indices: &[usize], value: &str, extra_filename: &mut String) {
        let (key, val) = value.split_once('=').unwrap_or((value, ""));
        match key {
            "incremental" => self.incremental_args.extend_from_slice(indices),
            "opt-level" => self.opt_level = Some(val.to_string()),
            "extra-filename" => *extra_filename = val.to_string(),
            _ => {}
        }
    }
}

fn split_list(value: &str) -> Vec<String> {
    value.split(',').map(str::to_owned).collect()
}

/// A missing file that looks like a link input must not be keyed by its path text alone.
pub fn looks_like_link_input(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    matches!(
        ext,
        "o" | "a"
            | "so"
            | "dylib"
            | "tbd"
            | "lib"
            | "obj"
            | "rlib"
            | "rmeta"
            | "res"
            | "ld"
            | "def"
            | "exp"
            | "json"
            | "profdata"
    ) || name.contains(".so.")
}

/// Paths under the profile root are rewritten to a token so a crate hashes identically in different target directories.
pub fn profile_root(out_dir: &Path) -> PathBuf {
    let name = out_dir.file_name().and_then(|n| n.to_str());
    if name == Some("deps") {
        return out_dir.parent().unwrap_or(out_dir).to_path_buf();
    }
    let parent = out_dir.parent();
    if parent.and_then(|p| p.file_name()).and_then(|n| n.to_str()) == Some("build")
        && let Some(root) = parent.and_then(|p| p.parent())
    {
        return root.to_path_buf();
    }
    out_dir.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    const BASE: [&str; 5] = ["--crate-name", "a", "--out-dir", "/o", "--emit=dep-info,link"];

    fn with(extra: &[&str]) -> Result<RustcArgs, Skip> {
        let mut args = BASE.to_vec();
        args.extend(extra);
        parse(&os(&args))
    }

    #[test]
    fn parses_a_cargo_style_invocation() {
        let parsed = parse(&os(&[
            "--crate-name",
            "serde",
            "src/lib.rs",
            "--emit=dep-info,metadata,link",
            "--out-dir",
            "/t/debug/deps",
            "-C",
            "extra-filename=-abc123",
            "--extern",
            "dep=/t/debug/deps/libdep-1.rlib",
            "-L",
            "native=/t/debug/build/x/out",
            "-L",
            "dependency=/t/debug/deps",
        ]))
        .unwrap();
        assert_eq!(parsed.crate_name, "serde");
        assert_eq!(parsed.extra_filename, "-abc123");
        assert_eq!(parsed.out_dir, PathBuf::from("/t/debug/deps"));
        assert_eq!(
            parsed.paths,
            vec![
                (9, PathArg::Extern { name: "dep".into(), path: "/t/debug/deps/libdep-1.rlib".into() }),
                (11, PathArg::NativeDir("/t/debug/build/x/out".into())),
            ]
        );
    }

    #[test]
    fn joined_forms_are_recognised() {
        let parsed =
            with(&["-Lnative=/n", "-L/plain", "--extern=dep=/d/libdep.rlib", "-lstatic=foo", "-lbar"]).unwrap();
        assert_eq!(
            parsed.paths,
            vec![
                (5, PathArg::NativeDir("/n".into())),
                (6, PathArg::NativeDir("/plain".into())),
                (7, PathArg::Extern { name: "--extern=dep".into(), path: "/d/libdep.rlib".into() }),
            ]
        );
        assert_eq!(parsed.libs.len(), 2);
        assert!(parsed.libs[0].bundled());
        assert!(!parsed.libs[1].bundled());
        assert_eq!(parsed.libs[1].name, "bar");
    }

    #[test]
    fn link_args_and_targets_report_paths() {
        let parsed =
            with(&["-C", "link-arg=/x/obj.o", "-Clink-args=-L/libs -Wl,-T,/s/script.ld foo.a", "--target", "my.json"])
                .unwrap();
        assert_eq!(
            parsed.paths,
            vec![
                (6, PathArg::MaybeFiles(vec!["/x/obj.o".into()])),
                (7, PathArg::NativeDir("/libs".into())),
                (7, PathArg::MaybeFiles(vec!["/s/script.ld".into(), "foo.a".into()])),
                (9, PathArg::Target("my.json".into())),
            ]
        );
    }

    #[test]
    fn static_lib_modifiers() {
        let lib = native_lib("static:-bundle,+whole-archive=foo:renamed");
        assert_eq!((lib.kind.as_str(), lib.name.as_str()), ("static", "foo"));
        assert!(!lib.bundled());
        assert_eq!(native_lib("static:+verbatim=libx.a").file_names(), vec!["libx.a".to_string()]);
    }

    #[test]
    fn incremental_and_opt_level_are_reported() {
        let parsed = with(&["-C", "incremental=/t/debug/incremental", "-Cdebuginfo=2"]).unwrap();
        assert_eq!(parsed.incremental_args, vec![5, 6]);
        assert!(parsed.incremental() && parsed.unoptimised());
        assert!(parsed.paths.is_empty(), "the session directory is not an input");
        let joined = with(&["-Cincremental=/i", "--codegen=opt-level=0"]).unwrap();
        assert_eq!(joined.incremental_args, vec![5]);
        assert!(joined.unoptimised());
        assert!(!with(&["--codegen", "incremental=/i", "-C", "opt-level=1"]).unwrap().unoptimised());
        assert!(!with(&["-O"]).unwrap().unoptimised());
        assert!(!with(&["-Copt-level=s"]).unwrap().unoptimised());
        assert!(!with(&[]).unwrap().incremental());
    }

    #[test]
    fn exotic_emit_and_sysroot_are_skipped() {
        assert_eq!(with(&["--sysroot", "/s"]).unwrap_err(), Skip::Unverifiable);
        assert_eq!(with(&["--sysroot=/s"]).unwrap_err(), Skip::Unverifiable);
        assert_eq!(
            parse(&os(&["--crate-name", "a", "--out-dir", "/o", "--emit=asm,dep-info"])).unwrap_err(),
            Skip::UnsupportedEmit
        );
        assert_eq!(parse(&os(&["--print", "cfg"])).unwrap_err(), Skip::NotACompile);
        assert_eq!(parse(&os(&["-vV"])).unwrap_err(), Skip::UnsupportedEmit);
    }

    #[test]
    fn profile_root_handles_deps_and_build_scripts() {
        assert_eq!(profile_root(Path::new("/t/debug/deps")), PathBuf::from("/t/debug"));
        assert_eq!(profile_root(Path::new("/t/debug/build/foo-1")), PathBuf::from("/t/debug"));
        assert_eq!(profile_root(Path::new("/t/x86/debug/deps")), PathBuf::from("/t/x86/debug"));
    }
}
