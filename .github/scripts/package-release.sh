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

archive=frust-toolchain-$target$suffix.tar.gz
tar -cf - -C "$work/tree" . | gzip -9n > "$out/$archive"
# sha256sum is not on macOS; both tools print "<digest>  <name>".
if command -v sha256sum > /dev/null; then
    (cd "$out" && sha256sum "$archive" > "$archive.sha256")
else
    (cd "$out" && shasum -a 256 "$archive" > "$archive.sha256")
fi
printf '%s\n' "$id" > "$out/frust-toolchain-$target$suffix.id"
echo "packaged $id as $out/$archive"
