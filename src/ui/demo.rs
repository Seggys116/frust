//! A scripted build that drives the real `Ui` API with synthetic instants.

use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::vt::Vt;
use super::{Outcome, Style, Ui, UiOptions};
use crate::config::Osc;
use crate::events::{CacheResult, Event};
use crate::lock::{Owner, WaitInfo};

pub const SESSION_MS: u64 = 8_700;
const REGISTRY: &str = "registry+https://github.com/rust-lang/crates.io-index";

pub fn cargo_status(verb: &str, text: &str) -> String {
    format!("\x1b[1m\x1b[32m{verb:>12}\x1b[0m {text}")
}

pub fn artifact_message(package_id: &str, target: &str, kind: &str, fresh: bool) -> Value {
    json!({
        "reason": "compiler-artifact",
        "package_id": package_id,
        "target": {"kind": [kind], "crate_types": [kind], "name": target},
        "filenames": [],
        "fresh": fresh,
    })
}

pub fn cache_event(result: CacheResult, krate: &str, saved_ms: u64) -> Event {
    Event::Cache {
        t: 0,
        result,
        krate: krate.to_string(),
        version: String::new(),
        compile_ms: saved_ms,
        saved_ms,
        bytes: 0,
    }
}

pub fn wait_info(held_secs: u64) -> WaitInfo {
    WaitInfo {
        owner: Some(Owner {
            pid: 4242,
            command: "cargo build".to_string(),
            cwd: "/work/other".to_string(),
            since_unix: 0,
        }),
        ahead: 1,
        waited: Duration::from_secs(held_secs),
    }
}

/// Cloneable so the caller keeps a handle to what the `Ui` wrote.
#[derive(Clone, Default)]
pub struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl SharedBuf {
    pub fn new() -> SharedBuf {
        SharedBuf::default()
    }

    pub fn contents(&self) -> String {
        let bytes = self.0.lock().unwrap_or_else(|e| e.into_inner());
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub fn demo_options(style: Style, width: u16, osc: Osc) -> UiOptions {
    UiOptions {
        style,
        width,
        osc,
        workspace: "my-app".to_string(),
        command: "build".to_string(),
        expected_units: Some(14),
        toolchain: "rustc 1.99.0".to_string(),
    }
}

struct Script<'a> {
    ui: &'a mut Ui,
    start: Instant,
    ms: u64,
    pace: &'a mut dyn FnMut(Duration),
}

impl Script<'_> {
    fn at(&mut self, ms: u64) -> Instant {
        if ms > self.ms {
            (self.pace)(Duration::from_millis(ms - self.ms));
            self.ms = ms;
        }
        self.start + Duration::from_millis(self.ms)
    }

    fn tick(&mut self, ms: u64) {
        let now = self.at(ms);
        self.ui.tick(now);
    }

    fn lock_wait(&mut self, ms: u64, held_secs: u64) {
        let now = self.at(ms);
        self.ui.lock_waiting(&wait_info(held_secs), now);
    }

    fn lock_acquired(&mut self, ms: u64) {
        let now = self.at(ms);
        self.ui.lock_acquired(now);
    }

    fn stderr(&mut self, ms: u64, line: &str) {
        let now = self.at(ms);
        self.ui.on_cargo_stderr(line, now);
    }

    fn status(&mut self, ms: u64, verb: &str, text: &str) {
        self.stderr(ms, &cargo_status(verb, text));
    }

    fn json(&mut self, ms: u64, msg: &Value) {
        let now = self.at(ms);
        self.ui.on_cargo_json(msg, now);
    }

    fn cache(&mut self, ms: u64, result: CacheResult, krate: &str, saved_ms: u64) {
        let now = self.at(ms);
        self.ui.on_cache_event(&cache_event(result, krate, saved_ms), now);
    }

    fn registry_artifact(&mut self, ms: u64, name: &str, version: &str, target: &str, kind: &str, fresh: bool) {
        let id = format!("{REGISTRY}#{name}@{version}");
        self.json(ms, &artifact_message(&id, target, kind, fresh));
    }
}

const WARNING: &[&str] = &[
    "\x1b[1m\x1b[33mwarning\x1b[0m\x1b[1m: unused variable: `x`\x1b[0m",
    "\x1b[1m\x1b[38;5;12m  --> \x1b[0mcrates/core/src/lib.rs:12:9",
    "\x1b[1m\x1b[38;5;12m   |\x1b[0m",
    "\x1b[1m\x1b[38;5;12m12 |\x1b[0m     let x = 1;",
    "\x1b[1m\x1b[38;5;12m   |\x1b[0m         \x1b[1m\x1b[33m^\x1b[0m \x1b[1m\x1b[33mhelp: if this is intentional, prefix it with an underscore: `_x`\x1b[0m",
    "\x1b[1m\x1b[38;5;12m   |\x1b[0m",
    "\x1b[1m\x1b[38;5;12m   = \x1b[0m\x1b[1mnote\x1b[0m: `#[warn(unused_variables)]` on by default",
    "",
    "\x1b[1m\x1b[33mwarning\x1b[0m\x1b[1m: `my-app-core` (lib) generated 1 warning\x1b[0m",
];

/// Does not finish the build; the returned instant is when the caller should call `finish`.
pub fn scripted_session(ui: &mut Ui, start: Instant) -> Instant {
    scripted_session_paced(ui, start, &mut |_| {})
}

