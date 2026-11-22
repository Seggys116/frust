#!/usr/bin/env python3
"""Export, check and describe the stored rustc PGO profile bundles of the macOS toolchains.

A bundle is what `build-toolchain-pgo.sh generate` leaves after native training, and what
`build-toolchain-pgo.sh stored` consumes without training again. It is a directory (a tar.zst build
asset, docs/build-assets.md) that holds one merged profile per component (`rustc-pgo.profdata`, and
`cargo-pgo.profdata` when Cargo was trained too), a `SHA256SUMS` of them and a `MANIFEST` that records
the compiler sources, the target, the settings and the tools the profile was collected with.

A profile only fits the code it was collected from, so `check` refuses a bundle that was made for
another Rust commit, another target or machine, another LLVM, or other compiler build settings. A
changed fRust patch is only reported: it touches a few functions, and the build that uses the profile
measures how many functions no longer match and refuses the result when too many do not.

Usage:
  pgo-rustc-bundle.py settings ROOT HOST
      Print the sha-256 of the build settings that shape the instrumented compiler code.
  pgo-rustc-bundle.py export ROOT OUT --host H --rust-commit C --llvm-version V --llvm-profdata TOOL
        --evidence E --component rustc=FILE [--component cargo=FILE] [--rustc-perf COMMIT]
      Write the bundle files into the new directory OUT.
  pgo-rustc-bundle.py check ROOT BUNDLE --host H --rust-commit C --llvm-version V --llvm-profdata TOOL
        --evidence E
      Refuse the bundle unless it fits this build, and record the profile in the evidence file so that
      the profile-use checks of pgo-rustc-evidence.py can judge the build that used it.
"""

import argparse
import hashlib
import json
import os
import platform
import re
import subprocess
import sys
from pathlib import Path

BUNDLE_VERSION = "1"
MIN_FUNCTIONS = 20000
COMPONENTS = ("rustc", "cargo")
DEPLOYMENT = {"aarch64-apple-darwin": "11.0", "x86_64-apple-darwin": "10.12"}
MACHINE = {"aarch64-apple-darwin": "arm64", "x86_64-apple-darwin": "x86_64"}
SETTINGS_FILES = ("support/bootstrap.toml", "support/bootstrap-common.toml", "support/bootstrap-macos.toml")
PATCH_GLOBS = ("support/patches/*.patch", "support/patches/rustc/*.patch")
SKIPPED_KEYS = ("jobs",)


def sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def settings_hash(root, host):
    """Hash of the setting lines of the recipe files; comments, blanks and the job count do not count."""
    if host not in DEPLOYMENT:
        sys.exit(f"no macOS recipe for {host}")
    lines = [f"host {host}", f"deployment-target {DEPLOYMENT[host]}"]
    for name in SETTINGS_FILES:
        for line in (Path(root) / name).read_text().splitlines():
            line = line.strip()
            if not line or line.startswith("#") or re.match(rf"^({'|'.join(SKIPPED_KEYS)})\s*=", line):
                continue
            lines.append(line)
    return hashlib.sha256(("\n".join(lines) + "\n").encode()).hexdigest()


def patch_hashes(root):
    root = Path(root)
    found = sorted({path for pattern in PATCH_GLOBS for path in root.glob(pattern)})
    return [(sha256(path), path.relative_to(root).as_posix()) for path in found]


def profile_summary(tool, profile):
    proc = subprocess.run([tool, "show", str(profile)], capture_output=True, text=True)
    if proc.returncode != 0:
        return None, proc.stderr.strip()[-400:]

    def field(pattern):
        match = re.search(pattern, proc.stdout)
        return match.group(1) if match else None

    functions, top = field(r"Total functions:\s*(\d+)"), field(r"Maximum function count:\s*(\d+)")
    return {"level": field(r"Instrumentation level:\s*(\w+)"), "functions": int(functions) if functions else 0,
            "max_function_count": int(top) if top else 0}, ""


def tool_version(tool):
    proc = subprocess.run([tool, "--version"], capture_output=True, text=True)
    return [line.strip() for line in proc.stdout.strip().splitlines()[:2]]


def machine_name():
    machine = platform.machine().lower()
    return {"aarch64": "arm64", "amd64": "x86_64"}.get(machine, machine)


def macos_version():
    return platform.mac_ver()[0] or "unknown"


def load(path):
    path = Path(path)
    return json.loads(path.read_text()) if path.exists() else {}


def store(path, key, value):
    data = load(path)
    data[key] = value
    Path(path).write_text(json.dumps(data, indent=2, sort_keys=True) + "\n")


