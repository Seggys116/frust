#!/usr/bin/env bash
# Create an isolated, shallow checkout of the pinned Rust commit with the submodules the
# optimized Linux build needs, each at its pinned gitlink. Nothing is built here.
#
# Usage: prepare-rust-checkout.sh DEST [SOURCE_CHECKOUT...]
#
# Objects are fetched from the first SOURCE_CHECKOUT that already holds the pinned commit of
# the superproject or of a submodule (local clones, no network). When none has it, the URL from
# .gitmodules (or the upstream Rust repository) is used. Existing checkouts are only read.
set -euo pipefail
if [[ $# -lt 1 ]]; then
    echo "usage: $0 DEST [SOURCE_CHECKOUT...]" >&2
    exit 2
fi
root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
dest=$(realpath -m -- "$1")
shift
sources=("$@")
if [[ ${#sources[@]} -eq 0 ]]; then
    sources=("$root/target/compiler-rewrite/src/rust" "$root/target/compiler-rewrite/optimized-dist/rust")
fi
lock=$root/support/upstream.lock
commit=$(awk -F= '$1 == "rust_commit" { print $2 }' "$lock")
[[ -n $commit ]] || { echo "rust_commit missing from $lock" >&2; exit 2; }
if [[ -e $dest ]]; then
    echo "destination already exists: $dest" >&2
    exit 1
fi

# Fetch one commit into an empty repository at $1 from the first source that has it.
fetch_commit() {
    local dir=$1 want=$2 relpath=$3 fallback=$4 candidate
    mkdir -p "$dir"
    git -C "$dir" init -q
    for candidate in "${sources[@]}"; do
        candidate=$candidate/$relpath
        [[ -e $candidate/.git ]] || continue
        if git -C "$candidate" cat-file -e "$want^{commit}" 2>/dev/null; then
            if git -C "$dir" fetch -q --depth 1 "file://$(realpath "$candidate")" "$want" 2>/dev/null; then
                git -C "$dir" reset -q --hard FETCH_HEAD
                echo "$relpath $want from $candidate"
                return 0
            fi
        fi
    done
    [[ -n $fallback ]] || { echo "no source holds $relpath $want" >&2; return 1; }
    git -C "$dir" fetch -q --depth 1 "$fallback" "$want"
    git -C "$dir" reset -q --hard FETCH_HEAD
    echo "$relpath $want from $fallback"
}

fetch_commit "$dest" "$commit" "" "https://github.com/rust-lang/rust.git"
[[ $(git -C "$dest" rev-parse HEAD) == "$commit" ]]

# Submodules used by the build, the training run and the packaged licence notices.
modules=(
    src/llvm-project src/tools/cargo library/backtrace src/tools/rustc-perf
    src/doc/book src/doc/edition-guide src/doc/embedded-book src/doc/nomicon
    src/doc/reference src/doc/rust-by-example
)
for module in "${modules[@]}"; do
    want=$(git -C "$dest" rev-parse "HEAD:$module")
    url=$(git -C "$dest" config -f .gitmodules --get "submodule.$module.url" || true)
    rmdir "$dest/$module" 2>/dev/null || true
    fetch_commit "$dest/$module" "$want" "$module" "$url"
    [[ $(git -C "$dest/$module" rev-parse HEAD) == "$want" ]]
done
bash "$root/support/verify-pins.sh" "$dest"
echo "prepared $dest"
