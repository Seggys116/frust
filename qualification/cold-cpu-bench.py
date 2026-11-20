#!/usr/bin/env python3
"""Paired cold-build CPU comparison of several toolchains on one cohort application.

Every round builds the application once per toolchain from an empty target directory, in a
rotated order (round r starts with toolchain r mod n, and odd rounds run the rotation
backwards), so each toolchain occupies every position equally often. All builds are pinned to
--cpus with taskset. CPU is user plus system seconds of the Cargo process tree from wait4.
fRust toolchains run with the unit cache off (FRUST_UNIT_CACHE=0), so every unit is compiled.

  --toolchain NAME=PREFIX[:frust]   PREFIX/bin/cargo and PREFIX/bin/rustc are used; ':frust'
                                    selects PREFIX/bin/fcargo and PREFIX/bin/frustc instead.
The first toolchain is the reference for the ratios.
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


def run_tree(command, cwd, env, log):
    start = time.monotonic()
    with open(log, "wb") as sink:
        child = subprocess.Popen(command, cwd=cwd, env=env, stdout=sink, stderr=subprocess.STDOUT)
        _, status, usage = os.wait4(child.pid, 0)
    return (usage.ru_utime + usage.ru_stime, usage.ru_utime, usage.ru_stime,
            time.monotonic() - start, usage.ru_maxrss, os.waitstatus_to_exitcode(status))


class Toolchain:
    def __init__(self, spec):
        name, _, rest = spec.partition("=")
        prefix, _, kind = rest.partition(":")
        self.name, self.prefix, self.frust = name, Path(prefix), kind == "frust"
        bin_dir = self.prefix / "bin"
        self.cargo = bin_dir / ("fcargo" if self.frust else "cargo")
        self.rustc = bin_dir / ("frustc" if self.frust else "rustc")
        for tool in (self.cargo, self.rustc):
            if not os.access(tool, os.X_OK):
                sys.exit(f"{name}: {tool} is not executable")

    def env(self, work, target):
        env = {key: value for key, value in os.environ.items()
               if not key.startswith(("CARGO", "RUSTC", "RUSTFLAGS", "RUSTDOCFLAGS", "FRUST",
                                      "XDG", "RUSTUP"))}
        env.update(CARGO_TARGET_DIR=str(target), CARGO_HOME=str(Path.home() / ".cargo"),
                   CARGO_TERM_COLOR="never", CARGO_INCREMENTAL="0", RUSTC=str(self.rustc),
                   XDG_CACHE_HOME=str(work / f"{self.name}-xdg"),
                   XDG_CONFIG_HOME=str(work / "xdg-config"),
                   PATH=os.pathsep.join([str(self.cargo.parent), "/usr/bin", "/bin"]))
        if self.frust:
            env.update(FRUST_UNIT_CACHE="0", FRUST_PROGRESS="plain")
        return env


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--toolchain", action="append", required=True)
    parser.add_argument("--app", required=True)
    parser.add_argument("--release", action="store_true")
    parser.add_argument("--rounds", type=int, default=6)
    parser.add_argument("--cpus", default="6-9")
    parser.add_argument("--jobs", type=int, default=4)
    parser.add_argument("--work", required=True)
    parser.add_argument("--out", required=True)
    args = parser.parse_args()

    chains = [Toolchain(spec) for spec in args.toolchain]
    work = Path(args.work)
    work.mkdir(parents=True, exist_ok=True)
    app = Path(args.app)
    profile = "release" if args.release else "dev"
    samples = {chain.name: [] for chain in chains}
    for round_number in range(args.rounds):
        shift = round_number % len(chains)
        order = chains[shift:] + chains[:shift]
        if round_number % 2:
            order.reverse()
        for chain in order:
            target = work / f"{chain.name}-target"
            shutil.rmtree(target, ignore_errors=True)
            shutil.rmtree(work / f"{chain.name}-xdg", ignore_errors=True)
            command = ["taskset", "-c", args.cpus, str(chain.cargo), "build", "--locked",
                       "--offline", "-j", str(args.jobs)] + (["--release"] if args.release else [])
            log = work / f"{chain.name}-{profile}.log"
            cpu, user, system, wall, rss, status = run_tree(command, app, chain.env(work, target),
                                                            log)
            units = sum(1 for line in open(log, errors="replace")
                        if line.lstrip().startswith("Compiling "))
            record = dict(toolchain=chain.name, app=app.name, profile=profile,
                          round=round_number, cpu=cpu, user=user, sys=system, wall=wall,
                          rss_kib=rss, status=status, units=units, cpus=args.cpus,
                          jobs=args.jobs)
            with open(args.out, "a") as handle:
                handle.write(json.dumps(record) + "\n")
            if status != 0:
                sys.exit(f"{chain.name} failed; see {log}")
            samples[chain.name].append((round_number, cpu))
            print(f"r{round_number} {chain.name:10} cpu={cpu:8.2f} wall={wall:7.2f} "
                  f"units={units}", flush=True)
            shutil.rmtree(target, ignore_errors=True)

    reference = chains[0].name
    base = dict(samples[reference])
    print(f"\n{app.name} {profile} cold, {args.rounds} rounds, cpus {args.cpus}, -j{args.jobs}")
    print("toolchain    median_cpu   min_cpu  ratio_median  ratio_min  paired_median  paired_min")
    for chain in chains:
        mine = dict(samples[chain.name])
        paired = [mine[r] / base[r] for r in mine if r in base]
        med, low = statistics.median(mine.values()), min(mine.values())
        print(f"{chain.name:10} {med:12.3f} {low:9.3f} "
              f"{med / statistics.median(base.values()):13.4f} {low / min(base.values()):10.4f} "
              f"{statistics.median(paired):14.4f} {min(paired):11.4f}")


if __name__ == "__main__":
    main()
