//! `fcargo` selects the central target default and, on a terminal, draws fRust progress.
//! Plain, dumb, quiet, and explicit JSON invocations replace this process with Cargo.

use std::env;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use frust_core::{Header, Phase, PhaseMark, ProgressMode, mode_from_env, render, unescape_json_string};

fn main() {
    let mut args: Vec<String> = env::args().skip(1).collect();
    let mode = mode_from_env(env::var("FRUST_PROGRESS").ok().as_deref(), env::var("TERM").ok().as_deref(), stderr_is_tty());
    let cargo = cargo_binary();
    if passthrough(mode, &args) {
        exec_cargo(&cargo, &args);
    }
    if mode == ProgressMode::Interactive {
        run_interactive(&cargo, &args, mode);
    }
    if !args.iter().any(|arg| arg == "--message-format" || arg.starts_with("--message-format=")) {
        args.push("--message-format=json".to_string());
    }
    let started = Instant::now();
    let mut child = match cargo_command(&cargo, &args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            eprintln!("fRust could not run {}: {err}", cargo.display());
            std::process::exit(101);
        }
    };
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let stderr_thread = thread::spawn(move || {
        let mut reader = stderr;
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let _ = io::stderr().write_all(&buf[..n]);
                    let _ = io::stderr().flush();
                }
            }
        }
    });
    let mut phases = initial_phases(&args);
    let mut observed = BuildObs::default();
    let mut stdout_text = String::new();
    let reader = BufReader::new(stdout);
    for line in reader.lines() {
        let Ok(line) = line else { break };
        let mut diagnostics = Vec::new();
        apply_message(&line, &mut phases, &mut observed, &mut stdout_text, &mut diagnostics);
        for diagnostic in diagnostics {
            let _ = writeln!(io::stderr(), "{diagnostic}");
        }
    }
    let _ = stderr_thread.join();
    let status = child.wait().unwrap_or_else(|_| std::process::exit(101));
    if status.success() {
        phases = final_phases(&phases, &observed);
    } else if !phases.iter().any(|phase| phase.mark == PhaseMark::Failed) {
        let name = if phases.iter().any(|phase| phase.name == "Test") { "Test" } else { "Compile" };
        mark(&mut phases, name, PhaseMark::Failed, "cargo reported failure");
    }
    if env::var_os("FRUST_BUILD_REPORT").is_some() {
        let _ = writeln!(io::stderr(), "{}", build_report(&phases));
    }
    let frame_mode = if started.elapsed() >= Duration::from_millis(125) || !status.success() {
        mode
    } else {
        ProgressMode::Static
    };
    let frame = render(&header_for(&args), &phases, frame_mode, term_width(), 0, color_enabled());
    let _ = io::stderr().write_all(frame.as_bytes());
    if !stdout_text.is_empty() {
        println!("{stdout_text}");
    }
    process_exit(status);
}

fn passthrough(mode: ProgressMode, args: &[String]) -> bool {
    if mode == ProgressMode::Plain {
        return true;
    }
    args.iter().any(|arg| {
        arg == "--quiet" || arg == "-q" || arg == "--message-format" || arg.starts_with("--message-format=") || arg == "--json"
    })
}

fn exec_cargo(cargo: &Path, args: &[String]) -> ! {
    let err = cargo_command(cargo, args).exec();
    eprintln!("fRust could not run {}: {err}", cargo.display());
    std::process::exit(101);
}

fn cargo_command(cargo: &Path, args: &[String]) -> Command {
    let mut command = Command::new(cargo);
    command.args(args).env("FRUST_INVOCATION", "fcargo");
    if env::var_os("RUSTC").is_none() {
        if let Some(frustc) = sibling_frustc(cargo) {
            command.env("RUSTC", frustc);
        }
    }
    command
}

