#!/bin/sh
# Resolve the pinned Rust object, gitlinks, stage-0 manifest and license notices.
set -eu
root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
lock=$root/support/upstream.lock
src=${1:-/home/seggy116/Projects/frust/target/compiler-rewrite/src/rust}
out=${2:-}

get() {
    key=$1
    awk -F= -v k="$key" '$1 == k { print substr($0, index($0, "=") + 1); exit }' "$lock"
}

commit=$(get rust_commit)
cargo=$(get cargo_gitlink)
llvm=$(get llvm_gitlink)
stage0_commit=$(get stage0_compiler_git_commit)
stage0_hash=$(get stage0_manifest_hash)

cd "$src"
actual=$(git rev-parse HEAD)
tree=$(git ls-tree "$commit" -- src/tools/cargo src/llvm-project)
cargo_head=$(git -C src/tools/cargo rev-parse HEAD)
llvm_head=$(git -C src/llvm-project rev-parse HEAD)
manifest=$(awk -F= '$1 == "compiler_channel_manifest_hash" { print $2; exit }' src/stage0)
stage0_line=$(awk -F= '$1 == "compiler_git_commit_hash" { print $2; exit }' src/stage0)

fail=0
check() {
    name=$1
    want=$2
    got=$3
    if [ "$want" != "$got" ]; then
        echo "mismatch $name want=$want got=$got" >&2
        fail=1
    fi
}

check rust "$commit" "$actual"
check cargo "$cargo" "$cargo_head"
check llvm "$llvm" "$llvm_head"
check stage0_commit "$stage0_commit" "$stage0_line"
check stage0_manifest "$stage0_hash" "$manifest"

echo "$tree" | awk 'NF != 4 || $1 != "160000" || $2 != "commit" { missing = 1 } END { exit missing }' || {
    echo "gitlinks must be two mode-160000 commits" >&2
    fail=1
}

for notice in COPYRIGHT LICENSE-MIT LICENSE-APACHE src/tools/cargo/LICENSE-MIT src/tools/cargo/LICENSE-APACHE src/llvm-project/LICENSE.TXT; do
    if [ ! -s "$notice" ]; then
        echo "missing notice $notice" >&2
        fail=1
    fi
done

report="rust $actual
cargo $cargo_head
llvm $llvm_head
stage0 $stage0_line
stage0_manifest $manifest
gitlinks
$tree"

printf '%s\n' "$report"
if [ -n "$out" ]; then
    printf '%s\n' "$report" > "$out"
fi
exit "$fail"
