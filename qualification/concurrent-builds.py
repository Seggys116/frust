#!/usr/bin/env python3
"""Local benchmark of overlapping Cargo commands: two supplied cargo binaries, same fixtures, same paths.

This measures contention and sharing cost. cooperative-builds.py is the correctness proof for shared
compilation and stays the place to check behaviour. Here nothing is slept, staggered or released by a
marker: every command of a scenario is spawned at once and the commands meet at Cargo's own locks.

Sides: --a and --b are LABEL=CARGO[,RUSTC], for example fcargo and cargo. Both sides run the same
workspaces at the same absolute paths, never at the same time. Each (round, scenario, count, state)
runs the two sides back to back in an order that alternates with the round, so use an even --rounds.

Scenarios (--scenarios, with --count lanes each):
  same-workspace  count identical commands in one workspace and one target directory
  overlap         count commands in one workspace, each limited to a different member (-p) over shared units
  independent     count workspaces at distinct paths, each with its own target directory. The generated
                  fixture's root crate cb_dep_a is a vendored registry crate (a directory source with a
                  checksum in Cargo.lock), the one thing these workspaces can share through a unit cache;
                  cb_dep_b, the members and the shared path crate stay path dependencies
  mixed           count commands in one workspace cycling through --mix (default dev build, release build,
                  rust-analyzer style check)
  custom          the exact lanes given with --lane, which may name a workspace and a rustc per lane
--preset ci replaces --scenarios with a short fixed matrix, one cell per name and state for each --count:
  build-dev, build-release     independent workspaces
  build-shared                 identical dev builds in one workspace and target directory
  check-storm-dev              check and ra lanes alternating in one workspace
  check-storm-release          release checks in one workspace
It needs only what a minimal rustup profile ships, and its default --states is cold,warm,cache.
A lane is MODE[:PROFILE][,ws=K][,package=P][,rustc=PATH][,rustc-a=PATH][,rustc-b=PATH] with MODE one of
build check test clippy doc fmt ra, PROFILE dev or release. ra is what rust-analyzer runs on save:
check --workspace --all-targets with JSON diagnostics. fmt is always cargo fmt --check.

States (--states):
  cold    empty target directories and an empty per-side FRUST_HOME and XDG cache
  warm    target and cache populated by one unmeasured run of each distinct lane command
  cache   cache populated, target directories removed afterwards
  target  target directories populated, cache removed afterwards
  edit    warm, then one source file of the shared crate gets a new public function

Semantics of every reported number (also written to the JSON header as "labels"):
  cpu       ru_utime + ru_stime from wait4 for the command and the descendants it reaped. A command that
            waits on a peer's compilation is not charged for it, so judge a scenario by cpu_total, the sum
            over its commands. Processes that detach or outlive their parent are not counted. CPU is the
            only performance figure the summary compares between the sides.
  wall      orchestration timing only (spawn to wait4 return, scenario first spawn to last exit, launch
            skew), kept in the records for deadlines and diagnosis and never summarised or compared.
  max_rss   ru_maxrss from wait4: the largest resident set of any one reaped process of that command, not
            a sum. Scenario max_rss is the largest of the commands. Simultaneous peak memory is not measured.
  storage   allocated bytes (st_blocks * 512) of regular files and symlinks, each inode once, taken after
            the commands finish. target counts every target directory used, store counts FRUST_HOME and the
            XDG cache, total counts both with inodes shared between them once. Reflinked or cloned bytes
            are counted in full. total_delta is total after minus total before the measured commands. This
            is a footprint, never a count of bytes written.
  write     On Linux, wait4 ru_oublock times 512 records bytes charged to the command and its reaped
            descendants when pages are dirtied. It includes writes that may later be cancelled.
            On macOS this byte count is unavailable.
  locks     lock_waits counts every "Blocking waiting for file lock on X" line Cargo printed, in total and
            by X. No lock hold duration is recorded: Cargo prints only that a wait began and the unit cache
            reports no timing for its locks.
  cache     with FRUST_UNIT_CACHE_DEBUG=1 (pass it with --env-b) the counts of unit cache hit, miss, store
            and skip lines and the last cumulative "written" figure of its stats line, in MiB rounded by the
            cache to one decimal. cache_written_mib is a store counter and is not comparable with write_bytes.

Validation, deterministic and checked on every measured run: fixtures are generated from fixed text with
fixed modification times and compared by content digest before and after each run; every command must exit
0 within --timeout; the shared path and vendored crates must be unchanged and the generated lockfile must pin
the vendored crate by checksum; a built fixture program must print the value computed here; test counts,
documentation pages and rust-analyzer artifact sets must match what the fixture implies and be identical for
both sides; a warm or target-state lane that still prints Compiling or Checking is warned. The exit status is
1 when any validation fails.

Example:
  concurrent-builds.py --a fcargo=$(command -v fcargo) --b cargo=$(command -v cargo) --out run.jsonl \\
      --count 1,4 --rounds 4 --jobs 2 --states cold,warm

Short paired matrix with unit cache counters on the fRust side, as the Benchmark workflow runs it:
  concurrent-builds.py --a official=$CARGO,$RUSTC --b frust=$(command -v fcargo),$(command -v frustc) \\
      --preset ci --count 3 --rounds 2 --size 200 --env-b FRUST_UNIT_CACHE=1 --env-b FRUST_UNIT_CACHE_DEBUG=1 \\
      --out contention.ndjson
The results file ends with one summary record (kind "summary") holding the per-cell medians and ratios.
"""
from __future__ import annotations

import argparse
import errno
import fcntl
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import platform
import re
import runpy
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import threading
import time

sys.dont_write_bytecode = True

REPO = Path(__file__).resolve().parents[1]
PAIRED = runpy.run_path(Path(__file__).with_name("paired_stats.py"))
FIXED_MTIME = 1577836800
MODES = ("build", "check", "test", "clippy", "doc", "fmt")
LANE_MODES = MODES + ("ra",)
PROFILES = ("dev", "release")
STATES = ("cold", "warm", "cache", "target", "edit")
SCENARIOS = ("same-workspace", "overlap", "independent", "mixed", "custom")
DEFAULT_SCENARIOS = "same-workspace,overlap,independent,mixed"
# Preset cell: (name, layout, lane specs cycled over the lanes). An independent layout gives each lane its own
# workspace, a shared one puts every lane in workspace 0.
PRESETS = {
    "ci": (
        ("build-dev", "independent", ("build",)),
        ("build-release", "independent", ("build:release",)),
        ("build-shared", "shared", ("build",)),
        ("check-storm-dev", "shared", ("check", "ra")),
        ("check-storm-release", "shared", ("check:release",)),
        ("check-and-lint", "shared", ("check", "clippy", "ra")),
        ("test-doc-fmt", "shared", ("test", "doc", "fmt")),
    ),
}
VENDORED = {"name": "cb_dep_a", "version": "1.0.0"}
OVERLAP_PACKAGES = ("cb_left", "cb_right", "cb_app", "cb_core")
BUILTIN_TEST_COUNTS = {"cb_core": 1, "cb_left": 1, "cb_right": 1, "cb_app": 0}
DROP_PREFIXES = ("CARGO", "RUSTC", "RUSTFLAGS", "RUSTDOC", "FRUST", "XDG")
SKIP_DIRS = {"target", ".git"}
MUL = 6364136223846793005
SEED = 1442695040888963407
MASK = (1 << 64) - 1
EDIT_TEXT = "\n#[allow(dead_code)]\npub fn concurrent_builds_edit() -> u64 {\n    1\n}\n"
BUILTIN_EDIT_FILE = "cb_core/src/lib.rs"
COMPILING = re.compile(r"^\s*(?:Compiling|Checking) ", re.M)
LOCK_WAIT = re.compile(r"Blocking waiting for file lock on ([^\r\n]+)")
CACHE_EVENT = re.compile(r"frust-unit-cache: (script-)?(hit|miss|store|skip) ")
CACHE_WRITTEN = re.compile(r"frust-unit-cache: stats [^\r\n]*?\bwritten=([0-9.]+)")
# Metrics the summary reports per side, and those it also turns into paired ratios. Wall is in neither.
SUMMARY_METRICS = ("cpu_total", "max_rss_bytes", "total_bytes", "target_bytes", "store_bytes", "write_bytes",
                   "lock_waits_total", "cache_written_mib")
