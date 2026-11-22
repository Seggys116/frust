#!/bin/bash
# Add or replace the lock lines of build assets that were uploaded to a GitHub release.
# Usage: lock-build-asset.sh <release-tag> <asset-file>...
#
# For each file this writes "<name> <sha256> <size> <url>" into support/build-assets.lock (or the
# file named by FRUST_BUILD_ASSETS_LOCK): a line of the same asset name is replaced in place, a new
# name is appended. The url is https://github.com/<repository>/releases/download/<tag>/<name>, with
# the repository from GITHUB_REPOSITORY (default Seggys116/frust). The digest and size are those of
# the local file, so upload exactly that file, then run
#   bash support/fetch-build-assets.sh "$(mktemp -d)" <name>...
# which downloads each asset again and compares it with the lock (docs/build-assets.md).
set -euo pipefail
if [[ $# -lt 2 ]]; then
    echo "usage: lock-build-asset.sh <release-tag> <asset-file>..." >&2
    exit 2
fi
tag=$1
shift
[[ $tag =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]] || { echo "bad release tag: $tag" >&2; exit 2; }
here=$(cd "$(dirname "$0")" && pwd)
lock=${FRUST_BUILD_ASSETS_LOCK:-$here/../build-assets.lock}
[[ -f $lock ]] || { echo "no lock file $lock" >&2; exit 1; }
repository=${GITHUB_REPOSITORY:-Seggys116/frust}

if command -v sha256sum > /dev/null; then
    sha256_of() { sha256sum "$1" | awk '{ print $1 }'; }
else
    sha256_of() { shasum -a 256 "$1" | awk '{ print $1 }'; }
fi

for file in "$@"; do
    [[ -f $file ]] || { echo "not a file: $file" >&2; exit 1; }
    name=$(basename "$file")
    [[ $name =~ ^[A-Za-z0-9][A-Za-z0-9._-]*\.tar\.zst$ ]] || { echo "asset name must be a plain file name ending in .tar.zst: $name" >&2; exit 1; }
    sha=$(sha256_of "$file")
    size=$(wc -c < "$file" | tr -d ' ')
    line="$name $sha $size https://github.com/$repository/releases/download/$tag/$name"
    tmp=$(mktemp)
    if grep -q "^$name " "$lock"; then
        awk -v name="$name" -v line="$line" '$1 == name { print line; next } { print }' "$lock" > "$tmp"
        verb=replaced
    else
        { cat "$lock"; printf '%s\n' "$line"; } > "$tmp"
        verb=added
    fi
    cat "$tmp" > "$lock"
    rm -f "$tmp"
    echo "$verb $line"
done
