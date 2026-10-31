//! Waiters differ from the holder by a harmless `-j 2` so they queue instead of joining (see `join.rs`); a gate-file build script holds the holder mid-build.

mod common;

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use common::Sandbox;

const EARLY: &str = "pub fn early() -> u32 { 1 }\n";
const GATE_LIB: &str = "pub fn gate() -> u32 { 2 }\n";
const GATE_BUILD: &str = r#"fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if let Some(started) = std::env::var_os("FRUST_TEST_STARTED") {
        let _ = std::fs::write(started, b"started");
    }
    if let Some(gate) = std::env::var_os("FRUST_TEST_GATE") {
        let gate = std::path::PathBuf::from(gate);
        let begun = std::time::Instant::now();
        while !gate.exists() && begun.elapsed() < std::time::Duration::from_secs(120) {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}
"#;
const MAIN: &str = "fn main() { println!(\"{}\", early::early() + gate::gate()); }\n";
const WAIT: Duration = Duration::from_secs(60);

struct Scene {
    sb: Sandbox,
    project: PathBuf,
    gate: PathBuf,
    started: PathBuf,
    lock_dir: PathBuf,
    slot: PathBuf,
}

impl Scene {
    fn new(tag: &str) -> Scene {
        let sb = Sandbox::new(tag);
        sb.vendor_crate("early", "1.0.0", EARLY);
        sb.vendor_crate("gate", "1.0.0", GATE_LIB);
        fs::write(sb.vendor().join("gate-1.0.0").join("build.rs"), GATE_BUILD).unwrap();
        let project = sb.project("app", &[("early", "1.0.0"), ("gate", "1.0.0")], MAIN);
        let target = sb.central_target(&project);
        let lock_dir = frust::lock::dir_for(&frust::paths::Dirs::at(sb.home()), &target);
        let slot = frust::lock::slot_dir_for(&frust::paths::Dirs::at(sb.home()), &target, "debug@host");
        let (gate, started) = (sb.root.join("gate-open"), sb.root.join("gate-started"));
        Scene { sb, project, gate, started, lock_dir, slot }
    }

    fn build(&self, render: &str, extra: &[(&str, &str)]) -> Running {
        self.build_args(&["build"], render, extra)
    }

    fn waiter(&self, render: &str, extra: &[(&str, &str)]) -> Running {
        self.build_args(&["build", "-j", "2"], render, extra)
    }

    fn build_args(&self, args: &[&str], render: &str, extra: &[(&str, &str)]) -> Running {
        let mut cmd = self.sb.bin("fcargo");
        cmd.args(args)
            .current_dir(&self.project)
            .env("FRUST_RENDER", render)
            .env("FRUST_TEST_GATE", &self.gate)
            .env("FRUST_TEST_STARTED", &self.started)
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        common::own_group(&mut cmd);
        for (k, v) in extra {
            cmd.env(k, v);
        }
        Running::spawn(cmd)
    }

    fn open_gate(&self) {
        fs::write(&self.gate, b"open").unwrap();
    }

    fn wait_started(&self) {
        wait_for("the holder's build script to start", || self.started.exists());
    }

    fn progress(&self) -> Option<serde_json::Value> {
        serde_json::from_slice(&fs::read(self.slot.join("progress.json")).ok()?).ok()
    }

    fn owner_pid(&self) -> Option<u64> {
        let owner: serde_json::Value = serde_json::from_slice(&fs::read(self.slot.join("owner.json")).ok()?).ok()?;
        owner["pid"].as_u64()
    }

    fn tickets(&self) -> Vec<String> {
        fs::read_dir(self.lock_dir.join("queue"))
            .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_default()
    }
}

impl Drop for Scene {
    fn drop(&mut self) {
        // Never leave a build script spinning, whatever the test outcome.
        let _ = fs::write(&self.gate, b"open");
    }
}

struct Running {
    child: Child,
    stderr: Arc<Mutex<String>>,
    reader: Option<thread::JoinHandle<()>>,
}

impl Running {
    fn spawn(mut cmd: Command) -> Running {
        let mut child = cmd.spawn().unwrap();
        let pipe = child.stderr.take().unwrap();
        let stderr = Arc::new(Mutex::new(String::new()));
        let sink = stderr.clone();
        let reader = thread::spawn(move || {
            for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                let mut text = sink.lock().unwrap();
                text.push_str(&line);
                text.push('\n');
            }
        });
        Running { child, stderr, reader: Some(reader) }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn stderr(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }

    fn wait_stderr(&self, what: &str, pred: impl Fn(&str) -> bool) {
        let deadline = Instant::now() + WAIT;
        while !pred(&self.stderr()) {
            assert!(Instant::now() < deadline, "timed out waiting for {what}; stderr so far:\n{}", self.stderr());
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn finish(mut self) -> (ExitStatus, String) {
        let status = self.child.wait().unwrap();
        if let Some(reader) = self.reader.take() {
            reader.join().unwrap();
        }
        (status, self.stderr())
    }

    fn kill_group(&self) {
        common::kill_tree(self.pid());
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            self.kill_group();
            let _ = self.child.wait();
        }
    }
}

fn wait_for(what: &str, mut pred: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !pred() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(20));
    }
}

