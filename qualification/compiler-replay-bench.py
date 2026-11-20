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


def capture(prefix, app, profile, crate, work):
    target = work / f"prep-{prefix.name}-{profile}"
    env = clean_env()
    env.update(CARGO_TARGET_DIR=str(target), CARGO_HOME=str(Path.home() / ".cargo"),
               CARGO_INCREMENTAL="0", CARGO_TERM_COLOR="never", FRUST_UNIT_CACHE="0",
               RUSTC=str(prefix / "bin" / "rustc"),
               PATH=os.pathsep.join([str(prefix / "bin"), "/usr/bin", "/bin"]))
    log = work / f"prep-{prefix.name}-{profile}.log"
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
            return words, run_env
    sys.exit(f"no rustc invocation for {crate} in {log}")


def run_measured(words, run_env, out_dir, cpu, passes):
    """Run the replay as a direct child so wait4 reports its rusage."""
    out_dir.mkdir(parents=True, exist_ok=True)
    args = list(words[1:])
    for index, word in enumerate(args):
        if word == "--out-dir":
            args[index + 1] = str(out_dir)
    if passes:
        args += ["-Z", "time-passes"]
    env = clean_env()
    env.update(run_env)
    if passes:
        env["RUSTC_BOOTSTRAP"] = "1"
    cwd = run_env.get("CARGO_MANIFEST_DIR") or os.getcwd()
    log = out_dir / ("passes.log" if passes else "plain.log")
    start = time.monotonic()
    with open(log, "wb") as sink:
        child = subprocess.Popen(["taskset", "-c", cpu, words[0]] + args, cwd=cwd, env=env,
                                 stdout=sink, stderr=subprocess.STDOUT)
        _, status, usage = os.wait4(child.pid, 0)
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
    for path in sorted(directory.glob("*.rlib")) + sorted(directory.glob("*.rmeta")):
        hasher.update(path.name.encode())
        hasher.update(path.read_bytes())
    return hasher.hexdigest()[:16]


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
    args = parser.parse_args()

    work = Path(args.work)
    work.mkdir(parents=True, exist_ok=True)
    app = Path(args.app)
    chains = []
    for spec in args.toolchain:
        name, _, prefix = spec.partition("=")
        prefix = Path(prefix)
        words, run_env = capture(prefix, app, args.profile, args.crate, work)
        if Path(words[0]).resolve() != (prefix / "bin" / "rustc").resolve():
            sys.exit(f"{name}: captured compiler {words[0]} is not {prefix}/bin/rustc")
        chains.append((name, words, run_env))
        print(f"captured {name}: {args.crate} {args.profile}", flush=True)

    cpu = {name: [] for name, _, _ in chains}
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
            total, user, system, wall, _ = run_measured(words, run_env, out_dir, args.cpu, False)
            hashes[name].add(digest(out_dir))
            shutil.rmtree(out_dir, ignore_errors=True)
            _, _, _, _, log = run_measured(words, run_env, out_dir, args.cpu, True)
            passes = parse_passes(log)
            cpu[name].append((round_number, total))
            phases[name].append(passes)
            with open(args.out, "a") as handle:
                handle.write(json.dumps(dict(toolchain=name, crate=args.crate,
                                             profile=args.profile, round=round_number,
                                             cpu=total, user=user, sys=system, wall=wall,
                                             passes=passes)) + "\n")
            print(f"r{round_number} {name:10} cpu={total:7.3f} "
                  f"passes_total={passes.get('total', 0):7.3f}", flush=True)

    reference = chains[0][0]
    base = dict(cpu[reference])
    print(f"\n{args.crate} {args.profile}, {args.rounds} rounds, cpu {args.cpu}")
    print("toolchain    median_cpu  min_cpu  ratio_median  ratio_min  paired_median  outputs")
    for name, _, _ in chains:
        mine = dict(cpu[name])
        paired = [mine[r] / base[r] for r in mine]
        print(f"{name:10} {statistics.median(mine.values()):11.3f} {min(mine.values()):8.3f} "
              f"{statistics.median(mine.values()) / statistics.median(base.values()):13.4f} "
              f"{min(mine.values()) / min(base.values()):10.4f} "
              f"{statistics.median(paired):14.4f}  {len(hashes[name])} distinct")
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
