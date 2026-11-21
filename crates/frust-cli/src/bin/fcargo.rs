//! `fcargo` selects the central target default and, on a terminal, draws fRust progress.
//! Plain, dumb, quiet, explicit message-format, and non-build invocations replace this process with Cargo.

use std::env;
use std::ffi::{OsStr, OsString, c_int, c_ulong, c_void};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use frust_core::{
    GcConfig, GcReport, Header, HostOs, LiveSet, Phase, PhaseMark, ProgressMode, ResolvedRoots, canonical_workspace, central_due,
    collect_central, collect_profile_dir, load_roots_from_host, mode_from_env, render, resolve_max_bytes, slot_root,
    touch_slot_marker, unescape_json_string,
};

/// Subcommands that accept `--message-format`; everything else goes to Cargo untouched.
const BUILD_SUBCOMMANDS: &[&str] = &["build", "b", "check", "c", "test", "t", "bench", "run", "r", "doc", "d", "clippy", "rustc", "rustdoc"];
/// Options that take a separate value, so the value is never mistaken for the subcommand.
const VALUE_OPTIONS: &[&str] = &[
    "-Z",
    "--config",
    "-C",
    "--color",
    "-j",
    "--jobs",
    "--target-dir",
    "--build-dir",
    "--manifest-path",
    "--lockfile-path",
    "--target",
    "--explain",
    "--message-format",
];
const REPAINT: Duration = Duration::from_millis(125);
/// Longest stdout run held in memory; a longer line is forwarded in pieces.
const MAX_CHUNK: usize = 8 << 20;
const CHANNEL_DEPTH: usize = 64;
const SHOW_CURSOR: &[u8] = b"\x1b[?25h";
const MAX_JSON_DEPTH: usize = 128;
/// A cleanup that frees less than this stays silent unless FRUST_GC_VERBOSE=1.
const GC_NOTICE_BYTES: u64 = 100 << 20;

static CURSOR_HIDDEN: AtomicBool = AtomicBool::new(false);

#[cfg(target_os = "linux")]
const TIOCGWINSZ: c_ulong = 0x5413;
#[cfg(target_os = "macos")]
const TIOCGWINSZ: c_ulong = 0x4008_7468;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
const TIOCGWINSZ: c_ulong = 0;

#[repr(C)]
#[allow(dead_code)] // the pixel fields only complete the kernel's struct layout
struct Winsize {
    rows: u16,
    cols: u16,
    xpixel: u16,
    ypixel: u16,
}

extern "C" {
    fn isatty(fd: c_int) -> c_int;
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    fn signal(signum: c_int, handler: usize) -> usize;
    fn raise(signum: c_int) -> c_int;
    fn write(fd: c_int, buf: *const c_void, count: usize) -> isize;
}

fn main() {
    let raw: Vec<OsString> = env::args_os().skip(1).collect();
    let args = lossy_args(&raw);
    let exe = env::current_exe().ok().and_then(|path| path.canonicalize().ok());
    let selection = select_cargo(env::var_os("FRUST_CARGO"), exe.as_deref(), |path| path.is_file());
    if let Some(exe) = &exe {
        if runs_self(&selection.cargo, exe, env::var_os("PATH").as_deref()) {
            fail(&format!(
                "{} resolves to this fcargo; set FRUST_CARGO to the real cargo or install cargo next to fcargo",
                selection.cargo.display()
            ));
        }
    }
    if let Some(toolchain) = leading_toolchain(&args) {
        if let Some(message) = toolchain_error(toolchain, &selection, || is_rustup_proxy(&selection.cargo)) {
            fail(&message);
        }
    }
    let rustc = select_frustc(&args, env::var_os("RUSTC").is_some(), &selection, exe.as_deref(), |path| path.is_file());
    let mode = mode_from_env(env::var("FRUST_PROGRESS").ok().as_deref(), env::var("TERM").ok().as_deref(), stderr_is_tty());
    match launch_plan(mode, &args) {
        Launch::Passthrough => exec_cargo(cargo_command(&selection, &raw, rustc.as_deref())),
        Launch::Wrapped { insert_at } => {
            let no_color = env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty());
            let ansi = wants_ansi(&args, env::var("CARGO_TERM_COLOR").ok().as_deref(), no_color, color_enabled());
            let argv = inject(&raw, insert_at, json_format(ansi));
            run_wrapped(cargo_command(&selection, &argv, rustc.as_deref()), &args, mode);
        }
    }
}

fn fail(message: &str) -> ! {
    eprintln!("fRust: {message}");
    std::process::exit(1);
}

fn lossy_args(raw: &[OsString]) -> Vec<String> {
    raw.iter().map(|arg| arg.to_string_lossy().into_owned()).collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CargoSource {
    /// `FRUST_CARGO` named the binary.
    Explicit,
    /// A `cargo` beside the running fcargo.
    Packaged,
    /// Bare `cargo`, left to PATH lookup.
    Search,
}

#[derive(Debug, PartialEq, Eq)]
struct Selection {
    cargo: PathBuf,
    source: CargoSource,
}

/// `exe` is the canonical path of the running fcargo, so symlinks and PATH lookups land in the toolchain bin directory.
fn select_cargo(explicit: Option<OsString>, exe: Option<&Path>, is_file: impl Fn(&Path) -> bool) -> Selection {
    if let Some(path) = explicit.filter(|path| !path.is_empty()) {
        return Selection { cargo: PathBuf::from(path), source: CargoSource::Explicit };
    }
    if let Some(sibling) = exe.and_then(Path::parent).map(|dir| dir.join("cargo")) {
        if is_file(sibling.as_path()) {
            return Selection { cargo: sibling, source: CargoSource::Packaged };
        }
    }
    Selection { cargo: PathBuf::from("cargo"), source: CargoSource::Search }
}

/// RUSTC is left alone when the user set it or when a `+toolchain` must choose its own compiler.
/// `cargo clippy` gets the toolchain's `rustc` instead of `frustc`: clippy-driver runs as a
/// workspace wrapper and only recognises the compiler argument cargo passes when its file stem is
/// `rustc`. It is the same binary, so only the frustc-named behaviours are skipped.
fn select_frustc(
    args: &[String],
    rustc_set: bool,
    selection: &Selection,
    exe: Option<&Path>,
    is_file: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    if rustc_set || leading_toolchain(args).is_some() {
        return None;
    }
    let name = if subcommand(args) == Some("clippy") { "rustc" } else { "frustc" };
    let beside_cargo = selection.cargo.parent().filter(|dir| !dir.as_os_str().is_empty()).map(|dir| dir.join(name));
    let beside_exe = if selection.source == CargoSource::Search {
        exe.and_then(Path::parent).map(|dir| dir.join(name))
    } else {
        None
    };
    beside_cargo.into_iter().chain(beside_exe).find(|path| is_file(path.as_path()))
}

fn leading_toolchain(args: &[String]) -> Option<&str> {
    args.first().map(String::as_str).filter(|arg| arg.starts_with('+') && arg.len() > 1)
}

/// Only rustup's cargo proxy understands `+toolchain`; the packaged cargo would reject it.
fn toolchain_error(toolchain: &str, selection: &Selection, rustup_proxy: impl FnOnce() -> bool) -> Option<String> {
    if selection.source == CargoSource::Search || rustup_proxy() {
        return None;
    }
    Some(format!(
        "`{toolchain}` needs rustup's cargo proxy, but the selected cargo ({}) is not one. \
         Run `cargo {toolchain} ...` through rustup, or set FRUST_CARGO to rustup's cargo proxy.",
        selection.cargo.display()
    ))
}

fn is_rustup_proxy(cargo: &Path) -> bool {
    let Some(resolved) = resolve_program(cargo, env::var_os("PATH").as_deref(), |path| path.is_file()) else {
        return false;
    };
    if resolved.canonicalize().ok().and_then(|real| real.file_name().map(|name| name == "rustup")) == Some(true) {
        return true;
    }
    // Rustup installs proxies as hard links to its own binary.
    let Some(rustup) = resolved.parent().map(|dir| dir.join("rustup")) else { return false };
    match (std::fs::metadata(&resolved), std::fs::metadata(&rustup)) {
        (Ok(cargo), Ok(rustup)) => cargo.dev() == rustup.dev() && cargo.ino() == rustup.ino(),
        _ => false,
    }
}

fn resolve_program(program: &Path, path_var: Option<&OsStr>, is_file: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    if program.components().count() > 1 {
        return Some(program.to_path_buf());
    }
    env::split_paths(path_var?).map(|dir| dir.join(program)).find(|candidate| is_file(candidate.as_path()))
}

/// Executing a cargo that is this binary would recurse forever.
fn runs_self(cargo: &Path, exe: &Path, path_var: Option<&OsStr>) -> bool {
    resolve_program(cargo, path_var, |path| path.is_file())
        .and_then(|path| path.canonicalize().ok())
        .is_some_and(|path| path == exe)
}

#[derive(Debug, PartialEq, Eq)]
enum Launch {
    Passthrough,
    Wrapped { insert_at: usize },
}

fn launch_plan(mode: ProgressMode, args: &[String]) -> Launch {
    if mode == ProgressMode::Plain {
        return Launch::Passthrough;
    }
    let Some(index) = subcommand_index(args) else { return Launch::Passthrough };
    if !BUILD_SUBCOMMANDS.contains(&args[index].as_str()) {
        return Launch::Passthrough;
    }
    let end = separator(args);
    if args[..end].iter().any(|arg| user_controls_output(arg)) {
        return Launch::Passthrough;
    }
    Launch::Wrapped { insert_at: end }
}

/// Position of the standalone `--` that starts the program arguments, or the argument count.
fn separator(args: &[String]) -> usize {
    args.iter().position(|arg| arg == "--").unwrap_or(args.len())
}

fn user_controls_output(arg: &str) -> bool {
    matches!(arg, "--quiet" | "-q" | "--json" | "--help" | "-h" | "--message-format") || arg.starts_with("--message-format=")
}

/// The first argument that is neither a leading `+toolchain`, a global option, nor an option value.
fn subcommand_index(args: &[String]) -> Option<usize> {
    let mut index = usize::from(leading_toolchain(args).is_some());
    while let Some(arg) = args.get(index) {
        if arg == "--" {
            return None;
        }
        if !arg.starts_with('-') {
            return Some(index);
        }
        index += if VALUE_OPTIONS.contains(&arg.as_str()) { 2 } else { 1 };
    }
    None
}

fn subcommand(args: &[String]) -> Option<&str> {
    subcommand_index(args).map(|index| args[index].as_str())
}

fn inject(raw: &[OsString], at: usize, arg: &str) -> Vec<OsString> {
    let mut argv = raw.to_vec();
    argv.insert(at.min(argv.len()), OsString::from(arg));
    argv
}

fn json_format(ansi: bool) -> &'static str {
    if ansi {
        "--message-format=json-diagnostic-rendered-ansi"
    } else {
        "--message-format=json"
    }
}

