#!/usr/bin/env python3
"""Collect paired control and fRust build samples for one workload and edit kind.

Every pair times both toolchains through time-build.sh and alternates which one
goes first. Edit samples start from a copy of an incremental target seeded with
the unmodified tree, so reapplying a patch never reuses an earlier patched build.
Cold samples start from an empty target. The trace kind runs the fixed edit
trace once per toolchain and records peak RSS and allocated target bytes.

The fRust side runs with the Cargo unit cache on (--unit-cache) and a store that
this script owns: FRUST_HOME points into --store-root, so the store never sits in
the runner's default cache directory and cannot carry entries from another pair,
series or job. The state at each sample is part of the protocol:

  cold     the store is deleted and checked empty before every fRust sample
  clean    each toolchain first builds the unmodified tree into an empty target
           (the fRust store is emptied before that build and filled by it), the
           target is removed, and the timed rebuild runs with the store warm
  others   the store is emptied once, before the seed build, and stays warm

The control toolchain has no store. Ambient FRUST_HOME and FRUST_UNIT_CACHE*
variables are dropped from both sides.

Samples are appended to a file in the format `frust-gates --samples` reads. The
clean series is not a gate input, so it is written only to the optional --jsonl
file, which also records every timed build with the store size around it.
"""

import argparse
import datetime
import json
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
STORE_VARS = ("FRUST_HOME", "FRUST_UNIT_CACHE")


def parse_args():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--control", required=True, type=pathlib.Path, help="control toolchain bin directory")
    parser.add_argument("--frust", required=True, type=pathlib.Path, help="fRust toolchain bin directory")
    parser.add_argument("--workload", required=True, choices=("ripgrep", "syn", "regex", "fd"))
    parser.add_argument("--kind", required=True, choices=EDIT_KINDS + ("cold", "clean", "trace"))
    parser.add_argument("--pairs", type=int, default=30)
    parser.add_argument("--work", required=True, type=pathlib.Path, help="scratch directory for targets and logs")
    parser.add_argument("--samples", required=True, type=pathlib.Path, help="file the sample lines are appended to")
    parser.add_argument("--jsonl", type=pathlib.Path, help="file one JSON record per timed build is appended to")
    parser.add_argument("--unit-cache", choices=("on", "off"), default="on",
                        help="Cargo unit cache for the fRust side (default on, as shipped under fcargo)")
    parser.add_argument("--store-root", type=pathlib.Path,
                        help="directory that holds the fRust store (default <work>/store)")
    parser.add_argument("--frust-env", action="append", default=[], metavar="KEY=VALUE")
    args = parser.parse_args()
    args.store_root = args.store_root or args.work / "store"
    return args


def append_json(path, record):
    with path.open("a") as out:
        out.write(json.dumps(record, sort_keys=True) + "\n")


class Runner:
    def __init__(self, args):
        self.args = args
        self.bins = {"control": args.control.resolve(), "frust": args.frust.resolve()}
        ambient = {key: value for key, value in os.environ.items() if not key.startswith(STORE_VARS)}
        self.env = {"control": dict(ambient), "frust": dict(ambient)}
        self.store = args.store_root.resolve() / "frust-home"
        self.env["frust"]["FRUST_HOME"] = str(self.store)
        self.env["frust"]["FRUST_UNIT_CACHE"] = "1" if args.unit_cache == "on" else "0"
        for item in args.frust_env:
            key, _, value = item.partition("=")
            if key.startswith(STORE_VARS):
                sys.exit(f"{key} is set by --store-root and --unit-cache, not by --frust-env")
            self.env["frust"][key] = value
        self.logs = args.work / "logs"
        self.logs.mkdir(parents=True, exist_ok=True)
        self.count = 0
        if args.jsonl:
            append_json(args.jsonl, {
                "type": "pair-meta", "workload": args.workload, "series": args.kind, "pairs": args.pairs,
                "unit_cache": args.unit_cache, "store": str(self.store), "frust_env": args.frust_env,
                "control": str(self.bins["control"]), "frust": str(self.bins["frust"]),
                "utc": datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds"),
            })

    def store_entries(self):
        """Number of top-level entries in the fRust store directory."""
        try:
            with os.scandir(self.store) as entries:
                return sum(1 for _ in entries)
        except FileNotFoundError:
            return 0

    def store_bytes(self):
        return allocated_bytes(self.store) if self.store_entries() else 0

    def fresh_store(self):
        """Delete the fRust store and prove it is empty. It is recreated so FRUST_HOME exists."""
        shutil.rmtree(self.store, ignore_errors=True)
        self.store.mkdir(parents=True)
        if self.store_entries():
            sys.exit(f"could not empty the fRust store {self.store}")

    def sample(self, toolchain, kind, target, label=None):
        """Run one timed build and return (wall seconds, peak RSS KiB)."""
        label = label or kind
        self.count += 1
        log = self.logs / f"{self.count:04d}-{self.args.workload}-{label}-{toolchain}.log"
        command = [str(TIME_BUILD), str(self.bins[toolchain]), self.args.workload, kind, str(target), str(log)]
        tracked = self.args.jsonl and toolchain == "frust" and self.args.unit_cache == "on"
        before = self.store_bytes() if tracked else None
        result = subprocess.run(command, env=self.env[toolchain], capture_output=True, text=True)
        text = log.read_text(errors="replace") if log.exists() else ""
        wall = re.search(r"^WALL_SECONDS:([0-9.]+)$", text, re.M)
        rss = re.search(r"^PEAK_RSS_KIB:([0-9]+)$", text, re.M)
        if result.returncode != 0 or not wall or not rss or not text.rstrip().endswith("EXIT:0"):
            tail = "\n".join(text.splitlines()[-30:])
            sys.exit(f"{toolchain} {label} sample failed ({result.returncode})\n{result.stderr}\n{tail}")
        if self.args.jsonl:
            append_json(self.args.jsonl, {
                "type": "pair-sample", "workload": self.args.workload, "series": self.args.kind,
                "sample": label, "toolchain": toolchain, "wall": float(wall.group(1)),
                "rss_kib": int(rss.group(1)), "store_before_bytes": before,
                "store_after_bytes": self.store_bytes() if tracked else None,
            })
        return float(wall.group(1)), int(rss.group(1))

    def seed(self, toolchain):
        target = self.args.work / f"seed-{toolchain}"
        self.sample(toolchain, "seed", target)
        return target

    def clean_sample(self, toolchain, target):
        """Build the unmodified tree cold, remove the target, and time the rebuild with the store warm."""
        if toolchain == "frust":
            self.fresh_store()
        self.sample(toolchain, "seed", target, label="clean-fill")
        shutil.rmtree(target, ignore_errors=True)
        if toolchain == "frust" and self.args.unit_cache == "on" and not self.store_entries():
            sys.exit("the cold build left the fRust unit store empty, so a clean rebuild would not measure it")
        return self.sample(toolchain, "seed", target, label="clean")[0]


