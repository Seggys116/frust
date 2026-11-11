//! Opt-in page-in trace for the first-edit diagnosis: `sudo -n fs_usage -w -f pageio` around each probed build, then a per-tool summary.

use serde::Serialize;
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub(super) const TRACE_ENV: &str = "FRUST_BENCH_PAGEIN_TRACE";
pub(super) const SUMMARY_FILE: &str = "pageio-summary.txt";
pub(super) const MARKDOWN_TOP: usize = 15;
const SUMMARY_TOP: usize = 40;
const ATTACH: Duration = Duration::from_millis(300);
const FLUSH: Duration = Duration::from_millis(200);
const TERM_WAIT: Duration = Duration::from_secs(5);
const KILL_WAIT: Duration = Duration::from_secs(2);
const NO_PATH: &str = "(no path)";

pub(super) fn trace_dir() -> Option<PathBuf> {
    std::env::var_os(TRACE_ENV).filter(|v| !v.is_empty()).map(PathBuf::from)
}

// False, after a warning, when the directory cannot be made or sudo would need a password.
pub(super) fn prepare(dir: &Path) -> bool {
    if let Err(e) = fs::create_dir_all(dir) {
        eprintln!("frust bench: warning: {TRACE_ENV}: cannot create {}: {e}; skipping the page-in trace", dir.display());
        return false;
    }
    let ok = quiet(Command::new("sudo").args(["-n", "true"])).status().is_ok_and(|s| s.success());
    if !ok {
        eprintln!("frust bench: warning: {TRACE_ENV} is set but `sudo -n` is unavailable; skipping the page-in trace");
    }
    ok
}

fn quiet(cmd: &mut Command) -> &mut Command {
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
}

// A running `sudo -n fs_usage` child; dropping it stops the tracer.
pub(super) struct Tracer {
    child: Option<Child>,
}

impl Tracer {
    pub(super) fn start(file: &Path, fresh: bool) -> Option<Tracer> {
        let warn = |what: String| eprintln!("frust bench: warning: page-in trace {}: {what}; skipping it", file.display());
        let out = match OpenOptions::new().create(true).write(true).append(!fresh).truncate(fresh).open(file) {
            Ok(out) => out,
            Err(e) => {
                warn(e.to_string());
                return None;
            }
        };
        let err = out.try_clone().ok()?;
        let child = match Command::new("sudo").args(["-n", "fs_usage", "-w", "-f", "pageio"]).stdin(Stdio::null()).stdout(out).stderr(err).spawn() {
            Ok(child) => child,
            Err(e) => {
                warn(format!("cannot launch sudo fs_usage: {e}"));
                return None;
            }
        };
        let mut tracer = Tracer { child: Some(child) };
        thread::sleep(ATTACH);
        if let Some(Ok(Some(status))) = tracer.child.as_mut().map(Child::try_wait) {
            tracer.child = None;
            warn(format!("fs_usage exited before the build ({status})"));
            return None;
        }
        Some(tracer)
    }

    pub(super) fn stop(mut self) {
        self.halt();
    }

    fn halt(&mut self) {
        let Some(mut child) = self.child.take() else { return };
        thread::sleep(FLUSH);
        // The tracer runs as root, so only a sudo'd signal reaches it; -x keeps pkill from matching its own sudo.
        for (signal, wait) in [("-TERM", TERM_WAIT), ("-KILL", KILL_WAIT)] {
            let _ = quiet(Command::new("sudo").args(["-n", "pkill", signal, "-x", "fs_usage"])).status();
            if exited(&mut child, wait) {
                return;
            }
        }
        eprintln!("frust bench: warning: the page-in tracer (pid {}) did not exit after SIGKILL", child.id());
    }
}

impl Drop for Tracer {
    fn drop(&mut self) {
        self.halt();
    }
}

fn exited(child: &mut Child, wait: Duration) -> bool {
    let deadline = Instant::now() + wait;
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return true,
            Ok(None) if Instant::now() >= deadline => return false,
            Ok(None) => thread::sleep(Duration::from_millis(20)),
        }
    }
}

#[derive(Debug, PartialEq)]
pub(super) struct PageIn {
    pub(super) path: String,
    pub(super) process: Option<String>,
}

fn tokens(line: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in line.char_indices() {
        match (c.is_whitespace(), start) {
            (true, Some(s)) => {
                out.push((s, &line[s..i]));
                start = None;
            }
            (false, None) => start = Some(i),
            _ => {}
        }
    }
    if let Some(s) = start {
        out.push((s, &line[s..]));
    }
    out
}

fn process_token(token: &str) -> Option<&str> {
    let (name, id) = token.rsplit_once('.')?;
    (!name.is_empty() && !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit())).then_some(name)
}

