//! Progress of long maintenance passes: library code reports real counts and the CLI decides how to show them.

use std::io::Write;
use std::time::{Duration, Instant};

use crate::fsx::human_bytes;
use crate::ui::term::{fmt_duration, terminal_width, truncate_visible};

/// Receives the phases and items of a gc or compaction pass; every method does nothing by default.
pub trait Progress {
    /// Starts a phase; `total` is its item count when known in advance and `bytes` names what item bytes mean.
    fn phase(&mut self, _name: &str, _total: Option<u64>, _bytes: &'static str) {}
    /// One item of the current phase is done; `scope` names its workspace, profile or class.
    fn item(&mut self, _scope: &str, _bytes: u64) {}
}

/// What automatic maintenance passes, which never print.
pub struct Silent;

impl Progress for Silent {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Phase { name: String, total: Option<u64> },
    Item { scope: String, bytes: u64 },
}

/// Keeps every event, so tests can check order, counts and totals.
#[derive(Debug, Default)]
pub struct Recorder {
    pub events: Vec<Event>,
}

impl Progress for Recorder {
    fn phase(&mut self, name: &str, total: Option<u64>, _bytes: &'static str) {
        self.events.push(Event::Phase { name: name.to_string(), total });
    }

    fn item(&mut self, scope: &str, bytes: u64) {
        self.events.push(Event::Item { scope: scope.to_string(), bytes });
    }
}

impl Recorder {
    pub fn phases(&self) -> Vec<&str> {
        self.events.iter().filter_map(|e| if let Event::Phase { name, .. } = e { Some(name.as_str()) } else { None }).collect()
    }

    /// The items of every phase with this name, with the totals those phases announced.
    pub fn items(&self, phase: &str) -> (Vec<Option<u64>>, Vec<(String, u64)>) {
        let (mut totals, mut items, mut inside) = (Vec::new(), Vec::new(), false);
        for event in &self.events {
            match event {
                Event::Phase { name, total } => {
                    inside = name == phase;
                    if inside {
                        totals.push(*total);
                    }
                }
                Event::Item { scope, bytes } if inside => items.push((scope.clone(), *bytes)),
                Event::Item { .. } => {}
            }
        }
        (totals, items)
    }

    pub fn bytes(&self, phase: &str) -> u64 {
        self.items(phase).1.iter().map(|(_, bytes)| bytes).sum()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Off,
    /// One status line rewritten in place, for a terminal.
    Live,
    /// A line per phase and at most one more every few seconds, for logs.
    Lines,
}

impl Mode {
    /// `--quiet` wins over `--progress`, which forces output even with `FRUST_RENDER=off`.
    pub fn choose(quiet: bool, forced: bool, render_off: bool, terminal: bool) -> Mode {
        match (quiet, forced || !render_off, terminal) {
            (true, _, _) | (false, false, _) => Mode::Off,
            (false, true, true) => Mode::Live,
            (false, true, false) => Mode::Lines,
        }
    }
}

const LIVE_EVERY: Duration = Duration::from_millis(100);
const LINES_EVERY: Duration = Duration::from_secs(5);

/// Renders progress on stderr: no fake percentages, an ETA only when the phase announced its total.
pub struct Status<W: Write> {
    mode: Mode,
    out: Option<W>,
    task: &'static str,
    started: Instant,
    phase: String,
    phase_started: Instant,
    total: Option<u64>,
    done: u64,
    bytes: u64,
    noun: &'static str,
    scope: String,
    last: Option<Instant>,
    drawn: bool,
}

impl<W: Write> Status<W> {
    pub fn new(mode: Mode, out: W, task: &'static str) -> Status<W> {
        let now = Instant::now();
        Status { mode, out: Some(out), task, started: now, phase: String::new(), phase_started: now, total: None, done: 0, bytes: 0, noun: "", scope: String::new(), last: None, drawn: false }
    }

