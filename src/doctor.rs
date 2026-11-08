//! `frust doctor`: checks the environment frust relies on and explains what would stop it accelerating builds.

use std::env;
use std::path::Path;
use std::time::SystemTime;

use crate::cache::{self, TrustMode, TrustSummary};
use crate::config::Config;
use crate::fsx;
use crate::paths::Dirs;
use crate::toolchain::{self, Probe};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone)]
pub struct Check {
    pub name: &'static str,
    pub status: Status,
    pub detail: String,
}

fn check(name: &'static str, status: Status, detail: impl Into<String>) -> Check {
    Check { name, status, detail: detail.into() }
}

pub fn run(cfg: &Config, dirs: &Dirs, cwd: &Path) -> Vec<Check> {
    let mut checks = Vec::new();

    match toolchain::resolve_tool("cargo") {
        Some(path) => checks.push(check("cargo", Status::Ok, path.display().to_string())),
        None => checks.push(check("cargo", Status::Fail, "not found on PATH; install Rust from https://rustup.rs")),
    }
    match toolchain::resolve_tool("rustc") {
        Some(path) => checks.push(check("rustc", Status::Ok, path.display().to_string())),
        None => checks.push(check("rustc", Status::Fail, "not found on PATH")),
    }
    let probe = Probe::run(None, cwd);
    match &probe {
        Some(probe) => {
            checks.push(check(
                "toolchain",
                Status::Ok,
                format!("rustc {} on {} (id {}), cargo {}.{}.{}", probe.release, probe.host, probe.id, probe.cargo_version.0, probe.cargo_version.1, probe.cargo_version.2),
            ));
        }
        None => checks.push(check("toolchain", Status::Fail, "could not query rustc -vV / cargo -V")),
    }

    let wrapper_ok = crate::paths::sibling_wrapper().is_some()
        || toolchain::find_on_path(&crate::paths::wrapper_binary_name()).is_some();
    checks.push(if wrapper_ok {
        check("rustc wrapper", Status::Ok, "frust-wrap found next to the frust binary")
    } else {
        check("rustc wrapper", Status::Fail, "frust-wrap not found; reinstall with `cargo install --path .` so all binaries are installed")
    });

    let sysroot = probe.as_ref().filter(|p| !p.sysroot.is_empty()).map(|p| (Path::new(p.sysroot.as_str()), p.host.as_str()));
    let linkers = configured_linkers(cwd);
    checks.extend(linker_check(&linkers, cwd, sysroot));
    if let Some(probe) = &probe {
        let user = !linkers.is_empty() || toolchain::env_sets_linker(env::vars_os());
        let linker = toolchain::default_linker(&probe.host, toolchain::linker_opted_out(), user, sysroot.map(|(root, _)| root), |p| p.is_file());
        checks.push(check("default linker", Status::Ok, linker.describe()));
        let deps = cfg.tune && probe.cargo_version >= (1, 63, 0);
        checks.push(check("tuning", Status::Ok, format!("{} (FRUST_TUNE={}, FRUST_LINKER={}; dependency debuginfo is skipped when a project defines profiles)", toolchain::tuning_label(deps, &linker), on_off(cfg.tune), if toolchain::linker_opted_out() { "system" } else { "auto" })));
    }

    // A foreign RUSTC_WRAPPER (such as sccache) wins over frust-wrap, so the compile cache cannot run.
    match env::var("RUSTC_WRAPPER") {
        Ok(w) if !w.is_empty() && !w.contains("frust-wrap") => checks.push(check(
            "foreign wrapper",
            Status::Warn,
            format!("RUSTC_WRAPPER={w} is set; frust leaves it in place and the compile cache is inactive. Unset it to let frust cache."),
        )),
        _ => checks.push(check("foreign wrapper", Status::Ok, "RUSTC_WRAPPER is not set by anything else")),
    }

    checks.push(if cfg.cache {
        check("compile cache", Status::Ok, "enabled")
    } else {
        check("compile cache", Status::Warn, "disabled by FRUST_CACHE=off")
    });

    let now = SystemTime::now();
    checks.push(trust_check(TrustMode::from_env(), &cache::trust_summary(dirs, now), now));

    let root = dirs.root();
    match std::fs::create_dir_all(root) {
        Ok(()) => {
            checks.push(check("state directory", Status::Ok, root.display().to_string()));
            checks.push(if fsx::reflink_supported(root) {
                check("copy-on-write clones", Status::Ok, "supported: cache hits cost no extra disk blocks")
            } else {
                check("copy-on-write clones", Status::Warn, "unsupported on this filesystem: cache hits copy files and use extra disk space")
            });
            match fsx::free_space(root) {
                Ok(free) if free < 5 * 1024 * 1024 * 1024 => {
                    checks.push(check("free disk space", Status::Warn, format!("only {} free", fsx::human_bytes(free))))
                }
                Ok(free) => checks.push(check("free disk space", Status::Ok, format!("{} free", fsx::human_bytes(free)))),
                Err(err) => checks.push(check("free disk space", Status::Warn, err.to_string())),
            }
        }
        Err(err) => checks.push(check("state directory", Status::Fail, format!("{}: {err}", root.display()))),
    }

    for linker in ["lld", "mold", "ld64.lld"] {
        if toolchain::find_on_path(linker).is_some() {
            checks.push(check("fast linker", Status::Ok, format!("{linker} is available; enable it in .cargo/config.toml to speed up linking")));
            break;
        }
    }

    let term = env::var("TERM").unwrap_or_default();
    let osc = cfg.osc;
    checks.push(check(
        "terminal",
        if term == "dumb" { Status::Warn } else { Status::Ok },
        format!(
            "TERM={term}, OSC progress {}, title {}, links {}",
            on_off(osc.progress),
            on_off(osc.title),
            on_off(osc.links)
        ),
    ));
    checks
}