fn elapsed_token(token: &str) -> bool {
    token.contains('.') && token.parse::<f64>().is_ok()
}

// One `fs_usage -w -f pageio` page-in line: timestamp, PgIn[..] or PAGE_IN.., fields, device, path, elapsed, W, process.tid.
pub(super) fn parse_line(line: &str) -> Option<PageIn> {
    let tokens = tokens(line);
    let kind = tokens.iter().take(3).position(|(_, t)| t.starts_with("PgIn") || t.starts_with("PAGE_IN"))?;
    let rest = &tokens[kind + 1..];
    let mut end = rest.len();
    let mut process = None;
    if end > 0 && let Some(name) = process_token(rest[end - 1].1) {
        process = Some(name.to_string());
        end -= 1;
    }
    let Some(path_at) = rest[..end].iter().position(|(_, t)| t.starts_with('/') && !t.starts_with("/dev/")) else {
        return Some(PageIn { path: NO_PATH.to_string(), process });
    };
    if end > path_at + 1 && rest[end - 1].1 == "W" {
        end -= 1;
    }
    if end > path_at + 1 && elapsed_token(rest[end - 1].1) {
        end -= 1;
    }
    let (from, _) = rest[path_at];
    let (last_at, last) = rest[end - 1];
    Some(PageIn { path: line[from..last_at + last.len()].to_string(), process })
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(super) struct Count {
    pub(super) name: String,
    pub(super) count: u64,
}

#[derive(Debug, Default, PartialEq, Serialize)]
pub(super) struct TraceSummary {
    pub(super) files: usize,
    pub(super) pageins: u64,
    pub(super) top_paths: Vec<Count>,
    pub(super) processes: Vec<Count>,
}

fn ranked(counts: BTreeMap<String, u64>, top: usize) -> Vec<Count> {
    let mut list: Vec<Count> = counts.into_iter().map(|(name, count)| Count { name, count }).collect();
    list.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.name.cmp(&b.name)));
    list.truncate(top);
    list
}

pub(super) fn summarise<'a>(texts: impl IntoIterator<Item = &'a str>, top: usize) -> TraceSummary {
    let (mut paths, mut processes) = (BTreeMap::new(), BTreeMap::new());
    let (mut files, mut pageins) = (0, 0);
    for text in texts {
        files += 1;
        for page in text.lines().filter_map(parse_line) {
            pageins += 1;
            *paths.entry(page.path).or_insert(0) += 1;
            if let Some(process) = page.process {
                *processes.entry(process).or_insert(0) += 1;
            }
        }
    }
    TraceSummary { files, pageins, top_paths: ranked(paths, top), processes: ranked(processes, top) }
}

#[derive(Debug, Serialize)]
pub(super) struct PageinTrace {
    pub(super) dir: String,
    pub(super) tools: Vec<(String, TraceSummary)>,
}

fn summary_text(trace: &PageinTrace) -> String {
    let mut out = String::new();
    for (tool, s) in &trace.tools {
        out.push_str(&format!("== {tool}: {} page-in lines in {} trace files ==\ntop paths:\n", s.pageins, s.files));
        out.extend(s.top_paths.iter().map(|c| format!("{:>8}  {}\n", c.count, c.name)));
        out.push_str("processes:\n");
        out.extend(s.processes.iter().map(|c| format!("{:>8}  {}\n", c.count, c.name)));
        out.push('\n');
    }
    out
}

// Reads every trace file written for each tool, in order, and writes the summary beside them.
pub(super) fn summarise_dir(dir: &Path, traced: &[(&str, PathBuf)], tools: &[&str]) -> PageinTrace {
    let tools = tools.iter().map(|&tool| {
        let texts: Vec<String> = traced.iter().filter(|(t, _)| *t == tool)
            .filter_map(|(_, file)| fs::read(file).ok().map(|b| String::from_utf8_lossy(&b).into_owned())).collect();
        (tool.to_string(), summarise(texts.iter().map(String::as_str), SUMMARY_TOP))
    }).collect();
    let trace = PageinTrace { dir: dir.display().to_string(), tools };
    let summary = dir.join(SUMMARY_FILE);
    if let Err(e) = fs::write(&summary, summary_text(&trace)) {
        eprintln!("frust bench: warning: cannot write {}: {e}", summary.display());
    }
    trace
}

pub(super) fn rows(trace: &PageinTrace, top: usize) -> Vec<Vec<String>> {
    let mut rows = vec![vec!["tool".to_string(), "page-ins".to_string(), "path".to_string()]];
    for (tool, s) in &trace.tools {
        rows.extend(s.top_paths.iter().take(top).map(|c| vec![tool.clone(), c.count.to_string(), c.name.clone()]));
    }
    rows
}