RATIO_METRICS = ("cpu_total", "max_rss_bytes", "total_bytes", "write_bytes", "lock_waits_total")
APFS_METRICS = ("apfs_write_bytes", "apfs_metadata_bytes", "apfs_footprint_bytes")
LABELS = {
    "cpu": "ru_utime+ru_stime from wait4 for the command and the descendants it reaped; waiting on a "
           "peer's compilation costs nothing here, so compare cpu_total; detached processes are not counted; "
           "the only performance figure the summary compares",
    "wall": "orchestration timing only: spawn to wait4 return per command, first spawn to last exit per "
            "scenario; never summarised or compared",
    "max_rss_bytes": "ru_maxrss from wait4: largest resident set of any one reaped process of the command, "
                     "not a sum; scenario value is the largest command; simultaneous peak is not measured",
    "storage": "allocated footprint, st_blocks*512 of files and symlinks, each inode once, after the commands; "
               "reflinks counted in full; total counts inodes shared by target and store once; total_delta is "
               "after minus before; not a count of bytes written",
    "write_bytes": "Linux only: 512 times wait4 ru_oublock for the command and reaped children. "
                   "The kernel charges bytes when pages are dirtied, including writes later cancelled. "
                   "Null on macOS. The scenario value sums the commands.",
    "oublock": "ru_oublock from wait4 summed like cpu; platform-dependent units, not bytes",
    "lock_waits": "count of Cargo 'Blocking waiting for file lock on X' lines, total and by X",
    "cache_events": "counts of frust-unit-cache hit, miss, store, skip lines (script- prefix kept) with "
                    "FRUST_UNIT_CACHE_DEBUG=1; null without it",
    "cache_written_mib": "last cumulative written= of the unit cache stats line, MiB rounded to 0.1 by the "
                         "cache, summed over commands; a store counter, not comparable with write_bytes",
}

ACTIVE = set()
ACTIVE_LOCK = threading.Lock()


class Lane:
    """One command of a scenario."""

    def __init__(self, mode, profile="dev", slot=0, package=None, rustc=None):
        self.mode, self.profile, self.slot, self.package = mode, profile, slot, package
        self.rustc = dict(rustc or {})

    def key(self):
        return (self.mode, self.profile, self.slot, self.package, tuple(sorted(self.rustc.items())))

    def describe(self):
        text = f"{self.mode}:{self.profile}@ws{self.slot}"
        return text + (f" -p {self.package}" if self.package else "")

    def moved(self, slot=None, package=None):
        return Lane(self.mode, self.profile, self.slot if slot is None else slot,
                    package or self.package, self.rustc)


class Cell:
    """One measured combination of scenario, lane count and state."""

    def __init__(self, scenario, count, state, lanes):
        self.scenario, self.count, self.state, self.lanes = scenario, count, state, lanes
        self.slots = sorted({lane.slot for lane in lanes})

    def key(self):
        return (self.scenario, self.count, self.state)

    def name(self):
        return f"{self.scenario} n={self.count} {self.state}"


class Side:
    def __init__(self, key, label, cargo, rustc, work):
        self.key, self.label = key, label
        self.cargo = Path(cargo).absolute()
        self.rustc = Path(rustc).absolute() if rustc else None
        self.home = work / f"{label}-home"
        self.xdg = work / f"{label}-xdg"


class Job:
    def __init__(self, command, cwd, env, log):
        self.command, self.cwd, self.env, self.log = command, cwd, env, log


def parse_lane(spec):
    head, *options = spec.split(",")
    mode, _, profile = head.partition(":")
    profile = profile or "dev"
    if mode not in LANE_MODES or profile not in PROFILES or (mode == "fmt" and profile != "dev"):
        raise argparse.ArgumentTypeError(f"bad lane {spec!r}")
    lane = Lane(mode, profile)
    for option in options:
        key, sep, value = option.partition("=")
        if not sep or not value:
            raise argparse.ArgumentTypeError(f"bad lane option {option!r} in {spec!r}")
        if key == "ws" and value.isdigit():
            lane.slot = int(value)
        elif key == "package":
            lane.package = value
        elif key == "rustc":
            lane.rustc = {"a": value, "b": value}
        elif key in ("rustc-a", "rustc-b"):
            lane.rustc[key[-1]] = value
        else:
            raise argparse.ArgumentTypeError(f"unknown lane option {option!r} in {spec!r}")
    return lane


def parse_side(spec):
    label, sep, rest = spec.partition("=")
    cargo, _, rustc = rest.partition(",")
    if not sep or not cargo or not re.fullmatch(r"[A-Za-z0-9_.-]+", label):
        raise argparse.ArgumentTypeError(f"{spec!r} is not LABEL=CARGO[,RUSTC]")
    return label, cargo, rustc


def parse_env(spec):
    key, sep, value = spec.partition("=")
    if not sep or not key:
        raise argparse.ArgumentTypeError(f"{spec!r} is not KEY=VALUE")
    return key, value


def parse_list(text, allowed, what):
    items = [item for item in text.split(",") if item]
    bad = [item for item in items if item not in allowed]
    if bad or not items:
        raise argparse.ArgumentTypeError(f"bad {what} {bad or text!r}; choose from {', '.join(allowed)}")
    return items


def fold(seed, size):
    acc = seed
    for i in range(size):
        acc = (acc * MUL + i + 1) & MASK
    return acc


# Generated text is rustfmt-clean so a concurrent cargo fmt --check lane passes.
def crate_source(upstream, size, test_value, proc_macro=False):
    """Formatted Rust whose value() folds size generated functions over its upstream value."""
    out = [f"const MUL: u64 = {MUL};"]
    if upstream is None:
        out.append(f"const SEED: u64 = {SEED};")
    out.append("")
    for i in range(size):
        out += [f"pub fn f{i}(x: u64) -> u64 {{",
                f"    x.wrapping_mul(MUL).wrapping_add({i + 1})", "}", ""]
    start = "SEED" if upstream is None else f"{upstream}::value()"
    if proc_macro:
        start = f"cb_macro::passthrough!({start})"
    out += ["pub fn value() -> u64 {", f"    let mut acc = {start};"]
    out += [f"    acc = f{i}(acc);" for i in range(size)]
    out += ["    acc", "}"]
    if test_value is not None:
        out += ["", "#[cfg(test)]", "mod tests {", "    #[test]", "    fn value_is_stable() {",
                f"        assert_eq!(super::value(), {test_value});", "    }", "}"]
    return "\n".join(out) + "\n"


def write_text(path, text):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)


def manifest(name, dependency=None, version="0.1.0"):
    text = f'[package]\nname = "{name}"\nversion = "{version}"\nedition = "2021"\n'
    if dependency:
        text += f"\n[dependencies]\n{dependency}\n"
    return text


def normalize_mtimes(root):
    for dirpath, dirnames, filenames in os.walk(root, topdown=False):
        for name in filenames + dirnames:
            os.utime(os.path.join(dirpath, name), (FIXED_MTIME, FIXED_MTIME), follow_symlinks=False)
    os.utime(root, (FIXED_MTIME, FIXED_MTIME))


def write_vendored_crate(vendor, size, name):
    """Write a checksummed directory-source crate that goes into Cargo.lock."""
    version = VENDORED["version"]
    manifest_text = manifest(name, version=version)
    source_text = crate_source(None, size, None)
    # Stands in for the SHA-256 of a .crate file, which an offline harness has none of. Any stable value
    # works: Cargo only requires that the lockfile and the directory source agree.
    checksum = hashlib.sha256((manifest_text + "\0" + source_text).encode()).hexdigest()
    write_text(vendor / name / "Cargo.toml", manifest_text)
    write_text(vendor / name / "src" / "lib.rs", source_text)
    files = {
        path: hashlib.sha256((vendor / name / path).read_bytes()).hexdigest()
        for path in ("Cargo.toml", "src/lib.rs")
    }
    write_text(vendor / name / ".cargo-checksum.json", json.dumps({"files": files, "package": checksum}) + "\n")
    return checksum


