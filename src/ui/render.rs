//! Pure functions from model state and a `now` to strings; nothing here writes or reads a clock.

use std::time::{Duration, Instant};

use super::model::{ActiveUnit, Completion, LockView, Model};
use super::term::{Paint, fmt_duration, fmt_ms, hyperlink, visible_width};
use super::{RunStats, UiOptions};

const MIN_BAR_WIDTH: usize = 12;
/// Wider than this the bar stops reading as one glance, so spare columns stay empty.
const MAX_BAR_WIDTH: usize = 56;
/// Columns left free at the right edge so a full line never autowraps.
const RIGHT_MARGIN: usize = 2;
pub const MAX_UNIT_LINES: usize = 4;
const VERB_COLS: usize = 9;
const FRAME_MS: u128 = 80;
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const BAR_FILL: &str = "━";
const BAR_HEAD: &str = "╸";
const BAR_TRACK: &str = "─";
const SHIMMER_WIDTH: usize = 6;
/// A unit running this long is worth noticing, so its timer turns yellow.
const SLOW_UNIT: Duration = Duration::from_secs(5);

pub const TITLE_PUSH: &str = "\x1b[22;0t";
pub const TITLE_POP: &str = "\x1b[23;0t";

pub fn header(p: Paint, opts: &UiOptions) -> String {
    format!(
        "{} {}  {}",
        p.bold("frust"),
        p.dim(&format!("{} {}", opts.command, opts.workspace)),
        p.dim(&opts.toolchain)
    )
}

pub fn plain_header(opts: &UiOptions) -> String {
    format!("frust {} {} ({})", opts.command, opts.workspace, opts.toolchain)
}

pub fn live_region(m: &Model, p: Paint, elapsed: Duration, now: Instant, width: usize) -> Vec<String> {
    if let Some(lock) = &m.lock {
        return vec![lock_line_ansi(p, lock)];
    }
    let mut lines = vec![progress_line(m, p, elapsed, width)];
    if let Some(text) = &m.transient {
        lines.push(format!("    {}", p.dim(text)));
        return lines;
    }
    // Active units are stored in start order, so the longest-running come first.
    for unit in m.active.iter().take(MAX_UNIT_LINES) {
        lines.push(unit_line(unit, p, now, width));
    }
    if m.active.len() > MAX_UNIT_LINES {
        lines.push(format!("    {}", p.dim(&format!("+ {} more", m.active.len() - MAX_UNIT_LINES))));
    }
    lines
}

fn unit_line(unit: &ActiveUnit, p: Paint, now: Instant, width: usize) -> String {
    let mut line = format!("    {} {}", p.cyan("▸"), p.bold(&unit.name));
    if let Some(v) = &unit.version {
        line.push(' ');
        line.push_str(&p.dim(&format!("v{v}")));
    }
    if unit.build_script {
        line.push(' ');
        line.push_str(&p.dim("(build script)"));
    }
    let running = now.saturating_duration_since(unit.started);
    let timer = fmt_duration(running);
    let timer = if running >= SLOW_UNIT { p.yellow(&timer) } else { p.dim(&timer) };
    let gap = width.saturating_sub(RIGHT_MARGIN + visible_width(&line) + visible_width(&timer));
    format!("{line}{}{timer}", " ".repeat(gap.max(2)))
}

fn progress_line(m: &Model, p: Paint, elapsed: Duration, width: usize) -> String {
    let frame = (elapsed.as_millis() / FRAME_MS) as usize;
    let done = m.done().to_string();
    let mut tail = String::new();
    if let (Some(total), Some(pct)) = (m.total(), m.percent()) {
        tail.push_str(&format!("  {}", p.bold(&format!("{pct:>3}%"))));
        tail.push_str(&format!("  {}{}", p.bold(&done), p.dim(&format!("/{total}"))));
    } else {
        tail.push_str(&format!("  {}", p.bold(&done)));
    }
    tail.push_str(&format!("  {}", p.dim(&fmt_duration(elapsed))));
    if m.cache_hits + m.cache_misses > 0 {
        tail.push_str(&format!(
            "  {} {} {}",
            p.green(&format!("{} hit", m.cache_hits)),
            p.dim("·"),
            p.dim(&format!("{} miss", m.cache_misses))
        ));
    }
    let lead = format!("  {} ", p.cyan(SPINNER[frame % SPINNER.len()]));
    let bar_width = width.saturating_sub(RIGHT_MARGIN + visible_width(&lead) + visible_width(&tail)).min(MAX_BAR_WIDTH);
    format!("{lead}{}{tail}", bar(m, p, frame, bar_width))
}