def copy_tree(source, dest):
    if sys.platform == "darwin":
        # BSD cp has no --reflink. -c clones on APFS and fails elsewhere, so fall back to a plain copy.
        if subprocess.run(["cp", "-a", "-c", str(source), str(dest)], capture_output=True).returncode == 0:
            return
        shutil.rmtree(dest, ignore_errors=True)
        subprocess.run(["cp", "-a", str(source), str(dest)], check=True)
    else:
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
            if args.kind == "clean":
                wall[toolchain] = runner.clean_sample(toolchain, scratch)
            else:
                if args.kind == "cold":
                    # Nothing from an earlier pair may serve this build.
                    if toolchain == "frust":
                        runner.fresh_store()
                else:
                    copy_tree(seeds[toolchain], scratch)
                wall[toolchain], _ = runner.sample(toolchain, args.kind, scratch)
            shutil.rmtree(scratch, ignore_errors=True)
        if args.kind != "clean":
            with args.samples.open("a") as out:
                out.write(f"edit {args.workload} {args.kind} {wall['frust']:.9f} {wall['control']:.9f}\n")
        print(f"pair {index + 1}/{args.pairs} control {wall['control']:.3f}s frust {wall['frust']:.3f}s", flush=True)


def run_trace(runner, seeds):
    args = runner.args
    edits = TRACES.get(args.workload, TRACES["default"])
    rss = {}
    size = {}
    store = 0
    for toolchain in ("control", "frust"):
        target = args.work / f"trace-{toolchain}"
        copy_tree(seeds[toolchain], target)
        peaks = []
        # The final noop build is the normal rebuild after the reversed edits.
        for kind in edits + ("noop",):
            peaks.append(runner.sample(toolchain, kind, target)[1])
        rss[toolchain] = max(peaks)
        size[toolchain] = allocated_bytes(target)
        if toolchain == "frust":
            store = runner.store_bytes()
        shutil.rmtree(target, ignore_errors=True)
    # The gate counts target bytes. The unit store is a separate cache and is recorded beside it.
    with args.samples.open("a") as out:
        out.write(f"resource {args.workload} {rss['frust']} {rss['control']} {size['frust']} {size['control']}\n")
    if args.jsonl:
        append_json(args.jsonl, {
            "type": "pair-resource", "workload": args.workload, "rss_kib": rss,
            "target_bytes": size, "frust_store_bytes": store,
        })
    print(f"trace rss control {rss['control']} frust {rss['frust']} bytes control {size['control']} frust {size['frust']} frust store {store}")


def main():
    args = parse_args()
    args.work.mkdir(parents=True, exist_ok=True)
    runner = Runner(args)
    # Every series starts from an empty store, whatever an earlier series or job left behind.
    runner.fresh_store()
    seeds = {}
    if args.kind not in ("cold", "clean"):
        seeds = {toolchain: runner.seed(toolchain) for toolchain in ("control", "frust")}
    if args.kind == "trace":
        run_trace(runner, seeds)
    else:
        run_edit_pairs(runner, seeds)


if __name__ == "__main__":
    main()
