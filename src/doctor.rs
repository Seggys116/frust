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
    match Probe::run(None, cwd) {
        Some(probe) => {
            checks.push(check(
                "toolchain",
                Status::Ok,
                format!("rustc {} on {} (id {}), cargo {}.{}.{}", probe.release, probe.host, probe.id, probe.cargo_version.0, probe.cargo_version.1, probe.cargo_version.2),
            ));
        }
        None => checks.push(check("toolchain", Status::Fail, "could not query rustc -vV / cargo -V")),
    }

    let wrapper_ok = env::current_exe().ok().is_some_and(|e| e.with_file_name("frust-wrap").exists())
        || toolchain::find_on_path("frust-wrap").is_some();
    checks.push(if wrapper_ok {
        check("rustc wrapper", Status::Ok, "frust-wrap found next to the frust binary")
    } else {
        check("rustc wrapper", Status::Fail, "frust-wrap not found; reinstall with `cargo install --path .` so all binaries are installed")
    });

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
    fn ages_are_shown_in_the_largest_whole_unit() {
        let now = SystemTime::now();
        for (secs, text) in [(5, "5s ago"), (59, "59s ago"), (60, "1m ago"), (7200, "2h ago"), (3 * 86_400 + 5, "3d ago")] {
            assert_eq!(ago(now, now - Duration::from_secs(secs)), text);
        }
        assert_eq!(ago(now, now + Duration::from_secs(10)), "0s ago", "a time in the future is shown as now");
    }
}
