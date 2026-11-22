#!/usr/bin/env python3
"""Record and judge the evidence that a rustc PGO build really used its profile.

Usage: pgo-rustc-evidence.py EVIDENCE generate-flags|use-flags|warnings|driver|summary ...
Each check updates EVIDENCE (the JSON file of pgo-rustc-train.py) and exits 1 on a failed gate.
Run with -h after a command for its arguments.
"""

import argparse
import json
import re
import shutil
import subprocess
import sys
from pathlib import Path

COUNTERS_SECTION = "__llvm_prf_cnts"
ANSI = re.compile(r"\x1b\[[0-9;]*m")
MISMATCH = re.compile(r"hash mismatch\)\s+(\S+)", re.I)
MISSING = re.compile(r"no profile data available for function\s+(\S+)", re.I)
MALFORMED = re.compile(r"malformed instrumentation profile", re.I)


def load(path):
    path = Path(path)
    return json.loads(path.read_text()) if path.exists() else {}


def record(path, key, value, failures):
    data = load(path)
    value["failures"] = failures
    data[key] = value
    Path(path).write_text(json.dumps(data, indent=2, sort_keys=True) + "\n")
    for failure in failures:
        print(f"::error title=PGO {key}::{failure}", flush=True)
    detail = {name: item for name, item in value.items() if name != "failures"}
    print(f"{key}: {'FAILED' if failures else 'ok'} {json.dumps(detail)}", flush=True)
    return 1 if failures else 0


def read_lines(log):
    return ANSI.sub("", Path(log).read_text(errors="replace")).splitlines()


def flags(args):
    # Cargo logs each compiler command as "Running `...`" under -vv; other lines may quote old flags.
    lines = [line for line in read_lines(args.log) if "Running `" in line]
    generate = sum(1 for line in lines if "-Cprofile-generate=" in line)
    if args.command == "generate-flags":
        use = sum(1 for line in lines if "-Cprofile-use=" in line)
        failures = []
        if generate < args.min:
            failures.append(f"only {generate} compiler command(s) carry -Cprofile-generate, {args.min} required")
        if use:
            failures.append(f"{use} compiler command(s) of the instrumented build carry -Cprofile-use")
        return record(args.evidence, "instrumented_build", {"profile_generate_commands": generate,
                                                            "profile_use_commands": use}, failures)
    used = sum(1 for line in lines if f"-Cprofile-use={args.profile}" in line)
    other = sum(1 for line in lines if "-Cprofile-use=" in line) - used
    warn_flag = sum(1 for line in lines if "-pgo-warn-missing-function" in line)
    failures = []
    if used < args.min:
        failures.append(f"only {used} compiler command(s) carry -Cprofile-use={args.profile}, {args.min} required")
    if other:
        failures.append(f"{other} compiler command(s) carry a different -Cprofile-use profile")
    if generate:
        failures.append(f"{generate} compiler command(s) of the final build carry -Cprofile-generate")
    if not warn_flag:
        failures.append("no command carries -Cllvm-args=-pgo-warn-missing-function, so the log is not verbose and "
                        "the profile diagnostics below cannot be trusted")
    return record(args.evidence, "final_build", {"profile_use_commands": used, "other_profile_commands": other,
                                                 "profile_generate_commands": generate,
                                                 "warn_flag_commands": warn_flag}, failures)


def warnings(args):
    lines = read_lines(args.log)
    mismatch = {m.group(1) for line in lines for m in [MISMATCH.search(line)] if m}
    missing = {m.group(1) for line in lines for m in [MISSING.search(line)] if m}
    malformed = sum(1 for line in lines if MALFORMED.search(line))
    functions = load(args.evidence).get("train", {}).get("merged", {}).get("functions", 0)
    result = {"mismatch_functions": len(mismatch), "missing_functions": len(missing), "malformed_lines": malformed,
              "profile_functions": functions, "visible_scope": "in-tree compiler crates; Cargo hides registry-crate warnings"}
    failures = []
    if functions:
        result["mismatch_percent"] = round(100 * len(mismatch) / functions, 3)
        result["missing_percent"] = round(100 * len(missing) / functions, 3)
        if result["mismatch_percent"] > args.max_mismatch:
            failures.append(f"{result['mismatch_percent']}% of the profiled functions changed their control flow "
                            f"since instrumentation (limit {args.max_mismatch}%), so the profile does not fit this code")
        if result["missing_percent"] > args.max_missing:
            failures.append(f"{result['missing_percent']}% of the profiled functions have no profile data "
                            f"(limit {args.max_missing}%)")
    else:
        failures.append("the training evidence has no profile function count to compare with")
    if malformed:
        failures.append(f"{malformed} line(s) report a malformed instrumentation profile")
    return record(args.evidence, "profile_use_warnings", result, failures)


