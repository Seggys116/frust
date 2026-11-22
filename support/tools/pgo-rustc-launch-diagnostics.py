#!/usr/bin/env python3
"""Native diagnostics for an instrumented rustc on macOS.

Usage: pgo-rustc-launch-diagnostics.py launch|probe ...
  launch  What a failed launch of the instrumented stage-2 rustc leaves behind: a native lldb backtrace,
          the profile section layout of the binaries, the crash report of a fresh process, and an
          environment and profile-file differential. It records evidence and always exits 0, so it
          never softens a launch gate of the caller.
  probe   Builds small rlib, dylib and executable shapes with an official Rust and the instrumented
          flags of stage 2, runs each variant and checks its exit status and emitted raw profiles. It
          writes its artifacts even when a variant fails, and exits 1 when any variant does.
Run with -h after a command for its arguments.
"""

import argparse
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import time
import traceback
from pathlib import Path

ANSI = re.compile(r"\x1b\[[0-9;]*m")
PROFILE_MAGICS = {(0xFF6C70726F667281).to_bytes(8, "little"), (0xFF6C70726F667381).to_bytes(8, "little")}
MACHO64 = bytes.fromhex("cffaedfe")
REPORT_DIRS = (Path.home() / "Library/Logs/DiagnosticReports", Path("/Library/Logs/DiagnosticReports"))
DEPLOYMENT = {"aarch64-apple-darwin": "11.0", "x86_64-apple-darwin": "10.12"}
LAUNCH_ENV = ("DYLD_LIBRARY_PATH", "MACOSX_DEPLOYMENT_TARGET", "MACOSX_STD_DEPLOYMENT_TARGET")

# Instrumented stage-2 flags in groups, taken from the Running lines of the instrumented-build.log of
# Benchmark run 44940641452 (21 November 2026, rust 0ef1bc386059, aarch64-apple-darwin) for rustc_driver
# and rustc_main. The launch command writes the commands of its own build log beside these to show drift.
FLAG_GROUPS = {
    "base": ["-Copt-level=3", "-Ccodegen-units=1", "-Zunstable-options", "--remap-path-scope=all",
             "-Zannotate-moves", "-Zmacro-backtrace", "-Alinker-messages", "-Zon-broken-pipe=kill"],
    "link": ["-Csplit-debuginfo=unpacked", "-Zosx-rpath-install-name", "-Clink-args=-Wl,-rpath,@loader_path/../lib"],
    "lto": ["-Zdylib-lto", "-Clto=thin", "-Cembed-bitcode=yes"],
    "gen": ["-Cprofile-generate={profile_dir}"],
    "vp": ["-Cllvm-args=-vp-counters-per-site=4"],
    "strip": ["-Cllvm-args=-static-func-strip-dirname-prefix=7"],
}
ALL_GROUPS = tuple(FLAG_GROUPS)
VARIANTS = (
    ("control", ("base", "link"), "small"),
    ("gen", ("base", "link", "gen"), "small"),
    ("gen-vp", ("base", "link", "gen", "vp"), "small"),
    ("gen-vp-strip", ("base", "link", "gen", "vp", "strip"), "small"),
    ("thin-lto", ("base", "link", "lto"), "small"),
    ("gen-thin", ("base", "link", "lto", "gen"), "small"),
    ("gen-vp-thin", ("base", "link", "lto", "gen", "vp"), "small"),
    ("stage2-actual", ALL_GROUPS, "small"),
    ("stage2-actual-no-link", tuple(g for g in ALL_GROUPS if g != "link"), "small"),
    ("control-large", ("base", "link"), "large"),
    ("stage2-actual-large", ALL_GROUPS, "large"),
)
SCALES = {"small": 300, "large": 12000}

