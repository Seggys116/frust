#!/usr/bin/env python3
"""Assemble an Ubuntu 24.04 (noble) amd64 build sysroot for the LLVM build tools.

Usage: fetch-noble-sysroot.py DEST

Downloads the newest version of each package below from the noble, noble-updates and
noble-security main indexes on archive.ubuntu.com, checks each .deb against the SHA256 in the
index, and extracts it into DEST/root with dpkg-deb. DEST/packages.txt records name, version,
filename and sha256 of everything used. Tools linked against this sysroot need at most the
glibc of a stock Ubuntu 24.04 system (2.39).
"""
import gzip
import hashlib
import subprocess
import sys
import urllib.error
import urllib.request
from pathlib import Path

MIRROR = "http://archive.ubuntu.com/ubuntu"
SUITES = ["noble", "noble-updates", "noble-security"]
PACKAGES = [
    "libc6", "libc6-dev", "libcrypt1", "libcrypt-dev", "linux-libc-dev",
    "libgcc-13-dev", "libstdc++-13-dev", "libgcc-s1", "libstdc++6",
    "zlib1g", "zlib1g-dev", "libzstd1", "libzstd-dev",
]


def parse(text):
    for block in text.split("\n\n"):
        fields = {}
        for line in block.splitlines():
            if line[:1] in (" ", "\t") or ":" not in line:
                continue
            key, value = line.split(":", 1)
            fields[key] = value.strip()
        if "Package" in fields:
            yield fields


def fetch(url):
    request = urllib.request.Request(url, headers={"User-Agent": "frust-sysroot-fetch"})
    try:
        with urllib.request.urlopen(request) as response:
            return response.read()
    except urllib.error.HTTPError as error:
        sys.exit(f"{url}: {error}")


def newer(a, b):
    return subprocess.run(["dpkg", "--compare-versions", a, "gt", b]).returncode == 0


def main():
    dest = Path(sys.argv[1]).resolve()
    (dest / "debs").mkdir(parents=True, exist_ok=True)
    root = dest / "root"
    root.mkdir(exist_ok=True)
    best = {}
    for suite in SUITES:
        url = f"{MIRROR}/dists/{suite}/main/binary-amd64/Packages.gz"
        text = gzip.decompress(fetch(url)).decode()
        for fields in parse(text):
            name = fields["Package"]
            if name in PACKAGES and (name not in best or newer(fields["Version"], best[name]["Version"])):
                best[name] = fields
    missing = sorted(set(PACKAGES) - set(best))
    if missing:
        sys.exit(f"not in the noble indexes: {missing}")
    lines = []
    for name in PACKAGES:
        fields = best[name]
        deb = dest / "debs" / Path(fields["Filename"]).name
        if not deb.exists() or hashlib.sha256(deb.read_bytes()).hexdigest() != fields["SHA256"]:
            data = fetch(f"{MIRROR}/{fields['Filename']}")
            if hashlib.sha256(data).hexdigest() != fields["SHA256"]:
                sys.exit(f"sha256 mismatch for {fields['Filename']}")
            deb.write_bytes(data)
        subprocess.run(["dpkg-deb", "-x", str(deb), str(root)], check=True)
        lines.append(f"{name} {fields['Version']} {fields['Filename']} {fields['SHA256']}")
    # Merged /usr, as base-files sets it up: libc.so names /lib/x86_64-linux-gnu/libc.so.6.
    # Clang then finds GCC through lib/gcc/... and its depfiles name C++ headers as
    # lib/gcc/x86_64-linux-gnu/13/../../../../include/...; ninja folds ".." lexically to
    # include/..., so that path must exist too, or every object stays dirty forever.
    for link, target in (("lib", "usr/lib"), ("lib64", "usr/lib64"), ("include", "usr/include")):
        if not (root / link).exists():
            (root / link).symlink_to(target)
    (dest / "packages.txt").write_text("\n".join(lines) + "\n")
    print("\n".join(lines))


main()