/// A fine line that fills in half-cell steps; without a total a short segment sweeps back and forth.
fn bar(m: &Model, p: Paint, frame: usize, width: usize) -> String {
    let width = width.max(MIN_BAR_WIDTH);
    let Some(total) = m.total() else {
        let span = width - SHIMMER_WIDTH;
        let step = frame % (2 * span);
        let at = if step <= span { step } else { 2 * span - step };
        return format!(
            "{}{}{}",
            p.dim(&BAR_TRACK.repeat(at)),
            p.cyan(&BAR_FILL.repeat(SHIMMER_WIDTH)),
            p.dim(&BAR_TRACK.repeat(width - SHIMMER_WIDTH - at))
        );
    };
    let halves = (m.done().min(total) as usize * width * 2 / total as usize).min(width * 2);
    let (full, head) = (halves / 2, halves % 2);
    let rest = width - full - head;
    format!(
        "{}{}{}",
        p.cyan(&BAR_FILL.repeat(full)),
        if head == 1 { p.cyan(BAR_HEAD) } else { String::new() },
        p.dim(&BAR_TRACK.repeat(rest))
    )
}

/// Whole seconds only: the lock age is known to the second, so fractions would be false precision.
fn fmt_whole_secs(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 { format!("{secs}s") } else { format!("{}m{:02}s", secs / 60, secs % 60) }
}

fn lock_detail(l: &LockView) -> String {
    let Some(pid) = l.pid else { return String::new() };
    let mut parts = Vec::new();
    if !l.command.is_empty() {
        parts.push(l.command.clone());
    }
    parts.push(fmt_whole_secs(l.held));
    format!("held by pid {pid} ({})", parts.join(", "))
}

pub fn lock_line_ansi(p: Paint, l: &LockView) -> String {
    let mut line = format!("  {}", p.yellow("waiting for build lock"));
    let detail = lock_detail(l);
    if !detail.is_empty() {
        line.push_str(&format!("  {}", p.dim(&detail)));
    }
    if l.ahead > 0 {
        line.push_str(&format!("  {}", p.bold(&format!("{} ahead", l.ahead))));
    }
    line
}

pub fn lock_line_plain(l: &LockView) -> String {
    let mut line = String::from("waiting   build lock");
    let detail = lock_detail(l);
    if !detail.is_empty() {
        line.push(' ');
        line.push_str(&detail);
    }
    if l.ahead > 0 {
        line.push_str(&format!(", {} ahead", l.ahead));
    }
    line
}

pub fn is_notable(c: &Completion) -> bool {
    c.local || c.duration >= Duration::from_secs(1) || (c.cached && c.saved_ms >= 1000)
}

fn time_text(c: &Completion) -> String {
    if c.cached && c.saved_ms > 0 {
        format!("saved {}", fmt_ms(c.saved_ms))
    } else {
        fmt_duration(c.duration)
    }
}

pub fn completion_ansi(c: &Completion, p: Paint, width: usize, links: bool) -> String {
    let (verb, styled) = if c.cached { ("Cached", p.cyan("Cached")) } else { ("Compiled", p.green("Compiled")) };
    let name = match (&c.version, links && !c.local) {
        (Some(v), true) => hyperlink(&format!("https://docs.rs/{}/{v}", c.name), &c.name),
        _ => c.name.clone(),
    };
    let mut left = format!("  {}{styled} {name}", " ".repeat(VERB_COLS.saturating_sub(verb.len())));
    if let Some(v) = &c.version {
        left.push(' ');
        left.push_str(&p.dim(&format!("v{v}")));
    }
    if c.build_script {
        left.push(' ');
        left.push_str(&p.dim("(build script)"));
    }
    let right = p.dim(&time_text(c));
    let used = visible_width(&left) + visible_width(&right);
    let gap = width.saturating_sub(2).saturating_sub(used).max(2);
    format!("{left}{}{right}", " ".repeat(gap))
}