CORE_SOURCE = """\
use std::sync::atomic::{AtomicU64, Ordering};

pub trait Shape { fn weight(&self, x: u64) -> u64; }
pub struct Sq(pub u64);
pub struct Tri(pub u64);
pub struct Ring(pub u64);
impl Shape for Sq { fn weight(&self, x: u64) -> u64 { x.wrapping_mul(self.0) ^ (x >> 3) } }
impl Shape for Tri { fn weight(&self, x: u64) -> u64 { if x & 1 == 0 { x / 2 + self.0 } else { x * 3 + 1 } } }
impl Shape for Ring { fn weight(&self, x: u64) -> u64 { x.rotate_left(self.0 as u32 & 31) } }

pub static COUNT: AtomicU64 = AtomicU64::new(0);

#[inline(never)]
pub fn mix(mut x: u64, rounds: u32) -> u64 {
    for i in 0..rounds {
        x = x.rotate_left(7) ^ x.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(u64::from(i));
        if x & 3 == 0 { x ^= x >> 11; }
    }
    COUNT.fetch_add(1, Ordering::Relaxed);
    x
}

// Indirect calls and variable-size copies are the sites that value profiling counts.
#[inline(never)]
pub fn dispatch(shapes: &[Box<dyn Shape>], mut x: u64) -> u64 {
    let mut buffer = vec![0u8; 256];
    for round in 0..64usize {
        x = shapes[round % shapes.len()].weight(x);
        let len = 1 + (x as usize % 255);
        let source = vec![(x & 0xFF) as u8; len];
        buffer[..len].copy_from_slice(&source);
        x ^= u64::from(buffer[len / 2]);
    }
    x
}
"""
GENERATED_HEAD = "pub fn run_all(mut x: u64) -> u64 {\n"
DYLIB_SOURCE = """\
use probe_core::{dispatch, mix, run_all, Ring, Shape, Sq, Tri};

#[inline(never)]
pub fn dylib_entry(seed: u64) -> u64 {
    let shapes: Vec<Box<dyn Shape>> = vec![Box::new(Sq(7)), Box::new(Tri(3)), Box::new(Ring(13))];
    let mut x = mix(seed, 1000);
    x = dispatch(&shapes, x);
    run_all(x)
}

pub fn calls() -> u64 {
    probe_core::COUNT.load(std::sync::atomic::Ordering::Relaxed)
}
"""
MAIN_SOURCE = """\
fn main() {
    let value = probe_dylib::dylib_entry(42);
    println!("probe {value:016x} {}", probe_dylib::calls());
}
"""


def text(data):
    return data.decode(errors="replace") if isinstance(data, bytes) else data


def clip(value, head=40, tail=40):
    lines = value.splitlines()
    if len(lines) <= head + tail:
        return value
    return "\n".join(lines[:head] + [f"... {len(lines) - head - tail} lines omitted ..."] + lines[-tail:])


def status_of(returncode, timed_out=False):
    if timed_out:
        return "timeout"
    if returncode is None:
        return "not run"
    if returncode < 0:
        try:
            return f"signal {signal.Signals(-returncode).name}"
        except ValueError:
            return f"signal {-returncode}"
    return f"exit {returncode}"


def run(argv, env=None, cwd=None, timeout=60):
    started = time.monotonic()
    try:
        proc = subprocess.Popen([str(a) for a in argv], env=env, cwd=cwd, stdin=subprocess.DEVNULL,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
    except OSError as error:
        return {"argv": [str(a) for a in argv], "returncode": None, "status": f"not run: {error}", "stdout": "",
                "stderr": "", "seconds": 0.0, "ok": False}
    timed_out = False
    try:
        out, err = proc.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        timed_out = True
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        out, err = proc.communicate()
    code = proc.returncode
    return {"argv": [str(a) for a in argv], "returncode": code, "status": status_of(code, timed_out),
            "stdout": text(out), "stderr": text(err), "seconds": round(time.monotonic() - started, 2),
            "ok": code == 0 and not timed_out}


def write(path, content):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content if isinstance(content, str) else "\n".join(content) + "\n")
    return path


def guarded(notes, name, function, *args):
    # One failed collector must not stop the others.
    try:
        return function(*args)
    except Exception:  # noqa: BLE001
        notes.append(f"{name} raised:\n{traceback.format_exc()}")
        return None


def listing(path, limit=60):
    path = Path(path)
    if not path.exists():
        return [f"{path}: does not exist"]
    entries = [path] if path.is_file() else sorted(path.rglob("*"))
    rows = [f"{p}  {p.stat().st_size} bytes" for p in entries if p.is_file()]
    return rows[:limit] + ([f"... {len(rows) - limit} more"] if len(rows) > limit else []) or [f"{path}: empty"]


# ---- Mach-O profile sections --------------------------------------------------------------------------------

SEGMENT_KEYS = {"segname", "vmaddr", "vmsize", "fileoff", "filesize", "maxprot", "initprot", "flags"}
SECTION_KEYS = {"sectname", "segname", "addr", "size", "offset", "align", "flags"}


def parse_load_commands(output):
    segments, current, section = [], None, None
    for raw in output.splitlines():
        line = raw.strip()
        if line.startswith("Load command"):
            current = section = None
        elif line in ("cmd LC_SEGMENT_64", "cmd LC_SEGMENT"):
            current, section = {"sections": []}, None
            segments.append(current)
        elif line == "Section" and current is not None:
            section = {}
            current["sections"].append(section)
        elif current is not None and " " in line:
            key, value = line.split(None, 1)
            if section is not None and key in SECTION_KEYS:
                section[key] = value
            elif section is None and key in SEGMENT_KEYS:
                current[key] = value
    return segments


def number(value, default=0):
    try:
        return int(value, 0)
    except (TypeError, ValueError):
        return default