/// Explicit `--color`, then CARGO_TERM_COLOR, then NO_COLOR, then whether the progress UI is coloured.
fn wants_ansi(args: &[String], term_color: Option<&str>, no_color: bool, progress_color: bool) -> bool {
    match color_choice(args).or(term_color) {
        Some("always") => true,
        Some("never") => false,
        _ => !no_color && progress_color,
    }
}

fn color_choice(args: &[String]) -> Option<&str> {
    let cargo_args = &args[..separator(args)];
    let mut found = None;
    for (index, arg) in cargo_args.iter().enumerate() {
        if arg == "--color" {
            found = cargo_args.get(index + 1).map(String::as_str);
        } else if let Some(value) = arg.strip_prefix("--color=") {
            found = Some(value);
        }
    }
    found
}

fn cargo_command(selection: &Selection, args: &[OsString], rustc: Option<&Path>) -> Command {
    let mut command = Command::new(&selection.cargo);
    command.args(args).env("FRUST_INVOCATION", "fcargo");
    if let Some(rustc) = rustc {
        command.env("RUSTC", rustc);
    }
    // The packaged toolchain's own cargo-clippy, cargo-fmt and rustfmt must win over a rustup proxy
    // of the same name that happens to come earlier on PATH.
    if selection.source == CargoSource::Packaged {
        if let Some(dir) = selection.cargo.parent().filter(|dir| !dir.as_os_str().is_empty()) {
            if let Some(path) = path_with_first(dir, env::var_os("PATH").as_deref()) {
                command.env("PATH", path);
            }
        }
    }
    command
}

/// `path` with `dir` as its first entry; any later copy of `dir` is dropped.
fn path_with_first(dir: &Path, path: Option<&OsStr>) -> Option<OsString> {
    let rest = path.map(env::split_paths).into_iter().flatten().filter(|entry| entry != dir);
    env::join_paths(std::iter::once(dir.to_path_buf()).chain(rest)).ok()
}

fn exec_cargo(mut command: Command) -> ! {
    let err = command.exec();
    eprintln!("fRust could not run {}: {err}", command.get_program().to_string_lossy());
    std::process::exit(101);
}

enum CargoEvent {
    Stderr(Vec<u8>),
    /// `whole` is true when the bytes are one complete stdout line.
    Stdout { bytes: Vec<u8>, whole: bool },
}

/// Interactive runs repaint at most eight times a second.
/// A command that exits before the first frame still gets one static frame.
fn run_wrapped(mut command: Command, args: &[String], mode: ProgressMode) -> ! {
    let interactive = mode == ProgressMode::Interactive;
    if interactive {
        install_signal_handlers();
    }
    let _cursor = CursorGuard;
    let started = Instant::now();
    let mut child = match command.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
        Ok(child) => child,
        Err(err) => {
            eprintln!("fRust could not run {}: {err}", command.get_program().to_string_lossy());
            std::process::exit(101);
        }
    };
    let rx = spawn_readers(&mut child);
    let header = header_for(args);
    let mut phases = initial_phases(subcommand(args));
    let mut observed = BuildObs::default();
    let mut live = LiveSet::new();
    let mut painter = Painter::new(interactive);
    let mut last_paint: Option<Instant> = None;
    loop {
        match rx.recv_timeout(REPAINT) {
            Ok(CargoEvent::Stderr(bytes)) => painter.stderr_chunk(&bytes),
            Ok(CargoEvent::Stdout { bytes, whole }) => match whole.then(|| cargo_message(&bytes)).flatten() {
                Some(message) => {
                    let mut diagnostics = Vec::new();
                    apply_message(&message, &mut phases, &mut observed, &mut diagnostics);
                    collect_live(&message, &mut live);
                    for diagnostic in diagnostics {
                        painter.stderr_chunk(format!("{diagnostic}\n").as_bytes());
                    }
                }
                None => painter.stdout_chunk(&bytes),
            },
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        let due = last_paint.is_none_or(|stamp| stamp.elapsed() >= REPAINT);
        if interactive && started.elapsed() >= REPAINT && due {
            painter.paint(&header, &phases, mode, paint_tick(&started));
            last_paint = Some(Instant::now());
        }
    }
    let status = child.wait().unwrap_or_else(|_| std::process::exit(101));
    let phases = settle_phases(&phases, &observed, status.success());
    if env::var_os("FRUST_BUILD_REPORT").is_some() {
        painter.stderr_chunk(build_report(&phases, &observed, started.elapsed()).as_bytes());
    }
    let frame_mode = if started.elapsed() >= REPAINT || !status.success() { mode } else { ProgressMode::Static };
    painter.finish(&header, &phases, frame_mode, paint_tick(&started));
    if status.success() && !live.is_empty() {
        collect_stale(&live, args);
    }
    std::process::exit(exit_code(status));
}

/// What a successful build used, taken from cargo's own messages.
fn collect_live(message: &Json, live: &mut LiveSet) {
    match message.get("reason").and_then(Json::as_str) {
        Some("compiler-artifact") => {
            if let Some(Json::Array(names)) = message.get("filenames") {
                for name in names.iter().filter_map(Json::as_str) {
                    live.add_artifact(name);
                }
            }
            if let Some(executable) = message.get("executable").and_then(Json::as_str) {
                live.add_artifact(executable);
            }
        }
        Some("build-script-executed") => {
            if let Some(out_dir) = message.get("out_dir").and_then(Json::as_str) {
                live.add_out_dir(out_dir);
            }
        }
        _ => {}
    }
}

/// Cleanup after a successful build. It can neither change the exit code nor print more than one line.
fn collect_stale(live: &LiveSet, args: &[String]) {
    if env::var("FRUST_GC").is_ok_and(|value| value == "0") {
        return;
    }
    let verbose = env::var("FRUST_GC_VERBOSE").is_ok_and(|value| value == "1");
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_collection(live, args)));
    if let Ok((report, error)) = outcome {
        if let Some(line) = gc_notice(&report, error.as_deref(), verbose) {
            eprintln!("{line}");
        }
    }
}

/// Prunes each profile directory the build wrote to, then keeps the central store within budget.
/// Returns the combined report and the first error seen.
fn run_collection(live: &LiveSet, args: &[String]) -> (GcReport, Option<String>) {
    let now = SystemTime::now();
    let config = GcConfig::default();
    let mut total = GcReport::default();
    let mut first_error: Option<String> = None;
    let mut note = |err: io::Error| {
        first_error.get_or_insert_with(|| err.to_string());
    };
    let profiles = live.profile_dirs();
    for profile in &profiles {
        match collect_profile_dir(profile, live, &config, now) {
            Ok(report) => total.merge(&report),
            Err(err) => note(err),
        }
    }
    let Some(roots) = host_roots() else { return (total, first_error) };
    let Some(slot) = profiles.iter().find_map(|profile| slot_root(&roots.targets_dir, profile)) else {
        return (total, first_error);
    };
    if let Err(err) = touch_slot_marker(&slot, &find_workspace_root(args)) {
        note(err);
    }
    if central_due(&roots.target_root, now) {
        let budget = resolve_max_bytes(&roots.target_root, roots.cache_max_bytes);
        match collect_central(&roots.target_root, budget, &slot, now) {
            Ok(report) => total.merge(&report),
            Err(err) => note(err),
        }
    }
    (total, first_error)
}