def write_builtin_fixture(template, deps, vendor, size, wide_members, proc_macro, extra_vendored):
    """Write the workspace template, the shared path crate and the vendored crate. Returns the expected values."""
    value_a = fold(SEED, size)
    value_b = fold(value_a, size)
    value_core = fold(value_b, size)
    left, right = (value_core + 1) & MASK, (value_core + 2) & MASK
    # cb_dep_a is a checksummed registry crate served from a directory source, so workspaces at different
    # paths see one identical package that a unit cache can share. cb_dep_b stays a path crate under deps.
    checksum = write_vendored_crate(vendor, size, VENDORED["name"])
    write_text(template / ".cargo" / "config.toml",
               '[source.crates-io]\nreplace-with = "cb-vendor"\n\n'
               '[source.cb-vendor]\ndirectory = "../fixture-vendor"\n')
    write_text(deps / "cb_dep_b" / "Cargo.toml", manifest("cb_dep_b", f'cb_dep_a = "{VENDORED["version"]}"'))
    write_text(deps / "cb_dep_b" / "src" / "lib.rs", crate_source("cb_dep_a", size, None))
    extra = tuple(f"cb_extra_{index}" for index in range(wide_members))
    members = ("cb_app", "cb_core", "cb_left", "cb_right") + extra
    write_text(template / "Cargo.toml", '[workspace]\nresolver = "2"\nmembers = [' +
               ", ".join(f'"{member}"' for member in members) + "]\n")
    core_deps = 'cb_dep_b = { path = "../../fixture-deps/cb_dep_b" }'
    if proc_macro:
        write_text(deps / "cb_macro" / "Cargo.toml", manifest("cb_macro") + "\n[lib]\nproc-macro = true\n")
        write_text(deps / "cb_macro" / "src" / "lib.rs",
                   "extern crate proc_macro;\nuse proc_macro::TokenStream;\n\n"
                   "#[proc_macro]\npub fn passthrough(input: TokenStream) -> TokenStream {\n    input\n}\n")
        core_deps += '\ncb_macro = { path = "../../fixture-deps/cb_macro" }'
    vendor_checksums = {VENDORED["name"]: checksum}
    core = crate_source("cb_dep_b", size, value_core, proc_macro)
    if extra_vendored:
        extra = [f"cb_registry_{index}" for index in range(extra_vendored)]
        for name in extra:
            vendor_checksums[name] = write_vendored_crate(vendor, size, name)
        core_deps += "".join(f'\n{name} = "{VENDORED["version"]}"' for name in extra)
        calls = "\n".join(f"    value ^= {name}::value();" for name in extra)
        core += f"\npub fn registry_values() -> u64 {{\n    let mut value = 0;\n{calls}\n    value\n}}\n"
    write_text(template / "cb_core" / "Cargo.toml", manifest("cb_core", core_deps))
    write_text(template / "cb_core" / "src" / "lib.rs", core)
    for name, delta, value in (("cb_left", 1, left), ("cb_right", 2, right)):
        write_text(template / name / "Cargo.toml", manifest(name, 'cb_core = { path = "../cb_core" }'))
        write_text(template / name / "src" / "lib.rs",
                   f"pub fn value() -> u64 {{\n    cb_core::value().wrapping_add({delta})\n}}\n\n"
                   f"#[cfg(test)]\nmod tests {{\n    #[test]\n    fn value_is_stable() {{\n"
                   f"        assert_eq!(super::value(), {value});\n    }}\n}}\n")
    write_text(template / "cb_app" / "Cargo.toml",
               manifest("cb_app", 'cb_left = { path = "../cb_left" }\ncb_right = { path = "../cb_right" }'))
    write_text(template / "cb_app" / "src" / "main.rs",
               'fn main() {\n    println!("{}", cb_left::value() ^ cb_right::value());\n}\n')
    for index, name in enumerate(extra):
        write_text(template / name / "Cargo.toml", manifest(name))
        write_text(template / name / "src" / "lib.rs", f"pub fn value() -> usize {{\n    {index}\n}}\n")
    normalize_mtimes(deps)
    normalize_mtimes(vendor)
    normalize_mtimes(template)
    return {"app_output": str(left ^ right), "vendor_checksums": vendor_checksums}


def tree_digest(root):
    """Content digest of a tree, independent of modification times and of target and .git directories."""
    digest = hashlib.sha256()
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = sorted(name for name in dirnames if name not in SKIP_DIRS)
        for name in sorted(filenames):
            path = Path(dirpath) / name
            digest.update(path.relative_to(root).as_posix().encode() + b"\0")
            if path.is_symlink():
                digest.update(b"link:" + os.readlink(path).encode())
            else:
                digest.update(hashlib.sha256(path.read_bytes()).digest())
    return digest.hexdigest()


def allocated(paths, seen=None):
    """Allocated bytes of the files and symlinks under paths, counting each inode once."""
    seen = set() if seen is None else seen
    total = 0
    for path in paths:
        for root, _, files in os.walk(path):
            for name in files:
                try:
                    info = os.lstat(os.path.join(root, name))
                except OSError:
                    continue
                if (info.st_dev, info.st_ino) not in seen:
                    seen.add((info.st_dev, info.st_ino))
                    total += info.st_blocks * 512
    return total


def rmtree(path):
    shutil.rmtree(path, ignore_errors=True)


def kill_group(pgid):
    """Kill a process group. True when it still had members."""
    try:
        os.killpg(pgid, signal.SIGKILL)
    except (ProcessLookupError, PermissionError):
        return False
    return True


def run_commands(jobs, timeout):
    """Spawn every job at once and wait for each in its own thread. Returns one result dict per job."""
    sinks = [open(job.log, "wb") for job in jobs]
    # The barrier releases every thread together, so spawn times differ only by thread wake-up.
    barrier = threading.Barrier(len(jobs))
    results = [None] * len(jobs)

    def worker(index):
        job = jobs[index]
        barrier.wait()
        begin = time.monotonic()
        try:
            child = subprocess.Popen(job.command, cwd=job.cwd, env=job.env, stdin=subprocess.DEVNULL,
                                     stdout=sinks[index], stderr=subprocess.STDOUT, start_new_session=True)
        except OSError as error:
            results[index] = {"error": str(error), "begin": begin, "finish": time.monotonic()}
            return
        with ACTIVE_LOCK:
            ACTIVE.add(child.pid)
        expired = threading.Event()

        def expire():
            expired.set()
            kill_group(child.pid)

        timer = threading.Timer(timeout, expire)
        timer.start()
        try:
            # Waiting on the pid itself keeps each command's usage separate from its peers'.
            _, status, usage = os.wait4(child.pid, 0)
        finally:
            finish = time.monotonic()
            timer.cancel()
        results[index] = {
            "begin": begin, "finish": finish, "pgid": child.pid, "timed_out": expired.is_set(),
            "cpu": usage.ru_utime + usage.ru_stime,
            # ru_maxrss is bytes on macOS and KiB on Linux.
            "max_rss_bytes": usage.ru_maxrss * (1 if sys.platform == "darwin" else 1024),
            "write_bytes": usage.ru_oublock * 512 if sys.platform.startswith("linux") else None,
            "oublock": usage.ru_oublock,
            "status": os.waitstatus_to_exitcode(status),
        }

    def guarded(index):
        try:
            worker(index)
        except Exception as error:
            results[index] = {"error": repr(error), "begin": 0.0, "finish": 0.0}

    threads = [threading.Thread(target=guarded, args=(i,), daemon=True) for i in range(len(jobs))]
    try:
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join()
    finally:
        for sink in sinks:
            sink.close()
    # Anything still in a command's process group once all commands exited is flagged, then killed.
    for result in results:
        pgid = result.get("pgid")
        if pgid is not None:
            result["lingering"] = kill_group(pgid)
            with ACTIVE_LOCK:
                ACTIVE.discard(pgid)
    return results


def result_ok(result):
    return "error" not in result and result["status"] == 0 and not result["timed_out"]


def tail(path, size=3000):
    return Path(path).read_text(errors="replace")[-size:]


def mib(value):
    return f"{value / 1048576:.1f}MiB"


def lock_waits(text):
    """Cargo's file lock waits in a command's output, in total and by what was locked."""
    by_kind = {}
    for kind in LOCK_WAIT.findall(text):
        by_kind[kind.strip()] = by_kind.get(kind.strip(), 0) + 1
    return {"total": sum(by_kind.values()), "by_kind": by_kind}


def cache_counters(text):
    """Unit cache decision counts and written MiB from FRUST_UNIT_CACHE_DEBUG lines, or None without any."""
    events = {}
    for script, kind in CACHE_EVENT.findall(text):
        name = ("script-" if script else "") + kind
        events[name] = events.get(name, 0) + 1
    written = CACHE_WRITTEN.findall(text)
    if not events and not written:
        return None
    return {"events": events, "written_mib": float(written[-1]) if written else None}