def profile_sections(path):
    """Segment and __llvm_* section layout with file-extent checks and the first bytes of each profile section."""
    path = Path(path)
    lines = [f"## {path.name}", ""]
    proc = run(["otool", "-l", path], timeout=120)
    if not proc["ok"]:
        return lines + [f"otool -l failed: {proc['status']} {proc['stderr'].strip()}"]
    file_size = path.stat().st_size
    with path.open("rb") as handle:
        thin = handle.read(4) == MACHO64
        lines.append(f"file bytes {file_size}; thin 64-bit Mach-O {thin}")
        for segment in parse_load_commands(proc["stdout"]):
            end = number(segment.get("fileoff")) + number(segment.get("filesize"))
            flag = "  PAST END OF FILE" if end > file_size else ""
            lines.append(f"segment {segment.get('segname')} vmaddr {segment.get('vmaddr')} vmsize {number(segment.get('vmsize'))} "
                         f"fileoff {segment.get('fileoff')} filesize {segment.get('filesize')} prot "
                         f"{segment.get('initprot')}/{segment.get('maxprot')}{flag}")
            for section in segment["sections"]:
                name = section.get("sectname", "")
                if not name.startswith("__llvm"):
                    continue
                size, offset = number(section.get("size")), number(section.get("offset"))
                zerofill = number(section.get("flags")) & 0xFF in (1, 12, 18)
                flag = "  PAST END OF FILE" if offset and offset + size > file_size else ""
                lines.append(f"  section {name} size {size} ({size / 1024:.1f} KiB) offset {offset} align {section.get('align')} "
                             f"flags {section.get('flags')}{' zerofill' if zerofill else ''}{flag}")
                if thin and offset and not zerofill and offset + size <= file_size:
                    handle.seek(offset)
                    data = handle.read(min(size, 1 << 20))
                    lines.append(f"    first 48 bytes {data[:48].hex()}; nonzero bytes in the first "
                                 f"{len(data)}: {sum(1 for b in data if b)}")
    return lines


def profile_section_kib(path):
    """Sizes of the profile sections in KiB, for a one-line view."""
    proc = run(["otool", "-l", path], timeout=120)
    sizes = {}
    for segment in parse_load_commands(proc["stdout"]):
        for section in segment["sections"]:
            name = section.get("sectname", "")
            if name.startswith("__llvm_prf"):
                sizes[name.removeprefix("__llvm_prf_")] = number(section.get("size")) // 1024
    return sizes


# ---- crash reports and lldb ---------------------------------------------------------------------------------

def new_reports(since):
    return [p for d in REPORT_DIRS if d.is_dir() for p in sorted(d.iterdir())
            if p.suffix in (".ips", ".crash") and p.stat().st_mtime >= since]


def wait_for_reports(since, wanted, seconds):
    deadline = time.monotonic() + seconds
    while True:
        found = new_reports(since)
        if any(wanted in p.name for p in found) or time.monotonic() >= deadline:
            return found
        time.sleep(2)


def summarize_report(path):
    content = Path(path).read_text(errors="replace")
    lines = [f"report {Path(path).name} ({len(content)} bytes)"]
    try:
        document = json.loads(content.partition("\n")[2])
    except ValueError:
        crashed = next((i for i, line in enumerate(content.splitlines()) if re.match(r"Thread \d+ Crashed", line)), None)
        keep = [line for line in content.splitlines() if re.match(r"(Exception|Termination|Crashed Thread|Triggered)", line)]
        return lines + keep + (content.splitlines()[crashed:crashed + 30] if crashed is not None else [])
    for key in ("procName", "pid", "exception", "termination", "vmRegionInfo", "asi", "faultingThread"):
        if key in document:
            lines.append(f"{key}: {json.dumps(document[key])}")
    images = document.get("usedImages", [])
    threads = document.get("threads", [])
    index = document.get("faultingThread")
    if isinstance(index, int) and index < len(threads):
        lines.append("faulting thread frames:")
        for frame in threads[index].get("frames", [])[:30]:
            image = images[frame["imageIndex"]] if frame.get("imageIndex", -1) < len(images) else {}
            lines.append(f"  {image.get('name', '?')} +{frame.get('imageOffset')} {frame.get('symbol', '')} "
                         f"+{frame.get('symbolLocation', '')}")
    return lines


def collect_reports(since, out, name, wait):
    found = wait_for_reports(since, name, wait)
    lines = [f"{len(found)} crash report(s) newer than the launch: {', '.join(p.name for p in found) or 'none'}"]
    for path in found:
        if path.stat().st_size <= 8 << 20:
            shutil.copy2(path, Path(out) / path.name)
        lines += summarize_report(path)
    return lines


