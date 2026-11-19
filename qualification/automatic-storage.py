#!/usr/bin/env python3
"""Measure automatic target cleanup through the real fCargo entry point."""

import argparse
import json
import os
from pathlib import Path
import runpy
import shutil
import subprocess


MODES = ("noninteractive", "plain", "quiet", "json", "static", "static-disabled", "explicit-target")
PACKAGE = "frust_storage_probe"


def measure(args, mode, helpers):
    lane = args.output / mode
    source = lane / "source"
    (source / "src").mkdir(parents=True)
    config = lane / "config/frust"
    config.mkdir(parents=True)
    store = lane / "store"
    (config / "config.json").write_text(json.dumps(dict(
        version=1, target_root=str(store), cache_root=str(store),
    )))
    features = "".join(f"variant{i} = []\n" for i in range(args.generations))
    (source / "Cargo.toml").write_text(
        f'[package]\nname = "{PACKAGE}"\nversion = "0.0.0"\nedition = "2024"\n[features]\n'
        + features
    )
    (source / "src/main.rs").write_text(
        "fn main() {\n" + "".join(
            f'    if cfg!(feature = "variant{i}") {{ println!("{i}"); }}\n'
            for i in range(args.generations)
        ) + "}\n"
    )
    env = {key: val for key, val in os.environ.items()
           if key in ("HOME", "CARGO_HOME", "USER", "LANG", "LC_ALL")}
    env.update(PATH=f"{args.cargo.parent}:/usr/bin:/bin", TERM="xterm",
               XDG_CONFIG_HOME=str(lane / "config"), FRUST_CARGO=str(args.cargo),
               CARGO_BUILD_JOBS="1", CARGO_TERM_COLOR="never")
    if mode == "plain":
        env["FRUST_PROGRESS"] = "plain"
    elif mode.startswith("static"):
        env["FRUST_PROGRESS"] = "static"
    if mode == "static-disabled":
        env["FRUST_GC"] = "0"
    if mode == "explicit-target":
        env["CARGO_TARGET_DIR"] = str(lane / "explicit-target")
    extra = {"quiet": ["--quiet"], "json": ["--message-format=json"]}.get(mode, [])
    try:
        for generation in range(args.generations):
            command = [args.fcargo, "build", "--offline", "--features",
                       f"variant{generation}", *extra]
            log = lane / f"{generation}.log"
            timing = helpers["timed"](command, source, env, log, args.cpus)
            profiles = ([lane / "explicit-target/debug"] if mode == "explicit-target"
                        else list((store / "targets").glob("*/*/debug")))
            if len(profiles) != 1:
                raise RuntimeError(f"expected one central debug profile in {store}")
            profile, = profiles
            out = subprocess.check_output([profile / PACKAGE], text=True).strip()
            if out != str(generation):
                raise RuntimeError(f"{mode} ran revision {out} instead of {generation}")
            state = profile / ".frust-live.json"
            live_generation = json.loads(state.read_text())["generation"] if state.exists() else 0
            outputs = list((profile / "deps").glob(f"{PACKAGE}-*"))
            outputs.extend((profile / "build" / PACKAGE).glob(f"*/out/{PACKAGE}"))
            units = [path for path in outputs if path.is_file() and os.access(path, os.X_OK)]
            print(json.dumps(dict(mode=mode, revision=generation,
                                  live_generation=live_generation, executables=len(units),
                                  disk=helpers["disk_usage"](profile.parent), **timing)), flush=True)
        command = [args.fcargo, "build", "--offline", "--features",
                   f"variant{args.generations - 1}", "--message-format=json"]
        log = lane / "noop.log"
        timing = helpers["timed"](command, source, env, log, args.cpus)
        artifacts = [json.loads(line) for line in log.read_text().splitlines() if line.startswith("{")]
        matching = [entry for entry in artifacts if entry.get("reason") == "compiler-artifact"]
        if not matching or not all(entry.get("fresh") for entry in matching):
            raise RuntimeError(f"{mode} did not keep the active build fresh")
        print(json.dumps(dict(mode=mode, revision="noop", **timing)), flush=True)
    finally:
        shutil.rmtree(source)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fcargo", type=Path, required=True)
    parser.add_argument("--cargo", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--cpus", required=True)
    parser.add_argument("--generations", type=int, default=6)
    if (args := parser.parse_args()).generations < 4:
        parser.error("at least four generations are needed to observe eviction")
    for key in ("fcargo", "cargo", "output"):
        setattr(args, key, getattr(args, key).resolve())
    args.output.mkdir()
    helpers = runpy.run_path(Path(__file__).with_name("cpu-benchmark.py"))
    for mode in MODES:
        measure(args, mode, helpers)


if __name__ == "__main__":
    main()
