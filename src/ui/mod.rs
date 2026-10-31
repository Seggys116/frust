//! Terminal UI for builds.
//! Output goes to an injected writer and time is passed in, so the display can be driven headlessly (see [`vt`], [`demo`]).

pub mod demo;
mod model;
mod render;
pub mod term;
pub mod vt;

use std::io::Write;
use std::time::{Duration, Instant};

use crate::config::Osc;
use crate::events::{self, Event};
use crate::lock::WaitInfo;

use model::{LockView, Model};
use term::{Paint, strip_ansi, truncate_visible};

/// Title updates are limited to about four per second.
const TITLE_MIN_GAP: Duration = Duration::from_millis(250);
/// Live-region redraws are limited to about forty per second so a slow terminal cannot stall cargo's pipe.
const PAINT_MIN_GAP: Duration = Duration::from_millis(25);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    Ansi,
    Plain,
}

#[derive(Debug, Clone)]
pub struct UiOptions {
    pub style: Style,
    pub width: u16,
    pub osc: Osc,
    pub workspace: String,
    pub command: String,
    pub expected_units: Option<u32>,
    pub toolchain: String,
}

#[derive(Debug, Clone, Copy)]
pub struct Outcome {
    pub success: bool,
    pub exit_code: i32,
}

#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub struct RunStats {
    pub units: u32,
    pub cached: u32,
    pub compiled: u32,
    pub fresh: u32,
    pub saved_ms: u64,
    pub warnings: u32,
    pub errors: u32,
}

/// Last OSC 9;4 state sent, so it is only re-sent when it changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Progress {
    Percent(u32),
    Indeterminate,
}

pub struct Ui {
    opts: UiOptions,
    paint: Paint,
    out: Box<dyn Write + Send>,
    model: Model,
    start: Instant,
    /// Rows the live region currently occupies below the cursor's anchor.
    live_lines: usize,
    last_frame: Vec<String>,
    last_paint: Option<Instant>,
    cursor_hidden: bool,
    progress_sent: Option<Progress>,
    title_pushed: bool,
    last_title: Option<String>,
    last_title_at: Option<Instant>,
    /// Plain style prints the lock wait once per owner; this is the owner last announced.
    plain_lock_owner: Option<Option<u32>>,
    finished: bool,
    styler: render::DiagStyler,
}

impl Ui {
    pub fn new(opts: UiOptions, out: Box<dyn Write + Send>, now: Instant) -> Ui {
        let paint = Paint::new(opts.style == Style::Ansi);
        let model = Model::new(opts.expected_units);
        let mut ui = Ui {
            opts,
            paint,
            out,
            model,
            start: now,
            live_lines: 0,
            last_frame: Vec::new(),
            last_paint: None,
            cursor_hidden: false,
            progress_sent: None,
            title_pushed: false,
            last_title: None,
            last_title_at: None,
            plain_lock_owner: None,
            finished: false,
            styler: render::DiagStyler::default(),
        };
        match ui.opts.style {
            Style::Plain => {
                let header = render::plain_header(&ui.opts);
                ui.emit_lines(&[header]);
            }
            Style::Ansi => {
                let header = truncate_visible(&render::header(ui.paint, &ui.opts), ui.width());
                ui.flush(vec![header], now);
            }
        }
        ui
    }

    pub fn on_cargo_stderr(&mut self, line: &str, now: Instant) {
        if self.finished {
            return;
        }
        let line = line.trim_end_matches(['\r', '\n']);
        let permanent = self.model.on_stderr(line, now);
        let permanent = match (self.opts.style, permanent) {
            (Style::Ansi, Some(text)) if self.paint.enabled() => {
                Some(self.styler.style(self.paint, &strip_ansi(&text)).unwrap_or(text))
            }
            (_, permanent) => permanent,
        };
        match self.opts.style {
            Style::Plain => {
                if let Some(text) = permanent {
                    self.emit_lines(&[strip_ansi(&text)]);
                }
            }
            // Diagnostics stay unclipped: a wrapped error is readable, a clipped one is not.
            Style::Ansi => self.flush(permanent.into_iter().collect(), now),
        }
    }

