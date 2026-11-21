#!/usr/bin/env python3
"""Check support/official-components.lock, the pins of the official components in the release.

Usage: check-official-components-lock.py [--offline] [lock-file]

Always checked (no network):
  * the header names the release, its date and a 64 hex channel manifest sha256, and the release is
    the rust_release of support/upstream.lock;
  * every entry has five columns, a bundle that is one of the three shipped ones, a component that is
    rust-std, rust-docs or rust-src, a 64 hex sha256 and an https URL on
    static.rust-lang.org/dist/<date>/ that names the component, release and target and ends in .tar.gz
    (rust-src is target independent: its target column is "any" and the file is rust-src-<release>);
  * no (bundle, component, target) appears twice, and every bundle has exactly the entries the
    release promises (rust-docs for the host, rust-src for target "any", rust-std for the host-free
    target list of the bundle). clippy, rustfmt and rust-analyzer are built from source, not pinned here.

Without --offline it also downloads https://static.rust-lang.org/dist/channel-rust-<release>.toml and
its .sha256, and checks that the file's hash is the header's, that the .sha256 file agrees, and that
every entry's sha256 equals the hash the manifest gives for the package at that URL.

Only the Python standard library is used. Exit status 0 means every check passed.
"""
import hashlib
import re
import sys
import tomllib
import urllib.request
from pathlib import Path

HOST = "https://static.rust-lang.org"
HEX64 = re.compile(r"^[0-9a-f]{64}$")

# bundle -> (host target of the host-only components, rust-std targets, in the order of the lock)
BUNDLES = {
    "linux": (
        "x86_64-unknown-linux-gnu",
        [
            "x86_64-unknown-linux-musl",
            "aarch64-unknown-linux-gnu",
            "wasm32-unknown-unknown",
            "wasm32-wasip1",
        ],
    ),
    "macos-aarch64": (
        "aarch64-apple-darwin",
        [
            "x86_64-apple-darwin",
            "aarch64-apple-ios",
            "aarch64-apple-ios-sim",
            "x86_64-apple-ios",
            "aarch64-apple-ios-macabi",
            "x86_64-apple-ios-macabi",
            "wasm32-unknown-unknown",
            "wasm32-wasip1",
        ],
    ),
    "macos-x86_64": (
        "x86_64-apple-darwin",
        [
            "aarch64-apple-darwin",
            "aarch64-apple-ios",
            "aarch64-apple-ios-sim",
            "x86_64-apple-ios",
            "aarch64-apple-ios-macabi",
            "x86_64-apple-ios-macabi",
            "wasm32-unknown-unknown",
            "wasm32-wasip1",
        ],
    ),
}
COMPONENTS = {"rust-std", "rust-docs", "rust-src"}
HOSTS = {host for host, _ in BUNDLES.values()}

errors = []


def fail(message):
    errors.append(message)


def fetch(url):
    request = urllib.request.Request(url, headers={"User-Agent": "frust-ci-lock-check"})
    last = None
    for _ in range(4):
        try:
            with urllib.request.urlopen(request, timeout=60) as response:
                return response.read()
        except OSError as error:
            last = error
    raise SystemExit(f"cannot download {url}: {last}")


def upstream_release(root):
    for line in (root / "support" / "upstream.lock").read_text().splitlines():
        if line.startswith("rust_release="):
            return line.split("=", 1)[1].strip()
    raise SystemExit("support/upstream.lock has no rust_release")


def parse(path):
    header = {}
    entries = []
    for number, line in enumerate(path.read_text().splitlines(), 1):
        if not line.strip():
            continue
        if line.startswith("#"):
            match = re.match(
                r"^# release (\S+) \([0-9a-f]+ [0-9-]+\) dated (\d{4}-\d{2}-\d{2}); channel manifest sha256 (\S+)$",
                line,
            )
            if match:
                header = {"release": match[1], "date": match[2], "manifest": match[3]}
            continue
        columns = line.split()
        if len(columns) != 5:
            fail(f"line {number}: expected 5 columns, got {len(columns)}")
            continue
        entries.append((number, *columns))
    return header, entries


