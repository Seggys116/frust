//! The holder is kept mid-build by a vendored dependency whose build script blocks until the test creates a gate file.

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
const BROKEN_MAIN: &str = "fn main() { let n: u32 = \"three\"; println!(\"{}\", early::early() + gate::gate() + n); }\n";
const WAIT: Duration = Duration::from_secs(60);

struct Scene {
    sb: Sandbox,
    project: PathBuf,
    target: PathBuf,
    gate: PathBuf,
    started: PathBuf,
    lock_dir: PathBuf,
}

impl Scene {
    fn new(tag: &str, main: &str) -> Scene {
        let sb = Sandbox::new(tag);
        sb.vendor_crate("early", "1.0.0", EARLY);
        sb.vendor_crate("gate", "1.0.0", GATE_LIB);
        fs::write(sb.vendor().join("gate-1.0.0").join("build.rs"), GATE_BUILD).unwrap();
        let project = sb.project("app", &[("early", "1.0.0"), ("gate", "1.0.0")], main);
        let target = sb.central_target(&project);
        let lock_dir = frust::lock::dir_for(&frust::paths::Dirs::at(sb.home()), &target);
        let (gate, started) = (sb.root.join("gate-open"), sb.root.join("gate-started"));
        Scene { sb, project, target, gate, started, lock_dir }
    }

    fn run(&self, args: &[&str], render: &str, gated: bool) -> Running {
        let mut cmd = self.sb.bin("fcargo");
        cmd.args(args)
            .current_dir(&self.project)
            .env("FRUST_RENDER", render)
            .env("FRUST_TEST_STARTED", &self.started)
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        common::own_group(&mut cmd);
        // The build script reads this variable but cargo does not track it, so omitting it only stops the wait.
        if gated {
            cmd.env("FRUST_TEST_GATE", &self.gate);
        }
        Running::spawn(cmd)
    }

    fn build(&self, render: &str) -> Running {
        self.run(&["build"], render, true)
    }

    fn open_gate(&self) {
        fs::write(&self.gate, b"open").unwrap();
    }

    fn wait_started(&self) {
        wait_for("the holder's build script to start", || self.started.exists());
    }

    fn tickets(&self) -> Vec<String> {
        fs::read_dir(self.lock_dir.join("queue"))
            .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_default()
    }

