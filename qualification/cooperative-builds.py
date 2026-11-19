#!/usr/bin/env python3
"""Check shared compilation and independent progress with overlapping Cargo checks."""

import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import time


def create_workspace(root):
    members = ("shared", "left", "right", "independent")
    (root / "Cargo.toml").write_text(
        '[workspace]\nresolver = "2"\nmembers = [' +
        ", ".join(f'"{member}"' for member in members) + "]\n"
    )
    for member in members:
        source = root / member / "src"
        source.mkdir(parents=True)
        manifest = f'[package]\nname = "{member}"\nversion = "0.1.0"\nedition = "2021"\n'
        if member in ("left", "right"):
            manifest += '\n[dependencies]\nshared = { path = "../shared" }\n'
            expression = "shared::value()"
        else:
            expression = "42"
        (source.parent / "Cargo.toml").write_text(manifest)
        (source / "lib.rs").write_text(f"pub fn value() -> u32 {{ {expression} }}\n")


def create_wrapper(root):
    wrapper = root / "record-rustc"
    wrapper.write_text("""#!/bin/sh
set -eu
previous=
for arg do
    if [ "$previous" = --crate-name ] && [ "$arg" != ___ ]; then
        : > "$PROBE_DIR/$PROBE_REQUEST.$arg.$$"
        if [ "$PROBE_CASE" = conflict ] || [ "$PROBE_REQUEST:$arg" = first:shared ]; then
            if [ "$PROBE_CASE" = edit ]; then
                "$@"
                : > "$PROBE_DIR/compiled"
            fi
            while [ ! -e "$PROBE_DIR/release" ]; do sleep 0.02; done
            if [ "$PROBE_CASE" = edit ]; then exit 0; fi
        fi
        break
    fi
    previous=$arg
done
exec "$@"
""")
    wrapper.chmod(0o700)
    return wrapper


def wait_for_marker(root, pattern, process, timeout):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline and process.poll() is None:
        if next(root.glob(pattern), None) is not None:
            return True
        time.sleep(0.02)
    return next(root.glob(pattern), None) is not None


def stop(process):
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    process.wait()


