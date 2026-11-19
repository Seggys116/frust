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


def split_canonical(args, source, prefix, cached):
    env = os.environ | {
        "FRUST_PARTITION_PREFIX": str(prefix),
        "FRUST_PARTITION_COUNT": str(args.parts),
    }
    env.pop("FRUST_PARTITION_REUSE_KEYS", None)
    if cached:
        keys = prefix.parent / "reuse.keys"
        keys.write_text("\n".join(cached) + "\n")
        env["FRUST_PARTITION_REUSE_KEYS"] = str(keys)
    subprocess.run([
        args.llvm_bin / "opt", f"-load-pass-plugin={args.plugin}",
        "-passes=frust-split-canonical-partitions", "-disable-output", source,
    ], env=env, check=True)
    return Path(f"{prefix}.keys").read_text().splitlines()


def split_legacy(args, source, prefix):
    subprocess.run([
        args.llvm_bin / "llvm-split", f"-j={args.parts}", "--preserve-locals=false",
        source, "-o", prefix,
    ], check=True)


def emit(args, label, inputs, cached, retain_hit_bitcode=False, exact_cache=None):
    output = args.output / label
    output.mkdir()
    objects = []
    produced = {}
    hits = 0
    start = time.monotonic()
    start_cpu = cpu_time()
    merged = output / "combined.o"
    module_key = None
    if exact_cache is not None:
        module_key = fingerprint(inputs)
        known = exact_cache.get(module_key)
        if known is not None:
            # Output objects are immutable within this isolated replay.
            os.link(known, merged)
            print(json.dumps({
                "build": label,
                "seconds": time.monotonic() - start,
                "cpu": cpu_time() - start_cpu,
                "split_cpu": 0,
                "emission_cpu": 0,
                "link_cpu": 0,
                "partition_hits": args.parts,
                "bitcode_bytes": 0,
                "retained_object_bytes": 0,
                "parts": args.parts,
                "exact_module_hit": True,
                "sha256": fingerprint(merged),
            }), flush=True)
            return merged, {}
    canonical_in_process = args.seed_module is not None and not args.legacy_split
    keys = None
    if args.seed_module is not None:
        prefix = output / "part"
        if canonical_in_process:
            keys = split_canonical(args, inputs, prefix, {} if retain_hit_bitcode else cached)
            if len(keys) != args.parts or any(len(key) != 64 for key in keys):
                raise RuntimeError("splitter manifest does not match the partition count")
        else:
            split_legacy(args, inputs, prefix)
        inputs = prefix
    cpu_after_split = cpu_time()
    bitcode_bytes = 0
    retained_object_bytes = 0
    emission_cpu = 0.0
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
        if canonical.is_file():
            bitcode_bytes += canonical.stat().st_size
        key = fingerprint(canonical) if keys is None else keys[index]
        obj = cached.get(key)
        if obj is None:
            obj = output / f"{index}.o"
            # llc verifies new inputs. Exact cached bytes were already verified.
            emission_start = cpu_time()
            subprocess.run([
                args.llvm_bin / "llc", "-O3", "-filetype=obj", *args.llc_arg,
                "-disable-verify=false", canonical, "-o", obj,
            ], check=True)
            emission_cpu += cpu_time() - emission_start
            retained_object_bytes += obj.stat().st_size
            produced[key] = obj
        else:
            hits += 1
        objects.append(obj)

    link_start = cpu_time()
    subprocess.run([args.linker, "-flavor", "gnu", "-r", *objects, "-o", merged], check=True)
    if exact_cache is not None:
        exact_cache[module_key] = merged
    finish_cpu = cpu_time()
    (output / "objects.json").write_text(json.dumps([str(path.resolve()) for path in objects]) + "\n")
    print(json.dumps({
        "build": label,
        "seconds": time.monotonic() - start,
        "cpu": finish_cpu - start_cpu,
        "split_cpu": cpu_after_split - start_cpu,
        "emission_cpu": emission_cpu,
        "link_cpu": finish_cpu - link_start,
        "partition_hits": hits,
        "bitcode_bytes": bitcode_bytes,
        "retained_object_bytes": retained_object_bytes,
        "parts": args.parts,
        "exact_module_hit": False,
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
    parser.add_argument("--retain-hit-bitcode", action="store_true",
                        help="write canonical bitcode for hits as well as misses")
    parser.add_argument("--compare-warm", type=int, default=0, metavar="PAIRS",
                        help="alternate retaining and eliding hit bitcode with a shared seed")
    parser.add_argument("--compare-exact", type=int, default=0, metavar="PAIRS",
                        help="replay exact seed and edit modules with and without split bypass")
    parser.add_argument("--llc-arg", action="append", default=[])
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--parts", type=int, default=16)
    args = parser.parse_args()
    if args.parts < 1:
        parser.error("--parts must be positive")
    if min(args.compare_warm, args.compare_exact) < 0:
        parser.error("comparison pair counts cannot be negative")

    prefixes = args.seed_prefix is not None and args.edited_prefix is not None
    modules = args.seed_module is not None and args.edited_module is not None
    if prefixes == modules or (prefixes and (args.seed_module or args.edited_module)):
        parser.error("provide both input prefixes or both optimized modules")
    if modules and (args.seed_prefix or args.edited_prefix):
        parser.error("prefixes and optimized modules cannot be mixed")
    if args.legacy_split and not modules:
        parser.error("--legacy-split requires optimized modules")
    if (args.compare_warm or args.compare_exact) and (
            not modules or args.legacy_split or args.retain_hit_bitcode):
        parser.error("comparisons require the fused splitter without --retain-hit-bitcode")

    args.output.mkdir()
    seed_input = args.seed_module if modules else args.seed_prefix
    edited_input = args.edited_module if modules else args.edited_prefix
    # Keys stay inside the experiment with one fixed LLVM and codegen command.
    exact_cache = {} if args.compare_exact else None
    seed, seed_objects = emit(args, "seed", seed_input, {}, exact_cache=exact_cache)
    warm, edited_objects = emit(args, "edited-warm", edited_input, seed_objects,
                                args.retain_hit_bitcode, exact_cache)
    fresh, _ = emit(args, "edited-fresh", edited_input, {})
    expected = fingerprint(fresh)
    if fingerprint(warm) != expected:
        raise SystemExit("warm and fresh relocatable objects differ")
    for index in range(args.compare_warm):
        order = ("retain", "elide") if index % 2 else ("elide", "retain")
        for mode in order:
            merged, _ = emit(args, f"{mode}-{index + 1}", edited_input, seed_objects,
                             retain_hit_bitcode=mode == "retain")
            if fingerprint(merged) != expected:
                raise SystemExit(f"{mode} and fresh relocatable objects differ")
    known_objects = seed_objects | edited_objects
    for index in range(args.compare_exact):
        source, original = (seed_input, seed) if index % 2 == 0 else (edited_input, warm)
        order = ("exact", "split") if index % 4 < 2 else ("split", "exact")
        for mode in order:
            merged, _ = emit(args, f"{mode}-replay-{index + 1}", source, known_objects,
                             exact_cache=exact_cache if mode == "exact" else None)
            if fingerprint(merged) != fingerprint(original):
                raise SystemExit(f"{mode} and original relocatable objects differ")


if __name__ == "__main__":
    main()
