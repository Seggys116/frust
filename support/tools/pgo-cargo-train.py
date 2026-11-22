#!/usr/bin/env python3
"""Train an instrumented fRust Cargo on build scenarios of rustc-perf crates and merge its raw profiles.

The scenarios are those a user (and the benchmark) runs: a cold build, a no-op rebuild, a leaf edit,
a clean rebuild that the unit cache serves, and a few metadata commands, each for one crate of the
rustc-perf compile benchmarks. Cargo runs in its fCargo mode (FRUST_INVOCATION=fcargo) with a fresh
unit cache store per crate. Only Cargo is instrumented: the compiler that it drives is the plain one
given with --rustc, and dependencies are fetched by the plain --fetch-cargo, so the raw profiles are
Cargo's and the training time is mostly Cargo's own. Raw profiles are merged into one indexed profile
as they appear and deleted, as in pgo-rustc-train.py, whose merging and checks this tool shares.
Exit status 0 means the profile passed every gate; the evidence (key train_cargo) is written either way.

Usage: pgo-cargo-train.py --cargo C --fetch-cargo F --rustc R --perf-dir D --llvm-profdata P --crates a,b
       --work W --output O --evidence E [--jobs N] [--noops N] [--budget-minutes M]
       [--unit-timeout-minutes U] [--min-crates K] [--min-files F] [--min-functions N]
       [--exclude-cohorts qualification/cohorts.lock]
"""

import argparse
import importlib.util
import json
import os
import shutil
import signal
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("pgo_rustc_train", HERE / "pgo-rustc-train.py")
common = importlib.util.module_from_spec(spec)
spec.loader.exec_module(common)


def perf_config(crate_dir):
    path = crate_dir / "perf-config.json"
    return json.loads(path.read_text()) if path.exists() else {}


