#!/bin/bash
# Turn a built toolchain into the three release files frust-install downloads.
# Usage: package-release.sh <toolchain.tar.zst> <version> <target> <frust-install> <out-dir> [baseline|v3]
# Writes frust-toolchain-<name>.tar.gz, .tar.gz.sha256 and .id into <out-dir>, where <name> is
# <target>, or <target>-v3 for the v3 variant. The archive holds the toolchain at its root with
# frust-install added and the manifest id set to the release id (frust-<version>-<rust commit>,
# plus -v3 for the v3 variant, so the two builds of one release never share an install directory
# or a Cargo cache key).
# The optional variant is only for x86_64-unknown-linux-gnu, the one target with two builds. It is
# recorded in the manifest, where frust-install reads it: baseline keeps the plain names, v3 builds
# the compiler and tools for x86-64-v3. Without it the manifest names no variant.
set -euo pipefail
if [[ $# -lt 5 || $# -gt 6 ]]; then
    echo "usage: package-release.sh <toolchain.tar.zst> <version> <target> <frust-install> <out-dir> [baseline|v3]" >&2
    exit 2
fi
root=$(cd "$(dirname "$0")/../.." && pwd)
input=$1
version=$2
target=$3
installer=$4
mkdir -p "$5"
out=$(cd "$5" && pwd)
variant=${6:-}
[[ $version =~ ^[0-9][0-9A-Za-z.+-]*$ ]] || { echo "invalid version $version" >&2; exit 2; }
case $variant in
    '' | baseline) suffix= ;;
    v3) suffix=-v3 ;;
    *) echo "unknown variant $variant, expected baseline or v3" >&2; exit 2 ;;
esac
if [[ -n $variant && $target != x86_64-unknown-linux-gnu ]]; then
    echo "variant $variant is only built for x86_64-unknown-linux-gnu, not $target" >&2
    exit 2
fi

rust_commit=$(awk -F= '$1 == "rust_commit" { print $2; exit }' "$root/support/upstream.lock")
id=frust-$version-${rust_commit:0:9}$suffix
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