    pub fn on_cargo_json(&mut self, msg: &serde_json::Value, now: Instant) {
        if self.finished {
            return;
        }
        let completion = self.model.on_json(msg, now);
        match self.opts.style {
            Style::Plain => {
                if let Some(c) = completion {
                    self.emit_lines(&[render::completion_plain(&c)]);
                }
            }
            Style::Ansi => {
                let lines: Vec<String> = completion
                    .filter(render::is_notable)
                    .map(|c| render::completion_ansi(&c, self.paint, self.width(), self.opts.osc.links))
                    .map(|l| truncate_visible(&l, self.width()))
                    .into_iter()
                    .collect();
                self.flush(lines, now);
            }
        }
    }

    pub fn on_cache_event(&mut self, ev: &Event, now: Instant) {
        if self.finished {
            return;
        }
        self.model.on_cache(ev);
        if self.opts.style == Style::Ansi {
            self.flush(Vec::new(), now);
        }
    }

    pub fn lock_waiting(&mut self, info: &WaitInfo, now: Instant) {
        if self.finished {
            return;
        }
        let view = match &info.owner {
            Some(owner) => {
                // Without a recorded owner start, our own wait is the best available figure.
                let held = if owner.since_unix > 0 {
                    (events::now_ms() / 1000).saturating_sub(owner.since_unix)
                } else {
                    info.waited.as_secs()
                };
                LockView {
                    pid: Some(owner.pid),
                    command: owner.command.clone(),
                    held: Duration::from_secs(held),
                    ahead: info.ahead,
                }
            }
            None => LockView { pid: None, command: String::new(), held: info.waited, ahead: info.ahead },
        };
        match self.opts.style {
            Style::Plain => {
                let owner = view.pid;
                if self.plain_lock_owner != Some(owner) {
                    self.plain_lock_owner = Some(owner);
                    self.emit_lines(&[render::lock_line_plain(&view)]);
                }
                self.model.lock = Some(view);
            }
            Style::Ansi => {
                self.model.lock = Some(view);
                self.flush(Vec::new(), now);
            }
        }
    }

    pub fn lock_acquired(&mut self, now: Instant) {
        if self.finished {
            return;
        }
        self.model.lock = None;
        self.plain_lock_owner = None;
        if self.opts.style == Style::Ansi {
            self.flush(Vec::new(), now);
        }
    }

    pub fn note(&mut self, verb: &str, text: &str, now: Instant) {
        if self.finished {
            return;
        }
        let text: String = text.chars().filter(|c| !c.is_control()).collect();
        match self.opts.style {
            Style::Plain => self.emit_lines(&[format!("{verb:<10}{text}")]),
            Style::Ansi => {
                let line = truncate_visible(&format!("  {}", self.paint.dim(&format!("{verb} {text}"))), self.width());
                self.flush(vec![line], now);
            }
        }
    }

    pub fn tick(&mut self, now: Instant) {
        if self.finished || self.opts.style == Style::Plain {
            return;
        }
        self.flush(Vec::new(), now);
    }

    pub fn finish(&mut self, outcome: Outcome, now: Instant) -> RunStats {
        let stats = self.model.stats.clone();
        if self.finished {
            return stats;
        }
        self.finished = true;
        let elapsed = now.saturating_duration_since(self.start);
        // `build-finished` disagreeing with the exit status signals a failure the exit code did not show.
        let success = outcome.success && self.model.build_finished != Some(false);
        let profile = self.model.profile.as_deref();

        match self.opts.style {
            Style::Plain => {
                let lines = render::summary_plain(success, profile, elapsed, &stats);
                self.emit_lines(&lines);
            }
            Style::Ansi => {
                let width = self.width();
                let osc = self.opts.osc;
                let mut buf = String::new();
                if osc.progress && !success {
                    buf.push_str(&render::osc_progress_error(self.model.percent().unwrap_or(100)));
                }
                if self.live_lines > 0 {
                    buf.push_str(&format!("\x1b[{}A", self.live_lines));
                }
                buf.push_str("\r\x1b[J");
                for line in render::summary_ansi(self.paint, success, profile, elapsed, &stats) {
                    buf.push_str(&truncate_visible(&line, width));
                    buf.push('\n');
                }
                if self.cursor_hidden {
                    buf.push_str("\x1b[?25h");
                }
                if osc.progress {
                    buf.push_str(&render::osc_progress_clear());
                }
                if self.title_pushed {
                    buf.push_str(render::TITLE_POP);
                }
                self.live_lines = 0;
                self.last_frame.clear();
                self.cursor_hidden = false;
                self.emit(&buf);
            }
        }
        stats
    }