fn host_roots() -> Option<ResolvedRoots> {
    let home = env::var_os("HOME").map(PathBuf::from).filter(|path| path.is_absolute())?;
    let os = if cfg!(target_os = "macos") { HostOs::Macos } else { HostOs::Linux };
    let xdg_config = env::var("XDG_CONFIG_HOME").ok();
    let xdg_cache = env::var("XDG_CACHE_HOME").ok();
    load_roots_from_host(os, &home, xdg_config.as_deref(), xdg_cache.as_deref()).ok()
}

/// The line to print after a cleanup, if any.
fn gc_notice(report: &GcReport, error: Option<&str>, verbose: bool) -> Option<String> {
    if verbose {
        if let Some(error) = error {
            return Some(format!("fRust: stale build data cleanup failed: {error}"));
        }
        if report.skipped_busy && report.freed_bytes == 0 {
            return Some("fRust: stale build data cleanup skipped because a build holds the target directory".to_string());
        }
    }
    (verbose || report.freed_bytes > GC_NOTICE_BYTES)
        .then(|| format!("fRust: freed {} of stale build data", format_bytes(report.freed_bytes)))
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// The workspace the build ran in: the nearest manifest at or above the start directory that
/// declares `[workspace]`, else the nearest manifest's directory.
fn find_workspace_root(args: &[String]) -> PathBuf {
    let manifest_dir = manifest_path_arg(args)
        .and_then(|manifest| Path::new(manifest).parent())
        .filter(|dir| !dir.as_os_str().is_empty());
    let start = canonical_workspace(manifest_dir.unwrap_or(Path::new(".")));
    let mut package: Option<&Path> = None;
    for dir in start.ancestors() {
        let Ok(text) = std::fs::read_to_string(dir.join("Cargo.toml")) else { continue };
        package.get_or_insert(dir);
        if text.lines().map(str::trim).any(|line| line == "[workspace]" || line.starts_with("[workspace.")) {
            return dir.to_path_buf();
        }
    }
    package.unwrap_or(&start).to_path_buf()
}

fn manifest_path_arg(args: &[String]) -> Option<&str> {
    let cargo_args = &args[..separator(args)];
    let mut found = None;
    for (index, arg) in cargo_args.iter().enumerate() {
        if arg == "--manifest-path" {
            found = cargo_args.get(index + 1).map(String::as_str);
        } else if let Some(value) = arg.strip_prefix("--manifest-path=") {
            found = Some(value);
        }
    }
    found
}

fn spawn_readers(child: &mut Child) -> mpsc::Receiver<CargoEvent> {
    let stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let (tx, rx) = mpsc::sync_channel(CHANNEL_DEPTH);
    let stderr_tx = tx.clone();
    thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match stderr.read(&mut buf) {
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
        let mut reader = BufReader::new(stdout);
        let mut at_start = true;
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match read_chunk(&mut reader, &mut buf, MAX_CHUNK, &mut at_start) {
                Ok(Some(whole)) => {
                    if tx.send(CargoEvent::Stdout { bytes: std::mem::take(&mut buf), whole }).is_err() {
                        break;
                    }
                }
                Ok(None) | Err(_) => break,
            }
        }
    });
    rx
}

/// Read one line (including its newline) or at most `limit` bytes of a longer one.
/// Returns `None` at EOF, otherwise whether the bytes are a complete line.
fn read_chunk<R: BufRead>(reader: &mut R, buf: &mut Vec<u8>, limit: usize, at_start: &mut bool) -> io::Result<Option<bool>> {
    let n = reader.by_ref().take(limit as u64).read_until(b'\n', buf)?;
    if n == 0 {
        return Ok(None);
    }
    let line_ended = buf.last() == Some(&b'\n') || n < limit;
    let whole = *at_start && line_ended;
    *at_start = line_ended;
    Ok(Some(whole))
}

/// Draws the progress frame on stderr and keeps program output intact around it.
struct Painter {
    rows: usize,
    mid_line: bool,
    interactive: bool,
    stdout_shared: bool,
}

impl Painter {
    fn new(interactive: bool) -> Self {
        Painter { rows: 0, mid_line: false, interactive, stdout_shared: interactive && stream_is_tty(1) }
    }

    fn erase(&mut self) {
        if self.rows == 0 {
            return;
        }
        let _ = write!(io::stderr(), "\u{1b}[{}A\u{1b}[J", self.rows);
        self.rows = 0;
        show_cursor();
    }

    fn note(&mut self, bytes: &[u8]) {
        if let (true, Some(last)) = (self.interactive, bytes.last()) {
            self.mid_line = *last != b'\n';
        }
    }

    fn stderr_chunk(&mut self, bytes: &[u8]) {
        self.erase();
        let mut err = io::stderr();
        let _ = err.write_all(bytes);
        let _ = err.flush();
        self.note(bytes);
    }

    fn stdout_chunk(&mut self, bytes: &[u8]) {
        if self.stdout_shared {
            self.erase();
        }
        let mut out = io::stdout().lock();
        let _ = out.write_all(bytes);
        let _ = out.flush();
        if self.stdout_shared {
            self.note(bytes);
        }
    }

    /// A frame is not drawn while a partial output line is on screen.
    fn paint(&mut self, header: &Header, phases: &[Phase], mode: ProgressMode, tick: u32) {
        if self.mid_line {
            return;
        }
        let width = term_width();
        let frame = render(header, phases, mode, width, tick, color_enabled());
        if frame.is_empty() {
            self.erase();
            return;
        }
        self.draw(&frame, width);
    }

    fn draw(&mut self, frame: &str, width: usize) {
        let mut err = io::stderr();
        if self.rows == 0 {
            hide_cursor();
        } else {
            let _ = write!(err, "\u{1b}[{}A\u{1b}[J", self.rows);
        }
        let _ = err.write_all(frame.as_bytes());
        let _ = err.flush();
        self.rows = frame_rows(frame, width);
    }

    fn finish(&mut self, header: &Header, phases: &[Phase], mode: ProgressMode, tick: u32) {
        if self.mid_line {
            let _ = io::stderr().write_all(b"\n");
            self.mid_line = false;
        }
        let width = term_width();
        let frame = render(header, phases, mode, width, tick, color_enabled());
        if self.rows == 0 {
            let _ = io::stderr().write_all(frame.as_bytes());
        } else {
            self.draw(&frame, width);
        }
        show_cursor();
    }
}

/// Terminal rows a frame occupies, counting lines that wrap at `width`.
fn frame_rows(frame: &str, width: usize) -> usize {
    frame.lines().map(|line| visible_len(line).div_ceil(width.max(1)).max(1)).sum()
}

fn visible_len(line: &str) -> usize {
    let mut count = 0;
    let mut chars = line.chars();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            count += 1;
        } else if chars.next() == Some('[') {
            // A CSI sequence ends at its first byte in 0x40..=0x7e.
            for next in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&next) {
                    break;
                }
            }
        }
    }
    count
}

fn paint_tick(started: &Instant) -> u32 {
    (started.elapsed().as_millis() / 125) as u32
}

fn hide_cursor() {
    // The flag is set first so a signal arriving mid-write still restores the cursor.
    if !CURSOR_HIDDEN.swap(true, Ordering::SeqCst) {
        let _ = io::stderr().write_all(b"\x1b[?25l");
    }
}

fn show_cursor() {
    if CURSOR_HIDDEN.swap(false, Ordering::SeqCst) {
        let _ = io::stderr().write_all(SHOW_CURSOR);
    }
}

/// Restores the cursor when the main thread unwinds from a panic.
struct CursorGuard;

impl Drop for CursorGuard {
    fn drop(&mut self) {
        show_cursor();
    }
}

extern "C" fn restore_cursor_and_reraise(signum: c_int) {
    // Only async-signal-safe calls: write, signal, raise.
    unsafe {
        if CURSOR_HIDDEN.load(Ordering::SeqCst) {
            write(2, SHOW_CURSOR.as_ptr().cast(), SHOW_CURSOR.len());
        }
        signal(signum, 0);
        raise(signum);
    }
}

fn install_signal_handlers() {
    let handler = restore_cursor_and_reraise as extern "C" fn(c_int) as usize;
    for signum in [2, 15] {
        unsafe {
            signal(signum, handler);
        }
    }
}

fn exit_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or_else(|| status.signal().map_or(1, |signum| 128 + signum))
}

fn stderr_is_tty() -> bool {
    stream_is_tty(2)
}

fn stream_is_tty(fd: c_int) -> bool {
    unsafe { isatty(fd) == 1 }
}

fn color_enabled() -> bool {
    match env::var("FRUST_PROGRESS_COLOR").ok().as_deref() {
        Some("always") => true,
        Some("never") => false,
        _ => stderr_is_tty() && env::var("TERM").ok().as_deref() != Some("dumb"),
    }
}

fn ioctl_columns(fd: c_int) -> Option<usize> {
    let mut size = Winsize { rows: 0, cols: 0, xpixel: 0, ypixel: 0 };
    let ok = unsafe { ioctl(fd, TIOCGWINSZ, &mut size as *mut Winsize) } == 0;
    (ok && size.cols > 0).then_some(usize::from(size.cols))
}