def parse_manifest(path):
    """Return (single-value keys, profile lines, patch lines) of a bundle MANIFEST."""
    values, profiles, patches = {}, [], {}
    in_patches = False
    for line in Path(path).read_text().splitlines():
        if not line.strip():
            continue
        if line.startswith("  "):
            if in_patches:
                digest, _, name = line.strip().partition("  ")
                patches[name] = digest
            continue
        in_patches = False
        key, _, rest = line.partition(" ")
        if key == "profile":
            profiles.append(rest.split())
        elif key == "patches-sha256":
            in_patches = True
        else:
            values[key] = rest
    return values, profiles, patches


def export(args):
    out = Path(args.out)
    if out.exists():
        sys.exit(f"bundle directory exists: {out}")
    components = {}
    for item in args.component:
        name, _, path = item.partition("=")
        if name not in COMPONENTS or not path or name in components:
            sys.exit(f"bad --component {item!r}, expected rustc=FILE or cargo=FILE, once each")
        components[name] = Path(path)
    if "rustc" not in components:
        sys.exit("a bundle needs a rustc profile")
    train = load(args.evidence).get("train", {})
    if train.get("host") != args.host:
        sys.exit(f"the training evidence is for {train.get('host')}, not {args.host}")
    if train.get("gates_failed"):
        sys.exit("the training evidence has failed gates, so its profile is not exported")
    out.mkdir(parents=True)
    sums, profile_lines = [], []
    for name in COMPONENTS:
        if name not in components:
            continue
        source = components[name]
        summary, error = profile_summary(args.llvm_profdata, source)
        if summary is None or summary["level"] != "IR" or summary["functions"] < MIN_FUNCTIONS:
            sys.exit(f"the {name} profile {source} is not a usable IR profile: {summary or error}")
        target = out / f"{name}-pgo.profdata"
        target.write_bytes(source.read_bytes())
        digest = sha256(target)
        sums.append(f"{digest}  {target.name}\n")
        profile_lines.append(f"profile {name} {target.name} {digest} {target.stat().st_size} {summary['functions']}")
    (out / "SHA256SUMS").write_text("".join(sums))
    version = tool_version(args.llvm_profdata)
    trained_cargo = load(args.evidence).get("train_cargo", {})
    cargo_lines = []
    if "cargo" in components:
        if trained_cargo.get("gates_failed") or not trained_cargo.get("crates_complete"):
            sys.exit("the cargo profile has no passing training evidence, so it is not exported")
        cargo_lines = [f"cargo-training-crates {','.join(trained_cargo.get('crates_complete', []))}",
                       f"cargo-training-raw-files {trained_cargo.get('raw_files', 0)}"]
    lines = [
        f"bundle-version {BUNDLE_VERSION}",
        f"target {args.host}",
        f"machine {machine_name()}",
        f"macos {macos_version()}",
        f"deployment-target {DEPLOYMENT[args.host]}",
        f"rust-commit {args.rust_commit}",
        f"llvm-version {args.llvm_version}",
        f"llvm-profdata {version[1] if len(version) > 1 else (version[0] if version else 'unknown')}",
        f"settings-sha256 {settings_hash(args.root, args.host)}",
        f"rustc-perf {args.rustc_perf or 'unknown'}",
        f"training-crates {','.join(train.get('crates_complete', []))}",
        f"training-profiles {','.join(train.get('profiles', []))}",
        f"training-scenarios {train.get('scenarios', '')}",
        f"training-raw-files {train.get('raw_files', 0)}",
        f"training-seconds {train.get('seconds', 0)}",
        *cargo_lines,
        *profile_lines,
        "patches-sha256",
        *[f"  {digest}  {name}" for digest, name in patch_hashes(args.root)],
    ]
    (out / "MANIFEST").write_text("\n".join(lines) + "\n")
    print(f"bundle exported to {out}: {', '.join(sorted(components))}")
    return 0


def fail(message):
    print(f"::error title=stored PGO profile::{message}", flush=True)
    return 1


