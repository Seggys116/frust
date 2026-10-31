//! `frust`'s own commands; everything else goes to cargo unchanged, so new cargo subcommands need no support here.

use std::env;
use std::ffi::OsString;
use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::time::Duration;

use crate::bench;
use crate::compact::{self, Category, CompactOptions};
use crate::config::Config;
use crate::doctor::{self, Status};
use crate::fsx::human_bytes;
use crate::gc::{self, Class, GcOptions};
use crate::lock;
use crate::paths::Dirs;
use crate::run;
use crate::stats;
use crate::style::{Styler, duration_ms, row};
use crate::toolchain::{Probe, resolve_tool};
use crate::ui::{self, Style};
use crate::update;
use crate::workspace;

/// Commands owned by frust. Chosen to avoid every cargo built-in name.
const META: &[&str] = &["stats", "gc", "compact", "doctor", "locks", "env", "where", "bench", "ui-demo", "link", "tool", "self-update", "help"];

pub fn frust_main(args: Vec<OsString>) -> i32 {
    let first = args.first().and_then(|a| a.to_str()).map(str::to_owned);
    match first.as_deref() {
        None => {
            print_help();
            0
        }
        Some("--frust-help") | Some("--help") | Some("-h") if args.len() == 1 => {
            print_help();
            0
        }
        Some("-V") | Some("--version") if args.len() == 1 => {
            println!("frust {}", env!("CARGO_PKG_VERSION"));
            run::cargo_main(args)
        }
        Some(cmd) if META.contains(&cmd) => meta(cmd, args[1..].to_vec()),
        Some(_) => run::cargo_main(args),
    }
}

fn meta(cmd: &str, rest: Vec<OsString>) -> i32 {
    let cfg = Config::from_env();
    let dirs = Dirs::discover();
    let flags: Vec<String> = rest.iter().map(|a| a.to_string_lossy().into_owned()).collect();
    match cmd {
        "help" => {
            print_help();
            0
        }
        "stats" => stats_cmd(&dirs, &flags),
        "gc" => gc_cmd(&cfg, &dirs, &flags),
        "compact" => compact_cmd(&dirs, &flags),
        "doctor" => doctor_cmd(&cfg, &dirs),
        "locks" => locks_cmd(&dirs),
        "env" => env_cmd(&dirs),
        "where" => where_cmd(&dirs, &flags),
        "bench" => bench::run(&dirs, &flags),
        "ui-demo" => ui_demo(&flags),
        "link" => link_cmd(&flags),
        "self-update" => update::run(&dirs, &flags),
        "tool" => match flags.split_first() {
            Some((tool, _)) => run::tool_main(tool, rest[1..].to_vec()),
            None => {
                eprintln!("usage: frust tool <name> [args...]");
                2
            }
        },
        _ => 2,
    }
}

fn print_help() {
    let s = Styler::detect();
    println!("{}  a thin accelerator on top of your installed Rust toolchain\n", s.bold("frust"));
    println!("{}", s.bold("Usage"));
    println!("  frust <cargo args>        run cargo with the shared compile cache, central target");
    println!("                            directories, queued build locks and the live build display");
    println!("  fcargo <cargo args>       the same, under cargo's own name");
    println!("  fclippy, frustc, frustdoc, frustup, frustfmt    wrapped toolchain tools\n");
    println!("{}", s.bold("Commands"));
    for (name, text) in [
        ("stats", "cache hit rate, time saved, disk use, target directories"),
        ("gc", "prune the cache and target directories  [--dry-run] [--yes] [--max-gib N] [--max-idle-days N]"),
        ("compact", "shrink target directories: drop superseded variants, compress, dedupe  [--dry-run] [--yes] [--no-compress] [--no-dedupe] [--keep-variants]"),
        ("doctor", "check the environment frust depends on"),
        ("locks", "show running and queued builds"),
        ("env", "print shell exports so other tools (IDEs, rust-analyzer) use the cache"),
        ("where", "print the target directory for this workspace  [cache|root] [--fresh]"),
        ("bench", "measure speedups and disk savings on generated projects  [--json] [--quick] [--runs N] [--linking]"),
        ("ui-demo", "render a scripted build to check the display  [--plain] [--width N]"),
        ("link", "create f<tool> shims for any toolchain tool  [--dir D] <tool>..."),
        ("tool", "run any toolchain tool unchanged: frust tool rustfmt --version"),
        ("self-update", "install the latest release in place  [--yes] [--check] [--version vX.Y.Z] [--disable | --enable]"),
    ] {
        println!("  {:<9} {}", s.cyan(name), text);
    }
    println!("\nAnything else is passed to cargo untouched, including +toolchain and future subcommands.");
    println!("Environment: FRUST_HOME FRUST_CACHE FRUST_TARGET FRUST_TUNE FRUST_LINK FRUST_RENDER FRUST_OSC FRUST_LOCK_TIMEOUT_SECS FRUST_CACHE_MAX_GIB FRUST_UPDATE_CHECK");
}

