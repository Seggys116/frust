#!/usr/bin/env python3
"""Collect paired control and fRust build samples for one workload and edit kind.

Every pair times both toolchains through time-build.sh and alternates which one
goes first. Edit samples start from a copy of an incremental target seeded with
the unmodified tree, so reapplying a patch never reuses an earlier patched build.
Cold samples start from an empty target. The trace kind runs the fixed edit
trace once per toolchain and records peak RSS and allocated target bytes.

Samples are appended to a file in the format `frust-gates --samples` reads.
"""

import argparse
import os
import pathlib
import re
import shutil
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
TIME_BUILD = ROOT / "qualification" / "time-build.sh"
EDIT_KINDS = ("leaf", "wide", "holdout", "noop")
TRACES = {"fd": ("holdout",), "default": ("leaf", "wide")}


def parse_args():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--control", required=True, type=pathlib.Path, help="control toolchain bin directory")
    parser.add_argument("--frust", required=True, type=pathlib.Path, help="fRust toolchain bin directory")
    parser.add_argument("--workload", required=True, choices=("ripgrep", "syn", "regex", "fd"))
    parser.add_argument("--kind", required=True, choices=EDIT_KINDS + ("cold", "trace"))
    parser.add_argument("--pairs", type=int, default=30)
    parser.add_argument("--work", required=True, type=pathlib.Path, help="scratch directory for targets and logs")
    parser.add_argument("--samples", required=True, type=pathlib.Path, help="file the sample lines are appended to")
    parser.add_argument("--frust-env", action="append", default=[], metavar="KEY=VALUE")
    return parser.parse_args()


class Runner:
    def __init__(self, args):
        self.args = args
        self.bins = {"control": args.control.resolve(), "frust": args.frust.resolve()}
        self.env = {"control": dict(os.environ), "frust": dict(os.environ)}
        for item in args.frust_env:
            key, _, value = item.partition("=")
            self.env["frust"][key] = value
        self.logs = args.work / "logs"
        self.logs.mkdir(parents=True, exist_ok=True)
        self.count = 0

    def sample(self, toolchain, kind, target):
        """Run one timed build and return (wall seconds, peak RSS KiB)."""
        self.count += 1
        log = self.logs / f"{self.count:04d}-{self.args.workload}-{kind}-{toolchain}.log"
        command = [str(TIME_BUILD), str(self.bins[toolchain]), self.args.workload, kind, str(target), str(log)]
        result = subprocess.run(command, env=self.env[toolchain], capture_output=True, text=True)
        text = log.read_text(errors="replace") if log.exists() else ""
        wall = re.search(r"^WALL_SECONDS:([0-9.]+)$", text, re.M)
        rss = re.search(r"^PEAK_RSS_KIB:([0-9]+)$", text, re.M)
        if result.returncode != 0 or not wall or not rss or not text.rstrip().endswith("EXIT:0"):
            tail = "\n".join(text.splitlines()[-30:])
            sys.exit(f"{toolchain} {kind} sample failed ({result.returncode})\n{result.stderr}\n{tail}")
        return float(wall.group(1)), int(rss.group(1))

    def seed(self, toolchain):
        target = self.args.work / f"seed-{toolchain}"
        self.sample(toolchain, "seed", target)
        return target


def copy_tree(source, dest):
    subprocess.run(["cp", "-a", "--reflink=auto", str(source), str(dest)], check=True)


def allocated_bytes(path):
    out = subprocess.run(["du", "-sk", str(path)], check=True, capture_output=True, text=True).stdout
    return int(out.split()[0]) * 1024


def run_edit_pairs(runner, seeds):
    args = runner.args
    scratch = args.work / "run"
    for index in range(args.pairs):
        order = ("control", "frust") if index % 2 == 0 else ("frust", "control")
        wall = {}
        for toolchain in order:
            shutil.rmtree(scratch, ignore_errors=True)
            if args.kind != "cold":
                copy_tree(seeds[toolchain], scratch)
            wall[toolchain], _ = runner.sample(toolchain, args.kind, scratch)
            shutil.rmtree(scratch, ignore_errors=True)
        with args.samples.open("a") as out:
            out.write(f"edit {args.workload} {args.kind} {wall['frust']:.9f} {wall['control']:.9f}\n")
        print(f"pair {index + 1}/{args.pairs} control {wall['control']:.3f}s frust {wall['frust']:.3f}s", flush=True)


def run_trace(runner, seeds):
    args = runner.args
    edits = TRACES.get(args.workload, TRACES["default"])
    rss = {}
    size = {}
    for toolchain in ("control", "frust"):
        target = args.work / f"trace-{toolchain}"
        copy_tree(seeds[toolchain], target)
        peaks = []
        # The final noop build is the normal rebuild after the reversed edits.
        for kind in edits + ("noop",):
            peaks.append(runner.sample(toolchain, kind, target)[1])
        rss[toolchain] = max(peaks)
        size[toolchain] = allocated_bytes(target)
        shutil.rmtree(target, ignore_errors=True)
    with args.samples.open("a") as out:
        out.write(f"resource {args.workload} {rss['frust']} {rss['control']} {size['frust']} {size['control']}\n")
    print(f"trace rss control {rss['control']} frust {rss['frust']} bytes control {size['control']} frust {size['frust']}")


def main():
    args = parse_args()
    args.work.mkdir(parents=True, exist_ok=True)
    runner = Runner(args)
    seeds = {}
    if args.kind != "cold":
        seeds = {toolchain: runner.seed(toolchain) for toolchain in ("control", "frust")}
    if args.kind == "trace":
        run_trace(runner, seeds)
    else:
        run_edit_pairs(runner, seeds)


if __name__ == "__main__":
    main()