def check(args):
    bundle = Path(args.bundle)
    manifest = bundle / "MANIFEST"
    sums_file = bundle / "SHA256SUMS"
    for path in (manifest, sums_file):
        if not path.is_file():
            return fail(f"{bundle} has no {path.name}")
    values, profiles, stored_patches = parse_manifest(manifest)
    if values.get("bundle-version") != BUNDLE_VERSION:
        return fail(f"bundle-version {values.get('bundle-version')!r} is not the {BUNDLE_VERSION!r} this recipe reads")
    expected = {
        "target": args.host,
        "machine": MACHINE.get(args.host),
        "rust-commit": args.rust_commit,
        "llvm-version": args.llvm_version,
        "deployment-target": DEPLOYMENT.get(args.host),
        "settings-sha256": settings_hash(args.root, args.host),
    }
    for key, want in expected.items():
        if values.get(key) != want:
            return fail(f"the bundle was made with {key} {values.get(key)!r}, this build has {want!r}. "
                        "Regenerate the profiles (docs/build-assets.md) instead of using a stale bundle")
    machine = machine_name()
    if machine != expected["machine"]:
        return fail(f"profiles for {args.host} are only valid on a native {expected['machine']} machine, "
                    f"this is {machine}")
    tool_lines = tool_version(args.llvm_profdata)
    if not any(args.llvm_version in line for line in tool_lines):
        return fail(f"{args.llvm_profdata} is not LLVM {args.llvm_version}: {tool_lines}")
    sums = {}
    for line in sums_file.read_text().splitlines():
        digest, _, name = line.partition("  ")
        if digest and name:
            sums[name] = digest
    names = {entry[1] for entry in profiles}
    if not any(entry[0] == "rustc" for entry in profiles):
        return fail("the MANIFEST lists no rustc profile")
    if names != set(sums):
        return fail(f"SHA256SUMS lists {sorted(sums)} but the MANIFEST lists {sorted(names)}")
    merged = {}
    for component, name, digest, size, functions in profiles:
        path = bundle / name
        if not path.is_file() or sha256(path) != digest or sums[name] != digest:
            return fail(f"{name} does not match the digest recorded in the MANIFEST and SHA256SUMS")
        if str(path.stat().st_size) != size:
            return fail(f"{name} is {path.stat().st_size} bytes, the MANIFEST says {size}")
        summary, error = profile_summary(args.llvm_profdata, path)
        if summary is None:
            return fail(f"llvm-profdata cannot read {name}: {error}")
        if summary["level"] != "IR":
            return fail(f"{name} has instrumentation level {summary['level']}, expected IR")
        if summary["functions"] < MIN_FUNCTIONS or str(summary["functions"]) != functions:
            return fail(f"{name} holds {summary['functions']} functions, the MANIFEST says {functions} "
                        f"and at least {MIN_FUNCTIONS} are required")
        merged[component] = dict(summary, file=name, sha256=digest, bytes=int(size))
    current = dict((name, digest) for digest, name in patch_hashes(args.root))
    changed = sorted(name for name in set(current) | set(stored_patches) if current.get(name) != stored_patches.get(name))
    if changed:
        print(f"::notice title=stored PGO profile::{len(changed)} fRust patch(es) differ from the ones the profile "
              f"was trained with ({', '.join(changed)}); the build checks how many functions still match", flush=True)
    store(args.evidence, "train", {
        "host": args.host, "machine": machine, "source": "stored", "bundle": str(bundle.name),
        "llvm_profdata": tool_lines, "profiles": values.get("training-profiles", "").split(","),
        "scenarios": values.get("training-scenarios"),
        "crates_requested": values.get("training-crates", "").split(","),
        "crates_complete": values.get("training-crates", "").split(","),
        "raw_files": int(values.get("training-raw-files", "0") or 0), "raw_bytes": 0, "bad_raw_files": 0,
        "trained_on": values.get("macos"), "changed_patches": changed,
        "merged": dict(merged["rustc"]), "components": merged, "gates_failed": [],
    })
    found = ", ".join("%s %d functions" % (name, entry["functions"]) for name, entry in sorted(merged.items()))
    print(f"stored profile ok: {found}, trained on macOS {values.get('macos')}, {len(changed)} changed patch(es)",
          flush=True)
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    cmd = sub.add_parser("settings")
    cmd.add_argument("root")
    cmd.add_argument("host")
    cmd = sub.add_parser("export")
    cmd.add_argument("root")
    cmd.add_argument("out")
    cmd.add_argument("--component", action="append", default=[])
    cmd.add_argument("--rustc-perf", default="")
    cmd = sub.add_parser("check")
    cmd.add_argument("root")
    cmd.add_argument("bundle")
    for cmd in sub.choices["export"], sub.choices["check"]:
        cmd.add_argument("--host", required=True)
        cmd.add_argument("--rust-commit", required=True)
        cmd.add_argument("--llvm-version", required=True)
        cmd.add_argument("--llvm-profdata", required=True)
        cmd.add_argument("--evidence", required=True)
    args = parser.parse_args()
    if args.command == "settings":
        print(settings_hash(args.root, args.host))
        return 0
    args.llvm_profdata = os.path.abspath(args.llvm_profdata)
    return {"export": export, "check": check}[args.command](args)


if __name__ == "__main__":
    sys.exit(main())
