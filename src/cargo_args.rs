//! Flags are deliberately not modelled and everything is kept verbatim, so frust stays independent of the cargo version.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

const GLOBAL_WITH_VALUE: &[&str] = &["-C", "-Z", "--config", "--color"];

#[derive(Debug, Clone, Default)]
pub struct CargoArgs {
    pub toolchain: Option<OsString>,
    pub globals: Vec<OsString>,
    pub subcommand: Option<String>,
    pub rest: Vec<OsString>,
}

impl CargoArgs {
    pub fn parse<I: IntoIterator<Item = OsString>>(args: I) -> CargoArgs {
        let mut parsed = CargoArgs::default();
        let mut iter = args.into_iter().peekable();
        if let Some(first) = iter.peek()
            && first.to_string_lossy().starts_with('+') {
                parsed.toolchain = iter.next();
            }
        while let Some(arg) = iter.next() {
            let text = arg.to_string_lossy().into_owned();
            if text.starts_with('-') {
                let takes_value = GLOBAL_WITH_VALUE.contains(&text.as_str());
                parsed.globals.push(arg);
                if takes_value
                    && let Some(value) = iter.next() {
                        parsed.globals.push(value);
                    }
            } else {
                parsed.subcommand = Some(text);
                parsed.rest = iter.collect();
                break;
            }
        }
        parsed
    }

    pub fn split_rest(&self) -> (&[OsString], &[OsString]) {
        match self.rest.iter().position(|a| a == "--") {
            Some(i) => (&self.rest[..i], &self.rest[i..]),
            None => (&self.rest[..], &[]),
        }
    }

    pub fn has_flag(&self, names: &[&str]) -> bool {
        let (own, _) = self.split_rest();
        own.iter().chain(self.globals.iter()).any(|a| {
            let text = a.to_string_lossy();
            names.iter().any(|n| text == *n || text.strip_prefix(n).is_some_and(|r| r.starts_with('=')))
        })
    }

    pub fn value_of(&self, name: &str) -> Option<OsString> {
        let (own, _) = self.split_rest();
        let all: Vec<&OsString> = self.globals.iter().chain(own.iter()).collect();
        for (i, arg) in all.iter().enumerate() {
            let text = arg.to_string_lossy();
            if text == name {
                return all.get(i + 1).map(|v| (*v).clone());
            }
            if let Some(value) = text.strip_prefix(name).and_then(|r| r.strip_prefix('=')) {
                return Some(OsString::from(value));
            }
        }
        None
    }

    pub fn chdir(&self) -> Option<PathBuf> {
        let mut i = 0;
        while i < self.globals.len() {
            if self.globals[i] == "-C" {
                return self.globals.get(i + 1).map(PathBuf::from);
            }
            i += 1;
        }
        None
    }

    /// Removes every occurrence of a frust-only flag from the arguments before `--` and reports whether one was there.
    pub fn strip_flag(&mut self, name: &str) -> bool {
        let end = self.rest.iter().position(|a| a == "--").unwrap_or(self.rest.len());
        let before = self.rest.len();
        let mut index = 0;
        self.rest.retain(|a| {
            let drop = index < end && a == name;
            index += 1;
            !drop
        });
        self.rest.len() != before
    }

    /// False for any `clean` that cargo scopes below the whole target directory, or that deletes nothing.
    pub fn cleans_whole_target(&self) -> bool {
        let (own, _) = self.split_rest();
        !self.globals.iter().chain(own.iter()).any(|a| {
            let text = a.to_string_lossy();
            matches!(text.as_ref(), "-r" | "--release" | "--doc" | "--dry-run" | "--profile" | "--package" | "--target")
                || ["--profile=", "--package=", "--target="].iter().any(|p| text.starts_with(p))
                || (text.starts_with("-p") && !text.starts_with("--"))
                || text.starts_with("-Z")
        })
    }

    pub fn assemble(&self, extra_globals: &[OsString], subcommand: &str, rest: &[OsString]) -> Vec<OsString> {
        let mut out = Vec::new();
        out.extend(self.toolchain.iter().cloned());
        out.extend(extra_globals.iter().cloned());
        out.extend(self.globals.iter().cloned());
        out.push(OsString::from(subcommand));
        out.extend(rest.iter().cloned());
        out
    }