pub fn completion_plain(c: &Completion) -> String {
    let verb = if c.cached { "cached" } else { "compiled" };
    let version = c.version.as_ref().map(|v| format!(" v{v}")).unwrap_or_default();
    let script = if c.build_script { " (build script)" } else { "" };
    format!("{verb:<10}{}{version}{script}  {}", c.name, time_text(c))
}


pub fn summary_ansi(p: Paint, success: bool, profile: Option<&str>, elapsed: Duration, stats: &RunStats) -> Vec<String> {
    let took = fmt_duration(elapsed);
    let first = if success {
        let text = match profile {
            Some(profile) => format!("{profile} in {took}"),
            None => format!("in {took}"),
        };
        format!("  {} {} {text}", p.bold_green("✔"), p.bold_green("Finished"))
    } else {
        format!("  {} {}", p.bold_red("✘"), p.bold_red(&format!("Failed in {took}")))
    };

    let count = |n: u32, label: &str, paint: fn(Paint, &str) -> String| format!("{} {}", p.bold(&n.to_string()), paint(p, label));
    let mut items = Vec::new();
    if stats.errors > 0 {
        items.push(count(stats.errors, if stats.errors == 1 { "error" } else { "errors" }, Paint::red));
    }
    if stats.warnings > 0 {
        items.push(count(stats.warnings, if stats.warnings == 1 { "warning" } else { "warnings" }, Paint::yellow));
    }
    items.push(count(stats.units, if stats.units == 1 { "unit" } else { "units" }, Paint::dim));
    items.push(count(stats.cached, "cached", Paint::cyan));
    items.push(count(stats.compiled, "compiled", Paint::dim));
    items.push(count(stats.fresh, "fresh", Paint::dim));
    if stats.saved_ms > 0 {
        items.push(format!("{} {}", p.dim("saved"), p.bold(&format!("~{}", fmt_ms(stats.saved_ms)))));
    }
    vec![first, format!("    {}", items.join(&p.dim("  ·  ")))]
}

