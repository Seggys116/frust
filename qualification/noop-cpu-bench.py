#!/usr/bin/env python3
"""Paired CPU comparison of a no-op Cargo build across several Cargo and rustc configurations.

Each --config is NAME=CARGO,RUSTC[,MODE]. MODE is one of
  official  no fRust variables at all (the default when CARGO is not named fcargo, frust or fclippy)
  frust     FRUST_UNIT_CACHE=1 with an isolated FRUST_HOME, as ci-bench.py runs fRust
Every configuration builds the application once with the frozen release profile of ci-bench.py into
its own CARGO_TARGET_DIR, so every timed build is a no-op that compiles nothing. A round then times
--noops no-op builds of each configuration, in an order rotated by the round number so each takes every
position. CPU is user plus system seconds of the whole process tree from wait4, never wall time. The
sample of a round is the median of its --noops builds, as in ci-bench.py.

The report gives each configuration's median sample, its median paired ratio against the first
configuration (ratios are formed per round, so a slow host moves both sides together) and the 95% sign
interval of that median. Page cache and CPU frequency are whatever the host has; this tool does not
pin or evict anything. A timed build that compiles a unit is an error.

  qualification/noop-cpu-bench.py --app DIR --work DIR --rounds 20 \\
      --config official=$HOME/.rustup/toolchains/1.100.0-x86_64-unknown-linux-gnu/bin/cargo,$HOME/.rustup/toolchains/1.100.0-x86_64-unknown-linux-gnu/bin/rustc \\
      --config new=/path/bin/fcargo,/path/bin/frustc
"""
import argparse
import json
import os
from pathlib import Path
import re
import shutil
import statistics
import subprocess
import sys

HERE = Path(__file__).resolve().parent
sys.dont_write_bytecode = True
sys.path.insert(0, str(HERE))
from paired_stats import median_interval  # noqa: E402

PROFILE = {
    "CARGO_PROFILE_RELEASE_OPT_LEVEL": "3",
    "CARGO_PROFILE_RELEASE_LTO": "fat",
    "CARGO_PROFILE_RELEASE_CODEGEN_UNITS": "1",
    "CARGO_PROFILE_RELEASE_INCREMENTAL": "true",
    "CARGO_PROFILE_RELEASE_DEBUG": "0",
    "CARGO_PROFILE_RELEASE_STRIP": "none",
}
FRUST_ENTRIES = ("fcargo", "frust", "fclippy")


class Config:
    def __init__(self, text, work):
        name, _, rest = text.partition("=")
        parts = rest.split(",")
        if not name or len(parts) < 2:
            sys.exit(f"--config wants NAME=CARGO,RUSTC[,MODE], got {text!r}")
        self.name, self.cargo, self.rustc = name, parts[0], parts[1]
        default = "frust" if Path(self.cargo).name in FRUST_ENTRIES else "official"
        self.mode = parts[2] if len(parts) > 2 else default
        if self.mode not in ("official", "frust"):
            sys.exit(f"unknown mode {self.mode!r} in {text!r}")
        self.target = work / f"{name}-target"
        self.home = work / f"{name}-home"
        self.xdg = work / f"{name}-xdg"

    def env(self):
        env = {key: value for key, value in os.environ.items()
               if not key.startswith(("FRUST_", "CARGO_PROFILE_")) and key != "CARGO_INCREMENTAL"}
        env.update(PROFILE)
        env["CARGO_TARGET_DIR"] = str(self.target)
        env["XDG_CACHE_HOME"] = str(self.xdg)
        env["RUSTC"] = self.rustc
        if self.mode == "frust":
            env.update(FRUST_HOME=str(self.home), FRUST_UNIT_CACHE="1")
        return env


def run(config, app, jobs, log):
    command = [config.cargo, "build", "--locked", "--offline", "--release", "-j", str(jobs)]
    with open(log, "wb") as sink:
        child = subprocess.Popen(command, cwd=app, env=config.env(), stdout=sink, stderr=subprocess.STDOUT)
        _, status, usage = os.wait4(child.pid, 0)
    code = os.waitstatus_to_exitcode(status)
    if code != 0:
        sys.exit(f"{config.name}: build failed with {code}; see {log}")
    return usage.ru_utime + usage.ru_stime


def compiled_units(log):
    return len(re.findall(r"^\s*Compiling ", Path(log).read_text(errors="replace"), re.M))


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--app", required=True, help="application directory with a Cargo.lock and fetched dependencies")
    parser.add_argument("--work", required=True, help="scratch directory for targets, stores and logs")
    parser.add_argument("--config", action="append", required=True)
    parser.add_argument("--rounds", type=int, default=20)
    parser.add_argument("--noops", type=int, default=5)
    parser.add_argument("--jobs", type=int, default=4)
    parser.add_argument("--out", help="JSON lines file for the raw samples")
    args = parser.parse_args()

    app = Path(args.app).resolve()
    work = Path(args.work).resolve()
    work.mkdir(parents=True, exist_ok=True)
    configs = [Config(text, work) for text in args.config]
    if len({config.name for config in configs}) != len(configs):
        sys.exit("configuration names must differ")
    for config in configs:
        for path in (config.target, config.home, config.xdg):
            shutil.rmtree(path, ignore_errors=True)
        config.xdg.mkdir(parents=True)
        log = work / f"{config.name}-warm.log"
        run(config, app, args.jobs, log)
        run(config, app, args.jobs, log)
        if compiled_units(log):
            sys.exit(f"{config.name}: the warm-up rebuild compiled units; see {log}")

    rows = {config.name: [] for config in configs}
    sink = open(args.out, "w") if args.out else None
    for round_number in range(args.rounds):
        shift = round_number % len(configs)
        for config in configs[shift:] + configs[:shift]:
            log = work / f"{config.name}-noop.log"
            samples = []
            for _ in range(args.noops):
                samples.append(run(config, app, args.jobs, log))
                if compiled_units(log):
                    sys.exit(f"{config.name} compiled during a timed no-op; see {log}")
            rows[config.name].append(statistics.median(samples))
            if sink:
                sink.write(json.dumps(dict(round=round_number, config=config.name, cpu=rows[config.name][-1],
                                           samples=samples)) + "\n")
    if sink:
        sink.close()

    base = configs[0].name
    print(f"{'config':14} {'median cpu ms':>14} {'ratio vs ' + base:>16} {'95% interval':>18}")
    for config in configs:
        cpu = statistics.median(rows[config.name]) * 1000
        ratios = [mine / theirs for mine, theirs in zip(rows[config.name], rows[base])]
        interval = median_interval(ratios)
        if config.name == base or interval is None:
            print(f"{config.name:14} {cpu:14.2f} {'':>16} {'':>18}")
            continue
        print(f"{config.name:14} {cpu:14.2f} {statistics.median(ratios) * 100:15.1f}% "
              f"[{interval[0] * 100:6.1f}, {interval[1] * 100:6.1f}]")
    print(f"{args.rounds} rounds, median of {args.noops} no-op builds per sample, cargo -j {args.jobs}")


if __name__ == "__main__":
    main()
