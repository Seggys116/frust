#!/bin/bash
# Time one release build of a frozen cohort.
# Usage: time-build.sh <toolchain-bin-dir> <workload> <kind> <target-dir> <log>
# kind is seed, cold, leaf, wide, noop, or holdout.
# seed and noop build the unmodified sources. Cold uses the leaf or holdout
# edit in an empty target. Edits are reversed before a sample is accepted.
set -euo pipefail
if [[ $# -ne 5 ]]; then
  echo "usage: time-build.sh <toolchain-bin-dir> <workload> <kind> <target-dir> <log>" >&2
  exit 2
fi
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
edit=$kind
if [[ $kind == cold ]]; then
  edit=leaf
  if [[ $workload == fd ]]; then
    edit=holdout
  fi
  if [[ -d $target && -n $(find "$target" -mindepth 1 -maxdepth 1 -print -quit) ]]; then
    echo "cold target must be empty: $target" >&2
    exit 2
  fi
fi
case "$workload:$edit" in
  ripgrep:leaf) patch=$root/qualification/edits/ripgrep-leaf.patch ;;
  ripgrep:wide) patch=$root/qualification/edits/ripgrep-wide.patch ;;
  syn:leaf) patch=$root/qualification/edits/syn-leaf.patch ;;
  syn:wide) patch=$root/qualification/edits/syn-wide.patch ;;
  regex:leaf) patch=$root/qualification/edits/regex-leaf.patch ;;
  regex:wide) patch=$root/qualification/edits/regex-wide.patch ;;
  fd:holdout) patch=$root/qualification/edits/fd-holdout.patch ;;
  *:seed|*:noop) patch= ;;
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
unset RUSTC CARGO RUSTFLAGS RUSTUP_TOOLCHAIN FRUST_INVOCATION FRUST_PROGRESS FRUST_CARGO FRUST_CENTRAL_TARGET
unset CARGO_INCREMENTAL CARGO_ENCODED_RUSTFLAGS RUSTC_WRAPPER RUSTC_WORKSPACE_WRAPPER
unset CARGO_BUILD_TARGET RUSTC_BOOTSTRAP CARGO_BUILD_RUSTFLAGS CARGO_BUILD_RUSTC
unset CARGO_BUILD_RUSTC_WRAPPER CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER
export CARGO_BUILD_BUILD_DIR=$target
# The optimized compiler is selected by the frustc hardlink. Suites that invoke rustc stay on the stock pipeline.
if [[ ! -x "$bins/cargo" || ! -x "$bins/rustc" ]]; then
  echo "missing cargo or rustc in $bins" >&2
  exit 2
fi
export RUSTC="$bins/rustc"
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

case "$kind" in
  leaf|wide|holdout|noop)
    if [[ ! -d "$target/release" ]]; then
      echo "seed the release target before taking a $kind sample: $target" >&2
      exit 2
    fi ;;
esac

git_dir=$(git -C "$cohort" rev-parse --absolute-git-dir)
lock=$git_dir/frust-timing.lock
if ! mkdir "$lock"; then
  echo "cannot acquire timing lock for $workload" >&2
  exit 1
fi
applied=false
cleanup() {
  local status=$?
  if "$applied" && ! git -C "$cohort" apply --recount --reverse "$patch" >&2; then
    status=1
  fi
  if ! rmdir "$lock"; then
    status=1
  fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

status=$(git -C "$cohort" status --porcelain --untracked-files=no)
if [[ -n "$status" ]]; then
  echo "timing requires an unmodified $workload cohort" >&2
  exit 1
fi
if [[ -n "$patch" ]]; then
  git -C "$cohort" apply --recount "$patch"
  applied=true
fi
cd "$cohort"

{
  echo "workload $workload"
  echo "kind $kind"
  echo "package $package"
  echo "bins $bins"
  echo "cwd $(pwd)"
  echo "target $target"
  echo "RUSTC=$RUSTC"
  echo "FRUST_EXPERIMENTAL_CODEGEN_CACHE=${FRUST_EXPERIMENTAL_CODEGEN_CACHE:-0}"
  echo "FRUST_EXPERIMENTAL_RELR=${FRUST_EXPERIMENTAL_RELR:-0}"
  "$RUSTC" -Vv
} > "$log" 2>&1

if python3 - "$bins/cargo" "$package" >> "$log" 2>&1 <<'PYCODE'
import resource
import subprocess
import sys
import time

start = time.monotonic_ns()
result = subprocess.run([
    sys.argv[1], "build", "--release", "--locked", "--offline", "-p", sys.argv[2],
])
seconds = (time.monotonic_ns() - start) / 1_000_000_000
rss = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
if sys.platform == "darwin":
    rss = (rss + 1023) // 1024
print(f"WALL_SECONDS:{seconds:.9f}")
print(f"PEAK_RSS_KIB:{rss}")
sys.exit(result.returncode if result.returncode >= 0 else 128 - result.returncode)
PYCODE
then
  code=0
else
  code=$?
fi

if "$applied"; then
  if ! git apply --recount --reverse "$patch"; then
    echo "could not restore the source edit in $cohort" >&2
    code=1
  fi
  applied=false
fi
echo "EXIT:$code" >> "$log"
exit "$code"