/// Every counter is always present so logs diff cleanly.
pub fn summary_plain(success: bool, profile: Option<&str>, elapsed: Duration, stats: &RunStats) -> Vec<String> {
    let took = fmt_duration(elapsed);
    let first = if !success {
        format!("{:<10}in {took}", "failed")
    } else {
        match profile {
            Some(profile) => format!("{:<10}{profile} in {took}", "finished"),
            None => format!("{:<10}in {took}", "finished"),
        }
    };
    let second = format!(
        "units {}  cached {}  compiled {}  fresh {}  saved {}  warnings {}  errors {}",
        stats.units,
        stats.cached,
        stats.compiled,
        stats.fresh,
        fmt_ms(stats.saved_ms),
        stats.warnings,
        stats.errors
    );
    vec![first, second]
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Severity {
    Error,
    Warning,
}

/// Only colour is added, so columns line up as rustc drew them; an unrecognised line returns `None`.
#[derive(Default)]
pub struct DiagStyler {
    block: Option<Severity>,
}

impl DiagStyler {
    pub fn style(&mut self, p: Paint, plain: &str) -> Option<String> {
        if plain.trim().is_empty() {
            self.block = None;
            return None;
        }
        if let Some(line) = self.header(p, plain) {
            return Some(line);
        }
        if plain.starts_with("For more information about this error") {
            return Some(p.dim(plain));
        }
        let severity = self.block?;
        arrow(p, plain).or_else(|| gutter(p, plain, severity)).or_else(|| note_line(p, plain))
    }

    fn header(&mut self, p: Paint, plain: &str) -> Option<String> {
        let (severity, rest) = match plain.strip_prefix("error") {
            Some(rest) => (Severity::Error, rest),
            None => (Severity::Warning, plain.strip_prefix("warning")?),
        };
        let (code, message) = match rest.strip_prefix('[') {
            Some(inner) => {
                let (code, after) = inner.split_once("]: ")?;
                (format!("[{code}]"), after)
            }
            None => (String::new(), rest.strip_prefix(": ")?),
        };
        let label = match severity {
            Severity::Error => p.bold_red(&format!("error{code}")),
            Severity::Warning => p.bold_yellow(&format!("warning{code}")),
        };
        // Closing notes summarise what was already printed, so they recede and open no snippet block.
        let closing = message.starts_with("could not compile")
            || message.starts_with("aborting due to")
            || message.starts_with("build failed")
            || (message.starts_with('`') && message.contains(") generated "));
        if closing {
            self.block = None;
            return Some(format!("{label}{}{}", p.dim(": "), p.dim(message)));
        }
        self.block = Some(severity);
        Some(format!("{label}{} {}", p.dim(":"), p.bold(message)))
    }
}

fn arrow(p: Paint, plain: &str) -> Option<String> {
    let trimmed = plain.trim_start();
    let path = trimmed.strip_prefix("--> ")?;
    let indent = &plain[..plain.len() - trimmed.len()];
    Some(format!("{indent}{} {}", p.bold_blue("-->"), p.cyan(path)))
}

fn gutter(p: Paint, plain: &str, severity: Severity) -> Option<String> {
    let pipe = plain.find('|')?;
    let (margin, rest) = plain.split_at(pipe);
    if !margin.chars().all(|c| c.is_ascii_digit() || c == ' ') {
        return None;
    }
    let body = &rest[1..];
    let numbered = margin.chars().any(|c| c.is_ascii_digit());
    let margin_styled = p.bold_blue(margin.trim_end()) + &margin[margin.trim_end().len()..];
    let body_styled = if !numbered && body.trim_start().starts_with(['^', '-', '~', '+', '|', '_']) { underline(p, body, severity) } else { body.to_string() };
    Some(format!("{margin_styled}{}{body_styled}", p.bold_blue("|")))
}

fn underline(p: Paint, body: &str, severity: Severity) -> String {
    let paint_for = |c: char| -> fn(Paint, &str) -> String {
        match (c, severity) {
            ('^', Severity::Error) => Paint::bold_red,
            ('^', Severity::Warning) => Paint::bold_yellow,
            _ => Paint::bold_blue,
        }
    };
    let mut out = String::new();
    let mut current: Option<char> = None;
    let mut run = String::new();
    let flush = |out: &mut String, run: &mut String, current: Option<char>| {
        if !run.is_empty() {
            match current {
                Some(c) => out.push_str(&paint_for(c)(p, run)),
                None => out.push_str(run),
            }
            run.clear();
        }
    };
    for ch in body.chars() {
        let next = match ch {
            '^' | '-' | '~' | '+' | '|' | '_' => Some(ch),
            _ => current,
        };
        // A change of marker, or the label after spaces, starts a new coloured run.
        if next != current {
            flush(&mut out, &mut run, current);
            current = next;
        }
        run.push(ch);
    }
    flush(&mut out, &mut run, current);
    out
}

fn note_line(p: Paint, plain: &str) -> Option<String> {
    let trimmed = plain.trim_start();
    let indent = &plain[..plain.len() - trimmed.len()];
    let (equals, body) = match trimmed.strip_prefix("= ") {
        Some(body) => (format!("{} ", p.bold_blue("=")), body),
        None => (String::new(), trimmed),
    };
    let (label, message) = body.split_once(": ")?;
    let styled = match label {
        "note" => p.bold_cyan(label),
        "help" => p.bold_green(label),
        _ => return None,
    };
    Some(format!("{indent}{equals}{styled}{} {message}", p.dim(":")))
}

/// Strip control characters so a hostile workspace name cannot end the OSC string early.
fn sanitize(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

pub fn title_text(workspace: &str, done: u32, total: Option<u32>) -> String {
    let workspace = sanitize(workspace);
    match total {
        Some(total) => format!("frust: building {workspace} {done}/{total}"),
        None => format!("frust: building {workspace} {done}"),
    }
}

pub fn osc_title(text: &str) -> String {
    format!("\x1b]2;{}\x07", sanitize(text))
}

pub fn osc_progress(pct: u32) -> String {
    format!("\x1b]9;4;1;{pct}\x07")
}

pub fn osc_progress_indeterminate() -> String {
    "\x1b]9;4;3\x07".to_string()
}

pub fn osc_progress_error(pct: u32) -> String {
    format!("\x1b]9;4;2;{pct}\x07")
}

pub fn osc_progress_clear() -> String {
    "\x1b]9;4;0\x07".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::term::strip_ansi;

    fn completion(cached: bool, local: bool, ms: u64, saved_ms: u64) -> Completion {
        Completion {
            name: "syn".to_string(),
            version: Some("2.0.1".to_string()),
            local,
            build_script: false,
            duration: Duration::from_millis(ms),
            cached,
            saved_ms,
        }
    }

    #[test]
    fn completion_lines_are_right_aligned_to_width_minus_two() {
        let p = Paint::new(true);
        for c in [completion(false, false, 2310, 0), completion(true, false, 400, 3200)] {
            let line = completion_ansi(&c, p, 60, false);
            assert_eq!(visible_width(&line), 58);
        }
        let line = strip_ansi(&completion_ansi(&completion(false, false, 2310, 0), p, 60, false));
        assert!(line.starts_with("   Compiled syn v2.0.1"));
        assert!(line.ends_with("2.31s"));
        let cached = strip_ansi(&completion_ansi(&completion(true, false, 400, 3200), p, 60, false));
        assert!(cached.starts_with("     Cached syn v2.0.1"));
        assert!(cached.ends_with("saved 3.20s"));
    }

    #[test]
    fn links_wrap_registry_crates_only_and_add_no_width() {
        let p = Paint::new(true);
        let registry = completion_ansi(&completion(false, false, 2000, 0), p, 60, true);
        assert!(registry.contains("\x1b]8;;https://docs.rs/syn/2.0.1\x1b\\syn\x1b]8;;\x1b\\"));
        assert_eq!(visible_width(&registry), 58);
        let local = completion_ansi(&completion(false, true, 2000, 0), p, 60, true);
        assert!(!local.contains("\x1b]8"));
    }

    #[test]
    fn notable_rules() {
        assert!(!is_notable(&completion(false, false, 999, 0)));
        assert!(is_notable(&completion(false, false, 1000, 0)));
        assert!(is_notable(&completion(false, true, 5, 0)));
        assert!(is_notable(&completion(true, false, 5, 1500)));
        assert!(!is_notable(&completion(true, false, 5, 500)));
    }

    #[test]
    fn plain_completion_lines() {
        assert_eq!(completion_plain(&completion(false, false, 1400, 0)), "compiled  syn v2.0.1  1.40s");
        assert_eq!(completion_plain(&completion(true, false, 10, 3200)), "cached    syn v2.0.1  saved 3.20s");
    }

    #[test]
    fn lock_lines() {
        let l = LockView { pid: Some(4242), command: "cargo build".to_string(), held: Duration::from_secs(12), ahead: 1 };
        assert_eq!(lock_line_plain(&l), "waiting   build lock held by pid 4242 (cargo build, 12s), 1 ahead");
        assert_eq!(
            strip_ansi(&lock_line_ansi(Paint::new(true), &l)),
            "  waiting for build lock  held by pid 4242 (cargo build, 12s)  1 ahead"
        );
        let anon = LockView { pid: None, command: String::new(), held: Duration::ZERO, ahead: 0 };
        assert_eq!(lock_line_plain(&anon), "waiting   build lock");
    }

    #[test]
    fn progress_line_shows_percent_fraction_and_counters() {
        let mut m = Model::new(Some(87));
        m.stats.units = 37;
        m.cache_hits = 20;
        m.cache_misses = 5;
        let line = strip_ansi(&progress_line(&m, Paint::new(true), Duration::from_millis(12_300), 100));
        assert!(line.contains(" 42%"), "{line}");
        assert!(line.contains("37/87"), "{line}");
        assert!(!line.contains('~'), "the total is exact, not an estimate: {line}");
        assert!(line.contains("12.3s"));
        assert!(line.contains("20 hit · 5 miss"), "{line}");
        assert!(visible_width(&line) <= 100 - RIGHT_MARGIN, "never wider than the terminal");
        let narrow = strip_ansi(&progress_line(&m, Paint::new(true), Duration::from_millis(12_300), 80));
        assert_eq!(visible_width(&narrow), 80 - RIGHT_MARGIN, "the bar fills a narrow terminal");
    }

    #[test]
    fn the_bar_fills_in_half_cell_steps() {
        let mut m = Model::new(Some(10));
        let p = Paint::new(false);
        let at = |m: &Model| strip_ansi(&bar(m, p, 0, 40));
        let cells = at(&m);
        assert_eq!(cells.chars().count(), 40);
        assert_eq!(cells.matches(BAR_TRACK).count(), 40, "nothing done yet");
        m.stats.units = 5;
        let cells = at(&m);
        assert_eq!((cells.matches(BAR_FILL).count(), cells.matches(BAR_HEAD).count(), cells.matches(BAR_TRACK).count()), (20, 0, 20));
        m.stats.units = 1;
        let cells = at(&m);
        assert_eq!((cells.matches(BAR_FILL).count(), cells.matches(BAR_HEAD).count(), cells.matches(BAR_TRACK).count()), (4, 0, 36));
        let mut odd = Model::new(Some(80));
        odd.stats.units = 1;
        let cells = at(&odd);
        assert_eq!(cells.matches(BAR_HEAD).count(), 1, "a sub-cell fraction shows the half-step head: {cells}");
        assert_eq!(cells.chars().count(), 40);
        m.stats.units = 10;
        let cells = at(&m);
        assert!(cells.chars().all(|c| c.to_string() == BAR_FILL), "{cells:?}");
    }

    #[test]
    fn without_a_total_a_segment_sweeps_the_track() {
        let m = Model::new(None);
        let p = Paint::new(false);
        let a = strip_ansi(&bar(&m, p, 0, 30));
        let b = strip_ansi(&bar(&m, p, 3, 30));
        assert_ne!(a, b);
        assert_eq!(a.chars().count(), 30);
        assert_eq!(a.matches(BAR_FILL).count(), SHIMMER_WIDTH);
        assert!(a.starts_with(BAR_FILL), "starts at the left edge");
        let turn = strip_ansi(&bar(&m, p, 24, 30));
        assert!(turn.ends_with(BAR_FILL), "reaches the right edge: {turn}");
        assert_eq!(strip_ansi(&bar(&m, p, 48, 30)), a, "wraps after a full sweep");
    }

    #[test]
    fn unit_timers_align_at_the_right_edge_and_slow_ones_stand_out() {
        let now = Instant::now();
        let unit = ActiveUnit { name: "serde".into(), version: Some("1.0.200".into()), local: false, build_script: false, started: now - Duration::from_millis(1200) };
        let line = strip_ansi(&unit_line(&unit, Paint::new(true), now, 60));
        assert_eq!(visible_width(&line), 60 - RIGHT_MARGIN, "{line:?}");
        assert!(line.starts_with("    ▸ serde v1.0.200"), "{line:?}");
        assert!(line.ends_with("1.20s"), "{line:?}");
        let slow = ActiveUnit { started: now - Duration::from_secs(9), ..unit };
        assert!(unit_line(&slow, Paint::new(true), now, 60).contains("\x1b[33m"), "slow units turn yellow");
    }

    #[test]
    fn narrow_terminals_keep_a_minimum_bar() {
        let m = Model::new(Some(10));
        assert_eq!(strip_ansi(&bar(&m, Paint::new(false), 0, 3)).chars().count(), MIN_BAR_WIDTH);
    }

    #[test]
    fn summary_omits_zero_optional_counters() {
        let stats = RunStats { units: 87, cached: 61, compiled: 20, fresh: 6, saved_ms: 38_400, warnings: 2, errors: 0 };
        let lines = summary_ansi(Paint::new(true), true, Some("dev profile [unoptimized + debuginfo]"), Duration::from_millis(4210), &stats);
        assert_eq!(strip_ansi(&lines[0]), "  ✔ Finished dev profile [unoptimized + debuginfo] in 4.21s");
        assert_eq!(strip_ansi(&lines[1]), "    2 warnings  ·  87 units  ·  61 cached  ·  20 compiled  ·  6 fresh  ·  saved ~38.4s");
        let quiet = RunStats { units: 1, ..RunStats::default() };
        let lines = summary_ansi(Paint::new(false), false, None, Duration::from_millis(4210), &quiet);
        assert_eq!(lines[0], "  ✘ Failed in 4.21s");
        assert_eq!(lines[1], "    1 unit  ·  0 cached  ·  0 compiled  ·  0 fresh");
    }

    #[test]
    fn plain_summary_lines() {
        let stats = RunStats { units: 87, cached: 61, compiled: 20, fresh: 6, saved_ms: 38_400, warnings: 2, errors: 0 };
        let lines = summary_plain(true, Some("dev profile [unoptimized + debuginfo]"), Duration::from_millis(4210), &stats);
        assert_eq!(lines[0], "finished  dev profile [unoptimized + debuginfo] in 4.21s");
        assert_eq!(lines[1], "units 87  cached 61  compiled 20  fresh 6  saved 38.4s  warnings 2  errors 0");
        assert_eq!(summary_plain(true, None, Duration::from_millis(4210), &stats)[0], "finished  in 4.21s");
        assert_eq!(summary_plain(false, None, Duration::from_millis(4210), &stats)[0], "failed    in 4.21s");
    }

    #[test]
    fn titles_are_sanitised() {
        assert_eq!(title_text("my-app", 3, Some(10)), "frust: building my-app 3/10");
        assert_eq!(title_text("my-app", 3, None), "frust: building my-app 3");
        assert_eq!(title_text("a\x07b\x1bc", 1, None), "frust: building abc 1");
        assert_eq!(osc_title("x\x07y"), "\x1b]2;xy\x07");
    }

    fn styled(lines: &[&str]) -> Vec<Option<String>> {
        let mut styler = DiagStyler::default();
        lines.iter().map(|l| styler.style(Paint::new(true), l)).collect()
    }

    #[test]
    fn styling_a_diagnostic_changes_colour_and_nothing_else() {
        let block = [
            "error[E0425]: cannot find value `y` in this scope",
            " --> src/lib.rs:12:9",
            "   |",
            "12 |     let x = y;",
            "   |             ^ not found in this scope",
            "   |",
            "   = help: consider importing it",
            "",
        ];
        let out = styled(&block);
        for (line, styled) in block.iter().zip(&out) {
            if line.is_empty() {
                assert_eq!(styled, &None);
            } else {
                let styled = styled.as_ref().unwrap_or_else(|| panic!("unstyled: {line:?}"));
                assert_eq!(&strip_ansi(styled), line, "text must be preserved");
                assert!(styled.contains('\x1b'), "no colour on {line:?}");
            }
        }
        assert!(out[0].as_ref().unwrap().starts_with("\x1b[1;31merror[E0425]"));
        assert!(out[4].as_ref().unwrap().contains("\x1b[1;31m^ not found in this scope"));
    }

    #[test]
    fn warning_carets_are_yellow_and_secondary_spans_blue() {
        let out = styled(&["warning: unused variable: `x`", "  |", "3 |     let x = 1;", "  |         ^ -- here"]);
        let caret = out[3].as_ref().unwrap();
        assert!(caret.contains("\x1b[1;33m^ \x1b[0m") || caret.contains("\x1b[1;33m^"), "{caret:?}");
        assert!(caret.contains("\x1b[1;34m-- here"), "{caret:?}");
        assert_eq!(strip_ansi(caret), "  |         ^ -- here");
    }

    #[test]
    fn closing_notes_recede_and_end_the_block() {
        let mut styler = DiagStyler::default();
        let p = Paint::new(true);
        let closing = styler.style(p, "error: could not compile `app` (lib) due to 1 previous error").unwrap();
        assert!(closing.contains("\x1b[2mcould not compile"), "{closing:?}");
        // No block is open, so a stray gutter-looking line is left alone.
        assert_eq!(styler.style(p, "  | not a diagnostic"), None);
        let rollup = styler.style(p, "warning: `app` (lib) generated 2 warnings").unwrap();
        assert!(rollup.contains("\x1b[2m`app`"), "{rollup:?}");
    }

    #[test]
    fn explain_hint_is_dimmed() {
        let mut styler = DiagStyler::default();
        let hint = "For more information about this error, try `rustc --explain E0425`.";
        assert_eq!(styler.style(Paint::new(true), hint), Some(format!("\x1b[2m{hint}\x1b[0m")));
    }

    #[test]
    fn unrecognised_lines_are_left_to_cargo() {
        let mut styler = DiagStyler::default();
        assert_eq!(styler.style(Paint::new(true), "running 3 tests"), None);
        assert_eq!(styler.style(Paint::new(true), "warning without a colon"), None);
    }
}
