#!/usr/bin/env python3
"""Train an instrumented stage-2 rustc with rustc-perf and merge its raw profiles as they appear.

A unit is one collector run: one benchmark crate with one build profile. Raw profiles are merged into
one indexed profile after each sweep and then deleted, so the disk holds a few seconds of raw data
and not the many gigabytes that one file per rustc process adds up to. Exit status 0 means the
profile passed every gate (the gates_failed list of the evidence); the evidence file is written
either way.

Usage: pgo-rustc-train.py --rustc R --cargo C --collector X --perf-dir D --llvm-profdata P --host T
       --crates a,b --work W --output O --evidence E [--profiles Check,Debug,Opt] [--scenarios All]
       [--jobs N] [--budget-minutes M] [--unit-timeout-minutes U] [--min-crates K] [--min-files F]
       [--min-functions N] [--exclude-cohorts qualification/cohorts.lock]
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
from pathlib import Path

BATCH = 64
SETTLE_SECONDS = 4.0
MIN_FREE_BYTES = 1 << 30
MAX_BAD_FRACTION = 0.02


def note(kind, title, message):
    print(f"::{kind} title={title}::{message}", flush=True)


def tail(path, lines=40):
    try:
        return "\n".join(Path(path).read_text(errors="replace").splitlines()[-lines:])
    except OSError:
        return ""


def update_evidence(path, key, value):
    path = Path(path)
    data = json.loads(path.read_text()) if path.exists() else {}
    data[key] = value
    path.write_text(json.dumps(data, indent=2, sort_keys=True) + "\n")


class Merger:
    """Folds settled .profraw files of one flat directory into one indexed profile."""

    def __init__(self, tool, raw, merged):
        self.tool, self.raw, self.merged = tool, raw, merged
        self.files = self.size = self.bad = self.failed_merges = 0
        self.errors = []

    def sweep(self, final):
        now = time.time()
        ready = []
        for path in sorted(self.raw.glob("*.profraw")):
            try:
                stat = path.stat()
            except OSError:
                continue
            if final or now - stat.st_mtime >= SETTLE_SECONDS:
                ready.append((path, stat.st_size))
        for start in range(0, len(ready), BATCH):
            self.merge(ready[start:start + BATCH])

    def merge(self, batch):
        usable = []
        for path, size in batch:
            self.files += 1
            self.size += size
            if size:
                usable.append(path)
            else:
                self.bad += 1
        if usable:
            listing = self.raw.parent / "merge-input.txt"
            entries = ([self.merged] if self.merged.exists() else []) + usable
            listing.write_text("".join(f"{entry}\n" for entry in entries))
            out = self.merged.with_name(self.merged.name + ".next")
            proc = subprocess.run(
                [self.tool, "merge", "--failure-mode=all", "-o", str(out), f"--input-files={listing}"],
                capture_output=True, text=True)
            if proc.returncode == 0 and out.exists():
                self.bad += sum(1 for line in proc.stderr.splitlines() if ".profraw" in line)
                os.replace(out, self.merged)
            else:
                self.failed_merges += 1
                self.bad += len(usable)
                self.errors.append(proc.stderr.strip()[-400:])
                out.unlink(missing_ok=True)
        for path, _ in batch:
            path.unlink(missing_ok=True)


def run_unit(args, merger, crate, profile, raw, logs):
    """Run the collector for one crate and profile, merging while it runs. Returns the unit record."""
    log = logs / f"{crate}-{profile}.log"
    command = [
        str(args.collector), "profile_local", "eprintln", str(args.rustc), "--id", "Test",
        "--cargo", str(args.cargo), "--profiles", profile, "--scenarios", args.scenarios,
        "--exact-match", crate, "--frontend-threads", "1", "--jobs", str(args.jobs),
    ]
    env = dict(os.environ, RUSTC_BOOTSTRAP="1", LLVM_PROFILE_FILE=str(raw / "default_%m_%p.profraw"))
    files_before, size_before = merger.files, merger.size
    started = time.monotonic()
    reason = ""
    with open(log, "wb") as handle:
        try:
            proc = subprocess.Popen(command, cwd=args.perf_dir, env=env, stdout=handle, stderr=subprocess.STDOUT,
                                    start_new_session=True)
        except OSError as error:
            return {"crate": crate, "profile": profile, "ok": False, "reason": f"cannot start the collector: {error}",
                    "seconds": 0.0, "raw_files": 0, "raw_bytes": 0, "log": log.name}
        while True:
            try:
                status = proc.wait(timeout=2.0)
                break
            except subprocess.TimeoutExpired:
                pass
            merger.sweep(final=False)
            if time.monotonic() - started > args.unit_timeout_minutes * 60:
                reason = "timeout"
            elif shutil.disk_usage(raw).free < MIN_FREE_BYTES:
                reason = "disk"
            if reason:
                try:
                    os.killpg(proc.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                status = proc.wait()
                break
    merger.sweep(final=True)
    seen = merger.files - files_before
    if not reason and status != 0:
        reason = f"exit {status}"
    if not reason and seen == 0:
        reason = "no raw profile was written"
    return {
        "crate": crate, "profile": profile, "ok": not reason, "reason": reason,
        "seconds": round(time.monotonic() - started, 1), "raw_files": seen,
        "raw_bytes": merger.size - size_before, "log": log.name,
    }


def profile_summary(tool, merged):
    if not merged.exists():
        return {}
    text = subprocess.run([tool, "show", str(merged)], capture_output=True, text=True).stdout

    def field(pattern):
        match = re.search(pattern, text)
        return match.group(1) if match else None

    functions, top = field(r"Total functions:\s*(\d+)"), field(r"Maximum function count:\s*(\d+)")
    return {"level": field(r"Instrumentation level:\s*(\w+)"),
            "functions": int(functions) if functions else 0, "max_function_count": int(top) if top else 0}


def check_crates(args, crates):
    problems = []
    cohorts = []
    if args.exclude_cohorts:
        for line in Path(args.exclude_cohorts).read_text().splitlines():
            if line.strip() and not line.startswith("#"):
                cohorts.append(line.split()[0])
    bench = Path(args.perf_dir) / "collector" / "compile-benchmarks"
    for crate in crates:
        if any(crate == name or crate.startswith(name + "-") for name in cohorts):
            problems.append(f"{crate} belongs to a measured cohort")
        if not (bench / crate).is_dir():
            problems.append(f"{crate} is not a directory of {bench}")
    return problems


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    for name in ("rustc", "cargo", "collector", "perf-dir", "llvm-profdata", "host", "crates", "work", "output",
                 "evidence"):
        parser.add_argument(f"--{name}", required=True)
    parser.add_argument("--profiles", default="Check,Debug,Opt")
    parser.add_argument("--scenarios", default="All")
    parser.add_argument("--jobs", type=int, default=2)
    parser.add_argument("--budget-minutes", type=float, default=80)
    parser.add_argument("--unit-timeout-minutes", type=float, default=25)
    parser.add_argument("--min-crates", type=int, default=4)
    parser.add_argument("--min-files", type=int, default=50)
    parser.add_argument("--min-functions", type=int, default=20000)
    parser.add_argument("--exclude-cohorts", default="")
    args = parser.parse_args()
    for name in ("rustc", "cargo", "collector", "perf_dir", "llvm_profdata"):
        setattr(args, name, os.path.abspath(getattr(args, name)))
    work = Path(os.path.abspath(args.work))
    if work.exists():
        sys.exit(f"work directory exists: {work}")
    raw, logs = work / "raw", work / "logs"
    raw.mkdir(parents=True)
    logs.mkdir()
    crates = [crate for crate in args.crates.split(",") if crate]
    profiles = [profile for profile in args.profiles.split(",") if profile]
    problems = check_crates(args, crates)
    if problems or not crates or not profiles:
        sys.exit("bad training set: " + "; ".join(problems or ["no crates or profiles"]))

    host = re.search(r"^host: (\S+)$", subprocess.run([args.rustc, "-vV"], capture_output=True, text=True).stdout, re.M)
    if not host or host.group(1) != args.host:
        sys.exit(f"the instrumented rustc reports host {host and host.group(1)}, expected {args.host}")
    tool = args.llvm_profdata
    version = subprocess.run([tool, "--version"], capture_output=True, text=True).stdout.strip().splitlines()
    merged = work / "merged.profdata"
    merger = Merger(tool, raw, merged)

    started = time.monotonic()
    units, skipped, stop = [], [], ""
    for crate in crates:
        for profile in profiles:
            if not stop and time.monotonic() - started >= args.budget_minutes * 60:
                stop = "budget"
            if not stop and shutil.disk_usage(work).free < 2 * MIN_FREE_BYTES:
                stop = "disk"
            if stop:
                skipped.append(f"{crate}:{profile}")
                continue
            unit = run_unit(args, merger, crate, profile, raw, logs)
            units.append(unit)
            print(f"{'ok  ' if unit['ok'] else 'FAIL'} {crate} {profile} {unit['seconds']}s "
                  f"{unit['raw_files']} raw files {unit['raw_bytes'] // (1 << 20)} MiB {unit['reason']}", flush=True)
            if not unit["ok"]:
                note("warning", f"PGO training {crate} {profile}", f"{unit['reason']}\n{tail(logs / unit['log'])}")

    done = {crate for crate in crates if all(any(u["crate"] == crate and u["profile"] == p and u["ok"] for u in units)
                                             for p in profiles)}
    summary = profile_summary(tool, merged)
    gates = []
    if len(done) < args.min_crates:
        gates.append(f"only {len(done)} crate(s) trained in every profile, {args.min_crates} required")
    if merger.files < args.min_files:
        gates.append(f"only {merger.files} raw profile file(s) were observed, {args.min_files} required")
    if merger.failed_merges or merger.bad > MAX_BAD_FRACTION * max(merger.files, 1):
        gates.append(f"{merger.bad} of {merger.files} raw profile file(s) could not be merged "
                     f"({merger.failed_merges} failed merge call(s)): {merger.errors[:2]}")
    if summary.get("level") != "IR":
        gates.append(f"merged profile instrumentation level is {summary.get('level')}, expected IR")
    if summary.get("functions", 0) < args.min_functions:
        gates.append(f"merged profile holds {summary.get('functions', 0)} function(s), {args.min_functions} required")
    if summary.get("max_function_count", 0) == 0:
        gates.append("merged profile has no non-zero function count")

    evidence = {
        "host": args.host, "machine": os.uname().machine, "llvm_profdata": version[:2],
        "crates_requested": crates, "crates_complete": sorted(done), "profiles": profiles,
        "scenarios": args.scenarios, "jobs": args.jobs, "budget_minutes": args.budget_minutes,
        "stopped_by": stop, "skipped_units": skipped, "units": units,
        "raw_files": merger.files, "raw_bytes": merger.size, "bad_raw_files": merger.bad,
        "failed_merges": merger.failed_merges, "merged": summary, "gates_failed": gates,
        "seconds": round(time.monotonic() - started, 1),
    }
    update_evidence(args.evidence, "train", evidence)
    if gates:
        for gate in gates:
            note("error", "PGO training gate", gate)
        sys.exit(1)
    output = Path(os.path.abspath(args.output))
    output.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(merged, output)
    print(f"merged {merger.files} raw profiles ({merger.size // (1 << 20)} MiB) of {len(done)} crate(s) into {output} "
          f"({summary['functions']} functions)", flush=True)


if __name__ == "__main__":
    main()
