#!/usr/bin/env python3
"""Measure one cold Cargo build under an optional cgroup memory limit.

For each round the build runs in a fresh systemd scope (`systemd-run --user --scope`) with
MemorySwapMax=0 and, when --memory-max is given, MemoryMax, so the limit is a hard bound. A
fresh CARGO_TARGET_DIR is used and the frozen release profile of ci-bench.py is applied.

Recorded per round, one JSON line each:
  cpu_s        user plus system seconds of the cargo process tree, from wait4
  maxrss_kib   ru_maxrss from wait4: the largest single process in the tree, not their sum
  cgroup_peak  memory.peak of the scope: the highest concurrent charge, page cache included
  oom_kill     memory.events oom_kill count of the scope; a nonzero value means the limit was hit
  exit         cargo's exit status
  reported_jobs the count named in cargo's verbose note when it lowered the implicit default,
               otherwise null

To compare the implicit default with the CPU count under the same limit, run once with
`--jobs implicit` (no -j, CARGO_BUILD_JOBS unset) and once with `--jobs N` where N is the CPU count.
To size the per-job budget without a limit, run `--jobs 1`, `--jobs 4` and `--jobs N` and compare
cgroup_peak.

Limits: Linux with cgroup v2, a delegated memory controller for the user manager and systemd-run
are required; macOS has no cgroups and is not supported. cgroup_peak includes reclaimable page
cache, so an unconstrained run overstates what the build needs. One crate graph is one data point.
"""
import argparse
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile

sys.dont_write_bytecode = True

# The frozen profile of qualification/ci-bench.py.
PROFILE = {
    "CARGO_PROFILE_RELEASE_OPT_LEVEL": "3",
    "CARGO_PROFILE_RELEASE_LTO": "fat",
    "CARGO_PROFILE_RELEASE_CODEGEN_UNITS": "1",
    "CARGO_PROFILE_RELEASE_INCREMENTAL": "true",
    "CARGO_PROFILE_RELEASE_DEBUG": "0",
    "CARGO_PROFILE_RELEASE_STRIP": "none",
}
JOBS_NOTE = re.compile(r"using (\d+) parallel jobs instead of (\d+)")


def own_cgroup():
    for line in Path("/proc/self/cgroup").read_text().splitlines():
        if line.startswith("0::"):
            return Path("/sys/fs/cgroup") / line[3:].lstrip("/")
    sys.exit("no cgroup v2 hierarchy for this process")


def read_int(path):
    try:
        return int(path.read_text().strip())
    except (OSError, ValueError):
        return None


def read_event(path, key):
    try:
        for line in path.read_text().splitlines():
            name, _, value = line.partition(" ")
            if name == key:
                return int(value)
    except (OSError, ValueError):
        pass
    return None


def inner(args):
    """Runs inside the scope: build once, then read the scope's own counters."""
    work = args.work
    try:
        env = dict(os.environ)
        env.pop("CARGO_INCREMENTAL", None)
        env.pop("CARGO_BUILD_JOBS", None)
        env.update(PROFILE)
        env["CARGO_TARGET_DIR"] = str(work / "target")
        command = [args.cargo, "build", "--release", "--locked", "--offline", "-v"]
        if args.jobs != "implicit":
            command += ["-j", args.jobs]
        command += args.cargo_args
        if args.cpus:
            command = ["taskset", "-c", args.cpus] + command
        if args.rustc:
            env["RUSTC"] = args.rustc
        log = work / "build.log"
        with log.open("wb") as sink:
            child = subprocess.Popen(command, cwd=args.manifest_dir, env=env,
                                     stdout=sink, stderr=subprocess.STDOUT)
            _, status, usage = os.wait4(child.pid, 0)
        child.returncode = os.waitstatus_to_exitcode(status)
        scope = own_cgroup()
        noted = JOBS_NOTE.search(log.read_text(errors="replace"))
        print(json.dumps({
            "cpu_s": round(usage.ru_utime + usage.ru_stime, 3),
            "maxrss_kib": usage.ru_maxrss,
            "cgroup_peak": read_int(scope / "memory.peak"),
            "oom_kill": read_event(scope / "memory.events", "oom_kill"),
            "exit": child.returncode,
            "reported_jobs": int(noted.group(1)) if noted else None,
        }), flush=True)
    finally:
        shutil.rmtree(work, ignore_errors=True)


def outer(args):
    if not shutil.which("systemd-run"):
        sys.exit("systemd-run is required")
    properties = ["-p", "MemorySwapMax=0", "-p", "OOMPolicy=continue"]
    if args.memory_max:
        properties += ["-p", f"MemoryMax={args.memory_max}"]
    results = []
    args.work_root.mkdir(parents=True, exist_ok=True)
    for round_no in range(1, args.rounds + 1):
        command = ["systemd-run", "--user", "--scope", "--quiet", "--collect"] + properties + [
            sys.executable, "-I", str(Path(__file__).resolve()), "--inner",
            "--cargo", args.cargo, "--jobs", args.jobs,
            "--manifest-dir", args.manifest_dir,
        ]
        if args.cpus:
            command += ["--cpus", args.cpus]
        if args.rustc:
            command += ["--rustc", args.rustc]
        with tempfile.TemporaryDirectory(prefix="memory-jobs-", dir=args.work_root) as work:
            command += ["--work", work, "--"] + args.cargo_args
            done = subprocess.run(command, stdout=subprocess.PIPE, text=True)
        lines = [line for line in done.stdout.splitlines() if line.startswith("{")]
        if done.returncode != 0 or not lines:
            sys.exit(f"round {round_no}: scope failed with status {done.returncode}")
        record = json.loads(lines[-1])
        record.update(round=round_no, memory_max=args.memory_max, jobs=args.jobs, cpus=args.cpus)
        results.append(record)
        print(json.dumps(record), flush=True)
    if args.out:
        with open(args.out, "a") as out:
            for record in results:
                out.write(json.dumps(record) + "\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--manifest-dir", required=True, help="directory of the crate or workspace to build")
    parser.add_argument("--cargo", default="cargo", help="Cargo executable (default: cargo)")
    parser.add_argument("--rustc", help="RUSTC to build with (default: Cargo's own choice)")
    parser.add_argument("--memory-max", help="scope MemoryMax, e.g. 4G; default: no limit")
    parser.add_argument("--jobs", default="implicit",
                        help="'implicit' (no -j) or a positive integer for -j (default: implicit)")
    parser.add_argument("--cpus", help="taskset CPU list, e.g. 0-15; default: not pinned")
    parser.add_argument("--rounds", type=int, default=3)
    parser.add_argument("--out", help="append the JSON lines to this file")
    parser.add_argument("--work-root", type=Path,
                        default=Path(__file__).resolve().parents[1] / "target" / "memory-jobs",
                        help="directory for temporary build output")
    parser.add_argument("--work", type=Path, help=argparse.SUPPRESS)
    parser.add_argument("--inner", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("cargo_args", nargs="*", help="extra arguments after --, e.g. -p ripgrep")
    args = parser.parse_args()
    if args.jobs != "implicit" and not args.jobs.isdigit():
        parser.error("--jobs must be 'implicit' or a positive integer")
    if args.inner and args.work is None:
        parser.error("the scope needs a work directory")
    inner(args) if args.inner else outer(args)


if __name__ == "__main__":
    main()