def check_pair(root, args, cargo_options, second_packages):
    root.mkdir()
    create_workspace(root)
    first_packages = ("left",)
    if root.name == "conflict":
        first_packages = ("shared", "independent")
        for member in first_packages:
            with (root / member / "src" / "lib.rs").open("a") as source:
                source.write('pub const CONFIG: &str = env!("PROBE_REQUEST");\n')
    cargo_home = root / "cargo-home"
    cargo_home.mkdir()
    compiler_env = {
        key: value for key, value in os.environ.items()
        if not key.startswith(("CARGO_", "RUSTC_", "RUSTFLAGS", "RUSTUP_TOOLCHAIN"))
    }
    compiler_env.update(
        CARGO_HOME=str(cargo_home),
        CARGO_BUILD_BUILD_DIR=str(root / "build"),
        CARGO_TERM_COLOR="never",
        RUSTC=str(args.rustc),
        RUSTC_WRAPPER=str(create_wrapper(root)),
        FRUST_PROGRESS="plain",
        PROBE_CASE=root.name,
        PROBE_DIR=str(root),
    )
    processes = {}

    def start(request, packages):
        command = [str(args.cargo), *cargo_options, "check", "--offline", "-j", str(args.jobs)]
        command.extend(["--target-dir", str(root / "target")])
        if root.name == "conflict":
            command.append("--message-format=json")
        for package in packages:
            command.extend(["-p", package])
        env = dict(compiler_env, PROBE_REQUEST=request)
        with (root / f"{request}.log").open("wb") as log:
            process = subprocess.Popen(command, cwd=root, env=env, stdout=log,
                                       stderr=subprocess.STDOUT, start_new_session=True)
        processes[request] = process
        return process

    try:
        first = start("first", first_packages)
        marker = "compiled" if root.name == "edit" else "first.shared.*"
        if root.name == "conflict":
            marker = "first.*.*"
        if not wait_for_marker(root, marker, first, args.timeout):
            raise RuntimeError("first request did not reach the shared compilation")
        if root.name == "edit":
            source = root / "shared" / "src" / "lib.rs"
            times = source.stat()
            # Atomically replace the compiled bytes while preserving size and mtime.
            replacement = source.with_suffix(".new")
            replacement.write_text(source.read_text().replace("42", "43"))
            os.utime(replacement, ns=(times.st_atime_ns, times.st_mtime_ns))
            replacement.replace(source)
        second = start("second", second_packages)
        if root.name == "conflict":
            overlapped = wait_for_marker(root, "second.*.*", second, args.timeout)
        elif "independent" in second_packages:
            overlapped = wait_for_marker(root, "second.independent.*", second, args.timeout)
        else:
            # Leave the first request in the compiler while the follower starts.
            time.sleep(0.25)
            overlapped = True
        if root.name == "interrupted":
            stop(first)
        else:
            (root / "release").touch()
        for request, process in processes.items():
            if request == "first" and root.name == "interrupted":
                continue
            if process.wait(timeout=args.timeout):
                raise RuntimeError(f"{request} Cargo check failed")
        if root.name == "conflict":
            for request in processes:
                messages = (root / f"{request}.log").read_text().splitlines()
                events = [json.loads(line) for line in messages if line.startswith("{")]
                if [event["success"] for event in events if event["reason"] == "build-finished"] != [True]:
                    raise RuntimeError(f"{request} did not emit one successful build-finished event")
            print(f"conflict: both checks finished, overlapping first units {overlapped}", flush=True)
            return overlapped and all(
                any(root.glob(f"{request}.{member}.*"))
                for request in processes for member in first_packages
            )
        warm_is_fresh = True
        if root.name == "identical":
            if start("warm", ("left",)).wait(timeout=args.timeout):
                raise RuntimeError("warm Cargo check failed")
            left_count = sum(1 for _ in root.glob("*.left.*"))
            warm_is_fresh = left_count == 1 and not any(root.glob("warm.*.*"))
            print(f"identical: downstream compiler invocations {left_count}", flush=True)
        count = sum(1 for _ in root.glob("*.shared.*"))
        print(f"{root.name}: shared compiler invocations {count}", flush=True)
        if "independent" in second_packages:
            print(f"{root.name}: independent compilation started before release {overlapped}",
                  flush=True)
        expected = 2 if root.name in ("edit", "interrupted") else 1
        return count == expected and overlapped and warm_is_fresh
    except (RuntimeError, subprocess.TimeoutExpired):
        for request in processes:
            log = (root / f"{request}.log").read_text(errors="replace")
            print(f"{request} Cargo output\n{log[-4000:]}", flush=True)
        raise
    finally:
        for process in processes.values():
            if process.poll() is None:
                stop(process)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cargo", type=Path, required=True)
    parser.add_argument("--rustc", type=Path, required=True)
    parser.add_argument("--timeout", type=float, default=5)
    parser.add_argument("--jobs", type=int, default=2,
                        metavar="N", help="Cargo job limit per request")
    parser.add_argument("--recovery", action="store_true",
                        help="also check interruption, replacement with restored mtime, and conflicts")
    parser.add_argument("cargo_options", nargs=argparse.REMAINDER,
                        help="global Cargo options after --")
    args = parser.parse_args()
    if args.jobs < 1 or args.timeout <= 0:
        parser.error("--jobs and --timeout must be positive")
    args.cargo = args.cargo.absolute()
    args.rustc = args.rustc.absolute()
    options = args.cargo_options
    if options[:1] == ["--"]:
        options = options[1:]
    build_root = Path(__file__).resolve().parents[1] / "target" / "compiler-rewrite"
    build_root.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="cooperative-", dir=build_root) as work:
        root = Path(work)
        results = [
            check_pair(root / "identical", args, options, ("left",)),
            check_pair(root / "overlap", args, options, ("right", "independent")),
        ]
        if args.recovery:
            results.extend([
                check_pair(root / "interrupted", args, options, ("right", "independent")),
                check_pair(root / "edit", args, options, ("right", "independent")),
                check_pair(root / "conflict", args, options, ("shared", "independent")),
            ])
    return 0 if all(results) else 1


if __name__ == "__main__":
    raise SystemExit(main())
