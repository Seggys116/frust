#!/usr/bin/env bash
# Package a FRUST_PROFILE_EXPORT directory (rustc-pgo, rustdoc-pgo, clippy-pgo and rustc-bolt
# .profdata, SHA256SUMS, MANIFEST) as one reproducible tarball whose single top-level directory
# can be extracted anywhere and passed to support/optimize-linux-v2.sh as FRUST_STORED_PROFILES.
#
# Usage: package-profiles.sh PROFILE_DIR OUT.tar.zst
set -euo pipefail
if [[ $# -ne 2 ]]; then
    echo "usage: $0 PROFILE_DIR OUT.tar.zst" >&2
    exit 2
fi
src=$(realpath -- "$1")
out=$(realpath -m -- "$2")
[[ $out == *.tar.zst ]] || { echo "OUT must end in .tar.zst" >&2; exit 2; }
[[ ! -e $out ]] || { echo "exists: $out" >&2; exit 1; }
name=$(basename -- "$out" .tar.zst)
files=(MANIFEST SHA256SUMS clippy-pgo.profdata rustc-bolt.profdata rustc-pgo.profdata
    rustdoc-pgo.profdata)
for file in "${files[@]}"; do
    [[ -s $src/$file ]] || { echo "missing $src/$file" >&2; exit 1; }
done
(cd "$src" && sha256sum --quiet -c SHA256SUMS)
grep -q '^target-cpu ' "$src/MANIFEST" || { echo "MANIFEST has no target-cpu line" >&2; exit 1; }
tar --sort=name --owner=0 --group=0 --numeric-owner --mtime=@0 \
    --transform "s|^|$name/|" -C "$src" -cf - "${files[@]}" | zstd -19 -T0 -q -o "$out"
echo "packaged $out ($(stat -c %s "$out") bytes)"