def lldb_backtrace(argv, env_pairs, out, timeout):
    lldb = shutil.which("lldb")
    if not lldb:
        return ["lldb is not installed"]
    commands = ["settings set target.env-vars " + " ".join(f"{k}={v}" for k, v in env_pairs),
                "settings set target.disable-aslr false", "run", "process status", "thread info", "thread backtrace all",
                "frame info", "register read", "disassemble --pc --count 16", "image lookup --address $pc", "image list",
                "process kill"]
    command = [lldb, "--batch", "--no-lldbinit"] + [a for c in commands for a in ("-o", c)] + ["--", *argv]
    result = run(command, env={k: v for k, v in os.environ.items() if not k.startswith("DYLD_")}, timeout=timeout)
    write(Path(out) / "lldb.txt", [f"{result['status']} after {result['seconds']} s", "--- stdout ---",
                                   clip(result["stdout"], 400, 400), "--- stderr ---", clip(result["stderr"], 40, 40)])
    keep = [line.strip() for line in result["stdout"].splitlines()
            if "stop reason" in line or "frame #" in line or "exited with status" in line]
    return [f"lldb: {result['status']} after {result['seconds']} s, full output in lldb.txt"] + keep[:24]


# ---- launch ---------------------------------------------------------------------------------------------------

def command_flags(log):
    """The rustc_driver and rustc_main compiler commands of an instrumented build log, without path-only flags."""
    found = {}
    for line in Path(log).read_text(errors="replace").splitlines():
        line = ANSI.sub("", line)
        if "Running `" not in line or "-Cprofile-generate=" not in line:
            continue
        for crate in ("rustc_driver", "rustc_main"):
            if f"--crate-name {crate} " in line and crate not in found:
                tokens = re.findall(r"'[^']*'|\S+", line.split("Running `", 1)[1])
                kept, skip, join = [], False, ""
                for token in tokens:
                    if skip:
                        skip = False
                    elif token in ("--cfg", "--check-cfg", "--extern", "-L", "--remap-path-prefix"):
                        skip = True
                    elif token in ("-C", "-Z"):
                        join = token
                    elif not token.startswith("--remap-path-prefix="):
                        kept.append(join + token.strip("'"))
                        join = ""
                found[crate] = kept
    return found


def flag_drift(commands, profile_dir):
    known = {flag.format(profile_dir=profile_dir) for flags in FLAG_GROUPS.values() for flag in flags}
    # Flags the probe leaves out on purpose: they select files or metadata layout, not code generation.
    ignored = {"-Zembed-metadata=no", "-Cprefer-dynamic", "-Zbinary-dep-depinfo", "-Cembed-bitcode=no"}
    lines = []
    for crate, tokens in commands.items():
        extra = [t for t in tokens if re.match(r"^-[CZA]", t) and t not in known | ignored
                 and not t.startswith(("-Cmetadata", "-Cextra-filename"))]
        missing = sorted(known - set(tokens))
        lines.append(f"{crate}: -C/-Z/-A flags of the build log that the probe flag groups lack: {extra or 'none'}")
        lines.append(f"{crate}: probe flags absent from the build log command: {missing or 'none'}")
    return lines


def differential(rustc, base_env, out, profile_dir):
    work = Path(out) / "differential"
    work.mkdir(parents=True, exist_ok=True)
    with_dyld = dict(base_env)
    minimal = {k: base_env[k] for k in ("PATH", "HOME", "TMPDIR", "DYLD_LIBRARY_PATH", "RUSTC_BOOTSTRAP",
                                         "MACOSX_DEPLOYMENT_TARGET") if k in base_env}
    no_dyld = {k: v for k, v in base_env.items() if k != "DYLD_LIBRARY_PATH"}
    no_profile = {k: v for k, v in base_env.items() if k != "LLVM_PROFILE_FILE"}
    variants = [
        ("gate", ["-vV"], with_dyld, None),
        ("gate-repeat", ["-vV"], with_dyld, None),
        ("gate-second-command", ["--print=sysroot"], with_dyld, None),
        ("no-dyld-library-path", ["-vV"], no_dyld, None),
        ("minimal-environment", ["-vV"], minimal, None),
        ("cwd-output-directory", ["-vV"], with_dyld, str(work)),
        ("profile-file-dev-null", ["-vV"], {**no_profile, "LLVM_PROFILE_FILE": "/dev/null"}, None),
        ("profile-file-writable", ["-vV"], {**no_profile, "LLVM_PROFILE_FILE": f"{work}/writable/%m-%p.profraw"}, None),
        ("profile-file-continuous", ["-vV"], {**no_profile, "LLVM_PROFILE_FILE": f"{work}/continuous/%m%c.profraw"}, None),
        ("profile-file-missing-directory", ["-vV"], {**no_profile, "LLVM_PROFILE_FILE": f"{work}/absent/%p.profraw"}, None),
        ("dyld-print-libraries", ["-vV"], {**with_dyld, "DYLD_PRINT_LIBRARIES": "1"}, None),
        ("dyld-print-initializers", ["-vV"], {**with_dyld, "DYLD_PRINT_INITIALIZERS": "1"}, None),
    ]
    rows, results = [], []
    for name, args, env, cwd in variants:
        before = set(listing(profile_dir, 400))
        result = run([rustc, *args], env=env, cwd=cwd, timeout=60)
        gained = sorted(set(listing(profile_dir, 400)) - before)
        verbose = name.startswith("dyld-print")
        write(work / f"{name}.txt", [f"argv {result['argv']}", f"{result['status']} after {result['seconds']} s",
                                     "--- stdout ---", clip(result["stdout"], 20, 20), "--- stderr ---",
                                     clip(result["stderr"], 80 if verbose else 20, 80 if verbose else 20),
                                     "--- new files in the profile directory ---", *gained])
        rows.append(f"| {name} | {result['status']} | {result['seconds']} | {len(gained)} | {len(result['stdout'])} |")
        results.append({"variant": name, "status": result["status"], "new_profile_files": len(gained)})
    write(work / "results.json", json.dumps(results, indent=2))
    return ["| variant | result | seconds | new profile files | stdout bytes |", "| --- | --- | --- | --- | --- |", *rows]


