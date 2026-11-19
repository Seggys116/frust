#!/usr/bin/env python3
"""Measure exact post-LTO partition reuse in one isolated experiment."""

import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import time


def fingerprint(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def emit(args, label, inputs, cached):
    output = args.output / label
    output.mkdir()
    objects = []
    produced = {}
    hits = 0
    start = time.monotonic()
    for index in range(args.parts):
        source = Path(f"{inputs}{index}")
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
            subprocess.run([
                args.llvm_bin / "llc", "-O3", "-filetype=obj", canonical, "-o", obj,
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
    parser.add_argument("--seed-prefix", type=Path, required=True)
    parser.add_argument("--edited-prefix", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--parts", type=int, default=16)
    args = parser.parse_args()
    if args.parts < 1:
        parser.error("--parts must be positive")

    args.output.mkdir()
    # Keys stay inside the experiment with one fixed LLVM and codegen command.
    _, seed_objects = emit(args, "seed", args.seed_prefix, {})
    warm, _ = emit(args, "edited-warm", args.edited_prefix, seed_objects)
    fresh, _ = emit(args, "edited-fresh", args.edited_prefix, {})
    if fingerprint(warm) != fingerprint(fresh):
        raise SystemExit("warm and fresh relocatable objects differ")


if __name__ == "__main__":
    main()