fn user() -> String {
    frust::lock::current_user().0
}

fn assert_ok(status: ExitStatus, stderr: &str) {
    assert!(status.success(), "build failed ({status:?}):\n{stderr}");
}

fn assert_queue_clean(scene: &Scene) {
    assert!(scene.tickets().is_empty(), "leftover tickets: {:?}", scene.tickets());
    assert!(!scene.slot.join("owner.json").exists());
    assert!(!scene.slot.join("progress.json").exists());
    let logs: Vec<_> = fs::read_dir(&scene.slot).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().ends_with(".log")).collect();
    assert!(logs.is_empty(), "build logs removed: {logs:?}");
}

fn app_output(scene: &Scene) -> String {
    let out = common::run_binary(&scene.sb.central_target(&scene.project), "debug", "app");
    common::stdout(&out).trim().to_string()
}

#[test]
fn waiter_shows_holder_and_progress_then_reuses_its_work() {
    let scene = Scene::new("collab-share");
    let holder = scene.build("plain", &[]);
    let holder_pid = holder.pid();
    scene.wait_started();
    wait_for("published progress", || scene.progress().is_some_and(|p| p["done"].as_u64().unwrap_or(0) >= 1));

    let waiter = scene.waiter("off", &[]);
    let expect = format!("held by {} pid {holder_pid}", user());
    waiter.wait_stderr("the wait line with user, pid and progress", |e| e.contains(&expect) && e.contains("compiling gate"));
    assert_eq!(scene.tickets().len(), 1, "the waiter holds exactly one ticket");

    scene.open_gate();
    let (status, holder_err) = holder.finish();
    assert_ok(status, &holder_err);
    let (status, waiter_err) = waiter.finish();
    assert_ok(status, &waiter_err);

    let joined = format!("frust: joined build of {} pid {holder_pid}", user());
    assert!(waiter_err.contains(&joined), "{waiter_err}");
    assert!(!waiter_err.contains("identical command"), "a different command queues, it does not follow:\n{waiter_err}");
    // The waiter's cargo found everything fresh: nothing was compiled or even restored twice.
    for krate in ["early", "gate"] {
        assert_eq!(scene.sb.count_cache("miss", krate), 1, "{krate} compiled once");
        assert_eq!(scene.sb.count_cache("hit", krate), 0, "{krate} not even restored again");
    }
    assert_queue_clean(&scene);
    assert_eq!(app_output(&scene), "3");
}

#[test]
fn rendered_waiter_reports_fresh_units_reused() {
    let scene = Scene::new("collab-render");
    let holder = scene.build("off", &[]);
    let holder_pid = holder.pid();
    scene.wait_started();
    let waiter = scene.waiter("plain", &[]);
    waiter.wait_stderr("the plain lock line", |e| e.contains(&format!("held by pid {holder_pid} ({}: cargo build", user())));

    scene.open_gate();
    let (status, err) = holder.finish();
    assert_ok(status, &err);
    let (status, err) = waiter.finish();
    assert_ok(status, &err);
    let line = err.lines().find(|l| l.starts_with("joined    build of")).unwrap_or_else(|| panic!("no joined line:\n{err}"));
    assert!(line.contains(&format!("{} pid {holder_pid}", user())), "{line}");
    // early, gate's build script, gate and app: all fresh for the waiter.
    let reused: u32 = line.rsplit(", ").next().unwrap().split(' ').next().unwrap().parse().unwrap();
    assert!(reused >= 4, "{line}");
    assert_eq!(scene.sb.count_cache("miss", "early"), 1);
    assert_queue_clean(&scene);
}