def snapshot(rustc, host, base_env, out, profile_dir):
    lib = Path(rustc).resolve().parent.parent / "lib"
    lines = [f"host {host}", f"rustc {rustc}", ""]
    for argv in (["sw_vers"], ["uname", "-a"], ["sysctl", "machdep.cpu.brand_string", "hw.memsize", "kern.osversion"],
                 ["sh", "-c", "ulimit -a"], ["file", rustc], ["lipo", "-info", rustc], ["otool", "-hv", rustc],
                 ["codesign", "-dvvv", "--entitlements", "-", rustc], ["xattr", "-l", rustc],
                 ["df", "-h", profile_dir if Path(profile_dir).exists() else Path(profile_dir).parent]):
        result = run(argv, timeout=60)
        lines += [f"$ {' '.join(map(str, argv))}  [{result['status']}]", clip(result["stdout"] + result["stderr"], 30, 10), ""]
    lines += ["", "relevant environment of the gate launch:"]
    lines += [f"  {key}={base_env[key]}" for key in LAUNCH_ENV]
    lines += ["", f"profile directory {profile_dir}:", *listing(profile_dir), "", f"driver libraries in {lib}:"]
    lines += listing(lib, 20)
    write(Path(out) / "environment.txt", lines)


def launch(args):
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    rustc = str(Path(args.rustc))
    notes = []
    since = time.time() - 2
    base_env = dict(os.environ, RUSTC_BOOTSTRAP="1", DYLD_LIBRARY_PATH=args.lib_path,
                    MACOSX_DEPLOYMENT_TARGET=args.deployment, MACOSX_STD_DEPLOYMENT_TARGET=args.deployment)
    summary = [f"### Instrumented launch diagnostics, {args.host}", ""]
    guarded(notes, "snapshot", snapshot, rustc, args.host, base_env, out, args.profile_dir)
    driver = sorted(Path(rustc).resolve().parent.parent.glob("lib/librustc_driver-*.dylib"))
    for binary in [Path(rustc), *driver]:
        sections = guarded(notes, f"sections {binary.name}", profile_sections, binary)
        if sections:
            write(out / f"profile-sections-{binary.name}.txt", sections)
            summary += ["```", *[l for l in sections if "PAST END" in l or l.startswith(("##", "file bytes"))
                                 or "__llvm_prf_cnts" in l or "__llvm_prf_vnds" in l], "```"]
    if args.build_log and Path(args.build_log).exists():
        commands = guarded(notes, "build log commands", command_flags, args.build_log) or {}
        write(out / "instrumented-commands.txt", [f"{crate}: {' '.join(tokens)}" for crate, tokens in commands.items()]
              + flag_drift(commands, args.profile_dir))
    table = guarded(notes, "differential", differential, rustc, base_env, out, args.profile_dir)
    if table:
        summary += ["", "Differential of fresh launches (each `rustc` run is a new process):", "", *table]
    env_pairs = [(k, base_env[k]) for k in ("RUSTC_BOOTSTRAP", "DYLD_LIBRARY_PATH", "MACOSX_DEPLOYMENT_TARGET",
                                            "MACOSX_STD_DEPLOYMENT_TARGET")]
    backtrace = guarded(notes, "lldb", lldb_backtrace, [rustc, "-vV"], env_pairs, out, args.lldb_timeout)
    if backtrace:
        summary += ["", "Native backtrace:", "```", *backtrace, "```"]
    crash = guarded(notes, "crash reports", collect_reports, since, out, "rustc", args.report_wait)
    if crash:
        write(out / "crash-reports.txt", crash)
        summary += ["", "Crash reports of the launches above:", "```", *crash[:40], "```"]
    log = run(["log", "show", "--last", "10m", "--style", "compact", "--predicate",
               'process == "ReportCrash" OR process == "amfid" OR process == "syspolicyd" OR eventMessage CONTAINS "rustc"'],
              timeout=90)
    write(out / "unified-log.txt", [f"{log['status']}", clip(log["stdout"], 150, 150), clip(log["stderr"], 10, 10)])
    write(out / "collector-errors.txt", notes or ["none"])
    if notes:
        summary += ["", f"{len(notes)} collector(s) raised; see collector-errors.txt"]
    write(out / "summary.md", summary)
    print("\n".join(summary))
    return 0