    fn line(&self, now: Instant) -> String {
        let mut text = format!("{} {}", self.task, self.phase);
        text.push_str(&match self.total {
            Some(total) => format!("  {}/{total}", self.done),
            None => format!("  {}", self.done),
        });
        if self.bytes > 0 {
            text.push_str(&format!("  {} {}", human_bytes(self.bytes), self.noun));
        }
        if !self.scope.is_empty() {
            text.push_str(&format!("  {}", self.scope));
        }
        text.push_str(&format!("  {}", fmt_duration(now - self.started)));
        if let Some(total) = self.total.filter(|total| self.done > 0 && self.done < *total) {
            let per = (now - self.phase_started).as_secs_f64() / self.done as f64;
            text.push_str(&format!("  ETA {}", fmt_duration(Duration::from_secs_f64(per * (total - self.done) as f64))));
        }
        text
    }

    fn draw(&mut self, now: Instant, force: bool) {
        let every = if self.mode == Mode::Live { LIVE_EVERY } else { LINES_EVERY };
        if !force && self.last.is_some_and(|last| now - last < every) {
            return;
        }
        self.last = Some(now);
        let line = self.line(now);
        let live = self.mode == Mode::Live;
        let Some(out) = self.out.as_mut().filter(|_| self.mode != Mode::Off) else { return };
        let _ = if live {
            let width = usize::from(terminal_width()).saturating_sub(1).max(20);
            write!(out, "\r\x1b[2K{}", truncate_visible(&line, width))
        } else {
            writeln!(out, "frust: {line}")
        };
        let _ = out.flush();
        self.drawn |= live;
    }

    /// Clears the live line so the report that follows starts on a clean row.
    pub fn finish(&mut self) {
        if self.drawn
            && let Some(out) = self.out.as_mut()
        {
            let _ = write!(out, "\r\x1b[2K");
            let _ = out.flush();
            self.drawn = false;
        }
    }

    pub fn into_inner(mut self) -> W {
        self.finish();
        self.out.take().expect("the writer is taken only here")
    }
}

impl<W: Write> Progress for Status<W> {
    fn phase(&mut self, name: &str, total: Option<u64>, bytes: &'static str) {
        let now = Instant::now();
        (self.phase, self.total, self.done, self.bytes, self.noun, self.phase_started) = (name.to_string(), total, 0, 0, bytes, now);
        self.scope.clear();
        self.draw(now, true);
    }

    fn item(&mut self, scope: &str, bytes: u64) {
        self.done += 1;
        self.bytes = self.bytes.saturating_add(bytes);
        if self.scope != scope {
            self.scope = scope.to_string();
        }
        self.draw(Instant::now(), false);
    }
}

impl<W: Write> Drop for Status<W> {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_wins_and_progress_forces_output_past_render_off() {
        assert_eq!(Mode::choose(true, true, false, true), Mode::Off);
        assert_eq!(Mode::choose(false, false, true, true), Mode::Off);
        assert_eq!(Mode::choose(false, true, true, false), Mode::Lines);
        assert_eq!(Mode::choose(false, false, false, true), Mode::Live);
        assert_eq!(Mode::choose(false, false, false, false), Mode::Lines);
    }

    #[test]
    fn lines_mode_prints_each_phase_and_throttles_items() {
        let mut status = Status::new(Mode::Lines, Vec::new(), "gc");
        status.phase("remove", Some(3), "freed");
        for _ in 0..3 {
            status.item("app-1 superseded build artifacts", 1024);
        }
        status.phase("tidy", None, "freed");
        let text = String::from_utf8(status.into_inner()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert!(lines[0].starts_with("frust: gc remove  0/3  ") && lines[1].starts_with("frust: gc tidy  0  "), "{text}");
    }

    #[test]
    fn live_mode_rewrites_one_line_with_counts_bytes_and_an_eta() {
        let mut status = Status::new(Mode::Live, Vec::new(), "compact");
        status.phase("compress and dedupe", Some(4), "examined");
        status.last = None;
        status.item("app-1", 2048);
        let text = String::from_utf8(status.into_inner()).unwrap();
        assert!(text.starts_with("\r\x1b[2Kcompact compress and dedupe  0/4"), "{text:?}");
        assert!(text.contains("1/4  2.0 KiB examined  app-1") && text.contains("ETA "), "{text:?}");
        assert!(text.ends_with("\r\x1b[2K"), "the line is cleared at the end: {text:?}");
        assert!(!text.contains('\n'));
    }
}
