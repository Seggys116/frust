#!/usr/bin/env python3
"""Check shared compilation and independent progress with overlapping Cargo checks.

Every case runs on a plain path-crate workspace and on a workspace whose shared unit is a host proc
macro that Cargo builds in full for a check. The proc macro cases hold the macro compilation in the
wrapper and prove that peers share it, that a restored-mtime edit and a killed producer are rebuilt, and
that a warm check recompiles nothing. A last case adds a build script, which keeps ordinary scheduling:
the second request must not start any unit before the first one finishes.
"""

import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import time


PROC_MACRO_SOURCE = """extern crate proc_macro;
use proc_macro::TokenStream;

#[proc_macro]
pub fn answer(_input: TokenStream) -> TokenStream {
    "42u32".parse().unwrap()
}
"""


def create_workspace(root, proc_macro=False, build_script=False):
    # The held unit is the plain library "shared" or, for the proc macro cases, the macro crate.
    held = "macros" if proc_macro else "shared"
    members = (held, "left", "right", "independent")
    (root / "Cargo.toml").write_text(
        '[workspace]\nresolver = "2"\nmembers = [' +
        ", ".join(f'"{member}"' for member in members) + "]\n"
    )
    for member in members:
        source = root / member / "src"
        source.mkdir(parents=True)
        manifest = f'[package]\nname = "{member}"\nversion = "0.1.0"\nedition = "2021"\n'
        if member in ("left", "right"):
            manifest += f'\n[dependencies]\n{held} = {{ path = "../{held}" }}\n'
            expression = f"{held}::answer!()" if proc_macro else "shared::value()"
        else:
            expression = "42"
        if proc_macro and member == held:
            manifest += "\n[lib]\nproc-macro = true\n"
            (source / "lib.rs").write_text(PROC_MACRO_SOURCE)
            if build_script:
                (source.parent / "build.rs").write_text("fn main() {}\n")
        else:
            (source / "lib.rs").write_text(f"pub fn value() -> u32 {{ {expression} }}\n")
        (source.parent / "Cargo.toml").write_text(manifest)


def create_wrapper(root):
    wrapper = root / "record-rustc"
    wrapper.write_text("""#!/bin/sh
set -eu
previous=
for arg do
    if [ "$previous" = --crate-name ] && [ "$arg" != ___ ]; then
        : > "$PROBE_DIR/$PROBE_REQUEST.$arg.$$"
        if [ "$PROBE_CASE" = conflict ] || [ "$PROBE_REQUEST:$arg" = "first:$PROBE_HELD" ]; then
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


def check_pair(root, args, cargo_options, second_packages, scenario=None,
               proc_macro=False, build_script=False):
    # The scenario names the behaviour under test. It defaults to the directory name, and the
    # proc macro cases reuse a scenario in a directory of their own.
    scenario = scenario or root.name
    held = "macros" if proc_macro else "shared"
    root.mkdir()
    create_workspace(root, proc_macro, build_script)
    first_packages = ("left",)
    if scenario == "identical":
        with (root / "left" / "src" / "lib.rs").open("a") as source:
            source.write("pub fn warning_fixture() { let frust_replay_probe = 0; }\n")
    if scenario == "conflict":
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
        PROBE_CASE=scenario,
        PROBE_DIR=str(root),
        PROBE_HELD=held,
    )
    processes = {}

    def start(request, packages):
        # Each request must leave a unit available for the other to lock.
        jobs = 1 if scenario == "conflict" else args.jobs
        command = [str(args.cargo), *cargo_options, "check", "--offline", "-j", str(jobs)]
        command.extend(["--target-dir", str(root / "target")])
        if scenario == "conflict":
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
        marker = "compiled" if scenario == "edit" else f"first.{held}.*"
        if scenario == "conflict":
            marker = "first.*.*"
        if not wait_for_marker(root, marker, first, args.timeout):
            raise RuntimeError("first request did not reach the shared compilation")
        if scenario == "edit":
            source = root / held / "src" / "lib.rs"
            times = source.stat()
            # Atomically replace the compiled bytes while preserving size and mtime.
            replacement = source.with_suffix(".new")
            replacement.write_text(source.read_text().replace("42", "43"))
            os.utime(replacement, ns=(times.st_atime_ns, times.st_mtime_ns))
            replacement.replace(source)
        second = start("second", second_packages)
        if scenario == "conflict":
            overlapped = wait_for_marker(root, "second.*.*", second, args.timeout)
        elif scenario == "fallback":
            # A build script keeps ordinary scheduling, so the second request must wait for the first.
            overlapped = wait_for_marker(root, "second.independent.*", second, 1.0)
        elif "independent" in second_packages:
            overlapped = wait_for_marker(root, "second.independent.*", second, args.timeout)
        else:
            # Leave the first request in the compiler while the follower starts.
            time.sleep(0.25)
            overlapped = True
        if scenario == "interrupted":
            stop(first)
        else:
            (root / "release").touch()
        for request, process in processes.items():
            if request == "first" and scenario == "interrupted":
                continue
            if process.wait(timeout=args.timeout):
                raise RuntimeError(f"{request} Cargo check failed")
        if scenario == "conflict":
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
        if scenario == "identical":
            if start("warm", ("left",)).wait(timeout=args.timeout):
                raise RuntimeError("warm Cargo check failed")
            left_count = sum(1 for _ in root.glob("*.left.*"))
            warm_is_fresh = left_count == 1 and not any(root.glob("warm.*.*"))
            for request in ("first", "second", "warm"):
                messages = (root / f"{request}.log").read_text()
                if "unused variable: `frust_replay_probe`" not in messages:
                    raise RuntimeError(f"{request} did not report the cached warning")
            print(f"{root.name}: downstream compiler invocations {left_count}", flush=True)
        count = sum(1 for _ in root.glob(f"*.{held}.*"))
        print(f"{root.name}: shared compiler invocations {count}", flush=True)
        if "independent" in second_packages:
            print(f"{root.name}: independent compilation started before release {overlapped}",
                  flush=True)
        expected = 2 if scenario in ("edit", "interrupted") else 1
        # Only the fallback case must not overlap: cooperative scheduling is what lets a peer proceed.
        return count == expected and overlapped == (scenario != "fallback") and warm_is_fresh
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
            check_pair(root / "proc-macro-identical", args, options, ("left",),
                       scenario="identical", proc_macro=True),
            check_pair(root / "proc-macro-overlap", args, options, ("right", "independent"),
                       scenario="overlap", proc_macro=True),
            check_pair(root / "proc-macro-fallback", args, options, ("right", "independent"),
                       scenario="fallback", proc_macro=True, build_script=True),
        ]
        if args.recovery:
            results.extend([
                check_pair(root / "interrupted", args, options, ("right", "independent")),
                check_pair(root / "edit", args, options, ("right", "independent")),
                check_pair(root / "conflict", args, options, ("shared", "independent")),
                check_pair(root / "proc-macro-interrupted", args, options, ("right", "independent"),
                           scenario="interrupted", proc_macro=True),
                check_pair(root / "proc-macro-edit", args, options, ("right", "independent"),
                           scenario="edit", proc_macro=True),
            ])
    return 0 if all(results) else 1


if __name__ == "__main__":
    raise SystemExit(main())