mkdir "$work/tree"
zstd -dc "$input" | tar -xf - -C "$work"
mv "$work/frust"/* "$work/tree"/
install -m 755 "$installer" "$work/tree/bin/frust-install"
printf '%s\n' "$id" > "$work/tree/frust-toolchain-id"
python3 -I - "$work/tree/manifest.json" "$id" "$version" "$target" "$variant" <<'PY'
import json
import sys

path, release_id, version, target, variant = sys.argv[1:]
manifest = json.load(open(path))
manifest.update({"id": release_id, "version": version, "target": target})
if variant:
    manifest["variant"] = variant
with open(path, "w") as out:
    json.dump(manifest, out, separators=(",", ":"))
    out.write("\n")
PY

# Symlinks with an absolute or empty target point into the build machine (rustc-src/rust does)
# and the installer rejects them, so they are dropped.
while IFS= read -r -d '' link; do
    destination=$(readlink "$link")
    case $destination in
        ''|/*) echo "dropping symlink ${link#"$work/tree"/} -> ${destination:-<empty>}"; rm -f "$link" ;;
    esac
done < <(find "$work/tree" -type l -print0)

# Every binary the release promises has to be in the built tree before anything is downloaded.
# cargo-clippy, clippy-driver, rustfmt, cargo-fmt, rust-analyzer and libexec/rust-analyzer-proc-macro-srv
# link the compiler's own crates, so they come from the build (package-toolchain.sh), never from the
# official packages.
for required in bin/cargo bin/rustc bin/rustdoc bin/cargo-clippy bin/clippy-driver bin/rustfmt bin/cargo-fmt \
    bin/rust-analyzer libexec/rust-analyzer-proc-macro-srv bin/frust-install; do
    if [[ ! -f $work/tree/$required || ! -x $work/tree/$required ]]; then
        echo "package-release.sh: the toolchain has no executable $required" >&2
        exit 1
    fi
done
# Extra command names are hard links, so sysroot discovery and the argv[0] dispatch of the patched
# Cargo see the real binary: frust and fclippy are Cargo, frustfmt is rustfmt, frustup is the installer.
ln -f "$work/tree/bin/cargo" "$work/tree/bin/frust"
ln -f "$work/tree/bin/cargo" "$work/tree/bin/fclippy"
ln -f "$work/tree/bin/rustfmt" "$work/tree/bin/frustfmt"
ln -f "$work/tree/bin/frust-install" "$work/tree/bin/frustup"

# Extra std targets, rust-docs and rust-src are the official packages, pinned by sha256 in
# support/official-components.lock (the fRust rustc reports the same -vV as official 1.100.0).
case $target in
    x86_64-unknown-linux-gnu) bundle=linux ;;
    aarch64-apple-darwin) bundle=macos-aarch64 ;;
    x86_64-apple-darwin) bundle=macos-x86_64 ;;
    *) echo "package-release.sh: no official component bundle for $target" >&2; exit 2 ;;
esac
"$root/support/fetch-official-components.sh" "$bundle" "$work/tree" "$work/official-components.txt"
for required in lib/rustlib/src/rust/library/core/src/lib.rs lib/rustlib/src/rust/library/std/src/lib.rs lib/rustlib/src/rust/library/Cargo.toml; do
    if [[ ! -f $work/tree/$required ]]; then
        echo "package-release.sh: the toolchain has no $required (rust-src)" >&2
        exit 1
    fi
done
for linked in frust fclippy fcargo fcargo-ui frustc frustdoc frustfmt frustup; do
    if [[ ! -f $work/tree/bin/$linked ]]; then
        echo "package-release.sh: the toolchain has no bin/$linked" >&2
        exit 1
    fi
done

# Record what the tree holds in the manifest (frust-install lists targets and components from it)
# and refuse a tree whose target set differs from the host plus the pinned bundle.
python3 -I - "$work/tree" "$work/official-components.txt" "$root/support/official-components.lock" "$bundle" "$target" <<'PY'
import glob
import json
import os
import sys

tree, report, lock, bundle, host = sys.argv[1:]
pinned = []
for line in open(lock):
    fields = line.split()
    if len(fields) == 5 and not line.startswith("#") and fields[0] == bundle and fields[1] == "rust-std":
        pinned.append(fields[2])
reported = []
official = set()
for line in open(report):
    component, component_target = line.split()
    if component == "rust-std":
        reported.append(component_target)
    else:
        official.add(component)
if reported != pinned:
    sys.exit(f"package-release.sh: fetched std targets {reported} differ from the pinned {pinned}")
targets = [host] + pinned
found = sorted(
    os.path.basename(path)
    for path in glob.glob(os.path.join(tree, "lib", "rustlib", "*"))
    if glob.glob(os.path.join(path, "lib", "*.rlib"))
)
if found != sorted(targets):
    sys.exit(f"package-release.sh: lib/rustlib holds std for {found}, expected {sorted(targets)}")
for name in targets:
    libs = glob.glob(os.path.join(tree, "lib", "rustlib", name, "lib", "lib*.rlib"))
    names = {os.path.basename(path).split("-")[0] for path in libs}
    missing = {"libcore", "liballoc", "libstd"} - names
    if missing:
        sys.exit(f"package-release.sh: std for {name} lacks {sorted(missing)}")
components = ["cargo", "rustc", "rustdoc", "clippy", "rustfmt"]
if not os.path.isfile(os.path.join(tree, "bin", "rust-analyzer")):
    sys.exit("package-release.sh: bin/rust-analyzer is missing")
components.append("rust-analyzer")
if "rust-docs" in official:
    components.append("rust-docs")
if "rust-src" in official:
    components.append("rust-src")
if not {"rust-docs", "rust-src"} <= set(components):
    sys.exit("package-release.sh: rust-docs or rust-src was not installed")
path = os.path.join(tree, "manifest.json")
manifest = json.load(open(path))
manifest["targets"] = targets
manifest["components"] = components
with open(path, "w") as out:
    json.dump(manifest, out, separators=(",", ":"))
    out.write("\n")
PY

archive=frust-toolchain-$target$suffix.tar.gz
# pigz writes the same gzip format on all cores; the tree is several GB with the official components.
compress=gzip
command -v pigz > /dev/null && compress=pigz
tar -cf - -C "$work/tree" . | "$compress" -9n > "$out/$archive"
# sha256sum is not on macOS; both tools print "<digest>  <name>".
if command -v sha256sum > /dev/null; then
    (cd "$out" && sha256sum "$archive" > "$archive.sha256")
else
    (cd "$out" && shasum -a 256 "$archive" > "$archive.sha256")
fi
printf '%s\n' "$id" > "$out/frust-toolchain-$target$suffix.id"
echo "packaged $id as $out/$archive"