def sum_optional(values):
    """Sum, or None when there is nothing to sum or any value is unavailable."""
    values = list(values)
    return None if not values or any(v is None for v in values) else sum(values)


def median_known(values):
    values = [v for v in values if v is not None]
    return statistics.median(values) if values else None


def show(value, render):
    return "n/a" if value is None else render(value)


def merge_counts(counts):
    merged = {}
    for count in counts:
        for key, value in (count or {}).items():
            merged[key] = merged.get(key, 0) + value
    return merged


def paired_metric(values, parity, want):
    interval = PAIRED["median_interval"](values)
    evidence = PAIRED["interval_result"](interval, parity) if len(values) == want else "incomplete"
    return {"median": statistics.median(values), "min": min(values), "max": max(values),
            "pairs": len(values), "confidence95": interval, "evidence": evidence}


def paired_display(metric):
    interval = metric["confidence95"]
    bound = "unavailable" if interval is None else f"{interval[0]:.4g} to {interval[1]:.4g}"
    return f"{metric['median']:.4g} (95% interval {bound}, n={metric['pairs']}, {metric['evidence']})"


def flock_stats(trace):
    fields = ("attempts", "try_conflicts", "waits", "wait_ns", "contended_cpu_ns", "holds",
              "exclusive_hold_ns", "shared_hold_ns", "unfinished")
    stats = {}
    held = {}
    conflicts = {}

    def counters(kind):
        return stats.setdefault(kind, dict.fromkeys(fields, 0))

    def finish(key, start):
        if key in held:
            kind, mode, acquired = held.pop(key)
            row = counters(kind)
            row["holds"] += 1
            field = "exclusive_hold_ns" if mode == fcntl.LOCK_EX else "shared_hold_ns"
            row[field] += start - acquired

    records = []
    for line in trace.read_text(encoding="ascii").splitlines():
        pid, fd, op, flags, start, end, result, error, cpu, path = line.split("\t")
        key = (int(pid), int(fd))
        records.append((int(end), int(start), key, op, int(flags), int(result), int(error), int(cpu), path))
    for end, start, key, op, flags, result, error, cpu, path in sorted(records):
        if op == "C":
            if result == 0:
                finish(key, start)
                conflicts.pop(key, None)
            continue
        if op != "F" or end < start:
            raise ValueError("invalid flock trace record")
        if cpu == 2**64 - 1:
            raise ValueError("flock trace is missing thread CPU time")
        name = Path(os.fsdecode(bytes.fromhex(path))).name if path != "-" else "unknown"
        if re.fullmatch(r"[a-f0-9]{64}\.lock", name):
            name = "cache-entry"
        elif name == ".lock":
            name = "build-unit"
        elif re.fullmatch(r"s-[a-z0-9]+-[a-z0-9]+\.lock", name):
            name = "incremental-session"
        row = counters(name)
        row["attempts"] += 1
        mode = flags & ~fcntl.LOCK_NB
        if (mode == fcntl.LOCK_UN and result == 0) or (mode != fcntl.LOCK_UN and key in held and held[key][1] != mode):
            finish(key, start)
        if result == 0 and mode != fcntl.LOCK_UN:
            if conflicts.get(key) == (path, mode) and not flags & fcntl.LOCK_NB:
                row["waits"] += 1
                row["wait_ns"] += end - start
                row["contended_cpu_ns"] += cpu
            held.setdefault(key, (name, mode, end))
        conflicts.pop(key, None)
        if result != 0 and flags & fcntl.LOCK_NB and error in (errno.EAGAIN, errno.EWOULDBLOCK):
            row["try_conflicts"] += 1
            row["contended_cpu_ns"] += cpu
            conflicts[key] = (path, mode)
    for kind, _, _ in held.values():
        counters(kind)["unfinished"] += 1
    return stats


class ApfsWrites:
    def __init__(self, probe, root, mount, devices):
        self.probe, self.root, self.mount, self.devices = probe, root, mount, devices
        self.calibration = probe.calibrate_image(root, mount, devices)
        calibrated = self.calibration["usable_devices"]
        if len(calibrated) != 1:
            raise probe.ProbeError(f"expected one calibrated APFS volume, found {calibrated}")
        self.data_device = calibrated[0]
        match = re.fullmatch(r"(/dev/disk[0-9]+)s[0-9]+", self.data_device)
        if not match or match[1] not in devices:
            raise probe.ProbeError(f"could not identify the container of {self.data_device}")
        self.container = match[1]
        self.initial_available = None

    def header(self):
        return {"calibration": self.calibration, "data_device": self.data_device, "container": self.container,
                "write_scope": "completed virtual-volume writes plus container metadata, including the post-build sync",
                "footprint_scope": "occupied APFS blocks above the fixture setup, with both stores cleared before seeding"}

    def run(self, tag, jobs, timeout):
        results = []
        sample = self.probe.sample(self.root, self.mount, self.devices, tag,
                                   lambda: results.extend(run_commands(jobs, timeout)))
        devices = sample["write_bytes_by_device"]
        metadata = devices[self.data_device]["WrMeta"] + devices[self.container]["WrMeta"]
        write_bytes = devices[self.data_device]["WrData"] + devices[self.data_device]["PgOut"] + metadata
        return results, {"apfs_write_bytes": write_bytes, "apfs_metadata_bytes": metadata,
                         "apfs_footprint_bytes": self.initial_available - self.probe.available_bytes(self.mount),
                         "apfs_allocated_bytes_delta": sample["allocated_bytes_delta"],
                         "apfs_write_bytes_by_device": devices}