fn quoted(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = value;
    while let Some(start) = rest.find(['"', '\'']) {
        let quote = rest.as_bytes()[start] as char;
        let after = &rest[start + 1..];
        let Some(end) = after.find(quote) else { break };
        out.push(after[..end].to_string());
        rest = &after[end + 1..];
    }
    out
}

/// Linker names from `-C linker=X`, `-Clinker=X` and `--codegen linker=X` in a flag list.
fn flag_linkers(tokens: &[String]) -> Vec<String> {
    let mut found = Vec::new();
    for (i, token) in tokens.iter().enumerate() {
        let setting = if token == "-C" || token == "--codegen" {
            tokens.get(i + 1).map(String::as_str)
        } else {
            token.strip_prefix("-C").or_else(|| token.strip_prefix("--codegen="))
        };
        if let Some(name) = setting.and_then(|s| s.strip_prefix("linker=")).filter(|n| !n.is_empty()) {
            found.push(name.to_string());
        }
    }
    found
}

/// Reads `target.<triple>.linker` and the rustflags keys of one cargo config file.
fn config_linkers(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut table = String::new();
    let mut lines = text.lines();
    while let Some(raw) = lines.next() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            table = line.trim_matches(['[', ']']).replace([' ', '"', '\''], "");
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        let key = key.trim().trim_matches(['"', '\'']);
        let full = if table.is_empty() { key.to_string() } else { format!("{table}.{key}") };
        let target = full.starts_with("target.");
        if target && full.ends_with(".linker") {
            found.extend(quoted(value));
        } else if full == "build.rustflags" || (target && full.ends_with(".rustflags")) {
            let mut value = value.trim().to_string();
            while value.starts_with('[') && !value.contains(']') {
                let Some(next) = lines.next() else { break };
                value.push(' ');
                value.push_str(next.split('#').next().unwrap_or(""));
            }
            let tokens: Vec<String> = quoted(&value).iter().flat_map(|s| s.split_whitespace().map(str::to_string).collect::<Vec<_>>()).collect();
            found.extend(flag_linkers(&tokens));
        }
    }
    found
}

/// True when one cargo config file (or `--config` value) names a linker.
pub(crate) fn config_sets_linker(text: &str) -> bool {
    !config_linkers(text).is_empty()
}

/// Every explicit linker that applies to `cwd`: cargo config files, `RUSTFLAGS` and `CARGO_BUILD_RUSTFLAGS`.
fn configured_linkers(cwd: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for file in crate::run::config_files(None, cwd) {
        if let Ok(text) = std::fs::read_to_string(&file) {
            found.extend(config_linkers(&text));
        }
    }
    for var in ["RUSTFLAGS", "CARGO_BUILD_RUSTFLAGS"] {
        if let Ok(flags) = env::var(var) {
            let tokens: Vec<String> = flags.split_whitespace().map(str::to_string).collect();
            found.extend(flag_linkers(&tokens));
        }
    }
    found.sort();
    found.dedup();
    found
}

/// Resolves each name the way the wrapper does, so a warning here means the wrapper skips the cache.
fn linker_check(names: &[String], cwd: &Path, sysroot: Option<(&Path, &str)>) -> Option<Check> {
    if names.is_empty() {
        return None;
    }
    let missing: Vec<String> = names
        .iter()
        .filter(|name| {
            let path = Path::new(name.as_str());
            if path.is_absolute() || path.parent().is_some_and(|p| !p.as_os_str().is_empty()) {
                !cwd.join(path).is_file()
            } else {
                toolchain::resolve_linker(name, sysroot).is_none()
            }
        })
        .map(|name| format!("cache bypassed: linker {name} not found; searched PATH and the toolchain sysroot"))
        .collect();
    Some(if missing.is_empty() {
        check("explicit linker", Status::Ok, format!("{} resolves, so the compile cache applies", names.join(", ")))
    } else {
        check("explicit linker", Status::Warn, missing.join("; "))
    })
}

fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

fn trust_check(mode: TrustMode, summary: &TrustSummary, now: SystemTime) -> Check {
    let counts = format!("{} violation(s) logged, {} audit(s) passed", summary.violations, summary.audits_passed);
    match summary.quarantined.iter().map(|(_, when)| *when).max() {
        Some(last) => check(
            "stamp trust",
            Status::Warn,
            format!(
                "mode {}, {} volume(s) quarantined, last violation {}; {counts}; see stamp-trust/violations.jsonl under the state directory",
                mode.name(),
                summary.quarantined.len(),
                ago(now, last)
            ),
        ),
        None => check("stamp trust", Status::Ok, format!("mode {}, no volume quarantined, {counts}", mode.name())),
    }
}

fn ago(now: SystemTime, then: SystemTime) -> String {
    let secs = now.duration_since(then).map_or(0, |d| d.as_secs());
    match secs {
        0..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn stamp_trust_warns_only_while_a_volume_is_quarantined() {
        let now = SystemTime::now();
        let clean = TrustSummary { quarantined: Vec::new(), violations: 0, audits_passed: 12 };
        let ok = trust_check(TrustMode::On, &clean, now);
        assert_eq!((ok.name, ok.status), ("stamp trust", Status::Ok));
        assert!(ok.detail.contains("mode on") && ok.detail.contains("12 audit(s) passed"), "{}", ok.detail);

        let hit = TrustSummary {
            quarantined: vec![(1, now - Duration::from_secs(7200)), (2, now - Duration::from_secs(90))],
            violations: 3,
            audits_passed: 0,
        };
        let warn = trust_check(TrustMode::Audit, &hit, now);
        assert_eq!(warn.status, Status::Warn);
        assert!(warn.detail.contains("mode audit") && warn.detail.contains("2 volume(s) quarantined"), "{}", warn.detail);
        assert!(warn.detail.contains("last violation 1m ago") && warn.detail.contains("3 violation(s)"), "{}", warn.detail);
    }

    #[test]
    fn linkers_are_read_from_config_tables_and_every_flag_spelling() {
        let text = "[build]\nrustflags = [\"-C\", \"linker=rust-lld\"]\n\n[target.x86_64-pc-windows-msvc]\nlinker = \"lld-link\"\nrustflags = [\n  \"-Clinker=ld.lld\", # comment\n]\n\n[profile.dev]\nlinker = \"ignored\"\n";
        assert_eq!(config_linkers(text), ["rust-lld", "lld-link", "ld.lld"]);
        assert_eq!(config_linkers("target.a.linker = 'mold'\n"), ["mold"]);
        assert!(config_linkers("[build]\njobs = 4\n").is_empty());
        let flags: Vec<String> = ["-C", "linker=a", "-Clinker=b", "--codegen=linker=c", "--codegen", "linker=d", "-C", "opt-level=3", "-Clinker="]
            .iter().map(|s| s.to_string()).collect();
        assert_eq!(flag_linkers(&flags), ["a", "b", "c", "d"]);
    }

    #[test]
    fn an_unresolvable_linker_warns_with_the_bypass_reason_and_a_sysroot_linker_is_ok() {
        let root = env::temp_dir().join(format!("frust-doctor-linker-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let bin = root.join("lib").join("rustlib").join("x86_64-test").join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let lld = bin.join(format!("frust-test-rust-lld{}", env::consts::EXE_SUFFIX));
        std::fs::write(&lld, b"lld").unwrap();
        crate::sys::make_executable(&lld).unwrap();
        let sysroot = Some((root.as_path(), "x86_64-test"));
        let names = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        assert!(linker_check(&[], &root, sysroot).is_none());
        let ok = linker_check(&names(&["frust-test-rust-lld"]), &root, sysroot).unwrap();
        assert_eq!(ok.status, Status::Ok, "{}", ok.detail);
        let warn = linker_check(&names(&["frust-test-rust-lld", "frust-test-absent"]), &root, sysroot).unwrap();
        assert_eq!(warn.status, Status::Warn);
        assert_eq!(warn.detail, "cache bypassed: linker frust-test-absent not found; searched PATH and the toolchain sysroot");
        let without = linker_check(&names(&["frust-test-rust-lld"]), &root, None).unwrap();
        assert_eq!(without.status, Status::Warn, "no sysroot means only PATH is searched");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ages_are_shown_in_the_largest_whole_unit() {
        let now = SystemTime::now();
        for (secs, text) in [(5, "5s ago"), (59, "59s ago"), (60, "1m ago"), (7200, "2h ago"), (3 * 86_400 + 5, "3d ago")] {
            assert_eq!(ago(now, now - Duration::from_secs(secs)), text);
        }
        assert_eq!(ago(now, now + Duration::from_secs(10)), "0s ago", "a time in the future is shown as now");
    }
}
