//! frust's own commands, the build display, and lock queueing between processes.

mod common;

use std::fs;
use std::time::{Duration, Instant};

use common::{Sandbox, assert_success, stderr, stdout};
use frust::ui::vt::Vt;

const LEAF: &str = "pub fn value() -> u32 { 41 }\n";
const MAIN: &str = "fn main() { println!(\"{}\", leaf::value()); }\n";

fn project_with_leaf(sb: &Sandbox, dir: &str) -> std::path::PathBuf {
    sb.vendor_crate("leaf", "1.0.0", LEAF);
    sb.project(dir, &[("leaf", "1.0.0")], MAIN)
}

#[test]
fn plain_renderer_reports_units_and_cache_use() {
    let sb = Sandbox::new("plain");
    let p = project_with_leaf(&sb, "a");
    let first = sb.bin("fcargo").arg("build").env("FRUST_RENDER", "plain").current_dir(&p).output().unwrap();
    assert_success(&first);
    let text = stderr(&first);
    assert!(text.starts_with("frust build"), "{text}");
    assert!(text.contains("compiled  leaf v1.0.0"), "{text}");
    assert!(text.contains("finished  dev profile"), "{text}");
    assert!(!text.contains('\x1b'), "plain mode must emit no escape codes: {text:?}");

    sb.bin("fcargo").arg("clean").current_dir(&p).output().unwrap();
    let second = sb.bin("fcargo").arg("build").env("FRUST_RENDER", "plain").current_dir(&p).output().unwrap();
    let text = stderr(&second);
    assert!(text.contains("cached    leaf v1.0.0"), "{text}");
    assert!(text.contains("cached ") && !text.contains("cached 0"), "units must be reported as cached: {text}");
}

#[test]
fn ansi_renderer_leaves_a_clean_final_screen() {
    let sb = Sandbox::new("ansi");
    let p = project_with_leaf(&sb, "a");
    let out = sb
        .bin("fcargo")
        .arg("build")
        .env("FRUST_RENDER", "ansi")
        .env("FRUST_OSC", "off")
        .env_remove("NO_COLOR")
        .current_dir(&p)
        .output()
        .unwrap();
    assert_success(&out);
    let raw = stderr(&out);
    assert!(raw.contains("\x1b[?25l") && raw.contains("\x1b[?25h"), "cursor must be hidden then restored");

    let mut vt = Vt::new(100, 40);
    vt.feed(&raw);
    let screen = vt.plain_text();
    assert!(screen.contains("Finished"), "{screen}");
    assert!(screen.contains("units"), "{screen}");
    assert!(!screen.contains('%') && !screen.contains('▸'), "no live region may remain: {screen}");
    assert!(vt.cursor_visible());
}

#[test]
fn osc_sequences_follow_configuration() {
    let sb = Sandbox::new("osc");
    let p = project_with_leaf(&sb, "a");
    let with = sb.bin("fcargo").arg("build").env("FRUST_RENDER", "ansi").env("FRUST_OSC", "all").current_dir(&p).output().unwrap();
    let raw = stderr(&with);
    assert!(raw.contains("\x1b]9;4;"), "progress OSC expected");
    assert!(raw.contains("\x1b]2;frust:"), "title OSC expected");
    assert!(raw.contains("\x1b]9;4;0"), "progress must be cleared at the end");

    sb.bin("fcargo").arg("clean").current_dir(&p).output().unwrap();
    let without =sb.bin("fcargo").arg("build").env("FRUST_RENDER", "ansi").env("FRUST_OSC", "off").current_dir(&p).output().unwrap();
    assert!(!stderr(&without).contains("\x1b]"), "no OSC when disabled");
}