class Harness:
    def __init__(self, args, work, sides, pin, apfs=None):
        self.args, self.work, self.sides, self.pin = args, work, sides, pin
        self.apfs = apfs
        self.template = work / "template"
        self.deps = work / "fixture-deps"
        self.vendor = work / "fixture-vendor"
        self.logs = (apfs.root if apfs else work) / "logs"
        self.builtin = not args.fixture_dir
        self.cargo_home = (Path(args.cargo_home).absolute() if args.cargo_home else
                           work / "cargo-home" if self.builtin else Path.home() / ".cargo")
        self.edit_file = args.edit_file or (BUILTIN_EDIT_FILE if self.builtin else None)
        self.extra = {side.key: {**dict(args.env), **dict(getattr(args, f"env_{side.key}"))} for side in sides}
        self.expected = {}
        self.locked = False
        self.signatures = {}
        self.problems = []
        self.records = []

    def env(self, side, lane, target):
        # Same sanitised environment as the other benches: nothing inherited that could change Cargo or fRust.
        env = {k: v for k, v in os.environ.items() if not k.startswith(DROP_PREFIXES)}
        env.update(CARGO_HOME=str(self.cargo_home), CARGO_TARGET_DIR=str(target), CARGO_TERM_COLOR="never",
                   FRUST_PROGRESS="plain", FRUST_HOME=str(side.home), XDG_CACHE_HOME=str(side.xdg),
                   XDG_CONFIG_HOME=str(self.work / "xdg-config"),
                   PATH=os.pathsep.join([str(side.cargo.parent), env.get("PATH", "")]))
        if self.apfs:
            env["TMPDIR"] = str(self.work / "tmp")
        rustc = (lane.rustc.get(side.key) if lane else None) or side.rustc
        if rustc:
            compiler = Path(rustc).absolute()
            if lane and lane.mode == "clippy" and compiler.name == "frustc":
                sibling = compiler.resolve().with_name("rustc")
                if sibling.is_file() and compiler.samefile(sibling):
                    compiler = sibling
            env["RUSTC"] = str(compiler)
        env.update(self.extra[side.key])
        return env

    def slot_dir(self, slot):
        return self.work / f"ws{slot}"

    def target_dir(self, lane):
        suffix = "-ra" if lane.mode == "ra" and self.args.ra_target == "separate" else ""
        return self.work / f"target-{lane.slot}{suffix}"

    def command(self, side, lane):
        # cargo fmt rejects Cargo's global flags, so it gets only its own arguments.
        command = [str(side.cargo)]
        if lane.mode == "fmt":
            command += ["fmt"] + (["-p", lane.package] if lane.package else ["--all"]) + ["--", "--check"]
        else:
            command += ["check" if lane.mode == "ra" else lane.mode, "--offline"]
            if self.locked:
                command.append("--locked")
            if self.args.jobs:
                command += ["-j", str(self.args.jobs)]
            if lane.profile == "release":
                command.append("--release")
            command += ["-p", lane.package] if lane.package else ["--workspace"]
            if lane.mode in ("clippy", "ra"):
                command.append("--all-targets")
            if lane.mode == "doc":
                command.append("--no-deps")
            if lane.mode == "ra":
                command.append("--message-format=json-diagnostic-rendered-ansi")
        return ["taskset", "-c", self.pin] + command if self.pin else command

    def job(self, side, lane, log):
        return Job(self.command(side, lane), self.slot_dir(lane.slot),
                   self.env(side, lane, self.target_dir(lane)), log)

    def prepare(self):
        self.logs.mkdir(parents=True, exist_ok=True)
        (self.work / "xdg-config").mkdir(exist_ok=True)
        if self.apfs:
            (self.work / "tmp").mkdir()
        if self.builtin:
            self.expected = write_builtin_fixture(self.template, self.deps, self.vendor,
                                                  self.args.size, self.args.wide_members, self.args.proc_macro,
                                                  self.args.extra_vendored)
        else:
            shutil.copytree(self.args.fixture_dir, self.template, symlinks=True,
                            ignore=shutil.ignore_patterns(*SKIP_DIRS))
        # One lockfile, made once, is copied into every workspace so no command ever rewrites it.
        if not (self.template / "Cargo.lock").is_file() and not self.args.dry_run:
            side = self.sides[0]
            done = subprocess.run([str(side.cargo), "generate-lockfile", "--offline"], cwd=self.template,
                                  env=self.env(side, None, self.work / "lock-target"),
                                  capture_output=True, text=True)
            if done.returncode:
                sys.exit(f"cargo generate-lockfile failed for the fixture:\n{done.stderr[-3000:]}")
        self.locked = (self.template / "Cargo.lock").is_file() or (self.builtin and self.args.dry_run)
        if self.builtin and self.locked and not self.args.dry_run:
            lock_text = (self.template / "Cargo.lock").read_text()
            for name, checksum in self.expected["vendor_checksums"].items():
                if f'checksum = "{checksum}"' not in lock_text:
                    sys.exit(f"the generated Cargo.lock does not pin {name} by its checksum")
        self.template_digest = tree_digest(self.template)
        self.deps_digest = self.shared_digest()
        if self.apfs:
            os.sync()
            self.apfs.initial_available = self.apfs.probe.available_bytes(self.apfs.mount)

    def shared_digest(self):
        """Digest of the read-only crates every workspace uses, or None when the fixture has none."""
        roots = [root for root in (self.deps, self.vendor) if root.is_dir()]
        if not roots:
            return None
        return hashlib.sha256("".join(tree_digest(root) for root in roots).encode()).hexdigest()

    def reset_sources(self, cell, problems):
        # Both sides always build at the same workspace paths, restored byte for byte from the template.
        for slot in cell.slots:
            rmtree(self.slot_dir(slot))
            shutil.copytree(self.template, self.slot_dir(slot), symlinks=True)
            if tree_digest(self.slot_dir(slot)) != self.template_digest:
                problems.append(f"workspace ws{slot} differs from the template after reset")

    def wipe_targets(self, cell):
        for lane in cell.lanes:
            rmtree(self.target_dir(lane))

    def wipe_store(self, side):
        for path in (side.home, side.xdg):
            rmtree(path)
            path.mkdir(parents=True)

    def apply_edit(self, cell):
        for slot in cell.slots:
            with open(self.slot_dir(slot) / self.edit_file, "a") as source:
                source.write(EDIT_TEXT)

    def warm_up(self, cell, side, tag):
        seen = set()
        for lane in cell.lanes:
            if lane.key() in seen:
                continue
            seen.add(lane.key())
            job = self.job(side, lane, self.logs / f"{tag}-warm{len(seen)}.log")
            result = run_commands([job], self.args.timeout)[0]
            if not result_ok(result):
                sys.exit(f"warm-up of {lane.describe()} failed for {side.label} in {cell.name()}\n"
                         f"{tail(job.log)}")

    def measure(self, cell, side, rnd, position):
        tag = f"r{rnd}-{side.label}-{self.cell_index[cell.key()]}"
        problems, warnings = [], []
        if self.apfs:
            for pattern in ("target-*", "ws[0-9]*", "tmp"):
                for path in self.work.glob(pattern):
                    rmtree(path)
            (self.work / "tmp").mkdir()
        # Every measured run starts from sources, targets and store rebuilt for its state.
        self.reset_sources(cell, problems)
        self.wipe_targets(cell)
        for owner in self.sides if self.apfs else (side,):
            self.wipe_store(owner)
        if cell.state != "cold":
            self.warm_up(cell, side, tag)
        if cell.state == "cache":
            self.wipe_targets(cell)
        elif cell.state == "target":
            self.wipe_store(side)
        elif cell.state == "edit":
            self.apply_edit(cell)
        source_digests = {slot: tree_digest(self.slot_dir(slot)) for slot in cell.slots}
        targets = sorted({self.target_dir(lane) for lane in cell.lanes})
        stores = [side.home, side.xdg]
        before = allocated(targets + stores)
        # Storage is read outside the timed region, after the last command has exited.
        jobs = [self.job(side, lane, self.logs / f"{tag}-lane{i}.log") for i, lane in enumerate(cell.lanes)]
        traces = []
        if self.args.lock_trace_library:
            loader = "DYLD_INSERT_LIBRARIES" if sys.platform == "darwin" else "LD_PRELOAD"
            for job in jobs:
                trace = job.log.with_suffix(".flock")
                job.env.update({loader: self.args.lock_trace_library, "FRUST_BENCH_FLOCK_LOG": str(trace)})
                traces.append(trace)
        if self.apfs:
            results, apfs_metrics = self.apfs.run(tag, jobs, self.args.timeout)
            if cell.state in ("cold", "cache") and not apfs_metrics["apfs_write_bytes"]:
                problems.append(f"the {cell.state} build had no observed APFS writes")
        else:
            results, apfs_metrics = run_commands(jobs, self.args.timeout), {}
        target_bytes = allocated(targets)
        store_bytes = allocated(stores)
        total_bytes = allocated(targets + stores)
        lanes = []
        for index, (lane, job, result) in enumerate(zip(cell.lanes, jobs, results)):
            ok = result_ok(result)
            if not ok:
                problems.append(f"lane{index} {lane.describe()} failed: {result.get('error') or result}\n"
                                f"{tail(job.log)}")
            if result.get("lingering"):
                warnings.append(f"lane{index} left processes in its group, killed; their CPU is not counted")
            text = Path(job.log).read_text(errors="replace")
            compiling = None if lane.mode in ("ra", "fmt") else len(COMPILING.findall(text))
            if cell.state in ("warm", "target") and compiling:
                warnings.append(f"lane{index} printed {compiling} Compiling or Checking lines in state {cell.state}")
            signature = self.check_output(lane, text, job.log, problems) if ok else None
            if signature is not None:
                self.signatures.setdefault((cell.key(), index), {}).setdefault(side.label, set()).add(signature)
            lanes.append({
                "index": index, "mode": lane.mode, "profile": lane.profile, "workspace": lane.slot,
                "package": lane.package, "command": job.command, "rustc": job.env.get("RUSTC"),
                "wall": result["finish"] - result["begin"], "cpu": result.get("cpu"),
                "max_rss_bytes": result.get("max_rss_bytes"), "exit": result.get("status"),
                "write_bytes": result.get("write_bytes"),
                "oublock": result.get("oublock"), "lock_waits": lock_waits(text), "cache": cache_counters(text),
                "timed_out": result.get("timed_out", False), "compiling_lines": compiling,
                "signature": signature, "log": str(job.log), "ok": ok,
            })
            if traces:
                try:
                    trace = flock_stats(traces[index])
                except FileNotFoundError as error:
                    external = lane.mode == "fmt" or (sys.platform == "darwin" and lane.mode == "clippy")
                    if not external:
                        problems.append(f"lane{index} lock trace unavailable: {error}")
                    lanes[-1]["flock"] = {}
                except (OSError, ValueError) as error:
                    problems.append(f"lane{index} lock trace unavailable: {error}")
                else:
                    if not trace:
                        problems.append(f"lane{index} lock trace is empty")
                    lanes[-1]["flock"] = trace
        # Commands must leave sources and the shared path crates exactly as they found them.
        for slot, digest in source_digests.items():
            if tree_digest(self.slot_dir(slot)) != digest:
                problems.append(f"sources of ws{slot} changed during the commands")
        if self.deps_digest is not None and self.shared_digest() != self.deps_digest:
            problems.append("the shared path or vendored crates changed during the commands")
        begins = [r["begin"] for r in results]
        record = {
            "kind": "scenario", "scenario": cell.scenario, "count": cell.count, "state": cell.state,
            "side": side.label, "round": rnd, "position": position, "lanes": lanes,
            "wall": max(r["finish"] for r in results) - min(begins), "launch_skew": max(begins) - min(begins),
            "cpu_total": sum(lane["cpu"] or 0.0 for lane in lanes),
            "max_rss_bytes": max(lane["max_rss_bytes"] or 0 for lane in lanes),
            "target_bytes": target_bytes, "store_bytes": store_bytes, "total_bytes": total_bytes,
            "total_delta_bytes": total_bytes - before,
            "write_bytes": sum_optional(lane["write_bytes"] for lane in lanes),
            "oublock": sum(lane["oublock"] or 0 for lane in lanes),
            "lock_waits_total": sum(lane["lock_waits"]["total"] for lane in lanes),
            "lock_waits": merge_counts(lane["lock_waits"]["by_kind"] for lane in lanes),
            "cache_events": merge_counts(lane["cache"]["events"] for lane in lanes if lane["cache"]) or None,
            "cache_written_mib": sum_optional(lane["cache"]["written_mib"] for lane in lanes if lane["cache"]),
            "problems": problems, "warnings": warnings, "ok": not problems,
            **apfs_metrics,
        }
        self.problems += [f"{cell.name()} {side.label} round {rnd}: {p}" for p in problems]
        return record

    def check_output(self, lane, text, log, problems):
        """Validate what a successful command produced. Returns a signature to compare across sides."""
        if lane.mode == "ra":
            return self.check_ra(lane, text, problems)
        if lane.mode == "test":
            passed = sum(int(n) for n in re.findall(r"test result: ok\. (\d+) passed", text))
            if self.builtin:
                expected = (sum(BUILTIN_TEST_COUNTS.values()) if lane.package is None
                            else BUILTIN_TEST_COUNTS.get(lane.package))
                if expected is not None and passed != expected:
                    problems.append(f"{lane.describe()} ran {passed} tests, expected {expected}")
            return f"passed={passed}"
        if not self.builtin:
            return None
        target = self.target_dir(lane)
        if lane.mode == "build" and lane.package in (None, "cb_app"):
            binary = target / ("release" if lane.profile == "release" else "debug") / "cb_app"
            done = subprocess.run([str(binary)], capture_output=True, text=True, timeout=60) \
                if binary.is_file() else None
            if done is None or done.stdout.strip() != self.expected["app_output"]:
                problems.append(f"{lane.describe()} did not produce a cb_app printing "
                                f"{self.expected['app_output']}")
            return f"app={done.stdout.strip() if done else None}"
        if lane.mode == "doc":
            wanted = [lane.package] if lane.package in ("cb_core", "cb_left", "cb_right") else (
                ["cb_core", "cb_left", "cb_right"] if lane.package is None else [])
            for name in wanted:
                if not (target / "doc" / name / "index.html").is_file():
                    problems.append(f"{lane.describe()} wrote no documentation page for {name}")
        return None

    def check_ra(self, lane, text, problems):
        events = []
        for line in text.splitlines():
            if line.startswith("{"):
                try:
                    events.append(json.loads(line))
                except ValueError:
                    continue
        finished = [e for e in events if e.get("reason") == "build-finished"]
        if len(finished) != 1 or finished[0].get("success") is not True:
            problems.append(f"{lane.describe()} did not emit one successful build-finished event")
        names = sorted({
            "{}:{}:{}".format(e["target"]["name"], ",".join(e["target"]["kind"]),
                              "test" if e.get("profile", {}).get("test") else "build")
            for e in events if e.get("reason") == "compiler-artifact"})
        if self.builtin and lane.package is None:
            have = {name.split(":")[0] for name in names}
            missing = {"cb_core", "cb_left", "cb_right", "cb_app", "cb_dep_a", "cb_dep_b"} - have
            if missing:
                problems.append(f"{lane.describe()} reported no artifact for {sorted(missing)}")
        return hashlib.sha256("|".join(names).encode()).hexdigest()[:16]

    def compare_signatures(self):
        # A lane must produce the same observable output on every round and on both sides.
        for (key, index), by_side in sorted(self.signatures.items()):
            values = set().union(*by_side.values())
            if len(values) > 1:
                detail = {label: sorted(found) for label, found in by_side.items()}
                self.problems.append(f"{' '.join(map(str, key))} lane{index} outputs differ: {detail}")

    def run(self, cells):
        self.cell_index = {cell.key(): i for i, cell in enumerate(cells)}
        for rnd in range(self.args.rounds):
            # Alternating the order gives each side the first position in half of the rounds.
            order = self.sides if rnd % 2 == 0 else self.sides[::-1]
            for cell in cells:
                for position, side in enumerate(order):
                    record = self.measure(cell, side, rnd, position)
                    self.records.append(record)
                    self.emit(record)
        self.compare_signatures()

    def write_record(self, record):
        with open(self.args.out, "a") as handle:
            handle.write(json.dumps(record, sort_keys=True) + "\n")

    def emit(self, record):
        self.write_record(record)
        head = f"r{record['round']} {record['side']} {record['scenario']} n={record['count']} {record['state']}"
        if self.apfs:
            measures = " ".join(f"{key}={mib(record[key])}" for key in APFS_METRICS)
            print(f"{head} {measures}{'' if record['ok'] else ' FAILED'}", flush=True)
            return
        if not self.args.summary_only:
            for lane in record["lanes"]:
                print(f"{head} lane{lane['index']} {lane['mode']}:{lane['profile']}@ws{lane['workspace']} "
                      f"cpu={(lane['cpu'] or 0):.3f}s max_rss={mib(lane['max_rss_bytes'] or 0)} "
                      f"write={show(lane['write_bytes'], mib)} lock_waits={lane['lock_waits']['total']} "
                      f"exit={lane['exit']}", flush=True)
        print(f"{head} SCENARIO cpu_total={record['cpu_total']:.3f}s max_rss={mib(record['max_rss_bytes'])} "
              f"write={show(record['write_bytes'], mib)} lock_waits={record['lock_waits_total']} "
              f"cache_written={show(record['cache_written_mib'], lambda v: f'{v:.1f}MiB')} "
              f"target={mib(record['target_bytes'])} store={mib(record['store_bytes'])} "
              f"total={mib(record['total_bytes'])} delta={mib(record['total_delta_bytes'])}"
              f"{'' if record['ok'] else ' FAILED'}", flush=True)
        for warning in record["warnings"]:
            print(f"  warning: {warning}", flush=True)

    def summary_data(self, cells):
        """Medians per side and paired b/a ratios per cell, over the rounds both sides completed."""
        a, b = self.sides
        metrics = APFS_METRICS if self.apfs else SUMMARY_METRICS
        ratio_metrics = APFS_METRICS if self.apfs else RATIO_METRICS
        entries = []
        for cell in cells:
            ok = {s.label: {r["round"]: r for r in self.records
                            if r["ok"] and r["side"] == s.label and (r["scenario"], r["count"], r["state"]) == cell.key()}
                  for s in self.sides}
            entry = {"scenario": cell.scenario, "count": cell.count, "state": cell.state, "sides": {},
                     "ratios": {}, "deltas": {}}
            for side in self.sides:
                rows = list(ok[side.label].values())
                entry["sides"][side.label] = {"ok_rounds": len(rows),
                                              **{m: median_known(r[m] for r in rows) for m in metrics}}
            shared = sorted(set(ok[a.label]) & set(ok[b.label]))
            for metric in ratio_metrics:
                pairs = [(ok[a.label][r][metric], ok[b.label][r][metric]) for r in shared]
                pairs = [(x, y) for x, y in pairs if x is not None and y is not None]
                if pairs:
                    entry["deltas"][metric] = paired_metric([y - x for x, y in pairs], 0, self.args.rounds)
                ratios = [y / x for x, y in pairs if x]
                if ratios:
                    entry["ratios"][metric] = {**paired_metric(ratios, 1.0, self.args.rounds),
                                               "b_lower": sum(ratio < 1.0 for ratio in ratios)}
            entries.append(entry)
        return {"kind": "summary", "a": a.label, "b": b.label, "cells": entries}

    def summary(self, cells):
        if self.args.lock_trace_library:
            self.lock_summary(cells)
            return
        data = self.summary_data(cells)
        self.write_record(data)
        print(f"\npaired rounds, ratios {data['b']}/{data['a']} and differences {data['b']} - {data['a']}")
        for entry in data["cells"]:
            print(f"{entry['scenario']} n={entry['count']} {entry['state']}")
            for label, row in entry["sides"].items():
                if row["ok_rounds"]:
                    if self.apfs:
                        measures = " ".join(f"{key}={mib(row[key])}" for key in APFS_METRICS)
                        print(f"  {label:10} {measures} [{row['ok_rounds']} ok]")
                        continue
                    print(f"  {label:10} cpu_total={row['cpu_total']:8.3f}s max_rss={mib(row['max_rss_bytes'])} "
                          f"write={show(row['write_bytes'], mib)} "
                          f"lock_waits={show(row['lock_waits_total'], '{:g}'.format)} "
                          f"cache_written={show(row['cache_written_mib'], lambda v: f'{v:.1f}MiB')} "
                          f"target={mib(row['target_bytes'])} store={mib(row['store_bytes'])} "
                          f"total={mib(row['total_bytes'])} [{row['ok_rounds']} ok]")
            for metric, ratio in entry["ratios"].items():
                print(f"  ratio {metric:22} {paired_display(ratio)}")
            for metric, delta in entry["deltas"].items():
                if metric not in entry["ratios"] or entry["ratios"][metric]["pairs"] != delta["pairs"]:
                    print(f"  delta {metric:22} {paired_display(delta)}")

    def lock_summary(self, cells):
        table = []
        for cell in cells:
            entry = {"scenario": cell.scenario, "count": cell.count, "state": cell.state, "sides": {}, "deltas": {}}
            print(cell.name())
            by_side = {}
            for side in self.sides:
                rows = [r for r in self.records if r["ok"] and r["side"] == side.label
                        and (r["scenario"], r["count"], r["state"]) == cell.key()]
                rounds = {}
                for row in rows:
                    traces = [lane.get("flock", {}) for lane in row["lanes"]]
                    rounds[row["round"]] = {kind: merge_counts(trace.get(kind) for trace in traces)
                                            for kind in set().union(*traces)}
                by_side[side.label] = rounds
                locks = {}
                for kind in sorted(set().union(*rounds.values())):
                    fields = next(row[kind].keys() for row in rounds.values() if kind in row)
                    locks[kind] = {key: statistics.median(row.get(kind, {}).get(key, 0) for row in rounds.values())
                                   for key in fields}
                    counts = locks[kind]
                    print(f"  {side.label:10} {kind:24} tries blocked={counts['try_conflicts']:g} "
                          f"waits={counts['waits']:g} wait={counts['wait_ns'] / 1e6:.2f}ms "
                          f"contended CPU={counts['contended_cpu_ns'] / 1e3:.2f}us "
                          f"holds={counts['holds']:g} exclusive={counts['exclusive_hold_ns'] / 1e6:.2f}ms "
                          f"shared={counts['shared_hold_ns'] / 1e6:.2f}ms "
                          f"unfinished={counts['unfinished']:g}")
                entry["sides"][side.label] = {"rounds": len(rounds), "locks": locks}
            control, frust = (by_side[side.label] for side in self.sides)
            shared = control.keys() & frust.keys()
            if shared:
                kinds = set().union(*(control[r].keys() | frust[r].keys() for r in shared))
                for kind in sorted(kinds):
                    deltas = {}
                    for metric in ("waits", "try_conflicts", "contended_cpu_ns"):
                        differences = [frust[r].get(kind, {}).get(metric, 0) - control[r].get(kind, {}).get(metric, 0)
                                       for r in shared]
                        deltas[metric] = paired_metric(differences, 0, self.args.rounds)
                    entry["deltas"][kind] = deltas
                    print(f"  {kind:24} wait delta {paired_display(deltas['waits'])} "
                          f"contended CPU delta in ns {paired_display(deltas['contended_cpu_ns'])}")
            table.append(entry)
        self.write_record({"kind": "lock-summary", "cells": table})

    def header(self, cells):
        def version(command, side):
            try:
                done = subprocess.run(command, capture_output=True, text=True,
                                      env=self.env(side, None, self.work / "version-target"))
            except OSError:
                return None
            return done.stdout.strip() or None

        return {
            "kind": "run", "labels": LABELS, "argv": sys.argv, "rounds": self.args.rounds,
            "jobs": self.args.jobs, "size": self.args.size, "pin": self.pin,
            "wide_members": self.args.wide_members,
            "proc_macro": self.args.proc_macro,
            "lock_trace": bool(self.args.lock_trace_library),
            "apfs": self.apfs.header() if self.apfs else None,
            "template_digest": self.template_digest, "deps_digest": self.deps_digest,
            "preset": self.args.preset, "states": self.args.states,
            "vendored": ({**VENDORED, "checksum": self.expected["vendor_checksums"][VENDORED["name"]],
                          "count": len(self.expected["vendor_checksums"])} if self.builtin else None),
            "locked": self.locked, "cargo_home": str(self.cargo_home),
            "host": {"system": platform.system(), "machine": platform.machine(),
                     "uname": " ".join(platform.uname()), "python": platform.python_version(),
                     "logical_cpus": os.cpu_count()},
            "sides": [{"label": s.label, "cargo": str(s.cargo), "rustc": str(s.rustc) if s.rustc else None,
                       "cargo_version": version([str(s.cargo), "--version"], s),
                       "rustc_version": version([str(s.rustc) if s.rustc else "rustc", "--version"], s)}
                      for s in self.sides],
            "env": self.extra,
            "cells": [{"name": c.name(), "lanes": [lane.describe() for lane in c.lanes]} for c in cells],
        }


