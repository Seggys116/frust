#!/usr/bin/env python3
"""Compare real builds with process-tree CPU time on native Linux."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]
BUILD = ROOT / "target/compiler-rewrite"
CORPUS = BUILD / "corpus"
WORKLOADS = {
    "ripgrep": ("ripgrep", "rg", "crates/core/main.rs",
                "matches = stats.matches(),", "matches = stats.matches().saturating_add(1),",
                "e89fff89ac9af12e8d4ce9d5fd07beb408ca730f",
                "7a7d39cda8a03930e578f1dbb724e055771901842eca239e03b01e19da946a64"),
    "fd": ("fd-find", "fd", "src/main.rs",
           "let mut should_warn = pattern.contains('/');",
           "let mut should_warn = pattern.contains('/') || pattern.contains(':');",
           "b2a287b3993768e0ec52f0852916917f23fb2a38",
           "080f85e31279c422c5591ad7092ef93c7171711122c7de2b7aa32b84c5e72ed9"),
    "oxipng": ("oxipng", "oxipng", "src/cli.rs",
               '.version(env!("CARGO_PKG_VERSION"))',
               '.version(concat!(env!("CARGO_PKG_VERSION"), "-edited"))',
               "36f3ef8aac65ecf1761739ea2f2e530281f1312b",
               "22f3f5ee510e8af2e78194091197d1a223c171381aab9bb3479f32379033f6bb"),
}
PROFILES = ("dev", "release", "fat-default", "fat16-incremental", "fat1-incremental",
            "fat16-nonincremental", "dev-nonincremental", "off16-incremental",
            "off16-nonincremental", "thin16-incremental", "thin16-nonincremental")
FD_EDITS = ":@%;,="
FRUST_LABELS = ("frust", "compressed", "candidate")


def disk_usage(root):
    """Count allocated and logical bytes, charging hard-linked files once."""
    counters = {}
    seen = set()

    def walk(path):
        stat = path.lstat()
        inode = (stat.st_dev, stat.st_ino)
        if inode in seen:
            return 0, 0, 0
        seen.add(inode)
        logical, allocated, entries = stat.st_size, stat.st_blocks * 512, 1
        if not path.is_symlink() and path.is_dir():
            for child in path.iterdir():
                size, blocks, count = walk(child)
                logical += size
                allocated += blocks
                entries += count
        return logical, allocated, entries

    for profile in ("debug", "release"):
        profile_dir = root / profile
        if not profile_dir.is_dir():
            continue
        incremental = [
            path for path in (profile_dir / "incremental",
                              *(profile_dir / "build").glob("*/*/incremental"))
            if path.is_dir()
        ]
        if incremental:
            usage = [walk(path) for path in incremental]
            size, blocks, entries = map(sum, zip(*usage))
            counters[f"{profile}/incremental"] = dict(logical=size, allocated=blocks,
                                                       entries=entries)
        for name in ("deps", "build", ".fingerprint"):
            directory = profile_dir / name
            if directory.is_dir():
                size, blocks, entries = walk(directory)
                counters[f"{profile}/{name}"] = dict(logical=size, allocated=blocks, entries=entries)
    size, blocks, entries = walk(root) if root.is_dir() else (0, 0, 0)
    return {name: value + sum(part[name] for part in counters.values())
            for name, value in zip(("logical", "allocated", "entries"), (size, blocks, entries))} | {
                "parts": counters,
            }


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def snapshot(workload, source):
    repo = BUILD / "cohorts" / workload
    *_, commit, lock = WORKLOADS[workload]
    source.mkdir()
    with subprocess.Popen(["git", "-C", repo, "archive", commit], stdout=subprocess.PIPE) as archive:
        subprocess.run(["tar", "-x", "-C", source], stdin=archive.stdout, check=True)
        archive.stdout.close()
        if archive.wait() != 0:
            raise RuntimeError(f"cannot archive {workload} at {commit}")
    if digest(repo / "Cargo.lock") != lock:
        raise RuntimeError(f"lockfile digest mismatch for {workload}")
    shutil.copyfile(repo / "Cargo.lock", source / "Cargo.lock")


def timed(command, cwd, env, log, affinity):
    with log.open("wb") as stream:
        start = time.monotonic_ns()
        child = subprocess.Popen(["taskset", "-c", affinity, *command], cwd=cwd, env=env,
                                 stdout=stream, stderr=subprocess.STDOUT, start_new_session=True)
        # Linux includes usage from descendants reaped by the direct child.
        try:
            _, status, usage = os.wait4(child.pid, 0)
        except KeyboardInterrupt:
            os.killpg(child.pid, signal.SIGINT)
            _, status, _ = os.wait4(child.pid, 0)
            child.returncode = os.waitstatus_to_exitcode(status)
            raise
        child.returncode = os.waitstatus_to_exitcode(status)
        wall = (time.monotonic_ns() - start) / 1e9
    if child.returncode:
        raise RuntimeError(f"command exited {child.returncode}. See {log}")
    return dict(wall=wall, cpu=usage.ru_utime + usage.ru_stime, user=usage.ru_utime,
                system=usage.ru_stime, peak_rss_kib=usage.ru_maxrss)


def verify(workload, artifact, revision, work):
    if workload == "ripgrep":
        result = subprocess.run([artifact, "--no-config", "--stats", "rust"], input=b"rust\n",
                                capture_output=True, check=True)
        assert f"{revision + 1} matches\n".encode() in result.stdout, result.stdout
    elif workload == "fd":
        character = FD_EDITS[max(0, revision - 1)]
        result = subprocess.run([artifact, "--no-ignore", character, work / "fd-input"],
                                capture_output=True)
        if revision:
            assert result.returncode != 0 and b"path-separation character" in result.stderr
        else:
            assert result.returncode == 0 and b"with:colon" in result.stdout
    else:
        version = subprocess.check_output([artifact, "--version"], text=True)
        suffix = "-edited" if revision == 1 else f"-edited-{revision}"
        if revision:
            assert version.rstrip().endswith(suffix), version
        else:
            assert "-edited" not in version, version


def sample_env(label, bins, profile, target, jobs, debug_compression=None):
    env = {key: value for key, value in os.environ.items()
           if key in ("HOME", "USER", "LANG", "LC_ALL", "CARGO_HOME")}
    env.update(PATH=f"{bins}:/usr/bin:/bin", CARGO_TARGET_DIR=str(target),
               CARGO_BUILD_JOBS=str(jobs), FRUST_PROGRESS="plain", CARGO_TERM_COLOR="never")
    if label not in FRUST_LABELS:
        env["RUSTC"] = str(bins / "rustc")
    elif label == "compressed":
        env["RUSTC_BOOTSTRAP"] = "1"
        env["CARGO_ENCODED_RUSTFLAGS"] = "\x1f".join((
            f"-Zdebuginfo-compression={debug_compression}",
            f"-Clink-arg=-Wl,--compress-debug-sections={debug_compression}",
        ))
    if profile == "dev-nonincremental":
        env["CARGO_PROFILE_DEV_INCREMENTAL"] = "false"
    for lto in ("off", "thin", "fat"):
        if not profile.startswith(lto):
            continue
        env.update(CARGO_PROFILE_RELEASE_LTO=lto, CARGO_PROFILE_RELEASE_OPT_LEVEL="3",
                   CARGO_PROFILE_RELEASE_DEBUG="0", CARGO_PROFILE_RELEASE_STRIP="none")
        if profile != "fat-default":
            env["CARGO_PROFILE_RELEASE_CODEGEN_UNITS"] = "1" if profile == "fat1-incremental" else "16"
            env["CARGO_PROFILE_RELEASE_INCREMENTAL"] = str(profile.endswith("-incremental")).lower()
        break
    return env


def record_sample(output, result):
    with (output / "samples.jsonl").open("a") as samples:
        print(json.dumps(result), file=samples)
    print(f"{result['workload']} {result['profile']} {result['case']} "
          f"{result['toolchain']} CPU {result['cpu']:.3f}s wall {result['wall']:.3f}s", flush=True)


def prepare_fd_inputs(output, runtime):
    (output / "fd-input").mkdir()
    (output / "fd-input/with:colon").touch()
    if runtime:
        for index in range(32):
            search = output / "fd-runtime" / str(index)
            search.mkdir(parents=True)
            for file in range(400):
                (search / f"file-{file:04}.rs").touch()


def runtime_runs(args, output, directory, workload, profile, mode, round_number, order):
    _, binary, *_ = WORKLOADS[workload]
    expected = None
    env = {key: value for key, value in os.environ.items() if key in ("HOME", "LANG", "LC_ALL")}
    env["PATH"] = "/usr/bin:/bin"
    for index in range(args.runtime + 1):
        labels = order if index % 2 else order[::-1]
        for label in labels:
            executable = directory / label / mode / binary
            log = directory / f"{label}-runtime-{index}.log"
            png_dir = directory / "runtime-png"
            if workload == "ripgrep":
                command = [executable, "--no-config", "-j", "1", "-c", "-F",
                           "frust_needle_7", *[CORPUS / "search.txt"] * 16]
            elif workload == "fd":
                command = [executable, "--threads", "1", "--no-ignore", "--type", "f", "",
                           output / "fd-runtime"]
            else:
                png_dir.mkdir(exist_ok=True)
                command = [executable, "-o", "4", "--threads", "1", "--dir", png_dir,
                           *sorted((CORPUS / "pngs").glob("*.png"))]
            row = timed(command, output, env, log, args.cpus)
            if workload == "oxipng":
                products = sorted(png_dir.glob("*.png"))
                assert len(products) == 8
                signature = hashlib.sha256("".join(map(digest, products)).encode()).hexdigest()
                shutil.rmtree(png_dir)
            elif workload == "fd":
                signature = hashlib.sha256(b"\n".join(sorted(log.read_bytes().splitlines()))).hexdigest()
            else:
                signature = digest(log)
            if expected is None:
                expected = signature
            assert expected == signature, (workload, label, index, signature)
            if index:
                row.update(toolchain=label, workload=workload, profile=profile, case=f"runtime-{index}",
                           round=round_number + 1, output_sha256=signature,
                           log=str(log.relative_to(output)))
                record_sample(output, row)


def run_lane(args, output, workload, profile, bins_by_label, round_number):
    package, binary, path, old, new, *_ = WORKLOADS[workload]
    source = output / workload / "source"
    path = source / path
    old_text = path.read_text()
    assert old_text.count(old) == 1
    new_text = old_text.replace(old, new)
    directory = output / workload / profile / str(round_number)
    directory.mkdir(parents=True)
    labels = list(bins_by_label)
    # Rotate the first toolchain between rounds and reverse it for the edit.
    order = labels[round_number % len(labels):] + labels[:round_number % len(labels)]
    profile_flags = [] if profile.startswith("dev") else ["--release"]
    mode = "debug" if not profile_flags else "release"

    def sample(label, kind, revision, fresh=False, traced=False):
        bins = bins_by_label[label]
        target = directory / (f"{label}-fresh" if fresh else label)
        command = [bins / ("fcargo" if label in FRUST_LABELS else "cargo")]
        if traced:
            command.extend(["rustc", *profile_flags, "-p", package, "--offline", "--locked",
                            "-vv", "--", "-Ztime-passes", "-Ztime-llvm-passes", "-Csave-temps"])
        else:
            command.extend(["build", *profile_flags, "-p", package, "--offline", "--locked",
                            "--message-format=json"])
        env = sample_env(label, bins, profile, target, args.jobs, args.frust_debug_compression)
        if traced:
            env["RUSTC_BOOTSTRAP"] = "1"
        log = directory / f"{label}-{kind}.log"
        result = timed(command, source, env, log, args.cpus)
        artifact = target / mode / binary
        verify(workload, artifact, revision, output)
        if not traced:
            records = [json.loads(line) for line in log.read_text().splitlines()
                       if line.startswith('{')]
            compiled = sum(record.get("reason") == "compiler-artifact"
                           and record.get("fresh") is False for record in records)
            if not kind.startswith("noop"):
                assert compiled > 0, (label, kind, compiled)
            result["compiled"] = compiled

        result.update(toolchain=label, workload=workload, profile=profile, case=kind, revision=int(revision),
                      round=round_number + 1, artifact_bytes=artifact.stat().st_size,
                      artifact_sha256=digest(artifact), log=str(log.relative_to(output)))
        if kind in ("cold", "edit", "revert", "edit-fresh", "clean-rebuild") or kind.startswith("history-"):
            stripped = directory / f"{label}-{kind}.stripped"
            subprocess.run(["strip", "--strip-unneeded", "-o", stripped, artifact], check=True)
            result["stripped_bytes"] = stripped.stat().st_size
            stripped.unlink()
            usage = disk_usage(target)
            result["allocated_target_bytes"] = usage["allocated"]
            result["disk"] = usage
            if args.candidate:
                text = directory / f"{label}-{kind}.text"
                subprocess.run(["objcopy", "-O", "binary", "--only-section=.text", artifact, text],
                               check=True)
                if text.stat().st_size == 0:
                    raise RuntimeError(f"empty .text section in {artifact}")
                result["text_sha256"] = digest(text)
                text.unlink()
        record_sample(output, result)
        return artifact

    try:
        for label in order:
            sample(label, "cold", False)
        for i in range(args.noops):
            noop_order = order if i % 2 == 0 else order[::-1]
            for label in noop_order:
                sample(label, f"noop-{i + 1}", False)
        path.write_text(new_text)
        for label in reversed(order):
            sample(label, "edit", True)
        if args.fresh and profile == "fat16-incremental" and round_number == 0:
            for label in order:
                warm = directory / label / mode / binary
                fresh = sample(label, "edit-fresh", True, fresh=True)
                text = directory / f"{label}-parity.text"
                digests = []
                for artifact in (warm, fresh):
                    subprocess.run(["objcopy", "-O", "binary", "--only-section=.text",
                                    artifact, text], check=True)
                    if text.stat().st_size == 0:
                        raise RuntimeError(f"empty .text section in {artifact}")
                    digests.append(digest(text))
                text.unlink()
                with (output / "parity.jsonl").open("a") as results:
                    print(json.dumps(dict(workload=workload, toolchain=label, warm=digests[0],
                                         fresh=digests[1], identical=digests[0] == digests[1])),
                          file=results)
        path.write_text(old_text)
        for label in order:
            sample(label, "revert", False)
        for index in range(1, args.history_edits + 1):
            revision = 0 if index == args.history_edits else index
            edit = new
            if workload == "ripgrep":
                edit = new.replace("add(1)", f"add({revision})")
            elif workload == "fd" and revision:
                edit = new.replace("':'", repr(FD_EDITS[revision - 1]))
            elif workload == "oxipng" and revision > 1:
                edit = new.replace('-edited"', f'-edited-{revision}"')
            path.write_text(old_text.replace(old, edit) if revision else old_text)
            for label in order if index % 2 else reversed(order):
                sample(label, f"history-{index}", revision)
        runtime_profile = args.runtime_all or profile in ("release", "fat16-incremental")
        if args.runtime and runtime_profile and round_number == 0:
            runtime_runs(args, output, directory, workload, profile, mode, round_number, order)
        # Diagnostic passes run last because instrumentation changes CPU cost.
        if args.trace and profile == "fat16-incremental" and round_number == 0:
            for label in order:
                sample(label, "trace-seed", False, traced=True)
            path.write_text(new_text)
            for label in order:
                sample(label, "trace-edit", True, traced=True)
        if args.clean:
            path.write_text(old_text)
            for label in order:
                bins = bins_by_label[label]
                target = directory / label
                command = [bins / ("fcargo" if label in FRUST_LABELS else "cargo"), "clean", "--offline"]
                log = directory / f"{label}-clean.log"
                before = disk_usage(target)
                env = sample_env(label, bins, profile, target, args.jobs, args.frust_debug_compression)
                result = timed(command, source, env,
                               log, args.cpus)
                after = disk_usage(target)
                if (target / mode / binary).exists():
                    raise RuntimeError(f"cargo clean left {target / mode / binary}")
                record_sample(output, result | dict(
                    toolchain=label, workload=workload, profile=profile, case="clean",
                    round=round_number + 1, disk_before=before, disk=after,
                    log=str(log.relative_to(output)),
                ))
                sample(label, "clean-rebuild", False)
    finally:
        path.write_text(old_text)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("official", "control", "frust", "output"):
        parser.add_argument("--" + name, type=Path, required=True)
    parser.add_argument("--candidate", type=Path, help="alternate packaged fRust bin directory")
    parser.add_argument("--toolchains", nargs="+", choices=("official", "control", *FRUST_LABELS),
                        default=("official", "control", "frust"))
    parser.add_argument("--workloads", nargs="+", default=list(WORKLOADS), choices=WORKLOADS)
    parser.add_argument("--profiles", nargs="+", default=list(PROFILES), choices=PROFILES)
    parser.add_argument("--repeats", type=int, default=1)
    parser.add_argument("--noops", type=int, default=5)
    parser.add_argument("--jobs", type=int, default=6)
    parser.add_argument("--cpus", default="0-5")
    parser.add_argument("--runtime", type=int, default=0)
    parser.add_argument("--runtime-all", action="store_true")
    parser.add_argument("--history-edits", type=int, default=0)
    parser.add_argument("--clean", action="store_true")
    parser.add_argument("--frust-debug-compression", choices=("zlib", "zstd"),
                        help="benchmark compressed DWARF in the fRust dev lane")
    parser.add_argument("--fresh", action="store_true")
    parser.add_argument("--trace", action="store_true")
    args = parser.parse_args()
    if min(args.repeats, args.noops, args.jobs) < 1 or min(args.runtime, args.history_edits) < 0:
        parser.error("repeats, noops and jobs must be positive, runtime and history nonnegative")
    if args.history_edits == 1 or args.history_edits > len(FD_EDITS) + 1:
        parser.error("--history-edits must be zero or 2–7, including the final revert")
    if args.frust_debug_compression and any(not p.startswith("dev") for p in args.profiles):
        parser.error("--frust-debug-compression requires dev profiles")
    if ("compressed" in args.toolchains) != bool(args.frust_debug_compression):
        parser.error("select compressed with --frust-debug-compression")
    if ("candidate" in args.toolchains) != bool(args.candidate):
        parser.error("select candidate with --candidate")

    output = args.output.resolve()
    output.mkdir()
    bins = {name: getattr(args, "frust" if name == "compressed" else name).resolve()
            for name in args.toolchains}
    identities = {}
    for label, directory in bins.items():
        rustc = directory / ("frustc" if label in FRUST_LABELS else "rustc")
        identities[label] = {
            "rustc": subprocess.check_output([rustc, "-Vv"], text=True),
            "cargo": subprocess.check_output([directory / "cargo", "-Vv"], text=True),
            "rustc_sha256": digest(rustc),
            "cargo_sha256": digest(directory / "cargo"),
        }
    (output / "environment.json").write_text(json.dumps(dict(
        host=os.uname()._asdict() if hasattr(os.uname(), "_asdict") else list(os.uname()),
        cpus=args.cpus, jobs=args.jobs, toolchains=identities,
        frust_debug_compression=args.frust_debug_compression,
    ), indent=2) + "\n")
    if "fd" in args.workloads:
        prepare_fd_inputs(output, args.runtime)
    try:
        for workload in args.workloads:
            (output / workload).mkdir()
            snapshot(workload, output / workload / "source")
        for profile in args.profiles:
            for workload in args.workloads:
                for index in range(args.repeats):
                    run_lane(args, output, workload, profile, bins, index)
    finally:
        for workload in args.workloads:
            source = output / workload / "source"
            if source.is_dir():
                shutil.rmtree(source)


if __name__ == "__main__":
    main()