# ---- probe ----------------------------------------------------------------------------------------------------

def flags_for(groups, kind, profile_dir):
    flags = [flag.format(profile_dir=profile_dir) for group in ALL_GROUPS if group in groups for flag in FLAG_GROUPS[group]]
    if "lto" not in groups:
        flags.append("-Cembed-bitcode=no")
    if kind == "dylib":
        flags.append("-Cprefer-dynamic")
    return flags


def generated_source(count):
    body = "".join(
        f"    x = f{i}(x);\n" for i in range(count)
    )
    functions = "".join(
        f"#[inline(never)]\npub fn f{i}(x: u64) -> u64 {{ if x % {3 + i % 11} == 0 {{ x.rotate_left({1 + i % 31}) ^ {i}u64 }} "
        f"else if x & {1 + i % 7} != 0 {{ x.wrapping_add({i * 2654435761 % 1000003}u64) }} else {{ x ^ (x >> {1 + i % 13}) }} }}\n"
        for i in range(count)
    )
    return functions + GENERATED_HEAD + body + "    x\n}\n"


def rustc_command(rustc, name, kind, flags, source, output, lib_dir, extern_args, host, extra=()):
    return [rustc, "--crate-name", name, "--edition=2024", source, "--crate-type", kind, "--emit=link", "--target", host,
            "-Cmetadata=probe", "-Cextra-filename=-probe", *output, "-L", f"dependency={lib_dir}",
            *extern_args, *flags, *extra]


def raw_profile_facts(directory, profdata, work):
    files = sorted(Path(directory).rglob("*.profraw")) if Path(directory).exists() else []
    facts = {"files": len(files), "bytes": sum(f.stat().st_size for f in files), "valid_magic": 0, "functions": None,
             "merge": "not run"}
    for path in files:
        with path.open("rb") as handle:
            facts["valid_magic"] += handle.read(8) in PROFILE_MAGICS
    if files and profdata:
        merged = Path(work) / f"{Path(directory).name}.profdata"
        merge = run([profdata, "merge", "-o", merged, *files], timeout=120)
        facts["merge"] = merge["status"] if merge["ok"] else f"{merge['status']}: {merge['stderr'].strip()[:200]}"
        if merge["ok"]:
            show = run([profdata, "show", merged], timeout=120)
            found = re.search(r"Total functions:\s*(\d+)", show["stdout"])
            facts["functions"] = int(found.group(1)) if found else None
    return facts