def plan_cells(args, members):
    cells = []
    for name, layout, specs in PRESETS[args.preset] if args.preset else ():
        for count in args.count:
            lanes = []
            for i in range(count):
                lane = parse_lane(specs[i % len(specs)])
                lanes.append(lane.moved(slot=i) if layout == "independent" else lane)
            cells += [Cell(name, count, state, lanes) for state in args.states]
    for scenario in args.scenarios:
        counts = [len(args.lane)] if scenario == "custom" else args.count
        for count in counts:
            lanes = []
            for i in range(count):
                if scenario == "custom":
                    lanes.append(args.lane[i])
                elif scenario == "mixed":
                    lanes.append(args.mix[i % len(args.mix)])
                else:
                    base = Lane(args.mode, args.profile)
                    if scenario == "overlap":
                        lanes.append(base.moved(package=members[i % len(members)]))
                    else:
                        lanes.append(base.moved(slot=i if scenario == "independent" else 0))
            for state in args.states:
                cells.append(Cell(scenario, count, state, lanes))
    return cells


def pinning(spec):
    """taskset CPU list, or None when pinning is off or unavailable."""
    if spec == "none" or not sys.platform.startswith("linux") or shutil.which("taskset") is None:
        return None
    if spec == "auto":
        return ",".join(str(cpu) for cpu in sorted(os.sched_getaffinity(0)))
    return spec