    fn width(&self) -> usize {
        usize::from(self.opts.width).max(8)
    }

    /// A failed write (closed pipe, full disk) must not abort the build being displayed.
    fn emit(&mut self, text: &str) {
        let _ = self.out.write_all(text.as_bytes());
        let _ = self.out.flush();
    }

    fn emit_lines(&mut self, lines: &[String]) {
        let mut text = String::new();
        for line in lines {
            text.push_str(line);
            text.push('\n');
        }
        self.emit(&text);
    }

    /// Skips the redraw when nothing changed so idle ticks cost no output.
    fn flush(&mut self, permanent: Vec<String>, now: Instant) {
        // Live-only redraws closer than the gap wait for the next tick; permanent lines always print.
        let deferred = permanent.is_empty() && self.last_paint.is_some_and(|at| now.saturating_duration_since(at) < PAINT_MIN_GAP);
        let mut buf = String::new();
        if !deferred {
            self.last_paint = Some(now);
            let elapsed = now.saturating_duration_since(self.start);
            let width = self.width();
            let frame: Vec<String> = render::live_region(&self.model, self.paint, elapsed, now, width)
                .iter()
                .map(|line| truncate_visible(line, width))
                .collect();
            if !permanent.is_empty() || frame != self.last_frame || !self.cursor_hidden {
                if !self.cursor_hidden {
                    buf.push_str("\x1b[?25l");
                    self.cursor_hidden = true;
                }
                if self.live_lines > 0 {
                    buf.push_str(&format!("\x1b[{}A", self.live_lines));
                }
                buf.push_str("\r\x1b[J");
                for line in permanent.iter().chain(frame.iter()) {
                    buf.push_str(line);
                    buf.push('\n');
                }
                self.live_lines = frame.len();
                self.last_frame = frame;
            }
        }
        buf.push_str(&self.osc_updates(now));
        if !buf.is_empty() {
            self.emit(&buf);
        }
    }

