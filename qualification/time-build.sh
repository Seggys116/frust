#!/bin/bash
# Time one release build of a frozen cohort.
# Usage: time-build.sh <toolchain-bin-dir> <workload> <kind> <target-dir> <log>
# kind is cold, leaf, wide, noop, or holdout.
# cold deletes nothing; the caller supplies an empty target directory.
# leaf, wide, and holdout apply the frozen patch, build, and reverse it.
# noop builds the clean tree.
set -euo pipefail
root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
# Paths are resolved before the cohort cd. A relative PATH would not survive it.
abs() {
  if [[ $1 = /* ]]; then printf '%s\n' "$1"; else printf '%s\n' "$PWD/$1"; fi
}
bins=$(abs "$1")
workload=$2
kind=$3
target=$(abs "$4")
log=$(abs "$5")
cohort=$root/target/compiler-rewrite/cohorts/$workload
case "$workload:$kind" in
  ripgrep:leaf) patch=$root/qualification/edits/ripgrep-leaf.patch ;;
  ripgrep:wide) patch=$root/qualification/edits/ripgrep-wide.patch ;;
  syn:leaf) patch=$root/qualification/edits/syn-leaf.patch ;;
  syn:wide) patch=$root/qualification/edits/syn-wide.patch ;;
  regex:leaf) patch=$root/qualification/edits/regex-leaf.patch ;;
  regex:wide) patch=$root/qualification/edits/regex-wide.patch ;;
  fd:holdout) patch=$root/qualification/edits/fd-holdout.patch ;;
  *:cold|*:noop) patch= ;;
  *) echo "unknown workload/kind $workload $kind" >&2; exit 2 ;;
esac
case "$workload" in
  ripgrep) package=ripgrep ;;
  # The generated syn lock also contains registry syn 3.0.10 and syn 2.0.119.
  # -p syn is ambiguous; the path package is the workload.
  syn)
    syn_version=$(awk 'BEGIN{p=0} /^\[package\]/{p=1; next} /^\[/{p=0} p && $1=="version"{gsub(/"/,"",$3); print $3; exit}' "$cohort/Cargo.toml")
    package="path+file://${cohort}#${syn_version}"
    ;;
  regex) package=regex ;;
  fd) package=fd-find ;;
  *) echo "unknown workload $workload" >&2; exit 2 ;;
esac

export PATH="$bins:/usr/bin:/bin"
unset RUSTC CARGO RUSTFLAGS RUSTUP_TOOLCHAIN FRUST_INVOCATION FRUST_PROGRESS FRUST_CARGO FRUST_CENTRAL_TARGET || true
# The optimized compiler is selected by the frustc hardlink. Suites that invoke rustc stay on the stock pipeline.
if [[ -x "$bins/frustc" ]]; then
  export RUSTC="$bins/frustc"
fi
export CARGO_PROFILE_RELEASE_OPT_LEVEL=3
export CARGO_PROFILE_RELEASE_LTO=fat
export CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1
export CARGO_PROFILE_RELEASE_INCREMENTAL=true
export CARGO_PROFILE_RELEASE_DEBUG=0
export CARGO_PROFILE_RELEASE_STRIP=none
export CARGO_TERM_COLOR=never
export CARGO_TARGET_DIR=$target
export FRUST_PROGRESS=plain

if [[ -n "$patch" ]]; then
  patch --fuzz=0 -p1 -d "$cohort" < "$patch"
fi
cd "$cohort"
cleanup() {
  status=$?
  if [[ -n "${patch:-}" ]]; then
    patch --fuzz=0 -R -p1 -d "$cohort" < "$patch" || true
  fi
  exit "$status"
}
trap cleanup EXIT

{
  echo "workload $workload"
  echo "kind $kind"
  echo "package $package"
  echo "bins $bins"
  echo "cwd $(pwd)"
  echo "target $target"
  echo "rustc $($bins/rustc -Vv | head -n 1)"
  set +e
  /usr/bin/time -v cargo build --release --locked --offline -p "$package"
  code=$?
  set -e
  echo "EXIT:$code"
} > "$log" 2>&1
exit "$code"