def counters_sections(path):
    tool = shutil.which("otool")
    command = [tool, "-l", path] if tool else ["readelf", "-SW", path]
    proc = subprocess.run(command, capture_output=True, text=True)
    if proc.returncode != 0:
        sys.exit(f"{command[0]} failed on {path}: {proc.stderr.strip()}")
    return proc.stdout.count(COUNTERS_SECTION)


def driver(args):
    found = counters_sections(args.file)
    failures = []
    if args.expect == "instrumented" and not found:
        failures.append(f"{args.file} has no {COUNTERS_SECTION} section, so it is not instrumented")
    if args.expect == "plain" and found:
        failures.append(f"{args.file} still carries {found} {COUNTERS_SECTION} reference(s), so it is instrumented")
    key = "instrumented_driver" if args.expect == "instrumented" else "final_driver"
    return record(args.evidence, key, {"file": Path(args.file).name, "counter_sections": found}, failures)


def summary(args):
    data = load(args.evidence)
    train = data.get("train", {})
    stored = train.get("source") == "stored"
    lines = [f"### {args.title}", ""]
    lines.append(f"Target `{train.get('host', 'unknown')}` on a `{train.get('machine', 'unknown')}` machine; "
                 f"profile tool {'; '.join(train.get('llvm_profdata', [])) or 'unknown'}.")
    merged = train.get("merged", {})
    if stored:
        parts = ", ".join(f"{name} {entry.get('functions', 0)} functions ({entry.get('bytes', 0) >> 20} MiB)"
                          for name, entry in sorted(train.get("components", {}).items()))
        lines.append(f"Stored profile bundle `{train.get('bundle')}`, trained on macOS {train.get('trained_on')} "
                     f"over {len(train.get('crates_complete', []))} crates ({', '.join(train.get('profiles', []))}, "
                     f"scenarios {train.get('scenarios')}), {train.get('raw_files', 0)} raw files: {parts}. "
                     f"{len(train.get('changed_patches', []))} fRust patch(es) changed since it was trained.")
    else:
        lines.append(f"Training: {len(train.get('crates_complete', []))} of {len(train.get('crates_requested', []))} "
                     f"crates complete in every profile ({', '.join(train.get('profiles', []))}, scenarios "
                     f"{train.get('scenarios')}), stopped by `{train.get('stopped_by') or 'completion'}`, "
                     f"{train.get('raw_files', 0)} raw files observed ({train.get('raw_bytes', 0) >> 20} MiB), "
                     f"{train.get('bad_raw_files', 0)} unusable, merged profile {merged.get('functions', 0)} "
                     f"functions at level {merged.get('level')}.")
    failed = [u for u in train.get("units", []) if not u["ok"]]
    for unit in failed:
        lines.append(f"- training unit {unit['crate']} {unit['profile']} failed: {unit['reason']}")
    if train.get("skipped_units"):
        lines.append(f"- {len(train['skipped_units'])} unit(s) not run ({train.get('stopped_by')}).")
    lines += ["", "| Check | Result | Detail |", "| --- | --- | --- |"]
    for key in ("instrumented_build", "instrumented_driver", "final_build", "profile_use_warnings", "final_driver"):
        entry = data.get(key)
        if entry is None:
            if not stored:
                lines.append(f"| {key} | not run | |")
            continue
        detail = {k: v for k, v in entry.items() if k != "failures"}
        lines.append(f"| {key} | {'FAILED' if entry['failures'] else 'ok'} | `{json.dumps(detail)}` |")
    for gate in train.get("gates_failed", []):
        lines.append(f"- training gate failed: {gate}")
    print("\n".join(lines))
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("evidence")
    sub = parser.add_subparsers(dest="command", required=True)
    gen = sub.add_parser("generate-flags")
    gen.add_argument("log")
    gen.add_argument("--min", type=int, default=50)
    use = sub.add_parser("use-flags")
    use.add_argument("log")
    use.add_argument("profile")
    use.add_argument("--min", type=int, default=50)
    warn = sub.add_parser("warnings")
    warn.add_argument("log")
    warn.add_argument("--max-mismatch", type=float, default=5.0)
    warn.add_argument("--max-missing", type=float, default=50.0)
    drv = sub.add_parser("driver")
    drv.add_argument("file")
    drv.add_argument("--expect", choices=("instrumented", "plain"), required=True)
    summ = sub.add_parser("summary")
    summ.add_argument("--title", default="Native rustc PGO candidate")
    args = parser.parse_args()
    handler = {"generate-flags": flags, "use-flags": flags, "warnings": warnings, "driver": driver,
               "summary": summary}[args.command]
    sys.exit(handler(args))


if __name__ == "__main__":
    main()
