#!/bin/bash
# Apply the ordered fRust patch series onto the pinned Cargo checkout.
# Compiler patches live in support/patches/rustc/ and are applied separately.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
CARGO_SRC=${FRUST_CARGO_SRC:-$ROOT/target/compiler-rewrite/src/rust/src/tools/cargo}
cd "$CARGO_SRC"
for patch in "$ROOT"/support/patches/*.patch; do
    if git apply --check "$patch" >/dev/null 2>&1; then
        git apply "$patch"
        echo "applied $(basename "$patch")"
    elif git apply --reverse --check "$patch" >/dev/null 2>&1; then
        echo "already applied $(basename "$patch")"
    else
        echo "patch does not apply: $patch" >&2
        exit 1
    fi
done
