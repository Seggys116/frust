#!/usr/bin/env python3
"""Paired build CPU comparison of official Rust against fRust with its unit cache, for CI.

One run measures one cohort application on one host. Every round builds the application
with both toolchains, one after the other, and alternates which goes first. Scenarios, all
with the frozen release profile of qualification/PROTOCOL.md on both sides:

  cold         empty target, and for fRust an empty unit cache store
  clean        target removed; the fRust store was filled by this round's cold build
  noop         immediate rebuild, median of --noops builds
  edit-<kind>  rebuild of a copy of the clean target after the frozen edit of that kind

The official toolchain has no store. The fRust store lives in a per-toolchain directory under
--work that FRUST_HOME points at, and it is deleted and checked empty before every cold build,
so no cold build can be served by an earlier round. A cold fRust build that leaves the store
empty is a failure, because the store would not be isolated and the clean series meaningless.

An unmeasured official build runs first, so unpacking the fetched registry sources is not charged
to whichever toolchain builds first in round 0.

With --noop-only, each toolchain builds its own target once before timing. The timed no-op pairs
alternate directly on those warm targets, and a run fails if either Cargo compiles a unit.

Page-cache state (Linux). Whether the compiler maps librustc_driver and libLLVM text with 2 MiB
pages depends on how those files entered the page cache (qualification/toolchain-folio-state.py),
so the run records it for both toolchains, as "folio" records, before the first timed build and
after the last. --cache-state installed leaves the toolchains as their installers left them, which
is what users get. --cache-state evicted drops the cached pages of both toolchains' libraries
(qualification/page-cache-state.py evict) before every build, so both start every build from the
same cold-cache state and the compilers alone are compared.

CPU is user plus system seconds of the cargo process tree from wait4. On Linux builds are
pinned with taskset. macOS has no taskset, so there nothing is pinned and the metadata record
says so. The Toolchain class and the helpers come from run-store-bench.py.

The output is JSON lines: one meta record (host, toolchains, commit, cohort, pinning, cache
state), the folio records, then one sample record per build and a stop record when the time
budget ended the run early.
"""
import argparse
import contextlib
import datetime
import hashlib
import importlib.util
import json
import os
import pathlib
import platform
import re
import shutil
import statistics
import subprocess
import sys
import time
from collections import Counter

if sys.version_info < (3, 9):
    sys.exit("ci-bench.py needs Python 3.9 or newer (os.waitstatus_to_exitcode)")

ROOT = pathlib.Path(__file__).resolve().parent.parent
EDIT_DIR = ROOT / "qualification" / "edits"
EDITS = {"ripgrep": ("leaf", "wide"), "syn": ("leaf", "wide"), "regex": ("leaf", "wide"), "fd": ("holdout",)}
SCHEMA = "frust-compare-1"
# The shared release profile of qualification/PROTOCOL.md, identical on both sides.
PROFILE = {
    "CARGO_PROFILE_RELEASE_OPT_LEVEL": "3",
    "CARGO_PROFILE_RELEASE_LTO": "fat",
    "CARGO_PROFILE_RELEASE_CODEGEN_UNITS": "1",
    "CARGO_PROFILE_RELEASE_INCREMENTAL": "true",
    "CARGO_PROFILE_RELEASE_DEBUG": "0",
    "CARGO_PROFILE_RELEASE_STRIP": "none",
}
UNIT_EVENT = re.compile(r"frust-unit-cache: (script-)?(hit|miss|store|skip) ")
PAGE_CACHE_STATE = ROOT / "qualification" / "page-cache-state.py"
FOLIO_STATE = ROOT / "qualification" / "toolchain-folio-state.py"
FOLIO_FIELD = re.compile(r"(\w+)=(\d+)MiB")