def probe_variant(context, name, groups, scale):
    root = context["work"] / name
    shutil.rmtree(root, ignore_errors=True)
    for part in ("lib", "bin", "src", "logs"):
        (root / part).mkdir(parents=True)
    profile_dir = root / "raw"
    host, rustc = context["host"], context["rustc"]
    env = dict(os.environ, RUSTC_BOOTSTRAP="1", MACOSX_DEPLOYMENT_TARGET=context["deployment"],
               MACOSX_STD_DEPLOYMENT_TARGET=context["deployment"])
    env.pop("LLVM_PROFILE_FILE", None)
    result = {"variant": name, "groups": list(groups), "scale": scale, "failures": []}
    sources = {"core": CORE_SOURCE + generated_source(SCALES[scale]), "dylib": DYLIB_SOURCE, "main": MAIN_SOURCE}
    for key, content in sources.items():
        write(root / "src" / f"{key}.rs", content)
    lib, src, bin_dir = root / "lib", root / "src", root / "bin"
    steps = [("core", "probe_core", "lib", []),
             ("dylib", "probe_dylib", "dylib", [("probe_core", lib / "libprobe_core-probe.rlib")]),
             ("main", "probe_main", "bin", [("probe_dylib", lib / "libprobe_dylib-probe.dylib")])]
    for key, crate, kind, externs in steps:
        extern_args = [a for n, p in externs for a in ("--extern", f"{n}={p}")]
        output = ["-o", str(bin_dir / "probe_main")] if kind == "bin" else ["--out-dir", str(lib)]
        # A bin that links a prefer-dynamic dylib may need prefer-dynamic itself; the retry is recorded.
        attempts = [()] if kind != "bin" else [(), ("-Cprefer-dynamic",)]
        for attempt in attempts:
            command = rustc_command(rustc, crate, kind, flags_for(groups, kind, profile_dir), src / f"{key}.rs", output,
                                    lib, extern_args, host, attempt)
            compiled = run(command, env=env, timeout=context["compile_timeout"])
            write(root / "logs" / f"compile-{key}{'-prefer-dynamic' if attempt else ''}.log",
                  [f"{' '.join(compiled['argv'])}", f"{compiled['status']} after {compiled['seconds']} s",
                   clip(compiled["stderr"], 150, 150)])
            result[f"compile_{key}"] = compiled["status"]
            result[f"compile_{key}_seconds"] = compiled["seconds"]
            if attempt:
                result["exe_prefer_dynamic"] = True
            if compiled["ok"]:
                break
        if not compiled["ok"]:
            result["failures"].append(f"{key} compile: {compiled['status']}")
            return result, root
    exe = bin_dir / "probe_main"
    dylibs = sorted(lib.glob("*.dylib"))
    dyld = ":".join([str(lib), context["sysroot_lib"]])
    runs = {"default": dict(os.environ, DYLD_LIBRARY_PATH=dyld),
            "explicit-profile-file": dict(os.environ, DYLD_LIBRARY_PATH=dyld,
                                          LLVM_PROFILE_FILE=f"{root}/raw-explicit/%m-%p.profraw")}
    runs["default"].pop("LLVM_PROFILE_FILE", None)
    instrumented = "gen" in groups
    for label, run_env in runs.items():
        first = run([exe], env=run_env, timeout=60)
        write(root / "logs" / f"run-{label}.log", [first["status"], "--- stdout ---", first["stdout"], "--- stderr ---",
                                                    clip(first["stderr"], 40, 40)])
        output = first["stdout"].strip()
        result[f"run_{label}"] = first["status"]
        result[f"output_{label}"] = output
        if not first["ok"]:
            result["failures"].append(f"run {label}: {first['status']}")
            result.setdefault("failed_run", {"label": label, "env": {k: run_env[k] for k in ("DYLD_LIBRARY_PATH",)
                                                                       if k in run_env}})
        elif scale in context["expected"] and output != context["expected"][scale]:
            result["failures"].append(f"run {label}: output {output!r} differs from the control {context['expected'][scale]!r}")
        elif name.startswith("control"):
            context["expected"].setdefault(scale, output)
        directory = profile_dir if label == "default" else root / "raw-explicit"
        facts = raw_profile_facts(directory, context["profdata"], root)
        result[f"raw_{label}"] = facts
        if instrumented and first["ok"]:
            if not facts["files"]:
                result["failures"].append(f"run {label}: no raw profile was written")
            elif facts["valid_magic"] != facts["files"]:
                result["failures"].append(f"run {label}: {facts['files'] - facts['valid_magic']} raw file(s) without the profile magic")
            elif context["profdata"] and not facts["functions"]:
                result["failures"].append(f"run {label}: merged profile has no functions ({facts['merge']})")
        elif not instrumented and facts["files"]:
            result["failures"].append(f"run {label}: an uninstrumented variant wrote {facts['files']} raw file(s)")
    result["sections_kib"] = {p.name: profile_section_kib(p) for p in [exe, *dylibs]}
    return result, root


def capture_failure(context, result, root):
    out = context["out"] / "failures" / result["variant"]
    out.mkdir(parents=True, exist_ok=True)
    exe = root / "bin" / "probe_main"
    lib = root / "lib"
    since = time.time() - 2
    dyld = ":".join([str(lib), context["sysroot_lib"]])
    notes = []
    fresh = run([exe], env=dict(os.environ, DYLD_LIBRARY_PATH=dyld), timeout=60)
    lines = [f"fresh launch: {fresh['status']}"]
    lines += guarded(notes, "lldb", lldb_backtrace, [str(exe)], [("DYLD_LIBRARY_PATH", dyld)], out, 90) or []
    lines += guarded(notes, "crash reports", collect_reports, since, out, "probe_main", 15) or []
    for binary in [exe, *sorted(lib.glob("*.dylib"))]:
        sections = guarded(notes, f"sections {binary.name}", profile_sections, binary)
        if sections:
            write(out / f"profile-sections-{binary.name}.txt", sections)
        if binary.stat().st_size <= 40 << 20:
            shutil.copy2(binary, out / binary.name)
    write(out / "capture.txt", lines + notes)
    return lines[:8]


