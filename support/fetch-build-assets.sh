#!/bin/bash
# Download, verify and unpack build assets of the optimized Linux toolchain build.
# Usage: fetch-build-assets.sh <dest-dir> <asset-name>...
#
# The assets are files of a GitHub release that support/build-assets.lock names. The lock holds one
# asset per line as "<name> <sha256> <size> <url>"; blank lines and lines starting with # are
# ignored, and a "# release: <tag>" comment names the release. docs/build-assets.md says what each
# asset is and how the lock is refreshed.
#
# Each wanted asset is downloaded, its size and sha-256 are checked against the lock, and only then
# is it unpacked into <dest-dir>/<name without .tar.zst>. A name that the lock does not list is an
# error, and so is a missing lock. Nothing is downloaded before every name has been checked.
# An asset that is already unpacked from the same digest is left alone.
#
# Standard output gets one "<name> <directory>" line per asset; everything else goes to standard
# error. Environment: FRUST_BUILD_ASSETS_LOCK replaces the lock file (default: build-assets.lock
# next to this script). Needs curl, zstd, tar, awk and sha256sum or shasum.
set -euo pipefail

die() {
    echo "fetch-build-assets: $*" >&2
    exit 1
}

case ${1:-} in
    -h | --help)
        sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'
        exit 0
        ;;
esac
if [[ $# -lt 2 ]]; then
    echo "usage: fetch-build-assets.sh <dest-dir> <asset-name>..." >&2
    exit 2
fi
dest_arg=$1
shift
wanted=("$@")

here=$(cd "$(dirname "$0")" && pwd)
lock=${FRUST_BUILD_ASSETS_LOCK:-$here/build-assets.lock}
[[ -f $lock ]] || die "the lock file $lock does not exist. It names the release that holds the build assets and lists them (see docs/build-assets.md)"

for tool in curl zstd tar awk; do
    command -v "$tool" > /dev/null || die "$tool is required"
done
if command -v sha256sum > /dev/null; then
    sha256_of() { sha256sum "$1" | awk '{ print $1 }'; }
elif command -v shasum > /dev/null; then
    sha256_of() { shasum -a 256 "$1" | awk '{ print $1 }'; }
else
    die "sha256sum or shasum is required"
fi

names=()
shas=()
sizes=()
urls=()
number=0
while IFS= read -r line || [[ -n $line ]]; do
    number=$((number + 1))
    case $line in
        '' | '#'*) continue ;;
    esac
    name= sha= size= url= extra=
    read -r name sha size url extra <<< "$line"
    [[ -n $url && -z $extra ]] || die "$lock:$number: expected '<name> <sha256> <size> <url>', got: $line"
    [[ $name =~ ^[A-Za-z0-9][A-Za-z0-9._-]*\.tar\.zst$ ]] || die "$lock:$number: asset name '$name' must be a plain file name ending in .tar.zst"
    [[ $sha =~ ^[0-9a-f]{64}$ ]] || die "$lock:$number: '$sha' is not a lowercase sha-256 digest"
    [[ $size =~ ^[0-9]+$ ]] || die "$lock:$number: '$size' is not a size in bytes"
    case $url in
        https://* | file://*) ;;
        *) die "$lock:$number: the url of $name must start with https://" ;;
    esac
    for seen in ${names[@]+"${names[@]}"}; do
        [[ $seen != "$name" ]] || die "$lock:$number: $name is listed twice"
    done
    names+=("$name")
    shas+=("$sha")
    sizes+=("$size")
    urls+=("$url")
done < "$lock"
[[ ${#names[@]} -gt 0 ]] || die "$lock lists no assets"

tag=$(sed -n 's/^# *release: *\([^ ]*\) *$/\1/p' "$lock" | sed -n 1p)
echo "fetch-build-assets: lock $lock${tag:+ (release $tag)}" >&2

# Print the position of asset $1 in the lock arrays, or fail.
index_of() {
    local i
    for ((i = 0; i < ${#names[@]}; i++)); do
        if [[ ${names[i]} == "$1" ]]; then
            echo "$i"
            return 0
        fi
    done
    return 1
}

for want in "${wanted[@]}"; do
    index_of "$want" > /dev/null || die "unknown build asset '$want'. The lock lists: ${names[*]}"
done

mkdir -p "$dest_arg"
dest=$(cd "$dest_arg" && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

for want in "${wanted[@]}"; do
    i=$(index_of "$want")
    name=${names[i]} sha=${shas[i]} size=${sizes[i]} url=${urls[i]}
    dir=$dest/${name%.tar.zst}
    stamp=$dir/.frust-build-asset
    if [[ -f $stamp && $(cat "$stamp") == "$sha" ]]; then
        echo "fetch-build-assets: $name is already unpacked in $dir" >&2
        echo "$name $dir"
        continue
    fi

    file=$tmp/$name
    echo "fetch-build-assets: downloading $name" >&2
    curl --fail --location --silent --show-error --proto '=https,file' --proto-redir '=https' \
        --retry 5 --retry-delay 5 --retry-all-errors --connect-timeout 30 \
        --output "$file" "$url" || die "could not download $name from $url"
    actual_size=$(wc -c < "$file" | tr -d ' ')
    [[ $actual_size == "$size" ]] || die "$name is $actual_size bytes, the lock says $size"
    actual=$(sha256_of "$file")
    [[ $actual == "$sha" ]] || die "checksum mismatch for $name: the lock says $sha, the download is $actual"

    # The digest is the lock's; this guards against a lock entry that points at a malformed archive.
    listing=$tmp/listing
    zstd -dc "$file" | tar -tf - > "$listing" || die "$name is not a zstd-compressed tar archive"
    if grep -Eq '(^/|(^|/)\.\.(/|$))' "$listing"; then
        die "$name holds an absolute path or a .. component"
    fi

    rm -rf "$dir"
    mkdir -p "$dir"
    # The archives hold one top-level directory named like the asset. It is dropped, so that the
    # files land directly in $dir. An archive laid out differently is unpacked as it is.
    strip=0
    top=${name%.tar.zst}
    if ! grep -v "^$top\(/\|\$\)" "$listing" | grep -q .; then
        strip=1
    fi
    zstd -dc "$file" | tar -xf - -C "$dir" --no-same-owner --strip-components="$strip" || die "could not unpack $name"
    printf '%s\n' "$sha" > "$stamp"
    rm -f "$file" "$listing"
    echo "fetch-build-assets: unpacked $name into $dir" >&2
    echo "$name $dir"
done