fn term_width() -> usize {
    let queried = [2, 1].into_iter().find_map(ioctl_columns);
    resolve_width(queried, env::var("COLUMNS").ok().as_deref())
}

/// The terminal's own report wins, then COLUMNS, then 80.
fn resolve_width(queried: Option<usize>, columns: Option<&str>) -> usize {
    queried
        .filter(|width| *width > 0)
        .or_else(|| columns.and_then(|text| text.trim().parse().ok()).filter(|width| *width > 0))
        .unwrap_or(80)
}

fn header_for(args: &[String]) -> Header {
    Header { subtitle: subcommand(args).unwrap_or("build").to_string() }
}

fn initial_phases(subcommand: Option<&str>) -> Vec<Phase> {
    let mut phases = vec![Phase {
        name: "Resolve".into(),
        mark: PhaseMark::Active,
        detail: String::new(),
        completed: None,
        total: None,
    }];
    if matches!(subcommand, Some("test" | "t" | "bench")) {
        phases.push(pending("Test"));
    } else {
        phases.push(pending("Compile"));
    }
    phases
}

fn pending(name: &str) -> Phase {
    Phase { name: name.into(), mark: PhaseMark::Pending, detail: String::new(), completed: None, total: None }
}

#[derive(Default, Debug, PartialEq, Eq)]
struct BuildObs {
    units: u32,
    rebuilt: u32,
}

impl BuildObs {
    fn all_fresh(&self) -> bool {
        self.units > 0 && self.rebuilt == 0
    }
}

fn work_phase(phases: &[Phase]) -> &'static str {
    if phases.iter().any(|phase| phase.name == "Test") {
        "Test"
    } else {
        "Compile"
    }
}