#[test]
fn killing_the_holder_lets_the_waiter_proceed() {
    let scene = Scene::new("collab-kill");
    let holder = scene.build("off", &[]);
    scene.wait_started();
    let waiter = scene.waiter("off", &[]);
    waiter.wait_stderr("the wait line", |e| e.contains(&format!("pid {}", holder.pid())));

    // Ctrl-C style: the whole group (fcargo, cargo, rustc, build script) dies at once.
    holder.kill_group();
    let (status, _) = holder.finish();
    assert!(!status.success());
    let waiter_pid = u64::from(waiter.pid());
    wait_for("the waiter to take over the lock", || scene.owner_pid() == Some(waiter_pid));

    scene.open_gate();
    let (status, err) = waiter.finish();
    assert_ok(status, &err);
    assert!(!err.contains("joined build"), "a killed build has nothing to join:\n{err}");
    assert_queue_clean(&scene);
    assert_eq!(app_output(&scene), "3");
}

#[test]
fn timeout_reports_the_holder_and_withdraws_the_ticket() {
    let scene = Scene::new("collab-timeout");
    let holder = scene.build("off", &[]);
    scene.wait_started();
    let waiter = scene.waiter("off", &[("FRUST_LOCK_TIMEOUT_SECS", "1")]);
    let (status, err) = waiter.finish();
    assert_eq!(status.code(), Some(75), "{err}");
    assert!(err.contains(&format!("timed out waiting for build lock held by {} pid {}", user(), holder.pid())), "{err}");
    assert!(scene.tickets().is_empty(), "ticket withdrawn: {:?}", scene.tickets());

    scene.open_gate();
    let (status, err) = holder.finish();
    assert_ok(status, &err);
}

#[test]
fn signals_while_queued_withdraw_the_ticket() {
    let scene = Scene::new("collab-signal");
    let holder = scene.build("off", &[]);
    scene.wait_started();
    // SIGTERM has no Windows counterpart; Ctrl-Break stands in for the interrupt there.
    #[cfg(unix)]
    let signals = [2, 15];
    #[cfg(windows)]
    let signals = [2];
    for signal in signals {
        let waiter = scene.waiter("off", &[]);
        waiter.wait_stderr("the wait line", |e| e.contains("waiting for build lock"));
        assert_eq!(scene.tickets().len(), 1);
        #[cfg(unix)]
        {
            // SAFETY: signalling only the waiting fcargo process.
            unsafe { libc::kill(waiter.pid() as i32, signal) };
        }
        #[cfg(windows)]
        common::interrupt(&waiter.child);
        let (status, err) = waiter.finish();
        assert_eq!(status.code(), Some(128 + signal), "{err}");
        assert!(err.contains("interrupted while waiting for build lock"), "{err}");
        assert!(scene.tickets().is_empty(), "ticket withdrawn after signal {signal}: {:?}", scene.tickets());
    }
    scene.open_gate();
    let (status, err) = holder.finish();
    assert_ok(status, &err);
}

// File modes have no Windows equivalent.
#[cfg(unix)]
#[test]
fn lock_state_is_shared_on_a_group_writable_home() {
    use std::os::unix::fs::PermissionsExt;
    let scene = Scene::new("collab-perms");
    fs::set_permissions(scene.sb.home(), fs::Permissions::from_mode(0o775)).unwrap();
    assert!(!scene.sb.home().join("locks").exists(), "nothing has locked yet");
    scene.open_gate();
    let (status, err) = scene.build("off", &[]).finish();
    assert_ok(status, &err);
    let mode = |p: &std::path::Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&scene.sb.home().join("locks")), 0o775);
    assert_eq!(mode(&scene.lock_dir.join("queue")), 0o775);
    assert_eq!(mode(&scene.lock_dir.join("lock")), 0o664);
    assert_eq!(mode(&scene.slot.join("last.json")), 0o664);
    assert_eq!(mode(&scene.slot.join("lock")), 0o664);
}
