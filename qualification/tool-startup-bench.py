#!/usr/bin/env python3
"""Measure paired CPU and peak RSS for the packaged command entry points."""

import argparse
import json
import os
from pathlib import Path
import statistics
import subprocess
import sys
import tempfile


TOOLS = {
    "fcargo": "-V",
    "frustc": "-vV",
    "fclippy": "--version",
    "frustdoc": "-V",
    "frustfmt": "--version",
    "rust-analyzer": "--version",
}


def sample(binary, flag, env):
    with tempfile.TemporaryFile() as output:
        child = subprocess.Popen([str(binary), flag], env=env, stdin=subprocess.DEVNULL,
                                 stdout=output, stderr=subprocess.STDOUT)
        _, status, usage = os.wait4(child.pid, 0)
        child.returncode = os.waitstatus_to_exitcode(status)
        output.seek(0)
        contents = output.read()
    if child.returncode or not contents:
        raise RuntimeError(f"{binary} {flag} exited {child.returncode}, output {contents!r}")
    return {
        "cpu": usage.ru_utime + usage.ru_stime,
        "rss_bytes": usage.ru_maxrss * (1 if sys.platform == "darwin" else 1024),
        "output": contents.decode(errors="replace"),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--control", type=Path, required=True, help="control toolchain bin directory")
    parser.add_argument("--frust", type=Path, required=True, help="candidate toolchain bin directory")
    parser.add_argument("--rounds", type=int, default=30)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    if args.rounds < 2:
        parser.error("use at least two rounds")
    sides = {"control": args.control.absolute(), "frust": args.frust.absolute()}
    env = {
        key: value for key, value in os.environ.items()
        if not key.startswith(("CARGO", "RUSTC", "RUSTUP", "RUSTDOC", "FRUST"))
    }
    env.update(FRUST_PROGRESS="plain", TERM="dumb")
    ratios = {tool: [] for tool in TOOLS}
    with args.out.open("x", encoding="utf-8") as output:
        for index in range(args.rounds):
            order = list(sides) if index % 2 == 0 else list(reversed(sides))
            for tool, flag in TOOLS.items():
                pair = {label: sample(sides[label] / tool, flag, env) for label in order}
                if pair["control"]["output"] != pair["frust"]["output"]:
                    raise RuntimeError(f"{tool} {flag} output differs: {pair}")
                if pair["control"]["cpu"] > 0:
                    ratios[tool].append(pair["frust"]["cpu"] / pair["control"]["cpu"])
                output.write(json.dumps({"schema": "frust-startup-1", "tool": tool, "flag": flag,
                                         "pair": index, "order": order, **pair}) + "\n")
    for tool, values in ratios.items():
        median = f"{statistics.median(values) * 100:.1f}%" if values else "unavailable"
        print(f"{tool}: median candidate CPU {median} of control ({len(values)} pairs)")


if __name__ == "__main__":
    main()