    /// Title is sent only when its text changed and the rate limit allows.
    fn osc_updates(&mut self, now: Instant) -> String {
        let osc = self.opts.osc;
        let mut out = String::new();
        if osc.title {
            if !self.title_pushed {
                out.push_str(render::TITLE_PUSH);
                self.title_pushed = true;
            }
            let text = render::title_text(&self.opts.workspace, self.model.done(), self.model.total());
            let due = self.last_title_at.is_none_or(|at| now.saturating_duration_since(at) >= TITLE_MIN_GAP);
            if due && self.last_title.as_deref() != Some(text.as_str()) {
                out.push_str(&render::osc_title(&text));
                self.last_title = Some(text);
                self.last_title_at = Some(now);
            }
        }
        if osc.progress {
            let state = match self.model.percent() {
                Some(pct) => Progress::Percent(pct),
                None => Progress::Indeterminate,
            };
            if self.progress_sent != Some(state) {
                out.push_str(&match state {
                    Progress::Percent(pct) => render::osc_progress(pct),
                    Progress::Indeterminate => render::osc_progress_indeterminate(),
                });
                self.progress_sent = Some(state);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::demo::{
        SharedBuf, artifact_message, cache_event, cargo_status, demo_options, scripted_session, wait_info,
    };
    use super::term::visible_width;
    use super::vt::Vt;
    use super::*;
    use crate::events::CacheResult;

    const ALL_OSC: Osc = Osc { progress: true, title: true, links: true };

    fn drive(style: Style, width: u16, osc: Osc) -> (String, RunStats) {
        let buf = SharedBuf::new();
        let start = Instant::now();
        let mut ui = Ui::new(demo_options(style, width, osc), Box::new(buf.clone()), start);
        let end = scripted_session(&mut ui, start);
        let stats = ui.finish(Outcome { success: true, exit_code: 0 }, end);
        (buf.contents(), stats)
    }

    fn replay(raw: &str, cols: u16) -> Vt {
        let mut vt = Vt::new(cols, 500);
        vt.feed(raw);
        vt
    }

    fn ms(base: Instant, n: u64) -> Instant {
        base + Duration::from_millis(n)
    }

    fn small_ui(style: Style, osc: Osc, expected: Option<u32>) -> (Ui, SharedBuf, Instant) {
        let buf = SharedBuf::new();
        let start = Instant::now();
        let mut opts = demo_options(style, 80, osc);
        opts.expected_units = expected;
        (Ui::new(opts, Box::new(buf.clone()), start), buf, start)
    }

    #[test]
    fn plain_session_matches_snapshot() {
        let (raw, stats) = drive(Style::Plain, 100, ALL_OSC);
        let expected = [
            "frust build my-app (rustc 1.99.0)",
            "waiting   build lock held by pid 4242 (cargo build, 12s), 1 ahead",
            "cached    unicode-ident v1.0.12  saved 0.60s",
            "compiled  proc-macro2 v1.0.86  0.50s",
            "compiled  quote v1.0.36  0.20s",
            "compiled  serde v1.0.200 (build script)  0.40s",
            "cached    serde_derive v1.0.200  saved 1.30s",
            "compiled  serde v1.0.200  1.40s",
            "compiled  log v0.4.21  0.30s",
            "compiled  syn v2.0.1  3.40s",
            "warning: unused variable: `x`",
            "  --> crates/core/src/lib.rs:12:9",
            "   |",
            "12 |     let x = 1;",
            "   |         ^ help: if this is intentional, prefix it with an underscore: `_x`",
            "   |",
            "   = note: `#[warn(unused_variables)]` on by default",
            "",
            "warning: `my-app-core` (lib) generated 1 warning",
            "compiled  my-app-core v0.1.0  1.00s",
            "compiled  my-app v0.1.0  1.20s",
            "finished  dev profile [unoptimized + debuginfo] in 8.70s",
            "units 13  cached 3  compiled 7  fresh 3  saved 3.70s  warnings 1  errors 0",
        ];
        assert_eq!(raw, expected.join("\n") + "\n");
        assert!(!raw.contains('\x1b'));
        assert_eq!(
            stats,
            RunStats { units: 13, cached: 3, compiled: 7, fresh: 3, saved_ms: 3700, warnings: 1, errors: 0 }
        );
    }

    #[test]
    fn ansi_final_screen_has_summary_and_no_live_region() {
        let (raw, stats) = drive(Style::Ansi, 100, ALL_OSC);
        let vt = replay(&raw, 100);
        let lines = vt.screen();
        let n = lines.len();
        assert_eq!(lines[0], "frust build my-app  rustc 1.99.0");
        assert_eq!(lines[n - 2], "  ✔ Finished dev profile [unoptimized + debuginfo] in 8.70s");
        assert_eq!(lines[n - 1], "    1 warning  ·  13 units  ·  3 cached  ·  7 compiled  ·  3 fresh  ·  saved ~3.70s");
        assert!(!lines.iter().any(|l| l.contains("/~14")), "live progress line left behind");
        assert!(!lines.iter().any(|l| l.trim_start().starts_with("> ")), "live unit line left behind");
        assert!(vt.cursor_visible());
        assert_eq!(stats.units, 13);

        // Notable completions only: slow, path and high-saving units get lines.
        let text = lines.join("\n");
        for wanted in ["Compiled syn v2.0.1", "Compiled serde v1.0.200", "Compiled my-app-core", "Compiled my-app v0.1.0", "Cached serde_derive v1.0.200"] {
            assert!(text.contains(wanted), "missing {wanted:?} in\n{text}");
        }
        for unwanted in ["proc-macro2", "itoa", "libc", "quote", "unicode-ident", "Compiled log"] {
            assert!(!text.contains(unwanted), "unexpected {unwanted:?} in\n{text}");
        }
        let syn = lines.iter().find(|l| l.contains("Compiled syn")).unwrap();
        assert_eq!(syn.chars().count(), 98, "time column should end at width - 2");
        assert!(text.contains("warning: unused variable: `x`"));
    }

    #[test]
    fn live_region_shows_progress_units_and_lock() {
        let (mut ui, buf, start) = small_ui(Style::Ansi, Osc::default(), Some(14));
        ui.lock_waiting(&wait_info(12), ms(start, 100));
        let vt = replay(&buf.contents(), 100);
        assert_eq!(vt.screen()[1], "  waiting for build lock  held by pid 4242 (cargo build, 12s)  1 ahead");
        assert!(!vt.cursor_visible());

        ui.lock_acquired(ms(start, 500));
        ui.on_cargo_stderr(&cargo_status("Compiling", "syn v2.0.1"), ms(start, 600));
        ui.on_cache_event(&cache_event(CacheResult::Hit, "other", 100), ms(start, 700));
        ui.tick(ms(start, 2700));
        let vt = replay(&buf.contents(), 100);
        let screen = vt.screen();
        assert_eq!(screen.len(), 3);
        assert!(screen[1].contains("0/~14"), "{}", screen[1]);
        assert!(screen[1].contains("2.70s"), "{}", screen[1]);
        assert!(screen[1].contains("cache 1 hit / 0 miss"), "{}", screen[1]);
        assert_eq!(screen[2], "    > syn v2.0.1  2.10s");
    }

    #[test]
    fn live_region_is_capped_at_six_lines() {
        let (mut ui, buf, start) = small_ui(Style::Ansi, Osc::default(), None);
        for i in 0..9 {
            ui.on_cargo_stderr(&cargo_status("Compiling", &format!("crate{i} v1.0.0")), ms(start, 1000));
        }
        ui.tick(ms(start, 1100));
        let screen = replay(&buf.contents(), 100).screen();
        assert_eq!(screen.len(), 7);
        assert_eq!(screen[2], "    > crate0 v1.0.0  0.10s");
        assert_eq!(screen[5], "    > crate3 v1.0.0  0.10s");
        assert_eq!(screen[6], "    + 5 more");
    }

    #[test]
    fn nothing_is_written_when_the_frame_is_unchanged() {
        let (mut ui, buf, start) = small_ui(Style::Ansi, Osc::default(), Some(10));
        ui.tick(ms(start, 1000));
        let before = buf.contents().len();
        ui.tick(ms(start, 1000));
        ui.tick(ms(start, 1000));
        assert_eq!(buf.contents().len(), before);
        ui.tick(ms(start, 2000));
        assert!(buf.contents().len() > before);
    }

    #[test]
    fn live_redraws_are_coalesced_but_permanent_lines_print_at_once() {
        let (mut ui, buf, start) = small_ui(Style::Ansi, Osc::default(), None);
        ui.on_cargo_stderr(&cargo_status("Compiling", "a v1.0.0"), ms(start, 100));
        let painted = buf.contents().len();
        ui.on_cargo_stderr(&cargo_status("Compiling", "b v1.0.0"), ms(start, 110));
        ui.on_cache_event(&cache_event(CacheResult::Hit, "a", 100), ms(start, 115));
        assert_eq!(buf.contents().len(), painted, "redraw inside the gap should wait");
        ui.on_cargo_stderr("error[E0425]: cannot find value `y` in this scope", ms(start, 120));
        assert!(buf.contents().contains("error[E0425]"), "diagnostics print immediately");
        ui.on_cargo_stderr(&cargo_status("Compiling", "c v1.0.0"), ms(start, 125));
        ui.tick(ms(start, 160));
        let screen = replay(&buf.contents(), 100).screen();
        let text = screen.join("\n");
        for wanted in ["a v1.0.0", "b v1.0.0", "c v1.0.0"] {
            assert!(text.contains(wanted), "missing {wanted:?} in\n{text}");
        }
    }

    #[test]
    fn output_never_exceeds_the_terminal_width() {
        let buf = SharedBuf::new();
        let start = Instant::now();
        let mut opts = demo_options(Style::Ansi, 40, ALL_OSC);
        opts.workspace = "a-very-long-workspace-name-that-overflows-forty-columns".to_string();
        let mut ui = Ui::new(opts, Box::new(buf.clone()), start);
        ui.on_cargo_stderr(&cargo_status("Compiling", "a-rather-long-registry-crate-name v1.2.3"), ms(start, 0));
        ui.on_cargo_stderr(
            &cargo_status("Compiling", "my-extremely-long-workspace-member-name v0.1.0 (/some/very/long/path/to/the/crate)"),
            ms(start, 0),
        );
        ui.on_cache_event(&cache_event(CacheResult::Hit, "a_rather_long_registry_crate_name", 5000), ms(start, 10));
        let id = "registry+https://github.com/rust-lang/crates.io-index#a-rather-long-registry-crate-name@1.2.3";
        ui.on_cargo_json(&artifact_message(id, "a_rather_long_registry_crate_name", "lib", false), ms(start, 2000));
        ui.tick(ms(start, 2500));
        let id = "path+file:///x#my-extremely-long-workspace-member-name@0.1.0";
        ui.on_cargo_json(&artifact_message(id, "my_extremely_long_workspace_member_name", "lib", false), ms(start, 3000));
        ui.lock_waiting(&wait_info(12), ms(start, 3100));
        ui.tick(ms(start, 3200));
        ui.finish(Outcome { success: true, exit_code: 0 }, ms(start, 3300));

        let raw = buf.contents();
        for segment in raw.split('\n') {
            assert!(visible_width(segment) <= 40, "{} columns: {segment:?}", visible_width(segment));
        }
        assert!(raw.contains("Cached"), "{raw}");
        assert!(raw.contains("\x1b]8;;https://docs.rs/a-rather-long-registry-crate-name/1.2.3"));
    }

    #[test]
    fn fresh_units_are_counted_but_never_printed() {
        for style in [Style::Plain, Style::Ansi] {
            let (mut ui, buf, start) = small_ui(style, Osc::default(), None);
            let id = "registry+https://github.com/rust-lang/crates.io-index#freshy@1.0.0";
            ui.on_cargo_json(&artifact_message(id, "freshy", "lib", true), ms(start, 10));
            ui.on_cargo_json(&artifact_message(id, "freshy", "lib", true), ms(start, 20));
            ui.tick(ms(start, 5000));
            let stats = ui.finish(Outcome { success: true, exit_code: 0 }, ms(start, 5100));
            assert_eq!((stats.fresh, stats.units, stats.compiled, stats.cached), (2, 2, 0, 0));
            assert!(!buf.contents().contains("freshy"), "{style:?}");
        }
    }

    #[test]
    fn hit_after_artifact_moves_unit_from_compiled_to_cached() {
        let (mut ui, _buf, start) = small_ui(Style::Plain, Osc::default(), None);
        let id = "registry+https://github.com/rust-lang/crates.io-index#serde-json@1.0.0";
        ui.on_cargo_stderr(&cargo_status("Compiling", "serde-json v1.0.0"), ms(start, 0));
        ui.on_cargo_json(&artifact_message(id, "serde_json", "lib", false), ms(start, 300));
        ui.on_cache_event(&cache_event(CacheResult::Hit, "serde_json", 1200), ms(start, 350));
        let stats = ui.finish(Outcome { success: true, exit_code: 0 }, ms(start, 400));
        assert_eq!((stats.cached, stats.compiled, stats.units, stats.saved_ms), (1, 0, 1, 1200));
    }

    #[test]
    fn hit_before_artifact_counts_as_cached_with_saved_time() {
        let (mut ui, buf, start) = small_ui(Style::Plain, Osc::default(), None);
        let id = "registry+https://github.com/rust-lang/crates.io-index#serde-json@1.0.0";
        ui.on_cargo_stderr(&cargo_status("Compiling", "serde-json v1.0.0"), ms(start, 0));
        ui.on_cache_event(&cache_event(CacheResult::Hit, "serde_json", 1200), ms(start, 100));
        ui.on_cargo_json(&artifact_message(id, "serde_json", "lib", false), ms(start, 300));
        let stats = ui.finish(Outcome { success: true, exit_code: 0 }, ms(start, 400));
        assert_eq!((stats.cached, stats.compiled), (1, 0));
        assert!(buf.contents().contains("cached    serde-json v1.0.0  saved 1.20s\n"));
    }

    #[test]
    fn osc_progress_and_title_follow_the_build() {
        let (raw, _) = drive(Style::Ansi, 100, ALL_OSC);
        let vt = replay(&raw, 100);
        let progress: Vec<&str> = vt.osc_log().iter().map(String::as_str).filter(|p| p.starts_with("9;4;")).collect();
        let pcts = [0, 7, 14, 21, 28, 35, 42, 50, 57, 64, 71, 78, 85, 92];
        let mut expected: Vec<String> = pcts.iter().map(|p| format!("9;4;1;{p}")).collect();
        expected.push("9;4;0".to_string());
        assert_eq!(progress, expected);

        let titles: Vec<&String> = vt.osc_log().iter().filter(|p| p.starts_with("2;")).collect();
        assert_eq!(titles[0], "2;frust: building my-app 0/14");
        assert!(titles.iter().all(|t| t.starts_with("2;frust: building my-app ")));
        assert!(titles.len() >= 3);
        assert!(titles.windows(2).all(|w| w[0] != w[1]), "repeated title");
        assert_eq!(raw.matches("\x1b[22;0t").count(), 1);
        assert_eq!(raw.matches("\x1b[23;0t").count(), 1);
        assert!(raw.find("\x1b[22;0t").unwrap() < raw.find("\x1b[23;0t").unwrap());
        // Links wrap registry crates in permanent lines.
        assert!(vt.osc_log().iter().any(|p| p == "8;;https://docs.rs/syn/2.0.1"));
        assert!(!vt.osc_log().iter().any(|p| p.contains("my-app-core") && p.starts_with("8;;http")));
    }

    #[test]
    fn title_updates_are_rate_limited() {
        let osc = Osc { progress: false, title: true, links: false };
        let (mut ui, buf, start) = small_ui(Style::Ansi, osc, Some(10));
        let id = "registry+https://github.com/rust-lang/crates.io-index#a@1.0.0";
        // Text changes at 100 ms and 200 ms, both inside the gap after the first title.
        ui.on_cargo_json(&artifact_message(id, "a", "lib", true), ms(start, 100));
        ui.on_cargo_json(&artifact_message(id, "a", "lib", true), ms(start, 200));
        assert_eq!(replay(&buf.contents(), 80).osc_log(), ["2;frust: building my-app 0/10"]);
        ui.tick(ms(start, 400));
        assert_eq!(replay(&buf.contents(), 80).osc_log().last().unwrap(), "2;frust: building my-app 2/10");
    }

    #[test]
    fn osc_is_silent_when_disabled_and_in_plain() {
        let (raw, _) = drive(Style::Ansi, 100, Osc::default());
        assert!(replay(&raw, 100).osc_log().is_empty());
        assert!(!raw.contains("\x1b[22;0t"));
        let (raw, _) = drive(Style::Plain, 100, ALL_OSC);
        assert!(!raw.contains('\x1b'));
    }

    #[test]
    fn indeterminate_progress_without_an_estimate() {
        let osc = Osc { progress: true, title: false, links: false };
        let (mut ui, buf, start) = small_ui(Style::Ansi, osc, None);
        let id = "registry+https://github.com/rust-lang/crates.io-index#a@1.0.0";
        ui.on_cargo_json(&artifact_message(id, "a", "lib", true), ms(start, 100));
        ui.finish(Outcome { success: true, exit_code: 0 }, ms(start, 200));
        let log: Vec<String> = replay(&buf.contents(), 80).osc_log().to_vec();
        assert_eq!(log, ["9;4;3", "9;4;0"]);
    }

    #[test]
    fn failed_run_prints_failed_and_flags_progress() {
        let osc = Osc { progress: true, title: false, links: false };
        let (mut ui, buf, start) = small_ui(Style::Ansi, osc, Some(10));
        ui.on_cargo_stderr(&cargo_status("Compiling", "app v0.1.0 (/work/app)"), ms(start, 0));
        ui.on_cargo_stderr("error[E0425]: cannot find value `y` in this scope", ms(start, 500));
        ui.on_cargo_stderr("error: could not compile `app` (bin \"app\") due to 1 previous error", ms(start, 600));
        let stats = ui.finish(Outcome { success: false, exit_code: 101 }, ms(start, 4210));
        assert_eq!(stats.errors, 1);
        let vt = replay(&buf.contents(), 100);
        let screen = vt.screen();
        let n = screen.len();
        assert_eq!(screen[n - 2], "  ✘ Failed in 4.21s");
        assert!(screen[n - 1].trim_start().starts_with("1 error  ·"), "{}", screen[n - 1]);
        assert!(screen.iter().any(|l| l.starts_with("error[E0425]")));
        let progress: Vec<&String> = vt.osc_log().iter().filter(|p| p.starts_with("9;4;")).collect();
        assert_eq!(progress[progress.len() - 2], "9;4;2;0");
        assert_eq!(progress[progress.len() - 1], "9;4;0");
        assert!(vt.cursor_visible());

        let (mut plain, buf, start) = small_ui(Style::Plain, Osc::default(), None);
        plain.finish(Outcome { success: false, exit_code: 101 }, ms(start, 4210));
        assert!(buf.contents().contains("failed    in 4.21s\n"));
    }

    #[test]
    fn build_finished_failure_overrides_a_zero_exit() {
        let (mut ui, buf, start) = small_ui(Style::Plain, Osc::default(), None);
        ui.on_cargo_json(&serde_json::json!({"reason": "build-finished", "success": false}), ms(start, 10));
        ui.finish(Outcome { success: true, exit_code: 0 }, ms(start, 20));
        assert!(buf.contents().contains("failed    in 0.02s\n"));
    }

    #[test]
    fn plain_lock_wait_prints_once_per_owner() {
        let (mut ui, buf, start) = small_ui(Style::Plain, Osc::default(), None);
        ui.lock_waiting(&wait_info(1), ms(start, 10));
        ui.lock_waiting(&wait_info(2), ms(start, 20));
        let mut other = wait_info(3);
        other.owner.as_mut().unwrap().pid = 99;
        ui.lock_waiting(&other, ms(start, 30));
        let out = buf.contents();
        assert_eq!(out.matches("waiting   build lock").count(), 2);
        assert!(out.contains("held by pid 99 (cargo build, 3s), 1 ahead"));
    }

    #[test]
    fn calls_after_finish_are_ignored() {
        let (mut ui, buf, start) = small_ui(Style::Plain, Osc::default(), None);
        let first = ui.finish(Outcome { success: true, exit_code: 0 }, ms(start, 10));
        let written = buf.contents();
        ui.on_cargo_stderr("late line", ms(start, 20));
        let second = ui.finish(Outcome { success: true, exit_code: 0 }, ms(start, 30));
        assert_eq!(buf.contents(), written);
        assert_eq!(first, second);
    }

    #[test]
    fn snapshots_render_through_the_demo_entry_points() {
        let plain = demo::render_snapshot(Style::Plain, 100);
        assert!(plain.starts_with("frust build my-app (rustc 1.99.0)\n"));
        let ansi = demo::render_snapshot(Style::Ansi, 100);
        assert!(ansi.contains("Finished dev profile"));
        assert!(!ansi.contains('\x1b'));
    }
}
