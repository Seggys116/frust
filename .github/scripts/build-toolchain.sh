#!/bin/bash
# Build a stage-2 toolchain from the pinned sources and pack it as a tar.zst.
# Usage: build-toolchain.sh <control|frust> <workdir> <output.tar.zst> [host-triple]
# control is the unpatched stage-2 with its stock Cargo. frust applies support/patches and is
# packaged with fcargo. Both use the same recipe, support/bootstrap.toml. The host defaults to
# the one in support/upstream.lock and must be the machine this runs on.
set -euo pipefail
variant=${1:?usage: build-toolchain.sh <control|frust> <workdir> <output.tar.zst> [host-triple]}
work=$(mkdir -p "${2:?}" && cd "$2" && pwd)
out=$(cd "$(dirname "${3:?}")" && pwd)/$(basename "$3")
root=$(cd "$(dirname "$0")/../.." && pwd)
lock=$root/support/upstream.lock
[[ $variant == control || $variant == frust ]] || { echo "unknown variant $variant" >&2; exit 2; }

get() {
    awk -F= -v k="$1" '$1 == k { print substr($0, index($0, "=") + 1); exit }' "$lock"
}

host=${4:-$(get host)}
src=$work/rust
build=$work/build
git init -q "$src"
# Bootstrap resolves the prebuilt LLVM from the first parent of the pinned merge commit.
git -C "$src" fetch -q --depth 2 https://github.com/rust-lang/rust.git "$(get rust_commit)"
git -C "$src" -c advice.detachedHead=false checkout -q --detach FETCH_HEAD
# Cargo is patched before bootstrap runs, so its submodule is populated here.
git -C "$src" submodule update -q --init --depth 1 src/tools/cargo
[[ $(git -C "$src/src/tools/cargo" rev-parse HEAD) == "$(get cargo_gitlink)" ]]

if [[ $variant == frust ]]; then
    FRUST_RUST_SRC=$src "$root/support/apply-rustc-patches.sh"
    FRUST_CARGO_SRC=$src/src/tools/cargo "$root/support/apply-patches.sh"
fi

# build-dir goes right after jobs, inside [build]. awk keeps this working with BSD tools.
awk -v dir="$build" '{ print } /^jobs = / { printf "build-dir = \"%s\"\n", dir }' "$root/support/bootstrap.toml" > "$work/bootstrap.toml"
(cd "$src" && RUST_BACKTRACE=1 python3 x.py build --config "$work/bootstrap.toml" --stage 2 \
    --jobs "$(getconf _NPROCESSORS_ONLN)" library compiler rustdoc cargo)

stage2=$build/$host/stage2
cargo_bin=$build/$host/stage2-tools-bin/cargo
dest=$work/prefix/$variant
mkdir -p "$work/prefix"
if [[ $variant == control ]]; then
    cp -a "$stage2" "$dest"
    install -m 755 "$cargo_bin" "$dest/bin/cargo"
else
    (cd "$root/crates" && cargo build --release --locked -p frust-rs --bin fcargo --target-dir "$work/fcargo")
    FRUST_RUST_SRC=$src "$root/support/package-toolchain.sh" "$stage2" "$cargo_bin" \
        "$work/fcargo/release/fcargo" "$dest" "frust-$(get rust_commit | cut -c1-9)-ci" "$host"
fi
rm -rf "$build"
tar -cf - -C "$work/prefix" "$variant" | zstd -T0 -3 -q > "$out"
echo "packed $variant for $host into $out"