fn flag_value(flags: &[String], name: &str) -> Option<String> {
    flags.iter().position(|f| f == name).and_then(|i| flags.get(i + 1).cloned())
}

fn stats_cmd(dirs: &Dirs, _flags: &[String]) -> i32 {
    let s = Styler::detect();
    let ledger = stats::read_ledger(dirs);
    let totals = stats::totals(&ledger);
    let store = stats::store_info(dirs);
    let targets = stats::targets(dirs);

    println!("{}", s.bold("Compile cache"));
    println!("{}", row(&s, "entries", &store.entries.to_string()));
    println!(
        "{}",
        row(&s, "size", &format!("{} on disk, {} logical", human_bytes(store.allocated_bytes), human_bytes(store.logical_bytes)))
    );
    println!(
        "{}",
        row(
            &s,
            "hit rate",
            &format!(
                "{}  {}",
                s.green(&format!("{:.1}%", totals.hit_rate() * 100.0)),
                s.dim(&format!("{} hits, {} misses", totals.hits, totals.misses))
            )
        )
    );
    println!("{}", row(&s, "time saved", &format!("{}  {}", s.green(&duration_ms(totals.saved_ms)), s.dim("estimated from original compile times"))));
    println!("{}", row(&s, "data restored", &human_bytes(totals.hit_bytes)));

    println!("\n{}", s.bold("Builds"));
    let avg = totals.run_wall_ms.checked_div(totals.runs).unwrap_or(0);
    println!("{}", row(&s, "runs", &format!("{}  {}", totals.runs, s.dim(&format!("{} failed, {} average", totals.failed_runs, duration_ms(avg))))));

    let uses = stats::workspace_use(&ledger);
    if !uses.is_empty() {
        println!("\n{}", s.bold("Reuse by workspace"));
        for w in uses.iter().take(8) {
            println!(
                "  {:<24} {:>4} builds  {}  {}",
                w.name,
                w.runs,
                s.green(&format!("{:.0}% of units reused", w.reuse_rate() * 100.0)),
                s.dim(&format!("no-change {}  edit {}", duration_ms(w.idle_wall_ms), duration_ms(w.edit_wall_ms)))
            );
        }
    }

    println!("\n{}", s.bold("Target directories"));
    if targets.is_empty() {
        println!("  {}", s.dim("none yet"));
    }
    let mut all = compact::Breakdown::default();
    for target in targets.iter().take(12) {
        let flag = if target.root_exists { String::new() } else { s.yellow("  orphaned") };
        let physical = compact::physical_size(&[&target.path]);
        let b = compact::breakdown(&target.path);
        all.merge(&b);
        // What compaction can remove from the incremental state without costing an edit build.
        let superseded: u64 = compact::profile_dirs(&target.path)
            .iter()
            .map(|p| {
                let a = compact::incremental_anatomy(p);
                a.stale_sessions + a.deps_objects_stale + a.dead_dir_bytes
            })
            .sum();
        let superseded = if superseded > 0 { format!(" ({} superseded)", human_bytes(superseded)) } else { String::new() };
        println!(
            "  {:>10}  {}  {}{}",
            human_bytes(target.allocated_bytes),
            target.workspace,
            s.dim(&format!(
                "{} idle {}  incremental {}{superseded}  physical {}  compressed {} files",
                target.toolchain,
                idle(target.idle),
                human_bytes(b.get(Category::Incremental).allocated),
                human_bytes(physical),
                b.total().compressed
            )),
            flag
        );
    }
    if targets.len() > 12 {
        println!("  {}", s.dim(&format!("... and {} more", targets.len() - 12)));
    }
    let total = all.total();
    if total.files > 0 {
        println!("\n{}", s.bold("Where target bytes go"));
        print_breakdown(&s, &all);
        println!(
            "  {}",
            s.dim(&format!(
                "{} logical, {} allocated; transparent compression saves {} (`frust compact`)",
                human_bytes(total.logical),
                human_bytes(total.allocated),
                human_bytes(compressed_saving(&all))
            ))
        );
    }
    println!("\n{}", s.dim(&format!("state directory: {}", dirs.root().display())));
    0
}