    pub fn original(&self) -> Vec<OsString> {
        match &self.subcommand {
            Some(sub) => self.assemble(&[], sub, &self.rest),
            None => {
                let mut out: Vec<OsString> = self.toolchain.iter().cloned().collect();
                out.extend(self.globals.iter().cloned());
                out
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Compile,
    /// The lock covers only the build phase, never a long-running program or test run.
    Exec(ExecKind),
    Other,
    /// Passes through unless frust chose the target directory and the whole of it is cleaned.
    Clean,
    CacheOnly,
    Inert,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecKind {
    Run,
    Test,
    Bench,
}

pub fn classify(subcommand: Option<&str>) -> Class {
    match subcommand {
        None => Class::Inert,
        Some("build" | "b" | "check" | "c" | "clippy" | "doc" | "d") => Class::Compile,
        Some("run" | "r") => Class::Exec(ExecKind::Run),
        Some("test" | "t") => Class::Exec(ExecKind::Test),
        Some("bench") => Class::Exec(ExecKind::Bench),
        Some("install") => Class::CacheOnly,
        Some("clean") => Class::Clean,
        Some(
            "uninstall" | "new" | "init" | "login" | "logout" | "search" | "yank" | "owner" | "config" | "version"
            | "help" | "locate-project" | "metadata" | "pkgid" | "read-manifest" | "verify-project" | "tree" | "vendor"
            | "fetch" | "generate-lockfile" | "update" | "add" | "remove" | "fmt" | "report" | "info",
        ) => Class::Inert,
        Some(_) => Class::Other,
    }
}

/// Anything that changes how cargo talks (`--message-format`, `-q`, `-v`, plans, graphs) opts out.
pub fn renderer_compatible(args: &CargoArgs) -> bool {
    let (own, _) = args.split_rest();
    let combined = args.globals.iter().chain(own.iter());
    for arg in combined {
        let text = arg.to_string_lossy();
        let incompatible = text.starts_with("--message-format")
            || matches!(text.as_ref(), "-q" | "--quiet" | "-v" | "-vv" | "--verbose" | "--build-plan" | "--unit-graph" | "--timings")
            || text.starts_with("--timings=")
            || text.starts_with("-Z")
            || (text.starts_with("-v") && text.chars().skip(1).all(|c| c == 'v'));
        if incompatible {
            return false;
        }
    }
    true
}

pub fn os(s: &str) -> OsString {
    OsStr::new(s).to_os_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> CargoArgs {
        CargoArgs::parse(args.iter().map(|a| os(a)))
    }

    #[test]
    fn clean_is_its_own_class() {
        assert_eq!(classify(Some("clean")), Class::Clean);
        assert_eq!(classify(Some("frobnicate")), Class::Other);
    }

    #[test]
    fn strip_flag_removes_only_the_frust_flag_before_the_separator() {
        let mut args = parse(&["clean", "--frust-evict-registry", "--quiet", "--", "--frust-evict-registry"]);
        assert!(args.strip_flag("--frust-evict-registry"));
        assert_eq!(args.rest, vec![os("--quiet"), os("--"), os("--frust-evict-registry")]);
        assert!(!args.strip_flag("--frust-evict-registry"));
        let mut plain = parse(&["clean", "-q"]);
        assert!(!plain.strip_flag("--frust-evict-registry"));
        assert_eq!(plain.rest, vec![os("-q")]);
    }

    #[test]
    fn only_an_unscoped_clean_counts_as_whole_target() {
        assert!(parse(&["clean"]).cleans_whole_target());
        assert!(parse(&["+nightly", "-q", "clean", "--manifest-path", "x/Cargo.toml"]).cleans_whole_target());
        for scoped in [
            vec!["clean", "--release"], vec!["clean", "-r"], vec!["clean", "--profile", "bench"], vec!["clean", "--profile=bench"],
            vec!["clean", "-p", "serde"], vec!["clean", "-pserde"], vec!["clean", "--package=serde"], vec!["clean", "--doc"],
            vec!["clean", "--dry-run"], vec!["clean", "--target", "x86_64-unknown-linux-gnu"], vec!["-Zunstable-options", "clean"],
        ] {
            assert!(!parse(&scoped).cleans_whole_target(), "{scoped:?}");
        }
    }
}