def main():
    args = sys.argv[1:]
    offline = "--offline" in args
    args = [a for a in args if a != "--offline"]
    root = Path(__file__).resolve().parents[2]
    path = Path(args[0]) if args else root / "support" / "official-components.lock"

    header, entries = parse(path)
    if not header:
        raise SystemExit(f"{path}: no '# release <version> (<commit> <date>) dated <date>; channel manifest sha256 <hash>' header")
    release, date, manifest_hash = header["release"], header["date"], header["manifest"]
    if not HEX64.match(manifest_hash):
        fail(f"header manifest sha256 is not 64 lowercase hex: {manifest_hash}")
    expected_release = upstream_release(root)
    if release != expected_release:
        fail(f"lock is for release {release}, support/upstream.lock pins {expected_release}")

    seen = {}
    for number, bundle, component, target, sha, url in entries:
        where = f"line {number} ({bundle} {component} {target})"
        if bundle not in BUNDLES:
            fail(f"{where}: unknown bundle {bundle}")
        if component not in COMPONENTS:
            fail(f"{where}: unknown component {component}")
        if not HEX64.match(sha):
            fail(f"{where}: sha256 is not 64 lowercase hex: {sha}")
        # The package file is named after the component without -preview. rust-std is per target. The
        # manifest serves the architecture-independent rust-docs of x86_64-apple-darwin from the
        # aarch64-apple-darwin file, so for the others any shipped host triple is accepted.
        stem = f"{HOST}/dist/{date}/{component.removesuffix('-preview')}-{release}-"
        if component == "rust-std":
            ok = url == f"{stem}{target}.tar.gz"
        elif component == "rust-src":
            ok = target == "any" and url == f"{HOST}/dist/{date}/rust-src-{release}.tar.gz"
        else:
            ok = url.startswith(stem) and url.endswith(".tar.gz") and url[len(stem) : -len(".tar.gz")] in HOSTS
        if not ok:
            fail(f"{where}: url {url} does not name {component} {release} for {target}")
        key = (bundle, component, target)
        if key in seen:
            fail(f"{where}: duplicate of line {seen[key]}")
        seen[key] = number

    for bundle, (host, targets) in BUNDLES.items():
        want = {("rust-docs", host), ("rust-src", "any")} | {("rust-std", t) for t in targets}
        have = {(c, t) for (b, c, t) in seen if b == bundle}
        for component, target in sorted(want - have):
            fail(f"bundle {bundle} is missing {component} for {target}")
        for component, target in sorted(have - want):
            fail(f"bundle {bundle} has unexpected {component} for {target}")

    checked = 0
    if not offline and not errors:
        manifest_url = f"{HOST}/dist/channel-rust-{release}.toml"
        published = fetch(manifest_url + ".sha256").decode().split()
        if not published or published[0] != manifest_hash:
            fail(f"{manifest_url}.sha256 says {published[:1]}, the header pins {manifest_hash}")
        body = fetch(manifest_url)
        actual = hashlib.sha256(body).hexdigest()
        if actual != manifest_hash:
            fail(f"{manifest_url} hashes to {actual}, the header pins {manifest_hash}")
        manifest = tomllib.loads(body.decode())
        if manifest["date"] != date:
            fail(f"manifest date is {manifest['date']}, the lock says {date}")
        by_url = {}
        for name, package in manifest["pkg"].items():
            for target, files in package.get("target", {}).items():
                if files.get("available") and "url" in files:
                    by_url[files["url"]] = files["hash"]
        for number, bundle, component, target, sha, url in entries:
            published = by_url.get(url)
            if published is None:
                fail(f"line {number}: {url} is not in the channel manifest")
            elif published != sha:
                fail(f"line {number}: {url} is {sha} in the lock and {published} in the manifest")
            checked += 1

    if errors:
        for message in errors:
            print(f"error: {message}", file=sys.stderr)
        raise SystemExit(1)
    mode = "structure only" if offline else f"{checked} hashes match the channel manifest"
    print(f"{path.name}: {len(entries)} entries in {len(BUNDLES)} bundles for {release} ({date}); {mode}")


main()