fn compressed_saving(b: &compact::Breakdown) -> u64 {
    b.rows.values().filter(|r| r.compressed > 0).map(|r| r.logical.saturating_sub(r.allocated)).sum()
}

fn print_breakdown(s: &Styler, b: &compact::Breakdown) {
    let total = b.total().allocated.max(1);
    for cat in Category::ALL {
        let row = b.get(cat);
        if row.files == 0 {
            continue;
        }
        let compressed = if row.compressed > 0 { format!("  {} compressed", row.compressed) } else { String::new() };
        println!(
            "  {:<22} {:>10}  {:>5.1}%  {}",
            cat.label(),
            human_bytes(row.allocated),
            row.allocated as f64 * 100.0 / total as f64,
            s.dim(&format!("{} files{compressed}", row.files))
        );
    }
}

fn idle(duration: Duration) -> String {
    let secs = duration.as_secs();
    match secs {
        0..=89 => format!("{secs}s"),
        90..=5399 => format!("{}m", secs / 60),
        5400..=129_599 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

fn gc_cmd(cfg: &Config, dirs: &Dirs, flags: &[String]) -> i32 {
    if flags.iter().any(|f| f == gc::AUTO_CHILD_FLAG) {
        return gc::run_auto_child(dirs, cfg);
    }
    let s = Styler::detect();
    let mut opts = GcOptions::new(cfg.cache_max_bytes);
    if let Some(gib) = flag_value(flags, "--max-gib").and_then(|v| v.parse::<f64>().ok()) {
        opts.max_cache_bytes = (gib * 1024.0 * 1024.0 * 1024.0) as u64;
    }
    if let Some(days) = flag_value(flags, "--max-idle-days").and_then(|v| v.parse::<u64>().ok()) {
        opts.target_max_idle = Duration::from_secs(days * 86_400);
    }
    let plan = gc::plan(dirs, &opts);
    println!("{}", s.bold("Garbage collection plan"));
    for class in Class::ALL.into_iter().filter(|c| *c != Class::Ledger) {
        let (count, bytes) = plan.by_class(class);
        if count > 0 {
            println!("  {:>6} items  {:>10}  {}", count, human_bytes(bytes), class.label());
        }
    }
    if plan.trim_ledger {
        println!("  {:>6}        {:>10}  {}", "-", "-", Class::Ledger.label());
    }
    let cats = plan.target_categories();
    if !cats.is_empty() {
        println!("  {}", s.dim("target directory content removed, by kind:"));
        for (cat, bytes) in cats {
            println!("  {}", s.dim(&format!("        {:>10}  {}", human_bytes(bytes), cat.label())));
        }
    }
    if plan.removals.is_empty() && !plan.trim_ledger {
        println!("  {}", s.dim("nothing to remove"));
        return 0;
    }
    println!("  {} {}", s.bold("total"), s.green(&human_bytes(plan.bytes())));
    if flags.iter().any(|f| f == "--dry-run") {
        for removal in plan.removals.iter().take(40) {
            println!("  {}", s.dim(&removal.path.display().to_string()));
        }
        return 0;
    }
    if !flags.iter().any(|f| f == "--yes" || f == "-y") {
        if !std::io::stdin().is_terminal() {
            eprintln!("frust: refusing to delete without confirmation; re-run with --yes (or --dry-run to list paths)");
            return 1;
        }
        print!("Remove these? [y/N] ");
        let _ = std::io::stdout().flush();
        let mut answer = String::new();
        let _ = std::io::stdin().lock().read_line(&mut answer);
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            println!("aborted");
            return 1;
        }
    }
    let (freed, failures) = gc::execute(dirs, &plan);
    println!("freed {}", s.green(&human_bytes(freed)));
    if compact::physical_size(&[&dirs.targets()]) > 0 && cfg!(target_os = "macos") {
        println!("{}", s.dim("`frust compact` can also compress what remains in target directories"));
    }
    for (path, err) in &failures {
        eprintln!("frust: could not remove {}: {err}", path.display());
    }
    i32::from(!failures.is_empty())
}

fn compact_cmd(dirs: &Dirs, flags: &[String]) -> i32 {
    let s = Styler::detect();
    let has = |f: &str| flags.iter().any(|x| x == f);
    let mut opts = CompactOptions::full();
    opts.compress = !has("--no-compress");
    opts.dedupe = !has("--no-dedupe");
    opts.remove_superseded = !has("--keep-variants");
    let targets = compact::central_targets(dirs);
    if targets.is_empty() && !dirs.store().is_dir() {
        println!("{}", s.dim("no target directories yet"));
        return 0;
    }
    if has("--dry-run") {
        opts.dry_run = true;
        let report = compact::compact(dirs, &targets, &opts);
        println!("{}", s.bold("Compaction plan (estimate; nothing changed)"));
        print_compact_report(&s, &report, true);
        for (path, _) in report.removed.iter().take(40) {
            println!("  {}", s.dim(&path.display().to_string()));
        }
        return 0;
    }
    // Deleting superseded variants follows gc's confirmation rule; compression and dedupe change no contents.
    if opts.remove_superseded && !has("--yes") && !has("-y") {
        let plan = compact::compact(dirs, &targets, &CompactOptions { compress: false, dedupe: false, dry_run: true, ..opts.clone() });
        if !plan.removed.is_empty() {
            println!(
                "{} {} units, {} incremental sessions, {} dead incremental directories and {} object aliases, {}",
                s.bold("Superseded:"),
                plan.dead_units,
                plan.stale_sessions,
                plan.dead_incremental,
                plan.stale_objects,
                human_bytes(plan.removed_bytes)
            );
            if !std::io::stdin().is_terminal() {
                eprintln!("frust: refusing to delete without confirmation; re-run with --yes, --keep-variants or --dry-run");
                return 1;
            }
            print!("Remove these? [y/N] ");
            let _ = std::io::stdout().flush();
            let mut answer = String::new();
            let _ = std::io::stdin().lock().read_line(&mut answer);
            if !matches!(answer.trim(), "y" | "Y" | "yes") {
                println!("aborted");
                return 1;
            }
        }
    }
    let report = compact::compact(dirs, &targets, &opts);
    println!("{}", s.bold("Compaction"));
    print_compact_report(&s, &report, false);
    for (path, err) in &report.errors {
        eprintln!("frust: {}: {err}", path.display());
    }
    i32::from(!report.errors.is_empty())
}

fn print_compact_report(s: &Styler, r: &compact::Report, estimate: bool) {
    let verb = if estimate { "would " } else { "" };
    println!("{}", row(s, "profiles", &format!("{} examined", r.profiles)));
    if r.removed_bytes > 0 || r.dead_units > 0 || r.stale_sessions > 0 || r.stale_objects > 0 || r.dead_incremental > 0 {
        println!(
            "{}",
            row(
                s,
                "superseded",
                &format!(
                    "{verb}remove {} dead units, {} stale incremental sessions, {} dead incremental directories and {} superseded object aliases: {}",
                    r.dead_units,
                    r.stale_sessions,
                    r.dead_incremental,
                    r.stale_objects,
                    s.green(&human_bytes(r.removed_bytes))
                )
            )
        );
    }
    if r.supported {
        println!(
            "{}",
            row(
                s,
                "compressed",
                &format!(
                    "{verb}compress {} files: {} -> {}",
                    r.compressed_files,
                    human_bytes(r.compress_before),
                    human_bytes(r.compress_after)
                )
            )
        );
        println!(
            "{}",
            row(s, "deduplicated", &format!("{verb}clone {} identical files: {} freed", r.deduped_files, human_bytes(r.dedupe_saved)))
        );
    } else {
        println!("{}", row(s, "compression", &s.dim("not available on this platform (APFS only)")));
    }
    let saved = r.physical_before.saturating_sub(r.physical_after);
    let pct = if r.physical_before == 0 { 0.0 } else { saved as f64 * 100.0 / r.physical_before as f64 };
    println!(
        "{}",
        row(
            s,
            "physical size",
            &format!(
                "{} -> {}  {}",
                human_bytes(r.physical_before),
                human_bytes(r.physical_after),
                s.green(&format!("{} smaller ({pct:.0}%)", human_bytes(saved)))
            )
        )
    );
    for busy in &r.busy {
        println!("  {}", s.yellow(&format!("skipped (a build holds it): {}", busy.display())));
    }
    for p in &r.unclassified {
        println!("  {}", s.dim(&format!("fingerprints not understood, nothing removed: {}", p.display())));
    }
}

fn doctor_cmd(cfg: &Config, dirs: &Dirs) -> i32 {
    let s = Styler::detect();
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let checks = doctor::run(cfg, dirs, &cwd);
    let mut failed = false;
    for check in &checks {
        let tag = match check.status {
            Status::Ok => s.green("ok  "),
            Status::Warn => s.yellow("warn"),
            Status::Fail => {
                failed = true;
                s.red("FAIL")
            }
        };
        println!("{tag}  {:<22} {}", check.name, check.detail);
    }
    i32::from(failed)
}

fn locks_cmd(dirs: &Dirs) -> i32 {
    let s = Styler::detect();
    let statuses = lock::list(dirs);
    if statuses.is_empty() {
        println!("{}", s.dim("no builds running or queued"));
        return 0;
    }
    for status in statuses {
        println!("{}", s.bold(&status.target));
        match status.owner {
            Some(owner) => {
                let user = status.holder.as_ref().map(|h| format!("{}  ", h.user)).unwrap_or_default();
                println!(
                    "  building  {user}pid {}  `{}`  for {}s  in {}",
                    owner.pid,
                    owner.command,
                    crate::fsx::unix_now().saturating_sub(owner.since_unix),
                    owner.cwd
                );
                if let Some(progress) = &status.progress {
                    println!("  progress  {}", s.dim(&progress.summary()));
                }
            }
            None => println!("  {}", s.dim("idle")),
        }
        if status.waiting > 0 {
            println!("  {} waiting", status.waiting);
        }
    }
    0
}

fn env_cmd(dirs: &Dirs) -> i32 {
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let Some((probe, _)) = Probe::cached(dirs, None, &cwd) else {
        eprintln!("frust: could not query the toolchain");
        return 1;
    };
    if let Some(wrapper) = env::current_exe().ok().map(|e| e.with_file_name("frust-wrap")).filter(|p| p.exists()) {
        println!("export RUSTC_WRAPPER='{}'", wrapper.display());
    }
    println!("export FRUST_RUSTC_ID='{}'", probe.id);
    let (root, _) = workspace::locate_cached(dirs, &cwd, None, None, &probe.id);
    println!("export CARGO_TARGET_DIR='{}'", workspace::central_target(dirs, root.as_deref(), &probe.id).display());
    0
}

/// Answers from the startup memos like a build; `--fresh` probes and locates from scratch instead.
fn where_cmd(dirs: &Dirs, flags: &[String]) -> i32 {
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let fresh = flags.iter().any(|f| f == "--fresh");
    let probe = || -> Option<Probe> { if fresh { Probe::run(None, &cwd) } else { Probe::cached(dirs, None, &cwd).map(|(p, _)| p) } };
    let locate = |id: &str| -> Option<PathBuf> {
        if fresh { workspace::locate(&cwd, None, None) } else { workspace::locate_cached(dirs, &cwd, None, None, id).0 }
    };
    match flags.iter().find(|f| !f.starts_with("--")).map(String::as_str) {
        Some("cache") => println!("{}", dirs.root().display()),
        Some("root") => {
            let root = if fresh {
                workspace::locate(&cwd, None, None)
            } else {
                probe().and_then(|p| locate(&p.id)).or_else(|| workspace::locate(&cwd, None, None))
            };
            println!("{}", root.unwrap_or(cwd.clone()).display());
        }
        _ => {
            let Some(probe) = probe() else {
                eprintln!("frust: could not query the toolchain");
                return 1;
            };
            let root = locate(&probe.id);
            println!("{}", workspace::central_target(dirs, root.as_deref(), &probe.id).display());
        }
    }
    0
}

fn ui_demo(flags: &[String]) -> i32 {
    let style = if flags.iter().any(|f| f == "--plain") { Style::Plain } else { Style::Ansi };
    let width = flag_value(flags, "--width").and_then(|v| v.parse().ok()).unwrap_or(80);
    if flags.iter().any(|f| f == "--snapshot") {
        println!("{}", ui::demo::render_snapshot(style, width));
    } else {
        play_demo(style, width);
    }
    0
}

fn link_cmd(flags: &[String]) -> i32 {
    let dir = flag_value(flags, "--dir")
        .map(PathBuf::from)
        .or_else(|| env::current_exe().ok().and_then(|e| e.parent().map(PathBuf::from)))
        .unwrap_or_else(|| PathBuf::from("."));
    let Ok(exe) = env::current_exe() else {
        eprintln!("frust: cannot locate the frust binary");
        return 1;
    };
    let mut skip = false;
    let mut code = 0;
    for (i, tool) in flags.iter().enumerate() {
        if skip {
            skip = false;
            continue;
        }
        if tool == "--dir" {
            skip = true;
            continue;
        }
        if resolve_tool(tool).is_none() {
            eprintln!("frust: `{tool}` is not on PATH; skipping");
            code = 1;
            continue;
        }
        let link = dir.join(format!("f{tool}{}", env::consts::EXE_SUFFIX));
        if link.exists() {
            println!("exists   {}", link.display());
            continue;
        }
        match crate::sys::link_tool(&exe, &link) {
            Ok(()) => println!("created  {} -> {}", link.display(), exe.display()),
            Err(err) => {
                eprintln!("frust: cannot create {}: {err}", link.display());
                code = 1;
            }
        }
        let _ = i;
    }
    code
}

/// Run the scripted session against the real terminal, sleeping between steps.
fn play_demo(style: Style, width: u16) {
    let start = std::time::Instant::now();
    let mut ui = ui::Ui::new(ui::demo::demo_options(style, width, crate::config::Config::from_env().osc), Box::new(std::io::stdout()), start);
    let end = ui::demo::scripted_session_paced(&mut ui, start, &mut |gap| {
        std::thread::sleep(gap.min(Duration::from_millis(400)));
    });
    ui.finish(ui::Outcome { success: true, exit_code: 0 }, end);
}
