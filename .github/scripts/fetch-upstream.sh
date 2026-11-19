#!/bin/bash
# Fetch the pinned rust, cargo and llvm commits with only the paths the pin and patch checks read.
# Usage: fetch-upstream.sh <destination>
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
dest=${1:?usage: fetch-upstream.sh <destination>}
lock=$root/support/upstream.lock

get() {
    awk -F= -v k="$1" '$1 == k { print substr($0, index($0, "=") + 1); exit }' "$lock"
}

# Patched files, as paths relative to the tree the patches apply to.
patched() {
    grep -h '^--- a/' "$@" | sed 's|^--- a/||' | sort -u
}

fetch() {
    local dir=$1 url=$2 commit=$3
    shift 3
    git init -q "$dir"
    printf '/%s\n' "$@" | git -C "$dir" sparse-checkout set --no-cone --stdin
    git -C "$dir" fetch -q --depth 1 --filter=blob:none "$url" "$commit"
    git -C "$dir" checkout -q --detach FETCH_HEAD
}

rustc_patches=("$root"/support/patches/rustc/*.patch)
cargo_patches=("$root"/support/patches/*.patch)
mapfile -t rustc_files < <(patched "${rustc_patches[@]}")
mapfile -t cargo_files < <(patched "${cargo_patches[@]}")

fetch "$dest" https://github.com/rust-lang/rust.git "$(get rust_commit)" \
    COPYRIGHT LICENSE-MIT LICENSE-APACHE src/stage0 "${rustc_files[@]}"
fetch "$dest/src/tools/cargo" https://github.com/rust-lang/cargo.git "$(get cargo_gitlink)" \
    LICENSE-MIT LICENSE-APACHE "${cargo_files[@]}"
fetch "$dest/src/llvm-project" https://github.com/rust-lang/llvm-project.git "$(get llvm_gitlink)" LICENSE.TXT
