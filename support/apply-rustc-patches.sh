#!/bin/bash
# Apply compiler patches onto the pinned Rust checkout.
# Cargo patches in support/patches/*.patch are not applied here.
# support/patches/rustc/dropped/ holds rejected candidates and is not applied.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
RUST_SRC=${FRUST_RUST_SRC:-$ROOT/target/compiler-rewrite/src/rust}
cd "$RUST_SRC"
shopt -s nullglob
patches=("$ROOT"/support/patches/rustc/*.patch)
if [[ ${#patches[@]} -eq 0 ]]; then
  echo "no rustc patches"
  exit 0
fi
for patch in "${patches[@]}"; do
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
