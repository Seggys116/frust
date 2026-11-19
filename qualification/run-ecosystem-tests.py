#!/usr/bin/env python3
"""Compare `cargo test` outcomes of official Rust and fRust (with its unit cache) on locked cohorts.

For each application the script runs the test suite three times: official Cargo in a clean
target, fRust with an empty cache (stores units), and fRust again after deleting the target
(restores units). The aggregate of every `test result:` line (passed, failed, ignored,
filtered) must be identical across the three runs.
"""
import argparse
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys

RESULT = re.compile(r"test result: (\w+)\. (\d+) passed; (\d+) failed; (\d+) ignored; "
                    r"(\d+) measured; (\d+) filtered out")


def run(cargo, rustc, app, target, store, extra_env, log, jobs, release):
    env = {key: value for key, value in os.environ.items()
           if not key.startswith(("CARGO", "RUSTC", "RUSTFLAGS", "FRUST", "XDG"))}
    env.update(CARGO_TARGET_DIR=str(target), CARGO_HOME=str(Path.home() / ".cargo"),
               CARGO_TERM_COLOR="never", CARGO_INCREMENTAL="0", FRUST_PROGRESS="plain",
               XDG_CACHE_HOME=str(store), XDG_CONFIG_HOME=str(store / "config"),
               PATH=os.pathsep.join([str(Path(cargo).parent), env.get("PATH", "")]),
               RUSTC=str(rustc), **extra_env)
    command = [str(cargo), "test", "--locked", "--offline", "-j", str(jobs), "--workspace",
               "--no-fail-fast"] + (["--release"] if release else [])
    with open(log, "wb") as sink:
        status = subprocess.run(command, cwd=app, env=env, stdout=sink,
                                stderr=subprocess.STDOUT).returncode
    totals = [0, 0, 0, 0, 0]
    results = 0
    for line in log.read_text(errors="replace").splitlines():
        match = RESULT.search(line)
        if match:
            results += 1
            for index, value in enumerate(match.groups()[1:]):
                totals[index] += int(value)
    return status, results, dict(zip(("passed", "failed", "ignored", "measured", "filtered"), totals))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--official-cargo", required=True)
    parser.add_argument("--official-rustc", required=True)
    parser.add_argument("--frust-cargo", required=True)
    parser.add_argument("--frust-rustc", required=True)
    parser.add_argument("--app", action="append", required=True, help="cohort directory (repeatable)")
    parser.add_argument("--release", action="store_true")
    parser.add_argument("--jobs", type=int, default=6)
    parser.add_argument("--work", required=True)
    parser.add_argument("--out", required=True)
    args = parser.parse_args()
    work = Path(args.work)
    work.mkdir(parents=True, exist_ok=True)
    failures = 0
    rows = []
    for app in map(Path, args.app):
        runs = {}
        store = work / "frust-store"
        shutil.rmtree(store, ignore_errors=True)
        target = work / "target"
        for label, cargo, rustc, cache_env in (
                ("official", args.official_cargo, args.official_rustc, {"FRUST_UNIT_CACHE": "0"}),
                ("frust-cold", args.frust_cargo, args.frust_rustc, {}),
                ("frust-restored", args.frust_cargo, args.frust_rustc, {})):
            shutil.rmtree(target, ignore_errors=True)
            runs[label] = run(cargo, rustc, app, target, store / label.split("-")[0], cache_env,
                              work / f"{app.name}-{label}.log", args.jobs, args.release)
        base = runs["official"]
        for label in ("frust-cold", "frust-restored"):
            same = runs[label][2] == base[2] and runs[label][0] == base[0]
            failures += 0 if same else 1
            rows.append(dict(app=app.name, run=label, status=runs[label][0], totals=runs[label][2],
                             official_status=base[0], official_totals=base[2], identical=same))
            print(f"{'PASS' if same else 'FAIL'} {app.name} {label}: {runs[label][2]} "
                  f"(official {base[2]}, exit {runs[label][0]} vs {base[0]})", flush=True)
    Path(args.out).write_text(json.dumps(rows, indent=1))
    shutil.rmtree(target, ignore_errors=True)
    shutil.rmtree(store, ignore_errors=True)
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
