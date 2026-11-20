#!/usr/bin/env python3
"""Paired CPU comparison of several Cargo/rustc configurations on cold, no-op and edit builds,
to separate the cost of the fRust unit cache from the cost of fcargo against official Cargo.

Each --config is NAME=CARGO,RUSTC,MODE with MODE one of
  official  no fRust variables at all
  on        FRUST_UNIT_CACHE=1 and FRUST_UNIT_CACHE_DEBUG=1 with an isolated FRUST_HOME, as ci-bench.py
  off       FRUST_UNIT_CACHE=0 with an isolated FRUST_HOME
Every build uses the frozen release profile of ci-bench.py, CARGO_TARGET_DIR (as CI does),
taskset --cpus and -j --jobs. Before every measured build the page cache of every file of every
toolchain under test (bin/, lib/*.so*, host rustlib lib/*.so*) is dropped, as in
cold-build-bench-equalised.py. Round r runs the configurations rotated by r, so each takes every position.

Per round and configuration: cold (empty target and store), --noops no-op builds (median),
then each --edit on a reflink copy of the cold target. CPU is user plus system seconds of the
Cargo process tree from wait4. Output: JSON lines, and a summary of paired ratios against the
first configuration.
"""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import shutil
import statistics
import subprocess
import sys

HERE = Path(__file__).resolve().parent
sys.dont_write_bytecode = True
_spec = importlib.util.spec_from_file_location("ci_bench", HERE / "ci-bench.py")
CI = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(CI)
STORE = CI.STORE


def prefix_files(prefix):
    prefix = Path(prefix)
    found = [p for p in (prefix / "bin").iterdir() if p.is_file()]
    found += [p for p in (prefix / "lib").glob("*.so*") if p.is_file() and not p.is_symlink()]
    found += [p for p in (prefix / "lib" / "rustlib").glob("*/lib/*.so*") if p.is_file()]
    return found


def evict(files):
    for path in files:
        try:
            fd = os.open(path, os.O_RDONLY)
        except OSError:
            continue
        try:
            os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
        finally:
            os.close(fd)


