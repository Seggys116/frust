#!/usr/bin/env python3
"""Measure runtime and stripped size for one program built by both toolchains.

The program is built with the shared qualified release profile. Timed runs use
the unstripped binary. Size is taken from a stripped copy. Runs alternate which
binary goes first after one untimed warm-up each. The commands and corpus are
frozen in qualification/CORPUS.md. Results are appended in the format
`frust-gates --samples` reads.
"""

import argparse
import hashlib
import os
import pathlib
import shutil
import subprocess
import sys
import time

ROOT = pathlib.Path(__file__).resolve().parent.parent
COHORTS = ROOT / "target" / "compiler-rewrite" / "cohorts"
PROGRAMS = {
    "ripgrep": ("ripgrep", "rg"),
    "rustfmt": ("rustfmt-nightly", "rustfmt"),
    "oxipng": ("oxipng", "oxipng"),
}
CLEARED = (
    "RUSTC CARGO RUSTFLAGS RUSTUP_TOOLCHAIN FRUST_INVOCATION FRUST_PROGRESS FRUST_CARGO FRUST_CENTRAL_TARGET "
    "CARGO_INCREMENTAL CARGO_ENCODED_RUSTFLAGS RUSTC_WRAPPER RUSTC_WORKSPACE_WRAPPER CARGO_BUILD_TARGET "
    "RUSTC_BOOTSTRAP CARGO_BUILD_RUSTFLAGS CARGO_BUILD_RUSTC CARGO_BUILD_RUSTC_WRAPPER "
    "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER"
).split()
PROFILE = {
    "CARGO_PROFILE_RELEASE_OPT_LEVEL": "3",
    "CARGO_PROFILE_RELEASE_LTO": "fat",
    "CARGO_PROFILE_RELEASE_CODEGEN_UNITS": "1",
    "CARGO_PROFILE_RELEASE_INCREMENTAL": "true",
    "CARGO_PROFILE_RELEASE_DEBUG": "0",
    "CARGO_PROFILE_RELEASE_STRIP": "none",
    "CARGO_TERM_COLOR": "never",
    "FRUST_PROGRESS": "plain",
}


def parse_args():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--control", required=True, type=pathlib.Path, help="control toolchain bin directory")
    parser.add_argument("--frust", required=True, type=pathlib.Path, help="fRust toolchain bin directory")
    parser.add_argument("--workload", required=True, choices=sorted(PROGRAMS))
    parser.add_argument("--corpus", required=True, type=pathlib.Path, help="directory written by make_corpus.py")
    parser.add_argument("--work", required=True, type=pathlib.Path)
    parser.add_argument("--samples", required=True, type=pathlib.Path)
    parser.add_argument("--runs", type=int, default=30)
    parser.add_argument("--frust-env", action="append", default=[], metavar="KEY=VALUE")
    return parser.parse_args()


def build_env(bins, extra):
    env = {key: value for key, value in os.environ.items() if key not in CLEARED}
    env["PATH"] = f"{bins}:/usr/bin:/bin"
    env["RUSTC"] = str(bins / "frustc") if (bins / "frustc").exists() else str(bins / "rustc")
    env.update(PROFILE)
    env.update(extra)
    return env


def build(bins, extra, workload, target):
    package, binary = PROGRAMS[workload]
    env = build_env(bins, extra)
    env["CARGO_TARGET_DIR"] = str(target)
    command = [str(bins / "cargo"), "build", "--release", "--locked", "--offline", "-p", package]
    result = subprocess.run(command, cwd=COHORTS / workload, env=env, capture_output=True, text=True)
    if result.returncode != 0:
        sys.exit(f"{workload} build failed with {bins}\n{result.stderr[-4000:]}")
    return target / "release" / binary


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def stripped_size(binary, work, name):
    copy = work / f"stripped-{name}"
    subprocess.run(["strip", "--strip-unneeded", "-o", str(copy), str(binary)], check=True)
    return copy.stat().st_size


def command_for(workload, binary, corpus, scratch):
    """Return the frozen command and a setup callable that resets its inputs."""
    if workload == "ripgrep":
        return [str(binary), "--no-config", "-j", "1", "-F", "frust_needle_7", "search.txt"], corpus, lambda: None
    if workload == "rustfmt":
        tree = scratch / "rust"

        def reset():
            shutil.rmtree(tree, ignore_errors=True)
            shutil.copytree(corpus / "rust", tree)

        files = sorted(path.name for path in (corpus / "rust").glob("item_*.rs"))
        return [str(binary), "--edition", "2021", *files], tree, reset
    out = scratch / "out"

    def reset():
        shutil.rmtree(out, ignore_errors=True)

    pngs = sorted(str(path) for path in (corpus / "pngs").glob("*.png"))
    return [str(binary), "-o", "4", "--threads", "1", "--dir", str(out), *pngs], corpus, reset


def timed_run(command, cwd, reset):
    reset()
    start = time.perf_counter()
    result = subprocess.run(command, cwd=cwd, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    seconds = time.perf_counter() - start
    if result.returncode != 0:
        sys.exit(f"{command[0]} exited {result.returncode}\n{result.stderr.decode(errors='replace')[-2000:]}")
    return seconds


def main():
    args = parse_args()
    args.work.mkdir(parents=True, exist_ok=True)
    frust_env = dict(item.partition("=")[::2] for item in args.frust_env)
    binaries = {
        "control": build(args.control.resolve(), {}, args.workload, args.work / "target-control"),
        "frust": build(args.frust.resolve(), frust_env, args.workload, args.work / "target-frust"),
    }
    identical = sha256(binaries["control"]) == sha256(binaries["frust"])
    sizes = {name: stripped_size(path, args.work, name) for name, path in binaries.items()}

    commands = {}
    for name, path in binaries.items():
        scratch = args.work / f"run-{name}"
        scratch.mkdir(exist_ok=True)
        commands[name] = command_for(args.workload, path, args.corpus.resolve(), scratch)
        timed_run(*commands[name])
    seconds = {"control": [], "frust": []}
    for index in range(args.runs):
        order = ("control", "frust") if index % 2 == 0 else ("frust", "control")
        for name in order:
            seconds[name].append(timed_run(*commands[name]))

    frust_list = ",".join(f"{value:.9f}" for value in seconds["frust"])
    control_list = ",".join(f"{value:.9f}" for value in seconds["control"])
    with args.samples.open("a") as out:
        out.write(f"runtime {args.workload} {int(identical)} {frust_list} {control_list}\n")
        out.write(f"size {args.workload} {sizes['frust']} {sizes['control']}\n")
    print(f"{args.workload} identical={identical} stripped control {sizes['control']} frust {sizes['frust']}")


if __name__ == "__main__":
    main()
