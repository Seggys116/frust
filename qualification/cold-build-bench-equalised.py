#!/usr/bin/env python3
"""Paired cold-build comparison (qualification/cold-cpu-bench.py) with the page-cache state of
every toolchain equalised before every build.

Before each cold build, the cached pages of every file of every toolchain under test (all
regular files in bin/, lib/*.so*, and the host rustlib lib/*.so*) are dropped with
POSIX_FADV_DONTNEED (qualification/page-cache-state.py "evict"), so each build faults its
compiler in with ordinary small folios from disk-cache-cold state, whichever toolchain ran
before. Without this, a toolchain whose rustc_driver/libLLVM happen to sit in huge folios is
measured with fewer iTLB misses (see page-cache-state.py). `fincore` records how many bytes of
the toolchains' files were still resident right after the eviction (pages mapped by a running
process cannot be dropped; nothing of these toolchains should be running).

Rounds rotate the order (round r starts with toolchain r mod n; odd rounds run backwards), as
in cold-cpu-bench.py. The first toolchain is the reference. The summary reports, per
toolchain: median and min CPU (user+sys of the Cargo process tree) and wall, ratios of the
medians and minima against the reference, paired per-round ratios, the pair count and the
rounds won (lower CPU than the reference in the same round).

Usage: cold-build-bench-equalised.py --toolchain NAME=PREFIX[:frust] ... --app DIR
       [--release] [--rounds N] [--cpus 6-9] [--jobs 4] --work DIR --out JSONL [--summary JSON]
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
spec = importlib.util.spec_from_file_location("cold_cpu_bench", HERE / "cold-cpu-bench.py")
bench = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bench)


def toolchain_files(prefix):
    prefix = Path(prefix)
    found = [p for p in (prefix / "bin").iterdir() if p.is_file() and not p.is_symlink()]
    found += [p for p in (prefix / "lib").glob("*.so*") if p.is_file() and not p.is_symlink()]
    found += [p for p in (prefix / "lib" / "rustlib").glob("*/lib/*.so")
              if p.is_file() and not p.is_symlink()]
    return sorted(set(found))


def evict(files):
    for path in files:
        fd = os.open(path, os.O_RDONLY)
        try:
            os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
        finally:
            os.close(fd)


def resident_bytes(files):
    if not shutil.which("fincore"):
        return None
    out = subprocess.run(["fincore", "--bytes", "--noheadings", "--raw", "--output", "RES"]
                         + [str(f) for f in files], capture_output=True, text=True, check=True)
    return sum(int(line) for line in out.stdout.split())


def summarise(chains, records):
    reference = chains[0].name
    by = {c.name: {r["round"]: r for r in records if r["toolchain"] == c.name} for c in chains}
    base = by[reference]
    rows = []
    for chain in chains:
        mine = by[chain.name]
        cpu = [r["cpu"] for r in mine.values()]
        wall = [r["wall"] for r in mine.values()]
        rounds = sorted(set(mine) & set(base))
        paired_cpu = [mine[r]["cpu"] / base[r]["cpu"] for r in rounds]
        paired_wall = [mine[r]["wall"] / base[r]["wall"] for r in rounds]
        base_cpu = [base[r]["cpu"] for r in base]
        base_wall = [base[r]["wall"] for r in base]
        rows.append(dict(
            toolchain=chain.name, rounds=len(cpu), pairs=len(rounds),
            cpu_median=statistics.median(cpu), cpu_min=min(cpu),
            wall_median=statistics.median(wall), wall_min=min(wall),
            cpu_ratio_median=statistics.median(cpu) / statistics.median(base_cpu),
            cpu_ratio_min=min(cpu) / min(base_cpu),
            cpu_paired_median=statistics.median(paired_cpu), cpu_paired_max=max(paired_cpu),
            wall_ratio_median=statistics.median(wall) / statistics.median(base_wall),
            wall_ratio_min=min(wall) / min(base_wall),
            wall_paired_median=statistics.median(paired_wall),
            cpu_wins=sum(1 for r in rounds if mine[r]["cpu"] < base[r]["cpu"]),
            wall_wins=sum(1 for r in rounds if mine[r]["wall"] < base[r]["wall"]),
        ))
    return rows


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
    parser.add_argument("--summary")
    parser.add_argument("--warmup", type=int, default=0,
                        help="discarded cold builds before round 0, cycling through the "
                             "toolchains; the first measured build otherwise runs on a CPU "
                             "rested by the idle time before the series and is always the "
                             "reference's")
    args = parser.parse_args()

    chains = [bench.Toolchain(spec) for spec in args.toolchain]
    files = sorted({f for chain in chains for f in toolchain_files(chain.prefix)})
    work = Path(args.work).resolve()
    work.mkdir(parents=True, exist_ok=True)
    app = Path(args.app).resolve()
    profile = "release" if args.release else "dev"
    records = []
    for index in range(args.warmup):
        chain = chains[index % len(chains)]
        target = work / f"{chain.name}-target"
        shutil.rmtree(target, ignore_errors=True)
        command = ["taskset", "-c", args.cpus, str(chain.cargo), "build", "--locked",
                   "--offline", "-j", str(args.jobs)] + (["--release"] if args.release else [])
        cpu, _, _, wall, _, status = bench.run_tree(command, app, chain.env(work, target),
                                                    work / "warmup.log")
        if status != 0:
            sys.exit(f"warm-up build with {chain.name} failed; see {work / 'warmup.log'}")
        print(f"warmup {chain.name:10} cpu={cpu:8.2f} wall={wall:7.2f} (discarded)", flush=True)
        shutil.rmtree(target, ignore_errors=True)
    for round_number in range(args.rounds):
        shift = round_number % len(chains)
        order = chains[shift:] + chains[:shift]
        if round_number % 2:
            order.reverse()
        for position, chain in enumerate(order):
            target = work / f"{chain.name}-target"
            shutil.rmtree(target, ignore_errors=True)
            shutil.rmtree(work / f"{chain.name}-xdg", ignore_errors=True)
            evict(files)
            resident = resident_bytes(files)
            command = ["taskset", "-c", args.cpus, str(chain.cargo), "build", "--locked",
                       "--offline", "-j", str(args.jobs)] + (["--release"] if args.release else [])
            log = work / f"{chain.name}-{profile}.log"
            cpu, user, system, wall, rss, status = bench.run_tree(
                command, app, chain.env(work, target), log)
            units = sum(1 for line in open(log, errors="replace")
                        if line.lstrip().startswith("Compiling "))
            record = dict(toolchain=chain.name, app=app.name, profile=profile,
                          round=round_number, position=position, cpu=cpu, user=user,
                          sys=system, wall=wall, rss_kib=rss, status=status, units=units,
                          cpus=args.cpus, jobs=args.jobs, page_cache="evicted-all",
                          resident_after_evict=resident)
            with open(args.out, "a") as handle:
                handle.write(json.dumps(record) + "\n")
            if status != 0:
                sys.exit(f"{chain.name} failed; see {log}")
            records.append(record)
            print(f"r{round_number} {chain.name:10} cpu={cpu:8.2f} wall={wall:7.2f} "
                  f"units={units} resident={resident}", flush=True)
            shutil.rmtree(target, ignore_errors=True)

    rows = summarise(chains, records)
    print(f"\n{app.name} {profile} cold, {args.rounds} rounds, cpus {args.cpus}, -j{args.jobs},"
          f" page cache evicted for all toolchains before every build")
    print("toolchain      cpu_med  cpu_min  r_med  r_min  paired_med  wall_r_med  wall_r_min"
          "  pairs  cpu_wins  wall_wins")
    for row in rows:
        print(f"{row['toolchain']:12} {row['cpu_median']:8.2f} {row['cpu_min']:8.2f} "
              f"{row['cpu_ratio_median']:6.4f} {row['cpu_ratio_min']:6.4f} "
              f"{row['cpu_paired_median']:10.4f} {row['wall_ratio_median']:11.4f} "
              f"{row['wall_ratio_min']:11.4f} {row['pairs']:6} {row['cpu_wins']:9} "
              f"{row['wall_wins']:10}")
    if args.summary:
        Path(args.summary).write_text(json.dumps(dict(
            app=app.name, profile=profile, rounds=args.rounds, cpus=args.cpus, jobs=args.jobs,
            reference=chains[0].name, rows=rows), indent=1) + "\n")


if __name__ == "__main__":
    main()
