#!/usr/bin/env python3
"""Measure exact post-LTO partition reuse in one isolated experiment."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import resource
import subprocess
import time


def fingerprint(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def cpu_time():
    own = resource.getrusage(resource.RUSAGE_SELF)
    children = resource.getrusage(resource.RUSAGE_CHILDREN)
    return own.ru_utime + own.ru_stime + children.ru_utime + children.ru_stime


def split_canonical(args, source, prefix):
    env = os.environ | {
        "FRUST_PARTITION_PREFIX": str(prefix),
        "FRUST_PARTITION_COUNT": str(args.parts),
    }
    subprocess.run([
        args.llvm_bin / "opt", f"-load-pass-plugin={args.plugin}",
        "-passes=frust-split-canonical-partitions", "-disable-output", source,
    ], env=env, check=True)


def split_legacy(args, source, prefix):
    subprocess.run([
        args.llvm_bin / "llvm-split", f"-j={args.parts}", "--preserve-locals=false",
        source, "-o", prefix,
    ], check=True)


def emit(args, label, inputs, cached):
    output = args.output / label
    output.mkdir()
    objects = []
    produced = {}
    hits = 0
    start = time.monotonic()
    start_cpu = cpu_time()
    canonical_in_process = args.seed_module is not None and not args.legacy_split
    if args.seed_module is not None:
        prefix = output / "part"
        split = split_canonical if canonical_in_process else split_legacy
        split(args, inputs, prefix)
        inputs = prefix
    cpu_after_split = cpu_time()
    for index in range(args.parts):
        source = Path(f"{inputs}{index}")
        canonical = source
        if not canonical_in_process:
            canonical = output / f"{index}.bc"
            subprocess.run([
                args.llvm_bin / "opt",
                f"-load-pass-plugin={args.plugin}",
                "-passes=strip-dead-prototypes,frust-sort-declarations,verify",
                source, "-o", canonical,
            ], check=True)
        key = fingerprint(canonical)
        obj = cached.get(key)
        if obj is None:
            obj = output / f"{index}.o"
            # llc verifies new inputs. Exact cached bytes were already verified.
            subprocess.run([
                args.llvm_bin / "llc", "-O3", "-filetype=obj", *args.llc_arg,
                "-disable-verify=false", canonical, "-o", obj,
            ], check=True)
            produced[key] = obj
        else:
            hits += 1
        objects.append(obj)

    merged = output / "combined.o"
    subprocess.run([args.linker, "-flavor", "gnu", "-r", *objects, "-o", merged], check=True)
    print(json.dumps({
        "build": label,
        "seconds": time.monotonic() - start,
        "cpu": cpu_time() - start_cpu,
        "split_cpu": cpu_after_split - start_cpu,
        "partition_hits": hits,
        "parts": args.parts,
        "sha256": fingerprint(merged),
    }), flush=True)
    return merged, produced


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--llvm-bin", type=Path, required=True)
    parser.add_argument("--linker", type=Path, required=True)
    parser.add_argument("--plugin", type=lambda path: Path(path).resolve(), required=True)
    parser.add_argument("--seed-prefix", type=Path)
    parser.add_argument("--edited-prefix", type=Path)
    parser.add_argument("--seed-module", type=Path)
    parser.add_argument("--edited-module", type=Path)
    parser.add_argument("--legacy-split", action="store_true")
    parser.add_argument("--llc-arg", action="append", default=[])
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--parts", type=int, default=16)
    args = parser.parse_args()
    if args.parts < 1:
        parser.error("--parts must be positive")

    prefixes = args.seed_prefix is not None and args.edited_prefix is not None
    modules = args.seed_module is not None and args.edited_module is not None
    if prefixes == modules or (prefixes and (args.seed_module or args.edited_module)):
        parser.error("provide both input prefixes or both optimized modules")
    if modules and (args.seed_prefix or args.edited_prefix):
        parser.error("prefixes and optimized modules cannot be mixed")
    if args.legacy_split and not modules:
        parser.error("--legacy-split requires optimized modules")

    args.output.mkdir()
    seed_input = args.seed_module if modules else args.seed_prefix
    edited_input = args.edited_module if modules else args.edited_prefix
    # Keys stay inside the experiment with one fixed LLVM and codegen command.
    _, seed_objects = emit(args, "seed", seed_input, {})
    warm, _ = emit(args, "edited-warm", edited_input, seed_objects)
    fresh, _ = emit(args, "edited-fresh", edited_input, {})
    if fingerprint(warm) != fingerprint(fresh):
        raise SystemExit("warm and fresh relocatable objects differ")


if __name__ == "__main__":
    main()
