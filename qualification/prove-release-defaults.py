#!/usr/bin/env python3
"""Check automatic native fat-LTO settings through packaged fCargo."""

import argparse
import json
import os
from pathlib import Path
import re
import shutil
import subprocess


def build(work, case, command, profile, expected, env, cargo_args=(),
          profile_name="release", compiler_args=()):
    manifest = work / "source/Cargo.toml"
    manifest.write_text("[package]\nname = \"frust_parity\"\nversion = \"0.1.0\"\n"
                        "edition = \"2021\"\n\n" + profile)
    target = work / case
    subcommand = "rustc" if compiler_args else "build"
    args = [command, subcommand, "--locked", "--offline", "--profile", profile_name, "-vv",
            *cargo_args]
    if compiler_args:
        args.extend(("--", *compiler_args))
    result = subprocess.run(args, cwd=manifest.parent, text=True, capture_output=True,
                            env=env | {"CARGO_TARGET_DIR": str(target)})
    log = result.stdout + result.stderr
    (work / f"{case}.log").write_text(log)
    if result.returncode != 0:
        raise SystemExit(f"{case} failed. See {work / (case + '.log')}")
    invocation, = [line for line in log.splitlines()
                   if "Running `" in line and "--crate-name frust_parity " in line]
    units = re.findall(r"-C codegen-units=(\d+)", invocation)
    incremental = "-C incremental=" in invocation
    lto = re.findall(r"-C lto(?:=([a-z]+))?(?=\s|$)", invocation)
    actual = (units[-1] if units else None, incremental, (lto[-1] or "fat") if lto else None)
    if actual != expected:
        raise SystemExit(f"{case}: expected {expected}, got {actual}")
    print(f"PASS {case} {actual}", flush=True)
    return target / profile_name / "frust_parity"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("toolchain_prefix", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()

    bins = args.toolchain_prefix.resolve() / "bin"
    work = args.output.resolve()
    work.mkdir()
    fixture = Path(__file__).resolve().parent / "parity/fixture"
    shutil.copytree(fixture, work / "source")

    env = {key: value for key, value in os.environ.items()
           if key in ("HOME", "USER", "LANG", "LC_ALL")}
    env.update(PATH=f"{bins}:/usr/bin:/bin", CARGO_HOME=str(work / "cargo-home"),
               FRUST_PROGRESS="plain", RUSTC_BOOTSTRAP="1")
    fcargo = bins / "fcargo"
    fat = '[profile.release]\nlto = "fat"\n'
    defaults = ("16", True, "fat")
    stock = (None, False, "fat")

    first = build(work, "automatic", fcargo, fat, defaults, env, compiler_args=("-Ztime-passes",))
    assert subprocess.check_output([first], text=True).strip() == "21 2 4 22 44 3"
    script_invocation, = [line for line in (work / "automatic.log").read_text().splitlines()
                          if "Running `" in line and "--crate-name build_script_build " in line]
    assert "-C incremental=" not in script_invocation
    assert "-C codegen-units=16" not in script_invocation
    print("PASS host-build-script", flush=True)
    source = work / "source/src/main.rs"
    old = source.read_text()
    assert old.count("n.wrapping_add(11)") == 1
    source.write_text(old.replace("n.wrapping_add(11)", "n.wrapping_add(12)"))
    edited = build(work, "automatic", fcargo, fat, defaults, env, compiler_args=("-Ztime-passes",))
    fresh = build(work, "fresh", fcargo, fat, defaults, env, compiler_args=("-Ztime-passes",))
    for artifact in (edited, fresh):
        assert subprocess.check_output([artifact], text=True).strip() == "22 2 4 22 44 3"
        subprocess.run(["objcopy", "-O", "binary", "--only-section=.text",
                        artifact, artifact.with_suffix(".text")], check=True)
    assert edited.with_suffix(".text").read_bytes() == fresh.with_suffix(".text").read_bytes()
    for case in ("automatic", "fresh"):
        log = (work / f"{case}.log").read_text()
        assert any(float(seconds) > 0 for seconds in re.findall(
            r"time:\s+(\d+(?:\.\d+)?);[^\n]*\bLLVM_fatlto\b", log,
        ))

    build(work, "boolean-fat", fcargo, "[profile.release]\nlto = true\n", defaults, env)
    build(work, "thin", fcargo, '[profile.release]\nlto = "thin"\n', (None, False, "thin"), env)
    build(work, "size", fcargo, fat + 'opt-level = "z"\n', stock, env)
    build(work, "no-fat", fcargo, "", (None, False, None), env)
    build(work, "manifest-opt-out", fcargo, fat + "incremental = false\n", stock, env)
    build(work, "ci-default", fcargo, fat, stock, env | {"CI": "true"})
    build(work, "global-opt-out", fcargo, fat, stock, env | {"CARGO_INCREMENTAL": "0"})
    build(work, "global-opt-in", fcargo, fat + "incremental = false\n",
          defaults, env | {"CARGO_INCREMENTAL": "1"})
    build(work, "explicit-units", fcargo, fat + "codegen-units = 1\n",
          ("1", False, "fat"), env)
    build(work, "explicit-incremental-and-units", fcargo,
          fat + "codegen-units = 3\nincremental = true\n", ("3", True, "fat"), env)
    build(work, "environment-units", fcargo, fat,
          ("7", False, "fat"), env | {"CARGO_PROFILE_RELEASE_CODEGEN_UNITS": "7"})
    build(work, "package-override", fcargo,
          fat + "[profile.release.package.frust_parity]\nincremental = false\n",
          stock, env)
    build(work, "cargo-config-opt-out", fcargo, fat, stock, env,
          cargo_args=("--config", "build.incremental=false"))
    build(work, "inherited-profile", fcargo, fat + '[profile.custom]\ninherits = "release"\n',
          defaults, env, profile_name="custom")
    build(work, "plain-cargo", bins / "cargo", fat, stock, env | {"RUSTC": str(bins / "frustc")})
    build(work, "explicit-rustc", fcargo, fat, stock, env | {"RUSTC": str(bins / "rustc")})
    build(work, "explicit-native-target", fcargo, fat, defaults, env,
          cargo_args=("--target", "x86_64-unknown-linux-gnu"))
    cross_env = env | {"__CARGO_TEST_CHANNEL_OVERRIDE_DO_NOT_USE_THIS": "nightly",
                       "CARGO_TARGET_DIR": str(work / "cross")}
    graph = subprocess.check_output([
        fcargo, "build", "--locked", "--offline", "--release", "-Z", "unstable-options",
        "--unit-graph", "--target", "wasm32-unknown-unknown",
    ], cwd=work / "source", env=cross_env, text=True)
    (work / "cross-graph.json").write_text(graph)
    units = json.loads(graph)
    root, = [units["units"][i] for i in units["roots"]]
    assert root["platform"] == "wasm32-unknown-unknown"
    assert root["profile"]["incremental"] is False
    assert root["profile"].get("codegen_units") is None
    print("PASS cross-target-profile", flush=True)
    print("RELEASE_DEFAULTS_OK", flush=True)


if __name__ == "__main__":
    main()
