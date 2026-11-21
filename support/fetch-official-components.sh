#!/bin/bash
# Install the official Rust components pinned in support/official-components.lock into a toolchain tree.
# Usage: fetch-official-components.sh <bundle> <toolchain-tree> [report-file]
#   bundle          linux, macos-aarch64 or macos-x86_64 (the first column of the lock)
#   toolchain-tree  the tree being packaged; it must already hold the fRust host std
#   report-file     optional; one "<component> <target>" line is written per installed package
#
# The fRust compiler reports the same `rustc -vV` as official 1.100.0, so the official rust-std
# packages for other targets and the rust-docs and rust-src packages work unchanged next to it. Code
# that links rustc_private (clippy, rustfmt, rust-analyzer and its proc-macro server) is built from
# source with the fRust compiler instead and is not fetched here. Each package is downloaded over
# https, its sha256 is checked against the pin before anything is unpacked, and it is unpacked into
# its own temporary directory. rust-src goes to lib/rustlib/src/rust/ (its own layout), where
# rust-analyzer's sysroot discovery looks for library/. Packages are installed the way
# rust-installer does: the entries of the package's manifest.in, never over an existing path, so the
# host std built by fRust cannot be replaced by an official one.
#
# FRUST_OFFICIAL_CACHE=<dir> keeps the downloads (named after the file in the url) between runs; a
# cached file is used only when it matches the pin, and is downloaded again when it does not.
# FRUST_OFFICIAL_LOCK=<file> reads another lock file (the self-test of this script uses it).
# Works on Linux and macOS: no GNU-only options, sha256 through sha256sum or shasum.
set -euo pipefail
trap 'echo "fetch-official-components.sh failed at line $LINENO" >&2' ERR

if [[ $# -lt 2 || $# -gt 3 ]]; then
    echo "usage: fetch-official-components.sh <linux|macos-aarch64|macos-x86_64> <toolchain-tree> [report-file]" >&2
    exit 2
fi
root=$(cd "$(dirname "$0")/.." && pwd)
bundle=$1
tree=$2
report=${3:-}
lock=${FRUST_OFFICIAL_LOCK:-$root/support/official-components.lock}
cache=${FRUST_OFFICIAL_CACHE:-}

die() {
    echo "fetch-official-components: $*" >&2
    exit 1
}

[[ $bundle =~ ^[a-z0-9][a-z0-9_-]*$ ]] || die "invalid bundle name $bundle"
[[ -f $lock ]] || die "lock file $lock is missing"
[[ -d $tree/bin && -d $tree/lib/rustlib ]] || die "$tree is not a toolchain tree (no bin/ or lib/rustlib/)"
tree=$(cd "$tree" && pwd)
if [[ -n $cache ]]; then
    mkdir -p "$cache"
    cache=$(cd "$cache" && pwd)
fi
if [[ -n $report ]]; then
    : > "$report"
fi

if command -v sha256sum > /dev/null; then
    sha256_of() { sha256sum "$1" | awk '{ print $1 }'; }
elif command -v shasum > /dev/null; then
    sha256_of() { shasum -a 256 "$1" | awk '{ print $1 }'; }
else
    die "neither sha256sum nor shasum is installed"
fi

retry_all=()
if curl --help all 2> /dev/null | grep -q -- '--retry-all-errors'; then
    retry_all=(--retry-all-errors)
fi

# A relative path without empty, "." or ".." components, as found in tar listings and manifests.
safe_relative() {
    local path=$1 part
    [[ -n $path && $path != /* ]] || return 1
    local IFS=/
    for part in $path; do
        case $part in
            '' | . | ..) return 1 ;;
        esac
    done
    return 0
}

work=$(mktemp -d "${TMPDIR:-/tmp}/frust-official.XXXXXX")
trap 'rm -rf "$work"' EXIT

download() {
    local url=$1 sha=$2 dest=$3 name
    name=${url##*/}
    if [[ -n $cache && -f $cache/$name ]]; then
        if [[ $(sha256_of "$cache/$name") == "$sha" ]]; then
            cp "$cache/$name" "$dest"
            return 0
        fi
        echo "cached $name does not match its pin, downloading it again" >&2
        rm -f "$cache/$name"
    fi
    curl -fsSL --retry 5 --retry-delay 5 --connect-timeout 30 ${retry_all[@]+"${retry_all[@]}"} -o "$dest" "$url"
    local got
    got=$(sha256_of "$dest")
    if [[ $got != "$sha" ]]; then
        rm -f "$dest"
        die "sha256 mismatch for $url: expected $sha, got $got"
    fi
    if [[ -n $cache ]]; then
        cp "$dest" "$cache/$name.partial"
        mv "$cache/$name.partial" "$cache/$name"
    fi
}

# Refuse a tarball that holds anything but regular files and directories at safe relative paths.
check_listing() {
    local archive=$1 line first
    while IFS= read -r line; do
        safe_relative "$line" || die "$archive has the unsafe entry path '$line'"
    done < <(tar -tzf "$archive")
    while IFS= read -r line; do
        first=${line:0:1}
        case $first in
            - | d) ;;
            *) die "$archive has an entry that is neither a file nor a directory: $line" ;;
        esac
    done < <(tar -tzvf "$archive")
}

# Copy one manifest.in entry (file:<path> or dir:<path>) from package component dir $1 into the tree.
install_entry() {
    local source_root=$1 kind=$2 rel=$3 source dest
    safe_relative "$rel" || die "unsafe path '$rel' in a package manifest"
    source=$source_root/$rel
    dest=$tree/$rel
    [[ ! -L $source ]] || die "package entry $rel is a symbolic link"
    if [[ -e $dest || -L $dest ]]; then
        die "refusing to overwrite $rel, it is already in the toolchain tree"
    fi
    mkdir -p "$(dirname "$dest")"
    case $kind in
        file)
            [[ -f $source ]] || die "package file $rel is missing"
            cp "$source" "$dest"
            ;;
        dir)
            [[ -d $source ]] || die "package directory $rel is missing"
            cp -R "$source" "$dest"
            ;;
        *) die "unknown manifest entry kind '$kind' for $rel" ;;
    esac
}