def parse_args():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--a", type=parse_side, required=True, metavar="LABEL=CARGO[,RUSTC]")
    parser.add_argument("--b", type=parse_side, required=True, metavar="LABEL=CARGO[,RUSTC]")
    parser.add_argument("--scenarios", help=f"comma list of {', '.join(SCENARIOS)} "
                        f"(default {DEFAULT_SCENARIOS}, or custom when --lane is given)")
    parser.add_argument("--preset", choices=sorted(PRESETS), help="a fixed scenario matrix instead of --scenarios")
    parser.add_argument("--states", help=f"comma list of {', '.join(STATES)} (default cold,warm, or "
                        f"cold,warm,cache with --preset)")
    parser.add_argument("--count", default="4", help="commands or workspaces per scenario, comma list allowed")
    parser.add_argument("--rounds", type=int, default=4, help="paired rounds; use an even number")
    parser.add_argument("--jobs", type=int, default=2, help="cargo -j per command, 0 leaves Cargo's default")
    parser.add_argument("--mode", choices=MODES, default="build", help="command of same-workspace, overlap, independent")
    parser.add_argument("--profile", choices=PROFILES, default="dev")
    parser.add_argument("--mix", default="build,build:release,ra", help="lanes the mixed scenario cycles through")
    parser.add_argument("--lane", action="append", type=parse_lane, default=[], help="a custom lane, repeatable")
    parser.add_argument("--members", help="comma list of packages for overlap with --fixture-dir")
    parser.add_argument("--fixture-dir", help="copy this workspace instead of the generated one (needs Cargo.lock "
                        "or an offline resolution, and a CARGO_HOME holding its dependencies)")
    parser.add_argument("--edit-file", help="file, relative to the workspace, that the edit state appends to")
    parser.add_argument("--size", type=int, default=400, help="generated functions per crate of the generated fixture")
    parser.add_argument("--wide-members", type=int, default=0, help="extra library members in the generated workspace")
    parser.add_argument("--proc-macro", action="store_true", help="include a shared host proc macro in the generated fixture")
    parser.add_argument("--extra-vendored", type=int, default=0, help="extra checksummed dependencies in the generated fixture")
    parser.add_argument("--ra-target", choices=("shared", "separate"), default="shared",
                        help="ra lanes use the workspace's target directory or their own")
    parser.add_argument("--cpus", default="none", help="none, auto or a taskset list (Linux only)")
    parser.add_argument("--env", action="append", type=parse_env, default=[], help="KEY=VALUE for both sides")
    parser.add_argument("--env-a", action="append", type=parse_env, default=[], help="KEY=VALUE for side a only")
    parser.add_argument("--env-b", action="append", type=parse_env, default=[], help="KEY=VALUE for side b only")
    parser.add_argument("--cargo-home", help="CARGO_HOME (default an empty one, or ~/.cargo with --fixture-dir)")
    parser.add_argument("--timeout", type=float, default=3600, help="seconds per command")
    parser.add_argument("--work", help="absent or empty directory for fixtures and targets")
    parser.add_argument("--keep", action="store_true", help="keep the work directory and its logs")
    parser.add_argument("--out", help="JSON lines results file (required unless --dry-run)")
    parser.add_argument("--append", action="store_true", help="append to an existing --out")
    parser.add_argument("--summary-only", action="store_true", help="omit the per-command lines")
    parser.add_argument("--lock-trace-library", type=lambda path: str(Path(path).resolve()),
                        help="native bench-flock shared library for a separate lock diagnostic run")
    parser.add_argument("--apfs-writes", action="store_true",
                        help="calibrate and measure virtual APFS write bytes on macOS in a private image")
    parser.add_argument("--dry-run", action="store_true", help="print the plan and the fixture digest only")
    args = parser.parse_args()
    if args.preset and (args.scenarios or args.lane):
        parser.error("--preset replaces --scenarios and --lane")
    try:
        args.scenarios = [] if args.preset else parse_list(
            args.scenarios or ("custom" if args.lane else DEFAULT_SCENARIOS), SCENARIOS, "scenario")
        args.states = parse_list(args.states or ("cold,warm,cache" if args.preset else "cold,warm"),
                                 STATES, "state")
        args.count = [int(n) for n in args.count.split(",")]
        args.mix = [parse_lane(spec) for spec in args.mix.split(",")]
    except (argparse.ArgumentTypeError, ValueError) as error:
        parser.error(str(error))
    if min(args.count) < 1 or args.rounds < 1 or args.jobs < 0 or args.size < 1 or args.timeout <= 0:
        parser.error("--count, --rounds and --size must be positive, --jobs not negative, --timeout positive")
    if args.wide_members < 0 or (args.wide_members and args.fixture_dir):
        parser.error("--wide-members is a nonnegative count for the generated fixture")
    if args.proc_macro and args.fixture_dir:
        parser.error("--proc-macro needs the generated fixture")
    if args.extra_vendored < 0 or (args.extra_vendored and args.fixture_dir):
        parser.error("--extra-vendored is a nonnegative count for the generated fixture")
    if "custom" in args.scenarios and not args.lane:
        parser.error("the custom scenario needs --lane")
    if args.lane and "custom" not in args.scenarios:
        parser.error("--lane only applies to the custom scenario")
    if args.a[0] == args.b[0]:
        parser.error("--a and --b need different labels")
    if not args.dry_run and not args.out:
        parser.error("--out is required unless --dry-run")
    if args.out and Path(args.out).exists() and not args.append and not args.dry_run:
        parser.error(f"{args.out} exists; choose another file or pass --append")
    if args.lock_trace_library and not Path(args.lock_trace_library).is_file():
        parser.error("--lock-trace-library must be a compiled bench-flock library")
    if args.apfs_writes:
        if sys.platform != "darwin":
            parser.error("--apfs-writes requires macOS")
        if args.fixture_dir or args.cargo_home or args.keep or args.lock_trace_library:
            parser.error("--apfs-writes needs a generated fixture and private Cargo home without persistent work files or lock tracing")
        isolated = {"CARGO_HOME", "CARGO_TARGET_DIR", "CARGO_BUILD_BUILD_DIR", "FRUST_HOME", "TMPDIR",
                    "XDG_CACHE_HOME", "XDG_CONFIG_HOME"}
        if any(key in isolated for key, _ in args.env + args.env_a + args.env_b):
            parser.error("--apfs-writes manages the writable benchmark paths on its own volume")
    if args.fixture_dir:
        if not (Path(args.fixture_dir) / "Cargo.toml").is_file():
            parser.error("--fixture-dir has no Cargo.toml")
        if "edit" in args.states and not args.edit_file:
            parser.error("the edit state needs --edit-file with --fixture-dir")
        if "overlap" in args.scenarios and not args.members:
            parser.error("the overlap scenario needs --members with --fixture-dir")
    if args.rounds % 2:
        print("note: an odd --rounds leaves the two sides unequal in first position", file=sys.stderr)
    return args