def probe(args):
    if sys.platform != "darwin":
        sys.exit("the instrumented-rustc probe runs on macOS only")
    out, work = Path(args.out), Path(args.work)
    out.mkdir(parents=True, exist_ok=True)
    work.mkdir(parents=True, exist_ok=True)
    version = run([args.rustc, "-vV"], timeout=30)
    release = re.search(r"^release:\s*(\S+)", version["stdout"], re.M)
    host = re.search(r"^host:\s*(\S+)", version["stdout"], re.M)
    if not (release and host) or release.group(1) != args.release:
        sys.exit(f"expected official Rust {args.release}, found {release.group(1) if release else version['status']}")
    host = host.group(1)
    sysroot = run([args.rustc, "--print", "sysroot"], timeout=30)["stdout"].strip()
    profdata = Path(sysroot) / "lib/rustlib" / host / "bin/llvm-profdata"
    context = {"rustc": args.rustc, "host": host, "deployment": DEPLOYMENT.get(host, "11.0"), "work": work, "out": out,
               "sysroot_lib": f"{sysroot}/lib/rustlib/{host}/lib", "profdata": str(profdata) if profdata.exists() else None,
               "expected": {}, "compile_timeout": args.compile_timeout}
    wanted = set(args.variants.split(",")) if args.variants else None
    deadline = time.monotonic() + args.budget_minutes * 60
    results, detailed = [], 0
    header = [f"### Instrumented rustc probe, {host}", "", f"Official Rust {args.release}; llvm-profdata "
              f"{'available' if context['profdata'] else 'missing, merged profiles are not checked'}; stage-2 flag groups "
              "are those of Benchmark run 44940641452 (see FLAG_GROUPS).", ""]
    for name, groups, scale in VARIANTS:
        if wanted and name not in wanted:
            continue
        if time.monotonic() > deadline:
            results.append({"variant": name, "scale": scale, "failures": ["not run: probe time budget reached"]})
            continue
        try:
            result, root = probe_variant(context, name, groups, scale)
        except Exception:  # noqa: BLE001
            result, root = {"variant": name, "scale": scale, "failures": [f"probe raised: {traceback.format_exc()[-400:]}"]}, None
        if result["failures"] and root is not None and "failed_run" in result and detailed < args.detail_limit:
            detailed += 1
            result["capture"] = capture_failure(context, result, root)
        results.append(result)
        print(f"{name}: {'FAILED ' + '; '.join(result['failures']) if result['failures'] else 'ok'}", flush=True)
        if root is not None:
            shutil.copytree(root / "logs", out / "variants" / name / "logs", dirs_exist_ok=True)
            if name == "stage2-actual":
                shutil.copytree(root / "src", out / "variants" / name / "src", dirs_exist_ok=True)
            shutil.rmtree(root, ignore_errors=True)
    rows = ["| variant | scale | compile | default run | explicit run | raw files | merged functions | sections KiB | failures |",
            "| --- | --- | --- | --- | --- | --- | --- | --- | --- |"]
    for r in results:
        compiles = "/".join(r.get(f"compile_{k}", "-").replace("exit ", "") for k in ("core", "dylib", "main"))
        raw = r.get("raw_default", {})
        sizes = "; ".join(f"{n}: " + ",".join(f"{k}={v}" for k, v in s.items() if k in ("cnts", "data", "vnds", "vnodes"))
                          for n, s in r.get("sections_kib", {}).items())
        rows.append(f"| {r['variant']} | {r.get('scale', '')} | {compiles} | {r.get('run_default', '-')} | "
                    f"{r.get('run_explicit-profile-file', '-')} | {raw.get('files', '-')} | {raw.get('functions', '-')} | "
                    f"{sizes or '-'} | {'; '.join(r['failures']) or 'none'} |")
    failed = [r["variant"] for r in results if r["failures"]]
    verdict = [f"{len(failed)} of {len(results)} variant(s) failed: {', '.join(failed)}" if failed
               else f"All {len(results)} variants passed."]
    write(out / "summary.md", header + rows + ["", *verdict])
    write(out / "results.json", json.dumps(results, indent=2, default=str))
    print((out / "summary.md").read_text())
    return 1 if failed else 0


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    one = sub.add_parser("launch", help="diagnose a failed launch of the instrumented stage-2 rustc")
    one.add_argument("--rustc", required=True)
    one.add_argument("--host", required=True)
    one.add_argument("--lib-path", required=True, help="DYLD_LIBRARY_PATH of the launch gate")
    one.add_argument("--deployment", required=True)
    one.add_argument("--profile-dir", required=True, help="the -Cprofile-generate directory of the build")
    one.add_argument("--out", required=True)
    one.add_argument("--build-log", help="instrumented-build.log, for the compiler commands and the flag drift")
    one.add_argument("--lldb-timeout", type=int, default=180)
    one.add_argument("--report-wait", type=int, default=45, help="seconds to wait for a crash report")
    two = sub.add_parser("probe", help="build and run instrumented dylib and executable shapes with an official Rust")
    two.add_argument("--rustc", required=True, help="the official rustc")
    two.add_argument("--release", default="1.100.0")
    two.add_argument("--work", required=True)
    two.add_argument("--out", required=True)
    two.add_argument("--variants", help="comma separated variant names, default all")
    two.add_argument("--budget-minutes", type=int, default=20)
    two.add_argument("--compile-timeout", type=int, default=600)
    two.add_argument("--detail-limit", type=int, default=3, help="failing variants that get lldb and crash-report capture")
    args = parser.parse_args()
    return {"launch": launch, "probe": probe}[args.command](args)


if __name__ == "__main__":
    sys.exit(main())
