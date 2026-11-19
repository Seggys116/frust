#!/usr/bin/env python3
"""Compare reverted executables from different incremental LTO settings."""

import argparse
from pathlib import Path
import runpy
import shutil


def main():
    bench = runpy.run_path(Path(__file__).with_name("cpu-benchmark.py"))
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--lane", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--workload", choices=bench["WORKLOADS"], required=True)
    parser.add_argument("--cpus", required=True)
    parser.add_argument("--runtime", type=int, default=30)
    parser.add_argument("--toolchain", choices=("official", "control", "frust", "candidate"),
                        default="frust")
    args = parser.parse_args()
    if args.runtime < 1:
        parser.error("--runtime must be positive")

    lane, output = args.lane.resolve(), args.output.resolve()
    output.mkdir()
    profiles = ("off16-incremental", "thin16-incremental", "fat16-incremental")
    name = bench["WORKLOADS"][args.workload][1]
    try:
        if args.workload == "fd":
            bench["prepare_fd_inputs"](output, True)
        for profile in profiles:
            binary = lane / args.workload / profile / "0" / args.toolchain / "release" / name
            bench["verify"](args.workload, binary, 0, output)
            target = output / profile / "release"
            target.mkdir(parents=True)
            shutil.copy2(binary, target / name)
        bench["runtime_runs"](args, output, output, args.workload,
                              f"{args.toolchain} incremental LTO profiles", "release", 0, profiles)
    finally:
        if args.workload == "fd":
            for name in ("fd-input", "fd-runtime"):
                if (output / name).is_dir():
                    shutil.rmtree(output / name)


if __name__ == "__main__":
    main()