def load_store_bench():
    """Import run-store-bench.py, whose name is not a valid module name."""
    sys.dont_write_bytecode = True
    path = pathlib.Path(__file__).resolve().with_name("run-store-bench.py")
    spec = importlib.util.spec_from_file_location("run_store_bench", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


STORE = load_store_bench()


def count_cpus(spec):
    """Number of CPUs in a taskset list such as 0-3,6."""
    total = 0
    for part in spec.split(","):
        low, _, high = part.partition("-")
        total += int(high or low) - int(low) + 1
    return total


def plan_pinning(spec):
    """Return (taskset CPU list or None, logical CPU count, note)."""
    if not sys.platform.startswith("linux"):
        return None, os.cpu_count() or 1, "no pinning: this OS has no taskset, builds use every logical CPU"
    available = sorted(os.sched_getaffinity(0))
    if spec == "none":
        return None, len(available), "no pinning: disabled with --cpus none"
    if shutil.which("taskset") is None:
        return None, len(available), "no pinning: taskset is not installed"
    if spec == "auto":
        return ",".join(str(cpu) for cpu in available), len(available), "taskset on every available CPU"
    return spec, count_cpus(spec), "taskset on the CPUs given with --cpus"


def capture(command):
    try:
        done = subprocess.run(command, capture_output=True, text=True)
    except OSError:
        return None
    return done.stdout.strip() if done.returncode == 0 and done.stdout.strip() else None


def host_info():
    info = {
        "system": platform.system(), "machine": platform.machine(), "uname": " ".join(platform.uname()),
        "python": platform.python_version(), "logical_cpus": os.cpu_count(),
        "image_os": os.environ.get("ImageOS"), "image_version": os.environ.get("ImageVersion"),
        "runner_name": os.environ.get("RUNNER_NAME"),
    }
    if sys.platform == "darwin":
        info["cpu_model"] = capture(["sysctl", "-n", "machdep.cpu.brand_string"])
        info["physical_cpus"] = capture(["sysctl", "-n", "hw.physicalcpu"])
        info["performance_cpus"] = capture(["sysctl", "-n", "hw.perflevel0.physicalcpu"])
        info["efficiency_cpus"] = capture(["sysctl", "-n", "hw.perflevel1.physicalcpu"])
        info["memory_bytes"] = capture(["sysctl", "-n", "hw.memsize"])
        info["os"] = "macOS " + (capture(["sw_vers", "-productVersion"]) or "unknown")
    else:
        model, cores, current = None, set(), {}
        try:
            for line in pathlib.Path("/proc/cpuinfo").read_text(errors="replace").splitlines():
                key, _, value = line.partition(":")
                key, value = key.strip(), value.strip()
                if key == "model name" and model is None:
                    model = value
                if key in ("physical id", "core id"):
                    current[key] = value
                    if len(current) == 2:
                        cores.add((current["physical id"], current["core id"]))
                        current = {}
        except OSError:
            pass
        info["cpu_model"] = model
        info["physical_cpus"] = str(len(cores)) if cores else None
        info["memory_bytes"] = None
        try:
            for line in pathlib.Path("/proc/meminfo").read_text().splitlines():
                if line.startswith("MemTotal:"):
                    info["memory_bytes"] = str(int(line.split()[1]) * 1024)
        except OSError:
            pass
        info["os"] = None
        try:
            for line in pathlib.Path("/etc/os-release").read_text().splitlines():
                if line.startswith("PRETTY_NAME="):
                    info["os"] = line.partition("=")[2].strip('"')
        except OSError:
            pass
    return info


def git_head(directory):
    return capture(["git", "-C", str(directory), "rev-parse", "HEAD"])


def tree_clean(app):
    done = subprocess.run(["git", "-C", str(app), "status", "--porcelain", "--untracked-files=no"],
                          capture_output=True, text=True)
    return done.returncode == 0 and not done.stdout.strip()


def copy_tree(source, dest):
    if sys.platform == "darwin":
        # BSD cp has no --reflink. -c clones on APFS and fails elsewhere, so fall back to a plain copy.
        if subprocess.run(["cp", "-a", "-c", str(source), str(dest)], capture_output=True).returncode == 0:
            return
        shutil.rmtree(dest, ignore_errors=True)
        subprocess.run(["cp", "-a", str(source), str(dest)], check=True)
    else:
        subprocess.run(["cp", "-a", "--reflink=auto", str(source), str(dest)], check=True)


def evict_page_cache(prefixes):
    """Drop the cached pages of the toolchains' libraries. Dirty pages are written out first, because
    POSIX_FADV_DONTNEED leaves them cached."""
    os.sync()
    done = subprocess.run([sys.executable, "-I", str(PAGE_CACHE_STATE), "evict", *map(str, prefixes)],
                          stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
    if done.returncode != 0:
        sys.exit(f"page-cache-state.py evict failed: {done.stderr.strip()}")


def log_counts(log):
    text = pathlib.Path(log).read_text(errors="replace")
    events = Counter(f"{prefix}{kind}" for prefix, kind in UNIT_EVENT.findall(text))
    return len(re.findall(r"^\s*Compiling ", text, re.M)), dict(sorted(events.items()))


class CiToolchain(STORE.Toolchain):
    """run-store-bench's Toolchain with the frozen profile, FRUST_HOME isolation and optional pinning."""

    def __init__(self, name, cargo, rustc, work, pin, jobs, uses_store, extra_env, evict_prefixes=()):
        super().__init__(name, cargo, rustc, work, pin or "", jobs, uses_store)
        self.pin = pin
        self.store = self.work / f"{name}-store"
        self.xdg_cache = self.work / f"{name}-xdg-cache"
        self.extra_env = extra_env
        # The toolchain directory: <prefix>/bin/cargo.
        self.prefix = pathlib.Path(cargo).parent.parent
        self.evict_prefixes = list(evict_prefixes)

    def env(self, target):
        env = super().env(target)
        # The profile asks for incremental builds, so the parent's CARGO_INCREMENTAL=0 must go.
        env.pop("CARGO_INCREMENTAL", None)
        env.update(PROFILE)
        env["XDG_CACHE_HOME"] = str(self.xdg_cache)
        if self.uses_store:
            # FRUST_HOME moves the store on Linux and macOS alike. XDG_CACHE_HOME is ignored on macOS.
            env.update(FRUST_HOME=str(self.store), FRUST_UNIT_CACHE="1", FRUST_UNIT_CACHE_DEBUG="1")
        env.update(self.extra_env)
        return env

    def build(self, app, release, target, log):
        command = [str(self.cargo), "build", "--locked", "--offline", "-j", str(self.jobs)]
        if release:
            command.append("--release")
        if self.pin:
            command = ["taskset", "-c", self.pin] + command
        if self.evict_prefixes:
            evict_page_cache(self.evict_prefixes)
        cpu, wall, rss, status = STORE.tree_cpu(command, app, self.env(target), log)
        if sys.platform == "darwin":
            rss //= 1024  # ru_maxrss is bytes on macOS and KiB on Linux
        return cpu, wall, rss, status

    def reset_store(self):
        shutil.rmtree(self.store, ignore_errors=True)
        shutil.rmtree(self.xdg_cache, ignore_errors=True)
        self.store.mkdir(parents=True)


class Edit:
    """A frozen source edit. It is reversed with the original modification times restored."""

    def __init__(self, repo, workload, kind):
        self.repo, self.kind = repo, kind
        self.patch = EDIT_DIR / f"{workload}-{kind}.patch"
        if not self.patch.is_file():
            sys.exit(f"missing edit {self.patch}")
        self.files = [repo / line[len("+++ b/"):].split("\t")[0].strip()
                      for line in self.patch.read_text().splitlines() if line.startswith("+++ b/")]
        if not self.files:
            sys.exit(f"{self.patch} names no file")

    def git(self, *extra):
        subprocess.run(["git", "-C", str(self.repo), "apply", "--recount", *extra, str(self.patch)], check=True)

    @contextlib.contextmanager
    def applied(self):
        # Reversing a patch bumps mtimes. Restoring them keeps every later copy of the clean
        # target valid, so a wide sample never also pays for rebuilding the leaf file.
        saved = {path: os.stat(path) for path in self.files}
        self.git()
        try:
            yield
        finally:
            self.git("--reverse")
            for path, stat in saved.items():
                os.utime(path, ns=(stat.st_atime_ns, stat.st_mtime_ns))


class Bench:
    def __init__(self, args, official, frust, edits):
        self.args, self.official, self.frust, self.edits = args, official, frust, edits
        self.app = pathlib.Path(args.app).resolve()
        self.work = pathlib.Path(args.work).resolve()
        self.logs = self.work / "logs"
        self.logs.mkdir(parents=True, exist_ok=True)
        self.workload = self.app.name

    def emit(self, record):
        with open(self.args.out, "a") as handle:
            handle.write(json.dumps(record, sort_keys=True) + "\n")

    def measure(self, chain, scenario, round_number, position, target):
        log = self.logs / f"{self.workload}-r{round_number}-{chain.name}-{scenario}.log"
        store_before = STORE.allocated(chain.store) if chain.uses_store else 0
        cpu, wall, rss, status = chain.build(self.app, True, target, log)
        compiled, events = log_counts(log)
        record = dict(type="sample", schema=SCHEMA, toolchain=chain.name, scenario=scenario,
                      round=round_number, position=position, app=self.workload, profile="release",
                      cpu=cpu, wall=wall, rss_kib=rss, status=status, compiled=compiled, cache_events=events,
                      target_bytes=STORE.allocated(target), store_before_bytes=store_before,
                      store_bytes=STORE.allocated(chain.store) if chain.uses_store else 0)
        self.emit(record)
        if status != 0:
            sys.exit(f"{chain.name} {scenario} failed; see {log}")
        print(f"{chain.name:8} {scenario:12} r{round_number} cpu={cpu:8.2f} wall={wall:7.2f} "
              f"compiled={compiled} store={record['store_bytes']}", flush=True)
        return record

    def noop(self, chain, round_number, position, target):
        log = self.logs / f"{self.workload}-r{round_number}-{chain.name}-noop.log"
        cpus, walls, rss_peak, compiled_max = [], [], 0, 0
        for _ in range(self.args.noops):
            cpu, wall, rss, status = chain.build(self.app, True, target, log)
            if status != 0:
                sys.exit(f"{chain.name} noop failed; see {log}")
            compiled, _ = log_counts(log)
            if self.args.noop_only and compiled:
                sys.exit(f"{chain.name} compiled during a timed no-op; see {log}")
            cpus.append(cpu)
            walls.append(wall)
            rss_peak = max(rss_peak, rss)
            compiled_max = max(compiled_max, compiled)
        record = dict(type="sample", schema=SCHEMA, toolchain=chain.name, scenario="noop",
                      round=round_number, position=position, app=self.workload, profile="release",
                      cpu=statistics.median(cpus), wall=statistics.median(walls), rss_kib=rss_peak,
                      status=0, compiled=compiled_max, runs=len(cpus), cpu_samples=cpus)
        self.emit(record)
        print(f"{chain.name:8} {'noop':12} r{round_number} cpu={record['cpu']:8.4f} "
              f"(median of {len(cpus)}, compiled={compiled_max})", flush=True)

    def sequence(self, chain, round_number, position):
        target = self.work / f"{chain.name}-target"
        scratch = self.work / f"{chain.name}-edit-target"
        shutil.rmtree(target, ignore_errors=True)
        # Nothing from an earlier round may serve the cold build.
        chain.reset_store()
        if chain.uses_store and STORE.allocated(chain.store) != 0:
            sys.exit(f"could not empty the {chain.name} store {chain.store}")
        cold = self.measure(chain, "cold", round_number, position, target)
        if chain.uses_store and cold["store_bytes"] == 0:
            sys.exit(f"the cold {chain.name} build wrote nothing to {chain.store}; the unit cache is not "
                     "using the isolated store, so cold and clean results would be wrong")
        shutil.rmtree(target, ignore_errors=True)
        self.measure(chain, "clean", round_number, position, target)
        self.noop(chain, round_number, position, target)
        for edit in self.edits:
            shutil.rmtree(scratch, ignore_errors=True)
            copy_tree(target, scratch)
            with edit.applied():
                self.measure(chain, f"edit-{edit.kind}", round_number, position, scratch)
            shutil.rmtree(scratch, ignore_errors=True)
        shutil.rmtree(target, ignore_errors=True)

    def warm_up(self, chain, target):
        log = self.logs / f"{self.workload}-{chain.name}-warm-up.log"
        cpu, wall, _, status = chain.build(self.app, True, target, log)
        self.emit(dict(type="warm-up", schema=SCHEMA, toolchain=chain.name, cpu=cpu, wall=wall, status=status))
        if status != 0:
            sys.exit(f"{chain.name} warm-up build failed; see {log}")

    def probe_folio(self, phase):
        """Record how each toolchain's driver and LLVM text is mapped during a compile (Linux only).

        Under --cache-state evicted the pages are dropped first, so the record shows the state the
        next timed build starts from. A failed probe is recorded and does not stop the run."""
        if not sys.platform.startswith("linux"):
            return
        if self.args.cache_state == "evicted":
            evict_page_cache([chain.prefix for chain in (self.official, self.frust)])
        for chain in (self.official, self.frust):
            done = subprocess.run([sys.executable, "-I", str(FOLIO_STATE), str(chain.prefix)],
                                  capture_output=True, text=True)
            record = dict(type="folio", schema=SCHEMA, phase=phase, toolchain=chain.name,
                          prefix=str(chain.prefix), cache_state=self.args.cache_state)
            lines = done.stdout.strip().splitlines()
            if done.returncode == 0 and lines:
                record.update(raw=lines[-1], mib={name: int(value) for name, value in FOLIO_FIELD.findall(lines[-1])})
                print(f"folio {phase:6} {chain.name:8} {lines[-1]}", flush=True)
            else:
                record["error"] = (done.stderr.strip() or done.stdout.strip() or f"exit {done.returncode}")[-400:]
                print(f"folio {phase:6} {chain.name:8} probe failed: {record['error']}", flush=True)
            self.emit(record)

    def rounds(self, budget, started, durations):
        for round_number in range(self.args.rounds):
            if budget and durations and (time.monotonic() - started) + 1.15 * max(durations) > budget:
                self.emit(dict(type="stop", schema=SCHEMA, reason="time budget",
                               rounds_completed=round_number, rounds_requested=self.args.rounds))
                print(f"stopping after {round_number} of {self.args.rounds} rounds: the next round would "
                      f"not fit in the {self.args.time_budget_minutes} minute budget", flush=True)
                break
            began = time.monotonic()
            order = [self.official, self.frust] if round_number % 2 == 0 else [self.frust, self.official]
            for position, chain in enumerate(order):
                if self.args.noop_only:
                    self.noop(chain, round_number, position, self.work / f"{chain.name}-target")
                else:
                    self.sequence(chain, round_number, position)
            durations.append(time.monotonic() - began)

    def run(self):
        budget = self.args.time_budget_minutes * 60
        started = time.monotonic()
        chains = (self.official, self.frust)
        try:
            if self.args.noop_only:
                for chain in chains:
                    target = self.work / f"{chain.name}-target"
                    shutil.rmtree(target, ignore_errors=True)
                    chain.reset_store()
                    self.warm_up(chain, target)
            else:
                # Unpack registry sources before the first timed build.
                target = self.work / "warm-up-target"
                try:
                    self.warm_up(self.official, target)
                finally:
                    shutil.rmtree(target, ignore_errors=True)
            self.probe_folio("before")
            try:
                self.rounds(budget, started, [])
            finally:
                self.probe_folio("after")
        finally:
            for chain in chains:
                if self.args.noop_only:
                    shutil.rmtree(self.work / f"{chain.name}-target", ignore_errors=True)
                shutil.rmtree(chain.store, ignore_errors=True)
                shutil.rmtree(chain.xdg_cache, ignore_errors=True)


def tool_versions(cargo, rustc):
    return {"cargo": capture([str(cargo), "-V"]), "rustc": capture([str(rustc), "-Vv"])}


def parse_args():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--official-cargo", required=True)
    parser.add_argument("--official-rustc", required=True)
    parser.add_argument("--expect-official-release", default="1.100.0",
                        help="the release line rustc -Vv must report for the official toolchain")
    parser.add_argument("--frust-cargo", required=True, help="fcargo of the fRust toolchain")
    parser.add_argument("--frust-rustc", required=True, help="frustc of the fRust toolchain")
    parser.add_argument("--frust-env", action="append", default=[], metavar="KEY=VALUE",
                        help="extra environment for fRust builds only")
    parser.add_argument("--frust-store", choices=("on", "off"), default="on")
    parser.add_argument("--cache-state", choices=("installed", "evicted"), default="installed",
                        help="installed: leave the toolchains' page-cache state as the installers left it; "
                             "evicted (Linux): drop both toolchains' library pages before every build")
    parser.add_argument("--app", required=True, help="cohort application directory, named after its workload")
    parser.add_argument("--label", required=True, help="platform label recorded in the results")
    parser.add_argument("--rounds", type=int, default=5)
    parser.add_argument("--noops", type=int, default=5)
    parser.add_argument("--noop-only", action="store_true", help="time adjacent pairs on warmed targets")
    parser.add_argument("--edits", default="auto", help="auto (the frozen edits of the workload), none, or a comma list")
    parser.add_argument("--cpus", default="auto", help="taskset CPU list on Linux, auto or none")
    parser.add_argument("--jobs", type=int, default=0, help="cargo -j, 0 for one per CPU in use")
    parser.add_argument("--time-budget-minutes", type=float, default=0,
                        help="start no round that would end after this many minutes, 0 for no limit")
    parser.add_argument("--work", required=True, help="scratch directory for targets, stores and logs")
    parser.add_argument("--out", required=True, help="JSON lines file, appended to")
    return parser.parse_args()


def main():
    args = parse_args()
    app = pathlib.Path(args.app).resolve()
    if not app.is_dir():
        sys.exit(f"no cohort application at {app}")
    if not tree_clean(app):
        sys.exit(f"{app} has uncommitted changes; the cohort must be the frozen tree")
    work = pathlib.Path(args.work).resolve()
    work.mkdir(parents=True, exist_ok=True)
    pathlib.Path(args.out).parent.mkdir(parents=True, exist_ok=True)

    pin, cpu_count, pin_note = plan_pinning(args.cpus)
    jobs = args.jobs or cpu_count
    extra = {}
    for item in args.frust_env:
        key, _, value = item.partition("=")
        extra[key] = value
    forbidden = [key for key in extra if key.startswith(("FRUST_HOME", "FRUST_UNIT_CACHE"))]
    if forbidden:
        sys.exit(f"{', '.join(forbidden)} is controlled by this script, not by --frust-env")

    if args.cache_state == "evicted" and not sys.platform.startswith("linux"):
        sys.exit("--cache-state evicted needs Linux (POSIX_FADV_DONTNEED on the page cache)")
    official = CiToolchain("official", args.official_cargo, args.official_rustc, work, pin, jobs, False, {})
    frust = CiToolchain("frust", args.frust_cargo, args.frust_rustc, work, pin, jobs,
                        args.frust_store == "on", extra)
    if args.cache_state == "evicted":
        for chain in (official, frust):
            chain.evict_prefixes = [official.prefix, frust.prefix]
    versions = {"official": tool_versions(args.official_cargo, args.official_rustc),
                "frust": tool_versions(args.frust_cargo, args.frust_rustc)}
    release_line = re.search(r"^release: (\S+)$", versions["official"]["rustc"] or "", re.M)
    if not release_line or release_line.group(1) != args.expect_official_release:
        sys.exit(f"official rustc reports release {release_line and release_line.group(1)}, "
                 f"expected {args.expect_official_release}")

    workload = app.name
    if args.noop_only or args.edits == "none":
        kinds = ()
    elif args.edits == "auto":
        kinds = EDITS.get(workload, ())
    else:
        kinds = tuple(part for part in args.edits.split(",") if part)
    edits = [Edit(app, workload, kind) for kind in kinds]

    lock = app / "Cargo.lock"
    host = host_info()
    meta = dict(
        type="meta", schema=SCHEMA, label=args.label, workload=workload, host=host,
        pinning=dict(cpus=pin, note=pin_note), jobs=jobs, logical_cpus_in_use=cpu_count,
        profile=PROFILE, rounds_requested=args.rounds, noops=args.noops, noop_only=args.noop_only, edits=list(kinds),
        time_budget_minutes=args.time_budget_minutes, unit_cache=args.frust_store,
        cache_state=args.cache_state, toolchain_prefixes=dict(official=str(official.prefix), frust=str(frust.prefix)),
        frust_env=extra, toolchains=versions,
        commit=os.environ.get("GITHUB_SHA") or git_head(ROOT),
        run=dict(id=os.environ.get("GITHUB_RUN_ID"), attempt=os.environ.get("GITHUB_RUN_ATTEMPT"),
                 repository=os.environ.get("GITHUB_REPOSITORY"), ref=os.environ.get("GITHUB_REF")),
        cohort=dict(commit=git_head(app), cargo_lock_sha256=hashlib.sha256(lock.read_bytes()).hexdigest()),
        utc=datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds"),
    )
    bench = Bench(args, official, frust, edits)
    bench.emit(meta)
    print(f"host: {host['cpu_model']} / {host['os'] or host['system']} / {host['logical_cpus']} logical CPUs")
    print(f"{pin_note}; cargo -j {jobs}")
    print(f"workload {workload}, edits {list(kinds)}, unit cache {args.frust_store}", flush=True)

    bench.run()
    if not tree_clean(app):
        sys.exit(f"{app} was left modified")


if __name__ == "__main__":
    main()
