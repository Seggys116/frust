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

/// Which OSC (operating system command) features the terminal gets.
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
}

impl Config {
    pub fn from_env() -> Config {
        Config {
            cache: flag("FRUST_CACHE", true),
            cache_primary: flag("FRUST_CACHE_PRIMARY", true),
            cache_max_bytes: env::var("FRUST_CACHE_MAX_GIB")
                .ok()
                .and_then(|v| v.parse::<f64>().ok())
                .map(|gib| (gib * 1024.0 * 1024.0 * 1024.0) as u64)
                .unwrap_or(16 * 1024 * 1024 * 1024),
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
        }
    }
}

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