pub fn scripted_session_paced(ui: &mut Ui, start: Instant, pace: &mut dyn FnMut(Duration)) -> Instant {
    let mut s = Script { ui, start, ms: 0, pace };
    let comp = |name: &str, version: &str, suffix: &str| format!("{name} v{version}{suffix}");

    // Another frust run holds the build lock first; the second poll reports the same owner.
    s.lock_wait(300, 12);
    s.tick(600);
    s.lock_wait(1300, 13);
    s.lock_acquired(2000);

    s.status(2000, "Blocking", "waiting for file lock on package cache");
    s.status(2100, "Downloading", "crates ...");
    s.status(2300, "Downloaded", "log v0.4.21");

    s.status(2500, "Compiling", &comp("proc-macro2", "1.0.86", ""));
    s.status(2500, "Compiling", &comp("unicode-ident", "1.0.12", ""));
    // A hit that lands while the unit is still running.
    s.cache(2600, CacheResult::Hit, "unicode_ident", 600);
    s.tick(2800);
    s.registry_artifact(2900, "unicode-ident", "1.0.12", "unicode_ident", "lib", false);
    s.cache(3000, CacheResult::Miss, "proc_macro2", 0);
    s.registry_artifact(3000, "proc-macro2", "1.0.86", "proc_macro2", "lib", false);

    s.status(3000, "Compiling", &comp("quote", "1.0.36", ""));
    s.status(3000, "Compiling", &comp("syn", "2.0.1", ""));
    s.tick(3100);
    s.registry_artifact(3200, "quote", "1.0.36", "quote", "lib", false);
    // A hit that lands after the unit was already counted as compiled.
    s.cache(3300, CacheResult::Hit, "quote", 1800);

    s.status(3300, "Compiling", &comp("serde", "1.0.200", " (build script)"));
    s.registry_artifact(3700, "serde", "1.0.200", "build_script_build", "custom-build", false);
    s.json(3750, &json!({"reason": "build-script-executed", "package_id": format!("{REGISTRY}#serde@1.0.200")}));
    s.status(3800, "Compiling", &comp("serde", "1.0.200", ""));
    s.status(3800, "Compiling", &comp("serde_derive", "1.0.200", ""));
    s.cache(4000, CacheResult::Hit, "serde_derive", 1300);
    s.tick(4100);
    s.registry_artifact(4300, "serde_derive", "1.0.200", "serde_derive", "proc-macro", false);

    // Cargo prints nothing for fresh units, only the artifact message.
    s.registry_artifact(4400, "itoa", "1.0.11", "itoa", "lib", true);
    s.registry_artifact(4400, "libc", "0.2.155", "libc", "lib", true);
    s.registry_artifact(4400, "cfg-if", "1.0.0", "cfg_if", "lib", true);
    s.tick(4800);

    s.registry_artifact(5200, "serde", "1.0.200", "serde", "lib", false);
    s.cache(5200, CacheResult::Miss, "serde", 0);
    s.status(5300, "Compiling", &comp("log", "0.4.21", ""));
    s.tick(5500);
    let old_style_id = format!("log 0.4.21 ({REGISTRY})");
    s.json(5600, &artifact_message(&old_style_id, "log", "lib", false));
    s.cache(5600, CacheResult::Miss, "log", 0);
    s.tick(6000);

    s.registry_artifact(6400, "syn", "2.0.1", "syn", "lib", false);
    s.cache(6400, CacheResult::Miss, "syn", 0);
    s.status(6400, "Compiling", &comp("my-app-core", "0.1.0", " (/work/my-app/crates/core)"));
    s.tick(6800);
    for (i, line) in WARNING.iter().enumerate() {
        s.stderr(7000 + 20 * i as u64, line);
    }
    s.json(7400, &artifact_message("path+file:///work/my-app/crates/core#my-app-core@0.1.0", "my_app_core", "lib", false));
    s.status(7400, "Compiling", &comp("my-app", "0.1.0", " (/work/my-app)"));
    s.tick(8000);
    s.json(8600, &artifact_message("path+file:///work/my-app#0.1.0", "my_app", "bin", false));
    s.json(8650, &json!({"reason": "build-finished", "success": true}));
    s.status(SESSION_MS, "Finished", "`dev` profile [unoptimized + debuginfo] target(s) in 8.70s");
    s.at(SESSION_MS)
}

pub fn render_raw(style: Style, width: u16, osc: Osc) -> String {
    let buf = SharedBuf::new();
    let start = Instant::now();
    let mut ui = Ui::new(demo_options(style, width, osc), Box::new(buf.clone()), start);
    let end = scripted_session(&mut ui, start);
    ui.finish(Outcome { success: true, exit_code: 0 }, end);
    buf.contents()
}

/// Ansi output is replayed through the virtual terminal with all OSC features on.
pub fn render_snapshot(style: Style, width: u16) -> String {
    match style {
        Style::Plain => render_raw(style, width, Osc::default()),
        Style::Ansi => {
            let raw = render_raw(style, width, Osc { progress: true, title: true, links: true });
            let mut vt = Vt::new(width, 500);
            vt.feed(&raw);
            let mut text = vt.plain_text();
            text.push('\n');
            text
        }
    }
}
