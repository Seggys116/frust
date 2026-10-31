//! frust wraps the installed Rust toolchain without shipping or pinning a compiler.

pub mod bench;
pub mod cache;
pub mod cargo_args;
pub mod cli;
pub mod compact;
pub mod config;
pub mod doctor;
pub mod events;
pub mod fsx;
pub mod gc;
pub mod history;
pub mod lock;
pub mod paths;
pub mod run;
pub mod signals;
pub mod stats;
pub mod style;
pub mod sys;
pub mod toolchain;
pub mod unit_graph;
pub mod ui;
pub mod update;
pub mod workspace;
pub mod wrapper;

use std::ffi::OsString;

#[derive(Debug, Clone, Copy)]
pub enum Persona {
    /// Multi-call entry point: a symlink named `f<tool>` behaves as that tool's wrapper.
    Frust,
    Cargo,
    Clippy,
    Rustc,
    Tool(&'static str),
    Wrap,
}

pub fn main_for(persona: Persona) -> i32 {
    let mut argv = std::env::args_os();
    let argv0 = argv.next().unwrap_or_default();
    let args: Vec<OsString> = argv.collect();
    match persona {
        Persona::Frust => dispatch_by_name(&argv0, args),
        Persona::Cargo => run::cargo_main(args),
        Persona::Clippy => run::cargo_main(with_subcommand("clippy", args)),
        Persona::Rustc => rustc_main(args),
        Persona::Tool(name) => run::tool_main(name, args),
        Persona::Wrap => wrapper::run(args),
    }
}

fn dispatch_by_name(argv0: &OsString, args: Vec<OsString>) -> i32 {
    let name = sys::tool_name(argv0);
    match name.as_str() {
        "frust" | "" => cli::frust_main(args),
        "fcargo" => run::cargo_main(args),
        "fclippy" => run::cargo_main(with_subcommand("clippy", args)),
        "frustc" => rustc_main(args),
        other => match other.strip_prefix('f') {
            Some(tool) if !tool.is_empty() => run::tool_main(tool, args),
            _ => cli::frust_main(args),
        },
    }
}

fn with_subcommand(subcommand: &str, mut args: Vec<OsString>) -> Vec<OsString> {
    let at = usize::from(args.first().is_some_and(|a| a.to_string_lossy().starts_with('+')));
    args.insert(at, OsString::from(subcommand));
    args
}

/// Routes rustc through the cache wrapper, which passes through anything it cannot safely cache.
fn rustc_main(args: Vec<OsString>) -> i32 {
    let Some(rustc) = toolchain::resolve_tool("rustc") else {
        eprintln!("frust: cannot find `rustc` on PATH");
        return 127;
    };
    let mut full = vec![rustc.into_os_string()];
    full.extend(args);
    wrapper::run(full)
}