    fn assert_clean(&self) {
        assert!(self.tickets().is_empty(), "leftover tickets: {:?}", self.tickets());
        for slot in fs::read_dir(self.lock_dir.join("slots")).unwrap().flatten() {
            for entry in fs::read_dir(slot.path()).unwrap().flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                assert!(matches!(name.as_str(), "lock" | "last.json"), "leftover {}", entry.path().display());
            }
        }
    }

    fn app_output(&self, profile: &str) -> String {
        let out = common::run_binary(&self.target, profile, "app");
        common::stdout(&out).trim().to_string()
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

struct Done {
    status: ExitStatus,
    stderr: String,
    at: Instant,
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

    fn running(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }

    fn interrupt(&self) {
        common::interrupt(&self.child);
    }

    fn kill_group(&self) {
        common::kill_tree(self.pid());
    }

    fn into_done(mut self, status: ExitStatus, at: Instant) -> Done {
        if let Some(reader) = self.reader.take() {
            reader.join().unwrap();
        }
        Done { status, stderr: self.stderr(), at }
    }

    fn finish(mut self) -> Done {
        let status = self.child.wait().unwrap();
        let at = Instant::now();
        self.into_done(status, at)
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

fn finish_all(mut runs: Vec<Running>) -> Vec<Done> {
    let mut exits: Vec<Option<(ExitStatus, Instant)>> = runs.iter().map(|_| None).collect();
    let deadline = Instant::now() + WAIT;
    while exits.iter().any(Option::is_none) {
        for (run, exit) in runs.iter_mut().zip(exits.iter_mut()) {
            if exit.is_none()
                && let Some(status) = run.child.try_wait().unwrap()
            {
                *exit = Some((status, Instant::now()));
            }
        }
        assert!(Instant::now() < deadline, "builds did not finish");
        thread::sleep(Duration::from_millis(2));
    }
    runs.into_iter().zip(exits).map(|(run, exit)| exit.map(|(status, at)| run.into_done(status, at)).unwrap()).collect()
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

fn assert_ok(done: &Done) {
    assert!(done.status.success(), "build failed ({:?}):\n{}", done.status, done.stderr);
}

fn count_lines(text: &str, prefix: &str) -> usize {
    text.lines().filter(|l| l.starts_with(prefix)).count()
}

fn millis_apart(a: Instant, b: Instant) -> u128 {
    if a > b { (a - b).as_millis() } else { (b - a).as_millis() }
}

#[test]
fn identical_builds_share_one_compile_and_finish_together() {
    let scene = Scene::new("join-share", MAIN);
    let holder = scene.build("plain");
    let holder_pid = holder.pid();
    scene.wait_started();
    holder.wait_stderr("the holder to finish early", |e| e.contains("compiled  early"));

    let joiner = scene.build("plain");
    let joined = format!("joined    running build of {} pid {holder_pid} (identical command)", user());
    joiner.wait_stderr("the joiner to follow the holder", |e| e.contains(&joined) && e.contains("compiled  early"));
    assert!(!joiner.stderr().contains("waiting   build lock"), "a joiner does not queue:\n{}", joiner.stderr());
    assert_eq!(scene.tickets().len(), 1, "the joiner keeps one ticket while it follows");

    let locks = scene.sb.bin("frust").arg("locks").output().unwrap();
    let locks = common::stdout(&locks);
    assert!(locks.contains("[debug]") && locks.contains(&format!("pid {holder_pid}")) && locks.contains("1 joined"), "{locks}");

    scene.open_gate();
    let done = finish_all(vec![holder, joiner]);
    let (holder, joiner) = (&done[0], &done[1]);
    assert_ok(holder);
    assert_ok(joiner);
    let delta = millis_apart(holder.at, joiner.at);
    eprintln!("joiner finished {delta} ms from the holder");
    assert!(delta < 1500, "both finish together, {delta} ms apart");

    for krate in ["early", "gate"] {
        assert_eq!(scene.sb.count_cache("miss", krate), 1, "{krate} compiled once");
        assert_eq!(scene.sb.count_cache("hit", krate), 0, "{krate} not restored again");
    }
    for text in [&holder.stderr, &joiner.stderr] {
        assert_eq!(count_lines(text, "compiled  early"), 1, "{text}");
        assert_eq!(count_lines(text, "compiled  gate v1.0.0  "), 1, "{text}");
        assert_eq!(count_lines(text, "compiled  app"), 1, "{text}");
    }
    let summary = |text: &str| text.lines().find(|l| l.starts_with("units ")).map(str::to_string);
    assert_eq!(summary(&holder.stderr), summary(&joiner.stderr), "both report the same build");
    let shared = format!("shared    build of {} pid {holder_pid} (identical command), compiled once for both", user());
    assert!(joiner.stderr.contains(&shared), "{}", joiner.stderr);
    scene.assert_clean();
    assert_eq!(scene.app_output("debug"), "3");
}

#[test]
fn compile_error_reaches_every_follower_with_the_same_exit_code() {
    let scene = Scene::new("join-error", BROKEN_MAIN);
    let holder = scene.build("off");
    scene.wait_started();
    let rendered = scene.build("plain");
    let piped = scene.build("off");
    rendered.wait_stderr("the rendered follower", |e| e.contains("joined    running build of"));
    piped.wait_stderr("the piped follower", |e| e.contains("frust: joined running build of"));

    scene.open_gate();
    let done = finish_all(vec![holder, rendered, piped]);
    let code = done[0].status.code();
    assert_eq!(code, Some(101), "{}", done[0].stderr);
    for d in &done {
        assert_eq!(d.status.code(), code, "same exit code:\n{}", d.stderr);
        assert_eq!(d.stderr.matches("error[E0308]").count(), 1, "the error, once:\n{}", d.stderr);
        assert!(d.stderr.contains("expected `u32`, found `&str`"), "{}", d.stderr);
    }
    for d in &done[1..] {
        assert!(d.stderr.contains("(identical command) failed with exit code 101"), "{}", d.stderr);
        assert!(!d.stderr.contains("Blocking"), "a follower runs no cargo of its own:\n{}", d.stderr);
    }
    eprintln!("followers finished {} and {} ms from the holder", millis_apart(done[0].at, done[1].at), millis_apart(done[0].at, done[2].at));
    assert_eq!(scene.sb.count_cache("miss", "gate"), 1);
    scene.assert_clean();
}

#[test]
fn killing_the_holder_lets_the_joiner_finish_the_build() {
    let scene = Scene::new("join-kill", MAIN);
    let holder = scene.build("plain");
    scene.wait_started();
    holder.wait_stderr("the holder to finish early", |e| e.contains("compiled  early"));
    let joiner = scene.build("plain");
    joiner.wait_stderr("the joiner to follow", |e| e.contains("joined    running build of") && e.contains("compiled  early"));

    holder.kill_group();
    let killed = holder.finish();
    assert!(!killed.status.success());
    joiner.wait_stderr("the takeover", |e| e.contains("takeover  build of") && e.contains("building here"));

    scene.open_gate();
    let done = joiner.finish();
    assert_ok(&done);
    // early finished before the kill: fresh on disk, neither recompiled nor shown twice.
    assert_eq!(scene.sb.count_cache("miss", "early"), 1);
    assert_eq!(count_lines(&done.stderr, "compiled  early"), 1, "{}", done.stderr);
    assert!(done.stderr.contains("compiled  app"), "{}", done.stderr);
    scene.assert_clean();
    assert_eq!(scene.app_output("debug"), "3");
}

#[test]
fn release_and_debug_builds_run_side_by_side() {
    let scene = Scene::new("join-profiles", MAIN);
    let mut debug = scene.build("off");
    let debug_started = Instant::now();
    scene.wait_started();

    let release = scene.run(&["build", "--release"], "off", false).finish();
    assert_ok(&release);
    assert!(debug.running(), "the release build finished while the debug build was still running");
    assert!(release.at > debug_started);
    assert!(!release.stderr.contains("waiting for build lock"), "{}", release.stderr);
    assert_eq!(scene.app_output("release"), "3");

    scene.open_gate();
    let debug = debug.finish();
    assert_ok(&debug);
    assert!(debug.at > release.at, "the two builds overlapped");
    assert_eq!(scene.app_output("debug"), "3");
    scene.assert_clean();
}

#[test]
fn different_commands_queue_in_order_behind_a_holder_they_cannot_join() {
    let scene = Scene::new("join-fifo", MAIN);
    let holder = scene.build("off");
    let holder_pid = holder.pid();
    scene.wait_started();
    let first = scene.run(&["build", "-j", "2"], "off", true);
    let held = format!("waiting for build lock held by {} pid {holder_pid}", user());
    first.wait_stderr("the first waiter", |e| e.contains(&held));
    // `check` shares the debug profile directory, so cargo would serialise it anyway.
    let second = scene.run(&["check"], "off", true);
    second.wait_stderr("the second waiter", |e| e.contains(&held) && e.contains("1 ahead in queue"));
    assert_eq!(scene.tickets().len(), 2);

    scene.open_gate();
    let done = finish_all(vec![holder, first, second]);
    for d in &done {
        assert_ok(d);
        assert!(!d.stderr.contains("joined running build"), "{}", d.stderr);
    }
    assert!(done[1].at < done[2].at, "first come, first served");
    scene.assert_clean();
}

#[test]
fn interrupting_a_joiner_leaves_the_holder_alone() {
    let scene = Scene::new("join-signal", MAIN);
    let mut holder = scene.build("off");
    scene.wait_started();
    let joiner = scene.build("plain");
    joiner.wait_stderr("the joiner to follow", |e| e.contains("joined    running build of"));
    assert_eq!(scene.tickets().len(), 1);

    joiner.interrupt();
    let stopped = joiner.finish();
    assert_eq!(stopped.status.code(), Some(130), "{}", stopped.stderr);
    assert!(stopped.stderr.contains("interrupted"), "{}", stopped.stderr);
    assert!(scene.tickets().is_empty(), "ticket withdrawn: {:?}", scene.tickets());
    assert!(holder.running(), "the holder carries on");

    scene.open_gate();
    let done = holder.finish();
    assert_ok(&done);
    scene.assert_clean();
    assert_eq!(scene.app_output("debug"), "3");
}