pub(super) fn process_lines(trace: &PageinTrace, top: usize) -> Vec<String> {
    trace.tools.iter().map(|(tool, s)| {
        let names: Vec<String> = s.processes.iter().take(top).map(|c| format!("{} {}", c.name, c.count)).collect();
        format!("{tool}: {} page-in lines in {} trace files; by process: {}", s.pageins, s.files, if names.is_empty() { "n/a".to_string() } else { names.join(", ") })
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
12:34:56.789012  PgIn[A]         D=0x0012a000  B=0x4000   /dev/disk3s5  /Users/runner/.rustup/toolchains/stable/lib/librustc_driver.dylib     0.000412 W rustc.1234567
12:34:56.789100  PgIn[AT2]       D=0x0012b000  B=0x8000   /dev/disk3s5  /Library/Developer/CommandLineTools/usr/bin/ld   0.000101 W ld.42
12:34:56.789200  PAGE_IN_FILE    D=0x0012c000  B=0x1000   /dev/disk3s5  /Users/runner/work/My Repo/target/debug/deps/libfoo.rlib  0.000050   cargo.77
12:34:56.789300  PgIn[A]         D=0x0012d000  B=0x4000   /dev/disk3s5  /Users/runner/.rustup/toolchains/stable/lib/librustc_driver.dylib     0.000398 W rustc.1234568
12:34:56.789400  RdData[A]       D=0x0012e000  B=0x4000   /dev/disk3s5  /etc/hosts   0.000010   cat.9
12:34:56.789500  PgIn[A]         D=0x0012f000  B=0x4000   /dev/disk3s5   0.000020 W kernel_task.0
garbage line without a page-in
";

    #[test]
    fn page_in_lines_yield_the_path_and_the_process_name() {
        let lines: Vec<&str> = SAMPLE.lines().collect();
        assert_eq!(parse_line(lines[0]), Some(PageIn { path: "/Users/runner/.rustup/toolchains/stable/lib/librustc_driver.dylib".into(), process: Some("rustc".into()) }));
        assert_eq!(parse_line(lines[1]).unwrap().path, "/Library/Developer/CommandLineTools/usr/bin/ld");
        assert_eq!(parse_line(lines[2]), Some(PageIn { path: "/Users/runner/work/My Repo/target/debug/deps/libfoo.rlib".into(), process: Some("cargo".into()) }));
        assert_eq!(parse_line(lines[4]), None);
        assert_eq!(parse_line(lines[5]), Some(PageIn { path: NO_PATH.into(), process: Some("kernel_task".into()) }));
        assert_eq!(parse_line(lines[6]), None);
        assert_eq!(parse_line(""), None);
        assert_eq!(parse_line("12:00:00.000001  PgIn[A]  /dev/disk1s1  /bin/ls"), Some(PageIn { path: "/bin/ls".into(), process: None }));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_tracer_never_outlives_its_build_with_or_without_sudo() {
        let file = std::env::temp_dir().join(format!("frust-pageio-test-{}.txt", std::process::id()));
        let started = Instant::now();
        if let Some(tracer) = Tracer::start(&file, true) {
            tracer.stop();
        }
        assert!(started.elapsed() < TERM_WAIT + KILL_WAIT + Duration::from_secs(2), "{:?}", started.elapsed());
        assert!(file.is_file(), "the trace file is opened before the tracer starts");
        fs::remove_file(&file).unwrap();
    }

    #[test]
    fn the_summary_ranks_paths_and_processes_across_trace_files() {
        let s = summarise([SAMPLE, SAMPLE.lines().next().unwrap()], 2);
        assert_eq!((s.files, s.pageins), (2, 6));
        assert_eq!(s.top_paths, [
            Count { name: "/Users/runner/.rustup/toolchains/stable/lib/librustc_driver.dylib".into(), count: 3 },
            Count { name: NO_PATH.into(), count: 1 },
        ]);
        assert_eq!(s.processes[0], Count { name: "rustc".into(), count: 3 });
        let trace = PageinTrace { dir: "d".into(), tools: vec![("cargo".into(), s), ("frust-tuned".into(), TraceSummary::default())] };
        assert!(summary_text(&trace).starts_with("== cargo: 6 page-in lines in 2 trace files ==\ntop paths:\n       3  /Users/runner/"));
        assert_eq!(rows(&trace, 1).len(), 2);
        assert_eq!(process_lines(&trace, 5)[1], "frust-tuned: 0 page-in lines in 0 trace files; by process: n/a");
    }
}