fn sibling_frustc(cargo: &Path) -> Option<PathBuf> {
    let parent = cargo.parent()?;
    if parent.as_os_str().is_empty() {
        return None;
    }
    let path = parent.join("frustc");
    path.is_file().then_some(path)
}

enum CargoEvent {
    Stderr(Vec<u8>),
    Line(String),
}

/// Interactive runs repaint at most eight times a second.
/// A command that exits before the first frame still gets one static frame.
fn run_interactive(cargo: &Path, args: &[String], mode: ProgressMode) -> ! {
    let mut args = args.to_vec();
    if !args.iter().any(|arg| arg == "--message-format" || arg.starts_with("--message-format=")) {
        args.push("--message-format=json".to_string());
    }
    let started = Instant::now();
    let mut child = match cargo_command(cargo, &args).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
        Ok(child) => child,
        Err(err) => {
            eprintln!("fRust could not run {}: {err}", cargo.display());
            std::process::exit(101);
        }
    };
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = mpsc::channel();
    let stderr_tx = tx.clone();
    thread::spawn(move || {
        let mut reader = stderr;
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if stderr_tx.send(CargoEvent::Stderr(buf[..n].to_vec())).is_err() {
                        break;
                    }
                }
            }
        }
    });
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(CargoEvent::Line(line)).is_err() {
                break;
            }
        }
    });
    let mut phases = initial_phases(&args);
    let mut observed = BuildObs::default();
    let mut stdout_text = String::new();
    let mut drawn = 0usize;
    let mut cursor_hidden = false;
    let mut last_paint: Option<Instant> = None;
    loop {
        match rx.recv_timeout(Duration::from_millis(125)) {
            Ok(CargoEvent::Stderr(bytes)) => {
                erase_frame(&mut drawn, &mut cursor_hidden);
                let _ = io::stderr().write_all(&bytes);
                let _ = io::stderr().flush();
            }
            Ok(CargoEvent::Line(line)) => {
                let mut diagnostics = Vec::new();
                apply_message(&line, &mut phases, &mut observed, &mut stdout_text, &mut diagnostics);
                if !diagnostics.is_empty() {
                    erase_frame(&mut drawn, &mut cursor_hidden);
                    for diagnostic in diagnostics {
                        let _ = writeln!(io::stderr(), "{diagnostic}");
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        let due = last_paint.is_none_or(|stamp| stamp.elapsed() >= Duration::from_millis(125));
        if started.elapsed() >= Duration::from_millis(125) && due {
            paint_frame(&header_for(&args), &phases, mode, paint_tick(&started), &mut drawn, &mut cursor_hidden);
            last_paint = Some(Instant::now());
        }
    }
    let status = child.wait().unwrap_or_else(|_| std::process::exit(101));
    if status.success() {
        phases = final_phases(&phases, &observed);
    } else if !phases.iter().any(|phase| phase.mark == PhaseMark::Failed) {
        let name = if phases.iter().any(|phase| phase.name == "Test") { "Test" } else { "Compile" };
        mark(&mut phases, name, PhaseMark::Failed, "cargo reported failure");
    }
    if env::var_os("FRUST_BUILD_REPORT").is_some() {
        erase_frame(&mut drawn, &mut cursor_hidden);
        let _ = writeln!(io::stderr(), "{}", build_report(&phases));
    }
    let frame_mode = if started.elapsed() >= Duration::from_millis(125) || !status.success() { mode } else { ProgressMode::Static };
    if drawn == 0 {
        let frame = render(&header_for(&args), &phases, frame_mode, term_width(), 0, color_enabled());
        let _ = io::stderr().write_all(frame.as_bytes());
    } else {
        paint_frame(&header_for(&args), &phases, frame_mode, paint_tick(&started), &mut drawn, &mut cursor_hidden);
    }
    show_cursor(&mut cursor_hidden);
    if !stdout_text.is_empty() {
        println!("{stdout_text}");
    }
    process_exit(status);
}

fn paint_tick(started: &Instant) -> u32 {
    (started.elapsed().as_millis() / 125) as u32
}

fn paint_frame(header: &Header, phases: &[Phase], mode: ProgressMode, tick: u32, drawn: &mut usize, cursor_hidden: &mut bool) {
    let frame = render(header, phases, mode, term_width(), tick, color_enabled());
    let mut err = io::stderr();
    if *drawn == 0 {
        hide_cursor(cursor_hidden);
        let _ = err.write_all(frame.as_bytes());
    } else {
        let _ = write!(err, "\u{1b}[{drawn}A");
        for line in frame.lines() {
            let _ = write!(err, "\u{1b}[2K{line}\n");
        }
        let lines = frame.lines().count();
        if *drawn > lines {
            for _ in lines..*drawn {
                let _ = write!(err, "\u{1b}[2K\n");
            }
            let _ = write!(err, "\u{1b}[{}A", *drawn - lines);
        }
    }
    let _ = err.flush();
    *drawn = frame.lines().count();
}

fn erase_frame(drawn: &mut usize, cursor_hidden: &mut bool) {
    if *drawn == 0 {
        return;
    }
    let mut err = io::stderr();
    let _ = write!(err, "\u{1b}[{drawn}A");
    for _ in 0..*drawn {
        let _ = write!(err, "\u{1b}[2K\n");
    }
    let _ = write!(err, "\u{1b}[{drawn}A");
    let _ = err.flush();
    *drawn = 0;
    show_cursor(cursor_hidden);
}

fn hide_cursor(hidden: &mut bool) {
    if !*hidden {
        let _ = write!(io::stderr(), "\u{1b}[?25l");
        *hidden = true;
    }
}

fn show_cursor(hidden: &mut bool) {
    if *hidden {
        let _ = write!(io::stderr(), "\u{1b}[?25h");
        *hidden = false;
    }
}

fn process_exit(status: std::process::ExitStatus) -> ! {
    std::process::exit(status.code().unwrap_or(1));
}

fn cargo_binary() -> PathBuf {
    if let Some(path) = env::var_os("FRUST_CARGO") {
        return PathBuf::from(path);
    }
    let argv0 = env::args().next().unwrap_or_default();
    let sibling = Path::new(&argv0).parent().unwrap_or(Path::new(".")).join("cargo");
    if sibling.exists() {
        sibling
    } else {
        PathBuf::from("cargo")
    }
}

fn stderr_is_tty() -> bool {
    libc_isatty(2)
}

fn libc_isatty(fd: i32) -> bool {
    extern "C" {
        fn isatty(fd: i32) -> i32;
    }
    unsafe { isatty(fd) == 1 }
}

fn color_enabled() -> bool {
    match env::var("FRUST_PROGRESS_COLOR").ok().as_deref() {
        Some("always") => true,
        Some("never") => false,
        _ => stderr_is_tty() && env::var("TERM").ok().as_deref() != Some("dumb"),
    }
}

fn term_width() -> usize {
    env::var("COLUMNS").ok().and_then(|text| text.parse().ok()).unwrap_or(80)
}

fn header_for(args: &[String]) -> Header {
    let command = args.iter().find(|arg| !arg.starts_with('-')).cloned().unwrap_or_else(|| "build".into());
    Header { subtitle: command }
}

fn initial_phases(args: &[String]) -> Vec<Phase> {
    let mut phases = vec![Phase {
        name: "Resolve".into(),
        mark: PhaseMark::Active,
        detail: String::new(),
        completed: None,
        total: None,
    }];
    if args.iter().any(|arg| arg == "test" || arg == "bench") {
        phases.push(pending("Test"));
    } else {
        phases.push(pending("Compile"));
    }
    phases
}

fn pending(name: &str) -> Phase {
    Phase { name: name.into(), mark: PhaseMark::Pending, detail: String::new(), completed: None, total: None }
}

#[derive(Default)]
struct BuildObs {
    saw_artifact: bool,
    any_rebuilt: bool,
}

fn apply_message(line: &str, phases: &mut Vec<Phase>, observed: &mut BuildObs, program_output: &mut String, diagnostics: &mut Vec<String>) {
    let Some(kind) = json_field(line, "reason") else {
        if !line.is_empty() {
            program_output.push_str(line);
            program_output.push('\n');
        }
        return;
    };
    match kind.as_str() {
        "compiler-artifact" | "build-script-executed" => {
            observed.saw_artifact = true;
            if kind == "compiler-artifact" && !line.contains("\"fresh\":true") {
                observed.any_rebuilt = true;
            }
            mark(phases, "Resolve", PhaseMark::Done, "Dependencies ready");
            let name = if phases.iter().any(|phase| phase.name == "Test") { "Test" } else { "Compile" };
            mark(phases, name, PhaseMark::Active, String::new());
        }
        "compiler-message" => {
            if let Some(rendered) = json_field(line, "rendered") {
                diagnostics.push(rendered.trim_end_matches('\n').to_string());
            }
            if line.contains("\"level\":\"error\"") {
                mark(phases, "Compile", PhaseMark::Failed, "rustc reported an error");
            }
        }
        "build-finished" => {
            let ok = line.contains("\"success\":true");
            let name = if phases.iter().any(|phase| phase.name == "Test") { "Test" } else { "Compile" };
            if ok {
                mark(phases, "Resolve", PhaseMark::Done, "Dependencies ready");
                if observed.saw_artifact && !observed.any_rebuilt {
                    mark(phases, name, PhaseMark::Skipped, "Reused cached units");
                } else {
                    mark(phases, name, PhaseMark::Done, "Finished");
                }
            } else if phases.iter().all(|phase| phase.mark != PhaseMark::Failed) {
                mark(phases, name, PhaseMark::Failed, "cargo reported failure");
            }
        }
        _ => {}
    }
}

fn mark(phases: &mut Vec<Phase>, name: &str, state: PhaseMark, detail: impl Into<String>) {
    if let Some(phase) = phases.iter_mut().find(|phase| phase.name == name) {
        if phase.mark == PhaseMark::Failed && state != PhaseMark::Failed {
            return;
        }
        phase.mark = state;
        let detail = detail.into();
        if !detail.is_empty() {
            phase.detail = detail;
        }
    }
}

fn final_phases(phases: &[Phase], observed: &BuildObs) -> Vec<Phase> {
    let mut phases = phases.to_vec();
    for phase in &mut phases {
        if phase.mark == PhaseMark::Active || phase.mark == PhaseMark::Pending {
            if observed.saw_artifact && !observed.any_rebuilt && phase.name != "Resolve" {
                phase.mark = PhaseMark::Skipped;
                if phase.detail.is_empty() {
                    phase.detail = "Reused cached units".into();
                }
            } else {
                phase.mark = PhaseMark::Done;
                if phase.detail.is_empty() {
                    phase.detail = "Finished".into();
                }
            }
        }
    }
    phases
}

fn build_report(phases: &[Phase]) -> String {
    let mut out = String::from("fRust build report\n");
    for label in ["Analysis", "Code generation", "Full LTO", "Native link"] {
        let observed = phases.iter().find(|phase| phase.name == label);
        let detail = observed.map(|phase| phase.detail.as_str()).filter(|detail| !detail.is_empty()).unwrap_or("Unavailable");
        out.push_str(&format!("  {label:<16} {detail}\n"));
    }
    out
}

/// Pull one JSON string field. Cargo escapes diagnostics, including ANSI as `\u001b`.
fn json_field(line: &str, field: &str) -> Option<String> {
    let pattern = format!("\"{field}\":\"");
    let start = line.find(&pattern)? + pattern.len();
    unescape_json_string(&line[start..])
}