fn apply_message(message: &Json, phases: &mut [Phase], observed: &mut BuildObs, diagnostics: &mut Vec<String>) {
    let Some(kind) = message.get("reason").and_then(Json::as_str) else { return };
    match kind {
        "compiler-artifact" | "build-script-executed" => {
            observed.units += 1;
            if kind == "compiler-artifact" && message.get("fresh").and_then(Json::as_bool) != Some(true) {
                observed.rebuilt += 1;
            }
            mark(phases, "Resolve", PhaseMark::Done, "Dependencies ready");
            let name = work_phase(phases);
            mark(phases, name, PhaseMark::Active, String::new());
        }
        "compiler-message" => {
            let diagnostic = message.get("message");
            if let Some(rendered) = diagnostic.and_then(|inner| inner.get("rendered")).and_then(Json::as_str) {
                diagnostics.push(rendered.trim_end_matches('\n').to_string());
            }
            let level = diagnostic.and_then(|inner| inner.get("level")).and_then(Json::as_str);
            if matches!(level, Some("error" | "error: internal compiler error")) {
                let name = work_phase(phases);
                mark(phases, name, PhaseMark::Failed, "rustc reported an error");
            }
        }
        "build-finished" => {
            let name = work_phase(phases);
            if message.get("success").and_then(Json::as_bool) == Some(true) {
                mark(phases, "Resolve", PhaseMark::Done, "Dependencies ready");
                if observed.all_fresh() {
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

fn mark(phases: &mut [Phase], name: &str, state: PhaseMark, detail: impl Into<String>) {
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

/// Final phases once cargo has exited: finished on success, a failed phase otherwise.
fn settle_phases(phases: &[Phase], observed: &BuildObs, success: bool) -> Vec<Phase> {
    if success {
        return final_phases(phases, observed);
    }
    let mut phases = phases.to_vec();
    if !phases.iter().any(|phase| phase.mark == PhaseMark::Failed) {
        let name = work_phase(&phases);
        mark(&mut phases, name, PhaseMark::Failed, "cargo reported failure");
    }
    phases
}

fn final_phases(phases: &[Phase], observed: &BuildObs) -> Vec<Phase> {
    let mut phases = phases.to_vec();
    for phase in &mut phases {
        if phase.mark == PhaseMark::Active || phase.mark == PhaseMark::Pending {
            if observed.all_fresh() && phase.name != "Resolve" {
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

/// Reports what fcargo observed. Cargo's message stream carries no per-phase compiler timing,
/// so analysis, code generation, LTO and link durations are not reported.
fn build_report(phases: &[Phase], observed: &BuildObs, elapsed: Duration) -> String {
    let mut out = String::from("fRust build report\n");
    for phase in phases {
        let state = mark_word(phase.mark);
        if phase.detail.is_empty() {
            out.push_str(&format!("  {:<12} {state}\n", phase.name));
        } else {
            out.push_str(&format!("  {:<12} {state}: {}\n", phase.name, phase.detail));
        }
    }
    out.push_str(&format!("  {:<12} {} seen, {} rebuilt\n", "Units", observed.units, observed.rebuilt));
    out.push_str(&format!("  {:<12} {:.2}s\n", "Elapsed", elapsed.as_secs_f64()));
    out.push_str("  Compiler phase timing is not available from cargo's message stream\n");
    out
}

fn mark_word(mark: PhaseMark) -> &'static str {
    match mark {
        PhaseMark::Done => "done",
        PhaseMark::Active => "active",
        PhaseMark::Pending => "pending",
        PhaseMark::Skipped => "skipped",
        PhaseMark::Failed => "failed",
    }
}

/// Parsed JSON, kept just detailed enough to read cargo's messages.
#[derive(Debug, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    Number,
    Str(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(fields) => fields.iter().find(|(name, _)| name == key).map(|(_, value)| value),
            _ => None,
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(text) => Some(text),
            _ => None,
        }
    }

    fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(flag) => Some(*flag),
            _ => None,
        }
    }
}

/// A stdout line is a cargo message only if it is a JSON object with a string `reason` at the top level.
fn cargo_message(line: &[u8]) -> Option<Json> {
    let text = std::str::from_utf8(line).ok()?.trim_matches([' ', '\t', '\r', '\n']);
    if !text.starts_with('{') {
        return None;
    }
    let value = parse_json(text)?;
    value.get("reason")?.as_str()?;
    Some(value)
}

fn parse_json(text: &str) -> Option<Json> {
    let mut parser = JsonParser { text, pos: 0 };
    let value = parser.value(0)?;
    parser.skip_ws();
    (parser.pos == text.len()).then_some(value)
}

struct JsonParser<'a> {
    text: &'a str,
    pos: usize,
}

impl JsonParser<'_> {
    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            self.pos += 1;
        }
    }

    fn eat(&mut self, byte: u8) -> bool {
        let found = self.peek() == Some(byte);
        if found {
            self.pos += 1;
        }
        found
    }

    fn literal(&mut self, word: &str, value: Json) -> Option<Json> {
        if self.text.as_bytes()[self.pos..].starts_with(word.as_bytes()) {
            self.pos += word.len();
            Some(value)
        } else {
            None
        }
    }

    fn value(&mut self, depth: usize) -> Option<Json> {
        if depth > MAX_JSON_DEPTH {
            return None;
        }
        self.skip_ws();
        match self.peek()? {
            b'{' => self.object(depth),
            b'[' => self.array(depth),
            b'"' => self.string().map(Json::Str),
            b't' => self.literal("true", Json::Bool(true)),
            b'f' => self.literal("false", Json::Bool(false)),
            b'n' => self.literal("null", Json::Null),
            b'-' | b'0'..=b'9' => self.number(),
            _ => None,
        }
    }

    fn object(&mut self, depth: usize) -> Option<Json> {
        self.pos += 1;
        let mut fields = Vec::new();
        self.skip_ws();
        if self.eat(b'}') {
            return Some(Json::Object(fields));
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            self.skip_ws();
            if !self.eat(b':') {
                return None;
            }
            fields.push((key, self.value(depth + 1)?));
            self.skip_ws();
            if self.eat(b'}') {
                return Some(Json::Object(fields));
            }
            if !self.eat(b',') {
                return None;
            }
        }
    }

    fn array(&mut self, depth: usize) -> Option<Json> {
        self.pos += 1;
        self.skip_ws();
        let mut items = Vec::new();
        if self.eat(b']') {
            return Some(Json::Array(items));
        }
        loop {
            items.push(self.value(depth + 1)?);
            self.skip_ws();
            if self.eat(b']') {
                return Some(Json::Array(items));
            }
            if !self.eat(b',') {
                return None;
            }
        }
    }

    fn string(&mut self) -> Option<String> {
        if !self.eat(b'"') {
            return None;
        }
        let start = self.pos;
        let bytes = self.text.as_bytes();
        let mut end = start;
        loop {
            match *bytes.get(end)? {
                b'"' => break,
                b'\\' => end += 2,
                _ => end += 1,
            }
        }
        // The slice ends at the closing quote, which the decoder expects.
        let decoded = unescape_json_string(&self.text[start..=end])?;
        self.pos = end + 1;
        Some(decoded)
    }

    fn number(&mut self) -> Option<Json> {
        let start = self.pos;
        while matches!(self.peek(), Some(b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')) {
            self.pos += 1;
        }
        self.text[start..self.pos].parse::<f64>().ok().map(|_| Json::Number)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const ARTIFACT_STALE: &str = r#"{"reason":"compiler-artifact","package_id":"probe 0.1.0 (path+file:///p)","manifest_path":"/p/Cargo.toml","target":{"kind":["bin"],"crate_types":["bin"],"name":"probe","src_path":"/p/src/main.rs","edition":"2021","doc":true,"doctest":false,"test":true},"profile":{"opt_level":"0","debuginfo":2,"debug_assertions":true,"overflow_checks":true,"test":false},"features":[],"filenames":["/t/debug/probe"],"executable":"/t/debug/probe","fresh":false}"#;
    const ARTIFACT_FRESH: &str = r#"{"reason":"compiler-artifact","package_id":"probe 0.1.0 (path+file:///p)","target":{"name":"probe"},"filenames":["/t/debug/probe"],"fresh":true}"#;
    const ERROR_MESSAGE: &str = r#"{"reason":"compiler-message","package_id":"probe 0.1.0 (path+file:///p)","target":{"name":"probe"},"message":{"rendered":"error[E0308]: mismatched types\n --> src/main.rs:2:20\n\n","$message_type":"diagnostic","children":[],"code":{"code":"E0308","explanation":null},"level":"error","message":"mismatched types","spans":[]}}"#;
    const WARNING_MESSAGE: &str = r#"{"reason":"compiler-message","target":{"name":"probe"},"message":{"rendered":"warning: unused variable\n","level":"warning","message":"unused variable","spans":[]}}"#;
    const FINISHED_OK: &str = r#"{"reason":"build-finished","success":true}"#;
    const FINISHED_FAILED: &str = r#"{"reason":"build-finished","success":false}"#;

    fn strs(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    fn message(text: &str) -> Json {
        cargo_message(text.as_bytes()).unwrap_or_else(|| panic!("not a cargo message: {text}"))
    }

    fn feed(texts: &[&str], phases: &mut [Phase], observed: &mut BuildObs) -> Vec<String> {
        let mut diagnostics = Vec::new();
        for text in texts {
            apply_message(&message(text), phases, observed, &mut diagnostics);
        }
        diagnostics
    }

    fn phase<'a>(phases: &'a [Phase], name: &str) -> &'a Phase {
        phases.iter().find(|phase| phase.name == name).unwrap()
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("fcargo-unit-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn subcommand_detection_skips_global_options_and_values() {
        assert_eq!(subcommand(&strs(&["build"])), Some("build"));
        assert_eq!(subcommand(&strs(&["--manifest-path", "x/Cargo.toml", "build"])), Some("build"));
        assert_eq!(subcommand(&strs(&["-Z", "unstable-options", "test"])), Some("test"));
        assert_eq!(subcommand(&strs(&["--config", "a=b", "-C", "dir", "--color", "always", "-j", "4", "check"])), Some("check"));
        assert_eq!(subcommand(&strs(&["--target-dir", "t", "--locked", "run"])), Some("run"));
        assert_eq!(subcommand(&strs(&["-Zfoo", "--color=never", "-j4", "-v", "doc"])), Some("doc"));
        assert_eq!(subcommand(&strs(&["+nightly", "-q", "clippy"])), Some("clippy"));
        assert_eq!(subcommand(&strs(&["--version"])), None);
        assert_eq!(subcommand(&strs(&["--", "build"])), None);
        assert_eq!(subcommand(&strs(&["--color"])), None);
        assert_eq!(subcommand(&[]), None);
    }

    #[test]
    fn header_reports_the_subcommand_not_an_option_value() {
        assert_eq!(header_for(&strs(&["--manifest-path", "x", "test"])).subtitle, "test");
        assert_eq!(header_for(&strs(&["-C", "dir"])).subtitle, "build");
    }

    #[test]
    fn launch_plan_wraps_build_like_subcommands() {
        for sub in ["build", "b", "check", "c", "test", "t", "bench", "run", "r", "doc", "d", "clippy", "rustc", "rustdoc"] {
            let args = strs(&[sub, "--release"]);
            assert_eq!(launch_plan(ProgressMode::Interactive, &args), Launch::Wrapped { insert_at: 2 }, "{sub}");
            assert_eq!(launch_plan(ProgressMode::Static, &args), Launch::Wrapped { insert_at: 2 }, "{sub}");
        }
    }

    #[test]
    fn injection_lands_before_the_separator() {
        let args = strs(&["test", "--lib", "--", "--nocapture"]);
        let Launch::Wrapped { insert_at } = launch_plan(ProgressMode::Static, &args) else { panic!("expected wrapping") };
        assert_eq!(insert_at, 2);
        let raw: Vec<OsString> = args.iter().map(OsString::from).collect();
        let argv = lossy_args(&inject(&raw, insert_at, json_format(false)));
        assert_eq!(argv, strs(&["test", "--lib", "--message-format=json", "--", "--nocapture"]));
        let run = strs(&["run", "--", "a"]);
        assert_eq!(launch_plan(ProgressMode::Static, &run), Launch::Wrapped { insert_at: 1 });
        let tail = lossy_args(&inject(&[OsString::from("build")], 1, json_format(true)));
        assert_eq!(tail, strs(&["build", "--message-format=json-diagnostic-rendered-ansi"]));
    }

    #[test]
    fn non_build_subcommands_pass_straight_through() {
        for args in [
            &["--version"][..],
            &["-V"],
            &["clean"],
            &["new", "demo"],
            &["add", "serde"],
            &["init"],
            &["install", "ripgrep"],
            &["metadata", "--format-version", "1"],
            &["fmt"],
            &["xtask", "build"],
            &["help", "build"],
            &["--manifest-path", "x", "clean"],
            &[],
        ] {
            assert_eq!(launch_plan(ProgressMode::Interactive, &strs(args)), Launch::Passthrough, "{args:?}");
        }
    }

    #[test]
    fn explicit_output_options_force_passthrough() {
        for extra in ["--message-format=json", "--message-format=short", "--quiet", "-q", "--json", "--help", "-h"] {
            let args = strs(&["build", extra]);
            assert_eq!(launch_plan(ProgressMode::Interactive, &args), Launch::Passthrough, "{extra}");
        }
        let split = strs(&["build", "--message-format", "json"]);
        assert_eq!(launch_plan(ProgressMode::Static, &split), Launch::Passthrough);
        assert_eq!(launch_plan(ProgressMode::Plain, &strs(&["build"])), Launch::Passthrough);
    }

    #[test]
    fn options_after_the_separator_belong_to_the_program() {
        let quiet = strs(&["test", "--", "-q"]);
        assert_eq!(launch_plan(ProgressMode::Static, &quiet), Launch::Wrapped { insert_at: 1 });
        let format = strs(&["run", "--", "--message-format=json"]);
        assert_eq!(launch_plan(ProgressMode::Static, &format), Launch::Wrapped { insert_at: 1 });
    }

    #[test]
    fn ansi_follows_color_choice_then_environment() {
        assert!(wants_ansi(&strs(&["build"]), None, false, true));
        assert!(!wants_ansi(&strs(&["build"]), None, false, false));
        assert!(!wants_ansi(&strs(&["build"]), None, true, true));
        assert!(wants_ansi(&strs(&["build"]), Some("always"), false, false));
        assert!(!wants_ansi(&strs(&["build"]), Some("never"), false, true));
        assert!(wants_ansi(&strs(&["build"]), Some("auto"), false, true));
        assert!(wants_ansi(&strs(&["build", "--color", "always"]), Some("never"), true, false));
        assert!(!wants_ansi(&strs(&["--color=never", "build"]), Some("always"), false, true));
        assert!(!wants_ansi(&strs(&["test", "--", "--color", "always"]), None, false, false));
        assert_eq!(json_format(true), "--message-format=json-diagnostic-rendered-ansi");
        assert_eq!(json_format(false), "--message-format=json");
    }

    #[test]
    fn cargo_messages_need_a_top_level_string_reason() {
        assert!(cargo_message(ARTIFACT_STALE.as_bytes()).is_some());
        assert!(cargo_message(format!("  {FINISHED_OK}\r\n").as_bytes()).is_some());
        assert!(cargo_message(br#"{"reason":"x"}"#).is_some());
        // Program output that merely mentions the text is not a message.
        assert!(cargo_message(br#"saw "reason":"compiler-artifact" in the log"#).is_none());
        assert!(cargo_message(br#"{"data":{"reason":"nested"}}"#).is_none());
        assert!(cargo_message(br#"{"reason":1}"#).is_none());
        assert!(cargo_message(br#"{"reason":null}"#).is_none());
        assert!(cargo_message(br#"{"reason":"x"} trailing"#).is_none());
        assert!(cargo_message(br#"{"reason":"unterminated"#).is_none());
        assert!(cargo_message(br#"["reason","x"]"#).is_none());
        assert!(cargo_message(b"").is_none());
        assert!(cargo_message(b"\n").is_none());
        assert!(cargo_message(b"{\"reason\":\"x\",\"junk\":\xff}").is_none());
        assert!(cargo_message(b"{\"reason\":\"\xff\"}").is_none());
    }

    #[test]
    fn json_parser_reads_nested_values_and_escapes() {
        let value = parse_json(r#"{"a":[1,-2.5e3,true,false,null,{"b":"x"}],"c":{},"d":"x\n\u001b\"y","e":true}"#).unwrap();
        assert!(matches!(value.get("a"), Some(Json::Array(items)) if items.len() == 6 && items[0] == Json::Number));
        assert_eq!(value.get("c"), Some(&Json::Object(Vec::new())));
        assert_eq!(value.get("d").and_then(Json::as_str), Some("x\n\u{1b}\"y"));
        assert_eq!(value.get("e").and_then(Json::as_bool), Some(true));
        assert_eq!(value.get("missing"), None);
        assert_eq!(parse_json("[]"), Some(Json::Array(Vec::new())));
        assert_eq!(parse_json(r#"["a",true]"#), Some(Json::Array(vec![Json::Str("a".into()), Json::Bool(true)])));
        assert!(parse_json("[1,]").is_none());
        assert!(parse_json("[1 2]").is_none());
        assert!(parse_json("{\"a\":1,}").is_none());
        assert!(parse_json("{\"a\" 1}").is_none());
        assert!(parse_json("{\"a\":01x}").is_none());
        let deep = format!("{}{}", "[".repeat(MAX_JSON_DEPTH + 8), "]".repeat(MAX_JSON_DEPTH + 8));
        assert!(parse_json(&deep).is_none());
    }

    fn chunks(input: &[u8], limit: usize) -> Vec<(Vec<u8>, bool)> {
        let mut reader = Cursor::new(input);
        let mut at_start = true;
        let mut out = Vec::new();
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match read_chunk(&mut reader, &mut buf, limit, &mut at_start).unwrap() {
                Some(whole) => out.push((buf.clone(), whole)),
                None => return out,
            }
        }
    }

    #[test]
    fn program_output_is_forwarded_byte_exact() {
        let input = b"first\n\nbad\xff\xfe bytes\n{\"reason\":\"build-finished\",\"success\":true}\nno newline at end";
        let read = chunks(input, MAX_CHUNK);
        assert_eq!(read.len(), 5);
        assert!(read.iter().all(|(_, whole)| *whole));
        let forwarded: Vec<u8> = read
            .iter()
            .filter(|(bytes, whole)| !(*whole && cargo_message(bytes).is_some()))
            .flat_map(|(bytes, _)| bytes.clone())
            .collect();
        assert_eq!(forwarded, b"first\n\nbad\xff\xfe bytes\nno newline at end");
        assert_eq!(read[1].0, b"\n");
        assert_eq!(read[4].0, b"no newline at end");
    }

    #[test]
    fn long_lines_are_split_without_losing_bytes() {
        let read = chunks(b"abcdefgh\nz", 4);
        let expected: Vec<(Vec<u8>, bool)> =
            vec![(b"abcd".to_vec(), false), (b"efgh".to_vec(), false), (b"\n".to_vec(), false), (b"z".to_vec(), true)];
        assert_eq!(read, expected);
        assert_eq!(chunks(b"abc\n", 4), vec![(b"abc\n".to_vec(), true)]);
        assert!(chunks(b"", 4).is_empty());
        // The tail of a split message must not be mistaken for a message.
        let piece = chunks(br#"{"reason":"x"}"#, 8);
        assert!(piece.iter().all(|(_, whole)| !whole || piece.len() == 1));
    }

    #[test]
    fn toolchain_argument_needs_rustup_unless_cargo_is_searched() {
        assert_eq!(leading_toolchain(&strs(&["+nightly", "build"])), Some("+nightly"));
        assert_eq!(leading_toolchain(&strs(&["build", "+nightly"])), None);
        assert_eq!(leading_toolchain(&strs(&["+"])), None);
        let packaged = Selection { cargo: PathBuf::from("/opt/frust/bin/cargo"), source: CargoSource::Packaged };
        let message = toolchain_error("+nightly", &packaged, || false).unwrap();
        assert!(message.contains("+nightly") && message.contains("rustup") && message.contains("FRUST_CARGO"), "{message}");
        assert!(message.contains("/opt/frust/bin/cargo"), "{message}");
        let explicit = Selection { cargo: PathBuf::from("/x/cargo"), source: CargoSource::Explicit };
        assert!(toolchain_error("+1.100.0", &explicit, || false).is_some());
        assert!(toolchain_error("+nightly", &explicit, || true).is_none());
        assert!(toolchain_error("+nightly", &packaged, || true).is_none());
        let searched = Selection { cargo: PathBuf::from("cargo"), source: CargoSource::Search };
        assert!(toolchain_error("+nightly", &searched, || panic!("not consulted")).is_none());
    }

    #[test]
    fn cargo_selection_prefers_explicit_then_sibling_then_path() {
        let exe = Path::new("/opt/frust/bin/fcargo");
        let present = |path: &Path| path == Path::new("/opt/frust/bin/cargo");
        let explicit = select_cargo(Some(OsString::from("/custom/cargo")), Some(exe), present);
        assert_eq!(explicit, Selection { cargo: PathBuf::from("/custom/cargo"), source: CargoSource::Explicit });
        let packaged = select_cargo(None, Some(exe), present);
        assert_eq!(packaged, Selection { cargo: PathBuf::from("/opt/frust/bin/cargo"), source: CargoSource::Packaged });
        let empty = select_cargo(Some(OsString::new()), Some(exe), present);
        assert_eq!(empty.source, CargoSource::Packaged);
        let missing = select_cargo(None, Some(exe), |_| false);
        assert_eq!(missing, Selection { cargo: PathBuf::from("cargo"), source: CargoSource::Search });
        assert_eq!(select_cargo(None, None, present).source, CargoSource::Search);
    }

    #[test]
    fn frustc_is_chosen_unless_the_user_decides() {
        let frustc = PathBuf::from("/opt/frust/bin/frustc");
        let present = |path: &Path| path == Path::new("/opt/frust/bin/frustc");
        let exe = Path::new("/opt/frust/bin/fcargo");
        let packaged = Selection { cargo: PathBuf::from("/opt/frust/bin/cargo"), source: CargoSource::Packaged };
        assert_eq!(select_frustc(&strs(&["build"]), false, &packaged, Some(exe), present), Some(frustc.clone()));
        assert_eq!(select_frustc(&strs(&["build"]), true, &packaged, Some(exe), present), None);
        assert_eq!(select_frustc(&strs(&["+nightly", "build"]), false, &packaged, Some(exe), present), None);
        let searched = Selection { cargo: PathBuf::from("cargo"), source: CargoSource::Search };
        assert_eq!(select_frustc(&strs(&["build"]), false, &searched, Some(exe), present), Some(frustc));
        assert_eq!(select_frustc(&strs(&["build"]), false, &searched, None, present), None);
        let explicit = Selection { cargo: PathBuf::from("/elsewhere/cargo"), source: CargoSource::Explicit };
        assert_eq!(select_frustc(&strs(&["build"]), false, &explicit, Some(exe), present), None);
    }

    #[test]
    fn the_toolchain_directory_leads_path_exactly_once() {
        let dir = Path::new("/opt/frust/bin");
        let joined = path_with_first(dir, Some(OsStr::new("/usr/bin:/opt/frust/bin:/bin"))).unwrap();
        assert_eq!(joined, OsString::from("/opt/frust/bin:/usr/bin:/bin"));
        assert_eq!(path_with_first(dir, None).unwrap(), OsString::from("/opt/frust/bin"));
    }

    #[test]
    fn clippy_gets_the_rustc_name_that_clippy_driver_recognises() {
        let rustc = PathBuf::from("/opt/frust/bin/rustc");
        let present = |path: &Path| path == Path::new("/opt/frust/bin/rustc") || path == Path::new("/opt/frust/bin/frustc");
        let exe = Path::new("/opt/frust/bin/fcargo-ui");
        let packaged = Selection { cargo: PathBuf::from("/opt/frust/bin/cargo"), source: CargoSource::Packaged };
        assert_eq!(select_frustc(&strs(&["clippy"]), false, &packaged, Some(exe), present), Some(rustc.clone()));
        assert_eq!(select_frustc(&strs(&["--manifest-path", "x/Cargo.toml", "clippy", "--all"]), false, &packaged, Some(exe), present), Some(rustc));
        assert_eq!(select_frustc(&strs(&["clippy"]), true, &packaged, Some(exe), present), None);
        assert_eq!(select_frustc(&strs(&["+nightly", "clippy"]), false, &packaged, Some(exe), present), None);
        assert_eq!(select_frustc(&strs(&["check"]), false, &packaged, Some(exe), present), Some(PathBuf::from("/opt/frust/bin/frustc")));
    }

    #[test]
    fn program_lookup_uses_path_only_for_bare_names() {
        let path_var = env::join_paths(["/a", "/b"]).unwrap();
        let in_b = |path: &Path| path == Path::new("/b/cargo");
        assert_eq!(resolve_program(Path::new("cargo"), Some(&path_var), in_b), Some(PathBuf::from("/b/cargo")));
        assert_eq!(resolve_program(Path::new("cargo"), Some(&path_var), |_| false), None);
        assert_eq!(resolve_program(Path::new("cargo"), None, in_b), None);
        assert_eq!(resolve_program(Path::new("./cargo"), None, |_| false), Some(PathBuf::from("./cargo")));
        assert_eq!(resolve_program(Path::new("/x/cargo"), None, |_| false), Some(PathBuf::from("/x/cargo")));
    }

    #[test]
    fn recursion_guard_sees_through_symlinks_and_path_lookup() {
        let dir = scratch("guard");
        let bin = dir.join("bin");
        let other = dir.join("other");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let exe = bin.join("fcargo");
        std::fs::write(&exe, "").unwrap();
        std::fs::write(other.join("cargo"), "").unwrap();
        std::os::unix::fs::symlink(&exe, other.join("self-cargo")).unwrap();
        let exe = exe.canonicalize().unwrap();
        let path_var = env::join_paths([&other]).unwrap();
        assert!(runs_self(&exe, &exe, None));
        assert!(runs_self(&other.join("self-cargo"), &exe, None));
        assert!(runs_self(Path::new("self-cargo"), &exe, Some(&path_var)));
        assert!(!runs_self(&other.join("cargo"), &exe, None));
        assert!(!runs_self(Path::new("cargo"), &exe, Some(&path_var)));
        assert!(!runs_self(Path::new("absent"), &exe, Some(&path_var)));
        assert!(!runs_self(&dir.join("nowhere/cargo"), &exe, None));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rustup_proxy_is_recognized_by_symlink_or_hard_link() {
        let dir = scratch("proxy");
        let rustup = dir.join("rustup");
        std::fs::write(&rustup, "proxy").unwrap();
        std::os::unix::fs::symlink(&rustup, dir.join("cargo-link")).unwrap();
        std::fs::hard_link(&rustup, dir.join("cargo")).unwrap();
        let real = scratch("proxy-real");
        std::fs::write(real.join("cargo"), "real").unwrap();
        std::fs::write(real.join("rustup"), "unrelated").unwrap();
        assert!(is_rustup_proxy(&dir.join("cargo-link")));
        assert!(is_rustup_proxy(&dir.join("cargo")));
        assert!(!is_rustup_proxy(&real.join("cargo")));
        assert!(!is_rustup_proxy(&real.join("missing")));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&real);
    }

    #[test]
    fn width_prefers_the_terminal_then_columns_then_default() {
        assert_eq!(resolve_width(Some(120), Some("40")), 120);
        assert_eq!(resolve_width(None, Some("40")), 40);
        assert_eq!(resolve_width(Some(0), Some(" 50 ")), 50);
        assert_eq!(resolve_width(None, Some("wide")), 80);
        assert_eq!(resolve_width(None, Some("0")), 80);
        assert_eq!(resolve_width(None, None), 80);
    }

    #[test]
    fn frame_rows_count_wrapped_lines_and_ignore_colour_codes() {
        assert_eq!(visible_len("plain"), 5);
        assert_eq!(visible_len("\u{1b}[38;5;172m[>>]\u{1b}[0m x"), 6);
        assert_eq!(frame_rows("one\ntwo\n", 80), 2);
        assert_eq!(frame_rows("0123456789\n", 10), 1);
        assert_eq!(frame_rows("01234567890\n\n", 10), 3);
        assert_eq!(frame_rows("\u{1b}[1mab\u{1b}[0m\n", 2), 1);
        assert_eq!(frame_rows("", 80), 0);
        assert_eq!(frame_rows("x\n", 0), 1);
    }

    #[test]
    fn exit_codes_follow_status_or_signal() {
        assert_eq!(exit_code(ExitStatus::from_raw(0)), 0);
        assert_eq!(exit_code(ExitStatus::from_raw(3 << 8)), 3);
        assert_eq!(exit_code(ExitStatus::from_raw(2)), 130);
        assert_eq!(exit_code(ExitStatus::from_raw(15)), 143);
    }

    #[test]
    fn initial_phases_pick_test_only_for_the_subcommand() {
        assert_eq!(phase(&initial_phases(Some("test")), "Test").mark, PhaseMark::Pending);
        assert_eq!(phase(&initial_phases(Some("t")), "Test").mark, PhaseMark::Pending);
        assert_eq!(phase(&initial_phases(Some("bench")), "Test").mark, PhaseMark::Pending);
        for sub in [Some("build"), Some("run"), None] {
            let phases = initial_phases(sub);
            assert!(phases.iter().all(|phase| phase.name != "Test"), "{sub:?}");
            assert_eq!(phase(&phases, "Compile").mark, PhaseMark::Pending);
        }
        assert_eq!(initial_phases(None)[0].mark, PhaseMark::Active);
    }

    #[test]
    fn rebuilt_artifacts_finish_as_done() {
        let mut phases = initial_phases(Some("build"));
        let mut observed = BuildObs::default();
        let diagnostics = feed(&[ARTIFACT_STALE], &mut phases, &mut observed);
        assert!(diagnostics.is_empty());
        assert_eq!(observed, BuildObs { units: 1, rebuilt: 1 });
        assert_eq!(phase(&phases, "Resolve").mark, PhaseMark::Done);
        assert_eq!(phase(&phases, "Compile").mark, PhaseMark::Active);
        feed(&[FINISHED_OK], &mut phases, &mut observed);
        assert_eq!(phase(&phases, "Compile").mark, PhaseMark::Done);
        assert_eq!(phase(&phases, "Compile").detail, "Finished");
    }

    #[test]
    fn fully_fresh_builds_are_skipped() {
        let mut phases = initial_phases(Some("build"));
        let mut observed = BuildObs::default();
        feed(&[ARTIFACT_FRESH, ARTIFACT_FRESH, FINISHED_OK], &mut phases, &mut observed);
        assert_eq!(observed, BuildObs { units: 2, rebuilt: 0 });
        assert_eq!(phase(&phases, "Compile").mark, PhaseMark::Skipped);
        assert_eq!(phase(&phases, "Compile").detail, "Reused cached units");
        let mut mixed = initial_phases(Some("build"));
        let mut seen = BuildObs::default();
        feed(&[ARTIFACT_FRESH, ARTIFACT_STALE, FINISHED_OK], &mut mixed, &mut seen);
        assert_eq!(phase(&mixed, "Compile").mark, PhaseMark::Done);
        let mut empty = initial_phases(Some("build"));
        feed(&[FINISHED_OK], &mut empty, &mut BuildObs::default());
        assert_eq!(phase(&empty, "Compile").mark, PhaseMark::Done);
    }

    #[test]
    fn compiler_errors_mark_the_work_phase_failed_and_surface_diagnostics() {
        let mut phases = initial_phases(Some("build"));
        let mut observed = BuildObs::default();
        let diagnostics = feed(&[ARTIFACT_STALE, WARNING_MESSAGE, ERROR_MESSAGE, FINISHED_FAILED], &mut phases, &mut observed);
        assert_eq!(diagnostics.len(), 2);
        assert_eq!(diagnostics[0], "warning: unused variable");
        assert_eq!(diagnostics[1], "error[E0308]: mismatched types\n --> src/main.rs:2:20");
        assert_eq!(phase(&phases, "Compile").mark, PhaseMark::Failed);
        assert_eq!(phase(&phases, "Compile").detail, "rustc reported an error");

        let mut tests = initial_phases(Some("test"));
        feed(&[ARTIFACT_STALE, ERROR_MESSAGE], &mut tests, &mut BuildObs::default());
        assert_eq!(phase(&tests, "Test").mark, PhaseMark::Failed);
        assert_eq!(phase(&tests, "Resolve").mark, PhaseMark::Done);
    }

    #[test]
    fn warnings_alone_do_not_fail_the_build() {
        let mut phases = initial_phases(Some("check"));
        let mut observed = BuildObs::default();
        feed(&[ARTIFACT_STALE, WARNING_MESSAGE, FINISHED_OK], &mut phases, &mut observed);
        assert_eq!(phase(&phases, "Compile").mark, PhaseMark::Done);
    }

    #[test]
    fn failed_build_without_a_diagnostic_still_marks_failure() {
        let mut phases = initial_phases(Some("test"));
        feed(&[FINISHED_FAILED], &mut phases, &mut BuildObs::default());
        assert_eq!(phase(&phases, "Test").mark, PhaseMark::Failed);
        assert_eq!(phase(&phases, "Test").detail, "cargo reported failure");
        let settled = settle_phases(&initial_phases(Some("build")), &BuildObs::default(), false);
        assert_eq!(phase(&settled, "Compile").mark, PhaseMark::Failed);
        let kept = settle_phases(&phases, &BuildObs::default(), false);
        assert_eq!(phase(&kept, "Test").detail, "cargo reported failure");
    }

    #[test]
    fn unrelated_messages_leave_phases_alone() {
        let mut phases = initial_phases(Some("build"));
        let before = phases.clone();
        feed(&[r#"{"reason":"build-script-something","x":1}"#, r#"{"reason":"timing-info"}"#], &mut phases, &mut BuildObs::default());
        assert_eq!(phases, before);
        let mut observed = BuildObs::default();
        feed(&[r#"{"reason":"build-script-executed","package_id":"p"}"#], &mut phases, &mut observed);
        assert_eq!(observed, BuildObs { units: 1, rebuilt: 0 });
    }

    #[test]
    fn final_phases_settle_unfinished_work() {
        let phases = initial_phases(Some("build"));
        let stale = final_phases(&phases, &BuildObs { units: 3, rebuilt: 1 });
        assert_eq!(phase(&stale, "Resolve").mark, PhaseMark::Done);
        assert_eq!(phase(&stale, "Compile").mark, PhaseMark::Done);
        assert_eq!(phase(&stale, "Compile").detail, "Finished");
        let fresh = final_phases(&phases, &BuildObs { units: 3, rebuilt: 0 });
        assert_eq!(phase(&fresh, "Resolve").mark, PhaseMark::Done);
        assert_eq!(phase(&fresh, "Compile").mark, PhaseMark::Skipped);
        assert_eq!(phase(&fresh, "Compile").detail, "Reused cached units");
        let nothing = final_phases(&phases, &BuildObs::default());
        assert_eq!(phase(&nothing, "Compile").mark, PhaseMark::Done);
        let done = settle_phases(&phases, &BuildObs { units: 1, rebuilt: 1 }, true);
        assert!(done.iter().all(|phase| phase.mark == PhaseMark::Done));
    }

    #[test]
    fn mark_never_overrides_a_failure() {
        let mut phases = initial_phases(Some("build"));
        mark(&mut phases, "Compile", PhaseMark::Failed, "boom");
        mark(&mut phases, "Compile", PhaseMark::Done, "Finished");
        assert_eq!(phase(&phases, "Compile").mark, PhaseMark::Failed);
        assert_eq!(phase(&phases, "Compile").detail, "boom");
        mark(&mut phases, "Missing", PhaseMark::Done, "ignored");
        assert_eq!(phases.len(), 2);
    }

    #[test]
    fn build_report_lists_only_observed_data() {
        let mut phases = initial_phases(Some("build"));
        let mut observed = BuildObs::default();
        feed(&[ARTIFACT_FRESH, ARTIFACT_STALE, FINISHED_OK], &mut phases, &mut observed);
        let report = build_report(&phases, &observed, Duration::from_millis(500));
        assert!(report.starts_with("fRust build report\n"), "{report}");
        assert!(report.contains(&format!("  {:<12} done: Dependencies ready\n", "Resolve")), "{report}");
        assert!(report.contains(&format!("  {:<12} done: Finished\n", "Compile")), "{report}");
        assert!(report.contains(&format!("  {:<12} 2 seen, 1 rebuilt\n", "Units")), "{report}");
        assert!(report.contains(&format!("  {:<12} 0.50s\n", "Elapsed")), "{report}");
        assert!(report.contains("not available from cargo's message stream"), "{report}");
        for label in ["Analysis", "Full LTO", "Native link"] {
            assert!(!report.contains(label), "{report}");
        }
        let pending_only = build_report(&initial_phases(Some("test")), &BuildObs::default(), Duration::ZERO);
        assert!(pending_only.contains(&format!("  {:<12} pending\n", "Test")), "{pending_only}");
    }

    #[test]
    fn live_set_is_built_from_artifacts_executables_and_build_script_output() {
        let root = scratch("live");
        let profile = root.join("debug");
        std::fs::create_dir_all(profile.join("deps")).unwrap();
        std::fs::create_dir_all(profile.join(".fingerprint")).unwrap();
        let p = profile.display();
        let lib = format!(
            r#"{{"reason":"compiler-artifact","package_id":"dep 1.0.0 (x)","target":{{"name":"dep"}},"filenames":["{p}/deps/libdep-0000000000000001.rlib","{p}/deps/libdep-0000000000000001.rmeta"],"executable":null,"fresh":true}}"#
        );
        let bin = format!(
            r#"{{"reason":"compiler-artifact","target":{{"name":"app"}},"filenames":["{p}/app"],"executable":"{p}/app","fresh":false}}"#
        );
        let script = format!(
            r#"{{"reason":"build-script-executed","package_id":"dep 1.0.0 (x)","linked_libs":[],"cfgs":[],"out_dir":"{p}/build/dep-0000000000000002/out"}}"#
        );
        let mut live = LiveSet::new();
        assert!(live.is_empty());
        let texts = [lib.as_str(), bin.as_str(), script.as_str(), ERROR_MESSAGE, WARNING_MESSAGE, FINISHED_OK];
        for text in texts {
            collect_live(&message(text), &mut live);
        }
        let expected = vec![
            profile.join("deps/libdep-0000000000000001.rlib"),
            profile.join("deps/libdep-0000000000000001.rmeta"),
            profile.join("app"),
            profile.join("app"),
        ];
        assert_eq!(live.artifacts(), expected.as_slice());
        assert_eq!(live.out_dirs(), [profile.join("build/dep-0000000000000002/out")].as_slice());
        assert_eq!(live.profile_dirs(), vec![profile.clone()]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn messages_without_usable_paths_add_nothing() {
        let mut live = LiveSet::new();
        for text in [ARTIFACT_FRESH.replace("\"filenames\":[\"/t/debug/probe\"],", "").as_str(), FINISHED_OK, FINISHED_FAILED] {
            collect_live(&message(text), &mut live);
        }
        assert!(live.is_empty());
        collect_live(&message(r#"{"reason":"compiler-artifact","filenames":[1,null,"/no/such/dir/deps/libx.rlib"],"executable":7}"#), &mut live);
        assert_eq!(live.artifacts(), [PathBuf::from("/no/such/dir/deps/libx.rlib")].as_slice());
        assert!(live.profile_dirs().is_empty(), "a path outside a cargo layout has no profile directory");
    }

    #[test]
    fn workspace_root_prefers_the_nearest_workspace_manifest() {
        let root = scratch("workspace");
        let canonical = std::fs::canonicalize(&root).unwrap();
        let member = root.join("crates").join("a");
        std::fs::create_dir_all(&member).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = [\"crates/a\"]\n").unwrap();
        std::fs::write(member.join("Cargo.toml"), "[package]\nname = \"a\"\n").unwrap();
        let manifest = member.join("Cargo.toml");
        let spaced = strs(&["build", "--manifest-path", manifest.to_str().unwrap()]);
        assert_eq!(find_workspace_root(&spaced), canonical);
        let joined = vec![format!("--manifest-path={}", manifest.display()), "check".to_string()];
        assert_eq!(find_workspace_root(&joined), canonical);
        std::fs::write(root.join("Cargo.toml"), "[workspace.package]\nversion = \"1\"\n").unwrap();
        assert_eq!(find_workspace_root(&spaced), canonical);

        let lone = scratch("lone-package");
        std::fs::write(lone.join("Cargo.toml"), "[package]\nname = \"lone\"\n").unwrap();
        let lone_manifest = lone.join("Cargo.toml");
        let args = strs(&["build", "--manifest-path", lone_manifest.to_str().unwrap()]);
        assert_eq!(find_workspace_root(&args), std::fs::canonicalize(&lone).unwrap());
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&lone);
    }

    #[test]
    fn manifest_path_option_is_read_only_before_the_separator() {
        assert_eq!(manifest_path_arg(&strs(&["build", "--manifest-path", "a/Cargo.toml"])), Some("a/Cargo.toml"));
        assert_eq!(manifest_path_arg(&strs(&["--manifest-path=b/Cargo.toml", "test"])), Some("b/Cargo.toml"));
        assert_eq!(manifest_path_arg(&strs(&["run", "--", "--manifest-path", "c/Cargo.toml"])), None);
        assert_eq!(manifest_path_arg(&strs(&["build"])), None);
        assert_eq!(manifest_path_arg(&strs(&["build", "--manifest-path"])), None);
    }

    #[test]
    fn cleanup_notices_are_quiet_unless_large_or_verbose() {
        let freed = |freed_bytes| GcReport { freed_bytes, ..GcReport::default() };
        assert_eq!(gc_notice(&freed(50 << 20), None, false), None);
        assert_eq!(gc_notice(&freed(GC_NOTICE_BYTES), None, false), None);
        assert_eq!(
            gc_notice(&freed(1_288_490_189), None, false).as_deref(),
            Some("fRust: freed 1.2 GiB of stale build data")
        );
        assert_eq!(gc_notice(&freed(50 << 20), None, true).as_deref(), Some("fRust: freed 50.0 MiB of stale build data"));
        assert_eq!(gc_notice(&freed(0), None, true).as_deref(), Some("fRust: freed 0 B of stale build data"));
        assert_eq!(gc_notice(&freed(0), Some("boom"), false), None);
        let failed = gc_notice(&freed(0), Some("boom"), true).unwrap();
        assert!(failed.contains("cleanup failed: boom"), "{failed}");
        let busy = GcReport { skipped_busy: true, ..GcReport::default() };
        assert!(gc_notice(&busy, None, true).unwrap().contains("skipped"));
        assert_eq!(gc_notice(&busy, None, false), None);
    }

    #[test]
    fn byte_counts_use_binary_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(3 << 30), "3.0 GiB");
        assert_eq!(format_bytes(1 << 40), "1.0 TiB");
        assert!(format_bytes(u64::MAX).ends_with(" TiB"));
    }
}
