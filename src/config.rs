//! Runtime configuration from `FRUST_*` environment variables; every knob has a safe default.

use std::env;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetMode {
    Central,
    Local,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderMode {
    Auto,
    Ansi,
    Plain,
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Osc {
    pub progress: bool,
    pub title: bool,
    pub links: bool,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub cache: bool,
    pub cache_primary: bool,
    pub cache_max_bytes: u64,
    pub target: TargetMode,
    pub tune: bool,
    pub render: RenderMode,
    pub osc: Osc,
    pub lock_timeout: Option<Duration>,
    pub lock: bool,
    pub debug: bool,
    pub gc_sync: bool,
    /// Store compile results before the wrapper exits instead of in a detached helper.
    pub store_sync: bool,
    pub keylock_wait: Duration,
}

/// Default same-unit compile wait, overridden by `FRUST_KEYLOCK_WAIT_SECS`.
pub const KEY_WAIT_CAP: Duration = Duration::from_secs(300);

impl Config {
    pub fn from_env() -> Config {
        Config {
            cache: flag("FRUST_CACHE", true),
            cache_primary: flag("FRUST_CACHE_PRIMARY", true),
            cache_max_bytes: env::var("FRUST_CACHE_MAX_GIB")
                .ok()
                .and_then(|v| v.trim().parse::<f64>().ok())
                .filter(|gib| *gib >= 0.0)
                .map_or(u64::MAX, |gib| (gib * 1024.0 * 1024.0 * 1024.0) as u64),
            target: match env::var("FRUST_TARGET").as_deref() {
                Ok("local") | Ok("off") => TargetMode::Local,
                _ => TargetMode::Central,
            },
            tune: flag("FRUST_TUNE", false),
            render: match env::var("FRUST_RENDER").as_deref() {
                Ok("ansi") | Ok("always") => RenderMode::Ansi,
                Ok("plain") => RenderMode::Plain,
                Ok("off") | Ok("never") | Ok("0") => RenderMode::Off,
                _ => RenderMode::Auto,
            },
            osc: parse_osc(),
            lock: flag("FRUST_LOCK", true),
            lock_timeout: env::var("FRUST_LOCK_TIMEOUT_SECS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .filter(|&s| s > 0)
                .map(Duration::from_secs),
            debug: flag("FRUST_DEBUG", false),
            gc_sync: flag("FRUST_GC_SYNC", false),
            store_sync: flag("FRUST_STORE_SYNC", false),
            keylock_wait: env::var("FRUST_KEYLOCK_WAIT_SECS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .map(Duration::from_secs)
                .unwrap_or(KEY_WAIT_CAP),
        }
    }
}

/// `FRUST_CACHE_UNIT_VERSIONS`: entries of one path unit that stay in the store; defaults to 1 and is capped at `wrapper::UNIT_VERSIONS`.
pub fn unit_versions() -> usize {
    env::var("FRUST_CACHE_UNIT_VERSIONS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(1)
        .clamp(1, crate::wrapper::UNIT_VERSIONS)
}

/// `FRUST_CACHE_TESTS`: whether test and bench harness executables are stored; off by default, since they are rebuilt on edits and rarely restored.
pub fn cache_tests() -> bool {
    flag("FRUST_CACHE_TESTS", false)
}

/// The most slot lock files a store can ever use; gc treats every slot below it as canonical.
pub const MAX_STORE_SLOTS: usize = 16;

/// Background store slots: `FRUST_STORE_SLOTS` (1..=16) or a default sized for the volume's I/O, which Windows tolerates less well than its core count suggests.
pub fn store_slots() -> usize {
    let cpus = std::thread::available_parallelism().map_or(2, |n| n.get());
    env::var("FRUST_STORE_SLOTS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| (1..=MAX_STORE_SLOTS).contains(n))
        .unwrap_or_else(|| default_store_slots(cpus))
}

fn default_store_slots(cpus: usize) -> usize {
    if cfg!(windows) { (cpus / 2).clamp(2, 8) } else { (cpus / 2).clamp(1, 4) }
}

/// Days (fractional allowed) as a duration; negative or non-numeric input is rejected and huge values saturate.
pub fn days_to_duration(days: f64) -> Option<Duration> {
    (days >= 0.0).then(|| Duration::from_secs_f64((days * 86_400.0).min(u32::MAX as f64 * 86_400.0)))
}

/// `FRUST_CACHE_MAX_AGE_DAYS`: cache entries unused for longer are evicted; defaults to 14 days.
pub fn cache_max_age() -> Duration {
    env::var("FRUST_CACHE_MAX_AGE_DAYS")
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .and_then(days_to_duration)
        .unwrap_or(DEFAULT_CACHE_MAX_AGE)
}

pub const DEFAULT_CACHE_MAX_AGE: Duration = Duration::from_secs(14 * 86_400);

fn flag(name: &str, default: bool) -> bool {
    match env::var(name).as_deref() {
        Ok("0") | Ok("off") | Ok("false") | Ok("no") => false,
        Ok("1") | Ok("on") | Ok("true") | Ok("yes") => true,
        _ => default,
    }
}

/// `FRUST_OSC` is `auto` (default), `off`, `all`, or a comma list of `progress,title,links`.
fn parse_osc() -> Osc {
    let all = Osc { progress: true, title: true, links: true };
    match env::var("FRUST_OSC").as_deref() {
        Ok("off") | Ok("never") | Ok("0") => Osc::default(),
        Ok("all") | Ok("always") | Ok("1") => all,
        Ok(list) if !list.is_empty() && list != "auto" => Osc {
            progress: list.split(',').any(|p| p.trim() == "progress"),
            title: list.split(',').any(|p| p.trim() == "title"),
            links: list.split(',').any(|p| p.trim() == "links"),
        },
        _ => detect_osc(),
    }
}

/// Conservative auto-detection: only terminals known to handle these sequences.
fn detect_osc() -> Osc {
    let program = env::var("TERM_PROGRAM").unwrap_or_default();
    let term = env::var("TERM").unwrap_or_default();
    let known_progress = matches!(program.as_str(), "ghostty" | "WezTerm" | "iTerm.app")
        || env::var_os("WT_SESSION").is_some()
        || env::var_os("GHOSTTY_RESOURCES_DIR").is_some();
    let known_links = known_progress
        || matches!(program.as_str(), "vscode" | "Apple_Terminal" | "Hyper")
        || term.contains("kitty")
        || env::var_os("KITTY_WINDOW_ID").is_some()
        || env::var_os("VTE_VERSION").is_some();
    let modern = known_links || term.starts_with("xterm") || term.starts_with("tmux") || term.starts_with("screen");
    Osc { progress: known_progress, title: modern && term != "dumb", links: known_links }
}

#[cfg(test)]
mod tests {
    use super::default_store_slots;

    #[test]
    fn default_store_slots_scale_with_cores_within_the_platform_bounds() {
        let (floor, ceiling) = if cfg!(windows) { (2, 8) } else { (1, 4) };
        assert_eq!(default_store_slots(1), floor);
        assert_eq!(default_store_slots(64), ceiling);
        assert_eq!(default_store_slots(6), 3);
    }
}
