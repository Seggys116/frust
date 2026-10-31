//! frust's own commands, the build display, and lock queueing between processes.

mod common;

use std::fs;
use std::time::Duration;

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
    let without = sb.bin("fcargo").arg("build").env("FRUST_RENDER", "ansi").env("FRUST_OSC", "off").current_dir(&p).output().unwrap();
    assert!(!stderr(&without).contains("\x1b]"), "no OSC when disabled");
}

#[test]
fn concurrent_builds_queue_instead_of_clashing() {
    let sb = Sandbox::new("lock");
    let p = sb.project("slow", &[], "fn main() {}\n");
    fs::write(p.join("build.rs"), "fn main() { std::thread::sleep(std::time::Duration::from_millis(1800)); }\n").unwrap();

    let first = sb.bin("fcargo").arg("build").env("FRUST_RENDER", "plain").current_dir(&p).spawn_piped();
    std::thread::sleep(Duration::from_millis(500));
    let second = sb.bin("fcargo").arg("build").env("FRUST_RENDER", "plain").current_dir(&p).output().unwrap();
    let first = first.wait_with_output().unwrap();
    assert_success(&first);
    assert_success(&second);
    let text = stderr(&second);
    // An identical build in the same repo joins the running one instead of queueing behind it.
    assert!(text.contains("joined    running build of") || text.contains("shared    build of"), "second build must join the first: {text}");
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
    assert!(stdout(&dry).contains("cache entries over budget"), "{}", stdout(&dry));
    assert!(sb.count_cache("miss", "leaf") == 1);
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
fn ui_demo_snapshot_renders_headlessly() {
    let sb = Sandbox::new("demo");
    let out = sb.bin("frust").args(["ui-demo", "--snapshot", "--width", "70"]).output().unwrap();
    assert_success(&out);
    let text = stdout(&out);
    assert!(text.contains("Finished") && text.contains("cached"), "{text}");
    assert!(text.lines().all(|l| l.chars().count() <= 70 || l.starts_with("  ") || l.starts_with("warning")), "{text}");
}

trait SpawnPiped {
    fn spawn_piped(&mut self) -> std::process::Child;
}

impl SpawnPiped for std::process::Command {
    fn spawn_piped(&mut self) -> std::process::Child {
        self.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap()
    }
}
