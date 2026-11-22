#!/usr/bin/env python3
"""Replay one crate's exact rustc invocation with several compilers, pinned to one core.

Preparation builds the application once per toolchain and profile with `cargo build -v` (each
toolchain needs its own dependency rlibs, because standard library hashes differ between
builds) and records the rustc command Cargo ran for --crate. Each round then replays that
command once per toolchain in rotated order and records the compiler's CPU time (wait4), and
once more with `-Z time-passes` (RUSTC_BOOTSTRAP=1) for phase attribution. Outputs go to a
private directory per toolchain; the outputs of the plain runs are hashed so that object
code identity between rounds of one toolchain can be checked.

  --toolchain NAME=PREFIX      PREFIX/bin/cargo and PREFIX/bin/rustc
The first toolchain is the reference for the ratios.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import statistics
import subprocess
import sys
import threading
import time

KEY_PASSES = ("total", "parse_crate", "expand_crate", "type_check_crate", "MIR_borrow_checking",
              "MIR_effect_checking", "monomorphization_collector_graph_walk",
              "generate_crate_metadata", "codegen_crate", "LLVM_passes", "finish_ongoing_codegen",
              "link", "llvm_dump_timing_file")


def clean_env():
    env = {key: value for key, value in os.environ.items()
           if not key.startswith(("CARGO", "RUSTC", "RUSTFLAGS", "RUSTDOCFLAGS", "FRUST", "XDG",
                                  "RUSTUP"))}
    env["PATH"] = "/usr/bin:/bin"
    return env


# The frozen release profile of qualification/PROTOCOL.md that ci-bench.py uses on both sides.
CI_PROFILE = {
    "CARGO_PROFILE_RELEASE_OPT_LEVEL": "3",
    "CARGO_PROFILE_RELEASE_LTO": "fat",
    "CARGO_PROFILE_RELEASE_CODEGEN_UNITS": "1",
    "CARGO_PROFILE_RELEASE_INCREMENTAL": "true",
    "CARGO_PROFILE_RELEASE_DEBUG": "0",
    "CARGO_PROFILE_RELEASE_STRIP": "none",
}


def capture(prefix, app, profile, crate, work, ci_profile=False):
    target = work / f"prep-{prefix.name}-{profile}"
    env = clean_env()
    env.update(CARGO_TARGET_DIR=str(target), CARGO_HOME=str(Path.home() / ".cargo"),
               CARGO_INCREMENTAL="0", CARGO_TERM_COLOR="never", FRUST_UNIT_CACHE="0",
               RUSTC=str(prefix / "bin" / "rustc"),
               PATH=os.pathsep.join([str(prefix / "bin"), "/usr/bin", "/bin"]))
    if ci_profile:
        env.pop("CARGO_INCREMENTAL")
        env.update(CI_PROFILE)
    log = work / f"prep-{prefix.name}-{profile}.log"
    # Cargo does not print the CARGO_* variables it gives rustc, so a wrapper records the
    # environment and directory of the one invocation that is replayed.
    record = work / f"prep-{prefix.name}-{profile}.env"
    record.unlink(missing_ok=True)
    wrapper = work / "record-env.sh"
    wrapper.write_text('''#!/bin/bash
for ((i = 1; i < $#; i++)); do
    if [[ ${!i} == --crate-name ]]; then
        j=$((i + 1))
        if [[ ${!j} == "$REPLAY_RECORD_CRATE" ]]; then
            { pwd; env -0 | tr '\\0' '\\1'; } > "$REPLAY_RECORD_FILE"
        fi
    fi
done
exec "$@"
''')
    wrapper.chmod(0o755)
    env.update(RUSTC_WRAPPER=str(wrapper), REPLAY_RECORD_CRATE=crate,
               REPLAY_RECORD_FILE=str(record))
    command = [str(prefix / "bin" / "cargo"), "build", "-v", "--locked", "--offline", "-j", "8"]
    if profile == "release":
        command.append("--release")
    with open(log, "wb") as sink:
        subprocess.run(command, cwd=app, env=env, stdout=sink, stderr=subprocess.STDOUT,
                       check=True)
    pattern = re.compile(r"Running `(.*--crate-name " + re.escape(crate) + r" .*)`$")
    for line in open(log, errors="replace"):
        match = pattern.search(line.rstrip("\n"))
        if match:
            words = shlex.split(match.group(1))
            run_env = {}
            while words and re.match(r"^[A-Za-z_][A-Za-z0-9_]*=", words[0]):
                key, _, value = words.pop(0).partition("=")
                run_env[key] = value
            if words[0] == str(wrapper):
                words = words[1:]
            if not record.exists():
                sys.exit(f"the wrapper did not see {crate} in {log}")
            cwd, _, blob = record.read_text(errors="replace").partition("\n")
            for entry in blob.split("\x01"):
                key, _, value = entry.partition("=")
                if key.startswith("CARGO_") and not key.startswith(
                        ("CARGO_TARGET", "CARGO_HOME", "CARGO_PROFILE", "CARGO_INCREMENTAL",
                         "CARGO_TERM")) or key in ("LD_LIBRARY_PATH", "CARGO", "RUSTC_BOOTSTRAP"):
                    run_env.setdefault(key, value)
            run_env["REPLAY_CWD"] = cwd
            return words, run_env
    sys.exit(f"no rustc invocation for {crate} in {log}")


class FrequencySampler(threading.Thread):
    """Mean frequency (GHz) of the pinned CPUs while a replay runs.

    CPU seconds scale with the clock, and on a shared or power-limited host the clock moves by
    a factor of two between runs. Seconds times mean frequency estimates cycles and is the
    steadier figure there. Hosts without cpufreq report 0 and the estimate is unavailable."""

    def __init__(self, cpu):
        super().__init__(daemon=True)
        self.paths = []
        for part in cpu.split(","):
            low, _, high = part.partition("-")
            for number in range(int(low), int(high or low) + 1):
                path = Path(f"/sys/devices/system/cpu/cpu{number}/cpufreq/scaling_cur_freq")
                if path.exists():
                    self.paths.append(path)
        self.samples = []
        self.stop = threading.Event()

    def run(self):
        while not self.stop.is_set():
            for path in self.paths:
                try:
                    self.samples.append(int(path.read_text()) / 1e6)
                except (OSError, ValueError):
                    pass
            self.stop.wait(0.05)

    def mean(self):
        return statistics.mean(self.samples) if self.samples else 0.0


LAST_GHZ = 0.0


def run_measured(words, run_env, out_dir, cpu, passes, extra_env=None):
    """Run the replay as a direct child so wait4 reports its rusage."""
    out_dir.mkdir(parents=True, exist_ok=True)
    args = list(words[1:])
    for index, word in enumerate(args):
        if word == "--out-dir":
            args[index + 1] = str(out_dir)
        elif word == "-C" and index + 1 < len(args) and args[index + 1].startswith("incremental="):
            # A cold build starts from an empty incremental directory, private to this run.
            args[index + 1] = f"incremental={out_dir / 'incremental'}"
    if passes:
        args += ["-Z", "time-passes"]
    env = clean_env()
    env.update({key: value for key, value in run_env.items() if key != "REPLAY_CWD"})
    env.update(extra_env or {})
    if passes:
        env["RUSTC_BOOTSTRAP"] = "1"
    cwd = run_env.get("REPLAY_CWD") or run_env.get("CARGO_MANIFEST_DIR") or os.getcwd()
    log = out_dir / ("passes.log" if passes else "plain.log")
    global LAST_GHZ
    sampler = FrequencySampler(cpu)
    start = time.monotonic()
    with open(log, "wb") as sink:
        child = subprocess.Popen(["taskset", "-c", cpu, words[0]] + args, cwd=cwd, env=env,
                                 stdout=sink, stderr=subprocess.STDOUT)
        sampler.start()
        _, status, usage = os.wait4(child.pid, 0)
    sampler.stop.set()
    sampler.join()
    LAST_GHZ = sampler.mean()
    wall = time.monotonic() - start
    code = os.waitstatus_to_exitcode(status)
    if code != 0:
        sys.exit(f"replay failed ({code}); see {log}")
    return usage.ru_utime + usage.ru_stime, usage.ru_utime, usage.ru_stime, wall, log


def parse_passes(log):
    totals = {}
    for line in open(log, errors="replace"):
        match = re.match(r"time:\s+([0-9.]+);.*\s(\S+)$", line.rstrip())
        if match:
            totals[match.group(2)] = totals.get(match.group(2), 0.0) + float(match.group(1))
    return totals


def digest(directory):
    hasher = hashlib.sha256()
    for path in (sorted(directory.glob("*.rlib")) + sorted(directory.glob("*.rmeta"))
                 + sorted(p for p in directory.iterdir()
                          if p.is_file() and os.access(p, os.X_OK) and "." not in p.name)):
        hasher.update(path.name.encode())
        hasher.update(path.read_bytes())
    return hasher.hexdigest()[:16]


def interval_text(ratios):
    """Distribution-free 95% interval of the median paired ratio (qualification/paired_stats.py)."""
    sys.path.insert(0, str(Path(__file__).resolve().parent))
    from paired_stats import median_interval
    interval = median_interval(ratios)
    return "n/a" if interval is None else f"[{interval[0]:.4f},{interval[1]:.4f}]"


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--toolchain", action="append", required=True)
    parser.add_argument("--app", required=True)
    parser.add_argument("--crate", required=True)
    parser.add_argument("--profile", choices=("dev", "release"), default="dev")
    parser.add_argument("--rounds", type=int, default=8)
    parser.add_argument("--cpu", default="6")
    parser.add_argument("--work", required=True)
    parser.add_argument("--out", required=True)
    parser.add_argument("--ci-profile", action="store_true",
                        help="use the frozen release profile of ci-bench.py (fat LTO, one CGU, "
                             "incremental) with --profile release")
    parser.add_argument("--no-passes", action="store_true",
                        help="skip the -Z time-passes run of each round")
    parser.add_argument("--env", action="append", default=[],
                        help="NAME:KEY=VALUE, an extra environment variable for one toolchain's "
                             "replays only (not its capture)")
    args = parser.parse_args()
    extra = {}
    for item in args.env:
        name, _, assign = item.partition(":")
        key, _, value = assign.partition("=")
        extra.setdefault(name, {})[key] = value

    work = Path(args.work)
    work.mkdir(parents=True, exist_ok=True)
    app = Path(args.app)
    chains = []
    for spec in args.toolchain:
        name, _, prefix = spec.partition("=")
        prefix = Path(prefix)
        words, run_env = capture(prefix, app, args.profile, args.crate, work, args.ci_profile)
        if Path(words[0]).resolve() != (prefix / "bin" / "rustc").resolve():
            sys.exit(f"{name}: captured compiler {words[0]} is not {prefix}/bin/rustc")
        chains.append((name, words, run_env))
        print(f"captured {name}: {args.crate} {args.profile}", flush=True)

    cpu = {name: [] for name, _, _ in chains}
    mcycles = {name: [] for name, _, _ in chains}
    phases = {name: [] for name, _, _ in chains}
    hashes = {name: set() for name, _, _ in chains}
    for round_number in range(args.rounds):
        shift = round_number % len(chains)
        order = chains[shift:] + chains[:shift]
        if round_number % 2:
            order.reverse()
        for name, words, run_env in order:
            out_dir = work / f"replay-{name}-{args.profile}"
            shutil.rmtree(out_dir, ignore_errors=True)
            total, user, system, wall, _ = run_measured(words, run_env, out_dir, args.cpu, False,
                                                        extra.get(name))
            ghz = LAST_GHZ
            hashes[name].add(digest(out_dir))
            shutil.rmtree(out_dir, ignore_errors=True)
            if args.no_passes:
                passes = {}
            else:
                _, _, _, _, log = run_measured(words, run_env, out_dir, args.cpu, True,
                                               extra.get(name))
                passes = parse_passes(log)
                shutil.rmtree(out_dir, ignore_errors=True)
            cpu[name].append((round_number, total))
            mcycles[name].append((round_number, total * ghz * 1000))
            phases[name].append(passes)
            with open(args.out, "a") as handle:
                handle.write(json.dumps(dict(toolchain=name, crate=args.crate,
                                             profile=args.profile, round=round_number,
                                             cpu=total, user=user, sys=system, wall=wall,
                                             ghz=ghz, mcycles=total * ghz * 1000,
                                             passes=passes)) + "\n")
            print(f"r{round_number} {name:10} cpu={total:7.3f} "
                  f"passes_total={passes.get('total', 0):7.3f}", flush=True)

    reference = chains[0][0]
    base = dict(cpu[reference])
    print(f"\n{args.crate} {args.profile}, {args.rounds} rounds, cpu {args.cpu}")
    print("toolchain    median_cpu  min_cpu  ratio_median  ratio_min  paired_median  "
          "paired_95%_interval  outputs")
    for name, _, _ in chains:
        mine = dict(cpu[name])
        paired = [mine[r] / base[r] for r in mine]
        print(f"{name:10} {statistics.median(mine.values()):11.3f} {min(mine.values()):8.3f} "
              f"{statistics.median(mine.values()) / statistics.median(base.values()):13.4f} "
              f"{min(mine.values()) / min(base.values()):10.4f} "
              f"{statistics.median(paired):14.4f}  {interval_text(paired):>19}  "
              f"{len(hashes[name])} distinct")
    print("\nestimated Mcycles (CPU seconds x mean pinned-core frequency)")
    mbase = dict(mcycles[reference])
    for name, _, _ in chains:
        mine = dict(mcycles[name])
        paired = [mine[r] / mbase[r] for r in mine if mbase[r] > 0]
        if paired:
            print(f"{name:10} median {statistics.median(mine.values()):10.0f}  paired_median "
                  f"{statistics.median(paired):.4f}  interval {interval_text(paired)}")
    print("\nmedian -Z time-passes seconds (ratio to reference)")
    names = [name for name, _, _ in chains]
    print("pass".ljust(40) + "".join(f"{name:>18}" for name in names))
    for key in KEY_PASSES:
        row = []
        ref = statistics.median(p.get(key, 0.0) for p in phases[reference])
        for name in names:
            value = statistics.median(p.get(key, 0.0) for p in phases[name])
            row.append(f"{value:9.3f} ({value / ref if ref else 0:5.3f})")
        print(key.ljust(40) + "".join(f"{cell:>18}" for cell in row))


if __name__ == "__main__":
    main()