def print_plan(args, harness, cells):
    print(f"fixture {'generated' if harness.builtin else args.fixture_dir} digest {harness.template_digest}")
    for cell in cells:
        print(cell.name())
        for index, lane in enumerate(cell.lanes):
            print(f"  lane{index} {lane.describe()}: {' '.join(harness.command(harness.sides[0], lane))}")


def run_harness(args, work, apfs=None):
    work.mkdir(exist_ok=True)
    sides = []
    for key, spec in zip("ab", (args.a, args.b)):
        sides.append(Side(key, spec[0], spec[1], spec[2], work))
    harness = Harness(args, work, sides, pinning(args.cpus), apfs)
    try:
        harness.prepare()
        members = args.members.split(",") if args.members else list(OVERLAP_PACKAGES)
        cells = plan_cells(args, members)
        if args.dry_run:
            print_plan(args, harness, cells)
            return 0
        with open(args.out, "a") as handle:
            handle.write(json.dumps(harness.header(cells), sort_keys=True) + "\n")
        harness.run(cells)
        harness.summary(cells)
        if harness.problems:
            print(f"\nVALIDATION FAILED ({len(harness.problems)})", file=sys.stderr)
            for problem in harness.problems:
                print(f"  {problem}", file=sys.stderr)
            return 1
        print("\nvalidation passed")
        return 0
    finally:
        with ACTIVE_LOCK:
            for pgid in ACTIVE:
                kill_group(pgid)


def main():
    args = parse_args()
    for _, cargo, rustc in (args.a, args.b):
        for tool in (cargo, rustc):
            if tool and not os.access(tool, os.X_OK):
                sys.exit(f"{tool} is not an executable file")
    if args.work:
        work = Path(args.work).absolute()
        if work.exists() and any(work.iterdir()):
            sys.exit(f"{work} is not empty")
        work.mkdir(parents=True, exist_ok=True)
    else:
        base = REPO / "target" / "concurrent-builds"
        base.mkdir(parents=True, exist_ok=True)
        work = Path(tempfile.mkdtemp(prefix="run-", dir=base))
    if args.apfs_writes and not args.dry_run:
        spec = importlib.util.spec_from_file_location("darwin_writes", REPO / "qualification/darwin-write-probe.py")
        probe = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(probe)
        try:
            with probe.mounted_image(work, size="2g") as image:
                apfs = ApfsWrites(probe, *image)
                return run_harness(args, apfs.mount / "benchmark", apfs)
        except probe.ProbeError as error:
            print(f"APFS measurement failed: {error}", file=sys.stderr)
            return 1
        finally:
            if not args.work and not any(work.iterdir()):
                work.rmdir()
    try:
        return run_harness(args, work)
    finally:
        if not args.keep:
            for child in work.iterdir():
                rmtree(child) if child.is_dir() and not child.is_symlink() else child.unlink()
            if not args.work:
                work.rmdir()


if __name__ == "__main__":
    raise SystemExit(main())