install_package() {
    local component=$1 target=$2 package=$3 entry_count=0 listing sub name
    local -a expected
    # The build leaves lib/rustlib/src as an empty directory; sources already in it would be mixed.
    if [[ $component == rust-src ]] && [[ -n $(find "$tree/lib/rustlib/src" -type f 2> /dev/null | sed -n 1p) ]]; then
        die "refusing to install rust-src: lib/rustlib/src already holds files in the toolchain tree"
    fi
    # Official std files carry other hashes than ours: merging into an existing target would leave two libstds.
    if [[ $component == rust-std && ( -e $tree/lib/rustlib/$target || -L $tree/lib/rustlib/$target ) ]]; then
        die "refusing to install rust-std for $target: lib/rustlib/$target is already in the toolchain tree"
    fi
    case $component in
        rust-std) expected=("rust-std-$target") ;;
        rust-docs) expected=(rust-docs) ;;
        rust-src) expected=(rust-src) ;;
    esac
    # The one top directory of the tarball is <package>-<version>-<target>.
    local -a tops=()
    for sub in "$package"/*; do
        [[ -d $sub ]] && tops+=("$sub")
    done
    [[ ${#tops[@]} -eq 1 ]] || die "$component for $target: expected one top directory in the package, found ${#tops[@]}"
    local top=${tops[0]}
    [[ -f $top/components ]] || die "$component for $target: the package has no components file"
    local listed
    listed=$(tr -d '\r' < "$top/components" | sed '/^$/d')
    [[ $listed == "${expected[0]}" ]] || die "$component for $target: the package holds '$listed', expected '${expected[0]}'"
    for name in "${expected[@]}"; do
        local manifest=$top/$name/manifest.in
        [[ -f $manifest ]] || die "$component for $target: $name/manifest.in is missing"
        while IFS= read -r listing || [[ -n $listing ]]; do
            [[ -n $listing ]] || continue
            case $listing in
                file:*) install_entry "$top/$name" file "${listing#file:}" ;;
                dir:*) install_entry "$top/$name" dir "${listing#dir:}" ;;
                *) die "$component for $target: unreadable manifest line '$listing'" ;;
            esac
            entry_count=$((entry_count + 1))
        done < "$manifest"
    done
    [[ $entry_count -gt 0 ]] || die "$component for $target: the package manifest is empty"
    case $component in
        rust-std)
            local rlibs=("$tree/lib/rustlib/$target/lib/"*.rlib)
            [[ -e ${rlibs[0]} ]] || die "rust-std for $target installed no lib/rustlib/$target/lib/*.rlib"
            ;;
        rust-src)
            [[ -f $tree/lib/rustlib/src/rust/library/std/src/lib.rs && -f $tree/lib/rustlib/src/rust/library/core/src/lib.rs ]] ||
                die "rust-src installed no lib/rustlib/src/rust/library/{core,std}/src/lib.rs"
            ;;
        rust-docs)
            [[ -f $tree/share/doc/rust/html/index.html ]] || die "rust-docs installed no share/doc/rust/html/index.html"
            ;;
    esac
}

count=0
while IFS= read -r line || [[ -n $line ]]; do
    case $line in
        '' | '#'*) continue ;;
    esac
    read -r line_bundle component target sha url extra <<< "$line"
    [[ -z ${extra:-} && -n ${url:-} ]] || die "malformed lock line: $line"
    [[ $line_bundle == "$bundle" ]] || continue
    case $component in
        rust-std | rust-docs | rust-src) ;;
        *) die "lock names the unsupported component $component" ;;
    esac
    [[ $target =~ ^[a-z0-9][a-z0-9_.-]*$ ]] || die "invalid target $target in the lock"
    [[ $sha =~ ^[0-9a-f]{64}$ ]] || die "invalid sha256 for $component $target in the lock"
    case $url in
        https://static.rust-lang.org/dist/*.tar.gz) ;;
        *) die "lock url $url is not an https://static.rust-lang.org/dist/ tarball" ;;
    esac
    safe_relative "${url#https://}" || die "lock url $url is not plain"

    echo "official component: $component $target"
    package=$work/package
    mkdir "$package"
    archive=$work/package.tar.gz
    download "$url" "$sha" "$archive"
    check_listing "$archive"
    tar -xzf "$archive" --no-same-owner -C "$package"
    rm -f "$archive"
    install_package "$component" "$target" "$package"
    rm -rf "$package"
    if [[ -n $report ]]; then
        printf '%s %s\n' "$component" "$target" >> "$report"
    fi
    count=$((count + 1))
done < "$lock"

[[ $count -gt 0 ]] || die "bundle $bundle has no entries in $lock"
echo "installed $count official packages for bundle $bundle into $tree"