class Config:
    def __init__(self, spec, work, cpus, jobs, extra_env):
        name, _, rest = spec.partition("=")
        cargo, rustc, mode = rest.split(",")
        if mode not in ("official", "on", "off"):
            sys.exit(f"bad mode in {spec}")
        self.name, self.cargo, self.rustc, self.mode = name, Path(cargo), Path(rustc), mode
        self.work, self.cpus, self.jobs, self.extra_env = work, cpus, jobs, extra_env
        self.store = work / f"{name}-home"
        self.prefixes = {self.cargo.parent.parent, self.rustc.parent.parent}

    def env(self, target):
        env = {k: v for k, v in os.environ.items()
               if not k.startswith(("CARGO", "RUSTC", "RUSTFLAGS", "FRUST", "XDG"))}
        env.update(CARGO_TARGET_DIR=str(target), CARGO_HOME=str(Path.home() / ".cargo"),
                   CARGO_TERM_COLOR="never", FRUST_PROGRESS="plain", RUSTC=str(self.rustc),
                   XDG_CACHE_HOME=str(self.work / f"{self.name}-xdg"),
                   XDG_CONFIG_HOME=str(self.work / "xdg-config"),
                   PATH=os.pathsep.join([str(self.cargo.parent), env.get("PATH", "")]))
        env.update(CI.PROFILE)
        if self.mode == "on":
            env.update(FRUST_HOME=str(self.store), FRUST_UNIT_CACHE="1", FRUST_UNIT_CACHE_DEBUG="1")
        elif self.mode == "off":
            env.update(FRUST_HOME=str(self.store), FRUST_UNIT_CACHE="0")
        env.update(self.extra_env)
        return env

    def build(self, app, target, log):
        command = ["taskset", "-c", self.cpus, str(self.cargo), "build", "--locked", "--offline",
                   "--release", "-j", str(self.jobs)]
        return STORE.tree_cpu(command, app, self.env(target), log)

    def reset(self):
        for path in (self.store, self.work / f"{self.name}-xdg"):
            shutil.rmtree(path, ignore_errors=True)
        self.store.mkdir(parents=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--config", action="append", required=True)
    parser.add_argument("--app", required=True)
    parser.add_argument("--rounds", type=int, default=6)
    parser.add_argument("--noops", type=int, default=3)
    parser.add_argument("--edit", action="append", default=[], help="edit kind, e.g. leaf")
    parser.add_argument("--scenarios", default="cold,noop,edit")
    parser.add_argument("--cpus", default="6-9")
    parser.add_argument("--jobs", type=int, default=4)
    parser.add_argument("--env", action="append", default=[], help="KEY=VALUE for every non-official config")
    parser.add_argument("--no-evict", action="store_true",
                        help="leave the page cache alone, as ci-bench.py does")
    parser.add_argument("--work", required=True)
    parser.add_argument("--out", required=True)
    args = parser.parse_args()

    work = Path(args.work).resolve()
    work.mkdir(parents=True, exist_ok=True)
    app = Path(args.app).resolve()
    if not CI.tree_clean(app):
        sys.exit(f"{app} is modified")
    extra = dict(item.partition("=")[::2] for item in args.env)
    configs = [Config(spec, work, args.cpus, args.jobs, extra) for spec in args.config]
    for config in configs:
        if config.mode == "official":
            config.extra_env = {}
    files = [f for c in configs for p in sorted(c.prefixes) for f in prefix_files(p)]
    files = sorted(set(files))
    edits = [CI.Edit(app, app.name, kind) for kind in args.edit]
    scenarios = args.scenarios.split(",")
    logs = work / "logs"
    logs.mkdir(exist_ok=True)
    records = []

    def emit(record):
        records.append(record)
        with open(args.out, "a") as handle:
            handle.write(json.dumps(record, sort_keys=True) + "\n")

    def measured(config, scenario, rnd, target, position=None):
        log = logs / f"r{rnd}-{config.name}-{scenario}.log"
        if not args.no_evict:
            evict(files)
        cpu, wall, rss, status = config.build(app, target, log)
        if status != 0:
            sys.exit(f"{config.name} {scenario} failed, see {log}")
        compiled, events = CI.log_counts(log)
        return dict(config=config.name, scenario=scenario, round=rnd, position=position, cpu=cpu,
                    wall=wall, compiled=compiled, events=events)

    for rnd in range(args.rounds):
        # Rotation only: with an even number of configurations, rotating and reversing would keep
        # every configuration on one position parity and confound it with any alternating load.
        order = configs[rnd % len(configs):] + configs[:rnd % len(configs)]
        for position, config in enumerate(order):
            target = work / f"{config.name}-target"
            shutil.rmtree(target, ignore_errors=True)
            config.reset()
            rec = measured(config, "cold", rnd, target, position)
            if "cold" in scenarios:
                emit(rec)
            if "noop" in scenarios:
                cpus = [measured(config, "noop", rnd, target)["cpu"] for _ in range(args.noops)]
                emit(dict(config=config.name, scenario="noop", round=rnd, cpu=statistics.median(cpus),
                          samples=cpus))
            if "edit" in scenarios:
                for edit in edits:
                    scratch = work / f"{config.name}-edit-target"
                    shutil.rmtree(scratch, ignore_errors=True)
                    CI.copy_tree(target, scratch)
                    with edit.applied():
                        emit(measured(config, f"edit-{edit.kind}", rnd, scratch, position))
                    shutil.rmtree(scratch, ignore_errors=True)
            shutil.rmtree(target, ignore_errors=True)
            print(" ".join(f"{r['config']}:{r['scenario']}={r['cpu']:.3f}" for r in records
                           if r["round"] == rnd and r["config"] == config.name), flush=True)

    reference = configs[0].name
    print(f"\npaired CPU ratio against {reference}: median (min..max) [pairs, wins]")
    for scenario in sorted({r["scenario"] for r in records}):
        base = {r["round"]: r["cpu"] for r in records if r["config"] == reference and r["scenario"] == scenario}
        for config in configs:
            mine = {r["round"]: r["cpu"] for r in records if r["config"] == config.name and r["scenario"] == scenario}
            ratios = [mine[k] / base[k] for k in mine if k in base]
            if not ratios:
                continue
            wins = sum(1 for k in mine if k in base and mine[k] < base[k])
            print(f"{scenario:12} {config.name:12} cpu_med={statistics.median(mine.values()):8.3f} "
                  f"ratio={statistics.median(ratios):.4f} ({min(ratios):.4f}..{max(ratios):.4f}) "
                  f"[{len(ratios)}, {wins}] delta_med={statistics.median(mine[k] - base[k] for k in mine if k in base):+.3f}s")


if __name__ == "__main__":
    main()
