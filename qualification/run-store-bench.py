#!/usr/bin/env python3
"""Paired build CPU comparison of official Cargo against fRust with its unit cache.

Each round builds one cohort application with both toolchains in alternating order:
  cold      empty target directory, empty unit cache
  clean     target directory removed, cache populated by the cold build
  noop      immediate rebuild (median of --noops runs)
  peer      empty target directory, cache populated only by --peer (another application)
CPU is user plus system seconds of the cargo process tree from wait4. All builds are pinned to
--cpus so P-core and E-core scheduling cannot skew the comparison.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import statistics
import subprocess
import sys
import time


def tree_cpu(command, cwd, env, log):
    """Run command pinned and return (cpu seconds, wall seconds, max rss kib, exit status)."""
    start = time.monotonic()
    with open(log, "wb") as sink:
        child = subprocess.Popen(command, cwd=cwd, env=env, stdout=sink, stderr=subprocess.STDOUT)
        _, status, usage = os.wait4(child.pid, 0)
    wall = time.monotonic() - start
    return usage.ru_utime + usage.ru_stime, wall, usage.ru_maxrss, os.waitstatus_to_exitcode(status)


def allocated(path):
    """Allocated bytes of a tree, counting each inode once."""
    seen, total = set(), 0
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


class Toolchain:
    def __init__(self, name, cargo, rustc, work, cpus, jobs, uses_store):
        self.name, self.cargo, self.rustc = name, cargo, rustc
        self.work, self.cpus, self.jobs, self.uses_store = Path(work), cpus, jobs, uses_store
        self.store = self.work / f"{name}-xdg"

    def env(self, target):
        env = {key: value for key, value in os.environ.items()
               if not key.startswith(("CARGO", "RUSTC", "RUSTFLAGS", "FRUST", "XDG"))}
        env.update(CARGO_TARGET_DIR=str(target), CARGO_HOME=str(Path.home() / ".cargo"),
                   CARGO_TERM_COLOR="never", CARGO_INCREMENTAL="0", FRUST_PROGRESS="plain",
                   XDG_CACHE_HOME=str(self.store), XDG_CONFIG_HOME=str(self.work / "xdg-config"),
                   PATH=os.pathsep.join([str(Path(self.cargo).parent), env.get("PATH", "")]))
        if self.rustc:
            env["RUSTC"] = str(self.rustc)
        if not self.uses_store:
            env["FRUST_UNIT_CACHE"] = "0"
        return env

    def build(self, app, release, target, log):
        command = ["taskset", "-c", self.cpus, str(self.cargo), "build", "--locked", "--offline",
                   "-j", str(self.jobs)] + (["--release"] if release else [])
        return tree_cpu(command, app, self.env(target), log)

    def reset_store(self):
        shutil.rmtree(self.store, ignore_errors=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--official-cargo", required=True)
    parser.add_argument("--official-rustc")
    parser.add_argument("--frust-cargo", required=True, help="fcargo (or a cargo run as fcargo)")
    parser.add_argument("--frust-rustc", required=True)
    parser.add_argument("--app", required=True, help="cohort application directory")
    parser.add_argument("--frust-store", choices=("on", "off"), default="on",
                        help="off disables the unit cache for fRust (isolates the cache cost)")
    parser.add_argument("--peer", help="a second application used to warm the cache for 'peer'")
    parser.add_argument("--release", action="store_true")
    parser.add_argument("--rounds", type=int, default=3)
    parser.add_argument("--noops", type=int, default=5)
    parser.add_argument("--cpus", default="0-5")
    parser.add_argument("--jobs", type=int, default=6)
    parser.add_argument("--work", required=True, help="scratch directory under the repo build root")
    parser.add_argument("--out", required=True)
    args = parser.parse_args()

    work = Path(args.work)
    work.mkdir(parents=True, exist_ok=True)
    official = Toolchain("official", args.official_cargo, args.official_rustc, work, args.cpus,
                         args.jobs, False)
    frust = Toolchain("frust", args.frust_cargo, args.frust_rustc, work, args.cpus,
                      args.jobs, args.frust_store == "on")
    app = Path(args.app)
    profile = "release" if args.release else "dev"
    records = []

    def measure(chain, scenario, round_number, target, application):
        log = work / f"{chain.name}-{scenario}.log"
        cpu, wall, rss, status = chain.build(application, args.release, target, log)
        record = dict(toolchain=chain.name, scenario=scenario, round=round_number, profile=profile,
                      app=application.name, cpu=cpu, wall=wall, rss_kib=rss, status=status,
                      target_bytes=allocated(target), store_bytes=allocated(chain.store))
        records.append(record)
        with open(args.out, "a") as handle:
            handle.write(json.dumps(record) + "\n")
        if status != 0:
            sys.exit(f"{chain.name} {scenario} failed; see {log}")
        print(f"{chain.name:8} {scenario:6} r{round_number} cpu={cpu:8.2f} wall={wall:7.2f}", flush=True)

    for round_number in range(args.rounds):
        order = [official, frust] if round_number % 2 == 0 else [frust, official]
        for chain in order:
            target = work / f"{chain.name}-target"
            chain.reset_store()
            shutil.rmtree(target, ignore_errors=True)
            measure(chain, "cold", round_number, target, app)
            shutil.rmtree(target, ignore_errors=True)
            measure(chain, "clean", round_number, target, app)
            noops = []
            for _ in range(args.noops):
                cpu, wall, rss, status = chain.build(app, args.release, target, work / "noop.log")
                noops.append(cpu)
            median = statistics.median(noops)
            records.append(dict(toolchain=chain.name, scenario="noop", round=round_number,
                                profile=profile, app=app.name, cpu=median))
            with open(args.out, "a") as handle:
                handle.write(json.dumps(records[-1]) + "\n")
            print(f"{chain.name:8} noop   r{round_number} cpu={median:8.4f}", flush=True)
            if args.peer and chain.uses_store:
                chain.reset_store()
                peer_target = work / f"{chain.name}-peer-target"
                shutil.rmtree(peer_target, ignore_errors=True)
                chain.build(Path(args.peer), args.release, peer_target, work / "peer-warm.log")
                shutil.rmtree(target, ignore_errors=True)
                measure(chain, "peer", round_number, target, app)
                shutil.rmtree(peer_target, ignore_errors=True)
            shutil.rmtree(target, ignore_errors=True)
    for chain in (official, frust):
        chain.reset_store()

    print("\nscenario  official_cpu  frust_cpu  ratio  (median)   official_min  frust_min  ratio (min)")
    for scenario in ("cold", "clean", "noop", "peer"):
        def pick(name, wanted=scenario):
            return [r["cpu"] for r in records
                    if r["toolchain"] == name and r["scenario"] == wanted]
        # The peer scenario is compared with official Rust's ordinary cold build.
        base = pick("official", "cold" if scenario == "peer" else scenario)
        mine = pick("frust")
        if mine and base:
            print(f"{scenario:8}  {statistics.median(base):12.3f}  {statistics.median(mine):9.3f}  "
                  f"{statistics.median(mine) / statistics.median(base):.3f}"
                  f"   {min(base):12.3f}  {min(mine):9.3f}  {min(mine) / min(base):.3f}")


if __name__ == "__main__":
    main()
