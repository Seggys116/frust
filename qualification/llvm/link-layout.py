#!/usr/bin/env python3
"""Replay an ELF link with partition objects and measure output runtime."""

import argparse
import hashlib
import json
from pathlib import Path
import random
import re
import resource
from statistics import median
import subprocess
import tempfile

def cpu():
    usage = resource.getrusage(resource.RUSAGE_CHILDREN)
    return usage.ru_utime + usage.ru_stime

def run(command, **kw):
    start = cpu()
    result = subprocess.run(command, check=True, **kw)
    return cpu() - start, result

def link(out, command, old_object, label, objects, order=None):
    if command.count(str(old_object)) != 1:
        raise RuntimeError("captured link must contain one replaceable LTO object")
    argv = []
    for arg in command:
        argv.extend(map(str, objects) if arg == str(old_object) else [arg])
    executable = out / label
    argv[argv.index("-o") + 1] = str(executable)
    if order:
        argv.append(f"-Wl,--symbol-ordering-file={order},--no-warn-symbol-ordering")
    cpu_seconds, _ = run(["cc", *argv])
    stripped = out / f"{label}.strip"
    run(["strip", "--strip-unneeded", "-o", stripped, executable])
    print(json.dumps(dict(kind="link", variant=label, cpu=cpu_seconds, stripped=stripped.stat().st_size)),
          flush=True)
    return executable

def run_runtime(app, corpus, tree, repetitions, binaries):
    if app == "ripgrep":
        tree = [corpus / "search.txt"] * 16
        scenarios = {"literal": ["--no-config", "-j", "1", "-c", "-F", "frust_needle_7", *tree],
                     "regex": ["--no-config", "-j", "1", "-c", r"frust_needle_[0-9]+", *tree]}
    else:
        for index in range(32):
            leaf = tree / str(index)
            leaf.mkdir()
            for entry in range(400):
                (leaf / f"file-{entry:04}.rs").touch()
        scenarios = {"walk": ["-j", "1", "--no-ignore", "--type", "f", "", tree]}

    rng = random.Random(44)
    for case, args in scenarios.items():
        rows = []
        reference = None
        for repeat in range(repetitions + 4):
            order = list(binaries)
            rng.shuffle(order)
            pair = {}
            for mode in order:
                seconds, result = run([binaries[mode], *args], capture_output=True)
                signature = hashlib.sha256(b"\n".join(sorted(result.stdout.splitlines()))).hexdigest()
                if reference is None:
                    reference = signature
                assert signature == reference, (case, mode, result.stdout[:100])
                pair[mode] = seconds
            if repeat >= 4:
                rows.append(pair)
        result = {}
        for mode in tuple(binaries)[1:]:
            ratios = [row[mode] / row["stock"] for row in rows]
            bounds = sorted(median(rng.choices(ratios, k=len(ratios))) for _ in range(2000))
            result[mode] = dict(median=median(ratios), upper95=bounds[1899])
        print(json.dumps(dict(kind="runtime", case=case, ratios=result, pairs=rows,
                              output_sha256=reference)), flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for flag in ("fixture", "corpus", "llvm-bin", "output"):
        parser.add_argument("--" + flag, type=lambda path: Path(path).resolve(), required=True)
    parser.add_argument("--workload", choices=("ripgrep", "fd"), required=True)
    parser.add_argument("--sectioned-probe", type=Path,
                        help="partition probe output built with function and data sections")
    parser.add_argument("--pairs", type=int, default=30)
    args = parser.parse_args()
    if args.pairs < 1:
        parser.error("--pairs must be positive")

    app, fixture, corpus, llvm, out = args.workload, args.fixture, args.corpus, args.llvm_bin, args.output
    out.mkdir()
    command = json.loads((fixture / "edit-link/args.json").read_text())
    old_object, = (fixture / "edit-link").glob("*.o")
    name = "rg" if app == "ripgrep" else "fd"
    baseline = fixture / f"edit-{name}"
    seed = fixture / f"seed-{name}"
    parts = fixture / "parts-16/edited-fresh"

    names = out / "layout.txt"
    _, symbols = run([llvm / "llvm-nm", "--numeric-sort", "--defined-only", seed], capture_output=True)
    ordered = []
    for line in symbols.stdout.splitlines():
        columns = line.split(maxsplit=2)
        if len(columns) == 3 and columns[1] in {b"t", b"T", b"W", b"w"}:
            ordered.append(columns[2])
    if len(ordered) < 100:
        seed_obj, = (fixture / "seed-link").glob("*.o")
        _, table = run(["readelf", "-W", "-S", seed_obj], capture_output=True)
        ordered = re.findall(rb"\]\s+\.text\.([^\s]+)\s+PROGBITS", table.stdout)
    assert len(ordered) > 100
    names.write_bytes(b"\n".join(ordered) + b"\n")
    if args.sectioned_probe:
        objects = json.loads((args.sectioned_probe / "edited-fresh/objects.json").read_text())
    else:
        objects = []
        for index in range(16):
            output = out / f"{index}.o"
            run([llvm / "llc", "-O3", "-relocation-model=pic", "-function-sections", "-data-sections",
                 "-filetype=obj", parts / f"part{index}", "-o", output], capture_output=True)
            objects.append(output)
    plain = link(out, command, old_object, "sections", objects)
    ordered_binary = link(out, command, old_object, "ordered", objects, names)
    if args.sectioned_probe:
        warm = json.loads((args.sectioned_probe / "edited-warm/objects.json").read_text())
        warm_binary = link(out, command, old_object, "ordered-warm", warm, names)
        if warm_binary.read_bytes() != ordered_binary.read_bytes():
            raise RuntimeError("warm and fresh ordered executables differ")
    previous = link(out, command, old_object, "unsectioned", [parts / f"{index}.o" for index in range(16)])

    for binary in (baseline, previous, plain, ordered_binary):
        if app == "ripgrep":
            _, probe = run([binary, "--no-config", "--stats", "rust"], input=b"rust\n", capture_output=True)
            assert b"2 matches\n" in probe.stdout
        else:
            probe = subprocess.run([binary, "--no-ignore", ":", corpus / "rust"],
                                   capture_output=True)
            assert probe.returncode != 0 and b"path-separation character" in probe.stderr


    binaries = {"stock": baseline, "unsectioned": previous, "sections": plain, "ordered": ordered_binary}
    with tempfile.TemporaryDirectory(dir=out, prefix="runtime-tree-") as directory:
        run_runtime(app, corpus, Path(directory), args.pairs, binaries)


if __name__ == "__main__":
    main()