class Unit:
    """One crate: its scenarios run in order, each a Cargo invocation that must exit 0."""

    def __init__(self, args, merger, crate, raw, logs, deadline):
        self.args, self.merger, self.crate, self.raw, self.logs, self.deadline = args, merger, crate, raw, logs, deadline
        self.source = Path(args.work) / "src" / crate
        self.config = perf_config(Path(args.perf_dir) / "collector" / "compile-benchmarks" / crate)
        self.options = self.config.get("cargo_opts", "").split()
        self.target = Path(args.work) / "target" / crate
        self.store = Path(args.work) / "store" / crate
        self.invocations = 0
        self.log = open(logs / f"{crate}.log", "wb")

    def env(self, profiled):
        env = {key: value for key, value in os.environ.items()
               if not key.startswith(("CARGO", "RUSTC", "RUSTFLAGS", "FRUST", "XDG", "LLVM_PROFILE"))}
        env.update(CARGO_HOME=str(Path(self.args.work) / "cargo-home"), CARGO_TARGET_DIR=str(self.target),
                   CARGO_TERM_COLOR="never", CARGO_INCREMENTAL="0", FRUST_PROGRESS="plain",
                   XDG_CACHE_HOME=str(self.store), XDG_CONFIG_HOME=str(Path(self.args.work) / "xdg-config"),
                   RUSTC=self.args.rustc)
        if profiled:
            env.update(FRUST_INVOCATION="fcargo", LLVM_PROFILE_FILE=str(self.raw / "cargo_%m_%p.profraw"))
        return env

    def run(self, label, cargo, command, profiled=True):
        """Run one invocation under the unit timeout, merging raw profiles while it runs."""
        self.log.write(f"\n$ {label}: {' '.join(command)}\n".encode())
        self.log.flush()
        started = time.monotonic()
        proc = subprocess.Popen([cargo, *command], cwd=self.source, env=self.env(profiled), stdout=self.log,
                                stderr=subprocess.STDOUT, start_new_session=True)
        reason = ""
        while True:
            try:
                status = proc.wait(timeout=2.0)
                break
            except subprocess.TimeoutExpired:
                pass
            self.merger.sweep(final=False)
            if time.monotonic() - started > self.args.unit_timeout_minutes * 60:
                reason = f"{label} timed out"
            elif time.monotonic() > self.deadline:
                reason = f"{label} stopped by the budget"
            elif shutil.disk_usage(self.raw).free < common.MIN_FREE_BYTES:
                reason = f"{label} stopped by low disk"
            if reason:
                try:
                    os.killpg(proc.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                status = proc.wait()
                break
        self.merger.sweep(final=True)
        self.invocations += 1
        if reason:
            return reason
        if status != 0:
            return f"{label} exited {status}"
        return ""

    def edit(self):
        touched = self.config.get("touch_file")
        candidates = [touched] if touched else ["src/lib.rs", "src/main.rs"]
        for name in candidates:
            path = self.source / name
            if path.is_file():
                with open(path, "ab") as handle:
                    handle.write(b"\n// fRust PGO training edit\n")
                return ""
        return f"no source file to edit among {candidates}"

    def scenarios(self):
        a = self.args
        cargo, fetch = a.cargo, a.fetch_cargo
        build = ["build", "--locked", "--offline", "-j", str(a.jobs), *self.options]
        steps = [("fetch", fetch, ["fetch", "--locked"], False)]
        for profile in ([], ["--release"]):
            tag = "release" if profile else "dev"
            steps += [(f"cold {tag}", cargo, build + profile, True)]
            steps += [(f"no-op {tag} {i}", cargo, build + profile, True) for i in range(a.noops)]
            steps += [("edit", None, None, True), (f"leaf edit {tag}", cargo, build + profile, True)]
            steps += [(f"no-op after edit {tag}", cargo, build + profile, True), ("clean", None, None, True)]
            steps += [(f"clean rebuild {tag}", cargo, build + profile, True)]
            steps += [(f"no-op after clean {tag} {i}", cargo, build + profile, True) for i in range(a.noops)]
        steps += [("metadata", cargo, ["metadata", "--locked", "--offline", "--format-version", "1"], True),
                  ("tree", cargo, ["tree", "--locked", "--offline", "-e", "normal"], True),
                  ("check", cargo, ["check", "--locked", "--offline", "-j", str(a.jobs), *self.options], True)]
        return steps

    def execute(self):
        started = time.monotonic()
        shutil.rmtree(self.source, ignore_errors=True)
        shutil.copytree(Path(self.args.perf_dir) / "collector" / "compile-benchmarks" / self.crate, self.source)
        files_before, size_before = self.merger.files, self.merger.size
        reason = ""
        for label, cargo, command, profiled in self.scenarios():
            if label == "edit":
                reason = self.edit()
            elif label == "clean":
                shutil.rmtree(self.target, ignore_errors=True)
            else:
                reason = self.run(label, cargo, command, profiled)
            if reason:
                break
        self.log.close()
        seen = self.merger.files - files_before
        if not reason and seen == 0:
            reason = "no raw profile was written"
        shutil.rmtree(self.source, ignore_errors=True)
        shutil.rmtree(self.target, ignore_errors=True)
        shutil.rmtree(self.store, ignore_errors=True)
        return {"crate": self.crate, "ok": not reason, "reason": reason, "seconds": round(time.monotonic() - started, 1),
                "invocations": self.invocations, "raw_files": seen, "raw_bytes": self.merger.size - size_before,
                "log": f"{self.crate}.log"}


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    for name in ("cargo", "fetch-cargo", "rustc", "perf-dir", "llvm-profdata", "crates", "work", "output", "evidence"):
        parser.add_argument(f"--{name}", required=True)
    parser.add_argument("--jobs", type=int, default=2)
    parser.add_argument("--noops", type=int, default=4)
    parser.add_argument("--budget-minutes", type=float, default=30)
    parser.add_argument("--unit-timeout-minutes", type=float, default=15)
    parser.add_argument("--min-crates", type=int, default=3)
    parser.add_argument("--min-files", type=int, default=20)
    parser.add_argument("--min-functions", type=int, default=5000)
    parser.add_argument("--exclude-cohorts", default="")
    args = parser.parse_args()
    for name in ("cargo", "fetch_cargo", "rustc", "perf_dir", "llvm_profdata"):
        setattr(args, name, os.path.abspath(getattr(args, name)))
    work = Path(os.path.abspath(args.work))
    args.work = str(work)
    if work.exists():
        sys.exit(f"work directory exists: {work}")
    raw, logs = work / "raw", work / "logs"
    raw.mkdir(parents=True)
    logs.mkdir()
    crates = [crate for crate in args.crates.split(",") if crate]
    problems = common.check_crates(args, crates)
    if problems or not crates:
        sys.exit("bad training set: " + "; ".join(problems or ["no crates"]))

    merger = common.Merger(args.llvm_profdata, raw, work / "merged.profdata")
    started = time.monotonic()
    deadline = started + args.budget_minutes * 60
    units, skipped, stop = [], [], ""
    for crate in crates:
        if not stop and time.monotonic() >= deadline:
            stop = "budget"
        if not stop and shutil.disk_usage(work).free < 2 * common.MIN_FREE_BYTES:
            stop = "disk"
        if stop:
            skipped.append(crate)
            continue
        unit = Unit(args, merger, crate, raw, logs, deadline).execute()
        units.append(unit)
        print(f"{'ok  ' if unit['ok'] else 'FAIL'} {crate} {unit['seconds']}s {unit['invocations']} runs "
              f"{unit['raw_files']} raw files {unit['raw_bytes'] // (1 << 20)} MiB {unit['reason']}", flush=True)
        if not unit["ok"]:
            common.note("warning", f"Cargo PGO training {crate}", f"{unit['reason']}\n{common.tail(logs / unit['log'])}")

    done = sorted(unit["crate"] for unit in units if unit["ok"])
    merged = work / "merged.profdata"
    summary = common.profile_summary(args.llvm_profdata, merged)
    gates = []
    if len(done) < args.min_crates:
        gates.append(f"only {len(done)} crate(s) trained completely, {args.min_crates} required")
    if merger.files < args.min_files:
        gates.append(f"only {merger.files} raw profile file(s) were observed, {args.min_files} required")
    if merger.failed_merges or merger.bad > common.MAX_BAD_FRACTION * max(merger.files, 1):
        gates.append(f"{merger.bad} of {merger.files} raw profile file(s) could not be merged "
                     f"({merger.failed_merges} failed merge call(s)): {merger.errors[:2]}")
    if summary.get("level") != "IR":
        gates.append(f"merged profile instrumentation level is {summary.get('level')}, expected IR")
    if summary.get("functions", 0) < args.min_functions:
        gates.append(f"merged profile holds {summary.get('functions', 0)} function(s), {args.min_functions} required")
    if summary.get("max_function_count", 0) == 0:
        gates.append("merged profile has no non-zero function count")
    evidence = {
        "crates_requested": crates, "crates_complete": done, "stopped_by": stop, "skipped_crates": skipped,
        "units": units, "raw_files": merger.files, "raw_bytes": merger.size, "bad_raw_files": merger.bad,
        "failed_merges": merger.failed_merges, "merged": summary, "gates_failed": gates,
        "seconds": round(time.monotonic() - started, 1),
    }
    common.update_evidence(args.evidence, "train_cargo", evidence)
    if gates:
        for gate in gates:
            common.note("error", "Cargo PGO training gate", gate)
        sys.exit(1)
    output = Path(os.path.abspath(args.output))
    output.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(merged, output)
    print(f"merged {merger.files} raw profiles ({merger.size // (1 << 20)} MiB) of {len(done)} crate(s) into {output} "
          f"({summary['functions']} functions)", flush=True)


if __name__ == "__main__":
    main()