#[test]
fn concurrent_builds_queue_instead_of_clashing() {
    let sb = Sandbox::new("lock");
    let p = sb.project("slow", &[], "fn main() {}\n");
    fs::write(p.join("build.rs"), r#"fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    std::fs::write("gate-started", b"started").unwrap();
    let started = std::time::Instant::now();
    while !std::path::Path::new("gate-open").exists() {
        assert!(started.elapsed() < std::time::Duration::from_secs(120), "gate never opened");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
"#).unwrap();

    let mut first = sb.bin("fcargo").arg("build").env("FRUST_RENDER", "plain").current_dir(&p).spawn_captured(&sb, "first");
    if !wait_until(|| p.join("gate-started").exists()) {
        fs::write(p.join("gate-open"), b"open").unwrap();
        first.stop();
        let first = first.finish();
        panic!("holder never reached the build script: {}", stderr(&first));
    }
    let holder_pid = first.child.id();
    let second = sb.bin("fcargo").arg("build").env("FRUST_RENDER", "plain").current_dir(&p).spawn_captured(&sb, "second");
    let dirs = frust::paths::Dirs::at(sb.home());
    let joined = wait_until(|| frust::lock::list(&dirs).iter().any(|lock| {
        lock.holder.as_ref().is_some_and(|holder| holder.owner.pid == holder_pid) && lock.joiners.len() == 1
    }));
    fs::write(p.join("gate-open"), b"open").unwrap();
    let second = second.finish();
    let first = first.finish();
    assert_success(&first);
    assert_success(&second);
    assert!(joined, "follower never joined the holder: {}", stderr(&second));
    let text = stderr(&second);
    // An identical build in the same repo joins the running one instead of queueing behind it.
    assert!(text.contains("joined    running build of") || text.contains("shared    build of"), "second build must join the first: {text}");
}

fn wait_until(mut ready: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !ready() {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    true
}

#[test]
fn stats_gc_doctor_and_locks_commands_work() {
    let sb = Sandbox::new("meta");
    let p = project_with_leaf(&sb, "a");
    assert_success(&sb.bin("fcargo").arg("build").current_dir(&p).output().unwrap());

    let stats = sb.bin("frust").arg("stats").output().unwrap();
    assert_success(&stats);
    assert!(stdout(&stats).contains("Compile cache") && stdout(&stats).contains("hit rate"), "{}", stdout(&stats));

    let doctor = sb.bin("frust").arg("doctor").output().unwrap();
    assert_success(&doctor);
    assert!(stdout(&doctor).contains("rustc wrapper"), "{}", stdout(&doctor));

    let locks = sb.bin("frust").arg("locks").output().unwrap();
    assert_success(&locks);
    assert!(stdout(&locks).contains("no builds running"), "{}", stdout(&locks));

    let dry = sb.bin("frust").args(["gc", "--max-gib", "0", "--dry-run"]).output().unwrap();
    assert_success(&dry);
    assert!(stdout(&dry).contains("cache entries expired, superseded or over a set size cap"), "{}", stdout(&dry));
    assert!(sb.count_cache("miss", "leaf") == 1);
}

#[test]
fn env_prints_the_wrapper_line_with_the_platform_suffix() {
    let sb = Sandbox::new("env-wrapper");
    let p = project_with_leaf(&sb, "a");
    let out = sb.bin("frust").arg("env").current_dir(&p).output().unwrap();
    assert_success(&out);
    let text = stdout(&out);
    let line = text.lines().find(|l| l.starts_with("export RUSTC_WRAPPER=")).unwrap_or_else(|| panic!("no RUSTC_WRAPPER line: {text}"));
    let path = std::path::PathBuf::from(line.trim_start_matches("export RUSTC_WRAPPER='").trim_end_matches('\''));
    let expected = format!("frust-wrap{}", std::env::consts::EXE_SUFFIX);
    assert_eq!(path.file_name().unwrap().to_string_lossy(), expected, "{line}");
    assert!(path.is_file(), "{line}");

    let doctor = sb.bin("frust").arg("doctor").current_dir(&p).output().unwrap();
    assert!(stdout(&doctor).contains("frust-wrap found next to the frust binary"), "{}", stdout(&doctor));
}

#[test]
fn gc_refuses_without_confirmation_and_frees_with_it() {
    let sb = Sandbox::new("gc");
    let p = project_with_leaf(&sb, "a");
    assert_success(&sb.bin("fcargo").arg("build").current_dir(&p).output().unwrap());

    let refused = sb.bin("frust").args(["gc", "--max-gib", "0"]).stdin(std::process::Stdio::null()).output().unwrap();
    assert!(!refused.status.success(), "non-interactive gc must not delete unasked");

    let done = sb.bin("frust").args(["gc", "--max-gib", "0", "--yes"]).output().unwrap();
    assert_success(&done);
    assert!(stdout(&done).contains("freed"));

    // Everything is rebuilt transparently afterwards.
    assert_success(&sb.bin("fcargo").arg("clean").current_dir(&p).output().unwrap());
    assert_success(&sb.bin("fcargo").arg("build").current_dir(&p).output().unwrap());
}

#[test]
fn gc_and_compact_cover_every_workspace_and_report_progress_on_stderr() {
    let sb = Sandbox::new("global-progress");
    let a = project_with_leaf(&sb, "alpha");
    let b = project_with_leaf(&sb, "beta");
    for p in [&a, &b] {
        assert_success(&sb.bin("fcargo").arg("build").current_dir(p).output().unwrap());
    }
    let workspace = |p: &std::path::Path| sb.central_target(p).parent().unwrap().file_name().unwrap().to_string_lossy().into_owned();
    let (wa, wb) = (workspace(&a), workspace(&b));
    assert_ne!(wa, wb);

    let dry = sb.bin("frust").args(["gc", "--dry-run", "--max-gib", "0", "--progress"]).output().unwrap();
    assert_success(&dry);
    let (out, err) = (stdout(&dry), stderr(&dry));
    assert!(out.contains("by workspace") && out.contains(&wa) && out.contains(&wb) && out.contains("cache store"), "{out}");
    for phase in ["scan store", "size store entries", "measure targets", "plan targets", "plan home"] {
        assert!(err.lines().any(|l| l.starts_with(&format!("frust: gc {phase}  "))), "no {phase} line: {err}");
    }
    assert!(!err.contains('\r'), "a log gets whole lines, not a live status: {err:?}");

    let quiet = sb.bin("frust").args(["gc", "--dry-run", "--progress", "--quiet"]).output().unwrap();
    assert_success(&quiet);
    assert!(!stderr(&quiet).contains("frust: gc"), "{}", stderr(&quiet));
    let default = sb.bin("frust").args(["gc", "--dry-run"]).output().unwrap();
    assert!(!stderr(&default).contains("frust: gc"), "FRUST_RENDER=off keeps logs quiet unless --progress: {}", stderr(&default));

    let compact = sb.bin("frust").args(["compact", "--dry-run", "--progress"]).output().unwrap();
    assert_success(&compact);
    let (out, err) = (stdout(&compact), stderr(&compact));
    assert!(out.contains(&wa) && out.contains(&wb), "{out}");
    assert!(err.lines().any(|l| l.starts_with("frust: compact measure  ")) && err.lines().any(|l| l.starts_with("frust: compact lock profiles  ")), "{err}");

    let scoped = sb.bin("frust").args(["compact", "--dry-run", "--workspace", &a.display().to_string()]).output().unwrap();
    assert_success(&scoped);
    let out = stdout(&scoped);
    assert!(out.contains(&wa) && !out.contains(&wb) && !out.contains("cache store"), "{out}");
    let unknown = sb.bin("frust").args(["compact", "--dry-run", "--workspace", "no-such-workspace"]).output().unwrap();
    assert_eq!(unknown.status.code(), Some(2), "{}", stderr(&unknown));
}

#[test]
fn ui_demo_snapshot_renders_headlessly() {
    let sb = Sandbox::new("demo");
    let out = sb.bin("frust").args(["ui-demo", "--snapshot", "--width", "70"]).output().unwrap();
    assert_success(&out);
    let text = stdout(&out);
    assert!(text.contains("Finished") && text.contains("cached"), "{text}");
    assert!(text.lines().all(|l| l.chars().count() <= 70 || l.starts_with("  ") || l.starts_with("warning")), "{text}");
}

struct BuildChild {
    child: std::process::Child,
    stdout: std::path::PathBuf,
    stderr: std::path::PathBuf,
}

impl BuildChild {
    fn stop(&mut self) {
        common::kill_tree(self.child.id());
        let _ = self.child.wait();
    }

    fn finish(mut self) -> std::process::Output {
        if !wait_until(|| self.child.try_wait().unwrap().is_some()) {
            self.stop();
            panic!("build never finished: {}", fs::read_to_string(&self.stderr).unwrap());
        }
        std::process::Output {
            status: self.child.wait().unwrap(),
            stdout: fs::read(&self.stdout).unwrap(),
            stderr: fs::read(&self.stderr).unwrap(),
        }
    }
}

impl Drop for BuildChild {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            self.stop();
        }
    }
}

trait SpawnCaptured {
    fn spawn_captured(&mut self, sb: &Sandbox, tag: &str) -> BuildChild;
}

impl SpawnCaptured for std::process::Command {
    fn spawn_captured(&mut self, sb: &Sandbox, tag: &str) -> BuildChild {
        let stdout = sb.root.join(format!("{tag}.stdout"));
        let stderr = sb.root.join(format!("{tag}.stderr"));
        self.stdout(fs::File::create(&stdout).unwrap()).stderr(fs::File::create(&stderr).unwrap());
        let child = common::own_group(self).spawn().unwrap();
        BuildChild { child, stdout, stderr }
    }
}
